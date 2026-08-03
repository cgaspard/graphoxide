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
        let missing_relation = edges.iter().all(|edge| edge.relation.is_empty());
        let confidences: BTreeSet<_> = if missing_relation {
            BTreeSet::new()
        } else {
            edges
                .iter()
                .map(|e| {
                    serde_json::to_string(&e.confidence)
                        .unwrap()
                        .trim_matches('"')
                        .to_owned()
                })
                .collect()
        };
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
    explain_with_overlay(graph, query, None)
}

pub fn explain_with_overlay(
    graph: &KnowledgeGraph,
    query: &str,
    overlay: Option<&serde_json::Map<String, serde_json::Value>>,
) -> String {
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
    ];
    if let Some(entry) = overlay
        .and_then(|entries| entries.get(&node.id))
        .and_then(serde_json::Value::as_object)
    {
        let status = entry
            .get("status")
            .and_then(serde_json::Value::as_str)
            .map(sanitize_label)
            .unwrap_or_default();
        let uses = entry
            .get("uses")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        let score = entry
            .get("score")
            .map(serde_json::Value::to_string)
            .unwrap_or_else(|| "0".into());
        let mut lesson = match status.as_str() {
            "contested" => format!(
                "  Lesson: contested (useful {uses} / dead-end {})",
                entry
                    .get("neg")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(0)
            ),
            "preferred" => {
                format!("  Lesson: preferred source (start here) — {uses} useful, score={score}")
            }
            _ => format!(
                "  Lesson: {} — {uses} useful, score={score}",
                if status.is_empty() {
                    "tentative"
                } else {
                    &status
                }
            ),
        };
        if entry
            .get("stale")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            lesson.push_str(" [code changed since — re-verify]");
        }
        lines.push(lesson);
    }
    lines.push(format!("  Degree:    {}", index.degree(position)));
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
            let remainder = &connections[20..];
            lines.push(format!("  ... and {} more", remainder.len()));
            let mut by_file: HashMap<(bool, String), usize> = HashMap::new();
            for (incoming, _, edge_index) in remainder {
                let edge = &graph.links[*edge_index];
                let file = if edge.source_file.is_empty() {
                    "(unknown file)"
                } else {
                    &edge.source_file
                };
                *by_file.entry((*incoming, file.to_owned())).or_default() += 1;
            }
            let mut grouped: Vec<_> = by_file.into_iter().collect();
            grouped.sort_by(
                |((incoming_a, file_a), count_a), ((incoming_b, file_b), count_b)| {
                    count_b
                        .cmp(count_a)
                        .then_with(|| incoming_a.cmp(incoming_b))
                        .then_with(|| file_a.cmp(file_b))
                },
            );
            lines.push("  Grouped by file:".into());
            for ((incoming, file), count) in grouped.iter().take(20) {
                lines.push(format!(
                    "    {} {}: {} {}",
                    if *incoming { "<--" } else { "-->" },
                    sanitize_label(file),
                    count,
                    if *count == 1 {
                        "connection"
                    } else {
                        "connections"
                    }
                ));
            }
            if grouped.len() > 20 {
                lines.push(format!("    ... and {} more files", grouped.len() - 20));
            }
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use graphoxide_core::{Confidence, Edge, Node};
    use std::collections::BTreeMap;

    fn node(id: &str, label: &str, source: &str, location: Option<&str>) -> Node {
        Node {
            id: id.into(),
            label: label.into(),
            file_type: "code".into(),
            source_file: source.into(),
            source_location: location.map(Into::into),
            community: Some(0),
            extra: BTreeMap::new(),
        }
    }

    fn edge(source: &str, target: &str, relation: &str, confidence: Confidence) -> Edge {
        Edge {
            source: source.into(),
            target: target.into(),
            relation: relation.into(),
            confidence,
            source_file: String::new(),
            extra: BTreeMap::new(),
        }
    }

    fn single_call_graph() -> KnowledgeGraph {
        KnowledgeGraph {
            nodes: vec![
                node(
                    "create_patch",
                    "createPatchHandler()",
                    "server/create-patch-handler.ts",
                    None,
                ),
                node(
                    "validate",
                    "validateSanitySession()",
                    "server/sanity-validate-session.ts",
                    None,
                ),
            ],
            links: vec![edge(
                "create_patch",
                "validate",
                "calls",
                Confidence::Extracted,
            )],
            ..Default::default()
        }
    }

    #[test]
    fn test_forward_arrow() {
        let out = shortest_path(
            &single_call_graph(),
            "createPatchHandler",
            "validateSanitySession",
        );
        assert!(out.contains("Shortest path (1 hops):"));
        assert!(out.contains("createPatchHandler() --calls [EXTRACTED]--> validateSanitySession()"));
    }

    #[test]
    fn test_reverse_arrow() {
        let out = shortest_path(
            &single_call_graph(),
            "validateSanitySession",
            "createPatchHandler",
        );
        assert!(out.contains("validateSanitySession() <--calls [EXTRACTED]-- createPatchHandler()"));
        assert!(
            !out.contains("validateSanitySession() --calls [EXTRACTED]--> createPatchHandler()")
        );
    }

    fn misranking_graph() -> KnowledgeGraph {
        let mut nodes = vec![
            node("target", "Degenerate Reject-Everything Judge", "", None),
            node("decoy", "Rejection Summary", "", None),
        ];
        for index in 0..30 {
            nodes.push(node(
                &format!("j{index}"),
                &format!("Judge Helper {index}"),
                "",
                None,
            ));
            nodes.push(node(
                &format!("e{index}"),
                &format!("Everything Widget {index}"),
                "",
                None,
            ));
        }
        KnowledgeGraph {
            nodes,
            links: vec![
                edge("target", "j0", "verified_by", Confidence::Extracted),
                edge("decoy", "e0", "mentions", Confidence::Extracted),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn test_endpoint_prefers_full_token_match() {
        let out = shortest_path(
            &misranking_graph(),
            "Reject-everything judge",
            "Judge Helper 0",
        );
        assert!(out.contains("Degenerate Reject-Everything Judge"));
        assert!(!out.contains("No path found"));
    }

    #[test]
    fn test_endpoint_falls_back_to_score_head() {
        let out = shortest_path(&misranking_graph(), "Rejection judge", "Judge Helper 0");
        assert!(out.contains("No path found"));
    }

    fn diamond_graph() -> KnowledgeGraph {
        KnowledgeGraph {
            nodes: vec![
                node("a", "Alpha", "a.py", None),
                node("p", "Pmid", "p.py", None),
                node("q", "Qmid", "q.py", None),
                node("b", "Beta", "b.py", None),
            ],
            links: vec![
                edge("a", "p", "calls", Confidence::Extracted),
                edge("p", "b", "calls", Confidence::Extracted),
                edge("a", "q", "calls", Confidence::Extracted),
                edge("q", "b", "calls", Confidence::Extracted),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn test_path_deterministic_across_hash_seeds() {
        let graph = diamond_graph();
        let outputs: Vec<_> = (0..8)
            .map(|_| shortest_path(&graph, "Alpha", "Beta"))
            .collect();
        assert!(outputs.windows(2).all(|pair| pair[0] == pair[1]));
        assert!(outputs[0].contains("Pmid"));
    }

    #[test]
    fn test_path_relation_matches_stored_edge_not_fabricated() {
        let graph = KnowledgeGraph {
            nodes: vec![
                node("a", "Alpha", "a.py", None),
                node("b", "Beta", "b.py", None),
            ],
            links: vec![edge("a", "b", "references", Confidence::Inferred)],
            ..Default::default()
        };
        let out = shortest_path(&graph, "Alpha", "Beta");
        assert!(out.contains("--references [INFERRED]-->"));
        assert!(!out.contains("calls"));
    }

    #[test]
    fn test_path_relation_fallback_related_when_missing() {
        let graph = KnowledgeGraph {
            nodes: vec![
                node("a", "Alpha", "a.py", None),
                node("b", "Beta", "b.py", None),
            ],
            links: vec![edge("a", "b", "", Confidence::Extracted)],
            ..Default::default()
        };
        assert!(shortest_path(&graph, "Alpha", "Beta").contains("--related-->"));
    }

    fn flipped_marker_graph() -> KnowledgeGraph {
        let mut flipped = edge("logger", "draft", "imports_from", Confidence::Extracted);
        flipped
            .extra
            .insert("_src".into(), serde_json::json!("draft"));
        flipped
            .extra
            .insert("_tgt".into(), serde_json::json!("logger"));
        KnowledgeGraph {
            nodes: vec![
                node("ingest", "ingest.ts", "src/ingest.ts", None),
                node("logger", "logger.ts", "src/logger.ts", None),
                node(
                    "draft",
                    "draft-generator.ts",
                    "src/draft-generator.ts",
                    None,
                ),
            ],
            links: vec![
                edge("ingest", "logger", "calls", Confidence::Extracted),
                flipped,
            ],
            ..Default::default()
        }
    }

    #[test]
    fn test_path_direction_recovered_from_src_tgt_markers() {
        let out = shortest_path(&flipped_marker_graph(), "ingest", "draft-generator");
        assert!(out.contains("ingest.ts --calls [EXTRACTED]--> logger.ts"));
        assert!(out.contains("logger.ts <--imports_from [EXTRACTED]-- draft-generator.ts"));
        assert!(!out.contains("--imports_from [EXTRACTED]-->"));
    }

    #[test]
    fn test_path_canonical_marker_graph_still_forward() {
        let mut call = edge("a", "b", "calls", Confidence::Extracted);
        call.extra.insert("_src".into(), serde_json::json!("a"));
        call.extra.insert("_tgt".into(), serde_json::json!("b"));
        let graph = KnowledgeGraph {
            nodes: vec![
                node("a", "Alpha", "a.py", None),
                node("b", "Beta", "b.py", None),
            ],
            links: vec![call],
            ..Default::default()
        };
        assert!(
            shortest_path(&graph, "Alpha", "Beta").contains("Alpha --calls [EXTRACTED]--> Beta")
        );
        assert!(
            shortest_path(&graph, "Beta", "Alpha").contains("Beta <--calls [EXTRACTED]-- Alpha")
        );
    }

    #[test]
    fn test_explain_direction_recovered_from_src_tgt_markers() {
        let mut call = edge("hub", "spoke", "calls", Confidence::Extracted);
        call.extra.insert("_src".into(), serde_json::json!("spoke"));
        call.extra.insert("_tgt".into(), serde_json::json!("hub"));
        let graph = KnowledgeGraph {
            nodes: vec![
                node("hub", "hub.ts", "src/hub.ts", None),
                node("spoke", "spoke.ts", "src/spoke.ts", None),
            ],
            links: vec![call],
            ..Default::default()
        };
        let out = explain(&graph, "hub");
        assert!(out.contains("<-- spoke.ts [calls]"));
        assert!(!out.contains("--> spoke.ts"));
    }

    fn explain_graph() -> KnowledgeGraph {
        KnowledgeGraph {
            nodes: vec![
                node(
                    "validate",
                    "validateSanitySession()",
                    "server/sanity-validate-session.ts",
                    None,
                ),
                node(
                    "create_patch",
                    "createPatchHandler()",
                    "server/create-patch-handler.ts",
                    None,
                ),
                node(
                    "create_edit",
                    "createEditHandler()",
                    "server/create-edit-handler.ts",
                    None,
                ),
                node("stable", "stableStringify()", "shared/stringify.ts", None),
            ],
            links: vec![
                edge("create_patch", "validate", "calls", Confidence::Extracted),
                edge("create_edit", "validate", "calls", Confidence::Extracted),
                edge("validate", "stable", "calls", Confidence::Extracted),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn test_callee_shows_callers_as_inbound() {
        let out = explain(&explain_graph(), "validateSanitySession");
        assert!(out.contains("<-- createPatchHandler() [calls]"));
        assert!(out.contains("<-- createEditHandler() [calls]"));
        assert!(out.contains("--> stableStringify() [calls]"));
    }

    #[test]
    fn test_caller_shows_callee_as_outbound() {
        let out = explain(&explain_graph(), "createPatchHandler");
        assert!(out.contains("--> validateSanitySession() [calls]"));
        assert!(!out.contains("<-- "));
    }

    #[test]
    fn test_explain_source_file_path_prefers_file_level_node() {
        let source = "app/api/example/route.ts";
        let graph = KnowledgeGraph {
            nodes: vec![
                node("get", "GET()", source, Some("L42")),
                node("file", "route.ts", source, Some("L1")),
            ],
            links: vec![edge("file", "get", "contains", Confidence::Extracted)],
            ..Default::default()
        };
        let out = explain(&graph, source);
        assert!(out.contains("Node: route.ts"));
        assert!(out.contains("ID:        file"));
        assert!(!out.contains("Node: GET()"));
    }

    #[test]
    fn test_explain_shows_preferred_lesson_line() {
        let overlay = serde_json::json!({
            "validate": {"status":"preferred", "score":2.4, "uses":3, "stale":false}
        });
        let out = explain_with_overlay(
            &explain_graph(),
            "validateSanitySession",
            overlay.as_object(),
        );
        assert!(out.contains("Lesson: preferred source (start here) — 3 useful, score=2.4"));
        assert!(!out.contains("code changed"));
    }

    #[test]
    fn test_explain_shows_contested_and_stale_lesson() {
        let overlay = serde_json::json!({
            "validate": {"status":"contested", "score":-0.1, "uses":2, "neg":1, "stale":true}
        });
        let out = explain_with_overlay(
            &explain_graph(),
            "validateSanitySession",
            overlay.as_object(),
        );
        assert!(out.contains("Lesson: contested (useful 2 / dead-end 1)"));
        assert!(out.contains("[code changed since — re-verify]"));
    }

    #[test]
    fn test_explain_no_lesson_line_for_unannotated_node() {
        assert!(!explain(&explain_graph(), "validateSanitySession").contains("Lesson:"));
    }

    #[test]
    fn test_explain_connection_shows_call_site_line() {
        let mut call = edge("loader", "trans", "calls", Confidence::Extracted);
        call.source_file = "apollo.py".into();
        call.extra
            .insert("source_location".into(), serde_json::json!("L158"));
        let graph = KnowledgeGraph {
            nodes: vec![
                node("loader", "load_state()", "apollo.py", Some("L90")),
                node("trans", "transition_state()", "state.py", Some("L56")),
            ],
            links: vec![call],
            ..Default::default()
        };
        let out = explain(&graph, "transition_state");
        let line = out
            .lines()
            .find(|line| line.contains("<-- load_state()"))
            .expect("inbound caller line");
        assert!(line.contains("apollo.py:L158"));
        assert!(!line.contains("apollo.py:L90"));
    }

    fn high_degree_graph(count: usize, files: &[&str]) -> KnowledgeGraph {
        let mut graph = KnowledgeGraph {
            nodes: vec![node("hub", "hub()", "lib/hub.py", None)],
            ..Default::default()
        };
        for index in 0..count {
            let id = format!("caller_{index}");
            let file = files[index % files.len()];
            graph.nodes.push(node(&id, &format!("{id}()"), file, None));
            let mut call = edge(&id, "hub", "calls", Confidence::Extracted);
            call.source_file = file.into();
            call.extra.insert(
                "source_location".into(),
                serde_json::json!(format!("L{}", 10 + index)),
            );
            graph.links.push(call);
        }
        graph
    }

    #[test]
    fn test_explain_truncation_notice_present_for_high_degree_node() {
        let out = explain(&high_degree_graph(30, &["a.py", "b.py", "c.py"]), "hub");
        assert!(out.contains("Connections (30):"));
        assert!(out.contains("... and 10 more"));
    }

    #[test]
    fn test_explain_groups_cut_callers_by_file_instead_of_dropping_them() {
        let out = explain(
            &high_degree_graph(
                30,
                &[
                    "app/handlers/email.py",
                    "app/jobs/retry.py",
                    "lib/workers/queue.py",
                ],
            ),
            "hub",
        );
        assert!(out.contains("Grouped by file:"));
        assert!(out.contains("<-- lib/workers/queue.py: 4 connections"));
        assert!(out.contains("<-- app/handlers/email.py: 3 connections"));
        assert!(out.contains("<-- app/jobs/retry.py: 3 connections"));
    }

    #[test]
    fn test_explain_no_grouping_section_when_under_cutoff() {
        let out = explain(&high_degree_graph(5, &["a.py"]), "hub");
        assert!(!out.contains("Grouped by file:"));
        assert!(!out.contains("more"));
    }

    #[test]
    fn test_explain_grouping_boundary_at_exactly_21_vs_20_connections() {
        let out21 = explain(&high_degree_graph(21, &["lib/only.py"]), "hub");
        assert!(out21.contains("Grouped by file:"));
        assert!(out21.contains("<-- lib/only.py: 1 connection"));
        let out20 = explain(&high_degree_graph(20, &["lib/only.py"]), "hub");
        assert!(!out20.contains("Grouped by file:"));
        assert!(!out20.contains("more"));
    }

    fn ambiguous_graph(reverse: bool) -> KnowledgeGraph {
        let mut nodes = vec![
            node(
                "chat",
                "MetricsPort",
                "services/chat/src/application/ports/metrics.port.ts",
                None,
            ),
            node(
                "scraping",
                "MetricsPort",
                "services/scraping/src/application/ports/metrics.port.ts",
                None,
            ),
        ];
        if reverse {
            nodes.reverse();
        }
        KnowledgeGraph {
            nodes,
            ..Default::default()
        }
    }

    #[test]
    fn test_explain_ambiguous_label_lists_every_candidate() {
        let out = explain(&ambiguous_graph(false), "MetricsPort");
        assert!(out.contains("Ambiguous"));
        assert!(out.contains("services/chat/src/application/ports/metrics.port.ts"));
        assert!(out.contains("services/scraping/src/application/ports/metrics.port.ts"));
        assert!(!out.contains("Node: MetricsPort\n  ID:"));
    }

    #[test]
    fn test_explain_ambiguous_answer_does_not_depend_on_node_order() {
        let candidate_lines = |out: String| {
            let mut lines: Vec<_> = out
                .lines()
                .filter(|line| line.contains("metrics.port.ts"))
                .map(str::trim)
                .map(str::to_owned)
                .collect();
            lines.sort();
            lines
        };
        assert_eq!(
            candidate_lines(explain(&ambiguous_graph(false), "MetricsPort")),
            candidate_lines(explain(&ambiguous_graph(true), "MetricsPort"))
        );
    }

    #[test]
    fn test_explain_matches_within_one_file_are_not_ambiguous() {
        let source = "services/chat/src/application/ports/metrics.port.ts";
        let graph = KnowledgeGraph {
            nodes: vec![
                node("file", "metrics.port.ts", source, Some("L1")),
                node("member", "MetricsPort", source, Some("L4")),
            ],
            links: vec![edge("file", "member", "contains", Confidence::Extracted)],
            ..Default::default()
        };
        let out = explain(&graph, "MetricsPort");
        assert!(!out.contains("Ambiguous"));
        assert!(out.contains("Node: MetricsPort"));
    }
}
