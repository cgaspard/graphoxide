(function initializeSemanticAtlas() {
  'use strict';

  const graph = globalThis.GRAPHOXIDE_GRAPH_FIXTURE;
  if (!graph || graph.contractVersion !== 1) {
    throw new Error('Semantic Atlas requires graph concept fixture contract v1.');
  }

  const SVG_NS = 'http://www.w3.org/2000/svg';
  const WORLD = { width: 1820, height: 1090, left: 32, right: 32, top: 72, bottom: 30 };
  const LANE_GAP = 14;
  const FLOW_LEFT = 285;
  const FLOW_RIGHT = 1715;
  const MIN_SCALE = 0.28;
  const MAX_SCALE = 3.4;
  const colors = [
    { color: '#315b80', tint: '#e5edf3' },
    { color: '#d3633b', tint: '#f6e8e0' },
    { color: '#77527e', tint: '#eee7ef' },
    { color: '#397361', tint: '#e3eee9' },
    { color: '#9a772d', tint: '#f3ecd9' },
    { color: '#58666e', tint: '#e7ebed' },
    { color: '#9a4f62', tint: '#f2e5e8' },
    { color: '#53763b', tint: '#e8eee3' },
  ];

  const elements = {
    stage: document.getElementById('atlas'),
    svg: document.getElementById('graph'),
    world: document.getElementById('world'),
    laneLayer: document.getElementById('lane-layer'),
    edgeLayer: document.getElementById('edge-layer'),
    nodeLayer: document.getElementById('node-layer'),
    search: document.getElementById('search'),
    searchBox: document.querySelector('.search-box'),
    searchCount: document.getElementById('search-count'),
    searchResults: document.getElementById('search-results'),
    domainFilters: document.getElementById('domain-filters'),
    relationFilters: document.getElementById('relation-filters'),
    relationTotal: document.getElementById('relation-total'),
    confidenceFilters: document.getElementById('confidence-filters'),
    visibleStatus: document.getElementById('visible-status'),
    interactionHint: document.getElementById('interaction-hint'),
    inspector: document.getElementById('inspector'),
    emptyInspector: document.getElementById('empty-inspector'),
    nodeInspector: document.getElementById('node-inspector'),
    minimap: document.getElementById('minimap'),
    minimapContent: document.getElementById('minimap-content'),
    minimapViewport: document.getElementById('minimap-viewport'),
    zoomOutput: document.getElementById('zoom-output'),
    helpDialog: document.getElementById('help-dialog'),
  };

  const nodesById = new Map(graph.nodes.map((node) => [node.id, node]));
  const adjacency = new Map(graph.nodes.map((node) => [node.id, []]));
  const incoming = new Map(graph.nodes.map((node) => [node.id, 0]));
  const outgoing = new Map(graph.nodes.map((node) => [node.id, 0]));
  for (const edge of graph.edges) {
    adjacency.get(edge.source).push({ edge, node: nodesById.get(edge.target), direction: 'out' });
    adjacency.get(edge.target).push({ edge, node: nodesById.get(edge.source), direction: 'in' });
    outgoing.set(edge.source, outgoing.get(edge.source) + 1);
    incoming.set(edge.target, incoming.get(edge.target) + 1);
  }

  const communities = [];
  const communityById = new Map();
  for (const node of graph.nodes) {
    if (!communityById.has(node.community)) {
      const palette = colors[communities.length % colors.length];
      const community = { id: node.community, name: node.communityName, nodes: [], ...palette };
      communities.push(community);
      communityById.set(node.community, community);
    }
    communityById.get(node.community).nodes.push(node);
  }

  const relationCounts = new Map();
  for (const edge of graph.edges) relationCounts.set(edge.relation, (relationCounts.get(edge.relation) || 0) + 1);
  const relationOrder = [...relationCounts].sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]));

  const state = {
    x: 0,
    y: 0,
    scale: 1,
    fitScale: 1,
    selectedId: null,
    focusedId: null,
    hoveredId: null,
    searchMatches: new Set(),
    searchIndex: 0,
    activeRelation: null,
    visibleCommunities: new Set(communities.map((community) => community.id)),
    visibleConfidence: new Set(['EXTRACTED', 'INFERRED', 'AMBIGUOUS']),
    pointer: null,
  };

  const positions = new Map();
  const nodeElements = new Map();
  const edgeElements = new Map();
  const laneElements = new Map();

  const svgElement = (tag, attributes = {}) => {
    const element = document.createElementNS(SVG_NS, tag);
    for (const [name, value] of Object.entries(attributes)) element.setAttribute(name, String(value));
    return element;
  };

  const setText = (id, value) => { document.getElementById(id).textContent = String(value); };
  const normalizeRelation = (relation) => relation.replace(/_/gu, ' ');
  const nodeWidth = (node) => Math.max(132, Math.min(190, 74 + node.label.length * 5.3));
  const nodeHeight = 48;
  const flowRatio = (node) => {
    const into = incoming.get(node.id);
    const out = outgoing.get(node.id);
    if (into + out === 0) return .5;
    return into / (into + out);
  };
  const isHub = (node) => node.degree >= 5;

  function calculateLayout() {
    const availableHeight = WORLD.height - WORLD.top - WORLD.bottom;
    const laneHeight = (availableHeight - LANE_GAP * (communities.length - 1)) / communities.length;
    communities.forEach((community, laneIndex) => {
      community.top = WORLD.top + laneIndex * (laneHeight + LANE_GAP);
      community.height = laneHeight;
      const ordered = [...community.nodes].sort((a, b) => flowRatio(a) - flowRatio(b) || b.degree - a.degree || a.label.localeCompare(b.label));
      ordered.forEach((node, index) => {
        const sequence = ordered.length === 1 ? .5 : index / (ordered.length - 1);
        const semantic = flowRatio(node);
        const xRatio = .68 * sequence + .32 * semantic;
        const stagger = index % 3 === 0 ? -22 : index % 3 === 1 ? 18 : 0;
        positions.set(node.id, {
          x: FLOW_LEFT + xRatio * (FLOW_RIGHT - FLOW_LEFT),
          y: community.top + community.height / 2 + stagger,
          laneIndex,
        });
      });
    });
  }

  function edgePath(edge) {
    const source = positions.get(edge.source);
    const target = positions.get(edge.target);
    const sourceNode = nodesById.get(edge.source);
    const targetNode = nodesById.get(edge.target);
    const sourceOffset = Math.min(nodeWidth(sourceNode) / 2, Math.abs(target.x - source.x) / 3);
    const targetOffset = Math.min(nodeWidth(targetNode) / 2, Math.abs(target.x - source.x) / 3);
    const direction = target.x >= source.x ? 1 : -1;
    const sx = source.x + direction * sourceOffset;
    const tx = target.x - direction * targetOffset;
    const dy = Math.abs(target.y - source.y);
    const bend = Math.max(44, Math.abs(tx - sx) * .42, dy * .24);
    return `M ${sx} ${source.y} C ${sx + direction * bend} ${source.y}, ${tx - direction * bend} ${target.y}, ${tx} ${target.y}`;
  }

  function edgeMidpoint(edge) {
    const source = positions.get(edge.source);
    const target = positions.get(edge.target);
    return { x: (source.x + target.x) / 2, y: (source.y + target.y) / 2 - 5 };
  }

  function renderLanes() {
    elements.laneLayer.replaceChildren();
    communities.forEach((community, index) => {
      const group = svgElement('g', { class: 'lane', 'data-community': community.id });
      const surface = svgElement('rect', {
        class: 'lane-surface', x: WORLD.left, y: community.top, width: WORLD.width - WORLD.left - WORLD.right,
        height: community.height, rx: 11, fill: community.tint, stroke: community.color, 'fill-opacity': .46, 'stroke-opacity': .2,
      });
      const baseline = svgElement('line', {
        class: 'lane-baseline', x1: FLOW_LEFT, y1: community.top + community.height / 2,
        x2: FLOW_RIGHT, y2: community.top + community.height / 2,
      });
      const laneRule = svgElement('line', {
        class: 'lane-rule', x1: 54, y1: community.top + 27, x2: 54, y2: community.top + community.height - 27,
        stroke: community.color,
      });
      const number = svgElement('text', { class: 'lane-number', x: 75, y: community.top + 38 });
      number.textContent = String(index + 1).padStart(2, '0');
      const name = svgElement('text', { class: 'lane-name', x: 75, y: community.top + 68 });
      name.textContent = community.name;
      const count = svgElement('text', { class: 'lane-count', x: 75, y: community.top + 89 });
      count.textContent = `${community.nodes.length} LANDMARKS`;
      group.append(surface, baseline, laneRule, number, name, count);
      elements.laneLayer.append(group);
      laneElements.set(community.id, group);
    });
  }

  function renderEdges() {
    elements.edgeLayer.replaceChildren();
    graph.edges.forEach((edge, index) => {
      const group = svgElement('g', { class: 'edge-group', 'data-edge-index': index });
      const path = svgElement('path', {
        class: `edge relation-${edge.relation} confidence-${edge.confidence}`,
        d: edgePath(edge),
        'data-source': edge.source,
        'data-target': edge.target,
        'aria-hidden': 'true',
      });
      const midpoint = edgeMidpoint(edge);
      const labelText = normalizeRelation(edge.relation);
      const labelWidth = 13 + labelText.length * 5;
      const labelGroup = svgElement('g', { class: 'edge-label-group', transform: `translate(${midpoint.x} ${midpoint.y})` });
      const labelBg = svgElement('rect', { class: 'edge-label-bg', x: -labelWidth / 2, y: -8, width: labelWidth, height: 15, rx: 7 });
      const label = svgElement('text', { class: 'edge-label', x: 0, y: 3, 'text-anchor': 'middle' });
      label.textContent = labelText;
      labelGroup.append(labelBg, label);
      group.append(path, labelGroup);
      elements.edgeLayer.append(group);
      edgeElements.set(index, { group, path, labelGroup, edge });
    });
  }

  function nodeType(node) {
    if (node.kind === 'database') return { glyph: 'DB', label: 'data' };
    if (node.kind === 'config') return { glyph: '◇', label: 'config' };
    if (node.kind === 'template') return { glyph: 'TXT', label: 'template' };
    return { glyph: '{ }', label: 'code' };
  }

  function renderNodes() {
    elements.nodeLayer.replaceChildren();
    graph.nodes.forEach((node) => {
      const position = positions.get(node.id);
      const community = communityById.get(node.community);
      const width = nodeWidth(node);
      const type = nodeType(node);
      const group = svgElement('g', {
        class: `graph-node${isHub(node) ? ' is-hub' : ''}`,
        transform: `translate(${position.x} ${position.y})`,
        tabindex: '0', role: 'treeitem',
        'aria-label': `${node.label}, ${type.label}, ${node.degree} connections, ${community.name}`,
        'aria-selected': 'false',
        'data-node-id': node.id,
        style: `--domain-color:${community.color}`,
      });
      if (isHub(node)) {
        group.append(svgElement('rect', { class: 'hub-ring', x: -width / 2 - 5, y: -nodeHeight / 2 - 5, width: width + 10, height: nodeHeight + 10, rx: 12, stroke: community.color }));
      }
      const card = svgElement('rect', { class: 'node-card', x: -width / 2, y: -nodeHeight / 2, width, height: nodeHeight, rx: type.label === 'data' ? 20 : type.label === 'config' ? 3 : 8 });
      const accent = svgElement('rect', { class: 'node-accent', x: -width / 2, y: -nodeHeight / 2, width: 5, height: nodeHeight, rx: 2, fill: community.color });
      const typeBg = svgElement(type.label === 'config' ? 'polygon' : 'rect', type.label === 'config'
        ? { class: 'node-type-bg', points: `${-width / 2 + 22},-11 ${-width / 2 + 33},0 ${-width / 2 + 22},11 ${-width / 2 + 11},0` }
        : { class: 'node-type-bg', x: -width / 2 + 10, y: -11, width: 25, height: 22, rx: type.label === 'data' ? 11 : 4 });
      const icon = svgElement('text', { class: 'node-icon', x: -width / 2 + 22.5, y: 3 });
      icon.textContent = type.glyph;
      const label = svgElement('text', { class: 'node-label', x: -width / 2 + 43, y: -3 });
      label.textContent = node.label;
      const meta = svgElement('text', { class: 'node-meta', x: -width / 2 + 43, y: 11 });
      meta.textContent = node.file.split('/').pop();
      const degree = svgElement('text', { class: 'node-degree', x: width / 2 - 9, y: 11 });
      degree.textContent = String(node.degree).padStart(2, '0');
      const hit = svgElement('rect', { class: 'node-hit', x: -width / 2 - 5, y: -nodeHeight / 2 - 5, width: width + 10, height: nodeHeight + 10, rx: 10 });
      group.append(card, accent, typeBg, icon, label, meta, degree, hit);
      group.addEventListener('click', (event) => { event.stopPropagation(); selectNode(node.id, false); });
      group.addEventListener('dblclick', (event) => { event.stopPropagation(); selectNode(node.id, true); announce(`Open source: ${node.file}:${node.location}`); });
      group.addEventListener('focus', () => { state.focusedId = node.id; updateGraphClasses(); });
      group.addEventListener('mouseenter', () => { state.hoveredId = node.id; updateGraphClasses(); });
      group.addEventListener('mouseleave', () => { state.hoveredId = null; updateGraphClasses(); });
      group.addEventListener('keydown', (event) => handleNodeKeydown(event, node.id));
      elements.nodeLayer.append(group);
      nodeElements.set(node.id, group);
    });
  }

  function renderMinimap() {
    elements.minimapContent.replaceChildren();
    for (const community of communities) {
      elements.minimapContent.append(svgElement('rect', {
        class: 'minimap-lane', x: WORLD.left, y: community.top, width: WORLD.width - WORLD.left - WORLD.right,
        height: community.height, rx: 18, fill: community.tint,
      }));
    }
    for (const node of graph.nodes) {
      const position = positions.get(node.id);
      const community = communityById.get(node.community);
      elements.minimapContent.append(svgElement('circle', { class: 'minimap-node', cx: position.x, cy: position.y, r: isHub(node) ? 13 : 8, fill: community.color }));
    }
  }

  function renderFilters() {
    elements.domainFilters.replaceChildren();
    for (const community of communities) {
      const button = document.createElement('button');
      button.type = 'button';
      button.className = 'domain-button';
      button.dataset.community = community.id;
      button.setAttribute('aria-pressed', 'true');
      const swatch = document.createElement('i');
      swatch.className = 'domain-swatch';
      swatch.style.background = community.color;
      swatch.style.color = community.color;
      const name = document.createElement('span');
      name.textContent = community.name;
      const count = document.createElement('small');
      count.textContent = community.nodes.length;
      button.append(swatch, name, count);
      button.addEventListener('click', () => toggleCommunity(community.id, button));
      elements.domainFilters.append(button);
    }

    elements.relationFilters.replaceChildren();
    const all = document.createElement('button');
    all.type = 'button';
    all.className = 'relation-chip';
    all.dataset.relation = '';
    all.setAttribute('aria-pressed', 'true');
    all.textContent = 'all';
    all.addEventListener('click', () => selectRelation(null));
    elements.relationFilters.append(all);
    for (const [relation, count] of relationOrder) {
      const button = document.createElement('button');
      button.type = 'button';
      button.className = 'relation-chip';
      button.dataset.relation = relation;
      button.setAttribute('aria-pressed', 'false');
      button.append(document.createTextNode(normalizeRelation(relation)));
      const tally = document.createElement('small');
      tally.textContent = count;
      button.append(tally);
      button.addEventListener('click', () => selectRelation(relation));
      elements.relationFilters.append(button);
    }
  }

  function currentRelatedIds() {
    const anchor = state.selectedId || state.hoveredId || state.focusedId;
    if (!anchor) return null;
    const ids = new Set([anchor]);
    for (const connection of adjacency.get(anchor) || []) ids.add(connection.node.id);
    return ids;
  }

  function edgeIsVisible(edge) {
    return state.visibleCommunities.has(nodesById.get(edge.source).community)
      && state.visibleCommunities.has(nodesById.get(edge.target).community)
      && state.visibleConfidence.has(edge.confidence)
      && (!state.activeRelation || edge.relation === state.activeRelation);
  }

  function nodeIsVisible(node) {
    if (!state.visibleCommunities.has(node.community)) return false;
    if (!state.activeRelation) return true;
    return graph.edges.some((edge) => edge.relation === state.activeRelation && (edge.source === node.id || edge.target === node.id) && edgeIsVisible(edge));
  }

  function updateGraphClasses() {
    const relatedIds = currentRelatedIds();
    const activeAnchor = state.selectedId || state.hoveredId || state.focusedId;
    let visibleNodes = 0;
    let visibleEdges = 0;

    for (const node of graph.nodes) {
      const element = nodeElements.get(node.id);
      const visible = nodeIsVisible(node);
      if (visible) visibleNodes += 1;
      element.style.display = visible ? '' : 'none';
      element.classList.toggle('is-selected', state.selectedId === node.id);
      element.classList.toggle('is-muted', Boolean(relatedIds && !relatedIds.has(node.id)));
      element.classList.toggle('is-match', state.searchMatches.has(node.id));
      element.setAttribute('aria-selected', String(state.selectedId === node.id));
      element.setAttribute('aria-hidden', String(!visible));
    }

    for (const [index, entry] of edgeElements) {
      const visible = edgeIsVisible(entry.edge);
      if (visible) visibleEdges += 1;
      entry.group.style.display = visible ? '' : 'none';
      const related = Boolean(activeAnchor && (entry.edge.source === activeAnchor || entry.edge.target === activeAnchor));
      const searchRelated = state.searchMatches.has(entry.edge.source) && state.searchMatches.has(entry.edge.target);
      entry.path.classList.toggle('is-related', related);
      entry.path.classList.toggle('is-search-related', !activeAnchor && searchRelated);
      entry.path.classList.toggle('is-muted', Boolean(activeAnchor && !related));
      entry.labelGroup.classList.toggle('is-related', related);
      entry.group.dataset.visible = String(visible);
      edgeElements.set(index, entry);
    }

    for (const community of communities) {
      laneElements.get(community.id).classList.toggle('lane-collapsed', !state.visibleCommunities.has(community.id));
    }

    elements.visibleStatus.textContent = `${visibleNodes} of ${graph.nodes.length} nodes · ${visibleEdges} relations visible`;
  }

  function applyTransform() {
    elements.world.setAttribute('transform', `translate(${state.x} ${state.y}) scale(${state.scale})`);
    const ratio = state.scale / state.fitScale;
    const semanticClass = ratio < 1.22 ? 'semantic-overview' : ratio < 1.86 ? 'semantic-structure' : 'semantic-detail';
    elements.svg.classList.remove('semantic-overview', 'semantic-structure', 'semantic-detail');
    elements.svg.classList.add(semanticClass);
    elements.zoomOutput.value = `${Math.round(ratio * 100)}%`;
    document.querySelectorAll('[data-view]').forEach((button) => {
      const active = button.dataset.view === semanticClass.replace('semantic-', '');
      button.setAttribute('aria-pressed', String(active));
    });
    updateMinimapViewport();
  }

  function fitGraph() {
    const bounds = elements.stage.getBoundingClientRect();
    if (bounds.width <= 0 || bounds.height <= 0) return;
    const paddingX = 42;
    const paddingY = 48;
    state.fitScale = Math.min((bounds.width - paddingX * 2) / WORLD.width, (bounds.height - paddingY * 2) / WORLD.height);
    state.scale = state.fitScale;
    state.x = (bounds.width - WORLD.width * state.scale) / 2;
    state.y = (bounds.height - WORLD.height * state.scale) / 2 + 12;
    applyTransform();
  }

  function setView(level) {
    const ratios = { overview: 1, structure: 1.48, detail: 2.12 };
    const bounds = elements.stage.getBoundingClientRect();
    const centerWorld = screenToWorld(bounds.width / 2, bounds.height / 2);
    const nextScale = Math.min(MAX_SCALE, Math.max(MIN_SCALE, state.fitScale * ratios[level]));
    state.x = bounds.width / 2 - centerWorld.x * nextScale;
    state.y = bounds.height / 2 - centerWorld.y * nextScale;
    state.scale = nextScale;
    applyTransform();
  }

  function zoomAt(factor, screenX, screenY) {
    const worldPoint = screenToWorld(screenX, screenY);
    const nextScale = Math.min(MAX_SCALE, Math.max(MIN_SCALE, state.scale * factor));
    state.x = screenX - worldPoint.x * nextScale;
    state.y = screenY - worldPoint.y * nextScale;
    state.scale = nextScale;
    applyTransform();
  }

  function screenToWorld(x, y) {
    return { x: (x - state.x) / state.scale, y: (y - state.y) / state.scale };
  }

  function centerNode(id, detail = false) {
    const position = positions.get(id);
    const bounds = elements.stage.getBoundingClientRect();
    if (detail && state.scale < state.fitScale * 1.5) state.scale = Math.min(MAX_SCALE, state.fitScale * 1.85);
    state.x = bounds.width / 2 - position.x * state.scale;
    state.y = bounds.height / 2 - position.y * state.scale;
    applyTransform();
  }

  function updateMinimapViewport() {
    const bounds = elements.stage.getBoundingClientRect();
    const topLeft = screenToWorld(0, 0);
    elements.minimapViewport.setAttribute('x', String(Math.max(0, topLeft.x)));
    elements.minimapViewport.setAttribute('y', String(Math.max(0, topLeft.y)));
    elements.minimapViewport.setAttribute('width', String(Math.min(WORLD.width, bounds.width / state.scale)));
    elements.minimapViewport.setAttribute('height', String(Math.min(WORLD.height, bounds.height / state.scale)));
  }

  function selectNode(id, center) {
    const node = nodesById.get(id);
    if (!node || !nodeIsVisible(node)) return;
    state.selectedId = id;
    state.focusedId = id;
    updateInspector(node);
    updateGraphClasses();
    if (center) centerNode(id, true);
    announce(`${node.label} selected. ${node.degree} immediate connections.`);
  }

  function clearSelection() {
    state.selectedId = null;
    elements.inspector.classList.remove('has-selection');
    elements.emptyInspector.hidden = false;
    elements.nodeInspector.hidden = true;
    updateGraphClasses();
    elements.interactionHint.textContent = 'Drag to pan · scroll to zoom · select a node to trace dependencies';
  }

  function updateInspector(node) {
    const community = communityById.get(node.community);
    const connections = [...adjacency.get(node.id)].sort((a, b) => a.direction.localeCompare(b.direction) || a.node.label.localeCompare(b.node.label));
    elements.inspector.style.setProperty('--detail-color', community.color);
    elements.inspector.style.setProperty('--detail-tint', community.tint);
    elements.inspector.classList.add('has-selection');
    elements.emptyInspector.hidden = true;
    elements.nodeInspector.hidden = false;
    setText('detail-kind', `${nodeType(node).label} · ${community.name}`);
    setText('detail-label', node.label);
    setText('detail-id', node.id);
    setText('detail-file', node.file);
    setText('detail-location', node.location);
    setText('detail-domain', community.name);
    setText('detail-degree', node.degree);
    setText('detail-flow', `${incoming.get(node.id)} in · ${outgoing.get(node.id)} out`);
    setText('connection-count', connections.length);
    const container = document.getElementById('connections');
    container.replaceChildren();
    for (const connection of connections) {
      const button = document.createElement('button');
      button.type = 'button';
      button.className = 'connection';
      const direction = document.createElement('span');
      direction.className = 'direction';
      direction.textContent = connection.direction === 'out' ? '→' : '←';
      const identity = document.createElement('span');
      const label = document.createElement('strong');
      label.textContent = connection.node.label;
      const domain = document.createElement('small');
      domain.textContent = connection.node.communityName;
      identity.append(label, domain);
      const relation = document.createElement('em');
      relation.textContent = normalizeRelation(connection.edge.relation);
      button.append(direction, identity, relation);
      button.title = `${connection.edge.confidence.toLowerCase()} evidence`;
      button.addEventListener('click', () => selectNode(connection.node.id, true));
      container.append(button);
    }
    elements.interactionHint.textContent = `Tracing ${node.label} · ${connections.length} immediate connections`;
  }

  function toggleCommunity(id, button) {
    if (state.visibleCommunities.has(id)) {
      if (state.visibleCommunities.size === 1) return;
      state.visibleCommunities.delete(id);
    } else {
      state.visibleCommunities.add(id);
    }
    button.setAttribute('aria-pressed', String(state.visibleCommunities.has(id)));
    if (state.selectedId && !nodeIsVisible(nodesById.get(state.selectedId))) clearSelection();
    updateGraphClasses();
  }

  function showAllCommunities() {
    state.visibleCommunities = new Set(communities.map((community) => community.id));
    document.querySelectorAll('.domain-button').forEach((button) => button.setAttribute('aria-pressed', 'true'));
    updateGraphClasses();
  }

  function selectRelation(relation) {
    state.activeRelation = relation;
    document.querySelectorAll('.relation-chip').forEach((button) => button.setAttribute('aria-pressed', String((button.dataset.relation || null) === relation)));
    if (state.selectedId && !nodeIsVisible(nodesById.get(state.selectedId))) clearSelection();
    updateGraphClasses();
  }

  function toggleConfidence(confidence, button) {
    if (state.visibleConfidence.has(confidence)) {
      if (state.visibleConfidence.size === 1) return;
      state.visibleConfidence.delete(confidence);
    } else {
      state.visibleConfidence.add(confidence);
    }
    button.setAttribute('aria-pressed', String(state.visibleConfidence.has(confidence)));
    if (state.selectedId && !nodeIsVisible(nodesById.get(state.selectedId))) clearSelection();
    updateGraphClasses();
  }

  function searchNodes(query) {
    const terms = query.trim().toLocaleLowerCase().split(/\s+/u).filter(Boolean);
    if (terms.length === 0) return [];
    return graph.nodes
      .map((node) => {
        const label = node.label.toLocaleLowerCase();
        const id = node.id.toLocaleLowerCase();
        const file = node.file.toLocaleLowerCase();
        const domain = node.communityName.toLocaleLowerCase();
        let score = 0;
        for (const term of terms) {
          if (label === term) score += 100;
          else if (label.startsWith(term)) score += 45;
          else if (label.includes(term)) score += 24;
          if (id.includes(term)) score += 10;
          if (file.includes(term)) score += 5;
          if (domain.includes(term)) score += 3;
        }
        return { node, score };
      })
      .filter((entry) => entry.score > 0 && nodeIsVisible(entry.node))
      .sort((a, b) => b.score - a.score || b.node.degree - a.node.degree || a.node.label.localeCompare(b.node.label))
      .slice(0, 12)
      .map((entry) => entry.node);
  }

  function updateSearch() {
    const query = elements.search.value;
    const results = searchNodes(query);
    state.searchMatches = new Set(results.map((node) => node.id));
    state.searchIndex = 0;
    elements.searchBox.classList.toggle('has-query', Boolean(query));
    elements.searchCount.textContent = query ? String(results.length) : '';
    elements.search.setAttribute('aria-expanded', String(results.length > 0));
    elements.searchResults.replaceChildren();
    elements.searchResults.hidden = results.length === 0;
    results.forEach((node, index) => {
      const community = communityById.get(node.community);
      const button = document.createElement('button');
      button.type = 'button';
      button.className = 'search-result';
      button.setAttribute('role', 'option');
      button.setAttribute('aria-selected', String(index === 0));
      button.style.setProperty('--result-color', community.color);
      const dot = document.createElement('i');
      const identity = document.createElement('span');
      const label = document.createElement('strong');
      label.textContent = node.label;
      const file = document.createElement('small');
      file.textContent = `${node.file}:${node.location}`;
      identity.append(label, file);
      const domain = document.createElement('em');
      domain.textContent = community.name;
      button.append(dot, identity, domain);
      button.addEventListener('click', () => chooseSearchResult(node));
      elements.searchResults.append(button);
    });
    updateGraphClasses();
  }

  function chooseSearchResult(node) {
    elements.search.value = node.label;
    elements.searchResults.hidden = true;
    elements.search.setAttribute('aria-expanded', 'false');
    state.searchMatches = new Set([node.id]);
    selectNode(node.id, true);
  }

  function moveSearchIndex(delta) {
    const buttons = [...elements.searchResults.querySelectorAll('.search-result')];
    if (buttons.length === 0) return;
    state.searchIndex = (state.searchIndex + delta + buttons.length) % buttons.length;
    buttons.forEach((button, index) => button.setAttribute('aria-selected', String(index === state.searchIndex)));
    buttons[state.searchIndex].scrollIntoView({ block: 'nearest' });
  }

  function handleNodeKeydown(event, id) {
    if (event.key === 'Enter' || event.key === ' ') {
      event.preventDefault();
      selectNode(id, false);
      return;
    }
    const direction = { ArrowLeft: [-1, 0], ArrowRight: [1, 0], ArrowUp: [0, -1], ArrowDown: [0, 1] }[event.key];
    if (!direction) return;
    event.preventDefault();
    const origin = positions.get(id);
    const candidates = graph.nodes.filter((node) => node.id !== id && nodeIsVisible(node)).map((node) => {
      const position = positions.get(node.id);
      const dx = position.x - origin.x;
      const dy = position.y - origin.y;
      const projection = dx * direction[0] + dy * direction[1];
      const cross = Math.abs(dx * direction[1] - dy * direction[0]);
      return { node, projection, distance: projection + cross * 1.7 };
    }).filter((candidate) => candidate.projection > 8).sort((a, b) => a.distance - b.distance);
    const next = candidates[0]?.node;
    if (next) {
      state.focusedId = next.id;
      nodeElements.get(next.id).focus();
      centerNode(next.id, false);
    }
  }

  function announce(message) {
    elements.interactionHint.textContent = message;
  }

  function bindInteractions() {
    elements.svg.addEventListener('click', (event) => { if (event.target.closest('.graph-node')) return; clearSelection(); });
    elements.svg.addEventListener('pointerdown', (event) => {
      if (event.button !== 0 || event.target.closest('.graph-node')) return;
      state.pointer = { id: event.pointerId, x: event.clientX, y: event.clientY, startX: state.x, startY: state.y };
      elements.svg.setPointerCapture(event.pointerId);
      elements.svg.classList.add('is-panning');
    });
    elements.svg.addEventListener('pointermove', (event) => {
      if (!state.pointer || state.pointer.id !== event.pointerId) return;
      state.x = state.pointer.startX + event.clientX - state.pointer.x;
      state.y = state.pointer.startY + event.clientY - state.pointer.y;
      applyTransform();
    });
    const endPan = (event) => {
      if (!state.pointer || state.pointer.id !== event.pointerId) return;
      state.pointer = null;
      elements.svg.releasePointerCapture(event.pointerId);
      elements.svg.classList.remove('is-panning');
    };
    elements.svg.addEventListener('pointerup', endPan);
    elements.svg.addEventListener('pointercancel', endPan);
    elements.svg.addEventListener('wheel', (event) => {
      event.preventDefault();
      const bounds = elements.stage.getBoundingClientRect();
      zoomAt(Math.exp(-event.deltaY * .00115), event.clientX - bounds.left, event.clientY - bounds.top);
    }, { passive: false });

    elements.minimap.addEventListener('click', (event) => {
      const box = elements.minimap.getBoundingClientRect();
      const worldX = (event.clientX - box.left) / box.width * WORLD.width;
      const worldY = (event.clientY - box.top) / box.height * WORLD.height;
      const stage = elements.stage.getBoundingClientRect();
      state.x = stage.width / 2 - worldX * state.scale;
      state.y = stage.height / 2 - worldY * state.scale;
      applyTransform();
    });

    elements.search.addEventListener('input', updateSearch);
    elements.search.addEventListener('keydown', (event) => {
      if (event.key === 'ArrowDown') { event.preventDefault(); moveSearchIndex(1); }
      if (event.key === 'ArrowUp') { event.preventDefault(); moveSearchIndex(-1); }
      if (event.key === 'Enter') {
        const results = searchNodes(elements.search.value);
        if (results[state.searchIndex]) { event.preventDefault(); chooseSearchResult(results[state.searchIndex]); }
      }
      if (event.key === 'Escape') {
        elements.search.value = '';
        updateSearch();
        elements.search.blur();
      }
    });

    document.querySelectorAll('[data-view]').forEach((button) => button.addEventListener('click', () => setView(button.dataset.view)));
    elements.confidenceFilters.querySelectorAll('button').forEach((button) => button.addEventListener('click', () => toggleConfidence(button.dataset.confidence, button)));
    document.getElementById('show-all-domains').addEventListener('click', showAllCommunities);
    document.getElementById('zoom-in').addEventListener('click', () => {
      const box = elements.stage.getBoundingClientRect(); zoomAt(1.24, box.width / 2, box.height / 2);
    });
    document.getElementById('zoom-out').addEventListener('click', () => {
      const box = elements.stage.getBoundingClientRect(); zoomAt(1 / 1.24, box.width / 2, box.height / 2);
    });
    document.getElementById('fit').addEventListener('click', fitGraph);
    document.getElementById('inspector-close').addEventListener('click', clearSelection);
    document.getElementById('source-card').addEventListener('click', () => {
      if (!state.selectedId) return;
      const node = nodesById.get(state.selectedId);
      announce(`Prototype source action: ${node.file}:${node.location}`);
    });
    document.getElementById('help-button').addEventListener('click', () => elements.helpDialog.showModal());

    document.addEventListener('pointerdown', (event) => {
      if (!elements.searchResults.contains(event.target) && event.target !== elements.search) {
        elements.searchResults.hidden = true;
        elements.search.setAttribute('aria-expanded', 'false');
      }
    });
    document.addEventListener('keydown', (event) => {
      const tag = event.target.tagName?.toLocaleLowerCase();
      const typing = tag === 'input' || tag === 'textarea' || event.target.isContentEditable;
      if (!typing && (event.key === 'f' || event.key === '/')) {
        event.preventDefault();
        elements.search.focus();
      } else if (!typing && (event.key === '+' || event.key === '=')) {
        const box = elements.stage.getBoundingClientRect(); zoomAt(1.24, box.width / 2, box.height / 2);
      } else if (!typing && event.key === '-') {
        const box = elements.stage.getBoundingClientRect(); zoomAt(1 / 1.24, box.width / 2, box.height / 2);
      } else if (!typing && event.key === '0') {
        fitGraph();
      } else if (!typing && event.key === '?') {
        elements.helpDialog.showModal();
      } else if (event.key === 'Escape' && !elements.helpDialog.open) {
        clearSelection();
      }
    });

    const observer = new ResizeObserver(() => fitGraph());
    observer.observe(elements.stage);
  }

  function initialize() {
    calculateLayout();
    renderLanes();
    renderEdges();
    renderNodes();
    renderMinimap();
    renderFilters();
    bindInteractions();
    setText('commit', graph.builtAtCommit);
    setText('node-total', graph.nodes.length);
    setText('edge-total', graph.edges.length);
    setText('relation-total', relationCounts.size);
    updateGraphClasses();
    requestAnimationFrame(fitGraph);
  }

  initialize();
})();
