import assert from 'node:assert/strict';
import { EventEmitter } from 'node:events';
import test from 'node:test';
import {
  classifyWatchProcessClose,
  CloseObservableProcess,
  ProcessTracker,
  quarantineUnclosedWatchProcess,
  SharedWatchRelease,
  trackProcessUntilClose,
  waitForProcessClose,
  WatchStartupDeadline,
  WatchStartupDeadlineScheduler,
} from '../src/process-tracker';

class FakeProcess extends EventEmitter implements CloseObservableProcess {
  exitCode: number | null;
  signalCode: string | null = null;
  readonly signals: Array<NodeJS.Signals | number | undefined> = [];

  constructor(
    exitCode: number | null = null,
    private readonly acceptsSignal = true,
    private readonly recordsExitSignal = true,
  ) {
    super();
    this.exitCode = exitCode;
  }

  kill(signal?: NodeJS.Signals | number): boolean {
    this.signals.push(signal);
    if (this.acceptsSignal && this.recordsExitSignal) this.signalCode = typeof signal === 'string' ? signal : 'SIGTERM';
    return this.acceptsSignal;
  }
}

class ManualDeadlineScheduler implements WatchStartupDeadlineScheduler {
  private nextHandle = 1;
  private readonly tasks = new Map<number, { readonly callback: () => void; readonly delayMs: number }>();

  set(callback: () => void, delayMs: number): unknown {
    const handle = this.nextHandle;
    this.nextHandle += 1;
    this.tasks.set(handle, { callback, delayMs });
    return handle;
  }

  clear(handle: unknown): void {
    if (typeof handle === 'number') this.tasks.delete(handle);
  }

  runNext(): number {
    const entry = this.tasks.entries().next().value as [number, { readonly callback: () => void; readonly delayMs: number }] | undefined;
    assert.ok(entry, 'No scheduled deadline was available.');
    const [handle, task] = entry;
    this.tasks.delete(handle);
    task.callback();
    return task.delayMs;
  }

  get size(): number {
    return this.tasks.size;
  }
}

test('shares one bounded completion across every caller blocked by a watch generation', async () => {
  const release = new SharedWatchRelease(17);
  const completion = release.completion;
  for (let index = 0; index < 10_000; index += 1) {
    assert.equal(release.completion, completion);
  }

  let settled = false;
  void completion.then(() => { settled = true; });
  await Promise.resolve();
  assert.equal(settled, false);
  assert.equal(release.settle('failed'), true);
  assert.deepEqual(await completion, { generation: 17, status: 'failed' });
  assert.equal(settled, true);
  assert.equal(release.settle('completed'), false);
});

test('signals each running tracked process exactly once on disposal', () => {
  const tracker = new ProcessTracker<FakeProcess>();
  const first = new FakeProcess();
  const second = new FakeProcess();
  const exited = new FakeProcess(0);
  tracker.track(first);
  tracker.track(second);
  tracker.track(exited);

  assert.equal(tracker.terminateAll(), 2);
  assert.deepEqual(first.signals, ['SIGTERM']);
  assert.deepEqual(second.signals, ['SIGTERM']);
  assert.deepEqual(exited.signals, []);
  assert.equal(tracker.terminateAll(), 0);
});

test('released processes are not signalled', () => {
  const tracker = new ProcessTracker<FakeProcess>();
  const child = new FakeProcess();
  tracker.track(child);
  tracker.release(child);
  assert.equal(tracker.size, 0);
  assert.equal(tracker.terminateAll(), 0);
  assert.deepEqual(child.signals, []);
});

test('an error before close retains process ownership and settles only on close', async () => {
  const tracker = new ProcessTracker<FakeProcess>();
  const child = new FakeProcess();
  let settled = false;
  const closed = trackProcessUntilClose(tracker, child).then((result) => {
    settled = true;
    return result;
  });
  const processError = new Error('spawn failed after returning a child');

  child.emit('error', processError);
  await Promise.resolve();
  assert.equal(settled, false);
  assert.equal(tracker.size, 1);

  child.exitCode = -2;
  child.emit('close', -2, null);
  const result = await closed;
  assert.equal(result.error, processError);
  assert.equal(result.code, -2);
  assert.equal(tracker.size, 0);
});

test('watch close classification separates intentional startup cancellation from failures', () => {
  const processError = new Error('spawn error');
  assert.deepEqual(classifyWatchProcessClose({
    reachedReady: false,
    intentional: true,
    code: null,
    signal: 'SIGTERM',
    error: processError,
  }), { kind: 'cancelled' });
  assert.deepEqual(classifyWatchProcessClose({
    reachedReady: false,
    intentional: false,
    code: -2,
    signal: null,
    error: processError,
  }), { kind: 'startup-failure', error: processError });
});

test('only an unexpected post-ready watch close produces a direct runtime failure', () => {
  assert.equal(classifyWatchProcessClose({
    reachedReady: true,
    intentional: true,
    code: 0,
    signal: null,
  }).kind, 'stopped');
  const unexpected = classifyWatchProcessClose({
    reachedReady: true,
    intentional: false,
    code: 0,
    signal: null,
  });
  assert.equal(unexpected.kind, 'runtime-failure');
  if (unexpected.kind === 'runtime-failure') assert.match(unexpected.error.message, /after readiness \(code 0\)/u);
});

test('waiting for watch stop ignores error until close confirms ownership release', async () => {
  const child = new FakeProcess();
  child.on('error', () => undefined);
  let settled = false;
  const close = waitForProcessClose(child, 1000, 'timed out').then(() => { settled = true; });
  child.emit('error', new Error('early process error'));
  await Promise.resolve();
  assert.equal(settled, false);
  child.emit('close', 1, null);
  await close;
  assert.equal(settled, true);
});

test('watch startup deadline requests stop then reaches bounded quarantine grace', () => {
  const scheduler = new ManualDeadlineScheduler();
  const events: string[] = [];
  const deadline = new WatchStartupDeadline(10_000, 2_000, {
    onReadinessTimeout: () => events.push('stop'),
    onStopGraceTimeout: () => events.push('quarantine'),
  }, scheduler);

  deadline.start();
  assert.equal(scheduler.size, 1);
  assert.equal(scheduler.runNext(), 10_000);
  assert.deepEqual(events, ['stop']);
  assert.equal(scheduler.size, 1);
  assert.equal(scheduler.runNext(), 2_000);
  assert.deepEqual(events, ['stop', 'quarantine']);
  assert.equal(scheduler.size, 0);
  deadline.dispose();
});

test('quarantine escalates only the still-running extension-owned watch child', () => {
  const child = new FakeProcess(null, true, false);
  const quarantine = quarantineUnclosedWatchProcess(child, true, 10_000, 2_000);
  assert.ok(quarantine);
  assert.deepEqual(child.signals, ['SIGKILL']);
  assert.match(quarantine.message, /quarantined it as the active graph writer/u);
  assert.match(quarantine.message, /reload the VS Code window/u);

  const unowned = new FakeProcess(null, true, false);
  assert.equal(quarantineUnclosedWatchProcess(unowned, false, 10_000, 2_000), undefined);
  assert.deepEqual(unowned.signals, []);
});
