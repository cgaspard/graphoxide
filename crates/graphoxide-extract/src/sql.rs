//! Deterministic SQL DDL extraction.
//!
//! The general fallback scanner can find a few `CREATE` declarations, but SQL
//! needs a little more structure: schema-qualified/quoted identifiers,
//! PL/pgSQL routines whose bodies are not valid plain SQL, foreign-key edges,
//! and `FROM`/`JOIN` dependencies.  This scanner intentionally stays
//! statement-oriented so an unparseable routine body cannot consume later DDL.

use graphoxide_core::{make_id, Confidence, Edge, Extraction, Node};
use regex::Regex;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    path::Path,
};

#[derive(Debug, Clone)]
struct Definition {
    name: String,
    id: String,
    kind: String,
    start: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DollarMask {
    All,
    PreserveDelimiters,
}

fn mask_range(bytes: &mut [u8], start: usize, end: usize) {
    for byte in &mut bytes[start..end] {
        if !matches!(*byte, b'\n' | b'\r') {
            *byte = b' ';
        }
    }
}

fn dollar_delimiter_end(bytes: &[u8], start: usize) -> Option<usize> {
    if bytes.get(start) != Some(&b'$') {
        return None;
    }
    let mut cursor = start + 1;
    if bytes.get(cursor) == Some(&b'$') {
        return Some(cursor + 1);
    }
    if !bytes
        .get(cursor)
        .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
    {
        return None;
    }
    cursor += 1;
    while bytes
        .get(cursor)
        .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
    {
        cursor += 1;
    }
    (bytes.get(cursor) == Some(&b'$')).then_some(cursor + 1)
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    (!needle.is_empty())
        .then(|| {
            haystack
                .windows(needle.len())
                .position(|window| window == needle)
        })
        .flatten()
}

/// Replace non-code SQL bytes with spaces while retaining byte offsets and
/// line breaks. Double-quoted identifiers remain visible to the DDL regexes.
fn mask_sql(text: &str, dollar_mask: DollarMask) -> String {
    let original = text.as_bytes();
    let mut masked = original.to_vec();
    let mut cursor = 0;
    while cursor < original.len() {
        if original[cursor..].starts_with(b"--") {
            let start = cursor;
            cursor += 2;
            while cursor < original.len() && !matches!(original[cursor], b'\n' | b'\r') {
                cursor += 1;
            }
            mask_range(&mut masked, start, cursor);
            continue;
        }
        if original[cursor..].starts_with(b"/*") {
            let start = cursor;
            let mut depth = 1usize;
            cursor += 2;
            while cursor < original.len() && depth > 0 {
                if original[cursor..].starts_with(b"/*") {
                    depth += 1;
                    cursor += 2;
                } else if original[cursor..].starts_with(b"*/") {
                    depth -= 1;
                    cursor += 2;
                } else {
                    cursor += 1;
                }
            }
            mask_range(&mut masked, start, cursor);
            continue;
        }
        if original[cursor] == b'\'' {
            let start = cursor;
            cursor += 1;
            while cursor < original.len() {
                if original[cursor] == b'\\' {
                    cursor = (cursor + 2).min(original.len());
                } else if original[cursor] == b'\'' && original.get(cursor + 1) == Some(&b'\'') {
                    cursor += 2;
                } else if original[cursor] == b'\'' {
                    cursor += 1;
                    break;
                } else {
                    cursor += 1;
                }
            }
            mask_range(&mut masked, start, cursor);
            continue;
        }
        if original[cursor] == b'"' {
            let start = cursor;
            cursor += 1;
            while cursor < original.len() {
                if original[cursor] == b'"' && original.get(cursor + 1) == Some(&b'"') {
                    cursor += 2;
                } else if original[cursor] == b'"' {
                    cursor += 1;
                    break;
                } else {
                    cursor += 1;
                }
            }
            for byte in &mut masked[start..cursor] {
                if matches!(*byte, b'\n' | b'\r') {
                    *byte = b' ';
                }
            }
            continue;
        }
        let separated = cursor == 0
            || !original[cursor - 1].is_ascii_alphanumeric()
                && !matches!(original[cursor - 1], b'_' | b'$');
        if separated && let Some(delimiter_end) = dollar_delimiter_end(original, cursor) {
            let delimiter = &original[cursor..delimiter_end];
            let content_start = delimiter_end;
            if let Some(relative_close) = find_bytes(&original[content_start..], delimiter) {
                let close_start = content_start + relative_close;
                let close_end = close_start + delimiter.len();
                if dollar_mask == DollarMask::All {
                    mask_range(&mut masked, cursor, close_end);
                } else {
                    mask_range(&mut masked, content_start, close_start);
                }
                cursor = close_end;
            } else {
                let mask_start = if dollar_mask == DollarMask::All {
                    cursor
                } else {
                    content_start
                };
                mask_range(&mut masked, mask_start, original.len());
                cursor = original.len();
            }
            continue;
        }
        cursor += 1;
    }
    String::from_utf8(masked).expect("SQL lexical masking preserves UTF-8")
}

fn routine_dollar_body(text: &str) -> Option<std::ops::Range<usize>> {
    let visible = mask_sql(text, DollarMask::PreserveDelimiters);
    let body_start = Regex::new(r"(?i)\bAS\s*(\$(?:[A-Za-z_][A-Za-z0-9_]*)?\$)")
        .expect("valid SQL routine-body regex");
    for captures in body_start.captures_iter(&visible) {
        let delimiter_match = captures.get(1).expect("routine dollar delimiter");
        let delimiter = &text[delimiter_match.start()..delimiter_match.end()];
        let start = delimiter_match.end();
        if let Some(relative_end) = text[start..].find(delimiter) {
            return Some(start..start + relative_end);
        }
    }
    None
}

pub(crate) fn extract_sql(path: &Path, source_file: &str) -> anyhow::Result<Extraction> {
    let text = fs::read_to_string(path)?;
    extract_sql_bytes(path, source_file, text.as_bytes())
}

/// Extract SQL DDL from an already-read source buffer.
///
/// The byte-oriented path performs no filesystem access so it is safe to run
/// on an extraction worker after the I/O service has completed the read.
#[allow(dead_code)] // Activated by the byte-oriented engine dispatch.
pub(crate) fn extract_sql_bytes(
    path: &Path,
    source_file: &str,
    bytes: &[u8],
) -> anyhow::Result<Extraction> {
    let text = std::str::from_utf8(bytes)?;
    let stem = Path::new(source_file)
        .with_extension("")
        .to_string_lossy()
        .replace('\\', "/");
    let file_id = make_id(&[&stem]);
    let mut nodes = vec![node(
        file_id.clone(),
        path.file_name()
            .and_then(|value| value.to_str())
            .unwrap_or(source_file),
        source_file,
        1,
        "file",
        None,
    )];
    let mut edges = Vec::new();
    let mut seen_nodes = HashSet::from([file_id.clone()]);
    let mut seen_edges = HashSet::new();

    // A SQL identifier is either an ordinary identifier or a double-quoted
    // identifier. Qualified names may mix both forms.
    let identifier = r#"(?:"(?:""|[^"])+"|[A-Za-z_][A-Za-z0-9_$]*)(?:\s*\.\s*(?:"(?:""|[^"])+"|[A-Za-z_][A-Za-z0-9_$]*))*"#;
    let create = Regex::new(&format!(
        r"(?im)^\s*CREATE\s+(?:OR\s+REPLACE\s+)?(TABLE|VIEW|FUNCTION|PROCEDURE)\s+(?:IF\s+NOT\s+EXISTS\s+)?({identifier})"
    ))?;
    let top_level = mask_sql(text, DollarMask::All);
    let mut definitions = Vec::new();
    let mut definitions_by_name = HashMap::<String, String>::new();
    for captures in create.captures_iter(&top_level) {
        let matched = captures.get(0).expect("CREATE match");
        let kind_match = captures.get(1).expect("CREATE kind");
        let name_match = captures.get(2).expect("CREATE identifier");
        let kind = text[kind_match.start()..kind_match.end()].to_ascii_lowercase();
        let name = compact_identifier(&text[name_match.start()..name_match.end()]);
        let id = make_id(&[&stem, &name]);
        let line = line_of(text, matched.start());
        if seen_nodes.insert(id.clone()) {
            let label = if matches!(kind.as_str(), "function" | "procedure") {
                format!("{name}()")
            } else {
                name.clone()
            };
            nodes.push(node(id.clone(), &label, source_file, line, &kind, None));
            push_edge(
                &mut edges,
                &mut seen_edges,
                &file_id,
                &id,
                "contains",
                source_file,
                line,
                None,
            );
        }
        definitions_by_name
            .entry(identifier_key(&name))
            .or_insert_with(|| id.clone());
        definitions.push(Definition {
            name,
            id,
            kind,
            start: matched.start(),
        });
    }

    // Split dependency scans at the next top-level CREATE declaration.
    // Dollar-quoted PL/pgSQL can contain semicolons freely, so semicolon
    // splitting is unsafe.
    let reads = Regex::new(&format!(r"(?i)\b(?:FROM|JOIN)\s+({identifier})"))?;
    for (index, definition) in definitions.iter().enumerate() {
        let end = definitions
            .get(index + 1)
            .map_or(text.len(), |next| next.start);
        if matches!(definition.kind.as_str(), "view" | "function" | "procedure") {
            let statement = &text[definition.start..end];
            let statement_mask = mask_sql(statement, DollarMask::All);
            emit_reads_from_masked(
                text,
                statement,
                &statement_mask,
                definition.start,
                definition,
                &reads,
                source_file,
                &definitions_by_name,
                &mut nodes,
                &mut seen_nodes,
                &mut edges,
                &mut seen_edges,
            );
            if matches!(definition.kind.as_str(), "function" | "procedure")
                && let Some(body_range) = routine_dollar_body(statement)
            {
                let body = &statement[body_range.clone()];
                let body_mask = mask_sql(body, DollarMask::All);
                emit_reads_from_masked(
                    text,
                    body,
                    &body_mask,
                    definition.start + body_range.start,
                    definition,
                    &reads,
                    source_file,
                    &definitions_by_name,
                    &mut nodes,
                    &mut seen_nodes,
                    &mut edges,
                    &mut seen_edges,
                );
            }
        }
    }

    let references = Regex::new(&format!(r"(?i)\bREFERENCES\s+({identifier})"))?;
    let alter = Regex::new(&format!(r"(?im)^\s*ALTER\s+TABLE\s+({identifier})"))?;
    for captures in references.captures_iter(&top_level) {
        let matched = captures.get(0).expect("REFERENCES match");
        let target_match = captures.get(1).expect("REFERENCES identifier");
        let target_name = compact_identifier(&text[target_match.start()..target_match.end()]);
        let line = line_of(text, matched.start());
        let source_name = alter
            .captures_iter(&top_level[..matched.start()])
            .last()
            .filter(|capture| {
                // An ALTER belongs to this reference only when no later CREATE
                // starts a new statement before it.
                let alter_start = capture.get(0).expect("ALTER match").start();
                definitions
                    .iter()
                    .filter(|definition| definition.start < matched.start())
                    .map(|definition| definition.start)
                    .max()
                    .is_none_or(|create_start| alter_start > create_start)
            })
            .map(|capture| {
                let source_match = capture.get(1).expect("ALTER identifier");
                compact_identifier(&text[source_match.start()..source_match.end()])
            })
            .or_else(|| {
                definitions
                    .iter()
                    .filter(|definition| {
                        definition.kind == "table" && definition.start < matched.start()
                    })
                    .max_by_key(|definition| definition.start)
                    .map(|definition| definition.name.clone())
            });
        let Some(source_name) = source_name else {
            continue;
        };
        let source = target_id(
            &source_name,
            line,
            source_file,
            &definitions_by_name,
            &mut nodes,
            &mut seen_nodes,
        );
        let target = target_id(
            &target_name,
            line,
            source_file,
            &definitions_by_name,
            &mut nodes,
            &mut seen_nodes,
        );
        push_edge(
            &mut edges,
            &mut seen_edges,
            &source,
            &target,
            "references",
            source_file,
            line,
            Some("foreign_key"),
        );
    }

    // Keep ordering byte-stable across platforms and regex implementation
    // details. The file anchor remains first for upstream compatibility.
    nodes[1..].sort_by(|left, right| left.id.cmp(&right.id));
    edges.sort_by(|left, right| {
        (
            left.true_source(),
            left.true_target(),
            left.relation.as_str(),
        )
            .cmp(&(
                right.true_source(),
                right.true_target(),
                right.relation.as_str(),
            ))
    });
    Ok(Extraction {
        nodes,
        edges,
        hyperedges: Vec::new(),
    })
}

#[allow(clippy::too_many_arguments)]
fn emit_reads_from_masked(
    full_text: &str,
    original_region: &str,
    masked_region: &str,
    region_start: usize,
    definition: &Definition,
    reads: &Regex,
    source_file: &str,
    definitions: &HashMap<String, String>,
    nodes: &mut Vec<Node>,
    seen_nodes: &mut HashSet<String>,
    edges: &mut Vec<Edge>,
    seen_edges: &mut HashSet<(String, String, String)>,
) {
    for captures in reads.captures_iter(masked_region) {
        let matched = captures.get(0).expect("FROM match");
        let identifier = captures.get(1).expect("FROM identifier");
        let target_name =
            compact_identifier(&original_region[identifier.start()..identifier.end()]);
        let line = line_of(full_text, region_start + matched.start());
        let target = target_id(
            &target_name,
            line,
            source_file,
            definitions,
            nodes,
            seen_nodes,
        );
        push_edge(
            edges,
            seen_edges,
            &definition.id,
            &target,
            "reads_from",
            source_file,
            line,
            Some("query"),
        );
    }
}

fn target_id(
    name: &str,
    line: usize,
    source_file: &str,
    definitions: &HashMap<String, String>,
    nodes: &mut Vec<Node>,
    seen_nodes: &mut HashSet<String>,
) -> String {
    if let Some(id) = definitions.get(&identifier_key(name)) {
        return id.clone();
    }
    let id = make_id(&[name]);
    if seen_nodes.insert(id.clone()) {
        nodes.push(node(
            id.clone(),
            name,
            "",
            line,
            "reference",
            Some(source_file),
        ));
    }
    id
}

fn node(
    id: String,
    label: &str,
    source_file: &str,
    line: usize,
    kind: &str,
    origin_file: Option<&str>,
) -> Node {
    let mut extra = BTreeMap::from([
        ("_origin".into(), "sql".into()),
        ("type".into(), kind.into()),
    ]);
    if let Some(origin_file) = origin_file {
        extra.insert("origin_file".into(), origin_file.into());
    }
    Node {
        id,
        label: label.to_owned(),
        file_type: "code".into(),
        source_file: source_file.to_owned(),
        source_location: Some(format!("L{line}")),
        community: None,
        extra,
    }
}

#[allow(clippy::too_many_arguments)]
fn push_edge(
    edges: &mut Vec<Edge>,
    seen: &mut HashSet<(String, String, String)>,
    source: &str,
    target: &str,
    relation: &str,
    source_file: &str,
    line: usize,
    context: Option<&str>,
) {
    if !seen.insert((source.to_owned(), target.to_owned(), relation.to_owned())) {
        return;
    }
    let mut extra = BTreeMap::from([
        ("_src".into(), source.into()),
        ("_tgt".into(), target.into()),
        ("source_location".into(), format!("L{line}").into()),
        ("weight".into(), 1.0.into()),
    ]);
    if let Some(context) = context {
        extra.insert("context".into(), context.into());
    }
    edges.push(Edge {
        source: source.to_owned(),
        target: target.to_owned(),
        relation: relation.to_owned(),
        confidence: Confidence::Extracted,
        source_file: source_file.to_owned(),
        extra,
    });
}

fn compact_identifier(value: &str) -> String {
    value
        .split('.')
        .map(str::trim)
        .collect::<Vec<_>>()
        .join(".")
}

fn identifier_key(value: &str) -> String {
    compact_identifier(value).to_ascii_lowercase()
}

fn line_of(text: &str, offset: usize) -> usize {
    text[..offset].bytes().filter(|byte| *byte == b'\n').count() + 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn byte_entrypoint_does_not_require_a_source_file() {
        let extraction = extract_sql_bytes(
            Path::new("missing.sql"),
            "db/missing.sql",
            b"CREATE TABLE accounts (id int);",
        )
        .expect("extract in-memory SQL source");
        assert!(extraction.nodes.iter().any(|node| node.label == "accounts"));
    }

    #[test]
    fn recovers_quoted_routines_and_schema_names_without_duplicates() {
        let mut fixture = tempfile::NamedTempFile::new().expect("SQL fixture");
        fixture
            .write_all(
                br#"CREATE TABLE "public"."accounts" (id int);
CREATE OR REPLACE FUNCTION "public"."raise_notice_fn"() RETURNS void AS $$
BEGIN RAISE NOTICE 'hi'; END;
$$ LANGUAGE plpgsql;
CREATE TABLE Sales.Customer (id int REFERENCES "public"."accounts");"#,
            )
            .expect("write SQL fixture");
        let extraction = extract_sql(fixture.path(), "db/schema.sql").expect("extract SQL");
        let labels: Vec<_> = extraction
            .nodes
            .iter()
            .map(|node| node.label.as_str())
            .collect();
        assert!(labels.contains(&r#""public"."accounts""#));
        assert!(labels.contains(&r#""public"."raise_notice_fn"()"#));
        assert!(labels.contains(&"Sales.Customer"));
        assert_eq!(
            extraction
                .nodes
                .iter()
                .map(|node| &node.id)
                .collect::<HashSet<_>>()
                .len(),
            extraction.nodes.len()
        );
        assert!(extraction
            .edges
            .iter()
            .any(|edge| edge.relation == "references"));
    }

    #[test]
    fn masks_non_code_at_the_top_level_and_inside_routines() {
        let source = br#"-- CREATE TABLE line_ghost (id int REFERENCES line_target);
/* CREATE TABLE block_ghost (id int);
   /* ALTER TABLE accounts ADD FOREIGN KEY (id) REFERENCES block_target; */
*/
CREATE TABLE users_ (id int);
CREATE TABLE "public"."accounts" (id int REFERENCES users_);
CREATE OR REPLACE FUNCTION "public"."read_accounts"() RETURNS void AS $fn$
BEGIN
  RAISE NOTICE 'FROM users_';
  -- JOIN users_
  EXECUTE 'SELECT * FROM users_' || suffix;
  PERFORM $sql$SELECT * FROM users_$sql$;
  CREATE TABLE body_ghost (id int);
  SELECT * FROM "public"."accounts";
END;
$fn$ LANGUAGE plpgsql;
CREATE PROCEDURE refresh_accounts() AS $proc$
BEGIN
  /* FROM users_ */
  SELECT * FROM "public"."accounts";
END;
$proc$ LANGUAGE plpgsql;
CREATE VIEW report AS
SELECT * FROM "public"."accounts"
WHERE note = 'FROM users_';
SELECT 'REFERENCES string_ghost';
-- ALTER TABLE accounts ADD FOREIGN KEY (id) REFERENCES trailing_ghost;
"#;
        let extraction = extract_sql_bytes(Path::new("schema.sql"), "db/schema.sql", source)
            .expect("extract masked SQL");

        for ghost in [
            "line_ghost",
            "line_target",
            "block_ghost",
            "block_target",
            "body_ghost",
            "string_ghost",
            "trailing_ghost",
        ] {
            assert!(
                extraction
                    .nodes
                    .iter()
                    .all(|node| !node.label.contains(ghost)),
                "phantom SQL node {ghost}"
            );
        }

        let node_id = |label: &str| {
            extraction
                .nodes
                .iter()
                .find(|node| node.label == label)
                .unwrap_or_else(|| panic!("SQL node {label}"))
                .id
                .clone()
        };
        let users = node_id("users_");
        let accounts = node_id(r#""public"."accounts""#);
        let function = node_id(r#""public"."read_accounts"()"#);
        let procedure = node_id("refresh_accounts()");
        let view = node_id("report");
        let has_edge = |source: &str, target: &str, relation: &str| {
            extraction.edges.iter().any(|edge| {
                edge.true_source() == source
                    && edge.true_target() == target
                    && edge.relation == relation
            })
        };

        assert!(has_edge(&accounts, &users, "references"));
        assert!(has_edge(&function, &accounts, "reads_from"));
        assert!(has_edge(&procedure, &accounts, "reads_from"));
        assert!(has_edge(&view, &accounts, "reads_from"));
        assert!(!has_edge(&function, &users, "reads_from"));
        assert!(!has_edge(&procedure, &users, "reads_from"));
        assert!(!has_edge(&view, &users, "reads_from"));
    }
}
