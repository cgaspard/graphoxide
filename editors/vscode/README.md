# Graphoxide for VS Code

Graphoxide turns a repository into a navigable code knowledge graph. This extension brings extraction, architecture discovery, impact analysis, and source navigation into VS Code.

> **Origin and thanks.** Graphoxide grew from ideas pioneered by the
> [Graphify project](https://github.com/Graphify-Labs/graphify), created by Safi
> Shamsi and its contributors. Graphoxide is an independent Rust project, not
> an official Graphify release or affiliated project. See [NOTICE](NOTICE).

## Requirements

The extension requires VS Code 1.101 or newer for native MCP server discovery.

Marketplace and packaged VSIX builds include the native Graphoxide executable for
their target platform, so no separate CLI installation is required. When running
the extension from source, build `graphoxide` in the repository or make it
available on `PATH`:

```console
graphoxide --help
```

Alternatively, set **Graphoxide: Binary Path** to an absolute executable path. The extension has no Node runtime dependencies and its graph visualization loads no remote scripts or assets.

Binary discovery checks an explicit setting first, followed by the packaged
binary, `PATH`, and this repository's release/debug build directories.
Linux-targeted packages use a statically linked musl executable and therefore do
not inherit a glibc-version requirement from the GitHub Actions runner.

Tagged releases build and publish six VSIX packages for macOS, Linux, and
Windows on x64 and arm64. Each package stages the matching native executable,
records its version and target, includes the third-party license report, and is
verified before Marketplace publication. Extension-specific notes live in
[`../../releasenotes/vscode`](../../releasenotes/vscode); the CLI has parallel
notes under [`../../releasenotes/cli`](../../releasenotes/cli).

## Get started

1. Open a repository in VS Code.
2. At the first-open prompt, choose **Enable Graphoxide**.
3. Graphoxide builds or updates the local graph and registers its MCP server with VS Code.
4. Choose continuous watch, update-on-save, or manual freshness.
5. Open the **Graphoxide Control Center** from the status bar or Graph Explorer title to review graph health, automatic updates, AI labeling, and MCP connections in one place.
6. If Claude Code, Codex, or OpenCode is detected, optionally install Graphoxide at project or user scope from the Control Center.
7. Select the Graphoxide icon in the Activity Bar to explore communities, architectural hubs, files, and query results.

Enabling is workspace-specific and requires a trusted workspace. **Not now** asks
again on a future opening; **Don’t ask for this workspace** suppresses the prompt.
Run **Graphoxide: Reset Workspace Welcome** to restore it, or turn off
**Graphoxide: Prompt On First Open** globally. Existing graphs and external MCP
registrations are not deleted when managed mode is disabled.

By default, the extension reads `graphoxide-out/graph.json`. Configure **Graphoxide: Graph Path** if the graph lives elsewhere.

## Run the extension from source

The repository includes a one-click Extension Development Host configuration and an architecture-rich sample application.

1. Open the Graphoxide repository root in VS Code.
2. Open **Run and Debug** (`Cmd+Shift+D` on macOS or `Ctrl+Shift+D` elsewhere).
3. Select **Graphoxide: Run VS Code Extension**.
4. Press `F5`.

The sequential pre-launch task builds the release `graphoxide` binary, installs extension dependencies with `npm ci`, runs compilation/lint/tests, extracts `examples/vscode-sample`, and launches a new VS Code window with that sample open. The development host receives `target/release` on its `PATH`, so every extension command uses the binary you just built.

The sample README suggests queries and node pairs that exercise the explorer, visualization, CodeLens, source reveal, path finding, affected-node analysis, watcher, report, and export workflows. Its generated `graphoxide-out/` directory is intentionally ignored and is rebuilt before each launch.

## End-to-end testing

The E2E suite launches a clean VS Code Extension Development Host with a disposable
copy of the sample project:

```console
cargo build --release --locked --bin graphoxide
cd editors/vscode
npm ci
npm run test:e2e
```

It verifies executable discovery without a `PATH` dependency, managed extraction,
native MCP initialization, Codex usage instructions, tool metadata, project
overview, relation filtering, injected-instance call flows, active-group and
side-by-side graph placement, project MCP installation/removal for Claude Code,
Codex, and OpenCode, update-on-save, and continuous watch mode. User-level tool
configuration is never modified by the suite.

## Explore the graph

The Graph Explorer groups indexed code by community, architectural importance, and source file. Selecting a node opens the exact source line. Expanding communities reveals their member symbols, and the node context menu provides explanation and impact commands.

The interactive graph supports:

- A purple-first cinematic Constellation view with deterministic community territories
- A focused Investigation Lens with truthful incoming and outgoing relationships
- Recorded source-to-target arrows and confidence labels, glyphs, and line patterns that do not rely on color alone
- Pan and zoom
- Node search
- Community and relationship filters
- Node details, source navigation, and explanations
- Keyboard navigation, reduced motion, forced colors, and screen-reader status updates
- Automatic refresh when `graph.json` changes
- Configurable display limits for very large repositories

The graph opens in the active editor group by default, using the available editor
space. Run **Graphoxide: Open Interactive Graph Beside** only when you explicitly
want the source and graph side by side.

Only a deterministic, degree-ranked subset is drawn when a graph exceeds
**Graphoxide: Visualization Max Nodes**; the configured value is clamped between
25 and 5,000. The host also bounds the relationship payload sent to the webview,
and the renderer applies additional level-of-detail limits while preserving the
selected node and its immediate context. Omitted node and relationship counts are
shown rather than presented as an empty result. The full graph remains available
to sidebar and CLI commands.

The focused Lens describes generic graph direction precisely without assigning
unsupported caller, dependency, or effect semantics. It does not infer a risk
score or reconstruct source code. Relationship provenance is shown only when the
graph records it; known extracted, inferred, and ambiguous confidence values use
distinct encodings, with an explicit Unspecified fallback for any other value.
Arrows follow each relationship's recorded source and target facts, which
Graphoxide preserves even when the graph container uses undirected compatibility
semantics.

The visual hierarchy is anchored in Graphoxide purple for selection, focus,
active modes, and primary actions. Lavender supplies high-contrast highlights;
cyan and teal remain secondary signals for relationship direction and recorded
confidence, with matching arrows, patterns, glyphs, and text labels.

## Understand code in context

When **Graphoxide: Code Lens Enabled** is on, indexed symbols show their graph connection count directly above the source. Select that CodeLens—or right-click and choose **Graphoxide: Explain Symbol at Cursor**—to inspect the symbol and its neighbors.

Useful commands include:

| Command | Purpose |
| --- | --- |
| `Graphoxide: Build Graph` | Create the initial graph when this workspace has no graph file |
| `Graphoxide: Update Graph (Incremental)` | Refresh an existing valid graph while preserving its baseline |
| `Graphoxide: Rebuild Graph (Full)` | Rescan every supported input and replace an existing generated graph after confirmation |
| `Graphoxide: Query Graph` | Ask a natural-language structural question using offline graph traversal |
| `Graphoxide: Explain Node` | Describe a symbol and its immediate relationships |
| `Graphoxide: Find Path Between Nodes` | Find the shortest connection between two symbols |
| `Graphoxide: Show Affected Nodes` | Trace reverse dependencies and likely change impact |
| `Graphoxide: Show Architectural Hubs` | List the most connected nodes |
| `Graphoxide: Generate Architecture Report` | Create a Markdown architecture report |
| `Graphoxide: Export Graph…` | Export HTML, call-flow HTML, GraphML, Cypher, JSON, or an Obsidian vault |

Query results appear in a dedicated sidebar view. Node results link back to source. Text output also streams to **View → Output → Graphoxide**.

Keyboard shortcuts:

- Query graph: `Cmd+Shift+G Q` on macOS, `Ctrl+Shift+G Q` elsewhere
- Explain symbol at cursor: `Cmd+Shift+G E` on macOS, `Ctrl+Shift+G E` elsewhere

## Keep the graph current

Use **Graphoxide: Update Graph (Incremental)** for routine manual maintenance of
an existing valid graph. Use **Graphoxide: Rebuild Graph (Full)** when you need a
clean rescan, or when an existing `graph.json` cannot be loaded. A full rebuild
requires confirmation. If watch mode is running, Graphoxide stops it before the
rebuild and leaves it stopped so it cannot race the replacement build.

Managed workspaces remember one freshness policy. Continuous watch incrementally
rebuilds while the workspace is open, update-on-save performs a debounced update,
and manual mode changes nothing in the background. Use **Graphoxide: Configure
Automatic Updates…** to change the policy. The status bar shows an eye while watch
mode is active. Managed automatic refreshes accept intentional graph reductions,
so deleting or consolidating code cannot leave the graph permanently stale behind
the CLI shrink guard.

Builds and automatic updates honor `graphoxide.graphPath`. The configured path
must end in `graph.json` and place that file in a dedicated output directory; the
extension refuses to use the workspace root or one of its ancestors as that
output directory.

Place a `.graphoxideignore` file in the workspace or a nested directory to keep
generated or low-value files out of builds. It uses bounded Git-style patterns,
including `!` negation, and remains active when Git ignore handling is disabled.
Continuous watch snapshots ignore rules when it starts, so after changing an
ignore file, run **Graphoxide: Rebuild Graph (Full)** and start the watcher again.
The full syntax, precedence, and examples are documented in the repository
[`README`](../../README.md#exclude-generated-or-low-value-files).

For smaller projects, **Graphoxide: Update On Save** can run a debounced update whenever a source document is saved. It is disabled by default. Watch mode is more efficient for sustained editing sessions.

Graph build and update commands require a trusted workspace. All spawned commands
are argument-safe, run without a shell, support cancellation where applicable,
and stream diagnostics to the Graphoxide output channel.

## Improve community names with an LLM

Graphoxide can optionally ask an LLM to replace its deterministic community
names with concise architecture-oriented labels. Run **Graphoxide: Configure AI
Community Labeling…** and choose OpenAI, LM Studio, Ollama, a custom
OpenAI-compatible endpoint, or Anthropic. LM Studio defaults to
`http://127.0.0.1:1234/v1`, Ollama defaults to
`http://127.0.0.1:11434/v1`. Ollama can also use an explicitly configured LAN
HTTP endpoint, and it remains key-optional there. Local
model discovery uses LM Studio's OpenAI-compatible models endpoint or Ollama's
native tags endpoint; the model ID can always be entered manually.
LM Studio and Ollama also accept optional API keys. Each provider uses a
separate Secret Storage entry bound to its configured endpoint, and the key is
sent as Bearer authentication.
Both local providers use the same 600-second request timeout by default so a
cold model has time to load; adjust `graphoxide.llm.timeoutSeconds` when needed.
LM Studio label requests disable model reasoning because community
naming is a short structured-output task.

Then run **Graphoxide: Improve Community Names with AI**. Before sending
anything, the extension shows the endpoint, model, exact graph file, resolved
executable, and data disclosure for confirmation. The request contains up to 12
representative `node.label` values per community. Labels can include
source-derived identifiers, filenames, and truncated comments or docstrings.
Full files and `source_file` metadata are not included. The command replaces
community names in the selected `graph.json`, updates the adjacent label sidecar,
and regenerates `GRAPH_REPORT.md`; it does not perform semantic source extraction.

API keys are kept in VS Code Secret Storage, not settings. Provider, endpoint,
model, concurrency, batch size, and timeout are machine-scoped settings. Remote
endpoints must use HTTPS except for an explicitly configured Ollama endpoint.
Before labeling through non-loopback Ollama HTTP, the confirmation warns that
graph-derived labels and any optional key will travel without TLS. Remote model
discovery remains disabled, so enter the model ID manually. For this labeling
command, the extension ignores Binary Path,
Additional Arguments, `PATH`, and `GRAPHOXIDE_BINARY`, and invokes only its
packaged executable (or this repository's own build in an Extension Development
Host). Use **Graphoxide: Clear Stored AI Credential…** to delete a key.

## Connect an AI coding client

Graphoxide includes an MCP server in the same binary:

```console
graphoxide serve
```

For an enabled workspace, the extension publishes Graphoxide directly to VS Code
through its native MCP provider. Open **Graphoxide: Open Control Center** to detect
and configure other installed clients:

| Client | Project scope | User scope |
| --- | --- | --- |
| VS Code | Native provider for the enabled workspace | Managed by VS Code |
| Claude Code | `.mcp.json` | Claude Code user MCP registry |
| Codex | `.codex/config.toml` | `~/.codex/config.toml` |
| OpenCode | `opencode.json` | `~/.config/opencode/opencode.json` |

The Control Center separates project scope from all-projects user scope, reports
installed, missing, and stale registrations, and confirms every Install, Update,
or Remove action. It preserves unrelated servers and settings. Project
configuration can be shared with collaborators; user configuration applies
across projects.

The extension does not keep an MCP process running. VS Code, Claude Code, Codex,
or OpenCode starts `graphoxide serve` over stdio when it needs Graphoxide tools and
stops it according to that client's lifecycle. **Start MCP Server in Terminal
(Diagnostics)** is only for manual troubleshooting. **Copy MCP Configuration**
remains available for unsupported clients.

The MCP server tells Codex to use Graphoxide before broad filesystem searches for
architecture, navigation, call-flow, and impact questions. Its compact
`project_overview` tool is the recommended first call; focused graph queries can
then restrict traversal to call, import, type, or structural relationships. Tool
results are static-analysis evidence with source locations and confidence labels,
which Codex uses to synthesize and verify its final explanation.

Core extraction, clustering, querying, visualization, and reports are offline and require no API key. Explicit AI configuration may contact a loopback endpoint to discover models, and the labeling command contacts the confirmed model endpoint.

## Settings

Open **Graphoxide: Open Settings** to configure:

- Executable and graph paths
- Query token budget and BFS/DFS traversal
- Affected-node traversal depth
- Automatic graph refresh and update-on-save debounce
- First-open managed-workspace prompt
- Visualization node limit
- Source CodeLens
- Additional CLI arguments
- Output-channel reveal behavior
- AI community-labeling provider, endpoint, model, concurrency, batch size, and request timeout

Settings that identify files are scoped per workspace, so multi-root workspaces can use a different graph for each folder. AI endpoint and model settings are machine-scoped. Commands prefer the active editor's workspace and prompt when necessary.

## Troubleshooting

**The binary cannot be found:** Set **Graphoxide: Binary Path** to the absolute path returned by `which graphoxide` (macOS/Linux) or `where graphoxide` (Windows), then run **Refresh Graph**. AI labeling intentionally requires the packaged binary or a build from this repository and does not use that setting.

**The graph is empty or missing:** Run **Graphoxide: Build Graph**. If the repository already has a graph, verify **Graphoxide: Graph Path**.

**A graph fails to load:** Open **View → Output → Graphoxide** for the validation error, then use **Graphoxide: Rebuild Graph (Full)** if the generated file should be replaced. The parser accepts both built graphs using `links` and raw graphs using `edges`.

**A source node does not open:** Graph paths must be repository-relative. For safety, absolute paths and paths containing `..` are never opened from graph data.

## Privacy and security

The extension does not send complete source files over the network. Its visualizer uses a restrictive Content Security Policy, has no external runtime dependencies, and escapes graph content before rendering. The explicit AI-labeling command sends representative node labels and community IDs to the endpoint shown in its confirmation dialog; labels can contain identifiers, filenames, and truncated comments or docstrings, while full files and `source_file` metadata are excluded. Model discovery contacts only a configured loopback OpenAI-compatible, LM Studio, or Ollama endpoint. MCP clients may separately read graph-derived results when you enable their integration.

## License

Apache License 2.0. See [LICENSE](LICENSE), [LICENSE-MIT](LICENSE-MIT), and
[NOTICE](NOTICE).
