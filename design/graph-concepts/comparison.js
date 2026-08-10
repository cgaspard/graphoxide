(function installConceptChooser() {
  'use strict';

  const concepts = {
    constellation: {
      label: 'Constellation',
      path: 'constellation/?selected=checkout-service&trace=1',
      kicker: 'System landscape',
      summary: 'A cinematic, clustered canvas for discovery and directed impact tracing.',
      tags: ['Canvas', 'Overview', 'Impact'],
      prompt: 'Try search, density controls, and Trace impact.',
    },
    'semantic-atlas': {
      label: 'Semantic Atlas',
      path: 'semantic-atlas/',
      kicker: 'Architecture map',
      summary: 'An ordered domain atlas for reading architecture and evidence from upstream to dependencies.',
      tags: ['SVG', 'Domains', 'Semantic zoom'],
      prompt: 'Try Structure or Detail, domain filters, and the minimap.',
    },
    'investigation-lens': {
      label: 'Investigation Lens',
      path: 'investigation-lens/?select=stripe-adapter&trace=1',
      kicker: 'Focused workflow',
      summary: 'A bounded caller-and-impact workspace built to answer one code question quickly.',
      tags: ['DOM + SVG', 'Neighborhood', 'Evidence'],
      prompt: 'Try hop depth, history, path trace, and Set as focus.',
    },
  };

  const tabs = [...document.querySelectorAll('[data-concept]')];
  const frame = document.getElementById('concept-frame');
  const openLink = document.getElementById('open-concept');
  const pathLabel = document.getElementById('preview-path');
  const kicker = document.getElementById('selection-kicker');
  const summary = document.getElementById('selection-summary');
  const tags = document.getElementById('selection-tags');
  const prompt = document.getElementById('preview-prompt');
  const loading = document.getElementById('preview-loading');
  let selectedId = 'constellation';

  function selectConcept(id, options = {}) {
    const concept = concepts[id];
    if (!concept) return;
    selectedId = id;

    for (const tab of tabs) {
      const active = tab.dataset.concept === id;
      tab.classList.toggle('is-active', active);
      tab.setAttribute('aria-selected', String(active));
      tab.tabIndex = active ? 0 : -1;
      if (active && options.focus) tab.focus();
    }

    kicker.textContent = concept.kicker;
    summary.textContent = concept.summary;
    tags.replaceChildren(...concept.tags.map((tag) => {
      const badge = document.createElement('span');
      badge.textContent = tag;
      return badge;
    }));
    prompt.textContent = concept.prompt;
    pathLabel.textContent = concept.path;
    openLink.href = concept.path;
    frame.title = `Live preview of ${concept.label}`;
    frame.setAttribute('aria-labelledby', `tab-${id}`);

    if (frame.getAttribute('src') !== concept.path) {
      loading.classList.add('is-visible');
      frame.classList.add('is-loading');
      frame.src = concept.path;
    }

    if (options.updateHash) history.replaceState(null, '', `#review-${id}`);
  }

  for (const tab of tabs) {
    tab.addEventListener('click', () => selectConcept(tab.dataset.concept, { updateHash: true }));
    tab.addEventListener('keydown', (event) => {
      if (!['ArrowDown', 'ArrowUp', 'Home', 'End'].includes(event.key)) return;
      event.preventDefault();
      const currentIndex = tabs.findIndex((candidate) => candidate.dataset.concept === selectedId);
      const nextIndex = event.key === 'Home'
        ? 0
        : event.key === 'End'
          ? tabs.length - 1
          : (currentIndex + (event.key === 'ArrowDown' ? 1 : -1) + tabs.length) % tabs.length;
      selectConcept(tabs[nextIndex].dataset.concept, { focus: true, updateHash: true });
    });
  }

  frame.addEventListener('load', () => {
    loading.classList.remove('is-visible');
    frame.classList.remove('is-loading');
  });

  const requestedId = location.hash.startsWith('#review-') ? location.hash.slice('#review-'.length) : '';
  if (concepts[requestedId]) selectConcept(requestedId);
})();
