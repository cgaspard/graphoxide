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
  'graphoxide.rebuild',
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
  private refreshPending = false;
  private refreshPendingReload = false;

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
      services.cli.onDidChangeBuildSummary(() => void this.refresh()),
      services.cli.onDidChangeBuildProgress(() => void this.postBuildProgress()),
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
    if (message.type === 'cancelBuild') {
      this.services.cli.cancelActiveBuild();
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
    await this.manageIntegration(message.type as 'install' | 'uninstall', message.id, message.scope as InstallScope);
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
      ? `${scopeStatus.configPath}\n\nThis removes only the Graphoxide entry. Other configuration is preserved.`
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
    if (this.refreshing) {
      this.refreshPending = true;
      this.refreshPendingReload ||= reloadGraph;
      return;
    }
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
      const latestIndex = folder && graphState?.model
        ? await this.services.cli.latestBuildSummary(
            this.services.store.managedOutput(folder).outputDirectory,
            graphState.graphUri,
          )
        : undefined;
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
          exists: graphState?.graphFileExists ?? false,
          path: graphState?.graphUri.fsPath ?? (folder ? this.services.store.graphUri(folder).fsPath : null),
          error: graphState?.error ?? null,
          nodes: model?.snapshot.nodes.length ?? 0,
          edges: model?.snapshot.edges.length ?? 0,
          communities: model?.communities().length ?? 0,
          modified: graphState?.modified ?? null,
          builtAtCommit: model?.snapshot.builtAtCommit ?? null,
          latestIndex: latestIndex ?? null,
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
      if (this.refreshPending) {
        const pendingReload = this.refreshPendingReload;
        this.refreshPending = false;
        this.refreshPendingReload = false;
        await this.refresh(pendingReload);
      }
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

  private postBuildProgress(): void {
    const progress = this.services.cli.buildProgress;
    if (!progress) {
      this.post({ type: 'buildProgress', message: undefined });
    } else {
      this.post({ type: 'buildProgress', message: progress.message });
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
  <title>Graphoxide</title>
  <style>
    * { box-sizing: border-box; }
    :root { color-scheme: light dark; }
    body { width: 100%; max-width: 960px; margin: 0 auto; padding: 24px; color: var(--vscode-foreground); background: var(--vscode-editor-background); font-family: var(--vscode-font-family); }
    h1, h2, h3, p { margin-top: 0; } h1 { margin-bottom: 0; font-size: 20px; } h2 { margin-bottom: 4px; font-size: 15px; } h3 { margin-bottom: 2px; font-size: 12px; }
    .muted, .detail, .path, dt { color: var(--vscode-descriptionForeground); }
    .header { display: flex; align-items: center; justify-content: space-between; gap: 16px; margin-bottom: 14px; }
    .card { min-width: 0; padding: 15px; border: 1px solid var(--vscode-panel-border); border-radius: 9px; background: var(--vscode-editorWidget-background); }
    .card-head { display: flex; align-items: center; justify-content: space-between; gap: 12px; }
    .badge { flex: none; padding: 2px 7px; border: 1px solid var(--vscode-panel-border); border-radius: 999px; color: var(--vscode-descriptionForeground); font-size: 11px; white-space: nowrap; }
    .badge.good { color: var(--vscode-testing-iconPassed); } .badge.warn { color: var(--vscode-editorWarning-foreground); } .badge.bad { color: var(--vscode-errorForeground); }
    dl { display: grid; grid-template-columns: minmax(92px, auto) minmax(0, 1fr); gap: 6px 10px; margin: 12px 0 0; font-size: 12px; } dt, dd { margin: 0; } dd { min-width: 0; overflow-wrap: anywhere; }
    .metrics { display: grid; grid-template-columns: repeat(4, minmax(0, 1fr)); gap: 7px; margin: 12px 0 0; }
    .metric { padding: 8px; border: 1px solid var(--vscode-panel-border); border-radius: 6px; text-align: center; } .metric strong { display: block; font-size: 15px; } .metric span { color: var(--vscode-descriptionForeground); font-size: 10px; }
    .actions { display: flex; flex-wrap: wrap; gap: 7px; margin-top: 12px; }
    button { max-width: 100%; min-height: 26px; padding: 4px 10px; border: 1px solid transparent; border-radius: 3px; color: var(--vscode-button-foreground); background: var(--vscode-button-background); font: inherit; white-space: normal; overflow-wrap: anywhere; cursor: pointer; }
    button.secondary { border-color: var(--vscode-button-border, transparent); color: var(--vscode-button-secondaryForeground); background: var(--vscode-button-secondaryBackground); }
    button.link { min-height: 0; padding: 0; border: 0; color: var(--vscode-textLink-foreground); background: transparent; text-align: left; overflow-wrap: anywhere; }
    button:hover:not(:disabled) { background: var(--vscode-button-hoverBackground); } button.secondary:hover:not(:disabled) { background: var(--vscode-button-secondaryHoverBackground); } button.link:hover:not(:disabled) { color: var(--vscode-textLink-activeForeground); background: transparent; text-decoration: underline; }
    button:focus-visible { outline: 2px solid var(--vscode-focusBorder); outline-offset: 2px; } button:disabled { opacity: .45; cursor: default; }
    .error { margin-bottom: 12px; padding: 8px 10px; border: 1px solid var(--vscode-inputValidation-errorBorder); background: var(--vscode-inputValidation-errorBackground); color: var(--vscode-inputValidation-errorForeground); font-size: 12px; }
    .dashboard { display: grid; gap: 12px; }
    /* Status line */
    .status-line { display: flex; align-items: center; gap: 8px; padding: 9px 12px; margin-bottom: 12px; border-radius: 7px; font-size: 13px; }
    .status-line.ready { background: var(--vscode-testing-iconPassedBackground, transparent); border: 1px solid var(--vscode-testing-iconPassed, transparent); }
    .status-line.error { background: var(--vscode-inputValidation-errorBackground); border: 1px solid var(--vscode-inputValidation-errorBorder); }
    .status-line.missing { background: var(--vscode-editorWidget-background); border: 1px solid var(--vscode-panel-border); }
    .status-line .dot { width: 7px; height: 7px; border-radius: 50%; flex-shrink: 0; }
    .status-line.ready .dot { background: var(--vscode-testing-iconPassed); }
    .status-line.error .dot { background: var(--vscode-errorForeground); }
    .status-line.missing .dot { background: var(--vscode-descriptionForeground); }
    .status-line .meta { color: var(--vscode-descriptionForeground); margin-left: auto; font-size: 11px; }
    /* Build progress banner */
    .build-progress { display: flex; align-items: center; gap: 10px; padding: 9px 12px; margin-bottom: 10px; border-radius: 6px; background: rgba(139,92,246,0.08); border: 1px solid rgba(139,92,246,0.25); font-size: 12px; }
    .build-progress .spinner { width: 14px; height: 14px; border: 2px solid rgba(139,92,246,0.25); border-top-color: #8b5cf6; border-radius: 50%; animation: spin 0.7s linear infinite; flex-shrink: 0; }
    @keyframes spin { to { transform: rotate(360deg); } }
    .build-progress .phase { color: #8b5cf6; white-space: nowrap; flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; }
    .build-progress button.cancel { margin-left: 8px; padding: 2px 8px; min-height: 22px; font-size: 11px; flex-shrink: 0; }
    /* Settings cards */
    .settings-row { display: grid; grid-template-columns: 1fr 1fr; gap: 10px; }
    .settings-card h2 { font-size: 13px; margin-bottom: 6px; }
    .settings-card .inline-status { display: flex; align-items: center; gap: 7px; font-size: 12px; margin-bottom: 8px; }
    .dot-green { width: 6px; height: 6px; border-radius: 50%; background: var(--vscode-testing-iconPassed); flex-shrink: 0; }
    /* MCP compact */
    .mcp-pills { display: flex; gap: 7px; flex-wrap: wrap; margin-top: 6px; }
    .mcp-pill { padding: 3px 9px; border-radius: 999px; font-size: 11px; border: 1px solid var(--vscode-panel-border); background: var(--vscode-editor-background); display: inline-flex; align-items: center; gap: 5px; }
    .mcp-pill .state { width: 6px; height: 6px; border-radius: 50%; }
    .mcp-pill .state.green { background: var(--vscode-testing-iconPassed); }
    .mcp-pill .state.yellow { background: var(--vscode-editorWarning-foreground); }
    .loading { padding: 34px; text-align: center; color: var(--vscode-descriptionForeground); }
    @media (max-width: 760px) { body { padding: 18px; } .settings-row { grid-template-columns: 1fr; } .metrics { grid-template-columns: repeat(2, minmax(0, 1fr)); } }
    @media (forced-colors: active) { .card, .mcp-pill, .build-progress, .status-line { border-color: CanvasText; } }
  </style>
</head>
<body>
  <header class="header"><h1>Graphoxide</h1><button class="secondary" id="refresh">Refresh status</button></header>
  <div id="error" class="error" role="alert" hidden></div>
  <div id="content" class="loading" aria-live="polite">Loading…</div>
  <script nonce="${nonce}">
    const api = acquireVsCodeApi();
    let busy = false;
    let buildProgressMsg = undefined;
    document.getElementById('refresh').addEventListener('click', () => api.postMessage({ type: 'refresh' }));
    window.addEventListener('message', event => {
      const message = event.data;
      if (message.type === 'busy') { busy = Boolean(message.busy); updateDisabled(); }
      if (message.type === 'error') showError(message.message);
      if (message.type === 'state') render(message);
      if (message.type === 'buildProgress') { buildProgressMsg = message.message; updateBuildProgress(); }
    });
    function escapeHtml(value) { return String(value == null ? '' : value).replace(/[&<>"']/g, char => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[char])); }
    function badge(text, state) { return '<span class="badge ' + state + '">' + escapeHtml(text) + '</span>'; }
    function command(id, label, secondary, disabled) { return '<button ' + (secondary ? 'class="secondary" ' : '') + 'data-command="' + escapeHtml(id) + '"' + (disabled ? ' disabled' : '') + '>' + escapeHtml(label) + '</button>'; }
    function formatDuration(milliseconds) { if (milliseconds < 1000) return milliseconds + ' ms'; const seconds = milliseconds / 1000; return (seconds < 10 ? seconds.toFixed(1) : Math.round(seconds)) + ' s'; }
    function formatBytes(bytes) { if (bytes < 1024) return bytes + ' B'; const units = ['KiB', 'MiB', 'GiB']; let value = bytes; let unit = -1; do { value /= 1024; unit += 1; } while (value >= 1024 && unit < units.length - 1); return value.toFixed(value < 10 ? 1 : 0) + ' ' + units[unit]; }
    function formatStages(stages) { const labels = { scan_extract: 'scan/extract', detect: 'detect', extract: 'extract', build: 'build', cluster: 'cluster', write: 'write' }; return Object.keys(labels).filter(key => stages[key] > 0).map(key => labels[key] + ' ' + formatDuration(stages[key])).join(' · '); }
    function abbrevNumber(n) { if (n >= 10000) return (n / 1000).toFixed(n >= 100000 ? 0 : 1) + 'K'; return String(n); }
    function showError(value) { const element = document.getElementById('error'); element.textContent = value; element.hidden = false; }
    function updateBuildProgress() {
      var banner = document.getElementById('build-progress-banner');
      if (!banner) return;
      if (buildProgressMsg === undefined) { banner.style.display = 'none'; }
      else { banner.style.display = 'flex'; banner.querySelector('.phase').textContent = escapeHtml(buildProgressMsg); }
    }
    function render(state) {
      document.getElementById('error').hidden = true;
      var graphReady = state.graph.status === 'ready';
      var statusTone = graphReady ? 'ready' : state.graph.status === 'error' ? 'error' : 'missing';
      var statusLabel = graphReady ? 'Graph ready' : state.graph.status === 'error' ? 'Graph error' : 'Graph not built';
      var managedLabel = state.managed.watching ? 'Watch running' : state.managed.enabled ? 'Managed' : 'Manual';
      var aiLabel = state.ai.enabled ? state.ai.provider + ' configured' : 'AI off';
      var mcpCount = state.mcp.configuredScopes;
      var metaParts = [managedLabel, aiLabel];
      if (mcpCount) metaParts.push(mcpCount + ' MCP');
      var statusLine = '<div class="status-line ' + statusTone + '" aria-label="' + escapeHtml(statusLabel) + '"><span class="dot" aria-hidden="true"></span><span>' + escapeHtml(statusLabel) + '</span><span class="meta">' + (state.workspace ? escapeHtml(state.workspace.name) : '') + '</span></div>';
      var progressBanner = '<div class="build-progress" id="build-progress-banner"' + (buildProgressMsg === undefined ? ' style="display:none"' : '') + '><div class="spinner" aria-hidden="true"></div><span class="phase">' + escapeHtml(buildProgressMsg || '') + '</span><button class="secondary cancel" data-action="cancelBuild">Cancel</button></div>';
      var graph = state.graph;
      var pathLink = graph.path ? '<button class="link" data-path="' + escapeHtml(graph.path) + '" title="Open">' + escapeHtml(graph.path) + '</button>' : '—';
      var updated = graph.modified ? new Date(graph.modified).toLocaleString() : '—';
      var latest = graph.latestIndex;
      var sourceBytesStr = latest && latest.sourceBytes != null ? '<div class="metric"><strong>' + abbrevNumber(latest.sourceBytes) + '</strong><span>Source</span></div>' : '';
      var problem = graph.error ? '<div class="error">' + escapeHtml(graph.error) + '</div>' : '';
      var ready = graph.status === 'ready';
      var exists = graph.exists;
      var actions = ready
        ? command('graphoxide.update', 'Update incrementally', false, false) + command('graphoxide.rebuild', 'Full rebuild…', true, false) + command('graphoxide.openGraph', 'Open graph', true, false)
        : exists
          ? command('graphoxide.rebuild', 'Full rebuild…', false, !state.workspace)
          : command('graphoxide.initialize', 'Build graph', false, !state.workspace);
      var watcherStatus = state.managed.watching ? '<span class="dot-green"></span>Running' : 'Stopped';
      var modeLabel = state.managed.freshness === 'watch' ? 'Continuous watch' : state.managed.freshness === 'save' ? 'Update on save' : 'Manual';
      var mcpPills = state.mcp.rows.map(row => {
        var pillClass = row.scopes.some(s => s.configured && !s.stale) ? 'green' : 'yellow';
        return '<span class="mcp-pill"><span class="state ' + pillClass + '"></span>' + escapeHtml(row.name) + '</span>';
      }).join('');
      var latestIndexHtml = '';
      if (latest) {
        var latestStages = formatStages(latest.stagesMs);
        var sourceSizeLine = latest.sourceBytes != null ? '<dt>Indexed source size</dt><dd>' + escapeHtml(formatBytes(latest.sourceBytes)) + '</dd>' : '';
        var changedLine = latest.mode === 'incremental' ? '<dt>Changed / deleted</dt><dd>' + latest.files.changed + ' / ' + latest.files.deleted + '</dd>' : '';
        latestIndexHtml = '<h3 style="font-size:12px;margin-top:14px;color:var(--vscode-descriptionForeground)">Latest index</h3><dl><dt>Total time</dt><dd>' + escapeHtml(formatDuration(latest.elapsedMs)) + '</dd><dt>Operation</dt><dd>' + (latest.mode === 'full' ? 'Full rebuild' : 'Incremental update') + '</dd><dt>Indexed inputs</dt><dd>' + latest.files.indexed + '</dd>' + sourceSizeLine + changedLine + '<dt>Completed</dt><dd>' + escapeHtml(new Date(latest.completedAt).toLocaleString()) + '</dd>' + (latestStages ? '<dt>Stages</dt><dd>' + escapeHtml(latestStages) + '</dd>' : '') + '</dl>';
      }
      document.getElementById('content').className = '';
      document.getElementById('content').innerHTML =
        statusLine +
        problem +
        '<main class="dashboard">' +
        '<section class="card">' + progressBanner +
        '<div class="metrics"><div class="metric"><strong>' + abbrevNumber(graph.nodes) + '</strong><span>Nodes</span></div><div class="metric"><strong>' + abbrevNumber(graph.edges) + '</strong><span>Edges</span></div><div class="metric"><strong>' + abbrevNumber(graph.communities) + '</strong><span>Communities</span></div>' + sourceBytesStr + '</div>' +
        '<dl><dt>Graph path</dt><dd>' + pathLink + '</dd><dt>Last updated</dt><dd>' + escapeHtml(updated) + '</dd></dl>' +
        latestIndexHtml +
        '<div class="actions">' + actions + '</div></section>' +
        '<div class="settings-row">' +
        '<div class="card settings-card"><h2>Workspace</h2><div class="inline-status"><span class="' + (state.managed.enabled ? 'dot-green' : '') + '" style="width:6px;height:6px;border-radius:50%;background:' + (state.managed.enabled ? 'var(--vscode-testing-iconPassed)' : 'var(--vscode-descriptionForeground)') + ';"></span><span>' + escapeHtml(modeLabel) + ' · ' + watcherStatus + '</span></div>' +
        '<div class="actions" style="margin-top:6px;">' + command('graphoxide.configureFreshness', 'Change mode', true, false) + (state.managed.watching ? command('graphoxide.stopWatch', 'Stop watcher', true, !state.workspace) : command('graphoxide.startWatch', 'Start watcher', true, !state.workspace)) + '</div></div>' +
        '<div class="card settings-card"><h2>AI Labeling</h2><div class="inline-status"><span style="color:' + (state.ai.enabled ? 'var(--vscode-testing-iconPassed)' : 'var(--vscode-descriptionForeground)') + ';font-size:12px;">' + escapeHtml(state.ai.enabled ? state.ai.provider : 'Disabled') + '</span></div>' +
        '<div class="actions" style="margin-top:6px;">' + command('graphoxide.configureAiLabeling', state.ai.enabled ? 'Configure' : 'Set up', true, false) + (state.ai.enabled ? command('graphoxide.improveCommunityLabels', 'Improve names', true, !ready) : '') + '</div></div>' +
        '</div>' +
        (state.mcp.rows.length > 0 ? '<div class="card"><h2>MCP Integrations</h2><div class="mcp-pills">' + mcpPills + '</div></div>' : '') +
        '</main>';
      bindActions(); updateDisabled();
    }
    function bindActions() {
      document.querySelectorAll('[data-command]').forEach(button => button.addEventListener('click', () => api.postMessage({ type: 'command', command: button.dataset.command })));
      document.querySelectorAll('[data-action]').forEach(button => button.addEventListener('click', () => {
        if (button.dataset.action === 'cancelBuild') api.postMessage({ type: 'cancelBuild' });
      }));
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
