//! Portable Wikipedia-style Markdown export.

use crate::obsidian::{communities_from_graph, community_labels_from_graph, Communities};
use graphoxide_core::{Confidence, KnowledgeGraph, Node};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    path::Path,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GodNodeArticle {
    pub id: String,
    pub label: String,
    pub degree: usize,
}

#[derive(Debug, Clone, Default)]
pub struct WikiOptions {
    pub community_labels: BTreeMap<i64, String>,
    pub cohesion: BTreeMap<i64, f64>,
    pub god_nodes: Vec<GodNodeArticle>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WikiReport {
    pub article_count: usize,
    pub stale_nodes_dropped: usize,
    pub warnings: Vec<String>,
}

/// Backward-compatible CLI entry point. Graph community metadata is used when
/// present; an unclustered graph becomes one community so the export remains
/// useful before clustering.
pub fn export_wiki(graph: &KnowledgeGraph, directory: &Path) -> anyhow::Result<()> {
    let mut communities = communities_from_graph(graph);
    if communities.is_empty() && !graph.nodes.is_empty() {
        communities.insert(0, graph.nodes.iter().map(|node| node.id.clone()).collect());
    }
    let options = WikiOptions {
        community_labels: community_labels_from_graph(graph),
        ..WikiOptions::default()
    };
    export_wiki_with_options(graph, &communities, directory, &options).map(|_| ())
}

pub fn export_wiki_with_options(
    graph: &KnowledgeGraph,
    communities: &Communities,
    directory: &Path,
    options: &WikiOptions,
) -> anyhow::Result<WikiReport> {
    fs::create_dir_all(directory)?;
    let by_id = graph
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect::<HashMap<_, _>>();
    let mut report = WikiReport::default();
    let mut valid = Communities::new();
    for (community, members) in communities {
        let retained = members
            .iter()
            .filter(|id| by_id.contains_key(id.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        report.stale_nodes_dropped += members.len().saturating_sub(retained.len());
        if !retained.is_empty() {
            valid.insert(*community, retained);
        }
    }
    if !communities.is_empty() && valid.is_empty() {
        anyhow::bail!("all community node IDs are stale; rebuild communities before wiki export");
    }
    if report.stale_nodes_dropped > 0 {
        let warning = format!(
            "dropped {} stale community node ID(s) during wiki export",
            report.stale_nodes_dropped
        );
        eprintln!("warning: {warning}");
        report.warnings.push(warning);
    }

    let node_community = reverse_communities(&valid);
    let mut used_slugs = BTreeSet::new();
    let mut community_articles = BTreeMap::new();
    for community in valid.keys() {
        let label = community_label(*community, options);
        community_articles.insert(*community, allocate_slug(&label, &mut used_slugs));
    }
    let valid_gods = options
        .god_nodes
        .iter()
        .filter(|god| by_id.contains_key(god.id.as_str()))
        .collect::<Vec<_>>();
    let mut god_articles = BTreeMap::new();
    for god in &valid_gods {
        god_articles.insert(god.id.clone(), allocate_slug(&god.label, &mut used_slugs));
    }

    for (community, members) in &valid {
        let label = community_label(*community, options);
        let body = community_article(
            graph,
            *community,
            members,
            &label,
            options.cohesion.get(community).copied(),
            &node_community,
            &community_articles,
            options,
        );
        write_markdown(directory, &community_articles[community], &body)?;
        report.article_count += 1;
    }
    for god in valid_gods {
        let body = god_node_article(
            graph,
            god,
            &node_community,
            &community_articles,
            &god_articles,
            options,
        );
        write_markdown(directory, &god_articles[&god.id], &body)?;
        report.article_count += 1;
    }
    let index = index_markdown(
        &valid,
        &community_articles,
        &valid_god_data(options, &by_id),
        &god_articles,
        options,
    );
    graphoxide_core::write_text_atomic(directory.join("index.md"), &index)?;
    Ok(report)
}

fn valid_god_data<'a>(
    options: &'a WikiOptions,
    by_id: &HashMap<&str, &Node>,
) -> Vec<&'a GodNodeArticle> {
    options
        .god_nodes
        .iter()
        .filter(|god| by_id.contains_key(god.id.as_str()))
        .collect()
}

fn index_markdown(
    communities: &Communities,
    community_articles: &BTreeMap<i64, String>,
    gods: &[&GodNodeArticle],
    god_articles: &BTreeMap<String, String>,
    options: &WikiOptions,
) -> String {
    let mut output = String::from("# Knowledge graph\n\n## Communities\n\n");
    for community in communities.keys() {
        let label = community_label(*community, options);
        output.push_str(&format!(
            "- {}\n",
            markdown_link(&label, &community_articles[community])
        ));
    }
    if !gods.is_empty() {
        output.push_str("\n## Highly connected nodes\n\n");
        for god in gods {
            output.push_str(&format!(
                "- {} — {} connections\n",
                markdown_link(&god.label, &god_articles[&god.id]),
                god.degree
            ));
        }
    }
    output
}

#[allow(clippy::too_many_arguments)]
fn community_article(
    graph: &KnowledgeGraph,
    community: i64,
    members: &[String],
    label: &str,
    cohesion: Option<f64>,
    node_community: &HashMap<&str, i64>,
    articles: &BTreeMap<i64, String>,
    options: &WikiOptions,
) -> String {
    let by_id = graph
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect::<HashMap<_, _>>();
    let mut output = format!("# {label}\n\n");
    if let Some(cohesion) = cohesion {
        output.push_str(&format!("_Community cohesion {cohesion:.2}_\n\n"));
    }
    output.push_str("## Nodes\n\n");
    let mut nodes = members
        .iter()
        .filter_map(|id| by_id.get(id.as_str()).copied())
        .collect::<Vec<_>>();
    nodes.sort_by(|left, right| {
        left.label
            .to_ascii_lowercase()
            .cmp(&right.label.to_ascii_lowercase())
            .then_with(|| left.source_file.cmp(&right.source_file))
            .then_with(|| left.id.cmp(&right.id))
    });
    for node in nodes.iter().take(25) {
        if node.source_file.is_empty() {
            output.push_str(&format!("- {}\n", node.label));
        } else {
            output.push_str(&format!("- {} — `{}`\n", node.label, node.source_file));
        }
    }
    if nodes.len() > 25 {
        output.push_str(&format!("- _and {} more nodes_\n", nodes.len() - 25));
    }

    let member_ids = members.iter().map(String::as_str).collect::<BTreeSet<_>>();
    let mut audit = graph
        .links
        .iter()
        .filter(|edge| {
            member_ids.contains(edge.true_source()) || member_ids.contains(edge.true_target())
        })
        .collect::<Vec<_>>();
    audit.sort_by(|left, right| {
        left.true_source()
            .cmp(right.true_source())
            .then_with(|| left.true_target().cmp(right.true_target()))
            .then_with(|| left.relation.cmp(&right.relation))
    });
    if !audit.is_empty() {
        output.push_str("\n## Relationship audit\n\n");
        for edge in audit.iter().take(50) {
            let source = by_id
                .get(edge.true_source())
                .map_or(edge.true_source(), |node| node.label.as_str());
            let target = by_id
                .get(edge.true_target())
                .map_or(edge.true_target(), |node| node.label.as_str());
            output.push_str(&format!(
                "- {source} —{}→ {target} [{}]\n",
                edge.relation,
                confidence_name(edge.confidence)
            ));
        }
    }

    let mut cross = BTreeSet::new();
    for edge in &graph.links {
        for (inside, outside) in [
            (edge.true_source(), edge.true_target()),
            (edge.true_target(), edge.true_source()),
        ] {
            if member_ids.contains(inside)
                && let Some(other) = node_community.get(outside).copied()
                && other != community
            {
                cross.insert(other);
            }
        }
    }
    if !cross.is_empty() {
        output.push_str("\n## Related communities\n\n");
        for other in cross {
            output.push_str(&format!(
                "- {}\n",
                markdown_link(&community_label(other, options), &articles[&other])
            ));
        }
    }
    output.push_str("\n---\n[← index](index.md)\n");
    output
}

fn god_node_article(
    graph: &KnowledgeGraph,
    god: &GodNodeArticle,
    node_community: &HashMap<&str, i64>,
    community_articles: &BTreeMap<i64, String>,
    god_articles: &BTreeMap<String, String>,
    options: &WikiOptions,
) -> String {
    let by_id = graph
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect::<HashMap<_, _>>();
    let mut output = format!("# {}\n\n_{} connections_\n", god.label, god.degree);
    if let Some(community) = node_community.get(god.id.as_str()) {
        output.push_str(&format!(
            "\nCommunity: {}\n",
            markdown_link(
                &community_label(*community, options),
                &community_articles[community]
            )
        ));
    }
    let mut neighbors = BTreeSet::new();
    for edge in &graph.links {
        if edge.true_source() == god.id {
            neighbors.insert(edge.true_target());
        } else if edge.true_target() == god.id {
            neighbors.insert(edge.true_source());
        }
    }
    if !neighbors.is_empty() {
        output.push_str("\n## Connections\n\n");
        for id in neighbors {
            let label = by_id.get(id).map_or(id, |node| node.label.as_str());
            if let Some(article) = god_articles.get(id) {
                output.push_str(&format!("- {}\n", markdown_link(label, article)));
            } else {
                output.push_str(&format!("- {label}\n"));
            }
        }
    }
    output.push_str("\n---\n[← index](index.md)\n");
    output
}

fn reverse_communities(communities: &Communities) -> HashMap<&str, i64> {
    communities
        .iter()
        .flat_map(|(community, members)| {
            members
                .iter()
                .map(move |member| (member.as_str(), *community))
        })
        .collect()
}

fn community_label(community: i64, options: &WikiOptions) -> String {
    options
        .community_labels
        .get(&community)
        .cloned()
        .unwrap_or_else(|| format!("Community {community}"))
}

fn allocate_slug(label: &str, used: &mut BTreeSet<String>) -> String {
    let base = safe_filename(label);
    let mut candidate = base.clone();
    let mut suffix = 2_u64;
    while used.contains(&candidate.to_ascii_lowercase()) {
        candidate = format!("{base}_{suffix}");
        suffix += 1;
    }
    used.insert(candidate.to_ascii_lowercase());
    candidate
}

fn safe_filename(label: &str) -> String {
    let mut result = String::new();
    let mut separator = false;
    for character in label.trim().chars() {
        if character.is_whitespace() {
            separator = true;
            continue;
        }
        if separator && !result.is_empty() {
            result.push('_');
        }
        separator = false;
        if matches!(
            character,
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
        ) {
            result.push('_');
        } else {
            result.push(character);
        }
    }
    let result = result.trim_matches(['.', '_']).to_owned();
    if result.is_empty() {
        "Community".into()
    } else {
        result
    }
}

fn markdown_link(label: &str, slug: &str) -> String {
    let display = label.replace('[', "\\[").replace(']', "\\]");
    format!("[{display}]({}.md)", percent_encode(slug))
}

fn percent_encode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(*byte, b'_' | b'-' | b'.' | b'~') {
            encoded.push(char::from(*byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn confidence_name(confidence: Confidence) -> &'static str {
    match confidence {
        Confidence::Extracted => "EXTRACTED",
        Confidence::Inferred => "INFERRED",
        Confidence::Ambiguous => "AMBIGUOUS",
    }
}

fn write_markdown(directory: &Path, slug: &str, body: &str) -> anyhow::Result<()> {
    graphoxide_core::write_text_atomic(directory.join(format!("{slug}.md")), body)
}
