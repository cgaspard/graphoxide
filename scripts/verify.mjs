#!/usr/bin/env node

import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const release = process.argv.includes('--release');
const prePush = process.argv.includes('--pre-push');

if ((release && prePush) || (!release && !prePush) || process.argv.length !== 3) {
  fail('Usage: node scripts/verify.mjs --pre-push | --release');
}

const vscode = path.join(root, 'editors', 'vscode');
run('cargo', ['fmt', '--all', '--', '--check']);
run('cargo', ['clippy', '--workspace', '--all-targets', '--', '-D', 'warnings']);
run('cargo', ['test', '--workspace', '--no-fail-fast', '--locked']);
run('node', ['--test', 'scripts/benchmark-graph-build.test.mjs']);
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
  const result = spawnSync(command, args, { cwd, stdio: 'inherit' });
  if (result.error) fail(`${display}: ${result.error.message}`);
  if (result.status !== 0) process.exit(result.status ?? 1);
}

function fail(message) {
  process.stderr.write(`${message}\n`);
  process.exit(2);
}
