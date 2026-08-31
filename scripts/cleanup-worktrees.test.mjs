// Unit tests for scripts/cleanup-worktrees.mjs. The pure classification logic
// is tested directly; one end-to-end test drives the real script against a
// temporary repository with real worktrees, so the post-merge hook path stays
// exercised in the fast gate.

import test from 'node:test';
import assert from 'node:assert/strict';
import { execFileSync, spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import {
  classifyWorktree,
  isClean,
  isUnderManagedRoot,
  mergeStateAgainstMain,
  parseWorktreeList,
  WORKTREES_DIR_NAME,
} from './cleanup-worktrees.mjs';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const scriptPath = path.join(root, 'scripts', 'cleanup-worktrees.mjs');

test('parseWorktreeList handles main, linked, and detached worktrees', () => {
  const text = [
    'worktree /repo',
    'HEAD abcdef0123456789abcdef0123456789abcdef01',
    'branch refs/heads/main',
    '',
    'worktree /repo-worktrees/42-universal-capability-indexing',
    'HEAD 1234567890abcdef1234567890abcdef12345678',
    'branch refs/heads/agent/42-universal-capability-indexing',
    '',
    'worktree /elsewhere/detached',
    'HEAD deadbeefdeadbeefdeadbeefdeadbeefdeadbeef',
    '',
  ].join('\n');
  const worktrees = parseWorktreeList(text);
  assert.equal(worktrees.length, 3);
  assert.equal(worktrees[0].branch, 'main');
  assert.equal(worktrees[1].branch, 'agent/42-universal-capability-indexing');
  assert.equal(worktrees[2].branch, undefined);
});

test('isClean treats empty porcelain output as clean', () => {
  assert.equal(isClean(''), true);
  assert.equal(isClean('\n'), true);
  assert.equal(isClean(' M crates/foo.rs\n'), false);
  assert.equal(isClean('?? untracked.txt\n'), false);
});

test('mergeStateAgainstMain distinguishes merged, never started, and unmerged', () => {
  assert.equal(mergeStateAgainstMain(''), 'no-unique-commits');
  assert.equal(
    mergeStateAgainstMain('- abc123abc123abc123abc123abc123abc123abc1\n'),
    'merged',
  );
  assert.equal(
    mergeStateAgainstMain(
      '- abc123abc123abc123abc123abc123abc123abc1\n+ def456def456def456def456def456def456def456def456\n',
    ),
    'unmerged',
  );
});

test('isUnderManagedRoot matches the root itself and children only', () => {
  const managed = '/repo-worktrees';
  assert.equal(isUnderManagedRoot('/repo-worktrees', managed), true);
  assert.equal(isUnderManagedRoot('/repo-worktrees/7-fix', managed), true);
  assert.equal(isUnderManagedRoot('/repo-worktrees-evil/7-fix', managed), false);
  assert.equal(isUnderManagedRoot('/repo-worktrees/7-fix', undefined), false);
});

test('classifyWorktree protects the main checkout and foreign paths', () => {
  const base = { clean: true, mergeState: 'merged', branch: 'agent/1-x' };
  assert.equal(
    classifyWorktree({ ...base, isMain: true, underRoot: true }).action,
    'skip',
  );
  assert.equal(
    classifyWorktree({ ...base, isMain: false, underRoot: false }).action,
    'skip',
  );
  assert.equal(
    classifyWorktree({ ...base, isMain: false, underRoot: true, branch: undefined }).action,
    'skip',
  );
});

test('classifyWorktree keeps dirty or unmerged worktrees, removes clean merged ones', () => {
  const base = { isMain: false, underRoot: true, branch: 'agent/1-x' };
  assert.equal(classifyWorktree({ ...base, clean: false, mergeState: 'merged' }).action, 'keep');
  assert.equal(classifyWorktree({ ...base, clean: true, mergeState: 'unmerged' }).action, 'keep');
  assert.equal(classifyWorktree({ ...base, clean: true, mergeState: 'merged' }).action, 'remove');
  assert.equal(
    classifyWorktree({ ...base, clean: true, mergeState: 'no-unique-commits' }).action,
    'remove',
  );
  assert.equal(
    classifyWorktree({ ...base, clean: true, mergeState: 'unmerged', force: true }).action,
    'remove',
  );
  // force never bypasses the dirty check
  assert.equal(
    classifyWorktree({ ...base, clean: false, mergeState: 'merged', force: true }).action,
    'keep',
  );
});

// ---------------------------------------------------------------------------
// End-to-end: drive the real script against a temporary repository.
// ---------------------------------------------------------------------------

function git(cwd, ...args) {
  return execFileSync('git', args, { cwd, encoding: 'utf8' });
}

function commit(cwd, file, text, message) {
  fs.writeFileSync(path.join(cwd, file), text, 'utf8');
  git(cwd, 'add', file);
  git(
    cwd,
    '-c',
    'user.email=cleanup@test',
    '-c',
    'user.name=cleanup',
    'commit',
    '-m',
    message,
  );
}

function runScript(cwd, ...args) {
  const result = spawnSync(process.execPath, [scriptPath, ...args], {
    cwd,
    encoding: 'utf8',
  });
  assert.equal(result.status, 0, `script failed: ${result.stderr}`);
  return result.stdout;
}

test('end-to-end: sweep removes merged clean worktrees, keeps dirty and unmerged ones', () => {
  const base = fs.mkdtempSync(path.join(os.tmpdir(), 'cleanup-worktrees-'));
  const repo = path.join(base, 'repo');
  const worktreesRoot = path.join(base, WORKTREES_DIR_NAME);
  fs.mkdirSync(repo, { recursive: true });

  git(repo, 'init', '-b', 'main');
  git(repo, 'config', 'commit.gpgsign', 'false');
  commit(repo, 'README.md', 'main baseline\n', 'baseline');

  const merged = path.join(worktreesRoot, '1-merged-clean');
  git(repo, 'worktree', 'add', merged, '-b', 'agent/1-merged-clean');
  commit(merged, 'feature.md', 'merged feature\n', 'feature');
  git(repo, 'merge', '--no-ff', '-m', 'merge 1', 'agent/1-merged-clean');

  const dirty = path.join(worktreesRoot, '2-dirty-merged');
  git(repo, 'worktree', 'add', dirty, '-b', 'agent/2-dirty-merged');
  commit(dirty, 'feature.md', 'dirty feature\n', 'feature');
  git(repo, 'merge', '--no-ff', '-m', 'merge 2', 'agent/2-dirty-merged');
  fs.writeFileSync(path.join(dirty, 'wip.md'), 'uncommitted\n');

  const unmerged = path.join(worktreesRoot, '3-unmerged-clean');
  git(repo, 'worktree', 'add', unmerged, '-b', 'agent/3-unmerged-clean');
  commit(unmerged, 'feature.md', 'unmerged feature\n', 'feature');

  // Scaffolded from main with no commits: the worktree holds no work, so the
  // sweep removes it (the "never started" case).
  const neverStarted = path.join(worktreesRoot, '4-never-started');
  git(repo, 'worktree', 'add', neverStarted, '-b', 'agent/4-never-started');

  try {
    const dry = runScript(repo, '--dry-run');
    assert.match(dry, /would remove .*1-merged-clean \(agent\/1-merged-clean\)/u);
    assert.match(dry, /would remove .*4-never-started \(agent\/4-never-started\)/u);
    assert.match(dry, /no unique commits/u);
    assert.match(dry, /kept .*2-dirty-merged/u);
    assert.match(dry, /kept .*3-unmerged-clean/u);
    assert.ok(fs.existsSync(merged), 'dry-run must not remove anything');

    const out = runScript(repo);
    assert.match(out, /removed .*1-merged-clean \(agent\/1-merged-clean\)/u);
    assert.match(out, /removed .*4-never-started \(agent\/4-never-started\)/u);
    assert.match(out, /kept .*2-dirty-merged/u);
    assert.match(out, /kept .*3-unmerged-clean/u);
    assert.equal(fs.existsSync(merged), false);
    assert.equal(fs.existsSync(neverStarted), false);
    assert.ok(fs.existsSync(dirty), 'dirty worktree must survive the sweep');
    assert.ok(fs.existsSync(unmerged), 'unmerged worktree must survive the sweep');
    let branchStillExists = true;
    try {
      git(repo, 'show-ref', '--verify', 'refs/heads/agent/1-merged-clean');
    } catch {
      branchStillExists = false;
    }
    assert.equal(branchStillExists, false, 'merged branch must be deleted');

    // explicit mode with --force removes the unmerged branch's worktree
    const forced = runScript(repo, '--force', 'agent/3-unmerged-clean');
    assert.match(forced, /removed .*3-unmerged-clean \(agent\/3-unmerged-clean\)/u);
    assert.equal(fs.existsSync(unmerged), false);

    // explicit mode without a matching worktree reports it
    const missing = runScript(repo, 'agent/99-absent');
    assert.match(missing, /no managed worktree found for this branch/u);

    // the dirty worktree is removed once it becomes clean (agent committed)
    fs.rmSync(path.join(dirty, 'wip.md'));
    const afterClean = runScript(repo);
    assert.match(afterClean, /removed .*2-dirty-merged \(agent\/2-dirty-merged\)/u);
    assert.equal(fs.existsSync(dirty), false);
  } finally {
    fs.rmSync(base, { recursive: true, force: true });
  }
});
