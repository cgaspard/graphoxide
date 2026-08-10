(function investigationLens() {
  'use strict';

  const graph = globalThis.GRAPHOXIDE_GRAPH_FIXTURE;
  const root = document.documentElement;
  const graphSurface = document.getElementById('graphSurface');
  const graphViewport = document.getElementById('graphViewport');
  const loadingState = document.getElementById('loadingState');
  const emptyState = document.getElementById('emptyState');
  const inspector = document.getElementById('inspector');
  const inspectorContent = document.getElementById('inspectorContent');
  const historyTrail = document.getElementById('historyTrail');
  const searchInput = document.getElementById('searchInput');
  const searchResults = document.getElementById('searchResults');
  const scenarioSelect = document.getElementById('scenarioSelect');
  const traceButton = document.getElementById('traceButton');
  const traceLabel = document.getElementById('traceLabel');
  const clearTraceButton = document.getElementById('clearTraceButton');
  const shortcutDialog = document.getElementById('shortcutDialog');
  const announcer = document.getElementById('announcer');

  if (!graph || graph.contractVersion !== 1) {
    graphSurface.innerHTML = '<p class="search-empty">The shared graph fixture could not be loaded.</p>';
    return;
  }

  const nodes = graph.nodes;
  const edges = graph.edges;
  const nodeById = new Map(nodes.map((node) => [node.id, node]));
  const outgoing = new Map(nodes.map((node) => [node.id, []]));
  const incoming = new Map(nodes.map((node) => [node.id, []]));
  for (const edge of edges) {
    outgoing.get(edge.source)?.push(edge);
    incoming.get(edge.target)?.push(edge);
  }

  const state = {
    focusId: 'checkout-service',
    selectedId: 'checkout-service',
    depth: 2,
    trace: false,
    scenario: 'typical',
    history: ['checkout-service'],
    historyIndex: 0,
    expandedColumns: new Set(),
    searchIndex: -1,
    searchMatches: [],
  };

  let visibleNodeIds = new Set();
  let visibleEdges = [];
  let distanceState = { upstream: new Map(), downstream: new Map() };
  let redrawFrame = 0;

  const escapeHtml = (value) => String(value).replace(/[&<>"']/g, (character) => ({
    '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;',
  })[character]);

  function relationshipClass(relation) {
    if (relation === 'reads') return 'reads';
    if (relation === 'writes' || relation === 'creates') return 'writes';
    if (relation === 'publishes' || relation === 'dispatches') return 'events';
    return 'calls';
  }

  function kindGlyph(kind) {
    return ({ code: 'ƒ', database: 'DB', config: '{}', template: '</>' })[kind] || '•';
  }

  function confidenceLabel(confidence) {
    return ({ EXTRACTED: 'Verified', INFERRED: 'Inferred', AMBIGUOUS: 'Ambiguous' })[confidence] || confidence;
  }

  function confidenceGlyph(confidence) {
    return ({ EXTRACTED: '✓', INFERRED: '≈', AMBIGUOUS: '?' })[confidence] || '·';
  }

  function breadthFirstDistances(startId, direction, maximumDepth) {
    const distances = new Map([[startId, 0]]);
    const queue = [startId];
    while (queue.length) {
      const current = queue.shift();
      const currentDepth = distances.get(current);
      if (currentDepth >= maximumDepth) continue;
      const candidates = direction === 'upstream' ? incoming.get(current) : outgoing.get(current);
      for (const edge of candidates || []) {
        const next = direction === 'upstream' ? edge.source : edge.target;
        if (distances.has(next)) continue;
        distances.set(next, currentDepth + 1);
        queue.push(next);
      }
    }
    distances.delete(startId);
    return distances;
  }

  function edgeTowardFocus(nodeId, direction, distance, distances) {
    if (direction === 'upstream') {
      return (outgoing.get(nodeId) || []).find((edge) => {
        if (distance === 1) return edge.target === state.focusId;
        return distances.get(edge.target) === distance - 1;
      });
    }
    return (incoming.get(nodeId) || []).find((edge) => {
      if (distance === 1) return edge.source === state.focusId;
      return distances.get(edge.source) === distance - 1;
    });
  }

  function sortedNodesAtDistance(distances, distance, excluded = new Set()) {
    return [...distances.entries()]
      .filter(([id, value]) => value === distance && !excluded.has(id))
      .map(([id]) => nodeById.get(id))
      .filter(Boolean)
      .sort((left, right) => right.degree - left.degree || left.label.localeCompare(right.label));
  }

  function columnTitle(direction, distance) {
    if (direction === 'upstream') {
      if (distance === 1) return ['Direct callers', 'flow into focus'];
      return [`${distance} hops upstream`, 'earlier in the flow'];
    }
    if (distance === 1) return ['Direct effects', 'called or changed next'];
    return [`${distance} hops affected`, 'later in the flow'];
  }

  function nodeCardMarkup(node, direction, distance, edge) {
    const relation = edge?.relation || 'related';
    const confidence = edge?.confidence || 'INFERRED';
    const selected = node.id === state.selectedId;
    const ariaDirection = direction === 'upstream' ? 'upstream of' : 'downstream from';
    return `<button class="node-card${selected ? ' is-selected' : ''}" type="button"
      data-node-id="${escapeHtml(node.id)}" data-kind="${escapeHtml(node.kind)}"
      data-direction="${direction}" data-distance="${distance}"
      aria-label="${escapeHtml(node.label)}, ${distance} hop ${ariaDirection} ${nodeById.get(state.focusId).label}, ${relation}, ${confidenceLabel(confidence)}">
      <span class="node-kind" aria-hidden="true">${escapeHtml(kindGlyph(node.kind))}</span>
      <span class="node-main"><strong>${escapeHtml(node.label)}</strong><small>${escapeHtml(node.file)}:${escapeHtml(node.location)}</small></span>
      <span class="node-meta" aria-hidden="true"><span class="relation-chip">${escapeHtml(relation.replaceAll('_', ' '))}</span><span class="confidence-mark ${confidence.toLowerCase()}">${confidenceGlyph(confidence)}</span></span>
    </button>`;
  }

  function flowColumnMarkup(direction, distance, columnNodes, distances, columnIndex) {
    const [title, subtitle] = columnTitle(direction, distance);
    const columnKey = `${direction}:${distance}`;
    const normalLimit = state.scenario === 'dense' ? 9 : 5;
    const expanded = state.expandedColumns.has(columnKey);
    const shownNodes = expanded ? columnNodes : columnNodes.slice(0, normalLimit);
    const hiddenCount = columnNodes.length - shownNodes.length;
    const cards = shownNodes.map((node) => nodeCardMarkup(
      node,
      direction,
      distance,
      edgeTowardFocus(node.id, direction, distance, distances),
    )).join('');
    const overflow = hiddenCount > 0
      ? `<button class="overflow-card" type="button" data-expand-column="${columnKey}">+ ${hiddenCount} more at this depth</button>`
      : (expanded && columnNodes.length > normalLimit
        ? `<button class="overflow-card" type="button" data-expand-column="${columnKey}">Show fewer</button>`
        : '');
    return `<section class="flow-column direction-${direction}" data-column-index="${columnIndex}" aria-label="${title}">
      <header class="column-heading"><div><strong>${title}</strong><span>${subtitle}</span></div><span class="column-count">${columnNodes.length}</span></header>
      <div class="node-stack">${cards || '<span class="search-empty">No symbols</span>'}${overflow}</div>
    </section>`;
  }

  function focusColumnMarkup(columnIndex) {
    const node = nodeById.get(state.focusId);
    const directIn = (incoming.get(node.id) || []).length;
    const directOut = (outgoing.get(node.id) || []).length;
    return `<section class="flow-column direction-focus" data-column-index="${columnIndex}" aria-label="Current focus">
      <header class="column-heading"><div><strong>Current focus</strong><span>investigation anchor</span></div><span class="column-count">1</span></header>
      <div class="node-stack">
        <button class="focus-card" type="button" data-node-id="${escapeHtml(node.id)}" data-direction="focus" data-distance="0" aria-label="Current focus, ${escapeHtml(node.label)}">
          <span class="focus-node-icon" aria-hidden="true">${escapeHtml(kindGlyph(node.kind))}</span>
          <span class="focus-community">${escapeHtml(node.communityName)}</span>
          <h2>${escapeHtml(node.label)}</h2>
          <span class="focus-file">${escapeHtml(node.file)}:${escapeHtml(node.location)}</span>
          <span class="focus-metrics" aria-label="${directIn} incoming, ${directOut} outgoing, degree ${node.degree}">
            <span><b>${directIn}</b>incoming</span><span><b>${directOut}</b>outgoing</span><span><b>${node.degree}</b>degree</span>
          </span>
        </button>
      </div>
    </section>`;
  }

  function renderGraph({ center = false } = {}) {
    const upstream = breadthFirstDistances(state.focusId, 'upstream', state.depth);
    const downstream = breadthFirstDistances(state.focusId, 'downstream', state.depth);
    distanceState = { upstream, downstream };

    const excludedFromDownstream = new Set(
      [...downstream.keys()].filter((id) => upstream.has(id) && upstream.get(id) <= downstream.get(id)),
    );
    const columns = [];
    let columnIndex = 0;
    for (let distance = state.depth; distance >= 1; distance -= 1) {
      const columnNodes = sortedNodesAtDistance(upstream, distance);
      columns.push(flowColumnMarkup('upstream', distance, columnNodes, upstream, columnIndex));
      columnIndex += 1;
    }
    columns.push(focusColumnMarkup(columnIndex));
    columnIndex += 1;
    for (let distance = 1; distance <= state.depth; distance += 1) {
      const columnNodes = sortedNodesAtDistance(downstream, distance, excludedFromDownstream);
      columns.push(flowColumnMarkup('downstream', distance, columnNodes, downstream, columnIndex));
      columnIndex += 1;
    }

    graphSurface.className = `graph-surface depth-${state.depth}${state.scenario === 'dense' ? ' dense' : ''}`;
    graphSurface.innerHTML = `<svg class="connection-layer" aria-hidden="true"></svg>${columns.join('')}`;
    visibleNodeIds = new Set([state.focusId]);
    graphSurface.querySelectorAll('[data-node-id]').forEach((element) => visibleNodeIds.add(element.dataset.nodeId));
    visibleEdges = edges.filter((edge) => visibleNodeIds.has(edge.source) && visibleNodeIds.has(edge.target));

    bindGraphInteractions();
    updateNodeStates();
    updateGraphStats();
    scheduleConnections();
    if (center) requestAnimationFrame(centerOnFocus);
  }

  function bindGraphInteractions() {
    graphSurface.querySelectorAll('[data-node-id]').forEach((button) => {
      button.addEventListener('click', () => selectNode(button.dataset.nodeId));
      button.addEventListener('dblclick', () => setFocus(button.dataset.nodeId));
      button.addEventListener('keydown', handleGraphKeydown);
    });
    graphSurface.querySelectorAll('[data-expand-column]').forEach((button) => {
      button.addEventListener('click', () => {
        const key = button.dataset.expandColumn;
        if (state.expandedColumns.has(key)) state.expandedColumns.delete(key);
        else state.expandedColumns.add(key);
        renderGraph();
      });
    });
  }

  function handleGraphKeydown(event) {
    const button = event.currentTarget;
    if (event.key === 'Enter') {
      event.preventDefault();
      setFocus(button.dataset.nodeId);
      return;
    }
    if (!['ArrowLeft', 'ArrowRight', 'ArrowUp', 'ArrowDown'].includes(event.key)) return;
    event.preventDefault();
    const column = button.closest('.flow-column');
    const columns = [...graphSurface.querySelectorAll('.flow-column')];
    const currentColumnIndex = columns.indexOf(column);
    const currentButtons = [...column.querySelectorAll('[data-node-id]')];
    const rowIndex = Math.max(0, currentButtons.indexOf(button));
    let target = null;
    if (event.key === 'ArrowUp' || event.key === 'ArrowDown') {
      const delta = event.key === 'ArrowUp' ? -1 : 1;
      target = currentButtons[(rowIndex + delta + currentButtons.length) % currentButtons.length];
    } else {
      const delta = event.key === 'ArrowLeft' ? -1 : 1;
      let nextColumnIndex = currentColumnIndex + delta;
      while (nextColumnIndex >= 0 && nextColumnIndex < columns.length) {
        const candidates = [...columns[nextColumnIndex].querySelectorAll('[data-node-id]')];
        if (candidates.length) {
          target = candidates[Math.min(rowIndex, candidates.length - 1)];
          break;
        }
        nextColumnIndex += delta;
      }
    }
    if (target) {
      selectNode(target.dataset.nodeId, { moveFocus: false });
      target.focus({ preventScroll: true });
      target.scrollIntoView({ block: 'nearest', inline: 'nearest', behavior: reducedMotion() ? 'auto' : 'smooth' });
    }
  }

  function centerOnFocus() {
    const focus = graphSurface.querySelector(`[data-node-id="${CSS.escape(state.focusId)}"]`);
    if (!focus) return;
    const destination = focus.offsetLeft + focus.offsetWidth / 2 - graphViewport.clientWidth / 2;
    graphViewport.scrollTo({ left: Math.max(0, destination), behavior: reducedMotion() ? 'auto' : 'smooth' });
  }

  function scheduleConnections() {
    cancelAnimationFrame(redrawFrame);
    redrawFrame = requestAnimationFrame(drawConnections);
  }

  function drawConnections() {
    const svg = graphSurface.querySelector('.connection-layer');
    if (!svg) return;
    const surfaceRect = graphSurface.getBoundingClientRect();
    svg.replaceChildren();
    svg.setAttribute('viewBox', `0 0 ${graphSurface.offsetWidth} ${graphSurface.offsetHeight}`);

    const definitions = document.createElementNS('http://www.w3.org/2000/svg', 'defs');
    definitions.innerHTML = '<marker id="flowArrow" viewBox="0 0 8 8" refX="7" refY="4" markerWidth="5" markerHeight="5" orient="auto-start-reverse"><path d="M0 0 8 4 0 8z" fill="context-stroke"/></marker>';
    svg.append(definitions);
    const trace = traceContext();

    for (const edge of visibleEdges) {
      const source = graphSurface.querySelector(`[data-node-id="${CSS.escape(edge.source)}"]`);
      const target = graphSurface.querySelector(`[data-node-id="${CSS.escape(edge.target)}"]`);
      if (!source || !target || source === target) continue;
      const sourceRect = source.getBoundingClientRect();
      const targetRect = target.getBoundingClientRect();
      const forward = targetRect.left >= sourceRect.left;
      const startX = (forward ? sourceRect.right : sourceRect.left) - surfaceRect.left;
      const endX = (forward ? targetRect.left : targetRect.right) - surfaceRect.left;
      const startY = sourceRect.top + sourceRect.height / 2 - surfaceRect.top;
      const endY = targetRect.top + targetRect.height / 2 - surfaceRect.top;
      const bend = Math.max(20, Math.abs(endX - startX) * .42);
      const path = document.createElementNS('http://www.w3.org/2000/svg', 'path');
      path.setAttribute('d', `M${startX},${startY} C${startX + (forward ? bend : -bend)},${startY} ${endX - (forward ? bend : -bend)},${endY} ${endX},${endY}`);
      path.setAttribute('marker-end', 'url(#flowArrow)');
      const key = edgeKey(edge);
      const classes = [`relation-${relationshipClass(edge.relation)}`, `confidence-${edge.confidence.toLowerCase()}`];
      if (state.trace && trace.edgeKeys.has(key)) classes.push('trace-edge');
      else if (state.trace) classes.push('muted-edge');
      path.setAttribute('class', classes.join(' '));
      svg.append(path);
    }
  }

  function edgeKey(edge) {
    return `${edge.source}\u0000${edge.target}\u0000${edge.relation}`;
  }

  function findDirectedPath(startId, endId) {
    if (startId === endId) return { nodeIds: [startId], pathEdges: [] };
    const queue = [startId];
    const previous = new Map([[startId, null]]);
    while (queue.length) {
      const current = queue.shift();
      for (const edge of outgoing.get(current) || []) {
        if (previous.has(edge.target)) continue;
        previous.set(edge.target, { id: current, edge });
        if (edge.target === endId) {
          const nodeIds = [endId];
          const pathEdges = [];
          let cursor = endId;
          while (cursor !== startId) {
            const step = previous.get(cursor);
            pathEdges.unshift(step.edge);
            cursor = step.id;
            nodeIds.unshift(cursor);
          }
          return { nodeIds, pathEdges };
        }
        queue.push(edge.target);
      }
    }
    return null;
  }

  function traceContext() {
    if (!state.trace) return { nodeIds: new Set(), edgeKeys: new Set(), path: null, mode: 'none' };
    if (state.selectedId && state.selectedId !== state.focusId) {
      const path = findDirectedPath(state.focusId, state.selectedId) || findDirectedPath(state.selectedId, state.focusId);
      if (path) {
        return {
          nodeIds: new Set(path.nodeIds),
          edgeKeys: new Set(path.pathEdges.map(edgeKey)),
          path,
          mode: 'path',
        };
      }
    }
    const nodeIds = new Set([state.focusId]);
    const edgeKeys = new Set();
    for (const edge of edges) {
      const sourceDistance = edge.source === state.focusId ? 0 : distanceState.downstream.get(edge.source);
      const targetDistance = distanceState.downstream.get(edge.target);
      if (sourceDistance !== undefined && targetDistance === sourceDistance + 1 && targetDistance <= state.depth) {
        nodeIds.add(edge.source);
        nodeIds.add(edge.target);
        edgeKeys.add(edgeKey(edge));
      }
    }
    return { nodeIds, edgeKeys, path: null, mode: 'impact' };
  }

  function updateNodeStates() {
    const trace = traceContext();
    graphSurface.querySelectorAll('[data-node-id]').forEach((button) => {
      const id = button.dataset.nodeId;
      button.classList.toggle('is-selected', id === state.selectedId && id !== state.focusId);
      button.classList.toggle('is-traced', state.trace && trace.nodeIds.has(id));
      button.classList.toggle('is-muted', state.trace && !trace.nodeIds.has(id));
      button.setAttribute('aria-current', id === state.focusId ? 'true' : 'false');
    });
    renderPathBar(trace);
    scheduleConnections();
  }

  function selectNode(id, { moveFocus = true } = {}) {
    if (!nodeById.has(id)) return;
    state.selectedId = id;
    updateNodeStates();
    renderInspector();
    if (moveFocus) {
      const card = graphSurface.querySelector(`[data-node-id="${CSS.escape(id)}"]`);
      card?.focus({ preventScroll: true });
    }
    announce(`${nodeById.get(id).label} selected. Press Enter to investigate.`);
  }

  function setFocus(id, { fromHistory = false } = {}) {
    if (!nodeById.has(id) || id === state.focusId) return;
    state.focusId = id;
    state.selectedId = id;
    state.trace = false;
    state.expandedColumns.clear();
    if (!fromHistory) {
      state.history = state.history.slice(0, state.historyIndex + 1);
      state.history.push(id);
      state.historyIndex = state.history.length - 1;
    }
    renderAll({ center: true });
    announce(`Now investigating ${nodeById.get(id).label}.`);
  }

  function setDepth(depth) {
    state.depth = Math.max(1, Math.min(3, depth));
    state.expandedColumns.clear();
    document.querySelectorAll('[data-depth]').forEach((button) => {
      button.setAttribute('aria-pressed', String(Number(button.dataset.depth) === state.depth));
    });
    renderGraph({ center: true });
    renderInspector();
    announce(`Neighborhood depth ${state.depth}.`);
  }

  function toggleTrace(force) {
    state.trace = force ?? !state.trace;
    traceButton.setAttribute('aria-pressed', String(state.trace));
    traceLabel.textContent = state.trace ? 'Exit trace' : 'Trace impact';
    updateNodeStates();
    renderInspector();
    announce(state.trace ? 'Impact trace active.' : 'Impact trace cleared.');
  }

  function renderPathBar(trace = traceContext()) {
    const pathBar = document.getElementById('pathBar');
    const summary = document.getElementById('pathSummary');
    pathBar.classList.toggle('is-tracing', state.trace);
    clearTraceButton.hidden = !state.trace;
    if (!state.trace) {
      const selected = nodeById.get(state.selectedId);
      summary.textContent = selected && selected.id !== state.focusId
        ? `${selected.label} selected · ${relationshipSummary(selected.id)}`
        : 'Select a node to inspect its evidence. Press Enter to make it the new focus.';
      return;
    }
    if (trace.mode === 'path' && trace.path) {
      summary.textContent = `${trace.path.nodeIds.map((id) => nodeById.get(id).label).join('  →  ')} · ${trace.path.pathEdges.length} step${trace.path.pathEdges.length === 1 ? '' : 's'}`;
      return;
    }
    const affected = Math.max(0, trace.nodeIds.size - 1);
    summary.textContent = `Impact radius · ${affected} affected symbol${affected === 1 ? '' : 's'} within ${state.depth} hop${state.depth === 1 ? '' : 's'}`;
  }

  function relationshipSummary(id) {
    const edge = edges.find((candidate) => candidate.source === state.focusId && candidate.target === id)
      || edges.find((candidate) => candidate.source === id && candidate.target === state.focusId);
    if (!edge) {
      const path = findDirectedPath(state.focusId, id) || findDirectedPath(id, state.focusId);
      return path ? `${path.pathEdges.length} hops from focus` : 'outside the directed path';
    }
    const direction = edge.source === state.focusId ? 'downstream' : 'upstream';
    return `${edge.relation.replaceAll('_', ' ')} · ${direction} · ${confidenceLabel(edge.confidence)}`;
  }

  function downstreamReach(id, maximumDepth = 5) {
    return breadthFirstDistances(id, 'downstream', maximumDepth);
  }

  function riskFor(node) {
    const reach = downstreamReach(node.id);
    const relevantEdges = edges.filter((edge) => edge.source === node.id || edge.target === node.id);
    const uncertain = relevantEdges.filter((edge) => edge.confidence !== 'EXTRACTED').length;
    const communities = new Set([...reach.keys()].map((id) => nodeById.get(id)?.community).filter(Boolean));
    communities.delete(node.community);
    const score = Math.min(96, 18 + reach.size * 3 + uncertain * 5 + communities.size * 4);
    const level = score >= 66 ? 'High change risk' : score >= 42 ? 'Moderate change risk' : 'Low change risk';
    const explanation = `${reach.size} downstream symbols across ${communities.size + 1} communit${communities.size ? 'ies' : 'y'}; ${uncertain} relationship${uncertain === 1 ? '' : 's'} need verification.`;
    return { score, level, explanation };
  }

  function relevantEvidence(node) {
    const direct = edges.filter((edge) => edge.source === node.id || edge.target === node.id);
    return direct
      .sort((left, right) => {
        const confidenceOrder = { AMBIGUOUS: 0, INFERRED: 1, EXTRACTED: 2 };
        return confidenceOrder[left.confidence] - confidenceOrder[right.confidence];
      })
      .slice(0, 5);
  }

  function sourceLines(node) {
    const line = Number.parseInt(node.location.replace(/\D/g, ''), 10) || 1;
    const dependencies = (outgoing.get(node.id) || []).slice(0, 3);
    const functionName = node.id.replaceAll('-', '_');
    const body = dependencies.length
      ? dependencies.map((edge, index) => {
        const target = edge.target.replaceAll('-', '_');
        if (edge.relation === 'reads') return `    ${target} = ${target}.read(context)`;
        if (edge.relation === 'writes') return `    await ${target}.write(result)`;
        if (edge.relation === 'publishes') return `    ${target}.publish(result.events)`;
        if (edge.relation === 'creates') return `    ${target} = ${target}.create(context)`;
        return `    ${index ? 'await ' : ''}${target}(context)`;
      })
      : ['    return context'];
    return {
      start: Math.max(1, line - 2),
      values: ['@trace_operation', `def ${functionName}(context):`, ...body, '    return result'],
      highlight: 1,
    };
  }

  function renderInspector() {
    if (state.scenario === 'loading') {
      inspectorContent.innerHTML = `<div class="inspector-placeholder"><svg viewBox="0 0 48 48" aria-hidden="true"><circle cx="24" cy="24" r="17"/><path d="M24 7v8m0 18v8M7 24h8m18 0h8"/></svg><strong>Preparing evidence</strong><p>Source context and risk signals appear when the neighborhood is ready.</p></div>`;
      return;
    }
    if (state.scenario === 'empty') {
      inspectorContent.innerHTML = `<div class="inspector-placeholder"><svg viewBox="0 0 48 48" aria-hidden="true"><circle cx="24" cy="24" r="10"/><path d="M7 12h8M33 36h8M12 41v-8M36 15V7"/></svg><strong>No relationship evidence</strong><p>The source symbol is available, but this scope has no edges to inspect.</p></div>`;
      return;
    }

    const node = nodeById.get(state.selectedId) || nodeById.get(state.focusId);
    const risk = riskFor(node);
    const evidence = relevantEvidence(node);
    const lines = sourceLines(node);
    const evidenceMarkup = evidence.length
      ? evidence.map((edge) => {
        const otherId = edge.source === node.id ? edge.target : edge.source;
        const other = nodeById.get(otherId);
        const direction = edge.source === node.id ? 'outgoing' : 'incoming';
        const confidenceClass = edge.confidence.toLowerCase();
        return `<div class="evidence-row">
          <span class="evidence-shape ${confidenceClass}" aria-label="${confidenceLabel(edge.confidence)}">${confidenceGlyph(edge.confidence)}</span>
          <span><b>${escapeHtml(edge.relation.replaceAll('_', ' '))} · ${direction}</b><small>${escapeHtml(other?.label || otherId)}</small></span>
          <code>${edge.confidence === 'EXTRACTED' ? 'AST' : edge.confidence === 'INFERRED' ? 'resolver' : 'review'}</code>
        </div>`;
      }).join('')
      : '<p class="search-empty">No direct evidence.</p>';

    inspectorContent.innerHTML = `<div class="inspector-inner">
      <header class="inspector-head"><div><span class="eyebrow">${node.id === state.focusId ? 'Current focus' : 'Selected symbol'}</span><h2>${escapeHtml(node.label)}</h2></div><span class="kind-pill">${escapeHtml(node.kind)}</span></header>
      <button class="source-link" type="button" id="sourceLink"><span>${escapeHtml(node.file)}:${escapeHtml(node.location)}</span><span aria-hidden="true">↗</span></button>
      <div class="risk-card"><span class="risk-score" aria-label="Risk score ${risk.score} out of 100">${risk.score}</span><div><strong>${risk.level}</strong><p>${risk.explanation}</p></div></div>
      <section class="inspector-section"><div class="section-label">Relationship evidence <span>${evidence.length} direct</span></div>${evidenceMarkup}</section>
      <section class="inspector-section"><div class="section-label">Source context <span>${escapeHtml(node.location)}</span></div><div class="code-panel"><div class="code-toolbar"><span>${escapeHtml(node.file.split('/').pop())}</span><span>read-only preview</span></div><pre id="sourcePreview"></pre></div></section>
      <div class="inspector-actions"><button id="focusSelectionButton" class="primary-button" type="button" ${node.id === state.focusId ? 'disabled' : ''}>Set as focus</button><button id="traceSelectionButton" class="secondary-button" type="button">${state.trace ? 'Clear trace' : 'Trace path'}</button></div>
    </div>`;

    const preview = document.getElementById('sourcePreview');
    lines.values.forEach((value, index) => {
      const lineElement = document.createElement('span');
      lineElement.className = `code-line${index === lines.highlight ? ' highlight' : ''}`;
      lineElement.dataset.line = String(lines.start + index);
      lineElement.textContent = value;
      preview.append(lineElement);
    });
    document.getElementById('focusSelectionButton').addEventListener('click', () => setFocus(node.id));
    document.getElementById('traceSelectionButton').addEventListener('click', () => toggleTrace());
    document.getElementById('sourceLink').addEventListener('click', () => announce(`Prototype action: open ${node.file} at ${node.location}.`));
  }

  function updateGraphStats() {
    const inferred = visibleEdges.filter((edge) => edge.confidence !== 'EXTRACTED').length;
    document.getElementById('graphStats').textContent = `${visibleNodeIds.size} visible symbols · ${visibleEdges.length} relationships · ${inferred} inferred or ambiguous`;
  }

  function renderHistory() {
    historyTrail.replaceChildren();
    state.history.forEach((id, index) => {
      const wrapper = document.createElement('span');
      wrapper.className = `history-item${index === state.historyIndex ? ' current' : ''}`;
      const button = document.createElement('button');
      button.type = 'button';
      button.textContent = nodeById.get(id)?.label || id;
      button.setAttribute('aria-current', index === state.historyIndex ? 'page' : 'false');
      button.addEventListener('click', () => moveHistory(index));
      wrapper.append(button);
      historyTrail.append(wrapper);
    });
    document.getElementById('backButton').disabled = state.historyIndex <= 0;
    document.getElementById('forwardButton').disabled = state.historyIndex >= state.history.length - 1;
  }

  function moveHistory(index) {
    if (index < 0 || index >= state.history.length || index === state.historyIndex) return;
    state.historyIndex = index;
    state.focusId = state.history[index];
    state.selectedId = state.focusId;
    state.trace = false;
    renderAll({ center: true });
  }

  function renderFocusHeader() {
    const node = nodeById.get(state.focusId);
    document.getElementById('focusTitle').textContent = node.label;
    document.getElementById('focusLocation').textContent = `${node.file} · ${node.location}`;
  }

  function renderScenario() {
    const normal = state.scenario === 'typical' || state.scenario === 'dense';
    graphViewport.hidden = !normal;
    loadingState.hidden = state.scenario !== 'loading';
    emptyState.hidden = state.scenario !== 'empty';
    inspector.classList.toggle('is-placeholder', !normal);
    if (normal) renderGraph({ center: true });
    else {
      document.getElementById('graphStats').textContent = state.scenario === 'loading' ? 'Resolving graph…' : '1 indexed symbol · 0 relationships';
    }
  }

  function renderAll({ center = false } = {}) {
    renderFocusHeader();
    renderHistory();
    renderScenario();
    renderInspector();
    renderPathBar();
    traceButton.setAttribute('aria-pressed', String(state.trace));
    if (center && (state.scenario === 'typical' || state.scenario === 'dense')) requestAnimationFrame(centerOnFocus);
  }

  function renderSearchResults() {
    const term = searchInput.value.trim().toLocaleLowerCase();
    if (!term) {
      closeSearchResults();
      return;
    }
    state.searchMatches = nodes
      .map((node) => {
        const label = node.label.toLocaleLowerCase();
        const id = node.id.toLocaleLowerCase();
        const file = node.file.toLocaleLowerCase();
        const score = label === term ? 0 : label.startsWith(term) ? 1 : id.startsWith(term) ? 2 : label.includes(term) ? 3 : file.includes(term) ? 4 : 99;
        return { node, score };
      })
      .filter((match) => match.score < 99)
      .sort((left, right) => left.score - right.score || right.node.degree - left.node.degree)
      .slice(0, 10)
      .map((match) => match.node);
    state.searchIndex = state.searchMatches.length ? 0 : -1;
    searchResults.innerHTML = state.searchMatches.length
      ? state.searchMatches.map((node, index) => `<button class="search-result" type="button" role="option" data-search-id="${escapeHtml(node.id)}" aria-selected="${index === state.searchIndex}"><span class="result-icon" aria-hidden="true">${escapeHtml(kindGlyph(node.kind))}</span><span><strong>${escapeHtml(node.label)}</strong><small>${escapeHtml(node.file)}:${escapeHtml(node.location)}</small></span><span class="result-community">${escapeHtml(node.communityName)}</span></button>`).join('')
      : '<div class="search-empty">No symbol or file matches that query.</div>';
    searchResults.classList.add('open');
    searchInput.setAttribute('aria-expanded', 'true');
    searchResults.querySelectorAll('[data-search-id]').forEach((button) => button.addEventListener('click', () => chooseSearchResult(button.dataset.searchId)));
  }

  function updateSearchSelection() {
    searchResults.querySelectorAll('[data-search-id]').forEach((button, index) => {
      button.setAttribute('aria-selected', String(index === state.searchIndex));
      if (index === state.searchIndex) button.scrollIntoView({ block: 'nearest' });
    });
  }

  function chooseSearchResult(id) {
    searchInput.value = '';
    closeSearchResults();
    if (state.scenario === 'loading' || state.scenario === 'empty') {
      state.scenario = 'typical';
      scenarioSelect.value = 'typical';
    }
    setFocus(id);
  }

  function closeSearchResults() {
    state.searchMatches = [];
    state.searchIndex = -1;
    searchResults.classList.remove('open');
    searchResults.replaceChildren();
    searchInput.setAttribute('aria-expanded', 'false');
  }

  function announce(message) {
    announcer.textContent = '';
    requestAnimationFrame(() => { announcer.textContent = message; });
  }

  function reducedMotion() {
    return matchMedia('(prefers-reduced-motion: reduce)').matches;
  }

  searchInput.addEventListener('input', renderSearchResults);
  searchInput.addEventListener('keydown', (event) => {
    if (!state.searchMatches.length) {
      if (event.key === 'Escape') { searchInput.value = ''; closeSearchResults(); }
      return;
    }
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault();
      const delta = event.key === 'ArrowDown' ? 1 : -1;
      state.searchIndex = (state.searchIndex + delta + state.searchMatches.length) % state.searchMatches.length;
      updateSearchSelection();
    }
    if (event.key === 'Enter') {
      event.preventDefault();
      chooseSearchResult(state.searchMatches[Math.max(0, state.searchIndex)].id);
    }
    if (event.key === 'Escape') {
      event.preventDefault();
      searchInput.value = '';
      closeSearchResults();
    }
  });

  document.querySelectorAll('[data-depth]').forEach((button) => button.addEventListener('click', () => setDepth(Number(button.dataset.depth))));
  traceButton.addEventListener('click', () => toggleTrace());
  clearTraceButton.addEventListener('click', () => toggleTrace(false));
  document.getElementById('backButton').addEventListener('click', () => moveHistory(state.historyIndex - 1));
  document.getElementById('forwardButton').addEventListener('click', () => moveHistory(state.historyIndex + 1));
  document.getElementById('emptySearchButton').addEventListener('click', () => searchInput.focus());
  document.getElementById('shortcutButton').addEventListener('click', () => shortcutDialog.showModal());

  scenarioSelect.addEventListener('change', () => {
    state.scenario = scenarioSelect.value;
    state.trace = false;
    traceLabel.textContent = 'Trace impact';
    if (state.scenario === 'dense') {
      state.depth = 3;
      if (state.focusId !== 'checkout-service') {
        state.focusId = 'checkout-service';
        state.selectedId = state.focusId;
        state.history = [...state.history.slice(0, state.historyIndex + 1), state.focusId];
        state.historyIndex = state.history.length - 1;
      }
      document.querySelectorAll('[data-depth]').forEach((button) => button.setAttribute('aria-pressed', String(Number(button.dataset.depth) === state.depth)));
    }
    renderAll({ center: true });
    announce(`${scenarioSelect.options[scenarioSelect.selectedIndex].text} preview.`);
  });

  document.addEventListener('click', (event) => {
    if (!event.target.closest('.search-box')) closeSearchResults();
  });

  document.addEventListener('keydown', (event) => {
    const interactive = event.target.matches('input, select, textarea') || event.target.isContentEditable;
    if (event.key === '/' && !interactive) {
      event.preventDefault();
      searchInput.focus();
    } else if (event.key === '?' && !interactive) {
      event.preventDefault();
      shortcutDialog.showModal();
    } else if ((event.key === 't' || event.key === 'T') && !interactive && !shortcutDialog.open) {
      event.preventDefault();
      toggleTrace();
    } else if (event.key === '[' && !interactive) {
      event.preventDefault();
      setDepth(state.depth - 1);
    } else if (event.key === ']' && !interactive) {
      event.preventDefault();
      setDepth(state.depth + 1);
    } else if (event.key === 'Backspace' && !interactive && !shortcutDialog.open) {
      event.preventDefault();
      moveHistory(state.historyIndex + (event.shiftKey ? 1 : -1));
    } else if (event.key === 'Escape' && state.trace && !shortcutDialog.open) {
      toggleTrace(false);
    }
  });

  const resizeObserver = new ResizeObserver(scheduleConnections);
  resizeObserver.observe(graphSurface);
  window.addEventListener('resize', scheduleConnections);

  const previewParameters = new URLSearchParams(location.search);
  const requestedScenario = previewParameters.get('state');
  if (['typical', 'dense', 'loading', 'empty'].includes(requestedScenario)) {
    state.scenario = requestedScenario;
    scenarioSelect.value = requestedScenario;
    if (requestedScenario === 'dense') state.depth = 3;
  }
  const requestedDepth = Number(previewParameters.get('depth'));
  if ([1, 2, 3].includes(requestedDepth)) state.depth = requestedDepth;
  const requestedSelection = previewParameters.get('select');
  if (nodeById.has(requestedSelection)) state.selectedId = requestedSelection;
  if (previewParameters.get('trace') === '1') {
    state.trace = true;
    traceLabel.textContent = 'Exit trace';
  }
  document.querySelectorAll('[data-depth]').forEach((button) => {
    button.setAttribute('aria-pressed', String(Number(button.dataset.depth) === state.depth));
  });

  renderAll({ center: true });
  root.dataset.prototypeReady = 'true';
})();
