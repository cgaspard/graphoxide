import assert from 'node:assert/strict';
import test from 'node:test';
import {
  mcpJsonEntryMatches,
  openCodeEntryMatches,
  readMcpJsonEntry,
  readOpenCodeEntry,
  removeMcpJson,
  removeOpenCode,
  upsertMcpJson,
  upsertOpenCode,
} from '../src/mcp/config';
import {
  codexInvocationMatches,
  readCodexInvocation,
  removeCodexInvocation,
  upsertCodexInvocation,
} from '../src/mcp/toml';

const invocation = {
  command: '/Applications/Graphoxide/bin/graphoxide',
  args: ['--profile', 'team one', 'serve'],
  cwd: '/work/example',
} as const;

test('edits only Graphoxide in a shared .mcp.json document', () => {
  const original = JSON.stringify({ project: 'kept', mcpServers: { existing: { command: 'other' } } });
  const inserted = upsertMcpJson(original, invocation);
  const parsed = JSON.parse(inserted.content) as { project: string; mcpServers: Record<string, unknown> };
  assert.equal(inserted.existed, false);
  assert.equal(parsed.project, 'kept');
  assert.deepEqual(parsed.mcpServers.existing, { command: 'other' });
  assert.equal(mcpJsonEntryMatches(readMcpJsonEntry(inserted.content), invocation), true);

  const removed = removeMcpJson(inserted.content);
  const after = JSON.parse(removed.content) as { mcpServers: Record<string, unknown> };
  assert.equal(removed.existed, true);
  assert.deepEqual(after.mcpServers.existing, { command: 'other' });
  assert.equal(after.mcpServers.graphoxide, undefined);
});

test('preserves OpenCode configuration while adding and removing Graphoxide', () => {
  const original = JSON.stringify({ theme: 'graphoxide-dark', mcp: { existing: { type: 'remote', url: 'https://example.test' } } });
  const inserted = upsertOpenCode(original, invocation);
  const parsed = JSON.parse(inserted.content) as { theme: string; mcp: Record<string, unknown> };
  assert.equal(parsed.theme, 'graphoxide-dark');
  assert.deepEqual(parsed.mcp.existing, { type: 'remote', url: 'https://example.test' });
  assert.equal(openCodeEntryMatches(readOpenCodeEntry(inserted.content), invocation), true);

  const removed = removeOpenCode(inserted.content);
  const after = JSON.parse(removed.content) as { mcp: Record<string, unknown> };
  assert.deepEqual(after.mcp.existing, { type: 'remote', url: 'https://example.test' });
  assert.equal(after.mcp.graphoxide, undefined);
});

test('edits only the Graphoxide Codex TOML table and preserves comments', () => {
  const original = '# user preference\nmodel = "gpt-example"\n\n[mcp_servers.other]\ncommand = "other"\nargs = []\n';
  const inserted = upsertCodexInvocation(original, invocation, true);
  assert.equal(inserted.existed, false);
  assert.match(inserted.content, /# user preference/u);
  assert.match(inserted.content, /\[mcp_servers\.other\]/u);
  assert.deepEqual(readCodexInvocation(inserted.content), invocation);
  assert.equal(codexInvocationMatches(inserted.content, invocation), true);

  const updated = upsertCodexInvocation(inserted.content, { ...invocation, args: ['serve'] }, true);
  assert.equal(updated.existed, true);
  assert.deepEqual(readCodexInvocation(updated.content)?.args, ['serve']);

  const removed = removeCodexInvocation(updated.content);
  assert.equal(removed.existed, true);
  assert.match(removed.content, /# user preference/u);
  assert.match(removed.content, /\[mcp_servers\.other\]/u);
  assert.doesNotMatch(removed.content, /mcp_servers\.graphoxide/u);
});
