import assert from 'node:assert/strict';
import test from 'node:test';
import {
  BUILD_PROGRESS_PREFIX,
  BUILD_PROGRESS_MAX_LINE,
  BuildCompletedEvent,
  BuildProgressDecoder,
  BuildProgressEvent,
  BuildProgressFrame,
  BuildProgressRun,
  LatestBuildSummaryStore,
  ownsBuildProgressGeneration,
  phaseProgressMessage,
} from '../src/build-progress';

const nonce = '0123456789abcdef0123456789abcdef';
const forgedNonce = 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa';
const started = {
  schema_version: 1 as const,
  run_nonce: nonce,
  type: 'started' as const,
  operation: 'update' as const,
  mode: 'adaptive' as const,
};
const extracting = {
  schema_version: 1 as const,
  run_nonce: nonce,
  type: 'phase' as const,
  operation: 'update' as const,
  phase: 'extracting' as const,
  processed: 3,
  total: 12,
};
const completed: BuildCompletedEvent = {
  schema_version: 1,
  run_nonce: nonce,
  type: 'completed',
  operation: 'update',
  mode: 'incremental',
  status: 'rebuilt',
  elapsed_ms: 1450,
  stages_ms: { scan_extract: 0, detect: 10, extract: 800, build: 400, cluster: 200, write: 40 },
  files: { indexed: 12, changed: 2, deleted: 1 },
  source_bytes: 9876,
};

function wire(event: unknown, newline = '\n'): string {
  return `${BUILD_PROGRESS_PREFIX}${JSON.stringify(event)}${newline}`;
}

function paddedWire(event: unknown, byteLength: number, newline = '\n'): string {
  const base = wire(event, newline);
  const padding = byteLength - Buffer.byteLength(base, 'utf8');
  assert.ok(padding >= 0, 'requested framed length is smaller than the event');
  return `${base.slice(0, -newline.length)}${' '.repeat(padding)}${newline}`;
}

function consume(frames: readonly BuildProgressFrame[], run: BuildProgressRun): {
  readonly accepted: readonly BuildProgressEvent[];
  readonly stderr: string;
} {
  const accepted: BuildProgressEvent[] = [];
  let stderr = '';
  for (const frame of frames) {
    if (frame.event && run.accept(frame.event)) accepted.push(frame.event);
    else stderr += frame.raw;
  }
  return { accepted, stderr };
}

test('decoder preserves ordered interleaving and fragmented CRLF envelopes', () => {
  const decoder = new BuildProgressDecoder(nonce);
  const run = new BuildProgressRun();
  const stream = `warning A\n${wire(started, '\r\n')}${wire(extracting)}warning B\n${wire(completed)}`;
  const chunks = [stream.slice(0, 19), stream.slice(19, 73), stream.slice(73, -2), stream.slice(-2)];
  const accepted: BuildProgressEvent[] = [];
  let stderr = '';
  for (const chunk of chunks) {
    const result = consume(decoder.push(chunk).frames, run);
    accepted.push(...result.accepted);
    stderr += result.stderr;
  }
  const trailing = consume(decoder.finish().frames, run);
  accepted.push(...trailing.accepted);
  stderr += trailing.stderr;
  assert.deepEqual(accepted, [started, extracting, completed]);
  assert.equal(stderr, 'warning A\nwarning B\n');
  assert.deepEqual(run.successfulCompletion(0, false), completed);
});

test('StringDecoder round-trips a multibyte diagnostic split across child chunks', () => {
  const diagnostic = Buffer.from('before 🧪 after\n', 'utf8');
  const emoji = Buffer.from('🧪', 'utf8');
  const offset = diagnostic.indexOf(emoji);
  const decoder = new BuildProgressDecoder(nonce);
  const frames = [
    ...decoder.push(diagnostic.subarray(0, offset + 1)).frames,
    ...decoder.push(diagnostic.subarray(offset + 1, offset + 3)).frames,
    ...decoder.push(diagnostic.subarray(offset + 3)).frames,
    ...decoder.finish().frames,
  ];
  assert.equal(frames.map((frame) => frame.raw).join(''), diagnostic.toString('utf8'));
  assert.ok(frames.every((frame) => frame.event === undefined));
});

test('decoder bounds oversized partial lines until a real newline boundary', () => {
  const decoder = new BuildProgressDecoder(nonce);
  const oversized = 'x'.repeat(8193);
  assert.equal(decoder.push(oversized).frames.map((frame) => frame.raw).join(''), oversized);
  assert.equal(decoder.push(`${wire(started)}tail`).frames.map((frame) => frame.raw).join(''), wire(started));
  const next = decoder.push(`\n${wire(started)}`).frames;
  assert.equal(next[0]?.raw, 'tail\n');
  assert.deepEqual(next[1]?.event, started);

  const utf8Oversized = `${BUILD_PROGRESS_PREFIX}${JSON.stringify({ ...completed, padding: '🧪'.repeat(2100) })}\n`;
  const utf8 = new BuildProgressDecoder(nonce).push(Buffer.from(utf8Oversized));
  assert.equal(utf8.frames.map((frame) => frame.raw).join(''), utf8Oversized);
  assert.ok(utf8.frames.every((frame) => frame.event === undefined));
});

test('decoder bounds the complete LF or CRLF wire record in UTF-8 bytes', () => {
  const exact = paddedWire(started, BUILD_PROGRESS_MAX_LINE);
  const accepted = new BuildProgressDecoder(nonce).push(exact).frames;
  assert.equal(Buffer.byteLength(exact, 'utf8'), BUILD_PROGRESS_MAX_LINE);
  assert.deepEqual(accepted[0]?.event, started);

  const overLf = paddedWire(started, BUILD_PROGRESS_MAX_LINE + 1);
  const rejectedLf = new BuildProgressDecoder(nonce).push(overLf).frames;
  assert.equal(rejectedLf[0]?.event, undefined);
  assert.equal(rejectedLf[0]?.raw, overLf);

  const overCrlf = paddedWire(started, BUILD_PROGRESS_MAX_LINE + 1, '\r\n');
  const rejectedCrlf = new BuildProgressDecoder(nonce).push(overCrlf).frames;
  assert.equal(rejectedCrlf[0]?.event, undefined);
  assert.equal(rejectedCrlf[0]?.raw, overCrlf);

  const multibyte = `${'🧪'.repeat(Math.floor(BUILD_PROGRESS_MAX_LINE / 4))}x\n`;
  assert.equal(Buffer.byteLength(multibyte, 'utf8'), BUILD_PROGRESS_MAX_LINE + 2);
  const diagnostic = new BuildProgressDecoder(nonce).push(Buffer.from(multibyte)).frames;
  assert.equal(diagnostic.map((frame) => frame.raw).join(''), multibyte);
  assert.ok(diagnostic.every((frame) => frame.event === undefined));
});

test('decoder authenticates exact bounded payloads and leaves forged or malformed lines raw', () => {
  const forged = { ...started, run_nonce: forgedNonce };
  const invalid = [
    forged,
    { ...completed, path: '/private/source' },
    { ...completed, files: { ...completed.files, warning: 'source text' } },
    { ...completed, source_bytes: Number.MAX_SAFE_INTEGER + 1 },
    { ...extracting, total: undefined },
    { ...extracting, processed: 13 },
    { ...extracting, mode: 'adaptive' },
  ];
  const text = invalid.map((event) => wire(event)).join('');
  const decoded = new BuildProgressDecoder(nonce).push(text);
  assert.equal(decoded.frames.map((frame) => frame.raw).join(''), text);
  assert.ok(decoded.frames.every((frame) => frame.event === undefined));
});

test('sequence-rejected authenticated records survive in exact stderr order', () => {
  const duplicateStart = wire(started);
  const regressing = wire({ ...extracting, phase: 'scanning', processed: undefined, total: undefined });
  const stream = `${wire(started)}warning A\n${duplicateStart}${wire(extracting)}${regressing}warning B\n${wire(completed)}`;
  const decoder = new BuildProgressDecoder(nonce);
  const result = consume(decoder.push(stream).frames, new BuildProgressRun());
  assert.deepEqual(result.accepted.map((event) => event.type), ['started', 'phase', 'completed']);
  assert.equal(result.stderr, `warning A\n${duplicateStart}${regressing}warning B\n`);
});

test('run requires matching start, monotonic phases/counters, one terminal, and successful close', () => {
  const lone = new BuildProgressRun();
  assert.equal(lone.accept(completed), false);

  const run = new BuildProgressRun();
  assert.equal(run.accept(started), true);
  assert.equal(run.accept({ ...extracting, phase: 'scanning', processed: undefined, total: undefined }), true);
  assert.equal(run.accept({ ...extracting, processed: 0 }), true);
  assert.equal(run.accept({ ...extracting, processed: 5 }), true);
  assert.equal(run.accept({ ...extracting, processed: 4 }), false);
  assert.equal(run.accept({ ...extracting, processed: 6, total: 13 }), false);
  assert.equal(run.accept({ ...extracting, operation: 'index' }), false);
  assert.equal(run.accept({ ...extracting, run_nonce: forgedNonce }), false);
  assert.equal(run.accept({ ...extracting, phase: 'building', processed: undefined, total: undefined }), true);
  assert.equal(run.accept(completed), true);
  assert.equal(run.accept({ ...extracting, phase: 'publishing', processed: undefined, total: undefined }), false);
  assert.equal(run.successfulCompletion(1, false), undefined);
  assert.equal(run.successfulCompletion(0, true), undefined);
  assert.deepEqual(run.successfulCompletion(0, false), completed);

  const known = new BuildProgressRun();
  assert.equal(known.accept({ ...started, mode: 'full' }), true);
  assert.equal(known.accept(completed), false);
  assert.equal(known.accept({ ...completed, mode: 'full' }), true);

  const adaptiveFailure = new BuildProgressRun();
  assert.equal(adaptiveFailure.accept(started), true);
  assert.equal(adaptiveFailure.accept({ ...started, type: 'failed', mode: 'full' }), false);
  assert.equal(adaptiveFailure.accept({ ...started, type: 'failed' }), true);
});

test('phase messages expose phase and known counters without inventing percentage', () => {
  assert.equal(phaseProgressMessage(extracting), 'Extracting inputs… (3/12)');
  assert.equal(
    phaseProgressMessage({ ...extracting, phase: 'waiting', processed: undefined, total: undefined }),
    'Waiting for build lock…',
  );
});

test('only the owning child generation may update or clear active progress', () => {
  assert.equal(ownsBuildProgressGeneration(12, 12), true);
  assert.equal(ownsBuildProgressGeneration(12, 11), false, 'stale watch frame/close superseded owner');
  assert.equal(ownsBuildProgressGeneration(12, 13), false, 'future generation cleared current owner');
  assert.equal(ownsBuildProgressGeneration(undefined, 12), false);
});

test('summary is conditional, path-free, exact-keyed, and invalidates on graph identity', async () => {
  const values = new Map<string, unknown>();
  const state = {
    get<T>(key: string): T | undefined { return values.get(key) as T | undefined; },
    async update(key: string, value: unknown): Promise<void> { values.set(key, value); },
  };
  const store = new LatestBuildSummaryStore(state);
  let identityReads = 0;
  assert.equal(await store.latestWithIdentity('/private/workspace/out', async () => {
    identityReads += 1;
    return { mtime: 100, size: 200 };
  }), undefined);
  assert.equal(identityReads, 0, 'no pre-success graph stat/backfill');

  await store.record('/private/workspace/out', completed, { mtime: 100, size: 200 }, 300);
  assert.equal((await store.latestWithIdentity('/private/workspace/out', async () => {
    identityReads += 1;
    return { mtime: 100, size: 200 };
  }))?.completedAt, 300);
  assert.equal(identityReads, 1);
  assert.equal(store.latest('/private/workspace/out', { mtime: 101, size: 200 }), undefined);
  assert.equal(store.latest('/private/workspace/out', { mtime: 100, size: 201 }), undefined);
  assert.doesNotMatch(JSON.stringify([...values.values()]), /private|workspace/u);

  const key = [...values.keys()][0];
  assert.ok(key);
  const persisted = values.get(key);
  assert.ok(Array.isArray(persisted));
  values.set(key, [{ ...persisted[0], unexpectedText: '/private/source' }]);
  assert.equal(store.latest('/private/workspace/out', { mtime: 100, size: 200 }), undefined);
});

test('watch generation supersedes delayed identity and retains prior success on failure', async () => {
  const values = new Map<string, unknown>();
  const state = {
    get<T>(key: string): T | undefined { return values.get(key) as T | undefined; },
    async update(key: string, value: unknown): Promise<void> { values.set(key, value); },
  };
  const store = new LatestBuildSummaryStore(state);
  await store.record('/output', completed, { mtime: 1, size: 2 }, 3);
  let release!: () => void;
  let reached!: () => void;
  const gate = new Promise<void>((resolve) => { release = resolve; });
  const startedIdentity = new Promise<void>((resolve) => { reached = resolve; });
  const older = store.recordWithIdentity('/output', { ...completed, elapsed_ms: 99 }, async () => {
    reached();
    await gate;
    return { mtime: 10, size: 20 };
  }, 100);
  await startedIdentity;
  store.invalidatePending('/output'); // authentic next watch pass started, then later failed
  release();
  assert.equal(await older, false);
  assert.equal(store.latest('/output', { mtime: 1, size: 2 })?.completedAt, 3);
  assert.equal(store.latest('/output', { mtime: 10, size: 20 }), undefined);
});

test('invalidation during a pending Memento update compensates the superseded write', async () => {
  const values = new Map<string, unknown>();
  let blockUpdates = false;
  let updateStarted!: () => void;
  let releaseUpdate!: () => void;
  const startedUpdate = new Promise<void>((resolve) => { updateStarted = resolve; });
  const updateGate = new Promise<void>((resolve) => { releaseUpdate = resolve; });
  const state = {
    get<T>(key: string): T | undefined { return values.get(key) as T | undefined; },
    async update(key: string, value: unknown): Promise<void> {
      if (blockUpdates) {
        blockUpdates = false;
        updateStarted();
        await updateGate;
      }
      values.set(key, value);
    },
  };
  const store = new LatestBuildSummaryStore(state);
  await store.record('/output', completed, { mtime: 1, size: 2 }, 3);
  blockUpdates = true;
  const pending = store.recordWithIdentity(
    '/output',
    { ...completed, elapsed_ms: 99 },
    async () => ({ mtime: 10, size: 20 }),
    100,
  );
  await startedUpdate;
  store.invalidatePending('/output');
  releaseUpdate();
  assert.equal(await pending, false);
  assert.equal(store.latest('/output', { mtime: 1, size: 2 })?.completedAt, 3);
  assert.equal(store.latest('/output', { mtime: 10, size: 20 }), undefined);
});
