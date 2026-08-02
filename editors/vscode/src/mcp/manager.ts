import { randomBytes } from 'node:crypto';
import * as vscode from 'vscode';
import { InstallScope, IntegrationStatus, ScopeStatus, installerById, integrationReports } from './installers';
import { resolvedInvocation } from './runtime';

interface ManagerMessage {
  readonly type?: unknown;
  readonly id?: unknown;
  readonly scope?: unknown;
  readonly path?: unknown;
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

export class McpManagerPanel implements vscode.Disposable {
  private static current: McpManagerPanel | undefined;
  private readonly panel: vscode.WebviewPanel;
  private readonly subscriptions: vscode.Disposable[] = [];
  private busy = false;

  static show(context: vscode.ExtensionContext, workspaceEnabled: () => boolean): void {
    const column = vscode.window.activeTextEditor?.viewColumn ?? vscode.ViewColumn.One;
    if (McpManagerPanel.current) {
      McpManagerPanel.current.panel.reveal(column);
      void McpManagerPanel.current.refresh();
      return;
    }
    const panel = vscode.window.createWebviewPanel(
      'graphoxide.mcpManager',
      'Graphoxide MCP Integrations',
      column,
      { enableScripts: true, retainContextWhenHidden: true },
    );
    McpManagerPanel.current = new McpManagerPanel(panel, context, workspaceEnabled);
  }

  private constructor(
    panel: vscode.WebviewPanel,
    private readonly context: vscode.ExtensionContext,
    private readonly workspaceEnabled: () => boolean,
  ) {
    this.panel = panel;
    this.panel.iconPath = vscode.Uri.joinPath(context.extensionUri, 'media', 'activity.svg');
    this.panel.webview.html = this.renderHtml();
    this.subscriptions.push(
      this.panel.onDidDispose(() => this.dispose()),
      this.panel.webview.onDidReceiveMessage((message: ManagerMessage) => this.handleMessage(message)),
    );
    void this.refresh();
  }

  dispose(): void {
    for (const subscription of this.subscriptions.splice(0)) subscription.dispose();
    if (McpManagerPanel.current === this) McpManagerPanel.current = undefined;
  }

  private async handleMessage(message: ManagerMessage): Promise<void> {
    if (message.type === 'refresh') {
      await this.refresh();
      return;
    }
    if (message.type === 'open' && typeof message.path === 'string') {
      await this.openConfig(message.path);
      return;
    }
    if (this.busy || (message.type !== 'install' && message.type !== 'uninstall')) return;
    if (typeof message.id !== 'string' || (message.scope !== 'user' && message.scope !== 'project')) return;
    const installer = installerById(message.id);
    if (!installer) return;
    const scope = message.scope;
    if (message.type === 'install') {
      const target = scope === 'user' ? 'your user configuration (all projects)' : 'this project';
      const choice = await vscode.window.showInformationMessage(
        `Install Graphoxide MCP for ${installer.displayName} in ${target}?`,
        { modal: true },
        'Install',
      );
      if (choice !== 'Install') return;
    }
    this.busy = true;
    this.post({ type: 'busy', busy: true });
    try {
      const folder = vscode.workspace.workspaceFolders?.[0];
      const invocation = await resolvedInvocation(folder, this.context.extensionUri);
      const result = message.type === 'install'
        ? await installer.install({ folder, invocation }, scope)
        : await installer.uninstall({ folder, invocation }, scope);
      if (result.ok) void vscode.window.showInformationMessage(result.message);
      else void vscode.window.showErrorMessage(result.message);
    } finally {
      this.busy = false;
      this.post({ type: 'busy', busy: false });
      await this.refresh();
    }
  }

  private async refresh(): Promise<void> {
    const folder = vscode.workspace.workspaceFolders?.[0];
    const invocation = await resolvedInvocation(folder, this.context.extensionUri);
    const reports = await integrationReports({ folder, invocation });
    const rows: IntegrationRow[] = reports.map(({ installer, status }) => ({
      id: installer.id,
      name: installer.displayName,
      description: installer.description,
      detected: status.detected,
      scopes: scopeRows(installer.scopes, status),
    }));
    this.post({
      type: 'state',
      rows,
      workspace: folder?.name ?? null,
      nativeEnabled: this.workspaceEnabled(),
      command: invocation.command,
    });
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
  <title>Graphoxide MCP Integrations</title>
  <style>
    * { box-sizing: border-box; }
    body { max-width: 980px; margin: 0 auto; padding: 28px; color: var(--vscode-foreground); background: var(--vscode-editor-background); font-family: var(--vscode-font-family); }
    h1 { margin: 0 0 5px; font-size: 24px; } .lead { margin: 0 0 22px; color: var(--vscode-descriptionForeground); }
    .native, .card { margin-bottom: 14px; padding: 16px; border: 1px solid var(--vscode-panel-border); border-radius: 8px; background: var(--vscode-editorWidget-background); }
    .native { display: flex; align-items: center; justify-content: space-between; gap: 20px; }
    .native p, .card p { margin: 4px 0 0; color: var(--vscode-descriptionForeground); font-size: 13px; }
    .card-head { display: flex; align-items: baseline; justify-content: space-between; gap: 12px; } h2 { margin: 0; font-size: 17px; }
    .badge { padding: 2px 8px; border-radius: 999px; border: 1px solid var(--vscode-panel-border); font-size: 11px; }
    .badge.on { color: var(--vscode-testing-iconPassed); } .badge.off { color: var(--vscode-descriptionForeground); }
    .scopes { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 10px; margin-top: 14px; }
    .scope { padding: 12px; border: 1px solid var(--vscode-panel-border); border-radius: 6px; }
    .scope-title { display: flex; justify-content: space-between; gap: 8px; font-weight: 600; }
    .path { margin-top: 5px; color: var(--vscode-descriptionForeground); font: 11px var(--vscode-editor-font-family); overflow-wrap: anywhere; cursor: pointer; }
    .detail { min-height: 18px; margin-top: 4px; color: var(--vscode-descriptionForeground); font-size: 11px; }
    .actions { display: flex; gap: 7px; margin-top: 10px; }
    button { padding: 5px 11px; border: 0; border-radius: 3px; color: var(--vscode-button-foreground); background: var(--vscode-button-background); cursor: pointer; }
    button.secondary { color: var(--vscode-button-secondaryForeground); background: var(--vscode-button-secondaryBackground); }
    button:disabled { opacity: .45; cursor: default; }
    .toolbar { display: flex; justify-content: flex-end; margin-bottom: 12px; }
    @media (max-width: 680px) { body { padding: 18px; } .scopes { grid-template-columns: 1fr; } .native { align-items: flex-start; flex-direction: column; } }
  </style>
</head>
<body>
  <h1>MCP integrations</h1>
  <p class="lead">Graphoxide is a stdio MCP server. AI clients start it when they need tools; this extension manages registration, not a persistent background process.</p>
  <div class="native"><div><strong>VS Code native MCP</strong><p>Registered through the extension API for the current workspace.</p></div><span class="badge" id="native-badge">Checking…</span></div>
  <div class="toolbar"><button class="secondary" id="refresh">Refresh detection</button></div>
  <div id="cards">Detecting installed tools…</div>
  <script nonce="${nonce}">
    const api = acquireVsCodeApi(); let busy = false;
    document.getElementById('refresh').addEventListener('click', () => api.postMessage({ type: 'refresh' }));
    window.addEventListener('message', event => {
      const message = event.data;
      if (message.type === 'busy') { busy = message.busy; disableButtons(); }
      if (message.type === 'state') render(message);
    });
    function escapeHtml(value) { return String(value).replace(/[&<>"]/g, char => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[char])); }
    function render(message) {
      const nativeBadge = document.getElementById('native-badge');
      nativeBadge.textContent = message.nativeEnabled ? 'Enabled' : 'Workspace not enabled';
      nativeBadge.className = 'badge ' + (message.nativeEnabled ? 'on' : 'off');
      document.getElementById('cards').innerHTML = message.rows.map(row => card(row)).join('');
      document.querySelectorAll('[data-action]').forEach(button => button.addEventListener('click', () => api.postMessage({ type: button.dataset.action, id: button.dataset.id, scope: button.dataset.scope })));
      document.querySelectorAll('[data-path]').forEach(element => element.addEventListener('click', () => api.postMessage({ type: 'open', path: element.dataset.path })));
      disableButtons();
    }
    function card(row) {
      return '<section class="card"><div class="card-head"><h2>' + escapeHtml(row.name) + '</h2><span class="badge ' + (row.detected ? 'on' : 'off') + '">' + (row.detected ? 'Detected' : 'Not detected') + '</span></div><p>' + escapeHtml(row.description) + '</p><div class="scopes">' + row.scopes.map(scope => scopeCard(row, scope)).join('') + '</div></section>';
    }
    function scopeCard(row, scope) {
      const state = scope.stale ? 'Update needed' : (scope.configured ? 'Installed' : 'Not installed');
      const stateClass = scope.configured && !scope.stale ? 'on' : 'off';
      const action = scope.configured ? 'uninstall' : 'install';
      const label = scope.configured ? 'Remove' : 'Install';
      return '<div class="scope"><div class="scope-title"><span>' + (scope.scope === 'user' ? 'User · all projects' : 'Project · this workspace') + '</span><span class="badge ' + stateClass + '">' + state + '</span></div><div class="path" data-path="' + escapeHtml(scope.configPath) + '">' + escapeHtml(scope.configPath) + '</div><div class="detail">' + escapeHtml(scope.detail || '') + '</div><div class="actions"><button data-action="' + action + '" data-id="' + row.id + '" data-scope="' + scope.scope + '" ' + (!row.detected && !scope.configured ? 'disabled' : '') + '>' + label + '</button>' + (scope.stale ? '<button class="secondary" data-action="install" data-id="' + row.id + '" data-scope="' + scope.scope + '">Update</button>' : '') + '</div></div>';
    }
    function disableButtons() { if (busy) document.querySelectorAll('button').forEach(button => button.disabled = true); }
  </script>
</body>
</html>`;
  }
}

function scopeRows(scopes: readonly InstallScope[], status: IntegrationStatus): ScopeRow[] {
  const rows: ScopeRow[] = [];
  for (const scope of scopes) {
    const value = scope === 'user' ? status.user : status.project;
    if (value) rows.push({ scope, ...value });
  }
  return rows;
}
