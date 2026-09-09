#!/usr/bin/env node

import { readdir, readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const SCRIPT_PATH = /\bnode(?:\s+--\S+)*\s+scripts\/([A-Za-z0-9_.-]+\.mjs)\b/gu;
const LOCAL_SCRIPT_IMPORT = /(?:from|import)\s*\(?\s*["']\.\/([A-Za-z0-9_.-]+\.mjs)["']/gu;
const TEST_THREADS_FLAG = '--test-threads';

function withoutComments(content) {
  return content
    .replace(/\/\*[\s\S]*?\*\//gu, '')
    .replace(/(^|\s)\/\/.*$/gmu, '$1')
    .replace(/(^|\s)#.*$/gmu, '$1');
}

function violationsIn(name, content) {
  const violations = [];
  // Error text is not executable configuration. Keep the complete reachable
  // source otherwise, including multiline argument arrays.
  const uncommented = withoutComments(content).replace(
    /\bthrow\s+new\s+Error\(\s*(?:"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*')\s*\)\s*;?/gsu,
    '',
  );
  for (const match of uncommented.matchAll(/--test-threads\s*(?:=|\s+)\s*([^\s'"\\]+)/gu)) {
    if (match[1] !== 'num-cpus') violations.push(`${name}: ${TEST_THREADS_FLAG}=${match[1]}`);
  }
  for (const [pattern, label] of [
    [/\bRUST_TEST_THREADS\s*(?:=|:)\s*["']?\d+["']?\b/u, 'fixed RUST_TEST_THREADS'],
    [/\bmax-threads\s*=\s*\d+\b/u, 'fixed max-threads'],
    [/\bCARGO_BUILD_JOBS\s*(?:=|:)\s*["']?\d+["']?\b/u, 'CARGO_BUILD_JOBS'],
    [/\bcargo\b[\s\S]{0,4096}?\b(?:test|build)\b[\s\S]{0,4096}?(?:--jobs(?:=|\s+)\d+\b|-j\s*\d+\b)/u, 'fixed cargo --jobs'],
    [/^\s*jobs\s*=\s*\d+\s*$/mu, 'fixed cargo config jobs'],
  ]) {
    if (pattern.test(uncommented)) violations.push(`${name}: ${label}`);
  }
  return violations;
}

export function parallelismViolations(files) {
  return Object.entries(files).flatMap(([name, content]) => violationsIn(name, content));
}

function referencedScripts(content) {
  return new Set([...content.matchAll(SCRIPT_PATH)].map((match) => match[1]));
}

function localScriptImports(content) {
  return new Set([...content.matchAll(LOCAL_SCRIPT_IMPORT)].map((match) => match[1]));
}

export function reachableRootScripts(seeds, sources) {
  const pending = [...seeds];
  const visited = new Set();
  while (pending.length) {
    const name = pending.pop();
    if (!name || visited.has(name) || name.endsWith('.test.mjs') || !(name in sources)) continue;
    visited.add(name);
    pending.push(...referencedScripts(sources[name]), ...localScriptImports(sources[name]));
  }
  return visited;
}

async function activeConfiguration(root) {
  const packageJson = await readFile(join(root, 'package.json'), 'utf8');
  const workflowDir = join(root, '.github/workflows');
  const workflowNames = (await readdir(workflowDir))
    .filter((name) => /\.ya?ml$/u.test(name));
  const workflows = await Promise.all(workflowNames.map(async (name) => [
    `.github/workflows/${name}`,
    await readFile(join(workflowDir, name), 'utf8'),
  ]));
  const files = {
    'package.json': packageJson,
    '.config/nextest.toml': await readFile(join(root, '.config/nextest.toml'), 'utf8'),
    '.cargo/config.toml': await readFile(join(root, '.cargo/config.toml'), 'utf8').catch((error) => {
      if (error.code === 'ENOENT') return '';
      throw error;
    }),
    ...Object.fromEntries(workflows),
  };
  // Only root `scripts/` entrypoints are in scope here. Workflow steps may run
  // identically named extension-local scripts under a different working directory.
  const scriptNames = (await readdir(join(root, 'scripts'))).filter((name) => name.endsWith('.mjs'));
  const sources = Object.fromEntries(await Promise.all(scriptNames.map(async (name) => [
    name,
    await readFile(join(root, 'scripts', name), 'utf8'),
  ])));
  const seeds = [...referencedScripts(packageJson), ...workflows.flatMap(([, content]) => [...referencedScripts(content)])];
  for (const name of reachableRootScripts(seeds, sources)) {
    files[`scripts/${name}`] = sources[name];
  }
  return files;
}

export async function checkConfiguredParallelism(root) {
  const violations = parallelismViolations(await activeConfiguration(root));
  if (violations.length) throw new Error(`active test/build configuration caps parallelism:\n${violations.join('\n')}`);
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const root = dirname(dirname(fileURLToPath(import.meta.url)));
  await checkConfiguredParallelism(root);
  console.log('test parallelism verification passed');
}
