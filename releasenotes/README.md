# Release notes

Graphoxide keeps separate user-facing notes for its two release surfaces:

- `releasenotes/cli/<version>.yaml` describes the standalone native CLI and MCP server.
- `releasenotes/vscode/<version>.yaml` describes the VS Code extension.

Both products currently ship from the same tag and must match the version in
`Cargo.toml` and `editors/vscode/package.json`. The release workflow requires
both files, combines them into one GitHub Release body, builds the standalone
archives, and publishes the target-specific VSIX packages to the VS Code
Marketplace.

## Schema

```yaml
version: 0.1.0
date: 2026-08-02
highlights:
  - Short user-facing summary.
added:
  - New capability.
changed:
  - Behavior change.
fixed:
  - Bug fix.
removed: []
```

The renderer intentionally supports only top-level scalar fields and lists of
plain strings. Keep each item on one line; nested YAML is not supported.

## Cutting a release

1. Set the same version in `[workspace.package]` in `Cargo.toml` and in
   `editors/vscode/package.json` (including its lockfile).
2. Add both `releasenotes/cli/<version>.yaml` and
   `releasenotes/vscode/<version>.yaml`.
3. Validate and preview the combined notes:

   ```bash
   node scripts/render-release-notes.mjs <version> --check
   node scripts/render-release-notes.mjs <version>
   ```

4. Commit the release, create `v<version>`, and push the tag. Odd minor versions
   publish to the Marketplace pre-release channel; even minor versions publish
   as stable. A semver suffix such as `-rc.1` remains GitHub-only.

The tag workflow attaches versioned and stable-name mirrors of all CLI and VSIX
assets plus `SHA256SUMS` to the GitHub Release.
