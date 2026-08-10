# CI and release dependencies

Every external GitHub Action used by this repository is pinned to a full
40-character commit SHA. The adjacent version comment records the reviewed
upstream release; tags and branches are not accepted as executable workflow
references. `scripts/workflow-dependencies.mjs` enforces this policy in local
verification and pull-request CI.

Dependabot checks GitHub Actions plus the root and VS Code npm lockfiles each
week. Its pull requests are review signals only: the repository does not
auto-merge dependency changes. Reviewers must verify the upstream release,
update the immutable SHA and version comment together, and run the normal
pre-push and release gates when release automation changes.

## Node and runner baseline

Workflow JavaScript actions use the Node 24 action runtime. GitHub-hosted
runners satisfy the requirement. The dedicated manual qualification runner
must run GitHub Actions Runner 2.327.1 or newer before it receives the
`graphoxide-qualification` label; older runners cannot execute Node 24 actions.

Graphoxide development and packaging continue to use Node 22 explicitly. The
VS Code extension's ESLint 10 flat configuration and TypeScript 5.9 toolchain
are tested against that runtime. This infrastructure update does not raise the
extension's declared VS Code compatibility floor.

## Known upstream warnings

The repository does not suppress deprecation diagnostics. The following warning
sources remain owned upstream as of this update:

- [`@vscode/vsce` 3.9.2](https://github.com/microsoft/vscode-vsce) still brings
  deprecated `whatwg-encoding` through Cheerio and `prebuild-install` through
  its optional Keytar integration. There is no newer supported `vsce` release
  that removes those transitive packages.
- The downloaded VS Code Extension Host and Agent Host can emit Node
  `DEP0169` independently of Graphoxide extension code. The upstream work is
  tracked in [microsoft/vscode#301941](https://github.com/microsoft/vscode/issues/301941)
  and [microsoft/vscode#328548](https://github.com/microsoft/vscode/issues/328548).
- `actions/download-artifact` 8.0.1 can emit Node `DEP0005` from the
  `@actions/artifact` extraction dependency. The action is already on its
  current Node 24 release, and the remaining warning is tracked in
  [actions/download-artifact#484](https://github.com/actions/download-artifact/issues/484).
- `actions/deploy-pages` 5.0.0 can emit Node `DEP0040` from its bundled
  `@actions/artifact` dependency during a successful deployment. No newer
  supported release removes it; the upstream fix is tracked in
  [actions/deploy-pages#434](https://github.com/actions/deploy-pages/issues/434)
  and [actions/deploy-pages#413](https://github.com/actions/deploy-pages/issues/413).

Action-owned Node 20, `punycode`, `Buffer()`, and `url.parse()` warnings other
than the exact upstream-owned exceptions above are not accepted; update or
replace the responsible action instead.

## Verification

Run the pin policy and repository gates before publishing workflow changes:

```bash
npm run test:workflows
npm run verify:pre-push
npm run verify:release
```

The pull request must then pass exact-head CI. After merge, verify exact-main CI
and any workflow-specific deployment, and inspect annotations for action-runtime
deprecations. Release workflow changes are not considered exercised by a product
tag until the release matrix, Marketplace publication, provenance attestation,
and GitHub release all succeed for that exact tagged commit.
