# Changelog

## 0.13.0 — 2026-08-29

- The bundled CLI now builds an incremental, provenance-bound LLM wiki without
  copying raw source files into the wiki or its Git-tracked registry. New
  `graphoxide wiki` commands (`plan`, `draft`, `render`, `materialize`,
  `check`, `index`, `openapi`) produce deterministic graph-derived Markdown with
  an `llms.txt`, a manifest, and a lexical search index; canonical rendering
  reads only graph, catalog metadata, and the reviewed plan.
- New `graphoxide registry` commands manage a Git-trackable, metadata-only
  Registry v1 tree: logical origins whose local locations never enter Git,
  deterministic discovery and scanning, capture publishing, freshness policies,
  lifecycle management, and append-only secret-free model-run records.
- Seven new read-only MCP tools expose wiki state to AI agents: `wiki_status`,
  `wiki_freshness`, `wiki_search`, `wiki_get_page`, `wiki_get_evidence`,
  `wiki_validate_draft`, and `wiki_attest_review`.
- New `graphoxide.registryBinding` workspace setting optionally binds a local
  origin from a validated Registry v1 tree to extract, index, update, and watch
  commands. Source locations and secrets remain outside the shared registry.

## 0.12.0 — 2026-08-25

- The bundled CLI now links IDL and schema type relationships across files:
  field references in GraphQL, Protobuf, Thrift, and other text-based IDLs
  resolve to the real declared types, so cross-file type/service relationships
  appear in the graph instead of dangling placeholders.
- IDL import/include edges (for example a Protobuf `import "common.proto"`)
  now resolve to the actual imported schema file.

## 0.11.0 — 2026-08-24

- The bundled CLI now indexes SQLite databases. It reads the file format
  directly — without opening a database or executing SQL — and shows tables,
  views, indexes, triggers, columns, and foreign-key relationships in your
  graph alongside source code.
- Recursive archive indexing now decodes four more codecs: BZIP2, XZ,
  Zstandard, and LZ4. Files and directories nested inside these archives (for
  example a diagram or a `.tar` inside a `.tar.bz2`) are extracted under the
  same bounded, deterministic limits as ZIP, GZIP, and TAR.

## 0.10.10 — 2026-08-22

- The bundled CLI now extracts a much wider range of real-world PDFs that were
  previously rejected or under-extracted: cross-reference streams with
  self-referencing entries, multi-megabyte files, tagged (structured) PDFs,
  JPEG/image streams, and ToUnicode CMaps that omit the optional `/Type /CMap`
  key.
- Exporting a report to a bare filename (no directory component) no longer
  fails with "No such file or directory".
- The bundled CLI now compiles for the Windows release targets; its shared
  dependencies were previously declared for unix only.

## 0.10.9 — 2026-08-20

- The in-editor "N graph connections" CodeLens pills are now off by default so
  open files stay uncluttered. Enable `graphoxide.codeLens.enabled` to bring
  them back.
- The bundled CLI no longer fails runtime AST cache persistence for Dart files
  that emit source-less concept nodes (Flutter/Bloc routes and annotations),
  which previously logged a provenance warning per affected file.

## 0.10.8 — 2026-08-16

- Builds now run silently with a live percentage in the status bar instead of modal
  progress popups. After a full rebuild completes while in watch mode, the watch
  process restarts automatically so incremental updates resume.
- The status bar now shows fine-grained sub-stage labels (Reconciling baseline,
  Merging nodes, Resolving edges, Deduplicating entities) during graph construction,
  and the Control Center "Latest index" card shows a "Build detail" line with
  per-sub-stage durations (reconcile, merge, dedup, topology).
- Restored the MCP install/update/remove UI in the Control Center, which was removed
  during the status-first redesign.
- Migrated the extension to TypeScript 6 with node16 module resolution.
- Clicking a graph node whose source lives inside an archive (e.g. charts/x.tgz!/
  templates/yml) now shows an information message with an "Open archive" action
  instead of a raw file:/// error. A new graphoxide.sourceLinks.enabled setting
  (default true) turns node source links off entirely.

## 0.10.7 — 2026-08-14

- Fixed Cancel button alignment in the Control Center build progress banner so it
  pushes to the right edge instead of sitting next to the phase label. Long phase
  text now truncates with ellipsis.

## 0.10.6 — 2026-08-13

- Redesigned the Control Center to a compact status-first layout replacing the chip
  overview with a single status line showing graph state, workspace mode, AI provider,
  and MCP count at a glance.
- Added inline build progress banner inside the graph card with spinner, phase label
  matching the status bar exactly, and Cancel button that terminates tracked child
  processes.
- Collapsed MCP integrations into inline pills; moved settings to side-by-side cards.

## 0.10.5 — 2026-08-13

- Fixed a stuck status-bar spinner during watch mode where terminal build events
  were dropped by the state machine, leaving progress visible indefinitely.
  Progress now clears unconditionally on every authenticated terminal event even
  when the state machine rejects it for mode mismatch after an adaptive start.

## 0.10.4 — 2026-08-12

- Added conditional build summary card to the Control Center after each successful
  index, showing exact node/edge counts, graph identity, and shrink authorization
  with bounded formatting and no filesystem paths.
- Reported indexed phases, node/edge counters, and completion evidence during managed
  builds and watch restarts through the bounded build-progress channel without
  inventing percentages or blocking the structural path.
- Accepted explicit LAN Ollama HTTP endpoints under opt-in confirmation that discloses
  the plaintext transport risk while rejecting link-local, metadata-range, and
  unspecified addresses automatically.
- Collapsed the Control Center into a focused dashboard flow with explicit card
  ordering, bounded content truncation, and semantic screen-reader sections.
- Repaired graph visualizer node overlap and edge routing in dense clusters by
  clamping layout iterations, reserving incident edges for selected nodes, and
  preserving cycle structure through deterministic passes.
- Replaced ad-hoc browser-test timeouts with a shared bounded process helper (45 s
  wall clock, completion-marker validation, diagnostic bounding) across all visualizer
  and Control Center harnesses.

## 0.10.3 — 2026-08-11

- Applied native macOS and Windows memory ceilings to the bounded automatic
  budget policy while preserving explicit overrides and the conservative
  fallback when no trustworthy ceiling is available.
- Classified confirmed MPEG transport streams named `.ts` through a bounded
  no-follow probe and cancellation-aware streaming instead of TypeScript
  parsing, while preserving ordinary and adversarial near-match TypeScript.
- Admitted loaded and pending manifest bytes before extraction and graph
  materialization, preserved the last committed graph and manifest on capacity
  or transition-verification failures, and repaired stale endpoints and
  hyperedges across TypeScript/media changes.
- Kept forced builds from authorizing runtime-cache replay and let a later
  ordinary build safely repair authorization before reuse.
- Coordinated activation resume, explicit graph commands, save refresh, and
  watch startup through one bounded non-queuing graph-mutation lifecycle.
- Paused automatic retries after a structural failure with one actionable
  diagnostic until an explicit successful command clears the failure for that
  output.
- Retained extension-owned process writers through child `close`, added finite
  watch readiness and stop deadlines, and quarantined unclosed watch children
  until exit is confirmed.

## 0.10.2 — 2026-08-10

- Rebuilt the graph visualizer as a purple-first cinematic explorer with a
  Constellation overview and a focused Investigation Lens.
- Preserved graph truth by presenting recorded incoming and outgoing
  relationships, exact known confidence, provenance, and community facts, and
  explicit `Unspecified` labels when the graph does not provide a known value.
- Added deterministic search, filters, density controls, pan and zoom, selection
  history, source reveal, and keyboard navigation across global and focused
  views.
- Bounded webview snapshots and rendering with deterministic node, relationship,
  detail, and string limits while disclosing omitted graph content.
- Improved screen-reader status, visible focus, reduced-motion behavior,
  forced-colors support, and redundant non-color relationship cues.
- Kept the visualizer local-only under a strict content security policy and
  cleared stale or malformed graph state instead of leaving obsolete content
  visible.

## 0.10.1 — 2026-08-10

- Redacted high-confidence credential-bearing values before generic structured
  facts enter bundled graph or reusable cache output, while retaining safe
  structure and explicit redaction markers.
- Retired exact pre-redaction AST and runtime cache payloads under the managed
  rebuild lock and stopped publication when the bounded migration is unsafe or
  incomplete.
- Upgraded `quick-xml` to 0.41.0 to remediate RUSTSEC-2026-0194 and
  RUSTSEC-2026-0195 while preserving bounded XML parsing behavior.
- Released bundled runtime-cache payload and non-returned transfer credits
  before signaling persist or non-hit completion while retaining returned-hit
  accounting until the caller drops the hit.
- Added locked dependency-advisory gates and least-privilege CodeQL analysis;
  issue #59 tracks the remaining Rust macro and data-flow coverage gap.

## 0.10.0 — 2026-08-09

- Bundled semantic Graphviz DOT extraction, recursive ZIP/TAR/GZIP indexing, and
  bounded provenance for supported PDF, OOXML, OpenDocument, and EPUB content.
- Added validated local parser-result reuse for eligible extension-managed
  builds while keeping default indexing offline and graph output deterministic.
- Included `graphoxide enrich` in the bundled executable as a CLI-only explicit
  opt-in; the extension never selects a profile, collects consent, or initiates
  provider requests.
- Qualified cold, warm, and incremental bundled indexing with a
  content-addressed CI corpus and server-enforced benchmark and qualification
  regression tests, without introducing timing thresholds.
- Hardened container, document, cache, and graph-identity boundaries while
  preserving nested source, page, package-part, and relationship provenance.

## 0.8.2 — 2026-08-08

- Bundled `graphoxide index`, which publishes a graph, incremental manifest, and
  deterministic coverage report associated with the exact accepted graph bytes.
- Hardened managed graph writes, cancellation, and untrusted ignore and output
  paths while preserving existing `extract` behavior.
- Recolored the Marketplace graph-cube icon with the shared purple and cyan
  palette while preserving its geometry and activity-bar icon.

## 0.8.1 — 2026-08-07

- Added deterministic file-coverage auditing through
  `graphoxide audit coverage`, including visible outcomes for unknown,
  extensionless, excluded, and unreadable files.
- Kept sensitive payloads unopened during coverage classification and exposed
  truthful format capabilities in root-relative human and JSON reports.

## 0.8.0 — 2026-08-05

- Bundled the bounded universal indexer with isolated I/O and compute lanes,
  deterministic resource controls, and opt-in runtime telemetry.
- Added truthful capability reporting and structural coverage for supported
  data, schema, diagram, engineering, simulation, and archive formats.
- Hardened recursive archive indexing against sensitive members, aggregate
  graph amplification, and unsafe or malformed content.

## 0.6.0 — 2026-08-05

- Fixed builds and updates failing outright in workspaces containing a
  `.vscode/mcp.json` file, whose server map lives at the document root under
  `servers` and was previously treated as a malformed MCP configuration.
- Indexed a `mcp.json` carrying no MCP server map as ordinary JSON instead of
  rejecting it, and reported files skipped during a build or update rather than
  failing the whole operation.
- Preserved the graph records of a file that stops extracting during an
  incremental update instead of reconciling them away.
- Promoted the 0.5 build, update, and full-rebuild commands, build telemetry,
  and incremental rebuilds to the stable channel.

## 0.5.0 — 2026-08-04

- Added explicit initial-build, incremental-update, and confirmed full-rebuild
  commands with consistent custom-output handling and workspace-trust guards.
- Added stable graph-build telemetry and elapsed timing to CLI-backed operations.
- Hardened live watch restarts when graph paths change and covered custom-output
  build, save, and watch transitions in Extension Host E2E tests.

## 0.4.5 — 2026-08-04

- Replaced the Marketplace icon with the web’s orange Graphoxide graph-cube mark
  and added clear gallery-safe padding around it.

## 0.4.4 — 2026-08-04

- Added a new Graphoxide identity across the Marketplace icon, activity bar, and
  website favicon.
- Made external MCP installation project-only, placing the current workspace at
  the top of each integration and retaining legacy global entries only for safe
  removal.
- Made project MCP registrations survive extension upgrades by persisting the
  bundled binary through VS Code global storage and repairing abandoned entries.

## 0.4.3 — 2026-08-03

- Fixed workspace graphification failures caused by valid JSON-with-comments
  files such as `.vscode/tasks.json`, `.vscode/launch.json`, and `tsconfig.json`.
- Added parser, CLI, MCP, and Extension Host regression coverage for comments
  and trailing commas, plus repo-relative diagnostics for malformed files.

## 0.4.2 — 2026-08-03

- Fixed workspace graphification failures caused by valid JSON-with-comments
  files such as `.vscode/tasks.json`, `.vscode/launch.json`, and `tsconfig.json`.
- Added parser, CLI, MCP, and Extension Host regression coverage for comments
  and trailing commas, plus repo-relative diagnostics for malformed files.

## 0.4.1 — 2026-08-03

- Added opt-in AI community naming for OpenAI, LM Studio, Ollama,
  OpenAI-compatible endpoints, and Anthropic, with local model discovery,
  Secret Storage credentials, explicit data disclosure, and a trusted-binary
  execution boundary.
- Added a unified Graphoxide Control Center for graph health, workspace freshness,
  AI configuration, trusted-executable status, and clearer project/user MCP
  installation management with confirmed changes.
- Added deterministic Extension Host E2E coverage for keyed LM Studio and keyless
  Ollama discovery and labeling. Local requests now allow ten minutes by default,
  and LM Studio disables reasoning for concise structured community names.

## 0.2.0

- Accounted for all 3,978 pinned Graphify v0.9.32 cases across the CLI and
  extension surface: 3,975 verified parity mappings and 3 reviewed expected
  divergences.
- Expanded multi-language extraction and resolution while preventing unsafe
  cross-runtime identity collisions.
- Added graph-first incremental updates, partial-build guards, portable caches,
  and deterministic stale-source pruning.
- Bundled the matching standalone binary and 133 generated agent integration
  artifacts in every platform-specific VSIX.
- Added a live Graphify-versus-Graphoxide differential release gate.

## 0.1.1

- JavaScript and TypeScript graphs now include exported declarations and
  variable-bound arrow, async, function-expression, and generator functions.
- Exported symbols carry export metadata, and newly recognized functions retain
  their file/class containment and call relationships.

## 0.1.0

- Initial Graphoxide VS Code integration.
- Added graph explorer, community and file browsing, architectural hubs, and linked query results.
- Added interactive local-only canvas visualization with graph filters and source navigation.
- Added extraction, incremental updates, watch mode, graph queries, paths, explanations, impact analysis, reports, and exports.
- Added graph-aware CodeLens, editor context actions, status bar state, multi-root support, and configurable update-on-save.
- Added MCP configuration and diagnostic server commands.
- Added trust-aware first-open onboarding that builds the graph, enables native
  VS Code MCP discovery, and remembers continuous, save-triggered, or manual
  freshness per workspace.
- Added MCP client detection and project/user installers for Claude Code, Codex,
  and OpenCode, with configuration status, safe updates, and removal.
- Interactive graphs now open in the active editor group by default, with a
  separate command for an intentional side-by-side view.
- Platform-specific VSIX packages now bundle the standalone Graphoxide executable;
  source checkouts also discover repository release and debug builds automatically.
- Added a real VS Code Extension Host E2E suite covering managed extraction, MCP,
  visualization placement, installers, update-on-save, and watch mode.
- Improved Codex tool selection with MCP server instructions, intent-based tool
  descriptions, parameter guidance, and read-only annotations.
- Added a compact `project_overview` MCP tool and functional call/import/type/
  structure filters for focused retrieval.
- Python call graphs now resolve typed constructor-injected fields and method
  parameters, including checkout calls through repositories, gateways, and
  notification services.
- Managed save and startup refreshes now accept intentional graph reductions;
  the CLI also exposes the previously documented `graphoxide update --force`.
- Added tag-driven Marketplace publishing for macOS, Linux, and Windows on x64
  and arm64, with the matching standalone executable verified inside every VSIX.
- Added component-specific release notes, binary provenance stamps, third-party
  license reports, checksums, and GitHub build attestations for release artifacts.
