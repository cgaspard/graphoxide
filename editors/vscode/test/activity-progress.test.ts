import assert from 'node:assert/strict';
import test from 'node:test';
import {
  ACTIVITY_PROGRESS_PREFIX,
  ActivityProgressDecoder,
  ActivityProgressEvent,
  ActivityProgressRun,
  activityProgressMessage,
} from '../src/activity-progress';
import { BUILD_PROGRESS_MAX_LINE } from '../src/build-progress';

const nonce = '0123456789abcdef0123456789abcdef';
const started: ActivityProgressEvent = { schema_version: 1, run_nonce: nonce, operation: 'label', type: 'started' };

function wire(event: unknown, newline = '\n'): string {
  return `${ACTIVITY_PROGRESS_PREFIX}${JSON.stringify(event)}${newline}`;
}

function phase(name: 'preparing' | 'labeling' | 'retrying' | 'publishing', processed?: number, total?: number): ActivityProgressEvent {
  return { ...started, type: 'phase', phase: name, ...(processed === undefined ? {} : { processed, total }) };
}

test('label progress preserves fragmented UTF-8 diagnostics and framed phase order', () => {
  const decoder = new ActivityProgressDecoder(nonce);
  const run = new ActivityProgressRun('label');
  const events = [started, phase('labeling', 0, 4), phase('labeling', 4, 4), phase('publishing'), { ...started, type: 'completed' }];
  const stream = Buffer.from(`before 🧪\n${events.map((event) => wire(event, '\r\n')).join('')}after\n`);
  const frames = [...stream].flatMap((byte) => decoder.push(Buffer.from([byte])).frames);
  frames.push(...decoder.finish().frames);
  const admitted = frames.flatMap((frame) => frame.event && run.accept(frame.event) ? [frame.event] : []);
  assert.deepEqual(admitted, events);
  assert.equal(frames.filter((frame) => !frame.event).map((frame) => frame.raw).join(''), 'before 🧪\nafter\n');
});

test('activity decoder retains forged, malformed, source-derived, and oversized records as diagnostics', () => {
  const invalid = [
    { ...started, run_nonce: 'a'.repeat(32) },
    { ...started, operation: 'extract' },
    { ...started, path: '/private/source' },
    { ...phase('labeling'), phase: 'source-derived phase' },
    { ...phase('labeling'), processed: 1 },
    phase('labeling', 3, 2),
    phase('labeling', -1, 2),
    phase('labeling', 0.5, 2),
    phase('labeling', 0, Number.MAX_SAFE_INTEGER + 1),
    { ...started, type: 'completed', provider_response: 'secret' },
    { ...started, padding: '🧪'.repeat(BUILD_PROGRESS_MAX_LINE) },
  ];
  const text = `${invalid.map((event) => wire(event)).join('')}${ACTIVITY_PROGRESS_PREFIX}{invalid json}\n`;
  const decoder = new ActivityProgressDecoder(nonce);
  const frames = [...decoder.push(text).frames, ...decoder.finish().frames];
  assert.ok(frames.every((frame) => !frame.event));
  assert.equal(frames.map((frame) => frame.raw).join(''), text);
});

test('an unterminated activity completion never clears the lifecycle', () => {
  const decoder = new ActivityProgressDecoder(nonce);
  const line = wire({ ...started, type: 'completed' }).trimEnd();
  const frames = [...decoder.push(line).frames, ...decoder.finish().frames];
  assert.deepEqual(frames, [{ raw: line }]);
});

test('activity lifecycle requires matching start, local monotonic counters, and one terminal', () => {
  const run = new ActivityProgressRun('label');
  assert.equal(run.accept(phase('labeling', 0, 4)), false);
  assert.equal(run.accept({ ...started, operation: 'wiki' }), false);
  assert.equal(run.accept(started), true);
  assert.equal(run.accept(started), false);
  assert.equal(run.accept(phase('preparing')), true);
  assert.equal(run.accept(phase('labeling')), true);
  assert.equal(run.accept(phase('labeling', 0, 4)), true);
  assert.equal(run.accept(phase('labeling', 2, 4)), true);
  assert.equal(run.accept(phase('labeling', 1, 4)), false);
  assert.equal(run.accept(phase('labeling', 3, 5)), false);
  assert.equal(run.accept({ ...phase('labeling', 3, 4), run_nonce: 'a'.repeat(32) }), false);
  assert.equal(run.accept({ ...phase('labeling', 3, 4), operation: 'wiki' }), false);
  assert.equal(run.accept(phase('retrying', 0, 1)), true, 'retry work has its own total');
  assert.equal(run.accept(phase('retrying', 1, 1)), true);
  assert.equal(run.accept(phase('publishing')), true);
  assert.equal(run.accept({ ...started, type: 'completed' }), true);
  assert.equal(run.accept(phase('labeling', 4, 4)), false);
  assert.equal(run.accept({ ...started, type: 'failed' }), false);
});

test('Wiki authoring and review counters advance independently and failure is terminal', () => {
  const run = new ActivityProgressRun('wiki');
  const wiki = { ...started, operation: 'wiki' as const };
  const events: ActivityProgressEvent[] = [
    wiki,
    { ...wiki, type: 'phase', phase: 'admitting', processed: 0, total: 3 },
    { ...wiki, type: 'phase', phase: 'admitting', processed: 3, total: 3 },
    { ...wiki, type: 'phase', phase: 'authoring', processed: 0, total: 2 },
    { ...wiki, type: 'phase', phase: 'authoring', processed: 1, total: 2 },
    { ...wiki, type: 'phase', phase: 'reviewing', processed: 0, total: 1 },
    { ...wiki, type: 'phase', phase: 'reviewing', processed: 1, total: 1 },
    { ...wiki, type: 'phase', phase: 'authoring', processed: 2, total: 2 },
    { ...wiki, type: 'failed' },
  ];
  for (const event of events) assert.equal(run.accept(event), true, JSON.stringify(event));
  assert.equal(run.accept({ ...wiki, type: 'completed' }), false);
});

test('activity messages show known work counters without an overall percentage', () => {
  assert.equal(activityProgressMessage(phase('labeling', 2, 7)), 'Naming communities with AI… (2/7)');
  assert.equal(activityProgressMessage(phase('retrying', 0, 1)), 'Retrying community names with AI… (0/1)');
  assert.equal(activityProgressMessage({ ...started, operation: 'wiki', type: 'phase', phase: 'authoring' }), 'Writing Wiki pages with AI…');
  assert.equal(activityProgressMessage(phase('publishing')), 'Publishing…');
});
