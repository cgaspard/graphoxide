import assert from 'node:assert/strict';
import test from 'node:test';
import {
  AUTOMATIC_UPDATES_PAUSED,
  BoundedTextTail,
  compactCommandDiagnostic,
  compactGuidedError,
  ERROR_DIAGNOSTIC_LIMIT,
} from '../src/process-output';

test('selects one actionable error instead of accumulated lock-wait stderr', () => {
  const diagnostic = compactCommandDiagnostic(
    '[graphoxide] waiting for the rebuild lock at /workspace/out\n'
      + 'Error: isolated retained extraction output exceeds its 39514280-byte budget at schema.graphql\n',
    '',
    1,
  );
  assert.equal(
    diagnostic,
    'Error: isolated retained extraction output exceeds its 39514280-byte budget at schema.graphql',
  );
});

test('bounds diagnostics and retained stderr tails', () => {
  const tail = new BoundedTextTail(16);
  tail.append('first-line\n');
  tail.append('second-line\n');
  assert.equal(tail.value().length, 16);
  assert.match(tail.value(), /^…/u);
  assert.match(tail.value(), /second-line/u);

  const diagnostic = compactCommandDiagnostic(`Error: ${'x'.repeat(ERROR_DIAGNOSTIC_LIMIT * 2)}`, '', 1);
  assert.equal(diagnostic.length, ERROR_DIAGNOSTIC_LIMIT);
  assert.match(diagnostic, /…$/u);
});

test('falls back to an exit-code diagnostic when the process is silent', () => {
  assert.equal(compactCommandDiagnostic('', '', 9), 'Graphoxide exited with code 9.');
});

test('structural failure guidance truthfully exposes automatic suppression', () => {
  const diagnostic = compactGuidedError(
    new Error(`retained extraction output exceeds its budget ${'x'.repeat(ERROR_DIAGNOSTIC_LIMIT * 2)}`),
    AUTOMATIC_UPDATES_PAUSED,
  );
  assert.match(diagnostic, /retained extraction output exceeds its budget/u);
  assert.match(diagnostic, /Automatic graph updates are paused/u);
  assert.match(diagnostic, /retry Build, Update, or Full Rebuild/u);
  assert.ok(diagnostic.endsWith(AUTOMATIC_UPDATES_PAUSED));
  assert.ok(diagnostic.length <= ERROR_DIAGNOSTIC_LIMIT);
});
