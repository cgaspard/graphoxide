import { ChildProcessWithoutNullStreams, spawn } from 'node:child_process';
import * as vscode from 'vscode';
import { workspaceGraphMutationAllowed } from './build';
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

const WATCH_READINESS_TIMEOUT_MS = 10_000;
const WATCH_STOP_GRACE_MS = 2_000;

export class GraphoxideCli implements vscode.Disposable {
  readonly output = vscode.window.createOutputChannel('Graphoxide', { log: true });
  private readonly mutationCoordinator = new GraphMutationCoordinator();
  private readonly activeRunProcesses = new ProcessTracker<ChildProcessWithoutNullStreams>();
  private readonly reportedErrors = new WeakSet<object>();
  private watchProcess?: ChildProcessWithoutNullStreams;
  private watchGeneration?: number;
  private watchReady = false;
  private watchStart?: Promise<void>;
  private watchRelease?: SharedWatchRelease;
  private readonly watchLifecycleState = new WatchLifecycle();
  private readonly watchEmitter = new vscode.EventEmitter<boolean>();
  private nextMutationBarrier?: MutationStartBarrier;
  private disposed = false;
  readonly onDidChangeWatch = this.watchEmitter.event;

  constructor(private readonly extensionUri: vscode.Uri) {}

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
        return this.run({
          ...options,
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

  async run(options: RunOptions): Promise<RunResult> {
    const execute = async (token?: vscode.CancellationToken): Promise<RunResult> => {
      if (this.disposed) throw new vscode.CancellationError();
      const config = vscode.workspace.getConfiguration('graphoxide', options.folder.uri);
      const useTrustedExecutable = shouldUseTrustedExecutable(options.trustedExecutable, options.environment);
      const invocation = useTrustedExecutable
        ? trustedExtensionInvocation(this.extensionUri, options.folder)
        : extensionInvocation(this.extensionUri, options.folder);
      const executable = invocation.command;
      const prefix = useTrustedExecutable ? invocation.args : invocation.args.slice(0, -1);
      const args = [...prefix, ...options.args];
      this.logInfo(`$ ${executable} ${args.map(formatArgument).join(' ')}`);
      const result = await new Promise<RunResult>((resolve, reject) => {
        let child: ChildProcessWithoutNullStreams;
        try {
          child = spawn(executable, args, {
            cwd: options.folder.uri.fsPath,
            env: overlayEnvironment(process.env, options.environment),
            shell: false,
          });
        } catch (error) {
          reject(error);
          return;
        }
        const close = trackProcessUntilClose(this.activeRunProcesses, child);
        let settled = false;
        let stdout = '';
        const stderr = new BoundedTextTail(STDERR_CAPTURE_LIMIT);
        const cancellation = token?.onCancellationRequested(() => child.kill('SIGTERM'));
        const finish = (error?: Error, value?: RunResult): void => {
          if (settled) return;
          settled = true;
          cancellation?.dispose();
          if (error) reject(error);
          else if (value) resolve(value);
        };
        child.stdout.on('data', (chunk: Buffer) => {
          const text = chunk.toString();
          stdout += text;
          this.appendOutput(text);
        });
        child.stderr.on('data', (chunk: Buffer) => {
          stderr.append(chunk.toString());
        });
        void close.then(({ code, signal, error }) => {
          if (token?.isCancellationRequested || this.disposed) {
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
      if (result.stderr.trim()) this.appendOutput(result.stderr);
      const reveal = config.get<string>('revealOutput', 'onError');
      if (reveal === 'always' && !this.disposed) this.output.show(true);
      return result;
    };

    try {
      if (options.showProgress === false) return await execute();
      return await vscode.window.withProgress(
        { location: vscode.ProgressLocation.Notification, title: options.title, cancellable: options.cancellable ?? true },
        async (_progress, token) => execute(token),
      );
    } catch (error) {
      if (error instanceof vscode.CancellationError) throw error;
      const reported = this.reportFailure(error, options.failureGuidance);
      if (!this.disposed && vscode.workspace.getConfiguration('graphoxide', options.folder.uri).get<string>('revealOutput', 'onError') !== 'never') {
        this.output.show(true);
      }
      throw reported;
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
        () => this.startWatchProcess(folder, environment),
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

  private async startWatchProcess(folder: vscode.WorkspaceFolder, environment: EnvironmentOverlay): Promise<void> {
    if (this.disposed) throw new vscode.CancellationError();
    if (this.watchStart) return this.watchStart;
    if (this.watchProcess) {
      await this.stopWatchAndWait();
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
    const args = [...invocation.args.slice(0, -1), 'watch', folder.uri.fsPath];
    const outputDirectory = environment.GRAPHOXIDE_OUT;
    if (!outputDirectory) throw new Error('Graphoxide watch mode requires a managed output directory.');
    this.logInfo(`$ ${executable} ${args.map(formatArgument).join(' ')}`);
    const generation = this.watchLifecycleState.beginStart(outputDirectory);
    const watchStart = new Promise<void>((resolve, reject) => {
      let child: ChildProcessWithoutNullStreams;
      try {
        child = spawn(executable, args, {
          cwd: folder.uri.fsPath,
          env: overlayEnvironment(process.env, environment),
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
        const text = chunk.toString();
        if (reachedReady) this.appendOutput(text);
        else startupStderr.append(text);
      });
      child.on('error', (error) => {
        processError ??= error;
      });
      child.on('close', (code, signal) => {
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
    this.activeRunProcesses.terminateAll();
    this.stopWatch();
    this.watchEmitter.dispose();
    this.output.dispose();
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
