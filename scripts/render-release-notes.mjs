#!/usr/bin/env node

import { readFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const repositoryRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const requestedVersion = process.argv[2];
const checkOnly = process.argv.includes('--check');

if (!requestedVersion || process.argv.some((argument, index) => index > 2 && argument !== '--check')) {
  fail('Usage: node scripts/render-release-notes.mjs <version|--current> [--check]');
}

const cargoVersion = await workspaceVersion();
const version = requestedVersion === '--current' ? cargoVersion : requestedVersion;
const vscodePackage = JSON.parse(await readFile(path.join(repositoryRoot, 'editors/vscode/package.json'), 'utf8'));
if (cargoVersion !== version) fail(`Cargo workspace version ${cargoVersion} does not match release ${version}`);
if (vscodePackage.version !== version) fail(`VS Code extension version ${vscodePackage.version} does not match release ${version}`);

const components = [
  ['cli', 'Graphoxide CLI'],
  ['vscode', 'Graphoxide for VS Code'],
];
const notes = [];
for (const [directory, title] of components) {
  const file = path.join(repositoryRoot, 'releasenotes', directory, `${version}.yaml`);
  let text;
  try {
    text = await readFile(file, 'utf8');
  } catch (error) {
    if (error?.code === 'ENOENT') fail(`Missing ${path.relative(repositoryRoot, file)}`);
    throw error;
  }
  const data = parseYaml(text);
  if (data.version !== version) {
    fail(`${path.relative(repositoryRoot, file)} declares version ${data.version ?? '(missing)'}, expected ${version}`);
  }
  notes.push({ title, data });
}

if (!checkOnly) process.stdout.write(render(version, notes));

async function workspaceVersion() {
  const cargo = await readFile(path.join(repositoryRoot, 'Cargo.toml'), 'utf8');
  const marker = '[workspace.package]';
  const start = cargo.indexOf(marker);
  if (start < 0) fail('Could not find [workspace.package] in Cargo.toml');
  const remainder = cargo.slice(start + marker.length);
  const nextSection = remainder.search(/^\[/mu);
  const section = nextSection >= 0 ? remainder.slice(0, nextSection) : remainder;
  const found = section?.match(/^version\s*=\s*"([^"]+)"\s*$/mu)?.[1];
  if (!found) fail('Could not read [workspace.package] version from Cargo.toml');
  return found;
}

function render(releaseVersion, componentNotes) {
  const releaseDate = componentNotes.map(({ data }) => data.date).find(Boolean);
  const lines = [`# Graphoxide v${releaseVersion}`, ''];
  if (releaseDate) lines.push(`_Released ${releaseDate}_`, '');

  for (const { title, data } of componentNotes) {
    lines.push(`## ${title}`, '');
    appendSection(lines, 'Highlights', data.highlights);
    appendSection(lines, 'Added', data.added);
    appendSection(lines, 'Changed', data.changed);
    appendSection(lines, 'Fixed', data.fixed);
    appendSection(lines, 'Removed', data.removed);
  }

  lines.push(
    '## Install',
    '',
    'This release contains standalone Graphoxide archives and platform-specific VSIX packages for macOS, Linux, and Windows on x64 and arm64. Every VSIX contains the matching standalone Graphoxide executable.',
    '',
    'Verify downloaded files against `SHA256SUMS`, then either place `graphoxide` on your `PATH` or install the VS Code package:',
    '',
    '```bash',
    `code --install-extension graphoxide-vscode-<platform>-${releaseVersion}.vsix`,
    '```',
    '',
  );
  return `${lines.join('\n')}\n`;
}

function appendSection(lines, title, values) {
  if (!Array.isArray(values) || values.length === 0) return;
  lines.push(`### ${title}`);
  for (const value of values) lines.push(`- ${value}`);
  lines.push('');
}

function parseYaml(text) {
  const result = {};
  let currentList;
  for (const raw of text.split(/\r?\n/u)) {
    const line = raw.replace(/\s+$/u, '');
    if (!line.trim() || line.trimStart().startsWith('#')) continue;
    const list = line.match(/^\s*-\s+(.*)$/u);
    if (list && currentList) {
      currentList.push(unquote(list[1]));
      continue;
    }
    const property = line.match(/^([A-Za-z_][A-Za-z0-9_]*)\s*:\s*(.*)$/u);
    if (!property) fail(`Unsupported release-note YAML line: ${line}`);
    const [, key, rawValue] = property;
    if (rawValue === '') {
      result[key] = [];
      currentList = result[key];
    } else if (rawValue === '[]') {
      result[key] = [];
      currentList = undefined;
    } else {
      result[key] = unquote(rawValue);
      currentList = undefined;
    }
  }
  return result;
}

function unquote(value) {
  const trimmed = value.trim();
  if ((trimmed.startsWith('"') && trimmed.endsWith('"')) || (trimmed.startsWith("'") && trimmed.endsWith("'"))) {
    return trimmed.slice(1, -1);
  }
  return trimmed;
}

function fail(message) {
  console.error(message);
  process.exit(2);
}
