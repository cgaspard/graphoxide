//! Reverse impact traversal.

use crate::query::GraphIndex;
use graphoxide_core::{sanitize_label, KnowledgeGraph};
use std::collections::{HashSet, VecDeque};
use unicode_normalization::UnicodeNormalization;

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

fn normalized_label(value: &str) -> String {
    value.nfc().flat_map(char::to_lowercase).collect()
}

fn bare_name(value: &str) -> String {
    let normalized = normalized_label(value);
    normalized
        .strip_suffix("()")
        .unwrap_or(&normalized)
        .to_owned()
}

fn query_basename(query: &str) -> &str {
    query
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(query)
}

fn prefer_file_node(index: &GraphIndex<'_>, matches: &[usize], query: &str) -> Option<usize> {
    let basename = normalized_label(query_basename(query));
    let exact: Vec<_> = matches
        .iter()
        .copied()
        .filter(|position| {
            let node = index.node(*position);
            node.source_location.as_deref() == Some("L1")
                && normalized_label(&node.label) == basename
        })
        .collect();
    if exact.len() == 1 {
        return exact.first().copied();
    }
    let l1: Vec<_> = matches
        .iter()
        .copied()
        .filter(|position| index.node(*position).source_location.as_deref() == Some("L1"))
        .collect();
    if l1.len() == 1 {
        return l1.first().copied();
    }
    let basename_matches: Vec<_> = matches
        .iter()
        .copied()
        .filter(|position| normalized_label(&index.node(*position).label) == basename)
        .collect();
    (basename_matches.len() == 1).then(|| basename_matches[0])
}

/// Resolve an affected traversal seed without guessing across equally-good
/// symbols. Unlike general search, normalization preserves distinct accents.
pub fn resolve_seed(graph: &KnowledgeGraph, query: &str) -> Option<usize> {
    let index = GraphIndex::new(graph);
    let query = query.trim_end_matches(['/', '\\']);
    let query = if query.is_empty() { "/" } else { query };
    if let Some(position) = index.position(query) {
        return Some(position);
    }
    let normalized = normalized_label(query);
    let exact_labels: Vec<_> = graph
        .nodes
        .iter()
        .enumerate()
        .filter_map(|(position, node)| {
            (normalized_label(&node.label) == normalized).then_some(position)
        })
        .collect();
    if exact_labels.len() == 1 {
        return exact_labels.first().copied();
    }
    let bare = bare_name(query);
    let bare_labels: Vec<_> = graph
        .nodes
        .iter()
        .enumerate()
        .filter_map(|(position, node)| (bare_name(&node.label) == bare).then_some(position))
        .collect();
    if bare_labels.len() == 1 {
        return bare_labels.first().copied();
    }
    let exact_sources: Vec<_> = graph
        .nodes
        .iter()
        .enumerate()
        .filter_map(|(position, node)| {
            (normalized_label(&node.source_file) == normalized).then_some(position)
        })
        .collect();
    if exact_sources.len() == 1 {
        return exact_sources.first().copied();
    }
    if !exact_sources.is_empty() {
        if let Some(position) = prefer_file_node(&index, &exact_sources, query) {
            return Some(position);
        }
    }
    let contains: Vec<_> = graph
        .nodes
        .iter()
        .enumerate()
        .filter_map(|(position, node)| {
            normalized_label(&node.label)
                .contains(&normalized)
                .then_some(position)
        })
        .collect();
    (contains.len() == 1).then(|| contains[0])
}

pub fn affected(graph: &KnowledgeGraph, query: &str, depth: usize, relations: &[String]) -> String {
    let index = GraphIndex::new(graph);
    let Some(seed) = resolve_seed(graph, query) else {
        return format!("No unique node match for {query}");
    };
    let allowed: HashSet<&str> = if relations.is_empty() {
        DEFAULT_RELATIONS.iter().copied().collect()
    } else {
        relations.iter().map(String::as_str).collect()
    };
    let mut seen = HashSet::from([seed]);
    let mut queue = VecDeque::from([(seed, 0usize)]);
    let mut hits = Vec::new();
    for edge in &graph.links {
        if edge.true_source() == index.node(seed).id
            && matches!(edge.relation.as_str(), "method" | "contains")
        {
            if let Some(member) = index.position(edge.true_target()) {
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
            if edge.true_target() != index.node(current).id {
                continue;
            }
            if !allowed.contains(edge.relation.as_str()) {
                continue;
            }
            let Some(source) = index.position(edge.true_source()) else {
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

#[cfg(test)]
mod tests {
    use super::*;
    use graphoxide_core::{Confidence, Edge, Node};
    use std::collections::BTreeMap;

    fn node(id: &str, label: &str, source_file: &str, location: Option<&str>) -> Node {
        Node {
            id: id.into(),
            label: label.into(),
            file_type: "code".into(),
            source_file: source_file.into(),
            source_location: location.map(Into::into),
            community: None,
            extra: BTreeMap::new(),
        }
    }

    fn edge(source: &str, target: &str, relation: &str) -> Edge {
        Edge {
            source: source.into(),
            target: target.into(),
            relation: relation.into(),
            confidence: Confidence::Extracted,
            source_file: String::new(),
            extra: BTreeMap::new(),
        }
    }

    fn impact_graph() -> KnowledgeGraph {
        KnowledgeGraph {
            nodes: vec![
                node("target", "Foo", "pkg/foo.py", Some("L1")),
                node("caller", "X()", "app.py", Some("L4")),
                node("barrel", "__init__.py", "pkg/__init__.py", None),
                node("consumer", "app.py", "app.py", None),
            ],
            links: vec![
                edge("caller", "target", "calls"),
                edge("barrel", "target", "re_exports"),
                edge("consumer", "target", "imports"),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn test_affected_cli_reverse_traverses_impact_edges() {
        let out = affected(&impact_graph(), "Foo", 2, &[]);
        for expected in [
            "Affected nodes for Foo",
            "X()",
            "calls",
            "__init__.py",
            "re_exports",
            "app.py",
            "imports",
        ] {
            assert!(out.contains(expected), "missing {expected:?} in {out}");
        }
    }

    #[test]
    fn test_affected_cli_relation_filter_limits_reverse_traversal() {
        let out = affected(&impact_graph(), "Foo", 2, &["calls".into()]);
        assert!(out.contains("Relations: calls"));
        assert!(out.contains("X()"));
        assert!(!out.contains("__init__.py"));
    }

    #[test]
    fn test_affected_cli_forces_directed_on_undirected_graph() {
        let mut call = edge("B", "A", "calls");
        call.extra.insert("_src".into(), serde_json::json!("A"));
        call.extra.insert("_tgt".into(), serde_json::json!("B"));
        let graph = KnowledgeGraph {
            directed: false,
            nodes: vec![
                node("A", "caller_fn", "a.py", Some("L1")),
                node("B", "callee_fn", "b.py", Some("L2")),
            ],
            links: vec![call],
            ..Default::default()
        };
        let out = affected(&graph, "B", 2, &["calls".into()]);
        assert!(out.contains("caller_fn"));
        assert!(!out.contains("No affected nodes found."));
    }

    #[test]
    fn test_affected_cli_loads_edges_keyed_graph() {
        let graph: KnowledgeGraph = serde_json::from_value(serde_json::json!({
            "directed": false,
            "nodes": [
                {"id":"target", "label":"Foo", "source_file":"pkg/foo.py", "source_location":"L1"},
                {"id":"caller", "label":"X()", "source_file":"app.py", "source_location":"L4"}
            ],
            "edges": [{"source":"caller", "target":"target", "relation":"calls", "confidence":"EXTRACTED"}]
        })).expect("edges-keyed graph");
        let out = affected(&graph, "Foo", 2, &[]);
        assert!(out.contains("X()"));
        assert!(out.contains("calls"));
    }

    #[test]
    fn test_resolve_seed_bare_name_matches_callable_label() {
        let graph = KnowledgeGraph {
            nodes: vec![
                node("a", "classifyProperty()", "pkg/entity.py", None),
                node("b", "classifyPropertySafe()", "app/context.py", None),
            ],
            ..Default::default()
        };
        assert_eq!(resolve_seed(&graph, "classifyProperty"), Some(0));
        assert_eq!(resolve_seed(&graph, "classifyPropertySafe"), Some(1));
    }

    #[test]
    fn test_resolve_seed_decorated_query_matches_bare_label() {
        let graph = KnowledgeGraph {
            nodes: vec![
                node("a", "Foo", "pkg/foo.py", None),
                node("b", "FooBar", "pkg/foobar.py", None),
            ],
            ..Default::default()
        };
        assert_eq!(resolve_seed(&graph, "Foo()"), Some(0));
    }

    #[test]
    fn test_resolve_seed_matches_unicode_normalized_label() {
        let graph = KnowledgeGraph {
            nodes: vec![node("a", "Auditoría", "pkg/auditoria.py", None)],
            ..Default::default()
        };
        assert_eq!(resolve_seed(&graph, "Auditori\u{301}a"), Some(0));
    }

    #[test]
    fn test_resolve_seed_preserves_distinct_accents() {
        let graph = KnowledgeGraph {
            nodes: vec![
                node("a", "resume", "pkg/resume.py", None),
                node("b", "résumé", "pkg/resume_accented.py", None),
            ],
            ..Default::default()
        };
        assert_eq!(resolve_seed(&graph, "resume"), Some(0));
    }

    #[test]
    fn test_resolve_seed_bare_name_tie_still_returns_none() {
        let graph = KnowledgeGraph {
            nodes: vec![
                node("a", "dup()", "pkg/one.py", None),
                node("b", "dup()", "pkg/two.py", None),
            ],
            ..Default::default()
        };
        assert_eq!(resolve_seed(&graph, "dup"), None);
    }

    fn source_file_graph(include_file_node: bool) -> KnowledgeGraph {
        let source = "app/api/example/route.ts";
        let mut nodes = vec![node("get", "GET()", source, Some("L42"))];
        if include_file_node {
            nodes.push(node("file", "route.ts", source, Some("L1")));
        } else {
            nodes.push(node("post", "POST()", source, Some("L20")));
        }
        KnowledgeGraph {
            nodes,
            ..Default::default()
        }
    }

    #[test]
    fn test_resolve_seed_source_file_path_prefers_file_level_node() {
        assert_eq!(
            resolve_seed(&source_file_graph(true), "app/api/example/route.ts"),
            Some(1)
        );
    }

    #[test]
    fn test_resolve_seed_source_file_trailing_slash_parity() {
        assert_eq!(
            resolve_seed(&source_file_graph(true), "app/api/example/route.ts/"),
            Some(1)
        );
    }

    #[test]
    fn test_resolve_seed_source_file_ambiguous_no_file_node_returns_none() {
        assert_eq!(
            resolve_seed(&source_file_graph(false), "app/api/example/route.ts"),
            None
        );
    }

    #[test]
    fn test_affected_cli_source_file_path_uses_file_level_node() {
        let mut graph = source_file_graph(true);
        graph.nodes.push(node(
            "consumer",
            "consumer.ts",
            "app/consumer.ts",
            Some("L1"),
        ));
        graph.links.push(edge("consumer", "file", "imports_from"));
        let out = affected(&graph, "app/api/example/route.ts", 2, &[]);
        assert!(out.contains("Affected nodes for route.ts"));
        assert!(out.contains("consumer.ts"));
        assert!(out.contains("imports_from"));
    }

    #[test]
    fn test_affected_reports_call_site_line_not_def_line() {
        let mut call = edge("loader", "transition", "calls");
        call.source_file = "apollo_pipeline_status.py".into();
        call.extra
            .insert("source_location".into(), serde_json::json!("L158"));
        let graph = KnowledgeGraph {
            nodes: vec![
                node(
                    "loader",
                    "_load_apollo_app_state()",
                    "apollo_pipeline_status.py",
                    Some("L90"),
                ),
                node("transition", "transition_state()", "state.py", Some("L56")),
            ],
            links: vec![call],
            ..Default::default()
        };
        let out = affected(&graph, "transition_state", 2, &[]);
        assert!(out.contains("apollo_pipeline_status.py:L158"));
        assert!(!out.contains("apollo_pipeline_status.py:L90"));
    }

    #[test]
    fn test_affected_falls_back_to_def_line_when_edge_has_no_location() {
        let graph = KnowledgeGraph {
            nodes: vec![
                node("loader", "load()", "a.py", Some("L90")),
                node("target", "target()", "b.py", Some("L5")),
            ],
            links: vec![edge("loader", "target", "calls")],
            ..Default::default()
        };
        assert!(affected(&graph, "target", 2, &[]).contains("a.py:L90"));
    }

    fn member_seed_graph() -> KnowledgeGraph {
        KnowledgeGraph {
            nodes: vec![
                node("proc", "Processor", "processor.py", None),
                node("proc_call", ".call()", "processor.py", None),
                node("runner", "Runner", "runner.py", None),
                node("runner_run", ".run()", "runner.py", None),
            ],
            links: vec![
                edge("proc", "proc_call", "method"),
                edge("runner", "runner_run", "method"),
                edge("runner_run", "proc_call", "calls"),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn test_class_affected_reaches_method_bound_caller() {
        let out = affected(&member_seed_graph(), "Processor", 2, &[]);
        assert!(out.contains(".run()"));
    }

    #[test]
    fn test_member_method_node_not_reported_as_hit() {
        let out = affected(&member_seed_graph(), "Processor", 2, &[]);
        assert!(!out.lines().any(|line| line.starts_with("- .call()")));
    }

    #[test]
    fn test_method_contains_still_excluded_from_general_walk() {
        let graph = KnowledgeGraph {
            nodes: vec![
                node("a", "A", "a.py", None),
                node("a_m", ".m()", "a.py", None),
                node("b", "B", "b.py", None),
                node("b_m", ".n()", "b.py", None),
            ],
            links: vec![
                edge("a", "a_m", "method"),
                edge("a_m", "b", "calls"),
                edge("b", "b_m", "method"),
            ],
            ..Default::default()
        };
        assert!(!affected(&graph, "A", 3, &[]).contains(".n()"));
    }

    #[test]
    fn test_class_level_caller_still_works() {
        let graph = KnowledgeGraph {
            nodes: vec![
                node("svc", "Svc", "svc.py", None),
                node("caller", ".use()", "caller.py", None),
            ],
            links: vec![edge("caller", "svc", "references")],
            ..Default::default()
        };
        assert!(affected(&graph, "Svc", 2, &[]).contains(".use()"));
    }
}
