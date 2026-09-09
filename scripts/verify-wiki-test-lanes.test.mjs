import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

import { checkParallelWikiLane, listKnowledgebaseTests } from './verify-wiki-test-lanes.mjs';
import { fastWikiTestCommands, fastWikiTestEnvironment } from './wiki-test-fast.mjs';

const root = dirname(dirname(fileURLToPath(import.meta.url)));

test('test listing uses Cargo when optional nextest is unavailable', () => {
  const calls = [];
  const names = listKnowledgebaseTests('/fixture', (command, args, options) => {
    calls.push({ command, args, options });
    if (command === 'cargo-nextest') return { status: 1 };
    return { status: 0, stdout: 'direct_cli_initializes: test\nhelper: benchmark\n\n1 test, 1 benchmark\n' };
  });
  assert.deepEqual(names, ['direct_cli_initializes']);
  assert.equal(calls[1].command, 'cargo');
  assert.deepEqual(calls[1].args, [
    'test', '-p', 'graphoxide-cli', '--test', 'knowledgebase_cli', '--locked',
    '--', '--list', '--format', 'terse',
  ]);
  assert.equal(calls[1].options.cwd, '/fixture');
});

test('test listing reads nextest JSON and fails on a failed listing', () => {
  const run = (command) => command === 'cargo-nextest'
    ? { status: 0 }
    : { status: 0, stdout: JSON.stringify({ 'rust-suites': { cli: { testcases: { direct_cli_initializes: {} } } } }) };
  assert.deepEqual(listKnowledgebaseTests('/fixture', run), ['direct_cli_initializes']);
  assert.throws(
    () => listKnowledgebaseTests('/fixture', (command) => ({ status: command === 'cargo-nextest' ? 0 : 101 })),
    /test listing exited 101/u,
  );
});

test('requires the full lane to use nextest with dynamic CPU concurrency', () => {
  assert.doesNotThrow(() => checkParallelWikiLane(
    'cargo nextest run -p graphoxide-cli --test knowledgebase_cli --profile ci --test-threads=num-cpus --locked',
    '[profile.default]\nslow-timeout = { period = "30s", terminate-after = 3 }\n',
  ));
  assert.throws(
    () => checkParallelWikiLane('cargo nextest run -p graphoxide-cli --test-threads=8', ''),
    /must use dynamic --test-threads=num-cpus/,
  );
  assert.throws(
    () => checkParallelWikiLane('RUST_TEST_THREADS=1 cargo nextest run -p graphoxide-cli --test-threads=num-cpus', ''),
    /must not force RUST_TEST_THREADS=1/,
  );
  assert.throws(
    () => checkParallelWikiLane('cargo nextest run -p graphoxide-cli --test-threads=num-cpus', '[test-groups]\nknowledgebase = { max-threads = 1 }'),
    /must not cap test concurrency/,
  );
});

test('requires the fast Nextest lane to use dynamic CPU concurrency', () => {
  const args = fastWikiTestCommands(true)[0].args;
  assert.ok(args.indexOf('--test-threads=num-cpus') < args.indexOf('--'));
  for (const cap of ['1', '2']) {
    const supplied = { RUST_TEST_THREADS: cap };
    assert.equal(fastWikiTestEnvironment(supplied).RUST_TEST_THREADS, undefined);
    assert.equal(supplied.RUST_TEST_THREADS, cap);
  }
});

test('keeps CI knowledgebase test status in a workspace-local JUnit artifact without process output', async () => {
  const config = await readFile(join(root, '.config/nextest.toml'), 'utf8');
  assert.match(config, /\[profile\.ci\.junit\][\s\S]*path\s*=\s*"target\/nextest\/ci\/wiki-test-full\.junit\.xml"/);
  assert.match(config, /\[profile\.ci\.junit\][\s\S]*store-success-output\s*=\s*false/);
  assert.match(config, /\[profile\.ci\.junit\][\s\S]*store-failure-output\s*=\s*false/);
});

test('only the packaged VSIX smoke test receives a bounded ten-minute default deadline', async () => {
  const config = await readFile(join(root, '.config/nextest.toml'), 'utf8');
  const sections = config.split(/(?=^\[)/mu);
  const overrides = sections.filter((section) => section.startsWith('[[profile.default.overrides]]'));
  assert.equal(overrides.length, 1);
  assert.match(overrides[0], /filter = 'binary\(=knowledgebase_cli\) & test\(=packaged_artifact_smoke::packaged_vsix_bundled_binary_is_native_and_indexes_fixture\)'/u);
  assert.match(overrides[0], /slow-timeout = \{ period = "60s", terminate-after = 10 \}/u);
  assert.match(sections.find((section) => section.startsWith('[profile.default]')),
    /slow-timeout = \{ period = "30s", terminate-after = 3 \}/u);
  assert.match(sections.find((section) => section.startsWith('[profile.fast]')),
    /slow-timeout = \{ period = "15s", terminate-after = 1 \}/u);
});

test('does not retain the removed knowledgebase system-test feature', async () => {
  const manifest = await readFile(join(root, 'crates/graphoxide-cli/Cargo.toml'), 'utf8');
  assert.doesNotMatch(manifest, /wiki-system-tests/);
});
