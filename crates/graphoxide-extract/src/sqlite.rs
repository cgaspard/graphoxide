//! Bounded, read-only SQLite file-format schema extraction.
//!
//! This module parses the SQLite file format directly (header, B-tree pages,
//! and the `sqlite_master` rows) without opening a database, executing SQL, or
//! reading row values. It emits the same schema-fact conventions as the DDL
//! scanner in [`crate::sql`]: file/table/view/index/trigger nodes, `contains`
//! ownership edges, `references` edges for foreign keys, and `defines` edges
//! for columns (primary-key columns carry a `primary_key` context).
//!
//! The parser is deliberately conservative: every page offset is bounds-
//! checked, every loop is budgeted, and malformed input yields an
//! inventory-style diagnostic node instead of a panic.
//!
//! DuckDB catalogs are a different container (an OpenSSL-style zip of WAL
//! files); the magic check below rejects them so they fall through to the
//! bounded container inventory rather than being misparsed as SQLite.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use graphoxide_core::{make_id, Confidence, Edge, Extraction, Node};

/// Maximum accepted input size (the I/O plane already caps reads lower).
const MAX_SQLITE_BYTES: usize = 64 * 1024 * 1024;
/// Minimum SQLite header length.
const SQLITE_HEADER_LEN: usize = 100;
/// Maximum number of B-tree cells parsed per page.
const MAX_CELLS_PER_PAGE: usize = 32_768;
/// Maximum number of schema objects retained.
const MAX_SCHEMA_OBJECTS: usize = 16_384;
/// Maximum number of total facts (nodes + edges) retained.
const MAX_FACTS: usize = 262_144;
/// Maximum bytes of a single `sqlite_master` record.
const MAX_RECORD_BYTES: usize = 1_048_576;
/// Maximum number of pages walked to resolve a B-tree root.
const MAX_PAGES: usize = 4_096;
/// Maximum identifier length accepted.
const MAX_IDENTIFIER_BYTES: usize = 4_096;

const SQLITE_MAGIC: &[u8; 16] = b"SQLite format 3\0";

fn be16(bytes: &[u8], offset: usize) -> Option<u16> {
    let value = bytes.get(offset..offset + 2)?;
    Some(u16::from_be_bytes([value[0], value[1]]))
}

fn be32(bytes: &[u8], offset: usize) -> Option<u32> {
    let value = bytes.get(offset..offset + 4)?;
    Some(u32::from_be_bytes([value[0], value[1], value[2], value[3]]))
}

fn be64(bytes: &[u8], offset: usize) -> Option<u64> {
    let value = bytes.get(offset..offset + 8)?;
    Some(u64::from_be_bytes([
        value[0], value[1], value[2], value[3], value[4], value[5], value[6], value[7],
    ]))
}

/// A decoded `sqlite_master` row.
#[derive(Debug, Clone)]
struct SchemaRow {
    /// `table`, `index`, `view`, or `trigger`.
    rtype: String,
    name: String,
    sql: Option<String>,
}

/// The decoded file header facts needed for page addressing.
#[derive(Debug, Clone, Copy)]
struct SqliteHeader {
    page_size: u32,
    database_size_pages: u32,
    reserved_bytes: u32,
}

/// Decode and sanity-check the 100-byte SQLite header.
fn parse_header(bytes: &[u8]) -> Result<SqliteHeader, String> {
    if bytes.len() < SQLITE_HEADER_LEN {
        return Err("truncated SQLite header".into());
    }
    if &bytes[..16] != SQLITE_MAGIC {
        return Err("missing SQLite magic header".into());
    }
    let page_size = be16(bytes, 16).unwrap_or(1);
    // A page size of 1 means 65536; any other value must be a power of two
    // between 512 and 65536.
    let page_size = if page_size == 1 {
        65_536
    } else {
        page_size as u32
    };
    if !(512..=65_536).contains(&page_size) || !page_size.is_power_of_two() {
        return Err(format!("invalid SQLite page size {page_size}"));
    }
    let file_format_write = bytes[18];
    let file_format_read = bytes[19];
    if file_format_write != file_format_read {
        return Err("SQLite file format version mismatch".into());
    }
    if file_format_write > 1 {
        return Err(format!(
            "unsupported SQLite file format version {file_format_write}"
        ));
    }
    let database_size_pages = be32(bytes, 28).unwrap_or(0);
    let text_encoding = be32(bytes, 56).unwrap_or(1);
    if text_encoding != 1 && text_encoding != 2 && text_encoding != 3 {
        return Err(format!("unsupported SQLite text encoding {text_encoding}"));
    }
    // The reserved-bytes-per-page field is a single byte at offset 20 (not a
    // 16-bit value); offset 21 is the maximum embedded payload fraction.
    let reserved_bytes = bytes[20] as u32;
    if reserved_bytes >= page_size / 2 {
        return Err(format!(
            "SQLite reserved bytes {reserved_bytes} exceed half the page size {page_size}"
        ));
    }
    Ok(SqliteHeader {
        page_size,
        database_size_pages,
        reserved_bytes,
    })
}

/// Page header facts for one B-tree page.
struct PageHeader {
    /// 0x02 interior index, 0x05 interior table, 0x0a leaf index, 0x0d leaf
    /// table.
    page_type: u8,
    cell_count: u16,
    rightmost_pointer: Option<u32>,
}

/// Offset of the B-tree page header within the file, for the given page.
/// Page 1 carries the 100-byte database file header first; every other page
/// starts its B-tree header at the beginning of the page.
fn page_header_offset(page_number: u32, page_offset: usize) -> usize {
    page_offset + if page_number == 1 { 100 } else { 0 }
}

fn parse_page_header(
    bytes: &[u8],
    page_number: u32,
    page_offset: usize,
    header: &SqliteHeader,
) -> Option<PageHeader> {
    let usable = (header.page_size - header.reserved_bytes) as usize;
    let header_start = page_header_offset(page_number, page_offset);
    if header_start + 12 > page_offset + usable {
        return None;
    }
    let type_byte = *bytes.get(header_start)?;
    if !matches!(type_byte, 0x02 | 0x05 | 0x0a | 0x0d) {
        return None;
    }
    let cell_count = be16(bytes, header_start + 3)?;
    if (cell_count as usize) > MAX_CELLS_PER_PAGE {
        return None;
    }
    let is_interior = type_byte == 0x02 || type_byte == 0x05;
    let rightmost_pointer = if is_interior {
        be32(bytes, header_start + 8)
    } else {
        None
    };
    Some(PageHeader {
        page_type: type_byte,
        cell_count,
        rightmost_pointer,
    })
}

/// Decode a SQLite varint (1-9 big-endian bytes). The first eight bytes
/// contribute their lower seven bits each (the high bit is the continuation
/// flag); the ninth byte contributes all eight of its bits.
fn varint(bytes: &[u8], offset: usize) -> Option<(u64, usize)> {
    let mut value: u64 = 0;
    for index in 0..9 {
        let byte = *bytes.get(offset + index)?;
        if index < 8 {
            value = (value << 7) | byte as u64 & 0x7f;
            if byte & 0x80 == 0 {
                return Some((value, index + 1));
            }
        } else {
            value = (value << 8) | byte as u64;
            return Some((value, 9));
        }
    }
    None
}

/// A decoded record field.
#[derive(Debug, Clone)]
enum Value {
    Null,
    /// Integer values (rowids, root pages) are decoded for record-shape
    /// validation but `sqlite_master` facts only surface the text fields.
    #[allow(dead_code)]
    Int(i64),
    /// Reals and blobs appear in general SQLite records; `sqlite_master` rows
    /// are all text/int, so the parser decodes but does not surface them.
    #[allow(dead_code)]
    Real(f64),
    #[allow(dead_code)]
    Blob(Vec<u8>),
    Text(String),
}

impl Value {
    fn as_str(&self) -> Option<&str> {
        match self {
            Value::Text(text) => Some(text),
            _ => None,
        }
    }
}

/// The payload layout implied by one record serial type number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SerialLayout {
    Null,
    /// Big-endian two's-complement integer of `length` bytes.
    Integer(usize),
    /// 8-byte IEEE 754 double.
    Float,
    /// The constants 0 and 1, which store no payload bytes.
    ConstantZero,
    ConstantOne,
    Text(usize),
    Blob(usize),
}

fn serial_layout(type_value: u64) -> Option<SerialLayout> {
    match type_value {
        0 => Some(SerialLayout::Null),
        1 => Some(SerialLayout::Integer(1)),
        2 => Some(SerialLayout::Integer(2)),
        3 => Some(SerialLayout::Integer(3)),
        4 => Some(SerialLayout::Integer(4)),
        5 => Some(SerialLayout::Integer(6)),
        6 => Some(SerialLayout::Integer(8)),
        7 => Some(SerialLayout::Float),
        8 => Some(SerialLayout::ConstantZero),
        9 => Some(SerialLayout::ConstantOne),
        10 | 11 => None,
        12 => Some(SerialLayout::Integer(8)),
        13 => Some(SerialLayout::Float),
        _ => {
            if type_value >= 14 {
                let length = ((type_value - 12) / 2) as usize;
                // Even serial types are BLOBs; odd serial types are TEXT.
                if type_value.is_multiple_of(2) {
                    Some(SerialLayout::Blob(length))
                } else {
                    Some(SerialLayout::Text(length))
                }
            } else {
                None
            }
        }
    }
}

fn read_signed_big_endian(bytes: &[u8], length: usize) -> i64 {
    let mut value: i64 = 0;
    for byte in bytes {
        value = (value << 8) | *byte as i64;
    }
    if length < 8 {
        let shift = (8 - length) * 8;
        value = value << shift >> shift;
    }
    value
}

fn decode_record(payload: &[u8]) -> Option<Vec<Value>> {
    let (header_length, consumed) = varint(payload, 0)?;
    let header_length = header_length as usize;
    if header_length < consumed || header_length > payload.len() {
        return None;
    }
    let mut layouts = Vec::new();
    let mut offset = consumed;
    while offset < header_length {
        let (type_value, type_consume) = varint(payload, offset)?;
        layouts.push(serial_layout(type_value)?);
        offset += type_consume;
        if offset > header_length {
            return None;
        }
    }
    let mut body = offset;
    let mut values = Vec::with_capacity(layouts.len());
    for layout in &layouts {
        let value = match layout {
            SerialLayout::Null => Value::Null,
            SerialLayout::Float => {
                let raw = be64(payload, body)?;
                body += 8;
                Value::Real(f64::from_bits(raw))
            }
            SerialLayout::ConstantZero => Value::Int(0),
            SerialLayout::ConstantOne => Value::Int(1),
            SerialLayout::Integer(length) => {
                let length = *length;
                let slice: &[u8] = payload.get(body..body + length)?;
                body += length;
                Value::Int(read_signed_big_endian(slice, length))
            }
            SerialLayout::Text(length) | SerialLayout::Blob(length) => {
                let length = *length;
                let slice: &[u8] = payload.get(body..body + length)?;
                body += length;
                if matches!(layout, SerialLayout::Text(_)) {
                    Value::Text(
                        String::from_utf8_lossy(slice)
                            .into_owned()
                            .chars()
                            .take(MAX_IDENTIFIER_BYTES)
                            .collect(),
                    )
                } else {
                    Value::Blob(slice.to_vec())
                }
            }
        };
        values.push(value);
    }
    Some(values)
}

/// Decode one cell from a `sqlite_master` (root page 1) leaf-table B-tree.
fn decode_schema_cell(
    bytes: &[u8],
    page_offset: usize,
    cell_pointer: usize,
    header: &SqliteHeader,
) -> Option<SchemaRow> {
    let usable = (header.page_size - header.reserved_bytes) as usize;
    if cell_pointer == 0 || cell_pointer > usable {
        return None;
    }
    let absolute = page_offset.checked_add(cell_pointer)?;
    let (payload_length, consumed) = varint(bytes, absolute)?;
    let payload_length = payload_length as usize;
    if payload_length > MAX_RECORD_BYTES {
        return None;
    }
    let (_, rowid_consumed) = varint(bytes, absolute + consumed)?;
    let payload_start = absolute + consumed + rowid_consumed;
    let payload = bytes.get(payload_start..payload_start + payload_length)?;
    let values = decode_record(payload)?;
    if values.len() < 5 {
        return None;
    }
    // The sqlite_master record is (type, name, tbl_name, rootpage, sql); the
    // rowid is the B-tree cell key, not a record field.
    let [rtype, name, _tbl_name, _rootpage, sql] = values.as_slice() else {
        return None;
    };
    let rtype = rtype.as_str()?;
    let name = name.as_str()?;
    let sql = match sql {
        Value::Text(text) => Some(text.as_str()),
        _ => None,
    };
    if name.len() > MAX_IDENTIFIER_BYTES {
        return None;
    }
    let rtype = rtype.to_ascii_lowercase();
    if !matches!(rtype.as_str(), "table" | "index" | "view" | "trigger") {
        return None;
    }
    Some(SchemaRow {
        rtype,
        name: name.to_owned(),
        sql: sql.map(str::to_owned),
    })
}

/// Walk the B-tree rooted at `root_page` and collect all leaf cells.
fn walk_btree(
    bytes: &[u8],
    header: &SqliteHeader,
    root_page: u32,
) -> Result<Vec<SchemaRow>, String> {
    let usable = (header.page_size - header.reserved_bytes) as usize;
    if root_page < 1 || (root_page as usize) > header.database_size_pages as usize {
        return Err(format!("sqlite_master root page {root_page} out of range"));
    }
    let mut rows = Vec::new();
    let mut visited = BTreeSet::new();
    let mut stack = vec![root_page];
    let mut pages_walked = 0usize;
    while let Some(page_number) = stack.pop() {
        if !visited.insert(page_number) {
            continue;
        }
        pages_walked += 1;
        if pages_walked > MAX_PAGES {
            return Err(format!("SQLite B-tree walk exceeded {MAX_PAGES} pages"));
        }
        let page_offset = (page_number as usize - 1)
            .checked_mul(header.page_size as usize)
            .ok_or_else(|| "SQLite page offset overflow".to_owned())?;
        let page_end = page_offset
            .checked_add(usable)
            .ok_or_else(|| "SQLite page end overflow".to_owned())?;
        if page_end > bytes.len() {
            return Err(format!(
                "SQLite page {page_number} extends beyond file end ({} bytes)",
                bytes.len()
            ));
        }
        let page_header = parse_page_header(bytes, page_number, page_offset, header)
            .ok_or_else(|| format!("invalid SQLite B-tree page header on page {page_number}"))?;
        // The cell pointer array immediately follows the B-tree page header.
        // Leaf pages use an 8-byte header; interior pages use a 12-byte header
        // (8 + a 4-byte rightmost pointer).
        let header_start = page_header_offset(page_number, page_offset);
        let is_interior = page_header.page_type == 0x02 || page_header.page_type == 0x05;
        let pointer_array_start = header_start + if is_interior { 12 } else { 8 };
        match page_header.page_type {
            0x0d | 0x0a => {
                // Leaf page: iterate the cell pointer array.
                for index in 0..page_header.cell_count as usize {
                    let pointer =
                        be16(bytes, pointer_array_start + index * 2).unwrap_or(0) as usize;
                    if rows.len() < MAX_SCHEMA_OBJECTS
                        && let Some(row) = decode_schema_cell(bytes, page_offset, pointer, header)
                    {
                        rows.push(row);
                    }
                }
            }
            0x02 | 0x05 => {
                // Interior page: each cell begins with a 4-byte child pointer,
                // and the page's last child is the rightmost pointer.
                let mut children = Vec::with_capacity(page_header.cell_count as usize + 1);
                for index in 0..page_header.cell_count as usize {
                    let cell_pointer =
                        be16(bytes, pointer_array_start + index * 2).unwrap_or(0) as usize;
                    if cell_pointer == 0 || cell_pointer > usable {
                        continue;
                    }
                    if let Some(child) = be32(bytes, page_offset + cell_pointer) {
                        children.push(child);
                    }
                }
                if let Some(rightmost) = page_header.rightmost_pointer {
                    children.push(rightmost);
                }
                // Push in reverse so the leftmost child is processed first.
                stack.extend(children.iter().rev().copied());
            }
            _ => {
                return Err(format!(
                    "unexpected SQLite page type 0x{:02x} on page {page_number}",
                    page_header.page_type
                ));
            }
        }
    }
    Ok(rows)
}

/// Extract schema facts from an already-read SQLite file buffer.
///
/// The byte-oriented path performs no filesystem access so it is safe to run
/// on an extraction worker after the I/O service has completed the read.
pub fn extract_sqlite_bytes(
    path: &Path,
    source_file: &str,
    bytes: &[u8],
) -> anyhow::Result<Extraction> {
    if bytes.is_empty() {
        return Ok(rejected_extraction(
            path,
            source_file,
            "empty file",
            bytes.len(),
        ));
    }
    if bytes.len() > MAX_SQLITE_BYTES {
        return Ok(rejected_extraction(
            path,
            source_file,
            &format!("exceeds {} byte extraction limit", MAX_SQLITE_BYTES),
            bytes.len(),
        ));
    }
    let header = match parse_header(bytes) {
        Ok(header) => header,
        Err(diagnostic) => {
            return Ok(rejected_extraction(
                path,
                source_file,
                &diagnostic,
                bytes.len(),
            ));
        }
    };
    // The sqlite_master root page is fixed at page 1 by the file format.
    let rows = match walk_btree(bytes, &header, 1) {
        Ok(rows) => rows,
        Err(diagnostic) => {
            return Ok(rejected_extraction(
                path,
                source_file,
                &diagnostic,
                bytes.len(),
            ));
        }
    };
    Ok(build_extraction(path, source_file, &rows, bytes.len()))
}

/// Path-based entry point for the legacy extraction facade.
pub fn extract_sqlite(path: &Path, source_file: &str) -> anyhow::Result<Extraction> {
    let bytes = std::fs::read(path)?;
    extract_sqlite_bytes(path, source_file, &bytes)
}

fn rejected_extraction(
    path: &Path,
    source_file: &str,
    diagnostic: &str,
    byte_length: usize,
) -> Extraction {
    let stem = Path::new(source_file)
        .with_extension("")
        .to_string_lossy()
        .replace('\\', "/");
    let id = make_id(&["format_inventory", "sqlite", &stem]);
    let node = Node {
        id,
        label: path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or(source_file)
            .to_owned(),
        file_type: "document".into(),
        source_file: source_file.to_owned(),
        source_location: None,
        community: None,
        extra: BTreeMap::from([
            (
                "type".to_owned(),
                serde_json::Value::String("format_inventory".into()),
            ),
            (
                "format".to_owned(),
                serde_json::Value::String("sqlite".into()),
            ),
            (
                "format_capability".to_owned(),
                serde_json::Value::String("structural_partial".into()),
            ),
            (
                "parse_status".to_owned(),
                serde_json::Value::String("rejected".into()),
            ),
            (
                "diagnostic".to_owned(),
                serde_json::Value::String(diagnostic.to_owned()),
            ),
            (
                "byte_length".to_owned(),
                serde_json::json!(byte_length as u64),
            ),
        ]),
    };
    Extraction {
        nodes: vec![node],
        ..Extraction::default()
    }
}

fn sqlite_node(id: String, label: &str, source_file: &str, kind: &str) -> Node {
    Node {
        id,
        label: label.to_owned(),
        file_type: "code".into(),
        source_file: source_file.to_owned(),
        source_location: None,
        community: None,
        extra: BTreeMap::from([
            (
                "_origin".to_owned(),
                serde_json::Value::String("sqlite".into()),
            ),
            (
                "type".to_owned(),
                serde_json::Value::String(kind.to_owned()),
            ),
        ]),
    }
}

fn build_extraction(
    path: &Path,
    source_file: &str,
    rows: &[SchemaRow],
    byte_length: usize,
) -> Extraction {
    let stem = Path::new(source_file)
        .with_extension("")
        .to_string_lossy()
        .replace('\\', "/");
    let file_id = make_id(&[&stem]);
    let file_label = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(source_file)
        .to_owned();
    let mut nodes: BTreeMap<String, Node> = BTreeMap::new();
    let mut edges: BTreeSet<(String, String, String, Option<String>)> = BTreeSet::new();
    let mut facts = 0usize;

    nodes.insert(
        file_id.clone(),
        Node {
            id: file_id.clone(),
            label: file_label,
            file_type: "code".into(),
            source_file: source_file.to_owned(),
            source_location: None,
            community: None,
            extra: BTreeMap::from([
                (
                    "_origin".to_owned(),
                    serde_json::Value::String("sqlite".to_owned()),
                ),
                (
                    "type".to_owned(),
                    serde_json::Value::String("file".to_owned()),
                ),
                (
                    "database_bytes".to_owned(),
                    serde_json::json!(byte_length as u64),
                ),
            ]),
        },
    );
    facts += 1;

    // First pass: collect all declared object ids so foreign keys can resolve
    // against them; unresolved targets become explicit reference nodes.
    let mut declared: BTreeMap<String, String> = BTreeMap::new();
    for row in rows {
        if row.rtype == "index" {
            continue;
        }
        let object_id = make_id(&[&stem, &row.name]);
        declared.insert(row.name.to_ascii_lowercase(), object_id);
    }

    for row in rows {
        if facts >= MAX_FACTS {
            break;
        }
        let is_index = row.rtype == "index";
        let object_id = make_id(&[&stem, &row.name]);
        if nodes
            .insert(
                object_id.clone(),
                sqlite_node(
                    object_id.clone(),
                    &row.name,
                    source_file,
                    row.rtype.as_str(),
                ),
            )
            .is_none()
        {
            facts += 1;
        }
        edges.insert((file_id.clone(), object_id.clone(), "contains".into(), None));
        facts += 1;

        // Columns, foreign keys, and primary keys come from the CREATE
        // statement text. Indexes and triggers do not declare columns here.
        if let Some(sql) = &row.sql
            && !is_index
            && row.rtype == "table"
        {
            emit_column_facts(
                &mut nodes,
                &mut edges,
                &mut facts,
                &object_id,
                &stem,
                &declared,
                source_file,
                sql,
            );
        }
    }

    let mut nodes: Vec<Node> = nodes.into_values().collect();
    // Keep the file anchor first, then sort the rest by id for determinism.
    nodes.sort_by(|a, b| {
        let a_first = a.id == file_id;
        let b_first = b.id == file_id;
        b_first.cmp(&a_first).then_with(|| a.id.cmp(&b.id))
    });
    let edges: Vec<Edge> = edges
        .into_iter()
        .map(|(source, target, relation, context)| {
            let mut extra = BTreeMap::from([
                ("_src".to_owned(), source.clone().into()),
                ("_tgt".to_owned(), target.clone().into()),
                ("weight".to_owned(), 1.0.into()),
            ]);
            if let Some(context) = &context {
                extra.insert("context".to_owned(), context.clone().into());
            }
            Edge {
                source,
                target,
                relation,
                confidence: Confidence::Extracted,
                source_file: source_file.to_owned(),
                extra,
            }
        })
        .collect();
    Extraction {
        nodes,
        edges,
        ..Extraction::default()
    }
}

struct ForeignKey {
    column: String,
    referenced_table: String,
    referenced_column: Option<String>,
}

/// Parse the column list of a `CREATE TABLE` statement and emit `defines`
/// edges for columns (primary-key columns carry a `primary_key` context) plus
/// `references` edges for foreign keys.
///
/// The parser is intentionally shallow: it understands the column-definition
/// grammar (type names, constraints, `PRIMARY KEY`, `REFERENCES`, `FOREIGN
/// KEY`) well enough to recover names and relationships, and ignores
/// everything else. It never executes SQL.
#[allow(clippy::too_many_arguments)]
fn emit_column_facts(
    nodes: &mut BTreeMap<String, Node>,
    edges: &mut BTreeSet<(String, String, String, Option<String>)>,
    facts: &mut usize,
    table_id: &str,
    stem: &str,
    declared: &BTreeMap<String, String>,
    source_file: &str,
    sql: &str,
) {
    let Some(open) = sql.find('(') else {
        return;
    };
    let Some(close) = sql.rfind(')') else {
        return;
    };
    if close <= open {
        return;
    }
    let definition = &sql[open + 1..close];
    for segment in split_top_level(definition) {
        let segment = segment.trim();
        if segment.is_empty() || *facts + 2 > MAX_FACTS {
            continue;
        }
        // Table-level constraints begin with a constraint keyword.
        if let Some(keyword) = segment.split_whitespace().next()
            && matches!(
                keyword.to_ascii_uppercase().as_str(),
                "PRIMARY" | "UNIQUE" | "CHECK" | "CONSTRAINT" | "FOREIGN"
            )
        {
            if let Some(fk) = parse_table_foreign_key(segment) {
                emit_foreign_key(
                    nodes,
                    edges,
                    facts,
                    table_id,
                    stem,
                    declared,
                    source_file,
                    &fk,
                );
            }
            continue;
        }
        let mut tokens = segment.splitn(2, char::is_whitespace);
        let column_name = tokens.next().map(strip_identifier).unwrap_or_default();
        let rest = tokens.next().unwrap_or("");
        if column_name.is_empty() || column_name.len() > MAX_IDENTIFIER_BYTES {
            continue;
        }
        let column_id = make_id(&[stem, table_id, &column_name]);
        if nodes
            .insert(
                column_id.clone(),
                sqlite_node(column_id.clone(), &column_name, source_file, "column"),
            )
            .is_none()
        {
            *facts = facts.saturating_add(1);
        }
        edges.insert((
            table_id.to_owned(),
            column_id.clone(),
            "defines".into(),
            Some("column".into()),
        ));
        *facts = facts.saturating_add(1);
        if rest.to_ascii_uppercase().contains("PRIMARY KEY") {
            edges.insert((
                table_id.to_owned(),
                column_id.clone(),
                "defines".into(),
                Some("primary_key".into()),
            ));
            *facts = facts.saturating_add(1);
        }
        if let Some(mut fk) = parse_column_foreign_key(rest) {
            fk.column = column_name;
            emit_foreign_key(
                nodes,
                edges,
                facts,
                table_id,
                stem,
                declared,
                source_file,
                &fk,
            );
        }
    }
}

fn parse_table_foreign_key(segment: &str) -> Option<ForeignKey> {
    let upper = segment.to_ascii_uppercase();
    let start = upper.find("FOREIGN KEY")?;
    let rest = &segment[start..];
    let columns = extract_parenthesized(rest)?;
    let references = rest[rest.find("REFERENCES")? + "REFERENCES".len()..].trim_start();
    let (table, table_end) = split_identifier(references)?;
    let referenced_column = references[table_end..]
        .trim_start()
        .strip_prefix('(')
        .and_then(|rest| rest.find(')'))
        .map(|end| strip_identifier(&rest[..end]));
    let first_column = columns
        .split(',')
        .next()
        .map(strip_identifier)
        .filter(|value| !value.is_empty())
        .unwrap_or_default();
    Some(ForeignKey {
        column: first_column,
        referenced_table: table,
        referenced_column,
    })
}

fn parse_column_foreign_key(rest: &str) -> Option<ForeignKey> {
    let upper = rest.to_ascii_uppercase();
    let start = upper.find("REFERENCES")?;
    let references = &rest[start + "REFERENCES".len()..];
    let (table, table_end) = split_identifier(references.trim_start())?;
    let referenced_column = references[table_end..]
        .trim_start()
        .strip_prefix('(')
        .and_then(|rest| rest.find(')'))
        .map(|end| strip_identifier(&rest[..end]));
    Some(ForeignKey {
        column: String::new(),
        referenced_table: table,
        referenced_column,
    })
}

#[allow(clippy::too_many_arguments)]
fn emit_foreign_key(
    nodes: &mut BTreeMap<String, Node>,
    edges: &mut BTreeSet<(String, String, String, Option<String>)>,
    facts: &mut usize,
    table_id: &str,
    stem: &str,
    declared: &BTreeMap<String, String>,
    source_file: &str,
    fk: &ForeignKey,
) {
    let target_id = declared
        .get(&fk.referenced_table.to_ascii_lowercase())
        .filter(|id| id.as_str() != table_id)
        .cloned()
        .unwrap_or_else(|| {
            // Unresolved target: emit an explicit reference node.
            let reference_id = make_id(&[stem, "ref", &fk.referenced_table]);
            if nodes
                .insert(
                    reference_id.clone(),
                    Node {
                        id: reference_id.clone(),
                        label: fk.referenced_table.clone(),
                        file_type: "code".into(),
                        source_file: String::new(),
                        source_location: None,
                        community: None,
                        extra: BTreeMap::from([
                            (
                                "_origin".to_owned(),
                                serde_json::Value::String("sqlite".into()),
                            ),
                            (
                                "type".to_owned(),
                                serde_json::Value::String("reference".into()),
                            ),
                            (
                                "origin_file".to_owned(),
                                serde_json::Value::String(source_file.to_owned()),
                            ),
                        ]),
                    },
                )
                .is_none()
            {
                *facts = facts.saturating_add(1);
            }
            reference_id
        });
    let context = if let Some(column) = &fk.referenced_column {
        format!("foreign_key:{column}")
    } else if !fk.column.is_empty() {
        format!("foreign_key:{}", fk.column)
    } else {
        "foreign_key".to_owned()
    };
    if *facts < MAX_FACTS {
        edges.insert((
            table_id.to_owned(),
            target_id,
            "references".into(),
            Some(context),
        ));
        *facts = facts.saturating_add(1);
    }
}

fn split_top_level(definition: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    let mut in_quote: Option<char> = None;
    for (index, character) in definition.char_indices() {
        match (in_quote, character) {
            (Some(current), next) if current == next => in_quote = None,
            (None, '\'') | (None, '"') | (None, '`') => in_quote = Some(character),
            (None, '(') => depth += 1,
            (None, ')') => depth = depth.saturating_sub(1),
            (None, ',') if depth == 0 => {
                segments.push(definition[start..index].to_owned());
                start = index + 1;
            }
            _ => {}
        }
    }
    segments.push(definition[start..].to_owned());
    segments
}

fn strip_identifier(raw: &str) -> String {
    let trimmed = raw.trim();
    let trimmed = trimmed.trim_end_matches(';');
    let bytes = trimmed.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'[' && bytes[bytes.len() - 1] == b']')
            || (bytes[0] == b'`' && bytes[bytes.len() - 1] == b'`'))
    {
        let inner = &trimmed[1..trimmed.len() - 1];
        if bytes[0] == b'"' {
            inner.replace("''", "\"")
        } else {
            inner.to_owned()
        }
    } else {
        trimmed.to_owned()
    }
}

fn split_identifier(raw: &str) -> Option<(String, usize)> {
    let bytes = raw.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let (start, end) = if bytes[0] == b'"' {
        let close = bytes[1..].iter().position(|byte| *byte == b'"')? + 1;
        (1, close)
    } else if bytes[0] == b'[' {
        let close = bytes[1..].iter().position(|byte| *byte == b']')? + 1;
        (1, close)
    } else if bytes[0] == b'`' {
        let close = bytes[1..].iter().position(|byte| *byte == b'`')? + 1;
        (1, close)
    } else {
        let end = bytes
            .iter()
            .position(|byte| !byte.is_ascii_alphanumeric() && *byte != b'_')
            .unwrap_or(bytes.len());
        (0, end)
    };
    if end <= start {
        return None;
    }
    Some((raw[start..end].to_owned(), end))
}

fn extract_parenthesized(segment: &str) -> Option<String> {
    let start = segment.find('(')?;
    let end = segment[start..].find(')')? + start;
    Some(segment[start + 1..end].to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn be16_into(buf: &mut [u8], offset: usize, value: u16) {
        buf[offset..offset + 2].copy_from_slice(&value.to_be_bytes());
    }

    fn be32_into(buf: &mut [u8], offset: usize, value: u32) {
        buf[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
    }

    fn write_varint(buf: &mut Vec<u8>, value: u64) {
        if value < 0x80 {
            buf.push(value as u8);
            return;
        }
        // Nine-byte form: eight bytes carry seven bits each (high bit set as
        // the continuation flag) and the final byte carries all eight bits.
        let mut slots = [0u8; 9];
        let mut remaining = value;
        for slot in (0..8).rev() {
            slots[slot] = ((remaining & 0x7f) | 0x80) as u8;
            remaining >>= 7;
        }
        slots[8] = remaining as u8;
        buf.extend_from_slice(&slots);
    }

    fn serial_type_for_text(length: usize) -> u64 {
        // Odd serial types encode TEXT: length = (N - 13) / 2, so N = 13 + 2L.
        13 + length as u64 * 2
    }

    fn encode_sqlite_value(buf: &mut Vec<u8>, value: &Value) {
        match value {
            Value::Null => write_varint(buf, 0),
            Value::Int(0) => write_varint(buf, 8),
            Value::Int(1) => write_varint(buf, 9),
            Value::Int(n) => {
                let mut width = 1usize;
                while width < 8 {
                    let low = -(1i64 << (width * 8 - 1));
                    let high = 1i64 << (width * 8 - 1);
                    if *n >= low && *n < high {
                        break;
                    }
                    width += 1;
                }
                let serial = match width {
                    1 => 1u64,
                    2 => 2,
                    3 => 3,
                    4 => 4,
                    6 => 5,
                    _ => 12, // 8-byte integers use serial type 12
                };
                write_varint(buf, serial);
                let bytes = n.to_be_bytes();
                buf.extend_from_slice(&bytes[8 - width..]);
            }
            Value::Real(f) => {
                write_varint(buf, 13);
                buf.extend_from_slice(&f.to_bits().to_be_bytes());
            }
            Value::Text(s) => {
                let bytes = s.as_bytes();
                write_varint(buf, serial_type_for_text(bytes.len()));
                buf.extend_from_slice(bytes);
            }
            Value::Blob(b) => {
                // Even serial types encode BLOB: length = (N - 12) / 2, so N = 12 + 2L.
                write_varint(buf, 12 + b.len() as u64 * 2);
                buf.extend_from_slice(b);
            }
        }
    }

    fn encode_record(values: &[Value]) -> Vec<u8> {
        let mut header = Vec::new();
        let mut body = Vec::new();
        for value in values {
            let mut value_bytes = Vec::new();
            encode_sqlite_value(&mut value_bytes, value);
            // The first varint is the serial type; the remainder is the body.
            let mut type_len = 0usize;
            let mut shifted = value_bytes[0] as u64;
            while shifted & 0x80 != 0 {
                shifted >>= 7;
                type_len += 1;
            }
            type_len += 1;
            header.extend_from_slice(&value_bytes[..type_len]);
            body.extend_from_slice(&value_bytes[type_len..]);
        }
        let mut record = Vec::new();
        write_varint(&mut record, (header.len() + body.len()) as u64);
        record.extend_from_slice(&header);
        record.extend_from_slice(&body);
        record
    }

    /// Build a minimal, valid single-page SQLite file whose `sqlite_master`
    /// (page 1) leaf B-tree holds the supplied schema rows. The file is
    /// assembled by hand so the tests exercise the real byte parser without
    /// depending on a system `sqlite3` binary. Cell offsets are file-relative
    /// (page 1 begins at file offset 0, after the 100-byte database header).
    fn build_sqlite(rows: &[(Value, Value, Value, Option<Value>)]) -> Vec<u8> {
        let page_size: usize = 4096;
        let mut cells: Vec<Vec<u8>> = Vec::new();
        for (index, (rtype, name, tbl, sql)) in rows.iter().enumerate() {
            let rowid = index as i64 + 1;
            // The sqlite_master record is (type, name, tbl_name, rootpage,
            // sql); the rowid is the cell key, not a record field.
            let record = encode_record(&[
                rtype.clone(),
                name.clone(),
                tbl.clone(),
                Value::Int(rowid),
                sql.clone().unwrap_or(Value::Null),
            ]);
            let mut cell = Vec::new();
            write_varint(&mut cell, record.len() as u64);
            write_varint(&mut cell, rowid as u64);
            cell.extend_from_slice(&record);
            cells.push(cell);
        }
        let cell_count = cells.len() as u16;
        // The B-tree page header starts at file offset 100 (after the
        // database header) and the cell pointer array follows it at 108.
        let page_header_start = 100usize;
        let pointer_array_start = page_header_start + 8;
        // Cells are packed at the end of the page, growing downward from the
        // last usable byte (page_size, reserved space is zero).
        let mut cursor = page_size;
        let mut cell_offsets = Vec::new();
        for cell in &cells {
            cursor = cursor.saturating_sub(cell.len());
            cell_offsets.push(cursor);
        }
        let mut file = vec![0u8; page_size];
        // Database header.
        file[..16].copy_from_slice(SQLITE_MAGIC);
        be16_into(&mut file, 16, page_size as u16);
        file[18] = 1; // file format write
        file[19] = 1; // file format read
        file[21] = 64; // max embedded payload fraction
        file[22] = 32; // min embedded payload fraction
        file[23] = 32; // leaf payload fraction
        be32_into(&mut file, 28, 1); // database size in pages
        be32_into(&mut file, 40, 1); // schema cookie
        be32_into(&mut file, 44, 4); // schema format number
        be32_into(&mut file, 56, 1); // text encoding (UTF-8)
        file[92..96].copy_from_slice(&1_000_000_000u32.to_be_bytes());
        file[96..100].copy_from_slice(&1_000_000_000u32.to_be_bytes());
        // B-tree page header (leaf table, 0x0d) at file offset 100.
        file[page_header_start] = 0x0d;
        be16_into(&mut file, page_header_start + 1, 0); // first free block
        be16_into(&mut file, page_header_start + 3, cell_count);
        be16_into(&mut file, page_header_start + 5, cursor as u16); // cell content start
        be16_into(&mut file, page_header_start + 7, 0); // fragmented free bytes
                                                        // Cell pointer array.
        for (index, offset) in cell_offsets.iter().enumerate() {
            be16_into(&mut file, pointer_array_start + index * 2, *offset as u16);
        }
        // Cell content.
        for (offset, cell) in cell_offsets.iter().zip(cells.iter()) {
            let offset = *offset;
            file[offset..offset + cell.len()].copy_from_slice(cell);
        }
        file
    }

    fn row(
        rtype: &str,
        name: &str,
        tbl: &str,
        sql: Option<&str>,
    ) -> (Value, Value, Value, Option<Value>) {
        (
            Value::Text(rtype.into()),
            Value::Text(name.into()),
            Value::Text(tbl.into()),
            sql.map(str::to_owned).map(Value::Text),
        )
    }

    fn labels(extraction: &Extraction) -> Vec<String> {
        extraction
            .nodes
            .iter()
            .map(|node| node.label.clone())
            .collect()
    }

    fn has_edge<'a>(extraction: &'a Extraction, relation: &str) -> impl Iterator<Item = &'a Edge> {
        extraction
            .edges
            .iter()
            .filter(move |edge| edge.relation == relation)
    }

    /// Generate a real SQLite file with the system `sqlite3` CLI when
    /// available, so the parser is exercised against a producer it did not
    /// author. Returns `None` when the binary is absent so the hand-built
    /// fixtures remain the primary coverage.
    fn real_sqlite_bytes(statements: &str) -> Option<Vec<u8>> {
        let dir = tempfile::tempdir().ok()?;
        let db = dir.path().join("live.db");
        let output = std::process::Command::new("sqlite3")
            .arg(&db)
            .arg(statements)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        std::fs::read(&db).ok()
    }

    /// Produce the byte source for a schema fixture: a real `sqlite3`-created
    /// database when the CLI is available (the strongest evidence), otherwise
    /// a hand-assembled single-page file.
    fn fixture_bytes(
        sqlite_statements: &str,
        rows: &[(Value, Value, Value, Option<Value>)],
    ) -> Vec<u8> {
        real_sqlite_bytes(sqlite_statements).unwrap_or_else(|| build_sqlite(rows))
    }

    #[test]
    fn parses_a_database_generated_by_sqlite3() {
        let Some(bytes) = real_sqlite_bytes(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT); \
             CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER REFERENCES users(id));",
        ) else {
            eprintln!("sqlite3 CLI unavailable; skipping live-database test");
            return;
        };
        let extraction =
            extract_sqlite_bytes(Path::new("live.db"), "live.db", &bytes).expect("byte extraction");
        let all = labels(&extraction);
        assert!(all.contains(&"users".to_owned()), "table missing: {all:?}");
        assert!(all.contains(&"orders".to_owned()), "table missing: {all:?}");
        let object_id = |label: &str| {
            extraction
                .nodes
                .iter()
                .find(|node| node.label == label)
                .expect("object node")
                .id
                .clone()
        };
        assert!(
            has_edge(&extraction, "references")
                .any(|edge| edge.true_source() == object_id("orders")
                    && edge.true_target() == object_id("users")),
            "missing foreign-key reference orders -> users"
        );
    }

    #[test]
    fn rejects_non_sqlite_magic() {
        let bytes = b"definitely not a sqlite file at all";
        let extraction =
            extract_sqlite_bytes(Path::new("data.db"), "data.db", bytes).expect("byte extraction");
        assert_eq!(extraction.nodes.len(), 1);
        assert_eq!(
            extraction
                .nodes
                .first()
                .and_then(|node| node.extra.get("parse_status"))
                .and_then(serde_json::Value::as_str),
            Some("rejected")
        );
        assert!(extraction.edges.is_empty());
    }

    #[test]
    fn rejects_truncated_header() {
        let bytes = b"SQLite format 3\0\x10\x00";
        let extraction =
            extract_sqlite_bytes(Path::new("tiny.db"), "tiny.db", bytes).expect("byte extraction");
        assert_eq!(extraction.nodes.len(), 1);
        assert!(extraction.edges.is_empty());
    }

    #[test]
    fn rejects_empty_file() {
        let extraction =
            extract_sqlite_bytes(Path::new("empty.db"), "empty.db", &[]).expect("byte extraction");
        assert_eq!(extraction.nodes.len(), 1);
        assert_eq!(
            extraction
                .nodes
                .first()
                .and_then(|node| node.extra.get("parse_status"))
                .and_then(serde_json::Value::as_str),
            Some("rejected")
        );
    }

    /// A DuckDB catalog is a zip of WAL files, not a SQLite B-tree file. It
    /// must be rejected by the SQLite magic check so it falls through to the
    /// bounded container inventory instead of being misparsed.
    #[test]
    fn rejects_duckdb_catalog_with_stable_inventory() {
        // A ZIP local-file-header magic (as a DuckDB catalog begins with),
        // padded past the 100-byte header threshold so the magic check - not
        // the length check - is what rejects it.
        let mut bytes = vec![0u8; 200];
        bytes[..10].copy_from_slice(b"PK\x03\x04\x14\x00\x00\x00\x00\x00");
        let extraction =
            extract_sqlite_bytes(Path::new("catalog.duckdb"), "catalog.duckdb", &bytes)
                .expect("byte extraction");
        assert_eq!(extraction.nodes.len(), 1);
        let node = extraction.nodes.first().expect("inventory node");
        assert_eq!(
            node.extra.get("format").and_then(serde_json::Value::as_str),
            Some("sqlite")
        );
        assert_eq!(
            node.extra
                .get("parse_status")
                .and_then(serde_json::Value::as_str),
            Some("rejected")
        );
        assert_eq!(
            node.extra
                .get("diagnostic")
                .and_then(serde_json::Value::as_str),
            Some("missing SQLite magic header")
        );
        assert!(extraction.edges.is_empty());
    }

    /// An encrypted SQLite file (SEE) keeps the 100-byte header magic but the
    /// rest of the file is ciphertext, so the B-tree walk cannot resolve page 1
    /// and the file is rejected with a stable diagnostic.
    #[test]
    fn rejects_encrypted_file_with_stable_inventory() {
        let mut bytes = build_sqlite(&[row(
            "table",
            "secret",
            "secret",
            Some("CREATE TABLE secret (id INTEGER PRIMARY KEY)"),
        )]);
        // Corrupt every byte after the 100-byte header to simulate encryption.
        for byte in &mut bytes[100..] {
            *byte ^= 0xA5;
        }
        let extraction = extract_sqlite_bytes(Path::new("locked.db"), "locked.db", &bytes)
            .expect("byte extraction");
        assert_eq!(extraction.nodes.len(), 1);
        let node = extraction.nodes.first().expect("inventory node");
        assert_eq!(
            node.extra
                .get("parse_status")
                .and_then(serde_json::Value::as_str),
            Some("rejected")
        );
        assert!(extraction.edges.is_empty());
    }

    #[test]
    fn extracts_tables_columns_and_foreign_keys() {
        let statements = "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT); \
                         CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER REFERENCES users(id)); \
                         CREATE INDEX idx_orders_user ON orders(user_id);";
        let rows = [
            row(
                "table",
                "users",
                "users",
                Some("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)"),
            ),
            row(
                "table",
                "orders",
                "orders",
                Some(
                    "CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER REFERENCES users(id))",
                ),
            ),
            row("index", "idx_orders_user", "orders", None),
        ];
        let bytes = fixture_bytes(statements, &rows);
        let extraction =
            extract_sqlite_bytes(Path::new("shop.db"), "shop.db", &bytes).expect("byte extraction");

        let all = labels(&extraction);
        assert!(
            all.contains(&"shop.db".to_owned()),
            "file anchor missing: {all:?}"
        );
        assert!(all.contains(&"users".to_owned()), "table missing: {all:?}");
        assert!(all.contains(&"orders".to_owned()), "table missing: {all:?}");
        assert!(
            all.contains(&"idx_orders_user".to_owned()),
            "index missing: {all:?}"
        );
        assert!(all.contains(&"id".to_owned()), "column missing: {all:?}");

        // The file owns each schema object.
        let file_id = extraction.nodes.first().expect("file first").id.clone();
        let object_id = |label: &str| {
            extraction
                .nodes
                .iter()
                .find(|node| node.label == label)
                .expect("object node")
                .id
                .clone()
        };
        for table in ["users", "orders", "idx_orders_user"] {
            assert!(
                has_edge(&extraction, "contains")
                    .any(|edge| edge.true_source() == file_id
                        && edge.true_target() == object_id(table)),
                "missing contains edge for {table}"
            );
        }

        // orders.user_id references users.
        let users = object_id("users");
        let orders = object_id("orders");
        assert!(
            has_edge(&extraction, "references")
                .any(|edge| edge.true_source() == orders && edge.true_target() == users),
            "missing foreign-key reference orders -> users"
        );

        // The primary-key column carries a primary_key context.
        assert!(
            has_edge(&extraction, "defines").any(|edge| edge
                .extra
                .get("context")
                .and_then(serde_json::Value::as_str)
                == Some("primary_key")),
            "missing primary_key defines context"
        );
    }

    #[test]
    fn unresolved_foreign_key_becomes_reference_node() {
        let statements =
            "CREATE TABLE events (id INTEGER PRIMARY KEY, actor_id INTEGER REFERENCES actors(id));";
        let rows = [row(
            "table",
            "events",
            "events",
            Some("CREATE TABLE events (id INTEGER PRIMARY KEY, actor_id INTEGER REFERENCES actors(id))"),
        )];
        let bytes = fixture_bytes(statements, &rows);
        let extraction =
            extract_sqlite_bytes(Path::new("log.db"), "log.db", &bytes).expect("byte extraction");
        assert!(
            extraction.nodes.iter().any(|node| node.label == "actors"
                && node.extra.get("type").and_then(serde_json::Value::as_str) == Some("reference")),
            "expected an explicit reference node for the unresolved table"
        );
        assert!(
            has_edge(&extraction, "references").count() == 1,
            "expected exactly one foreign-key reference edge"
        );
    }

    #[test]
    fn table_level_foreign_key_is_resolved() {
        let statements = "CREATE TABLE accounts (id INTEGER PRIMARY KEY); \
                         CREATE TABLE invoices (id INTEGER PRIMARY KEY, account_id INTEGER, FOREIGN KEY (account_id) REFERENCES accounts(id));";
        let rows = [
            row("table", "accounts", "accounts", Some("CREATE TABLE accounts (id INTEGER PRIMARY KEY)")),
            row(
                "table",
                "invoices",
                "invoices",
                Some("CREATE TABLE invoices (id INTEGER PRIMARY KEY, account_id INTEGER, FOREIGN KEY (account_id) REFERENCES accounts(id))"),
            ),
        ];
        let bytes = fixture_bytes(statements, &rows);
        let extraction = extract_sqlite_bytes(Path::new("billing.db"), "billing.db", &bytes)
            .expect("byte extraction");
        let object_id = |label: &str| {
            extraction
                .nodes
                .iter()
                .find(|node| node.label == label)
                .expect("object node")
                .id
                .clone()
        };
        assert!(
            has_edge(&extraction, "references")
                .any(|edge| edge.true_source() == object_id("invoices")
                    && edge.true_target() == object_id("accounts")),
            "missing table-level foreign-key edge"
        );
    }
}
