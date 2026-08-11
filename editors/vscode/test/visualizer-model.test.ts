import assert from 'node:assert/strict';
import test from 'node:test';
import { GraphEdge, GraphModel, GraphNode, GraphSnapshot } from '../src/graph';
import {
  assertVisualizerSnapshot,
  buildVisualizerSnapshot,
  DEFAULT_VISUALIZER_NODE_LIMIT,
  isKnownGraphConfidence,
  isVisualizerSnapshot,
  isVisualizerWebviewMessage,
  MAX_VISUALIZER_EDGE_LIMIT,
  MAX_VISUALIZER_NODE_LIMIT,
  MAX_VISUALIZER_SNAPSHOT_STRING_CODE_UNITS,
  MAX_VISUALIZER_STRING_CODE_UNITS,
  MIN_VISUALIZER_EDGE_LIMIT,
  MIN_VISUALIZER_NODE_LIMIT,
  normalizeVisualizerNodeLimit,
  visualizerEdgeLimit,
} from '../src/visualizer-model';

const noAttributes: Readonly<Record<string, unknown>> = Object.freeze({});

function graphNode(
  id: string,
  overrides: Partial<Omit<GraphNode, 'id' | 'attributes'>> = {},
): GraphNode {
  return {
    id,
    label: overrides.label ?? id,
    fileType: overrides.fileType ?? 'code',
    sourceFile: overrides.sourceFile ?? `src/${id}.ts`,
    ...(overrides.sourceLocation === undefined ? {} : { sourceLocation: overrides.sourceLocation }),
    ...(overrides.community === undefined ? {} : { community: overrides.community }),
    ...(overrides.communityName === undefined ? {} : { communityName: overrides.communityName }),
    attributes: noAttributes,
  };
}

function graphEdge(
  source: string,
  target: string,
  relation = 'calls',
  overrides: {
    confidence?: string;
    sourceFile?: string;
    sourceLocation?: unknown;
  } = {},
): GraphEdge {
  const attributes: Record<string, unknown> = { source, target, relation };
  if (overrides.sourceLocation !== undefined) attributes.source_location = overrides.sourceLocation;
  return {
    source,
    target,
    relation,
    ...(overrides.confidence === undefined ? {} : { confidence: overrides.confidence }),
    ...(overrides.sourceFile === undefined ? {} : { sourceFile: overrides.sourceFile }),
    attributes,
  };
}

function graphSnapshot(nodes: readonly GraphNode[], edges: readonly GraphEdge[]): GraphSnapshot {
  return { nodes, edges, directed: true, builtAtCommit: 'abc123' };
}

test('normalizes node and edge budgets deterministically', () => {
  assert.equal(normalizeVisualizerNodeLimit(), DEFAULT_VISUALIZER_NODE_LIMIT);
  assert.equal(normalizeVisualizerNodeLimit(Number.NaN), DEFAULT_VISUALIZER_NODE_LIMIT);
  assert.equal(normalizeVisualizerNodeLimit(Number.POSITIVE_INFINITY), DEFAULT_VISUALIZER_NODE_LIMIT);
  assert.equal(normalizeVisualizerNodeLimit(-20), MIN_VISUALIZER_NODE_LIMIT);
  assert.equal(normalizeVisualizerNodeLimit(25.9), MIN_VISUALIZER_NODE_LIMIT);
  assert.equal(normalizeVisualizerNodeLimit(999_999), MAX_VISUALIZER_NODE_LIMIT);
  assert.equal(visualizerEdgeLimit(0), MIN_VISUALIZER_EDGE_LIMIT);
  assert.equal(visualizerEdgeLimit(750), 3_000);
  assert.equal(visualizerEdgeLimit(5_000), MAX_VISUALIZER_EDGE_LIMIT);
});

test('produces identical snapshots for permuted node and edge input', () => {
  const nodes = [
    graphNode('β', { label: 'Βήτα', community: '2', communityName: 'Runtime' }),
    graphNode('a', { label: 'Alpha', community: '1', communityName: 'Gateway' }),
    graphNode('z', { label: 'Zulu', community: '1', communityName: 'Gateway' }),
    graphNode('m', { label: 'Middle' }),
  ];
  const edges = [
    graphEdge('a', 'β', 'calls', { confidence: 'EXTRACTED', sourceFile: 'src/a.ts', sourceLocation: 'L7' }),
    graphEdge('z', 'a', 'imports', { confidence: 'INFERRED' }),
    graphEdge('β', 'm', 'references', { confidence: 'AMBIGUOUS' }),
    graphEdge('m', 'a', 'custom_relation'),
  ];
  const options = { selectedNodeId: 'm', nodeLimit: 25 } as const;
  const first = buildVisualizerSnapshot(new GraphModel(graphSnapshot(nodes, edges)), options);
  const second = buildVisualizerSnapshot(graphSnapshot([...nodes].reverse(), [edges[2]!, edges[0]!, edges[3]!, edges[1]!]), options);

  assert.deepEqual(second, first);
  assert.equal(first.nodes[0]?.id, 'm');
  assert.deepEqual(first.relations.map((facet) => facet.value), ['calls', 'custom_relation', 'imports', 'references']);
  assert.deepEqual(first.confidences.map((facet) => facet.value), ['EXTRACTED', 'INFERRED', 'AMBIGUOUS', null]);
  assert.equal(isVisualizerSnapshot(first), true);
  assert.equal(isVisualizerSnapshot(JSON.parse(JSON.stringify(first)) as unknown), true);
});

test('preserves extracted edge endpoints when the graph container is undirected', () => {
  const snapshot = buildVisualizerSnapshot({
    ...graphSnapshot([graphNode('caller'), graphNode('callee')], [graphEdge('caller', 'callee')]),
    directed: false,
  });

  assert.equal(snapshot.directed, false);
  assert.deepEqual(
    snapshot.edges.map((edge) => ({ source: edge.source, target: edge.target })),
    [{ source: 'caller', target: 'callee' }],
  );
});

test('clamps 5000 nodes and 100000 edges to the hard serialized limits', () => {
  const nodes = Array.from({ length: 5_000 }, (_, index) => graphNode(`n${String(index).padStart(4, '0')}`, {
    community: String(index % 80),
    communityName: `Community ${index % 80}`,
  }));
  const confidenceValues = ['EXTRACTED', 'INFERRED', 'AMBIGUOUS', undefined] as const;
  const edges = Array.from({ length: 100_000 }, (_, index) => graphEdge(
    nodes[index % nodes.length]!.id,
    nodes[(index * 17 + 31) % nodes.length]!.id,
    `relation_${index % 13}`,
    {
      ...(confidenceValues[index % confidenceValues.length] === undefined
        ? {}
        : { confidence: confidenceValues[index % confidenceValues.length] }),
      sourceFile: `src/evidence-${index % 97}.ts`,
      sourceLocation: `L${index % 1_000 + 1}`,
    },
  ));

  const snapshot = buildVisualizerSnapshot(graphSnapshot(nodes, edges), { nodeLimit: 999_999, selectedNodeId: 'n4999' });
  assert.equal(snapshot.limits.nodes, MAX_VISUALIZER_NODE_LIMIT);
  assert.equal(snapshot.limits.edges, MAX_VISUALIZER_EDGE_LIMIT);
  assert.equal(snapshot.nodes.length, MAX_VISUALIZER_NODE_LIMIT);
  assert.equal(snapshot.edges.length, MAX_VISUALIZER_EDGE_LIMIT);
  assert.equal(snapshot.counts.totalEdges, 100_000);
  assert.equal(snapshot.counts.eligibleEdges, 100_000);
  assert.equal(snapshot.counts.omittedEdgesByEdgeLimit, 88_000);
  assert.equal(snapshot.counts.omittedEdges, 88_000);
  assert.ok(snapshot.edges.every((edge) => edge.source !== undefined && edge.target !== undefined));
  assertVisualizerSnapshot(snapshot);
});

test('keeps malicious and Unicode source strings as inert serializable data', () => {
  const hostile = '</script><img src=x onerror="globalThis.pwned=1">\u2028雪💜';
  const nodes = [
    graphNode('α<script>', {
      label: hostile,
      fileType: 'document<svg>',
      sourceFile: 'docs/<unsafe>&雪.md',
      sourceLocation: '§3.1',
      community: '<community>',
      communityName: '紫の領域',
    }),
    graphNode('target'),
  ];
  const edges = [
    graphEdge('α<script>', 'target', 'calls</script>', {
      confidence: 'UNKNOWN<script>',
      sourceFile: 'src/<edge>.ts',
      sourceLocation: 'L9<script>',
    }),
    graphEdge('target', 'α<script>', 'object-location', { sourceLocation: { line: 9 } }),
  ];
  const snapshot = buildVisualizerSnapshot(graphSnapshot(nodes, edges));
  const roundTrip = JSON.parse(JSON.stringify(snapshot)) as typeof snapshot;

  assert.equal(roundTrip.nodes.find((node) => node.id === 'α<script>')?.label, hostile);
  assert.equal(roundTrip.edges.find((edge) => edge.relation === 'calls</script>')?.sourceLocation, 'L9<script>');
  assert.equal(roundTrip.edges.find((edge) => edge.relation === 'object-location')?.sourceLocation, null);
  assert.equal(Object.hasOwn(roundTrip.nodes[0]!, 'attributes'), false);
  assert.equal(Object.hasOwn(roundTrip.edges[0]!, 'attributes'), false);
  assert.equal(Object.hasOwn(roundTrip.nodes[0]!, 'risk'), false);
  assertVisualizerSnapshot(roundTrip);
});

test('rejects an oversized exposed string with a fixed non-echoing diagnostic', () => {
  const oversized = `secret:${'x'.repeat(MAX_VISUALIZER_STRING_CODE_UNITS)}`;
  const graph = graphSnapshot([graphNode(oversized)], []);

  assert.throws(
    () => buildVisualizerSnapshot(graph),
    (error: unknown) => error instanceof Error
      && error.message === 'Graph data exceeds the visualizer per-string safety limit'
      && !error.message.includes('secret'),
  );

  const valid = buildVisualizerSnapshot(graphSnapshot([graphNode('safe')], []));
  const invalidGuardPayload = {
    ...valid,
    nodes: [{ ...valid.nodes[0]!, label: oversized }],
  };
  assert.equal(isVisualizerSnapshot(invalidGuardPayload), false);
  assert.equal(isVisualizerWebviewMessage({ type: 'reveal', id: oversized }), false);
});

test('rejects a snapshot whose cumulative string values exceed the aggregate budget', () => {
  const repeatedLabel = 'x'.repeat(
    Math.floor(MAX_VISUALIZER_SNAPSHOT_STRING_CODE_UNITS / DEFAULT_VISUALIZER_NODE_LIMIT) + 1,
  );
  assert.ok(repeatedLabel.length < MAX_VISUALIZER_STRING_CODE_UNITS);
  const oversizedNodes = Array.from(
    { length: DEFAULT_VISUALIZER_NODE_LIMIT },
    (_, index) => graphNode(`n${index}`, { label: repeatedLabel, sourceFile: 'src/file.ts' }),
  );

  assert.throws(
    () => buildVisualizerSnapshot(graphSnapshot(oversizedNodes, [])),
    { message: 'Graph data exceeds the visualizer cumulative string safety limit' },
  );

  const valid = buildVisualizerSnapshot(graphSnapshot(
    oversizedNodes.map((node) => ({ ...node, label: 'safe' })),
    [],
  ));
  const invalidGuardPayload = {
    ...valid,
    nodes: valid.nodes.map((node) => ({ ...node, label: repeatedLabel })),
  };
  assert.equal(isVisualizerSnapshot(invalidGuardPayload), false);
});

test('preserves cycles and parallel facts while counting missing values and invalid endpoints truthfully', () => {
  const nodes = [graphNode('a'), graphNode('b'), graphNode('c')];
  const edges = [
    graphEdge('a', 'b', 'calls', { confidence: 'EXTRACTED', sourceLocation: 12 }),
    graphEdge('a', 'b', 'references'),
    graphEdge('b', 'a', 'calls', { confidence: 'INFERRED' }),
    graphEdge('a', 'a', ''),
    graphEdge('a', 'missing', 'dangling', { confidence: 'AMBIGUOUS' }),
  ];
  const snapshot = buildVisualizerSnapshot(graphSnapshot(nodes, edges));
  const a = snapshot.nodes.find((node) => node.id === 'a');
  const b = snapshot.nodes.find((node) => node.id === 'b');

  assert.deepEqual({ degree: a?.degree, in: a?.inDegree, out: a?.outDegree }, { degree: 5, in: 2, out: 4 });
  assert.deepEqual({ degree: b?.degree, in: b?.inDegree, out: b?.outDegree }, { degree: 3, in: 2, out: 1 });
  assert.equal(snapshot.edges.length, 4);
  assert.equal(snapshot.edges.filter((edge) => edge.source === 'a' && edge.target === 'b').length, 2);
  assert.equal(snapshot.edges.find((edge) => edge.relation === 'references')?.confidence, null);
  assert.equal(snapshot.edges.find((edge) => edge.relation === 'calls')?.sourceLocation, '12');
  assert.equal(snapshot.counts.totalEdges, 5);
  assert.equal(snapshot.counts.validEdges, 4);
  assert.equal(snapshot.counts.invalidEndpointEdges, 1);
  assert.equal(snapshot.counts.omittedEdges, 1);
  assertVisualizerSnapshot(snapshot);

  const singleEdge = buildVisualizerSnapshot(graphSnapshot(
    [graphNode('source'), graphNode('target')],
    [graphEdge('source', 'target')],
  ));
  const source = singleEdge.nodes.find((node) => node.id === 'source');
  const target = singleEdge.nodes.find((node) => node.id === 'target');
  assert.deepEqual({ in: source?.inDegree, out: source?.outDegree }, { in: 0, out: 1 });
  assert.deepEqual({ in: target?.inDegree, out: target?.outDegree }, { in: 1, out: 0 });
});

test('reserves a selected node and its incident edges inside a community scope', () => {
  const nodes = Array.from({ length: 42 }, (_, index) => graphNode(`n${String(index).padStart(2, '0')}`, {
    community: index < 40 ? 'core' : 'other',
    communityName: index < 40 ? (index === 0 ? 'Core' : 'Core Systems') : 'Other',
  }));
  const edges: GraphEdge[] = [];
  for (let index = 0; index < 30; index += 1) {
    edges.push(graphEdge('n00', `n${String(index + 1).padStart(2, '0')}`, 'calls', { confidence: 'EXTRACTED' }));
  }
  edges.push(graphEdge('n39', 'n00', 'selected_relation', { confidence: 'AMBIGUOUS' }));
  edges.push(graphEdge('n39', 'n40', 'cross_scope'));

  const snapshot = buildVisualizerSnapshot(graphSnapshot(nodes, edges), {
    communityId: 'core',
    nodeLimit: 25,
    selectedNodeId: 'n39',
  });

  assert.deepEqual(snapshot.scope, { kind: 'community', id: 'core' });
  assert.equal(snapshot.selectedNodeId, 'n39');
  assert.equal(snapshot.nodes[0]?.id, 'n39');
  assert.ok(snapshot.nodes.some((node) => node.id === 'n00'));
  assert.equal(snapshot.edges[0]?.relation, 'selected_relation');
  assert.equal(snapshot.counts.totalNodes, 42);
  assert.equal(snapshot.counts.scopedNodes, 40);
  assert.equal(snapshot.counts.includedNodes, 25);
  assert.equal(snapshot.counts.omittedNodesByScope, 2);
  assert.equal(snapshot.counts.omittedNodesByLimit, 15);
  assert.equal(snapshot.counts.omittedEdgesByScope, 1);
  assert.equal(snapshot.counts.selectedIncidentEdges, 1);
  assert.equal(snapshot.counts.includedSelectedIncidentEdges, 1);
  assert.deepEqual(snapshot.communities[0]?.names, ['Core', 'Core Systems']);

  const outsideSelection = buildVisualizerSnapshot(graphSnapshot(nodes, edges), {
    communityId: 'other',
    selectedNodeId: 'n39',
  });
  assert.equal(outsideSelection.selectedNodeId, null);
});

test('reserves selected neighbors ahead of unrelated hubs and reports incidents before node truncation', () => {
  const nodes = Array.from({ length: 30 }, (_, index) => graphNode(`n${String(index).padStart(2, '0')}`));
  const edges: GraphEdge[] = [
    graphEdge('n29', 'n28', 'focus_out'),
    graphEdge('n27', 'n29', 'focus_in'),
  ];
  for (let index = 1; index < 27; index += 1) {
    edges.push(graphEdge('n00', `n${String(index).padStart(2, '0')}`, 'hub'));
  }

  const snapshot = buildVisualizerSnapshot(graphSnapshot(nodes, edges), {
    nodeLimit: 25,
    selectedNodeId: 'n29',
  });

  assert.deepEqual(snapshot.nodes.slice(0, 3).map((node) => node.id), ['n29', 'n27', 'n28']);
  assert.equal(snapshot.counts.selectedIncidentEdges, 2);
  assert.equal(snapshot.counts.includedSelectedIncidentEdges, 2);
  assert.deepEqual(new Set(snapshot.edges.slice(0, 2).map((edge) => edge.relation)), new Set(['focus_in', 'focus_out']));
});

test('reports edge-limit truncation exactly and ranks selected incidents first', () => {
  const nodes = Array.from({ length: 100 }, (_, index) => graphNode(`n${String(index).padStart(3, '0')}`));
  const edges = Array.from({ length: 4_000 }, (_, index) => graphEdge(
    `n${String(index % 100).padStart(3, '0')}`,
    `n${String((index * 7 + 1) % 100).padStart(3, '0')}`,
    `relation_${index % 5}`,
    { confidence: index % 3 === 0 ? 'INFERRED' : 'EXTRACTED' },
  ));
  const snapshot = buildVisualizerSnapshot(graphSnapshot(nodes, edges), { nodeLimit: 100, selectedNodeId: 'n099' });

  assert.equal(snapshot.limits.edges, MIN_VISUALIZER_EDGE_LIMIT);
  assert.equal(snapshot.counts.eligibleEdges, 4_000);
  assert.equal(snapshot.counts.includedEdges, 2_800);
  assert.equal(snapshot.counts.omittedEdgesByEdgeLimit, 1_200);
  assert.equal(snapshot.counts.omittedEdges, 1_200);
  assert.ok(snapshot.counts.selectedIncidentEdges > 0);
  assert.equal(snapshot.counts.includedSelectedIncidentEdges, snapshot.counts.selectedIncidentEdges);
  assert.ok(snapshot.edges
    .slice(0, snapshot.counts.selectedIncidentEdges)
    .every((edge) => edge.source === 'n099' || edge.target === 'n099'));
  assertVisualizerSnapshot(snapshot);
});

test('validates only supported host messages and internally consistent snapshots', () => {
  assert.equal(isVisualizerWebviewMessage({ type: 'ready' }), true);
  assert.equal(isVisualizerWebviewMessage({ type: 'reveal', id: 'node' }), true);
  assert.equal(isVisualizerWebviewMessage({ type: 'explain', id: '' }), true);
  assert.equal(isVisualizerWebviewMessage({ type: 'reveal', id: 42 }), false);
  assert.equal(isVisualizerWebviewMessage({ type: 'risk', id: 'node' }), false);
  assert.equal(isKnownGraphConfidence('EXTRACTED'), true);
  assert.equal(isKnownGraphConfidence('UNKNOWN'), false);

  const snapshot = buildVisualizerSnapshot(graphSnapshot([graphNode('a'), graphNode('b')], [graphEdge('a', 'b')]));
  const wrongContract = { ...snapshot, contractVersion: 2 };
  const danglingPayload = { ...snapshot, edges: [{ ...snapshot.edges[0]!, target: 'missing' }] };
  const inconsistentCounts = { ...snapshot, counts: { ...snapshot.counts, omittedEdges: 100 } };
  assert.equal(isVisualizerSnapshot(wrongContract), false);
  assert.equal(isVisualizerSnapshot(danglingPayload), false);
  assert.equal(isVisualizerSnapshot(inconsistentCounts), false);
  assert.throws(() => assertVisualizerSnapshot(wrongContract), /Invalid Graphoxide visualizer snapshot/u);
});
