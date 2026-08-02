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

## Get started

1. Open a repository in VS Code.
2. At the first-open prompt, choose **Enable Graphoxide**.
3. Graphoxide builds or updates the local graph and registers its MCP server with VS Code.
4. Choose continuous watch, update-on-save, or manual freshness.
5. If Claude Code, Codex, or OpenCode is detected, optionally open the MCP manager and install Graphoxide at project or user scope.
6. Select the Graphoxide icon in the Activity Bar to explore communities, architectural hubs, files, and query results.

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
native MCP initialization and tool calls, active-group and side-by-side graph
placement, project MCP installation/removal for Claude Code, Codex, and OpenCode,
update-on-save, and continuous watch mode. User-level tool configuration is never
modified by the suite.

## Explore the graph

The Graph Explorer groups indexed code by community, architectural importance, and source file. Selecting a node opens the exact source line. Expanding communities reveals their member symbols, and the node context menu provides explanation and impact commands.

The interactive graph supports:

- Community-aware layout and coloring
- Pan and zoom
- Node search
- Community and relationship filters
- Node details, source navigation, and explanations
- Automatic refresh when `graph.json` changes
- Configurable display limits for very large repositories

The graph opens in the active editor group by default, using the available editor
space. Run **Graphoxide: Open Interactive Graph Beside** only when you explicitly
want the source and graph side by side.

Only the highest-degree nodes are initially drawn when a graph exceeds **Graphoxide: Visualization Max Nodes**. The full graph remains available to sidebar and CLI commands.

## Understand code in context

When **Graphoxide: Code Lens Enabled** is on, indexed symbols show their graph connection count directly above the source. Select that CodeLens—or right-click and choose **Graphoxide: Explain Symbol at Cursor**—to inspect the symbol and its neighbors.

Useful commands include:

| Command | Purpose |
| --- | --- |
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

Managed workspaces remember one freshness policy. Continuous watch incrementally
rebuilds while the workspace is open, update-on-save performs a debounced update,
and manual mode changes nothing in the background. Use **Graphoxide: Configure
Automatic Updates…** to change the policy. The status bar shows an eye while watch
mode is active.

For smaller projects, **Graphoxide: Update On Save** can run a debounced update whenever a source document is saved. It is disabled by default. Watch mode is more efficient for sustained editing sessions.

All spawned commands are argument-safe, run without a shell, support cancellation where applicable, and stream diagnostics to the Graphoxide output channel.

## Connect an AI coding client

Graphoxide includes an MCP server in the same binary:

```console
graphoxide serve
```

For an enabled workspace, the extension publishes Graphoxide directly to VS Code
through its native MCP provider. Run **Graphoxide: Manage MCP Integrations…** to
detect and configure other installed clients:

| Client | Project scope | User scope |
| --- | --- | --- |
| VS Code | Native provider for the enabled workspace | Managed by VS Code |
| Claude Code | `.mcp.json` | Claude Code user MCP registry |
| Codex | `.codex/config.toml` | `~/.codex/config.toml` |
| OpenCode | `opencode.json` | `~/.config/opencode/opencode.json` |

The manager reports installed, missing, and stale registrations and offers an
explicit Install, Update, or Remove action. It preserves unrelated servers and
settings. Project configuration can be shared with collaborators; user
configuration applies across projects.

The extension does not keep an MCP process running. VS Code, Claude Code, Codex,
or OpenCode starts `graphoxide serve` over stdio when it needs Graphoxide tools and
stops it according to that client's lifecycle. **Start MCP Server in Terminal
(Diagnostics)** is only for manual troubleshooting. **Copy MCP Configuration**
remains available for unsupported clients.

Core extraction, clustering, querying, visualization, and reports are offline and require no API key. Optional community labeling is a separate CLI feature.

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

Settings that identify files are scoped per workspace, so multi-root workspaces can use a different graph for each folder. Commands prefer the active editor's workspace and prompt when necessary.

## Troubleshooting

**The binary cannot be found:** Set **Graphoxide: Binary Path** to the absolute path returned by `which graphoxide` (macOS/Linux) or `where graphoxide` (Windows), then run **Refresh Graph**.

**The graph is empty or missing:** Run **Extract Workspace**. If the repository already has a graph, verify **Graphoxide: Graph Path**.

**A graph fails to load:** Open **View → Output → Graphoxide** for the validation error. The parser accepts both built graphs using `links` and raw graphs using `edges`.

**A source node does not open:** Graph paths must be repository-relative. For safety, absolute paths and paths containing `..` are never opened from graph data.

## Privacy and security

The extension does not send source code or graph data over the network. Its visualizer uses a restrictive Content Security Policy, has no external runtime dependencies, and escapes graph content before rendering. Network access occurs only if you independently configure and invoke optional CLI labeling.

## License

Apache License 2.0. See [LICENSE](LICENSE), [LICENSE-MIT](LICENSE-MIT), and
[NOTICE](NOTICE).
