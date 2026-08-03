#!/usr/bin/env node

/**
 * Canonical agent-guidance payload for every Graphoxide distribution.
 *
 * Graphify's wheel committed 133 mostly duplicated Markdown files and guarded
 * their package-data globs. Graphoxide expands the CLI's embedded guidance into
 * that inventory, so CLI installs, standalone archives, and VSIX files consume
 * the same canonical Markdown sources.
 */

import { readFileSync } from 'node:fs';
import { lstat, mkdir, readdir, readFile, rm, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const repositoryRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const canonicalAssetRoot = path.join(repositoryRoot, 'crates', 'graphoxide-cli', 'assets');

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
    await mkdir(output);
    created = true;
    for (const relativePath of artifactPaths) {
      const destination = path.join(output, ...relativePath.split('/'));
      await mkdir(path.dirname(destination), { recursive: true });
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

export async function verifyAgentArtifacts(rootDirectory) {
  validateManifest();
  const root = path.resolve(rootDirectory);
  const actual = await filesBelow(root);
  const expected = [...artifactPaths].sort();
  if (JSON.stringify(actual) !== JSON.stringify(expected)) {
    throw new Error(`Agent artifact inventory mismatch: expected ${expected.length}, found ${actual.length}`);
  }
  for (const relativePath of expected) {
    const file = path.join(root, ...relativePath.split('/'));
    const info = await lstat(file);
    if (!info.isFile() || info.isSymbolicLink()) {
      throw new Error(`Agent artifact is not a regular file: ${relativePath}`);
    }
    const content = await readFile(file, 'utf8');
    if (content.length < 120 || !content.toLowerCase().includes('graphoxide')) {
      throw new Error(`Agent artifact is empty or malformed: ${relativePath}`);
    }
  }
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
