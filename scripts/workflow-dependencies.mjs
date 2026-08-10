#!/usr/bin/env node

import { readdirSync, readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import { parse } from 'yaml';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const workflowsDirectory = path.join(root, '.github', 'workflows');
const actionReference = /^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+(?:\/[A-Za-z0-9_.-]+)*@([0-9a-f]{40})$/u;
const versionComment = /^(?:v\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?|master \(\d{4}-\d{2}-\d{2}\))$/u;

export function inspectWorkflowActionPins(text, source = '<workflow>') {
  const errors = [];
  const semanticReferences = [];
  let document;
  try {
    document = parse(text, { maxAliasCount: 0, uniqueKeys: true });
  } catch (error) {
    errors.push(`${source}: invalid workflow YAML: ${error instanceof Error ? error.message : String(error)}`);
    return { actions: 0, errors };
  }
  collectUsesReferences(document, semanticReferences, errors, source);

  const canonicalReferences = [];
  let actions = 0;

  for (const [index, line] of text.split(/\r?\n/u).entries()) {
    if (!/^\s*(?:-\s*)?uses\s*:/u.test(line)) continue;
    const match = /^\s*(?:-\s*)?uses:\s*([^\s#]+)(?:\s+#\s*(.+?))?\s*$/u.exec(line);
    if (!match) {
      errors.push(`${source}:${index + 1}: uses must be an unquoted action reference`);
      continue;
    }

    const reference = match[1];
    const comment = match[2];
    canonicalReferences.push(reference);
    if (reference.startsWith('./')) continue;
    actions += 1;

    if (!actionReference.test(reference)) {
      errors.push(`${source}:${index + 1}: external actions must use an immutable 40-character commit SHA`);
    }
    if (!comment || !versionComment.test(comment)) {
      errors.push(`${source}:${index + 1}: pinned actions require an exact release comment`);
    }
  }

  if (!sameReferences(semanticReferences, canonicalReferences)) {
    errors.push(`${source}: every uses key must use the canonical one-action-per-line form`);
  }

  return { actions, errors };
}

function collectUsesReferences(value, references, errors, source, depth = 0, seen = { nodes: 0 }) {
  seen.nodes += 1;
  if (depth > 64 || seen.nodes > 10_000) {
    errors.push(`${source}: workflow structure exceeds the pin validator limits`);
    return;
  }
  if (Array.isArray(value)) {
    for (const child of value) collectUsesReferences(child, references, errors, source, depth + 1, seen);
    return;
  }
  if (!value || typeof value !== 'object') return;
  for (const [key, child] of Object.entries(value)) {
    if (key === 'uses') {
      if (typeof child !== 'string') {
        errors.push(`${source}: uses values must be strings`);
      } else {
        references.push(child);
      }
    }
    collectUsesReferences(child, references, errors, source, depth + 1, seen);
  }
}

function sameReferences(left, right) {
  if (left.length !== right.length) return false;
  const leftSorted = [...left].sort();
  const rightSorted = [...right].sort();
  return leftSorted.every((value, index) => value === rightSorted[index]);
}

export function inspectRepositoryWorkflows() {
  const files = readdirSync(workflowsDirectory, { withFileTypes: true })
    .filter((entry) => entry.isFile() && /\.ya?ml$/u.test(entry.name))
    .map((entry) => entry.name)
    .sort();
  const errors = [];
  let actions = 0;

  for (const file of files) {
    const result = inspectWorkflowActionPins(
      readFileSync(path.join(workflowsDirectory, file), 'utf8'),
      `.github/workflows/${file}`,
    );
    actions += result.actions;
    errors.push(...result.errors);
  }

  return { actions, errors, files };
}

if (path.resolve(process.argv[1] ?? '') === fileURLToPath(import.meta.url)) {
  const result = inspectRepositoryWorkflows();
  if (result.errors.length > 0) {
    process.stderr.write(`${result.errors.join('\n')}\n`);
    process.exit(1);
  }
  process.stdout.write(`Validated ${result.actions} immutable action references across ${result.files.length} workflows.\n`);
}
