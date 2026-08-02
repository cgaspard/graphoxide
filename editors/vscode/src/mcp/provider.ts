import * as vscode from 'vscode';
import { extensionInvocation } from './runtime';

const PROVIDER_ID = 'graphoxide.mcpProvider';

export class GraphoxideMcpProvider implements vscode.Disposable {
  private readonly changeEmitter = new vscode.EventEmitter<void>();
  private readonly subscriptions: vscode.Disposable[] = [];

  constructor(
    context: vscode.ExtensionContext,
    private readonly workspaceEnabled: (folder: vscode.WorkspaceFolder) => boolean,
  ) {
    if (typeof vscode.lm?.registerMcpServerDefinitionProvider !== 'function') return;
    this.subscriptions.push(
      vscode.lm.registerMcpServerDefinitionProvider(PROVIDER_ID, {
        onDidChangeMcpServerDefinitions: this.changeEmitter.event,
        provideMcpServerDefinitions: () => this.definitions(context),
      }),
      vscode.workspace.onDidChangeConfiguration((event) => {
        if (event.affectsConfiguration('graphoxide.binaryPath') || event.affectsConfiguration('graphoxide.extraArguments')) this.refresh();
      }),
      vscode.workspace.onDidChangeWorkspaceFolders(() => this.refresh()),
    );
  }

  refresh(): void {
    this.changeEmitter.fire();
  }

  dispose(): void {
    for (const subscription of this.subscriptions) subscription.dispose();
    this.changeEmitter.dispose();
  }

  private definitions(context: vscode.ExtensionContext): vscode.McpStdioServerDefinition[] {
    const folders = vscode.workspace.workspaceFolders?.filter(this.workspaceEnabled) ?? [];
    return folders.map((folder) => {
      const invocation = extensionInvocation(context.extensionUri, folder);
      const label = folders.length > 1 ? `Graphoxide · ${folder.name}` : 'Graphoxide';
      const definition = new vscode.McpStdioServerDefinition(
        label,
        invocation.command,
        [...invocation.args],
        {},
        String(context.extension.packageJSON.version),
      );
      definition.cwd = folder.uri;
      return definition;
    });
  }
}
