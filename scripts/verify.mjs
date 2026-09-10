#!/usr/bin/env node

import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const release = process.argv.includes('--release');
const prePush = process.argv.includes('--pre-push');
const testConcurrency = process.env.GRAPHOXIDE_TEST_CONCURRENCY;

if ((release && prePush) || (!release && !prePush) || process.argv.length !== 3) {
  fail('Usage: node scripts/verify.mjs --pre-push | --release');
}
if (testConcurrency !== undefined && !/^[1-9]\d*$/u.test(testConcurrency)) {
  fail('GRAPHOXIDE_TEST_CONCURRENCY must be a positive integer when set.');
}

const vscode = path.join(root, 'editors', 'vscode');
run('node', ['scripts/verify-test-parallelism.mjs']);
run('node', ['scripts/verify-test-consolidation.mjs']);
run('node', ['scripts/verify-wiki-test-lanes.mjs', '--list']);
run('cargo', ['fmt', '--all', '--', '--check']);
run('cargo', ['clippy', '--workspace', '--all-targets', '--', '-D', 'warnings']);
run('node', [
  '--test',
  ...(testConcurrency === undefined ? [] : [`--test-concurrency=${testConcurrency}`]),
  'scripts/agent-artifacts.test.mjs',
  'scripts/benchmark-graph-build.test.mjs',
  'scripts/cleanup-worktrees.test.mjs',
  'scripts/qualify-universal-indexing.test.mjs',
  'scripts/rust-coverage.test.mjs',
  'scripts/verify-test-parallelism.test.mjs',
  'scripts/verify-test-consolidation.test.mjs',
  'scripts/security-audit.test.mjs',
  'scripts/wiki-test-fast.test.mjs',
  'scripts/verify-wiki-test-lanes.test.mjs',
  'scripts/workflow-dependencies.test.mjs',
]);
run('cargo', ['test', '--workspace', '--no-fail-fast', '--locked']);
run('npm', ['run', 'check'], vscode);
run('node', ['scripts/agent-artifacts.mjs', '--check']);
run('node', ['scripts/render-release-notes.mjs', '--current', '--check']);

if (release) {
  run('cargo', ['build', '--release', '--workspace', '--locked']);
  run('npm', ['run', 'test:e2e'], vscode);
  run('npm', ['run', 'package'], vscode);
}

function run(command, args, cwd = root) {
  const display = [command, ...args].join(' ');
  process.stdout.write(`\n[verify] ${display}\n`);
  const env = command === 'cargo' && args[0] === 'test'
    ? { ...process.env, RUST_MIN_STACK: process.env.RUST_MIN_STACK ?? '8388608' }
    : process.env;
  const result = spawnSync(command, args, { cwd, env, stdio: 'inherit' });
  if (result.error) fail(`${display}: ${result.error.message}`);
  if (result.status !== 0) process.exit(result.status ?? 1);
}

function fail(message) {
  process.stderr.write(`${message}\n`);
  process.exit(2);
}
