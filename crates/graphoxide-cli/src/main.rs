//! Graphoxide CLI — a fast, dependency-free Rust code-graph tool.
//!
//! The command surface includes extraction, analysis, query, export, integrations,
//! and an MCP stdio server.

mod site;

use anyhow::Context;
use clap::{Args, Parser, Subcommand, ValueEnum};
use graphoxide_cli::build_progress::{
    BuildProgressFactory, BuildProgressMode, BuildProgressPhase, BuildProgressReporter,
};
use graphoxide_cli::watch as watch_service;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

#[derive(Parser)]
#[command(
    name = "graphoxide",
    version,
    about = "Turn a folder of code into a queryable knowledge graph"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// Explicit controls for the isolated I/O/CPU indexing runtime.
///
/// These flags apply only to the default executor. The legacy executor has no
/// equivalent resource-isolation contract, so combining it with any override
/// is rejected instead of silently ignoring the requested limit.
#[derive(Args, Debug, Clone, Default)]
struct RuntimeOptions {
    /// Managed-memory budget for runtime queues, registered parser allowances,
    /// completed-output admission, caches, and graph staging, in bytes.
    #[arg(long, value_name = "BYTES")]
    memory_budget_bytes: Option<usize>,
    /// Number of dedicated filesystem/cache I/O workers.
    #[arg(long, value_name = "COUNT")]
    io_workers: Option<usize>,
    /// Number of fixed-owner extraction CPU workers.
    #[arg(long, value_name = "COUNT")]
    compute_workers: Option<usize>,
    /// I/O backend selection for the isolated runtime.
    #[arg(long, value_enum, value_name = "BACKEND")]
    io_backend: Option<RuntimeIoBackendArg>,
    /// Maximum byte count requested for one I/O read operation.
    #[arg(long, value_name = "BYTES")]
    read_batch_bytes: Option<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum RuntimeIoBackendArg {
    Auto,
    Threaded,
    IoUring,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
enum ProgressModeArg {
    #[default]
    Auto,
    Never,
    Json,
}

impl From<ProgressModeArg> for BuildProgressMode {
    fn from(value: ProgressModeArg) -> Self {
        match value {
            ProgressModeArg::Auto => Self::Auto,
            ProgressModeArg::Never => Self::Never,
            ProgressModeArg::Json => Self::Json,
        }
    }
}

impl RuntimeOptions {
    fn is_empty(&self) -> bool {
        self.memory_budget_bytes.is_none()
            && self.io_workers.is_none()
            && self.compute_workers.is_none()
            && self.io_backend.is_none()
            && self.read_batch_bytes.is_none()
    }

    fn resolve(&self) -> anyhow::Result<graphoxide_index_runtime::IndexRuntimeConfig> {
        let mut config = graphoxide_index_runtime::IndexRuntimeConfig::default();
        if let Some(memory_budget_bytes) = self.memory_budget_bytes {
            config.memory_budget_bytes = memory_budget_bytes;
        }
        if let Some(io_workers) = self.io_workers {
            config.io_workers = io_workers;
        }
        if let Some(compute_workers) = self.compute_workers {
            config.compute_workers = compute_workers;
        }
        if let Some(io_backend) = self.io_backend {
            config.io_backend = match io_backend {
                RuntimeIoBackendArg::Auto => graphoxide_index_runtime::IoBackendSelection::Auto,
                RuntimeIoBackendArg::Threaded => {
                    graphoxide_index_runtime::IoBackendSelection::Threaded
                }
                RuntimeIoBackendArg::IoUring => {
                    graphoxide_index_runtime::IoBackendSelection::IoUring
                }
            };
        }
        if let Some(read_batch_bytes) = self.read_batch_bytes {
            config.read_batch_bytes = read_batch_bytes;
        }
        config.validate().map_err(|error| {
            anyhow::anyhow!("invalid isolated runtime configuration: {error:?}")
        })?;
        Ok(config)
    }

    fn resolve_for_executor(
        &self,
        legacy_executor: bool,
    ) -> anyhow::Result<Option<graphoxide_index_runtime::IndexRuntimeConfig>> {
        if legacy_executor {
            if self.is_empty() {
                return Ok(None);
            }
            anyhow::bail!(
                "isolated runtime options cannot be combined with --legacy-executor; remove the overrides or use the default executor"
            );
        }
        self.resolve().map(Some)
    }
}

#[derive(Subcommand)]
enum Command {
    /// Explicitly enrich eligible offline inventory with provider-authored facts.
    Enrich {
        #[command(flatten)]
        args: graphoxide_cli::enrich::EnrichArgs,
    },
    /// Headless deterministic extraction into graphoxide-out/
    Extract {
        #[command(flatten)]
        build: ProjectBuildOptions,
        /// Use the retired path-based/Rayon extractor instead of the default
        /// dedicated I/O and CPU execution runtime.
        #[arg(long)]
        legacy_executor: bool,
    },
    /// Build a deterministic graph and its associated universal coverage report
    Index {
        #[command(flatten)]
        build: ProjectBuildOptions,
    },
    /// Audit graph integrity or report bounded file-indexing coverage
    #[command(
        after_help = "Coverage report: graphoxide audit coverage [PATH] [--json] [--strict]\nGraph-audit a directory literally named coverage with: graphoxide audit ./coverage"
    )]
    Audit {
        /// Project path, or `coverage` to report bounded file-indexing coverage.
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Project path for `audit coverage`; use `./coverage` to graph-audit a
        /// directory whose literal name is `coverage`.
        #[arg(value_name = "COVERAGE_PATH")]
        coverage_path: Option<PathBuf>,
        /// Emit a machine-readable JSON report
        #[arg(long)]
        json: bool,
        /// Exit for graph conservation failures or incomplete coverage scans
        #[arg(long)]
        strict: bool,
        /// Bypass the incremental AST cache for a graph audit
        #[arg(long)]
        force: bool,
    },
    /// Inspect a graph/extraction JSON file for parallel-edge collapse risk
    Diagnose {
        /// Raw compatibility arguments: `multigraph [--graph PATH] [OPTIONS]`.
        #[arg(allow_hyphen_values = true, num_args = 0..)]
        args: Vec<String>,
    },
    /// Re-extract code files and update the graph (no LLM needed)
    Update {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Allow an intentional graph reduction after files or relationships are removed
        #[arg(long)]
        force: bool,
        /// Preserve the raw, unclustered extraction shape.
        #[arg(long)]
        no_cluster: bool,
        /// Emit one machine-readable build report to stdout.
        #[arg(long)]
        json: bool,
        /// Progress rendering on stderr; JSON is a prefixed protocol for integrations.
        #[arg(long, value_enum, default_value = "auto")]
        progress: ProgressModeArg,
        /// Atomically write additive runtime telemetry to this JSON sidecar.
        ///
        /// This never changes stdout, including the stable `--json` build
        /// report. The sidecar is intended for benchmark and runtime analysis.
        #[arg(long)]
        runtime_report: Option<PathBuf>,
        /// Use the retired path-based/Rayon update executor instead of the
        /// default dedicated I/O and CPU execution runtime.
        #[arg(long)]
        legacy_executor: bool,
        #[command(flatten)]
        runtime: RuntimeOptions,
    },
    /// List the bounded structured-format capability contract used by the default executor.
    #[command(visible_alias = "capabilities")]
    Formats {
        /// Emit the complete registry contract as machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// BFS traversal of graph.json for a question
    Query {
        question: String,
        /// Cap output at N tokens
        #[arg(long, default_value_t = 2000)]
        budget: usize,
        #[arg(long, default_value = "graphoxide-out/graph.json")]
        graph: PathBuf,
        /// Traverse depth-first instead of breadth-first
        #[arg(long)]
        dfs: bool,
        /// Restrict traversal to a relationship context (call, import, type, structure)
        #[arg(long = "context")]
        contexts: Vec<String>,
    },
    /// Shortest path between two nodes
    Path {
        a: String,
        b: String,
        #[arg(long, default_value = "graphoxide-out/graph.json")]
        graph: PathBuf,
    },
    /// Plain-language explanation of a node and its neighbors
    Explain {
        node: String,
        #[arg(long, default_value = "graphoxide-out/graph.json")]
        graph: PathBuf,
    },
    /// Reverse traversal to find nodes impacted by X
    Affected {
        node: String,
        #[arg(long, default_value_t = 2)]
        depth: usize,
        #[arg(long = "relation")]
        relations: Vec<String>,
        #[arg(long, default_value = "graphoxide-out/graph.json")]
        graph: PathBuf,
    },
    /// List the most connected nodes (architectural hubs)
    #[command(alias = "god_nodes")]
    GodNodes {
        #[arg(long, default_value_t = 10)]
        top: usize,
        #[arg(long)]
        json: bool,
        #[arg(long, default_value = "graphoxide-out/graph.json")]
        graph: PathBuf,
    },
    /// Save a Q&A outcome into work memory for the next graph/reflection update
    SaveResult {
        #[arg(long)]
        question: String,
        #[arg(long, required_unless_present = "answer_file")]
        answer: Option<String>,
        /// Read a long or multiline answer from a UTF-8 file.
        #[arg(long)]
        answer_file: Option<PathBuf>,
        #[arg(long = "type", default_value = "query")]
        query_type: String,
        #[arg(long, num_args = 0..)]
        nodes: Vec<String>,
        #[arg(long, value_parser = ["useful", "dead_end", "corrected"])]
        outcome: Option<String>,
        #[arg(long)]
        correction: Option<String>,
        #[arg(long, default_value = "graphoxide-out/memory")]
        memory_dir: PathBuf,
    },
    /// Aggregate saved work-memory outcomes into deterministic lessons
    Reflect {
        #[arg(long, default_value = "graphoxide-out/memory")]
        memory_dir: PathBuf,
        #[arg(long, default_value = "graphoxide-out/reflections/LESSONS.md")]
        out: PathBuf,
        #[arg(long)]
        graph: Option<PathBuf>,
        #[arg(long)]
        analysis: Option<PathBuf>,
        #[arg(long)]
        labels: Option<PathBuf>,
        #[arg(long, default_value_t = 30.0)]
        half_life_days: f64,
        #[arg(long, default_value_t = 2)]
        min_corroboration: usize,
        /// Skip when the lessons file is already newer than every input.
        #[arg(long)]
        if_stale: bool,
    },
    /// Rerun clustering on an existing graph.json
    ClusterOnly {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Compatibility flag: do not generate an HTML visualization.
        #[arg(long)]
        no_viz: bool,
        /// Compatibility flag: retain deterministic/default labels.
        #[arg(long)]
        no_label: bool,
    },
    /// Name graph communities through an OpenAI- or Anthropic-compatible HTTP endpoint
    Label {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        backend: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        missing_only: bool,
        #[arg(long, default_value_t = 4)]
        max_concurrency: usize,
        #[arg(long, default_value_t = graphoxide_graph::DEFAULT_BATCH_SIZE)]
        batch_size: usize,
        /// Whole-request timeout for each LLM labeling batch, in seconds.
        #[arg(long)]
        timeout_seconds: Option<f64>,
    },
    /// Generate GRAPH_REPORT.md from an existing graph
    Report {
        #[arg(long, default_value = "graphoxide-out/graph.json")]
        graph: PathBuf,
        #[arg(long, default_value = "graphoxide-out/GRAPH_REPORT.md")]
        output: PathBuf,
    },
    /// Export an existing graph as HTML, callflow HTML, GraphML, Cypher, wiki, Obsidian, or JSON
    Export {
        #[arg(value_parser = ["html", "callflow-html", "graphml", "cypher", "neo4j", "falkordb", "wiki", "obsidian", "json"])]
        format: String,
        /// Output path, or the graph path for `callflow-html --output ...`.
        positional: Option<PathBuf>,
        #[arg(long)]
        output: Option<PathBuf>,
        #[arg(long)]
        graph: Option<PathBuf>,
        /// Custom Obsidian vault directory.
        #[arg(long = "dir")]
        directory: Option<PathBuf>,
        /// Remove/skip the interactive visualization rather than rendering it.
        #[arg(long)]
        no_viz: bool,
        #[arg(long, default_value_t = 15)]
        max_sections: usize,
    },
    /// Measure query latency on an existing graph
    Benchmark {
        question: String,
        #[arg(long, default_value_t = 100)]
        iterations: usize,
        #[arg(long, default_value = "graphoxide-out/graph.json")]
        graph: PathBuf,
    },
    /// Merge graph.json files into one graph
    MergeGraphs {
        #[arg(required = true)]
        inputs: Vec<PathBuf>,
        #[arg(long, alias = "out")]
        output: PathBuf,
        #[arg(long)]
        force: bool,
    },
    /// Render containment/import relationships as a text tree
    Tree {
        root: Option<String>,
        #[arg(long, default_value = "graphoxide-out/graph.json")]
        graph: PathBuf,
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Discover and merge project graphs under one or more roots
    GlobalGraph {
        #[arg(required = true)]
        roots: Vec<PathBuf>,
        #[arg(long, default_value = ".graphoxide-global/graph.json")]
        output: PathBuf,
        #[arg(long)]
        force: bool,
    },
    /// Maintain the user-wide ~/.graphoxide/global-graph.json
    Global {
        #[command(subcommand)]
        command: GlobalCommand,
    },
    /// Merge-driver entry point for graph.json conflicts
    MergeDriver {
        base: PathBuf,
        ours: PathBuf,
        theirs: PathBuf,
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Notify when a watcher recorded pending non-code changes
    CheckUpdate {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Watch a project and rebuild after source changes
    Watch {
        path: PathBuf,
        /// Allow an intentional graph reduction after files or relationships are removed.
        #[arg(long)]
        force: bool,
        /// Preserve the raw, unclustered extraction shape.
        #[arg(long)]
        no_cluster: bool,
        /// Atomically write additive runtime telemetry after each rebuild.
        #[arg(long)]
        runtime_report: Option<PathBuf>,
        /// Progress rendering for each admitted rebuild pass.
        #[arg(long, value_enum, default_value = "auto")]
        progress: ProgressModeArg,
        /// Use the retired path-based/Rayon rebuild executor instead of the
        /// default dedicated I/O and CPU execution runtime.
        #[arg(long)]
        legacy_executor: bool,
        #[command(flatten)]
        runtime: RuntimeOptions,
    },
    /// Install a coding-agent skill and its project integration
    Install {
        /// Host name, accepted positionally for compatibility
        platform: Option<String>,
        /// Host name, accepted as an explicit flag
        #[arg(long = "platform")]
        platform_flag: Option<String>,
        /// Install the skill into the current project instead of the user scope
        #[arg(long)]
        project: bool,
        /// Make Claude's first un-oriented source read fail once per session
        #[arg(long)]
        strict: bool,
    },
    /// Remove coding-agent integrations
    Uninstall {
        /// Host name, accepted positionally for compatibility; omit to remove all
        platform: Option<String>,
        /// Host name, accepted as an explicit flag
        #[arg(long = "platform")]
        platform_flag: Option<String>,
        /// Remove only project-scoped artifacts
        #[arg(long)]
        project: bool,
    },
    /// Install, remove, or inspect git hooks
    Hook {
        #[command(subcommand)]
        command: HookCommand,
    },
    /// Install or remove Claude Code project integration
    Claude {
        #[command(subcommand)]
        command: ClaudeCommand,
    },
    /// Run `codebuddy install` or `codebuddy uninstall` integration management
    #[command(name = "codebuddy")]
    CodeBuddy {
        #[command(subcommand)]
        command: PlatformInstallCommand,
    },
    /// Install or remove the Devin skill at ~/.config/devin/skills/graphoxide
    Devin {
        #[command(subcommand)]
        command: PlatformInstallCommand,
    },
    /// Run `agents install`/`uninstall`; `skills` is an equivalent alias
    #[command(alias = "skills")]
    Agents {
        #[command(subcommand)]
        command: PlatformInstallCommand,
    },
    /// Install or remove Codex integration
    Codex {
        #[command(subcommand)]
        command: PlatformInstallCommand,
    },
    /// Install or remove Antigravity integration
    Antigravity {
        #[command(subcommand)]
        command: PlatformInstallCommand,
    },
    /// Install or remove Amp integration
    Amp {
        #[command(subcommand)]
        command: PlatformInstallCommand,
    },
    /// Claude Code PreToolUse hook that nudges graph queries
    HookGuard {
        /// Host hook contract: search, read, or gemini
        mode: Option<String>,
        /// Compatibility flag for Graphify's opt-in strict read guard
        #[arg(long)]
        strict: bool,
    },
    /// Codex PreToolUse compatibility hook (recognized no-op)
    HookCheck,
    /// Internal: detach a supervised Git-hook rebuild
    #[command(hide = true)]
    HookLaunch {
        mode: String,
        root: PathBuf,
        log: PathBuf,
    },
    /// Internal: supervise a Git-hook rebuild and enforce its timeout
    #[command(hide = true)]
    HookSupervise { mode: String, root: PathBuf },
    /// Internal: execute a Git-hook rebuild
    #[command(hide = true)]
    HookRebuild {
        mode: String,
        root: PathBuf,
        /// Compatibility escape hatch for the retired path-based hook rebuild.
        #[arg(long, hide = true)]
        legacy_executor: bool,
        #[command(flatten)]
        runtime: RuntimeOptions,
    },
    /// Start the MCP server over stdio or Streamable HTTP
    Serve {
        #[arg(default_value = "graphoxide-out/graph.json")]
        graph: PathBuf,
        #[arg(long, value_parser = ["stdio", "http"], default_value = "stdio")]
        transport: String,
        #[arg(long, default_value = "127.0.0.1")]
        host: std::net::IpAddr,
        #[arg(long, default_value_t = 8080)]
        port: u16,
        #[arg(long)]
        api_key: Option<String>,
        #[arg(long = "path", default_value = "/mcp")]
        mount_path: String,
        #[arg(long)]
        json_response: bool,
        #[arg(long)]
        stateless: bool,
        #[arg(long, default_value_t = 3600.0)]
        session_timeout: f64,
    },
    /// Preview the Graphoxide website on localhost
    Site {
        /// Directory containing the static website
        #[arg(default_value = "website")]
        path: PathBuf,
        /// Loopback TCP port (use 0 to select an available port)
        #[arg(long, default_value_t = 8080)]
        port: u16,
    },
}

#[derive(Args, Debug)]
struct ProjectBuildOptions {
    #[arg(default_value = ".")]
    path: PathBuf,
    /// Restrict extraction to code and configuration inputs
    #[arg(long)]
    code_only: bool,
    /// Skip clustering, write raw extraction only
    #[arg(long)]
    no_cluster: bool,
    /// Full re-scan: skip the incremental manifest gate
    #[arg(long)]
    force: bool,
    /// Include a read-only PostgreSQL catalog, optionally using this DSN.
    #[arg(long, num_args = 0..=1, default_missing_value = "")]
    postgres: Option<String>,
    /// Permit a known-incomplete extraction to bypass shrink protection.
    #[arg(long)]
    allow_partial: bool,
    /// Emit extraction stage durations to stderr.
    #[arg(long)]
    timing: bool,
    /// Emit one machine-readable workflow report to stdout.
    #[arg(long)]
    json: bool,
    /// Progress rendering on stderr; JSON is a prefixed protocol for integrations.
    #[arg(long, value_enum, default_value = "auto")]
    progress: ProgressModeArg,
    /// Atomically write additive runtime telemetry to this JSON sidecar.
    ///
    /// This never changes stdout, including the stable `--json` report. The
    /// sidecar is intended for benchmark and runtime analysis.
    #[arg(long)]
    runtime_report: Option<PathBuf>,
    #[command(flatten)]
    runtime: RuntimeOptions,
    /// Place the managed graphoxide-out directory beneath this root.
    #[arg(long, visible_alias = "output")]
    out: Option<PathBuf>,
    /// Exclude a path or ignore-style pattern; repeated flags replace the persisted set.
    #[arg(long)]
    exclude: Vec<String>,
    /// Ignore VCS ignore files while continuing to honor .graphoxideignore/.graphifyignore.
    #[arg(long)]
    no_gitignore: bool,
}

#[derive(Subcommand)]
enum HookCommand {
    Install {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    Uninstall {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    Status {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
}

#[derive(Subcommand)]
enum GlobalCommand {
    Add {
        graph: PathBuf,
        #[arg(long = "as")]
        repo_tag: Option<String>,
    },
    Remove {
        repo_tag: String,
    },
    List,
    Path,
}

#[derive(Subcommand)]
enum ClaudeCommand {
    Install {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        project: bool,
        /// Make the first un-oriented source read fail once per session
        #[arg(long)]
        strict: bool,
    },
    Uninstall {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        project: bool,
    },
    Status {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
}

#[derive(Subcommand)]
enum PlatformInstallCommand {
    Install {
        #[arg(long)]
        project: bool,
    },
    Uninstall {
        #[arg(long)]
        project: bool,
    },
}

fn flatten_extractions(
    extractions: Vec<graphoxide_core::Extraction>,
) -> graphoxide_core::Extraction {
    let mut flattened = graphoxide_core::Extraction::default();
    for extraction in extractions {
        flattened.nodes.extend(extraction.nodes);
        flattened.edges.extend(extraction.edges);
        flattened.hyperedges.extend(extraction.hyperedges);
    }
    flattened
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IncrementalGraphBudget {
    max_baseline_file_bytes: u64,
    max_graph_materialized_bytes: usize,
}

fn graph_budget_after_pending_manifest(
    cache_and_runs_bytes: usize,
    pending_manifest_retained_bytes: usize,
) -> anyhow::Result<usize> {
    cache_and_runs_bytes
        .checked_sub(pending_manifest_retained_bytes)
        .filter(|remaining| *remaining > 0)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "pending manifest retains {pending_manifest_retained_bytes} bytes, exhausting the {cache_and_runs_bytes}-byte cache/run memory budget; increase --memory-budget-bytes or request a full rebuild"
            )
        })
}

/// Reserve baseline headroom while the fresh extraction vector is still live.
/// The merged-graph limit is derived from the exact byte slice admitted by the
/// capped reader, so unused baseline headroom remains available to materialization.
fn incremental_graph_budget_after_retained_scan(
    cache_and_runs_bytes: usize,
    retained_output_bytes: usize,
    pending_manifest_retained_bytes: usize,
) -> anyhow::Result<IncrementalGraphBudget> {
    let max_graph_materialized_bytes =
        graph_budget_after_pending_manifest(cache_and_runs_bytes, pending_manifest_retained_bytes)?;
    let remaining = max_graph_materialized_bytes
        .checked_sub(retained_output_bytes)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "fresh extraction output retains {retained_output_bytes} bytes and its pending manifest retains {pending_manifest_retained_bytes} bytes, exceeding the {cache_and_runs_bytes}-byte cache/run memory budget; increase --memory-budget-bytes or request a full rebuild"
            )
        })?;
    let max_baseline_file_bytes =
        remaining / graphoxide_graph::incremental::INCREMENTAL_GRAPH_WORKING_SET_MULTIPLIER;
    anyhow::ensure!(
        max_baseline_file_bytes > 0,
        "fresh extraction output retains {retained_output_bytes} bytes and its pending manifest retains {pending_manifest_retained_bytes} bytes of the {cache_and_runs_bytes}-byte cache/run memory budget, leaving insufficient incremental graph headroom; increase --memory-budget-bytes or request a full rebuild"
    );
    Ok(IncrementalGraphBudget {
        max_baseline_file_bytes: u64::try_from(max_baseline_file_bytes).unwrap_or(u64::MAX),
        max_graph_materialized_bytes,
    })
}

fn read_incremental_baseline(
    path: &std::path::Path,
    cache_and_runs_bytes: usize,
    budget: IncrementalGraphBudget,
) -> anyhow::Result<(graphoxide_core::KnowledgeGraph, usize)> {
    let admitted = graphoxide_core::read_graph_capped(path, budget.max_baseline_file_bytes)
        .with_context(|| {
            format!(
                "load incremental baseline {} within the cache/run memory budget; increase --memory-budget-bytes or request a full rebuild",
                path.display()
            )
        })?;
    let baseline_working_set = admitted
        .admitted_bytes
        .saturating_mul(graphoxide_graph::incremental::INCREMENTAL_GRAPH_WORKING_SET_MULTIPLIER);
    // Fresh facts are a separate admission constraint while the baseline
    // loads. Do not subtract them a second time here: the graph-stage 8x
    // multiplier already includes its source-fact representation alongside
    // normalized facts, graph objects, and indexes.
    let max_merged_materialized_bytes = budget
        .max_graph_materialized_bytes
        .checked_sub(baseline_working_set)
        .filter(|remaining| *remaining > 0)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "incremental baseline {} requires an estimated {baseline_working_set}-byte working set while the pending manifest is live, exhausting the {cache_and_runs_bytes}-byte cache/run memory budget; increase --memory-budget-bytes or request a full rebuild",
                path.display(),
            )
        })?;
    Ok((admitted.graph, max_merged_materialized_bytes))
}

fn optional_baseline_leaves_full_graph_headroom(
    retained_output_bytes: usize,
    max_materialized_bytes: usize,
) -> bool {
    retained_output_bytes
        .saturating_mul(graphoxide_graph::incremental::INCREMENTAL_GRAPH_WORKING_SET_MULTIPLIER)
        <= max_materialized_bytes
}

fn stale_local_sources(
    graph: &graphoxide_core::KnowledgeGraph,
    root: &std::path::Path,
    live_sources: &[PathBuf],
) -> Vec<PathBuf> {
    use std::path::Component;

    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let live = live_sources
        .iter()
        .map(|source| {
            let source = if source.is_absolute() {
                source.clone()
            } else {
                root.join(source)
            };
            fs::canonicalize(&source).unwrap_or(source)
        })
        .collect::<std::collections::BTreeSet<_>>();
    let mut stale = std::collections::BTreeSet::new();
    for node in &graph.nodes {
        let source = node.source_file.as_str();
        if source.is_empty() {
            continue;
        }
        // Marked member facts are lifecycle-owned by the scanned outer
        // container. Unmarked facts, including real paths whose spelling
        // contains `!/`, remain owned by their complete source path.
        let tracked_source = node
            .extra
            .get(graphoxide_core::CONTAINER_SOURCE_ATTRIBUTE)
            .and_then(|value| value.as_str())
            .filter(|owner| !owner.is_empty())
            .unwrap_or(source);
        if tracked_source.contains("://")
            || tracked_source.contains(":/")
            || tracked_source.contains(":\\")
        {
            continue;
        }
        let source_path = std::path::Path::new(tracked_source);
        let source_candidate = if source_path.is_absolute() {
            if !source_path.starts_with(&root) {
                continue;
            }
            source_path.to_path_buf()
        } else {
            if source_path
                .components()
                .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
            {
                continue;
            }
            root.join(source_path)
        };
        let normalized_source =
            fs::canonicalize(&source_candidate).unwrap_or_else(|_| source_candidate.clone());
        if live.contains(&normalized_source) {
            continue;
        }
        stale.insert(source_candidate);
    }
    stale.into_iter().collect()
}

fn normalized_local_source_identity(root: &Path, source: &Path) -> Option<PathBuf> {
    use std::path::Component;

    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let candidate = if source.is_absolute() {
        source.to_path_buf()
    } else {
        if source
            .components()
            .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
        {
            return None;
        }
        root.join(source)
    };
    let candidate = fs::canonicalize(&candidate).unwrap_or(candidate);
    candidate.starts_with(&root).then_some(candidate)
}

const BASELINE_REPRESENTATION_DIAGNOSTIC_PATH_MAX_BYTES: usize = 256;

fn bounded_baseline_representation_source(source: &str) -> String {
    let mut end = source
        .len()
        .min(BASELINE_REPRESENTATION_DIAGNOSTIC_PATH_MAX_BYTES);
    while !source.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    let sample = if end < source.len() {
        format!("{}…", &source[..end])
    } else {
        source.to_owned()
    };
    format!("{sample:?}")
}

fn baseline_node_is_structural(node: &graphoxide_core::Node) -> bool {
    node.extra
        .get("_origin")
        .and_then(serde_json::Value::as_str)
        .and_then(graphoxide_graph::origin_is_structural)
        .unwrap_or_else(|| {
            node.source_location.as_deref().is_some_and(|location| {
                let mut chars = location.chars();
                chars.next() == Some('L') && chars.next().is_some_and(|ch| ch.is_ascii_digit())
            })
        })
}

fn baseline_ambiguous_representation_evidence(
    baseline: &graphoxide_core::KnowledgeGraph,
    detection: &graphoxide_extract::detect::DetectResult,
    root: &Path,
) -> (
    std::collections::BTreeSet<PathBuf>,
    std::collections::BTreeSet<PathBuf>,
) {
    let current_code_ts = detection
        .files
        .get(graphoxide_extract::detect::FileType::Code.as_str())
        .into_iter()
        .flatten()
        .map(Path::new)
        .filter(|path| {
            path.extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("ts"))
        })
        .filter_map(|path| normalized_local_source_identity(root, path))
        .collect::<std::collections::BTreeSet<_>>();
    let current_mpeg_ts = detection
        .files
        .get(graphoxide_extract::detect::FileType::Video.as_str())
        .into_iter()
        .flatten()
        .map(Path::new)
        .filter(|path| {
            path.extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("ts"))
        })
        .filter_map(|path| normalized_local_source_identity(root, path))
        .collect::<std::collections::BTreeSet<_>>();
    if current_code_ts.is_empty() && current_mpeg_ts.is_empty() {
        return (
            std::collections::BTreeSet::new(),
            std::collections::BTreeSet::new(),
        );
    }
    let mut representations = std::collections::BTreeMap::<PathBuf, (bool, bool)>::new();
    for node in &baseline.nodes {
        let Some(identity) = normalized_local_source_identity(root, Path::new(&node.source_file))
        else {
            continue;
        };
        if !current_code_ts.contains(&identity) && !current_mpeg_ts.contains(&identity) {
            continue;
        }
        let representation = representations.entry(identity).or_default();
        if node.extra.get("type").and_then(serde_json::Value::as_str) == Some("format_inventory")
            && node.extra.get("format").and_then(serde_json::Value::as_str)
                == Some("mpeg_transport_stream")
        {
            representation.0 = true;
        } else if baseline_node_is_structural(node) {
            representation.1 = true;
        }
    }
    let needs_rebuild = current_code_ts
        .iter()
        .filter(|identity| {
            let (has_mpeg, has_code) = representations.get(*identity).copied().unwrap_or_default();
            has_mpeg || !has_code
        })
        .chain(current_mpeg_ts.iter().filter(|identity| {
            let (has_mpeg, has_code) = representations.get(*identity).copied().unwrap_or_default();
            !has_mpeg || has_code
        }))
        .cloned()
        .collect();
    let ownership_conflicts = current_code_ts
        .iter()
        .filter(|identity| {
            representations
                .get(*identity)
                .is_some_and(|(has_mpeg, _)| *has_mpeg)
        })
        .chain(current_mpeg_ts.iter().filter(|identity| {
            representations
                .get(*identity)
                .is_some_and(|(_, has_code)| *has_code)
        }))
        .cloned()
        .collect();
    (needs_rebuild, ownership_conflicts)
}

fn ensure_incremental_baseline_representation_is_verified(
    baseline: &graphoxide_core::KnowledgeGraph,
    detection: &graphoxide_extract::detect::DetectResult,
    ownership_reset_sources: &[PathBuf],
    rebuilt_sources: &[PathBuf],
    verified_representation_sources: &[PathBuf],
    root: &Path,
) -> anyhow::Result<()> {
    let verified_resets = ownership_reset_sources
        .iter()
        .filter_map(|path| normalized_local_source_identity(root, path))
        .collect::<std::collections::BTreeSet<_>>();
    let rebuilt = rebuilt_sources
        .iter()
        .filter_map(|path| normalized_local_source_identity(root, path))
        .collect::<std::collections::BTreeSet<_>>();
    let verified_representations = verified_representation_sources
        .iter()
        .filter_map(|path| normalized_local_source_identity(root, path))
        .collect::<std::collections::BTreeSet<_>>();
    let verified_rebuild_or_exclusion = rebuilt
        .union(&verified_resets)
        .cloned()
        .chain(verified_representations)
        .collect::<std::collections::BTreeSet<_>>();
    let (needs_rebuild, ownership_conflicts) =
        baseline_ambiguous_representation_evidence(baseline, detection, root);
    let unverified_rebuild = needs_rebuild
        .difference(&verified_rebuild_or_exclusion)
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let unverified_reset = ownership_conflicts
        .difference(&verified_resets)
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let unverified = unverified_rebuild
        .union(&unverified_reset)
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    if let Some(first) = unverified.first() {
        let first = first.strip_prefix(root).unwrap_or(first).to_string_lossy();
        anyhow::bail!(
            "committed graph disagrees with the current TypeScript/MPEG representation for {} source(s) without a verified structural rebuild or ownership reset; first source: {}; refusing incremental publication; rerun with --force for a Full Rebuild",
            unverified.len(),
            bounded_baseline_representation_source(&first)
        );
    }
    Ok(())
}

fn gate_baseline_representation_resets(
    baseline: &graphoxide_core::KnowledgeGraph,
    detection: &graphoxide_extract::detect::DetectResult,
    reset_candidates: &[PathBuf],
    rebuilt_sources: &[PathBuf],
    verified_representation_sources: &[PathBuf],
    root: &Path,
) -> anyhow::Result<Vec<PathBuf>> {
    let authoritative = reset_candidates
        .iter()
        .filter_map(|source| normalized_local_source_identity(root, source))
        .collect::<std::collections::BTreeSet<_>>();
    let verified = verified_representation_sources
        .iter()
        .chain(rebuilt_sources)
        .filter_map(|source| normalized_local_source_identity(root, source))
        .collect::<std::collections::BTreeSet<_>>();
    let (needs_rebuild, ownership_conflicts) =
        baseline_ambiguous_representation_evidence(baseline, detection, root);
    let conflict_resets = ownership_conflicts
        .intersection(&verified)
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let mut resets = authoritative
        .union(&conflict_resets)
        .cloned()
        .collect::<Vec<_>>();
    resets.sort();
    resets.dedup();
    let detected_kinds = detection
        .files
        .iter()
        .filter(|(kind, _)| matches!(kind.as_str(), "code" | "video"))
        .flat_map(|(kind, paths)| {
            paths.iter().filter_map(|path| {
                let path = Path::new(path);
                path.extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("ts"))
                    .then(|| {
                        normalized_local_source_identity(root, path)
                            .map(|identity| (identity, kind.as_str()))
                    })
                    .flatten()
            })
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    let verified_rebuilds = rebuilt_sources
        .iter()
        .filter_map(|source| normalized_local_source_identity(root, source))
        .filter(|identity| needs_rebuild.contains(identity))
        .collect::<std::collections::BTreeSet<_>>();
    let verified_exclusions = verified_representation_sources
        .iter()
        .filter_map(|source| normalized_local_source_identity(root, source))
        .filter(|identity| needs_rebuild.contains(identity))
        .collect::<std::collections::BTreeSet<_>>();
    let mut failed_rechecks = std::collections::BTreeSet::new();
    let checked_identities = resets
        .iter()
        .chain(&verified_rebuilds)
        .chain(&verified_exclusions)
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    for identity in &checked_identities {
        let Some(expected) = detected_kinds.get(identity) else {
            continue;
        };
        let verified = graphoxide_extract::detect::classify_ambiguous_typescript_file_checked(
            identity, identity,
        )
        .is_ok_and(|actual| actual.as_str() == *expected);
        if !verified {
            failed_rechecks.insert(identity.clone());
        }
    }
    if let Some(first) = failed_rechecks.first() {
        let source = first.strip_prefix(root).unwrap_or(first).to_string_lossy();
        anyhow::bail!(
            "extension-ambiguous TypeScript/MPEG representation changed after extraction for {} source(s); first source: {}; graph and manifest were not changed; retry the operation or rerun with --force for a Full Rebuild",
            failed_rechecks.len(),
            bounded_baseline_representation_source(&source)
        );
    }
    Ok(resets)
}

fn emit_build_timing(
    report: &graphoxide_cli::build_telemetry::BuildTelemetry,
    timing: bool,
) -> anyhow::Result<()> {
    if timing {
        let stages = &report.stages_ms;
        let mut stage_values = match report.operation {
            graphoxide_cli::build_telemetry::BuildOperation::Extract
            | graphoxide_cli::build_telemetry::BuildOperation::Index => {
                vec![
                    ("detect/extract", stages.scan_extract),
                    ("build", stages.build),
                ]
            }
            graphoxide_cli::build_telemetry::BuildOperation::Update => vec![
                ("detect", stages.detect),
                ("extract", stages.extract),
                ("build", stages.build),
            ],
        };
        if report.graph.clustered {
            stage_values.push(("cluster", stages.cluster));
        }
        stage_values.push(("write", stages.write));
        for (stage, milliseconds) in stage_values {
            eprintln!(
                "[graphoxide timing] {stage}: {}",
                graphoxide_cli::build_telemetry::format_elapsed(milliseconds)
            );
        }
        eprintln!(
            "[graphoxide timing] total: {}",
            graphoxide_cli::build_telemetry::format_elapsed(report.elapsed_ms)
        );
    }
    Ok(())
}

fn emit_build_report(
    report: &graphoxide_cli::build_telemetry::BuildTelemetry,
    json: bool,
    timing: bool,
    human: &str,
) -> anyhow::Result<()> {
    emit_build_timing(report, timing)?;
    if json {
        write_output(&serde_json::to_string(report)?)
    } else {
        write_output(human)
    }
}

fn emit_project_build_report(
    report: &graphoxide_cli::build_telemetry::BuildTelemetry,
    json: bool,
    timing: bool,
    human: &str,
    coverage: Option<&graphoxide_cli::index::PublishedCoverage>,
) -> anyhow::Result<()> {
    let Some(coverage) = coverage else {
        return emit_build_report(report, json, timing, human);
    };
    emit_build_timing(report, timing)?;
    if json {
        write_output(&serde_json::to_string(
            &graphoxide_cli::index::IndexBuildReport::new(report, coverage),
        )?)
    } else {
        let completeness = if coverage.complete {
            ""
        } else {
            " (incomplete)"
        };
        write_output(&format!(
            "{human}\nWrote associated coverage{completeness} to {}",
            coverage.path,
        ))
    }
}

fn write_runtime_report_if_requested(
    report: &graphoxide_cli::build_telemetry::BuildTelemetry,
    runtime_report: Option<&std::path::Path>,
    runtime: Option<&graphoxide_cli::build_telemetry::IndexRuntimeConfiguration>,
    io: Option<graphoxide_index_runtime::RuntimeIoTelemetry>,
    work: Option<graphoxide_extract::RuntimeWorkTelemetry>,
    cache: Option<graphoxide_cli::build_telemetry::RuntimeCacheTelemetryV2>,
) -> anyhow::Result<()> {
    let Some(path) = runtime_report else {
        return Ok(());
    };
    let mut sidecar = runtime.map_or_else(
        || graphoxide_cli::build_telemetry::IndexRuntimeTelemetryV2::legacy(report.clone()),
        |runtime| {
            graphoxide_cli::build_telemetry::IndexRuntimeTelemetryV2::isolated(
                report.clone(),
                runtime.clone(),
            )
        },
    );
    if let Some(cache) = cache {
        sidecar = sidecar.with_cache(cache);
    }
    if let Some(io) = io {
        sidecar = sidecar.with_io(io.into());
    }
    if let Some(work) = work {
        sidecar = sidecar.with_work(work.into());
    }
    graphoxide_cli::build_telemetry::write_runtime_report_v2(path, &sidecar)
        .with_context(|| format!("write runtime telemetry sidecar {}", path.display()))
}

fn isolated_runtime_configuration(
    config: graphoxide_index_runtime::IndexRuntimeConfig,
    admitted_requests: usize,
) -> graphoxide_cli::build_telemetry::IndexRuntimeConfiguration {
    let backend_resolution = config.io_backend.resolve();
    let io_backend = match backend_resolution.effective {
        graphoxide_index_runtime::EffectiveIoBackend::Threaded => {
            graphoxide_cli::build_telemetry::RuntimeIoBackend::Threaded
        }
    };
    let io_backend_request = match backend_resolution.requested {
        graphoxide_index_runtime::IoBackendSelection::Auto => {
            graphoxide_cli::build_telemetry::RuntimeIoBackendRequest::Auto
        }
        graphoxide_index_runtime::IoBackendSelection::Threaded => {
            graphoxide_cli::build_telemetry::RuntimeIoBackendRequest::Threaded
        }
        graphoxide_index_runtime::IoBackendSelection::IoUring => {
            graphoxide_cli::build_telemetry::RuntimeIoBackendRequest::IoUring
        }
    };
    let evidence = config.execution_evidence(admitted_requests);
    graphoxide_cli::build_telemetry::IndexRuntimeConfiguration {
        execution_model: graphoxide_cli::build_telemetry::RuntimeExecutionModel::Isolated,
        io_backend,
        io_backend_request: Some(io_backend_request),
        io_backend_fallback: backend_resolution.fallback_reason.map(str::to_owned),
        memory_budget_bytes: Some(config.memory_budget_bytes),
        io_workers: Some(config.io_workers),
        compute_workers: Some(config.compute_workers),
        read_batch_bytes: Some(config.read_batch_bytes),
        cache_partitions: Some(graphoxide_index_runtime::cache::RUNTIME_CACHE_SHARDS),
        admission: Some(graphoxide_cli::build_telemetry::RuntimeAdmissionTelemetry {
            admitted_requests: evidence.admitted_requests,
            effective_io_workers: evidence.effective_io_workers,
            effective_compute_workers: evidence.effective_compute_workers,
            effective_read_batch_bytes: evidence.effective_read_batch_bytes,
            io_pool_bytes_per_worker: evidence.io_pool_bytes_per_worker,
            io_buffers_bytes: evidence.io_buffers_bytes,
            ready_inputs_bytes: evidence.ready_inputs_bytes,
            cpu_arenas_bytes: evidence.cpu_arenas_bytes,
            cache_and_runs_bytes: evidence.cache_and_runs_bytes,
            query_reserve_bytes: evidence.query_reserve_bytes,
            emergency_reserve_bytes: evidence.emergency_reserve_bytes,
        }),
    }
}

fn cluster_with_resource_gate(graph: &mut graphoxide_core::KnowledgeGraph) -> anyhow::Result<()> {
    graphoxide_graph::ClusterResourceLimits::default().check(graph)?;
    graphoxide_graph::cluster(graph)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProjectBuildWorkflow {
    Extract { legacy_executor: bool },
    Index,
}

impl ProjectBuildWorkflow {
    const fn legacy_executor(self) -> bool {
        match self {
            Self::Extract { legacy_executor } => legacy_executor,
            Self::Index => false,
        }
    }

    const fn operation(self) -> graphoxide_cli::build_telemetry::BuildOperation {
        match self {
            Self::Extract { .. } => graphoxide_cli::build_telemetry::BuildOperation::Extract,
            Self::Index => graphoxide_cli::build_telemetry::BuildOperation::Index,
        }
    }

    const fn is_index(self) -> bool {
        matches!(self, Self::Index)
    }
}

fn run_project_build(
    options: ProjectBuildOptions,
    workflow: ProjectBuildWorkflow,
) -> anyhow::Result<()> {
    let cancellation = graphoxide_index_runtime::RuntimeCancellation::new();
    if workflow.is_index() {
        let interrupt = cancellation.clone();
        ctrlc::set_handler(move || interrupt.cancel())
            .context("install index Ctrl-C cancellation handler")?;
    }
    run_project_build_with_cancellation(options, workflow, cancellation)
}

fn acquire_project_build_lock(
    output_directory: &std::path::Path,
    cancellation: &graphoxide_index_runtime::RuntimeCancellation,
    progress: &mut BuildProgressReporter,
) -> anyhow::Result<watch_service::RebuildLockGuard> {
    const RETRY_INTERVAL: Duration = Duration::from_millis(25);
    let mut announced_wait = false;
    loop {
        anyhow::ensure!(
            !cancellation.is_cancelled(),
            "project build cancelled while waiting for the rebuild lock"
        );
        if let Some(lock) = watch_service::RebuildLockGuard::acquire(output_directory, false)? {
            return Ok(lock);
        }
        if !announced_wait {
            if progress.emits_legacy_wait_diagnostic() {
                // Preserve the established actionable diagnostic for default
                // piped stderr. Other enabled modes own their path-free phase.
                eprintln!(
                    "[graphoxide] waiting for the rebuild lock at {}",
                    output_directory.display()
                );
            }
            progress.phase(BuildProgressPhase::Waiting);
            announced_wait = true;
        }
        thread::sleep(RETRY_INTERVAL);
    }
}

fn run_project_build_with_cancellation(
    options: ProjectBuildOptions,
    workflow: ProjectBuildWorkflow,
    cancellation: graphoxide_index_runtime::RuntimeCancellation,
) -> anyhow::Result<()> {
    let ProjectBuildOptions {
        path,
        code_only,
        no_cluster,
        force,
        postgres,
        allow_partial,
        timing,
        json,
        progress,
        runtime_report,
        runtime,
        out,
        exclude,
        no_gitignore,
    } = options;
    let source_metadata = fs::metadata(&path).with_context(|| {
        format!(
            "project source root {} must already exist and be a directory",
            path.display()
        )
    })?;
    anyhow::ensure!(
        source_metadata.is_dir(),
        "project source root {} must be a directory",
        path.display()
    );
    let legacy_executor = workflow.legacy_executor();
    let total_started = std::time::Instant::now();
    let effective_force = graphoxide_cli::extract_cli::force_enabled(
        force,
        std::env::var("GRAPHOXIDE_FORCE").ok().as_deref(),
        std::env::var("GRAPHIFY_FORCE").ok().as_deref(),
    );
    let output_directory = managed_output_directory(&path, out.as_deref());
    let output = output_directory.join("graph.json");
    let manifest_path = output_directory.join("manifest.json");
    let mut progress_reporter = if effective_force {
        BuildProgressReporter::new(
            workflow.operation(),
            graphoxide_cli::build_telemetry::BuildMode::Full,
            progress.into(),
        )?
    } else {
        BuildProgressReporter::new_adaptive(workflow.operation(), progress.into())?
    };
    progress_reporter.start();
    if let Some(runtime_report) = runtime_report.as_deref() {
        graphoxide_cli::index::validate_runtime_report_destination(
            runtime_report,
            &output_directory,
        )?;
    }
    if workflow.is_index() {
        graphoxide_cli::index::validate_index_prior_artifacts(&output_directory)?;
        graphoxide_cli::index::validate_index_build_config_destinations(&output_directory)?;
    }
    // Both writers share one coherent view of prior state through graph,
    // manifest, coverage, and build-policy publication. A non-index extract
    // must not replace graph.json while index hashes it for association.
    let _build_lock =
        acquire_project_build_lock(&output_directory, &cancellation, &mut progress_reporter)?;
    graphoxide_extract::cache::prepare_structured_redaction_cache_schema(&output_directory)
        .with_context(|| {
            format!(
                "prepare the managed cache schema in {}",
                output_directory.display()
            )
        })?;
    if workflow.is_index() {
        // Recheck after any lock wait and schema retirement so a cooperating
        // publisher cannot leave a newly unsafe prior-state path for the
        // baseline readers below.
        graphoxide_cli::index::validate_index_prior_artifacts(&output_directory)?;
        graphoxide_cli::index::validate_index_build_config_destinations(&output_directory)?;
    }
    // A committed graph is a sufficient carry-forward baseline for an
    // explicitly code-only rebuild. This preserves live semantic
    // records when a fresh clone does not contain the manifest.
    let incremental_mode =
        !effective_force && output.is_file() && (manifest_path.is_file() || code_only);
    if incremental_mode && !json {
        write_output(if legacy_executor {
            "Incremental scan: reusing unchanged extraction cache entries."
        } else {
            "Incremental scan: isolating I/O and extracting only content-hash changes."
        })?;
    }
    let mode = if incremental_mode {
        graphoxide_cli::build_telemetry::BuildMode::Incremental
    } else {
        graphoxide_cli::build_telemetry::BuildMode::Full
    };
    let mut telemetry = graphoxide_cli::build_telemetry::BuildTelemetry::new(
        workflow.operation(),
        mode,
        graphoxide_cli::build_telemetry::BuildStatus::Rebuilt,
        output.clone(),
    );
    let persisted = if workflow.is_index() {
        graphoxide_cli::index::read_index_build_config(&output_directory)?
    } else {
        watch_service::read_build_config(&output_directory)
    };
    let prepared_index_build_config = if workflow.is_index() {
        Some(graphoxide_cli::index::prepare_index_build_config(
            persisted.clone(),
            (!exclude.is_empty()).then_some(exclude.as_slice()),
            no_gitignore.then_some(false),
            !no_cluster,
        )?)
    } else {
        None
    };
    let effective_excludes = if exclude.is_empty() {
        persisted.excludes.clone()
    } else {
        exclude.clone()
    };
    let honor_gitignore = !no_gitignore && persisted.honor_gitignore;
    let runtime_config = if workflow.is_index() {
        Some(runtime.resolve()?)
    } else {
        runtime.resolve_for_executor(legacy_executor)?
    };
    let graph_memory_budget = runtime_config.map_or(
        graphoxide_graph::DEFAULT_FACT_MATERIALIZATION_MAX_BYTES,
        |config| config.memory_budget().cache_and_runs_bytes,
    );
    let scan_started = std::time::Instant::now();
    let detect_options = graphoxide_extract::detect::DetectOptions {
        extra_excludes: effective_excludes,
        output_dir: Some(output_directory.clone()),
        honor_gitignore,
        ..Default::default()
    };
    let mut coverage_report = if workflow.is_index() {
        progress_reporter.phase(BuildProgressPhase::Auditing);
        let mut coverage_options =
            graphoxide_extract::coverage::CoverageOptions::from(&detect_options);
        coverage_options.code_only = code_only;
        let report = graphoxide_extract::coverage::audit_coverage_with_cancellation(
            &path,
            &coverage_options,
            &cancellation,
        )?;
        graphoxide_cli::index::validate_coverage_for_index(&report, allow_partial)?;
        Some(report)
    } else {
        None
    };
    progress_reporter.phase(BuildProgressPhase::Scanning);
    let extraction_progress = progress_reporter.counter_emitter(BuildProgressPhase::Extracting);
    let (scan, runtime_extraction_telemetry, indexed_source_bytes) = if let Some(runtime_config) =
        runtime_config
    {
        match (runtime_report.is_some(), extraction_progress) {
            (true, Some(progress)) => {
                let scan = graphoxide_extract::extract_project_with_runtime_scan_options_deferred_manifest_with_cancellation_and_telemetry_and_progress(
                    &path,
                    effective_force,
                    &output_directory,
                    code_only,
                    &detect_options,
                    runtime_config,
                    cancellation.clone(),
                    progress,
                )?;
                (
                    scan.result,
                    Some(scan.telemetry),
                    Some(scan.indexed_source_bytes),
                )
            }
            (true, None) => {
                let scan = if workflow.is_index() {
                    graphoxide_extract::extract_project_with_runtime_scan_options_deferred_manifest_with_cancellation_and_telemetry(
                        &path,
                        effective_force,
                        &output_directory,
                        code_only,
                        &detect_options,
                        runtime_config,
                        cancellation.clone(),
                    )?
                } else {
                    graphoxide_extract::extract_project_with_runtime_scan_options_deferred_manifest_with_telemetry(
                        &path,
                        effective_force,
                        &output_directory,
                        code_only,
                        &detect_options,
                        runtime_config,
                    )?
                };
                (scan.result, Some(scan.telemetry), None)
            }
            (false, Some(progress)) => {
                let scan = graphoxide_extract::extract_project_with_runtime_scan_options_deferred_manifest_with_cancellation_and_progress(
                    &path,
                    effective_force,
                    &output_directory,
                    code_only,
                    &detect_options,
                    runtime_config,
                    cancellation.clone(),
                    progress,
                )?;
                (
                    scan.result,
                    Some(scan.telemetry),
                    Some(scan.indexed_source_bytes),
                )
            }
            (false, None) => {
                let scan = if workflow.is_index() {
                    graphoxide_extract::extract_project_with_runtime_scan_options_deferred_manifest_with_cancellation(
                        &path,
                        effective_force,
                        &output_directory,
                        code_only,
                        &detect_options,
                        runtime_config,
                        cancellation.clone(),
                    )?
                } else {
                    graphoxide_extract::extract_project_with_runtime_scan_options_deferred_manifest(
                        &path,
                        effective_force,
                        &output_directory,
                        code_only,
                        &detect_options,
                        runtime_config,
                    )?
                };
                (scan, None, None)
            }
        }
    } else {
        if let Some(progress) = extraction_progress {
            (
                graphoxide_extract::extract_project_with_scan_options_deferred_manifest_with_progress(
                    &path,
                    effective_force,
                    &output_directory,
                    code_only,
                    &detect_options,
                    progress,
                )?,
                None,
                None,
            )
        } else {
            (
                graphoxide_extract::extract_project_with_scan_options_deferred_manifest(
                    &path,
                    effective_force,
                    &output_directory,
                    code_only,
                    &detect_options,
                )?,
                None,
                None,
            )
        }
    };
    progress_reporter.set_indexed_inputs(scan.progress.succeeded);
    let runtime_telemetry = runtime_config
        .map(|config| isolated_runtime_configuration(config, scan.detection.total_files));
    let runtime_cache_telemetry = runtime_extraction_telemetry.map(|runtime| {
        graphoxide_cli::build_telemetry::RuntimeCacheTelemetryV2::from_runtime(
            scan.runtime_cache,
            runtime.cache_io,
        )
    });
    let runtime_io_telemetry = runtime_extraction_telemetry.map(|runtime| runtime.io);
    let runtime_work_telemetry = runtime_extraction_telemetry.map(|runtime| runtime.work);
    telemetry.stages_ms.scan_extract =
        graphoxide_cli::build_telemetry::elapsed_millis(scan_started);
    telemetry.files.detected = scan.detection.total_files;
    telemetry.files.processed = scan.changed_sources;
    telemetry.files.changed = scan.changed_sources;
    telemetry.files.unchanged = scan.unchanged_sources;
    telemetry.files.deleted = scan.deleted_sources;
    telemetry.files.unclassified = scan.detection.unclassified.len();
    telemetry.files.sensitive = scan.detection.skipped_sensitive.len();
    telemetry.warnings.extend(scan.detection.warning.clone());
    telemetry
        .warnings
        .extend(scan.detection.walk_errors.iter().cloned());
    telemetry
        .warnings
        .extend(scan.runtime_cache_diagnostics.iter().cloned());
    telemetry.warnings.extend(scan.warnings.iter().cloned());
    for warning in &scan.runtime_cache_diagnostics {
        eprintln!("{warning}");
    }
    for warning in &scan.warnings {
        eprintln!("{warning}");
    }
    let build_progress = graphoxide_cli::build_guard::BuildProgress::new(
        scan.progress.total,
        scan.progress.succeeded,
    )?
    .ensure_any_success("local extraction")?;
    if code_only {
        let skipped = scan
            .detection
            .files
            .iter()
            .filter(|(kind, _)| kind.as_str() != "code")
            .map(|(_, files)| files.len())
            .sum::<usize>();
        telemetry.files.skipped = telemetry.files.skipped.saturating_add(skipped);
        if !json {
            write_output(&format!(
                "--code-only: skipping {skipped} non-code input(s)"
            ))?;
        }
    }
    for skipped in &scan.detection.skipped_sensitive {
        eprintln!("skipped as potentially sensitive: {skipped}");
    }
    let mut extractions = scan.extractions;
    let rebuilt_sources = scan.rebuilt_sources;
    let verified_representation_sources = scan.verified_representation_sources;
    let mut ownership_prune_sources = scan.ownership_prune_sources;
    let pending_manifest = scan.pending_manifest;
    let mut rebuilt_provider_sources = Vec::new();
    if let Some(dsn) = postgres.as_deref() {
        let extraction = graphoxide_extract::pg_introspect::introspect_postgres(
            (!dsn.is_empty()).then_some(dsn),
        )?;
        if let Some(source) = extraction
            .nodes
            .first()
            .map(|node| node.source_file.clone())
        {
            rebuilt_provider_sources.push(source);
        }
        extractions.push(extraction);
    }
    rebuilt_provider_sources.sort();
    rebuilt_provider_sources.dedup();
    let retained_output_bytes = graphoxide_extract::extractions_retained_bytes(&extractions)?;
    debug_assert!(retained_output_bytes >= scan.retained_output_bytes);
    let pending_manifest_retained_bytes = scan.pending_manifest_retained_bytes;
    let graph_budget_without_baseline =
        graph_budget_after_pending_manifest(graph_memory_budget, pending_manifest_retained_bytes)?;
    let (previous, graph_materialization_budget) = if incremental_mode {
        let budget = incremental_graph_budget_after_retained_scan(
            graph_memory_budget,
            retained_output_bytes,
            pending_manifest_retained_bytes,
        )?;
        let (previous, materialization_budget) =
            read_incremental_baseline(&output, graph_memory_budget, budget)?;
        (Some(previous), materialization_budget)
    } else if !no_cluster && output.is_file() {
        // A forced/full rebuild may recover from an unreadable or
        // over-budget baseline; community remapping is best-effort in
        // that mode and never weakens the new build's own bound.
        incremental_graph_budget_after_retained_scan(
            graph_memory_budget,
            retained_output_bytes,
            pending_manifest_retained_bytes,
        )
        .ok()
        .and_then(|budget| read_incremental_baseline(&output, graph_memory_budget, budget).ok())
        .filter(|(_, materialization_budget)| {
            optional_baseline_leaves_full_graph_headroom(
                retained_output_bytes,
                *materialization_budget,
            )
        })
        .map_or(
            (None, graph_budget_without_baseline),
            |(previous, budget)| (Some(previous), budget),
        )
    } else {
        (None, graph_budget_without_baseline)
    };
    if incremental_mode && let Some(baseline) = previous.as_ref() {
        ownership_prune_sources = gate_baseline_representation_resets(
            baseline,
            &scan.detection,
            &ownership_prune_sources,
            &rebuilt_sources,
            &verified_representation_sources,
            &path,
        )?;
        ensure_incremental_baseline_representation_is_verified(
            baseline,
            &scan.detection,
            &ownership_prune_sources,
            &rebuilt_sources,
            &verified_representation_sources,
            &path,
        )?;
    }
    let scan_detection = scan.detection;
    let live_sources = scan_detection
        .files
        .values()
        .flatten()
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    let mut prune_sources = previous
        .as_ref()
        .map(|graph| stale_local_sources(graph, &path, &live_sources))
        .unwrap_or_default();
    prune_sources.sort();
    prune_sources.dedup();
    progress_reporter.phase(BuildProgressPhase::Building);
    let build_started = std::time::Instant::now();
    if no_cluster {
        graphoxide_graph::disambiguate_file_labels_in_extractions(&mut extractions);
        if incremental_mode {
            let fresh = flatten_extractions(extractions);
            let baseline = previous
                .as_ref()
                .expect("incremental mode loads a required baseline");
            extractions = vec![graphoxide_graph::incremental::merge_raw_extraction_from_graph_with_rebuilt_sources_and_ownership_resets_and_materialization_limit(
                        fresh,
                        baseline,
                        &rebuilt_sources,
                        &rebuilt_provider_sources,
                        graphoxide_graph::incremental::IncrementalBaselinePrunes {
                            deletion_sources: &prune_sources,
                            ownership_reset_sources: &ownership_prune_sources,
                        },
                        Some(&path),
                        graph_materialization_budget,
                    )?];
        }
        extractions = vec![graphoxide_graph::dedupe_raw_extractions(&extractions)];
        telemetry.stages_ms.build = graphoxide_cli::build_telemetry::elapsed_millis(build_started);
        // An incomplete-build shrink check may read the committed
        // graph again. Release the inspected baseline first so that
        // check cannot create a second retained whole-graph copy.
        drop(previous);
        progress_reporter.phase(BuildProgressPhase::Publishing);
        let write_started = std::time::Instant::now();
        let mut published_coverage = None;
        let outcome = if workflow.is_index() {
            let report = coverage_report
                .take()
                .expect("index prepares coverage before graph construction");
            graphoxide_cli::build_guard::commit_index_build(
                &output,
                graphoxide_cli::build_guard::BuildArtifact::Raw(&extractions),
                build_progress,
                allow_partial,
                &cancellation,
                || pending_manifest.commit_strict(),
                || {
                    published_coverage = Some(graphoxide_cli::index::publish_associated_coverage(
                        &output_directory,
                        &output,
                        report,
                    )?);
                    Ok(())
                },
            )?
        } else {
            graphoxide_cli::build_guard::commit_build(
                &output,
                graphoxide_cli::build_guard::BuildArtifact::Raw(&extractions),
                build_progress,
                allow_partial,
                || pending_manifest.commit(),
            )?
        };
        if outcome == graphoxide_cli::build_guard::BuildCommitOutcome::RefusedShrink {
            telemetry.stages_ms.write =
                graphoxide_cli::build_telemetry::elapsed_millis(write_started);
            telemetry.elapsed_ms = graphoxide_cli::build_telemetry::elapsed_millis(total_started);
            telemetry.status = graphoxide_cli::build_telemetry::BuildStatus::RefusedShrink;
            progress_reporter.complete(&telemetry, indexed_source_bytes);
            anyhow::bail!("{outcome}");
        }
        let nodes: usize = extractions.iter().map(|e| e.nodes.len()).sum();
        let edges: usize = extractions.iter().map(|e| e.edges.len()).sum();
        if workflow.is_index() {
            graphoxide_cli::index::write_prepared_index_build_config(
                &output_directory,
                prepared_index_build_config
                    .as_ref()
                    .expect("index validates its build config before extraction"),
            )?;
        } else {
            save_build_config_in(
                &output_directory,
                true,
                (!exclude.is_empty()).then_some(exclude.as_slice()),
                no_gitignore.then_some(false),
            )?;
        }
        telemetry.stages_ms.write = graphoxide_cli::build_telemetry::elapsed_millis(write_started);
        telemetry.elapsed_ms = graphoxide_cli::build_telemetry::elapsed_millis(total_started);
        telemetry.graph.nodes = nodes;
        telemetry.graph.edges = edges;
        telemetry.graph.clustered = false;
        let human = format!(
            "Wrote {nodes} nodes and {edges} edges to {} in {}",
            output.display(),
            graphoxide_cli::build_telemetry::format_elapsed(telemetry.elapsed_ms)
        );
        write_runtime_report_if_requested(
            &telemetry,
            runtime_report.as_deref(),
            runtime_telemetry.as_ref(),
            runtime_io_telemetry,
            runtime_work_telemetry,
            runtime_cache_telemetry,
        )?;
        emit_project_build_report(
            &telemetry,
            json,
            timing,
            &human,
            published_coverage.as_ref(),
        )?;
        progress_reporter.complete(&telemetry, indexed_source_bytes);
        return Ok(());
    }
    let (staged_extractions, build_options, normalization_root) = if incremental_mode {
        let fresh = flatten_extractions(extractions);
        let baseline = previous
            .as_ref()
            .expect("incremental mode loads a required baseline");
        let merged = graphoxide_graph::incremental::merge_raw_extraction_from_graph_with_rebuilt_sources_and_ownership_resets_and_materialization_limit(
                    fresh,
                    baseline,
                    &rebuilt_sources,
                    &rebuilt_provider_sources,
                    graphoxide_graph::incremental::IncrementalBaselinePrunes {
                        deletion_sources: &prune_sources,
                        ownership_reset_sources: &ownership_prune_sources,
                    },
                    Some(&path),
                    graph_materialization_budget,
                )?;
        (
            vec![merged],
            graphoxide_graph::BuildOptions {
                directed: previous.as_ref().is_some_and(|graph| graph.directed),
                ..graphoxide_graph::BuildOptions::default()
            },
            Some(path.as_path()),
        )
    } else {
        (extractions, graphoxide_graph::BuildOptions::default(), None)
    };
    let build_emitter = progress_reporter.counter_emitter(BuildProgressPhase::Building);
    let sub_stage_emitter: Option<
        std::sync::Arc<dyn Fn(graphoxide_graph::BuildSubStage) + Send + Sync>,
    > = progress_reporter.phase_emitter().map(
        |emit: std::sync::Arc<dyn Fn(BuildProgressPhase) + Send + Sync>| {
            let adapter: std::sync::Arc<dyn Fn(graphoxide_graph::BuildSubStage) + Send + Sync> =
                std::sync::Arc::new(move |stage: graphoxide_graph::BuildSubStage| {
                    let phase = match stage {
                        graphoxide_graph::BuildSubStage::Normalizing => {
                            BuildProgressPhase::Building
                        }
                        graphoxide_graph::BuildSubStage::ResolvingSemanticIds => {
                            BuildProgressPhase::Building
                        }
                        graphoxide_graph::BuildSubStage::MergingNodes => {
                            BuildProgressPhase::MergingNodes
                        }
                        graphoxide_graph::BuildSubStage::ResolvingTwins => {
                            BuildProgressPhase::Building
                        }
                        graphoxide_graph::BuildSubStage::IndexingAliases => {
                            BuildProgressPhase::Building
                        }
                        graphoxide_graph::BuildSubStage::ResolvingEdges => {
                            BuildProgressPhase::ResolvingEdges
                        }
                        graphoxide_graph::BuildSubStage::ResolvingHyperedges => {
                            BuildProgressPhase::ResolvingEdges
                        }
                        graphoxide_graph::BuildSubStage::Deduplicating => {
                            BuildProgressPhase::Deduplicating
                        }
                        graphoxide_graph::BuildSubStage::DisambiguatingLabels => {
                            BuildProgressPhase::Building
                        }
                    };
                    (emit)(phase);
                });
            adapter
        },
    );
    let sub_stage_ref = sub_stage_emitter.as_deref();
    let mut graph = graphoxide_cli::build_guard::stage_graph_from_extractions_with_materialization_limit_and_root_and_substage(
                staged_extractions,
                &output_directory,
                build_options,
                graph_materialization_budget,
                normalization_root,
                build_emitter.as_ref().map(|e| e.as_ref()),
                sub_stage_ref,
            )?
            .into_parts()
            .0;
    telemetry.stages_ms.build = graphoxide_cli::build_telemetry::elapsed_millis(build_started);
    progress_reporter.phase(BuildProgressPhase::Clustering);
    let cluster_started = std::time::Instant::now();
    cluster_with_resource_gate(&mut graph)?;
    if let Some(previous) = &previous {
        graphoxide_graph::cluster::remap_communities_to_previous(&mut graph, previous);
    }
    drop(previous);
    telemetry.stages_ms.cluster = graphoxide_cli::build_telemetry::elapsed_millis(cluster_started);
    progress_reporter.phase(BuildProgressPhase::Publishing);
    let write_started = std::time::Instant::now();
    let mut published_coverage = None;
    let outcome = if workflow.is_index() {
        let report = coverage_report
            .take()
            .expect("index prepares coverage before graph construction");
        graphoxide_cli::build_guard::commit_index_build(
            &output,
            graphoxide_cli::build_guard::BuildArtifact::Graph(&graph),
            build_progress,
            allow_partial,
            &cancellation,
            || pending_manifest.commit_strict(),
            || {
                published_coverage = Some(graphoxide_cli::index::publish_associated_coverage(
                    &output_directory,
                    &output,
                    report,
                )?);
                Ok(())
            },
        )?
    } else {
        graphoxide_cli::build_guard::commit_build(
            &output,
            graphoxide_cli::build_guard::BuildArtifact::Graph(&graph),
            build_progress,
            allow_partial,
            || pending_manifest.commit(),
        )?
    };
    if outcome == graphoxide_cli::build_guard::BuildCommitOutcome::RefusedShrink {
        telemetry.stages_ms.write = graphoxide_cli::build_telemetry::elapsed_millis(write_started);
        telemetry.elapsed_ms = graphoxide_cli::build_telemetry::elapsed_millis(total_started);
        telemetry.status = graphoxide_cli::build_telemetry::BuildStatus::RefusedShrink;
        progress_reporter.complete(&telemetry, indexed_source_bytes);
        anyhow::bail!("{outcome}");
    }
    if workflow.is_index() {
        graphoxide_cli::index::write_prepared_index_build_config(
            &output_directory,
            prepared_index_build_config
                .as_ref()
                .expect("index validates its build config before extraction"),
        )?;
    } else {
        save_build_config_in(
            &output_directory,
            false,
            (!exclude.is_empty()).then_some(exclude.as_slice()),
            no_gitignore.then_some(false),
        )?;
    }
    telemetry.stages_ms.write = graphoxide_cli::build_telemetry::elapsed_millis(write_started);
    telemetry.elapsed_ms = graphoxide_cli::build_telemetry::elapsed_millis(total_started);
    telemetry.graph.nodes = graph.nodes.len();
    telemetry.graph.edges = graph.links.len();
    telemetry.graph.clustered = true;
    let human = format!(
        "Wrote {} nodes and {} edges to {} in {}",
        graph.nodes.len(),
        graph.links.len(),
        output.display(),
        graphoxide_cli::build_telemetry::format_elapsed(telemetry.elapsed_ms)
    );
    write_runtime_report_if_requested(
        &telemetry,
        runtime_report.as_deref(),
        runtime_telemetry.as_ref(),
        runtime_io_telemetry,
        runtime_work_telemetry,
        runtime_cache_telemetry,
    )?;
    emit_project_build_report(
        &telemetry,
        json,
        timing,
        &human,
        published_coverage.as_ref(),
    )?;
    progress_reporter.complete(&telemetry, indexed_source_bytes);
    Ok(())
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    match cli.command {
        Command::Enrich { args } => write_output(&graphoxide_cli::enrich::run(args)?),
        Command::Formats { json } => write_output(&format_capability_output(json)?),
        Command::Extract {
            build,
            legacy_executor,
        } => run_project_build(build, ProjectBuildWorkflow::Extract { legacy_executor }),
        Command::Index { build } => run_project_build(build, ProjectBuildWorkflow::Index),
        Command::Audit {
            path,
            coverage_path,
            json,
            strict,
            force,
        } => {
            if path.as_os_str() == std::ffi::OsStr::new("coverage") {
                run_coverage_audit(
                    coverage_path
                        .as_deref()
                        .unwrap_or_else(|| std::path::Path::new(".")),
                    json,
                    strict,
                    force,
                )
            } else {
                anyhow::ensure!(
                    coverage_path.is_none(),
                    "unexpected second audit path {}; use `graphoxide audit coverage [PATH]` for a coverage report",
                    coverage_path
                        .as_deref()
                        .expect("checked as present")
                        .display()
                );
                run_audit(&path, json, strict, force)
            }
        }
        Command::Diagnose { args } => run_diagnose(&args),
        Command::Query {
            question,
            budget,
            graph,
            dfs,
            contexts,
        } => {
            let started = std::time::Instant::now();
            let graph = resolve_managed_graph_path(graph);
            let graph_data = graphoxide_core::read_graph(&graph)?;
            let (contexts, context_source) =
                graphoxide_query::resolve_context_filters(&question, &contexts);
            let mut result = if dfs {
                graphoxide_query::query_graph_dfs_filtered(
                    &graph_data,
                    &question,
                    2,
                    budget,
                    &contexts,
                )
            } else {
                graphoxide_query::query_graph_filtered(&graph_data, &question, 2, budget, &contexts)
            };
            if let Some(source) = context_source {
                annotate_query_context(&mut result, &contexts, source);
            }
            record_query("query", &question, &graph, &result, started.elapsed());
            write_output(&result)
        }
        Command::Path { a, b, graph } => {
            let started = std::time::Instant::now();
            let graph = resolve_managed_graph_path(graph);
            let graph_data = graphoxide_core::read_graph(&graph)?;
            let result = graphoxide_query::shortest_path(&graph_data, &a, &b);
            record_query(
                "path",
                &format!("{a} -> {b}"),
                &graph,
                &result,
                started.elapsed(),
            );
            write_output(&result)
        }
        Command::Explain { node, graph } => {
            let started = std::time::Instant::now();
            let graph = resolve_managed_graph_path(graph);
            let graph_data = graphoxide_core::read_graph(&graph)?;
            let overlay = load_learning_overlay(&graph);
            let result =
                graphoxide_query::explain_with_overlay(&graph_data, &node, overlay.as_ref());
            record_query("explain", &node, &graph, &result, started.elapsed());
            write_output(&result)
        }
        Command::Affected {
            node,
            depth,
            relations,
            graph,
        } => {
            let started = std::time::Instant::now();
            let graph_data = graphoxide_core::read_graph(&graph)?;
            let result = graphoxide_query::affected(&graph_data, &node, depth, &relations);
            record_query("affected", &node, &graph, &result, started.elapsed());
            write_output(&result)
        }
        Command::GodNodes { top, json, graph } => {
            let graph = graphoxide_core::read_graph(graph)?;
            let nodes = graphoxide_query::god_nodes(&graph, top);
            let output = format_god_nodes(&nodes, json)?;
            write_output(&output)
        }
        Command::SaveResult {
            question,
            answer,
            answer_file,
            query_type,
            nodes,
            outcome,
            correction,
            memory_dir,
        } => {
            let answer = if let Some(path) = answer_file {
                std::fs::read_to_string(&path)
                    .with_context(|| format!("read answer file {}", path.display()))?
                    .trim()
                    .to_owned()
            } else {
                answer.ok_or_else(|| anyhow::anyhow!("--answer or --answer-file is required"))?
            };
            let output = graphoxide_core::save_query_result(
                &question,
                &answer,
                &memory_dir,
                &graphoxide_core::SaveResultOptions {
                    query_type,
                    source_nodes: nodes,
                    outcome,
                    correction,
                    ..Default::default()
                },
            )?;
            write_output(&format!("Saved to {}", output.display()))
        }
        Command::Reflect {
            memory_dir,
            out,
            graph,
            analysis,
            labels,
            half_life_days,
            min_corroboration,
            if_stale,
        } => {
            let graph = graph.or_else(|| {
                let candidate =
                    managed_output_directory(std::path::Path::new("."), None).join("graph.json");
                candidate.exists().then_some(candidate)
            });
            let analysis = analysis.or_else(|| {
                graph.as_ref().map(|path| {
                    path.parent()
                        .unwrap_or_else(|| std::path::Path::new("."))
                        .join(".graphify_analysis.json")
                })
            });
            let labels = labels.or_else(|| {
                graph.as_ref().map(|path| {
                    path.parent()
                        .unwrap_or_else(|| std::path::Path::new("."))
                        .join(".graphify_labels.json")
                })
            });
            if if_stale
                && graphoxide_core::lessons_fresh(
                    &out,
                    &memory_dir,
                    graph.as_deref(),
                    analysis.as_deref(),
                    labels.as_deref(),
                )
            {
                return write_output(&format!(
                    "Lessons already up to date -> {} (skipped; omit --if-stale to force)",
                    out.display()
                ));
            }
            let (output, aggregate) = graphoxide_core::reflect(
                &memory_dir,
                &out,
                &graphoxide_core::ReflectOptions {
                    graph_path: graph,
                    analysis_path: analysis,
                    labels_path: labels,
                    half_life_days,
                    min_corroboration,
                    ..Default::default()
                },
            )?;
            write_output(&format!(
                "Reflected {} memories ({} useful, {} dead ends, {} corrected) -> {}",
                aggregate.total,
                aggregate.counts.useful,
                aggregate.counts.dead_end,
                aggregate.counts.corrected,
                output.display()
            ))
        }
        Command::Update {
            path,
            force,
            no_cluster,
            json,
            progress,
            runtime_report,
            legacy_executor,
            runtime,
        } => rebuild(
            &path,
            no_cluster,
            force,
            json,
            progress,
            runtime_report.as_deref(),
            legacy_executor,
            runtime,
        ),
        Command::ClusterOnly {
            path,
            graph: graph_override,
            no_viz: _,
            no_label,
        } => {
            let known_managed_directory = graph_override
                .is_none()
                .then(|| path.is_dir())
                .filter(|is_directory| *is_directory)
                .map(|_| managed_output_directory(&path, None));
            let sidecar_directory = graph_override.as_ref().map_or_else(
                || {
                    if path.is_dir() {
                        managed_output_directory(&path, None)
                    } else {
                        path.parent()
                            .unwrap_or_else(|| std::path::Path::new("."))
                            .to_path_buf()
                    }
                },
                |graph| {
                    let parent = graph.parent().unwrap_or_else(|| std::path::Path::new("."));
                    if matches!(
                        parent.file_name().and_then(|value| value.to_str()),
                        Some("graphoxide-out" | "graphify-out")
                    ) {
                        parent.to_path_buf()
                    } else {
                        managed_output_directory(&path, None)
                    }
                },
            );
            let graph_path = if let Some(graph) = graph_override {
                graph
            } else if path.is_dir() {
                managed_output_directory(&path, None).join("graph.json")
            } else {
                path
            };
            let _managed_graph_lock =
                acquire_managed_graph_lock(&graph_path, known_managed_directory.as_deref())?;
            let mut graph = graphoxide_core::read_graph(&graph_path)?;
            let mut previous = graph.clone();
            let labels = load_community_labels(
                graph_path
                    .parent()
                    .unwrap_or_else(|| std::path::Path::new(".")),
            );
            if !labels.is_empty() {
                for node in &mut previous.nodes {
                    if let Some(label) = node.community.and_then(|cid| labels.get(&cid)) {
                        node.extra
                            .insert("community_name".into(), label.clone().into());
                    }
                }
            }
            remove_placeholder_community_names(&mut previous);
            cluster_with_resource_gate(&mut graph)?;
            graphoxide_graph::cluster::remap_communities_to_previous(&mut graph, &previous);
            graphoxide_core::write_graph_atomic(&graph_path, &graph, true)?;
            write_cluster_sidecars(&sidecar_directory, &graph, !no_label)?;
            write_output(&format!("Reclustered {} nodes", graph.nodes.len()))
        }
        Command::Label {
            path,
            backend,
            model,
            missing_only,
            max_concurrency,
            batch_size,
            timeout_seconds,
        } => label_communities(
            &path,
            backend.as_deref(),
            model.as_deref(),
            missing_only,
            max_concurrency,
            batch_size,
            timeout_seconds,
        ),
        Command::Report { graph, output } => {
            let graph = graphoxide_core::read_graph(graph)?;
            let analysis = graphoxide_graph::analyze(&graph)?;
            write_text(
                &output,
                &graphoxide_export::render_report(&graph, &analysis),
            )?;
            write_output(&format!("Wrote {}", output.display()))
        }
        Command::Export {
            format,
            positional,
            output,
            graph,
            directory,
            no_viz,
            max_sections,
        } => run_export(
            &format,
            positional,
            output,
            graph,
            directory,
            no_viz,
            max_sections,
        ),
        Command::Benchmark {
            question,
            iterations,
            graph,
        } => {
            let graph = graphoxide_core::read_graph(graph)?;
            let start = std::time::Instant::now();
            for _ in 0..iterations {
                std::hint::black_box(graphoxide_query::query_graph(&graph, &question, 2, 2000));
            }
            let elapsed = start.elapsed().as_secs_f64() * 1000.0;
            write_output(&format!(
                "{iterations} queries in {elapsed:.3} ms ({:.3} ms/query)",
                elapsed / iterations.max(1) as f64
            ))
        }
        Command::MergeGraphs {
            inputs,
            output,
            force,
        } => {
            let _managed_graph_lock = acquire_managed_graph_lock(&output, None)?;
            let mut graphs = Vec::new();
            for input in inputs {
                let graph = graphoxide_core::read_graph(&input)?;
                graphs.push((input, graph));
            }
            let graph = graphoxide_graph::merge_repository_graphs(graphs);
            if !graphoxide_core::write_graph_atomic(&output, &graph, force)? {
                anyhow::bail!("refusing to shrink existing output; pass --force")
            }
            write_output(&format!(
                "Wrote {} nodes and {} edges to {}",
                graph.nodes.len(),
                graph.links.len(),
                output.display()
            ))
        }
        Command::Tree {
            root,
            graph,
            output,
        } => {
            let graph = graphoxide_core::read_graph(graph)?;
            let rendered = graphoxide_export::render_tree(&graph, root.as_deref());
            if let Some(output) = output {
                write_text(&output, &rendered)?;
                write_output(&format!("Wrote {}", output.display()))
            } else {
                write_output(&rendered)
            }
        }
        Command::GlobalGraph {
            roots,
            output,
            force,
        } => global_graph(&roots, &output, force),
        Command::Global { command } => global_command(command),
        Command::MergeDriver {
            base,
            ours,
            theirs,
            output,
        } => merge_driver(&base, &ours, &theirs, output.as_deref()),
        Command::CheckUpdate { path } => check_update(&path),
        Command::Watch {
            path,
            force,
            no_cluster,
            runtime_report,
            progress,
            legacy_executor,
            runtime,
        } => watch(
            path,
            force,
            no_cluster,
            runtime_report.as_deref(),
            progress,
            legacy_executor,
            runtime,
        ),
        Command::Install {
            platform,
            platform_flag,
            project,
            strict,
        } => install_agent_platform(platform, platform_flag, project, strict),
        Command::Uninstall {
            platform,
            platform_flag,
            project,
        } => uninstall_agent_platform(platform, platform_flag, project),
        Command::Hook { command } => hook(command),
        Command::Claude { command } => claude(command),
        Command::CodeBuddy { command } => direct_codebuddy_platform(command),
        Command::Devin { command } => direct_devin_platform(command),
        Command::Agents { command } => direct_agents_platform(command),
        Command::Codex { command } => {
            direct_agent_platform(graphoxide_cli::install::Platform::Codex, command)
        }
        Command::Antigravity { command } => {
            direct_agent_platform(graphoxide_cli::install::Platform::Antigravity, command)
        }
        Command::Amp { command } => {
            direct_agent_platform(graphoxide_cli::install::Platform::Amp, command)
        }
        Command::HookGuard { mode, strict } => hook_guard(mode.as_deref(), strict),
        Command::HookCheck => Ok(()),
        Command::HookLaunch { mode, root, log } => {
            graphoxide_cli::hooks::launch_detached(mode.parse()?, &root, &log)?;
            Ok(())
        }
        Command::HookSupervise { mode, root } => {
            graphoxide_cli::hooks::supervise(mode.parse()?, &root)
        }
        Command::HookRebuild {
            mode,
            root,
            legacy_executor,
            runtime,
        } => rebuild_hook(mode.parse()?, &root, legacy_executor, runtime),
        Command::Serve {
            graph,
            transport,
            host,
            port,
            api_key,
            mount_path,
            json_response,
            stateless,
            session_timeout,
        } => {
            if transport == "stdio" {
                graphoxide_mcp::serve_graph(graph)
            } else {
                let api_key = api_key.or_else(|| {
                    std::env::var("GRAPHOXIDE_API_KEY")
                        .ok()
                        .or_else(|| std::env::var("GRAPHIFY_API_KEY").ok())
                });
                let session_timeout = (session_timeout > 0.0)
                    .then(|| std::time::Duration::from_secs_f64(session_timeout));
                graphoxide_mcp::http::serve_http(
                    graph,
                    host,
                    port,
                    graphoxide_mcp::http::HttpOptions {
                        mount_path,
                        api_key,
                        stateless,
                        json_response,
                        session_timeout,
                        max_project_contexts: graphoxide_mcp::http::max_server_contexts_from_env(),
                    },
                )
            }
        }
        Command::Site { path, port } => site::serve(&path, port),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DiagnoseCliOptions {
    graph: PathBuf,
    max_examples: usize,
    directed: Option<bool>,
    json: bool,
    extract_path: Option<PathBuf>,
}

const DIAGNOSE_USAGE: &str = "Usage: graphoxide diagnose multigraph [--graph path] [--json] [--max-examples N] [--directed] [--undirected] [--extract-path path]";

fn parse_diagnose_args(args: &[String]) -> anyhow::Result<DiagnoseCliOptions> {
    if args.first().map(String::as_str) != Some("multigraph") {
        anyhow::bail!(DIAGNOSE_USAGE);
    }
    let mut options = DiagnoseCliOptions {
        graph: PathBuf::from("graphoxide-out/graph.json"),
        max_examples: 5,
        directed: None,
        json: false,
        extract_path: None,
    };
    let mut direction_flag = None;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--graph" => {
                index += 1;
                let Some(path) = args.get(index) else {
                    anyhow::bail!("error: --graph requires a path");
                };
                options.graph = PathBuf::from(path);
            }
            "--json" => options.json = true,
            "--max-examples" => {
                index += 1;
                let Some(raw) = args.get(index) else {
                    anyhow::bail!("error: --max-examples requires an integer");
                };
                let value = raw
                    .parse::<isize>()
                    .map_err(|_| anyhow::anyhow!("error: --max-examples requires an integer"))?;
                anyhow::ensure!(value >= 0, "error: --max-examples must be >= 0");
                options.max_examples = value as usize;
            }
            "--directed" => {
                anyhow::ensure!(
                    direction_flag != Some("undirected"),
                    "error: --directed and --undirected are mutually exclusive"
                );
                direction_flag = Some("directed");
                options.directed = Some(true);
            }
            "--undirected" => {
                anyhow::ensure!(
                    direction_flag != Some("directed"),
                    "error: --directed and --undirected are mutually exclusive"
                );
                direction_flag = Some("undirected");
                options.directed = Some(false);
            }
            "--extract-path" => {
                index += 1;
                let Some(path) = args.get(index) else {
                    anyhow::bail!("error: --extract-path requires a path");
                };
                options.extract_path = Some(PathBuf::from(path));
            }
            unknown => anyhow::bail!("error: unknown diagnose option {unknown}"),
        }
        index += 1;
    }
    Ok(options)
}

fn run_diagnose(args: &[String]) -> anyhow::Result<()> {
    let options = parse_diagnose_args(args)?;
    let graph = resolve_managed_graph_path(options.graph);
    let summary = graphoxide_graph::diagnose_file(
        graph,
        options.directed,
        options.max_examples,
        options.extract_path.as_deref(),
    )?;
    let output = if options.json {
        serde_json::to_string_pretty(&graphoxide_graph::format_diagnostic_json(&summary))?
    } else {
        graphoxide_graph::format_diagnostic_report(&summary)
    };
    write_output(&output)
}

#[derive(Debug, serde::Serialize)]
struct AuditInput {
    extractions: usize,
    nodes: usize,
    edges: usize,
    hyperedges: usize,
    unresolved_calls: usize,
    empty_extractions: usize,
    anchor_only_files: Vec<String>,
}

#[derive(Debug, serde::Serialize)]
struct AuditFinding {
    severity: &'static str,
    code: &'static str,
    source_file: String,
    subject: String,
}

#[derive(Debug, serde::Serialize)]
struct AuditReport {
    root: String,
    input: AuditInput,
    findings: Vec<AuditFinding>,
    build: graphoxide_graph::BuildReport,
    strict_violations: usize,
}

fn run_coverage_audit(
    path: &std::path::Path,
    json: bool,
    strict: bool,
    force: bool,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !force,
        "--force applies to graph extraction audits and cannot be used with `audit coverage`"
    );
    let report = graphoxide_extract::coverage::audit_coverage(
        path,
        &graphoxide_extract::coverage::CoverageOptions::detector_defaults(),
    )?;
    let output = graphoxide_cli::coverage::render_coverage_report(&report, json)?;
    write_output(&output)?;
    let strict_failures = report.strict_failure_count();
    if strict && strict_failures > 0 {
        anyhow::bail!(
            "strict coverage audit failed with {strict_failures} incomplete scan or unreadable file outcome(s)"
        );
    }
    Ok(())
}

fn run_audit(path: &std::path::Path, json: bool, strict: bool, force: bool) -> anyhow::Result<()> {
    let output_directory = managed_output_directory(path, None);
    let _build_lock = watch_service::RebuildLockGuard::acquire(&output_directory, true)?
        .ok_or_else(|| anyhow::anyhow!("failed to acquire the blocking rebuild lock"))?;
    graphoxide_extract::cache::prepare_structured_redaction_cache_schema(&output_directory)
        .with_context(|| {
            format!(
                "prepare the managed cache schema in {}",
                output_directory.display()
            )
        })?;
    let extractions = graphoxide_extract::extract_project_with_options_and_output(
        path,
        force,
        &output_directory,
    )?;
    let (_, build) = graphoxide_graph::build_graph_with_report(&extractions)?;
    let report = audit_report(path, &extractions, build);
    let output = if json {
        serde_json::to_string_pretty(&report)?
    } else {
        render_audit_report(&report)?
    };
    write_output(&output)?;
    if strict && report.strict_violations > 0 {
        anyhow::bail!(
            "strict graph audit failed with {} conservation violation(s)",
            report.strict_violations
        )
    }
    Ok(())
}

fn audit_report(
    root: &std::path::Path,
    extractions: &[graphoxide_core::Extraction],
    build: graphoxide_graph::BuildReport,
) -> AuditReport {
    use std::collections::{BTreeMap, BTreeSet};

    let mut findings = Vec::new();
    let mut all_ids = BTreeSet::new();
    let mut declarations: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut empty_extractions = 0;
    let mut anchor_only_files = Vec::new();

    for (index, extraction) in extractions.iter().enumerate() {
        let source_file = extraction
            .nodes
            .first()
            .map(|node| node.source_file.as_str())
            .or_else(|| {
                extraction
                    .edges
                    .first()
                    .map(|edge| edge.source_file.as_str())
            })
            .filter(|source| !source.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("<extraction:{index}>"));
        if extraction.nodes.is_empty() && extraction.edges.is_empty() {
            empty_extractions += 1;
            findings.push(AuditFinding {
                severity: "warning",
                code: "empty_extraction",
                source_file,
                subject: "extractor emitted neither nodes nor edges".into(),
            });
            continue;
        }

        let mut local_ids = BTreeSet::new();
        for node in &extraction.nodes {
            if node.id.is_empty() {
                findings.push(AuditFinding {
                    severity: "error",
                    code: "empty_node_id",
                    source_file: node.source_file.clone(),
                    subject: node.label.clone(),
                });
                continue;
            }
            if !local_ids.insert(node.id.as_str()) {
                findings.push(AuditFinding {
                    severity: "error",
                    code: "duplicate_node_id_in_extraction",
                    source_file: node.source_file.clone(),
                    subject: node.id.clone(),
                });
            }
            all_ids.insert(node.id.as_str());
            declarations
                .entry(node.id.as_str())
                .or_default()
                .push(node.source_file.as_str());
            if node.label.trim().is_empty() {
                findings.push(AuditFinding {
                    severity: "error",
                    code: "empty_node_label",
                    source_file: node.source_file.clone(),
                    subject: node.id.clone(),
                });
            }
            if node.source_file.trim().is_empty()
                && node
                    .extra
                    .get("origin_file")
                    .and_then(|value| value.as_str())
                    .is_none_or(str::is_empty)
            {
                findings.push(AuditFinding {
                    severity: "error",
                    code: "empty_node_source_file",
                    source_file: source_file.clone(),
                    subject: node.id.clone(),
                });
            }
            if node.source_location.as_deref().is_some_and(|location| {
                location
                    .strip_prefix('L')
                    .and_then(|line| line.parse::<usize>().ok())
                    .is_none_or(|line| line == 0)
            }) {
                findings.push(AuditFinding {
                    severity: "error",
                    code: "invalid_source_location",
                    source_file: node.source_file.clone(),
                    subject: format!("{} at {:?}", node.id, node.source_location),
                });
            }
            let parse_error_count = node
                .extra
                .get("parse_error_count")
                .and_then(|value| value.as_u64())
                .unwrap_or(0);
            let missing_node_count = node
                .extra
                .get("missing_node_count")
                .and_then(|value| value.as_u64())
                .unwrap_or(0);
            let compatibility_count = node
                .extra
                .get("parser_compatibility_count")
                .and_then(|value| value.as_u64())
                .unwrap_or(0);
            let unclassified_parser_error = node
                .extra
                .get("parser_has_error")
                .and_then(|value| value.as_bool())
                == Some(true)
                && parse_error_count == 0
                && missing_node_count == 0
                && compatibility_count == 0;
            if unclassified_parser_error || parse_error_count > 0 || missing_node_count > 0 {
                let spans = node
                    .extra
                    .get("parse_error_spans")
                    .and_then(|value| value.as_array())
                    .into_iter()
                    .flatten()
                    .filter_map(|value| value.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                findings.push(AuditFinding {
                    severity: "error",
                    code: "parser_errors",
                    source_file: node.source_file.clone(),
                    subject: if spans.is_empty() {
                        format!(
                            "{parse_error_count} error node(s), {missing_node_count} missing node(s)"
                        )
                    } else {
                        format!(
                            "{parse_error_count} error node(s), {missing_node_count} missing node(s): {spans}"
                        )
                    },
                });
            }
            if compatibility_count > 0 {
                let spans = node
                    .extra
                    .get("parser_compatibility_spans")
                    .and_then(|value| value.as_array())
                    .into_iter()
                    .flatten()
                    .filter_map(|value| value.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                findings.push(AuditFinding {
                    severity: "warning",
                    code: "parser_compatibility",
                    source_file: node.source_file.clone(),
                    subject: format!("{compatibility_count} known grammar ambiguity: {spans}"),
                });
            }
            if let Some(parse_status) = node
                .extra
                .get("parse_status")
                .and_then(|value| value.as_str())
                && !matches!(parse_status, "complete" | "parsed")
                && graphoxide_extract::format_registry::format_registry()
                    .find_by_path(std::path::Path::new(&node.source_file))
                    .is_some_and(|spec| {
                        spec.capability
                            == graphoxide_extract::format_registry::FormatCapability::SemanticFull
                    })
            {
                let diagnostic_count = node
                    .extra
                    .get("dot_diagnostics")
                    .and_then(|value| value.as_array())
                    .map_or_else(
                        || usize::from(node.extra.contains_key("diagnostic")),
                        Vec::len,
                    );
                findings.push(AuditFinding {
                    severity: "error",
                    code: "semantic_parse_incomplete",
                    source_file: node.source_file.clone(),
                    subject: format!(
                        "semantic parser reported {parse_status} with {diagnostic_count} diagnostic(s)"
                    ),
                });
            }
        }
        let only_file_anchor = extraction.nodes.len() == 1
            && extraction.edges.is_empty()
            && extraction.nodes[0]
                .extra
                .get("type")
                .and_then(|value| value.as_str())
                == Some("file");
        if only_file_anchor {
            anchor_only_files.push(source_file.clone());
            findings.push(AuditFinding {
                severity: "warning",
                code: "anchor_only_file",
                source_file,
                subject: "only a file anchor was emitted".into(),
            });
        }
    }

    for (id, sources) in declarations {
        if sources.len() > 1 {
            findings.push(AuditFinding {
                severity: "warning",
                code: "duplicate_node_id_across_extractions",
                source_file: sources.join(", "),
                subject: id.into(),
            });
        }
    }

    let structural_relations = [
        "contains",
        "method",
        "extends",
        "implements",
        "inherits",
        "declares",
    ];
    for extraction in extractions {
        for edge in &extraction.edges {
            if edge.relation.trim().is_empty() {
                findings.push(AuditFinding {
                    severity: "error",
                    code: "empty_edge_relation",
                    source_file: edge.source_file.clone(),
                    subject: format!("{} -> {}", edge.true_source(), edge.true_target()),
                });
            }
            if !all_ids.contains(edge.true_source()) {
                findings.push(AuditFinding {
                    severity: "error",
                    code: "unresolved_edge_source",
                    source_file: edge.source_file.clone(),
                    subject: edge.true_source().into(),
                });
            }
            if !all_ids.contains(edge.true_target()) {
                findings.push(AuditFinding {
                    severity: if structural_relations.contains(&edge.relation.as_str()) {
                        "error"
                    } else {
                        "warning"
                    },
                    code: if edge
                        .extra
                        .get("unresolved_call")
                        .and_then(|value| value.as_bool())
                        == Some(true)
                    {
                        "unresolved_call"
                    } else {
                        "unresolved_edge_target"
                    },
                    source_file: edge.source_file.clone(),
                    subject: format!("{} -> {}", edge.true_source(), edge.true_target()),
                });
            }
        }
    }
    anchor_only_files.sort();
    findings.sort_by(|left, right| {
        (
            left.severity,
            left.code,
            left.source_file.as_str(),
            left.subject.as_str(),
        )
            .cmp(&(
                right.severity,
                right.code,
                right.source_file.as_str(),
                right.subject.as_str(),
            ))
    });

    let finding_errors = findings
        .iter()
        .filter(|finding| finding.severity == "error")
        .count();
    let build_losses = build.node_drops.values().sum::<usize>()
        + build.node_merges.values().sum::<usize>()
        + build.edge_drops.values().sum::<usize>()
        + build.hyperedge_drops.values().sum::<usize>();
    AuditReport {
        root: root.display().to_string(),
        input: AuditInput {
            extractions: extractions.len(),
            nodes: extractions.iter().map(|item| item.nodes.len()).sum(),
            edges: extractions.iter().map(|item| item.edges.len()).sum(),
            hyperedges: extractions.iter().map(|item| item.hyperedges.len()).sum(),
            unresolved_calls: extractions
                .iter()
                .flat_map(|item| &item.edges)
                .filter(|edge| {
                    edge.extra
                        .get("unresolved_call")
                        .and_then(|value| value.as_bool())
                        == Some(true)
                })
                .count(),
            empty_extractions,
            anchor_only_files,
        },
        findings,
        build,
        strict_violations: finding_errors + build_losses,
    }
}

fn render_audit_report(report: &AuditReport) -> anyhow::Result<String> {
    use std::fmt::Write as _;

    let mut output = String::new();
    writeln!(output, "Graph audit: {}", report.root)?;
    writeln!(
        output,
        "Input: {} files, {} nodes, {} edges, {} hyperedges",
        report.input.extractions, report.input.nodes, report.input.edges, report.input.hyperedges
    )?;
    writeln!(
        output,
        "Output: {} nodes, {} edges, {} hyperedges",
        report.build.output_nodes, report.build.output_edges, report.build.output_hyperedges
    )?;
    writeln!(
        output,
        "Signals: {} unresolved calls, {} empty extractions, {} anchor-only files",
        report.input.unresolved_calls,
        report.input.empty_extractions,
        report.input.anchor_only_files.len()
    )?;
    if !report.build.node_drops.is_empty() {
        writeln!(
            output,
            "Node drops: {}",
            serde_json::to_string(&report.build.node_drops)?
        )?;
    }
    if !report.build.node_merges.is_empty() {
        writeln!(
            output,
            "Node merges: {}",
            serde_json::to_string(&report.build.node_merges)?
        )?;
    }
    if !report.build.edge_drops.is_empty() {
        writeln!(
            output,
            "Edge drops: {}",
            serde_json::to_string(&report.build.edge_drops)?
        )?;
    }
    if !report.build.edge_repairs.is_empty() {
        writeln!(
            output,
            "Edge repairs: {}",
            serde_json::to_string(&report.build.edge_repairs)?
        )?;
    }
    if !report.build.hyperedge_drops.is_empty() {
        writeln!(
            output,
            "Hyperedge drops: {}",
            serde_json::to_string(&report.build.hyperedge_drops)?
        )?;
    }
    if !report.build.hyperedge_repairs.is_empty() {
        writeln!(
            output,
            "Hyperedge repairs: {}",
            serde_json::to_string(&report.build.hyperedge_repairs)?
        )?;
    }
    writeln!(
        output,
        "Findings: {} (showing up to 20)",
        report.findings.len()
    )?;
    for finding in report.findings.iter().take(20) {
        writeln!(
            output,
            "- [{}] {} {}: {}",
            finding.severity, finding.code, finding.source_file, finding.subject
        )?;
    }
    if report.findings.len() > 20 {
        writeln!(output, "- … {} more", report.findings.len() - 20)?;
    }
    writeln!(
        output,
        "Strict conservation violations: {}",
        report.strict_violations
    )?;
    Ok(output.trim_end().to_owned())
}

fn watch(
    path: PathBuf,
    force: bool,
    no_cluster: bool,
    runtime_report: Option<&std::path::Path>,
    progress: ProgressModeArg,
    legacy_executor: bool,
    runtime: RuntimeOptions,
) -> anyhow::Result<()> {
    use notify::{RecursiveMode, Watcher};
    let progress_factory = BuildProgressFactory::new(progress.into())?;
    let runtime_config = runtime.resolve_for_executor(legacy_executor)?;
    let output_directory = managed_output_directory(&path, None);
    watch_service::validate_watch_output_directory(&path, &output_directory)?;
    let filter = watch_service::WatchEventFilter::with_output_directory(
        &path,
        watch_service::read_build_config(&output_directory).honor_gitignore,
        Some(&output_directory),
    );
    let (sender, receiver) = std::sync::mpsc::channel();
    let mut watcher = notify::recommended_watcher(sender)?;
    watcher.watch(&path, RecursiveMode::Recursive)?;
    write_output(&format!("Watching {}", path.display()))?;
    loop {
        let Ok(first) = receiver.recv()? else {
            continue;
        };
        let mut changed = first
            .paths
            .into_iter()
            .filter(|changed| filter.accepts(changed, changed.is_dir()))
            .collect::<Vec<_>>();
        if changed.is_empty() {
            continue;
        }

        // Coalesce a burst of editor writes, but never wait indefinitely for
        // total filesystem silence. Linux editors and language servers can
        // continuously emit events in an active workspace.
        let batch_started = std::time::Instant::now();
        let hard_deadline = batch_started + std::time::Duration::from_secs(3);
        let quiet_period = std::time::Duration::from_millis(750);
        let mut quiet_deadline = batch_started + quiet_period;
        loop {
            let deadline = quiet_deadline.min(hard_deadline);
            let timeout = deadline.saturating_duration_since(std::time::Instant::now());
            match receiver.recv_timeout(timeout) {
                Ok(Ok(event)) => {
                    let paths = event
                        .paths
                        .into_iter()
                        .filter(|changed| filter.accepts(changed, changed.is_dir()))
                        .collect::<Vec<_>>();
                    if !paths.is_empty() {
                        changed.extend(paths);
                        quiet_deadline =
                            (std::time::Instant::now() + quiet_period).min(hard_deadline);
                    }
                }
                Ok(Err(error)) => eprintln!("[graphoxide] watch error: {error}"),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    anyhow::bail!("filesystem watcher disconnected")
                }
            }
        }
        changed.sort();
        changed.dedup();
        let mut structural_changes = Vec::new();
        let mut has_notify_only_change = false;
        for changed in &changed {
            if watch_change_requires_structural_rebuild(changed) {
                structural_changes.push(changed.clone());
            } else {
                has_notify_only_change = true;
            }
        }
        if !structural_changes.is_empty() {
            let options = watch_service::RebuildOptions {
                changed_paths: Some(structural_changes),
                output_directory: Some(output_directory.clone()),
                force,
                no_cluster,
                acquire_lock: true,
                block_on_lock: false,
                ..Default::default()
            };
            let rebuild = rebuild_watch_project_with_progress_factory(
                &path,
                &options,
                runtime_config,
                runtime_report,
                &progress_factory,
            );
            match rebuild {
                Ok(result) => {
                    for warning in result.warnings {
                        eprintln!("[graphoxide watch] {warning}");
                    }
                    if result.status == watch_service::RebuildStatus::Queued {
                        eprintln!("[graphoxide watch] rebuild already in progress; changes queued");
                    }
                }
                Err(error) => eprintln!("[graphoxide] rebuild failed: {error}"),
            }
        }
        if has_notify_only_change {
            watch_service::notify_only_in(&output_directory)?;
        }
    }
}

fn watch_change_requires_structural_rebuild(path: &std::path::Path) -> bool {
    if path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("ts"))
    {
        return true;
    }
    graphoxide_extract::detect::classify_file(path)
        == Some(graphoxide_extract::detect::FileType::Code)
}

/// Execute one admitted watch rebuild. The filesystem-notification loop above
/// is intentionally thin; this boundary keeps the durable lock/journal state
/// in `watch` while making the default isolated executor directly testable.
fn rebuild_watch_project(
    path: &std::path::Path,
    options: &watch_service::RebuildOptions,
    runtime_config: Option<graphoxide_index_runtime::IndexRuntimeConfig>,
    runtime_report: Option<&std::path::Path>,
    progress: ProgressModeArg,
) -> anyhow::Result<watch_service::RebuildResult> {
    let progress_factory = BuildProgressFactory::new(progress.into())?;
    rebuild_watch_project_with_progress_factory(
        path,
        options,
        runtime_config,
        runtime_report,
        &progress_factory,
    )
}

fn rebuild_watch_project_with_progress_factory(
    path: &std::path::Path,
    options: &watch_service::RebuildOptions,
    runtime_config: Option<graphoxide_index_runtime::IndexRuntimeConfig>,
    runtime_report: Option<&std::path::Path>,
    progress_factory: &BuildProgressFactory,
) -> anyhow::Result<watch_service::RebuildResult> {
    if let Some(runtime_config) = runtime_config {
        let mut first_progress = Some(
            if options.changed_paths.is_none() && options.scope == watch_service::RebuildScope::Full
            {
                progress_factory.reporter(
                    graphoxide_cli::build_telemetry::BuildOperation::Update,
                    graphoxide_cli::build_telemetry::BuildMode::Full,
                )
            } else {
                progress_factory
                    .adaptive_reporter(graphoxide_cli::build_telemetry::BuildOperation::Update)
            },
        );
        first_progress
            .as_mut()
            .expect("first watch progress reporter exists")
            .start();
        let result = watch_service::rebuild_project_with_executor(path, options, |request| {
            let progress_reporter = if let Some(reporter) = first_progress.take() {
                reporter
            } else {
                let mut reporter = if request.scope == watch_service::RebuildScope::Full {
                    progress_factory.reporter(
                        graphoxide_cli::build_telemetry::BuildOperation::Update,
                        graphoxide_cli::build_telemetry::BuildMode::Full,
                    )
                } else {
                    progress_factory
                        .adaptive_reporter(graphoxide_cli::build_telemetry::BuildOperation::Update)
                };
                reporter.start();
                reporter
            };
            let mut outcome = rebuild_isolated_pass(IsolatedRebuildRequest {
                path: &request.watch_root,
                output_directory: &request.output_directory,
                marker_value: &request.marker_value,
                no_cluster: request.no_cluster,
                force: request.force,
                scope: request.scope,
                pass: request.pass,
                runtime_config,
                collect_runtime_telemetry: runtime_report.is_some(),
                progress_reporter,
            })?;
            if runtime_report.is_some() {
                hydrate_unchanged_graph_report(&mut outcome)?;
            }
            write_runtime_report_if_requested(
                &outcome.telemetry,
                runtime_report,
                Some(&outcome.runtime_telemetry),
                Some(outcome.runtime_io),
                Some(outcome.runtime_work),
                Some(outcome.runtime_cache),
            )?;
            outcome.complete_progress();
            Ok(outcome.result)
        })?;
        if let Some(mut reporter) = first_progress {
            let telemetry = legacy_rebuild_telemetry(&result);
            reporter.set_indexed_inputs(result.stats.detected_files);
            reporter.complete(&telemetry, None);
        }
        Ok(result)
    } else {
        let mut progress_reporter = if options.changed_paths.is_none()
            && options.scope == watch_service::RebuildScope::Full
        {
            progress_factory.reporter(
                graphoxide_cli::build_telemetry::BuildOperation::Update,
                graphoxide_cli::build_telemetry::BuildMode::Full,
            )
        } else {
            progress_factory
                .adaptive_reporter(graphoxide_cli::build_telemetry::BuildOperation::Update)
        };
        progress_reporter.start();
        let result =
            watch_service::rebuild_project_with_progress_observer(path, options, |event| {
                report_legacy_rebuild_progress(&mut progress_reporter, event)
            })?;
        let telemetry = legacy_rebuild_telemetry(&result);
        progress_reporter.set_indexed_inputs(result.stats.detected_files);
        write_runtime_report_if_requested(&telemetry, runtime_report, None, None, None, None)?;
        progress_reporter.complete(&telemetry, None);
        Ok(result)
    }
}

#[cfg(test)]
fn relevant_watch_paths(root: &std::path::Path, paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths
        .into_iter()
        .filter(|changed_path| {
            let relative = changed_path.strip_prefix(root).unwrap_or(changed_path);
            !relative.components().any(|component| {
                matches!(
                    component.as_os_str().to_str(),
                    Some("graphoxide-out" | "graphify-out" | ".git")
                )
            })
        })
        .collect()
}

fn check_update(path: &std::path::Path) -> anyhow::Result<()> {
    let output_directory = managed_output_directory(path, None);
    let notice = watch_service::check_update_in(path, &output_directory);
    if let Some(message) = notice.message {
        write_output(&format!("[graphoxide check-update] {message}"))?;
    }
    Ok(())
}

fn label_communities(
    path: &std::path::Path,
    backend: Option<&str>,
    model: Option<&str>,
    missing_only: bool,
    max_concurrency: usize,
    batch_size: usize,
    timeout_seconds: Option<f64>,
) -> anyhow::Result<()> {
    let known_managed_directory = if path.is_dir() {
        Some(managed_output_directory(path, None))
    } else {
        None
    };
    let graph_path = known_managed_directory
        .as_ref()
        .map_or_else(|| path.to_path_buf(), |output| output.join("graph.json"));
    let _managed_graph_lock =
        acquire_managed_graph_lock(&graph_path, known_managed_directory.as_deref())?;
    let mut graph = graphoxide_core::read_graph(&graph_path)?;
    let output = graph_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    let mut existing = load_community_labels(output);
    let mut communities = std::collections::BTreeMap::<i64, Vec<String>>::new();
    for node in &graph.nodes {
        if let Some(community) = node.community {
            communities
                .entry(community)
                .or_default()
                .push(node.id.clone());
            if let Some(label) = node
                .extra
                .get("community_name")
                .and_then(serde_json::Value::as_str)
                .filter(|label| !is_placeholder_community_label(community, label))
            {
                existing
                    .entry(community)
                    .or_insert_with(|| label.to_owned());
            }
        }
    }
    if missing_only {
        communities.retain(|community, _| {
            existing
                .get(community)
                .is_none_or(|label| is_placeholder_community_label(*community, label))
        });
    }
    if communities.is_empty() {
        return write_output("No communities need labels.");
    }
    let Some(backend) = resolve_label_backend(backend) else {
        return write_output(
            "No LLM backend configured; keeping current community labels. Pass --backend or set an API key.",
        );
    };
    let transport = LabelHttpTransport::new(&backend, model, timeout_seconds)?;
    if let Some(warning) = &transport.warning {
        eprintln!("[graphoxide] warning: {warning}");
    }
    let mut options = graphoxide_graph::LabelingOptions::new(&backend);
    options.model = model.map(str::to_owned);
    options.max_concurrency = max_concurrency;
    options.batch_size = batch_size;
    options.allow_ollama_parallel =
        std::env::var("GRAPHIFY_OLLAMA_PARALLEL").is_ok_and(|value| value.trim() == "1");
    options.allow_claude_cli_parallel =
        std::env::var("GRAPHIFY_CLAUDE_CLI_PARALLEL").is_ok_and(|value| value.trim() == "1");
    let gods = graphoxide_graph::god_nodes(&graph, 10);
    let (generated, usage) = graphoxide_graph::label_communities_with(
        &graph,
        &communities,
        &gods,
        &options,
        |request| transport.call(request),
    )?;
    for (community, label) in generated {
        existing.insert(community, label);
    }
    let mut updated = 0;
    for node in &mut graph.nodes {
        let Some(community) = node.community else {
            continue;
        };
        let Some(label) = existing.get(&community) else {
            continue;
        };
        let label = graphoxide_core::sanitize_label(label);
        if !label.is_empty() {
            node.extra.insert("community_name".into(), label.into());
            updated += 1;
        }
    }
    graphoxide_core::write_graph_atomic(&graph_path, &graph, true)?;
    write_community_label_sidecars(output, &existing)?;
    let analysis = graphoxide_graph::analyze(&graph)?;
    let report_path = output.join("GRAPH_REPORT.md");
    write_text(
        &report_path,
        &graphoxide_export::render_report(&graph, &analysis),
    )?;
    write_output(&format!(
        "Updated community labels on {updated} nodes and regenerated {} ({} input / {} output tokens).",
        report_path.display(), usage.input, usage.output
    ))
}

fn resolve_label_backend(explicit: Option<&str>) -> Option<String> {
    explicit
        .map(str::to_owned)
        .or_else(|| std::env::var("GRAPHOXIDE_LLM_PROVIDER").ok())
        .or_else(|| std::env::var("GRAPHIFY_LLM_PROVIDER").ok())
        .or_else(|| {
            [
                ("gemini", &["GEMINI_API_KEY", "GOOGLE_API_KEY"][..]),
                ("kimi", &["MOONSHOT_API_KEY"][..]),
                ("claude", &["ANTHROPIC_API_KEY"][..]),
                ("openai", &["OPENAI_API_KEY"][..]),
                ("deepseek", &["DEEPSEEK_API_KEY"][..]),
            ]
            .into_iter()
            .find_map(|(backend, keys)| {
                keys.iter()
                    .any(|key| std::env::var(key).is_ok_and(|value| !value.is_empty()))
                    .then(|| backend.to_owned())
            })
        })
        .or_else(|| {
            (std::env::var_os("OLLAMA_BASE_URL").is_some()
                || std::env::var_os("OLLAMA_HOST").is_some())
            .then(|| "ollama".to_owned())
        })
        .map(|backend| backend.to_ascii_lowercase())
}

#[derive(Debug)]
struct LabelHttpTransport {
    client: reqwest::blocking::Client,
    endpoint: String,
    model: String,
    key: Option<String>,
    anthropic: bool,
    disable_reasoning: bool,
    timeout: std::time::Duration,
    warning: Option<String>,
    require_success_status: bool,
}

#[derive(Debug, PartialEq, Eq)]
struct LabelTransportInputs {
    endpoint: String,
    model: String,
    key: Option<String>,
    anthropic: bool,
    disable_reasoning: bool,
    warning: Option<String>,
    ollama_dns_override: Option<OllamaDnsOverride>,
}

#[derive(Debug, PartialEq, Eq)]
struct OllamaDnsOverride {
    host: String,
    addresses: Vec<std::net::SocketAddr>,
}

fn resolve_label_transport_inputs<F>(
    backend: &str,
    requested_model: Option<&str>,
    mut environment: F,
) -> anyhow::Result<LabelTransportInputs>
where
    F: FnMut(&str) -> Option<String>,
{
    let (backend, disable_reasoning) = match backend {
        "anthropic" => ("claude", false),
        "lm-studio" | "lmstudio" => ("openai", true),
        backend => (backend, false),
    };
    let (base_key, default_base, key_names, model_key, default_model, anthropic) = match backend {
        "gemini" => (
            "GEMINI_BASE_URL",
            "https://generativelanguage.googleapis.com/v1beta/openai/",
            &["GEMINI_API_KEY", "GOOGLE_API_KEY"][..],
            "GRAPHIFY_GEMINI_MODEL",
            "gemini-3-flash-preview",
            false,
        ),
        "kimi" => (
            "KIMI_BASE_URL",
            "https://api.moonshot.ai/v1",
            &["MOONSHOT_API_KEY"][..],
            "GRAPHIFY_KIMI_MODEL",
            "kimi-k2.6",
            false,
        ),
        "claude" => (
            "ANTHROPIC_BASE_URL",
            "https://api.anthropic.com/v1",
            &["ANTHROPIC_API_KEY"][..],
            "ANTHROPIC_MODEL",
            "claude-sonnet-4-6",
            true,
        ),
        "openai" => (
            "OPENAI_BASE_URL",
            "https://api.openai.com/v1",
            &["OPENAI_API_KEY"][..],
            "GRAPHIFY_OPENAI_MODEL",
            "gpt-4.1-mini",
            false,
        ),
        "deepseek" => (
            "DEEPSEEK_BASE_URL",
            "https://api.deepseek.com",
            &["DEEPSEEK_API_KEY"][..],
            "GRAPHIFY_DEEPSEEK_MODEL",
            "deepseek-v4-flash",
            false,
        ),
        "ollama" => (
            "OLLAMA_BASE_URL",
            "http://localhost:11434/v1",
            &["OLLAMA_API_KEY"][..],
            "OLLAMA_MODEL",
            "qwen2.5-coder:7b",
            false,
        ),
        _ => anyhow::bail!("unsupported labeling backend {backend:?}"),
    };
    let base = environment("GRAPHOXIDE_LLM_BASE_URL")
        .or_else(|| environment(base_key))
        .unwrap_or_else(|| default_base.into());
    let mut parsed_base = reqwest::Url::parse(&base)?;
    let mut ollama_dns_override = None;
    let normalized_base = if backend == "ollama" {
        anyhow::ensure!(
            parsed_base.username().is_empty()
                && parsed_base.password().is_none()
                && parsed_base.query().is_none()
                && parsed_base.fragment().is_none(),
            "Ollama base URL may not contain credentials, a query string, or a fragment"
        );
        let validated = graphoxide_extract::llm::plan_ollama_connection(&base, false)?;
        if let Ok(address) = validated.canonical_host.parse::<std::net::IpAddr>() {
            parsed_base
                .set_ip_host(address)
                .map_err(|()| anyhow::anyhow!("Ollama base URL has an invalid IP host"))?;
        } else {
            parsed_base
                .set_host(Some(&validated.canonical_host))
                .map_err(|error| anyhow::anyhow!("Ollama base URL has an invalid host: {error}"))?;
        }
        ollama_dns_override = Some(OllamaDnsOverride {
            host: validated.canonical_host,
            addresses: validated
                .resolved_addresses
                .into_iter()
                .map(|address| std::net::SocketAddr::new(address, 0))
                .collect(),
        });
        parsed_base.to_string()
    } else {
        base
    };
    let suffix = if anthropic {
        "messages"
    } else {
        "chat/completions"
    };
    let endpoint = if normalized_base.trim_end_matches('/').ends_with(suffix) {
        normalized_base.trim_end_matches('/').to_owned()
    } else {
        format!("{}/{suffix}", normalized_base.trim_end_matches('/'))
    };
    let key = key_names
        .iter()
        .find_map(|name| environment(name).filter(|key| !key.is_empty()));
    let parsed = reqwest::Url::parse(&endpoint)?;
    let loopback = ollama_dns_override.as_ref().map_or_else(
        || {
            parsed.host_str().is_some_and(|host| {
                host.eq_ignore_ascii_case("localhost")
                    || host
                        .trim_matches(['[', ']'])
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback())
            })
        },
        |resolution| {
            resolution
                .addresses
                .iter()
                .all(|address| address.ip().is_loopback())
        },
    );
    if key.is_none() && !loopback && backend != "ollama" {
        anyhow::bail!(
            "none of {} is set for backend {backend:?}",
            key_names.join(", ")
        )
    }
    let warning = (backend == "ollama" && parsed_base.scheme() == "http" && !loopback).then(|| {
        let host = graphoxide_core::sanitize_label(parsed_base.host_str().unwrap_or_default());
        format!(
            "Ollama labeling sends graph-derived labels and any configured API key to {:?} over plaintext HTTP",
            host
        )
    });
    let model = requested_model
        .map(str::to_owned)
        .or_else(|| environment(model_key))
        .or_else(|| environment("GRAPHOXIDE_MODEL"))
        .unwrap_or_else(|| default_model.into());
    Ok(LabelTransportInputs {
        endpoint,
        model,
        key,
        anthropic,
        disable_reasoning,
        warning,
        ollama_dns_override,
    })
}

impl LabelHttpTransport {
    fn new(
        backend: &str,
        requested_model: Option<&str>,
        timeout_seconds: Option<f64>,
    ) -> anyhow::Result<Self> {
        let inputs = resolve_label_transport_inputs(backend, requested_model, |name| {
            std::env::var(name).ok()
        })?;
        let timeout = label_request_timeout(timeout_seconds)?;
        let mut client = reqwest::blocking::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(timeout);
        if let Some(resolution) = &inputs.ollama_dns_override {
            client = client
                .redirect(reqwest::redirect::Policy::none())
                .no_proxy()
                .resolve_to_addrs(&resolution.host, &resolution.addresses);
        }
        Ok(Self {
            client: client.build()?,
            endpoint: inputs.endpoint,
            model: inputs.model,
            key: inputs.key,
            anthropic: inputs.anthropic,
            disable_reasoning: inputs.disable_reasoning,
            timeout,
            warning: inputs.warning,
            require_success_status: inputs.ollama_dns_override.is_some(),
        })
    }

    fn call(
        &self,
        request: &graphoxide_graph::LabelRequest,
    ) -> anyhow::Result<graphoxide_graph::LabelResponse> {
        let model = request.model.as_deref().unwrap_or(&self.model);
        let mut body = serde_json::json!({
            "model": model,
            "max_tokens": request.max_tokens,
            "temperature": 0,
            "messages": [{"role": "user", "content": request.prompt}],
        });
        if self.disable_reasoning {
            body["reasoning_effort"] = "none".into();
        }
        let mut builder = self.client.post(&self.endpoint).json(&body);
        if self.anthropic {
            builder = builder.header("anthropic-version", "2023-06-01");
            if let Some(key) = &self.key {
                builder = builder.header("x-api-key", key);
            }
        } else if let Some(key) = &self.key {
            builder = builder.bearer_auth(key);
        }
        let response = builder
            .send()
            .map_err(|error| {
                if error.is_timeout() {
                    anyhow::anyhow!(
                        "label request to {} timed out after {}s; local models may need more time, so increase --timeout-seconds (or GRAPHOXIDE_LLM_TIMEOUT_SECONDS)",
                        self.endpoint,
                        self.timeout.as_secs_f64()
                    )
                } else {
                    error.into()
                }
            })?;
        if self.require_success_status {
            anyhow::ensure!(
                response.status().is_success(),
                "label endpoint returned HTTP {}",
                response.status()
            );
        }
        let response = response.error_for_status()?.json::<serde_json::Value>()?;
        let content = response
            .pointer("/choices/0/message/content")
            .or_else(|| response.pointer("/content/0/text"))
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("label endpoint returned no message content"))?;
        Ok(graphoxide_graph::LabelResponse {
            content: content.to_owned(),
            usage: graphoxide_graph::LabelUsage {
                input: response
                    .pointer("/usage/prompt_tokens")
                    .or_else(|| response.pointer("/usage/input_tokens"))
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0),
                output: response
                    .pointer("/usage/completion_tokens")
                    .or_else(|| response.pointer("/usage/output_tokens"))
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0),
            },
        })
    }
}

fn label_request_timeout(explicit: Option<f64>) -> anyhow::Result<std::time::Duration> {
    let (source, seconds) = if let Some(seconds) = explicit {
        ("--timeout-seconds", seconds)
    } else if let Ok(value) = std::env::var("GRAPHOXIDE_LLM_TIMEOUT_SECONDS") {
        (
            "GRAPHOXIDE_LLM_TIMEOUT_SECONDS",
            value.parse::<f64>().map_err(|error| {
                anyhow::anyhow!("GRAPHOXIDE_LLM_TIMEOUT_SECONDS must be a number: {error}")
            })?,
        )
    } else if let Ok(value) = std::env::var("GRAPHIFY_API_TIMEOUT") {
        (
            "GRAPHIFY_API_TIMEOUT",
            value.parse::<f64>().map_err(|error| {
                anyhow::anyhow!("GRAPHIFY_API_TIMEOUT must be a number: {error}")
            })?,
        )
    } else {
        ("default", 600.0)
    };
    anyhow::ensure!(
        seconds.is_finite() && seconds > 0.0,
        "{source} must be a finite number greater than zero"
    );
    std::time::Duration::try_from_secs_f64(seconds)
        .map_err(|error| anyhow::anyhow!("{source} is not a valid timeout: {error}"))
}

fn install_agent_platform(
    positional: Option<String>,
    flagged: Option<String>,
    project: bool,
    strict: bool,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        positional.is_none() || flagged.is_none(),
        "specify the platform either positionally or with --platform, not both"
    );
    let platform: graphoxide_cli::install::Platform = positional
        .or(flagged)
        .unwrap_or_else(|| "claude".to_owned())
        .parse()?;
    let cwd = std::env::current_dir()?;
    let context = graphoxide_cli::install::InstallContext::for_current_process(cwd, project)?;
    graphoxide_cli::install::install_with_strict(platform, &context, strict)?;
    write_output(&format!(
        "Graphoxide {platform} integration installed ({} scope).",
        if project { "project" } else { "user" }
    ))
}

fn uninstall_agent_platform(
    positional: Option<String>,
    flagged: Option<String>,
    project: bool,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        positional.is_none() || flagged.is_none(),
        "specify the platform either positionally or with --platform, not both"
    );
    let cwd = std::env::current_dir()?;
    let context = graphoxide_cli::install::InstallContext::for_current_process(cwd, project)?;
    if let Some(name) = positional.or(flagged) {
        let platform = name.parse()?;
        graphoxide_cli::install::uninstall(platform, &context)?;
    } else {
        graphoxide_cli::install::uninstall_all(&context)?;
    }
    write_output("Graphoxide integration removed.")
}

fn claude(command: ClaudeCommand) -> anyhow::Result<()> {
    match command {
        ClaudeCommand::Status { path } => {
            let markdown = [path.join("CLAUDE.md"), path.join(".claude/CLAUDE.md")]
                .iter()
                .any(|candidate| {
                    std::fs::read_to_string(candidate)
                        .ok()
                        .is_some_and(|text| text.lines().any(|line| line == "## graphoxide"))
                });
            let settings = std::fs::read_to_string(path.join(".claude/settings.json"))
                .ok()
                .is_some_and(|text| text.contains("graphoxide hook-guard"));
            write_output(if markdown && settings {
                "Claude Code graphoxide integration installed."
            } else {
                "Claude Code graphoxide integration not installed."
            })
        }
        ClaudeCommand::Install {
            path,
            project,
            strict,
        } => {
            let markdown = if project {
                path.join("CLAUDE.md")
            } else {
                path.join(".claude/CLAUDE.md")
            };
            let already_configured = has_graphoxide_section(&markdown);
            let context =
                graphoxide_cli::install::InstallContext::for_current_process(path, project)?;
            graphoxide_cli::install::install_with_strict(
                graphoxide_cli::install::Platform::Claude,
                &context,
                strict,
            )?;
            write_output(if already_configured {
                "Claude Code graphoxide integration already configured (no change)."
            } else {
                "Claude Code graphoxide integration installed."
            })
        }
        ClaudeCommand::Uninstall { path, project } => {
            let markdown_targets = [
                path.join("CLAUDE.md"),
                path.join("CLAUDE.local.md"),
                path.join(".claude/CLAUDE.md"),
                path.join(".claude/CLAUDE.local.md"),
            ];
            let had_markdown = markdown_targets.iter().any(|target| target.is_file());
            let had_section = markdown_targets
                .iter()
                .any(|target| has_graphoxide_section(target));
            let context =
                graphoxide_cli::install::InstallContext::for_current_process(path, project)?;
            graphoxide_cli::install::uninstall(
                graphoxide_cli::install::Platform::Claude,
                &context,
            )?;
            if had_section {
                write_output("Claude Code graphoxide integration removed.")
            } else if had_markdown {
                write_output("Graphoxide section not found in CLAUDE.md; nothing to do.")
            } else {
                write_output("No CLAUDE.md found; nothing to do.")
            }
        }
    }
}

fn has_graphoxide_section(path: &std::path::Path) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .is_some_and(|text| text.lines().any(|line| line == "## graphoxide"))
}

fn direct_agent_platform(
    platform: graphoxide_cli::install::Platform,
    command: PlatformInstallCommand,
) -> anyhow::Result<()> {
    let (installing, project) = match command {
        PlatformInstallCommand::Install { project } => (true, project),
        PlatformInstallCommand::Uninstall { project } => (false, project),
    };
    let context = graphoxide_cli::install::InstallContext::for_current_process(
        std::env::current_dir()?,
        project,
    )?;
    if installing {
        graphoxide_cli::install::install(platform, &context)?;
        write_output(&format!("Graphoxide {platform} integration installed."))
    } else {
        graphoxide_cli::install::uninstall(platform, &context)?;
        write_output(&format!("Graphoxide {platform} integration removed."))
    }
}

fn direct_agents_platform(command: PlatformInstallCommand) -> anyhow::Result<()> {
    let (installing, project) = match command {
        PlatformInstallCommand::Install { project } => (true, project),
        PlatformInstallCommand::Uninstall { project } => (false, project),
    };
    let context = graphoxide_cli::install::InstallContext::for_current_process(
        std::env::current_dir()?,
        project,
    )?;
    if installing {
        graphoxide_cli::install::agents_platform_install(&context)?;
        write_output("Graphoxide agents integration installed.")
    } else {
        graphoxide_cli::install::agents_platform_uninstall(&context)?;
        write_output("Graphoxide agents integration removed.")
    }
}

fn direct_devin_platform(command: PlatformInstallCommand) -> anyhow::Result<()> {
    let (installing, project) = match command {
        PlatformInstallCommand::Install { project } => (true, project),
        PlatformInstallCommand::Uninstall { project } => (false, project),
    };
    let context = graphoxide_cli::install::InstallContext::for_current_process(
        std::env::current_dir()?,
        project,
    )?;
    if installing {
        let changed = graphoxide_cli::install::devin_platform_install(&context)?;
        if !changed {
            return write_output("Graphoxide Devin integration already configured (no change).");
        }
        if project {
            write_output(
                "Graphoxide Devin integration installed. Commit it with: git add .devin .windsurf",
            )
        } else {
            write_output("Graphoxide Devin integration installed.")
        }
    } else if graphoxide_cli::install::devin_platform_uninstall(&context)? {
        write_output("Graphoxide Devin integration removed.")
    } else {
        write_output("Graphoxide Devin integration has nothing to remove.")
    }
}

fn direct_codebuddy_platform(command: PlatformInstallCommand) -> anyhow::Result<()> {
    let (installing, project) = match command {
        PlatformInstallCommand::Install { project } => (true, project),
        PlatformInstallCommand::Uninstall { project } => (false, project),
    };
    let context = graphoxide_cli::install::InstallContext::for_current_process(
        std::env::current_dir()?,
        project,
    )?;
    if installing {
        let changed = graphoxide_cli::install::codebuddy_platform_install(&context)?;
        write_output(if changed {
            "Graphoxide CodeBuddy integration installed."
        } else {
            "Graphoxide CodeBuddy integration already configured (no change)."
        })
    } else {
        graphoxide_cli::install::codebuddy_platform_uninstall(&context)?;
        write_output("Graphoxide CodeBuddy integration removed.")
    }
}

fn hook(command: HookCommand) -> anyhow::Result<()> {
    let message = match command {
        HookCommand::Install { path } => {
            let executable =
                std::env::current_exe().unwrap_or_else(|_| PathBuf::from("graphoxide"));
            graphoxide_cli::hooks::install(&path, &executable)?
        }
        HookCommand::Uninstall { path } => graphoxide_cli::hooks::uninstall(&path)?,
        HookCommand::Status { path } => graphoxide_cli::hooks::status(&path),
    };
    write_output(&message)
}

/// Execute the supervised hook rebuild through the same isolated watch
/// dispatcher used by the interactive watcher. The hidden command defaults to
/// isolation as well; the retired executor remains reachable only through its
/// explicit compatibility flag.
fn rebuild_hook(
    mode: graphoxide_cli::hooks::HookMode,
    root: &std::path::Path,
    legacy_executor: bool,
    runtime: RuntimeOptions,
) -> anyhow::Result<()> {
    let force = ["GRAPHOXIDE_FORCE", "GRAPHIFY_FORCE"]
        .into_iter()
        .any(|name| {
            std::env::var(name).is_ok_and(|value| {
                matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes")
            })
        });
    let changed_paths = match mode {
        graphoxide_cli::hooks::HookMode::PostCommit => Some(
            std::env::var("GRAPHOXIDE_CHANGED")
                .or_else(|_| std::env::var("GRAPHIFY_CHANGED"))
                .unwrap_or_default()
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(PathBuf::from)
                .collect::<Vec<_>>(),
        ),
        graphoxide_cli::hooks::HookMode::PostCheckout => None,
    };
    if changed_paths.as_ref().is_some_and(Vec::is_empty) {
        return Ok(());
    }
    let runtime_config = runtime.resolve_for_executor(legacy_executor)?;
    let result = rebuild_watch_project(
        root,
        &watch_service::RebuildOptions {
            changed_paths,
            output_directory: watch_service::output_directory_from_env(root),
            force,
            acquire_lock: true,
            block_on_lock: false,
            ..Default::default()
        },
        runtime_config,
        None,
        ProgressModeArg::Never,
    )?;
    for warning in result.warnings {
        eprintln!("[graphoxide hook] {warning}");
    }
    println!("[graphoxide hook] rebuild status: {:?}", result.status);
    Ok(())
}

fn hook_guard(mode: Option<&str>, strict: bool) -> anyhow::Result<()> {
    use std::io::Read;
    use std::io::Write as _;

    let mode = mode.unwrap_or_default();
    if !matches!(mode, "search" | "read" | "gemini") {
        return Ok(());
    }
    let mut input = Vec::new();
    if mode != "gemini" && std::io::stdin().read_to_end(&mut input).is_err() {
        return Ok(());
    }
    let output = graphoxide_cli::hook_guard::evaluate_strict(
        mode,
        &input,
        &graphoxide_cli::hook_guard::GuardContext::for_current_process(),
        strict,
    );
    if output.is_empty() {
        return Ok(());
    }
    match std::io::stdout().lock().write_all(output.as_bytes()) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[allow(clippy::too_many_arguments)]
fn rebuild(
    path: &std::path::Path,
    no_cluster: bool,
    force: bool,
    json: bool,
    progress: ProgressModeArg,
    runtime_report: Option<&std::path::Path>,
    legacy_executor: bool,
    runtime: RuntimeOptions,
) -> anyhow::Result<()> {
    rebuild_with_executor(
        path,
        no_cluster,
        force,
        json,
        progress,
        runtime_report,
        legacy_executor,
        runtime,
    )
}

#[allow(clippy::too_many_arguments)]
fn rebuild_with_executor(
    path: &std::path::Path,
    no_cluster: bool,
    force: bool,
    json: bool,
    progress: ProgressModeArg,
    runtime_report: Option<&std::path::Path>,
    legacy_executor: bool,
    runtime: RuntimeOptions,
) -> anyhow::Result<()> {
    if legacy_executor {
        runtime.resolve_for_executor(true)?;
        return rebuild_legacy(path, no_cluster, force, json, progress, runtime_report);
    }
    rebuild_isolated(
        path,
        no_cluster,
        force,
        json,
        progress,
        runtime_report,
        runtime.resolve()?,
    )
}

fn rebuild_legacy(
    path: &std::path::Path,
    no_cluster: bool,
    force: bool,
    json: bool,
    progress: ProgressModeArg,
    runtime_report: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    let mut progress_reporter = BuildProgressReporter::new_adaptive(
        graphoxide_cli::build_telemetry::BuildOperation::Update,
        progress.into(),
    )?;
    progress_reporter.start();
    let result = watch_service::rebuild_project_with_progress_observer(
        path,
        &watch_service::RebuildOptions {
            scope: watch_service::RebuildScope::Incremental,
            output_directory: Some(managed_output_directory(path, None)),
            force,
            no_cluster,
            acquire_lock: true,
            block_on_lock: true,
            ..Default::default()
        },
        |event| report_legacy_rebuild_progress(&mut progress_reporter, event),
    )?;
    for warning in &result.warnings {
        eprintln!("[graphoxide update] {warning}");
    }
    let telemetry = legacy_rebuild_telemetry(&result);
    progress_reporter.set_indexed_inputs(result.stats.detected_files);
    let elapsed = graphoxide_cli::build_telemetry::format_elapsed(telemetry.elapsed_ms);
    let human = match result.status {
        watch_service::RebuildStatus::Rebuilt => {
            format!(
                "Wrote {} nodes and {} edges to {} in {elapsed}",
                telemetry.graph.nodes,
                telemetry.graph.edges,
                result.graph_path.display()
            )
        }
        watch_service::RebuildStatus::Unchanged => {
            format!("No code-graph topology changes detected in {elapsed}; outputs left untouched.")
        }
        watch_service::RebuildStatus::NoTrackedChanges => {
            format!("No tracked code files in change set; nothing to rebuild ({elapsed}).")
        }
        watch_service::RebuildStatus::Queued => {
            format!("A rebuild is already running; changes were queued in {elapsed}.")
        }
        watch_service::RebuildStatus::RefusedShrink => String::new(),
    };
    write_runtime_report_if_requested(&telemetry, runtime_report, None, None, None, None)?;
    if result.status == watch_service::RebuildStatus::RefusedShrink {
        if json {
            emit_build_report(&telemetry, true, false, "")?;
        }
        progress_reporter.complete(&telemetry, None);
        anyhow::bail!(
            "refusing to overwrite a smaller graph because the loss is not explained by rebuilt or deleted sources; pass --force after verifying the reduction"
        )
    } else {
        emit_build_report(&telemetry, json, false, &human)?;
        progress_reporter.complete(&telemetry, None);
        Ok(())
    }
}

fn report_legacy_rebuild_progress(
    reporter: &mut BuildProgressReporter,
    event: watch_service::RebuildProgress,
) {
    // One CLI invocation has one terminal envelope. Pending-journal drain
    // passes remain part of that aggregate lifecycle, so later pass phases
    // must not regress the first pass's monotonic wire sequence.
    if event.pass != 1 {
        return;
    }
    let phase = match event.phase {
        watch_service::RebuildProgressPhase::Waiting => BuildProgressPhase::Waiting,
        watch_service::RebuildProgressPhase::Scanning => BuildProgressPhase::Scanning,
        watch_service::RebuildProgressPhase::Extracting => BuildProgressPhase::Extracting,
        watch_service::RebuildProgressPhase::Building => BuildProgressPhase::Building,
        watch_service::RebuildProgressPhase::Clustering => BuildProgressPhase::Clustering,
        watch_service::RebuildProgressPhase::Publishing => BuildProgressPhase::Publishing,
    };
    match (event.processed, event.total) {
        (Some(processed), Some(total)) => reporter.phase_progress(phase, processed, total),
        (None, None) => reporter.phase(phase),
        _ => debug_assert!(false, "legacy progress counters must be paired"),
    }
}

fn legacy_rebuild_telemetry(
    result: &watch_service::RebuildResult,
) -> graphoxide_cli::build_telemetry::BuildTelemetry {
    let mode = match result.scope {
        watch_service::RebuildScope::Full => graphoxide_cli::build_telemetry::BuildMode::Full,
        watch_service::RebuildScope::Incremental => {
            graphoxide_cli::build_telemetry::BuildMode::Incremental
        }
    };
    let status = match result.status {
        watch_service::RebuildStatus::Rebuilt => {
            graphoxide_cli::build_telemetry::BuildStatus::Rebuilt
        }
        watch_service::RebuildStatus::Unchanged => {
            graphoxide_cli::build_telemetry::BuildStatus::Unchanged
        }
        watch_service::RebuildStatus::NoTrackedChanges => {
            graphoxide_cli::build_telemetry::BuildStatus::NoTrackedChanges
        }
        watch_service::RebuildStatus::Queued => {
            graphoxide_cli::build_telemetry::BuildStatus::Queued
        }
        watch_service::RebuildStatus::RefusedShrink => {
            graphoxide_cli::build_telemetry::BuildStatus::RefusedShrink
        }
    };
    let mut telemetry = graphoxide_cli::build_telemetry::BuildTelemetry::new(
        graphoxide_cli::build_telemetry::BuildOperation::Update,
        mode,
        status,
        result.graph_path.clone(),
    );
    telemetry.elapsed_ms = result.timings.total_ms;
    telemetry.stages_ms.detect = result.timings.detect_ms;
    telemetry.stages_ms.extract = result.timings.extract_ms;
    telemetry.stages_ms.build = result.timings.build_ms;
    telemetry.stages_ms.cluster = result.timings.cluster_ms;
    telemetry.stages_ms.write = result.timings.write_ms;
    telemetry.files.detected = result.stats.detected_files;
    telemetry.files.processed = result.stats.processed_files;
    telemetry.files.changed = result.stats.changed_files;
    telemetry.files.unchanged = result.stats.unchanged_files;
    telemetry.files.deleted = result.stats.deleted_files;
    telemetry.graph.nodes = result.stats.nodes;
    telemetry.graph.edges = result.stats.edges;
    telemetry.graph.clustered = result.clustered;
    telemetry.passes = result.passes;
    telemetry.warnings = result.warnings.clone();
    if result.status != watch_service::RebuildStatus::RefusedShrink
        && let Ok(graph) = graphoxide_core::read_graph(&result.graph_path)
    {
        telemetry.graph.nodes = graph.nodes.len();
        telemetry.graph.edges = graph.links.len();
        telemetry.graph.clustered = graph.nodes.iter().any(|node| node.community.is_some());
    }
    telemetry
}

fn rebuild_isolated(
    path: &std::path::Path,
    no_cluster: bool,
    force: bool,
    json: bool,
    progress: ProgressModeArg,
    runtime_report: Option<&std::path::Path>,
    runtime_config: graphoxide_index_runtime::IndexRuntimeConfig,
) -> anyhow::Result<()> {
    let mut progress_reporter = BuildProgressReporter::new_adaptive(
        graphoxide_cli::build_telemetry::BuildOperation::Update,
        progress.into(),
    )?;
    progress_reporter.start();
    let output_directory = managed_output_directory(path, None);
    let _build_lock = acquire_project_build_lock(
        &output_directory,
        &graphoxide_index_runtime::RuntimeCancellation::new(),
        &mut progress_reporter,
    )?;
    graphoxide_extract::cache::prepare_structured_redaction_cache_schema(&output_directory)
        .with_context(|| {
            format!(
                "prepare the managed cache schema in {}",
                output_directory.display()
            )
        })?;
    let mut outcome = rebuild_isolated_pass(IsolatedRebuildRequest {
        path,
        output_directory: &output_directory,
        marker_value: &path.to_string_lossy(),
        no_cluster,
        force,
        scope: watch_service::RebuildScope::Incremental,
        pass: 1,
        runtime_config,
        collect_runtime_telemetry: runtime_report.is_some(),
        progress_reporter,
    })?;
    if json || runtime_report.is_some() {
        hydrate_unchanged_graph_report(&mut outcome)?;
    }
    write_runtime_report_if_requested(
        &outcome.telemetry,
        runtime_report,
        Some(&outcome.runtime_telemetry),
        Some(outcome.runtime_io),
        Some(outcome.runtime_work),
        Some(outcome.runtime_cache),
    )?;
    if outcome.result.status == watch_service::RebuildStatus::RefusedShrink {
        if json {
            emit_build_report(&outcome.telemetry, true, false, "")?;
        }
        outcome.complete_progress();
        anyhow::bail!(
            "refusing to overwrite a smaller graph because the loss is not explained by rebuilt or deleted sources; pass --force after verifying the reduction"
        );
    }
    let human = match outcome.result.status {
        watch_service::RebuildStatus::Rebuilt => format!(
            "Wrote {} nodes and {} edges to {} in {}",
            outcome.result.stats.nodes,
            outcome.result.stats.edges,
            outcome.result.graph_path.display(),
            graphoxide_cli::build_telemetry::format_elapsed(outcome.telemetry.elapsed_ms)
        ),
        watch_service::RebuildStatus::Unchanged => format!(
            "No code-graph topology changes detected in {}; outputs left untouched.",
            graphoxide_cli::build_telemetry::format_elapsed(outcome.telemetry.elapsed_ms)
        ),
        watch_service::RebuildStatus::NoTrackedChanges => format!(
            "No tracked code files in change set; nothing to rebuild ({}).",
            graphoxide_cli::build_telemetry::format_elapsed(outcome.telemetry.elapsed_ms)
        ),
        watch_service::RebuildStatus::Queued => format!(
            "A rebuild is already running; changes were queued in {}.",
            graphoxide_cli::build_telemetry::format_elapsed(outcome.telemetry.elapsed_ms)
        ),
        watch_service::RebuildStatus::RefusedShrink => unreachable!("handled above"),
    };
    emit_build_report(&outcome.telemetry, json, false, &human)?;
    outcome.complete_progress();
    Ok(())
}

struct IsolatedRebuildOutcome {
    result: watch_service::RebuildResult,
    telemetry: graphoxide_cli::build_telemetry::BuildTelemetry,
    runtime_telemetry: graphoxide_cli::build_telemetry::IndexRuntimeConfiguration,
    runtime_cache: graphoxide_cli::build_telemetry::RuntimeCacheTelemetryV2,
    runtime_io: graphoxide_index_runtime::RuntimeIoTelemetry,
    runtime_work: graphoxide_extract::RuntimeWorkTelemetry,
    progress_reporter: BuildProgressReporter,
    indexed_source_bytes: Option<u64>,
}

impl IsolatedRebuildOutcome {
    fn complete_progress(&mut self) {
        self.progress_reporter
            .complete(&self.telemetry, self.indexed_source_bytes);
    }
}

#[derive(Debug, Default)]
struct AcceptedGraphNodeStats {
    count: usize,
    clustered: bool,
}

fn deserialize_accepted_graph_nodes<'de, D>(
    deserializer: D,
) -> Result<AcceptedGraphNodeStats, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct NodeStatsVisitor;

    impl<'de> serde::de::Visitor<'de> for NodeStatsVisitor {
        type Value = AcceptedGraphNodeStats;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a graph node array")
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            let mut stats = AcceptedGraphNodeStats::default();
            while let Some(node) = sequence.next_element::<graphoxide_core::Node>()? {
                stats.count = stats.count.saturating_add(1);
                stats.clustered |= node.community.is_some();
            }
            Ok(stats)
        }
    }

    deserializer.deserialize_seq(NodeStatsVisitor)
}

fn deserialize_accepted_graph_edges<'de, D>(deserializer: D) -> Result<usize, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct EdgeCountVisitor;

    impl<'de> serde::de::Visitor<'de> for EdgeCountVisitor {
        type Value = usize;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a graph edge array")
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            let mut count = 0usize;
            while sequence.next_element::<graphoxide_core::Edge>()?.is_some() {
                count = count.saturating_add(1);
            }
            Ok(count)
        }
    }

    deserializer.deserialize_seq(EdgeCountVisitor)
}

#[derive(serde::Deserialize)]
struct AcceptedGraphStats {
    #[serde(deserialize_with = "deserialize_accepted_graph_nodes")]
    nodes: AcceptedGraphNodeStats,
    #[serde(
        default,
        alias = "edges",
        deserialize_with = "deserialize_accepted_graph_edges"
    )]
    links: usize,
}

fn read_accepted_graph_stats(path: &std::path::Path) -> anyhow::Result<AcceptedGraphStats> {
    use std::io::Read as _;

    let file = std::fs::File::open(path)
        .with_context(|| format!("open accepted graph {} for telemetry", path.display()))?;
    let cap = graphoxide_core::max_graph_bytes();
    let size = file
        .metadata()
        .with_context(|| format!("inspect accepted graph {} for telemetry", path.display()))?
        .len();
    anyhow::ensure!(
        size <= cap,
        "accepted graph {} is {size} bytes, exceeding the {cap}-byte telemetry limit",
        path.display()
    );
    let mut reader = std::io::BufReader::new(file).take(cap.saturating_add(1));
    let stats = serde_json::from_reader(&mut reader)
        .with_context(|| format!("read accepted graph {} for telemetry", path.display()))?;
    anyhow::ensure!(
        reader.limit() > 0,
        "accepted graph {} grew beyond the {cap}-byte telemetry limit",
        path.display()
    );
    Ok(stats)
}

fn hydrate_unchanged_graph_report(outcome: &mut IsolatedRebuildOutcome) -> anyhow::Result<()> {
    if outcome.result.status != watch_service::RebuildStatus::Unchanged {
        return Ok(());
    }
    let stats = read_accepted_graph_stats(&outcome.result.graph_path)?;
    outcome.result.stats.nodes = stats.nodes.count;
    outcome.result.stats.edges = stats.links;
    outcome.result.clustered = stats.nodes.clustered;
    outcome.telemetry.graph.nodes = stats.nodes.count;
    outcome.telemetry.graph.edges = stats.links;
    outcome.telemetry.graph.clustered = stats.nodes.clustered;
    Ok(())
}

struct IsolatedRebuildRequest<'a> {
    path: &'a std::path::Path,
    output_directory: &'a std::path::Path,
    marker_value: &'a str,
    no_cluster: bool,
    force: bool,
    scope: watch_service::RebuildScope,
    pass: usize,
    runtime_config: graphoxide_index_runtime::IndexRuntimeConfig,
    collect_runtime_telemetry: bool,
    progress_reporter: BuildProgressReporter,
}

fn telemetry_status(
    status: watch_service::RebuildStatus,
) -> graphoxide_cli::build_telemetry::BuildStatus {
    match status {
        watch_service::RebuildStatus::Rebuilt => {
            graphoxide_cli::build_telemetry::BuildStatus::Rebuilt
        }
        watch_service::RebuildStatus::Unchanged => {
            graphoxide_cli::build_telemetry::BuildStatus::Unchanged
        }
        watch_service::RebuildStatus::NoTrackedChanges => {
            graphoxide_cli::build_telemetry::BuildStatus::NoTrackedChanges
        }
        watch_service::RebuildStatus::Queued => {
            graphoxide_cli::build_telemetry::BuildStatus::Queued
        }
        watch_service::RebuildStatus::RefusedShrink => {
            graphoxide_cli::build_telemetry::BuildStatus::RefusedShrink
        }
    }
}

fn write_watch_markers(
    output_directory: &std::path::Path,
    marker_value: &str,
) -> anyhow::Result<()> {
    for marker in [
        watch_service::ROOT_MARKER,
        watch_service::COMPAT_ROOT_MARKER,
    ] {
        graphoxide_core::write_text_atomic(output_directory.join(marker), marker_value)?;
    }
    Ok(())
}

fn rebuild_isolated_pass(
    request: IsolatedRebuildRequest<'_>,
) -> anyhow::Result<IsolatedRebuildOutcome> {
    let IsolatedRebuildRequest {
        path,
        output_directory,
        marker_value,
        no_cluster,
        force,
        scope,
        pass,
        runtime_config,
        collect_runtime_telemetry,
        mut progress_reporter,
    } = request;
    let started = std::time::Instant::now();
    let output = output_directory.join("graph.json");
    let manifest_path = output_directory.join("manifest.json");
    let persisted = watch_service::read_build_config(output_directory);
    let scan_started = std::time::Instant::now();
    let detect_options = graphoxide_extract::detect::DetectOptions {
        output_dir: Some(output_directory.to_path_buf()),
        extra_excludes: persisted.excludes,
        honor_gitignore: persisted.honor_gitignore,
        ..Default::default()
    };
    progress_reporter.phase(BuildProgressPhase::Scanning);
    let extraction_progress = progress_reporter.counter_emitter(BuildProgressPhase::Extracting);
    let (scan, runtime_extraction_telemetry, indexed_source_bytes, baseline_eligible) = match (
        collect_runtime_telemetry,
        extraction_progress,
    ) {
        (true, Some(progress)) => {
            let scan = graphoxide_extract::extract_project_with_runtime_scan_options_deferred_manifest_with_cancellation_and_telemetry_and_progress(
                    path,
                    force,
                    output_directory,
                    false,
                    &detect_options,
                    runtime_config,
                    graphoxide_index_runtime::RuntimeCancellation::new(),
                    progress,
                )?;
            (
                scan.result,
                scan.telemetry,
                Some(scan.indexed_source_bytes),
                scan.incremental_baseline_eligible,
            )
        }
        (true, None) => {
            let scan = graphoxide_extract::extract_project_with_runtime_scan_options_deferred_manifest_with_cancellation_and_telemetry_and_build_evidence(
                    path,
                    force,
                    output_directory,
                    false,
                    &detect_options,
                    runtime_config,
                    graphoxide_index_runtime::RuntimeCancellation::new(),
                )?;
            (
                scan.result,
                scan.telemetry,
                Some(scan.indexed_source_bytes),
                scan.incremental_baseline_eligible,
            )
        }
        (false, Some(progress)) => {
            let scan = graphoxide_extract::extract_project_with_runtime_scan_options_deferred_manifest_with_cancellation_and_progress(
                    path,
                    force,
                    output_directory,
                    false,
                    &detect_options,
                    runtime_config,
                    graphoxide_index_runtime::RuntimeCancellation::new(),
                    progress,
                )?;
            (
                scan.result,
                scan.telemetry,
                Some(scan.indexed_source_bytes),
                scan.incremental_baseline_eligible,
            )
        }
        (false, None) => {
            let scan = graphoxide_extract::extract_project_with_runtime_scan_options_deferred_manifest_with_cancellation_and_build_evidence(
                path,
                force,
                output_directory,
                false,
                &detect_options,
                runtime_config,
                graphoxide_index_runtime::RuntimeCancellation::new(),
            )?;
            (
                scan.result,
                scan.telemetry,
                Some(scan.indexed_source_bytes),
                scan.incremental_baseline_eligible,
            )
        }
    };
    let scope = if scope == watch_service::RebuildScope::Full || !baseline_eligible {
        watch_service::RebuildScope::Full
    } else {
        watch_service::RebuildScope::Incremental
    };
    let mode = match scope {
        watch_service::RebuildScope::Full => graphoxide_cli::build_telemetry::BuildMode::Full,
        watch_service::RebuildScope::Incremental => {
            graphoxide_cli::build_telemetry::BuildMode::Incremental
        }
    };
    progress_reporter.set_indexed_inputs(scan.progress.succeeded);
    if !scan.detection.walk_errors.is_empty() {
        let preview = scan
            .detection
            .walk_errors
            .iter()
            .take(5)
            .cloned()
            .collect::<Vec<_>>()
            .join("; ");
        let remaining = scan.detection.walk_errors.len().saturating_sub(5);
        let suffix = (remaining > 0).then(|| format!("; and {remaining} more"));
        anyhow::bail!(
            "refusing to rebuild from an incomplete filesystem scan ({} walk error(s)): {preview}{}",
            scan.detection.walk_errors.len(),
            suffix.unwrap_or_default()
        );
    }
    let runtime_telemetry =
        isolated_runtime_configuration(runtime_config, scan.detection.total_files);
    let runtime_cache = graphoxide_cli::build_telemetry::RuntimeCacheTelemetryV2::from_runtime(
        scan.runtime_cache,
        runtime_extraction_telemetry.cache_io,
    );
    let runtime_io = runtime_extraction_telemetry.io;
    let runtime_work = runtime_extraction_telemetry.work;
    let mut telemetry = graphoxide_cli::build_telemetry::BuildTelemetry::new(
        graphoxide_cli::build_telemetry::BuildOperation::Update,
        mode,
        graphoxide_cli::build_telemetry::BuildStatus::Rebuilt,
        output.clone(),
    );
    telemetry.stages_ms.extract = graphoxide_cli::build_telemetry::elapsed_millis(scan_started);
    telemetry.files.detected = scan.detection.total_files;
    telemetry.files.processed = scan.changed_sources;
    telemetry.files.changed = scan.changed_sources;
    telemetry.files.unchanged = scan.unchanged_sources;
    telemetry.files.deleted = scan.deleted_sources;
    telemetry.files.unclassified = scan.detection.unclassified.len();
    telemetry.files.sensitive = scan.detection.skipped_sensitive.len();
    telemetry.warnings.extend(scan.detection.warning.clone());
    telemetry
        .warnings
        .extend(scan.detection.walk_errors.iter().cloned());
    telemetry
        .warnings
        .extend(scan.runtime_cache_diagnostics.iter().cloned());
    telemetry.warnings.extend(scan.warnings.iter().cloned());
    let scan_detection = scan.detection;
    let live_sources = scan_detection
        .files
        .values()
        .flatten()
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    let scan_retained_output_bytes = scan.retained_output_bytes;
    let pending_manifest_retained_bytes = scan.pending_manifest_retained_bytes;
    let extractions = scan.extractions;
    let rebuilt_sources = scan.rebuilt_sources;
    let verified_representation_sources = scan.verified_representation_sources;
    let mut ownership_prune_sources = scan.ownership_prune_sources;
    let pending_manifest = scan.pending_manifest;
    let mut result = watch_service::RebuildResult {
        status: watch_service::RebuildStatus::Rebuilt,
        scope,
        graph_path: output.clone(),
        manifest_path,
        passes: pass,
        clustered: !no_cluster,
        warnings: telemetry.warnings.clone(),
        stats: watch_service::RebuildStats {
            detected_files: telemetry.files.detected,
            processed_files: telemetry.files.processed,
            changed_files: telemetry.files.changed,
            unchanged_files: telemetry.files.unchanged,
            deleted_files: telemetry.files.deleted,
            ..Default::default()
        },
        timings: watch_service::RebuildTimings::default(),
    };
    let finish = |mut result: watch_service::RebuildResult,
                  mut telemetry: graphoxide_cli::build_telemetry::BuildTelemetry,
                  runtime_telemetry: graphoxide_cli::build_telemetry::IndexRuntimeConfiguration,
                  progress_reporter: BuildProgressReporter|
     -> IsolatedRebuildOutcome {
        telemetry.status = telemetry_status(result.status);
        telemetry.elapsed_ms = graphoxide_cli::build_telemetry::elapsed_millis(started);
        result.timings.extract_ms = telemetry.stages_ms.extract;
        result.timings.build_ms = telemetry.stages_ms.build;
        result.timings.cluster_ms = telemetry.stages_ms.cluster;
        result.timings.write_ms = telemetry.stages_ms.write;
        result.timings.total_ms = telemetry.elapsed_ms;
        IsolatedRebuildOutcome {
            result,
            telemetry,
            runtime_telemetry,
            runtime_cache,
            runtime_io,
            runtime_work,
            progress_reporter,
            indexed_source_bytes,
        }
    };
    let mut unchanged_candidate = telemetry.files.changed == 0
        && telemetry.files.deleted == 0
        && ownership_prune_sources.is_empty()
        && output.is_file();
    let needs_ambiguous_baseline_audit = scan_detection
        .files
        .iter()
        .filter(|(kind, _)| matches!(kind.as_str(), "code" | "video"))
        .flat_map(|(_, paths)| paths)
        .map(Path::new)
        .any(|path| {
            path.extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("ts"))
        });
    if unchanged_candidate && !needs_ambiguous_baseline_audit {
        progress_reporter.phase(BuildProgressPhase::Publishing);
        pending_manifest.commit()?;
        watch_service::clear_needs_update(output_directory)?;
        result.status = watch_service::RebuildStatus::Unchanged;
        return Ok(finish(
            result,
            telemetry,
            runtime_telemetry,
            progress_reporter,
        ));
    }
    if scan.progress.total == 0 && !output.is_file() {
        result.status = watch_service::RebuildStatus::NoTrackedChanges;
        result.clustered = false;
        return Ok(finish(
            result,
            telemetry,
            runtime_telemetry,
            progress_reporter,
        ));
    }
    let build_progress = graphoxide_cli::build_guard::BuildProgress::new(
        scan.progress.total,
        scan.progress.succeeded,
    )?
    .ensure_any_success("isolated local extraction")?;
    let graph_memory_budget = runtime_config.memory_budget().cache_and_runs_bytes;
    let retained_output_bytes = graphoxide_extract::extractions_retained_bytes(&extractions)?;
    debug_assert_eq!(retained_output_bytes, scan_retained_output_bytes);
    let graph_budget_without_baseline =
        graph_budget_after_pending_manifest(graph_memory_budget, pending_manifest_retained_bytes)?;
    let (previous, graph_materialization_budget) = if output.is_file() {
        let budget = incremental_graph_budget_after_retained_scan(
            graph_memory_budget,
            retained_output_bytes,
            pending_manifest_retained_bytes,
        )?;
        let (previous, materialization_budget) =
            read_incremental_baseline(&output, graph_memory_budget, budget)?;
        (Some(previous), materialization_budget)
    } else {
        (None, graph_budget_without_baseline)
    };
    if let Some(baseline) = previous.as_ref() {
        ownership_prune_sources = gate_baseline_representation_resets(
            baseline,
            &scan_detection,
            &ownership_prune_sources,
            &rebuilt_sources,
            &verified_representation_sources,
            path,
        )?;
        ensure_incremental_baseline_representation_is_verified(
            baseline,
            &scan_detection,
            &ownership_prune_sources,
            &rebuilt_sources,
            &verified_representation_sources,
            path,
        )?;
        unchanged_candidate = telemetry.files.changed == 0
            && telemetry.files.deleted == 0
            && ownership_prune_sources.is_empty()
            && output.is_file();
    }
    if unchanged_candidate {
        progress_reporter.phase(BuildProgressPhase::Publishing);
        pending_manifest.commit()?;
        watch_service::clear_needs_update(output_directory)?;
        result.status = watch_service::RebuildStatus::Unchanged;
        return Ok(finish(
            result,
            telemetry,
            runtime_telemetry,
            progress_reporter,
        ));
    }
    let mut prune_sources = previous
        .as_ref()
        .map(|graph| stale_local_sources(graph, path, &live_sources))
        .unwrap_or_default();
    prune_sources.sort();
    prune_sources.dedup();
    progress_reporter.phase(BuildProgressPhase::Building);
    let build_started = std::time::Instant::now();
    let (staged_extractions, build_options, normalization_root) = if let Some(baseline) = &previous
    {
        let fresh = flatten_extractions(extractions);
        let merged = graphoxide_graph::incremental::merge_raw_extraction_from_graph_with_rebuilt_sources_and_ownership_resets_and_materialization_limit(
            fresh,
            baseline,
            &rebuilt_sources,
            &[],
            graphoxide_graph::incremental::IncrementalBaselinePrunes {
                deletion_sources: &prune_sources,
                ownership_reset_sources: &ownership_prune_sources,
            },
            Some(path),
            graph_materialization_budget,
        )?;
        (
            vec![merged],
            graphoxide_graph::BuildOptions {
                directed: previous.as_ref().is_some_and(|graph| graph.directed),
                ..graphoxide_graph::BuildOptions::default()
            },
            Some(path),
        )
    } else {
        (extractions, graphoxide_graph::BuildOptions::default(), None)
    };
    let build_emitter = progress_reporter.counter_emitter(BuildProgressPhase::Building);
    let sub_stage_emitter: Option<
        std::sync::Arc<dyn Fn(graphoxide_graph::BuildSubStage) + Send + Sync>,
    > = progress_reporter.phase_emitter().map(
        |emit: std::sync::Arc<dyn Fn(BuildProgressPhase) + Send + Sync>| {
            let adapter: std::sync::Arc<dyn Fn(graphoxide_graph::BuildSubStage) + Send + Sync> =
                std::sync::Arc::new(move |stage: graphoxide_graph::BuildSubStage| {
                    let phase = match stage {
                        graphoxide_graph::BuildSubStage::Normalizing => {
                            BuildProgressPhase::Building
                        }
                        graphoxide_graph::BuildSubStage::ResolvingSemanticIds => {
                            BuildProgressPhase::Building
                        }
                        graphoxide_graph::BuildSubStage::MergingNodes => {
                            BuildProgressPhase::MergingNodes
                        }
                        graphoxide_graph::BuildSubStage::ResolvingTwins => {
                            BuildProgressPhase::Building
                        }
                        graphoxide_graph::BuildSubStage::IndexingAliases => {
                            BuildProgressPhase::Building
                        }
                        graphoxide_graph::BuildSubStage::ResolvingEdges => {
                            BuildProgressPhase::ResolvingEdges
                        }
                        graphoxide_graph::BuildSubStage::ResolvingHyperedges => {
                            BuildProgressPhase::ResolvingEdges
                        }
                        graphoxide_graph::BuildSubStage::Deduplicating => {
                            BuildProgressPhase::Deduplicating
                        }
                        graphoxide_graph::BuildSubStage::DisambiguatingLabels => {
                            BuildProgressPhase::Building
                        }
                    };
                    (emit)(phase);
                });
            adapter
        },
    );
    let sub_stage_ref = sub_stage_emitter.as_deref();
    let mut graph = graphoxide_cli::build_guard::stage_graph_from_extractions_with_materialization_limit_and_root_and_substage(
        staged_extractions,
        output_directory,
        build_options,
        graph_materialization_budget,
        normalization_root,
        build_emitter.as_ref().map(|e| e.as_ref()),
        sub_stage_ref,
    )?
    .into_parts()
    .0;
    if no_cluster {
        for node in &mut graph.nodes {
            node.community = None;
        }
        telemetry.stages_ms.build = graphoxide_cli::build_telemetry::elapsed_millis(build_started);
        result.stats.nodes = graph.nodes.len();
        result.stats.edges = graph.links.len();
        progress_reporter.phase(BuildProgressPhase::Publishing);
        if previous
            .as_ref()
            .is_some_and(|existing| watch_service::same_topology(existing, &graph))
        {
            let write_started = std::time::Instant::now();
            pending_manifest.commit()?;
            watch_service::clear_needs_update(output_directory)?;
            telemetry.stages_ms.write =
                graphoxide_cli::build_telemetry::elapsed_millis(write_started);
            result.status = watch_service::RebuildStatus::Unchanged;
            result.clustered = false;
            return Ok(finish(
                result,
                telemetry,
                runtime_telemetry,
                progress_reporter,
            ));
        }
        drop(previous);
        let write_started = std::time::Instant::now();
        let outcome = graphoxide_cli::build_guard::commit_build(
            &output,
            graphoxide_cli::build_guard::BuildArtifact::Graph(&graph),
            build_progress,
            force,
            || {
                write_watch_markers(output_directory, marker_value)?;
                pending_manifest.commit()
            },
        )?;
        if outcome == graphoxide_cli::build_guard::BuildCommitOutcome::RefusedShrink {
            telemetry.stages_ms.write =
                graphoxide_cli::build_telemetry::elapsed_millis(write_started);
            result.status = watch_service::RebuildStatus::RefusedShrink;
            result.clustered = false;
            return Ok(finish(
                result,
                telemetry,
                runtime_telemetry,
                progress_reporter,
            ));
        }
        telemetry.stages_ms.write = graphoxide_cli::build_telemetry::elapsed_millis(write_started);
        telemetry.graph.nodes = result.stats.nodes;
        telemetry.graph.edges = result.stats.edges;
        save_build_config_in(output_directory, true, None, None)?;
        watch_service::clear_needs_update(output_directory)?;
        result.clustered = false;
        return Ok(finish(
            result,
            telemetry,
            runtime_telemetry,
            progress_reporter,
        ));
    }
    telemetry.stages_ms.build = graphoxide_cli::build_telemetry::elapsed_millis(build_started);
    result.stats.nodes = graph.nodes.len();
    result.stats.edges = graph.links.len();
    if previous
        .as_ref()
        .is_some_and(|existing| watch_service::same_topology(existing, &graph))
    {
        progress_reporter.phase(BuildProgressPhase::Publishing);
        let write_started = std::time::Instant::now();
        pending_manifest.commit()?;
        watch_service::clear_needs_update(output_directory)?;
        telemetry.stages_ms.write = graphoxide_cli::build_telemetry::elapsed_millis(write_started);
        result.status = watch_service::RebuildStatus::Unchanged;
        result.clustered = false;
        return Ok(finish(
            result,
            telemetry,
            runtime_telemetry,
            progress_reporter,
        ));
    }
    progress_reporter.phase(BuildProgressPhase::Clustering);
    let cluster_started = std::time::Instant::now();
    cluster_with_resource_gate(&mut graph)?;
    if let Some(previous) = &previous {
        graphoxide_graph::cluster::remap_communities_to_previous(&mut graph, previous);
    }
    drop(previous);
    telemetry.stages_ms.cluster = graphoxide_cli::build_telemetry::elapsed_millis(cluster_started);
    progress_reporter.phase(BuildProgressPhase::Publishing);
    let write_started = std::time::Instant::now();
    let outcome = graphoxide_cli::build_guard::commit_build(
        &output,
        graphoxide_cli::build_guard::BuildArtifact::Graph(&graph),
        build_progress,
        force,
        || {
            write_watch_markers(output_directory, marker_value)?;
            pending_manifest.commit()
        },
    )?;
    if outcome == graphoxide_cli::build_guard::BuildCommitOutcome::RefusedShrink {
        telemetry.stages_ms.write = graphoxide_cli::build_telemetry::elapsed_millis(write_started);
        result.status = watch_service::RebuildStatus::RefusedShrink;
        result.clustered = false;
        return Ok(finish(
            result,
            telemetry,
            runtime_telemetry,
            progress_reporter,
        ));
    }
    save_build_config_in(output_directory, false, None, None)?;
    telemetry.stages_ms.write = graphoxide_cli::build_telemetry::elapsed_millis(write_started);
    telemetry.graph.nodes = result.stats.nodes;
    telemetry.graph.edges = result.stats.edges;
    telemetry.graph.clustered = true;
    watch_service::clear_needs_update(output_directory)?;
    Ok(finish(
        result,
        telemetry,
        runtime_telemetry,
        progress_reporter,
    ))
}

fn save_build_config_in(
    output_directory: &std::path::Path,
    no_cluster: bool,
    excludes: Option<&[String]>,
    honor_gitignore: Option<bool>,
) -> anyhow::Result<()> {
    watch_service::write_build_config_with_cluster(
        output_directory,
        excludes,
        honor_gitignore,
        Some(!no_cluster),
    )?;
    Ok(())
}

fn global_graph(roots: &[PathBuf], output: &std::path::Path, force: bool) -> anyhow::Result<()> {
    let _managed_graph_lock = acquire_managed_graph_lock(output, None)?;
    let mut paths = Vec::new();
    for root in roots {
        for entry in ignore::WalkBuilder::new(root).hidden(false).build() {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type().is_some_and(|t| t.is_file())
                && path.file_name().and_then(|v| v.to_str()) == Some("graph.json")
                && path
                    .parent()
                    .and_then(|p| p.file_name())
                    .and_then(|v| v.to_str())
                    == Some("graphoxide-out")
            {
                paths.push(path.to_path_buf());
            }
        }
    }
    paths.sort();
    paths.dedup();
    if paths.is_empty() {
        anyhow::bail!("no graphoxide-out/graph.json files found")
    }
    let mut chunks = Vec::new();
    for path in &paths {
        let graph = graphoxide_core::read_graph(path)?;
        chunks.push(graphoxide_core::Extraction {
            nodes: graph.nodes,
            edges: graph.links,
            hyperedges: graph.hyperedges,
        });
    }
    let mut graph = graphoxide_graph::build_graph(&chunks)?;
    cluster_with_resource_gate(&mut graph)?;
    if !graphoxide_core::write_graph_atomic(output, &graph, force)? {
        anyhow::bail!("refusing to shrink existing global graph; pass --force")
    }
    write_output(&format!(
        "Merged {} project graphs into {} ({} nodes, {} edges)",
        paths.len(),
        output.display(),
        graph.nodes.len(),
        graph.links.len()
    ))
}

fn global_command(command: GlobalCommand) -> anyhow::Result<()> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("HOME is not set"))?;
    let directory = home.join(".graphoxide");
    let graph_path = directory.join("global-graph.json");
    let manifest_path = directory.join("global-manifest.json");
    match command {
        GlobalCommand::Path => write_output(&graph_path.display().to_string()),
        GlobalCommand::List => {
            let manifest = read_global_manifest(&manifest_path)?;
            let repos = manifest
                .get("repos")
                .and_then(|value| value.as_object())
                .cloned()
                .unwrap_or_default();
            if repos.is_empty() {
                return write_output(
                    "Global graph is empty. Use `graphoxide global add` to add a project.",
                );
            }
            let mut lines = vec![format!("Global graph: {}", graph_path.display())];
            for (tag, info) in repos {
                lines.push(format!(
                    "  {tag}: {} nodes, added {}",
                    info.get("node_count").and_then(|v| v.as_u64()).unwrap_or(0),
                    info.get("added_at").and_then(|v| v.as_str()).unwrap_or("?")
                ));
            }
            write_output(&lines.join("\n"))
        }
        GlobalCommand::Add { graph, repo_tag } => {
            let source = graph.canonicalize()?;
            let tag = repo_tag.unwrap_or_else(|| {
                source
                    .parent()
                    .and_then(|path| path.parent())
                    .and_then(|path| path.file_name())
                    .and_then(|name| name.to_str())
                    .unwrap_or("project")
                    .to_owned()
            });
            let mut global = if graph_path.is_file() {
                graphoxide_core::read_graph(&graph_path)?
            } else {
                graphoxide_core::KnowledgeGraph::default()
            };
            let removed_ids: std::collections::BTreeSet<_> = global
                .nodes
                .iter()
                .filter(|node| node.extra.get("repo").and_then(|v| v.as_str()) == Some(&tag))
                .map(|node| node.id.clone())
                .collect();
            let removed = removed_ids.len();
            global.nodes.retain(|node| !removed_ids.contains(&node.id));
            global.links.retain(|edge| {
                !removed_ids.contains(edge.true_source())
                    && !removed_ids.contains(edge.true_target())
            });
            global.hyperedges.retain(|edge| {
                edge.get("nodes")
                    .and_then(|members| members.as_array())
                    .is_none_or(|members| {
                        members.iter().all(|member| {
                            member
                                .as_str()
                                .is_none_or(|member| !removed_ids.contains(member))
                        })
                    })
            });

            let source_graph = graphoxide_core::read_graph(&source)?;
            let external: std::collections::BTreeMap<_, _> = global
                .nodes
                .iter()
                .filter(|node| node.source_file.is_empty())
                .map(|node| (graphoxide_core::normalize_id(&node.label), node.id.clone()))
                .collect();
            let mut remap = std::collections::BTreeMap::new();
            for mut node in source_graph.nodes {
                let local_id = node.id.clone();
                let id = if node.source_file.is_empty() {
                    external
                        .get(&graphoxide_core::normalize_id(&node.label))
                        .cloned()
                        .unwrap_or_else(|| graphoxide_core::make_id(&["external", &node.label]))
                } else {
                    graphoxide_core::make_id(&[&tag, &node.id])
                };
                remap.insert(local_id.clone(), id.clone());
                if global.nodes.iter().any(|existing| existing.id == id) {
                    continue;
                }
                node.id = id;
                if !node.source_file.is_empty() {
                    node.extra.insert("repo".into(), tag.clone().into());
                    node.extra.insert("local_id".into(), local_id.into());
                }
                global.nodes.push(node);
            }
            for mut edge in source_graph.links {
                let Some(source_id) = remap.get(edge.true_source()).cloned() else {
                    continue;
                };
                let Some(target_id) = remap.get(edge.true_target()).cloned() else {
                    continue;
                };
                if source_id == target_id {
                    continue;
                }
                edge.source = source_id.clone();
                edge.target = target_id.clone();
                edge.extra.insert("_src".into(), source_id.into());
                edge.extra.insert("_tgt".into(), target_id.into());
                global.links.push(edge);
            }
            for mut hyperedge in source_graph.hyperedges {
                let Some(object) = hyperedge.as_object_mut() else {
                    continue;
                };
                let Some(members) = object
                    .get_mut("nodes")
                    .and_then(|value| value.as_array_mut())
                else {
                    continue;
                };
                let mapped: Vec<_> = members
                    .iter()
                    .filter_map(|member| remap.get(member.as_str()?).cloned())
                    .map(serde_json::Value::from)
                    .collect();
                if mapped.is_empty() {
                    continue;
                }
                *members = mapped;
                let local_id = object
                    .get("id")
                    .and_then(|value| value.as_str())
                    .unwrap_or("hyperedge");
                object.insert(
                    "id".into(),
                    graphoxide_core::make_id(&[&tag, local_id]).into(),
                );
                object.insert("repo".into(), tag.clone().into());
                global.hyperedges.push(hyperedge);
            }
            global.nodes.sort_by(|a, b| a.id.cmp(&b.id));
            global.nodes.dedup_by(|a, b| a.id == b.id);
            global.links.sort_by(|a, b| {
                (a.true_source(), a.true_target(), a.relation.as_str()).cmp(&(
                    b.true_source(),
                    b.true_target(),
                    b.relation.as_str(),
                ))
            });
            global.links.dedup_by(|a, b| {
                (a.true_source(), a.true_target(), a.relation.as_str())
                    == (b.true_source(), b.true_target(), b.relation.as_str())
            });
            std::fs::create_dir_all(&directory)?;
            graphoxide_core::write_graph_atomic(&graph_path, &global, true)?;
            let mut manifest = read_global_manifest(&manifest_path)?;
            let repos = manifest
                .as_object_mut()
                .unwrap()
                .entry("repos")
                .or_insert_with(|| serde_json::json!({}));
            repos.as_object_mut().unwrap().insert(
                tag.clone(),
                serde_json::json!({
                    "added_at": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs().to_string(),
                    "source_path": source.display().to_string(),
                    "node_count": remap.len(),
                    "edge_count": global.links.len()
                }),
            );
            write_text(
                &manifest_path,
                &(serde_json::to_string_pretty(&manifest)? + "\n"),
            )?;
            write_output(&format!(
                "Added '{tag}' to global graph: +{} nodes, -{removed} pruned. Global: {}",
                remap.len(),
                graph_path.display()
            ))
        }
        GlobalCommand::Remove { repo_tag } => {
            let mut manifest = read_global_manifest(&manifest_path)?;
            let known = manifest
                .get_mut("repos")
                .and_then(|value| value.as_object_mut())
                .and_then(|repos| repos.remove(&repo_tag));
            if known.is_none() {
                anyhow::bail!("repo '{repo_tag}' not in global graph")
            }
            let mut graph = graphoxide_core::read_graph(&graph_path)?;
            let ids: std::collections::BTreeSet<_> = graph
                .nodes
                .iter()
                .filter(|node| node.extra.get("repo").and_then(|v| v.as_str()) == Some(&repo_tag))
                .map(|node| node.id.clone())
                .collect();
            graph.nodes.retain(|node| !ids.contains(&node.id));
            graph.links.retain(|edge| {
                !ids.contains(edge.true_source()) && !ids.contains(edge.true_target())
            });
            graph.hyperedges.retain(|edge| {
                edge.get("repo").and_then(|value| value.as_str()) != Some(&repo_tag)
            });
            graphoxide_core::write_graph_atomic(&graph_path, &graph, true)?;
            write_text(
                &manifest_path,
                &(serde_json::to_string_pretty(&manifest)? + "\n"),
            )?;
            write_output(&format!(
                "Removed '{repo_tag}' from global graph ({} nodes pruned).",
                ids.len()
            ))
        }
    }
}

fn read_global_manifest(path: &std::path::Path) -> anyhow::Result<serde_json::Value> {
    if !path.is_file() {
        return Ok(serde_json::json!({"version":1,"repos":{}}));
    }
    let value: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path)?)
        .map_err(|error| anyhow::anyhow!("invalid global manifest {}: {error}", path.display()))?;
    if !value.is_object() {
        anyhow::bail!(
            "invalid global manifest {}: expected object",
            path.display()
        )
    }
    Ok(value)
}

fn merge_driver(
    base: &std::path::Path,
    ours: &std::path::Path,
    theirs: &std::path::Path,
    output: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    let destination = output.unwrap_or(ours);
    let _managed_graph_lock = acquire_managed_graph_lock(destination, None)?;
    let mut chunks = Vec::new();
    for path in [base, ours, theirs] {
        if path.exists() {
            let graph = graphoxide_core::read_graph(path)?;
            chunks.push(graphoxide_core::Extraction {
                nodes: graph.nodes,
                edges: graph.links,
                hyperedges: graph.hyperedges,
            });
        }
    }
    let graph = graphoxide_graph::build_graph(&chunks)?;
    graphoxide_core::write_graph_atomic(destination, &graph, true)?;
    write_output(&format!(
        "Merged graph conflict into {}",
        destination.display()
    ))
}

fn record_query(
    kind: &str,
    question: &str,
    graph: &std::path::Path,
    result: &str,
    duration: std::time::Duration,
) {
    // Query stamps and optional logs are best-effort; read-only cache paths
    // must never turn a successful graph query into an error.
    let _ = (|| -> anyhow::Result<()> {
        let root = graph.parent().unwrap_or_else(|| std::path::Path::new("."));
        let stamp = root.join("cache/last_query_stamp");
        if let Some(parent) = stamp.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(stamp, b"")?;
        Ok(())
    })();
    graphoxide_query::log_query_from_env(&graphoxide_query::QueryLogRecord {
        kind,
        question,
        corpus: graph,
        result: Some(result),
        duration_ms: Some(duration.as_secs_f64() * 1000.0),
        mode: None,
        depth: None,
        nodes_returned: None,
    });
}

fn write_text(path: &std::path::Path, text: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&temporary, text)?;
    graphoxide_core::replace_file(&temporary, path)?;
    Ok(())
}

fn managed_output_directory(
    root: &std::path::Path,
    explicit_root: Option<&std::path::Path>,
) -> PathBuf {
    if let Some(explicit_root) = explicit_root {
        return explicit_root.join("graphoxide-out");
    }
    let configured = std::env::var_os("GRAPHOXIDE_OUT")
        .or_else(|| std::env::var_os("GRAPHIFY_OUT"))
        .map(PathBuf::from);
    match configured {
        Some(path) if path.is_absolute() => path,
        Some(path) => root.join(path),
        None => root.join("graphoxide-out"),
    }
}

fn acquire_managed_graph_lock(
    graph_path: &std::path::Path,
    known_managed_directory: Option<&std::path::Path>,
) -> anyhow::Result<Option<watch_service::RebuildLockGuard>> {
    let inferred = graph_path.parent().filter(|parent| {
        graph_path.file_name().and_then(|value| value.to_str()) == Some("graph.json")
            && matches!(
                parent.file_name().and_then(|value| value.to_str()),
                Some("graphoxide-out" | "graphify-out")
            )
    });
    let Some(output_directory) = known_managed_directory.or(inferred) else {
        return Ok(None);
    };
    watch_service::RebuildLockGuard::acquire(output_directory, true)
}

fn resolve_managed_graph_path(graph: PathBuf) -> PathBuf {
    if graph == std::path::Path::new("graphoxide-out/graph.json") {
        managed_output_directory(std::path::Path::new("."), None).join("graph.json")
    } else {
        graph
    }
}

fn load_community_labels(directory: &std::path::Path) -> std::collections::BTreeMap<i64, String> {
    for name in [".graphoxide_labels.json", ".graphify_labels.json"] {
        let Ok(bytes) = std::fs::read(directory.join(name)) else {
            continue;
        };
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            continue;
        };
        let object = value
            .get("labels")
            .or_else(|| value.get("communities"))
            .unwrap_or(&value)
            .as_object();
        if let Some(object) = object {
            let labels = object
                .iter()
                .filter_map(|(key, value)| {
                    let community = key.parse().ok()?;
                    let label = value.as_str().map(str::to_owned).or_else(|| {
                        value
                            .get("label")
                            .or_else(|| value.get("name"))
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned)
                    })?;
                    Some((community, label))
                })
                .collect();
            return labels;
        }
    }
    std::collections::BTreeMap::new()
}

fn is_placeholder_community_label(community: i64, label: &str) -> bool {
    label.trim().is_empty() || label.trim() == format!("Community {community}")
}

fn remove_placeholder_community_names(graph: &mut graphoxide_core::KnowledgeGraph) {
    for node in &mut graph.nodes {
        let placeholder = node.community.is_some_and(|community| {
            node.extra
                .get("community_name")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|label| is_placeholder_community_label(community, label))
        });
        if placeholder {
            node.extra.remove("community_name");
        }
    }
}

fn write_community_label_sidecars(
    output: &std::path::Path,
    labels: &std::collections::BTreeMap<i64, String>,
) -> anyhow::Result<()> {
    let labels = labels
        .iter()
        .map(|(community, label)| (community.to_string(), label))
        .collect::<std::collections::BTreeMap<_, _>>();
    for name in [".graphoxide_labels.json", ".graphify_labels.json"] {
        graphoxide_core::write_json_atomic(output.join(name), &labels, true)?;
    }
    Ok(())
}

fn write_cluster_sidecars(
    output: &std::path::Path,
    graph: &graphoxide_core::KnowledgeGraph,
    persist_labels: bool,
) -> anyhow::Result<()> {
    use std::collections::BTreeMap;
    std::fs::create_dir_all(output)?;
    let communities = graphoxide_export::communities_from_graph(graph);
    let analysis = graphoxide_graph::analyze(graph)?;
    let cohesion: BTreeMap<_, _> = communities
        .keys()
        .map(|community| (community.to_string(), 0.0_f64))
        .collect();
    let analysis_value = serde_json::json!({
        "communities": communities.iter().map(|(community, members)| (community.to_string(), members)).collect::<BTreeMap<_, _>>(),
        "cohesion": cohesion,
        "gods": analysis.god_nodes,
        "surprises": analysis.surprising_connections,
        "questions": analysis.suggested_questions,
    });
    for name in [".graphoxide_analysis.json", ".graphify_analysis.json"] {
        graphoxide_core::write_json_atomic(output.join(name), &analysis_value, true)?;
    }
    if persist_labels {
        write_community_label_sidecars(
            output,
            &graphoxide_export::community_labels_from_graph(graph),
        )?;
    }
    let report = graphoxide_export::render_report(graph, &graphoxide_graph::analyze(graph)?);
    graphoxide_core::write_text_atomic(output.join("GRAPH_REPORT.md"), &report)
}

#[allow(clippy::too_many_arguments)]
fn run_export(
    format: &str,
    positional: Option<PathBuf>,
    requested_output: Option<PathBuf>,
    requested_graph: Option<PathBuf>,
    requested_directory: Option<PathBuf>,
    no_viz: bool,
    max_sections: usize,
) -> anyhow::Result<()> {
    let managed = managed_output_directory(std::path::Path::new("."), None);
    let positional_is_graph = format == "callflow-html"
        && requested_output.is_some()
        && requested_graph.is_none()
        && positional.is_some();
    let graph_path = requested_graph
        .or_else(|| positional_is_graph.then(|| positional.clone()).flatten())
        .unwrap_or_else(|| managed.join("graph.json"));
    let default_output = match format {
        "html" => managed.join("graph.html"),
        "callflow-html" => managed.join("callflow.html"),
        "graphml" => managed.join("graph.graphml"),
        "cypher" | "neo4j" | "falkordb" => managed.join("cypher.txt"),
        "wiki" => managed.join("wiki"),
        "obsidian" => managed.join("obsidian"),
        "json" => managed.join("graph-copy.json"),
        _ => unreachable!("clap validates export formats"),
    };
    let output = requested_directory
        .or(requested_output)
        .or_else(|| (!positional_is_graph).then_some(positional).flatten())
        .unwrap_or(default_output);

    if format == "html" && no_viz {
        if output.is_file() {
            std::fs::remove_file(&output)?;
        }
        return write_output(&format!(
            "Visualization disabled; removed {}",
            output.display()
        ));
    }

    let graph = graphoxide_core::read_graph(&graph_path)?;
    match format {
        "html" => {
            let labels = load_community_labels(
                graph_path
                    .parent()
                    .unwrap_or_else(|| std::path::Path::new(".")),
            );
            let options = graphoxide_export::HtmlOptions {
                community_labels: labels,
                ..Default::default()
            };
            write_text(
                &output,
                &graphoxide_export::render_html_with_options(&graph, &options)?,
            )?;
        }
        "callflow-html" => {
            let sidecar_directory = graph_path
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."));
            let labels = load_community_labels(sidecar_directory);
            let report = std::fs::read_to_string(sidecar_directory.join("GRAPH_REPORT.md"))
                .unwrap_or_default();
            graphoxide_export::write_callflow_html(
                &graph,
                &output,
                &labels,
                &report,
                max_sections,
            )?;
            return write_output(&format!("callflow HTML written to {}", output.display()));
        }
        "graphml" => graphoxide_export::write_graphml(&graph, &output)?,
        "cypher" | "neo4j" | "falkordb" => {
            write_text(&output, &graphoxide_export::render_cypher(&graph))?
        }
        "wiki" => graphoxide_export::export_wiki(&graph, &output)?,
        "obsidian" => {
            let mut communities = graphoxide_export::communities_from_graph(&graph);
            if communities.is_empty() && !graph.nodes.is_empty() {
                communities.insert(0, graph.nodes.iter().map(|node| node.id.clone()).collect());
            }
            let labels = load_community_labels(
                graph_path
                    .parent()
                    .unwrap_or_else(|| std::path::Path::new(".")),
            );
            let options = graphoxide_export::VaultOptions {
                community_labels: labels.clone(),
                ..Default::default()
            };
            graphoxide_export::export_vault_with_options(&graph, &communities, &output, &options)?;
            graphoxide_export::export_canvas(
                &graph,
                &communities,
                &output.join("graph.canvas"),
                &labels,
            )?;
        }
        "json" => {
            graphoxide_export::export_graph_json(&graph, &output, true)?;
        }
        _ => unreachable!("clap validates export formats"),
    }
    write_output(&format!("Wrote {}", output.display()))
}

fn write_output(output: &str) -> anyhow::Result<()> {
    let mut stdout = std::io::stdout().lock();
    match writeln!(stdout, "{output}") {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Render the immutable, byte-oriented format contract without touching the
/// filesystem. This is intentionally driven directly by `FormatRegistry`, so
/// CLI capability reporting cannot drift from detector/adaptor ownership.
fn format_capability_output(json: bool) -> anyhow::Result<String> {
    let reports = graphoxide_extract::format_registry::format_registry()
        .capability_reports()
        .collect::<Vec<_>>();
    if json {
        return serde_json::to_string_pretty(&reports).map_err(Into::into);
    }

    let mut output = String::new();
    for report in reports {
        let extensions = report.extensions.join(",");
        let file_names = report.file_names.join(",");
        use std::fmt::Write as _;
        writeln!(
            output,
            "{}\t{}\t{}\t{}\t{}\t{}",
            report.id.as_str(),
            report.capability.as_str(),
            report.schema_requirement.as_str(),
            report.adapter.as_str(),
            extensions,
            file_names,
        )?;
    }
    Ok(output.trim_end_matches('\n').to_owned())
}

fn annotate_query_context(output: &mut String, contexts: &[String], source: &str) {
    if contexts.is_empty() {
        return;
    }
    let annotation = format!("Context: {} ({source})", contexts.join(", "));
    if let Some(header_end) = output.find('\n') {
        output.insert_str(header_end, &format!(" | {annotation}"));
    } else {
        output.push_str(&format!("\n{annotation}"));
    }
}

fn format_god_nodes(nodes: &[(String, String, usize)], json: bool) -> anyhow::Result<String> {
    if json {
        #[derive(serde::Serialize)]
        struct GodNode<'a> {
            id: &'a str,
            label: &'a str,
            degree: usize,
        }
        let values: Vec<_> = nodes
            .iter()
            .map(|(id, label, degree)| GodNode {
                id,
                label,
                degree: *degree,
            })
            .collect();
        return Ok(serde_json::to_string_pretty(&values)?);
    }
    let mut lines = vec!["God nodes (most connected):".to_owned()];
    lines.extend(nodes.iter().enumerate().map(|(index, (_, label, degree))| {
        format!(
            "  {}. {} - {} edges",
            index + 1,
            graphoxide_core::sanitize_label(label),
            degree
        )
    }));
    Ok(lines.join("\n"))
}

fn load_learning_overlay(
    graph_path: &std::path::Path,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    serde_json::to_value(graphoxide_core::load_learning_overlay(graph_path))
        .ok()?
        .as_object()
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::{
        annotate_query_context, audit_report, format_capability_output, format_god_nodes,
        incremental_graph_budget_after_retained_scan, load_learning_overlay,
        optional_baseline_leaves_full_graph_headroom, read_incremental_baseline,
        relevant_watch_paths, resolve_label_transport_inputs, run_project_build_with_cancellation,
        stale_local_sources, watch_change_requires_structural_rebuild, Cli, Command,
        IncrementalGraphBudget, ProgressModeArg, ProjectBuildOptions, ProjectBuildWorkflow,
        RuntimeIoBackendArg, RuntimeOptions,
    };
    use clap::Parser;
    use graphoxide_cli::build_progress::{BuildProgressMode, BuildProgressReporter};
    use std::path::{Path, PathBuf};

    fn god_test_graph() -> graphoxide_core::KnowledgeGraph {
        let node = |id: &str, label: &str, source: &str| graphoxide_core::Node {
            id: id.into(),
            label: label.into(),
            file_type: "code".into(),
            source_file: source.into(),
            source_location: Some("L1".into()),
            community: None,
            extra: Default::default(),
        };
        let mut graph = graphoxide_core::KnowledgeGraph {
            nodes: vec![
                node("hub", "Auth", "auth.py"),
                node("file", "auth.py", "auth.py"),
            ],
            ..Default::default()
        };
        for index in 0..4 {
            let id = format!("caller{index}");
            graph
                .nodes
                .push(node(&id, &format!("c{index}()"), &format!("m{index}.py")));
            graph.links.push(graphoxide_core::Edge {
                source: id,
                target: "hub".into(),
                relation: "calls".into(),
                confidence: graphoxide_core::Confidence::Extracted,
                source_file: String::new(),
                extra: Default::default(),
            });
        }
        graph.links.push(graphoxide_core::Edge {
            source: "file".into(),
            target: "hub".into(),
            relation: "contains".into(),
            confidence: graphoxide_core::Confidence::Extracted,
            source_file: String::new(),
            extra: Default::default(),
        });
        graph
    }

    #[test]
    fn remote_ollama_http_is_key_optional_but_transport_is_disclosed() {
        let environment = std::collections::BTreeMap::from([
            (
                "GRAPHOXIDE_LLM_BASE_URL".to_owned(),
                "http://192.168.10.10:11434/v1".to_owned(),
            ),
            ("OLLAMA_MODEL".to_owned(), "qwen-test:latest".to_owned()),
        ]);
        let inputs =
            resolve_label_transport_inputs("ollama", None, |name| environment.get(name).cloned())
                .expect("resolve keyless LAN Ollama transport");

        assert_eq!(
            inputs.endpoint,
            "http://192.168.10.10:11434/v1/chat/completions"
        );
        assert_eq!(inputs.model, "qwen-test:latest");
        assert_eq!(inputs.key, None);
        let override_ = inputs.ollama_dns_override.as_ref().unwrap();
        assert_eq!(override_.host, "192.168.10.10");
        assert_eq!(
            override_.addresses,
            vec!["192.168.10.10:0".parse().unwrap()]
        );
        assert!(inputs.warning.is_some_and(|warning| {
            warning.contains("graph-derived labels") && warning.contains("plaintext HTTP")
        }));
    }

    #[test]
    fn remote_ollama_transport_keeps_optional_key_and_blocks_metadata_targets() {
        let keyed = std::collections::BTreeMap::from([
            (
                "GRAPHOXIDE_LLM_BASE_URL".to_owned(),
                "http://192.168.10.10:11434/v1".to_owned(),
            ),
            ("OLLAMA_API_KEY".to_owned(), "bound-key".to_owned()),
        ]);
        let inputs =
            resolve_label_transport_inputs("ollama", Some("qwen"), |name| keyed.get(name).cloned())
                .expect("resolve keyed LAN Ollama transport");
        assert_eq!(inputs.key.as_deref(), Some("bound-key"));

        for base_url in [
            "http://169.254.169.254:11434/v1",
            "http://0.0.0.0:11434/v1",
            "file:///tmp/ollama",
            "http://secret@192.168.10.10:11434/v1",
            "http://@192.168.10.10:11434/v1",
            "http://:@192.168.10.10:11434/v1",
            "http:@192.168.10.10:11434/v1",
            r"http:\@192.168.10.10:11434/v1",
            "http://192.168.10.10:11434/v1?key=secret",
        ] {
            let environment = std::collections::BTreeMap::from([(
                "GRAPHOXIDE_LLM_BASE_URL".to_owned(),
                base_url.to_owned(),
            )]);
            assert!(
                resolve_label_transport_inputs("ollama", Some("qwen"), |name| {
                    environment.get(name).cloned()
                })
                .is_err(),
                "{base_url}"
            );
        }
    }

    #[test]
    fn ollama_ipv6_loopback_keeps_a_bracketed_endpoint_without_a_remote_warning() {
        let environment = std::collections::BTreeMap::from([(
            "GRAPHOXIDE_LLM_BASE_URL".to_owned(),
            "http://[::1]:11434/v1".to_owned(),
        )]);
        let inputs = resolve_label_transport_inputs("ollama", Some("qwen"), |name| {
            environment.get(name).cloned()
        })
        .expect("resolve IPv6 loopback Ollama transport");
        assert_eq!(inputs.endpoint, "http://[::1]:11434/v1/chat/completions");
        assert!(inputs.warning.is_none());
        let override_ = inputs.ollama_dns_override.as_ref().unwrap();
        assert_eq!(override_.host, "::1");
        assert_eq!(override_.addresses, vec!["[::1]:0".parse().unwrap()]);
    }

    #[test]
    fn watch_paths_exclude_generated_graph_and_git_events() {
        let root = Path::new("/workspace/project");
        let paths = vec![
            root.join("src/main.rs"),
            root.join("graphoxide-out/graph.json"),
            root.join("graphoxide-out/graph.json.123.tmp"),
            root.join(".git/index"),
            root.join("docs/guide.md"),
        ];

        assert_eq!(
            relevant_watch_paths(root, paths),
            vec![root.join("src/main.rs"), root.join("docs/guide.md")]
        );
    }

    #[test]
    fn watch_dispatches_confirmed_mpeg_ts_as_a_structural_change() {
        let temp = tempfile::tempdir().expect("tempdir");
        let media = temp.path().join("segment.ts");
        let mut packet = [0xff; 188];
        packet[..4].copy_from_slice(&[0x47, 0x40, 0x00, 0x10]);
        std::fs::write(&media, packet.repeat(5)).expect("write MPEG transport stream");
        assert!(watch_change_requires_structural_rebuild(&media));

        let code = temp.path().join("main.ts");
        std::fs::write(&code, b"export const answer = 42;\n").expect("write TypeScript");
        assert!(watch_change_requires_structural_rebuild(&code));
        assert!(watch_change_requires_structural_rebuild(
            &temp.path().join("deleted.ts")
        ));

        let video = temp.path().join("clip.mp4");
        std::fs::write(&video, b"not relevant to structural extraction").expect("write video");
        assert!(!watch_change_requires_structural_rebuild(&video));
    }

    #[test]
    fn watch_paths_exclude_generated_components_without_a_shared_prefix() {
        let paths = vec![
            PathBuf::from("graphoxide-out/graph.json"),
            PathBuf::from("src/lib.rs"),
        ];

        assert_eq!(
            relevant_watch_paths(Path::new("."), paths),
            vec![PathBuf::from("src/lib.rs")]
        );
    }

    #[test]
    fn update_accepts_force_for_managed_graph_reductions() {
        let cli = Cli::try_parse_from(["graphoxide", "update", ".", "--force"])
            .expect("parse update --force");
        assert!(matches!(cli.command, Command::Update { force: true, .. }));
    }

    #[test]
    fn build_commands_accept_explicit_progress_modes_and_default_to_auto() {
        let extract = Cli::try_parse_from(["graphoxide", "extract", ".", "--progress=json"])
            .expect("parse extract progress");
        assert!(matches!(
            extract.command,
            Command::Extract {
                build: ProjectBuildOptions {
                    progress: ProgressModeArg::Json,
                    ..
                },
                ..
            }
        ));
        let index = Cli::try_parse_from(["graphoxide", "index", ".", "--progress", "never"])
            .expect("parse index progress");
        assert!(matches!(
            index.command,
            Command::Index {
                build: ProjectBuildOptions {
                    progress: ProgressModeArg::Never,
                    ..
                }
            }
        ));
        let update = Cli::try_parse_from(["graphoxide", "update", "."])
            .expect("parse default update progress");
        assert!(matches!(
            update.command,
            Command::Update {
                progress: ProgressModeArg::Auto,
                ..
            }
        ));
        let watch = Cli::try_parse_from(["graphoxide", "watch", ".", "--progress=json"])
            .expect("parse watch progress");
        assert!(matches!(
            watch.command,
            Command::Watch {
                progress: ProgressModeArg::Json,
                ..
            }
        ));
    }

    #[test]
    fn extract_and_update_accept_opt_in_runtime_reports() {
        let extract = Cli::try_parse_from([
            "graphoxide",
            "extract",
            ".",
            "--runtime-report",
            "runtime/extract.json",
        ])
        .expect("parse extract runtime report");
        assert!(matches!(
            extract.command,
            Command::Extract {
                build: ProjectBuildOptions {
                    runtime_report: Some(path),
                    ..
                },
                ..
            } if path.as_path() == Path::new("runtime/extract.json")
        ));

        let update = Cli::try_parse_from([
            "graphoxide",
            "update",
            ".",
            "--runtime-report=runtime/update.json",
        ])
        .expect("parse update runtime report");
        assert!(matches!(
            update.command,
            Command::Update {
                runtime_report: Some(path),
                ..
            } if path.as_path() == Path::new("runtime/update.json")
        ));

        let watch = Cli::try_parse_from([
            "graphoxide",
            "watch",
            ".",
            "--runtime-report",
            "runtime/watch.json",
        ])
        .expect("parse watch runtime report");
        assert!(matches!(
            watch.command,
            Command::Watch {
                runtime_report: Some(path),
                ..
            } if path.as_path() == Path::new("runtime/watch.json")
        ));
    }

    #[test]
    fn isolated_runtime_controls_are_available_for_extract_update_and_watch() {
        let extract = Cli::try_parse_from([
            "graphoxide",
            "extract",
            ".",
            "--memory-budget-bytes",
            "1048576",
            "--io-workers=2",
            "--compute-workers",
            "3",
            "--io-backend",
            "io-uring",
            "--read-batch-bytes=4096",
        ])
        .expect("parse extract runtime controls");
        let Command::Extract {
            build: ProjectBuildOptions { runtime, .. },
            ..
        } = extract.command
        else {
            panic!("expected extract command");
        };
        assert_eq!(runtime.memory_budget_bytes, Some(1_048_576));
        assert_eq!(runtime.io_workers, Some(2));
        assert_eq!(runtime.compute_workers, Some(3));
        assert_eq!(runtime.io_backend, Some(RuntimeIoBackendArg::IoUring));
        assert_eq!(runtime.read_batch_bytes, Some(4096));
        let resolved = runtime.resolve().expect("valid explicit runtime controls");
        assert_eq!(resolved.memory_budget_bytes, 1_048_576);
        assert_eq!(resolved.io_workers, 2);
        assert_eq!(resolved.compute_workers, 3);
        assert_eq!(
            resolved.io_backend,
            graphoxide_index_runtime::IoBackendSelection::IoUring
        );
        assert_eq!(resolved.read_batch_bytes, 4096);

        let update = Cli::try_parse_from([
            "graphoxide",
            "update",
            ".",
            "--memory-budget-bytes=1048576",
            "--io-workers=1",
            "--compute-workers=1",
            "--io-backend=threaded",
            "--read-batch-bytes=1024",
        ])
        .expect("parse update runtime controls");
        assert!(matches!(
            update.command,
            Command::Update {
                runtime: RuntimeOptions {
                    memory_budget_bytes: Some(1_048_576),
                    io_workers: Some(1),
                    compute_workers: Some(1),
                    io_backend: Some(RuntimeIoBackendArg::Threaded),
                    read_batch_bytes: Some(1024),
                },
                ..
            }
        ));

        let watch = Cli::try_parse_from([
            "graphoxide",
            "watch",
            ".",
            "--memory-budget-bytes=1048576",
            "--io-workers=1",
            "--compute-workers=1",
            "--io-backend=threaded",
            "--read-batch-bytes=1024",
        ])
        .expect("parse watch runtime controls");
        assert!(matches!(
            watch.command,
            Command::Watch {
                runtime: RuntimeOptions {
                    memory_budget_bytes: Some(1_048_576),
                    io_workers: Some(1),
                    compute_workers: Some(1),
                    io_backend: Some(RuntimeIoBackendArg::Threaded),
                    read_batch_bytes: Some(1024),
                },
                ..
            }
        ));
    }

    #[test]
    fn index_accepts_the_shared_build_controls_but_not_the_legacy_executor() {
        let index = Cli::try_parse_from([
            "graphoxide",
            "index",
            "workspace",
            "--code-only",
            "--no-cluster",
            "--force",
            "--allow-partial",
            "--timing",
            "--json",
            "--runtime-report",
            "runtime/index.json",
            "--memory-budget-bytes",
            "1048576",
            "--io-workers",
            "2",
            "--compute-workers",
            "3",
            "--read-batch-bytes",
            "4096",
            "--out",
            "artifacts",
            "--exclude",
            "vendor/**",
            "--no-gitignore",
        ])
        .expect("parse index controls");
        let Command::Index {
            build:
                ProjectBuildOptions {
                    path,
                    code_only: true,
                    no_cluster: true,
                    force: true,
                    allow_partial: true,
                    timing: true,
                    json: true,
                    runtime_report: Some(runtime_report),
                    runtime,
                    out: Some(out),
                    exclude,
                    no_gitignore: true,
                    ..
                },
        } = index.command
        else {
            panic!("expected index command");
        };
        assert_eq!(path, Path::new("workspace"));
        assert_eq!(runtime_report, Path::new("runtime/index.json"));
        assert_eq!(out, Path::new("artifacts"));
        assert_eq!(exclude, ["vendor/**"]);
        assert_eq!(runtime.io_workers, Some(2));
        assert_eq!(runtime.compute_workers, Some(3));

        let error = match Cli::try_parse_from(["graphoxide", "index", ".", "--legacy-executor"]) {
            Ok(_) => panic!("index must not expose the unbounded legacy executor"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("--legacy-executor"));
    }

    #[test]
    fn cancelled_index_cli_path_preserves_seeded_artifacts() {
        use std::fs;

        let temp = tempfile::tempdir().expect("temporary fixture");
        let project = temp.path().join("project");
        let output_root = temp.path().join("output-root");
        let output = output_root.join("graphoxide-out");
        fs::create_dir_all(&project).expect("project");
        fs::create_dir_all(&output).expect("output");
        fs::write(project.join("main.rs"), "fn main() {}\n").expect("source");
        fs::write(output.join("graph.json"), b"old graph\n").expect("graph");
        fs::write(output.join("manifest.json"), b"old manifest\n").expect("manifest");
        fs::write(output.join("coverage.json"), b"old coverage\n").expect("coverage");
        let before = ["graph.json", "manifest.json", "coverage.json"]
            .map(|name| fs::read(output.join(name)).unwrap());
        let cancellation = graphoxide_index_runtime::RuntimeCancellation::new();
        cancellation.cancel();

        let error = run_project_build_with_cancellation(
            ProjectBuildOptions {
                path: project,
                code_only: false,
                no_cluster: false,
                force: false,
                postgres: None,
                allow_partial: false,
                timing: false,
                json: false,
                progress: ProgressModeArg::Never,
                runtime_report: None,
                runtime: RuntimeOptions::default(),
                out: Some(output_root),
                exclude: Vec::new(),
                no_gitignore: false,
            },
            ProjectBuildWorkflow::Index,
            cancellation,
        )
        .expect_err("cancelled index");
        assert!(error.to_string().contains("project build cancelled"));
        for (name, expected) in ["graph.json", "manifest.json", "coverage.json"]
            .into_iter()
            .zip(before)
        {
            assert_eq!(fs::read(output.join(name)).unwrap(), expected, "{name}");
        }
    }

    #[test]
    fn legacy_executor_rejects_isolated_runtime_controls() {
        let options = RuntimeOptions {
            io_workers: Some(2),
            ..RuntimeOptions::default()
        };
        assert!(options.resolve_for_executor(true).is_err());
        assert!(RuntimeOptions::default()
            .resolve_for_executor(true)
            .expect("legacy without overrides")
            .is_none());
    }

    #[test]
    fn isolated_executor_is_the_default_and_legacy_requires_an_explicit_flag() {
        let extract =
            Cli::try_parse_from(["graphoxide", "extract", "."]).expect("parse default extract");
        assert!(matches!(
            extract.command,
            Command::Extract {
                legacy_executor: false,
                ..
            }
        ));
        let update =
            Cli::try_parse_from(["graphoxide", "update", "."]).expect("parse default update");
        assert!(matches!(
            update.command,
            Command::Update {
                legacy_executor: false,
                ..
            }
        ));
        let watch = Cli::try_parse_from(["graphoxide", "watch", "."]).expect("parse default watch");
        assert!(matches!(
            watch.command,
            Command::Watch {
                legacy_executor: false,
                ..
            }
        ));
        let legacy = Cli::try_parse_from(["graphoxide", "extract", ".", "--legacy-executor"])
            .expect("parse legacy escape hatch");
        assert!(matches!(
            legacy.command,
            Command::Extract {
                legacy_executor: true,
                ..
            }
        ));
        let legacy = Cli::try_parse_from(["graphoxide", "update", ".", "--legacy-executor"])
            .expect("parse legacy update escape hatch");
        assert!(matches!(
            legacy.command,
            Command::Update {
                legacy_executor: true,
                ..
            }
        ));
        let legacy = Cli::try_parse_from(["graphoxide", "watch", ".", "--legacy-executor"])
            .expect("parse legacy watch escape hatch");
        assert!(matches!(
            legacy.command,
            Command::Watch {
                legacy_executor: true,
                ..
            }
        ));
        let hook = Cli::try_parse_from(["graphoxide", "hook-rebuild", "post-checkout", "."])
            .expect("parse default hook rebuild");
        assert!(matches!(
            hook.command,
            Command::HookRebuild {
                legacy_executor: false,
                ..
            }
        ));
        let legacy = Cli::try_parse_from([
            "graphoxide",
            "hook-rebuild",
            "post-checkout",
            ".",
            "--legacy-executor",
        ])
        .expect("parse legacy hook rebuild escape hatch");
        assert!(matches!(
            legacy.command,
            Command::HookRebuild {
                legacy_executor: true,
                ..
            }
        ));
    }

    #[test]
    fn isolated_watch_pass_publishes_graph_before_manifest_and_reports_unchanged() {
        let project = tempfile::tempdir().expect("temporary project");
        std::fs::write(project.path().join("app.py"), "def app():\n    return 1\n")
            .expect("write source");
        let output = project.path().join("graphoxide-out");
        let first = super::rebuild_isolated_pass(super::IsolatedRebuildRequest {
            path: project.path(),
            output_directory: &output,
            marker_value: ".",
            no_cluster: true,
            force: false,
            scope: graphoxide_cli::watch::RebuildScope::Incremental,
            pass: 1,
            runtime_config: graphoxide_index_runtime::IndexRuntimeConfig::default(),
            collect_runtime_telemetry: false,
            progress_reporter: BuildProgressReporter::new_adaptive(
                graphoxide_cli::build_telemetry::BuildOperation::Update,
                BuildProgressMode::Never,
            )
            .expect("silent progress reporter"),
        })
        .expect("isolated watch pass");
        assert_eq!(
            first.result.status,
            graphoxide_cli::watch::RebuildStatus::Rebuilt
        );
        assert_eq!(
            first.result.scope,
            graphoxide_cli::watch::RebuildScope::Full,
            "a missing committed baseline requires a truthful full pass"
        );
        assert_eq!(
            first.telemetry.mode,
            graphoxide_cli::build_telemetry::BuildMode::Full
        );
        assert_eq!(
            first.runtime_telemetry.execution_model,
            graphoxide_cli::build_telemetry::RuntimeExecutionModel::Isolated
        );
        assert!(output.join("graph.json").is_file());
        assert!(output.join("manifest.json").is_file());
        assert_eq!(
            std::fs::read_to_string(output.join(graphoxide_cli::watch::ROOT_MARKER))
                .expect("root marker"),
            "."
        );

        let second = super::rebuild_isolated_pass(super::IsolatedRebuildRequest {
            path: project.path(),
            output_directory: &output,
            marker_value: ".",
            no_cluster: true,
            force: false,
            scope: graphoxide_cli::watch::RebuildScope::Incremental,
            pass: 2,
            runtime_config: graphoxide_index_runtime::IndexRuntimeConfig::default(),
            collect_runtime_telemetry: false,
            progress_reporter: BuildProgressReporter::new_adaptive(
                graphoxide_cli::build_telemetry::BuildOperation::Update,
                BuildProgressMode::Never,
            )
            .expect("silent progress reporter"),
        })
        .expect("unchanged isolated watch pass");
        assert_eq!(
            second.result.status,
            graphoxide_cli::watch::RebuildStatus::Unchanged
        );
        assert_eq!(
            second.result.scope,
            graphoxide_cli::watch::RebuildScope::Incremental
        );
        assert_eq!(
            second.telemetry.mode,
            graphoxide_cli::build_telemetry::BuildMode::Incremental
        );
    }

    #[test]
    fn incremental_budget_charges_fresh_output_before_baseline_admission() {
        let after_fresh = incremental_graph_budget_after_retained_scan(2_000, 400, 600)
            .expect("fresh output leaves graph headroom");
        assert_eq!(after_fresh.max_baseline_file_bytes, 125);
        assert_eq!(after_fresh.max_graph_materialized_bytes, 1_400);
        let error = incremental_graph_budget_after_retained_scan(2_000, 400, 1_599)
            .expect_err("one byte cannot hold the baseline and merged graph");
        let error = error.to_string();
        assert!(error.contains("insufficient incremental graph headroom"));
        assert!(error.contains("pending manifest retains 1599 bytes"));

        assert!(
            optional_baseline_leaves_full_graph_headroom(1_000, 8_000),
            "a proven in-budget full graph may retain the optional baseline"
        );
        assert!(
            !optional_baseline_leaves_full_graph_headroom(1_000, 7_999),
            "an otherwise in-budget full graph must skip optional remapping when the loaded baseline removes required headroom"
        );

        let directory = tempfile::tempdir().expect("temporary baseline directory");
        let graph_path = directory.path().join("graph.json");
        graphoxide_core::write_json_atomic(
            &graph_path,
            &serde_json::json!({"nodes": [], "links": [], "hyperedges": []}),
            true,
        )
        .expect("write tiny baseline");
        let error = read_incremental_baseline(
            &graph_path,
            2_000,
            IncrementalGraphBudget {
                max_baseline_file_bytes: 2_000,
                max_graph_materialized_bytes: 1,
            },
        )
        .expect_err("post-read baseline working set must fit");
        let error = error.to_string();
        assert!(error.contains("increase --memory-budget-bytes"), "{error}");
        assert!(error.contains("request a full rebuild"), "{error}");
    }

    #[test]
    fn stale_sources_follow_explicit_outer_container_ownership() {
        let project = tempfile::tempdir().expect("temporary project");
        let root = project
            .path()
            .canonicalize()
            .expect("canonical project root");
        let archive = root.join("archives/structured.tar");
        std::fs::create_dir_all(archive.parent().expect("archive parent"))
            .expect("create archive directory");
        std::fs::write(&archive, b"deterministic archive placeholder")
            .expect("write archive placeholder");
        let literal = root.join("literal!/file.rs");
        std::fs::create_dir_all(literal.parent().expect("literal parent"))
            .expect("create literal directory");
        std::fs::write(&literal, b"fn literal() {}\n").expect("write literal source");
        let node = |id: &str, source_file: &str| graphoxide_core::Node {
            id: id.into(),
            label: id.into(),
            file_type: "structured_file".into(),
            source_file: source_file.into(),
            source_location: None,
            community: None,
            extra: Default::default(),
        };
        let member_node = |id: &str, source_file: &str| {
            let mut node = node(id, source_file);
            node.extra.insert(
                graphoxide_core::CONTAINER_SOURCE_ATTRIBUTE.into(),
                "archives/structured.tar".into(),
            );
            node
        };
        let graph = graphoxide_core::KnowledgeGraph {
            nodes: vec![
                node("archive", "archives/structured.tar"),
                member_node("csv_member", "archives/structured.tar!/inventory/ports.csv"),
                member_node(
                    "nested_member",
                    "archives/structured.tar!/nested/config.zip!/runtime.toml",
                ),
                node("literal", "literal!/file.rs"),
            ],
            ..Default::default()
        };

        assert!(
            stale_local_sources(&graph, &root, &[archive.clone(), literal.clone()]).is_empty(),
            "member facts must remain live with their explicitly recorded outer container"
        );

        assert_eq!(
            stale_local_sources(&graph, &root, std::slice::from_ref(&literal)),
            vec![archive.clone()],
            "deleted container members must collapse to their shared outer prune identity"
        );
        assert_eq!(
            stale_local_sources(&graph, &root, std::slice::from_ref(&archive)),
            vec![literal],
            "an unmarked path containing the reserved spelling remains independently tracked"
        );
    }

    #[test]
    fn isolated_incremental_pass_rejects_an_over_budget_baseline_without_committing() {
        let project = tempfile::tempdir().expect("temporary project");
        let source = project.path().join("app.py");
        std::fs::write(&source, "def app():\n    return 1\n").expect("write source");
        let output = project.path().join("graphoxide-out");
        super::rebuild_isolated_pass(super::IsolatedRebuildRequest {
            path: project.path(),
            output_directory: &output,
            marker_value: ".",
            no_cluster: true,
            force: false,
            scope: graphoxide_cli::watch::RebuildScope::Incremental,
            pass: 1,
            runtime_config: graphoxide_index_runtime::IndexRuntimeConfig::default(),
            collect_runtime_telemetry: false,
            progress_reporter: BuildProgressReporter::new_adaptive(
                graphoxide_cli::build_telemetry::BuildOperation::Update,
                BuildProgressMode::Never,
            )
            .expect("silent progress reporter"),
        })
        .expect("initial isolated pass");

        let graph_path = output.join("graph.json");
        let manifest_path = output.join("manifest.json");
        let mut graph_value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&graph_path).expect("read initial graph"))
                .expect("parse initial graph");
        graph_value["budget_regression_padding"] = "x".repeat(64 * 1024).into();
        std::fs::write(
            &graph_path,
            serde_json::to_vec_pretty(&graph_value).expect("serialize padded graph"),
        )
        .expect("write padded graph");
        let graph_before = std::fs::read(&graph_path).expect("snapshot graph");
        let manifest_before = std::fs::read(&manifest_path).expect("snapshot manifest");
        std::fs::write(
            &source,
            "def app():\n    return 1\n\ndef changed():\n    return app()\n",
        )
        .expect("change source");

        let error = match super::rebuild_isolated_pass(super::IsolatedRebuildRequest {
            path: project.path(),
            output_directory: &output,
            marker_value: ".",
            no_cluster: true,
            force: false,
            scope: graphoxide_cli::watch::RebuildScope::Incremental,
            pass: 2,
            runtime_config: graphoxide_index_runtime::IndexRuntimeConfig {
                memory_budget_bytes: 1024 * 1024,
                io_workers: 1,
                compute_workers: 1,
                io_backend: graphoxide_index_runtime::IoBackendSelection::Threaded,
                read_batch_bytes: 4 * 1024,
            },
            collect_runtime_telemetry: false,
            progress_reporter: BuildProgressReporter::new_adaptive(
                graphoxide_cli::build_telemetry::BuildOperation::Update,
                BuildProgressMode::Never,
            )
            .expect("silent progress reporter"),
        }) {
            Ok(_) => panic!("oversized baseline must fail"),
            Err(error) => error,
        };

        let error = format!("{error:#}");
        assert!(error.contains("load incremental baseline"), "{error}");
        assert!(
            error.contains("exceeds") && error.contains("byte cap"),
            "{error}"
        );
        assert_eq!(std::fs::read(&graph_path).unwrap(), graph_before);
        assert_eq!(std::fs::read(&manifest_path).unwrap(), manifest_before);
    }

    #[test]
    fn isolated_reverse_aba_aborts_then_force_repairs_without_losing_unrelated_semantics() {
        for no_cluster in [false, true] {
            let project = tempfile::tempdir().expect("temporary project");
            let main = project.path().join("main.ts");
            let segment = project.path().join("segment.ts");
            let code = b"export const phantom = 42;\n";
            std::fs::write(
                &main,
                b"import { phantom } from './segment';\nexport const main = phantom;\n",
            )
            .expect("write importer");
            std::fs::write(&segment, code).expect("write initial TypeScript");
            let output = project.path().join("graphoxide-out");
            let request = |force| super::IsolatedRebuildRequest {
                path: project.path(),
                output_directory: &output,
                marker_value: ".",
                no_cluster,
                force,
                scope: graphoxide_cli::watch::RebuildScope::Incremental,
                pass: 1,
                runtime_config: graphoxide_index_runtime::IndexRuntimeConfig::default(),
                collect_runtime_telemetry: false,
                progress_reporter: BuildProgressReporter::new_adaptive(
                    graphoxide_cli::build_telemetry::BuildOperation::Update,
                    BuildProgressMode::Never,
                )
                .expect("silent progress reporter"),
            };
            super::rebuild_isolated_pass(request(false)).expect("initial Code baseline");
            let manifest_path = output.join("manifest.json");
            let graph_path = output.join("graph.json");
            let stale_code_manifest = std::fs::read(&manifest_path).expect("Code manifest");

            let mut media = vec![0xff; 5 * 188];
            for packet in 0..5 {
                let offset = packet * 188;
                media[offset..offset + 4].copy_from_slice(&[0x47, 0x40, packet as u8, 0x10]);
            }
            std::fs::write(&segment, media).expect("publish MPEG generation");
            super::rebuild_isolated_pass(request(true)).expect("publish MPEG graph generation");

            let media_id =
                graphoxide_core::make_id(&["format_inventory", "mpeg_transport_stream", "segment"]);
            let mut seeded = graphoxide_core::read_graph(&graph_path).expect("read MPEG graph");
            assert!(seeded.nodes.iter().any(|node| node.id == media_id));
            seeded.nodes.extend([
                graphoxide_core::Node {
                    id: "stale_media_semantic".into(),
                    label: "stale media semantic".into(),
                    file_type: "concept".into(),
                    source_file: "segment.ts".into(),
                    source_location: None,
                    community: None,
                    extra: std::collections::BTreeMap::from([(
                        "_origin".into(),
                        serde_json::json!("semantic"),
                    )]),
                },
                graphoxide_core::Node {
                    id: "unrelated_semantic_overlay".into(),
                    label: "unrelated semantic overlay".into(),
                    file_type: "concept".into(),
                    source_file: "main.ts".into(),
                    source_location: None,
                    community: None,
                    extra: std::collections::BTreeMap::from([(
                        "_origin".into(),
                        serde_json::json!("semantic"),
                    )]),
                },
            ]);
            seeded.links.push(graphoxide_core::Edge {
                source: "unrelated_semantic_overlay".into(),
                target: media_id.clone(),
                relation: "semantic_dependency".into(),
                confidence: graphoxide_core::Confidence::Inferred,
                source_file: "main.ts".into(),
                extra: std::collections::BTreeMap::from([(
                    "_origin".into(),
                    serde_json::json!("semantic"),
                )]),
            });
            seeded.hyperedges.extend([
                serde_json::json!({
                    "id": "foreign_media_flow",
                    "nodes": ["unrelated_semantic_overlay", media_id],
                    "source_file": "main.ts",
                    "_origin": "semantic",
                }),
                serde_json::json!({
                    "id": "unrelated_overlay_flow",
                    "nodes": ["unrelated_semantic_overlay"],
                    "source_file": "main.ts",
                    "_origin": "semantic",
                }),
            ]);
            graphoxide_core::write_graph_atomic(&graph_path, &seeded, true)
                .expect("seed graph-ahead semantic facts");
            std::fs::write(&manifest_path, &stale_code_manifest)
                .expect("restore graph-behind Code manifest");
            std::fs::write(&segment, code).expect("restore byte-identical TypeScript");
            let graph_before = std::fs::read(&graph_path).expect("seeded graph bytes");
            let manifest_before = std::fs::read(&manifest_path).expect("stale manifest bytes");

            let error = match super::rebuild_isolated_pass(request(false)) {
                Ok(_) => panic!("normal reverse-ABA update must fail closed"),
                Err(error) => error,
            };
            assert!(
                format!("{error:#}").contains("committed graph disagrees"),
                "{error:#}"
            );
            assert_eq!(std::fs::read(&graph_path).unwrap(), graph_before);
            assert_eq!(std::fs::read(&manifest_path).unwrap(), manifest_before);

            super::rebuild_isolated_pass(request(true)).expect("forced update repairs reverse ABA");
            let repaired = graphoxide_core::read_graph(&graph_path).expect("read repaired graph");
            assert!(repaired
                .nodes
                .iter()
                .all(|node| { node.id != media_id && node.id != "stale_media_semantic" }));
            assert!(repaired.nodes.iter().any(|node| {
                node.source_file == "segment.ts"
                    && node.extra.get("type").and_then(serde_json::Value::as_str) == Some("file")
            }));
            assert!(repaired
                .nodes
                .iter()
                .any(|node| node.id == "unrelated_semantic_overlay"));
            assert!(repaired
                .links
                .iter()
                .all(|edge| { edge.true_source() != media_id && edge.true_target() != media_id }));
            assert!(repaired.hyperedges.iter().all(|hyperedge| {
                hyperedge["nodes"]
                    .as_array()
                    .is_none_or(|members| members.iter().all(|member| member != &media_id))
            }));
            assert!(repaired
                .hyperedges
                .iter()
                .any(|hyperedge| hyperedge["id"] == "unrelated_overlay_flow"));
            let manifest: serde_json::Value = serde_json::from_slice(
                &std::fs::read(&manifest_path).expect("read repaired manifest"),
            )
            .expect("decode repaired manifest");
            assert_eq!(manifest["segment.ts"]["source_kind"], "code");
        }
    }

    #[test]
    fn isolated_semantic_only_code_baseline_fails_closed_then_force_repairs_structural_facts() {
        for no_cluster in [false, true] {
            let project = tempfile::tempdir().expect("temporary project");
            std::fs::write(
                project.path().join("main.ts"),
                b"export const main = true;\n",
            )
            .expect("write main");
            std::fs::write(
                project.path().join("segment.ts"),
                b"export const phantom = 42;\n",
            )
            .expect("write segment");
            let output = project.path().join("graphoxide-out");
            let request = |force| super::IsolatedRebuildRequest {
                path: project.path(),
                output_directory: &output,
                marker_value: ".",
                no_cluster,
                force,
                scope: graphoxide_cli::watch::RebuildScope::Incremental,
                pass: 1,
                runtime_config: graphoxide_index_runtime::IndexRuntimeConfig::default(),
                collect_runtime_telemetry: false,
                progress_reporter: BuildProgressReporter::new_adaptive(
                    graphoxide_cli::build_telemetry::BuildOperation::Update,
                    BuildProgressMode::Never,
                )
                .expect("silent progress reporter"),
            };
            super::rebuild_isolated_pass(request(false)).expect("initial Code baseline");
            let graph_path = output.join("graph.json");
            let manifest_path = output.join("manifest.json");
            let mut baseline =
                graphoxide_core::read_graph(&graph_path).expect("read initial graph");
            let removed = baseline
                .nodes
                .iter()
                .filter(|node| node.source_file == "segment.ts")
                .map(|node| node.id.clone())
                .collect::<std::collections::BTreeSet<_>>();
            baseline
                .nodes
                .retain(|node| node.source_file != "segment.ts");
            baseline.links.retain(|edge| {
                edge.source_file != "segment.ts"
                    && !removed.contains(edge.true_source())
                    && !removed.contains(edge.true_target())
            });
            baseline.hyperedges.retain_mut(|hyperedge| {
                let Some(members) = hyperedge
                    .get_mut("nodes")
                    .and_then(serde_json::Value::as_array_mut)
                else {
                    return false;
                };
                members.retain(|member| {
                    member
                        .as_str()
                        .is_none_or(|member| !removed.contains(member))
                });
                !members.is_empty()
            });
            baseline.nodes.push(graphoxide_core::Node {
                id: "semantic_only_segment".into(),
                label: "semantic only segment".into(),
                file_type: "concept".into(),
                source_file: "segment.ts".into(),
                source_location: None,
                community: None,
                extra: std::collections::BTreeMap::from([(
                    "_origin".into(),
                    serde_json::json!("semantic"),
                )]),
            });
            graphoxide_core::write_graph_atomic(&graph_path, &baseline, true)
                .expect("write semantic-only graph");
            let graph_before = std::fs::read(&graph_path).expect("semantic-only graph bytes");
            let manifest_before = std::fs::read(&manifest_path).expect("Code manifest bytes");

            let error = match super::rebuild_isolated_pass(request(false)) {
                Ok(_) => panic!("unchanged Code without structural marker must fail closed"),
                Err(error) => error,
            };
            let diagnostic = format!("{error:#}");
            assert!(
                diagnostic.contains("verified structural rebuild or ownership reset")
                    && diagnostic.contains("rerun with --force"),
                "{diagnostic}"
            );
            assert_eq!(std::fs::read(&graph_path).unwrap(), graph_before);
            assert_eq!(std::fs::read(&manifest_path).unwrap(), manifest_before);

            super::rebuild_isolated_pass(request(true))
                .expect("force repairs missing structural representation");
            let repaired = graphoxide_core::read_graph(&graph_path).expect("read repaired graph");
            assert!(repaired
                .nodes
                .iter()
                .any(|node| node.id == "semantic_only_segment"));
            assert!(repaired.nodes.iter().any(|node| {
                node.source_file == "segment.ts"
                    && node.extra.get("type").and_then(serde_json::Value::as_str) == Some("file")
            }));
        }
    }

    #[test]
    fn baseline_representation_recheck_diagnostic_is_counted_sorted_and_bounded() {
        let project = tempfile::tempdir().expect("temporary representation project");
        let root = std::fs::canonicalize(project.path()).expect("canonical project root");
        for source in ["a.ts", "b.ts"] {
            std::fs::write(root.join(source), b"export const value = true;\n")
                .expect("write TypeScript source");
        }
        let detection = graphoxide_extract::detect::detect(
            &root,
            &graphoxide_extract::detect::DetectOptions::default(),
        )
        .expect("detect both TypeScript sources");
        let inventory = |source: &str| graphoxide_core::Node {
            id: format!("media_{source}"),
            label: source.into(),
            file_type: "document".into(),
            source_file: source.into(),
            source_location: None,
            community: None,
            extra: std::collections::BTreeMap::from([
                ("type".into(), serde_json::json!("format_inventory")),
                ("format".into(), serde_json::json!("mpeg_transport_stream")),
            ]),
        };
        let baseline = graphoxide_core::KnowledgeGraph {
            nodes: vec![inventory("a.ts"), inventory("b.ts")],
            ..Default::default()
        };
        let reset_candidates = [root.join("b.ts"), root.join("a.ts")];
        std::fs::remove_file(root.join("a.ts")).expect("remove first source after detection");
        std::fs::remove_file(root.join("b.ts")).expect("remove second source after detection");

        let error = super::gate_baseline_representation_resets(
            &baseline,
            &detection,
            &reset_candidates,
            &[],
            &[],
            &root,
        )
        .expect_err("both final generation rechecks must fail as one bounded diagnostic");
        let diagnostic = error.to_string();
        assert!(diagnostic.contains("for 2 source(s)"), "{diagnostic}");
        assert!(
            diagnostic.contains("first source: \"a.ts\""),
            "{diagnostic}"
        );
        assert!(!diagnostic.contains("b.ts"), "{diagnostic}");
    }

    #[test]
    fn incremental_baseline_charges_exact_admitted_generation_bytes() {
        let project = tempfile::tempdir().expect("temporary project");
        let graph_path = project.path().join("graph.json");
        let mut bytes =
            br#"{"directed":true,"multigraph":false,"graph":{},"nodes":[],"links":[]}"#.to_vec();
        bytes.extend(std::iter::repeat_n(b' ', 257));
        std::fs::write(&graph_path, &bytes).expect("write padded graph generation");
        let multiplier = graphoxide_graph::incremental::INCREMENTAL_GRAPH_WORKING_SET_MULTIPLIER;
        let pending_manifest_retained_bytes = 333;
        let expected_remaining = 997;
        let cache_and_runs_bytes =
            bytes.len() * multiplier + pending_manifest_retained_bytes + expected_remaining;

        let (graph, remaining) = read_incremental_baseline(
            &graph_path,
            cache_and_runs_bytes,
            IncrementalGraphBudget {
                max_baseline_file_bytes: u64::try_from(bytes.len())
                    .expect("fixture length fits in u64"),
                max_graph_materialized_bytes: cache_and_runs_bytes
                    - pending_manifest_retained_bytes,
            },
        )
        .expect("load baseline within exact accounting budget");

        assert!(graph.nodes.is_empty());
        assert_eq!(remaining, expected_remaining);
    }

    #[test]
    fn watch_rebuild_dispatcher_defaults_to_isolated_and_requires_legacy_opt_in() {
        let project = tempfile::tempdir().expect("temporary project");
        let source = project.path().join("app.py");
        std::fs::write(&source, "def app():\n    return 1\n").expect("write source");
        let output = project.path().join("graphoxide-out");
        let isolated_report = project.path().join("isolated-runtime.json");
        let options = graphoxide_cli::watch::RebuildOptions {
            changed_paths: Some(vec![source.clone()]),
            output_directory: Some(output.clone()),
            no_cluster: true,
            acquire_lock: true,
            block_on_lock: false,
            ..Default::default()
        };
        let isolated = super::rebuild_watch_project(
            project.path(),
            &options,
            Some(graphoxide_index_runtime::IndexRuntimeConfig::default()),
            Some(&isolated_report),
            ProgressModeArg::Never,
        )
        .expect("isolated watch dispatch");
        assert_eq!(
            isolated.status,
            graphoxide_cli::watch::RebuildStatus::Rebuilt
        );
        let isolated_value: serde_json::Value = serde_json::from_slice(
            &std::fs::read(&isolated_report).expect("isolated runtime report"),
        )
        .expect("parse isolated runtime report");
        assert_eq!(isolated_value["runtime"]["execution_model"], "isolated");

        std::fs::write(
            &source,
            "def app():\n    return 1\n\ndef legacy():\n    return app()\n",
        )
        .expect("update source");
        let legacy_report = project.path().join("legacy-runtime.json");
        let legacy = super::rebuild_watch_project(
            project.path(),
            &options,
            None,
            Some(&legacy_report),
            ProgressModeArg::Never,
        )
        .expect("legacy watch dispatch");
        assert!(legacy.succeeded());
        let legacy_value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&legacy_report).expect("legacy runtime report"))
                .expect("parse legacy runtime report");
        assert_eq!(legacy_value["runtime"]["execution_model"], "legacy");
    }

    #[test]
    fn formats_command_reports_the_registry_contract_without_path_probing() {
        for spelling in ["formats", "capabilities"] {
            let cli = Cli::try_parse_from(["graphoxide", spelling, "--json"])
                .unwrap_or_else(|error| panic!("parse {spelling} command: {error}"));
            assert!(matches!(cli.command, Command::Formats { json: true }));
        }

        let text = format_capability_output(false).expect("render text contract");
        assert!(text.contains("delimited-data\tstructural_partial"));
        assert!(text.contains("protobuf-binary\tinventory_only\trequired"));
        assert!(text.contains("source-code\tstructural_partial"));
        assert!(text.contains("package-manifest\tinventory_only"));
        assert!(text.contains("tar-archive\tstructural_partial"));
        assert!(text.contains("json5\tstructural_partial\tnot_required\tstructured\tjson5"));
        assert!(text.contains("json-lines\tsemantic_full\tnot_required\tstructured\tjsonl,ndjson"));
        assert!(
            text.contains("graphviz-dot\tsemantic_full\tnot_required\tdiagram\tdot,gv,graphviz")
        );
        assert!(text.contains("yaml\tstructural_partial\tnot_required\tstructured\tyaml,yml"));

        let json = format_capability_output(true).expect("render JSON contract");
        let reports: Vec<serde_json::Value> =
            serde_json::from_str(&json).expect("deserialize format contract");
        assert_eq!(
            reports.len(),
            graphoxide_extract::format_registry::format_registry()
                .specs()
                .len()
        );
        assert!(reports.iter().any(|report| {
            report["id"] == "openusd-ascii" && report["capability"] == "structural_partial"
        }));
        for id in ["json5", "yaml", "named-yaml-configuration"] {
            let report = reports
                .iter()
                .find(|report| report["id"] == id)
                .unwrap_or_else(|| panic!("missing {id} capability report"));
            assert_eq!(report["capability"], "structural_partial", "{id}");
            assert_eq!(
                report["limits"]["max_input_bytes"],
                16 * 1024 * 1024,
                "{id}"
            );
            assert_eq!(report["limits"]["max_nesting"], 32, "{id}");
            assert_eq!(report["limits"]["max_records"], 4_096, "{id}");
        }
        for extension in ["jsonl", "ndjson"] {
            assert!(reports.iter().any(|report| {
                report["id"] == "json-lines"
                    && report["capability"] == "semantic_full"
                    && report["extensions"]
                        .as_array()
                        .is_some_and(|extensions| extensions.iter().any(|value| value == extension))
            }));
        }
        assert!(reports.iter().any(|report| {
            report["id"] == "graphviz-dot"
                && report["capability"] == "semantic_full"
                && report["limits"]["max_input_bytes"] == 8 * 1024 * 1024
                && report["limits"]["max_nesting"] == 64
                && report["limits"]["max_records"] == 350_000
        }));
    }

    #[test]
    fn audit_accepts_json_strict_and_cache_bypass() {
        let cli =
            Cli::try_parse_from(["graphoxide", "audit", ".", "--json", "--strict", "--force"])
                .expect("parse audit flags");
        assert!(matches!(
            cli.command,
            Command::Audit {
                json: true,
                strict: true,
                force: true,
                ..
            }
        ));
    }

    #[test]
    fn audit_coverage_accepts_an_optional_path_and_flags_in_either_order() {
        for arguments in [
            vec![
                "graphoxide",
                "audit",
                "coverage",
                "workspace",
                "--json",
                "--strict",
            ],
            vec![
                "graphoxide",
                "audit",
                "coverage",
                "--strict",
                "--json",
                "workspace",
            ],
            vec![
                "graphoxide",
                "audit",
                "--json",
                "coverage",
                "workspace",
                "--strict",
            ],
        ] {
            let cli = Cli::try_parse_from(arguments).expect("parse coverage audit flags");
            assert!(matches!(
                cli.command,
                Command::Audit {
                    path,
                    coverage_path: Some(coverage_path),
                    json: true,
                    strict: true,
                    force: false,
                } if path == Path::new("coverage") && coverage_path == Path::new("workspace")
            ));
        }

        let cli = Cli::try_parse_from(["graphoxide", "audit", "coverage", "--json"])
            .expect("parse coverage audit with default path");
        assert!(matches!(
            cli.command,
            Command::Audit {
                path,
                coverage_path: None,
                json: true,
                ..
            } if path == Path::new("coverage")
        ));
    }

    #[test]
    fn audit_dot_coverage_remains_a_legacy_graph_audit_path() {
        let cli = Cli::try_parse_from(["graphoxide", "audit", "./coverage", "--json"])
            .expect("parse literal coverage directory");
        assert!(matches!(
            cli.command,
            Command::Audit {
                path,
                coverage_path: None,
                json: true,
                ..
            } if path.as_os_str() == std::ffi::OsStr::new("./coverage")
        ));
    }

    fn assert_pathless_postgres(arguments: &[&str], expected_no_cluster: bool) {
        let cli = Cli::try_parse_from(arguments).expect("parse pathless PostgreSQL extract");
        match cli.command {
            Command::Extract {
                build:
                    ProjectBuildOptions {
                        path,
                        postgres,
                        no_cluster,
                        ..
                    },
                ..
            } => {
                assert_eq!(path, PathBuf::from("."));
                assert_eq!(postgres.as_deref(), Some("test-dsn"));
                assert_eq!(no_cluster, expected_no_cluster);
            }
            _ => panic!("expected extract command"),
        }
    }

    #[test]
    fn test_pathless_postgres_extract_initializes_empty_detection_clustered_space() {
        assert_pathless_postgres(&["graphoxide", "extract", "--postgres", "test-dsn"], false);
    }

    #[test]
    fn test_pathless_postgres_extract_initializes_empty_detection_clustered_equals() {
        assert_pathless_postgres(&["graphoxide", "extract", "--postgres=test-dsn"], false);
    }

    #[test]
    fn test_pathless_postgres_extract_initializes_empty_detection_no_cluster_space() {
        assert_pathless_postgres(
            &[
                "graphoxide",
                "extract",
                "--postgres",
                "test-dsn",
                "--no-cluster",
            ],
            true,
        );
    }

    #[test]
    fn test_pathless_postgres_extract_initializes_empty_detection_no_cluster_equals() {
        assert_pathless_postgres(
            &[
                "graphoxide",
                "extract",
                "--postgres=test-dsn",
                "--no-cluster",
            ],
            true,
        );
    }

    #[test]
    fn audit_accounts_for_unresolved_call_facts() {
        let extraction: graphoxide_core::Extraction = serde_json::from_value(serde_json::json!({
            "nodes": [{
                "id": "caller",
                "label": "caller()",
                "file_type": "code",
                "source_file": "a.js",
                "source_location": "L1",
                "type": "function"
            }],
            "edges": [{
                "source": "caller",
                "target": "__graphoxide_call_missing",
                "relation": "calls",
                "confidence": "INFERRED",
                "source_file": "a.js",
                "unresolved_call": true,
                "callee": "missing",
                "member_call": false
            }]
        }))
        .expect("audit fixture");
        let (_, build) =
            graphoxide_graph::build_graph_with_report(std::slice::from_ref(&extraction))
                .expect("build audit fixture");

        let report = audit_report(Path::new("."), &[extraction], build);

        assert_eq!(report.input.unresolved_calls, 1);
        assert!(report.strict_violations > 0);
        assert!(report
            .findings
            .iter()
            .any(|finding| finding.code == "unresolved_call"));
        assert_eq!(report.build.dropped_edge_count(), 1);
        assert!(report.build.edges_accounted_for());
    }

    #[test]
    fn query_cli_accepts_repeated_explicit_contexts() {
        let cli = Cli::try_parse_from([
            "graphoxide",
            "query",
            "extract",
            "--context",
            "call",
            "--context",
            "import",
        ])
        .expect("parse query contexts");
        assert!(matches!(
            cli.command,
            Command::Query { contexts, .. } if contexts == ["call", "import"]
        ));
    }

    #[test]
    fn query_cli_context_annotation_matches_upstream_contract() {
        let mut output = "Traversal: BFS depth=2\n\nNODE extract".to_owned();
        annotate_query_context(&mut output, &["call".into()], "explicit");
        assert!(output.contains("Context: call (explicit)"));
        assert!(output.contains("NODE extract"));
    }

    #[test]
    fn test_god_nodes_cli_text_output() {
        let nodes = graphoxide_query::god_nodes(&god_test_graph(), 10);
        let output = format_god_nodes(&nodes, false).expect("text god-node output");
        assert!(output.contains("God nodes (most connected):"));
        assert!(output.contains("Auth"));
        assert!(output.contains("edges"));
        assert!(!output.contains("auth.py"));
    }

    #[test]
    fn test_god_nodes_cli_underscore_alias() {
        let cli =
            Cli::try_parse_from(["graphoxide", "god_nodes"]).expect("underscore god-nodes alias");
        assert!(matches!(cli.command, Command::GodNodes { .. }));
    }

    #[test]
    fn test_god_nodes_cli_top_limits() {
        let graph = god_test_graph();
        let nodes = graphoxide_query::god_nodes(&graph, 1);
        assert_eq!(nodes.len(), 1);
        assert_eq!(
            format_god_nodes(&nodes, false)
                .unwrap()
                .matches(" edges")
                .count(),
            1
        );
    }

    #[test]
    fn test_god_nodes_cli_json() {
        let output = format_god_nodes(&[("hub".into(), "Auth".into(), 5)], true)
            .expect("JSON god-node output");
        let value: serde_json::Value = serde_json::from_str(&output).expect("valid JSON");
        assert_eq!(value[0]["id"], "hub");
        assert_eq!(value[0]["label"], "Auth");
        assert_eq!(value[0]["degree"], 5);
    }

    #[test]
    fn test_god_nodes_cli_missing_graph_errors() {
        let missing = std::env::temp_dir().join(format!(
            "graphoxide-missing-god-nodes-{}-graph.json",
            std::process::id()
        ));
        let error = graphoxide_core::read_graph(&missing).expect_err("missing graph must fail");
        assert!(error.to_string().to_lowercase().contains("not found"));
    }

    #[test]
    fn learning_overlay_marks_a_vanished_source_stale() {
        let directory = std::env::temp_dir().join(format!(
            "graphoxide-learning-overlay-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).expect("create overlay fixture directory");
        let graph = directory.join("graph.json");
        std::fs::write(&graph, "{}").expect("write graph marker");
        std::fs::write(
            directory.join(".graphify_learning.json"),
            r#"{"nodes":{"validate":{"source_file":"missing.py","code_fingerprint":"deadbeef"}}}"#,
        )
        .expect("write overlay");
        let overlay = load_learning_overlay(&graph).expect("load overlay");
        assert_eq!(overlay["validate"]["stale"], true);
        std::fs::remove_file(directory.join(".graphify_learning.json")).expect("remove overlay");
        std::fs::remove_file(graph).expect("remove graph marker");
        std::fs::remove_dir(directory).expect("remove overlay fixture directory");
    }

    #[test]
    fn serve_cli_defaults_to_stdio_and_the_managed_graph() {
        let cli = Cli::try_parse_from(["graphoxide", "serve"]).expect("parse serve defaults");
        assert!(matches!(
            cli.command,
            Command::Serve {
                graph,
                transport,
                host,
                port: 8080,
                ..
            } if graph == std::path::Path::new("graphoxide-out/graph.json")
                && transport == "stdio"
                && host == "127.0.0.1".parse::<std::net::IpAddr>().unwrap()
        ));
    }

    #[test]
    fn serve_cli_exposes_streamable_http_controls() {
        let cli = Cli::try_parse_from([
            "graphoxide",
            "serve",
            "g.json",
            "--transport",
            "http",
            "--host",
            "0.0.0.0",
            "--port",
            "9000",
            "--api-key",
            "secret",
            "--path",
            "/graph",
            "--json-response",
            "--stateless",
        ])
        .expect("parse HTTP serve flags");
        assert!(matches!(
            cli.command,
            Command::Serve {
                graph,
                transport,
                port: 9000,
                api_key: Some(key),
                mount_path,
                json_response: true,
                stateless: true,
                ..
            } if graph == std::path::Path::new("g.json")
                && transport == "http"
                && key == "secret"
                && mount_path == "/graph"
        ));
    }
}
