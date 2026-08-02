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

The validator checks local asset references, in-page anchors, image alternative
text, and accidental remote script, stylesheet, or image dependencies.

## Deploy to GitHub Pages

The `deploy-pages.yml` workflow uploads this directory as a Pages artifact and
deploys it with the official GitHub Pages actions. In the repository settings,
select **GitHub Actions** as the Pages source. A deployment runs on changes to
`website/` pushed to `main`, or can be started manually.

When a custom domain is ready, add it in the repository's Pages settings. GitHub
will create or update `website/CNAME`; keep that file in version control afterward.

## Content notes

- Performance claims come from the repository's `BENCHMARKS.md` and include a
  visible qualification.
- Graphify attribution and licensing appear at the top and bottom of the page;
  the banner also states that Graphoxide is an independent, unaffiliated project.
- Repository links assume the project will live at
  `https://github.com/cgaspard/graphoxide`.
