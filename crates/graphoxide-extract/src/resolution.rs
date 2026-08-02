//! Deterministic corpus-level ghost, import, and call resolution.
use graphoxide_core::{make_id, normalize_id, Extraction};
use std::collections::{BTreeMap, BTreeSet};
pub fn resolve(extractions: &mut [Extraction]) {
    disambiguate(extractions);
    let mut definitions: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    let mut call_definitions: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    let mut file_nodes: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for extraction in extractions.iter() {
        for node in &extraction.nodes {
            if node.source_file.is_empty() {
                continue;
            }
            if node.extra.get("type").and_then(|v| v.as_str()) == Some("function") {
                let label = normalize_id(node.label.trim_start_matches('.').trim_end_matches("()"));
                call_definitions
                    .entry(label)
                    .or_default()
                    .push((node.id.clone(), node.source_file.clone()));
                continue;
            }
            let label = normalize_id(node.label.trim_start_matches('.').trim_end_matches("()"));
            definitions
                .entry(label)
                .or_default()
                .push((node.id.clone(), node.source_file.clone()));
            if node.source_location.as_deref() == Some("L1") {
                let stem = std::path::Path::new(&node.source_file)
                    .file_stem()
                    .and_then(|v| v.to_str())
                    .unwrap_or("");
                file_nodes
                    .entry(normalize_id(stem))
                    .or_default()
                    .push(node.id.clone());
            }
        }
    }
    for values in definitions.values_mut() {
        values.sort();
        values.dedup()
    }
    for values in file_nodes.values_mut() {
        values.sort();
        values.dedup()
    }
    for values in call_definitions.values_mut() {
        values.sort();
        values.dedup();
    }
    let mut remap = BTreeMap::new();
    for extraction in extractions.iter() {
        for node in &extraction.nodes {
            let key = normalize_id(node.label.trim_start_matches('.').trim_end_matches("()"));
            if node.source_file.is_empty() {
                let origin = node
                    .extra
                    .get("origin_file")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if let Some(ids) = definitions.get(&key) {
                    let compatible: Vec<_> = ids
                        .iter()
                        .filter(|(_, source)| same_language_family(origin, source))
                        .collect();
                    if compatible.len() == 1 {
                        remap.insert(node.id.clone(), compatible[0].0.clone());
                    }
                }
            } else if node.extra.get("type").and_then(|v| v.as_str()) == Some("module") {
                if let Some(ids) = file_nodes.get(&key) {
                    if ids.len() == 1 {
                        remap.insert(node.id.clone(), ids[0].clone());
                    }
                }
            }
        }
    }
    for extraction in extractions {
        let import_targets: Vec<String> = extraction
            .edges
            .iter()
            .filter(|edge| matches!(edge.relation.as_str(), "imports" | "imports_from"))
            .map(|edge| normalize_id(edge.true_target()))
            .collect();
        extraction.edges.retain_mut(|edge| {
            if !edge
                .extra
                .get("unresolved_call")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                return true;
            }
            let callee = edge
                .extra
                .get("callee")
                .and_then(|v| v.as_str())
                .map(normalize_id)
                .unwrap_or_default();
            let candidates: Vec<_> = call_definitions
                .get(&callee)
                .into_iter()
                .flatten()
                .filter(|(_, source)| same_language_family(&edge.source_file, source))
                .filter(|(id, _)| id != edge.true_source())
                .filter(|(_, source)| {
                    let stem = std::path::Path::new(source)
                        .file_stem()
                        .and_then(|v| v.to_str())
                        .map(normalize_id)
                        .unwrap_or_default();
                    !stem.is_empty()
                        && import_targets
                            .iter()
                            .any(|target| target == &stem || target.ends_with(&format!("_{stem}")))
                })
                .collect();
            let target = if candidates.len() == 1 {
                Some(candidates[0].0.clone())
            } else {
                let mut ranked: Vec<_> = candidates
                    .into_iter()
                    .map(|(id, source)| (path_proximity(&edge.source_file, source), id))
                    .collect();
                ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
                (!ranked.is_empty() && (ranked.len() == 1 || ranked[0].0 > ranked[1].0))
                    .then(|| ranked[0].1.clone())
            };
            let Some(target) = target else { return false };
            edge.target = target.clone();
            edge.extra.insert("_tgt".into(), target.into());
            edge.extra.remove("unresolved_call");
            edge.extra.remove("callee");
            true
        });
        for edge in &mut extraction.edges {
            if let Some(id) = remap.get(edge.true_source()) {
                edge.source = id.clone();
                edge.extra.insert("_src".into(), id.clone().into());
            }
            if let Some(id) = remap.get(edge.true_target()) {
                edge.target = id.clone();
                edge.extra.insert("_tgt".into(), id.clone().into());
            }
        }
        extraction
            .nodes
            .retain(|node| !remap.contains_key(&node.id));
        let mut seen_calls = BTreeSet::new();
        extraction.edges.retain(|edge| {
            !edge
                .extra
                .get("resolution_only")
                .and_then(|value| value.as_bool())
                .unwrap_or(false)
                && (!matches!(edge.relation.as_str(), "calls" | "indirect_call")
                    || seen_calls.insert((
                        edge.true_source().to_owned(),
                        edge.true_target().to_owned(),
                        edge.relation.clone(),
                    )))
        });
    }
}

fn path_proximity(a: &str, b: &str) -> usize {
    let a: Vec<_> = a.split('/').collect();
    let b: Vec<_> = b.split('/').collect();
    a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count()
}

fn same_language_family(a: &str, b: &str) -> bool {
    if a.is_empty() {
        return true;
    }
    fn family(path: &str) -> &str {
        match std::path::Path::new(path)
            .extension()
            .and_then(|v| v.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str()
        {
            "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts" => "js",
            "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" => "c",
            "cs" | "razor" | "cshtml" => "csharp",
            "sh" | "bash" | "zsh" => "shell",
            // These literals live for the duration of this function and cover
            // all families that participate in the shared resolver.
            "py" | "pyi" => "python",
            "java" => "java",
            "go" => "go",
            "rs" => "rust",
            "rb" => "ruby",
            _ => "other",
        }
    }
    family(a) == family(b)
}

fn disambiguate(extractions: &mut [Extraction]) {
    let mut groups: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for extraction in extractions.iter() {
        for node in &extraction.nodes {
            if !matches!(
                node.extra.get("type").and_then(|v| v.as_str()),
                Some("module" | "namespace")
            ) {
                let source_key = if node.source_file.is_empty() {
                    node.extra
                        .get("origin_file")
                        .and_then(|value| value.as_str())
                        .unwrap_or("")
                } else {
                    &node.source_file
                };
                if source_key.is_empty() {
                    continue;
                }
                groups
                    .entry(node.id.clone())
                    .or_default()
                    .insert(source_key.to_owned());
            }
        }
    }
    let ambiguous: BTreeSet<_> = groups
        .into_iter()
        .filter(|(_, sources)| sources.len() > 1)
        .map(|(id, _)| id)
        .collect();
    if ambiguous.is_empty() {
        return;
    }
    for extraction in extractions {
        let mut local = BTreeMap::new();
        for node in &mut extraction.nodes {
            if ambiguous.contains(&node.id) {
                let old = node.id.clone();
                let source_key = if node.source_file.is_empty() {
                    node.extra
                        .get("origin_file")
                        .and_then(|value| value.as_str())
                        .unwrap_or("")
                } else {
                    &node.source_file
                };
                if source_key.is_empty() {
                    continue;
                }
                let new = make_id(&[source_key, &old]);
                node.id = new.clone();
                local.insert(old, new);
            }
        }
        for edge in &mut extraction.edges {
            if let Some(new) = local.get(edge.true_source()) {
                edge.source = new.clone();
                edge.extra.insert("_src".into(), new.clone().into());
            }
            if let Some(new) = local.get(edge.true_target()) {
                edge.target = new.clone();
                edge.extra.insert("_tgt".into(), new.clone().into());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn empty_is_safe() {
        let mut values = [];
        resolve(&mut values);
    }
}
