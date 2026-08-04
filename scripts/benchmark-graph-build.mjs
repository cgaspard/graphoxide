#!/usr/bin/env node

import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import {
  appendFileSync,
  cpSync,
  lstatSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  statSync,
  utimesSync,
} from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { performance } from 'node:perf_hooks';
import { fileURLToPath, pathToFileURL } from 'node:url';

const repositoryRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const DEFAULT_RUNS = 5;
const MAX_RUNS = 100;
const RUN_DIRECTORY_PREFIX = 'graphoxide-graph-build-';
const MUTATION_TARGET = 'jvm/app/Runner.java';
const MUTATION_TEXT =
  '\nfinal class GraphoxideBenchmarkMutation { static int revision() { return 1; } }\n';
const MAX_COMMAND_OUTPUT_BYTES = 16 * 1024 * 1024;
const FULL_BUILD_ARGS = ['extract', '.', '--force', '--json'];
const INCREMENTAL_UPDATE_ARGS = ['update', '.', '--json'];

const SOURCE_EXTENSIONS = new Set([
  '.c',
  '.cc',
  '.cpp',
  '.cs',
  '.go',
  '.java',
  '.js',
  '.jsx',
  '.py',
  '.rb',
  '.rs',
  '.ts',
  '.tsx',
]);

export const HELP = `Usage: node scripts/benchmark-graph-build.mjs [options]

Measure a fresh-output graph build and a one-file incremental update. The
result is emitted as structured JSON; timings are observations, not targets.

Options:
  --runs <count>     Number of fresh samples (default: ${DEFAULT_RUNS}, max: ${MAX_RUNS})
  --binary <path>    Graphoxide executable (default: target/release/graphoxide)
  --fixture <path>   Fixture directory (default: parity/corpora/language-matrix)
  --help             Show this help
`;

function defaultBinary() {
  const executable = process.platform === 'win32' ? 'graphoxide.exe' : 'graphoxide';
  return path.join(repositoryRoot, 'target', 'release', executable);
}

function optionValue(argument, name, argv, index) {
  const inline = argument.startsWith(`${name}=`) ? argument.slice(name.length + 1) : undefined;
  if (inline !== undefined) {
    if (!inline) throw new Error(`${name} requires a value`);
    return { value: inline, nextIndex: index };
  }
  const value = argv[index + 1];
  if (!value || value.startsWith('--')) throw new Error(`${name} requires a value`);
  return { value, nextIndex: index + 1 };
}

export function parseArguments(argv, cwd = process.cwd()) {
  const options = {
    runs: DEFAULT_RUNS,
    binary: defaultBinary(),
    binaryExplicit: false,
    fixture: path.join(repositoryRoot, 'parity', 'corpora', 'language-matrix'),
    help: false,
  };

  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index];
    if (argument === '--help') {
      options.help = true;
      continue;
    }
    if (argument === '--runs' || argument.startsWith('--runs=')) {
      const parsed = optionValue(argument, '--runs', argv, index);
      index = parsed.nextIndex;
      if (!/^\d+$/.test(parsed.value)) throw new Error('--runs must be a positive integer');
      options.runs = Number(parsed.value);
      if (options.runs < 1 || options.runs > MAX_RUNS) {
        throw new Error(`--runs must be between 1 and ${MAX_RUNS}`);
      }
      continue;
    }
    if (argument === '--binary' || argument.startsWith('--binary=')) {
      const parsed = optionValue(argument, '--binary', argv, index);
      index = parsed.nextIndex;
      options.binary = path.resolve(cwd, parsed.value);
      options.binaryExplicit = true;
      continue;
    }
    if (argument === '--fixture' || argument.startsWith('--fixture=')) {
      const parsed = optionValue(argument, '--fixture', argv, index);
      index = parsed.nextIndex;
      options.fixture = path.resolve(cwd, parsed.value);
      continue;
    }
    throw new Error(`unknown option: ${argument}`);
  }

  options.binary = path.resolve(options.binary);
  options.fixture = path.resolve(options.fixture);
  return options;
}

function compareText(left, right) {
  if (left < right) return -1;
  if (left > right) return 1;
  return 0;
}

function canonicalJson(value) {
  if (value === null || typeof value !== 'object') return JSON.stringify(value);
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(',')}]`;
  const entries = Object.keys(value)
    .sort(compareText)
    .map((key) => `${JSON.stringify(key)}:${canonicalJson(value[key])}`);
  return `{${entries.join(',')}}`;
}

function collectCorpusEntries(root) {
  const entries = [];
  const visit = (directory) => {
    for (const entry of readdirSync(directory, { withFileTypes: true })) {
      const absolute = path.join(directory, entry.name);
      entries.push({ absolute, entry });
      if (entry.isDirectory()) visit(absolute);
    }
  };
  visit(root);
  return entries
    .map(({ absolute, entry }) => ({
      absolute,
      entry,
      relative: path
        .relative(root, absolute)
        .split(path.sep)
        .join('/')
        .normalize('NFC'),
    }))
    .sort((left, right) => compareText(left.relative, right.relative));
}

export function describeFixture(fixture) {
  const root = path.resolve(fixture);
  const rootStat = lstatSync(root);
  if (rootStat.isSymbolicLink()) throw new Error(`fixture root must not be a symlink: ${root}`);
  if (!rootStat.isDirectory()) throw new Error(`fixture is not a directory: ${root}`);

  const records = [];
  const seen = new Set();
  let fileCount = 0;
  let totalBytes = 0;
  for (const { absolute, entry, relative } of collectCorpusEntries(root)) {
    if (seen.has(relative)) {
      throw new Error(`duplicate NFC-normalized fixture path: ${relative}`);
    }
    seen.add(relative);
    if (entry.isSymbolicLink()) throw new Error(`fixture symlinks are forbidden: ${relative}`);
    if (entry.isDirectory()) {
      records.push({ kind: 'directory', path: relative });
      continue;
    }
    if (!entry.isFile()) throw new Error(`unsupported fixture entry: ${relative}`);
    const content = readFileSync(absolute);
    fileCount += 1;
    totalBytes += content.length;
    records.push({
      kind: 'file',
      path: relative,
      size: content.length,
      sha256: createHash('sha256').update(content).digest('hex'),
    });
  }

  const payload = canonicalJson({ schema: 'graphoxide-corpus-input-v1', records });
  return {
    sha256: createHash('sha256').update(payload, 'utf8').digest('hex'),
    file_count: fileCount,
    total_bytes: totalBytes,
  };
}

function sha256File(file) {
  return createHash('sha256').update(readFileSync(file)).digest('hex');
}

function relativeDisplay(absolute) {
  const relative = path.relative(repositoryRoot, absolute);
  return relative && !relative.startsWith(`..${path.sep}`) && relative !== '..'
    ? relative.split(path.sep).join('/')
    : absolute;
}

function requireExecutable(binary) {
  let metadata;
  try {
    metadata = statSync(binary);
  } catch (error) {
    if (error?.code === 'ENOENT') {
      throw new Error(
        `Graphoxide binary not found at ${binary}; run npm run benchmark:graph-build or build the release binary first`,
      );
    }
    throw error;
  }
  if (!metadata.isFile()) throw new Error(`Graphoxide binary is not a file: ${binary}`);
}

function buildDefaultReleaseBinary() {
  const result = spawnSync(
    'cargo',
    ['build', '--quiet', '--release', '--locked', '--bin', 'graphoxide'],
    {
      cwd: repositoryRoot,
      encoding: 'utf8',
      maxBuffer: MAX_COMMAND_OUTPUT_BYTES,
    },
  );
  if (result.error) throw new Error(`release build failed to start: ${result.error.message}`);
  if (result.status !== 0) {
    const diagnostics = outputPreview(result.stderr || result.stdout);
    throw new Error(
      `release build exited with status ${result.status}${diagnostics ? `: ${diagnostics}` : ''}`,
    );
  }
}

function outputPreview(value) {
  const normalized = String(value ?? '').trim();
  return normalized.length <= 2_000 ? normalized : `${normalized.slice(0, 2_000)}…`;
}

export function parseCliReport(stdout, commandName = 'graphoxide command') {
  const text = String(stdout).trim();
  if (!text) throw new Error(`${commandName} emitted no JSON on stdout`);
  let report;
  try {
    report = JSON.parse(text);
  } catch (error) {
    throw new Error(`${commandName} emitted invalid JSON: ${error.message}`);
  }
  if (report === null || Array.isArray(report) || typeof report !== 'object') {
    throw new Error(`${commandName} JSON must be an object`);
  }
  if (
    typeof report.elapsed_ms !== 'number' ||
    !Number.isFinite(report.elapsed_ms) ||
    report.elapsed_ms < 0
  ) {
    throw new Error(`${commandName} JSON must contain a finite non-negative elapsed_ms`);
  }
  return report;
}

export function validateCliReport(report, expected, commandName = 'graphoxide command') {
  for (const field of ['operation', 'mode', 'status']) {
    if (report[field] !== expected[field]) {
      throw new Error(
        `${commandName} JSON must report ${field}=${expected[field]} (received ${String(
          report[field],
        )})`,
      );
    }
  }
  for (const field of ['changed', 'processed']) {
    if (expected[field] !== undefined && report.files?.[field] !== expected[field]) {
      throw new Error(
        `${commandName} JSON must report files.${field}=${expected[field]} (received ${String(
          report.files?.[field],
        )})`,
      );
    }
  }
  return report;
}

function sanitizedEnvironment() {
  const environment = { ...process.env };
  for (const name of [
    'GRAPHOXIDE_FORCE',
    'GRAPHIFY_FORCE',
    'GRAPHOXIDE_OUT',
    'GRAPHIFY_OUT',
    'GRAPHOXIDE_VIZ_NODE_LIMIT',
    'GRAPHIFY_VIZ_NODE_LIMIT',
  ]) {
    delete environment[name];
  }
  return environment;
}

export function runCliJson(binary, args, cwd, expected, clock = () => performance.now()) {
  const started = clock();
  const result = spawnSync(binary, args, {
    cwd,
    encoding: 'utf8',
    env: sanitizedEnvironment(),
    maxBuffer: MAX_COMMAND_OUTPUT_BYTES,
  });
  const externalWallMs = clock() - started;
  const commandName = `graphoxide ${args[0]}`;
  if (result.error) throw new Error(`${commandName} failed to start: ${result.error.message}`);
  if (result.status !== 0) {
    const stderr = outputPreview(result.stderr);
    const stdout = outputPreview(result.stdout);
    throw new Error(
      `${commandName} exited with status ${result.status}${stderr ? `: ${stderr}` : ''}${
        stdout ? ` (stdout: ${stdout})` : ''
      }`,
    );
  }
  const report = validateCliReport(parseCliReport(result.stdout, commandName), expected, commandName);
  return {
    external_wall_ms: roundMilliseconds(externalWallMs),
    reported_elapsed_ms: report.elapsed_ms,
    cli_report: report,
  };
}

function findMutationTarget(project) {
  const preferred = path.join(project, ...MUTATION_TARGET.split('/'));
  try {
    if (statSync(preferred).isFile()) return preferred;
  } catch (error) {
    if (error?.code !== 'ENOENT') throw error;
  }
  const fallback = collectCorpusEntries(project).find(
    ({ absolute, entry }) =>
      entry.isFile() && SOURCE_EXTENSIONS.has(path.extname(absolute).toLowerCase()),
  );
  if (!fallback) {
    throw new Error('fixture contains no source file with a built-in deterministic mutation strategy');
  }
  return fallback.absolute;
}

function mutationText(file) {
  switch (path.extname(file).toLowerCase()) {
    case '.py':
      return '\ndef graphoxide_benchmark_mutation():\n    return 1\n';
    case '.js':
    case '.jsx':
    case '.ts':
    case '.tsx':
      return '\nexport function graphoxideBenchmarkMutation() { return 1; }\n';
    case '.rs':
      return '\nfn graphoxide_benchmark_mutation() -> usize { 1 }\n';
    case '.go':
      return '\nfunc graphoxideBenchmarkMutation() int { return 1 }\n';
    case '.rb':
      return '\ndef graphoxide_benchmark_mutation\n  1\nend\n';
    case '.cs':
      return '\ninternal static class GraphoxideBenchmarkMutation { internal static int Revision() => 1; }\n';
    case '.c':
    case '.cc':
    case '.cpp':
      return '\nstatic int graphoxide_benchmark_mutation(void) { return 1; }\n';
    case '.java':
      return MUTATION_TEXT;
    default:
      return '\n// graphoxide benchmark mutation\n';
  }
}

export function mutateCopiedFixture(project) {
  const target = findMutationTarget(project);
  const metadata = statSync(target);
  const before = sha256File(target);
  appendFileSync(target, mutationText(target), 'utf8');
  // Some filesystems expose coarse timestamp precision. Move mtime far enough
  // forward that the incremental detector must observe the content change.
  const changedMtime = new Date(Math.max(Date.now(), metadata.mtimeMs + 2_000));
  utimesSync(target, metadata.atime, changedMtime);
  const after = sha256File(target);
  if (before === after) throw new Error('benchmark fixture mutation did not change the source file');
  return {
    path: path.relative(project, target).split(path.sep).join('/'),
    sha256_before: before,
    sha256_after: after,
  };
}

function cleanupRunDirectory(runDirectory) {
  const resolved = path.resolve(runDirectory);
  const expectedParent = path.resolve(os.tmpdir());
  if (
    path.dirname(resolved) !== expectedParent ||
    !path.basename(resolved).startsWith(RUN_DIRECTORY_PREFIX)
  ) {
    throw new Error(`refusing to clean unexpected benchmark directory: ${resolved}`);
  }
  rmSync(resolved, { recursive: true, force: false });
}

export function runSample({ binary, fixture, run }) {
  const runDirectory = mkdtempSync(path.join(os.tmpdir(), RUN_DIRECTORY_PREFIX));
  const project = path.join(runDirectory, 'project');
  try {
    cpSync(fixture, project, { recursive: true, errorOnExist: true, preserveTimestamps: true });
    const fullBuild = runCliJson(
      binary,
      FULL_BUILD_ARGS,
      project,
      { operation: 'extract', mode: 'full', status: 'rebuilt' },
    );
    const mutation = mutateCopiedFixture(project);
    const incrementalUpdate = runCliJson(binary, INCREMENTAL_UPDATE_ARGS, project, {
      operation: 'update',
      mode: 'incremental',
      status: 'rebuilt',
      changed: 1,
      processed: 1,
    });
    return {
      run,
      mutation,
      full_build: fullBuild,
      incremental_update: incrementalUpdate,
    };
  } finally {
    cleanupRunDirectory(runDirectory);
  }
}

function roundMilliseconds(value) {
  return Number(value.toFixed(3));
}

export function summarize(values) {
  if (!Array.isArray(values) || values.length === 0) {
    throw new Error('cannot summarize an empty sample set');
  }
  if (values.some((value) => typeof value !== 'number' || !Number.isFinite(value) || value < 0)) {
    throw new Error('timing samples must be finite non-negative numbers');
  }
  const sorted = [...values].sort((left, right) => left - right);
  const middle = Math.floor(sorted.length / 2);
  const median =
    sorted.length % 2 === 1 ? sorted[middle] : (sorted[middle - 1] + sorted[middle]) / 2;
  return {
    min: roundMilliseconds(sorted[0]),
    median: roundMilliseconds(median),
    max: roundMilliseconds(sorted.at(-1)),
  };
}

export function summarizeSamples(samples) {
  const phase = (name) => ({
    external_wall_ms: summarize(samples.map((sample) => sample[name].external_wall_ms)),
    reported_elapsed_ms: summarize(samples.map((sample) => sample[name].reported_elapsed_ms)),
  });
  return {
    full_build: phase('full_build'),
    incremental_update: phase('incremental_update'),
  };
}

function probe(command, args, cwd = repositoryRoot) {
  const result = spawnSync(command, args, {
    cwd,
    encoding: 'utf8',
    maxBuffer: MAX_COMMAND_OUTPUT_BYTES,
  });
  if (result.error || result.status !== 0) return null;
  const output = String(result.stdout).trim();
  return output || null;
}

function repositoryMetadata() {
  const status = spawnSync('git', ['status', '--porcelain'], {
    cwd: repositoryRoot,
    encoding: 'utf8',
    maxBuffer: MAX_COMMAND_OUTPUT_BYTES,
  });
  return {
    commit: probe('git', ['rev-parse', 'HEAD']),
    dirty:
      status.error || status.status !== 0 ? null : String(status.stdout).trim().length !== 0,
  };
}

function environmentMetadata() {
  const cpus = os.cpus();
  return {
    platform: process.platform,
    os_release: os.release(),
    architecture: process.arch,
    logical_cpu_count: cpus.length,
    cpu_model: cpus[0]?.model ?? null,
    total_memory_bytes: os.totalmem(),
    node_version: process.version,
  };
}

export function runBenchmark(options) {
  requireExecutable(options.binary);
  const fixture = describeFixture(options.fixture);
  const cliVersion = probe(options.binary, ['--version']);
  if (!cliVersion) throw new Error(`could not read CLI version from ${options.binary}`);

  const samples = [];
  for (let run = 1; run <= options.runs; run += 1) {
    samples.push(runSample({ binary: options.binary, fixture: options.fixture, run }));
  }

  return {
    schema_version: 1,
    benchmark: 'graphoxide-graph-build',
    generated_at: new Date().toISOString(),
    runs: options.runs,
    fixture: {
      path: relativeDisplay(options.fixture),
      ...fixture,
      mutation: {
        preferred_path: MUTATION_TARGET,
        method: 'append one deterministic source declaration in the temporary copy',
      },
    },
    commands: {
      full_build: ['graphoxide', ...FULL_BUILD_ARGS],
      incremental_update: ['graphoxide', ...INCREMENTAL_UPDATE_ARGS],
    },
    metadata: {
      repository: repositoryMetadata(),
      binary: {
        path: relativeDisplay(options.binary),
        sha256: sha256File(options.binary),
      },
      cli_version: cliVersion,
      rust_version: probe('rustc', ['--version']),
      environment: environmentMetadata(),
    },
    samples,
    summary: summarizeSamples(samples),
    notes: [
      'Compilation, fixture copying, metadata collection, and source mutation are outside timed regions.',
      'External wall time includes process startup; reported elapsed_ms is supplied by the CLI.',
      'Operating-system filesystem caches are not flushed or controlled.',
      'These measurements are descriptive observations for this fixture and environment, not performance targets.',
    ],
  };
}

export function main(argv = process.argv.slice(2)) {
  const options = parseArguments(argv);
  if (options.help) {
    process.stdout.write(HELP);
    return;
  }
  if (!options.binaryExplicit) buildDefaultReleaseBinary();
  const report = runBenchmark(options);
  process.stdout.write(`${JSON.stringify(report, null, 2)}\n`);
}

const directInvocation =
  process.argv[1] && import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href;
if (directInvocation) {
  try {
    main();
  } catch (error) {
    process.stderr.write(`benchmark-graph-build: ${error.message}\n`);
    process.exitCode = 1;
  }
}
