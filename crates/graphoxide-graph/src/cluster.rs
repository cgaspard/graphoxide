//! Deterministic Leiden community assignment.

use crate::build::is_file_node_label;
use graphoxide_core::KnowledgeGraph;
use network_partitions::{leiden, network::LabeledNetworkBuilder};
use rand::{rngs::SmallRng, SeedableRng};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    hash::Hash,
};

pub fn cluster(graph: &mut KnowledgeGraph) -> anyhow::Result<()> {
    if graph.nodes.is_empty() {
        return Ok(());
    }
    let mut degree: HashMap<&str, usize> = graph.nodes.iter().map(|n| (n.id.as_str(), 0)).collect();
    let mut edges: Vec<_> = graph
        .links
        .iter()
        .filter_map(|edge| {
            if edge.true_source() == edge.true_target() {
                return None;
            }
            *degree.get_mut(edge.true_source())? += 1;
            *degree.get_mut(edge.true_target())? += 1;
            Some((
                edge.true_source().to_owned(),
                edge.true_target().to_owned(),
                edge.extra
                    .get("weight")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(1.0),
            ))
        })
        .collect();
    edges.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    let mut assignments: HashMap<String, usize> = HashMap::new();
    if !edges.is_empty() {
        let mut builder = LabeledNetworkBuilder::new();
        let network = builder.build(edges.into_iter(), true);
        let mut rng = SmallRng::seed_from_u64(42);
        let (_, partition) = leiden::leiden(
            network.compact(),
            None,
            Some(1),
            Some(1.0),
            Some(0.001),
            &mut rng,
            true,
            None,
        )
        .map_err(|e| anyhow::anyhow!("Leiden failed: {e:?}"))?;
        for item in &partition {
            assignments.insert(network.label_for(item.node_id).to_owned(), item.cluster);
        }
    }
    let mut next = assignments.values().max().map(|v| v + 1).unwrap_or(0);
    for node in &graph.nodes {
        if !assignments.contains_key(&node.id) {
            assignments.insert(node.id.clone(), next);
            next += 1;
        }
    }
    postprocess_assignments(graph, &degree, &mut assignments);
    let mut members: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    for (node, cid) in &assignments {
        members.entry(*cid).or_default().push(node.clone())
    }
    for group in members.values_mut() {
        group.sort()
    }
    let mut groups: Vec<_> = members.into_values().collect();
    groups.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    let reindexed: HashMap<_, _> = groups
        .iter()
        .enumerate()
        .flat_map(|(cid, nodes)| nodes.iter().map(move |node| (node.clone(), cid as i64)))
        .collect();
    let hub_groups = groups
        .iter()
        .enumerate()
        .map(|(community, nodes)| (community as i64, nodes.clone()))
        .collect::<BTreeMap<_, _>>();
    let labels = label_communities_by_hub(graph, &hub_groups);
    for node in &mut graph.nodes {
        let cid = reindexed[&node.id];
        node.community = Some(cid);
        node.extra
            .insert("community_name".into(), labels[&cid].clone().into());
    }
    Ok(())
}

pub fn communities(graph: &KnowledgeGraph) -> BTreeMap<i64, Vec<String>> {
    let mut output = BTreeMap::new();
    for node in &graph.nodes {
        if let Some(community) = node.community {
            output
                .entry(community)
                .or_insert_with(Vec::new)
                .push(node.id.clone());
        }
    }
    for members in output.values_mut() {
        members.sort();
    }
    output
}

/// Deterministically partition positive weighted links between arbitrary labels.
///
/// Opposite directions share one undirected edge. Contributions are sorted
/// before finite, saturating aggregation so shuffled input cannot alter the
/// partitioner's weights.
pub fn partition_weighted_labels<T>(
    links: impl IntoIterator<Item = (T, T, f64)>,
) -> anyhow::Result<Vec<Vec<T>>>
where
    T: Clone + Eq + Hash + Ord,
{
    let weights = aggregate_weighted_labels(links);
    if weights.is_empty() {
        return Ok(Vec::new());
    }
    let mut builder = LabeledNetworkBuilder::new();
    let network = builder.build(
        weights
            .into_iter()
            .map(|((source, target), weight)| (source, target, weight)),
        true,
    );
    let mut rng = SmallRng::seed_from_u64(42);
    let (_, partition) = leiden::leiden(
        network.compact(),
        None,
        Some(1),
        Some(1.0),
        Some(0.001),
        &mut rng,
        true,
        None,
    )
    .map_err(|error| anyhow::anyhow!("weighted Leiden clustering failed: {error:?}"))?;
    let mut groups = BTreeMap::<usize, Vec<T>>::new();
    for item in &partition {
        groups
            .entry(item.cluster)
            .or_default()
            .push(network.label_for(item.node_id).clone());
    }
    let mut groups = groups.into_values().collect::<Vec<_>>();
    for group in &mut groups {
        group.sort();
    }
    groups.sort();
    Ok(groups)
}

fn aggregate_weighted_labels<T>(
    links: impl IntoIterator<Item = (T, T, f64)>,
) -> BTreeMap<(T, T), f64>
where
    T: Ord,
{
    let mut contributions = BTreeMap::<(T, T), Vec<f64>>::new();
    for (source, target, weight) in links {
        if source == target || !weight.is_finite() || weight <= 0.0 {
            continue;
        }
        let edge = if source < target {
            (source, target)
        } else {
            (target, source)
        };
        contributions.entry(edge).or_default().push(weight);
    }
    contributions
        .into_iter()
        .map(|(edge, mut values)| {
            values.sort_by(f64::total_cmp);
            let weight = values.into_iter().fold(0.0, |total, value| {
                let sum = total + value;
                if sum.is_finite() {
                    sum
                } else {
                    f64::MAX
                }
            });
            (edge, weight)
        })
        .collect()
}

fn matches_uri_reference_form(label: &str) -> bool {
    label.starts_with("./") || label.starts_with("../") || label.starts_with("#/")
}

fn is_compressed_document_wrapper_label(node: &graphoxide_core::Node, label: &str) -> bool {
    if node.file_type != "document" {
        return false;
    }
    let mut file_name = node.source_file.rsplit('/').next().unwrap_or_default();
    let mut stripped_compression = false;
    loop {
        let Some((stem, extension)) = file_name.rsplit_once('.') else {
            return false;
        };
        if matches!(extension, "bz2" | "gz" | "lz4" | "lzma" | "xz" | "zst") {
            stripped_compression = true;
        } else if !(stripped_compression && extension == "tar") {
            return false;
        }
        file_name = stem;
        if label == file_name {
            return true;
        }
    }
}

fn is_navigation_label_artifact(node: &graphoxide_core::Node, label: &str) -> bool {
    is_file_node_label(label, &node.source_file)
        || label.contains("://")
        || matches_uri_reference_form(label)
        || (node
            .extra
            .get("type")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|kind| kind.ends_with("_link"))
            && (label.starts_with('#') || label.starts_with('/')))
        || is_compressed_document_wrapper_label(node, label)
}

/// Deterministic, backend-free names based on each community's structural hub.
/// Semantic labels use full-graph degree followed by ascending node ID;
/// artifact-only communities receive an explicit structural fallback.
pub fn label_communities_by_hub(
    graph: &KnowledgeGraph,
    groups: &BTreeMap<i64, Vec<String>>,
) -> BTreeMap<i64, String> {
    let nodes = graph
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect::<BTreeMap<_, _>>();
    let mut degree = graph
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), 0usize))
        .collect::<BTreeMap<_, _>>();
    let mut seen_edges = BTreeSet::new();
    for edge in &graph.links {
        let source = edge.true_source();
        let target = edge.true_target();
        if !nodes.contains_key(source) || !nodes.contains_key(target) {
            continue;
        }
        let key = if source <= target {
            (source, target)
        } else {
            (target, source)
        };
        if !seen_edges.insert(key) {
            continue;
        }
        if source == target {
            *degree.entry(source).or_default() += 2;
        } else {
            *degree.entry(source).or_default() += 1;
            *degree.entry(target).or_default() += 1;
        }
    }
    groups
        .iter()
        .map(|(community, members)| {
            let hub = members
                .iter()
                .filter_map(|member| nodes.get(member.as_str()).map(|node| (member, *node)))
                .min_by_key(|member| {
                    let (member, node) = member;
                    let label = node.label.trim();
                    let label = if label.is_empty() {
                        node.id.as_str()
                    } else {
                        label
                    };
                    (
                        is_navigation_label_artifact(node, label),
                        std::cmp::Reverse(degree.get(member.as_str()).copied().unwrap_or(0)),
                        member.as_str(),
                    )
                })
                .filter(|(_, node)| {
                    let label = node.label.trim();
                    let label = if label.is_empty() { &node.id } else { label };
                    !is_navigation_label_artifact(node, label)
                });
            let label = hub
                .map(|(_, node)| node)
                .map(|node| {
                    let label = node.label.trim();
                    let label = if label.is_empty() { &node.id } else { label };
                    label.strip_suffix("()").unwrap_or(label).to_owned()
                })
                .filter(|label| !label.is_empty())
                .unwrap_or_else(|| format!("Community {community}"));
            (*community, label)
        })
        .collect()
}

/// Stable 16-hex membership fingerprints used to invalidate stale labels.
pub fn community_member_sigs(groups: &BTreeMap<i64, Vec<String>>) -> BTreeMap<i64, String> {
    groups
        .iter()
        .map(|(community, members)| {
            let mut members = members.iter().map(String::as_str).collect::<Vec<_>>();
            members.sort_unstable();
            let mut digest = Sha256::new();
            for member in members {
                digest.update(member.as_bytes());
                digest.update([0]);
            }
            (*community, hex::encode(digest.finalize())[..16].to_owned())
        })
        .collect()
}

/// Ratio of unique, undirected intra-community edges to the complete-graph
/// maximum. Empty/singleton communities are fully cohesive by definition.
pub fn cohesion_score(graph: &KnowledgeGraph, members: &[String]) -> f64 {
    if members.len() <= 1 {
        return 1.0;
    }
    let member_set = members.iter().map(String::as_str).collect::<BTreeSet<_>>();
    let actual = graph
        .links
        .iter()
        .filter_map(|edge| {
            let source = edge.true_source();
            let target = edge.true_target();
            if source == target || !member_set.contains(source) || !member_set.contains(target) {
                return None;
            }
            Some(if source <= target {
                (source, target)
            } else {
                (target, source)
            })
        })
        .collect::<BTreeSet<_>>()
        .len();
    let possible = members.len() * (members.len() - 1) / 2;
    actual as f64 / possible as f64
}

pub fn score_all(
    graph: &KnowledgeGraph,
    groups: &BTreeMap<i64, Vec<String>>,
) -> BTreeMap<i64, f64> {
    groups
        .iter()
        .map(|(community, members)| (*community, cohesion_score(graph, members)))
        .collect()
}

/// Pure remapping counterpart used by incremental callers and tests.
pub fn remap_community_map(
    groups: &BTreeMap<i64, Vec<String>>,
    previous: &BTreeMap<String, i64>,
) -> BTreeMap<i64, Vec<String>> {
    let mut old = BTreeMap::<i64, BTreeSet<String>>::new();
    for (node, community) in previous {
        old.entry(*community).or_default().insert(node.clone());
    }
    let new = groups
        .iter()
        .map(|(community, nodes)| (*community, nodes.iter().cloned().collect::<BTreeSet<_>>()))
        .collect::<BTreeMap<_, _>>();
    let mut overlaps = Vec::new();
    for (old_id, old_nodes) in &old {
        for (new_id, new_nodes) in &new {
            let overlap = old_nodes.intersection(new_nodes).count();
            if overlap > 0 {
                overlaps.push((overlap, *old_id, *new_id));
            }
        }
    }
    overlaps.sort_by(|left, right| {
        right
            .0
            .cmp(&left.0)
            .then_with(|| left.1.cmp(&right.1))
            .then_with(|| left.2.cmp(&right.2))
    });
    let mut mapping = BTreeMap::new();
    let mut used = BTreeSet::new();
    for (_, old_id, new_id) in overlaps {
        if !mapping.contains_key(&new_id) && used.insert(old_id) {
            mapping.insert(new_id, old_id);
        }
    }
    let mut unmatched = groups
        .iter()
        .filter(|(community, _)| !mapping.contains_key(community))
        .map(|(community, nodes)| (*community, nodes.clone()))
        .collect::<Vec<_>>();
    unmatched.sort_by(|left, right| {
        right
            .1
            .len()
            .cmp(&left.1.len())
            .then_with(|| left.1.cmp(&right.1))
    });
    let mut next = 0;
    for (community, _) in unmatched {
        while used.contains(&next) {
            next += 1;
        }
        mapping.insert(community, next);
        used.insert(next);
        next += 1;
    }
    groups
        .iter()
        .map(|(community, nodes)| {
            let mut nodes = nodes.clone();
            nodes.sort();
            (mapping[community], nodes)
        })
        .collect()
}

fn postprocess_assignments(
    graph: &KnowledgeGraph,
    _degree: &HashMap<&str, usize>,
    assignments: &mut HashMap<String, usize>,
) {
    let mut adjacency: BTreeMap<String, BTreeSet<String>> = graph
        .nodes
        .iter()
        .map(|n| (n.id.clone(), BTreeSet::new()))
        .collect();
    for edge in &graph.links {
        if edge.true_source() != edge.true_target() {
            if let Some(values) = adjacency.get_mut(edge.true_source()) {
                values.insert(edge.true_target().to_owned());
            }
            if let Some(values) = adjacency.get_mut(edge.true_target()) {
                values.insert(edge.true_source().to_owned());
            }
        }
    }
    let max_size = 10usize.max(graph.nodes.len() / 4);
    let mut groups = BTreeMap::<usize, Vec<String>>::new();
    for (id, cid) in assignments.iter() {
        groups.entry(*cid).or_default().push(id.clone());
    }
    let mut first_pass = Vec::new();
    for members in groups.values_mut() {
        members.sort();
        if members.len() > max_size {
            first_pass.extend(split_community(graph, members));
        } else {
            first_pass.push(members.clone());
        }
    }
    let mut final_groups = Vec::new();
    for members in first_pass {
        let internal_edges = members
            .iter()
            .map(|id| {
                adjacency
                    .get(id)
                    .map(|neighbors| {
                        neighbors
                            .iter()
                            .filter(|neighbor| members.binary_search(neighbor).is_ok())
                            .count()
                    })
                    .unwrap_or(0)
            })
            .sum::<usize>()
            / 2;
        let possible = members
            .len()
            .saturating_mul(members.len().saturating_sub(1))
            / 2;
        if members.len() >= 50 && possible > 0 && internal_edges as f64 / (possible as f64) < 0.05 {
            let split = split_community(graph, &members);
            if split.len() > 1 {
                final_groups.extend(split);
            } else {
                final_groups.push(members);
            }
        } else {
            final_groups.push(members);
        }
    }
    assignments.clear();
    for (community, members) in final_groups.into_iter().enumerate() {
        for id in members {
            assignments.insert(id, community);
        }
    }
}

fn split_community(graph: &KnowledgeGraph, members: &[String]) -> Vec<Vec<String>> {
    let member_set: BTreeSet<_> = members.iter().map(String::as_str).collect();
    let mut edges: Vec<_> = graph
        .links
        .iter()
        .filter(|edge| {
            edge.true_source() != edge.true_target()
                && member_set.contains(edge.true_source())
                && member_set.contains(edge.true_target())
        })
        .map(|edge| {
            (
                edge.true_source().to_owned(),
                edge.true_target().to_owned(),
                edge.extra
                    .get("weight")
                    .and_then(|value| value.as_f64())
                    .unwrap_or(1.0),
            )
        })
        .collect();
    edges.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    if edges.is_empty() {
        return members.iter().cloned().map(|member| vec![member]).collect();
    }
    let mut builder = LabeledNetworkBuilder::new();
    let network = builder.build(edges.into_iter(), true);
    let mut rng = SmallRng::seed_from_u64(42);
    let Ok((_, partition)) = leiden::leiden(
        network.compact(),
        None,
        Some(1),
        Some(1.0),
        Some(0.001),
        &mut rng,
        true,
        None,
    ) else {
        return vec![members.to_vec()];
    };
    let mut groups = BTreeMap::<usize, Vec<String>>::new();
    let mut covered = BTreeSet::new();
    for item in &partition {
        let label = network.label_for(item.node_id).to_owned();
        covered.insert(label.clone());
        groups.entry(item.cluster).or_default().push(label);
    }
    for member in members {
        if !covered.contains(member) {
            groups
                .entry(groups.keys().next_back().copied().unwrap_or(0) + 1)
                .or_default()
                .push(member.clone());
        }
    }
    let mut values: Vec<_> = groups.into_values().collect();
    for group in &mut values {
        group.sort();
    }
    if values.len() <= 1 {
        vec![members.to_vec()]
    } else {
        values
    }
}

/// Greedily map new communities onto prior IDs by maximum member overlap.
pub fn remap_communities_to_previous(current: &mut KnowledgeGraph, previous: &KnowledgeGraph) {
    let mut old = BTreeMap::<i64, BTreeSet<String>>::new();
    let mut new = BTreeMap::<i64, BTreeSet<String>>::new();
    for node in &previous.nodes {
        if let Some(cid) = node.community {
            old.entry(cid).or_default().insert(node.id.clone());
        }
    }
    for node in &current.nodes {
        if let Some(cid) = node.community {
            new.entry(cid).or_default().insert(node.id.clone());
        }
    }
    let pure_groups = new
        .iter()
        .map(|(id, members)| (*id, members.iter().cloned().collect::<Vec<_>>()))
        .collect();
    let previous_assignments = old
        .iter()
        .flat_map(|(id, members)| members.iter().map(move |member| (member.clone(), *id)))
        .collect();
    let remapped = remap_community_map(&pure_groups, &previous_assignments);
    let mapping = remapped
        .iter()
        .flat_map(|(final_id, members)| {
            members.iter().filter_map(|member| {
                new.iter()
                    .find(|(_, candidates)| candidates.contains(member))
                    .map(|(new_id, _)| (*new_id, *final_id))
            })
        })
        .collect::<BTreeMap<_, _>>();
    let old_names: BTreeMap<_, _> = previous
        .nodes
        .iter()
        .filter_map(|n| Some((n.community?, n.extra.get("community_name")?.clone())))
        .collect();
    for node in &mut current.nodes {
        if let Some(cid) = node.community {
            let mapped = mapping[&cid];
            node.community = Some(mapped);
            if let Some(name) = old_names.get(&mapped) {
                node.extra.insert("community_name".into(), name.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weighted_label_aggregation_is_order_independent_and_finite() {
        let first = aggregate_weighted_labels(vec![(0_i64, 1_i64, 1e16), (1, 0, 1.0), (0, 1, 1.0)]);
        let second =
            aggregate_weighted_labels(vec![(0_i64, 1_i64, 1.0), (1, 0, 1.0), (0, 1, 1e16)]);

        assert_eq!(first, second);
        assert_eq!(first[&(0, 1)], 10_000_000_000_000_002.0);
    }

    #[test]
    fn weighted_label_aggregation_saturates_finite_overflow() {
        let weights = aggregate_weighted_labels(vec![(0_i64, 1_i64, f64::MAX), (1, 0, f64::MAX)]);

        assert_eq!(weights[&(0, 1)], f64::MAX);
    }

    #[test]
    fn empty_graph_is_supported() {
        let mut graph = KnowledgeGraph::default();
        cluster(&mut graph).unwrap();
    }
}
