import * as path from 'node:path';
import * as vscode from 'vscode';
import { graphBuildOutputDirectory } from './build';
import { GraphModel, GraphNode, parseGraphJson } from './graph';

export interface ManagedGraphOutput {
  readonly graphUri: vscode.Uri;
  readonly outputDirectory: string;
  readonly environment: Readonly<{ GRAPHOXIDE_OUT: string }>;
}

export interface GraphState {
  readonly folder: vscode.WorkspaceFolder;
  readonly graphUri: vscode.Uri;
  readonly graphFileExists: boolean;
  readonly model?: GraphModel;
  readonly error?: string;
  readonly modified?: number;
}

export class GraphStore implements vscode.Disposable {
  private stateValue?: GraphState;
  private readonly changeEmitter = new vscode.EventEmitter<GraphState | undefined>();
  private watcher?: vscode.FileSystemWatcher;
  readonly onDidChange = this.changeEmitter.event;

  get state(): GraphState | undefined {
    return this.stateValue;
  }

  async initialize(): Promise<void> {
    const folder = await this.preferredFolder(false);
    if (folder) {
      await this.load(folder);
    } else {
      this.setState(undefined);
    }
    this.resetWatcher(folder);
  }

  async preferredFolder(promptWhenMultiple = true): Promise<vscode.WorkspaceFolder | undefined> {
    const folders = vscode.workspace.workspaceFolders;
    if (!folders?.length) {
      return undefined;
    }
    const active = vscode.window.activeTextEditor
      ? vscode.workspace.getWorkspaceFolder(vscode.window.activeTextEditor.document.uri)
      : undefined;
    if (active) {
      return active;
    }
    if (folders.length === 1 || !promptWhenMultiple) {
      return folders[0];
    }
    const selected = await vscode.window.showQuickPick(
      folders.map((folder) => ({ label: folder.name, description: folder.uri.fsPath, folder })),
      { title: 'Choose the Graphoxide workspace' },
    );
    return selected?.folder;
  }

  graphUri(folder: vscode.WorkspaceFolder): vscode.Uri {
    const configured = vscode.workspace.getConfiguration('graphoxide', folder.uri).get<string>('graphPath', 'graphoxide-out/graph.json');
    return path.isAbsolute(configured) ? vscode.Uri.file(configured) : vscode.Uri.joinPath(folder.uri, ...configured.split(/[\\/]/u));
  }

  managedOutput(folder: vscode.WorkspaceFolder): ManagedGraphOutput {
    const graphUri = this.graphUri(folder);
    const outputDirectory = graphBuildOutputDirectory(folder.uri.fsPath, graphUri.fsPath);
    return { graphUri, outputDirectory, environment: { GRAPHOXIDE_OUT: outputDirectory } };
  }

  async load(folder?: vscode.WorkspaceFolder): Promise<GraphState | undefined> {
    const target = folder ?? this.stateValue?.folder ?? await this.preferredFolder(false);
    if (!target) {
      this.setState(undefined);
      return undefined;
    }
    const graphUri = this.graphUri(target);
    const watchedGraphChanged = this.stateValue?.folder.uri.toString() !== target.uri.toString()
      || this.stateValue?.graphUri.toString() !== graphUri.toString();
    try {
      const [bytes, stat] = await Promise.all([vscode.workspace.fs.readFile(graphUri), vscode.workspace.fs.stat(graphUri)]);
      const model = new GraphModel(parseGraphJson(new TextDecoder().decode(bytes)));
      const next = { folder: target, graphUri, graphFileExists: true, model, modified: stat.mtime };
      this.setState(next);
      if (watchedGraphChanged) this.resetWatcher(target);
      return next;
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      const missing = /FileNotFound|ENOENT|EntryNotFound/u.test(message);
      const next = { folder: target, graphUri, graphFileExists: !missing, error: missing ? undefined : message };
      this.setState(next);
      if (watchedGraphChanged) this.resetWatcher(target);
      return next;
    }
  }

  async chooseNode(options: { title: string; placeHolder?: string; nodes?: readonly GraphNode[] }): Promise<GraphNode | undefined> {
    const model = this.stateValue?.model;
    if (!model) {
      await vscode.window.showInformationMessage('Build this workspace graph before selecting a graph node.', 'Build Graph').then(async (choice) => {
        if (choice === 'Build Graph') await vscode.commands.executeCommand('graphoxide.initialize');
      });
      return undefined;
    }
    const nodes = options.nodes ?? model.snapshot.nodes;
    const selected = await vscode.window.showQuickPick(
      nodes.slice(0, 20000).map((node) => ({
        label: `$(symbol-method) ${node.label}`,
        description: node.sourceFile || node.fileType,
        detail: `${node.id} · ${model.degree(node.id)} connections`,
        node,
      })),
      { title: options.title, placeHolder: options.placeHolder, matchOnDescription: true, matchOnDetail: true },
    );
    return selected?.node;
  }

  dispose(): void {
    this.watcher?.dispose();
    this.changeEmitter.dispose();
  }

  private setState(state: GraphState | undefined): void {
    this.stateValue = state;
    this.changeEmitter.fire(state);
    void vscode.commands.executeCommand('setContext', 'graphoxide.hasGraph', Boolean(state?.model));
    void vscode.commands.executeCommand('setContext', 'graphoxide.hasGraphFile', Boolean(state?.graphFileExists));
  }

  private resetWatcher(folder?: vscode.WorkspaceFolder): void {
    this.watcher?.dispose();
    const target = folder ?? this.stateValue?.folder;
    const configured = target ? vscode.workspace.getConfiguration('graphoxide', target.uri).get<string>('graphPath', 'graphoxide-out/graph.json') : undefined;
    const pattern = target && configured
      ? path.isAbsolute(configured)
        ? new vscode.RelativePattern(path.dirname(configured), path.basename(configured))
        : new vscode.RelativePattern(target, configured)
      : '**/graphoxide-out/graph.json';
    this.watcher = vscode.workspace.createFileSystemWatcher(pattern);
    const refresh = (uri: vscode.Uri): void => {
      if (!vscode.workspace.getConfiguration('graphoxide', uri).get<boolean>('autoRefresh', true)) return;
      const current = this.stateValue;
      if (!current || uri.fsPath === current.graphUri.fsPath) void this.load(vscode.workspace.getWorkspaceFolder(uri));
    };
    this.watcher.onDidCreate(refresh);
    this.watcher.onDidChange(refresh);
    this.watcher.onDidDelete(refresh);
  }
}
