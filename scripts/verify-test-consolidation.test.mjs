import assert from 'node:assert/strict';
import { mkdtemp, mkdir, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';

import { checkConsolidation } from './verify-test-consolidation.mjs';

const SPEC = [{ crate: 'demo', aggregator: 'all_tests', testName: 'all_tests' }];

async function fixture({ paths = ['one.rs', 'two.rs'], cargoExtra = '' } = {}) {
  const root = await mkdtemp(join(tmpdir(), 'graphoxide-test-consolidation-'));
  const tests = join(root, 'crates/demo/tests');
  await mkdir(tests, { recursive: true });
  await writeFile(join(root, 'crates/demo/Cargo.toml'), `[package]\nautotests = false\n\n[[test]]\nname = "all_tests"\npath = "tests/all_tests.rs"\n${cargoExtra}`);
  await writeFile(join(tests, 'all_tests.rs'), paths.map((path) => `#[path = "${path}"]\nmod ${path.replace(/\.rs$/, '')};`).join('\n'));
  for (const path of paths) await writeFile(join(tests, path), '');
  return root;
}

test('accepts an aggregator that covers every other test root', async () => {
  const root = await fixture();
  try {
    assert.deepEqual(await checkConsolidation(root, SPEC), []);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test('reports a source root omitted from the aggregator', async () => {
  const root = await fixture({ paths: ['one.rs'] });
  try {
    await writeFile(join(root, 'crates/demo/tests/two.rs'), '');
    await assert.rejects(checkConsolidation(root, SPEC), /demo: aggregator paths differ.*two\.rs/s);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test('reports multiple explicit test targets', async () => {
  const root = await fixture({ cargoExtra: '\n[[test]]\nname = "second"\npath = "tests/second.rs"\n' });
  try {
    await assert.rejects(checkConsolidation(root, SPEC), /demo: expected exactly one \[\[test\]\] target/);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});
