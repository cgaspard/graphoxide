import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import {
  chmodSync,
  closeSync,
  constants as fsConstants,
  cpSync,
  fstatSync,
  linkSync,
  lstatSync,
  mkdirSync,
  mkdtempSync,
  openSync,
  readFileSync,
  readSync,
  realpathSync,
  rmSync,
  symlinkSync,
  unlinkSync,
  writeFileSync,
} from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

import {
  COMMAND_OUTPUT_MAX_BYTES,
  LARGE_CORPUS_BYTES,
  LARGE_FILE_BYTES,
  LARGE_FILE_COUNT,
  generateLargeFile,
  loadCorpusProfile,
  chargeEvidence,
  materializeLargeCorpus,
  materializeContentAddressedCorpus,
  mutateCorpus,
  parseArguments,
  readBoundedRegularFile,
  requireLargeGenerationReserve,
  runCaptured,
  validatePortablePath,
  validatePortablePathSequence,
  validateCatalogClosure,
  validateIndexStdout,
  validateRuntimeTelemetryV2,
  writeReportAtomic,
} from './qualify-universal-indexing.mjs';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const catalog = path.join(root, 'benchmarks', 'universal', 'catalog.json');
const TEST_SNAPSHOT_MAX_BYTES = 64 * 1024 * 1024;

function temporary(name) {
  return realpathSync(mkdtempSync(path.join(os.tmpdir(), `graphoxide-qualification-${name}-`)));
}

function sha256(bytes) {
  return createHash('sha256').update(bytes).digest('hex');
}

function snapshotDescriptor(
  descriptor,
  label,
  { maximum = TEST_SNAPSHOT_MAX_BYTES, length = null } = {},
) {
  const opened = fstatSync(descriptor);
  assert.ok(opened.isFile(), `${label} must be a regular file`);
  assert.ok(Number.isSafeInteger(opened.size) && opened.size >= 0 && opened.size <= maximum);
  const byteCount = length ?? opened.size;
  assert.ok(Number.isSafeInteger(byteCount) && byteCount >= 0 && byteCount <= opened.size);
  const bytes = Buffer.alloc(byteCount);
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

function descriptorSnapshot(file, options) {
  const descriptor = openSync(file, fsConstants.O_RDONLY | (fsConstants.O_NOFOLLOW ?? 0));
  try {
    return snapshotDescriptor(descriptor, file, options);
  } finally {
    closeSync(descriptor);
  }
}

function validRuntime(overrides = {}) {
  const runtime = {
    schema_version: 2,
    build: {
      schema_version: 1,
      operation: 'index',
      mode: 'full',
      status: 'rebuilt',
      elapsed_ms: 12,
      graph: { nodes: 3, edges: 2 },
    },
    runtime: {
      execution_model: 'isolated',
      io_backend: 'threaded',
      io_backend_request: 'auto',
      io_backend_fallback: null,
      memory_budget_bytes: 536870912,
      io_workers: 2,
      compute_workers: 2,
      read_batch_bytes: 262144,
      cache_partitions: 4,
      admission: {
        admitted_requests: 3,
        effective_io_workers: 2,
        effective_compute_workers: 2,
        effective_read_batch_bytes: 262144,
        io_pool_bytes_per_worker: 262144,
        io_buffers_bytes: 524288,
        ready_inputs_bytes: 1048576,
        cpu_arenas_bytes: 1048576,
        cache_and_runs_bytes: 1048576,
        query_reserve_bytes: 1048576,
        emergency_reserve_bytes: 1048576,
      },
    },
    io: {
      sources_selected: 3,
      source_bytes_selected: 30,
      sources_read: 2,
      source_bytes_read: 20,
      sources_delivered: 2,
      source_bytes_delivered: 20,
      source_bytes_avoided: 10,
      read_failures: 0,
      peak_ready_bytes: 20,
      peak_ready_items: 2,
    },
    work: { parses: 2 },
    cache: {
      enabled: true,
      metadata_hits: 1,
      runtime_hits: 0,
      legacy_hits: 0,
      misses: 2,
      bypasses: 0,
      stale_or_corrupt: 0,
      probe_failures: 0,
      payload_reads_avoided: 1,
      parses_avoided: 1,
      stores: 2,
      already_present: 0,
      store_failures: 0,
      payload_bytes_read: 0,
      payload_bytes_written: 20,
      artifact_bytes_read: 0,
      artifact_bytes_written: 24,
      peak_in_flight_transfer_bytes: 20,
    },
    process: { peak_rss_bytes: 1234, peak_rss_source: 'getrusage_maxrss_kib' },
    simd: { architecture: 'test', detected_features: [], enabled_kernels: [] },
  };
  return {
    ...runtime,
    ...overrides,
    io: { ...runtime.io, ...overrides.io },
    work: { ...runtime.work, ...overrides.work },
    cache: { ...runtime.cache, ...overrides.cache },
    process: { ...runtime.process, ...overrides.process },
  };
}

test('checked-in corpus is fully content-addressed and reconstructable', () => {
  const closure = validateCatalogClosure(catalog);
  assert.equal(closure.manifest_count, 1);
  assert.equal(closure.object_count, 12);
  const profile = loadCorpusProfile(catalog, 'ci-mixed-v1');
  assert.equal(profile.file_count, 11);
  assert.equal(profile.total_bytes, 648);
  assert.match(profile.manifest_sha256, /^[a-f0-9]{64}$/);
  assert.equal(profile.entries.reduce((sum, entry) => sum + entry.size, 0), 648);
  assert.equal(profile.mutation.size, 109);
  assert.notEqual(profile.mutation.before_sha256, profile.mutation.after_sha256);
});

test('git attributes preserve canonical metadata and byte-exact objects', () => {
  const manifest = 'benchmarks/universal/manifests/e9160a70af5454989d64c33dbf2fa4eca8d2ecd842568fce31830d69d67ced0d.json';
  const object = 'benchmarks/universal/objects/sha256/d0/d02f2cddbff360dc2c0bdb5dd20dfd3bbcbee276fb317fbc37c44e0f054e02df';
  const result = spawnSync('git', ['check-attr', 'text', 'eol', '--', manifest, object], {
    cwd: root,
    encoding: 'utf8',
  });
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, /manifests\/.*: text: set/);
  assert.match(result.stdout, /manifests\/.*: eol: lf/);
  assert.match(result.stdout, /objects\/.*: text: unset/);
});

test('materialization and mutation preserve size and change the pinned source digest', () => {
  const parent = temporary('materialize');
  const project = path.join(parent, 'project');
  try {
    const profile = loadCorpusProfile(catalog, 'ci-mixed-v1');
    materializeContentAddressedCorpus(profile, project);
    for (const entry of profile.entries) {
      const file = path.join(project, ...entry.path.split('/'));
      const bytes = readBoundedRegularFile(file, 16 * 1024 * 1024, entry.path);
      assert.equal(bytes.length, entry.size);
      assert.equal(sha256(bytes), entry.sha256);
    }
    assert.match(
      readFileSync(path.join(project, 'src', 'lib.rs'), 'ascii'),
      /pub const QUALIFICATION_FILE_0000:/,
    );
    const mutation = mutateCorpus(project, profile);
    assert.equal(mutation.size_before, mutation.size_after);
    assert.equal(mutation.sha256_before, profile.mutation.before_sha256);
    assert.equal(mutation.sha256_after, profile.mutation.after_sha256);
    assert.match(
      readFileSync(path.join(project, 'src', 'lib.rs'), 'ascii'),
      /pub const QUALIFICATION_FILE_9000:/,
    );
  } finally {
    rmSync(parent, { recursive: true, force: false });
  }
});

test('materialization refuses an existing target', () => {
  const parent = temporary('existing');
  const project = path.join(parent, 'project');
  try {
    mkdirSync(project);
    assert.throws(
      () => materializeContentAddressedCorpus(loadCorpusProfile(catalog, 'ci-mixed-v1'), project),
      /already exists/,
    );
  } finally {
    rmSync(parent, { recursive: true, force: false });
  }
});

test('portable logical paths reject aliases and cross-platform hazards', () => {
  for (const candidate of [
    '',
    '/absolute',
    '../escape',
    'a/../b',
    'a\\b',
    'C:/drive',
    'aux.txt',
    'trailing.',
    'trailing ',
    'has space/file.rs',
    'unicode-é.rs',
    'bad*/file',
    'graphoxide-out/graph.json',
  ]) {
    assert.throws(() => validatePortablePath(candidate), /portable|reserved/);
  }
  assert.equal(validatePortablePath('src/Good_name-1.rs'), 'src/Good_name-1.rs');
  assert.throws(
    () => validatePortablePathSequence(['a', 'a/b.rs']),
    /prefix conflict/,
  );
  const wide = Array.from(
    { length: 4096 },
    (_, index) => `shared/${index.toString().padStart(4, '0')}.rs`,
  );
  assert.equal(validatePortablePathSequence(wide).length, 4096);
  assert.throws(
    () => validatePortablePathSequence(['A', 'a/b.rs']),
    /prefix conflict/,
  );
  assert.throws(
    () => validatePortablePathSequence(['a', 'a-.rs', 'a/b.rs']),
    /prefix conflict/,
  );
  const deep = Array.from({ length: 1900 }, () => 'a').join('/');
  assert.equal(validatePortablePathSequence([`${deep}/a`, `${deep}/b`]).length, 2);
  assert.throws(
    () => validatePortablePathSequence([deep, `${deep}/b`]),
    /prefix conflict/,
  );
});

test('bounded readers reject symlink and hard-link sources', () => {
  const parent = temporary('links');
  try {
    const source = path.join(parent, 'source');
    const hard = path.join(parent, 'hard');
    const symbolic = path.join(parent, 'symbolic');
    writeFileSync(source, 'evidence');
    linkSync(source, hard);
    symlinkSync(source, symbolic);
    assert.throws(() => readBoundedRegularFile(source, 100, 'source'), /single-link/);
    assert.throws(() => readBoundedRegularFile(hard, 100, 'hard'), /single-link/);
    assert.throws(() => readBoundedRegularFile(symbolic, 100, 'symbolic'), /regular file/);
  } finally {
    rmSync(parent, { recursive: true, force: false });
  }
});

test('argument parsing makes destructive optional modes explicit', () => {
  assert.deepEqual(parseArguments(['--modes=warm']).modes, ['warm']);
  assert.throws(
    () => parseArguments(['--profile=synthetic-70gib-v1', '--large-root=/tmp']),
    /acknowledge-70-gib/,
  );
  assert.throws(
    () => parseArguments(['--modes=controlled_os_cold']),
    /absolute helper/,
  );
  assert.equal(LARGE_FILE_COUNT * LARGE_FILE_BYTES, LARGE_CORPUS_BYTES);
});

test('large generator is exact, deterministic, readable through its held descriptor, and same-size mutable', () => {
  const parent = temporary('large-file');
  const firstProject = path.join(parent, 'first');
  const secondProject = path.join(parent, 'second');
  const relative = 'generated/00/source-0000.rs';
  let firstDescriptor;
  try {
    const first = path.join(firstProject, ...relative.split('/'));
    const second = path.join(secondProject, ...relative.split('/'));
    mkdirSync(path.dirname(first), { recursive: true });
    mkdirSync(path.dirname(second), { recursive: true });
    const firstEvidence = generateLargeFile(first, 0);
    const secondEvidence = generateLargeFile(second, 0);
    firstDescriptor = openSync(first, fsConstants.O_RDONLY | (fsConstants.O_NOFOLLOW ?? 0));
    const firstSnapshot = snapshotDescriptor(firstDescriptor, first, {
      maximum: LARGE_FILE_BYTES,
      length: 128,
    });
    assert.equal(firstSnapshot.metadata.size, LARGE_FILE_BYTES);
    assert.deepEqual(firstEvidence, secondEvidence);
    assert.match(firstEvidence.sha256, /^[a-f0-9]{64}$/);
    assert.match(firstEvidence.mutation_sha256, /^[a-f0-9]{64}$/);
    assert.notEqual(firstEvidence.sha256, firstEvidence.mutation_sha256);
    const header = firstSnapshot.bytes;
    assert.match(header.toString('ascii'), /pub const QUALIFICATION_FILE_0000:/);
    const mutation = mutateCorpus(firstProject, {
      kind: 'generated_large',
      mutation: {
        path: relative,
        size: LARGE_FILE_BYTES,
        before_sha256: firstEvidence.sha256,
        after_sha256: firstEvidence.mutation_sha256,
      },
    });
    assert.equal(mutation.size_before, LARGE_FILE_BYTES);
    assert.equal(mutation.size_after, LARGE_FILE_BYTES);
    assert.equal(mutation.sha256_after, firstEvidence.mutation_sha256);
    const mutatedHeader = snapshotDescriptor(firstDescriptor, first, {
      maximum: LARGE_FILE_BYTES,
      length: 128,
    }).bytes;
    assert.match(mutatedHeader.toString('ascii'), /pub const QUALIFICATION_FILE_9000:/);
    assert.doesNotMatch(mutatedHeader.toString('ascii'), /pub const QUALIFICATION_FILE_0000:/);
  } finally {
    if (firstDescriptor !== undefined) closeSync(firstDescriptor);
    rmSync(parent, { recursive: true, force: false });
  }
});

test('large generator rechecks remaining bytes plus reserve during progress', () => {
  const remaining = 128;
  const required =
    BigInt(remaining) * BigInt(LARGE_FILE_BYTES) + 32n * 1024n * 1024n * 1024n;
  assert.equal(requireLargeGenerationReserve(required, remaining), required);
  assert.throws(
    () => requireLargeGenerationReserve(required - 1n, remaining),
    /remaining files and reserve/,
  );
});

test('large generation rejects a group/world-writable parent before disk admission', {
  skip: process.platform === 'win32' && 'POSIX ownership modes are not available',
}, () => {
  const largeRoot = temporary('large-root-mode');
  try {
    chmodSync(largeRoot, 0o777);
    assert.throws(
      () => materializeLargeCorpus({ largeRoot }),
      /owned by the runner and not group\/world writable/,
    );
    chmodSync(largeRoot, 0o770);
    assert.throws(
      () => materializeLargeCorpus({ largeRoot }),
      /owned by the runner and not group\/world writable/,
    );
  } finally {
    chmodSync(largeRoot, 0o700);
    rmSync(largeRoot, { recursive: true, force: false });
  }
});

test('runtime v2 validator enforces exact shape and conservation invariants', () => {
  const runtime = validRuntime();
  assert.equal(validateRuntimeTelemetryV2(runtime, 'full'), runtime);
  assert.throws(
    () => validateRuntimeTelemetryV2(validRuntime({ io: { sources_delivered: 1 } }), 'full'),
    /inconsistent/,
  );
  assert.throws(
    () => validateRuntimeTelemetryV2(validRuntime({ cache: { payload_reads_avoided: 0 } }), 'full'),
    /inconsistent/,
  );
  assert.throws(
    () => validateRuntimeTelemetryV2({ ...runtime, unexpected: true }, 'full'),
    /exactly/,
  );
});

test('index stdout requires finite non-negative elapsed evidence', () => {
  const report = {
    schema_version: 1,
    build: {
      schema_version: 1,
      operation: 'index',
      mode: 'full',
      status: 'rebuilt',
      elapsed_ms: 0,
    },
    coverage: { complete: true },
  };
  assert.equal(validateIndexStdout(report, 'full'), report);
  for (const elapsed of [-1, Number.NaN, Number.POSITIVE_INFINITY, '1']) {
    assert.throws(
      () => validateIndexStdout({ ...report, build: { ...report.build, elapsed_ms: elapsed } }, 'full'),
      /complete rebuilt full index/,
    );
  }
});

test('captured child output stops at cap plus one and retains lossless base64', async () => {
  const result = await runCaptured(
    process.execPath,
    ['-e', 'process.stdout.write("x".repeat(8192)); setInterval(() => {}, 1000)'],
    { cwd: root, timeoutMs: 10_000, env: {}, outputLimit: 1024 },
  );
  assert.equal(result.output_limit_exceeded, true);
  assert.equal(result.stdout.retained_bytes, 1024);
  assert.equal(result.stdout.observed_prefix_bytes, 1025);
  assert.equal(Buffer.from(result.stdout.retained_base64, 'base64').length, 1024);
  assert.ok(result.signal || result.status !== 0);
});

test('capture timeout closes inherited descendant pipes', async () => {
  const started = Date.now();
  const program = [
    'const {spawn}=require("node:child_process");',
    'spawn(process.execPath,["-e","setInterval(()=>{},1000)"],{stdio:["ignore","inherit","inherit"]});',
  ].join('');
  const result = await runCaptured(process.execPath, ['-e', program], {
    cwd: root,
    timeoutMs: 100,
    env: {},
    outputLimit: 1024,
  });
  assert.equal(result.timed_out, true);
  assert.ok(Date.now() - started < 6_000);
});

test('atomic report publication is no-overwrite and leaves complete JSON', () => {
  const parent = temporary('report');
  const report = path.join(parent, 'report.json');
  try {
    const publication = writeReportAtomic(report, { schema_version: 2, status: 'passed' });
    assert.match(publication.sha256, /^[a-f0-9]{64}$/);
    assert.deepEqual(JSON.parse(readFileSync(report, 'utf8')), {
      schema_version: 2,
      status: 'passed',
    });
    assert.equal(lstatSync(report).nlink, 1);
    assert.throws(() => writeReportAtomic(report, { status: 'replacement' }), /already exists/);
  } finally {
    rmSync(parent, { recursive: true, force: false });
  }
});

test('atomic report publication rejects a swapped temporary pathname', {
  skip: process.platform === 'win32' && 'ordinary Windows users cannot create test symlinks',
}, () => {
  const parent = temporary('report-swap');
  const report = path.join(parent, 'report.json');
  const attacker = path.join(parent, 'attacker.json');
  try {
    writeFileSync(attacker, '{"status":"attacker"}\n');
    assert.throws(
      () => writeReportAtomic(report, { status: 'passed' }, {
        beforeLink(temporaryReport) {
          unlinkSync(temporaryReport);
          symlinkSync(attacker, temporaryReport);
        },
      }),
      /verified report inode|retained for inspection/,
    );
    assert.throws(() => lstatSync(report), /ENOENT/);
  } finally {
    rmSync(parent, { recursive: true, force: false });
  }
});

test('atomic report publication rejects a replaced published pathname', {
  skip: process.platform === 'win32' && 'Windows does not permit replacing this open test file',
}, () => {
  const parent = temporary('report-published-swap');
  const report = path.join(parent, 'report.json');
  const attacker = path.join(parent, 'attacker.json');
  const attackerBytes = '{"status":"attacker"}\n';
  try {
    writeFileSync(attacker, attackerBytes);
    assert.throws(
      () => writeReportAtomic(report, { status: 'passed' }, {
        afterLink(_temporaryReport, publishedReport) {
          unlinkSync(publishedReport);
          linkSync(attacker, publishedReport);
        },
      }),
      /verified report inode|could not be verified/,
    );
    assert.equal(readFileSync(attacker, 'utf8'), attackerBytes);
    assert.equal(readFileSync(report, 'utf8'), attackerBytes);
  } finally {
    rmSync(parent, { recursive: true, force: false });
  }
});

test('report publication rejects an ancestor symlink alias', {
  skip: process.platform === 'win32' && 'ordinary Windows users cannot create test symlinks',
}, () => {
  const parent = temporary('report-alias');
  const real = path.join(parent, 'real');
  const alias = path.join(parent, 'alias');
  try {
    mkdirSync(real);
    symlinkSync(real, alias, process.platform === 'win32' ? 'junction' : 'dir');
    assert.throws(
      () => writeReportAtomic(path.join(alias, 'report.json'), { status: 'passed' }),
      /canonical absolute directory|alias/,
    );
  } finally {
    rmSync(parent, { recursive: true, force: false });
  }
});

test('report publication rejects a group/world-writable parent', {
  skip: process.platform === 'win32' && 'POSIX ownership modes are not available',
}, () => {
  const parent = temporary('report-parent-mode');
  try {
    chmodSync(parent, 0o777);
    assert.throws(
      () => writeReportAtomic(path.join(parent, 'report.json'), { status: 'passed' }),
      /owned by the runner and not group\/world writable/,
    );
  } finally {
    chmodSync(parent, 0o700);
    rmSync(parent, { recursive: true, force: false });
  }
});

test('checked-in content store contains no aliases or unexpected object bytes', () => {
  const copy = temporary('corrupt-copy');
  try {
    cpSync(path.join(root, 'benchmarks', 'universal'), copy, { recursive: true });
    const copiedCatalog = path.join(copy, 'catalog.json');
    const profile = loadCorpusProfile(copiedCatalog, 'ci-mixed-v1');
    const object = profile.entries[0];
    const objectPath = path.join(copy, 'objects', 'sha256', object.sha256.slice(0, 2), object.sha256);
    chmodSync(objectPath, 0o600);
    writeFileSync(objectPath, Buffer.from('changed'));
    assert.throws(() => loadCorpusProfile(copiedCatalog, 'ci-mixed-v1'), /size|SHA-256/);
  } finally {
    rmSync(copy, { recursive: true, force: false });
  }
});

test('catalog closure rejects an unreferenced content-addressed object', () => {
  const copy = temporary('extra-object');
  try {
    cpSync(path.join(root, 'benchmarks', 'universal'), copy, { recursive: true });
    const bytes = Buffer.from('unreferenced\n');
    const digest = sha256(bytes);
    const directory = path.join(copy, 'objects', 'sha256', digest.slice(0, 2));
    mkdirSync(directory, { recursive: true });
    writeFileSync(path.join(directory, digest), bytes);
    assert.throws(() => validateCatalogClosure(path.join(copy, 'catalog.json')), /unreferenced/);
  } finally {
    rmSync(copy, { recursive: true, force: false });
  }
});

test('catalog closure stops at its bounded manifest directory count', () => {
  const copy = temporary('extra-manifest');
  try {
    cpSync(path.join(root, 'benchmarks', 'universal'), copy, { recursive: true });
    writeFileSync(path.join(copy, 'manifests', `${'f'.repeat(64)}.json`), '{}\n');
    assert.throws(() => validateCatalogClosure(path.join(copy, 'catalog.json')), /entry ceiling/);
  } finally {
    rmSync(copy, { recursive: true, force: false });
  }
});

test('catalog profile admission is rejected before object traversal can amplify work', () => {
  const copy = temporary('profile-admission');
  try {
    cpSync(path.join(root, 'benchmarks', 'universal'), copy, { recursive: true });
    const copiedCatalog = path.join(copy, 'catalog.json');
    const value = JSON.parse(readFileSync(copiedCatalog, 'utf8'));
    value.profiles.push({ ...value.profiles[0], name: 'second-profile' });
    writeFileSync(copiedCatalog, `${JSON.stringify(value)}\n`);
    assert.throws(() => validateCatalogClosure(copiedCatalog), /profile count/);
  } finally {
    rmSync(copy, { recursive: true, force: false });
  }
});

test('catalog JSON rejects invalid UTF-8 instead of replacement decoding', () => {
  const parent = temporary('invalid-utf8');
  const invalidCatalog = path.join(parent, 'catalog.json');
  try {
    writeFileSync(invalidCatalog, Buffer.from([0xff, 0xfe]));
    assert.throws(() => loadCorpusProfile(invalidCatalog, 'ci-mixed-v1'), /invalid JSON/);
  } finally {
    rmSync(parent, { recursive: true, force: false });
  }
});

test('command output ceiling remains exactly eight MiB in production', () => {
  assert.equal(COMMAND_OUTPUT_MAX_BYTES, 8 * 1024 * 1024);
});

test('qualification labels admission-credit peaks separately from completed bytes and RSS', () => {
  const runner = readFileSync(path.join(root, 'scripts', 'qualify-universal-indexing.mjs'), 'utf8');
  const benchmarkGuide = readFileSync(path.join(root, 'BENCHMARKS.md'), 'utf8');
  const qualificationGuide = readFileSync(
    path.join(root, 'benchmarks', 'universal', 'README.md'),
    'utf8',
  );
  assert.match(runner, /peak_cache_transfer_credit_bytes/);
  assert.doesNotMatch(runner, /\bpeak_cache_transfer_bytes:/);
  for (const guide of [benchmarkGuide, qualificationGuide]) {
    assert.match(guide, /peak live reserved admission credits/);
    assert.match(guide, /resident-memory observation/);
    assert.match(guide, /payload_bytes_read/);
  }
});

test('manual qualification workflow is main-only and validates output command data', () => {
  const workflow = readFileSync(path.join(root, '.github', 'workflows', 'qualification.yml'), 'utf8');
  assert.match(
    workflow,
    /if: github\.repository == 'cgaspard\/graphoxide' && github\.ref == 'refs\/heads\/main'/,
  );
  assert.match(workflow, /timeout-minutes: 720/);
  const rawControlCheck = workflow.indexOf('process.env.REPORT_RAW');
  const realpath = workflow.indexOf('realpath -- "${REPORT_DIRECTORY}"');
  const canonicalControlCheck = workflow.indexOf('process.env.REPORT_PARENT');
  const outputWrite = workflow.indexOf(`printf '%s\\n' "path=\${report_path}"`);
  assert.ok(rawControlCheck >= 0, 'workflow must reject raw report-directory control bytes');
  assert.ok(realpath > rawControlCheck, 'workflow must validate raw input before realpath');
  assert.ok(canonicalControlCheck > realpath, 'workflow must validate the canonical path');
  assert.ok(outputWrite > canonicalControlCheck, 'workflow must validate before writing GITHUB_OUTPUT');
  assert.doesNotMatch(workflow, /echo "path=.*GITHUB_OUTPUT/);
});

test('qualification workflows byte-copy and exclusively pass a staged single-link binary', () => {
  const workflows = [
    ['hosted CI', readFileSync(path.join(root, '.github', 'workflows', 'ci.yml'), 'utf8'), 1],
    [
      'manual qualification',
      readFileSync(path.join(root, '.github', 'workflows', 'qualification.yml'), 'utf8'),
      3,
    ],
  ];
  for (const [label, workflow, expectedInvocations] of workflows) {
    assert.equal(
      [...workflow.matchAll(/target\/release\/graphoxide/g)].length,
      1,
      `${label} must name the Cargo artifact only as the staging source`,
    );
    assert.match(workflow, /source_binary="target\/release\/graphoxide"/);
    assert.match(workflow, /mkdir -m 0700 -- "\$\{staged_parent\}"/);
    assert.doesNotMatch(workflow, /test ! -e "\$\{staged_parent\}"|install -d/);
    assert.match(workflow, /staged_parent_identity="\$\(stat -c '%d:%i'/);
    assert.match(workflow, /stat -c '%F'.*source_binary.*regular file/);
    assert.match(workflow, /stat -c '%u'.*source_binary.*id -u/);
    assert.match(
      workflow,
      /install -m 0700 "\$\{source_binary\}" "\$\{staged_binary\}"/,
    );
    assert.match(workflow, /staged_binary="\$\(realpath -- "\$\{staged_binary\}"\)"/);
    assert.match(workflow, /stat -c '%h'.*staged_binary/);
    assert.match(workflow, /stat -c '%d:%i'.*staged_binary.*!=.*source_identity/);
    assert.match(workflow, /cmp -s "\$\{source_binary\}" "\$\{staged_binary\}"/);
    assert.match(workflow, /stat -c '%d:%i'.*staged_parent.*=.*staged_parent_identity/);
    assert.doesNotMatch(workflow, /--binary\s+target\/release\/graphoxide/);
    const invocations = [...workflow.matchAll(/--binary "\$\{STAGED_BINARY\}"/g)];
    assert.equal(invocations.length, expectedInvocations, `${label} must use only the staged path`);
  }
  const rootGuide = readFileSync(path.join(root, 'README.md'), 'utf8');
  const qualificationGuide = readFileSync(
    path.join(root, 'benchmarks', 'universal', 'README.md'),
    'utf8',
  );
  for (const guide of [rootGuide, qualificationGuide]) {
    assert.match(guide, /install -m 0700 target\/release\/graphoxide "\$staged_binary"/);
    assert.match(guide, /value\.nlink !== 1/);
    assert.match(guide, /--binary "\$staged_binary"/);
  }
});

test('staging primitives refuse an existing parent and de-alias a hardlinked binary', {
  skip: process.platform === 'win32' && 'POSIX install and inode semantics are required',
}, () => {
  const fixture = temporary('binary-stage');
  const cargoObject = path.join(fixture, 'cargo-object');
  const cargoBinary = path.join(fixture, 'cargo-graphoxide');
  const stagedParent = path.join(fixture, 'stage');
  const stagedBinary = path.join(stagedParent, 'graphoxide');
  try {
    writeFileSync(cargoObject, Buffer.from('deterministic executable bytes\n'), { mode: 0o700 });
    linkSync(cargoObject, cargoBinary);
    const firstMkdir = spawnSync('mkdir', ['-m', '0700', '--', stagedParent], {
      encoding: 'utf8',
    });
    assert.equal(firstMkdir.status, 0, firstMkdir.stderr);
    const repeatedMkdir = spawnSync('mkdir', ['-m', '0700', '--', stagedParent], {
      encoding: 'utf8',
    });
    assert.notEqual(repeatedMkdir.status, 0, 'atomic staging mkdir must refuse an existing parent');
    const installed = spawnSync('install', ['-m', '0700', cargoBinary, stagedBinary], {
      encoding: 'utf8',
    });
    assert.equal(installed.status, 0, installed.stderr);
    const source = descriptorSnapshot(cargoBinary);
    const destination = descriptorSnapshot(stagedBinary);
    assert.equal(source.metadata.nlink, 2, 'fixture source must simulate Cargo hard links');
    assert.equal(destination.metadata.nlink, 1);
    assert.notDeepEqual(
      [destination.metadata.dev, destination.metadata.ino],
      [source.metadata.dev, source.metadata.ino],
    );
    assert.equal(destination.metadata.mode & 0o777, 0o700);
    assert.deepEqual(destination.bytes, source.bytes);
  } finally {
    rmSync(fixture, { recursive: true, force: false });
  }
});

test('aggregate evidence exhaustion retains a bounded digest record', () => {
  const budget = { charged_bytes: 0, limit_bytes: 256, exhausted: false };
  const compact = chargeEvidence(
    {
      label: 'oversized',
      measured: true,
      status: 'failed',
      failure: 'provider diagnostic',
      process: {
        status: 1,
        signal: null,
        timed_out: false,
        output_limit_exceeded: false,
        external_wall_ms: 2,
        stdout: { bytes: 1000, retained_bytes: 1000, observed_prefix_sha256: 'a'.repeat(64) },
        stderr: { bytes: 1000, retained_bytes: 1000, observed_prefix_sha256: 'b'.repeat(64) },
      },
      payload: 'x'.repeat(10_000),
    },
    budget,
  );
  assert.equal(budget.exhausted, true);
  assert.equal(compact.status, 'failed');
  assert.match(compact.compacted_evidence.sha256, /^[a-f0-9]{64}$/);
  assert.equal(compact.compacted_evidence.process.status, 1);
  assert.ok(Buffer.byteLength(JSON.stringify(compact)) < 2_000);
});
