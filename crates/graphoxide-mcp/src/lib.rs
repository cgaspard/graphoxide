//! Graphoxide MCP stdio server.
use graphoxide_core::KnowledgeGraph;
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{
        Implementation, ListResourcesResult, PaginatedRequestParams, ReadResourceRequestParams,
        ReadResourceResponse, ReadResourceResult, Resource, ResourceContents, ServerCapabilities,
        ServerInfo,
    },
    schemars,
    service::RequestContext,
    tool, tool_handler, tool_router, ErrorData, RoleServer, ServerHandler, ServiceExt,
};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
};

const SERVER_INSTRUCTIONS: &str = "Use Graphoxide before broad filesystem searches when a user asks to explore, explain, navigate, trace, or assess impact in a codebase. Start with project_overview for architecture, query_graph for a focused neighborhood, then get_node, get_neighbors, or shortest_path for exact evidence. Treat results as deterministic static-analysis evidence, synthesize the answer yourself, cite returned source locations, and verify runtime behavior in source or tests. A no-match result does not prove a concept is absent. Clearly distinguish INFERRED edges from EXTRACTED facts.";

#[derive(Debug, Clone, Default)]
struct GraphoxideServer {
    cache: Arc<Mutex<GraphCache>>,
}
#[derive(Debug, Default)]
struct GraphCache {
    values: HashMap<PathBuf, (u128, u64, Arc<KnowledgeGraph>)>,
    order: VecDeque<PathBuf>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct Project {
    #[schemars(description = "Project root containing graphoxide-out/graph.json")]
    project_path: Option<String>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct QueryParams {
    #[schemars(
        description = "Natural-language question, exact symbol, filename, or domain term to find"
    )]
    question: String,
    #[schemars(description = "Traversal mode: 'bfs' (default) or 'dfs'")]
    mode: Option<String>,
    #[schemars(description = "Neighborhood depth from 0 to 6; defaults to 2")]
    depth: Option<usize>,
    #[schemars(description = "Approximate maximum response tokens; defaults to 2000")]
    token_budget: Option<usize>,
    #[schemars(
        description = "Optional relationship categories: call, import, type, structure, or exact relation names"
    )]
    context_filter: Option<Vec<String>>,
    #[schemars(
        description = "Project root containing graphoxide-out/graph.json; defaults to the server working directory"
    )]
    project_path: Option<String>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct OverviewParams {
    #[schemars(
        description = "Maximum architectural hubs to include; defaults to 8 and caps at 20"
    )]
    top_n: Option<usize>,
    #[schemars(description = "Approximate maximum response tokens; defaults to 3000")]
    token_budget: Option<usize>,
    #[schemars(
        description = "Project root containing graphoxide-out/graph.json; defaults to the server working directory"
    )]
    project_path: Option<String>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct NodeParams {
    #[schemars(description = "Exact or partial symbol label, source filename, or node ID")]
    label: String,
    #[schemars(
        description = "Project root containing graphoxide-out/graph.json; defaults to the server working directory"
    )]
    project_path: Option<String>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct NeighborParams {
    #[schemars(description = "Exact or partial symbol label, source filename, or node ID")]
    label: String,
    #[schemars(
        description = "Optional relation substring such as calls, references, imports, or method"
    )]
    relation_filter: Option<String>,
    #[schemars(description = "Approximate maximum response tokens; defaults to 2000")]
    token_budget: Option<usize>,
    #[schemars(
        description = "Project root containing graphoxide-out/graph.json; defaults to the server working directory"
    )]
    project_path: Option<String>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct CommunityParams {
    #[schemars(description = "Numeric community ID returned by project_overview or query_graph")]
    community_id: i64,
    #[schemars(description = "Approximate maximum response tokens; defaults to 2000")]
    token_budget: Option<usize>,
    #[schemars(
        description = "Project root containing graphoxide-out/graph.json; defaults to the server working directory"
    )]
    project_path: Option<String>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct PathParams {
    #[schemars(description = "Starting symbol label, source filename, or node ID")]
    source: String,
    #[schemars(description = "Target symbol label, source filename, or node ID")]
    target: String,
    #[schemars(description = "Optional maximum acceptable hop count")]
    max_hops: Option<usize>,
    #[schemars(
        description = "Project root containing graphoxide-out/graph.json; defaults to the server working directory"
    )]
    project_path: Option<String>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct GodParams {
    #[schemars(description = "Number of hubs to return; defaults to 10")]
    top_n: Option<usize>,
    #[schemars(
        description = "Project root containing graphoxide-out/graph.json; defaults to the server working directory"
    )]
    project_path: Option<String>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct PrListParams {
    #[schemars(description = "Optional pull request base branch filter")]
    base: Option<String>,
    #[schemars(description = "Optional GitHub repository in OWNER/REPO form")]
    repo: Option<String>,
    #[schemars(description = "Project root used as the GitHub CLI working directory")]
    project_path: Option<String>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct PrImpactParams {
    #[schemars(description = "GitHub pull request number")]
    pr_number: i64,
    #[schemars(description = "Optional GitHub repository in OWNER/REPO form")]
    repo: Option<String>,
    #[schemars(
        description = "Project root containing the graph and used as the GitHub CLI working directory"
    )]
    project_path: Option<String>,
}

#[tool_router]
impl GraphoxideServer {
    #[tool(
        description = "Use for codebase questions that need a focused, source-located graph neighborhood. Prefer exact symbols or domain terms; use context_filter=['call'] for execution flow. The result is evidence for the agent to synthesize, not a generated answer.",
        annotations(
            title = "Query code graph",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn query_graph(&self, Parameters(p): Parameters<QueryParams>) -> String {
        let path = graph_path(p.project_path);
        match self.load_graph(&path) {
            Ok(graph) => {
                stamp_query(&path);
                let context_filter = p.context_filter.unwrap_or_default();
                if p.mode.as_deref() == Some("dfs") {
                    graphoxide_query::query_graph_dfs_filtered(
                        &graph,
                        &p.question,
                        p.depth.unwrap_or(2).min(6),
                        p.token_budget.unwrap_or(2000),
                        &context_filter,
                    )
                } else {
                    graphoxide_query::query_graph_filtered(
                        &graph,
                        &p.question,
                        p.depth.unwrap_or(2).min(6),
                        p.token_budget.unwrap_or(2000),
                        &context_filter,
                    )
                }
            }
            Err(error) => format!("Could not load {}: {error}", path.display()),
        }
    }
    #[tool(
        description = "Use first when asked to explore, summarize, or explain a codebase. Returns a compact architecture inventory with graph counts, hubs, communities, and indexed source files.",
        annotations(
            title = "Overview of project architecture",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn project_overview(&self, Parameters(p): Parameters<OverviewParams>) -> String {
        self.with_graph(p.project_path, |graph| {
            project_overview_text(
                graph,
                p.top_n.unwrap_or(8).min(20),
                p.token_budget.unwrap_or(3000),
            )
        })
    }
    #[tool(
        description = "Use after locating a specific symbol to get its exact graph identity, source location, type, community, and connection count.",
        annotations(
            title = "Inspect one code symbol",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn get_node(&self, Parameters(p): Parameters<NodeParams>) -> String {
        self.with_graph(p.project_path, |g| node_text(g, &p.label))
    }
    #[tool(
        description = "Use for precise incoming and outgoing relationships around one known symbol. Optionally restrict to relations such as calls, references, imports, or method.",
        annotations(
            title = "Inspect symbol relationships",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn get_neighbors(&self, Parameters(p): Parameters<NeighborParams>) -> String {
        self.with_graph(p.project_path, |g| {
            neighbors_text(
                g,
                &p.label,
                p.relation_filter.as_deref(),
                p.token_budget.unwrap_or(2000),
            )
        })
    }
    #[tool(
        description = "Use after project_overview or query_graph returns a community ID to inspect that architectural area and its source locations.",
        annotations(
            title = "Inspect architecture community",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn get_community(&self, Parameters(p): Parameters<CommunityParams>) -> String {
        self.with_graph(p.project_path, |g| {
            let mut nodes: Vec<_> = g
                .nodes
                .iter()
                .filter(|n| n.community == Some(p.community_id))
                .collect();
            nodes.sort_by(|a, b| a.id.cmp(&b.id));
            if nodes.is_empty() {
                return format!("Community {} not found.", p.community_id);
            }
            let name = nodes[0]
                .extra
                .get("community_name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let mut lines = vec![if name.is_empty()
                || name == format!("Community {}", p.community_id)
            {
                format!("Community {} ({} nodes):", p.community_id, nodes.len())
            } else {
                format!(
                    "Community {} — {} ({} nodes):",
                    p.community_id,
                    graphoxide_core::sanitize_label(name),
                    nodes.len()
                )
            }];
            lines.extend(nodes.into_iter().map(|n| {
                format!(
                    "- {} [{} {}]",
                    n.label,
                    n.source_file,
                    n.source_location.as_deref().unwrap_or("")
                )
            }));
            cut_lines(lines, p.token_budget.unwrap_or(2000))
        })
    }
    #[tool(
        description = "Use to identify highly connected architectural hubs for impact analysis or to choose where codebase exploration should begin.",
        annotations(
            title = "Find architectural hubs",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn god_nodes(&self, Parameters(p): Parameters<GodParams>) -> String {
        self.with_graph(p.project_path, |g| {
            let mut lines = vec!["God nodes (most connected):".into()];
            lines.extend(
                graphoxide_query::god_nodes(g, p.top_n.unwrap_or(10))
                    .into_iter()
                    .enumerate()
                    .map(|(i, (_, label, degree))| {
                        format!("  {}. {} - {} edges", i + 1, label, degree)
                    }),
            );
            lines.join("\n")
        })
    }
    #[tool(
        description = "Use to verify graph availability, coverage, and confidence distribution. For an architecture summary, prefer project_overview.",
        annotations(
            title = "Check graph coverage",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn graph_stats(&self, Parameters(p): Parameters<Project>) -> String {
        self.with_graph(p.project_path, graph_stats_text)
    }
    #[tool(
        description = "Use to test how two known symbols are structurally connected. Report edge directions and confidence; a shortest structural path is not necessarily a runtime execution path.",
        annotations(
            title = "Trace symbols through the graph",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn shortest_path(&self, Parameters(p): Parameters<PathParams>) -> String {
        self.with_graph(p.project_path, |g| {
            let result = graphoxide_query::shortest_path(g, &p.source, &p.target);
            if let Some(limit) = p.max_hops {
                if let Some(hops) = result
                    .strip_prefix("Shortest path (")
                    .and_then(|rest| rest.split_whitespace().next())
                    .and_then(|value| value.parse::<usize>().ok())
                    .filter(|hops| *hops > limit)
                {
                    return format!("Path exceeds max_hops={limit} ({hops} hops found).");
                }
            }
            result
        })
    }
    #[tool(
        description = "Use when the user asks which open GitHub pull requests may need graph-based impact review. Requires an authenticated GitHub CLI.",
        annotations(
            title = "List pull requests",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    fn list_prs(&self, Parameters(p): Parameters<PrListParams>) -> String {
        let mut args = vec!["pr".into(), "list".into(), "--limit".into(), "30".into()];
        append_pr_filters(&mut args, p.base, p.repo);
        gh_owned(p.project_path, &args)
    }
    #[tool(
        description = "Use to combine one GitHub pull request's changed files with Graphoxide communities and symbols for a likely-impact summary. Requires an authenticated GitHub CLI.",
        annotations(
            title = "Assess pull request impact",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    fn get_pr_impact(&self, Parameters(p): Parameters<PrImpactParams>) -> String {
        self.pr_impact(p)
    }
    #[tool(
        description = "Use when triaging several GitHub pull requests by changed files, checks, and review status before deeper graph impact calls. Requires an authenticated GitHub CLI.",
        annotations(
            title = "Triage pull requests",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    fn triage_prs(&self, Parameters(p): Parameters<PrListParams>) -> String {
        let mut args = vec![
            "pr".into(),
            "list".into(),
            "--limit".into(),
            "30".into(),
            "--json".into(),
            "number,title,files,statusCheckRollup,reviewDecision".into(),
        ];
        append_pr_filters(&mut args, p.base, p.repo);
        gh_owned(p.project_path, &args)
    }
}

impl GraphoxideServer {
    fn with_graph(
        &self,
        project: Option<String>,
        f: impl FnOnce(&KnowledgeGraph) -> String,
    ) -> String {
        let path = graph_path(project);
        match self.load_graph(&path) {
            Ok(graph) => f(&graph),
            Err(error) => format!("Could not load {}: {error}", path.display()),
        }
    }

    fn load_graph(&self, path: &Path) -> anyhow::Result<Arc<KnowledgeGraph>> {
        let metadata = std::fs::metadata(path)?;
        let stamp = metadata
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let size = metadata.len();
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| anyhow::anyhow!("graph cache lock poisoned"))?;
        if let Some((cached_stamp, cached_size, graph)) = cache.values.get(path) {
            if *cached_stamp == stamp && *cached_size == size {
                return Ok(graph.clone());
            }
        }
        let graph = Arc::new(graphoxide_core::read_graph(path)?);
        cache
            .values
            .insert(path.to_path_buf(), (stamp, size, graph.clone()));
        cache.order.retain(|p| p != path);
        cache.order.push_back(path.to_path_buf());
        while cache.order.len() > 8 {
            if let Some(old) = cache.order.pop_front() {
                cache.values.remove(&old);
            }
        }
        Ok(graph)
    }

    fn pr_impact(&self, p: PrImpactParams) -> String {
        let root = p.project_path.clone();
        let mut args = vec![
            "pr".into(),
            "view".into(),
            p.pr_number.to_string(),
            "--json".into(),
            "number,title,files,baseRefName,author,reviewDecision,statusCheckRollup".into(),
        ];
        if let Some(repo) = p.repo {
            args.push("--repo".into());
            args.push(repo);
        }
        let raw = match run_gh(root.clone(), &args) {
            Ok(raw) => raw,
            Err(error) => return format!("PR #{} not found or gh failed: {error}", p.pr_number),
        };
        let value: serde_json::Value = match serde_json::from_str(&raw) {
            Ok(value) => value,
            Err(error) => return format!("Could not parse gh response: {error}"),
        };
        let files: Vec<_> = value
            .get("files")
            .and_then(|files| files.as_array())
            .into_iter()
            .flatten()
            .filter_map(|file| {
                file.get("path")
                    .or_else(|| file.get("name"))
                    .and_then(|path| path.as_str())
                    .map(str::to_owned)
            })
            .collect();
        let graph_path = graph_path(root);
        let graph = match self.load_graph(&graph_path) {
            Ok(graph) => graph,
            Err(error) => return format!("Could not load {}: {error}", graph_path.display()),
        };
        let touched: Vec<_> = graph
            .nodes
            .iter()
            .filter(|node| {
                files.iter().any(|file| {
                    node.source_file == *file || node.source_file.ends_with(&format!("/{file}"))
                })
            })
            .collect();
        let communities: BTreeSet<_> = touched.iter().filter_map(|node| node.community).collect();
        let title = value.get("title").and_then(|v| v.as_str()).unwrap_or("");
        let mut lines = vec![
            format!(
                "PR #{}: {}",
                p.pr_number,
                graphoxide_core::sanitize_label(title)
            ),
            format!(
                "Graph impact: {} nodes across {} communities",
                touched.len(),
                communities.len()
            ),
            format!("Communities touched: {communities:?}"),
            format!("Files changed ({}):", files.len()),
        ];
        lines.extend(files.iter().take(20).map(|file| format!("  {file}")));
        if files.len() > 20 {
            lines.push(format!("  … and {} more", files.len() - 20));
        }
        lines.join("\n")
    }
}

#[tool_handler]
impl ServerHandler for GraphoxideServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_server_info(Implementation::new("graphoxide", env!("CARGO_PKG_VERSION")))
        .with_instructions(SERVER_INSTRUCTIONS)
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        let resources = [
            "report",
            "stats",
            "god-nodes",
            "surprises",
            "audit",
            "questions",
        ]
        .into_iter()
        .map(|name| {
            Resource::new(format!("graphoxide://{name}"), name)
                .with_mime_type("text/plain")
                .with_description(format!("Graphoxide {name} for the current project"))
        })
        .collect();
        Ok(ListResourcesResult::with_all_items(resources))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        let path = graph_path(None);
        let graph = self
            .load_graph(&path)
            .map_err(|e| ErrorData::resource_not_found(e.to_string(), None))?;
        let name = request.uri.trim_start_matches("graphoxide://");
        let analysis = graphoxide_graph::analyze(&graph)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let text = match name {
            "report" => graphoxide_export::render_report(&graph, &analysis),
            "stats" => graph_stats_text(&graph),
            "god-nodes" => analysis
                .god_nodes
                .iter()
                .map(|n| format!("{} — {} edges", n.label, n.degree))
                .collect::<Vec<_>>()
                .join("\n"),
            "surprises" => analysis
                .surprising_connections
                .iter()
                .map(|s| format!("{} --{}--> {}: {}", s.source, s.relation, s.target, s.why))
                .collect::<Vec<_>>()
                .join("\n"),
            "questions" => analysis.suggested_questions.join("\n"),
            "audit" => audit_text(&graph),
            _ => {
                return Err(ErrorData::resource_not_found(
                    "unknown Graphoxide resource",
                    None,
                ))
            }
        };
        Ok(ReadResourceResult::new(vec![ResourceContents::text(text, request.uri)]).into())
    }
}
fn graph_path(project: Option<String>) -> PathBuf {
    PathBuf::from(project.unwrap_or_else(|| ".".into())).join("graphoxide-out/graph.json")
}
fn stamp_query(graph: &Path) {
    let Some(out) = graph.parent() else { return };
    let stamp = out.join("cache/last_query_stamp");
    if let Some(parent) = stamp.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(stamp, b"");
}
fn project_overview_text(g: &KnowledgeGraph, top_n: usize, token_budget: usize) -> String {
    let index = graphoxide_query::GraphIndex::new(g);
    let communities = g.nodes.iter().enumerate().fold(
        BTreeMap::<i64, Vec<usize>>::new(),
        |mut communities, (position, node)| {
            if let Some(community) = node.community {
                communities.entry(community).or_default().push(position);
            }
            communities
        },
    );
    let sources: BTreeSet<_> = g
        .nodes
        .iter()
        .filter(|node| !node.source_file.is_empty())
        .map(|node| node.source_file.as_str())
        .collect();
    let mut lines = vec![
        "Project overview (deterministic static-analysis evidence):".into(),
        format!(
            "Coverage: {} nodes | {} relationships | {} communities | {} source files",
            g.nodes.len(),
            g.links.len(),
            communities.len(),
            sources.len()
        ),
        "Architectural hubs:".into(),
    ];
    lines.extend(
        graphoxide_query::god_nodes(g, top_n)
            .into_iter()
            .enumerate()
            .map(|(index, (_, label, degree))| {
                format!(
                    "  {}. {} — {} relationships",
                    index + 1,
                    graphoxide_core::sanitize_label(&label),
                    degree
                )
            }),
    );
    lines.push("Communities:".into());
    for (community, positions) in communities {
        let hub = positions.iter().copied().max_by(|a, b| {
            index
                .degree(*a)
                .cmp(&index.degree(*b))
                .then_with(|| index.node(*b).id.cmp(&index.node(*a).id))
        });
        let name = positions
            .iter()
            .find_map(|position| {
                index
                    .node(*position)
                    .extra
                    .get("community_name")
                    .and_then(|value| value.as_str())
                    .filter(|value| !value.is_empty())
            })
            .unwrap_or("");
        let label = hub
            .map(|position| index.node(position).label.as_str())
            .unwrap_or("");
        lines.push(format!(
            "  - {community}{}: {} nodes; hub {}",
            if name.is_empty() {
                String::new()
            } else {
                format!(" ({})", graphoxide_core::sanitize_label(name))
            },
            positions.len(),
            graphoxide_core::sanitize_label(label)
        ));
    }
    lines.push(format!("Indexed source files ({}):", sources.len()));
    lines.extend(
        sources
            .into_iter()
            .map(|source| format!("  - {}", graphoxide_core::sanitize_label(source))),
    );
    lines.push(
        "Limits: this graph describes static structure; verify dynamic behavior and data-dependent branches in source or tests."
            .into(),
    );
    cut_lines(lines, token_budget)
}
fn graph_stats_text(g: &KnowledgeGraph) -> String {
    let communities = g
        .nodes
        .iter()
        .filter_map(|n| n.community)
        .collect::<BTreeSet<_>>()
        .len();
    let sources = g
        .nodes
        .iter()
        .filter(|n| !n.source_file.is_empty())
        .map(|n| n.source_file.as_str())
        .collect::<BTreeSet<_>>()
        .len();
    let total = g.links.len().max(1);
    let percentage = |confidence| {
        g.links
            .iter()
            .filter(|edge| edge.confidence == confidence)
            .count()
            * 100
            / total
    };
    format!(
        "Nodes: {}\nEdges: {}\nCommunities: {}\nSource files: {}\nEXTRACTED: {}%\nINFERRED: {}%\nAMBIGUOUS: {}%",
        g.nodes.len(),
        g.links.len(),
        communities,
        sources,
        percentage(graphoxide_core::Confidence::Extracted),
        percentage(graphoxide_core::Confidence::Inferred),
        percentage(graphoxide_core::Confidence::Ambiguous),
    )
}

fn node_text(graph: &KnowledgeGraph, query: &str) -> String {
    let index = graphoxide_query::GraphIndex::new(graph);
    let Some(position) = graphoxide_query::query::find_node(&index, query)
        .first()
        .copied()
    else {
        return format!(
            "No node matching '{}' found.",
            graphoxide_core::sanitize_label(query)
        );
    };
    let node = index.node(position);
    let community = node
        .extra
        .get("community_name")
        .and_then(|value| value.as_str())
        .map(str::to_owned)
        .or_else(|| node.community.map(|value| value.to_string()))
        .unwrap_or_default();
    format!(
        "Node: {}\n  ID: {}\n  Source: {} {}\n  Type: {}\n  Community: {}\n  Degree: {}",
        graphoxide_core::sanitize_label(&node.label),
        graphoxide_core::sanitize_label(&node.id),
        graphoxide_core::sanitize_label(&node.source_file),
        graphoxide_core::sanitize_label(node.source_location.as_deref().unwrap_or("")),
        graphoxide_core::sanitize_label(&node.file_type),
        graphoxide_core::sanitize_label(&community),
        index.degree(position)
    )
}

fn neighbors_text(
    graph: &KnowledgeGraph,
    query: &str,
    relation_filter: Option<&str>,
    token_budget: usize,
) -> String {
    let index = graphoxide_query::GraphIndex::new(graph);
    let matches = graphoxide_query::query::find_node(&index, query);
    let Some(position) = matches.first().copied() else {
        return format!(
            "No node matching '{}' found.",
            graphoxide_core::sanitize_label(query)
        );
    };
    let node = index.node(position);
    let filter = relation_filter.unwrap_or("").to_lowercase();
    let mut rows = Vec::new();
    for edge in &graph.links {
        if !filter.is_empty() && !edge.relation.to_lowercase().contains(&filter) {
            continue;
        }
        let (direction, other) = if edge.true_source() == node.id {
            ("-->", edge.true_target())
        } else if edge.true_target() == node.id {
            ("<--", edge.true_source())
        } else {
            continue;
        };
        let label = index
            .position(other)
            .map(|other| index.node(other).label.as_str())
            .unwrap_or(other);
        let location = edge
            .extra
            .get("source_location")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        rows.push((
            label.to_lowercase(),
            format!(
                "  {direction} {} [{}] [{}]{}",
                graphoxide_core::sanitize_label(label),
                graphoxide_core::sanitize_label(&edge.relation),
                match edge.confidence {
                    graphoxide_core::Confidence::Extracted => "EXTRACTED",
                    graphoxide_core::Confidence::Inferred => "INFERRED",
                    graphoxide_core::Confidence::Ambiguous => "AMBIGUOUS",
                },
                if location.is_empty() {
                    String::new()
                } else {
                    format!(
                        " at={}:{}",
                        graphoxide_core::sanitize_label(&edge.source_file),
                        graphoxide_core::sanitize_label(location)
                    )
                }
            ),
        ));
    }
    rows.sort();
    let mut lines = vec![format!(
        "Neighbors of {}:",
        graphoxide_core::sanitize_label(&node.label)
    )];
    lines.extend(rows.into_iter().map(|(_, row)| row));
    cut_lines(lines, token_budget)
}

fn cut_lines(lines: Vec<String>, token_budget: usize) -> String {
    let budget = token_budget.saturating_mul(3);
    let mut output = String::new();
    for line in lines {
        let needed = line.chars().count() + usize::from(!output.is_empty());
        if output.chars().count() + needed > budget {
            output.push_str("\n[truncated — raise token_budget for more]");
            break;
        }
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(&line);
    }
    output
}
fn audit_text(g: &KnowledgeGraph) -> String {
    let ids: BTreeSet<_> = g.nodes.iter().map(|n| n.id.as_str()).collect();
    let dangling = g
        .links
        .iter()
        .filter(|e| !ids.contains(e.true_source()) || !ids.contains(e.true_target()))
        .count();
    let duplicates = g.nodes.len().saturating_sub(ids.len());
    format!(
        "Duplicate node IDs: {duplicates}\nDangling edges: {dangling}\nStatus: {}",
        if duplicates == 0 && dangling == 0 {
            "OK"
        } else {
            "NEEDS ATTENTION"
        }
    )
}
fn append_pr_filters(args: &mut Vec<String>, base: Option<String>, repo: Option<String>) {
    if let Some(base) = base {
        args.push("--base".into());
        args.push(base);
    }
    if let Some(repo) = repo {
        args.push("--repo".into());
        args.push(repo);
    }
}

fn run_gh(project: Option<String>, args: &[String]) -> anyhow::Result<String> {
    let root = PathBuf::from(project.unwrap_or_else(|| ".".into()));
    let output = Command::new("gh").args(args).current_dir(root).output()?;
    if !output.status.success() {
        anyhow::bail!("{}", String::from_utf8_lossy(&output.stderr).trim())
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn gh_owned(project: Option<String>, args: &[String]) -> String {
    run_gh(project, args).unwrap_or_else(|error| format!("Error: {error}"))
}
pub fn serve() -> anyhow::Result<()> {
    tokio::runtime::Runtime::new()?.block_on(async {
        let service = GraphoxideServer::default()
            .serve(rmcp::transport::stdio())
            .await?;
        service.waiting().await?;
        anyhow::Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_instructions_tell_codex_when_and_how_to_use_the_graph() {
        let info = GraphoxideServer::default().get_info();
        let instructions = info.instructions.expect("server instructions");
        assert!(instructions.starts_with("Use Graphoxide before broad filesystem searches"));
        assert!(instructions.contains("project_overview"));
        assert!(instructions.contains("does not prove"));
    }

    #[test]
    fn local_graph_tools_advertise_clear_read_only_contracts() {
        for tool in [
            GraphoxideServer::project_overview_tool_attr(),
            GraphoxideServer::query_graph_tool_attr(),
            GraphoxideServer::get_node_tool_attr(),
            GraphoxideServer::get_neighbors_tool_attr(),
        ] {
            let annotations = tool.annotations.expect("tool annotations");
            assert_eq!(annotations.read_only_hint, Some(true));
            assert_eq!(annotations.destructive_hint, Some(false));
            assert_eq!(annotations.open_world_hint, Some(false));
            assert!(tool
                .description
                .is_some_and(|description| description.len() > 60));
        }
    }

    #[test]
    fn overview_is_compact_and_explicit_about_static_evidence() {
        let graph = KnowledgeGraph {
            nodes: vec![graphoxide_core::Node {
                id: "checkout".into(),
                label: "CheckoutService".into(),
                file_type: "code".into(),
                source_file: "src/checkout.py".into(),
                source_location: Some("L10".into()),
                community: Some(1),
                extra: BTreeMap::from([("community_name".into(), "Checkout".into())]),
            }],
            ..Default::default()
        };
        let overview = project_overview_text(&graph, 8, 2000);
        assert!(overview.contains("deterministic static-analysis evidence"));
        assert!(overview.contains("CheckoutService"));
        assert!(overview.contains("src/checkout.py"));
        assert!(overview.contains("verify dynamic behavior"));
    }
}
