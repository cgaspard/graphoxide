//! Markdown architecture report.

use graphoxide_core::{sanitize_label, Confidence, KnowledgeGraph};
use graphoxide_graph::{find_import_cycles, Analysis};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DetectionSummary {
    pub total_files: usize,
    pub total_words: usize,
    pub warning: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TokenCost {
    pub input: u64,
    pub output: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReportOptions {
    pub root: String,
    pub detection: DetectionSummary,
    pub tokens: TokenCost,
    pub cohesion: BTreeMap<i64, f64>,
    pub min_community_size: usize,
    pub built_at_commit: Option<String>,
    pub learning: Option<Value>,
    pub obsidian: bool,
}

impl Default for ReportOptions {
    fn default() -> Self {
        Self {
            root: ".".into(),
            detection: DetectionSummary::default(),
            tokens: TokenCost::default(),
            cohesion: BTreeMap::new(),
            min_community_size: 3,
            built_at_commit: None,
            learning: None,
            obsidian: false,
        }
    }
}

pub fn render_report(graph: &KnowledgeGraph, analysis: &Analysis) -> String {
    render_report_with_options(graph, analysis, &ReportOptions::default())
}

pub fn render_report_with_options(
    graph: &KnowledgeGraph,
    analysis: &Analysis,
    options: &ReportOptions,
) -> String {
    let communities = community_members(graph);
    let extracted = graph
        .links
        .iter()
        .filter(|edge| edge.confidence == Confidence::Extracted)
        .count();
    let inferred_edges = graph
        .links
        .iter()
        .filter(|edge| edge.confidence == Confidence::Inferred)
        .collect::<Vec<_>>();
    let ambiguous_edges = graph
        .links
        .iter()
        .filter(|edge| edge.confidence == Confidence::Ambiguous)
        .collect::<Vec<_>>();
    let denominator = graph.links.len().max(1) as f64;
    let percentage = |count: usize| (count as f64 / denominator * 100.0).round() as usize;
    let inferred_average = (!inferred_edges.is_empty()).then(|| {
        inferred_edges
            .iter()
            .map(|edge| {
                edge.extra
                    .get("confidence_score")
                    .and_then(Value::as_f64)
                    .unwrap_or_else(|| edge.confidence.default_score())
            })
            .sum::<f64>()
            / inferred_edges.len() as f64
    });

    let thin = communities
        .values()
        .filter(|members| !members.is_empty() && members.len() < options.min_community_size)
        .count();
    let shown = communities.len().saturating_sub(thin);
    let mut lines = vec![
        format!("# Graph Report - {}", sanitize_label(&options.root)),
        String::new(),
        "## Corpus Check".into(),
    ];
    if let Some(warning) = &options.detection.warning {
        lines.push(format!("- {}", sanitize_label(warning)));
    } else {
        lines.push(format!(
            "- {} files · ~{} words",
            format_count(options.detection.total_files as u64),
            format_count(options.detection.total_words as u64)
        ));
        lines.push("- Verdict: corpus is large enough that graph structure adds value.".into());
    }
    lines.extend([
        String::new(),
        "## Summary".into(),
        format!(
            "- {} nodes · {} edges · {} communities{}",
            graph.nodes.len(),
            graph.links.len(),
            communities.len(),
            if thin == 0 {
                String::new()
            } else {
                format!(" ({shown} shown, {thin} thin omitted)")
            }
        ),
        format!(
            "- Extraction: {}% EXTRACTED · {}% INFERRED · {}% AMBIGUOUS{}",
            percentage(extracted),
            percentage(inferred_edges.len()),
            percentage(ambiguous_edges.len()),
            inferred_average
                .map(|average| format!(
                    " · INFERRED: {} edges (avg confidence: {average:.2})",
                    inferred_edges.len()
                ))
                .unwrap_or_default()
        ),
        format!(
            "- Token cost: {} input · {} output",
            format_count(options.tokens.input),
            format_count(options.tokens.output)
        ),
    ]);

    if let Some(commit) = options.built_at_commit.as_deref() {
        lines.extend([
            String::new(),
            "## Graph Freshness".into(),
            format!("- Built from commit: `{}`", &commit[..commit.len().min(8)]),
            "- Compare this with `git rev-parse HEAD` to check graph freshness.".into(),
        ]);
    }

    if !communities.is_empty() {
        lines.extend([String::new(), "## Community Hubs (Navigation)".into()]);
        for community in communities.keys() {
            let label = community_name(graph, *community);
            if options.obsidian {
                lines.push(format!(
                    "- [[_COMMUNITY_{}|{}]]",
                    safe_community_name(&label),
                    sanitize_label(&label)
                ));
            } else {
                lines.push(format!("- {}", sanitize_label(&label)));
            }
        }
    }

    lines.extend([
        String::new(),
        "## God Nodes (most connected - your core abstractions)".into(),
    ]);
    if analysis.god_nodes.is_empty() {
        lines.push("- None detected.".into());
    } else {
        for (index, node) in analysis.god_nodes.iter().enumerate() {
            lines.push(format!(
                "{}. `{}` - {} edges",
                index + 1,
                sanitize_label(&node.label),
                node.degree
            ));
        }
    }

    lines.extend([
        String::new(),
        "## Surprising Connections (you probably didn't know these)".into(),
    ]);
    if analysis.surprising_connections.is_empty() {
        lines.push("- None detected - all connections are within the same source files.".into());
    } else {
        for surprise in &analysis.surprising_connections {
            let confidence = match (surprise.confidence, surprise.confidence_score) {
                (Confidence::Inferred, Some(score)) => format!("INFERRED {score:.2}"),
                (confidence, _) => format!("{confidence:?}").to_uppercase(),
            };
            let semantic = if surprise.relation == "semantically_similar_to" {
                " [semantically similar]"
            } else {
                ""
            };
            lines.push(format!(
                "- `{}` --{}--> `{}`  [{confidence}]{semantic}",
                sanitize_label(&surprise.source),
                sanitize_label(&surprise.relation),
                sanitize_label(&surprise.target)
            ));
            let note = surprise
                .note
                .as_deref()
                .or(surprise.why.as_deref())
                .filter(|note| !note.is_empty())
                .map(|note| format!("  _{}_", sanitize_label(note)))
                .unwrap_or_default();
            lines.push(format!(
                "  {} → {}{}",
                sanitize_label(&surprise.source_files[0]),
                sanitize_label(&surprise.source_files[1]),
                note
            ));
        }
    }

    let has_code = graph.nodes.iter().any(|node| node.file_type == "code")
        || graph
            .links
            .iter()
            .any(|edge| matches!(edge.relation.as_str(), "imports" | "imports_from"));
    if has_code {
        lines.extend([String::new(), "## Import Cycles".into()]);
        let cycles = find_import_cycles(graph, 20, 20);
        if cycles.is_empty() {
            lines.push("- None detected.".into());
        } else {
            for cycle in cycles {
                let mut path = cycle.cycle.clone();
                if let Some(first) = cycle.cycle.first() {
                    path.push(first.clone());
                }
                lines.push(format!(
                    "- {}-file cycle: `{}`",
                    cycle.length,
                    path.join(" -> ")
                ));
            }
        }
    }

    if !graph.hyperedges.is_empty() {
        lines.extend([String::new(), "## Hyperedges (group relationships)".into()]);
        for hyperedge in &graph.hyperedges {
            let label = hyperedge
                .get("label")
                .or_else(|| hyperedge.get("id"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let members = hyperedge
                .get("nodes")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ");
            let confidence = hyperedge
                .get("confidence")
                .and_then(Value::as_str)
                .unwrap_or("INFERRED");
            let confidence = hyperedge
                .get("confidence_score")
                .and_then(Value::as_f64)
                .map(|score| format!("{confidence} {score:.2}"))
                .unwrap_or_else(|| confidence.into());
            lines.push(format!(
                "- **{}** — {} [{}]",
                sanitize_label(label),
                sanitize_label(&members),
                confidence
            ));
        }
    }

    lines.extend([
        String::new(),
        format!(
            "## Communities ({} total, {} thin omitted)",
            communities.len(),
            thin
        ),
    ]);
    for (community, members) in &communities {
        if members.len() < options.min_community_size {
            continue;
        }
        lines.extend([
            String::new(),
            format!(
                "### Community {community} - \"{}\"",
                sanitize_label(&community_name(graph, *community))
            ),
            format!(
                "Cohesion: {:.2}",
                options.cohesion.get(community).copied().unwrap_or(0.0)
            ),
            format!(
                "Nodes ({}): {}",
                members.len(),
                members
                    .iter()
                    .filter_map(|id| graph.nodes.iter().find(|node| node.id == *id))
                    .take(8)
                    .map(|node| sanitize_label(&node.label))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ]);
    }

    if !ambiguous_edges.is_empty() {
        let labels = graph
            .nodes
            .iter()
            .map(|node| (node.id.as_str(), node.label.as_str()))
            .collect::<BTreeMap<_, _>>();
        lines.extend([String::new(), "## Ambiguous Edges - Review These".into()]);
        for edge in &ambiguous_edges {
            lines.push(format!(
                "- `{}` → `{}`  [AMBIGUOUS]",
                sanitize_label(
                    labels
                        .get(edge.true_source())
                        .copied()
                        .unwrap_or(edge.true_source())
                ),
                sanitize_label(
                    labels
                        .get(edge.true_target())
                        .copied()
                        .unwrap_or(edge.true_target())
                )
            ));
            lines.push(format!(
                "  {} · relation: {}",
                sanitize_label(&edge.source_file),
                sanitize_label(&edge.relation)
            ));
        }
    }

    append_learning(&mut lines, options.learning.as_ref());

    if !analysis.suggested_questions.is_empty() {
        lines.extend([String::new(), "## Suggested Questions".into()]);
        for question in &analysis.suggested_questions {
            lines.push(format!("- {}", sanitize_label(question)));
        }
    }

    lines.extend([
        String::new(),
        "## Confidence audit".into(),
        format!("- Extracted: {extracted}"),
        format!(
            "- Inferred: {}{}",
            inferred_edges.len(),
            inferred_average
                .map(|average| format!(" (avg confidence: {average:.2})"))
                .unwrap_or_default()
        ),
        format!("- Ambiguous: {}", ambiguous_edges.len()),
    ]);
    lines.join("\n")
}

fn community_members(graph: &KnowledgeGraph) -> BTreeMap<i64, Vec<String>> {
    let mut communities = BTreeMap::<i64, Vec<String>>::new();
    for node in &graph.nodes {
        if let Some(community) = node.community {
            communities
                .entry(community)
                .or_default()
                .push(node.id.clone());
        }
    }
    for members in communities.values_mut() {
        members.sort();
    }
    communities
}

fn community_name(graph: &KnowledgeGraph, community: i64) -> String {
    graph
        .nodes
        .iter()
        .filter(|node| node.community == Some(community))
        .find_map(|node| {
            node.extra
                .get("community_name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
        })
        .map(str::to_owned)
        .unwrap_or_else(|| format!("Community {community}"))
}

fn safe_community_name(label: &str) -> String {
    let blocked = [
        '\\', '/', '*', '?', ':', '"', '<', '>', '|', '#', '^', '[', ']',
    ];
    let mut cleaned = label
        .replace(['\r', '\n'], " ")
        .chars()
        .filter(|character| !blocked.contains(character))
        .collect::<String>();
    cleaned = cleaned.trim().to_owned();
    let lower = cleaned.to_ascii_lowercase();
    for suffix in [".markdown", ".mdx", ".md"] {
        if lower.ends_with(suffix) {
            cleaned.truncate(cleaned.len() - suffix.len());
            break;
        }
    }
    if cleaned.is_empty() {
        "unnamed".into()
    } else {
        cleaned
    }
}

fn append_learning(lines: &mut Vec<String>, learning: Option<&Value>) {
    let Some(learning) = learning.and_then(Value::as_object) else {
        return;
    };
    let mut preferred = learning
        .get("overlay")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(id, entry)| {
            let entry = entry.as_object()?;
            (entry.get("status").and_then(Value::as_str) == Some("preferred"))
                .then_some((id.as_str(), entry))
        })
        .collect::<Vec<_>>();
    preferred.sort_by(|left, right| {
        let uses = |entry: &serde_json::Map<String, Value>| {
            entry.get("uses").and_then(Value::as_i64).unwrap_or(0)
        };
        let score = |entry: &serde_json::Map<String, Value>| {
            entry.get("score").and_then(Value::as_f64).unwrap_or(0.0)
        };
        uses(right.1)
            .cmp(&uses(left.1))
            .then_with(|| score(right.1).total_cmp(&score(left.1)))
            .then_with(|| left.0.cmp(right.0))
    });
    let dead_ends = learning
        .get("dead_ends")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    if preferred.is_empty() && dead_ends.is_empty() {
        return;
    }
    lines.extend([String::new(), "## Work-memory lessons".into()]);
    if !preferred.is_empty() {
        lines.extend([
            String::new(),
            "**Preferred sources** — corroborated by past sessions; start here.".into(),
        ]);
        for (id, entry) in preferred.into_iter().take(10) {
            let label = entry.get("label").and_then(Value::as_str).unwrap_or(id);
            let uses = entry.get("uses").and_then(Value::as_i64).unwrap_or(0);
            let score = entry.get("score").and_then(Value::as_f64).unwrap_or(0.0);
            let stale = if entry.get("stale").and_then(Value::as_bool).unwrap_or(false) {
                " _(code changed — re-verify)_"
            } else {
                ""
            };
            lines.push(format!(
                "- `{}` ({uses}× useful, score={score}){stale}",
                sanitize_label(label)
            ));
        }
    }
    if !dead_ends.is_empty() {
        lines.extend([
            String::new(),
            "**Known dead ends** — questions that led nowhere; don't re-derive.".into(),
        ]);
        for dead_end in dead_ends {
            let question = dead_end
                .get("question")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let nodes = dead_end
                .get("nodes")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(|node| format!("`{}`", sanitize_label(node)))
                .collect::<Vec<_>>()
                .join(", ");
            let sources = if nodes.is_empty() {
                String::new()
            } else {
                format!(" -> {nodes}")
            };
            lines.push(format!("- \"{}\"{sources}", sanitize_label(question)));
        }
    }
}

fn format_count(value: u64) -> String {
    let digits = value.to_string();
    let mut output = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            output.push(',');
        }
        output.push(character);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_formatting_is_stable() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(1_200), "1,200");
        assert_eq!(format_count(62_400), "62,400");
    }

    #[test]
    fn safe_names_drop_markdown_suffix_and_reserved_characters() {
        assert_eq!(safe_community_name("Auth/[Core].md"), "AuthCore");
        assert_eq!(safe_community_name("***"), "unnamed");
    }
}
