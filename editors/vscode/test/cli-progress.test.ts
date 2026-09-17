import assert from 'node:assert/strict';
import { EventEmitter } from 'node:events';
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import path from 'node:path';
import { PassThrough } from 'node:stream';
import test from 'node:test';
import { runInThisContext } from 'node:vm';
import type * as vscode from 'vscode';
import type { BuildProgressSnapshot, GraphoxideCli } from '../src/cli';

class TestEmitter<T> {
  private readonly listeners = new Set<(value: T) => void>();
  readonly event = (listener: (value: T) => void): vscode.Disposable => {
    this.listeners.add(listener);
    return { dispose: () => { this.listeners.delete(listener); } };
  };
  fire(value: T): void { for (const listener of this.listeners) listener(value); }
  dispose(): void { this.listeners.clear(); }
}

class TestCancellationError extends Error {}

class TestCancellationTokenSource {
  private readonly emitter = new TestEmitter<void>();
  readonly token = {
    isCancellationRequested: false,
    onCancellationRequested: this.emitter.event,
  };
  cancel(): void {
    if (this.token.isCancellationRequested) return;
    this.token.isCancellationRequested = true;
    this.emitter.fire();
  }
  dispose(): void { this.emitter.dispose(); }
}

class TestChild extends EventEmitter {
  readonly stdout = new PassThrough();
  readonly stderr = new PassThrough();
  readonly stdin = new PassThrough();
  exitCode: number | null = null;
  signalCode: NodeJS.Signals | null = null;
  readonly signals: NodeJS.Signals[] = [];
  progressNonce?: string;
  deferClose = false;
  kill(signal: NodeJS.Signals = 'SIGTERM'): boolean {
    this.signals.push(signal);
    if (this.exitCode !== null || this.signalCode !== null) return false;
    this.signalCode = signal;
    if (!this.deferClose) queueMicrotask(() => this.emit('close', null, signal));
    return true;
  }
  finish(): void {
    if (this.signalCode !== null) return;
    this.exitCode = 0;
    this.emit('close', 0, null);
  }
}

interface Harness {
  readonly cli: GraphoxideCli;
  readonly observations: (BuildProgressSnapshot | undefined)[];
  readonly nativeProgress: vscode.ProgressOptions[];
  readonly invocations: (readonly string[])[];
  readonly children: TestChild[];
  readonly cancellationFrames: string[];
  readonly outputText: string[];
}

/** Exercise the actual child/process lifecycle while replacing only host APIs. */
function createHarness(onGraphPublished?: (target: string) => Promise<unknown>): Harness {
  const nativeProgress: vscode.ProgressOptions[] = [];
  const observations: (BuildProgressSnapshot | undefined)[] = [];
  const invocations: (readonly string[])[] = [];
  const children: TestChild[] = [];
  const cancellationFrames: string[] = [];
  const outputText: string[] = [];
  const output = { append: (value: string) => outputText.push(value), info: () => {}, error: () => {}, show: () => {}, dispose: () => {} };
  const host = {
    EventEmitter: TestEmitter,
    CancellationError: TestCancellationError,
    CancellationTokenSource: TestCancellationTokenSource,
    ProgressLocation: { Window: 10, Notification: 15 },
    commands: { executeCommand: async () => undefined },
    workspace: {
      isTrusted: true,
      getConfiguration: () => ({ get: (_key: string, fallback: unknown) => fallback }),
    },
    window: {
      createOutputChannel: () => output,
      showInformationMessage: async () => undefined,
      withProgress: async (options: vscode.ProgressOptions, task: (progress: vscode.Progress<unknown>, token: vscode.CancellationToken) => Promise<unknown>) => {
        nativeProgress.push(options);
        return task({ report: () => {} }, new TestCancellationTokenSource().token);
      },
    },
  };
  const spawn = (_command: string, args: readonly string[], options: { readonly env: NodeJS.ProcessEnv }): TestChild => {
    invocations.push(args);
    const child = new TestChild();
    child.progressNonce = options.env.GRAPHOXIDE_PROGRESS_NONCE;
    children.push(child);
    let cancelledCooperatively = false;
    child.stdin.on('data', (data: Buffer) => {
      assert.equal(options.env.GRAPHOXIDE_CANCEL_STDIN, '1');
      assert.equal(data.toString(), `${options.env.GRAPHOXIDE_PROGRESS_NONCE}\n`);
      cancellationFrames.push(data.toString());
      cancelledCooperatively = true;
    });
    queueMicrotask(() => {
      const operation = args[0];
      if (operation === 'watch') { child.stdout.write('Watching /workspace\n'); return; }
      const activity = operation === 'label' || operation === 'wiki';
      const nonce = options.env.GRAPHOXIDE_PROGRESS_NONCE;
      const prefix = activity ? '[graphoxide-activity] ' : '[graphoxide-progress] ';
      const send = (event: Record<string, unknown>): void => {
        if (child.signalCode !== null) return;
        child.stderr.write(`${prefix}${JSON.stringify({ schema_version: 1, run_nonce: nonce, operation, ...event })}\n`);
      };
      if (args.includes('--progress=json')) {
        send({ type: 'started', ...(activity ? {} : { mode: 'full' }) });
        send({ type: 'phase', phase: activity ? 'preparing' : 'building' });
        send({ type: 'phase', phase: activity ? operation === 'label' ? 'labeling' : 'authoring' : 'building', processed: 0, total: 2 });
        send({ type: 'phase', phase: activity ? operation === 'label' ? 'labeling' : 'authoring' : 'building', processed: 2, total: 2 });
        send({ type: 'phase', phase: 'publishing' });
        if (activity) send({ type: 'completed' });
      }
      if (!cancelledCooperatively) child.finish();
    });
    return child;
  };
  const modulePath = path.join(__dirname, '..', 'src', 'cli.js');
  const localRequire = createRequire(modulePath);
  const runtime = {
    extensionInvocation: () => ({ command: 'test-graphoxide', args: ['serve'] }),
    trustedExtensionInvocation: () => ({ command: 'test-graphoxide', args: [] }),
  };
  const requireDependency = (name: string): unknown => {
    if (name === 'vscode') return host;
    if (name === 'node:child_process') return { spawn };
    if (name === './mcp/runtime') return runtime;
    return localRequire(name);
  };
  const exports: { GraphoxideCli?: new (uri: vscode.Uri, state?: vscode.Memento, onGraphPublished?: (target: string) => Promise<unknown>) => GraphoxideCli } = {};
  const load = runInThisContext(`(function(require, exports) { ${readFileSync(modulePath, 'utf8')}\n})`, { filename: modulePath }) as (require: (name: string) => unknown, exports: object) => void;
  load(requireDependency, exports);
  assert.ok(exports.GraphoxideCli);
  const cli = new exports.GraphoxideCli({ fsPath: '/extension' } as vscode.Uri, undefined, onGraphPublished);
  cli.onDidChangeBuildProgress((progress) => observations.push(progress));
  return { cli, observations, nativeProgress, invocations, children, cancellationFrames, outputText };
}

const folder = { uri: { fsPath: '/workspace' }, name: 'workspace', index: 0 } as vscode.WorkspaceFolder;

for (const args of [['extract', '.'], ['label'], ['wiki', 'source', 'add', '/source.md', '--allow-model-egress']] as const) {
  test(`${args.join(' ')} uses one managed progress surface through publication`, async () => {
    const harness = createHarness();
    try {
      const outcome = await harness.cli.runMutation({
        title: 'Graphoxide task', folder, args,
        mutationTarget: '/workspace/graphoxide-out', mutationOrigin: 'interactive',
        mutationLabel: 'working', suppressAutomaticOnFailure: false,
      });
      assert.equal(outcome.kind, 'completed');
      assert.equal(harness.nativeProgress.length, 0, 'Managed progress duplicated the VS Code native window progress.');
      assert.ok(harness.invocations[0]?.includes('--progress=json'));
      const active = harness.observations.filter((value) => value !== undefined);
      assert.ok(active.length >= 3, 'The operation did not stream its phases.');
      assert.ok(active.every((value) => value.presentation === 'status'));
      assert.ok(active.some((value) => /\(2\/2\)/u.test(value.message)));
      assert.match(active.at(-1)!.message, /Publishing/u);
      assert.equal(harness.observations.at(-1), undefined);
    } finally {
      harness.cli.dispose();
    }
  });
}

test('interactive query uses the shared status without requesting an unsupported protocol', async () => {
  const harness = createHarness();
  try {
    await harness.cli.run({ title: 'Query graph', folder, args: ['query', 'example'] });
    assert.equal(harness.nativeProgress.length, 0);
    assert.equal(harness.observations.length, 2);
    assert.equal(harness.observations[0]?.operation, 'command');
    assert.equal(harness.observations[0]?.message, 'Query graph');
    assert.equal(harness.observations.at(-1), undefined);
    assert.equal(harness.invocations[0]?.includes('--progress=json'), false);
  } finally {
    harness.cli.dispose();
  }
});

test('internal commands opting out of progress remain silent', async () => {
  const harness = createHarness();
  try {
    await harness.cli.run({ title: 'Inspect version', folder, args: ['--version'], showProgress: false });
    assert.equal(harness.nativeProgress.length, 0);
    assert.equal(harness.observations.length, 0);
    assert.equal(harness.invocations.at(-1)?.includes('--progress=json'), false);
  } finally {
    harness.cli.dispose();
  }
});

test('managed progress stays visible until the completed graph is loaded into the UI', async () => {
  const harness = createHarness();
  let releaseLoad!: () => void;
  let reachedLoad!: () => void;
  const loading = new Promise<void>((resolve) => { reachedLoad = resolve; });
  const release = new Promise<void>((resolve) => { releaseLoad = resolve; });
  const command = harness.cli.runMutation({
    title: 'Graphoxide update', folder, args: ['update', '.'],
    mutationTarget: '/workspace/graphoxide-out', mutationOrigin: 'interactive',
    mutationLabel: 'updating', suppressAutomaticOnFailure: false,
    afterSuccess: async () => { reachedLoad(); await release; },
  });
  try {
    await loading;
    assert.match(harness.cli.buildProgress?.message ?? '', /Loading graph/u);
    assert.ok(harness.observations.every((value) => value !== undefined), 'Progress cleared before the UI loaded the new graph.');
    assert.equal(harness.cli.mutationLifecycle().phase, 'running');
    releaseLoad();
    assert.equal((await command).kind, 'completed');
    assert.equal(harness.observations.at(-1), undefined);
  } finally {
    releaseLoad();
    await command;
    harness.cli.dispose();
  }
});

test('Control Center cancellation clears progress without pausing automatic updates', async () => {
  const harness = createHarness();
  const subscription = harness.cli.onDidChangeBuildProgress((progress) => {
    if (progress?.message.startsWith('Building graph')) harness.cli.cancelActiveBuild();
  });
  try {
    await assert.rejects(harness.cli.runMutation({
      title: 'Graphoxide update', folder, args: ['update', '.'],
      mutationTarget: '/workspace/graphoxide-out', mutationOrigin: 'automatic',
      mutationLabel: 'updating', suppressAutomaticOnFailure: true,
    }), TestCancellationError);
    assert.equal(harness.observations.at(-1), undefined);
    assert.equal(harness.cli.mutationLifecycle().phase, 'idle');
    assert.deepEqual(harness.cli.mutationLifecycle().automaticFailures, []);
  } finally {
    subscription.dispose();
    harness.cli.dispose();
  }
});

test('overlapping Wiki completion restores the graph progress still loading behind it', async () => {
  const harness = createHarness();
  let releaseGraph!: () => void;
  let releaseWiki!: () => void;
  let graphReached!: () => void;
  let wikiReached!: () => void;
  const graphLoading = new Promise<void>((resolve) => { graphReached = resolve; });
  const wikiLoading = new Promise<void>((resolve) => { wikiReached = resolve; });
  const graphGate = new Promise<void>((resolve) => { releaseGraph = resolve; });
  const wikiGate = new Promise<void>((resolve) => { releaseWiki = resolve; });
  const graph = harness.cli.runMutation({
    title: 'Graphoxide graph', folder, args: ['extract', '.'],
    mutationTarget: '/workspace/graphoxide-out', mutationOrigin: 'interactive',
    mutationLabel: 'building', suppressAutomaticOnFailure: false,
    afterSuccess: async () => { graphReached(); await graphGate; },
  });
  await graphLoading;
  const graphGeneration = harness.cli.buildProgress?.generation;
  const wiki = harness.cli.run({
    title: 'Graphoxide Wiki', folder, args: ['wiki', 'source', 'add', 'input.md'],
    afterSuccess: async () => { wikiReached(); await wikiGate; },
  });
  try {
    await wikiLoading;
    assert.equal(harness.cli.buildProgress?.operation, 'wiki');
    assert.notEqual(harness.cli.buildProgress?.generation, graphGeneration);
    releaseWiki();
    await wiki;
    assert.equal(harness.cli.buildProgress?.operation, 'extract');
    assert.equal(harness.cli.buildProgress?.generation, graphGeneration);
    assert.ok(harness.observations.every((value) => value !== undefined), 'A finishing Wiki hid an unfinished graph build.');
    releaseGraph();
    await graph;
    assert.equal(harness.cli.buildProgress, undefined);
  } finally {
    releaseWiki();
    releaseGraph();
    await Promise.all([wiki, graph]);
    harness.cli.dispose();
  }
});

test('cancellation during the final UI load remains cancellation', async () => {
  const harness = createHarness();
  try {
    await assert.rejects(harness.cli.runMutation({
      title: 'Graphoxide update', folder, args: ['update', '.'],
      mutationTarget: '/workspace/graphoxide-out', mutationOrigin: 'automatic',
      mutationLabel: 'updating', suppressAutomaticOnFailure: true,
      afterSuccess: async () => { harness.cli.cancelActiveBuild(); },
    }), TestCancellationError);
    assert.equal(harness.cli.buildProgress, undefined);
    assert.deepEqual(harness.cli.mutationLifecycle().automaticFailures, []);
  } finally {
    harness.cli.dispose();
  }
});

for (const dispose of [false, true]) {
  test(`Wiki ${dispose ? 'disposal' : 'cancellation'} uses authenticated stdin and waits for rollback close`, async () => {
    const harness = createHarness();
    let requested!: () => void;
    const request = new Promise<void>((resolve) => { requested = resolve; });
    const subscription = harness.cli.onDidChangeBuildProgress((progress) => {
      if (!progress?.message.startsWith('Writing Wiki')) return;
      if (dispose) harness.cli.dispose();
      else harness.cli.cancelActiveBuild();
      requested();
    });
    const run = harness.cli.run({ title: 'Build Wiki', folder, args: ['wiki', 'source', 'add', '/source.md', '--allow-model-egress'] });
    const cancellation = assert.rejects(run, TestCancellationError);
    try {
      await request;
      assert.equal(harness.cancellationFrames.length, 1);
      assert.match(harness.cancellationFrames[0]!, /^[0-9a-f]{32}\n$/u);
      assert.deepEqual(harness.children[0]?.signals, [], 'Wiki rollback must never be interrupted with process signals.');
      assert.ok(harness.cli.buildProgress, 'Wiki cancellation cleared before rollback and child close.');
      if (!dispose) assert.equal(harness.cli.buildProgress.message, 'Cancelling Wiki and cleaning up…');
      harness.children[0]!.stderr.write('Wiki rollback failed: retained recovery artifacts.\n');
      harness.children[0]!.finish();
      await cancellation;
      assert.equal(harness.cli.buildProgress, undefined);
      if (!dispose) assert.match(harness.outputText.join(''), /Wiki rollback failed: retained recovery artifacts/u);
    } finally {
      harness.children[0]?.finish();
      await cancellation;
      subscription.dispose();
      harness.cli.dispose();
    }
  });
}

test('extension-side loading exposes one cancellable owned activity until work settles', async () => {
  const harness = createHarness();
  let release!: () => void;
  let observedToken: vscode.CancellationToken | undefined;
  const held = new Promise<void>((resolve) => { release = resolve; });
  let cancelCalls = 0;
  const work = harness.cli.runUiActivity('Loading graph…', async (token) => {
    observedToken = token;
    await held;
  }, () => { cancelCalls += 1; });
  const cancelled = assert.rejects(work, TestCancellationError);
  try {
    assert.equal(harness.cli.buildProgress?.message, 'Loading graph…');
    harness.cli.cancelActiveBuild();
    assert.equal(observedToken?.isCancellationRequested, true);
    assert.equal(cancelCalls, 1);
    assert.equal(harness.cli.buildProgress?.message, 'Cancelling…');
    release();
    await cancelled;
    assert.equal(harness.cli.buildProgress, undefined);
  } finally {
    release();
    await cancelled;
    harness.cli.dispose();
  }
});

function watchEvent(child: TestChild, value: Record<string, unknown>): void {
  child.stderr.write(`[graphoxide-progress] ${JSON.stringify({ schema_version: 1, run_nonce: child.progressNonce, operation: 'update', ...value })}\n`);
}

function completeWatchPass(child: TestChild): void {
  watchEvent(child, { type: 'completed', mode: 'incremental', status: 'rebuilt', elapsed_ms: 1,
    stages_ms: { scan_extract: 0, detect: 0, extract: 0, build: 1, cluster: 0, write: 0 },
    files: { indexed: 1, changed: 1, deleted: 0 } });
}

test('watch startup is visible and pass progress remains until the graph reload finishes', async () => {
  let release!: () => void;
  let loaded!: () => void;
  const held = new Promise<void>((resolve) => { release = resolve; });
  const loading = new Promise<void>((resolve) => { loaded = resolve; });
  const harness = createHarness(async (target) => {
    assert.equal(target, '/workspace/out');
    loaded();
    await held;
  });
  try {
    assert.deepEqual(await harness.cli.startWatch(folder, { GRAPHOXIDE_OUT: '/workspace/out' }), { kind: 'watching' });
    assert.ok(harness.observations.some((value) => value?.message === 'Starting watch mode…'));
    assert.equal(harness.observations.at(-1), undefined, 'Idle watch mode must clear startup progress.');
    const child = harness.children[0]!;
    watchEvent(child, { type: 'started', mode: 'adaptive' });
    watchEvent(child, { type: 'phase', phase: 'publishing' });
    completeWatchPass(child);
    await loading;
    assert.equal(harness.cli.buildProgress?.message, 'Loading graph…');
    completeWatchPass(child); // A duplicate terminal cannot clear an active reload.
    assert.equal(harness.cli.buildProgress?.message, 'Loading graph…');
    release();
    await Promise.resolve();
    await Promise.resolve();
    assert.equal(harness.cli.buildProgress, undefined);
    await harness.cli.stopWatchAndWait();
  } finally {
    release();
    harness.cli.dispose();
  }
});

test('an older watch reload cannot clear a newer pass or stop cleanup', async () => {
  let release!: () => void;
  let loaded!: () => void;
  const held = new Promise<void>((resolve) => { release = resolve; });
  const loading = new Promise<void>((resolve) => { loaded = resolve; });
  const harness = createHarness(async () => { loaded(); await held; });
  try {
    await harness.cli.startWatch(folder, { GRAPHOXIDE_OUT: '/workspace/out' });
    const child = harness.children[0]!;
    watchEvent(child, { type: 'started', mode: 'adaptive' });
    completeWatchPass(child);
    await loading;
    watchEvent(child, { type: 'started', mode: 'adaptive' });
    watchEvent(child, { type: 'phase', phase: 'extracting', processed: 0, total: 2 });
    const current = harness.cli.buildProgress;
    release();
    await Promise.resolve();
    await Promise.resolve();
    assert.equal(harness.cli.buildProgress, current);
    harness.cli.cancelActiveBuild();
    assert.equal(harness.cli.buildProgress?.message, 'Stopping watch mode…');
    await Promise.resolve();
    assert.equal(harness.cli.buildProgress, undefined);
  } finally {
    release();
    harness.cli.dispose();
  }
});

for (const dispose of [false, true]) {
  test(`${dispose ? 'disposal' : 'cancellation'} while the previous watcher closes prevents a replacement spawn`, async () => {
    const harness = createHarness();
    await harness.cli.startWatch(folder, { GRAPHOXIDE_OUT: '/workspace/out' });
    const oldChild = harness.children[0]!;
    oldChild.deferClose = true;
    harness.cli.stopWatch();
    let reached!: () => void;
    const restarting = new Promise<void>((resolve) => { reached = resolve; });
    const subscription = harness.cli.onDidChangeBuildProgress((progress) => {
      if (progress?.message === 'Starting watch mode…') reached();
    });
    const restart = harness.cli.startWatch(folder, { GRAPHOXIDE_OUT: '/workspace/out' });
    const cancelled = assert.rejects(restart, TestCancellationError);
    try {
      await restarting;
      if (dispose) harness.cli.dispose();
      else harness.cli.cancelActiveBuild();
      assert.ok(harness.cli.buildProgress, 'Restart cancellation released progress before the old process closed.');
      oldChild.emit('close', null, 'SIGTERM');
      await cancelled;
      assert.equal(harness.children.length, 1, 'A cancelled restart spawned a replacement watcher.');
      assert.equal(harness.cli.watching, false);
      assert.equal(harness.cli.buildProgress, undefined);
    } finally {
      if (harness.cli.watchLifecycle().phase !== 'stopped') oldChild.emit('close', null, 'SIGTERM');
      await cancelled;
      subscription.dispose();
      harness.cli.dispose();
    }
  });
}

test('a buffered watch start after Stop cannot replace stopping progress', async () => {
  const harness = createHarness();
  try {
    await harness.cli.startWatch(folder, { GRAPHOXIDE_OUT: '/workspace/out' });
    const child = harness.children[0]!;
    child.deferClose = true;
    watchEvent(child, { type: 'started', mode: 'adaptive' });
    watchEvent(child, { type: 'phase', phase: 'extracting', processed: 0, total: 2 });
    harness.cli.cancelActiveBuild();
    const stopping = harness.cli.buildProgress;
    assert.equal(stopping?.message, 'Stopping watch mode…');
    watchEvent(child, { type: 'started', mode: 'adaptive' });
    watchEvent(child, { type: 'phase', phase: 'extracting', processed: 1, total: 2 });
    completeWatchPass(child);
    assert.equal(harness.cli.buildProgress, stopping, 'Buffered watch data replaced the stopping status.');
    child.emit('close', null, 'SIGTERM');
    assert.equal(harness.cli.buildProgress, undefined);
  } finally {
    if (harness.cli.watchLifecycle().phase !== 'stopped') harness.children[0]?.emit('close', null, 'SIGTERM');
    harness.cli.dispose();
  }
});
