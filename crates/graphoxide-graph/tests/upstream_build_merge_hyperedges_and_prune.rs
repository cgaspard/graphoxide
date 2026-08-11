//! Executable port of upstream `test_build_merge_hyperedges_and_prune.py` plus
//! Graphoxide provenance regressions.

use graphoxide_core::Extraction;
use graphoxide_graph::{build_merge, infer_merge_root};
use serde_json::{json, Value};
use std::{collections::BTreeSet, fs, path::Path};
use tempfile::tempdir;

fn write_graph(path: &Path, nodes: Value, edges: Value, hyperedges: Value) {
    fs::write(
        path,
        serde_json::to_vec(&json!({
            "nodes": nodes, "edges": edges, "hyperedges": hyperedges
        }))
        .unwrap(),
    )
    .unwrap();
}

fn extraction(value: Value) -> Extraction {
    serde_json::from_value(value).unwrap()
}

fn hyperedge_ids(graph: &graphoxide_core::KnowledgeGraph) -> BTreeSet<&str> {
    graph
        .hyperedges
        .iter()
        .filter_map(|value| value["id"].as_str())
        .collect()
}

fn seed_two_file_graph(base: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let root = base.join("corpus");
    fs::create_dir(&root).unwrap();
    let graph_path = base.join("graph.json");
    write_graph(
        &graph_path,
        json!([
            {"id": "a1", "label": "a1", "file_type": "document", "source_file": "a.md"},
            {"id": "b1", "label": "b1", "file_type": "document", "source_file": "b.md"}
        ]),
        json!([]),
        json!([
            {"id": "he_a", "label": "flow A", "source_file": "a.md", "nodes": ["a1"]},
            {"id": "he_b", "label": "flow B", "source_file": "b.md", "nodes": ["b1"]},
            {"id": "he_global", "label": "cross-file flow", "nodes": ["a1", "b1"]}
        ]),
    );
    (root, graph_path)
}

#[test]
fn update_preserves_hyperedges_of_unchanged_files() {
    let tmp = tempdir().unwrap();
    let (root, graph_path) = seed_two_file_graph(tmp.path());
    let new_chunk = extraction(json!({
        "nodes": [{"id": "b1", "label": "b1", "file_type": "document", "source_file": "b.md"}],
        "edges": [],
        "hyperedges": [{"id": "he_b_v2", "label": "flow B v2", "source_file": "b.md", "nodes": ["b1"]}]
    }));
    let graph = build_merge(&[new_chunk], graph_path, &[], Some(&root)).unwrap();
    let ids = hyperedge_ids(&graph);
    assert!(ids.contains("he_a"));
    assert!(ids.contains("he_global"));
    assert!(ids.contains("he_b_v2"));
    assert!(!ids.contains("he_b"));
}

#[test]
fn update_without_root_still_preserves_hyperedges() {
    let tmp = tempdir().unwrap();
    let (_, graph_path) = seed_two_file_graph(tmp.path());
    let new_chunk = extraction(json!({
        "nodes": [{"id": "b1", "label": "b1", "file_type": "document", "source_file": "b.md"}],
        "edges": [], "hyperedges": [{"id": "he_b_v2", "source_file": "b.md", "nodes": ["b1"]}]
    }));
    let graph = build_merge(&[new_chunk], graph_path, &[], None).unwrap();
    let ids = hyperedge_ids(&graph);
    assert!(ids.is_superset(&BTreeSet::from(["he_a", "he_global", "he_b_v2"])));
    assert!(!ids.contains("he_b"));
}

#[test]
fn deleted_file_hyperedges_are_pruned() {
    let tmp = tempdir().unwrap();
    let (root, graph_path) = seed_two_file_graph(tmp.path());
    let graph = build_merge(&[], graph_path, &[root.join("a.md")], Some(&root)).unwrap();
    let ids = hyperedge_ids(&graph);
    assert!(!ids.contains("he_a"));
    assert!(ids.contains("he_b"));
    assert!(ids.contains("he_global"));
    assert_eq!(
        graph
            .hyperedges
            .iter()
            .find(|hyperedge| hyperedge["id"] == "he_global")
            .unwrap()["nodes"],
        json!(["b1"])
    );
    assert!(!graph.nodes.iter().any(|node| node.id == "a1"));
}

#[test]
fn ownership_prune_repairs_only_endpoints_that_lost_a_real_node() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().join("corpus");
    fs::create_dir(&root).unwrap();
    let graph_path = tmp.path().join("graph.json");
    write_graph(
        &graph_path,
        json!([
            {"id":"main","label":"main","file_type":"code","source_file":"main.ts"},
            {"id":"segment_symbol","label":"phantom","file_type":"code","source_file":"segment.ts"},
            {"id":"keep","label":"keep","file_type":"code","source_file":"keep.ts"}
        ]),
        json!([
            {"source":"main","target":"segment_symbol","relation":"semantic_dependency","confidence":"INFERRED","source_file":"main.ts","_origin":"semantic"},
            {"source":"main","target":"ref_segment","relation":"imports_from","confidence":"EXTRACTED","source_file":"main.ts"},
            {"source":"main","target":"external_missing","relation":"mentions","confidence":"INFERRED","source_file":"main.ts","_origin":"semantic"}
        ]),
        json!([
            {"id":"mixed","source_file":"main.ts","nodes":["main","segment_symbol","keep"]},
            {"id":"unary","source_file":"main.ts","nodes":["main","segment_symbol"]},
            {"id":"empty_after_prune","source_file":"main.ts","nodes":["segment_symbol"]},
            {"id":"unrelated_missing","source_file":"main.ts","nodes":["main","external_missing","keep"]}
        ]),
    );
    let prune = [root.join("segment.ts")];

    let raw = graphoxide_graph::incremental::merge_raw_extraction(
        &Extraction::default(),
        &graph_path,
        &prune,
        Some(&root),
    )
    .unwrap();
    assert!(raw.nodes.iter().all(|node| node.id != "segment_symbol"));
    assert!(raw
        .edges
        .iter()
        .all(|edge| edge.true_target() != "segment_symbol"));
    assert!(raw
        .edges
        .iter()
        .any(|edge| edge.true_target() == "ref_segment"));
    assert!(raw
        .edges
        .iter()
        .any(|edge| edge.true_target() == "external_missing"));
    let mixed = raw
        .hyperedges
        .iter()
        .find(|hyperedge| hyperedge["id"] == "mixed")
        .unwrap();
    assert_eq!(mixed["nodes"], json!(["main", "keep"]));
    assert_eq!(
        raw.hyperedges
            .iter()
            .find(|hyperedge| hyperedge["id"] == "unary")
            .unwrap()["nodes"],
        json!(["main"])
    );
    assert!(raw
        .hyperedges
        .iter()
        .all(|hyperedge| hyperedge["id"] != "empty_after_prune"));
    assert!(raw
        .hyperedges
        .iter()
        .any(|hyperedge| hyperedge["id"] == "unrelated_missing"));

    let clustered = build_merge(&[], &graph_path, &prune, Some(&root)).unwrap();
    assert!(clustered
        .links
        .iter()
        .all(|edge| edge.true_target() != "segment_symbol"));
    let mixed = clustered
        .hyperedges
        .iter()
        .find(|hyperedge| hyperedge["id"] == "mixed")
        .unwrap();
    assert_eq!(mixed["nodes"], json!(["main", "keep"]));
}

#[test]
fn ownership_prune_keeps_endpoints_when_a_fresh_node_reuses_the_id() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().join("corpus");
    fs::create_dir(&root).unwrap();
    let graph_path = tmp.path().join("graph.json");
    write_graph(
        &graph_path,
        json!([
            {"id":"main","label":"main","file_type":"code","source_file":"main.ts"},
            {"id":"shared","label":"old","file_type":"code","source_file":"segment.ts"}
        ]),
        json!([
            {"source":"main","target":"shared","relation":"semantic_dependency","confidence":"INFERRED","source_file":"main.ts","_origin":"semantic"}
        ]),
        json!([{"id":"shared_flow","source_file":"main.ts","nodes":["main","shared"]}]),
    );
    let fresh = extraction(json!({
        "nodes":[{"id":"shared","label":"replacement","file_type":"concept","source_file":"replacement.md","_origin":"semantic"}],
        "edges":[],
        "hyperedges":[]
    }));
    let prune = [root.join("segment.ts")];

    let raw = graphoxide_graph::incremental::merge_raw_extraction(
        &fresh,
        &graph_path,
        &prune,
        Some(&root),
    )
    .unwrap();
    assert!(raw
        .nodes
        .iter()
        .any(|node| node.id == "shared" && node.source_file == "replacement.md"));
    assert!(raw.edges.iter().any(|edge| edge.true_target() == "shared"));
    assert!(raw
        .hyperedges
        .iter()
        .any(|hyperedge| hyperedge["id"] == "shared_flow"));

    let clustered = build_merge(&[fresh], &graph_path, &prune, Some(&root)).unwrap();
    assert!(clustered
        .nodes
        .iter()
        .any(|node| node.id == "shared" && node.source_file == "replacement.md"));
    assert!(clustered
        .links
        .iter()
        .any(|edge| edge.true_target() == "shared"));
    assert!(clustered
        .hyperedges
        .iter()
        .any(|hyperedge| hyperedge["id"] == "shared_flow"));
}

#[test]
fn authoritative_ownership_reset_replaces_both_tiers_but_keeps_fresh_facts() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().join("corpus");
    fs::create_dir(&root).unwrap();
    let graph_path = tmp.path().join("graph.json");
    write_graph(
        &graph_path,
        json!([
            {"id":"main","label":"main","file_type":"code","source_file":"main.ts"},
            {"id":"old_ast","label":"old structural","file_type":"code","source_file":"segment.ts"},
            {"id":"old_semantic","label":"old semantic","file_type":"concept","source_file":"segment.ts","_origin":"semantic"},
            {"id":"keep","label":"keep","file_type":"code","source_file":"keep.ts"}
        ]),
        json!([
            {"source":"old_semantic","target":"keep","relation":"semantic_dependency","confidence":"INFERRED","source_file":"segment.ts","_origin":"semantic"},
            {"source":"main","target":"old_semantic","relation":"semantic_dependency","confidence":"INFERRED","source_file":"main.ts","_origin":"semantic"},
            {"source":"main","target":"ref_segment","relation":"imports_from","confidence":"EXTRACTED","source_file":"main.ts"}
        ]),
        json!([
            {"id":"owned_semantic","source_file":"segment.ts","_origin":"semantic","nodes":["old_semantic","keep"]},
            {"id":"foreign_multi","source_file":"main.ts","_origin":"semantic","nodes":["main","old_semantic","keep"]},
            {"id":"foreign_unary","source_file":"main.ts","_origin":"semantic","nodes":["main","old_semantic"]},
            {"id":"foreign_empty","source_file":"main.ts","_origin":"semantic","nodes":["old_semantic"]}
        ]),
    );
    let existing = graphoxide_core::read_graph(&graph_path).unwrap();
    let fresh = extraction(json!({
        "nodes":[{"id":"format_inventory_mpeg_transport_stream_segment","label":"segment.ts","file_type":"document","source_file":"segment.ts","type":"format_inventory"}],
        "edges":[{"source":"format_inventory_mpeg_transport_stream_segment","target":"ref_media","relation":"references","confidence":"EXTRACTED","source_file":"segment.ts"}],
        "hyperedges":[{"id":"fresh_media_flow","source_file":"segment.ts","nodes":["format_inventory_mpeg_transport_stream_segment"]}]
    }));
    let rebuilt = [root.join("segment.ts")];
    let ownership_resets = [root.join("segment.ts")];
    let merged = graphoxide_graph::incremental::merge_raw_extraction_from_graph_with_rebuilt_sources_and_ownership_resets_and_materialization_limit(
        fresh,
        &existing,
        &rebuilt,
        &[],
        graphoxide_graph::incremental::IncrementalBaselinePrunes {
            deletion_sources: &[],
            ownership_reset_sources: &ownership_resets,
        },
        Some(&root),
        usize::MAX,
    )
    .unwrap();

    assert!(merged
        .nodes
        .iter()
        .all(|node| node.id != "old_ast" && node.id != "old_semantic"));
    assert!(merged.nodes.iter().any(|node| {
        node.id == "format_inventory_mpeg_transport_stream_segment"
            && node.source_file == "segment.ts"
    }));
    assert!(merged
        .edges
        .iter()
        .all(|edge| edge.true_source() != "old_semantic" && edge.true_target() != "old_semantic"));
    assert!(merged
        .edges
        .iter()
        .any(|edge| edge.true_target() == "ref_segment"));
    assert!(merged.edges.iter().any(|edge| {
        edge.true_source() == "format_inventory_mpeg_transport_stream_segment"
            && edge.true_target() == "ref_media"
    }));
    assert!(
        merged
            .hyperedges
            .iter()
            .all(|hyperedge| hyperedge["id"] != "owned_semantic"
                && hyperedge["id"] != "foreign_empty")
    );
    assert_eq!(
        merged
            .hyperedges
            .iter()
            .find(|hyperedge| hyperedge["id"] == "foreign_multi")
            .unwrap()["nodes"],
        json!(["main", "keep"])
    );
    assert_eq!(
        merged
            .hyperedges
            .iter()
            .find(|hyperedge| hyperedge["id"] == "foreign_unary")
            .unwrap()["nodes"],
        json!(["main"])
    );
    assert!(merged
        .hyperedges
        .iter()
        .any(|hyperedge| hyperedge["id"] == "fresh_media_flow"));
}

#[test]
fn deleted_container_prunes_owned_sourceless_hyperedge_but_keeps_global() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().join("corpus");
    fs::create_dir(&root).unwrap();
    let graph_path = tmp.path().join("graph.json");
    write_graph(
        &graph_path,
        json!([
            {"id": "archive", "label": "archive", "file_type": "document", "source_file": "archives/x.tar", "type": "container"},
            {"id": "keep", "label": "keep", "file_type": "document", "source_file": "keep.md"}
        ]),
        json!([]),
        json!([
            {"id": "owned_sourceless", "nodes": ["keep"], "_container_source": "archives/x.tar"},
            {"id": "global_sourceless", "nodes": ["keep"]}
        ]),
    );

    let graph = build_merge(&[], graph_path, &[root.join("archives/x.tar")], Some(&root)).unwrap();
    let ids = hyperedge_ids(&graph);
    assert!(!ids.contains("owned_sourceless"));
    assert!(ids.contains("global_sourceless"));
    assert!(graph.nodes.iter().any(|node| node.id == "keep"));
}

#[test]
fn prune_without_root_removes_ghost_nodes_via_grandparent_fallback() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().join("corpus");
    let output = root.join("graphoxide-out");
    fs::create_dir_all(&output).unwrap();
    let graph_path = output.join("graph.json");
    write_graph(
        &graph_path,
        json!([
            {"id": "h1", "label": "handoff", "file_type": "document", "source_file": "HANDOFF.md"},
            {"id": "k1", "label": "keep", "file_type": "document", "source_file": "KEEP.md"}
        ]),
        json!([]),
        json!([]),
    );
    let graph = build_merge(&[], graph_path, &[root.join("HANDOFF.md")], None).unwrap();
    let labels: BTreeSet<_> = graph.nodes.iter().map(|node| node.label.as_str()).collect();
    assert!(!labels.contains("handoff"));
    assert!(labels.contains("keep"));
}

#[test]
fn prune_without_root_uses_graphoxide_root_marker() {
    let tmp = tempdir().unwrap();
    let output = tmp.path().join("out");
    fs::create_dir(&output).unwrap();
    let graph_path = output.join("graph.json");
    let real_root = tmp.path().join("elsewhere/repo");
    fs::create_dir_all(&real_root).unwrap();
    fs::write(
        output.join(".graphoxide_root"),
        real_root.to_string_lossy().as_bytes(),
    )
    .unwrap();
    write_graph(
        &graph_path,
        json!([{"id": "h1", "label": "handoff", "file_type": "document", "source_file": "HANDOFF.md"}]),
        json!([]),
        json!([]),
    );
    assert_eq!(
        infer_merge_root(&graph_path).unwrap(),
        real_root.canonicalize().unwrap()
    );
    let graph = build_merge(&[], graph_path, &[real_root.join("HANDOFF.md")], None).unwrap();
    assert!(!graph.nodes.iter().any(|node| node.label == "handoff"));
}

#[cfg(unix)]
#[test]
fn prune_matches_across_symlinked_root() {
    use std::os::unix::fs::symlink;
    let tmp = tempdir().unwrap();
    let real = tmp.path().join("real");
    let output = real.join("graphoxide-out");
    fs::create_dir_all(&output).unwrap();
    let link = tmp.path().join("link");
    symlink(&real, &link).unwrap();
    let graph_path = output.join("graph.json");
    write_graph(
        &graph_path,
        json!([
            {"id": "h1", "label": "handoff", "file_type": "document", "source_file": "HANDOFF.md"},
            {"id": "k1", "label": "keep", "file_type": "document", "source_file": "KEEP.md"}
        ]),
        json!([]),
        json!([]),
    );
    let graph = build_merge(&[], graph_path, &[link.join("HANDOFF.md")], Some(&real)).unwrap();
    let labels: BTreeSet<_> = graph.nodes.iter().map(|node| node.label.as_str()).collect();
    assert!(!labels.contains("handoff"));
    assert!(labels.contains("keep"));
}

#[cfg(not(unix))]
#[test]
fn prune_matches_across_symlinked_root() {}

#[test]
fn reextracted_file_in_prune_sources_is_not_deleted() {
    let tmp = tempdir().unwrap();
    let output = tmp.path().join("graphoxide-out");
    fs::create_dir(&output).unwrap();
    let graph_path = output.join("graph.json");
    write_graph(
        &graph_path,
        json!([
            {"id": "foo_widget_cache", "label": "Widget Cache Design", "file_type": "concept", "source_file": "docs/foo.md", "source_location": "L1"},
            {"id": "bar_other", "label": "Other", "file_type": "concept", "source_file": "docs/bar.md", "source_location": "L1"}
        ]),
        json!([]),
        json!([]),
    );
    let new_chunk = extraction(json!({"nodes": [{
        "id": "foo_widget_cache", "label": "Widget Cache Design", "file_type": "concept",
        "source_file": "docs/foo.md", "source_location": "L2"
    }], "edges": []}));
    let graph = build_merge(
        &[new_chunk],
        graph_path,
        &["docs/foo.md".into()],
        Some(tmp.path()),
    )
    .unwrap();
    assert!(graph
        .nodes
        .iter()
        .any(|node| node.label == "Widget Cache Design"));
}

#[test]
fn genuine_deletion_still_prunes() {
    let tmp = tempdir().unwrap();
    let output = tmp.path().join("graphoxide-out");
    fs::create_dir(&output).unwrap();
    let graph_path = output.join("graph.json");
    write_graph(
        &graph_path,
        json!([
            {"id": "foo_widget_cache", "label": "Widget Cache Design", "file_type": "concept", "source_file": "docs/foo.md"},
            {"id": "bar_other", "label": "Other", "file_type": "concept", "source_file": "docs/bar.md"}
        ]),
        json!([]),
        json!([]),
    );
    let new_chunk = extraction(
        json!({"nodes": [{"id": "foo_widget_cache", "label": "Widget Cache Design", "file_type": "concept", "source_file": "docs/foo.md"}], "edges": []}),
    );
    let graph = build_merge(
        &[new_chunk],
        graph_path,
        &["docs/bar.md".into()],
        Some(tmp.path()),
    )
    .unwrap();
    assert!(!graph.nodes.iter().any(|node| node.label == "Other"));
    assert!(graph
        .nodes
        .iter()
        .any(|node| node.label == "Widget Cache Design"));
}

#[test]
fn prune_matches_node_stored_absolute_against_relative_delete() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().join("corpus");
    let output = root.join("graphoxide-out");
    fs::create_dir_all(&output).unwrap();
    let graph_path = output.join("graph.json");
    write_graph(
        &graph_path,
        json!([
            {"id": "g1", "label": "gone", "file_type": "code", "source_file": root.join("gone.py")},
            {"id": "k1", "label": "keep", "file_type": "code", "source_file": "keep.py"}
        ]),
        json!([{"source": "g1", "target": "k1", "type": "calls", "source_file": root.join("gone.py")}]),
        json!([]),
    );
    let graph = build_merge(&[], graph_path, &["gone.py".into()], None).unwrap();
    assert!(!graph.nodes.iter().any(|node| node.label == "gone"));
    assert!(graph.nodes.iter().any(|node| node.label == "keep"));
    assert!(graph.links.is_empty());
}

#[test]
fn prune_reextracted_absolute_node_not_deleted() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().join("corpus");
    let output = root.join("graphoxide-out");
    fs::create_dir_all(&output).unwrap();
    let graph_path = output.join("graph.json");
    write_graph(
        &graph_path,
        json!([{"id": "g1", "label": "gone", "file_type": "code", "source_file": root.join("mod.py")}]),
        json!([]),
        json!([]),
    );
    let new_chunk = extraction(
        json!({"nodes": [{"id": "g1", "label": "gone", "file_type": "code", "source_file": "mod.py"}], "edges": []}),
    );
    let graph = build_merge(&[new_chunk], graph_path, &["mod.py".into()], None).unwrap();
    assert!(graph.nodes.iter().any(|node| node.label == "gone"));
}
