//! Graph-aware pull-request triage helpers.

use graphoxide_core::KnowledgeGraph;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::process::Command;
use std::time::{Duration, SystemTime};

const STALE_DAYS: u64 = 14;
const STATUS_ORDER: &[&str] = &[
    "WRONG-BASE",
    "CI-FAIL",
    "CHANGES-REQ",
    "DRAFT",
    "STALE",
    "PENDING",
    "APPROVED",
    "READY",
];

#[derive(Debug, Clone)]
pub struct PrInfo {
    pub number: u64,
    pub title: String,
    pub branch: String,
    pub base_branch: String,
    pub author: String,
    pub is_draft: bool,
    pub review_decision: String,
    pub ci_status: String,
    pub updated_at: SystemTime,
    pub expected_base: String,
    pub worktree_path: Option<String>,
    pub communities_touched: Vec<i64>,
    pub nodes_affected: usize,
    pub files_changed: Vec<String>,
}

impl PrInfo {
    pub fn days_old_at(&self, now: SystemTime) -> u64 {
        now.duration_since(self.updated_at)
            .unwrap_or_default()
            .as_secs()
            / Duration::from_secs(86_400).as_secs()
    }

    pub fn status(&self, base: &str) -> &'static str {
        classify_at(self, base, SystemTime::now())
    }

    pub fn blast_radius(&self) -> String {
        if self.nodes_affected == 0 {
            return String::new();
        }
        let community_count = self.communities_touched.len();
        format!(
            "{} node{} / {} communit{}",
            self.nodes_affected,
            if self.nodes_affected == 1 { "" } else { "s" },
            community_count,
            if community_count == 1 { "y" } else { "ies" }
        )
    }
}

pub fn classify(pr: &PrInfo, base: &str) -> &'static str {
    classify_at(pr, base, SystemTime::now())
}

pub fn classify_at(pr: &PrInfo, base: &str, now: SystemTime) -> &'static str {
    if pr.base_branch != base {
        "WRONG-BASE"
    } else if pr.ci_status == "FAILURE" {
        "CI-FAIL"
    } else if pr.review_decision == "CHANGES_REQUESTED" {
        "CHANGES-REQ"
    } else if pr.is_draft {
        "DRAFT"
    } else if pr.days_old_at(now) >= STALE_DAYS {
        "STALE"
    } else if pr.review_decision == "APPROVED" {
        "APPROVED"
    } else if pr.ci_status == "PENDING" {
        "PENDING"
    } else {
        "READY"
    }
}

pub fn parse_ci(rollup: &[Value]) -> &'static str {
    if rollup.is_empty() {
        return "NONE";
    }
    let failures = [
        "FAILURE",
        "CANCELLED",
        "TIMED_OUT",
        "ACTION_REQUIRED",
        "STARTUP_FAILURE",
    ];
    if rollup.iter().any(|check| {
        check
            .get("conclusion")
            .and_then(Value::as_str)
            .is_some_and(|conclusion| failures.contains(&conclusion))
    }) {
        return "FAILURE";
    }
    if rollup.iter().any(|check| {
        matches!(
            check.get("status").and_then(Value::as_str),
            Some("IN_PROGRESS" | "QUEUED")
        )
    }) {
        return "PENDING";
    }
    if rollup
        .iter()
        .any(|check| check.get("conclusion").and_then(Value::as_str) == Some("SUCCESS"))
    {
        "SUCCESS"
    } else {
        "NONE"
    }
}

pub fn path_match(graph_source: &str, pr_file: &str) -> bool {
    let graph_source = graph_source.replace('\\', "/");
    let pr_file = pr_file.replace('\\', "/");
    graph_source == pr_file
        || graph_source.ends_with(&format!("/{pr_file}"))
        || pr_file.ends_with(&format!("/{graph_source}"))
}

pub fn compute_pr_impact(files: &[String], graph: &KnowledgeGraph) -> (Vec<i64>, usize) {
    let mut file_communities: BTreeMap<&str, BTreeSet<i64>> = BTreeMap::new();
    let mut file_counts: BTreeMap<&str, usize> = BTreeMap::new();
    for node in &graph.nodes {
        if node.source_file.is_empty() {
            continue;
        }
        if let Some(community) = node.community {
            file_communities
                .entry(&node.source_file)
                .or_default()
                .insert(community);
        } else {
            file_communities.entry(&node.source_file).or_default();
        }
        *file_counts.entry(&node.source_file).or_default() += 1;
    }
    let mut communities = BTreeSet::new();
    let mut matched = BTreeSet::new();
    let mut nodes = 0;
    for file in files {
        for (source, source_communities) in &file_communities {
            if !matched.contains(source) && path_match(source, file) {
                communities.extend(source_communities);
                nodes += file_counts[source];
                matched.insert(*source);
            }
        }
    }
    (communities.into_iter().collect(), nodes)
}

pub fn format_prs_text(prs: &[PrInfo], base: &str) -> String {
    let mut actionable: Vec<_> = prs.iter().filter(|pr| pr.base_branch == base).collect();
    let wrong = prs.len() - actionable.len();
    let now = SystemTime::now();
    actionable.sort_by_key(|pr| {
        let status = classify_at(pr, base, now);
        (
            STATUS_ORDER
                .iter()
                .position(|candidate| *candidate == status)
                .unwrap_or(usize::MAX),
            pr.days_old_at(now),
        )
    });
    let mut sections = vec![format!(
        "Open PRs targeting {base}: {}  ({wrong} on wrong base, not shown)\n",
        actionable.len()
    )];
    for pr in actionable {
        let status = classify_at(pr, base, now);
        let blast_radius = pr.blast_radius();
        let impact = if blast_radius.is_empty() {
            String::new()
        } else {
            format!("  blast_radius={blast_radius}")
        };
        sections.push(format!(
            "#{} [{status}] CI={} review={} age={}d author={}{}\n  {}",
            pr.number,
            pr.ci_status,
            if pr.review_decision.is_empty() {
                "none"
            } else {
                &pr.review_decision
            },
            pr.days_old_at(now),
            pr.author,
            impact,
            pr.title
        ));
    }
    sections.join("\n\n")
}

pub fn build_community_labels(graph: &KnowledgeGraph, top_n: usize) -> BTreeMap<i64, Vec<String>> {
    let mut labels: BTreeMap<i64, Vec<String>> = BTreeMap::new();
    for node in &graph.nodes {
        let Some(community) = node.community else {
            continue;
        };
        let label = if node.label.is_empty() {
            &node.id
        } else {
            &node.label
        };
        if !label.is_empty() && labels.entry(community).or_default().len() < top_n {
            labels.get_mut(&community).unwrap().push(label.clone());
        }
    }
    labels
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub success: bool,
    pub stdout: Vec<u8>,
}

pub trait CommandRunner {
    fn run(&self, program: &str, arguments: &[&str]) -> io::Result<CommandOutput>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemCommandRunner;

impl CommandRunner for SystemCommandRunner {
    fn run(&self, program: &str, arguments: &[&str]) -> io::Result<CommandOutput> {
        let output = Command::new(program).args(arguments).output()?;
        Ok(CommandOutput {
            success: output.status.success(),
            stdout: output.stdout,
        })
    }
}

/// Decode command output using UTF-8 regardless of the host locale. Invalid
/// bytes are replaced, matching the upstream subprocess policy.
pub fn decode_subprocess_utf8(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

pub fn gh_with(runner: &impl CommandRunner, arguments: &[&str]) -> Option<Value> {
    let output = runner.run("gh", arguments).ok()?;
    if !output.success {
        return None;
    }
    serde_json::from_str(&decode_subprocess_utf8(&output.stdout)).ok()
}

pub fn fetch_pr_files_with(
    runner: &impl CommandRunner,
    number: u64,
    repo: Option<&str>,
) -> Vec<String> {
    let number = number.to_string();
    let mut arguments = vec!["pr", "diff", number.as_str(), "--name-only"];
    if let Some(repo) = repo {
        arguments.extend(["--repo", repo]);
    }
    let Ok(output) = runner.run("gh", &arguments) else {
        return Vec::new();
    };
    if !output.success {
        return Vec::new();
    }
    decode_subprocess_utf8(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

pub fn fetch_worktrees_with(runner: &impl CommandRunner) -> BTreeMap<String, String> {
    let Ok(output) = runner.run("git", &["worktree", "list", "--porcelain"]) else {
        return BTreeMap::new();
    };
    if !output.success {
        return BTreeMap::new();
    }
    parse_worktrees(&decode_subprocess_utf8(&output.stdout))
}

pub fn parse_worktrees(output: &str) -> BTreeMap<String, String> {
    let mut mapping = BTreeMap::new();
    let mut current_path = None;
    for line in output.lines() {
        if line.is_empty() {
            current_path = None;
        } else if let Some(path) = line.strip_prefix("worktree ") {
            current_path = Some(path.to_owned());
        } else if let (Some(branch), Some(path)) = (
            line.strip_prefix("branch refs/heads/"),
            current_path.as_ref(),
        ) {
            mapping.insert(branch.to_owned(), path.clone());
        }
    }
    mapping
}

pub fn detect_default_branch_with(runner: &impl CommandRunner, repo: Option<&str>) -> String {
    let mut gh_arguments = vec!["repo", "view", "--json", "defaultBranchRef"];
    if let Some(repo) = repo {
        gh_arguments.extend(["--repo", repo]);
    }
    if let Some(branch) = gh_with(runner, &gh_arguments)
        .and_then(|value| value.get("defaultBranchRef").cloned())
        .and_then(|value| value.get("name").and_then(Value::as_str).map(str::to_owned))
        .filter(|branch| !branch.is_empty())
    {
        return branch;
    }
    let Ok(output) = runner.run("git", &["symbolic-ref", "refs/remotes/origin/HEAD"]) else {
        return "main".into();
    };
    if output.success {
        let reference = decode_subprocess_utf8(&output.stdout);
        if let Some(branch) = reference
            .trim()
            .rsplit('/')
            .next()
            .filter(|value| !value.is_empty())
        {
            return branch.to_owned();
        }
    }
    "main".into()
}
