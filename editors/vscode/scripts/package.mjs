import { chmod, copyFile, mkdir, readFile, rm, stat, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { artifactPaths, stageAgentArtifacts } from '../../../scripts/agent-artifacts.mjs';

const extensionRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const repositoryRoot = path.resolve(extensionRoot, '..', '..');
const options = parseOptions(process.argv.slice(2));
const target = options.target ?? hostTarget();
const executableName = target.startsWith('win32-') ? 'graphoxide.exe' : 'graphoxide';
const source = options.binary
  ? path.resolve(process.cwd(), options.binary)
  : path.join(repositoryRoot, 'target', 'release', process.platform === 'win32' ? 'graphoxide.exe' : 'graphoxide');
const binDirectory = path.join(extensionRoot, 'bin');
const destination = path.join(binDirectory, executableName);
const thirdPartySource = path.join(repositoryRoot, 'THIRD_PARTY_LICENSES.html');
const thirdPartyDestination = path.join(extensionRoot, 'THIRD_PARTY_LICENSES.html');
const agentAssetsDestination = path.join(extensionRoot, 'agent-assets');
const vsceCli = path.join(extensionRoot, 'node_modules', '@vscode', 'vsce', 'vsce');
const packageJson = JSON.parse(await readFile(path.join(extensionRoot, 'package.json'), 'utf8'));
let stagedThirdPartyLicenses = false;
let stagedAgentAssets = false;

try {
  if (!options.binary) {
    if (target !== hostTarget()) {
      throw new Error(`--target ${target} requires an explicit --binary built for that target`);
    }
    run('cargo', ['build', '--release', '--locked', '--bin', 'graphoxide'], repositoryRoot);
  }
  const sourceStat = await stat(source);
  if (!sourceStat.isFile() || sourceStat.size < 1_000_000) {
    throw new Error(`Graphoxide binary is missing or unexpectedly small: ${source} (${sourceStat.size} bytes)`);
  }
  await mkdir(binDirectory, { recursive: true });
  await copyFile(source, destination);
  if (!target.startsWith('win32-')) await chmod(destination, 0o755);
  await writeFile(
    path.join(binDirectory, 'graphoxide.version'),
    `${packageJson.version}\n${target}\n${executableName}\n`,
  );
  try {
    await stat(thirdPartyDestination);
  } catch (error) {
    if (error?.code !== 'ENOENT') throw error;
    await copyFile(thirdPartySource, thirdPartyDestination);
    stagedThirdPartyLicenses = true;
  }
  await stageAgentArtifacts(agentAssetsDestination);
  stagedAgentAssets = true;

  const output = options.out
    ? path.resolve(process.cwd(), options.out)
    : path.join(extensionRoot, `${packageJson.name}-${target}-${packageJson.version}.vsix`);
  await mkdir(path.dirname(output), { recursive: true });
  const vsceArgs = ['package', '--no-dependencies', '--target', target];
  if (options.preRelease) vsceArgs.push('--pre-release');
  vsceArgs.push('--out', output);
  run(process.execPath, [vsceCli, ...vsceArgs], extensionRoot);
  await verifyVsix(output, executableName);
} finally {
  await rm(binDirectory, { recursive: true, force: true });
  if (stagedAgentAssets) await rm(agentAssetsDestination, { recursive: true, force: true });
  if (stagedThirdPartyLicenses) await rm(thirdPartyDestination, { force: true });
}

function run(command, args, cwd) {
  const result = spawnSync(command, args, { cwd, stdio: 'inherit', shell: false });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(`${command} exited with code ${result.status}`);
}

function hostTarget() {
  const architecture = process.arch === 'arm64' ? 'arm64' : process.arch === 'x64' ? 'x64' : undefined;
  if (!architecture) throw new Error(`Unsupported extension architecture: ${process.arch}`);
  if (process.platform === 'darwin') return `darwin-${architecture}`;
  if (process.platform === 'linux') return `linux-${architecture}`;
  if (process.platform === 'win32') return `win32-${architecture}`;
  throw new Error(`Unsupported extension platform: ${process.platform}`);
}

function parseOptions(args) {
  const parsed = { preRelease: false };
  for (let index = 0; index < args.length; index += 1) {
    const argument = args[index];
    if (argument === '--pre-release') {
      parsed.preRelease = true;
      continue;
    }
    if (argument === '--target' || argument === '--binary' || argument === '--out') {
      const value = args[index + 1];
      if (!value) throw new Error(`${argument} requires a value`);
      parsed[argument.slice(2)] = value;
      index += 1;
      continue;
    }
    throw new Error(`Unknown package option: ${argument}`);
  }
  return parsed;
}

async function verifyVsix(vsixPath, expectedExecutable) {
  const bytes = await readFile(vsixPath);
  const endSignature = 0x06054b50;
  let end = -1;
  for (let offset = bytes.length - 22; offset >= Math.max(0, bytes.length - 65_557); offset -= 1) {
    if (bytes.readUInt32LE(offset) === endSignature) {
      end = offset;
      break;
    }
  }
  if (end < 0) throw new Error(`${vsixPath} is not a readable ZIP archive`);

  const totalEntries = bytes.readUInt16LE(end + 10);
  let offset = bytes.readUInt32LE(end + 16);
  const entries = new Map();
  for (let index = 0; index < totalEntries; index += 1) {
    if (bytes.readUInt32LE(offset) !== 0x02014b50) {
      throw new Error(`${vsixPath} has an invalid central directory`);
    }
    const size = bytes.readUInt32LE(offset + 24);
    const nameLength = bytes.readUInt16LE(offset + 28);
    const extraLength = bytes.readUInt16LE(offset + 30);
    const commentLength = bytes.readUInt16LE(offset + 32);
    const name = bytes.subarray(offset + 46, offset + 46 + nameLength).toString('utf8');
    entries.set(name, size);
    offset += 46 + nameLength + extraLength + commentLength;
  }

  const executable = `extension/bin/${expectedExecutable}`;
  const executableSize = entries.get(executable);
  if (!executableSize || executableSize < 1_000_000) {
    throw new Error(`${vsixPath} is missing ${executable} or it is unexpectedly small (${executableSize ?? 0} bytes)`);
  }
  for (const required of [
    'extension/bin/graphoxide.version',
    'extension/THIRD_PARTY_LICENSES.html',
    'extension/dist/webview/graph-visualizer.js',
    'extension/media/graph-visualizer.css',
  ]) {
    const requiredSize = entries.get(required);
    if (!requiredSize) throw new Error(`${vsixPath} is missing ${required} or it is empty`);
  }
  for (const artifact of artifactPaths) {
    const required = `extension/agent-assets/${artifact}`;
    if (!entries.has(required)) throw new Error(`${vsixPath} is missing ${required}`);
  }
  console.log(`[package] verified ${executable} in ${vsixPath} (${executableSize} bytes)`);
}
