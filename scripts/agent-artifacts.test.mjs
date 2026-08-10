import assert from 'node:assert/strict';
import {
  link,
  lstat,
  mkdir,
  mkdtemp,
  readFile,
  realpath,
  rename,
  rm,
  symlink,
  writeFile,
} from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';

import {
  AGENT_ARTIFACT_MAX_BYTES,
  artifactPaths,
  stageAgentArtifacts,
  verifyAgentArtifacts,
} from './agent-artifacts.mjs';

async function stagedFixture(name) {
  const temporary = await realpath(
    await mkdtemp(path.join(os.tmpdir(), `graphoxide-agent-${name}-`)),
  );
  const output = path.join(temporary, 'agent-assets');
  await stageAgentArtifacts(output);
  return { output, temporary };
}

function artifactFile(root, relativePath = artifactPaths[0]) {
  return path.join(root, ...relativePath.split('/'));
}

test('agent artifacts stage in a private directory and verify through bounded descriptors', async () => {
  const fixture = await stagedFixture('private');
  try {
    await assert.doesNotReject(() => verifyAgentArtifacts(fixture.output));
    const metadata = await lstat(fixture.output);
    if (process.platform !== 'win32') assert.equal(metadata.mode & 0o777, 0o700);
  } finally {
    await rm(fixture.temporary, { recursive: true, force: true });
  }
});

test('agent artifact verification rejects hard-linked files', async () => {
  const fixture = await stagedFixture('hardlink');
  const target = artifactFile(fixture.output);
  const preserved = path.join(fixture.temporary, 'preserved.md');
  try {
    await rename(target, preserved);
    await link(preserved, target);
    await assert.rejects(
      () => verifyAgentArtifacts(fixture.output),
      /single-link regular file/,
    );
  } finally {
    await rm(fixture.temporary, { recursive: true, force: true });
  }
});

test('agent artifact verification rejects files over its byte ceiling', async () => {
  const fixture = await stagedFixture('oversize');
  try {
    await writeFile(
      artifactFile(fixture.output),
      Buffer.alloc(AGENT_ARTIFACT_MAX_BYTES + 1, 0x61),
    );
    await assert.rejects(
      () => verifyAgentArtifacts(fixture.output),
      new RegExp(`${AGENT_ARTIFACT_MAX_BYTES}-byte ceiling`, 'u'),
    );
  } finally {
    await rm(fixture.temporary, { recursive: true, force: true });
  }
});

test('agent artifact inventory rejects symlink entries', {
  skip: process.platform === 'win32' && 'ordinary Windows users cannot create test symlinks',
}, async () => {
  const fixture = await stagedFixture('symlink');
  const target = artifactFile(fixture.output);
  const preserved = path.join(fixture.temporary, 'preserved.md');
  try {
    await rename(target, preserved);
    await symlink(preserved, target);
    await assert.rejects(
      () => verifyAgentArtifacts(fixture.output),
      /Symlink is not allowed in agent artifacts/,
    );
  } finally {
    await rm(fixture.temporary, { recursive: true, force: true });
  }
});

test('agent artifact verification fails closed on pathname replacement after opening', {
  skip: process.platform === 'win32' && 'Windows does not permit replacing this open test file',
}, async () => {
  const fixture = await stagedFixture('swap');
  const relativePath = artifactPaths[0];
  const target = artifactFile(fixture.output, relativePath);
  const preserved = path.join(fixture.temporary, 'preserved.md');
  const attacker = path.join(fixture.temporary, 'attacker.md');
  let replaced = false;
  try {
    await writeFile(attacker, '# Graphoxide attacker replacement\n'.repeat(8));
    await assert.rejects(
      () => verifyAgentArtifacts(fixture.output, {
        async afterArtifactOpen(openedPath, openedRelativePath) {
          if (openedRelativePath !== relativePath) return;
          assert.equal(openedPath, target);
          await rename(target, preserved);
          await symlink(attacker, target);
          replaced = true;
        },
      }),
      /escaped its verified root|changed while being verified/,
    );
    assert.equal(replaced, true);
  } finally {
    await rm(fixture.temporary, { recursive: true, force: true });
  }
});

test('agent artifact verification rejects an ancestor redirected after inventory', {
  skip: process.platform === 'win32' && 'ordinary Windows users cannot create test symlinks',
}, async () => {
  const fixture = await stagedFixture('ancestor-swap');
  const relativePath = 'skills/agents/references/add-watch.md';
  const externalDirectory = path.join(fixture.temporary, 'external-references');
  const displacedDirectory = path.join(fixture.temporary, 'preserved-references');
  const attackerBytes = '# Graphoxide external replacement\n'.repeat(8);
  let replaced = false;
  try {
    await mkdir(externalDirectory);
    await writeFile(path.join(externalDirectory, 'add-watch.md'), attackerBytes);
    await assert.rejects(
      () => verifyAgentArtifacts(fixture.output, {
        async beforeArtifactOpen(openedPath, openedRelativePath) {
          if (openedRelativePath !== relativePath) return;
          const openedParent = path.dirname(openedPath);
          await rename(openedParent, displacedDirectory);
          await symlink(externalDirectory, openedParent, 'dir');
          replaced = true;
        },
      }),
      /escaped its verified root/,
    );
    assert.equal(replaced, true);
    assert.equal(
      await readFile(path.join(externalDirectory, 'add-watch.md'), 'utf8'),
      attackerBytes,
    );
  } finally {
    await rm(fixture.temporary, { recursive: true, force: true });
  }
});
