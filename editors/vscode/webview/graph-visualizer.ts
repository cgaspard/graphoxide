/*
 * Graphoxide cinematic graph webview.
 *
 * The extension host owns graph loading and source navigation. This browser
 * client only consumes inert, bounded snapshots and sends node IDs back to the
 * host. The HTML shell must provide:
 *
 *   <main id="graphoxide-root"></main>
 *
 * Development Extension Hosts may opt into the strictly bounded test bridge
 * with `data-test-mode="true"` on that root. Production shells must omit it.
 *
 * Host -> client messages:
 *   { type: 'replaceGraph', graph: VisualizerSnapshotV1 }
 *   { type: 'status', status: 'loading' | 'error', message?: string }
 *   { type: 'sourceLinks', enabled: boolean }
 *   { type: 'testAction', action: TestAction, value?: string } // test mode only
 *
 * Client -> host messages:
 *   { type: 'ready' }
 *   { type: 'reveal' | 'explain', id: string }
 *   { type: 'rendererState', state: RendererState }
 */

interface GraphoxideVsCodeApi {
  postMessage(message: ClientMessage): void;
  getState(): unknown;
  setState(state: PersistedState): void;
}

declare function acquireVsCodeApi(): GraphoxideVsCodeApi;

type GraphMode = 'global' | 'focus';
type Density = 'focus' | 'balanced' | 'complete';
type TestAction = 'select-first' | 'enter-focus' | 'toggle-trace' | 'return-global' | 'set-query' | 'reveal-selected';

interface VisualizerNodeV1 {
  readonly id: string;
  readonly label: string;
  readonly file: string;
  readonly location: string | null;
  readonly kind: string;
  readonly community: string | null;
  readonly communityName: string | null;
  readonly degree: number;
  readonly inDegree: number;
  readonly outDegree: number;
}

interface VisualizerEdgeV1 {
  readonly source: string;
  readonly target: string;
  readonly relation: string;
  readonly confidence: string | null;
  readonly sourceFile: string | null;
  readonly sourceLocation: string | null;
}

interface SnapshotCountsV1 {
  readonly totalNodes: number;
  readonly scopedNodes: number;
  readonly includedNodes: number;
  readonly omittedNodes: number;
  readonly omittedNodesByScope: number;
  readonly omittedNodesByLimit: number;
  readonly totalEdges: number;
  readonly validEdges: number;
  readonly scopedEdges: number;
  readonly eligibleEdges: number;
  readonly includedEdges: number;
  readonly omittedEdges: number;
  readonly invalidEndpointEdges: number;
  readonly omittedEdgesByScope: number;
  readonly omittedEdgesByNodeLimit: number;
  readonly omittedEdgesByEdgeLimit: number;
  readonly selectedIncidentEdges: number;
  readonly includedSelectedIncidentEdges: number;
}

type VisualizerScopeV1 =
  | { readonly kind: 'all' }
  | { readonly kind: 'community'; readonly id: string | null };

interface VisualizerCommunityFacetV1 {
  readonly id: string | null;
  readonly name: string | null;
  readonly names: readonly string[];
  readonly nodeCount: number;
}

interface VisualizerValueFacetV1<T extends string | null> {
  readonly value: T;
  readonly count: number;
}

interface VisualizerSnapshotV1 {
  readonly contractVersion: 1;
  readonly directed: boolean;
  readonly builtAtCommit: string | null;
  readonly scope: VisualizerScopeV1;
  readonly selectedNodeId: string | null;
  readonly limits: { readonly nodes: number; readonly edges: number };
  readonly counts: SnapshotCountsV1;
  readonly nodes: readonly VisualizerNodeV1[];
  readonly edges: readonly VisualizerEdgeV1[];
  readonly communities: readonly VisualizerCommunityFacetV1[];
  readonly relations: readonly VisualizerValueFacetV1<string>[];
  readonly confidences: readonly VisualizerValueFacetV1<string | null>[];
}

interface PersistedState {
  readonly version: 2;
  readonly mode: GraphMode;
  readonly selectedId: string | null;
  readonly focusId: string | null;
  readonly keyboardId: string | null;
  readonly query: string;
  readonly communityFilter: string | null;
  readonly communityFilterUnassigned: boolean;
  readonly relationFilter: string | null;
  readonly density: Density;
  readonly traceActive: boolean;
  readonly scale: number;
  readonly offsetX: number;
  readonly offsetY: number;
  readonly cameraInitialized: boolean;
  readonly history: readonly string[];
  readonly historyIndex: number;
  readonly expandedIncoming: boolean;
  readonly expandedOutgoing: boolean;
  readonly layoutFingerprint: string | null;
}

interface RendererState {
  readonly mode: GraphMode;
  readonly selectedId: string | null;
  readonly focusId: string | null;
  readonly query: string;
  readonly communityFilter: string | null;
  readonly relationFilter: string | null;
  readonly visibleNodes: number;
  readonly visibleEdges: number;
  readonly traceActive: boolean;
}

type ClientMessage =
  | { readonly type: 'ready' }
  | { readonly type: 'reveal' | 'explain'; readonly id: string }
  | { readonly type: 'rendererState'; readonly state: RendererState }
  | { readonly type: 'geometryDiagnostics'; readonly diagnostics: GeometryDiagnostics };

interface Point {
  readonly x: number;
  readonly y: number;
}

interface ScreenRect {
  readonly left: number;
  readonly top: number;
  readonly right: number;
  readonly bottom: number;
}

interface GlyphRect extends ScreenRect {
  readonly id: string;
}

interface GeometryGlyph extends ScreenRect {
  readonly nodeIndex: number;
  readonly emphasized: boolean;
}

interface GeometryLabel extends ScreenRect {
  readonly kind: 'community' | 'node';
  readonly itemIndex: number;
}

interface GeometryDiagnostics {
  readonly viewport: { readonly width: number; readonly height: number; readonly dpr: number };
  readonly scale: number;
  readonly fittedScale: number;
  readonly glyphs: readonly GeometryGlyph[];
  readonly labels: readonly GeometryLabel[];
  readonly visibleNodes: number;
  readonly visibleEdges: number;
  readonly positions: number;
  readonly spatialCells: number;
  readonly layoutMilliseconds: number;
  readonly drawMilliseconds: number;
}

interface DrawGeometry {
  readonly glyphs: readonly GlyphRect[];
  readonly labels: readonly GeometryLabel[];
}

interface MutablePoint {
  x: number;
  y: number;
}

interface CommunityLayout {
  readonly id: string | null;
  readonly name: string;
  readonly color: string;
  readonly center: Point;
  readonly hull: readonly Point[];
  readonly radius: number;
  readonly nodeCount: number;
}

interface CommunityLayoutDraft {
  readonly id: string | null;
  readonly name: string;
  readonly color: string;
  readonly nodes: readonly VisualizerNodeV1[];
  readonly localPositions: ReadonlyMap<string, Point>;
  readonly radius: number;
}

interface PackedCommunity {
  readonly center: Point;
  readonly radius: number;
}

interface LensEntry {
  readonly node: VisualizerNodeV1;
  readonly edges: readonly VisualizerEdgeV1[];
  readonly direction: 'incoming' | 'outgoing';
}

interface RenderCache {
  nodes: readonly VisualizerNodeV1[];
  edges: readonly VisualizerEdgeV1[];
  edgeEntries: readonly VisualizerEdgeV1[];
}

interface UiElements {
  readonly root: HTMLElement;
  readonly shell: HTMLElement;
  readonly search: HTMLInputElement;
  readonly searchResults: HTMLElement;
  readonly globalMode: HTMLButtonElement;
  readonly focusMode: HTMLButtonElement;
  readonly community: HTMLSelectElement;
  readonly relation: HTMLSelectElement;
  readonly densityButtons: Readonly<Record<Density, HTMLButtonElement>>;
  readonly trace: HTMLButtonElement;
  readonly reset: HTMLButtonElement;
  readonly canvas: HTMLCanvasElement;
  readonly canvasProxy: HTMLElement;
  readonly stage: HTMLElement;
  readonly stageTitle: HTMLElement;
  readonly globalSurface: HTMLElement;
  readonly lensSurface: HTMLElement;
  readonly lensHeading: HTMLElement;
  readonly incomingColumn: HTMLElement;
  readonly focusColumn: HTMLElement;
  readonly outgoingColumn: HTMLElement;
  readonly history: HTMLElement;
  readonly historyBack: HTMLButtonElement;
  readonly historyForward: HTMLButtonElement;
  readonly inspector: HTMLElement;
  readonly status: HTMLElement;
  readonly statusTitle: HTMLElement;
  readonly statusMessage: HTMLElement;
  readonly stats: HTMLElement;
  readonly footerHelp: HTMLElement;
  readonly legend: HTMLElement;
  readonly zoomIn: HTMLButtonElement;
  readonly zoomOut: HTMLButtonElement;
  readonly fit: HTMLButtonElement;
  readonly announcer: HTMLElement;
}

const MAX_NODES = 5_000;
const MAX_EDGES = 12_000;
const MIN_NODES = 25;
const MIN_EDGES = 2_800;
// Mirrored from visualizer-model.ts because this classic browser bundle cannot import host code.
const MAX_STRING_CODE_UNITS = 16_384;
const MAX_SNAPSHOT_STRING_CODE_UNITS = 8_000_000;
const MAX_SEARCH_RESULTS = 20;
const MAX_LABELS = 120;
const MAX_FILTER_OPTIONS = 500;
const MAX_HISTORY = 20;
const LENS_INITIAL_PER_SIDE = 6;
const LENS_MAX_PER_SIDE = 72;
const EDGE_BUDGETS: Readonly<Record<Density, number>> = Object.freeze({ focus: 260, balanced: 1_200, complete: 2_800 });
const UNASSIGNED_LAYOUT_SEED = '\u0000unassigned';
const POSITION_CELL = 180;
const NODE_SPACING_WORLD = 72;
const COMMUNITY_PADDING_WORLD = 58;
const COMMUNITY_GAP_WORLD = 34;
const COMMUNITY_SPIRAL_STEP = 106;
const COMMUNITY_PACK_CELL = 256;
const MIN_CAMERA_SCALE = 0.02;
const COMMUNITY_COLORS = Object.freeze(['#8B5CF6', '#7BC3E8', '#55C8BE', '#C9B8FF', '#6D8FE8', '#D19A66', '#D274A7', '#77B879']);

const defaultState: PersistedState = Object.freeze({
  version: 2,
  mode: 'global',
  selectedId: null,
  focusId: null,
  keyboardId: null,
  query: '',
  communityFilter: null,
  communityFilterUnassigned: false,
  relationFilter: null,
  density: 'balanced',
  traceActive: false,
  scale: 1,
  offsetX: 0,
  offsetY: 0,
  cameraInitialized: false,
  history: [],
  historyIndex: -1,
  expandedIncoming: false,
  expandedOutgoing: false,
  layoutFingerprint: null,
});

const vscode = acquireVsCodeApi();
const root = document.getElementById('graphoxide-root');
if (!(root instanceof HTMLElement)) throw new Error('Graphoxide webview root is missing.');
const testMode = root.dataset.testMode === 'true';
const ui = buildShell(root);
const contextCandidate = ui.canvas.getContext('2d', { alpha: true });
if (!contextCandidate) throw new Error('Graphoxide could not initialize the graph canvas.');
const context: CanvasRenderingContext2D = contextCandidate;

let graph: VisualizerSnapshotV1 | null = null;
// The extension host can disable node source links (graphoxide.sourceLinks.enabled).
let sourceLinksEnabled = true;
let communityOptions = new Map<string, { readonly all: boolean; readonly id: string | null }>();
let relationOptions = new Map<string, { readonly all: boolean; readonly relation: string | null }>();
let canonicalCommunityNames = new Map<string | null, string>();
let nodeById = new Map<string, VisualizerNodeV1>();
let nodeIndexById = new Map<string, number>();
let incoming = new Map<string, VisualizerEdgeV1[]>();
let outgoing = new Map<string, VisualizerEdgeV1[]>();
let positions = new Map<string, Point>();
let spatialGrid = new Map<string, VisualizerNodeV1[]>();
let nodeClearances = new Map<string, number>();
let communityLayouts: readonly CommunityLayout[] = [];
let communityColors = new Map<string | null, string>();
let renderCache: RenderCache = { nodes: [], edges: [], edgeEntries: [] };
let state = sanitizePersistedState(vscode.getState());
let drawFrame: number | null = null;
let lastAnimationFrame = 0;
let persistenceTimer: number | null = null;
let searchIndex = -1;
let searchOpen = false;
let pointer: { id: number; startX: number; startY: number; lastX: number; lastY: number; moved: boolean } | null = null;
let reducedMotion = globalThis.matchMedia('(prefers-reduced-motion: reduce)').matches;
let forcedColors = globalThis.matchMedia('(forced-colors: active)').matches;
let lastLayoutMilliseconds = 0;

bindEvents();
setHostStatus('loading', 'Loading graph', 'Waiting for the workspace graph…');
vscode.postMessage({ type: 'ready' });

function buildShell(host: HTMLElement): UiElements {
  host.replaceChildren();
  const skip = make('a', 'gx-skip-link', 'Skip to graph');
  skip.setAttribute('href', '#gx-graph-stage');
  host.append(skip);

  const shell = make('div', 'gx-shell');
  host.append(shell);
  const commandBar = make('header', 'gx-command-bar');
  shell.append(commandBar);

  const brand = make('div', 'gx-brand');
  brand.setAttribute('aria-label', 'Graphoxide graph explorer');
  const brandMark = make('span', 'gx-brand-mark');
  brandMark.setAttribute('aria-hidden', 'true');
  for (let index = 0; index < 4; index += 1) brandMark.append(make('i'));
  const brandCopy = make('span', 'gx-brand-copy');
  brandCopy.append(make('strong', '', 'Graphoxide'), make('small', '', 'Cinematic explorer'));
  brand.append(brandMark, brandCopy);
  commandBar.append(brand);

  const searchWrap = make('div', 'gx-search-wrap');
  searchWrap.append(make('span', 'gx-search-icon', '⌕'));
  const search = document.createElement('input');
  search.id = 'gx-search';
  search.type = 'search';
  search.autocomplete = 'off';
  search.spellcheck = false;
  search.placeholder = 'Find a symbol, file, or domain…';
  search.setAttribute('aria-label', 'Search graph');
  search.setAttribute('role', 'combobox');
  search.setAttribute('aria-autocomplete', 'list');
  search.setAttribute('aria-controls', 'gx-search-results');
  search.setAttribute('aria-expanded', 'false');
  const searchHint = make('kbd', '', '/');
  const searchResults = make('div', 'gx-search-results');
  searchResults.id = 'gx-search-results';
  searchResults.setAttribute('role', 'listbox');
  searchResults.setAttribute('aria-label', 'Graph search results');
  searchWrap.append(search, searchHint, searchResults);
  commandBar.append(searchWrap);

  const modeSwitch = make('div', 'gx-mode-switch');
  modeSwitch.setAttribute('role', 'group');
  modeSwitch.setAttribute('aria-label', 'Graph perspective');
  const globalMode = button('gx-mode-global', 'Constellation');
  const focusMode = button('gx-mode-focus', 'Investigation Lens');
  modeSwitch.append(globalMode, focusMode);
  commandBar.append(modeSwitch);

  const body = make('div', 'gx-body');
  shell.append(body);
  const navigation = make('aside', 'gx-navigation');
  navigation.setAttribute('aria-label', 'Graph filters');
  body.append(navigation);
  navigation.append(make('p', 'gx-eyebrow', 'Architecture map'), make('h1', 'gx-nav-title', 'System constellation'));

  const communityLabel = make('label', 'gx-field-label', 'Domain');
  const community = document.createElement('select');
  community.id = 'gx-community';
  community.setAttribute('aria-label', 'Filter domain');
  communityLabel.append(community);
  navigation.append(communityLabel);

  const relationLabel = make('label', 'gx-field-label', 'Relationship');
  const relation = document.createElement('select');
  relation.id = 'gx-relation';
  relation.setAttribute('aria-label', 'Filter relationship');
  relationLabel.append(relation);
  navigation.append(relationLabel);

  navigation.append(make('p', 'gx-field-heading', 'Signal density'));
  const densityGroup = make('div', 'gx-density-group');
  densityGroup.setAttribute('role', 'group');
  densityGroup.setAttribute('aria-label', 'Graph edge density');
  const densityButtons: Record<Density, HTMLButtonElement> = {
    focus: button('gx-density-focus', 'Focus'),
    balanced: button('gx-density-balanced', 'Balanced'),
    complete: button('gx-density-complete', 'Dense'),
  };
  densityGroup.append(densityButtons.focus, densityButtons.balanced, densityButtons.complete);
  navigation.append(densityGroup);

  const trace = button('gx-trace', 'Trace incoming relationships');
  trace.className = 'gx-trace-button';
  trace.setAttribute('aria-pressed', 'false');
  trace.disabled = true;
  const reset = button('gx-reset', 'Reset view');
  reset.className = 'gx-secondary-button';
  navigation.append(trace, reset);

  const legend = make('section', 'gx-semantic-legend');
  legend.setAttribute('aria-label', 'Graph encoding legend');
  navigation.append(legend);
  renderFixedLegend(legend);

  const stage = make('section', 'gx-stage');
  stage.id = 'gx-graph-stage';
  stage.tabIndex = -1;
  stage.setAttribute('aria-label', 'Graph workspace');
  body.append(stage);
  const stageBar = make('div', 'gx-stage-bar');
  stage.append(stageBar);
  const stageCopy = make('div');
  const stageTitle = make('h2', 'gx-stage-title', 'All domains');
  stageCopy.append(make('p', 'gx-eyebrow', 'Live workspace data'), stageTitle);
  stageBar.append(stageCopy);
  const history = make('nav', 'gx-history');
  history.setAttribute('aria-label', 'Investigation history');
  const historyBack = button('gx-history-back', '←');
  historyBack.title = 'Previous focus';
  const historyTrail = make('div', 'gx-history-trail');
  const historyForward = button('gx-history-forward', '→');
  historyForward.title = 'Next focus';
  history.append(historyBack, historyTrail, historyForward);
  stageBar.append(history);

  const globalSurface = make('section', 'gx-global-surface');
  globalSurface.setAttribute('aria-label', 'Constellation graph');
  stage.append(globalSurface);
  const canvas = document.createElement('canvas');
  canvas.id = 'gx-graph-canvas';
  canvas.tabIndex = 0;
  canvas.setAttribute('role', 'application');
  canvas.setAttribute('aria-label', 'Interactive code knowledge graph');
  canvas.setAttribute('aria-describedby', 'gx-canvas-help');
  canvas.setAttribute('aria-activedescendant', 'gx-canvas-active');
  canvas.setAttribute('aria-owns', 'gx-canvas-active');
  globalSurface.append(canvas);
  const canvasProxy = make('span', 'gx-visually-hidden');
  canvasProxy.id = 'gx-canvas-active';
  canvasProxy.setAttribute('role', 'option');
  canvasProxy.textContent = 'No active symbol';
  globalSurface.append(canvasProxy);
  const canvasHelp = make('p', 'gx-visually-hidden', 'Use arrow keys to move between symbols, Enter to inspect, T to trace incoming relationships, and Escape to clear the selection.');
  canvasHelp.id = 'gx-canvas-help';
  globalSurface.append(canvasHelp);
  const viewport = make('div', 'gx-viewport-tools');
  const zoomIn = button('gx-zoom-in', '+');
  zoomIn.setAttribute('aria-label', 'Zoom in');
  const zoomOut = button('gx-zoom-out', '−');
  zoomOut.setAttribute('aria-label', 'Zoom out');
  const fit = button('gx-fit', 'Fit');
  fit.setAttribute('aria-label', 'Fit graph to view');
  viewport.append(zoomIn, zoomOut, fit);
  globalSurface.append(viewport);

  const lensSurface = make('section', 'gx-lens-surface');
  lensSurface.hidden = true;
  lensSurface.setAttribute('aria-label', 'Investigation Lens');
  stage.append(lensSurface);
  const lensHeading = make('div', 'gx-lens-heading');
  lensSurface.append(lensHeading);
  const lensFlow = make('div', 'gx-lens-flow');
  lensSurface.append(lensFlow);
  const incomingColumn = make('section', 'gx-lens-column gx-lens-incoming');
  incomingColumn.setAttribute('aria-label', 'Incoming relationships');
  const focusColumn = make('section', 'gx-lens-column gx-lens-focus');
  focusColumn.setAttribute('aria-label', 'Current investigation focus');
  const outgoingColumn = make('section', 'gx-lens-column gx-lens-outgoing');
  outgoingColumn.setAttribute('aria-label', 'Outgoing relationships');
  lensFlow.append(incomingColumn, focusColumn, outgoingColumn);

  const footer = make('footer', 'gx-stage-footer');
  const stats = make('span', 'gx-stats');
  stats.setAttribute('aria-live', 'polite');
  const footerHelp = make('span', 'gx-footer-help', 'Scroll to zoom · drag to pan · Enter to inspect');
  footer.append(stats, footerHelp);
  stage.append(footer);

  const status = make('section', 'gx-status');
  status.setAttribute('aria-live', 'polite');
  const statusOrb = make('span', 'gx-status-orb');
  statusOrb.setAttribute('aria-hidden', 'true');
  const statusTitle = make('strong', '', 'Loading graph');
  const statusMessage = make('p', '', 'Waiting for the workspace graph…');
  status.append(statusOrb, statusTitle, statusMessage);
  stage.append(status);

  const inspector = make('aside', 'gx-inspector');
  inspector.setAttribute('aria-label', 'Node inspector');
  inspector.setAttribute('aria-live', 'polite');
  body.append(inspector);

  const announcer = make('div', 'gx-visually-hidden');
  announcer.setAttribute('aria-live', 'polite');
  host.append(announcer);

  return {
    root: host,
    shell,
    search,
    searchResults,
    globalMode,
    focusMode,
    community,
    relation,
    densityButtons,
    trace,
    reset,
    canvas,
    canvasProxy,
    stage,
    stageTitle,
    globalSurface,
    lensSurface,
    lensHeading,
    incomingColumn,
    focusColumn,
    outgoingColumn,
    history: historyTrail,
    historyBack,
    historyForward,
    inspector,
    status,
    statusTitle,
    statusMessage,
    stats,
    footerHelp,
    legend,
    zoomIn,
    zoomOut,
    fit,
    announcer,
  };
}

function make<K extends keyof HTMLElementTagNameMap>(tag: K, className = '', text = ''): HTMLElementTagNameMap[K] {
  const element = document.createElement(tag);
  if (className) element.className = className;
  if (text) element.textContent = text;
  return element;
}

function button(id: string, label: string): HTMLButtonElement {
  const element = document.createElement('button');
  element.type = 'button';
  element.id = id;
  element.textContent = label;
  return element;
}

function renderFixedLegend(container: HTMLElement): void {
  container.replaceChildren(make('p', 'gx-field-heading', 'Confidence'));
  const items: readonly [string, string, string][] = [
    ['gx-confidence-swatch gx-solid', '✓', 'Extracted'],
    ['gx-confidence-swatch gx-dashed', '≈', 'Inferred'],
    ['gx-confidence-swatch gx-dotted', '?', 'Ambiguous'],
    ['gx-confidence-swatch gx-unknown', '·', 'Unspecified'],
  ];
  for (const [className, glyph, label] of items) {
    const row = make('span', 'gx-legend-row');
    const swatch = make('i', className, glyph);
    swatch.setAttribute('aria-hidden', 'true');
    row.append(swatch, document.createTextNode(label));
    container.append(row);
  }
  const direction = make('span', 'gx-legend-row');
  const arrow = make('i', 'gx-direction-swatch', '→');
  arrow.setAttribute('aria-hidden', 'true');
  direction.append(arrow, document.createTextNode('Recorded source → target'));
  container.append(direction);
}

function bindEvents(): void {
  globalThis.addEventListener('message', handleHostMessageEvent);
  globalThis.addEventListener('resize', resizeCanvas);
  const motionQuery = globalThis.matchMedia('(prefers-reduced-motion: reduce)');
  motionQuery.addEventListener('change', (event) => {
    reducedMotion = event.matches;
    requestDraw();
  });
  const colorsQuery = globalThis.matchMedia('(forced-colors: active)');
  colorsQuery.addEventListener('change', (event) => {
    forcedColors = event.matches;
    requestDraw();
  });

  ui.search.value = state.query;
  ui.search.addEventListener('input', () => {
    searchOpen = true;
    state = { ...state, query: boundedText(ui.search.value, 256) };
    searchIndex = -1;
    renderSearchResults();
    persistSoon();
    emitRendererState();
  });
  ui.search.addEventListener('focus', () => {
    searchOpen = true;
    renderSearchResults();
  });
  ui.search.addEventListener('keydown', handleSearchKeydown);
  ui.search.parentElement?.addEventListener('focusout', (event) => {
    const next = event.relatedTarget;
    if (!(next instanceof Node) || !ui.search.parentElement?.contains(next)) closeSearchResults();
  });
  document.addEventListener('pointerdown', (event) => {
    const target = event.target;
    if (!(target instanceof Node) || !ui.searchResults.contains(target) && target !== ui.search) closeSearchResults();
  });
  document.addEventListener('keydown', (event) => {
    const target = event.target;
    const editing = target instanceof HTMLInputElement || target instanceof HTMLSelectElement || target instanceof HTMLTextAreaElement;
    if (event.key === '/' && !editing) {
      event.preventDefault();
      ui.search.focus();
    } else if ((event.key === 't' || event.key === 'T') && !editing) {
      event.preventDefault();
      toggleTrace();
    } else if (event.key === 'Escape' && !editing) {
      if (state.traceActive) setTrace(false);
      else selectNode(null);
    }
  });

  ui.globalMode.addEventListener('click', () => setMode('global'));
  ui.focusMode.addEventListener('click', () => setMode('focus'));
  ui.community.addEventListener('change', () => {
    const choice = communityOptions.get(ui.community.value);
    if (!choice) return;
    state = {
      ...state,
      communityFilter: choice.all ? null : choice.id,
      communityFilterUnassigned: !choice.all && choice.id === null,
      traceActive: false,
    };
    if (sanitizeViewIdsForFilters()) announce('Hidden investigation targets were cleared for the selected domain.');
    updateView();
  });
  ui.relation.addEventListener('change', () => {
    const choice = relationOptions.get(ui.relation.value);
    if (!choice) return;
    state = { ...state, relationFilter: choice.all ? null : choice.relation, traceActive: false };
    if (sanitizeViewIdsForFilters()) announce('Hidden investigation targets were cleared for the selected relationship.');
    updateView();
  });
  for (const density of ['focus', 'balanced', 'complete'] as const) {
    ui.densityButtons[density].addEventListener('click', () => {
      state = { ...state, density };
      updateControlStates();
      recomputeRenderEdges();
      renderStats();
      requestDraw();
      persist();
      emitRendererState();
    });
  }
  ui.trace.addEventListener('click', toggleTrace);
  ui.reset.addEventListener('click', resetView);
  ui.zoomIn.addEventListener('click', () => zoomAt(1.22));
  ui.zoomOut.addEventListener('click', () => zoomAt(1 / 1.22));
  ui.fit.addEventListener('click', () => fitView(true));
  ui.historyBack.addEventListener('click', () => moveHistory(state.historyIndex - 1));
  ui.historyForward.addEventListener('click', () => moveHistory(state.historyIndex + 1));

  ui.canvas.addEventListener('pointerdown', handlePointerDown);
  ui.canvas.addEventListener('pointermove', handlePointerMove);
  ui.canvas.addEventListener('pointerup', handlePointerEnd);
  ui.canvas.addEventListener('pointercancel', handlePointerEnd);
  ui.canvas.addEventListener('dblclick', (event) => {
    const node = hitTest(event.clientX, event.clientY);
    if (node) revealNode(node.id);
  });
  ui.canvas.addEventListener('wheel', (event) => {
    event.preventDefault();
    const bounds = ui.canvas.getBoundingClientRect();
    zoomAt(Math.exp(-event.deltaY * 0.001), event.clientX - bounds.left, event.clientY - bounds.top);
    persistSoon();
  }, { passive: false });
  ui.canvas.addEventListener('keydown', handleCanvasKeydown);
  const resizeObserver = new ResizeObserver(resizeCanvas);
  resizeObserver.observe(ui.canvas);
}

function handleHostMessageEvent(event: MessageEvent<unknown>): void {
  // VS Code forwards extension messages from its same-origin outer webview.
  // Require the sender origin to match this webview document.
  if (event.origin !== globalThis.origin) return;
  handleHostMessage(event.data);
}

function handleHostMessage(value: unknown): void {
  if (!isRecord(value) || typeof value.type !== 'string') return;
  if (value.type === 'replaceGraph') {
    const snapshot = parseSnapshot(value.graph);
    if (!snapshot) {
      clearGraphForStatus();
      setHostStatus('error', 'Graph could not be displayed', 'The extension supplied an incompatible bounded graph snapshot.');
      emitRendererState();
      return;
    }
    installGraph(snapshot);
    return;
  }
  if (value.type === 'sourceLinks' && typeof value.enabled === 'boolean') {
    if (value.enabled === sourceLinksEnabled) return;
    sourceLinksEnabled = value.enabled;
    updateView();
    return;
  }
  if (value.type === 'status' && (value.status === 'loading' || value.status === 'error')) {
    const message = typeof value.message === 'string' ? boundedText(value.message, 800) : '';
    clearGraphForStatus();
    if (value.status === 'loading') setHostStatus('loading', 'Loading graph', message || 'Resolving the workspace graph…');
    else setHostStatus('error', 'Graph could not be loaded', message || 'Rebuild or refresh the workspace graph, then try again.');
    emitRendererState();
    return;
  }
  if (value.type === 'testAction' && testMode && isTestAction(value.action)) {
    handleTestAction(value.action, value.value);
  }
}

function handleTestAction(action: TestAction, value: unknown): void {
  if (action === 'select-first') {
    const first = [...renderCache.nodes].sort(compareNodes)[0];
    if (first) selectNode(first.id, true);
  } else if (action === 'enter-focus') {
    if (state.selectedId) setFocus(state.selectedId);
  } else if (action === 'toggle-trace') {
    toggleTrace();
  } else if (action === 'return-global') {
    setMode('global');
  } else if (action === 'set-query') {
    if (typeof value === 'string' && value.length <= 256) {
      ui.search.value = value;
      state = { ...state, query: value };
      renderSearchResults();
      persist();
    }
  } else if (action === 'reveal-selected') {
    if (state.selectedId) revealNode(state.selectedId);
  }
  emitRendererState();
}

function installGraph(snapshot: VisualizerSnapshotV1): void {
  graph = snapshot;
  canonicalCommunityNames = new Map(snapshot.communities.map((facet) => [facet.id, domainDisplayName(facet.id, facet.name)]));
  nodeById = new Map(snapshot.nodes.map((node) => [node.id, node]));
  nodeIndexById = new Map(snapshot.nodes.map((node, index) => [node.id, index]));
  incoming = new Map(snapshot.nodes.map((node) => [node.id, []]));
  outgoing = new Map(snapshot.nodes.map((node) => [node.id, []]));
  for (const edge of snapshot.edges) {
    incoming.get(edge.target)?.push(edge);
    outgoing.get(edge.source)?.push(edge);
  }
  for (const values of incoming.values()) values.sort(compareEdges);
  for (const values of outgoing.values()) values.sort(compareEdges);

  state = sanitizeStateForGraph(state, snapshot);
  const fingerprint = graphLayoutFingerprint(snapshot.nodes);
  if (state.layoutFingerprint !== fingerprint) {
    state = {
      ...state,
      scale: 1,
      offsetX: 0,
      offsetY: 0,
      cameraInitialized: false,
      layoutFingerprint: fingerprint,
    };
  }
  if (state.selectedId === null && snapshot.selectedNodeId !== null) {
    state = { ...state, selectedId: snapshot.selectedNodeId, keyboardId: snapshot.selectedNodeId };
  }
  if (state.focusId === null && state.selectedId !== null) state = { ...state, focusId: state.selectedId };
  sanitizeViewIdsForFilters();
  const layoutStarted = performance.now();
  buildLayout();
  lastLayoutMilliseconds = performance.now() - layoutStarted;
  populateFilters();
  updateControlStates();
  updateView();
  requestAnimationFrame(() => {
    resizeCanvas();
    const bounds = ui.canvas.getBoundingClientRect();
    if (state.mode === 'global' && !state.cameraInitialized && snapshot.nodes.length > 0 && bounds.width > 0 && bounds.height > 0) fitView(true);
  });
  persist();
  emitRendererState();
}

function clearGraphForStatus(): void {
  graph = null;
  nodeById = new Map();
  nodeIndexById = new Map();
  incoming = new Map();
  outgoing = new Map();
  positions = new Map();
  spatialGrid = new Map();
  nodeClearances = new Map();
  communityLayouts = [];
  communityColors = new Map();
  canonicalCommunityNames = new Map();
  renderCache = { nodes: [], edges: [], edgeEntries: [] };
  state = {
    ...state,
    mode: 'global',
    selectedId: null,
    focusId: null,
    keyboardId: null,
    query: '',
    communityFilter: null,
    communityFilterUnassigned: false,
    relationFilter: null,
    traceActive: false,
    scale: 1,
    offsetX: 0,
    offsetY: 0,
    cameraInitialized: false,
    history: [],
    historyIndex: -1,
    expandedIncoming: false,
    expandedOutgoing: false,
  };
  ui.search.value = '';
  closeSearchResults();
  communityOptions = new Map([['community-all', { all: true, id: null }]]);
  relationOptions = new Map([['relation-all', { all: true, relation: null }]]);
  ui.community.replaceChildren(option('community-all', 'All domains'));
  ui.relation.replaceChildren(option('relation-all', 'All relationships'));
  ui.incomingColumn.replaceChildren();
  ui.focusColumn.replaceChildren();
  ui.outgoingColumn.replaceChildren();
  ui.lensHeading.replaceChildren();
  ui.inspector.replaceChildren();
  ui.inspector.classList.add('is-empty');
  ui.history.replaceChildren();
  ui.stageTitle.textContent = 'All domains';
  ui.stats.textContent = '';
  ui.footerHelp.textContent = 'Scroll to zoom · drag to pan · Enter to inspect';
  updateControlStates();
  requestDraw();
  persist();
}

function parseSnapshot(value: unknown): VisualizerSnapshotV1 | null {
  if (!isRecord(value)
    || value.contractVersion !== 1
    || typeof value.directed !== 'boolean'
    || !isNullableBoundedString(value.builtAtCommit, MAX_STRING_CODE_UNITS)
    || !isVisualizerScope(value.scope)
    || !isNullableBoundedString(value.selectedNodeId, MAX_STRING_CODE_UNITS)
    || !isRecord(value.limits)
    || !isCount(value.limits.nodes)
    || !isCount(value.limits.edges)
    || !isRecord(value.counts)
    || !hasValidSnapshotCounts(value.counts)
    || !Array.isArray(value.nodes)
    || value.nodes.length > MAX_NODES
    || !value.nodes.every(isNode)
    || !Array.isArray(value.edges)
    || value.edges.length > MAX_EDGES
    || !value.edges.every(isEdge)
    || !Array.isArray(value.communities)
    || value.communities.length > MAX_NODES
    || !value.communities.every(isCommunityFacet)
    || !Array.isArray(value.relations)
    || value.relations.length > MAX_EDGES
    || !value.relations.every((facet) => isValueFacet(facet, false))
    || !Array.isArray(value.confidences)
    || value.confidences.length > MAX_EDGES
    || !value.confidences.every((facet) => isValueFacet(facet, true))) return null;

  const snapshot = value as unknown as VisualizerSnapshotV1;
  const counts = snapshot.counts;
  if (counts.includedNodes !== snapshot.nodes.length
    || counts.includedEdges !== snapshot.edges.length
    || counts.omittedNodes !== counts.totalNodes - counts.includedNodes
    || counts.omittedNodesByScope + counts.omittedNodesByLimit !== counts.omittedNodes
    || counts.invalidEndpointEdges + counts.omittedEdgesByScope + counts.omittedEdgesByNodeLimit
      + counts.omittedEdgesByEdgeLimit + counts.includedEdges !== counts.totalEdges
    || counts.omittedEdges !== counts.totalEdges - counts.includedEdges
    || counts.includedNodes > counts.scopedNodes
    || counts.scopedNodes > counts.totalNodes
    || counts.includedEdges > counts.eligibleEdges
    || counts.eligibleEdges > counts.scopedEdges
    || counts.scopedEdges > counts.validEdges
    || counts.validEdges > counts.totalEdges
    || counts.includedSelectedIncidentEdges > counts.selectedIncidentEdges
    || counts.selectedIncidentEdges > counts.scopedEdges
    || snapshot.limits.nodes < MIN_NODES
    || snapshot.limits.nodes > MAX_NODES
    || snapshot.limits.edges < MIN_EDGES
    || snapshot.limits.edges > MAX_EDGES
    || snapshot.nodes.length > snapshot.limits.nodes
    || snapshot.edges.length > snapshot.limits.edges
    || snapshot.limits.edges !== visualizerEdgeLimit(snapshot.nodes.length)
    || snapshot.communities.length > snapshot.nodes.length
    || snapshot.communities.reduce((count, facet) => count + facet.names.length, 0) > snapshot.nodes.length
    || snapshot.relations.length > snapshot.edges.length
    || snapshot.confidences.length > snapshot.edges.length
    || !snapshotStringsWithinBudget(snapshot)) return null;

  const ids = new Set<string>();
  for (const node of snapshot.nodes) {
    if (ids.has(node.id)) return null;
    ids.add(node.id);
  }
  for (const edge of snapshot.edges) {
    if (!ids.has(edge.source) || !ids.has(edge.target)) return null;
  }
  if (snapshot.selectedNodeId !== null && !ids.has(snapshot.selectedNodeId)) return null;
  return snapshot;
}

function sanitizePersistedState(value: unknown): PersistedState {
  if (!isRecord(value) || (value.version !== 1 && value.version !== 2)) return defaultState;
  const retainCamera = value.version === 2;
  const history = Array.isArray(value.history)
    ? value.history.filter((entry): entry is string => isBoundedString(entry, 16_384)).slice(-MAX_HISTORY)
    : [];
  const historyIndex = clampInteger(value.historyIndex, -1, history.length - 1, history.length - 1);
  return {
    version: 2,
    mode: value.mode === 'focus' ? 'focus' : 'global',
    selectedId: nullableBoundedString(value.selectedId, 16_384),
    focusId: nullableBoundedString(value.focusId, 16_384),
    keyboardId: nullableBoundedString(value.keyboardId, 16_384),
    query: typeof value.query === 'string' ? boundedText(value.query, 256) : '',
    communityFilter: nullableBoundedString(value.communityFilter, MAX_STRING_CODE_UNITS),
    communityFilterUnassigned: value.communityFilterUnassigned === true,
    relationFilter: nullableBoundedString(value.relationFilter, MAX_STRING_CODE_UNITS),
    density: isDensity(value.density) ? value.density : 'balanced',
    traceActive: value.traceActive === true,
    scale: retainCamera ? clampNumber(value.scale, MIN_CAMERA_SCALE, 5, 1) : 1,
    offsetX: retainCamera ? clampNumber(value.offsetX, -10_000_000, 10_000_000, 0) : 0,
    offsetY: retainCamera ? clampNumber(value.offsetY, -10_000_000, 10_000_000, 0) : 0,
    cameraInitialized: retainCamera && value.cameraInitialized === true,
    history,
    historyIndex,
    expandedIncoming: value.expandedIncoming === true,
    expandedOutgoing: value.expandedOutgoing === true,
    layoutFingerprint: retainCamera && typeof value.layoutFingerprint === 'string' && value.layoutFingerprint.length <= 32
      ? value.layoutFingerprint
      : null,
  };
}

function sanitizeStateForGraph(current: PersistedState, snapshot: VisualizerSnapshotV1): PersistedState {
  const ids = new Set(snapshot.nodes.map((node) => node.id));
  const relations = new Set(snapshot.edges.map((edge) => edge.relation));
  const communities = new Set(snapshot.nodes.flatMap((node) => node.community === null ? [] : [node.community]));
  const hasUnassigned = snapshot.nodes.some((node) => node.community === null);
  const retainedHistory = current.history
    .map((id, originalIndex) => ({ id, originalIndex }))
    .filter((entry) => ids.has(entry.id))
    .slice(-MAX_HISTORY);
  const history = retainedHistory.map((entry) => entry.id);
  const selectedId = current.selectedId !== null && ids.has(current.selectedId) ? current.selectedId : null;
  const focusId = current.focusId !== null && ids.has(current.focusId) ? current.focusId : selectedId;
  const keyboardId = current.keyboardId !== null && ids.has(current.keyboardId) ? current.keyboardId : selectedId;
  const retainUnassigned = current.communityFilterUnassigned && hasUnassigned;
  const communityFilter = !current.communityFilterUnassigned && current.communityFilter !== null && communities.has(current.communityFilter)
    ? current.communityFilter
    : null;
  const relationFilter = current.relationFilter !== null && relations.has(current.relationFilter) ? current.relationFilter : null;
  const mode: GraphMode = current.mode === 'focus' && focusId !== null ? 'focus' : 'global';
  const retainedHistoryIndex = retainedHistory.findIndex((entry) => entry.originalIndex === current.historyIndex);
  const focusedHistoryIndex = focusId === null ? -1 : history.lastIndexOf(focusId);
  const historyIndex = retainedHistoryIndex >= 0
    ? retainedHistoryIndex
    : focusedHistoryIndex >= 0 ? focusedHistoryIndex : history.length - 1;
  return {
    ...current,
    selectedId,
    focusId,
    keyboardId,
    communityFilter,
    communityFilterUnassigned: retainUnassigned,
    relationFilter,
    mode,
    traceActive: current.traceActive && selectedId !== null && mode === 'global',
    history,
    historyIndex,
  };
}

function persist(): void {
  if (persistenceTimer !== null) {
    globalThis.clearTimeout(persistenceTimer);
    persistenceTimer = null;
  }
  vscode.setState(state);
}

function persistSoon(): void {
  if (persistenceTimer !== null) globalThis.clearTimeout(persistenceTimer);
  persistenceTimer = globalThis.setTimeout(() => persist(), 120);
}

function emitRendererState(): void {
  vscode.postMessage({
    type: 'rendererState',
    state: {
      mode: state.mode,
      selectedId: state.selectedId,
      focusId: state.focusId,
      query: state.query,
      communityFilter: state.communityFilter,
      relationFilter: state.relationFilter,
      visibleNodes: renderCache.nodes.length,
      visibleEdges: renderCache.edges.length,
      traceActive: state.traceActive,
    },
  });
}

function setHostStatus(kind: 'loading' | 'error' | 'empty' | 'ready', title: string, message: string): void {
  ui.status.dataset.kind = kind;
  ui.status.hidden = kind === 'ready';
  ui.statusTitle.textContent = title;
  ui.statusMessage.textContent = message;
  ui.canvas.setAttribute('aria-busy', String(kind === 'loading'));
}

function isNode(value: unknown): value is VisualizerNodeV1 {
  return isRecord(value)
    && isBoundedString(value.id, MAX_STRING_CODE_UNITS)
    && isBoundedString(value.label, MAX_STRING_CODE_UNITS)
    && isBoundedString(value.file, MAX_STRING_CODE_UNITS)
    && isNullableBoundedString(value.location, MAX_STRING_CODE_UNITS)
    && isBoundedString(value.kind, MAX_STRING_CODE_UNITS)
    && isNullableBoundedString(value.community, MAX_STRING_CODE_UNITS)
    && isNullableBoundedString(value.communityName, MAX_STRING_CODE_UNITS)
    && isCount(value.degree)
    && isCount(value.inDegree)
    && isCount(value.outDegree);
}

function isEdge(value: unknown): value is VisualizerEdgeV1 {
  return isRecord(value)
    && isBoundedString(value.source, MAX_STRING_CODE_UNITS)
    && isBoundedString(value.target, MAX_STRING_CODE_UNITS)
    && isBoundedString(value.relation, MAX_STRING_CODE_UNITS)
    && isNullableBoundedString(value.confidence, MAX_STRING_CODE_UNITS)
    && isNullableBoundedString(value.sourceFile, MAX_STRING_CODE_UNITS)
    && isNullableBoundedString(value.sourceLocation, MAX_STRING_CODE_UNITS);
}

function isVisualizerScope(value: unknown): value is VisualizerScopeV1 {
  return isRecord(value)
    && (value.kind === 'all'
      || (value.kind === 'community' && isNullableBoundedString(value.id, MAX_STRING_CODE_UNITS)));
}

function isCommunityFacet(value: unknown): value is VisualizerCommunityFacetV1 {
  return isRecord(value)
    && isNullableBoundedString(value.id, MAX_STRING_CODE_UNITS)
    && isNullableBoundedString(value.name, MAX_STRING_CODE_UNITS)
    && Array.isArray(value.names)
    && value.names.length <= MAX_NODES
    && value.names.every((name) => isBoundedString(name, MAX_STRING_CODE_UNITS))
    && isCount(value.nodeCount);
}

function isValueFacet(value: unknown, nullable: boolean): value is VisualizerValueFacetV1<string | null> {
  return isRecord(value)
    && (isBoundedString(value.value, MAX_STRING_CODE_UNITS) || (nullable && value.value === null))
    && isCount(value.count);
}

function hasValidSnapshotCounts(value: Record<string, unknown>): boolean {
  const keys: readonly (keyof SnapshotCountsV1)[] = [
    'totalNodes',
    'scopedNodes',
    'includedNodes',
    'omittedNodes',
    'omittedNodesByScope',
    'omittedNodesByLimit',
    'totalEdges',
    'validEdges',
    'scopedEdges',
    'eligibleEdges',
    'includedEdges',
    'omittedEdges',
    'invalidEndpointEdges',
    'omittedEdgesByScope',
    'omittedEdgesByNodeLimit',
    'omittedEdgesByEdgeLimit',
    'selectedIncidentEdges',
    'includedSelectedIncidentEdges',
  ];
  return keys.every((key) => isCount(value[key]));
}

function visualizerEdgeLimit(includedNodes: number): number {
  return Math.min(MAX_EDGES, Math.max(MIN_EDGES, includedNodes * 4));
}

function snapshotStringsWithinBudget(snapshot: VisualizerSnapshotV1): boolean {
  let total = 0;
  let valid = true;
  const add = (value: string | null): void => {
    if (value === null || !valid) return;
    if (value.length > MAX_STRING_CODE_UNITS) {
      valid = false;
      return;
    }
    total += value.length;
    if (total > MAX_SNAPSHOT_STRING_CODE_UNITS) valid = false;
  };
  add(snapshot.builtAtCommit);
  add(snapshot.scope.kind);
  if (snapshot.scope.kind === 'community') add(snapshot.scope.id);
  add(snapshot.selectedNodeId);
  for (const node of snapshot.nodes) {
    add(node.id);
    add(node.label);
    add(node.file);
    add(node.location);
    add(node.kind);
    add(node.community);
    add(node.communityName);
  }
  for (const edge of snapshot.edges) {
    add(edge.source);
    add(edge.target);
    add(edge.relation);
    add(edge.confidence);
    add(edge.sourceFile);
    add(edge.sourceLocation);
  }
  for (const community of snapshot.communities) {
    add(community.id);
    add(community.name);
    for (const name of community.names) add(name);
  }
  for (const relation of snapshot.relations) add(relation.value);
  for (const confidence of snapshot.confidences) add(confidence.value);
  return valid;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function isCount(value: unknown): value is number {
  return typeof value === 'number' && Number.isSafeInteger(value) && value >= 0;
}

function isBoundedString(value: unknown, maximum: number): value is string {
  return typeof value === 'string' && value.length <= maximum;
}

function isNullableBoundedString(value: unknown, maximum: number): value is string | null {
  return value === null || isBoundedString(value, maximum);
}

function nullableBoundedString(value: unknown, maximum: number): string | null {
  return isNullableBoundedString(value, maximum) ? value : null;
}

function boundedText(value: string, maximum: number): string {
  return value.length <= maximum ? value : value.slice(0, maximum);
}

function isDensity(value: unknown): value is Density {
  return value === 'focus' || value === 'balanced' || value === 'complete';
}

function isTestAction(value: unknown): value is TestAction {
  return value === 'select-first'
    || value === 'enter-focus'
    || value === 'toggle-trace'
    || value === 'return-global'
    || value === 'set-query'
    || value === 'reveal-selected';
}

function clampNumber(value: unknown, minimum: number, maximum: number, fallback: number): number {
  return typeof value === 'number' && Number.isFinite(value) ? Math.max(minimum, Math.min(maximum, value)) : fallback;
}

function clampInteger(value: unknown, minimum: number, maximum: number, fallback: number): number {
  return typeof value === 'number' && Number.isSafeInteger(value) ? Math.max(minimum, Math.min(maximum, value)) : fallback;
}

function compareText(left: string, right: string): number {
  return left.localeCompare(right, 'en', { sensitivity: 'base', numeric: true });
}

function compareNodes(left: VisualizerNodeV1, right: VisualizerNodeV1): number {
  return right.degree - left.degree || compareText(left.label, right.label) || compareText(left.id, right.id);
}

function compareEdges(left: VisualizerEdgeV1, right: VisualizerEdgeV1): number {
  return compareText(left.relation, right.relation)
    || compareText(left.source, right.source)
    || compareText(left.target, right.target)
    || compareText(left.sourceFile ?? '', right.sourceFile ?? '')
    || compareText(left.sourceLocation ?? '', right.sourceLocation ?? '');
}

function requiredNode(id: string): VisualizerNodeV1 {
  const node = nodeById.get(id);
  if (!node) throw new Error('Graph node is no longer available.');
  return node;
}

function nodePassesCommunity(node: VisualizerNodeV1): boolean {
  if (state.communityFilterUnassigned) return node.community === null;
  if (state.communityFilter === null) return true;
  return node.community === state.communityFilter;
}

function nodePassesActiveFilters(id: string): boolean {
  const node = nodeById.get(id);
  if (!node || !nodePassesCommunity(node)) return false;
  if (state.relationFilter === null) return true;
  return (graph?.edges ?? []).some((edge) => edge.relation === state.relationFilter
    && (edge.source === id || edge.target === id)
    && nodePassesCommunity(requiredNode(edge.source))
    && nodePassesCommunity(requiredNode(edge.target)));
}

function sanitizeViewIdsForFilters(): boolean {
  const selectedId = state.selectedId !== null && nodePassesActiveFilters(state.selectedId) ? state.selectedId : null;
  const focusId = state.focusId !== null && nodePassesActiveFilters(state.focusId) ? state.focusId : null;
  const keyboardId = state.keyboardId !== null && nodePassesActiveFilters(state.keyboardId) ? state.keyboardId : null;
  const changed = selectedId !== state.selectedId || focusId !== state.focusId || keyboardId !== state.keyboardId;
  if (!changed) return false;
  state = {
    ...state,
    mode: state.mode === 'focus' && focusId === null ? 'global' : state.mode,
    selectedId,
    focusId,
    keyboardId,
    traceActive: state.traceActive && selectedId !== null && state.mode === 'global',
  };
  return true;
}

function updateView(): void {
  if (!graph) return;
  let nodes = graph.nodes.filter((node) => nodePassesCommunity(node));
  let nodeIds = new Set(nodes.map((node) => node.id));
  let edges = graph.edges.filter((edge) => nodeIds.has(edge.source)
    && nodeIds.has(edge.target)
    && (state.relationFilter === null || edge.relation === state.relationFilter));
  if (state.relationFilter !== null) {
    const incidentIds = new Set<string>();
    for (const edge of edges) {
      incidentIds.add(edge.source);
      incidentIds.add(edge.target);
    }
    nodes = nodes.filter((node) => incidentIds.has(node.id));
    nodeIds = new Set(nodes.map((node) => node.id));
    edges = edges.filter((edge) => nodeIds.has(edge.source) && nodeIds.has(edge.target));
  }
  renderCache = { nodes, edges, edgeEntries: [] };
  recomputeRenderEdges();
  updateControlStates();
  renderHistory();
  renderLens();
  renderInspector();
  renderSearchResults();
  ui.stageTitle.textContent = activeDomainTitle();
  renderStats();
  if (graph.nodes.length === 0) {
    setHostStatus('empty', 'This graph is empty', 'Build or refresh the workspace graph to add indexed symbols.');
  } else if (nodes.length === 0) {
    setHostStatus('empty', 'No relationships match', 'Change the domain or relationship filter to return to the graph.');
  } else {
    setHostStatus('ready', '', '');
  }
  requestDraw();
  persistSoon();
  emitRendererState();
}

function renderStats(): void {
  if (!graph) {
    ui.stats.textContent = '';
    return;
  }
  const filtered = renderCache.edges.length;
  const drawn = renderCache.edgeEntries.length;
  const focusId = state.focusId ?? state.selectedId;
  const focusRelationships = focusId === null
    ? 0
    : renderCache.edges.reduce((count, edge) => count + Number(edge.source === focusId || edge.target === focusId), 0);
  const relationships = state.mode === 'focus'
    ? `${focusRelationships.toLocaleString()} focus relationships`
    : drawn < filtered
      ? `${drawn.toLocaleString()} of ${filtered.toLocaleString()} relationships drawn`
      : `${drawn.toLocaleString()} relationships drawn`;
  ui.footerHelp.textContent = state.mode === 'focus'
    ? 'Choose a relationship card to move the Lens · Open source from the focus card'
    : 'Scroll to zoom · drag to pan · Enter to inspect';
  const omissions: string[] = [];
  if (graph.counts.omittedNodes > 0) omissions.push(`${graph.counts.omittedNodes.toLocaleString()} symbols omitted by bounded snapshot`);
  if (graph.counts.omittedEdges > 0) omissions.push(`${graph.counts.omittedEdges.toLocaleString()} relationships omitted by bounded snapshot`);
  ui.stats.textContent = `${renderCache.nodes.length.toLocaleString()} symbols · ${relationships}${omissions.length > 0 ? ` · ${omissions.join(' · ')}` : ''}`;
}

function activeDomainTitle(): string {
  if (!graph) return 'Workspace graph';
  if (state.communityFilterUnassigned) return 'Unassigned domain';
  if (state.communityFilter !== null) {
    const facet = graph.communities.find((entry) => entry.id === state.communityFilter);
    return canonicalDomainName(state.communityFilter, facet?.name ?? null);
  }
  if (graph.scope.kind === 'community') {
    const scopeId = graph.scope.id;
    const facet = graph.communities.find((entry) => entry.id === scopeId);
    return canonicalDomainName(scopeId, facet?.name ?? null);
  }
  return 'All domains';
}

function populateFilters(): void {
  if (!graph) return;
  const communityCounts = new Map<string | null, { name: string; count: number }>();
  for (const node of graph.nodes) {
    const key = node.community;
    const entry = communityCounts.get(key);
    if (entry) entry.count += 1;
    else communityCounts.set(key, { name: canonicalDomainName(node.community, node.communityName), count: 1 });
  }
  communityOptions = new Map([['community-all', { all: true, id: null }]]);
  ui.community.replaceChildren(option('community-all', 'All domains'));
  const communities = [...communityCounts].sort((left, right) => right[1].count - left[1].count
    || compareText(left[1].name, right[1].name)
    || compareText(left[0] ?? '', right[0] ?? ''));
  const visibleCommunities = boundedFilterOptions(communities, ([id]) => state.communityFilterUnassigned
    ? id === null
    : state.communityFilter !== null && id === state.communityFilter);
  visibleCommunities.forEach(([id, entry], index) => {
    const token = `community-${index}`;
    communityOptions.set(token, { all: false, id });
    ui.community.append(option(token, `${entry.name} · ${entry.count}`));
  });
  appendOmittedFilterOption(ui.community, communities.length - visibleCommunities.length, 'domains');

  const relationCounts = new Map<string, number>();
  for (const edge of graph.edges) relationCounts.set(edge.relation, (relationCounts.get(edge.relation) ?? 0) + 1);
  relationOptions = new Map([['relation-all', { all: true, relation: null }]]);
  ui.relation.replaceChildren(option('relation-all', 'All relationships'));
  const relations = [...relationCounts].sort((left, right) => right[1] - left[1] || compareText(left[0], right[0]));
  const visibleRelations = boundedFilterOptions(relations, ([relation]) => relation === state.relationFilter);
  visibleRelations.forEach(([relation, count], index) => {
    const token = `relation-${index}`;
    relationOptions.set(token, { all: false, relation });
    ui.relation.append(option(token, `${relation || 'Unspecified relationship'} · ${count}`));
  });
  appendOmittedFilterOption(ui.relation, relations.length - visibleRelations.length, 'relationships');
  updateFilterSelectValues();
}

function boundedFilterOptions<T>(entries: readonly T[], isActive: (entry: T) => boolean): readonly T[] {
  if (entries.length <= MAX_FILTER_OPTIONS) return entries;
  const retained = entries.slice(0, MAX_FILTER_OPTIONS);
  const active = entries.find(isActive);
  if (active && !retained.includes(active)) retained[MAX_FILTER_OPTIONS - 1] = active;
  return retained;
}

function appendOmittedFilterOption(select: HTMLSelectElement, omitted: number, noun: string): void {
  if (omitted <= 0) return;
  const notice = option(`${select.id}-omitted`, `… ${omitted.toLocaleString()} more ${noun}`);
  notice.disabled = true;
  select.append(notice);
}

function option(value: string, label: string): HTMLOptionElement {
  const element = document.createElement('option');
  element.value = value;
  element.textContent = label;
  return element;
}

function domainDisplayName(id: string | null, name: string | null): string {
  const value = name ?? id;
  if (value === null) return 'Unassigned';
  return value === '' ? 'Unnamed domain' : value;
}

function canonicalDomainName(id: string | null, fallbackName: string | null): string {
  return canonicalCommunityNames.get(id) ?? domainDisplayName(id, fallbackName);
}

function updateControlStates(): void {
  ui.globalMode.classList.toggle('is-active', state.mode === 'global');
  ui.focusMode.classList.toggle('is-active', state.mode === 'focus');
  ui.globalMode.setAttribute('aria-pressed', String(state.mode === 'global'));
  ui.focusMode.setAttribute('aria-pressed', String(state.mode === 'focus'));
  ui.focusMode.disabled = state.selectedId === null && state.focusId === null;
  ui.globalSurface.hidden = state.mode !== 'global';
  ui.lensSurface.hidden = state.mode !== 'focus';
  for (const density of ['focus', 'balanced', 'complete'] as const) {
    const active = state.density === density;
    ui.densityButtons[density].classList.toggle('is-active', active);
    ui.densityButtons[density].setAttribute('aria-pressed', String(active));
  }
  ui.trace.disabled = state.selectedId === null || state.mode !== 'global';
  ui.trace.classList.toggle('is-active', state.traceActive);
  ui.trace.setAttribute('aria-pressed', String(state.traceActive));
  ui.trace.textContent = state.traceActive ? 'Hide incoming relationships' : 'Trace incoming relationships';
  updateFilterSelectValues();
  ui.historyBack.disabled = state.historyIndex <= 0;
  ui.historyForward.disabled = state.historyIndex < 0 || state.historyIndex >= state.history.length - 1;
  const activeNode = state.keyboardId ? nodeById.get(state.keyboardId) : undefined;
  ui.canvasProxy.textContent = activeNode ? `${activeNode.label}, ${activeNode.kind}, ${activeNode.file}` : 'No active symbol';
}

function updateFilterSelectValues(): void {
  const communityToken = [...communityOptions].find(([, choice]) => !choice.all
    && (state.communityFilterUnassigned
      ? choice.id === null
      : state.communityFilter !== null && choice.id === state.communityFilter))?.[0] ?? 'community-all';
  const relationToken = [...relationOptions].find(([, choice]) => !choice.all && choice.relation === state.relationFilter)?.[0]
    ?? 'relation-all';
  ui.community.value = communityToken;
  ui.relation.value = relationToken;
}

function recomputeRenderEdges(): void {
  const selectedId = state.selectedId;
  const focusId = state.focusId;
  const ranked = [...renderCache.edges].sort((left, right) => {
    const leftSelected = selectedId !== null && (left.source === selectedId || left.target === selectedId) ? 0 : 1;
    const rightSelected = selectedId !== null && (right.source === selectedId || right.target === selectedId) ? 0 : 1;
    const leftTrace = state.traceActive && selectedId !== null && left.target === selectedId ? 0 : 1;
    const rightTrace = state.traceActive && selectedId !== null && right.target === selectedId ? 0 : 1;
    const leftFocus = focusId !== null && (left.source === focusId || left.target === focusId) ? 0 : 1;
    const rightFocus = focusId !== null && (right.source === focusId || right.target === focusId) ? 0 : 1;
    return leftSelected - rightSelected
      || leftTrace - rightTrace
      || leftFocus - rightFocus
      || confidenceRank(left.confidence) - confidenceRank(right.confidence)
      || compareEdges(left, right);
  });
  renderCache.edgeEntries = ranked.slice(0, Math.min(EDGE_BUDGETS[state.density], ranked.length));
}

function setMode(mode: GraphMode): void {
  if (!graph) return;
  if (mode === 'focus') {
    const focusId = state.selectedId ?? state.focusId;
    if (!focusId) return;
    setFocus(focusId);
    return;
  }
  state = { ...state, mode: 'global', traceActive: false };
  updateView();
  ui.canvas.focus();
  if (!state.cameraInitialized && renderCache.nodes.length > 0) {
    globalThis.requestAnimationFrame(() => {
      const bounds = ui.canvas.getBoundingClientRect();
      if (!state.cameraInitialized && bounds.width > 0 && bounds.height > 0) fitView(true);
    });
  }
  announce('Returned to Constellation view.');
}

function selectNode(id: string | null, center = false): void {
  if (id !== null && !nodeById.has(id)) return;
  state = { ...state, selectedId: id, keyboardId: id, focusId: id ?? state.focusId, traceActive: false };
  if (id !== null && center && state.mode === 'global') centerNode(id);
  renderInspector();
  renderLens();
  updateControlStates();
  recomputeRenderEdges();
  requestDraw();
  persist();
  emitRendererState();
  if (id === null) announce('Selection cleared.');
  else announce(`${requiredNode(id).label} selected.`);
}

function setFocus(id: string): void {
  if (!nodeById.has(id)) return;
  const truncated = state.history.slice(0, state.historyIndex + 1);
  const history = truncated.at(-1) === id ? truncated : [...truncated, id].slice(-MAX_HISTORY);
  state = {
    ...state,
    mode: 'focus',
    selectedId: id,
    focusId: id,
    keyboardId: id,
    traceActive: false,
    history,
    historyIndex: history.length - 1,
    expandedIncoming: false,
    expandedOutgoing: false,
  };
  updateView();
  announce(`Investigation Lens focused on ${requiredNode(id).label}.`);
}

function toggleTrace(): void {
  setTrace(!state.traceActive);
}

function setTrace(active: boolean): void {
  if (state.mode !== 'global' || state.selectedId === null) active = false;
  state = { ...state, traceActive: active };
  recomputeRenderEdges();
  updateControlStates();
  requestDraw();
  persist();
  emitRendererState();
  announce(active ? 'Incoming relationships highlighted.' : 'Incoming relationship highlight cleared.');
}

function resetView(): void {
  state = {
    ...state,
    communityFilter: null,
    communityFilterUnassigned: false,
    relationFilter: null,
    traceActive: false,
    query: '',
    scale: 1,
    offsetX: 0,
    offsetY: 0,
    cameraInitialized: false,
  };
  ui.search.value = '';
  closeSearchResults();
  updateView();
  if (state.mode === 'global') fitView(true);
  announce('View reset.');
}

function moveHistory(index: number): void {
  if (index < 0 || index >= state.history.length) return;
  const id = state.history[index];
  if (!id || !nodeById.has(id)) return;
  if (!nodePassesActiveFilters(id)) {
    state = {
      ...state,
      communityFilter: null,
      communityFilterUnassigned: false,
      relationFilter: null,
      traceActive: false,
    };
  }
  state = {
    ...state,
    mode: 'focus',
    selectedId: id,
    focusId: id,
    keyboardId: id,
    traceActive: false,
    historyIndex: index,
    expandedIncoming: false,
    expandedOutgoing: false,
  };
  updateView();
  announce(`Investigation history: ${requiredNode(id).label}.`);
}

function revealNode(id: string): void {
  if (!nodeById.has(id) || !sourceLinksEnabled) return;
  vscode.postMessage({ type: 'reveal', id });
}

function explainNode(id: string): void {
  if (!nodeById.has(id)) return;
  vscode.postMessage({ type: 'explain', id });
}

function renderHistory(): void {
  ui.history.replaceChildren();
  const activeIndex = state.historyIndex >= 0 ? state.historyIndex : state.history.length - 1;
  const start = Math.max(0, Math.min(Math.max(0, activeIndex - 2), Math.max(0, state.history.length - 6)));
  const visible = state.history.slice(start, start + 6);
  for (let offset = 0; offset < visible.length; offset += 1) {
    const id = visible[offset];
    const node = id ? nodeById.get(id) : undefined;
    if (!id || !node) continue;
    const item = document.createElement('button');
    item.type = 'button';
    item.className = 'gx-history-item';
    item.textContent = node.label;
    item.title = node.id;
    const index = start + offset;
    item.classList.toggle('is-active', index === state.historyIndex);
    item.setAttribute('aria-current', index === state.historyIndex ? 'step' : 'false');
    item.addEventListener('click', () => moveHistory(index));
    ui.history.append(item);
  }
  updateControlStates();
}

function searchMatches(): readonly VisualizerNodeV1[] {
  if (!graph) return [];
  const query = state.query.trim().toLocaleLowerCase('en');
  if (!query) return [];
  const terms = query.split(/\s+/u).filter(Boolean).slice(0, 8);
  return graph.nodes
    .map((node) => {
      const label = node.label.toLocaleLowerCase('en');
      const id = node.id.toLocaleLowerCase('en');
      const file = node.file.toLocaleLowerCase('en');
      const community = (node.communityName ?? node.community ?? '').toLocaleLowerCase('en');
      if (!terms.every((term) => label.includes(term) || id.includes(term) || file.includes(term) || community.includes(term))) return null;
      let score = 0;
      for (const term of terms) {
        if (label === term) score += 100;
        else if (label.startsWith(term)) score += 50;
        else if (label.includes(term)) score += 25;
        if (id.includes(term)) score += 8;
        if (file.includes(term)) score += 4;
        if (community.includes(term)) score += 2;
      }
      return { node, score };
    })
    .filter((entry): entry is { node: VisualizerNodeV1; score: number } => entry !== null)
    .sort((left, right) => right.score - left.score || compareNodes(left.node, right.node))
    .slice(0, MAX_SEARCH_RESULTS)
    .map((entry) => entry.node);
}

function renderSearchResults(): void {
  ui.searchResults.replaceChildren();
  if (!searchOpen) {
    ui.searchResults.hidden = true;
    ui.search.setAttribute('aria-expanded', 'false');
    ui.search.removeAttribute('aria-activedescendant');
    return;
  }
  const matches = searchMatches();
  if (!state.query.trim()) {
    closeSearchResults();
    return;
  }
  ui.searchResults.hidden = false;
  ui.search.setAttribute('aria-expanded', 'true');
  searchIndex = Math.min(searchIndex, matches.length - 1);
  if (matches.length === 0) {
    const empty = make('p', 'gx-search-empty', 'No matching symbols');
    ui.searchResults.append(empty);
    return;
  }
  matches.forEach((node, index) => {
    const item = document.createElement('button');
    item.type = 'button';
    item.id = `gx-search-result-${index}`;
    item.className = 'gx-search-result';
    item.setAttribute('role', 'option');
    item.setAttribute('aria-selected', String(index === searchIndex));
    item.append(make('strong', '', node.label), make('span', '', `${node.kind} · ${node.file}`));
    item.addEventListener('click', () => chooseSearchResult(node));
    ui.searchResults.append(item);
  });
  if (searchIndex >= 0) ui.search.setAttribute('aria-activedescendant', `gx-search-result-${searchIndex}`);
  else ui.search.removeAttribute('aria-activedescendant');
}

function chooseSearchResult(node: VisualizerNodeV1): void {
  closeSearchResults();
  if (!nodePassesActiveFilters(node.id)) {
    state = {
      ...state,
      communityFilter: null,
      communityFilterUnassigned: false,
      relationFilter: null,
      traceActive: false,
    };
  }
  if (state.mode === 'focus') {
    setFocus(node.id);
    return;
  }
  state = { ...state, selectedId: node.id, focusId: node.id, keyboardId: node.id, traceActive: false };
  updateView();
  centerNode(node.id);
  announce(`${node.label} selected.`);
}

function closeSearchResults(): void {
  searchOpen = false;
  ui.searchResults.hidden = true;
  ui.search.setAttribute('aria-expanded', 'false');
  ui.search.removeAttribute('aria-activedescendant');
  searchIndex = -1;
}

function handleSearchKeydown(event: KeyboardEvent): void {
  const matches = searchMatches();
  if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
    event.preventDefault();
    if (matches.length === 0) return;
    const direction = event.key === 'ArrowDown' ? 1 : -1;
    searchIndex = searchIndex < 0
      ? (direction > 0 ? 0 : matches.length - 1)
      : (searchIndex + direction + matches.length) % matches.length;
    renderSearchResults();
  } else if (event.key === 'Enter' && searchIndex >= 0) {
    event.preventDefault();
    const node = matches[searchIndex];
    if (node) chooseSearchResult(node);
  } else if (event.key === 'Escape') {
    event.preventDefault();
    closeSearchResults();
  }
}

function announce(message: string): void {
  ui.announcer.textContent = '';
  globalThis.setTimeout(() => { ui.announcer.textContent = message; }, 10);
}

function renderLens(): void {
  ui.incomingColumn.replaceChildren();
  ui.focusColumn.replaceChildren();
  ui.outgoingColumn.replaceChildren();
  if (!graph || state.mode !== 'focus') return;
  const focusId = state.focusId ?? state.selectedId;
  if (!focusId || !nodeById.has(focusId)) {
    ui.lensHeading.replaceChildren(make('h2', '', 'Choose a symbol to investigate'));
    return;
  }
  const focus = requiredNode(focusId);
  ui.lensHeading.replaceChildren(
    make('p', 'gx-eyebrow', 'One-hop architecture reading'),
    make('h2', '', focus.label),
    make('p', '', 'Incoming relationships converge on the focus; outgoing relationships flow away from it.'),
  );

  const allowedEdges = new Set(renderCache.edges);
  const incomingEntries = groupLensEntries((incoming.get(focusId) ?? []).filter((edge) => allowedEdges.has(edge)), 'incoming');
  const outgoingEntries = groupLensEntries((outgoing.get(focusId) ?? []).filter((edge) => allowedEdges.has(edge)), 'outgoing');
  const incomingRelationships = relationshipCount(incomingEntries);
  const outgoingRelationships = relationshipCount(outgoingEntries);
  renderLensColumn(ui.incomingColumn, 'Incoming relationships', incomingEntries, incomingRelationships, state.expandedIncoming, 'incoming');
  renderFocusCard(focus, incomingRelationships, outgoingRelationships);
  renderLensColumn(ui.outgoingColumn, 'Outgoing relationships', outgoingEntries, outgoingRelationships, state.expandedOutgoing, 'outgoing');
}

function groupLensEntries(edges: readonly VisualizerEdgeV1[], direction: LensEntry['direction']): readonly LensEntry[] {
  const grouped = new Map<string, VisualizerEdgeV1[]>();
  for (const edge of edges) {
    const id = direction === 'incoming' ? edge.source : edge.target;
    if (!nodeById.has(id)) continue;
    const values = grouped.get(id);
    if (values) values.push(edge);
    else grouped.set(id, [edge]);
  }
  return [...grouped]
    .map(([id, values]) => ({ node: requiredNode(id), edges: values.sort(compareEdges), direction }))
    .sort((left, right) => compareNodes(left.node, right.node));
}

function relationshipCount(entries: readonly LensEntry[]): number {
  return entries.reduce((count, entry) => count + entry.edges.length, 0);
}

function renderLensColumn(
  container: HTMLElement,
  heading: string,
  entries: readonly LensEntry[],
  exactRelationships: number,
  expanded: boolean,
  direction: LensEntry['direction'],
): void {
  const header = make('header', 'gx-lens-column-header');
  const count = make('span', '', exactRelationships.toLocaleString());
  count.title = `${exactRelationships.toLocaleString()} exact relationships across ${entries.length.toLocaleString()} related symbols`;
  header.append(make('h3', '', heading), count);
  container.append(header);
  if (entries.length === 0) {
    container.append(make('p', 'gx-lens-empty', direction === 'incoming' ? 'No incoming relationships in this view.' : 'No outgoing relationships in this view.'));
    return;
  }
  const visible = entries.slice(0, expanded ? LENS_MAX_PER_SIDE : LENS_INITIAL_PER_SIDE);
  const list = make('div', 'gx-lens-list');
  container.append(list);
  for (const entry of visible) list.append(createLensCard(entry));
  if (entries.length > LENS_INITIAL_PER_SIDE) {
    const more = document.createElement('button');
    more.type = 'button';
    more.className = 'gx-show-more';
    more.textContent = expanded ? 'Show fewer' : `Show ${Math.min(entries.length, LENS_MAX_PER_SIDE) - LENS_INITIAL_PER_SIDE} more`;
    more.setAttribute('aria-expanded', String(expanded));
    more.addEventListener('click', () => {
      state = direction === 'incoming'
        ? { ...state, expandedIncoming: !expanded }
        : { ...state, expandedOutgoing: !expanded };
      renderLens();
      persist();
    });
    container.append(more);
  }
  if (entries.length > LENS_MAX_PER_SIDE) {
    container.append(make('p', 'gx-lens-truncation', `${(entries.length - LENS_MAX_PER_SIDE).toLocaleString()} additional related symbols are outside the 72-card Lens limit.`));
  }
}

function createLensCard(entry: LensEntry): HTMLElement {
  const card = make('article', `gx-lens-card gx-lens-card-${entry.direction}`);
  card.dataset.nodeId = entry.node.id;
  const action = document.createElement('button');
  action.type = 'button';
  action.className = 'gx-lens-card-action';
  action.setAttribute('aria-label', `Focus ${entry.node.label}, ${entry.direction} relationship`);
  const heading = make('span', 'gx-card-heading');
  const glyph = make('span', 'gx-kind-glyph', kindGlyph(entry.node.kind));
  glyph.setAttribute('aria-hidden', 'true');
  const title = make('span');
  title.append(make('strong', '', entry.node.label), make('small', '', `${entry.node.kind} · ${entry.node.file}`));
  heading.append(glyph, title, make('span', 'gx-flow-arrow', entry.direction === 'incoming' ? '→' : '→'));
  action.append(heading);
  action.addEventListener('click', () => setFocus(entry.node.id));
  card.append(action);
  const relations = make('div', 'gx-card-relations');
  for (const edge of entry.edges.slice(0, 4)) relations.append(createRelationshipRow(edge, entry.direction));
  if (entry.edges.length > 4) relations.append(make('small', 'gx-more-relations', `+${entry.edges.length - 4} more exact relationships`));
  card.append(relations);
  action.addEventListener('keydown', (event) => {
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault();
      const actions = [...(card.parentElement?.querySelectorAll<HTMLButtonElement>('.gx-lens-card-action') ?? [])];
      const index = actions.indexOf(action);
      const next = actions[index + (event.key === 'ArrowDown' ? 1 : -1)];
      next?.focus();
    }
  });
  return card;
}

function renderFocusCard(node: VisualizerNodeV1, incomingCount: number, outgoingCount: number): void {
  const header = make('header', 'gx-lens-column-header');
  header.append(make('h3', '', 'Focus'), make('span', '', '1'));
  ui.focusColumn.append(header);
  const card = make('article', 'gx-focus-card');
  const halo = make('span', 'gx-focus-halo', kindGlyph(node.kind));
  halo.setAttribute('aria-hidden', 'true');
  card.append(halo, make('p', 'gx-eyebrow', canonicalDomainName(node.community, node.communityName)), make('h3', '', node.label));
  card.append(make('p', 'gx-focus-meta', `${node.kind} · ${node.file}${node.location ? `:${node.location}` : ''}`));
  const metrics = make('dl', 'gx-focus-metrics');
  appendMetric(metrics, 'Incoming', node.inDegree);
  appendMetric(metrics, 'Outgoing', node.outDegree);
  appendMetric(metrics, 'Visible incoming', incomingCount);
  appendMetric(metrics, 'Visible outgoing', outgoingCount);
  card.append(metrics);
  const actions = make('div', 'gx-card-actions');
  if (sourceLinksEnabled) {
    const open = button('', 'Open source');
    open.className = 'gx-primary-button';
    open.addEventListener('click', () => revealNode(node.id));
    actions.append(open);
  }
  const explain = button('', 'Explain');
  explain.className = 'gx-secondary-button';
  explain.addEventListener('click', () => explainNode(node.id));
  actions.append(explain);
  card.append(actions);
  ui.focusColumn.append(card);
}

function appendMetric(list: HTMLDListElement, label: string, value: number): void {
  list.append(make('div', 'gx-metric'));
  const container = list.lastElementChild;
  if (container) container.append(make('dt', '', label), make('dd', '', value.toLocaleString()));
}

function createRelationshipRow(edge: VisualizerEdgeV1, direction: LensEntry['direction']): HTMLElement {
  const row = make('div', 'gx-relationship-row');
  const confidence = confidencePresentation(edge.confidence);
  const swatch = make('i', `gx-confidence-swatch ${confidence.className}`, confidence.glyph);
  swatch.setAttribute('aria-hidden', 'true');
  const copy = make('span');
  copy.append(make('strong', '', edge.relation || 'Unspecified relationship'), make('small', '', `${direction === 'incoming' ? 'Incoming' : 'Outgoing'} · ${confidence.label}`));
  row.append(swatch, copy);
  const provenance = relationshipProvenance(edge);
  if (provenance) row.append(make('small', 'gx-provenance', provenance));
  return row;
}

function relationshipProvenance(edge: VisualizerEdgeV1): string {
  if (edge.sourceFile && edge.sourceLocation) return `${edge.sourceFile}:${edge.sourceLocation}`;
  return edge.sourceFile ?? (edge.sourceLocation ? `Location ${edge.sourceLocation}` : 'No relationship source location recorded.');
}

function confidencePresentation(value: string | null): { readonly className: string; readonly glyph: string; readonly label: string } {
  if (value === 'EXTRACTED') return { className: 'gx-solid', glyph: '✓', label: 'Extracted' };
  if (value === 'INFERRED') return { className: 'gx-dashed', glyph: '≈', label: 'Inferred' };
  if (value === 'AMBIGUOUS') return { className: 'gx-dotted', glyph: '?', label: 'Ambiguous' };
  return { className: 'gx-unknown', glyph: '·', label: 'Unspecified' };
}

function confidenceRank(value: string | null): number {
  if (value === 'EXTRACTED') return 0;
  if (value === 'INFERRED') return 1;
  if (value === 'AMBIGUOUS') return 2;
  return 3;
}

function kindGlyph(kind: string): string {
  const shape = canonicalNodeShape(kind);
  if (shape === 'code') return '●';
  if (shape === 'document') return '▣';
  if (shape === 'image') return '◆';
  if (shape === 'concept') return '⬡';
  return '○';
}

function canonicalNodeShape(kind: string): 'code' | 'document' | 'image' | 'concept' | 'unknown' {
  const normalized = kind.toLocaleLowerCase('en');
  if (normalized === 'code' || normalized.includes('code')) return 'code';
  if (normalized === 'document' || normalized === 'paper' || normalized === 'rationale') return 'document';
  if (normalized === 'image' || normalized.includes('image')) return 'image';
  if (normalized === 'concept' || normalized.includes('concept')) return 'concept';
  return 'unknown';
}

function renderInspector(): void {
  ui.inspector.replaceChildren();
  ui.inspector.classList.add('is-empty');
  if (!graph) return;
  const id = state.selectedId;
  if (!id || !nodeById.has(id)) {
    const empty = make('div', 'gx-inspector-empty');
    empty.append(make('span', 'gx-inspector-orb', '✦'), make('h2', '', 'Inspect the constellation'), make('p', '', 'Select a symbol to read its exact relationships and source provenance.'));
    ui.inspector.append(empty);
    return;
  }
  ui.inspector.classList.remove('is-empty');
  const node = requiredNode(id);
  const header = make('header', 'gx-inspector-header');
  header.append(make('p', 'gx-eyebrow', 'Selected symbol'), make('h2', '', node.label), make('p', '', `${node.kind} · ${node.file}${node.location ? `:${node.location}` : ''}`));
  ui.inspector.append(header);

  const actions = make('div', 'gx-inspector-actions');
  if (sourceLinksEnabled) {
    const open = button('', 'Open source');
    open.className = 'gx-primary-button';
    open.addEventListener('click', () => revealNode(node.id));
    actions.append(open);
  }
  const focus = button('', 'Open Lens');
  focus.className = 'gx-secondary-button';
  focus.addEventListener('click', () => setFocus(node.id));
  const explain = button('', 'Explain');
  explain.className = 'gx-secondary-button';
  explain.addEventListener('click', () => explainNode(node.id));
  actions.append(focus, explain);
  ui.inspector.append(actions);

  const metrics = make('dl', 'gx-inspector-metrics');
  appendMetric(metrics, 'Degree', node.degree);
  appendMetric(metrics, 'Incoming', node.inDegree);
  appendMetric(metrics, 'Outgoing', node.outDegree);
  ui.inspector.append(metrics);

  const relationships = make('section', 'gx-inspector-relationships');
  relationships.append(make('h3', '', 'Visible relationships'));
  const allIncidents = renderCache.edges
    .filter((edge) => edge.source === id || edge.target === id)
    .sort((left, right) => confidenceRank(left.confidence) - confidenceRank(right.confidence) || compareEdges(left, right));
  const incidents = allIncidents.slice(0, 16);
  if (incidents.length === 0) relationships.append(make('p', 'gx-lens-empty', 'No relationships are visible with the current filters.'));
  for (const edge of incidents) {
    const direction: LensEntry['direction'] = edge.target === id ? 'incoming' : 'outgoing';
    const otherId = direction === 'incoming' ? edge.source : edge.target;
    const row = createRelationshipRow(edge, direction);
    const other = nodeById.get(otherId);
    if (other) {
      const otherButton = document.createElement('button');
      otherButton.type = 'button';
      otherButton.className = 'gx-related-node';
      otherButton.textContent = other.label;
      otherButton.addEventListener('click', () => selectNode(other.id, true));
      row.append(otherButton);
    }
    relationships.append(row);
  }
  if (allIncidents.length > incidents.length) {
    relationships.append(make('p', 'gx-lens-truncation', `+${(allIncidents.length - incidents.length).toLocaleString()} more visible relationships. Open the Lens to explore the neighborhood.`));
  }
  ui.inspector.append(relationships);
}

function buildLayout(): void {
  positions = new Map();
  spatialGrid = new Map();
  nodeClearances = new Map();
  communityColors = new Map();
  if (!graph || graph.nodes.length === 0) {
    communityLayouts = [];
    return;
  }
  const groups = new Map<string | null, VisualizerNodeV1[]>();
  for (const node of graph.nodes) {
    const key = node.community;
    const values = groups.get(key);
    if (values) values.push(node);
    else groups.set(key, [node]);
  }
  const ordered = [...groups].sort((left, right) => {
    const leftName = canonicalDomainName(left[0], left[1][0]?.communityName ?? null);
    const rightName = canonicalDomainName(right[0], right[1][0]?.communityName ?? null);
    return right[1].length - left[1].length || compareText(leftName, rightName) || compareText(left[0] ?? '', right[0] ?? '');
  });
  const drafts = ordered.map(([key, rawNodes], groupIndex): CommunityLayoutDraft => {
    const nodes = [...rawNodes].sort(compareNodes);
    const seed = stableHash(key ?? UNASSIGNED_LAYOUT_SEED);
    const localPositions = new Map<string, Point>();
    nodes.forEach((node, index) => {
      localPositions.set(node.id, localNodePosition(index, nodes.length, seed));
    });
    const localRadius = nodes.reduce((maximum, node) => {
      const point = localPositions.get(node.id);
      return point ? Math.max(maximum, Math.hypot(point.x, point.y)) : maximum;
    }, 0);
    return {
      id: key,
      name: canonicalDomainName(key, nodes[0]?.communityName ?? null),
      color: COMMUNITY_COLORS[groupIndex % COMMUNITY_COLORS.length] ?? '#8B5CF6',
      nodes,
      localPositions,
      radius: localRadius + COMMUNITY_PADDING_WORLD,
    };
  });
  const centers = packCommunities(drafts);
  const layouts = drafts.map((draft, index): CommunityLayout => {
    const center = centers[index] ?? { x: 0, y: 0 };
    for (const node of draft.nodes) {
      const local = draft.localPositions.get(node.id) ?? { x: 0, y: 0 };
      positions.set(node.id, { x: center.x + local.x, y: center.y + local.y });
    }
    const points = draft.nodes.map((node) => positions.get(node.id)).filter((point): point is Point => point !== undefined);
    return {
      id: draft.id,
      name: draft.name,
      color: draft.color,
      center,
      hull: paddedHull(points, center, COMMUNITY_PADDING_WORLD),
      radius: draft.radius,
      nodeCount: draft.nodes.length,
    };
  });
  communityLayouts = layouts;
  communityColors = new Map(layouts.map((layout) => [layout.id, layout.color]));
  rebuildNodeSpatialIndex(graph.nodes);
}

function localNodePosition(index: number, nodeCount: number, seed: number): Point {
  if (index === 0) return { x: 0, y: 0 };
  let ring = 1;
  let ringStart = 1;
  while (index >= ringStart + ring * 6) {
    ringStart += ring * 6;
    ring += 1;
  }
  const count = Math.min(ring * 6, nodeCount - ringStart);
  const slot = index - ringStart;
  const rotation = (seed % 6_283) / 1_000 + ring * 0.37;
  const angle = rotation + slot / Math.max(1, count) * Math.PI * 2;
  const radius = ring * NODE_SPACING_WORLD;
  return { x: Math.cos(angle) * radius, y: Math.sin(angle) * radius };
}

function packCommunities(drafts: readonly CommunityLayoutDraft[]): readonly Point[] {
  if (drafts.length === 0) return [];
  const goldenAngle = Math.PI * (3 - Math.sqrt(5));
  const placements: PackedCommunity[] = [];
  const grid = new Map<string, number[]>();
  const nextCandidateByRadius = new Map<number, number>();
  const centers: Point[] = [];
  drafts.forEach((draft, index) => {
    let center: Point | undefined;
    if (index === 0) {
      center = { x: 0, y: 0 };
    } else {
      const bucket = Math.round(draft.radius / NODE_SPACING_WORLD);
      let candidateIndex = nextCandidateByRadius.get(bucket) ?? 1;
      const maximumAttempts = Math.max(2_048, drafts.length * 8);
      for (let attempt = 0; attempt < maximumAttempts; attempt += 1, candidateIndex += 1) {
        const distanceFromCenter = COMMUNITY_SPIRAL_STEP * Math.sqrt(candidateIndex);
        const angle = candidateIndex * goldenAngle;
        const candidate = {
          x: Math.cos(angle) * distanceFromCenter,
          y: Math.sin(angle) * distanceFromCenter,
        };
        if (communityPositionAvailable(candidate, draft.radius, placements, grid)) {
          center = candidate;
          nextCandidateByRadius.set(bucket, candidateIndex + 1);
          break;
        }
      }
    }
    if (!center) {
      const outerRadius = placements.reduce((maximum, placement) =>
        Math.max(maximum, Math.hypot(placement.center.x, placement.center.y) + placement.radius), 0);
      const angle = index * goldenAngle;
      const distanceFromCenter = outerRadius + draft.radius + COMMUNITY_GAP_WORLD;
      center = { x: Math.cos(angle) * distanceFromCenter, y: Math.sin(angle) * distanceFromCenter };
    }
    const placement = { center, radius: draft.radius };
    const placementIndex = placements.length;
    placements.push(placement);
    centers.push(center);
    for (const key of communityPackKeys(center, draft.radius + COMMUNITY_GAP_WORLD / 2)) {
      const values = grid.get(key);
      if (values) values.push(placementIndex);
      else grid.set(key, [placementIndex]);
    }
  });
  return centers;
}

function communityPositionAvailable(
  center: Point,
  radius: number,
  placements: readonly PackedCommunity[],
  grid: ReadonlyMap<string, readonly number[]>,
): boolean {
  const candidates = new Set<number>();
  for (const key of communityPackKeys(center, radius + COMMUNITY_GAP_WORLD / 2)) {
    for (const index of grid.get(key) ?? []) candidates.add(index);
  }
  for (const index of candidates) {
    const other = placements[index];
    if (!other) continue;
    if (distance(center, other.center) < radius + other.radius + COMMUNITY_GAP_WORLD) return false;
  }
  return true;
}

function communityPackKeys(center: Point, radius: number): readonly string[] {
  const keys: string[] = [];
  const minimumX = Math.floor((center.x - radius) / COMMUNITY_PACK_CELL);
  const maximumX = Math.floor((center.x + radius) / COMMUNITY_PACK_CELL);
  const minimumY = Math.floor((center.y - radius) / COMMUNITY_PACK_CELL);
  const maximumY = Math.floor((center.y + radius) / COMMUNITY_PACK_CELL);
  for (let x = minimumX; x <= maximumX; x += 1) {
    for (let y = minimumY; y <= maximumY; y += 1) keys.push(`${x},${y}`);
  }
  return keys;
}

function rebuildNodeSpatialIndex(nodes: readonly VisualizerNodeV1[]): void {
  spatialGrid = new Map();
  nodeClearances = new Map(nodes.map((node) => [node.id, POSITION_CELL]));
  for (const node of nodes) {
    const point = positions.get(node.id);
    if (!point) continue;
    const cellX = Math.floor(point.x / POSITION_CELL);
    const cellY = Math.floor(point.y / POSITION_CELL);
    for (let x = cellX - 1; x <= cellX + 1; x += 1) {
      for (let y = cellY - 1; y <= cellY + 1; y += 1) {
        for (const other of spatialGrid.get(`${x},${y}`) ?? []) {
          const otherPoint = positions.get(other.id);
          if (!otherPoint) continue;
          // The renderer and collision audit reserve each semantic shape's full
          // screen-space bounding box. Chebyshev separation is therefore the
          // conservative clearance: diagonal points must not claim the larger
          // Euclidean gap while their projected boxes still overlap on both axes.
          const separation = Math.max(Math.abs(point.x - otherPoint.x), Math.abs(point.y - otherPoint.y));
          nodeClearances.set(node.id, Math.min(nodeClearances.get(node.id) ?? POSITION_CELL, separation));
          nodeClearances.set(other.id, Math.min(nodeClearances.get(other.id) ?? POSITION_CELL, separation));
        }
      }
    }
    const key = spatialKey(point.x, point.y);
    const values = spatialGrid.get(key);
    if (values) values.push(node);
    else spatialGrid.set(key, [node]);
  }
}

function stableHash(value: string): number {
  let hash = 2166136261;
  for (let index = 0; index < value.length; index += 1) {
    hash ^= value.charCodeAt(index);
    hash = Math.imul(hash, 16777619);
  }
  return hash >>> 0;
}

function graphLayoutFingerprint(nodes: readonly VisualizerNodeV1[]): string {
  let hash = 2166136261;
  const ordered = [...nodes].sort((left, right) => compareText(left.id, right.id) || compareText(left.community ?? '', right.community ?? ''));
  for (const node of ordered) {
    for (const value of [node.id, node.community ?? UNASSIGNED_LAYOUT_SEED]) {
      for (let index = 0; index < value.length; index += 1) {
        hash ^= value.charCodeAt(index);
        hash = Math.imul(hash, 16777619);
      }
      hash ^= 0;
      hash = Math.imul(hash, 16777619);
    }
  }
  return `2:${nodes.length}:${(hash >>> 0).toString(16).padStart(8, '0')}`;
}

function spatialKey(x: number, y: number): string {
  return `${Math.floor(x / POSITION_CELL)},${Math.floor(y / POSITION_CELL)}`;
}

function paddedHull(points: readonly Point[], center: Point, padding: number): readonly Point[] {
  if (points.length < 3) return circularHull(points, center, padding);
  const sorted = [...points].sort((left, right) => left.x - right.x || left.y - right.y);
  const cross = (origin: Point, left: Point, right: Point): number =>
    (left.x - origin.x) * (right.y - origin.y) - (left.y - origin.y) * (right.x - origin.x);
  const lower: Point[] = [];
  for (const point of sorted) {
    while (lower.length >= 2 && cross(lower[lower.length - 2] ?? point, lower[lower.length - 1] ?? point, point) <= 0) lower.pop();
    lower.push(point);
  }
  const upper: Point[] = [];
  for (let index = sorted.length - 1; index >= 0; index -= 1) {
    const point = sorted[index];
    if (!point) continue;
    while (upper.length >= 2 && cross(upper[upper.length - 2] ?? point, upper[upper.length - 1] ?? point, point) <= 0) upper.pop();
    upper.push(point);
  }
  const hull = [...lower.slice(0, -1), ...upper.slice(0, -1)];
  if (hull.length < 3) return circularHull(points, center, padding);
  const centroid: MutablePoint = hull.reduce((sum, point) => ({ x: sum.x + point.x, y: sum.y + point.y }), { x: 0, y: 0 });
  centroid.x /= hull.length;
  centroid.y /= hull.length;
  return hull.map((point) => {
    const magnitude = Math.max(1, distance(point, centroid));
    return {
      x: point.x + (point.x - centroid.x) / magnitude * padding,
      y: point.y + (point.y - centroid.y) / magnitude * padding,
    };
  });
}

function circularHull(points: readonly Point[], center: Point, padding: number): readonly Point[] {
  const radius = points.reduce((maximum, point) => Math.max(maximum, distance(point, center)), 0) + padding;
  return Array.from({ length: 12 }, (_, index) => ({
    x: center.x + Math.cos(index / 12 * Math.PI * 2) * radius,
    y: center.y + Math.sin(index / 12 * Math.PI * 2) * radius,
  }));
}

function resizeCanvas(): void {
  const bounds = ui.canvas.getBoundingClientRect();
  const dpr = Math.min(2, Math.max(1, globalThis.devicePixelRatio || 1));
  const width = Math.max(1, Math.round(bounds.width * dpr));
  const height = Math.max(1, Math.round(bounds.height * dpr));
  if (ui.canvas.width !== width || ui.canvas.height !== height) {
    ui.canvas.width = width;
    ui.canvas.height = height;
    requestDraw();
  }
}

function requestDraw(): void {
  if (drawFrame === null) drawFrame = globalThis.requestAnimationFrame(draw);
}

function draw(timestamp: number): void {
  const drawStarted = performance.now();
  drawFrame = null;
  const animateTrace = state.traceActive && !reducedMotion && state.mode === 'global';
  if (animateTrace && timestamp - lastAnimationFrame < 34) {
    drawFrame = globalThis.requestAnimationFrame(draw);
    return;
  }
  lastAnimationFrame = timestamp;
  const bounds = ui.canvas.getBoundingClientRect();
  const dpr = Math.min(2, Math.max(1, globalThis.devicePixelRatio || 1));
  context.setTransform(dpr, 0, 0, dpr, 0, 0);
  context.clearRect(0, 0, bounds.width, bounds.height);
  if (!graph || state.mode !== 'global' || renderCache.nodes.length === 0) return;

  context.save();
  context.translate(state.offsetX, state.offsetY);
  context.scale(state.scale, state.scale);
  drawCommunityHulls(bounds);
  drawEdges(bounds, timestamp);
  const geometry = drawNodes(bounds);
  context.restore();
  if (testMode) publishGeometryDiagnostics(bounds, geometry, performance.now() - drawStarted);
  if (animateTrace) drawFrame = globalThis.requestAnimationFrame(draw);
}

function drawCommunityHulls(bounds: DOMRect): void {
  const visibleCommunities = new Set(renderCache.nodes.map((node) => node.community));
  const selectedCommunity = state.selectedId ? nodeById.get(state.selectedId)?.community : undefined;
  const layouts = communityLayouts
    .filter((layout) => visibleCommunities.has(layout.id))
    .sort((left, right) => Number(right.id === selectedCommunity) - Number(left.id === selectedCommunity)
      || right.nodeCount - left.nodeCount
      || compareText(left.name, right.name))
    .slice(0, 64);
  for (const layout of layouts) {
    if (layout.hull.length === 0 || !isWorldVisible(layout.center, bounds, 560)) continue;
    context.save();
    context.globalAlpha = state.traceActive && layout.id !== selectedCommunity ? 0.22 : 1;
    context.beginPath();
    layout.hull.forEach((point, index) => index === 0 ? context.moveTo(point.x, point.y) : context.lineTo(point.x, point.y));
    context.closePath();
    context.fillStyle = forcedColors ? 'Canvas' : colorWithAlpha(layout.color, 0.055);
    context.strokeStyle = forcedColors ? 'CanvasText' : colorWithAlpha(layout.color, 0.32);
    context.lineWidth = Math.max(1, 1.2 / state.scale);
    context.setLineDash([]);
    context.fill();
    context.stroke();
    context.restore();
  }
}

function drawEdges(bounds: DOMRect, timestamp: number): void {
  const visibleIds = new Set(renderCache.nodes.map((node) => node.id));
  let animatedParticles = 0;
  for (const edge of renderCache.edgeEntries) {
    if (!visibleIds.has(edge.source) || !visibleIds.has(edge.target)) continue;
    const source = positions.get(edge.source);
    const target = positions.get(edge.target);
    if (!source || !target || !isSegmentVisible(source, target, bounds)) continue;
    const traced = state.traceActive && state.selectedId !== null && edge.target === state.selectedId;
    const selected = state.selectedId !== null && (edge.source === state.selectedId || edge.target === state.selectedId);
    const selfLoop = edge.source === edge.target;
    const selfLoopNodeRadius = selfLoop ? nodeScreenRadius(requiredNode(edge.source)) : 0;
    if (selfLoop && selfLoopNodeRadius < 3.5 && !selected && !traced) continue;
    const confidence = confidencePresentation(edge.confidence);
    context.save();
    context.globalAlpha = state.traceActive && !traced ? 0.075 : selected ? 0.9 : 0.28;
    context.strokeStyle = forcedColors ? 'CanvasText' : traced ? '#7BC3E8' : selected ? '#C9B8FF' : '#7C86A3';
    context.fillStyle = context.strokeStyle;
    context.lineWidth = (traced ? 2.6 : selected ? 1.8 : 1) / state.scale;
    context.setLineDash(confidence.className === 'gx-dashed'
      ? [7 / state.scale, 5 / state.scale]
      : confidence.className === 'gx-dotted' || confidence.className === 'gx-unknown'
        ? [2 / state.scale, 5 / state.scale]
        : []);
    if (selfLoop) drawSelfLoop(source, selfLoopNodeRadius);
    else drawArrow(
      source,
      target,
      nodeEdgeExtent(requiredNode(edge.source)) / state.scale,
      nodeEdgeExtent(requiredNode(edge.target)) / state.scale,
    );
    if (edge.source !== edge.target && traced && !reducedMotion && animatedParticles < 96) {
      drawTraceParticle(source, target, timestamp, edge);
      animatedParticles += 1;
    }
    context.restore();
  }
  context.setLineDash([]);
  context.globalAlpha = 1;
}

function drawArrow(source: Point, target: Point, sourcePadding: number, targetPadding: number): void {
  const dx = target.x - source.x;
  const dy = target.y - source.y;
  const length = Math.max(1, Math.hypot(dx, dy));
  const unitX = dx / length;
  const unitY = dy / length;
  const start = { x: source.x + unitX * sourcePadding, y: source.y + unitY * sourcePadding };
  const end = { x: target.x - unitX * targetPadding, y: target.y - unitY * targetPadding };
  context.beginPath();
  context.moveTo(start.x, start.y);
  context.lineTo(end.x, end.y);
  context.stroke();
  const arrowSize = 5 / state.scale;
  context.beginPath();
  context.moveTo(end.x, end.y);
  context.lineTo(end.x - unitX * arrowSize - unitY * arrowSize * 0.7, end.y - unitY * arrowSize + unitX * arrowSize * 0.7);
  context.lineTo(end.x - unitX * arrowSize + unitY * arrowSize * 0.7, end.y - unitY * arrowSize - unitX * arrowSize * 0.7);
  context.closePath();
  context.fill();
}

function drawSelfLoop(point: Point, nodeRadius: number): void {
  const radius = Math.max(6, nodeRadius + 5) / state.scale;
  const center = { x: point.x, y: point.y - (nodeRadius + 7) / state.scale };
  const startAngle = Math.PI * 0.38;
  const endAngle = Math.PI * 2.52;
  context.beginPath();
  context.arc(center.x, center.y, radius, startAngle, endAngle);
  context.stroke();
  const end = {
    x: center.x + Math.cos(endAngle) * radius,
    y: center.y + Math.sin(endAngle) * radius,
  };
  const tangent = endAngle + Math.PI / 2;
  const arrowSize = 5 / state.scale;
  context.beginPath();
  context.moveTo(end.x, end.y);
  context.lineTo(
    end.x - Math.cos(tangent - 0.65) * arrowSize,
    end.y - Math.sin(tangent - 0.65) * arrowSize,
  );
  context.lineTo(
    end.x - Math.cos(tangent + 0.65) * arrowSize,
    end.y - Math.sin(tangent + 0.65) * arrowSize,
  );
  context.closePath();
  context.fill();
}

function drawTraceParticle(source: Point, target: Point, timestamp: number, edge: VisualizerEdgeV1): void {
  const phase = ((timestamp / 1_400) + (stableHash(`${edge.source}\u0000${edge.target}\u0000${edge.relation}`) % 1_000) / 1_000) % 1;
  const x = source.x + (target.x - source.x) * phase;
  const y = source.y + (target.y - source.y) * phase;
  context.save();
  context.globalAlpha = 1;
  context.fillStyle = forcedColors ? 'Highlight' : '#7BC3E8';
  context.beginPath();
  context.arc(x, y, 3.4 / state.scale, 0, Math.PI * 2);
  context.fill();
  context.restore();
}

function drawNodes(bounds: DOMRect): DrawGeometry {
  const candidates: VisualizerNodeV1[] = [];
  const tracedSources = new Set<string>();
  if (state.traceActive && state.selectedId !== null) {
    for (const edge of renderCache.edgeEntries) if (edge.target === state.selectedId) tracedSources.add(edge.source);
  }
  for (const node of renderCache.nodes) {
    const point = positions.get(node.id);
    if (!point || !isWorldVisible(point, bounds, 38)) continue;
    candidates.push(node);
    const selected = node.id === state.selectedId;
    const keyboard = node.id === state.keyboardId;
    const traced = tracedSources.has(node.id);
    const radius = nodeScreenRadius(node);
    context.save();
    context.globalAlpha = state.traceActive && !selected && !traced ? 0.18 : 1;
    const shape = state.scale < 0.12 ? 'code' : canonicalNodeShape(node.kind);
    context.fillStyle = forcedColors
      ? (selected ? 'Highlight' : shape === 'unknown' ? 'Canvas' : 'CanvasText')
      : selected ? '#8B5CF6' : traced ? '#7BC3E8' : shape === 'unknown' ? '#0B0D14' : communityColor(node.community);
    context.strokeStyle = forcedColors ? 'Canvas' : '#0B0D14';
    if (shape === 'unknown') context.strokeStyle = forcedColors ? 'CanvasText' : communityColor(node.community);
    context.lineWidth = nodeStrokeWidth(radius) / state.scale;
    nodeShapePath(point, radius / state.scale, shape);
    context.fill();
    context.stroke();
    if (selected || keyboard) {
      context.strokeStyle = forcedColors ? 'Highlight' : selected ? '#C9B8FF' : '#7BC3E8';
      context.lineWidth = (selected ? 2.5 : 1.5) / state.scale;
      context.setLineDash(keyboard && !selected ? [4 / state.scale, 3 / state.scale] : []);
      context.beginPath();
      context.arc(point.x, point.y, (radius + 5) / state.scale, 0, Math.PI * 2);
      context.stroke();
      context.setLineDash([]);
      if (selected) {
        context.globalAlpha = 0.35;
        context.beginPath();
        context.arc(point.x, point.y, (radius + 10) / state.scale, 0, Math.PI * 2);
        context.stroke();
      }
    }
    context.restore();
  }
  return drawLabels(candidates, bounds);
}

function nodeShapePath(point: Point, radius: number, shape: ReturnType<typeof canonicalNodeShape>): void {
  context.beginPath();
  if (shape === 'document') {
    context.rect(point.x - radius, point.y - radius, radius * 2, radius * 2);
    return;
  }
  if (shape === 'image') {
    context.moveTo(point.x, point.y - radius * 1.25);
    context.lineTo(point.x + radius * 1.25, point.y);
    context.lineTo(point.x, point.y + radius * 1.25);
    context.lineTo(point.x - radius * 1.25, point.y);
    context.closePath();
    return;
  }
  if (shape === 'concept') {
    for (let index = 0; index < 6; index += 1) {
      const angle = Math.PI / 3 * index - Math.PI / 2;
      const x = point.x + Math.cos(angle) * radius * 1.15;
      const y = point.y + Math.sin(angle) * radius * 1.15;
      if (index === 0) context.moveTo(x, y);
      else context.lineTo(x, y);
    }
    context.closePath();
    return;
  }
  context.arc(point.x, point.y, radius, 0, Math.PI * 2);
}

function drawLabels(nodes: readonly VisualizerNodeV1[], viewport: DOMRect): DrawGeometry {
  const labels = [...nodes]
    .sort((left, right) => {
      const leftPriority = left.id === state.selectedId || left.id === state.keyboardId ? 0 : 1;
      const rightPriority = right.id === state.selectedId || right.id === state.keyboardId ? 0 : 1;
      return leftPriority - rightPriority || compareNodes(left, right);
    });
  const fittedScale = fitScaleForViewport(viewport);
  const zoomRatio = state.scale / Math.max(MIN_CAMERA_SCALE, fittedScale);
  const labelBudget = Math.min(MAX_LABELS, Math.max(16, Math.floor(
    viewport.width * viewport.height / (zoomRatio < 1.3 ? 18_000 : zoomRatio < 2.2 ? 13_000 : 9_000),
  )));
  const glyphs: GlyphRect[] = nodes.flatMap((node) => {
    const point = positions.get(node.id);
    if (!point) return [];
    const extent = glyphScreenExtent(node);
    const x = point.x * state.scale + state.offsetX;
    const y = point.y * state.scale + state.offsetY;
    return [{ id: node.id, left: x - extent, top: y - extent, right: x + extent, bottom: y + extent }];
  });
  const occupied: ScreenRect[] = [];
  const acceptedLabels = drawCommunityLabels(glyphs, occupied, viewport, zoomRatio);
  context.font = `${11 / state.scale}px -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif`;
  context.textBaseline = 'top';
  let drawn = 0;
  for (const node of labels) {
    if (drawn >= labelBudget) break;
    const point = positions.get(node.id);
    if (!point) continue;
    const priority = node.id === state.selectedId || node.id === state.keyboardId;
    if (!priority && zoomRatio < 0.72) continue;
    const label = fittedCanvasLabel(boundedText(node.label, 52), Math.min(220, viewport.width * 0.44));
    const width = context.measureText(label).width * state.scale + 10;
    const height = 17;
    const extent = nodeShapeExtent(nodeScreenRadius(node));
    const screenPoint = { x: point.x * state.scale + state.offsetX, y: point.y * state.scale + state.offsetY };
    const anchors = labelAnchors(screenPoint, extent, width, height);
    let accepted: { readonly bounds: ScreenRect; readonly x: number; readonly y: number; readonly align: CanvasTextAlign } | undefined;
    for (const anchor of anchors) {
      const padded = inflateRectangle(anchor.bounds, 2);
      if (!rectangleInsideViewport(padded, viewport, 5)) continue;
      if (occupied.some((entry) => rectanglesOverlap(entry, padded))) continue;
      if (glyphs.some((glyph) => glyph.id !== node.id && rectanglesOverlap(glyph, padded))) continue;
      accepted = anchor;
      break;
    }
    if (!accepted) continue;
    occupied.push(inflateRectangle(accepted.bounds, 2));
    acceptedLabels.push({
      ...accepted.bounds,
      kind: 'node',
      itemIndex: nodeIndexById.get(node.id) ?? -1,
    });
    drawn += 1;
    context.textAlign = accepted.align;
    context.lineWidth = 3 / state.scale;
    context.strokeStyle = forcedColors ? 'Canvas' : 'rgba(9, 11, 17, .94)';
    context.fillStyle = forcedColors ? 'CanvasText' : node.id === state.selectedId ? '#FFFFFF' : '#D9DDEA';
    const worldX = (accepted.x - state.offsetX) / state.scale;
    const worldY = (accepted.y - state.offsetY) / state.scale;
    context.strokeText(label, worldX, worldY);
    context.fillText(label, worldX, worldY);
  }
  return { glyphs, labels: acceptedLabels };
}

function drawCommunityLabels(
  glyphs: readonly GlyphRect[],
  occupied: ScreenRect[],
  viewport: DOMRect,
  zoomRatio: number,
): GeometryLabel[] {
  const acceptedLabels: GeometryLabel[] = [];
  if (zoomRatio < 0.7) return acceptedLabels;
  const visibleCommunities = new Set(renderCache.nodes.map((node) => node.community));
  const selectedCommunity = state.selectedId ? nodeById.get(state.selectedId)?.community : undefined;
  const budget = Math.min(28, Math.max(4, Math.floor(viewport.width * viewport.height / 46_000)));
  const layouts = communityLayouts
    .filter((layout) => visibleCommunities.has(layout.id))
    .sort((left, right) => Number(right.id === selectedCommunity) - Number(left.id === selectedCommunity)
      || right.nodeCount - left.nodeCount
      || compareText(left.name, right.name));
  context.font = `${10 / state.scale}px ui-monospace, SFMono-Regular, Menlo, monospace`;
  context.textBaseline = 'top';
  context.textAlign = 'center';
  let drawn = 0;
  for (const [layoutIndex, layout] of layouts.entries()) {
    if (drawn >= budget || layout.hull.length === 0) break;
    const label = fittedCanvasLabel(layout.name.toLocaleUpperCase('en'), Math.min(210, viewport.width * 0.38));
    const width = context.measureText(label).width * state.scale + 12;
    const height = 16;
    const screenX = layout.center.x * state.scale + state.offsetX;
    const screenY = Math.min(...layout.hull.map((point) => point.y)) * state.scale + state.offsetY + 7;
    const bounds = { left: screenX - width / 2, top: screenY, right: screenX + width / 2, bottom: screenY + height };
    const padded = inflateRectangle(bounds, 2);
    if (!rectangleInsideViewport(padded, viewport, 5)
      || occupied.some((entry) => rectanglesOverlap(entry, padded))
      || glyphs.some((glyph) => rectanglesOverlap(glyph, padded))) continue;
    occupied.push(padded);
    acceptedLabels.push({ ...bounds, kind: 'community', itemIndex: layoutIndex });
    drawn += 1;
    context.fillStyle = forcedColors ? 'CanvasText' : '#C9B8FF';
    context.globalAlpha = forcedColors ? 1 : 0.78;
    context.fillText(label, layout.center.x, (screenY - state.offsetY) / state.scale);
    context.globalAlpha = 1;
  }
  return acceptedLabels;
}

function labelAnchors(
  point: Point,
  extent: number,
  width: number,
  height: number,
): readonly { readonly bounds: ScreenRect; readonly x: number; readonly y: number; readonly align: CanvasTextAlign }[] {
  const gap = 6;
  return [
    {
      bounds: { left: point.x - width / 2, top: point.y + extent + gap, right: point.x + width / 2, bottom: point.y + extent + gap + height },
      x: point.x,
      y: point.y + extent + gap,
      align: 'center',
    },
    {
      bounds: { left: point.x + extent + gap, top: point.y - height / 2, right: point.x + extent + gap + width, bottom: point.y + height / 2 },
      x: point.x + extent + gap + 5,
      y: point.y - height / 2,
      align: 'left',
    },
    {
      bounds: { left: point.x - extent - gap - width, top: point.y - height / 2, right: point.x - extent - gap, bottom: point.y + height / 2 },
      x: point.x - extent - gap - 5,
      y: point.y - height / 2,
      align: 'right',
    },
    {
      bounds: { left: point.x - width / 2, top: point.y - extent - gap - height, right: point.x + width / 2, bottom: point.y - extent - gap },
      x: point.x,
      y: point.y - extent - gap - height,
      align: 'center',
    },
  ];
}

function fittedCanvasLabel(value: string, maximumScreenWidth: number): string {
  if (context.measureText(value).width * state.scale <= maximumScreenWidth) return value;
  let length = Math.min(value.length, 48);
  while (length > 1) {
    const candidate = `${value.slice(0, length).trimEnd()}…`;
    if (context.measureText(candidate).width * state.scale <= maximumScreenWidth) return candidate;
    length -= 1;
  }
  return '…';
}

function inflateRectangle(rectangle: ScreenRect, amount: number): ScreenRect {
  return {
    left: rectangle.left - amount,
    top: rectangle.top - amount,
    right: rectangle.right + amount,
    bottom: rectangle.bottom + amount,
  };
}

function rectangleInsideViewport(rectangle: ScreenRect, viewport: DOMRect, margin: number): boolean {
  return rectangle.left >= margin
    && rectangle.top >= margin
    && rectangle.right <= viewport.width - margin
    && rectangle.bottom <= viewport.height - margin;
}

function publishGeometryDiagnostics(viewport: DOMRect, geometry: DrawGeometry, drawMilliseconds: number): void {
  const diagnostics: GeometryDiagnostics = {
    viewport: {
      width: viewport.width,
      height: viewport.height,
      dpr: Math.min(2, Math.max(1, globalThis.devicePixelRatio || 1)),
    },
    scale: state.scale,
    fittedScale: fitScaleForViewport(viewport),
    glyphs: geometry.glyphs.map((glyph) => ({
      left: glyph.left,
      top: glyph.top,
      right: glyph.right,
      bottom: glyph.bottom,
      nodeIndex: nodeIndexById.get(glyph.id) ?? -1,
      emphasized: glyph.id === state.selectedId || glyph.id === state.keyboardId,
    })),
    labels: geometry.labels,
    visibleNodes: renderCache.nodes.length,
    visibleEdges: renderCache.edgeEntries.length,
    positions: positions.size,
    spatialCells: spatialGrid.size,
    layoutMilliseconds: lastLayoutMilliseconds,
    drawMilliseconds,
  };
  Reflect.set(globalThis, '__graphoxideVisualizerDiagnostics', diagnostics);
  document.documentElement.dataset.graphoxideDiagnostics = String(diagnostics.glyphs.length);
  vscode.postMessage({ type: 'geometryDiagnostics', diagnostics });
}

function rectanglesOverlap(
  left: { readonly left: number; readonly top: number; readonly right: number; readonly bottom: number },
  right: { readonly left: number; readonly top: number; readonly right: number; readonly bottom: number },
): boolean {
  return left.left < right.right && left.right > right.left && left.top < right.bottom && left.bottom > right.top;
}

function communityColor(id: string | null): string {
  return communityColors.get(id) ?? '#8B5CF6';
}

function nodeScreenRadius(node: VisualizerNodeV1): number {
  const base = Math.min(9.8, Math.max(3.3, 3.2 + Math.sqrt(Math.max(0, node.degree)) * 1.15));
  const projected = Math.min(10, Math.max(1.5, base * Math.sqrt(Math.max(MIN_CAMERA_SCALE, state.scale))));
  const clearance = (nodeClearances.get(node.id) ?? NODE_SPACING_WORLD) * state.scale;
  const collisionSafe = Math.max(0.25, (clearance - 5) / (2 * 1.45));
  return Math.min(projected, collisionSafe);
}

function nodeShapeExtent(radius: number): number {
  // At overview scales every semantic shape is intentionally rendered as a
  // circle. Reserve diamond/hexagon corners only once those shapes are visible;
  // otherwise the conservative box itself can force microscopic glyphs.
  return radius * (state.scale < 0.12 ? 1 : 1.45);
}

function nodeStrokeWidth(radius: number): number {
  return state.scale < 0.12
    ? Math.min(1.25, Math.max(0.75, radius * 0.9))
    : Math.min(2.1, Math.max(1.15, radius * 0.9));
}

function nodeEdgeExtent(node: VisualizerNodeV1): number {
  const radius = nodeScreenRadius(node);
  return nodeShapeExtent(radius) + nodeStrokeWidth(radius) / 2;
}

function glyphScreenExtent(node: VisualizerNodeV1): number {
  return nodeEdgeExtent(node) + 0.75;
}

function colorWithAlpha(color: string, alpha: number): string {
  const match = /^#([0-9a-f]{6})$/iu.exec(color);
  if (!match?.[1]) return color;
  const value = Number.parseInt(match[1], 16);
  return `rgba(${value >>> 16}, ${(value >>> 8) & 255}, ${value & 255}, ${alpha})`;
}

function isWorldVisible(point: Point, bounds: DOMRect, margin: number): boolean {
  const x = point.x * state.scale + state.offsetX;
  const y = point.y * state.scale + state.offsetY;
  return x >= -margin && y >= -margin && x <= bounds.width + margin && y <= bounds.height + margin;
}

function isSegmentVisible(source: Point, target: Point, bounds: DOMRect): boolean {
  return isWorldVisible(source, bounds, 80)
    || isWorldVisible(target, bounds, 80)
    || (Math.min(source.x, target.x) * state.scale + state.offsetX <= bounds.width
      && Math.max(source.x, target.x) * state.scale + state.offsetX >= 0
      && Math.min(source.y, target.y) * state.scale + state.offsetY <= bounds.height
      && Math.max(source.y, target.y) * state.scale + state.offsetY >= 0);
}

function zoomAt(factor: number, screenX?: number, screenY?: number): void {
  const bounds = ui.canvas.getBoundingClientRect();
  const anchorX = screenX ?? bounds.width / 2;
  const anchorY = screenY ?? bounds.height / 2;
  const previousScale = state.scale;
  const scale = clampNumber(previousScale * factor, MIN_CAMERA_SCALE, 5, previousScale);
  const worldX = (anchorX - state.offsetX) / previousScale;
  const worldY = (anchorY - state.offsetY) / previousScale;
  state = {
    ...state,
    scale,
    offsetX: anchorX - worldX * scale,
    offsetY: anchorY - worldY * scale,
    cameraInitialized: true,
  };
  requestDraw();
  persistSoon();
}

function fitView(announceChange = false): void {
  const worldBounds = visibleWorldBounds();
  if (!worldBounds) return;
  const bounds = ui.canvas.getBoundingClientRect();
  if (bounds.width <= 0 || bounds.height <= 0) return;
  const scale = fitScaleForViewport(bounds);
  state = {
    ...state,
    scale,
    offsetX: bounds.width / 2 - (worldBounds.minimumX + worldBounds.maximumX) / 2 * scale,
    offsetY: bounds.height / 2 - (worldBounds.minimumY + worldBounds.maximumY) / 2 * scale,
    cameraInitialized: true,
  };
  requestDraw();
  persist();
  if (announceChange) announce('Graph fitted to the viewport.');
}

function fitScaleForViewport(viewport: DOMRect): number {
  const worldBounds = visibleWorldBounds();
  if (!worldBounds || viewport.width <= 0 || viewport.height <= 0) return 1;
  const margin = 38;
  const availableWidth = Math.max(1, viewport.width - margin * 2);
  const availableHeight = Math.max(1, viewport.height - margin * 2);
  const worldWidth = Math.max(120, worldBounds.maximumX - worldBounds.minimumX);
  const worldHeight = Math.max(100, worldBounds.maximumY - worldBounds.minimumY);
  return clampNumber(Math.min(availableWidth / worldWidth, availableHeight / worldHeight), MIN_CAMERA_SCALE, 2.2, 1);
}

function visibleWorldBounds(): {
  readonly minimumX: number;
  readonly maximumX: number;
  readonly minimumY: number;
  readonly maximumY: number;
} | null {
  const visibleCommunities = new Set(renderCache.nodes.map((node) => node.community));
  const hullPoints = communityLayouts
    .filter((layout) => visibleCommunities.has(layout.id))
    .flatMap((layout) => layout.hull);
  const points = hullPoints.length > 0
    ? hullPoints
    : renderCache.nodes.map((node) => positions.get(node.id)).filter((point): point is Point => point !== undefined);
  if (points.length === 0) return null;
  return {
    minimumX: Math.min(...points.map((point) => point.x)),
    maximumX: Math.max(...points.map((point) => point.x)),
    minimumY: Math.min(...points.map((point) => point.y)),
    maximumY: Math.max(...points.map((point) => point.y)),
  };
}

function centerNode(id: string): void {
  const point = positions.get(id);
  if (!point) return;
  const bounds = ui.canvas.getBoundingClientRect();
  state = {
    ...state,
    offsetX: bounds.width / 2 - point.x * state.scale,
    offsetY: bounds.height / 2 - point.y * state.scale,
    cameraInitialized: true,
  };
  requestDraw();
  persistSoon();
}

function handlePointerDown(event: PointerEvent): void {
  if (event.button !== 0) return;
  ui.canvas.setPointerCapture(event.pointerId);
  pointer = { id: event.pointerId, startX: event.clientX, startY: event.clientY, lastX: event.clientX, lastY: event.clientY, moved: false };
  ui.canvas.classList.add('is-panning');
}

function handlePointerMove(event: PointerEvent): void {
  if (!pointer || pointer.id !== event.pointerId) return;
  const dx = event.clientX - pointer.lastX;
  const dy = event.clientY - pointer.lastY;
  const moved = pointer.moved || Math.hypot(event.clientX - pointer.startX, event.clientY - pointer.startY) > 4;
  pointer = { ...pointer, lastX: event.clientX, lastY: event.clientY, moved };
  if (moved) {
    state = { ...state, offsetX: state.offsetX + dx, offsetY: state.offsetY + dy, cameraInitialized: true };
    requestDraw();
    persistSoon();
  }
}

function handlePointerEnd(event: PointerEvent): void {
  if (!pointer || pointer.id !== event.pointerId) return;
  const moved = pointer.moved;
  pointer = null;
  ui.canvas.classList.remove('is-panning');
  if (ui.canvas.hasPointerCapture(event.pointerId)) ui.canvas.releasePointerCapture(event.pointerId);
  if (!moved) {
    const node = hitTest(event.clientX, event.clientY);
    selectNode(node?.id ?? null);
  }
}

function hitTest(clientX: number, clientY: number): VisualizerNodeV1 | null {
  const bounds = ui.canvas.getBoundingClientRect();
  const world = {
    x: (clientX - bounds.left - state.offsetX) / state.scale,
    y: (clientY - bounds.top - state.offsetY) / state.scale,
  };
  const cellX = Math.floor(world.x / POSITION_CELL);
  const cellY = Math.floor(world.y / POSITION_CELL);
  const cellRadius = Math.max(1, Math.min(8, Math.ceil(25 / state.scale / POSITION_CELL)));
  const visibleIds = new Set(renderCache.nodes.map((node) => node.id));
  let best: { node: VisualizerNodeV1; distance: number } | null = null;
  for (let x = cellX - cellRadius; x <= cellX + cellRadius; x += 1) {
    for (let y = cellY - cellRadius; y <= cellY + cellRadius; y += 1) {
      for (const node of spatialGrid.get(`${x},${y}`) ?? []) {
        if (!visibleIds.has(node.id)) continue;
        const point = positions.get(node.id);
        if (!point) continue;
        const nodeDistance = distance(world, point) * state.scale;
        const hitRadius = Math.max(12, nodeShapeExtent(nodeScreenRadius(node)) + 8);
        if (nodeDistance <= hitRadius && (!best || nodeDistance < best.distance)) best = { node, distance: nodeDistance };
      }
    }
  }
  return best?.node ?? null;
}

function handleCanvasKeydown(event: KeyboardEvent): void {
  if (!graph || renderCache.nodes.length === 0) return;
  if (event.key === 'Enter') {
    event.preventDefault();
    const id = state.keyboardId ?? state.selectedId;
    if (id) selectNode(id);
    return;
  }
  if (event.key === 'Escape') {
    event.preventDefault();
    selectNode(null);
    return;
  }
  const direction = event.key === 'ArrowLeft' ? { x: -1, y: 0 }
    : event.key === 'ArrowRight' ? { x: 1, y: 0 }
      : event.key === 'ArrowUp' ? { x: 0, y: -1 }
        : event.key === 'ArrowDown' ? { x: 0, y: 1 }
          : null;
  if (!direction) return;
  event.preventDefault();
  const current = state.keyboardId && nodeById.has(state.keyboardId)
    ? requiredNode(state.keyboardId)
    : [...renderCache.nodes].sort(compareNodes)[0];
  if (!current) return;
  const origin = positions.get(current.id);
  if (!origin) return;
  let best: { node: VisualizerNodeV1; score: number } | null = null;
  for (const candidate of renderCache.nodes) {
    if (candidate.id === current.id) continue;
    const point = positions.get(candidate.id);
    if (!point) continue;
    const dx = point.x - origin.x;
    const dy = point.y - origin.y;
    const projection = dx * direction.x + dy * direction.y;
    if (projection <= 0) continue;
    const perpendicular = Math.abs(dx * direction.y - dy * direction.x);
    const score = projection + perpendicular * 2.25;
    if (!best || score < best.score || score === best.score && compareNodes(candidate, best.node) < 0) best = { node: candidate, score };
  }
  if (!best) return;
  state = { ...state, keyboardId: best.node.id };
  ui.canvasProxy.textContent = `${best.node.label}, ${best.node.kind}, ${best.node.file}`;
  centerNode(best.node.id);
  requestDraw();
  persistSoon();
  announce(`${best.node.label}, ${best.node.kind}.`);
}

function distance(left: Point, right: Point): number {
  return Math.hypot(left.x - right.x, left.y - right.y);
}
