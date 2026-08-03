//! Fail-closed graph-layer stale-source detection for incremental builds.

use crate::detect::{is_sensitive, DetectResult};
use serde_json::Value;
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};
use unicode_normalization::UnicodeNormalization;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StaleSourceReport {
    pub stale: Vec<String>,
    pub warnings: Vec<String>,
}

fn nfc(value: impl AsRef<str>) -> String {
    value.as_ref().nfc().collect()
}

fn lexical_absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

fn resolved(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| lexical_absolute(path))
}

fn within(path: &Path, root: &Path) -> bool {
    path.starts_with(root) || resolved(path).starts_with(root)
}

fn evidence_spellings(root: &Path, raw: &str) -> Vec<String> {
    let raw = raw.split(" [").next().unwrap_or(raw).trim();
    let path = Path::new(raw);
    let anchored = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let mut spellings = vec![nfc(raw.replace('\\', "/")), nfc(anchored.to_string_lossy())];
    spellings.push(nfc(resolved(&anchored).to_string_lossy()));
    spellings.sort();
    spellings.dedup();
    spellings
}

/// Find graph `source_file` values that are provably deleted or excluded.
///
/// An alive path that merely disappeared from a scan is retained and warned;
/// this prevents spelling/normalization drift or walk errors from mass-pruning
/// valid graph nodes.
pub fn stale_graph_sources(
    graph_path: &Path,
    scan_root: &Path,
    seen_files: &BTreeSet<PathBuf>,
    detection: Option<&DetectResult>,
) -> StaleSourceReport {
    let data: Value = match fs::read(graph_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
    {
        Some(Value::Object(object)) => Value::Object(object),
        _ => return StaleSourceReport::default(),
    };
    let root = resolved(scan_root);
    let out_base = graph_path
        .parent()
        .and_then(Path::parent)
        .map(resolved)
        .unwrap_or_else(|| root.clone());
    let seen = seen_files
        .iter()
        .flat_map(|path| {
            let anchored = if path.is_absolute() {
                path.clone()
            } else {
                scan_root.join(path)
            };
            [
                nfc(anchored.to_string_lossy()),
                nfc(resolved(&anchored).to_string_lossy()),
            ]
        })
        .collect::<BTreeSet<_>>();
    let seen_basenames = seen_files
        .iter()
        .filter_map(|path| path.file_name())
        .map(|name| nfc(name.to_string_lossy()))
        .collect::<BTreeSet<_>>();

    let mut excluded_exact = BTreeSet::new();
    let mut excluded_prefixes = Vec::new();
    if let Some(detection) = detection {
        for raw in detection.ignored.iter().chain(&detection.pruned_noise_dirs) {
            let directory = raw.ends_with('/') || raw.ends_with(std::path::MAIN_SEPARATOR);
            for spelling in evidence_spellings(scan_root, raw) {
                if directory {
                    excluded_prefixes.push(spelling.trim_end_matches('/').to_owned() + "/");
                } else {
                    excluded_exact.insert(spelling);
                }
            }
        }
        for raw in &detection.skipped_sensitive {
            excluded_exact.extend(evidence_spellings(scan_root, raw));
        }
    }
    let provably_excluded = |path: &Path| {
        if is_sensitive(path) {
            return true;
        }
        let spellings = [
            nfc(path.to_string_lossy()).replace('\\', "/"),
            nfc(resolved(path).to_string_lossy()).replace('\\', "/"),
        ];
        spellings.iter().any(|spelling| {
            excluded_exact.contains(spelling)
                || excluded_prefixes
                    .iter()
                    .any(|prefix| spelling.starts_with(prefix))
        })
    };

    let mut report = StaleSourceReport::default();
    let mut checked = BTreeSet::new();
    let mut kept_alive = Vec::new();
    for node in data
        .get("nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(source) = node.get("source_file").and_then(Value::as_str) else {
            continue;
        };
        if source.contains("://") || !checked.insert(source.to_owned()) {
            continue;
        }
        let path = Path::new(source);
        let candidates = if path.is_absolute() {
            vec![path.to_path_buf()]
        } else {
            let mut candidates = vec![root.join(path)];
            if out_base != root {
                candidates.push(out_base.join(path));
            }
            candidates
        };
        let in_root = candidates
            .into_iter()
            .filter(|candidate| within(candidate, &root))
            .collect::<Vec<_>>();
        if in_root.is_empty() {
            continue;
        }
        let in_scan = in_root.iter().any(|candidate| {
            seen.contains(&nfc(candidate.to_string_lossy()))
                || seen.contains(&nfc(resolved(candidate).to_string_lossy()))
        });
        if in_scan {
            continue;
        }
        let alive = in_root
            .iter()
            .filter(|candidate| candidate.exists())
            .collect::<Vec<_>>();
        if !alive.is_empty() {
            if alive.iter().all(|candidate| provably_excluded(candidate)) {
                report.stale.push(source.to_owned());
            } else {
                kept_alive.push(source.to_owned());
            }
            continue;
        }
        let portable = source.replace('\\', "/");
        if !portable.contains('/')
            && seen_basenames.contains(&nfc(Path::new(source)
                .file_name()
                .unwrap()
                .to_string_lossy()))
        {
            kept_alive.push(source.to_owned());
            continue;
        }
        report.stale.push(source.to_owned());
    }
    if !kept_alive.is_empty() {
        report.warnings.push(format!(
            "fail-closed: kept graph nodes from {} source file(s) that left the scan corpus but still exist on disk",
            kept_alive.len()
        ));
    }
    report
}
