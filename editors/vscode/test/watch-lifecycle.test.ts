import assert from 'node:assert/strict';
import test from 'node:test';
import { WatchLifecycle } from '../src/watch-lifecycle';

test('retains an exited generation when stop and replacement finish before observation', async () => {
  const lifecycle = new WatchLifecycle();
  const previous = lifecycle.beginStart('custom-output');
  lifecycle.markReady(previous);

  // This is the ordering the former E2E poll could not observe: both transient
  // phases complete synchronously before the test gets another event-loop turn.
  lifecycle.markStopping(previous);
  lifecycle.markExited(previous);
  const replacement = lifecycle.beginStart('default-output');
  lifecycle.markReady(replacement);

  const observed = await lifecycle.waitFor(
    (snapshot) => snapshot.phase === 'ready'
      && snapshot.lastExitedGeneration >= previous
      && snapshot.activeGeneration === replacement
      && snapshot.targetMatchesExpected === true,
    { description: 'a retained replacement transition', timeoutMs: 100, expectedTarget: 'default-output' },
  );

  assert.equal(observed.activeGeneration, previous + 1);
  assert.equal(observed.lastExitedGeneration, previous);
});

test('does not let a replacement own publication before the old generation exits', () => {
  const lifecycle = new WatchLifecycle();
  const previous = lifecycle.beginStart('custom-output');
  lifecycle.markReady(previous);
  lifecycle.markStopping(previous);

  assert.throws(
    () => lifecycle.beginStart('default-output'),
    /while lifecycle phase is stopping/u,
  );

  lifecycle.markExited(previous);
  assert.equal(lifecycle.beginStart('default-output'), previous + 1);
});

test('waits on lifecycle transitions instead of scheduler polling', async () => {
  const lifecycle = new WatchLifecycle();
  const previous = lifecycle.beginStart('custom-output');
  lifecycle.markReady(previous);
  const replacementReady = lifecycle.waitFor(
    (snapshot) => snapshot.phase === 'ready'
      && snapshot.lastExitedGeneration === previous
      && snapshot.activeGeneration === previous + 1,
    { description: 'an event-driven replacement', timeoutMs: 100, expectedTarget: 'default-output' },
  );

  lifecycle.markStopping(previous);
  lifecycle.markExited(previous);
  const replacement = lifecycle.beginStart('default-output');
  lifecycle.markReady(replacement);

  assert.equal((await replacementReady).targetMatchesExpected, true);
});
