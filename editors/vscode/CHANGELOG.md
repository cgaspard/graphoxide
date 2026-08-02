# Changelog

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
