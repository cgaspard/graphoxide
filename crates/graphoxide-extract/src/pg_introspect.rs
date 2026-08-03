//! Read-only PostgreSQL catalog introspection.
//!
//! Catalog rows are converted directly to graph records. This avoids allowing
//! one unparseable extension routine to consume later foreign-key DDL during
//! SQL parser error recovery.

use graphoxide_core::{make_id, Confidence, Edge, Extraction, Node};
use postgres::{Client, NoTls};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_CATALOG_ROWS: usize = 100_000;
const MAX_IDENTIFIER_BYTES: usize = 4_096;

pub const TABLE_QUERY: &str = r#"
    SELECT table_schema, table_name, table_type
    FROM information_schema.tables
    WHERE table_schema NOT IN ('pg_catalog', 'information_schema')
    ORDER BY table_schema, table_name
    LIMIT 100001;
"#;

pub const VIEW_QUERY: &str = r#"
    SELECT table_schema, table_name, view_definition
    FROM information_schema.views
    WHERE table_schema NOT IN ('pg_catalog', 'information_schema')
    ORDER BY table_schema, table_name
    LIMIT 100001;
"#;

pub const ROUTINE_QUERY: &str = r#"
    SELECT routine_schema, routine_name, routine_type,
           routine_definition, external_language
    FROM information_schema.routines
    WHERE routine_schema NOT IN ('pg_catalog', 'information_schema')
    ORDER BY routine_schema, routine_name
    LIMIT 100001;
"#;

/// Foreign keys come from `pg_constraint`, not the privilege-filtered
/// `information_schema.referential_constraints` view. The correlated arrays
/// preserve composite-key order while returning one row per constraint.
pub const FOREIGN_KEY_QUERY: &str = r#"
    SELECT
        con.conname AS constraint_name,
        ns.nspname AS table_schema,
        rel.relname AS table_name,
        (SELECT ARRAY_AGG(att.attname ORDER BY k.ord)
           FROM UNNEST(con.conkey) WITH ORDINALITY AS k(attnum, ord)
           JOIN pg_catalog.pg_attribute att
             ON att.attrelid = con.conrelid AND att.attnum = k.attnum
        ) AS columns,
        fns.nspname AS foreign_table_schema,
        frel.relname AS foreign_table_name,
        (SELECT ARRAY_AGG(att.attname ORDER BY k.ord)
           FROM UNNEST(con.confkey) WITH ORDINALITY AS k(attnum, ord)
           JOIN pg_catalog.pg_attribute att
             ON att.attrelid = con.confrelid AND att.attnum = k.attnum
        ) AS foreign_columns
    FROM pg_catalog.pg_constraint con
    JOIN pg_catalog.pg_class rel ON rel.oid = con.conrelid
    JOIN pg_catalog.pg_namespace ns ON ns.oid = rel.relnamespace
    JOIN pg_catalog.pg_class frel ON frel.oid = con.confrelid
    JOIN pg_catalog.pg_namespace fns ON fns.oid = frel.relnamespace
    WHERE con.contype = 'f'
      AND ns.nspname NOT IN ('pg_catalog', 'information_schema')
    ORDER BY ns.nspname, rel.relname, con.conname
    LIMIT 100001;
"#;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PgTable {
    pub schema: String,
    pub name: String,
    pub table_type: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PgView {
    pub schema: String,
    pub name: String,
    pub definition: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PgRoutine {
    pub schema: String,
    pub name: String,
    pub routine_type: String,
    pub definition: Option<String>,
    pub external_language: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PgForeignKey {
    pub constraint_name: String,
    pub table_schema: String,
    pub table_name: String,
    pub columns: Vec<String>,
    pub foreign_schema: String,
    pub foreign_table: String,
    pub foreign_columns: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PgCatalog {
    pub host: String,
    pub dbname: String,
    pub tables: Vec<PgTable>,
    pub views: Vec<PgView>,
    pub routines: Vec<PgRoutine>,
    pub foreign_keys: Vec<PgForeignKey>,
}

/// Errors returned by an injectable catalog source. Public injection keeps the
/// parity suite hermetic: it never opens a socket or requires a database.
#[derive(Clone, Debug, thiserror::Error)]
pub enum PgCatalogSourceError {
    #[error("PostgreSQL driver is unavailable: {0}")]
    DriverUnavailable(String),
    #[error("PostgreSQL connection failed: {0}")]
    Connection(String),
    #[error("PostgreSQL catalog query failed: {0}")]
    Query(String),
}

pub trait PostgresCatalogSource {
    fn load_catalog(&self, dsn: Option<&str>) -> Result<PgCatalog, PgCatalogSourceError>;
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PgIntrospectionError {
    #[error("PostgreSQL support is unavailable: {0}")]
    DriverUnavailable(String),
    #[error("could not connect to PostgreSQL: {0}")]
    Connection(String),
    #[error("could not query PostgreSQL catalogs: {0}")]
    Query(String),
    #[error("PostgreSQL catalog {catalog} exceeds the {MAX_CATALOG_ROWS}-row limit")]
    TooManyRows { catalog: &'static str },
}

/// Use the synchronous Rust PostgreSQL driver to inspect a live database.
pub fn introspect_postgres(dsn: Option<&str>) -> Result<Extraction, PgIntrospectionError> {
    introspect_postgres_with_source(dsn, &LivePostgresCatalogSource)
}

/// Introspect using an injected catalog source. This is also useful to embed
/// Graphoxide in applications that already own a database connection pool.
pub fn introspect_postgres_with_source(
    dsn: Option<&str>,
    source: &impl PostgresCatalogSource,
) -> Result<Extraction, PgIntrospectionError> {
    let catalog = source.load_catalog(dsn).map_err(|error| match error {
        PgCatalogSourceError::DriverUnavailable(message) => {
            PgIntrospectionError::DriverUnavailable(message)
        }
        PgCatalogSourceError::Connection(message) => PgIntrospectionError::Connection(
            sanitize_connection_error(&message, dsn.unwrap_or_default()),
        ),
        PgCatalogSourceError::Query(message) => PgIntrospectionError::Query(first_line(&message)),
    })?;
    extraction_from_catalog(&catalog)
}

/// Convert a materialized catalog to a deterministic, credential-free graph.
pub fn extraction_from_catalog(catalog: &PgCatalog) -> Result<Extraction, PgIntrospectionError> {
    check_catalog_size("tables", catalog.tables.len())?;
    check_catalog_size("views", catalog.views.len())?;
    check_catalog_size("routines", catalog.routines.len())?;
    check_catalog_size("foreign keys", catalog.foreign_keys.len())?;

    let host = portable_component(&catalog.host, "localhost");
    let dbname = portable_component(&catalog.dbname, "db");
    let source_file = format!("postgresql:/{host}/{dbname}");
    let stem = std::path::Path::new(&source_file)
        .with_extension("")
        .to_string_lossy()
        .replace('\\', "/");
    let file_id = make_id(&[&stem]);
    let mut nodes = BTreeMap::<String, Node>::new();
    nodes.insert(
        file_id.clone(),
        sql_node(file_id.clone(), &dbname, "file", &source_file, 1),
    );
    let mut object_ids = BTreeMap::<(String, String), String>::new();
    let mut object_id_owners = BTreeMap::<String, (String, String)>::new();
    let mut ambiguous_object_ids = BTreeSet::<String>::new();
    let mut edges = Vec::new();
    let mut seen_edges = BTreeSet::new();
    let mut line = 1_usize;

    let mut tables = catalog.tables.clone();
    tables.sort_by(|left, right| {
        (&left.schema, &left.name, &left.table_type).cmp(&(
            &right.schema,
            &right.name,
            &right.table_type,
        ))
    });
    for table in tables {
        if table.table_type != "BASE TABLE"
            || !valid_identifier(&table.schema)
            || !valid_identifier(&table.name)
        {
            continue;
        }
        line += 1;
        let label = qualified_label(&table.schema, &table.name);
        let id = make_id(&[&stem, &label]);
        if id.is_empty() {
            continue;
        }
        let object_key = (table.schema.clone(), table.name.clone());
        if ambiguous_object_ids.contains(&id) {
            continue;
        }
        if let Some(owner) = object_id_owners.get(&id) {
            if owner != &object_key {
                // `make_id` case-folds. PostgreSQL quoted identifiers do not,
                // so Foo and foo must never be silently welded into one FK
                // endpoint. Keep the first node for observability but make
                // both colliding identities ineligible for relationship joins.
                object_ids.remove(owner);
                object_ids.remove(&object_key);
                ambiguous_object_ids.insert(id);
                continue;
            }
        } else {
            object_id_owners.insert(id.clone(), object_key.clone());
        }
        object_ids.entry(object_key).or_insert_with(|| id.clone());
        if nodes.contains_key(&id) {
            continue;
        }
        nodes.insert(
            id.clone(),
            sql_node(id.clone(), &label, "table", &source_file, line),
        );
        push_sql_edge(
            &mut edges,
            &mut seen_edges,
            &file_id,
            &id,
            "contains",
            &source_file,
            line,
            None,
        );
    }

    let mut views = catalog.views.clone();
    views.sort_by(|left, right| (&left.schema, &left.name).cmp(&(&right.schema, &right.name)));
    for view in views {
        if !valid_identifier(&view.schema) || !valid_identifier(&view.name) {
            continue;
        }
        line += 1;
        let label = qualified_label(&view.schema, &view.name);
        let id = make_id(&[&stem, &label]);
        if id.is_empty() || nodes.contains_key(&id) {
            continue;
        }
        nodes.insert(
            id.clone(),
            sql_node(id.clone(), &label, "view", &source_file, line),
        );
        push_sql_edge(
            &mut edges,
            &mut seen_edges,
            &file_id,
            &id,
            "contains",
            &source_file,
            line,
            None,
        );
    }

    // FK relationships are materialized before routines conceptually, and do
    // not depend on routine parsing at all. One constraint yields one edge,
    // even when its key arrays contain multiple columns.
    let mut foreign_keys = catalog.foreign_keys.clone();
    foreign_keys.sort_by(|left, right| {
        (
            &left.table_schema,
            &left.table_name,
            &left.constraint_name,
            &left.foreign_schema,
            &left.foreign_table,
        )
            .cmp(&(
                &right.table_schema,
                &right.table_name,
                &right.constraint_name,
                &right.foreign_schema,
                &right.foreign_table,
            ))
    });
    for foreign_key in foreign_keys {
        if !valid_identifier(&foreign_key.constraint_name)
            || !valid_identifier(&foreign_key.table_schema)
            || !valid_identifier(&foreign_key.table_name)
            || !valid_identifier(&foreign_key.foreign_schema)
            || !valid_identifier(&foreign_key.foreign_table)
            || foreign_key.columns.is_empty()
            || foreign_key.columns.len() != foreign_key.foreign_columns.len()
            || foreign_key
                .columns
                .iter()
                .any(|value| !valid_identifier(value))
            || foreign_key
                .foreign_columns
                .iter()
                .any(|value| !valid_identifier(value))
        {
            continue;
        }
        let Some(source) = object_ids.get(&(
            foreign_key.table_schema.clone(),
            foreign_key.table_name.clone(),
        )) else {
            continue;
        };
        let Some(target) = object_ids.get(&(
            foreign_key.foreign_schema.clone(),
            foreign_key.foreign_table.clone(),
        )) else {
            continue;
        };
        line += 1;
        push_sql_edge(
            &mut edges,
            &mut seen_edges,
            source,
            target,
            "references",
            &source_file,
            line,
            Some("foreign_key"),
        );
    }

    let mut routines = catalog.routines.clone();
    routines.sort_by(|left, right| {
        (&left.schema, &left.name, &left.routine_type).cmp(&(
            &right.schema,
            &right.name,
            &right.routine_type,
        ))
    });
    for routine in routines {
        if !matches!(routine.routine_type.as_str(), "FUNCTION" | "PROCEDURE")
            || !valid_identifier(&routine.schema)
            || !valid_identifier(&routine.name)
        {
            continue;
        }
        line += 1;
        let qualified = qualified_label(&routine.schema, &routine.name);
        let id = make_id(&[&stem, &qualified]);
        if id.is_empty() || nodes.contains_key(&id) {
            continue;
        }
        let label = format!("{qualified}()");
        // Upstream represents procedures as functions because the SQL grammar
        // used for the reconstructed DDL parses that form reliably.
        nodes.insert(
            id.clone(),
            sql_node(id.clone(), &label, "function", &source_file, line),
        );
        push_sql_edge(
            &mut edges,
            &mut seen_edges,
            &file_id,
            &id,
            "contains",
            &source_file,
            line,
            None,
        );
    }

    let file = nodes.remove(&file_id).expect("file node inserted above");
    let mut ordered_nodes = vec![file];
    ordered_nodes.extend(nodes.into_values());
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
        nodes: ordered_nodes,
        edges,
        hyperedges: Vec::new(),
    })
}

pub struct LivePostgresCatalogSource;

impl PostgresCatalogSource for LivePostgresCatalogSource {
    fn load_catalog(&self, dsn: Option<&str>) -> Result<PgCatalog, PgCatalogSourceError> {
        let dsn = dsn.unwrap_or_default();
        let mut client = Client::connect(dsn, NoTls)
            .map_err(|error| PgCatalogSourceError::Connection(error.to_string()))?;
        client
            .batch_execute("BEGIN TRANSACTION ISOLATION LEVEL SERIALIZABLE READ ONLY DEFERRABLE")
            .map_err(|error| PgCatalogSourceError::Query(error.to_string()))?;

        let table_rows = client
            .query(TABLE_QUERY, &[])
            .map_err(|error| PgCatalogSourceError::Query(error.to_string()))?;
        bounded_rows("tables", table_rows.len())?;
        let tables = table_rows
            .into_iter()
            .map(|row| PgTable {
                schema: row.get(0),
                name: row.get(1),
                table_type: row.get(2),
            })
            .collect();

        let view_rows = client
            .query(VIEW_QUERY, &[])
            .map_err(|error| PgCatalogSourceError::Query(error.to_string()))?;
        bounded_rows("views", view_rows.len())?;
        let views = view_rows
            .into_iter()
            .map(|row| PgView {
                schema: row.get(0),
                name: row.get(1),
                definition: row.get(2),
            })
            .collect();

        let routine_rows = client
            .query(ROUTINE_QUERY, &[])
            .map_err(|error| PgCatalogSourceError::Query(error.to_string()))?;
        bounded_rows("routines", routine_rows.len())?;
        let routines = routine_rows
            .into_iter()
            .map(|row| PgRoutine {
                schema: row.get(0),
                name: row.get(1),
                routine_type: row.get(2),
                definition: row.get(3),
                external_language: row.get(4),
            })
            .collect();

        let foreign_key_rows = client
            .query(FOREIGN_KEY_QUERY, &[])
            .map_err(|error| PgCatalogSourceError::Query(error.to_string()))?;
        bounded_rows("foreign keys", foreign_key_rows.len())?;
        let foreign_keys = foreign_key_rows
            .into_iter()
            .map(|row| PgForeignKey {
                constraint_name: row.get(0),
                table_schema: row.get(1),
                table_name: row.get(2),
                columns: row.get::<_, Option<Vec<String>>>(3).unwrap_or_default(),
                foreign_schema: row.get(4),
                foreign_table: row.get(5),
                foreign_columns: row.get::<_, Option<Vec<String>>>(6).unwrap_or_default(),
            })
            .collect();
        let _ = client.batch_execute("ROLLBACK");

        let (host, dbname) = connection_identity(dsn);
        Ok(PgCatalog {
            host,
            dbname,
            tables,
            views,
            routines,
            foreign_keys,
        })
    }
}

fn bounded_rows(name: &'static str, len: usize) -> Result<(), PgCatalogSourceError> {
    if len > MAX_CATALOG_ROWS {
        Err(PgCatalogSourceError::Query(format!(
            "{name} exceeds the {MAX_CATALOG_ROWS}-row limit"
        )))
    } else {
        Ok(())
    }
}

fn check_catalog_size(catalog: &'static str, len: usize) -> Result<(), PgIntrospectionError> {
    if len > MAX_CATALOG_ROWS {
        Err(PgIntrospectionError::TooManyRows { catalog })
    } else {
        Ok(())
    }
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_IDENTIFIER_BYTES && !value.chars().any(char::is_control)
}

fn quote_identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn qualified_label(schema: &str, name: &str) -> String {
    format!("{}.{}", quote_identifier(schema), quote_identifier(name))
}

fn sql_node(id: String, label: &str, kind: &str, source_file: &str, line: usize) -> Node {
    Node {
        id,
        label: label.into(),
        file_type: "code".into(),
        source_file: source_file.into(),
        source_location: Some(format!("L{line}")),
        community: None,
        extra: BTreeMap::from([
            ("_origin".into(), "sql".into()),
            ("type".into(), kind.into()),
        ]),
    }
}

#[allow(clippy::too_many_arguments)]
fn push_sql_edge(
    edges: &mut Vec<Edge>,
    seen: &mut BTreeSet<(String, String, String)>,
    source: &str,
    target: &str,
    relation: &str,
    source_file: &str,
    line: usize,
    context: Option<&str>,
) {
    if source.is_empty()
        || target.is_empty()
        || !seen.insert((source.into(), target.into(), relation.into()))
    {
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
        source: source.into(),
        target: target.into(),
        relation: relation.into(),
        confidence: Confidence::Extracted,
        source_file: source_file.into(),
        extra,
    });
}

fn portable_component(value: &str, fallback: &str) -> String {
    let value = value.trim();
    let value = if value.is_empty() { fallback } else { value };
    let mut clean = value
        .chars()
        .take(255)
        .map(|character| match character {
            '/' | '\\' => '_',
            character if character.is_control() => '_',
            character => character,
        })
        .collect::<String>();
    if matches!(clean.as_str(), "." | "..") {
        clean = fallback.into();
    }
    clean
}

fn connection_identity(dsn: &str) -> (String, String) {
    let trimmed = dsn.trim();
    if let Some(rest) = trimmed
        .strip_prefix("postgresql://")
        .or_else(|| trimmed.strip_prefix("postgres://"))
    {
        let rest = rest.split(['?', '#']).next().unwrap_or(rest);
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        let authority = authority.rsplit('@').next().unwrap_or(authority);
        let host = if let Some(bracketed) = authority.strip_prefix('[') {
            bracketed
                .split_once(']')
                .map(|(host, _)| host)
                .unwrap_or(bracketed)
        } else {
            authority.split(':').next().unwrap_or(authority)
        };
        let dbname = path.split('/').next().unwrap_or(path);
        return (
            portable_component(host, "localhost"),
            portable_component(dbname, "db"),
        );
    }
    let host = dsn_keyword(trimmed, "host").unwrap_or_else(|| "localhost".into());
    let dbname = dsn_keyword(trimmed, "dbname").unwrap_or_else(|| "db".into());
    (
        portable_component(&host, "localhost"),
        portable_component(&dbname, "db"),
    )
}

fn dsn_keyword(dsn: &str, wanted: &str) -> Option<String> {
    for token in dsn.split_whitespace() {
        let Some((key, value)) = token.split_once('=') else {
            continue;
        };
        if key == wanted {
            return Some(value.trim_matches(['\'', '"']).to_owned());
        }
    }
    None
}

fn first_line(message: &str) -> String {
    message
        .lines()
        .next()
        .unwrap_or("operation failed")
        .to_owned()
}

fn sanitize_connection_error(message: &str, dsn: &str) -> String {
    let mut line = first_line(message);
    if !dsn.is_empty() {
        line = line.replace(dsn, "<redacted DSN>");
    }
    // Scrub URL user-info and libpq password tokens if a driver ever embeds
    // either form in its first-line diagnostic.
    if let Ok(url_credentials) = regex::Regex::new(r"(?i)(postgres(?:ql)?://)[^/@\s]+@") {
        line = url_credentials
            .replace_all(&line, "$1<redacted>@")
            .into_owned();
    }
    if let Ok(password) = regex::Regex::new(r#"(?i)password\s*=\s*('[^']*'|"[^"]*"|\S+)"#) {
        line = password
            .replace_all(&line, "password=<redacted>")
            .into_owned();
    }
    if line.trim().is_empty() {
        "connection attempt failed".into()
    } else {
        line
    }
}
