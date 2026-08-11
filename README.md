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
report. Without an explicit memory override, Graphoxide uses one eighth of the
detected host or process memory ceiling, capped at 8 GiB; if the platform cannot
report a trustworthy ceiling, it falls back to 512 MiB. It persists validated
parser results under `cache/runtime-v2`; exact
path, content, extractor-version, and runtime-option evidence can avoid parsing
on a later build, while strong source-generation evidence can also avoid a
payload read. Unsafe, stale, incomplete, or corrupt entries are treated as
cache misses. `--force` reparses every selected source and publishes no runtime
cache authorization; the next ordinary build reparses and safely repairs those
entries before they can be reused. The optional runtime report
records cache hits, misses, bypasses, rejected entries, and writes without
changing graph bytes. Cache frames are integrity-checked for accidental damage;
like other user-writable build outputs, they are not an authentication boundary
against a process running as the same user. Treat `graphoxide-out` as local
managed state: remove any copy received from an untrusted source before reusing
it. Older cache files may remain after a forced build, but its committed manifest
does not authorize them for replay. `graphoxide formats --json`
reports each registered family's actual semantic, schema, structural,
container, or inventory-only support and its parser limits. The managed budget
governs Graphoxide's queues and registered format-parser allowances, admits
completed extraction facts before they enter the aggregate result, and bounds
caches and graph staging. It is not a hard process RSS limit; discovery and
language parsers retain their own fixed safety caps.

Because `.ts` is shared by TypeScript and MPEG transport streams, Graphoxide
uses a fixed-size packet-header probe before classification. Confirmed media is
classified as video and represented by a schema-compatible inventory-only media
node. It is never admitted to the TypeScript parser or JavaScript resolver;
ordinary and near-match `.ts` source remains code.

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
| `enrich [path]` | Explicitly add bounded, provider-authored facts from a named enrichment profile |
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

Generic structured extraction applies a bounded, deterministic redaction policy
before retained facts enter the graph or extraction caches. JSON/JSONC/JSON
Lines, TOML, XML, delimited tables, INI/properties/environment-style files,
named JSON configuration references, and MCP configuration facts recognize a
narrow set of credential-bearing keys and high-confidence value signatures.
Redacted scalars keep their key, location, container shape, original scalar
type, and an explicit `<redacted>` marker; secret-like labels and references use
safe labels and identifiers. YAML and JSON5 structural fallbacks retain keys,
not decoded scalar values. Literal sensitive paths such as `.env` remain
excluded before open, while an ordinary selected path such as `app.env` uses
the same bounded key/value policy.

The policy covers common password, authorization, token, API-key, private-key,
credential, connection-string, secret-assignment, credentialed-URL, JWT,
Basic/Bearer, and recognized provider-token forms. It deliberately has no
entropy heuristic and is not general-purpose secret discovery; unrecognized
values under ordinary keys can remain visible. Source files are never modified.
Cache schema 30 invalidates pre-redaction AST facts, moves current framed cache
storage to `cache/runtime-v2`, and, under the exclusive rebuild lock, erases
exact legacy AST artifacts and retired `cache/runtime-v1` payloads before any
new build can publish. Unsafe or busy legacy cache layouts stop the build rather
than being followed or ignored. A failed migration leaves the previously
accepted graph untouched; the first successful rebuild replaces it with the
redacted graph. Remove `graphoxide-out` manually when immediate removal of all
previously published local output is required.

ZIP, TAR, and single-member GZIP inputs recursively index supported member
formats in the default isolated runtime. Members stay in memory, receive stable
`outer!/member` provenance, and are never extracted to the filesystem. Archive
paths, nesting, member counts, decoded bytes, expansion ratios, retained facts,
and compressed scratch are bounded; sensitive members remain visible as inert
inventory without being decompressed. BZIP2, XZ, Zstandard, 7z, and RAR remain
inventory-only. Encrypted members, links, special entries, and unsupported ZIP
compression reject that archive before any child dispatch.

DOCX, XLSX, PPTX, ODT, ODS, ODP, and EPUB packages use a dedicated byte-only
ZIP route before generic archive recursion. The bounded parser emits ordered
sections, sheets, slides, or publication spine entries together with their
backing package parts and contained internal relationships. It enforces
independent package, member, XML nesting/event, decoded-byte, retained-model,
relationship, text, string, and fact ceilings and returns one stable rejection
diagnostic on malformed or hostile input. External relationships, formulas,
signatures, fonts, media, and unreferenced opaque members remain inert and are
never fetched, evaluated, rendered, or dispatched as source files. Recognized
macro, script, OLE/ActiveX, and encrypted package structures reject the package
before semantic publication.

PDF inputs use an in-process, byte-only parser for bounded classic-xref
documents. It supports raw or single-Flate page streams and a conservative
Type 1 Standard-14/WinAnsi text subset, emitting deterministic document and
page facts with page-numbered provenance under explicit input, object, page,
decoded-stream, text, and fact ceilings. Encrypted, incremental, hybrid-xref,
object-stream, unsupported-font/filter, malformed, or over-limit PDFs retain a
stable inventory diagnostic instead of attempting unbounded recovery. Actions,
annotations, embedded files, external references, images, and JavaScript are
never traversed or executed; OCR and rendering remain explicit future
enrichment work. The isolated runtime's shared parser arena applies an
additional 16× source admission, so its current 16 MiB per-file policy admits
semantic PDF parsing only below roughly 1 MiB even though the registry's
absolute PDF input ceiling is 16 MiB.

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

## Reproducible universal-indexing qualification

The qualification runner exercises explicit cold, warm, and one-file
same-size incremental modes against the strict content-addressed
`ci-mixed-v1` corpus. It retains raw samples, failures, runtime telemetry v2,
coverage outcomes, streaming artifact digests, and machine/storage environment
evidence in one atomic report. Performance observations are never CI
thresholds.

```bash
cargo build --release --locked --bin graphoxide
stage_parent="$(mktemp -d "${TMPDIR:-/tmp}/graphoxide-qualification-binary.XXXXXX")"
chmod 700 "$stage_parent"
stage_parent="$(realpath "$stage_parent")"
staged_binary="$stage_parent/graphoxide"
install -m 0700 target/release/graphoxide "$staged_binary"
staged_binary="$(realpath "$staged_binary")"
STAGED_BINARY="$staged_binary" node --input-type=module -e \
  'import { lstatSync } from "node:fs"; const value = lstatSync(process.env.STAGED_BINARY); if (!value.isFile() || value.isSymbolicLink() || value.nlink !== 1) throw new Error("staged binary must be a single-link regular file")'
cmp -s target/release/graphoxide "$staged_binary"
npm run qualification:ci -- \
  --binary "$staged_binary" \
  --report "$(pwd -P)/qualification-ci.json"
```

The runner never overwrites a report or corpus target and retains each
exclusive qualification project for inspection. It deliberately requires a
single-link binary so the reported content identity cannot alias another
pathname; Cargo artifacts may be hard-linked, so operator and CI commands use
`install` to make and verify a byte-copy in a new private directory. The
optional exact 70 GiB
profile and Linux controlled-OS-cold mode require separate acknowledgement and
safety arguments. See
[`benchmarks/universal/README.md`](benchmarks/universal/README.md) for the corpus
layout, evidence ceilings, mode semantics, storage preflights, helper boundary,
main-only manual workflow, and manual cleanup contract.

## VS Code extension

The bundled extension turns Graphoxide into an IDE-native architecture browser.
It provides an Activity Bar explorer, interactive graph canvas, community and
hub views, graph-aware CodeLens, source navigation, query results, impact/path
workflows, managed graph freshness, report/export commands, and MCP integration.

Install the packaged extension for your platform from this checkout, for example:

```bash
code --install-extension editors/vscode/graphoxide-vscode-darwin-arm64-0.10.2.vsix
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

## Explicit opt-in media transcript summaries

`graphoxide enrich` is separate from deterministic indexing, updates, and watch
mode. Those offline workflows never invoke an enrichment profile, inspect an
enrichment sidecar, require an API key, or make a provider request. List the
available profiles without provider configuration:

```bash
graphoxide enrich --list-profiles --json
```

The initial `media-transcript-summary-v1` profile summarizes text that the user
has already supplied for an indexed audio or video file. For a media source such
as `media/briefing.mp4`, place its UTF-8 transcript at:

```text
.graphoxide/enrichment-input/media/briefing.mp4.transcript.txt
```

This profile does not perform transcription, OCR, media decoding, rendering, or
uploading. It verifies that the media path is a live regular project file and an
existing graph inventory fact, but never reads or sends the media payload. The
only provider data boundary is the redacted transcript text.

Every outbound run requires all provider fields and the exact consent token on
the command line. The API key remains in the named environment variable:

```bash
export GRAPHOXIDE_ENRICHMENT_KEY=...

graphoxide enrich . \
  --profile media-transcript-summary-v1 \
  --provider openai-compatible \
  --endpoint https://provider.example/v1 \
  --model bounded-summary-model \
  --api-key-env GRAPHOXIDE_ENRICHMENT_KEY \
  --consent send-redacted-transcript-text \
  --json
```

There is no environment-variable, persisted-setting, or auto-detection shortcut
for profile selection or consent. Provider endpoints must use HTTPS; plain HTTP
is permitted only for verified loopback addresses. Credentials, query strings,
and fragments in the endpoint are rejected, redirects are not followed, and the
isolated client does not inherit proxy settings.

The command validates the complete candidate set before its first request. A run
admits at most 32 transcripts, 64 KiB per transcript, and 1 MiB in total. It
rejects traversal, sensitive paths, non-UTF-8 input, symlinks, hard links, and
non-regular files. `redaction-v1` normalizes newlines and replaces the selected
credential, recognized credential patterns, and bounded secret-like environment
values before request construction. The validated provider output passes through
the same redaction boundary before logs, cache records, or graph facts. Requests
cap model output at 512 tokens; response bodies are capped at 16 KiB. Rate-limit
responses can retry once, only with a `Retry-After` no greater than 30 seconds,
and requests remain paced by `--requests-per-minute`. Per-request timeout and
graceful cancellation are explicit, while the whole run is bounded to 15
minutes.

Validated results use a dedicated cache beneath
`.graphoxide/enrichment-cache/v1/`; they never enter the structural extraction
cache. Cache identity binds the provider, canonical endpoint, model, redacted
input digest, and selected API credential; strict records carry a keyed integrity
tag. Unsafe cache namespace or parent links fail closed; an unsafe final cache
entry is an inert miss and can be replaced only after the graph commits. Graph
nodes carry `_origin: "enrichment"`, the profile, model, redaction and
input-digest evidence, the `redacted_transcript_text_only` boundary, and
`verification: "unverified_model_output"`; `has_enrichment` edges retain the
source and profile linkage. Repeating the same source/profile
replaces that profile's prior fact rather than accumulating stale summaries.
Publication uses a graph digest compare-and-swap and one atomic write, so a
failure, cancellation, or concurrent graph update does not partially replace the
accepted graph. The existing rebuild-lock marker remains coordination state and
may be refreshed by an attempt that later fails; it contains no transcript or
provider content.

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
for by 3,965 executable parity mappings and 13 reviewed expected divergences;
expected divergences are reported separately and never presented as blanket
parity. The end-to-end corpus gate also compares reviewed deterministic graphs
from both implementations.

```bash
cargo test --workspace --no-fail-fast
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

Benchmark methodology and the raw observation schema are in [BENCHMARKS.md](BENCHMARKS.md). Porting decisions and the compatibility contract are documented in [HANDOFF.md](HANDOFF.md).
The immutable GitHub Actions, update, runner, and known-upstream-warning policy
is documented in [docs/ci-release-dependencies.md](docs/ci-release-dependencies.md).
Locked Cargo/npm advisory scans and the time-bounded exception process are
documented in [docs/security-scanning.md](docs/security-scanning.md).

## License

Graphoxide is distributed under the [Apache License 2.0](LICENSE). It retains
the upstream attribution and historical MIT notice required for the work from
which it was derived. See [NOTICE](NOTICE), [LICENSE-MIT](LICENSE-MIT), and
[DERIVATION.md](DERIVATION.md). Rust dependency notices are collected in
[THIRD_PARTY_LICENSES.html](THIRD_PARTY_LICENSES.html).
