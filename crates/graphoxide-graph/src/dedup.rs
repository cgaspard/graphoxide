//! Deterministic semantic-node deduplication.
use graphoxide_core::{normalize_id, KnowledgeGraph};
use rapidfuzz::distance::{jaro, jaro_winkler};
use std::collections::{BTreeMap, BTreeSet, HashMap};
pub fn deduplicate(graph: &mut KnowledgeGraph) -> usize {
    let mut remap: BTreeMap<String, String> = BTreeMap::new();
    let mut by_label: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, node) in graph.nodes.iter().enumerate() {
        if node.file_type != "code" {
            by_label
                .entry(normalize_id(&node.label))
                .or_default()
                .push(i)
        }
    }
    for indices in by_label.values() {
        if indices.len() < 2 {
            continue;
        }
        let winner = *indices
            .iter()
            .min_by_key(|i| rank(&graph.nodes[**i]))
            .unwrap();
        for &loser in indices {
            if loser == winner {
                continue;
            }
            let same = graph.nodes[loser].source_file == graph.nodes[winner].source_file;
            let concepts = graph.nodes[loser].file_type == "concept"
                && graph.nodes[winner].file_type == "concept";
            if same || (concepts && entropy(&graph.nodes[winner].label) >= 2.5) {
                remap.insert(
                    graph.nodes[loser].id.clone(),
                    graph.nodes[winner].id.clone(),
                );
            }
        }
    }
    let candidates: Vec<_> = graph
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| {
            n.file_type != "code" && !remap.contains_key(&n.id) && n.label.chars().count() >= 4
        })
        .map(|(i, _)| i)
        .collect();
    for (ai, &a) in candidates.iter().enumerate() {
        for &b in &candidates[ai + 1..] {
            let na = &graph.nodes[a];
            let nb = &graph.nodes[b];
            let norm_a = normalize_id(&na.label).replace('_', " ");
            let norm_b = normalize_id(&nb.label).replace('_', " ");
            if numeric_tokens(&norm_a) != numeric_tokens(&norm_b)
                && (!numeric_tokens(&norm_a).is_empty() || !numeric_tokens(&norm_b).is_empty())
            {
                continue;
            }
            let (short, long) = if norm_a.len() <= norm_b.len() {
                (&norm_a, &norm_b)
            } else {
                (&norm_b, &norm_a)
            };
            if (long.starts_with(short) && long != short)
                || (short.chars().count() < 8 && short.chars().count() != long.chars().count())
            {
                continue;
            }
            let cross = na.source_file != nb.source_file;
            let mut sim = if cross && norm_a.chars().count() >= 12 {
                jaro::similarity(norm_a.chars(), norm_b.chars())
            } else {
                jaro_winkler::similarity(norm_a.chars(), norm_b.chars())
            };
            if na.community.is_some() && na.community == nb.community {
                sim += 0.05;
            }
            if sim >= 0.92 {
                let (winner, loser) = if rank(na) <= rank(nb) {
                    (na, nb)
                } else {
                    (nb, na)
                };
                remap.insert(loser.id.clone(), winner.id.clone());
            }
        }
    }
    if remap.is_empty() {
        return 0;
    }
    for edge in &mut graph.links {
        if let Some(id) = remap.get(edge.true_source()) {
            edge.source = id.clone();
            edge.extra.insert("_src".into(), id.clone().into());
        }
        if let Some(id) = remap.get(edge.true_target()) {
            edge.target = id.clone();
            edge.extra.insert("_tgt".into(), id.clone().into());
        }
    }
    graph.nodes.retain(|n| !remap.contains_key(&n.id));
    let mut seen = BTreeSet::new();
    graph.links.retain(|e| {
        e.source != e.target
            && seen.insert((e.source.clone(), e.target.clone(), e.relation.clone()))
    });
    remap.len()
}
fn numeric_tokens(value: &str) -> Vec<u64> {
    let mut result = Vec::new();
    let mut current = String::new();
    for ch in value.chars() {
        if ch.is_ascii_digit() {
            current.push(ch);
        } else if !current.is_empty() {
            result.push(current.parse().unwrap_or(0));
            current.clear();
        }
    }
    if !current.is_empty() {
        result.push(current.parse().unwrap_or(0));
    }
    result.sort_unstable();
    result
}
fn rank(node: &graphoxide_core::Node) -> (std::cmp::Reverse<usize>, usize, String, String) {
    (
        std::cmp::Reverse(usize::from(
            node.extra
                .get("defines_own_id")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        )),
        node.label.len(),
        node.label.clone(),
        node.source_file.clone(),
    )
}
fn entropy(value: &str) -> f64 {
    let mut counts = HashMap::new();
    let len = value.chars().count() as f64;
    for c in value.chars() {
        *counts.entry(c).or_insert(0usize) += 1
    }
    counts
        .values()
        .map(|n| {
            let p = *n as f64 / len;
            -p * p.log2()
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn entropy_distinguishes_repetition() {
        assert!(entropy("architecture") > entropy("aaaaaaaa"));
    }
    #[test]
    fn numeric_tokens_ignore_zero_padding() {
        assert_eq!(numeric_tokens("phase 09"), numeric_tokens("phase 9"));
        assert_ne!(numeric_tokens("adr 11"), numeric_tokens("adr 13"));
    }
}
