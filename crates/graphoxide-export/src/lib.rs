//! Output generation.
//!
//! Port of upstream `report.py` (GRAPH_REPORT.md), `export.py` (Obsidian
//! vault, graph.json, graph.html), `tree_html.py`, and `callflow_html.py`.
//! HTML templates are embedded at compile time — no runtime asset files.
//!
//! Upstream's graph.svg came from matplotlib; the port either implements a
//! small deterministic layout or defers SVG (see HANDOFF.md § "Exports").

pub mod html;
pub mod obsidian;
pub mod report;
pub mod wiki;

pub use html::{
    derive_sections_from_communities, render_callflow_html, render_callflow_html_with_options,
    render_html, render_html_with_options, write_callflow_html, ArchitectureSection, HtmlOptions,
};
pub use obsidian::{
    communities_from_graph, community_labels_from_graph, export_canvas, export_vault,
    export_vault_with_options, node_filenames, obsidian_safe_stem, render_canvas, Communities,
    VaultOptions,
};
pub use report::{
    render_report, render_report_with_options, DetectionSummary, ReportOptions, TokenCost,
};
pub use wiki::{export_wiki, export_wiki_with_options, GodNodeArticle, WikiOptions, WikiReport};

use graphoxide_core::KnowledgeGraph;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

/// Render a deterministic, dependency-oriented text tree. Cycles are marked
/// instead of recursed, which keeps this safe on arbitrary knowledge graphs.
pub fn render_tree(graph: &graphoxide_core::KnowledgeGraph, root: Option<&str>) -> String {
    use std::collections::{BTreeMap, BTreeSet};
    let labels: BTreeMap<_, _> = graph
        .nodes
        .iter()
        .map(|n| (n.id.as_str(), n.label.as_str()))
        .collect();
    let mut children: BTreeMap<&str, Vec<(&str, &str)>> = BTreeMap::new();
    let mut incoming = BTreeSet::new();
    for edge in &graph.links {
        if matches!(
            edge.relation.as_str(),
            "contains" | "method" | "imports" | "imports_from"
        ) {
            children
                .entry(edge.true_source())
                .or_default()
                .push((edge.true_target(), edge.relation.as_str()));
            incoming.insert(edge.true_target());
        }
    }
    for values in children.values_mut() {
        values.sort_by(|a, b| labels.get(a.0).cmp(&labels.get(b.0)).then_with(|| a.cmp(b)));
        values.dedup();
    }
    let roots: Vec<&str> = if let Some(query) = root {
        let q = query.to_lowercase();
        graph
            .nodes
            .iter()
            .filter(|n| n.id == query || n.label.to_lowercase() == q)
            .map(|n| n.id.as_str())
            .take(1)
            .collect()
    } else {
        graph
            .nodes
            .iter()
            .filter(|n| !incoming.contains(n.id.as_str()) && children.contains_key(n.id.as_str()))
            .map(|n| n.id.as_str())
            .collect()
    };
    let mut out = Vec::new();
    for (index, id) in roots.iter().enumerate() {
        if index > 0 {
            out.push(String::new());
        }
        out.push(labels.get(id).copied().unwrap_or(id).to_owned());
        let mut stack = BTreeSet::from([*id]);
        tree_children(id, "", &children, &labels, &mut stack, &mut out);
    }
    if out.is_empty() {
        "No matching tree roots found.".into()
    } else {
        out.join("\n")
    }
}

fn tree_children<'a>(
    id: &'a str,
    prefix: &str,
    children: &std::collections::BTreeMap<&'a str, Vec<(&'a str, &'a str)>>,
    labels: &std::collections::BTreeMap<&'a str, &'a str>,
    stack: &mut std::collections::BTreeSet<&'a str>,
    out: &mut Vec<String>,
) {
    let Some(values) = children.get(id) else {
        return;
    };
    for (index, (child, relation)) in values.iter().enumerate() {
        let last = index + 1 == values.len();
        let branch = if last { "└──" } else { "├──" };
        let cycle = stack.contains(child);
        out.push(format!(
            "{prefix}{branch} {} [{}]{}",
            labels.get(child).copied().unwrap_or(child),
            relation,
            if cycle { " ↩" } else { "" }
        ));
        if !cycle {
            stack.insert(child);
            tree_children(
                child,
                &format!("{prefix}{}", if last { "    " } else { "│   " }),
                children,
                labels,
                stack,
                out,
            );
            stack.remove(child);
        }
    }
}

/// Render importable Cypher using MERGE so rerunning an export is idempotent.
pub fn render_cypher(graph: &graphoxide_core::KnowledgeGraph) -> String {
    let mut out = String::from("CREATE CONSTRAINT graphoxide_node_id IF NOT EXISTS FOR (n:GraphoxideNode) REQUIRE n.id IS UNIQUE;\n");
    for node in &graph.nodes {
        out.push_str(&format!(
            "MERGE (n:GraphoxideNode {{id:'{}'}}) SET n.label='{}', n.file_type='{}', n.source_file='{}';\n",
            cypher(&node.id), cypher(&node.label), cypher(&node.file_type), cypher(&node.source_file)
        ));
    }
    for edge in &graph.links {
        let relation: String = edge
            .relation
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_uppercase()
                } else {
                    '_'
                }
            })
            .collect();
        out.push_str(&format!(
            "MATCH (a:GraphoxideNode {{id:'{}'}}), (b:GraphoxideNode {{id:'{}'}}) MERGE (a)-[:{}]->(b);\n",
            cypher(edge.true_source()), cypher(edge.true_target()), relation
        ));
    }
    out
}

fn cypher(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control() || *character == '\t')
        .collect::<String>()
        .replace('\\', "\\\\")
        .replace('\'', "\\'")
}

pub fn render_graphml(graph: &KnowledgeGraph) -> String {
    let graph_attributes = graphml_graph_attributes(graph);
    let node_attributes: Vec<_> = graph.nodes.iter().map(graphml_node_attributes).collect();
    let edge_attributes: Vec<_> = graph.links.iter().map(graphml_edge_attributes).collect();
    let mut keys: BTreeMap<(GraphmlScope, String), GraphmlType> = BTreeMap::new();
    collect_graphml_keys(&mut keys, GraphmlScope::Graph, &graph_attributes);
    for attributes in &node_attributes {
        collect_graphml_keys(&mut keys, GraphmlScope::Node, attributes);
    }
    for attributes in &edge_attributes {
        collect_graphml_keys(&mut keys, GraphmlScope::Edge, attributes);
    }
    let mut key_ids = BTreeMap::new();
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<graphml xmlns=\"http://graphml.graphdrawing.org/xmlns\">\n",
    );
    for (index, ((scope, name), value_type)) in keys.iter().enumerate() {
        let key = format!("k{index}");
        key_ids.insert((*scope, name.clone()), key.clone());
        out.push_str(&format!(
            "<key id=\"{key}\" for=\"{}\" attr.name=\"{}\" attr.type=\"{}\"/>\n",
            scope.as_str(),
            xml(name),
            value_type.as_str()
        ));
    }
    out.push_str(&format!(
        "<graph id=\"G\" edgedefault=\"{}\">\n",
        if graph.directed {
            "directed"
        } else {
            "undirected"
        }
    ));
    write_graphml_data(&mut out, GraphmlScope::Graph, &graph_attributes, &key_ids);
    for (node, attributes) in graph.nodes.iter().zip(&node_attributes) {
        out.push_str(&format!("<node id=\"{}\">\n", xml(&node.id)));
        write_graphml_data(&mut out, GraphmlScope::Node, attributes, &key_ids);
        out.push_str("</node>\n");
    }
    for (index, (edge, attributes)) in graph.links.iter().zip(&edge_attributes).enumerate() {
        out.push_str(&format!(
            "<edge id=\"e{index}\" source=\"{}\" target=\"{}\">\n",
            xml(edge.true_source()),
            xml(edge.true_target())
        ));
        write_graphml_data(&mut out, GraphmlScope::Edge, attributes, &key_ids);
        out.push_str("</edge>\n");
    }
    out.push_str("</graph>\n</graphml>\n");
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum GraphmlScope {
    Graph,
    Node,
    Edge,
}

impl GraphmlScope {
    fn as_str(self) -> &'static str {
        match self {
            Self::Graph => "graph",
            Self::Node => "node",
            Self::Edge => "edge",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum GraphmlType {
    String,
    Boolean,
    Long,
    Double,
}

impl GraphmlType {
    fn as_str(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Boolean => "boolean",
            Self::Long => "long",
            Self::Double => "double",
        }
    }
}

fn graphml_graph_attributes(graph: &KnowledgeGraph) -> BTreeMap<String, Value> {
    let mut attributes = graph.extra.clone();
    attributes.insert("hyperedges".into(), Value::Array(graph.hyperedges.clone()));
    attributes.retain(|name, _| !name.starts_with('_'));
    attributes
}

fn graphml_node_attributes(node: &graphoxide_core::Node) -> BTreeMap<String, Value> {
    let mut attributes = node.extra.clone();
    attributes.retain(|name, _| !name.starts_with('_'));
    attributes.insert("label".into(), Value::String(node.label.clone()));
    attributes.insert("file_type".into(), Value::String(node.file_type.clone()));
    attributes.insert(
        "source_file".into(),
        Value::String(node.source_file.clone()),
    );
    attributes.insert(
        "community".into(),
        Value::Number(node.community.unwrap_or(-1).into()),
    );
    if let Some(location) = &node.source_location {
        attributes.insert("source_location".into(), Value::String(location.clone()));
    }
    attributes
}

fn graphml_edge_attributes(edge: &graphoxide_core::Edge) -> BTreeMap<String, Value> {
    let mut attributes = edge.extra.clone();
    attributes.retain(|name, _| !name.starts_with('_'));
    attributes.insert("relation".into(), Value::String(edge.relation.clone()));
    attributes.insert(
        "confidence".into(),
        Value::String(format!("{:?}", edge.confidence).to_uppercase()),
    );
    attributes.insert(
        "source_file".into(),
        Value::String(edge.source_file.clone()),
    );
    attributes
}

fn collect_graphml_keys(
    keys: &mut BTreeMap<(GraphmlScope, String), GraphmlType>,
    scope: GraphmlScope,
    attributes: &BTreeMap<String, Value>,
) {
    for (name, value) in attributes {
        let value_type = graphml_type(value);
        keys.entry((scope, name.clone()))
            .and_modify(|existing| {
                if *existing != value_type {
                    *existing = GraphmlType::String;
                }
            })
            .or_insert(value_type);
    }
}

fn graphml_type(value: &Value) -> GraphmlType {
    match value {
        Value::Bool(_) => GraphmlType::Boolean,
        Value::Number(number) if number.is_i64() || number.is_u64() => GraphmlType::Long,
        Value::Number(_) => GraphmlType::Double,
        _ => GraphmlType::String,
    }
}

fn graphml_text(value: &Value, expected: GraphmlType) -> String {
    if expected == GraphmlType::String {
        match value {
            Value::Null => String::new(),
            Value::String(value) => value.clone(),
            Value::Array(_) | Value::Object(_) => {
                serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
            }
            _ => value.to_string(),
        }
    } else {
        value.to_string()
    }
}

fn write_graphml_data(
    out: &mut String,
    scope: GraphmlScope,
    attributes: &BTreeMap<String, Value>,
    key_ids: &BTreeMap<(GraphmlScope, String), String>,
) {
    for (name, value) in attributes {
        let key = &key_ids[&(scope, name.clone())];
        let expected = graphml_type(value);
        let text = graphml_text(value, expected);
        out.push_str(&format!("<data key=\"{key}\">{}</data>\n", xml(&text)));
    }
}

fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Atomically write GraphML. Serialization is completed before the destination
/// is touched, so errors cannot leave a `.tmp` artifact or truncate good output.
pub fn write_graphml(graph: &KnowledgeGraph, output: &Path) -> anyhow::Result<()> {
    graphoxide_core::write_text_atomic(output, &render_graphml(graph))
}

/// Result of inspecting an existing graph for shrink protection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExistingGraphNodeCount {
    NothingToProtect,
    Count(usize),
    Malformed,
}

pub fn existing_graph_node_count(path: &Path) -> ExistingGraphNodeCount {
    if !path.exists() {
        return ExistingGraphNodeCount::NothingToProtect;
    }
    let Ok(bytes) = fs::read(path) else {
        return fs::metadata(path)
            .ok()
            .filter(|metadata| metadata.len() > 0)
            .map_or(ExistingGraphNodeCount::NothingToProtect, |_| {
                ExistingGraphNodeCount::Malformed
            });
    };
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return ExistingGraphNodeCount::NothingToProtect;
    }
    match serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|value| value.get("nodes").and_then(Value::as_array).map(Vec::len))
    {
        Some(count) => ExistingGraphNodeCount::Count(count),
        None => ExistingGraphNodeCount::Malformed,
    }
}

/// Graph JSON export with upstream's fail-closed shrink guard.
pub fn export_graph_json(
    graph: &KnowledgeGraph,
    output: &Path,
    force: bool,
) -> anyhow::Result<bool> {
    if !force {
        match existing_graph_node_count(output) {
            ExistingGraphNodeCount::Count(count) if graph.nodes.len() < count => return Ok(false),
            ExistingGraphNodeCount::Malformed => return Ok(false),
            _ => {}
        }
    }
    graphoxide_core::write_graph_atomic(output, graph, true)?;
    Ok(true)
}

const BACKUP_ARTIFACTS: &[&str] = &[
    "graph.json",
    "GRAPH_REPORT.md",
    ".graphoxide_labels.json",
    ".graphify_labels.json",
    ".graphoxide_analysis.json",
    ".graphify_analysis.json",
    "manifest.json",
    ".graphoxide_semantic_marker",
    ".graphify_semantic_marker",
    "cost.json",
];

/// Snapshot a costly or curated graph before overwriting it.
pub fn backup_if_protected(output_directory: &Path) -> Option<PathBuf> {
    if std::env::var_os("GRAPHOXIDE_NO_BACKUP").is_some()
        || std::env::var_os("GRAPHIFY_NO_BACKUP").is_some()
    {
        return None;
    }
    let graph = output_directory.join("graph.json");
    if !graph.is_file() {
        return None;
    }
    let semantic = [".graphoxide_semantic_marker", ".graphify_semantic_marker"]
        .iter()
        .any(|name| output_directory.join(name).is_file());
    let curated = [".graphoxide_labels.json", ".graphify_labels.json"]
        .iter()
        .any(|name| labels_are_curated(&output_directory.join(name)));
    if !semantic && !curated {
        return None;
    }
    let date = chrono::Local::now()
        .date_naive()
        .format("%Y-%m-%d")
        .to_string();
    let backup = output_directory.join(date);
    if backup.join("graph.json").is_file()
        && fs::read(&graph).ok() == fs::read(backup.join("graph.json")).ok()
    {
        return Some(backup);
    }
    fs::create_dir_all(&backup).ok()?;
    let mut copied = 0;
    for name in BACKUP_ARTIFACTS {
        let source = output_directory.join(name);
        if source.is_file() && fs::copy(&source, backup.join(name)).is_ok() {
            copied += 1;
        }
    }
    (copied > 0).then_some(backup)
}

fn labels_are_curated(path: &Path) -> bool {
    let Ok(bytes) = fs::read(path) else {
        return false;
    };
    let Ok(labels) = serde_json::from_slice::<BTreeMap<String, String>>(&bytes) else {
        return false;
    };
    labels
        .iter()
        .any(|(community, label)| label != &format!("Community {community}"))
}
