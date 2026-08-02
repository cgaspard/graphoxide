export const MCP_SERVER_KEY = 'graphoxide';

export interface ServerInvocation {
  readonly command: string;
  readonly args: readonly string[];
  readonly cwd?: string;
}

export interface McpJsonEntry {
  readonly type: 'stdio';
  readonly command: string;
  readonly args: string[];
  readonly cwd?: string;
}

interface McpJsonDocument {
  mcpServers?: Record<string, unknown>;
  [key: string]: unknown;
}

interface OpenCodeDocument {
  $schema?: string;
  mcp?: Record<string, unknown>;
  [key: string]: unknown;
}

interface OpenCodeEntry {
  readonly type: 'local';
  readonly command: string[];
  readonly enabled: true;
}

export interface EditResult {
  readonly content: string;
  readonly existed: boolean;
}

export function desiredMcpJsonEntry(invocation: ServerInvocation, includeCwd = false): McpJsonEntry {
  return {
    type: 'stdio',
    command: invocation.command,
    args: [...invocation.args],
    ...(includeCwd && invocation.cwd ? { cwd: invocation.cwd } : {}),
  };
}

export function mcpJsonEntryMatches(value: unknown, invocation: ServerInvocation): boolean {
  if (!isRecord(value)) return false;
  return value.type === 'stdio'
    && value.command === invocation.command
    && stringArrayEquals(value.args, invocation.args);
}

export function readMcpJsonEntry(content: string | undefined): unknown {
  if (content === undefined) return undefined;
  const document = parseJsonObject(content);
  return isRecord(document.mcpServers) ? document.mcpServers[MCP_SERVER_KEY] : undefined;
}

export function upsertMcpJson(content: string | undefined, invocation: ServerInvocation): EditResult {
  const document: McpJsonDocument = content === undefined ? {} : parseJsonObject(content);
  const servers = isRecord(document.mcpServers) ? document.mcpServers : {};
  const existed = servers[MCP_SERVER_KEY] !== undefined;
  servers[MCP_SERVER_KEY] = desiredMcpJsonEntry(invocation);
  document.mcpServers = servers;
  return { content: formatJson(document), existed };
}

export function removeMcpJson(content: string): EditResult {
  const document: McpJsonDocument = parseJsonObject(content);
  const servers = isRecord(document.mcpServers) ? document.mcpServers : undefined;
  const existed = servers?.[MCP_SERVER_KEY] !== undefined;
  if (servers && existed) delete servers[MCP_SERVER_KEY];
  return { content: formatJson(document), existed };
}

export function desiredOpenCodeEntry(invocation: ServerInvocation): OpenCodeEntry {
  return { type: 'local', command: [invocation.command, ...invocation.args], enabled: true };
}

export function openCodeEntryMatches(value: unknown, invocation: ServerInvocation): boolean {
  if (!isRecord(value)) return false;
  return value.type === 'local'
    && value.enabled !== false
    && stringArrayEquals(value.command, [invocation.command, ...invocation.args]);
}

export function readOpenCodeEntry(content: string | undefined): unknown {
  if (content === undefined) return undefined;
  const document = parseJsonObject(content) as OpenCodeDocument;
  return isRecord(document.mcp) ? document.mcp[MCP_SERVER_KEY] : undefined;
}

export function upsertOpenCode(content: string | undefined, invocation: ServerInvocation): EditResult {
  const document: OpenCodeDocument = content === undefined ? {} : parseJsonObject(content);
  const servers = isRecord(document.mcp) ? document.mcp : {};
  const existed = servers[MCP_SERVER_KEY] !== undefined;
  servers[MCP_SERVER_KEY] = desiredOpenCodeEntry(invocation);
  document.mcp = servers;
  document.$schema ??= 'https://opencode.ai/config.json';
  return { content: formatJson(document), existed };
}

export function removeOpenCode(content: string): EditResult {
  const document = parseJsonObject(content) as OpenCodeDocument;
  const servers = isRecord(document.mcp) ? document.mcp : undefined;
  const existed = servers?.[MCP_SERVER_KEY] !== undefined;
  if (servers && existed) delete servers[MCP_SERVER_KEY];
  return { content: formatJson(document), existed };
}

function parseJsonObject(content: string): Record<string, unknown> {
  const parsed: unknown = JSON.parse(content);
  if (!isRecord(parsed)) throw new Error('configuration root must be a JSON object');
  return parsed;
}

function formatJson(value: unknown): string {
  return `${JSON.stringify(value, null, 2)}\n`;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function stringArrayEquals(value: unknown, expected: readonly string[]): boolean {
  return Array.isArray(value)
    && value.length === expected.length
    && value.every((item, index) => typeof item === 'string' && item === expected[index]);
}
