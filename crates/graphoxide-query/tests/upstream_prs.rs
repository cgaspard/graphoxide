use graphoxide_core::{KnowledgeGraph, Node};
use graphoxide_query::prs::*;
use serde_json::json;
use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::time::{Duration, SystemTime};

fn make_pr() -> PrInfo {
    PrInfo {
        number: 1,
        title: "Test PR".into(),
        branch: "feature".into(),
        base_branch: "v8".into(),
        author: "alice".into(),
        is_draft: false,
        review_decision: String::new(),
        ci_status: "SUCCESS".into(),
        updated_at: SystemTime::now() - Duration::from_secs(86_400),
        expected_base: "v8".into(),
        worktree_path: None,
        communities_touched: vec![],
        nodes_affected: 0,
        files_changed: vec![],
    }
}

fn graph_node(id: &str, label: &str, source: &str, community: Option<i64>) -> Node {
    Node {
        id: id.into(),
        label: label.into(),
        file_type: "code".into(),
        source_file: source.into(),
        source_location: None,
        community,
        extra: BTreeMap::new(),
    }
}

fn impact_graph() -> KnowledgeGraph {
    KnowledgeGraph {
        nodes: vec![
            graph_node("n1", "n1", "src/auth/api.py", Some(0)),
            graph_node("n2", "n2", "src/auth/api.py", Some(0)),
            graph_node("n3", "n3", "src/utils/helpers.py", Some(1)),
        ],
        ..KnowledgeGraph::default()
    }
}

#[derive(Default)]
struct MockRunner {
    outputs: RefCell<VecDeque<io::Result<CommandOutput>>>,
    calls: RefCell<Vec<(String, Vec<String>)>>,
}

impl MockRunner {
    fn with(outputs: impl IntoIterator<Item = io::Result<CommandOutput>>) -> Self {
        Self {
            outputs: RefCell::new(outputs.into_iter().collect()),
            calls: RefCell::new(Vec::new()),
        }
    }
}

impl CommandRunner for MockRunner {
    fn run(&self, program: &str, arguments: &[&str]) -> io::Result<CommandOutput> {
        self.calls.borrow_mut().push((
            program.into(),
            arguments.iter().map(|value| (*value).into()).collect(),
        ));
        self.outputs
            .borrow_mut()
            .pop_front()
            .unwrap_or_else(|| Err(io::Error::new(io::ErrorKind::NotFound, "missing")))
    }
}

fn output(success: bool, stdout: &str) -> io::Result<CommandOutput> {
    Ok(CommandOutput {
        success,
        stdout: stdout.as_bytes().to_vec(),
    })
}

#[test]
fn test_ready() {
    assert_eq!(classify(&make_pr(), "v8"), "READY");
}

#[test]
fn test_ci_fail() {
    let mut pr = make_pr();
    pr.ci_status = "FAILURE".into();
    assert_eq!(classify(&pr, "v8"), "CI-FAIL");
}

#[test]
fn test_changes_req() {
    let mut pr = make_pr();
    pr.review_decision = "CHANGES_REQUESTED".into();
    assert_eq!(classify(&pr, "v8"), "CHANGES-REQ");
}

#[test]
fn test_draft() {
    let mut pr = make_pr();
    pr.is_draft = true;
    assert_eq!(classify(&pr, "v8"), "DRAFT");
}

#[test]
fn test_stale() {
    let mut pr = make_pr();
    pr.updated_at = SystemTime::now() - Duration::from_secs(20 * 86_400);
    assert_eq!(classify(&pr, "v8"), "STALE");
}

#[test]
fn test_draft_not_marked_stale() {
    let mut pr = make_pr();
    pr.updated_at = SystemTime::now() - Duration::from_secs(20 * 86_400);
    pr.is_draft = true;
    assert_eq!(classify(&pr, "v8"), "DRAFT");
}

#[test]
fn test_pending() {
    let mut pr = make_pr();
    pr.ci_status = "PENDING".into();
    assert_eq!(classify(&pr, "v8"), "PENDING");
}

#[test]
fn test_wrong_base() {
    let mut pr = make_pr();
    pr.base_branch = "master".into();
    pr.ci_status = "FAILURE".into();
    assert_eq!(classify(&pr, "v8"), "WRONG-BASE");
}

#[test]
fn test_empty_rollup_returns_none() {
    assert_eq!(parse_ci(&[]), "NONE");
}

#[test]
fn test_failure_conclusion() {
    assert_eq!(
        parse_ci(&[json!({"conclusion":"FAILURE","status":"COMPLETED"})]),
        "FAILURE"
    );
}

#[test]
fn test_cancelled_is_failure() {
    assert_eq!(
        parse_ci(&[json!({"conclusion":"CANCELLED","status":"COMPLETED"})]),
        "FAILURE"
    );
}

#[test]
fn test_timed_out_is_failure() {
    assert_eq!(
        parse_ci(&[json!({"conclusion":"TIMED_OUT","status":"COMPLETED"})]),
        "FAILURE"
    );
}

#[test]
fn test_in_progress_is_pending() {
    assert_eq!(
        parse_ci(&[json!({"conclusion":null,"status":"IN_PROGRESS"})]),
        "PENDING"
    );
}

#[test]
fn test_success() {
    assert_eq!(
        parse_ci(&[json!({"conclusion":"SUCCESS","status":"COMPLETED"})]),
        "SUCCESS"
    );
}

#[test]
fn test_mixed_success_and_failure_is_failure() {
    assert_eq!(
        parse_ci(&[
            json!({"conclusion":"SUCCESS","status":"COMPLETED"}),
            json!({"conclusion":"FAILURE","status":"COMPLETED"})
        ]),
        "FAILURE"
    );
}

#[test]
fn test_exact_match() {
    assert!(path_match("src/auth/api.py", "src/auth/api.py"));
}

#[test]
fn test_graph_path_longer_with_boundary() {
    assert!(path_match("src/auth/api.py", "api.py"));
}

#[test]
fn test_no_false_positive_on_partial_filename() {
    assert!(!path_match("config.py", "g.py"));
    assert!(!path_match("g.py", "config.py"));
}

#[test]
fn test_both_directions_work() {
    assert!(path_match("api.py", "src/auth/api.py"));
    assert!(path_match("src/auth/api.py", "api.py"));
}

#[test]
fn test_matching_files_returns_correct_communities_and_count() {
    assert_eq!(
        compute_pr_impact(&["src/auth/api.py".into()], &impact_graph()),
        (vec![0], 2)
    );
}

#[test]
fn test_matching_both_files() {
    assert_eq!(
        compute_pr_impact(
            &["src/auth/api.py".into(), "src/utils/helpers.py".into()],
            &impact_graph()
        ),
        (vec![0, 1], 3)
    );
}

#[test]
fn test_empty_files_returns_empty() {
    assert_eq!(compute_pr_impact(&[], &impact_graph()), (vec![], 0));
}

#[test]
fn test_no_matching_files_returns_empty() {
    assert_eq!(
        compute_pr_impact(&["docs/README.md".into()], &impact_graph()),
        (vec![], 0)
    );
}

#[test]
fn test_no_double_counting_when_basename_matches_multiple_paths() {
    let graph = KnowledgeGraph {
        nodes: vec![
            graph_node("a1", "a1", "src/auth/api.py", Some(0)),
            graph_node("a2", "a2", "src/admin/api.py", Some(1)),
        ],
        ..KnowledgeGraph::default()
    };
    assert_eq!(
        compute_pr_impact(&["src/auth/api.py".into()], &graph),
        (vec![0], 1)
    );
}

#[test]
fn test_no_double_counting_same_graph_file_matched_by_two_pr_files() {
    assert_eq!(
        compute_pr_impact(
            &["src/auth/api.py".into(), "api.py".into()],
            &impact_graph()
        ),
        (vec![0], 2)
    );
}

const WORKTREES: &str = "worktree /home/user/proj\nHEAD abc123\nbranch refs/heads/main\n\nworktree /home/user/proj-feature\nHEAD def456\nbranch refs/heads/feature-x\n\n";

#[test]
fn test_normal_case_maps_branch_to_path() {
    assert_eq!(
        fetch_worktrees_with(&MockRunner::with([output(true, WORKTREES)])),
        BTreeMap::from([
            ("feature-x".into(), "/home/user/proj-feature".into()),
            ("main".into(), "/home/user/proj".into())
        ])
    );
}

#[test]
fn test_detached_head_does_not_leak_into_next_record() {
    let text = "worktree /home/user/detached\nHEAD abc123\ndetached\n\nworktree /home/user/proj-feature\nHEAD def456\nbranch refs/heads/feature-x\n\n";
    assert_eq!(
        fetch_worktrees_with(&MockRunner::with([output(true, text)])),
        BTreeMap::from([("feature-x".into(), "/home/user/proj-feature".into())])
    );
}

#[test]
fn test_empty_output_returns_empty_dict() {
    assert!(fetch_worktrees_with(&MockRunner::with([output(true, "")])).is_empty());
}

#[test]
fn test_nonzero_returncode_returns_empty_dict() {
    assert!(fetch_worktrees_with(&MockRunner::with([output(false, "")])).is_empty());
}

#[test]
fn test_subprocess_failure_returns_empty_dict() {
    let runner = MockRunner::with([Err(io::Error::new(io::ErrorKind::NotFound, "git"))]);
    assert!(fetch_worktrees_with(&runner).is_empty());
}

#[test]
fn test_contains_pr_metadata_and_count_header() {
    let mut first = make_pr();
    first.number = 101;
    first.title = "Add awesome feature".into();
    let mut second = make_pr();
    second.number = 102;
    second.title = "Fix flaky test".into();
    second.ci_status = "FAILURE".into();
    let mut third = make_pr();
    third.number = 103;
    third.title = "Wrong base PR".into();
    third.base_branch = "master".into();
    let text = format_prs_text(&[first, second, third], "v8");
    for expected in [
        "Open PRs targeting v8: 2",
        "(1 on wrong base, not shown)",
        "#101",
        "Add awesome feature",
        "#102",
        "Fix flaky test",
        "[READY]",
        "[CI-FAIL]",
    ] {
        assert!(text.contains(expected), "{expected}");
    }
    assert!(!text.contains("#103"));
}

#[test]
fn test_empty_pr_list() {
    let text = format_prs_text(&[], "v8");
    assert!(text.contains("Open PRs targeting v8: 0"));
    assert!(text.contains("(0 on wrong base, not shown)"));
}

#[test]
fn test_gh_returns_main() {
    let runner = MockRunner::with([output(true, r#"{"defaultBranchRef":{"name":"main"}}"#)]);
    assert_eq!(detect_default_branch_with(&runner, None), "main");
    assert_eq!(runner.calls.borrow().len(), 1);
}

#[test]
fn test_falls_back_to_git_symbolic_ref() {
    let runner = MockRunner::with([
        output(false, ""),
        output(true, "refs/remotes/origin/develop\n"),
    ]);
    assert_eq!(detect_default_branch_with(&runner, None), "develop");
}

#[test]
fn test_both_fail_returns_main() {
    let runner = MockRunner::with([output(false, ""), output(false, "")]);
    assert_eq!(detect_default_branch_with(&runner, None), "main");
}

#[test]
fn test_gh_returns_empty_dict_falls_back() {
    let runner = MockRunner::with([
        output(true, "{}"),
        output(true, "refs/remotes/origin/trunk\n"),
    ]);
    assert_eq!(detect_default_branch_with(&runner, None), "trunk");
}

#[test]
fn test_git_timeout_returns_main() {
    let runner = MockRunner::with([
        output(false, ""),
        Err(io::Error::new(io::ErrorKind::TimedOut, "git")),
    ]);
    assert_eq!(detect_default_branch_with(&runner, None), "main");
}

#[test]
fn test_basic_grouping() {
    let graph = KnowledgeGraph {
        nodes: vec![
            graph_node("a", "Alpha", "", Some(0)),
            graph_node("b", "Beta", "", Some(0)),
            graph_node("c", "Gamma", "", Some(1)),
        ],
        ..KnowledgeGraph::default()
    };
    assert_eq!(
        build_community_labels(&graph, 4),
        BTreeMap::from([
            (0, vec!["Alpha".into(), "Beta".into()]),
            (1, vec!["Gamma".into()])
        ])
    );
}

#[test]
fn test_top_n_capped() {
    let graph = KnowledgeGraph {
        nodes: (0..10)
            .map(|index| graph_node(&index.to_string(), &format!("Node{index}"), "", Some(0)))
            .collect(),
        ..KnowledgeGraph::default()
    };
    assert_eq!(build_community_labels(&graph, 4)[&0].len(), 4);
}

#[test]
fn test_no_community_field_skipped() {
    let graph = KnowledgeGraph {
        nodes: vec![graph_node("x", "X", "", None)],
        ..KnowledgeGraph::default()
    };
    assert!(build_community_labels(&graph, 4).is_empty());
}

#[test]
fn test_empty_nodes() {
    assert!(build_community_labels(&KnowledgeGraph::default(), 4).is_empty());
}

const NON_LATIN1: &str = "docs: add Persian (فارسی) 🏆";

#[test]
fn test_fixture_is_cp1252_undecodable() {
    assert!(NON_LATIN1.chars().any(|character| character as u32 > 0xff));
    assert_eq!(decode_subprocess_utf8(NON_LATIN1.as_bytes()), NON_LATIN1);
}

#[test]
fn test_gh_decodes_output_as_utf8() {
    let encoded = serde_json::to_string(&json!([{"title": NON_LATIN1}])).unwrap();
    let runner = MockRunner::with([output(true, &encoded)]);
    assert_eq!(
        gh_with(&runner, &["pr", "list"]),
        Some(json!([{"title": NON_LATIN1}]))
    );
}

#[test]
fn test_fetch_pr_files_decodes_output_as_utf8() {
    let runner = MockRunner::with([output(true, "src/café.py\n")]);
    assert_eq!(fetch_pr_files_with(&runner, 1, None), ["src/café.py"]);
}

#[test]
fn test_fetch_worktrees_decodes_output_as_utf8() {
    let runner = MockRunner::with([output(
        true,
        "worktree /home/کاربر/proj\nbranch refs/heads/ویژگی\n\n",
    )]);
    assert_eq!(fetch_worktrees_with(&runner)["ویژگی"], "/home/کاربر/proj");
}

#[test]
fn test_detect_default_branch_decodes_output_as_utf8() {
    let runner = MockRunner::with([
        output(false, ""),
        output(true, "refs/remotes/origin/ویژگی\n"),
    ]);
    assert_eq!(detect_default_branch_with(&runner, None), "ویژگی");
}
