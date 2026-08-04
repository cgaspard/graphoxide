import assert from 'node:assert/strict';
import { cpSync, mkdtempSync, readFileSync, rmSync, statSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

import {
  describeFixture,
  mutateCopiedFixture,
  parseArguments,
  parseCliReport,
  summarize,
  summarizeSamples,
  validateCliReport,
} from './benchmark-graph-build.mjs';

const repositoryRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const baselineFixture = path.join(repositoryRoot, 'parity', 'corpora', 'language-matrix');

test('parseArguments supplies the reproducible baseline defaults', () => {
  const options = parseArguments([], repositoryRoot);
  assert.equal(options.runs, 5);
  assert.equal(options.fixture, baselineFixture);
  assert.equal(options.binaryExplicit, false);
  assert.equal(
    options.binary,
    path.join(
      repositoryRoot,
      'target',
      'release',
      process.platform === 'win32' ? 'graphoxide.exe' : 'graphoxide',
    ),
  );
  assert.equal(options.help, false);
});

test('parseArguments accepts split and inline values', () => {
  const cwd = path.join(repositoryRoot, 'scripts');
  const options = parseArguments(
    ['--runs=7', '--binary', '../bin/graphoxide', '--fixture=../fixture', '--help'],
    cwd,
  );
  assert.equal(options.runs, 7);
  assert.equal(options.binaryExplicit, true);
  assert.equal(options.binary, path.join(repositoryRoot, 'bin', 'graphoxide'));
  assert.equal(options.fixture, path.join(repositoryRoot, 'fixture'));
  assert.equal(options.help, true);
});

test('parseArguments rejects unbounded or malformed runs', () => {
  assert.throws(() => parseArguments(['--runs', '0']), /between 1 and 100/);
  assert.throws(() => parseArguments(['--runs=101']), /between 1 and 100/);
  assert.throws(() => parseArguments(['--runs=1.5']), /positive integer/);
  assert.throws(() => parseArguments(['--unknown']), /unknown option/);
});

test('describeFixture matches the parity-pinned language matrix digest', () => {
  assert.deepEqual(describeFixture(baselineFixture), {
    sha256: '1b0e49b8bbac8a7414a38bb36b6e52b4259e66d04cbc5b2e5663e93a88adf0ef',
    file_count: 33,
    total_bytes: 7366,
  });
});

test('mutateCopiedFixture changes only the deterministic source in a copy', () => {
  const temporary = mkdtempSync(path.join(os.tmpdir(), 'graphoxide-benchmark-test-'));
  const copy = path.join(temporary, 'fixture');
  const originalTarget = path.join(baselineFixture, 'jvm', 'app', 'Runner.java');
  const original = readFileSync(originalTarget, 'utf8');
  try {
    cpSync(baselineFixture, copy, { recursive: true });
    const copiedTarget = path.join(copy, 'jvm', 'app', 'Runner.java');
    const mtimeBefore = statSync(copiedTarget).mtimeMs;
    const mutation = mutateCopiedFixture(copy);
    assert.equal(mutation.path, 'jvm/app/Runner.java');
    assert.notEqual(mutation.sha256_before, mutation.sha256_after);
    assert.match(
      readFileSync(copiedTarget, 'utf8'),
      /GraphoxideBenchmarkMutation/,
    );
    assert.ok(statSync(copiedTarget).mtimeMs > mtimeBefore);
    assert.equal(readFileSync(originalTarget, 'utf8'), original);
  } finally {
    rmSync(temporary, { recursive: true, force: false });
  }
});

test('parseCliReport requires one JSON object with elapsed_ms', () => {
  assert.deepEqual(parseCliReport('{"elapsed_ms":12.5,"status":"rebuilt"}'), {
    elapsed_ms: 12.5,
    status: 'rebuilt',
  });
  assert.throws(() => parseCliReport(''), /emitted no JSON/);
  assert.throws(() => parseCliReport('progress\n{"elapsed_ms":1}'), /invalid JSON/);
  assert.throws(() => parseCliReport('[]'), /must be an object/);
  assert.throws(() => parseCliReport('{"elapsed_ms":-1}'), /finite non-negative elapsed_ms/);
  assert.throws(() => parseCliReport('{"elapsed_ms":"1"}'), /finite non-negative elapsed_ms/);
});

test('validateCliReport rejects the wrong build path and no-op incremental updates', () => {
  const full = {
    operation: 'extract',
    mode: 'full',
    status: 'rebuilt',
    files: { processed: 33, changed: 33 },
  };
  assert.equal(
    validateCliReport(full, { operation: 'extract', mode: 'full', status: 'rebuilt' }),
    full,
  );
  assert.throws(
    () =>
      validateCliReport(full, {
        operation: 'extract',
        mode: 'incremental',
        status: 'rebuilt',
      }),
    /mode=incremental/,
  );

  const noOpUpdate = {
    operation: 'update',
    mode: 'incremental',
    status: 'unchanged',
    files: { processed: 0, changed: 0 },
  };
  assert.throws(
    () =>
      validateCliReport(noOpUpdate, {
        operation: 'update',
        mode: 'incremental',
        status: 'rebuilt',
        changed: 1,
        processed: 1,
      }),
    /status=rebuilt/,
  );
});

test('summarize reports min, median, and max without thresholds', () => {
  assert.deepEqual(summarize([9, 1, 4]), { min: 1, median: 4, max: 9 });
  assert.deepEqual(summarize([8, 2, 4, 6]), { min: 2, median: 5, max: 8 });
  assert.throws(() => summarize([]), /empty sample set/);
  assert.throws(() => summarize([1, Number.NaN]), /finite non-negative/);
});

test('summarizeSamples keeps CLI and external timing domains separate', () => {
  const samples = [
    {
      full_build: { external_wall_ms: 12, reported_elapsed_ms: 10 },
      incremental_update: { external_wall_ms: 5, reported_elapsed_ms: 3 },
    },
    {
      full_build: { external_wall_ms: 20, reported_elapsed_ms: 18 },
      incremental_update: { external_wall_ms: 9, reported_elapsed_ms: 7 },
    },
  ];
  assert.deepEqual(summarizeSamples(samples), {
    full_build: {
      external_wall_ms: { min: 12, median: 16, max: 20 },
      reported_elapsed_ms: { min: 10, median: 14, max: 18 },
    },
    incremental_update: {
      external_wall_ms: { min: 5, median: 7, max: 9 },
      reported_elapsed_ms: { min: 3, median: 5, max: 7 },
    },
  });
});
