import { execFile } from 'node:child_process';
import * as fs from 'node:fs';
import * as path from 'node:path';
import { promisify } from 'node:util';
import * as vscode from 'vscode';
import { trustedExecutableCandidates } from '../llm/config';
import { ServerInvocation } from './config';
import { bundledBinary, ensureStableBinary, executableName } from './stable-binary';

const execFileAsync = promisify(execFile);

/**
 * The subset of `vscode.ExtensionContext` needed to resolve a binary path that
 * outlives this extension release. Declared structurally so callers can pass the
 * context straight through.
 */
export interface ExtensionPaths {
  readonly extensionUri: vscode.Uri;
  readonly globalStorageUri: vscode.Uri;
}

export function configuredInvocation(folder?: vscode.WorkspaceFolder): ServerInvocation {
  const configuration = vscode.workspace.getConfiguration('graphoxide', folder?.uri);
  const command = configuration.get<string>('binaryPath', 'graphoxide');
  const args = [...configuration.get<string[]>('extraArguments', []), 'serve'];
  return { command, args, ...(folder ? { cwd: folder.uri.fsPath } : {}) };
}

export function extensionInvocation(extensionUri: vscode.Uri, folder?: vscode.WorkspaceFolder): ServerInvocation {
  const configured = configuredInvocation(folder);
  return { ...configured, command: resolveGraphoxideExecutable(extensionUri, configured.command, folder) };
}

/**
 * Resolve an executable that is controlled by this extension. This deliberately
 * excludes workspace settings, extra arguments, environment overrides, and PATH
 * because callers may pass API credentials to the child process.
 */
export function trustedExtensionInvocation(
  extensionUri: vscode.Uri,
  folder: vscode.WorkspaceFolder,
): ServerInvocation {
  const command = trustedExecutable(extensionUri);
  if (!command) {
    throw new Error(
      'AI community labeling requires the Graphoxide executable bundled with this extension or built in this repository. '
      + 'The configured binary, extra arguments, environment override, and PATH are not used for this command.',
    );
  }
  return { command, args: [], cwd: folder.uri.fsPath };
}

export async function resolvedInvocation(folder?: vscode.WorkspaceFolder, paths?: ExtensionPaths): Promise<ServerInvocation> {
  if (paths) return persistableInvocation(paths, folder);
  const configured = configuredInvocation(folder);
  const command = await resolveExecutable(configured.command) ?? configured.command;
  return { ...configured, command };
}

/**
 * Invocation safe to write into another tool's configuration file. Identical to
 * `extensionInvocation` except that the bundled binary is reported through a
 * version-independent link, because the external clients that read these files
 * cannot re-resolve the path after an extension upgrade moves it.
 */
export function persistableInvocation(paths: ExtensionPaths, folder?: vscode.WorkspaceFolder): ServerInvocation {
  const invocation = extensionInvocation(paths.extensionUri, folder);
  return { ...invocation, command: stableCommand(paths, invocation.command) };
}

/**
 * Redirect only the bundled binary. An explicit `binaryPath`, a PATH hit, a
 * `GRAPHOXIDE_BINARY` override, and a repository build all live outside the
 * versioned extension directory already, so they are left exactly as resolved.
 */
function stableCommand(paths: ExtensionPaths, command: string): string {
  const bundled = bundledBinary(paths.extensionUri.fsPath, process.platform);
  if (path.normalize(command) !== path.normalize(bundled)) return command;
  const stableDir = path.join(paths.globalStorageUri.fsPath, 'bin');
  return ensureStableBinary(paths.extensionUri.fsPath, stableDir, process.platform)?.path ?? command;
}

export function resolveGraphoxideExecutable(
  extensionUri: vscode.Uri,
  configured: string,
  folder?: vscode.WorkspaceFolder,
): string {
  if (configured !== 'graphoxide') return resolveConfiguredPath(configured, folder);
  const executable = executableName(process.platform);
  const override = process.env.GRAPHOXIDE_BINARY?.trim();
  const candidates = [
    override,
    path.join(extensionUri.fsPath, 'bin', executable),
    findOnPath(executable),
    path.resolve(extensionUri.fsPath, '..', '..', 'target', 'release', executable),
    path.resolve(extensionUri.fsPath, '..', '..', 'target', 'debug', executable),
  ];
  return candidates.find((candidate): candidate is string => Boolean(candidate) && isExecutable(candidate!)) ?? configured;
}

export async function resolveExecutable(command: string): Promise<string | undefined> {
  if (path.isAbsolute(command)) return command;
  const lookup = process.platform === 'win32' ? 'where.exe' : '/usr/bin/which';
  try {
    const { stdout } = await execFileAsync(lookup, [command], { timeout: 4000 });
    return stdout.split(/\r?\n/u).map((line) => line.trim()).find(Boolean);
  } catch {
    return undefined;
  }
}

export async function commandDetected(command: string): Promise<boolean> {
  return (await resolveExecutable(command)) !== undefined;
}

function resolveConfiguredPath(configured: string, folder?: vscode.WorkspaceFolder): string {
  if (path.isAbsolute(configured)) return configured;
  if (configured.includes('/') || configured.includes('\\')) {
    return folder ? path.resolve(folder.uri.fsPath, configured) : configured;
  }
  return findOnPath(configured) ?? configured;
}

function findOnPath(executable: string): string | undefined {
  const pathValue = process.env.PATH;
  if (!pathValue) return undefined;
  const extensions = process.platform === 'win32'
    ? (process.env.PATHEXT ?? '.EXE;.CMD;.BAT;.COM').split(';')
    : [''];
  for (const directory of pathValue.split(path.delimiter).filter(Boolean)) {
    for (const extension of extensions) {
      const candidate = path.join(directory, process.platform === 'win32' ? `${executable.replace(/\.exe$/iu, '')}${extension}` : executable);
      if (isExecutable(candidate)) return candidate;
    }
  }
  return undefined;
}

function trustedExecutable(extensionUri: vscode.Uri): string | undefined {
  return trustedExecutableCandidates(extensionUri.fsPath, process.platform).find(isExecutable);
}

function isExecutable(file: string): boolean {
  try {
    fs.accessSync(file, process.platform === 'win32' ? fs.constants.F_OK : fs.constants.X_OK);
    return fs.statSync(file).isFile();
  } catch {
    return false;
  }
}
