#!/usr/bin/env node

import { spawnSync } from 'node:child_process';
import { readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

import { cargoNextestIsAvailable, fastWikiTestCommands } from './wiki-test-fast.mjs';

export function checkParallelWikiLane(command, nextestConfig) {
  if (/\bRUST_TEST_THREADS\s*=\s*1\b/.test(`${command}\n${nextestConfig}`)) {
    throw new Error('knowledgebase test lanes must not force RUST_TEST_THREADS=1');
  }
  if (!command.startsWith('cargo nextest run ')) {
    throw new Error('wiki:test-full must use cargo nextest run');
  }
  if (!command.includes('--test-threads=num-cpus')) {
    throw new Error('knowledgebase test lanes must use dynamic --test-threads=num-cpus');
  }
  if (/--test-threads(?:=|\s+)(?!num-cpus\b)\S+/.test(command) || /max-threads\s*=\s*1/.test(nextestConfig)) {
    throw new Error('knowledgebase test lanes must not cap test concurrency');
  }
}

function listedTestNames(list) {
  return Object.values(list['rust-suites'] ?? {}).flatMap((suite) => Object.keys(suite.testcases ?? {}));
}

export function listKnowledgebaseTests(root, run = spawnSync) {
  const nextest = cargoNextestIsAvailable(run);
  const args = nextest
    ? ['nextest', 'list', '-p', 'graphoxide-cli', '--test', 'knowledgebase_cli',
      '--locked', '--message-format', 'json']
    : ['test', '-p', 'graphoxide-cli', '--test', 'knowledgebase_cli',
      '--locked', '--', '--list', '--format', 'terse'];
  const result = run('cargo', args, {
    cwd: root,
    encoding: 'utf8',
    stdio: ['ignore', 'pipe', 'inherit'],
  });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(`knowledgebase test listing exited ${result.status}`);
  return nextest
    ? listedTestNames(JSON.parse(result.stdout))
    : result.stdout.split(/\r?\n/u).filter((line) => line.endsWith(': test'))
      .map((line) => line.slice(0, -': test'.length));
}

export async function checkWikiTestLanes(root, { list = false } = {}) {
  const [packageJson, nextestConfig, integration] = await Promise.all([
    readFile(join(root, 'package.json'), 'utf8'),
    readFile(join(root, '.config/nextest.toml'), 'utf8'),
    readFile(join(root, 'crates/graphoxide-cli/tests/knowledgebase_cli.rs'), 'utf8'),
  ]);
  const full = JSON.parse(packageJson).scripts?.['wiki:test-full'] ?? '';
  checkParallelWikiLane(full, nextestConfig);
  checkParallelWikiLane(
    [fastWikiTestCommands(true)[0].command, ...fastWikiTestCommands(true)[0].args].join(' '),
    nextestConfig,
  );
  if (!full.includes('--test knowledgebase_cli') || !integration.includes('direct_cli_')
    || !integration.includes('mod direct_source_regressions;')) {
    throw new Error('knowledgebase test lanes must retain direct CLI coverage');
  }
  if (list) {
    const names = listKnowledgebaseTests(root);
    for (const prefix of ['direct_cli_', 'direct_source_regressions::']) {
      if (!names.some((name) => name.startsWith(prefix))) {
        throw new Error(`knowledgebase integration target is missing ${prefix} coverage`);
      }
    }
  }
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const root = dirname(dirname(fileURLToPath(import.meta.url)));
  await checkWikiTestLanes(root, { list: process.argv.includes('--list') });
  console.log('knowledgebase test lanes verification passed');
}
