//! Namespace-aware PHP type resolution.
//!
//! The compatibility scanner records fully-qualified declaration and target
//! identities.  Resolve only exact, unique FQN matches before the generic
//! label resolver runs; a same-named class in another namespace is never
//! evidence for a rewrite.

use graphoxide_core::{normalize_id, Extraction};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub(crate) const PHP_FQN: &str = "php_fqn";
pub(crate) const PHP_SOURCE_FQN: &str = "php_source_fqn";
pub(crate) const PHP_TARGET_FQN: &str = "php_target_fqn";

fn is_php(path: &str) -> bool {
    matches!(
        std::path::Path::new(path)
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str(),
        "php" | "phtml" | "php3" | "php4" | "php5" | "php7" | "phps"
    )
}

pub(crate) fn resolve_types(extractions: &mut [Extraction]) {
    let mut definitions = BTreeMap::<String, Vec<String>>::new();
    for node in extractions.iter().flat_map(|extraction| &extraction.nodes) {
        if node.source_file.is_empty() || !is_php(&node.source_file) {
            continue;
        }
        let Some(fqn) = node.extra.get(PHP_FQN).and_then(Value::as_str) else {
            continue;
        };
        definitions
            .entry(normalize_id(fqn))
            .or_default()
            .push(node.id.clone());
    }
    for ids in definitions.values_mut() {
        ids.sort();
        ids.dedup();
    }

    let unique = definitions
        .into_iter()
        .filter_map(|(fqn, ids)| (ids.len() == 1).then(|| (fqn, ids[0].clone())))
        .collect::<BTreeMap<_, _>>();
    let mut remap = BTreeMap::<String, String>::new();
    for extraction in extractions.iter_mut() {
        for edge in &mut extraction.edges {
            if !is_php(&edge.source_file) {
                continue;
            }
            if let Some(fqn) = edge
                .extra
                .get(PHP_SOURCE_FQN)
                .and_then(Value::as_str)
                .map(str::to_owned)
            {
                let old = edge.true_source().to_owned();
                if let Some(source) = unique.get(&normalize_id(&fqn)) {
                    edge.source = source.clone();
                    edge.extra.insert("_src".into(), source.clone().into());
                    if old != *source {
                        remap.insert(old, source.clone());
                    }
                }
            }
            edge.extra.remove(PHP_SOURCE_FQN);

            let target_fqn = edge
                .extra
                .get(PHP_TARGET_FQN)
                .and_then(Value::as_str)
                .map(str::to_owned);
            if let Some(fqn) = target_fqn {
                let old = edge.true_target().to_owned();
                if let Some(target) = unique.get(&normalize_id(&fqn)) {
                    edge.target = target.clone();
                    edge.extra.insert("_tgt".into(), target.clone().into());
                    if old != *target {
                        remap.insert(old, target.clone());
                    }
                }
            }
            edge.extra.remove(PHP_TARGET_FQN);
        }
    }
    if remap.is_empty() {
        return;
    }

    for extraction in extractions.iter_mut() {
        for edge in &mut extraction.edges {
            if let Some(target) = remap.get(edge.true_target()) {
                edge.target = target.clone();
                edge.extra.insert("_tgt".into(), target.clone().into());
            }
            if let Some(source) = remap.get(edge.true_source()) {
                edge.source = source.clone();
                edge.extra.insert("_src".into(), source.clone().into());
            }
        }
    }
    let referenced = extractions
        .iter()
        .flat_map(|extraction| &extraction.edges)
        .flat_map(|edge| [edge.true_source().to_owned(), edge.true_target().to_owned()])
        .collect::<BTreeSet<_>>();
    for extraction in extractions {
        extraction.nodes.retain(|node| {
            !remap.contains_key(&node.id)
                || !node.source_file.is_empty()
                || referenced.contains(&node.id)
        });
    }
}
