import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { VisualizerSnapshot } from '../src/visualizer-model';

interface DenseFixtureSpec {
  readonly version: 1;
  readonly nodeCount: number;
  readonly edgeCount: number;
  readonly communityGroups: readonly { readonly count: number; readonly size: number }[];
}

export type VisualizerFixtureName = 'small' | 'medium' | 'dense' | 'maximum' | 'maximum-one-community' | 'maximum-singletons';

export function visualizerFixture(name: VisualizerFixtureName): VisualizerSnapshot {
  if (name === 'small') return buildFixture(42, 71, communitySizes(7, 6));
  if (name === 'medium') return buildFixture(240, 480, [28, 24, 20, 18, 16, 14, 12, 10, ...communitySizes(14, 7)]);
  if (name === 'dense') {
    const spec = readDenseSpec();
    const sizes = spec.communityGroups.flatMap((group) => Array.from({ length: group.count }, () => group.size));
    assert.equal(sizes.reduce((sum, size) => sum + size, 0), spec.nodeCount);
    return buildFixture(spec.nodeCount, spec.edgeCount, sizes);
  }
  if (name === 'maximum-one-community') return buildFixture(5_000, 12_000, [5_000]);
  if (name === 'maximum-singletons') return buildFixture(5_000, 12_000, communitySizes(5_000, 1));
  return buildFixture(5_000, 12_000, communitySizes(80, 62, 40));
}

function readDenseSpec(): DenseFixtureSpec {
  const file = path.join(process.cwd(), 'test', 'fixtures', 'visualizer-dense-spec.json');
  return JSON.parse(readFileSync(file, 'utf8')) as DenseFixtureSpec;
}

function communitySizes(count: number, size: number, remainder = 0): number[] {
  return Array.from({ length: count }, (_, index) => size + Number(index < remainder));
}

function buildFixture(nodeCount: number, edgeCount: number, rawCommunitySizes: readonly number[]): VisualizerSnapshot {
  const communitySizes = normalizeCommunitySizes(nodeCount, rawCommunitySizes);
  const communityForNode: string[] = [];
  const communityStarts: number[] = [];
  let cursor = 0;
  communitySizes.forEach((size, communityIndex) => {
    communityStarts.push(cursor);
    for (let offset = 0; offset < size; offset += 1) communityForNode.push(`domain-${String(communityIndex).padStart(4, '0')}`);
    cursor += size;
  });
  const degrees = Array.from({ length: nodeCount }, () => 0);
  const inDegrees = Array.from({ length: nodeCount }, () => 0);
  const outDegrees = Array.from({ length: nodeCount }, () => 0);
  const edgePairs = Array.from({ length: edgeCount }, (_, index) => {
    const source = index % nodeCount;
    const sourceCommunity = communityForNode[source] ?? 'domain-0000';
    const communityIndex = Number.parseInt(sourceCommunity.slice('domain-'.length), 10);
    const hub = communityStarts[communityIndex] ?? 0;
    const target = index < nodeCount
      ? (source + 1) % nodeCount
      : index < nodeCount * 2
        ? hub
        : (source * 37 + Math.floor(index / nodeCount) * 17 + 11) % nodeCount;
    degrees[source] = (degrees[source] ?? 0) + 1;
    degrees[target] = (degrees[target] ?? 0) + 1;
    outDegrees[source] = (outDegrees[source] ?? 0) + 1;
    inDegrees[target] = (inDegrees[target] ?? 0) + 1;
    return { source, target };
  });
  const nodes = Array.from({ length: nodeCount }, (_, index) => {
    const community = communityForNode[index] ?? 'domain-0000';
    return {
      id: `symbol-${String(index).padStart(5, '0')}`,
      label: index % 9 === 0 ? `importantCheckoutCoordinator${index}()` : `symbol${index}`,
      file: 'cartograph/domain.py',
      location: `L${index + 1}`,
      kind: ['function', 'file', 'concept', 'image'][index % 4] ?? 'function',
      community,
      communityName: community.replace('domain-', 'Domain '),
      degree: degrees[index] ?? 0,
      inDegree: inDegrees[index] ?? 0,
      outDegree: outDegrees[index] ?? 0,
    };
  });
  const edges = edgePairs.map((edge, index) => ({
    source: nodes[edge.source]?.id ?? nodes[0]!.id,
    target: nodes[edge.target]?.id ?? nodes[0]!.id,
    relation: index % 5 === 0 ? 'references' : 'calls',
    confidence: index % 7 === 0 ? 'INFERRED' : 'EXTRACTED',
    sourceFile: 'cartograph/domain.py',
    sourceLocation: `L${edge.source + 1}`,
  }));
  const communities = communitySizes.map((size, index) => {
    const id = `domain-${String(index).padStart(4, '0')}`;
    return { id, name: `Domain ${String(index).padStart(4, '0')}`, names: [`Domain ${String(index).padStart(4, '0')}`], nodeCount: size };
  });
  const extracted = edges.reduce((count, edge) => count + Number(edge.confidence === 'EXTRACTED'), 0);
  const calls = edges.reduce((count, edge) => count + Number(edge.relation === 'calls'), 0);
  return {
    contractVersion: 1,
    directed: true,
    builtAtCommit: 'visualizer-fixture',
    scope: { kind: 'all' },
    selectedNodeId: null,
    limits: { nodes: Math.max(25, nodeCount), edges: Math.max(2_800, Math.min(12_000, nodeCount * 4)) },
    counts: {
      totalNodes: nodeCount,
      scopedNodes: nodeCount,
      includedNodes: nodeCount,
      omittedNodes: 0,
      omittedNodesByScope: 0,
      omittedNodesByLimit: 0,
      totalEdges: edgeCount,
      validEdges: edgeCount,
      scopedEdges: edgeCount,
      eligibleEdges: edgeCount,
      includedEdges: edgeCount,
      omittedEdges: 0,
      invalidEndpointEdges: 0,
      omittedEdgesByScope: 0,
      omittedEdgesByNodeLimit: 0,
      omittedEdgesByEdgeLimit: 0,
      selectedIncidentEdges: 0,
      includedSelectedIncidentEdges: 0,
    },
    nodes,
    edges,
    communities,
    relations: [
      { value: 'calls', count: calls },
      { value: 'references', count: edgeCount - calls },
    ],
    confidences: [
      { value: 'EXTRACTED', count: extracted },
      { value: 'INFERRED', count: edgeCount - extracted },
    ],
  };
}

function normalizeCommunitySizes(nodeCount: number, rawSizes: readonly number[]): number[] {
  const sizes = rawSizes.filter((size) => Number.isSafeInteger(size) && size > 0);
  const total = sizes.reduce((sum, size) => sum + size, 0);
  if (total === nodeCount) return sizes;
  if (total > nodeCount) {
    let remaining = nodeCount;
    return sizes.flatMap((size) => {
      if (remaining <= 0) return [];
      const retained = Math.min(size, remaining);
      remaining -= retained;
      return [retained];
    });
  }
  return [...sizes, nodeCount - total];
}
