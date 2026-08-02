//! Graphoxide CLI — a fast, dependency-free Rust code-graph tool.
//!
//! The command surface includes extraction, analysis, query, export, integrations,
//! and an MCP stdio server.

mod site;

use clap::{Parser, Subcommand};
use std::{io::Write, path::PathBuf};

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
    /// Headless extraction: AST parse a folder and write graphoxide-out/
    Extract {
        path: PathBuf,
        /// Offline AST-only extraction (the Rust core's default)
        #[arg(long)]
        code_only: bool,
        /// Skip clustering, write raw extraction only
        #[arg(long)]
        no_cluster: bool,
        /// Full re-scan: skip the incremental manifest gate
        #[arg(long)]
        force: bool,
    },
    /// Re-extract code files and update the graph (no LLM needed)
    Update {
        #[arg(default_value = ".")]
        path: PathBuf,
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
    GodNodes {
        #[arg(long, default_value_t = 10)]
        top: usize,
        #[arg(long)]
        json: bool,
        #[arg(long, default_value = "graphoxide-out/graph.json")]
        graph: PathBuf,
    },
    /// Rerun clustering on an existing graph.json
    ClusterOnly { path: PathBuf },
    /// Name graph communities through an OpenAI- or Anthropic-compatible HTTP endpoint
    Label {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        missing_only: bool,
    },
    /// Generate GRAPH_REPORT.md from an existing graph
    Report {
        #[arg(long, default_value = "graphoxide-out/graph.json")]
        graph: PathBuf,
        #[arg(long, default_value = "graphoxide-out/GRAPH_REPORT.md")]
        output: PathBuf,
    },
    /// Export an existing graph as html, callflow-html, graphml, cypher, obsidian, or json
    Export {
        #[arg(value_parser = ["html", "callflow-html", "graphml", "cypher", "obsidian", "json"])]
        format: String,
        output: PathBuf,
        #[arg(long, default_value = "graphoxide-out/graph.json")]
        graph: PathBuf,
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
        #[arg(long)]
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
    /// Claude Code PreToolUse hook that nudges graph queries
    HookGuard,
    /// Start the MCP stdio server
    Serve,
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

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();
    match cli.command {
        Command::Extract {
            path,
            code_only: _,
            no_cluster,
            force,
        } => {
            let extractions = graphoxide_extract::extract_project_with_options(&path, force)?;
            let output = path.join("graphoxide-out/graph.json");
            let previous = graphoxide_core::read_graph(&output).ok();
            if no_cluster {
                if !graphoxide_core::write_raw_extractions_atomic(&output, &extractions, force)? {
                    anyhow::bail!("refusing to overwrite a larger existing graph; pass --force after verifying the reduction");
                }
                let nodes: usize = extractions.iter().map(|e| e.nodes.len()).sum();
                let edges: usize = extractions.iter().map(|e| e.edges.len()).sum();
                save_build_config(&path, true)?;
                return write_output(&format!(
                    "Wrote {nodes} nodes and {edges} edges to {}",
                    output.display()
                ));
            }
            let mut graph = graphoxide_graph::build_graph(&extractions)?;
            graphoxide_graph::cluster(&mut graph)?;
            if let Some(previous) = &previous {
                graphoxide_graph::cluster::remap_communities_to_previous(&mut graph, previous);
            }
            if !graphoxide_core::write_graph_atomic(&output, &graph, force)? {
                anyhow::bail!("refusing to overwrite a larger existing graph; pass --force after verifying the reduction");
            }
            save_build_config(&path, false)?;
            write_output(&format!(
                "Wrote {} nodes and {} edges to {}",
                graph.nodes.len(),
                graph.links.len(),
                output.display()
            ))
        }
        Command::Query {
            question,
            budget,
            graph,
            dfs,
        } => {
            let started = std::time::Instant::now();
            let graph_data = graphoxide_core::read_graph(&graph)?;
            let result = if dfs {
                graphoxide_query::query_graph_dfs(&graph_data, &question, 2, budget)
            } else {
                graphoxide_query::query_graph(&graph_data, &question, 2, budget)
            };
            record_query("query", &question, &graph, &result, started.elapsed());
            write_output(&result)
        }
        Command::Path { a, b, graph } => {
            let started = std::time::Instant::now();
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
            let graph_data = graphoxide_core::read_graph(&graph)?;
            let result = graphoxide_query::explain(&graph_data, &node);
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
            let output = if json {
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
                serde_json::to_string_pretty(&values)?
            } else {
                let mut lines = vec!["God nodes (most connected):".to_owned()];
                lines.extend(nodes.iter().enumerate().map(|(i, (_, label, degree))| {
                    format!(
                        "  {}. {} - {} edges",
                        i + 1,
                        graphoxide_core::sanitize_label(label),
                        degree
                    )
                }));
                lines.join("\n")
            };
            write_output(&output)
        }
        Command::Update { path } => rebuild(&path, false, false),
        Command::ClusterOnly { path } => {
            let graph_path = if path.is_dir() {
                path.join("graphoxide-out/graph.json")
            } else {
                path
            };
            let mut graph = graphoxide_core::read_graph(&graph_path)?;
            let previous = graph.clone();
            graphoxide_graph::cluster(&mut graph)?;
            graphoxide_graph::cluster::remap_communities_to_previous(&mut graph, &previous);
            graphoxide_core::write_graph_atomic(&graph_path, &graph, true)?;
            write_output(&format!("Reclustered {} nodes", graph.nodes.len()))
        }
        Command::Label {
            path,
            model,
            missing_only,
        } => label_communities(&path, model.as_deref(), missing_only),
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
            output,
            graph,
        } => {
            let graph = graphoxide_core::read_graph(graph)?;
            match format.as_str() {
                "html" => write_text(&output, &graphoxide_export::render_html(&graph)?)?,
                "callflow-html" => {
                    write_text(&output, &graphoxide_export::render_callflow_html(&graph)?)?
                }
                "graphml" => write_text(&output, &graphoxide_export::render_graphml(&graph))?,
                "cypher" => write_text(&output, &graphoxide_export::render_cypher(&graph))?,
                "obsidian" => graphoxide_export::export_vault(&graph, &output)?,
                "json" => {
                    graphoxide_core::write_graph_atomic(&output, &graph, true)?;
                }
                _ => unreachable!(),
            }
            write_output(&format!("Wrote {}", output.display()))
        }
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
            let mut extractions = Vec::new();
            for input in inputs {
                let graph = graphoxide_core::read_graph(input)?;
                extractions.push(graphoxide_core::Extraction {
                    nodes: graph.nodes,
                    edges: graph.links,
                    hyperedges: graph.hyperedges,
                });
            }
            let graph = graphoxide_graph::build_graph(&extractions)?;
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
        Command::Hook { command } => hook(command),
        Command::Claude { command } => claude(command),
        Command::HookGuard => hook_guard(),
        Command::Serve => graphoxide_mcp::serve(),
        Command::Site { path, port } => site::serve(&path, port),
    }
}

fn watch(path: PathBuf) -> anyhow::Result<()> {
    use notify::{RecursiveMode, Watcher};
    let (sender, receiver) = std::sync::mpsc::channel();
    let mut watcher = notify::recommended_watcher(sender)?;
    watcher.watch(&path, RecursiveMode::Recursive)?;
    write_output(&format!("Watching {}", path.display()))?;
    loop {
        let Ok(first) = receiver.recv()? else {
            continue;
        };
        let mut changed = relevant_watch_paths(&path, first.paths);
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
                    let paths = relevant_watch_paths(&path, event.paths);
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
        if changed
            .iter()
            .any(|changed_path| graphoxide_extract::detect::is_supported_path(changed_path))
        {
            if let Err(error) = rebuild(&path, false, true) {
                eprintln!("[graphoxide] rebuild failed: {error}");
            }
        } else {
            let flag = path.join("graphoxide-out/needs_update");
            if let Some(parent) = flag.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(flag, b"")?;
        }
    }
}

fn relevant_watch_paths(root: &std::path::Path, paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths
        .into_iter()
        .filter(|changed_path| {
            let relative = changed_path.strip_prefix(root).unwrap_or(changed_path);
            !relative.components().any(|component| {
                let component = component.as_os_str();
                component == "graphoxide-out" || component == ".git"
            })
        })
        .collect()
}

fn check_update(path: &std::path::Path) -> anyhow::Result<()> {
    if path.join("graphoxide-out/needs_update").is_file() {
        write_output(&format!(
            "[graphoxide check-update] Pending non-code changes in {}.\n[graphoxide check-update] Run `graphoxide update {}` to rebuild the offline graph.",
            path.display(),
            path.display()
        ))
    } else {
        Ok(())
    }
}

fn label_communities(
    path: &std::path::Path,
    model: Option<&str>,
    missing_only: bool,
) -> anyhow::Result<()> {
    let graph_path = if path.is_dir() {
        path.join("graphoxide-out/graph.json")
    } else {
        path.to_path_buf()
    };
    let mut graph = graphoxide_core::read_graph(&graph_path)?;
    let mut degree = std::collections::BTreeMap::<String, usize>::new();
    for edge in &graph.links {
        *degree.entry(edge.true_source().to_owned()).or_default() += 1;
        *degree.entry(edge.true_target().to_owned()).or_default() += 1;
    }
    let mut communities = std::collections::BTreeMap::<i64, Vec<&graphoxide_core::Node>>::new();
    for node in &graph.nodes {
        if let Some(community) = node.community {
            communities.entry(community).or_default().push(node);
        }
    }
    if missing_only {
        communities.retain(|community, nodes| {
            !nodes.iter().any(|node| {
                node.extra
                    .get("community_name")
                    .and_then(|value| value.as_str())
                    .is_some_and(|name| {
                        !name.is_empty() && name != format!("Community {community}")
                    })
            })
        });
    }
    if communities.is_empty() {
        return write_output("No communities need labels.");
    }
    let mut rows = Vec::new();
    for (community, nodes) in &mut communities {
        nodes.sort_by(|a, b| {
            degree
                .get(&b.id)
                .cmp(&degree.get(&a.id))
                .then_with(|| a.id.cmp(&b.id))
        });
        let labels = nodes
            .iter()
            .take(12)
            .map(|node| graphoxide_core::sanitize_label(&node.label))
            .collect::<Vec<_>>()
            .join(", ");
        rows.push(format!("{community}: {labels}"));
    }
    let prompt = format!(
        "Name each software-architecture community below with a concise 2-5 word noun phrase. Return only a JSON object mapping the numeric community id to its label.\n{}",
        rows.join("\n")
    );
    let graphoxide_base = std::env::var("GRAPHOXIDE_LLM_BASE_URL").ok();
    let openai_base = std::env::var("OPENAI_BASE_URL").ok();
    let anthropic_base = std::env::var("ANTHROPIC_BASE_URL").ok();
    let openai_key = std::env::var("OPENAI_API_KEY").ok();
    let anthropic_key = std::env::var("ANTHROPIC_API_KEY").ok();
    let provider = std::env::var("GRAPHOXIDE_LLM_PROVIDER")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let use_anthropic = provider == "anthropic"
        || (provider != "openai"
            && graphoxide_base.is_none()
            && openai_base.is_none()
            && (anthropic_base.is_some() || (openai_key.is_none() && anthropic_key.is_some())));
    let mut base = if use_anthropic {
        graphoxide_base
            .or(anthropic_base)
            .unwrap_or_else(|| "https://api.anthropic.com/v1".into())
    } else {
        graphoxide_base
            .or(openai_base)
            .unwrap_or_else(|| "https://api.openai.com/v1".into())
    };
    let endpoint = if use_anthropic {
        if !base.ends_with("/messages") {
            base = format!("{}/messages", base.trim_end_matches('/'));
        }
        base
    } else {
        if !base.ends_with("/chat/completions") {
            base = format!("{}/chat/completions", base.trim_end_matches('/'));
        }
        base
    };
    let model = model
        .map(str::to_owned)
        .or_else(|| std::env::var("GRAPHOXIDE_MODEL").ok())
        .unwrap_or_else(|| {
            if use_anthropic {
                "claude-sonnet-4-20250514".into()
            } else {
                "gpt-4o-mini".into()
            }
        });
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()?;
    let mut request = if use_anthropic {
        client.post(&endpoint).json(&serde_json::json!({
            "model": model,
            "max_tokens": 1024,
            "temperature": 0,
            "messages": [{"role":"user", "content":prompt}]
        }))
    } else {
        client.post(&endpoint).json(&serde_json::json!({
            "model": model,
            "temperature": 0,
            "messages": [{"role":"user", "content":prompt}]
        }))
    };
    if use_anthropic {
        request = request.header("anthropic-version", "2023-06-01");
        if let Some(key) = anthropic_key {
            request = request.header("x-api-key", key);
        } else if !endpoint.contains("localhost") && !endpoint.contains("127.0.0.1") {
            anyhow::bail!(
                "ANTHROPIC_API_KEY is not set (or configure ANTHROPIC_BASE_URL for a local endpoint)"
            )
        }
    } else if let Some(key) = openai_key {
        request = request.bearer_auth(key);
    } else if !endpoint.contains("localhost") && !endpoint.contains("127.0.0.1") {
        anyhow::bail!(
            "OPENAI_API_KEY is not set (or configure GRAPHOXIDE_LLM_BASE_URL for a local endpoint)"
        )
    }
    let response = request
        .send()?
        .error_for_status()?
        .json::<serde_json::Value>()?;
    let content = response
        .pointer("/choices/0/message/content")
        .and_then(|value| value.as_str())
        .or_else(|| {
            response
                .pointer("/content/0/text")
                .and_then(|value| value.as_str())
        })
        .ok_or_else(|| anyhow::anyhow!("label endpoint returned no message content"))?;
    let begin = content.find('{').unwrap_or(0);
    let end = content
        .rfind('}')
        .map(|index| index + 1)
        .unwrap_or(content.len());
    let labels: serde_json::Value = serde_json::from_str(&content[begin..end])?;
    let labels = labels
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("label response must be a JSON object"))?;
    let mut updated = 0;
    for node in &mut graph.nodes {
        let Some(community) = node.community else {
            continue;
        };
        let Some(label) = labels
            .get(&community.to_string())
            .and_then(|value| value.as_str())
        else {
            continue;
        };
        let label = graphoxide_core::sanitize_label(label);
        if !label.is_empty() {
            node.extra.insert("community_name".into(), label.into());
            updated += 1;
        }
    }
    graphoxide_core::write_graph_atomic(&graph_path, &graph, true)?;
    let analysis = graphoxide_graph::analyze(&graph)?;
    let report_path = graph_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("GRAPH_REPORT.md");
    write_text(
        &report_path,
        &graphoxide_export::render_report(&graph, &analysis),
    )?;
    write_output(&format!(
        "Updated community labels on {updated} nodes and regenerated {}.",
        report_path.display()
    ))
}

fn claude(command: ClaudeCommand) -> anyhow::Result<()> {
    let (action, root) = match command {
        ClaudeCommand::Install { path } => ("install", path),
        ClaudeCommand::Uninstall { path } => ("uninstall", path),
        ClaudeCommand::Status { path } => ("status", path),
    };
    let marker_start = "<!-- graphoxide:start -->";
    let marker_end = "<!-- graphoxide:end -->";
    let claude_md = root.join("CLAUDE.md");
    let settings_path = root.join(".claude/settings.json");
    if action == "status" {
        let markdown = std::fs::read_to_string(&claude_md)
            .ok()
            .is_some_and(|text| text.contains(marker_start));
        let settings = std::fs::read_to_string(&settings_path)
            .ok()
            .is_some_and(|text| text.contains("graphoxide hook-guard"));
        return write_output(if markdown && settings {
            "Claude Code graphoxide integration installed."
        } else {
            "Claude Code graphoxide integration not installed."
        });
    }
    if action == "install" {
        let block = format!(
            "{marker_start}\n## graphoxide\nFor codebase questions, run `graphoxide query \"<question>\"` before broad file searches. Rebuild with `graphoxide update .` after structural code changes.\n{marker_end}"
        );
        let mut markdown = std::fs::read_to_string(&claude_md).unwrap_or_default();
        replace_managed_block(&mut markdown, marker_start, marker_end, &block);
        write_text(&claude_md, &markdown)?;

        let mut settings: serde_json::Value = if settings_path.is_file() {
            serde_json::from_str(&std::fs::read_to_string(&settings_path)?).map_err(|error| {
                anyhow::anyhow!(
                    "refusing to modify invalid {}: {error}",
                    settings_path.display()
                )
            })?
        } else {
            serde_json::json!({})
        };
        let executable = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("graphoxide"));
        let command = shell_command(&executable, "hook-guard");
        let hooks = settings
            .as_object_mut()
            .ok_or_else(|| {
                anyhow::anyhow!("{} must contain a JSON object", settings_path.display())
            })?
            .entry("hooks")
            .or_insert_with(|| serde_json::json!({}));
        let hooks = hooks.as_object_mut().ok_or_else(|| {
            anyhow::anyhow!("{}.hooks must be an object", settings_path.display())
        })?;
        let pre = hooks
            .entry("PreToolUse")
            .or_insert_with(|| serde_json::json!([]));
        let pre = pre.as_array_mut().ok_or_else(|| {
            anyhow::anyhow!(
                "{}.hooks.PreToolUse must be an array",
                settings_path.display()
            )
        })?;
        pre.retain(|item| !item.to_string().contains("graphoxide hook-guard"));
        for matcher in ["Bash|Grep", "Read|Glob"] {
            pre.push(serde_json::json!({
                "matcher": matcher,
                "hooks": [{"type":"command", "command":command}]
            }));
        }
        write_text(
            &settings_path,
            &(serde_json::to_string_pretty(&settings)? + "\n"),
        )?;
        return write_output("Claude Code graphoxide integration installed.");
    }

    for target in [
        claude_md,
        root.join("CLAUDE.local.md"),
        root.join(".claude/CLAUDE.local.md"),
    ] {
        if target.is_file() {
            let mut markdown = std::fs::read_to_string(&target)?;
            remove_managed_block(&mut markdown, marker_start, marker_end);
            if markdown.trim().is_empty() {
                std::fs::remove_file(target)?;
            } else {
                write_text(&target, &markdown)?;
            }
        }
    }
    for target in [settings_path, root.join(".claude/settings.local.json")] {
        if !target.is_file() {
            continue;
        }
        let mut settings: serde_json::Value =
            match serde_json::from_str(&std::fs::read_to_string(&target)?) {
                Ok(value) => value,
                Err(_) => continue,
            };
        if let Some(pre) = settings
            .get_mut("hooks")
            .and_then(|value| value.get_mut("PreToolUse"))
            .and_then(|value| value.as_array_mut())
        {
            pre.retain(|item| !item.to_string().contains("graphoxide hook-guard"));
            write_text(&target, &(serde_json::to_string_pretty(&settings)? + "\n"))?;
        }
    }
    write_output("Claude Code graphoxide integration removed.")
}

fn replace_managed_block(text: &mut String, start: &str, end: &str, block: &str) {
    if let (Some(begin), Some(finish)) = (text.find(start), text.find(end)) {
        let finish = finish + end.len();
        text.replace_range(begin..finish, block);
    } else {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(block);
        text.push('\n');
    }
}

fn remove_managed_block(text: &mut String, start: &str, end: &str) {
    if let (Some(begin), Some(finish)) = (text.find(start), text.find(end)) {
        let mut finish = finish + end.len();
        if text.as_bytes().get(finish) == Some(&b'\n') {
            finish += 1;
        }
        text.replace_range(begin..finish, "");
    }
}

fn shell_command(executable: &std::path::Path, argument: &str) -> String {
    let raw = executable.to_string_lossy();
    if raw.contains([' ', '\'', '"']) {
        format!("'{}' {argument}", raw.replace('\'', "'\\''"))
    } else {
        format!("{raw} {argument}")
    }
}

fn hook(command: HookCommand) -> anyhow::Result<()> {
    let (action, path) = match command {
        HookCommand::Install { path } => ("install", path),
        HookCommand::Uninstall { path } => ("uninstall", path),
        HookCommand::Status { path } => ("status", path),
    };
    let hooks = git_hooks_dir(&path).unwrap_or_else(|| path.join(".git/hooks"));
    let targets = [hooks.join("post-commit"), hooks.join("post-checkout")];
    match action {
        "install" => {
            std::fs::create_dir_all(&hooks)?;
            let executable =
                std::env::current_exe().unwrap_or_else(|_| PathBuf::from("graphoxide"));
            let block = format!(
                "# graphoxide:start\n{} . >/dev/null 2>&1 &\n# graphoxide:end\n",
                shell_command(&executable, "update")
            );
            for target in &targets {
                install_hook_block(target, &block)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(target, std::fs::Permissions::from_mode(0o755))?;
                }
            }
            let driver = format!("{} %O %A %B", shell_command(&executable, "merge-driver"));
            let _ = std::process::Command::new("git")
                .args([
                    "config",
                    "merge.graphoxide.name",
                    "Graphoxide graph union merge",
                ])
                .current_dir(&path)
                .status();
            let _ = std::process::Command::new("git")
                .args(["config", "merge.graphoxide.driver", &driver])
                .current_dir(&path)
                .status();
            ensure_line(
                &path.join(".gitattributes"),
                "graphoxide-out/graph.json merge=graphoxide",
            )?;
            write_output("Graphoxide git hooks installed.")
        }
        "uninstall" => {
            for target in &targets {
                if target.exists() {
                    remove_hook_block(target)?;
                }
            }
            let _ = std::process::Command::new("git")
                .args(["config", "--unset", "merge.graphoxide.driver"])
                .current_dir(&path)
                .status();
            remove_line(
                &path.join(".gitattributes"),
                "graphoxide-out/graph.json merge=graphoxide",
            )?;
            let _ = std::process::Command::new("git")
                .args(["config", "--unset", "merge.graphoxide.name"])
                .current_dir(&path)
                .status();
            write_output("Graphoxide git hooks removed.")
        }
        _ => {
            let installed = targets.iter().all(|p| p.exists());
            write_output(if installed {
                "Graphoxide git hooks installed."
            } else {
                "Graphoxide git hooks not installed."
            })
        }
    }
}

fn git_hooks_dir(root: &std::path::Path) -> Option<PathBuf> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--path-format=absolute", "--git-path", "hooks"])
        .current_dir(root)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| PathBuf::from(String::from_utf8_lossy(&output.stdout).trim().to_owned()))
}

fn install_hook_block(path: &std::path::Path, block: &str) -> anyhow::Result<()> {
    let mut existing = std::fs::read_to_string(path).unwrap_or_else(|_| "#!/bin/sh\n".into());
    if let (Some(start), Some(end)) = (
        existing.find("# graphoxide:start"),
        existing.find("# graphoxide:end"),
    ) {
        let end = end + "# graphoxide:end".len();
        existing.replace_range(start..end, block.trim_end());
    } else {
        if !existing.ends_with('\n') {
            existing.push('\n');
        }
        existing.push_str(block);
    }
    write_text(path, &existing)
}

fn remove_hook_block(path: &std::path::Path) -> anyhow::Result<()> {
    let mut existing = std::fs::read_to_string(path)?;
    if let (Some(start), Some(end)) = (
        existing.find("# graphoxide:start"),
        existing.find("# graphoxide:end"),
    ) {
        let mut end = end + "# graphoxide:end".len();
        if existing.as_bytes().get(end) == Some(&b'\n') {
            end += 1;
        }
        existing.replace_range(start..end, "");
        if existing.trim() == "#!/bin/sh" {
            std::fs::remove_file(path)?;
        } else {
            write_text(path, &existing)?;
        }
    }
    Ok(())
}

fn ensure_line(path: &std::path::Path, line: &str) -> anyhow::Result<()> {
    let mut text = std::fs::read_to_string(path).unwrap_or_default();
    if !text.lines().any(|existing| existing.trim() == line) {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(line);
        text.push('\n');
        write_text(path, &text)?;
    }
    Ok(())
}

fn remove_line(path: &std::path::Path, line: &str) -> anyhow::Result<()> {
    if !path.is_file() {
        return Ok(());
    }
    let text = std::fs::read_to_string(path)?;
    let filtered = text
        .lines()
        .filter(|existing| existing.trim() != line)
        .collect::<Vec<_>>()
        .join("\n");
    if filtered.trim().is_empty() {
        std::fs::remove_file(path)?;
    } else {
        write_text(path, &(filtered + "\n"))?;
    }
    Ok(())
}

fn hook_guard() -> anyhow::Result<()> {
    use std::io::Read;
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let value: serde_json::Value = match serde_json::from_str(&input) {
        Ok(value) => value,
        Err(_) => return write_output("{}"),
    };
    let tool = value
        .get("tool_input")
        .and_then(|v| v.as_object())
        .or_else(|| value.as_object());
    let input = tool
        .map(|t| {
            ["prompt", "command", "pattern", "path", "file_path"]
                .iter()
                .filter_map(|key| t.get(*key).and_then(|v| v.as_str()))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    let graph = PathBuf::from("graphoxide-out/graph.json");
    let stamp = PathBuf::from("graphoxide-out/cache/last_query_stamp");
    let stamp_fresh = stamp
        .metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|time| time.elapsed().ok())
        .is_some_and(|age| age < std::time::Duration::from_secs(300));
    let lower = input.to_lowercase().replace('\\', "/");
    let should_nudge = graph.is_file()
        && !stamp_fresh
        && !lower.contains("graphoxide-out/")
        && !input.is_empty()
        && [
            "code",
            "function",
            "class",
            "architecture",
            "call",
            "import",
            "impact",
            "where",
            "grep",
            "rg ",
            "find ",
            ".rs",
            ".py",
            ".ts",
            ".go",
            ".java",
        ]
        .iter()
        .any(|term| lower.contains(term));
    let output = if should_nudge {
        serde_json::json!({"hookSpecificOutput":{"hookEventName":"PreToolUse","additionalContext":"Use graphoxide query first to inspect the project knowledge graph before searching files broadly."}})
    } else {
        serde_json::json!({})
    };
    write_output(&serde_json::to_string(&output)?)
}

fn rebuild(path: &std::path::Path, no_cluster: bool, force: bool) -> anyhow::Result<()> {
    let Some(_lock) = RebuildLock::acquire(path)? else {
        return write_output("A rebuild is already running; changes were queued.");
    };
    loop {
        let extractions = graphoxide_extract::extract_project(path)?;
        let output = path.join("graphoxide-out/graph.json");
        let previous = graphoxide_core::read_graph(&output).ok();
        if no_cluster {
            if !graphoxide_core::write_raw_extractions_atomic(&output, &extractions, force)? {
                anyhow::bail!("refusing to overwrite a larger existing graph; pass --force after verifying the reduction")
            }
            save_build_config(path, no_cluster)?;
            return write_output(&format!("Wrote raw extraction to {}", output.display()));
        }
        let mut graph = graphoxide_graph::build_graph(&extractions)?;
        graphoxide_graph::cluster(&mut graph)?;
        if let Some(previous) = &previous {
            graphoxide_graph::cluster::remap_communities_to_previous(&mut graph, previous);
        }
        if !graphoxide_core::write_graph_atomic(&output, &graph, force)? {
            anyhow::bail!("refusing to overwrite a larger existing graph; pass --force after verifying the reduction")
        }
        save_build_config(path, no_cluster)?;
        let needs_update = path.join("graphoxide-out/needs_update");
        if needs_update.is_file() {
            std::fs::remove_file(needs_update)?;
        }
        let pending = path.join("graphoxide-out/pending-changes");
        if pending.is_file() {
            std::fs::remove_file(&pending)?;
            eprintln!("[graphoxide] changes arrived during rebuild; rebuilding once more");
            continue;
        }
        return write_output(&format!(
            "Wrote {} nodes and {} edges to {}",
            graph.nodes.len(),
            graph.links.len(),
            output.display()
        ));
    }
}

struct RebuildLock {
    path: PathBuf,
}

impl RebuildLock {
    fn acquire(root: &std::path::Path) -> anyhow::Result<Option<Self>> {
        let out = root.join("graphoxide-out");
        std::fs::create_dir_all(&out)?;
        let path = out.join("rebuild.lock");
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                writeln!(file, "{}", std::process::id())?;
                let pending = out.join("pending-changes");
                if pending.is_file() {
                    std::fs::remove_file(pending)?;
                }
                Ok(Some(Self { path }))
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                std::fs::write(out.join("pending-changes"), b"")?;
                Ok(None)
            }
            Err(error) => Err(error.into()),
        }
    }
}
impl Drop for RebuildLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn save_build_config(root: &std::path::Path, no_cluster: bool) -> anyhow::Result<()> {
    write_text(
        &root.join("graphoxide-out/.graphoxide_build.json"),
        &serde_json::to_string_pretty(&serde_json::json!({
            "code_only": true,
            "cluster": !no_cluster,
            "version": env!("CARGO_PKG_VERSION")
        }))?,
    )
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
    let _ = (|| -> anyhow::Result<()> {
        let enabled = |name: &str| {
            std::env::var(name)
                .ok()
                .is_some_and(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes"))
        };
        if enabled("GRAPHOXIDE_QUERY_LOG_DISABLE") {
            return Ok(());
        }
        let path = match std::env::var("GRAPHOXIDE_QUERY_LOG") {
            Ok(path) if !path.trim().is_empty() => PathBuf::from(path),
            _ if enabled("GRAPHOXIDE_QUERY_LOG_ENABLE") => {
                let Some(home) = std::env::var_os("HOME") else {
                    return Ok(());
                };
                PathBuf::from(home).join(".cache/graphoxide-queries.log")
            }
            _ => return Ok(()),
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let parts: Vec<_> = result.split_whitespace().collect();
        let nodes = parts.windows(3).find_map(|w| {
            (w[1] == "nodes" && w[2] == "found")
                .then(|| w[0].parse::<usize>().ok())
                .flatten()
        });
        let mut value = serde_json::json!({
            "ts_unix_ms": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis(),
            "kind": kind,
            "question": question,
            "corpus": graph.display().to_string(),
            "nodes_returned": nodes,
            "result_chars": result.len(),
            "duration_ms": duration.as_secs_f64() * 1000.0
        });
        if enabled("GRAPHOXIDE_QUERY_LOG_RESPONSES") {
            value["response"] = result.into();
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        writeln!(file, "{}", serde_json::to_string(&value)?)?;
        Ok(())
    })();
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

fn write_output(output: &str) -> anyhow::Result<()> {
    let mut stdout = std::io::stdout().lock();
    match writeln!(stdout, "{output}") {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::relevant_watch_paths;
    use std::path::{Path, PathBuf};

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
}
