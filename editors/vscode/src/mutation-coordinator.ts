export type GraphMutationOrigin = 'automatic' | 'interactive' | 'watch';
export type GraphMutationFailurePolicy = 'pause-automatic' | 'report-only';

export interface GraphMutationRequest {
  readonly target: string;
  readonly origin: GraphMutationOrigin;
  readonly label: string;
  readonly failurePolicy: GraphMutationFailurePolicy;
}

export interface GraphMutationCompletion {
  readonly generation: number;
  readonly status: 'completed' | 'failed';
}

export interface GraphMutationBusy {
  readonly kind: 'busy';
  readonly activeGeneration: number;
  readonly activeTarget: string;
  readonly activeOrigin: GraphMutationOrigin;
  readonly activeFailurePolicy: GraphMutationFailurePolicy;
  readonly activeLabel: string;
  readonly completion: Promise<GraphMutationCompletion>;
}

export type GraphMutationOutcome<T> =
  | { readonly kind: 'completed'; readonly generation: number; readonly value: T }
  | GraphMutationBusy
  | { readonly kind: 'suppressed'; readonly target: string }
  | { readonly kind: 'disposed' };

export interface GraphMutationSnapshot {
  readonly phase: 'idle' | 'running' | 'disposed';
  readonly generation: number;
  readonly activeGeneration?: number;
  readonly activeTarget?: string;
  readonly activeOrigin?: GraphMutationOrigin;
  readonly activeFailurePolicy?: GraphMutationFailurePolicy;
  readonly activeLabel?: string;
  readonly lastCompletedGeneration: number;
  readonly lastFailedGeneration: number;
  readonly automaticFailures: readonly string[];
}

interface ActiveMutation extends GraphMutationRequest {
  readonly generation: number;
  readonly completion: Promise<GraphMutationCompletion>;
}

/**
 * A bounded, process-agnostic gate for graph writers.
 *
 * The extension deliberately does not queue graph mutations. Interactive
 * callers can retry once the current operation finishes, while automatic
 * callers may coalesce one retry in their own lifecycle. A failed target is
 * held open for interactive recovery instead of being retried after every
 * save or activation event.
 */
export class GraphMutationCoordinator {
  private generation = 0;
  private active?: ActiveMutation;
  private lastCompletedGeneration = 0;
  private lastFailedGeneration = 0;
  private readonly automaticFailures = new Set<string>();
  private readonly idleWaiters = new Set<() => void>();
  private disposed = false;

  request<T>(
    request: GraphMutationRequest,
    execute: () => Promise<T>,
    shouldPauseAutomatic: (error: unknown) => boolean = () => true,
  ): Promise<GraphMutationOutcome<T>> {
    if (this.disposed) return Promise.resolve({ kind: 'disposed' });
    if (request.origin === 'automatic' && this.automaticFailures.has(request.target)) {
      return Promise.resolve({ kind: 'suppressed', target: request.target });
    }
    if (this.active) {
      return Promise.resolve({
        kind: 'busy',
        activeGeneration: this.active.generation,
        activeTarget: this.active.target,
        activeOrigin: this.active.origin,
        activeFailurePolicy: this.active.failurePolicy,
        activeLabel: this.active.label,
        completion: this.active.completion,
      });
    }

    const generation = this.generation + 1;
    this.generation = generation;
    let completionStatus: GraphMutationCompletion['status'] = 'failed';
    let resolveCompletion!: (completion: GraphMutationCompletion) => void;
    const completion = new Promise<GraphMutationCompletion>((resolve) => { resolveCompletion = resolve; });
    const execution = Promise.resolve().then(execute);
    this.active = { ...request, generation, completion };
    return execution.then(
      (value) => {
        completionStatus = 'completed';
        if (request.failurePolicy === 'pause-automatic') {
          this.automaticFailures.delete(request.target);
        }
        this.lastCompletedGeneration = generation;
        return { kind: 'completed' as const, generation, value };
      },
      (error: unknown) => {
        if (request.failurePolicy === 'pause-automatic' && shouldPauseAutomatic(error)) {
          this.automaticFailures.add(request.target);
        }
        this.lastFailedGeneration = generation;
        throw error;
      },
    ).finally(() => {
      if (this.active?.generation === generation) this.active = undefined;
      this.resolveIdleWaiters();
      // Busy callers use this as a join point. Resolve only after ownership is
      // released, and never forward the owner's failure to secondary callers.
      resolveCompletion({ generation, status: completionStatus });
    });
  }

  waitForIdle(): Promise<void> {
    if (!this.active || this.disposed) return Promise.resolve();
    return new Promise<void>((resolve) => this.idleWaiters.add(resolve));
  }

  snapshot(): GraphMutationSnapshot {
    return {
      phase: this.disposed ? 'disposed' : this.active ? 'running' : 'idle',
      generation: this.generation,
      ...(this.active ? {
        activeGeneration: this.active.generation,
        activeTarget: this.active.target,
        activeOrigin: this.active.origin,
        activeFailurePolicy: this.active.failurePolicy,
        activeLabel: this.active.label,
      } : {}),
      lastCompletedGeneration: this.lastCompletedGeneration,
      lastFailedGeneration: this.lastFailedGeneration,
      automaticFailures: [...this.automaticFailures].sort(),
    };
  }

  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    this.resolveIdleWaiters();
  }

  private resolveIdleWaiters(): void {
    if (this.active && !this.disposed) return;
    for (const resolve of this.idleWaiters) resolve();
    this.idleWaiters.clear();
  }
}

/** Proves that a joined mutation successfully serviced this structural target. */
export function busyCompletionSatisfiesStructuralTarget(
  busy: GraphMutationBusy,
  completion: GraphMutationCompletion,
  target: string,
): boolean {
  return busy.activeTarget === target
    && busy.activeFailurePolicy === 'pause-automatic'
    && completion.generation === busy.activeGeneration
    && completion.status === 'completed';
}
