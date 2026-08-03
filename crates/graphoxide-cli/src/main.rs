//! Graphoxide CLI — a fast, dependency-free Rust code-graph tool.
//!
//! The command surface includes extraction, analysis, query, export, integrations,
//! and an MCP stdio server.

mod site;

use anyhow::Context;
use clap::{Parser, Subcommand};
use graphoxide_cli::watch as watch_service;
use std::{fs, io::Write, path::PathBuf};

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

#[derive(Subcommand)]
enum Command {
    /// Headless deterministic extraction into graphoxide-out/
    Extract {
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
        /// Place the managed graphoxide-out directory beneath this root.
        #[arg(long, visible_alias = "output")]
        out: Option<PathBuf>,
        /// Exclude a path or ignore-style pattern; repeated flags replace the persisted set.
        #[arg(long)]
        exclude: Vec<String>,
        /// Ignore VCS ignore files while continuing to honor .graphoxideignore/.graphifyignore.
        #[arg(long)]
        no_gitignore: bool,
    },
    /// Audit extraction and graph construction for silent loss or malformed facts
    Audit {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Emit a machine-readable JSON report
        #[arg(long)]
        json: bool,
        /// Exit unsuccessfully when extraction or graph construction loses facts
        #[arg(long)]
        strict: bool,
        /// Bypass the incremental AST cache
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
    Watch { path: PathBuf },
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
    HookRebuild { mode: String, root: PathBuf },
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
    for source in graph
        .nodes
        .iter()
        .map(|node| node.source_file.as_str())
        .filter(|source| !source.is_empty())
    {
        if source.contains("://") || source.contains(":/") || source.contains(":\\") {
            continue;
        }
        let source_path = std::path::Path::new(source);
        let candidate = if source_path.is_absolute() {
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
        let normalized = fs::canonicalize(&candidate).unwrap_or_else(|_| candidate.clone());
        if !live.contains(&normalized) {
            stale.insert(candidate);
        }
    }
    stale.into_iter().collect()
}

fn emit_extract_timing(enabled: bool, stage: &str, started: std::time::Instant) {
    if enabled {
        eprintln!(
            "[graphoxide timing] {stage}: {:.3}s",
            started.elapsed().as_secs_f64()
        );
    }
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();
    match cli.command {
        Command::Extract {
            path,
            code_only,
            no_cluster,
            force,
            postgres,
            allow_partial,
            timing,
            out,
            exclude,
            no_gitignore,
        } => {
            let total_started = std::time::Instant::now();
            let effective_force = graphoxide_cli::extract_cli::force_enabled(
                force,
                std::env::var("GRAPHOXIDE_FORCE").ok().as_deref(),
                std::env::var("GRAPHIFY_FORCE").ok().as_deref(),
            );
            let output_directory = managed_output_directory(&path, out.as_deref());
            let output = output_directory.join("graph.json");
            let manifest_path = output_directory.join("manifest.json");
            // A committed graph is a sufficient carry-forward baseline for an
            // explicitly code-only rebuild. This preserves live semantic
            // records when a fresh clone does not contain the manifest.
            let incremental_mode =
                !effective_force && output.is_file() && (manifest_path.is_file() || code_only);
            if incremental_mode {
                write_output("Incremental scan: reusing unchanged extraction cache entries.")?;
            }
            let persisted = watch_service::read_build_config(&output_directory);
            let effective_excludes = if exclude.is_empty() {
                persisted.excludes.clone()
            } else {
                exclude.clone()
            };
            let honor_gitignore = !no_gitignore && persisted.honor_gitignore;
            let scan_started = std::time::Instant::now();
            let scan = graphoxide_extract::extract_project_with_scan_options_deferred_manifest(
                &path,
                effective_force,
                &output_directory,
                code_only,
                &graphoxide_extract::detect::DetectOptions {
                    extra_excludes: effective_excludes,
                    output_dir: Some(output_directory.clone()),
                    honor_gitignore,
                    ..Default::default()
                },
            )?;
            emit_extract_timing(timing, "detect/extract", scan_started);
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
                write_output(&format!(
                    "--code-only: skipping {skipped} non-code input(s)"
                ))?;
            }
            for skipped in &scan.detection.skipped_sensitive {
                eprintln!("skipped as potentially sensitive: {skipped}");
            }
            let mut extractions = scan.extractions;
            let pending_manifest = scan.pending_manifest;
            if let Some(dsn) = postgres.as_deref() {
                extractions.push(graphoxide_extract::pg_introspect::introspect_postgres(
                    (!dsn.is_empty()).then_some(dsn),
                )?);
            }
            let previous = graphoxide_core::read_graph(&output).ok();
            let live_sources = scan
                .detection
                .files
                .values()
                .flatten()
                .map(PathBuf::from)
                .collect::<Vec<_>>();
            let prune_sources = previous
                .as_ref()
                .map(|graph| stale_local_sources(graph, &path, &live_sources))
                .unwrap_or_default();
            let build_started = std::time::Instant::now();
            if no_cluster {
                graphoxide_graph::disambiguate_file_labels_in_extractions(&mut extractions);
                if incremental_mode {
                    let fresh = flatten_extractions(extractions);
                    extractions = vec![graphoxide_graph::merge_raw_extraction(
                        &fresh,
                        &output,
                        &prune_sources,
                        Some(&path),
                    )?];
                }
                extractions = vec![graphoxide_graph::dedupe_raw_extractions(&extractions)];
                emit_extract_timing(timing, "build", build_started);
                let outcome = graphoxide_cli::build_guard::commit_build(
                    &output,
                    graphoxide_cli::build_guard::BuildArtifact::Raw(&extractions),
                    build_progress,
                    allow_partial,
                    || pending_manifest.commit(),
                )?;
                if outcome == graphoxide_cli::build_guard::BuildCommitOutcome::RefusedShrink {
                    anyhow::bail!("{outcome}");
                }
                let nodes: usize = extractions.iter().map(|e| e.nodes.len()).sum();
                let edges: usize = extractions.iter().map(|e| e.edges.len()).sum();
                save_build_config_in(
                    &output_directory,
                    true,
                    (!exclude.is_empty()).then_some(exclude.as_slice()),
                    no_gitignore.then_some(false),
                )?;
                write_output(&format!(
                    "Wrote {nodes} nodes and {edges} edges to {}",
                    output.display()
                ))?;
                emit_extract_timing(timing, "total", total_started);
                return Ok(());
            }
            let mut graph = if incremental_mode {
                graphoxide_graph::build_merge(&extractions, &output, &prune_sources, Some(&path))?
            } else {
                graphoxide_graph::build_graph(&extractions)?
            };
            graphoxide_graph::cluster(&mut graph)?;
            if let Some(previous) = &previous {
                graphoxide_graph::cluster::remap_communities_to_previous(&mut graph, previous);
            }
            emit_extract_timing(timing, "build", build_started);
            let outcome = graphoxide_cli::build_guard::commit_build(
                &output,
                graphoxide_cli::build_guard::BuildArtifact::Graph(&graph),
                build_progress,
                allow_partial,
                || pending_manifest.commit(),
            )?;
            if outcome == graphoxide_cli::build_guard::BuildCommitOutcome::RefusedShrink {
                anyhow::bail!("{outcome}");
            }
            save_build_config_in(
                &output_directory,
                false,
                (!exclude.is_empty()).then_some(exclude.as_slice()),
                no_gitignore.then_some(false),
            )?;
            write_output(&format!(
                "Wrote {} nodes and {} edges to {}",
                graph.nodes.len(),
                graph.links.len(),
                output.display()
            ))?;
            emit_extract_timing(timing, "total", total_started);
            Ok(())
        }
        Command::Audit {
            path,
            json,
            strict,
            force,
        } => run_audit(&path, json, strict, force),
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
        } => rebuild(&path, no_cluster, force),
        Command::ClusterOnly {
            path,
            graph: graph_override,
            no_viz: _,
            no_label,
        } => {
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
            graphoxide_graph::cluster(&mut graph)?;
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
        } => label_communities(
            &path,
            backend.as_deref(),
            model.as_deref(),
            missing_only,
            max_concurrency,
            batch_size,
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
        Command::Watch { path } => watch(path),
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
        Command::HookRebuild { mode, root } => graphoxide_cli::hooks::rebuild(mode.parse()?, &root),
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

fn run_audit(path: &std::path::Path, json: bool, strict: bool, force: bool) -> anyhow::Result<()> {
    let extractions = graphoxide_extract::extract_project_with_options(path, force)?;
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

fn watch(path: PathBuf) -> anyhow::Result<()> {
    use notify::{RecursiveMode, Watcher};
    let filter = watch_service::WatchEventFilter::new(
        &path,
        watch_service::read_build_config(&path.join(watch_service::OUTPUT_DIRECTORY))
            .honor_gitignore,
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
        let code_changes = changed
            .iter()
            .filter(|changed| {
                graphoxide_extract::detect::classify_file(changed)
                    == Some(graphoxide_extract::detect::FileType::Code)
            })
            .cloned()
            .collect::<Vec<_>>();
        if !code_changes.is_empty() {
            let options = watch_service::RebuildOptions {
                changed_paths: Some(code_changes),
                acquire_lock: true,
                block_on_lock: false,
                ..Default::default()
            };
            match watch_service::rebuild_project(&path, &options) {
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
        if changed.iter().any(|changed| {
            graphoxide_extract::detect::classify_file(changed)
                != Some(graphoxide_extract::detect::FileType::Code)
        }) {
            watch_service::notify_only(&path)?;
        }
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
    let notice = watch_service::check_update(path);
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
) -> anyhow::Result<()> {
    let graph_path = if path.is_dir() {
        managed_output_directory(path, None).join("graph.json")
    } else {
        path.to_path_buf()
    };
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
    let transport = LabelHttpTransport::new(&backend, model)?;
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
}

impl LabelHttpTransport {
    fn new(backend: &str, requested_model: Option<&str>) -> anyhow::Result<Self> {
        let backend = match backend {
            "anthropic" => "claude",
            backend => backend,
        };
        let (base_key, default_base, key_names, model_key, default_model, anthropic) = match backend
        {
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
        let base = std::env::var("GRAPHOXIDE_LLM_BASE_URL")
            .ok()
            .or_else(|| std::env::var(base_key).ok())
            .unwrap_or_else(|| default_base.into());
        let suffix = if anthropic {
            "messages"
        } else {
            "chat/completions"
        };
        let endpoint = if base.trim_end_matches('/').ends_with(suffix) {
            base.trim_end_matches('/').to_owned()
        } else {
            format!("{}/{suffix}", base.trim_end_matches('/'))
        };
        let key = key_names
            .iter()
            .find_map(|name| std::env::var(name).ok().filter(|key| !key.is_empty()));
        let parsed = reqwest::Url::parse(&endpoint)?;
        let loopback = parsed.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        });
        if key.is_none() && !loopback {
            anyhow::bail!(
                "none of {} is set for backend {backend:?}",
                key_names.join(", ")
            )
        }
        let model = requested_model
            .map(str::to_owned)
            .or_else(|| std::env::var(model_key).ok())
            .or_else(|| std::env::var("GRAPHOXIDE_MODEL").ok())
            .unwrap_or_else(|| default_model.into());
        Ok(Self {
            client: reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()?,
            endpoint,
            model,
            key,
            anthropic,
        })
    }

    fn call(
        &self,
        request: &graphoxide_graph::LabelRequest,
    ) -> anyhow::Result<graphoxide_graph::LabelResponse> {
        let model = request.model.as_deref().unwrap_or(&self.model);
        let body = serde_json::json!({
            "model": model,
            "max_tokens": request.max_tokens,
            "temperature": 0,
            "messages": [{"role": "user", "content": request.prompt}],
        });
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
            .send()?
            .error_for_status()?
            .json::<serde_json::Value>()?;
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

fn rebuild(path: &std::path::Path, no_cluster: bool, force: bool) -> anyhow::Result<()> {
    let result = watch_service::rebuild_project(
        path,
        &watch_service::RebuildOptions {
            force,
            no_cluster,
            acquire_lock: true,
            block_on_lock: true,
            ..Default::default()
        },
    )?;
    for warning in &result.warnings {
        eprintln!("[graphoxide update] {warning}");
    }
    match result.status {
        watch_service::RebuildStatus::Rebuilt => {
            let graph = graphoxide_core::read_graph(&result.graph_path)?;
            write_output(&format!(
                "Wrote {} nodes and {} edges to {}",
                graph.nodes.len(),
                graph.links.len(),
                result.graph_path.display()
            ))
        }
        watch_service::RebuildStatus::Unchanged => {
            write_output("No code-graph topology changes detected; outputs left untouched.")
        }
        watch_service::RebuildStatus::NoTrackedChanges => {
            write_output("No tracked code files in change set; nothing to rebuild.")
        }
        watch_service::RebuildStatus::Queued => {
            write_output("A rebuild is already running; changes were queued.")
        }
        watch_service::RebuildStatus::RefusedShrink => anyhow::bail!(
            "refusing to overwrite a smaller graph because the loss is not explained by rebuilt or deleted sources; pass --force after verifying the reduction"
        ),
    }
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
    graphoxide_graph::cluster(&mut graph)?;
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
    let destination = output.unwrap_or(ours);
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
        annotate_query_context, audit_report, format_god_nodes, load_learning_overlay,
        relevant_watch_paths, Cli, Command,
    };
    use clap::Parser;
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

    fn assert_pathless_postgres(arguments: &[&str], expected_no_cluster: bool) {
        let cli = Cli::try_parse_from(arguments).expect("parse pathless PostgreSQL extract");
        match cli.command {
            Command::Extract {
                path,
                postgres,
                no_cluster,
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
