//! Deterministic work-memory reflection and its derived learning sidecar.
//!
//! This is the Rust port of Graphify's `reflect.py`: outcome-tagged Q&A documents
//! are scored with time decay and corroboration, rendered into `LESSONS.md`, and
//! optionally projected onto canonical graph node IDs without mutating graph.json.

use anyhow::{Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub const LEARNING_SIDECAR_NAME: &str = ".graphify_learning.json";
const LEARNING_SCHEMA_VERSION: u32 = 1;
const PROVENANCE_CAP: usize = 5;
const UNCATEGORIZED: &str = "Uncategorized";
const OUTCOMES: [&str; 3] = ["useful", "dead_end", "corrected"];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryDoc {
    pub query_type: String,
    pub date: String,
    pub question: String,
    pub outcome: Option<String>,
    pub correction: String,
    pub contributor: String,
    pub source_nodes: Vec<String>,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SourceLesson {
    pub node: String,
    pub n: usize,
    pub score: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContestedLesson {
    pub node: String,
    pub pos: usize,
    pub neg: usize,
    pub score: f64,
    pub verdict: String,
    pub last: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeadEndLesson {
    pub question: String,
    pub nodes: Vec<String>,
    pub date: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorrectionLesson {
    pub question: String,
    pub correction: String,
    pub date: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutcomeCounts {
    pub useful: usize,
    pub dead_end: usize,
    pub corrected: usize,
    pub unmarked: usize,
}

impl OutcomeCounts {
    fn record(&mut self, outcome: Option<&str>) {
        match outcome {
            Some("useful") => self.useful += 1,
            Some("dead_end") => self.dead_end += 1,
            Some("corrected") => self.corrected += 1,
            _ => self.unmarked += 1,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LessonBucket {
    pub counts: OutcomeCounts,
    pub preferred: Vec<SourceLesson>,
    pub tentative: Vec<SourceLesson>,
    pub contested: Vec<ContestedLesson>,
    pub dead_ends: Vec<DeadEndLesson>,
    pub corrections: Vec<CorrectionLesson>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LessonAggregate {
    pub total: usize,
    pub counts: OutcomeCounts,
    pub min_corroboration: usize,
    pub preferred: Vec<SourceLesson>,
    pub tentative: Vec<SourceLesson>,
    pub contested: Vec<ContestedLesson>,
    pub dead_ends: Vec<DeadEndLesson>,
    pub corrections: Vec<CorrectionLesson>,
    pub by_community: BTreeMap<String, LessonBucket>,
    node_provenance: BTreeMap<String, Vec<ProvenanceEvent>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProvenanceEvent {
    date: String,
    question: String,
    outcome: String,
}

#[derive(Debug, Clone)]
pub struct SaveResultOptions {
    pub query_type: String,
    pub source_nodes: Vec<String>,
    pub outcome: Option<String>,
    pub correction: Option<String>,
    pub now: Option<DateTime<Utc>>,
}

impl Default for SaveResultOptions {
    fn default() -> Self {
        Self {
            query_type: "query".into(),
            source_nodes: Vec::new(),
            outcome: None,
            correction: None,
            now: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReflectOptions {
    pub graph_path: Option<PathBuf>,
    pub analysis_path: Option<PathBuf>,
    pub labels_path: Option<PathBuf>,
    pub now: Option<DateTime<Utc>>,
    pub half_life_days: f64,
    pub min_corroboration: usize,
}

impl Default for ReflectOptions {
    fn default() -> Self {
        Self {
            graph_path: None,
            analysis_path: None,
            labels_path: None,
            now: None,
            half_life_days: 30.0,
            min_corroboration: 2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LearningProvenance {
    pub q: String,
    pub date: String,
    pub outcome: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LearningEntry {
    pub status: String,
    pub score: f64,
    pub uses: usize,
    pub last: String,
    pub label: String,
    pub source_file: String,
    pub code_fingerprint: String,
    pub provenance: Vec<LearningProvenance>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verdict: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub neg: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LearningSidecar {
    pub version: u32,
    pub generated_at: String,
    pub nodes: BTreeMap<String, LearningEntry>,
}

/// Escape a value for the deliberately small YAML double-quoted subset emitted
/// by [`save_query_result`].
fn yaml_escape(value: &str) -> String {
    let mut output = String::new();
    for character in value.chars() {
        match character {
            '\\' => output.push_str("\\\\"),
            '"' => output.push_str("\\\""),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            '\0' => output.push_str("\\0"),
            '\u{2028}' => output.push_str("\\L"),
            '\u{2029}' => output.push_str("\\P"),
            c if (c as u32) < 0x20 || c == '\u{7f}' => {
                output.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => output.push(c),
        }
    }
    output
}

fn yaml_unescape(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    let mut output = String::new();
    let mut index = 0;
    while index < chars.len() {
        if chars[index] != '\\' || index + 1 >= chars.len() {
            output.push(chars[index]);
            index += 1;
            continue;
        }
        let escape = chars[index + 1];
        let simple = match escape {
            'n' => Some('\n'),
            'r' => Some('\r'),
            't' => Some('\t'),
            '0' => Some('\0'),
            '"' => Some('"'),
            '\\' => Some('\\'),
            'L' => Some('\u{2028}'),
            'P' => Some('\u{2029}'),
            _ => None,
        };
        if let Some(character) = simple {
            output.push(character);
            index += 2;
            continue;
        }
        let digits = if escape == 'x' {
            2
        } else if escape == 'u' {
            4
        } else {
            0
        };
        if digits > 0 && index + 2 + digits <= chars.len() {
            let encoded: String = chars[index + 2..index + 2 + digits].iter().collect();
            if let Ok(codepoint) = u32::from_str_radix(&encoded, 16)
                && let Some(character) = char::from_u32(codepoint)
            {
                output.push(character);
                index += 2 + digits;
                continue;
            }
        }
        output.push('\\');
        index += 1;
    }
    output
}

fn quoted_scalar(line: &str) -> Option<(&str, String)> {
    let (key, value) = line.split_once(':')?;
    let value = value.trim();
    if value.len() < 2 || !value.starts_with('"') || !value.ends_with('"') {
        return None;
    }
    Some((key, yaml_unescape(&value[1..value.len() - 1])))
}

fn quoted_list(value: &str) -> Vec<String> {
    let Some((_, tail)) = value.split_once('[') else {
        return Vec::new();
    };
    let Some(body) = tail.rsplit_once(']').map(|(body, _)| body) else {
        return Vec::new();
    };
    let chars: Vec<char> = body.chars().collect();
    let mut values = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        if chars[index] != '"' {
            index += 1;
            continue;
        }
        index += 1;
        let mut encoded = String::new();
        while index < chars.len() {
            match chars[index] {
                '"' => {
                    index += 1;
                    break;
                }
                '\\' if index + 1 < chars.len() => {
                    encoded.push('\\');
                    encoded.push(chars[index + 1]);
                    index += 2;
                }
                character => {
                    encoded.push(character);
                    index += 1;
                }
            }
        }
        values.push(yaml_unescape(&encoded));
    }
    values
}

/// Parse the frontmatter of a saved memory document. Plain Markdown returns
/// `None`, which keeps generated lessons from feeding themselves back in.
pub fn parse_memory_doc(text: &str) -> Option<MemoryDoc> {
    if !text.starts_with("---") {
        return None;
    }
    let mut lines = text.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    let mut doc = MemoryDoc::default();
    for line in lines {
        if line.trim() == "---" {
            break;
        }
        if line.starts_with("source_nodes:") {
            doc.source_nodes = quoted_list(line);
            continue;
        }
        let Some((key, value)) = quoted_scalar(line) else {
            continue;
        };
        match key {
            "type" => doc.query_type = value,
            "date" => doc.date = value,
            "question" => doc.question = value,
            "outcome" => doc.outcome = Some(value),
            "correction" => doc.correction = value,
            "contributor" => doc.contributor = value,
            _ => {}
        }
    }
    Some(doc)
}

/// Read direct `*.md` children, skip foreign/unreadable documents, and sort by
/// `(date, filename)` for deterministic aggregation.
pub fn load_memory_docs(memory_dir: &Path) -> Vec<MemoryDoc> {
    let Ok(entries) = fs::read_dir(memory_dir) else {
        return Vec::new();
    };
    let mut paths = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|v| v.to_str()) == Some("md"))
        .collect::<Vec<_>>();
    paths.sort();
    let mut docs = Vec::new();
    for path in paths {
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let Some(mut doc) = parse_memory_doc(&text) else {
            continue;
        };
        doc.path = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_owned();
        docs.push(doc);
    }
    docs.sort_by(|left, right| (&left.date, &left.path).cmp(&(&right.date, &right.path)));
    docs
}

/// Save one Q&A result as extraction-friendly Markdown with injection-safe YAML
/// frontmatter.
pub fn save_query_result(
    question: &str,
    answer: &str,
    memory_dir: &Path,
    options: &SaveResultOptions,
) -> Result<PathBuf> {
    if let Some(outcome) = options.outcome.as_deref()
        && !OUTCOMES.contains(&outcome)
    {
        anyhow::bail!("outcome must be one of useful, dead_end, corrected, got {outcome:?}");
    }
    fs::create_dir_all(memory_dir)
        .with_context(|| format!("create memory directory {}", memory_dir.display()))?;
    let now = options.now.unwrap_or_else(Utc::now);
    let mut slug = String::new();
    for character in question.to_lowercase().chars().take(50) {
        slug.push(if character.is_alphanumeric() || character == '_' {
            character
        } else {
            '_'
        });
    }
    let slug = slug.trim_matches('_');
    let filename = format!("query_{}_{}.md", now.format("%Y%m%d_%H%M%S"), slug);
    let correction = options
        .correction
        .as_deref()
        .filter(|value| !value.is_empty());
    let mut lines = vec![
        "---".into(),
        format!("type: \"{}\"", yaml_escape(&options.query_type)),
        format!(
            "date: \"{}\"",
            now.to_rfc3339_opts(SecondsFormat::AutoSi, false)
        ),
        format!("question: \"{}\"", yaml_escape(question)),
        "contributor: \"graphoxide\"".into(),
    ];
    if let Some(outcome) = options.outcome.as_deref() {
        lines.push(format!("outcome: \"{}\"", yaml_escape(outcome)));
    }
    if let Some(correction) = correction {
        lines.push(format!("correction: \"{}\"", yaml_escape(correction)));
    }
    if !options.source_nodes.is_empty() {
        let nodes = options
            .source_nodes
            .iter()
            .take(10)
            .map(|node| format!("\"{}\"", yaml_escape(node)))
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(format!("source_nodes: [{nodes}]"));
    }
    lines.extend([
        "---".into(),
        String::new(),
        format!("# Q: {question}"),
        String::new(),
        "## Answer".into(),
        String::new(),
        answer.into(),
    ]);
    if options.outcome.is_some() || correction.is_some() {
        lines.extend([String::new(), "## Outcome".into(), String::new()]);
        if let Some(outcome) = options.outcome.as_deref() {
            lines.push(format!("- Signal: {outcome}"));
        }
        if let Some(correction) = correction {
            lines.push(format!("- Correction: {correction}"));
        }
    }
    if !options.source_nodes.is_empty() {
        lines.extend([String::new(), "## Source Nodes".into(), String::new()]);
        lines.extend(options.source_nodes.iter().map(|node| format!("- {node}")));
    }
    let output = memory_dir.join(filename);
    fs::write(&output, lines.join("\n"))
        .with_context(|| format!("write memory document {}", output.display()))?;
    Ok(output)
}

#[derive(Default)]
struct RunningBucket {
    counts: OutcomeCounts,
    score: BTreeMap<String, f64>,
    pos: BTreeMap<String, usize>,
    neg: BTreeMap<String, usize>,
    last: BTreeMap<String, String>,
    provenance: BTreeMap<String, Vec<ProvenanceEvent>>,
    dead_ends: Vec<DeadEndLesson>,
    corrections: Vec<CorrectionLesson>,
}

fn parse_date(value: &str) -> Option<DateTime<Utc>> {
    if value.is_empty() {
        return None;
    }
    DateTime::parse_from_rfc3339(value)
        .map(|date| date.with_timezone(&Utc))
        .ok()
        .or_else(|| {
            chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")
                .ok()?
                .and_hms_opt(0, 0, 0)
                .map(|date| date.and_utc())
        })
}

fn decay(date: &str, now: DateTime<Utc>, half_life_days: f64) -> f64 {
    if half_life_days <= 0.0 {
        return 1.0;
    }
    let Some(date) = parse_date(date) else {
        return 1.0;
    };
    let age_days = ((now - date).num_milliseconds() as f64 / 86_400_000.0).max(0.0);
    0.5_f64.powf(age_days / half_life_days)
}

fn document_community(
    nodes: &[String],
    node_community: Option<&BTreeMap<String, String>>,
) -> String {
    let Some(mapping) = node_community.filter(|mapping| !mapping.is_empty()) else {
        return UNCATEGORIZED.into();
    };
    let mut counts = BTreeMap::<String, usize>::new();
    for label in nodes.iter().filter_map(|node| mapping.get(node)) {
        *counts.entry(label.clone()).or_default() += 1;
    }
    counts
        .into_iter()
        .min_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)))
        .map(|(label, _)| label)
        .unwrap_or_else(|| UNCATEGORIZED.into())
}

fn record_node(bucket: &mut RunningBucket, node: &str, sign: i8, weight: f64, doc: &MemoryDoc) {
    *bucket.score.entry(node.into()).or_default() += f64::from(sign) * weight;
    if sign > 0 {
        *bucket.pos.entry(node.into()).or_default() += 1;
    } else if sign < 0 {
        *bucket.neg.entry(node.into()).or_default() += 1;
    }
    let last = bucket.last.entry(node.into()).or_default();
    if doc.date > *last {
        *last = doc.date.clone();
    }
    if matches!(doc.outcome.as_deref(), Some("useful" | "corrected")) {
        bucket
            .provenance
            .entry(node.into())
            .or_default()
            .push(ProvenanceEvent {
                date: doc.date.clone(),
                question: doc.question.clone(),
                outcome: doc.outcome.clone().unwrap_or_default(),
            });
    }
}

fn rounded_score(score: f64) -> f64 {
    (score * 1_000_000_000.0).round() / 1_000_000_000.0
}

fn finalize_sources(
    bucket: &RunningBucket,
    min_corroboration: usize,
) -> (Vec<SourceLesson>, Vec<SourceLesson>, Vec<ContestedLesson>) {
    let mut preferred = Vec::new();
    let mut tentative = Vec::new();
    let mut contested = Vec::new();
    for (node, raw_score) in &bucket.score {
        let pos = bucket.pos.get(node).copied().unwrap_or(0);
        let neg = bucket.neg.get(node).copied().unwrap_or(0);
        let score = rounded_score(*raw_score);
        if pos > 0 && neg > 0 {
            contested.push(ContestedLesson {
                node: node.clone(),
                pos,
                neg,
                score,
                verdict: if score > 0.0 {
                    "useful"
                } else if score < 0.0 {
                    "dead end"
                } else {
                    "even"
                }
                .into(),
                last: bucket.last.get(node).cloned().unwrap_or_default(),
            });
        } else if pos > 0 {
            let entry = SourceLesson {
                node: node.clone(),
                n: pos,
                score,
            };
            if pos >= min_corroboration {
                preferred.push(entry);
            } else {
                tentative.push(entry);
            }
        }
    }
    let source_order = |left: &SourceLesson, right: &SourceLesson| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.node.cmp(&right.node))
    };
    preferred.sort_by(source_order);
    tentative.sort_by(source_order);
    contested.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.node.cmp(&right.node))
    });
    (preferred, tentative, contested)
}

fn dedupe_dead_ends(items: &[DeadEndLesson]) -> Vec<DeadEndLesson> {
    let mut latest = BTreeMap::new();
    for item in items {
        latest.insert(item.question.clone(), item.clone());
    }
    let mut output = latest.into_values().collect::<Vec<_>>();
    output.sort_by(|left, right| (&left.date, &left.question).cmp(&(&right.date, &right.question)));
    output
}

fn dedupe_corrections(items: &[CorrectionLesson]) -> Vec<CorrectionLesson> {
    let mut latest = BTreeMap::new();
    for item in items {
        latest.insert(item.question.clone(), item.clone());
    }
    let mut output = latest.into_values().collect::<Vec<_>>();
    output.sort_by(|left, right| (&left.date, &left.question).cmp(&(&right.date, &right.question)));
    output
}

fn public_bucket(bucket: &RunningBucket, min_corroboration: usize) -> LessonBucket {
    let (preferred, tentative, contested) = finalize_sources(bucket, min_corroboration);
    LessonBucket {
        counts: bucket.counts.clone(),
        preferred,
        tentative,
        contested,
        dead_ends: dedupe_dead_ends(&bucket.dead_ends),
        corrections: dedupe_corrections(&bucket.corrections),
    }
}

/// Aggregate memory outcomes into deterministic source, dead-end, correction,
/// and optional per-community lesson buckets.
pub fn aggregate_lessons(
    docs: &[MemoryDoc],
    node_community: Option<&BTreeMap<String, String>>,
    now: DateTime<Utc>,
    half_life_days: f64,
    min_corroboration: usize,
    known_nodes: Option<&BTreeSet<String>>,
) -> LessonAggregate {
    let mut overall = RunningBucket::default();
    let mut communities = BTreeMap::<String, RunningBucket>::new();
    for doc in docs {
        let mut seen = HashSet::new();
        let nodes = doc
            .source_nodes
            .iter()
            .filter(|node| seen.insert((*node).clone()))
            .filter(|node| known_nodes.is_none_or(|known| known.contains(*node)))
            .cloned()
            .collect::<Vec<_>>();
        let community = document_community(&nodes, node_community);
        let bucket = communities.entry(community).or_default();
        let sign = match doc.outcome.as_deref() {
            Some("useful") => 1,
            Some("dead_end" | "corrected") => -1,
            _ => 0,
        };
        let weight = if sign == 0 {
            0.0
        } else {
            decay(&doc.date, now, half_life_days)
        };
        for target in [&mut overall, bucket] {
            target.counts.record(doc.outcome.as_deref());
            if sign != 0 {
                for node in &nodes {
                    record_node(target, node, sign, weight, doc);
                }
            }
            match doc.outcome.as_deref() {
                Some("dead_end") => target.dead_ends.push(DeadEndLesson {
                    question: doc.question.clone(),
                    nodes: nodes.clone(),
                    date: doc.date.clone(),
                }),
                Some("corrected") => target.corrections.push(CorrectionLesson {
                    question: doc.question.clone(),
                    correction: doc.correction.clone(),
                    date: doc.date.clone(),
                }),
                _ => {}
            }
        }
    }
    let (preferred, tentative, contested) = finalize_sources(&overall, min_corroboration);
    let by_community = node_community
        .filter(|mapping| !mapping.is_empty())
        .map(|_| {
            communities
                .iter()
                .map(|(label, bucket)| (label.clone(), public_bucket(bucket, min_corroboration)))
                .collect()
        })
        .unwrap_or_default();
    LessonAggregate {
        total: docs.len(),
        counts: overall.counts,
        min_corroboration,
        preferred,
        tentative,
        contested,
        dead_ends: dedupe_dead_ends(&overall.dead_ends),
        corrections: dedupe_corrections(&overall.corrections),
        by_community,
        node_provenance: overall.provenance,
    }
}

fn render_bucket(output: &mut Vec<String>, bucket: &LessonBucket, corroboration: usize) {
    if !bucket.preferred.is_empty() {
        output.extend([
            format!(
                "**Preferred sources** — corroborated by ≥{corroboration} useful results; start here."
            ),
            String::new(),
        ]);
        output.extend(
            bucket
                .preferred
                .iter()
                .map(|entry| format!("- `{}` ({}× useful)", entry.node, entry.n)),
        );
        output.push(String::new());
    }
    if !bucket.tentative.is_empty() {
        output.extend([
            format!(
                "**Tentative** — useful in fewer than {corroboration} results; verify before relying."
            ),
            String::new(),
        ]);
        output.extend(
            bucket
                .tentative
                .iter()
                .map(|entry| format!("- `{}` ({}× useful)", entry.node, entry.n)),
        );
        output.push(String::new());
    }
    if !bucket.contested.is_empty() {
        output.extend([
            "**Contested** — mixed signals; recency decides.".into(),
            String::new(),
        ]);
        for entry in &bucket.contested {
            let verdict = if entry.verdict == "even" {
                "evenly split".into()
            } else {
                format!("recency leans **{}**", entry.verdict)
            };
            let latest = entry.last.get(..10).filter(|_| entry.last.len() >= 10);
            output.push(format!(
                "- `{}` — {}× useful, {}× dead end/corrected → {}{}",
                entry.node,
                entry.pos,
                entry.neg,
                verdict,
                latest.map_or_else(String::new, |day| format!(" (latest {day})"))
            ));
        }
        output.push(String::new());
    }
    if !bucket.dead_ends.is_empty() {
        output.extend([
            "**Known dead ends** — led nowhere; don't re-derive.".into(),
            String::new(),
        ]);
        for entry in &bucket.dead_ends {
            let nodes = entry
                .nodes
                .iter()
                .map(|node| format!("`{node}`"))
                .collect::<Vec<_>>()
                .join(", ");
            output.push(if nodes.is_empty() {
                format!("- \"{}\"", entry.question)
            } else {
                format!("- \"{}\" — {nodes}", entry.question)
            });
        }
        output.push(String::new());
    }
    if !bucket.corrections.is_empty() {
        output.extend([
            "**Corrections** — do these differently.".into(),
            String::new(),
        ]);
        output.extend(
            bucket
                .corrections
                .iter()
                .map(|entry| format!("- \"{}\" → {}", entry.question, entry.correction)),
        );
        output.push(String::new());
    }
    if bucket.preferred.is_empty()
        && bucket.tentative.is_empty()
        && bucket.contested.is_empty()
        && bucket.dead_ends.is_empty()
        && bucket.corrections.is_empty()
    {
        output.extend(["_No marked outcomes yet._".into(), String::new()]);
    }
}

/// Render a byte-stable lessons document.
pub fn render_lessons_md(aggregate: &LessonAggregate) -> String {
    let counts = &aggregate.counts;
    let mut output = vec![
        "# Lessons".into(),
        String::new(),
        format!(
            "_Auto-generated by `graphoxide reflect` from {} session {} in graphoxide-out/memory/. Deterministic; no LLM. Use for orientation — verify before relying, and revisit dead ends if the code has changed since._",
            aggregate.total,
            if aggregate.total == 1 { "memory" } else { "memories" }
        ),
        String::new(),
        "## Summary".into(),
        String::new(),
        format!(
            "- {} useful · {} dead ends · {} corrected · {} unmarked",
            counts.useful, counts.dead_end, counts.corrected, counts.unmarked
        ),
        String::new(),
        "## Lessons".into(),
        String::new(),
    ];
    let overall = LessonBucket {
        counts: aggregate.counts.clone(),
        preferred: aggregate.preferred.clone(),
        tentative: aggregate.tentative.clone(),
        contested: aggregate.contested.clone(),
        dead_ends: aggregate.dead_ends.clone(),
        corrections: aggregate.corrections.clone(),
    };
    render_bucket(&mut output, &overall, aggregate.min_corroboration);
    if !aggregate.by_community.is_empty() {
        output.extend(["## By topic".into(), String::new()]);
        let mut labels = aggregate.by_community.keys().collect::<Vec<_>>();
        labels.sort_by(|left, right| {
            (*left == UNCATEGORIZED)
                .cmp(&(*right == UNCATEGORIZED))
                .then_with(|| left.cmp(right))
        });
        for label in labels {
            output.extend([format!("### {label}"), String::new()]);
            render_bucket(
                &mut output,
                &aggregate.by_community[label],
                aggregate.min_corroboration,
            );
        }
    }
    format!("{}\n", output.join("\n").trim_end_matches('\n'))
}

fn file_modified(path: &Path) -> Option<SystemTime> {
    path.metadata()
        .and_then(|metadata| metadata.modified())
        .ok()
}

/// Whether an existing lessons output is at least as new as every memory and
/// graph-sidecar input.
pub fn lessons_fresh(
    out_path: &Path,
    memory_dir: &Path,
    graph_path: Option<&Path>,
    analysis_path: Option<&Path>,
    labels_path: Option<&Path>,
) -> bool {
    let Some(output_time) = file_modified(out_path) else {
        return false;
    };
    let mut inputs = Vec::new();
    if let Ok(entries) = fs::read_dir(memory_dir) {
        inputs.extend(
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.extension().and_then(|v| v.to_str()) == Some("md")),
        );
    }
    inputs.extend(
        [graph_path, analysis_path, labels_path]
            .into_iter()
            .flatten()
            .map(Path::to_path_buf),
    );
    inputs
        .iter()
        .filter_map(|path| file_modified(path))
        .all(|input_time| output_time >= input_time)
}

fn load_known_nodes(graph_path: &Path) -> Option<BTreeSet<String>> {
    let data: Value = serde_json::from_slice(&fs::read(graph_path).ok()?).ok()?;
    let mut known = BTreeSet::new();
    for node in data.get("nodes")?.as_array()? {
        let Some(node) = node.as_object() else {
            continue;
        };
        for key in ["id", "label"] {
            if let Some(value) = node.get(key).and_then(Value::as_str) {
                known.insert(value.into());
            }
        }
    }
    (!known.is_empty()).then_some(known)
}

fn load_node_community(
    graph_path: &Path,
    analysis_path: &Path,
    labels_path: &Path,
) -> Option<BTreeMap<String, String>> {
    let analysis: Value = serde_json::from_slice(&fs::read(analysis_path).ok()?).ok()?;
    let communities = analysis.get("communities")?.as_object()?;
    if communities.is_empty() {
        return None;
    }
    let labels: BTreeMap<String, String> = fs::read(labels_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|value| value.as_object().cloned())
        .map(|object| {
            object
                .into_iter()
                .filter_map(|(key, value)| value.as_str().map(|value| (key, value.into())))
                .collect()
        })
        .unwrap_or_default();
    let graph: Value = serde_json::from_slice(&fs::read(graph_path).ok()?).unwrap_or(Value::Null);
    let id_to_label = graph
        .get("nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|node| {
            Some((
                node.get("id")?.as_str()?.to_owned(),
                node.get("label")?.as_str()?.to_owned(),
            ))
        })
        .collect::<BTreeMap<_, _>>();
    let mut result = BTreeMap::new();
    for (community, members) in communities {
        let label = labels
            .get(community)
            .cloned()
            .unwrap_or_else(|| format!("Community {community}"));
        for member in members.as_array().into_iter().flatten() {
            let member = member
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| member.to_string());
            result
                .entry(member.clone())
                .or_insert_with(|| label.clone());
            if let Some(node_label) = id_to_label.get(&member) {
                result
                    .entry(node_label.clone())
                    .or_insert_with(|| label.clone());
            }
        }
    }
    Some(result)
}

/// Build `LESSONS.md`, optionally filter/group against a graph, and best-effort
/// write the derived learning sidecar next to that graph.
pub fn reflect(
    memory_dir: &Path,
    out_path: &Path,
    options: &ReflectOptions,
) -> Result<(PathBuf, LessonAggregate)> {
    let docs = load_memory_docs(memory_dir);
    let now = options.now.unwrap_or_else(Utc::now);
    let (node_community, known_nodes) = if let Some(graph_path) = &options.graph_path {
        let analysis = options.analysis_path.clone().unwrap_or_else(|| {
            graph_path
                .parent()
                .unwrap_or(Path::new("."))
                .join(".graphify_analysis.json")
        });
        let labels = options.labels_path.clone().unwrap_or_else(|| {
            graph_path
                .parent()
                .unwrap_or(Path::new("."))
                .join(".graphify_labels.json")
        });
        (
            load_node_community(graph_path, &analysis, &labels),
            load_known_nodes(graph_path),
        )
    } else {
        (None, None)
    };
    let aggregate = aggregate_lessons(
        &docs,
        node_community.as_ref(),
        now,
        options.half_life_days,
        options.min_corroboration,
        known_nodes.as_ref(),
    );
    if let Some(parent) = out_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(out_path, render_lessons_md(&aggregate))?;
    if let Some(graph_path) = &options.graph_path {
        let _ = write_learning_sidecar(&aggregate, graph_path, now);
    }
    Ok((out_path.to_path_buf(), aggregate))
}

type GraphIdentityMaps = (
    BTreeSet<String>,
    BTreeMap<String, Vec<String>>,
    BTreeMap<String, Value>,
);

fn build_id_label_maps(graph_path: &Path) -> GraphIdentityMaps {
    let Ok(bytes) = fs::read(graph_path) else {
        return Default::default();
    };
    let Ok(data) = serde_json::from_slice::<Value>(&bytes) else {
        return Default::default();
    };
    let mut ids = BTreeSet::new();
    let mut label_to_ids = BTreeMap::<String, Vec<String>>::new();
    let mut nodes = BTreeMap::new();
    for node in data
        .get("nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(id) = node.get("id").and_then(Value::as_str) else {
            continue;
        };
        ids.insert(id.into());
        nodes.insert(id.into(), node.clone());
        if let Some(label) = node.get("label").and_then(Value::as_str) {
            label_to_ids
                .entry(label.into())
                .or_default()
                .push(id.into());
        }
    }
    (ids, label_to_ids, nodes)
}

fn canonical_id(
    cited: &str,
    ids: &BTreeSet<String>,
    label_to_ids: &BTreeMap<String, Vec<String>>,
) -> Option<String> {
    if ids.contains(cited) {
        return Some(cited.into());
    }
    let matches = label_to_ids.get(cited)?;
    (matches.len() == 1).then(|| matches[0].clone())
}

fn resolve_source_path(source: &str, graph_path: &Path) -> Option<PathBuf> {
    if source.is_empty() {
        return None;
    }
    let source = Path::new(source);
    if source.is_absolute() {
        return source.is_file().then(|| source.to_path_buf());
    }
    let output = graph_path.parent().unwrap_or(Path::new("."));
    let mut candidates = Vec::new();
    for marker in [".graphify_root", ".graphoxide_root"] {
        if let Ok(root) = fs::read_to_string(output.join(marker)) {
            let root = root.trim();
            if !root.is_empty() {
                candidates.push(PathBuf::from(root));
            }
        }
    }
    if matches!(
        output.file_name().and_then(|name| name.to_str()),
        Some("graphify-out" | "graphoxide-out")
    ) {
        if let Some(parent) = output.parent() {
            candidates.push(parent.into());
        }
        candidates.push(output.into());
    } else {
        candidates.push(output.into());
        if let Some(parent) = output.parent() {
            candidates.push(parent.into());
        }
    }
    candidates.push(PathBuf::from("."));
    let mut seen = BTreeSet::new();
    candidates
        .into_iter()
        .filter(|base| seen.insert(base.clone()))
        .map(|base| base.join(source))
        .find(|candidate| candidate.is_file())
}

fn content_hash(path: &Path) -> String {
    fs::read(path)
        .map(|bytes| hex::encode(Sha256::digest(bytes)))
        .unwrap_or_default()
}

fn code_fingerprint(node: Option<&Value>, graph_path: &Path) -> String {
    node.and_then(|node| node.get("source_file"))
        .and_then(Value::as_str)
        .and_then(|source| resolve_source_path(source, graph_path))
        .map(|path| content_hash(&path))
        .unwrap_or_default()
}

fn provenance_for(aggregate: &LessonAggregate, cited: &str) -> Vec<LearningProvenance> {
    let mut events = aggregate
        .node_provenance
        .get(cited)
        .cloned()
        .unwrap_or_default();
    events.sort_by(|left, right| (&right.date, &right.question).cmp(&(&left.date, &left.question)));
    events
        .into_iter()
        .take(PROVENANCE_CAP)
        .map(|event| LearningProvenance {
            q: event.question,
            date: event.date,
            outcome: event.outcome,
        })
        .collect()
}

/// Project aggregate source verdicts to unambiguous canonical node IDs.
pub fn build_learning_overlay(
    aggregate: &LessonAggregate,
    graph_path: &Path,
    now: DateTime<Utc>,
) -> LearningSidecar {
    let (ids, label_to_ids, nodes) = build_id_label_maps(graph_path);
    let mut output = BTreeMap::new();
    let mut add = |cited: &str,
                   status: &str,
                   score: f64,
                   uses: usize,
                   last: &str,
                   verdict: Option<&str>,
                   neg: Option<usize>| {
        let Some(id) = canonical_id(cited, &ids, &label_to_ids) else {
            return;
        };
        if output.contains_key(&id) {
            return;
        }
        let node = nodes.get(&id);
        let provenance = provenance_for(aggregate, cited);
        let last = if last.is_empty() {
            provenance
                .first()
                .map(|entry| entry.date.clone())
                .unwrap_or_default()
        } else {
            last.into()
        };
        output.insert(
            id,
            LearningEntry {
                status: status.into(),
                score,
                uses,
                last,
                label: node
                    .and_then(|node| node.get("label"))
                    .and_then(Value::as_str)
                    .unwrap_or(cited)
                    .into(),
                source_file: node
                    .and_then(|node| node.get("source_file"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .into(),
                code_fingerprint: code_fingerprint(node, graph_path),
                provenance,
                verdict: verdict.map(str::to_owned),
                neg,
                stale: None,
            },
        );
    };
    for entry in &aggregate.preferred {
        add(
            &entry.node,
            "preferred",
            entry.score,
            entry.n,
            "",
            None,
            None,
        );
    }
    for entry in &aggregate.tentative {
        add(
            &entry.node,
            "tentative",
            entry.score,
            entry.n,
            "",
            None,
            None,
        );
    }
    for entry in &aggregate.contested {
        add(
            &entry.node,
            "contested",
            entry.score,
            entry.pos,
            &entry.last,
            Some(&entry.verdict),
            Some(entry.neg),
        );
    }
    LearningSidecar {
        version: LEARNING_SCHEMA_VERSION,
        generated_at: now.to_rfc3339_opts(SecondsFormat::AutoSi, false),
        nodes: output,
    }
}

/// Write the deterministic learning sidecar next to graph.json.
pub fn write_learning_sidecar(
    aggregate: &LessonAggregate,
    graph_path: &Path,
    now: DateTime<Utc>,
) -> Result<PathBuf> {
    let output = graph_path
        .parent()
        .unwrap_or(Path::new("."))
        .join(LEARNING_SIDECAR_NAME);
    let sidecar = build_learning_overlay(aggregate, graph_path, now);
    fs::write(
        &output,
        format!("{}\n", serde_json::to_string_pretty(&sidecar)?),
    )?;
    Ok(output)
}

/// Load the learning sidecar and recompute `stale` from current source content.
pub fn load_learning_overlay(graph_path: &Path) -> BTreeMap<String, LearningEntry> {
    let sidecar = graph_path
        .parent()
        .unwrap_or(Path::new("."))
        .join(LEARNING_SIDECAR_NAME);
    let Ok(bytes) = fs::read(sidecar) else {
        return BTreeMap::new();
    };
    let Ok(data) = serde_json::from_slice::<Value>(&bytes) else {
        return BTreeMap::new();
    };
    let Some(nodes) = data.get("nodes").and_then(Value::as_object) else {
        return BTreeMap::new();
    };
    let mut output = nodes
        .iter()
        .filter_map(|(id, value)| {
            serde_json::from_value::<LearningEntry>(value.clone())
                .ok()
                .map(|entry| (id.clone(), entry))
        })
        .collect::<BTreeMap<_, _>>();
    for entry in output.values_mut() {
        entry.stale = Some(if entry.source_file.is_empty() {
            false
        } else if let Some(path) = resolve_source_path(&entry.source_file, graph_path) {
            entry.code_fingerprint.is_empty() || content_hash(&path) != entry.code_fingerprint
        } else {
            true
        });
    }
    output
}
