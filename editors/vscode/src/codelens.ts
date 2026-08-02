import * as path from 'node:path';
import * as vscode from 'vscode';
import { sourceLine } from './graph';
import { GraphStore } from './store';

export class GraphCodeLensProvider implements vscode.CodeLensProvider, vscode.Disposable {
  private readonly changeEmitter = new vscode.EventEmitter<void>();
  private readonly subscription: vscode.Disposable;
  readonly onDidChangeCodeLenses = this.changeEmitter.event;

  constructor(private readonly store: GraphStore) {
    this.subscription = store.onDidChange(() => this.changeEmitter.fire());
  }

  refresh(): void {
    this.changeEmitter.fire();
  }

  provideCodeLenses(document: vscode.TextDocument): vscode.CodeLens[] {
    if (!vscode.workspace.getConfiguration('graphoxide', document.uri).get<boolean>('codeLens.enabled', true)) return [];
    const state = this.store.state;
    if (!state?.model || document.uri.scheme !== 'file') return [];
    const relative = path.relative(state.folder.uri.fsPath, document.uri.fsPath);
    if (relative.startsWith('..') || path.isAbsolute(relative)) return [];
    return state.model.nodesForSourceFile(relative).slice(0, 250).map((node) => {
      const line = Math.min(Math.max(sourceLine(node) - 1, 0), Math.max(document.lineCount - 1, 0));
      const degree = state.model?.degree(node.id) ?? 0;
      return new vscode.CodeLens(new vscode.Range(line, 0, line, 0), {
        command: 'graphoxide.explain',
        title: `$(type-hierarchy) ${degree} graph connection${degree === 1 ? '' : 's'}`,
        tooltip: `Explain ${node.label} with Graphoxide`,
        arguments: [node],
      });
    });
  }

  dispose(): void {
    this.subscription.dispose();
    this.changeEmitter.dispose();
  }
}
