use graphoxide_core::{write_graph_atomic, Confidence, Edge, KnowledgeGraph, Node};
use graphoxide_graph::{
    deduplicate_entities, merge_repository_graphs, prefix_graph_for_global, prune_repo_from_graph,
    GlobalGraphStore,
};
use std::collections::BTreeMap;
use tempfile::tempdir;

fn node(id: &str, label: &str, source_file: &str) -> Node {
    Node {
        id: id.into(),
        label: label.into(),
        file_type: "code".into(),
        source_file: source_file.into(),
        source_location: None,
        community: None,
        extra: BTreeMap::new(),
    }
}

fn edge(source: &str, target: &str) -> Edge {
    Edge {
        source: source.into(),
        target: target.into(),
        relation: "imports".into(),
        confidence: Confidence::Extracted,
        source_file: String::new(),
        extra: BTreeMap::new(),
    }
}

fn graph(nodes: Vec<Node>, links: Vec<Edge>) -> KnowledgeGraph {
    KnowledgeGraph {
        nodes,
        links,
        ..KnowledgeGraph::default()
    }
}

fn write(path: &std::path::Path, graph: &KnowledgeGraph) {
    write_graph_atomic(path, graph, true).unwrap();
}

#[test]
fn test_prefix_graph_preserves_label() {
    let prefixed = prefix_graph_for_global(
        &graph(
            vec![node("userservice", "UserService", "src/user.py")],
            vec![],
        ),
        "repoA",
    );
    assert!(prefixed
        .nodes
        .iter()
        .any(|node| node.id == "repoA::userservice"));
    assert!(!prefixed.nodes.iter().any(|node| node.id == "userservice"));
    assert_eq!(prefixed.nodes[0].label, "UserService");
}

#[test]
fn test_prefix_graph_sets_repo_and_local_id() {
    let prefixed = prefix_graph_for_global(
        &graph(vec![node("userservice", "UserService", "")], vec![]),
        "repoA",
    );
    assert_eq!(prefixed.nodes[0].extra["repo"], "repoA");
    assert_eq!(prefixed.nodes[0].extra["local_id"], "userservice");
}

#[test]
fn test_prefix_graph_rewrites_edges() {
    let prefixed = prefix_graph_for_global(
        &graph(
            vec![node("a", "A", "a.py"), node("b", "B", "b.py")],
            vec![edge("a", "b")],
        ),
        "repo1",
    );
    assert_eq!(prefixed.links[0].source, "repo1::a");
    assert_eq!(prefixed.links[0].target, "repo1::b");
}

#[test]
fn test_prefix_graph_rewrites_edge_directional_attributes() {
    let mut import = edge("rota", "collections");
    import.relation = "imports_from".into();
    import.extra.insert("_src".into(), "rota".into());
    import.extra.insert("_tgt".into(), "collections".into());
    let prefixed = prefix_graph_for_global(
        &graph(
            vec![
                node("rota", "rota.js", "rota.js"),
                node("collections", "collections.js", "collections.js"),
            ],
            vec![import],
        ),
        "repoA",
    );
    assert_eq!(prefixed.links[0].extra["_src"], "repoA::rota");
    assert_eq!(prefixed.links[0].extra["_tgt"], "repoA::collections");
}

#[test]
fn test_prune_repo_removes_correct_nodes() {
    let mut nodes = vec![
        node("repoA::userservice", "UserService", ""),
        node("repoB::userservice", "UserService", ""),
        node("repoA::auth", "Auth", ""),
    ];
    nodes[0].extra.insert("repo".into(), "repoA".into());
    nodes[1].extra.insert("repo".into(), "repoB".into());
    nodes[2].extra.insert("repo".into(), "repoA".into());
    let mut graph = graph(nodes, vec![]);
    assert_eq!(prune_repo_from_graph(&mut graph, "repoA"), 2);
    assert_eq!(graph.nodes.len(), 1);
    assert_eq!(graph.nodes[0].id, "repoB::userservice");
}

#[test]
fn test_prune_repo_returns_zero_if_not_present() {
    let mut item = node("repoA::x", "x", "");
    item.extra.insert("repo".into(), "repoA".into());
    let mut graph = graph(vec![item], vec![]);
    assert_eq!(prune_repo_from_graph(&mut graph, "repoB"), 0);
    assert_eq!(graph.nodes.len(), 1);
}

#[test]
fn test_global_add_creates_global_graph() {
    let temporary = tempdir().unwrap();
    let source = temporary.path().join("graph.json");
    write(
        &source,
        &graph(
            vec![node("userservice", "UserService", "src/user.py")],
            vec![],
        ),
    );
    let store = GlobalGraphStore::new(temporary.path().join(".graphoxide"));
    let result = store.add(&source, "repoA").unwrap();
    assert!(!result.skipped);
    assert!(result.nodes_added > 0);
    assert!(store.manifest_path().is_file());
    assert!(store.list().unwrap().contains_key("repoA"));
}

#[test]
fn test_global_add_skip_on_unchanged_hash() {
    let temporary = tempdir().unwrap();
    let source = temporary.path().join("graph.json");
    write(&source, &graph(vec![node("x", "X", "x.py")], vec![]));
    let store = GlobalGraphStore::new(temporary.path().join("global"));
    store.add(&source, "repoA").unwrap();
    assert!(store.add(&source, "repoA").unwrap().skipped);
}

#[test]
fn test_global_add_two_repos_no_collision() {
    let temporary = tempdir().unwrap();
    let first = temporary.path().join("graph1.json");
    let second = temporary.path().join("graph2.json");
    let same = graph(
        vec![node("userservice", "UserService", "src/user.py")],
        vec![],
    );
    write(&first, &same);
    write(&second, &same);
    let store = GlobalGraphStore::new(temporary.path().join("global"));
    store.add(&first, "repoA").unwrap();
    store.add(&second, "repoB").unwrap();
    let global = store.load_graph().unwrap();
    assert!(global
        .nodes
        .iter()
        .any(|node| node.id == "repoA::userservice"));
    assert!(global
        .nodes
        .iter()
        .any(|node| node.id == "repoB::userservice"));
    assert_eq!(global.nodes.len(), 2);
}

#[test]
fn test_global_remove() {
    let temporary = tempdir().unwrap();
    let source = temporary.path().join("graph.json");
    write(&source, &graph(vec![node("x", "X", "x.py")], vec![]));
    let store = GlobalGraphStore::new(temporary.path().join("global"));
    store.add(&source, "repoA").unwrap();
    assert!(store.remove("repoA").unwrap() > 0);
    assert!(!store.list().unwrap().contains_key("repoA"));
}

#[test]
fn test_global_remove_unknown_tag_raises() {
    let temporary = tempdir().unwrap();
    let store = GlobalGraphStore::new(temporary.path().join("global"));
    assert!(store
        .remove("nonexistent")
        .unwrap_err()
        .to_string()
        .contains("not in global graph"));
}

#[test]
fn test_global_add_collision_warning() {
    let temporary = tempdir().unwrap();
    let first = temporary.path().join("graph1.json");
    let second = temporary.path().join("graph2.json");
    let same = graph(vec![node("x", "X", "x.py")], vec![]);
    write(&first, &same);
    write(&second, &same);
    let store = GlobalGraphStore::new(temporary.path().join("global"));
    store.add(&first, "myrepo").unwrap();
    let result = store.add(&second, "myrepo").unwrap();
    assert!(result.warning.unwrap().to_lowercase().contains("repo tag"));
}

#[test]
fn test_dedup_raises_on_cross_repo_nodes() {
    let mut first = node("repoA::userservice", "UserService", "");
    let mut second = node("repoB::userservice", "UserService", "");
    first.extra.insert("repo".into(), "repoA".into());
    second.extra.insert("repo".into(), "repoB".into());
    let error = deduplicate_entities(&[first, second], &[], &BTreeMap::new()).unwrap_err();
    assert!(error.to_string().contains("multiple repos"));
}

#[test]
fn test_dedup_ok_with_single_repo() {
    let mut first = node("repoA::userservice", "UserService", "");
    let mut second = node("repoA::auth", "Auth", "");
    first.extra.insert("repo".into(), "repoA".into());
    second.extra.insert("repo".into(), "repoA".into());
    let (nodes, edges, _) = deduplicate_entities(&[first, second], &[], &BTreeMap::new()).unwrap();
    assert_eq!(nodes.len(), 2);
    assert!(edges.is_empty());
}

#[test]
fn test_dedup_ok_with_no_repo_attr() {
    let (nodes, edges, _) = deduplicate_entities(
        &[
            node("userservice", "UserService", ""),
            node("auth", "Auth", ""),
        ],
        &[],
        &BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(nodes.len(), 2);
    assert!(edges.is_empty());
}

#[test]
fn test_merge_graphs_prefixes_ids() {
    let same = || {
        graph(
            vec![node("userservice", "UserService", "src/user.py")],
            vec![],
        )
    };
    let merged = merge_repository_graphs(vec![
        ("repo1/graphoxide-out/graph.json".into(), same()),
        ("repo2/graphoxide-out/graph.json".into(), same()),
    ]);
    assert!(merged
        .nodes
        .iter()
        .any(|node| node.id == "repo1::userservice"));
    assert!(merged
        .nodes
        .iter()
        .any(|node| node.id == "repo2::userservice"));
    assert_eq!(merged.nodes.len(), 2);
}

#[test]
fn test_global_add_rewires_edges_to_deduplicated_externals() {
    let temporary = tempdir().unwrap();
    let first = temporary.path().join("graph1.json");
    let second = temporary.path().join("graph2.json");
    write(
        &first,
        &graph(
            vec![
                node("moda", "ModA", "src/a.py"),
                node("requests", "requests", ""),
            ],
            vec![edge("moda", "requests")],
        ),
    );
    write(
        &second,
        &graph(
            vec![
                node("modb", "ModB", "src/b.py"),
                node("requests", "requests", ""),
            ],
            vec![edge("modb", "requests")],
        ),
    );
    let store = GlobalGraphStore::new(temporary.path().join("global"));
    store.add(&first, "repoA").unwrap();
    store.add(&second, "repoB").unwrap();
    let graph = store.load_graph().unwrap();
    assert!(graph.nodes.iter().any(|node| node.id == "repoA::requests"));
    assert!(!graph.nodes.iter().any(|node| node.id == "repoB::requests"));
    assert!(graph.links.iter().any(|edge| {
        edge.true_source() == "repoA::moda" && edge.true_target() == "repoA::requests"
    }));
    assert!(graph.links.iter().any(|edge| {
        edge.true_source() == "repoB::modb"
            && edge.true_target() == "repoA::requests"
            && edge.relation == "imports"
    }));
}

#[test]
fn test_global_add_rejects_oversized_source_graph() {
    let temporary = tempdir().unwrap();
    let source = temporary.path().join("graph.json");
    write(&source, &graph(vec![node("x", "X", "src/x.py")], vec![]));
    let store = GlobalGraphStore::with_cap(temporary.path().join("global"), 8);
    assert!(store
        .add(&source, "repoA")
        .unwrap_err()
        .to_string()
        .contains("exceeds"));
}
