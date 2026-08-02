import { MCP_SERVER_KEY, ServerInvocation } from './config';

interface BlockSpan {
  readonly start: number;
  readonly end: number;
}

export interface TomlEditResult {
  readonly content: string;
  readonly existed: boolean;
}

export function readCodexInvocation(content: string): ServerInvocation | undefined {
  const lines = content.split('\n');
  const span = findBlock(lines);
  if (!span) return undefined;
  let command: string | undefined;
  let args: string[] = [];
  let cwd: string | undefined;
  for (let index = span.start + 1; index < span.end; index += 1) {
    const line = stripComment(lines[index] ?? '').trim();
    const value = rightHandSide(line);
    if (line.startsWith('command') && value) command = parseTomlString(value);
    if (line.startsWith('args') && value) args = parseStringArray(value);
    if (line.startsWith('cwd') && value) cwd = parseTomlString(value);
  }
  return command === undefined ? undefined : { command, args, ...(cwd ? { cwd } : {}) };
}

export function codexInvocationMatches(content: string, invocation: ServerInvocation): boolean {
  const current = readCodexInvocation(content);
  return current !== undefined
    && current.command === invocation.command
    && current.args.length === invocation.args.length
    && current.args.every((argument, index) => argument === invocation.args[index]);
}

export function upsertCodexInvocation(content: string, invocation: ServerInvocation, includeCwd: boolean): TomlEditResult {
  const block = serializeBlock(invocation, includeCwd);
  if (content.trim().length === 0) return { content: `${block}\n`, existed: false };
  const lines = content.split('\n');
  const span = findBlock(lines);
  if (span) {
    const merged = [...lines.slice(0, span.start), ...block.split('\n'), ...lines.slice(span.end)];
    return { content: merged.join('\n'), existed: true };
  }
  return { content: `${content.replace(/\s*$/u, '')}\n\n${block}\n`, existed: false };
}

export function removeCodexInvocation(content: string): TomlEditResult {
  const lines = content.split('\n');
  const span = findBlock(lines);
  if (!span) return { content, existed: false };
  const before = lines.slice(0, span.start);
  const after = lines.slice(span.end);
  if (before.at(-1)?.trim() === '') before.pop();
  else if (after[0]?.trim() === '') after.shift();
  const merged = [...before, ...after].join('\n').replace(/\s*$/u, '');
  return { content: merged.length > 0 ? `${merged}\n` : '', existed: true };
}

function serializeBlock(invocation: ServerInvocation, includeCwd: boolean): string {
  const args = invocation.args.map(tomlString).join(', ');
  const lines = [
    `[mcp_servers.${MCP_SERVER_KEY}]`,
    `command = ${tomlString(invocation.command)}`,
    `args = [${args}]`,
  ];
  if (includeCwd && invocation.cwd) lines.push(`cwd = ${tomlString(invocation.cwd)}`);
  return lines.join('\n');
}

function findBlock(lines: readonly string[]): BlockSpan | undefined {
  const headers = new Set([`[mcp_servers.${MCP_SERVER_KEY}]`, `[mcp_servers."${MCP_SERVER_KEY}"]`]);
  const start = lines.findIndex((line) => headers.has(stripComment(line).trim()));
  if (start < 0) return undefined;
  let end = lines.length;
  for (let index = start + 1; index < lines.length; index += 1) {
    if (/^\[\[?[^\]]+\]\]?$/u.test(stripComment(lines[index] ?? '').trim())) {
      end = index;
      break;
    }
  }
  return { start, end };
}

function tomlString(value: string): string {
  return `"${value.replace(/\\/gu, '\\\\').replace(/"/gu, '\\"').replace(/\n/gu, '\\n').replace(/\r/gu, '\\r').replace(/\t/gu, '\\t')}"`;
}

function parseTomlString(value: string): string | undefined {
  const trimmed = value.trim();
  if (!trimmed.startsWith('"') || !trimmed.endsWith('"')) return undefined;
  const inner = trimmed.slice(1, -1);
  let output = '';
  for (let index = 0; index < inner.length; index += 1) {
    const character = inner[index];
    if (character !== '\\') {
      output += character;
      continue;
    }
    const escaped = inner[index + 1];
    index += 1;
    if (escaped === 'n') output += '\n';
    else if (escaped === 'r') output += '\r';
    else if (escaped === 't') output += '\t';
    else if (escaped !== undefined) output += escaped;
  }
  return output;
}

function parseStringArray(value: string): string[] {
  const trimmed = value.trim();
  if (!trimmed.startsWith('[') || !trimmed.endsWith(']')) return [];
  const body = trimmed.slice(1, -1).trim();
  if (body.length === 0) return [];
  return splitTopLevel(body)
    .map(parseTomlString)
    .filter((entry): entry is string => entry !== undefined);
}

function splitTopLevel(value: string): string[] {
  const entries: string[] = [];
  let current = '';
  let quoted = false;
  for (let index = 0; index < value.length; index += 1) {
    const character = value[index];
    if (character === '"' && !isEscaped(value, index)) quoted = !quoted;
    if (character === ',' && !quoted) {
      entries.push(current.trim());
      current = '';
    } else {
      current += character;
    }
  }
  if (current.trim().length > 0) entries.push(current.trim());
  return entries;
}

function isEscaped(value: string, index: number): boolean {
  let slashes = 0;
  for (let cursor = index - 1; cursor >= 0 && value[cursor] === '\\'; cursor -= 1) slashes += 1;
  return slashes % 2 === 1;
}

function stripComment(line: string): string {
  let quoted = false;
  for (let index = 0; index < line.length; index += 1) {
    if (line[index] === '"' && !isEscaped(line, index)) quoted = !quoted;
    if (line[index] === '#' && !quoted) return line.slice(0, index);
  }
  return line;
}

function rightHandSide(line: string): string | undefined {
  const separator = line.indexOf('=');
  return separator < 0 ? undefined : line.slice(separator + 1).trim();
}
