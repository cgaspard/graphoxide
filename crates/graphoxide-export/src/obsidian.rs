//! Obsidian vault and Canvas exporters.
//!
//! Filenames are intentionally computed by one shared routine.  Obsidian
//! links and Canvas file cards must agree even for long, punctuation-only, or
//! case-colliding labels.

use anyhow::Context;
use graphoxide_core::{Confidence, KnowledgeGraph, Node};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    path::{Component, Path, PathBuf},
};

pub type Communities = BTreeMap<i64, Vec<String>>;

#[derive(Debug, Clone, Default)]
pub struct VaultOptions {
    pub community_labels: BTreeMap<i64, String>,
    pub cohesion: BTreeMap<i64, f64>,
}

/// Produce a portable Obsidian/Canvas filename stem.
pub fn obsidian_safe_stem(label: &str) -> String {
    let normalized = label.replace("\r\n", " ").replace(['\r', '\n'], " ");
    let mut cleaned: String = normalized
        .chars()
        .filter(|character| {
            !matches!(
                character,
                '\\' | '/' | '*' | '?' | ':' | '"' | '<' | '>' | '|' | '#' | '^' | '[' | ']'
            )
        })
        .collect::<String>()
        .trim()
        .to_owned();

    let lower = cleaned.to_lowercase();
    for extension in [".markdown", ".mdx", ".qmd", ".md"] {
        if lower.ends_with(extension) {
            cleaned.truncate(cleaned.len() - extension.len());
            break;
        }
    }

    if cleaned.starts_with('.') {
        let remainder = cleaned.trim_start_matches('.');
        if remainder.chars().any(is_word_character) {
            cleaned = format!("dot-{remainder}");
        }
    }
    if !cleaned.chars().any(is_word_character) {
        cleaned = "unnamed".into();
    }
    cap_filename(&cleaned, 200)
}

fn is_word_character(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

fn cap_filename(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_owned();
    }
    let digest = hex::encode(Sha256::digest(value.as_bytes()));
    let suffix = format!("_{}", &digest[..8]);
    let keep = limit.saturating_sub(suffix.len());
    let mut boundary = keep.min(value.len());
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    format!("{}{}", &value[..boundary], suffix)
}

/// Stable, case-insensitive collision handling shared by both exporters.
pub fn node_filenames(graph: &KnowledgeGraph) -> BTreeMap<String, String> {
    let mut result = BTreeMap::new();
    let mut used = BTreeSet::new();
    for node in &graph.nodes {
        let base = obsidian_safe_stem(if node.label.is_empty() {
            &node.id
        } else {
            &node.label
        });
        let mut candidate = base.clone();
        let mut suffix = 1_u64;
        while used.contains(&candidate.to_lowercase()) {
            candidate = format!("{base}_{suffix}");
            suffix += 1;
        }
        used.insert(candidate.to_lowercase());
        result.insert(node.id.clone(), candidate);
    }
    result
}

pub fn communities_from_graph(graph: &KnowledgeGraph) -> Communities {
    let mut communities: Communities = BTreeMap::new();
    for node in &graph.nodes {
        if let Some(community) = node.community {
            communities
                .entry(community)
                .or_default()
                .push(node.id.clone());
        }
    }
    communities
}

pub fn community_labels_from_graph(graph: &KnowledgeGraph) -> BTreeMap<i64, String> {
    let mut labels = BTreeMap::new();
    for node in &graph.nodes {
        if let (Some(community), Some(label)) = (
            node.community,
            node.extra.get("community_name").and_then(Value::as_str),
        ) {
            labels.entry(community).or_insert_with(|| label.to_owned());
        }
    }
    labels
}

/// Backward-compatible convenience entry point used by the CLI.
pub fn export_vault(graph: &KnowledgeGraph, directory: &Path) -> anyhow::Result<()> {
    let mut communities = communities_from_graph(graph);
    if communities.is_empty() && !graph.nodes.is_empty() {
        communities.insert(0, graph.nodes.iter().map(|node| node.id.clone()).collect());
    }
    let options = VaultOptions {
        community_labels: community_labels_from_graph(graph),
        ..Default::default()
    };
    export_vault_with_options(graph, &communities, directory, &options).map(|_| ())
}

/// Export one note per node and one overview note per declared community.
///
/// Existing files are only overwritten when the prior export manifest marks
/// them as Graphoxide-owned.  Removed owned notes are pruned on rerun; foreign
/// user notes and `.obsidian` settings are never touched.
pub fn export_vault_with_options(
    graph: &KnowledgeGraph,
    communities: &Communities,
    directory: &Path,
    options: &VaultOptions,
) -> anyhow::Result<usize> {
    fs::create_dir_all(directory)?;
    let manifest_path = directory.join(".graphoxide_obsidian_manifest.json");
    let prior_owned = load_owned_manifest(&manifest_path);
    let mut written = BTreeSet::new();
    let mut skipped = BTreeSet::new();
    let filenames = node_filenames(graph);
    let by_id: HashMap<_, _> = graph
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect();
    let node_community = reverse_communities(communities);
    let mut notes_written = 0;

    for node in &graph.nodes {
        let stem = &filenames[&node.id];
        let relative = format!("{stem}.md");
        let community = node_community.get(node.id.as_str()).copied();
        let community_name = community
            .map(|cid| community_name(cid, options))
            .unwrap_or_else(|| "Unclustered".into());
        let dominant = dominant_confidence(graph, &node.id);
        let file_tag = if node.file_type.is_empty() {
            "document"
        } else {
            node.file_type.as_str()
        };
        let mut body = format!(
            "---\nsource_file: \"{}\"\ntype: \"{}\"\ncommunity: \"{}\"\ntags:\n  - graphoxide/{}\n  - graphoxide/{}\n---\n\n# {}\n\n",
            yaml_scalar(&node.source_file),
            yaml_scalar(&node.file_type),
            yaml_scalar(&community_name),
            file_tag,
            confidence_name(dominant),
            node.label
        );
        let neighbors = neighbors(graph, &node.id);
        if !neighbors.is_empty() {
            body.push_str("## Connections\n");
            for (neighbor_id, relation, confidence) in neighbors {
                let Some(stem) = filenames.get(neighbor_id) else {
                    continue;
                };
                body.push_str(&format!(
                    "- [[{stem}]] - `{relation}` [{}]\n",
                    confidence_name(confidence)
                ));
            }
            body.push('\n');
        }
        body.push_str(&format!(
            "#graphoxide/{} #graphoxide/{}\n",
            file_tag,
            confidence_name(dominant)
        ));
        if owned_write(
            directory,
            &relative,
            &body,
            &prior_owned,
            &mut written,
            &mut skipped,
        )? {
            notes_written += 1;
        }
    }

    let community_filenames = community_filenames(communities, options);
    for (community, members) in communities {
        let surviving: Vec<_> = members
            .iter()
            .filter_map(|id| by_id.get(id.as_str()).copied())
            .collect();
        let name = community_name(*community, options);
        let mut body = String::from("---\ntype: community\n");
        if let Some(cohesion) = options.cohesion.get(community) {
            body.push_str(&format!("cohesion: {cohesion:.2}\n"));
        }
        body.push_str(&format!(
            "members: {}\n---\n\n# {name}\n\n**Members:** {} nodes\n\n## Members\n",
            surviving.len(),
            surviving.len()
        ));
        let mut sorted = surviving;
        sorted.sort_by(|left, right| {
            left.label
                .cmp(&right.label)
                .then_with(|| left.id.cmp(&right.id))
        });
        for node in sorted {
            let stem = &filenames[&node.id];
            body.push_str(&format!("- [[{stem}]]"));
            if !node.file_type.is_empty() {
                body.push_str(&format!(" - {}", node.file_type));
            }
            if !node.source_file.is_empty() {
                body.push_str(&format!(" - {}", node.source_file));
            }
            body.push('\n');
        }
        let relative = format!("{}.md", community_filenames[community]);
        if owned_write(
            directory,
            &relative,
            &body,
            &prior_owned,
            &mut written,
            &mut skipped,
        )? {
            notes_written += 1;
        }
    }

    let graph_config = json!({"colorGroups": options.community_labels.iter().map(|(cid, label)| {
        json!({"query": format!("tag:#community/{}", label.replace(' ', "_")), "color": {"a": 1, "rgb": cid.abs() % 16_777_216}})
    }).collect::<Vec<_>>()});
    let config_text = serde_json::to_string_pretty(&graph_config)?;
    let _ = owned_write(
        directory,
        ".obsidian/graph.json",
        &config_text,
        &prior_owned,
        &mut written,
        &mut skipped,
    )?;

    for stale in prior_owned
        .difference(&written)
        .filter(|path| !skipped.contains(*path))
    {
        if !safe_owned_relative(stale) {
            continue;
        }
        let target = directory.join(stale);
        if target.is_file() {
            let _ = fs::remove_file(target);
        }
    }
    graphoxide_core::write_json_atomic(
        &manifest_path,
        &OwnedManifest {
            files: written.into_iter().collect(),
        },
        true,
    )?;
    Ok(notes_written)
}

fn confidence_name(confidence: Confidence) -> &'static str {
    match confidence {
        Confidence::Extracted => "EXTRACTED",
        Confidence::Inferred => "INFERRED",
        Confidence::Ambiguous => "AMBIGUOUS",
    }
}

fn dominant_confidence(graph: &KnowledgeGraph, id: &str) -> Confidence {
    let mut counts = [0_usize; 3];
    for edge in &graph.links {
        if edge.true_source() == id || edge.true_target() == id {
            counts[match edge.confidence {
                Confidence::Extracted => 0,
                Confidence::Inferred => 1,
                Confidence::Ambiguous => 2,
            }] += 1;
        }
    }
    let index = counts
        .iter()
        .enumerate()
        .max_by_key(|(index, count)| (**count, std::cmp::Reverse(*index)))
        .map(|(index, _)| index)
        .unwrap_or(0);
    [
        Confidence::Extracted,
        Confidence::Inferred,
        Confidence::Ambiguous,
    ][index]
}

fn neighbors<'a>(graph: &'a KnowledgeGraph, id: &'a str) -> Vec<(&'a str, &'a str, Confidence)> {
    let mut result = Vec::new();
    for edge in &graph.links {
        if edge.true_source() == id {
            result.push((edge.true_target(), edge.relation.as_str(), edge.confidence));
        } else if edge.true_target() == id {
            result.push((edge.true_source(), edge.relation.as_str(), edge.confidence));
        }
    }
    result.sort_by(|left, right| left.0.cmp(right.0).then_with(|| left.1.cmp(right.1)));
    result
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

fn community_name(community: i64, options: &VaultOptions) -> String {
    options
        .community_labels
        .get(&community)
        .cloned()
        .unwrap_or_else(|| format!("Community {community}"))
}

fn community_filenames(communities: &Communities, options: &VaultOptions) -> BTreeMap<i64, String> {
    let mut result = BTreeMap::new();
    let mut used = BTreeSet::new();
    for community in communities.keys() {
        let base = format!(
            "_COMMUNITY_{}",
            obsidian_safe_stem(&community_name(*community, options))
        );
        let mut candidate = base.clone();
        let mut suffix = 1;
        while used.contains(&candidate.to_lowercase()) {
            candidate = format!("{base}_{suffix}");
            suffix += 1;
        }
        used.insert(candidate.to_lowercase());
        result.insert(*community, candidate);
    }
    result
}

#[derive(Serialize, serde::Deserialize, Default)]
struct OwnedManifest {
    #[serde(default)]
    files: Vec<String>,
}

fn load_owned_manifest(path: &Path) -> BTreeSet<String> {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<OwnedManifest>(&bytes).ok())
        .map(|manifest| manifest.files.into_iter().collect())
        .unwrap_or_default()
}

fn owned_write(
    directory: &Path,
    relative: &str,
    content: &str,
    prior_owned: &BTreeSet<String>,
    written: &mut BTreeSet<String>,
    skipped: &mut BTreeSet<String>,
) -> anyhow::Result<bool> {
    anyhow::ensure!(safe_owned_relative(relative), "unsafe vault output path");
    let target = directory.join(relative);
    if target.exists() && !prior_owned.contains(relative) {
        skipped.insert(relative.into());
        return Ok(false);
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    graphoxide_core::write_text_atomic(&target, content)?;
    written.insert(relative.into());
    Ok(true)
}

fn safe_owned_relative(relative: &str) -> bool {
    let path = Path::new(relative);
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn yaml_scalar(value: &str) -> String {
    let mut result = String::new();
    for character in value.chars() {
        match character {
            '\\' => result.push_str("\\\\"),
            '"' => result.push_str("\\\""),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            '\0' => result.push_str("\\0"),
            value if value.is_control() => {
                result.push_str(&format!("\\x{:02x}", value as u32));
            }
            value => result.push(value),
        }
    }
    result
}

/// Build a complete Obsidian Canvas document.
pub fn render_canvas(
    graph: &KnowledgeGraph,
    requested_communities: &Communities,
    community_labels: &BTreeMap<i64, String>,
) -> Value {
    let filenames = node_filenames(graph);
    let by_id: HashMap<_, _> = graph
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect();
    let mut communities = requested_communities.clone();
    if communities.is_empty() && !graph.nodes.is_empty() {
        communities.insert(0, graph.nodes.iter().map(|node| node.id.clone()).collect());
    }
    let community_count = communities.len();
    let outer_columns = ceil_sqrt(community_count.max(1));
    let outer_rows = community_count.div_ceil(outer_columns);
    let ids: Vec<_> = communities.keys().copied().collect();
    let mut sizes = BTreeMap::new();
    let mut inner_columns = BTreeMap::new();
    for (community, members) in &communities {
        let count = members
            .iter()
            .filter(|member| by_id.contains_key(member.as_str()) && filenames.contains_key(*member))
            .count();
        let columns = ceil_sqrt(count.max(1));
        inner_columns.insert(*community, columns);
        sizes.insert(
            *community,
            (
                600_usize.max(220 * columns),
                400_usize.max(100 * count.div_ceil(columns) + 120),
            ),
        );
    }
    let column_widths: Vec<_> = (0..outer_columns)
        .map(|column| {
            (0..outer_rows)
                .filter_map(|row| ids.get(row * outer_columns + column))
                .map(|id| sizes[id].0)
                .max()
                .unwrap_or(0)
        })
        .collect();
    let row_heights: Vec<_> = (0..outer_rows)
        .map(|row| {
            (0..outer_columns)
                .filter_map(|column| ids.get(row * outer_columns + column))
                .map(|id| sizes[id].1)
                .max()
                .unwrap_or(0)
        })
        .collect();

    let mut canvas_nodes = Vec::new();
    let mut valid_ids = BTreeSet::new();
    for (index, community) in ids.iter().enumerate() {
        let column = index % outer_columns;
        let row = index / outer_columns;
        let x: usize = column_widths[..column].iter().sum::<usize>() + column * 80;
        let y: usize = row_heights[..row].iter().sum::<usize>() + row * 80;
        let (width, height) = sizes[community];
        canvas_nodes.push(json!({
            "id": format!("g{community}"), "type": "group",
            "label": community_labels.get(community).cloned().unwrap_or_else(|| format!("Community {community}")),
            "x": x, "y": y, "width": width, "height": height,
            "color": ((index % 6) + 1).to_string()
        }));
        let mut members: Vec<&Node> = communities[community]
            .iter()
            .filter_map(|member| by_id.get(member.as_str()).copied())
            .collect();
        members.sort_by(|left, right| {
            left.label
                .cmp(&right.label)
                .then_with(|| left.id.cmp(&right.id))
        });
        let columns = inner_columns[community];
        for (member_index, node) in members.into_iter().enumerate() {
            valid_ids.insert(node.id.as_str());
            let card_column = member_index % columns;
            let card_row = member_index / columns;
            canvas_nodes.push(json!({
                "id": format!("n_{}", node.id), "type": "file",
                "file": format!("{}.md", filenames[&node.id]),
                "x": x + 20 + card_column * 200,
                "y": y + 80 + card_row * 80,
                "width": 180, "height": 60
            }));
        }
    }
    let mut weighted_edges: Vec<_> = graph
        .links
        .iter()
        .filter(|edge| {
            valid_ids.contains(edge.true_source()) && valid_ids.contains(edge.true_target())
        })
        .collect();
    weighted_edges.sort_by(|left, right| {
        edge_weight(right)
            .total_cmp(&edge_weight(left))
            .then_with(|| left.true_source().cmp(right.true_source()))
            .then_with(|| left.true_target().cmp(right.true_target()))
    });
    let canvas_edges: Vec<_> = weighted_edges
        .into_iter()
        .take(200)
        .enumerate()
        .map(|(index, edge)| {
            json!({
                "id": format!("e_{index}_{}_{}", edge.true_source(), edge.true_target()),
                "fromNode": format!("n_{}", edge.true_source()),
                "toNode": format!("n_{}", edge.true_target()),
                "label": format!("{} [{}]", edge.relation, confidence_name(edge.confidence)).trim().to_owned()
            })
        })
        .collect();
    json!({"nodes": canvas_nodes, "edges": canvas_edges})
}

fn edge_weight(edge: &graphoxide_core::Edge) -> f64 {
    edge.extra
        .get("weight")
        .and_then(Value::as_f64)
        .unwrap_or(1.0)
}

fn ceil_sqrt(value: usize) -> usize {
    let mut candidate = (value as f64).sqrt() as usize;
    if candidate * candidate < value {
        candidate += 1;
    }
    candidate.max(1)
}

pub fn export_canvas(
    graph: &KnowledgeGraph,
    communities: &Communities,
    output: &Path,
    community_labels: &BTreeMap<i64, String>,
) -> anyhow::Result<()> {
    let value = render_canvas(graph, communities, community_labels);
    graphoxide_core::write_json_atomic(output, &value, true)
        .with_context(|| format!("write Canvas export {}", output.display()))
}

#[allow(dead_code)]
fn _assert_paths_are_relative(paths: impl IntoIterator<Item = PathBuf>) -> bool {
    paths.into_iter().all(|path| path.is_relative())
}
