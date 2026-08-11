import { GraphEdge, GraphModel, GraphNode, GraphSnapshot } from './graph';

export const VISUALIZER_SNAPSHOT_CONTRACT_VERSION = 1 as const;
export const DEFAULT_VISUALIZER_NODE_LIMIT = 750;
export const MIN_VISUALIZER_NODE_LIMIT = 25;
export const MAX_VISUALIZER_NODE_LIMIT = 5_000;
export const MIN_VISUALIZER_EDGE_LIMIT = 2_800;
export const MAX_VISUALIZER_EDGE_LIMIT = 12_000;
/** Maximum UTF-16 code units in any individual string value sent to the webview. */
export const MAX_VISUALIZER_STRING_CODE_UNITS = 16_384;
/** Maximum UTF-16 code units across all string values in one serialized snapshot. */
export const MAX_VISUALIZER_SNAPSHOT_STRING_CODE_UNITS = 8_000_000;

const VISUALIZER_STRING_LIMIT_ERROR = 'Graph data exceeds the visualizer per-string safety limit';
const VISUALIZER_SNAPSHOT_STRING_LIMIT_ERROR = 'Graph data exceeds the visualizer cumulative string safety limit';

export const KNOWN_GRAPH_CONFIDENCES = ['EXTRACTED', 'INFERRED', 'AMBIGUOUS'] as const;
export type KnownGraphConfidence = typeof KNOWN_GRAPH_CONFIDENCES[number];

export interface VisualizerNode {
  readonly id: string;
  readonly label: string;
  readonly file: string;
  readonly location: string | null;
  readonly kind: string;
  readonly community: string | null;
  readonly communityName: string | null;
  readonly degree: number;
  readonly inDegree: number;
  readonly outDegree: number;
}

export interface VisualizerEdge {
  readonly source: string;
  readonly target: string;
  readonly relation: string;
  readonly confidence: string | null;
  readonly sourceFile: string | null;
  readonly sourceLocation: string | null;
}

export interface VisualizerCommunityFacet {
  readonly id: string | null;
  /** A deterministic real name from `names`, or null when none was recorded. */
  readonly name: string | null;
  /** Every distinct real community name recorded for this ID. */
  readonly names: readonly string[];
  readonly nodeCount: number;
}

export interface VisualizerValueFacet<T extends string | null> {
  readonly value: T;
  readonly count: number;
}

export type VisualizerScope =
  | { readonly kind: 'all' }
  | { readonly kind: 'community'; readonly id: string | null };

export interface VisualizerSnapshotCounts {
  readonly totalNodes: number;
  readonly scopedNodes: number;
  readonly includedNodes: number;
  readonly omittedNodes: number;
  readonly omittedNodesByScope: number;
  readonly omittedNodesByLimit: number;
  readonly totalEdges: number;
  readonly validEdges: number;
  readonly scopedEdges: number;
  readonly eligibleEdges: number;
  readonly includedEdges: number;
  readonly omittedEdges: number;
  readonly invalidEndpointEdges: number;
  readonly omittedEdgesByScope: number;
  readonly omittedEdgesByNodeLimit: number;
  readonly omittedEdgesByEdgeLimit: number;
  readonly selectedIncidentEdges: number;
  readonly includedSelectedIncidentEdges: number;
}

export interface VisualizerSnapshot {
  readonly contractVersion: typeof VISUALIZER_SNAPSHOT_CONTRACT_VERSION;
  readonly directed: boolean;
  readonly builtAtCommit: string | null;
  readonly scope: VisualizerScope;
  /** The selected node only when it exists inside this bounded snapshot. */
  readonly selectedNodeId: string | null;
  readonly limits: {
    readonly nodes: number;
    readonly edges: number;
  };
  readonly counts: VisualizerSnapshotCounts;
  readonly nodes: readonly VisualizerNode[];
  readonly edges: readonly VisualizerEdge[];
  readonly communities: readonly VisualizerCommunityFacet[];
  readonly relations: readonly VisualizerValueFacet<string>[];
  readonly confidences: readonly VisualizerValueFacet<string | null>[];
}

export interface BuildVisualizerSnapshotOptions {
  /** Invalid values use the default; finite values are truncated and clamped. */
  readonly nodeLimit?: number;
  /** Reserved in both the node and edge rankings when it belongs to the scope. */
  readonly selectedNodeId?: string;
  /** Undefined means all communities; null selects nodes without a community. */
  readonly communityId?: string | null;
}

export type VisualizerWebviewMessage =
  | { readonly type: 'ready' }
  | { readonly type: 'reveal' | 'explain'; readonly id: string };

interface NodeMetrics {
  degree: number;
  inDegree: number;
  outDegree: number;
}

interface RankedEdge {
  readonly edge: VisualizerEdge;
  readonly selectedIncident: boolean;
  readonly endpointDegree: number;
}

export function normalizeVisualizerNodeLimit(value?: number): number {
  if (value === undefined || !Number.isFinite(value)) return DEFAULT_VISUALIZER_NODE_LIMIT;
  return Math.max(MIN_VISUALIZER_NODE_LIMIT, Math.min(MAX_VISUALIZER_NODE_LIMIT, Math.trunc(value)));
}

export function visualizerEdgeLimit(includedNodes: number): number {
  const safeNodes = Number.isFinite(includedNodes) ? Math.max(0, Math.trunc(includedNodes)) : 0;
  return Math.min(MAX_VISUALIZER_EDGE_LIMIT, Math.max(MIN_VISUALIZER_EDGE_LIMIT, safeNodes * 4));
}

export function isKnownGraphConfidence(value: unknown): value is KnownGraphConfidence {
  return typeof value === 'string' && (KNOWN_GRAPH_CONFIDENCES as readonly string[]).includes(value);
}

/**
 * Produce the complete, bounded, inert-data contract sent to the graph webview.
 * No arbitrary node/edge attribute is copied into the result.
 */
export function buildVisualizerSnapshot(
  input: GraphModel | GraphSnapshot,
  options: BuildVisualizerSnapshotOptions = {},
): VisualizerSnapshot {
  const graph = input instanceof GraphModel ? input.snapshot : input;
  assertGraphStringsWithinLimit(graph, options);
  const nodeLimit = normalizeVisualizerNodeLimit(options.nodeLimit);
  const hasCommunityScope = options.communityId !== undefined;
  const scope: VisualizerScope = hasCommunityScope
    ? { kind: 'community', id: options.communityId ?? null }
    : { kind: 'all' };

  assertUniqueNodeIds(graph.nodes);
  const metrics = buildNodeMetrics(graph.nodes, graph.edges);
  const allNodeIds = new Set(graph.nodes.map((node) => node.id));
  const scopedNodes = graph.nodes.filter((node) => !hasCommunityScope || (node.community ?? null) === options.communityId);
  const scopedNodeIds = new Set(scopedNodes.map((node) => node.id));
  const selectedInScope = options.selectedNodeId !== undefined && scopedNodeIds.has(options.selectedNodeId)
    ? options.selectedNodeId
    : null;
  const selectedNeighborEdgeCounts = buildSelectedNeighborEdgeCounts(
    graph.edges,
    scopedNodeIds,
    selectedInScope,
  );
  const includedSourceNodes = [...scopedNodes]
    .sort((left, right) => compareRankedNodes(
      left,
      right,
      metrics,
      selectedInScope,
      selectedNeighborEdgeCounts,
    ))
    .slice(0, nodeLimit);
  const includedNodeIds = new Set(includedSourceNodes.map((node) => node.id));
  const selectedNodeId = selectedInScope !== null && includedNodeIds.has(selectedInScope) ? selectedInScope : null;
  const nodes = includedSourceNodes.map((node) => toVisualizerNode(node, requiredMetrics(metrics, node.id)));

  let validEdges = 0;
  let scopedEdges = 0;
  let eligibleEdges = 0;
  let selectedIncidentEdges = 0;
  const rankedEdges: RankedEdge[] = [];
  for (const edge of graph.edges) {
    if (!allNodeIds.has(edge.source) || !allNodeIds.has(edge.target)) continue;
    validEdges += 1;
    if (!scopedNodeIds.has(edge.source) || !scopedNodeIds.has(edge.target)) continue;
    scopedEdges += 1;
    if (selectedNodeId !== null && (edge.source === selectedNodeId || edge.target === selectedNodeId)) {
      selectedIncidentEdges += 1;
    }
    if (!includedNodeIds.has(edge.source) || !includedNodeIds.has(edge.target)) continue;
    eligibleEdges += 1;
    rankedEdges.push({
      edge: toVisualizerEdge(edge),
      selectedIncident: selectedNodeId !== null && (edge.source === selectedNodeId || edge.target === selectedNodeId),
      endpointDegree: requiredMetrics(metrics, edge.source).degree + requiredMetrics(metrics, edge.target).degree,
    });
  }

  const edgeLimit = visualizerEdgeLimit(nodes.length);
  rankedEdges.sort(compareRankedEdges);
  const includedRankedEdges = rankedEdges.slice(0, edgeLimit);
  const includedSelectedIncidentEdges = includedRankedEdges.reduce((count, entry) => count + Number(entry.selectedIncident), 0);
  const edges = includedRankedEdges.map((entry) => entry.edge);

  const totalNodes = graph.nodes.length;
  const totalEdges = graph.edges.length;
  const includedNodes = nodes.length;
  const includedEdges = edges.length;
  const invalidEndpointEdges = totalEdges - validEdges;
  const omittedEdgesByScope = validEdges - scopedEdges;
  const omittedEdgesByNodeLimit = scopedEdges - eligibleEdges;
  const omittedEdgesByEdgeLimit = eligibleEdges - includedEdges;
  const counts: VisualizerSnapshotCounts = {
    totalNodes,
    scopedNodes: scopedNodes.length,
    includedNodes,
    omittedNodes: totalNodes - includedNodes,
    omittedNodesByScope: totalNodes - scopedNodes.length,
    omittedNodesByLimit: scopedNodes.length - includedNodes,
    totalEdges,
    validEdges,
    scopedEdges,
    eligibleEdges,
    includedEdges,
    omittedEdges: totalEdges - includedEdges,
    invalidEndpointEdges,
    omittedEdgesByScope,
    omittedEdgesByNodeLimit,
    omittedEdgesByEdgeLimit,
    selectedIncidentEdges,
    includedSelectedIncidentEdges,
  };

  const snapshot: VisualizerSnapshot = {
    contractVersion: VISUALIZER_SNAPSHOT_CONTRACT_VERSION,
    directed: graph.directed,
    builtAtCommit: graph.builtAtCommit ?? null,
    scope,
    selectedNodeId,
    limits: { nodes: nodeLimit, edges: edgeLimit },
    counts,
    nodes,
    edges,
    communities: buildCommunityFacets(nodes),
    relations: buildValueFacets(edges.map((edge) => edge.relation), compareText),
    confidences: buildValueFacets(edges.map((edge) => edge.confidence), compareConfidenceValues),
  };
  assertVisualizerSnapshotStringBudgets(snapshot);
  return snapshot;
}

export function isVisualizerWebviewMessage(value: unknown): value is VisualizerWebviewMessage {
  if (!isRecord(value) || typeof value.type !== 'string') return false;
  if (value.type === 'ready') return true;
  return (value.type === 'reveal' || value.type === 'explain') && isBudgetedString(value.id);
}

export function isVisualizerSnapshot(value: unknown): value is VisualizerSnapshot {
  if (!isRecord(value)
    || value.contractVersion !== VISUALIZER_SNAPSHOT_CONTRACT_VERSION
    || typeof value.directed !== 'boolean'
    || !isBudgetedNullableString(value.builtAtCommit)
    || !isVisualizerScope(value.scope)
    || !isBudgetedNullableString(value.selectedNodeId)
    || !isRecord(value.limits)
    || !isNonNegativeInteger(value.limits.nodes)
    || !isNonNegativeInteger(value.limits.edges)
    || !isRecord(value.counts)
    || !hasValidCounts(value.counts)
    || !Array.isArray(value.nodes)
    || !value.nodes.every(isVisualizerNode)
    || !Array.isArray(value.edges)
    || !value.edges.every(isVisualizerEdge)
    || !Array.isArray(value.communities)
    || !value.communities.every(isCommunityFacet)
    || !Array.isArray(value.relations)
    || !value.relations.every((facet) => isValueFacet(facet, false))
    || !Array.isArray(value.confidences)
    || !value.confidences.every((facet) => isValueFacet(facet, true))) {
    return false;
  }

  const snapshot = value as unknown as VisualizerSnapshot;
  const counts = value.counts as unknown as VisualizerSnapshotCounts;
  if (counts.includedNodes !== value.nodes.length
    || counts.includedEdges !== value.edges.length
    || counts.omittedNodes !== counts.totalNodes - counts.includedNodes
    || counts.omittedNodesByScope + counts.omittedNodesByLimit !== counts.omittedNodes
    || counts.invalidEndpointEdges + counts.omittedEdgesByScope + counts.omittedEdgesByNodeLimit
      + counts.omittedEdgesByEdgeLimit + counts.includedEdges !== counts.totalEdges
    || counts.omittedEdges !== counts.totalEdges - counts.includedEdges
    || counts.includedNodes > counts.scopedNodes
    || counts.scopedNodes > counts.totalNodes
    || counts.includedEdges > counts.eligibleEdges
    || counts.eligibleEdges > counts.scopedEdges
    || counts.scopedEdges > counts.validEdges
    || counts.validEdges > counts.totalEdges
    || counts.includedSelectedIncidentEdges > counts.selectedIncidentEdges
    || counts.selectedIncidentEdges > counts.scopedEdges
    || value.limits.nodes < MIN_VISUALIZER_NODE_LIMIT
    || value.limits.nodes > MAX_VISUALIZER_NODE_LIMIT
    || value.limits.edges < MIN_VISUALIZER_EDGE_LIMIT
    || value.limits.edges > MAX_VISUALIZER_EDGE_LIMIT
    || value.nodes.length > value.limits.nodes
    || value.edges.length > value.limits.edges
    || value.limits.edges !== visualizerEdgeLimit(value.nodes.length)
    || value.communities.length > value.nodes.length
    || value.communities.reduce((count, facet) => count + facet.names.length, 0) > value.nodes.length
    || value.relations.length > value.edges.length
    || value.confidences.length > value.edges.length
    || visualizerSnapshotStringBudgetFailure(snapshot) !== null) {
    return false;
  }
  const nodeIds = new Set<string>();
  for (const node of value.nodes) {
    if (nodeIds.has(node.id)) return false;
    nodeIds.add(node.id);
  }
  if (value.selectedNodeId !== null && !nodeIds.has(value.selectedNodeId)) return false;
  return value.edges.every((edge) => nodeIds.has(edge.source) && nodeIds.has(edge.target));
}

export function assertVisualizerSnapshot(value: unknown): asserts value is VisualizerSnapshot {
  if (!isVisualizerSnapshot(value)) throw new Error('Invalid Graphoxide visualizer snapshot');
}

function assertUniqueNodeIds(nodes: readonly GraphNode[]): void {
  const seen = new Set<string>();
  for (const node of nodes) {
    if (seen.has(node.id)) throw new Error(`Cannot visualize duplicate node ID: ${node.id}`);
    seen.add(node.id);
  }
}

function assertGraphStringsWithinLimit(
  graph: GraphSnapshot,
  options: BuildVisualizerSnapshotOptions,
): void {
  assertStringWithinLimit(graph.builtAtCommit);
  assertStringWithinLimit(options.selectedNodeId);
  assertStringWithinLimit(options.communityId);
  for (const node of graph.nodes) {
    assertStringWithinLimit(node.id);
    assertStringWithinLimit(node.label);
    assertStringWithinLimit(node.sourceFile);
    assertStringWithinLimit(node.sourceLocation);
    assertStringWithinLimit(node.fileType);
    assertStringWithinLimit(node.community);
    assertStringWithinLimit(node.communityName);
  }
  for (const edge of graph.edges) {
    assertStringWithinLimit(edge.source);
    assertStringWithinLimit(edge.target);
    assertStringWithinLimit(edge.relation);
    assertStringWithinLimit(edge.confidence);
    assertStringWithinLimit(edge.sourceFile);
    assertStringWithinLimit(optionalAttributeString(edge.attributes, 'source_location'));
  }
}

function assertStringWithinLimit(value: string | null | undefined): void {
  if (value !== null && value !== undefined && value.length > MAX_VISUALIZER_STRING_CODE_UNITS) {
    throw new Error(VISUALIZER_STRING_LIMIT_ERROR);
  }
}

function assertVisualizerSnapshotStringBudgets(snapshot: VisualizerSnapshot): void {
  const failure = visualizerSnapshotStringBudgetFailure(snapshot);
  if (failure === 'per-string') throw new Error(VISUALIZER_STRING_LIMIT_ERROR);
  if (failure === 'cumulative') throw new Error(VISUALIZER_SNAPSHOT_STRING_LIMIT_ERROR);
}

function visualizerSnapshotStringBudgetFailure(
  snapshot: VisualizerSnapshot,
): 'per-string' | 'cumulative' | null {
  let total = 0;
  let failure: 'per-string' | 'cumulative' | null = null;
  const add = (value: string | null): void => {
    if (value === null || failure !== null) return;
    if (value.length > MAX_VISUALIZER_STRING_CODE_UNITS) {
      failure = 'per-string';
      return;
    }
    total += value.length;
    if (total > MAX_VISUALIZER_SNAPSHOT_STRING_CODE_UNITS) failure = 'cumulative';
  };

  add(snapshot.builtAtCommit);
  add(snapshot.scope.kind);
  if (snapshot.scope.kind === 'community') add(snapshot.scope.id);
  add(snapshot.selectedNodeId);
  for (const node of snapshot.nodes) {
    add(node.id);
    add(node.label);
    add(node.file);
    add(node.location);
    add(node.kind);
    add(node.community);
    add(node.communityName);
  }
  for (const edge of snapshot.edges) {
    add(edge.source);
    add(edge.target);
    add(edge.relation);
    add(edge.confidence);
    add(edge.sourceFile);
    add(edge.sourceLocation);
  }
  for (const community of snapshot.communities) {
    add(community.id);
    add(community.name);
    for (const name of community.names) add(name);
  }
  for (const relation of snapshot.relations) add(relation.value);
  for (const confidence of snapshot.confidences) add(confidence.value);
  return failure;
}

function buildNodeMetrics(nodes: readonly GraphNode[], edges: readonly GraphEdge[]): ReadonlyMap<string, NodeMetrics> {
  const metrics = new Map(nodes.map((node): [string, NodeMetrics] => [node.id, { degree: 0, inDegree: 0, outDegree: 0 }]));
  for (const edge of edges) {
    const source = metrics.get(edge.source);
    const target = metrics.get(edge.target);
    if (source) {
      source.degree += 1;
      source.outDegree += 1;
    }
    if (target) {
      target.inDegree += 1;
      if (edge.target !== edge.source) target.degree += 1;
    }
  }
  return metrics;
}

function buildSelectedNeighborEdgeCounts(
  edges: readonly GraphEdge[],
  scopedNodeIds: ReadonlySet<string>,
  selectedNodeId: string | null,
): ReadonlyMap<string, number> {
  const counts = new Map<string, number>();
  if (selectedNodeId === null) return counts;
  for (const edge of edges) {
    if (!scopedNodeIds.has(edge.source) || !scopedNodeIds.has(edge.target)) continue;
    let neighbor: string | null = null;
    if (edge.source === selectedNodeId) neighbor = edge.target;
    else if (edge.target === selectedNodeId) neighbor = edge.source;
    if (neighbor !== null && neighbor !== selectedNodeId) {
      counts.set(neighbor, (counts.get(neighbor) ?? 0) + 1);
    }
  }
  return counts;
}

function requiredMetrics(metrics: ReadonlyMap<string, NodeMetrics>, id: string): NodeMetrics {
  const value = metrics.get(id);
  if (!value) throw new Error(`Cannot visualize edge endpoint without a node: ${id}`);
  return value;
}

function compareRankedNodes(
  left: GraphNode,
  right: GraphNode,
  metrics: ReadonlyMap<string, NodeMetrics>,
  selectedNodeId: string | null,
  selectedNeighborEdgeCounts: ReadonlyMap<string, number>,
): number {
  const selectedDifference = Number(right.id === selectedNodeId) - Number(left.id === selectedNodeId);
  if (selectedDifference !== 0) return selectedDifference;
  const neighborDifference = (selectedNeighborEdgeCounts.get(right.id) ?? 0)
    - (selectedNeighborEdgeCounts.get(left.id) ?? 0);
  if (neighborDifference !== 0) return neighborDifference;
  const degreeDifference = requiredMetrics(metrics, right.id).degree - requiredMetrics(metrics, left.id).degree;
  if (degreeDifference !== 0) return degreeDifference;
  return compareText(left.label, right.label)
    || compareText(left.id, right.id)
    || compareText(left.sourceFile, right.sourceFile)
    || compareNullableText(left.sourceLocation ?? null, right.sourceLocation ?? null)
    || compareText(left.fileType, right.fileType)
    || compareNullableText(left.community ?? null, right.community ?? null)
    || compareNullableText(left.communityName ?? null, right.communityName ?? null);
}

function compareRankedEdges(left: RankedEdge, right: RankedEdge): number {
  const incidentDifference = Number(right.selectedIncident) - Number(left.selectedIncident);
  if (incidentDifference !== 0) return incidentDifference;
  const degreeDifference = right.endpointDegree - left.endpointDegree;
  if (degreeDifference !== 0) return degreeDifference;
  const confidenceDifference = confidenceRank(left.edge.confidence) - confidenceRank(right.edge.confidence);
  if (confidenceDifference !== 0) return confidenceDifference;
  return compareText(left.edge.source, right.edge.source)
    || compareText(left.edge.target, right.edge.target)
    || compareText(left.edge.relation, right.edge.relation)
    || compareNullableText(left.edge.confidence, right.edge.confidence)
    || compareNullableText(left.edge.sourceFile, right.edge.sourceFile)
    || compareNullableText(left.edge.sourceLocation, right.edge.sourceLocation);
}

function toVisualizerNode(node: GraphNode, metrics: NodeMetrics): VisualizerNode {
  return {
    id: node.id,
    label: node.label,
    file: node.sourceFile,
    location: node.sourceLocation ?? null,
    kind: node.fileType,
    community: node.community ?? null,
    communityName: node.communityName ?? null,
    degree: metrics.degree,
    inDegree: metrics.inDegree,
    outDegree: metrics.outDegree,
  };
}

function toVisualizerEdge(edge: GraphEdge): VisualizerEdge {
  return {
    source: edge.source,
    target: edge.target,
    relation: edge.relation,
    confidence: edge.confidence ?? null,
    sourceFile: edge.sourceFile ?? null,
    sourceLocation: optionalAttributeString(edge.attributes, 'source_location'),
  };
}

function optionalAttributeString(attributes: Readonly<Record<string, unknown>>, key: string): string | null {
  const value = attributes[key];
  if (typeof value === 'string') return value;
  if (typeof value === 'number' && Number.isFinite(value)) return String(value);
  return null;
}

function buildCommunityFacets(nodes: readonly VisualizerNode[]): readonly VisualizerCommunityFacet[] {
  const groups = new Map<string | null, { count: number; names: Set<string> }>();
  for (const node of nodes) {
    const group = groups.get(node.community) ?? { count: 0, names: new Set<string>() };
    group.count += 1;
    if (node.communityName !== null) group.names.add(node.communityName);
    groups.set(node.community, group);
  }
  return [...groups.entries()]
    .map(([id, group]) => {
      const names = [...group.names].sort(compareText);
      return { id, name: names[0] ?? null, names, nodeCount: group.count };
    })
    .sort((left, right) => right.nodeCount - left.nodeCount
      || compareNullableText(left.id, right.id)
      || compareNullableText(left.name, right.name));
}

function buildValueFacets<T extends string | null>(
  values: readonly T[],
  compare: (left: T, right: T) => number,
): readonly VisualizerValueFacet<T>[] {
  const counts = new Map<T, number>();
  for (const value of values) counts.set(value, (counts.get(value) ?? 0) + 1);
  return [...counts.entries()]
    .map(([value, count]) => ({ value, count }))
    .sort((left, right) => compare(left.value, right.value));
}

function confidenceRank(value: string | null): number {
  if (value === 'EXTRACTED') return 0;
  if (value === 'INFERRED') return 1;
  if (value === 'AMBIGUOUS') return 2;
  if (value === null) return 4;
  return 3;
}

function compareConfidenceValues(left: string | null, right: string | null): number {
  const rankDifference = confidenceRank(left) - confidenceRank(right);
  return rankDifference || compareNullableText(left, right);
}

function compareText(left: string, right: string): number {
  return left < right ? -1 : left > right ? 1 : 0;
}

function compareNullableText(left: string | null, right: string | null): number {
  if (left === right) return 0;
  if (left === null) return 1;
  if (right === null) return -1;
  return compareText(left, right);
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function isBudgetedString(value: unknown): value is string {
  return typeof value === 'string' && value.length <= MAX_VISUALIZER_STRING_CODE_UNITS;
}

function isBudgetedNullableString(value: unknown): value is string | null {
  return value === null || isBudgetedString(value);
}

function isNonNegativeInteger(value: unknown): value is number {
  return typeof value === 'number' && Number.isSafeInteger(value) && value >= 0;
}

function isVisualizerScope(value: unknown): value is VisualizerScope {
  return isRecord(value)
    && (value.kind === 'all' || (value.kind === 'community' && isBudgetedNullableString(value.id)));
}

function isVisualizerNode(value: unknown): value is VisualizerNode {
  return isRecord(value)
    && isBudgetedString(value.id)
    && isBudgetedString(value.label)
    && isBudgetedString(value.file)
    && isBudgetedNullableString(value.location)
    && isBudgetedString(value.kind)
    && isBudgetedNullableString(value.community)
    && isBudgetedNullableString(value.communityName)
    && isNonNegativeInteger(value.degree)
    && isNonNegativeInteger(value.inDegree)
    && isNonNegativeInteger(value.outDegree);
}

function isVisualizerEdge(value: unknown): value is VisualizerEdge {
  return isRecord(value)
    && isBudgetedString(value.source)
    && isBudgetedString(value.target)
    && isBudgetedString(value.relation)
    && isBudgetedNullableString(value.confidence)
    && isBudgetedNullableString(value.sourceFile)
    && isBudgetedNullableString(value.sourceLocation);
}

function isCommunityFacet(value: unknown): value is VisualizerCommunityFacet {
  return isRecord(value)
    && isBudgetedNullableString(value.id)
    && isBudgetedNullableString(value.name)
    && Array.isArray(value.names)
    && value.names.every(isBudgetedString)
    && isNonNegativeInteger(value.nodeCount);
}

function isValueFacet(value: unknown, nullable: boolean): boolean {
  return isRecord(value)
    && (isBudgetedString(value.value) || (nullable && value.value === null))
    && isNonNegativeInteger(value.count);
}

function hasValidCounts(value: Record<string, unknown>): boolean {
  const keys: readonly (keyof VisualizerSnapshotCounts)[] = [
    'totalNodes',
    'scopedNodes',
    'includedNodes',
    'omittedNodes',
    'omittedNodesByScope',
    'omittedNodesByLimit',
    'totalEdges',
    'validEdges',
    'scopedEdges',
    'eligibleEdges',
    'includedEdges',
    'omittedEdges',
    'invalidEndpointEdges',
    'omittedEdgesByScope',
    'omittedEdgesByNodeLimit',
    'omittedEdgesByEdgeLimit',
    'selectedIncidentEdges',
    'includedSelectedIncidentEdges',
  ];
  return keys.every((key) => isNonNegativeInteger(value[key]));
}
