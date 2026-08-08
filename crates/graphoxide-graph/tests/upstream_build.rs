//! Executable case-for-case port of Graphify `tests/test_build.py` at the
//! revision pinned by `parity/upstream.lock.json`.

use graphoxide_core::{
    read_graph, write_graph_atomic, Confidence, Extraction, KnowledgeGraph, Node,
};
use graphoxide_graph::{
    build_graph, build_graph_from_value, build_graph_with_options,
    build_graph_with_options_and_root, build_merge, build_merge_with_options,
    canonicalize_extraction, cluster, dedupe_raw_edges, dedupe_raw_nodes, edge_data, edge_datas,
    graph_has_legacy_ids, is_ast_tier_value, merge_raw_extraction, semantic_id_remap, BuildOptions,
    IncrementalOptions,
};
use serde_json::{json, Value};
use std::{collections::BTreeMap, fs, path::PathBuf};
use tempfile::TempDir;

fn build_value(value: Value) -> KnowledgeGraph {
    build_graph_from_value(&value, BuildOptions::default(), None)
        .unwrap()
        .0
}

fn build_value_with(value: Value, options: BuildOptions) -> KnowledgeGraph {
    build_graph_from_value(&value, options, None).unwrap().0
}

fn build_value_at(value: Value, root: &std::path::Path) -> KnowledgeGraph {
    build_graph_from_value(&value, BuildOptions::default(), Some(root))
        .unwrap()
        .0
}

fn extraction(value: Value) -> Extraction {
    canonicalize_extraction(&value).0
}

fn node<'a>(graph: &'a KnowledgeGraph, id: &str) -> &'a Node {
    graph.nodes.iter().find(|node| node.id == id).unwrap()
}

fn has_node(graph: &KnowledgeGraph, id: &str) -> bool {
    graph.nodes.iter().any(|node| node.id == id)
}

fn has_edge(graph: &KnowledgeGraph, source: &str, target: &str) -> bool {
    !edge_datas(graph, source, target).is_empty()
}

fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../../../tests/fixtures/upstream/extraction.json"
    ))
    .unwrap()
}

fn no_dedup() -> BuildOptions {
    BuildOptions {
        deduplicate_semantic_nodes: false,
        ..BuildOptions::default()
    }
}

fn incremental_no_dedup() -> IncrementalOptions {
    IncrementalOptions {
        deduplicate_semantic_nodes: false,
        ..IncrementalOptions::default()
    }
}

#[test]
fn test_dedupe_edges_collapses_exact_parallels() {
    let edges = vec![
        json!({"source":"a","target":"b","relation":"calls","source_location":"L1"}),
        json!({"source":"a","target":"b","relation":"calls","source_location":"L9"}),
        json!({"source":"a","target":"b","relation":"imports"}),
        json!({"source":"b","target":"c","relation":"calls"}),
    ];
    let out = dedupe_raw_edges(&edges);
    let keys: Vec<_> = out
        .iter()
        .map(|edge| {
            (
                edge["source"].as_str().unwrap(),
                edge["target"].as_str().unwrap(),
                edge["relation"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        keys,
        vec![
            ("a", "b", "calls"),
            ("a", "b", "imports"),
            ("b", "c", "calls")
        ]
    );
    assert_eq!(out[0]["source_location"], "L1");
}

#[test]
fn test_dedupe_edges_is_idempotent() {
    let edges = vec![
        json!({"source":"a","target":"b","relation":"calls"}),
        json!({"source":"a","target":"b","relation":"calls"}),
    ];
    let once = dedupe_raw_edges(&edges);
    let twice = dedupe_raw_edges(&once.iter().chain(&edges).cloned().collect::<Vec<_>>());
    assert_eq!(once.len(), 1);
    assert_eq!(twice.len(), 1);
}

#[test]
fn test_dedupe_nodes_collapses_by_id_last_wins() {
    let nodes = vec![
        json!({"id":"foundation","label":"Foundation","type":"module","source_file":"A.swift"}),
        json!({"id":"akit","label":"AKit","file_type":"code"}),
        json!({"id":"foundation","label":"Foundation","type":"module","source_file":"B.swift"}),
    ];
    let out = dedupe_raw_nodes(&nodes);
    assert_eq!(
        out.iter()
            .map(|node| node["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["foundation", "akit"]
    );
    assert_eq!(out[0]["source_file"], "B.swift");
}

#[test]
fn test_build_from_json_node_count() {
    assert_eq!(build_value(fixture()).nodes.len(), 4);
}

#[test]
fn test_build_from_json_edge_count() {
    assert_eq!(build_value(fixture()).links.len(), 4);
}

#[test]
fn test_null_weight_edge_builds_and_clusters() {
    let mut graph = build_value(json!({
        "nodes":[
            {"id":"a","label":"A","file_type":"code","source_file":"a.py"},
            {"id":"b","label":"B","file_type":"code","source_file":"b.py"},
            {"id":"c","label":"C","file_type":"code","source_file":"c.py"}
        ],
        "edges":[
            {"source":"a","target":"b","relation":"references","weight":null,"confidence_score":null},
            {"source":"b","target":"c","relation":"references","weight":2.5}
        ]
    }));
    let ab = edge_data(&graph, "a", "b").unwrap();
    assert_eq!(ab.extra["weight"], 1.0);
    assert_eq!(ab.extra["confidence_score"], 1.0);
    assert_eq!(edge_data(&graph, "b", "c").unwrap().extra["weight"], 2.5);
    cluster(&mut graph).unwrap();
}

#[test]
fn test_malformed_weights_normalize() {
    let graph = build_value(json!({
        "nodes": (0..4).map(|i| json!({"id":format!("n{i}"),"label":i.to_string(),"file_type":"code","source_file":format!("{i}.py")})).collect::<Vec<_>>(),
        "edges":[
            {"source":"n0","target":"n1","relation":"references","weight":"3.5"},
            {"source":"n1","target":"n2","relation":"references","weight":"NaN"},
            {"source":"n2","target":"n3","relation":"references","weight":-4}
        ]
    }));
    assert_eq!(edge_data(&graph, "n0", "n1").unwrap().extra["weight"], 3.5);
    assert_eq!(edge_data(&graph, "n1", "n2").unwrap().extra["weight"], 1.0);
    assert_eq!(edge_data(&graph, "n2", "n3").unwrap().extra["weight"], 1.0);
}

#[test]
fn test_nodes_have_label() {
    assert_eq!(
        node(&build_value(fixture()), "n_transformer").label,
        "Transformer"
    );
}

#[test]
fn test_edges_have_confidence() {
    assert_eq!(
        edge_data(&build_value(fixture()), "n_attention", "n_concept_attn")
            .unwrap()
            .confidence,
        Confidence::Inferred
    );
}

#[test]
fn test_ambiguous_edge_preserved() {
    assert_eq!(
        edge_data(&build_value(fixture()), "n_layernorm", "n_concept_attn")
            .unwrap()
            .confidence,
        Confidence::Ambiguous
    );
}

#[test]
fn test_legacy_node_source_canonicalized() {
    let graph = build_value(
        json!({"nodes":[{"id":"n1","label":"A","file_type":"code","source":"a.py"}],"edges":[]}),
    );
    assert_eq!(node(&graph, "n1").source_file, "a.py");
    assert!(!node(&graph, "n1").extra.contains_key("source"));
}

#[test]
fn test_legacy_edge_from_to_canonicalized() {
    let graph = build_value(json!({
        "nodes":[
            {"id":"n1","label":"A","file_type":"code","source_file":"a.py"},
            {"id":"n2","label":"B","file_type":"code","source_file":"b.py"}
        ],
        "edges":[{"from":"n1","to":"n2","relation":"calls","confidence":"EXTRACTED","source_file":"a.py","weight":1.0}]
    }));
    assert_eq!(graph.links.len(), 1);
}

#[test]
fn test_legacy_node_name_path_aliases_folded() {
    let graph = build_value(
        json!({"nodes":[{"id":"n1","name":"Foo","path":"a/b.md","file_type":"concept"}],"edges":[]}),
    );
    let attrs = node(&graph, "n1");
    assert_eq!(attrs.label, "Foo");
    assert_eq!(attrs.source_file, "a/b.md");
    assert!(!attrs.extra.contains_key("name"));
    assert!(!attrs.extra.contains_key("path"));
}

#[test]
fn test_legacy_edge_type_confidence_score_aliases_folded() {
    let graph = build_value(json!({
        "nodes":[
            {"id":"n1","label":"A","file_type":"code","source_file":"a.py"},
            {"id":"n2","label":"B","file_type":"code","source_file":"b.py"}
        ],
        "edges":[{"source":"n1","target":"n2","type":"references","confidence_score":0.9,"source_file":"a.py"}]
    }));
    let data = edge_data(&graph, "n1", "n2").unwrap();
    assert_eq!(data.relation, "references");
    assert_eq!(data.confidence, Confidence::Inferred);
    assert_eq!(data.extra["confidence_score"], 0.9);
    assert!(!data.extra.contains_key("type"));
}

#[test]
fn test_node_alias_canonical_field_wins() {
    let graph = build_value(
        json!({"nodes":[{"id":"n1","label":"Real","name":"Alias","file_type":"code","source_file":"a.py"}],"edges":[]}),
    );
    let attrs = node(&graph, "n1");
    assert_eq!(attrs.label, "Real");
    assert_eq!(attrs.extra["name"], "Alias");
}

#[test]
fn test_alias_node_ghost_merges_into_ast_twin() {
    let graph = build_value(json!({"nodes":[
        {"id":"src_foo_helper","label":"helper","file_type":"code","source_file":"src/foo.py","_origin":"ast","source_location":"L10"},
        {"id":"helper_ghost","name":"helper","path":"src/foo.py","file_type":"code"}
    ],"edges":[]}));
    assert!(has_node(&graph, "src_foo_helper"));
    assert!(!has_node(&graph, "helper_ghost"));
    assert_eq!(graph.nodes.len(), 1);
}

#[test]
fn test_alias_node_gets_nonempty_norm_label() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("graph.json");
    let graph = build_value(
        json!({"nodes":[{"id":"n1","name":"Foo","path":"a/b.md","file_type":"concept"}],"edges":[]}),
    );
    assert!(write_graph_atomic(&path, &graph, true).unwrap());
    let value: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert_eq!(value["nodes"][0]["norm_label"], "foo");
}

#[test]
fn test_extraction_warning_breakdown_by_cause() {
    let (_, report) = canonicalize_extraction(&json!({
        "nodes":[
            {"id":"n1","label":"A","file_type":"code","source_file":"a.py"},
            {"id":"n2","label":"B","file_type":"code","source_file":"b.py"},
            {"id":"x1","file_type":"code","source_file":"x.py"},
            {"id":"x2","file_type":"code","source_file":"x.py"}
        ],
        "edges":[
            {"source":"n1","target":"n2","confidence":"EXTRACTED","source_file":"a.py"},
            {"source":"n2","target":"n1","confidence":"EXTRACTED","source_file":"a.py"},
            {"source":"n1","target":"x1","confidence":"EXTRACTED","source_file":"a.py"}
        ]
    }));
    assert_eq!(report.issue_count("missing required field 'label'"), 2);
    assert_eq!(report.issue_count("missing required field 'relation'"), 3);
}

#[test]
fn test_absolute_derived_semantic_ids_rekeyed() {
    let temp = TempDir::new().unwrap();
    let abs = temp.path().join("docs/DATAFLOW.md");
    let mut stem = abs.clone();
    stem.set_extension("");
    let abs_stem = graphoxide_core::normalize_id(&stem.to_string_lossy());
    let graph = build_value_at(
        json!({
            "nodes":[
                {"id":abs_stem,"label":"DATAFLOW.md","file_type":"document","source_file":abs},
                {"id":format!("{abs_stem}_pipeline"),"label":"Pipeline","file_type":"concept","source_file":abs}
            ],
            "edges":[{"source":abs_stem,"target":format!("{abs_stem}_pipeline"),"relation":"describes","confidence":"INFERRED","source_file":abs,"weight":1.0}]
        }),
        temp.path(),
    );
    assert!(has_node(&graph, "docs_dataflow"));
    assert!(has_node(&graph, "docs_dataflow_pipeline"));
    assert!(!has_node(&graph, &abs_stem));
    assert_eq!(
        node(&graph, "docs_dataflow").source_file,
        "docs/DATAFLOW.md"
    );
    assert!(has_edge(&graph, "docs_dataflow", "docs_dataflow_pipeline"));
}

#[test]
fn test_absolute_derived_semantic_ids_rekeyed_backslash() {
    let temp = TempDir::new().unwrap();
    let abs = temp.path().join("docs/DATAFLOW.md");
    let mut stem = abs.clone();
    stem.set_extension("");
    let abs_stem = graphoxide_core::normalize_id(&stem.to_string_lossy());
    let backslash = abs.to_string_lossy().replace('/', "\\");
    let graph = build_value_at(
        json!({"nodes":[
        {"id":format!("{abs_stem}_pipeline"),"label":"Pipeline","file_type":"concept","source_file":backslash}
    ],"edges":[]}),
        temp.path(),
    );
    assert!(has_node(&graph, "docs_dataflow_pipeline"));
    assert_eq!(
        node(&graph, "docs_dataflow_pipeline").source_file,
        "docs/DATAFLOW.md"
    );
}

#[test]
fn test_source_file_backslash_normalized() {
    let graph = build_value(json!({"nodes":[
        {"id":"n1","label":"A","file_type":"code","source_file":"src\\middleware\\auth.py"},
        {"id":"n2","label":"B","file_type":"code","source_file":"src/middleware/auth.py"}
    ],"edges":[]}));
    assert!(graph
        .nodes
        .iter()
        .all(|node| node.source_file == "src/middleware/auth.py"));
}

#[test]
fn test_edge_missing_source_file_backfilled_from_node() {
    let graph = build_value(json!({
        "nodes":[
            {"id":"n1","label":"A","file_type":"concept","source_file":"docs/a.md"},
            {"id":"n2","label":"B","file_type":"concept","source_file":"docs/b.md"}
        ],
        "edges":[{"source":"n1","target":"n2","relation":"relates_to","confidence":"INFERRED"}]
    }));
    assert_eq!(
        edge_data(&graph, "n1", "n2").unwrap().source_file,
        "docs/a.md"
    );
}

#[test]
fn test_build_merges_multiple_extractions() {
    let first = extraction(
        json!({"nodes":[{"id":"n1","label":"A","file_type":"code","source_file":"a.py"}],"edges":[]}),
    );
    let second = extraction(json!({
        "nodes":[{"id":"n2","label":"B","file_type":"document","source_file":"b.md"}],
        "edges":[{"source":"n1","target":"n2","relation":"references","confidence":"INFERRED","source_file":"b.md","weight":1.0}]
    }));
    let graph = build_graph(&[first, second]).unwrap();
    assert_eq!(graph.nodes.len(), 2);
    assert_eq!(graph.links.len(), 1);
}

#[test]
fn test_none_file_type_defaults_to_concept() {
    let graph = build_value(json!({"nodes":[
        {"id":"n1","label":"Stub","file_type":null,"source_file":"a.py"},
        {"id":"n2","label":"Real","file_type":"code","source_file":"b.py"}
    ],"edges":[]}));
    assert_eq!(node(&graph, "n1").file_type, "concept");
    assert_eq!(node(&graph, "n2").file_type, "code");
}

#[test]
fn test_missing_file_type_defaults_to_concept() {
    let graph =
        build_value(json!({"nodes":[{"id":"n1","label":"Bare","source_file":"a.py"}],"edges":[]}));
    assert_eq!(node(&graph, "n1").file_type, "concept");
}

#[test]
fn test_real_invalid_file_type_coerced_to_concept() {
    let graph = build_value(
        json!({"nodes":[{"id":"n1","label":"Bad","file_type":"weird_type","source_file":"a.py"}],"edges":[]}),
    );
    assert_eq!(node(&graph, "n1").file_type, "concept");
}

#[test]
fn test_file_type_synonym_mapping() {
    let graph = build_value(json!({"nodes":[
        {"id":"n1","label":"MD","file_type":"markdown","source_file":"a.md"},
        {"id":"n2","label":"Tool","file_type":"tool","source_file":"b.py"},
        {"id":"n3","label":"Pat","file_type":"pattern","source_file":"c.md"}
    ],"edges":[]}));
    assert_eq!(node(&graph, "n1").file_type, "document");
    assert_eq!(node(&graph, "n2").file_type, "code");
    assert_eq!(node(&graph, "n3").file_type, "concept");
}

#[test]
fn test_ghost_merge_unique_located_node_still_merges() {
    let graph = build_value(json!({"nodes":[
        {"id":"ast_render","label":"render","file_type":"code","source_file":"src/app/index.ts","source_location":"L10","_origin":"ast"},
        {"id":"ghost_render","label":"render","file_type":"code","source_file":"src/app/index.ts"},
        {"id":"caller","label":"main","file_type":"code","source_file":"src/main.ts","source_location":"L1","_origin":"ast"}
    ],"edges":[{"source":"caller","target":"ghost_render","relation":"calls","confidence":"EXTRACTED","source_file":"src/main.ts","weight":1.0}]}));
    assert!(!has_node(&graph, "ghost_render"));
    assert!(has_edge(&graph, "caller", "ast_render"));
}

#[test]
fn test_ghost_merge_uses_source_file_not_basename() {
    let graph = build_value(json!({"nodes":[
        {"id":"a_render","label":"render","file_type":"code","source_file":"src/a/index.ts","source_location":"L10","_origin":"ast"},
        {"id":"b_render","label":"render","file_type":"code","source_file":"src/b/index.ts","source_location":"L20","_origin":"ast"},
        {"id":"ghost_render","label":"render","file_type":"code","source_file":"src/a/index.ts"},
        {"id":"caller","label":"main","file_type":"code","source_file":"src/main.ts","source_location":"L1","_origin":"ast"}
    ],"edges":[{"source":"caller","target":"ghost_render","relation":"calls","confidence":"EXTRACTED","source_file":"src/main.ts","weight":1.0}]}));
    assert!(!has_node(&graph, "ghost_render"));
    assert!(has_edge(&graph, "caller", "a_render"));
    assert!(!has_edge(&graph, "caller", "b_render"));
    assert!(has_node(&graph, "b_render"));
}

#[test]
fn test_ghost_merge_not_across_directories_same_basename() {
    let graph = build_value(json!({"nodes":[
        {"id":"docs_a_index","label":"Quickstart","file_type":"document","source_file":"docs/product_a/index.md","source_location":"L1"},
        {"id":"docs_b_index","label":"Quickstart","file_type":"document","source_file":"docs/product_b/index.md"},
        {"id":"docs_hub","label":"Docs","file_type":"concept","source_file":"docs/hub.md","source_location":"L1"}
    ],"edges":[{"source":"docs_hub","target":"docs_b_index","relation":"links_to","confidence":"INFERRED","source_file":"docs/hub.md"}]}));
    assert!(has_node(&graph, "docs_a_index") && has_node(&graph, "docs_b_index"));
    assert!(has_edge(&graph, "docs_hub", "docs_b_index"));
    assert!(!has_edge(&graph, "docs_hub", "docs_a_index"));
}

#[test]
fn test_ghost_merge_non_ast_different_files_both_survive() {
    let graph = build_value_with(
        json!({"nodes":[
        {"id":"dir_a_update_build_merge","label":"build_merge() function","file_type":"concept","source_file":"dir_a/update.md","source_location":"L10"},
        {"id":"dir_b_update_build_merge","label":"build_merge() function","file_type":"concept","source_file":"dir_b/update.md","source_location":"L12"}
    ],"edges":[]}),
        no_dedup(),
    );
    let mut ids: Vec<_> = graph.nodes.iter().map(|node| node.id.as_str()).collect();
    ids.sort_unstable();
    assert_eq!(
        ids,
        vec!["dir_a_update_build_merge", "dir_b_update_build_merge"]
    );
}

#[test]
fn test_ghost_merge_non_ast_same_file_still_merges() {
    let graph = build_value(json!({"nodes":[
        {"id":"a_foo","label":"Foo","file_type":"concept","source_file":"x/doc.md","source_location":"L1"},
        {"id":"b_foo","label":"Foo","file_type":"concept","source_file":"x/doc.md","source_location":"L2"}
    ],"edges":[]}));
    assert_eq!(graph.nodes.len(), 1);
}

fn call_extraction() -> Extraction {
    extraction(json!({
        "nodes":[
            {"id":"x_b","label":"b()","file_type":"code","source_file":"x.js","source_location":"L1","_origin":"ast"},
            {"id":"x_a","label":"a()","file_type":"code","source_file":"x.js","source_location":"L2","_origin":"ast"}
        ],
        "edges":[{"source":"x_a","target":"x_b","relation":"calls","confidence":"EXTRACTED","source_file":"x.js","_origin":"ast"}]
    }))
}

#[test]
fn test_build_merge_preserves_call_edge_direction() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("graph.json");
    let graph = build_graph_with_options(&[call_extraction()], no_dedup()).unwrap();
    write_graph_atomic(&path, &graph, true).unwrap();
    let saved = read_graph(&path).unwrap();
    assert_eq!(saved.links[0].source, "x_a");
    assert_eq!(saved.links[0].target, "x_b");
    let merged = build_merge(&[], &path, &[], None).unwrap();
    write_graph_atomic(&path, &merged, true).unwrap();
    let reloaded = read_graph(&path).unwrap();
    assert_eq!(reloaded.links[0].source, "x_a");
    assert_eq!(reloaded.links[0].target, "x_b");
}

#[test]
fn test_build_merge_directed_edge_direction_survives_round_trip() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("graph.json");
    let graph = build_graph_with_options(
        &[call_extraction()],
        BuildOptions {
            directed: true,
            ..no_dedup()
        },
    )
    .unwrap();
    write_graph_atomic(&path, &graph, true).unwrap();
    let merged = build_merge(&[], &path, &[], None).unwrap();
    assert!(merged.directed);
    assert!(has_edge(&merged, "x_a", "x_b"));
    assert!(!has_edge(&merged, "x_b", "x_a"));
}

#[test]
fn test_build_merge_inherits_directed_flag_from_disk() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("graph.json");
    let ext = extraction(
        json!({"nodes":[{"id":"a","label":"a","file_type":"concept","source_file":"x.md","source_location":"L1"}],"edges":[]}),
    );
    let directed = build_graph_with_options(
        std::slice::from_ref(&ext),
        BuildOptions {
            directed: true,
            ..no_dedup()
        },
    )
    .unwrap();
    write_graph_atomic(&path, &directed, true).unwrap();
    assert!(build_merge(&[], &path, &[], None).unwrap().directed);
    let undirected = build_graph_with_options(&[ext], no_dedup()).unwrap();
    write_graph_atomic(&path, &undirected, true).unwrap();
    assert!(!build_merge(&[], &path, &[], None).unwrap().directed);
}

#[test]
fn test_build_merge_fresh_graph_defaults_undirected() {
    let temp = TempDir::new().unwrap();
    let graph = build_merge(&[], temp.path().join("missing.json"), &[], None).unwrap();
    assert!(!graph.directed);
}

#[test]
fn test_build_merge_explicit_directed_overrides_disk_flag() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("graph.json");
    let ext = extraction(
        json!({"nodes":[{"id":"a","label":"a","file_type":"concept","source_file":"x.md","source_location":"L1"}],"edges":[]}),
    );
    let directed = build_graph_with_options(
        std::slice::from_ref(&ext),
        BuildOptions {
            directed: true,
            ..no_dedup()
        },
    )
    .unwrap();
    write_graph_atomic(&path, &directed, true).unwrap();
    let explicit_false = build_merge_with_options(
        &[],
        &path,
        &[],
        None,
        IncrementalOptions {
            directed: Some(false),
            ..incremental_no_dedup()
        },
    )
    .unwrap();
    assert!(!explicit_false.directed);
    let undirected = build_graph_with_options(&[ext], no_dedup()).unwrap();
    write_graph_atomic(&path, &undirected, true).unwrap();
    let explicit_true = build_merge_with_options(
        &[],
        &path,
        &[],
        None,
        IncrementalOptions {
            directed: Some(true),
            ..incremental_no_dedup()
        },
    )
    .unwrap();
    assert!(explicit_true.directed);
}

#[test]
fn test_build_from_json_preserves_first_direction_on_bidirectional_pair() {
    let options = BuildOptions {
        collapse_undirected_reverse_edges: true,
        deduplicate_semantic_nodes: false,
        ..BuildOptions::default()
    };
    let graph = build_value_with(
        json!({
            "nodes":[
                {"id":"a_handler","label":"a","file_type":"code","source_file":"a.ts"},
                {"id":"z_emitter","label":"z","file_type":"code","source_file":"z.ts"}
            ],
            "edges":[
                {"source":"a_handler","target":"z_emitter","relation":"calls","confidence":"EXTRACTED","source_file":"a.ts"},
                {"source":"z_emitter","target":"a_handler","relation":"calls","confidence":"EXTRACTED","source_file":"z.ts"}
            ]
        }),
        options,
    );
    assert_eq!(graph.links.len(), 1);
    assert_eq!(graph.links[0].extra["_src"], "a_handler");
    assert_eq!(graph.links[0].extra["_tgt"], "z_emitter");
}

fn loaded_parallel_graph(directed: bool, multigraph: bool, links: Value) -> KnowledgeGraph {
    serde_json::from_value(json!({
        "directed": directed,
        "multigraph": multigraph,
        "graph": {},
        "nodes": [
            {"id":"a","label":"A","file_type":"concept","source_file":""},
            {"id":"b","label":"B","file_type":"concept","source_file":""}
        ],
        "links": links
    }))
    .unwrap()
}

#[test]
fn test_edge_data_simple_graph() {
    let graph = loaded_parallel_graph(
        false,
        false,
        json!([{"source":"a","target":"b","relation":"calls","confidence":"EXTRACTED"}]),
    );
    let data = edge_data(&graph, "a", "b").unwrap();
    assert_eq!(data.relation, "calls");
    assert_eq!(data.confidence, Confidence::Extracted);
}

#[test]
fn test_edge_datas_simple_graph_returns_singleton_list() {
    let graph = loaded_parallel_graph(
        false,
        false,
        json!([{"source":"a","target":"b","relation":"calls","confidence":"EXTRACTED"}]),
    );
    let data = edge_datas(&graph, "a", "b");
    assert_eq!(data.len(), 1);
    assert_eq!(data[0].relation, "calls");
}

#[test]
fn test_edge_data_multigraph_with_parallel_edges() {
    let graph = loaded_parallel_graph(
        false,
        true,
        json!([
            {"source":"a","target":"b","relation":"calls","confidence":"EXTRACTED"},
            {"source":"a","target":"b","relation":"references","confidence":"INFERRED"}
        ]),
    );
    assert!(matches!(
        edge_data(&graph, "a", "b").unwrap().relation.as_str(),
        "calls" | "references"
    ));
}

#[test]
fn test_edge_datas_multigraph_returns_all_parallel_edges() {
    let graph = loaded_parallel_graph(
        false,
        true,
        json!([
            {"source":"a","target":"b","relation":"calls","confidence":"EXTRACTED"},
            {"source":"a","target":"b","relation":"references","confidence":"INFERRED"}
        ]),
    );
    let relations: std::collections::BTreeSet<_> = edge_datas(&graph, "a", "b")
        .into_iter()
        .map(|edge| edge.relation.as_str())
        .collect();
    assert_eq!(
        relations,
        std::collections::BTreeSet::from(["calls", "references"])
    );
}

#[test]
fn test_edge_data_multidigraph() {
    let graph = loaded_parallel_graph(
        true,
        true,
        json!([
            {"source":"a","target":"b","relation":"calls"},
            {"source":"a","target":"b","relation":"imports"}
        ]),
    );
    assert!(matches!(
        edge_data(&graph, "a", "b").unwrap().relation.as_str(),
        "calls" | "imports"
    ));
    assert_eq!(edge_datas(&graph, "a", "b").len(), 2);
}

#[test]
fn test_edge_data_node_link_multigraph_roundtrip() {
    let value = json!({
        "directed":false,"multigraph":true,"graph":{},
        "nodes":[{"id":"a","label":"A"},{"id":"b","label":"B"}],
        "links":[
            {"source":"a","target":"b","relation":"calls","confidence":"EXTRACTED"},
            {"source":"a","target":"b","relation":"references","confidence":"INFERRED"}
        ]
    });
    let graph: KnowledgeGraph = serde_json::from_value(value.clone()).unwrap();
    assert!(graph.multigraph);
    assert!(matches!(
        edge_data(&graph, "a", "b").unwrap().relation.as_str(),
        "calls" | "references"
    ));
    assert_eq!(edge_datas(&graph, "a", "b").len(), 2);
    let roundtrip: KnowledgeGraph =
        serde_json::from_value(serde_json::to_value(graph).unwrap()).unwrap();
    assert_eq!(roundtrip.links.len(), 2);
}

#[test]
fn test_build_from_json_relativizes_absolute_source_file() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("myproject");
    fs::create_dir(&root).unwrap();
    let absolute = root.join("docs/overview.md");
    let graph = build_value_at(
        json!({
            "nodes":[{"id":"overview_intro","label":"Intro","source_file":absolute,"file_type":"document"}],
            "edges":[{"source":"overview_intro","target":"overview_intro","relation":"self","confidence":"EXTRACTED","confidence_score":1.0,"source_file":absolute}]
        }),
        &root,
    );
    assert_eq!(
        node(&graph, "docs_overview_intro").source_file,
        "docs/overview.md"
    );
}

#[test]
fn test_build_relativizes_absolute_source_file() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("proj");
    fs::create_dir(&root).unwrap();
    let absolute = root.join("src/main.py");
    let ext = extraction(
        json!({"nodes":[{"id":"main_fn","label":"main","source_file":absolute,"file_type":"code"}],"edges":[]}),
    );
    let graph = build_graph_with_options_and_root(&[ext], &root, BuildOptions::default()).unwrap();
    assert_eq!(node(&graph, "src_main_fn").source_file, "src/main.py");
}

#[test]
fn test_build_relativizes_and_round_trips_container_source_provenance() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("proj");
    fs::create_dir(&root).unwrap();
    let container = root.join("archives/structured.tar");
    let container = container.to_string_lossy().to_string();
    let member = format!("{container}!/nested/config.toml");
    let extraction: Extraction = serde_json::from_value(json!({
        "nodes": [
            {"id":"member_a","label":"a","file_type":"document","source_file":member,"_container_source":container},
            {"id":"member_b","label":"b","file_type":"document","source_file":member,"_container_source":container}
        ],
        "edges": [
            {"source":"member_a","target":"member_b","relation":"contains","confidence":"EXTRACTED","source_file":member,"_container_source":container}
        ],
        "hyperedges": [
            {"id":"member_group","nodes":["member_a","member_b"],"relation":"groups","source_file":member,"_container_source":container}
        ]
    }))
    .unwrap();
    let graph =
        build_graph_with_options_and_root(&[extraction], &root, BuildOptions::default()).unwrap();
    let round_trip: KnowledgeGraph =
        serde_json::from_value(serde_json::to_value(graph).unwrap()).unwrap();

    assert!(round_trip.nodes.iter().all(|node| {
        node.extra
            .get(graphoxide_core::CONTAINER_SOURCE_ATTRIBUTE)
            .and_then(Value::as_str)
            == Some("archives/structured.tar")
    }));
    assert!(round_trip.links.iter().all(|edge| {
        edge.extra
            .get(graphoxide_core::CONTAINER_SOURCE_ATTRIBUTE)
            .and_then(Value::as_str)
            == Some("archives/structured.tar")
    }));
    assert!(round_trip.hyperedges.iter().all(|hyperedge| {
        hyperedge
            .get(graphoxide_core::CONTAINER_SOURCE_ATTRIBUTE)
            .and_then(Value::as_str)
            == Some("archives/structured.tar")
    }));
    assert!(round_trip
        .nodes
        .iter()
        .all(|node| node.source_file == "archives/structured.tar!/nested/config.toml"));
}

#[test]
fn test_build_from_json_ambiguous_old_stem_alias_stays_dangling() {
    let temp = TempDir::new().unwrap();
    let graph = build_value_at(
        json!({
            "nodes":[
                {"id":"dev_monitoring_ping","label":"ping.h","file_type":"code","source_file":"Dev/monitoring/ping.h"},
                {"id":"www_pages_api_ping","label":"ping.php","file_type":"code","source_file":"www/pages/api/ping.php"},
                {"id":"dev_poker_server","label":"server.cpp","file_type":"code","source_file":"Dev/poker/server.cpp"}
            ],
            "edges":[{"source":"dev_poker_server","target":"ping","relation":"imports","confidence":"EXTRACTED","source_file":"Dev/poker/server.cpp"}]
        }),
        temp.path(),
    );
    assert!(!has_edge(&graph, "dev_poker_server", "dev_monitoring_ping"));
    assert!(!has_edge(&graph, "dev_poker_server", "www_pages_api_ping"));
}

#[test]
fn test_build_from_json_ambiguous_alias_detected_despite_header_impl_salting() {
    let temp = TempDir::new().unwrap();
    let graph = build_value_at(
        json!({
            "nodes":[
                {"id":"tools_aolserver_utility_h_tools_aolserver_utility","label":"utility.h","file_type":"code","source_file":"Tools/aolserver/utility.h"},
                {"id":"tools_aolserver_utility_cpp_tools_aolserver_utility","label":"utility.cpp","file_type":"code","source_file":"Tools/aolserver/utility.cpp"},
                {"id":"wwwapi_masque_com_pages_utility","label":"utility.php","file_type":"code","source_file":"wwwapi.masque.com/pages/utility.php"},
                {"id":"dev_poker_server","label":"server.cpp","file_type":"code","source_file":"Dev/poker/server.cpp"}
            ],
            "edges":[{"source":"dev_poker_server","target":"utility","relation":"imports","confidence":"EXTRACTED","source_file":"Dev/poker/server.cpp"}]
        }),
        temp.path(),
    );
    assert!(!has_edge(
        &graph,
        "dev_poker_server",
        "wwwapi_masque_com_pages_utility"
    ));
    assert!(!has_edge(
        &graph,
        "dev_poker_server",
        "tools_aolserver_utility_h_tools_aolserver_utility"
    ));
}

#[test]
fn test_build_from_json_unambiguous_old_stem_alias_still_resolves() {
    let temp = TempDir::new().unwrap();
    let graph = build_value_at(
        json!({
            "nodes":[
                {"id":"dev_monitoring_utility","label":"utility.h","file_type":"code","source_file":"Dev/monitoring/utility.h"},
                {"id":"dev_poker_server","label":"server.cpp","file_type":"code","source_file":"Dev/poker/server.cpp"}
            ],
            "edges":[{"source":"dev_poker_server","target":"utility","relation":"imports","confidence":"EXTRACTED","source_file":"Dev/poker/server.cpp"}]
        }),
        temp.path(),
    );
    assert!(has_edge(
        &graph,
        "dev_poker_server",
        "dev_monitoring_utility"
    ));
}

#[test]
fn test_build_from_json_relative_source_file_unchanged() {
    let temp = TempDir::new().unwrap();
    let graph = build_value_at(
        json!({"nodes":[
        {"id":"foo_bar","label":"bar","source_file":"src/foo.py","file_type":"code"}
    ],"edges":[]}),
        temp.path(),
    );
    assert_eq!(node(&graph, "src_foo_bar").source_file, "src/foo.py");
}

#[test]
fn test_build_merge_prune_absolute_paths_match_relative_nodes() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("corpus");
    fs::create_dir(&root).unwrap();
    let path = temp.path().join("graph.json");
    let graph = build_value_with(
        json!({
            "nodes":[
                {"id":"n1","label":"login","file_type":"code","source_file":"module_a/auth.py"},
                {"id":"n2","label":"format_date","file_type":"code","source_file":"module_b/utils.py"}
            ],
            "edges":[{"source":"n1","target":"n2","relation":"calls","confidence":"EXTRACTED","source_file":"module_b/utils.py","weight":1.0}]
        }),
        no_dedup(),
    );
    write_graph_atomic(&path, &graph, true).unwrap();
    let merged = build_merge(&[], &path, &[root.join("module_b/utils.py")], Some(&root)).unwrap();
    assert!(!merged.nodes.iter().any(|node| node.label == "format_date"));
    assert!(merged.nodes.iter().any(|node| node.label == "login"));
    assert!(merged.links.is_empty());
}

#[test]
fn test_build_merge_prune_windows_backslash_paths() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("corpus");
    fs::create_dir(&root).unwrap();
    let path = temp.path().join("graph.json");
    let graph = build_value_with(
        json!({"nodes":[
        {"id":"n1","label":"parse_date","file_type":"code","source_file":"module_b/utils.py"}
    ],"edges":[]}),
        no_dedup(),
    );
    write_graph_atomic(&path, &graph, true).unwrap();
    let windows = PathBuf::from(
        root.join("module_b/utils.py")
            .to_string_lossy()
            .replace('/', "\\"),
    );
    let merged = build_merge(&[], &path, &[windows], Some(&root)).unwrap();
    assert!(!merged.nodes.iter().any(|node| node.label == "parse_date"));
}

#[test]
fn test_build_merge_replaces_changed_file_stale_edges() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("corpus");
    fs::create_dir(&root).unwrap();
    let path = temp.path().join("graph.json");
    let original = build_value_with(
        json!({
            "nodes":[
                {"id":"A","label":"A","file_type":"document","source_file":"changed.md"},
                {"id":"B","label":"B","file_type":"document","source_file":"changed.md"},
                {"id":"K","label":"K","file_type":"document","source_file":"keep.md"}
            ],
            "edges":[
                {"source":"A","target":"B","relation":"references","confidence":"EXTRACTED","source_file":"changed.md","weight":1.0},
                {"source":"K","target":"A","relation":"references","confidence":"EXTRACTED","source_file":"keep.md","weight":1.0}
            ]
        }),
        no_dedup(),
    );
    write_graph_atomic(&path, &original, true).unwrap();
    let absolute = root.join("changed.md").to_string_lossy().replace('/', "\\");
    let fresh = extraction(json!({
        "nodes":[
            {"id":"A","label":"A","file_type":"document","source_file":absolute},
            {"id":"C","label":"C","file_type":"document","source_file":absolute}
        ],
        "edges":[{"source":"A","target":"C","relation":"references","confidence":"EXTRACTED","source_file":absolute,"weight":1.0}]
    }));
    let merged =
        build_merge_with_options(&[fresh], &path, &[], Some(&root), incremental_no_dedup())
            .unwrap();
    assert!(!merged.nodes.iter().any(|node| node.label == "B"));
    assert!(!has_edge(&merged, "A", "B"));
    assert!(merged.nodes.iter().any(|node| node.label == "C"));
    assert!(has_edge(&merged, "A", "C"));
    assert!(merged.nodes.iter().any(|node| node.label == "K"));
    assert!(has_edge(&merged, "K", "A"));
}

fn write_two_tier_graph(path: &std::path::Path) {
    let value = json!({
        "directed":false,
        "nodes":[
            {"id":"docs_readme","label":"Readme","file_type":"document","source_file":"docs/readme.md","source_location":"L1","_origin":"ast"},
            {"id":"docs_readme_intro","label":"Intro","file_type":"document","source_file":"docs/readme.md","source_location":"L3","_origin":"ast"},
            {"id":"auth_flow","label":"Auth Flow","file_type":"concept","source_file":"docs/readme.md","source_location":null}
        ],
        "links":[{"source":"docs_readme","target":"docs_readme_intro","relation":"contains","confidence":"EXTRACTED","source_file":"docs/readme.md","source_location":"L3","weight":1.0,"_origin":"ast"}],
        "hyperedges":[{"id":"auth_group","label":"Auth Group","nodes":["docs_readme","auth_flow"],"relation":"form","confidence":"INFERRED","source_file":"docs/readme.md"}]
    });
    fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
}

#[test]
fn test_build_merge_semantic_reextract_preserves_ast_layer() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("graph.json");
    write_two_tier_graph(&path);
    let fresh = extraction(json!({"nodes":[
        {"id":"session_model","label":"Session Model","file_type":"concept","source_file":"docs/readme.md","source_location":null}
    ],"edges":[]}));
    let graph =
        build_merge_with_options(&[fresh], &path, &[], None, incremental_no_dedup()).unwrap();
    assert!(has_node(&graph, "docs_readme") && has_node(&graph, "docs_readme_intro"));
    assert!(has_edge(&graph, "docs_readme", "docs_readme_intro"));
    assert!(!has_node(&graph, "auth_flow"));
    assert!(has_node(&graph, "session_model"));
}

#[test]
fn test_build_merge_ast_reextract_preserves_semantic_layer() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("graph.json");
    write_two_tier_graph(&path);
    let fresh = extraction(json!({
        "nodes":[
            {"id":"docs_readme","label":"Readme","file_type":"document","source_file":"docs/readme.md","source_location":"L1","_origin":"ast"},
            {"id":"docs_readme_quickstart","label":"Quickstart","file_type":"document","source_file":"docs/readme.md","source_location":"L5","_origin":"ast"}
        ],
        "edges":[{"source":"docs_readme","target":"docs_readme_quickstart","relation":"contains","confidence":"EXTRACTED","source_file":"docs/readme.md","source_location":"L5","weight":1.0,"_origin":"ast"}]
    }));
    let graph =
        build_merge_with_options(&[fresh], &path, &[], None, incremental_no_dedup()).unwrap();
    assert!(has_node(&graph, "auth_flow"));
    assert!(!has_node(&graph, "docs_readme_intro"));
    assert!(has_node(&graph, "docs_readme_quickstart"));
    assert!(has_edge(&graph, "docs_readme", "docs_readme_quickstart"));
    assert!(graph
        .hyperedges
        .iter()
        .any(|value| value["id"] == "auth_group"));
}

#[test]
fn test_merge_raw_extraction_tier_scoped() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("graph.json");
    write_two_tier_graph(&path);
    let fresh = extraction(json!({"nodes":[
        {"id":"session_model","label":"Session Model","file_type":"concept","source_file":"docs/readme.md","source_location":null}
    ],"edges":[]}));
    let merged = merge_raw_extraction(&fresh, &path, &[], None).unwrap();
    let ids: std::collections::BTreeSet<_> =
        merged.nodes.iter().map(|node| node.id.as_str()).collect();
    assert!(ids.is_superset(&std::collections::BTreeSet::from([
        "docs_readme",
        "docs_readme_intro",
        "session_model"
    ])));
    assert!(!ids.contains("auth_flow"));
    assert!(merged
        .edges
        .iter()
        .any(|edge| edge.source == "docs_readme" && edge.target == "docs_readme_intro"));
    assert!(!merged
        .hyperedges
        .iter()
        .any(|value| value["id"] == "auth_group"));
}

#[test]
fn test_incremental_container_members_follow_the_outer_source_lifecycle() {
    let baseline: KnowledgeGraph = serde_json::from_value(json!({
        "nodes": [
            {"id":"archive","label":"structured.tar","file_type":"document","source_file":"archives/structured.tar","type":"container"},
            {"id":"kept_old","label":"kept","file_type":"document","source_file":"archives/structured.tar!/nested/kept.toml","source_location":"L1","_origin":"fallback","_container_source":"archives/structured.tar"},
            {"id":"removed","label":"removed","file_type":"document","source_file":"archives/structured.tar!/nested/removed.csv","source_location":"L1","_origin":"fallback","_container_source":"archives/structured.tar"},
            {"id":"literal_member_old","label":"archive literal","file_type":"code","source_file":"archives/structured.tar!/literal.rs","source_location":"L1","_origin":"ast","_container_source":"archives/structured.tar"},
            {"id":"unrelated","label":"unrelated","file_type":"code","source_file":"src/unrelated.rs","source_location":"L1","_origin":"ast"},
            {"id":"literal","label":"literal","file_type":"code","source_file":"archives/structured.tar!/literal.rs","source_location":"L1","_origin":"ast"}
        ],
        "links": [
            {"source":"archive","target":"kept_old","relation":"contains","confidence":"EXTRACTED","source_file":"archives/structured.tar"},
            {"source":"kept_old","target":"removed","relation":"references","confidence":"EXTRACTED","source_file":"archives/structured.tar!/nested/removed.csv","source_location":"L1","_origin":"fallback","_container_source":"archives/structured.tar"}
        ],
        "hyperedges": [
            {"id":"removed_group","nodes":["kept_old","removed"],"relation":"groups","source_file":"archives/structured.tar!/nested/removed.csv","_container_source":"archives/structured.tar"},
            {"id":"owned_without_source","nodes":["kept_old","removed"],"relation":"groups","_container_source":"archives/structured.tar"}
        ]
    }))
    .unwrap();

    let unrelated: Extraction = serde_json::from_value(json!({
        "nodes": [
            {"id":"changed_elsewhere","label":"changed","file_type":"code","source_file":"src/changed.rs","source_location":"L1","_origin":"ast"}
        ],
        "edges": []
    }))
    .unwrap();
    let retained =
        graphoxide_graph::incremental::merge_raw_extraction_from_graph_with_materialization_limit(
            unrelated,
            &baseline,
            &[],
            None,
            1024 * 1024,
        )
        .unwrap();
    assert!(retained.nodes.iter().any(|node| node.id == "kept_old"));
    assert!(retained.nodes.iter().any(|node| node.id == "removed"));
    assert!(retained
        .nodes
        .iter()
        .any(|node| node.id == "literal_member_old"));
    assert!(retained.nodes.iter().any(|node| node.id == "literal"));
    assert!(retained
        .hyperedges
        .iter()
        .any(|value| value["id"] == "removed_group"));
    assert!(retained
        .hyperedges
        .iter()
        .any(|value| value["id"] == "owned_without_source"));

    let changed_archive: Extraction = serde_json::from_value(json!({
        "nodes": [
            {"id":"archive_current","label":"structured.tar","file_type":"document","source_file":"archives/structured.tar","type":"container"},
            {"id":"kept_current","label":"kept","file_type":"document","source_file":"archives/structured.tar!/nested/kept.toml","source_location":"L1","_origin":"fallback","_container_source":"archives/structured.tar"},
            {"id":"literal_member_current","label":"archive literal","file_type":"code","source_file":"archives/structured.tar!/literal.rs","source_location":"L2","_origin":"ast","_container_source":"archives/structured.tar"}
        ],
        "edges": [
            {"source":"archive_current","target":"kept_current","relation":"contains","confidence":"EXTRACTED","source_file":"archives/structured.tar"}
        ]
    }))
    .unwrap();
    let replaced = graphoxide_graph::incremental::merge_raw_extraction_from_graph_with_rebuilt_sources_and_materialization_limit(
            changed_archive,
            &baseline,
            &[PathBuf::from("archives/structured.tar")],
            &[],
            &[],
            None,
            1024 * 1024,
        )
        .unwrap();
    let replaced_ids = replaced
        .nodes
        .iter()
        .map(|node| node.id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert!(replaced_ids.contains("archive_current"));
    assert!(replaced_ids.contains("kept_current"));
    assert!(replaced_ids.contains("literal_member_current"));
    assert!(replaced_ids.contains("unrelated"));
    assert!(replaced_ids.contains("literal"));
    assert!(!replaced_ids.contains("archive"));
    assert!(!replaced_ids.contains("kept_old"));
    assert!(!replaced_ids.contains("removed"));
    assert!(!replaced_ids.contains("literal_member_old"));
    assert!(replaced.hyperedges.iter().all(|value| !matches!(
        value["id"].as_str(),
        Some("removed_group" | "owned_without_source")
    )));

    let rejected_archive: Extraction = serde_json::from_value(json!({
        "nodes": [
            {"id":"archive_rejected","label":"structured.tar","file_type":"document","source_file":"archives/structured.tar","type":"format_inventory","parse_status":"rejected"}
        ],
        "edges": []
    }))
    .unwrap();
    let rejected = graphoxide_graph::incremental::merge_raw_extraction_from_graph_with_rebuilt_sources_and_materialization_limit(
            rejected_archive,
            &baseline,
            &[PathBuf::from("archives/structured.tar")],
            &[],
            &[],
            None,
            1024 * 1024,
        )
        .unwrap();
    assert!(rejected
        .nodes
        .iter()
        .any(|node| node.id == "archive_rejected"));
    assert!(rejected.nodes.iter().any(|node| node.id == "literal"));
    assert!(rejected.nodes.iter().any(|node| node.id == "unrelated"));
    assert!(rejected.nodes.iter().all(|node| {
        !matches!(
            node.id.as_str(),
            "archive" | "kept_old" | "removed" | "literal_member_old"
        )
    }));
    assert!(rejected.hyperedges.is_empty());

    let pruned =
        graphoxide_graph::incremental::merge_raw_extraction_from_graph_with_materialization_limit(
            Extraction::default(),
            &baseline,
            &[PathBuf::from("archives/structured.tar")],
            None,
            1024 * 1024,
        )
        .unwrap();
    assert_eq!(
        pruned
            .nodes
            .iter()
            .map(|node| node.id.as_str())
            .collect::<Vec<_>>(),
        vec!["unrelated", "literal"],
        "deleting the outer archive must prune marked members but preserve a colliding real path"
    );
    assert!(pruned.edges.is_empty());
    assert!(pruned.hyperedges.is_empty());

    let changed_literal: Extraction = serde_json::from_value(json!({
        "nodes": [
            {"id":"literal_current","label":"literal","file_type":"code","source_file":"archives/structured.tar!/literal.rs","source_location":"L2","_origin":"ast"}
        ],
        "edges": []
    }))
    .unwrap();
    let literal_replaced =
        graphoxide_graph::incremental::merge_raw_extraction_from_graph_with_materialization_limit(
            changed_literal,
            &baseline,
            &[],
            None,
            1024 * 1024,
        )
        .unwrap();
    assert!(literal_replaced
        .nodes
        .iter()
        .any(|node| node.id == "literal_member_old"));
    assert!(literal_replaced
        .nodes
        .iter()
        .any(|node| node.id == "literal_current"));
    assert!(!literal_replaced
        .nodes
        .iter()
        .any(|node| node.id == "literal"));

    let literal_pruned =
        graphoxide_graph::incremental::merge_raw_extraction_from_graph_with_materialization_limit(
            Extraction::default(),
            &baseline,
            &[PathBuf::from("archives/structured.tar!/literal.rs")],
            None,
            1024 * 1024,
        )
        .unwrap();
    assert!(literal_pruned
        .nodes
        .iter()
        .any(|node| node.id == "literal_member_old"));
    assert!(!literal_pruned.nodes.iter().any(|node| node.id == "literal"));
}

#[test]
fn test_authoritative_rebuild_handles_container_to_structured_transition() {
    let baseline: KnowledgeGraph = serde_json::from_value(json!({
        "nodes": [
            {"id":"config","label":"config.json","file_type":"document","source_file":"config.json","type":"container"},
            {"id":"old_member","label":"old.json","file_type":"document","source_file":"config.json","type":"container_member"},
            {"id":"seeded_a","label":"chosen concept A","file_type":"concept","source_file":"config.json","source_location":null,"_origin":"semantic"},
            {"id":"seeded_b","label":"chosen concept B","file_type":"concept","source_file":"config.json","source_location":null,"_origin":"semantic"}
        ],
        "links": [
            {"source":"config","target":"old_member","relation":"contains","confidence":"EXTRACTED","source_file":"config.json"},
            {"source":"seeded_a","target":"seeded_b","relation":"contains","confidence":"INFERRED","source_file":"config.json","_origin":"semantic"}
        ]
    }))
    .unwrap();
    let fresh: Extraction = serde_json::from_value(json!({
        "nodes": [
            {"id":"config","label":"config.json","file_type":"document","source_file":"config.json","source_location":"L1","_origin":"structured","type":"structured_file"}
        ],
        "edges": []
    }))
    .unwrap();

    let merged = graphoxide_graph::incremental::merge_raw_extraction_from_graph_with_rebuilt_sources_and_materialization_limit(
        fresh,
        &baseline,
        &[PathBuf::from("config.json")],
        &[],
        &[PathBuf::from("config.json")],
        None,
        1024 * 1024,
    )
    .unwrap();

    assert_eq!(
        merged
            .nodes
            .iter()
            .filter(|node| node.id == "config")
            .count(),
        1
    );
    assert!(merged.nodes.iter().any(|node| {
        node.id == "config"
            && node.extra.get("type").and_then(Value::as_str) == Some("structured_file")
    }));
    assert!(merged
        .nodes
        .iter()
        .all(|node| { node.extra.get("type").and_then(Value::as_str) != Some("container") }));
    assert!(merged.nodes.iter().all(|node| node.id != "old_member"));
    assert!(merged.nodes.iter().any(|node| node.id == "seeded_a"));
    assert!(merged.nodes.iter().any(|node| node.id == "seeded_b"));
    assert_eq!(merged.edges.len(), 1);
    assert!(merged
        .edges
        .iter()
        .any(|edge| edge.source == "seeded_a" && edge.target == "seeded_b"));
}

#[test]
fn test_authoritative_rebuild_handles_structured_to_container_transition() {
    let baseline: KnowledgeGraph = serde_json::from_value(json!({
        "nodes": [
            {"id":"config","label":"config.json","file_type":"document","source_file":"config.json","source_location":"L1","_origin":"structured","type":"structured_file"},
            {"id":"old_key","label":"old","file_type":"document","source_file":"config.json","source_location":"L2","_origin":"structured","type":"structured_key"},
            {"id":"seeded_a","label":"chosen concept A","file_type":"concept","source_file":"config.json","source_location":null,"_origin":"semantic"},
            {"id":"seeded_b","label":"chosen concept B","file_type":"concept","source_file":"config.json","source_location":null,"_origin":"semantic"}
        ],
        "links": [
            {"source":"config","target":"old_key","relation":"contains","confidence":"EXTRACTED","source_file":"config.json","source_location":"L2"},
            {"source":"seeded_a","target":"seeded_b","relation":"contains","confidence":"INFERRED","source_file":"config.json","_origin":"semantic"}
        ]
    }))
    .unwrap();
    let fresh: Extraction = serde_json::from_value(json!({
        "nodes": [
            {"id":"config","label":"config.json","file_type":"document","source_file":"config.json","type":"container"},
            {"id":"new_member","label":"new.json","file_type":"document","source_file":"config.json","type":"container_member"}
        ],
        "edges": [
            {"source":"config","target":"new_member","relation":"contains","confidence":"EXTRACTED","source_file":"config.json"}
        ]
    }))
    .unwrap();

    let merged = graphoxide_graph::incremental::merge_raw_extraction_from_graph_with_rebuilt_sources_and_materialization_limit(
        fresh,
        &baseline,
        &[PathBuf::from("config.json")],
        &[],
        &[],
        None,
        1024 * 1024,
    )
    .unwrap();

    assert_eq!(
        merged
            .nodes
            .iter()
            .filter(|node| node.id == "config")
            .count(),
        1
    );
    assert!(merged.nodes.iter().any(|node| {
        node.id == "config" && node.extra.get("type").and_then(Value::as_str) == Some("container")
    }));
    assert!(merged.nodes.iter().all(|node| node.id != "old_key"));
    assert!(merged.nodes.iter().any(|node| node.id == "new_member"));
    assert!(merged.nodes.iter().any(|node| node.id == "seeded_a"));
    assert!(merged.nodes.iter().any(|node| node.id == "seeded_b"));
    assert!(merged
        .edges
        .iter()
        .any(|edge| edge.source == "config" && edge.target == "new_member"));
    assert!(merged
        .edges
        .iter()
        .any(|edge| edge.source == "seeded_a" && edge.target == "seeded_b"));
    assert!(merged
        .edges
        .iter()
        .all(|edge| edge.source != "config" || edge.target != "old_key"));
}

#[test]
fn test_authoritative_container_rebuild_removes_unmarked_inventory_member() {
    let baseline: KnowledgeGraph = serde_json::from_value(json!({
        "nodes": [
            {"id":"archive","label":"archive.zip","file_type":"document","source_file":"archive.zip","type":"container"},
            {"id":"removed_member","label":"removed.json","file_type":"document","source_file":"archive.zip","type":"container_member"}
        ],
        "links": [
            {"source":"archive","target":"removed_member","relation":"contains","confidence":"EXTRACTED","source_file":"archive.zip"}
        ]
    }))
    .unwrap();
    let fresh: Extraction = serde_json::from_value(json!({
        "nodes": [
            {"id":"archive","label":"archive.zip","file_type":"document","source_file":"archive.zip","type":"container"}
        ],
        "edges": []
    }))
    .unwrap();

    let merged = graphoxide_graph::incremental::merge_raw_extraction_from_graph_with_rebuilt_sources_and_materialization_limit(
        fresh,
        &baseline,
        &[PathBuf::from("archive.zip")],
        &[],
        &[],
        None,
        1024 * 1024,
    )
    .unwrap();

    assert_eq!(merged.nodes.len(), 1);
    assert_eq!(merged.nodes[0].id, "archive");
    assert!(merged.edges.is_empty());
}

#[test]
fn test_explicit_merge_without_rebuilt_evidence_preserves_container_baseline() {
    let baseline: KnowledgeGraph = serde_json::from_value(json!({
        "nodes": [
            {"id":"archive","label":"archive.zip","file_type":"document","source_file":"archive.zip","type":"container"},
            {"id":"old_member","label":"old.json","file_type":"document","source_file":"archive.zip","type":"container_member"}
        ],
        "links": []
    }))
    .unwrap();
    let fresh: Extraction = serde_json::from_value(json!({
        "nodes": [
            {"id":"unowned_fresh","label":"fresh","file_type":"document","source_file":"archive.zip","source_location":"L1","_origin":"structured"}
        ],
        "edges": []
    }))
    .unwrap();

    let merged = graphoxide_graph::incremental::merge_raw_extraction_from_graph_with_rebuilt_sources_and_materialization_limit(
        fresh,
        &baseline,
        &[],
        &[],
        &[],
        None,
        1024 * 1024,
    )
    .unwrap();

    assert!(merged.nodes.iter().any(|node| node.id == "archive"));
    assert!(merged.nodes.iter().any(|node| node.id == "old_member"));
    assert!(merged.nodes.iter().any(|node| node.id == "unowned_fresh"));
}

#[test]
fn test_authoritative_rebuild_does_not_infer_cross_file_replacement() {
    let baseline: KnowledgeGraph = serde_json::from_value(json!({
        "nodes": [
            {"id":"main_old","label":"main","file_type":"code","source_file":"src/main.ts","source_location":"L1","_origin":"ast"},
            {"id":"dependency_old","label":"dependency","file_type":"code","source_file":"src/dependency.ts","source_location":"L1","_origin":"ast"}
        ],
        "links": []
    }))
    .unwrap();
    let fresh: Extraction = serde_json::from_value(json!({
        "nodes": [
            {"id":"main_new","label":"main","file_type":"code","source_file":"src/main.ts","source_location":"L1","_origin":"ast"},
            {"id":"dependency_cross_file","label":"cross-file provenance","file_type":"code","source_file":"src/dependency.ts","source_location":"L2","_origin":"ast"}
        ],
        "edges": []
    }))
    .unwrap();

    let merged = graphoxide_graph::incremental::merge_raw_extraction_from_graph_with_rebuilt_sources_and_materialization_limit(
        fresh,
        &baseline,
        &[PathBuf::from("src/main.ts")],
        &[],
        &[],
        None,
        1024 * 1024,
    )
    .unwrap();

    assert!(merged.nodes.iter().all(|node| node.id != "main_old"));
    assert!(merged.nodes.iter().any(|node| node.id == "main_new"));
    assert!(merged.nodes.iter().any(|node| node.id == "dependency_old"));
    assert!(merged
        .nodes
        .iter()
        .any(|node| node.id == "dependency_cross_file"));
}

#[test]
fn test_authoritative_rebuild_preserves_postgresql_provider_replacement() {
    let baseline: KnowledgeGraph = serde_json::from_value(json!({
        "nodes": [
            {"id":"old_table","label":"old","file_type":"code","source_file":"postgresql:/host/db","source_location":"L1","_origin":"sql"}
        ],
        "links": []
    }))
    .unwrap();
    let fresh: Extraction = serde_json::from_value(json!({
        "nodes": [
            {"id":"new_table","label":"new","file_type":"code","source_file":"postgresql:/host/db","source_location":"L1","_origin":"sql"}
        ],
        "edges": []
    }))
    .unwrap();

    let unprivileged = graphoxide_graph::incremental::merge_raw_extraction_from_graph_with_rebuilt_sources_and_materialization_limit(
        fresh.clone(),
        &baseline,
        &[],
        &[],
        &[],
        None,
        1024 * 1024,
    )
    .unwrap();
    assert!(unprivileged.nodes.iter().any(|node| node.id == "old_table"));

    let merged = graphoxide_graph::incremental::merge_raw_extraction_from_graph_with_rebuilt_sources_and_materialization_limit(
        fresh,
        &baseline,
        &[],
        &["postgresql:/host/db".into()],
        &[],
        None,
        1024 * 1024,
    )
    .unwrap();

    assert!(merged.nodes.iter().all(|node| node.id != "old_table"));
    assert!(merged.nodes.iter().any(|node| node.id == "new_table"));
}

#[test]
fn test_merge_raw_extraction_from_loaded_graph_matches_path_merge() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("graph.json");
    write_two_tier_graph(&path);
    let existing = read_graph(&path).unwrap();
    let fresh = extraction(json!({"nodes":[
        {"id":"session_model","label":"Session Model","file_type":"concept","source_file":"docs/readme.md","source_location":null}
    ],"edges":[]}));
    let expected = merge_raw_extraction(&fresh, &path, &[], None).unwrap();

    // Once loaded, the bounded merge has no reason to touch the graph path.
    fs::remove_file(&path).unwrap();
    let actual =
        graphoxide_graph::incremental::merge_raw_extraction_from_graph_with_materialization_limit(
            fresh,
            &existing,
            &[],
            None,
            1024 * 1024,
        )
        .unwrap();

    assert_eq!(
        serde_json::to_value(actual).unwrap(),
        serde_json::to_value(expected).unwrap()
    );
}

#[test]
fn test_merge_raw_extraction_low_materialization_budget_fails_closed() {
    let existing = build_value_with(
        json!({"nodes":[{
            "id":"retained",
            "label":"x".repeat(4096),
            "file_type":"document",
            "source_file":"keep.md"
        }],"edges":[]}),
        no_dedup(),
    );
    let error =
        graphoxide_graph::incremental::merge_raw_extraction_from_graph_with_materialization_limit(
            Extraction::default(),
            &existing,
            &[],
            None,
            1024,
        )
        .unwrap_err();

    assert!(error.to_string().contains("estimated"));
    assert!(error.to_string().contains("exceeds 1024-byte"));
    assert_eq!(existing.nodes.len(), 1, "the baseline remains untouched");
}

#[test]
fn test_is_ast_tier_legacy_fallback() {
    assert!(is_ast_tier_value(&json!({"_origin":"ast"})));
    for origin in ["fallback", "terraform", "sql", "dotnet", "scip"] {
        assert!(
            is_ast_tier_value(&json!({"_origin":origin})),
            "{origin} must be replaced with the deterministic extraction tier"
        );
    }
    assert!(is_ast_tier_value(
        &json!({"_origin":"ast","source_location":null})
    ));
    assert!(is_ast_tier_value(&json!({"source_location":"L10"})));
    assert!(is_ast_tier_value(
        &json!({"_origin":"future-parser","source_location":"L10"})
    ));
    assert!(!is_ast_tier_value(&json!({"source_location":null})));
    assert!(!is_ast_tier_value(&json!({})));
    assert!(!is_ast_tier_value(
        &json!({"_origin":"semantic","source_location":"L10"})
    ));
}

#[test]
fn test_incremental_merge_replaces_every_deterministic_extractor_origin() {
    for origin in ["fallback", "terraform", "sql", "dotnet", "scip"] {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("graph.json");
        let original = build_value_with(
            json!({
                "nodes":[
                    {"id":"old_a","label":"old a","file_type":"code","source_file":"changed.src","source_location":"L1","_origin":origin},
                    {"id":"old_b","label":"old b","file_type":"code","source_file":"changed.src","source_location":"L2","_origin":origin}
                ],
                "edges":[
                    {"source":"old_a","target":"old_b","relation":"calls","confidence":"EXTRACTED","source_file":"changed.src","source_location":"L2","_origin":origin}
                ]
            }),
            no_dedup(),
        );
        write_graph_atomic(&path, &original, true).unwrap();
        let fresh = extraction(json!({
            "nodes":[
                {"id":"new_a","label":"new a","file_type":"code","source_file":"changed.src","source_location":"L1","_origin":origin},
                {"id":"new_c","label":"new c","file_type":"code","source_file":"changed.src","source_location":"L3","_origin":origin}
            ],
            "edges":[
                {"source":"new_a","target":"new_c","relation":"calls","confidence":"EXTRACTED","source_file":"changed.src","source_location":"L3","_origin":origin}
            ]
        }));

        let graph = build_merge_with_options(
            std::slice::from_ref(&fresh),
            &path,
            &[],
            None,
            incremental_no_dedup(),
        )
        .unwrap();
        assert!(!has_node(&graph, "old_a"), "stale {origin} node survived");
        assert!(!has_node(&graph, "old_b"), "stale {origin} node survived");
        assert!(!has_edge(&graph, "old_a", "old_b"));
        assert!(has_edge(&graph, "new_a", "new_c"));

        write_graph_atomic(&path, &original, true).unwrap();
        let raw = merge_raw_extraction(&fresh, &path, &[], None).unwrap();
        assert!(raw.nodes.iter().all(|node| node.id != "old_a"));
        assert!(raw.nodes.iter().all(|node| node.id != "old_b"));
        assert!(raw
            .edges
            .iter()
            .all(|edge| edge.source != "old_a" || edge.target != "old_b"));
    }
}

#[test]
fn test_incremental_merge_discovers_edge_and_hyperedge_only_sources() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("graph.json");
    let original = build_value_with(
        json!({
            "nodes":[
                {"id":"a","label":"a","file_type":"code","source_file":"keep.src","source_location":"L1","_origin":"ast"},
                {"id":"b","label":"b","file_type":"code","source_file":"keep.src","source_location":"L2","_origin":"ast"}
            ],
            "edges":[
                {"source":"a","target":"b","relation":"old","confidence":"EXTRACTED","source_file":"edge-only.sql","source_location":"L1","_origin":"sql"}
            ],
            "hyperedges":[
                {"id":"old_group","nodes":["a","b"],"relation":"old","source_file":"hyper-only.tf","_origin":"terraform"}
            ]
        }),
        no_dedup(),
    );
    write_graph_atomic(&path, &original, true).unwrap();
    let fresh = extraction(json!({
        "nodes":[],
        "edges":[
            {"source":"a","target":"b","relation":"new","confidence":"EXTRACTED","source_file":"edge-only.sql","source_location":"L2","_origin":"sql"}
        ],
        "hyperedges":[
            {"id":"new_group","nodes":["a","b"],"relation":"new","source_file":"hyper-only.tf","_origin":"terraform"}
        ]
    }));

    let merged = merge_raw_extraction(&fresh, &path, &[], None).unwrap();
    assert!(merged.edges.iter().all(|edge| edge.relation != "old"));
    assert!(merged.edges.iter().any(|edge| edge.relation == "new"));
    assert!(merged
        .hyperedges
        .iter()
        .all(|hyperedge| hyperedge["id"] != "old_group"));
    assert!(merged
        .hyperedges
        .iter()
        .any(|hyperedge| hyperedge["id"] == "new_group"));
}

#[test]
fn test_build_merge_root_collapses_convention_drift() {
    let temp = TempDir::new().unwrap();
    let out = temp.path().join("graphoxide-out");
    fs::create_dir(&out).unwrap();
    let path = out.join("graph.json");
    let stored = build_value_with(
        json!({"nodes":[
        {"id":"wiki_overview_overview","label":"Overview","file_type":"document","source_file":"docs/wiki/overview.md"},
        {"id":"wiki_overview_stale","label":"Stale","file_type":"document","source_file":"docs/wiki/overview.md"}
    ],"edges":[]}),
        no_dedup(),
    );
    write_graph_atomic(&path, &stored, true).unwrap();
    let drift = extraction(json!({"nodes":[
        {"id":"overview_overview","label":"Overview","file_type":"document","source_file":"overview.md"}
    ],"edges":[]}));
    let buggy =
        build_merge_with_options(&[drift], &path, &[], None, incremental_no_dedup()).unwrap();
    assert_eq!(buggy.nodes.len(), 3);
    write_graph_atomic(&path, &stored, true).unwrap();
    let fixed = extraction(json!({"nodes":[
        {"id":"wiki_overview_overview","label":"Overview","file_type":"document","source_file":temp.path().join("docs/wiki/overview.md")}
    ],"edges":[]}));
    let graph = build_merge_with_options(
        &[fixed],
        &path,
        &[],
        Some(temp.path()),
        incremental_no_dedup(),
    )
    .unwrap();
    assert_eq!(graph.nodes.len(), 1);
    assert!(!has_node(&graph, "docs_wiki_overview_stale"));
    assert_eq!(
        node(&graph, "docs_wiki_overview_overview").source_file,
        "docs/wiki/overview.md"
    );
}

#[test]
fn test_build_merge_rejects_oversized_existing_graph() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("graph.json");
    fs::write(
        &path,
        serde_json::to_vec(&json!({"nodes":[],"links":[]})).unwrap(),
    )
    .unwrap();
    let error = build_merge_with_options(
        &[],
        &path,
        &[],
        None,
        IncrementalOptions {
            max_graph_bytes: Some(8),
            ..IncrementalOptions::default()
        },
    )
    .unwrap_err();
    assert!(error.to_string().contains("exceeds"));
}

#[test]
fn test_build_from_json_skips_non_hashable_node_id() {
    let graph = build_value(json!({"nodes":[
        {"id":"a","label":"A","file_type":"code","source_file":"a.py"},
        {"id":["x","y"],"label":"B","file_type":"code","source_file":"b.py"},
        {"label":"C","file_type":"code","source_file":"c.py"}
    ],"edges":[]}));
    assert_eq!(
        graph
            .nodes
            .iter()
            .map(|node| node.id.as_str())
            .collect::<Vec<_>>(),
        vec!["a"]
    );
}

#[test]
fn test_build_from_json_skips_edge_with_non_hashable_endpoint() {
    let graph = build_value(json!({
        "nodes":[
            {"id":"a","label":"A","file_type":"code","source_file":"a.py"},
            {"id":"b","label":"B","file_type":"code","source_file":"b.py"}
        ],
        "edges":[
            {"source":"a","target":["b","c"],"relation":"calls","confidence":"INFERRED","source_file":"a.py"},
            {"source":"a","target":"b","relation":"imports","confidence":"EXTRACTED","source_file":"a.py"}
        ]
    }));
    assert_eq!(graph.nodes.len(), 2);
    assert_eq!(graph.links.len(), 1);
    assert!(has_edge(&graph, "a", "b"));
}

#[test]
fn test_graph_has_legacy_ids_detects_old_scheme() {
    let old = extraction(
        json!({"nodes":[{"id":"api_readme","source_file":"docs/v1/api/README.md","type":"document","source_location":"L1"}],"edges":[]}),
    );
    let new = extraction(
        json!({"nodes":[{"id":"docs_v1_api_readme","source_file":"docs/v1/api/README.md","type":"document","source_location":"L1"}],"edges":[]}),
    );
    assert!(graph_has_legacy_ids(
        &old.nodes,
        Some(std::path::Path::new("."))
    ));
    assert!(!graph_has_legacy_ids(
        &new.nodes,
        Some(std::path::Path::new("."))
    ));
    let top = extraction(
        json!({"nodes":[{"id":"setup","source_file":"setup.py","source_location":"L1"}],"edges":[]}),
    );
    assert!(!graph_has_legacy_ids(
        &top.nodes,
        Some(std::path::Path::new("."))
    ));
    let sourceless = extraction(json!({"nodes":[{"id":"x","label":"y"}],"edges":[]}));
    assert!(!graph_has_legacy_ids(
        &sourceless.nodes,
        Some(std::path::Path::new("."))
    ));
    let symbol = extraction(
        json!({"nodes":[{"id":"sub_thing","source_file":"pkg/sub/thing.go","type":"code","source_location":"L3"}],"edges":[]}),
    );
    assert!(!graph_has_legacy_ids(
        &symbol.nodes,
        Some(std::path::Path::new("."))
    ));
}

#[test]
fn test_semantic_rekey_relative_vs_absolute_source_file() {
    let relative = extraction(
        json!({"nodes":[{"id":"api_readme","source_file":"docs/v1/api/README.md","type":"document"}],"edges":[]}),
    );
    assert_eq!(
        semantic_id_remap(&[relative]),
        BTreeMap::from([("api_readme".to_owned(), "docs_v1_api_readme".to_owned())])
    );
    let absolute = extraction(
        json!({"nodes":[{"id":"api_readme","source_file":"/abs/docs/v1/api/README.md","type":"document"}],"edges":[]}),
    );
    assert!(semantic_id_remap(&[absolute]).is_empty());
}

#[test]
fn test_cross_language_imports_references_are_dropped() {
    let graph = build_value(json!({
        "nodes":[
            {"id":"backend_worker_py","label":"worker.py","file_type":"code","source_file":"backend/worker.py","source_location":"L1","_origin":"ast"},
            {"id":"src_time_ts","label":"time.ts","file_type":"code","source_file":"src/time.ts","source_location":"L1","_origin":"ast"},
            {"id":"src_util_ts","label":"util.ts","file_type":"code","source_file":"src/util.ts","source_location":"L1","_origin":"ast"}
        ],
        "edges":[
            {"source":"backend_worker_py","target":"src_time_ts","relation":"imports","confidence":"EXTRACTED","source_file":"backend/worker.py","weight":1.0},
            {"source":"src_time_ts","target":"src_util_ts","relation":"imports","confidence":"EXTRACTED","source_file":"src/time.ts","weight":1.0}
        ]
    }));
    assert!(!has_edge(&graph, "backend_worker_py", "src_time_ts"));
    assert!(has_edge(&graph, "src_time_ts", "src_util_ts"));
}

#[test]
fn test_cross_family_reference_to_unknown_ext_is_kept() {
    let graph = build_value(json!({
        "nodes":[
            {"id":"pkg_json","label":"package.json","file_type":"code","source_file":"package.json","source_location":"L1","_origin":"ast"},
            {"id":"src_app_ts","label":"app.ts","file_type":"code","source_file":"src/app.ts","source_location":"L1","_origin":"ast"}
        ],
        "edges":[{"source":"pkg_json","target":"src_app_ts","relation":"references","confidence":"EXTRACTED","source_file":"package.json","weight":1.0}]
    }));
    assert!(has_edge(&graph, "pkg_json", "src_app_ts"));
}

#[test]
fn test_markdown_doc_twin_merges_into_semantic_doc_node() {
    let graph = build_value(json!({
        "nodes":[
            {"id":"docs_readme_doc","label":"README","file_type":"document","source_file":"docs/readme.md","source_location":"L1"},
            {"id":"docs_readme","label":"readme.md","file_type":"document","source_file":"docs/readme.md","source_location":"L1"},
            {"id":"code_auth","label":"auth","file_type":"code","source_file":"auth.py","source_location":"L1"},
            {"id":"docs_guide","label":"guide.md","file_type":"document","source_file":"docs/guide.md","source_location":"L1"}
        ],
        "edges":[
            {"source":"docs_readme_doc","target":"code_auth","relation":"references","source_file":"docs/readme.md","confidence":"INFERRED","weight":1.0},
            {"source":"docs_guide","target":"docs_readme","relation":"references","source_file":"docs/guide.md","confidence":"EXTRACTED","weight":1.0}
        ]
    }));
    assert!(!has_node(&graph, "docs_readme"));
    assert!(has_node(&graph, "docs_readme_doc"));
    assert!(has_edge(&graph, "docs_guide", "docs_readme_doc"));
    assert!(has_edge(&graph, "docs_readme_doc", "code_auth"));
}

#[test]
fn test_doc_twin_merge_does_not_touch_code_symbols() {
    let graph = build_value(json!({"nodes":[
        {"id":"m_foo","label":"foo","file_type":"code","source_file":"m.py","source_location":"L1"},
        {"id":"m_foo_doc","label":"foo rationale","file_type":"rationale","source_file":"m.py","source_location":"L2"}
    ],"edges":[]}));
    assert!(has_node(&graph, "m_foo"));
    assert!(has_node(&graph, "m_foo_doc"));
}

#[test]
fn test_doc_twin_merge_preserves_declared_graphviz_ids() {
    let graph = build_value(json!({
        "nodes": [
            {
                "id": "architecture_diagram_graphviz_service",
                "label": "service",
                "file_type": "document",
                "source_file": "architecture.dot",
                "source_location": "L1",
                "diagram_format": "graphviz",
                "dot_id": "service"
            },
            {
                "id": "architecture_diagram_graphviz_service_doc",
                "label": "service_doc",
                "file_type": "document",
                "source_file": "architecture.dot",
                "source_location": "L2",
                "diagram_format": "graphviz",
                "dot_id": "service_doc"
            }
        ],
        "edges": [{
            "source": "architecture_diagram_graphviz_service",
            "target": "architecture_diagram_graphviz_service_doc",
            "relation": "flows_to",
            "source_file": "architecture.dot",
            "confidence": "EXTRACTED",
            "diagram_format": "graphviz"
        }]
    }));
    assert!(has_node(&graph, "architecture_diagram_graphviz_service"));
    assert!(has_node(
        &graph,
        "architecture_diagram_graphviz_service_doc"
    ));
    assert!(has_edge(
        &graph,
        "architecture_diagram_graphviz_service",
        "architecture_diagram_graphviz_service_doc"
    ));
}

#[test]
fn test_diagram_origin_remains_authoritative_during_semantic_enrichment() {
    let graph = build_value(json!({
        "nodes": [
            {
                "id": "architecture_diagram_graphviz_api",
                "label": "Declared API",
                "file_type": "document",
                "source_file": "architecture.dot",
                "source_location": "L2",
                "_origin": "diagram",
                "diagram_format": "graphviz",
                "dot_id": "api",
                "type": "node"
            },
            {
                "id": "architecture_diagram_graphviz_api",
                "label": "Model rewrite",
                "file_type": "concept",
                "source_file": "architecture.dot",
                "_origin": "semantic",
                "type": "concept",
                "summary": "Optional enrichment"
            }
        ],
        "edges": []
    }));
    let api = node(&graph, "architecture_diagram_graphviz_api");
    assert_eq!(api.label, "Declared API");
    assert_eq!(api.file_type, "document");
    assert_eq!(api.extra["_origin"], "diagram");
    assert_eq!(api.extra["type"], "node");
    assert_eq!(api.extra["dot_id"], "api");
    assert_eq!(api.extra["summary"], "Optional enrichment");
}

#[test]
fn test_build_from_json_prunes_dangling_hyperedge_members() {
    let graph = build_value(json!({
        "nodes":[
            {"id":"alpha","label":"alpha","file_type":"code","source_file":"a.py"},
            {"id":"beta","label":"beta","file_type":"code","source_file":"a.py"}
        ],
        "edges":[],
        "hyperedges":[
            {"id":"he_partial","nodes":["alpha","beta","ghost_member"],"source_file":"a.py"},
            {"id":"he_all_ghost","nodes":["ghost1","ghost2"],"source_file":"a.py"}
        ]
    }));
    assert_eq!(graph.hyperedges.len(), 1);
    assert_eq!(graph.hyperedges[0]["id"], "he_partial");
    assert_eq!(graph.hyperedges[0]["nodes"], json!(["alpha", "beta"]));
}
