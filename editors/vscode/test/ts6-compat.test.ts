import assert from 'node:assert/strict';
import { readFile, access } from 'node:fs/promises';
import path from 'node:path';
import test from 'node:test';

const distRoot = path.resolve(process.cwd(), 'dist');
const extensionEntry = path.join(distRoot, 'src', 'extension.js');
const cliModule = path.join(distRoot, 'src', 'cli.js');
const buildProgressModule = path.join(distRoot, 'src', 'build-progress.js');
const webviewBundle = path.join(distRoot, 'webview', 'graph-visualizer.js');

/**
 * TypeScript 6 migration compatibility tests.
 *
 * Verify that the emitted JavaScript remains a valid CommonJS module
 * compatible with the VS Code Extension Host (which uses `require`).
 * These tests run after `npm run compile` and assert on the emitted artifacts.
 */

test('extension entry is CommonJS with an exports assignment', async () => {
  const source = await readFile(extensionEntry, 'utf8');
  // Must use require() for imports (CJS), not import statements (ESM).
  assert.match(source, /require\(/u, 'extension.js must use require() for imports');
  assert.doesNotMatch(source, /^\s*import\s+.*from\s+/mu, 'extension.js must not contain ESM import statements');
  // Must export via exports.X or module.exports (CJS), not export (ESM).
  assert.match(source, /\bexports\.\w+\s*=|module\.exports/u, 'extension.js must use exports.X or module.exports');
  assert.doesNotMatch(source, /^\s*export\s+/mu, 'extension.js must not contain ESM export statements');
});

test('cli module emits CommonJS require() calls for node: builtins', async () => {
  const source = await readFile(cliModule, 'utf8');
  assert.match(source, /require\(["']node:child_process["']\)/u, 'cli.js must require node:child_process');
  // cli.js uses vscode.EventEmitter (not node:events directly), so just verify
  // it requires the vscode module.
  assert.match(source, /require\(["']vscode["']\)/u, 'cli.js must require vscode');
});

test('build-progress module emits CommonJS require() calls for node: builtins', async () => {
  const source = await readFile(buildProgressModule, 'utf8');
  assert.match(source, /require\(["']node:crypto["']\)/u, 'build-progress.js must require node:crypto');
  assert.match(source, /require\(["']node:string_decoder["']\)/u, 'build-progress.js must require node:string_decoder');
});

test('no emitted module contains ESM syntax', async () => {
  const modules = [extensionEntry, cliModule, buildProgressModule];
  for (const file of modules) {
    const source = await readFile(file, 'utf8');
    assert.doesNotMatch(
      source,
      /^\s*import\s*\{[^}]*\}\s+from\s*["']/mu,
      `${path.basename(file)} must not contain named ESM imports`,
    );
    assert.doesNotMatch(
      source,
      /^\s*export\s+(default|const|let|var|function|class)\b/mu,
      `${path.basename(file)} must not contain ESM export declarations`,
    );
  }
});

test('webview bundle is a self-contained script (no module system)', async () => {
  const source = await readFile(webviewBundle, 'utf8');
  // The webview runs in a browser context: no require, no import, no module.exports.
  assert.doesNotMatch(source, /require\(/u, 'webview bundle must not use require()');
  assert.doesNotMatch(source, /^\s*import\s+/mu, 'webview bundle must not use ESM imports');
  assert.doesNotMatch(source, /module\.exports/u, 'webview bundle must not use module.exports');
  assert.doesNotMatch(source, /exports\./u, 'webview bundle must not use exports.X');
});

test('tsconfig.json uses node16 module resolution (TS6 compatible)', async () => {
  const configPath = path.resolve(process.cwd(), 'tsconfig.json');
  const config = JSON.parse(await readFile(configPath, 'utf8'));
  assert.equal(config.compilerOptions.module, 'node16');
  assert.equal(config.compilerOptions.moduleResolution, 'node16');
  assert.deepEqual(config.compilerOptions.types, ['node']);
});

test('tsconfig.webview.json uses esnext module (TS6 compatible, no outFile)', async () => {
  const configPath = path.resolve(process.cwd(), 'tsconfig.webview.json');
  const config = JSON.parse(await readFile(configPath, 'utf8'));
  assert.equal(config.compilerOptions.module, 'esnext');
  assert.equal(config.compilerOptions.outFile, undefined, 'outFile is deprecated in TS6');
  assert.equal(config.compilerOptions.outDir, 'dist/webview');
  assert.equal(config.compilerOptions.rootDir, 'webview');
});

test('package.json main entry points to emitted CJS file', async () => {
  const pkgPath = path.resolve(process.cwd(), 'package.json');
  const pkg = JSON.parse(await readFile(pkgPath, 'utf8'));
  assert.equal(pkg.main, './dist/src/extension.js');
  // Verify the file actually exists.
  await access(extensionEntry);
});

test('emitted JS target is ES2022 (no downleveling needed)', async () => {
  // Spot-check: the emitted code should use modern features (class fields, optional chaining)
  // rather than downleveled helpers, confirming target=ES2022 is respected.
  const source = await readFile(cliModule, 'utf8');
  // ES2022: class static blocks or optional chaining should appear untranspiled.
  assert.match(source, /\?\.|\?\?/u, 'cli.js should contain optional chaining/nullish coalescing (ES2020+)');
});
