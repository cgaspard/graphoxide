import assert from 'node:assert/strict';
import test from 'node:test';
import {
  busyCompletionSatisfiesStructuralTarget,
  GraphMutationBusy,
  GraphMutationCoordinator,
} from '../src/mutation-coordinator';

class Barrier {
  readonly reached: Promise<void>;
  private readonly markReached: () => void;
  private readonly releasePromise: Promise<void>;
  private readonly markReleased: () => void;

  constructor() {
    let reached!: () => void;
    let released!: () => void;
    this.reached = new Promise<void>((resolve) => { reached = resolve; });
    this.releasePromise = new Promise<void>((resolve) => { released = resolve; });
    this.markReached = reached;
    this.markReleased = released;
  }

  async hold(): Promise<void> {
    this.markReached();
    await this.releasePromise;
  }

  release(): void {
    this.markReleased();
  }
}

test('admits one mutation without queuing a duplicate child', async () => {
  const coordinator = new GraphMutationCoordinator();
  const barrier = new Barrier();
  let executions = 0;
  const first = coordinator.request(
    { target: '/workspace/graphoxide-out', origin: 'automatic', label: 'managed startup', failurePolicy: 'pause-automatic' },
    async () => {
      executions += 1;
      await barrier.hold();
      return 'built';
    },
  );
  await barrier.reached;

  const duplicate = await coordinator.request(
    { target: '/workspace/graphoxide-out', origin: 'interactive', label: 'full rebuild', failurePolicy: 'pause-automatic' },
    async () => {
      executions += 1;
      return 'duplicate';
    },
  );

  assert.equal(duplicate.kind, 'busy');
  assert.equal(executions, 1);
  assert.equal(coordinator.snapshot().phase, 'running');
  barrier.release();
  const completed = await first;
  assert.equal(completed.kind, 'completed');
  assert.equal(executions, 1);
  assert.equal(coordinator.snapshot().phase, 'idle');
});

test('concurrent failed watch callers join after ownership clears and report once', async () => {
  const coordinator = new GraphMutationCoordinator();
  const barrier = new Barrier();
  const failure = new Error('watch failed before readiness');
  let executions = 0;
  let reports = 0;
  const startWatch = async (execute: () => Promise<void>): Promise<void> => {
    try {
      const outcome = await coordinator.request(
        { target: '/workspace/graphoxide-out', origin: 'automatic', label: 'starting watch mode', failurePolicy: 'report-only' },
        execute,
      );
      if (outcome.kind === 'busy') await outcome.completion;
    } catch {
      reports += 1;
    }
  };
  const owner = startWatch(async () => {
    executions += 1;
    await barrier.hold();
    throw failure;
  });
  await barrier.reached;
  const duplicate = startWatch(async () => { executions += 1; });

  barrier.release();
  await Promise.all([owner, duplicate]);
  assert.equal(executions, 1);
  assert.equal(reports, 1);
  assert.equal(coordinator.snapshot().phase, 'idle');
  assert.deepEqual(coordinator.snapshot().automaticFailures, []);
});

test('a busy join satisfies resume only after successful same-target structural work', () => {
  const target = '/workspace/graphoxide-out';
  const structural: GraphMutationBusy = {
    kind: 'busy',
    activeGeneration: 7,
    activeTarget: target,
    activeOrigin: 'interactive',
    activeFailurePolicy: 'pause-automatic',
    activeLabel: 'manual update',
    completion: Promise.resolve({ generation: 7, status: 'completed' }),
  };
  assert.equal(busyCompletionSatisfiesStructuralTarget(
    structural,
    { generation: 7, status: 'completed' },
    target,
  ), true);
  assert.equal(busyCompletionSatisfiesStructuralTarget(
    { ...structural, activeTarget: '/other/out' },
    { generation: 7, status: 'completed' },
    target,
  ), false);
  assert.equal(busyCompletionSatisfiesStructuralTarget(
    { ...structural, activeFailurePolicy: 'report-only' },
    { generation: 7, status: 'completed' },
    target,
  ), false);
  assert.equal(busyCompletionSatisfiesStructuralTarget(
    structural,
    { generation: 7, status: 'failed' },
    target,
  ), false);
});

test('suppresses automatic retries after failure but permits interactive recovery', async () => {
  const coordinator = new GraphMutationCoordinator();
  let executions = 0;
  await assert.rejects(
    coordinator.request(
      { target: '/workspace/graphoxide-out', origin: 'automatic', label: 'update on save', failurePolicy: 'pause-automatic' },
      async () => {
        executions += 1;
        throw new Error('retained extraction output exceeds its budget');
      },
    ),
    /exceeds its budget/u,
  );

  const suppressed = await coordinator.request(
    { target: '/workspace/graphoxide-out', origin: 'automatic', label: 'update on save', failurePolicy: 'pause-automatic' },
    async () => {
      executions += 1;
      return 'unexpected';
    },
  );
  assert.equal(suppressed.kind, 'suppressed');
  assert.equal(executions, 1);

  const recovered = await coordinator.request(
    { target: '/workspace/graphoxide-out', origin: 'interactive', label: 'full rebuild', failurePolicy: 'pause-automatic' },
    async () => {
      executions += 1;
      return 'recovered';
    },
  );
  assert.equal(recovered.kind, 'completed');
  assert.equal(executions, 2);
  assert.deepEqual(coordinator.snapshot().automaticFailures, []);
});

test('does not pause automatic work after a classified cancellation', async () => {
  const coordinator = new GraphMutationCoordinator();
  const cancellation = new Error('cancelled');
  await assert.rejects(
    coordinator.request(
      { target: '/workspace/out', origin: 'interactive', label: 'cancelled rebuild', failurePolicy: 'pause-automatic' },
      async () => { throw cancellation; },
      (error) => error !== cancellation,
    ),
    /cancelled/u,
  );
  const automatic = await coordinator.request(
    { target: '/workspace/out', origin: 'automatic', label: 'save update', failurePolicy: 'pause-automatic' },
    async () => 'updated',
  );
  assert.equal(automatic.kind, 'completed');
});

test('report-only failures do not suppress structural automatic updates', async () => {
  const coordinator = new GraphMutationCoordinator();
  await assert.rejects(
    coordinator.request(
      { target: '/workspace/out', origin: 'interactive', label: 'community labeling', failurePolicy: 'report-only' },
      async () => { throw new Error('provider authentication failed'); },
    ),
    /authentication failed/u,
  );
  assert.deepEqual(coordinator.snapshot().automaticFailures, []);
  const structural = await coordinator.request(
    { target: '/workspace/out', origin: 'automatic', label: 'managed refresh', failurePolicy: 'pause-automatic' },
    async () => 'updated',
  );
  assert.equal(structural.kind, 'completed');
});

test('watch startup is report-only and cannot clear a structural failure latch', async () => {
  const coordinator = new GraphMutationCoordinator();
  const target = '/workspace/out';
  await assert.rejects(coordinator.request(
    { target, origin: 'automatic', label: 'starting watch mode', failurePolicy: 'report-only' },
    async () => { throw new Error('watch failed before readiness'); },
  ));
  assert.deepEqual(coordinator.snapshot().automaticFailures, []);

  await assert.rejects(coordinator.request(
    { target, origin: 'interactive', label: 'full rebuild', failurePolicy: 'pause-automatic' },
    async () => { throw new Error('structural failure'); },
  ));
  const watch = await coordinator.request(
    { target, origin: 'interactive', label: 'starting watch mode', failurePolicy: 'report-only' },
    async () => 'watching',
  );
  assert.equal(watch.kind, 'completed');
  assert.deepEqual(coordinator.snapshot().automaticFailures, [target]);
});

test('report-only work cannot clear a prior structural failure latch', async () => {
  const coordinator = new GraphMutationCoordinator();
  const target = '/workspace/out';
  await assert.rejects(coordinator.request(
    { target, origin: 'automatic', label: 'managed refresh', failurePolicy: 'pause-automatic' },
    async () => { throw new Error('structural failure'); },
  ));

  const labeling = await coordinator.request(
    { target, origin: 'interactive', label: 'community labeling', failurePolicy: 'report-only' },
    async () => 'labeled',
  );
  assert.equal(labeling.kind, 'completed');
  assert.deepEqual(coordinator.snapshot().automaticFailures, [target]);

  await assert.rejects(coordinator.request(
    { target, origin: 'interactive', label: 'community relabeling', failurePolicy: 'report-only' },
    async () => { throw new Error('provider failure'); },
  ));
  assert.deepEqual(coordinator.snapshot().automaticFailures, [target]);

  const suppressed = await coordinator.request(
    { target, origin: 'automatic', label: 'save update', failurePolicy: 'pause-automatic' },
    async () => 'unexpected',
  );
  assert.equal(suppressed.kind, 'suppressed');

  const recovery = await coordinator.request(
    { target, origin: 'interactive', label: 'full rebuild', failurePolicy: 'pause-automatic' },
    async () => 'rebuilt',
  );
  assert.equal(recovery.kind, 'completed');
  assert.deepEqual(coordinator.snapshot().automaticFailures, []);
});

test('bounds mutations globally while allowing the next target after idle', async () => {
  const coordinator = new GraphMutationCoordinator();
  const barrier = new Barrier();
  const first = coordinator.request(
    { target: '/workspace-a/out', origin: 'interactive', label: 'workspace A', failurePolicy: 'pause-automatic' },
    () => barrier.hold(),
  );
  await barrier.reached;
  const blocked = await coordinator.request(
    { target: '/workspace-b/out', origin: 'automatic', label: 'workspace B', failurePolicy: 'pause-automatic' },
    async () => undefined,
  );
  assert.equal(blocked.kind, 'busy');
  barrier.release();
  await first;
  const second = await coordinator.request(
    { target: '/workspace-b/out', origin: 'automatic', label: 'workspace B', failurePolicy: 'pause-automatic' },
    async () => 'done',
  );
  assert.equal(second.kind, 'completed');
});

test('disposal releases idle waiters and rejects future admission', async () => {
  const coordinator = new GraphMutationCoordinator();
  const barrier = new Barrier();
  const active = coordinator.request(
    { target: '/workspace/out', origin: 'automatic', label: 'active', failurePolicy: 'pause-automatic' },
    () => barrier.hold(),
  );
  await barrier.reached;
  const idle = coordinator.waitForIdle();
  coordinator.dispose();
  await idle;
  const disposed = await coordinator.request(
    { target: '/workspace/out', origin: 'interactive', label: 'late', failurePolicy: 'pause-automatic' },
    async () => undefined,
  );
  assert.equal(disposed.kind, 'disposed');
  barrier.release();
  await active;
});
