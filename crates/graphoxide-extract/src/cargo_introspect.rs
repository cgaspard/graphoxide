//! Deterministic Cargo workspace introspection.
//!
//! Only packages declared by the workspace become nodes. Dependency entries
//! resolve through that package index, so registry-only dependencies never
//! become graph hubs.

use graphoxide_core::{Confidence, Edge, Extraction, Node};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component, Path, PathBuf},
};

const MAX_MANIFEST_BYTES: u64 = 2 * 1024 * 1024;
const MAX_WORKSPACE_MEMBERS: usize = 10_000;

#[derive(Debug, thiserror::Error)]
pub enum CargoIntrospectionError {
    #[error("could not read Cargo manifest {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("Cargo manifest {path} exceeds the {MAX_MANIFEST_BYTES}-byte limit")]
    TooLarge { path: PathBuf },
    #[error("invalid Cargo manifest {path}: {source}")]
    Toml {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("workspace member pattern escapes the workspace root: {0}")]
    UnsafeMemberPattern(String),
    #[error("workspace expands beyond {MAX_WORKSPACE_MEMBERS} package manifests")]
    TooManyMembers,
    #[error("workspace package name {name:?} is declared by both {first} and {second}")]
    DuplicatePackageName {
        name: String,
        first: String,
        second: String,
    },
}

#[derive(Clone)]
struct CrateManifest {
    id: String,
    source_file: String,
    data: toml::Value,
}

/// Return crate nodes and workspace-internal dependency edges for `root`.
pub fn introspect_cargo(root: &Path) -> Result<Extraction, CargoIntrospectionError> {
    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let root_manifest = root.join("Cargo.toml");
    let root_data = load_manifest(&root_manifest)?;
    let manifests = member_manifests(&root, &root_manifest, &root_data)?;

    let mut crates = BTreeMap::<String, CrateManifest>::new();
    for manifest in manifests {
        let data = if manifest == root_manifest {
            root_data.clone()
        } else {
            load_manifest(&manifest)?
        };
        let Some(name) = package_name(&data) else {
            continue;
        };
        let source_file = portable_relative(&root, &manifest);
        let value = CrateManifest {
            id: format!("crate:{name}"),
            source_file: source_file.clone(),
            data,
        };
        if let Some(existing) = crates.insert(name.clone(), value) {
            return Err(CargoIntrospectionError::DuplicatePackageName {
                name,
                first: existing.source_file,
                second: source_file,
            });
        }
    }

    let nodes = crates
        .iter()
        .map(|(name, krate)| Node {
            id: krate.id.clone(),
            label: name.clone(),
            // Upstream omits this optional raw field; `concept` is the schema
            // default applied by both builders when it is absent.
            file_type: "concept".into(),
            source_file: krate.source_file.clone(),
            source_location: Some("L1".into()),
            community: None,
            extra: BTreeMap::new(),
        })
        .collect();

    let mut edges = Vec::new();
    for krate in crates.values() {
        let Some(dependencies) = krate
            .data
            .get("dependencies")
            .and_then(toml::Value::as_table)
        else {
            continue;
        };
        for (dependency_key, specification) in dependencies {
            let real_name = specification
                .as_table()
                .and_then(|table| table.get("package"))
                .and_then(toml::Value::as_str)
                .filter(|name| !name.is_empty())
                .unwrap_or(dependency_key);
            let Some(target) = crates.get(real_name) else {
                continue;
            };
            if target.id == krate.id {
                continue;
            }
            edges.push(crate_dependency_edge(
                &krate.id,
                &target.id,
                &krate.source_file,
            ));
        }
    }
    edges.sort_by(|left, right| {
        left.true_source()
            .cmp(right.true_source())
            .then_with(|| left.true_target().cmp(right.true_target()))
    });
    edges.dedup_by(|left, right| {
        left.true_source() == right.true_source()
            && left.true_target() == right.true_target()
            && left.relation == right.relation
    });

    Ok(Extraction {
        nodes,
        edges,
        hyperedges: Vec::new(),
    })
}

fn load_manifest(path: &Path) -> Result<toml::Value, CargoIntrospectionError> {
    let metadata = fs::metadata(path).map_err(|source| CargoIntrospectionError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.len() > MAX_MANIFEST_BYTES {
        return Err(CargoIntrospectionError::TooLarge {
            path: path.to_path_buf(),
        });
    }
    let text = fs::read_to_string(path).map_err(|source| CargoIntrospectionError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    toml::from_str(&text).map_err(|source| CargoIntrospectionError::Toml {
        path: path.to_path_buf(),
        source,
    })
}

fn package_name(data: &toml::Value) -> Option<String> {
    data.get("package")?
        .as_table()?
        .get("name")?
        .as_str()
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

fn member_manifests(
    root: &Path,
    root_manifest: &Path,
    root_data: &toml::Value,
) -> Result<Vec<PathBuf>, CargoIntrospectionError> {
    let mut manifests = BTreeSet::new();
    if root_data.get("package").is_some_and(toml::Value::is_table) {
        manifests.insert(root_manifest.to_path_buf());
    }
    let members = root_data
        .get("workspace")
        .and_then(toml::Value::as_table)
        .and_then(|workspace| workspace.get("members"))
        .and_then(toml::Value::as_array);
    let Some(members) = members else {
        return Ok(manifests.into_iter().collect());
    };
    for pattern in members.iter().filter_map(toml::Value::as_str) {
        for member in expand_member_pattern(root, pattern)? {
            let manifest = member.join("Cargo.toml");
            let is_regular_file = fs::symlink_metadata(&manifest)
                .map(|metadata| metadata.file_type().is_file())
                .unwrap_or(false);
            if is_regular_file {
                manifests.insert(manifest);
                if manifests.len() > MAX_WORKSPACE_MEMBERS {
                    return Err(CargoIntrospectionError::TooManyMembers);
                }
            }
        }
    }
    Ok(manifests.into_iter().collect())
}

fn expand_member_pattern(
    root: &Path,
    pattern: &str,
) -> Result<Vec<PathBuf>, CargoIntrospectionError> {
    let path = Path::new(pattern);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(CargoIntrospectionError::UnsafeMemberPattern(
            pattern.to_owned(),
        ));
    }
    let segments = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(segment) => segment.to_str().map(str::to_owned),
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut candidates = vec![root.to_path_buf()];
    for segment in segments {
        let wildcard = segment.contains(['*', '?']);
        let mut next = Vec::new();
        for base in candidates {
            if wildcard {
                if !base.is_dir() {
                    continue;
                }
                let mut entries = fs::read_dir(&base)
                    .map_err(|source| CargoIntrospectionError::Io {
                        path: base.clone(),
                        source,
                    })?
                    .filter_map(Result::ok)
                    .collect::<Vec<_>>();
                entries.sort_by_key(std::fs::DirEntry::file_name);
                for entry in entries {
                    let file_type =
                        entry
                            .file_type()
                            .map_err(|source| CargoIntrospectionError::Io {
                                path: entry.path(),
                                source,
                            })?;
                    let name = entry.file_name();
                    let Some(name) = name.to_str() else { continue };
                    if file_type.is_dir()
                        && !file_type.is_symlink()
                        && wildcard_match(&segment, name)
                    {
                        next.push(entry.path());
                    }
                }
            } else {
                let candidate = base.join(&segment);
                let is_regular_directory = fs::symlink_metadata(&candidate)
                    .map(|metadata| metadata.file_type().is_dir())
                    .unwrap_or(false);
                if is_regular_directory {
                    next.push(candidate);
                }
            }
            if next.len() > MAX_WORKSPACE_MEMBERS {
                return Err(CargoIntrospectionError::TooManyMembers);
            }
        }
        candidates = next;
    }
    candidates.sort();
    candidates.dedup();
    Ok(candidates)
}

fn wildcard_match(pattern: &str, text: &str) -> bool {
    let pattern = pattern.as_bytes();
    let text = text.as_bytes();
    let mut table = vec![vec![false; text.len() + 1]; pattern.len() + 1];
    table[0][0] = true;
    for index in 1..=pattern.len() {
        if pattern[index - 1] == b'*' {
            table[index][0] = table[index - 1][0];
        }
    }
    for p in 1..=pattern.len() {
        for t in 1..=text.len() {
            table[p][t] = match pattern[p - 1] {
                b'*' => table[p - 1][t] || table[p][t - 1],
                b'?' => table[p - 1][t - 1],
                byte => byte == text[t - 1] && table[p - 1][t - 1],
            };
        }
    }
    table[pattern.len()][text.len()]
}

fn portable_relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn crate_dependency_edge(source: &str, target: &str, source_file: &str) -> Edge {
    Edge {
        source: source.into(),
        target: target.into(),
        relation: "crate_depends_on".into(),
        confidence: Confidence::Extracted,
        source_file: source_file.into(),
        extra: BTreeMap::from([
            ("context".into(), "cargo_dependency".into()),
            ("source_location".into(), "L1".into()),
            ("weight".into(), 1.0.into()),
        ]),
    }
}
