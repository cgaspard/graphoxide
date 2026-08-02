import { cp, mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { downloadAndUnzipVSCode, runTests } from '@vscode/test-electron';

const extensionRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const repositoryRoot = path.resolve(extensionRoot, '..', '..');
const temporaryRoot = await mkdtemp(path.join(tmpdir(), 'graphoxide-vscode-e2e-'));
const workspace = path.join(temporaryRoot, 'sample');
const electronEnvironment = new Map();

try {
  await cp(path.join(repositoryRoot, 'examples', 'vscode-sample'), workspace, { recursive: true });
  await rm(path.join(workspace, 'graphoxide-out'), { recursive: true, force: true });
  await mkdir(path.join(workspace, '.vscode'), { recursive: true });
  await writeFile(path.join(workspace, '.vscode', 'settings.json'), JSON.stringify({
    'graphoxide.promptOnFirstOpen': false,
    'graphoxide.revealOutput': 'never',
    'graphoxide.updateOnSaveDelay': 250,
  }, null, 2));
  const vscodeExecutablePath = await downloadAndUnzipVSCode(process.env.VSCODE_E2E_VERSION ?? 'stable');
  for (const key of ['ELECTRON_RUN_AS_NODE', 'VSCODE_CLI', 'VSCODE_ESM_ENTRYPOINT']) {
    electronEnvironment.set(key, process.env[key]);
    delete process.env[key];
  }
  await runTests({
    vscodeExecutablePath,
    extensionDevelopmentPath: extensionRoot,
    extensionTestsPath: path.join(extensionRoot, 'dist', 'test', 'e2e', 'index.js'),
    launchArgs: [workspace, '--disable-extensions', '--skip-welcome', '--skip-release-notes'],
  });
} finally {
  for (const [key, value] of electronEnvironment) {
    if (value === undefined) delete process.env[key];
    else process.env[key] = value;
  }
  const expectedPrefix = path.join(tmpdir(), 'graphoxide-vscode-e2e-');
  if (!temporaryRoot.startsWith(expectedPrefix)) throw new Error(`Refusing to remove unexpected E2E path: ${temporaryRoot}`);
  await rm(temporaryRoot, { recursive: true, force: true });
}
