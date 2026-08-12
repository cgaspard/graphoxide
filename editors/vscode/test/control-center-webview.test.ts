import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import path from 'node:path';
import test from 'node:test';

const controlCenterPath = path.join(process.cwd(), 'src', 'control-center.ts');

test('uses an explicit dashboard flow without stretching unrelated cards', async () => {
  const source = await readFile(controlCenterPath, 'utf8');
  const renderStart = source.indexOf("document.getElementById('content').innerHTML");
  const renderEnd = source.indexOf('bindActions();', renderStart);
  const render = source.slice(renderStart, renderEnd);

  assert.ok(renderStart >= 0 && renderEnd > renderStart);
  assert.match(
    render,
    /<main class="dashboard">' \+ graphCard\(state\) \+ '<div class="dashboard-secondary">' \+ managedCard\(state\) \+ aiCard\(state\) \+ '<\/div>' \+ mcpCard\(state\) \+ '<\/main>/u,
  );
  assert.match(source, /\.dashboard \{ display: grid; gap: 14px; \}/u);
  assert.match(
    source,
    /\.dashboard-secondary \{ display: grid; grid-template-columns: repeat\(2, minmax\(0, 1fr\)\); align-items: start; gap: 14px; \}/u,
  );
  assert.doesNotMatch(source, /\.wide\s*\{/u);
  assert.doesNotMatch(source, /<main class="grid">/u);
});

test('collapses the secondary cards and bounds long Control Center content', async () => {
  const source = await readFile(controlCenterPath, 'utf8');

  assert.match(
    source,
    /@media \(max-width: 760px\) \{[^\n]*body \{ padding: 18px; \}[^\n]*\.dashboard-secondary \{ grid-template-columns: 1fr; \}/u,
  );
  assert.match(source, /@media \(max-width: 420px\) \{[^\n]*dl \{ grid-template-columns: 1fr;/u);
  assert.match(source, /\.card \{ min-width: 0;/u);
  assert.match(source, /\.card-head > :first-child,[^\n]+\{ min-width: 0; \}/u);
  assert.match(source, /dd \{ min-width: 0; overflow-wrap: anywhere; \}/u);
  assert.match(source, /button \{ max-width: 100%;[^\n]+white-space: normal; overflow-wrap: anywhere;/u);
});

test('retains semantic sections, focus treatment, and forced-color borders', async () => {
  const source = await readFile(controlCenterPath, 'utf8');

  for (const heading of ['graph-heading', 'managed-heading', 'ai-heading', 'mcp-heading']) {
    assert.match(source, new RegExp(`<section class="card" aria-labelledby="${heading}"`, 'u'));
    assert.match(source, new RegExp(`<h2 id="${heading}">`, 'u'));
  }
  assert.match(source, /button:focus-visible \{ outline: 2px solid var\(--vscode-focusBorder\);/u);
  assert.match(source, /@media \(forced-colors: active\) \{ \.card, \.integration, \.scope, \.native, \.chip \{ border-color: CanvasText; \} \}/u);
});
