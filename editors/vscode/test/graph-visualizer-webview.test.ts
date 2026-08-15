import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import path from 'node:path';
import test from 'node:test';

const visualizerPath = path.join(process.cwd(), 'webview', 'graph-visualizer.ts');
const hostPath = path.join(process.cwd(), 'src', 'visualizer.ts');
const stylesheetPath = path.join(process.cwd(), 'media', 'graph-visualizer.css');
const tsconfigPath = path.join(process.cwd(), 'tsconfig.webview.json');

test('uses the fixed purple-led palette with accessible solid-action text', async () => {
  const stylesheet = await readFile(stylesheetPath, 'utf8');
  assert.match(stylesheet, /--gx-purple:\s*#8B5CF6;/u);
  assert.match(stylesheet, /--gx-lavender:\s*#C9B8FF;/u);
  assert.match(stylesheet, /--gx-cyan:\s*#7BC3E8;/u);
  assert.match(stylesheet, /--gx-surface:\s*#1B1F2B;/u);
  assert.ok(contrast('#8B5CF6', '#090B12') >= 4.5);
  assert.match(stylesheet, /\.gx-primary-button,[\s\S]*?color:\s*var\(--gx-canvas\);[\s\S]*?background:\s*var\(--gx-purple\);/u);
  assert.match(stylesheet, /\.gx-mode-switch button\[aria-pressed="true"\]:hover:not\(:disabled\),[\s\S]*?\.gx-primary-button:hover:not\(:disabled\),[\s\S]*?color:\s*var\(--gx-canvas\);[\s\S]*?background:\s*var\(--gx-purple\);/u);
  assert.match(stylesheet, /@media \(forced-colors: active\)/u);
  assert.match(stylesheet, /@media \(prefers-reduced-motion: reduce\)/u);
});

test('keeps the browser client bounded and treats snapshot text as inert', async () => {
  const source = await readFile(visualizerPath, 'utf8');
  assert.doesNotMatch(source, /\.innerHTML\b/u);
  assert.doesNotMatch(source, /incoming impact|incoming callers/iu);
  assert.match(source, /const MAX_NODES = 5_000;/u);
  assert.match(source, /const MAX_EDGES = 12_000;/u);
  assert.match(source, /const MAX_STRING_CODE_UNITS = 16_384;/u);
  assert.match(source, /const MAX_SNAPSHOT_STRING_CODE_UNITS = 8_000_000;/u);
  assert.match(source, /const MAX_SEARCH_RESULTS = 20;/u);
  assert.match(source, /const MAX_FILTER_OPTIONS = 500;/u);
  assert.match(source, /const MAX_HISTORY = 20;/u);
  assert.match(source, /const LENS_INITIAL_PER_SIDE = 6;/u);
  assert.match(source, /const LENS_MAX_PER_SIDE = 72;/u);
  assert.match(source, /Math\.min\(2, Math\.max\(1, globalThis\.devicePixelRatio/u);
  assert.match(source, /animatedParticles < 96/u);
  assert.match(source, /\.slice\(0, 64\)/u);
  assert.match(source, /const labelBudget = Math\.min\(MAX_LABELS,[\s\S]*?if \(drawn >= labelBudget\) break;/u);
  assert.match(source, /const priority = node\.id === state\.selectedId \|\| node\.id === state\.keyboardId;[\s\S]*?occupied\.some\(\(entry\) => rectanglesOverlap\(entry, padded\)\)/u);
  assert.match(source, /No relationship source location recorded\./u);
  assert.match(source, /search\.setAttribute\('role', 'combobox'\);/u);
  assert.match(source, /canvas\.setAttribute\('aria-owns', 'gx-canvas-active'\);/u);
  assert.match(source, /const activeNode = state\.keyboardId \? nodeById\.get\(state\.keyboardId\) : undefined;[\s\S]*?ui\.canvasProxy\.textContent = activeNode/u);
  assert.match(source, /function snapshotStringsWithinBudget\([\s\S]*?add\(snapshot\.builtAtCommit\);[\s\S]*?for \(const community of snapshot\.communities\)[\s\S]*?for \(const confidence of snapshot\.confidences\) add\(confidence\.value\);/u);
  assert.match(source, /\|\| !snapshotStringsWithinBudget\(snapshot\)\) return null;/u);
  assert.match(source, /const selfLoop = edge\.source === edge\.target;[\s\S]*?const selfLoopNodeRadius = selfLoop \? nodeScreenRadius\(requiredNode\(edge\.source\)\) : 0;[\s\S]*?if \(selfLoop && selfLoopNodeRadius < 3\.5 && !selected && !traced\) continue;[\s\S]*?if \(selfLoop\) drawSelfLoop\(source, selfLoopNodeRadius\);/u);
  assert.match(source, /for \(const edge of renderCache\.edgeEntries\) if \(edge\.target === state\.selectedId\) tracedSources\.add\(edge\.source\);/u);
  assert.doesNotMatch(source, /for \(const edge of renderCache\.edges\) if \(edge\.target === state\.selectedId\) tracedSources/u);
  assert.match(source, /function drawSelfLoop\(point: Point, nodeRadius: number\): void \{[\s\S]*?context\.arc\([\s\S]*?context\.fill\(\);/u);
  assert.match(source, /document\.createTextNode\('Recorded source → target'\)/u);
  assert.match(source, /const searchWrap = make\('div', 'gx-search-wrap'\);/u);
  assert.match(source, /relation \|\| 'Unspecified relationship'/u);
  assert.match(source, /relationOptions\.set\(token, \{ all: false, relation \}\);/u);
  assert.match(source, /function boundedFilterOptions<T>[\s\S]*?entries\.slice\(0, MAX_FILTER_OPTIONS\)[\s\S]*?retained\[MAX_FILTER_OPTIONS - 1\] = active;/u);
  assert.match(source, /if \(state\.communityFilterUnassigned\) return node\.community === null;/u);
  assert.match(source, /function domainDisplayName\([\s\S]*?return value === '' \? 'Unnamed domain' : value;/u);
  assert.match(source, /traceActive: current\.traceActive && selectedId !== null && mode === 'global',/u);
  assert.match(source, /relationFilter: nullableBoundedString\(value\.relationFilter, MAX_STRING_CODE_UNITS\),/u);
  assert.match(source, /communityFilter: nullableBoundedString\(value\.communityFilter, MAX_STRING_CODE_UNITS\),/u);
  assert.match(source, /function relationshipCount\(entries: readonly LensEntry\[\]\): number \{\s*return entries\.reduce\(\(count, entry\) => count \+ entry\.edges\.length, 0\);/u);
  assert.match(source, /additional related symbols are outside the 72-card Lens limit\./u);
  assert.match(source, /searchIndex = searchIndex < 0\s*\? \(direction > 0 \? 0 : matches\.length - 1\)/u);
  assert.match(source, /communityColors = new Map\(layouts\.map\(\(layout\) => \[layout\.id, layout\.color\]\)\);/u);
  assert.match(source, /return communityColors\.get\(id\) \?\? '#8B5CF6';/u);
  assert.doesNotMatch(source, /communityLayouts\.find\(/u);
  assert.match(source, /function sanitizeViewIdsForFilters\(\): boolean \{[\s\S]*?selectedId[\s\S]*?focusId[\s\S]*?keyboardId[\s\S]*?mode: state\.mode === 'focus' && focusId === null \? 'global' : state\.mode/u);
  assert.match(source, /if \(!nodePassesActiveFilters\(node\.id\)\) \{[\s\S]*?communityFilter: null,[\s\S]*?relationFilter: null,/u);
  assert.match(source, /let searchOpen = false;/u);
  assert.match(source, /if \(!searchOpen\) \{[\s\S]*?aria-expanded', 'false'/u);
  assert.match(source, /addEventListener\('focusout',[\s\S]*?closeSearchResults\(\)/u);
  assert.match(source, /const cellRadius = Math\.max\(1, Math\.min\(8, Math\.ceil\(25 \/ state\.scale \/ POSITION_CELL\)\)\);/u);
  assert.match(source, /function packCommunities\([\s\S]*?const goldenAngle = Math\.PI \* \(3 - Math\.sqrt\(5\)\);/u);
  assert.match(source, /return right\[1\]\.length - left\[1\]\.length[\s\S]*?const centers = packCommunities\(drafts\);/u);
  assert.match(source, /function localNodePosition\([\s\S]*?ring \* 6[\s\S]*?NODE_SPACING_WORLD/u);
  assert.match(source, /function nodeScreenRadius\([\s\S]*?const collisionSafe[\s\S]*?return Math\.min\(projected, collisionSafe\);/u);
  assert.match(source, /const hitRadius = Math\.max\(12, nodeShapeExtent\(nodeScreenRadius\(node\)\) \+ 8\);/u);
  assert.match(source, /function drawLabels\([\s\S]*?const glyphs:[\s\S]*?drawCommunityLabels\(glyphs, occupied, viewport, zoomRatio\)/u);
  assert.match(source, /version: 2,[\s\S]*?layoutFingerprint: null,/u);
  assert.match(source, /const retainCamera = value\.version === 2;[\s\S]*?cameraInitialized: retainCamera && value\.cameraInitialized === true/u);
  assert.match(source, /complete: button\('gx-density-complete', 'Dense'\)/u);
  assert.match(source, /of \$\{filtered\.toLocaleString\(\)\} relationships drawn/u);
  assert.match(source, /symbols omitted by bounded snapshot/u);
  assert.match(source, /relationships omitted by bounded snapshot/u);
  assert.match(source, /canonicalCommunityNames = new Map\(snapshot\.communities\.map/u);
  assert.match(source, /return canonicalCommunityNames\.get\(id\) \?\? domainDisplayName\(id, fallbackName\);/u);
  assert.match(source, /ui\.stageTitle\.textContent = activeDomainTitle\(\);/u);
  assert.doesNotMatch(source, /incoming paths|incoming path/iu);
  assert.match(source, /stage\.tabIndex = -1;/u);
  assert.match(source, /search\.setAttribute\('aria-autocomplete', 'list'\);/u);
  assert.equal(source.match(/setAttribute\('role', 'group'\);/gu)?.length, 2);
  assert.match(source, /state\.mode === 'focus'\s*\? `\$\{focusRelationships\.toLocaleString\(\)\} focus relationships`/u);
  assert.match(source, /Choose a relationship card to move the Lens/u);
  assert.match(source, /more visible relationships\. Open the Lens to explore the neighborhood\./u);
  assert.match(source, /state\.mode === 'global' && !state\.cameraInitialized[\s\S]*?bounds\.width > 0 && bounds\.height > 0/u);
  assert.match(source, /if \(bounds\.width <= 0 \|\| bounds\.height <= 0\) return;/u);
  assert.match(source, /\.map\(\(id, originalIndex\) => \(\{ id, originalIndex \}\)\)[\s\S]*?retainedHistory\.findIndex\(\(entry\) => entry\.originalIndex === current\.historyIndex\)/u);
  assert.match(source, /retainedHistoryIndex >= 0[\s\S]*?focusedHistoryIndex >= 0 \? focusedHistoryIndex : history\.length - 1/u);
  assert.match(source, /const visible = state\.history\.slice\(start, start \+ 6\);/u);
  assert.match(source, /ui\.inspector\.classList\.add\('is-empty'\);[\s\S]*?ui\.inspector\.classList\.remove\('is-empty'\);/u);
  const confidenceSection = source.slice(
    source.indexOf('function confidencePresentation'),
    source.indexOf('function kindGlyph'),
  );
  assert.doesNotMatch(confidenceSection, /toLocaleUpperCase/u);
  const stylesheet = await readFile(stylesheetPath, 'utf8');
  assert.match(stylesheet, /#gx-graph-canvas:focus-visible \{\s*box-shadow:\s*inset[^;}]+var\(--gx-lavender\);/u);
  assert.match(stylesheet, /\.gx-inspector:empty,\s*\.gx-inspector\.is-empty \{\s*display: none;/u);
});

test('gates the bounded Extension Host bridge and clears stale graphs on status changes', async () => {
  const source = await readFile(visualizerPath, 'utf8');
  const host = await readFile(hostPath, 'utf8');
  assert.match(source, /const testMode = root\.dataset\.testMode === 'true';/u);
  assert.match(source, /addEventListener\('message', handleHostMessageEvent\);/u);
  assert.match(source, /function handleHostMessageEvent\(event: MessageEvent<unknown>\): void \{\s*[\s\S]*?if \(event\.origin !== globalThis\.origin\) return;\s*handleHostMessage\(event\.data\);\s*\}/u);
  assert.match(source, /value\.type === 'testAction' && testMode && isTestAction\(value\.action\)/u);
  for (const action of ['select-first', 'enter-focus', 'toggle-trace', 'return-global', 'set-query', 'reveal-selected']) {
    assert.match(source, new RegExp(`'${action}'`, 'u'));
  }
  assert.doesNotMatch(source, /eval\s*\(|new Function\s*\(/u);
  assert.equal(source.match(/clearGraphForStatus\(\);/gu)?.length, 2);
  assert.match(source, /function clearGraphForStatus\(\): void \{[\s\S]*?graph = null;[\s\S]*?renderCache = \{ nodes: \[\], edges: \[\], edgeEntries: \[\] \};[\s\S]*?selectedId: null,[\s\S]*?focusId: null,[\s\S]*?traceActive: false,[\s\S]*?scale: 1,[\s\S]*?offsetX: 0,[\s\S]*?offsetY: 0,[\s\S]*?cameraInitialized: false,/u);
  assert.match(source, /function clearGraphForStatus\(\): void \{[\s\S]*?ui\.stageTitle\.textContent = 'All domains';[\s\S]*?ui\.footerHelp\.textContent = 'Scroll to zoom · drag to pan · Enter to inspect';/u);
  assert.match(source, /function installGraph\(snapshot: VisualizerSnapshotV1\): void \{\s*graph = snapshot;/u);
  const installSection = source.slice(source.indexOf('function installGraph'), source.indexOf('function clearGraphForStatus'));
  assert.doesNotMatch(installSection, /setHostStatus\('ready'/u);
  assert.doesNotMatch(source, /postMessage\(\{ type: 'ready' \}\);\s*emitRendererState\(\);/u);
  assert.match(host, /isNullableBoundedString\(state\.selectedId, MAX_VISUALIZER_STRING_CODE_UNITS\)/u);
  assert.match(host, /isNullableBoundedString\(state\.focusId, MAX_VISUALIZER_STRING_CODE_UNITS\)/u);
  assert.match(host, /isNullableBoundedString\(state\.communityFilter, MAX_VISUALIZER_STRING_CODE_UNITS\)/u);
  assert.match(host, /isNullableBoundedString\(state\.relationFilter, MAX_VISUALIZER_STRING_CODE_UNITS\)/u);
});

test('compiles as a dependency-free classic browser bundle', async () => {
  const config = JSON.parse(await readFile(tsconfigPath, 'utf8')) as {
    compilerOptions?: { module?: string; outDir?: string; rootDir?: string; types?: unknown[]; lib?: string[] };
    files?: string[];
  };
  assert.equal(config.compilerOptions?.module, 'esnext');
  assert.equal(config.compilerOptions?.outDir, 'dist/webview');
  assert.equal(config.compilerOptions?.rootDir, 'webview');
  assert.deepEqual(config.compilerOptions?.types, []);
  assert.ok(config.compilerOptions?.lib?.includes('DOM.Iterable'));
  assert.deepEqual(config.files, ['webview/graph-visualizer.ts']);
});

function contrast(left: string, right: string): number {
  const leftLuminance = luminance(left);
  const rightLuminance = luminance(right);
  return (Math.max(leftLuminance, rightLuminance) + 0.05)
    / (Math.min(leftLuminance, rightLuminance) + 0.05);
}

function luminance(color: string): number {
  const channels = [1, 3, 5].map((offset) => Number.parseInt(color.slice(offset, offset + 2), 16) / 255);
  const linear = channels.map((channel) => channel <= 0.04045
    ? channel / 12.92
    : ((channel + 0.055) / 1.055) ** 2.4);
  return linear[0]! * 0.2126 + linear[1]! * 0.7152 + linear[2]! * 0.0722;
}
