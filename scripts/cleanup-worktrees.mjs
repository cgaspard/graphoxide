#!/usr/bin/env node

// Safe pruner for agent issue worktrees.
//
// Worktrees are created under the sibling directory `graphoxide-worktrees/`
// (see AGENTS.md, "Issues, branches, and worktrees"). This script removes
// worktrees whose branch has been merged into `main` and whose working tree
// is clean. It never touches:
//   - the main checkout,
//   - worktrees outside the managed sibling directory,
//   - detached-HEAD worktrees,
//   - worktrees with uncommitted changes (including untracked files).
//
// Usage:
//   node scripts/cleanup-worktrees.mjs [--dry-run] [--force] [branch ...]
//
//   (no branches)  sweep mode: classify every managed worktree and remove
//                  the ones that are clean and merged into main. Used by the
//                  post-merge hook.
//   (branch ...)   explicit mode: remove the worktree for each named branch
//                  when it is clean; --force also allows unmerged branches.
//                  The dirty check is never bypassed.

import { execFileSync } from 'node:child_process';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

export const WORKTREES_DIR_NAME = 'graphoxide-worktrees';

// ---------------------------------------------------------------------------
// Pure helpers (unit-tested in cleanup-worktrees.test.mjs)
// ---------------------------------------------------------------------------

/**
 * Parse `git worktree list --porcelain` output.
 *
 * Each record is separated by a blank line and contains:
 *   worktree <path>
 *   HEAD <sha>
 *   branch refs/heads/<name>   (absent for detached HEAD)
 *   bare                        (absent for normal worktrees)
 */
export function parseWorktreeList(text) {
  const worktrees = [];
  for (const block of text.split(/\n\n+/u)) {
    const lines = block.split(/\r?\n/u).filter((line) => line.trim() !== '');
    if (lines.length === 0) continue;
    const worktree = {};
    for (const line of lines) {
      if (line.startsWith('worktree ')) worktree.path = line.slice('worktree '.length);
      else if (line.startsWith('HEAD ')) worktree.head = line.slice('HEAD '.length);
      else if (line.startsWith('branch ')) {
        const ref = line.slice('branch '.length);
        worktree.branch = ref.replace(/^refs\/heads\//u, '');
      } else if (line === 'bare') worktree.bare = true;
    }
    if (worktree.path) worktrees.push(worktree);
  }
  return worktrees;
}

/** A worktree is clean when `git status --porcelain` is empty. */
export function isClean(statusPorcelain) {
  return statusPorcelain.trim() === '';
}

/**
 * `git cherry main <branch>` prints one line per commit of the branch that
 * is not reachable from main: "-" when an equivalent patch already exists in
 * main (covers merge commits and GitHub squash merges), "+" when it does not.
 *
 * Returns:
 *   - 'unmerged'          the branch has commits main lacks,
 *   - 'merged'            every commit has an equivalent patch in main,
 *   - 'no-unique-commits' cherry printed nothing: the branch is fully merged
 *                         (rebase-style) or was scaffolded with no commits.
 *                         Either way, a clean worktree holds no work.
 */
export function mergeStateAgainstMain(cherryOutput) {
  const lines = cherryOutput.split(/\r?\n/u).filter((line) => line.trim() !== '');
  if (lines.length === 0) return 'no-unique-commits';
  return lines.every((line) => line.startsWith('-')) ? 'merged' : 'unmerged';
}

/** True when `worktreePath` is the managed root or inside it. */
export function isUnderManagedRoot(worktreePath, managedRoot) {
  if (managedRoot === undefined) return false;
  const resolved = path.resolve(worktreePath);
  const root = path.resolve(managedRoot);
  return resolved === root || resolved.startsWith(root + path.sep);
}

/**
 * Decide what to do with one worktree.
 *
 * @param {object} input
 * @param {boolean} input.isMain         the main checkout
 * @param {boolean} input.underRoot      inside the managed sibling directory
 * @param {string|undefined} input.branch branch name (undefined = detached)
 * @param {boolean} input.clean          working tree clean (no untracked)
 * @param {'merged'|'no-unique-commits'|'unmerged'} input.mergeState
 * @param {boolean} [input.force]        explicit-mode override for unmerged
 * @returns {{action: 'remove'|'keep'|'skip', reason: string}}
 */
export function classifyWorktree({ isMain, underRoot, branch, clean, mergeState, force = false }) {
  if (isMain) return { action: 'skip', reason: 'main checkout' };
  if (!underRoot) return { action: 'skip', reason: 'outside managed worktree root' };
  if (branch === undefined || branch === '') return { action: 'skip', reason: 'detached HEAD' };
  if (!clean) return { action: 'keep', reason: 'dirty worktree (uncommitted or untracked files)' };
  if (mergeState === 'unmerged' && !force) {
    return { action: 'keep', reason: 'branch has unmerged commits' };
  }
  if (mergeState === 'merged') return { action: 'remove', reason: 'merged into main and clean' };
  if (mergeState === 'no-unique-commits') {
    return {
      action: 'remove',
      reason: 'no unique commits (merged or never started) and clean',
    };
  }
  return { action: 'remove', reason: 'forced' };
}

// ---------------------------------------------------------------------------
// Git plumbing
// ---------------------------------------------------------------------------

function git(cwd, ...args) {
  return execFileSync('git', args, { cwd, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] });
}

function mainRootFrom(cwd) {
  // --git-common-dir points at <main root>/.git for both main and linked
  // worktrees, so its parent directory is always the main checkout.
  const commonDir = git(cwd, 'rev-parse', '--path-format=absolute', '--git-common-dir').trim();
  return path.dirname(commonDir);
}

export function collectWorktreeState(repoRoot) {
  git(repoRoot, 'worktree', 'prune');
  const records = parseWorktreeList(git(repoRoot, 'worktree', 'list', '--porcelain'));
  const managedRoot = path.join(repoRoot, '..', WORKTREES_DIR_NAME);
  const mainRoot = mainRootFrom(repoRoot);
  let hasMain = true;
  try {
    git(repoRoot, 'rev-parse', '--verify', '--quiet', 'refs/heads/main');
  } catch {
    hasMain = false;
  }

  return records.map((record) => {
    const entry = {
      path: record.path,
      branch: record.branch,
      isMain: path.resolve(record.path) === path.resolve(mainRoot),
      underRoot: isUnderManagedRoot(record.path, managedRoot),
      clean: false,
      mergeState: 'unmerged',
    };
    if (!entry.isMain && entry.branch) {
      entry.clean = isClean(git(record.path, 'status', '--porcelain'));
      entry.mergeState = hasMain
        ? mergeStateAgainstMain(git(mainRoot, 'cherry', 'main', entry.branch))
        : 'unmerged';
    }
    return entry;
  });
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

function run(args) {
  const flags = new Set();
  const branches = [];
  for (const arg of args) {
    if (arg === '--dry-run') flags.add('dry-run');
    else if (arg === '--force') flags.add('force');
    else if (arg === '--help' || arg === '-h') flags.add('help');
    else branches.push(arg);
  }

  if (flags.has('help')) {
    process.stdout.write(
      'usage: node scripts/cleanup-worktrees.mjs [--dry-run] [--force] [branch ...]\n',
    );
    return 0;
  }

  const cwd = process.cwd();
  const repoRoot = git(cwd, 'rev-parse', '--show-toplevel').trim();
  const state = collectWorktreeState(repoRoot);
  const removed = [];
  const kept = [];
  const seenBranches = new Set();
  const dryRun = flags.has('dry-run');
  const force = flags.has('force');

  const select = (entry) => {
    if (branches.length > 0) {
      if (!entry.branch || !branches.includes(entry.branch)) return false;
      return !entry.isMain && entry.underRoot;
    }
    return true;
  };

  for (const entry of state) {
    if (!select(entry)) continue;
    const verdict = classifyWorktree({
      isMain: entry.isMain,
      underRoot: entry.underRoot,
      branch: entry.branch,
      clean: entry.clean,
      mergeState: entry.mergeState,
      force,
    });
    if (verdict.action !== 'remove') {
      kept.push(`${entry.path} (${entry.branch ?? 'detached'}): ${verdict.reason}`);
      continue;
    }
    if (entry.branch) seenBranches.add(entry.branch);
    const label = dryRun ? 'would remove' : 'removed';
    if (!dryRun) {
      // `git worktree remove` without --force refuses dirty trees, which is
      // the second layer of protection against the status check above.
      git(repoRoot, 'worktree', 'remove', entry.path);
      git(repoRoot, 'branch', '-D', entry.branch);
    }
    removed.push(`${label} ${entry.path} (${entry.branch}): ${verdict.reason}`);
  }

  for (const branch of branches) {
    if (!seenBranches.has(branch)) {
      process.stdout.write(
        `skipped ${branch}: no managed worktree found for this branch\n`,
      );
    }
  }
  for (const line of removed) process.stdout.write(`${line}\n`);
  for (const line of kept) process.stdout.write(`kept ${line}\n`);
  if (removed.length === 0 && kept.length === 0) {
    process.stdout.write('no managed worktrees to clean up\n');
  }
  process.stdout.write(
    `worktree cleanup: ${removed.length} removed, ${kept.length} kept\n`,
  );
  return 0;
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  try {
    process.exitCode = run(process.argv.slice(2));
  } catch (error) {
    process.stderr.write(`cleanup-worktrees: ${error instanceof Error ? error.message : String(error)}\n`);
    process.exitCode = 1;
  }
}

export { run as runCleanup };
