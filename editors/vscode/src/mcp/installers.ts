import { execFile } from 'node:child_process';
import * as fs from 'node:fs/promises';
import * as os from 'node:os';
import * as path from 'node:path';
import { promisify } from 'node:util';
import * as vscode from 'vscode';
import {
  MCP_SERVER_KEY,
  ServerInvocation,
  desiredMcpJsonEntry,
  invocationForScope,
  mcpJsonEntryMatches,
  openCodeEntryMatches,
  readMcpJsonEntry,
  readOpenCodeEntry,
  removeMcpJson,
  removeOpenCode,
  upsertMcpJson,
  upsertOpenCode,
} from './config';
import { codexInvocationMatches, readCodexInvocation, removeCodexInvocation, upsertCodexInvocation } from './toml';
import { commandDetected, resolveExecutable } from './runtime';

const execFileAsync = promisify(execFile);

export type IntegrationId = 'claude-code' | 'codex' | 'opencode';
export type InstallScope = 'user' | 'project';

export interface ScopeStatus {
  readonly configured: boolean;
  readonly stale: boolean;
  readonly configPath: string;
  readonly detail?: string;
}

export interface IntegrationStatus {
  readonly detected: boolean;
  readonly user: ScopeStatus;
  readonly project?: ScopeStatus;
}

export interface InstallerContext {
  readonly folder?: vscode.WorkspaceFolder;
  /** Workspace-aware invocation used only for project-scope registrations. */
  readonly invocation: ServerInvocation;
  /** Extension-controlled invocation safe to persist across all projects. */
  readonly userInvocation: ServerInvocation;
}

export interface InstallResult {
  readonly ok: boolean;
  readonly message: string;
}

export interface IntegrationInstaller {
  readonly id: IntegrationId;
  readonly displayName: string;
  readonly description: string;
  readonly scopes: readonly InstallScope[];
  status(context: InstallerContext): Promise<IntegrationStatus>;
  install(context: InstallerContext, scope: InstallScope): Promise<InstallResult>;
  uninstall(context: InstallerContext, scope: InstallScope): Promise<InstallResult>;
}

export function allInstallers(): readonly IntegrationInstaller[] {
  return [new ClaudeCodeInstaller(), new CodexInstaller(), new OpenCodeInstaller()];
}

export function installerById(id: string): IntegrationInstaller | undefined {
  return allInstallers().find((installer) => installer.id === id);
}

export async function integrationReports(context: InstallerContext): Promise<readonly { installer: IntegrationInstaller; status: IntegrationStatus }[]> {
  const installers = allInstallers();
  const statuses = await Promise.all(installers.map((installer) => installer.status(context)));
  return installers.map((installer, index) => ({ installer, status: statuses[index]! }));
}

class ClaudeCodeInstaller implements IntegrationInstaller {
  readonly id = 'claude-code' as const;
  readonly displayName = 'Claude Code';
  readonly description = 'User scope uses Claude Code’s MCP registry; project scope writes the shared .mcp.json file.';
  readonly scopes = ['user', 'project'] as const;

  async status(context: InstallerContext): Promise<IntegrationStatus> {
    const detected = Boolean(vscode.extensions.getExtension('anthropic.claude-code')) || await commandDetected('claude');
    const userPath = path.join(os.homedir(), '.claude.json');
    const user = await claudeUserStatus(
      userPath,
      context.userInvocation,
      !detected ? 'Claude Code was not detected.' : undefined,
    );
    const project = context.folder
      ? await mcpJsonStatus(path.join(context.folder.uri.fsPath, '.mcp.json'), context.invocation, 'Shared with the project; Claude asks for trust before first use.')
      : undefined;
    return { detected, user, ...(project ? { project } : {}) };
  }

  async install(context: InstallerContext, scope: InstallScope): Promise<InstallResult> {
    if (scope === 'project') return installMcpJsonProject(context);
    const executable = await resolveExecutable('claude');
    if (!executable) return { ok: false, message: 'The Claude Code CLI was not found on PATH.' };
    const entry = desiredMcpJsonEntry(context.userInvocation);
    await execFileAsync(executable, ['mcp', 'remove', '--scope', 'user', MCP_SERVER_KEY], { timeout: 15000 }).catch(() => undefined);
    try {
      await execFileAsync(executable, ['mcp', 'add-json', '--scope', 'user', MCP_SERVER_KEY, JSON.stringify(entry)], { timeout: 15000 });
      return { ok: true, message: 'Registered Graphoxide for Claude Code at user scope.' };
    } catch (error) {
      return failedCommand('Claude Code registration failed', error);
    }
  }

  async uninstall(context: InstallerContext, scope: InstallScope): Promise<InstallResult> {
    if (scope === 'project') return uninstallMcpJsonProject(context);
    const executable = await resolveExecutable('claude');
    if (!executable) return { ok: false, message: 'The Claude Code CLI was not found on PATH.' };
    try {
      await execFileAsync(executable, ['mcp', 'remove', '--scope', 'user', MCP_SERVER_KEY], { timeout: 15000 });
      return { ok: true, message: 'Removed Graphoxide from Claude Code user scope.' };
    } catch (error) {
      return failedCommand('Claude Code removal failed', error);
    }
  }
}

class CodexInstaller implements IntegrationInstaller {
  readonly id = 'codex' as const;
  readonly displayName = 'Codex';
  readonly description = 'Edits only [mcp_servers.graphoxide] in Codex config.toml while preserving all other settings and comments.';
  readonly scopes = ['user', 'project'] as const;

  async status(context: InstallerContext): Promise<IntegrationStatus> {
    const detected = Boolean(vscode.extensions.getExtension('openai.chatgpt')) || await commandDetected('codex');
    const userPath = path.join(codexHome(), 'config.toml');
    const user = await codexStatus(userPath, context.userInvocation, !detected ? 'Codex was not detected.' : undefined);
    const project = context.folder
      ? await codexStatus(path.join(context.folder.uri.fsPath, '.codex', 'config.toml'), context.invocation, 'Project config is loaded only after Codex trusts this repository.')
      : undefined;
    return { detected, user, ...(project ? { project } : {}) };
  }

  async install(context: InstallerContext, scope: InstallScope): Promise<InstallResult> {
    const file = codexPath(context, scope);
    if (!file) return noWorkspace();
    try {
      const existing = await readOptional(file) ?? '';
      const edit = upsertCodexInvocation(
        existing,
        invocationForScope(context.invocation, context.userInvocation, scope),
        scope === 'project',
      );
      await writeFile(file, edit.content);
      return { ok: true, message: `${edit.existed ? 'Updated' : 'Added'} [mcp_servers.graphoxide] in ${file}.` };
    } catch (error) {
      return failedFile(file, error);
    }
  }

  async uninstall(context: InstallerContext, scope: InstallScope): Promise<InstallResult> {
    const file = codexPath(context, scope);
    if (!file) return noWorkspace();
    try {
      const existing = await readOptional(file);
      if (existing === undefined) return { ok: true, message: 'Graphoxide was not configured in this Codex scope.' };
      const edit = removeCodexInvocation(existing);
      if (edit.existed) await writeFile(file, edit.content);
      return { ok: true, message: edit.existed ? `Removed [mcp_servers.graphoxide] from ${file}.` : 'Graphoxide was not configured in this Codex scope.' };
    } catch (error) {
      return failedFile(file, error);
    }
  }
}

class OpenCodeInstaller implements IntegrationInstaller {
  readonly id = 'opencode' as const;
  readonly displayName = 'OpenCode';
  readonly description = 'Adds a local Graphoxide server to OpenCode’s user or project opencode.json.';
  readonly scopes = ['user', 'project'] as const;

  async status(context: InstallerContext): Promise<IntegrationStatus> {
    const detected = await commandDetected('opencode');
    const userPath = openCodeUserPath();
    const user = await openCodeStatus(userPath, context.userInvocation, !detected ? 'OpenCode was not detected.' : undefined);
    const project = context.folder
      ? await openCodeStatus(path.join(context.folder.uri.fsPath, 'opencode.json'), context.invocation)
      : undefined;
    return { detected, user, ...(project ? { project } : {}) };
  }

  async install(context: InstallerContext, scope: InstallScope): Promise<InstallResult> {
    const file = openCodePath(context, scope);
    if (!file) return noWorkspace();
    try {
      const edit = upsertOpenCode(
        await readOptional(file),
        invocationForScope(context.invocation, context.userInvocation, scope),
      );
      await writeFile(file, edit.content);
      return { ok: true, message: `${edit.existed ? 'Updated' : 'Added'} Graphoxide in ${file}.` };
    } catch (error) {
      return failedFile(file, error);
    }
  }

  async uninstall(context: InstallerContext, scope: InstallScope): Promise<InstallResult> {
    const file = openCodePath(context, scope);
    if (!file) return noWorkspace();
    try {
      const existing = await readOptional(file);
      if (existing === undefined) return { ok: true, message: 'Graphoxide was not configured in this OpenCode scope.' };
      const edit = removeOpenCode(existing);
      if (edit.existed) await writeFile(file, edit.content);
      return { ok: true, message: edit.existed ? `Removed Graphoxide from ${file}.` : 'Graphoxide was not configured in this OpenCode scope.' };
    } catch (error) {
      return failedFile(file, error);
    }
  }
}

async function installMcpJsonProject(context: InstallerContext): Promise<InstallResult> {
  if (!context.folder) return noWorkspace();
  const file = path.join(context.folder.uri.fsPath, '.mcp.json');
  try {
    const edit = upsertMcpJson(await readOptional(file), context.invocation);
    await writeFile(file, edit.content);
    return { ok: true, message: `${edit.existed ? 'Updated' : 'Added'} Graphoxide in ${file}.` };
  } catch (error) {
    return failedFile(file, error);
  }
}

async function uninstallMcpJsonProject(context: InstallerContext): Promise<InstallResult> {
  if (!context.folder) return noWorkspace();
  const file = path.join(context.folder.uri.fsPath, '.mcp.json');
  try {
    const existing = await readOptional(file);
    if (existing === undefined) return { ok: true, message: 'Graphoxide was not configured in the project .mcp.json.' };
    const edit = removeMcpJson(existing);
    if (edit.existed) await writeFile(file, edit.content);
    return { ok: true, message: edit.existed ? `Removed Graphoxide from ${file}.` : 'Graphoxide was not configured in the project .mcp.json.' };
  } catch (error) {
    return failedFile(file, error);
  }
}

async function mcpJsonStatus(file: string, invocation: ServerInvocation, detail?: string): Promise<ScopeStatus> {
  try {
    const entry = readMcpJsonEntry(await readOptional(file));
    return { configured: entry !== undefined, stale: entry !== undefined && !mcpJsonEntryMatches(entry, invocation), configPath: file, ...(detail ? { detail } : {}) };
  } catch (error) {
    return invalidStatus(file, error);
  }
}

async function claudeUserStatus(file: string, invocation: ServerInvocation, detail?: string): Promise<ScopeStatus> {
  try {
    const entry = readClaudeUserEntry(await readOptional(file));
    return {
      configured: entry !== undefined,
      stale: entry !== undefined && !mcpJsonEntryMatches(entry, invocation),
      configPath: file,
      ...(detail ? { detail } : {}),
    };
  } catch (error) {
    return invalidStatus(file, error);
  }
}

async function codexStatus(file: string, invocation: ServerInvocation, detail?: string): Promise<ScopeStatus> {
  try {
    const content = await readOptional(file);
    const configured = content !== undefined && readCodexInvocation(content) !== undefined;
    return { configured, stale: configured && !codexInvocationMatches(content!, invocation), configPath: file, ...(detail ? { detail } : {}) };
  } catch (error) {
    return invalidStatus(file, error);
  }
}

async function openCodeStatus(file: string, invocation: ServerInvocation, detail?: string): Promise<ScopeStatus> {
  try {
    const entry = readOpenCodeEntry(await readOptional(file));
    return { configured: entry !== undefined, stale: entry !== undefined && !openCodeEntryMatches(entry, invocation), configPath: file, ...(detail ? { detail } : {}) };
  } catch (error) {
    return invalidStatus(file, error);
  }
}

function readClaudeUserEntry(content: string | undefined): unknown {
  if (content === undefined) return undefined;
  const parsed: unknown = JSON.parse(content);
  if (typeof parsed !== 'object' || parsed === null || Array.isArray(parsed)) return undefined;
  const servers = (parsed as Record<string, unknown>).mcpServers;
  return typeof servers === 'object' && servers !== null && !Array.isArray(servers)
    ? (servers as Record<string, unknown>)[MCP_SERVER_KEY]
    : undefined;
}

function codexHome(): string {
  const configured = process.env.CODEX_HOME?.trim();
  return configured || path.join(os.homedir(), '.codex');
}

function codexPath(context: InstallerContext, scope: InstallScope): string | undefined {
  if (scope === 'user') return path.join(codexHome(), 'config.toml');
  return context.folder ? path.join(context.folder.uri.fsPath, '.codex', 'config.toml') : undefined;
}

function openCodeUserPath(): string {
  const configHome = process.env.XDG_CONFIG_HOME?.trim() || path.join(os.homedir(), '.config');
  return path.join(configHome, 'opencode', 'opencode.json');
}

function openCodePath(context: InstallerContext, scope: InstallScope): string | undefined {
  if (scope === 'user') return openCodeUserPath();
  return context.folder ? path.join(context.folder.uri.fsPath, 'opencode.json') : undefined;
}

async function readOptional(file: string): Promise<string | undefined> {
  try {
    return await fs.readFile(file, 'utf8');
  } catch (error) {
    if (isNodeError(error) && error.code === 'ENOENT') return undefined;
    throw error;
  }
}

async function writeFile(file: string, content: string): Promise<void> {
  await fs.mkdir(path.dirname(file), { recursive: true });
  await fs.writeFile(file, content, 'utf8');
}

function invalidStatus(file: string, error: unknown): ScopeStatus {
  return { configured: false, stale: false, configPath: file, detail: `Could not parse configuration: ${messageOf(error)}` };
}

function noWorkspace(): InstallResult {
  return { ok: false, message: 'No workspace folder is open.' };
}

function failedFile(file: string, error: unknown): InstallResult {
  return { ok: false, message: `Could not update ${file}: ${messageOf(error)}` };
}

function failedCommand(prefix: string, error: unknown): InstallResult {
  const detail = isNodeError(error) && typeof error.stderr === 'string' ? error.stderr.trim() : messageOf(error);
  return { ok: false, message: `${prefix}: ${detail}` };
}

function messageOf(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function isNodeError(error: unknown): error is NodeJS.ErrnoException & { stderr?: string } {
  return error instanceof Error;
}
