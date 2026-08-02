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

pub use html::{render_callflow_html, render_html};
pub use obsidian::export_vault;
pub use report::render_report;

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
    value.replace('\\', "\\\\").replace('\'', "\\'")
}

pub fn render_graphml(graph: &graphoxide_core::KnowledgeGraph) -> String {
    let mut out=String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<graphml xmlns=\"http://graphml.graphdrawing.org/xmlns\"><graph edgedefault=\"directed\">\n");
    for node in &graph.nodes {
        out.push_str(&format!(
            "<node id=\"{}\"><data key=\"label\">{}</data></node>\n",
            xml(&node.id),
            xml(&node.label)
        ))
    }
    for (i, edge) in graph.links.iter().enumerate() {
        out.push_str(&format!("<edge id=\"e{i}\" source=\"{}\" target=\"{}\"><data key=\"relation\">{}</data></edge>\n",xml(edge.true_source()),xml(edge.true_target()),xml(&edge.relation)))
    }
    out.push_str("</graph></graphml>\n");
    out
}
fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
