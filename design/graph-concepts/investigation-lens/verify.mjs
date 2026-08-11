import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const directory = dirname(fileURLToPath(import.meta.url));
const [html, css, javascript, readme] = await Promise.all([
  readFile(join(directory, 'index.html'), 'utf8'),
  readFile(join(directory, 'styles.css'), 'utf8'),
  readFile(join(directory, 'app.js'), 'utf8'),
  readFile(join(directory, 'README.md'), 'utf8'),
]);

assert.match(html, /\.\.\/shared\/fixture\.js/, 'prototype must use the shared fixture');
assert.doesNotMatch(javascript, /rawNodes|const\s+edges\s*=\s*\[/, 'prototype must not copy fixture topology');
assert.match(html, /id="loadingState"/, 'loading preview is required');
assert.match(html, /id="emptyState"/, 'empty preview is required');
assert.match(html, /value="dense"/, 'dense preview is required');
assert.match(html, /aria-live=/, 'live announcements are required');
assert.match(html, /Skip to dependency flow/, 'skip navigation is required');
assert.match(css, /prefers-reduced-motion:\s*reduce/, 'reduced-motion support is required');
assert.match(css, /forced-colors:\s*active/, 'forced-colors support is required');
assert.match(css, /relation-reads/, 'non-color relation encodings are required');
assert.match(javascript, /ArrowLeft/, 'flow keyboard navigation is required');
assert.match(javascript, /findDirectedPath/, 'path tracing is required');
assert.match(javascript, /riskFor/, 'change-risk cues are required');
assert.match(readme, /No build step/, 'standalone run contract must be documented');

console.log('Investigation Lens static contract verified.');
