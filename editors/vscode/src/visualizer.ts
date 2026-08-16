import { randomBytes } from 'node:crypto';
import * as vscode from 'vscode';
import { GraphModel } from './graph';
import {
  buildVisualizerSnapshot,
  isVisualizerWebviewMessage,
  MAX_VISUALIZER_EDGE_LIMIT,
  MAX_VISUALIZER_NODE_LIMIT,
  MAX_VISUALIZER_STRING_CODE_UNITS,
} from './visualizer-model';

export interface GraphVisualizerRendererState {
  readonly mode: 'global' | 'focus';
  readonly selectedId: string | null;
  readonly focusId: string | null;
  readonly query: string;
  readonly communityFilter: string | null;
  readonly relationFilter: string | null;
  readonly visibleNodes: number;
  readonly visibleEdges: number;
  readonly traceActive: boolean;
}

export type GraphVisualizerTestAction =
  | 'select-first'
  | 'enter-focus'
  | 'toggle-trace'
  | 'return-global'
  | 'set-query'
  | 'reveal-selected';

type VisualizerAvailability =
  | { readonly status: 'loading' }
  | { readonly status: 'error'; readonly message: string };

interface RendererStateWaiter {
  readonly resolve: (state: GraphVisualizerRendererState) => void;
  readonly reject: (error: Error) => void;
  readonly timeout: NodeJS.Timeout;
}

const TEST_ACTIONS: readonly GraphVisualizerTestAction[] = [
  'select-first',
  'enter-focus',
  'toggle-trace',
  'return-global',
  'set-query',
  'reveal-selected',
];
const TEST_QUERY_LIMIT = 256;
const RENDERER_STATE_TIMEOUT_MS = 10_000;

export class GraphVisualizer implements vscode.Disposable {
  private panel?: vscode.WebviewPanel;
  private model?: GraphModel;
  private selectedCommunity?: string | null;
  private selectedNodeId?: string;
  private availability: VisualizerAvailability = { status: 'loading' };
  private messageSubscription?: vscode.Disposable;
  private webviewReady = false;
  private rendererStateValue?: GraphVisualizerRendererState;
  private readonly rendererStateWaiters = new Set<RendererStateWaiter>();
  private readonly testMode: boolean;

  constructor(
    private readonly extensionUri: vscode.Uri,
    private readonly onReveal: (id: string) => void,
    private readonly onExplain: (id: string) => void,
    extensionMode: vscode.ExtensionMode,
  ) {
    this.testMode = extensionMode !== vscode.ExtensionMode.Production;
  }

  show(model: GraphModel, selectedCommunity?: string, placement: 'active' | 'beside' = 'active'): void {
    this.model = model;
    this.availability = { status: 'loading' };
    this.selectedCommunity = normalizeCommunityScope(model, selectedCommunity);
    this.selectedNodeId = validNodeId(model, this.selectedNodeId);
    this.clearRendererState();

    const column = placement === 'beside'
      ? vscode.ViewColumn.Beside
      : vscode.window.activeTextEditor?.viewColumn ?? this.panel?.viewColumn ?? vscode.ViewColumn.One;
    if (!this.panel) this.createPanel(column);
    this.updatePanelTitle();
    this.publishCurrentState();
    this.panel?.reveal(column, false);
  }

  refresh(model?: GraphModel, error?: string): void {
    this.model = model;
    this.clearRendererState();
    if (model) {
      this.selectedCommunity = retainCommunityScope(model, this.selectedCommunity);
      this.selectedNodeId = validNodeId(model, this.selectedNodeId);
      this.availability = { status: 'loading' };
    } else {
      this.selectedCommunity = undefined;
      this.selectedNodeId = undefined;
      const message = error?.trim();
      this.availability = message ? { status: 'error', message } : { status: 'loading' };
      this.rejectRendererStateWaiters(new Error('The Graphoxide visualizer has no current graph model.'));
    }
    this.updatePanelTitle();
    this.publishCurrentState();
  }

  async visualizerState(): Promise<GraphVisualizerRendererState> {
    this.assertTestMode();
    if (!this.panel) throw new Error('Open the Graphoxide graph before reading renderer state.');
    if (this.rendererStateValue) return copyRendererState(this.rendererStateValue);
    return new Promise<GraphVisualizerRendererState>((resolve, reject) => {
      const waiter: RendererStateWaiter = {
        resolve,
        reject,
        timeout: setTimeout(() => {
          this.rendererStateWaiters.delete(waiter);
          reject(new Error(`Timed out after ${RENDERER_STATE_TIMEOUT_MS} ms waiting for Graphoxide renderer state.`));
        }, RENDERER_STATE_TIMEOUT_MS),
      };
      this.rendererStateWaiters.add(waiter);
    });
  }

  async visualizerAction(action: GraphVisualizerTestAction, value?: string): Promise<void> {
    this.assertTestMode();
    if (!isTestAction(action)) throw new Error('Unsupported Graphoxide visualizer test action.');
    if (action === 'set-query') {
      if (typeof value !== 'string') throw new Error('The set-query visualizer action requires a string value.');
      if (value.length > TEST_QUERY_LIMIT) throw new Error(`Visualizer test queries are limited to ${TEST_QUERY_LIMIT} characters.`);
    } else if (value !== undefined) {
      throw new Error(`The ${action} visualizer action does not accept a value.`);
    }
    if (!this.panel || !this.webviewReady || !this.model) {
      throw new Error('Open a ready Graphoxide graph before sending a visualizer test action.');
    }
    const accepted = await this.panel.webview.postMessage({
      type: 'testAction',
      action,
      ...(value === undefined ? {} : { value }),
    });
    if (!accepted) throw new Error('The Graphoxide renderer did not accept the visualizer test action.');
  }

  dispose(): void {
    const panel = this.panel;
    this.clearPanel(new Error('The Graphoxide visualizer was disposed.'));
    panel?.dispose();
  }

  private createPanel(column: vscode.ViewColumn): void {
    const mediaRoot = vscode.Uri.joinPath(this.extensionUri, 'media');
    const webviewRoot = vscode.Uri.joinPath(this.extensionUri, 'dist', 'webview');
    const panel = vscode.window.createWebviewPanel(
      'graphoxide.graph',
      'Graphoxide Graph',
      column,
      {
        enableScripts: true,
        retainContextWhenHidden: true,
        localResourceRoots: [mediaRoot, webviewRoot],
      },
    );
    this.panel = panel;
    this.webviewReady = false;
    panel.iconPath = vscode.Uri.joinPath(mediaRoot, 'activity.svg');
    panel.onDidDispose(() => this.clearPanel(new Error('The Graphoxide visualizer was closed.')));
    this.messageSubscription = panel.webview.onDidReceiveMessage((message: unknown) => this.handleMessage(message));
    panel.webview.html = renderShell(panel.webview, this.extensionUri, this.testMode);
  }

  private clearPanel(error: Error): void {
    this.messageSubscription?.dispose();
    this.messageSubscription = undefined;
    this.panel = undefined;
    this.webviewReady = false;
    this.selectedNodeId = undefined;
    this.clearRendererState();
    this.rejectRendererStateWaiters(error);
  }

  private handleMessage(message: unknown): void {
    if (isRendererStateMessage(message)) {
      if (!this.model || !rendererIdsAreCurrent(message.state, this.model)) return;
      this.selectedNodeId = message.state.selectedId ?? undefined;
      if (!this.testMode) return;
      const state = copyRendererState(message.state);
      this.rendererStateValue = state;
      for (const waiter of this.rendererStateWaiters) {
        clearTimeout(waiter.timeout);
        waiter.resolve(copyRendererState(state));
      }
      this.rendererStateWaiters.clear();
      return;
    }
    if (!isVisualizerWebviewMessage(message)) return;
    if (message.type === 'ready') {
      this.webviewReady = true;
      this.publishCurrentState();
      return;
    }
    if (!this.model?.getNode(message.id)) return;
    if (message.type === 'reveal') this.onReveal(message.id);
    else this.onExplain(message.id);
  }

  private publishCurrentState(): void {
    const panel = this.panel;
    if (!panel || !this.webviewReady) return;
    void panel.webview.postMessage({
      type: 'sourceLinks',
      enabled: vscode.workspace.getConfiguration('graphoxide').get<boolean>('sourceLinks.enabled', true),
    });
    const model = this.model;
    if (!model) {
      void panel.webview.postMessage({ type: 'status', ...this.availability });
      return;
    }
    try {
      const nodeLimit = vscode.workspace.getConfiguration('graphoxide').get<number>('visualization.maxNodes', 750);
      const selectedNodeId = validNodeId(model, this.selectedNodeId);
      const graph = buildVisualizerSnapshot(model, {
        nodeLimit,
        ...(selectedNodeId === undefined ? {} : { selectedNodeId }),
        ...(this.selectedCommunity === undefined ? {} : { communityId: this.selectedCommunity }),
      });
      void panel.webview.postMessage({ type: 'replaceGraph', graph });
    } catch (error) {
      this.model = undefined;
      this.selectedNodeId = undefined;
      this.clearRendererState();
      const message = error instanceof Error ? error.message : String(error);
      this.availability = { status: 'error', message };
      void panel.webview.postMessage({ type: 'status', status: 'error', message });
    }
  }

  private updatePanelTitle(): void {
    if (!this.panel) return;
    this.panel.title = this.selectedCommunity === undefined
      ? 'Graphoxide Graph'
      : `Graphoxide · Community ${this.selectedCommunity ?? 'unassigned'}`;
  }

  private clearRendererState(): void {
    this.rendererStateValue = undefined;
  }

  private rejectRendererStateWaiters(error: Error): void {
    for (const waiter of this.rendererStateWaiters) {
      clearTimeout(waiter.timeout);
      waiter.reject(error);
    }
    this.rendererStateWaiters.clear();
  }

  private assertTestMode(): void {
    if (!this.testMode) throw new Error('Graphoxide visualizer test controls are unavailable in production.');
  }
}

function normalizeCommunityScope(model: GraphModel, selectedCommunity?: string): string | null | undefined {
  if (selectedCommunity === undefined) return undefined;
  const community = selectedCommunity === 'unassigned' ? null : selectedCommunity;
  return model.snapshot.nodes.some((node) => (node.community ?? null) === community) ? community : undefined;
}

function retainCommunityScope(model: GraphModel, selectedCommunity?: string | null): string | null | undefined {
  if (selectedCommunity === undefined) return undefined;
  return model.snapshot.nodes.some((node) => (node.community ?? null) === selectedCommunity)
    ? selectedCommunity
    : undefined;
}

function validNodeId(model: GraphModel, id?: string): string | undefined {
  return id !== undefined && model.getNode(id) ? id : undefined;
}

function rendererIdsAreCurrent(state: GraphVisualizerRendererState, model: GraphModel): boolean {
  return (state.selectedId === null || Boolean(model.getNode(state.selectedId)))
    && (state.focusId === null || Boolean(model.getNode(state.focusId)));
}

function isTestAction(value: unknown): value is GraphVisualizerTestAction {
  return typeof value === 'string' && (TEST_ACTIONS as readonly string[]).includes(value);
}

function isRendererStateMessage(value: unknown): value is { readonly type: 'rendererState'; readonly state: GraphVisualizerRendererState } {
  if (!isRecord(value) || value.type !== 'rendererState' || !isRecord(value.state)) return false;
  const state = value.state;
  return (state.mode === 'global' || state.mode === 'focus')
    && isNullableBoundedString(state.selectedId, MAX_VISUALIZER_STRING_CODE_UNITS)
    && isNullableBoundedString(state.focusId, MAX_VISUALIZER_STRING_CODE_UNITS)
    && typeof state.query === 'string'
    && state.query.length <= TEST_QUERY_LIMIT
    && isNullableBoundedString(state.communityFilter, MAX_VISUALIZER_STRING_CODE_UNITS)
    && isNullableBoundedString(state.relationFilter, MAX_VISUALIZER_STRING_CODE_UNITS)
    && isBoundedCount(state.visibleNodes, MAX_VISUALIZER_NODE_LIMIT)
    && isBoundedCount(state.visibleEdges, MAX_VISUALIZER_EDGE_LIMIT)
    && typeof state.traceActive === 'boolean';
}

function copyRendererState(state: GraphVisualizerRendererState): GraphVisualizerRendererState {
  return {
    mode: state.mode,
    selectedId: state.selectedId,
    focusId: state.focusId,
    query: state.query,
    communityFilter: state.communityFilter,
    relationFilter: state.relationFilter,
    visibleNodes: state.visibleNodes,
    visibleEdges: state.visibleEdges,
    traceActive: state.traceActive,
  };
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function isNullableBoundedString(value: unknown, maximum: number): value is string | null {
  return value === null || (typeof value === 'string' && value.length <= maximum);
}

function isBoundedCount(value: unknown, maximum: number): value is number {
  return typeof value === 'number' && Number.isSafeInteger(value) && value >= 0 && value <= maximum;
}

function renderShell(webview: vscode.Webview, extensionUri: vscode.Uri, testMode: boolean): string {
  const nonce = randomBytes(18).toString('base64');
  const styleUri = webview.asWebviewUri(vscode.Uri.joinPath(extensionUri, 'media', 'graph-visualizer.css'));
  const scriptUri = webview.asWebviewUri(vscode.Uri.joinPath(extensionUri, 'dist', 'webview', 'graph-visualizer.js'));
  const csp = [
    "default-src 'none'",
    "base-uri 'none'",
    "form-action 'none'",
    "object-src 'none'",
    "frame-src 'none'",
    "connect-src 'none'",
    `img-src ${webview.cspSource} data:`,
    `style-src ${webview.cspSource}`,
    `script-src 'nonce-${nonce}'`,
  ].join('; ');
  return `<!doctype html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta http-equiv="Content-Security-Policy" content="${escapeAttribute(csp)}">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <title>Graphoxide Graph</title>
  <link rel="stylesheet" href="${escapeAttribute(styleUri.toString())}">
</head>
<body>
  <main id="graphoxide-root" data-test-mode="${String(testMode)}"></main>
  <script nonce="${escapeAttribute(nonce)}" src="${escapeAttribute(scriptUri.toString())}"></script>
</body>
</html>`;
}

function escapeAttribute(value: string): string {
  return value.replace(/[&<>"']/gu, (character) => ({
    '&': '&amp;',
    '<': '&lt;',
    '>': '&gt;',
    '"': '&quot;',
    "'": '&#39;',
  })[character] ?? character);
}
