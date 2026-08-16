# Graphoxide contributor guide

## Mission and scope

Graphoxide is a Rust workspace that produces deterministic code knowledge graphs,
a CLI/MCP server, and a bundled VS Code extension. Preserve graph compatibility,
determinism, bounded resource use, and safe handling of source material. Prefer
small, well-tested changes over broad rewrites.

## Repository map

- `crates/graphoxide-cli`: CLI, including `extract`, `update`, `serve`, and
  release-facing commands.
- `crates/graphoxide-extract`: language and file extractors.
- `crates/graphoxide-graph`: graph construction, clustering, cache, and output.
- `crates/graphoxide-mcp`: stdio and Streamable HTTP MCP transports.
- `editors/vscode`: TypeScript VS Code extension, including bundled-binary and
  MCP integration code.
- `parity`: Graphify differential fixtures and coverage contracts.
- `releasenotes/{cli,vscode}`: required release notes for each shipped version.

## Working rules

1. Inspect the focused code path before editing; use `rg` for source discovery.
2. Preserve unrelated working-tree changes. Never reset, checkout, or mass-stage
   files you did not create.
3. Treat extraction as untrusted-input processing: do not execute repository
   payloads, keep limits explicit, and retain useful diagnostics on malformed or
   unreadable inputs.
4. Keep structural indexing deterministic. Expensive AI/OCR/media enrichment
   must remain explicit and opt-in.
5. Do not alter a user's global MCP, profile, or credential configuration unless
   the requested feature explicitly requires it. Project MCP registrations must
   keep a project-specific working directory.
6. For a change touching extraction, graph shape, resolution, or caching, run the
   focused Rust tests and the applicable parity fixture before widening scope.
7. For a VS Code change, run the extension checks; run Extension Host E2E when
   commands, packaging, workspace behavior, or MCP installation changes.

## Verification

Run focused checks during development, then the repository gate before pushing:

```bash
npm run verify:pre-push
```

That gate runs Rust format, Clippy, workspace tests, VS Code compile/lint/unit
tests, agent-artifact validation, and current-version release-note validation.

Before tagging a release, additionally run:

```bash
npm run verify:release
```

This adds the release build, VS Code Extension Host E2E suite, and VSIX package
validation. CI also runs the Graphify differential parity suite and website and
license checks; do not tag until CI for the exact main-branch commit is green.

### Rust code coverage ratchet

Executed Rust code coverage is measured separately from the indexing pipeline's
file-admission/corpus "coverage". The ratchet keeps a committed baseline
(`scripts/rust-coverage-baseline.json`) of line, region, and function coverage
and fails if any metric regresses beyond a small tolerance. It exercises the same
unit and integration tests as the gates.

The full measurement requires the `cargo-llvm-cov` tool (not installed in the
default CI toolchain), so it runs locally:

```bash
node scripts/rust-coverage.mjs --check   # measure + enforce the ratchet
node scripts/rust-coverage.mjs --update  # re-baseline after an intentional drop
```

The fast ratchet *logic* test (`scripts/rust-coverage.test.mjs`) runs in every
gate via `verify.mjs`. Re-baseline only when a change intentionally lowers
coverage; the baseline otherwise acts as a guard against unexplained regressions.

## Issues, branches, and worktrees

Create the GitHub issue first. Name every issue worktree and its branch from the
issue number using this exact convention:

```text
####-issue-description
```

For example, issue 42 is `42-universal-capability-indexing`, with branch
`agent/42-universal-capability-indexing`, stored beneath the sibling directory
`/Users/cgaspard/Projects/cgaspard/graphoxide-worktrees/42-universal-capability-indexing`
(relative to this checkout: `../graphoxide-worktrees/42-universal-capability-indexing`).
Use lowercase, hyphenated descriptions. One issue owns one branch and one
worktree; do not use the main checkout for feature work.

## Release process

1. Select the next semantic version and update both `[workspace.package]` in
   `Cargo.toml` and `editors/vscode/package.json`/`package-lock.json`.
2. Add `releasenotes/cli/<version>.yaml` and
   `releasenotes/vscode/<version>.yaml`, then run
   `node scripts/render-release-notes.mjs <version> --check`.
3. Update the VS Code changelog and any versioned install examples.
4. Run `npm run verify:release`, commit only the intended release files, and push
   main. Wait for the exact-SHA CI run to pass.
5. Create and push the annotated `v<version>` tag on that exact commit. The tag
   workflow validates release notes, builds all platform artifacts, publishes the
   Marketplace channel, and creates the GitHub release. Confirm the workflow and
   release assets before announcing availability.

Odd minor versions publish to the VS Code Marketplace prerelease channel; even
minor versions publish as stable. Version suffixes are GitHub-release only.

## Hooks

Run `npm install` once at the repository root after cloning to install Husky's
versioned hooks. The pre-push hook invokes `npm run verify:pre-push`. An emergency
bypass is `HUSKY=0 git push`, but it must be followed by documented CI validation.
