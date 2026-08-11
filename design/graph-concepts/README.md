# Graph visualization concept review

This directory contains three standalone, dependency-free visual directions for
the same deterministic Graphoxide fixture, plus a top-level chooser that makes
them easy to review side by side. None of these files changes the shipping VS
Code webview, Graphoxide graph contract, or package configuration.

## Run the review

From the repository root:

```bash
python3 -m http.server 4173 --directory design/graph-concepts
```

Open <http://127.0.0.1:4173/>. The chooser embeds one live prototype at a time
and links to each full-size concept. It also works from `file://` without network
resources.

Run the complete dependency-free static checks with:

```bash
node design/graph-concepts/verify.mjs
```

## Comparison summary

| Direction | Best at | Dense graphs | Investigation speed | Accessibility | Performance approach | Integration risk |
| --- | --- | --- | --- | --- | --- | --- |
| Cinematic Constellation | Whole-system discovery and impact storytelling | Good with edge budgets and focus fading; crossings remain | Good | Good keyboard model, but canvas is less semantic | Canvas culling, DPR cap, label LOD, 260/1,200/2,800 edge budgets | Low–moderate; closest to the current canvas renderer |
| Semantic Atlas | Architecture comprehension by domain | Good at domain scale; production needs SVG aggregation or virtualization | Fair | Strong focusable SVG and non-color encodings | Semantic LOD; future lane aggregation/virtualization | Moderate–high; renderer and layout model both change |
| Investigation Lens | Callers, effects, paths, and evidence around one symbol | Strong because the visible neighborhood is bounded | Strong | Strong semantic buttons and keyboard flow | Only 1–3 hops enter the DOM/SVG surface | Moderate; best as a second mode, and mock evidence must not be presented as truth |

## Recommendation

Use **Constellation as the global canvas**, add **Investigation Lens as a
selected-symbol mode**, and borrow **Semantic Atlas's confidence patterns,
artifact shapes, and semantic zoom vocabulary** across both. This recommendation
is based on task fit, bounded rendering, accessibility, and compatibility with
the current VS Code canvas—not aesthetics alone.

The pieces fit when they share a selected node, graph filters, source actions,
keyboard vocabulary, and the existing immutable node/edge contract. The two
geometries should remain distinct:

- Constellation clusters teach stable system geography; Atlas lanes teach a
  left-to-right architectural axis. Combining them in one map would make both
  harder to learn.
- Investigation Lens stays legible by excluding unrelated topology. Overlaying
  its cards on a whole-graph map defeats that constraint.
- The light editorial Atlas palette and dark operational concepts should be
  adapted to common VS Code theme tokens rather than mixed directly.
- Investigation Lens's risk score and source preview are deterministic mock
  presentation data. They need explicit provenance before production use.

The safest first production slice is Constellation's deterministic layout,
focus fading, and edge budgets behind the existing data/message bridge. A
bounded Lens can then be tested separately on real repositories at the current
750-node cap.

## Contents

- `index.html`, `comparison.css`, and `comparison.js`: live chooser and decision
  matrix.
- `shared/`: the immutable 42-node, 71-edge fixture used by every concept.
- `constellation/`: clustered canvas overview and impact tracing.
- `semantic-atlas/`: domain-lane SVG map and semantic zoom.
- `investigation-lens/`: bounded caller/effect columns and evidence workflow.
- `verify.mjs`: static/runtime-contract checks for the chooser, fixture, and all
  three concepts.
