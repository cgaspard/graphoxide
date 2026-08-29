use anyhow::{anyhow, ensure, Context as _};
use graphoxide_core::{KnowledgeGraph, Node, CONTAINER_SOURCE_ATTRIBUTE};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read as _,
    path::{Component, Path, PathBuf},
};

const CATALOG_FILE: &str = "catalog.json";
const MAX_CATALOG_BYTES: u64 = 16 * 1024 * 1024;
const MAX_ID_BYTES: usize = 4_096;
const MAX_CATALOG_ANNOTATION_BYTES: usize = 64 * 1024;
const MAX_RETAINED_CATALOG_ANNOTATION_BYTES: usize = MAX_CATALOG_BYTES as usize;
const MAX_RETAINED_CATALOG_INDEX_BYTES: usize = MAX_CATALOG_BYTES as usize;
const MAX_TOTAL_CATALOG_ANNOTATION_BYTES: usize = 128 * 1024 * 1024;
const MAX_TRANSIENT_CATALOG_SOURCE_RECORD_BYTES: usize = MAX_CATALOG_BYTES as usize;
const CATALOG_INDEX_ENTRY_OVERHEAD_BYTES: usize = 1_024;
const SOURCE_HASH_BUFFER_BYTES: usize = 64 * 1024;
const MAX_PERCENT_DECODE_LAYERS: usize = 8;

/// Validated, deterministic source provenance annotations for project files.
pub struct Catalog {
    version: u64,
    project_root: PathBuf,
    entries_by_source: BTreeMap<String, CatalogAnnotation>,
    citation_keys: BTreeSet<String>,
    scan_exclusion: String,
    inactive_source_paths: BTreeSet<String>,
    restrict_to_active_sources: bool,
}

struct CatalogAnnotation {
    value: Value,
    sha256: String,
    serialized_bytes: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogV1Envelope {
    version: u64,
    entries: Vec<CatalogEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogV2Envelope {
    version: u64,
    sources: Vec<CatalogSource>,
    captures: Vec<CatalogCapture>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CatalogEntry {
    source_id: String,
    capture_id: String,
    source_path: String,
    sha256: String,
    captured_at: String,
    accessed_at: String,
    updated_at: String,
    representation: String,
    source_system: String,
    url: String,
    location: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogSource {
    source_id: String,
    active_capture_id: String,
    source_system: String,
    url: String,
    location: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogCapture {
    capture_id: String,
    source_id: String,
    source_path: String,
    sha256: String,
    captured_at: String,
    accessed_at: String,
    updated_at: String,
    representation: String,
}

#[derive(Deserialize)]
struct CatalogVersionHeader {
    version: u64,
}

struct CatalogSourceRecord {
    active_capture_id: String,
    source_system: String,
    url: String,
    location: String,
    active_seen: bool,
}

#[derive(Default)]
struct CatalogAccumulator {
    citation_keys: BTreeSet<String>,
    entries_by_source: BTreeMap<String, CatalogAnnotation>,
    inactive_source_paths: BTreeSet<String>,
    retained_annotation_bytes: usize,
    retained_index_bytes: usize,
}

impl Catalog {
    /// Worst-case catalog loader and final-node annotation bytes retained beside a graph build.
    pub const fn memory_reservation_bytes() -> usize {
        MAX_CATALOG_BYTES as usize
            + MAX_RETAINED_CATALOG_ANNOTATION_BYTES
            + MAX_RETAINED_CATALOG_INDEX_BYTES
            + MAX_TOTAL_CATALOG_ANNOTATION_BYTES
            + MAX_TRANSIENT_CATALOG_SOURCE_RECORD_BYTES
    }

    pub fn load(project_root: &Path, catalog_dir: &Path) -> anyhow::Result<Self> {
        Self::load_inner(project_root, catalog_dir, true)
    }

    /// Load catalog identities without requiring an on-disk source snapshot.
    pub fn load_metadata(metadata_root: &Path, catalog_dir: &Path) -> anyhow::Result<Self> {
        Self::load_inner(metadata_root, catalog_dir, false)
    }

    fn load_inner(
        root: &Path,
        catalog_dir: &Path,
        bind_active_sources: bool,
    ) -> anyhow::Result<Self> {
        let project_root = fs::canonicalize(root)
            .with_context(|| format!("resolve project root {}", root.display()))?;
        ensure!(project_root.is_dir(), "project root is not a directory");
        let catalog_path = checked_catalog_path(&project_root, catalog_dir)?;
        let bytes = read_catalog_bytes(&project_root, &catalog_path)?;
        let scan_exclusion = format!(
            "/{}",
            catalog_path
                .strip_prefix(&project_root)
                .context("catalog metadata path escaped the project root")?
                .to_str()
                .context("catalog metadata path is not UTF-8")?
                .replace('\\', "/")
        );

        let version = serde_json::from_slice::<CatalogVersionHeader>(&bytes)
            .with_context(|| format!("parse {}", catalog_path.display()))?;
        let mut accumulator = CatalogAccumulator::default();
        match version.version {
            1 => {
                let envelope = serde_json::from_slice::<CatalogV1Envelope>(&bytes)
                    .with_context(|| format!("parse {}", catalog_path.display()))?;
                ensure!(envelope.version == 1, "unsupported catalog version");
                for entry in envelope.entries {
                    accumulator.add_entry(
                        entry,
                        true,
                        bind_active_sources.then_some(&project_root),
                    )?;
                }
            }
            2 => load_v2_catalog(
                &bytes,
                bind_active_sources.then_some(&project_root),
                &mut accumulator,
            )?,
            _ => anyhow::bail!("unsupported catalog version"),
        }
        accumulator
            .inactive_source_paths
            .retain(|path| !accumulator.entries_by_source.contains_key(path));
        if bind_active_sources {
            for source_path in &accumulator.inactive_source_paths {
                validate_existing_inactive_catalog_source_admission(&project_root, source_path)?;
            }
        }

        Ok(Self {
            version: version.version,
            project_root,
            entries_by_source: accumulator.entries_by_source,
            citation_keys: accumulator.citation_keys,
            scan_exclusion,
            inactive_source_paths: accumulator.inactive_source_paths,
            restrict_to_active_sources: false,
        })
    }

    /// Bind one validated Registry v1 origin to the existing catalog contract.
    ///
    /// The registry remains the Git authority; this adapter retains only the
    /// active capture annotations needed by extraction and final rehash checks.
    pub fn from_registry_origin(
        project_root: &Path,
        registry: &crate::registry::RegistrySnapshot,
        origin_id: &str,
    ) -> anyhow::Result<Self> {
        Self::from_registry_origin_inner(project_root, registry, origin_id, true)
    }

    /// Project an already-pinned Registry v1 tree for graph-only operations.
    ///
    /// Canonical wiki rendering must not reopen raw sources. This adapter
    /// retains the identical active-capture annotation contract without
    /// binding or hashing the original source locations.
    pub fn from_registry_origin_metadata(
        metadata_root: &Path,
        registry: &crate::registry::RegistrySnapshot,
        origin_id: &str,
    ) -> anyhow::Result<Self> {
        Self::from_registry_origin_inner(metadata_root, registry, origin_id, false)
    }

    fn from_registry_origin_inner(
        project_root: &Path,
        registry: &crate::registry::RegistrySnapshot,
        origin_id: &str,
        bind_active_sources: bool,
    ) -> anyhow::Result<Self> {
        let project_root = fs::canonicalize(project_root)
            .with_context(|| format!("resolve project root {}", project_root.display()))?;
        ensure!(project_root.is_dir(), "project root is not a directory");
        let origin = registry
            .origins()
            .get(origin_id)
            .context("registry origin does not exist")?;
        let mut accumulator = CatalogAccumulator::default();
        for active in registry
            .active_captures()
            .into_iter()
            .filter(|active| active.source().origin_id == origin_id)
        {
            let source = active.source();
            let capture = active.capture();
            accumulator.add_entry(
                CatalogEntry {
                    source_id: source.source_id.clone(),
                    capture_id: capture.capture_id.clone(),
                    source_path: capture.relative_path.clone(),
                    sha256: capture.sha256.clone(),
                    captured_at: capture.observed_at.clone(),
                    accessed_at: capture.observed_at.clone(),
                    updated_at: capture.observed_at.clone(),
                    representation: capture.representation.clone(),
                    source_system: format!("registry-{}", origin.kind),
                    url: format!("local-registry:{}", source.origin_id),
                    location: capture.relative_path.clone(),
                },
                true,
                bind_active_sources.then_some(&project_root),
            )?;
        }
        ensure!(
            !accumulator.entries_by_source.is_empty(),
            "registry origin has no active captures"
        );
        Ok(Self {
            version: 2,
            project_root,
            entries_by_source: accumulator.entries_by_source,
            citation_keys: accumulator.citation_keys,
            scan_exclusion: String::new(),
            inactive_source_paths: BTreeSet::new(),
            restrict_to_active_sources: true,
        })
    }

    pub fn citation_keys(&self) -> BTreeSet<String> {
        self.citation_keys.clone()
    }

    /// Return the validated active capture annotations keyed by project-relative source path.
    pub fn active_annotations(&self) -> BTreeMap<String, Value> {
        self.entries_by_source
            .iter()
            .map(|(source_path, annotation)| (source_path.clone(), annotation.value.clone()))
            .collect()
    }

    /// Catalog schema version retained for callers choosing snapshot binding behavior.
    pub const fn version(&self) -> u64 {
        self.version
    }

    /// Root-anchored project-relative ignore pattern for this catalog's metadata file.
    pub fn scan_exclusion(&self) -> &str {
        &self.scan_exclusion
    }

    /// Root-anchored project-relative paths that indexing must exclude, including catalog metadata and inactive captures.
    pub fn scan_exclusions(&self) -> impl Iterator<Item = String> + '_ {
        std::iter::once(self.scan_exclusion.as_str())
            .filter(|path| !path.is_empty())
            .map(str::to_owned)
            .chain(
                self.inactive_source_paths
                    .iter()
                    .map(|path| format!("/{path}")),
            )
    }

    /// Active project-relative source paths in deterministic order.
    pub fn active_source_paths(&self) -> impl Iterator<Item = &str> {
        self.entries_by_source.keys().map(String::as_str)
    }

    /// Registry bindings index only active registry sources; legacy catalogs
    /// retain their current additive annotation behavior.
    pub const fn restricts_scan_to_active_sources(&self) -> bool {
        self.restrict_to_active_sources
    }

    /// Reject annotations that would exceed the bounded graph payload.
    fn preflight_apply_to_nodes(&self, nodes: &[Node]) -> anyhow::Result<()> {
        self.annotation_bytes_for_nodes(nodes).map(|_| ())
    }

    /// Return the exact serialized bytes that annotating these nodes retains.
    pub fn annotation_bytes_for_nodes(&self, nodes: &[Node]) -> anyhow::Result<usize> {
        let mut total_annotation_bytes = 0usize;
        for node in nodes {
            let Some(annotation) = self.annotation_for_node(node) else {
                continue;
            };
            total_annotation_bytes = total_annotation_bytes
                .checked_add(annotation.serialized_bytes)
                .context("catalog annotation payload size overflow")?;
            ensure!(
                total_annotation_bytes <= MAX_TOTAL_CATALOG_ANNOTATION_BYTES,
                "catalog annotations exceed the {MAX_TOTAL_CATALOG_ANNOTATION_BYTES}-byte limit"
            );
        }
        Ok(total_annotation_bytes)
    }

    /// Apply metadata after the caller has prepared the graph; verify_sources
    /// remains the single integrity gate immediately before publication.
    pub fn apply_to_nodes(&self, nodes: &mut [Node]) -> anyhow::Result<()> {
        self.preflight_apply_to_nodes(nodes)?;
        for node in nodes {
            node.extra.remove("catalog");
            if let Some(annotation) = self.annotation_for_node(node) {
                node.extra
                    .insert("catalog".into(), annotation.value.clone());
            }
        }
        Ok(())
    }

    /// Revalidate every admitted source immediately before publication.
    pub fn verify_sources(&self) -> anyhow::Result<()> {
        for (source_path, annotation) in &self.entries_by_source {
            verify_catalog_source(&self.project_root, source_path, &annotation.sha256)?;
        }
        Ok(())
    }

    /// Ensure graph annotations are exactly the active catalog records for their source paths.
    pub fn validate_graph_annotations(&self, graph: &KnowledgeGraph) -> anyhow::Result<()> {
        for node in &graph.nodes {
            let expected = self.annotation_for_node(node);
            let actual = node.extra.get("catalog");
            ensure!(
                actual == expected.map(|annotation| &annotation.value),
                "catalog graph annotation does not match the active capture"
            );
        }
        Ok(())
    }

    fn annotation_for_node(&self, node: &Node) -> Option<&CatalogAnnotation> {
        let source_path = node
            .extra
            .get(CONTAINER_SOURCE_ATTRIBUTE)
            .and_then(Value::as_str)
            .filter(|source| !source.is_empty())
            .unwrap_or(&node.source_file);
        self.entries_by_source.get(source_path)
    }
}

impl CatalogAccumulator {
    fn add_entry(
        &mut self,
        entry: CatalogEntry,
        active: bool,
        project_root: Option<&Path>,
    ) -> anyhow::Result<()> {
        validate_entry(&entry, active.then_some(project_root).flatten())?;
        let citation_key = format!("{}#{}", entry.source_id, entry.capture_id);
        let source_path = entry.source_path.clone();
        self.retained_index_bytes = self
            .retained_index_bytes
            .checked_add(catalog_index_entry_bytes(
                &citation_key,
                &source_path,
                &entry.sha256,
            ))
            .context("catalog retained index size overflow")?;
        ensure!(
            self.retained_index_bytes <= MAX_RETAINED_CATALOG_INDEX_BYTES,
            "catalog retained index metadata exceeds the {MAX_RETAINED_CATALOG_INDEX_BYTES}-byte limit"
        );
        ensure!(
            self.citation_keys.insert(citation_key),
            "duplicate catalog citation key"
        );
        if !active {
            self.inactive_source_paths.insert(source_path);
            return Ok(());
        }
        let sha256 = entry.sha256.clone();
        let annotation = serde_json::to_value(entry).context("serialize catalog annotation")?;
        let serialized_bytes = serde_json::to_vec(&annotation)
            .context("measure catalog annotation")?
            .len();
        ensure!(
            serialized_bytes <= MAX_CATALOG_ANNOTATION_BYTES,
            "catalog annotation exceeds the {MAX_CATALOG_ANNOTATION_BYTES}-byte limit"
        );
        self.retained_annotation_bytes = self
            .retained_annotation_bytes
            .checked_add(serialized_bytes)
            .context("catalog retained annotation size overflow")?;
        ensure!(
            self.retained_annotation_bytes <= MAX_RETAINED_CATALOG_ANNOTATION_BYTES,
            "catalog retained annotations exceed the {MAX_RETAINED_CATALOG_ANNOTATION_BYTES}-byte limit"
        );
        ensure!(
            self.entries_by_source
                .insert(
                    source_path,
                    CatalogAnnotation {
                        value: annotation,
                        sha256,
                        serialized_bytes,
                    },
                )
                .is_none(),
            "duplicate catalog source_path"
        );
        Ok(())
    }
}

fn load_v2_catalog(
    bytes: &[u8],
    project_root: Option<&Path>,
    accumulator: &mut CatalogAccumulator,
) -> anyhow::Result<()> {
    let envelope: CatalogV2Envelope = serde_json::from_slice(bytes).context("parse v2 catalog")?;
    ensure!(envelope.version == 2, "unsupported catalog version");

    let mut sources = BTreeMap::new();
    let mut source_record_bytes = 0usize;
    for source in envelope.sources {
        validate_source(&source)?;
        source_record_bytes = source_record_bytes
            .checked_add(catalog_source_record_bytes(&source))
            .context("catalog source record size overflow")?;
        ensure!(
            source_record_bytes <= MAX_TRANSIENT_CATALOG_SOURCE_RECORD_BYTES,
            "catalog source records exceed the {MAX_TRANSIENT_CATALOG_SOURCE_RECORD_BYTES}-byte limit"
        );
        ensure!(
            sources
                .insert(
                    source.source_id.clone(),
                    CatalogSourceRecord {
                        active_capture_id: source.active_capture_id,
                        source_system: source.source_system,
                        url: source.url,
                        location: source.location,
                        active_seen: false,
                    },
                )
                .is_none(),
            "duplicate catalog source_id"
        );
    }
    for capture in envelope.captures {
        validate_capture(&capture)?;
        let source = sources
            .get_mut(&capture.source_id)
            .context("catalog capture references an unknown source")?;
        let active = source.active_capture_id == capture.capture_id;
        ensure!(
            !active || !source.active_seen,
            "duplicate catalog source/capture identity"
        );
        source.active_seen |= active;
        accumulator.add_entry(
            CatalogEntry {
                source_id: capture.source_id,
                capture_id: capture.capture_id,
                source_path: capture.source_path,
                sha256: capture.sha256,
                captured_at: capture.captured_at,
                accessed_at: capture.accessed_at,
                updated_at: capture.updated_at,
                representation: capture.representation,
                source_system: source.source_system.clone(),
                url: source.url.clone(),
                location: source.location.clone(),
            },
            active,
            project_root,
        )?;
    }
    ensure!(
        sources.values().all(|source| source.active_seen),
        "catalog source references a missing active capture"
    );
    for inactive_path in &accumulator.inactive_source_paths {
        let active_prefix = format!("{inactive_path}/");
        ensure!(
            !accumulator
                .entries_by_source
                .range(active_prefix.clone()..)
                .next()
                .is_some_and(|(active_path, _)| active_path.starts_with(&active_prefix)),
            "inactive catalog source_path must not be an ancestor of an active source_path"
        );
    }
    Ok(())
}

fn catalog_source_record_bytes(source: &CatalogSource) -> usize {
    source
        .source_id
        .len()
        .saturating_add(source.active_capture_id.len())
        .saturating_add(source.source_system.len())
        .saturating_add(source.url.len())
        .saturating_add(source.location.len())
        .saturating_add(CATALOG_INDEX_ENTRY_OVERHEAD_BYTES)
}

/// Charge ownership outside the serialized annotation payload: both B-tree
/// indexes, their string keys, the duplicated digest, and map/object storage.
fn catalog_index_entry_bytes(citation_key: &str, source_path: &str, sha256: &str) -> usize {
    citation_key
        .len()
        .saturating_add(source_path.len())
        .saturating_add(sha256.len())
        .saturating_add(CATALOG_INDEX_ENTRY_OVERHEAD_BYTES)
}

fn read_catalog_bytes(project_root: &Path, catalog_path: &Path) -> anyhow::Result<Vec<u8>> {
    let unsafe_input = || anyhow!("catalog input changed or is unsafe");
    let mut file =
        crate::detect::open_control_file_nofollow(catalog_path).map_err(|_| unsafe_input())?;
    let opened_identity = graphoxide_index_runtime::validate_opened_regular_single_link(&file)
        .map_err(|_| unsafe_input())?
        .ok_or_else(unsafe_input)?;
    ensure!(
        opened_identity.length_bytes() <= MAX_CATALOG_BYTES,
        "catalog exceeds the {MAX_CATALOG_BYTES}-byte limit"
    );

    let mut bytes = Vec::with_capacity(opened_identity.length_bytes() as usize);
    file.by_ref()
        .take(MAX_CATALOG_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("read catalog input")?;
    ensure!(
        bytes.len() as u64 <= MAX_CATALOG_BYTES,
        "catalog exceeds the {MAX_CATALOG_BYTES}-byte limit"
    );

    let after_identity = graphoxide_index_runtime::validate_opened_regular_single_link(&file)
        .map_err(|_| unsafe_input())?
        .ok_or_else(unsafe_input)?;
    let current =
        crate::detect::open_control_file_nofollow(catalog_path).map_err(|_| unsafe_input())?;
    let current_identity = graphoxide_index_runtime::validate_opened_regular_single_link(&current)
        .map_err(|_| unsafe_input())?
        .ok_or_else(unsafe_input)?;
    ensure!(
        opened_identity == after_identity
            && opened_identity == current_identity
            && opened_identity.length_bytes() == bytes.len() as u64
            && fs::canonicalize(project_root).ok().as_deref() == Some(project_root)
            && fs::canonicalize(catalog_path).ok().as_deref() == Some(catalog_path),
        "catalog input changed or is unsafe"
    );
    Ok(bytes)
}

fn checked_catalog_path(project_root: &Path, catalog_dir: &Path) -> anyhow::Result<PathBuf> {
    ensure!(
        !catalog_dir
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir)),
        "catalog directory must not contain . or .. components"
    );
    let lexical_dir = if catalog_dir.is_absolute() {
        catalog_dir.to_path_buf()
    } else {
        project_root.join(catalog_dir)
    };
    let metadata = fs::symlink_metadata(&lexical_dir)
        .with_context(|| format!("inspect catalog directory {}", lexical_dir.display()))?;
    ensure!(
        metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
        "catalog input must be a regular non-symlinked directory"
    );
    let resolved_dir = fs::canonicalize(&lexical_dir)
        .with_context(|| format!("resolve catalog directory {}", lexical_dir.display()))?;
    ensure!(
        resolved_dir == lexical_dir && resolved_dir.starts_with(project_root),
        "catalog directory must resolve without symlinks inside the project root"
    );

    let mut names = fs::read_dir(&resolved_dir)
        .with_context(|| format!("read catalog directory {}", resolved_dir.display()))?;
    let first = names
        .next()
        .transpose()?
        .context("catalog directory is empty")?;
    ensure!(
        first.file_name() == CATALOG_FILE && names.next().is_none(),
        "catalog directory must contain only {CATALOG_FILE}"
    );

    let catalog_path = resolved_dir.join(CATALOG_FILE);
    ensure!(
        fs::canonicalize(&catalog_path).ok().as_deref() == Some(catalog_path.as_path()),
        "catalog input must not resolve through a symlink"
    );
    Ok(catalog_path)
}

fn validate_entry(entry: &CatalogEntry, project_root: Option<&Path>) -> anyhow::Result<()> {
    ensure!(
        catalog_entry_metadata_bytes(entry)? <= MAX_CATALOG_ANNOTATION_BYTES,
        "catalog entry metadata exceeds the {MAX_CATALOG_ANNOTATION_BYTES}-byte limit"
    );
    for (name, value) in [
        ("source_id", entry.source_id.as_str()),
        ("capture_id", entry.capture_id.as_str()),
        ("source_path", entry.source_path.as_str()),
        ("sha256", entry.sha256.as_str()),
        ("captured_at", entry.captured_at.as_str()),
        ("accessed_at", entry.accessed_at.as_str()),
        ("updated_at", entry.updated_at.as_str()),
        ("representation", entry.representation.as_str()),
        ("source_system", entry.source_system.as_str()),
        ("url", entry.url.as_str()),
        ("location", entry.location.as_str()),
    ] {
        validate_catalog_metadata_value(name, value)?;
    }
    ensure!(
        wiki_id_is_valid(&entry.source_id) && wiki_id_is_valid(&entry.capture_id),
        "catalog IDs must be bounded wiki-reference identifiers"
    );
    ensure!(
        entry.sha256.len() == 64
            && entry
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')),
        "catalog sha256 must be 64 lowercase hexadecimal digits"
    );
    for (name, value) in [
        ("captured_at", entry.captured_at.as_str()),
        ("accessed_at", entry.accessed_at.as_str()),
        ("updated_at", entry.updated_at.as_str()),
    ] {
        ensure!(rfc3339_is_valid(value), "catalog {name} must be RFC3339");
    }

    let normalized = crate::project_path::normalize_project_path(&entry.source_path);
    ensure!(
        normalized.as_deref() == Some(entry.source_path.as_str()) && !entry.source_path.is_empty(),
        "catalog source_path must be normalized and project-relative"
    );
    if let Some(project_root) = project_root {
        validate_catalog_source_admission(project_root, &entry.source_path)?;
    }

    validate_catalog_reference(&entry.url)?;
    Ok(())
}

fn validate_source(source: &CatalogSource) -> anyhow::Result<()> {
    for (name, value) in [
        ("source_id", source.source_id.as_str()),
        ("active_capture_id", source.active_capture_id.as_str()),
        ("source_system", source.source_system.as_str()),
        ("url", source.url.as_str()),
        ("location", source.location.as_str()),
    ] {
        validate_catalog_metadata_value(name, value)?;
    }
    ensure!(
        wiki_id_is_valid(&source.source_id) && wiki_id_is_valid(&source.active_capture_id),
        "catalog IDs must be bounded wiki-reference identifiers"
    );
    validate_catalog_reference(&source.url)?;
    Ok(())
}

fn validate_catalog_reference(value: &str) -> anyhow::Result<()> {
    if let Ok(url) = reqwest::Url::parse(value) {
        let credential_free = value
            .split_once("://")
            .and_then(|(_, suffix)| suffix.split(['/', '?', '#']).next())
            .is_some_and(|authority| !authority.contains('@'));
        if matches!(url.scheme(), "http" | "https") {
            ensure!(
                url.host_str().is_some()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none()
                    && credential_free,
                "catalog HTTP(S) URL must be absolute, credential-free, and without query or fragment"
            );
            return Ok(());
        }
        if matches!(url.scheme(), "mcp" | "glean" | "gitlab" | "file") {
            ensure!(
                url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none()
                    && credential_free,
                "catalog source locator must be credential-free and without query or fragment"
            );
            return Ok(());
        }
    }

    let opaque_locator = value.split_once(':').is_some_and(|(scheme, rest)| {
        scheme.starts_with("local-")
            && !rest.is_empty()
            && scheme
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    });
    ensure!(
        Path::new(value).is_absolute() || opaque_locator,
        "catalog URL must be a safe HTTP(S) URL, supported source locator, or absolute local path"
    );
    ensure!(
        !value.chars().any(char::is_control),
        "catalog source locator must not contain control characters"
    );
    Ok(())
}

fn validate_capture(capture: &CatalogCapture) -> anyhow::Result<()> {
    for (name, value) in [
        ("capture_id", capture.capture_id.as_str()),
        ("source_id", capture.source_id.as_str()),
        ("source_path", capture.source_path.as_str()),
        ("sha256", capture.sha256.as_str()),
        ("captured_at", capture.captured_at.as_str()),
        ("accessed_at", capture.accessed_at.as_str()),
        ("updated_at", capture.updated_at.as_str()),
        ("representation", capture.representation.as_str()),
    ] {
        validate_catalog_metadata_value(name, value)?;
    }
    ensure!(
        wiki_id_is_valid(&capture.source_id) && wiki_id_is_valid(&capture.capture_id),
        "catalog IDs must be bounded wiki-reference identifiers"
    );
    ensure!(
        capture.sha256.len() == 64
            && capture
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')),
        "catalog sha256 must be 64 lowercase hexadecimal digits"
    );
    for (name, value) in [
        ("captured_at", capture.captured_at.as_str()),
        ("accessed_at", capture.accessed_at.as_str()),
        ("updated_at", capture.updated_at.as_str()),
    ] {
        ensure!(rfc3339_is_valid(value), "catalog {name} must be RFC3339");
    }
    ensure!(
        crate::project_path::normalize_project_path(&capture.source_path).as_deref()
            == Some(capture.source_path.as_str())
            && !capture.source_path.is_empty(),
        "catalog source_path must be normalized and project-relative"
    );
    Ok(())
}

fn catalog_entry_metadata_bytes(entry: &CatalogEntry) -> anyhow::Result<usize> {
    [
        entry.source_id.as_str(),
        entry.capture_id.as_str(),
        entry.source_path.as_str(),
        entry.sha256.as_str(),
        entry.captured_at.as_str(),
        entry.accessed_at.as_str(),
        entry.updated_at.as_str(),
        entry.representation.as_str(),
        entry.source_system.as_str(),
        entry.url.as_str(),
        entry.location.as_str(),
    ]
    .into_iter()
    .try_fold(0usize, |total, value| {
        total
            .checked_add(value.len())
            .context("catalog entry metadata size overflow")
    })
}

fn validate_catalog_metadata_value(name: &str, value: &str) -> anyhow::Result<()> {
    ensure!(!value.is_empty(), "catalog {name} must not be empty");
    ensure!(
        !value.chars().any(char::is_control),
        "catalog {name} must not contain control characters"
    );
    ensure!(
        !catalog_metadata_is_sensitive(value),
        "catalog {name} contains unsafe credential-shaped metadata"
    );
    Ok(())
}

fn catalog_metadata_is_sensitive(value: &str) -> bool {
    if crate::structured::structured_string_is_sensitive(value)
        || has_signed_url_parameter(value.as_bytes())
    {
        return true;
    }
    if !value.as_bytes().contains(&b'%') {
        return false;
    }
    let mut decoded = value.as_bytes().to_vec();
    for _ in 0..MAX_PERCENT_DECODE_LAYERS {
        decoded = match percent_decode(&decoded) {
            Some(decoded) => decoded,
            None => return true,
        };
        let decoded = match std::str::from_utf8(&decoded) {
            Ok(decoded) => decoded,
            Err(_) => return true,
        };
        if has_signed_url_parameter(decoded.as_bytes())
            || crate::structured::structured_string_is_sensitive(decoded)
        {
            return true;
        }
        if !decoded.as_bytes().contains(&b'%') {
            return false;
        }
    }
    true
}

fn has_signed_url_parameter(value: &[u8]) -> bool {
    value
        .split(|byte| matches!(byte, b'?' | b'&' | b';' | b'\n' | b'\r'))
        .any(|component| {
            let Some(delimiter) = component.iter().position(|byte| *byte == b'=') else {
                return false;
            };
            let (key, value) = component.split_at(delimiter);
            let value = &value[1..];
            !value.iter().all(u8::is_ascii_whitespace)
                && matches!(
                    trim_ascii(key).to_ascii_lowercase().as_slice(),
                    b"sig" | b"signature" | b"x-amz-signature" | b"x-goog-signature"
                )
        })
}

fn percent_decode(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            decoded.push(bytes[index]);
            index += 1;
            continue;
        }
        let high = hex_value(*bytes.get(index + 1)?)?;
        let low = hex_value(*bytes.get(index + 2)?)?;
        decoded.push(high << 4 | low);
        index += 3;
    }
    Some(decoded)
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn trim_ascii(value: &[u8]) -> &[u8] {
    let start = value
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(value.len());
    let end = value
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(start, |index| index + 1);
    &value[start..end]
}

fn verify_catalog_source(
    project_root: &Path,
    source_path: &str,
    expected_sha256: &str,
) -> anyhow::Result<()> {
    let (source_path, resolved_source) = catalog_source_paths(project_root, source_path)?;
    verify_source_sha256(
        project_root,
        &source_path,
        &resolved_source,
        expected_sha256,
    )
}

fn validate_catalog_source_admission(project_root: &Path, source_path: &str) -> anyhow::Result<()> {
    let (source_path, _) = catalog_source_paths(project_root, source_path)?;
    let unsafe_input = || anyhow!("catalog source changed or is unsafe");
    let file =
        crate::detect::open_control_file_nofollow(&source_path).map_err(|_| unsafe_input())?;
    graphoxide_index_runtime::validate_opened_regular_single_link(&file)
        .map_err(|_| unsafe_input())?
        .ok_or_else(unsafe_input)?;
    Ok(())
}

fn validate_existing_inactive_catalog_source_admission(
    project_root: &Path,
    source_path: &str,
) -> anyhow::Result<()> {
    let candidate = project_root.join(source_path);
    match fs::symlink_metadata(&candidate) {
        Ok(_) => validate_catalog_source_admission(project_root, source_path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("inspect inactive catalog source_path {source_path}")),
    }
}

fn catalog_source_paths(
    project_root: &Path,
    source_path: &str,
) -> anyhow::Result<(PathBuf, PathBuf)> {
    let source_file = project_root.join(source_path);
    let resolved_source = fs::canonicalize(&source_file)
        .with_context(|| format!("resolve catalog source_path {source_path}"))?;
    ensure!(
        resolved_source.starts_with(project_root) && resolved_source.is_file(),
        "catalog source_path must resolve to a file inside the project root"
    );
    Ok((source_file, resolved_source))
}

fn verify_source_sha256(
    project_root: &Path,
    source_path: &Path,
    resolved_source: &Path,
    expected_sha256: &str,
) -> anyhow::Result<()> {
    let unsafe_input = || anyhow!("catalog source changed or is unsafe");
    let mut file =
        crate::detect::open_control_file_nofollow(source_path).map_err(|_| unsafe_input())?;
    let opened_identity = graphoxide_index_runtime::validate_opened_regular_single_link(&file)
        .map_err(|_| unsafe_input())?
        .ok_or_else(unsafe_input)?;

    let mut digest = Sha256::new();
    let mut byte_length = 0_u64;
    let mut buffer = [0_u8; SOURCE_HASH_BUFFER_BYTES];
    loop {
        let read = file.read(&mut buffer).context("read catalog source")?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
        byte_length = byte_length
            .checked_add(u64::try_from(read).unwrap_or(u64::MAX))
            .context("catalog source size overflow")?;
    }

    let after_identity = graphoxide_index_runtime::validate_opened_regular_single_link(&file)
        .map_err(|_| unsafe_input())?
        .ok_or_else(unsafe_input)?;
    let current =
        crate::detect::open_control_file_nofollow(source_path).map_err(|_| unsafe_input())?;
    let current_identity = graphoxide_index_runtime::validate_opened_regular_single_link(&current)
        .map_err(|_| unsafe_input())?
        .ok_or_else(unsafe_input)?;
    ensure!(
        opened_identity == after_identity
            && opened_identity == current_identity
            && opened_identity.length_bytes() == byte_length
            && fs::canonicalize(project_root).ok().as_deref() == Some(project_root)
            && fs::canonicalize(source_path).ok().as_deref() == Some(resolved_source),
        "catalog source changed or is unsafe"
    );
    ensure!(
        hex::encode(digest.finalize()) == expected_sha256,
        "catalog sha256 does not match source_path"
    );
    Ok(())
}

fn wiki_id_is_valid(value: &str) -> bool {
    value.len() <= MAX_ID_BYTES
        && value.as_bytes().split_first().is_some_and(|(first, rest)| {
            first.is_ascii_alphanumeric()
                && rest
                    .iter()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        })
}

fn rfc3339_is_valid(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() < 20
        || bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || !matches!(bytes.get(10), Some(b'T' | b't'))
        || bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
    {
        return false;
    }
    let Some(year) = decimal(&bytes[0..4]) else {
        return false;
    };
    let Some(month) = decimal(&bytes[5..7]) else {
        return false;
    };
    let Some(day) = decimal(&bytes[8..10]) else {
        return false;
    };
    let Some(hour) = decimal(&bytes[11..13]) else {
        return false;
    };
    let Some(minute) = decimal(&bytes[14..16]) else {
        return false;
    };
    let Some(second) = decimal(&bytes[17..19]) else {
        return false;
    };
    if year == 0
        || !(1..=12).contains(&month)
        || day == 0
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return false;
    }

    let mut zone = 19;
    if bytes.get(zone) == Some(&b'.') {
        zone += 1;
        let start = zone;
        while bytes.get(zone).is_some_and(u8::is_ascii_digit) {
            zone += 1;
        }
        if zone == start {
            return false;
        }
    }
    match bytes.get(zone..) {
        Some([b'Z' | b'z']) => true,
        Some([b'+' | b'-', h1, h2, b':', m1, m2]) => {
            decimal(&[*h1, *h2]).is_some_and(|hours| hours <= 23)
                && decimal(&[*m1, *m2]).is_some_and(|minutes| minutes <= 59)
        }
        _ => false,
    }
}

fn decimal(bytes: &[u8]) -> Option<u32> {
    bytes.iter().try_fold(0, |value, byte| {
        byte.is_ascii_digit()
            .then(|| value * 10 + u32::from(byte - b'0'))
    })
}

fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        2 if year.is_multiple_of(400) || (year.is_multiple_of(4) && !year.is_multiple_of(100)) => {
            29
        }
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

#[cfg(test)]
mod tests {
    use super::{Catalog, MAX_CATALOG_ANNOTATION_BYTES, MAX_PERCENT_DECODE_LAYERS};
    use crate::detect::{detect, DetectOptions};
    use crate::registry::{
        add_origin, append_capture_and_activate, initialize_tree, RegistryCapture, RegistryOrigin,
        RegistrySnapshot,
    };
    use graphoxide_core::{KnowledgeGraph, Node, CONTAINER_SOURCE_ATTRIBUTE};
    use serde_json::{json, Value};
    use sha2::{Digest as _, Sha256};
    use std::{collections::BTreeMap, fs, path::Path};
    use tempfile::TempDir;

    fn sha256(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    fn entry(source_path: &str) -> Value {
        json!({
            "source_id": "source-one",
            "capture_id": "capture-one",
            "source_path": source_path,
            "sha256": sha256(b"source bytes"),
            "captured_at": "2026-08-24T12:34:56Z",
            "accessed_at": "2026-08-24T12:35:56+00:00",
            "updated_at": "2026-08-23T01:02:03.456-06:00",
            "representation": "markdown",
            "source_system": "confluence",
            "url": "https://docs.example.test/wiki/page",
            "location": "SPACE/Page"
        })
    }

    fn entry_for_source(source_path: &str, source: &[u8]) -> Value {
        let mut record = entry(source_path);
        record["sha256"] = json!(sha256(source));
        record
    }

    fn write_catalog(root: &Path, entries: Value) {
        let directory = root.join("catalog");
        write_catalog_at(&directory, entries);
    }

    fn write_catalog_at(directory: &Path, entries: Value) {
        fs::create_dir_all(directory).expect("create catalog directory");
        fs::write(
            directory.join("catalog.json"),
            serde_json::to_vec(&json!({"version": 1, "entries": entries}))
                .expect("serialize catalog"),
        )
        .expect("write catalog");
    }

    fn write_catalog_v2_at(directory: &Path, sources: Value, captures: Value) {
        fs::create_dir_all(directory).expect("create catalog directory");
        fs::write(
            directory.join("catalog.json"),
            serde_json::to_vec(&json!({
                "version": 2,
                "sources": sources,
                "captures": captures,
            }))
            .expect("serialize catalog"),
        )
        .expect("write catalog");
    }

    fn v2_source(source_id: &str, active_capture_id: &str) -> Value {
        json!({
            "source_id": source_id,
            "active_capture_id": active_capture_id,
            "source_system": "confluence",
            "url": "https://docs.example.test/wiki/page",
            "location": "SPACE/Page",
        })
    }

    fn v2_capture(source_id: &str, capture_id: &str, source_path: &str, source: &[u8]) -> Value {
        json!({
            "capture_id": capture_id,
            "source_id": source_id,
            "source_path": source_path,
            "sha256": sha256(source),
            "captured_at": "2026-08-24T12:34:56Z",
            "accessed_at": "2026-08-24T12:35:56+00:00",
            "updated_at": "2026-08-23T01:02:03.456-06:00",
            "representation": "markdown",
        })
    }

    fn project_with_source(source_path: &str) -> TempDir {
        let project = TempDir::new().expect("project tempdir");
        let path = project.path().join(source_path);
        fs::create_dir_all(path.parent().expect("source parent")).expect("create source parent");
        fs::write(path, "source bytes").expect("write source");
        project
    }

    fn node(source_file: &str, extra: BTreeMap<String, Value>) -> Node {
        Node {
            id: format!("node:{source_file}"),
            label: source_file.into(),
            file_type: "document".into(),
            source_file: source_file.into(),
            source_location: None,
            community: None,
            extra,
        }
    }

    #[test]
    fn normalizes_active_v2_capture_into_the_v1_annotation_shape() {
        let project = project_with_source("docs/page.md");
        write_catalog_v2_at(
            &project.path().join("catalog"),
            json!([v2_source("source-one", "capture-one")]),
            json!([v2_capture(
                "source-one",
                "capture-one",
                "docs/page.md",
                b"source bytes"
            )]),
        );

        let catalog = Catalog::load(project.path(), Path::new("catalog")).expect("load v2");
        let mut nodes = vec![node("docs/page.md", BTreeMap::new())];
        catalog
            .apply_to_nodes(&mut nodes)
            .expect("apply v2 annotation");

        assert_eq!(nodes[0].extra["catalog"], entry("docs/page.md"));
    }

    #[test]
    fn exposes_sorted_clones_of_active_catalog_annotations() {
        let project = project_with_source("docs/one.md");
        fs::write(project.path().join("docs/two.md"), "source bytes").expect("write source");
        write_catalog_v2_at(
            &project.path().join("catalog"),
            json!([
                v2_source("source-two", "capture-two"),
                v2_source("source-one", "capture-one"),
            ]),
            json!([
                v2_capture("source-two", "capture-two", "docs/two.md", b"source bytes"),
                v2_capture("source-one", "capture-one", "docs/one.md", b"source bytes"),
            ]),
        );

        let catalog = Catalog::load(project.path(), Path::new("catalog")).expect("load v2");
        let annotations = catalog.active_annotations();

        assert_eq!(
            annotations.keys().collect::<Vec<_>>(),
            vec!["docs/one.md", "docs/two.md"]
        );
        assert_eq!(annotations["docs/one.md"]["source_id"], "source-one");
        assert_eq!(annotations["docs/two.md"]["capture_id"], "capture-two");
    }

    #[test]
    fn reports_the_loaded_catalog_version() {
        let v1_project = project_with_source("docs/page.md");
        write_catalog(v1_project.path(), json!([entry("docs/page.md")]));
        assert_eq!(
            Catalog::load(v1_project.path(), Path::new("catalog"))
                .expect("load v1")
                .version(),
            1
        );

        let v2_project = TempDir::new().expect("project tempdir");
        write_catalog_v2_at(
            &v2_project.path().join("catalog"),
            json!([v2_source("source-one", "capture-one")]),
            json!([v2_capture(
                "source-one",
                "capture-one",
                "docs/page.md",
                b"source bytes"
            )]),
        );
        assert_eq!(
            Catalog::load_metadata(v2_project.path(), Path::new("catalog"))
                .expect("load v2")
                .version(),
            2
        );
    }

    #[test]
    fn leaves_inactive_historical_v2_captures_out_of_the_snapshot() {
        let project = project_with_source("docs/current.md");
        write_catalog_v2_at(
            &project.path().join("catalog"),
            json!([v2_source("source-one", "capture-current")]),
            json!([
                v2_capture(
                    "source-one",
                    "capture-current",
                    "docs/current.md",
                    b"source bytes"
                ),
                v2_capture(
                    "source-one",
                    "capture-history",
                    "docs/missing-history.md",
                    b"historical bytes"
                ),
            ]),
        );

        let catalog = Catalog::load(project.path(), Path::new("catalog")).expect("load v2");
        let mut nodes = vec![node("docs/missing-history.md", BTreeMap::new())];
        catalog
            .apply_to_nodes(&mut nodes)
            .expect("apply active annotations only");
        catalog
            .verify_sources()
            .expect("rehash active capture only");

        assert!(catalog
            .citation_keys()
            .contains("source-one#capture-history"));
        assert!(!nodes[0].extra.contains_key("catalog"));
    }

    #[test]
    fn rejects_an_inactive_v2_capture_that_is_an_ancestor_of_an_active_path() {
        let project = TempDir::new().expect("project tempdir");
        write_catalog_v2_at(
            &project.path().join("catalog"),
            json!([v2_source("source-one", "capture-active")]),
            json!([
                v2_capture(
                    "source-one",
                    "capture-active",
                    "raw/active.md",
                    b"active source bytes"
                ),
                v2_capture(
                    "source-one",
                    "capture-history",
                    "raw",
                    b"historical source bytes"
                ),
            ]),
        );

        assert!(Catalog::load_metadata(project.path(), Path::new("catalog")).is_err());
    }

    #[test]
    fn rejects_an_existing_inactive_v2_capture_path_that_is_not_a_regular_file() {
        let project = project_with_source("active.md");
        fs::create_dir_all(project.path().join("historical")).expect("historical directory");
        write_catalog_v2_at(
            &project.path().join("catalog"),
            json!([v2_source("source-one", "capture-active")]),
            json!([
                v2_capture("source-one", "capture-active", "active.md", b"source bytes"),
                v2_capture(
                    "source-one",
                    "capture-history",
                    "historical",
                    b"historical source bytes"
                ),
            ]),
        );

        assert!(Catalog::load(project.path(), Path::new("catalog")).is_err());
    }

    #[test]
    fn exposes_inactive_v2_capture_paths_for_index_exclusion() {
        let project = project_with_source("docs/current.md");
        fs::write(project.path().join("docs/history.md"), "historical bytes").unwrap();
        write_catalog_v2_at(
            &project.path().join("catalog"),
            json!([v2_source("source-one", "capture-current")]),
            json!([
                v2_capture(
                    "source-one",
                    "capture-current",
                    "docs/current.md",
                    b"source bytes"
                ),
                v2_capture(
                    "source-one",
                    "capture-history",
                    "docs/history.md",
                    b"historical bytes"
                ),
            ]),
        );

        let catalog = Catalog::load(project.path(), Path::new("catalog")).expect("load v2");

        assert_eq!(
            catalog.scan_exclusions().collect::<Vec<_>>(),
            ["/catalog/catalog.json", "/docs/history.md",].to_vec()
        );
    }

    #[test]
    fn validates_missing_unexpected_and_container_catalog_annotations() {
        let project = project_with_source("archive.zip");
        fs::write(project.path().join("history.zip"), "historical bytes").unwrap();
        let mut active = v2_capture(
            "source-one",
            "capture-active",
            "archive.zip",
            b"source bytes",
        );
        active["representation"] = json!("zip");
        let mut history = v2_capture(
            "source-one",
            "capture-history",
            "history.zip",
            b"historical bytes",
        );
        history["representation"] = json!("zip");
        write_catalog_v2_at(
            &project.path().join("catalog"),
            json!([v2_source("source-one", "capture-active")]),
            json!([active, history]),
        );
        let catalog = Catalog::load(project.path(), Path::new("catalog")).expect("load v2");
        let mut nodes = vec![node(
            "member.md",
            BTreeMap::from([(CONTAINER_SOURCE_ATTRIBUTE.into(), json!("archive.zip"))]),
        )];

        assert!(
            catalog
                .validate_graph_annotations(&KnowledgeGraph {
                    nodes: nodes.clone(),
                    ..KnowledgeGraph::default()
                })
                .is_err(),
            "missing active annotation must fail"
        );
        catalog
            .apply_to_nodes(&mut nodes)
            .expect("annotate container");
        assert!(
            catalog
                .validate_graph_annotations(&KnowledgeGraph {
                    nodes: nodes.clone(),
                    ..KnowledgeGraph::default()
                })
                .is_ok(),
            "exact container source must match"
        );

        let mut unrelated = node(
            "unrelated.md",
            BTreeMap::from([("catalog".into(), nodes[0].extra["catalog"].clone())]),
        );
        assert!(
            catalog
                .validate_graph_annotations(&KnowledgeGraph {
                    nodes: vec![unrelated.clone()],
                    ..KnowledgeGraph::default()
                })
                .is_err(),
            "unexpected unrelated annotation must fail"
        );
        unrelated.source_file = "history.zip".into();
        assert!(
            catalog
                .validate_graph_annotations(&KnowledgeGraph {
                    nodes: vec![unrelated],
                    ..KnowledgeGraph::default()
                })
                .is_err(),
            "inactive annotation must fail"
        );

        nodes[0]
            .extra
            .insert(CONTAINER_SOURCE_ATTRIBUTE.into(), json!("history.zip"));
        assert!(
            catalog
                .validate_graph_annotations(&KnowledgeGraph {
                    nodes,
                    ..KnowledgeGraph::default()
                })
                .is_err(),
            "container source matching must be exact"
        );
    }

    #[test]
    fn rejects_v2_source_with_dangling_active_capture() {
        let project = TempDir::new().expect("project tempdir");
        write_catalog_v2_at(
            &project.path().join("catalog"),
            json!([v2_source("source-one", "capture-missing")]),
            json!([]),
        );

        assert!(Catalog::load_metadata(project.path(), Path::new("catalog")).is_err());
    }

    #[test]
    fn rejects_duplicate_v2_source_and_capture_identities() {
        let project = TempDir::new().expect("project tempdir");
        write_catalog_v2_at(
            &project.path().join("catalog"),
            json!([
                v2_source("source-one", "capture-one"),
                v2_source("source-one", "capture-one")
            ]),
            json!([v2_capture(
                "source-one",
                "capture-one",
                "docs/page.md",
                b"source bytes"
            )]),
        );
        assert!(Catalog::load_metadata(project.path(), Path::new("catalog")).is_err());

        write_catalog_v2_at(
            &project.path().join("catalog"),
            json!([v2_source("source-one", "capture-one")]),
            json!([
                v2_capture("source-one", "capture-one", "docs/page.md", b"source bytes"),
                v2_capture(
                    "source-one",
                    "capture-one",
                    "docs/history.md",
                    b"historical bytes"
                ),
            ]),
        );
        assert!(Catalog::load_metadata(project.path(), Path::new("catalog")).is_err());
    }

    #[test]
    fn permits_the_same_v2_capture_id_for_distinct_sources() {
        let project = TempDir::new().expect("project tempdir");
        write_catalog_v2_at(
            &project.path().join("catalog"),
            json!([
                v2_source("source-one", "capture-shared"),
                v2_source("source-two", "capture-shared"),
            ]),
            json!([
                v2_capture("source-one", "capture-shared", "docs/one.md", b"one"),
                v2_capture("source-two", "capture-shared", "docs/two.md", b"two"),
            ]),
        );

        let catalog = Catalog::load_metadata(project.path(), Path::new("catalog"))
            .expect("capture identity includes its source");

        assert_eq!(
            catalog.citation_keys(),
            [
                "source-one#capture-shared".to_owned(),
                "source-two#capture-shared".to_owned(),
            ]
            .into_iter()
            .collect()
        );
    }

    #[test]
    fn rejects_v2_source_capture_mismatches_and_malformed_capture_fields() {
        let project = TempDir::new().expect("project tempdir");
        write_catalog_v2_at(
            &project.path().join("catalog"),
            json!([
                v2_source("source-one", "capture-shared"),
                v2_source("source-two", "capture-two"),
            ]),
            json!([
                v2_capture(
                    "source-two",
                    "capture-shared",
                    "docs/page.md",
                    b"source bytes"
                ),
                v2_capture("source-two", "capture-two", "docs/two.md", b"two"),
            ]),
        );
        assert!(Catalog::load_metadata(project.path(), Path::new("catalog")).is_err());

        for (field, value) in [
            ("sha256", json!("not-a-digest")),
            ("captured_at", json!("not-a-timestamp")),
            ("source_path", json!("../unsafe.md")),
            ("unexpected", json!(true)),
        ] {
            let mut capture =
                v2_capture("source-one", "capture-one", "docs/page.md", b"source bytes");
            capture[field] = value;
            write_catalog_v2_at(
                &project.path().join("catalog"),
                json!([v2_source("source-one", "capture-one")]),
                json!([capture]),
            );
            assert!(
                Catalog::load_metadata(project.path(), Path::new("catalog")).is_err(),
                "accepted malformed v2 capture {field}"
            );
        }
    }

    #[test]
    fn rejects_graph_annotations_that_do_not_match_the_active_capture() {
        let project = TempDir::new().expect("project tempdir");
        write_catalog_v2_at(
            &project.path().join("catalog"),
            json!([v2_source("source-one", "capture-one")]),
            json!([v2_capture(
                "source-one",
                "capture-one",
                "docs/page.md",
                b"source bytes"
            )]),
        );
        let catalog =
            Catalog::load_metadata(project.path(), Path::new("catalog")).expect("load v2 metadata");
        let mut stale = entry("docs/page.md");
        stale["sha256"] = json!("0".repeat(64));
        let graph = KnowledgeGraph {
            nodes: vec![node(
                "docs/page.md",
                BTreeMap::from([("catalog".into(), stale)]),
            )],
            ..KnowledgeGraph::default()
        };

        assert!(catalog.validate_graph_annotations(&graph).is_err());
    }

    #[test]
    fn loads_strict_catalog_and_exposes_citation_keys() {
        let project = project_with_source("docs/page.md");
        write_catalog(project.path(), json!([entry("docs/page.md")]));

        let catalog = Catalog::load(project.path(), Path::new("catalog")).expect("load catalog");

        assert_eq!(
            catalog.citation_keys(),
            ["source-one#capture-one".to_owned()].into_iter().collect()
        );
    }

    #[test]
    fn defers_digest_verification_until_explicit_publication_check() {
        let project = project_with_source("docs/page.md");
        let mut record = entry("docs/page.md");
        record["sha256"] = json!("0".repeat(64));
        write_catalog(project.path(), json!([record]));

        let catalog = Catalog::load(project.path(), Path::new("catalog"))
            .expect("admit structurally valid catalog without rehashing its source");
        let mut nodes = vec![node("docs/page.md", BTreeMap::new())];
        catalog
            .apply_to_nodes(&mut nodes)
            .expect("apply annotations without redundant source hashing");
        let error = catalog
            .verify_sources()
            .expect_err("publication verification must bind source bytes to the catalog digest");

        assert_eq!(
            error.to_string(),
            "catalog sha256 does not match source_path"
        );
        assert_eq!(nodes[0].extra["catalog"]["source_id"], "source-one");
    }

    #[test]
    fn binds_one_registry_origin_to_existing_catalog_provenance_rules() {
        let project = project_with_source("docs/defaults.yaml");
        let source = b"default_username: admin\ndefault_password: fake-only-password\n";
        fs::write(project.path().join("docs/defaults.yaml"), source).expect("write source");
        let registry = TempDir::new().expect("registry tempdir");
        initialize_tree(registry.path(), "demo-catalog").expect("initialize registry");
        add_origin(
            registry.path(),
            RegistryOrigin {
                version: 1,
                origin_id: "team-docs".into(),
                kind: "filesystem".into(),
                logical_name: "team-docs".into(),
            },
        )
        .expect("add origin");
        append_capture_and_activate(
            registry.path(),
            RegistryCapture {
                version: 1,
                capture_id: "capture-one".into(),
                source_id: "source-one".into(),
                relative_path: "docs/defaults.yaml".into(),
                sha256: sha256(source),
                observed_at: "2026-08-27T12:34:56Z".into(),
                representation: "yaml".into(),
            },
            Some("team-docs"),
        )
        .expect("append capture");
        let snapshot = RegistrySnapshot::load(registry.path()).expect("load registry");

        let catalog = Catalog::from_registry_origin(project.path(), &snapshot, "team-docs")
            .expect("bind registry origin");
        assert_eq!(
            catalog.citation_keys(),
            ["source-one#capture-one".to_owned()].into_iter().collect()
        );
        assert_eq!(
            catalog
                .active_source_paths()
                .map(str::to_owned)
                .collect::<Vec<_>>(),
            ["docs/defaults.yaml".to_owned()]
        );
        assert!(catalog.scan_exclusions().collect::<Vec<_>>().is_empty());
        let mut nodes = vec![node("docs/defaults.yaml", BTreeMap::new())];
        catalog
            .apply_to_nodes(&mut nodes)
            .expect("apply provenance");
        assert_eq!(nodes[0].extra["catalog"]["location"], "docs/defaults.yaml");
        assert_eq!(nodes[0].extra["catalog"]["url"], "local-registry:team-docs");
        catalog
            .verify_sources()
            .expect("rehash source before publish");
    }

    #[test]
    fn rejects_source_replaced_after_catalog_capture() {
        let project = project_with_source("docs/page.md");
        write_catalog(project.path(), json!([entry("docs/page.md")]));
        fs::write(project.path().join("docs/page.md"), "replacement bytes")
            .expect("replace source after capture");

        let catalog = Catalog::load(project.path(), Path::new("catalog"))
            .expect("admit source path without rereading its bytes");
        assert!(catalog.verify_sources().is_err());
    }

    #[test]
    fn exposes_exact_project_relative_catalog_file_scan_exclusion() {
        let project = project_with_source("docs/page.md");
        let catalog_dir = project.path().join("metadata/source-catalog");
        write_catalog_at(&catalog_dir, json!([entry("docs/page.md")]));
        // Absolute catalog paths must be symlink-free; canonicalize so the
        // test holds on platforms where the temp root itself is a symlink.
        let catalog_dir = fs::canonicalize(&catalog_dir).expect("canonical catalog directory");

        let catalog = Catalog::load(project.path(), &catalog_dir).expect("load nested catalog");

        assert_eq!(
            catalog.scan_exclusion(),
            "/metadata/source-catalog/catalog.json"
        );
    }

    #[test]
    fn roots_inactive_capture_scan_exclusions_at_the_project_boundary() {
        let project = project_with_source("elsewhere/raw/active.md");
        write_catalog_v2_at(
            &project.path().join("catalog"),
            json!([v2_source("source-one", "capture-active")]),
            json!([
                v2_capture(
                    "source-one",
                    "capture-active",
                    "elsewhere/raw/active.md",
                    b"source bytes"
                ),
                v2_capture(
                    "source-one",
                    "capture-history",
                    "raw",
                    b"historical source bytes"
                ),
            ]),
        );

        let catalog = Catalog::load(project.path(), Path::new("catalog"))
            .expect("load catalog with absent history");
        let detected = detect(
            project.path(),
            &DetectOptions {
                extra_excludes: catalog.scan_exclusions().collect(),
                ..DetectOptions::default()
            },
        )
        .expect("detect project");

        assert!(detected
            .files
            .values()
            .flatten()
            .any(|path| path.ends_with("elsewhere/raw/active.md")));
    }

    #[cfg(unix)]
    // Non-UTF-8 directory names can only exist on byte-oriented filesystems
    // (Linux ext4); APFS rejects them at creation, so the loader's UTF-8
    // rejection is only observable there.
    #[cfg(target_os = "linux")]
    #[test]
    fn rejects_non_utf8_catalog_scan_exclusion() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt as _};

        let project = project_with_source("docs/page.md");
        let catalog_dir = project
            .path()
            .join(OsString::from_vec(b"catalog-\xff".to_vec()));
        write_catalog_at(&catalog_dir, json!([entry("docs/page.md")]));

        let error = Catalog::load(project.path(), &catalog_dir)
            .err()
            .expect("reject non-UTF-8 catalog path");

        assert!(error.to_string().contains("not UTF-8"), "{error:#}");
    }

    #[test]
    fn rejects_unknown_fields_and_unsupported_versions() {
        let project = project_with_source("docs/page.md");
        let mut record = entry("docs/page.md");
        record["unexpected"] = json!(true);
        write_catalog(project.path(), json!([record]));
        assert!(Catalog::load(project.path(), Path::new("catalog")).is_err());

        fs::write(
            project.path().join("catalog/catalog.json"),
            serde_json::to_vec(&json!({"version": 2, "entries": []})).unwrap(),
        )
        .unwrap();
        assert!(Catalog::load(project.path(), Path::new("catalog")).is_err());

        fs::write(
            project.path().join("catalog/catalog.json"),
            serde_json::to_vec(&json!({"version": 1, "entries": [], "extra": null})).unwrap(),
        )
        .unwrap();
        assert!(Catalog::load(project.path(), Path::new("catalog")).is_err());
    }

    #[test]
    fn rejects_invalid_record_fields() {
        let project = project_with_source("docs/page.md");
        let invalid_values = [
            ("source_id", json!("")),
            ("source_id", json!("x".repeat(4097))),
            ("capture_id", json!("")),
            ("capture_id", json!("x".repeat(4097))),
            ("sha256", json!("ABCDEF")),
            ("sha256", json!("0".repeat(63))),
            ("captured_at", json!("2026-08-24")),
            ("accessed_at", json!("2026-13-24T12:34:56Z")),
            ("updated_at", json!("2026-08-24T12:34:56")),
            ("representation", json!("")),
            ("source_system", json!("")),
            ("url", json!("relative/page")),
            ("url", json!("ftp://docs.example.test/page")),
            ("url", json!("https://user@docs.example.test/page")),
            ("url", json!("https://@docs.example.test/page")),
            ("location", json!("")),
        ];

        for (field, value) in invalid_values {
            let mut record = entry("docs/page.md");
            record[field] = value;
            write_catalog(project.path(), json!([record]));
            assert!(
                Catalog::load(project.path(), Path::new("catalog")).is_err(),
                "accepted invalid {field}"
            );
        }
    }

    #[test]
    fn rejects_control_characters_in_catalog_metadata() {
        let project = project_with_source("docs/page.md");
        for (field, value) in [
            ("source_system", json!("confluence\ninjected")),
            ("location", json!("SPACE/\u{0001}Page")),
            ("representation", json!("markdown\rinjected")),
        ] {
            let mut record = entry("docs/page.md");
            record[field] = value;
            write_catalog(project.path(), json!([record]));

            let error = Catalog::load(project.path(), Path::new("catalog"))
                .err()
                .expect("reject V1 control-character metadata");
            assert!(
                error.to_string().contains("control characters"),
                "unexpected V1 {field} error: {error:#}"
            );
        }

        for (field, value) in [
            ("source_system", json!("confluence\ninjected")),
            ("location", json!("SPACE/\u{0001}Page")),
        ] {
            let mut source = v2_source("source-one", "capture-one");
            source[field] = value;
            write_catalog_v2_at(
                &project.path().join("catalog"),
                json!([source]),
                json!([v2_capture(
                    "source-one",
                    "capture-one",
                    "docs/page.md",
                    b"source bytes"
                )]),
            );

            let error = Catalog::load_metadata(project.path(), Path::new("catalog"))
                .err()
                .expect("reject V2 source control-character metadata");
            assert!(
                error.to_string().contains("control characters"),
                "unexpected V2 source {field} error: {error:#}"
            );
        }

        let mut capture = v2_capture("source-one", "capture-one", "docs/page.md", b"source bytes");
        capture["representation"] = json!("markdown\rinjected");
        write_catalog_v2_at(
            &project.path().join("catalog"),
            json!([v2_source("source-one", "capture-one")]),
            json!([capture]),
        );
        let error = Catalog::load_metadata(project.path(), Path::new("catalog"))
            .err()
            .expect("reject V2 capture control-character metadata");
        assert!(
            error.to_string().contains("control characters"),
            "unexpected V2 capture error: {error:#}"
        );
    }

    #[test]
    fn accepts_credential_free_non_web_source_locators() {
        let project = project_with_source("docs/page.md");
        for value in [
            "mcp://maas_glean/glean_search",
            "glean://chat/24ee75a9a2b84262a1e7b4d6979effd7",
            "gitlab://group/project/commit/5041ac6",
            "file:///Users/example/Downloads/input.md",
            "local-command:/tmp/example generated fixture",
            "/tmp/example/proto/device.proto",
        ] {
            let mut record = entry("docs/page.md");
            record["url"] = json!(value);
            write_catalog(project.path(), json!([record]));
            Catalog::load(project.path(), Path::new("catalog"))
                .expect("accept bounded credential-free source locator");

            let mut source = v2_source("source-one", "capture-one");
            source["url"] = json!(value);
            write_catalog_v2_at(
                &project.path().join("catalog"),
                json!([source]),
                json!([v2_capture(
                    "source-one",
                    "capture-one",
                    "docs/page.md",
                    b"source bytes"
                )]),
            );
            Catalog::load_metadata(project.path(), Path::new("catalog"))
                .expect("accept locator in a source record");
        }
    }

    #[test]
    fn rejects_secret_bearing_catalog_metadata_before_annotation() {
        let project = project_with_source("docs/page.md");
        for (field, value) in [
            (
                "url",
                json!("https://sharepoint.example.test/page?access_token=CATALOG_SECRET_SENTINEL"),
            ),
            (
                "location",
                json!("Documents; X-Amz-Signature=CATALOG_SECRET_SENTINEL"),
            ),
        ] {
            let mut record = entry("docs/page.md");
            record[field] = value;
            write_catalog(project.path(), json!([record]));

            let error = Catalog::load(project.path(), Path::new("catalog"))
                .err()
                .expect("reject secret-bearing catalog metadata");
            assert!(
                !error.to_string().contains("CATALOG_SECRET_SENTINEL"),
                "catalog rejection must not echo credentials: {error:#}"
            );
        }
    }

    #[test]
    fn rejects_oversized_metadata_before_percent_decoding() {
        let project = project_with_source("docs/page.md");
        let mut record = entry("docs/page.md");
        record["location"] = json!("%ZZ".repeat(MAX_CATALOG_ANNOTATION_BYTES / 3 + 1));
        write_catalog(project.path(), json!([record]));

        let error = match Catalog::load(project.path(), Path::new("catalog")) {
            Ok(_) => panic!("reject oversized metadata before decoding malformed percent escapes"),
            Err(error) => error,
        };

        assert_eq!(
            error.to_string(),
            "catalog entry metadata exceeds the 65536-byte limit"
        );
    }

    #[test]
    fn rejects_embedded_provider_tokens_before_catalog_source_resolution() {
        let project = project_with_source("docs/page.md");
        for (field, value) in [
            (
                "source_path",
                json!("Documents/sk-live-CATALOG_SECRET_SENTINEL"),
            ),
            (
                "location",
                json!("Documents/sk-live-CATALOG_SECRET_SENTINEL"),
            ),
            (
                "source_path",
                json!("Documents/sk-abcdefghijklmnop1234-CATALOG_SECRET_SENTINEL"),
            ),
            (
                "location",
                json!("Documents%2Fsk%2DCATALOG_SECRET_SENTINEL"),
            ),
        ] {
            let mut record = entry("docs/page.md");
            record[field] = value;
            write_catalog(project.path(), json!([record]));

            let error = Catalog::load(project.path(), Path::new("catalog"))
                .err()
                .expect("reject embedded provider token");
            assert!(
                !error.to_string().contains("CATALOG_SECRET_SENTINEL"),
                "catalog rejection must not echo credentials: {error:#}"
            );
        }
    }

    #[test]
    fn rejects_percent_encoded_signed_catalog_metadata_before_annotation() {
        let project = project_with_source("docs/page.md");
        for value in [
            "Documents%3B%20X%2DAmz%2DSignature%3DCATALOG_SECRET_SENTINEL",
            "Documents%253B%2520X%252DAmz%252DSignature%253DCATALOG_SECRET_SENTINEL",
        ] {
            let mut record = entry("docs/page.md");
            record["location"] = json!(value);
            write_catalog(project.path(), json!([record]));

            let error = Catalog::load(project.path(), Path::new("catalog"))
                .err()
                .expect("reject percent-encoded secret-bearing catalog metadata");

            assert!(
                !error.to_string().contains("CATALOG_SECRET_SENTINEL"),
                "catalog rejection must not echo credentials: {error:#}"
            );
        }
    }

    #[test]
    fn accepts_safe_nested_percent_encoded_catalog_metadata() {
        let project = project_with_source("docs/page.md");
        let mut record = entry("docs/page.md");
        record["location"] = json!("Documents%2520Page");
        write_catalog(project.path(), json!([record]));

        Catalog::load(project.path(), Path::new("catalog"))
            .expect("accept safe nested percent-encoded metadata");
    }

    #[test]
    fn rejects_percent_metadata_beyond_the_decode_layer_limit() {
        let project = project_with_source("docs/page.md");
        let mut record = entry("docs/page.md");
        record["location"] = json!(format!("%{}41", "25".repeat(MAX_PERCENT_DECODE_LAYERS)));
        write_catalog(project.path(), json!([record]));

        assert!(
            Catalog::load(project.path(), Path::new("catalog")).is_err(),
            "metadata requiring more than the bounded decode passes must not load"
        );
    }

    #[test]
    fn rejects_non_utf8_percent_decoded_catalog_metadata_before_annotation() {
        let project = project_with_source("docs/page.md");
        for value in [
            "x%FFapi%5Fkey=CATALOG_SECRET_SENTINEL",
            "x%25FFapi%255Fkey=CATALOG_SECRET_SENTINEL",
        ] {
            let mut record = entry("docs/page.md");
            record["location"] = json!(value);
            write_catalog(project.path(), json!([record]));

            let error = Catalog::load(project.path(), Path::new("catalog"))
                .err()
                .expect("reject non-UTF-8 decoded metadata");
            assert!(
                !error.to_string().contains("CATALOG_SECRET_SENTINEL"),
                "catalog rejection must not echo credentials: {error:#}"
            );
        }
    }

    #[test]
    fn rejects_malformed_percent_escape_after_nested_decoding() {
        let project = project_with_source("docs/page.md");
        let mut record = entry("docs/page.md");
        record["location"] = json!("Documents%25ZZ");
        write_catalog(project.path(), json!([record]));

        assert!(Catalog::load(project.path(), Path::new("catalog")).is_err());
    }

    #[test]
    fn rejects_non_normalized_missing_and_escaping_source_paths() {
        let project = project_with_source("docs/page.md");
        for source_path in [
            "/etc/passwd",
            "../page.md",
            "docs/../docs/page.md",
            "docs\\page.md",
            "docs/missing.md",
        ] {
            write_catalog(project.path(), json!([entry(source_path)]));
            assert!(
                Catalog::load(project.path(), Path::new("catalog")).is_err(),
                "accepted {source_path:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn rejects_source_paths_resolving_outside_the_project() {
        use std::os::unix::fs::symlink;

        let project = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::write(outside.path().join("page.md"), "outside").unwrap();
        symlink(outside.path(), project.path().join("docs")).unwrap();
        write_catalog(project.path(), json!([entry("docs/page.md")]));

        assert!(Catalog::load(project.path(), Path::new("catalog")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_source_even_when_its_target_is_inside_the_project() {
        use std::os::unix::fs::symlink;

        let project = project_with_source("docs/page.md");
        let target = project.path().join("docs/target.md");
        fs::write(&target, "source bytes").expect("source target");
        fs::remove_file(project.path().join("docs/page.md")).expect("replace source with link");
        symlink(&target, project.path().join("docs/page.md")).expect("source symlink");
        write_catalog(project.path(), json!([entry("docs/page.md")]));

        assert!(Catalog::load(project.path(), Path::new("catalog")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_hard_linked_source() {
        let project = project_with_source("docs/page.md");
        fs::hard_link(
            project.path().join("docs/page.md"),
            project.path().join("docs/second.md"),
        )
        .expect("source hard link");
        write_catalog(project.path(), json!([entry("docs/page.md")]));

        let error = Catalog::load(project.path(), Path::new("catalog"))
            .err()
            .expect("reject hard-linked source");
        assert_eq!(error.to_string(), "catalog source changed or is unsafe");
    }

    #[test]
    fn rejects_duplicate_citation_keys_and_ambiguous_source_paths() {
        let project = project_with_source("docs/page.md");
        let record = entry("docs/page.md");
        write_catalog(project.path(), json!([record.clone(), record]));
        assert!(Catalog::load(project.path(), Path::new("catalog")).is_err());

        let mut second = entry("docs/page.md");
        second["source_id"] = json!("source-two");
        second["capture_id"] = json!("capture-two");
        write_catalog(project.path(), json!([entry("docs/page.md"), second]));
        assert!(Catalog::load(project.path(), Path::new("catalog")).is_err());
    }

    #[test]
    fn rejects_duplicate_citation_keys_and_unreferenceable_ids() {
        let project = project_with_source("docs/one.md");
        fs::write(project.path().join("docs/two.md"), "second source").unwrap();
        write_catalog(
            project.path(),
            json!([entry("docs/one.md"), entry("docs/two.md")]),
        );
        assert!(Catalog::load(project.path(), Path::new("catalog")).is_err());

        for (field, value) in [
            ("source_id", "#source"),
            ("source_id", "-source"),
            ("source_id", "source space"),
            ("capture_id", "capture#fragment"),
            ("capture_id", "capture/one"),
        ] {
            let mut record = entry("docs/one.md");
            record[field] = json!(value);
            write_catalog(project.path(), json!([record]));
            assert!(
                Catalog::load(project.path(), Path::new("catalog")).is_err(),
                "accepted unreferenceable {field}={value:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_catalog_directory_or_file() {
        use std::os::unix::fs::symlink;

        let project = project_with_source("docs/page.md");
        let outside = TempDir::new().unwrap();
        write_catalog(outside.path(), json!([entry("docs/page.md")]));
        symlink(
            outside.path().join("catalog"),
            project.path().join("catalog"),
        )
        .unwrap();
        assert!(Catalog::load(project.path(), Path::new("catalog")).is_err());

        fs::remove_file(project.path().join("catalog")).unwrap();
        fs::create_dir(project.path().join("catalog")).unwrap();
        symlink(
            outside.path().join("catalog/catalog.json"),
            project.path().join("catalog/catalog.json"),
        )
        .unwrap();
        assert!(Catalog::load(project.path(), Path::new("catalog")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_hard_linked_catalog_file_with_stable_unsafe_path_error() {
        let project = project_with_source("docs/page.md");
        let outside = TempDir::new().unwrap();
        let outside_catalog = outside.path().join("catalog.json");
        fs::write(
            &outside_catalog,
            serde_json::to_vec(&json!({
                "version": 1,
                "entries": [entry("docs/page.md")]
            }))
            .unwrap(),
        )
        .unwrap();
        fs::create_dir(project.path().join("catalog")).unwrap();
        fs::hard_link(outside_catalog, project.path().join("catalog/catalog.json")).unwrap();

        let error = Catalog::load(project.path(), Path::new("catalog"))
            .err()
            .expect("reject hard-linked catalog");

        assert_eq!(error.to_string(), "catalog input changed or is unsafe");
    }

    #[test]
    fn rejects_oversized_catalogs_and_extra_directory_entries() {
        let project = project_with_source("docs/page.md");
        let directory = project.path().join("catalog");
        fs::create_dir(&directory).unwrap();
        fs::write(
            directory.join("catalog.json"),
            vec![b' '; 16 * 1024 * 1024 + 1],
        )
        .unwrap();
        assert!(Catalog::load(project.path(), Path::new("catalog")).is_err());

        write_catalog(project.path(), json!([entry("docs/page.md")]));
        fs::write(directory.join("unexpected.json"), "{}").unwrap();
        assert!(Catalog::load(project.path(), Path::new("catalog")).is_err());
    }

    #[test]
    fn rejects_catalog_directories_outside_the_project() {
        let project = project_with_source("docs/page.md");
        let outside = TempDir::new().unwrap();
        write_catalog(outside.path(), json!([entry("docs/page.md")]));

        assert!(Catalog::load(project.path(), outside.path().join("catalog").as_path()).is_err());
    }

    #[test]
    fn annotates_normal_and_container_nodes_deterministically() {
        let project = project_with_source("docs/page.md");
        fs::write(project.path().join("archive.zip"), "archive").unwrap();
        let mut archive_entry = entry_for_source("archive.zip", b"archive");
        archive_entry["source_id"] = json!("archive-source");
        archive_entry["capture_id"] = json!("archive-capture");
        write_catalog(
            project.path(),
            json!([archive_entry, entry("docs/page.md")]),
        );
        let catalog = Catalog::load(project.path(), Path::new("catalog")).unwrap();
        let mut nodes = vec![
            node("docs/page.md", BTreeMap::new()),
            node(
                "archive.zip!/member.md",
                BTreeMap::from([(CONTAINER_SOURCE_ATTRIBUTE.into(), json!("archive.zip"))]),
            ),
            node("unmatched.md", BTreeMap::new()),
        ];

        catalog.apply_to_nodes(&mut nodes).unwrap();
        let first = serde_json::to_value(&nodes).unwrap();
        catalog.apply_to_nodes(&mut nodes).unwrap();

        assert_eq!(serde_json::to_value(&nodes).unwrap(), first);
        assert_eq!(nodes[0].extra["catalog"], entry("docs/page.md"));
        assert_eq!(nodes[1].extra["catalog"], {
            let mut expected = entry_for_source("archive.zip", b"archive");
            expected["source_id"] = json!("archive-source");
            expected["capture_id"] = json!("archive-capture");
            expected
        });
        assert!(!nodes[2].extra.contains_key("catalog"));
    }

    #[test]
    fn defers_catalog_source_revalidation_until_publication() {
        let project = project_with_source("docs/page.md");
        write_catalog(project.path(), json!([entry("docs/page.md")]));
        let catalog = Catalog::load(project.path(), Path::new("catalog")).expect("load catalog");
        let mut nodes = vec![node(
            "docs/page.md",
            BTreeMap::from([("catalog".into(), json!({"stale": true}))]),
        )];
        fs::write(project.path().join("docs/page.md"), "replacement bytes")
            .expect("change source after catalog admission");

        catalog
            .apply_to_nodes(&mut nodes)
            .expect("apply annotations before the single publication verification");
        let error = catalog
            .verify_sources()
            .expect_err("reject source changed after catalog admission before publication");

        assert_eq!(
            error.to_string(),
            "catalog sha256 does not match source_path"
        );
        assert_eq!(nodes[0].extra["catalog"]["source_id"], "source-one");
    }

    #[test]
    fn revalidates_catalog_sources_not_present_in_nodes_before_publication() {
        let project = project_with_source("docs/page.md");
        fs::write(project.path().join("docs/other.md"), "other source").expect("other source");
        let mut other = entry_for_source("docs/other.md", b"other source");
        other["source_id"] = json!("source-two");
        other["capture_id"] = json!("capture-two");
        write_catalog(project.path(), json!([entry("docs/page.md"), other]));
        let catalog = Catalog::load(project.path(), Path::new("catalog")).expect("load catalog");
        let mut nodes = vec![node("docs/page.md", BTreeMap::new())];
        catalog
            .apply_to_nodes(&mut nodes)
            .expect("annotate matching source");
        fs::write(project.path().join("docs/other.md"), "changed source")
            .expect("change unmatched source after admission");

        let error = catalog
            .verify_sources()
            .expect_err("revalidate every catalog source before publication");

        assert_eq!(
            error.to_string(),
            "catalog sha256 does not match source_path"
        );
    }

    #[test]
    fn reserves_raw_retained_applied_and_transient_v2_catalog_memory() {
        assert_eq!(
            Catalog::memory_reservation_bytes(),
            192 * 1024 * 1024,
            "the catalog reservation covers raw input, source records, retained annotations, indexes, and graph annotations"
        );
    }

    #[test]
    fn admits_a_near_limit_v2_source_record_set_within_the_reserved_bound() {
        let project = TempDir::new().expect("project tempdir");
        let mut sources = Vec::new();
        let mut captures = Vec::new();
        for index in 0..200 {
            let source_id = format!("source-{index:0>4}");
            let capture_id = format!("capture-{index:0>4}");
            let mut source = v2_source(&source_id, &capture_id);
            source["location"] = json!("x".repeat(48 * 1024));
            sources.push(source);
            captures.push(v2_capture(
                &source_id,
                &capture_id,
                &format!("docs/{index}.md"),
                b"source bytes",
            ));
        }
        write_catalog_v2_at(
            &project.path().join("catalog"),
            Value::Array(sources),
            Value::Array(captures),
        );

        Catalog::load_metadata(project.path(), Path::new("catalog"))
            .expect("bounded v2 source records fit the catalog reservation");
    }

    #[test]
    fn rejects_catalog_when_retained_index_metadata_exceeds_its_limit() {
        let project = TempDir::new().expect("project tempdir");
        let mut entries = Vec::new();
        for index in 0..1_850 {
            let source_path = format!("docs/{index}.md");
            let path = project.path().join(&source_path);
            fs::create_dir_all(path.parent().expect("source parent"))
                .expect("create source parent");
            fs::write(path, "source bytes").expect("write source");

            let mut record = entry_for_source(&source_path, b"source bytes");
            record["source_id"] = json!(format!("s{index:0>4095}"));
            record["capture_id"] = json!(format!("c{index:0>4095}"));
            entries.push(record);
        }
        write_catalog(project.path(), Value::Array(entries));

        let error = match Catalog::load(project.path(), Path::new("catalog")) {
            Ok(_) => panic!("reject catalog with excessive retained index metadata"),
            Err(error) => error,
        };

        assert_eq!(
            error.to_string(),
            "catalog retained index metadata exceeds the 16777216-byte limit"
        );
    }

    #[test]
    fn container_marker_precedes_source_spelling_without_guessing_syntax() {
        let project = project_with_source("odd!/page.md");
        fs::write(project.path().join("archive.zip"), "archive").unwrap();
        let mut archive_entry = entry_for_source("archive.zip", b"archive");
        archive_entry["source_id"] = json!("archive-source");
        archive_entry["capture_id"] = json!("archive-capture");
        write_catalog(
            project.path(),
            json!([entry("odd!/page.md"), archive_entry]),
        );
        let catalog = Catalog::load(project.path(), Path::new("catalog")).unwrap();
        let mut nodes = vec![
            node("odd!/page.md", BTreeMap::new()),
            node(
                "member.md",
                BTreeMap::from([(CONTAINER_SOURCE_ATTRIBUTE.into(), json!("archive.zip"))]),
            ),
        ];

        catalog.apply_to_nodes(&mut nodes).unwrap();

        assert_eq!(nodes[0].extra["catalog"], entry("odd!/page.md"));
        assert_eq!(nodes[1].extra["catalog"]["source_id"], "archive-source");
    }

    #[test]
    fn rejects_url_query_and_fragment_before_annotation() {
        let project = project_with_source("docs/page.md");
        for url in [
            "https://sharepoint.example.test/page?id=42",
            "https://sharepoint.example.test/page#section",
        ] {
            let mut record = entry("docs/page.md");
            record["url"] = json!(url);
            write_catalog(project.path(), json!([record]));

            assert!(
                Catalog::load(project.path(), Path::new("catalog")).is_err(),
                "accepted unsafe URL {url:?}"
            );
        }
    }

    #[test]
    fn applying_catalog_removes_only_stale_catalog_annotations() {
        let project = project_with_source("docs/page.md");
        write_catalog(project.path(), json!([]));
        let catalog = Catalog::load(project.path(), Path::new("catalog")).unwrap();
        let mut nodes = vec![node(
            "docs/page.md",
            BTreeMap::from([
                ("catalog".into(), json!({"stale": true})),
                ("preserved".into(), json!("yes")),
            ]),
        )];

        catalog.apply_to_nodes(&mut nodes).unwrap();

        assert!(!nodes[0].extra.contains_key("catalog"));
        assert_eq!(nodes[0].extra["preserved"], json!("yes"));
    }

    #[test]
    fn accepts_replicated_annotations_above_the_measured_catalog_size() {
        let project = project_with_source("docs/page.md");
        let mut record = entry("docs/page.md");
        record["location"] = json!("x".repeat(16 * 1024));
        let expected_annotation = record.clone();
        write_catalog(project.path(), json!([record]));
        let catalog = Catalog::load(project.path(), Path::new("catalog")).unwrap();
        let mut nodes = (0..4_480)
            .map(|_| node("docs/page.md", BTreeMap::new()))
            .collect::<Vec<_>>();

        catalog
            .apply_to_nodes(&mut nodes)
            .expect("annotate the measured-size catalog payload");

        assert!(
            nodes
                .iter()
                .all(|node| node.extra.get("catalog") == Some(&expected_annotation)),
            "catalog annotations must preserve the flat catalog value"
        );
    }

    #[test]
    fn refuses_replicated_annotations_above_the_total_limit_before_modifying_nodes() {
        let project = project_with_source("docs/page.md");
        let mut record = entry("docs/page.md");
        record["location"] = json!("x".repeat(16 * 1024));
        write_catalog(project.path(), json!([record]));
        let catalog = Catalog::load(project.path(), Path::new("catalog")).unwrap();
        let mut nodes = (0..8_192)
            .map(|_| {
                node(
                    "docs/page.md",
                    BTreeMap::from([("catalog".into(), json!({"stale": true}))]),
                )
            })
            .collect::<Vec<_>>();

        let error = catalog
            .apply_to_nodes(&mut nodes)
            .expect_err("reject oversized repeated annotation payload");
        assert!(
            error.to_string().contains("catalog annotations exceed"),
            "{error:#}"
        );
        assert_eq!(
            nodes
                .iter()
                .filter(|node| node.extra.get("catalog") == Some(&json!({"stale": true})))
                .count(),
            nodes.len(),
            "an oversized repeated catalog annotation must not partially modify nodes"
        );
    }
}
