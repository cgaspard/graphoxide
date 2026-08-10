# Graph visualization concept fixture

The concept prototypes share one deterministic browser fixture so visual ideas can
be compared against the same topology. `fixture.js` has no runtime dependencies
and assigns a deeply frozen object to
`globalThis.GRAPHOXIDE_GRAPH_FIXTURE`.

The fixture mirrors the transformed data consumed by the current VS Code graph
webview, rather than the raw `graph.json` spelling:

```ts
interface ConceptGraphFixture {
  readonly contractVersion: 1;
  readonly fixtureId: string;
  readonly directed: boolean;
  readonly builtAtCommit: string;
  readonly nodes: readonly {
    id: string;
    label: string;
    file: string;
    location: string;
    kind: string;
    community: string;
    communityName: string;
    degree: number;
  }[];
  readonly edges: readonly {
    source: string;
    target: string;
    relation: string;
    confidence: "EXTRACTED" | "INFERRED" | "AMBIGUOUS";
  }[];
}
```

`degree` is calculated from the fixture edges when the script loads, which keeps
it correct while the sample topology evolves. Array order, IDs, and metadata are
stable. A concept must treat the object as read-only and keep any layout or UI
state in its own folder.

To use it from a standalone prototype:

```html
<script src="../shared/fixture.js"></script>
<script src="app.js"></script>
```

Run `node design/graph-concepts/shared/verify-fixture.mjs` after changing the
fixture. The verifier checks the contract, referential integrity, degrees,
community names, duplicate edges, and deep immutability.
