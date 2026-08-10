import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import vm from 'node:vm';

const root = new URL('./', import.meta.url);
const [html, css, app] = await Promise.all([
  readFile(new URL('index.html', root), 'utf8'),
  readFile(new URL('styles.css', root), 'utf8'),
  readFile(new URL('app.js', root), 'utf8'),
]);

new vm.Script(app, { filename: 'semantic-atlas/app.js' });

const ids = [...html.matchAll(/\bid="([^"]+)"/gu)].map((match) => match[1]);
assert.equal(ids.length, new Set(ids).size, 'HTML IDs must be unique');

const referencedIds = [...app.matchAll(/getElementById\('([^']+)'\)/gu)].map((match) => match[1]);
for (const id of referencedIds) {
  assert.ok(ids.includes(id), `app.js references missing element #${id}`);
}

const fixtureIndex = html.indexOf('<script src="../shared/fixture.js"></script>');
const appIndex = html.indexOf('<script src="app.js"></script>');
assert.ok(fixtureIndex >= 0, 'shared fixture script must be loaded');
assert.ok(appIndex > fixtureIndex, 'app.js must load after the shared fixture');
assert.match(app, /contractVersion !== 1/u, 'app must guard the fixture contract version');
assert.doesNotMatch(app, /\b(?:fetch|XMLHttpRequest|WebSocket)\s*\(/u, 'prototype must remain network-independent');
assert.doesNotMatch(html + css, /https?:\/\//u, 'prototype assets must remain local');
assert.match(html, /id="graph"[^>]+role="tree"/u, 'graph must expose its interactive tree role');
assert.match(css, /prefers-reduced-motion:\s*reduce/u, 'reduced-motion treatment must be present');
assert.match(css, /prefers-contrast:\s*more/u, 'high-contrast treatment must be present');

console.log(`Semantic Atlas OK: ${ids.length} unique controls/regions, local fixture contract v1`);
