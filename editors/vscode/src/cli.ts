import { ChildProcessWithoutNullStreams, spawn } from 'node:child_process';
import * as vscode from 'vscode';
import { workspaceGraphMutationAllowed } from './build';
import { EnvironmentOverlay, overlayEnvironment, shouldUseTrustedExecutable } from './llm/config';
import { extensionInvocation, trustedExtensionInvocation } from './mcp/runtime';
import { WatchLifecycle, WatchLifecycleSnapshot, WatchLifecycleWaitOptions } from './watch-lifecycle';

export interface RunOptions {
  readonly title: string;
  readonly folder: vscode.WorkspaceFolder;
  readonly args: readonly string[];
  readonly cancellable?: boolean;
  readonly showProgress?: boolean;
  readonly environment?: EnvironmentOverlay;
  readonly trustedExecutable?: boolean;
}

export interface RunResult {
  readonly stdout: string;
  readonly stderr: string;
  readonly exitCode: number;
}

export class GraphoxideCli implements vscode.Disposable {
  readonly output = vscode.window.createOutputChannel('Graphoxide', { log: true });
  private watchProcess?: ChildProcessWithoutNullStreams;
  private watchGeneration?: number;
  private watchReady = false;
  private watchStart?: Promise<void>;
  private readonly watchLifecycleState = new WatchLifecycle();
  private readonly watchEmitter = new vscode.EventEmitter<boolean>();
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

  async run(options: RunOptions): Promise<RunResult> {
    const execute = async (token?: vscode.CancellationToken): Promise<RunResult> => {
      const config = vscode.workspace.getConfiguration('graphoxide', options.folder.uri);
      const useTrustedExecutable = shouldUseTrustedExecutable(options.trustedExecutable, options.environment);
      const invocation = useTrustedExecutable
        ? trustedExtensionInvocation(this.extensionUri, options.folder)
        : extensionInvocation(this.extensionUri, options.folder);
      const executable = invocation.command;
      const prefix = useTrustedExecutable ? invocation.args : invocation.args.slice(0, -1);
      const args = [...prefix, ...options.args];
      this.output.info(`$ ${executable} ${args.map(formatArgument).join(' ')}`);
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
        let stdout = '';
        let stderr = '';
        const cancellation = token?.onCancellationRequested(() => child.kill('SIGTERM'));
        child.stdout.on('data', (chunk: Buffer) => {
          const text = chunk.toString();
          stdout += text;
          this.output.append(text);
        });
        child.stderr.on('data', (chunk: Buffer) => {
          const text = chunk.toString();
          stderr += text;
          this.output.append(text);
        });
        child.on('error', (error) => {
          cancellation?.dispose();
          reject(error);
        });
        child.on('close', (code, signal) => {
          cancellation?.dispose();
          if (token?.isCancellationRequested) {
            reject(new vscode.CancellationError());
            return;
          }
          resolve({ stdout, stderr, exitCode: code ?? (signal ? 1 : 0) });
        });
      });
      if (result.exitCode !== 0) {
        throw new Error(result.stderr.trim() || result.stdout.trim() || `Graphoxide exited with code ${result.exitCode}`);
      }
      const reveal = config.get<string>('revealOutput', 'onError');
      if (reveal === 'always') this.output.show(true);
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
      this.output.error(error instanceof Error ? error.message : String(error));
      if (vscode.workspace.getConfiguration('graphoxide', options.folder.uri).get<string>('revealOutput', 'onError') !== 'never') {
        this.output.show(true);
      }
      throw error;
    }
  }

  async startWatch(folder: vscode.WorkspaceFolder, environment: EnvironmentOverlay): Promise<void> {
    if (!workspaceGraphMutationAllowed(vscode.workspace.isTrusted)) {
      void vscode.window.showWarningMessage('Trust this workspace before starting Graphoxide watch mode.');
      return;
    }
    if (this.watching) {
      void vscode.window.showInformationMessage('Graphoxide watch mode is already running.');
      return;
    }
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
    this.output.info(`$ ${executable} ${args.map(formatArgument).join(' ')}`);
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
      let startupOutput = '';
      let startupSettled = false;
      const settleStartup = (error?: Error): void => {
        if (startupSettled) return;
        startupSettled = true;
        clearTimeout(readinessTimeout);
        if (error) reject(error);
        else resolve();
      };
      const readinessTimeout = setTimeout(() => {
        const error = new Error('watch mode did not report readiness within 10 seconds');
        if (this.watchProcess === child) this.stopWatch();
        settleStartup(error);
      }, 10000);
      child.stdout.on('data', (chunk: Buffer) => {
        const text = chunk.toString();
        this.output.append(text);
        if (!this.watchReady) startupOutput += text;
        if (this.watchProcess === child && !this.watchReady && /(^|\n)Watching\s/u.test(startupOutput)) {
          this.watchReady = true;
          this.watchLifecycleState.markReady(generation);
          this.watchEmitter.fire(true);
          void vscode.commands.executeCommand('setContext', 'graphoxide.watching', true);
          void vscode.window.showInformationMessage('Graphoxide watch mode started.');
          settleStartup();
        }
      });
      child.stderr.on('data', (chunk: Buffer) => this.output.append(chunk.toString()));
      child.on('error', (error) => {
        this.output.error(`Watch mode failed: ${error.message}`);
        this.output.show(true);
        settleStartup(error);
      });
      child.on('close', (code) => {
        this.watchLifecycleState.markExited(generation);
        if (this.watchProcess === child) {
          this.watchProcess = undefined;
          this.watchGeneration = undefined;
          this.watchReady = false;
          this.watchEmitter.fire(false);
          void vscode.commands.executeCommand('setContext', 'graphoxide.watching', false);
          if (code && code !== 0) void vscode.window.showErrorMessage(`Graphoxide watch mode stopped with exit code ${code}.`);
        }
        if (!startupSettled) settleStartup(new Error(`watch mode exited before it was ready (code ${code ?? 'unknown'})`));
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
      await new Promise<void>((resolve, reject) => {
        let settled = false;
        const onClose = (): void => settle();
        const onError = (error: Error): void => settle(error);
        const timeout = setTimeout(() => settle(new Error('watch mode did not stop within 5 seconds')), 5000);
        const settle = (error?: Error): void => {
          if (settled) return;
          settled = true;
          clearTimeout(timeout);
          child.removeListener('close', onClose);
          child.removeListener('error', onError);
          if (error) reject(error);
          else resolve();
        };
        child.once('close', onClose);
        child.once('error', onError);
        if (!this.requestWatchStop(child) && child.exitCode === null && child.signalCode === null) {
          settle(new Error('watch mode could not be signalled to stop'));
        }
      });
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
    this.stopWatch();
    this.watchEmitter.dispose();
    this.output.dispose();
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
      this.watchEmitter.fire(false);
      void vscode.commands.executeCommand('setContext', 'graphoxide.watching', false);
    }
    const running = child.exitCode === null && child.signalCode === null;
    return !running || !firstRequest || child.kill('SIGTERM');
  }
}

function formatArgument(value: string): string {
  return /^[a-zA-Z0-9_./:=+-]+$/u.test(value) ? value : JSON.stringify(value);
}
