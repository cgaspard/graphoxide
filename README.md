# Graphoxide

Graphoxide turns an unfamiliar codebase into an explorable architecture map.
It traces symbols, calls, imports, inheritance, containment, and dependencies so
developers and coding agents can answer questions, follow execution paths, and
understand the impact of a change without repeatedly searching every source file.

Everything needed for structural analysis runs locally in one native executable:
no Python runtime, hosted service, vector database, or API key is required.

> [!NOTE]
> **Origin and thanks.** Graphoxide grew from ideas pioneered by
> [Graphify](https://github.com/Graphify-Labs/graphify), created by Safi Shamsi
> and the Graphify contributors. Graphoxide is an independent Rust
> implementation, not an official Graphify release or affiliated project.

Graphoxide stores its deterministic, queryable graph at
`graphoxide-out/graph.json`. The format retains compatibility with Graphify's
graph schema while adding a native CLI/MCP runtime and IDE integration.

## Install

Rust 1.97.1 is pinned in `rust-toolchain.toml`.

Download a standalone archive for macOS, Linux, or Windows from the
[latest GitHub release](https://github.com/cgaspard/graphoxide/releases/latest),
verify it against `SHA256SUMS`, and place `graphoxide` on your `PATH`. Native
x64 and arm64 builds are published for all three operating systems. Linux
archives and Linux-targeted VSIX packages use statically linked musl binaries,
so they do not depend on the host distribution's glibc version.

To build from source instead:

```bash
cargo build --release --workspace
./target/release/graphoxide --help
```

The release build produces one executable, `graphoxide`, for extraction, graph
operations, queries, exports, watch mode, hooks, integrations, and the MCP server.

Run the binary directly from `target/release`, or put that directory on your `PATH` for the current shell:

```bash
export PATH="$PWD/target/release:$PATH"
graphoxide --version
```

All examples below assume `graphoxide` is on `PATH`. You can replace the command with its absolute path.

## Quick start

```bash
cd /path/to/project
graphoxide index . --code-only
graphoxide query "where are calls resolved?"
graphoxide report
```

The first command creates an associated graph, incremental manifest, and file
coverage report beneath `graphoxide-out/`; subsequent commands read the graph by
default.

## Typical workflow

### 1. Index a project

Run the bounded indexing workflow against the repository you want to inspect:

```bash
cd /path/to/project
graphoxide index . --code-only
```

This walks the repository, extracts code relationships in parallel, builds and deduplicates the graph, runs Leiden clustering, and creates:

```text
graphoxide-out/
├── graph.json       # queryable knowledge graph
├── manifest.json    # incremental file state
├── coverage.json    # deterministic file outcomes associated with graph.json
└── cache/           # reusable extraction cache
```

The walker honors `.gitignore` and `.graphoxideignore`. It skips dependencies, build artifacts, credential files, and its own `graphoxide-out/` directory.

`graphoxide extract` remains compatible for scripts that only want the graph
build. `graphoxide index` uses the same graph path and adds the associated
coverage artifact.

Useful indexing variants:

```bash
# Discard cached extraction results and scan every supported file again
graphoxide index . --code-only --force

# Write the upstream-compatible raw extraction without build/clustering
graphoxide index . --code-only --no-cluster

# Emit one JSON report with build and coverage-association evidence
graphoxide index . --code-only --json

# Inspect the deterministic structured-format capability contract
graphoxide formats --json

# Report which project files are covered, inventory-only, or unsupported
graphoxide audit coverage . --json

# Bound an isolated build explicitly and write additive runtime telemetry
graphoxide index . --memory-budget-bytes 1073741824 \
  --compute-workers 4 --runtime-report graphoxide-out/runtime.json
```

`--force` also permits replacing an existing graph with a smaller one. Use it intentionally if files were removed or ignore rules changed.

Successful builds report elapsed time. Add `--timing` for human-readable stage
durations on stderr. `extract --json` and `update --json` retain their existing
single build-report object; `index --json` adds an outer object containing that
build report and the coverage path and graph digest. Timing is never written
into `graph.json`, so telemetry does not affect deterministic graph output.

The default executor separates filesystem I/O from CPU extraction behind
bounded queues and a resolved managed-memory budget. `--memory-budget-bytes`,
`--io-workers`, `--compute-workers`, `--read-batch-bytes`, and `--io-backend`
provide explicit overrides; unsupported `io-uring` requests fall back to the
portable threaded backend and record that decision in the optional runtime
report. It persists validated parser results under `cache/runtime-v1`; exact
path, content, extractor-version, and runtime-option evidence can avoid parsing
on a later build, while strong source-generation evidence can also avoid a
payload read. Unsafe, stale, incomplete, or corrupt entries are treated as
cache misses, and `--force` bypasses cache reads. The optional runtime report
records cache hits, misses, bypasses, rejected entries, and writes without
changing graph bytes. Cache frames are integrity-checked for accidental damage;
like other user-writable build outputs, they are not an authentication boundary
against a process running as the same user. Treat `graphoxide-out` as local
managed state: remove any copy received from an untrusted source before reusing
it. `--force` bypasses cache reads for the current build, but fail-open cache
write errors can leave older entries in place. `graphoxide formats --json`
reports each registered family's actual semantic, schema, structural,
container, or inventory-only support and its parser limits. The managed budget
governs Graphoxide's queues and registered format-parser allowances, admits
completed extraction facts before they enter the aggregate result, and bounds
caches and graph staging. It is not a hard process RSS limit; discovery and
language parsers retain their own fixed safety caps.

### 2. Query the graph

Ask questions from the project root so the default `graphoxide-out/graph.json` is found:

```bash
graphoxide query "where are HTTP requests authenticated?"
graphoxide query "how are calls resolved?" --budget 1000
graphoxide query "configuration loading" --dfs
```

`query` ranks matching symbols and traverses the connected neighborhood. The default is breadth-first traversal with a 2,000-token output budget; `--dfs` switches to depth-first traversal.

Every graph-reading command can target a graph outside the current directory:

```bash
graphoxide query "request validation" \
  --graph /path/to/project/graphoxide-out/graph.json
```

Labels, IDs, and source paths can identify nodes. If a label exists in several files, use the exact node ID shown by `query` or `explain`.

### 3. Inspect relationships and impact

```bash
# Find a deterministic shortest path
graphoxide path parse_request persist_result

# Show a node and its incoming/outgoing relationships
graphoxide explain parse_request

# Reverse-traverse dependency relationships
graphoxide affected parse_request --depth 3

# Limit impact traversal to selected relation types
graphoxide affected parse_request \
  --relation calls \
  --relation imports

# Rank architectural hubs
graphoxide god-nodes --top 20
graphoxide god-nodes --top 20 --json

# Render containment/import relationships
graphoxide tree
graphoxide tree parse_request --output graphoxide-out/tree.txt
```

Pass `--graph /path/to/graph.json` to any of these commands when operating outside the project root.

### 4. Keep the graph current

Use the incremental update after changing code:

```bash
graphoxide update .

# The same update report as structured JSON
graphoxide update . --json
```

If files or relationships were intentionally removed, allow the resulting graph
reduction with `graphoxide update . --force`. Managed IDE save/watch refreshes
use this authoritative mode automatically.

To rebuild community assignments without re-extracting source files:

```bash
graphoxide cluster-only .
```

For continuous updates, leave the native watcher running in a separate terminal:

```bash
graphoxide watch .
```

Code changes trigger a debounced rebuild. Non-code changes create `graphoxide-out/needs_update`; inspect that state with:

```bash
graphoxide check-update .
```

### 5. Generate human-readable output

```bash
# Architecture report
graphoxide report

# Choose a different source graph and output file
graphoxide report \
  --graph /path/to/graph.json \
  --output architecture.md

# Self-contained viewers and interchange formats
graphoxide export html graphoxide-out/graph.html
graphoxide export callflow-html graphoxide-out/callflow.html
graphoxide export graphml graphoxide-out/graph.graphml
graphoxide export cypher graphoxide-out/graph.cypher
graphoxide export obsidian graphoxide-out/vault
graphoxide export json graphoxide-out/copy.json
```

HTML outputs open directly in a browser. GraphML can be imported by graph tools, Cypher uses idempotent `MERGE` statements, and the Obsidian export creates a linked vault.

## Command reference

| Command | Purpose |
|---|---|
| `index <path>` | Build a graph and publish its associated deterministic file coverage |
| `extract <path>` | Extract, build, deduplicate, cluster, and write a graph |
| `audit [path]` | Report unresolved, malformed, merged, repaired, or dropped graph facts |
| `update [path]` | Incrementally refresh an existing project graph |
| `cluster-only <path>` | Recompute communities without source extraction |
| `query <question>` | Search and traverse a relevant neighborhood |
| `path <a> <b>` | Find the shortest relationship path between two nodes |
| `explain <node>` | Show node metadata and immediate relationships |
| `affected <node>` | Find downstream impact with reverse traversal |
| `god-nodes` | Rank the graph's most connected architectural hubs |
| `tree [root]` | Render containment and import relationships |
| `report` | Generate `GRAPH_REPORT.md` |
| `export <format> <output>` | Export HTML, call-flow HTML, GraphML, Cypher, Obsidian, or JSON |
| `benchmark <question>` | Measure repeated in-process query latency |
| `watch <path>` | Watch source files and rebuild after changes |
| `merge-graphs` | Combine explicitly named graph files |
| `global-graph` | Discover graphs beneath roots and merge them |
| `global` | Maintain the user-wide graph under `~/.graphoxide/` |
| `label [path]` | Optionally label communities through an LLM endpoint |
| `serve` | Run the MCP stdio server |
| `hook` / `claude` | Install, remove, or inspect integrations |

Run `graphoxide <command> --help` for the exact arguments and defaults of any command.

Use `graphoxide audit . --json` for a machine-readable conservation report. Add
`--strict` in CI to return a failure whenever extraction or graph construction
loses an unexplained node or edge; the report still prints before the command
exits so the exact reason counters and source findings remain available.

Use `graphoxide audit coverage . --json` for a deterministic, read-only file
coverage report. It includes unknown and extensionless files without adding them
to `graph.json`, records sensitive and policy-excluded paths without reading
their contents, and reports ignored or pruned boundaries separately. Coverage
`--strict` fails only when the scan is incomplete or an in-scope file is
unreadable; unsupported formats remain valid, visible outcomes. The report
published by `graphoxide index` additionally records the relative graph path and
SHA-256 of the exact accepted graph bytes.

## Supported source formats

Compiled tree-sitter extraction covers Python, JavaScript/JSX, TypeScript/TSX,
Go, Rust, Java, C, C++, Ruby, and C#. Bash and JSON currently use the
deterministic fallback tier while their grammar-backed adapters are hardened.

Deterministic structured/regex extraction covers the remaining offline matrix, including Kotlin, Scala, PHP, Swift, Lua, Groovy, Elixir, Zig, Julia, Fortran, Verilog/SystemVerilog, Objective-C, PowerShell, Terraform/HCL, SQL, Apex, Dart, Pascal, Blade/Razor, Visual Studio solutions/projects, XAML, Delphi/Lazarus forms, Vue/Svelte/Astro containers, and package manifests. Header routing distinguishes C++, C, and Objective-C markers.

The walker honors `.gitignore` and `.graphoxideignore`, skips dependency/build/cache directories and sensitive credential files, and never re-ingests `graphoxide-out/`. The `outer!/member` source spelling is reserved for logical archive members, so physical directory names ending in `!` are skipped with a discovery diagnostic.

## Deterministic graph-build benchmark profiles

The local graph-build benchmark always uses the default isolated execution model,
records the opt-in `--runtime-report` sidecar for both full and incremental
passes, and validates the resulting graph and manifest before reporting a
sample. It measures observations on the local machine; it does not assert a
throughput threshold.

In addition to the source compatibility corpus, the following generated profiles
exercise structured-format families with fixed content, canonical archive
metadata, and a fixture SHA-256 in the JSON report:

| Profile | Fixture families |
| --- | --- |
| `structured-json` | JSON, JSONC, JSON5, JSON Schema, and configuration documents |
| `structured-containers` | YAML, TOML, CSV, XML, SVG, ZIP, TAR, GZIP, and SVGZ |
| `idl-schema` | Protobuf, FlatBuffers, Thrift, Cap'n Proto, Avro, WIT, Smithy, YANG, ASN.1, CDDL, GraphQL, OpenAPI, and AsyncAPI |
| `diagrams` | Graphviz DOT, Mermaid, PlantUML, D2, draw.io, Excalidraw, tldraw, BPMN, DBML, and Structurizr DSL |
| `facility-models` | KiCad, EAGLE, gEDA, IPC-2581, IFC, IDS, gbXML, CityGML, LandXML, EnergyPlus, Modelica, OpenFOAM, OpenConfig, and Redfish |
| `openusd-assets` | USDA, USDC, USDZ, URDF, SDF, MJCF, glTF, GLB, MaterialX, OpenDRIVE, OpenSCENARIO, and FMI metadata |

Run a profile with the generic command or the matching npm shortcut:

```bash
npm --silent run benchmark:graph-build -- --scenario idl-schema
npm --silent run benchmark:structured-containers
```

The output includes the selected profile's format families, fixture digest,
validated isolated-runtime telemetry, full/incremental graph artifact digests,
and separate external-wall and CLI-reported elapsed timings. The timings remain
environment-specific evidence rather than release gates.

## VS Code extension

The bundled extension turns Graphoxide into an IDE-native architecture browser.
It provides an Activity Bar explorer, interactive graph canvas, community and
hub views, graph-aware CodeLens, source navigation, query results, impact/path
workflows, managed graph freshness, report/export commands, and MCP integration.

Install the packaged extension for your platform from this checkout, for example:

```bash
code --install-extension editors/vscode/graphoxide-vscode-darwin-arm64-0.8.2.vsix
```

Then open a repository and accept the first-open **Enable Graphoxide** prompt.
The extension builds the graph, registers Graphoxide as a native VS Code MCP
server, asks how to keep the graph fresh, and offers project/user installers for
detected Claude Code, Codex, and OpenCode clients. The extension uses
`graphoxide` from `PATH` by default; set **Graphoxide: Binary Path** if the
executable lives elsewhere. Platform-specific VSIX packages include the same
standalone native executable, so Marketplace users do not need a separate CLI
installation.

Release builds publish six target-specific VSIX packages to the
[VS Code Marketplace](https://marketplace.visualstudio.com/items?itemName=cgaspard.graphoxide-vscode)
and attach the exact packages to the GitHub release. Each package is verified to
contain `bin/graphoxide` (or `bin/graphoxide.exe`) before it can be published.

The interactive graph opens in the current editor group. Use **Open Interactive
Graph Beside** when a split view is preferred.

Extension source, settings, shortcuts, and development instructions are in
[editors/vscode/README.md](editors/vscode/README.md).

## Releases and release notes

The standalone CLI and VS Code extension share one synchronized version but
maintain separate release notes under [`releasenotes/cli`](releasenotes/cli)
and [`releasenotes/vscode`](releasenotes/vscode). A `v<version>` tag validates
both note files, builds native CLI archives and binary-bundled VSIX packages for
macOS/Linux/Windows x64 and arm64, publishes the VSIX packages to the Marketplace,
attaches all artifacts plus `SHA256SUMS` to GitHub, and records build provenance.

See [`releasenotes/README.md`](releasenotes/README.md) for the release process.

## MCP

Run `graphoxide serve` as a stdio MCP server. It exposes eleven tools:

`project_overview`, `query_graph`, `get_node`, `get_neighbors`, `get_community`, `god_nodes`, `graph_stats`, `shortest_path`, `list_prs`, `get_pr_impact`, and `triage_prs`.

It also exposes report, stats, god-node, surprise, audit, and question resources. Every tool accepts an optional `project_path`; graph contexts hot-reload through an eight-entry mtime/size LRU. PR tools use the installed `gh` CLI.

The server advertises Codex-oriented usage instructions, intent-based tool
descriptions, input guidance, and read-only annotations. For architecture work,
agents begin with `project_overview`, narrow structural questions with
`query_graph`, and follow up with exact node, neighbor, or path calls. The
optional `context_filter` accepts `call`, `import`, `type`, `structure`, or an
exact relation name. Graphoxide reports source-located static evidence; the
calling agent remains responsible for synthesizing an answer and verifying
runtime behavior.

Python extraction follows typed constructor injection and typed method
parameters. Calls such as `self.inventory.reserve(...)` can therefore resolve
to the method on `InventoryRepository`, with an `INFERRED` confidence label and
receiver/type context rather than being omitted from the call graph.

A generic MCP client configuration looks like this:

```json
{
  "mcpServers": {
    "graphoxide": {
      "command": "/absolute/path/to/graphoxide",
      "args": ["serve"]
    }
  }
}
```

The server uses `graphoxide-out/graph.json` beneath its working directory by default. MCP calls can pass `project_path` to query another repository without restarting the server. The PR tools additionally require an authenticated GitHub CLI (`gh auth status`).

To configure a project for Claude Code automatically:

```bash
cd /path/to/project
graphoxide claude install .
graphoxide claude status .
```

The installer preserves existing `CLAUDE.md` and `.claude/settings.json` content. Remove only Graphoxide-managed integration content with `graphoxide claude uninstall .`.

## Website

The dependency-free product site lives in [`website/`](website/) and is deployed
to GitHub Pages by `.github/workflows/deploy-pages.yml`. Preview it locally with:

```bash
graphoxide site website --port 8080
```

Then open <http://localhost:8080>. See [website/README.md](website/README.md) for
validation, Pages deployment, and future custom-domain instructions.

## Git hooks

Install project-local git hooks with:

```bash
graphoxide hook install .
graphoxide hook status .
```

This installs post-commit/post-checkout refresh hooks and configures the graph union merge driver while retaining pre-existing hook content. Use `graphoxide hook uninstall .` to remove the managed sections.

## Global and merged graphs

```bash
graphoxide merge-graphs a/graph.json b/graph.json --output merged.json
graphoxide global add project/graphoxide-out/graph.json --as project-a
graphoxide global list
graphoxide global path
graphoxide global remove project-a
```

Global graphs live under `~/.graphoxide/`, prefix project-owned IDs for isolation, merge shared source-less dependencies by normalized label, and retain repo/local-ID metadata.

For one-off discovery beneath several directory roots, use:

```bash
graphoxide global-graph ~/work ~/personal \
  --output .graphoxide-global/graph.json
```

## Optional LLM community labels

Offline clustering always provides deterministic hub labels. Richer labels can be requested through an OpenAI- or Anthropic-compatible endpoint:

```bash
export OPENAI_API_KEY=...
graphoxide label graphoxide-out/graph.json --backend openai --model gpt-4.1-mini
```

For a keyless Ollama server on the same computer:

```bash
GRAPHOXIDE_LLM_BASE_URL=http://localhost:11434/v1 \
  graphoxide label graphoxide-out/graph.json --backend ollama --model qwen2.5-coder:7b
```

For LM Studio (with an optional OpenAI-compatible Bearer key):

```bash
export OPENAI_API_KEY=... # omit for a keyless loopback server
GRAPHOXIDE_LLM_BASE_URL=http://127.0.0.1:1234/v1 \
  graphoxide label graphoxide-out/graph.json --backend lm-studio \
  --model qwen/qwen3.6-27b --timeout-seconds 600
```

Label requests default to a 600-second whole-request timeout so LM Studio and
Ollama have time to load a cold local model. Use `--timeout-seconds` to tune it.
The LM Studio backend disables model reasoning for this short structured task,
preventing reasoning-only responses from consuming the label output budget.

For Anthropic, set `ANTHROPIC_API_KEY` (and optionally `ANTHROPIC_BASE_URL`)
instead. `GRAPHOXIDE_LLM_PROVIDER=anthropic` selects Anthropic explicitly when
multiple provider variables are present. Add `--missing-only` only when you want
to preserve every existing non-placeholder community name; without it, all
communities are relabeled.

The VS Code extension exposes configuration, execution, and status through the
**Graphoxide Control Center**, with Secret Storage for keys and an explicit
data/endpoint confirmation.

Labeling sends up to 12 graph node labels per community. Labels can include
source-derived identifiers, filenames, and truncated comments or docstrings.
Full files and `source_file` metadata are not included. Structural extraction,
builds, queries, reports, and exports remain fully offline.

## Query logging

Query logging is disabled by default. Enable JSONL audit logging with:

```bash
export GRAPHOXIDE_QUERY_LOG_ENABLE=1
export GRAPHOXIDE_QUERY_LOG=/path/to/graphoxide-query.jsonl

# Include full responses only when explicitly needed
export GRAPHOXIDE_QUERY_LOG_RESPONSES=1
```

Questions and full responses may contain sensitive project context, so choose the log location and response setting deliberately.

## Troubleshooting

- **`graphoxide-out/graph.json` was not found:** run `graphoxide extract . --code-only` from the project root, change to that root, or pass `--graph /absolute/path/to/graph.json`.
- **A graph overwrite is refused:** Graphoxide's shrink guard detected fewer nodes than the existing graph. Inspect deleted files and ignore rules, then rerun `graphoxide update . --force` (or extraction with `--force`) if the reduction is expected.
- **A source file is missing:** check its extension, `.gitignore`, `.graphoxideignore`, and whether it is beneath a dependency/build/cache directory.
- **A node name is ambiguous:** query the name first and reuse the exact node ID or source path from the result.
- **An MCP PR tool fails:** install `gh`, authenticate with `gh auth login`, and run the server inside a Git repository or supply `repo`.
- **The graph needs updates after documentation changes:** run `graphoxide check-update .`, followed by `graphoxide update .` when prompted.

## Compatibility and verification

The Python reference implementation is kept in the gitignored `upstream/`
checkout as a differential oracle. The pinned 3,978-case inventory is accounted
for by 3,966 executable parity mappings and 12 reviewed expected divergences;
expected divergences are reported separately and never presented as blanket
parity. The end-to-end corpus gate also compares reviewed deterministic graphs
from both implementations.

```bash
cargo test --workspace --no-fail-fast
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

Measured results and methodology are in [BENCHMARKS.md](BENCHMARKS.md). Porting decisions and the compatibility contract are documented in [HANDOFF.md](HANDOFF.md).

## License

Graphoxide is distributed under the [Apache License 2.0](LICENSE). It retains
the upstream attribution and historical MIT notice required for the work from
which it was derived. See [NOTICE](NOTICE), [LICENSE-MIT](LICENSE-MIT), and
[DERIVATION.md](DERIVATION.md). Rust dependency notices are collected in
[THIRD_PARTY_LICENSES.html](THIRD_PARTY_LICENSES.html).
