export interface KillableProcess {
  readonly exitCode: number | null;
  readonly signalCode: string | null;
  kill(signal?: NodeJS.Signals | number): boolean;
}

export interface CloseObservableProcess extends KillableProcess {
  once(event: 'error', listener: (error: Error) => void): this;
  once(event: 'close', listener: (code: number | null, signal: NodeJS.Signals | null) => void): this;
  removeListener(event: 'close', listener: (code: number | null, signal: NodeJS.Signals | null) => void): this;
}

export interface TrackedProcessClose {
  readonly code: number | null;
  readonly signal: NodeJS.Signals | null;
  readonly error?: Error;
}

export interface WatchProcessCloseContext extends TrackedProcessClose {
  readonly reachedReady: boolean;
  readonly intentional: boolean;
  readonly startupFailure?: Error;
}

export interface WatchStartupDeadlineScheduler {
  set(callback: () => void, delayMs: number): unknown;
  clear(handle: unknown): void;
}

export interface WatchStartupDeadlineCallbacks {
  readonly onReadinessTimeout: () => void;
  readonly onStopGraceTimeout: () => void;
}

export type WatchProcessCloseDisposition =
  | { readonly kind: 'cancelled' }
  | { readonly kind: 'startup-failure'; readonly error: Error }
  | { readonly kind: 'stopped' }
  | { readonly kind: 'runtime-failure'; readonly error: Error };

export interface WatchReleaseCompletion {
  readonly generation: number;
  readonly status: 'completed' | 'failed';
}

/**
 * One non-rejecting release signal shared by every caller blocked behind the
 * same watch generation. Keeping a single deferred prevents a long-running or
 * quarantined watcher from accumulating one retained resolver per command.
 */
export class SharedWatchRelease {
  readonly completion: Promise<WatchReleaseCompletion>;
  private resolveCompletion!: (completion: WatchReleaseCompletion) => void;
  private settled = false;

  constructor(readonly generation: number) {
    this.completion = new Promise<WatchReleaseCompletion>((resolve) => {
      this.resolveCompletion = resolve;
    });
  }

  settle(status: WatchReleaseCompletion['status']): boolean {
    if (this.settled) return false;
    this.settled = true;
    this.resolveCompletion({ generation: this.generation, status });
    return true;
  }
}

const nodeDeadlineScheduler: WatchStartupDeadlineScheduler = {
  set: (callback, delayMs) => setTimeout(callback, delayMs),
  clear: (handle) => clearTimeout(handle as NodeJS.Timeout),
};

/**
 * Runs a two-stage watch startup deadline: first request a graceful stop, then
 * quarantine/escalate if close is still unconfirmed after the stop grace.
 */
export class WatchStartupDeadline {
  private readinessHandle?: unknown;
  private stopGraceHandle?: unknown;
  private disposed = false;

  constructor(
    private readonly readinessTimeoutMs: number,
    private readonly stopGraceMs: number,
    private readonly callbacks: WatchStartupDeadlineCallbacks,
    private readonly scheduler: WatchStartupDeadlineScheduler = nodeDeadlineScheduler,
  ) {
    if (!Number.isSafeInteger(readinessTimeoutMs) || readinessTimeoutMs < 1) {
      throw new Error('Watch readiness timeout must be a positive integer.');
    }
    if (!Number.isSafeInteger(stopGraceMs) || stopGraceMs < 1) {
      throw new Error('Watch stop grace must be a positive integer.');
    }
  }

  start(): void {
    if (this.disposed || this.readinessHandle !== undefined || this.stopGraceHandle !== undefined) return;
    this.readinessHandle = this.scheduler.set(() => {
      this.readinessHandle = undefined;
      if (this.disposed) return;
      this.callbacks.onReadinessTimeout();
      // A synchronous close can dispose the deadline from the callback.
      if (this.disposed) return;
      this.stopGraceHandle = this.scheduler.set(() => {
        this.stopGraceHandle = undefined;
        if (!this.disposed) this.callbacks.onStopGraceTimeout();
      }, this.stopGraceMs);
    }, this.readinessTimeoutMs);
  }

  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    if (this.readinessHandle !== undefined) this.scheduler.clear(this.readinessHandle);
    if (this.stopGraceHandle !== undefined) this.scheduler.clear(this.stopGraceHandle);
    this.readinessHandle = undefined;
    this.stopGraceHandle = undefined;
  }
}

/**
 * Escalates only the exact extension-owned, still-running watch child and
 * returns the single diagnostic used while that child remains quarantined.
 */
export function quarantineUnclosedWatchProcess(
  child: KillableProcess,
  owned: boolean,
  readinessTimeoutMs: number,
  stopGraceMs: number,
): Error | undefined {
  if (!owned) return undefined;
  let escalation = 'The watch process was already exiting';
  if (child.exitCode === null && child.signalCode === null) {
    try {
      escalation = child.kill('SIGKILL')
        ? 'The extension sent SIGKILL to its watch process'
        : 'The extension could not signal its watch process again';
    } catch {
      escalation = 'The extension could not signal its watch process again';
    }
  }
  return new Error(
    `Watch startup did not report readiness within ${formatDuration(readinessTimeoutMs)} and did not close within ${formatDuration(stopGraceMs)} after SIGTERM. ${escalation}; Graphoxide has quarantined it as the active graph writer until exit is confirmed. If it remains stuck, reload the VS Code window.`,
  );
}

/** Tracks finite-lived children through close and signals active ones on disposal. */
export class ProcessTracker<T extends KillableProcess> {
  private readonly active = new Set<T>();

  track(child: T): void {
    this.active.add(child);
  }

  release(child: T): void {
    this.active.delete(child);
  }

  terminateAll(): number {
    let signalled = 0;
    for (const child of this.active) {
      if (child.exitCode !== null || child.signalCode !== null) continue;
      if (child.kill('SIGTERM')) signalled += 1;
    }
    return signalled;
  }

  get size(): number {
    return this.active.size;
  }
}

/**
 * Own a child until Node confirms its `close` event. Node guarantees `close`
 * after either `exit` or a spawn `error`, so an early `error` is retained as
 * the command result without making the process disappear from disposal.
 */
export function trackProcessUntilClose<T extends CloseObservableProcess>(
  tracker: ProcessTracker<T>,
  child: T,
): Promise<TrackedProcessClose> {
  tracker.track(child);
  return new Promise<TrackedProcessClose>((resolve) => {
    let processError: Error | undefined;
    child.once('error', (error) => { processError ??= error; });
    child.once('close', (code, signal) => {
      tracker.release(child);
      resolve({ code, signal, ...(processError ? { error: processError } : {}) });
    });
  });
}

/** Waits for confirmed process close; an earlier `error` event is not an exit. */
export function waitForProcessClose<T extends CloseObservableProcess>(
  child: T,
  timeoutMs: number,
  timeoutMessage: string,
): Promise<void> {
  return new Promise<void>((resolve, reject) => {
    let settled = false;
    const onClose = (): void => settle();
    const timer = setTimeout(() => settle(new Error(timeoutMessage)), timeoutMs);
    const settle = (error?: Error): void => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      child.removeListener('close', onClose);
      if (error) reject(error);
      else resolve();
    };
    child.once('close', onClose);
  });
}

/** Pure close classification keeps startup cancellation and failure reporting distinct. */
export function classifyWatchProcessClose(context: WatchProcessCloseContext): WatchProcessCloseDisposition {
  if (!context.reachedReady) {
    if (context.intentional) return { kind: 'cancelled' };
    return {
      kind: 'startup-failure',
      error: context.startupFailure
        ?? context.error
        ?? new Error(`watch mode exited before it was ready (${processExitDescription(context)})`),
    };
  }
  if (context.intentional) return { kind: 'stopped' };
  return {
    kind: 'runtime-failure',
    error: context.error
      ?? new Error(`watch mode exited after readiness (${processExitDescription(context)})`),
  };
}

function processExitDescription(close: TrackedProcessClose): string {
  return close.signal ? `signal ${close.signal}` : `code ${close.code ?? 'unknown'}`;
}

function formatDuration(milliseconds: number): string {
  return milliseconds % 1000 === 0 ? `${milliseconds / 1000} seconds` : `${milliseconds} ms`;
}
