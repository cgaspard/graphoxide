//! Lossless composition of independently-built repository graphs.

use graphoxide_core::KnowledgeGraph;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

fn repo_components(path: &Path) -> Vec<String> {
    let repository = path
        .parent()
        .and_then(Path::parent)
        .unwrap_or_else(|| path.parent().unwrap_or(path));
    repository
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .filter(|component| !component.is_empty() && *component != "/")
        .map(|component| {
            component
                .chars()
                .map(|character| {
                    if character.is_alphanumeric() || matches!(character, '-' | '_') {
                        character
                    } else {
                        '_'
                    }
                })
                .collect()
        })
        .collect()
}

pub fn distinct_repo_tags(paths: &[PathBuf]) -> Vec<String> {
    let components: Vec<_> = paths.iter().map(|path| repo_components(path)).collect();
    let max_depth = components.iter().map(Vec::len).max().unwrap_or(1).max(1);
    for depth in 1..=max_depth {
        let candidates: Vec<_> = components
            .iter()
            .map(|parts| {
                let start = parts.len().saturating_sub(depth);
                let value = parts[start..].join("__");
                if value.is_empty() {
                    "repo".into()
                } else {
                    value
                }
            })
            .collect();
        let unique: HashSet<_> = candidates.iter().collect();
        if unique.len() == candidates.len() {
            return candidates;
        }
    }
    components
        .iter()
        .enumerate()
        .map(|(index, parts)| {
            format!(
                "{}__{}",
                if parts.is_empty() {
                    "repo".into()
                } else {
                    parts.join("__")
                },
                index + 1
            )
        })
        .collect()
}

pub fn merge_repository_graphs(inputs: Vec<(PathBuf, KnowledgeGraph)>) -> KnowledgeGraph {
    let paths: Vec<_> = inputs.iter().map(|(path, _)| path.clone()).collect();
    let tags = distinct_repo_tags(&paths);
    let mut merged = KnowledgeGraph::default();
    for ((_, graph), tag) in inputs.into_iter().zip(tags) {
        let mut remap = BTreeMap::new();
        for mut node in graph.nodes {
            let old_id = node.id.clone();
            node.id = format!("{tag}::{old_id}");
            node.extra.insert("repo".into(), tag.clone().into());
            remap.insert(old_id, node.id.clone());
            merged.nodes.push(node);
        }
        for mut edge in graph.links {
            let source = edge.true_source().to_owned();
            let target = edge.true_target().to_owned();
            edge.source = remap
                .get(&source)
                .cloned()
                .unwrap_or_else(|| format!("{tag}::{source}"));
            edge.target = remap
                .get(&target)
                .cloned()
                .unwrap_or_else(|| format!("{tag}::{target}"));
            edge.extra.remove("_src");
            edge.extra.remove("_tgt");
            edge.extra.insert("repo".into(), tag.clone().into());
            merged.links.push(edge);
        }
        for mut hyperedge in graph.hyperedges {
            if let Some(object) = hyperedge.as_object_mut() {
                if let Some(id) = object.get("id").and_then(|value| value.as_str()) {
                    object.insert("id".into(), format!("{tag}::{id}").into());
                }
                if let Some(members) = object
                    .get_mut("nodes")
                    .and_then(|value| value.as_array_mut())
                {
                    for member in members {
                        if let Some(id) = member.as_str() {
                            *member = remap
                                .get(id)
                                .cloned()
                                .unwrap_or_else(|| format!("{tag}::{id}"))
                                .into();
                        }
                    }
                }
                object.insert("repo".into(), tag.clone().into());
            }
            merged.hyperedges.push(hyperedge);
        }
    }
    merged.directed = false;
    merged.multigraph = false;
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use graphoxide_core::{Confidence, Edge, Node};

    fn node(id: &str, label: &str) -> Node {
        Node {
            id: id.into(),
            label: label.into(),
            file_type: "code".into(),
            source_file: label.into(),
            source_location: None,
            community: None,
            extra: BTreeMap::new(),
        }
    }

    #[test]
    fn test_merge_graphs_mixed_directed_and_multigraph() {
        let mut first = KnowledgeGraph {
            directed: true,
            nodes: vec![node("x", "x")],
            ..Default::default()
        };
        first.multigraph = false;
        let second = KnowledgeGraph {
            nodes: vec![node("y", "y")],
            ..Default::default()
        };
        let third = KnowledgeGraph {
            multigraph: true,
            nodes: vec![node("z", "z")],
            ..Default::default()
        };
        let merged = merge_repository_graphs(vec![
            ("r1/graphify-out/graph.json".into(), first),
            ("r2/graphify-out/graph.json".into(), second),
            ("r3/graphify-out/graph.json".into(), third),
        ]);
        assert_eq!(merged.nodes.len(), 3);
        assert!(!merged.directed);
        assert!(!merged.multigraph);
    }

    #[test]
    fn test_merge_graphs_same_named_repo_dirs_do_not_collapse() {
        let merged = merge_repository_graphs(vec![
            (
                "src/graphify-out/graph.json".into(),
                KnowledgeGraph {
                    nodes: vec![node("app", "app.js")],
                    ..Default::default()
                },
            ),
            (
                "frontend/src/graphify-out/graph.json".into(),
                KnowledgeGraph {
                    nodes: vec![node("app", "App.jsx")],
                    ..Default::default()
                },
            ),
        ]);
        assert_eq!(merged.nodes.len(), 2);
        assert_ne!(merged.nodes[0].id, merged.nodes[1].id);
        assert_eq!(
            merged
                .nodes
                .iter()
                .map(|node| node.label.as_str())
                .collect::<HashSet<_>>(),
            HashSet::from(["app.js", "App.jsx"])
        );
    }

    #[test]
    fn test_distinct_repo_tags_unit() {
        assert_eq!(
            distinct_repo_tags(&[
                "backend/graphify-out/graph.json".into(),
                "web/graphify-out/graph.json".into(),
            ]),
            ["backend", "web"]
        );
        for paths in [
            vec![
                "proj/src/graphify-out/graph.json".into(),
                "proj/frontend/src/graphify-out/graph.json".into(),
            ],
            vec![
                "a/src/graphify-out/graph.json".into(),
                "b/src/graphify-out/graph.json".into(),
                "c/src/graphify-out/graph.json".into(),
            ],
        ] {
            let tags = distinct_repo_tags(&paths);
            assert_eq!(tags.iter().collect::<HashSet<_>>().len(), tags.len());
        }
    }

    #[test]
    fn test_merge_graphs_preserves_import_edge_direction() {
        let edge = |source: &str, target: &str| Edge {
            source: source.into(),
            target: target.into(),
            relation: "imports_from".into(),
            confidence: Confidence::Extracted,
            source_file: String::new(),
            extra: BTreeMap::new(),
        };
        let first = KnowledgeGraph {
            nodes: ["collections", "empresa", "logger", "rota"]
                .into_iter()
                .map(|id| node(id, &format!("{id}.js")))
                .collect(),
            links: vec![
                edge("rota", "collections"),
                edge("rota", "empresa"),
                edge("rota", "logger"),
            ],
            ..Default::default()
        };
        let second = KnowledgeGraph {
            nodes: vec![node("main", "main.js"), node("utils", "utils.js")],
            links: vec![edge("main", "utils")],
            ..Default::default()
        };
        let merged = merge_repository_graphs(vec![
            ("repo1/graphify-out/graph.json".into(), first),
            ("repo2/graphify-out/graph.json".into(), second),
        ]);
        assert_eq!((merged.nodes.len(), merged.links.len()), (6, 4));
        for edge in merged
            .links
            .iter()
            .filter(|edge| edge.source.starts_with("repo1::"))
        {
            assert_eq!(edge.source, "repo1::rota");
            assert!(matches!(
                edge.target.as_str(),
                "repo1::collections" | "repo1::empresa" | "repo1::logger"
            ));
        }
        assert!(merged
            .links
            .iter()
            .any(|edge| { edge.source == "repo2::main" && edge.target == "repo2::utils" }));
    }
}
