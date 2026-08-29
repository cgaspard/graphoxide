//! Deterministic navigation topics projected from existing graph communities.

use graphoxide_core::KnowledgeGraph;
use graphoxide_graph::{communities, label_communities_by_hub, partition_weighted_labels};
use std::collections::{BTreeMap, BTreeSet};

/// A graph-derived navigation topic containing one or more existing communities.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Topic {
    pub id: String,
    pub label: String,
    pub communities: Vec<i64>,
}

/// A complete, deterministic placement of graph communities into topics.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TopicTree {
    pub topics: Vec<Topic>,
    pub community_paths: BTreeMap<i64, Vec<String>>,
}

fn is_community_fallback(community: i64, label: &str) -> bool {
    label == format!("Community {community}")
}

/// Project existing community membership into weighted, graph-derived topics.
pub fn derive_topic_tree(graph: &KnowledgeGraph) -> anyhow::Result<TopicTree> {
    let communities = communities(graph);
    if communities.is_empty() {
        return Ok(TopicTree::default());
    }

    let relationships = cross_community_relationships(graph);
    let groups = topic_groups(&relationships, &communities)?;
    let labels = label_communities_by_hub(graph, &communities);
    let mut cross_community_degree = BTreeMap::<i64, f64>::new();
    for (source, target, weight) in &relationships {
        for community in [source, target] {
            let degree = cross_community_degree.entry(*community).or_default();
            *degree = (*degree + weight).min(f64::MAX);
        }
    }
    let mut community_paths = BTreeMap::new();
    let topics = groups
        .into_iter()
        .enumerate()
        .map(|(index, communities)| {
            let id = format!("topic-{index}");
            for community in &communities {
                community_paths.insert(*community, vec![id.clone()]);
            }
            let representative = communities
                .iter()
                .min_by(|left, right| {
                    is_community_fallback(**left, &labels[*left])
                        .cmp(&is_community_fallback(**right, &labels[*right]))
                        .then_with(|| {
                            cross_community_degree
                                .get(right)
                                .copied()
                                .unwrap_or_default()
                                .total_cmp(
                                    &cross_community_degree
                                        .get(left)
                                        .copied()
                                        .unwrap_or_default(),
                                )
                        })
                        .then_with(|| left.cmp(right))
                })
                .expect("topic groups are non-empty");
            let representative_label = &labels[representative];
            Topic {
                id,
                label: if is_community_fallback(*representative, representative_label) {
                    format!("Topic {index}")
                } else {
                    representative_label.clone()
                },
                communities,
            }
        })
        .collect();
    Ok(TopicTree {
        topics,
        community_paths,
    })
}

fn topic_groups(
    relationships: &[(i64, i64, f64)],
    communities: &BTreeMap<i64, Vec<String>>,
) -> anyhow::Result<Vec<Vec<i64>>> {
    let mut groups = partition_weighted_labels(relationships.iter().copied()).unwrap_or_default();
    let assigned = groups.iter().flatten().copied().collect::<BTreeSet<_>>();
    groups.extend(
        communities
            .keys()
            .filter(|community| !assigned.contains(community))
            .map(|community| vec![*community]),
    );
    groups.sort();
    Ok(groups)
}

/// Aggregate current graph links that cross existing communities.
///
/// The returned pairs are undirected, ordered by community ID, and retain no
/// relation kind. This is intentionally the sole cross-community projection
/// used by both topic placement and structured-wiki navigation.
pub(crate) fn cross_community_relationships(graph: &KnowledgeGraph) -> Vec<(i64, i64, f64)> {
    let node_communities = graph
        .nodes
        .iter()
        .filter_map(|node| {
            node.community
                .map(|community| (node.id.as_str(), community))
        })
        .fold(
            BTreeMap::<&str, i64>::new(),
            |mut groups, (node, community)| {
                groups
                    .entry(node)
                    .and_modify(|current| *current = (*current).min(community))
                    .or_insert(community);
                groups
            },
        );
    let mut contributions = BTreeMap::<(i64, i64), Vec<f64>>::new();
    for link in &graph.links {
        let (Some(source), Some(target)) = (
            node_communities.get(link.true_source()),
            node_communities.get(link.true_target()),
        ) else {
            continue;
        };
        if source == target {
            continue;
        }
        let weight = link
            .extra
            .get("weight")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(1.0);
        if !weight.is_finite() || weight <= 0.0 {
            continue;
        }
        let pair = if source <= target {
            (*source, *target)
        } else {
            (*target, *source)
        };
        contributions.entry(pair).or_default().push(weight);
    }
    contributions
        .into_iter()
        .map(|((source, target), mut values)| {
            values.sort_by(f64::total_cmp);
            let weight = values.into_iter().fold(0.0, |total, value| {
                let sum = total + value;
                if sum.is_finite() {
                    sum
                } else {
                    f64::MAX
                }
            });
            (source, target, weight)
        })
        .collect()
}
