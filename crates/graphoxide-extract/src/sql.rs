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

pub(crate) fn extract_sql(path: &Path, source_file: &str) -> anyhow::Result<Extraction> {
    let text = fs::read_to_string(path)?;
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
    let identifier =
        r#"(?:"[^"]+"|[A-Za-z_][A-Za-z0-9_$]*)(?:\s*\.\s*(?:"[^"]+"|[A-Za-z_][A-Za-z0-9_$]*))*"#;
    let create = Regex::new(&format!(
        r"(?im)^\s*CREATE\s+(?:OR\s+REPLACE\s+)?(TABLE|VIEW|FUNCTION|PROCEDURE)\s+(?:IF\s+NOT\s+EXISTS\s+)?({identifier})"
    ))?;
    let mut definitions = Vec::new();
    let mut definitions_by_name = HashMap::<String, String>::new();
    for captures in create.captures_iter(&text) {
        let matched = captures.get(0).expect("CREATE match");
        let kind = captures[1].to_ascii_lowercase();
        let name = compact_identifier(&captures[2]);
        let id = make_id(&[&stem, &name]);
        let line = line_of(&text, matched.start());
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

    // Split dependency scans at the next CREATE declaration. Dollar-quoted
    // PL/pgSQL can contain semicolons freely, so semicolon splitting is unsafe.
    for (index, definition) in definitions.iter().enumerate() {
        let end = definitions
            .get(index + 1)
            .map_or(text.len(), |next| next.start);
        let body = &text[definition.start..end];
        let reads = Regex::new(&format!(r"(?i)\b(?:FROM|JOIN)\s+({identifier})"))?;
        if matches!(definition.kind.as_str(), "view" | "function" | "procedure") {
            for captures in reads.captures_iter(body) {
                let target_name = compact_identifier(&captures[1]);
                let line = line_of(
                    &text,
                    definition.start + captures.get(0).expect("FROM match").start(),
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
                    &definition.id,
                    &target,
                    "reads_from",
                    source_file,
                    line,
                    Some("query"),
                );
            }
        }
    }

    let references = Regex::new(&format!(r"(?i)\bREFERENCES\s+({identifier})"))?;
    let alter = Regex::new(&format!(r"(?im)^\s*ALTER\s+TABLE\s+({identifier})"))?;
    for captures in references.captures_iter(&text) {
        let matched = captures.get(0).expect("REFERENCES match");
        let target_name = compact_identifier(&captures[1]);
        let line = line_of(&text, matched.start());
        let source_name = alter
            .captures_iter(&text[..matched.start()])
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
            .map(|capture| compact_identifier(&capture[1]))
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
}
