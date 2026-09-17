import assert from 'node:assert/strict';
import test from 'node:test';
import { BuildPhaseEvent, BuildProgressRun, BuildStartedEvent } from '../src/build-progress';

const started: BuildStartedEvent = {
  schema_version: 1,
  run_nonce: '0123456789abcdef0123456789abcdef',
  type: 'started',
  operation: 'update',
  mode: 'adaptive',
};

function phase(name: BuildPhaseEvent['phase'], processed?: number, total?: number): BuildPhaseEvent {
  return {
    schema_version: 1,
    run_nonce: started.run_nonce,
    type: 'phase',
    operation: 'update',
    phase: name,
    ...(processed === undefined ? {} : { processed, total }),
  };
}

test('a graph phase can discover its counter after its initial announcement', () => {
  const run = new BuildProgressRun('update');
  assert.equal(run.accept(started), true);
  assert.equal(run.accept(phase('building')), true);
  assert.equal(run.accept(phase('building', 0, 8)), true);
  assert.equal(run.accept(phase('building', 4, 8)), true);
  assert.equal(run.accept(phase('building', 3, 8)), false, 'a known counter must not regress');
  assert.equal(run.accept(phase('building', 5, 10)), false, 'a known total must remain stable');
  assert.equal(run.accept(phase('building', 8, 8)), true);
  assert.equal(run.accept(phase('clustering')), true);
  assert.equal(run.accept(phase('publishing')), true);
});

test('full and adaptive graph runs advance through graph construction to publication', () => {
  for (const mode of ['full', 'adaptive'] as const) {
    const run = new BuildProgressRun('update');
    assert.equal(run.accept({ ...started, mode }), true);
    for (const event of [
      phase('scanning'),
      phase('extracting', 0, 3),
      phase('extracting', 3, 3),
      phase('building'),
      phase('building', 0, 3),
      phase('building', 3, 3),
      phase('merging_nodes'),
      phase('resolving_edges'),
      phase('deduplicating'),
      phase('clustering'),
      phase('publishing'),
    ]) {
      assert.equal(run.accept(event), true, `${mode} stopped at ${JSON.stringify(event)}`);
    }
    assert.equal(run.accept(phase('extracting', 3, 3)), false, 'completed extraction cannot replace publication');
  }
});
