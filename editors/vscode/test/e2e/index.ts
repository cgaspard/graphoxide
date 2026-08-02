import assert from 'node:assert/strict';
import { ChildProcessWithoutNullStreams, spawn } from 'node:child_process';
import * as fs from 'node:fs/promises';
import * as path from 'node:path';
import * as vscode from 'vscode';
import { GraphoxideExtensionApi } from '../../src/extension';
import { installerById } from '../../src/mcp/installers';

interface JsonRpcResponse {
  readonly id?: number;
  readonly result?: unknown;
  readonly error?: unknown;
}

export async function run(): Promise<void> {
  const folder = vscode.workspace.workspaceFolders?.[0];
  assert.ok(folder, 'The E2E workspace must be open.');
  const extension = vscode.extensions.getExtension<GraphoxideExtensionApi>('cgaspard.graphoxide-vscode');
  assert.ok(extension, 'The Graphoxide extension was not discovered.');
  const api = await extension.activate();
  assert.equal(api.version, 1);

  const before = await api.status();
  assert.equal(before.enabled, false);
  assert.ok(before.mcp, 'The extension did not resolve an MCP invocation.');
  assert.ok(path.isAbsolute(before.mcp.command), `Expected an absolute Graphoxide executable, got ${before.mcp.command}`);
  await fs.access(before.mcp.command);

  await api.enableWorkspace('manual');
  const enabled = await api.status();
  assert.equal(enabled.enabled, true);
  assert.equal(enabled.freshness, 'manual');
  assert.ok((enabled.nodes ?? 0) >= 50, `Expected a populated graph, got ${enabled.nodes ?? 0} nodes.`);
  assert.ok((enabled.edges ?? 0) >= 50, `Expected graph relationships, got ${enabled.edges ?? 0} edges.`);
  assert.deepEqual(enabled.mcp?.args.slice(-1), ['serve']);
  assert.equal(enabled.mcp?.cwd, folder.uri.fsPath);

  await verifyMcpProtocol(enabled.mcp!);
  await verifyGraphPlacement(folder);
  await verifyProjectInstallers(folder, enabled.mcp!);
  await verifySaveAndWatchUpdates(api, folder, enabled.graphPath!);

  const finalStatus = await api.status();
  assert.equal(finalStatus.enabled, true);
  assert.equal(finalStatus.freshness, 'manual');
  assert.equal(finalStatus.watching, false);
  console.log(`Graphoxide E2E passed: ${finalStatus.nodes} nodes, ${finalStatus.edges} edges.`);
}

async function verifyMcpProtocol(invocation: NonNullable<Awaited<ReturnType<GraphoxideExtensionApi['status']>>['mcp']>): Promise<void> {
  const client = new JsonRpcClient(invocation.command, invocation.args, invocation.cwd);
  try {
    const initialized = await client.request('initialize', {
      protocolVersion: '2025-03-26',
      capabilities: {},
      clientInfo: { name: 'graphoxide-vscode-e2e', version: '1.0.0' },
    });
    assert.match(JSON.stringify(initialized), /graphoxide/iu);
    client.notify('notifications/initialized', {});
    const tools = await client.request('tools/list', {});
    assert.match(JSON.stringify(tools), /query_graph/iu);
    assert.match(JSON.stringify(tools), /graph_stats/iu);
    const stats = await client.request('tools/call', { name: 'graph_stats', arguments: { project_path: invocation.cwd } });
    assert.match(JSON.stringify(stats), /node/iu);
  } finally {
    await client.close();
  }
}

async function verifyGraphPlacement(folder: vscode.WorkspaceFolder): Promise<void> {
  const source = vscode.Uri.joinPath(folder.uri, 'cartograph', 'domain.py');
  const document = await vscode.workspace.openTextDocument(source);
  await vscode.window.showTextDocument(document, { viewColumn: vscode.ViewColumn.One, preview: false });
  const initialGroups = vscode.window.tabGroups.all.length;
  await vscode.commands.executeCommand('graphoxide.openGraph');
  await poll(() => vscode.window.tabGroups.activeTabGroup.activeTab?.label === 'Graphoxide Graph', 'graph webview to open');
  assert.equal(vscode.window.tabGroups.all.length, initialGroups, 'Default graph command unexpectedly created a split editor group.');
  await vscode.commands.executeCommand('workbench.action.closeActiveEditor');

  await vscode.window.showTextDocument(document, { viewColumn: vscode.ViewColumn.One, preview: false });
  await vscode.commands.executeCommand('graphoxide.openGraphBeside');
  await poll(
    () => vscode.window.tabGroups.all.length > initialGroups && vscode.window.tabGroups.all.some((group) => group.tabs.some((tab) => tab.label === 'Graphoxide Graph')),
    'explicit beside graph group to open',
  );
  const graphTab = vscode.window.tabGroups.all.flatMap((group) => group.tabs).find((tab) => tab.label === 'Graphoxide Graph');
  assert.ok(graphTab, 'The beside graph tab was not present.');
  await vscode.window.tabGroups.close(graphTab);
}

async function verifyProjectInstallers(
  folder: vscode.WorkspaceFolder,
  invocation: NonNullable<Awaited<ReturnType<GraphoxideExtensionApi['status']>>['mcp']>,
): Promise<void> {
  const root = folder.uri.fsPath;
  await fs.writeFile(path.join(root, '.mcp.json'), '{\n  "mcpServers": { "sentinel": { "command": "sentinel" } }\n}\n', 'utf8');
  await fs.mkdir(path.join(root, '.codex'), { recursive: true });
  await fs.writeFile(path.join(root, '.codex', 'config.toml'), '[mcp_servers.sentinel]\ncommand = "sentinel"\nargs = []\n', 'utf8');
  await fs.writeFile(path.join(root, 'opencode.json'), '{\n  "theme": "sentinel",\n  "mcp": {}\n}\n', 'utf8');

  for (const id of ['claude-code', 'codex', 'opencode'] as const) {
    const installer = installerById(id);
    assert.ok(installer, `Missing ${id} installer.`);
    const context = { folder, invocation };
    const installed = await installer.install(context, 'project');
    assert.equal(installed.ok, true, installed.message);
    const status = await installer.status(context);
    assert.equal(status.project?.configured, true, `${id} project registration was not detected.`);
    assert.equal(status.project?.stale, false, `${id} project registration was immediately stale.`);
    const removed = await installer.uninstall(context, 'project');
    assert.equal(removed.ok, true, removed.message);
    const after = await installer.status(context);
    assert.equal(after.project?.configured, false, `${id} project registration was not removed.`);
  }

  assert.match(await fs.readFile(path.join(root, '.mcp.json'), 'utf8'), /sentinel/u);
  assert.match(await fs.readFile(path.join(root, '.codex', 'config.toml'), 'utf8'), /mcp_servers\.sentinel/u);
  assert.match(await fs.readFile(path.join(root, 'opencode.json'), 'utf8'), /"theme": "sentinel"/u);
}

async function verifySaveAndWatchUpdates(api: GraphoxideExtensionApi, folder: vscode.WorkspaceFolder, graphPath: string): Promise<void> {
  await api.configureFreshness('save');
  await appendAndSave(vscode.Uri.joinPath(folder.uri, 'cartograph', 'domain.py'), '\n\ndef e2e_save_refresh_marker() -> str:\n    return "save"\n');
  await poll(() => graphContains(graphPath, 'e2e_save_refresh_marker'), 'save-triggered graph update', 30000);

  await api.configureFreshness('watch');
  await poll(async () => (await api.status()).watching, 'watch process to start');
  await appendAndSave(vscode.Uri.joinPath(folder.uri, 'cartograph', 'notifications.py'), '\n\ndef e2e_watch_refresh_marker() -> str:\n    return "watch"\n');
  await poll(() => graphContains(graphPath, 'e2e_watch_refresh_marker'), 'watch-triggered graph update', 30000);
  await api.configureFreshness('manual');
  await poll(async () => !(await api.status()).watching, 'watch process to stop');
}

async function appendAndSave(uri: vscode.Uri, text: string): Promise<void> {
  const document = await vscode.workspace.openTextDocument(uri);
  const edit = new vscode.WorkspaceEdit();
  edit.insert(uri, document.positionAt(document.getText().length), text);
  assert.equal(await vscode.workspace.applyEdit(edit), true);
  assert.equal(await document.save(), true);
}

async function graphContains(graphPath: string, marker: string): Promise<boolean> {
  try {
    const graph = JSON.parse(await fs.readFile(graphPath, 'utf8')) as { nodes?: Array<{ label?: string; id?: string }> };
    return graph.nodes?.some((node) => node.label?.includes(marker) || node.id?.includes(marker)) ?? false;
  } catch {
    return false;
  }
}

async function poll(check: () => boolean | Promise<boolean>, description: string, timeout = 10000): Promise<void> {
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) {
    if (await check()) return;
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  assert.fail(`Timed out waiting for ${description}.`);
}

class JsonRpcClient {
  private readonly child: ChildProcessWithoutNullStreams;
  private readonly pending = new Map<number, { resolve: (value: unknown) => void; reject: (error: Error) => void; timer: NodeJS.Timeout }>();
  private nextId = 1;
  private buffer = '';
  private stderr = '';

  constructor(command: string, args: readonly string[], cwd: string) {
    this.child = spawn(command, [...args], { cwd, env: process.env, shell: false });
    this.child.stdout.on('data', (chunk: Buffer) => this.consume(chunk.toString()));
    this.child.stderr.on('data', (chunk: Buffer) => { this.stderr += chunk.toString(); });
    this.child.on('error', (error) => this.rejectAll(error));
    this.child.on('close', (code) => {
      if (this.pending.size > 0) this.rejectAll(new Error(`MCP server exited with code ${code}: ${this.stderr}`));
    });
  }

  request(method: string, params: unknown): Promise<unknown> {
    const id = this.nextId;
    this.nextId += 1;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`Timed out waiting for MCP ${method}. stderr: ${this.stderr}`));
      }, 10000);
      this.pending.set(id, { resolve, reject, timer });
      this.write({ jsonrpc: '2.0', id, method, params });
    });
  }

  notify(method: string, params: unknown): void {
    this.write({ jsonrpc: '2.0', method, params });
  }

  async close(): Promise<void> {
    if (this.child.exitCode !== null) return;
    this.child.stdin.end();
    await Promise.race([
      new Promise<void>((resolve) => this.child.once('close', () => resolve())),
      new Promise<void>((resolve) => setTimeout(() => {
        this.child.kill('SIGTERM');
        resolve();
      }, 2000)),
    ]);
  }

  private write(message: unknown): void {
    this.child.stdin.write(`${JSON.stringify(message)}\n`);
  }

  private consume(chunk: string): void {
    this.buffer += chunk;
    let newline = this.buffer.indexOf('\n');
    while (newline >= 0) {
      const line = this.buffer.slice(0, newline).trim();
      this.buffer = this.buffer.slice(newline + 1);
      if (line) this.handleLine(line);
      newline = this.buffer.indexOf('\n');
    }
  }

  private handleLine(line: string): void {
    let response: JsonRpcResponse;
    try {
      response = JSON.parse(line) as JsonRpcResponse;
    } catch {
      return;
    }
    if (response.id === undefined) return;
    const pending = this.pending.get(response.id);
    if (!pending) return;
    clearTimeout(pending.timer);
    this.pending.delete(response.id);
    if (response.error !== undefined) pending.reject(new Error(`MCP error: ${JSON.stringify(response.error)}`));
    else pending.resolve(response.result);
  }

  private rejectAll(error: Error): void {
    for (const pending of this.pending.values()) {
      clearTimeout(pending.timer);
      pending.reject(error);
    }
    this.pending.clear();
  }
}
