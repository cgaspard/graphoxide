import { randomBytes } from 'node:crypto';
import * as vscode from 'vscode';
import { GraphoxideCli } from './cli';
import {
  aiSecretKey,
  apiKeyRequired,
  credentialForEndpoint,
  normalizeProviderBaseUrl,
} from './llm/config';
import { AiLabelingService } from './llm/service';
import { FreshnessMode, ManagedWorkspaceService } from './managed';
import {
  InstallScope,
  IntegrationStatus,
  ScopeStatus,
  installerById,
  integrationReports,
} from './mcp/installers';
import { ServerInvocation } from './mcp/config';
import { resolvedInvocation } from './mcp/runtime';
import { GraphStore } from './store';

interface ControlCenterMessage {
  readonly type?: unknown;
  readonly command?: unknown;
  readonly id?: unknown;
  readonly scope?: unknown;
  readonly path?: unknown;
}

interface ControlCenterServices {
  readonly store: GraphStore;
  readonly cli: GraphoxideCli;
  readonly managed: ManagedWorkspaceService;
  readonly aiLabeling: AiLabelingService;
}

interface ScopeRow extends ScopeStatus {
  readonly scope: InstallScope;
}

interface IntegrationRow {
  readonly id: string;
  readonly name: string;
  readonly description: string;
  readonly detected: boolean;
  readonly scopes: readonly ScopeRow[];
}

const CONTROL_CENTER_COMMANDS = new Set([
  'graphoxide.initialize',
  'graphoxide.update',
  'graphoxide.openGraph',
  'graphoxide.openGraphFile',
  'graphoxide.enableWorkspace',
  'graphoxide.disableWorkspace',
  'graphoxide.configureFreshness',
  'graphoxide.startWatch',
  'graphoxide.stopWatch',
  'graphoxide.configureAiLabeling',
  'graphoxide.clearAiCredential',
  'graphoxide.improveCommunityLabels',
  'graphoxide.openSettings',
]);

/** A single dashboard for graph health, managed workspaces, AI labeling, and MCP. */
export class ControlCenterPanel implements vscode.Disposable {
  private static current: ControlCenterPanel | undefined;
  private readonly panel: vscode.WebviewPanel;
  private readonly subscriptions: vscode.Disposable[] = [];
  private readonly allowedPaths = new Set<string>();
  private busy = false;
  private refreshing = false;

  static show(context: vscode.ExtensionContext, services: ControlCenterServices): void {
    const column = vscode.window.activeTextEditor?.viewColumn ?? vscode.ViewColumn.One;
    if (ControlCenterPanel.current) {
      ControlCenterPanel.current.panel.reveal(column);
      void ControlCenterPanel.current.refresh(true);
      return;
    }
    const panel = vscode.window.createWebviewPanel(
      'graphoxide.controlCenter',
      'Graphoxide Control Center',
      column,
      { enableScripts: true, retainContextWhenHidden: true },
    );
    ControlCenterPanel.current = new ControlCenterPanel(panel, context, services);
  }

  private constructor(
    panel: vscode.WebviewPanel,
    private readonly context: vscode.ExtensionContext,
    private readonly services: ControlCenterServices,
  ) {
    this.panel = panel;
    this.panel.iconPath = vscode.Uri.joinPath(context.extensionUri, 'media', 'activity.svg');
    this.panel.webview.html = this.renderHtml();
    this.subscriptions.push(
      this.panel.onDidDispose(() => this.dispose()),
      this.panel.webview.onDidReceiveMessage((message: ControlCenterMessage) => this.handleMessage(message)),
      services.store.onDidChange(() => void this.refresh()),
      services.cli.onDidChangeWatch(() => void this.refresh()),
      services.managed.onDidChangeEnablement(() => void this.refresh()),
      vscode.workspace.onDidChangeConfiguration((event) => {
        if (event.affectsConfiguration('graphoxide')) void this.refresh();
      }),
    );
    void this.refresh();
  }

  dispose(): void {
    for (const subscription of this.subscriptions.splice(0)) subscription.dispose();
    if (ControlCenterPanel.current === this) ControlCenterPanel.current = undefined;
  }

  private async handleMessage(message: ControlCenterMessage): Promise<void> {
    if (message.type === 'refresh') {
      if (!this.busy) await this.refresh(true);
      return;
    }
    if (message.type === 'openPath' && typeof message.path === 'string') {
      if (this.allowedPaths.has(message.path)) await this.openConfig(message.path);
      return;
    }
    if (this.busy) return;
    if (message.type === 'command' && typeof message.command === 'string') {
      const command = message.command;
      if (!CONTROL_CENTER_COMMANDS.has(command)) return;
      await this.runAction(() => vscode.commands.executeCommand(command));
      return;
    }
    if (message.type !== 'install' && message.type !== 'uninstall') return;
    if (typeof message.id !== 'string' || (message.scope !== 'user' && message.scope !== 'project')) return;
    await this.manageIntegration(message.type, message.id, message.scope);
  }

  private async manageIntegration(action: 'install' | 'uninstall', id: string, scope: InstallScope): Promise<void> {
    const installer = installerById(id);
    if (!installer || (scope === 'project' ? !installer.scopes.includes(scope) : action !== 'uninstall')) return;
    const folder = this.services.store.state?.folder ?? await this.services.store.preferredFolder(false);
    const invocation = await resolvedInvocation(folder, this.context);
    const installerContext = { folder, invocation };
    const status = await installer.status(installerContext);
    const scopeStatus = scope === 'user' ? status.legacyUser : status.project;
    if (!scopeStatus) return;
    if (scope === 'user' && !scopeStatus.configured) return;
    const target = scope === 'user' ? 'the legacy all-project registration' : 'this project';
    const operation = action === 'uninstall'
      ? 'Remove'
      : scopeStatus.configured
        ? 'Update'
        : 'Install';
    const detail = scope === 'user'
      ? `${scopeStatus.configPath}\n\nThis removes only the legacy Graphoxide entry. Other configuration is preserved.`
      : `${scopeStatus.configPath}\n\nCommand: ${formatInvocation(invocation)}\n\nOnly Graphoxide's MCP entry is changed; other configuration is preserved.`;
    const choice = await (action === 'uninstall'
      ? vscode.window.showWarningMessage(
          `Remove Graphoxide MCP from ${installer.displayName} for ${target}?`,
          { modal: true, detail },
          operation,
        )
      : vscode.window.showInformationMessage(
          `${operation} Graphoxide MCP for ${installer.displayName} in ${target}?`,
          { modal: true, detail },
          operation,
        ));
    if (choice !== operation) return;
    await this.runAction(async () => {
      const result = action === 'install'
        ? await installer.install(installerContext, scope)
        : await installer.uninstall(installerContext, scope);
      if (result.ok) void vscode.window.showInformationMessage(result.message);
      else void vscode.window.showErrorMessage(result.message);
    });
  }

  private async runAction(action: () => unknown): Promise<void> {
    this.busy = true;
    this.post({ type: 'busy', busy: true });
    try {
      await action();
    } finally {
      this.busy = false;
      this.post({ type: 'busy', busy: false });
      await this.refresh(true);
    }
  }

  private async refresh(reloadGraph = false): Promise<void> {
    if (this.refreshing) return;
    this.refreshing = true;
    try {
      const folder = this.services.store.state?.folder ?? await this.services.store.preferredFolder(false);
      const graphState = reloadGraph && folder
        ? await this.services.store.load(folder)
        : this.services.store.state;
      const invocation = await resolvedInvocation(folder, this.context);
      const reports = await integrationReports({ folder, invocation });
      const rows: IntegrationRow[] = reports.map(({ installer, status }) => ({
        id: installer.id,
        name: installer.displayName,
        description: installer.description,
        detected: status.detected,
        scopes: scopeRows(installer.scopes, status),
      }));
      this.allowedPaths.clear();
      if (graphState?.graphUri.fsPath) this.allowedPaths.add(graphState.graphUri.fsPath);
      for (const row of rows) {
        for (const scope of row.scopes) this.allowedPaths.add(scope.configPath);
      }

      const model = graphState?.model;
      const enabled = folder ? this.services.managed.isEnabled(folder) : false;
      const configuredFreshness: FreshnessMode = folder ? this.services.managed.freshness(folder) : 'manual';
      const configuredScopes = rows.flatMap((row) => row.scopes).filter((scope) => scope.configured).length;
      const staleScopes = rows.flatMap((row) => row.scopes).filter((scope) => scope.stale).length;
      const ai = await this.aiStatus(folder);
      this.post({
        type: 'state',
        workspace: folder ? { name: folder.name, path: folder.uri.fsPath, trusted: vscode.workspace.isTrusted } : null,
        graph: {
          status: graphState?.error ? 'error' : model ? 'ready' : 'missing',
          path: graphState?.graphUri.fsPath ?? (folder ? this.services.store.graphUri(folder).fsPath : null),
          error: graphState?.error ?? null,
          nodes: model?.snapshot.nodes.length ?? 0,
          edges: model?.snapshot.edges.length ?? 0,
          communities: model?.communities().length ?? 0,
          modified: graphState?.modified ?? null,
          builtAtCommit: model?.snapshot.builtAtCommit ?? null,
        },
        managed: {
          enabled,
          freshness: configuredFreshness,
          watching: this.services.cli.watching,
        },
        ai,
        mcp: {
          nativeEnabled: enabled && Boolean(folder),
          invocation: invocation.command,
          configuredScopes,
          staleScopes,
          rows,
        },
      });
    } catch (error) {
      this.post({ type: 'error', message: error instanceof Error ? error.message : String(error) });
    } finally {
      this.refreshing = false;
    }
  }

  private async aiStatus(folder: vscode.WorkspaceFolder | undefined): Promise<Record<string, unknown>> {
    const profile = this.services.aiLabeling.configuredProvider();
    const settings = vscode.workspace.getConfiguration('graphoxide');
    let executable: string | null = null;
    let executableError: string | null = null;
    if (folder) {
      try {
        executable = this.services.cli.trustedInvocation(folder).command;
      } catch (error) {
        executableError = error instanceof Error ? error.message : String(error);
      }
    }
    const timeoutSeconds = settings.get<number>('llm.timeoutSeconds', 600);
    if (!profile) return {
      enabled: false,
      provider: null,
      endpoint: null,
      model: null,
      credentialPresent: false,
      credentialRequired: false,
      executable,
      executableError,
      timeoutSeconds,
    };
    try {
      const endpoint = normalizeProviderBaseUrl(profile, settings.get<string>('llm.baseUrl', profile.defaultBaseUrl));
      const model = settings.get<string>('llm.model', profile.defaultModel).trim() || profile.defaultModel;
      const stored = await this.context.secrets.get(aiSecretKey(profile));
      return {
        enabled: true,
        provider: profile.label,
        endpoint,
        model,
        credentialPresent: Boolean(credentialForEndpoint(stored, endpoint)),
        credentialRequired: apiKeyRequired(profile, endpoint),
        executable,
        executableError,
        timeoutSeconds,
        ...(!model ? { configurationError: `${profile.label} requires a model ID.` } : {}),
      };
    } catch (error) {
      return {
        enabled: true,
        provider: profile.label,
        endpoint: null,
        model: null,
        credentialPresent: false,
        credentialRequired: false,
        executable,
        executableError,
        timeoutSeconds,
        configurationError: error instanceof Error ? error.message : String(error),
      };
    }
  }

  private async openConfig(file: string): Promise<void> {
    try {
      const document = await vscode.workspace.openTextDocument(vscode.Uri.file(file));
      await vscode.window.showTextDocument(document, { preview: true });
    } catch (error) {
      void vscode.window.showWarningMessage(`Could not open ${file}: ${error instanceof Error ? error.message : String(error)}`);
    }
  }

  private post(message: unknown): void {
    void this.panel.webview.postMessage(message);
  }

  private renderHtml(): string {
    const nonce = randomBytes(18).toString('base64');
    const csp = `default-src 'none'; style-src ${this.panel.webview.cspSource} 'unsafe-inline'; script-src 'nonce-${nonce}';`;
    return `<!doctype html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <meta http-equiv="Content-Security-Policy" content="${csp}">
  <title>Graphoxide Control Center</title>
  <style>
    * { box-sizing: border-box; }
    :root { color-scheme: light dark; }
    body { max-width: 1120px; margin: 0 auto; padding: 28px; color: var(--vscode-foreground); background: var(--vscode-editor-background); font-family: var(--vscode-font-family); }
    h1, h2, h3, p { margin-top: 0; } h1 { margin-bottom: 4px; font-size: 25px; } h2 { margin-bottom: 5px; font-size: 18px; } h3 { margin-bottom: 3px; font-size: 14px; }
    .muted, .lead, .detail, .path, dt { color: var(--vscode-descriptionForeground); } .lead { margin-bottom: 0; }
    .header { display: flex; align-items: flex-start; justify-content: space-between; gap: 24px; margin-bottom: 22px; }
    .overview { display: flex; flex-wrap: wrap; gap: 8px; margin-bottom: 18px; }
    .chip { display: inline-flex; align-items: center; gap: 6px; padding: 4px 9px; border: 1px solid var(--vscode-panel-border); border-radius: 999px; font-size: 12px; background: var(--vscode-editorWidget-background); }
    .dot { width: 7px; height: 7px; border-radius: 50%; background: var(--vscode-descriptionForeground); }
    .good .dot { background: var(--vscode-testing-iconPassed); } .warn .dot { background: var(--vscode-editorWarning-foreground); } .bad .dot { background: var(--vscode-errorForeground); }
    .grid { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 14px; }
    .card { min-width: 0; padding: 17px; border: 1px solid var(--vscode-panel-border); border-radius: 9px; background: var(--vscode-editorWidget-background); }
    .wide { grid-column: 1 / -1; }
    .card-head { display: flex; align-items: flex-start; justify-content: space-between; gap: 16px; }
    .badge { flex: none; padding: 2px 8px; border: 1px solid var(--vscode-panel-border); border-radius: 999px; color: var(--vscode-descriptionForeground); font-size: 11px; white-space: nowrap; }
    .badge.good { color: var(--vscode-testing-iconPassed); } .badge.warn { color: var(--vscode-editorWarning-foreground); } .badge.bad { color: var(--vscode-errorForeground); }
    dl { display: grid; grid-template-columns: minmax(92px, auto) minmax(0, 1fr); gap: 7px 12px; margin: 15px 0 0; font-size: 12px; } dt, dd { margin: 0; } dd { min-width: 0; overflow-wrap: anywhere; }
    .metrics { display: grid; grid-template-columns: repeat(3, 1fr); gap: 8px; margin-top: 15px; }
    .metric { padding: 10px; border: 1px solid var(--vscode-panel-border); border-radius: 6px; text-align: center; } .metric strong { display: block; font-size: 18px; } .metric span { color: var(--vscode-descriptionForeground); font-size: 11px; }
    .actions { display: flex; flex-wrap: wrap; gap: 7px; margin-top: 15px; }
    button { min-height: 28px; padding: 5px 11px; border: 1px solid transparent; border-radius: 3px; color: var(--vscode-button-foreground); background: var(--vscode-button-background); font: inherit; cursor: pointer; }
    button.secondary { border-color: var(--vscode-button-border, transparent); color: var(--vscode-button-secondaryForeground); background: var(--vscode-button-secondaryBackground); }
    button.link { min-height: 0; padding: 0; border: 0; color: var(--vscode-textLink-foreground); background: transparent; text-align: left; overflow-wrap: anywhere; }
    button:hover:not(:disabled) { background: var(--vscode-button-hoverBackground); } button.secondary:hover:not(:disabled) { background: var(--vscode-button-secondaryHoverBackground); } button.link:hover:not(:disabled) { color: var(--vscode-textLink-activeForeground); background: transparent; text-decoration: underline; }
    button:focus-visible { outline: 2px solid var(--vscode-focusBorder); outline-offset: 2px; } button:disabled { opacity: .45; cursor: default; }
    .error { margin-bottom: 14px; padding: 10px 12px; border: 1px solid var(--vscode-inputValidation-errorBorder); background: var(--vscode-inputValidation-errorBackground); color: var(--vscode-inputValidation-errorForeground); }
    .section-intro { margin-bottom: 14px; color: var(--vscode-descriptionForeground); font-size: 12px; }
    .native { display: flex; align-items: flex-start; justify-content: space-between; gap: 14px; margin-bottom: 13px; padding: 13px; border: 1px solid var(--vscode-panel-border); border-radius: 7px; }
    .integrations { display: grid; gap: 11px; }
    .integration { padding: 14px; border: 1px solid var(--vscode-panel-border); border-radius: 7px; }
    .scope-grid { display: grid; grid-template-columns: 1fr; gap: 9px; margin-top: 12px; }
    .scope { min-width: 0; padding: 11px; border: 1px solid var(--vscode-panel-border); border-radius: 6px; background: var(--vscode-sideBar-background); }
    .scope-head { display: flex; align-items: flex-start; justify-content: space-between; gap: 8px; } .scope .detail { min-height: 17px; margin: 5px 0 0; font-size: 11px; }
    .scope .path { margin: 7px 0 0; font: 11px var(--vscode-editor-font-family); overflow-wrap: anywhere; }
    .loading { padding: 34px; text-align: center; color: var(--vscode-descriptionForeground); }
    @media (max-width: 720px) { body { padding: 18px; } .header { align-items: stretch; flex-direction: column; gap: 12px; } .header button { align-self: flex-start; } .grid, .scope-grid { grid-template-columns: 1fr; } .wide { grid-column: auto; } }
    @media (forced-colors: active) { .card, .integration, .scope, .native, .chip { border-color: CanvasText; } }
  </style>
</head>
<body>
  <header class="header"><div><h1>Graphoxide Control Center</h1><p class="lead">Manage and monitor this workspace's graph, automation, AI labeling, and MCP connections.</p></div><button class="secondary" id="refresh">Refresh status</button></header>
  <div id="error" class="error" role="alert" hidden></div>
  <div id="content" class="loading" aria-live="polite">Loading Graphoxide status…</div>
  <script nonce="${nonce}">
    const api = acquireVsCodeApi();
    let busy = false;
    document.getElementById('refresh').addEventListener('click', () => api.postMessage({ type: 'refresh' }));
    window.addEventListener('message', event => {
      const message = event.data;
      if (message.type === 'busy') { busy = Boolean(message.busy); updateDisabled(); }
      if (message.type === 'error') showError(message.message);
      if (message.type === 'state') render(message);
    });
    function escapeHtml(value) { return String(value == null ? '' : value).replace(/[&<>"']/g, char => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[char])); }
    function badge(text, state) { return '<span class="badge ' + state + '">' + escapeHtml(text) + '</span>'; }
    function chip(text, state) { return '<span class="chip ' + state + '"><span class="dot" aria-hidden="true"></span>' + escapeHtml(text) + '</span>'; }
    function command(id, label, secondary, disabled) { return '<button ' + (secondary ? 'class="secondary" ' : '') + 'data-command="' + escapeHtml(id) + '"' + (disabled ? ' disabled' : '') + '>' + escapeHtml(label) + '</button>'; }
    function showError(value) { const element = document.getElementById('error'); element.textContent = 'Could not refresh status: ' + value; element.hidden = false; }
    function render(state) {
      document.getElementById('error').hidden = true;
      const graphReady = state.graph.status === 'ready';
      const graphLabel = graphReady ? 'Graph ready' : state.graph.status === 'error' ? 'Graph error' : 'Graph not built';
      const graphTone = graphReady ? 'good' : state.graph.status === 'error' ? 'bad' : 'warn';
      const mcpLabel = state.mcp.staleScopes ? state.mcp.staleScopes + ' MCP update' + (state.mcp.staleScopes === 1 ? '' : 's') : state.mcp.configuredScopes + ' MCP scope' + (state.mcp.configuredScopes === 1 ? '' : 's');
      const mcpTone = state.mcp.staleScopes ? 'warn' : state.mcp.configuredScopes ? 'good' : '';
      const overview = '<div class="overview" aria-label="Integration overview">'
        + chip(graphLabel, graphTone)
        + chip(state.managed.enabled ? 'Managed workspace' : 'Manual workspace', state.managed.enabled ? 'good' : '')
        + chip(state.ai.enabled ? 'AI · ' + state.ai.provider : 'AI disabled', state.ai.enabled ? 'good' : '')
        + chip(mcpLabel, mcpTone)
        + '</div>';
      document.getElementById('content').className = '';
      document.getElementById('content').innerHTML = overview + '<main class="grid">' + graphCard(state) + managedCard(state) + aiCard(state) + mcpCard(state) + '</main>';
      bindActions(); updateDisabled();
    }
    function graphCard(state) {
      const graph = state.graph; const ready = graph.status === 'ready';
      const status = ready ? badge('Ready', 'good') : graph.status === 'error' ? badge('Error', 'bad') : badge('Not built', 'warn');
      const path = graph.path ? '<button class="link" data-path="' + escapeHtml(graph.path) + '" title="Open graph file">' + escapeHtml(graph.path) + '</button>' : 'No workspace';
      const updated = graph.modified ? new Date(graph.modified).toLocaleString() : 'Not available';
      const problem = graph.error ? '<div class="error" role="status">' + escapeHtml(graph.error) + '</div>' : '';
      const actions = ready
        ? command('graphoxide.openGraph', 'Open graph', false, false) + command('graphoxide.update', 'Update graph', true, false) + command('graphoxide.openGraphFile', 'Open graph.json', true, false)
        : command('graphoxide.initialize', 'Extract workspace', false, !state.workspace);
      return '<section class="card" aria-labelledby="graph-heading"><div class="card-head"><div><h2 id="graph-heading">Workspace graph</h2><p class="muted">' + escapeHtml(state.workspace ? state.workspace.name : 'Open a workspace to get started') + '</p></div>' + status + '</div>' + problem
        + '<div class="metrics"><div class="metric"><strong>' + graph.nodes + '</strong><span>Nodes</span></div><div class="metric"><strong>' + graph.edges + '</strong><span>Edges</span></div><div class="metric"><strong>' + graph.communities + '</strong><span>Communities</span></div></div>'
        + '<dl><dt>Graph path</dt><dd>' + path + '</dd><dt>Last updated</dt><dd>' + escapeHtml(updated) + '</dd>' + (graph.builtAtCommit ? '<dt>Source commit</dt><dd>' + escapeHtml(graph.builtAtCommit) + '</dd>' : '') + '</dl><div class="actions">' + actions + '</div></section>';
    }
    function managedCard(state) {
      const value = state.managed; const label = value.enabled ? 'Enabled' : 'Disabled';
      const mode = value.freshness === 'watch' ? 'Continuous watch' : value.freshness === 'save' ? 'Update on save' : 'Manual updates';
      const actions = value.enabled
        ? command('graphoxide.configureFreshness', 'Change update mode', false, false) + command(value.watching ? 'graphoxide.stopWatch' : 'graphoxide.startWatch', value.watching ? 'Stop watcher' : 'Start watcher', true, !state.workspace) + command('graphoxide.disableWorkspace', 'Disable management', true, false)
        : command('graphoxide.enableWorkspace', 'Enable managed workspace', false, !state.workspace);
      return '<section class="card" aria-labelledby="managed-heading"><div class="card-head"><div><h2 id="managed-heading">Workspace management</h2><p class="muted">Keep graph data current while you work.</p></div>' + badge(label, value.enabled ? 'good' : '') + '</div><dl><dt>Update mode</dt><dd>' + escapeHtml(mode) + '</dd><dt>Watcher</dt><dd>' + (value.watching ? 'Running' : 'Stopped') + '</dd><dt>Workspace trust</dt><dd>' + (state.workspace ? state.workspace.trusted ? 'Trusted' : 'Restricted' : 'No workspace') + '</dd></dl><div class="actions">' + actions + '</div></section>';
    }
    function aiCard(state) {
      const ai = state.ai; const keyState = ai.credentialPresent ? 'Stored for this endpoint' : ai.credentialRequired ? 'Required · not stored' : 'Not stored · optional';
      const missingCredential = ai.enabled && ai.credentialRequired && !ai.credentialPresent;
      const issue = ai.configurationError || ai.executableError || (missingCredential ? 'An API key is required for this endpoint.' : '');
      const status = ai.enabled ? (issue ? badge('Needs attention', 'bad') : badge('Configured', 'good')) : badge('Disabled', '');
      const actions = command('graphoxide.configureAiLabeling', ai.enabled ? 'Change AI configuration' : 'Configure AI labeling', false, false)
        + (ai.enabled ? command('graphoxide.improveCommunityLabels', 'Improve community names', true, state.graph.status !== 'ready' || Boolean(issue)) : '')
        + (ai.credentialPresent ? command('graphoxide.clearAiCredential', 'Clear credential', true, false) : '')
        + command('graphoxide.openSettings', 'Advanced settings', true, false);
      return '<section class="card" aria-labelledby="ai-heading"><div class="card-head"><div><h2 id="ai-heading">AI community labeling</h2><p class="muted">Provider credentials stay in VS Code Secret Storage.</p></div>' + status + '</div>'
        + (issue ? '<div class="error" role="status">' + escapeHtml(issue) + '</div>' : '')
        + '<dl><dt>Provider</dt><dd>' + escapeHtml(ai.provider || 'None') + '</dd><dt>Model</dt><dd>' + escapeHtml(ai.model || 'Not configured') + '</dd><dt>Endpoint</dt><dd>' + escapeHtml(ai.endpoint || 'Not configured') + '</dd><dt>Credential</dt><dd>' + escapeHtml(keyState) + '</dd><dt>Request timeout</dt><dd>' + escapeHtml(ai.timeoutSeconds) + ' seconds</dd><dt>Trusted executable</dt><dd>' + escapeHtml(ai.executable || 'Not available') + '</dd></dl><div class="actions">' + actions + '</div></section>';
    }
    function mcpCard(state) {
      const nativeState = state.mcp.nativeEnabled ? badge('Active', 'good') : badge('Inactive', '');
      const rows = state.mcp.rows.map(integrationCard).join('');
      return '<section class="card wide" aria-labelledby="mcp-heading"><div class="card-head"><div><h2 id="mcp-heading">MCP integrations</h2><p class="muted">Connect this workspace’s Graphoxide graph to coding assistants.</p></div>' + badge(state.mcp.configuredScopes + ' installed', state.mcp.configuredScopes ? 'good' : '') + '</div>'
        + '<p class="section-intro">Each project registration starts a local stdio server in this workspace, so it reads this project’s graphoxide-out/graph.json. All-project installation is no longer offered. A legacy global entry appears only so you can remove it safely.</p>'
        + '<div class="native"><div><h3>VS Code native MCP</h3><p class="detail">Provided directly by this extension when managed workspace mode is enabled. No config file is edited.</p><p class="path">' + escapeHtml(state.mcp.invocation) + '</p></div>' + nativeState + '</div>'
        + '<div class="integrations">' + rows + '</div></section>';
    }
    function integrationCard(row) {
      const scopes = [...row.scopes].sort((left, right) => left.scope === right.scope ? 0 : left.scope === 'project' ? -1 : 1);
      return '<article class="integration"><div class="card-head"><div><h3>' + escapeHtml(row.name) + '</h3><p class="detail">' + escapeHtml(row.description) + '</p></div>' + badge(row.detected ? 'Detected' : 'Not detected', row.detected ? 'good' : '') + '</div><div class="scope-grid">' + scopes.map(scope => scopeCard(row, scope)).join('') + '</div></article>';
    }
    function scopeCard(row, scope) {
      const legacy = scope.scope === 'user';
      const title = legacy ? 'Legacy all-project registration' : 'This project';
      const subtitle = legacy ? 'Removal only' : 'Project scope';
      const state = legacy ? 'Remove recommended' : scope.stale ? 'Update needed' : scope.configured ? 'Installed' : 'Not installed';
      const tone = legacy || scope.stale ? 'warn' : scope.configured ? 'good' : '';
      let actions = '';
      if (scope.configured) {
        if (!legacy && scope.stale) actions += '<button data-action="install" data-id="' + escapeHtml(row.id) + '" data-scope="' + scope.scope + '"' + (!row.detected ? ' disabled' : '') + '>Update</button>';
        actions += '<button class="secondary" data-action="uninstall" data-id="' + escapeHtml(row.id) + '" data-scope="' + scope.scope + '">Remove</button>';
        actions += '<button class="secondary" data-path="' + escapeHtml(scope.configPath) + '">Open config</button>';
      } else if (!legacy) {
        actions = '<button data-action="install" data-id="' + escapeHtml(row.id) + '" data-scope="' + scope.scope + '"' + (!row.detected ? ' disabled' : '') + '>Install</button>';
      }
      return '<div class="scope"><div class="scope-head"><div><h3>' + title + '</h3><span class="detail">' + subtitle + '</span></div>' + badge(state, tone) + '</div><p class="path">' + escapeHtml(scope.configPath) + '</p><p class="detail">' + escapeHtml(scope.detail || '') + '</p><div class="actions">' + actions + '</div></div>';
    }
    function bindActions() {
      document.querySelectorAll('[data-command]').forEach(button => button.addEventListener('click', () => api.postMessage({ type: 'command', command: button.dataset.command })));
      document.querySelectorAll('[data-action]').forEach(button => button.addEventListener('click', () => api.postMessage({ type: button.dataset.action, id: button.dataset.id, scope: button.dataset.scope })));
      document.querySelectorAll('[data-path]').forEach(button => button.addEventListener('click', () => api.postMessage({ type: 'openPath', path: button.dataset.path })));
    }
    function updateDisabled() { document.querySelectorAll('button').forEach(button => { if (busy) button.disabled = true; }); document.getElementById('refresh').disabled = busy; }
  </script>
</body>
</html>`;
  }
}

function formatInvocation(invocation: ServerInvocation): string {
  return [invocation.command, ...invocation.args].map((part) => JSON.stringify(part)).join(' ');
}

function scopeRows(scopes: readonly InstallScope[], status: IntegrationStatus): ScopeRow[] {
  const rows: ScopeRow[] = [];
  for (const scope of scopes) {
    const value = scope === 'user' ? status.legacyUser : status.project;
    if (value) rows.push({ scope, ...value });
  }
  if (status.legacyUser.configured) rows.push({ scope: 'user', ...status.legacyUser });
  return rows;
}
