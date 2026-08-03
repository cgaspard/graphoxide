//! Executable port of the non-CLI cases in upstream `tests/test_reflect.py`.

use chrono::{DateTime, Duration, TimeZone, Utc};
use filetime::{set_file_mtime, FileTime};
use graphoxide_core::{
    aggregate_lessons, build_learning_overlay, lessons_fresh, load_learning_overlay,
    load_memory_docs, parse_memory_doc, reflect, render_lessons_md, save_query_result, MemoryDoc,
    ReflectOptions, SaveResultOptions, LEARNING_SIDECAR_NAME,
};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap()
}

fn days_before(days: i64) -> String {
    (now() - Duration::days(days)).to_rfc3339()
}

fn doc(outcome: Option<&str>, nodes: &[&str]) -> MemoryDoc {
    MemoryDoc {
        outcome: outcome.map(str::to_owned),
        source_nodes: nodes.iter().map(|value| (*value).into()).collect(),
        question: "q".into(),
        date: "2026-01-01".into(),
        ..Default::default()
    }
}

fn aggregate(docs: &[MemoryDoc]) -> graphoxide_core::LessonAggregate {
    aggregate_lessons(docs, None, now(), 30.0, 2, None)
}

fn save_options(outcome: Option<&str>, nodes: &[&str]) -> SaveResultOptions {
    SaveResultOptions {
        source_nodes: nodes.iter().map(|value| (*value).into()).collect(),
        outcome: outcome.map(str::to_owned),
        now: Some(now()),
        ..Default::default()
    }
}

fn write_raw_doc(
    memory: &Path,
    filename: &str,
    date: &str,
    outcome: &str,
    question: &str,
    nodes: &[&str],
    correction: &str,
) {
    fs::create_dir_all(memory).unwrap();
    let mut lines = vec![
        "---".into(),
        "type: \"query\"".into(),
        format!("date: \"{date}\""),
        format!("question: \"{question}\""),
        "contributor: \"graphoxide\"".into(),
        format!("outcome: \"{outcome}\""),
    ];
    if !correction.is_empty() {
        lines.push(format!("correction: \"{correction}\""));
    }
    if !nodes.is_empty() {
        lines.push(format!(
            "source_nodes: [{}]",
            nodes
                .iter()
                .map(|node| format!("\"{node}\""))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    lines.extend(["---".into(), String::new(), format!("# Q: {question}")]);
    fs::write(memory.join(filename), lines.join("\n")).unwrap();
}

fn write_graph(output: &Path, nodes: serde_json::Value) -> PathBuf {
    fs::create_dir_all(output).unwrap();
    let graph = output.join("graph.json");
    fs::write(
        &graph,
        serde_json::to_vec(&json!({
            "directed": true,
            "multigraph": false,
            "graph": {},
            "nodes": nodes,
            "links": []
        }))
        .unwrap(),
    )
    .unwrap();
    graph
}

fn overlay_corpus(memory: &Path) {
    write_raw_doc(
        memory,
        "p1.md",
        "2026-05-01",
        "useful",
        "how do I auth?",
        &["login()"],
        "",
    );
    write_raw_doc(
        memory,
        "p2.md",
        "2026-05-10",
        "useful",
        "auth again",
        &["login()"],
        "",
    );
    write_raw_doc(
        memory,
        "t1.md",
        "2026-05-02",
        "useful",
        "cache?",
        &["RedisClient"],
        "",
    );
    write_raw_doc(
        memory,
        "c1.md",
        "2026-05-03",
        "useful",
        "contested useful",
        &["Contested"],
        "",
    );
    write_raw_doc(
        memory,
        "c2.md",
        "2026-05-04",
        "dead_end",
        "contested dead",
        &["Contested"],
        "",
    );
    write_raw_doc(
        memory,
        "d1.md",
        "2026-05-05",
        "dead_end",
        "led nowhere",
        &["DeadEnd"],
        "",
    );
}

#[test]
fn test_parse_round_trips_a_saved_doc() {
    let temp = tempdir().unwrap();
    let path = save_query_result(
        "what is \"attention\"?",
        "softmax",
        &temp.path().join("memory"),
        &SaveResultOptions {
            query_type: "explain".into(),
            ..save_options(Some("useful"), &["AttentionLayer", "SoftmaxFunc"])
        },
    )
    .unwrap();
    let parsed = parse_memory_doc(&fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(parsed.query_type, "explain");
    assert_eq!(parsed.question, "what is \"attention\"?");
    assert_eq!(parsed.outcome.as_deref(), Some("useful"));
    assert_eq!(parsed.source_nodes, ["AttentionLayer", "SoftmaxFunc"]);
}

#[test]
fn test_parse_returns_none_for_foreign_doc() {
    assert!(parse_memory_doc("# just a note\n\nno frontmatter here\n").is_none());
    assert!(parse_memory_doc("").is_none());
}

#[test]
fn test_round_trip_survives_backslash_newline_and_quoted_node() {
    let temp = tempdir().unwrap();
    let path = save_query_result(
        "path is C:\\Users and a \"quote\"",
        "a",
        &temp.path().join("memory"),
        &SaveResultOptions {
            correction: Some("line1\nline2".into()),
            ..save_options(Some("corrected"), &["Node\"With\\Quote"])
        },
    )
    .unwrap();
    let parsed = parse_memory_doc(&fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(parsed.question, "path is C:\\Users and a \"quote\"");
    assert_eq!(parsed.correction, "line1\nline2");
    assert_eq!(parsed.source_nodes, ["Node\"With\\Quote"]);
}

#[test]
fn test_parse_handles_crlf() {
    let parsed = parse_memory_doc(
        "---\r\ntype: \"query\"\r\noutcome: \"useful\"\r\nsource_nodes: [\"A\"]\r\n---\r\n# body\r\n",
    )
    .unwrap();
    assert_eq!(parsed.outcome.as_deref(), Some("useful"));
    assert_eq!(parsed.source_nodes, ["A"]);
}

#[test]
fn test_load_memory_docs_skips_foreign_and_sorts() {
    let temp = tempdir().unwrap();
    let memory = temp.path().join("memory");
    fs::create_dir(&memory).unwrap();
    fs::write(memory.join("foreign.md"), "# not a memory doc\n").unwrap();
    save_query_result("first", "a", &memory, &save_options(Some("useful"), &[])).unwrap();
    save_query_result("second", "b", &memory, &save_options(Some("dead_end"), &[])).unwrap();
    let docs = load_memory_docs(&memory);
    assert_eq!(docs.len(), 2);
    assert_eq!(
        docs.iter()
            .filter_map(|doc| doc.outcome.as_deref())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["dead_end", "useful"])
    );
}

#[test]
fn test_load_memory_docs_missing_dir_is_empty() {
    let temp = tempdir().unwrap();
    assert!(load_memory_docs(&temp.path().join("nope")).is_empty());
}

#[test]
fn test_load_memory_docs_orders_by_date_then_filename() {
    let temp = tempdir().unwrap();
    let memory = temp.path().join("memory");
    write_raw_doc(&memory, "z.md", "2026-03-01", "dead_end", "march", &[], "");
    write_raw_doc(
        &memory,
        "a.md",
        "2026-01-01",
        "dead_end",
        "january",
        &[],
        "",
    );
    write_raw_doc(
        &memory,
        "b.md",
        "2026-02-01",
        "dead_end",
        "february",
        &[],
        "",
    );
    write_raw_doc(
        &memory,
        "c.md",
        "2026-01-01",
        "dead_end",
        "january-2",
        &[],
        "",
    );
    let docs = load_memory_docs(&memory);
    assert_eq!(
        docs.iter().map(|doc| doc.date.as_str()).collect::<Vec<_>>(),
        ["2026-01-01", "2026-01-01", "2026-02-01", "2026-03-01"]
    );
    assert_eq!(
        docs.iter()
            .filter(|doc| doc.date == "2026-01-01")
            .map(|doc| doc.path.as_str())
            .collect::<Vec<_>>(),
        ["a.md", "c.md"]
    );
}

#[test]
fn test_aggregate_counts_each_outcome() {
    let docs = [
        doc(Some("useful"), &["A"]),
        doc(Some("useful"), &["A", "B"]),
        doc(Some("dead_end"), &["C"]),
        MemoryDoc {
            outcome: Some("corrected".into()),
            correction: "use D".into(),
            ..doc(None, &[])
        },
        doc(None, &[]),
    ];
    let result = aggregate(&docs);
    assert_eq!(result.total, 5);
    assert_eq!(
        (
            result.counts.useful,
            result.counts.dead_end,
            result.counts.corrected,
            result.counts.unmarked
        ),
        (2, 1, 1, 1)
    );
}

#[test]
fn test_sources_split_into_preferred_tentative_contested() {
    let result = aggregate(&[
        doc(Some("useful"), &["A", "B"]),
        doc(Some("useful"), &["A", "B"]),
        doc(Some("useful"), &["C"]),
        doc(Some("dead_end"), &["A"]),
    ]);
    assert_eq!(
        result
            .preferred
            .iter()
            .map(|v| v.node.as_str())
            .collect::<Vec<_>>(),
        ["B"]
    );
    assert_eq!(
        result
            .tentative
            .iter()
            .map(|v| v.node.as_str())
            .collect::<Vec<_>>(),
        ["C"]
    );
    assert_eq!(
        result
            .contested
            .iter()
            .map(|v| v.node.as_str())
            .collect::<Vec<_>>(),
        ["A"]
    );
}

#[test]
fn test_corroboration_threshold_promotes_only_repeated_nodes() {
    let one = aggregate(&[doc(Some("useful"), &["A"])]);
    assert!(one.preferred.is_empty());
    assert_eq!(one.tentative[0].node, "A");
    let two = aggregate(&[doc(Some("useful"), &["A"]), doc(Some("useful"), &["A"])]);
    assert_eq!(two.preferred[0].node, "A");
    assert!(two.tentative.is_empty());
}

#[test]
fn test_recency_decides_contested_verdict() {
    let mut stale = doc(Some("useful"), &["N"]);
    stale.date = days_before(120);
    let mut fresh = doc(Some("dead_end"), &["N"]);
    fresh.date = days_before(1);
    assert_eq!(aggregate(&[stale, fresh]).contested[0].verdict, "dead end");
    let mut fresh = doc(Some("useful"), &["N"]);
    fresh.date = days_before(1);
    let mut stale = doc(Some("dead_end"), &["N"]);
    stale.date = days_before(120);
    assert_eq!(aggregate(&[fresh, stale]).contested[0].verdict, "useful");
}

#[test]
fn test_node_existence_gate_drops_stale_nodes() {
    let docs = [
        doc(Some("useful"), &["Alive", "Deleted"]),
        doc(Some("useful"), &["Alive", "Deleted"]),
    ];
    let known = BTreeSet::from(["Alive".into()]);
    let result = aggregate_lessons(&docs, None, now(), 30.0, 2, Some(&known));
    assert_eq!(
        result
            .preferred
            .iter()
            .map(|v| v.node.as_str())
            .collect::<Vec<_>>(),
        ["Alive"]
    );
}

#[test]
fn test_corroboration_counts_distinct_docs_not_citations() {
    let result = aggregate(&[doc(Some("useful"), &["A", "A"])]);
    assert!(result.preferred.is_empty());
    assert_eq!(
        (result.tentative[0].node.as_str(), result.tentative[0].n),
        ("A", 1)
    );
}

#[test]
fn test_min_corroboration_is_honored_not_hardcoded() {
    let docs = [doc(Some("useful"), &["A"]), doc(Some("useful"), &["A"])];
    assert_eq!(aggregate(&docs).preferred[0].node, "A");
    let at_three = aggregate_lessons(&docs, None, now(), 30.0, 3, None);
    assert!(at_three.preferred.is_empty());
    assert_eq!(at_three.tentative[0].node, "A");
}

#[test]
fn test_half_life_actually_feeds_decay() {
    let mut docs = vec![
        doc(Some("useful"), &["N"]),
        doc(Some("useful"), &["N"]),
        doc(Some("dead_end"), &["N"]),
    ];
    docs[0].date = days_before(90);
    docs[1].date = days_before(90);
    docs[2].date = days_before(1);
    assert_eq!(
        aggregate_lessons(&docs, None, now(), 100_000.0, 2, None).contested[0].verdict,
        "useful"
    );
    assert_eq!(
        aggregate_lessons(&docs, None, now(), 10.0, 2, None).contested[0].verdict,
        "dead end"
    );
}

#[test]
fn test_evenly_split_verdict_when_signals_cancel() {
    let day = days_before(5);
    let mut useful = doc(Some("useful"), &["N"]);
    useful.date = day.clone();
    let mut dead = doc(Some("dead_end"), &["N"]);
    dead.date = day;
    let result = aggregate(&[useful, dead]);
    assert_eq!(result.contested[0].verdict, "even");
    assert!(render_lessons_md(&result).contains("evenly split"));
}

#[test]
fn test_nonpositive_half_life_disables_decay() {
    let mut useful = doc(Some("useful"), &["N"]);
    useful.date = days_before(365);
    let mut dead = doc(Some("dead_end"), &["N"]);
    dead.date = days_before(1);
    assert_eq!(
        aggregate_lessons(&[useful, dead], None, now(), 0.0, 2, None).contested[0].verdict,
        "even"
    );
}

#[test]
fn test_negative_only_node_absent_from_sources() {
    let mut dead = doc(Some("dead_end"), &["Bad"]);
    dead.question = "why?".into();
    let result = aggregate(&[dead]);
    assert!(
        result.preferred.is_empty() && result.tentative.is_empty() && result.contested.is_empty()
    );
    assert_eq!(result.dead_ends[0].nodes, ["Bad"]);
}

#[test]
fn test_dead_ends_and_corrections_collected() {
    let mut dead = doc(Some("dead_end"), &["RedisClient"]);
    dead.question = "where is the cache?".into();
    let mut corrected = doc(Some("corrected"), &[]);
    corrected.question = "what hashes pw?".into();
    corrected.correction = "bcrypt".into();
    let result = aggregate(&[dead, corrected]);
    assert_eq!(result.dead_ends[0].question, "where is the cache?");
    assert_eq!(result.dead_ends[0].nodes, ["RedisClient"]);
    assert_eq!(result.corrections[0].correction, "bcrypt");
}

#[test]
fn test_dead_ends_and_corrections_follow_doc_order() {
    let temp = tempdir().unwrap();
    let memory = temp.path().join("memory");
    write_raw_doc(
        &memory,
        "later.md",
        "2026-02-01",
        "dead_end",
        "second",
        &[],
        "",
    );
    write_raw_doc(
        &memory,
        "earlier.md",
        "2026-01-01",
        "dead_end",
        "first",
        &[],
        "",
    );
    assert_eq!(
        aggregate(&load_memory_docs(&memory))
            .dead_ends
            .iter()
            .map(|v| v.question.as_str())
            .collect::<Vec<_>>(),
        ["first", "second"]
    );
}

#[test]
fn test_no_community_grouping_without_graph() {
    assert!(aggregate(&[doc(Some("useful"), &["A"])])
        .by_community
        .is_empty());
}

#[test]
fn test_doc_community_tie_breaks_to_smallest_label() {
    let mapping = BTreeMap::from([("x".into(), "Zeta".into()), ("y".into(), "Alpha".into())]);
    let first = aggregate_lessons(
        &[doc(Some("useful"), &["x", "y"])],
        Some(&mapping),
        now(),
        30.0,
        2,
        None,
    );
    let second = aggregate_lessons(
        &[doc(Some("useful"), &["y", "x"])],
        Some(&mapping),
        now(),
        30.0,
        2,
        None,
    );
    assert!(first.by_community.contains_key("Alpha") && !first.by_community.contains_key("Zeta"));
    assert_eq!(
        first.by_community.keys().collect::<Vec<_>>(),
        second.by_community.keys().collect::<Vec<_>>()
    );
}

#[test]
fn test_community_grouping_uses_plurality_community() {
    let mapping = BTreeMap::from([
        ("A".into(), "Auth".into()),
        ("B".into(), "Auth".into()),
        ("C".into(), "Cache".into()),
    ]);
    let result = aggregate_lessons(
        &[
            doc(Some("useful"), &["A", "B", "C"]),
            doc(Some("dead_end"), &["C"]),
            doc(Some("useful"), &["Z"]),
        ],
        Some(&mapping),
        now(),
        30.0,
        2,
        None,
    );
    assert_eq!(
        result
            .by_community
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["Auth", "Cache", "Uncategorized"])
    );
    assert_eq!(result.by_community["Auth"].counts.useful, 1);
    assert_eq!(result.by_community["Cache"].counts.dead_end, 1);
    assert_eq!(result.by_community["Uncategorized"].counts.useful, 1);
}

#[test]
fn test_render_is_deterministic() {
    let mut dead = doc(Some("dead_end"), &["C"]);
    dead.question = "dead?".into();
    let result = aggregate(&[doc(Some("useful"), &["A", "B"]), dead]);
    assert_eq!(render_lessons_md(&result), render_lessons_md(&result));
}

#[test]
fn test_render_has_summary_and_sections() {
    let mut dead = doc(Some("dead_end"), &["RedisClient"]);
    dead.question = "where is the cache?".into();
    let mut corrected = doc(Some("corrected"), &[]);
    corrected.question = "pw?".into();
    corrected.correction = "bcrypt".into();
    let markdown = render_lessons_md(&aggregate(&[
        doc(Some("useful"), &["AuthMiddleware"]),
        dead,
        corrected,
    ]));
    assert!(markdown.contains("# Lessons"));
    assert!(markdown.contains("1 useful · 1 dead ends · 1 corrected"));
    assert!(markdown.contains("`AuthMiddleware`"));
    assert!(markdown.contains("where is the cache?") && markdown.contains("bcrypt"));
    assert!(!markdown.contains("## By topic"));
}

#[test]
fn test_render_includes_by_topic_when_graph_present() {
    let mapping = BTreeMap::from([("A".into(), "Auth".into())]);
    let result = aggregate_lessons(
        &[doc(Some("useful"), &["A"])],
        Some(&mapping),
        now(),
        30.0,
        2,
        None,
    );
    let markdown = render_lessons_md(&result);
    assert!(markdown.contains("## By topic") && markdown.contains("### Auth"));
}

#[test]
fn test_topic_sections_alpha_with_uncategorized_last() {
    let mapping = BTreeMap::from([("a".into(), "Zeta".into()), ("b".into(), "Alpha".into())]);
    let result = aggregate_lessons(
        &[
            doc(Some("useful"), &["a"]),
            doc(Some("useful"), &["b"]),
            doc(Some("useful"), &["unknown"]),
        ],
        Some(&mapping),
        now(),
        30.0,
        2,
        None,
    );
    let markdown = render_lessons_md(&result);
    let headers = markdown
        .lines()
        .filter_map(|line| line.strip_prefix("### "))
        .collect::<Vec<_>>();
    assert_eq!(headers, ["Alpha", "Zeta", "Uncategorized"]);
}

#[test]
fn test_render_byte_stable_across_independent_aggregations() {
    let temp = tempdir().unwrap();
    let memory = temp.path().join("memory");
    write_raw_doc(
        &memory,
        "a.md",
        "2026-01-01",
        "useful",
        "q",
        &["A", "B"],
        "",
    );
    write_raw_doc(&memory, "b.md", "2026-01-02", "dead_end", "dead?", &[], "");
    let first = render_lessons_md(&aggregate(&load_memory_docs(&memory)));
    let second = render_lessons_md(&aggregate(&load_memory_docs(&memory)));
    assert_eq!(first, second);
}

#[test]
fn test_contested_node_renders_once_under_contested() {
    let mut dead = doc(Some("dead_end"), &["N"]);
    dead.question = "bad?".into();
    let markdown = render_lessons_md(&aggregate(&[doc(Some("useful"), &["N"]), dead]));
    assert!(markdown.contains("**Contested**"));
    assert_eq!(
        markdown
            .lines()
            .filter(|line| line.starts_with("- `N` —")
                && line.contains("useful")
                && line.contains("dead end"))
            .count(),
        1
    );
}

#[test]
fn test_header_is_cautious() {
    let markdown = render_lessons_md(&aggregate(&[doc(Some("useful"), &["A"])]));
    assert!(markdown.contains("verify before relying"));
    assert!(!markdown.contains("reuse what worked"));
}

#[test]
fn test_lessons_artifact_cannot_be_globbed_back_into_memory() {
    let markdown = render_lessons_md(&aggregate(&[doc(Some("useful"), &["A"])]));
    assert!(parse_memory_doc(&markdown).is_none());
    let temp = tempdir().unwrap();
    let memory = temp.path().join("memory");
    fs::create_dir(&memory).unwrap();
    fs::write(memory.join("LESSONS.md"), markdown).unwrap();
    save_query_result("real", "a", &memory, &save_options(Some("useful"), &[])).unwrap();
    save_query_result("real", "a", &memory, &save_options(Some("useful"), &[])).unwrap();
    let docs = load_memory_docs(&memory);
    assert_eq!(docs.len(), 1);
    assert_eq!(docs[0].question, "real");
}

#[test]
fn test_render_empty_memory_is_graceful() {
    let markdown = render_lessons_md(&aggregate(&[]));
    assert!(markdown.contains("from 0 session memories"));
    assert!(markdown.contains("_No marked outcomes yet._"));
}

#[test]
fn test_reflect_writes_lessons_file() {
    let temp = tempdir().unwrap();
    let memory = temp.path().join("memory");
    save_query_result("q1", "a1", &memory, &save_options(Some("useful"), &["A"])).unwrap();
    let output = temp.path().join("reflections/LESSONS.md");
    let (path, result) = reflect(
        &memory,
        &output,
        &ReflectOptions {
            now: Some(now()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(path.exists());
    assert_eq!(result.total, 1);
    assert!(fs::read_to_string(path).unwrap().contains("`A`"));
}

#[test]
fn test_second_session_benefits_from_the_first() {
    let temp = tempdir().unwrap();
    let output = temp.path().join("graphoxide-out");
    let memory = output.join("memory");
    save_query_result(
        "how does auth work?",
        "JWT in middleware",
        &memory,
        &save_options(Some("useful"), &["AuthMiddleware"]),
    )
    .unwrap();
    save_query_result(
        "where is the cache?",
        "not Redis",
        &memory,
        &save_options(Some("dead_end"), &["RedisClient"]),
    )
    .unwrap();
    let lessons = output.join("reflections/LESSONS.md");
    reflect(
        &memory,
        &lessons,
        &ReflectOptions {
            now: Some(now()),
            ..Default::default()
        },
    )
    .unwrap();
    let body = fs::read_to_string(lessons).unwrap();
    assert!(body.contains("`AuthMiddleware`") && body.contains("where is the cache?"));
}

fn set_mtime(path: &Path, seconds: i64) {
    set_file_mtime(path, FileTime::from_unix_time(seconds, 0)).unwrap();
}

#[test]
fn test_lessons_fresh_missing_output_is_not_fresh() {
    let temp = tempdir().unwrap();
    let memory = temp.path().join("memory");
    fs::create_dir(&memory).unwrap();
    fs::write(memory.join("q.md"), "x").unwrap();
    assert!(!lessons_fresh(
        &temp.path().join("LESSONS.md"),
        &memory,
        None,
        None,
        None
    ));
}

#[test]
fn test_lessons_fresh_true_when_output_newer_than_inputs() {
    let temp = tempdir().unwrap();
    let memory = temp.path().join("memory");
    fs::create_dir(&memory).unwrap();
    let input = memory.join("q.md");
    let output = temp.path().join("LESSONS.md");
    fs::write(&input, "x").unwrap();
    fs::write(&output, "y").unwrap();
    set_mtime(&input, 1_000);
    set_mtime(&output, 2_000);
    assert!(lessons_fresh(&output, &memory, None, None, None));
}

#[test]
fn test_lessons_fresh_false_when_memory_newer() {
    let temp = tempdir().unwrap();
    let memory = temp.path().join("memory");
    fs::create_dir(&memory).unwrap();
    let input = memory.join("q.md");
    let output = temp.path().join("LESSONS.md");
    fs::write(&input, "x").unwrap();
    fs::write(&output, "y").unwrap();
    set_mtime(&output, 1_000);
    set_mtime(&input, 2_000);
    assert!(!lessons_fresh(&output, &memory, None, None, None));
}

#[test]
fn test_lessons_fresh_false_when_graph_newer() {
    let temp = tempdir().unwrap();
    let memory = temp.path().join("memory");
    fs::create_dir(&memory).unwrap();
    let input = memory.join("q.md");
    let output = temp.path().join("LESSONS.md");
    let graph = temp.path().join("graph.json");
    for path in [&input, &output, &graph] {
        fs::write(path, "{}").unwrap();
    }
    set_mtime(&input, 1_000);
    set_mtime(&output, 1_500);
    set_mtime(&graph, 2_000);
    assert!(!lessons_fresh(&output, &memory, Some(&graph), None, None));
}

fn sidecar_freshness_case(newer: &str) {
    let temp = tempdir().unwrap();
    let memory = temp.path().join("memory");
    fs::create_dir(&memory).unwrap();
    let input = memory.join("q.md");
    let output = temp.path().join("LESSONS.md");
    let graph = temp.path().join("graph.json");
    let analysis = temp.path().join(".graphify_analysis.json");
    let labels = temp.path().join(".graphify_labels.json");
    for path in [&input, &output, &graph, &analysis, &labels] {
        fs::write(path, "{}").unwrap();
        set_mtime(path, 1_000);
    }
    set_mtime(&output, 1_500);
    set_mtime(
        if newer == "analysis" {
            &analysis
        } else {
            &labels
        },
        2_000,
    );
    assert!(!lessons_fresh(
        &output,
        &memory,
        Some(&graph),
        Some(&analysis),
        Some(&labels)
    ));
}

#[test]
fn test_lessons_fresh_false_when_graph_sidecar_newer_analysis() {
    sidecar_freshness_case("analysis");
}

#[test]
fn test_lessons_fresh_false_when_graph_sidecar_newer_labels() {
    sidecar_freshness_case("labels");
}

#[test]
fn test_dead_ends_and_corrections_dedupe_by_question() {
    let mut dead1 = doc(Some("dead_end"), &[]);
    dead1.question = "ws server?".into();
    dead1.date = "2026-01-01".into();
    let mut dead2 = dead1.clone();
    dead2.date = "2026-01-02".into();
    let mut fix1 = doc(Some("corrected"), &[]);
    fix1.question = "hash?".into();
    fix1.correction = "SHA-1".into();
    fix1.date = "2026-01-01".into();
    let mut fix2 = fix1.clone();
    fix2.correction = "SHA-256".into();
    fix2.date = "2026-01-03".into();
    let result = aggregate(&[dead1, dead2, fix1, fix2]);
    assert_eq!(result.dead_ends.len(), 1);
    assert_eq!(result.corrections.len(), 1);
    assert_eq!(result.corrections[0].correction, "SHA-256");
}

#[test]
fn test_sidecar_write_classifies_and_keys_by_canonical_id() {
    let temp = tempdir().unwrap();
    let output = temp.path().join("graphoxide-out");
    let source = temp.path().join("auth.py");
    fs::write(&source, "def login(): pass\n").unwrap();
    let graph = write_graph(
        &output,
        json!([
            {"id":"auth_login","label":"login()","source_file":source,"community":0},
            {"id":"redis_client","label":"RedisClient","source_file":"","community":0},
            {"id":"contested_node","label":"Contested","source_file":"","community":0},
            {"id":"deadend_node","label":"DeadEnd","source_file":"","community":0}
        ]),
    );
    let memory = output.join("memory");
    overlay_corpus(&memory);
    reflect(
        &memory,
        &output.join("reflections/LESSONS.md"),
        &ReflectOptions {
            graph_path: Some(graph.clone()),
            now: Some(now()),
            ..Default::default()
        },
    )
    .unwrap();
    let sidecar: serde_json::Value =
        serde_json::from_slice(&fs::read(output.join(LEARNING_SIDECAR_NAME)).unwrap()).unwrap();
    assert_eq!(sidecar["version"], 1);
    assert_eq!(sidecar["generated_at"], now().to_rfc3339());
    assert_eq!(sidecar["nodes"]["auth_login"]["status"], "preferred");
    assert_eq!(sidecar["nodes"]["auth_login"]["uses"], 2);
    assert_eq!(sidecar["nodes"]["auth_login"]["label"], "login()");
    assert!(sidecar["nodes"]["auth_login"]["score"].is_f64());
    assert!(sidecar["nodes"]["auth_login"]["provenance"]
        .as_array()
        .is_some_and(|v| !v.is_empty()));
    assert_eq!(sidecar["nodes"]["redis_client"]["status"], "tentative");
    assert_eq!(sidecar["nodes"]["contested_node"]["status"], "contested");
    assert!(sidecar["nodes"].get("deadend_node").is_none());
    let graph_value: serde_json::Value = serde_json::from_slice(&fs::read(graph).unwrap()).unwrap();
    assert!(graph_value["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .all(|node| node
            .as_object()
            .unwrap()
            .keys()
            .all(|key| !key.starts_with("learning"))));
}

#[test]
fn test_sidecar_is_byte_identical_across_runs() {
    let temp = tempdir().unwrap();
    let output = temp.path().join("graphoxide-out");
    let source = temp.path().join("auth.py");
    fs::write(&source, "def login(): pass\n").unwrap();
    let graph = write_graph(
        &output,
        json!([{"id":"auth_login","label":"login()","source_file":source,"community":0}]),
    );
    let memory = output.join("memory");
    write_raw_doc(
        &memory,
        "a.md",
        "2026-05-01",
        "useful",
        "q1",
        &["login()"],
        "",
    );
    write_raw_doc(
        &memory,
        "b.md",
        "2026-05-10",
        "useful",
        "q2",
        &["login()"],
        "",
    );
    let options = ReflectOptions {
        graph_path: Some(graph),
        now: Some(now()),
        ..Default::default()
    };
    reflect(&memory, &output.join("reflections/LESSONS.md"), &options).unwrap();
    let first = fs::read(output.join(LEARNING_SIDECAR_NAME)).unwrap();
    reflect(&memory, &output.join("reflections/LESSONS.md"), &options).unwrap();
    assert_eq!(first, fs::read(output.join(LEARNING_SIDECAR_NAME)).unwrap());
}

#[test]
fn test_loader_marks_entry_stale_when_source_file_changes() {
    let temp = tempdir().unwrap();
    let output = temp.path().join("graphoxide-out");
    let source = temp.path().join("auth.py");
    fs::write(&source, "def login(): pass\n").unwrap();
    let graph = write_graph(
        &output,
        json!([{"id":"auth_login","label":"login()","source_file":source,"community":0}]),
    );
    let memory = output.join("memory");
    write_raw_doc(
        &memory,
        "a.md",
        "2026-05-01",
        "useful",
        "q1",
        &["login()"],
        "",
    );
    write_raw_doc(
        &memory,
        "b.md",
        "2026-05-10",
        "useful",
        "q2",
        &["login()"],
        "",
    );
    reflect(
        &memory,
        &output.join("reflections/LESSONS.md"),
        &ReflectOptions {
            graph_path: Some(graph.clone()),
            now: Some(now()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        load_learning_overlay(&graph)["auth_login"].stale,
        Some(false)
    );
    fs::write(&source, "def login(): return 1 # changed\n").unwrap();
    assert_eq!(
        load_learning_overlay(&graph)["auth_login"].stale,
        Some(true)
    );
}

#[test]
fn test_relative_source_file_not_spuriously_stale_in_graphify_out_layout() {
    let temp = tempdir().unwrap();
    let output = temp.path().join("graphify-out");
    let source = temp.path().join("auth.py");
    fs::write(&source, "def login(): pass\n").unwrap();
    let graph = write_graph(
        &output,
        json!([{"id":"auth_login","label":"login()","source_file":"auth.py","community":0}]),
    );
    let memory = output.join("memory");
    write_raw_doc(
        &memory,
        "a.md",
        "2026-05-01",
        "useful",
        "q1",
        &["login()"],
        "",
    );
    write_raw_doc(
        &memory,
        "b.md",
        "2026-05-10",
        "useful",
        "q2",
        &["login()"],
        "",
    );
    reflect(
        &memory,
        &output.join("reflections/LESSONS.md"),
        &ReflectOptions {
            graph_path: Some(graph.clone()),
            now: Some(now()),
            ..Default::default()
        },
    )
    .unwrap();
    let fresh = load_learning_overlay(&graph);
    assert_eq!(fresh["auth_login"].status, "preferred");
    assert_eq!(fresh["auth_login"].stale, Some(false));
    fs::write(source, "changed\n").unwrap();
    assert_eq!(
        load_learning_overlay(&graph)["auth_login"].stale,
        Some(true)
    );
}

#[test]
fn test_relative_source_file_resolved_via_graphify_root_marker() {
    let temp = tempdir().unwrap();
    let project = temp.path().join("project");
    fs::create_dir(&project).unwrap();
    fs::write(project.join("auth.py"), "def login(): pass\n").unwrap();
    let output = temp.path().join("elsewhere-out");
    let graph = write_graph(
        &output,
        json!([{"id":"auth_login","label":"login()","source_file":"auth.py","community":0}]),
    );
    fs::write(
        output.join(".graphify_root"),
        project.to_string_lossy().as_bytes(),
    )
    .unwrap();
    let memory = output.join("memory");
    write_raw_doc(
        &memory,
        "a.md",
        "2026-05-01",
        "useful",
        "q1",
        &["login()"],
        "",
    );
    write_raw_doc(
        &memory,
        "b.md",
        "2026-05-10",
        "useful",
        "q2",
        &["login()"],
        "",
    );
    reflect(
        &memory,
        &output.join("reflections/LESSONS.md"),
        &ReflectOptions {
            graph_path: Some(graph.clone()),
            now: Some(now()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        load_learning_overlay(&graph)["auth_login"].stale,
        Some(false)
    );
}

#[test]
fn test_flat_layout_does_not_match_same_named_file_one_dir_up() {
    let temp = tempdir().unwrap();
    let project = temp.path().join("proj");
    fs::create_dir(&project).unwrap();
    fs::write(project.join("util.py"), "REAL = 1\n").unwrap();
    fs::write(temp.path().join("util.py"), "DECOY = 2\n").unwrap();
    let graph = write_graph(
        &project,
        json!([{"id":"util","label":"util.py","source_file":"util.py","community":0}]),
    );
    let memory = project.join("memory");
    write_raw_doc(
        &memory,
        "a.md",
        "2026-05-01",
        "useful",
        "q1",
        &["util.py"],
        "",
    );
    write_raw_doc(
        &memory,
        "b.md",
        "2026-05-10",
        "useful",
        "q2",
        &["util.py"],
        "",
    );
    reflect(
        &memory,
        &project.join("reflections/LESSONS.md"),
        &ReflectOptions {
            graph_path: Some(graph.clone()),
            now: Some(now()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(load_learning_overlay(&graph)["util"].stale, Some(false));
    fs::write(temp.path().join("util.py"), "DECOY = 999\n").unwrap();
    assert_eq!(load_learning_overlay(&graph)["util"].stale, Some(false));
    fs::write(project.join("util.py"), "REAL = 999\n").unwrap();
    assert_eq!(load_learning_overlay(&graph)["util"].stale, Some(true));
}

#[test]
fn test_provenance_capped_to_five_most_recent() {
    let temp = tempdir().unwrap();
    let output = temp.path().join("graphoxide-out");
    let source = temp.path().join("auth.py");
    fs::write(&source, "x\n").unwrap();
    let graph = write_graph(
        &output,
        json!([{"id":"auth_login","label":"login()","source_file":source,"community":0}]),
    );
    let memory = output.join("memory");
    for index in 0..7 {
        write_raw_doc(
            &memory,
            &format!("u{index}.md"),
            &format!("2026-05-{:02}", 10 + index),
            "useful",
            &format!("q{index}"),
            &["login()"],
            "",
        );
    }
    let docs = load_memory_docs(&memory);
    let result = aggregate(&docs);
    let overlay = build_learning_overlay(&result, &graph, now());
    let provenance = &overlay.nodes["auth_login"].provenance;
    assert_eq!(provenance.len(), 5);
    assert_eq!(provenance.first().unwrap().date, "2026-05-16");
    assert_eq!(provenance.last().unwrap().date, "2026-05-12");
}

#[test]
fn test_ambiguous_or_unresolved_citation_is_skipped() {
    let temp = tempdir().unwrap();
    let output = temp.path().join("graphoxide-out");
    let graph = write_graph(
        &output,
        json!([
            {"id":"dup_a","label":"Dup","source_file":"","community":0},
            {"id":"dup_b","label":"Dup","source_file":"","community":0},
            {"id":"solo","label":"Solo","source_file":"","community":0}
        ]),
    );
    let memory = output.join("memory");
    write_raw_doc(&memory, "a.md", "2026-05-01", "useful", "q1", &["Dup"], "");
    write_raw_doc(&memory, "b.md", "2026-05-02", "useful", "q2", &["Dup"], "");
    write_raw_doc(&memory, "c.md", "2026-05-03", "useful", "q3", &["Solo"], "");
    write_raw_doc(&memory, "d.md", "2026-05-04", "useful", "q4", &["Solo"], "");
    let overlay = build_learning_overlay(&aggregate(&load_memory_docs(&memory)), &graph, now());
    assert!(!overlay.nodes.contains_key("dup_a") && !overlay.nodes.contains_key("dup_b"));
    assert!(overlay.nodes.contains_key("solo"));
}
