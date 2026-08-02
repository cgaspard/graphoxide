//! Deterministic extraction merge and endpoint repair.

use graphoxide_core::{normalize_id, Edge, Extraction, KnowledgeGraph, Node};
use std::collections::{BTreeMap, BTreeSet};

pub fn build_graph(extractions: &[Extraction]) -> anyhow::Result<KnowledgeGraph> {
    let mut nodes: BTreeMap<String, Node> = BTreeMap::new();
    for extraction in extractions {
        for node in &extraction.nodes {
            if node.id.is_empty() {
                continue;
            }
            if let Some(existing) = nodes.get_mut(&node.id) {
                merge_node(existing, node);
            } else {
                nodes.insert(node.id.clone(), node.clone());
            }
        }
    }

    // Merge source-less semantic/annotation ghosts onto a unique sourced AST node
    // with the same normalized (source_file, label) identity.
    let mut canonical_by_key: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for node in nodes.values() {
        if !node.source_file.is_empty() {
            canonical_by_key
                .entry((path_key(&node.source_file), node.label.to_lowercase()))
                .or_default()
                .push(node.id.clone());
        }
    }
    let mut remap = BTreeMap::new();
    for node in nodes.values() {
        let key = (path_key(&node.source_file), node.label.to_lowercase());
        if let Some(candidates) = canonical_by_key.get(&key) {
            if candidates.len() == 1
                && candidates[0] != node.id
                && node.extra.get("_origin").and_then(|v| v.as_str()) != "ast".into()
            {
                remap.insert(node.id.clone(), candidates[0].clone());
            }
        }
    }
    for old in remap.keys() {
        nodes.remove(old);
    }

    let mut normalized: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for id in nodes.keys() {
        normalized
            .entry(normalize_id(id))
            .or_default()
            .push(id.clone());
    }
    let mut legacy: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for node in nodes.values() {
        let stem = std::path::Path::new(&node.source_file)
            .file_stem()
            .and_then(|v| v.to_str())
            .unwrap_or("");
        legacy
            .entry(normalize_id(&format!(
                "{stem}_{}",
                node.label.trim_start_matches('.').trim_end_matches("()")
            )))
            .or_default()
            .push(node.id.clone());
    }

    let mut all_edges: Vec<Edge> = extractions
        .iter()
        .flat_map(|e| e.edges.iter().cloned())
        .collect();
    all_edges.sort_by(|a, b| {
        (a.true_source(), a.true_target(), a.relation.as_str()).cmp(&(
            b.true_source(),
            b.true_target(),
            b.relation.as_str(),
        ))
    });
    let mut links = Vec::new();
    let mut seen_pairs: BTreeSet<(String, String)> = BTreeSet::new();
    let mut seen_reverse: BTreeSet<(String, String)> = BTreeSet::new();
    for mut edge in all_edges {
        let source = repair(edge.true_source(), &nodes, &normalized, &legacy, &remap);
        let target = repair(edge.true_target(), &nodes, &normalized, &legacy, &remap);
        let (Some(source), Some(target)) = (source, target) else {
            continue;
        };
        if source == target
            && matches!(
                edge.relation.as_str(),
                "imports" | "imports_from" | "re_exports"
            )
        {
            continue;
        }
        let source_node = &nodes[&source];
        let target_node = &nodes[&target];
        let source_family = language_family(&source_node.source_file);
        let target_family = language_family(&target_node.source_file);
        let cross_language =
            source_family.is_some() && target_family.is_some() && source_family != target_family;
        if cross_language
            && ((edge.relation == "calls"
                && edge.confidence != graphoxide_core::Confidence::Extracted)
                || matches!(
                    edge.relation.as_str(),
                    "imports" | "imports_from" | "references"
                ))
        {
            continue;
        }
        let key = (source.clone(), target.clone());
        let reverse = (target.clone(), source.clone());
        if seen_reverse.contains(&key) {
            continue;
        } // an earlier reverse-direction edge wins
        edge.source = source.clone();
        edge.target = target.clone();
        edge.extra.insert("_src".into(), source.into());
        edge.extra.insert("_tgt".into(), target.into());
        edge.extra.remove("target_file");
        edge.extra.remove("local_alias");
        for key in ["weight", "confidence_score"] {
            if let Some(value) = edge.extra.get(key) {
                let number = value
                    .as_f64()
                    .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
                    .filter(|number| number.is_finite() && *number >= 0.0)
                    .unwrap_or(1.0);
                edge.extra.insert(key.into(), number.into());
            }
        }
        if edge.source_file.is_empty() {
            edge.source_file = if !source_node.source_file.is_empty() {
                source_node.source_file.clone()
            } else {
                target_node.source_file.clone()
            };
        }
        edge.extra
            .entry("confidence_score".into())
            .or_insert_with(|| edge.confidence.default_score().into());
        if let Some(position) = links
            .iter()
            .position(|prior: &Edge| prior.source == edge.source && prior.target == edge.target)
        {
            links[position] = edge
        } else {
            seen_pairs.insert(key.clone());
            seen_reverse.insert(reverse);
            links.push(edge)
        }
    }
    let node_ids: BTreeSet<_> = nodes.keys().cloned().collect();
    let mut hyperedges = Vec::new();
    let mut hyperedge_ids = BTreeSet::new();
    for extraction in extractions {
        for raw in &extraction.hyperedges {
            let Some(mut object) = raw.as_object().cloned() else {
                continue;
            };
            if !object.get("nodes").is_some_and(|v| v.is_array()) {
                for alias in ["members", "node_ids"] {
                    if let Some(value) = object.remove(alias) {
                        object.insert("nodes".into(), value);
                        break;
                    }
                }
            }
            let mut members: Vec<_> = object
                .get("nodes")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str())
                .filter_map(|id| repair(id, &nodes, &normalized, &legacy, &remap))
                .filter(|id| node_ids.contains(id))
                .collect();
            members.sort();
            members.dedup();
            if members.is_empty() {
                continue;
            }
            object.insert("nodes".into(), serde_json::json!(members));
            let base = object
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("hyperedge");
            let mut id = base.to_owned();
            if !hyperedge_ids.insert(id.clone()) {
                id = graphoxide_core::make_id(&[
                    base,
                    object
                        .get("source_file")
                        .and_then(|v| v.as_str())
                        .unwrap_or("duplicate"),
                ]);
                hyperedge_ids.insert(id.clone());
            }
            object.insert("id".into(), id.into());
            hyperedges.push(object.into());
        }
    }
    let mut graph = KnowledgeGraph {
        directed: false,
        multigraph: false,
        nodes: nodes.into_values().collect(),
        links,
        hyperedges,
        extra: BTreeMap::from([("graph".into(), serde_json::json!({}))]),
    };
    crate::dedup::deduplicate(&mut graph);
    Ok(graph)
}

fn merge_node(existing: &mut Node, incoming: &Node) {
    let existing_ast = existing.extra.get("_origin").and_then(|v| v.as_str()) == Some("ast");
    let incoming_ast = incoming.extra.get("_origin").and_then(|v| v.as_str()) == Some("ast");
    if incoming_ast && !existing_ast {
        let old_extra = existing.extra.clone();
        *existing = incoming.clone();
        for (key, value) in old_extra {
            existing.extra.entry(key).or_insert(value);
        }
    } else if existing_ast && !incoming_ast {
        for (key, value) in &incoming.extra {
            existing.extra.insert(key.clone(), value.clone());
        }
        if existing.source_location.is_none() {
            existing.source_location = incoming.source_location.clone();
        }
    } else {
        let mut merged = incoming.clone();
        for (key, value) in &existing.extra {
            merged
                .extra
                .entry(key.clone())
                .or_insert_with(|| value.clone());
        }
        if merged.label.is_empty() {
            merged.label = existing.label.clone();
        }
        if merged.source_file.is_empty() {
            merged.source_file = existing.source_file.clone();
        }
        if merged.source_location.is_none() {
            merged.source_location = existing.source_location.clone();
        }
        *existing = merged;
    }
}

fn language_family(path: &str) -> Option<&'static str> {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|v| v.to_str())?
        .to_ascii_lowercase();
    Some(match ext.as_str() {
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts" => "js",
        "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" => "c",
        "cs" | "razor" | "cshtml" => "csharp",
        "py" | "pyi" => "python",
        "java" => "java",
        "go" => "go",
        "rs" => "rust",
        "rb" => "ruby",
        _ => return None,
    })
}

fn repair(
    value: &str,
    nodes: &BTreeMap<String, Node>,
    normalized: &BTreeMap<String, Vec<String>>,
    legacy: &BTreeMap<String, Vec<String>>,
    remap: &BTreeMap<String, String>,
) -> Option<String> {
    let value = remap.get(value).map(String::as_str).unwrap_or(value);
    if nodes.contains_key(value) {
        return Some(value.into());
    }
    let key = normalize_id(value);
    for index in [normalized, legacy] {
        if let Some(ids) = index.get(&key) {
            if ids.len() == 1 {
                return Some(ids[0].clone());
            }
        }
    }
    None
}
fn path_key(value: &str) -> String {
    value
        .replace('\\', "/")
        .trim_start_matches("./")
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use graphoxide_core::{Confidence, Edge};
    fn node(id: &str) -> Node {
        Node {
            id: id.into(),
            label: id.into(),
            file_type: "code".into(),
            source_file: "a.py".into(),
            source_location: None,
            community: None,
            extra: BTreeMap::new(),
        }
    }
    #[test]
    fn repairs_normalized_endpoints_and_drops_dangling() {
        let extraction = Extraction {
            nodes: vec![node("foo_bar"), node("target")],
            edges: vec![
                Edge {
                    source: "Foo-Bar".into(),
                    target: "target".into(),
                    relation: "calls".into(),
                    confidence: Confidence::Inferred,
                    source_file: "a.py".into(),
                    extra: BTreeMap::new(),
                },
                Edge {
                    source: "missing".into(),
                    target: "target".into(),
                    relation: "calls".into(),
                    confidence: Confidence::Inferred,
                    source_file: "a.py".into(),
                    extra: BTreeMap::new(),
                },
            ],
            hyperedges: Vec::new(),
        };
        let graph = build_graph(&[extraction]).unwrap();
        assert_eq!(graph.links.len(), 1);
        assert_eq!(graph.links[0].source, "foo_bar");
    }
}
