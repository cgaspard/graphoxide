export type WatchLifecyclePhase = 'stopped' | 'starting' | 'ready' | 'stopping';

export interface WatchLifecycleSnapshot {
  readonly revision: number;
  readonly phase: WatchLifecyclePhase;
  /** Monotonically increasing identity of the most recently started process. */
  readonly generation: number;
  readonly activeGeneration?: number;
  /** Highest generation whose child-process `close` event has been observed. */
  readonly lastExitedGeneration: number;
  /** Present only when an expected target was supplied to `snapshot`. */
  readonly targetMatchesExpected?: boolean;
}

interface InternalWatchLifecycleState {
  readonly revision: number;
  readonly phase: WatchLifecyclePhase;
  readonly generation: number;
  readonly activeGeneration?: number;
  readonly lastExitedGeneration: number;
  readonly target?: string;
}

export interface WatchLifecycleWaitOptions {
  readonly description: string;
  readonly timeoutMs: number;
  readonly expectedTarget?: string;
  readonly diagnostics?: (snapshot: WatchLifecycleSnapshot) => string;
}

/**
 * Retained, event-driven process lifecycle state for the VS Code watch child.
 *
 * A boolean can say whether a process is ready now, but cannot prove that an
 * earlier child exited when a stop and replacement both happen between two
 * event-loop turns. Generations and `lastExitedGeneration` preserve that fact.
 */
export class WatchLifecycle {
  private state: InternalWatchLifecycleState = {
    revision: 0,
    phase: 'stopped',
    generation: 0,
    lastExitedGeneration: 0,
  };
  private readonly listeners = new Set<() => void>();

  snapshot(expectedTarget?: string): WatchLifecycleSnapshot {
    return {
      revision: this.state.revision,
      phase: this.state.phase,
      generation: this.state.generation,
      ...(this.state.activeGeneration === undefined ? {} : { activeGeneration: this.state.activeGeneration }),
      lastExitedGeneration: this.state.lastExitedGeneration,
      ...(expectedTarget === undefined
        ? {}
        : { targetMatchesExpected: this.state.target !== undefined && this.state.target === expectedTarget }),
    };
  }

  beginStart(target: string): number {
    if (this.state.phase !== 'stopped') {
      throw new Error(`Cannot start a watch process while lifecycle phase is ${this.state.phase}.`);
    }
    const generation = this.state.generation + 1;
    this.setState({
      revision: this.state.revision + 1,
      phase: 'starting',
      generation,
      activeGeneration: generation,
      lastExitedGeneration: this.state.lastExitedGeneration,
      target,
    });
    return generation;
  }

  markReady(generation: number): void {
    if (!this.isActive(generation) || this.state.phase !== 'starting') return;
    this.setState({ ...this.state, revision: this.state.revision + 1, phase: 'ready' });
  }

  markStopping(generation: number): void {
    if (!this.isActive(generation) || this.state.phase === 'stopping') return;
    this.setState({ ...this.state, revision: this.state.revision + 1, phase: 'stopping' });
  }

  markExited(generation: number): void {
    if (!this.isActive(generation)) return;
    this.setState({
      revision: this.state.revision + 1,
      phase: 'stopped',
      generation: this.state.generation,
      lastExitedGeneration: Math.max(this.state.lastExitedGeneration, generation),
    });
  }

  waitFor(
    predicate: (snapshot: WatchLifecycleSnapshot) => boolean,
    options: WatchLifecycleWaitOptions,
  ): Promise<WatchLifecycleSnapshot> {
    const current = this.snapshot(options.expectedTarget);
    if (predicate(current)) return Promise.resolve(current);

    return new Promise<WatchLifecycleSnapshot>((resolve, reject) => {
      let settled = false;
      const finish = (snapshot?: WatchLifecycleSnapshot, error?: Error): void => {
        if (settled) return;
        settled = true;
        clearTimeout(timeout);
        this.listeners.delete(onChange);
        if (error) reject(error);
        else resolve(snapshot ?? this.snapshot(options.expectedTarget));
      };
      const onChange = (): void => {
        const snapshot = this.snapshot(options.expectedTarget);
        try {
          if (predicate(snapshot)) finish(snapshot);
        } catch (error) {
          finish(undefined, error instanceof Error ? error : new Error(String(error)));
        }
      };
      const timeout = setTimeout(() => {
        const snapshot = this.snapshot(options.expectedTarget);
        const observed = options.diagnostics?.(snapshot) ?? describeWatchLifecycle(snapshot);
        finish(undefined, new Error(`Timed out after ${options.timeoutMs} ms waiting for ${options.description}; observed ${observed}.`));
      }, options.timeoutMs);
      this.listeners.add(onChange);
    });
  }

  private isActive(generation: number): boolean {
    return this.state.activeGeneration === generation;
  }

  private setState(state: InternalWatchLifecycleState): void {
    this.state = state;
    for (const listener of [...this.listeners]) listener();
  }
}

export function describeWatchLifecycle(snapshot: WatchLifecycleSnapshot): string {
  const active = snapshot.activeGeneration === undefined ? 'none' : String(snapshot.activeGeneration);
  const target = snapshot.targetMatchesExpected === undefined
    ? 'not-compared'
    : snapshot.targetMatchesExpected ? 'expected' : 'different';
  return `phase=${snapshot.phase}, generation=${snapshot.generation}, active=${active}, lastExited=${snapshot.lastExitedGeneration}, target=${target}`;
}
