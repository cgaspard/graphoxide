//! Markdown architecture report.

use graphoxide_core::{sanitize_label, KnowledgeGraph};
use graphoxide_graph::Analysis;

pub fn render_report(graph: &KnowledgeGraph, analysis: &Analysis) -> String {
    let communities = graph
        .nodes
        .iter()
        .filter_map(|n| n.community)
        .collect::<std::collections::BTreeSet<_>>();
    let mut out=format!("# Graphoxide Knowledge Graph Report\n\n- Nodes: {}\n- Edges: {}\n- Communities: {}\n\n## Architectural hubs\n\n",graph.nodes.len(),graph.links.len(),communities.len());
    let sources = graph
        .nodes
        .iter()
        .filter(|n| !n.source_file.is_empty())
        .map(|n| n.source_file.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    out.push_str(&format!("- Source files: {}\n", sources.len()));
    if analysis.god_nodes.is_empty() {
        out.push_str("No hubs found.\n")
    } else {
        for (rank, node) in analysis.god_nodes.iter().enumerate() {
            out.push_str(&format!(
                "{}. **{}** — {} connections\n",
                rank + 1,
                sanitize_label(&node.label),
                node.degree
            ));
        }
    }
    out.push_str("\n## Communities\n\n");
    for cid in communities {
        let mut members: Vec<_> = graph
            .nodes
            .iter()
            .filter(|n| n.community == Some(cid))
            .collect();
        members.sort_by(|a, b| a.label.cmp(&b.label).then_with(|| a.id.cmp(&b.id)));
        let name = members
            .first()
            .and_then(|n| n.extra.get("community_name"))
            .and_then(|v| v.as_str())
            .unwrap_or("Unnamed");
        out.push_str(&format!(
            "### {} (community {})\n\n",
            sanitize_label(name),
            cid
        ));
        for node in members.iter().take(20) {
            out.push_str(&format!(
                "- {} — `{}`\n",
                sanitize_label(&node.label),
                sanitize_label(&node.source_file)
            ));
        }
        if members.len() > 20 {
            out.push_str(&format!("- … {} more\n", members.len() - 20));
        }
        out.push('\n');
    }
    out.push_str("\n## Surprising connections\n\n");
    if analysis.surprising_connections.is_empty() {
        out.push_str("No surprising connections found.\n")
    } else {
        for item in &analysis.surprising_connections {
            out.push_str(&format!(
                "- **{}** → **{}** (`{}`): {}\n",
                sanitize_label(&item.source),
                sanitize_label(&item.target),
                sanitize_label(&item.relation),
                sanitize_label(&item.why)
            ));
        }
    }
    out.push_str("\n## Suggested questions\n\n");
    for question in &analysis.suggested_questions {
        out.push_str(&format!("- {}\n", sanitize_label(question)));
    }
    let inferred = graph
        .links
        .iter()
        .filter(|e| e.confidence == graphoxide_core::Confidence::Inferred)
        .count();
    let ambiguous = graph
        .links
        .iter()
        .filter(|e| e.confidence == graphoxide_core::Confidence::Ambiguous)
        .count();
    out.push_str(&format!(
        "\n## Confidence audit\n\n- Extracted: {}\n- Inferred: {}\n- Ambiguous: {}\n",
        graph.links.len().saturating_sub(inferred + ambiguous),
        inferred,
        ambiguous
    ));
    out
}
