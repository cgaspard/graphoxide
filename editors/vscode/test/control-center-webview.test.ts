import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import path from 'node:path';
import test from 'node:test';

const controlCenterPath = path.join(process.cwd(), 'src', 'control-center.ts');

test('uses a compact status-first layout with inline build progress and cancel', async () => {
  const source = await readFile(controlCenterPath, 'utf8');

  // Status line replaces chips overview
  assert.match(source, /\.status-line \{/u);
  assert.match(source, /\.status-line\.ready \{/u);
  assert.match(source, /\.status-line\.error \{/u);
  assert.match(source, /\.status-line\.missing \{/u);

  // Build progress banner with spinner and cancel button
  assert.match(source, /\.build-progress \{/u);
  assert.match(source, /\.build-progress \.spinner \{/u);
  assert.match(source, /@keyframes spin \{/u);
  assert.match(source, /\.build-progress button\.cancel \{/u);
  assert.match(source, /'cancelBuild'/u);

  // Settings cards side-by-side
  assert.match(source, /\.settings-row \{ display: grid; grid-template-columns: 1fr 1fr/u);
  assert.match(source, /\.settings-card/u);

  // MCP compact pills
  assert.match(source, /\.mcp-pill \{/u);
  assert.match(source, /\.mcp-pills \{/u);

  // Number abbreviation
  assert.match(source, /function abbrevNumber/u);

  // Progress message handler
  assert.match(source, /message\.type === 'buildProgress'/u);
  assert.match(source, /updateBuildProgress/u);
});

test('bounds long Control Center content and responds to narrow widths', async () => {
  const source = await readFile(controlCenterPath, 'utf8');

  assert.match(source, /@media \(max-width: 760px\)/u);
  assert.match(source, /\.settings-row \{ grid-template-columns: 1fr; \}/u);
  assert.match(source, /\.card \{ min-width: 0/u);
  assert.match(source, /dd \{ min-width: 0; overflow-wrap: anywhere/u);
  assert.match(source, /button \{ max-width: 100%/u);
});

test('retains focus treatment and forced-color borders', async () => {
  const source = await readFile(controlCenterPath, 'utf8');

  assert.match(source, /button:focus-visible \{ outline: 2px solid var\(--vscode-focusBorder\)/u);
  assert.match(source, /@media \(forced-colors: active\)/u);
});

test('subscribes to build progress changes and forwards cancel message', async () => {
  const source = await readFile(controlCenterPath, 'utf8');

  assert.match(source, /onDidChangeBuildProgress/u);
  assert.match(source, /postBuildProgress/u);
  assert.match(source, /cancelBuild/u);
  assert.match(source, /cancelActiveBuild/u);
});
