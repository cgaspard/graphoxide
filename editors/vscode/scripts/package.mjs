import { chmod, copyFile, mkdir, rm } from 'node:fs/promises';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const extensionRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const repositoryRoot = path.resolve(extensionRoot, '..', '..');
const executableName = process.platform === 'win32' ? 'graphoxide.exe' : 'graphoxide';
const source = path.join(repositoryRoot, 'target', 'release', executableName);
const binDirectory = path.join(extensionRoot, 'bin');
const destination = path.join(binDirectory, executableName);
const target = platformTarget();

try {
  run('cargo', ['build', '--release', '--locked', '--bin', 'graphoxide'], repositoryRoot);
  await mkdir(binDirectory, { recursive: true });
  await copyFile(source, destination);
  if (process.platform !== 'win32') await chmod(destination, 0o755);
  run(process.platform === 'win32' ? 'npx.cmd' : 'npx', ['vsce', 'package', '--no-dependencies', '--target', target], extensionRoot);
} finally {
  await rm(binDirectory, { recursive: true, force: true });
}

function run(command, args, cwd) {
  const result = spawnSync(command, args, { cwd, stdio: 'inherit', shell: false });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(`${command} exited with code ${result.status}`);
}

function platformTarget() {
  const architecture = process.arch === 'arm64' ? 'arm64' : process.arch === 'x64' ? 'x64' : undefined;
  if (!architecture) throw new Error(`Unsupported extension architecture: ${process.arch}`);
  if (process.platform === 'darwin') return `darwin-${architecture}`;
  if (process.platform === 'linux') return `linux-${architecture}`;
  if (process.platform === 'win32') return `win32-${architecture}`;
  throw new Error(`Unsupported extension platform: ${process.platform}`);
}
