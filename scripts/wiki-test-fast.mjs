#!/usr/bin/env node

import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

export const FAST_WIKI_TESTS = Object.freeze([
  'wiki_source::tests::source_index_rejects_a_local_absolute_path_and_raw_body_field',
  'wiki_source::tests::source_add_local_file_records_only_a_bound_pointer',
  'wiki_source::tests::heterogeneous_admission_rolls_back_a_local_pointer_when_later_https_fails',
  'wiki_source::tests::https_refresh_requires_explicit_consent_and_retains_the_last_pointer',
  'wiki_direct::tests::author_output_writes_source_stable_front_matter_without_source_body',
  'wiki_direct::tests::human_confirmation_rejects_a_forged_ai_review_status',
]);

export function fastWikiTestCommands(useNextest) {
  if (useNextest) {
    return [{
      command: 'cargo',
      args: [
        'nextest', 'run', '-p', 'graphoxide-cli', '--lib', '--locked', '--profile', 'fast', '--test-threads=num-cpus', '--',
        ...FAST_WIKI_TESTS,
        '--exact',
      ],
    }];
  }

  return FAST_WIKI_TESTS.map((test) => ({
    command: 'cargo',
    args: ['test', '-p', 'graphoxide-cli', '--lib', '--locked', test, '--', '--exact'],
  }));
}

export function cargoNextestIsAvailable(run = spawnSync) {
  const result = run('cargo-nextest', ['--version'], { stdio: 'ignore' });
  return !result.error && result.status === 0;
}

export function fastWikiTestEnvironment(environment = process.env, run = spawnSync) {
  // Cargo's fallback inherits this variable unless it is explicitly removed.
  // The focused lane must use Cargo's available-thread default, not a shell cap.
  const env = { ...environment };
  delete env.RUST_TEST_THREADS;
  if (
    env.RUSTC_WRAPPER
    || env.RUSTC_WORKSPACE_WRAPPER
  ) {
    return env;
  }
  const result = run('sccache', ['--version'], { stdio: 'ignore' });
  if (result.error || result.status !== 0) return env;
  return { ...env, RUSTC_WRAPPER: 'sccache' };
}

export function runFastWikiTests(run = spawnSync, environment = process.env) {
  const commands = fastWikiTestCommands(cargoNextestIsAvailable(run));
  const env = fastWikiTestEnvironment(environment, run);
  for (const { command, args } of commands) {
    process.stdout.write(`\n[wiki:test-fast] ${[command, ...args].join(' ')}\n`);
    const result = run(command, args, { stdio: 'inherit', env });
    if (result.error) throw result.error;
    if (result.status !== 0) process.exit(result.status ?? 1);
  }
}

const invokedPath = process.argv[1] && path.resolve(process.argv[1]);
if (invokedPath === fileURLToPath(import.meta.url)) runFastWikiTests();
