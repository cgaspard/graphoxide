import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import test from 'node:test';
import { parse } from 'yaml';

import { inspectRepositoryWorkflows, inspectWorkflowActionPins } from './workflow-dependencies.mjs';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

test('all repository workflow actions use immutable reviewed revisions', () => {
  const result = inspectRepositoryWorkflows();
  assert.deepEqual(result.files, [
    'ci.yml',
    'deploy-pages.yml',
    'qualification.yml',
    'release.yml',
    'security.yml',
  ]);
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

test('security scans locked dependencies and source with least privilege', () => {
  const workflowText = readFileSync(path.join(root, '.github', 'workflows', 'security.yml'), 'utf8');
  const workflow = parse(workflowText, { maxAliasCount: 0, uniqueKeys: true });

  assert.deepEqual(workflow.permissions, { contents: 'read' });
  assert.deepEqual(workflow.on.push.branches, ['main']);
  assert.ok(Object.hasOwn(workflow.on, 'pull_request'));
  assert.equal(workflow.on.schedule.length, 1);
  assert.match(workflow.on.schedule[0].cron, /^\d+ \d+ \* \* \d+$/u);
  assert.equal(workflow.concurrency['cancel-in-progress'], true);

  const advisorySteps = workflow.jobs.advisories.steps;
  assert.equal(workflow.jobs.advisories['timeout-minutes'], 15);
  assert.ok(
    advisorySteps.some(
      (step) => step.with?.tool === 'cargo-audit@0.22.2' &&
        step.uses?.startsWith('taiki-e/install-action@'),
    ),
  );
  assert.equal(advisorySteps.some((step) => step.run?.startsWith('npm ci')), false);
  assert.ok(advisorySteps.some((step) => step.run === 'npm run audit:security'));

  const codeql = workflow.jobs.codeql;
  assert.deepEqual(codeql.permissions, { contents: 'read', 'security-events': 'write' });
  assert.equal(codeql['timeout-minutes'], 30);
  assert.equal(codeql.strategy['fail-fast'], false);
  assert.deepEqual(codeql.strategy.matrix.language, ['actions', 'javascript-typescript', 'rust']);
  const rustSetup = codeql.steps.find((step) => step.name === 'Set up Rust');
  assert.ok(rustSetup?.uses?.startsWith('dtolnay/rust-toolchain@'));
  assert.equal(rustSetup.if, "matrix.language == 'rust'");
  assert.equal(rustSetup.with?.toolchain, '1.97.1');
  assert.equal(rustSetup.with?.components, 'rust-src');
  const rustFetch = codeql.steps.find((step) => step.name === 'Fetch locked Rust dependencies');
  assert.equal(rustFetch?.if, "matrix.language == 'rust'");
  assert.equal(rustFetch?.run, 'cargo fetch --locked');
  const init = codeql.steps.find((step) => step.uses?.startsWith('github/codeql-action/init@'));
  assert.ok(init);
  assert.equal(init.with.languages, '${{ matrix.language }}');
  assert.equal(init.with['build-mode'], 'none');
  assert.equal(init.with.queries, 'security-extended');
  assert.equal(init.with['config-file'], './.github/codeql/codeql-config.yml');
  assert.ok(codeql.steps.indexOf(rustSetup) < codeql.steps.indexOf(rustFetch));
  assert.ok(codeql.steps.indexOf(rustFetch) < codeql.steps.indexOf(init));
  assert.ok(codeql.steps.some((step) => step.uses?.startsWith('github/codeql-action/analyze@')));

  for (const job of Object.values(workflow.jobs)) {
    const checkout = job.steps.find((step) => step.uses?.startsWith('actions/checkout@'));
    assert.equal(checkout?.with?.['persist-credentials'], false);
  }
  assert.doesNotMatch(workflowText, /pull_request_target|secrets\.|upload-artifact/iu);
});

test('CodeQL excludes only inert parity corpora', () => {
  const config = parse(
    readFileSync(path.join(root, '.github', 'codeql', 'codeql-config.yml'), 'utf8'),
    { maxAliasCount: 0, uniqueKeys: true },
  );
  assert.deepEqual(config['paths-ignore'], ['parity/corpora/**']);
  assert.equal(Object.hasOwn(config, 'paths'), false);
});
