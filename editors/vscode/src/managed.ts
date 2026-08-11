import * as vscode from 'vscode';
import { automaticGraphUpdateArguments } from './build';
import { GraphoxideCli, MutationRunOutcome } from './cli';
import { integrationReports } from './mcp/installers';
import { resolvedInvocation } from './mcp/runtime';
import { busyCompletionSatisfiesStructuralTarget } from './mutation-coordinator';
import { GraphStore } from './store';

export type FreshnessMode = 'watch' | 'save' | 'manual';

export interface ManagedResumeWatchBarrierControl {
  waitUntilReached(): Promise<void>;
  waitUntilBusyJoined(): Promise<void>;
  release(): void;
}

export class ManagedWorkspaceService implements vscode.Disposable {
  private readonly changeEmitter = new vscode.EventEmitter<void>();
  private readonly subscriptions: vscode.Disposable[] = [];
  private lifecycleGeneration = 0;
  private startPromise?: Promise<void>;
  private nextResumeWatchBarrier?: ManagedResumeWatchBarrier;
  private activeResumeWatchBarrier?: ManagedResumeWatchBarrier;
  private disposed = false;
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

  start(): Promise<void> {
    if (this.disposed) return Promise.resolve();
    if (this.startPromise) return this.startPromise;
    const startPromise = this.startOnce().catch((error: unknown) => {
      if (error instanceof vscode.CancellationError || this.disposed || this.cli.errorWasReported(error)) return;
      this.cli.output.error(`Managed workspace startup failed: ${error instanceof Error ? error.message : String(error)}`);
    }).finally(() => {
      if (this.startPromise === startPromise) this.startPromise = undefined;
    });
    this.startPromise = startPromise;
    return startPromise;
  }

  holdNextResumeWatchStart(): ManagedResumeWatchBarrierControl {
    if (this.nextResumeWatchBarrier || this.activeResumeWatchBarrier) {
      throw new Error('A managed resume watch barrier is already armed.');
    }
    const barrier = new ManagedResumeWatchBarrier();
    this.nextResumeWatchBarrier = barrier;
    return {
      waitUntilReached: () => barrier.waitUntilReached(),
      waitUntilBusyJoined: () => barrier.waitUntilBusyJoined(),
      release: () => barrier.release(),
    };
  }

  private async startOnce(): Promise<void> {
    if (!vscode.workspace.isTrusted) return;
    const generation = this.lifecycleGeneration;
    const folder = await this.store.preferredFolder(false);
    if (!folder || !this.isCurrent(generation)) return;
    const state = this.context.workspaceState.get<boolean>(this.enabledKey(folder));
    if (state === true) {
      await this.resume(folder, generation);
    } else if (state === undefined && vscode.workspace.getConfiguration('graphoxide', folder.uri).get<boolean>('promptOnFirstOpen', true)) {
      if (!this.isCurrent(generation)) return;
      await this.prompt(folder, generation);
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
    const generation = this.invalidateLifecycle();
    const target = folder ?? await this.store.preferredFolder(true);
    if (!target || !this.isCurrent(generation)) return;
    let environment: Readonly<{ GRAPHOXIDE_OUT: string }>;
    try {
      environment = this.store.managedOutput(target).environment;
      const state = await this.store.load(target);
      if (!this.isCurrent(generation)) return;
      if (state?.model) {
        const outcome = await this.cli.runMutation({
          title: 'Graphoxide: synchronizing workspace graph…',
          folder: target,
          args: automaticGraphUpdateArguments(target.uri.fsPath),
          environment,
          mutationTarget: environment.GRAPHOXIDE_OUT,
          mutationOrigin: 'interactive',
          mutationLabel: 'synchronizing the managed workspace',
          suppressAutomaticOnFailure: true,
        });
        if (outcome.kind !== 'completed') return;
      } else {
        const outcome = await this.cli.runMutation({
          title: 'Graphoxide: building workspace graph…',
          folder: target,
          args: ['extract', target.uri.fsPath],
          environment,
          mutationTarget: environment.GRAPHOXIDE_OUT,
          mutationOrigin: 'interactive',
          mutationLabel: 'building the managed workspace',
          suppressAutomaticOnFailure: true,
        });
        if (outcome.kind !== 'completed') return;
      }
      if (!this.isCurrent(generation)) return;
      await this.store.load(target);
    } catch (error) {
      // A newer enable/disable/configuration action owns workspace state. A
      // failed continuation from this invocation must not erase that choice or
      // surface stale recovery UI after its lifecycle generation was revoked.
      if (error instanceof vscode.CancellationError || !this.isCurrent(generation)) return;
      const action = await vscode.window.showErrorMessage(
        `Graphoxide could not initialize this workspace: ${error instanceof Error ? error.message : String(error)}`,
        'Open settings',
      );
      if (!this.isCurrent(generation)) return;
      if (action === 'Open settings') await vscode.commands.executeCommand('graphoxide.openSettings');
      return;
    }

    if (!this.isCurrent(generation)) return;
    await this.context.workspaceState.update(this.enabledKey(target), true);
    if (!this.isCurrent(generation)) return;
    this.changeEmitter.fire();
    const mode = preferredFreshness ?? await this.chooseFreshness(target) ?? 'manual';
    if (!this.isCurrent(generation) || !this.isEnabled(target)) return;
    await this.context.workspaceState.update(this.freshnessKey(target), mode);
    if (!this.isCurrent(generation)) return;
    if (mode === 'watch') {
      await this.cli.startWatch(target, environment, 'interactive');
      if (!this.isCurrent(generation)) return;
      if (!this.watchesTarget(environment.GRAPHOXIDE_OUT)) {
        void vscode.window.showWarningMessage(`Graphoxide is enabled for ${target.name} with continuous watch selected, but the watcher did not start. Retry Start Watch after the current graph operation finishes.`);
        return;
      }
    }

    if (!offerExternalIntegrations) {
      void vscode.window.showInformationMessage(`Graphoxide is managing ${target.name} with ${freshnessDescription(mode)}.`);
      return;
    }
    const toolNames = await this.detectUnconfiguredTools(target);
    if (!this.isCurrent(generation)) return;
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
    const generation = this.invalidateLifecycle();
    const target = folder ?? await this.store.preferredFolder(true);
    if (!target || !this.isCurrent(generation)) return;
    await this.cli.stopWatchAndWait();
    if (!this.isCurrent(generation)) return;
    await this.context.workspaceState.update(this.enabledKey(target), false);
    if (!this.isCurrent(generation)) return;
    await this.context.workspaceState.update(this.freshnessKey(target), 'manual');
    if (!this.isCurrent(generation)) return;
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
    const generation = this.invalidateLifecycle();
    const watchEnvironment = mode === 'watch' ? this.store.managedOutput(target).environment : undefined;
    await this.context.workspaceState.update(this.freshnessKey(target), mode);
    if (!this.isCurrent(generation)) return;
    await this.cli.stopWatchAndWait();
    if (watchEnvironment && this.isCurrent(generation) && this.isEnabled(target) && this.freshness(target) === 'watch') {
      await this.cli.startWatch(target, watchEnvironment, 'interactive');
      if (!this.isCurrent(generation)) return;
      if (!this.watchesTarget(watchEnvironment.GRAPHOXIDE_OUT)) {
        void vscode.window.showWarningMessage(`Graphoxide is configured for continuous watch mode in ${target.name}, but the watcher did not start. Retry Start Watch after the current graph operation finishes.`);
        return;
      }
    }
    if (!this.isCurrent(generation)) return;
    void vscode.window.showInformationMessage(`Graphoxide will use ${freshnessDescription(mode)} for ${target.name}.`);
  }

  async resetPrompt(folder?: vscode.WorkspaceFolder): Promise<void> {
    const generation = this.invalidateLifecycle();
    const target = folder ?? await this.store.preferredFolder(true);
    if (!target || !this.isCurrent(generation)) return;
    await this.context.workspaceState.update(this.enabledKey(target), undefined);
    if (!this.isCurrent(generation)) return;
    await this.context.workspaceState.update(this.freshnessKey(target), undefined);
    if (!this.isCurrent(generation)) return;
    this.changeEmitter.fire();
    await this.prompt(target, generation);
  }

  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    this.lifecycleGeneration += 1;
    this.nextResumeWatchBarrier?.cancel();
    this.activeResumeWatchBarrier?.cancel();
    this.nextResumeWatchBarrier = undefined;
    this.activeResumeWatchBarrier = undefined;
    for (const subscription of this.subscriptions) subscription.dispose();
    this.changeEmitter.dispose();
  }

  private async prompt(folder: vscode.WorkspaceFolder, generation: number): Promise<void> {
    const choice = await vscode.window.showInformationMessage(
      `Enable Graphoxide for “${folder.name}”? It will build a local architecture graph, register MCP with VS Code, and offer automatic updates.`,
      'Enable Graphoxide',
      'Not now',
      'Don’t ask for this workspace',
    );
    if (!this.isCurrent(generation)) return;
    if (choice === 'Enable Graphoxide') await this.enable(folder);
    if (choice === 'Don’t ask for this workspace') await this.context.workspaceState.update(this.enabledKey(folder), false);
  }

  private async resume(folder: vscode.WorkspaceFolder, generation: number): Promise<void> {
    const mode = this.freshness(folder);
    let state = await this.store.load(folder);
    if (!this.resumeIsCurrent(folder, mode, generation)) return;
    try {
      const environment = this.store.managedOutput(folder).environment;
      if (mode === 'watch' && this.watchesTarget(environment.GRAPHOXIDE_OUT)) return;
      if (!state?.model) {
        const completed = await this.completeResumeMutation(folder, mode, generation, environment.GRAPHOXIDE_OUT, () => this.cli.runMutation({
          title: 'Graphoxide: rebuilding managed workspace…',
          folder,
          args: ['extract', folder.uri.fsPath],
          showProgress: false,
          cancellable: false,
          environment,
          mutationTarget: environment.GRAPHOXIDE_OUT,
          mutationOrigin: 'automatic',
          mutationLabel: 'rebuilding the managed workspace at startup',
          suppressAutomaticOnFailure: true,
        }));
        if (!completed) return;
        if (!this.resumeIsCurrent(folder, mode, generation)) return;
        state = await this.store.load(folder);
        if (!state?.model || !this.resumeIsCurrent(folder, mode, generation)) return;
      } else if (mode !== 'manual') {
        // `--force` currently authorizes legitimate graph shrink after source
        // deletion as well as bypassing extraction caches. Keep it until the
        // CLI exposes those policies independently; dropping it can retain
        // stale deleted facts.
        const completed = await this.completeResumeMutation(folder, mode, generation, environment.GRAPHOXIDE_OUT, () => this.cli.runMutation({
          title: 'Graphoxide: refreshing managed workspace…',
          folder,
          args: automaticGraphUpdateArguments(folder.uri.fsPath),
          showProgress: false,
          cancellable: false,
          environment,
          mutationTarget: environment.GRAPHOXIDE_OUT,
          mutationOrigin: 'automatic',
          mutationLabel: 'refreshing the managed workspace at startup',
          suppressAutomaticOnFailure: true,
        }));
        if (!completed) return;
        if (!this.resumeIsCurrent(folder, mode, generation)) return;
        state = await this.store.load(folder);
        if (!state?.model || !this.resumeIsCurrent(folder, mode, generation)) return;
      }
      if (this.resumeIsCurrent(folder, mode, generation)
        && mode === 'watch'
        && !this.cli.mutationLifecycle().automaticFailures.includes(environment.GRAPHOXIDE_OUT)
        && !this.watchesTarget(environment.GRAPHOXIDE_OUT)) {
        const watching = await this.startWatchAfterResume(folder, mode, generation, environment);
        if (!watching
          && this.resumeIsCurrent(folder, mode, generation)
          && !this.cli.mutationLifecycle().automaticFailures.includes(environment.GRAPHOXIDE_OUT)) {
          this.cli.output.warn('Managed workspace startup could not start continuous watch after one bounded retry. Retry Start Watch after the current graph operation finishes.');
        }
      }
    } catch (error) {
      if (error instanceof vscode.CancellationError || this.disposed) return;
      if (!this.cli.errorWasReported(error)) {
        this.cli.output.error(`Managed workspace startup failed: ${error instanceof Error ? error.message : String(error)}`);
      }
    }
  }

  private async completeResumeMutation(
    folder: vscode.WorkspaceFolder,
    mode: FreshnessMode,
    generation: number,
    target: string,
    request: () => Promise<MutationRunOutcome>,
  ): Promise<boolean> {
    const outcome = await request();
    if (outcome.kind === 'completed') return this.resumeIsCurrent(folder, mode, generation);
    if (outcome.kind !== 'busy') return false;

    const completion = await outcome.completion;
    if (!this.resumeIsCurrent(folder, mode, generation)) return false;
    if (this.cli.mutationLifecycle().automaticFailures.includes(target)) return false;
    if (busyCompletionSatisfiesStructuralTarget(outcome, completion, target)) return true;

    // A different target or report-only writer did not service this workspace.
    // Re-request once after ownership clears; never turn this into a queue loop.
    const retry = await request();
    return retry.kind === 'completed' && this.resumeIsCurrent(folder, mode, generation);
  }

  private async startWatchAfterResume(
    folder: vscode.WorkspaceFolder,
    mode: FreshnessMode,
    generation: number,
    environment: Readonly<{ GRAPHOXIDE_OUT: string }>,
  ): Promise<boolean> {
    const barrier = this.nextResumeWatchBarrier;
    this.nextResumeWatchBarrier = undefined;
    if (barrier) this.activeResumeWatchBarrier = barrier;
    try {
      await barrier?.pause();
      for (let attempt = 0; attempt < 2; attempt += 1) {
        if (!this.resumeIsCurrent(folder, mode, generation)
          || this.cli.mutationLifecycle().automaticFailures.includes(environment.GRAPHOXIDE_OUT)) return false;
        const outcome = await this.cli.startWatch(folder, environment, 'automatic');
        if (this.watchesTarget(environment.GRAPHOXIDE_OUT)) return true;
        if (outcome.kind !== 'busy') return false;
        barrier?.markBusyJoined();
        await outcome.completion;
      }
      return this.watchesTarget(environment.GRAPHOXIDE_OUT);
    } finally {
      if (this.activeResumeWatchBarrier === barrier) this.activeResumeWatchBarrier = undefined;
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

  private invalidateLifecycle(): number {
    this.lifecycleGeneration += 1;
    return this.lifecycleGeneration;
  }

  private isCurrent(generation: number): boolean {
    return !this.disposed && this.lifecycleGeneration === generation;
  }

  private resumeIsCurrent(folder: vscode.WorkspaceFolder, mode: FreshnessMode, generation: number): boolean {
    return this.isCurrent(generation) && this.isEnabled(folder) && this.freshness(folder) === mode;
  }

  private watchesTarget(outputDirectory: string): boolean {
    const lifecycle = this.cli.watchLifecycle(outputDirectory);
    return this.cli.watching && lifecycle.phase === 'ready' && lifecycle.targetMatchesExpected === true;
  }
}

class ManagedResumeWatchBarrier {
  private reached = false;
  private busyJoined = false;
  private released = false;
  private readonly reachedPromise: Promise<void>;
  private readonly busyJoinedPromise: Promise<void>;
  private readonly releasePromise: Promise<void>;
  private markReached!: () => void;
  private markBusy!: () => void;
  private markReleased!: () => void;

  constructor() {
    this.reachedPromise = new Promise<void>((resolve) => { this.markReached = resolve; });
    this.busyJoinedPromise = new Promise<void>((resolve) => { this.markBusy = resolve; });
    this.releasePromise = new Promise<void>((resolve) => { this.markReleased = resolve; });
  }

  waitUntilReached(): Promise<void> {
    return this.reachedPromise;
  }

  waitUntilBusyJoined(): Promise<void> {
    return this.busyJoinedPromise;
  }

  async pause(): Promise<void> {
    if (!this.reached) {
      this.reached = true;
      this.markReached();
    }
    await this.releasePromise;
  }

  markBusyJoined(): void {
    if (this.busyJoined) return;
    this.busyJoined = true;
    this.markBusy();
  }

  release(): void {
    if (this.released) return;
    this.released = true;
    this.markReleased();
  }

  cancel(): void {
    if (!this.reached) {
      this.reached = true;
      this.markReached();
    }
    this.markBusyJoined();
    this.release();
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
