import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import path from 'node:path';
import test from 'node:test';
import {
  automaticGraphUpdateArguments,
  graphBuildDecision,
  graphBuildOutputDirectory,
  workspaceGraphMutationAllowed,
} from '../src/build';

test('automatic updates retain shrink authorization until the CLI splits force semantics', () => {
  assert.deepEqual(automaticGraphUpdateArguments('/workspace'), ['update', '/workspace', '--force']);
});

test('requires workspace trust for graph artifact mutations', () => {
  assert.equal(workspaceGraphMutationAllowed(false), false);
  assert.equal(workspaceGraphMutationAllowed(true), true);
});

test('routes build output to the configured graph directory', () => {
  const workspace = path.resolve('/work', 'example');
  assert.equal(
    graphBuildOutputDirectory(workspace, path.join(workspace, 'custom-output', 'graph.json')),
    path.join(workspace, 'custom-output'),
  );
  assert.throws(
    () => graphBuildOutputDirectory(workspace, path.join(workspace, 'custom-output', 'custom.json')),
    /Graph Path to end in graph\.json/,
  );
  assert.throws(
    () => graphBuildOutputDirectory(workspace, path.join(workspace, 'graph.json')),
    /dedicated output directory, not the workspace root or one of its ancestors/,
  );
  assert.throws(
    () => graphBuildOutputDirectory(workspace, path.join(path.dirname(workspace), 'graph.json')),
    /dedicated output directory, not the workspace root or one of its ancestors/,
  );
});

test('selects distinct CLI invocations for build, incremental update, and full rebuild', () => {
  const workspace = '/work/example';
  assert.deepEqual(
    graphBuildDecision('build', workspace, { graphFileExists: false, hasValidBaseline: false }),
    {
      kind: 'run',
      args: ['extract', workspace],
      progressTitle: 'Graphoxide: building graph…',
      completionMessage: 'Graphoxide graph build complete.',
    },
  );
  assert.deepEqual(
    graphBuildDecision('update', workspace, { graphFileExists: true, hasValidBaseline: true }),
    {
      kind: 'run',
      args: ['update', workspace],
      progressTitle: 'Graphoxide: updating graph incrementally…',
      completionMessage: 'Graphoxide incremental update complete.',
    },
  );
  assert.deepEqual(
    graphBuildDecision('rebuild', workspace, { graphFileExists: true, hasValidBaseline: false }),
    {
      kind: 'run',
      args: ['extract', workspace, '--force'],
      progressTitle: 'Graphoxide: rebuilding full graph…',
      completionMessage: 'Graphoxide full rebuild complete.',
    },
  );
});

test('blocks unsafe graph operations and points to the safe alternative', () => {
  const workspace = '/work/example';
  const buildOverExisting = graphBuildDecision('build', workspace, { graphFileExists: true, hasValidBaseline: true });
  assert.equal(buildOverExisting.kind, 'blocked');
  if (buildOverExisting.kind === 'blocked') assert.equal(buildOverExisting.suggestedCommand, 'graphoxide.rebuild');

  const updateMissing = graphBuildDecision('update', workspace, { graphFileExists: false, hasValidBaseline: false });
  assert.equal(updateMissing.kind, 'blocked');
  if (updateMissing.kind === 'blocked') assert.equal(updateMissing.suggestedCommand, 'graphoxide.initialize');

  const updateMalformed = graphBuildDecision('update', workspace, { graphFileExists: true, hasValidBaseline: false });
  assert.equal(updateMalformed.kind, 'blocked');
  if (updateMalformed.kind === 'blocked') assert.equal(updateMalformed.suggestedCommand, 'graphoxide.rebuild');

  const rebuildMissing = graphBuildDecision('rebuild', workspace, { graphFileExists: false, hasValidBaseline: false });
  assert.equal(rebuildMissing.kind, 'blocked');
  if (rebuildMissing.kind === 'blocked') assert.equal(rebuildMissing.suggestedCommand, 'graphoxide.initialize');
});

test('declares graph commands with clear labels and graph-state enablement', async () => {
  const packageJson = JSON.parse(await readFile(path.join(process.cwd(), 'package.json'), 'utf8')) as {
    contributes?: { commands?: Array<{ command?: string; title?: string; enablement?: string }> };
  };
  const commands = new Map(packageJson.contributes?.commands?.map((command) => [command.command, command]));
  assert.deepEqual(commands.get('graphoxide.initialize'), {
    command: 'graphoxide.initialize',
    title: 'Graphoxide: Build Graph',
    icon: '$(play)',
    enablement: '!graphoxide.hasGraphFile',
  });
  assert.deepEqual(commands.get('graphoxide.update'), {
    command: 'graphoxide.update',
    title: 'Graphoxide: Update Graph (Incremental)',
    icon: '$(refresh)',
    enablement: 'graphoxide.hasGraph',
  });
  assert.deepEqual(commands.get('graphoxide.rebuild'), {
    command: 'graphoxide.rebuild',
    title: 'Graphoxide: Rebuild Graph (Full)',
    icon: '$(sync)',
    enablement: 'graphoxide.hasGraphFile',
  });
  assert.deepEqual(commands.get('graphoxide.startWatch'), {
    command: 'graphoxide.startWatch',
    title: 'Graphoxide: Start Watch Mode',
    icon: '$(eye)',
    enablement: 'isWorkspaceTrusted',
  });
});
