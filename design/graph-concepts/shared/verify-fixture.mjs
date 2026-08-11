import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import vm from 'node:vm';

const fixtureUrl = new URL('./fixture.js', import.meta.url);
const source = await readFile(fixtureUrl, 'utf8');
const context = vm.createContext({});
vm.runInContext(source, context, { filename: fixtureUrl.pathname });
const fixture = context.GRAPHOXIDE_GRAPH_FIXTURE;

assert.equal(fixture.contractVersion, 1);
assert.equal(fixture.fixtureId, 'cartograph-checkout-v1');
assert.equal(fixture.directed, true);
assert.ok(fixture.nodes.length >= 40, 'fixture must exercise a moderately dense graph');
assert.ok(fixture.edges.length >= fixture.nodes.length, 'fixture must have more edges than nodes');

const nodeIds = new Set();
const communities = new Map();
for (const node of fixture.nodes) {
  assert.equal(typeof node.id, 'string');
  assert.equal(typeof node.label, 'string');
  assert.equal(typeof node.file, 'string');
  assert.equal(typeof node.location, 'string');
  assert.equal(typeof node.kind, 'string');
  assert.equal(typeof node.community, 'string');
  assert.equal(typeof node.communityName, 'string');
  assert.equal(typeof node.degree, 'number');
  assert.ok(!nodeIds.has(node.id), `duplicate node: ${node.id}`);
  nodeIds.add(node.id);
  const previousName = communities.get(node.community);
  assert.ok(!previousName || previousName === node.communityName, `community name mismatch: ${node.community}`);
  communities.set(node.community, node.communityName);
}

const degreeById = new Map([...nodeIds].map((id) => [id, 0]));
const edgeKeys = new Set();
for (const edge of fixture.edges) {
  assert.ok(nodeIds.has(edge.source), `missing edge source: ${edge.source}`);
  assert.ok(nodeIds.has(edge.target), `missing edge target: ${edge.target}`);
  assert.match(edge.relation, /^[a-z][a-z_]*$/u);
  assert.match(edge.confidence, /^(EXTRACTED|INFERRED|AMBIGUOUS)$/u);
  const key = `${edge.source}\u0000${edge.target}\u0000${edge.relation}`;
  assert.ok(!edgeKeys.has(key), `duplicate edge: ${key}`);
  edgeKeys.add(key);
  degreeById.set(edge.source, degreeById.get(edge.source) + 1);
  if (edge.target !== edge.source) degreeById.set(edge.target, degreeById.get(edge.target) + 1);
}

for (const node of fixture.nodes) {
  assert.equal(node.degree, degreeById.get(node.id), `incorrect degree: ${node.id}`);
}

const assertDeepFrozen = (value) => {
  if (!value || typeof value !== 'object') return;
  assert.ok(Object.isFrozen(value), 'fixture values must be frozen');
  for (const child of Object.values(value)) assertDeepFrozen(child);
};
assertDeepFrozen(fixture);

console.log(`Fixture OK: ${fixture.nodes.length} nodes, ${fixture.edges.length} edges, ${communities.size} communities`);
