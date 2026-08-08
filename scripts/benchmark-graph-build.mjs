#!/usr/bin/env node

import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import {
  appendFileSync,
  cpSync,
  lstatSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  statSync,
  utimesSync,
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
const FULL_BUILD_ARGS = ['extract', '.', '--force', '--json'];
const INCREMENTAL_UPDATE_ARGS = ['update', '.', '--json'];
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

export function runSample({ binary, fixture, run }) {
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
        preferred_path: MUTATION_TARGET,
        method: 'append one deterministic source declaration in the temporary copy',
      },
    },
    commands: {
      full_build: ['graphoxide', ...FULL_BUILD_ARGS],
      incremental_update: ['graphoxide', ...INCREMENTAL_UPDATE_ARGS],
      runtime_telemetry:
        'each command additionally receives --runtime-report beneath its temporary graphoxide-out directory',
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
      'These measurements are descriptive observations for this fixture and environment, not performance targets.',
    ],
  };
}

export function runBenchmark(options) {
  requireExecutable(options.binary);
  const cliVersion = probe(options.binary, ['--version']);
  if (!cliVersion) throw new Error(`could not read CLI version from ${options.binary}`);
  const materialized = materializeScenario(options);
  try {
    const fixture = describeFixture(materialized.fixture);
    const samples = [];
    for (let run = 1; run <= options.runs; run += 1) {
      samples.push(runSample({ binary: options.binary, fixture: materialized.fixture, run }));
    }

    return buildBenchmarkReport({
      options,
      materialized,
      fixture,
      samples,
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
