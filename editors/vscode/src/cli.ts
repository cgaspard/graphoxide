import { ChildProcessWithoutNullStreams, spawn } from 'node:child_process';
import * as vscode from 'vscode';
import { ActivityOperation, ActivityProgressDecoder, ActivityProgressEvent, ActivityProgressRun, activityProgressMessage } from './activity-progress';
import { registryBindingArguments, workspaceGraphMutationAllowed } from './build';
import {
  BUILD_PROGRESS_NONCE_ENV,
  BuildCompletedEvent,
  BuildProgressDecoder,
  BuildProgressEvent,
  BuildProgressFrame,
  BuildProgressRun,
  createBuildProgressNonce,
  graphFileForOutputTarget,
  LatestBuildSummary,
  LatestBuildSummaryStore,
  ownsBuildProgressGeneration,
  phaseProgressMessage,
} from './build-progress';
import { EnvironmentOverlay, overlayEnvironment, shouldUseTrustedExecutable } from './llm/config';
import { extensionInvocation, trustedExtensionInvocation } from './mcp/runtime';
import {
  GraphMutationBusy,
  GraphMutationCoordinator,
  GraphMutationOrigin,
  GraphMutationOutcome,
  GraphMutationSnapshot,
} from './mutation-coordinator';
import {
  AUTOMATIC_UPDATES_PAUSED,
  BoundedTextTail,
  compactCommandDiagnostic,
  compactError,
  compactGuidedError,
  STDERR_CAPTURE_LIMIT,
} from './process-output';
import {
  classifyWatchProcessClose,
  FiniteProcessCancellation,
  ProcessTracker,
  quarantineUnclosedWatchProcess,
  SharedWatchRelease,
  trackProcessUntilClose,
  waitForProcessClose,
  WatchStartupDeadline,
} from './process-tracker';
import { WatchLifecycle, WatchLifecycleSnapshot, WatchLifecycleWaitOptions } from './watch-lifecycle';

export interface RunOptions {
  readonly title: string;
  readonly folder: vscode.WorkspaceFolder;
  readonly args: readonly string[];
  readonly cancellable?: boolean;
  readonly showProgress?: boolean;
  readonly environment?: EnvironmentOverlay;
  readonly trustedExecutable?: boolean;
  readonly failureGuidance?: string;
  /** Internal managed-output identity for build progress and summaries. */
  readonly progressTarget?: string;
  /** Internal non-notification cancellation source used by managed/test runs. */
  readonly cancellationToken?: vscode.CancellationToken;
  /** Keep the owned progress visible while the published graph is loaded into the UI. */
  readonly afterSuccess?: () => Promise<unknown>;
}

export interface RunResult {
  readonly stdout: string;
  readonly stderr: string;
  readonly exitCode: number;
}

export interface MutationRunOptions extends RunOptions {
  readonly mutationTarget: string;
  readonly mutationOrigin: Exclude<GraphMutationOrigin, 'watch'>;
  readonly mutationLabel: string;
  readonly suppressAutomaticOnFailure: boolean;
}

export type MutationRunOutcome = GraphMutationOutcome<RunResult>;
export type WatchStartOutcome = { readonly kind: 'watching' } | { readonly kind: 'unavailable' } | GraphMutationBusy;

export interface BuildProgressSnapshot {
  readonly generation: number;
  readonly operation: 'extract' | 'index' | 'update' | ActivityOperation | 'command';
  readonly message: string;
  readonly presentation: 'notification' | 'status';
}

const WATCH_READINESS_TIMEOUT_MS = 10_000;
const WATCH_STOP_GRACE_MS = 2_000;

export class GraphoxideCli implements vscode.Disposable {
  readonly output = vscode.window.createOutputChannel('Graphoxide', { log: true });
  private readonly mutationCoordinator = new GraphMutationCoordinator();
  private readonly activeRunProcesses = new ProcessTracker<ChildProcessWithoutNullStreams>();
  private readonly cooperativeRunProcesses = new Set<ChildProcessWithoutNullStreams>();
  private readonly reportedErrors = new WeakSet<object>();
  private watchProcess?: ChildProcessWithoutNullStreams;
  private watchGeneration?: number;
  private watchReady = false;
  private watchStart?: Promise<void>;
  private watchRelease?: SharedWatchRelease;
  private readonly watchLifecycleState = new WatchLifecycle();
  private readonly watchEmitter = new vscode.EventEmitter<boolean>();
  private readonly buildSummaryEmitter = new vscode.EventEmitter<void>();
  private readonly buildProgressEmitter = new vscode.EventEmitter<BuildProgressSnapshot | undefined>();
  private readonly buildSummaries?: LatestBuildSummaryStore;
  private watchBuildProgress?: WatchBuildProgress;
  private activeBuildProgress?: BuildProgressSnapshot;
  private readonly progressSnapshots = new Map<number, BuildProgressSnapshot>();
  private readonly progressCancellations = new Map<number, vscode.CancellationTokenSource>();
  private nextBuildProgressGeneration = 0;
  private nextMutationBarrier?: MutationStartBarrier;
  private disposed = false;
  readonly onDidChangeWatch = this.watchEmitter.event;
  readonly onDidChangeBuildSummary = this.buildSummaryEmitter.event;
  readonly onDidChangeBuildProgress = this.buildProgressEmitter.event;

  constructor(
    private readonly extensionUri: vscode.Uri,
    workspaceState?: vscode.Memento,
    private readonly onGraphPublished?: (outputTarget: string) => Promise<unknown>,
  ) {
    this.buildSummaries = workspaceState ? new LatestBuildSummaryStore(workspaceState) : undefined;
  }

  get watching(): boolean {
    return Boolean(this.watchProcess) && this.watchReady;
  }

  get watchActive(): boolean {
    // Keep the child reference through `close` so a replacement cannot overlap
    // it, while preserving the user's explicit stop as an inactive watcher.
    return (Boolean(this.watchProcess) && this.watchLifecycleState.snapshot().phase !== 'stopping')
      || Boolean(this.watchStart);
  }

  get watchMutationActive(): boolean {
    return Boolean(this.watchProcess) || Boolean(this.watchStart);
  }

  get buildProgress(): BuildProgressSnapshot | undefined {
    return this.activeBuildProgress;
  }

  watchLifecycle(expectedOutputDirectory?: string): WatchLifecycleSnapshot {
    return this.watchLifecycleState.snapshot(expectedOutputDirectory);
  }

  waitForWatchLifecycle(
    expectedOutputDirectory: string,
    predicate: (snapshot: WatchLifecycleSnapshot) => boolean,
    options: Omit<WatchLifecycleWaitOptions, 'expectedTarget'>,
  ): Promise<WatchLifecycleSnapshot> {
    return this.watchLifecycleState.waitFor(predicate, { ...options, expectedTarget: expectedOutputDirectory });
  }

  mutationLifecycle(): GraphMutationSnapshot {
    return this.mutationCoordinator.snapshot();
  }

  waitForMutationIdle(): Promise<void> {
    return this.mutationCoordinator.waitForIdle();
  }

  errorWasReported(error: unknown): boolean {
    return typeof error === 'object' && error !== null && this.reportedErrors.has(error);
  }

  /** Cancel the operation currently shown in the status bar and Control Center. */
  cancelActiveBuild(): void {
    if (this.disposed) return;
    const generation = this.activeBuildProgress?.generation;
    if (generation !== undefined) this.progressCancellations.get(generation)?.cancel();
    if (generation === this.watchBuildProgress?.generation) this.stopWatch();
  }

  holdNextMutationStart(): MutationStartBarrierControl {
    if (this.nextMutationBarrier) throw new Error('A mutation start barrier is already armed.');
    const barrier = new MutationStartBarrier();
    this.nextMutationBarrier = barrier;
    return {
      waitUntilReached: () => barrier.waitUntilReached(),
      release: (error?: Error) => barrier.release(error),
    };
  }

  async runMutation(options: MutationRunOptions): Promise<MutationRunOutcome> {
    // During watch startup the coordinator owns the finite readiness phase, so
    // callers join that bounded operation. Once startup ownership is released,
    // a live/stopping watch remains the writer until its child actually closes.
    if (this.watchMutationActive && this.mutationCoordinator.snapshot().phase !== 'running') {
      if (options.mutationOrigin === 'interactive') {
        void vscode.window.showInformationMessage('Graphoxide watch mode is already maintaining this graph. Stop watch mode before running another graph build.');
      }
      return {
        kind: 'busy',
        activeGeneration: this.watchGeneration ?? 0,
        activeTarget: options.mutationTarget,
        activeOrigin: 'watch',
        activeFailurePolicy: 'report-only',
        activeLabel: 'watch mode',
        completion: this.waitForWatchRelease(),
      };
    }
    const outcome = await this.mutationCoordinator.request(
      {
        target: options.mutationTarget,
        origin: options.mutationOrigin,
        label: options.mutationLabel,
        failurePolicy: options.suppressAutomaticOnFailure ? 'pause-automatic' : 'report-only',
      },
      async () => {
        const barrier = this.nextMutationBarrier;
        this.nextMutationBarrier = undefined;
        if (barrier) await barrier.pause();
        // Any admitted finite graph mutation can replace the artifact before an
        // older watch-pass identity read finishes. Supersede that pending
        // association without deleting the last already-persisted success.
        this.buildSummaries?.invalidatePending(options.mutationTarget);
        return this.run({
          ...options,
          progressTarget: options.mutationTarget,
          ...(options.suppressAutomaticOnFailure ? { failureGuidance: AUTOMATIC_UPDATES_PAUSED } : {}),
        });
      },
      (error) => options.suppressAutomaticOnFailure && !(error instanceof vscode.CancellationError),
    );
    if (outcome.kind === 'busy' && options.mutationOrigin === 'interactive') {
      void vscode.window.showInformationMessage(`Graphoxide is already ${outcome.activeLabel}. Try this command again when it finishes.`);
    }
    return outcome;
  }

  /** Own a visible operation while extension-side work is still in progress. */
  async runUiActivity<T>(
    message: string,
    execute: (token: vscode.CancellationToken) => Promise<T>,
    onCancel?: () => void,
  ): Promise<T> {
    const generation = ++this.nextBuildProgressGeneration;
    const cancellation = new vscode.CancellationTokenSource();
    this.progressCancellations.set(generation, cancellation);
    const subscription = cancellation.token.onCancellationRequested(() => {
      this.setBuildProgress(generation, 'command', 'Cancelling…', 'status');
      onCancel?.();
    });
    try {
      if (this.disposed) throw new vscode.CancellationError();
      this.setBuildProgress(generation, 'command', message, 'status');
      if (cancellation.token.isCancellationRequested) throw new vscode.CancellationError();
      const result = await execute(cancellation.token);
      if (this.disposed || cancellation.token.isCancellationRequested) throw new vscode.CancellationError();
      return result;
    } finally {
      subscription.dispose();
      cancellation.dispose();
      this.progressCancellations.delete(generation);
      this.finishBuildProgress(generation);
    }
  }

  async run(options: RunOptions): Promise<RunResult> {
    const requestedBuildOperation = buildOperationFromArgs(options.args);
    const buildOperation = options.progressTarget ? requestedBuildOperation : undefined;
    const activityOperation = activityOperationFromArgs(options.args);
    const buildProgressEnabled = buildOperation !== undefined;
    const protocolProgress = buildProgressEnabled || activityOperation !== undefined;
    const ownedProgress = protocolProgress || options.showProgress !== false;
    const operation = buildOperation ?? activityOperation ?? 'command';
    const cooperativeCancellation = wikiMutationFromArgs(options.args);
    const progressGeneration = ownedProgress ? ++this.nextBuildProgressGeneration : undefined;
    const cancellationSource = ownedProgress ? new vscode.CancellationTokenSource() : undefined;
    const externalCancellation = cancellationSource && options.cancellationToken?.onCancellationRequested(() => cancellationSource.cancel());
    if (options.cancellationToken?.isCancellationRequested) cancellationSource?.cancel();
    const execute = async (token?: vscode.CancellationToken): Promise<RunResult> => {
      if (this.disposed || token?.isCancellationRequested) throw new vscode.CancellationError();
      const config = vscode.workspace.getConfiguration('graphoxide', options.folder.uri);
      const useTrustedExecutable = shouldUseTrustedExecutable(options.trustedExecutable, options.environment);
      const invocation = useTrustedExecutable
        ? trustedExtensionInvocation(this.extensionUri, options.folder)
        : extensionInvocation(this.extensionUri, options.folder);
      const executable = invocation.command;
      const prefix = useTrustedExecutable ? invocation.args : invocation.args.slice(0, -1);
      const registryArguments = requestedBuildOperation
        ? registryBindingArguments(options.folder.uri.fsPath, config.get<unknown>('registryBinding'))
        : [];
      const args = [...prefix, ...options.args, ...registryArguments, ...(protocolProgress ? ['--progress=json'] : [])];
      this.logInfo(`$ ${executable} ${args.map(formatArgument).join(' ')}`);
      const progressNonce = protocolProgress ? createBuildProgressNonce() : undefined;
      const progressDecoder = progressNonce && buildProgressEnabled ? new BuildProgressDecoder(progressNonce) : undefined;
      const activityDecoder = progressNonce && activityOperation ? new ActivityProgressDecoder(progressNonce) : undefined;
      const activityRun = activityOperation ? new ActivityProgressRun(activityOperation) : undefined;
      const progressRun = buildOperation ? new BuildProgressRun(buildOperation) : undefined;
      const progressPresentation = 'status';
      const acceptProgress = (event: BuildProgressEvent): boolean => {
        if (!progressRun?.accept(event)) return false;
        if (token?.isCancellationRequested) return true;
        if (event.type === 'started' && progressGeneration !== undefined) {
          const message = buildStartMessage(event.mode);
          this.setBuildProgress(progressGeneration, event.operation, message, progressPresentation);
        } else if (event.type === 'phase' && progressGeneration !== undefined) {
          const message = phaseProgressMessage(event);
          this.setBuildProgress(progressGeneration, event.operation, message, progressPresentation);
        }
        return true;
      };
      const acceptActivity = (event: ActivityProgressEvent): boolean => {
        if (!activityRun?.accept(event)) return false;
        if (token?.isCancellationRequested) return true;
        if ((event.type === 'started' || event.type === 'phase') && progressGeneration !== undefined) {
          this.setBuildProgress(progressGeneration, event.operation, activityProgressMessage(event), 'status');
        }
        return true;
      };
      const result = await new Promise<RunResult>((resolve, reject) => {
        let child: ChildProcessWithoutNullStreams;
        try {
          child = spawn(executable, args, {
            cwd: options.folder.uri.fsPath,
            env: overlayEnvironment(process.env, progressNonce
              ? {
                  ...options.environment,
                  [BUILD_PROGRESS_NONCE_ENV]: progressNonce,
                  ...(cooperativeCancellation ? { GRAPHOXIDE_CANCEL_STDIN: '1' } : {}),
                }
              : options.environment),
            shell: false,
          });
        } catch (error) {
          reject(error);
          return;
        }
        const close = trackProcessUntilClose(this.activeRunProcesses, child);
        if (cooperativeCancellation) this.cooperativeRunProcesses.add(child);
        let settled = false;
        let stdout = '';
        const stderr = new BoundedTextTail(STDERR_CAPTURE_LIMIT);
        const processCancellation = cooperativeCancellation ? undefined : new FiniteProcessCancellation(child);
        // A closed child's stdin may reject a racing cooperative cancellation;
        // close remains authoritative and no signal may interrupt Wiki rollback.
        child.stdin.on('error', () => {});
        const cancellation = token?.onCancellationRequested(() => {
          if (progressGeneration !== undefined) this.setBuildProgress(progressGeneration, operation,
            cooperativeCancellation ? 'Cancelling Wiki and cleaning up…' : 'Cancelling…', 'status');
          if (cooperativeCancellation) {
            if (child.exitCode === null && child.signalCode === null && !child.stdin.destroyed) {
              child.stdin.end(`${progressNonce}\n`);
            }
          } else processCancellation?.cancel();
        });
        const finish = (error?: Error, value?: RunResult): void => {
          if (settled) return;
          settled = true;
          cancellation?.dispose();
          processCancellation?.dispose();
          if (error) reject(error);
          else if (value) resolve(value);
        };
        child.stdout.on('data', (chunk: Buffer) => {
          const text = chunk.toString();
          stdout += text;
          this.appendOutput(text);
        });
        child.stderr.on('data', (chunk: Buffer) => {
          if (progressDecoder) this.consumeProgressFrames(progressDecoder.push(chunk).frames, stderr, acceptProgress);
          else if (activityDecoder) {
            for (const frame of activityDecoder.push(chunk).frames) {
              if (!frame.event || !acceptActivity(frame.event)) stderr.append(frame.raw);
            }
          } else stderr.append(chunk.toString());
        });
        void close.then(({ code, signal, error }) => {
          this.cooperativeRunProcesses.delete(child);
          if (progressDecoder) {
            this.consumeProgressFrames(progressDecoder.finish().frames, stderr, acceptProgress);
          }
          if (activityDecoder) {
            for (const frame of activityDecoder.finish().frames) {
              if (!frame.event || !acceptActivity(frame.event)) stderr.append(frame.raw);
            }
          }
          if (token?.isCancellationRequested || this.disposed) {
            if (stderr.value().trim()) this.appendOutput(stderr.value());
            finish(new vscode.CancellationError());
            return;
          }
          if (error) {
            finish(error);
            return;
          }
          finish(undefined, { stdout, stderr: stderr.value(), exitCode: code ?? (signal ? 1 : 0) });
        });
      });
      if (result.exitCode !== 0) {
        throw new GraphoxideCommandError(
          compactCommandDiagnostic(result.stderr, result.stdout, result.exitCode),
          result.exitCode,
        );
      }
      const completedEvent = progressRun?.successfulCompletion(result.exitCode, false);
      if (completedEvent && options.progressTarget) {
        await this.persistBuildSummary(options.progressTarget, completedEvent);
      }
      if (token?.isCancellationRequested || this.disposed) throw new vscode.CancellationError();
      if (options.afterSuccess) {
        if (progressGeneration !== undefined) {
          this.setBuildProgress(progressGeneration, operation, operation === 'command' ? 'Finishing…' : 'Loading graph…', 'status');
        }
        await options.afterSuccess();
        if (token?.isCancellationRequested || this.disposed) throw new vscode.CancellationError();
      }
      if (result.stderr.trim()) this.appendOutput(result.stderr);
      const reveal = config.get<string>('revealOutput', 'onError');
      if (reveal === 'always' && !this.disposed) this.output.show(true);
      return result;
    };

    try {
      if (progressGeneration !== undefined && cancellationSource) {
        this.progressCancellations.set(progressGeneration, cancellationSource);
        this.setBuildProgress(progressGeneration, operation, options.title.replace(/^Graphoxide:\s*/u, ''), 'status');
        return await execute(cancellationSource.token);
      }
      return await execute(options.cancellationToken);
    } catch (error) {
      if (error instanceof vscode.CancellationError) throw error;
      const reported = this.reportFailure(error, options.failureGuidance);
      if (!this.disposed && vscode.workspace.getConfiguration('graphoxide', options.folder.uri).get<string>('revealOutput', 'onError') !== 'never') {
        this.output.show(true);
      }
      throw reported;
    } finally {
      externalCancellation?.dispose();
      cancellationSource?.dispose();
      if (progressGeneration !== undefined) {
        this.progressCancellations.delete(progressGeneration);
        this.finishBuildProgress(progressGeneration);
      }
    }
  }

  async latestBuildSummary(outputTarget: string, graphUri: vscode.Uri): Promise<LatestBuildSummary | undefined> {
    if (!this.buildSummaries) return undefined;
    try {
      return await this.buildSummaries.latestWithIdentity(outputTarget, async () => {
        const stat = await vscode.workspace.fs.stat(graphUri);
        return { mtime: stat.mtime, size: stat.size };
      });
    } catch {
      return undefined;
    }
  }

  async startWatch(
    folder: vscode.WorkspaceFolder,
    environment: EnvironmentOverlay,
    origin: Exclude<GraphMutationOrigin, 'watch'> = 'interactive',
  ): Promise<WatchStartOutcome> {
    if (!workspaceGraphMutationAllowed(vscode.workspace.isTrusted)) {
      void vscode.window.showWarningMessage('Trust this workspace before starting Graphoxide watch mode.');
      return { kind: 'unavailable' };
    }
    const outputDirectory = environment.GRAPHOXIDE_OUT;
    if (!outputDirectory) throw new Error('Graphoxide watch mode requires a managed output directory.');
    if (this.isWatchingTarget(outputDirectory)) {
      void vscode.window.showInformationMessage('Graphoxide watch mode is already running.');
      return { kind: 'watching' };
    }
    let outcome: GraphMutationOutcome<void>;
    try {
      outcome = await this.mutationCoordinator.request(
        { target: outputDirectory, origin: 'watch', label: 'starting watch mode', failurePolicy: 'report-only' },
        () => this.runUiActivity('Starting watch mode…', (token) => this.startWatchProcess(folder, environment, token), () => this.stopWatch()),
      );
    } catch (error) {
      if (error instanceof vscode.CancellationError) throw error;
      throw this.reportFailure(error);
    }
    if (outcome.kind === 'busy'
      && outcome.activeTarget === outputDirectory
      && outcome.activeOrigin === 'watch') {
      await outcome.completion;
      return this.isWatchingTarget(outputDirectory) ? { kind: 'watching' } : { kind: 'unavailable' };
    }
    if (outcome.kind === 'busy' && origin === 'interactive') {
      void vscode.window.showInformationMessage(`Graphoxide is already ${outcome.activeLabel}. Start watch mode again when it finishes.`);
    }
    if (outcome.kind === 'busy') return outcome;
    return this.isWatchingTarget(outputDirectory) ? { kind: 'watching' } : { kind: 'unavailable' };
  }

  private async startWatchProcess(folder: vscode.WorkspaceFolder, environment: EnvironmentOverlay, token: vscode.CancellationToken): Promise<void> {
    if (this.disposed || token.isCancellationRequested) throw new vscode.CancellationError();
    if (this.watchStart) return this.watchStart;
    if (this.watchProcess) {
      await this.stopWatchAndWait();
      if (this.disposed || token.isCancellationRequested) throw new vscode.CancellationError();
      // Multiple callers can wait for the same `close`. Recheck ownership after
      // that await so later continuations join the replacement started by the
      // first instead of calling `beginStart` from stale pre-await state.
      if (this.watching) {
        void vscode.window.showInformationMessage('Graphoxide watch mode is already running.');
        return;
      }
      if (this.watchStart) return this.watchStart;
    }
    const invocation = extensionInvocation(this.extensionUri, folder);
    const executable = invocation.command;
    const registryArguments = registryBindingArguments(
      folder.uri.fsPath,
      vscode.workspace.getConfiguration('graphoxide', folder.uri).get<unknown>('registryBinding'),
    );
    const args = [
      ...invocation.args.slice(0, -1),
      'watch',
      folder.uri.fsPath,
      ...registryArguments,
      '--progress=json',
    ];
    const outputDirectory = environment.GRAPHOXIDE_OUT;
    if (!outputDirectory) throw new Error('Graphoxide watch mode requires a managed output directory.');
    const progressNonce = createBuildProgressNonce();
    if (this.disposed || token.isCancellationRequested) throw new vscode.CancellationError();
    this.logInfo(`$ ${executable} ${args.map(formatArgument).join(' ')}`);
    const generation = this.watchLifecycleState.beginStart(outputDirectory);
    const watchStart = new Promise<void>((resolve, reject) => {
      let child: ChildProcessWithoutNullStreams;
      try {
        child = spawn(executable, args, {
          cwd: folder.uri.fsPath,
          env: overlayEnvironment(process.env, {
            ...environment,
            [BUILD_PROGRESS_NONCE_ENV]: progressNonce,
          }),
          shell: false,
        });
      } catch (error) {
        this.watchLifecycleState.markExited(generation);
        reject(error);
        return;
      }
      this.watchProcess = child;
      this.watchGeneration = generation;
      this.watchRelease = new SharedWatchRelease(generation);
      const startupOutput = new BoundedTextTail(STDERR_CAPTURE_LIMIT);
      const startupStderr = new BoundedTextTail(STDERR_CAPTURE_LIMIT);
      const progressDecoder = new BuildProgressDecoder(progressNonce);
      let startupSettled = false;
      let reachedReady = false;
      let startupFailure: Error | undefined;
      let processError: Error | undefined;
      const startupDeadline = new WatchStartupDeadline(
        WATCH_READINESS_TIMEOUT_MS,
        WATCH_STOP_GRACE_MS,
        {
          onReadinessTimeout: () => {
            const lifecycle = this.watchLifecycleState.snapshot();
            const owned = this.watchProcess === child && this.watchGeneration === generation;
            if (this.disposed || (owned && lifecycle.phase === 'stopping')) return;
            startupFailure = new Error(`watch mode did not report readiness within ${WATCH_READINESS_TIMEOUT_MS / 1000} seconds`);
            if (owned) this.requestWatchStop(child);
          },
          onStopGraceTimeout: () => {
            const lifecycle = this.watchLifecycleState.snapshot();
            const owned = this.watchProcess === child && this.watchGeneration === generation;
            const intentional = this.disposed
              || (owned && lifecycle.phase === 'stopping' && startupFailure === undefined);
            const quarantine = quarantineUnclosedWatchProcess(
              child,
              owned,
              WATCH_READINESS_TIMEOUT_MS,
              WATCH_STOP_GRACE_MS,
            );
            if (!quarantine) return;
            if (intentional) {
              settleStartup(new vscode.CancellationError());
              return;
            }
            startupFailure = quarantine;
            // Release the finite startup coordinator so callers receive one
            // actionable failure. Keep watchProcess until `close`; that child
            // remains the writer gate for every later graph mutation.
            settleStartup(quarantine);
          },
        },
      );
      const settleStartup = (error?: Error): void => {
        if (startupSettled) return;
        startupSettled = true;
        startupDeadline.dispose();
        if (error) reject(error);
        else resolve();
      };
      startupDeadline.start();
      child.stdout.on('data', (chunk: Buffer) => {
        const text = chunk.toString();
        this.appendOutput(text);
        if (!this.watchReady) startupOutput.append(text);
        if (this.watchProcess === child
          && !this.watchReady
          && this.watchLifecycleState.snapshot().phase === 'starting'
          && /(^|\n)Watching\s/u.test(startupOutput.value())) {
          this.watchReady = true;
          reachedReady = true;
          this.watchLifecycleState.markReady(generation);
          if (startupStderr.value().trim()) this.appendOutput(startupStderr.value());
          this.watchEmitter.fire(true);
          void vscode.commands.executeCommand('setContext', 'graphoxide.watching', true);
          void vscode.window.showInformationMessage('Graphoxide watch mode started.');
          settleStartup();
        }
      });
      child.stderr.on('data', (chunk: Buffer) => {
        const target = reachedReady ? undefined : startupStderr;
        for (const frame of progressDecoder.push(chunk).frames) {
          if (frame.event && this.acceptWatchBuildProgress(frame.event, outputDirectory, generation)) continue;
          if (target) target.append(frame.raw);
          else this.appendOutput(frame.raw);
        }
      });
      child.on('error', (error) => {
        processError ??= error;
      });
      child.on('close', (code, signal) => {
        for (const frame of progressDecoder.finish().frames) {
          if (frame.event && this.acceptWatchBuildProgress(frame.event, outputDirectory, generation)) continue;
          if (reachedReady) this.appendOutput(frame.raw);
          else startupStderr.append(frame.raw);
        }
        this.finishWatchBuildProgress(generation);
        const lifecycle = this.watchLifecycleState.snapshot();
        const owned = this.watchProcess === child;
        const intentional = this.disposed
          || (owned && lifecycle.phase === 'stopping' && startupFailure === undefined);
        if (!reachedReady && !intentional && !startupFailure && !processError && startupStderr.value().trim()) {
          processError = new Error(compactCommandDiagnostic(startupStderr.value(), '', code ?? (signal ? 1 : 0)));
        }
        const disposition = classifyWatchProcessClose({
          reachedReady,
          intentional,
          startupFailure,
          code,
          signal,
          ...(processError ? { error: processError } : {}),
        });
        this.watchLifecycleState.markExited(generation);
        if (owned) {
          this.watchProcess = undefined;
          this.watchGeneration = undefined;
          this.watchReady = false;
          if (!this.disposed) {
            this.watchEmitter.fire(false);
            void vscode.commands.executeCommand('setContext', 'graphoxide.watching', false);
          }
        }
        this.resolveWatchRelease(
          generation,
          disposition.kind === 'runtime-failure' || disposition.kind === 'startup-failure' ? 'failed' : 'completed',
        );
        if (!startupSettled) {
          settleStartup(disposition.kind === 'cancelled'
            ? new vscode.CancellationError()
            : disposition.kind === 'startup-failure'
              ? disposition.error
              : new Error('watch mode closed with an inconsistent startup lifecycle'));
        } else if (disposition.kind === 'runtime-failure' && !this.disposed) {
          void vscode.window.showErrorMessage(`Graphoxide watch mode stopped unexpectedly: ${compactError(disposition.error)}.`);
        }
      });
    });
    this.watchStart = watchStart;
    try {
      await watchStart;
    } finally {
      if (this.watchStart === watchStart) this.watchStart = undefined;
    }
  }

  stopWatch(): void {
    const child = this.watchProcess;
    if (!child) return;
    this.requestWatchStop(child);
  }

  async stopWatchAndWait(): Promise<boolean> {
    const watchStart = this.watchStart;
    const child = this.watchProcess;
    if (child) {
      const close = waitForProcessClose(child, 5000, 'watch mode did not stop within 5 seconds');
      this.requestWatchStop(child);
      await close;
    }
    if (watchStart) {
      try {
        await watchStart;
      } catch {
        // Stopping during startup rejects the readiness promise by design.
      }
    }
    if (!child) return false;
    return true;
  }

  openServerTerminal(folder: vscode.WorkspaceFolder): void {
    const invocation = extensionInvocation(this.extensionUri, folder);
    const executable = invocation.command;
    const args = [...invocation.args];
    const terminal = vscode.window.createTerminal({ name: 'Graphoxide MCP', shellPath: executable, shellArgs: args, cwd: folder.uri });
    terminal.show();
  }

  invocation(folder: vscode.WorkspaceFolder): { readonly command: string; readonly args: readonly string[]; readonly cwd: string } {
    const invocation = extensionInvocation(this.extensionUri, folder);
    return { command: invocation.command, args: invocation.args, cwd: folder.uri.fsPath };
  }

  trustedInvocation(folder: vscode.WorkspaceFolder): { readonly command: string; readonly args: readonly string[]; readonly cwd: string } {
    const invocation = trustedExtensionInvocation(this.extensionUri, folder);
    return { command: invocation.command, args: invocation.args, cwd: folder.uri.fsPath };
  }

  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    this.mutationCoordinator.dispose();
    this.nextMutationBarrier?.release();
    this.nextMutationBarrier = undefined;
    for (const cancellation of this.progressCancellations.values()) cancellation.cancel();
    this.activeRunProcesses.terminateAll((child) => !this.cooperativeRunProcesses.has(child));
    this.stopWatch();
    this.watchEmitter.dispose();
    this.finishWatchBuildProgress();
    this.buildSummaryEmitter.dispose();
    this.buildProgressEmitter.dispose();
    this.output.dispose();
  }

  private async persistBuildSummary(outputTarget: string, event: BuildCompletedEvent): Promise<void> {
    if (!this.buildSummaries) return;
    try {
      const graphUri = vscode.Uri.file(graphFileForOutputTarget(outputTarget));
      const recorded = await this.buildSummaries.recordWithIdentity(outputTarget, event, async () => {
        const stat = await vscode.workspace.fs.stat(graphUri);
        return { mtime: stat.mtime, size: stat.size };
      });
      if (recorded && !this.disposed) this.buildSummaryEmitter.fire();
    } catch {
      // A missing/replaced graph cannot be associated safely with this event.
    }
  }

  private acceptWatchBuildProgress(
    event: BuildProgressEvent,
    outputTarget: string,
    ownerGeneration: number,
  ): boolean {
    // Only the currently owned child may mutate progress or summary state. A
    // late authenticated frame from an exited generation must remain ordinary
    // stderr and cannot supersede the replacement child's session.
    if (!ownsBuildProgressGeneration(this.watchGeneration, ownerGeneration)) return false;
    if (this.watchLifecycleState.snapshot().phase === 'stopping') return true;
    if (event.type === 'started') {
      const run = new BuildProgressRun('update');
      if (!run.accept(event)) return false;
      // A new authenticated pass is authoritative even if an earlier terminal
      // was truncated. It also prevents older async stats from binding to the
      // graph that this pass is about to replace.
      this.buildSummaries?.invalidatePending(outputTarget);
      this.finishWatchBuildProgress(ownerGeneration);
      const generation = ++this.nextBuildProgressGeneration;
      const session: WatchBuildProgress = { generation, ownerGeneration, run };
      this.watchBuildProgress = session;
      this.setBuildProgress(generation, event.operation, buildStartMessage(event.mode), 'status');
      return true;
    }
    if (event.type === 'phase') {
      const session = this.watchBuildProgress;
      if (!session || !session.run.accept(event)) return false;
      if (this.watchLifecycleState.snapshot().phase === 'stopping') return true;
      this.setBuildProgress(session.generation, event.operation, phaseProgressMessage(event), 'status');
      return true;
    }
    if (event.type === 'completed') {
      const session = this.watchBuildProgress;
      if (!session) return false;
      if (this.watchLifecycleState.snapshot().phase === 'stopping') return true;
      if (!session.run.accept(event)) return false;
      this.setBuildProgress(session.generation, event.operation, 'Loading graph…', 'status');
      void this.finishWatchPublication(session, outputTarget, event);
      return true;
    }
    if (event.type === 'failed' || event.type === 'not_completed') {
      const session = this.watchBuildProgress;
      if (!session) return false;
      if (this.watchLifecycleState.snapshot().phase === 'stopping') return true;
      if (!session.run.accept(event)) return false;
      this.finishWatchBuildProgress(ownerGeneration);
      return true;
    }
    return false;
  }

  private async finishWatchPublication(session: WatchBuildProgress, outputTarget: string, event: BuildCompletedEvent): Promise<void> {
    try {
      await this.persistBuildSummary(outputTarget, event);
      if (this.disposed || this.watchBuildProgress !== session || this.watchLifecycleState.snapshot().phase === 'stopping') return;
      await this.onGraphPublished?.(outputTarget);
    } catch (error) {
      if (!this.disposed) this.logInfo(`Could not refresh the published graph: ${compactError(error)}`);
    } finally {
      if (this.watchBuildProgress === session && this.watchLifecycleState.snapshot().phase !== 'stopping') {
        this.finishWatchBuildProgress(session.ownerGeneration);
      }
    }
  }

  private finishWatchBuildProgress(ownerGeneration?: number): void {
    const session = this.watchBuildProgress;
    if (ownerGeneration !== undefined
      && !ownsBuildProgressGeneration(session?.ownerGeneration, ownerGeneration)) return;
    this.watchBuildProgress = undefined;
    if (session) this.finishBuildProgress(session.generation);
  }

  private consumeProgressFrames(
    frames: readonly BuildProgressFrame[],
    stderr: BoundedTextTail,
    accept: (event: BuildProgressEvent) => boolean,
  ): void {
    for (const frame of frames) {
      if (!frame.event || !accept(frame.event)) stderr.append(frame.raw);
    }
  }

  private setBuildProgress(
    generation: number,
    operation: BuildProgressSnapshot['operation'],
    message: string,
    presentation: 'notification' | 'status',
  ): void {
    if (this.disposed) return;
    const snapshot = { generation, operation, message, presentation } as const;
    this.progressSnapshots.set(generation, snapshot);
    if (this.activeBuildProgress && this.activeBuildProgress.generation > generation) return;
    this.activeBuildProgress = snapshot;
    this.buildProgressEmitter.fire(snapshot);
  }

  private finishBuildProgress(generation: number): void {
    this.progressSnapshots.delete(generation);
    if (!ownsBuildProgressGeneration(this.activeBuildProgress?.generation, generation)) return;
    this.activeBuildProgress = [...this.progressSnapshots.values()].sort((a, b) => b.generation - a.generation)[0];
    if (!this.disposed) this.buildProgressEmitter.fire(this.activeBuildProgress);
  }

  private appendOutput(value: string): void {
    if (!this.disposed) this.output.append(value);
  }

  private logInfo(value: string): void {
    if (!this.disposed) this.output.info(value);
  }

  private reportFailure(error: unknown, guidance?: string): Error {
    const message = compactGuidedError(error, guidance);
    const reported = error instanceof GraphoxideCommandError
      ? new GraphoxideCommandError(message, error.exitCode, { cause: error })
      : new Error(message, { cause: error });
    this.reportedErrors.add(reported);
    if (!this.disposed) this.output.error(message);
    return reported;
  }

  private waitForWatchRelease(): Promise<{ readonly generation: number; readonly status: 'completed' | 'failed' }> {
    const generation = this.watchGeneration;
    const release = this.watchRelease;
    if (generation === undefined || !this.watchProcess) {
      return Promise.resolve({ generation: generation ?? 0, status: 'completed' });
    }
    if (release?.generation !== generation) {
      return Promise.resolve({ generation, status: 'failed' });
    }
    return release.completion;
  }

  private resolveWatchRelease(generation: number, status: 'completed' | 'failed'): void {
    const release = this.watchRelease;
    if (release?.generation !== generation) return;
    this.watchRelease = undefined;
    release.settle(status);
  }

  private requestWatchStop(child: ChildProcessWithoutNullStreams): boolean {
    const generation = this.watchProcess === child ? this.watchGeneration : undefined;
    const lifecycle = this.watchLifecycleState.snapshot();
    const firstRequest = generation !== undefined
      && lifecycle.activeGeneration === generation
      && lifecycle.phase !== 'stopping';
    if (firstRequest) this.watchLifecycleState.markStopping(generation);
    const session = this.watchBuildProgress;
    if (session && session.ownerGeneration === generation) this.setBuildProgress(session.generation, 'update', 'Stopping watch mode…', 'status');
    const wasReady = this.watchProcess === child && this.watchReady;
    if (wasReady) {
      this.watchReady = false;
      if (!this.disposed) {
        this.watchEmitter.fire(false);
        void vscode.commands.executeCommand('setContext', 'graphoxide.watching', false);
      }
    }
    const running = child.exitCode === null && child.signalCode === null;
    return !running || !firstRequest || child.kill('SIGTERM');
  }

  private isWatchingTarget(outputDirectory: string): boolean {
    const lifecycle = this.watchLifecycleState.snapshot(outputDirectory);
    return this.watching && lifecycle.phase === 'ready' && lifecycle.targetMatchesExpected === true;
  }
}

function formatArgument(value: string): string {
  return /^[a-zA-Z0-9_./:=+-]+$/u.test(value) ? value : JSON.stringify(value);
}

function activityOperationFromArgs(args: readonly string[]): ActivityOperation | undefined {
  return args[0] === 'label' ? 'label' : args[0] === 'wiki' && args[1] !== 'live' ? 'wiki' : undefined;
}

function wikiMutationFromArgs(args: readonly string[]): boolean {
  return args[0] === 'wiki' && (args[1] === 'init'
    || (args[1] === 'source' && ['add', 'refresh', 'review', 'confirm', 'retire'].includes(args[2] ?? '')));
}

function buildOperationFromArgs(args: readonly string[]): 'extract' | 'index' | 'update' | undefined {
  const operation = args[0];
  return operation === 'extract' || operation === 'index' || operation === 'update'
    ? operation
    : undefined;
}

function buildStartMessage(mode: 'full' | 'incremental' | 'adaptive'): string {
  if (mode === 'full') return 'Starting full graph build…';
  if (mode === 'incremental') return 'Starting incremental graph update…';
  return 'Starting graph update…';
}

interface WatchBuildProgress {
  readonly generation: number;
  readonly ownerGeneration: number;
  readonly run: BuildProgressRun;
}

export class GraphoxideCommandError extends Error {
  override readonly name = 'GraphoxideCommandError';

  constructor(message: string, readonly exitCode: number, options?: ErrorOptions) {
    super(message, options);
  }
}

export interface MutationStartBarrierControl {
  waitUntilReached(): Promise<void>;
  release(error?: Error): void;
}

class MutationStartBarrier {
  private reached = false;
  private released = false;
  private releaseError?: Error;
  private readonly reachedPromise: Promise<void>;
  private readonly releasePromise: Promise<void>;
  private markReached!: () => void;
  private markReleased!: () => void;

  constructor() {
    this.reachedPromise = new Promise<void>((resolve) => { this.markReached = resolve; });
    this.releasePromise = new Promise<void>((resolve) => { this.markReleased = resolve; });
  }

  async pause(): Promise<void> {
    if (!this.reached) {
      this.reached = true;
      this.markReached();
    }
    await this.releasePromise;
    if (this.releaseError) throw this.releaseError;
  }

  waitUntilReached(): Promise<void> {
    return this.reachedPromise;
  }

  release(error?: Error): void {
    if (this.released) return;
    this.released = true;
    this.releaseError = error;
    this.markReleased();
  }
}
