import { execFile } from 'node:child_process';
import * as fs from 'node:fs';
import * as path from 'node:path';
import { promisify } from 'node:util';
import * as vscode from 'vscode';
import { ServerInvocation } from './config';

const execFileAsync = promisify(execFile);

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

export async function resolvedInvocation(folder?: vscode.WorkspaceFolder, extensionUri?: vscode.Uri): Promise<ServerInvocation> {
  if (extensionUri) return extensionInvocation(extensionUri, folder);
  const configured = configuredInvocation(folder);
  const command = await resolveExecutable(configured.command) ?? configured.command;
  return { ...configured, command };
}

export function resolveGraphoxideExecutable(
  extensionUri: vscode.Uri,
  configured: string,
  folder?: vscode.WorkspaceFolder,
): string {
  if (configured !== 'graphoxide') return resolveConfiguredPath(configured, folder);
  const executable = process.platform === 'win32' ? 'graphoxide.exe' : 'graphoxide';
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

function isExecutable(file: string): boolean {
  try {
    fs.accessSync(file, process.platform === 'win32' ? fs.constants.F_OK : fs.constants.X_OK);
    return fs.statSync(file).isFile();
  } catch {
    return false;
  }
}
