//! Hugo preview support for the direct, pointer-only knowledgebase.

use anyhow::{ensure, Context, Result};
use graphoxide_core::{write_bytes_atomic_strict, write_json_atomic_strict};
use graphoxide_export::direct_taxonomy::{
    canonical_assignments, canonical_taxonomy_policy, parse_assignments, parse_taxonomy_policy,
    taxonomy_policy_digest, Assignments, TAXONOMY_ASSIGNMENTS_SCHEMA,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const PREVIEW_SCHEMA: &str = "graphoxide.direct-hugo-preview";
const PREVIEW_REVISION: u8 = 11;
const MAX_TAXONOMY_BYTES: usize = 1024 * 1024;
const MAX_DERIVED_PAGE_BYTES: usize = 256 * 1024;
const HUGO_VERSION: &str = "0.165.0";
const HUGO_BINARY_ENV: &str = "GRAPHOXIDE_HUGO_BINARY";

/// The materialized Hugo source and its isolated render destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectPreviewSite {
    pub source: PathBuf,
    pub public: PathBuf,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct PreviewRoot {
    schema: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct PreviewManifest {
    schema: String,
    revision: String,
    adapter_revision: u8,
    source_sha256: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct PreviewCurrent {
    schema: String,
    revision: String,
}

#[derive(Serialize)]
struct DirectSearchIndex<'a> {
    pages: Vec<DirectSearchEntry<'a>>,
}

#[derive(Serialize)]
struct DirectSearchEntry<'a> {
    title: String,
    route: &'a str,
    subject: &'a str,
    status: &'a str,
}

#[derive(Serialize)]
struct DirectHubPage {
    title: String,
    card_sections: Vec<DirectCardSection>,
}

#[derive(Serialize)]
struct DirectCardSection {
    title: String,
    cards: Vec<DirectNavigationCard>,
}

#[derive(Serialize)]
struct DirectNavigationCard {
    title: String,
    route: String,
    description: String,
    count: usize,
}

#[derive(Serialize)]
struct DirectMetadataRow {
    label: String,
    values: Vec<String>,
}

/// Materialize and serve the direct knowledgebase on loopback using the local `hugo` binary.
///
/// The preview opens no source locator: it renders only the pointer index, canonical taxonomy,
/// and generated assignments. The command remains foreground-owned so it has no detached state.
pub fn live(root: &Path, port: u16, open: bool) -> Result<()> {
    ensure!(port > 0, "Hugo server port must be positive");
    let hugo = local_hugo_binary()?;
    let root = checked_root(root)?;
    let output = managed_directory(&root, &[".graphoxide", "hugo", "direct-live"])?;
    let preview = materialize_direct_preview_with_hugo(&root, &output, &hugo)?;
    let mut command = Command::new(hugo);
    command
        .args(["server", "--bind", "127.0.0.1", "--port"])
        .arg(port.to_string())
        .args(["--source"])
        .arg(&preview.source)
        .args(["--destination"])
        .arg(&preview.public)
        .args([
            "--disableFastRender",
            "--noBuildLock",
            "--cleanDestinationDir",
        ]);
    if open {
        command.arg("--openBrowser");
    }
    let status = command.status().context("run direct Hugo preview")?;
    ensure!(status.success(), "direct Hugo preview failed");
    Ok(())
}

/// Materialize a deterministic, disposable Hugo source tree from direct-source metadata.
///
/// No raw source body, local binding root, provider configuration, or model response is accepted
/// by this API or written into the preview tree.
pub fn materialize_direct_preview(root: &Path, output: &Path) -> Result<DirectPreviewSite> {
    materialize_direct_preview_with_hugo(root, output, &local_hugo_binary()?)
}

fn materialize_direct_preview_with_hugo(
    root: &Path,
    output: &Path,
    hugo: &Path,
) -> Result<DirectPreviewSite> {
    let _ = validate_hugo_binary(hugo)?;
    let root = checked_root(root)?;
    let _operation = crate::wiki_direct::acquire_source_operation(&root)?;
    let (index, projection, policy, pages) = direct_projection(&root)?;
    let revision = preview_revision(&index, &projection, &policy, &pages)?;
    let output = prepare_preview_root(&root, output)?;
    let versions = managed_directory(&output, &["versions"])?;
    let version = versions.join(&revision);
    if version.exists() {
        validate_preview_version(&version, &revision)?;
    } else {
        let staging = tempfile::Builder::new()
            .prefix("graphoxide-direct-preview-")
            .tempdir_in(&versions)?;
        let candidate = staging.path().join("site");
        fs::create_dir(&candidate)?;
        let source = candidate.join("source");
        fs::create_dir(&source)?;
        fs::create_dir(candidate.join("public"))?;
        write_hugo_config(&source)?;
        write_hugo_layout(&source)?;
        write_direct_navigation(&source, &projection, &pages)?;
        let manifest = PreviewManifest {
            schema: PREVIEW_SCHEMA.into(),
            revision: revision.clone(),
            adapter_revision: PREVIEW_REVISION,
            source_sha256: tree_digest(&source)?,
        };
        write_json_atomic_strict(candidate.join("manifest.json"), &manifest, true)?;
        fs::rename(&candidate, &version)
            .with_context(|| format!("publish direct Hugo preview revision {revision}"))?;
        validate_preview_version(&version, &revision)?;
    }
    write_json_atomic_strict(
        output.join("current.json"),
        &PreviewCurrent {
            schema: PREVIEW_SCHEMA.into(),
            revision,
        },
        true,
    )?;
    Ok(DirectPreviewSite {
        source: version.join("source"),
        public: version.join("public"),
    })
}

fn local_hugo_binary() -> Result<PathBuf> {
    let path = std::env::var_os(HUGO_BINARY_ENV)
        .map(PathBuf::from)
        .with_context(|| format!("set {HUGO_BINARY_ENV} to a local Hugo {HUGO_VERSION} binary"))?;
    let path = path
        .canonicalize()
        .with_context(|| format!("canonicalize configured Hugo binary {}", path.display()))?;
    validate_hugo_binary(&path)
}

fn validate_hugo_binary(path: &Path) -> Result<PathBuf> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect configured Hugo binary {}", path.display()))?;
    ensure!(
        metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
        "configured Hugo binary must be a regular file"
    );
    ensure!(
        metadata.len() > 0 && metadata.len() <= 64 * 1024 * 1024,
        "configured Hugo binary has an invalid size"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        ensure!(
            metadata.permissions().mode() & 0o111 != 0,
            "configured Hugo binary is not executable"
        );
    }
    #[cfg(windows)]
    ensure!(
        path.extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("exe")),
        "configured Hugo binary must be a native executable"
    );
    let output = tempfile::Builder::new()
        .prefix("graphoxide-hugo-version-")
        .tempfile_in(
            path.parent()
                .context("configured Hugo binary has no parent")?,
        )?;
    let mut child = Command::new(path)
        .arg("version")
        .stdin(Stdio::null())
        .stdout(Stdio::from(output.reopen()?))
        .stderr(Stdio::from(output.reopen()?))
        .spawn()
        .with_context(|| format!("start configured Hugo binary {}", path.display()))?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("configured Hugo binary version check timed out");
        }
        thread::sleep(Duration::from_millis(10));
    };
    ensure!(
        status.success(),
        "configured Hugo binary version check failed"
    );
    let version_bytes = read_regular(output.path(), 16 * 1024)?;
    let version = String::from_utf8_lossy(&version_bytes);
    ensure!(
        version.contains(&format!("hugo v{HUGO_VERSION}")),
        "configured Hugo binary does not report pinned version {HUGO_VERSION}"
    );
    Ok(path.to_path_buf())
}

type DirectProjection = (
    crate::wiki_source::SourceIndex,
    crate::wiki_materialize::DirectMaterialization,
    graphoxide_export::direct_taxonomy::TaxonomyPolicy,
    BTreeMap<String, Vec<u8>>,
);

fn direct_projection(root: &Path) -> Result<DirectProjection> {
    let index = crate::wiki_source::load_source_index(root)?;
    let policy_path = root.join("taxonomy/policy.json");
    let policy_bytes = crate::enrich::safe_read_bounded(root, &policy_path, MAX_TAXONOMY_BYTES)
        .context("read bounded direct taxonomy policy")?;
    let policy = parse_taxonomy_policy(&policy_bytes)?;
    ensure!(
        policy_bytes == canonical_taxonomy_policy(&policy)?,
        "direct taxonomy policy is not canonical"
    );
    let revisions = index
        .sources
        .iter()
        .map(|source| (source.source_id.clone(), source.content_sha256.clone()))
        .collect::<BTreeMap<_, _>>();
    let assignments_path = root.join("taxonomy/assignments.json");
    let assignments =
        match crate::enrich::safe_read_bounded(root, &assignments_path, MAX_TAXONOMY_BYTES) {
            Ok(bytes) => {
                let assignments = parse_assignments(&bytes, &policy, &revisions)?;
                ensure!(
                    bytes == canonical_assignments(&assignments, &policy, &revisions)?,
                    "direct taxonomy assignments are not canonical"
                );
                assignments
            }
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound) =>
            {
                Assignments {
                    schema: TAXONOMY_ASSIGNMENTS_SCHEMA.into(),
                    policy_sha256: taxonomy_policy_digest(&policy)?,
                    assignments: Vec::new(),
                }
            }
            Err(error) => return Err(error).context("read bounded direct taxonomy assignments"),
        };
    let projection =
        crate::wiki_materialize::materialize_direct_sources(&index, &policy, &assignments)?;
    let pages = projection
        .pages
        .iter()
        .map(|page| {
            let source_digest = page
                .source_id
                .strip_prefix("src:")
                .context("direct derived page source id is invalid")?;
            ensure!(
                source_digest.len() == 64
                    && source_digest.bytes().all(|byte| byte.is_ascii_hexdigit())
                    && page.page_id == format!("source-{source_digest}"),
                "direct derived page id is invalid"
            );
            let path = root
                .join("content/provisional")
                .join(format!("{}.md", page.page_id));
            let bytes = crate::enrich::safe_read_bounded(root, &path, MAX_DERIVED_PAGE_BYTES)
                .with_context(|| format!("read derived page {}", page.page_id))?;
            validate_derived_page(page, &bytes)?;
            Ok((page.page_id.clone(), bytes))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    Ok((index, projection, policy, pages))
}

fn validate_derived_page(
    page: &crate::wiki_materialize::DirectSourcePage,
    bytes: &[u8],
) -> Result<()> {
    let markdown = std::str::from_utf8(bytes).context("direct derived page is not UTF-8")?;
    ensure!(!markdown.contains('\0'), "direct derived page contains NUL");
    let frontmatter = markdown
        .strip_prefix("---\n")
        .and_then(|markdown| markdown.split_once("\n---\n\n"))
        .context("direct derived page has no strict frontmatter")?;
    ensure!(
        !frontmatter.1.trim().is_empty(),
        "direct derived page has no Markdown body"
    );
    let mut fields = frontmatter.0.lines();
    let title = fields
        .next()
        .and_then(|line| line.strip_prefix("title: "))
        .context("direct derived page has no title")?;
    let title =
        serde_json::from_str::<String>(title).context("direct derived page title is invalid")?;
    ensure!(
        !title.trim().is_empty() && title.len() <= 160,
        "direct derived page title is invalid"
    );
    ensure!(
        fields.next() == Some(format!("source_id: {}", page.source_id).as_str()),
        "direct derived page source id does not match"
    );
    ensure!(
        fields.next() == Some(format!("content_sha256: {}", page.content_sha256).as_str()),
        "direct derived page digest does not match"
    );
    ensure!(
        fields.next() == Some("status: provisional"),
        "direct derived page status is invalid"
    );
    ensure!(
        fields.next().is_none(),
        "direct derived page frontmatter is unsupported"
    );
    Ok(())
}

fn preview_revision(
    index: &crate::wiki_source::SourceIndex,
    projection: &crate::wiki_materialize::DirectMaterialization,
    policy: &graphoxide_export::direct_taxonomy::TaxonomyPolicy,
    pages: &BTreeMap<String, Vec<u8>>,
) -> Result<String> {
    let mut digest = Sha256::new();
    digest.update(PREVIEW_SCHEMA.as_bytes());
    digest.update([0]);
    digest.update([PREVIEW_REVISION]);
    digest.update([0]);
    digest.update(serde_json::to_vec(index)?);
    digest.update([0]);
    digest.update(canonical_taxonomy_policy(policy)?);
    digest.update([0]);
    digest.update(serde_json::to_vec(projection)?);
    digest.update([0]);
    digest.update(serde_json::to_vec(pages)?);
    Ok(hex::encode(digest.finalize()))
}

fn checked_root(root: &Path) -> Result<PathBuf> {
    let metadata = fs::symlink_metadata(root)
        .with_context(|| format!("inspect direct knowledgebase root {}", root.display()))?;
    ensure!(
        metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
        "direct knowledgebase root must be a real directory"
    );
    root.canonicalize()
        .context("canonicalize direct knowledgebase root")
}

fn prepare_preview_root(root: &Path, output: &Path) -> Result<PathBuf> {
    let parent = output
        .parent()
        .context("direct preview output has no parent")?;
    let parent_metadata = fs::symlink_metadata(parent)
        .with_context(|| format!("inspect direct preview parent {}", parent.display()))?;
    ensure!(
        parent_metadata.file_type().is_dir() && !parent_metadata.file_type().is_symlink(),
        "direct preview output parent is unsafe"
    );
    let output = if output.exists() {
        let metadata = fs::symlink_metadata(output)?;
        ensure!(
            metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
            "direct preview output is unsafe"
        );
        output.canonicalize()?
    } else {
        fs::create_dir(output)?;
        output.canonicalize()?
    };
    ensure!(
        output.starts_with(root) || output.parent().is_some_and(|parent| parent.exists()),
        "direct preview output is unavailable"
    );
    let marker = output.join("manifest.json");
    if marker.exists() {
        let value: PreviewRoot = serde_json::from_slice(&read_regular(&marker, 64 * 1024)?)
            .context("parse direct preview root marker")?;
        ensure!(
            value.schema == PREVIEW_SCHEMA,
            "direct preview output is not owned"
        );
    } else {
        write_json_atomic_strict(
            &marker,
            &PreviewRoot {
                schema: PREVIEW_SCHEMA.into(),
            },
            true,
        )?;
    }
    Ok(output)
}

fn managed_directory(root: &Path, components: &[&str]) -> Result<PathBuf> {
    let mut directory = root.to_path_buf();
    for component in components {
        ensure!(
            !component.is_empty()
                && Path::new(component)
                    .components()
                    .all(|part| matches!(part, Component::Normal(_))),
            "direct Hugo path component is invalid"
        );
        directory.push(component);
        match fs::symlink_metadata(&directory) {
            Ok(metadata) => ensure!(
                metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
                "direct Hugo directory is unsafe"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&directory).with_context(|| {
                    format!("create direct Hugo directory {}", directory.display())
                })?
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(directory)
}

fn validate_preview_version(version: &Path, revision: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(version)?;
    ensure!(
        metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
        "direct preview revision is unsafe"
    );
    let manifest: PreviewManifest =
        serde_json::from_slice(&read_regular(&version.join("manifest.json"), 64 * 1024)?)
            .context("parse direct preview revision manifest")?;
    ensure!(
        manifest.schema == PREVIEW_SCHEMA
            && manifest.revision == revision
            && manifest.adapter_revision == PREVIEW_REVISION,
        "direct preview revision is unsupported"
    );
    ensure!(
        manifest.source_sha256 == tree_digest(&version.join("source"))?,
        "direct preview revision source changed"
    );
    Ok(())
}

fn write_hugo_config(source: &Path) -> Result<()> {
    write_bytes_atomic_strict(
        source.join("hugo.toml"),
        b"baseURL = \"/\"\nlanguageCode = \"en-us\"\ntitle = \"Graphoxide Knowledgebase\"\n[markup.goldmark.renderer]\nunsafe = false\n",
    )?;
    Ok(())
}

fn write_hugo_layout(source: &Path) -> Result<()> {
    let layouts = [
        ("layouts/_default/baseof.html", DIRECT_BASE_LAYOUT),
        ("layouts/_default/single.html", DIRECT_SINGLE_LAYOUT),
        ("layouts/_default/list.html", DIRECT_LIST_LAYOUT),
        ("layouts/search/list.html", DIRECT_SEARCH_LAYOUT),
        ("layouts/_partials/header.html", DIRECT_HEADER_PARTIAL),
        ("layouts/_partials/sidebar.html", DIRECT_SIDEBAR_PARTIAL),
        ("layouts/_partials/page-meta.html", DIRECT_PAGE_META_PARTIAL),
        (
            "layouts/_partials/page-navigation.html",
            DIRECT_PAGE_NAVIGATION_PARTIAL,
        ),
        (
            "layouts/_partials/page-outline.html",
            DIRECT_PAGE_OUTLINE_PARTIAL,
        ),
        ("layouts/_partials/footer.html", DIRECT_FOOTER_PARTIAL),
        ("assets/css/wiki.css", DIRECT_WIKI_CSS),
        ("assets/js/search.js", DIRECT_SEARCH_JS),
    ];
    for (relative, body) in layouts {
        let target = source.join(relative);
        fs::create_dir_all(target.parent().expect("layout asset has parent"))?;
        write_bytes_atomic_strict(target, body.as_bytes())?;
    }
    Ok(())
}

const DIRECT_BASE_LAYOUT: &str = r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>{{ .Title }} · {{ site.Title }}</title>{{ with resources.Get "css/wiki.css" }}<link rel="stylesheet" href="{{ .RelPermalink }}">{{ end }}</head><body>{{ partial "header.html" . }}<div class="shell{{ if .IsHome }} shell-home{{ end }}">{{ if not .IsHome }}<aside>{{ partial "sidebar.html" . }}</aside>{{ end }}<main>{{ block "main" . }}{{ .Content }}{{ end }}</main></div>{{ partial "footer.html" . }}{{ with resources.Get "js/search.js" }}<script defer src="{{ .RelPermalink }}"></script>{{ end }}</body></html>"#;

const DIRECT_HEADER_PARTIAL: &str = r#"{{ $sections := where (where site.Pages "Section" "domains") "Kind" "section" }}{{ $domains := where $sections "Title" "ne" "Domains" }}<header class="site-header"><div class="site-header-inner"><a class="site-brand" href="{{ "/" | relURL }}">{{ site.Title }}</a><p class="site-header-metrics" aria-label="Knowledgebase summary"><span>{{ len $domains }} domains</span><span>{{ len (where site.RegularPages "Section" "content") }} source pages</span><span>direct source index</span></p><div class="header-search"><form action="{{ "/search/" | relURL }}" role="search"><input id="wiki-search" name="q" type="search" placeholder="Search titles and subjects" aria-label="Search this knowledgebase" autocomplete="off" data-search-index="{{ "graphoxide/search.json" | relURL }}"><button type="submit">Search</button></form><nav id="wiki-search-results" aria-label="Search results" hidden><ul class="wiki-search-result-list"></ul></nav></div></div></header>"#;

const DIRECT_SIDEBAR_PARTIAL: &str = r#"{{ $sections := where (where site.Pages "Section" "domains") "Kind" "section" }}{{ $domains := where $sections "Title" "ne" "Domains" }}<nav class="sidebar" aria-label="Knowledgebase navigation"><strong>Domains</strong><ul>{{ range $domains.ByTitle }}<li><a href="{{ .RelPermalink }}">{{ .Title }}</a></li>{{ else }}<li>No domains are classified yet.</li>{{ end }}</ul><strong>Explore</strong><ul><li><a href="{{ "/browse/" | relURL }}">Facets and applicability</a></li><li><a href="{{ "/search/" | relURL }}">Search</a></li></ul><strong>Source workflow</strong><p class="sidebar-note">Pages are direct source pointers. Status shows whether a source remains provisional, reviewed, confirmed, or needs attention.</p></nav>"#;

const DIRECT_PAGE_META_PARTIAL: &str = r#"<dl class="metadata">{{ with .Params.primary_subject }}<dt>Subject</dt><dd><a href="{{ printf "/topics/%s/" . | relURL }}">{{ humanize . }}</a></dd>{{ end }}{{ range .Params.metadata_rows }}<dt>{{ .label }}</dt><dd>{{ range .values }}<span class="page-header-chip metadata-chip">{{ . }}</span>{{ end }}</dd>{{ end }}{{ with .Params.provenance }}<dt>Provenance</dt><dd><details class="metadata-provenance"><summary>Show source pointer</summary><code>{{ . }}</code></details></dd>{{ end }}</dl>"#;

const DIRECT_PAGE_NAVIGATION_PARTIAL: &str = r#"<nav class="page-navigation" aria-label="Page location"><a href="{{ "/" | relURL }}">Knowledgebase</a>{{ with .Params.primary_subject }}<span aria-hidden="true">/</span><a href="{{ printf "/topics/%s/" . | relURL }}">{{ humanize . }}</a>{{ end }}<span aria-hidden="true">/</span><span aria-current="page">{{ .Title }}</span></nav>"#;

const DIRECT_PAGE_OUTLINE_PARTIAL: &str = r#"{{ with .TableOfContents }}<section class="page-outline"><strong>On this page</strong>{{ . }}</section>{{ end }}"#;

const DIRECT_FOOTER_PARTIAL: &str = r#"<footer>Graphoxide knowledgebase · direct-source index · source material is never stored here.</footer>"#;

const DIRECT_SINGLE_LAYOUT: &str = r#"{{ define "main" }}<article><p class="eyebrow">Direct, pointer-indexed knowledge</p><h1>{{ .Title }} <span class="page-header-chip page-status-chip" aria-label="Source status"><strong>{{ humanize .Params.status }}</strong></span></h1>{{ with .Description }}<div class="summary">{{ . }}</div>{{ end }}{{ partial "page-meta.html" . }}{{ partial "page-navigation.html" . }}<div class="page-grid"><div class="page-body">{{ .Content }}</div><aside>{{ partial "page-outline.html" . }}</aside></div></article>{{ end }}"#;

const DIRECT_LIST_LAYOUT: &str = r#"{{ define "main" }}{{ if .IsHome }}{{ $sections := where (where site.Pages "Section" "domains") "Kind" "section" }}{{ $domains := where $sections "Title" "ne" "Domains" }}<article class="root-hub"><p class="eyebrow">Taxonomy navigator</p><h1>{{ .Title }}</h1><div class="summary">Browse direct-source knowledge by domain, then refine with facets and applicability.</div><nav class="root-actions" aria-label="Knowledgebase actions"><a class="root-action-primary" href="{{ "/search/" | relURL }}">Search</a><a href="{{ "/browse/" | relURL }}">Browse facets</a></nav><section><h2>Browse domains</h2><div class="knowledge-card-grid">{{ range $domains.ByTitle }}<article class="knowledge-card"><h3><a href="{{ .RelPermalink }}">{{ .Title }}</a></h3><p>Explore classified subjects and direct source pages.</p></article>{{ else }}<p>No domains are classified yet.</p>{{ end }}</div></section></article>{{ else }}<article class="taxonomy-hub"><p class="eyebrow">Taxonomy navigator</p><h1>{{ .Title }}</h1><div class="summary">{{ .Content }}</div>{{ with .Params.card_sections }}{{ range . }}<section><h2>{{ .title }}</h2><div class="knowledge-card-grid">{{ range .cards }}<article class="knowledge-card"><h3><a href="{{ .route }}">{{ .title }}</a></h3><p>{{ .description }}</p><small>{{ .count }} direct {{ if eq .count 1 }}page{{ else }}pages{{ end }}</small></article>{{ else }}<p>No direct sources are currently classified in this group.</p>{{ end }}</div></section>{{ end }}{{ end }}</article>{{ end }}{{ end }}"#;

const DIRECT_SEARCH_LAYOUT: &str = r#"{{ define "main" }}<article><p class="eyebrow">Direct source index</p><h1>Search</h1><p class="summary">Search titles and assigned subjects without opening source material.</p><input id="search-query" type="search" placeholder="Search titles and subjects" aria-label="Search this knowledgebase"><ul id="search-results">{{ range where site.RegularPages "Section" "content" }}<li data-search="{{ lower (printf "%s %s %s" .Title .Params.primary_subject .Params.status) }}"><a href="{{ .RelPermalink }}">{{ .Title }}</a><small>{{ humanize .Params.primary_subject }} · {{ humanize .Params.status }}</small></li>{{ end }}</ul></article><script>(()=>{const q=document.querySelector('#search-query'),r=[...document.querySelectorAll('#search-results li')],v=new URLSearchParams(location.search).get('q')||'';const f=()=>{const x=q.value.toLowerCase();r.forEach(e=>e.hidden=!e.dataset.search.includes(x))};q.value=v;q.addEventListener('input',f);f()})()</script>{{ end }}"#;

const DIRECT_SEARCH_JS: &str = r#"(()=>{const input=document.querySelector('#wiki-search'),results=document.querySelector('#wiki-search-results');if(!input||!results)return;let pages=[];const close=()=>{results.hidden=true;results.replaceChildren()};fetch(input.dataset.searchIndex).then(r=>r.ok?r.json():Promise.reject()).then(index=>{pages=Array.isArray(index.pages)?index.pages:[]}).catch(()=>{});input.addEventListener('input',()=>{const query=input.value.trim().toLowerCase();close();if(!query)return;const matches=pages.filter(page=>(`${page.title} ${page.subject} ${page.status}`).toLowerCase().includes(query)).slice(0,8);const list=document.createElement('ul');list.className='wiki-search-result-list';for(const page of matches){const item=document.createElement('li'),link=document.createElement('a');link.href=page.route;link.textContent=page.title;item.append(link);list.append(item)}if(matches.length){results.append(list);results.hidden=false}});input.addEventListener('keydown',event=>{if(event.key==='Escape')close()})})()"#;

const DIRECT_WIKI_CSS: &str = r#"body{margin:0;background:#f8fafc;color:#172033;font:16px system-ui,sans-serif;line-height:1.55}a{color:#0756a5}a:focus-visible,button:focus-visible,input:focus-visible{outline:2px solid #0756a5;outline-offset:2px}.site-header{background:linear-gradient(120deg,#11233d,#1e3a5f);color:#fff;border-bottom:1px solid #0b182a}.site-header-inner{display:grid;grid-template-columns:auto minmax(0,1fr) minmax(16rem,25rem);gap:.75rem 1.25rem;align-items:center;max-width:94rem;margin:0 auto;padding:.7rem 1.5rem}.site-brand{color:#fff;font-weight:750;text-decoration:none;white-space:nowrap}.site-header-metrics{display:flex;flex-wrap:wrap;gap:.2rem .7rem;align-items:center;margin:0;color:#d8e5f5;font-size:.75rem}.site-header-metrics span+span::before{content:"·";margin-right:.7rem;color:#8eabc9}.header-search{position:relative;min-width:0}.header-search form{display:flex;gap:.35rem}.header-search input{box-sizing:border-box;width:100%;min-width:0;margin:0;padding:.45rem .65rem;border:1px solid #abc2da;border-radius:.35rem;background:#fff;color:#172033;font:inherit}.header-search button{border:1px solid #abc2da;border-radius:.35rem;background:#e8f1fb;color:#172033;font:inherit;font-weight:700;padding:.4rem .65rem}.header-search button:hover{background:#fff}#wiki-search-results{position:absolute;z-index:10;top:calc(100% + .4rem);right:0;left:0;max-height:24rem;overflow:auto;border:1px solid #b9cde4;border-radius:.4rem;background:#fff;color:#172033;box-shadow:0 .75rem 1.75rem rgb(15 23 42 / .24)}#wiki-search-results[hidden]{display:none}.wiki-search-result-list{margin:0;padding:0;list-style:none}.wiki-search-result-list a{display:block;padding:.6rem .75rem;text-decoration:none}.wiki-search-result-list a:hover{background:#e8f1fb}.shell{display:grid;grid-template-columns:16rem minmax(0,72rem);gap:1.5rem;max-width:94rem;margin:0 auto;padding:1.5rem}.shell-home{display:block;max-width:80rem}.sidebar{font-size:.9rem}.sidebar strong{display:block;margin-top:1rem}.sidebar strong:first-child{margin-top:0}.sidebar ul{margin:.4rem 0;padding-left:1rem}.sidebar-note{color:#52657d;font-size:.82rem}main{min-width:0}article{background:#fff;border:1px solid #d7dee9;border-radius:.5rem;padding:2rem}.root-hub{padding:1.5rem 2rem}.eyebrow{margin:0 0 .4rem;color:#52657d;font-size:.82rem;font-weight:700;letter-spacing:.06em;text-transform:uppercase}h1{margin:.1rem 0 1rem;font-size:clamp(2rem,4vw,3.2rem);line-height:1.12}h2{margin-top:1.75rem}.summary{font-size:1.1rem}.root-actions{display:flex;flex-wrap:wrap;gap:.5rem;margin:1.25rem 0 1.5rem}.root-actions a{padding:.4rem .7rem;border:1px solid #b9cde4;border-radius:.35rem;background:#f8fafc;font-size:.9rem;font-weight:700;text-decoration:none}.root-actions .root-action-primary{border-color:#0756a5;background:#0756a5;color:#fff}.knowledge-card-grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(14rem,1fr));gap:1rem}.knowledge-card{min-height:12rem;padding:1rem;border:1px solid #d7dee9;border-radius:.4rem;background:#f8fafc}.knowledge-card h3{margin-top:0}.page-header-chips{display:flex;flex-wrap:wrap;gap:.4rem;margin-bottom:.75rem}.page-header-chip{display:inline-flex;gap:.35rem;padding:.12rem .5rem;border:1px solid #d7dee9;border-left:.25rem solid #64748b;border-radius:.3rem;background:#f8fafc;color:#52657d;font-size:.8rem;font-weight:600}.page-header-chip strong{color:#172033}.provisional-badge{border-left-color:#b45309}.metadata{display:grid;grid-template-columns:max-content 1fr;gap:.4rem 1rem;margin:1rem 0;padding:1rem;background:#f1f5f9}.metadata dt{font-weight:700}.metadata dd{margin:0;overflow-wrap:anywhere}.page-navigation{display:flex;flex-wrap:wrap;gap:.4rem;margin:1rem 0;padding:.75rem 1rem;border:1px solid #d7dee9;border-radius:.4rem;background:#f8fafc;font-size:.88rem}.page-grid{display:grid;grid-template-columns:minmax(0,1fr) 16rem;gap:2rem}.page-outline{border-top:1px solid #d7dee9;font-size:.9rem}.page-outline>strong{display:block;margin:.75rem 0}.page-outline ul{padding-left:1rem}#search-query{box-sizing:border-box;width:min(100%,38rem);padding:.55rem .7rem;border:1px solid #b9cde4;border-radius:.35rem;font:inherit}#search-results{padding-left:1.2rem}#search-results small{color:#52657d;margin-left:.35rem}footer{max-width:94rem;margin:0 auto;padding:2rem;color:#52657d;text-align:center}pre{overflow:auto;padding:1rem;border-radius:.4rem;background:#0f172a;color:#e2e8f0}table{display:block;max-width:100%;overflow:auto;border-collapse:collapse}th,td{padding:.55rem .7rem;border:1px solid #d7dee9;text-align:left;vertical-align:top}th{background:#e8eef6}@media(max-width:1100px){.site-header-inner{grid-template-columns:1fr}.header-search{max-width:38rem}}@media(max-width:800px){.shell,.page-grid{display:block}.sidebar{margin-bottom:1.25rem}.page-grid aside{margin-top:2rem}.site-header-inner{padding:.75rem 1rem}.shell{padding:1rem}.root-hub,article{padding:1.25rem}}"#;

/// Write deterministic navigation and pointer-only page metadata into a Hugo source tree.
pub fn write_direct_navigation(
    source: &Path,
    projection: &crate::wiki_materialize::DirectMaterialization,
    pages: &BTreeMap<String, Vec<u8>>,
) -> Result<()> {
    let content = source.join("content");
    fs::create_dir_all(&content)?;
    let by_id = projection
        .pages
        .iter()
        .map(|page| (page.page_id.as_str(), page))
        .collect::<BTreeMap<_, _>>();
    let mut root =
        String::from("Browse direct-source knowledge by subject, facet, and applicability.\n\n");
    root.push_str("## Explore domains\n\n");
    for group in &projection.domains {
        root.push_str(&group_link(group)?);
    }
    root.push_str("\n## Browse by facet\n\n");
    for axis in &projection.facets {
        root.push_str(&format!("### {}\n\n", markdown_text(&axis.label)?));
        for group in &axis.terms {
            root.push_str(&group_link(group)?);
        }
    }
    root.push_str("\n## Applicability\n\n");
    for group in &projection.applicability {
        root.push_str(&group_link(group)?);
    }
    write_page(&content, "/", "Knowledgebase", &root)?;
    let mut browse_sections = projection
        .facets
        .iter()
        .map(|axis| DirectCardSection {
            title: axis.label.clone(),
            cards: axis.terms.iter().map(group_card).collect::<Vec<_>>(),
        })
        .collect::<Vec<_>>();
    browse_sections.push(DirectCardSection {
        title: "Applicability".into(),
        cards: projection.applicability.iter().map(group_card).collect(),
    });
    write_hub_page(
        &content,
        "/browse/",
        "Browse",
        "Refine direct-source knowledge by facet or applicability.",
        browse_sections,
    )?;
    write_page(
        &content,
        "/search/",
        "Search",
        "Search direct source titles and assigned subjects.\n",
    )?;
    for group in &projection.domains {
        write_domain_page(&content, group, &projection.subjects)?;
    }
    for group in projection
        .subjects
        .iter()
        .chain(projection.applicability.iter())
        .chain(projection.facets.iter().flat_map(|axis| axis.terms.iter()))
    {
        write_group_page(&content, group, &by_id, pages)?;
    }
    for page in &projection.pages {
        write_derived_page(
            &content,
            page,
            pages
                .get(&page.page_id)
                .context("direct preview has no derived page")?,
        )?;
    }
    write_direct_search_index(source, projection, pages)?;
    Ok(())
}

fn write_direct_search_index(
    source: &Path,
    projection: &crate::wiki_materialize::DirectMaterialization,
    derived_pages: &BTreeMap<String, Vec<u8>>,
) -> Result<()> {
    let static_root = source.join("static/graphoxide");
    fs::create_dir_all(&static_root)?;
    let pages = projection
        .pages
        .iter()
        .map(|page| {
            Ok(DirectSearchEntry {
                title: derived_title(
                    derived_pages
                        .get(&page.page_id)
                        .context("direct preview has no derived page")?,
                )?,
                route: &page.route,
                subject: &page.primary_subject,
                status: status_id(&page.status),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    write_json_atomic_strict(
        static_root.join("search.json"),
        &DirectSearchIndex { pages },
        true,
    )
}

fn write_derived_page(
    content: &Path,
    page: &crate::wiki_materialize::DirectSourcePage,
    bytes: &[u8],
) -> Result<()> {
    let markdown = std::str::from_utf8(bytes).context("direct derived page is not UTF-8")?;
    let (frontmatter, body) = markdown
        .split_once("\n---\n\n")
        .context("direct derived page has no strict frontmatter")?;
    let frontmatter = frontmatter.replacen(
        "status: provisional",
        &format!("status: {}", status_id(&page.status)),
        1,
    );
    let mut metadata_rows = page
        .facets
        .iter()
        .map(|(axis, terms)| DirectMetadataRow {
            label: title_from_id(axis),
            values: terms.iter().map(|term| title_from_id(term)).collect(),
        })
        .collect::<Vec<_>>();
    if !page.applicability.is_empty() {
        metadata_rows.push(DirectMetadataRow {
            label: "Applicability".into(),
            values: page
                .applicability
                .iter()
                .map(|term| title_from_id(term))
                .collect(),
        });
    }
    let metadata_rows =
        serde_json::to_string(&metadata_rows).context("serialize direct preview metadata rows")?;
    let path = content
        .join(route_to_content_path(&page.route)?)
        .with_file_name("index.md");
    fs::create_dir_all(path.parent().context("direct page has no parent")?)?;
    write_bytes_atomic_strict(
        path,
        format!(
            "{frontmatter}\nprimary_subject: {}\nprovenance: {}\nmetadata_rows: {metadata_rows}\n---\n\n{body}",
            toml_string(&page.primary_subject),
            toml_string(&provenance_label(&page.location)),
        )
        .as_bytes(),
    )?;
    Ok(())
}

fn write_group_page(
    content: &Path,
    group: &crate::wiki_materialize::DirectBrowseGroup,
    pages: &BTreeMap<&str, &crate::wiki_materialize::DirectSourcePage>,
    derived_pages: &BTreeMap<String, Vec<u8>>,
) -> Result<()> {
    let mut cards = Vec::new();
    for page_id in &group.page_ids {
        let page = pages
            .get(page_id.as_str())
            .context("direct browse group references an unknown page")?;
        let title = derived_title(
            derived_pages
                .get(page_id)
                .context("direct preview has no derived page")?,
        )?;
        cards.push(DirectNavigationCard {
            title,
            route: page.route.clone(),
            description: format!("Direct source page · {}", status_label(&page.status)),
            count: 1,
        });
    }
    write_hub_page(
        content,
        &group.route,
        &group.label,
        "Explore direct source pages in this classification.",
        vec![DirectCardSection {
            title: "Direct source pages".into(),
            cards,
        }],
    )
}

fn write_domain_page(
    content: &Path,
    domain: &crate::wiki_materialize::DirectBrowseGroup,
    subjects: &[crate::wiki_materialize::DirectBrowseGroup],
) -> Result<()> {
    let cards = subjects
        .iter()
        .filter(|subject| subject.id.starts_with(&format!("{}/", domain.id)))
        .map(group_card)
        .collect();
    write_hub_page(
        content,
        &domain.route,
        &domain.label,
        "Explore subjects classified in this domain.",
        vec![DirectCardSection {
            title: "Subjects".into(),
            cards,
        }],
    )
}

fn group_card(group: &crate::wiki_materialize::DirectBrowseGroup) -> DirectNavigationCard {
    let count = group.page_ids.len();
    DirectNavigationCard {
        title: group.label.clone(),
        route: group.route.clone(),
        description: format!(
            "{count} classified direct source {}.",
            if count == 1 { "page" } else { "pages" }
        ),
        count,
    }
}

fn derived_title(bytes: &[u8]) -> Result<String> {
    let markdown = std::str::from_utf8(bytes).context("direct derived page is not UTF-8")?;
    let frontmatter = markdown
        .strip_prefix("---\n")
        .and_then(|markdown| markdown.split_once("\n---\n\n"))
        .context("direct derived page has no strict frontmatter")?
        .0;
    let title = frontmatter
        .lines()
        .find_map(|line| line.strip_prefix("title: "))
        .context("direct derived page has no title")?;
    serde_json::from_str(title).context("direct derived page title is invalid")
}

fn status_label(status: &crate::wiki_source::SourceStatus) -> &'static str {
    match status {
        crate::wiki_source::SourceStatus::Provisional => "provisional",
        crate::wiki_source::SourceStatus::AiReviewed => "AI reviewed",
        crate::wiki_source::SourceStatus::HumanConfirmed => "human confirmed",
        crate::wiki_source::SourceStatus::StaleError => "stale or error",
    }
}

fn status_id(status: &crate::wiki_source::SourceStatus) -> &'static str {
    match status {
        crate::wiki_source::SourceStatus::Provisional => "provisional",
        crate::wiki_source::SourceStatus::AiReviewed => "ai-reviewed",
        crate::wiki_source::SourceStatus::HumanConfirmed => "human-confirmed",
        crate::wiki_source::SourceStatus::StaleError => "stale-error",
    }
}

fn provenance_label(location: &crate::wiki_source::SourceLocation) -> String {
    match location {
        crate::wiki_source::SourceLocation::Git {
            remote,
            commit,
            path,
        } => {
            format!("Git {remote} at {commit}: {path}")
        }
        crate::wiki_source::SourceLocation::Https { url } => format!("HTTPS {url}"),
        crate::wiki_source::SourceLocation::BoundPath { .. } => "Local bound source".into(),
    }
}

fn group_link(group: &crate::wiki_materialize::DirectBrowseGroup) -> Result<String> {
    Ok(format!(
        "- [{}]({})\n",
        markdown_text(&group.label)?,
        group.route
    ))
}

fn write_hub_page(
    content: &Path,
    route: &str,
    title: &str,
    body: &str,
    card_sections: Vec<DirectCardSection>,
) -> Result<()> {
    let relative = route_to_content_path(route)?;
    let path = content.join(relative);
    fs::create_dir_all(path.parent().context("direct hub has no parent")?)?;
    let frontmatter = toml::to_string(&DirectHubPage {
        title: title.into(),
        card_sections,
    })
    .context("serialize direct hub frontmatter")?;
    write_bytes_atomic_strict(
        path,
        format!("+++\n{frontmatter}+++\n\n{body}\n").as_bytes(),
    )
}

fn write_page(content: &Path, route: &str, title: &str, body: &str) -> Result<()> {
    let relative = route_to_content_path(route)?;
    let path = content.join(relative);
    fs::create_dir_all(path.parent().context("direct page has no parent")?)?;
    let title = toml_string(title);
    write_bytes_atomic_strict(
        path,
        format!("---\ntitle: {title}\n---\n\n{body}").as_bytes(),
    )?;
    Ok(())
}

fn route_to_content_path(route: &str) -> Result<PathBuf> {
    let trimmed = route.trim_matches('/');
    ensure!(
        route.starts_with('/')
            && route.ends_with('/')
            && trimmed.split('/').all(|part| part.is_empty()
                || Path::new(part)
                    .components()
                    .all(|item| matches!(item, Component::Normal(_)))),
        "direct route is invalid"
    );
    Ok(if trimmed.is_empty() {
        PathBuf::from("_index.md")
    } else {
        PathBuf::from(trimmed).join("_index.md")
    })
}

fn markdown_text(value: &str) -> Result<String> {
    ensure!(
        !value.contains(['\0', '\r', '\n']),
        "direct preview text must be one line"
    );
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if matches!(
            character,
            '\\' | '`' | '*' | '_' | '[' | ']' | '(' | ')' | '<' | '>' | '#'
        ) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    Ok(escaped)
}

fn toml_string(value: &str) -> String {
    format!("{:?}", value)
}

fn title_from_id(value: &str) -> String {
    value
        .split('-')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn read_regular(path: &Path, cap: usize) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect direct preview artifact {}", path.display()))?;
    ensure!(
        metadata.file_type().is_file()
            && !metadata.file_type().is_symlink()
            && metadata.len() <= cap as u64,
        "direct preview artifact is unsafe"
    );
    fs::read(path).with_context(|| format!("read direct preview artifact {}", path.display()))
}

fn tree_digest(root: &Path) -> Result<String> {
    let mut files = BTreeSet::new();
    collect_files(root, root, &mut files)?;
    let mut digest = Sha256::new();
    for relative in files {
        digest.update(relative.as_bytes());
        digest.update([0]);
        digest.update(read_regular(&root.join(&relative), 8 * 1024 * 1024)?);
        digest.update([0]);
    }
    Ok(hex::encode(digest.finalize()))
}

fn collect_files(root: &Path, directory: &Path, files: &mut BTreeSet<String>) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
            collect_files(root, &path, files)?;
        } else if metadata.file_type().is_file() && !metadata.file_type().is_symlink() {
            let relative = path
                .strip_prefix(root)
                .context("direct preview file escaped root")?
                .to_str()
                .context("direct preview path is not UTF-8")?
                .replace('\\', "/");
            files.insert(relative);
        } else {
            anyhow::bail!("direct preview contains an unsafe path");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wiki_source::{
        write_source_index, SourceEntry, SourceIndex, SourceLocation, SourceStatus,
    };
    use graphoxide_export::direct_taxonomy::{
        canonical_assignments, canonical_taxonomy_policy, default_taxonomy_policy,
        produce_assignments, AssignmentInput,
    };

    #[cfg(unix)]
    #[test]
    fn direct_preview_is_pointer_only_and_deterministic() {
        let root = tempfile::tempdir().expect("knowledgebase root");
        let location = SourceLocation::BoundPath {
            binding: "fixture-root".into(),
            path: "protocols/control.md".into(),
        };
        let source = SourceEntry {
            source_id: location.source_id(),
            location,
            content_sha256: "a".repeat(64),
            bytes: 7,
            status: SourceStatus::AiReviewed,
        };
        write_source_index(
            root.path(),
            &SourceIndex {
                schema: "graphoxide.source-index".into(),
                sources: vec![source.clone()],
            },
        )
        .expect("source index");
        let policy = default_taxonomy_policy();
        fs::create_dir_all(root.path().join("taxonomy")).expect("taxonomy directory");
        fs::write(
            root.path().join("taxonomy/policy.json"),
            canonical_taxonomy_policy(&policy).expect("policy"),
        )
        .expect("write policy");
        let assignments = produce_assignments(
            &policy,
            &BTreeMap::from([(source.source_id.clone(), source.content_sha256.clone())]),
            std::iter::once(AssignmentInput {
                source_id: source.source_id.clone(),
                content_sha256: source.content_sha256.clone(),
                primary_subject: "interfaces-and-protocols/grpc".into(),
                facets: BTreeMap::from([("artifact-kind".into(), vec!["specification".into()])]),
                applicability: vec!["software".into()],
                page_ids: vec![format!(
                    "source-{}",
                    source.source_id.strip_prefix("src:").expect("source id")
                )],
            }),
        )
        .expect("assignments");
        fs::write(
            root.path().join("taxonomy/assignments.json"),
            canonical_assignments(
                &assignments,
                &policy,
                &BTreeMap::from([(source.source_id.clone(), source.content_sha256.clone())]),
            )
            .expect("canonical assignments"),
        )
        .expect("write assignments");
        fs::create_dir_all(root.path().join("content/provisional")).expect("derived directory");
        let derived = root.path().join("content/provisional").join(format!(
            "source-{}.md",
            source.source_id.strip_prefix("src:").expect("source id")
        ));
        let output_parent = tempfile::tempdir().expect("output parent");
        let output = output_parent.path().join("preview");
        let second_output = output_parent.path().join("preview-second");
        let hugo = fake_hugo(output_parent.path(), "hugo v0.165.0");

        fs::write(&derived, "RAW-SOURCE-SENTINEL").expect("invalid derived page");
        assert!(materialize_direct_preview_with_hugo(root.path(), &output, &hugo).is_err());
        fs::write(
            &derived,
            format!(
                "---\ntitle: \"Foreign page\"\nsource_id: src:{}\ncontent_sha256: {}\nstatus: provisional\n---\n\n## Foreign content.\n",
                "b".repeat(64),
                source.content_sha256
            ),
        )
        .expect("foreign derived page");
        assert!(materialize_direct_preview_with_hugo(root.path(), &output, &hugo).is_err());
        fs::write(
            &derived,
            format!(
                "---\ntitle: \"Derived [page](unsafe)\"\nsource_id: {}\ncontent_sha256: {}\nstatus: provisional\n---\n\n## A derived explanation.\n",
                source.source_id, source.content_sha256
            ),
        )
        .expect("derived page");

        let first = materialize_direct_preview_with_hugo(root.path(), &output, &hugo)
            .expect("first preview");
        let second = materialize_direct_preview_with_hugo(root.path(), &second_output, &hugo)
            .expect("second preview");

        assert_eq!(
            tree_digest(&first.source).expect("first source digest"),
            tree_digest(&second.source).expect("second source digest")
        );
        let tree = fs::read_to_string(first.source.join("content/content").join(format!(
            "source-{}/index.md",
            source.source_id.strip_prefix("src:").expect("source id")
        )))
        .unwrap_or_default();
        assert!(tree.contains("## A derived explanation."));
        assert!(!tree.contains("RAW-SOURCE-SENTINEL"));
        let home =
            fs::read_to_string(first.source.join("content/_index.md")).expect("home navigation");
        assert!(home.contains("[Interfaces and Protocols](/domains/interfaces-and-protocols/)"));
        assert!(home.contains("[Software](/browse/applicability/software/)"));
        assert!(home.contains("[Specification](/browse/artifact-kind/specification/)"));
        let browse = fs::read_to_string(first.source.join("content/browse/_index.md"))
            .expect("browse navigation");
        assert!(browse.starts_with("+++\n"));
        assert!(browse.contains("route = \"/browse/artifact-kind/specification/\""));
        assert!(browse.contains("card_sections"));
        assert!(!browse.contains("- [Specification]"));
        assert!(first.source.join("content/search/_index.md").is_file());
        let domain = fs::read_to_string(
            first
                .source
                .join("content/domains/interfaces-and-protocols/_index.md"),
        )
        .expect("domain navigation");
        assert!(domain.contains("title = \"Subjects\""));
        assert!(domain.contains("route = \"/topics/interfaces-and-protocols/grpc/\""));
        assert!(domain.contains("card_sections"));
        assert!(!domain.contains("- [gRPC]"));
        assert!(!domain.contains("Derived page"));
        let subject = fs::read_to_string(
            first
                .source
                .join("content/topics/interfaces-and-protocols/grpc/_index.md"),
        )
        .expect("subject navigation");
        assert!(subject.contains("title = \"Derived [page](unsafe)\""));
        assert!(subject.contains("route = \"/content/source-"));
        assert!(!subject.contains("Source A"));
        assert!(subject.contains("card_sections"));
        assert!(!subject.contains("- [Derived"));
        assert!(tree.contains("primary_subject: \"interfaces-and-protocols/grpc\""));
        assert!(tree.contains("metadata_rows:"));
        assert!(tree.contains("\"label\":\"Artifact Kind\""));
        assert!(tree.contains("\"values\":[\"Specification\"]"));
        assert!(tree.contains("\"label\":\"Applicability\""));
        assert!(!tree.contains("> **Source status:**"));
        assert!(tree.contains("status: ai-reviewed"));
        assert!(tree.contains("Local bound source"));
        assert!(!tree.contains("fixture-root"));
        let layout = fs::read_to_string(first.source.join("layouts/_default/single.html"))
            .expect("source page layout");
        assert!(layout.contains("page-status-chip"));
        let metadata = fs::read_to_string(first.source.join("layouts/_partials/page-meta.html"))
            .expect("source metadata partial");
        assert!(metadata.contains(".Params.metadata_rows"));
        assert!(metadata.contains("<details class=\"metadata-provenance\">"));
        assert!(metadata.contains("metadata-chip"));
        assert!(!layout.contains("Publication"));
        let base = fs::read_to_string(first.source.join("layouts/_default/baseof.html"))
            .expect("base layout");
        assert!(base.contains("{{ if not .IsHome }}<aside>"));
        let header = fs::read_to_string(first.source.join("layouts/_partials/header.html"))
            .expect("header partial");
        assert!(header.contains("action=\"{{ \"/search/\" | relURL }}\""));
        assert!(first.source.join("layouts/search/list.html").is_file());
        assert!(first.source.join("layouts/_partials/header.html").is_file());
        assert!(first
            .source
            .join("layouts/_partials/sidebar.html")
            .is_file());
        assert!(first
            .source
            .join("layouts/_partials/page-meta.html")
            .is_file());
        assert!(first
            .source
            .join("layouts/_partials/page-navigation.html")
            .is_file());
        assert!(first
            .source
            .join("layouts/_partials/page-outline.html")
            .is_file());
        assert!(first.source.join("layouts/_partials/footer.html").is_file());
        assert!(first.source.join("assets/css/wiki.css").is_file());
        assert!(first.source.join("assets/js/search.js").is_file());
        let search_index = fs::read_to_string(first.source.join("static/graphoxide/search.json"))
            .expect("local search index");
        assert!(search_index.contains("Derived [page](unsafe)"));
        assert!(!search_index.contains("RAW-SOURCE-SENTINEL"));
        assert!(base.contains("partial \"header.html\""));
        assert!(base.contains("partial \"sidebar.html\""));
        let list = fs::read_to_string(first.source.join("layouts/_default/list.html"))
            .expect("taxonomy hub layout");
        assert!(list.contains("Browse domains"));
        assert!(list.contains(".Params.card_sections"));
        assert!(
            !list.contains("<h1>{{ .Title }}</h1><div class=\"summary\">{{ .Content }}</div><nav")
        );
        let all = format!("{:?}", walk(&first.source));
        assert!(!all.contains("/home/"));
        assert!(!all.contains("fixture-root"));
        let rendered = walk(&first.source)
            .into_iter()
            .map(|path| fs::read_to_string(path).unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!rendered.contains("RAW-SOURCE-SENTINEL"));
        assert!(!rendered.contains("fixture-root"));
        assert!(!rendered.contains("/home/"));
    }

    #[cfg(unix)]
    #[test]
    fn direct_preview_rejects_an_unpinned_hugo_binary() {
        let directory = tempfile::tempdir().expect("tool directory");
        let hugo = fake_hugo(directory.path(), "hugo v0.161.1");
        let error = validate_hugo_binary(&hugo).expect_err("unpinned Hugo must fail");
        assert!(format!("{error:#}").contains("pinned version"));
    }

    #[cfg(unix)]
    fn fake_hugo(directory: &Path, version: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;

        let path = directory.join("hugo");
        fs::write(&path, format!("#!/bin/sh\nprintf '%s\\n' '{version}'\n")).expect("fake Hugo");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("fake Hugo mode");
        path
    }

    fn walk(root: &Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        fn visit(path: &Path, files: &mut Vec<PathBuf>) {
            for entry in fs::read_dir(path).expect("read directory") {
                let path = entry.expect("entry").path();
                if path.is_dir() {
                    visit(&path, files);
                } else {
                    files.push(path);
                }
            }
        }
        visit(root, &mut files);
        files
    }
}
