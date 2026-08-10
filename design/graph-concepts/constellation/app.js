(function runConstellationConcept() {
  'use strict';

  const graph = globalThis.GRAPHOXIDE_GRAPH_FIXTURE;
  if (!graph || graph.contractVersion !== 1) {
    throw new Error('The Graphoxide concept fixture is missing or incompatible.');
  }

  const canvas = document.querySelector('#graph-canvas');
  const context = canvas.getContext('2d', { alpha: true });
  const stage = document.querySelector('#graph-stage');
  const tooltip = document.querySelector('#tooltip');
  const search = document.querySelector('#search');
  const searchResults = document.querySelector('#search-results');
  const communityList = document.querySelector('#community-list');
  const communityLabels = document.querySelector('#community-labels');
  const relationFilters = document.querySelector('#relation-filters');
  const viewTitle = document.querySelector('#view-title');
  const allCommunitiesButton = document.querySelector('#all-communities');
  const traceButton = document.querySelector('#trace-button');
  const renderStats = document.querySelector('#render-stats');
  const inspector = document.querySelector('#inspector');
  const inspectorEmpty = document.querySelector('#inspector-empty');
  const inspectorContent = document.querySelector('#inspector-content');
  const emptyState = document.querySelector('#empty-state');
  const a11yStatus = document.querySelector('#a11y-status');

  const nodeById = new Map(graph.nodes.map((node) => [node.id, node]));
  const outgoing = new Map(graph.nodes.map((node) => [node.id, []]));
  const adjacent = new Map(graph.nodes.map((node) => [node.id, []]));
  graph.edges.forEach((edge, index) => {
    outgoing.get(edge.source).push({ edge, index });
    adjacent.get(edge.source).push({ edge, index, otherId: edge.target, direction: 'out' });
    if (edge.target !== edge.source) {
      adjacent.get(edge.target).push({ edge, index, otherId: edge.source, direction: 'in' });
    }
  });

  const communityEntries = [...new Map(graph.nodes.map((node) => [node.community, node.communityName])).entries()]
    .map(([id, name]) => ({ id, name, nodes: graph.nodes.filter((node) => node.community === id) }))
    .sort((left, right) => right.nodes.length - left.nodes.length || left.name.localeCompare(right.name));

  const palette = ['#a58cff', '#59d4e7', '#79aaff', '#f37cb6', '#69ddb7', '#efbb68', '#dc836b', '#91ca73'];
  const colorByCommunity = new Map(communityEntries.map((community, index) => [community.id, palette[index % palette.length]]));
  const relationGroups = [
    { id: 'flow', label: 'Flow', relations: new Set(['calls', 'routes_to', 'dispatches']) },
    { id: 'data', label: 'Data', relations: new Set(['reads', 'writes', 'creates', 'renders']) },
    { id: 'events', label: 'Events', relations: new Set(['publishes']) },
    { id: 'structure', label: 'Structure', relations: new Set(['imports']) },
  ];
  const relationGroupByRelation = new Map();
  for (const group of relationGroups) for (const relation of group.relations) relationGroupByRelation.set(relation, group.id);

  const state = {
    scale: 1,
    offsetX: 0,
    offsetY: 0,
    selectedId: null,
    hoveredId: null,
    focusedId: null,
    communityId: null,
    density: 'balanced',
    activeRelationGroups: new Set(relationGroups.map((group) => group.id)),
    traceEnabled: false,
    traceNodeIds: new Set(),
    traceEdgeIndexes: new Set(),
    pointer: null,
    layoutReady: false,
    searchIndex: -1,
    lastFrame: 0,
    animationFrameId: null,
  };

  const reducedMotion = globalThis.matchMedia('(prefers-reduced-motion: reduce)').matches;
  const positions = new Map();
  const communityCenters = new Map();
  const communityLabelElements = new Map();
  const stars = Array.from({ length: 150 }, (_, index) => ({
    x: randomUnit(`star-x-${index}`),
    y: randomUnit(`star-y-${index}`),
    radius: 0.35 + randomUnit(`star-r-${index}`) * 0.9,
    alpha: 0.08 + randomUnit(`star-a-${index}`) * 0.25,
  }));

  function hash(value) {
    let result = 2166136261;
    for (let index = 0; index < value.length; index += 1) {
      result ^= value.charCodeAt(index);
      result = Math.imul(result, 16777619);
    }
    return result >>> 0;
  }

  function randomUnit(seed) {
    return hash(seed) / 0xffffffff;
  }

  function relationGroup(edge) {
    return relationGroupByRelation.get(edge.relation) || 'structure';
  }

  function buildLayout() {
    const centers = [
      { x: -390, y: -175 },
      { x: 8, y: -245 },
      { x: 385, y: -150 },
      { x: -355, y: 205 },
      { x: 22, y: 188 },
      { x: 385, y: 225 },
      { x: 0, y: 480 },
      { x: -450, y: 475 },
    ];

    communityEntries.forEach((community, communityIndex) => {
      const center = centers[communityIndex] || {
        x: Math.cos(communityIndex * 2.4) * 450,
        y: Math.sin(communityIndex * 2.4) * 330,
      };
      communityCenters.set(community.id, center);
      const sortedNodes = [...community.nodes].sort((left, right) => right.degree - left.degree || left.id.localeCompare(right.id));
      sortedNodes.forEach((node, nodeIndex) => {
        if (nodeIndex === 0) {
          positions.set(node.id, { x: center.x, y: center.y });
          return;
        }
        const ring = Math.floor((nodeIndex - 1) / 6);
        const slot = (nodeIndex - 1) % 6;
        const countOnRing = Math.min(6, sortedNodes.length - 1 - ring * 6);
        const baseAngle = randomUnit(`${community.id}-rotation`) * Math.PI * 2;
        const angle = baseAngle + slot / countOnRing * Math.PI * 2 + ring * 0.37;
        const radius = 78 + ring * 58 + randomUnit(node.id) * 13;
        positions.set(node.id, {
          x: center.x + Math.cos(angle) * radius * 1.12,
          y: center.y + Math.sin(angle) * radius * 0.82,
        });
      });
    });
    state.layoutReady = true;
  }

  function buildNavigation() {
    document.querySelector('#overview-count').textContent = `${graph.nodes.length} nodes · ${graph.edges.length} links`;
    document.querySelector('#community-total').textContent = String(communityEntries.length);

    for (const community of communityEntries) {
      const button = document.createElement('button');
      button.type = 'button';
      button.className = 'community-button';
      button.dataset.community = community.id;
      button.setAttribute('aria-pressed', 'false');

      const signal = document.createElement('span');
      signal.className = 'community-signal';
      signal.style.color = colorByCommunity.get(community.id);
      signal.setAttribute('aria-hidden', 'true');

      const copy = document.createElement('span');
      const name = document.createElement('strong');
      name.textContent = community.name;
      const summary = document.createElement('small');
      const internalEdges = graph.edges.filter((edge) => nodeById.get(edge.source)?.community === community.id && nodeById.get(edge.target)?.community === community.id).length;
      summary.textContent = `${internalEdges} internal links`;
      copy.append(name, summary);

      const count = document.createElement('span');
      count.className = 'node-count';
      count.textContent = String(community.nodes.length).padStart(2, '0');
      button.append(signal, copy, count);
      button.addEventListener('click', () => setCommunity(community.id));
      communityList.append(button);

      const mapLabel = document.createElement('span');
      mapLabel.className = 'community-map-label';
      const dot = document.createElement('i');
      dot.style.color = colorByCommunity.get(community.id);
      mapLabel.append(dot, document.createTextNode(community.name));
      communityLabels.append(mapLabel);
      communityLabelElements.set(community.id, mapLabel);
    }

    for (const group of relationGroups) {
      const wrap = document.createElement('span');
      wrap.className = 'relation-chip';
      const input = document.createElement('input');
      input.type = 'checkbox';
      input.id = `relation-${group.id}`;
      input.checked = true;
      const label = document.createElement('label');
      label.htmlFor = input.id;
      label.textContent = group.label;
      input.addEventListener('change', () => {
        if (input.checked) state.activeRelationGroups.add(group.id);
        else state.activeRelationGroups.delete(group.id);
        clearTrace();
        draw();
      });
      wrap.append(input, label);
      relationFilters.append(wrap);
    }
  }

  function worldToScreen(position) {
    return {
      x: position.x * state.scale + state.offsetX,
      y: position.y * state.scale + state.offsetY,
    };
  }

  function screenToWorld(x, y) {
    return {
      x: (x - state.offsetX) / state.scale,
      y: (y - state.offsetY) / state.scale,
    };
  }

  function fitView() {
    const included = graph.nodes.filter((node) => !state.communityId || node.community === state.communityId);
    if (!included.length) return;
    const points = included.map((node) => positions.get(node.id));
    const minX = Math.min(...points.map((point) => point.x)) - 130;
    const maxX = Math.max(...points.map((point) => point.x)) + 130;
    const minY = Math.min(...points.map((point) => point.y)) - 125;
    const maxY = Math.max(...points.map((point) => point.y)) + 125;
    const width = Math.max(1, canvas.clientWidth);
    const height = Math.max(1, canvas.clientHeight);
    state.scale = Math.max(0.35, Math.min(1.35, Math.min(width / (maxX - minX), height / (maxY - minY)) * 0.89));
    state.offsetX = width / 2 - (minX + maxX) / 2 * state.scale;
    state.offsetY = height / 2 - (minY + maxY) / 2 * state.scale + 6;
    draw();
  }

  function resizeCanvas() {
    const ratio = Math.min(globalThis.devicePixelRatio || 1, 2);
    const width = Math.max(1, Math.floor(canvas.clientWidth * ratio));
    const height = Math.max(1, Math.floor(canvas.clientHeight * ratio));
    if (canvas.width !== width || canvas.height !== height) {
      canvas.width = width;
      canvas.height = height;
      context.setTransform(ratio, 0, 0, ratio, 0, 0);
      if (!state.layoutReady) return;
      fitView();
    } else {
      draw();
    }
  }

  function setCommunity(communityId) {
    state.communityId = communityId;
    state.hoveredId = null;
    if (state.selectedId && nodeById.get(state.selectedId)?.community !== communityId) selectNode(null);
    clearTrace();
    allCommunitiesButton.classList.toggle('active', communityId === null);
    allCommunitiesButton.setAttribute('aria-pressed', String(communityId === null));
    for (const button of communityList.querySelectorAll('.community-button')) {
      const active = button.dataset.community === communityId;
      button.classList.toggle('active', active);
      button.setAttribute('aria-pressed', String(active));
    }
    const selectedCommunity = communityEntries.find((community) => community.id === communityId);
    viewTitle.textContent = selectedCommunity?.name || 'All communities';
    fitView();
  }

  function visibleNodes() {
    return graph.nodes.filter((node) => !state.communityId || node.community === state.communityId);
  }

  function visibleEdges() {
    const visibleIds = new Set(visibleNodes().map((node) => node.id));
    const activeId = state.hoveredId || state.selectedId || state.focusedId;
    const budget = state.density === 'focus' ? 260 : state.density === 'balanced' ? 1200 : 2800;
    const candidates = graph.edges
      .map((edge, index) => ({ edge, index }))
      .filter(({ edge }) => visibleIds.has(edge.source) && visibleIds.has(edge.target) && state.activeRelationGroups.has(relationGroup(edge)))
      .sort((left, right) => {
        const leftRelevant = Number(left.edge.source === activeId || left.edge.target === activeId || state.traceEdgeIndexes.has(left.index));
        const rightRelevant = Number(right.edge.source === activeId || right.edge.target === activeId || state.traceEdgeIndexes.has(right.index));
        if (leftRelevant !== rightRelevant) return rightRelevant - leftRelevant;
        const confidenceRank = { EXTRACTED: 2, INFERRED: 1, AMBIGUOUS: 0 };
        const confidenceDifference = confidenceRank[right.edge.confidence] - confidenceRank[left.edge.confidence];
        if (confidenceDifference) return confidenceDifference;
        return (nodeById.get(right.edge.source).degree + nodeById.get(right.edge.target).degree)
          - (nodeById.get(left.edge.source).degree + nodeById.get(left.edge.target).degree);
      });
    if (state.density === 'focus') {
      if (activeId) {
        return candidates
          .filter(({ edge }) => edge.source === activeId || edge.target === activeId || state.traceNodeIds.has(edge.source) && state.traceNodeIds.has(edge.target))
          .slice(0, budget);
      }
      const quietLimit = Math.min(budget, Math.max(24, Math.ceil(candidates.length * 0.48)));
      return candidates.slice(0, quietLimit);
    }
    return candidates.slice(0, budget);
  }

  function hexToRgba(hex, alpha) {
    const numeric = Number.parseInt(hex.slice(1), 16);
    return `rgba(${numeric >> 16}, ${(numeric >> 8) & 255}, ${numeric & 255}, ${alpha})`;
  }

  function convexHull(points) {
    if (points.length <= 2) return points;
    const sorted = [...points].sort((left, right) => left.x - right.x || left.y - right.y);
    const cross = (origin, a, b) => (a.x - origin.x) * (b.y - origin.y) - (a.y - origin.y) * (b.x - origin.x);
    const lower = [];
    for (const point of sorted) {
      while (lower.length >= 2 && cross(lower.at(-2), lower.at(-1), point) <= 0) lower.pop();
      lower.push(point);
    }
    const upper = [];
    for (const point of sorted.reverse()) {
      while (upper.length >= 2 && cross(upper.at(-2), upper.at(-1), point) <= 0) upper.pop();
      upper.push(point);
    }
    lower.pop();
    upper.pop();
    return lower.concat(upper);
  }

  function roundedPolygonPath(points) {
    if (!points.length) return;
    if (points.length < 3) {
      const center = points[0] || { x: 0, y: 0 };
      context.arc(center.x, center.y, 70 * state.scale, 0, Math.PI * 2);
      return;
    }
    const midpoint = (a, b) => ({ x: (a.x + b.x) / 2, y: (a.y + b.y) / 2 });
    const lastMidpoint = midpoint(points.at(-1), points[0]);
    context.moveTo(lastMidpoint.x, lastMidpoint.y);
    points.forEach((point, index) => {
      const next = points[(index + 1) % points.length];
      const nextMidpoint = midpoint(point, next);
      context.quadraticCurveTo(point.x, point.y, nextMidpoint.x, nextMidpoint.y);
    });
    context.closePath();
  }

  function drawStars(width, height) {
    context.save();
    for (const star of stars) {
      context.beginPath();
      context.arc(star.x * width, star.y * height, star.radius, 0, Math.PI * 2);
      context.fillStyle = `rgba(198, 202, 229, ${star.alpha})`;
      context.fill();
    }
    context.restore();
  }

  function drawCommunityHulls(nodes) {
    const grouped = new Map();
    for (const node of nodes) {
      const values = grouped.get(node.community) || [];
      values.push(node);
      grouped.set(node.community, values);
    }

    for (const [communityId, members] of grouped) {
      const worldCenter = communityCenters.get(communityId);
      const color = colorByCommunity.get(communityId);
      const expanded = members.map((node) => {
        const point = positions.get(node.id);
        const dx = point.x - worldCenter.x;
        const dy = point.y - worldCenter.y;
        const distance = Math.max(1, Math.hypot(dx, dy));
        const padding = members.length < 3 ? 75 : 57;
        return worldToScreen({ x: point.x + dx / distance * padding, y: point.y + dy / distance * padding });
      });
      const hull = convexHull(expanded);
      const screenCenter = worldToScreen(worldCenter);
      const radius = Math.max(65, Math.max(...expanded.map((point) => Math.hypot(point.x - screenCenter.x, point.y - screenCenter.y)), 65));
      const gradient = context.createRadialGradient(screenCenter.x, screenCenter.y, 0, screenCenter.x, screenCenter.y, radius);
      const filtered = state.communityId && state.communityId !== communityId;
      gradient.addColorStop(0, hexToRgba(color, filtered ? 0.008 : 0.075));
      gradient.addColorStop(0.7, hexToRgba(color, filtered ? 0.004 : 0.025));
      gradient.addColorStop(1, hexToRgba(color, 0));
      context.save();
      context.beginPath();
      roundedPolygonPath(hull);
      context.fillStyle = gradient;
      context.fill();
      context.setLineDash([2, 5]);
      context.lineWidth = 0.65;
      context.strokeStyle = hexToRgba(color, filtered ? 0.03 : 0.18);
      context.stroke();
      context.restore();
    }
  }

  function edgeGeometry(edge) {
    const source = worldToScreen(positions.get(edge.source));
    const target = worldToScreen(positions.get(edge.target));
    const dx = target.x - source.x;
    const dy = target.y - source.y;
    const length = Math.max(1, Math.hypot(dx, dy));
    const bend = (randomUnit(`${edge.source}:${edge.target}:${edge.relation}`) - 0.5) * Math.min(42, length * 0.16);
    const control = { x: (source.x + target.x) / 2 - dy / length * bend, y: (source.y + target.y) / 2 + dx / length * bend };
    return { source, target, control };
  }

  function pointOnCurve(geometry, progress) {
    const remaining = 1 - progress;
    return {
      x: remaining * remaining * geometry.source.x + 2 * remaining * progress * geometry.control.x + progress * progress * geometry.target.x,
      y: remaining * remaining * geometry.source.y + 2 * remaining * progress * geometry.control.y + progress * progress * geometry.target.y,
    };
  }

  function drawArrow(geometry, color, alpha) {
    const point = pointOnCurve(geometry, 0.82);
    const before = pointOnCurve(geometry, 0.79);
    const angle = Math.atan2(point.y - before.y, point.x - before.x);
    context.save();
    context.translate(point.x, point.y);
    context.rotate(angle);
    context.beginPath();
    context.moveTo(0, 0);
    context.lineTo(-5, -2.6);
    context.lineTo(-4.1, 0);
    context.lineTo(-5, 2.6);
    context.closePath();
    context.fillStyle = hexToRgba(color, alpha);
    context.fill();
    context.restore();
  }

  function drawEdges(edgeEntries, activeId, timestamp) {
    for (const { edge, index } of edgeEntries) {
      const related = activeId && (edge.source === activeId || edge.target === activeId);
      const traced = state.traceEdgeIndexes.has(index);
      const faded = activeId && !related && !traced;
      const sourceColor = colorByCommunity.get(nodeById.get(edge.source).community);
      const targetColor = colorByCommunity.get(nodeById.get(edge.target).community);
      const geometry = edgeGeometry(edge);
      const gradient = context.createLinearGradient(geometry.source.x, geometry.source.y, geometry.target.x, geometry.target.y);
      const alpha = traced ? 0.82 : related ? 0.6 : faded ? 0.035 : edge.confidence === 'EXTRACTED' ? 0.17 : 0.09;
      gradient.addColorStop(0, hexToRgba(sourceColor, alpha));
      gradient.addColorStop(1, hexToRgba(targetColor, alpha));

      context.save();
      context.beginPath();
      context.moveTo(geometry.source.x, geometry.source.y);
      context.quadraticCurveTo(geometry.control.x, geometry.control.y, geometry.target.x, geometry.target.y);
      context.strokeStyle = gradient;
      context.lineWidth = traced ? 1.7 : related ? 1.15 : 0.68;
      if (edge.confidence === 'INFERRED') context.setLineDash([4, 5]);
      if (edge.confidence === 'AMBIGUOUS') context.setLineDash([1, 6]);
      context.stroke();
      context.restore();

      if (traced || related && state.scale > 0.62) drawArrow(geometry, targetColor, traced ? 0.9 : 0.48);
      if (traced && !reducedMotion) {
        const progress = ((timestamp * 0.00018 + randomUnit(`${edge.source}-${edge.target}`)) % 1);
        const particle = pointOnCurve(geometry, progress);
        const glow = context.createRadialGradient(particle.x, particle.y, 0, particle.x, particle.y, 7);
        glow.addColorStop(0, 'rgba(238, 234, 255, 0.95)');
        glow.addColorStop(0.25, hexToRgba(targetColor, 0.65));
        glow.addColorStop(1, hexToRgba(targetColor, 0));
        context.fillStyle = glow;
        context.beginPath();
        context.arc(particle.x, particle.y, 7, 0, Math.PI * 2);
        context.fill();
      }
    }
  }

  function nodeRadius(node) {
    return Math.max(3.3, Math.min(9.8, 3.2 + Math.sqrt(node.degree) * 1.45)) * Math.max(0.82, Math.min(1.12, Math.sqrt(state.scale)));
  }

  function shouldLabel(node, activeId) {
    if (node.id === activeId || node.id === state.selectedId || node.id === state.focusedId) return true;
    if (state.density === 'focus') return node.degree >= 8 && state.scale > 0.52;
    if (state.density === 'complete') return state.scale > 0.55 || node.degree >= 7;
    return node.degree >= 7 || state.scale > 0.88 && node.degree >= 4;
  }

  function roundedRectangle(x, y, width, height, radius) {
    const safeRadius = Math.min(radius, width / 2, height / 2);
    context.beginPath();
    context.moveTo(x + safeRadius, y);
    context.arcTo(x + width, y, x + width, y + height, safeRadius);
    context.arcTo(x + width, y + height, x, y + height, safeRadius);
    context.arcTo(x, y + height, x, y, safeRadius);
    context.arcTo(x, y, x + width, y, safeRadius);
    context.closePath();
  }

  function drawNodeLabel(node, screenPoint, radius, emphasized) {
    context.font = `${emphasized ? 520 : 450} 10px ${getComputedStyle(document.body).fontFamily}`;
    const textWidth = context.measureText(node.label).width;
    const x = screenPoint.x + radius + 7;
    const y = screenPoint.y - 9;
    roundedRectangle(x - 4, y - 2, textWidth + 8, 18, 4);
    context.fillStyle = emphasized ? 'rgba(12, 13, 23, 0.91)' : 'rgba(8, 9, 16, 0.71)';
    context.fill();
    if (emphasized) {
      context.strokeStyle = 'rgba(176, 166, 238, 0.18)';
      context.lineWidth = 0.7;
      context.stroke();
    }
    context.fillStyle = emphasized ? '#f1efff' : 'rgba(204, 208, 227, 0.72)';
    context.textBaseline = 'middle';
    context.fillText(node.label, x, y + 7);
  }

  function drawNodes(nodes, activeId, timestamp) {
    const adjacentToActive = new Set(activeId ? adjacent.get(activeId).map((entry) => entry.otherId) : []);
    if (activeId) adjacentToActive.add(activeId);
    const width = canvas.clientWidth;
    const height = canvas.clientHeight;

    for (const node of nodes) {
      const position = worldToScreen(positions.get(node.id));
      if (position.x < -50 || position.y < -50 || position.x > width + 50 || position.y > height + 50) continue;
      const color = colorByCommunity.get(node.community);
      const radius = nodeRadius(node);
      const active = node.id === activeId;
      const selected = node.id === state.selectedId;
      const focused = node.id === state.focusedId;
      const traced = state.traceNodeIds.has(node.id);
      const dimmed = activeId && !adjacentToActive.has(node.id) && !traced;
      const alpha = dimmed ? 0.16 : 1;

      context.save();
      context.globalAlpha = alpha;
      if (node.degree >= 7 || active || traced) {
        const haloRadius = radius * (active || traced ? 4.2 : 3.1);
        const halo = context.createRadialGradient(position.x, position.y, radius * 0.2, position.x, position.y, haloRadius);
        halo.addColorStop(0, hexToRgba(color, active || traced ? 0.38 : 0.18));
        halo.addColorStop(0.38, hexToRgba(color, active || traced ? 0.13 : 0.06));
        halo.addColorStop(1, hexToRgba(color, 0));
        context.fillStyle = halo;
        context.beginPath();
        context.arc(position.x, position.y, haloRadius, 0, Math.PI * 2);
        context.fill();
      }

      if ((selected || traced) && !reducedMotion) {
        const pulse = radius + 6 + (Math.sin(timestamp * 0.003 + randomUnit(node.id) * 4) + 1) * 2.5;
        context.beginPath();
        context.arc(position.x, position.y, pulse, 0, Math.PI * 2);
        context.lineWidth = 0.8;
        context.strokeStyle = hexToRgba(color, selected ? 0.38 : 0.2);
        context.stroke();
      }

      context.beginPath();
      context.arc(position.x, position.y, radius + (selected ? 1.4 : 0), 0, Math.PI * 2);
      const nodeGradient = context.createRadialGradient(position.x - radius * 0.3, position.y - radius * 0.35, 0, position.x, position.y, radius * 1.3);
      nodeGradient.addColorStop(0, active || selected ? '#ffffff' : '#e7e5ff');
      nodeGradient.addColorStop(0.2, color);
      nodeGradient.addColorStop(1, hexToRgba(color, 0.36));
      context.fillStyle = nodeGradient;
      context.fill();

      context.beginPath();
      context.arc(position.x, position.y, radius + 2.6, 0, Math.PI * 2);
      context.lineWidth = selected ? 1.5 : focused ? 1.2 : 0.55;
      context.strokeStyle = selected ? '#ddd8ff' : focused ? '#ffffff' : hexToRgba(color, node.degree >= 7 ? 0.48 : 0.2);
      context.stroke();

      if (focused) {
        context.setLineDash([2, 3]);
        context.beginPath();
        context.arc(position.x, position.y, radius + 6, 0, Math.PI * 2);
        context.strokeStyle = 'rgba(255, 255, 255, 0.7)';
        context.lineWidth = 1;
        context.stroke();
      }
      context.restore();
    }

    for (const node of nodes) {
      if (!shouldLabel(node, activeId)) continue;
      const traced = state.traceNodeIds.has(node.id);
      const related = !activeId || node.id === activeId || adjacentToActive.has(node.id) || traced;
      if (!related) continue;
      const position = worldToScreen(positions.get(node.id));
      if (position.x < -80 || position.y < -30 || position.x > width + 20 || position.y > height + 30) continue;
      drawNodeLabel(node, position, nodeRadius(node), node.id === activeId || node.id === state.selectedId);
    }
  }

  function updateCommunityLabels() {
    for (const community of communityEntries) {
      const element = communityLabelElements.get(community.id);
      const point = worldToScreen(communityCenters.get(community.id));
      element.style.left = `${point.x}px`;
      element.style.top = `${point.y - 104 * state.scale}px`;
      element.style.opacity = state.communityId && state.communityId !== community.id ? '0' : String(Math.max(0.2, Math.min(0.72, state.scale)));
    }
  }

  function draw(timestamp = performance.now()) {
    if (!state.layoutReady) return;
    const width = canvas.clientWidth;
    const height = canvas.clientHeight;
    context.clearRect(0, 0, width, height);
    drawStars(width, height);

    const nodes = visibleNodes();
    const edges = visibleEdges();
    const activeId = state.hoveredId || state.selectedId || state.focusedId;
    drawCommunityHulls(nodes);
    drawEdges(edges, activeId, timestamp);
    drawNodes(nodes, activeId, timestamp);
    updateCommunityLabels();

    const hiddenEdges = graph.edges.length - edges.length;
    renderStats.textContent = `${nodes.length} nodes  /  ${edges.length} links${hiddenEdges > 0 ? `  /  ${hiddenEdges} muted` : ''}`;
    emptyState.hidden = edges.length > 0;

    if (state.traceEnabled && !reducedMotion) scheduleTraceFrame();
  }

  function scheduleTraceFrame() {
    if (state.animationFrameId !== null || !state.traceEnabled || reducedMotion) return;
    state.animationFrameId = requestAnimationFrame((timestamp) => {
      state.animationFrameId = null;
      if (!state.traceEnabled) return;
      if (timestamp - state.lastFrame < 32) {
        scheduleTraceFrame();
        return;
      }
      state.lastFrame = timestamp;
      draw(timestamp);
    });
  }

  function hitTest(clientX, clientY) {
    const rectangle = canvas.getBoundingClientRect();
    const x = clientX - rectangle.left;
    const y = clientY - rectangle.top;
    let best = null;
    let bestDistance = 18;
    for (const node of visibleNodes()) {
      const position = worldToScreen(positions.get(node.id));
      const distance = Math.hypot(position.x - x, position.y - y);
      if (distance <= Math.max(bestDistance, nodeRadius(node) + 7) && distance < bestDistance) {
        best = node;
        bestDistance = distance;
      }
    }
    return best;
  }

  function showTooltip(node, clientX, clientY) {
    if (!node) {
      tooltip.classList.remove('open');
      tooltip.replaceChildren();
      return;
    }
    const strong = document.createElement('strong');
    strong.textContent = node.label;
    const small = document.createElement('small');
    small.textContent = `${node.file}:${node.location.replace(/^L/u, '')}`;
    tooltip.replaceChildren(strong, small);
    tooltip.classList.add('open');
    const stageRectangle = stage.getBoundingClientRect();
    tooltip.style.left = `${Math.min(stage.clientWidth - 245, Math.max(10, clientX - stageRectangle.left + 14))}px`;
    tooltip.style.top = `${Math.min(stage.clientHeight - 65, Math.max(68, clientY - stageRectangle.top + 14))}px`;
  }

  function selectNode(node, options = {}) {
    state.selectedId = node?.id || null;
    state.focusedId = node?.id || state.focusedId;
    clearTrace();
    traceButton.disabled = !node;
    inspectorEmpty.hidden = Boolean(node);
    inspectorContent.hidden = !node;
    inspector.classList.toggle('open', Boolean(node));
    if (!node) {
      a11yStatus.textContent = 'Selection cleared';
      draw();
      return;
    }

    const color = colorByCommunity.get(node.community);
    const communityPill = document.querySelector('#inspector-community');
    communityPill.style.color = color;
    communityPill.querySelector('span').textContent = node.communityName;
    document.querySelector('#inspector-kind').textContent = `${node.kind} symbol`;
    document.querySelector('#inspector-title').textContent = node.label;
    document.querySelector('#inspector-id').textContent = node.id;
    document.querySelector('#inspector-file').textContent = node.file;
    document.querySelector('#inspector-location').textContent = node.location || '—';
    document.querySelector('#inspector-degree').textContent = `${node.degree} direct`;

    const connections = [...adjacent.get(node.id)]
      .sort((left, right) => nodeById.get(right.otherId).degree - nodeById.get(left.otherId).degree || nodeById.get(left.otherId).label.localeCompare(nodeById.get(right.otherId).label));
    document.querySelector('#connection-count').textContent = String(connections.length).padStart(2, '0');
    const connectionList = document.querySelector('#connection-list');
    connectionList.replaceChildren();
    for (const connection of connections.slice(0, 10)) {
      const connectedNode = nodeById.get(connection.otherId);
      const button = document.createElement('button');
      button.type = 'button';
      button.className = 'connection-item';
      button.style.color = colorByCommunity.get(connectedNode.community);
      const dot = document.createElement('i');
      const copy = document.createElement('span');
      const name = document.createElement('strong');
      name.textContent = connectedNode.label;
      const relation = document.createElement('small');
      relation.textContent = `${connection.direction === 'out' ? 'outgoing' : 'incoming'} · ${connection.edge.relation}`;
      copy.append(name, relation);
      const degree = document.createElement('em');
      degree.textContent = `${connectedNode.degree}°`;
      button.append(dot, copy, degree);
      button.addEventListener('click', () => selectNode(connectedNode, { center: true }));
      connectionList.append(button);
    }

    if (options.center) centerNode(node.id, true);
    a11yStatus.textContent = `${node.label}, ${node.kind}, ${node.degree} connections, in ${node.communityName}`;
    draw();
  }

  function centerNode(nodeId, zoom = false) {
    const position = positions.get(nodeId);
    if (!position) return;
    if (zoom) state.scale = Math.max(state.scale, 0.95);
    state.offsetX = canvas.clientWidth / 2 - position.x * state.scale;
    state.offsetY = canvas.clientHeight / 2 - position.y * state.scale;
    draw();
  }

  function clearTrace() {
    state.traceEnabled = false;
    state.traceNodeIds.clear();
    state.traceEdgeIndexes.clear();
    if (state.animationFrameId !== null) cancelAnimationFrame(state.animationFrameId);
    state.animationFrameId = null;
    traceButton.classList.remove('active');
    traceButton.setAttribute('aria-pressed', 'false');
  }

  function toggleTrace() {
    if (!state.selectedId) return;
    if (state.traceEnabled) {
      clearTrace();
      a11yStatus.textContent = 'Impact trace cleared';
      draw();
      return;
    }

    state.traceEnabled = true;
    state.traceNodeIds = new Set([state.selectedId]);
    state.traceEdgeIndexes = new Set();
    let frontier = [state.selectedId];
    for (let depth = 0; depth < 3 && frontier.length; depth += 1) {
      const next = [];
      for (const nodeId of frontier) {
        for (const { edge, index } of outgoing.get(nodeId)) {
          if (!state.activeRelationGroups.has(relationGroup(edge))) continue;
          state.traceEdgeIndexes.add(index);
          if (!state.traceNodeIds.has(edge.target)) {
            state.traceNodeIds.add(edge.target);
            next.push(edge.target);
          }
        }
      }
      frontier = next;
    }
    traceButton.classList.add('active');
    traceButton.setAttribute('aria-pressed', 'true');
    a11yStatus.textContent = `Tracing ${state.traceNodeIds.size} downstream symbols across ${state.traceEdgeIndexes.size} relationships`;
    draw();
  }

  function resetFilters() {
    for (const input of relationFilters.querySelectorAll('input')) input.checked = true;
    state.activeRelationGroups = new Set(relationGroups.map((group) => group.id));
    state.density = 'balanced';
    for (const button of document.querySelectorAll('[data-density]')) button.classList.toggle('active', button.dataset.density === 'balanced');
    selectNode(null);
    setCommunity(null);
  }

  function zoomAt(factor, x = canvas.clientWidth / 2, y = canvas.clientHeight / 2) {
    const world = screenToWorld(x, y);
    state.scale = Math.max(0.22, Math.min(3.4, state.scale * factor));
    state.offsetX = x - world.x * state.scale;
    state.offsetY = y - world.y * state.scale;
    draw();
  }

  function chooseDirectionalNode(key) {
    const candidates = visibleNodes();
    if (!candidates.length) return;
    if (!state.focusedId || !positions.has(state.focusedId)) {
      state.focusedId = [...candidates].sort((left, right) => right.degree - left.degree || left.id.localeCompare(right.id))[0].id;
      draw();
      return;
    }
    const current = positions.get(state.focusedId);
    const directions = {
      ArrowLeft: { x: -1, y: 0 },
      ArrowRight: { x: 1, y: 0 },
      ArrowUp: { x: 0, y: -1 },
      ArrowDown: { x: 0, y: 1 },
    };
    const direction = directions[key];
    let best = null;
    let bestScore = Number.POSITIVE_INFINITY;
    for (const candidate of candidates) {
      if (candidate.id === state.focusedId) continue;
      const position = positions.get(candidate.id);
      const dx = position.x - current.x;
      const dy = position.y - current.y;
      const forward = dx * direction.x + dy * direction.y;
      if (forward <= 0) continue;
      const lateral = Math.abs(dx * direction.y - dy * direction.x);
      const score = Math.hypot(dx, dy) + lateral * 1.7;
      if (score < bestScore) {
        best = candidate;
        bestScore = score;
      }
    }
    if (best) {
      state.focusedId = best.id;
      a11yStatus.textContent = `Focused ${best.label}, ${best.degree} connections`;
      draw();
    }
  }

  function updateSearch() {
    const query = search.value.trim().toLocaleLowerCase();
    searchResults.replaceChildren();
    state.searchIndex = -1;
    if (!query) {
      searchResults.classList.remove('open');
      return;
    }

    const terms = query.split(/\s+/u).filter(Boolean);
    const results = graph.nodes
      .map((node) => {
        const haystacks = [node.label.toLocaleLowerCase(), node.id.toLocaleLowerCase(), node.file.toLocaleLowerCase(), node.communityName.toLocaleLowerCase()];
        const score = terms.reduce((total, term) => total + (haystacks[0] === term ? 100 : haystacks[0].startsWith(term) ? 45 : haystacks[0].includes(term) ? 24 : haystacks.some((value) => value.includes(term)) ? 8 : -1000), node.degree);
        return { node, score };
      })
      .filter((result) => result.score > 0)
      .sort((left, right) => right.score - left.score || left.node.label.localeCompare(right.node.label))
      .slice(0, 8);

    for (const result of results) {
      const button = document.createElement('button');
      button.type = 'button';
      button.className = 'search-result';
      button.setAttribute('role', 'option');
      button.setAttribute('aria-selected', 'false');
      button.style.color = colorByCommunity.get(result.node.community);
      const dot = document.createElement('i');
      const copy = document.createElement('span');
      const label = document.createElement('strong');
      label.textContent = result.node.label;
      const file = document.createElement('small');
      file.textContent = result.node.file;
      copy.append(label, file);
      const degree = document.createElement('em');
      degree.textContent = `${result.node.degree}°`;
      button.append(dot, copy, degree);
      button.addEventListener('click', () => chooseSearchResult(result.node));
      searchResults.append(button);
    }
    if (!results.length) {
      const message = document.createElement('p');
      message.className = 'symbol-id';
      message.style.padding = '10px';
      message.textContent = 'No matching symbols';
      searchResults.append(message);
    }
    searchResults.classList.add('open');
  }

  function chooseSearchResult(node) {
    search.value = node.label;
    searchResults.classList.remove('open');
    if (state.communityId && state.communityId !== node.community) setCommunity(null);
    selectNode(node, { center: true });
    canvas.focus();
  }

  function moveSearchSelection(delta) {
    const buttons = [...searchResults.querySelectorAll('.search-result')];
    if (!buttons.length) return;
    state.searchIndex = (state.searchIndex + delta + buttons.length) % buttons.length;
    buttons.forEach((button, index) => button.setAttribute('aria-selected', String(index === state.searchIndex)));
    buttons[state.searchIndex].scrollIntoView({ block: 'nearest' });
  }

  canvas.addEventListener('pointerdown', (event) => {
    canvas.setPointerCapture(event.pointerId);
    state.pointer = { id: event.pointerId, x: event.clientX, y: event.clientY, lastX: event.clientX, lastY: event.clientY, moved: false };
    canvas.classList.add('dragging');
  });

  canvas.addEventListener('pointermove', (event) => {
    if (state.pointer?.id === event.pointerId) {
      const deltaX = event.clientX - state.pointer.lastX;
      const deltaY = event.clientY - state.pointer.lastY;
      if (Math.hypot(event.clientX - state.pointer.x, event.clientY - state.pointer.y) > 3) state.pointer.moved = true;
      state.offsetX += deltaX;
      state.offsetY += deltaY;
      state.pointer.lastX = event.clientX;
      state.pointer.lastY = event.clientY;
      showTooltip(null);
      draw();
      return;
    }
    const node = hitTest(event.clientX, event.clientY);
    if (node?.id !== state.hoveredId) {
      state.hoveredId = node?.id || null;
      draw();
    }
    showTooltip(node, event.clientX, event.clientY);
  });

  function endPointer(event) {
    if (state.pointer?.id !== event.pointerId) return;
    const moved = state.pointer.moved;
    state.pointer = null;
    canvas.classList.remove('dragging');
    if (canvas.hasPointerCapture(event.pointerId)) canvas.releasePointerCapture(event.pointerId);
    if (!moved) {
      const node = hitTest(event.clientX, event.clientY);
      state.focusedId = node?.id || state.focusedId;
      selectNode(node);
    }
  }
  canvas.addEventListener('pointerup', endPointer);
  canvas.addEventListener('pointercancel', endPointer);
  canvas.addEventListener('pointerleave', () => {
    if (!state.pointer) {
      state.hoveredId = null;
      showTooltip(null);
      draw();
    }
  });
  canvas.addEventListener('dblclick', (event) => {
    const node = hitTest(event.clientX, event.clientY);
    if (node) a11yStatus.textContent = `Prototype action: open ${node.file} at ${node.location}`;
  });
  canvas.addEventListener('wheel', (event) => {
    event.preventDefault();
    const rectangle = canvas.getBoundingClientRect();
    zoomAt(Math.exp(-event.deltaY * 0.001), event.clientX - rectangle.left, event.clientY - rectangle.top);
  }, { passive: false });

  canvas.addEventListener('keydown', (event) => {
    if (['ArrowLeft', 'ArrowRight', 'ArrowUp', 'ArrowDown'].includes(event.key)) {
      event.preventDefault();
      chooseDirectionalNode(event.key);
    } else if (event.key === 'Enter' && state.focusedId) {
      event.preventDefault();
      selectNode(nodeById.get(state.focusedId));
    } else if (event.key.toLocaleLowerCase() === 't') {
      event.preventDefault();
      toggleTrace();
    } else if (event.key === 'Escape') {
      selectNode(null);
    } else if (event.key.toLocaleLowerCase() === 'r') {
      resetFilters();
    }
  });

  search.addEventListener('input', updateSearch);
  search.addEventListener('keydown', (event) => {
    if (event.key === 'ArrowDown') {
      event.preventDefault();
      moveSearchSelection(1);
    } else if (event.key === 'ArrowUp') {
      event.preventDefault();
      moveSearchSelection(-1);
    } else if (event.key === 'Enter' && state.searchIndex >= 0) {
      event.preventDefault();
      searchResults.querySelectorAll('.search-result')[state.searchIndex]?.click();
    } else if (event.key === 'Escape') {
      searchResults.classList.remove('open');
      search.blur();
    }
  });
  document.addEventListener('pointerdown', (event) => {
    if (!event.target.closest('.search-wrap')) searchResults.classList.remove('open');
  });
  document.addEventListener('keydown', (event) => {
    if (event.key === '/' && document.activeElement !== search) {
      event.preventDefault();
      search.focus();
    }
  });

  allCommunitiesButton.addEventListener('click', () => setCommunity(null));
  traceButton.addEventListener('click', toggleTrace);
  document.querySelector('#zoom-in').addEventListener('click', () => zoomAt(1.22));
  document.querySelector('#zoom-out').addEventListener('click', () => zoomAt(1 / 1.22));
  document.querySelector('#fit-view').addEventListener('click', fitView);
  document.querySelector('#close-inspector').addEventListener('click', () => selectNode(null));
  document.querySelector('#empty-reset').addEventListener('click', resetFilters);
  document.querySelector('#help-button').addEventListener('click', () => document.querySelector('#shortcut-dialog').showModal());
  document.querySelector('#explain-button').addEventListener('click', () => {
    if (state.selectedId) a11yStatus.textContent = `Prototype action: explain ${nodeById.get(state.selectedId).label}`;
  });
  for (const button of document.querySelectorAll('[data-density]')) {
    button.addEventListener('click', () => {
      state.density = button.dataset.density;
      for (const candidate of document.querySelectorAll('[data-density]')) candidate.classList.toggle('active', candidate === button);
      draw();
    });
  }

  const resizeObserver = new ResizeObserver(resizeCanvas);
  resizeObserver.observe(canvas);
  buildLayout();
  buildNavigation();
  resizeCanvas();

  const previewParameters = new URLSearchParams(globalThis.location.search);
  const previewNode = nodeById.get(previewParameters.get('selected'));
  if (previewNode) {
    selectNode(previewNode);
    if (previewParameters.get('trace') === '1') toggleTrace();
  }

  globalThis.GRAPHOXIDE_CONSTELLATION_DEBUG = Object.freeze({
    fixtureId: graph.fixtureId,
    getState: () => ({
      selectedId: state.selectedId,
      communityId: state.communityId,
      density: state.density,
      traceEnabled: state.traceEnabled,
      visibleNodes: visibleNodes().length,
      visibleEdges: visibleEdges().length,
    }),
    selectNode: (id) => selectNode(nodeById.get(id) || null, { center: true }),
    fitView,
  });
})();
