//! Reverse impact traversal.

use crate::query::{find_node, GraphIndex};
use graphoxide_core::{sanitize_label, KnowledgeGraph};
use std::collections::{HashSet, VecDeque};

pub const DEFAULT_RELATIONS: &[&str] = &[
    "calls",
    "indirect_call",
    "references",
    "imports",
    "imports_from",
    "re_exports",
    "inherits",
    "extends",
    "implements",
    "uses",
    "mixes_in",
    "embeds",
    "requires",
];

pub fn affected(graph: &KnowledgeGraph, query: &str, depth: usize, relations: &[String]) -> String {
    let index = GraphIndex::new(graph);
    let matches = find_node(&index, query);
    let Some(&seed) = matches.first() else {
        return format!("No unique node match for {query}");
    };
    if matches.iter().skip(1).any(|p| {
        index.node(*p).label == index.node(seed).label
            && index.node(*p).source_file != index.node(seed).source_file
    }) {
        return format!("No unique node match for {query}");
    }
    let allowed: HashSet<&str> = if relations.is_empty() {
        DEFAULT_RELATIONS.iter().copied().collect()
    } else {
        relations.iter().map(String::as_str).collect()
    };
    let mut seen = HashSet::from([seed]);
    let mut queue = VecDeque::from([(seed, 0usize)]);
    let mut hits = Vec::new();
    for edge in &graph.links {
        if edge.source == index.node(seed).id
            && matches!(edge.relation.as_str(), "method" | "contains")
        {
            if let Some(member) = index.position(&edge.target) {
                if seen.insert(member) {
                    queue.push_back((member, 0));
                }
            }
        }
    }
    while let Some((current, current_depth)) = queue.pop_front() {
        if current_depth >= depth {
            continue;
        }
        for (edge_index, edge) in graph.links.iter().enumerate() {
            if edge.target != index.node(current).id {
                continue;
            }
            if !allowed.contains(edge.relation.as_str()) {
                continue;
            }
            let Some(source) = index.position(&edge.source) else {
                continue;
            };
            if seen.insert(source) {
                hits.push((source, current_depth + 1, edge_index));
                queue.push_back((source, current_depth + 1));
            }
        }
    }
    let relation_names = if relations.is_empty() {
        DEFAULT_RELATIONS.join(", ")
    } else {
        relations.join(", ")
    };
    let mut lines = vec![
        format!(
            "Affected nodes for {}",
            sanitize_label(&index.node(seed).label)
        ),
        format!("Relations: {relation_names}"),
        format!("Depth: {depth}"),
    ];
    if hits.is_empty() {
        lines.push("No affected nodes found.".into());
    }
    for (position, _, edge_index) in hits {
        let node = index.node(position);
        let edge = &graph.links[edge_index];
        let file = if edge.source_file.is_empty() {
            &node.source_file
        } else {
            &edge.source_file
        };
        let location = edge
            .extra
            .get("source_location")
            .and_then(|v| v.as_str())
            .or(node.source_location.as_deref())
            .unwrap_or("");
        let place = if location.is_empty() {
            sanitize_label(file)
        } else {
            format!("{}:{}", sanitize_label(file), sanitize_label(location))
        };
        lines.push(format!(
            "- {} [{}] {}",
            sanitize_label(&node.label),
            sanitize_label(&edge.relation),
            place
        ));
    }
    lines.join("\n")
}
