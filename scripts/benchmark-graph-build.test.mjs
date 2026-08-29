import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import {
  chmodSync,
  closeSync,
  constants as fsConstants,
  cpSync,
  existsSync,
  fstatSync,
  linkSync,
  mkdirSync,
  mkdtempSync,
  openSync,
  readFileSync,
  readSync,
  realpathSync,
  renameSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from 'node:fs';
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
const TEST_FILE_MAX_BYTES = 64 * 1024 * 1024;

function snapshotDescriptor(descriptor, label) {
  const opened = fstatSync(descriptor);
  assert.ok(opened.isFile(), `${label} must be a regular file`);
  assert.equal(opened.nlink, 1, `${label} must be single-link`);
  assert.ok(Number.isSafeInteger(opened.size) && opened.size <= TEST_FILE_MAX_BYTES);
  const bytes = Buffer.alloc(opened.size);
  let position = 0;
  while (position < bytes.length) {
    const count = readSync(descriptor, bytes, position, bytes.length - position, position);
    assert.notEqual(count, 0, `${label} ended before its descriptor size`);
    position += count;
  }
  const after = fstatSync(descriptor);
  assert.deepEqual([after.dev, after.ino, after.size, after.nlink], [
    opened.dev,
    opened.ino,
    opened.size,
    opened.nlink,
  ]);
  return { bytes, metadata: after };
}

function openReadDescriptor(file) {
  return openSync(file, fsConstants.O_RDONLY | (fsConstants.O_NOFOLLOW ?? 0));
}

function zipMemberNames(bytes) {
  const names = [];
  for (
    let offset = bytes.indexOf(Buffer.from('PK\x01\x02', 'binary'));
    offset >= 0 && offset + 46 <= bytes.length && bytes.readUInt32LE(offset) === 0x02014b50;
  ) {
    const nameLength = bytes.readUInt16LE(offset + 28);
    const extraLength = bytes.readUInt16LE(offset + 30);
    const commentLength = bytes.readUInt16LE(offset + 32);
    names.push(bytes.subarray(offset + 46, offset + 46 + nameLength).toString('utf8'));
    offset += 46 + nameLength + extraLength + commentLength;
  }
  return names;
}

function assertMinimalPdf(bytes) {
  const text = bytes.toString('ascii');
  assert.match(text, /^%PDF-1\.4\n/);
  assert.match(text, /1 0 obj\n<< \/Type \/Catalog \/Pages 2 0 R >>\nendobj/);
  assert.match(text, /trailer\n<< \/Size 4 \/Root 1 0 R >>\nstartxref\n\d+\n%%EOF\n$/);
  const startxref = Number(text.match(/startxref\n(\d+)\n%%EOF\n$/)?.[1]);
  assert.equal(text.slice(startxref, startxref + 9), 'xref\n0 4\n');
}

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
    'catalog-wiki',
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

test('catalog/wiki fixture keeps mixed documents, archive-only inputs, ignored inputs, and a catalog annotation separate', () => {
  const fixture = materializeGeneratedScenario('catalog-wiki');
  try {
    const runbook = readFileSync(path.join(fixture, 'docs', 'runbook.md'), 'utf8');
    const catalog = JSON.parse(readFileSync(path.join(fixture, 'provenance', 'catalog.json'), 'utf8'));
    assert.match(runbook, /^---\ntitle: Runbook\nsources:\n  - source-one#capture-active\n  - source-one#capture-history\n---\n/);
    assert.deepEqual(catalog, {
      version: 2,
      sources: [
        {
          source_id: 'source-one',
          source_system: 'sharepoint',
          url: 'https://example.invalid/site/page',
          location: 'Site/Library/Folder/Page',
          active_capture_id: 'capture-active',
        },
      ],
      captures: [
        {
          source_id: 'source-one',
          capture_id: 'capture-active',
          source_path: 'raw/active.md',
          sha256: createHash('sha256').update(readFileSync(path.join(fixture, 'raw', 'active.md'))).digest('hex'),
          captured_at: '2026-08-24T12:00:00Z',
          accessed_at: '2026-08-24T12:00:00Z',
          updated_at: '2026-08-23T20:00:00Z',
          representation: 'markdown',
        },
        {
          source_id: 'source-one',
          capture_id: 'capture-history',
          source_path: 'raw/history.md',
          sha256: 'b'.repeat(64),
          captured_at: '2026-08-23T12:00:00Z',
          accessed_at: '2026-08-23T12:00:00Z',
          updated_at: '2026-08-23T12:00:00Z',
          representation: 'markdown',
        },
      ],
    });
    assert.equal(existsSync(path.join(fixture, 'raw', 'history.md')), false);
    assert.match(readFileSync(path.join(fixture, 'wiki.json'), 'utf8'), /"output":"llms\.txt"/);
    assert.match(readFileSync(path.join(fixture, 'metadata', 'services.json'), 'utf8'), /catalog-only annotation/);
    assert.match(readFileSync(path.join(fixture, 'metadata', 'services.yaml'), 'utf8'), /service:/);
    assertMinimalPdf(readFileSync(path.join(fixture, 'documents', 'guide.pdf')));
    assert.deepEqual(readFileSync(path.join(fixture, 'documents', 'catalog.docx')).subarray(0, 4), Buffer.from('PK\x03\x04'));
    const archive = readFileSync(path.join(fixture, 'archives', 'wiki-only.zip'));
    assert.deepEqual(zipMemberNames(archive), ['members/wiki-only.md']);
    assert.match(readFileSync(path.join(fixture, '.graphoxideignore'), 'utf8'), /^ignored\/\nprovenance\/\n$/);
    assert.throws(() => readFileSync(path.join(fixture, 'members', 'wiki-only.md')), /ENOENT/);
    assert.equal(readFileSync(path.join(fixture, 'ignored', 'private.md'), 'utf8'), 'Ignored by fixture policy.\n');
    assert.equal(describeFixture(fixture).file_count, 15);
    assert.equal(readFileSync(path.join(fixture, '.env'), 'utf8'), 'TOKEN=not-for-indexing\n');
    assert.match(readFileSync(path.join(fixture, 'metadata', 'malformed.json'), 'utf8'), /not valid JSON/);
    assert.deepEqual(profileForScenario('catalog-wiki').format_families, [
      'markdown',
      'structured-json',
      'configuration',
      'office-document',
      'pdf',
      'container',
      'catalog-metadata',
    ]);
  } finally {
    rmSync(fixture, { recursive: true, force: false });
  }
});

test('catalog/wiki V2 qualification accepts only active captures with a 4GB graph cap', {
  skip: !process.env.GRAPHOXIDE_QUALIFICATION_BINARY,
}, () => {
  const result = spawnSync(
    process.execPath,
    [
      path.join(repositoryRoot, 'scripts', 'benchmark-graph-build.mjs'),
      '--runs',
      '1',
      '--scenario',
      'catalog-wiki',
      '--binary',
      process.env.GRAPHOXIDE_QUALIFICATION_BINARY,
    ],
    {
      cwd: repositoryRoot,
      encoding: 'utf8',
      env: { ...process.env, GRAPHOXIDE_MAX_GRAPH_BYTES: '4GB' },
    },
  );
  assert.equal(result.status, 0, `${result.error?.message ?? ''}\n${result.stderr}\n${result.stdout}`);
  const sample = JSON.parse(result.stdout).samples[0];
  assert.equal(sample.incremental_update.cli_report.files.changed, 1);
  assert.equal(sample.incremental_update.cli_report.files.processed, 1);
  assert.equal(sample.catalog_only.runtime_telemetry.work.parses, 0);
  assert.equal(sample.catalog_only.runtime_telemetry.cache.misses, 0);
  assert.equal(
    sample.catalog_only.artifacts.manifest_sha256,
    sample.incremental_update.artifacts.manifest_sha256,
  );
  assert.deepEqual(sample.catalog_only.cache_tree.before, sample.catalog_only.cache_tree.after);
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

test('buildBenchmarkReport records the catalog/wiki cold, warm, source-incremental, and catalog-only workflow', () => {
  const options = parseArguments(['--runs', '1', '--scenario', 'catalog-wiki'], repositoryRoot);
  const materialized = {
    fixture: '/tmp/generated-catalog-wiki',
    generated: true,
    scenario: profileForScenario('catalog-wiki'),
  };
  const report = buildBenchmarkReport({
    options,
    materialized,
    fixture: { sha256: 'b'.repeat(64), file_count: 14, total_bytes: 990 },
    samples: [
      {
        full_build: { external_wall_ms: 12, reported_elapsed_ms: 10 },
        warm_build: { external_wall_ms: 8, reported_elapsed_ms: 6 },
        incremental_update: { external_wall_ms: 7, reported_elapsed_ms: 5 },
        catalog_only: { external_wall_ms: 4, reported_elapsed_ms: 3 },
        wiki_index: { external_wall_ms: 2 },
        wiki_check: { external_wall_ms: 1 },
      },
    ],
    metadata: { test: true },
  });

  assert.deepEqual(report.commands.catalog_only, [
    'graphoxide',
    'index',
    '.',
    '--catalog',
    'provenance',
    '--json',
  ]);
  assert.deepEqual(report.commands.full_build, report.commands.catalog_only);
  assert.deepEqual(report.commands.warm_build, report.commands.catalog_only);
  assert.deepEqual(report.commands.incremental_update, report.commands.catalog_only);
  assert.deepEqual(report.fixture.mutation, {
    preferred_path: 'src/benchmark.rs',
    method: 'append one deterministic source declaration in the temporary copy',
  });
  assert.deepEqual(report.commands.wiki_index, ['graphoxide', 'wiki', 'index', '.', '--config', 'wiki.json']);
  assert.deepEqual(report.commands.wiki_check, [
    'graphoxide',
    'wiki',
    'check',
    '.',
    '--config',
    'wiki.json',
    '--catalog',
    'provenance',
    '--graph',
    'graphoxide-out/graph.json',
  ]);
  assert.equal(report.summary.warm_build.reported_elapsed_ms.median, 6);
  assert.equal(report.summary.catalog_only.external_wall_ms.median, 4);
  assert.ok(report.notes.some((note) => note.includes('catalog-only')));
  assert.ok(!report.notes.some((note) => note.includes('remain deferred')));
});

test('catalog/wiki benchmark enforces fixture coverage and pins an explicit binary without Cargo', () => {
  const parent = mkdtempSync(path.join(os.tmpdir(), 'graphoxide-benchmark-wrapper-'));
  const binary = path.join(parent, 'graphoxide');
  const cargo = path.join(parent, 'cargo');
  const record = path.join(parent, 'commands.jsonl');
  const cargoCalled = path.join(parent, 'cargo-called');
  try {
    writeFileSync(
      binary,
      `#!${process.execPath}
const { appendFileSync, existsSync, mkdirSync, readFileSync, renameSync, writeFileSync } = require('node:fs');
const { createHash } = require('node:crypto');
const path = require('node:path');
const args = process.argv.slice(2);
appendFileSync(process.env.CATALOG_WRAPPER_RECORD, JSON.stringify(args) + '\\n');
if (args[0] === '--version') {
  process.stdout.write('graphoxide recording wrapper\\n');
  process.exit(0);
}
if (args[0] === 'wiki') {
  if (args[1] === 'index') {
    writeFileSync(path.join(process.cwd(), 'llms.txt'), '# Runbook\\n');
    if (process.env.CATALOG_WRAPPER_REPLACEMENT) {
      renameSync(process.env.CATALOG_WRAPPER_REPLACEMENT, process.argv[1]);
    }
    process.stdout.write('Indexed 1 wiki pages into ' + path.join(process.cwd(), 'llms.txt') + '\\n');
    process.exit(0);
  }
  if (args[1] === 'check') {
    const source = JSON.parse(readFileSync(path.join(process.cwd(), 'provenance', 'catalog.json'), 'utf8')).sources[0];
    if (source.location.endsWith('stale-check.md')) {
      process.exit(92);
    }
    process.stdout.write('Checked 1 wiki pages\\n');
    process.exit(0);
  }
}
if (args[0] !== 'index') process.exit(90);
const output = path.join(process.cwd(), 'graphoxide-out');
const graph = path.join(output, 'graph.json');
const manifest = path.join(output, 'manifest.json');
const cache = path.join(output, 'cache', 'runtime-v2', 'cache-entry');
const runtimePath = args[args.indexOf('--runtime-report') + 1];
if (!runtimePath) process.exit(91);
mkdirSync(output, { recursive: true });
mkdirSync(path.dirname(cache), { recursive: true });
if (!existsSync(cache)) writeFileSync(cache, 'stable extraction cache\\n');
if (process.env.CATALOG_WRAPPER_BAD_OUTCOME === 'cache' && runtimePath.endsWith('catalog-only.json')) {
  writeFileSync(cache, 'catalog-only cache mutation\\n');
}
const sourceHash = createHash('sha256').update(readFileSync(path.join(process.cwd(), 'raw', 'active.md'))).digest('hex');
const prior = existsSync(manifest) ? JSON.parse(readFileSync(manifest, 'utf8')).source_hash : null;
const changed = prior === null ? 10 : prior === sourceHash ? 0 : 1;
const mode = existsSync(graph) ? 'incremental' : 'full';
const source = JSON.parse(readFileSync(path.join(process.cwd(), 'provenance', 'catalog.json'), 'utf8')).sources[0];
const capture = JSON.parse(readFileSync(path.join(process.cwd(), 'provenance', 'catalog.json'), 'utf8')).captures.find((entry) => entry.capture_id === source.active_capture_id);
const catalog = { ...source, ...capture };
delete catalog.captures;
const build = { schema_version: 1, operation: 'index', mode, status: 'rebuilt', elapsed_ms: 1, files: { changed, processed: changed }, graph: { nodes: 2, edges: 0 } };
const cacheHit = changed === 0 ? 2 : 0;
const sourceRead = changed === 0 ? 0 : 2;
const badOutcome = process.env.CATALOG_WRAPPER_BAD_OUTCOME;
const nodes = [
  { source_file: badOutcome === 'misplaced-catalog' ? 'metadata/services.json' : 'raw/active.md', catalog },
  { source_file: badOutcome === 'misplaced-catalog' ? 'metadata/services.json' : 'raw/active.md', label: 'Full Derived Knowledge', catalog },
  { source_file: 'src/benchmark.rs', source_hash: sourceHash },
  ...(badOutcome === 'malformed' ? [] : [{ source_file: 'metadata/malformed.json' }]),
  ...(badOutcome === 'archive' ? [] : [{ source_file: 'archives/wiki-only.zip!/members/wiki-only.md' }]),
  ...(badOutcome === 'sensitive' ? [{ source_file: '.env' }] : []),
];
const runtime = {
  schema_version: 2,
  build,
  runtime: {
    execution_model: 'isolated', io_backend: 'threaded', io_backend_request: 'auto', io_backend_fallback: null,
    memory_budget_bytes: 1, io_workers: 1, compute_workers: 1, read_batch_bytes: 1, cache_partitions: 1,
    admission: { admitted_requests: 2, effective_io_workers: 1, effective_compute_workers: 1, effective_read_batch_bytes: 1, io_pool_bytes_per_worker: 1, io_buffers_bytes: 0, ready_inputs_bytes: 0, cpu_arenas_bytes: 0, cache_and_runs_bytes: 0, query_reserve_bytes: 0, emergency_reserve_bytes: 0 },
  },
  io: { sources_selected: 2, source_bytes_selected: 2, sources_read: sourceRead, source_bytes_read: sourceRead, sources_delivered: sourceRead, source_bytes_delivered: sourceRead, source_bytes_avoided: 2 - sourceRead, read_failures: 0, peak_ready_bytes: 0, peak_ready_items: 0 },
  work: { parses: sourceRead },
  cache: { enabled: true, metadata_hits: cacheHit, runtime_hits: 0, legacy_hits: 0, misses: sourceRead, bypasses: 0, stale_or_corrupt: 0, probe_failures: 0, payload_reads_avoided: cacheHit, parses_avoided: cacheHit, stores: 0, already_present: 0, store_failures: 0, payload_bytes_read: 0, payload_bytes_written: 0, artifact_bytes_read: 0, artifact_bytes_written: 0, peak_in_flight_transfer_bytes: 0 },
  process: { peak_rss_bytes: 1, peak_rss_source: 'getrusage_maxrss_bytes' },
  simd: { architecture: 'test', detected_features: [], enabled_kernels: [] },
};
build.graph.nodes = nodes.length;
const coverage = [
  { path: '.env', status: badOutcome === 'sensitive' ? 'covered' : 'excluded_sensitive' },
  { path: 'archives/wiki-only.zip', status: badOutcome === 'archive' ? 'unsupported' : 'covered', format_id: 'zip-archive', declared_capability: 'structural_partial' },
  { path: 'metadata/malformed.json', status: badOutcome === 'malformed' ? 'unsupported' : 'covered', format_id: 'json', declared_capability: 'semantic_full' },
];
if (badOutcome === 'ignored') coverage.push({ path: 'ignored/private.md', status: 'covered' });
writeFileSync(graph, JSON.stringify({ nodes, links: [] }));
writeFileSync(manifest, JSON.stringify({ source_hash: sourceHash }));
writeFileSync(runtimePath, JSON.stringify(runtime));
writeFileSync(path.join(output, 'coverage.json'), JSON.stringify({ files: coverage }));
process.stdout.write(JSON.stringify({ schema_version: 1, build, coverage: { complete: true } }) + '\\n');
`,
    );
    writeFileSync(
      cargo,
      `#!${process.execPath}\nrequire('node:fs').writeFileSync(process.env.CARGO_CALLED, 'called'); process.exit(97);\n`,
    );
    chmodSync(binary, 0o755);
    chmodSync(cargo, 0o755);

    const result = spawnSync(
      process.execPath,
      [
        path.join(repositoryRoot, 'scripts', 'benchmark-graph-build.mjs'),
        '--runs',
        '1',
        '--scenario',
        'catalog-wiki',
        '--binary',
        binary,
      ],
      {
        cwd: repositoryRoot,
        encoding: 'utf8',
        env: {
          ...process.env,
          PATH: parent,
          CATALOG_WRAPPER_RECORD: record,
          CARGO_CALLED: cargoCalled,
        },
      },
    );
    assert.equal(result.status, 0, `${result.error?.message ?? ''}\n${result.stderr}\n${result.stdout}`);
    assert.equal(existsSync(cargoCalled), false, 'an explicit binary must bypass Cargo');
    const report = JSON.parse(result.stdout);
    assert.deepEqual(Object.keys(report.samples[0]).filter((key) => key.endsWith('build') || key.endsWith('only')).sort(), [
      'catalog_only',
      'full_build',
      'warm_build',
    ]);
    assert.equal(report.samples[0].incremental_update.cli_report.files.changed, 1);
    assert.equal(report.samples[0].catalog_only.runtime_telemetry.work.parses, 0);
    assert.equal(report.samples[0].catalog_only.runtime_telemetry.cache.misses, 0);
    assert.equal(
      report.samples[0].catalog_only.artifacts.manifest_sha256,
      report.samples[0].incremental_update.artifacts.manifest_sha256,
    );
    assert.notEqual(
      report.samples[0].catalog_only.artifacts.graph_sha256,
      report.samples[0].incremental_update.artifacts.graph_sha256,
    );
    assert.deepEqual(
      report.samples[0].catalog_only.cache_tree.before,
      report.samples[0].catalog_only.cache_tree.after,
      'catalog-only indexing must leave the extraction cache tree unchanged',
    );
    assert.equal(report.samples[0].catalog_only.cache_tree.before.file_count, 1);
    assert.equal(report.samples[0].wiki_index.external_wall_ms >= 0, true);
    assert.equal(report.samples[0].wiki_check.external_wall_ms >= 0, true);
    const commands = readFileSync(record, 'utf8').trim().split('\n').map(JSON.parse);
    assert.equal(commands.filter(([command]) => command === 'index').length, 4);
    assert.ok(commands.some((command) => command[0] === 'wiki' && command[1] === 'index'));
    assert.ok(commands.some((command) => command[0] === 'wiki' && command[1] === 'check'));

    for (const outcome of ['sensitive', 'ignored', 'malformed', 'archive', 'misplaced-catalog', 'cache']) {
      const invalid = spawnSync(
        process.execPath,
        [
          path.join(repositoryRoot, 'scripts', 'benchmark-graph-build.mjs'),
          '--runs',
          '1',
          '--scenario',
          'catalog-wiki',
          '--binary',
          binary,
        ],
        {
          cwd: repositoryRoot,
          encoding: 'utf8',
          env: {
            ...process.env,
            PATH: parent,
            CATALOG_WRAPPER_RECORD: record,
            CARGO_CALLED: cargoCalled,
            CATALOG_WRAPPER_BAD_OUTCOME: outcome,
          },
        },
      );
      assert.notEqual(invalid.status, 0, `${outcome} outcome must fail qualification`);
      assert.match(
        invalid.stderr,
        outcome === 'cache' ? /changed extraction cache tree/ : /catalog\/wiki/,
      );
    }

    const replacingBinary = path.join(parent, 'graphoxide-replacing');
    const replacement = path.join(parent, 'graphoxide-replacement');
    cpSync(binary, replacingBinary);
    writeFileSync(
      replacement,
      `#!${process.execPath}
const args = process.argv.slice(2);
if (args[0] === 'wiki' && args[1] === 'check') {
  process.stdout.write('Checked 1 wiki pages\\n');
  process.exit(0);
}
process.exit(98);
`,
    );
    chmodSync(replacement, 0o755);
    const replaced = spawnSync(
      process.execPath,
      [
        path.join(repositoryRoot, 'scripts', 'benchmark-graph-build.mjs'),
        '--runs',
        '1',
        '--scenario',
        'catalog-wiki',
        '--binary',
        replacingBinary,
      ],
      {
        cwd: repositoryRoot,
        encoding: 'utf8',
        env: {
          ...process.env,
          PATH: parent,
          CATALOG_WRAPPER_RECORD: record,
          CARGO_CALLED: cargoCalled,
          CATALOG_WRAPPER_REPLACEMENT: replacement,
        },
      },
    );
    assert.notEqual(replaced.status, 0, 'a replacement between wiki phases must fail qualification');
    assert.match(replaced.stderr, /Graphoxide binary changed after graphoxide wiki index/);
  } finally {
    rmSync(parent, { recursive: true, force: false });
  }
});

test('mutateCopiedFixture changes only the deterministic source in a copy', () => {
  const temporary = realpathSync(
    mkdtempSync(path.join(os.tmpdir(), 'graphoxide-benchmark-test-')),
  );
  const copy = path.join(temporary, 'fixture');
  const originalTarget = path.join(baselineFixture, 'jvm', 'app', 'Runner.java');
  let originalDescriptor;
  let copiedDescriptor;
  try {
    originalDescriptor = openReadDescriptor(originalTarget);
    const original = snapshotDescriptor(originalDescriptor, originalTarget).bytes.toString('utf8');
    cpSync(baselineFixture, copy, { recursive: true });
    const copiedTarget = path.join(copy, 'jvm', 'app', 'Runner.java');
    copiedDescriptor = openReadDescriptor(copiedTarget);
    const before = snapshotDescriptor(copiedDescriptor, copiedTarget);
    const mutation = mutateCopiedFixture(copy);
    const after = snapshotDescriptor(copiedDescriptor, copiedTarget);
    assert.equal(mutation.path, 'jvm/app/Runner.java');
    assert.notEqual(mutation.sha256_before, mutation.sha256_after);
    assert.match(
      after.bytes.toString('utf8'),
      /GraphoxideBenchmarkMutation/,
    );
    assert.ok(after.metadata.mtimeMs > before.metadata.mtimeMs);
    assert.equal(
      snapshotDescriptor(originalDescriptor, originalTarget).bytes.toString('utf8'),
      original,
    );
  } finally {
    if (copiedDescriptor !== undefined) closeSync(copiedDescriptor);
    if (originalDescriptor !== undefined) closeSync(originalDescriptor);
    rmSync(temporary, { recursive: true, force: false });
  }
});

test('mutateCopiedFixture opens the deterministic source fallback when the preferred path is absent', () => {
  const fixture = materializeGeneratedScenario('many-small');
  try {
    const mutation = mutateCopiedFixture(fixture);
    assert.equal(mutation.path, 'src/benchmark.rs');
    assert.notEqual(mutation.sha256_before, mutation.sha256_after);
  } finally {
    rmSync(fixture, { recursive: true, force: false });
  }
});

test('mutateCopiedFixture rejects hard-linked mutation targets', () => {
  const temporary = realpathSync(
    mkdtempSync(path.join(os.tmpdir(), 'graphoxide-benchmark-hardlink-')),
  );
  const project = path.join(temporary, 'fixture');
  const target = path.join(project, 'jvm', 'app', 'Runner.java');
  const sentinel = path.join(temporary, 'sentinel.java');
  const original = 'final class ExternalSentinel {}\n';
  try {
    mkdirSync(path.dirname(target), { recursive: true });
    writeFileSync(sentinel, original);
    linkSync(sentinel, target);
    assert.throws(() => mutateCopiedFixture(project), /single-link regular file/);
    assert.equal(readFileSync(sentinel, 'utf8'), original);
  } finally {
    rmSync(temporary, { recursive: true, force: false });
  }
});

test('mutateCopiedFixture rejects symlink mutation targets without changing the referent', {
  skip: process.platform === 'win32' && 'ordinary Windows users cannot create test symlinks',
}, () => {
  const temporary = realpathSync(
    mkdtempSync(path.join(os.tmpdir(), 'graphoxide-benchmark-symlink-')),
  );
  const project = path.join(temporary, 'fixture');
  const target = path.join(project, 'jvm', 'app', 'Runner.java');
  const sentinel = path.join(temporary, 'sentinel.java');
  const original = 'final class ExternalSentinel {}\n';
  try {
    mkdirSync(path.dirname(target), { recursive: true });
    writeFileSync(sentinel, original);
    symlinkSync(sentinel, target);
    assert.throws(
      () => mutateCopiedFixture(project),
      /ELOOP|escaped its canonical project|single-link regular file/,
    );
    assert.equal(readFileSync(sentinel, 'utf8'), original);
  } finally {
    rmSync(temporary, { recursive: true, force: false });
  }
});

test('mutateCopiedFixture fails closed when its pathname is replaced after opening', {
  skip: process.platform === 'win32' && 'Windows does not permit replacing this open test file',
}, () => {
  const temporary = realpathSync(
    mkdtempSync(path.join(os.tmpdir(), 'graphoxide-benchmark-swap-')),
  );
  const project = path.join(temporary, 'fixture');
  const target = path.join(project, 'jvm', 'app', 'Runner.java');
  const displaced = path.join(temporary, 'displaced.java');
  const attacker = path.join(temporary, 'attacker.java');
  const original = 'final class IntendedTarget {}\n';
  const attackerBytes = 'final class ReplacementTarget {}\n';
  try {
    mkdirSync(path.dirname(target), { recursive: true });
    writeFileSync(target, original);
    writeFileSync(attacker, attackerBytes);
    assert.throws(
      () => mutateCopiedFixture(project, {
        beforeMutation(openedPath) {
          assert.equal(openedPath, target);
          renameSync(target, displaced);
          renameSync(attacker, target);
        },
      }),
      /path changed before mutation/,
    );
    assert.equal(readFileSync(displaced, 'utf8'), original);
    assert.equal(readFileSync(target, 'utf8'), attackerBytes);
  } finally {
    rmSync(temporary, { recursive: true, force: false });
  }
});

test('mutateCopiedFixture rejects an ancestor redirected between selection and opening', {
  skip: process.platform === 'win32' && 'ordinary Windows users cannot create test symlinks',
}, () => {
  const temporary = realpathSync(
    mkdtempSync(path.join(os.tmpdir(), 'graphoxide-benchmark-ancestor-')),
  );
  const project = path.join(temporary, 'fixture');
  const targetParent = path.join(project, 'jvm', 'app');
  const target = path.join(targetParent, 'Runner.java');
  const displacedParent = path.join(temporary, 'displaced-app');
  const externalParent = path.join(temporary, 'external-app');
  const externalTarget = path.join(externalParent, 'Runner.java');
  const original = 'final class IntendedTarget {}\n';
  const attackerBytes = 'final class ExternalSentinel {}\n';
  try {
    mkdirSync(targetParent, { recursive: true });
    mkdirSync(externalParent);
    writeFileSync(target, original);
    writeFileSync(externalTarget, attackerBytes);
    assert.throws(
      () => mutateCopiedFixture(project, {
        beforeOpen(openedPath) {
          assert.equal(path.basename(openedPath), 'Runner.java');
          const openedParent = path.dirname(openedPath);
          renameSync(openedParent, displacedParent);
          symlinkSync(externalParent, openedParent, 'dir');
        },
      }),
      /benchmark mutation target/,
    );
    assert.equal(readFileSync(path.join(displacedParent, 'Runner.java'), 'utf8'), original);
    assert.equal(readFileSync(externalTarget, 'utf8'), attackerBytes);
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

test('parseCliReport normalizes the index build envelope for the shared artifact checks', () => {
  assert.deepEqual(
    parseCliReport(
      JSON.stringify({
        schema_version: 1,
        build: { operation: 'index', mode: 'full', status: 'rebuilt', elapsed_ms: 12 },
        coverage: { complete: true },
      }),
    ),
    { operation: 'index', mode: 'full', status: 'rebuilt', elapsed_ms: 12 },
  );
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
      schema_version: 2,
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
        cache_partitions: 16,
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
        payload_bytes_read: 0,
        payload_bytes_written: 3300,
        artifact_bytes_read: 0,
        artifact_bytes_written: 3600,
        peak_in_flight_transfer_bytes: 100,
      },
      io: {
        sources_selected: 33,
        source_bytes_selected: 3300,
        sources_read: 33,
        source_bytes_read: 3300,
        sources_delivered: 33,
        source_bytes_delivered: 3300,
        source_bytes_avoided: 0,
        read_failures: 0,
        peak_ready_bytes: 100,
        peak_ready_items: 2,
      },
      work: {
        parses: 33,
      },
      process: {
        peak_rss_bytes: 123456,
        peak_rss_source: 'getrusage_maxrss_kib',
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
