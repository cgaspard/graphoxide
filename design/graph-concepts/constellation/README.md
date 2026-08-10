# Cinematic constellation

A standalone graph-visualization direction for Graphoxide. It treats a codebase
as a navigable system landscape: communities become faint luminous territories,
high-degree symbols become stars, and directed impact paths become animated
signals.

This is an isolated design prototype. It does not change the shipping VS Code
webview or package configuration.

## Try it

Open `index.html` in a browser. It works from `file://` and does not fetch any
fonts, scripts, or other network resources. For a local server instead:

```bash
python3 -m http.server 4173 --directory design/graph-concepts
```

Then visit <http://localhost:4173/constellation/>. A repeatable selected/trace
state is available at:

```text
http://localhost:4173/constellation/?selected=checkout-service&trace=1
```

On macOS, a reproducible 1440 × 960 review image can be captured with:

```bash
"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" \
  --headless=new --hide-scrollbars --window-size=1440,960 \
  --run-all-compositor-stages-before-draw --virtual-time-budget=1500 \
  --screenshot=/tmp/graphoxide-constellation.png \
  "http://localhost:4173/constellation/?selected=checkout-service&trace=1"
```

## What this direction is testing

- **Spatial comprehension.** A stable, seeded layout and community hulls make it
  easier to remember where subsystems live between visits.
- **Focus over decoration.** Ambient light is quiet until hover or selection.
  Unrelated nodes and edges recede; neighbors remain legible.
- **Impact as a first-class action.** “Trace impact” follows directed outgoing
  relationships for three hops and uses restrained moving particles to show
  direction.
- **Progressive density.** Focus, Balanced, and Complete modes apply explicit
  edge budgets (260 / 1,200 / 2,800). Focus also keeps only the strongest half of
  a small overview until a symbol is active. Relevant edges are sorted ahead of
  each budget, labels use zoom-and-degree level of detail, canvas work is viewport
  culled, trace motion is capped near 30 fps, and the device pixel ratio is capped
  at 2.
- **Useful graph controls.** Community and relationship filters, ranked search,
  pan, cursor-centered zoom, fit, neighbor navigation, and an inspector all run
  against the same topology.
- **Keyboard and motion access.** The canvas has a documented keyboard model:
  arrow keys move spatially, Enter selects, T traces, R resets, and Escape clears.
  Search is available with `/`, focus is visible, changes are announced through
  a live region, forced-colors has a fallback, and reduced-motion removes pulses
  and moving path particles.

## Production integration assumptions

The prototype consumes the current transformed VS Code visualizer contract from
`../shared/fixture.js`: nodes already include `degree`, normalized file/location
fields, and community labels; edges use source/target IDs, relation, and
confidence. Production can continue deriving that payload in
`editors/vscode/src/visualizer.ts`.

The canvas renderer is dependency-free and CSP-friendly. A production port would
move the standalone JS/CSS into the extension's webview template, replace the
prototype action notices with the existing `reveal` and `explain` messages, keep
the existing `visualization.maxNodes` cap, persist view state with VS Code's
webview state API, and benchmark edge budgets against real large graphs before
choosing defaults.

## Validation

```bash
node --check design/graph-concepts/constellation/app.js
node design/graph-concepts/shared/verify-fixture.mjs
git diff --check
```
