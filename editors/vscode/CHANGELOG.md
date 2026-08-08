# Changelog

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
