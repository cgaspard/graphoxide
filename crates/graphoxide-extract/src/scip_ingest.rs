//! Ingestion for Graphify's simplified SCIP-style JSON interchange.
//!
//! This intentionally consumes the defensive JSON shape used by upstream's
//! `scip_ingest.py`, not the official SCIP protobuf schema. Documents contain
//! symbol records, and symbol records contain occurrences and relationships.

use graphoxide_core::{sanitize_metadata, Confidence, Edge, Extraction, Node};
use serde_json::{Map, Value};
use sha1::{Digest, Sha1};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug)]
struct SymbolRecord<'a> {
    node_id: String,
    symbol_id: String,
    doc_path: String,
    raw: &'a Map<String, Value>,
}

/// Convert a simplified SCIP-style JSON value using upstream's default source
/// file and language fallbacks.
pub fn ingest_scip_json(doc: &Value) -> Extraction {
    ingest_scip_json_with_defaults(doc, "", "python")
}

/// Convert a simplified SCIP-style JSON value into an endpoint-safe extraction.
///
/// Arbitrary deserialized JSON is accepted. Invalid containers and records are
/// ignored, while unresolved or ambiguous relationship targets become external
/// stub nodes so emitted edges never dangle.
pub fn ingest_scip_json_with_defaults(
    doc: &Value,
    source_file: &str,
    language: &str,
) -> Extraction {
    let mut extraction = Extraction::default();
    let Some(root) = doc.as_object() else {
        return extraction;
    };
    let Some(documents) = root.get("documents").and_then(Value::as_array) else {
        return extraction;
    };

    let mut per_document = BTreeMap::<(String, String), String>::new();
    let mut global = BTreeMap::<String, Vec<String>>::new();
    let mut records = Vec::new();

    for document in documents {
        let Some(document) = document.as_object() else {
            continue;
        };
        let doc_path = coerce_str(document.get("relative_path"), source_file).to_owned();
        // Language is part of the accepted contract even though symbol kind is
        // currently sufficient for the Graphoxide node schema.
        let _doc_language = coerce_str(document.get("language"), language);
        let Some(symbols) = document.get("symbols").and_then(Value::as_array) else {
            continue;
        };

        for symbol in symbols {
            let Some(raw) = symbol.as_object() else {
                continue;
            };
            let symbol_id = coerce_str(raw.get("symbol"), "");
            if symbol_id.is_empty() {
                continue;
            }
            let node_id = make_scip_node_id(symbol_id, &doc_path);
            per_document
                .entry((symbol_id.to_owned(), doc_path.clone()))
                .or_insert_with(|| node_id.clone());
            let candidates = global.entry(symbol_id.to_owned()).or_default();
            if !candidates.contains(&node_id) {
                candidates.push(node_id.clone());
            }
            records.push(SymbolRecord {
                node_id,
                symbol_id: symbol_id.to_owned(),
                doc_path: doc_path.clone(),
                raw,
            });
        }
    }

    let mut seen_node_ids = BTreeSet::new();
    let mut seen_edges = BTreeSet::new();
    for record in &records {
        emit_symbol_node(record, &mut extraction.nodes, &mut seen_node_ids);
        emit_relationships(
            record,
            &per_document,
            &global,
            &mut extraction,
            &mut seen_node_ids,
            &mut seen_edges,
        );
    }
    extraction
}

fn emit_symbol_node(
    record: &SymbolRecord<'_>,
    nodes: &mut Vec<Node>,
    seen_node_ids: &mut BTreeSet<String>,
) {
    if !seen_node_ids.insert(record.node_id.clone()) {
        return;
    }
    let kind = coerce_str(record.raw.get("kind"), "unknown");
    let display_name = coerce_str(record.raw.get("display_name"), "");
    let description = record
        .raw
        .get("documentation")
        .and_then(Value::as_array)
        .and_then(|values| values.first())
        .and_then(Value::as_str)
        .unwrap_or("");
    let source_location = source_location(record.raw.get("occurrences"));
    let suffix = symbol_suffix(&record.symbol_id);
    let label = if !display_name.is_empty() {
        display_name
    } else if !suffix.is_empty() {
        suffix
    } else {
        &record.symbol_id
    };

    let metadata = sanitize_metadata(Some(&build_scip_metadata(
        &record.symbol_id,
        kind,
        description,
    )));
    let mut extra = BTreeMap::new();
    extra.insert("metadata".into(), Value::Object(metadata));
    nodes.push(Node {
        id: record.node_id.clone(),
        label: label.to_owned(),
        file_type: scip_kind_to_file_type(kind).to_owned(),
        source_file: record.doc_path.clone(),
        source_location: Some(source_location),
        community: None,
        extra,
    });
}

fn emit_relationships(
    record: &SymbolRecord<'_>,
    per_document: &BTreeMap<(String, String), String>,
    global: &BTreeMap<String, Vec<String>>,
    extraction: &mut Extraction,
    seen_node_ids: &mut BTreeSet<String>,
    seen_edges: &mut BTreeSet<(String, String, String, String)>,
) {
    let Some(relationships) = record.raw.get("relationships").and_then(Value::as_array) else {
        return;
    };
    let source_location = source_location(record.raw.get("occurrences"));

    for relationship in relationships {
        let Some(relationship) = relationship.as_object() else {
            continue;
        };
        let target_symbol = coerce_str(relationship.get("symbol"), "");
        if target_symbol.is_empty() {
            continue;
        }

        let resolved_target =
            resolve_relationship_target(target_symbol, &record.doc_path, per_document, global);
        let is_external = resolved_target.is_none();
        let target_node_id = resolved_target
            .cloned()
            .unwrap_or_else(|| make_scip_node_id(target_symbol, &record.doc_path));

        if is_external && seen_node_ids.insert(target_node_id.clone()) {
            let suffix = symbol_suffix(target_symbol);
            let label = if suffix.is_empty() {
                target_symbol
            } else {
                suffix
            };
            let metadata =
                sanitize_metadata(Some(&build_scip_metadata(target_symbol, "external", "")));
            let mut extra = BTreeMap::new();
            extra.insert("metadata".into(), Value::Object(metadata));
            extraction.nodes.push(Node {
                id: target_node_id.clone(),
                label: label.to_owned(),
                file_type: "code".into(),
                source_file: record.doc_path.clone(),
                source_location: Some(String::new()),
                community: None,
                extra,
            });
        }

        let relation = scip_relation_for(relationship).to_owned();
        let key = (
            record.node_id.clone(),
            target_node_id.clone(),
            relation.clone(),
            source_location.clone(),
        );
        if !seen_edges.insert(key) {
            continue;
        }

        let relationship_metadata = Value::Object(relationship.clone());
        let mut metadata = Map::new();
        metadata.insert("scip_relationship".into(), relationship_metadata);
        let mut extra = BTreeMap::new();
        extra.insert("confidence_score".into(), Value::from(1.0));
        extra.insert("source_location".into(), source_location.clone().into());
        extra.insert("weight".into(), Value::from(1.0));
        extra.insert("context".into(), "scip".into());
        extra.insert(
            "metadata".into(),
            Value::Object(sanitize_metadata(Some(&metadata))),
        );
        extraction.edges.push(Edge {
            source: record.node_id.clone(),
            target: target_node_id,
            relation,
            confidence: Confidence::Extracted,
            source_file: record.doc_path.clone(),
            extra,
        });
    }
}

fn resolve_relationship_target<'a>(
    target_symbol: &str,
    source_doc_path: &str,
    per_document: &'a BTreeMap<(String, String), String>,
    global: &'a BTreeMap<String, Vec<String>>,
) -> Option<&'a String> {
    if let Some(same_document) =
        per_document.get(&(target_symbol.to_owned(), source_doc_path.to_owned()))
    {
        return Some(same_document);
    }
    let candidates = global.get(target_symbol)?;
    (candidates.len() == 1).then(|| &candidates[0])
}

fn scip_relation_for(relationship: &Map<String, Value>) -> &'static str {
    if relationship.get("is_implementation") == Some(&Value::Bool(true)) {
        "scip_impl"
    } else if relationship.get("is_type_definition") == Some(&Value::Bool(true)) {
        "scip_typed"
    } else if relationship.get("is_definition") == Some(&Value::Bool(true)) {
        "scip_def"
    } else {
        "scip_ref"
    }
}

fn source_location(occurrences: Option<&Value>) -> String {
    let line = occurrences
        .and_then(Value::as_array)
        .and_then(|values| values.first())
        .and_then(Value::as_object)
        .and_then(|occurrence| occurrence.get("range"))
        .and_then(Value::as_array)
        .and_then(|range| range.first())
        .and_then(Value::as_i64)
        .filter(|line| *line > 0);
    line.map_or_else(String::new, |line| format!("L{line}"))
}

fn coerce_str<'a>(value: Option<&'a Value>, default: &'a str) -> &'a str {
    value.and_then(Value::as_str).unwrap_or(default)
}

fn symbol_suffix(symbol: &str) -> &str {
    symbol.rsplit_once('#').map_or(symbol, |(_, suffix)| suffix)
}

/// Derive upstream's stable, document-scoped SCIP node identifier.
pub fn make_scip_node_id(symbol: &str, source_file: &str) -> String {
    let digest = Sha1::digest(format!("{source_file}:{symbol}").as_bytes());
    let hash = &hex::encode(digest)[..12];
    let sanitized: String = symbol_suffix(symbol)
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' {
                character.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    let suffix = sanitized.trim_matches('_');
    if suffix.is_empty() {
        format!("scip_{hash}")
    } else {
        format!("scip_{suffix}_{hash}")
    }
}

/// All simplified SCIP symbols are code entities; their precise kind remains
/// available in metadata.
pub fn scip_kind_to_file_type(_kind: &str) -> &'static str {
    "code"
}

/// Build the unsanitized metadata payload used for a SCIP symbol node.
pub fn build_scip_metadata(symbol_id: &str, kind: &str, description: &str) -> Map<String, Value> {
    let mut metadata = Map::new();
    metadata.insert("scip_symbol".into(), symbol_id.into());
    metadata.insert("scip_kind".into(), kind.into());
    if !description.is_empty() {
        metadata.insert("scip_description".into(), description.into());
    }
    metadata
}
