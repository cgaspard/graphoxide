//! Deterministic Markdown wiki index validation and rendering.

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    fs,
    io::Write as _,
    path::{Component, Path, PathBuf},
};
#[cfg(any(all(target_os = "linux", target_arch = "x86_64"), target_os = "macos"))]
use std::{
    ffi::CString,
    io::Read as _,
    os::{
        fd::{AsRawFd as _, FromRawFd as _},
        unix::{
            ffi::OsStrExt as _,
            fs::{MetadataExt as _, PermissionsExt as _},
        },
    },
    sync::atomic::{AtomicU64, Ordering},
};

const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const MAX_PAGE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_FRONTMATTER_BYTES: usize = 64 * 1024;
const MAX_CANONICAL_PLAN_BYTES: usize = 8 * 1024 * 1024;
const MAX_DRAFT_SOURCES: usize = 12;
pub(crate) const MAX_CANONICAL_DRAFT_SECTIONS: usize = 8;
const MAX_CANONICAL_QUALITY_DIAGNOSTICS: usize = 256;
#[cfg(any(all(target_os = "linux", target_arch = "x86_64"), target_os = "macos"))]
static OUTPUT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WikiCheckReport {
    pub page_count: usize,
    pub output: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WikiConfig {
    version: u8,
    roots: Vec<String>,
    exclude: Vec<String>,
    required_frontmatter: Vec<String>,
    output: String,
}

#[derive(Debug)]
struct Page {
    path: PathBuf,
    title: String,
    sources: Vec<String>,
    structured: Option<StructuredPage>,
}

#[derive(Debug)]
struct StructuredPage {
    kind: String,
    graph_ref: String,
    parent: String,
    input_sha256: String,
    frontmatter_sha256: String,
    draft: Option<DraftMetadata>,
    canonical: Option<CanonicalMetadata>,
    tree_root: PathBuf,
    body: String,
}

#[derive(Debug)]
struct DraftMetadata {
    model: String,
    evidence_sha256: String,
}

#[derive(Debug)]
struct CanonicalMetadata {
    article_type: String,
    domain: String,
    coverage: String,
    review_status: String,
}

#[derive(Serialize)]
struct CanonicalQualityDiagnostic {
    citation: String,
    severity: &'static str,
    code: &'static str,
    message: &'static str,
}

#[derive(Serialize)]
struct CanonicalQualityReport {
    canonical_page_count: usize,
    complete_source_count: usize,
    partial_source_count: usize,
    inventory_only_source_count: usize,
    diagnostics: Vec<CanonicalQualityDiagnostic>,
}

/// A generated page represented without filesystem access.
///
/// Both the on-disk checker and the draft publisher use this view so generated
/// hierarchy and local-link policy cannot drift between validation paths.
pub(crate) struct GeneratedWikiPage<'a> {
    pub path: &'a Path,
    pub kind: &'a str,
    pub graph_ref: &'a str,
    pub parent: &'a str,
    pub input_sha256: &'a str,
    pub tree_root: &'a Path,
    pub body: &'a str,
}

pub fn index(root: &Path, config_path: &Path) -> anyhow::Result<WikiCheckReport> {
    let indexed = load(root, config_path, None, None, None)?;
    write_index(&indexed)?;
    Ok(check_report(&indexed))
}

pub fn check(
    root: &Path,
    config_path: &Path,
    citations: Option<&BTreeSet<String>>,
) -> anyhow::Result<WikiCheckReport> {
    check_with_graph(root, config_path, citations, None, None)
}

pub fn check_with_graph(
    root: &Path,
    config_path: &Path,
    citations: Option<&BTreeSet<String>>,
    graph: Option<&graphoxide_core::KnowledgeGraph>,
    active_annotations: Option<&BTreeMap<String, serde_json::Value>>,
) -> anyhow::Result<WikiCheckReport> {
    let indexed = load(root, config_path, citations, graph, active_annotations)?;
    let current = read_output(
        &indexed.output_root,
        &indexed.output_relative,
        indexed.rendered.len(),
    )?;
    anyhow::ensure!(
        current == indexed.rendered,
        "generated wiki output is stale: {}",
        indexed.output.display()
    );
    Ok(check_report(&indexed))
}

/// Validate a canonical rendered wiki against the exact reviewed-plan render.
///
/// The caller supplies the already validated graph/catalog projection so this
/// path never reads source bytes.
pub fn check_with_canonical_plan(
    root: &Path,
    config_path: &Path,
    citations: &BTreeSet<String>,
    graph: &graphoxide_core::KnowledgeGraph,
    active_annotations: &BTreeMap<String, serde_json::Value>,
    expected: &graphoxide_export::StructuredWikiPlan,
) -> anyhow::Result<WikiCheckReport> {
    check_canonical_plan(
        root,
        config_path,
        citations,
        graph,
        active_annotations,
        expected,
    )
    .map(|(report, _)| report)
}

/// Validate a canonical wiki and return its bounded machine-readable quality report.
#[doc(hidden)]
pub fn check_with_canonical_plan_quality(
    root: &Path,
    config_path: &Path,
    citations: &BTreeSet<String>,
    graph: &graphoxide_core::KnowledgeGraph,
    active_annotations: &BTreeMap<String, serde_json::Value>,
    expected: &graphoxide_export::StructuredWikiPlan,
) -> anyhow::Result<(WikiCheckReport, serde_json::Value)> {
    let (report, quality) = check_canonical_plan(
        root,
        config_path,
        citations,
        graph,
        active_annotations,
        expected,
    )?;
    Ok((
        report.clone(),
        serde_json::json!({
            "status": "ok",
            "page_count": report.page_count,
            "output": report.output,
            "canonical_page_count": quality.canonical_page_count,
            "complete_source_count": quality.complete_source_count,
            "partial_source_count": quality.partial_source_count,
            "inventory_only_source_count": quality.inventory_only_source_count,
            "diagnostics": quality.diagnostics,
        }),
    ))
}

fn check_canonical_plan(
    root: &Path,
    config_path: &Path,
    citations: &BTreeSet<String>,
    graph: &graphoxide_core::KnowledgeGraph,
    active_annotations: &BTreeMap<String, serde_json::Value>,
    expected: &graphoxide_export::StructuredWikiPlan,
) -> anyhow::Result<(WikiCheckReport, CanonicalQualityReport)> {
    let indexed = load(
        root,
        config_path,
        Some(citations),
        Some(graph),
        Some(active_annotations),
    )?;
    let current = read_output(
        &indexed.output_root,
        &indexed.output_relative,
        indexed.rendered.len(),
    )?;
    anyhow::ensure!(
        current == indexed.rendered,
        "generated wiki output is stale: {}",
        indexed.output.display()
    );
    validate_canonical_render_matches(&indexed.pages, expected)?;
    let quality = canonical_quality_report(&indexed, graph, active_annotations)?;
    Ok((check_report(&indexed), quality))
}

/// Read and validate a reviewed plan through the same no-follow descriptor
/// boundary used for wiki inputs.
pub fn load_canonical_plan(
    root: &Path,
    plan_path: &Path,
    catalog_citations: &BTreeSet<String>,
) -> anyhow::Result<graphoxide_export::WikiPlan> {
    let root = root.canonicalize().context("resolve wiki root")?;
    anyhow::ensure!(
        root.is_dir(),
        "wiki root is not a directory: {}",
        root.display()
    );
    let directory = OutputDirectory::open_existing(&root)?;
    let relative = config_relative_path(&root, plan_path)?;
    let bytes = directory.read_bounded_regular(&relative, MAX_CANONICAL_PLAN_BYTES)?;
    graphoxide_export::load_wiki_plan(&bytes, catalog_citations)
}

struct IndexedWiki {
    output_root: OutputDirectory,
    output_relative: PathBuf,
    output: PathBuf,
    rendered: String,
    page_count: usize,
    pages: Vec<Page>,
}

fn check_report(indexed: &IndexedWiki) -> WikiCheckReport {
    WikiCheckReport {
        page_count: indexed.page_count,
        output: indexed.output.clone(),
    }
}

fn canonical_quality_report(
    indexed: &IndexedWiki,
    graph: &graphoxide_core::KnowledgeGraph,
    active_annotations: &BTreeMap<String, serde_json::Value>,
) -> anyhow::Result<CanonicalQualityReport> {
    let mut quality = CanonicalQualityReport {
        canonical_page_count: 0,
        complete_source_count: 0,
        partial_source_count: 0,
        inventory_only_source_count: 0,
        diagnostics: Vec::new(),
    };
    let mut omitted = 0_usize;
    for page in &indexed.pages {
        let Some(structured) = &page.structured else {
            continue;
        };
        let Some(canonical) = &structured.canonical else {
            continue;
        };
        quality.canonical_page_count += 1;
        let citation = page.sources.first().cloned().unwrap_or_default();
        if canonical.review_status == "stale" {
            push_quality_diagnostic(
                &mut quality.diagnostics,
                &mut omitted,
                CanonicalQualityDiagnostic {
                    citation: citation.clone(),
                    severity: "warning",
                    code: "review-status-stale",
                    message: "The page is marked stale and needs review.",
                },
            );
        }
        if structured.kind != "source" {
            continue;
        }
        match canonical.coverage.as_str() {
            "complete" => quality.complete_source_count += 1,
            "partial" => {
                quality.partial_source_count += 1;
                push_quality_diagnostic(
                    &mut quality.diagnostics,
                    &mut omitted,
                    CanonicalQualityDiagnostic {
                        citation,
                        severity: "warning",
                        code: "coverage-partial",
                        message: "The reviewed plan declares partial extraction coverage.",
                    },
                );
            }
            "inventory-only" => {
                quality.inventory_only_source_count += 1;
                push_quality_diagnostic(
                    &mut quality.diagnostics,
                    &mut omitted,
                    CanonicalQualityDiagnostic {
                        citation,
                        severity: "info",
                        code: "coverage-inventory-only",
                        message: "No active graph evidence is available for this catalog capture.",
                    },
                );
            }
            _ => unreachable!("canonical coverage was validated while loading"),
        }
    }
    let evidence = graphoxide_export::project_wiki_evidence(graph, Some(active_annotations))?;
    for source in evidence.sources {
        if source.statuses.iter().any(|status| {
            let status = status.to_ascii_lowercase();
            status.contains("partial")
                || status.contains("reject")
                || status.contains("unsupported")
        }) {
            push_quality_diagnostic(
                &mut quality.diagnostics,
                &mut omitted,
                CanonicalQualityDiagnostic {
                    citation: source.citation.clone(),
                    severity: "warning",
                    code: "extraction-non-complete",
                    message: "Graph evidence records a non-complete extraction status.",
                },
            );
        }
        if !source.diagnostics.is_empty() {
            push_quality_diagnostic(
                &mut quality.diagnostics,
                &mut omitted,
                CanonicalQualityDiagnostic {
                    citation: source.citation.clone(),
                    severity: "warning",
                    code: "extraction-diagnostic",
                    message: "Graph evidence retains one or more extractor diagnostics.",
                },
            );
        }
        if source.title_candidates.len() == 1 && source.title_candidates[0] == source.source_id {
            push_quality_diagnostic(
                &mut quality.diagnostics,
                &mut omitted,
                CanonicalQualityDiagnostic {
                    citation: source.citation,
                    severity: "info",
                    code: "source-title-fallback",
                    message: "No extractor title was retained; the source identifier is in use.",
                },
            );
        }
    }
    if omitted != 0 {
        quality.diagnostics.push(CanonicalQualityDiagnostic {
            citation: String::new(),
            severity: "warning",
            code: "quality-diagnostics-truncated",
            message: "Additional quality diagnostics were omitted by the fixed output limit.",
        });
    }
    Ok(quality)
}

fn push_quality_diagnostic(
    diagnostics: &mut Vec<CanonicalQualityDiagnostic>,
    omitted: &mut usize,
    diagnostic: CanonicalQualityDiagnostic,
) {
    if diagnostics.len() < MAX_CANONICAL_QUALITY_DIAGNOSTICS.saturating_sub(1) {
        diagnostics.push(diagnostic);
    } else {
        *omitted += 1;
    }
}

fn load(
    root: &Path,
    config_path: &Path,
    citations: Option<&BTreeSet<String>>,
    graph: Option<&graphoxide_core::KnowledgeGraph>,
    active_annotations: Option<&BTreeMap<String, serde_json::Value>>,
) -> anyhow::Result<IndexedWiki> {
    let root = root.canonicalize().context("resolve wiki root")?;
    anyhow::ensure!(
        root.is_dir(),
        "wiki root is not a directory: {}",
        root.display()
    );
    let output_root = OutputDirectory::open_existing(&root)?;
    let config_relative = config_relative_path(&root, config_path)?;
    let config_path = root.join(&config_relative);
    let config = read_config(&output_root, &config_relative, &config_path)?;
    validate_config(&config)?;
    let output = safe_join(&root, &config.output, "output")?;
    anyhow::ensure!(
        config_path != output,
        "wiki output must not replace its config: {}",
        config_path.display()
    );
    let output_relative = output
        .strip_prefix(&root)
        .context("wiki output escaped its root")?
        .to_path_buf();
    reject_symlink(&output, "output")?;
    let excluded = config
        .exclude
        .iter()
        .map(|path| safe_join(&root, path, "exclude"))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let mut pages = Vec::new();
    for configured_root in &config.roots {
        let walk_root = safe_join(&root, configured_root, "root")?;
        let tree_root = walk_root
            .strip_prefix(&root)
            .context("wiki page root escaped its root")?;
        reject_symlink(&walk_root, "root")?;
        anyhow::ensure!(
            walk_root.is_dir(),
            "wiki root does not exist or is not a directory: {}",
            walk_root.display()
        );
        let mut walker = ignore::WalkBuilder::new(&walk_root);
        walker
            .hidden(false)
            .follow_links(false)
            .git_ignore(true)
            .git_global(false)
            .git_exclude(false)
            .require_git(false)
            .add_custom_ignore_filename(".graphoxideignore");
        for entry in walker.build() {
            let entry = entry?;
            let path = entry.path();
            if path == output || is_excluded(path, &excluded) || !is_markdown(path) {
                continue;
            }
            if entry.file_type().is_some_and(|kind| kind.is_symlink()) {
                anyhow::bail!("symlinked wiki page is not allowed: {}", path.display());
            }
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                anyhow::bail!("non-regular wiki page is not allowed: {}", path.display());
            }
            let relative = path
                .strip_prefix(&root)
                .context("wiki page escaped its root")?;
            if relative.as_os_str().is_empty() {
                continue;
            }
            pages.push(parse_page(
                &output_root,
                relative,
                path,
                &config.required_frontmatter,
                tree_root,
            )?);
        }
    }
    pages.sort_by(|left, right| left.path.cmp(&right.path));
    pages.dedup_by(|left, right| left.path == right.path);
    if let Some(citations) = citations {
        for page in &pages {
            for source in &page.sources {
                anyhow::ensure!(
                    citations.contains(source),
                    "wiki page {} references unknown citation {source}",
                    page.path.display()
                );
            }
        }
    }
    let hierarchy = validate_structured_pages(&output_root, &pages)?;
    if let Some(graph) = graph {
        validate_graph_coverage(&pages, graph, active_annotations)?;
    }
    let rendered = render(&root, &output, &pages, hierarchy.as_deref())?;
    Ok(IndexedWiki {
        output_root,
        output_relative,
        output,
        rendered,
        page_count: pages.len(),
        pages,
    })
}

fn write_index(indexed: &IndexedWiki) -> anyhow::Result<()> {
    let (parent, name) = output_parent(&indexed.output_root, &indexed.output_relative)?;
    write_text_atomic_in(&parent, &name, &indexed.rendered)
}

fn read_config(root: &OutputDirectory, relative: &Path, path: &Path) -> anyhow::Result<WikiConfig> {
    let text = read_text_in(root, relative, MAX_CONFIG_BYTES, "wiki config")?;
    serde_json::from_str(&text).with_context(|| format!("parse wiki config {}", path.display()))
}

fn config_relative_path(root: &Path, path: &Path) -> anyhow::Result<PathBuf> {
    // `root` is canonicalized by every caller; resolve an absolute config
    // path the same way so the prefix comparison is consistent on platforms
    // with stable ancestor symlinks (macOS maps /var to /private/var).
    let path = if path.is_absolute() {
        path.canonicalize()
            .with_context(|| format!("resolve wiki config path {}", path.display()))?
    } else {
        root.join(path)
    };
    let relative = path
        .strip_prefix(root)
        .context("wiki config must be beneath the wiki root")?;
    let mut normalized = PathBuf::new();
    for component in relative.components() {
        match component {
            Component::Normal(component) => normalized.push(component),
            Component::CurDir => {}
            Component::ParentDir => {
                anyhow::ensure!(
                    normalized.pop(),
                    "wiki config must be beneath the wiki root"
                );
            }
            Component::RootDir | Component::Prefix(_) => {
                anyhow::bail!("wiki config must be beneath the wiki root")
            }
        }
    }
    anyhow::ensure!(
        !normalized.as_os_str().is_empty(),
        "wiki config must be a file beneath the wiki root"
    );
    Ok(normalized)
}

fn validate_config(config: &WikiConfig) -> anyhow::Result<()> {
    anyhow::ensure!(
        config.version == 1,
        "unsupported wiki config version {}",
        config.version
    );
    anyhow::ensure!(
        !config.roots.is_empty(),
        "wiki config roots must not be empty"
    );
    for path in &config.roots {
        if path == "." {
            continue;
        }
        validate_safe_relative(path, "wiki config path")?;
    }
    for path in &config.exclude {
        validate_safe_relative(path, "wiki config path")?;
    }
    validate_safe_relative(&config.output, "wiki output")?;
    anyhow::ensure!(config.output == "llms.txt", "wiki output must be llms.txt");
    anyhow::ensure!(
        !config.required_frontmatter.is_empty(),
        "required_frontmatter must not be empty"
    );
    let mut seen = BTreeSet::new();
    for key in &config.required_frontmatter {
        anyhow::ensure!(valid_key(key), "invalid required frontmatter key {key:?}");
        anyhow::ensure!(
            seen.insert(key),
            "duplicate required frontmatter key {key:?}"
        );
    }
    Ok(())
}

fn safe_join(root: &Path, value: &str, kind: &str) -> anyhow::Result<PathBuf> {
    if kind == "root" && value == "." {
        return Ok(root.to_path_buf());
    }
    validate_safe_relative(value, kind)?;
    let path = root.join(value);
    let mut parent = path.parent();
    while let Some(candidate) = parent {
        if candidate.exists() {
            let resolved = candidate
                .canonicalize()
                .with_context(|| format!("resolve {kind} parent {}", candidate.display()))?;
            anyhow::ensure!(
                resolved.starts_with(root),
                "unsafe {kind}: {value} escapes {}",
                root.display()
            );
            return Ok(path);
        }
        parent = candidate.parent();
    }
    anyhow::bail!("unsafe {kind}: {value}")
}

fn validate_safe_relative(value: &str, kind: &str) -> anyhow::Result<()> {
    anyhow::ensure!(!value.is_empty(), "{kind} must not be empty");
    let path = Path::new(value);
    anyhow::ensure!(!path.is_absolute(), "unsafe absolute {kind}: {value}");
    anyhow::ensure!(
        path.components()
            .all(|component| matches!(component, Component::Normal(_))),
        "unsafe {kind}: {value}"
    );
    Ok(())
}

fn reject_symlink(path: &Path, kind: &str) -> anyhow::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => anyhow::ensure!(
            !metadata.file_type().is_symlink(),
            "symlinked wiki {kind} is not allowed: {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn is_excluded(path: &Path, excluded: &[PathBuf]) -> bool {
    excluded.iter().any(|exclude| path.starts_with(exclude))
}

fn is_markdown(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
}

fn parse_page(
    root: &OutputDirectory,
    relative: &Path,
    path: &Path,
    required: &[String],
    tree_root: &Path,
) -> anyhow::Result<Page> {
    let text = read_text_in(root, relative, MAX_PAGE_BYTES, "wiki page")?;
    let (fields, sources, body_start) = parse_frontmatter(path, &text)?;
    for key in required {
        anyhow::ensure!(
            fields.contains_key(key),
            "wiki page {} is missing frontmatter {key}",
            path.display()
        );
    }
    let title = fields.get("title").cloned().ok_or_else(|| {
        anyhow::anyhow!("wiki page {} is missing frontmatter title", path.display())
    })?;
    anyhow::ensure!(
        relative.to_str().is_some(),
        "wiki page path is not valid UTF-8: {}",
        path.display()
    );
    let structured = if fields.contains_key("input_sha256") {
        let generated_field = |key: &str| {
            fields.get(key).cloned().ok_or_else(|| {
                anyhow::anyhow!(
                    "generated wiki page {} is missing frontmatter {key}",
                    path.display()
                )
            })
        };
        let kind = generated_field("kind")?;
        let canonical = parse_canonical_metadata(&fields, path)?;
        let draft = parse_draft_metadata(&fields, path)?;
        anyhow::ensure!(
            canonical.is_none() || draft.is_none(),
            "generated wiki page {} cannot be both canonical and a draft",
            path.display()
        );
        if draft.is_some() {
            anyhow::ensure!(
                matches!(kind.as_str(), "source" | "community" | "topic"),
                "generated wiki page {} kind {kind} must not be draft",
                path.display()
            );
        }
        Some(StructuredPage {
            kind,
            graph_ref: generated_field("graph_ref")?,
            parent: generated_field("parent")?,
            input_sha256: generated_field("input_sha256")?,
            frontmatter_sha256: canonical_frontmatter_sha256(&text[..body_start]),
            draft,
            canonical,
            tree_root: tree_root.to_path_buf(),
            body: text[body_start..].to_owned(),
        })
    } else {
        None
    };
    Ok(Page {
        path: relative.to_path_buf(),
        title,
        sources,
        structured,
    })
}

fn parse_canonical_metadata(
    fields: &BTreeMap<String, String>,
    path: &Path,
) -> anyhow::Result<Option<CanonicalMetadata>> {
    let canonical_keys = [
        "article_type",
        "domain",
        "summary",
        "coverage",
        "review_status",
        "related",
        "aliases",
    ];
    if canonical_keys.iter().all(|key| !fields.contains_key(*key)) {
        return Ok(None);
    }
    for key in canonical_keys {
        anyhow::ensure!(
            fields.contains_key(key),
            "canonical wiki page {} is missing frontmatter {key}",
            path.display()
        );
    }
    let article_type = fields["article_type"].clone();
    anyhow::ensure!(
        matches!(
            article_type.as_str(),
            "overview"
                | "concept"
                | "component"
                | "interface"
                | "behavior"
                | "procedure"
                | "reference"
        ),
        "canonical wiki page {} has unsupported article_type {article_type:?}",
        path.display()
    );
    let coverage = fields["coverage"].clone();
    anyhow::ensure!(
        matches!(coverage.as_str(), "complete" | "partial" | "inventory-only"),
        "canonical wiki page {} has unsupported coverage {coverage:?}",
        path.display()
    );
    anyhow::ensure!(
        matches!(
            fields["review_status"].as_str(),
            "generated" | "reviewed" | "stale" | "archived"
        ),
        "canonical wiki page {} has unsupported review_status {:?}",
        path.display(),
        fields["review_status"]
    );
    Ok(Some(CanonicalMetadata {
        article_type,
        domain: fields["domain"].clone(),
        coverage,
        review_status: fields["review_status"].clone(),
    }))
}

fn parse_draft_metadata(
    fields: &BTreeMap<String, String>,
    path: &Path,
) -> anyhow::Result<Option<DraftMetadata>> {
    let draft = fields.get("draft");
    let model = fields.get("draft_model");
    let evidence_sha256 = fields.get("evidence_sha256");
    if draft.is_none() && model.is_none() && evidence_sha256.is_none() {
        return Ok(None);
    }
    let (Some(draft), Some(model), Some(evidence_sha256)) = (draft, model, evidence_sha256) else {
        anyhow::bail!(
            "generated wiki page {} has partial draft metadata",
            path.display()
        );
    };
    anyhow::ensure!(
        draft == "true",
        "generated wiki page {} draft must be true",
        path.display()
    );
    let metadata = DraftMetadata {
        model: model.clone(),
        evidence_sha256: evidence_sha256.clone(),
    };
    anyhow::ensure!(
        !metadata.model.is_empty()
            && metadata.model.len() <= 256
            && metadata.model.trim() == metadata.model
            && !metadata.model.chars().any(char::is_control),
        "generated wiki page {} has invalid draft_model",
        path.display()
    );
    anyhow::ensure!(
        metadata.evidence_sha256.len() == 64
            && metadata
                .evidence_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
        "generated wiki page {} has invalid evidence_sha256",
        path.display()
    );
    Ok(Some(metadata))
}

fn read_text_in(
    root: &OutputDirectory,
    path: &Path,
    limit: u64,
    kind: &str,
) -> anyhow::Result<String> {
    let bytes = root
        .read_bounded_regular(
            path,
            usize::try_from(limit).context("convert wiki byte limit")?,
        )
        .with_context(|| format!("read {kind} {}", path.display()))?;
    String::from_utf8(bytes).with_context(|| format!("{kind} {} is not UTF-8", path.display()))
}

fn read_output(
    root: &OutputDirectory,
    path: &Path,
    expected_bytes: usize,
) -> anyhow::Result<String> {
    let limit = expected_bytes
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("generated wiki output size limit overflow"))?;
    let bytes = root
        .read_bounded_regular(path, limit)
        .with_context(|| format!("read generated wiki output {}", path.display()))?;
    String::from_utf8(bytes)
        .with_context(|| format!("generated wiki output {} is not UTF-8", path.display()))
}

/// A verified directory descriptor used for the certified secure wiki
/// publication paths (Linux x86_64 and macOS).
///
/// All creation and replacement below is relative to this descriptor, so a
/// later rename of an ancestor cannot redirect output outside the directory
/// that was opened without following links.
pub(crate) struct OutputDirectory {
    #[cfg(any(all(target_os = "linux", target_arch = "x86_64"), target_os = "macos"))]
    file: fs::File,
}

pub(crate) fn require_secure_publication_support() -> anyhow::Result<()> {
    #[cfg(any(all(target_os = "linux", target_arch = "x86_64"), target_os = "macos"))]
    {
        Ok(())
    }
    #[cfg(not(any(all(target_os = "linux", target_arch = "x86_64"), target_os = "macos")))]
    anyhow::bail!("secure wiki publication is only supported on Linux x86_64 and macOS")
}

impl OutputDirectory {
    pub(crate) fn open_existing(path: &Path) -> anyhow::Result<Self> {
        #[cfg(any(all(target_os = "linux", target_arch = "x86_64"), target_os = "macos"))]
        {
            anyhow::ensure!(path.is_absolute(), "output directory must be absolute");
            if path == Path::new("/") {
                return Ok(Self {
                    file: open_directory_nofollow(path)?,
                });
            }
            // Component-wise no-follow walk from the root. Symlinked
            // components are rejected; on macOS the sole exception is a
            // symlink directly under the filesystem root (a stable OS
            // mapping such as /var -> /private/var - only the OS can
            // create root-level symlinks), which is resolved and the walk
            // continues against the real target.
            let root = CString::new("/").expect("root path contains no NUL");
            // SAFETY: the root C string remains valid for the call.
            let fd = unsafe {
                libc::open(
                    root.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            // SAFETY: open returned an owned descriptor above.
            let mut directory = unsafe { fs::File::from_raw_fd(fd) };
            let components = path.components().collect::<Vec<_>>();
            let mut position = 0;
            while position < components.len() {
                let component = components[position];
                match component {
                    Component::RootDir => position += 1,
                    Component::Normal(name) => {
                        let name_c = c_name(name)?;
                        match open_directory_at(directory.as_raw_fd(), &name_c) {
                            Ok(next) => {
                                directory = next;
                                position += 1;
                            }
                            Err(error) => {
                                #[cfg(target_os = "macos")]
                                {
                                    // Probe the failing component without
                                    // following it.
                                    let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
                                    let status = unsafe {
                                        libc::fstatat(
                                            directory.as_raw_fd(),
                                            name_c.as_ptr(),
                                            &mut stat,
                                            libc::AT_SYMLINK_NOFOLLOW,
                                        )
                                    };
                                    if status == 0
                                        && (stat.st_mode & libc::S_IFMT) == libc::S_IFLNK
                                        && position == 1
                                    {
                                        let rebased = fs::canonicalize(Path::new("/").join(name))
                                            .with_context(|| {
                                            format!("resolve output directory {}", path.display())
                                        })?;
                                        directory = open_directory_nofollow(&rebased)?;
                                        position += 1;
                                        continue;
                                    }
                                }
                                return Err(error);
                            }
                        }
                    }
                    _ => anyhow::bail!("unsafe output directory path"),
                }
            }
            Ok(Self { file: directory })
        }
        #[cfg(not(any(all(target_os = "linux", target_arch = "x86_64"), target_os = "macos")))]
        {
            let _ = path;
            anyhow::bail!("secure wiki publication is only supported on Linux x86_64 and macOS")
        }
    }

    pub(crate) fn open_or_create(&self, name: &OsStr) -> anyhow::Result<Self> {
        #[cfg(any(all(target_os = "linux", target_arch = "x86_64"), target_os = "macos"))]
        {
            let name = c_name(name)?;
            // SAFETY: the descriptor and C string remain valid for the call.
            let status = unsafe { libc::mkdirat(self.file.as_raw_fd(), name.as_ptr(), 0o777) };
            if status != 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(error.into());
                }
            }
            Ok(Self {
                file: open_directory_at(self.file.as_raw_fd(), &name)?,
            })
        }
        #[cfg(not(any(all(target_os = "linux", target_arch = "x86_64"), target_os = "macos")))]
        {
            let _ = name;
            anyhow::bail!("secure wiki publication is only supported on Linux x86_64 and macOS")
        }
    }

    fn duplicate(&self) -> anyhow::Result<Self> {
        #[cfg(any(all(target_os = "linux", target_arch = "x86_64"), target_os = "macos"))]
        {
            // SAFETY: fcntl receives a valid descriptor and returns a new owned descriptor.
            let fd = unsafe { libc::fcntl(self.file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
            if fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            // SAFETY: fcntl returned an owned descriptor above.
            Ok(Self {
                file: unsafe { fs::File::from_raw_fd(fd) },
            })
        }
        #[cfg(not(any(all(target_os = "linux", target_arch = "x86_64"), target_os = "macos")))]
        anyhow::bail!("secure wiki publication is only supported on Linux x86_64 and macOS")
    }

    pub(crate) fn entry_exists(&self, name: &OsStr) -> anyhow::Result<bool> {
        #[cfg(any(all(target_os = "linux", target_arch = "x86_64"), target_os = "macos"))]
        {
            let name = c_name(name)?;
            // SAFETY: zeroed stat is immediately written by fstatat on success.
            let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
            // SAFETY: the descriptor, C string, and stat output remain valid for the call.
            let status = unsafe {
                libc::fstatat(
                    self.file.as_raw_fd(),
                    name.as_ptr(),
                    &mut stat,
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            };
            if status == 0 {
                Ok(true)
            } else {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::NotFound {
                    Ok(false)
                } else {
                    Err(error.into())
                }
            }
        }
        #[cfg(not(any(all(target_os = "linux", target_arch = "x86_64"), target_os = "macos")))]
        {
            let _ = name;
            anyhow::bail!("secure wiki publication is only supported on Linux x86_64 and macOS")
        }
    }

    #[cfg(any(all(target_os = "linux", target_arch = "x86_64"), target_os = "macos"))]
    fn regular_file_mode(&self, name: &OsStr) -> anyhow::Result<Option<u32>> {
        #[cfg(any(all(target_os = "linux", target_arch = "x86_64"), target_os = "macos"))]
        {
            let name = c_name(name)?;
            // SAFETY: zeroed stat is immediately written by fstatat on success.
            let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
            // SAFETY: the descriptor, C string, and stat output remain valid for the call.
            let status = unsafe {
                libc::fstatat(
                    self.file.as_raw_fd(),
                    name.as_ptr(),
                    &mut stat,
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            };
            if status == 0 {
                anyhow::ensure!(
                    stat.st_mode & libc::S_IFMT == libc::S_IFREG,
                    "refusing non-file wiki output"
                );
                // `mode_t` is u32 on Linux and u16 on macOS; widen on the
                // platforms that need it (a uniform conversion is a clippy
                // useless_conversion on Linux).
                #[cfg(target_os = "macos")]
                let mode: u32 = u32::from(stat.st_mode) & 0o777;
                #[cfg(not(target_os = "macos"))]
                let mode: u32 = stat.st_mode & 0o777;
                Ok(Some(mode))
            } else {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::NotFound {
                    Ok(None)
                } else {
                    Err(error.into())
                }
            }
        }
        #[cfg(not(any(all(target_os = "linux", target_arch = "x86_64"), target_os = "macos")))]
        {
            let _ = name;
            anyhow::bail!("secure wiki publication is only supported on Linux x86_64 and macOS")
        }
    }

    fn create_new_file(&self, name: &OsStr) -> anyhow::Result<fs::File> {
        #[cfg(any(all(target_os = "linux", target_arch = "x86_64"), target_os = "macos"))]
        {
            let name = c_name(name)?;
            // SAFETY: the descriptor and C string remain valid for the call.
            let fd = unsafe {
                libc::openat(
                    self.file.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_WRONLY
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC,
                    0o666,
                )
            };
            if fd < 0 {
                Err(std::io::Error::last_os_error().into())
            } else {
                // SAFETY: openat returned an owned descriptor above.
                Ok(unsafe { fs::File::from_raw_fd(fd) })
            }
        }
        #[cfg(not(any(all(target_os = "linux", target_arch = "x86_64"), target_os = "macos")))]
        {
            let _ = name;
            anyhow::bail!("secure wiki publication is only supported on Linux x86_64 and macOS")
        }
    }

    fn open_file_if_exists(&self, name: &OsStr) -> anyhow::Result<Option<fs::File>> {
        #[cfg(any(all(target_os = "linux", target_arch = "x86_64"), target_os = "macos"))]
        {
            let name = c_name(name)?;
            // SAFETY: the descriptor and C string remain valid for the call.
            let fd = unsafe {
                libc::openat(
                    self.file.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
                )
            };
            if fd >= 0 {
                // SAFETY: openat returned an owned descriptor above.
                Ok(Some(unsafe { fs::File::from_raw_fd(fd) }))
            } else {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::NotFound {
                    Ok(None)
                } else {
                    Err(error.into())
                }
            }
        }
        #[cfg(not(any(all(target_os = "linux", target_arch = "x86_64"), target_os = "macos")))]
        {
            let _ = name;
            anyhow::bail!("secure wiki publication is only supported on Linux x86_64 and macOS")
        }
    }

    /// Read a bounded regular file through this directory descriptor.
    ///
    /// The parent and leaf are resolved relative to the descriptor, so a
    /// later replacement of a path ancestor cannot redirect the read.
    pub(crate) fn read_bounded_regular(&self, path: &Path, cap: usize) -> anyhow::Result<Vec<u8>> {
        #[cfg(any(all(target_os = "linux", target_arch = "x86_64"), target_os = "macos"))]
        {
            let (parent, name) = output_parent_if_existing(self, path)?
                .context("required wiki input parent disappeared")?;
            let mut file = parent
                .open_file_if_exists(&name)?
                .context("required wiki input disappeared")?;
            let initial = ensure_regular_file(&file, cap)?;
            let mut bytes = Vec::new();
            std::io::Read::by_ref(&mut file)
                .take((cap as u64).saturating_add(1))
                .read_to_end(&mut bytes)?;
            anyhow::ensure!(bytes.len() <= cap, "wiki input exceeds its byte cap");
            ensure_regular_file(&file, cap)?;
            let final_file = parent
                .open_file_if_exists(&name)?
                .context("wiki input disappeared during bounded read")?;
            let final_metadata = ensure_regular_file(&final_file, cap)?;
            anyhow::ensure!(
                same_file_identity(&initial, &final_metadata),
                "wiki input changed during bounded read"
            );
            Ok(bytes)
        }
        #[cfg(not(any(all(target_os = "linux", target_arch = "x86_64"), target_os = "macos")))]
        {
            let _ = (path, cap);
            anyhow::bail!("secure wiki publication is only supported on Linux x86_64 and macOS")
        }
    }

    /// Hash a bounded regular file while retaining only a bounded prefix.
    ///
    /// This preserves the descriptor-relative, no-follow read and TOCTOU
    /// checks used by `read_bounded_regular`, while callers can send a small
    /// text prefix only after verifying the complete source digest.
    pub(crate) fn read_prefix_and_sha256_regular(
        &self,
        path: &Path,
        cap: usize,
        prefix_cap: usize,
    ) -> anyhow::Result<(Vec<u8>, String)> {
        #[cfg(any(all(target_os = "linux", target_arch = "x86_64"), target_os = "macos"))]
        {
            let (parent, name) = output_parent_if_existing(self, path)?
                .context("required wiki input parent disappeared")?;
            let mut file = parent
                .open_file_if_exists(&name)?
                .context("required wiki input disappeared")?;
            let initial = ensure_regular_file(&file, cap)?;
            let mut prefix = Vec::with_capacity(prefix_cap.min(cap));
            let mut digest = Sha256::new();
            let mut bytes_read = 0_usize;
            let mut block = [0_u8; 64 * 1024];
            loop {
                let count = file.read(&mut block)?;
                if count == 0 {
                    break;
                }
                bytes_read = bytes_read
                    .checked_add(count)
                    .context("wiki input size overflow")?;
                anyhow::ensure!(bytes_read <= cap, "wiki input exceeds its byte cap");
                digest.update(&block[..count]);
                if prefix.len() < prefix_cap {
                    let remaining = prefix_cap - prefix.len();
                    prefix.extend_from_slice(&block[..count.min(remaining)]);
                }
            }
            ensure_regular_file(&file, cap)?;
            let final_file = parent
                .open_file_if_exists(&name)?
                .context("wiki input disappeared during bounded read")?;
            let final_metadata = ensure_regular_file(&final_file, cap)?;
            anyhow::ensure!(
                same_file_identity(&initial, &final_metadata),
                "wiki input changed during bounded read"
            );
            Ok((prefix, hex::encode(digest.finalize())))
        }
        #[cfg(not(any(all(target_os = "linux", target_arch = "x86_64"), target_os = "macos")))]
        {
            let _ = (path, cap, prefix_cap);
            anyhow::bail!("secure wiki publication is only supported on Linux x86_64 and macOS")
        }
    }

    pub(crate) fn write_new_text(&self, name: &OsStr, text: &str) -> anyhow::Result<()> {
        let mut file = self.create_new_file(name)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        Ok(())
    }

    pub(crate) fn create_directory(&self, name: &OsStr) -> anyhow::Result<Self> {
        self.open_or_create_new(name)
    }

    fn open_or_create_new(&self, name: &OsStr) -> anyhow::Result<Self> {
        #[cfg(any(all(target_os = "linux", target_arch = "x86_64"), target_os = "macos"))]
        {
            let name = c_name(name)?;
            // SAFETY: the descriptor and C string remain valid for the call.
            let status = unsafe { libc::mkdirat(self.file.as_raw_fd(), name.as_ptr(), 0o777) };
            if status != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(Self {
                file: open_directory_at(self.file.as_raw_fd(), &name)?,
            })
        }
        #[cfg(not(any(all(target_os = "linux", target_arch = "x86_64"), target_os = "macos")))]
        {
            let _ = name;
            anyhow::bail!("secure wiki publication is only supported on Linux x86_64 and macOS")
        }
    }

    /// Atomically publish `source` as `destination` without replacing an
    /// existing entry.
    ///
    /// Linux x86_64 uses the certified `renameat2(RENAME_NOREPLACE)`
    /// operation. macOS has no no-replace rename, so the publish reserves
    /// the destination name with an exclusive regular file, renames the
    /// source over that reservation (an atomic name replacement that works
    /// for files and directories), then re-checks dev/ino to prove the
    /// destination still names the published entry. A reservation or
    /// identity failure restores the pre-publish state and fails closed.
    pub(crate) fn rename_noreplace(
        &self,
        source: &OsStr,
        destination: &OsStr,
    ) -> anyhow::Result<()> {
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        {
            let source = c_name(source)?;
            let destination = c_name(destination)?;
            // SAFETY: both descriptors and C strings remain valid for the syscall.
            let status = unsafe {
                libc::syscall(
                    libc::SYS_renameat2,
                    self.file.as_raw_fd(),
                    source.as_ptr(),
                    self.file.as_raw_fd(),
                    destination.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            };
            if status == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error().into())
            }
        }
        #[cfg(target_os = "macos")]
        {
            let source = c_name(source)?;
            let destination = c_name(destination)?;
            // Record the source identity before publishing so the
            // post-publish re-check can prove the destination still names
            // the entry we moved.
            // SAFETY: zeroed stat is immediately written by fstatat on success.
            let mut source_stat = unsafe { std::mem::zeroed::<libc::stat>() };
            // SAFETY: the descriptor, C string, and stat output remain valid
            // for the call.
            let status = unsafe {
                libc::fstatat(
                    self.file.as_raw_fd(),
                    source.as_ptr(),
                    &mut source_stat,
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            };
            if status != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let source_type = source_stat.st_mode & libc::S_IFMT;
            if source_type != libc::S_IFREG && source_type != libc::S_IFDIR {
                anyhow::bail!("refusing non-regular wiki publication source");
            }
            // Reserve the destination name: this fails with EEXIST when any
            // entry already occupies the name, preserving no-clobber. The
            // reservation must match the source type because macOS refuses
            // to rename a directory over a file (ENOTDIR).
            let reservation_is_dir = source_type == libc::S_IFDIR;
            let mut reservation_file = None;
            if reservation_is_dir {
                // SAFETY: the descriptor and C string remain valid for the call.
                let status =
                    unsafe { libc::mkdirat(self.file.as_raw_fd(), destination.as_ptr(), 0o777) };
                if status != 0 {
                    return Err(std::io::Error::last_os_error().into());
                }
            } else {
                // SAFETY: the descriptor and C string remain valid for the call.
                let reservation = unsafe {
                    libc::openat(
                        self.file.as_raw_fd(),
                        destination.as_ptr(),
                        libc::O_WRONLY
                            | libc::O_CREAT
                            | libc::O_EXCL
                            | libc::O_NOFOLLOW
                            | libc::O_CLOEXEC,
                        0o666,
                    )
                };
                if reservation < 0 {
                    return Err(std::io::Error::last_os_error().into());
                }
                // SAFETY: openat returned an owned descriptor above.
                reservation_file = Some(unsafe { fs::File::from_raw_fd(reservation) });
            }
            let result = (|| {
                // SAFETY: the descriptor and C strings remain valid for the call.
                let status = unsafe {
                    libc::renameat(
                        self.file.as_raw_fd(),
                        source.as_ptr(),
                        self.file.as_raw_fd(),
                        destination.as_ptr(),
                    )
                };
                if status != 0 {
                    return Err(std::io::Error::last_os_error().into());
                }
                // TOCTOU re-check: between the rename and this point a local
                // attacker could have replaced the destination name. It must
                // still name the exact entry we published.
                // SAFETY: zeroed stat is immediately written by fstatat on success.
                let mut final_stat = unsafe { std::mem::zeroed::<libc::stat>() };
                // SAFETY: the descriptor, C string, and stat output remain
                // valid for the call.
                let status = unsafe {
                    libc::fstatat(
                        self.file.as_raw_fd(),
                        destination.as_ptr(),
                        &mut final_stat,
                        libc::AT_SYMLINK_NOFOLLOW,
                    )
                };
                if status != 0 {
                    return Err(std::io::Error::last_os_error().into());
                }
                if final_stat.st_dev != source_stat.st_dev
                    || final_stat.st_ino != source_stat.st_ino
                    || final_stat.st_mode & libc::S_IFMT != source_type
                {
                    // Remove the foreign entry so the destination is absent
                    // again, then fail closed.
                    let _ =
                        unsafe { libc::unlinkat(self.file.as_raw_fd(), destination.as_ptr(), 0) };
                    let _ = unsafe {
                        libc::unlinkat(
                            self.file.as_raw_fd(),
                            destination.as_ptr(),
                            libc::AT_REMOVEDIR,
                        )
                    };
                    anyhow::bail!("wiki output destination changed during publication");
                }
                Ok(())
            })();
            drop(reservation_file);
            if let Err(error) = result {
                // Restore the pre-publish state (destination absent) when
                // the rename could not complete.
                let flags = if reservation_is_dir {
                    libc::AT_REMOVEDIR
                } else {
                    0
                };
                let _ =
                    unsafe { libc::unlinkat(self.file.as_raw_fd(), destination.as_ptr(), flags) };
                Err(error)
            } else {
                Ok(())
            }
        }
        #[cfg(not(any(all(target_os = "linux", target_arch = "x86_64"), target_os = "macos")))]
        {
            let _ = (source, destination);
            anyhow::bail!("secure wiki publication is only supported on Linux x86_64 and macOS")
        }
    }

    pub(crate) fn sync(&self) -> anyhow::Result<()> {
        #[cfg(any(all(target_os = "linux", target_arch = "x86_64"), target_os = "macos"))]
        {
            if let Err(error) = self.file.sync_all()
                && !matches!(error.raw_os_error(), Some(code) if code == libc::ENOTSUP || code == libc::EINVAL)
            {
                Err(error.into())
            } else {
                Ok(())
            }
        }
        #[cfg(not(any(all(target_os = "linux", target_arch = "x86_64"), target_os = "macos")))]
        anyhow::bail!("secure wiki publication is only supported on Linux x86_64 and macOS")
    }
}

/// Open a live wiki root without following links, creating only its final
/// component beneath an already-existing parent. Complete release trees still
/// use the staged directory publisher.
pub(crate) fn open_or_create_output_root(path: &Path) -> anyhow::Result<OutputDirectory> {
    require_secure_publication_support()?;
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let name = path
        .file_name()
        .filter(|name| !name.is_empty())
        .context("wiki output root must have a final path component")?;
    let parent = path
        .parent()
        .context("wiki output root must have an existing parent")?
        .canonicalize()
        .context("resolve wiki output parent")?;
    OutputDirectory::open_existing(&parent)?.open_or_create(name)
}

#[cfg(any(all(target_os = "linux", target_arch = "x86_64"), target_os = "macos"))]
fn ensure_regular_file(file: &fs::File, cap: usize) -> anyhow::Result<fs::Metadata> {
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.file_type().is_file() && metadata.nlink() == 1,
        "refusing unsafe non-regular wiki input"
    );
    anyhow::ensure!(
        metadata.len() <= cap as u64,
        "wiki input exceeds its byte cap"
    );
    Ok(metadata)
}

#[cfg(any(all(target_os = "linux", target_arch = "x86_64"), target_os = "macos"))]
fn same_file_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.dev() == right.dev()
        && left.ino() == right.ino()
        && left.len() == right.len()
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
}

pub(crate) fn output_parent(
    root: &OutputDirectory,
    output: &Path,
) -> anyhow::Result<(OutputDirectory, OsString)> {
    let name = output
        .file_name()
        .filter(|name| !name.is_empty())
        .context("wiki output must have a final path component")?
        .to_os_string();
    let mut parent = root.duplicate()?;
    for component in output.parent().into_iter().flat_map(Path::components) {
        let Component::Normal(name) = component else {
            anyhow::bail!("wiki output has an unsafe parent path");
        };
        parent = parent.open_or_create(name)?;
    }
    Ok((parent, name))
}

fn output_parent_if_existing(
    root: &OutputDirectory,
    output: &Path,
) -> anyhow::Result<Option<(OutputDirectory, OsString)>> {
    let name = output
        .file_name()
        .filter(|name| !name.is_empty())
        .context("wiki output must have a final path component")?
        .to_os_string();
    #[cfg(any(all(target_os = "linux", target_arch = "x86_64"), target_os = "macos"))]
    {
        let mut parent = root.duplicate()?;
        for component in output.parent().into_iter().flat_map(Path::components) {
            let Component::Normal(name) = component else {
                anyhow::bail!("wiki output has an unsafe parent path");
            };
            match open_directory_at(parent.file.as_raw_fd(), &c_name(name)?) {
                Ok(opened) => parent = OutputDirectory { file: opened },
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound) =>
                {
                    return Ok(None);
                }
                Err(error) => return Err(error),
            }
        }
        Ok(Some((parent, name)))
    }
    #[cfg(not(any(all(target_os = "linux", target_arch = "x86_64"), target_os = "macos")))]
    {
        let _ = (root, output, name);
        anyhow::bail!("secure wiki publication is only supported on Linux x86_64 and macOS")
    }
}

pub(crate) fn write_text_atomic_in(
    parent: &OutputDirectory,
    name: impl AsRef<OsStr>,
    text: &str,
) -> anyhow::Result<()> {
    let name = name.as_ref();
    #[cfg(any(all(target_os = "linux", target_arch = "x86_64"), target_os = "macos"))]
    {
        let existing_mode = parent.regular_file_mode(name)?;
        let mut temporary = None;
        let mut file = None;
        for _ in 0..128 {
            let candidate = OsString::from(format!(
                ".wiki-output-{}-{}",
                std::process::id(),
                OUTPUT_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            match parent.create_new_file(&candidate) {
                Ok(opened) => {
                    temporary = Some(candidate);
                    file = Some(opened);
                    break;
                }
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|io| io.kind() == std::io::ErrorKind::AlreadyExists) =>
                {
                    continue
                }
                Err(error) => return Err(error),
            }
        }
        let temporary = temporary.context("could not reserve wiki output temporary file")?;
        let mut file = file.expect("temporary file and handle are created together");
        let result = (|| -> anyhow::Result<()> {
            if let Some(mode) = existing_mode {
                file.set_permissions(fs::Permissions::from_mode(mode))?;
            }
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            drop(file);
            if existing_mode.is_some() {
                replace_file_in(parent, &temporary, name)?;
            } else {
                parent.rename_noreplace(&temporary, name)?;
            }
            parent.sync()
        })();
        if result.is_err() {
            let _ = unlink_at(parent, &temporary);
        }
        result
    }
    #[cfg(not(any(all(target_os = "linux", target_arch = "x86_64"), target_os = "macos")))]
    {
        let _ = (parent, name, text);
        anyhow::bail!("secure wiki publication is only supported on Linux x86_64 and macOS")
    }
}

/// Atomically publish a small, new artifact without replacing an existing one.
pub(crate) fn write_new_text_atomic_in(
    parent: &OutputDirectory,
    name: impl AsRef<OsStr>,
    text: &str,
) -> anyhow::Result<()> {
    let name = name.as_ref();
    #[cfg(any(all(target_os = "linux", target_arch = "x86_64"), target_os = "macos"))]
    {
        anyhow::ensure!(!parent.entry_exists(name)?, "wiki output already exists");
        let mut temporary = None;
        let mut file = None;
        for _ in 0..128 {
            let candidate = OsString::from(format!(
                ".wiki-output-{}-{}",
                std::process::id(),
                OUTPUT_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            match parent.create_new_file(&candidate) {
                Ok(opened) => {
                    temporary = Some(candidate);
                    file = Some(opened);
                    break;
                }
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|io| io.kind() == std::io::ErrorKind::AlreadyExists) =>
                {
                    continue
                }
                Err(error) => return Err(error),
            }
        }
        let temporary = temporary.context("could not reserve wiki output temporary file")?;
        let mut file = file.expect("temporary file and handle are created together");
        let result = (|| -> anyhow::Result<()> {
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            drop(file);
            parent.rename_noreplace(&temporary, name)?;
            parent.sync()
        })();
        if result.is_err() {
            let _ = unlink_at(parent, &temporary);
        }
        result
    }
    #[cfg(not(any(all(target_os = "linux", target_arch = "x86_64"), target_os = "macos")))]
    {
        let _ = (parent, name, text);
        anyhow::bail!("secure wiki publication is only supported on Linux x86_64 and macOS")
    }
}

/// Remove one manifest-owned regular file through the pinned output directory.
pub(crate) fn remove_regular_file_in(root: &OutputDirectory, path: &Path) -> anyhow::Result<()> {
    let Some((parent, name)) = output_parent_if_existing(root, path)? else {
        anyhow::bail!("managed wiki output disappeared")
    };
    #[cfg(any(all(target_os = "linux", target_arch = "x86_64"), target_os = "macos"))]
    {
        anyhow::ensure!(
            parent.regular_file_mode(&name)?.is_some(),
            "refusing to remove a missing or non-file wiki output"
        );
        unlink_at(&parent, &name)?;
        parent.sync()
    }
    #[cfg(not(any(all(target_os = "linux", target_arch = "x86_64"), target_os = "macos")))]
    {
        let _ = (parent, name);
        anyhow::bail!("secure wiki publication is only supported on Linux x86_64 and macOS")
    }
}

#[cfg(any(all(target_os = "linux", target_arch = "x86_64"), target_os = "macos"))]
fn c_name(name: &OsStr) -> anyhow::Result<CString> {
    let bytes = name.as_bytes();
    anyhow::ensure!(
        !bytes.is_empty() && !bytes.contains(&b'/'),
        "unsafe output entry name"
    );
    CString::new(bytes).context("output entry name contains NUL")
}

#[cfg(any(all(target_os = "linux", target_arch = "x86_64"), target_os = "macos"))]
fn open_directory_nofollow(path: &Path) -> anyhow::Result<fs::File> {
    anyhow::ensure!(path.is_absolute(), "output directory must be absolute");
    let root = CString::new("/").expect("root path contains no NUL");
    // SAFETY: the root C string remains valid for the call.
    let fd = unsafe {
        libc::open(
            root.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: open returned an owned descriptor above.
    let mut directory = unsafe { fs::File::from_raw_fd(fd) };
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                directory = open_directory_at(directory.as_raw_fd(), &c_name(name)?)?
            }
            _ => anyhow::bail!("unsafe output directory path"),
        }
    }
    Ok(directory)
}

#[cfg(any(all(target_os = "linux", target_arch = "x86_64"), target_os = "macos"))]
fn open_directory_at(parent: std::os::fd::RawFd, name: &CString) -> anyhow::Result<fs::File> {
    // SAFETY: the descriptor and C string remain valid for the call.
    let fd = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: openat returned an owned descriptor above.
    Ok(unsafe { fs::File::from_raw_fd(fd) })
}

#[cfg(any(all(target_os = "linux", target_arch = "x86_64"), target_os = "macos"))]
fn replace_file_in(
    parent: &OutputDirectory,
    source: &OsStr,
    destination: &OsStr,
) -> anyhow::Result<()> {
    let source = c_name(source)?;
    let destination = c_name(destination)?;
    // SAFETY: the descriptor and C strings remain valid for the call.
    if unsafe {
        libc::renameat(
            parent.file.as_raw_fd(),
            source.as_ptr(),
            parent.file.as_raw_fd(),
            destination.as_ptr(),
        )
    } == 0
    {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().into())
    }
}

#[cfg(any(all(target_os = "linux", target_arch = "x86_64"), target_os = "macos"))]
fn unlink_at(parent: &OutputDirectory, name: &OsStr) -> anyhow::Result<()> {
    let name = c_name(name)?;
    // SAFETY: the descriptor and C string remain valid for the call.
    if unsafe { libc::unlinkat(parent.file.as_raw_fd(), name.as_ptr(), 0) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().into())
    }
}

pub(crate) fn parse_frontmatter(
    path: &Path,
    text: &str,
) -> anyhow::Result<(BTreeMap<String, String>, Vec<String>, usize)> {
    let mut lines = text.split_inclusive('\n');
    let opening = lines.next().unwrap_or_default();
    anyhow::ensure!(
        frontmatter_line(opening) == "---",
        "wiki page {} has malformed frontmatter",
        path.display()
    );
    let mut fields = BTreeMap::new();
    let mut sources = Vec::new();
    let mut seen_list_values = BTreeMap::<String, BTreeSet<String>>::new();
    let mut list_key = None::<String>;
    let mut bytes = opening.len();
    let mut closed = false;
    for raw_line in lines {
        bytes = bytes.saturating_add(raw_line.len());
        anyhow::ensure!(
            bytes <= MAX_FRONTMATTER_BYTES,
            "wiki page {} frontmatter exceeds {MAX_FRONTMATTER_BYTES}-byte limit",
            path.display()
        );
        let line = frontmatter_line(raw_line);
        if line == "---" {
            closed = true;
            break;
        }
        if let Some(value) = line.strip_prefix("  - ") {
            let key = list_key.as_deref().with_context(|| {
                format!("wiki page {} has malformed frontmatter", path.display())
            })?;
            anyhow::ensure!(
                !value.trim().is_empty(),
                "wiki page {} has malformed frontmatter",
                path.display()
            );
            let value = parse_scalar(value, path, key)?;
            if key == "sources" {
                validate_source(&value, path)?;
                sources.push(value.clone());
            }
            anyhow::ensure!(
                seen_list_values
                    .entry(key.to_owned())
                    .or_default()
                    .insert(value.clone()),
                "wiki page {} has duplicate {key} entry {value:?}",
                path.display()
            );
            continue;
        }
        let (key, value) = line.split_once(':').ok_or_else(|| {
            anyhow::anyhow!("wiki page {} has malformed frontmatter", path.display())
        })?;
        anyhow::ensure!(
            valid_key(key),
            "wiki page {} has invalid frontmatter key {key:?}",
            path.display()
        );
        anyhow::ensure!(
            !fields.contains_key(key),
            "wiki page {} has duplicate frontmatter key {key:?}",
            path.display()
        );
        if matches!(key, "sources" | "related" | "aliases") {
            anyhow::ensure!(
                value.trim().is_empty() || value.trim() == "[]",
                "wiki page {} {key} must be a list",
                path.display()
            );
            list_key = value.trim().is_empty().then(|| key.to_owned());
            fields.insert(key.to_owned(), String::new());
        } else {
            fields.insert(key.to_owned(), parse_scalar(value, path, key)?);
            list_key = None;
        }
    }
    anyhow::ensure!(
        closed,
        "wiki page {} has malformed frontmatter",
        path.display()
    );
    Ok((fields, sources, bytes))
}

fn parse_scalar(value: &str, path: &Path, key: &str) -> anyhow::Result<String> {
    let value = value.trim();
    anyhow::ensure!(
        !value.is_empty(),
        "wiki page {} has empty frontmatter {key}",
        path.display()
    );
    let scalar = if value.starts_with('"') {
        serde_json::from_str(value).with_context(|| {
            format!(
                "wiki page {} has malformed quoted frontmatter {key}",
                path.display()
            )
        })?
    } else {
        value.to_owned()
    };
    anyhow::ensure!(
        !scalar.is_empty() && !scalar.chars().any(char::is_control),
        "wiki page {} has invalid frontmatter {key}",
        path.display()
    );
    Ok(scalar)
}

fn frontmatter_line(line: &str) -> &str {
    let line = line.strip_suffix('\n').unwrap_or(line);
    line.strip_suffix('\r').unwrap_or(line)
}

fn canonical_frontmatter_sha256(frontmatter: &str) -> String {
    let canonical = frontmatter
        .split_inclusive('\n')
        .filter(|line| {
            let line = frontmatter_line(line);
            !line.starts_with("review_status:") && !line.starts_with("publication_state:")
        })
        .collect::<String>();
    hex::encode(Sha256::digest(canonical.as_bytes()))
}

fn valid_key(value: &str) -> bool {
    let mut chars = value.chars();
    chars
        .next()
        .is_some_and(|character| character.is_ascii_alphabetic())
        && chars
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
}

fn validate_source(value: &str, path: &Path) -> anyhow::Result<()> {
    let Some((source, capture)) = value.split_once('#') else {
        anyhow::bail!(
            "wiki page {} has invalid source reference {value:?}",
            path.display()
        );
    };
    anyhow::ensure!(
        !source.is_empty()
            && !capture.is_empty()
            && !capture.contains('#')
            && valid_source_identifier(source)
            && valid_source_identifier(capture),
        "wiki page {} has invalid source reference {value:?}",
        path.display()
    );
    Ok(())
}

fn valid_source_identifier(value: &str) -> bool {
    let mut characters = value.chars();
    characters
        .next()
        .is_some_and(|character| character.is_ascii_alphanumeric())
        && characters.all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
}

fn validate_structured_pages(
    root: &OutputDirectory,
    pages: &[Page],
) -> anyhow::Result<Option<Vec<(usize, usize)>>> {
    let structured_count = pages
        .iter()
        .filter(|page| page.structured.is_some())
        .count();
    if structured_count == 0 {
        return Ok(None);
    }
    anyhow::ensure!(
        structured_count == pages.len(),
        "generated wiki hierarchy cannot mix structured and manual pages"
    );

    let canonical_count = pages
        .iter()
        .filter(|page| {
            page.structured
                .as_ref()
                .and_then(|structured| structured.canonical.as_ref())
                .is_some()
        })
        .count();
    if canonical_count != 0 {
        anyhow::ensure!(
            canonical_count == pages.len(),
            "generated wiki hierarchy cannot mix canonical and legacy pages"
        );
        return validate_canonical_pages(root, pages).map(Some);
    }

    let generated = pages
        .iter()
        .map(|page| {
            let structured = page.structured.as_ref().expect("checked above");
            GeneratedWikiPage {
                path: &page.path,
                kind: &structured.kind,
                graph_ref: &structured.graph_ref,
                parent: &structured.parent,
                input_sha256: &structured.input_sha256,
                tree_root: &structured.tree_root,
                body: &structured.body,
            }
        })
        .collect::<Vec<_>>();
    for page in pages {
        validate_draft_model_body(page)?;
    }
    let hierarchy = validate_generated_pages(&generated)?;
    for page in pages {
        validate_local_links(root, page)?;
    }
    Ok(Some(hierarchy))
}

fn validate_canonical_pages(
    root: &OutputDirectory,
    pages: &[Page],
) -> anyhow::Result<Vec<(usize, usize)>> {
    let by_path = pages
        .iter()
        .enumerate()
        .map(|(index, page)| (page.path.as_path(), index))
        .collect::<BTreeMap<_, _>>();
    let generated = pages
        .iter()
        .map(|page| {
            let structured = page
                .structured
                .as_ref()
                .expect("canonical pages are structured");
            GeneratedWikiPage {
                path: &page.path,
                kind: &structured.kind,
                graph_ref: &structured.graph_ref,
                parent: &structured.parent,
                input_sha256: &structured.input_sha256,
                tree_root: &structured.tree_root,
                body: &structured.body,
            }
        })
        .collect::<Vec<_>>();
    validate_generated_page_targets(&generated)?;

    let mut roots = Vec::new();
    let mut parents = BTreeMap::new();
    let mut children = BTreeMap::<usize, Vec<usize>>::new();
    for (index, page) in pages.iter().enumerate() {
        let structured = page
            .structured
            .as_ref()
            .expect("canonical pages are structured");
        let canonical = structured.canonical.as_ref().expect("canonical metadata");
        anyhow::ensure!(
            structured.input_sha256.len() == 64
                && structured
                    .input_sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
            "canonical wiki page {} has invalid input_sha256",
            page.path.display()
        );
        let title_h1 = structured
            .body
            .strip_prefix("\r\n")
            .or_else(|| structured.body.strip_prefix('\n'))
            .unwrap_or(&structured.body);
        anyhow::ensure!(
            title_h1.starts_with(&format!("# {}\n", page.title)),
            "canonical wiki page {} does not start with its exact title H1",
            page.path.display()
        );
        anyhow::ensure!(
            matches!(
                structured.kind.as_str(),
                "root"
                    | "domain"
                    | "article"
                    | "topic"
                    | "community"
                    | "source"
                    | "reference"
                    | "inventory"
            ),
            "canonical wiki page {} has unsupported kind {:?}",
            page.path.display(),
            structured.kind
        );
        anyhow::ensure!(
            !canonical.domain.is_empty()
                && !canonical.article_type.is_empty()
                && !canonical.coverage.is_empty(),
            "canonical wiki page {} has invalid metadata",
            page.path.display()
        );
        validate_local_links(root, page)?;
        if structured.kind == "root" {
            anyhow::ensure!(
                page.path == structured.tree_root.join("index.md")
                    && structured.graph_ref == "root"
                    && structured.parent == "root"
                    && canonical.domain == "root",
                "canonical wiki root has an invalid identity"
            );
            roots.push(index);
            continue;
        }
        validate_safe_relative(&structured.parent, "canonical wiki parent")?;
        let parent_path = structured.tree_root.join(&structured.parent);
        let parent_index = *by_path.get(parent_path.as_path()).with_context(|| {
            format!(
                "canonical wiki page {} references missing parent {}",
                page.path.display(),
                parent_path.display()
            )
        })?;
        let parent = pages[parent_index]
            .structured
            .as_ref()
            .expect("canonical parent is structured");
        let is_agents = page.path == structured.tree_root.join("AGENTS.md");
        let valid_parent = match structured.kind.as_str() {
            "domain" => parent.kind == "root",
            "article" => parent.kind == "domain" && canonical.domain == parent.graph_ref,
            "topic" => parent.kind == "root",
            "community" => parent.kind == "topic",
            "source" => parent.kind == "root",
            "reference" => {
                (is_agents && parent.kind == "root") || (!is_agents && parent.kind == "source")
            }
            "inventory" => parent.kind == "source" && parent.graph_ref == structured.graph_ref,
            "root" => false,
            _ => false,
        };
        anyhow::ensure!(
            valid_parent,
            "canonical wiki page {} has invalid parent kind {}",
            page.path.display(),
            parent.kind
        );
        if matches!(
            structured.kind.as_str(),
            "article" | "community" | "source" | "inventory"
        ) || (structured.kind == "reference" && !is_agents)
        {
            anyhow::ensure!(
                !page.sources.is_empty(),
                "canonical wiki page {} has no evidence citations",
                page.path.display()
            );
        }
        parents.insert(index, parent_index);
        children.entry(parent_index).or_default().push(index);
    }
    anyhow::ensure!(
        roots.len() == 1,
        "canonical wiki hierarchy requires exactly one root page"
    );
    let root_index = roots[0];
    for index in 0..pages.len() {
        let mut current = index;
        let mut seen = BTreeSet::new();
        while current != root_index {
            anyhow::ensure!(
                seen.insert(current),
                "canonical wiki hierarchy contains a cycle at {}",
                pages[current].path.display()
            );
            current = *parents.get(&current).with_context(|| {
                format!(
                    "canonical wiki page {} is not reachable from the root",
                    pages[current].path.display()
                )
            })?;
        }
    }
    for child_indexes in children.values_mut() {
        child_indexes.sort_by(|left, right| pages[*left].path.cmp(&pages[*right].path));
    }
    let mut ordered = Vec::with_capacity(pages.len());
    append_hierarchy(root_index, 0, &children, &mut ordered);
    Ok(ordered)
}

fn validate_canonical_render_matches(
    pages: &[Page],
    expected: &graphoxide_export::StructuredWikiPlan,
) -> anyhow::Result<()> {
    let reference_evidence = canonical_reference_evidence(&expected.pages)?;
    let actual = pages
        .iter()
        .map(|page| {
            let structured = page
                .structured
                .as_ref()
                .expect("canonical pages are structured");
            let path = page
                .path
                .strip_prefix(&structured.tree_root)
                .context("canonical wiki page escaped its tree root")?;
            Ok((path, page))
        })
        .collect::<anyhow::Result<BTreeMap<_, _>>>()?;
    let expected_paths = expected
        .pages
        .iter()
        .map(|page| page.path.as_str())
        .collect::<BTreeSet<_>>();
    anyhow::ensure!(
        actual.len() == expected_paths.len(),
        "canonical wiki page set does not match the reviewed plan render"
    );
    for expected_page in &expected.pages {
        let page = actual
            .get(Path::new(&expected_page.path))
            .with_context(|| format!("canonical wiki is missing page {}", expected_page.path))?;
        let structured = page.structured.as_ref().expect("canonical page structure");
        let (fields, sources, body_start) =
            parse_frontmatter(Path::new(&expected_page.path), &expected_page.markdown)?;
        let expected_field = |key: &str| {
            fields.get(key).with_context(|| {
                format!(
                    "reviewed canonical render page {} lacks frontmatter {key}",
                    expected_page.path
                )
            })
        };
        anyhow::ensure!(
            page.title == *expected_field("title")?
                && structured.kind == *expected_field("kind")?
                && structured.graph_ref == *expected_field("graph_ref")?
                && structured.parent == *expected_field("parent")?
                && structured.input_sha256 == *expected_field("input_sha256")?
                && structured.frontmatter_sha256
                    == canonical_frontmatter_sha256(&expected_page.markdown[..body_start])
                && page.sources == sources,
            "canonical wiki page {} no longer matches the reviewed plan render",
            expected_page.path
        );
        let expected_body = &expected_page.markdown[body_start..];
        let actual_body = structured.body.as_str();
        if actual_body != expected_body {
            anyhow::ensure!(
                structured.kind == "article"
                    && canonical_draft_matches(
                        actual_body,
                        expected_body,
                        &page.sources,
                        &reference_evidence,
                    )?,
                "canonical wiki page {} body no longer matches the reviewed plan render",
                expected_page.path
            );
        }
    }
    Ok(())
}

/// Verify that optional model prose changes only canonical article bodies and
/// remains fully bound to the deterministic reference evidence.
pub(crate) fn validate_canonical_draft_overlays(
    actual: &[graphoxide_export::StructuredWikiPage],
    expected: &[graphoxide_export::StructuredWikiPage],
) -> anyhow::Result<BTreeSet<String>> {
    let reference_evidence = canonical_reference_evidence(expected)?;
    anyhow::ensure!(
        actual.len() == expected.len(),
        "canonical draft overlay page set does not match the reviewed render"
    );
    let mut actual_by_path = BTreeMap::new();
    for page in actual {
        anyhow::ensure!(
            actual_by_path.insert(page.path.as_str(), page).is_none(),
            "canonical draft overlay contains a duplicate page"
        );
    }
    let mut drafted = BTreeSet::new();
    for expected_page in expected {
        let actual_page = actual_by_path
            .get(expected_page.path.as_str())
            .with_context(|| {
                format!(
                    "canonical draft overlay is missing page {}",
                    expected_page.path
                )
            })?;
        let (_, expected_sources, expected_body_start) =
            parse_frontmatter(Path::new(&expected_page.path), &expected_page.markdown)?;
        let (actual_fields, actual_sources, actual_body_start) =
            parse_frontmatter(Path::new(&actual_page.path), &actual_page.markdown)?;
        anyhow::ensure!(
            canonical_frontmatter_sha256(&actual_page.markdown[..actual_body_start])
                == canonical_frontmatter_sha256(&expected_page.markdown[..expected_body_start])
                && actual_sources == expected_sources,
            "canonical draft overlay page {} changed deterministic frontmatter",
            expected_page.path
        );
        let expected_body = &expected_page.markdown[expected_body_start..];
        let actual_body = &actual_page.markdown[actual_body_start..];
        if actual_body != expected_body {
            anyhow::ensure!(
                actual_fields.get("kind").map(String::as_str) == Some("article")
                    && canonical_draft_matches(
                        actual_body,
                        expected_body,
                        &actual_sources,
                        &reference_evidence,
                    )?,
                "canonical draft overlay page {} does not contain an evidence-bound article draft",
                expected_page.path
            );
            drafted.insert(expected_page.path.clone());
        }
    }
    Ok(drafted)
}

fn canonical_reference_evidence(
    pages: &[graphoxide_export::StructuredWikiPage],
) -> anyhow::Result<BTreeMap<String, BTreeSet<String>>> {
    let mut evidence = BTreeMap::<String, BTreeSet<String>>::new();
    for page in pages {
        let (fields, citations, _) = parse_frontmatter(Path::new(&page.path), &page.markdown)?;
        if fields.get("kind").map(String::as_str) != Some("reference") {
            continue;
        }
        for id in page
            .markdown
            .lines()
            .filter_map(|line| line.strip_prefix("- Evidence block: `"))
            .filter_map(|id| id.strip_suffix('`'))
        {
            for citation in &citations {
                evidence
                    .entry(citation.clone())
                    .or_default()
                    .insert(id.to_owned());
            }
        }
    }
    Ok(evidence)
}

fn canonical_draft_matches(
    actual: &str,
    expected: &str,
    citations: &[String],
    reference_evidence: &BTreeMap<String, BTreeSet<String>>,
) -> anyhow::Result<bool> {
    let Some((prefix, expected_tail)) = canonical_title_prefix(expected) else {
        return Ok(false);
    };
    let Some(after_title) = actual.strip_prefix(prefix) else {
        return Ok(false);
    };
    let Some(after_title) = after_title.strip_prefix('\n') else {
        return Ok(false);
    };
    let Some((marker, after_marker)) = after_title.split_once('\n') else {
        return Ok(false);
    };
    let Some(checksum) = marker
        .strip_prefix("<!-- graphoxide-draft sha256=")
        .and_then(|value| value.strip_suffix(" -->"))
    else {
        return Ok(false);
    };
    if checksum.len() != 64
        || !checksum
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Ok(false);
    }
    let Some(draft) = after_marker.strip_suffix(expected_tail) else {
        return Ok(false);
    };
    if hex::encode(Sha256::digest(draft.as_bytes())) != checksum {
        return Ok(false);
    }
    let allowed = citations
        .iter()
        .filter_map(|citation| reference_evidence.get(citation))
        .flat_map(|ids| ids.iter().map(String::as_str))
        .collect::<BTreeSet<_>>();
    anyhow::ensure!(
        !allowed.is_empty(),
        "canonical article draft has no source evidence"
    );
    validate_canonical_draft_insertion(draft, &allowed)?;
    Ok(true)
}

fn canonical_title_prefix(body: &str) -> Option<(&str, &str)> {
    let first = body.find("# ")?;
    if !body[..first].trim().is_empty() {
        return None;
    }
    let end = body[first..].find('\n')? + first + 1;
    Some((&body[..end], &body[end..]))
}

fn validate_canonical_draft_insertion(draft: &str, allowed: &BTreeSet<&str>) -> anyhow::Result<()> {
    let draft = draft.trim();
    anyhow::ensure!(!draft.is_empty(), "canonical article draft is empty");
    let mut sections = 0_usize;
    let mut headings = BTreeSet::new();
    let mut cited = BTreeSet::new();
    let mut section_body = String::new();
    let mut section_open = false;
    for line in draft.lines() {
        if let Some(heading) = line.strip_prefix("## ") {
            let heading = heading.trim();
            anyhow::ensure!(
                !heading.is_empty()
                    && !heading.eq_ignore_ascii_case("sources")
                    && heading.len() <= 200
                    && !heading.contains('#')
                    && !heading.chars().any(char::is_control)
                    && !section_open
                    && headings.insert(heading),
                "canonical article draft has an invalid section boundary"
            );
            sections += 1;
            anyhow::ensure!(
                sections <= MAX_CANONICAL_DRAFT_SECTIONS,
                "canonical article draft has too many sections"
            );
            section_open = true;
            continue;
        }
        if let Some(ids) = line.strip_prefix("Evidence blocks: ") {
            anyhow::ensure!(
                section_open && !section_body.trim().is_empty(),
                "canonical article draft lacks a body"
            );
            validate_model_markdown_body(&section_body)?;
            anyhow::ensure!(
                !has_model_heading(&section_body),
                "canonical article draft model body contains a heading"
            );
            for id in ids.split(", ") {
                let id = id.strip_prefix('`').and_then(|id| id.strip_suffix('`'));
                let id = id.context("canonical article draft has malformed evidence IDs")?;
                anyhow::ensure!(
                    allowed.contains(id),
                    "canonical article draft cites unknown evidence"
                );
                anyhow::ensure!(cited.insert(id), "canonical article draft repeats evidence");
            }
            section_body.clear();
            section_open = false;
            continue;
        }
        anyhow::ensure!(
            section_open,
            "canonical article draft has content outside a section"
        );
        section_body.push_str(line);
        section_body.push('\n');
    }
    anyhow::ensure!(
        sections != 0 && !section_open && !cited.is_empty(),
        "canonical article draft is incomplete"
    );
    Ok(())
}

fn has_model_heading(body: &str) -> bool {
    let active = active_markdown(body);
    let mut previous = None;
    for line in active.lines() {
        if atx_heading(line).is_some()
            || previous.is_some_and(|title| setext_heading(title, line).is_some())
        {
            return true;
        }
        previous = Some(line);
    }
    false
}

/// Validate a complete generated page set without reading or writing files.
///
/// Callers that read from disk must additionally use descriptor-relative I/O
/// to prove each local link target is a regular, non-symlinked file.
pub(crate) fn validate_generated_pages(
    pages: &[GeneratedWikiPage<'_>],
) -> anyhow::Result<Vec<(usize, usize)>> {
    let by_path = pages
        .iter()
        .enumerate()
        .map(|(index, page)| (page.path.to_path_buf(), index))
        .collect::<BTreeMap<_, _>>();
    let mut identities = BTreeSet::new();
    let mut sources = BTreeMap::new();
    let mut inventories = BTreeMap::new();
    let mut roots = Vec::new();
    let mut parents = BTreeMap::new();
    let mut children = BTreeMap::<usize, Vec<usize>>::new();
    for (index, page) in pages.iter().enumerate() {
        let identity = match page.kind {
            "root" => {
                anyhow::ensure!(
                    page.graph_ref == "root" && page.parent == "root",
                    "generated wiki root {} has an invalid hierarchy identity",
                    page.path.display()
                );
                anyhow::ensure!(
                    page.path == page.tree_root.join("index.md"),
                    "generated wiki root must be index.md"
                );
                roots.push(index);
                Some("root")
            }
            "topic" => Some("topic"),
            "community" => {
                page.graph_ref.parse::<i64>().with_context(|| {
                    format!(
                        "generated wiki community {} has invalid graph_ref",
                        page.path.display()
                    )
                })?;
                Some("community")
            }
            "source" => {
                anyhow::ensure!(
                    sources.insert(page.graph_ref, index).is_none(),
                    "generated wiki hierarchy has duplicate source placement {}",
                    page.graph_ref
                );
                None
            }
            "inventory" => {
                anyhow::ensure!(
                    inventories.insert(page.graph_ref, index).is_none(),
                    "generated wiki hierarchy has duplicate inventory placement {}",
                    page.graph_ref
                );
                None
            }
            kind => anyhow::bail!(
                "generated wiki page {} has invalid kind {kind:?}",
                page.path.display()
            ),
        };
        if let Some(identity_kind) = identity {
            anyhow::ensure!(
                identities.insert((identity_kind, page.graph_ref)),
                "generated wiki hierarchy has duplicate {identity_kind} placement {}",
                page.graph_ref
            );
        }
        anyhow::ensure!(
            page.input_sha256.len() == 64
                && page
                    .input_sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
            "generated wiki page {} has invalid input_sha256",
            page.path.display()
        );
        validate_generated_local_links(page)?;
    }
    anyhow::ensure!(
        roots.len() == 1,
        "generated wiki hierarchy requires exactly one root page"
    );
    let root_index = roots[0];

    for (index, page) in pages.iter().enumerate() {
        if index == root_index {
            continue;
        }
        validate_safe_relative(page.parent, "wiki parent")?;
        let parent_path = page.tree_root.join(page.parent);
        let parent_index = *by_path.get(&parent_path).with_context(|| {
            format!(
                "generated wiki page {} references missing parent {}",
                page.path.display(),
                parent_path.display()
            )
        })?;
        let parent = &pages[parent_index];
        let valid_parent = match page.kind {
            "topic" => parent.kind == "root",
            "community" => parent.kind == "topic",
            "source" => matches!(parent.kind, "root" | "community"),
            "inventory" => parent.kind == "source" && parent.graph_ref == page.graph_ref,
            "root" => false,
            _ => unreachable!("kind checked above"),
        };
        anyhow::ensure!(
            valid_parent,
            "generated wiki page {} has invalid parent kind {}",
            page.path.display(),
            parent.kind
        );
        parents.insert(index, parent_index);
        children.entry(parent_index).or_default().push(index);
    }
    for source in inventories.keys() {
        anyhow::ensure!(
            sources.contains_key(source),
            "generated wiki inventory {} has no source page",
            source
        );
    }

    for index in 0..pages.len() {
        let mut current = index;
        let mut path = BTreeSet::new();
        while current != root_index {
            anyhow::ensure!(
                path.insert(current),
                "generated wiki hierarchy contains a cycle at {}",
                pages[current].path.display()
            );
            current = *parents.get(&current).with_context(|| {
                format!(
                    "generated wiki page {} is not reachable from the root",
                    pages[current].path.display()
                )
            })?;
        }
    }

    for child_indexes in children.values_mut() {
        child_indexes.sort_by(|left, right| pages[*left].path.cmp(pages[*right].path));
    }
    let mut ordered = Vec::with_capacity(pages.len());
    append_hierarchy(root_index, 0, &children, &mut ordered);
    anyhow::ensure!(
        ordered.len() == pages.len(),
        "generated wiki hierarchy is not fully reachable from the root"
    );
    Ok(ordered)
}

fn append_hierarchy(
    page: usize,
    depth: usize,
    children: &BTreeMap<usize, Vec<usize>>,
    ordered: &mut Vec<(usize, usize)>,
) {
    ordered.push((page, depth));
    if let Some(child_indexes) = children.get(&page) {
        for child in child_indexes {
            append_hierarchy(*child, depth + 1, children, ordered);
        }
    }
}

fn validate_generated_local_links(page: &GeneratedWikiPage<'_>) -> anyhow::Result<()> {
    for destination in markdown_link_destinations(page.body) {
        let _ = local_link_target(page.path, &destination)?;
    }
    Ok(())
}

/// Require every local generated-page link to resolve within this in-memory
/// page set. Draft output has no independent assets to publish.
pub(crate) fn validate_generated_page_targets(
    pages: &[GeneratedWikiPage<'_>],
) -> anyhow::Result<()> {
    let by_path = pages
        .iter()
        .map(|page| page.path.to_path_buf())
        .collect::<BTreeSet<_>>();
    for page in pages {
        for destination in markdown_link_destinations(page.body) {
            let Some(target) = local_link_target(page.path, &destination)? else {
                continue;
            };
            anyhow::ensure!(
                by_path.contains(&target),
                "wiki link from {} has missing target {destination:?}",
                page.path.display()
            );
        }
    }
    Ok(())
}

fn validate_local_links(root: &OutputDirectory, page: &Page) -> anyhow::Result<()> {
    let structured = page.structured.as_ref().expect("structured page");
    for destination in markdown_link_destinations(&structured.body) {
        let Some(target) = local_link_target(&page.path, &destination)? else {
            continue;
        };
        let (parent, name) = output_parent_if_existing(root, &target)
            .with_context(|| {
                format!(
                    "validate wiki link from {} to {destination:?}",
                    page.path.display()
                )
            })?
            .with_context(|| {
                format!(
                    "wiki link from {} has missing target {destination:?}",
                    page.path.display()
                )
            })?;
        let file = parent
            .open_file_if_exists(&name)
            .with_context(|| {
                format!(
                    "validate wiki link from {} to {destination:?}",
                    page.path.display()
                )
            })?
            .with_context(|| {
                format!(
                    "wiki link from {} has missing target {destination:?}",
                    page.path.display()
                )
            })?;
        anyhow::ensure!(
            file.metadata()?.file_type().is_file(),
            "wiki link from {} has non-regular target {destination:?}",
            page.path.display()
        );
    }
    Ok(())
}

fn markdown_link_destinations(text: &str) -> Vec<String> {
    inline_link_destinations(&active_markdown(text))
}

fn active_markdown(text: &str) -> String {
    let mut active = String::with_capacity(text.len());
    let mut fence = None;
    for chunk in text.split_inclusive('\n') {
        let newline = chunk.ends_with('\n');
        let line = chunk.strip_suffix('\n').unwrap_or(chunk);
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some((marker, minimum)) = fence {
            if markdown_fence(line).is_some_and(|(current, count, suffix)| {
                current == marker && count >= minimum && suffix.trim().is_empty()
            }) {
                fence = None;
            }
        } else if let Some((marker, count, _)) = markdown_fence(line) {
            fence = Some((marker, count));
        } else {
            active.push_str(&without_inline_code(line));
        }
        if newline {
            active.push('\n');
        }
    }
    active
}

fn closing_delimiter(text: &str, opening: usize, open: u8, close: u8) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 1usize;
    let mut cursor = opening.checked_add(1)?;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\\' => cursor = cursor.saturating_add(2),
            byte if byte == open => {
                depth = depth.checked_add(1)?;
                cursor += 1;
            }
            byte if byte == close => {
                depth -= 1;
                if depth == 0 {
                    return Some(cursor);
                }
                cursor += 1;
            }
            _ => cursor += 1,
        }
    }
    None
}

fn inline_link_destinations(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut destinations = Vec::new();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        if bytes[cursor] == b'\\' {
            cursor = cursor.saturating_add(2);
            continue;
        }
        if bytes[cursor] != b'[' {
            cursor += 1;
            continue;
        }
        let Some(label_end) = closing_delimiter(text, cursor, b'[', b']') else {
            cursor += 1;
            continue;
        };
        let mut destination_start = label_end + 1;
        while destination_start < bytes.len() && bytes[destination_start].is_ascii_whitespace() {
            destination_start += 1;
        }
        if bytes.get(destination_start) != Some(&b'(') {
            cursor += 1;
            continue;
        }
        let Some(destination_end) = closing_delimiter(text, destination_start, b'(', b')') else {
            cursor += 1;
            continue;
        };
        let contents = text[destination_start + 1..destination_end].trim();
        let destination = if let Some(angle) = contents.strip_prefix('<') {
            angle.split_once('>').map_or(angle, |(value, _)| value)
        } else {
            contents.split_ascii_whitespace().next().unwrap_or("")
        };
        destinations.push(destination.to_owned());
        cursor = destination_end + 1;
    }
    destinations
}

fn has_reference_link(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        if bytes[cursor] == b'\\' {
            cursor = cursor.saturating_add(2);
            continue;
        }
        if bytes[cursor] != b'[' {
            cursor += 1;
            continue;
        }
        let Some(label_end) = closing_delimiter(text, cursor, b'[', b']') else {
            cursor += 1;
            continue;
        };
        let mut reference_start = label_end + 1;
        while reference_start < bytes.len() && bytes[reference_start].is_ascii_whitespace() {
            reference_start += 1;
        }
        if bytes.get(reference_start) == Some(&b'[')
            && closing_delimiter(text, reference_start, b'[', b']').is_some()
        {
            return true;
        }
        cursor += 1;
    }
    false
}

fn has_reference_definition(text: &str) -> bool {
    text.lines().any(|line| {
        let indent = line.bytes().take_while(|byte| *byte == b' ').count();
        if indent > 3 || line.as_bytes().get(indent) != Some(&b'[') {
            return false;
        }
        closing_delimiter(line, indent, b'[', b']')
            .and_then(|closing| line.as_bytes().get(closing + 1))
            == Some(&b':')
    })
}

pub(crate) fn validate_model_markdown_body(body: &str) -> anyhow::Result<()> {
    let active = active_markdown(body);
    let unsafe_angle = active.char_indices().any(|(index, character)| {
        character == '<'
            && active[index + 1..]
                .chars()
                .next()
                .is_some_and(|next| next.is_ascii_alphabetic() || matches!(next, '/' | '!' | '?'))
    });
    let unsafe_heading = has_unsafe_model_heading(&active);
    anyhow::ensure!(
        inline_link_destinations(&active).is_empty()
            && !has_reference_link(&active)
            && !has_reference_definition(&active)
            && !unsafe_angle
            && !unsafe_heading,
        "invalid model Markdown body: links and raw HTML are not allowed"
    );
    Ok(())
}

fn has_unsafe_model_heading(active: &str) -> bool {
    let mut previous = None;
    for line in active.lines() {
        if atx_heading(line)
            .is_some_and(|(level, title)| level == 1 || title.eq_ignore_ascii_case("sources"))
            || previous.is_some_and(|title| {
                setext_heading(title, line).is_some_and(|(level, title)| {
                    level == 1 || title.eq_ignore_ascii_case("sources")
                })
            })
        {
            return true;
        }
        previous = Some(line);
    }
    false
}

fn validate_draft_model_body(page: &Page) -> anyhow::Result<()> {
    let structured = page.structured.as_ref().expect("structured page");
    let marker = "<!-- graphoxide-draft -->";
    let marker_count = structured.body.matches(marker).count();
    let legacy = structured.draft.is_none() && structured.kind == "community" && marker_count != 0;
    if structured.draft.is_none() && !legacy {
        return Ok(());
    }
    anyhow::ensure!(
        marker_count == 1,
        "generated wiki page {} has an invalid draft marker",
        page.path.display()
    );
    let (_, after_marker) = structured
        .body
        .split_once(marker)
        .expect("one marker checked");
    let sources = after_marker.rfind("\n## Sources\n").with_context(|| {
        format!(
            "generated wiki page {} lacks its draft Sources boundary",
            page.path.display()
        )
    })?;
    let model_body = after_marker[..sources].trim();
    anyhow::ensure!(
        !model_body.is_empty(),
        "generated wiki page {} has an empty model body",
        page.path.display()
    );
    validate_model_markdown_body(model_body)
        .with_context(|| format!("validate model body in {}", page.path.display()))
}

fn without_inline_code(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut visible = String::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        let Some(relative) = line[cursor..].find('`') else {
            visible.push_str(&line[cursor..]);
            break;
        };
        let opening = cursor + relative;
        visible.push_str(&line[cursor..opening]);
        let count = bytes[opening..]
            .iter()
            .take_while(|byte| **byte == b'`')
            .count();
        let mut search = opening + count;
        let mut closing = None;
        while search < bytes.len() {
            let Some(relative) = line[search..].find('`') else {
                break;
            };
            let candidate = search + relative;
            let candidate_count = bytes[candidate..]
                .iter()
                .take_while(|byte| **byte == b'`')
                .count();
            if candidate_count == count {
                closing = Some(candidate + count);
                break;
            }
            search = candidate + candidate_count;
        }
        if let Some(closing) = closing {
            cursor = closing;
        } else {
            visible.push_str(&line[opening..]);
            break;
        }
    }
    visible
}

fn markdown_fence(line: &str) -> Option<(u8, usize, &str)> {
    let indent = line
        .as_bytes()
        .iter()
        .take_while(|byte| **byte == b' ')
        .count();
    if indent > 3 {
        return None;
    }
    let line = &line[indent..];
    let marker = *line.as_bytes().first()?;
    if !matches!(marker, b'`' | b'~') {
        return None;
    }
    let count = line
        .as_bytes()
        .iter()
        .take_while(|byte| **byte == marker)
        .count();
    (count >= 3).then_some((marker, count, &line[count..]))
}

fn atx_heading(line: &str) -> Option<(usize, &str)> {
    let indent = line
        .as_bytes()
        .iter()
        .take_while(|byte| **byte == b' ')
        .count();
    if indent > 3 {
        return None;
    }
    let line = &line[indent..];
    let level = line
        .as_bytes()
        .iter()
        .take_while(|byte| **byte == b'#')
        .count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = &line[level..];
    if rest
        .chars()
        .next()
        .is_some_and(|character| !character.is_whitespace())
    {
        return None;
    }
    Some((level, rest.trim().trim_end_matches('#').trim_end()))
}

fn setext_heading<'a>(title: &'a str, underline: &str) -> Option<(usize, &'a str)> {
    let title = title.trim();
    if title.is_empty() {
        return None;
    }
    let indent = underline
        .as_bytes()
        .iter()
        .take_while(|byte| **byte == b' ')
        .count();
    if indent > 3 {
        return None;
    }
    let underline = &underline[indent..];
    let marker = *underline.as_bytes().first()?;
    if !matches!(marker, b'=' | b'-') || underline.is_empty() {
        return None;
    }
    let marker_count = underline
        .as_bytes()
        .iter()
        .take_while(|byte| **byte == marker)
        .count();
    underline[marker_count..]
        .trim()
        .is_empty()
        .then_some((usize::from(marker == b'-') + 1, title))
}

fn local_link_target(page: &Path, destination: &str) -> anyhow::Result<Option<PathBuf>> {
    let destination = destination.trim();
    if destination.is_empty() || destination.starts_with('#') {
        return Ok(None);
    }
    let bytes = destination.as_bytes();
    anyhow::ensure!(
        !destination.starts_with("//"),
        "unsafe wiki link {destination:?}"
    );
    anyhow::ensure!(
        bytes.get(1) != Some(&b':') && !destination.starts_with('/') && !destination.contains('\\'),
        "unsafe wiki link {destination:?}"
    );
    if let Some(scheme) = uri_scheme(destination) {
        anyhow::ensure!(
            scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https"),
            "unsafe wiki link {destination:?}"
        );
        return Ok(None);
    }
    let path = destination
        .split('#')
        .next()
        .unwrap_or_default()
        .split('?')
        .next()
        .unwrap_or_default();
    if path.is_empty() {
        return Ok(None);
    }
    let path = percent_decode(path)?;
    anyhow::ensure!(
        uri_scheme(&path).is_none(),
        "unsafe wiki link {destination:?}"
    );
    let bytes = path.as_bytes();
    anyhow::ensure!(
        bytes.get(1) != Some(&b':')
            && !path.starts_with('/')
            && !path.contains('\\')
            && !path.chars().any(char::is_control),
        "unsafe wiki link {destination:?}"
    );
    let mut resolved = page.parent().unwrap_or_else(|| Path::new("")).to_path_buf();
    for component in Path::new(&path).components() {
        match component {
            Component::Normal(component) => resolved.push(component),
            Component::CurDir => {}
            Component::ParentDir => anyhow::ensure!(
                resolved.pop(),
                "unsafe wiki link {destination:?} escapes the wiki root"
            ),
            Component::RootDir | Component::Prefix(_) => {
                anyhow::bail!("unsafe wiki link {destination:?}")
            }
        }
    }
    anyhow::ensure!(
        !resolved.as_os_str().is_empty(),
        "unsafe wiki link {destination:?}"
    );
    Ok(Some(resolved))
}

fn uri_scheme(value: &str) -> Option<&str> {
    let (scheme, _) = value.split_once(':')?;
    let mut bytes = scheme.bytes();
    (bytes.next().is_some_and(|byte| byte.is_ascii_alphabetic())
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.')))
    .then_some(scheme)
}

fn percent_decode(value: &str) -> anyhow::Result<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            anyhow::ensure!(index + 2 < bytes.len(), "invalid percent-encoded wiki link");
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3])?;
            decoded.push(u8::from_str_radix(hex, 16).context("invalid percent-encoded wiki link")?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).context("wiki link target is not UTF-8")
}

fn validate_graph_coverage(
    pages: &[Page],
    graph: &graphoxide_core::KnowledgeGraph,
    active_annotations: Option<&BTreeMap<String, serde_json::Value>>,
) -> anyhow::Result<()> {
    if pages.iter().all(|page| page.structured.is_none()) {
        return Ok(());
    }
    if pages.iter().all(|page| {
        page.structured
            .as_ref()
            .and_then(|structured| structured.canonical.as_ref())
            .is_some()
    }) {
        return validate_canonical_graph_coverage(pages, graph, active_annotations);
    }
    if let Some(active_annotations) = active_annotations {
        return validate_catalog_graph_coverage(pages, graph, active_annotations);
    }
    let represented_communities = pages
        .iter()
        .filter_map(|page| {
            let structured = page.structured.as_ref()?;
            (structured.kind == "community").then(|| (structured.graph_ref.clone(), page))
        })
        .collect::<BTreeMap<_, _>>();
    let represented_sources = pages
        .iter()
        .filter_map(|page| {
            let structured = page.structured.as_ref()?;
            matches!(structured.kind.as_str(), "source" | "inventory")
                .then(|| (structured.graph_ref.clone(), page))
        })
        .collect::<BTreeMap<_, _>>();
    let mut graph_communities = BTreeSet::new();
    let mut graph_sources = BTreeMap::<String, BTreeSet<i64>>::new();
    for node in &graph.nodes {
        let Some(catalog) = node
            .extra
            .get("catalog")
            .and_then(serde_json::Value::as_object)
        else {
            continue;
        };
        if let Some(source) = catalog.get("source_id").and_then(serde_json::Value::as_str) {
            let communities = graph_sources.entry(source.to_owned()).or_default();
            if let Some(community) = node.community {
                graph_communities.insert(community.to_string());
                communities.insert(community);
            }
        }
    }
    for community in &graph_communities {
        anyhow::ensure!(
            represented_communities.contains_key(community),
            "generated wiki is missing graph community {community}"
        );
    }
    for community in represented_communities.keys() {
        anyhow::ensure!(
            graph_communities.contains(community),
            "generated wiki has unexpected graph community {community}"
        );
    }
    for source in graph_sources.keys() {
        anyhow::ensure!(
            represented_sources.contains_key(source),
            "generated wiki is missing graph source {source}"
        );
    }
    for source in represented_sources.keys() {
        anyhow::ensure!(
            graph_sources.contains_key(source),
            "generated wiki has unexpected graph source {source}"
        );
    }
    for (source, communities) in &graph_sources {
        let page = represented_sources
            .get(source)
            .expect("source presence checked above");
        let structured = page.structured.as_ref().expect("structured source page");
        anyhow::ensure!(
            structured.kind == "source",
            "generated wiki graph source {source} must use kind source"
        );
        let parent_path = structured.tree_root.join(&structured.parent);
        let parent = pages
            .iter()
            .find(|page| page.path == parent_path)
            .and_then(|page| page.structured.as_ref())
            .expect("source hierarchy validated before graph coverage");
        let correct_parent = if let Some(primary) = communities.iter().next() {
            parent.kind == "community" && parent.graph_ref == primary.to_string()
        } else {
            parent.kind == "root"
        };
        anyhow::ensure!(
            correct_parent,
            "generated wiki source {source} has wrong parent {}",
            structured.parent
        );
    }
    Ok(())
}

fn validate_canonical_graph_coverage(
    pages: &[Page],
    graph: &graphoxide_core::KnowledgeGraph,
    active_annotations: Option<&BTreeMap<String, serde_json::Value>>,
) -> anyhow::Result<()> {
    let Some(active_annotations) = active_annotations else {
        return Ok(());
    };
    let source_pages = pages
        .iter()
        .filter_map(|page| {
            let structured = page.structured.as_ref()?;
            (structured.kind == "source").then_some((structured.graph_ref.as_str(), page))
        })
        .collect::<BTreeMap<_, _>>();
    let active_citations = active_annotations
        .values()
        .map(|annotation| {
            let source = annotation
                .get("source_id")
                .and_then(serde_json::Value::as_str)
                .context("active catalog annotation lacks source_id")?;
            let capture = annotation
                .get("capture_id")
                .and_then(serde_json::Value::as_str)
                .context("active catalog annotation lacks capture_id")?;
            Ok(format!("{source}#{capture}"))
        })
        .collect::<anyhow::Result<BTreeSet<_>>>()?;
    for citation in &active_citations {
        let page = source_pages.get(citation.as_str()).with_context(|| {
            format!("canonical wiki is missing source page for active capture {citation}")
        })?;
        anyhow::ensure!(
            page.sources.len() == 1 && page.sources[0] == *citation,
            "canonical wiki source page {} has mismatched capture citation",
            page.path.display()
        );
    }
    for node in &graph.nodes {
        let Some(catalog) = node
            .extra
            .get("catalog")
            .and_then(serde_json::Value::as_object)
        else {
            continue;
        };
        let Some(source) = catalog.get("source_id").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let Some(capture) = catalog
            .get("capture_id")
            .and_then(serde_json::Value::as_str)
        else {
            continue;
        };
        let citation = format!("{source}#{capture}");
        if active_citations.contains(&citation) {
            anyhow::ensure!(
                source_pages.contains_key(citation.as_str()),
                "canonical wiki omitted active graph capture {citation}"
            );
        }
    }
    Ok(())
}

fn validate_catalog_graph_coverage(
    pages: &[Page],
    graph: &graphoxide_core::KnowledgeGraph,
    active_annotations: &BTreeMap<String, serde_json::Value>,
) -> anyhow::Result<()> {
    let topics = graphoxide_export::derive_topic_tree(graph)?;
    let plan =
        graphoxide_export::render_structured_wiki_with_catalog(graph, &topics, active_annotations)?;
    let mut expected = BTreeMap::new();
    for page in plan.pages {
        let (fields, sources, _) = parse_frontmatter(Path::new(&page.path), &page.markdown)?;
        let path = PathBuf::from(&page.path);
        let fields = {
            let required = |key: &str| {
                fields.get(key).cloned().with_context(|| {
                    format!(
                        "generated catalog wiki page {} is missing frontmatter {key}",
                        path.display()
                    )
                })
            };
            (
                required("title")?,
                required("kind")?,
                required("graph_ref")?,
                required("parent")?,
                required("input_sha256")?,
            )
        };
        expected.insert(
            path,
            (fields.0, fields.1, fields.2, fields.3, fields.4, sources),
        );
    }
    let community_ranks = graph
        .nodes
        .iter()
        .filter_map(|node| {
            let community = node.community?;
            let catalog = node.extra.get("catalog")?.as_object()?;
            let citation = format!(
                "{}#{}",
                catalog.get("source_id")?.as_str()?,
                catalog.get("capture_id")?.as_str()?
            );
            Some((community, citation))
        })
        .fold(
            BTreeMap::<i64, BTreeMap<String, usize>>::new(),
            |mut ranks, (community, citation)| {
                let rank = ranks
                    .entry(community)
                    .or_default()
                    .entry(citation)
                    .or_default();
                *rank = rank.saturating_add(1);
                ranks
            },
        );
    let topic_ranks = topics
        .topics
        .iter()
        .map(|topic| {
            let mut ranks = BTreeMap::<String, usize>::new();
            for community in &topic.communities {
                for (citation, rank) in community_ranks.get(community).into_iter().flatten() {
                    let total = ranks.entry(citation.clone()).or_default();
                    *total = total.saturating_add(*rank);
                }
            }
            (topic.id.clone(), ranks)
        })
        .collect::<BTreeMap<_, _>>();
    let actual = pages
        .iter()
        .map(|page| {
            let structured = page
                .structured
                .as_ref()
                .expect("structured pages checked above");
            let path = page
                .path
                .strip_prefix(&structured.tree_root)
                .context("generated wiki page escaped its tree root")?
                .to_path_buf();
            Ok((
                path,
                (page.title.as_str(), structured, page.sources.as_slice()),
            ))
        })
        .collect::<anyhow::Result<BTreeMap<_, _>>>()?;
    for path in expected.keys() {
        anyhow::ensure!(
            actual.contains_key(path),
            "generated wiki is missing catalog page {}",
            path.display()
        );
    }
    for path in actual.keys() {
        anyhow::ensure!(
            expected.contains_key(path),
            "generated wiki has unexpected catalog page {}",
            path.display()
        );
    }
    for (path, (title, kind, graph_ref, parent, input_sha256, sources)) in expected {
        let (actual_title, actual, actual_sources) = actual
            .get(&path)
            .expect("catalog page presence checked above");
        anyhow::ensure!(
            *actual_title == title,
            "generated wiki catalog page {} has stale title",
            path.display()
        );
        anyhow::ensure!(
            actual.kind == kind && actual.graph_ref == graph_ref && actual.parent == parent,
            "generated wiki catalog page {} has stale hierarchy metadata",
            path.display()
        );
        let legacy_community = kind == "community"
            && actual.draft.is_none()
            && actual.body.contains("<!-- graphoxide-draft -->");
        if !legacy_community {
            anyhow::ensure!(
                actual.input_sha256 == input_sha256,
                "generated wiki catalog page {} has stale input digest",
                path.display()
            );
        }
        if actual.draft.is_none() && !legacy_community {
            anyhow::ensure!(
                *actual_sources == sources.as_slice(),
                "generated wiki catalog page {} has stale citations",
                path.display()
            );
            continue;
        }
        anyhow::ensure!(
            !actual_sources.is_empty() && actual_sources.len() <= MAX_DRAFT_SOURCES,
            "generated wiki catalog page {} has invalid draft citations",
            path.display()
        );
        if kind == "source" {
            anyhow::ensure!(
                *actual_sources == sources.as_slice(),
                "generated wiki catalog page {} has stale draft citations",
                path.display()
            );
            continue;
        }
        let ranks = match kind.as_str() {
            "community" => graph_ref
                .parse::<i64>()
                .ok()
                .and_then(|community| community_ranks.get(&community)),
            "topic" => topic_ranks.get(&graph_ref),
            _ => None,
        }
        .with_context(|| {
            format!(
                "generated wiki catalog page {} has no draft citation ownership",
                path.display()
            )
        })?;
        anyhow::ensure!(
            actual_sources
                .iter()
                .all(|citation| ranks.contains_key(citation)),
            "generated wiki catalog page {} has draft citations outside its graph ownership",
            path.display()
        );
        let mut ordered = actual_sources.to_vec();
        ordered.sort_by(|left, right| ranks[right].cmp(&ranks[left]).then_with(|| left.cmp(right)));
        anyhow::ensure!(
            *actual_sources == ordered,
            "generated wiki catalog page {} has nondeterministic draft citations",
            path.display()
        );
    }
    Ok(())
}

fn render(
    root: &Path,
    output_path: &Path,
    pages: &[Page],
    hierarchy: Option<&[(usize, usize)]>,
) -> anyhow::Result<String> {
    let mut output = String::from("# Wiki\n\n");
    let output_parent = output_path
        .parent()
        .expect("wiki output always has a parent");
    let legacy;
    let ordered = if let Some(hierarchy) = hierarchy {
        hierarchy
    } else {
        legacy = (0..pages.len()).map(|index| (index, 0)).collect::<Vec<_>>();
        &legacy
    };
    for (index, depth) in ordered {
        let page = &pages[*index];
        let link = relative_path(output_parent, &root.join(&page.path));
        let link = link
            .components()
            .map(|component| {
                component.as_os_str().to_str().ok_or_else(|| {
                    anyhow::anyhow!("wiki page path is not valid UTF-8: {}", page.path.display())
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?
            .join("/");
        let indent = "  ".repeat(*depth);
        output.push_str(&indent);
        output.push_str("- [");
        output.push_str(&escape_link_label(&page.title));
        output.push_str("](");
        output.push_str(&escape_link_destination(&link));
        output.push_str(")\n");
    }
    Ok(output)
}

fn escape_link_label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('[', "\\[")
        .replace(']', "\\]")
}

fn escape_link_destination(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/') {
            escaped.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            write!(escaped, "%{byte:02X}").expect("writing to a String cannot fail");
        }
    }
    escaped
}

fn relative_path(from: &Path, to: &Path) -> PathBuf {
    let from = from.components().collect::<Vec<_>>();
    let to = to.components().collect::<Vec<_>>();
    let shared = from
        .iter()
        .zip(&to)
        .take_while(|(left, right)| left == right)
        .count();
    let mut result = PathBuf::new();
    for _ in &from[shared..] {
        result.push("..");
    }
    for component in &to[shared..] {
        result.push(component.as_os_str());
    }
    result
}

#[cfg(all(test, target_os = "linux", target_arch = "x86_64"))]
mod tests {
    use super::{
        load, parse_frontmatter, read_config, same_file_identity,
        validate_canonical_draft_insertion, write_index, write_text_atomic_in, OutputDirectory,
    };
    use std::{
        collections::BTreeSet,
        ffi::CString,
        fs,
        os::unix::{
            ffi::OsStrExt as _,
            fs::{symlink, MetadataExt as _},
        },
        path::Path,
        time::{Duration, SystemTime},
    };

    #[test]
    fn frontmatter_accepts_bounded_canonical_lists() {
        let text = "---\ntitle: \"Page\"\nsources: []\nrelated:\n  - \"other/page.md\"\naliases:\n  - \"Other page\"\n---\n\n# Page\n";
        let (fields, sources, _) = parse_frontmatter(Path::new("page.md"), text).unwrap();
        assert_eq!(fields.get("sources").map(String::as_str), Some(""));
        assert_eq!(fields.get("related").map(String::as_str), Some(""));
        assert_eq!(fields.get("aliases").map(String::as_str), Some(""));
        assert!(sources.is_empty());
    }

    #[test]
    fn canonical_draft_rejects_nested_model_headings() {
        let allowed = BTreeSet::from(["block-1"]);
        assert!(validate_canonical_draft_insertion(
            "## Details\n### Nested model heading\nBody.\nEvidence blocks: `block-1`",
            &allowed,
        )
        .is_err());
    }

    #[test]
    fn canonical_draft_rejects_more_than_eight_sections() {
        let ids = (0..9)
            .map(|index| format!("block-{index}"))
            .collect::<Vec<_>>();
        let allowed = ids.iter().map(String::as_str).collect::<BTreeSet<_>>();
        let draft = ids
            .iter()
            .enumerate()
            .map(|(index, id)| format!("## Section {index}\nBody.\nEvidence blocks: `{id}`"))
            .collect::<Vec<_>>()
            .join("\n\n");
        assert!(validate_canonical_draft_insertion(&draft, &allowed).is_err());
    }

    #[test]
    fn output_descriptor_cannot_be_redirected_by_an_ancestor_swap() {
        let temp = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("outside");
        let managed = temp.path().join("managed");
        let moved = temp.path().join("moved");
        fs::create_dir(&managed).expect("managed");
        let parent = OutputDirectory::open_existing(&managed).expect("open managed directory");

        fs::rename(&managed, &moved).expect("move managed directory");
        symlink(outside.path(), &managed).expect("replace managed path with link");
        write_text_atomic_in(&parent, "llms.txt", "safe").expect("publish through directory fd");

        assert_eq!(
            fs::read_to_string(moved.join("llms.txt")).expect("managed output"),
            "safe"
        );
        assert!(!outside.path().join("llms.txt").exists());
    }

    #[test]
    fn output_directory_rejects_a_symlinked_ancestor() {
        let temp = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("outside");
        let path = temp.path().join("redirected/child");
        fs::create_dir(outside.path().join("child")).expect("outside child");
        symlink(outside.path(), temp.path().join("redirected")).expect("ancestor link");

        assert!(OutputDirectory::open_existing(&path).is_err());
    }

    #[test]
    fn index_output_stays_in_the_original_root_after_a_root_swap() {
        let temp = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("outside");
        let managed = temp.path().join("managed");
        let moved = temp.path().join("moved");
        fs::create_dir_all(managed.join("docs")).expect("docs");
        fs::write(
            managed.join("wiki.json"),
            r#"{"version":1,"roots":["docs"],"exclude":[],"required_frontmatter":["title","sources"],"output":"llms.txt"}"#,
        )
        .expect("config");
        fs::write(
            managed.join("docs/page.md"),
            "---\ntitle: Page\nsources:\n  - source#capture\n---\n",
        )
        .expect("page");
        let indexed =
            load(&managed, &managed.join("wiki.json"), None, None, None).expect("load wiki");

        fs::rename(&managed, &moved).expect("move wiki root");
        symlink(outside.path(), &managed).expect("replace wiki root with link");
        write_index(&indexed).expect("publish index");

        assert!(moved.join("llms.txt").exists());
        assert!(!outside.path().join("llms.txt").exists());
    }

    #[test]
    fn descriptor_input_read_uses_the_opened_root_and_rejects_a_swapped_leaf() {
        let temp = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("outside");
        let managed = temp.path().join("managed");
        let moved = temp.path().join("moved");
        fs::create_dir_all(managed.join("docs")).expect("docs");
        fs::write(managed.join("docs/page.md"), "trusted").expect("page");
        fs::write(outside.path().join("page.md"), "external").expect("outside page");
        let root = OutputDirectory::open_existing(&managed).expect("open managed directory");

        fs::rename(&managed, &moved).expect("move managed directory");
        symlink(outside.path(), &managed).expect("replace managed path with link");
        assert_eq!(
            root.read_bounded_regular(Path::new("docs/page.md"), 1024)
                .expect("read original root"),
            b"trusted"
        );

        fs::remove_file(moved.join("docs/page.md")).expect("remove page");
        symlink(outside.path().join("page.md"), moved.join("docs/page.md"))
            .expect("replace page with link");
        assert!(root
            .read_bounded_regular(Path::new("docs/page.md"), 1024)
            .is_err());
    }

    #[test]
    fn descriptor_input_read_rejects_fifo_without_blocking() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = OutputDirectory::open_existing(temp.path()).expect("open root");
        let fifo = temp.path().join("page.md");
        let fifo_name = CString::new(fifo.as_os_str().as_bytes()).expect("fifo path");
        // SAFETY: the C string is NUL-terminated and remains valid for the call.
        assert_eq!(unsafe { libc::mkfifo(fifo_name.as_ptr(), 0o600) }, 0);

        assert!(root
            .read_bounded_regular(Path::new("page.md"), 1024)
            .is_err());
    }

    #[test]
    fn descriptor_input_identity_rejects_a_same_size_in_place_rewrite() {
        let temp = tempfile::tempdir().expect("tempdir");
        let page = temp.path().join("page.md");
        let preserved_mtime = SystemTime::UNIX_EPOCH + Duration::new(1, 999_999_999);
        fs::write(&page, "old").expect("initial page");
        fs::File::open(&page)
            .expect("open initial page")
            .set_times(fs::FileTimes::new().set_modified(preserved_mtime))
            .expect("set initial timestamp");
        let initial = fs::metadata(&page).expect("initial metadata");

        fs::write(&page, "new").expect("rewrite same-size page");
        fs::File::open(&page)
            .expect("open rewritten page")
            .set_times(fs::FileTimes::new().set_modified(preserved_mtime))
            .expect("restore rewritten timestamp");
        let final_metadata = fs::metadata(&page).expect("rewritten metadata");

        assert_eq!(initial.dev(), final_metadata.dev());
        assert_eq!(initial.ino(), final_metadata.ino());
        assert_eq!(initial.len(), final_metadata.len());
        assert_eq!((initial.mtime(), initial.mtime_nsec()), (1, 999_999_999));
        assert_eq!(
            (initial.mtime(), initial.mtime_nsec()),
            (final_metadata.mtime(), final_metadata.mtime_nsec())
        );
        assert_ne!(
            (initial.ctime(), initial.ctime_nsec()),
            (final_metadata.ctime(), final_metadata.ctime_nsec())
        );
        assert!(!same_file_identity(&initial, &final_metadata));
    }

    #[test]
    fn config_read_uses_the_opened_root_after_a_swap() {
        let temp = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("outside");
        let managed = temp.path().join("managed");
        let moved = temp.path().join("moved");
        fs::create_dir(&managed).expect("managed");
        fs::write(
            managed.join("wiki.json"),
            r#"{"version":1,"roots":["docs"],"exclude":[],"required_frontmatter":["title","sources"],"output":"llms.txt"}"#,
        )
        .expect("config");
        fs::write(
            outside.path().join("wiki.json"),
            r#"{"version":2,"roots":["docs"],"exclude":[],"required_frontmatter":["title","sources"],"output":"llms.txt"}"#,
        )
        .expect("outside config");
        let root = OutputDirectory::open_existing(&managed).expect("open managed directory");

        fs::rename(&managed, &moved).expect("move managed directory");
        symlink(outside.path(), &managed).expect("replace managed path with link");
        assert_eq!(
            read_config(&root, Path::new("wiki.json"), &moved.join("wiki.json"))
                .expect("read original config")
                .version,
            1
        );

        fs::remove_file(moved.join("wiki.json")).expect("remove config");
        symlink(outside.path().join("wiki.json"), moved.join("wiki.json"))
            .expect("replace config with link");
        assert!(read_config(&root, Path::new("wiki.json"), &moved.join("wiki.json")).is_err());
    }
}
