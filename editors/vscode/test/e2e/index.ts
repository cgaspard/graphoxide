import assert from 'node:assert/strict';
import { ChildProcessWithoutNullStreams, spawn } from 'node:child_process';
import * as fs from 'node:fs/promises';
import * as http from 'node:http';
import type { AddressInfo, Socket } from 'node:net';
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
  await verifyAiProviders(api, enabled.graphPath!);
  await verifyControlCenter();
  await verifyCustomOutputMaintenance(api, folder);

  const finalStatus = await api.status();
  assert.equal(finalStatus.enabled, true);
  assert.equal(finalStatus.freshness, 'manual');
  assert.equal(finalStatus.watching, false);
  console.log(`Graphoxide E2E passed: ${finalStatus.nodes} nodes, ${finalStatus.edges} edges.`);
}

interface CapturedProviderRequest {
  readonly method: string;
  readonly path: string;
  readonly authorization?: string;
  readonly body?: Record<string, unknown>;
}

type FakeProviderKind = 'lm-studio' | 'ollama';

class FakeLabelProvider {
  private readonly sockets = new Set<Socket>();
  private server?: http.Server;
  private port = 0;
  readonly requests: CapturedProviderRequest[] = [];

  constructor(
    readonly kind: FakeProviderKind,
    readonly model: string,
    private readonly expectedKey?: string,
    private readonly completionDelayMs = 200,
  ) {}

  get baseUrl(): string {
    return `http://127.0.0.1:${this.port}/v1`;
  }

  async start(): Promise<void> {
    this.server = http.createServer((request, response) => {
      void this.handle(request, response).catch((error: unknown) => {
        response.statusCode = 500;
        response.end(JSON.stringify({ error: error instanceof Error ? error.message : String(error) }));
      });
    });
    this.server.on('connection', (socket) => {
      this.sockets.add(socket);
      socket.on('close', () => this.sockets.delete(socket));
    });
    await new Promise<void>((resolve) => this.server!.listen(0, '127.0.0.1', resolve));
    this.port = (this.server.address() as AddressInfo).port;
  }

  async close(): Promise<void> {
    const server = this.server;
    this.server = undefined;
    for (const socket of this.sockets) socket.destroy();
    this.sockets.clear();
    if (server) await new Promise<void>((resolve) => server.close(() => resolve()));
  }

  find(pathname: string): CapturedProviderRequest[] {
    return this.requests.filter((request) => request.path === pathname);
  }

  private async handle(request: http.IncomingMessage, response: http.ServerResponse): Promise<void> {
    const pathname = new URL(request.url ?? '/', 'http://provider.invalid').pathname;
    const body = await readJsonBody(request);
    const captured: CapturedProviderRequest = {
      method: request.method ?? 'GET',
      path: pathname,
      ...(typeof request.headers.authorization === 'string' ? { authorization: request.headers.authorization } : {}),
      ...(body ? { body } : {}),
    };
    this.requests.push(captured);
    if (this.expectedKey && captured.authorization !== `Bearer ${this.expectedKey}`) {
      this.respond(response, 401, { error: 'API key required' });
      return;
    }
    if (this.kind === 'lm-studio' && pathname === '/v1/models') {
      this.respond(response, 200, { data: [{ id: this.model }] });
      return;
    }
    if (this.kind === 'ollama' && pathname === '/api/tags') {
      this.respond(response, 200, { models: [{ name: this.model, model: this.model }] });
      return;
    }
    if (pathname === '/v1/chat/completions') {
      await new Promise((resolve) => setTimeout(resolve, this.completionDelayMs));
      const prompt = completionPrompt(body);
      const prefix = this.kind === 'lm-studio' ? 'LM Studio' : 'Ollama';
      const labels = Object.fromEntries(
        [...prompt.matchAll(/Community (-?\d+):/gu)].map((match) => [match[1]!, `${prefix} ${match[1]} Architecture`]),
      );
      this.respond(response, 200, {
        choices: [{ message: { content: JSON.stringify(labels) } }],
        usage: { prompt_tokens: 17, completion_tokens: 5 },
      });
      return;
    }
    this.respond(response, 404, { error: `Unhandled ${pathname}` });
  }

  private respond(response: http.ServerResponse, status: number, body: unknown): void {
    response.writeHead(status, { 'content-type': 'application/json' });
    response.end(JSON.stringify(body));
  }
}

async function readJsonBody(request: http.IncomingMessage): Promise<Record<string, unknown> | undefined> {
  const chunks: Buffer[] = [];
  for await (const chunk of request) chunks.push(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk));
  if (chunks.length === 0) return undefined;
  return JSON.parse(Buffer.concat(chunks).toString('utf8')) as Record<string, unknown>;
}

function completionPrompt(body: Record<string, unknown> | undefined): string {
  const messages = body?.messages;
  if (!Array.isArray(messages)) return '';
  const first = messages[0];
  if (typeof first !== 'object' || first === null || Array.isArray(first)) return '';
  const content = (first as Record<string, unknown>).content;
  return typeof content === 'string' ? content : '';
}

async function verifyAiProviders(api: GraphoxideExtensionApi, graphPath: string): Promise<void> {
  assert.ok(api.test, 'The Extension Development Host did not expose Graphoxide test controls.');
  const lmKey = 'lm-studio-e2e-key-never-log';
  const lmStudio = new FakeLabelProvider('lm-studio', 'e2e/lm-studio-model', lmKey, 250);
  const ollama = new FakeLabelProvider('ollama', 'e2e-ollama:latest', undefined, 250);
  try {
    await lmStudio.start();
    const lmModels = await api.test.configureAi({
      provider: 'lm-studio',
      baseUrl: lmStudio.baseUrl,
      model: lmStudio.model,
      apiKey: lmKey,
      timeoutSeconds: 600,
    });
    assert.deepEqual(lmModels, [lmStudio.model]);
    await api.test.improveCommunityLabels();
    const lmDiscovery = lmStudio.find('/v1/models');
    const lmCompletions = lmStudio.find('/v1/chat/completions');
    assert.equal(lmDiscovery.length, 1);
    assert.equal(lmDiscovery[0]?.authorization, `Bearer ${lmKey}`);
    assert.ok(lmCompletions.length >= 1, 'LM Studio did not receive a completion request.');
    assert.ok(lmCompletions.every((request) => request.authorization === `Bearer ${lmKey}`));
    assert.ok(lmCompletions.every((request) => request.body?.model === lmStudio.model));
    assert.ok(lmCompletions.every((request) => request.body?.reasoning_effort === 'none'));
    await assertGeneratedLabels(graphPath, 'LM Studio', lmKey);
    await lmStudio.close();

    await ollama.start();
    const ollamaModels = await api.test.configureAi({
      provider: 'ollama',
      baseUrl: ollama.baseUrl,
      model: ollama.model,
      timeoutSeconds: 600,
    });
    assert.deepEqual(ollamaModels, [ollama.model]);
    await api.test.improveCommunityLabels();
    const ollamaDiscovery = ollama.find('/api/tags');
    const ollamaCompletions = ollama.find('/v1/chat/completions');
    assert.equal(ollamaDiscovery.length, 1);
    assert.equal(ollamaDiscovery[0]?.authorization, undefined);
    assert.ok(ollamaCompletions.length >= 1, 'Ollama did not receive a completion request.');
    assert.ok(ollamaCompletions.every((request) => request.authorization === undefined));
    assert.ok(ollamaCompletions.every((request) => request.body?.model === ollama.model));
    assert.ok(ollamaCompletions.every((request) => request.body?.reasoning_effort === undefined));
    await assertGeneratedLabels(graphPath, 'Ollama', lmKey);
  } finally {
    await api.test.clearAi();
    await Promise.all([lmStudio.close(), ollama.close()]);
  }
}

async function assertGeneratedLabels(graphPath: string, prefix: string, forbiddenSecret: string): Promise<void> {
  const directory = path.dirname(graphPath);
  const graphText = await fs.readFile(graphPath, 'utf8');
  const graph = JSON.parse(graphText) as { nodes?: Array<{ community_name?: string }> };
  const generated = graph.nodes?.flatMap((node) => node.community_name ? [node.community_name] : []) ?? [];
  assert.ok(generated.length > 0, `${prefix} did not write community names to graph.json.`);
  assert.ok(generated.every((label) => label.startsWith(prefix)), `Unexpected ${prefix} community label.`);
  const sidecar = await fs.readFile(path.join(directory, '.graphoxide_labels.json'), 'utf8');
  const report = await fs.readFile(path.join(directory, 'GRAPH_REPORT.md'), 'utf8');
  assert.match(sidecar, new RegExp(prefix, 'u'));
  assert.match(report, new RegExp(prefix, 'u'));
  assert.doesNotMatch(`${graphText}\n${sidecar}\n${report}`, new RegExp(forbiddenSecret, 'u'));
}

async function verifyControlCenter(): Promise<void> {
  await vscode.commands.executeCommand('graphoxide.openControlCenter');
  await poll(
    () => vscode.window.tabGroups.all.some((group) => group.tabs.some((tab) => tab.label === 'Graphoxide Control Center')),
    'Control Center to open',
  );
  const tab = vscode.window.tabGroups.all.flatMap((group) => group.tabs).find((candidate) => candidate.label === 'Graphoxide Control Center');
  assert.ok(tab);
  await vscode.window.tabGroups.close(tab);
}

async function verifyMcpProtocol(invocation: NonNullable<Awaited<ReturnType<GraphoxideExtensionApi['status']>>['mcp']>): Promise<void> {
  const client = new JsonRpcClient(invocation.command, invocation.args, invocation.cwd);
  try {
    const initialized = await client.request('initialize', {
      protocolVersion: '2025-03-26',
      capabilities: {},
      clientInfo: { name: 'graphoxide-vscode-e2e', version: '1.0.0' },
    });
    const initializedText = JSON.stringify(initialized);
    assert.match(initializedText, /graphoxide/iu);
    assert.match(initializedText, /Use Graphoxide before broad filesystem searches/iu);
    client.notify('notifications/initialized', {});
    const tools = await client.request('tools/list', {});
    const toolsText = JSON.stringify(tools);
    assert.match(toolsText, /project_overview/iu);
    assert.match(toolsText, /query_graph/iu);
    assert.match(toolsText, /graph_stats/iu);
    assert.match(toolsText, /readOnlyHint[^}]*true/iu);
    const stats = await client.request('tools/call', { name: 'graph_stats', arguments: { project_path: invocation.cwd } });
    assert.match(JSON.stringify(stats), /node/iu);
    const overview = await client.request('tools/call', {
      name: 'project_overview',
      arguments: { project_path: invocation.cwd, token_budget: 4000 },
    });
    assert.match(JSON.stringify(overview), /CheckoutService/iu);
    assert.match(JSON.stringify(overview), /static-analysis evidence/iu);

    const callFlow = await client.request('tools/call', {
      name: 'query_graph',
      arguments: {
        question: 'CheckoutService checkout',
        context_filter: ['call'],
        depth: 2,
        token_budget: 4000,
        project_path: invocation.cwd,
      },
    });
    const callFlowText = JSON.stringify(callFlow);
    for (const expected of ['reserve', 'charge', 'release', 'save', 'send_confirmation']) {
      assert.match(callFlowText, new RegExp(expected, 'iu'), `Injected checkout call to ${expected} was missing from MCP results.`);
    }
    assert.doesNotMatch(callFlowText, /--references/iu, 'The call relation filter returned a non-call relationship.');

    const releasePath = await client.request('tools/call', {
      name: 'shortest_path',
      arguments: { source: 'checkout', target: 'release', project_path: invocation.cwd },
    });
    assert.match(JSON.stringify(releasePath), /Shortest path \(1 hops\).*--calls.*release/iu);
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
    const context = {
      folder,
      invocation,
    };
    assert.deepEqual(installer.scopes, ['project'], `${id} unexpectedly exposes an all-project install scope.`);
    const rejected = await installer.install(context, 'user');
    assert.equal(rejected.ok, false, `${id} accepted an all-project registration.`);
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

async function verifyCustomOutputMaintenance(api: GraphoxideExtensionApi, folder: vscode.WorkspaceFolder): Promise<void> {
  const testApi = api.test;
  assert.ok(testApi, 'Watch lifecycle controls must be available in Extension Development mode.');
  const configuration = vscode.workspace.getConfiguration('graphoxide', folder.uri);
  const defaultGraph = path.join(folder.uri.fsPath, 'graphoxide-out', 'graph.json');
  await configuration.update('graphPath', 'custom-output/graph.json', vscode.ConfigurationTarget.WorkspaceFolder);
  const graphPath = path.join(folder.uri.fsPath, 'custom-output', 'graph.json');

  await vscode.commands.executeCommand('graphoxide.initialize');
  await poll(() => graphContains(graphPath, 'checkout'), 'custom-output graph build', 30000);

  await api.configureFreshness('save');
  await appendAndSave(
    vscode.Uri.joinPath(folder.uri, 'cartograph', 'inventory.py'),
    '\n\ndef e2e_custom_save_marker() -> str:\n    return "custom-save"\n',
  );
  await poll(() => graphContains(graphPath, 'e2e_custom_save_marker'), 'custom-output save update', 30000);

  await api.configureFreshness('watch');
  const customWatch = testApi.watchLifecycle();
  assert.equal(customWatch.phase, 'ready');
  assert.equal(customWatch.processTarget, 'expected');
  assert.equal(customWatch.graphTarget, 'expected');
  assert.ok(customWatch.activeGeneration, 'The custom-output watch process has no generation.');
  await appendAndSave(
    vscode.Uri.joinPath(folder.uri, 'cartograph', 'payments.py'),
    '\n\ndef e2e_custom_watch_marker() -> str:\n    return "custom-watch"\n',
  );
  await poll(() => graphContains(graphPath, 'e2e_custom_watch_marker'), 'custom-output watch update', 30000);

  assert.equal(await graphContains(defaultGraph, 'e2e_custom_save_marker'), false);
  assert.equal(await graphContains(defaultGraph, 'e2e_custom_watch_marker'), false);

  const restartBarrier = testApi.holdNextGraphPathRestart();
  let staleGraphAfterExit: Buffer | undefined;
  try {
    await configuration.update('graphPath', 'intermediate-output/graph.json', vscode.ConfigurationTarget.WorkspaceFolder);
    const stopped = await restartBarrier.waitUntilReached();
    assert.equal(stopped.phase, 'stopped');
    assert.equal(stopped.activeGeneration, undefined);
    assert.equal(stopped.lastExitedGeneration, customWatch.activeGeneration);
    assert.equal(stopped.processTarget, 'none');
    assert.equal(stopped.graphTarget, 'different', 'The new graph path was published before the old process exited.');
    staleGraphAfterExit = await fs.readFile(graphPath);
    // Queue the final project-scoped path while the first reload is paused.
    // The waiter below must observe the tail of both configuration events, not
    // an intermediate replacement that happens to report readiness first.
    await configuration.update('graphPath', 'graphoxide-out/graph.json', vscode.ConfigurationTarget.WorkspaceFolder);
  } finally {
    restartBarrier.release();
  }

  assert.ok(staleGraphAfterExit, 'The stale graph was not captured after the old watch process exited.');
  const replacement = await testApi.waitForWatchRestart(customWatch.activeGeneration);
  assert.equal(replacement.phase, 'ready');
  assert.ok((replacement.activeGeneration ?? 0) > customWatch.activeGeneration);
  assert.ok(replacement.lastExitedGeneration >= customWatch.activeGeneration);
  assert.equal(replacement.processTarget, 'expected');
  assert.equal(replacement.graphTarget, 'expected');
  assert.equal((await api.status()).graphPath, defaultGraph);

  const replacementPublication = waitForGraphMarker(
    defaultGraph,
    'e2e_changed_graph_path_marker',
    'the replacement watch process to publish to the configured graph',
    30000,
  );
  await appendAndSave(
    vscode.Uri.joinPath(folder.uri, 'cartograph', 'audit.py'),
    '\n\ndef e2e_changed_graph_path_marker() -> str:\n    return "changed-graph-path"\n',
  );
  await replacementPublication;
  assert.deepEqual(
    await fs.readFile(graphPath),
    staleGraphAfterExit,
    'The exited watch process mutated its stale output after replacement publication.',
  );

  const replacementGeneration = replacement.activeGeneration;
  assert.ok(replacementGeneration, 'The replacement watch process has no generation.');
  const concurrentRestart = await testApi.restartWatchConcurrently();
  assert.equal(concurrentRestart.phase, 'ready');
  const concurrentGeneration = concurrentRestart.activeGeneration;
  assert.ok(concurrentGeneration, 'The concurrent replacement watch process has no generation.');
  assert.equal(
    concurrentGeneration,
    replacementGeneration + 1,
    'Concurrent direct callers did not converge on exactly one replacement generation.',
  );
  assert.equal(concurrentRestart.lastExitedGeneration, replacementGeneration);
  assert.equal(concurrentRestart.processTarget, 'expected');
  assert.equal(concurrentRestart.graphTarget, 'expected');

  await api.configureFreshness('manual');
  const finalWatch = testApi.watchLifecycle();
  assert.equal(finalWatch.phase, 'stopped');
  assert.ok(finalWatch.lastExitedGeneration >= concurrentGeneration);
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

async function waitForGraphMarker(
  graphPath: string,
  marker: string,
  description: string,
  timeoutMs: number,
): Promise<void> {
  const watcher = vscode.workspace.createFileSystemWatcher(
    new vscode.RelativePattern(path.dirname(graphPath), path.basename(graphPath)),
  );
  let observation = 'graph not inspected';
  await new Promise<void>((resolve, reject) => {
    let settled = false;
    const finish = (error?: Error): void => {
      if (settled) return;
      settled = true;
      clearTimeout(timeout);
      watcher.dispose();
      if (error) reject(error);
      else resolve();
    };
    const inspect = async (): Promise<void> => {
      try {
        const graph = JSON.parse(await fs.readFile(graphPath, 'utf8')) as { nodes?: Array<{ label?: string; id?: string }> };
        const containsMarker = graph.nodes?.some((node) => node.label?.includes(marker) || node.id?.includes(marker)) ?? false;
        observation = containsMarker ? 'readable graph with marker' : 'readable graph without marker';
        if (containsMarker) finish();
      } catch {
        observation = 'graph missing or unreadable';
      }
    };
    const inspectAfterChange = (): void => {
      void inspect();
    };
    watcher.onDidCreate(inspectAfterChange);
    watcher.onDidChange(inspectAfterChange);
    const timeout = setTimeout(
      () => finish(new Error(`Timed out after ${timeoutMs} ms waiting for ${description}; observed ${observation}.`)),
      timeoutMs,
    );
    void inspect();
  });
}

async function poll(check: () => boolean | Promise<boolean>, description: string, timeout = 10000, interval = 100): Promise<void> {
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) {
    if (await check()) return;
    await new Promise((resolve) => setTimeout(resolve, interval));
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
