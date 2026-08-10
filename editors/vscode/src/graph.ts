import * as path from 'node:path';

export interface GraphNode {
  readonly id: string;
  readonly label: string;
  readonly fileType: string;
  readonly sourceFile: string;
  readonly sourceLocation?: string;
  readonly community?: string;
  readonly communityName?: string;
  readonly attributes: Readonly<Record<string, unknown>>;
}

export interface GraphEdge {
  readonly source: string;
  readonly target: string;
  readonly relation: string;
  readonly confidence?: string;
  readonly sourceFile?: string;
  readonly attributes: Readonly<Record<string, unknown>>;
}

export interface Community {
  readonly id: string;
  readonly name: string;
  readonly nodes: readonly GraphNode[];
}

export interface GraphSnapshot {
  readonly nodes: readonly GraphNode[];
  readonly edges: readonly GraphEdge[];
  readonly directed: boolean;
  readonly builtAtCommit?: string;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function requiredString(record: Record<string, unknown>, key: string, context: string): string {
  const value = record[key];
  if (typeof value === 'string' || typeof value === 'number') {
    return String(value);
  }
  throw new Error(`${context} is missing a valid ${key}`);
}

function optionalString(record: Record<string, unknown>, key: string): string | undefined {
  const value = record[key];
  return typeof value === 'string' || typeof value === 'number' ? String(value) : undefined;
}

function parseNode(value: unknown, index: number): GraphNode {
  if (!isRecord(value)) {
    throw new Error(`nodes[${index}] must be an object`);
  }
  return {
    id: requiredString(value, 'id', `nodes[${index}]`),
    label: requiredString(value, 'label', `nodes[${index}]`),
    fileType: optionalString(value, 'file_type') ?? 'unknown',
    sourceFile: optionalString(value, 'source_file') ?? '',
    sourceLocation: optionalString(value, 'source_location'),
    community: optionalString(value, 'community'),
    communityName: optionalString(value, 'community_name'),
    attributes: value,
  };
}

function parseEdge(value: unknown, index: number): GraphEdge {
  if (!isRecord(value)) {
    throw new Error(`links[${index}] must be an object`);
  }
  return {
    source: optionalString(value, '_src') ?? requiredString(value, 'source', `links[${index}]`),
    target: optionalString(value, '_tgt') ?? requiredString(value, 'target', `links[${index}]`),
    relation: optionalString(value, 'relation') ?? 'related_to',
    confidence: optionalString(value, 'confidence'),
    sourceFile: optionalString(value, 'source_file'),
    attributes: value,
  };
}

export function parseGraphJson(text: string): GraphSnapshot {
  let raw: unknown;
  try {
    raw = JSON.parse(text) as unknown;
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    throw new Error(`graph.json is not valid JSON: ${message}`, { cause: error });
  }
  if (!isRecord(raw)) {
    throw new Error('graph.json must contain a JSON object');
  }
  if (!Array.isArray(raw.nodes)) {
    throw new Error('graph.json is missing its nodes array');
  }
  const edgeValues = Array.isArray(raw.links) ? raw.links : raw.edges;
  if (!Array.isArray(edgeValues)) {
    throw new Error('graph.json is missing its links (or edges) array');
  }
  const nodes = raw.nodes.map(parseNode);
  const seen = new Set<string>();
  for (const node of nodes) {
    if (seen.has(node.id)) {
      throw new Error(`graph.json contains duplicate node ID: ${node.id}`);
    }
    seen.add(node.id);
  }
  return {
    nodes,
    edges: edgeValues.map(parseEdge),
    directed: raw.directed === true,
    builtAtCommit: optionalString(raw, 'built_at_commit'),
  };
}

export class GraphModel {
  readonly snapshot: GraphSnapshot;
  private readonly byId = new Map<string, GraphNode>();
  private readonly adjacent = new Map<string, GraphEdge[]>();

  constructor(snapshot: GraphSnapshot) {
    this.snapshot = snapshot;
    for (const node of snapshot.nodes) {
      this.byId.set(node.id, node);
      this.adjacent.set(node.id, []);
    }
    for (const edge of snapshot.edges) {
      this.adjacent.get(edge.source)?.push(edge);
      if (edge.target !== edge.source) {
        this.adjacent.get(edge.target)?.push(edge);
      }
    }
  }

  getNode(id: string): GraphNode | undefined {
    return this.byId.get(id);
  }

  edgesFor(id: string): readonly GraphEdge[] {
    return this.adjacent.get(id) ?? [];
  }

  degree(id: string): number {
    return this.edgesFor(id).length;
  }

  neighbors(id: string): readonly GraphNode[] {
    const nodes = new Map<string, GraphNode>();
    for (const edge of this.edgesFor(id)) {
      const other = edge.source === id ? edge.target : edge.source;
      const node = this.byId.get(other);
      if (node) {
        nodes.set(node.id, node);
      }
    }
    return [...nodes.values()].sort((a, b) => a.label.localeCompare(b.label));
  }

  communities(): readonly Community[] {
    const groups = new Map<string, GraphNode[]>();
    for (const node of this.snapshot.nodes) {
      const id = node.community ?? 'unassigned';
      const group = groups.get(id) ?? [];
      group.push(node);
      groups.set(id, group);
    }
    return [...groups.entries()]
      .map(([id, nodes]) => ({
        id,
        name: nodes.find((node) => node.communityName)?.communityName ?? (id === 'unassigned' ? 'Unassigned' : `Community ${id}`),
        nodes: nodes.sort((a, b) => this.degree(b.id) - this.degree(a.id) || a.label.localeCompare(b.label)),
      }))
      .sort((a, b) => b.nodes.length - a.nodes.length || a.name.localeCompare(b.name));
  }

  hubs(limit: number): readonly GraphNode[] {
    return [...this.snapshot.nodes]
      .sort((a, b) => this.degree(b.id) - this.degree(a.id) || a.label.localeCompare(b.label))
      .slice(0, Math.max(0, limit));
  }

  search(query: string, limit = 100): readonly GraphNode[] {
    const terms = query.toLocaleLowerCase().split(/\s+/u).filter(Boolean);
    if (terms.length === 0) {
      return [];
    }
    return this.snapshot.nodes
      .map((node) => {
        const label = node.label.toLocaleLowerCase();
        const id = node.id.toLocaleLowerCase();
        const file = node.sourceFile.toLocaleLowerCase();
        let score = 0;
        for (const term of terms) {
          if (label === term) score += 100;
          else if (label.startsWith(term)) score += 40;
          else if (label.includes(term)) score += 20;
          if (id.includes(term)) score += 8;
          if (file.includes(term)) score += 3;
        }
        return { node, score };
      })
      .filter((result) => result.score > 0)
      .sort((a, b) => b.score - a.score || this.degree(b.node.id) - this.degree(a.node.id))
      .slice(0, limit)
      .map((result) => result.node);
  }

  nodesForSourceFile(relativeFile: string): readonly GraphNode[] {
    const normalized = relativeFile.split(path.sep).join('/').replace(/^\.\//u, '');
    return this.snapshot.nodes
      .filter((node) => node.sourceFile.replace(/^\.\//u, '') === normalized)
      .sort((a, b) => sourceLine(a) - sourceLine(b));
  }
}

export function sourceLine(node: GraphNode): number {
  const match = /^L?(\d+)/u.exec(node.sourceLocation ?? '');
  const line = match ? Number.parseInt(match[1] ?? '1', 10) : 1;
  return Number.isFinite(line) && line > 0 ? line : 1;
}

export function basenameForNode(node: GraphNode): string {
  return node.sourceFile ? path.posix.basename(node.sourceFile) : node.fileType;
}
