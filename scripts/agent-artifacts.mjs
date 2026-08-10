#!/usr/bin/env node

/**
 * Canonical agent-guidance payload for every Graphoxide distribution.
 *
 * Graphify's wheel committed 133 mostly duplicated Markdown files and guarded
 * their package-data globs. Graphoxide expands the CLI's embedded guidance into
 * that inventory, so CLI installs, standalone archives, and VSIX files consume
 * the same canonical Markdown sources.
 */

import { constants as fsConstants, readFileSync } from 'node:fs';
import { lstat, mkdir, open, readdir, realpath, rm, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const repositoryRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const canonicalAssetRoot = path.join(repositoryRoot, 'crates', 'graphoxide-cli', 'assets');
export const AGENT_ARTIFACT_MAX_BYTES = 1024 * 1024;
const READ_CHUNK_BYTES = 64 * 1024;

const SKILL_FILES = Object.freeze([
  'skill-agents.md',
  'skill-aider.md',
  'skill-amp.md',
  'skill-claw.md',
  'skill-codex.md',
  'skill-copilot.md',
  'skill-devin.md',
  'skill-droid.md',
  'skill-kilo.md',
  'skill-kiro.md',
  'skill-opencode.md',
  'skill-pi.md',
  'skill-trae.md',
  'skill-windows.md',
  'skill.md',
]);

const REFERENCE_HOSTS = Object.freeze([
  'agents',
  'amp',
  'claude',
  'claw',
  'codex',
  'copilot',
  'droid',
  'kilo',
  'kiro',
  'opencode',
  'pi',
  'trae',
  'vscode',
  'windows',
]);

const REFERENCE_TOPICS = Object.freeze([
  'add-watch.md',
  'exports.md',
  'extraction-spec.md',
  'github-and-merge.md',
  'hooks.md',
  'query.md',
  'transcribe.md',
  'update.md',
]);

const ALWAYS_ON = Object.freeze([
  'agents-md.md',
  'antigravity-rules.md',
  'claude-md.md',
  'gemini-md.md',
  'kiro-steering.md',
  'vscode-instructions.md',
]);

export const artifactPaths = Object.freeze([
  ...SKILL_FILES,
  ...REFERENCE_HOSTS.flatMap((host) =>
    REFERENCE_TOPICS.map((topic) => `skills/${host}/references/${topic}`),
  ),
  ...ALWAYS_ON.map((file) => `always_on/${file}`),
]);

export function renderArtifact(relativePath) {
  validateManifest();
  if (!artifactPaths.includes(relativePath)) {
    throw new Error(`Unknown Graphoxide agent artifact: ${relativePath}`);
  }

  if (SKILL_FILES.includes(relativePath)) return readCanonicalAsset(relativePath);

  if (relativePath.startsWith('always_on/')) {
    return renderAlwaysOn(relativePath.slice('always_on/'.length));
  }

  const match = relativePath.match(/^skills\/([^/]+)\/references\/([^/]+)$/u);
  if (!match) throw new Error(`Unsupported Graphoxide agent artifact path: ${relativePath}`);
  return readCanonicalAsset(path.posix.join('references', match[2]));
}

export async function stageAgentArtifacts(outputDirectory) {
  validateManifest();
  const output = path.resolve(outputDirectory);
  let created = false;
  try {
    await mkdir(output, { mode: 0o700 });
    created = true;
    for (const relativePath of artifactPaths) {
      const destination = path.join(output, ...relativePath.split('/'));
      await mkdir(path.dirname(destination), { recursive: true, mode: 0o700 });
      await writeFile(destination, renderArtifact(relativePath), { encoding: 'utf8', flag: 'wx' });
    }
    await verifyAgentArtifacts(output);
  } catch (error) {
    if (created) await rm(output, { recursive: true, force: true });
    if (error?.code === 'EEXIST') {
      throw new Error(`Refusing to replace existing agent-artifact directory: ${output}`, { cause: error });
    }
    throw error;
  }
  return output;
}

export async function verifyAgentArtifacts(rootDirectory, hooks = {}) {
  validateManifest();
  const root = await realpath(path.resolve(rootDirectory));
  const actual = await filesBelow(root);
  const expected = [...artifactPaths].sort();
  if (JSON.stringify(actual) !== JSON.stringify(expected)) {
    throw new Error(`Agent artifact inventory mismatch: expected ${expected.length}, found ${actual.length}`);
  }
  for (const relativePath of expected) {
    const file = path.join(root, ...relativePath.split('/'));
    const content = await readVerifiedArtifact(
      root,
      file,
      relativePath,
      hooks,
    );
    if (content.length < 120 || !content.toLowerCase().includes('graphoxide')) {
      throw new Error(`Agent artifact is empty or malformed: ${relativePath}`);
    }
  }
  if (await realpath(root) !== root) {
    throw new Error(`Agent artifact root changed during verification: ${root}`);
  }
}

async function readVerifiedArtifact(root, file, relativePath, hooks) {
  await hooks.beforeArtifactOpen?.(file, relativePath);
  // Open before consulting path metadata so validation and reading stay bound
  // to one inode even if the pathname is replaced.
  const descriptor = await open(
    file,
    fsConstants.O_RDONLY | (fsConstants.O_NOFOLLOW ?? 0),
  );
  try {
    const opened = await descriptor.stat({ bigint: true });
    if (!opened.isFile() || opened.nlink !== 1n || opened.size < 0n) {
      throw new Error(`Agent artifact is not a single-link regular file: ${relativePath}`);
    }
    if (opened.size > BigInt(AGENT_ARTIFACT_MAX_BYTES)) {
      throw new Error(
        `Agent artifact exceeds its ${AGENT_ARTIFACT_MAX_BYTES}-byte ceiling: ${relativePath}`,
      );
    }

    await hooks.afterArtifactOpen?.(file, relativePath);

    const descriptorBeforePathCheck = await descriptor.stat({ bigint: true });
    const canonicalFile = await realpath(file);
    const pathBeforeRead = await lstat(file, { bigint: true });
    const descriptorAfterPathCheck = await descriptor.stat({ bigint: true });
    if (
      canonicalFile !== file ||
      !isPathBelow(root, canonicalFile) ||
      pathBeforeRead.isSymbolicLink() ||
      !pathBeforeRead.isFile() ||
      pathBeforeRead.nlink !== 1n ||
      !sameSnapshot(opened, pathBeforeRead) ||
      !sameSnapshot(opened, descriptorBeforePathCheck) ||
      !sameSnapshot(descriptorBeforePathCheck, descriptorAfterPathCheck)
    ) {
      throw new Error(`Agent artifact escaped its verified root before reading: ${relativePath}`);
    }

    const chunks = [];
    let total = 0;
    while (true) {
      const buffer = Buffer.allocUnsafe(
        Math.min(READ_CHUNK_BYTES, AGENT_ARTIFACT_MAX_BYTES + 1 - total),
      );
      const { bytesRead } = await descriptor.read(buffer, 0, buffer.length, null);
      if (bytesRead === 0) break;
      total += bytesRead;
      if (total > AGENT_ARTIFACT_MAX_BYTES) {
        throw new Error(
          `Agent artifact exceeds its ${AGENT_ARTIFACT_MAX_BYTES}-byte ceiling: ${relativePath}`,
        );
      }
      chunks.push(buffer.subarray(0, bytesRead));
    }

    const after = await descriptor.stat({ bigint: true });
    const canonicalAfter = await realpath(file);
    const pathAfter = await lstat(file, { bigint: true });
    const descriptorFinal = await descriptor.stat({ bigint: true });
    if (
      canonicalAfter !== file ||
      !isPathBelow(root, canonicalAfter) ||
      pathAfter.isSymbolicLink() ||
      !pathAfter.isFile() ||
      pathAfter.nlink !== 1n ||
      !sameSnapshot(opened, after) ||
      !sameSnapshot(opened, pathAfter) ||
      !sameSnapshot(after, descriptorFinal) ||
      after.size !== BigInt(total) ||
      descriptorFinal.size !== BigInt(total)
    ) {
      throw new Error(`Agent artifact changed while being verified: ${relativePath}`);
    }
    return Buffer.concat(chunks, total).toString('utf8');
  } finally {
    await descriptor.close();
  }
}

function sameIdentity(left, right) {
  return left.dev === right.dev && left.ino === right.ino;
}

function sameSnapshot(left, right) {
  return (
    sameIdentity(left, right) &&
    left.size === right.size &&
    left.nlink === right.nlink &&
    left.mtimeNs === right.mtimeNs &&
    left.ctimeNs === right.ctimeNs
  );
}

function isPathBelow(directory, candidate) {
  const relative = path.relative(directory, candidate);
  return (
    relative !== '' &&
    relative !== '..' &&
    !relative.startsWith(`..${path.sep}`) &&
    !path.isAbsolute(relative)
  );
}

function validateManifest() {
  const unique = new Set(artifactPaths);
  if (artifactPaths.length !== 133 || unique.size !== 133) {
    throw new Error(`Graphoxide must declare exactly 133 unique agent artifacts, found ${artifactPaths.length}/${unique.size}`);
  }
  for (const relativePath of artifactPaths) {
    if (path.posix.isAbsolute(relativePath) || relativePath.split('/').includes('..') || relativePath.includes('\\')) {
      throw new Error(`Unsafe Graphoxide agent artifact path: ${relativePath}`);
    }
  }
}

async function filesBelow(directory, prefix = '') {
  const result = [];
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    const relative = prefix ? `${prefix}/${entry.name}` : entry.name;
    const absolute = path.join(directory, entry.name);
    if (entry.isSymbolicLink()) throw new Error(`Symlink is not allowed in agent artifacts: ${relative}`);
    if (entry.isDirectory()) result.push(...await filesBelow(absolute, relative));
    else if (entry.isFile()) result.push(relative);
    else throw new Error(`Unsupported filesystem entry in agent artifacts: ${relative}`);
  }
  return result.sort();
}

function readCanonicalAsset(relativePath) {
  const file = path.join(canonicalAssetRoot, ...relativePath.split('/'));
  return readFileSync(file, 'utf8');
}

function renderAlwaysOn(file) {
  const hosts = {
    'agents-md.md': 'AGENTS.md-compatible coding agents',
    'antigravity-rules.md': 'Google Antigravity',
    'claude-md.md': 'Claude Code',
    'gemini-md.md': 'Gemini CLI',
    'kiro-steering.md': 'Kiro steering',
    'vscode-instructions.md': 'VS Code and GitHub Copilot',
  };
  const host = hosts[file];
  if (!host) throw new Error(`Unknown Graphoxide always-on artifact: ${file}`);
  return `# Graphoxide repository guidance for ${host}

When \`graphoxide-out/graph.json\` exists, use \`graphoxide query\`, \`path\`, \`explain\`, or \`affected\` before scanning the repository broadly. Rebuild with \`graphoxide update .\` after source changes, and use \`graphoxide audit . --strict\` to detect silent graph loss. Cite source locations returned by the graph instead of presenting inferred relationships as facts.
`;
}

async function main(arguments_) {
  if (arguments_.length === 1 && arguments_[0] === '--check') {
    validateManifest();
    for (const relativePath of artifactPaths) renderArtifact(relativePath);
    process.stdout.write(`[agent-artifacts] validated ${artifactPaths.length} artifacts\n`);
    return;
  }
  if (arguments_.length === 2 && arguments_[0] === '--out' && arguments_[1]) {
    const output = await stageAgentArtifacts(arguments_[1]);
    process.stdout.write(`[agent-artifacts] staged ${artifactPaths.length} artifacts in ${output}\n`);
    return;
  }
  throw new Error('Usage: node scripts/agent-artifacts.mjs --check | --out <directory>');
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main(process.argv.slice(2)).catch((error) => {
    process.stderr.write(`${error.message}\n`);
    process.exitCode = 2;
  });
}
