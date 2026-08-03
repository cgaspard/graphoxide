use graphoxide_core::{Confidence, Edge, KnowledgeGraph, Node};
use graphoxide_query::*;
use serde_json::json;
use std::collections::{BTreeMap, HashMap, HashSet};

fn node(
    id: &str,
    label: &str,
    source_file: &str,
    source_location: Option<&str>,
    community: Option<i64>,
) -> Node {
    Node {
        id: id.into(),
        label: label.into(),
        file_type: "code".into(),
        source_file: source_file.into(),
        source_location: source_location.map(str::to_owned),
        community,
        extra: BTreeMap::new(),
    }
}

fn edge(source: &str, target: &str, relation: &str, context: Option<&str>) -> Edge {
    let mut extra = BTreeMap::new();
    if let Some(context) = context {
        extra.insert("context".into(), json!(context));
    }
    Edge {
        source: source.into(),
        target: target.into(),
        relation: relation.into(),
        confidence: if relation == "calls" {
            Confidence::Inferred
        } else {
            Confidence::Extracted
        },
        source_file: String::new(),
        extra,
    }
}

fn make_graph() -> KnowledgeGraph {
    KnowledgeGraph {
        nodes: vec![
            node("n1", "extract", "extract.py", Some("L10"), Some(0)),
            node("n2", "cluster", "cluster.py", Some("L5"), Some(0)),
            node("n3", "build", "build.py", Some("L1"), Some(1)),
            node("n4", "report", "report.py", Some("L1"), Some(1)),
            node("n5", "isolated", "other.py", Some("L1"), Some(2)),
        ],
        links: vec![
            edge("n1", "n2", "calls", Some("call")),
            edge("n2", "n3", "imports", Some("import")),
            edge("n3", "n4", "uses", None),
        ],
        ..KnowledgeGraph::default()
    }
}

fn big_graph() -> KnowledgeGraph {
    let mut graph = KnowledgeGraph::default();
    for i in 0..150 {
        graph.nodes.push(node(
            &format!("id{i}"),
            &format!("item node {i}"),
            &format!("pkg/item_{i}.py"),
            None,
            None,
        ));
    }
    graph
        .nodes
        .push(node("rareA", "ZebraQuokkaWidget", "zoo/zqw.py", None, None));
    graph.nodes.push(node(
        "rareB",
        "MarmosetGadget handler",
        "zoo/marmoset.py",
        None,
        None,
    ));
    graph
        .nodes
        .push(node("punct", "Foo.Bar:Baz", "pkg/foobar.py", None, None));
    graph
}

fn noisy_graph() -> KnowledgeGraph {
    let mut graph = KnowledgeGraph::default();
    for i in 0..20 {
        graph.nodes.push(node(
            &format!("err{i}"),
            &format!("error_handler_{i}"),
            &format!("err{i}.py"),
            None,
            Some(0),
        ));
        if i > 0 {
            graph.links.push(edge(
                &format!("err{}", i - 1),
                &format!("err{i}"),
                "calls",
                None,
            ));
        }
    }
    graph
        .nodes
        .push(node("fbs", "FooBarService", "service.py", None, Some(1)));
    graph
        .nodes
        .push(node("fbs_dep", "ServiceClient", "client.py", None, Some(1)));
    graph.links.push(edge("fbs", "fbs_dep", "uses", None));
    graph
}

fn id_set(index: &GraphIndex<'_>, positions: &HashSet<usize>) -> HashSet<String> {
    positions
        .iter()
        .map(|position| index.node(*position).id.clone())
        .collect()
}

fn ids(index: &GraphIndex<'_>, positions: &[usize]) -> Vec<String> {
    positions
        .iter()
        .map(|position| index.node(*position).id.clone())
        .collect()
}

fn positions(index: &GraphIndex<'_>, node_ids: &[&str]) -> HashSet<usize> {
    node_ids
        .iter()
        .map(|id| index.position(id).unwrap())
        .collect()
}

fn render(
    graph: &KnowledgeGraph,
    node_ids: &[&str],
    edge_ids: &[(&str, &str)],
    budget: usize,
    seed_ids: &[&str],
) -> String {
    let index = GraphIndex::new(graph);
    let visited = positions(&index, node_ids);
    let edges = edge_ids
        .iter()
        .map(|(source, target)| {
            (
                index.position(source).unwrap(),
                index.position(target).unwrap(),
            )
        })
        .collect::<Vec<_>>();
    let seeds = seed_ids
        .iter()
        .map(|id| index.position(id).unwrap())
        .collect::<Vec<_>>();
    subgraph_to_text(&index, &visited, &edges, budget, &seeds)
}

#[test]
fn test_communities_from_graph_basic() {
    let communities = communities_from_graph(&make_graph());
    assert_eq!(communities[&0], ["n1", "n2"]);
    assert_eq!(communities[&1], ["n3", "n4"]);
}

#[test]
fn test_communities_from_graph_no_community_attr() {
    let graph = KnowledgeGraph {
        nodes: vec![node("a", "foo", "", None, None)],
        ..KnowledgeGraph::default()
    };
    assert!(communities_from_graph(&graph).is_empty());
}

#[test]
fn test_communities_from_graph_isolated() {
    assert_eq!(communities_from_graph(&make_graph())[&2], ["n5"]);
}

#[test]
fn test_score_nodes_exact_label_match() {
    let graph = make_graph();
    let index = GraphIndex::new(&graph);
    assert_eq!(
        index.node(score_nodes(&index, &["extract".into()])[0].1).id,
        "n1"
    );
}

#[test]
fn test_score_nodes_no_match() {
    let graph = make_graph();
    assert!(score_nodes(&GraphIndex::new(&graph), &["xyzzy".into()]).is_empty());
}

#[test]
fn test_score_nodes_source_file_partial() {
    let graph = make_graph();
    let index = GraphIndex::new(&graph);
    assert!(ids(
        &index,
        &score_nodes(&index, &["cluster".into()])
            .iter()
            .map(|x| x.1)
            .collect::<Vec<_>>()
    )
    .contains(&"n2".into()));
}

#[test]
fn test_score_nodes_ignores_trailing_punctuation() {
    let graph = make_graph();
    let index = GraphIndex::new(&graph);
    assert_eq!(
        index
            .node(score_nodes(&index, &["extract?".into()])[0].1)
            .id,
        "n1"
    );
}

#[test]
fn test_score_nodes_multiword_exact_label_outranks_superset() {
    let mut graph = KnowledgeGraph::default();
    for (id, label) in [
        ("exact", "UOCE: Dehumidifier Driver"),
        ("super", "UOCE: Dehumidifier Driver State Machine"),
        ("decoy", "Dehumidifier Driver Helper"),
    ] {
        let mut item = node(id, label, "uoce_dehumidifier.yaml", None, Some(0));
        item.extra
            .insert("norm_label".into(), json!(label.to_lowercase()));
        graph.nodes.push(item);
    }
    let index = GraphIndex::new(&graph);
    let terms = "UOCE: Dehumidifier Driver"
        .split_whitespace()
        .map(|term| term.to_lowercase())
        .collect::<Vec<_>>();
    let scored = score_nodes(&index, &terms);
    assert_eq!(index.node(scored[0].1).id, "exact");
    assert!(scored[0].0 > scored[1].0);
}

#[test]
fn test_score_nodes_coverage_lone_generic_exact_hit_loses_to_multi_term_match() {
    let mut graph = KnowledgeGraph::default();
    for (id, label, source) in [
        ("target", "ClientLive.Index", "lib/clients_live/index.ex"),
        ("form", "ClientLive.Form", "lib/clients_live/form.ex"),
        ("show", "ClientLive.Show", "lib/clients_live/show.ex"),
    ] {
        graph.nodes.push(node(id, label, source, None, Some(0)));
    }
    for i in 0..3 {
        graph.nodes.push(node(
            &format!("leaf{i}"),
            "list()",
            &format!("lib/clients_live/helpers{i}.ex"),
            None,
            Some(0),
        ));
    }
    for i in 0..24 {
        graph.nodes.push(node(
            &format!("filler{i}"),
            &format!("shopping list {i}"),
            &format!("lib/filler{i}.ex"),
            None,
            Some(0),
        ));
    }
    let index = GraphIndex::new(&graph);
    let terms = "ClientLive.Index clients list columns"
        .split_whitespace()
        .map(|term| term.to_lowercase())
        .collect::<Vec<_>>();
    let scored = score_nodes(&index, &terms);
    let by_id: HashMap<_, _> = scored
        .iter()
        .map(|(score, position)| (index.node(*position).id.as_str(), *score))
        .collect();
    assert_eq!(index.node(scored[0].1).id, "target");
    assert!(by_id["target"] > by_id["leaf0"]);
}

#[test]
fn test_score_nodes_coverage_full_coverage_query_is_unchanged() {
    let graph = make_graph();
    let index = GraphIndex::new(&graph);
    let scored = score_nodes(&index, &["extract".into()]);
    let weight = compute_idf(&index, &["extract".into()])["extract"];
    let expected = (EXACT_MATCH_BONUS * 10.0 + EXACT_MATCH_BONUS + SOURCE_MATCH_BONUS) * weight;
    assert_eq!(index.node(scored[0].1).id, "n1");
    assert!((scored[0].0 - expected).abs() < 1e-10);
}

#[test]
fn test_find_node_ignores_trailing_punctuation() {
    let graph = make_graph();
    let index = GraphIndex::new(&graph);
    assert_eq!(ids(&index, &find_node(&index, "extract?")), ["n1"]);
}

#[test]
fn test_find_node_matches_full_punctuated_unicode_label() {
    let graph = KnowledgeGraph {
        nodes: vec![node(
            "n1",
            "Skill /auditar — Auditoría inquisitiva de enlaces",
            "",
            None,
            None,
        )],
        ..KnowledgeGraph::default()
    };
    let index = GraphIndex::new(&graph);
    assert_eq!(
        ids(
            &index,
            &find_node(&index, "Skill /auditar — Auditoría inquisitiva de enlaces")
        ),
        ["n1"]
    );
}

#[test]
fn test_find_node_matches_punctuated_file_label_exactly() {
    let mut first = node(
        "f1",
        "blockStream.ts",
        "lib/blockStream.ts",
        Some("L1"),
        None,
    );
    first
        .extra
        .insert("norm_label".into(), json!("blockstream.ts"));
    let mut second = node(
        "f2",
        "blockStream.test.ts",
        "lib/blockStream.test.ts",
        Some("L1"),
        None,
    );
    second
        .extra
        .insert("norm_label".into(), json!("blockstream.test.ts"));
    let graph = KnowledgeGraph {
        nodes: vec![first, second],
        ..KnowledgeGraph::default()
    };
    let index = GraphIndex::new(&graph);
    assert_eq!(index.node(find_node(&index, "blockStream.ts")[0]).id, "f1");
    assert_eq!(
        index.node(find_node(&index, "blockStream.test.ts")[0]).id,
        "f2"
    );
}

#[test]
fn test_find_node_resolves_when_label_and_norm_label_diverge() {
    let mut item = node("n1", "BlockStream", "lib/x.ts", Some("L1"), None);
    item.extra
        .insert("norm_label".into(), json!("blockstream.ts"));
    let graph = KnowledgeGraph {
        nodes: vec![item],
        ..KnowledgeGraph::default()
    };
    let index = GraphIndex::new(&graph);
    assert_eq!(ids(&index, &find_node(&index, "blockStream.ts")), ["n1"]);
}

#[test]
fn test_trigrams_basic() {
    assert_eq!(
        trigrams("foobar"),
        ["bar", "foo", "oba", "oob"]
            .into_iter()
            .map(str::to_owned)
            .collect()
    );
    assert_eq!(trigrams("ab"), ["ab".to_owned()].into_iter().collect());
    assert!(trigrams("").is_empty());
}

#[test]
fn test_node_search_text_includes_all_matched_fields() {
    let graph = big_graph();
    let punct = graph.nodes.iter().find(|node| node.id == "punct").unwrap();
    assert_eq!(
        node_search_text(punct, "punct")
            .split('\0')
            .collect::<Vec<_>>(),
        [
            "foo.bar:baz",
            "foo bar baz",
            "punct",
            "pkg/foobar.py",
            "pkg foobar py"
        ]
    );
}

#[test]
fn test_trigram_candidates_fast_path_fires_for_rare_term() {
    let graph = big_graph();
    let index = GraphIndex::new(&graph);
    let candidates = trigram_candidates(&index, &["zebraquokkawidget".into()]).unwrap();
    assert!(ids(&index, &candidates).contains(&"rareA".into()));
    assert!(candidates.len() < graph.nodes.len());
}

#[test]
fn test_trigram_candidates_falls_back_on_common_term() {
    let graph = big_graph();
    assert!(trigram_candidates(&GraphIndex::new(&graph), &["item".into()]).is_none());
}

#[test]
fn test_trigram_candidates_falls_back_on_short_token() {
    let graph = big_graph();
    assert!(trigram_candidates(&GraphIndex::new(&graph), &["ab".into()]).is_none());
}

#[test]
fn test_score_nodes_prefilter_is_identical_to_full_scan() {
    let graph = big_graph();
    let index = GraphIndex::new(&graph);
    for query in [
        "zebraquokkawidget",
        "marmosetgadget handler",
        "foo bar baz",
        "item",
        "node 42",
        "nonexistentxyz",
    ] {
        let terms = query_terms(query);
        assert_eq!(
            score_nodes(&index, &terms),
            score_query_full_scan(&index, &terms, false).ranked,
            "{query}"
        );
    }
}

#[test]
fn test_find_node_prefilter_is_identical_to_full_scan() {
    let graph = big_graph();
    let index = GraphIndex::new(&graph);
    for label in [
        "ZebraQuokkaWidget",
        "MarmosetGadget handler",
        "Foo Bar Baz",
        "item node 7",
        "missing",
    ] {
        assert_eq!(
            find_node(&index, label),
            find_node_full_scan(&index, label),
            "{label}"
        );
    }
}

#[test]
fn test_find_node_label_tokens_branch_covered_by_index() {
    let graph = big_graph();
    let index = GraphIndex::new(&graph);
    assert_eq!(ids(&index, &find_node(&index, "Foo Bar Baz")), ["punct"]);
}

#[test]
fn test_find_node_source_file_path_prefers_file_level_node() {
    let mut graph = big_graph();
    let source = "app/api/example/route.ts";
    graph.nodes.push(node(
        "example_route_get",
        "GET()",
        source,
        Some("L42"),
        None,
    ));
    graph
        .nodes
        .push(node("example_route", "route.ts", source, Some("L1"), None));
    let index = GraphIndex::new(&graph);
    let matches = ids(&index, &find_node(&index, source));
    assert_eq!(matches[0], "example_route");
    assert!(matches.contains(&"example_route_get".into()));
}

#[test]
fn test_trigram_index_cached_and_rebuilt_per_graph() {
    let graph = big_graph();
    let cache = std::sync::Arc::new(GraphQueryCache::default());
    let index = GraphIndex::new_with_cache(&graph, cache.clone());
    let repeated = GraphIndex::new_with_cache(&graph, cache);
    assert!(std::ptr::eq(
        index.trigram_index(),
        repeated.trigram_index()
    ));
    let graph2 = big_graph();
    let index2 = GraphIndex::new(&graph2);
    assert!(!std::ptr::eq(index.trigram_index(), index2.trigram_index()));
}

#[test]
fn test_query_terms_strips_search_punctuation() {
    assert_eq!(query_terms("what calls extract?"), ["calls", "extract"]);
}

#[test]
fn test_query_terms_drops_question_stopwords() {
    assert_eq!(
        query_terms("how does the frontier cache work"),
        ["frontier", "cache"]
    );
}

#[test]
fn test_query_terms_all_stopwords_falls_back_to_unfiltered() {
    assert_eq!(query_terms("how does it work"), ["how", "does", "work"]);
}

#[test]
fn test_query_terms_drops_german_question_stopwords() {
    assert_eq!(
        query_terms("Wie funktioniert die Authentifizierung?"),
        ["authentifizierung"]
    );
}

#[test]
fn test_query_terms_all_german_stopwords_falls_back_to_unfiltered() {
    assert_eq!(
        query_terms("wie funktioniert das"),
        ["wie", "funktioniert", "das"]
    );
}

#[test]
fn test_pick_seeds_german_query_seeds_content_node_not_heading_noise() {
    let mut graph = KnowledgeGraph {
        directed: true,
        nodes: vec![
            node(
                "cfg",
                "Die Konfiguration",
                "docs/konfiguration.md",
                None,
                None,
            ),
            node(
                "sec",
                "Wie wird gesichert",
                "docs/sicherheit.md",
                None,
                None,
            ),
            node("auth", "Authentifizierung", "src/auth.py", None, None),
            node("helper", "login_helper", "src/auth.py", None, None),
        ],
        ..KnowledgeGraph::default()
    };
    graph.links.push(edge("helper", "auth", "calls", None));
    let index = GraphIndex::new(&graph);
    let scores = score_query(
        &index,
        &query_terms("Wie funktioniert die Authentifizierung?"),
        true,
    );
    let seeds = ids(
        &index,
        &pick_seeds(
            &scores.ranked,
            3,
            0.2,
            Some(&index),
            Some(&scores.best_seed_by_term),
        ),
    );
    assert!(seeds.contains(&"auth".into()));
    assert!(!seeds.contains(&"cfg".into()));
    assert!(!seeds.contains(&"sec".into()));
}

#[test]
fn test_query_terms_filters_only_short_english_terms() {
    let segment = |text: &str| {
        let values: &[&str] = match text {
            "前端" => &["前端"],
            "依赖" => &["依赖"],
            "安装" => &["安装"],
            "包管理器" => &["包", "管理器"],
            "项目约定" => &["项目", "约定"],
            "a前" => &["a", "前"],
            _ => panic!("unexpected Chinese term {text}"),
        };
        values.iter().map(|value| (*value).to_owned()).collect()
    };
    assert_eq!(
        query_terms_with_chinese_segmenter(
            "前端 dependency 依赖 install 安装 to of 包管理器 项目约定 a前",
            Some(&segment),
        ),
        [
            "前端",
            "dependency",
            "依赖",
            "install",
            "安装",
            "包",
            "管理器",
            "包管理器",
            "项目",
            "约定",
            "项目约定",
            "前",
            "a前"
        ]
    );
}

#[test]
fn test_query_graph_text_keeps_short_non_english_terms() {
    let graph = KnowledgeGraph {
        nodes: vec![node(
            "frontend",
            "前端",
            "docs/前端.md",
            Some("L1"),
            Some(0),
        )],
        ..KnowledgeGraph::default()
    };
    let text = query_graph_text(&graph, "前端", "bfs", 1, 2000, &[]);
    assert!(!text.contains("No matching nodes found."));
    assert!(text.contains("NODE 前端"));
}

#[test]
fn test_infer_context_filters_for_calls_question() {
    assert_eq!(infer_context_filters("who calls extract"), ["call"]);
}

#[test]
fn test_resolve_context_filters_explicit_overrides_heuristic() {
    let (filters, source) = resolve_context_filters("who calls extract", &["field".into()]);
    assert_eq!(filters, ["field"]);
    assert_eq!(source, Some("explicit"));
}

#[test]
fn test_bfs_depth_1() {
    let graph = make_graph();
    let index = GraphIndex::new(&graph);
    let (visited, _) = bfs(&index, &[index.position("n1").unwrap()], 1);
    let visited = id_set(&index, &visited);
    assert!(visited.contains("n1"));
    assert!(visited.contains("n2"));
    assert!(!visited.contains("n3"));
}

#[test]
fn test_bfs_depth_2() {
    let graph = make_graph();
    let index = GraphIndex::new(&graph);
    let (visited, _) = bfs(&index, &[index.position("n1").unwrap()], 2);
    assert!(id_set(&index, &visited).contains("n3"));
}

#[test]
fn test_bfs_disconnected() {
    let graph = make_graph();
    let index = GraphIndex::new(&graph);
    let (visited, _) = bfs(&index, &[index.position("n5").unwrap()], 3);
    assert_eq!(
        id_set(&index, &visited),
        ["n5".to_owned()].into_iter().collect()
    );
}

#[test]
fn test_bfs_returns_edges() {
    let graph = make_graph();
    let index = GraphIndex::new(&graph);
    let (_, edges) = bfs(&index, &[index.position("n1").unwrap()], 1);
    assert!(edges.iter().any(|(source, target)| {
        index.node(*source).id == "n1" || index.node(*target).id == "n1"
    }));
}

#[test]
fn test_filter_graph_by_context_limits_traversal() {
    let graph = filter_graph_by_context(&make_graph(), &["call".into()]);
    let index = GraphIndex::new(&graph);
    let (visited, edges) = bfs(&index, &[index.position("n1").unwrap()], 2);
    let visited = id_set(&index, &visited);
    assert!(visited.contains("n2"));
    assert!(!visited.contains("n3"));
    assert_eq!(edges.len(), 1);
    assert_eq!(
        (
            index.node(edges[0].0).id.as_str(),
            index.node(edges[0].1).id.as_str()
        ),
        ("n1", "n2")
    );
}

#[test]
fn test_dfs_depth_1() {
    let graph = make_graph();
    let index = GraphIndex::new(&graph);
    let (visited, _) = dfs(&index, &[index.position("n1").unwrap()], 1);
    let visited = id_set(&index, &visited);
    assert!(visited.contains("n1"));
    assert!(visited.contains("n2"));
    assert!(!visited.contains("n3"));
}

#[test]
fn test_dfs_full_chain() {
    let graph = make_graph();
    let index = GraphIndex::new(&graph);
    let (visited, _) = dfs(&index, &[index.position("n1").unwrap()], 5);
    let visited = id_set(&index, &visited);
    assert!(["n1", "n2", "n3", "n4"]
        .iter()
        .all(|id| visited.contains(*id)));
}

#[test]
fn test_subgraph_to_text_contains_labels() {
    let text = render(&make_graph(), &["n1", "n2"], &[("n1", "n2")], 2000, &[]);
    assert!(text.contains("extract"));
    assert!(text.contains("cluster"));
}

#[test]
fn test_subgraph_to_text_truncates() {
    let text = render(
        &make_graph(),
        &["n1", "n2", "n3", "n4"],
        &[("n1", "n2")],
        1,
        &[],
    );
    assert!(text.contains("truncated"));
}

#[test]
fn test_subgraph_to_text_edge_included() {
    let text = render(&make_graph(), &["n1", "n2"], &[("n1", "n2")], 2000, &[]);
    assert!(text.contains("EDGE"));
    assert!(text.contains("calls"));
}

#[test]
fn test_subgraph_to_text_includes_edge_context() {
    assert!(
        render(&make_graph(), &["n1", "n2"], &[("n1", "n2")], 2000, &[]).contains("context=call")
    );
}

#[test]
fn test_subgraph_to_text_annotates_node_with_learning_status() {
    let mut graph = make_graph();
    graph.extra.insert(
        "_learning_overlay".into(),
        json!({"n1": {"status": "preferred", "stale": false}}),
    );
    let text = render(&graph, &["n1", "n2"], &[("n1", "n2")], 2000, &[]);
    let lines: HashMap<_, _> = text
        .lines()
        .filter(|line| line.starts_with("NODE "))
        .map(|line| (line.split_whitespace().nth(1).unwrap(), line))
        .collect();
    assert!(lines["extract"].contains("learning=preferred]"));
    assert!(!lines["cluster"].contains("learning="));
}

#[test]
fn test_subgraph_to_text_marks_stale_status() {
    let mut graph = make_graph();
    graph.extra.insert(
        "_learning_overlay".into(),
        json!({"n1": {"status": "contested", "stale": true}}),
    );
    assert!(render(&graph, &["n1"], &[], 2000, &[]).contains("learning=contested:stale]"));
}

#[test]
fn test_subgraph_to_text_learning_suffix_counts_against_budget() {
    let mut graph = make_graph();
    let bare = render(&graph, &["n1", "n2", "n3"], &[], 2000, &[]);
    let budget = bare.len() / 3 + 1;
    assert!(!render(&graph, &["n1", "n2", "n3"], &[], budget, &[]).contains("truncated"));
    graph.extra.insert(
        "_learning_overlay".into(),
        json!({
            "n1": {"status": "preferred", "stale": false},
            "n2": {"status": "preferred", "stale": false},
            "n3": {"status": "preferred", "stale": false}
        }),
    );
    let annotated = render(&graph, &["n1", "n2", "n3"], &[], budget, &[]);
    assert!(annotated.contains("learning=preferred"));
    assert!(annotated.contains("truncated"));
}

#[test]
fn test_subgraph_to_text_no_overlay_is_unchanged() {
    assert!(
        !render(&make_graph(), &["n1", "n2"], &[("n1", "n2")], 2000, &[]).contains("learning=")
    );
}

#[test]
fn test_query_graph_text_explicit_context_filter_changes_traversal() {
    let text = query_graph_text(&make_graph(), "extract", "bfs", 2, 2000, &["call".into()]);
    assert!(text.contains("Context: call (explicit)"));
    assert!(text.contains("cluster"));
    assert!(!text.contains("NODE build"));
}

#[test]
fn test_query_graph_text_heuristic_context_filter_changes_traversal() {
    let text = query_graph_text(&make_graph(), "who calls extract", "bfs", 2, 2000, &[]);
    assert!(text.contains("Context: call (heuristic)"));
    assert!(text.contains("cluster"));
    assert!(!text.contains("NODE build"));
}

fn write_graph(path: &std::path::Path, graph: &KnowledgeGraph) {
    std::fs::write(path, serde_json::to_vec(graph).unwrap()).unwrap();
}

#[test]
fn test_load_graph_roundtrip() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("graph.json");
    let graph = make_graph();
    write_graph(&path, &graph);
    let loaded = load_graph(&path).unwrap();
    assert_eq!(loaded.nodes.len(), graph.nodes.len());
    assert_eq!(loaded.links.len(), graph.links.len());
}

#[test]
fn test_load_graph_missing_file() {
    let temp = tempfile::tempdir().unwrap();
    let error = load_graph(&temp.path().join("nonexistent.json"))
        .unwrap_err()
        .to_string();
    assert!(error.contains("Graph file not found"));
}

#[test]
fn test_load_graph_corrupted_json_prints_recovery_message() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("graph.json");
    std::fs::write(&path, "{not valid json").unwrap();
    let error = load_graph(&path).unwrap_err().to_string();
    assert!(error.contains("graph.json is corrupted"));
    assert!(error.contains("Re-run /graphify to rebuild"));
}

#[test]
fn test_load_graph_generic_value_error_message_unchanged() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("graph.txt");
    std::fs::write(&path, "not a graph").unwrap();
    let error = load_graph(&path).unwrap_err().to_string();
    assert!(error.contains("must be a .json file"));
    assert!(!error.contains("corrupted"));
}

#[test]
fn test_load_graph_rejects_oversized_file() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("graph.json");
    write_graph(&path, &make_graph());
    let error = load_graph_with_cap(&path, 16).unwrap_err().to_string();
    assert!(error.contains("exceeds"));
    assert!(error.contains("byte cap"));
}

#[test]
fn test_load_graph_accepts_under_cap() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("graph.json");
    write_graph(&path, &make_graph());
    assert_eq!(
        load_graph_with_cap(&path, 10 * 1024 * 1024)
            .unwrap()
            .nodes
            .len(),
        5
    );
}

#[test]
fn test_maybe_reload_detects_graph_change() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("graph.json");
    let mut graph = KnowledgeGraph::default();
    graph.nodes.extend([
        node("alpha", "alpha", "", None, Some(0)),
        node("beta", "beta", "", None, Some(0)),
    ]);
    write_graph(&path, &graph);
    assert_eq!(load_graph(&path).unwrap().nodes.len(), 2);
    graph.nodes.push(node("gamma", "gamma", "", None, Some(0)));
    write_graph(&path, &graph);
    assert!(load_graph(&path)
        .unwrap()
        .nodes
        .iter()
        .any(|node| node.id == "gamma"));
}

#[test]
fn test_load_graph_cache_key_changes_with_content() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("graph.json");
    let mut graph = KnowledgeGraph {
        nodes: vec![node("a", "a", "", None, Some(0))],
        ..KnowledgeGraph::default()
    };
    write_graph(&path, &graph);
    let first = graph_file_key(&path).unwrap();
    graph.nodes.push(node("b", "b", "", None, Some(0)));
    write_graph(&path, &graph);
    assert_ne!(first, graph_file_key(&path).unwrap());
}

#[test]
fn test_idf_downweights_common_terms() {
    let graph = noisy_graph();
    let index = GraphIndex::new(&graph);
    let scored = score_nodes(&index, &["foobarservice".into(), "error".into()]);
    assert!(!scored.is_empty());
    assert_eq!(index.node(scored[0].1).id, "fbs");
}

#[test]
fn test_idf_cached_on_graph() {
    let graph = make_graph();
    let cache = std::sync::Arc::new(GraphQueryCache::default());
    let index = GraphIndex::new_with_cache(&graph, cache.clone());
    score_nodes(&index, &["extract".into()]);
    let repeated = GraphIndex::new_with_cache(&graph, cache);
    assert!(repeated.cached_idf_terms().contains(&"extract".into()));
}

#[test]
fn test_idf_new_graph_starts_fresh() {
    let first = make_graph();
    let second = make_graph();
    let first_index = GraphIndex::new(&first);
    let second_index = GraphIndex::new(&second);
    score_nodes(&first_index, &["extract".into()]);
    assert!(second_index.cached_idf_terms().is_empty());
}

#[test]
fn test_idf_rare_term_gets_high_weight() {
    let graph = make_graph();
    let index = GraphIndex::new(&graph);
    assert!(compute_idf(&index, &["extract".into()])["extract"] > 1.0);
}

#[test]
fn test_idf_common_term_gets_low_weight() {
    let graph = KnowledgeGraph {
        nodes: (0..20)
            .map(|i| {
                node(
                    &format!("n{i}"),
                    &format!("handle_{i}"),
                    &format!("f{i}.py"),
                    None,
                    None,
                )
            })
            .collect(),
        ..KnowledgeGraph::default()
    };
    let index = GraphIndex::new(&graph);
    assert!(compute_idf(&index, &["handle".into()])["handle"] < 1.0);
}

#[test]
fn test_pick_seeds_dominant_identifier_gives_one_seed() {
    assert_eq!(
        pick_seeds(&[(1000.0, 0), (1.0, 1), (0.9, 2)], 3, 0.2, None, None),
        [0]
    );
}

#[test]
fn test_pick_seeds_close_scores_keeps_multiple() {
    assert_eq!(
        pick_seeds(&[(10.0, 0), (9.0, 1), (8.5, 2)], 3, 0.2, None, None).len(),
        3
    );
}

#[test]
fn test_pick_seeds_empty() {
    assert!(pick_seeds(&[], 3, 0.2, None, None).is_empty());
}

#[test]
fn test_pick_seeds_single() {
    assert_eq!(pick_seeds(&[(5.0, 9)], 3, 0.2, None, None), [9]);
}

#[test]
fn test_pick_seeds_respects_max_k() {
    let scored = (0..10).map(|i| (10.0, i)).collect::<Vec<_>>();
    assert_eq!(pick_seeds(&scored, 3, 0.2, None, None).len(), 3);
}

#[test]
fn test_pick_seeds_without_diversity_args_is_unchanged() {
    assert_eq!(
        pick_seeds(&[(1000.0, 0), (1.0, 1), (0.9, 2)], 3, 0.2, None, None),
        [0]
    );
}

#[test]
fn test_pick_seeds_diversity_recovers_starved_term() {
    let graph = KnowledgeGraph {
        directed: true,
        nodes: vec![
            node("noise", "unrelated", "design_tokens.json", None, None),
            node("target", "rate_limit_widget", "src/widget.py", None, None),
            node("other", "something_else", "src/other.py", None, None),
        ],
        links: vec![edge("other", "target", "calls", None)],
        ..KnowledgeGraph::default()
    };
    let index = GraphIndex::new(&graph);
    let scores = score_query(&index, &["unrelated".into(), "widget".into()], true);
    assert_eq!(
        ids(&index, &pick_seeds(&scores.ranked, 3, 0.2, None, None)),
        ["noise"]
    );
    let diverse = ids(
        &index,
        &pick_seeds(
            &scores.ranked,
            3,
            0.2,
            Some(&index),
            Some(&scores.best_seed_by_term),
        ),
    );
    assert!(diverse.contains(&"noise".into()));
    assert!(diverse.contains(&"target".into()));
}

#[test]
fn test_pick_seeds_dedups_homonymous_generic_labels() {
    let mut graph = KnowledgeGraph {
        directed: true,
        ..KnowledgeGraph::default()
    };
    for i in 0..5 {
        graph.nodes.push(node(
            &format!("get{i}"),
            "GET",
            &format!("routes/r{i}.py"),
            None,
            None,
        ));
    }
    graph
        .nodes
        .push(node("um", "users_model", "models/users.py", None, None));
    let index = GraphIndex::new(&graph);
    let scored = (0..5)
        .map(|i| (1000.0, i))
        .chain([(900.0, 5)])
        .collect::<Vec<_>>();
    let seeds = ids(&index, &pick_seeds(&scored, 3, 0.2, Some(&index), None));
    assert_eq!(seeds.iter().filter(|id| id.starts_with("get")).count(), 1);
    assert!(seeds.contains(&"um".into()));
}

#[test]
fn test_pick_seeds_dedup_key_is_case_and_diacritic_normalized() {
    let graph = KnowledgeGraph {
        directed: true,
        nodes: vec![
            node("a", "GET", "a.py", None, None),
            node("b", "Get", "b.py", None, None),
            node("c", "get", "c.py", None, None),
        ],
        ..KnowledgeGraph::default()
    };
    let index = GraphIndex::new(&graph);
    assert_eq!(
        pick_seeds(
            &[(1000.0, 0), (990.0, 1), (980.0, 2)],
            3,
            0.2,
            Some(&index),
            None
        )
        .len(),
        1
    );
}

#[test]
fn test_pick_seeds_per_term_guarantee_does_not_reintroduce_generic_dupe() {
    let mut graph = KnowledgeGraph {
        directed: true,
        ..KnowledgeGraph::default()
    };
    for i in 0..3 {
        graph.nodes.push(node(
            &format!("get{i}"),
            "GET",
            &format!("r{i}.py"),
            None,
            None,
        ));
    }
    graph
        .nodes
        .push(node("um", "users_model", "users.py", None, None));
    graph.links.push(edge("um", "get0", "calls", None));
    let index = GraphIndex::new(&graph);
    let scores = score_query(&index, &["get".into(), "users".into()], true);
    let seeds = ids(
        &index,
        &pick_seeds(
            &scores.ranked,
            3,
            0.2,
            Some(&index),
            Some(&scores.best_seed_by_term),
        ),
    );
    assert_eq!(seeds.iter().filter(|id| id.starts_with("get")).count(), 1);
}

#[test]
fn test_score_nodes_scores_identical_labels_equally() {
    let graph = KnowledgeGraph {
        directed: true,
        nodes: vec![
            node("g1", "GET", "a.py", None, None),
            node("g2", "GET", "b.py", None, None),
            node("g3", "GET", "c.py", None, None),
        ],
        ..KnowledgeGraph::default()
    };
    let index = GraphIndex::new(&graph);
    let by_id: HashMap<_, _> = score_nodes(&index, &["get".into()])
        .into_iter()
        .map(|(score, position)| (index.node(position).id.as_str(), score))
        .collect();
    assert_eq!(by_id["g1"], by_id["g2"]);
    assert_eq!(by_id["g2"], by_id["g3"]);
}

#[test]
fn test_subgraph_to_text_truncation_hint_is_actionable() {
    let text = render(
        &make_graph(),
        &["n1", "n2", "n3", "n4"],
        &[("n1", "n2")],
        1,
        &[],
    );
    assert!(text.contains("truncated"));
    assert!(text.contains("get_node") || text.contains("context_filter"));
}

#[test]
fn test_query_seeds_from_identifier_not_noise() {
    let text = query_graph_text(
        &noisy_graph(),
        "FooBarService error handling",
        "bfs",
        2,
        2000,
        &[],
    );
    assert!(text.contains("FooBarService"));
    assert!(text.contains("ServiceClient"));
}

#[test]
fn test_query_graph_text_parameter_type_context_filter_changes_traversal() {
    let graph = KnowledgeGraph {
        nodes: vec![
            node("process", "process", "sample.cs", Some("L20"), None),
            node("payload", "Payload", "sample.cs", Some("L5"), None),
            node("other", "PayloadFactory", "sample.cs", Some("L40"), None),
        ],
        links: vec![
            edge("process", "payload", "references", Some("parameter_type")),
            edge("process", "other", "calls", Some("call")),
        ],
        ..KnowledgeGraph::default()
    };
    let text = query_graph_text(
        &graph,
        "who accepts Payload",
        "bfs",
        3,
        2000,
        &["parameter_type".into()],
    );
    assert!(text.contains("parameter_type"));
    assert!(text.contains("Payload"));
    assert!(!text.contains("PayloadFactory"));
}

#[test]
fn test_query_graph_text_context_filter_aliases_resolve() {
    for (alias, canonical) in [
        ("param", "parameter_type"),
        ("parameter", "parameter_type"),
        ("return", "return_type"),
        ("returns", "return_type"),
        ("generic", "generic_arg"),
        ("generics", "generic_arg"),
        ("annotation", "attribute"),
        ("decorator", "attribute"),
        ("parameter_type", "parameter_type"),
        ("field", "field"),
    ] {
        assert_eq!(normalize_context_filters(&[alias.into()]), [canonical]);
    }
}

#[test]
fn test_query_terms_chinese_segments_with_cached_jieba() {
    let segment = |text: &str| {
        assert_eq!(text, "页面路由");
        vec!["页面".into(), "路由".into()]
    };
    assert_eq!(
        query_terms_with_chinese_segmenter("页面路由", Some(&segment)),
        ["页面", "路由", "页面路由"]
    );
}

#[test]
fn test_query_terms_chinese_mixed() {
    let terms = query_terms("前端 router 路由配置");
    for term in ["前端", "router", "路由", "配置"] {
        assert!(terms.contains(&term.to_owned()));
    }
}

#[test]
fn test_query_terms_non_chinese_scripts_are_not_segmented() {
    assert!(!has_chinese("かなカナ한글"));
    assert_eq!(query_terms("かなカナ한글"), ["かなカナ한글"]);
}

#[test]
fn test_query_terms_chinese_no_jieba_fallback() {
    let terms = query_terms("页面路由");
    assert!(terms.contains(&"页面".into()));
    assert!(terms.contains(&"路由".into()));
    assert!(terms.contains(&"页面路由".into()));
    assert_eq!(terms.len(), 4);
}

#[test]
fn test_score_nodes_chinese_substring_match() {
    let graph = KnowledgeGraph {
        nodes: vec![
            node("n1", "路由桥接核对表", "doc.md", None, Some(0)),
            node("n2", "其他内容", "doc.md", None, Some(0)),
        ],
        ..KnowledgeGraph::default()
    };
    let index = GraphIndex::new(&graph);
    let matches = ids(
        &index,
        &score_nodes(&index, &["路由".into()])
            .into_iter()
            .map(|(_, position)| position)
            .collect::<Vec<_>>(),
    );
    assert!(matches.contains(&"n1".into()));
    assert!(!matches.contains(&"n2".into()));
}

#[test]
fn test_query_text_chinese_finds_routing_nodes() {
    let graph = KnowledgeGraph {
        nodes: vec![
            node("parent", "页面路由规范", "doc.md", Some("L1"), Some(0)),
            node("child", "路由桥接核对表", "doc.md", Some("L10"), Some(0)),
        ],
        links: vec![edge("parent", "child", "contains", None)],
        ..KnowledgeGraph::default()
    };
    let text = query_graph_text(&graph, "页面路由", "bfs", 2, 2000, &[]);
    assert!(!text.contains("No matching nodes found."));
    assert!(text.contains("路由"));
}

#[test]
fn test_community_header_shows_real_name() {
    assert_eq!(
        community_header(12, Some("Auth & Sessions")),
        "Community 12 — Auth & Sessions"
    );
}

#[test]
fn test_community_header_skips_placeholder_name() {
    assert_eq!(community_header(12, Some("Community 12")), "Community 12");
}

#[test]
fn test_community_header_falls_back_when_no_name() {
    assert_eq!(community_header(7, None), "Community 7");
    assert_eq!(community_header(7, Some("")), "Community 7");
}

#[test]
fn test_community_header_sanitizes_name() {
    let output = community_header(3, Some("Pay\0ments\x1b[31m"));
    assert!(output.starts_with("Community 3 — "));
    assert!(!output.contains('\0'));
    assert!(!output.contains('\x1b'));
}

const SYLLABLES: &[&str] = &[
    "foo", "bar", "baz", "get", "set", "run", "user", "name", "path", "build", "report", "extract",
    "router", "config", "service", "handler", "token", "auth", "rate", "limit", "widget", "model",
];

fn syllable_queries() -> Vec<Vec<String>> {
    vec![
        vec!["get"],
        vec!["get", "user"],
        vec!["router", "service", "handler"],
        vec!["extract", "build", "report", "path"],
        vec!["nonexistent"],
        vec!["nonexistent", "get"],
        vec!["bar", "bar"],
        vec!["baz", "run", "set", "auth", "rate", "limit"],
    ]
    .into_iter()
    .map(|terms| terms.into_iter().map(str::to_owned).collect())
    .collect()
}

#[derive(Clone)]
struct Deterministic(u64);

impl Deterministic {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0
    }

    fn range(&mut self, upper: usize) -> usize {
        (self.next() as usize) % upper
    }
}

fn random_scoring_graph(node_count: usize, seed: u64) -> KnowledgeGraph {
    let mut rng = Deterministic(seed);
    let mut graph = KnowledgeGraph {
        directed: true,
        ..KnowledgeGraph::default()
    };
    for i in 0..node_count {
        let word_count = 1 + rng.range(3);
        let mut words = Vec::new();
        while words.len() < word_count {
            let word = SYLLABLES[rng.range(SYLLABLES.len())];
            if !words.contains(&word) {
                words.push(word);
            }
        }
        let label = words.join("_");
        graph.nodes.push(node(
            &format!("n{i}"),
            &label,
            &format!("src/{}.py", &label[..label.len().min(8)]),
            None,
            None,
        ));
    }
    for _ in 0..node_count * 2 {
        let source = rng.range(node_count);
        let target = rng.range(node_count);
        if source != target {
            graph.links.push(edge(
                &format!("n{source}"),
                &format!("n{target}"),
                "calls",
                None,
            ));
        }
    }
    graph
}

fn reference_best_seed_by_term(
    index: &GraphIndex<'_>,
    terms: &[String],
) -> BTreeMap<String, usize> {
    let normalized: std::collections::BTreeSet<_> =
        terms.iter().flat_map(|term| search_tokens(term)).collect();
    let mut best = BTreeMap::new();
    for term in normalized {
        let scored = score_nodes(index, std::slice::from_ref(&term));
        let Some((best_score, _)) = scored.first() else {
            continue;
        };
        let mut tied = scored
            .iter()
            .take_while(|(score, _)| score == best_score)
            .map(|(_, position)| *position)
            .collect::<Vec<_>>();
        tied.sort_by(|left, right| {
            index
                .degree(*right)
                .cmp(&index.degree(*left))
                .then_with(|| {
                    index
                        .node(*left)
                        .label
                        .chars()
                        .count()
                        .cmp(&index.node(*right).label.chars().count())
                })
                .then_with(|| index.node(*left).id.cmp(&index.node(*right).id))
        });
        best.insert(term, tied[0]);
    }
    best
}

fn check_ranked_equivalence(terms: &[String]) {
    let graph = random_scoring_graph(80, 7);
    let index = GraphIndex::new(&graph);
    assert_eq!(
        score_query(&index, terms, false).ranked,
        score_nodes(&index, terms)
    );
}

fn check_best_seed_equivalence(terms: &[String]) {
    let graph = random_scoring_graph(80, 7);
    let index = GraphIndex::new(&graph);
    assert_eq!(
        score_query(&index, terms, true).best_seed_by_term,
        reference_best_seed_by_term(&index, terms)
    );
}

fn check_pick_seed_equivalence(terms: &[String]) {
    let graph = random_scoring_graph(80, 7);
    let index = GraphIndex::new(&graph);
    let scores = score_query(&index, terms, true);
    let reference = reference_best_seed_by_term(&index, terms);
    let optimized_seeds = pick_seeds(
        &scores.ranked,
        3,
        0.2,
        Some(&index),
        Some(&scores.best_seed_by_term),
    );
    let reference_seeds = pick_seeds(&scores.ranked, 3, 0.2, Some(&index), Some(&reference));
    assert_eq!(optimized_seeds, reference_seeds);
    for winner in reference.values() {
        if reference_seeds.contains(winner) {
            continue;
        }
        let label = index.node(*winner).label.to_lowercase();
        assert!(reference_seeds
            .iter()
            .any(|seed| index.node(*seed).label.to_lowercase() == label));
    }
}

macro_rules! parameter_case {
    ($name:ident, $check:ident, $index:expr) => {
        #[test]
        fn $name() {
            $check(&syllable_queries()[$index]);
        }
    };
}

parameter_case!(
    test_score_query_ranked_matches_score_nodes_byte_identical_terms0,
    check_ranked_equivalence,
    0
);
parameter_case!(
    test_score_query_ranked_matches_score_nodes_byte_identical_terms1,
    check_ranked_equivalence,
    1
);
parameter_case!(
    test_score_query_ranked_matches_score_nodes_byte_identical_terms2,
    check_ranked_equivalence,
    2
);
parameter_case!(
    test_score_query_ranked_matches_score_nodes_byte_identical_terms3,
    check_ranked_equivalence,
    3
);
parameter_case!(
    test_score_query_ranked_matches_score_nodes_byte_identical_terms4,
    check_ranked_equivalence,
    4
);
parameter_case!(
    test_score_query_ranked_matches_score_nodes_byte_identical_terms5,
    check_ranked_equivalence,
    5
);
parameter_case!(
    test_score_query_ranked_matches_score_nodes_byte_identical_terms6,
    check_ranked_equivalence,
    6
);
parameter_case!(
    test_score_query_ranked_matches_score_nodes_byte_identical_terms7,
    check_ranked_equivalence,
    7
);

parameter_case!(
    test_score_query_best_seed_by_term_matches_legacy_singleton_scoring_terms0,
    check_best_seed_equivalence,
    0
);
parameter_case!(
    test_score_query_best_seed_by_term_matches_legacy_singleton_scoring_terms1,
    check_best_seed_equivalence,
    1
);
parameter_case!(
    test_score_query_best_seed_by_term_matches_legacy_singleton_scoring_terms2,
    check_best_seed_equivalence,
    2
);
parameter_case!(
    test_score_query_best_seed_by_term_matches_legacy_singleton_scoring_terms3,
    check_best_seed_equivalence,
    3
);
parameter_case!(
    test_score_query_best_seed_by_term_matches_legacy_singleton_scoring_terms4,
    check_best_seed_equivalence,
    4
);
parameter_case!(
    test_score_query_best_seed_by_term_matches_legacy_singleton_scoring_terms5,
    check_best_seed_equivalence,
    5
);
parameter_case!(
    test_score_query_best_seed_by_term_matches_legacy_singleton_scoring_terms6,
    check_best_seed_equivalence,
    6
);
parameter_case!(
    test_score_query_best_seed_by_term_matches_legacy_singleton_scoring_terms7,
    check_best_seed_equivalence,
    7
);

parameter_case!(
    test_pick_seeds_with_optimized_best_seed_matches_legacy_semantics_terms0,
    check_pick_seed_equivalence,
    0
);
parameter_case!(
    test_pick_seeds_with_optimized_best_seed_matches_legacy_semantics_terms1,
    check_pick_seed_equivalence,
    1
);
parameter_case!(
    test_pick_seeds_with_optimized_best_seed_matches_legacy_semantics_terms2,
    check_pick_seed_equivalence,
    2
);
parameter_case!(
    test_pick_seeds_with_optimized_best_seed_matches_legacy_semantics_terms3,
    check_pick_seed_equivalence,
    3
);
parameter_case!(
    test_pick_seeds_with_optimized_best_seed_matches_legacy_semantics_terms4,
    check_pick_seed_equivalence,
    4
);
parameter_case!(
    test_pick_seeds_with_optimized_best_seed_matches_legacy_semantics_terms5,
    check_pick_seed_equivalence,
    5
);
parameter_case!(
    test_pick_seeds_with_optimized_best_seed_matches_legacy_semantics_terms6,
    check_pick_seed_equivalence,
    6
);
parameter_case!(
    test_pick_seeds_with_optimized_best_seed_matches_legacy_semantics_terms7,
    check_pick_seed_equivalence,
    7
);

#[test]
fn test_score_query_matches_legacy_across_random_deterministic_graphs() {
    let mut rng = Deterministic(42);
    for trial in 0..30 {
        let node_count = 20 + rng.range(181);
        let graph = random_scoring_graph(node_count, rng.next());
        let index = GraphIndex::new(&graph);
        let terms = (0..1 + rng.range(5))
            .map(|_| SYLLABLES[rng.range(SYLLABLES.len())].to_owned())
            .collect::<Vec<_>>();
        let reference = reference_best_seed_by_term(&index, &terms);
        let optimized = score_query(&index, &terms, true);
        assert_eq!(
            optimized.ranked,
            score_nodes(&index, &terms),
            "trial {trial}"
        );
        assert_eq!(optimized.best_seed_by_term, reference, "trial {trial}");
        assert_eq!(
            pick_seeds(
                &optimized.ranked,
                3,
                0.2,
                Some(&index),
                Some(&optimized.best_seed_by_term)
            ),
            pick_seeds(&optimized.ranked, 3, 0.2, Some(&index), Some(&reference)),
            "trial {trial}"
        );
    }
}

#[test]
fn test_score_query_matches_legacy_under_full_scan_fallback() {
    let terms = vec!["router".into(), "service".into(), "handler".into()];
    let graph = random_scoring_graph(80, 19);
    let index = GraphIndex::new(&graph);
    let optimized = score_query_full_scan(&index, &terms, true);
    assert_eq!(optimized.ranked, score_nodes(&index, &terms));
    assert_eq!(
        optimized.best_seed_by_term,
        reference_best_seed_by_term(&index, &terms)
    );
}

#[test]
fn test_query_graph_text_makes_exactly_one_score_query_call() {
    let graph = random_scoring_graph(60, 23);
    for query in [
        "foo",
        "foo bar",
        "router service handler",
        "get user run name path",
        "extract build report router config service token rate limit widget",
    ] {
        let mut calls = 0;
        query_graph_text_with_score_observer(&graph, query, "bfs", 1, 2000, &[], &mut || {
            calls += 1
        });
        assert_eq!(calls, 1, "{query}");
    }
}

#[test]
fn test_score_query_collect_per_term_seeds_false_omits_tracking() {
    let graph = random_scoring_graph(50, 29);
    let index = GraphIndex::new(&graph);
    let terms = vec!["foo".into(), "bar".into(), "baz".into()];
    let scores = score_query(&index, &terms, false);
    assert!(scores.best_seed_by_term.is_empty());
    assert_eq!(scores.ranked, score_nodes(&index, &terms));
}

fn star_graph() -> KnowledgeGraph {
    let mut graph = KnowledgeGraph {
        nodes: vec![node("hub", "Hub", "hub.py", Some("L1"), Some(0))],
        ..KnowledgeGraph::default()
    };
    for i in 0..40 {
        graph.nodes.push(node(
            &format!("s{i}"),
            &format!("spoke{i}"),
            &format!("s{i}.py"),
            Some("L1"),
            Some(0),
        ));
        graph
            .links
            .push(edge("hub", &format!("s{i}"), "calls", None));
    }
    graph.nodes.push(node(
        "answer",
        "CompanySpacingGate",
        "gate.py",
        Some("L12"),
        Some(0),
    ));
    graph.links.push(edge("s0", "answer", "calls", None));
    graph
}

#[test]
fn test_subgraph_to_text_seed_survives_truncation() {
    let graph = star_graph();
    let node_ids = graph
        .nodes
        .iter()
        .map(|node| node.id.as_str())
        .collect::<Vec<_>>();
    let edge_ids = graph
        .links
        .iter()
        .map(|edge| (edge.source.as_str(), edge.target.as_str()))
        .collect::<Vec<_>>();
    let text = render(&graph, &node_ids, &edge_ids, 30, &["answer"]);
    assert!(text.contains("CompanySpacingGate"));
    let first_node = text.lines().find(|line| line.starts_with("NODE ")).unwrap();
    assert!(first_node.contains("CompanySpacingGate"));
    assert!(text.contains("TRUNCATED"));
}

#[test]
fn test_query_graph_text_passes_seeds_so_answer_survives() {
    let text = query_graph_text(&star_graph(), "CompanySpacingGate", "bfs", 2, 40, &[]);
    let body = text
        .split_once("\n\n")
        .map(|(_, body)| body)
        .unwrap_or(&text);
    assert!(body.contains("CompanySpacingGate"));
}

#[test]
fn test_subgraph_to_text_truncation_notice_at_top() {
    let graph = star_graph();
    let node_ids = graph
        .nodes
        .iter()
        .map(|node| node.id.as_str())
        .collect::<Vec<_>>();
    let edge_ids = graph
        .links
        .iter()
        .map(|edge| (edge.source.as_str(), edge.target.as_str()))
        .collect::<Vec<_>>();
    let text = render(&graph, &node_ids, &edge_ids, 30, &["answer"]);
    assert!(text.starts_with("[!] TRUNCATED"));
    assert!(text.lines().next().unwrap().contains("of"));
    assert!(text.lines().next().unwrap().contains("nodes"));
    assert!(text.contains("truncated"));
}

#[test]
fn test_subgraph_to_text_no_notice_when_under_budget() {
    let text = render(&make_graph(), &["n1", "n2"], &[("n1", "n2")], 2000, &[]);
    assert!(!text.contains("TRUNCATED"));
    assert!(!text.contains("truncated"));
}

#[test]
fn test_subgraph_to_text_order_is_deterministic() {
    let graph = KnowledgeGraph {
        nodes: (0..10)
            .map(|i| {
                node(
                    &format!("z{i}"),
                    &format!("z{i}"),
                    &format!("z{i}.py"),
                    Some("L1"),
                    Some(0),
                )
            })
            .collect(),
        ..KnowledgeGraph::default()
    };
    let index = GraphIndex::new(&graph);
    let first: HashSet<_> = (0..10).collect();
    let second: HashSet<_> = (0..10).rev().collect();
    assert_eq!(
        subgraph_to_text(&index, &first, &[], 2000, &[]),
        subgraph_to_text(&index, &second, &[], 2000, &[])
    );
}

#[test]
fn test_cut_lines_to_budget_under_budget_is_byte_identical() {
    let lines = vec![
        "Neighbors of X:".into(),
        "  --> a [calls] [EXTRACTED]".into(),
        "  --> b [calls] [EXTRACTED]".into(),
    ];
    let output = cut_lines_to_budget(&lines, 2000, "use relation_filter");
    assert_eq!(output, lines.join("\n"));
    assert!(!output.contains("TRUNCATED"));
    assert!(!output.contains("truncated"));
}

#[test]
fn test_cut_lines_to_budget_over_budget_announces_at_top() {
    let lines = (0..200)
        .map(|i| format!("  --> node{i} [calls] [EXTRACTED]"))
        .collect::<Vec<_>>();
    let output = cut_lines_to_budget(&lines, 20, "use get_node for a specific symbol");
    assert!(output.starts_with("[!] TRUNCATED: showing "));
    let first = output.lines().next().unwrap();
    assert!(first.contains("of 200 lines"));
    assert!(output.contains("use get_node for a specific symbol"));
    assert!(output.contains("truncated"));
    let shown: usize = first
        .split_once("showing ")
        .unwrap()
        .1
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let body = output
        .split_once("\n\n")
        .unwrap()
        .1
        .split_once("\n... (truncated")
        .unwrap()
        .0;
    assert_eq!(body.lines().count(), shown);
}

#[test]
fn test_subgraph_to_text_ignores_dangling_src_tgt() {
    let mut relationship = edge("a", "b", "calls", None);
    relationship.extra.insert("_src".into(), json!("ghost"));
    relationship.extra.insert("_tgt".into(), json!("b"));
    let graph = KnowledgeGraph {
        nodes: vec![
            node("a", "Alpha", "a.py", Some("L1"), Some(0)),
            node("b", "Beta", "b.py", Some("L2"), Some(0)),
        ],
        links: vec![relationship],
        ..KnowledgeGraph::default()
    };
    let output = render(&graph, &["a", "b"], &[("a", "b")], 2000, &[]);
    assert!(output.contains("EDGE"));
    assert!(output.contains("Alpha"));
    assert!(output.contains("Beta"));
}

#[test]
fn test_subgraph_to_text_honors_valid_src_tgt_direction() {
    let mut relationship = edge("callee", "caller", "calls", None);
    relationship.extra.insert("_src".into(), json!("caller"));
    relationship.extra.insert("_tgt".into(), json!("callee"));
    let graph = KnowledgeGraph {
        nodes: vec![
            node("caller", "caller", "c.py", Some("L1"), Some(0)),
            node("callee", "callee", "d.py", Some("L2"), Some(0)),
        ],
        links: vec![relationship],
        ..KnowledgeGraph::default()
    };
    let output = render(
        &graph,
        &["caller", "callee"],
        &[("callee", "caller")],
        2000,
        &[],
    );
    let edge_line = output
        .lines()
        .find(|line| line.starts_with("EDGE"))
        .unwrap();
    assert!(edge_line.contains("caller --calls"));
    assert!(edge_line.contains("--> callee"));
}
