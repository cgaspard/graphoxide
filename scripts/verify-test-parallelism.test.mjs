import assert from 'node:assert/strict';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import { readFile } from 'node:fs/promises';

import { checkConfiguredParallelism, parallelismViolations, reachableRootScripts } from './verify-test-parallelism.mjs';

test('accepts unbounded active test and build commands', () => {
  assert.deepEqual(parallelismViolations({
    'package.json': '{"scripts":{"test":"cargo nextest run --test-threads=num-cpus"}}',
    '.config/nextest.toml': '[profile.default]\nslow-timeout = { period = "30s" }',
  }), []);
});

for (const [name, content, expected] of [
  ['equals-form serial Rust test threads', 'cargo test -- --test-threads=1', '--test-threads=1'],
  ['space-form fixed Rust test threads', 'cargo nextest run --test-threads 8', '--test-threads=8'],
  ['fixed Rust environment assignment', 'RUST_TEST_THREADS=8 cargo nextest run', 'fixed RUST_TEST_THREADS'],
  ['fixed Rust YAML environment', 'RUST_TEST_THREADS: "8"', 'fixed RUST_TEST_THREADS'],
  ['fixed nextest group', 'wiki = { max-threads = 8 }', 'fixed max-threads'],
  ['fixed cargo environment jobs', 'CARGO_BUILD_JOBS=8 cargo test', 'CARGO_BUILD_JOBS'],
  ['fixed cargo YAML jobs', 'CARGO_BUILD_JOBS: "8"', 'CARGO_BUILD_JOBS'],
  ['equals-form fixed cargo jobs', 'cargo test --jobs=8', 'fixed cargo --jobs'],
  ['short-form fixed cargo jobs', 'cargo build -j8', 'fixed cargo --jobs'],
  ['fixed cargo config jobs', '[build]\njobs = 8', 'fixed cargo config jobs'],
]) {
  test(`rejects ${name}`, () => {
    assert.deepEqual(parallelismViolations({ 'active-config': content }), [`active-config: ${expected}`]);
  });
}

test('ignores commented examples', () => {
  assert.deepEqual(parallelismViolations({
    'active-config': '# cargo test -- --test-threads=1\n// RUST_TEST_THREADS=8 cargo test\n# max-threads = 8',
  }), []);
});

test('follows an active script through one imported child', () => {
  assert.deepEqual(
    [...reachableRootScripts(['parent.mjs'], {
      'parent.mjs': "import './child.mjs';",
      'child.mjs': "const args = ['cargo', 'test'];",
    })].sort(),
    ['child.mjs', 'parent.mjs'],
  );
});

test('detects a fixed test thread setting in a multiline command array', () => {
  assert.deepEqual(parallelismViolations({
    'scripts/active.mjs': "const args = [\n  'cargo',\n  'test',\n  '--test-threads=1',\n];",
  }), ['scripts/active.mjs: --test-threads=1']);
});

test('ignores non-executable diagnostic text', () => {
  assert.deepEqual(parallelismViolations({
    'scripts/active.mjs': "throw new Error('refuses RUST_TEST_THREADS=8');",
  }), []);
});

test('does not let a diagnostic exclusion hide following executable arguments', () => {
  assert.deepEqual(parallelismViolations({
    'scripts/active.mjs': "if (bad) throw new Error('refuses RUST_TEST_THREADS=8'); const args = ['--test-threads=1'];",
  }), ['scripts/active.mjs: --test-threads=1']);
});

test('ignores a truly multiline diagnostic string', () => {
  assert.deepEqual(parallelismViolations({
    'scripts/active.mjs': 'throw new Error("refuses\\\nRUST_TEST_THREADS=8");',
  }), []);
});

test('does not hide executable interpolated template diagnostics', () => {
  assert.deepEqual(parallelismViolations({
    'scripts/active.mjs': "throw new Error(`refuses ${'RUST_TEST_THREADS=8'}`);",
  }), ['scripts/active.mjs: fixed RUST_TEST_THREADS']);
});

for (const [name, flag] of [
  ['equals form', '--jobs=8'],
  ['short form', '-j 8'],
]) {
  test(`detects multiline cargo test ${name} jobs`, () => {
    assert.deepEqual(parallelismViolations({
      'scripts/active.mjs': `const args = [\n  'cargo',\n  'test',\n  '${flag}',\n];`,
    }), ['scripts/active.mjs: fixed cargo --jobs']);
  });
}

test('current active configuration has no serial caps or fixed build jobs', async () => {
  const root = dirname(dirname(fileURLToPath(import.meta.url)));
  await assert.doesNotReject(() => checkConfiguredParallelism(root));
});

test('pre-push runs the real topology guards once before workspace tests', async () => {
  const root = dirname(dirname(fileURLToPath(import.meta.url)));
  const verify = await readFile(join(root, 'scripts/verify.mjs'), 'utf8');
  const parallelism = "run('node', ['scripts/verify-test-parallelism.mjs']);";
  const consolidation = "run('node', ['scripts/verify-test-consolidation.mjs']);";
  const wikiLanes = "run('node', ['scripts/verify-wiki-test-lanes.mjs', '--list']);";
  const cargoFmt = "run('cargo', ['fmt', '--all', '--', '--check']);";
  const workspaceTests = "run('cargo', ['test', '--workspace', '--no-fail-fast', '--locked']);";
  const fixtureTests = [
    "'scripts/verify-test-parallelism.test.mjs',",
    "'scripts/verify-test-consolidation.test.mjs',",
    "'scripts/verify-wiki-test-lanes.test.mjs',",
  ];

  assert.equal(verify.split(parallelism).length - 1, 1);
  assert.equal(verify.split(consolidation).length - 1, 1);
  assert.equal(verify.split(wikiLanes).length - 1, 1);
  assert.ok(verify.indexOf(parallelism) < verify.indexOf(consolidation));
  assert.ok(verify.indexOf(consolidation) < verify.indexOf(wikiLanes));
  assert.ok(verify.indexOf(wikiLanes) < verify.indexOf(cargoFmt));
  assert.ok(verify.indexOf(cargoFmt) < verify.indexOf(workspaceTests));
  for (const fixture of fixtureTests) assert.equal(verify.split(fixture).length - 1, 1);
});

test('CI runs each real topology guard and fixture suite before Cargo', async () => {
  const root = dirname(dirname(fileURLToPath(import.meta.url)));
  const workflow = await readFile(join(root, '.github', 'workflows', 'ci.yml'), 'utf8');
  const realGuards = [
    'scripts/verify-test-parallelism.mjs',
    'scripts/verify-test-consolidation.mjs',
    'scripts/verify-wiki-test-lanes.mjs --list',
  ];
  const fixtures = [
    'scripts/verify-test-parallelism.test.mjs',
    'scripts/verify-test-consolidation.test.mjs',
    'scripts/verify-wiki-test-lanes.test.mjs',
  ];
  const firstCargo = workflow.indexOf('cargo build --release --locked --bin graphoxide');

  assert.ok(firstCargo >= 0, 'qualification job must retain its Cargo build');
  for (const entry of [...realGuards, ...fixtures]) {
    assert.equal(workflow.split(entry).length - 1, 1, `CI must run ${entry} exactly once`);
    assert.ok(workflow.indexOf(entry) < firstCargo, `CI must run ${entry} before Cargo`);
  }
});
