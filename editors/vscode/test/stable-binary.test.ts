import assert from 'node:assert/strict';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import test from 'node:test';
import {
  bundledBinary,
  ensureStableBinary,
  executableName,
  isAbandonedExtensionBinary,
  isExtensionScopedBinary,
} from '../src/mcp/stable-binary';

/** A throwaway extension directory shipping a binary and its version sidecar. */
function extensionFixture(version: string): { root: string; extensionPath: string; stableDir: string } {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'graphoxide-stable-'));
  const extensionPath = path.join(root, `cgaspard.graphoxide-vscode-${version}-darwin-arm64`);
  fs.mkdirSync(path.join(extensionPath, 'bin'), { recursive: true });
  fs.writeFileSync(bundledBinary(extensionPath, 'darwin'), `#!/bin/sh\necho ${version}\n`, { mode: 0o755 });
  fs.writeFileSync(path.join(extensionPath, 'bin', 'graphoxide.version'), `${version}\ndarwin-arm64\ngraphoxide\n`);
  return { root, extensionPath, stableDir: path.join(root, 'globalStorage', 'bin') };
}

test('links the bundled binary to a path with no version in it', () => {
  const { extensionPath, stableDir } = extensionFixture('0.4.3');
  const result = ensureStableBinary(extensionPath, stableDir, 'darwin');
  assert.ok(result);
  assert.equal(result.linked, true);
  assert.equal(result.path, path.join(stableDir, 'graphoxide'));
  assert.doesNotMatch(result.path, /0\.4\.3/u);
  assert.equal(fs.readFileSync(result.path, 'utf8').trim(), '#!/bin/sh\necho 0.4.3'.trim());
});

test('reuses an existing link instead of relinking on every call', () => {
  const { extensionPath, stableDir } = extensionFixture('0.4.3');
  assert.equal(ensureStableBinary(extensionPath, stableDir, 'darwin')?.linked, true);
  assert.equal(ensureStableBinary(extensionPath, stableDir, 'darwin')?.linked, false);
});

test('relinks when the version sidecar reports a different build', () => {
  const { extensionPath, stableDir } = extensionFixture('0.4.3');
  const first = ensureStableBinary(extensionPath, stableDir, 'darwin');
  assert.equal(first?.linked, true);

  // Model an upgrade: a new extension directory, same stable destination.
  const upgraded = extensionFixture('0.5.0');
  const second = ensureStableBinary(upgraded.extensionPath, stableDir, 'darwin');
  assert.equal(second?.linked, true);
  assert.equal(second.path, first?.path);
  assert.match(fs.readFileSync(second.path, 'utf8'), /0\.5\.0/u);
});

test('the linked binary survives deletion of the extension directory it came from', () => {
  const { extensionPath, stableDir } = extensionFixture('0.4.3');
  const result = ensureStableBinary(extensionPath, stableDir, 'darwin');
  assert.ok(result);
  assert.equal(result.copied, undefined, 'expected a hard link within one temp volume');

  fs.rmSync(extensionPath, { recursive: true, force: true });

  // This is the whole point: an external MCP client holding this path keeps
  // working after VS Code removes the versioned directory on upgrade.
  assert.equal(fs.existsSync(result.path), true);
  assert.match(fs.readFileSync(result.path, 'utf8'), /0\.4\.3/u);
});

test('reports nothing to link when the extension ships no binary', () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'graphoxide-empty-'));
  assert.equal(ensureStableBinary(root, path.join(root, 'bin'), 'darwin'), undefined);
});

test('falls back to size and mtime when no version sidecar is shipped', () => {
  const { extensionPath, stableDir } = extensionFixture('0.4.3');
  fs.rmSync(path.join(extensionPath, 'bin', 'graphoxide.version'));
  assert.equal(ensureStableBinary(extensionPath, stableDir, 'darwin')?.linked, true);
  assert.equal(ensureStableBinary(extensionPath, stableDir, 'darwin')?.linked, false);
});

test('names the executable per platform', () => {
  assert.equal(executableName('win32'), 'graphoxide.exe');
  assert.equal(executableName('darwin'), 'graphoxide');
  assert.equal(executableName('linux'), 'graphoxide');
});

test('recognises binaries living inside an extension directory', () => {
  const cases = [
    '/Users/x/.vscode/extensions/cgaspard.graphoxide-vscode-0.1.0-darwin-arm64/bin/graphoxide',
    '/Users/x/.vscode-insiders/extensions/cgaspard.graphoxide-vscode-0.4.3-darwin-arm64/bin/graphoxide',
    '/Users/x/.cursor/extensions/cgaspard.graphoxide-vscode-0.2.0-darwin-arm64/bin/graphoxide',
    'C:\\Users\\x\\.vscode\\extensions\\cgaspard.graphoxide-vscode-0.1.0-win32-x64\\bin\\graphoxide.exe',
  ];
  for (const command of cases) assert.equal(isExtensionScopedBinary(command), true, command);
});

test('leaves paths the user chose deliberately alone', () => {
  const cases = [
    '/opt/homebrew/bin/graphoxide',
    '/Users/x/Library/Application Support/Code/User/globalStorage/cgaspard.graphoxide-vscode/bin/graphoxide',
    '/Users/x/Projects/graphoxide/target/release/graphoxide',
    '/Users/x/.vscode/extensions/some.other-extension-1.0.0/bin/graphoxide',
  ];
  for (const command of cases) assert.equal(isExtensionScopedBinary(command), false, command);
});

test('treats an extension path as abandoned only once it stops existing', () => {
  const present = '/Users/x/.vscode/extensions/cgaspard.graphoxide-vscode-0.4.3-darwin-arm64/bin/graphoxide';
  const missing = '/Users/x/.vscode/extensions/cgaspard.graphoxide-vscode-0.1.0-darwin-arm64/bin/graphoxide';
  const exists = (file: string): boolean => file === present;
  assert.equal(isAbandonedExtensionBinary(present, exists), false);
  assert.equal(isAbandonedExtensionBinary(missing, exists), true);
  // A missing path outside any extension directory is the user's to manage.
  assert.equal(isAbandonedExtensionBinary('/opt/homebrew/bin/graphoxide', exists), false);
});
