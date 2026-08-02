import * as path from 'node:path';
import * as vscode from 'vscode';
import { GraphoxideCli } from './cli';
import { GraphCodeLensProvider } from './codelens';
import { GraphNode } from './graph';
import { ManagedWorkspaceService } from './managed';
import { McpManagerPanel } from './mcp/manager';
import { GraphoxideMcpProvider } from './mcp/provider';
import { GraphStore } from './store';
import { communityFromArgument, GraphExplorerProvider, nodeFromArgument, ResultsProvider } from './tree';
import { GraphVisualizer } from './visualizer';

interface ExtensionServices {
  readonly store: GraphStore;
  readonly cli: GraphoxideCli;
  readonly explorer: GraphExplorerProvider;
  readonly results: ResultsProvider;
  readonly visualizer: GraphVisualizer;
  readonly statusBar: vscode.StatusBarItem;
  readonly managed: ManagedWorkspaceService;
}

export interface GraphoxideExtensionStatus {
  readonly enabled: boolean;
  readonly freshness: 'watch' | 'save' | 'manual';
  readonly watching: boolean;
  readonly graphPath?: string;
  readonly nodes?: number;
  readonly edges?: number;
  readonly mcp?: { readonly command: string; readonly args: readonly string[]; readonly cwd: string };
}

export interface GraphoxideExtensionApi {
  readonly version: 1;
  enableWorkspace(freshness?: 'watch' | 'save' | 'manual'): Promise<void>;
  configureFreshness(freshness: 'watch' | 'save' | 'manual'): Promise<void>;
  status(): Promise<GraphoxideExtensionStatus>;
}

export async function activate(context: vscode.ExtensionContext): Promise<GraphoxideExtensionApi> {
  const store = new GraphStore();
  const cli = new GraphoxideCli(context.extensionUri);
  const explorer = new GraphExplorerProvider(store);
  const results = new ResultsProvider();
  const statusBar = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 40);
  statusBar.name = 'Graphoxide';
  statusBar.command = 'graphoxide.showStatus';
  const visualizer = new GraphVisualizer(
    context.extensionUri,
    (id) => void revealNodeById(store, id),
    (id) => void vscode.commands.executeCommand('graphoxide.explain', store.state?.model?.getNode(id)),
  );
  const managed = new ManagedWorkspaceService(context, store, cli);
  const mcpProvider = new GraphoxideMcpProvider(context, (folder) => managed.isEnabled(folder));
  const services: ExtensionServices = { store, cli, explorer, results, visualizer, statusBar, managed };
  const codeLens = new GraphCodeLensProvider(store);

  context.subscriptions.push(
    store,
    cli,
    explorer,
    results,
    visualizer,
    codeLens,
    statusBar,
    managed,
    mcpProvider,
    vscode.window.registerTreeDataProvider('graphoxide.explorer', explorer),
    vscode.window.registerTreeDataProvider('graphoxide.results', results),
    vscode.languages.registerCodeLensProvider({ scheme: 'file' }, codeLens),
    store.onDidChange((state) => {
      updateStatusBar(statusBar, state?.model?.snapshot.nodes.length, state?.model?.snapshot.edges.length, state?.error, cli.watching);
      if (state?.model) visualizer.refresh(state.model);
    }),
    cli.onDidChangeWatch(() => updateStatusBar(statusBar, store.state?.model?.snapshot.nodes.length, store.state?.model?.snapshot.edges.length, store.state?.error, cli.watching)),
    managed.onDidChangeEnablement(() => mcpProvider.refresh()),
    vscode.workspace.onDidChangeConfiguration((event) => {
      if (event.affectsConfiguration('graphoxide.graphPath')) void store.initialize();
      if (event.affectsConfiguration('graphoxide.codeLens.enabled')) codeLens.refresh();
    }),
    vscode.window.onDidChangeActiveTextEditor((editor) => {
      const folder = editor ? vscode.workspace.getWorkspaceFolder(editor.document.uri) : undefined;
      if (folder && folder.uri.toString() !== store.state?.folder.uri.toString()) void store.load(folder);
    }),
    ...registerCommands(context, services),
    registerUpdateOnSave(services),
  );

  await vscode.commands.executeCommand('setContext', 'graphoxide.hasResults', false);
  await vscode.commands.executeCommand('setContext', 'graphoxide.watching', false);
  await store.initialize();
  updateStatusBar(statusBar, store.state?.model?.snapshot.nodes.length, store.state?.model?.snapshot.edges.length, store.state?.error, false);
  statusBar.show();
  void managed.start();
  return {
    version: 1,
    enableWorkspace: (freshness = 'manual') => managed.enable(undefined, freshness, false),
    configureFreshness: (freshness) => managed.configureFreshness(undefined, freshness),
    status: async () => {
      const folder = store.state?.folder ?? await store.preferredFolder(false);
      const state = folder ? await store.load(folder) : undefined;
      return {
        enabled: folder ? managed.isEnabled(folder) : false,
        freshness: folder ? managed.freshness(folder) : 'manual',
        watching: cli.watching,
        ...(state ? { graphPath: state.graphUri.fsPath } : {}),
        ...(state?.model ? { nodes: state.model.snapshot.nodes.length, edges: state.model.snapshot.edges.length } : {}),
        ...(folder ? { mcp: cli.invocation(folder) } : {}),
      };
    },
  };
}

export function deactivate(): void {
  // VS Code disposes everything registered in the extension context.
}

function registerCommands(context: vscode.ExtensionContext, services: ExtensionServices): vscode.Disposable[] {
  const { store, cli, explorer, results, visualizer, managed } = services;
  const command = (id: string, handler: (...args: unknown[]) => unknown): vscode.Disposable =>
    vscode.commands.registerCommand(id, (...args: unknown[]) => Promise.resolve(handler(...args)).catch((error: unknown) => handleError(error)));

  return [
    command('graphoxide.initialize', async () => {
      const folder = await requireFolder(store);
      if (!folder) return;
      const replacingCurrent = store.state?.folder.uri.toString() === folder.uri.toString() && Boolean(store.state?.model);
      const force = replacingCurrent
        ? await vscode.window.showWarningMessage('Replace the existing Graphoxide graph?', { modal: true }, 'Replace')
        : 'Replace';
      if (force !== 'Replace') return;
      await cli.run({ title: 'Graphoxide: extracting workspace…', folder, args: ['extract', folder.uri.fsPath, ...(replacingCurrent ? ['--force'] : [])] });
      await store.load(folder);
      void vscode.window.showInformationMessage('Graphoxide extraction complete.');
    }),
    command('graphoxide.update', async () => {
      const folder = await requireFolder(store);
      if (!folder) return;
      await cli.run({ title: 'Graphoxide: updating graph…', folder, args: ['update', folder.uri.fsPath] });
      await store.load(folder);
    }),
    command('graphoxide.startWatch', async () => {
      const folder = await requireFolder(store);
      if (folder) cli.startWatch(folder);
    }),
    command('graphoxide.stopWatch', () => cli.stopWatch()),
    command('graphoxide.refresh', async () => {
      await store.load();
      explorer.refresh();
    }),
    command('graphoxide.openGraph', () => {
      const model = store.state?.model;
      if (!model) return missingGraph();
      visualizer.show(model);
    }),
    command('graphoxide.openGraphBeside', () => {
      const model = store.state?.model;
      if (!model) return missingGraph();
      visualizer.show(model, undefined, 'beside');
    }),
    command('graphoxide.openGraphFile', async () => {
      if (!store.state?.model) return missingGraph();
      await vscode.window.showTextDocument(store.state.graphUri, { preview: true });
    }),
    command('graphoxide.query', async () => {
      const state = store.state;
      if (!state?.model) return missingGraph();
      const question = await vscode.window.showInputBox({ title: 'Query Graphoxide', prompt: 'Ask a question about this codebase', placeHolder: 'How does authentication work?', ignoreFocusOut: true });
      if (!question?.trim()) return;
      const config = vscode.workspace.getConfiguration('graphoxide', state.folder.uri);
      const budget = config.get<number>('defaultTokenBudget', 4000);
      const traversal = config.get<string>('queryTraversal', 'breadth-first');
      const args = ['query', question.trim(), '--budget', String(budget), '--graph', state.graphUri.fsPath];
      if (traversal === 'depth-first') args.push('--dfs');
      const result = await cli.run({ title: 'Graphoxide: querying graph…', folder: state.folder, args });
      results.setOutput(question.trim(), result.stdout, nodesMentioned(state.model.snapshot.nodes, result.stdout));
      await vscode.commands.executeCommand('graphoxide.results.focus');
    }),
    command('graphoxide.explain', async (value?: unknown) => {
      const state = store.state;
      if (!state?.model) return missingGraph();
      const node = nodeFromArgument(value) ?? await store.chooseNode({ title: 'Explain a Graphoxide node' });
      if (!node) return;
      const result = await cli.run({ title: `Graphoxide: explaining ${node.label}…`, folder: state.folder, args: ['explain', node.id, '--graph', state.graphUri.fsPath] });
      results.setOutput(`Explain · ${node.label}`, result.stdout, [node, ...nodesMentioned(state.model.snapshot.nodes, result.stdout).filter((other) => other.id !== node.id)]);
      await vscode.commands.executeCommand('graphoxide.results.focus');
    }),
    command('graphoxide.explainAtCursor', async () => {
      const state = store.state;
      const editor = vscode.window.activeTextEditor;
      if (!state?.model || !editor) return missingGraph();
      const relative = safeRelativePath(state.folder.uri.fsPath, editor.document.uri.fsPath);
      if (!relative) return void vscode.window.showInformationMessage('The active file is outside the indexed workspace.');
      const wordRange = editor.document.getWordRangeAtPosition(editor.selection.active);
      const word = wordRange ? editor.document.getText(wordRange) : '';
      const fileNodes = state.model.nodesForSourceFile(relative);
      const exact = fileNodes.filter((node) => node.label.toLocaleLowerCase() === word.toLocaleLowerCase());
      const node = exact[0] ?? (fileNodes.length === 1 ? fileNodes[0] : await store.chooseNode({ title: `Symbols in ${path.basename(relative)}`, nodes: fileNodes }));
      if (!node) return void vscode.window.showInformationMessage('No indexed graph node was found at the cursor.');
      await vscode.commands.executeCommand('graphoxide.explain', node);
    }),
    command('graphoxide.path', async () => {
      const state = store.state;
      if (!state?.model) return missingGraph();
      const source = await store.chooseNode({ title: 'Path: choose the starting node' });
      if (!source) return;
      const target = await store.chooseNode({ title: 'Path: choose the destination node' });
      if (!target) return;
      const result = await cli.run({ title: `Graphoxide: finding path from ${source.label} to ${target.label}…`, folder: state.folder, args: ['path', source.id, target.id, '--graph', state.graphUri.fsPath] });
      results.setOutput(`${source.label} → ${target.label}`, result.stdout, [source, ...nodesMentioned(state.model.snapshot.nodes, result.stdout).filter((node) => node.id !== source.id)]);
      await vscode.commands.executeCommand('graphoxide.results.focus');
    }),
    command('graphoxide.affected', async (value?: unknown) => {
      const state = store.state;
      if (!state?.model) return missingGraph();
      const node = nodeFromArgument(value) ?? await store.chooseNode({ title: 'Find nodes affected by…' });
      if (!node) return;
      const depth = vscode.workspace.getConfiguration('graphoxide', state.folder.uri).get<number>('defaultAffectedDepth', 2);
      const result = await cli.run({ title: `Graphoxide: tracing impact from ${node.label}…`, folder: state.folder, args: ['affected', node.id, '--depth', String(depth), '--graph', state.graphUri.fsPath] });
      results.setOutput(`Affected by ${node.label}`, result.stdout, [node, ...nodesMentioned(state.model.snapshot.nodes, result.stdout).filter((other) => other.id !== node.id)]);
      await vscode.commands.executeCommand('graphoxide.results.focus');
    }),
    command('graphoxide.godNodes', async () => {
      const state = store.state;
      if (!state?.model) return missingGraph();
      const result = await cli.run({ title: 'Graphoxide: finding architectural hubs…', folder: state.folder, args: ['god-nodes', '--top', '25', '--json', '--graph', state.graphUri.fsPath] });
      results.setNodes('Architectural hubs', state.model.hubs(25));
      cli.output.info(result.stdout.trim());
      await vscode.commands.executeCommand('graphoxide.results.focus');
    }),
    command('graphoxide.revealNode', async (value?: unknown) => {
      const node = nodeFromArgument(value) ?? await store.chooseNode({ title: 'Open a graph node in source' });
      if (node) await revealNode(store, node);
    }),
    command('graphoxide.showCommunity', (value?: unknown) => {
      const model = store.state?.model;
      const community = communityFromArgument(value);
      if (model && community) visualizer.show(model, community.id);
    }),
    command('graphoxide.report', async () => {
      const state = store.state;
      if (!state?.model) return missingGraph();
      const defaultUri = vscode.Uri.joinPath(state.folder.uri, 'graphoxide-out', 'GRAPH_REPORT.md');
      const output = await vscode.window.showSaveDialog({ title: 'Save Graphoxide architecture report', defaultUri, filters: { Markdown: ['md'] } });
      if (!output) return;
      await cli.run({ title: 'Graphoxide: generating architecture report…', folder: state.folder, args: ['report', '--graph', state.graphUri.fsPath, '--output', output.fsPath] });
      await vscode.window.showTextDocument(output, { preview: false });
    }),
    command('graphoxide.export', async () => {
      const state = store.state;
      if (!state?.model) return missingGraph();
      const picked = await vscode.window.showQuickPick([
        { label: 'Interactive HTML', format: 'html', extension: 'html' },
        { label: 'Call-flow HTML', format: 'callflow-html', extension: 'html' },
        { label: 'GraphML', format: 'graphml', extension: 'graphml' },
        { label: 'Cypher', format: 'cypher', extension: 'cypher' },
        { label: 'JSON', format: 'json', extension: 'json' },
        { label: 'Obsidian vault', format: 'obsidian', extension: '' },
      ], { title: 'Export Graphoxide graph' });
      if (!picked) return;
      let output: vscode.Uri | undefined;
      if (picked.format === 'obsidian') {
        output = (await vscode.window.showOpenDialog({ title: 'Choose Obsidian vault folder', canSelectFolders: true, canSelectFiles: false, canSelectMany: false }))?.[0];
      } else {
        output = await vscode.window.showSaveDialog({ defaultUri: vscode.Uri.joinPath(state.folder.uri, `graphoxide-export.${picked.extension}`) });
      }
      if (!output) return;
      await cli.run({ title: `Graphoxide: exporting ${picked.label}…`, folder: state.folder, args: ['export', picked.format, output.fsPath, '--graph', state.graphUri.fsPath] });
      void vscode.window.showInformationMessage(`Graphoxide export written to ${output.fsPath}`, 'Open').then(async (choice) => {
        if (choice === 'Open') await vscode.commands.executeCommand('revealFileInOS', output);
      });
    }),
    command('graphoxide.startServer', async () => {
      const folder = await requireFolder(store);
      if (folder) cli.openServerTerminal(folder);
    }),
    command('graphoxide.copyMcpConfig', async () => {
      const folder = await requireFolder(store);
      if (!folder) return;
      const invocation = cli.invocation(folder);
      const value = {
        mcpServers: {
          graphoxide: {
            command: invocation.command,
            args: invocation.args,
            cwd: invocation.cwd,
          },
        },
      };
      await vscode.env.clipboard.writeText(JSON.stringify(value, null, 2));
      void vscode.window.showInformationMessage('Graphoxide MCP configuration copied to the clipboard.');
    }),
    command('graphoxide.manageMcp', () => McpManagerPanel.show(context, () => {
      const folder = vscode.workspace.workspaceFolders?.[0];
      return folder ? managed.isEnabled(folder) : false;
    })),
    command('graphoxide.enableWorkspace', (freshness?: unknown) => managed.enable(undefined, isFreshnessMode(freshness) ? freshness : undefined)),
    command('graphoxide.disableWorkspace', () => managed.disable()),
    command('graphoxide.configureFreshness', (freshness?: unknown) => managed.configureFreshness(undefined, isFreshnessMode(freshness) ? freshness : undefined)),
    command('graphoxide.resetWorkspacePrompt', () => managed.resetPrompt()),
    command('graphoxide.showStatus', () => showStatus(services)),
    command('graphoxide.openSettings', () => vscode.commands.executeCommand('workbench.action.openSettings', '@ext:cgaspard.graphoxide-vscode')),
    command('graphoxide.clearResults', () => results.clear()),
  ];
}

function registerUpdateOnSave(services: ExtensionServices): vscode.Disposable {
  let timer: NodeJS.Timeout | undefined;
  let running = false;
  let pending = false;
  const update = async (): Promise<void> => {
    if (running) {
      pending = true;
      return;
    }
    const folder = services.store.state?.folder;
    if (!folder) return;
    running = true;
    try {
      await services.cli.run({ title: 'Graphoxide: updating after save…', folder, args: ['update', folder.uri.fsPath], showProgress: false, cancellable: false });
      await services.store.load(folder);
    } catch (error) {
      handleError(error);
    } finally {
      running = false;
      if (pending) {
        pending = false;
        void update();
      }
    }
  };
  return vscode.workspace.onDidSaveTextDocument((document) => {
    const state = services.store.state;
    const configured = vscode.workspace.getConfiguration('graphoxide', document.uri).get<boolean>('updateOnSave', false);
    const managed = state ? services.managed.freshness(state.folder) === 'save' : false;
    if (!state || (!configured && !managed)) return;
    const relative = safeRelativePath(state.folder.uri.fsPath, document.uri.fsPath);
    if (!relative || relative.startsWith('graphoxide-out/')) return;
    if (timer) clearTimeout(timer);
    const delay = vscode.workspace.getConfiguration('graphoxide', document.uri).get<number>('updateOnSaveDelay', 1200);
    timer = setTimeout(() => void update(), delay);
  });
}

async function requireFolder(store: GraphStore): Promise<vscode.WorkspaceFolder | undefined> {
  const folder = await store.preferredFolder(true);
  if (!folder) void vscode.window.showErrorMessage('Open a folder or workspace to use Graphoxide.');
  return folder;
}

function missingGraph(): void {
  void vscode.window.showInformationMessage('No Graphoxide graph was found.', 'Extract workspace').then(async (choice) => {
    if (choice) await vscode.commands.executeCommand('graphoxide.initialize');
  });
}

async function revealNodeById(store: GraphStore, id: string): Promise<void> {
  const node = store.state?.model?.getNode(id);
  if (node) await revealNode(store, node);
}

async function revealNode(store: GraphStore, node: GraphNode): Promise<void> {
  const state = store.state;
  if (!state || !node.sourceFile) {
    void vscode.window.showInformationMessage(`${node.label} has no source location.`);
    return;
  }
  const relative = node.sourceFile.replace(/\\/gu, '/');
  if (path.posix.isAbsolute(relative) || relative.split('/').includes('..')) {
    void vscode.window.showErrorMessage(`Graphoxide refused an unsafe source path: ${node.sourceFile}`);
    return;
  }
  const uri = vscode.Uri.joinPath(state.folder.uri, ...relative.split('/'));
  const lineMatch = /^L?(\d+)/u.exec(node.sourceLocation ?? '');
  const requestedLine = Math.max(0, Number.parseInt(lineMatch?.[1] ?? '1', 10) - 1);
  try {
    const document = await vscode.workspace.openTextDocument(uri);
    const line = Math.min(requestedLine, Math.max(0, document.lineCount - 1));
    const editor = await vscode.window.showTextDocument(document, { preview: true });
    const range = document.lineAt(line).range;
    editor.selection = new vscode.Selection(range.start, range.start);
    editor.revealRange(range, vscode.TextEditorRevealType.InCenterIfOutsideViewport);
  } catch (error) {
    throw new Error(`Could not open ${node.sourceFile}: ${error instanceof Error ? error.message : String(error)}`);
  }
}

function nodesMentioned(nodes: readonly GraphNode[], output: string): readonly GraphNode[] {
  const normalized = output.toLocaleLowerCase();
  return nodes.filter((node) => normalized.includes(node.id.toLocaleLowerCase()) || (node.label.length >= 3 && normalized.includes(node.label.toLocaleLowerCase()))).slice(0, 100);
}

function safeRelativePath(root: string, file: string): string | undefined {
  const relative = path.relative(root, file);
  return relative.startsWith('..') || path.isAbsolute(relative) ? undefined : relative.split(path.sep).join('/');
}

function updateStatusBar(item: vscode.StatusBarItem, nodes?: number, edges?: number, error?: string, watching = false): void {
  if (error) {
    item.text = '$(error) Graphoxide';
    item.tooltip = `Graphoxide could not load graph.json: ${error}`;
    item.backgroundColor = new vscode.ThemeColor('statusBarItem.errorBackground');
  } else if (nodes === undefined) {
    item.text = '$(circle-slash) Graphoxide';
    item.tooltip = 'No Graphoxide graph found. Click to get started.';
    item.backgroundColor = undefined;
  } else {
    item.text = `${watching ? '$(eye)' : '$(type-hierarchy)'} Graphoxide ${nodes}`;
    item.tooltip = `${nodes} nodes · ${edges ?? 0} edges${watching ? ' · watch mode active' : ''}`;
    item.backgroundColor = undefined;
  }
}

async function showStatus(services: ExtensionServices): Promise<void> {
  const state = services.store.state;
  if (!state?.model) {
    const choice = await vscode.window.showQuickPick([
      { label: '$(sparkle) Enable managed workspace', command: 'graphoxide.enableWorkspace' },
      { label: '$(play) Extract workspace once', command: 'graphoxide.initialize' },
      { label: '$(server-process) Manage MCP integrations', command: 'graphoxide.manageMcp' },
    ], { title: 'Graphoxide · No graph found' });
    if (choice) await vscode.commands.executeCommand(choice.command);
    return;
  }
  const snapshot = state.model.snapshot;
  const managed = services.managed.isEnabled(state.folder);
  const choice = await vscode.window.showQuickPick([
    { label: '$(type-hierarchy) Open interactive graph', command: 'graphoxide.openGraph' },
    { label: '$(split-horizontal) Open interactive graph beside', command: 'graphoxide.openGraphBeside' },
    { label: '$(search) Query graph', command: 'graphoxide.query' },
    { label: '$(refresh) Update graph', command: 'graphoxide.update' },
    { label: '$(server-process) Manage MCP integrations', command: 'graphoxide.manageMcp' },
    { label: '$(settings) Configure automatic updates', command: 'graphoxide.configureFreshness' },
    { label: managed ? '$(circle-slash) Disable managed workspace' : '$(sparkle) Enable managed workspace', command: managed ? 'graphoxide.disableWorkspace' : 'graphoxide.enableWorkspace' },
    { label: services.cli.watching ? '$(debug-stop) Stop watch mode' : '$(eye) Start watch mode', command: services.cli.watching ? 'graphoxide.stopWatch' : 'graphoxide.startWatch' },
    { label: '$(json) Open graph.json', command: 'graphoxide.openGraphFile' },
  ], {
    title: `Graphoxide · ${snapshot.nodes.length} nodes · ${snapshot.edges.length} edges · ${state.model.communities().length} communities`,
    placeHolder: state.graphUri.fsPath,
  });
  if (choice) await vscode.commands.executeCommand(choice.command);
}

function handleError(error: unknown): void {
  if (error instanceof vscode.CancellationError) return;
  const message = error instanceof Error ? error.message : String(error);
  void vscode.window.showErrorMessage(`Graphoxide: ${message}`);
}

function isFreshnessMode(value: unknown): value is 'watch' | 'save' | 'manual' {
  return value === 'watch' || value === 'save' || value === 'manual';
}
