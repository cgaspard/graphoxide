import * as vscode from 'vscode';
import { basenameForNode, Community, GraphNode } from './graph';
import { GraphState, GraphStore } from './store';

type ExplorerElement =
  | { readonly kind: 'summary'; readonly state: GraphState }
  | { readonly kind: 'section'; readonly section: 'communities' | 'hubs' | 'files'; readonly label: string; readonly count: number }
  | { readonly kind: 'community'; readonly community: Community }
  | { readonly kind: 'file'; readonly file: string; readonly nodes: readonly GraphNode[] }
  | { readonly kind: 'node'; readonly node: GraphNode };

export class GraphExplorerProvider implements vscode.TreeDataProvider<ExplorerElement>, vscode.Disposable {
  private readonly changeEmitter = new vscode.EventEmitter<ExplorerElement | undefined>();
  private readonly subscription: vscode.Disposable;
  readonly onDidChangeTreeData = this.changeEmitter.event;

  constructor(private readonly store: GraphStore) {
    this.subscription = store.onDidChange(() => this.changeEmitter.fire(undefined));
  }

  refresh(): void {
    this.changeEmitter.fire(undefined);
  }

  getTreeItem(element: ExplorerElement): vscode.TreeItem {
    const model = this.store.state?.model;
    switch (element.kind) {
      case 'summary': {
        const item = new vscode.TreeItem(`${element.state.model?.snapshot.nodes.length ?? 0} nodes · ${element.state.model?.snapshot.edges.length ?? 0} edges`);
        item.description = element.state.folder.name;
        item.iconPath = new vscode.ThemeIcon('graph');
        item.tooltip = new vscode.MarkdownString(`**${element.state.folder.name}**\n\n${element.state.graphUri.fsPath}`);
        item.command = { command: 'graphoxide.openGraph', title: 'Open interactive graph' };
        return item;
      }
      case 'section': {
        const item = new vscode.TreeItem(element.label, vscode.TreeItemCollapsibleState.Collapsed);
        item.description = String(element.count);
        item.iconPath = new vscode.ThemeIcon(element.section === 'communities' ? 'organization' : element.section === 'hubs' ? 'hubot' : 'files');
        return item;
      }
      case 'community': {
        const item = new vscode.TreeItem(element.community.name, vscode.TreeItemCollapsibleState.Collapsed);
        item.description = `${element.community.nodes.length} nodes`;
        item.iconPath = new vscode.ThemeIcon('symbol-namespace');
        item.contextValue = 'graphoxide.community';
        item.tooltip = `Community ${element.community.id} · ${element.community.nodes.length} nodes`;
        return item;
      }
      case 'file': {
        const item = new vscode.TreeItem(element.file, vscode.TreeItemCollapsibleState.Collapsed);
        item.description = `${element.nodes.length}`;
        item.iconPath = vscode.ThemeIcon.File;
        item.resourceUri = this.store.state ? vscode.Uri.joinPath(this.store.state.folder.uri, ...element.file.split('/')) : undefined;
        return item;
      }
      case 'node':
        return nodeTreeItem(element.node, model?.degree(element.node.id));
    }
  }

  getChildren(element?: ExplorerElement): ExplorerElement[] {
    const state = this.store.state;
    const model = state?.model;
    if (!state || !model) return [];
    if (!element) {
      const files = new Set(model.snapshot.nodes.map((node) => node.sourceFile).filter(Boolean));
      return [
        { kind: 'summary', state },
        { kind: 'section', section: 'communities', label: 'Communities', count: model.communities().length },
        { kind: 'section', section: 'hubs', label: 'Architectural Hubs', count: Math.min(20, model.snapshot.nodes.length) },
        { kind: 'section', section: 'files', label: 'Source Files', count: files.size },
      ];
    }
    if (element.kind === 'section') {
      if (element.section === 'communities') return model.communities().map((community) => ({ kind: 'community', community }));
      if (element.section === 'hubs') return model.hubs(20).map((node) => ({ kind: 'node', node }));
      const grouped = new Map<string, GraphNode[]>();
      for (const node of model.snapshot.nodes) {
        if (!node.sourceFile) continue;
        const nodes = grouped.get(node.sourceFile) ?? [];
        nodes.push(node);
        grouped.set(node.sourceFile, nodes);
      }
      return [...grouped.entries()].sort(([a], [b]) => a.localeCompare(b)).map(([file, nodes]) => ({ kind: 'file', file, nodes }));
    }
    if (element.kind === 'community') return element.community.nodes.map((node) => ({ kind: 'node', node }));
    if (element.kind === 'file') return element.nodes.map((node) => ({ kind: 'node', node }));
    if (element.kind === 'node') return model.neighbors(element.node.id).slice(0, 100).map((node) => ({ kind: 'node', node }));
    return [];
  }

  getParent(element: ExplorerElement): ExplorerElement | undefined {
    if (element.kind === 'node') {
      const community = this.store.state?.model?.communities().find((entry) => entry.nodes.some((node) => node.id === element.node.id));
      if (community) return { kind: 'community', community };
    }
    return undefined;
  }

  dispose(): void {
    this.subscription.dispose();
    this.changeEmitter.dispose();
  }
}

export type ResultElement =
  | { readonly kind: 'message'; readonly label: string; readonly description?: string; readonly icon?: string }
  | { readonly kind: 'node'; readonly node: GraphNode };

export class ResultsProvider implements vscode.TreeDataProvider<ResultElement>, vscode.Disposable {
  private values: readonly ResultElement[] = [];
  private readonly changeEmitter = new vscode.EventEmitter<ResultElement | undefined>();
  readonly onDidChangeTreeData = this.changeEmitter.event;

  getTreeItem(element: ResultElement): vscode.TreeItem {
    if (element.kind === 'node') return nodeTreeItem(element.node);
    const item = new vscode.TreeItem(element.label);
    item.description = element.description;
    item.iconPath = new vscode.ThemeIcon(element.icon ?? 'output');
    item.tooltip = [element.label, element.description].filter(Boolean).join(' — ');
    return item;
  }

  getChildren(): ResultElement[] {
    return [...this.values];
  }

  refresh(): void {
    this.changeEmitter.fire(undefined);
  }

  setOutput(title: string, output: string, nodes: readonly GraphNode[] = []): void {
    const lines = output.split(/\r?\n/u).map((line) => line.trim()).filter(Boolean).slice(0, 150);
    this.values = [
      { kind: 'message', label: title, description: `${lines.length} output lines`, icon: 'output' },
      ...nodes.map((node): ResultElement => ({ kind: 'node', node })),
      ...lines.map((line): ResultElement => ({ kind: 'message', label: line.length > 180 ? `${line.slice(0, 177)}…` : line })),
    ];
    this.changeEmitter.fire(undefined);
    void vscode.commands.executeCommand('setContext', 'graphoxide.hasResults', true);
  }

  setNodes(title: string, nodes: readonly GraphNode[]): void {
    this.values = [{ kind: 'message', label: title, description: `${nodes.length} nodes`, icon: 'search' }, ...nodes.map((node) => ({ kind: 'node' as const, node }))];
    this.changeEmitter.fire(undefined);
    void vscode.commands.executeCommand('setContext', 'graphoxide.hasResults', true);
  }

  clear(): void {
    this.values = [];
    this.changeEmitter.fire(undefined);
    void vscode.commands.executeCommand('setContext', 'graphoxide.hasResults', false);
  }

  dispose(): void {
    this.changeEmitter.dispose();
  }
}

function nodeTreeItem(node: GraphNode, degree?: number): vscode.TreeItem {
  const item = new vscode.TreeItem(node.label, vscode.TreeItemCollapsibleState.None);
  item.id = node.id;
  item.description = degree === undefined ? basenameForNode(node) : `${basenameForNode(node)} · ${degree}`;
  item.iconPath = new vscode.ThemeIcon(iconForNode(node));
  item.contextValue = 'graphoxide.node';
  item.tooltip = new vscode.MarkdownString(`**${node.label}**\n\n\`${node.id}\`\n\n${node.sourceFile || node.fileType}${node.sourceLocation ? `:${node.sourceLocation}` : ''}`);
  // Source links can be disabled (graphoxide.sourceLinks.enabled) to keep the
  // trees informational without opening editors.
  if (vscode.workspace.getConfiguration('graphoxide').get<boolean>('sourceLinks.enabled', true)) {
    item.command = { command: 'graphoxide.revealNode', title: 'Reveal node', arguments: [node] };
  }
  return item;
}

function iconForNode(node: GraphNode): string {
  if (node.fileType === 'document' || node.fileType === 'paper') return 'book';
  if (node.fileType === 'image') return 'file-media';
  if (node.fileType === 'concept') return 'lightbulb';
  if (/class|struct|interface|type/u.test(node.id)) return 'symbol-class';
  if (/module|file|package/u.test(node.id)) return 'symbol-module';
  return 'symbol-method';
}

export function nodeFromArgument(value: unknown): GraphNode | undefined {
  if (typeof value !== 'object' || value === null) return undefined;
  if ('node' in value) return nodeFromArgument((value as { node: unknown }).node);
  if ('id' in value && 'label' in value) return value as GraphNode;
  return undefined;
}

export function communityFromArgument(value: unknown): Community | undefined {
  if (typeof value !== 'object' || value === null) return undefined;
  if ('community' in value) return communityFromArgument((value as { community: unknown }).community);
  if ('id' in value && 'nodes' in value && Array.isArray((value as { nodes: unknown }).nodes)) return value as unknown as Community;
  return undefined;
}
