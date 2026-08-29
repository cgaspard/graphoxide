#!/usr/bin/env node

import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import {
  closeSync,
  constants as fsConstants,
  cpSync,
  fstatSync,
  fsyncSync,
  futimesSync,
  lstatSync,
  mkdirSync,
  mkdtempSync,
  openSync,
  readFileSync,
  readSync,
  realpathSync,
  readdirSync,
  rmSync,
  unlinkSync,
  writeSync,
  writeFileSync,
} from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { performance } from 'node:perf_hooks';
import { fileURLToPath, pathToFileURL } from 'node:url';

const repositoryRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const DEFAULT_RUNS = 5;
const MAX_RUNS = 100;
const RUN_DIRECTORY_PREFIX = 'graphoxide-graph-build-';
const SCENARIO_DIRECTORY_PREFIX = 'graphoxide-benchmark-scenario-';
const MUTATION_TARGET = 'jvm/app/Runner.java';
const MUTATION_TEXT =
  '\nfinal class GraphoxideBenchmarkMutation { static int revision() { return 1; } }\n';
const MAX_COMMAND_OUTPUT_BYTES = 16 * 1024 * 1024;
const MAX_BINARY_BYTES = 1024 * 1024 * 1024;
const FULL_BUILD_ARGS = ['extract', '.', '--force', '--json'];
const INCREMENTAL_UPDATE_ARGS = ['update', '.', '--json'];
const CATALOG_WIKI_BUILD_ARGS = ['index', '.', '--catalog', 'provenance', '--json'];
const CATALOG_WIKI_INDEX_ARGS = ['wiki', 'index', '.', '--config', 'wiki.json'];
const CATALOG_WIKI_CHECK_ARGS = [
  'wiki',
  'check',
  '.',
  '--config',
  'wiki.json',
  '--catalog',
  'provenance',
  '--graph',
  'graphoxide-out/graph.json',
];
const INDEX_RUNTIME_TELEMETRY_SCHEMA_VERSION = 2;

const SCENARIO_DEFINITIONS = Object.freeze({
  'compat-language-matrix': Object.freeze({
    description: 'fresh-output build plus one deterministic incremental source mutation',
    fixture: 'language-matrix',
    format_families: Object.freeze(['source-language-compatibility']),
  }),
  'many-small': Object.freeze({
    description: '192 small Rust sources plus a deterministic incremental mutation',
    fixture: 'generated',
    format_families: Object.freeze(['source-language-compatibility']),
  }),
  'mixed-size': Object.freeze({
    description: 'small source inputs with one large strict JSON document',
    fixture: 'generated',
    format_families: Object.freeze(['source-language-compatibility', 'structured-json']),
  }),
  'structured-json': Object.freeze({
    description: 'strict JSON, JSONC, JSON5, JSON Schema, and OpenAPI documents with a source anchor',
    fixture: 'generated',
    format_families: Object.freeze(['structured-json', 'configuration', 'schema']),
  }),
  'cache-warm': Object.freeze({
    description: 'source corpus whose incremental pass reuses unchanged inputs',
    fixture: 'generated',
    format_families: Object.freeze(['source-language-compatibility']),
  }),
  'slow-io': Object.freeze({
    description: 'many small files that expose metadata/read latency without external I/O injection',
    fixture: 'generated',
    format_families: Object.freeze(['source-language-compatibility']),
  }),
  'large-graph': Object.freeze({
    description: 'generated source declarations sized to exercise graph construction',
    fixture: 'generated',
    format_families: Object.freeze(['source-language-compatibility']),
  }),
  'structured-containers': Object.freeze({
    description: 'structured text plus deterministic ZIP, TAR, GZIP, and SVGZ container inputs',
    fixture: 'generated',
    format_families: Object.freeze(['structured-text', 'container', 'image-document']),
  }),
  'idl-schema': Object.freeze({
    description: 'textual protocol IDL, schema, descriptor, and API-contract inputs with a source anchor',
    fixture: 'generated',
    format_families: Object.freeze(['protocol-idl', 'schema', 'api-contract']),
  }),
  diagrams: Object.freeze({
    description: 'DOT, Mermaid, PlantUML, D2, draw.io, BPMN, and modeling-diagram inputs',
    fixture: 'generated',
    format_families: Object.freeze(['diagram', 'engineering-design']),
  }),
  'facility-models': Object.freeze({
    description: 'electrical, facility, building, thermal, and infrastructure-model inputs',
    fixture: 'generated',
    format_families: Object.freeze(['electrical-design', 'building-information', 'infrastructure-model']),
  }),
  'openusd-assets': Object.freeze({
    description: 'OpenUSD, robotics, simulation, geometry, and material-asset inputs',
    fixture: 'generated',
    format_families: Object.freeze(['openusd', 'simulation', 'robotics', '3d-asset']),
  }),
  'catalog-wiki': Object.freeze({
    description: 'mixed catalog and wiki documents with archive-only, ignored, malformed, and annotation inputs',
    fixture: 'generated',
    format_families: Object.freeze([
      'markdown',
      'structured-json',
      'configuration',
      'office-document',
      'pdf',
      'container',
      'catalog-metadata',
    ]),
  }),
});

export const BENCHMARK_SCENARIOS = Object.freeze(Object.keys(SCENARIO_DEFINITIONS));

// The profile is emitted in every report. It states which fixture family was
// exercised, without implying a performance threshold or claiming that it is a
// complete conformance suite for the underlying formats.
export function profileForScenario(name) {
  const definition = SCENARIO_DEFINITIONS[name];
  if (!definition) throw new Error(`unknown benchmark scenario: ${name}`);
  return Object.freeze({
    name,
    description: definition.description,
    format_families: [...definition.format_families],
  });
}

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
  --scenario <name>  Built-in deterministic profile (${BENCHMARK_SCENARIOS.join(', ')})
  --help             Show this help
`;

// Benchmark measurements are only valid for the default isolated execution
// model. Keep the guard separate from telemetry validation so an accidental
// future flag change cannot run even one legacy sample.
export function assertIsolatedBenchmarkArgs(args) {
  if (args.includes('--legacy-executor')) {
    throw new Error('benchmark commands must not opt into --legacy-executor');
  }
}

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
    fixtureExplicit: false,
    scenario: 'compat-language-matrix',
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
      options.fixtureExplicit = true;
      continue;
    }
    if (argument === '--scenario' || argument.startsWith('--scenario=')) {
      const parsed = optionValue(argument, '--scenario', argv, index);
      index = parsed.nextIndex;
      if (!(parsed.value in SCENARIO_DEFINITIONS)) {
        throw new Error(`--scenario must be one of: ${BENCHMARK_SCENARIOS.join(', ')}`);
      }
      options.scenario = parsed.value;
      continue;
    }
    throw new Error(`unknown option: ${argument}`);
  }

  options.binary = path.resolve(options.binary);
  options.fixture = path.resolve(options.fixture);
  if (options.fixtureExplicit && options.scenario !== 'compat-language-matrix') {
    throw new Error('--fixture cannot be combined with a generated --scenario');
  }
  if (options.fixtureExplicit) options.scenario = 'custom-fixture';
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

// Name the only supported benchmark scenario without pretending that an
// arbitrary custom fixture has the same coverage characteristics.
export function describeScenario(fixture, requestedScenario) {
  if (requestedScenario && requestedScenario !== 'custom-fixture') {
    return profileForScenario(requestedScenario);
  }
  const baseline = path.join(repositoryRoot, 'parity', 'corpora', 'language-matrix');
  if (path.resolve(fixture) === path.resolve(baseline)) {
    return profileForScenario('compat-language-matrix');
  }
  return {
    name: 'custom-fixture',
    description: 'fresh-output build plus one deterministic incremental source mutation',
    format_families: [],
  };
}

function writeScenarioFile(root, relative, content) {
  const target = path.join(root, ...relative.split('/'));
  mkdirSync(path.dirname(target), { recursive: true });
  writeFileSync(target, content, 'utf8');
}

function writeScenarioBytes(root, relative, bytes) {
  const target = path.join(root, ...relative.split('/'));
  mkdirSync(path.dirname(target), { recursive: true });
  writeFileSync(target, bytes);
}

function crc32(bytes) {
  let value = 0xffffffff;
  for (const byte of bytes) {
    value ^= byte;
    for (let bit = 0; bit < 8; bit += 1) {
      value = (value >>> 1) ^ (value & 1 ? 0xedb88320 : 0);
    }
  }
  return (value ^ 0xffffffff) >>> 0;
}

// Generate archives in-process instead of invoking host archive tools. Fixed
// timestamps, stored ZIP members, and canonical entry order make fixture
// digests portable and prevent the benchmark from depending on host tooling.
function createStoredZip(entries) {
  const localParts = [];
  const centralParts = [];
  let offset = 0;
  for (const entry of [...entries].sort((left, right) => compareText(left.path, right.path))) {
    const name = Buffer.from(entry.path, 'utf8');
    const data = Buffer.from(entry.data);
    const checksum = crc32(data);
    const local = Buffer.alloc(30);
    local.writeUInt32LE(0x04034b50, 0);
    local.writeUInt16LE(20, 4);
    local.writeUInt16LE(0x0800, 6);
    local.writeUInt16LE(0, 8);
    local.writeUInt16LE(0, 10);
    local.writeUInt16LE(0x0021, 12);
    local.writeUInt32LE(checksum, 14);
    local.writeUInt32LE(data.length, 18);
    local.writeUInt32LE(data.length, 22);
    local.writeUInt16LE(name.length, 26);
    local.writeUInt16LE(0, 28);
    localParts.push(local, name, data);

    const central = Buffer.alloc(46);
    central.writeUInt32LE(0x02014b50, 0);
    central.writeUInt16LE(0x0314, 4);
    central.writeUInt16LE(20, 6);
    central.writeUInt16LE(0x0800, 8);
    central.writeUInt16LE(0, 10);
    central.writeUInt16LE(0, 12);
    central.writeUInt16LE(0x0021, 14);
    central.writeUInt32LE(checksum, 16);
    central.writeUInt32LE(data.length, 20);
    central.writeUInt32LE(data.length, 24);
    central.writeUInt16LE(name.length, 28);
    central.writeUInt16LE(0, 30);
    central.writeUInt16LE(0, 32);
    central.writeUInt16LE(0, 34);
    central.writeUInt16LE(0, 36);
    central.writeUInt32LE(0, 38);
    central.writeUInt32LE(offset, 42);
    centralParts.push(central, name);
    offset += local.length + name.length + data.length;
  }
  const centralBytes = Buffer.concat(centralParts);
  const end = Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50, 0);
  end.writeUInt16LE(0, 4);
  end.writeUInt16LE(0, 6);
  end.writeUInt16LE(entries.length, 8);
  end.writeUInt16LE(entries.length, 10);
  end.writeUInt32LE(centralBytes.length, 12);
  end.writeUInt32LE(offset, 16);
  end.writeUInt16LE(0, 20);
  return Buffer.concat([...localParts, centralBytes, end]);
}

function writeTarOctal(header, offset, length, value) {
  const text = value.toString(8).padStart(length - 1, '0');
  header.write(`${text}\0`, offset, length, 'ascii');
}

function createTar(entries) {
  const parts = [];
  for (const entry of [...entries].sort((left, right) => compareText(left.path, right.path))) {
    const data = Buffer.from(entry.data);
    const header = Buffer.alloc(512);
    header.write(entry.path, 0, 100, 'utf8');
    writeTarOctal(header, 100, 8, 0o644);
    writeTarOctal(header, 108, 8, 0);
    writeTarOctal(header, 116, 8, 0);
    writeTarOctal(header, 124, 12, data.length);
    writeTarOctal(header, 136, 12, 0);
    header.fill(0x20, 148, 156);
    header.write('0', 156, 1, 'ascii');
    header.write('ustar\0', 257, 6, 'ascii');
    header.write('00', 263, 2, 'ascii');
    const checksum = header.reduce((sum, byte) => sum + byte, 0);
    header.write(`${checksum.toString(8).padStart(6, '0')}\0 `, 148, 8, 'ascii');
    parts.push(header, data);
    const remainder = data.length % 512;
    if (remainder !== 0) parts.push(Buffer.alloc(512 - remainder));
  }
  parts.push(Buffer.alloc(1024));
  return Buffer.concat(parts);
}

// A GZIP member with DEFLATE stored blocks. This is intentionally not the
// smallest representation: its fixed headers, fixed OS marker, and no-compress
// blocks make generated fixture bytes independent of the host zlib version.
function createStoredGzip(input) {
  const data = Buffer.from(input);
  const blocks = [];
  for (let offset = 0; offset < data.length; offset += 0xffff) {
    const length = Math.min(0xffff, data.length - offset);
    const block = Buffer.alloc(5);
    block[0] = offset + length === data.length ? 1 : 0;
    block.writeUInt16LE(length, 1);
    block.writeUInt16LE((~length) & 0xffff, 3);
    blocks.push(block, data.subarray(offset, offset + length));
  }
  const header = Buffer.from([0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 255]);
  const trailer = Buffer.alloc(8);
  trailer.writeUInt32LE(crc32(data), 0);
  trailer.writeUInt32LE(data.length >>> 0, 4);
  return Buffer.concat([header, ...blocks, trailer]);
}

function createGlb(json) {
  const jsonBytes = Buffer.from(JSON.stringify(json), 'utf8');
  const paddedLength = Math.ceil(jsonBytes.length / 4) * 4;
  const body = Buffer.alloc(paddedLength, 0x20);
  jsonBytes.copy(body);
  const header = Buffer.alloc(20);
  header.writeUInt32LE(0x46546c67, 0);
  header.writeUInt32LE(2, 4);
  header.writeUInt32LE(header.length + body.length, 8);
  header.writeUInt32LE(body.length, 12);
  header.writeUInt32LE(0x4e4f534a, 16);
  return Buffer.concat([header, body]);
}

function createMinimalPdf() {
  const objects = [
    '1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n',
    '2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n',
    '3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 72 72] >>\nendobj\n',
  ];
  const header = '%PDF-1.4\n';
  const offsets = [];
  let cursor = Buffer.byteLength(header, 'ascii');
  for (const object of objects) {
    offsets.push(cursor);
    cursor += Buffer.byteLength(object, 'ascii');
  }
  const xref = `xref\n0 4\n0000000000 65535 f \n${offsets
    .map((offset) => `${String(offset).padStart(10, '0')} 00000 n \n`)
    .join('')}trailer\n<< /Size 4 /Root 1 0 R >>\nstartxref\n${cursor}\n%%EOF\n`;
  return Buffer.from(`${header}${objects.join('')}${xref}`, 'ascii');
}

function writeSourceAnchor(root) {
  writeScenarioFile(
    root,
    'src/benchmark.rs',
    'pub fn graphoxide_benchmark_anchor() -> usize { 1 }\n',
  );
}

function writeSmallRustSources(root, count, declarations = 1) {
  for (let index = 0; index < count; index += 1) {
    const functions = Array.from(
      { length: declarations },
      (_, declaration) =>
        `pub fn benchmark_${index}_${declaration}() -> usize { ${index + declaration} }`,
    );
    writeScenarioFile(root, `src/generated_${index}.rs`, `${functions.join('\n')}\n`);
  }
}

export function materializeGeneratedScenario(name) {
  const definition = SCENARIO_DEFINITIONS[name];
  if (!definition || definition.fixture !== 'generated') {
    throw new Error(`scenario does not have a generated fixture: ${name}`);
  }
  const root = mkdtempSync(path.join(os.tmpdir(), SCENARIO_DIRECTORY_PREFIX));
  writeSourceAnchor(root);
  switch (name) {
    case 'many-small':
      writeSmallRustSources(root, 192);
      break;
    case 'mixed-size': {
      writeSmallRustSources(root, 32);
      const records = Array.from({ length: 8192 }, (_, index) => ({ index, name: `entry-${index}` }));
      writeScenarioFile(root, 'config/large.json', `${JSON.stringify({ records })}\n`);
      break;
    }
    case 'structured-json':
      for (let index = 0; index < 64; index += 1) {
        writeScenarioFile(
          root,
          `schemas/service-${index}.json`,
          `${JSON.stringify({
            $schema: 'https://json-schema.org/draft/2020-12/schema',
            title: `Service${index}`,
            type: 'object',
            properties: { id: { type: 'string' }, enabled: { type: 'boolean' } },
          })}\n`,
        );
      }
      writeScenarioFile(
        root,
        'config/service.jsonc',
        '{\n  // benchmark JSONC configuration\n  "service": { "name": "graphoxide", },\n}\n',
      );
      writeScenarioFile(
        root,
        'config/features.json5',
        "{ enabled: true, labels: ['structured', 'benchmark'], }\n",
      );
      writeScenarioFile(
        root,
        'schemas/catalog.schema.json',
        `${JSON.stringify({
          $schema: 'https://json-schema.org/draft/2020-12/schema',
          $id: 'https://benchmark.invalid/catalog.schema.json',
          type: 'object',
          properties: { services: { type: 'array', items: { type: 'string' } } },
        })}\n`,
      );
      writeScenarioFile(
        root,
        'api/openapi.json',
        `${JSON.stringify({
          openapi: '3.1.0',
          info: { title: 'Benchmark API', version: '1.0.0' },
          paths: { '/health': { get: { operationId: 'health' } } },
        })}\n`,
      );
      break;
    case 'cache-warm':
      writeSmallRustSources(root, 96, 2);
      break;
    case 'slow-io':
      writeSmallRustSources(root, 256);
      break;
    case 'large-graph':
      writeSmallRustSources(root, 256, 8);
      break;
    case 'structured-containers':
      writeScenarioFile(root, 'architecture.svg', '<svg xmlns="http://www.w3.org/2000/svg"><title>Architecture</title><g id="api"><text>API</text></g></svg>\n');
      writeScenarioFile(root, 'config/service.yaml', 'service:\n  name: graphoxide\n  ports: [443, 8443]\n');
      writeScenarioFile(root, 'config/runtime.toml', '[runtime]\nworkers = 4\n');
      writeScenarioFile(root, 'inventory/ports.csv', 'name,port\nhttps,443\n');
      writeScenarioFile(root, 'models/diagram.xml', '<model id="edge"><node id="api" /></model>\n');
      writeScenarioBytes(
        root,
        'archives/structured.zip',
        createStoredZip([
          { path: 'nested/diagram.svg', data: '<svg><title>nested</title></svg>\n' },
          { path: 'nested/service.yaml', data: 'service: nested\n' },
        ]),
      );
      const tar = createTar([
        { path: 'inventory/ports.csv', data: 'name,port\nhttps,443\n' },
        { path: 'metadata/runtime.toml', data: '[runtime]\nworkers = 4\n' },
      ]);
      writeScenarioBytes(root, 'archives/structured.tar', tar);
      writeScenarioBytes(root, 'archives/structured.tar.gz', createStoredGzip(tar));
      writeScenarioBytes(
        root,
        'architecture.svgz',
        createStoredGzip(Buffer.from('<svg><title>compressed architecture</title></svg>\n', 'utf8')),
      );
      break;
    case 'idl-schema':
      writeScenarioFile(root, 'idl/service.proto', 'syntax = "proto3"; package benchmark; message Request { string id = 1; } service Api { rpc Get(Request) returns (Request); }\n');
      writeScenarioFile(root, 'idl/model.fbs', 'namespace benchmark; table Request { id:string; } root_type Request;\n');
      writeScenarioFile(root, 'idl/service.thrift', 'namespace rs benchmark\nstruct Request { 1: string id }\n');
      writeScenarioFile(root, 'idl/service.capnp', '@0x9d9e7de1a1b2c3d4; struct Request { id @0 :Text; }\n');
      writeScenarioFile(root, 'idl/model.avsc', '{"type":"record","name":"Request","fields":[{"name":"id","type":"string"}]}\n');
      writeScenarioFile(root, 'idl/service.wit', 'package benchmark:api; interface service { get: func(id: string) -> string; }\n');
      writeScenarioFile(root, 'idl/service.smithy', '$version: "2"\nnamespace benchmark\nservice Api { version: "1.0" }\n');
      writeScenarioFile(root, 'idl/telemetry.yang', 'module telemetry { namespace "urn:benchmark"; prefix b; container telemetry { leaf enabled { type boolean; } } }\n');
      writeScenarioFile(root, 'idl/message.asn1', 'Benchmark DEFINITIONS ::= BEGIN Request ::= SEQUENCE { id UTF8String } END\n');
      writeScenarioFile(root, 'schema.graphql', 'type Query { request(id: ID!): Request } type Request { id: ID! }\n');
      writeScenarioFile(root, 'openapi.yaml', 'openapi: 3.1.0\ninfo: { title: Benchmark, version: 1.0.0 }\npaths: {}\n');
      writeScenarioFile(root, 'asyncapi.yaml', 'asyncapi: 3.0.0\ninfo: { title: Benchmark, version: 1.0.0 }\nchannels: {}\n');
      writeScenarioFile(root, 'schema.cddl', 'request = { id: tstr }\n');
      break;
    case 'diagrams':
      writeScenarioFile(root, 'architecture.dot', 'digraph G { api -> database [label="reads"]; }\n');
      writeScenarioFile(root, 'flow.mmd', 'flowchart LR\n  client --> api\n  api --> database\n');
      writeScenarioFile(root, 'sequence.puml', '@startuml\nclient -> api: request\napi --> client: response\n@enduml\n');
      writeScenarioFile(root, 'model.d2', 'client -> api: request\napi -> database: query\n');
      writeScenarioFile(root, 'drawio.drawio', '<mxfile><diagram id="benchmark">&lt;mxGraphModel/&gt;</diagram></mxfile>\n');
      writeScenarioFile(root, 'model.excalidraw', '{"type":"excalidraw","elements":[{"id":"api","type":"rectangle","text":"API"}]}\n');
      writeScenarioFile(root, 'model.tldr', '{"tldrawFileFormatVersion":1,"records":[{"id":"shape:api","typeName":"shape"}]}\n');
      writeScenarioFile(root, 'process.bpmn', '<definitions xmlns="http://www.omg.org/spec/BPMN/20100524/MODEL"><process id="benchmark"><startEvent id="start" /></process></definitions>\n');
      writeScenarioFile(root, 'model.dbml', 'Table services { id int [pk] name varchar }\n');
      writeScenarioFile(root, 'workspace.dsl', 'workspace "Benchmark" { model { softwareSystem "API" } }\n');
      break;
    case 'facility-models':
      writeScenarioFile(root, 'electrical.kicad_sch', '(kicad_sch (version 20231120) (generator graphoxide))\n');
      writeScenarioFile(root, 'electrical.sch', 'EESchema Schematic File Version 4\nLIBS:power\nEELAYER 29 0\n$EndSCHEMATC\n');
      writeScenarioFile(root, 'electrical.net', 'NETS\nNET GND U1.1 R1.2\n');
      writeScenarioFile(root, 'manufacturing.ipc', '<IPC-2581><Content><Step name="benchmark" /></Content></IPC-2581>\n');
      writeScenarioFile(root, 'facility.ifc', 'ISO-10303-21;\nHEADER;\nFILE_DESCRIPTION((\'benchmark\'),\'2;1\');\nENDSEC;\nDATA;\nENDSEC;\nEND-ISO-10303-21;\n');
      writeScenarioFile(root, 'facility.ids', '<ids xmlns="http://standards.buildingsmart.org/IDS"><info><title>benchmark</title></info></ids>\n');
      writeScenarioFile(root, 'facility.gbxml', '<gbXML><Campus id="benchmark" /></gbXML>\n');
      writeScenarioFile(root, 'facility.citygml', '<CityModel xmlns="http://www.opengis.net/citygml/2.0" />\n');
      writeScenarioFile(root, 'facility.landxml', '<LandXML version="1.2"><Project name="benchmark" /></LandXML>\n');
      writeScenarioFile(root, 'thermal.idf', 'Version, 24.1;\nBuilding, Benchmark;\n');
      writeScenarioFile(root, 'thermal.mo', 'model Benchmark end Benchmark;\n');
      writeScenarioFile(root, 'flow.controlDict', 'application simpleFoam;\n');
      writeScenarioFile(root, 'infrastructure.openconfig.yaml', 'openconfig-interfaces:interfaces:\n  interface:\n    - name: Ethernet1\n');
      writeScenarioFile(root, 'redfish.json', '{"@odata.type":"#ComputerSystem.v1_0_0.ComputerSystem","Id":"Server-1"}\n');
      writeScenarioFile(root, 'building.ifcxml', '<ifcXML xmlns="http://www.buildingsmart-tech.org/ifcXML/IFC4/final" />\n');
      break;
    case 'openusd-assets':
      writeScenarioFile(root, 'scene.usda', '#usda 1.0\ndef Xform "World" { def Xform "Robot" {} }\n');
      writeScenarioBytes(root, 'scene.usdc', Buffer.from('PXR-USDC\x00\x08\x00\x00\x00benchmark', 'binary'));
      writeScenarioBytes(
        root,
        'scene.usdz',
        createStoredZip([{ path: 'scene.usda', data: '#usda 1.0\ndef Xform "World" {}\n' }]),
      );
      writeScenarioFile(root, 'robot.urdf', '<robot name="benchmark"><link name="base" /></robot>\n');
      writeScenarioFile(root, 'world.sdf', '<sdf version="1.9"><world name="benchmark" /></sdf>\n');
      writeScenarioFile(root, 'robot.mjcf', '<mujoco model="benchmark"><worldbody /></mujoco>\n');
      writeScenarioFile(root, 'asset.gltf', '{"asset":{"version":"2.0","generator":"graphoxide"},"scenes":[{}]}\n');
      writeScenarioBytes(
        root,
        'asset.glb',
        createGlb({ asset: { version: '2.0', generator: 'graphoxide' }, scenes: [{}] }),
      );
      writeScenarioFile(root, 'material.mtlx', '<materialx version="1.38"><nodegraph name="benchmark" /></materialx>\n');
      writeScenarioFile(root, 'road.xodr', '<OpenDRIVE><header name="benchmark" /></OpenDRIVE>\n');
      writeScenarioFile(root, 'scenario.xosc', '<OpenSCENARIO><FileHeader description="benchmark" /></OpenSCENARIO>\n');
      writeScenarioFile(root, 'model.fmu.json', '{"fmiVersion":"3.0","modelName":"benchmark"}\n');
      break;
    case 'catalog-wiki':
      {
        const runbook =
          '---\ntitle: Runbook\nsources:\n  - source-one#capture-active\n  - source-one#capture-history\n---\n\n# Runbook\n\nThe wiki links to the service catalog.\n';
        const active =
          '# Active Capture\n\n## Full Derived Knowledge\n\nThe full extracted graph text stays available at the configured 4GB cap.\n';
        writeScenarioFile(root, 'docs/runbook.md', runbook);
        writeScenarioFile(root, 'raw/active.md', active);
        writeScenarioFile(
          root,
          'wiki.json',
          '{"version":1,"roots":["docs"],"exclude":["docs/drafts"],"required_frontmatter":["title","sources"],"output":"llms.txt"}\n',
        );
        writeScenarioFile(
          root,
          'provenance/catalog.json',
          `${JSON.stringify({
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
                sha256: createHash('sha256').update(active).digest('hex'),
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
          })}\n`,
        );
      }
      writeScenarioFile(root, 'metadata/services.json', '{"service":"wiki","annotation":"catalog-only annotation"}\n');
      writeScenarioFile(root, 'metadata/services.yaml', 'service: wiki\nowner: graphoxide\n');
      writeScenarioFile(root, 'metadata/catalog-only-metadata.json', '{"annotation":"catalog-only metadata edit"}\n');
      writeScenarioFile(root, 'metadata/malformed.json', '{ not valid JSON }\n');
      writeScenarioFile(root, '.env', 'TOKEN=not-for-indexing\n');
      writeScenarioFile(root, '.graphoxideignore', 'ignored/\nprovenance/\n');
      writeScenarioFile(root, 'ignored/private.md', 'Ignored by fixture policy.\n');
      writeScenarioBytes(root, 'documents/guide.pdf', createMinimalPdf());
      writeScenarioBytes(
        root,
        'documents/catalog.docx',
        createStoredZip([
          { path: '[Content_Types].xml', data: '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"/>\n' },
          { path: 'word/document.xml', data: '<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"/>\n' },
        ]),
      );
      writeScenarioBytes(
        root,
        'archives/wiki-only.zip',
        createStoredZip([{ path: 'members/wiki-only.md', data: '# Archive-only wiki record\n' }]),
      );
      break;
    default:
      throw new Error(`no generated fixture writer for scenario: ${name}`);
  }
  return root;
}

function materializeScenario(options) {
  if (options.scenario === 'custom-fixture' || options.scenario === 'compat-language-matrix') {
    return {
      fixture: options.fixture,
      generated: false,
      scenario: describeScenario(options.fixture, options.scenario),
      cleanup: () => {},
    };
  }
  const fixture = materializeGeneratedScenario(options.scenario);
  return {
    fixture,
    generated: true,
    scenario: profileForScenario(options.scenario),
    cleanup: () => rmSync(fixture, { recursive: true, force: false }),
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

function binaryIdentity(metadata) {
  return {
    dev: metadata.dev,
    ino: metadata.ino,
    size: metadata.size,
    mtime_ms: metadata.mtimeMs,
    mode: metadata.mode,
  };
}

function pinBinary(binary) {
  let before;
  try {
    before = lstatSync(binary);
  } catch (error) {
    if (error?.code === 'ENOENT') {
      throw new Error(
        `Graphoxide binary not found at ${binary}; run npm run benchmark:graph-build or build the release binary first`,
      );
    }
    throw error;
  }
  if (
    before.isSymbolicLink() ||
    !before.isFile() ||
    before.nlink !== 1 ||
    !Number.isSafeInteger(before.size) ||
    before.size < 0 ||
    before.size > MAX_BINARY_BYTES
  ) {
    throw new Error(`Graphoxide binary must be a single-link regular file within ${MAX_BINARY_BYTES} bytes`);
  }
  if (process.platform !== 'win32' && (before.mode & 0o111) === 0) {
    throw new Error(`Graphoxide binary is not executable: ${binary}`);
  }
  const descriptor = openSync(binary, fsConstants.O_RDONLY | (fsConstants.O_NOFOLLOW ?? 0));
  try {
    const opened = fstatSync(descriptor);
    if (!sameIdentity(before, opened) || !opened.isFile() || opened.nlink !== 1) {
      throw new Error(`Graphoxide binary changed identity while opening: ${binary}`);
    }
    const digest = createHash('sha256');
    const buffer = Buffer.allocUnsafe(1024 * 1024);
    let total = 0;
    while (true) {
      const count = readSync(descriptor, buffer, 0, buffer.length, null);
      if (count === 0) break;
      total += count;
      if (!Number.isSafeInteger(total) || total > MAX_BINARY_BYTES) {
        throw new Error(`Graphoxide binary exceeds its ${MAX_BINARY_BYTES}-byte ceiling: ${binary}`);
      }
      digest.update(buffer.subarray(0, count));
    }
    const after = fstatSync(descriptor);
    const pathAfter = lstatSync(binary);
    if (
      !sameIdentity(opened, after) ||
      !sameIdentity(opened, pathAfter) ||
      after.size !== total ||
      pathAfter.isSymbolicLink() ||
      pathAfter.nlink !== 1
    ) {
      throw new Error(`Graphoxide binary changed while being pinned: ${binary}`);
    }
    return { path: binary, sha256: digest.digest('hex'), identity: binaryIdentity(after) };
  } finally {
    closeSync(descriptor);
  }
}

function verifyPinnedBinary(pin, phase) {
  const observed = pinBinary(pin.path);
  if (
    pin.sha256 !== observed.sha256 ||
    pin.identity.dev !== observed.identity.dev ||
    pin.identity.ino !== observed.identity.ino ||
    pin.identity.size !== observed.identity.size
  ) {
    throw new Error(`Graphoxide binary changed ${phase}`);
  }
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
  if (report.build !== undefined) {
    if (report.build === null || Array.isArray(report.build) || typeof report.build !== 'object') {
      throw new Error(`${commandName} JSON build must be an object`);
    }
    report = report.build;
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

export function parseRuntimeTelemetry(stdout, commandName = 'graphoxide runtime telemetry') {
  const text = String(stdout).trim();
  if (!text) throw new Error(`${commandName} is empty`);
  let report;
  try {
    report = JSON.parse(text);
  } catch (error) {
    throw new Error(`${commandName} is invalid JSON: ${error.message}`);
  }
  if (report === null || Array.isArray(report) || typeof report !== 'object') {
    throw new Error(`${commandName} JSON must be an object`);
  }
  return report;
}

export function validateRuntimeTelemetry(report, expected, commandName = 'graphoxide runtime telemetry') {
  if (report.schema_version !== INDEX_RUNTIME_TELEMETRY_SCHEMA_VERSION) {
    throw new Error(
      `${commandName} must report schema_version=${INDEX_RUNTIME_TELEMETRY_SCHEMA_VERSION} (received ${String(
        report.schema_version,
      )})`,
    );
  }
  if (report.build === null || Array.isArray(report.build) || typeof report.build !== 'object') {
    throw new Error(`${commandName} must contain a build report object`);
  }
  validateCliReport(report.build, expected, `${commandName} build`);
  if (report.runtime === null || Array.isArray(report.runtime) || typeof report.runtime !== 'object') {
    throw new Error(`${commandName} must contain a runtime configuration object`);
  }
  for (const field of [
    'execution_model',
    'io_backend',
    'io_backend_request',
    'io_backend_fallback',
    'memory_budget_bytes',
    'io_workers',
    'compute_workers',
    'read_batch_bytes',
    'cache_partitions',
    'admission',
  ]) {
    if (!(field in report.runtime)) {
      throw new Error(`${commandName} runtime configuration is missing ${field}`);
    }
  }
  if (report.runtime.execution_model !== 'isolated') {
    throw new Error(
      `${commandName} must run through execution_model=isolated (received ${String(
        report.runtime.execution_model,
      )})`,
    );
  }
  if (!['auto', 'threaded', 'io_uring'].includes(report.runtime.io_backend_request)) {
    throw new Error(`${commandName} must record a valid isolated I/O backend request`);
  }
  if (
    report.runtime.io_backend_fallback !== null &&
    typeof report.runtime.io_backend_fallback !== 'string'
  ) {
    throw new Error(`${commandName} must record an I/O backend fallback as a string or null`);
  }
  for (const field of [
    'memory_budget_bytes',
    'io_workers',
    'compute_workers',
    'read_batch_bytes',
  ]) {
    if (!Number.isSafeInteger(report.runtime[field]) || report.runtime[field] <= 0) {
      throw new Error(`${commandName} runtime configuration has invalid ${field}`);
    }
  }
  const admission = report.runtime.admission;
  if (admission === null || Array.isArray(admission) || typeof admission !== 'object') {
    throw new Error(`${commandName} must contain isolated admission evidence`);
  }
  for (const field of [
    'admitted_requests',
    'effective_io_workers',
    'effective_compute_workers',
    'effective_read_batch_bytes',
    'io_pool_bytes_per_worker',
    'io_buffers_bytes',
    'ready_inputs_bytes',
    'cpu_arenas_bytes',
    'cache_and_runs_bytes',
    'query_reserve_bytes',
    'emergency_reserve_bytes',
  ]) {
    if (!Number.isSafeInteger(admission[field]) || admission[field] < 0) {
      throw new Error(`${commandName} admission evidence has invalid ${field}`);
    }
  }
  if (admission.admitted_requests < 1) {
    throw new Error(`${commandName} admission evidence must describe at least one request`);
  }
  if (
    admission.effective_io_workers < 1 ||
    admission.effective_compute_workers < 1 ||
    admission.effective_read_batch_bytes < 1 ||
    admission.io_pool_bytes_per_worker < 1
  ) {
    throw new Error(`${commandName} admission evidence must retain runnable pools`);
  }
  if (
    admission.effective_io_workers > report.runtime.io_workers ||
    admission.effective_compute_workers > report.runtime.compute_workers ||
    admission.effective_read_batch_bytes > report.runtime.read_batch_bytes ||
    admission.effective_read_batch_bytes > admission.io_pool_bytes_per_worker
  ) {
    throw new Error(`${commandName} admission evidence exceeds its configured bounds`);
  }
  const cache = report.cache;
  if (cache === null || Array.isArray(cache) || typeof cache !== 'object') {
    throw new Error(`${commandName} must contain runtime cache telemetry`);
  }
  if (typeof cache.enabled !== 'boolean') {
    throw new Error(`${commandName} cache telemetry has invalid enabled`);
  }
  for (const field of [
    'metadata_hits',
    'runtime_hits',
    'legacy_hits',
    'misses',
    'bypasses',
    'stale_or_corrupt',
    'probe_failures',
    'payload_reads_avoided',
    'parses_avoided',
    'stores',
    'already_present',
    'store_failures',
  ]) {
    if (!Number.isSafeInteger(cache[field]) || cache[field] < 0) {
      throw new Error(`${commandName} cache telemetry has invalid ${field}`);
    }
  }
  const expectedParsesAvoided =
    cache.metadata_hits + cache.runtime_hits + cache.legacy_hits;
  if (cache.parses_avoided !== expectedParsesAvoided) {
    throw new Error(
      `${commandName} cache telemetry parses_avoided does not match its hit counters`,
    );
  }
  if (cache.payload_reads_avoided !== cache.metadata_hits) {
    throw new Error(
      `${commandName} cache telemetry payload_reads_avoided does not match metadata_hits`,
    );
  }
  if (report.simd === null || Array.isArray(report.simd) || typeof report.simd !== 'object') {
    throw new Error(`${commandName} must contain SIMD telemetry`);
  }
  if (
    typeof report.simd.architecture !== 'string' ||
    !Array.isArray(report.simd.detected_features) ||
    !Array.isArray(report.simd.enabled_kernels)
  ) {
    throw new Error(`${commandName} SIMD telemetry has an invalid shape`);
  }
  const io = report.io;
  if (io === null || Array.isArray(io) || typeof io !== 'object') {
    throw new Error(`${commandName} must contain source I/O telemetry`);
  }
  for (const field of [
    'sources_selected',
    'source_bytes_selected',
    'sources_read',
    'source_bytes_read',
    'sources_delivered',
    'source_bytes_delivered',
    'source_bytes_avoided',
    'read_failures',
    'peak_ready_bytes',
    'peak_ready_items',
  ]) {
    if (!Number.isSafeInteger(io[field]) || io[field] < 0) {
      throw new Error(`${commandName} source I/O telemetry has invalid ${field}`);
    }
  }
  if (
    io.sources_delivered !== io.sources_read ||
    io.sources_selected !== io.sources_read + io.read_failures + cache.metadata_hits ||
    io.source_bytes_delivered > io.source_bytes_read ||
    io.source_bytes_delivered + io.source_bytes_avoided > io.source_bytes_selected
  ) {
    throw new Error(`${commandName} source I/O telemetry is inconsistent`);
  }
  if (
    report.work === null ||
    Array.isArray(report.work) ||
    typeof report.work !== 'object' ||
    !Number.isSafeInteger(report.work.parses) ||
    report.work.parses < 0 ||
    report.work.parses > io.sources_delivered
  ) {
    throw new Error(`${commandName} parser work telemetry is invalid`);
  }
  for (const field of [
    'payload_bytes_read',
    'payload_bytes_written',
    'artifact_bytes_read',
    'artifact_bytes_written',
    'peak_in_flight_transfer_bytes',
  ]) {
    if (!Number.isSafeInteger(cache[field]) || cache[field] < 0) {
      throw new Error(`${commandName} cache telemetry has invalid ${field}`);
    }
  }
  const processTelemetry = report.process;
  if (
    processTelemetry === null ||
    Array.isArray(processTelemetry) ||
    typeof processTelemetry !== 'object' ||
    (!Number.isSafeInteger(processTelemetry.peak_rss_bytes) &&
      processTelemetry.peak_rss_bytes !== null) ||
    !['getrusage_maxrss_bytes', 'getrusage_maxrss_kib', 'unavailable'].includes(
      processTelemetry.peak_rss_source,
    ) ||
    (processTelemetry.peak_rss_source === 'unavailable') !==
      (processTelemetry.peak_rss_bytes === null)
  ) {
    throw new Error(`${commandName} process telemetry has an invalid shape`);
  }
  return report;
}

export function readRuntimeTelemetry(runtimeReport, expected, commandName = 'graphoxide command') {
  let bytes;
  try {
    bytes = readFileSync(runtimeReport);
  } catch (error) {
    if (error?.code === 'ENOENT') {
      throw new Error(`${commandName} did not write requested runtime telemetry: ${runtimeReport}`);
    }
    throw error;
  }
  return validateRuntimeTelemetry(
    parseRuntimeTelemetry(bytes, `${commandName} runtime telemetry`),
    expected,
    `${commandName} runtime telemetry`,
  );
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

export function runCliJson(binary, args, cwd, expected, clock = () => performance.now(), binaryPin) {
  const commandName = `graphoxide ${args[0]}`;
  if (binaryPin) verifyPinnedBinary(binaryPin, `before ${commandName}`);
  const started = clock();
  const result = spawnSync(binary, args, {
    cwd,
    encoding: 'utf8',
    env: sanitizedEnvironment(),
    maxBuffer: MAX_COMMAND_OUTPUT_BYTES,
  });
  const externalWallMs = clock() - started;
  if (binaryPin) verifyPinnedBinary(binaryPin, `after ${commandName}`);
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

function runCliText(binary, args, cwd, clock = () => performance.now(), binaryPin) {
  const commandName = `graphoxide ${args.join(' ')}`;
  if (binaryPin) verifyPinnedBinary(binaryPin, `before ${commandName}`);
  const started = clock();
  const result = spawnSync(binary, args, {
    cwd,
    encoding: 'utf8',
    env: sanitizedEnvironment(),
    maxBuffer: MAX_COMMAND_OUTPUT_BYTES,
  });
  const externalWallMs = clock() - started;
  if (binaryPin) verifyPinnedBinary(binaryPin, `after ${commandName}`);
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
  return {
    external_wall_ms: roundMilliseconds(externalWallMs),
    stdout: String(result.stdout),
  };
}

function openMutationTarget(project, hooks) {
  const preferred = path.join(project, ...MUTATION_TARGET.split('/'));
  hooks.beforeOpen?.(preferred);
  try {
    return { descriptor: openMutationCandidate(preferred), target: preferred };
  } catch (error) {
    if (error?.code !== 'ENOENT') throw error;
  }
  const entries = collectCorpusEntries(project);
  if (entries.some(({ relative }) => relative === MUTATION_TARGET)) {
    throw new Error(`preferred benchmark mutation target could not be opened safely: ${MUTATION_TARGET}`);
  }
  const fallback = entries.find(
    ({ absolute, entry }) =>
      entry.isFile() && SOURCE_EXTENSIONS.has(path.extname(absolute).toLowerCase()),
  );
  if (!fallback) {
    throw new Error('fixture contains no source file with a built-in deterministic mutation strategy');
  }
  hooks.beforeOpen?.(fallback.absolute);
  return { descriptor: openMutationCandidate(fallback.absolute), target: fallback.absolute };
}

function openMutationCandidate(target) {
  // A pre-open stat would recreate the check/use gap this helper is meant to
  // close. The descriptor metadata and canonical path checks are authoritative.
  return openSync(target, fsConstants.O_RDWR | (fsConstants.O_NOFOLLOW ?? 0));
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

function sameIdentity(left, right) {
  return left.dev === right.dev && left.ino === right.ino;
}

function validateMutationDescriptor(metadata, opened, label) {
  if (
    !opened.isFile() ||
    opened.nlink !== 1 ||
    !Number.isSafeInteger(opened.size) ||
    opened.size < 0 ||
    !sameIdentity(metadata, opened)
  ) {
    throw new Error(`benchmark mutation target ${label}`);
  }
}

function sameMutationSnapshot(left, right) {
  return (
    sameIdentity(left, right) &&
    left.size === right.size &&
    left.nlink === right.nlink &&
    left.mtimeMs === right.mtimeMs &&
    left.ctimeMs === right.ctimeMs
  );
}

function isPathBelow(directory, candidate) {
  const relative = path.relative(directory, candidate);
  return (
    relative !== '' &&
    relative !== '..' &&
    !relative.startsWith(`..${path.sep}`) &&
    !path.isAbsolute(relative)
  );
}

function validateMutationPath(
  project,
  projectIdentity,
  target,
  descriptor,
  expected,
  label,
) {
  const descriptorBefore = fstatSync(descriptor);
  validateMutationDescriptor(expected, descriptorBefore, `${label} through its descriptor`);
  const targetCanonical = realpathSync(target);
  const pathMetadata = lstatSync(target);
  const projectAfter = lstatSync(project);
  const descriptorAfter = fstatSync(descriptor);
  if (
    targetCanonical !== target ||
    !isPathBelow(project, targetCanonical) ||
    projectAfter.isSymbolicLink() ||
    !projectAfter.isDirectory() ||
    !sameIdentity(projectIdentity, projectAfter) ||
    pathMetadata.isSymbolicLink() ||
    !pathMetadata.isFile() ||
    pathMetadata.nlink !== 1 ||
    !sameMutationSnapshot(expected, pathMetadata) ||
    !sameMutationSnapshot(expected, descriptorBefore) ||
    !sameMutationSnapshot(descriptorBefore, descriptorAfter)
  ) {
    throw new Error(`benchmark mutation target ${label}`);
  }
}

function hashMutationDescriptor(descriptor, metadata, label) {
  const digest = createHash('sha256');
  const buffer = Buffer.allocUnsafe(1024 * 1024);
  let position = 0;
  while (position < metadata.size) {
    const count = readSync(
      descriptor,
      buffer,
      0,
      Math.min(buffer.length, metadata.size - position),
      position,
    );
    if (count === 0) throw new Error(`benchmark mutation target ended while ${label}`);
    digest.update(buffer.subarray(0, count));
    position += count;
  }
  const after = fstatSync(descriptor);
  validateMutationDescriptor(metadata, after, `changed while ${label}`);
  if (after.size !== position || !sameMutationSnapshot(metadata, after)) {
    throw new Error(`benchmark mutation target changed while ${label}`);
  }
  return digest.digest('hex');
}

export function mutateCopiedFixture(project, hooks = {}) {
  const canonicalProject = realpathSync(path.resolve(project));
  const { descriptor, target } = openMutationTarget(canonicalProject, hooks);
  try {
    const opened = fstatSync(descriptor);
    const projectIdentity = lstatSync(canonicalProject);
    if (projectIdentity.isSymbolicLink() || !projectIdentity.isDirectory()) {
      throw new Error('benchmark fixture project must be a canonical directory');
    }
    validateMutationDescriptor(
      opened,
      opened,
      'must be a single-link regular file with a supported size',
    );
    validateMutationPath(
      canonicalProject,
      projectIdentity,
      target,
      descriptor,
      opened,
      'escaped its canonical project before mutation',
    );
    const before = hashMutationDescriptor(descriptor, opened, 'hashing its original bytes');

    hooks.beforeMutation?.(target);
    const beforeMutation = fstatSync(descriptor);
    validateMutationDescriptor(opened, beforeMutation, 'changed before mutation');
    validateMutationPath(
      canonicalProject,
      projectIdentity,
      target,
      descriptor,
      opened,
      'path changed before mutation',
    );

    const mutation = Buffer.from(mutationText(target), 'utf8');
    if (!Number.isSafeInteger(opened.size + mutation.length)) {
      throw new Error('benchmark mutation target would exceed the supported file size');
    }
    let offset = 0;
    while (offset < mutation.length) {
      const written = writeSync(
        descriptor,
        mutation,
        offset,
        mutation.length - offset,
        opened.size + offset,
      );
      if (written === 0) throw new Error('benchmark fixture mutation write made no progress');
      offset += written;
    }
    fsyncSync(descriptor);
    // Some filesystems expose coarse timestamp precision. Move mtime far enough
    // forward that the incremental detector must observe the content change.
    const changedMtime = new Date(Math.max(Date.now(), opened.mtimeMs + 2_000));
    futimesSync(descriptor, opened.atime, changedMtime);
    fsyncSync(descriptor);

    const mutated = fstatSync(descriptor);
    validateMutationDescriptor(opened, mutated, 'changed identity during mutation');
    if (mutated.size !== opened.size + mutation.length) {
      throw new Error('benchmark fixture mutation produced an unexpected file size');
    }
    const after = hashMutationDescriptor(descriptor, mutated, 'hashing its mutated bytes');
    hooks.afterMutation?.(target);
    validateMutationPath(
      canonicalProject,
      projectIdentity,
      target,
      descriptor,
      mutated,
      'path changed during mutation',
    );
    if (before === after) throw new Error('benchmark fixture mutation did not change the source file');
    return {
      path: path.relative(canonicalProject, target).split(path.sep).join('/'),
      sha256_before: before,
      sha256_after: after,
    };
  } finally {
    closeSync(descriptor);
  }
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

function readJsonArtifact(file, name) {
  let bytes;
  try {
    bytes = readFileSync(file);
  } catch (error) {
    if (error?.code === 'ENOENT') throw new Error(`benchmark ${name} was not written: ${file}`);
    throw error;
  }
  let value;
  try {
    value = JSON.parse(bytes.toString('utf8'));
  } catch (error) {
    throw new Error(`benchmark ${name} is not valid JSON: ${error.message}`);
  }
  if (value === null || Array.isArray(value) || typeof value !== 'object') {
    throw new Error(`benchmark ${name} must be a JSON object`);
  }
  return { bytes, value };
}

export function verifyBuildArtifacts(project, cliReport, commandName) {
  const output = path.join(project, 'graphoxide-out');
  const graph = readJsonArtifact(path.join(output, 'graph.json'), `${commandName} graph`);
  const manifest = readJsonArtifact(path.join(output, 'manifest.json'), `${commandName} manifest`);
  if (!Array.isArray(graph.value.nodes) || !Array.isArray(graph.value.links)) {
    throw new Error(`${commandName} graph must contain nodes and links arrays`);
  }
  if (cliReport.graph?.nodes !== graph.value.nodes.length) {
    throw new Error(
      `${commandName} graph node count differs from its machine-readable build report`,
    );
  }
  if (cliReport.graph?.edges !== graph.value.links.length) {
    throw new Error(
      `${commandName} graph edge count differs from its machine-readable build report`,
    );
  }
  return {
    graph_sha256: createHash('sha256').update(graph.bytes).digest('hex'),
    manifest_sha256: createHash('sha256').update(manifest.bytes).digest('hex'),
    nodes: graph.value.nodes.length,
    edges: graph.value.links.length,
  };
}

export function runSample({ binary, binaryPin, fixture, run }) {
  const runDirectory = mkdtempSync(path.join(os.tmpdir(), RUN_DIRECTORY_PREFIX));
  const project = path.join(runDirectory, 'project');
  try {
    assertIsolatedBenchmarkArgs(FULL_BUILD_ARGS);
    assertIsolatedBenchmarkArgs(INCREMENTAL_UPDATE_ARGS);
    cpSync(fixture, project, { recursive: true, errorOnExist: true, preserveTimestamps: true });
    const fullBuildRuntimeReport = path.join(
      project,
      'graphoxide-out',
      'benchmark-runtime-extract.json',
    );
    const fullBuildExpected = { operation: 'extract', mode: 'full', status: 'rebuilt' };
    const fullBuild = runCliJson(
      binary,
      [...FULL_BUILD_ARGS, '--runtime-report', fullBuildRuntimeReport],
      project,
      fullBuildExpected,
      undefined,
      binaryPin,
    );
    const fullBuildArtifact = verifyBuildArtifacts(project, fullBuild.cli_report, 'full build');
    const mutation = mutateCopiedFixture(project);
    const incrementalRuntimeReport = path.join(
      project,
      'graphoxide-out',
      'benchmark-runtime-update.json',
    );
    const incrementalExpected = {
      operation: 'update',
      mode: 'incremental',
      status: 'rebuilt',
      changed: 1,
      processed: 1,
    };
    const incrementalUpdate = runCliJson(
      binary,
      [...INCREMENTAL_UPDATE_ARGS, '--runtime-report', incrementalRuntimeReport],
      project,
      incrementalExpected,
      undefined,
      binaryPin,
    );
    const incrementalArtifact = verifyBuildArtifacts(
      project,
      incrementalUpdate.cli_report,
      'incremental update',
    );
    if (incrementalArtifact.graph_sha256 === fullBuildArtifact.graph_sha256) {
      throw new Error('deterministic source mutation did not change graph.json');
    }
    return {
      run,
      mutation,
      full_build: {
        ...fullBuild,
        artifacts: fullBuildArtifact,
        runtime_telemetry: readRuntimeTelemetry(
          fullBuildRuntimeReport,
          fullBuildExpected,
          'graphoxide extract',
        ),
      },
      incremental_update: {
        ...incrementalUpdate,
        artifacts: incrementalArtifact,
        runtime_telemetry: readRuntimeTelemetry(
          incrementalRuntimeReport,
          incrementalExpected,
          'graphoxide update',
        ),
      },
    };
  } finally {
    cleanupRunDirectory(runDirectory);
  }
}

function removeGraphArtifact(project) {
  const graph = path.join(project, 'graphoxide-out', 'graph.json');
  const metadata = lstatSync(graph);
  if (!metadata.isFile() || metadata.isSymbolicLink()) {
    throw new Error(`benchmark graph artifact is not a regular file: ${graph}`);
  }
  unlinkSync(graph);
}

function mutateCatalogOnly(project, location = 'fixtures/catalog-wiki/docs/runbook-catalog-only.md') {
  const catalog = path.join(project, 'provenance', 'catalog.json');
  const before = sha256File(catalog);
  const value = JSON.parse(readFileSync(catalog, 'utf8'));
  const source = value?.sources?.[0];
  if (!source || typeof source.location !== 'string') {
    throw new Error('catalog/wiki fixture has no mutable catalog location');
  }
  source.location = location;
  writeFileSync(catalog, `${JSON.stringify(value)}\n`);
  const after = sha256File(catalog);
  if (after === before) throw new Error('catalog-only mutation did not change catalog bytes');
  return { sha256_before: before, sha256_after: after, location: source.location };
}

function mutateActiveCatalogCapture(project) {
  const source = path.join(project, 'raw', 'active.md');
  const catalog = path.join(project, 'provenance', 'catalog.json');
  writeFileSync(source, `${readFileSync(source, 'utf8')}\nActive capture revision.\n`);
  const value = JSON.parse(readFileSync(catalog, 'utf8'));
  const sourceRecord = value?.sources?.[0];
  const capture = value?.captures?.find((candidate) => candidate?.capture_id === sourceRecord?.active_capture_id);
  if (!capture || typeof capture.sha256 !== 'string') {
    throw new Error('catalog/wiki fixture has no active capture');
  }
  capture.sha256 = sha256File(source);
  capture.updated_at = '2026-08-24T00:01:00Z';
  writeFileSync(catalog, `${JSON.stringify(value)}\n`);
  return { path: 'raw/active.md', sha256: capture.sha256 };
}

function assertCatalogAnnotation(project, location) {
  const graph = readJsonArtifact(path.join(project, 'graphoxide-out', 'graph.json'), 'catalog graph');
  if (
    !graph.value.nodes.some(
      (node) =>
        node?.source_file === 'raw/active.md' &&
        node?.catalog?.source_path === 'raw/active.md' &&
        node.catalog.location === location,
    )
  ) {
    throw new Error('catalog/wiki annotation was not applied to the runbook graph node');
  }
}

function assertActiveCaptureDerivedKnowledge(project) {
  const graph = readJsonArtifact(path.join(project, 'graphoxide-out', 'graph.json'), 'catalog graph');
  if (
    graph.value.nodes.some(
      (node) =>
        node?.source_file === 'raw/history.md' || node?.catalog?.capture_id === 'capture-history',
    )
  ) {
    throw new Error('catalog/wiki graph retained an absent historical capture');
  }
  if (
    !graph.value.nodes.some(
      (node) =>
        node?.source_file === 'raw/active.md' &&
        node?.label === 'Full Derived Knowledge' &&
        node?.catalog?.capture_id === 'capture-active',
    )
  ) {
    throw new Error('catalog/wiki graph omitted active capture derived knowledge');
  }
}

function assertCatalogWikiCoverage(project) {
  const graph = readJsonArtifact(path.join(project, 'graphoxide-out', 'graph.json'), 'catalog graph');
  const coverage = readJsonArtifact(path.join(project, 'graphoxide-out', 'coverage.json'), 'catalog coverage');
  if (!Array.isArray(graph.value.nodes) || !Array.isArray(coverage.value.files)) {
    throw new Error('catalog/wiki graph or coverage has an invalid shape');
  }
  const byPath = new Map();
  for (const file of coverage.value.files) {
    if (!file || typeof file.path !== 'string' || byPath.has(file.path)) {
      throw new Error('catalog/wiki coverage has an invalid or duplicate file outcome');
    }
    byPath.set(file.path, file);
  }
  const outcome = (file, expectedStatus, expectedFormat) => {
    const actual = byPath.get(file);
    if (actual?.status !== expectedStatus || (expectedFormat && actual.format_id !== expectedFormat)) {
      throw new Error(`catalog/wiki coverage outcome for ${file} is incorrect`);
    }
  };
  outcome('.env', 'excluded_sensitive');
  outcome('metadata/malformed.json', 'covered', 'json');
  outcome('archives/wiki-only.zip', 'covered', 'zip-archive');
  if (byPath.has('ignored/private.md')) {
    throw new Error('catalog/wiki ignored input unexpectedly appears in coverage');
  }
  const sourceFiles = new Set(
    graph.value.nodes.map((node) => node?.source_file).filter((source) => typeof source === 'string'),
  );
  for (const source of ['metadata/malformed.json', 'archives/wiki-only.zip!/members/wiki-only.md']) {
    if (!sourceFiles.has(source)) {
      throw new Error(`catalog/wiki graph omitted required source ${source}`);
    }
  }
  for (const source of ['.env', 'ignored/private.md']) {
    if (sourceFiles.has(source)) {
      throw new Error(`catalog/wiki graph included excluded source ${source}`);
    }
  }
}

function describeCacheTree(project) {
  return describeFixture(path.join(project, 'graphoxide-out', 'cache'));
}

function catalogIndexPass(binary, binaryPin, project, label, expected) {
  const runtimeReport = path.join(project, 'graphoxide-out', `benchmark-runtime-${label}.json`);
  const build = runCliJson(
    binary,
    [...CATALOG_WIKI_BUILD_ARGS, '--runtime-report', runtimeReport],
    project,
    expected,
    undefined,
    binaryPin,
  );
  return {
    ...build,
    artifacts: verifyBuildArtifacts(project, build.cli_report, label),
    runtime_telemetry: readRuntimeTelemetry(runtimeReport, expected, `graphoxide ${label}`),
  };
}

function runCatalogWikiSample({ binary, binaryPin, fixture, run }) {
  const runDirectory = mkdtempSync(path.join(os.tmpdir(), RUN_DIRECTORY_PREFIX));
  const project = path.join(runDirectory, 'project');
  try {
    assertIsolatedBenchmarkArgs(CATALOG_WIKI_BUILD_ARGS);
    cpSync(fixture, project, { recursive: true, errorOnExist: true, preserveTimestamps: true });

    const fullBuildExpected = { operation: 'index', mode: 'full', status: 'rebuilt' };
    const fullBuild = catalogIndexPass(binary, binaryPin, project, 'catalog-cold', fullBuildExpected);
    assertCatalogWikiCoverage(project);
    assertCatalogAnnotation(project, 'Site/Library/Folder/Page');
    assertActiveCaptureDerivedKnowledge(project);

    removeGraphArtifact(project);
    const warmBuild = catalogIndexPass(binary, binaryPin, project, 'catalog-warm', fullBuildExpected);
    if (
      warmBuild.artifacts.graph_sha256 !== fullBuild.artifacts.graph_sha256 ||
      warmBuild.artifacts.manifest_sha256 !== fullBuild.artifacts.manifest_sha256
    ) {
      throw new Error('warm catalog build changed graph or manifest bytes');
    }
    if (warmBuild.runtime_telemetry.cache.parses_avoided < 1) {
      throw new Error('warm catalog build reported no avoided parses');
    }

    const mutation = mutateActiveCatalogCapture(project);
    const incrementalExpected = {
      operation: 'index',
      mode: 'incremental',
      status: 'rebuilt',
      changed: 1,
      processed: 1,
    };
    const incrementalUpdate = catalogIndexPass(binary, binaryPin, project, 'catalog-incremental', incrementalExpected);
    if (
      incrementalUpdate.artifacts.graph_sha256 === fullBuild.artifacts.graph_sha256 ||
      incrementalUpdate.artifacts.manifest_sha256 === fullBuild.artifacts.manifest_sha256
    ) {
      throw new Error('one-source catalog workflow mutation did not change graph and manifest bytes');
    }

    const catalogMutation = mutateCatalogOnly(project);
    const catalogOnlyExpected = {
      operation: 'index',
      mode: 'incremental',
      status: 'rebuilt',
      changed: 0,
      processed: 0,
    };
    const cacheBefore = describeCacheTree(project);
    const catalogOnly = catalogIndexPass(binary, binaryPin, project, 'catalog-only', catalogOnlyExpected);
    const cacheAfter = describeCacheTree(project);
    if (catalogOnly.artifacts.manifest_sha256 !== incrementalUpdate.artifacts.manifest_sha256) {
      throw new Error('catalog-only mutation changed manifest bytes');
    }
    if (
      cacheAfter.sha256 !== cacheBefore.sha256 ||
      cacheAfter.file_count !== cacheBefore.file_count ||
      cacheAfter.total_bytes !== cacheBefore.total_bytes
    ) {
      throw new Error('catalog-only mutation changed extraction cache tree');
    }
    if (catalogOnly.artifacts.graph_sha256 === incrementalUpdate.artifacts.graph_sha256) {
      throw new Error('catalog-only mutation did not change graph annotations');
    }
    if (
      catalogOnly.runtime_telemetry.work.parses !== 0 ||
      catalogOnly.runtime_telemetry.cache.misses !== 0
    ) {
      throw new Error('catalog-only mutation re-extracted source text');
    }
    assertCatalogAnnotation(project, catalogMutation.location);

    const wikiIndex = runCliText(binary, CATALOG_WIKI_INDEX_ARGS, project, undefined, binaryPin);
    if (!wikiIndex.stdout.includes('Indexed 1 wiki pages')) {
      throw new Error('wiki index did not report the generated runbook page');
    }
    const wikiOutput = path.join(project, 'llms.txt');
    const wikiBeforeCheck = sha256File(wikiOutput);
    const wikiCheck = runCliText(binary, CATALOG_WIKI_CHECK_ARGS, project, undefined, binaryPin);
    if (!wikiCheck.stdout.includes('Checked 1 wiki pages')) {
      throw new Error('wiki check did not validate the generated runbook page');
    }
    if (sha256File(wikiOutput) !== wikiBeforeCheck) {
      throw new Error('wiki check modified deterministic generated output');
    }

    const staleCatalogMutation = mutateCatalogOnly(
      project,
      'fixtures/catalog-wiki/docs/runbook-stale-check.md',
    );
    const graphBeforeStaleCheck = sha256File(path.join(project, 'graphoxide-out', 'graph.json'));
    const wikiBeforeStaleCheck = sha256File(wikiOutput);
    let staleCheckFailed = false;
    try {
      runCliText(binary, CATALOG_WIKI_CHECK_ARGS, project, undefined, binaryPin);
    } catch {
      staleCheckFailed = true;
    }
    if (!staleCheckFailed) {
      throw new Error('catalog/wiki stale graph annotations were accepted');
    }
    if (
      sha256File(path.join(project, 'graphoxide-out', 'graph.json')) !== graphBeforeStaleCheck ||
      sha256File(wikiOutput) !== wikiBeforeStaleCheck
    ) {
      throw new Error('catalog/wiki stale graph check modified graph or wiki artifacts');
    }

    return {
      run,
      mutation,
      catalog_mutation: catalogMutation,
      stale_catalog_mutation: staleCatalogMutation,
      full_build: fullBuild,
      warm_build: warmBuild,
      incremental_update: incrementalUpdate,
      catalog_only: { ...catalogOnly, cache_tree: { before: cacheBefore, after: cacheAfter } },
      wiki_index: wikiIndex,
      wiki_check: wikiCheck,
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
  const summary = {
    full_build: phase('full_build'),
    incremental_update: phase('incremental_update'),
  };
  for (const name of ['warm_build', 'catalog_only']) {
    if (samples.every((sample) => sample[name] !== undefined)) summary[name] = phase(name);
  }
  return summary;
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

// Kept pure so the benchmark report contract can be tested without starting a
// release binary. `runBenchmark` owns observation; this function owns the
// stable, machine-readable result shape.
export function buildBenchmarkReport({
  options,
  materialized,
  fixture,
  samples,
  metadata,
  generatedAt = new Date().toISOString(),
}) {
  if (!Array.isArray(samples) || samples.length !== options.runs) {
    throw new Error('benchmark report sample count must equal requested runs');
  }
  const catalogWiki = materialized.scenario.name === 'catalog-wiki';
  const buildCommand = catalogWiki ? CATALOG_WIKI_BUILD_ARGS : FULL_BUILD_ARGS;
  const incrementalCommand = catalogWiki ? CATALOG_WIKI_BUILD_ARGS : INCREMENTAL_UPDATE_ARGS;
  return {
    schema_version: 1,
    benchmark: 'graphoxide-graph-build',
    generated_at: generatedAt,
    runs: options.runs,
    scenario: materialized.scenario,
    fixture: {
      path: materialized.generated ? `<generated:${options.scenario}>` : relativeDisplay(materialized.fixture),
      generated: materialized.generated,
      ...fixture,
      mutation: {
        preferred_path: catalogWiki ? 'src/benchmark.rs' : MUTATION_TARGET,
        method: 'append one deterministic source declaration in the temporary copy',
      },
    },
    commands: {
      full_build: ['graphoxide', ...buildCommand],
      incremental_update: ['graphoxide', ...incrementalCommand],
      runtime_telemetry:
        'each command additionally receives --runtime-report beneath its temporary graphoxide-out directory',
      ...(catalogWiki
        ? {
            warm_build: ['graphoxide', ...CATALOG_WIKI_BUILD_ARGS],
            catalog_only: ['graphoxide', ...CATALOG_WIKI_BUILD_ARGS],
            wiki_index: ['graphoxide', ...CATALOG_WIKI_INDEX_ARGS],
            wiki_check: ['graphoxide', ...CATALOG_WIKI_CHECK_ARGS],
          }
        : {}),
    },
    metadata,
    samples,
    summary: summarizeSamples(samples),
    notes: [
      'Compilation, fixture generation/copying, metadata collection, and source mutation are outside timed regions.',
      'External wall time includes process startup; reported elapsed_ms is supplied by the CLI.',
      'Each sample validates an opt-in runtime telemetry sidecar, effective admission layout, graph shape, graph counts, and mutation-visible graph digest.',
      'Runtime stage durations may overlap in isolated execution modes and must not be summed.',
      'Operating-system filesystem caches are not flushed or controlled.',
      'The slow-io profile uses many small files; it does not inject artificial device latency.',
      ...(catalogWiki
        ? [
            'The catalog/wiki profile measures cold, warm, one-source incremental, and catalog-only index passes before deterministic wiki index and check.',
            'Catalog-only passes require unchanged manifest bytes, zero source parses and cache misses, and changed graph annotation bytes.',
          ]
        : []),
      'These measurements are descriptive observations for this fixture and environment, not performance targets.',
    ],
  };
}

export function runBenchmark(options) {
  const binaryPin = pinBinary(options.binary);
  verifyPinnedBinary(binaryPin, 'before graphoxide --version');
  const cliVersion = probe(options.binary, ['--version']);
  verifyPinnedBinary(binaryPin, 'after graphoxide --version');
  if (!cliVersion) throw new Error(`could not read CLI version from ${options.binary}`);
  const materialized = materializeScenario(options);
  try {
    const fixture = describeFixture(materialized.fixture);
    const samples = [];
    for (let run = 1; run <= options.runs; run += 1) {
      samples.push(
        options.scenario === 'catalog-wiki'
          ? runCatalogWikiSample({
              binary: options.binary,
              binaryPin,
              fixture: materialized.fixture,
              run,
            })
          : runSample({ binary: options.binary, binaryPin, fixture: materialized.fixture, run }),
      );
    }

    verifyPinnedBinary(binaryPin, 'after benchmark phases');

    return buildBenchmarkReport({
      options,
      materialized,
      fixture,
      samples,
      metadata: {
        repository: repositoryMetadata(),
        binary: {
          path: relativeDisplay(options.binary),
          sha256: binaryPin.sha256,
          identity: binaryPin.identity,
        },
        cli_version: cliVersion,
        rust_version: probe('rustc', ['--version']),
        environment: environmentMetadata(),
      },
    });
  } finally {
    materialized.cleanup();
  }
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
