//! Deterministic Leiden community assignment.

use graphoxide_core::KnowledgeGraph;
use network_partitions::{leiden, network::LabeledNetworkBuilder};
use rand::{rngs::SmallRng, SeedableRng};
use std::collections::{BTreeMap, BTreeSet, HashMap};

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
    let labels: HashMap<i64, String> = groups
        .iter()
        .enumerate()
        .map(|(cid, nodes)| {
            let hub = nodes
                .iter()
                .min_by_key(|node| {
                    (
                        usize::MAX - degree.get(node.as_str()).copied().unwrap_or(0),
                        node.as_str(),
                    )
                })
                .unwrap();
            let label = graph
                .nodes
                .iter()
                .find(|n| &n.id == hub)
                .map(|n| n.label.trim_end_matches("()").to_owned())
                .unwrap_or_else(|| format!("Community {cid}"));
            (cid as i64, label)
        })
        .collect();
    for node in &mut graph.nodes {
        let cid = reindexed[&node.id];
        node.community = Some(cid);
        node.extra
            .insert("community_name".into(), labels[&cid].clone().into());
    }
    Ok(())
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
    let mut pairs = Vec::new();
    for (new_id, new_members) in &new {
        for (old_id, old_members) in &old {
            let overlap = new_members.intersection(old_members).count();
            if overlap > 0 {
                pairs.push((overlap, *new_id, *old_id));
            }
        }
    }
    pairs.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.2.cmp(&b.2))
            .then_with(|| a.1.cmp(&b.1))
    });
    let mut mapping = BTreeMap::new();
    let mut used_old = BTreeSet::new();
    for (_, new_id, old_id) in pairs {
        if !mapping.contains_key(&new_id) && used_old.insert(old_id) {
            mapping.insert(new_id, old_id);
        }
    }
    let mut next = old.keys().next_back().map(|v| v + 1).unwrap_or(0);
    for new_id in new.keys() {
        mapping.entry(*new_id).or_insert_with(|| {
            let assigned = next;
            next += 1;
            assigned
        });
    }
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
    fn empty_graph_is_supported() {
        let mut graph = KnowledgeGraph::default();
        cluster(&mut graph).unwrap();
    }
}
