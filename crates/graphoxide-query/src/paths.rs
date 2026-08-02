//! Shortest paths and node explanations.

use crate::query::{find_node, score_nodes, search_tokens, GraphIndex};
use graphoxide_core::{sanitize_label, KnowledgeGraph};
use std::collections::{BTreeSet, HashMap, VecDeque};

fn endpoint(index: &GraphIndex<'_>, query: &str) -> Option<usize> {
    let scored = score_nodes(
        index,
        &query
            .split_whitespace()
            .map(str::to_lowercase)
            .collect::<Vec<_>>(),
    );
    if scored.is_empty() {
        return None;
    }
    let wanted: BTreeSet<_> = search_tokens(query).into_iter().collect();
    scored
        .iter()
        .find(|(_, p)| {
            wanted.is_subset(&search_tokens(&index.node(*p).label).into_iter().collect())
        })
        .map(|(_, p)| *p)
        .or(Some(scored[0].1))
}

pub fn shortest_path(graph: &KnowledgeGraph, from: &str, to: &str) -> String {
    let index = GraphIndex::new(graph);
    let Some(source) = endpoint(&index, from) else {
        return format!("No node matching '{from}' found.");
    };
    let Some(target) = endpoint(&index, to) else {
        return format!("No node matching '{to}' found.");
    };
    if source == target {
        return format!("'{from}' and '{to}' both resolved to the same node '{}'. Use a more specific label or the exact node ID.", index.node(source).id);
    }
    let mut previous = HashMap::new();
    let mut queue = VecDeque::from([source]);
    previous.insert(source, None);
    while let Some(current) = queue.pop_front() {
        if current == target {
            break;
        }
        let mut neighbors: Vec<_> = graph
            .links
            .iter()
            .enumerate()
            .filter_map(|(edge_index, edge)| {
                index
                    .other(edge, current)
                    .map(|other| (index.node(other).id.as_str(), other, edge_index))
            })
            .collect();
        neighbors.sort_by_key(|(id, _, _)| *id);
        neighbors.dedup_by_key(|(_, other, _)| *other);
        for (_, next, edge_index) in neighbors {
            if let std::collections::hash_map::Entry::Vacant(entry) = previous.entry(next) {
                entry.insert(Some((current, edge_index)));
                queue.push_back(next);
            }
        }
    }
    if !previous.contains_key(&target) {
        return format!("No path found between '{from}' and '{to}'.");
    }
    let mut steps = Vec::new();
    let mut current = target;
    while let Some(Some((prior, edge))) = previous.get(&current).copied() {
        steps.push((prior, edge, current));
        current = prior;
    }
    steps.reverse();
    let hops = steps.len();
    let mut segments = vec![sanitize_label(&index.node(source).label)];
    for (prior, _, next) in steps {
        let mut forward = Vec::new();
        let mut backward = Vec::new();
        for edge in &graph.links {
            if edge.true_source() == index.node(prior).id
                && edge.true_target() == index.node(next).id
            {
                forward.push(edge);
            } else if edge.true_source() == index.node(next).id
                && edge.true_target() == index.node(prior).id
            {
                backward.push(edge);
            }
        }
        let (edges, is_forward) = if forward.is_empty() {
            (&backward, false)
        } else {
            (&forward, true)
        };
        let relations: BTreeSet<_> = edges
            .iter()
            .map(|e| {
                if e.relation.is_empty() {
                    "related"
                } else {
                    e.relation.as_str()
                }
            })
            .collect();
        let confidences: BTreeSet<_> = edges
            .iter()
            .map(|e| {
                serde_json::to_string(&e.confidence)
                    .unwrap()
                    .trim_matches('"')
                    .to_owned()
            })
            .collect();
        let relation = relations.into_iter().collect::<Vec<_>>().join("/");
        let confidence = if confidences.is_empty() {
            String::new()
        } else {
            format!(
                " [{}]",
                confidences.into_iter().collect::<Vec<_>>().join("/")
            )
        };
        if is_forward {
            segments.push(format!(
                "--{relation}{confidence}--> {}",
                sanitize_label(&index.node(next).label)
            ));
        } else {
            segments.push(format!(
                "<--{relation}{confidence}-- {}",
                sanitize_label(&index.node(next).label)
            ));
        }
    }
    format!("Shortest path ({hops} hops):\n  {}", segments.join(" "))
}

pub fn explain(graph: &KnowledgeGraph, query: &str) -> String {
    let index = GraphIndex::new(graph);
    let matches = find_node(&index, query);
    let Some(&position) = matches.first() else {
        return format!("No node matching '{query}' found.");
    };
    let winning = &index.node(position);
    let rivals: Vec<_> = matches
        .iter()
        .copied()
        .filter(|p| {
            index.node(*p).label.to_lowercase() == winning.label.to_lowercase()
                && index.node(*p).source_file != winning.source_file
        })
        .collect();
    if !rivals.is_empty() {
        let mut lines = vec![
            format!(
                "Ambiguous: '{query}' matches {} nodes in different files.",
                rivals.len() + 1
            ),
            format!("  {}\n    id: {}", winning.source_file, winning.id),
        ];
        for p in rivals {
            lines.push(format!(
                "  {}\n    id: {}",
                index.node(p).source_file,
                index.node(p).id
            ));
        }
        lines.push("Retry with the repo-relative path or the full node id.".into());
        return lines.join("\n");
    }
    let node = index.node(position);
    let community = node
        .extra
        .get("community_name")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .or_else(|| node.community.map(|v| v.to_string()))
        .unwrap_or_default();
    let mut lines = vec![
        format!("Node: {}", sanitize_label(&node.label)),
        format!("  ID:        {}", sanitize_label(&node.id)),
        format!(
            "  Source:    {}{}",
            sanitize_label(&node.source_file),
            node.source_location
                .as_ref()
                .map(|v| format!(" {}", sanitize_label(v)))
                .unwrap_or_default()
        ),
        format!("  Type:      {}", sanitize_label(&node.file_type)),
        format!("  Community: {}", sanitize_label(&community)),
        format!("  Degree:    {}", index.degree(position)),
    ];
    let mut connections = Vec::new();
    for (edge_index, edge) in graph.links.iter().enumerate() {
        if edge.true_source() == node.id {
            if let Some(other) = index.position(edge.true_target()) {
                connections.push((false, other, edge_index));
            }
        } else if edge.true_target() == node.id {
            if let Some(other) = index.position(edge.true_source()) {
                connections.push((true, other, edge_index));
            }
        }
    }
    connections.sort_by_key(|connection| std::cmp::Reverse(index.degree(connection.1)));
    if !connections.is_empty() {
        lines.push(String::new());
        lines.push(format!("Connections ({}):", connections.len()));
        for (incoming, other, edge_index) in connections.iter().take(20) {
            let edge = &graph.links[*edge_index];
            let confidence = serde_json::to_string(&edge.confidence).unwrap();
            let location = edge
                .extra
                .get("source_location")
                .and_then(|v| v.as_str())
                .map(|v| format!(" {}:{}", edge.source_file, v))
                .unwrap_or_default();
            lines.push(format!(
                "  {} {} [{}] [{}]{}",
                if *incoming { "<--" } else { "-->" },
                sanitize_label(&index.node(*other).label),
                sanitize_label(&edge.relation),
                confidence.trim_matches('"'),
                location
            ));
        }
        if connections.len() > 20 {
            lines.push(format!("  ... and {} more", connections.len() - 20));
        }
    }
    lines.join("\n")
}
