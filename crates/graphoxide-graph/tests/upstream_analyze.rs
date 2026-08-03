//! Behavioral port of pinned `tests/test_analyze.py` (49 collected cases).

use graphoxide_core::{Confidence, Edge, Extraction, KnowledgeGraph, Node};
use graphoxide_graph::{
    build_graph, file_category, find_import_cycles, god_nodes, graph_diff, is_concept_node,
    is_json_key_node, suggest_questions, surprise_score, surprising_connections,
};
use std::collections::{BTreeMap, BTreeSet};

fn node(id: &str, label: &str, source_file: &str, file_type: &str) -> Node {
    Node {
        id: id.into(),
        label: label.into(),
        file_type: file_type.into(),
        source_file: source_file.into(),
        source_location: None,
        community: None,
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

fn graph(nodes: Vec<Node>, links: Vec<Edge>, directed: bool) -> KnowledgeGraph {
    KnowledgeGraph {
        directed,
        multigraph: false,
        nodes,
        links,
        hyperedges: Vec::new(),
        extra: BTreeMap::new(),
    }
}

fn fixture_graph() -> KnowledgeGraph {
    let extraction: Extraction = serde_json::from_str(include_str!(
        "../../../tests/fixtures/upstream/extraction.json"
    ))
    .unwrap();
    build_graph(&[extraction]).unwrap()
}

fn communities(values: &[(i64, &[&str])]) -> BTreeMap<i64, Vec<String>> {
    values
        .iter()
        .map(|(id, nodes)| (*id, nodes.iter().map(|node| (*node).to_owned()).collect()))
        .collect()
}

fn node_communities(values: &[(&str, i64)]) -> BTreeMap<String, i64> {
    values
        .iter()
        .map(|(node, community)| ((*node).to_owned(), *community))
        .collect()
}

fn score(
    graph: &KnowledgeGraph,
    source: &str,
    target: &str,
    edge_index: usize,
    communities: &BTreeMap<String, i64>,
) -> (i64, Vec<String>) {
    let source_file = &graph
        .nodes
        .iter()
        .find(|node| node.id == source)
        .unwrap()
        .source_file;
    let target_file = &graph
        .nodes
        .iter()
        .find(|node| node.id == target)
        .unwrap()
        .source_file;
    surprise_score(
        graph,
        source,
        target,
        &graph.links[edge_index],
        communities,
        source_file,
        target_file,
        None,
    )
}

#[test]
fn test_god_nodes_returns_list() {
    assert!(god_nodes(&fixture_graph(), 3).len() <= 3);
}

#[test]
fn test_god_nodes_sorted_by_degree() {
    let result = god_nodes(&fixture_graph(), 10);
    assert!(result
        .windows(2)
        .all(|pair| pair[0].degree >= pair[1].degree));
}

#[test]
fn test_god_nodes_have_required_keys() {
    let result = god_nodes(&fixture_graph(), 1);
    assert!(!result[0].id.is_empty());
    assert!(!result[0].label.is_empty());
    assert!(result[0].degree > 0);
}

#[test]
fn test_surprising_connections_cross_source_multi_file() {
    let graph = fixture_graph();
    let result = surprising_connections(&graph, &BTreeMap::new(), 5);
    assert!(!result.is_empty());
    assert!(result
        .iter()
        .all(|surprise| surprise.source_files[0] != surprise.source_files[1]));
}

#[test]
fn test_surprising_connections_excludes_concept_nodes() {
    let mut graph = fixture_graph();
    graph
        .nodes
        .push(node("concept_x", "Abstract Concept", "", "document"));
    graph.links.push(edge(
        "n_transformer",
        "concept_x",
        "relates_to",
        Confidence::Inferred,
    ));
    let result = surprising_connections(&graph, &BTreeMap::new(), 10);
    assert!(result.iter().all(|surprise| {
        surprise.source != "Abstract Concept" && surprise.target != "Abstract Concept"
    }));
}

#[test]
fn test_surprising_connections_single_file_uses_community_bridges() {
    let mut nodes = Vec::new();
    let mut links = Vec::new();
    for index in 0..5 {
        nodes.push(node(
            &format!("a{index}"),
            &format!("A{index}"),
            "single.py",
            "code",
        ));
        nodes.push(node(
            &format!("b{index}"),
            &format!("B{index}"),
            "single.py",
            "code",
        ));
        if index < 4 {
            links.push(edge(
                &format!("a{index}"),
                &format!("a{}", index + 1),
                "calls",
                Confidence::Extracted,
            ));
            links.push(edge(
                &format!("b{index}"),
                &format!("b{}", index + 1),
                "calls",
                Confidence::Extracted,
            ));
        }
    }
    links.push(edge("a4", "b0", "references", Confidence::Inferred));
    let graph = graph(nodes, links, false);
    let groups = communities(&[
        (0, &["a0", "a1", "a2", "a3", "a4"]),
        (1, &["b0", "b1", "b2", "b3", "b4"]),
    ]);
    assert!(!surprising_connections(&graph, &groups, 5).is_empty());
}

#[test]
fn test_surprising_connections_ambiguous_scores_higher_than_extracted() {
    let graph = graph(
        vec![
            node("a", "Alpha", "repo1/model.py", "code"),
            node("b", "Beta", "repo2/train.py", "code"),
            node("c", "Gamma", "repo1/data.py", "code"),
            node("d", "Delta", "repo2/eval.py", "code"),
        ],
        vec![
            edge("a", "b", "calls", Confidence::Ambiguous),
            edge("c", "d", "calls", Confidence::Extracted),
        ],
        false,
    );
    let assignments = node_communities(&[("a", 0), ("c", 0), ("b", 1), ("d", 1)]);
    assert!(
        score(&graph, "a", "b", 0, &assignments).0 > score(&graph, "c", "d", 1, &assignments).0
    );
}

#[test]
fn test_surprise_score_accepts_precomputed_degrees() {
    let nodes = vec![
        node("hub", "Hub", "repo1/hub.py", "code"),
        node("leaf", "Leaf", "repo2/leaf.py", "code"),
        node("n1", "N1", "repo1/n1.py", "code"),
        node("n2", "N2", "repo1/n2.py", "code"),
        node("n3", "N3", "repo1/n3.py", "code"),
        node("n4", "N4", "repo1/n4.py", "code"),
    ];
    let links = ["leaf", "n1", "n2", "n3", "n4"]
        .into_iter()
        .map(|target| edge("hub", target, "calls", Confidence::Extracted))
        .collect();
    let graph = graph(nodes, links, false);
    let assignments = node_communities(&[("hub", 0), ("leaf", 1)]);
    let degrees = BTreeMap::from([
        ("hub".into(), 5),
        ("leaf".into(), 1),
        ("n1".into(), 1),
        ("n2".into(), 1),
        ("n3".into(), 1),
        ("n4".into(), 1),
    ]);
    let without = surprise_score(
        &graph,
        "hub",
        "leaf",
        &graph.links[0],
        &assignments,
        "repo1/hub.py",
        "repo2/leaf.py",
        None,
    );
    let with = surprise_score(
        &graph,
        "hub",
        "leaf",
        &graph.links[0],
        &assignments,
        "repo1/hub.py",
        "repo2/leaf.py",
        Some(&degrees),
    );
    assert_eq!(without, with);
}

#[test]
fn test_surprising_connections_cross_type_scores_higher() {
    let graph = graph(
        vec![
            node("a", "Transformer", "code/model.py", "code"),
            node("b", "FlashAttn", "papers/flash.pdf", "code"),
            node("c", "Trainer", "code/train.py", "code"),
            node("d", "Dataset", "code/data.py", "code"),
        ],
        vec![
            edge("a", "b", "references", Confidence::Extracted),
            edge("c", "d", "calls", Confidence::Extracted),
        ],
        false,
    );
    let assignments = node_communities(&[("a", 0), ("b", 1), ("c", 0), ("d", 0)]);
    let cross = score(&graph, "a", "b", 0, &assignments);
    let same = score(&graph, "c", "d", 1, &assignments);
    assert!(cross.0 > same.0);
    assert!(cross
        .1
        .iter()
        .any(|reason| reason.contains("code") && reason.contains("paper")));
}

fn cross_language_graph(relation: &str, confidence: Confidence) -> KnowledgeGraph {
    graph(
        vec![
            node("py_auth", "AuthError", "backend/auth.py", "code"),
            node("ts_member", "Member", "frontend/types.ts", "code"),
            node("py_a", "ServiceA", "backend/service.py", "code"),
            node("py_b", "ServiceB", "backend/utils.py", "code"),
        ],
        vec![
            edge("py_auth", "ts_member", relation, confidence),
            edge("py_a", "py_b", "calls", Confidence::Extracted),
        ],
        false,
    )
}

fn assert_cross_boundary_suppressed(relation: &str) {
    let graph = cross_language_graph(relation, Confidence::Inferred);
    let assignments =
        node_communities(&[("py_auth", 0), ("ts_member", 1), ("py_a", 0), ("py_b", 0)]);
    assert!(
        score(&graph, "py_auth", "ts_member", 0, &assignments).0
            <= score(&graph, "py_a", "py_b", 1, &assignments).0
    );
}

#[test]
fn test_cross_language_inferred_calls_suppressed() {
    assert_cross_boundary_suppressed("calls");
}

#[test]
fn test_cross_language_inferred_uses_suppressed() {
    assert_cross_boundary_suppressed("uses");
}

#[test]
fn test_cross_language_semantically_similar_not_suppressed() {
    let graph = cross_language_graph("semantically_similar_to", Confidence::Inferred);
    let assignments =
        node_communities(&[("py_auth", 0), ("ts_member", 1), ("py_a", 0), ("py_b", 0)]);
    assert!(
        score(&graph, "py_auth", "ts_member", 0, &assignments).0
            > score(&graph, "py_a", "py_b", 1, &assignments).0
    );
}

#[test]
fn test_same_language_inferred_calls_not_suppressed() {
    let graph = graph(
        vec![
            node("py_a", "ModuleA", "src/a.py", "code"),
            node("py_b", "ModuleB", "src/b.py", "code"),
            node("py_c", "ModuleC", "src/c.py", "code"),
            node("py_d", "ModuleD", "src/d.py", "code"),
        ],
        vec![
            edge("py_a", "py_b", "calls", Confidence::Inferred),
            edge("py_c", "py_d", "calls", Confidence::Extracted),
        ],
        false,
    );
    let assignments = node_communities(&[("py_a", 0), ("py_b", 1), ("py_c", 0), ("py_d", 1)]);
    assert!(
        score(&graph, "py_a", "py_b", 0, &assignments).0
            > score(&graph, "py_c", "py_d", 1, &assignments).0
    );
}

#[test]
fn test_cross_language_extracted_calls_not_suppressed() {
    let graph = cross_language_graph("calls", Confidence::Extracted);
    let assignments = node_communities(&[("py_auth", 0), ("ts_member", 1)]);
    assert!(score(&graph, "py_auth", "ts_member", 0, &assignments).0 >= 1);
}

#[test]
fn test_surprising_connections_have_why_field() {
    let graph = fixture_graph();
    for surprise in surprising_connections(&graph, &BTreeMap::new(), 5) {
        assert!(surprise.why.as_deref().is_some_and(|why| !why.is_empty()));
    }
}

#[test]
fn test_file_category() {
    for (path, category) in [
        ("model.py", "code"),
        ("flash.pdf", "paper"),
        ("diagram.png", "image"),
        ("notes.md", "doc"),
        ("app.swift", "code"),
        ("plugin.lua", "code"),
        ("build.zig", "code"),
        ("deploy.ps1", "code"),
        ("server.ex", "code"),
        ("component.jsx", "code"),
        ("analysis.jl", "code"),
        ("view.m", "code"),
    ] {
        assert_eq!(file_category(path), category, "{path}");
    }
}

#[test]
fn test_is_concept_node_empty_source() {
    assert!(is_concept_node(&node("c1", "Concept", "", "concept")));
}

#[test]
fn test_is_concept_node_real_file() {
    assert!(!is_concept_node(&node("n1", "Model", "model.py", "code")));
}

#[test]
fn test_surprising_connections_have_required_keys() {
    let graph = fixture_graph();
    for surprise in surprising_connections(&graph, &BTreeMap::new(), 5) {
        assert!(!surprise.source.is_empty());
        assert!(!surprise.target.is_empty());
        assert_eq!(surprise.source_files.len(), 2);
        assert!(matches!(
            surprise.confidence,
            Confidence::Extracted | Confidence::Inferred | Confidence::Ambiguous
        ));
    }
}

fn simple_graph(
    nodes: &[(&str, &str)],
    edges: &[(&str, &str, &str, Confidence)],
) -> KnowledgeGraph {
    graph(
        nodes
            .iter()
            .map(|(id, label)| node(id, label, "test.py", "code"))
            .collect(),
        edges
            .iter()
            .map(|(source, target, relation, confidence)| {
                edge(source, target, relation, *confidence)
            })
            .collect(),
        false,
    )
}

#[test]
fn test_graph_diff_new_nodes() {
    let old = simple_graph(&[("n1", "Alpha"), ("n2", "Beta")], &[]);
    let new = simple_graph(&[("n1", "Alpha"), ("n2", "Beta"), ("n3", "Gamma")], &[]);
    let diff = graph_diff(&old, &new);
    assert_eq!(diff.new_nodes.len(), 1);
    assert_eq!(diff.new_nodes[0].id, "n3");
    assert_eq!(diff.new_nodes[0].label, "Gamma");
    assert!(diff.removed_nodes.is_empty());
    assert!(diff.summary.contains("1 new node"));
}

#[test]
fn test_graph_diff_removed_nodes() {
    let old = simple_graph(&[("n1", "Alpha"), ("n2", "Beta"), ("n3", "Gamma")], &[]);
    let new = simple_graph(&[("n1", "Alpha"), ("n2", "Beta")], &[]);
    let diff = graph_diff(&old, &new);
    assert!(diff.new_nodes.is_empty());
    assert_eq!(diff.removed_nodes[0].id, "n3");
    assert!(diff.summary.contains("removed"));
}

#[test]
fn test_graph_diff_new_edges() {
    let nodes = [("n1", "Alpha"), ("n2", "Beta"), ("n3", "Gamma")];
    let old = simple_graph(&nodes, &[("n1", "n2", "calls", Confidence::Extracted)]);
    let new = simple_graph(
        &nodes,
        &[
            ("n1", "n2", "calls", Confidence::Extracted),
            ("n2", "n3", "uses", Confidence::Inferred),
        ],
    );
    let diff = graph_diff(&old, &new);
    assert_eq!(diff.new_edges.len(), 1);
    assert_eq!(diff.new_edges[0].relation, "uses");
    assert_eq!(diff.new_edges[0].confidence, Confidence::Inferred);
    assert!(diff.removed_edges.is_empty());
    assert!(diff.summary.contains("new edge"));
}

#[test]
fn test_graph_diff_empty_diff() {
    let graph = simple_graph(
        &[("n1", "Alpha"), ("n2", "Beta")],
        &[("n1", "n2", "calls", Confidence::Extracted)],
    );
    let diff = graph_diff(&graph, &graph);
    assert!(diff.new_nodes.is_empty());
    assert!(diff.removed_nodes.is_empty());
    assert!(diff.new_edges.is_empty());
    assert!(diff.removed_edges.is_empty());
    assert_eq!(diff.summary, "no changes");
}

fn code_doc_graph(relation: &str, confidence: Confidence) -> KnowledgeGraph {
    graph(
        vec![
            node("py_fn", "ProcessData", "src/processor.py", "code"),
            node("md_doc", "README Section", "docs/readme.md", "document"),
            node("py_a", "ServiceA", "src/service.py", "code"),
            node("py_b", "ServiceB", "src/utils.py", "code"),
        ],
        vec![
            edge("py_fn", "md_doc", relation, confidence),
            edge("py_a", "py_b", "calls", Confidence::Extracted),
        ],
        false,
    )
}

fn assert_code_doc_suppressed(relation: &str) {
    let graph = code_doc_graph(relation, Confidence::Inferred);
    let assignments = node_communities(&[("py_fn", 0), ("md_doc", 1), ("py_a", 0), ("py_b", 0)]);
    assert!(
        score(&graph, "py_fn", "md_doc", 0, &assignments).0
            <= score(&graph, "py_a", "py_b", 1, &assignments).0
    );
}

#[test]
fn test_code_doc_inferred_calls_suppressed() {
    assert_code_doc_suppressed("calls");
}

#[test]
fn test_code_doc_inferred_uses_suppressed() {
    assert_code_doc_suppressed("uses");
}

#[test]
fn test_code_doc_extracted_calls_not_suppressed() {
    let graph = code_doc_graph("calls", Confidence::Extracted);
    let assignments = node_communities(&[("py_fn", 0), ("md_doc", 1)]);
    assert!(score(&graph, "py_fn", "md_doc", 0, &assignments).0 >= 1);
}

#[test]
fn test_code_doc_inferred_semantically_similar_not_suppressed() {
    let graph = code_doc_graph("semantically_similar_to", Confidence::Inferred);
    let assignments = node_communities(&[("py_fn", 0), ("md_doc", 1), ("py_a", 0), ("py_b", 0)]);
    assert!(
        score(&graph, "py_fn", "md_doc", 0, &assignments).0
            > score(&graph, "py_a", "py_b", 1, &assignments).0
    );
}

#[test]
fn test_code_unknown_extension_inferred_calls_suppressed() {
    assert_eq!(file_category("vendor/random.xyz"), "doc");
    let graph = graph(
        vec![
            node("py_fn", "Handler", "src/handler.py", "code"),
            node("unk", "Handler", "vendor/unknown.xyz", "document"),
            node("py_a", "A", "src/a.py", "code"),
            node("py_b", "B", "src/b.py", "code"),
        ],
        vec![
            edge("py_fn", "unk", "calls", Confidence::Inferred),
            edge("py_a", "py_b", "calls", Confidence::Extracted),
        ],
        false,
    );
    let assignments = node_communities(&[("py_fn", 0), ("unk", 1), ("py_a", 0), ("py_b", 0)]);
    assert!(
        score(&graph, "py_fn", "unk", 0, &assignments).0
            <= score(&graph, "py_a", "py_b", 1, &assignments).0
    );
}

#[test]
fn test_code_paper_inferred_calls_not_suppressed() {
    let graph = graph(
        vec![
            node("py_model", "Transformer", "src/model.py", "code"),
            node(
                "pdf_paper",
                "Attention Is All You Need",
                "papers/vaswani.pdf",
                "paper",
            ),
            node("py_a", "ServiceA", "src/service.py", "code"),
            node("py_b", "ServiceB", "src/utils.py", "code"),
        ],
        vec![
            edge("py_model", "pdf_paper", "calls", Confidence::Inferred),
            edge("py_a", "py_b", "calls", Confidence::Extracted),
        ],
        false,
    );
    let assignments =
        node_communities(&[("py_model", 0), ("pdf_paper", 1), ("py_a", 0), ("py_b", 1)]);
    assert!(
        score(&graph, "py_model", "pdf_paper", 0, &assignments).0
            > score(&graph, "py_a", "py_b", 1, &assignments).0
    );
}

#[test]
fn test_is_json_key_node_noise_label() {
    assert!(is_json_key_node(&node("j1", "name", "schema.json", "code")));
}

#[test]
fn test_is_json_key_node_non_json_file() {
    assert!(!is_json_key_node(&node("n1", "name", "model.py", "code")));
}

fn assert_npm_dependency_key_filtered(dependency_key: &str) {
    let mut nodes = vec![
        node("real_node", "AuthService", "src/auth.py", "code"),
        node("dep_node", dependency_key, "frontend/package.json", "code"),
    ];
    let mut links = Vec::new();
    for index in 0..20 {
        let id = format!("pkg_{index}");
        nodes.push(node(
            &id,
            &format!("package-{index}"),
            "frontend/package.json",
            "code",
        ));
        links.push(edge("dep_node", &id, "contains", Confidence::Extracted));
    }
    links.push(edge(
        "real_node",
        "dep_node",
        "imports",
        Confidence::Extracted,
    ));
    let ids: BTreeSet<_> = god_nodes(&graph(nodes, links, false), 10)
        .into_iter()
        .map(|node| node.id)
        .collect();
    assert!(!ids.contains("dep_node"));
    assert!(ids.contains("real_node"));
}

#[test]
fn test_god_nodes_excludes_npm_dep_block_keys_dependencies() {
    assert_npm_dependency_key_filtered("dependencies");
}

#[test]
fn test_god_nodes_excludes_npm_dep_block_keys_dev_dependencies() {
    assert_npm_dependency_key_filtered("devDependencies");
}

#[test]
fn test_god_nodes_excludes_npm_dep_block_keys_peer_dependencies() {
    assert_npm_dependency_key_filtered("peerDependencies");
}

#[test]
fn test_god_nodes_excludes_npm_dep_block_keys_optional_dependencies() {
    assert_npm_dependency_key_filtered("optionalDependencies");
}

#[test]
fn test_god_nodes_excludes_npm_dep_block_keys_bundled_dependencies() {
    assert_npm_dependency_key_filtered("bundledDependencies");
}

#[test]
fn test_is_json_key_node_real_label() {
    assert!(!is_json_key_node(&node(
        "j2",
        "UserProfile",
        "schema.json",
        "code"
    )));
}

#[test]
fn test_god_nodes_excludes_json_noise() {
    let mut nodes = vec![
        node("real", "AuthService", "src/auth.py", "code"),
        node("json_name", "name", "schema.json", "code"),
    ];
    let mut links = Vec::new();
    for index in 0..8 {
        let id = format!("peer{index}");
        nodes.push(node(
            &id,
            &format!("Peer{index}"),
            &format!("src/peer{index}.py"),
            "code",
        ));
        links.push(edge("json_name", &id, "", Confidence::Extracted));
        links.push(edge("real", &id, "", Confidence::Extracted));
    }
    let labels: BTreeSet<_> = god_nodes(&graph(nodes, links, false), 10)
        .into_iter()
        .map(|node| node.label)
        .collect();
    assert!(!labels.contains("name"));
    assert!(labels.contains("AuthService"));
}

#[test]
fn test_god_nodes_filter_is_case_insensitive() {
    let mut nodes = vec![node("real", "RealAbstraction", "libs/real.py", "code")];
    let mut links = Vec::new();
    for index in 0..3 {
        let id = format!("peer{index}");
        nodes.push(node(
            &id,
            &format!("P{index}"),
            &format!("src/p{index}.py"),
            "code",
        ));
        links.push(edge("real", &id, "", Confidence::Extracted));
    }
    for (variant_index, variant) in ["Start", "START", "Name", "ID"].iter().enumerate() {
        let id = format!("json_{variant_index}");
        nodes.push(node(&id, variant, "testhelpers/data.json", "code"));
        for index in 0..15 {
            let target = format!("{id}_t{index}");
            nodes.push(node(
                &target,
                &format!("X{index}"),
                "testhelpers/data.json",
                "code",
            ));
            links.push(edge(&target, &id, "", Confidence::Extracted));
        }
    }
    let labels: BTreeSet<_> = god_nodes(&graph(nodes, links, false), 10)
        .into_iter()
        .map(|node| node.label)
        .collect();
    for variant in ["Start", "START", "Name", "ID"] {
        assert!(!labels.contains(variant));
    }
}

#[test]
fn test_suggest_questions_excludes_rationale_nodes_from_isolated_count() {
    let graph = graph(
        vec![
            node("service", "Service", "service.py", "code"),
            node("reason", "Explains service", "service.py", "rationale"),
        ],
        Vec::new(),
        false,
    );
    let questions = suggest_questions(&graph, &BTreeMap::new(), &BTreeMap::new(), 10);
    let isolated = questions
        .iter()
        .find(|question| question.kind == "isolated_nodes")
        .unwrap();
    assert!(isolated.why.starts_with("1 weakly-connected node"));
    let question = isolated.question.as_deref().unwrap();
    assert!(question.contains("`Service`"));
    assert!(!question.contains("Explains service"));
}

fn cycle_graph(directed: bool) -> KnowledgeGraph {
    let nodes = vec![
        node("a", "a.ts", "src/a.ts", "code"),
        node("b", "b.ts", "src/b.ts", "code"),
        node("c", "c.ts", "src/c.ts", "code"),
        node("d", "d.ts", "src/d.ts", "code"),
        node("react", "react", "", "code"),
    ];
    let mut links = vec![
        edge("a", "b", "imports_from", Confidence::Extracted),
        edge("b", "a", "imports_from", Confidence::Extracted),
        edge("b", "c", "imports_from", Confidence::Extracted),
        edge("c", "d", "imports_from", Confidence::Extracted),
        edge("d", "b", "imports_from", Confidence::Extracted),
        edge("c", "c", "imports_from", Confidence::Extracted),
        edge("a", "react", "calls", Confidence::Inferred),
        edge("a", "react", "contains", Confidence::Extracted),
        edge("a", "react", "imports_from", Confidence::Extracted),
    ];
    for link in &mut links {
        link.source_file = nodes
            .iter()
            .find(|node| node.id == link.source)
            .map(|node| node.source_file.clone())
            .unwrap_or_default();
    }
    graph(nodes, links, directed)
}

#[test]
fn test_find_import_cycles_returns_structured_records() {
    let cycles = find_import_cycles(&cycle_graph(true), 5, 20);
    assert!(!cycles.is_empty());
    assert!(!cycles[0].cycle.is_empty());
    assert_eq!(cycles[0].length, cycles[0].cycle.len());
    assert_eq!(cycles[0].why, "circular dependency");
}

#[test]
fn test_find_import_cycles_detects_2_and_3_cycles() {
    let cycles = find_import_cycles(&cycle_graph(true), 5, 20);
    let sets: Vec<BTreeSet<_>> = cycles
        .iter()
        .map(|cycle| cycle.cycle.iter().map(String::as_str).collect())
        .collect();
    assert!(sets.iter().any(|cycle| ["src/a.ts", "src/b.ts"]
        .into_iter()
        .all(|item| cycle.contains(item))));
    assert!(sets.iter().any(|cycle| {
        ["src/b.ts", "src/c.ts", "src/d.ts"]
            .into_iter()
            .all(|item| cycle.contains(item))
    }));
}

#[test]
fn test_find_import_cycles_includes_self_loop_cycle() {
    assert!(find_import_cycles(&cycle_graph(true), 5, 20)
        .iter()
        .any(|cycle| cycle.cycle == ["src/c.ts"] && cycle.length == 1));
}

#[test]
fn test_find_import_cycles_respects_max_cycle_length() {
    assert!(find_import_cycles(&cycle_graph(true), 2, 20)
        .iter()
        .all(|cycle| cycle.length <= 2));
}

#[test]
fn test_find_import_cycles_skips_nodes_without_source_file() {
    assert!(find_import_cycles(&cycle_graph(true), 5, 20)
        .iter()
        .flat_map(|cycle| &cycle.cycle)
        .all(|path| !path.contains("react")));
}

#[test]
fn test_find_import_cycles_handles_undirected_graph_input() {
    assert!(!find_import_cycles(&cycle_graph(false), 5, 20).is_empty());
}

#[test]
fn test_find_import_cycles_ignores_non_import_relations() {
    let graph = graph(
        vec![
            node("a", "a.ts", "src/a.ts", "code"),
            node("b", "b.ts", "src/b.ts", "code"),
        ],
        vec![
            edge("a", "b", "calls", Confidence::Inferred),
            edge("b", "a", "contains", Confidence::Extracted),
        ],
        true,
    );
    assert!(find_import_cycles(&graph, 5, 20).is_empty());
}

#[test]
fn test_find_import_cycles_empty_graph() {
    assert!(find_import_cycles(&graph(Vec::new(), Vec::new(), true), 5, 20).is_empty());
}

#[test]
fn test_find_import_cycles_no_cycles() {
    let nodes = vec![
        node("x", "x.ts", "x.ts", "code"),
        node("y", "y.ts", "y.ts", "code"),
    ];
    let mut import = edge("x", "y", "imports_from", Confidence::Extracted);
    import.source_file = "x.ts".into();
    assert!(find_import_cycles(&graph(nodes, vec![import], true), 5, 20).is_empty());
}
