import assert from 'node:assert/strict';
import { cpSync, mkdirSync, mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

import {
  BENCHMARK_SCENARIOS,
  assertIsolatedBenchmarkArgs,
  buildBenchmarkReport,
  describeFixture,
  describeScenario,
  materializeGeneratedScenario,
  mutateCopiedFixture,
  parseArguments,
  parseCliReport,
  parseRuntimeTelemetry,
  profileForScenario,
  summarize,
  summarizeSamples,
  validateCliReport,
  validateRuntimeTelemetry,
  verifyBuildArtifacts,
} from './benchmark-graph-build.mjs';

const repositoryRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const baselineFixture = path.join(repositoryRoot, 'parity', 'corpora', 'language-matrix');

test('parseArguments supplies the reproducible baseline defaults', () => {
  const options = parseArguments([], repositoryRoot);
  assert.equal(options.runs, 5);
  assert.equal(options.fixture, baselineFixture);
  assert.equal(options.fixtureExplicit, false);
  assert.equal(options.scenario, 'compat-language-matrix');
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

test('benchmark commands cannot opt into the legacy executor', () => {
  assert.doesNotThrow(() => assertIsolatedBenchmarkArgs(['extract', '.', '--force', '--json']));
  assert.throws(
    () => assertIsolatedBenchmarkArgs(['update', '.', '--json', '--legacy-executor']),
    /must not opt into --legacy-executor/,
  );
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
  assert.equal(options.fixtureExplicit, true);
  assert.equal(options.scenario, 'custom-fixture');
  assert.equal(options.help, true);
});

test('parseArguments accepts each deterministic built-in benchmark profile', () => {
  assert.deepEqual(BENCHMARK_SCENARIOS, [
    'compat-language-matrix',
    'many-small',
    'mixed-size',
    'structured-json',
    'cache-warm',
    'slow-io',
    'large-graph',
    'structured-containers',
    'idl-schema',
    'diagrams',
    'facility-models',
    'openusd-assets',
  ]);
  for (const scenario of BENCHMARK_SCENARIOS) {
    const options = parseArguments(['--scenario', scenario], repositoryRoot);
    assert.equal(options.scenario, scenario);
  }
  assert.throws(() => parseArguments(['--scenario', 'unknown']), /must be one of/);
  assert.throws(
    () => parseArguments(['--fixture', 'fixture', '--scenario', 'many-small'], repositoryRoot),
    /cannot be combined/,
  );
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

test('describeScenario labels the baseline without overstating custom-fixture coverage', () => {
  assert.equal(describeScenario(baselineFixture).name, 'compat-language-matrix');
  assert.equal(describeScenario(path.join(repositoryRoot, 'parity')).name, 'custom-fixture');
});

test('structured benchmark profiles materialize deterministic, selected fixture families', () => {
  const expectedFixtures = {
    'structured-json': {
      sha256: '357a299c41269f5b0d21f03efa7504af8bf92791b0f79ebec871cfbcf179ff08',
      file_count: 69,
      total_bytes: 10867,
    },
    'structured-containers': {
      sha256: '2de5faaddd332088601b4040c35cfb4b963b89950d27b3d4e0196228045ba14a',
      file_count: 10,
      total_bytes: 6829,
    },
    'idl-schema': {
      sha256: 'c6b6440bfec5218458e7ea81477f93033ec051d3732779695af8d225702d0667',
      file_count: 14,
      total_bytes: 990,
    },
    diagrams: {
      sha256: 'b4a32b2f0bb3529ef17196331f340464a26e836343a33d2c5ff7e9c838ecc314',
      file_count: 11,
      total_bytes: 733,
    },
    'facility-models': {
      sha256: '571d032c434aca0d28ab5ad9f0c93cf74d0ac2db7cd790fbcaa6665623b35737',
      file_count: 16,
      total_bytes: 926,
    },
    'openusd-assets': {
      sha256: '11e898183eb05fcca4df7c5d15e83aed3a13a09dac5c97c8daf9211630b821d8',
      file_count: 13,
      total_bytes: 818,
    },
  };

  for (const [scenario, expected] of Object.entries(expectedFixtures)) {
    const fixture = materializeGeneratedScenario(scenario);
    try {
      assert.deepEqual(describeFixture(fixture), expected, scenario);
      assert.deepEqual(describeScenario(fixture, scenario), profileForScenario(scenario), scenario);
    } finally {
      rmSync(fixture, { recursive: true, force: false });
    }
  }
});

test('container and binary structured benchmark fixtures retain their required wire signatures', () => {
  const containers = materializeGeneratedScenario('structured-containers');
  const assets = materializeGeneratedScenario('openusd-assets');
  try {
    assert.deepEqual(readFileSync(path.join(containers, 'archives', 'structured.zip')).subarray(0, 4), Buffer.from('PK\x03\x04'));
    assert.deepEqual(readFileSync(path.join(containers, 'archives', 'structured.tar')).subarray(257, 263), Buffer.from('ustar\0'));
    assert.deepEqual(readFileSync(path.join(containers, 'archives', 'structured.tar.gz')).subarray(0, 2), Buffer.from([0x1f, 0x8b]));
    assert.deepEqual(readFileSync(path.join(containers, 'architecture.svgz')).subarray(0, 2), Buffer.from([0x1f, 0x8b]));
    assert.deepEqual(readFileSync(path.join(assets, 'scene.usdc')).subarray(0, 8), Buffer.from('PXR-USDC'));
    assert.deepEqual(readFileSync(path.join(assets, 'scene.usdz')).subarray(0, 4), Buffer.from('PK\x03\x04'));
    assert.deepEqual(readFileSync(path.join(assets, 'asset.glb')).subarray(0, 4), Buffer.from('glTF'));
  } finally {
    rmSync(containers, { recursive: true, force: false });
    rmSync(assets, { recursive: true, force: false });
  }
});

test('buildBenchmarkReport preserves a generated profile, fixture digest, runtime-sidecar convention, and observations', () => {
  const options = parseArguments(['--runs', '2', '--scenario', 'idl-schema'], repositoryRoot);
  const materialized = {
    fixture: '/tmp/generated-idl-schema',
    generated: true,
    scenario: profileForScenario('idl-schema'),
  };
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
  const report = buildBenchmarkReport({
    options,
    materialized,
    fixture: { sha256: 'a'.repeat(64), file_count: 14, total_bytes: 990 },
    samples,
    metadata: { test: true },
    generatedAt: '2026-08-04T00:00:00.000Z',
  });

  assert.equal(report.generated_at, '2026-08-04T00:00:00.000Z');
  assert.deepEqual(report.scenario, profileForScenario('idl-schema'));
  assert.deepEqual(report.fixture, {
    path: '<generated:idl-schema>',
    generated: true,
    sha256: 'a'.repeat(64),
    file_count: 14,
    total_bytes: 990,
    mutation: {
      preferred_path: 'jvm/app/Runner.java',
      method: 'append one deterministic source declaration in the temporary copy',
    },
  });
  assert.deepEqual(report.commands.full_build, ['graphoxide', 'extract', '.', '--force', '--json']);
  assert.deepEqual(report.commands.incremental_update, ['graphoxide', 'update', '.', '--json']);
  assert.match(report.commands.runtime_telemetry, /--runtime-report/);
  assert.equal(report.summary.full_build.external_wall_ms.median, 16);
  assert.ok(report.notes.some((note) => note.includes('not performance targets')));
  assert.throws(
    () => buildBenchmarkReport({ ...{ options, materialized, samples: samples.slice(0, 1), metadata: {} }, fixture: {} }),
    /sample count/,
  );
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

test('runtime telemetry has an additive stable sidecar shape', () => {
  const telemetry = parseRuntimeTelemetry(
    JSON.stringify({
      schema_version: 1,
      build: {
        operation: 'extract',
        mode: 'full',
        status: 'rebuilt',
        elapsed_ms: 12,
        files: { processed: 33 },
      },
      runtime: {
        execution_model: 'isolated',
        io_backend: 'threaded',
        io_backend_request: 'auto',
        io_backend_fallback: null,
        memory_budget_bytes: 536870912,
        io_workers: 2,
        compute_workers: 3,
        read_batch_bytes: 262144,
        cache_partitions: null,
        admission: {
          admitted_requests: 33,
          effective_io_workers: 2,
          effective_compute_workers: 3,
          effective_read_batch_bytes: 262144,
          io_pool_bytes_per_worker: 53687091,
          io_buffers_bytes: 107374182,
          ready_inputs_bytes: 107374182,
          cpu_arenas_bytes: 107374182,
          cache_and_runs_bytes: 134217728,
          query_reserve_bytes: 26843545,
          emergency_reserve_bytes: 26843547,
        },
      },
      cache: {
        enabled: true,
        metadata_hits: 0,
        runtime_hits: 0,
        legacy_hits: 0,
        misses: 33,
        bypasses: 0,
        stale_or_corrupt: 0,
        probe_failures: 0,
        payload_reads_avoided: 0,
        parses_avoided: 0,
        stores: 33,
        already_present: 0,
        store_failures: 0,
      },
      simd: {
        architecture: 'x86_64',
        detected_features: ['avx2'],
        enabled_kernels: [],
      },
    }),
  );

  assert.equal(
    validateRuntimeTelemetry(telemetry, {
      operation: 'extract',
      mode: 'full',
      status: 'rebuilt',
    }),
    telemetry,
  );
  const missingWorkerCount = {
    ...telemetry,
    runtime: { ...telemetry.runtime },
  };
  delete missingWorkerCount.runtime.io_workers;
  assert.throws(
    () =>
      validateRuntimeTelemetry(missingWorkerCount, {
        operation: 'extract',
        mode: 'full',
        status: 'rebuilt',
      }),
    /missing io_workers/,
  );
  assert.throws(
    () =>
      validateRuntimeTelemetry(
        { ...telemetry, runtime: { ...telemetry.runtime, execution_model: 'legacy' } },
        { operation: 'extract', mode: 'full', status: 'rebuilt' },
      ),
    /execution_model=isolated/,
  );
  assert.throws(
    () =>
      validateRuntimeTelemetry(
        {
          ...telemetry,
          runtime: {
            ...telemetry.runtime,
            admission: { ...telemetry.runtime.admission, effective_io_workers: 2 },
            io_workers: 1,
          },
        },
        { operation: 'extract', mode: 'full', status: 'rebuilt' },
      ),
    /exceeds its configured bounds/,
  );
  assert.throws(
    () =>
      validateRuntimeTelemetry(
        { ...telemetry, cache: { ...telemetry.cache, parses_avoided: 1 } },
        { operation: 'extract', mode: 'full', status: 'rebuilt' },
      ),
    /parses_avoided does not match/,
  );
  assert.throws(
    () =>
      validateRuntimeTelemetry(
        { ...telemetry, cache: { ...telemetry.cache, payload_reads_avoided: 1 } },
        { operation: 'extract', mode: 'full', status: 'rebuilt' },
      ),
    /payload_reads_avoided does not match/,
  );
});

test('verifyBuildArtifacts ties graph bytes and counts to the CLI build report', () => {
  const temporary = mkdtempSync(path.join(os.tmpdir(), 'graphoxide-benchmark-artifacts-'));
  try {
    const output = path.join(temporary, 'graphoxide-out');
    mkdirSync(output, { recursive: true });
    writeFileSync(
      path.join(output, 'graph.json'),
      JSON.stringify({ nodes: [{ id: 'a' }], links: [{ source: 'a', target: 'a' }] }),
    );
    writeFileSync(path.join(output, 'manifest.json'), JSON.stringify({ 'src/lib.rs': {} }));
    const evidence = verifyBuildArtifacts(
      temporary,
      { graph: { nodes: 1, edges: 1 } },
      'fixture',
    );
    assert.equal(evidence.nodes, 1);
    assert.equal(evidence.edges, 1);
    assert.match(evidence.graph_sha256, /^[a-f0-9]{64}$/);
    assert.throws(
      () => verifyBuildArtifacts(temporary, { graph: { nodes: 2, edges: 1 } }, 'fixture'),
      /node count differs/,
    );
  } finally {
    rmSync(temporary, { recursive: true, force: false });
  }
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
