//! Incremental, graph-only publication of a reviewed canonical wiki.

use crate::wiki::{
    open_or_create_output_root, output_parent, parse_frontmatter, remove_regular_file_in,
    write_text_atomic_in, OutputDirectory,
};
use anyhow::{Context as _, Result};
use graphoxide_core::KnowledgeGraph;
use graphoxide_export::{load_wiki_plan, render_canonical_wiki, StructuredWikiPage};
use graphoxide_extract::{
    catalog::Catalog,
    registry::{RegistryReviewDecision, RegistrySnapshot},
    registry_state::{Availability, RegistryLocalState},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    fs,
    path::{Component, Path, PathBuf},
    process::Command,
    time::Instant,
};

const MAX_WIKI_PLAN_BYTES: usize = 8 * 1024 * 1024;
const MAX_LIVE_MANIFEST_BYTES: usize = 16 * 1024 * 1024;
const MAX_LIVE_PAGE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct MaterializeArgs {
    pub registry_repo: PathBuf,
    pub registry_rev: String,
    pub origin_id: String,
    pub graph: PathBuf,
    pub plan: PathBuf,
    pub output: PathBuf,
    pub drafts: Option<PathBuf>,
    pub policy: Option<PathBuf>,
    pub agent_jobs: usize,
    pub progress: MaterializeProgress,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum MaterializeProgress {
    Jsonl,
    Never,
}

#[derive(Deserialize, Serialize)]
struct LiveManifest {
    version: u64,
    registry: RegistryProvenance,
    graph_sha256: String,
    plan_sha256: String,
    sources: Vec<LiveSource>,
    pages: Vec<LivePage>,
    #[serde(default)]
    historical: Vec<HistoricalPage>,
}

#[derive(Deserialize, Serialize)]
struct RegistryProvenance {
    catalog_id: String,
    tree_sha256: String,
    git_commit: String,
    origin_id: String,
    policy: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct LiveSource {
    citation: String,
    state: String,
    pages: Vec<String>,
}

#[derive(Deserialize, Serialize)]
struct LivePage {
    path: String,
    sha256: String,
    state: String,
    #[serde(default)]
    review_id: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct HistoricalPage {
    path: String,
    archived_path: String,
    sha256: String,
    state: String,
}

#[derive(Serialize)]
struct SearchIndex {
    version: u64,
    entries: Vec<SearchEntry>,
}

#[derive(Serialize)]
struct SearchEntry {
    path: String,
    title: String,
    aliases: Vec<String>,
    kind: String,
    domain: String,
    citations: Vec<String>,
    locators: Vec<String>,
    evidence_ids: Vec<String>,
    body: String,
}

/// Materialize each source/reference page before reconciling global navigation.
///
/// This is deliberately graph-only: the registry commit, graph and reviewed
/// plan are pinned first, then all content comes from retained graph evidence.
pub fn materialize(args: MaterializeArgs) -> Result<()> {
    anyhow::ensure!(
        args.agent_jobs == 1,
        "wiki materialize requires --agent-jobs 1 in this release"
    );
    let registry_repo = checked_directory(&args.registry_repo, "registry repository")?;
    let git_commit = checked_registry_commit(&registry_repo, &args.registry_rev)?;
    let registry = RegistrySnapshot::load(&registry_repo)?;
    let stale_citations = locally_unavailable_citations(&registry)?;
    let policy = validate_policy_path(&registry_repo, args.policy.as_deref())?;
    let catalog =
        Catalog::from_registry_origin_metadata(&registry_repo, &registry, &args.origin_id)?;
    let (graph, graph_sha256) = read_graph(&args.graph)?;
    catalog.validate_graph_annotations(&graph)?;
    let plan_bytes = read_regular_file(&args.plan, MAX_WIKI_PLAN_BYTES, "wiki plan")?;
    let plan_sha256 = hex::encode(Sha256::digest(&plan_bytes));
    let plan = load_wiki_plan(&plan_bytes, &catalog.citation_keys())?;
    let rendered = render_canonical_wiki(&graph, &plan, &catalog.active_annotations())?;
    let mut publication = rendered.clone();
    let drafted_pages = apply_canonical_drafts(&mut publication.pages, args.drafts.as_deref())?;

    let output_path = absolute_path(&args.output)?;
    let output = open_or_create_output_root(&output_path)?;
    let previous = read_live_manifest(&output)?;
    let stale_pages = archive_changed_pages(&output, previous.as_ref(), &publication.pages)?;
    write_path(&output, "wiki.json", WIKI_CONFIG)?;

    let mut source_pages = BTreeMap::<String, Vec<&StructuredWikiPage>>::new();
    let mut global_pages = Vec::new();
    for page in &publication.pages {
        let (fields, citations, _) = parse_frontmatter(Path::new(&page.path), &page.markdown)?;
        let kind = fields.get("kind").map(String::as_str).unwrap_or_default();
        if matches!(kind, "source" | "reference" | "inventory") && citations.len() == 1 {
            source_pages
                .entry(citations[0].clone())
                .or_default()
                .push(page);
        } else {
            global_pages.push(page);
        }
    }
    for pages in source_pages.values_mut() {
        pages.sort_by(|left, right| {
            source_page_rank(left)
                .cmp(&source_page_rank(right))
                .then_with(|| left.path.cmp(&right.path))
        });
    }
    global_pages.sort_by(|left, right| left.path.cmp(&right.path));

    let mut manifest = LiveManifest {
        version: 1,
        registry: RegistryProvenance {
            catalog_id: registry.catalog_id().to_owned(),
            tree_sha256: registry.tree_sha256().to_owned(),
            git_commit,
            origin_id: args.origin_id,
            policy,
        },
        graph_sha256,
        plan_sha256: plan_sha256.clone(),
        sources: Vec::new(),
        pages: Vec::new(),
        historical: stale_pages.historical,
    };
    let started = Instant::now();
    for (citation, pages) in source_pages {
        let state = if stale_citations.contains(&citation) {
            "stale"
        } else {
            "source-ready"
        };
        let rendered_pages = pages
            .iter()
            .map(|page| Ok((page, with_publication_state(&page.markdown, state)?)))
            .collect::<Result<Vec<_>>>()?;
        for (page, markdown) in &rendered_pages {
            write_path(&output, &page.path, markdown)?;
        }
        let paths = pages
            .iter()
            .map(|page| page.path.clone())
            .collect::<Vec<_>>();
        manifest.sources.push(LiveSource {
            citation: citation.clone(),
            state: state.into(),
            pages: paths.clone(),
        });
        manifest
            .pages
            .extend(rendered_pages.iter().map(|(page, markdown)| LivePage {
                path: page.path.clone(),
                sha256: text_sha256(markdown),
                state: state.into(),
                review_id: None,
            }));
        manifest
            .sources
            .sort_by(|left, right| left.citation.cmp(&right.citation));
        manifest
            .pages
            .sort_by(|left, right| left.path.cmp(&right.path));
        write_manifest(&output, &manifest)?;
        emit_progress(
            args.progress,
            &citation,
            &paths,
            state,
            started.elapsed().as_millis(),
        );
    }

    for page in global_pages {
        let (fields, citations, _) = parse_frontmatter(Path::new(&page.path), &page.markdown)?;
        let review_id = (fields.get("kind").map(String::as_str) == Some("article"))
            .then(|| approved_review_for_page(&registry, &plan_sha256, &citations, page))
            .transpose()?
            .flatten();
        let state = if citations
            .iter()
            .any(|citation| stale_citations.contains(citation))
        {
            "stale"
        } else if review_id.is_some() {
            "reviewed-ready"
        } else if drafted_pages.contains(&page.path) {
            "draft-ready"
        } else {
            "source-ready"
        };
        let markdown = with_publication_state(&page.markdown, state)?;
        write_path(&output, &page.path, &markdown)?;
        manifest.pages.push(LivePage {
            path: page.path.clone(),
            sha256: text_sha256(&markdown),
            state: state.into(),
            review_id,
        });
    }
    for path in stale_pages.obsolete {
        remove_regular_file_in(&output, Path::new(&path))?;
    }
    manifest
        .pages
        .sort_by(|left, right| left.path.cmp(&right.path));
    write_path(&output, "changes.md", &changes_page(&manifest))?;
    write_json(&output, "search.json", &search_index(&publication.pages)?)?;
    crate::wiki::index(&output_path, &output_path.join("wiki.json"))?;
    crate::wiki::check_with_canonical_plan(
        &output_path,
        &output_path.join("wiki.json"),
        &catalog.citation_keys(),
        &graph,
        &catalog.active_annotations(),
        &rendered,
    )?;
    write_manifest(&output, &manifest)
}

fn apply_canonical_drafts(
    pages: &mut [StructuredWikiPage],
    drafts: Option<&Path>,
) -> Result<BTreeSet<String>> {
    let Some(drafts) = drafts else {
        return Ok(BTreeSet::new());
    };
    let drafts_path = checked_directory(drafts, "wiki draft directory")?;
    let drafts_root = OutputDirectory::open_existing(&drafts_path)?;
    let mut candidate = pages.to_vec();
    for page in &mut candidate {
        let path = Path::new(&page.path);
        let draft_path = drafts_path.join(path);
        let metadata = match fs::symlink_metadata(&draft_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("inspect draft {}", path.display()))
            }
        };
        anyhow::ensure!(
            metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
            "wiki draft {} must be a regular non-symlinked file",
            path.display()
        );
        let bytes = drafts_root
            .read_bounded_regular(path, MAX_LIVE_PAGE_BYTES)
            .with_context(|| format!("read wiki draft {}", path.display()))?;
        page.markdown = String::from_utf8(bytes)
            .with_context(|| format!("wiki draft {} is not UTF-8", path.display()))?;
    }
    let drafted = crate::wiki::validate_canonical_draft_overlays(&candidate, pages)?;
    pages.clone_from_slice(&candidate);
    Ok(drafted)
}

/// Local availability is metadata-only, so it can safely mark otherwise
/// graph-derived pages stale without rereading raw sources.
fn locally_unavailable_citations(registry: &RegistrySnapshot) -> Result<BTreeSet<String>> {
    let state = RegistryLocalState::open(registry.catalog_id(), registry.tree_sha256())?;
    let observations = state.observations()?;
    Ok(registry
        .active_captures()
        .into_iter()
        .filter(|capture| {
            observations
                .get(&capture.source().source_id)
                .is_some_and(|observation| {
                    matches!(
                        observation.availability,
                        Availability::Missing | Availability::Inaccessible
                    )
                })
        })
        .map(|capture| {
            format!(
                "{}#{}",
                capture.source().source_id,
                capture.capture().capture_id
            )
        })
        .collect())
}

fn approved_review_for_page(
    registry: &RegistrySnapshot,
    plan_sha256: &str,
    citations: &[String],
    page: &StructuredWikiPage,
) -> Result<Option<String>> {
    if citations.is_empty() {
        return Ok(None);
    }
    let capture_set_sha256 = active_capture_set_sha256(registry, citations)?;
    let draft_sha256 = reviewable_draft_sha256(&page.markdown);
    let legacy_draft_sha256 = text_sha256(&page.markdown);
    Ok(registry
        .reviews()
        .into_iter()
        .filter(|review| {
            review.decision == RegistryReviewDecision::Approved
                && review.plan_sha256 == plan_sha256
                && review.capture_set_sha256 == capture_set_sha256
                // Registry v1 review records created before publication-state
                // frontmatter bound the complete rendered draft.
                && (review.draft_sha256 == draft_sha256
                    || review.draft_sha256 == legacy_draft_sha256)
        })
        .max_by(|left, right| {
            left.reviewed_at
                .cmp(&right.reviewed_at)
                .then_with(|| left.review_id.cmp(&right.review_id))
        })
        .map(|review| review.review_id))
}

fn active_capture_set_sha256(registry: &RegistrySnapshot, citations: &[String]) -> Result<String> {
    let mut entries = BTreeSet::new();
    for citation in citations {
        let (source_id, capture_id) = citation.rsplit_once('#').with_context(|| {
            format!("reviewed page has malformed capture citation {citation:?}")
        })?;
        let capture = registry
            .captures()
            .get(capture_id)
            .with_context(|| format!("reviewed page references unknown capture {capture_id:?}"))?;
        anyhow::ensure!(
            capture.source_id == source_id,
            "reviewed page source/capture closure is invalid"
        );
        anyhow::ensure!(
            entries.insert(format!("{source_id}\t{capture_id}\t{}\n", capture.sha256)),
            "reviewed page repeats capture citation {citation:?}"
        );
    }
    Ok(hex::encode(Sha256::digest(
        entries.into_iter().collect::<String>().as_bytes(),
    )))
}

struct StalePages {
    historical: Vec<HistoricalPage>,
    obsolete: Vec<String>,
}

fn read_live_manifest(output: &OutputDirectory) -> Result<Option<LiveManifest>> {
    if !output.entry_exists(OsStr::new("wiki-manifest.json"))? {
        return Ok(None);
    }
    let bytes =
        output.read_bounded_regular(Path::new("wiki-manifest.json"), MAX_LIVE_MANIFEST_BYTES)?;
    let manifest: LiveManifest =
        serde_json::from_slice(&bytes).context("parse prior live wiki manifest")?;
    anyhow::ensure!(
        manifest.version == 1,
        "unsupported prior live wiki manifest"
    );
    Ok(Some(manifest))
}

fn archive_changed_pages(
    output: &OutputDirectory,
    previous: Option<&LiveManifest>,
    current: &[StructuredWikiPage],
) -> Result<StalePages> {
    let Some(previous) = previous else {
        return Ok(StalePages {
            historical: Vec::new(),
            obsolete: Vec::new(),
        });
    };
    let current = current
        .iter()
        .map(|page| (page.path.as_str(), reviewable_draft_sha256(&page.markdown)))
        .collect::<BTreeMap<_, _>>();
    let generation = format!(
        "{}-{}",
        previous.registry.git_commit,
        &previous.plan_sha256[..16.min(previous.plan_sha256.len())]
    );
    anyhow::ensure!(
        valid_git_object_id(&previous.registry.git_commit) && valid_sha256(&previous.plan_sha256),
        "prior live wiki manifest has invalid provenance digests"
    );
    let mut historical = previous
        .historical
        .iter()
        .map(copy_historical)
        .collect::<Vec<_>>();
    let mut obsolete = Vec::new();
    for page in &previous.pages {
        let source = Path::new(&page.path);
        anyhow::ensure!(
            source
                .components()
                .all(|component| matches!(component, Component::Normal(_))),
            "prior live wiki manifest has an unsafe page path"
        );
        let bytes = output.read_bounded_regular(source, MAX_LIVE_PAGE_BYTES)?;
        let text = String::from_utf8(bytes).context("prior live wiki page is not UTF-8")?;
        anyhow::ensure!(
            text_sha256(&text) == page.sha256,
            "prior live wiki page changed outside Graphoxide: {}",
            page.path
        );
        if current.get(page.path.as_str()) == Some(&reviewable_draft_sha256(&text)) {
            continue;
        }
        let archived_path = format!("history/{generation}/{}", page.path);
        let historical_text = with_publication_state(&text, "historical")?;
        write_path(output, &archived_path, &historical_text)?;
        historical.push(HistoricalPage {
            path: page.path.clone(),
            archived_path,
            sha256: text_sha256(&historical_text),
            state: "historical".into(),
        });
        if !current.contains_key(page.path.as_str()) {
            obsolete.push(page.path.clone());
        }
    }
    historical.sort_by(|left, right| left.archived_path.cmp(&right.archived_path));
    historical.dedup_by(|left, right| left.archived_path == right.archived_path);
    obsolete.sort();
    obsolete.dedup();
    Ok(StalePages {
        historical,
        obsolete,
    })
}

fn copy_historical(value: &HistoricalPage) -> HistoricalPage {
    HistoricalPage {
        path: value.path.clone(),
        archived_path: value.archived_path.clone(),
        sha256: value.sha256.clone(),
        state: "historical".into(),
    }
}

const WIKI_CONFIG: &str = "{\"version\":1,\"roots\":[\".\"],\"exclude\":[\"changes.md\",\"history\"],\"required_frontmatter\":[\"title\",\"sources\",\"kind\",\"graph_ref\",\"parent\",\"input_sha256\",\"publication_state\"],\"output\":\"llms.txt\"}\n";

fn source_page_rank(page: &StructuredWikiPage) -> u8 {
    if page.path.starts_with("sources/") {
        1
    } else {
        0
    }
}

fn with_publication_state(markdown: &str, state: &str) -> Result<String> {
    let review_status = match state {
        "reviewed-ready" => "reviewed",
        "stale" => "stale",
        "historical" => "archived",
        "source-ready" | "draft-ready" => "generated",
        _ => anyhow::bail!("unsupported wiki publication state {state:?}"),
    };
    let (frontmatter, body) = markdown
        .split_once("---\n\n")
        .context("canonical wiki page lacks closing frontmatter")?;
    anyhow::ensure!(
        frontmatter.starts_with("---\n") && frontmatter.ends_with('\n'),
        "canonical wiki page frontmatter is malformed"
    );
    let metadata = frontmatter
        .split_inclusive('\n')
        .filter(|line| {
            !line.starts_with("publication_state:") && !line.starts_with("review_status:")
        })
        .collect::<String>();
    Ok(format!(
        "{metadata}publication_state: {}\nreview_status: {}\n---\n\n{body}",
        serde_json::to_string(state)?,
        serde_json::to_string(review_status)?,
    ))
}

/// Digest the reviewable page content, excluding live publication state.
///
/// Review artifacts bind this value so a transition from `draft-ready` to
/// `reviewed-ready` cannot invalidate the reviewed prose or evidence closure.
pub fn reviewable_draft_sha256(markdown: &str) -> String {
    let normalized = markdown
        .split_once("---\n\n")
        .map(|(frontmatter, body)| {
            let frontmatter = frontmatter
                .split_inclusive('\n')
                .filter(|line| {
                    !line.starts_with("publication_state:") && !line.starts_with("review_status:")
                })
                .collect::<String>();
            format!("{frontmatter}---\n\n{body}")
        })
        .unwrap_or_else(|| markdown.to_owned());
    text_sha256(&normalized)
}

fn write_path(output: &OutputDirectory, path: &str, text: &str) -> Result<()> {
    let path = Path::new(path);
    anyhow::ensure!(
        path.components()
            .all(|component| matches!(component, Component::Normal(_))),
        "unsafe materialized wiki path"
    );
    let (parent, name) = output_parent(output, path)?;
    write_text_atomic_in(&parent, &name, text)
}

fn write_manifest(output: &OutputDirectory, manifest: &LiveManifest) -> Result<()> {
    write_json(output, "wiki-manifest.json", manifest)
}

fn write_json<T: Serialize>(output: &OutputDirectory, path: &str, value: &T) -> Result<()> {
    let text = format!("{}\n", serde_json::to_string_pretty(value)?);
    write_path(output, path, &text)
}

fn search_index(pages: &[StructuredWikiPage]) -> Result<SearchIndex> {
    let mut entries = Vec::with_capacity(pages.len());
    for page in pages {
        let (fields, citations, body_start) =
            parse_frontmatter(Path::new(&page.path), &page.markdown)?;
        let evidence_ids = page
            .markdown
            .lines()
            .filter_map(|line| line.strip_prefix("- Evidence block: `"))
            .filter_map(|line| line.strip_suffix('`'))
            .map(str::to_owned)
            .collect();
        let locators = page
            .markdown
            .lines()
            .filter_map(|line| {
                line.strip_prefix("- Source location: ")
                    .or_else(|| line.strip_prefix("- Structured path: "))
                    .or_else(|| line.strip_prefix("- Location: "))
            })
            .map(str::to_owned)
            .collect();
        entries.push(SearchEntry {
            path: page.path.clone(),
            title: fields.get("title").cloned().unwrap_or_default(),
            aliases: frontmatter_list(&page.markdown, "aliases")?,
            kind: fields.get("kind").cloned().unwrap_or_default(),
            domain: fields.get("domain").cloned().unwrap_or_default(),
            citations,
            locators,
            evidence_ids,
            body: page.markdown[body_start..].chars().take(2048).collect(),
        });
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(SearchIndex {
        version: 1,
        entries,
    })
}

fn frontmatter_list(markdown: &str, expected_key: &str) -> Result<Vec<String>> {
    let mut lines = markdown.lines();
    anyhow::ensure!(lines.next() == Some("---"), "wiki page lacks frontmatter");
    let mut in_list = false;
    let mut values = Vec::new();
    for line in lines {
        if line == "---" {
            break;
        }
        if let Some(key) = line.strip_suffix(':') {
            in_list = key == expected_key;
            continue;
        }
        if in_list {
            let Some(value) = line.strip_prefix("  - ") else {
                in_list = false;
                continue;
            };
            values.push(serde_json::from_str(value).context("parse canonical frontmatter list")?);
        }
    }
    Ok(values)
}

fn changes_page(manifest: &LiveManifest) -> String {
    let ready = manifest
        .sources
        .iter()
        .filter(|source| source.state == "source-ready")
        .count();
    let stale = manifest
        .sources
        .iter()
        .filter(|source| source.state == "stale")
        .count();
    format!(
        "# Changes\n\nLive materialization from registry `{}` at commit `{}`. {ready} source capture(s) are `source-ready` and {stale} are `stale`; use [the manifest](wiki-manifest.json) for exact page and digest provenance.\n",
        manifest.registry.catalog_id,
        manifest.registry.git_commit,
    )
}

fn emit_progress(
    progress: MaterializeProgress,
    citation: &str,
    pages: &[String],
    state: &str,
    elapsed_ms: u128,
) {
    if matches!(progress, MaterializeProgress::Jsonl) {
        eprintln!(
            "{}",
            serde_json::json!({
                "event": state,
                "citation": citation,
                "pages": pages,
                "elapsed_ms": elapsed_ms,
            })
        );
    }
}

fn checked_registry_commit(repo: &Path, requested: &str) -> Result<String> {
    anyhow::ensure!(
        matches!(requested.len(), 40 | 64)
            && requested.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "--registry-rev must be an exact Git commit object ID"
    );
    let commit = git_stdout(
        repo,
        ["rev-parse", "--verify", &format!("{requested}^{{commit}}")],
    )?;
    let head = git_stdout(repo, ["rev-parse", "HEAD"])?;
    anyhow::ensure!(
        commit == head,
        "registry repository must be checked out at --registry-rev"
    );
    let status = git_stdout(repo, ["status", "--porcelain=v1", "--untracked-files=all"])?;
    anyhow::ensure!(
        status.is_empty(),
        "registry repository must be clean before materialization"
    );
    Ok(commit)
}

fn git_stdout<const N: usize>(repo: &Path, args: [&str; N]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .context("run git for pinned registry")?;
    anyhow::ensure!(
        output.status.success(),
        "Git rejected the pinned registry request: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let text =
        String::from_utf8(output.stdout).context("Git returned non-UTF-8 registry output")?;
    Ok(text.trim().to_owned())
}

fn validate_policy_path(repo: &Path, policy: Option<&Path>) -> Result<Option<String>> {
    let Some(policy) = policy else {
        return Ok(None);
    };
    let policy = absolute_path(policy)?;
    let relative = policy
        .strip_prefix(repo)
        .context("--policy must be beneath --registry-repo")?;
    anyhow::ensure!(
        relative == Path::new("policies/freshness.json"),
        "--policy must be policies/freshness.json in the pinned registry"
    );
    let _ = read_regular_file(&policy, 256 * 1024, "freshness policy")?;
    Ok(Some("policies/freshness.json".into()))
}

fn read_graph(path: &Path) -> Result<(KnowledgeGraph, String)> {
    let bytes = read_regular_file(
        path,
        usize::try_from(graphoxide_core::max_graph_bytes()).unwrap_or(usize::MAX),
        "graph",
    )?;
    let digest = hex::encode(Sha256::digest(&bytes));
    Ok((
        serde_json::from_slice(&bytes).context("parse graph for wiki materialization")?,
        digest,
    ))
}

fn read_regular_file(path: &Path, cap: usize, label: &str) -> Result<Vec<u8>> {
    let path = absolute_path(path)?;
    let name = path
        .file_name()
        .filter(|name| !name.is_empty())
        .context("input must have a final path component")?;
    let parent = path
        .parent()
        .context("input must have a parent directory")?
        .canonicalize()
        .with_context(|| format!("resolve {label} parent"))?;
    OutputDirectory::open_existing(&parent)?
        .read_bounded_regular(Path::new(name), cap)
        .with_context(|| format!("read {label}"))
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn checked_directory(path: &Path, label: &str) -> Result<PathBuf> {
    let path = absolute_path(path)?;
    let metadata = fs::symlink_metadata(&path).with_context(|| format!("inspect {label}"))?;
    anyhow::ensure!(
        metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
        "{label} must be a non-symlinked directory"
    );
    path.canonicalize()
        .with_context(|| format!("resolve {label}"))
}

fn text_sha256(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_git_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::search_index;
    use graphoxide_export::StructuredWikiPage;

    #[test]
    fn search_index_retains_evidence_ids_and_bounded_body() {
        let page = StructuredWikiPage {
            path: "sources/example.md".into(),
            markdown: "---\ntitle: \"Example\"\nkind: \"source\"\narticle_type: \"reference\"\ngraph_ref: \"example#capture\"\nparent: \"index.md\"\ndomain: \"docs\"\nsummary: \"x\"\ncoverage: \"complete\"\nreview_status: \"generated\"\ninput_sha256: \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\nsources:\n  - \"example#capture\"\nrelated: []\naliases:\n  - \"example-alias\"\n---\n\n# Example\n\n- Evidence block: `block-1`\n- Source location: L4\n".into(),
        };
        let index = search_index(&[page]).expect("search index");
        assert_eq!(index.entries[0].evidence_ids, ["block-1"]);
        assert_eq!(index.entries[0].citations, ["example#capture"]);
        assert_eq!(index.entries[0].aliases, ["example-alias"]);
        assert_eq!(index.entries[0].locators, ["L4"]);
    }
}
