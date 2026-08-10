# Semantic Atlas

Semantic Atlas treats a code graph as an architectural map rather than a cloud
of points. Communities become stable horizontal “territories,” while directed
relations flow from upstream entry points on the left toward dependencies on the
right. The result is intended for reading and tracing, not just exploring.

## Run it

From the repository root:

```bash
python3 -m http.server 4173 --directory design/graph-concepts
```

Open <http://localhost:4173/semantic-atlas/>. It also works when `index.html` is
opened directly because it has no network or build-time dependencies.

Run the dependency-free checks with:

```bash
node design/graph-concepts/shared/verify-fixture.mjs
node design/graph-concepts/semantic-atlas/verify.mjs
```

For a reproducible desktop capture, leave the server running and use any recent
Chromium/Chrome binary (replace the executable path for your machine):

```bash
/path/to/chrome --headless --hide-scrollbars \
  --window-size=1600,1000 --force-device-scale-factor=1 \
  --run-all-compositor-stages-before-draw --virtual-time-budget=1200 \
  --screenshot=semantic-atlas.png \
  http://127.0.0.1:4173/semantic-atlas/
```

The canonical comparison viewport is 1600 × 1000 at device scale 1 with the
default Overview mode and no selected node.

## Design thesis

- **Editorial hierarchy:** a compact masthead, numbered domain lanes, quiet map
  chrome, and a dedicated reading panel make the content feel deliberate.
- **Stable domain layout:** communities own horizontal lanes. Within each lane,
  a deterministic blend of graph order and inbound/outbound balance places
  producers toward the left and dependencies toward the right.
- **Semantic zoom:** Overview shows territories and hubs; Structure promotes
  readable node names; Detail adds source metadata and relation labels. The
  three named modes are also reached continuously with wheel zoom.
- **Trace before tangle:** hover, keyboard focus, or selection promotes one-hop
  paths and mutes the rest without destroying spatial context.
- **More than color:** node silhouette/glyph distinguishes code, data, config,
  and template artifacts. Stroke patterns distinguish relation families;
  opacity and the inspector state evidence confidence explicitly. Every edge is
  directed with an arrowhead.
- **Dense-graph tools:** domain, relation, and evidence filters work together;
  search ranks label, ID, path, and domain matches; a minimap preserves
  orientation during close inspection.
- **Accessible interaction:** SVG nodes are labeled, focusable tree items.
  Directional keys move spatially, Enter inspects, `F` searches, `+`/`-` zoom,
  `0` fits, and Escape clears. Focus is never encoded by color alone and system
  reduced-motion/high-contrast preferences are respected.

## Integration assumptions

The prototype reads the immutable
`globalThis.GRAPHOXIDE_GRAPH_FIXTURE` v1 contract from
[`../shared/fixture.js`](../shared/fixture.js). It uses exactly the transformed
fields already passed to the shipped VS Code webview: `id`, `label`, `file`,
`location`, `kind`, `community`, `communityName`, and `degree` on nodes, plus
`source`, `target`, `relation`, and `confidence` on edges.

No shipped UI, extension behavior, package metadata, or source-opening API is
changed. The source card and double-click interaction intentionally announce the
action in this standalone concept; a production port would send the existing
`{ type: "reveal", id }` webview message. Layout and filters remain local state
and never mutate the fixture.

## Deliberate boundaries

This concept optimizes for architecture comprehension. It is less spatially
compact than a force-directed map, and a production version would need lane
virtualization or aggregation above the current 750-node webview cap. Edge
labels therefore appear only at semantic detail zoom or on the active trace.
