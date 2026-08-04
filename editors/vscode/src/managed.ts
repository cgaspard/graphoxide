import * as vscode from 'vscode';
import { GraphoxideCli } from './cli';
import { integrationReports } from './mcp/installers';
import { resolvedInvocation } from './mcp/runtime';
import { GraphStore } from './store';

export type FreshnessMode = 'watch' | 'save' | 'manual';

export class ManagedWorkspaceService implements vscode.Disposable {
  private readonly changeEmitter = new vscode.EventEmitter<void>();
  private readonly subscriptions: vscode.Disposable[] = [];
  readonly onDidChangeEnablement = this.changeEmitter.event;

  constructor(
    private readonly context: vscode.ExtensionContext,
    private readonly store: GraphStore,
    private readonly cli: GraphoxideCli,
  ) {
    this.subscriptions.push(vscode.workspace.onDidGrantWorkspaceTrust(() => void this.start()));
  }

  isEnabled(folder: vscode.WorkspaceFolder): boolean {
    return this.context.workspaceState.get<boolean>(this.enabledKey(folder)) === true;
  }

  freshness(folder: vscode.WorkspaceFolder): FreshnessMode {
    return this.context.workspaceState.get<FreshnessMode>(this.freshnessKey(folder), 'manual');
  }

  async start(): Promise<void> {
    if (!vscode.workspace.isTrusted) return;
    const folder = await this.store.preferredFolder(false);
    if (!folder) return;
    const state = this.context.workspaceState.get<boolean>(this.enabledKey(folder));
    if (state === true) {
      await this.resume(folder);
    } else if (state === undefined && vscode.workspace.getConfiguration('graphoxide', folder.uri).get<boolean>('promptOnFirstOpen', true)) {
      await this.prompt(folder);
    }
  }

  async enable(
    folder?: vscode.WorkspaceFolder,
    preferredFreshness?: FreshnessMode,
    offerExternalIntegrations = true,
  ): Promise<void> {
    if (!vscode.workspace.isTrusted) {
      void vscode.window.showWarningMessage('Trust this workspace before enabling Graphoxide.');
      return;
    }
    const target = folder ?? await this.store.preferredFolder(true);
    if (!target) return;
    let environment: Readonly<{ GRAPHOXIDE_OUT: string }>;
    try {
      environment = this.store.managedOutput(target).environment;
      const state = await this.store.load(target);
      if (state?.model) {
        await this.cli.run({ title: 'Graphoxide: synchronizing workspace graph…', folder: target, args: ['update', target.uri.fsPath, '--force'], environment });
      } else {
        await this.cli.run({ title: 'Graphoxide: building workspace graph…', folder: target, args: ['extract', target.uri.fsPath], environment });
      }
      await this.store.load(target);
    } catch (error) {
      await this.context.workspaceState.update(this.enabledKey(target), undefined);
      const action = await vscode.window.showErrorMessage(
        `Graphoxide could not initialize this workspace: ${error instanceof Error ? error.message : String(error)}`,
        'Open settings',
      );
      if (action === 'Open settings') await vscode.commands.executeCommand('graphoxide.openSettings');
      return;
    }

    await this.context.workspaceState.update(this.enabledKey(target), true);
    this.changeEmitter.fire();
    const mode = preferredFreshness ?? await this.chooseFreshness(target) ?? 'manual';
    await this.context.workspaceState.update(this.freshnessKey(target), mode);
    if (mode === 'watch') await this.cli.startWatch(target, environment);

    if (!offerExternalIntegrations) {
      void vscode.window.showInformationMessage(`Graphoxide is managing ${target.name} with ${freshnessDescription(mode)}.`);
      return;
    }
    const toolNames = await this.detectUnconfiguredTools(target);
    if (toolNames.length > 0) {
      const choice = await vscode.window.showInformationMessage(
        `Graphoxide is enabled. Also configure MCP for ${formatNames(toolNames)}?`,
        'Open Control Center',
        'Later',
      );
      if (choice === 'Open Control Center') await vscode.commands.executeCommand('graphoxide.openControlCenter');
    } else {
      void vscode.window.showInformationMessage(`Graphoxide is managing ${target.name} with ${freshnessDescription(mode)}.`);
    }
  }

  async disable(folder?: vscode.WorkspaceFolder): Promise<void> {
    const target = folder ?? await this.store.preferredFolder(true);
    if (!target) return;
    this.cli.stopWatch();
    await this.context.workspaceState.update(this.enabledKey(target), false);
    await this.context.workspaceState.update(this.freshnessKey(target), 'manual');
    this.changeEmitter.fire();
    void vscode.window.showInformationMessage('Graphoxide workspace management is disabled. Existing graph and external MCP registrations were left intact.');
  }

  async configureFreshness(folder?: vscode.WorkspaceFolder, preferredFreshness?: FreshnessMode): Promise<void> {
    const target = folder ?? await this.store.preferredFolder(true);
    if (!target) return;
    if (!this.isEnabled(target)) {
      const choice = await vscode.window.showInformationMessage('Enable managed Graphoxide before configuring automatic updates?', 'Enable Graphoxide');
      if (choice === 'Enable Graphoxide') await this.enable(target);
      return;
    }
    const mode = preferredFreshness ?? await this.chooseFreshness(target);
    if (!mode) return;
    const watchEnvironment = mode === 'watch' ? this.store.managedOutput(target).environment : undefined;
    await this.context.workspaceState.update(this.freshnessKey(target), mode);
    await this.cli.stopWatchAndWait();
    if (watchEnvironment) await this.cli.startWatch(target, watchEnvironment);
    void vscode.window.showInformationMessage(`Graphoxide will use ${freshnessDescription(mode)} for ${target.name}.`);
  }

  async resetPrompt(folder?: vscode.WorkspaceFolder): Promise<void> {
    const target = folder ?? await this.store.preferredFolder(true);
    if (!target) return;
    await this.context.workspaceState.update(this.enabledKey(target), undefined);
    await this.context.workspaceState.update(this.freshnessKey(target), undefined);
    this.changeEmitter.fire();
    await this.prompt(target);
  }

  dispose(): void {
    for (const subscription of this.subscriptions) subscription.dispose();
    this.changeEmitter.dispose();
  }

  private async prompt(folder: vscode.WorkspaceFolder): Promise<void> {
    const choice = await vscode.window.showInformationMessage(
      `Enable Graphoxide for “${folder.name}”? It will build a local architecture graph, register MCP with VS Code, and offer automatic updates.`,
      'Enable Graphoxide',
      'Not now',
      'Don’t ask for this workspace',
    );
    if (choice === 'Enable Graphoxide') await this.enable(folder);
    if (choice === 'Don’t ask for this workspace') await this.context.workspaceState.update(this.enabledKey(folder), false);
  }

  private async resume(folder: vscode.WorkspaceFolder): Promise<void> {
    const mode = this.freshness(folder);
    const state = await this.store.load(folder);
    try {
      const environment = this.store.managedOutput(folder).environment;
      if (!state?.model) {
        await this.cli.run({ title: 'Graphoxide: rebuilding managed workspace…', folder, args: ['extract', folder.uri.fsPath], showProgress: false, cancellable: false, environment });
        await this.store.load(folder);
      } else if (mode !== 'manual') {
        await this.cli.run({ title: 'Graphoxide: refreshing managed workspace…', folder, args: ['update', folder.uri.fsPath, '--force'], showProgress: false, cancellable: false, environment });
        await this.store.load(folder);
      }
      if (mode === 'watch' && !this.cli.watching) await this.cli.startWatch(folder, environment);
    } catch (error) {
      this.cli.output.error(`Managed workspace startup failed: ${error instanceof Error ? error.message : String(error)}`);
    }
  }

  private async chooseFreshness(folder: vscode.WorkspaceFolder): Promise<FreshnessMode | undefined> {
    const selected = await vscode.window.showQuickPick([
      { label: '$(eye) Continuous watch', description: 'Recommended', detail: 'Incrementally refresh while this workspace is open.', value: 'watch' as const },
      { label: '$(save) Update on save', description: 'Lighter weight', detail: 'Debounce an update after source files are saved.', value: 'save' as const },
      { label: '$(debug-pause) Manual updates', description: 'No background maintenance', detail: 'Refresh only when you run a Graphoxide command.', value: 'manual' as const },
    ], {
      title: `How should Graphoxide keep ${folder.name} current?`,
      placeHolder: 'Continuous watch is recommended for a fully managed workspace.',
      ignoreFocusOut: true,
    });
    return selected?.value;
  }

  private async detectUnconfiguredTools(folder: vscode.WorkspaceFolder): Promise<string[]> {
    const invocation = await resolvedInvocation(folder, this.context);
    const reports = await integrationReports({ folder, invocation });
    return reports
      .filter(({ status }) => status.detected && !status.project?.configured)
      .map(({ installer }) => installer.displayName);
  }

  private enabledKey(folder: vscode.WorkspaceFolder): string {
    return `managed.enabled.${folder.uri.toString()}`;
  }

  private freshnessKey(folder: vscode.WorkspaceFolder): string {
    return `managed.freshness.${folder.uri.toString()}`;
  }
}

function formatNames(names: readonly string[]): string {
  if (names.length < 2) return names[0] ?? 'detected AI tools';
  if (names.length === 2) return `${names[0]} and ${names[1]}`;
  return `${names.slice(0, -1).join(', ')}, and ${names.at(-1)}`;
}

function freshnessDescription(mode: FreshnessMode): string {
  if (mode === 'watch') return 'continuous watch mode';
  if (mode === 'save') return 'update-on-save mode';
  return 'manual updates';
}
