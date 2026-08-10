import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import vm from 'node:vm';

const root = new URL('./', import.meta.url);
const read = (path) => readFile(new URL(path, root), 'utf8');

const [chooserHtml, chooserCss, chooserApp, readme, fixtureSource] = await Promise.all([
  read('index.html'),
  read('comparison.css'),
  read('comparison.js'),
  read('README.md'),
  read('shared/fixture.js'),
]);

new vm.Script(chooserApp, { filename: 'graph-concepts/comparison.js' });

const makeElement = (id = '') => {
  const classes = new Set();
  const attributes = new Map();
  const listeners = new Map();
  const element = {
    id,
    dataset: {},
    tabIndex: 0,
    textContent: '',
    title: '',
    href: '',
    children: [],
    focused: false,
    classList: {
      add: (...names) => names.forEach((name) => classes.add(name)),
      remove: (...names) => names.forEach((name) => classes.delete(name)),
      toggle: (name, force) => force ? classes.add(name) : classes.delete(name),
      contains: (name) => classes.has(name),
    },
    setAttribute: (name, value) => attributes.set(name, String(value)),
    getAttribute: (name) => attributes.get(name) ?? null,
    addEventListener: (type, listener) => listeners.set(type, listener),
    replaceChildren: (...children) => { element.children = children; },
    focus: () => { element.focused = true; },
    listeners,
  };
  return element;
};

const chooserElements = new Map([
  'concept-frame',
  'open-concept',
  'preview-path',
  'selection-kicker',
  'selection-summary',
  'selection-tags',
  'preview-prompt',
  'preview-loading',
].map((id) => [id, makeElement(id)]));
const chooserTabs = ['constellation', 'semantic-atlas', 'investigation-lens'].map((id) => {
  const tab = makeElement(`tab-${id}`);
  tab.dataset.concept = id;
  return tab;
});
const chooserFrame = chooserElements.get('concept-frame');
let frameSource = 'constellation/?selected=checkout-service&trace=1';
Object.defineProperty(chooserFrame, 'src', {
  get: () => frameSource,
  set: (value) => { frameSource = value; chooserFrame.setAttribute('src', value); },
});
chooserFrame.setAttribute('src', frameSource);
let replacedHash = '';
const chooserContext = vm.createContext({
  document: {
    querySelectorAll: (selector) => selector === '[data-concept]' ? chooserTabs : [],
    getElementById: (id) => chooserElements.get(id),
    createElement: () => makeElement(),
  },
  location: { hash: '#review-semantic-atlas' },
  history: { replaceState: (_state, _unused, hash) => { replacedHash = hash; } },
});
vm.runInContext(chooserApp, chooserContext, { filename: 'graph-concepts/comparison.js' });
assert.equal(chooserElements.get('preview-path').textContent, 'semantic-atlas/', 'hash must select Semantic Atlas');
assert.equal(chooserFrame.title, 'Live preview of Semantic Atlas');
assert.equal(chooserFrame.getAttribute('aria-labelledby'), 'tab-semantic-atlas');
chooserTabs[2].listeners.get('click')();
assert.equal(chooserElements.get('open-concept').href, 'investigation-lens/?select=stripe-adapter&trace=1');
assert.equal(replacedHash, '#review-investigation-lens');
chooserTabs[2].listeners.get('keydown')({ key: 'ArrowDown', preventDefault() {} });
assert.equal(chooserElements.get('preview-path').textContent, 'constellation/?selected=checkout-service&trace=1', 'tab arrows must wrap');

const chooserIds = [...chooserHtml.matchAll(/\bid="([^"]+)"/gu)].map((match) => match[1]);
assert.equal(chooserIds.length, new Set(chooserIds).size, 'chooser HTML IDs must be unique');

for (const id of [...chooserApp.matchAll(/getElementById\('([^']+)'\)/gu)].map((match) => match[1])) {
  assert.ok(chooserIds.includes(id), `comparison.js references missing element #${id}`);
}

assert.match(chooserHtml, /role="tablist"/u, 'chooser must expose a tab list');
assert.match(chooserHtml, /id="concept-frame"[^>]+title=/u, 'live preview iframe must have a title');
assert.match(chooserHtml, /id="concept-frame"[^>]+role="tabpanel"/u, 'live preview must be associated with the selected tab');
assert.match(chooserHtml, /id="matrix-note"/u, 'decision matrix must have explanatory context');
assert.match(chooserCss, /prefers-reduced-motion:\s*reduce/u, 'chooser must support reduced motion');
assert.match(chooserCss, /forced-colors:\s*active/u, 'chooser must support forced colors');
assert.doesNotMatch(chooserHtml + chooserCss + chooserApp, /(?:src|href)="https?:\/\//u, 'chooser assets must remain local');

const requiredFactors = [
  'Best use case',
  'Visual identity',
  'Dense-graph legibility',
  'Investigation speed',
  'Accessibility',
  'Performance strategy',
  'Production integration risk',
  'Most reusable ideas',
];
for (const factor of requiredFactors) assert.ok(chooserHtml.includes(factor), `missing comparison factor: ${factor}`);
assert.match(chooserHtml, /Recommended hybrid/u, 'chooser must state a hybrid recommendation');
assert.match(chooserHtml, /Pieces that combine coherently/u, 'chooser must explain compatible ideas');
assert.match(chooserHtml, /Pieces that should stay separate/u, 'chooser must explain conflicting ideas');
assert.match(readme, /not aesthetics alone/u, 'recommendation must not be based only on appearance');

const concepts = [
  { id: 'constellation', label: 'Constellation' },
  { id: 'semantic-atlas', label: 'Semantic Atlas' },
  { id: 'investigation-lens', label: 'Investigation Lens' },
];

for (const concept of concepts) {
  const [html, css, app] = await Promise.all([
    read(`${concept.id}/index.html`),
    read(`${concept.id}/styles.css`),
    read(`${concept.id}/app.js`),
  ]);
  new vm.Script(app, { filename: `${concept.id}/app.js` });
  assert.match(html, /<script src="\.\.\/shared\/fixture\.js"><\/script>/u, `${concept.label} must load the shared fixture`);
  assert.match(html, /<script src="app\.js"><\/script>/u, `${concept.label} must load its application`);
  assert.ok(html.indexOf('../shared/fixture.js') < html.indexOf('app.js'), `${concept.label} must load fixture before app`);
  assert.match(app, /contractVersion\s*!==\s*1/u, `${concept.label} must guard fixture contract v1`);
  assert.doesNotMatch(app, /\b(?:fetch|XMLHttpRequest|WebSocket)\s*\(/u, `${concept.label} must remain network-independent`);
  assert.doesNotMatch(html + css, /(?:src|href)="https?:\/\//u, `${concept.label} assets must remain local`);
  assert.ok(chooserHtml.includes(`${concept.id}/`), `chooser must link to ${concept.label}`);
}

const fixtureContext = vm.createContext({});
vm.runInContext(fixtureSource, fixtureContext, { filename: 'shared/fixture.js' });
const fixture = fixtureContext.GRAPHOXIDE_GRAPH_FIXTURE;
assert.equal(fixture.contractVersion, 1);
assert.equal(fixture.nodes.length, 42);
assert.equal(fixture.edges.length, 71);
assert.equal(new Set(fixture.nodes.map((node) => node.community)).size, 6);
assert.ok(Object.isFrozen(fixture), 'shared fixture must remain immutable');

console.log(`Graph concept review OK: ${concepts.length} runnable concepts, ${fixture.nodes.length} nodes, ${fixture.edges.length} edges`);
