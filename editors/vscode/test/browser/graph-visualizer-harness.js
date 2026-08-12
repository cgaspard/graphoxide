(() => {
  'use strict';
  const parameters = new URLSearchParams(globalThis.location.search);
  const fixture = parameters.get('fixture') || 'dense';
  const scenario = parameters.get('scenario') || 'default';
  const expectedNodes = Number(parameters.get('nodes') || 0);
  if (parameters.get('scheduler') === 'timer') {
    // Chrome may advance its virtual clock to the test timeout without issuing
    // a native compositor frame. Audit runs use a bounded timer-backed frame;
    // screenshot runs keep the browser's native requestAnimationFrame.
    globalThis.requestAnimationFrame = (callback) => globalThis.setTimeout(() => callback(performance.now()), 16);
    globalThis.cancelAnimationFrame = (handle) => globalThis.clearTimeout(handle);
  }
  const errors = [];
  let rendererState = null;
  let started = false;
  let actionPhase = 0;
  let persistedState = null;
  let finished = false;
  let settleCheckQueued = false;
  let geometryGeneration = 0;
  let requiredGeometryGeneration = 0;
  let initialFitObserved = false;

  globalThis.addEventListener('error', (event) => {
    errors.push(String(event.error?.stack || event.message || event.error));
    finish(globalThis.__graphoxideVisualizerDiagnostics || null);
  });
  globalThis.addEventListener('unhandledrejection', (event) => {
    errors.push(String(event.reason?.stack || event.reason));
    finish(globalThis.__graphoxideVisualizerDiagnostics || null);
  });
  globalThis.acquireVsCodeApi = () => ({
    getState: () => persistedState,
    setState: (value) => { persistedState = value; },
    postMessage: (message) => {
      if (message?.type === 'ready' && !started) {
        started = true;
        void loadFixture();
      } else if (message?.type === 'rendererState') {
        rendererState = message.state;
        scheduleSettleCheck();
      } else if (message?.type === 'geometryDiagnostics') {
        geometryGeneration += 1;
        globalThis.__graphoxideVisualizerDiagnostics = message.diagnostics;
        scheduleSettleCheck();
      }
    },
  });

  async function loadFixture() {
    let graph;
    if (parameters.get('fixtureLoad') === 'sync') {
      // Chrome's command-line screenshot is captured immediately after load.
      // Admit the inert local fixture before that event so fit/draw work can
      // complete within the screenshot's bounded virtual-time budget.
      const request = new XMLHttpRequest();
      request.open('GET', `/fixture.json?name=${encodeURIComponent(fixture)}`, false);
      request.send();
      if (request.status !== 200) throw new Error(`Fixture request failed with ${request.status}.`);
      graph = JSON.parse(request.responseText);
    } else {
      const response = await fetch(`/fixture.json?name=${encodeURIComponent(fixture)}`);
      if (!response.ok) throw new Error(`Fixture request failed with ${response.status}.`);
      graph = await response.json();
    }
    globalThis.postMessage({ type: 'replaceGraph', graph }, globalThis.origin);
    globalThis.setTimeout(() => {
      if (finished) return;
      errors.push('Timed out waiting for the fitted visualizer state.');
      finish(globalThis.__graphoxideVisualizerDiagnostics || null);
    }, 5_000);
    scheduleSettleCheck();
  }

  function scheduleSettleCheck() {
    if (finished || settleCheckQueued) return;
    settleCheckQueued = true;
    // Diagnostics can arrive on Chrome's final virtual-time frame, so react to
    // the renderer message without requiring another animation frame to run.
    globalThis.queueMicrotask(() => {
      settleCheckQueued = false;
      checkSettledGraph();
    });
  }

  function checkSettledGraph() {
    if (finished) return;
    const diagnostics = globalThis.__graphoxideVisualizerDiagnostics;
    const fitted = diagnostics
      && diagnostics.visibleNodes === expectedNodes
      && Math.abs(diagnostics.scale - diagnostics.fittedScale) < 0.000_001;
    if (fitted) initialFitObserved = true;
    if (!initialFitObserved || !rendererState) {
      return;
    }
    if (scenario === 'trace') {
      if (actionPhase === 0) {
        actionPhase = 1;
        globalThis.postMessage({ type: 'testAction', action: 'select-first' }, globalThis.origin);
      } else if (actionPhase === 1 && rendererState.selectedId) {
        actionPhase = 2;
        requiredGeometryGeneration = geometryGeneration + 1;
        globalThis.postMessage({ type: 'testAction', action: 'toggle-trace' }, globalThis.origin);
      } else if (actionPhase === 2 && rendererState.traceActive && geometryGeneration >= requiredGeometryGeneration) {
        finish(diagnostics);
        return;
      }
    } else if (scenario === 'lens') {
      if (actionPhase === 0) {
        actionPhase = 1;
        globalThis.postMessage({ type: 'testAction', action: 'select-first' }, globalThis.origin);
      } else if (actionPhase === 1 && rendererState.selectedId) {
        actionPhase = 2;
        globalThis.postMessage({ type: 'testAction', action: 'enter-focus' }, globalThis.origin);
      } else if (actionPhase === 2 && rendererState.mode === 'focus') {
        finish(diagnostics);
        return;
      }
    } else if (scenario === 'filtered') {
      if (actionPhase === 0) {
        const select = document.querySelector('#gx-community');
        const option = select?.querySelector('option:not([value="community-all"]):not(:disabled)');
        if (!(select instanceof HTMLSelectElement) || !(option instanceof HTMLOptionElement)) {
          errors.push('No community filter option was available.');
          finish(diagnostics);
          return;
        }
        actionPhase = 1;
        requiredGeometryGeneration = geometryGeneration + 1;
        select.value = option.value;
        select.dispatchEvent(new Event('change', { bubbles: true }));
      } else if (actionPhase === 1 && rendererState.communityFilter !== null && geometryGeneration >= requiredGeometryGeneration) {
        finish(globalThis.__graphoxideVisualizerDiagnostics || diagnostics);
        return;
      }
    } else {
      finish(diagnostics);
      return;
    }
  }

  function finish(diagnostics) {
    if (finished) return;
    finished = true;
    const result = document.querySelector('#gx-result');
    if (!(result instanceof HTMLElement)) return;
    const memory = performance.memory;
    result.textContent = JSON.stringify({
      fixture,
      scenario,
      diagnostics: globalThis.__graphoxideVisualizerDiagnostics || diagnostics,
      rendererState,
      errors,
      domElements: document.querySelectorAll('*').length,
      usedJsHeapBytes: typeof memory?.usedJSHeapSize === 'number' ? memory.usedJSHeapSize : null,
    });
    document.documentElement.dataset.graphoxideAuditReady = 'true';
  }
})();
