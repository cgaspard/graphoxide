//! Graphoxide MCP server and MCP-configuration ingestion.

pub mod http;
pub mod mcp_ingest;
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
use serde_json::{json, Value};
use sha2::{Digest as _, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    fs,
    path::{Component, Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
};

const SERVER_INSTRUCTIONS: &str = "Use Graphoxide before broad filesystem searches when a user asks to explore, explain, navigate, trace, or assess impact in a codebase. Start with project_overview for architecture, query_graph for a focused neighborhood, then get_node, get_neighbors, or shortest_path for exact evidence. Treat results as deterministic static-analysis evidence, synthesize the answer yourself, cite returned source locations, and verify runtime behavior in source or tests. A no-match result does not prove a concept is absent. Clearly distinguish INFERRED edges from EXTRACTED facts.";
const MAX_WIKI_MANIFEST_BYTES: u64 = 16 * 1024 * 1024;
const MAX_WIKI_SEARCH_BYTES: u64 = 16 * 1024 * 1024;
const MAX_WIKI_PAGE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_WIKI_DRAFT_BYTES: usize = 256 * 1024;
const MAX_WIKI_RESPONSE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone)]
struct GraphoxideServer {
    cache: Arc<Mutex<GraphCache>>,
    default_graph: Option<PathBuf>,
    max_project_contexts: usize,
}
#[derive(Debug, Default)]
struct GraphCache {
    values: HashMap<
        PathBuf,
        (
            u128,
            u64,
            Arc<KnowledgeGraph>,
            Arc<graphoxide_query::GraphQueryCache>,
        ),
    >,
    /// Only non-default project graphs participate in eviction.
    order: VecDeque<PathBuf>,
}

impl Default for GraphoxideServer {
    fn default() -> Self {
        Self {
            cache: Arc::new(Mutex::new(GraphCache::default())),
            default_graph: None,
            max_project_contexts: http::DEFAULT_MAX_CONTEXTS,
        }
    }
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

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct WikiRootParams {
    #[schemars(description = "Published wiki root containing wiki-manifest.json")]
    wiki_root: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct WikiFreshnessParams {
    #[schemars(description = "Published wiki root containing wiki-manifest.json")]
    wiki_root: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct WikiSearchParams {
    #[schemars(description = "Published wiki root containing search.json")]
    wiki_root: String,
    #[schemars(
        description = "Case-insensitive title, alias, citation, locator, evidence ID, or body query"
    )]
    query: String,
    #[schemars(description = "Maximum matches from 1 to 50; defaults to 20")]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct WikiPageParams {
    #[schemars(description = "Published wiki root containing wiki-manifest.json")]
    wiki_root: String,
    #[schemars(description = "Manifest-declared relative page path")]
    page: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct WikiEvidenceParams {
    #[schemars(description = "Published wiki root containing search.json")]
    wiki_root: String,
    #[schemars(description = "Exact evidence-block ID from a wiki page or search result")]
    evidence_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct WikiDraftValidationParams {
    #[schemars(description = "Published wiki root containing search.json")]
    wiki_root: String,
    #[schemars(description = "Manifest-declared relative article path")]
    page: String,
    #[schemars(description = "Canonical JSON draft response with evidence-bound sections")]
    draft: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct WikiReviewAttestationParams {
    #[schemars(description = "Published wiki root containing wiki-manifest.json")]
    wiki_root: String,
    #[schemars(description = "Exact reviewed plan SHA-256 from wiki-manifest.json")]
    plan_sha256: String,
    #[schemars(description = "Active source#capture citations bound to the review")]
    capture_ids: Vec<String>,
    #[schemars(
        description = "Validated article draft JSON; its digest is attested without persisting the draft"
    )]
    draft: String,
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
        let is_default = p.project_path.is_none();
        let path = self.graph_path(p.project_path);
        match self.load_graph_context(&path, is_default) {
            Ok((graph, query_cache)) => {
                stamp_query(&path);
                let context_filter = p.context_filter.unwrap_or_default();
                graphoxide_query::query_graph_text_with_cache(
                    &graph,
                    query_cache,
                    &p.question,
                    p.mode.as_deref().unwrap_or("bfs"),
                    p.depth.unwrap_or(2).min(6),
                    p.token_budget.unwrap_or(2000),
                    &context_filter,
                )
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
            if let Some(limit) = p.max_hops
                && let Some(hops) = result
                    .strip_prefix("Shortest path (")
                    .and_then(|rest| rest.split_whitespace().next())
                    .and_then(|value| value.parse::<usize>().ok())
                    .filter(|hops| *hops > limit)
            {
                return format!("Path exceeds max_hops={limit} ({hops} hops found).");
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
    #[tool(
        description = "Use to inspect the live manifest, source/page states, and pinned registry provenance of a published Graphoxide LLM wiki. This reads only published artifacts, never raw sources.",
        annotations(
            title = "Inspect LLM wiki status",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn wiki_status(&self, Parameters(p): Parameters<WikiRootParams>) -> String {
        wiki_status_text(Path::new(&p.wiki_root))
            .unwrap_or_else(|error| format!("Could not read wiki status: {error}"))
    }
    #[tool(
        description = "Use to identify stale, historical, or otherwise non-ready published LLM wiki sources and pages from the live manifest. It reads only published artifacts.",
        annotations(
            title = "Inspect LLM wiki freshness",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn wiki_freshness(&self, Parameters(p): Parameters<WikiFreshnessParams>) -> String {
        wiki_freshness_text(Path::new(&p.wiki_root))
            .unwrap_or_else(|error| format!("Could not read wiki freshness: {error}"))
    }
    #[tool(
        description = "Use to search a published Graphoxide LLM wiki's deterministic lexical index by title, alias, citation, locator, evidence ID, or bounded body text. It reads only published artifacts.",
        annotations(
            title = "Search LLM wiki",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn wiki_search(&self, Parameters(p): Parameters<WikiSearchParams>) -> String {
        wiki_search_text(Path::new(&p.wiki_root), &p.query, p.limit.unwrap_or(20))
            .unwrap_or_else(|error| format!("Could not search wiki: {error}"))
    }
    #[tool(
        description = "Use to retrieve one manifest-declared current or historical wiki page after wiki_search or wiki_status. It rejects traversal and never follows a page outside the published wiki root.",
        annotations(
            title = "Read LLM wiki page",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn wiki_get_page(&self, Parameters(p): Parameters<WikiPageParams>) -> String {
        wiki_page_text(Path::new(&p.wiki_root), &p.page)
            .unwrap_or_else(|error| format!("Could not read wiki page: {error}"))
    }
    #[tool(
        description = "Use to resolve an exact evidence-block ID to the published pages, citations, and artifact locators that contain it. Read the returned pages for the evidence text.",
        annotations(
            title = "Resolve LLM wiki evidence",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn wiki_get_evidence(&self, Parameters(p): Parameters<WikiEvidenceParams>) -> String {
        wiki_evidence_text(Path::new(&p.wiki_root), &p.evidence_id)
            .unwrap_or_else(|error| format!("Could not resolve wiki evidence: {error}"))
    }
    #[tool(
        description = "Use to validate a canonical JSON article draft against the evidence-block IDs available on its published wiki page. It returns a result only and never writes drafts or pages.",
        annotations(
            title = "Validate LLM wiki draft",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn wiki_validate_draft(&self, Parameters(p): Parameters<WikiDraftValidationParams>) -> String {
        wiki_validate_draft_text(Path::new(&p.wiki_root), &p.page, &p.draft)
            .unwrap_or_else(|error| format!("Wiki draft is invalid: {error}"))
    }
    #[tool(
        description = "Use after wiki_validate_draft to emit a deterministic review-attestation JSON artifact. The caller must submit that artifact through Git review; this tool never writes registry heads, raw sources, or wiki output.",
        annotations(
            title = "Attest LLM wiki review",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn wiki_attest_review(&self, Parameters(p): Parameters<WikiReviewAttestationParams>) -> String {
        wiki_attest_review_text(
            Path::new(&p.wiki_root),
            &p.plan_sha256,
            &p.capture_ids,
            &p.draft,
        )
        .unwrap_or_else(|error| format!("Could not attest wiki review: {error}"))
    }
}

impl GraphoxideServer {
    fn with_default_graph(path: PathBuf, max_project_contexts: usize) -> Self {
        Self {
            cache: Arc::new(Mutex::new(GraphCache::default())),
            default_graph: Some(path),
            max_project_contexts: max_project_contexts.max(1),
        }
    }

    fn graph_path(&self, project: Option<String>) -> PathBuf {
        if let Some(project) = project {
            let root = PathBuf::from(project);
            if root.extension().and_then(|extension| extension.to_str()) == Some("json") {
                return root;
            }
            let native = root.join("graphoxide-out/graph.json");
            let legacy = root.join("graphify-out/graph.json");
            if native.exists() || !legacy.exists() {
                native
            } else {
                legacy
            }
        } else {
            self.default_graph
                .clone()
                .unwrap_or_else(|| PathBuf::from("graphoxide-out/graph.json"))
        }
    }

    fn with_graph(
        &self,
        project: Option<String>,
        f: impl FnOnce(&KnowledgeGraph) -> String,
    ) -> String {
        let is_default = project.is_none();
        let path = self.graph_path(project);
        match self.load_graph(&path, is_default) {
            Ok(graph) => f(&graph),
            Err(error) => format!("Could not load graph.json at {}: {error}", path.display()),
        }
    }

    fn load_graph(&self, path: &Path, is_default: bool) -> anyhow::Result<Arc<KnowledgeGraph>> {
        self.load_graph_context(path, is_default)
            .map(|(graph, _)| graph)
    }

    fn load_graph_context(
        &self,
        path: &Path,
        is_default: bool,
    ) -> anyhow::Result<(Arc<KnowledgeGraph>, Arc<graphoxide_query::GraphQueryCache>)> {
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
        let cached = cache
            .values
            .get(path)
            .filter(|(cached_stamp, cached_size, _, _)| {
                *cached_stamp == stamp && *cached_size == size
            })
            .map(|(_, _, graph, query_cache)| (graph.clone(), query_cache.clone()));
        if let Some((graph, query_cache)) = cached {
            if !is_default {
                cache.order.retain(|candidate| candidate != path);
                cache.order.push_back(path.to_path_buf());
            }
            return Ok((graph, query_cache));
        }
        let graph = Arc::new(graphoxide_core::read_graph(path)?);
        let query_cache = Arc::new(graphoxide_query::GraphQueryCache::default());
        cache.values.insert(
            path.to_path_buf(),
            (stamp, size, graph.clone(), query_cache.clone()),
        );
        cache.order.retain(|p| p != path);
        if !is_default {
            cache.order.push_back(path.to_path_buf());
        }
        while cache.order.len() > self.max_project_contexts {
            if let Some(old) = cache.order.pop_front() {
                cache.values.remove(&old);
            }
        }
        Ok((graph, query_cache))
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
        let is_default = root.is_none();
        let graph_path = self.graph_path(root);
        let graph = match self.load_graph(&graph_path, is_default) {
            Ok(graph) => graph,
            Err(error) => return format!("Could not load {}: {error}", graph_path.display()),
        };
        let (communities, nodes_affected) =
            graphoxide_query::prs::compute_pr_impact(&files, &graph);
        let title = value.get("title").and_then(|v| v.as_str()).unwrap_or("");
        let mut lines = vec![
            format!(
                "PR #{}: {}",
                p.pr_number,
                graphoxide_core::sanitize_label(title)
            ),
            format!(
                "Graph impact: {} nodes across {} communities",
                nodes_affected,
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
        let path = self.graph_path(None);
        let graph = self
            .load_graph(&path, true)
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
                .map(|s| {
                    format!(
                        "{} --{}--> {}: {}",
                        s.source,
                        s.relation,
                        s.target,
                        s.why.as_deref().or(s.note.as_deref()).unwrap_or_default()
                    )
                })
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
    graphoxide_query::cut_lines_to_budget(
        &lines,
        token_budget,
        "Raise token_budget or narrow the request with relation_filter or get_node.",
    )
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

fn canonical_wiki_root(root: &Path) -> anyhow::Result<PathBuf> {
    let metadata = fs::symlink_metadata(root)?;
    anyhow::ensure!(
        metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
        "wiki root must be a non-symlinked directory"
    );
    let root = fs::canonicalize(root)?;
    anyhow::ensure!(
        fs::metadata(&root)?.is_dir(),
        "wiki root is not a directory"
    );
    Ok(root)
}

fn safe_wiki_relative_path(path: &str) -> anyhow::Result<&Path> {
    let path = Path::new(path);
    anyhow::ensure!(
        !path.as_os_str().is_empty()
            && path
                .components()
                .all(|component| matches!(component, Component::Normal(_))),
        "wiki path must be a non-empty relative path without traversal"
    );
    Ok(path)
}

fn read_wiki_file(root: &Path, relative: &str, cap: u64) -> anyhow::Result<Vec<u8>> {
    let relative = safe_wiki_relative_path(relative)?;
    let path = root.join(relative);
    let metadata = fs::symlink_metadata(&path)?;
    anyhow::ensure!(
        metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
        "wiki artifact is not a regular file"
    );
    anyhow::ensure!(
        metadata.len() <= cap,
        "wiki artifact exceeds its byte limit"
    );
    let resolved = fs::canonicalize(&path)?;
    anyhow::ensure!(
        resolved.starts_with(root),
        "wiki artifact resolves outside the published wiki root"
    );
    let bytes = fs::read(&resolved)?;
    anyhow::ensure!(
        bytes.len() <= cap as usize,
        "wiki artifact exceeds its byte limit"
    );
    Ok(bytes)
}

fn required_array<'a>(value: &'a Value, key: &str) -> anyhow::Result<&'a Vec<Value>> {
    value
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("wiki JSON has no array field {key:?}"))
}

fn required_string<'a>(value: &'a Value, key: &str) -> anyhow::Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("wiki JSON has no non-empty string field {key:?}"))
}

fn load_wiki_manifest(root: &Path) -> anyhow::Result<(PathBuf, Vec<u8>, Value)> {
    let root = canonical_wiki_root(root)?;
    let bytes = read_wiki_file(&root, "wiki-manifest.json", MAX_WIKI_MANIFEST_BYTES)?;
    let manifest: Value = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(
        manifest.get("version").and_then(Value::as_u64) == Some(1),
        "unsupported wiki manifest version"
    );
    let _ = required_array(&manifest, "sources")?;
    let _ = required_array(&manifest, "pages")?;
    Ok((root, bytes, manifest))
}

fn load_wiki_search_index(root: &Path) -> anyhow::Result<Value> {
    let bytes = read_wiki_file(root, "search.json", MAX_WIKI_SEARCH_BYTES)?;
    let search: Value = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(
        search.get("version").and_then(Value::as_u64) == Some(1),
        "unsupported wiki search index version"
    );
    let _ = required_array(&search, "entries")?;
    Ok(search)
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn json_text(value: Value) -> anyhow::Result<String> {
    let text = serde_json::to_string_pretty(&value)?;
    anyhow::ensure!(
        text.len() <= MAX_WIKI_RESPONSE_BYTES,
        "wiki response exceeds its byte limit; narrow the request"
    );
    Ok(text)
}

fn state_counts(items: &[Value]) -> anyhow::Result<BTreeMap<String, usize>> {
    let mut counts = BTreeMap::new();
    for item in items {
        *counts
            .entry(required_string(item, "state")?.to_owned())
            .or_default() += 1;
    }
    Ok(counts)
}

fn wiki_status_text(root: &Path) -> anyhow::Result<String> {
    let (_, _, manifest) = load_wiki_manifest(root)?;
    let registry = manifest
        .get("registry")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("wiki manifest has no registry provenance"))?;
    let sources = required_array(&manifest, "sources")?;
    let pages = required_array(&manifest, "pages")?;
    json_text(json!({
        "version": 1,
        "registry": registry,
        "graph_sha256": required_string(&manifest, "graph_sha256")?,
        "plan_sha256": required_string(&manifest, "plan_sha256")?,
        "source_states": state_counts(sources)?,
        "page_states": state_counts(pages)?,
        "historical_pages": manifest.get("historical").and_then(Value::as_array).map_or(0, Vec::len),
    }))
}

fn non_ready_items(items: &[Value], identity: &str, ready: &[&str]) -> anyhow::Result<Vec<Value>> {
    let mut result = Vec::new();
    for item in items {
        let state = required_string(item, "state")?;
        if !ready.contains(&state) {
            result.push(json!({
                "identity": required_string(item, identity)?,
                "state": state,
            }));
        }
    }
    Ok(result)
}

fn wiki_freshness_text(root: &Path) -> anyhow::Result<String> {
    let (_, _, manifest) = load_wiki_manifest(root)?;
    let sources = required_array(&manifest, "sources")?;
    let pages = required_array(&manifest, "pages")?;
    let historical = manifest
        .get("historical")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    json_text(json!({
        "version": 1,
        "source_states": state_counts(sources)?,
        "page_states": state_counts(pages)?,
        "attention": {
            "sources": non_ready_items(sources, "citation", &["source-ready"] )?,
            "pages": non_ready_items(pages, "path", &["source-ready", "reviewed-ready"] )?,
            "historical": non_ready_items(historical, "archived_path", &[])?,
        }
    }))
}

fn truncated(value: &str, max_chars: usize) -> String {
    let mut value = value.chars();
    let text = value.by_ref().take(max_chars).collect::<String>();
    if value.next().is_some() {
        format!("{text}…")
    } else {
        text
    }
}

fn output_strings(entry: &Value, key: &str, max_items: usize) -> anyhow::Result<Vec<String>> {
    required_array(entry, key)?
        .iter()
        .take(max_items)
        .map(|value| {
            Ok(truncated(
                value.as_str().ok_or_else(|| {
                    anyhow::anyhow!("wiki search entry has a non-string {key:?} value")
                })?,
                4096,
            ))
        })
        .collect()
}

fn search_entry_summary(entry: &Value) -> anyhow::Result<Value> {
    Ok(json!({
        "path": truncated(required_string(entry, "path")?, 4096),
        "title": truncated(required_string(entry, "title")?, 4096),
        "aliases": output_strings(entry, "aliases", 64)?,
        "kind": truncated(required_string(entry, "kind")?, 256),
        "domain": truncated(required_string(entry, "domain")?, 1024),
        "citations": output_strings(entry, "citations", 256)?,
        "locators": output_strings(entry, "locators", 256)?,
        "evidence_ids": output_strings(entry, "evidence_ids", 256)?,
        "body": truncated(required_string(entry, "body")?, 8192),
    }))
}

fn wiki_search_text(root: &Path, query: &str, limit: usize) -> anyhow::Result<String> {
    anyhow::ensure!(
        !query.trim().is_empty() && query.len() <= 256,
        "wiki search query must be 1 to 256 bytes"
    );
    anyhow::ensure!(
        (1..=50).contains(&limit),
        "wiki search limit must be 1 to 50"
    );
    let root = canonical_wiki_root(root)?;
    let search = load_wiki_search_index(&root)?;
    let query = query.to_ascii_lowercase();
    let mut matches = Vec::new();
    for entry in required_array(&search, "entries")? {
        if entry.to_string().to_ascii_lowercase().contains(&query) {
            matches.push(search_entry_summary(entry)?);
            if matches.len() == limit {
                break;
            }
        }
    }
    json_text(json!({ "query": query, "matches": matches }))
}

fn manifest_page<'a>(manifest: &'a Value, page: &str) -> anyhow::Result<(&'a Value, &'a str)> {
    for current in required_array(manifest, "pages")? {
        if current.get("path").and_then(Value::as_str) == Some(page) {
            return Ok((current, "path"));
        }
    }
    for historical in manifest
        .get("historical")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        if historical.get("archived_path").and_then(Value::as_str) == Some(page) {
            return Ok((historical, "archived_path"));
        }
    }
    anyhow::bail!("wiki page is not declared by the live manifest")
}

fn wiki_page_text(root: &Path, page: &str) -> anyhow::Result<String> {
    let page = safe_wiki_relative_path(page)?;
    let page = page
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("wiki path is not UTF-8"))?;
    let (root, _, manifest) = load_wiki_manifest(root)?;
    let (entry, _) = manifest_page(&manifest, page)?;
    let expected_sha256 = required_string(entry, "sha256")?;
    anyhow::ensure!(valid_sha256(expected_sha256), "wiki page digest is invalid");
    let bytes = read_wiki_file(&root, page, MAX_WIKI_PAGE_BYTES)?;
    anyhow::ensure!(
        sha256_hex(&bytes) == expected_sha256,
        "wiki page changed outside its manifest"
    );
    let text = String::from_utf8(bytes)?;
    Ok(format!(
        "path: {page}\nstate: {}\nsha256: {expected_sha256}\n\n{text}",
        required_string(entry, "state")?
    ))
}

fn string_set(entry: &Value, key: &str) -> anyhow::Result<BTreeSet<String>> {
    required_array(entry, key)?
        .iter()
        .map(|value| {
            value
                .as_str()
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| anyhow::anyhow!("wiki entry has an invalid {key:?} value"))
        })
        .collect()
}

fn search_entry_for_path<'a>(search: &'a Value, page: &str) -> anyhow::Result<&'a Value> {
    required_array(search, "entries")?
        .iter()
        .find(|entry| entry.get("path").and_then(Value::as_str) == Some(page))
        .ok_or_else(|| anyhow::anyhow!("wiki page is not present in the search index"))
}

fn evidence_for_citations(
    search: &Value,
    citations: &BTreeSet<String>,
) -> anyhow::Result<BTreeSet<String>> {
    let mut evidence = BTreeSet::new();
    for entry in required_array(search, "entries")? {
        if !string_set(entry, "citations")?.is_disjoint(citations) {
            evidence.extend(string_set(entry, "evidence_ids")?);
        }
    }
    Ok(evidence)
}

fn wiki_evidence_text(root: &Path, evidence_id: &str) -> anyhow::Result<String> {
    anyhow::ensure!(
        !evidence_id.is_empty() && evidence_id.len() <= 1024,
        "evidence ID must be 1 to 1024 bytes"
    );
    let root = canonical_wiki_root(root)?;
    let search = load_wiki_search_index(&root)?;
    let mut matches = Vec::new();
    for entry in required_array(&search, "entries")? {
        if string_set(entry, "evidence_ids")?.contains(evidence_id) {
            matches.push(json!({
                "path": required_string(entry, "path")?,
                "citations": output_strings(entry, "citations", 256)?,
                "locators": output_strings(entry, "locators", 256)?,
                "evidence_ids": output_strings(entry, "evidence_ids", 256)?,
            }));
        }
    }
    anyhow::ensure!(
        !matches.is_empty(),
        "evidence ID is not published by this wiki"
    );
    json_text(json!({ "evidence_id": evidence_id, "matches": matches }))
}

fn validate_draft_body(body: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !body.trim().is_empty()
            && body.len() <= 64 * 1024
            && body
                .chars()
                .all(|character| !character.is_control() || matches!(character, '\n' | '\r' | '\t')),
        "draft section body is empty, too large, or contains a control character"
    );
    anyhow::ensure!(
        !body.contains("](")
            && !body.contains("][")
            && !body.contains("<!--")
            && !body.lines().any(|line| {
                let line = line.trim_start();
                line.starts_with('#') || (line.starts_with('[') && line.contains("]:"))
            })
            && !body.as_bytes().windows(2).any(|bytes| {
                bytes[0] == b'<'
                    && (bytes[1].is_ascii_alphabetic() || matches!(bytes[1], b'/' | b'!' | b'?'))
            }),
        "draft section body contains an uncontrolled heading, link, or HTML"
    );
    Ok(())
}

fn validate_draft_sections(draft: &str, allowed: &BTreeSet<String>) -> anyhow::Result<Vec<String>> {
    anyhow::ensure!(
        draft.len() <= MAX_WIKI_DRAFT_BYTES,
        "draft exceeds its byte limit"
    );
    let draft: Value = serde_json::from_str(draft)?;
    let object = draft
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("draft must be a JSON object"))?;
    anyhow::ensure!(
        object.len() == 1 && object.contains_key("sections"),
        "draft has unknown fields"
    );
    let sections = required_array(&draft, "sections")?;
    anyhow::ensure!(
        (1..=8).contains(&sections.len()),
        "draft has an invalid section count"
    );
    let mut headings = BTreeSet::new();
    let mut evidence = BTreeSet::new();
    for section in sections {
        let object = section
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("draft section must be a JSON object"))?;
        anyhow::ensure!(
            object.len() == 3
                && object.contains_key("heading")
                && object.contains_key("evidence_block_ids")
                && object.contains_key("body"),
            "draft section has unknown fields"
        );
        let heading = required_string(section, "heading")?;
        anyhow::ensure!(
            heading.len() <= 200
                && !heading.chars().any(char::is_control)
                && !heading.trim_start().starts_with('#')
                && !heading.eq_ignore_ascii_case("sources")
                && headings.insert(heading),
            "draft section heading is invalid or duplicated"
        );
        validate_draft_body(required_string(section, "body")?)?;
        let ids = string_set(section, "evidence_block_ids")?;
        anyhow::ensure!(
            !ids.is_empty()
                && ids.iter().all(|id| allowed.contains(id))
                && ids.iter().all(|id| evidence.insert(id.clone())),
            "draft section has unsupported or duplicate evidence block IDs"
        );
    }
    Ok(evidence.into_iter().collect())
}

fn wiki_validate_draft_text(root: &Path, page: &str, draft: &str) -> anyhow::Result<String> {
    let page = safe_wiki_relative_path(page)?;
    let page = page
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("wiki path is not UTF-8"))?;
    let root = canonical_wiki_root(root)?;
    let search = load_wiki_search_index(&root)?;
    let target = search_entry_for_path(&search, page)?;
    anyhow::ensure!(
        required_string(target, "kind")? == "article",
        "draft target is not an article"
    );
    let citations = string_set(target, "citations")?;
    anyhow::ensure!(!citations.is_empty(), "draft target has no citations");
    let evidence = evidence_for_citations(&search, &citations)?;
    let evidence_block_ids = validate_draft_sections(draft, &evidence)?;
    json_text(json!({
        "valid": true,
        "path": page,
        "sections": required_array(&serde_json::from_str::<Value>(draft)?, "sections")?.len(),
        "evidence_block_ids": evidence_block_ids,
        "draft_sha256": sha256_hex(draft.as_bytes()),
    }))
}

fn wiki_attest_review_text(
    root: &Path,
    plan_sha256: &str,
    capture_ids: &[String],
    draft: &str,
) -> anyhow::Result<String> {
    let (root, manifest_bytes, manifest) = load_wiki_manifest(root)?;
    anyhow::ensure!(
        valid_sha256(plan_sha256) && required_string(&manifest, "plan_sha256")? == plan_sha256,
        "review plan digest does not match the published wiki"
    );
    let captures = capture_ids.iter().cloned().collect::<BTreeSet<_>>();
    anyhow::ensure!(
        !captures.is_empty() && captures.len() == capture_ids.len(),
        "review capture IDs must be non-empty and unique"
    );
    let active = required_array(&manifest, "sources")?
        .iter()
        .map(|source| required_string(source, "citation").map(str::to_owned))
        .collect::<anyhow::Result<BTreeSet<_>>>()?;
    anyhow::ensure!(
        captures.is_subset(&active),
        "review includes a non-active capture ID"
    );
    let search = load_wiki_search_index(&root)?;
    let evidence = evidence_for_citations(&search, &captures)?;
    let evidence_block_ids = validate_draft_sections(draft, &evidence)?;
    json_text(json!({
        "version": 1,
        "plan_sha256": plan_sha256,
        "capture_ids": captures,
        "article_draft_sha256": sha256_hex(draft.as_bytes()),
        "evidence_block_ids": evidence_block_ids,
        "wiki_manifest_sha256": sha256_hex(&manifest_bytes),
    }))
}

pub fn serve() -> anyhow::Result<()> {
    serve_graph("graphoxide-out/graph.json")
}

pub fn serve_graph(graph_path: impl Into<PathBuf>) -> anyhow::Result<()> {
    let graph_path = graph_path.into();
    tokio::runtime::Runtime::new()?.block_on(async {
        let service =
            GraphoxideServer::with_default_graph(graph_path, http::max_server_contexts_from_env())
                .serve(rmcp::transport::stdio())
                .await?;
        service.waiting().await?;
        anyhow::Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn published_wiki_fixture() -> tempfile::TempDir {
        let temp = tempfile::tempdir().expect("temporary wiki root");
        let root = temp.path();
        let page = "---\ntitle: \"Operations overview\"\nkind: \"article\"\ndomain: \"operations\"\n---\n# Operations overview\n\nDefault username is `admin`; default password is `fake-password`.\n";
        let page_sha256 = sha256_hex(page.as_bytes());
        fs::create_dir_all(root.join("operations")).expect("wiki page directory");
        fs::write(root.join("operations/overview.md"), page).expect("wiki page");
        fs::write(
            root.join("wiki-manifest.json"),
            serde_json::to_vec_pretty(&json!({
                "version": 1,
                "registry": {
                    "catalog_id": "test-catalog",
                    "tree_sha256": "a".repeat(64),
                    "git_commit": "b".repeat(40),
                    "origin_id": "test-origin",
                    "policy": null
                },
                "graph_sha256": "c".repeat(64),
                "plan_sha256": "d".repeat(64),
                "sources": [{
                    "citation": "guide#capture-1",
                    "state": "source-ready",
                    "pages": ["operations/overview.md"]
                }],
                "pages": [{
                    "path": "operations/overview.md",
                    "sha256": page_sha256,
                    "state": "reviewed-ready"
                }],
                "historical": []
            }))
            .expect("manifest JSON"),
        )
        .expect("wiki manifest");
        fs::write(
            root.join("search.json"),
            serde_json::to_vec_pretty(&json!({
                "version": 1,
                "entries": [{
                    "path": "operations/overview.md",
                    "title": "Operations overview",
                    "aliases": ["operations"],
                    "kind": "article",
                    "domain": "operations",
                    "citations": ["guide#capture-1"],
                    "locators": ["guide.md:L4"],
                    "evidence_ids": ["evidence-1"],
                    "body": "Default username is admin; default password is fake-password."
                }]
            }))
            .expect("search JSON"),
        )
        .expect("search index");
        temp
    }

    #[test]
    fn published_wiki_tools_are_manifest_bound_and_preserve_knowledge_plane_text() {
        let temp = published_wiki_fixture();
        let root = temp.path();
        let draft = json!({
            "sections": [{
                "heading": "Defaults",
                "evidence_block_ids": ["evidence-1"],
                "body": "The documented defaults are available for this test system."
            }]
        })
        .to_string();

        assert!(wiki_status_text(root)
            .expect("wiki status")
            .contains("source-ready"));
        assert!(wiki_freshness_text(root)
            .expect("wiki freshness")
            .contains("reviewed-ready"));
        assert!(wiki_search_text(root, "fake-password", 1)
            .expect("wiki search")
            .contains("operations/overview.md"));
        assert!(wiki_page_text(root, "operations/overview.md")
            .expect("wiki page")
            .contains("fake-password"));
        assert!(wiki_evidence_text(root, "evidence-1")
            .expect("wiki evidence")
            .contains("guide#capture-1"));
        assert!(
            wiki_validate_draft_text(root, "operations/overview.md", &draft)
                .expect("wiki draft")
                .contains("\"valid\": true")
        );
        assert!(wiki_attest_review_text(
            root,
            &"d".repeat(64),
            &["guide#capture-1".into()],
            &draft,
        )
        .expect("wiki review")
        .contains("article_draft_sha256"));
        assert!(wiki_page_text(root, "../outside.md").is_err());
        fs::write(root.join("operations/overview.md"), "changed").expect("mutate wiki page");
        assert!(wiki_page_text(root, "operations/overview.md").is_err());
    }

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
            GraphoxideServer::wiki_status_tool_attr(),
            GraphoxideServer::wiki_freshness_tool_attr(),
            GraphoxideServer::wiki_search_tool_attr(),
            GraphoxideServer::wiki_get_page_tool_attr(),
            GraphoxideServer::wiki_get_evidence_tool_attr(),
            GraphoxideServer::wiki_validate_draft_tool_attr(),
            GraphoxideServer::wiki_attest_review_tool_attr(),
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
