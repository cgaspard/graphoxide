import { execFile } from 'node:child_process';
import * as fs from 'node:fs/promises';
import * as os from 'node:os';
import * as path from 'node:path';
import { promisify } from 'node:util';
import * as vscode from 'vscode';
import {
  MCP_SERVER_KEY,
  ServerInvocation,
  mcpJsonEntryCommand,
  mcpJsonEntryMatches,
  openCodeEntryCommand,
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
import { isAbandonedExtensionBinary } from './stable-binary';

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
  readonly legacyUser: ScopeStatus;
  readonly project?: ScopeStatus;
}

export interface InstallerContext {
  readonly folder?: vscode.WorkspaceFolder;
  /** Workspace-aware invocation persisted only in project-scope registrations. */
  readonly invocation: ServerInvocation;
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

/**
 * Rewrite project registrations whose executable belongs to an extension
 * directory that no longer exists. Releases before the binary was linked to a
 * version-independent path recorded the versioned directory instead, and VS Code
 * deletes that directory on upgrade, so those entries fail to start and nothing
 * else will ever fix them. Returns the integrations that were repaired.
 */
export async function repairAbandonedRegistrations(context: InstallerContext): Promise<readonly string[]> {
  if (!context.folder) return [];
  const repaired: string[] = [];
  for (const { installer, status } of await integrationReports(context)) {
    if (!status.project?.configured || !status.project.stale) continue;
    const command = await persistedProjectCommand(installer.id, status.project.configPath);
    if (!command || !isAbandonedExtensionBinary(command)) continue;
    const result = await installer.install(context, 'project');
    if (result.ok) repaired.push(installer.displayName);
  }
  return repaired;
}

/** The executable a project config currently records, in that client's format. */
async function persistedProjectCommand(id: IntegrationId, file: string): Promise<string | undefined> {
  try {
    const content = await readOptional(file);
    if (content === undefined) return undefined;
    if (id === 'claude-code') return mcpJsonEntryCommand(readMcpJsonEntry(content));
    if (id === 'opencode') return openCodeEntryCommand(readOpenCodeEntry(content));
    return readCodexInvocation(content)?.command;
  } catch {
    // Unparseable config: leave it for the user rather than rewriting blind.
    return undefined;
  }
}

class ClaudeCodeInstaller implements IntegrationInstaller {
  readonly id = 'claude-code' as const;
  readonly displayName = 'Claude Code';
  readonly description = 'Writes Graphoxide to this project’s shared .mcp.json file.';
  readonly scopes = ['project'] as const;

  async status(context: InstallerContext): Promise<IntegrationStatus> {
    const detected = Boolean(vscode.extensions.getExtension('anthropic.claude-code')) || await commandDetected('claude');
    const userPath = path.join(os.homedir(), '.claude.json');
    const legacyUser = await claudeLegacyUserStatus(
      userPath,
      !detected ? 'Claude Code was not detected.' : undefined,
    );
    const project = context.folder
      ? await mcpJsonStatus(path.join(context.folder.uri.fsPath, '.mcp.json'), context.invocation, 'Shared with the project; Claude asks for trust before first use.')
      : undefined;
    return { detected, legacyUser, ...(project ? { project } : {}) };
  }

  async install(context: InstallerContext, scope: InstallScope): Promise<InstallResult> {
    if (scope !== 'project') return projectOnly();
    return installMcpJsonProject(context);
  }

  async uninstall(context: InstallerContext, scope: InstallScope): Promise<InstallResult> {
    if (scope === 'project') return uninstallMcpJsonProject(context);
    const executable = await resolveExecutable('claude');
    if (!executable) return { ok: false, message: 'The Claude Code CLI was not found on PATH.' };
    try {
      await execFileAsync(executable, ['mcp', 'remove', '--scope', 'user', MCP_SERVER_KEY], { timeout: 15000 });
      return { ok: true, message: 'Removed the legacy all-project Graphoxide registration from Claude Code.' };
    } catch (error) {
      return failedCommand('Claude Code removal failed', error);
    }
  }
}

class CodexInstaller implements IntegrationInstaller {
  readonly id = 'codex' as const;
  readonly displayName = 'Codex';
  readonly description = 'Edits only this project’s [mcp_servers.graphoxide] table while preserving other settings and comments.';
  readonly scopes = ['project'] as const;

  async status(context: InstallerContext): Promise<IntegrationStatus> {
    const detected = Boolean(vscode.extensions.getExtension('openai.chatgpt')) || await commandDetected('codex');
    const userPath = path.join(codexHome(), 'config.toml');
    const legacyUser = await codexLegacyStatus(userPath, !detected ? 'Codex was not detected.' : undefined);
    const project = context.folder
      ? await codexStatus(path.join(context.folder.uri.fsPath, '.codex', 'config.toml'), context.invocation, 'Project config is loaded only after Codex trusts this repository.')
      : undefined;
    return { detected, legacyUser, ...(project ? { project } : {}) };
  }

  async install(context: InstallerContext, scope: InstallScope): Promise<InstallResult> {
    if (scope !== 'project') return projectOnly();
    const file = codexPath(context, scope);
    if (!file) return noWorkspace();
    try {
      const existing = await readOptional(file) ?? '';
      const edit = upsertCodexInvocation(
        existing,
        context.invocation,
        true,
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
  readonly description = 'Adds a local Graphoxide server to this project’s opencode.json.';
  readonly scopes = ['project'] as const;

  async status(context: InstallerContext): Promise<IntegrationStatus> {
    const detected = await commandDetected('opencode');
    const userPath = openCodeUserPath();
    const legacyUser = await openCodeLegacyStatus(userPath, !detected ? 'OpenCode was not detected.' : undefined);
    const project = context.folder
      ? await openCodeStatus(path.join(context.folder.uri.fsPath, 'opencode.json'), context.invocation)
      : undefined;
    return { detected, legacyUser, ...(project ? { project } : {}) };
  }

  async install(context: InstallerContext, scope: InstallScope): Promise<InstallResult> {
    if (scope !== 'project') return projectOnly();
    const file = openCodePath(context, scope);
    if (!file) return noWorkspace();
    try {
      const edit = upsertOpenCode(
        await readOptional(file),
        context.invocation,
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

async function claudeLegacyUserStatus(file: string, detail?: string): Promise<ScopeStatus> {
  try {
    const entry = readClaudeUserEntry(await readOptional(file));
    return {
      configured: entry !== undefined,
      stale: false,
      configPath: file,
      ...(detail ? { detail } : {}),
    };
  } catch (error) {
    return invalidStatus(file, error);
  }
}

async function codexLegacyStatus(file: string, detail?: string): Promise<ScopeStatus> {
  try {
    const content = await readOptional(file);
    return {
      configured: content !== undefined && readCodexInvocation(content) !== undefined,
      stale: false,
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

async function openCodeLegacyStatus(file: string, detail?: string): Promise<ScopeStatus> {
  try {
    return {
      configured: readOpenCodeEntry(await readOptional(file)) !== undefined,
      stale: false,
      configPath: file,
      ...(detail ? { detail } : {}),
    };
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

function projectOnly(): InstallResult {
  return { ok: false, message: 'Graphoxide MCP registrations are project-scoped. Open a workspace and install Graphoxide for that project.' };
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
