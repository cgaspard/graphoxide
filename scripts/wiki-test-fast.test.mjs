import assert from 'node:assert/strict';
import test from 'node:test';

import {
  FAST_WIKI_TESTS,
  cargoNextestIsAvailable,
  fastWikiTestCommands,
  fastWikiTestEnvironment,
  runFastWikiTests,
} from './wiki-test-fast.mjs';

test('fast wiki suite is a compact exact contract selection', () => {
  assert.deepEqual(FAST_WIKI_TESTS, [
    'wiki_source::tests::source_index_rejects_a_local_absolute_path_and_raw_body_field',
    'wiki_source::tests::source_add_local_file_records_only_a_bound_pointer',
    'wiki_source::tests::heterogeneous_admission_rolls_back_a_local_pointer_when_later_https_fails',
    'wiki_source::tests::https_refresh_requires_explicit_consent_and_retains_the_last_pointer',
    'wiki_direct::tests::author_output_writes_source_stable_front_matter_without_source_body',
    'wiki_direct::tests::human_confirmation_rejects_a_forged_ai_review_status',
  ]);
});

test('nextest runs the focused suite in one exact invocation', () => {
  assert.deepEqual(fastWikiTestCommands(true), [{
    command: 'cargo',
    args: [
      'nextest', 'run', '-p', 'graphoxide-cli', '--lib', '--locked', '--profile', 'fast', '--test-threads=num-cpus', '--',
      ...FAST_WIKI_TESTS,
      '--exact',
    ],
  }]);
});

test('cargo fallback preserves the same exact suite', () => {
  assert.deepEqual(
    fastWikiTestCommands(false),
    FAST_WIKI_TESTS.map((testName) => ({
      command: 'cargo',
      args: ['test', '-p', 'graphoxide-cli', '--lib', '--locked', testName, '--', '--exact'],
    })),
  );
});

test('nextest probing does not start Cargo before the test command', () => {
  const calls = [];
  const run = (command, args) => {
    calls.push([command, args]);
    return { status: 0 };
  };
  assert.equal(cargoNextestIsAvailable(run), true);
  assert.deepEqual(calls, [['cargo-nextest', ['--version']]]);
  assert.equal(cargoNextestIsAvailable(() => ({ status: 1 })), false);
  assert.equal(cargoNextestIsAvailable(() => ({ error: new Error('missing') })), false);
});

test('fast wiki tests use sccache only when no compiler wrapper is configured', () => {
  const available = () => ({ status: 0 });
  assert.equal(
    fastWikiTestEnvironment({ PATH: '/bin' }, available).RUSTC_WRAPPER,
    'sccache',
  );
  assert.equal(
    fastWikiTestEnvironment({ PATH: '/bin', RUSTC_WRAPPER: '/custom/wrapper' }, available)
      .RUSTC_WRAPPER,
    '/custom/wrapper',
  );
  assert.equal(
    fastWikiTestEnvironment({ PATH: '/bin', RUSTC_WORKSPACE_WRAPPER: '/custom/workspace' }, available)
      .RUSTC_WRAPPER,
    undefined,
  );
  assert.equal(
    fastWikiTestEnvironment({ PATH: '/bin' }, () => ({ status: 1 })).RUSTC_WRAPPER,
    undefined,
  );
});

test('fast wiki fallback removes every inherited Cargo test-thread cap', () => {
  const available = () => ({ status: 0 });
  for (const cap of ['1', '2']) {
    const supplied = { PATH: '/bin', RUST_TEST_THREADS: cap };
    const env = fastWikiTestEnvironment(supplied, available);
    assert.equal(env.RUST_TEST_THREADS, undefined);
    assert.equal(supplied.RUST_TEST_THREADS, cap);
  }
});

test('fast wiki command receives the selected compiler cache environment', () => {
  const calls = [];
  const run = (command, args, options) => {
    calls.push([command, args, options]);
    return { status: 0 };
  };
  runFastWikiTests(run, { PATH: '/bin' });
  const [, , [, , testOptions]] = calls;
  assert.equal(testOptions.env.RUSTC_WRAPPER, 'sccache');
});
