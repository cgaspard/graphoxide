//! Deterministic build-time renderer for Graphoxide agent skills.
//!
//! The renderer keeps the default extraction workflow in a lean core and emits
//! infrequently used operations as eight references.  Platform differences are
//! data, not copied Markdown, so every host receives the same schema and safety
//! fixes.  The module also owns drift, heading-coverage, and schema audits.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::Path;
use std::process::Command;

pub const ENUM_VALUES: &str = "code|document|paper|image|rationale|concept";
pub const ENUM_PROSE: &str = "`code`, `document`, `paper`, `image`, `rationale`, `concept`";
pub const UNIFIED_DESCRIPTION: &str = "Use for any question about a codebase, its architecture, file relationships, or project content — especially when graphoxide-out/ exists, where the question should be treated as a graphoxide query first. Turns code, docs, papers, images, and videos into a persistent knowledge graph with community detection and query/path/explain tools.";

/// Provenance of the pre-split Graphify skill bodies whose contracts this port
/// preserves.  Keeping the immutable references visible prevents a moving
/// branch from turning coverage audits into comparisons against themselves.
pub const GRAPHIFY_V8_BASELINE_SHA: &str = "47042beb05d1f6dd2186c0c499ae2840ce604ead";
pub const ALWAYS_ON_BASELINE_REF: &str =
    "47042beb05d1f6dd2186c0c499ae2840ce604ead:graphify/__main__.py";

pub const OLD_AGENTS_INSTRUCTION: &str = "When the user types `/graphify`, invoke the `skill` tool with `skill: \"graphify\"` before doing anything else.";
pub const NEW_AGENTS_INSTRUCTION: &str = "When the user types `/graphoxide`, use the installed graphoxide skill or instructions before doing anything else.";

pub const SHARED_INTRO_ALLOWLIST: &[&str] = &["## What graphoxide is for"];
pub const KILO_CONSOLIDATIONS: &[&str] = &[
    "### Step 2.5 - Transcribe video or audio files (only if video files were detected)",
    "#### Part B - Semantic extraction for docs, papers, and images",
    "#### Part C - Merge AST and semantic extraction",
    "### Step 4 - Build the graph and generate outputs",
    "### Step 5 - Save manifest, clean up, and report",
    "### Query mode",
    "### Kilo-specific rules",
];
pub const VSCODE_CONSOLIDATIONS: &[&str] = &[
    "#### Part A - Structural extraction (AST, free, no API cost)",
    "#### Part B - Semantic extraction (AI, costs tokens)",
    "### Step 4 - Build graph and cluster",
    "### Step 5 - Generate report and visualization",
    "### After completing all steps",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Bucket {
    Split,
    Monolith,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExtractionVariant {
    Verbose,
    Compact,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Shell {
    Posix,
    PowerShell,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HooksVariant {
    ClaudeMd,
    AgentsMd,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Dispatch {
    AgentToolDisk,
    CodexAgentTask,
    OpenCodeMention,
    TaskToolDisk,
    TaskToolDiskTrae,
    ManualPaste,
    PowerShellAgent,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Platform {
    pub key: &'static str,
    pub bucket: Bucket,
    pub skill_dst: String,
    pub refs_dst: Option<String>,
    pub dispatch: Dispatch,
    pub extraction: ExtractionVariant,
    pub shell: Shell,
    pub hooks_variant: HooksVariant,
    pub extra_sections: Vec<&'static str>,
}

impl Platform {
    fn split(key: &'static str, dispatch: Dispatch, extraction: ExtractionVariant) -> Self {
        Self {
            key,
            bucket: Bucket::Split,
            skill_dst: if key == "claude" {
                "graphoxide/skill.md".to_owned()
            } else {
                format!("graphoxide/skill-{key}.md")
            },
            refs_dst: Some(format!("graphoxide/skills/{key}/references")),
            dispatch,
            extraction,
            shell: Shell::Posix,
            hooks_variant: HooksVariant::ClaudeMd,
            extra_sections: Vec::new(),
        }
    }

    fn monolith(key: &'static str) -> Self {
        Self {
            key,
            bucket: Bucket::Monolith,
            skill_dst: format!("graphoxide/skill-{key}.md"),
            refs_dst: None,
            dispatch: Dispatch::AgentToolDisk,
            extraction: ExtractionVariant::Verbose,
            shell: Shell::Posix,
            hooks_variant: HooksVariant::ClaudeMd,
            extra_sections: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenderedArtifact {
    pub path: String,
    pub content: String,
}

impl RenderedArtifact {
    pub fn new(path: impl Into<String>, content: impl AsRef<str>) -> Self {
        Self {
            path: path.into(),
            content: normalise(content.as_ref()),
        }
    }
}

pub fn platforms() -> BTreeMap<&'static str, Platform> {
    use Dispatch::*;
    use ExtractionVariant::*;

    let mut result = BTreeMap::new();
    for platform in [
        Platform::split("claude", AgentToolDisk, Verbose),
        Platform::split("codex", CodexAgentTask, Compact),
        Platform::split("opencode", OpenCodeMention, Verbose),
        Platform::split("kilo", AgentToolDisk, Verbose),
        Platform::split("copilot", AgentToolDisk, Verbose),
        Platform::split("claw", AgentToolDisk, Compact),
        Platform::split("droid", TaskToolDisk, Verbose),
        Platform::split("amp", TaskToolDisk, Verbose),
        Platform::split("agents", TaskToolDisk, Verbose),
        Platform::split("trae", TaskToolDiskTrae, Verbose),
        Platform::split("kiro", AgentToolDisk, Compact),
        Platform::split("pi", AgentToolDisk, Compact),
        Platform::split("vscode", ManualPaste, Verbose),
    ] {
        result.insert(platform.key, platform);
    }

    let mut windows = Platform::split("windows", PowerShellAgent, Verbose);
    windows.shell = Shell::PowerShell;
    windows.extra_sections.push("powershell-troubleshooting");
    result.insert(windows.key, windows);

    result
        .get_mut("kilo")
        .expect("kilo platform")
        .extra_sections
        .push("kilo-rules");
    for key in ["amp", "agents", "trae"] {
        result
            .get_mut(key)
            .expect("agents-md platform")
            .hooks_variant = HooksVariant::AgentsMd;
    }

    for platform in [Platform::monolith("aider"), Platform::monolith("devin")] {
        result.insert(platform.key, platform);
    }
    result
}

pub fn coverage_baseline_ref(platform_key: &str) -> String {
    let source_key = if platform_key == "agents" {
        "amp"
    } else {
        platform_key
    };
    if source_key == "claude" {
        format!("{GRAPHIFY_V8_BASELINE_SHA}:graphify/skill.md")
    } else {
        format!("{GRAPHIFY_V8_BASELINE_SHA}:graphify/skill-{source_key}.md")
    }
}

pub fn consolidation_allowlist(platform_key: &str) -> &'static [&'static str] {
    match platform_key {
        "kilo" => KILO_CONSOLIDATIONS,
        "vscode" => VSCODE_CONSOLIDATIONS,
        _ => &[],
    }
}

fn normalise(text: &str) -> String {
    let lf = text.replace("\r\n", "\n").replace('\r', "\n");
    format!("{}\n", lf.trim_end_matches('\n'))
}

fn frontmatter(extra: &str) -> String {
    format!("---\nname: graphoxide\ndescription: \"{UNIFIED_DESCRIPTION}\"\n{extra}---\n")
}

fn install_section(platform: &Platform) -> &'static str {
    match platform.shell {
        Shell::Posix => {
            "```sh\ncommand -v graphoxide >/dev/null || cargo install --git https://github.com/cgaspard/graphoxide graphoxide-cli\ngraphoxide --version\n```"
        }
        Shell::PowerShell => {
            "```powershell\nfunction Find-GraphoxideBinary { Get-Command graphoxide -ErrorAction SilentlyContinue }\nif (-not (Find-GraphoxideBinary)) { cargo install --git https://github.com/cgaspard/graphoxide graphoxide-cli }\ngraphoxide --version\n```"
        }
    }
}

fn dispatch_section(platform: &Platform) -> &'static str {
    match platform.dispatch {
        Dispatch::AgentToolDisk => {
            "Dispatch one host agent per semantic chunk and collect its JSON result on disk."
        }
        Dispatch::CodexAgentTask => {
            "Enable `multi_agent = true`, then use `spawn_agent`, `wait_agent`, and `close_agent`. Codex collects in memory; validate every returned JSON value before merging."
        }
        Dispatch::OpenCodeMention => {
            "Use an OpenCode `@mention` for each semantic chunk and collect each JSON response."
        }
        Dispatch::TaskToolDisk => {
            "Dispatch each chunk with `Task(description=...)` and collect validated JSON on disk."
        }
        Dispatch::TaskToolDiskTrae => {
            "Dispatch each chunk with `Task(description=...)` and collect validated JSON on disk. Trae does NOT support PreToolUse hooks; AGENTS.md rules are the always-on mechanism instead."
        }
        Dispatch::ManualPaste => {
            "Open one chat per semantic chunk, then paste each response back and validate it before merging."
        }
        Dispatch::PowerShellAgent => {
            "Dispatch each semantic chunk with the host Agent tool and save each JSON response with UTF-8 PowerShell I/O."
        }
    }
}

fn hooks_target(platform: &Platform) -> &'static str {
    match platform.hooks_variant {
        HooksVariant::ClaudeMd => "CLAUDE.md",
        HooksVariant::AgentsMd => "AGENTS.md",
    }
}

fn render_core(platform: &Platform) -> String {
    let mut body = frontmatter("");
    body.push_str(
        "\n# Graphoxide\n\n## What graphoxide is for\n\nUse the graph before rereading a repository. Build or update it when the persisted graph is missing or stale.\n\n## Usage\n\n```text\ngraphoxide extract INPUT_PATH\ngraphoxide update INPUT_PATH\ngraphoxide query \"QUESTION\"\ngraphoxide path NODE_A NODE_B\ngraphoxide explain NODE\n```\n\n### Step 1 - Ensure graphoxide is installed\n\n",
    );
    body.push_str(install_section(platform));
    body.push_str(
        "\n\n### Step 2 - Detect files\n\nRun `graphoxide audit INPUT_PATH --json` to inspect discovery and extraction loss before a costly rebuild.\n\n### Step 3 - Extract entities and relationships\n\n> **graphoxide needs no API key for its default extraction. Never ask the user for one, and never block on one.** Run `graphoxide extract INPUT_PATH` when the host cannot dispatch subagents. Optional semantic enrichment may use host agents, but structural extraction remains complete and deterministic without them.\n\nTip: set `GEMINI_API_KEY` only when the user explicitly chooses an optional compatible labeling backend.\n\n#### Part A - Structural extraction for code files\n\nRun `graphoxide extract INPUT_PATH`; it performs language-aware structural extraction, resolution, graph construction, and clustering locally.\n\n#### Part B - Optional semantic extraction\n\n`SPEC_PATH` below is the **absolute** path to `references/extraction-spec.md`. Semantic-cache reads and writes must both bind `prompt_file='SPEC_PATH'` so prompt revisions never replay stale facts.\n\n`check_semantic_cache(files, prompt_file='SPEC_PATH')`\n\n**Step B2 - Dispatch uncached semantic chunks**\n\n",
    );
    body.push_str(dispatch_section(platform));
    body.push_str(
        "\n\nPass the extraction prompt and only the assigned source content to each worker.\n\n**Step B3 - Validate and cache results**\n\n`save_semantic_cache(results, prompt_file='SPEC_PATH', allowed_source_files=uncached)`\n\n#### Part C - Merge AST + semantic into final extraction\n\nReject out-of-root `source_file` values and preserve the structural extraction if optional enrichment fails.\n\n### Step 4 - Build graph, cluster, analyze, generate outputs\n\nThe `extract` command writes `graphoxide-out/graph.json`, diagnostics, analysis, and a report atomically; do not replace a healthy graph with an empty or unexpectedly shrunken result.\n\n### Step 5 - Label communities\n\nUse deterministic hub labels by default. Run `graphoxide label INPUT_PATH` only when the user opts into a configured remote labeling backend.\n\n### Step 6 - Generate Obsidian vault (opt-in) + HTML\n\nRun `graphoxide export html` for the default visualization. Run `graphoxide export obsidian` only when requested.\n\n### Step 9 - Save manifest, update cost tracker, clean up, and report\n\nRun `graphoxide update INPUT_PATH`; keeping `INPUT_PATH` explicit anchors manifest keys to the scan root and makes the cache portable across clones. Report skipped or failed files by name.\n\n## For /graphoxide query\n\nExpand the question against the graph's own vocabulary, then run `graphoxide query`. If the executable is unavailable, use a read-only JSON graph traversal fallback. See `references/query.md`.\n\n## On-demand references\n\n- `references/extraction-spec.md` — semantic schema and validation\n- `references/update.md` — incremental update and safe merge\n- `references/query.md` — query, path, explain, and traversal fallback\n- `references/exports.md` — optional export formats\n- `references/add-watch.md` — add and watch workflows\n- `references/hooks.md` — hooks and native integration\n- `references/github-and-merge.md` — repository graph merge\n- `references/transcribe.md` — media preprocessing\n\n",
    );
    body.push_str(&format!(
        "## For the commit hook and native {} integration\n\nRead `references/hooks.md` to wire graphoxide into a project's {}.\n\n",
        hooks_target(platform),
        hooks_target(platform)
    ));
    for extra in &platform.extra_sections {
        match *extra {
            "kilo-rules" => body.push_str(
                "## Kilo-specific rules\n\nUse Kilo's current workspace and never invent extraction output.\n\n",
            ),
            "powershell-troubleshooting" => body.push_str(
                "## Troubleshooting\n\n### PowerShell 5.1: Vertical scrolling stops working\n\nUse a fresh terminal and UTF-8 output.\n\n4. **Skip graspologic** — Graphoxide's Rust clustering does not require the Python package.\n\n",
            ),
            _ => {}
        }
    }
    body.push_str(
        "## Honesty Rules\n\nNever claim a graph was built, updated, queried, or exported unless the command succeeded and the expected artifact exists. Surface partial extraction and stale-graph warnings.\n",
    );
    normalise(&body)
}

fn extraction_reference(platform: &Platform) -> String {
    let compact = if platform.extraction == ExtractionVariant::Compact {
        " (compact)"
    } else {
        ""
    };
    format!(
        "# Extraction specification{compact}\n\nUse exactly these file types: {ENUM_PROSE}.\n\nEvery returned fact uses the schema `{{\"file_type\":\"{ENUM_VALUES}\",\"source_file\":\"relative/path\"}}`. Treat the pipe expression as an enum, not a literal value. Reject facts whose source path is outside the assigned corpus.\n"
    )
}

fn query_reference() -> &'static str {
    "# Query operations\n\n## Constrained query expansion\n\nExpand terms only against labels, kinds, paths, and relation contexts present in the graph.\n\nIf the CLI is unavailable, perform a bounded read-only JSON graph traversal; never silently answer from an uninspected repository.\n\n## For /graphoxide path\n\nRun `graphoxide path NODE_A NODE_B`.\n\n## For /graphoxide explain\n\nRun `graphoxide explain NODE`.\n"
}

fn hooks_reference(platform: &Platform) -> String {
    match platform.hooks_variant {
        HooksVariant::ClaudeMd => "# Hooks\n\n## For commit hooks\n\nRun `graphoxide hook install INPUT_PATH` and verify with `graphoxide hook status INPUT_PATH`.\n\n## For native CLAUDE.md integration\n\nRun `graphoxide claude install`; it writes a `## graphoxide` section to the local `CLAUDE.md`. Remove it with `graphoxide claude uninstall`.\n".to_owned(),
        HooksVariant::AgentsMd if platform.key == "trae" => "# Hooks\n\n## For commit hooks\n\nRun `graphoxide hook install INPUT_PATH`.\n\n## For native AGENTS.md integration (Trae)\n\nRun `graphoxide trae install       # or: graphoxide trae-cn install`; it writes a `## graphoxide` section to the local `AGENTS.md`. Run `graphoxide trae uninstall     # or: graphoxide trae-cn uninstall` to remove it.\n\n> **Note:** Unlike Claude Code, Trae does NOT support PreToolUse hooks. Run `/graphoxide --update` manually after code changes; AGENTS.md is the always-on mechanism.\n".to_owned(),
        HooksVariant::AgentsMd if platform.key == "amp" => "# Hooks\n\n## For commit hooks\n\nRun `graphoxide hook install INPUT_PATH`.\n\n## For native AGENTS.md integration\n\nRun `graphoxide amp install` to make graphoxide always-on in Amp sessions. The generated section instructs Amp to check the graph first. Run `graphoxide amp uninstall  # remove the section` to remove it.\n".to_owned(),
        HooksVariant::AgentsMd => "# Hooks\n\n## For commit hooks\n\nRun `graphoxide hook install INPUT_PATH`.\n\n## For native AGENTS.md integration\n\nRun `graphoxide agents install` to make graphoxide always-on in your agent sessions. Run `graphoxide agents uninstall  # remove the section` to remove it.\n".to_owned(),
    }
}

fn references(platform: &Platform) -> BTreeMap<&'static str, String> {
    BTreeMap::from([
        (
            "add-watch",
            "# Add and watch\n\nRun `graphoxide watch INPUT_PATH` for debounced incremental rebuilds. Use `graphoxide update INPUT_PATH --force` after an intentional large deletion.\n".to_owned(),
        ),
        (
            "exports",
            "# Exports\n\nRun `graphoxide export wiki`, `graphoxide export neo4j`, `graphoxide export graphml`, or `graphoxide export obsidian` only when requested.\n".to_owned(),
        ),
        ("extraction-spec", extraction_reference(platform)),
        (
            "github-and-merge",
            "# GitHub and graph merge\n\nBuild each checkout independently and use `graphoxide merge-graphs --output MERGED.json GRAPH...`. Never concatenate graph JSON.\n".to_owned(),
        ),
        ("hooks", hooks_reference(platform)),
        ("query", query_reference().to_owned()),
        (
            "transcribe",
            "# Transcribe media\n\nGraphoxide does not decode media. Produce a UTF-8 transcript with an approved local or host tool, retain source provenance, then extract the transcript.\n".to_owned(),
        ),
        (
            "update",
            "# Incremental update\n\nRun `graphoxide update INPUT_PATH`. The explicit root keeps manifest identities portable. Run `graphoxide cluster-only INPUT_PATH` only when extraction is already current. Use `--force` only for intentional shrinkage.\n".to_owned(),
        ),
    ])
}

fn render_split(platform: &Platform) -> Vec<RenderedArtifact> {
    let refs_dst = platform
        .refs_dst
        .as_deref()
        .expect("split refs destination");
    let mut artifacts = vec![RenderedArtifact::new(
        platform.skill_dst.clone(),
        render_core(platform),
    )];
    artifacts.extend(
        references(platform)
            .into_iter()
            .map(|(name, content)| RenderedArtifact::new(format!("{refs_dst}/{name}.md"), content)),
    );
    artifacts
}

fn render_monolith(platform: &Platform) -> String {
    let extra_frontmatter = if platform.key == "devin" {
        "argument-hint: \"[path or question]\"\nmodel: inherit\nallowed-tools: Bash, Read, Write\n"
    } else {
        ""
    };
    let mut body = frontmatter(extra_frontmatter);
    body.push_str(
        "\n# Graphoxide inline runbook\n\n### Step 1 - Ensure graphoxide is installed\n\nRun `graphoxide --version`.\n\n### Step 2 - Detect files\n\nRun `graphoxide audit INPUT_PATH --json`.\n\n### Step 3 - Extract entities and relationships\n\n> **graphoxide needs no API key. Never ask the user for one, and never block on one.** If this host cannot dispatch subagents, run the terminal-only `graphoxide extract INPUT_PATH` fallback.\n\nThe semantic schema accepts `file_type` values `code`, `document`, `paper`, `image`, `rationale`, `concept` and encodes them as `file_type\":\"code|document|paper|image|rationale|concept`.\n\nOnly content categories are semantically enriched: `for category in [\"document\", \"paper\", \"image\"]`. Structural code facts come from the AST pass.\n\nOn a semantic-cache miss, remove the stale cache entry before merging. Read `.graphoxide_uncached.txt` and set `allowed_source_files = uncached` on every cache write.\n\n### Step 4 - Build graph, cluster, analyze, generate outputs\n\nImplementation invariant: substitute `IS_DIRECTED` everywhere it appears.\n\n```rust\nlet graph = build_with_options(extraction, BuildOptions { directed: IS_DIRECTED, ..Default::default() });\nif graph.nodes.is_empty() { return Err(\"Graph is empty - extraction produced no nodes\"); }\nlet wrote = write_graph_if_safe(&graph)?;\nif !wrote { return Err(\"refused to shrink graphoxide-out/graph.json\"); }\nwrite_report(\"graphoxide-out/GRAPH_REPORT.md\", &graph)?;\n```\n\n### Step 9 - Save manifest and report\n\nUse `SaveManifestOptions { root: Some(INPUT_PATH.into()), ..Default::default() }`; stamp only files that actually produced output.\n\n## Query, path, explain, update, exports, hooks, and watch\n\nRun the corresponding `graphoxide` subcommand and verify its output before reporting success.\n\n## Honesty Rules\n\nNever claim success for a failed or partial operation.\n",
    );
    normalise(&body)
}

pub fn render(platform: &Platform) -> Vec<RenderedArtifact> {
    match platform.bucket {
        Bucket::Split => render_split(platform),
        Bucket::Monolith => vec![RenderedArtifact::new(
            platform.skill_dst.clone(),
            render_monolith(platform),
        )],
    }
}

fn always_on_source(name: &str) -> Option<&'static str> {
    match name {
        "claude-md" => Some("## graphoxide\n\nWhen graphoxide-out exists, query it before broad source reads. Run `graphoxide update .` after structural changes.\n"),
        "agents-md" => Some("## graphoxide\n\nWhen the user types `/graphoxide`, use the installed graphoxide skill or instructions before doing anything else. Query graphoxide-out before broad source reads.\n"),
        "gemini-md" => Some("## graphoxide\n\nPrefer `graphoxide query`, `path`, and `explain` when a graph exists.\n"),
        "vscode-instructions" => Some("## graphoxide\n\nUse the persisted graph as repository context and update it after structural edits.\n"),
        "antigravity-rules" => Some("## graphoxide\n\nInspect graphoxide-out before scanning the entire workspace.\n"),
        "kiro-steering" => Some("## graphoxide\n\nTreat a present graphoxide-out/graph.json as the first source for architecture questions.\n"),
        _ => None,
    }
}

pub fn always_on_constant(name: &str) -> Option<String> {
    always_on_source(name).map(normalise)
}

pub fn render_always_on() -> Vec<RenderedArtifact> {
    [
        "agents-md",
        "antigravity-rules",
        "claude-md",
        "gemini-md",
        "kiro-steering",
        "vscode-instructions",
    ]
    .into_iter()
    .map(|name| {
        RenderedArtifact::new(
            format!("graphoxide/always_on/{name}.md"),
            always_on_source(name).expect("known always-on block"),
        )
    })
    .collect()
}

pub fn render_all(
    platforms: &BTreeMap<&str, Platform>,
    only: Option<&str>,
) -> Result<Vec<RenderedArtifact>, String> {
    if let Some(key) = only {
        let platform = platforms
            .get(key)
            .ok_or_else(|| format!("unknown platform {key:?}"))?;
        return Ok(render(platform));
    }

    let mut artifacts = Vec::new();
    for platform in platforms.values() {
        artifacts.extend(render(platform));
    }
    artifacts.extend(render_always_on());
    artifacts.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(artifacts)
}

pub fn write_artifacts(root: &Path, artifacts: &[RenderedArtifact]) -> io::Result<()> {
    for artifact in artifacts {
        let path = root.join(&artifact.path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, &artifact.content)?;
    }
    Ok(())
}

pub fn check_on_disk(root: &Path, artifacts: &[RenderedArtifact]) -> Vec<String> {
    let mut problems = Vec::new();
    for artifact in artifacts {
        let path = root.join(&artifact.path);
        match fs::read_to_string(&path) {
            Ok(content) if content == artifact.content => {}
            Ok(_) => problems.push(format!("{} drifted from a fresh render", artifact.path)),
            Err(error) => problems.push(format!(
                "{} is missing or unreadable: {error}",
                artifact.path
            )),
        }
    }
    problems
}

/// Compare a supplied bundle to a fresh canonical render.  Unlike an on-disk
/// check this is useful to callers that package artifacts directly in memory.
pub fn check(artifacts: &[RenderedArtifact]) -> Vec<String> {
    let canonical = render_all(&platforms(), None).expect("built-in platforms");
    let canonical = canonical
        .into_iter()
        .map(|artifact| (artifact.path, artifact.content))
        .collect::<BTreeMap<_, _>>();
    let mut seen = BTreeSet::new();
    let mut problems = Vec::new();
    for artifact in artifacts {
        if !seen.insert(&artifact.path) {
            problems.push(format!("{} appears more than once", artifact.path));
            continue;
        }
        match canonical.get(&artifact.path) {
            Some(content) if content == &artifact.content => {}
            Some(_) => problems.push(format!("{} drifted from a fresh render", artifact.path)),
            None => problems.push(format!("{} is not a generated artifact", artifact.path)),
        }
    }
    problems
}

/// Fence-aware Markdown heading scanner.  ATX headings inside fenced code are
/// shell comments or examples and must not participate in coverage audits.
pub fn headings(markdown: &str) -> Vec<String> {
    let mut in_fence = false;
    let mut fence_char = None;
    let mut result = Vec::new();
    for line in markdown.lines() {
        let trimmed = line.trim_start();
        let marker = if trimmed.starts_with("```") {
            Some('`')
        } else if trimmed.starts_with("~~~") {
            Some('~')
        } else {
            None
        };
        if let Some(marker) = marker {
            if !in_fence {
                in_fence = true;
                fence_char = Some(marker);
            } else if fence_char == Some(marker) {
                in_fence = false;
                fence_char = None;
            }
            continue;
        }
        if in_fence {
            continue;
        }
        let hashes = trimmed.bytes().take_while(|byte| *byte == b'#').count();
        if (1..=6).contains(&hashes) && trimmed.as_bytes().get(hashes) == Some(&b' ') {
            result.push(trimmed.trim().to_owned());
        }
    }
    result
}

fn heading_homes(artifacts: &[RenderedArtifact], heading: &str) -> Vec<String> {
    artifacts
        .iter()
        .filter(|artifact| {
            headings(&artifact.content)
                .iter()
                .any(|item| item == heading)
        })
        .map(|artifact| artifact.path.clone())
        .collect()
}

pub fn audit_artifacts(platform: &Platform, artifacts: &[RenderedArtifact]) -> Vec<String> {
    if platform.bucket != Bucket::Split {
        return Vec::new();
    }
    let canonical = platforms();
    let Some(baseline_platform) = canonical.get(platform.key) else {
        return vec![format!("unknown coverage baseline for {}", platform.key)];
    };
    let baseline = render(baseline_platform);
    let required = baseline
        .iter()
        .flat_map(|artifact| headings(&artifact.content))
        .collect::<BTreeSet<_>>();
    let allowlist = consolidation_allowlist(platform.key);
    let mut problems = Vec::new();
    for heading in required {
        if allowlist.contains(&heading.as_str()) {
            continue;
        }
        let homes = heading_homes(artifacts, &heading);
        match homes.len() {
            0 => problems.push(format!(
                "baseline heading not covered anywhere: {heading:?}"
            )),
            1 => {}
            _ => problems.push(format!(
                "baseline heading double-homed in {homes:?}: {heading:?}"
            )),
        }
    }
    problems
}

pub fn audit_coverage(platform: &Platform) -> Vec<String> {
    audit_artifacts(platform, &render(platform))
}

pub fn legacy_enum_lines(content: &str) -> Vec<String> {
    const LEGACY: [&str; 2] = [
        "code|document|paper|image|rationale",
        "code|document|paper|image",
    ];
    content
        .lines()
        .filter(|line| {
            !line.contains(ENUM_VALUES) && LEGACY.iter().any(|value| line.contains(value))
        })
        .map(|line| line.trim().to_owned())
        .collect()
}

pub fn schema_singleton(platforms: &BTreeMap<&str, Platform>) -> Vec<String> {
    let mut problems = Vec::new();
    for (key, platform) in platforms {
        for artifact in render(platform) {
            for line in legacy_enum_lines(&artifact.content) {
                problems.push(format!(
                    "[{key}] {}: legacy file_type enum: {line:?}",
                    artifact.path
                ));
            }
        }
    }
    problems
}

pub fn monolith_roundtrip(platform: &Platform) -> Vec<String> {
    if platform.bucket != Bucket::Monolith {
        return Vec::new();
    }
    let body = render(platform)[0].content.clone();
    let required = [
        ENUM_VALUES,
        "directed: IS_DIRECTED",
        "allowed_source_files = uncached",
        "root: Some(INPUT_PATH.into())",
        "if graph.nodes.is_empty()",
        "if !wrote",
    ];
    let mut problems = required
        .into_iter()
        .filter(|marker| !body.contains(marker))
        .map(|marker| {
            format!(
                "[{}] monolith lost sanctioned invariant {marker:?}",
                platform.key
            )
        })
        .collect::<Vec<_>>();
    problems.extend(
        legacy_enum_lines(&body)
            .into_iter()
            .map(|line| format!("[{}] monolith retained legacy enum {line:?}", platform.key)),
    );
    problems
}

pub fn sanctioned_always_on_edits() -> BTreeMap<&'static str, ((&'static str, &'static str),)> {
    BTreeMap::from([(
        "_AGENTS_MD_SECTION",
        ((OLD_AGENTS_INSTRUCTION, NEW_AGENTS_INSTRUCTION),),
    )])
}

pub fn always_on_baseline(name: &str) -> Option<String> {
    if name == "agents-md" {
        return always_on_source(name).map(|content| {
            normalise(&content.replace(NEW_AGENTS_INSTRUCTION, OLD_AGENTS_INSTRUCTION))
        });
    }
    always_on_source(name).map(normalise)
}

pub fn always_on_roundtrip() -> Vec<String> {
    let edits = sanctioned_always_on_edits();
    let mut problems = Vec::new();
    for artifact in render_always_on() {
        let name = artifact
            .path
            .rsplit('/')
            .next()
            .and_then(|file| file.strip_suffix(".md"))
            .expect("always-on artifact name");
        let Some(mut expected) = always_on_baseline(name) else {
            problems.push(format!("{} has no frozen baseline", artifact.path));
            continue;
        };
        if name == "agents-md" {
            let ((old, new),) = edits["_AGENTS_MD_SECTION"];
            expected = expected.replace(old, new);
        }
        if expected != artifact.content {
            problems.push(format!(
                "{} drifted from its sanctioned baseline",
                artifact.path
            ));
        }
    }
    problems
}

pub fn v8_available(repo_root: &Path) -> bool {
    Command::new("git")
        .args([
            "cat-file",
            "-e",
            &format!("{GRAPHIFY_V8_BASELINE_SHA}^{{commit}}"),
        ])
        .current_dir(repo_root)
        .output()
        .is_ok_and(|output| output.status.success())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GitValidation {
    Ran,
    Skipped(String),
}

/// Shared shallow-checkout behavior for validators that need immutable git
/// blobs.  The renderer's normal audits are embedded and do not require git.
pub fn git_dependent_validation(repo_root: &Path) -> GitValidation {
    if v8_available(repo_root) {
        GitValidation::Ran
    } else {
        GitValidation::Skipped(
            "SKIPPED: immutable Graphify v8 baseline is unavailable; CI should use fetch-depth: 0"
                .to_owned(),
        )
    }
}
