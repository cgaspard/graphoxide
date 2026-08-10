import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import test from 'node:test';

import { inspectRepositoryWorkflows, inspectWorkflowActionPins } from './workflow-dependencies.mjs';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

test('all repository workflow actions use immutable reviewed revisions', () => {
  const result = inspectRepositoryWorkflows();
  assert.deepEqual(result.files, ['ci.yml', 'deploy-pages.yml', 'qualification.yml', 'release.yml']);
  assert.ok(result.actions > 0);
  assert.deepEqual(result.errors, []);
});

test('mutable, abbreviated, uncommented, and quoted action references are rejected', () => {
  const text = `
steps:
  - uses: actions/checkout@v7
  - uses: actions/setup-node@0123456789abcdef
  - uses: actions/upload-artifact@0123456789abcdef0123456789abcdef01234567
  - uses: "actions/download-artifact@0123456789abcdef0123456789abcdef01234567" # v8.0.1
`;
  const result = inspectWorkflowActionPins(text, 'fixture.yml');
  assert.equal(result.actions, 4);
  assert.ok(result.errors.length >= 6);
  assert.ok(result.errors.some((error) => error.includes('immutable 40-character commit SHA')));
  assert.ok(result.errors.some((error) => error.includes('exact release comment')));
  assert.ok(result.errors.some((error) => error.includes('canonical one-action-per-line form')));
});

test('quoted YAML keys, flow mappings, and aliases cannot bypass pin enforcement', () => {
  for (const text of [
    `steps:\n  - 'uses': actions/checkout@v7\n`,
    `steps: [{ uses: actions/checkout@v7 }]\n`,
    `action: &action { uses: actions/checkout@v7 }\nsteps: [*action]\n`,
  ]) {
    const result = inspectWorkflowActionPins(text, 'bypass.yml');
    assert.notDeepEqual(result.errors, []);
  }
});

test('local actions and reviewed master snapshots remain explicit exceptions', () => {
  const text = `
steps:
  - uses: ./local-action
  - uses: dtolnay/rust-toolchain@6c977a6ca4077a0ceb28ffbe03f59d46e9ac8772 # master (2026-08-10)
`;
  assert.deepEqual(inspectWorkflowActionPins(text).errors, []);
});

test('Dependabot provides review-only visibility for workflow and Node dependencies', () => {
  const config = readFileSync(path.join(root, '.github', 'dependabot.yml'), 'utf8');
  assert.equal([...config.matchAll(/package-ecosystem:\s*github-actions/gu)].length, 1);
  assert.equal([...config.matchAll(/package-ecosystem:\s*npm/gu)].length, 2);
  assert.match(config, /directory:\s*\/editors\/vscode/u);
  assert.equal([...config.matchAll(/interval:\s*weekly/gu)].length, 3);
  assert.doesNotMatch(config, /auto-merge|automerge/iu);
});
