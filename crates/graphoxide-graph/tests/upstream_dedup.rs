//! Behavioral port of pinned `tests/test_dedup.py` (65 collected cases).

use graphoxide_core::{Confidence, Edge, Extraction, Node};
use graphoxide_graph::{
    build_graph, deduplicate_entities, defines_id, is_variant_pair, label_entropy,
    normalized_label, numeric_tokens_differ, shingles, short_label_blocked, DedupDiagnosticLevel,
};
use rapidfuzz::distance::jaro_winkler;
use std::collections::{BTreeMap, BTreeSet};

fn node(id: &str, label: &str, file_type: &str, source_file: &str) -> Node {
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

fn concept(id: &str, label: &str, source_file: &str) -> Node {
    node(id, label, "concept", source_file)
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

fn run(nodes: &[Node], edges: &[Edge]) -> (Vec<Node>, Vec<Edge>) {
    let (nodes, edges, _) = deduplicate_entities(nodes, edges, &BTreeMap::new()).unwrap();
    (nodes, edges)
}

fn permutations<T: Clone>(values: &[T]) -> Vec<Vec<T>> {
    fn visit<T: Clone>(remaining: Vec<T>, prefix: Vec<T>, out: &mut Vec<Vec<T>>) {
        if remaining.is_empty() {
            out.push(prefix);
            return;
        }
        for index in 0..remaining.len() {
            let mut next_remaining = remaining.clone();
            let value = next_remaining.remove(index);
            let mut next_prefix = prefix.clone();
            next_prefix.push(value);
            visit(next_remaining, next_prefix, out);
        }
    }
    let mut out = Vec::new();
    visit(values.to_vec(), Vec::new(), &mut out);
    out
}

#[test]
fn test_entropy_short_label_low() {
    assert!(label_entropy("AI") < 2.5);
}

#[test]
fn test_entropy_normal_label_high() {
    assert!(label_entropy("AuthenticationManager") >= 2.5);
}

#[test]
fn test_entropy_empty_string() {
    assert_eq!(label_entropy(""), 0.0);
}

#[test]
fn test_shingles_produces_trigrams() {
    let values = shingles("hello", 3);
    assert!(values.contains("hel"));
    assert!(values.contains("ell"));
    assert!(values.contains("llo"));
}

#[test]
fn test_shingles_short_string() {
    assert_eq!(shingles("ab", 3), BTreeSet::from(["ab".to_owned()]));
}

#[test]
fn test_exact_duplicates_merged() {
    let nodes = [
        concept("userservice", "UserService", "test.md"),
        concept("userservice", "userservice", "test.md"),
        concept("user_service", "User Service", "test.md"),
    ];
    assert_eq!(run(&nodes, &[]).0.len(), 1);
}

#[test]
fn test_typo_merged() {
    let nodes = [
        concept("graphextractor", "GraphExtractor", "test.md"),
        concept("graph_extractor", "Graph Extractor", "test.md"),
    ];
    assert_eq!(run(&nodes, &[]).0.len(), 1);
}

#[test]
fn test_unrelated_not_merged() {
    let nodes = [
        concept("user", "UserService", "test.md"),
        concept("order", "OrderService", "test.md"),
    ];
    assert_eq!(run(&nodes, &[]).0.len(), 2);
}

#[test]
fn test_short_low_entropy_not_merged() {
    let nodes = [
        concept("ai", "AI", "test.md"),
        concept("ml", "ML", "test.md"),
    ];
    assert_eq!(run(&nodes, &[]).0.len(), 2);
}

#[test]
fn test_edges_rewired_after_merge() {
    let nodes = [
        concept("graphextractor", "GraphExtractor", "test.md"),
        concept("graph_extractor", "Graph Extractor", "test.md"),
        concept("parser", "Parser", "test.md"),
    ];
    let (_, edges) = run(&nodes, &[edge("graph_extractor", "parser", "uses")]);
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].source, "graphextractor");
}

#[test]
fn test_self_loops_dropped_after_merge() {
    let nodes = [
        concept("graphextractor", "GraphExtractor", "test.md"),
        concept("graph_extractor", "Graph Extractor", "test.md"),
    ];
    assert!(
        run(&nodes, &[edge("graphextractor", "graph_extractor", "same")])
            .1
            .is_empty()
    );
}

#[test]
fn test_community_boost_aids_merge() {
    let nodes = [
        concept("authmanager", "AuthManager", "test.md"),
        concept("auth_manager", "Auth Manager", "test.md"),
    ];
    let same = BTreeMap::from([("authmanager".into(), 1), ("auth_manager".into(), 1)]);
    let different = BTreeMap::from([("authmanager".into(), 1), ("auth_manager".into(), 2)]);
    let with = deduplicate_entities(&nodes, &[], &same).unwrap().0;
    let without = deduplicate_entities(&nodes, &[], &different).unwrap().0;
    assert!(with.len() <= without.len());
}

#[test]
fn test_empty_inputs() {
    let (nodes, edges, _) =
        deduplicate_entities(&[], &[], &BTreeMap::new()).expect("empty input is valid");
    assert!(nodes.is_empty());
    assert!(edges.is_empty());
}

#[test]
fn test_single_node_no_crash() {
    assert_eq!(
        run(&[concept("user", "UserService", "test.md")], &[])
            .0
            .len(),
        1
    );
}

#[test]
fn test_dedup_llm_flag_accepted() {
    // Rust exposes deterministic local dedup only; absence of an optional LLM
    // backend is represented by this default call and must remain valid.
    let nodes = [
        concept("user", "UserService", "test.md"),
        concept("order", "OrderService", "test.md"),
    ];
    assert_eq!(run(&nodes, &[]).0.len(), 2);
}

#[test]
fn test_build_calls_dedup() {
    let graph = build_graph(&[
        Extraction {
            nodes: vec![concept("graphextractor", "GraphExtractor", "a.py")],
            ..Extraction::default()
        },
        Extraction {
            nodes: vec![concept("graph_extractor", "Graph Extractor", "b.py")],
            ..Extraction::default()
        },
    ])
    .unwrap();
    assert_eq!(graph.nodes.len(), 1);
}

#[test]
fn test_build_dedup_preserves_semantic_attributes() {
    let mut ast = node("src_auth_login", "login", "code", "src/auth.py");
    ast.source_location = Some("L42".into());
    ast.extra.insert("_origin".into(), "ast".into());
    let mut semantic = node(
        "src_auth_login",
        "User login handler",
        "code",
        "src/auth.py",
    );
    semantic
        .extra
        .insert("summary".into(), "Authenticates a user.".into());
    semantic
        .extra
        .insert("confidence_score".into(), serde_json::json!(0.9));
    let graph = build_graph(&[
        Extraction {
            nodes: vec![ast],
            ..Extraction::default()
        },
        Extraction {
            nodes: vec![semantic],
            ..Extraction::default()
        },
    ])
    .unwrap();
    let node = &graph.nodes[0];
    assert_eq!(node.label, "login");
    assert_eq!(node.source_location.as_deref(), Some("L42"));
    assert_eq!(node.extra["summary"], "Authenticates a user.");
    assert_eq!(node.extra["confidence_score"], 0.9);
}

#[test]
fn test_dedup_does_not_merge_numeric_variants() {
    let nodes = [
        concept("asr1603", "ASR1603", "test.md"),
        concept("asr1605", "ASR1605", "test.md"),
    ];
    assert_eq!(run(&nodes, &[]).0.len(), 2);
}

#[test]
fn test_dedup_does_not_merge_short_insertion_variants() {
    let nodes = [
        concept("cranel", "cranel", "test.md"),
        concept("cranelr", "cranelr", "test.md"),
    ];
    assert_eq!(run(&nodes, &[]).0.len(), 2);
}

#[test]
fn test_dedup_does_not_merge_model_with_suffix() {
    let nodes = [
        concept("m1", "M1", "test.md"),
        concept("m1_pro", "M1 Pro", "test.md"),
    ];
    assert_eq!(run(&nodes, &[]).0.len(), 2);
}

#[test]
fn test_dedup_still_merges_real_typos() {
    let left = "graphextractor";
    let right = "graphextractar";
    let score = jaro_winkler::similarity(left.chars(), right.chars());
    assert!(!is_variant_pair(left, right));
    assert!(!short_label_blocked(left, right, score));
}

#[test]
fn test_variant_pair_helper() {
    assert!(is_variant_pair("asr1603", "asr1605"));
    assert!(is_variant_pair("cortex a55", "cortex a55x"));
    assert!(!is_variant_pair("graphextractor", "graphextracter"));
    assert!(!is_variant_pair("foo", "foo"));
}

#[test]
fn test_prefix_extension_symbols_not_merged() {
    for (left, right) in [
        ("getActiveSession", "getActiveSessions"),
        ("parseConfig", "parseConfigFile"),
        ("load", "loadAll"),
        ("handleRequest", "handleRequestTimeout"),
    ] {
        let nodes = [
            concept("left", left, "api.py"),
            concept("right", right, "api.py"),
        ];
        assert_eq!(run(&nodes, &[]).0.len(), 2, "{left} / {right}");
    }
}

#[test]
fn test_pass2_winner_union_does_not_pull_in_uncompared_same_label_nodes() {
    let nodes = [
        node("session_manager_auth", "Session Manager", "", "auth.md"),
        node("sm", "Session Manager", "", "billing.md"),
        node("session_managr_notes", "Session Managr", "", "notes.md"),
    ];
    let out = run(&nodes, &[]).0;
    assert_eq!(out.len(), 2);
    assert!(out.iter().any(|node| node.id == "sm"));
}

#[test]
fn test_prefix_guard_does_not_block_same_length_typos() {
    let left = normalized_label("GraphExtractor");
    let right = normalized_label("GraphExtractar");
    assert_eq!(left.chars().count(), right.chars().count());
    assert!(!(right.starts_with(&left) && right != left));
}

#[test]
fn test_prefix_guard_fires_for_extension_pairs() {
    for (left, right) in [
        ("getActiveSession", "getActiveSessions"),
        ("parseConfig", "parseConfigFile"),
        ("load", "loadAll"),
    ] {
        let mut values = [normalized_label(left), normalized_label(right)];
        values.sort_by_key(String::len);
        assert!(values[1].starts_with(&values[0]) && values[0] != values[1]);
    }
}

#[test]
fn test_numeric_tokens_differ_helper() {
    assert!(numeric_tokens_differ(
        "adr 0011 d5 pipeline placement",
        "adr 0013 d4 pipeline placement"
    ));
    assert!(numeric_tokens_differ(
        "3 1 product goals",
        "1 1 product goals"
    ));
    assert!(numeric_tokens_differ("code block3", "code block13"));
    assert!(!numeric_tokens_differ(
        "phase 09 overview",
        "phase 9 overview"
    ));
    assert!(!numeric_tokens_differ(
        "module layout wave 3",
        "module layouts wave 3"
    ));
    assert!(!numeric_tokens_differ("graph extractor", "graph extractar"));
}

#[test]
fn test_dedup_does_not_merge_numbered_siblings() {
    let nodes = [
        node(
            "n1",
            "Pipeline placement — 4 call sites (ADR 0013 D4)",
            "document",
            "docs/index-activity.md",
        ),
        node(
            "n2",
            "Pipeline placement — 4 call sites (ADR 0011 §D5)",
            "document",
            "docs/schema-matcher.md",
        ),
    ];
    assert_eq!(run(&nodes, &[]).0.len(), 2);
}

#[test]
fn test_dedup_does_not_merge_crossfile_rationale_boilerplate() {
    let nodes = [
        node(
            "r1",
            "Django app config for apps.platform.cards. No business logic here. Domain services live in services.py and adapters in providers.",
            "rationale",
            "apps/platform/cards/apps.py",
        ),
        node(
            "r2",
            "Django app config for apps.platform.cores. No business logic here. Domain services live in services.py and adapters in providers.",
            "rationale",
            "apps/platform/cores/apps.py",
        ),
    ];
    assert_eq!(run(&nodes, &[]).0.len(), 2);
}

#[test]
fn test_dedup_does_not_merge_crossfile_document_headings() {
    let nodes = [
        node(
            "d1",
            "Getting Started Installation Guide",
            "document",
            "docs/a.md",
        ),
        node(
            "d2",
            "Getting Started Installation Setup",
            "document",
            "docs/b.md",
        ),
    ];
    assert_eq!(run(&nodes, &[]).0.len(), 2);
}

#[test]
fn test_dedup_still_merges_samefile_rationale_duplicates() {
    let nodes = [
        node(
            "r1",
            "Counts-only metrics export, a read-only aggregation service.",
            "rationale",
            "apps/schemas/metrics.py",
        ),
        node(
            "r2",
            "Counts-only metrics export, the read-only aggregation service.",
            "rationale",
            "apps/schemas/metrics.py",
        ),
    ];
    assert_eq!(run(&nodes, &[]).0.len(), 1);
}

#[test]
fn test_dedup_does_not_merge_crossfile_shared_prefix_divergence() {
    let nodes = [
        concept("p1", "testing library jest native", "pkg-a/package.json"),
        concept("p2", "testing library react native", "pkg-b/package.json"),
    ];
    assert_eq!(run(&nodes, &[]).0.len(), 2);
}

#[test]
fn test_dedup_still_merges_crossfile_true_duplicates() {
    let nodes = [
        concept("g1", "GraphExtractor", "a.md"),
        concept("g2", "Graph Extractor", "b.md"),
    ];
    assert_eq!(run(&nodes, &[]).0.len(), 1);
}

#[test]
fn test_cross_chunk_id_collision_emits_warning() {
    let nodes = [
        concept(
            "readme_booking_service",
            "Booking Service",
            "module-a/README.md",
        ),
        concept(
            "readme_booking_service",
            "Booking Service",
            "module-b/README.md",
        ),
    ];
    let (out, _, report) =
        deduplicate_entities(&nodes, &[], &BTreeMap::new()).expect("valid dedup");
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].source_file, "module-a/README.md");
    assert!(report.diagnostics.iter().any(|diagnostic| {
        diagnostic.level == DedupDiagnosticLevel::Warning
            && diagnostic.node_id == "readme_booking_service"
            && diagnostic.kept_source_file == "module-a/README.md"
            && diagnostic.dropped_source_file == "module-b/README.md"
    }));
}

#[test]
fn test_same_id_same_source_file_no_warning() {
    let nodes = [
        concept(
            "readme_booking_service",
            "Booking Service",
            "module-a/README.md",
        ),
        concept(
            "readme_booking_service",
            "Booking Service (dupe)",
            "module-a/README.md",
        ),
    ];
    let report = deduplicate_entities(&nodes, &[], &BTreeMap::new())
        .unwrap()
        .2;
    assert!(!report
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.level == DedupDiagnosticLevel::Warning));
}

#[test]
fn test_dedup_summary_prints_fuzzy_count_when_no_exact_merges() {
    let nodes = [
        concept("g1", "GraphExtractor", "a.md"),
        concept("g2", "Graph Extractor", "b.md"),
    ];
    let report = deduplicate_entities(&nodes, &[], &BTreeMap::new())
        .unwrap()
        .2;
    assert_eq!(report.exact_merges, 0);
    assert_eq!(report.fuzzy_merges, 1);
}

#[test]
fn test_dedup_summary_still_reports_exact_only() {
    let nodes = [
        concept("u1", "User Service", "svc.md"),
        concept("u2", "user service", "svc.md"),
    ];
    let report = deduplicate_entities(&nodes, &[], &BTreeMap::new())
        .unwrap()
        .2;
    assert_eq!(report.exact_merges, 1);
    assert_eq!(report.fuzzy_merges, 0);
}

fn defining_node() -> Node {
    concept(
        "agents_make_batch_fixtures_make_batch_fixtures",
        "make-batch-fixtures agent",
        "agents/make-batch-fixtures.md",
    )
}

fn referencing_node() -> Node {
    concept(
        "agents_make_batch_fixtures_make_batch_fixtures",
        "make-batch-fixtures",
        "available/diagnose-issue/SKILL.md",
    )
}

fn assert_definer_wins(nodes: &[Node]) {
    let out = run(nodes, &[]).0;
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].source_file, "agents/make-batch-fixtures.md");
    assert_eq!(out[0].label, "make-batch-fixtures agent");
}

#[test]
fn test_defining_file_wins_over_referencing_file_definition_first() {
    assert_definer_wins(&[defining_node(), referencing_node()]);
}

#[test]
fn test_defining_file_wins_over_referencing_file_reference_first() {
    assert_definer_wins(&[referencing_node(), defining_node()]);
}

#[test]
fn test_reference_collision_is_silent() {
    let mut reference = referencing_node();
    reference
        .extra
        .insert("summary".into(), "Reference-local description.".into());
    let edges = [edge(
        "agents_make_batch_fixtures_make_batch_fixtures",
        "other",
        "relates_to",
    )];
    let (nodes, out_edges, report) =
        deduplicate_entities(&[defining_node(), reference], &edges, &BTreeMap::new()).unwrap();
    assert_eq!(nodes.len(), 1);
    assert_eq!(out_edges.len(), 1);
    assert!(!nodes[0].extra.contains_key("summary"));
    assert!(report.diagnostics.is_empty());
}

#[test]
fn test_absolute_source_path_still_defines_id() {
    let mut defining = defining_node();
    defining.source_file = "/home/u/proj/agents/make-batch-fixtures.md".into();
    let (nodes, _, report) =
        deduplicate_entities(&[referencing_node(), defining], &[], &BTreeMap::new()).unwrap();
    assert_eq!(nodes[0].label, "make-batch-fixtures agent");
    assert!(!report
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.level == DedupDiagnosticLevel::Warning));
}

#[test]
fn test_same_file_relabel_is_noted() {
    let nodes = [
        defining_node(),
        concept(
            "agents_make_batch_fixtures_make_batch_fixtures",
            "make-batch-fixtures helper agent",
            "agents/make-batch-fixtures.md",
        ),
    ];
    let report = deduplicate_entities(&nodes, &[], &BTreeMap::new())
        .unwrap()
        .2;
    assert!(report.diagnostics.iter().any(|diagnostic| {
        diagnostic.level == DedupDiagnosticLevel::Note
            && diagnostic.dropped_label == "make-batch-fixtures helper agent"
    }));
    assert!(!report
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.level == DedupDiagnosticLevel::Warning));
}

fn ast_semantic_nodes(ast_first: bool) -> Vec<Node> {
    let mut ast = node("src_auth_login", "login", "code", "src/auth.py");
    ast.source_location = Some("L42".into());
    ast.extra.insert("_origin".into(), "ast".into());
    let mut semantic = node(
        "src_auth_login",
        "User login handler",
        "code",
        "src/auth.py",
    );
    semantic
        .extra
        .insert("summary".into(), "Authenticates a user.".into());
    semantic
        .extra
        .insert("confidence_score".into(), serde_json::json!(0.9));
    if ast_first {
        vec![ast, semantic]
    } else {
        vec![semantic, ast]
    }
}

fn assert_complementary_attributes(ast_first: bool) {
    let nodes = run(&ast_semantic_nodes(ast_first), &[]).0;
    assert_eq!(nodes.len(), 1);
    let survivor = &nodes[0];
    assert_eq!(survivor.label, "login");
    assert_eq!(survivor.source_location.as_deref(), Some("L42"));
    assert_eq!(survivor.extra["_origin"], "ast");
    assert_eq!(survivor.extra["summary"], "Authenticates a user.");
    assert_eq!(survivor.extra["confidence_score"], 0.9);
}

#[test]
fn test_same_id_same_entity_retains_complementary_attributes_ast_first() {
    assert_complementary_attributes(true);
}

#[test]
fn test_same_id_same_entity_retains_complementary_attributes_semantic_first() {
    assert_complementary_attributes(false);
}

#[test]
fn test_cross_file_id_collision_does_not_mix_attributes() {
    let mut first = node("pkg_service_run", "run", "code", "pkg/service.py");
    first.source_location = Some("L10".into());
    let mut second = node("pkg_service_run", "run helper", "code", "pkg_service.py");
    second
        .extra
        .insert("summary".into(), "Different function.".into());
    let (nodes, _, report) = deduplicate_entities(&[first, second], &[], &BTreeMap::new()).unwrap();
    assert_eq!(nodes[0].source_file, "pkg/service.py");
    assert!(!nodes[0].extra.contains_key("summary"));
    assert!(report
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.level == DedupDiagnosticLevel::Warning));
}

#[test]
fn test_collision_survivor_is_order_independent() {
    let mut relabel = defining_node();
    relabel.label = "make-batch-fixtures helper agent".into();
    let base = [defining_node(), relabel, referencing_node()];
    let mut survivors = BTreeSet::new();
    for values in permutations(&base) {
        let survivor = run(&values, &[]).0.remove(0);
        survivors.insert((survivor.source_file, survivor.label));
    }
    assert_eq!(
        survivors,
        BTreeSet::from([(
            "agents/make-batch-fixtures.md".into(),
            "make-batch-fixtures agent".into()
        )])
    );
}

#[test]
fn test_bare_file_node_defines_its_own_id() {
    assert!(defines_id(&concept(
        "agents_make_batch_fixtures",
        "make-batch-fixtures.md",
        "agents/make-batch-fixtures.md"
    )));
}

#[test]
fn test_defines_id_helper() {
    assert!(defines_id(&defining_node()));
    assert!(!defines_id(&referencing_node()));
    assert!(defines_id(&concept(
        "readme_booking_service",
        "Booking Service",
        "module-a/README.md"
    )));
    assert!(!defines_id(&concept("agents_foo", "foo", "agent/foo.md")));
    assert!(!defines_id(&concept("docs_intro_foo", "foo", "")));
}

#[test]
fn test_dedup_gapfill_is_order_independent_with_multiple_losers() {
    let mut first = node("f", "f", "code", "m.py");
    first.source_location = Some("L1".into());
    let mut beta = node("f", "f helper beta", "code", "m.py");
    beta.extra.insert("summary".into(), "BETA".into());
    let mut alpha = node("f", "f helper alpha", "code", "m.py");
    alpha.extra.insert("summary".into(), "ALPHA".into());
    let mut summaries = BTreeSet::new();
    for values in permutations(&[first, beta, alpha]) {
        let output = run(&values, &[]).0;
        summaries.insert(output[0].extra["summary"].as_str().unwrap().to_owned());
    }
    assert_eq!(summaries.len(), 1);
}

#[test]
fn test_dedup_no_attribute_merge_when_source_file_missing() {
    let mut first = concept("c", "c", "");
    first.extra.insert("summary".into(), "A".into());
    let mut second = concept("c", "c", "");
    second.extra.insert("notes".into(), "B".into());
    let survivor = run(&[first, second], &[]).0.remove(0);
    assert!(!(survivor.extra.contains_key("summary") && survivor.extra.contains_key("notes")));
}

#[test]
fn test_dedup_survivor_does_not_inherit_false_origin_ast() {
    let mut ast = node("x", "run() [ast]", "code", "m.py");
    ast.source_location = Some("L2".into());
    ast.extra.insert("_origin".into(), "ast".into());
    let mut semantic = node("x", "run", "code", "m.py");
    semantic.source_location = Some("L9".into());
    let survivor = run(&[semantic, ast], &[]).0.remove(0);
    assert_ne!(
        survivor
            .extra
            .get("_origin")
            .and_then(serde_json::Value::as_str),
        Some("ast")
    );
}

#[test]
fn test_dedup_fills_explicit_none_attribute() {
    let first = node("y", "y", "code", "m.py");
    let mut second = node("y", "y helper", "code", "m.py");
    second.source_location = Some("L7".into());
    let survivor = run(&[first, second], &[]).0.remove(0);
    assert_eq!(survivor.source_location.as_deref(), Some("L7"));
}

#[test]
fn test_crossfile_identical_concepts_merge_and_rewire() {
    let nodes = [
        concept("sz_intl", "SHENZHEN INTERNATIONAL", "doc1.md"),
        concept(
            "shenzhen_international_holdings",
            "Shenzhen international",
            "doc2.md",
        ),
        concept("port_ops", "Port Operations", "doc2.md"),
    ];
    let (nodes, edges) = run(
        &nodes,
        &[edge(
            "shenzhen_international_holdings",
            "port_ops",
            "operates",
        )],
    );
    assert_eq!(nodes.len(), 2);
    assert!(nodes.iter().any(|node| node.id == "sz_intl"));
    assert_eq!(edges[0].source, "sz_intl");
    assert_eq!(edges[0].target, "port_ops");
}

#[test]
fn test_crossfile_one_char_typo_concepts_still_merge() {
    let nodes = [
        concept("g1", "Authentication Manager", "a.md"),
        concept("g2", "Authentication Managr", "b.md"),
    ];
    assert_eq!(run(&nodes, &[]).0.len(), 1);
}

fn assert_guarded_pair(left: Node, right: Node) {
    assert_eq!(run(&[left, right], &[]).0.len(), 2);
}

#[test]
fn test_crossfile_identical_labels_stay_distinct_for_guarded_types_document() {
    assert_guarded_pair(
        node(
            "d1",
            "Getting Started Installation Guide",
            "document",
            "docs/a.md",
        ),
        node(
            "d2",
            "Getting Started Installation Guide",
            "document",
            "docs/b.md",
        ),
    );
}

#[test]
fn test_crossfile_identical_labels_stay_distinct_for_guarded_types_rationale() {
    let label = "Django app config for apps.platform.cards. No business logic here. Domain services live in services.py.";
    assert_guarded_pair(
        node("r1", label, "rationale", "apps/platform/cards/apps.py"),
        node("r2", label, "rationale", "apps/platform/cores/apps.py"),
    );
}

#[test]
fn test_crossfile_identical_labels_stay_distinct_for_guarded_types_code() {
    assert_guarded_pair(
        node(
            "backend_a_render_frame",
            "render_frame",
            "code",
            "backend_a.py",
        ),
        node(
            "backend_b_render_frame",
            "render_frame",
            "code",
            "backend_b.py",
        ),
    );
}

#[test]
fn test_crossfile_identical_labels_stay_distinct_for_guarded_types_image_basename() {
    assert_guarded_pair(
        node("web_logo", "logo.png", "image", "web/assets/logo.png"),
        node("docs_logo", "logo.png", "image", "docs/img/logo.png"),
    );
}

#[test]
fn test_crossfile_identical_labels_stay_distinct_for_guarded_types_concept_image_mixed() {
    assert_guarded_pair(
        concept("logo_concept", "logo.png", "doc1.md"),
        node("logo_image", "logo.png", "image", "assets/logo.png"),
    );
}

#[test]
fn test_crossfile_identical_labels_stay_distinct_for_guarded_types_empty_source_file() {
    assert_guarded_pair(
        concept("shenzhen_a", "Shenzhen International", ""),
        concept("shenzhen_b", "Shenzhen International", ""),
    );
}

#[test]
fn test_crossfile_identical_labels_stay_distinct_for_guarded_types_low_entropy_concept() {
    assert_guarded_pair(
        concept("api_a", "API", "doc1.md"),
        concept("api_b", "API", "doc2.md"),
    );
}

#[test]
fn test_cross_repo_guard_still_raises() {
    let mut left = concept("c1", "Shenzhen International", "doc1.md");
    left.extra.insert("repo".into(), "repo-a".into());
    let mut right = concept("c2", "Shenzhen International", "doc2.md");
    right.extra.insert("repo".into(), "repo-b".into());
    let error = deduplicate_entities(&[left, right], &[], &BTreeMap::new()).unwrap_err();
    assert!(error.to_string().contains("multiple repos"));
}

#[test]
fn test_crossfile_concept_merge_is_order_independent() {
    let base = [
        concept("shenzhen", "SHENZHEN INTERNATIONAL", "doc1.md"),
        concept("shenzhen_intl", "Shenzhen international", "doc2.md"),
        concept(
            "shenzhen_international",
            "shenzhen-international",
            "doc3.md",
        ),
    ];
    let mut survivors = BTreeSet::new();
    for values in permutations(&base) {
        let output = run(&values, &[]).0;
        assert_eq!(output.len(), 1);
        survivors.insert(output[0].id.clone());
    }
    assert_eq!(survivors, BTreeSet::from(["shenzhen".to_owned()]));
}

#[test]
fn test_crossfile_concept_merge_deterministic_across_hash_seeds() {
    let orders = [[0, 1, 2], [2, 0, 1], [1, 2, 0], [2, 1, 0]];
    let base = [
        concept("shenzhen", "SHENZHEN INTERNATIONAL", "doc1.md"),
        concept("shenzhen_intl", "Shenzhen international", "doc2.md"),
        concept(
            "shenzhen_international",
            "shenzhen-international",
            "doc3.md",
        ),
    ];
    let results: BTreeSet<_> = orders
        .iter()
        .map(|order| {
            let values: Vec<_> = order.iter().map(|index| base[*index].clone()).collect();
            let out = run(&values, &[]).0;
            format!("{} {}", out.len(), out[0].id)
        })
        .collect();
    assert_eq!(results, BTreeSet::from(["1 shenzhen".to_owned()]));
}

#[test]
fn test_crossfile_concept_merge_is_transitive() {
    let nodes = [
        concept("acme_corp_one", "Acme Corp", "doc1.md"),
        concept("acme_corp_two", "Acme Corp", "doc2.md"),
        concept("acme_corp_three", "Acme Corp.", "doc3.md"),
    ];
    assert_eq!(run(&nodes, &[]).0.len(), 1);
}
