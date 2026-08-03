//! Persistent cross-repository graph composition.

use graphoxide_core::{read_graph_with_cap, write_graph_atomic, write_json_atomic, KnowledgeGraph};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlobalRepoRecord {
    pub added_at: String,
    pub source_path: String,
    pub node_count: usize,
    pub edge_count: usize,
    pub source_hash: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct GlobalManifest {
    #[serde(default = "manifest_version")]
    version: u32,
    #[serde(default)]
    repos: BTreeMap<String, GlobalRepoRecord>,
}

fn manifest_version() -> u32 {
    1
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GlobalAddResult {
    pub repo_tag: String,
    pub nodes_added: usize,
    pub nodes_removed: usize,
    pub skipped: bool,
    pub warning: Option<String>,
}

#[derive(Debug, Clone)]
pub struct GlobalGraphStore {
    directory: PathBuf,
    max_graph_bytes: u64,
}

impl GlobalGraphStore {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self::with_cap(directory, graphoxide_core::max_graph_bytes())
    }

    pub fn with_cap(directory: impl Into<PathBuf>, max_graph_bytes: u64) -> Self {
        Self {
            directory: directory.into(),
            max_graph_bytes,
        }
    }

    pub fn graph_path(&self) -> PathBuf {
        self.directory.join("global-graph.json")
    }

    pub fn manifest_path(&self) -> PathBuf {
        self.directory.join("global-manifest.json")
    }

    pub fn load_graph(&self) -> anyhow::Result<KnowledgeGraph> {
        let path = self.graph_path();
        if path.is_file() {
            read_graph_with_cap(path, self.max_graph_bytes)
        } else {
            Ok(KnowledgeGraph::default())
        }
    }

    pub fn list(&self) -> anyhow::Result<BTreeMap<String, GlobalRepoRecord>> {
        Ok(self.load_manifest()?.repos)
    }

    pub fn add(
        &self,
        source_path: impl AsRef<Path>,
        repo_tag: &str,
    ) -> anyhow::Result<GlobalAddResult> {
        anyhow::ensure!(!repo_tag.is_empty(), "repo tag cannot be empty");
        let source_path = source_path.as_ref();
        graphoxide_core::check_graph_file_size_cap_with(source_path, self.max_graph_bytes)?;
        let canonical_source = source_path.canonicalize().map_err(|error| {
            anyhow::anyhow!("graph not found: {}: {error}", source_path.display())
        })?;
        let bytes = std::fs::read(&canonical_source)?;
        let source_hash = hex::encode(Sha256::digest(&bytes))[..16].to_owned();
        let source_text = canonical_source.display().to_string();
        let mut manifest = self.load_manifest()?;
        let warning = manifest.repos.get(repo_tag).and_then(|record| {
            (record.source_path != source_text).then(|| {
                format!(
                    "repo tag '{repo_tag}' previously pointed to {:?}, now updating to {:?}",
                    record.source_path, source_text
                )
            })
        });
        if manifest
            .repos
            .get(repo_tag)
            .is_some_and(|record| record.source_hash == source_hash)
        {
            return Ok(GlobalAddResult {
                repo_tag: repo_tag.to_owned(),
                skipped: true,
                warning,
                ..GlobalAddResult::default()
            });
        }

        let source_graph = read_graph_with_cap(&canonical_source, self.max_graph_bytes)?;
        let prefixed = prefix_graph_for_global(&source_graph, repo_tag);
        let prefixed_edge_count = prefixed.links.len();
        let mut global = self.load_graph()?;
        let nodes_removed = prune_repo_from_graph(&mut global, repo_tag);

        let external_labels: BTreeMap<_, _> = global
            .nodes
            .iter()
            .filter(|node| node.source_file.is_empty() && !node.label.is_empty())
            .map(|node| (node.label.clone(), node.id.clone()))
            .collect();
        let remap: BTreeMap<_, _> = prefixed
            .nodes
            .iter()
            .filter_map(|node| {
                (node.source_file.is_empty())
                    .then(|| {
                        external_labels
                            .get(&node.label)
                            .map(|id| (node.id.clone(), id.clone()))
                    })
                    .flatten()
            })
            .collect();
        let nodes_added = prefixed.nodes.len() - remap.len();
        global.nodes.extend(
            prefixed
                .nodes
                .into_iter()
                .filter(|node| !remap.contains_key(&node.id)),
        );
        for mut edge in prefixed.links {
            let source = remap
                .get(edge.true_source())
                .cloned()
                .unwrap_or_else(|| edge.true_source().to_owned());
            let target = remap
                .get(edge.true_target())
                .cloned()
                .unwrap_or_else(|| edge.true_target().to_owned());
            if source == target {
                continue;
            }
            edge.source = source.clone();
            edge.target = target.clone();
            edge.extra.insert("_src".into(), source.into());
            edge.extra.insert("_tgt".into(), target.into());
            global.links.push(edge);
        }
        for mut hyperedge in prefixed.hyperedges {
            let Some(object) = hyperedge.as_object_mut() else {
                continue;
            };
            if let Some(members) = object
                .get_mut("nodes")
                .and_then(serde_json::Value::as_array_mut)
            {
                for member in members {
                    if let Some(mapped) = member.as_str().and_then(|id| remap.get(id)) {
                        *member = mapped.clone().into();
                    }
                }
            }
            global.hyperedges.push(hyperedge);
        }
        sort_and_deduplicate(&mut global);

        std::fs::create_dir_all(&self.directory)?;
        write_graph_atomic(self.graph_path(), &global, true)?;
        manifest.repos.insert(
            repo_tag.to_owned(),
            GlobalRepoRecord {
                added_at: unix_timestamp().to_string(),
                source_path: source_text,
                node_count: nodes_added,
                edge_count: prefixed_edge_count,
                source_hash,
            },
        );
        self.save_manifest(&manifest)?;
        Ok(GlobalAddResult {
            repo_tag: repo_tag.to_owned(),
            nodes_added,
            nodes_removed,
            skipped: false,
            warning,
        })
    }

    pub fn remove(&self, repo_tag: &str) -> anyhow::Result<usize> {
        let mut manifest = self.load_manifest()?;
        anyhow::ensure!(
            manifest.repos.remove(repo_tag).is_some(),
            "repo '{repo_tag}' not in global graph"
        );
        let mut graph = self.load_graph()?;
        let removed = prune_repo_from_graph(&mut graph, repo_tag);
        std::fs::create_dir_all(&self.directory)?;
        write_graph_atomic(self.graph_path(), &graph, true)?;
        self.save_manifest(&manifest)?;
        Ok(removed)
    }

    fn load_manifest(&self) -> anyhow::Result<GlobalManifest> {
        let path = self.manifest_path();
        if !path.is_file() {
            return Ok(GlobalManifest {
                version: 1,
                repos: BTreeMap::new(),
            });
        }
        let bytes = std::fs::read(&path)?;
        serde_json::from_slice(&bytes)
            .map_err(|error| anyhow::anyhow!("invalid global manifest {}: {error}", path.display()))
    }

    fn save_manifest(&self, manifest: &GlobalManifest) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.directory)?;
        write_json_atomic(self.manifest_path(), manifest, true)
    }
}

/// Prefix every graph identity for cross-project isolation without changing
/// display labels.
pub fn prefix_graph_for_global(graph: &KnowledgeGraph, repo_tag: &str) -> KnowledgeGraph {
    let mut graph = graph.clone();
    let remap: BTreeMap<_, _> = graph
        .nodes
        .iter()
        .map(|node| (node.id.clone(), format!("{repo_tag}::{}", node.id)))
        .collect();
    for node in &mut graph.nodes {
        let local_id = node.id.clone();
        node.id = remap[&local_id].clone();
        node.extra.insert("repo".into(), repo_tag.into());
        node.extra
            .entry("local_id".into())
            .or_insert_with(|| local_id.into());
    }
    for edge in &mut graph.links {
        let old_source = edge.source.clone();
        let old_target = edge.target.clone();
        edge.source = remap
            .get(&old_source)
            .cloned()
            .unwrap_or_else(|| format!("{repo_tag}::{old_source}"));
        edge.target = remap
            .get(&old_target)
            .cloned()
            .unwrap_or_else(|| format!("{repo_tag}::{old_target}"));
        for marker in ["_src", "_tgt"] {
            let Some(old) = edge.extra.get(marker).and_then(|value| value.as_str()) else {
                continue;
            };
            if let Some(mapped) = remap.get(old) {
                edge.extra.insert(marker.into(), mapped.clone().into());
            }
        }
    }
    for hyperedge in &mut graph.hyperedges {
        let Some(object) = hyperedge.as_object_mut() else {
            continue;
        };
        if let Some(id) = object.get("id").and_then(|value| value.as_str()) {
            object.insert("id".into(), format!("{repo_tag}::{id}").into());
        }
        if let Some(members) = object
            .get_mut("nodes")
            .and_then(serde_json::Value::as_array_mut)
        {
            for member in members {
                if let Some(mapped) = member.as_str().and_then(|id| remap.get(id)) {
                    *member = mapped.clone().into();
                }
            }
        }
        object.insert("repo".into(), repo_tag.into());
    }
    graph
}

/// Remove every node owned by `repo_tag` and all relationships incident to it.
pub fn prune_repo_from_graph(graph: &mut KnowledgeGraph, repo_tag: &str) -> usize {
    let removed_ids: BTreeSet<_> = graph
        .nodes
        .iter()
        .filter(|node| node.extra.get("repo").and_then(|value| value.as_str()) == Some(repo_tag))
        .map(|node| node.id.clone())
        .collect();
    graph.nodes.retain(|node| !removed_ids.contains(&node.id));
    graph.links.retain(|edge| {
        !removed_ids.contains(edge.true_source()) && !removed_ids.contains(edge.true_target())
    });
    graph.hyperedges.retain(|hyperedge| {
        hyperedge
            .get("nodes")
            .and_then(serde_json::Value::as_array)
            .is_none_or(|members| {
                members
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .all(|member| !removed_ids.contains(member))
            })
    });
    removed_ids.len()
}

fn sort_and_deduplicate(graph: &mut KnowledgeGraph) {
    graph.nodes.sort_by(|left, right| left.id.cmp(&right.id));
    graph.nodes.dedup_by(|left, right| left.id == right.id);
    graph.links.sort_by(|left, right| {
        (
            left.true_source(),
            left.true_target(),
            left.relation.as_str(),
        )
            .cmp(&(
                right.true_source(),
                right.true_target(),
                right.relation.as_str(),
            ))
    });
    graph.links.dedup_by(|left, right| {
        (
            left.true_source(),
            left.true_target(),
            left.relation.as_str(),
        ) == (
            right.true_source(),
            right.true_target(),
            right.relation.as_str(),
        )
    });
}

fn unix_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
