# Graphoxide website

The product site is a dependency-free static site. It uses local HTML, CSS,
JavaScript, and SVG only; no build step, remote font, analytics script, or CDN
asset is required.

## Preview locally

From the repository root:

```bash
graphoxide site website --port 8080
```

The same native Graphoxide binary serves the static site locally; no separate
web runtime is needed. Then open <http://localhost:8080>.

## Validate

With Node.js 18 or newer:

```bash
node website/scripts/validate.mjs
```

The validator checks both product and knowledgebase pages for local asset
references, in-page anchors, image alternative text, and accidental remote
script, stylesheet, or image dependencies.

## Deploy to GitHub Pages

The `deploy-pages.yml` workflow uploads this directory as a Pages artifact and
deploys it with the official GitHub Pages actions. In the repository settings,
select **GitHub Actions** as the Pages source. A deployment runs on changes to
`website/` pushed to `main`, or can be started manually.

When a custom domain is ready, add it in the repository's Pages settings. GitHub
will create or update `website/CNAME`; keep that file in version control afterward.

## Content notes

- `BENCHMARKS.md` describes the measurement methodology. Do not add performance
  numbers without current, reproducible evidence.
- Graphify attribution and licensing appear at the top and bottom of the page;
  the banner also states that Graphoxide is an independent, unaffiliated project.
- The deployed site is <https://cgaspard.github.io/graphoxide/> and the source
  repository is <https://github.com/cgaspard/graphoxide>.
- When publishing a new stable release, update the release link and pinned
  installation examples in `index.html` and `app.js`. Keep development builds
  distinct from released downloads; odd minor versions use the Marketplace
  prerelease channel and even minor versions use stable.
- `knowledgebase.html` documents the direct-source workflow: bootstrap, add,
  refresh, explicit review, status, retirement, local preview, and MCP
  capabilities. Keep its examples provider-neutral and never put credentials,
  source bodies, or private filesystem roots in the static site.
