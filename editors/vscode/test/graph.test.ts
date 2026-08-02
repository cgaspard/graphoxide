import assert from 'node:assert/strict';
import test from 'node:test';
import { GraphModel, parseGraphJson, sourceLine } from '../src/graph';

const fixture = JSON.stringify({
  directed: false,
  nodes: [
    { id: 'fn:a', label: 'authenticate', file_type: 'code', source_file: 'src/auth.ts', source_location: 'L10', community: 1 },
    { id: 'fn:b', label: 'loadUser', file_type: 'code', source_file: 'src/user.ts', source_location: '20', community: 1, community_name: 'Identity' },
    { id: 'doc:c', label: 'Architecture', file_type: 'document', source_file: 'README.md' },
  ],
  links: [
    { source: 'fn:a', target: 'fn:b', relation: 'calls', confidence: 'EXTRACTED' },
    { source: 'fn:a', target: 'doc:c', _src: 'doc:c', _tgt: 'fn:a', relation: 'references' },
  ],
});

test('parses and indexes a Graphoxide graph', () => {
  const model = new GraphModel(parseGraphJson(fixture));
  assert.equal(model.snapshot.nodes.length, 3);
  assert.equal(model.snapshot.edges[1]?.source, 'doc:c');
  assert.equal(model.degree('fn:a'), 2);
  assert.deepEqual(model.neighbors('fn:a').map((node) => node.label), ['Architecture', 'loadUser']);
  assert.equal(model.communities()[0]?.name, 'Identity');
});

test('search ranks exact and prefix label matches', () => {
  const model = new GraphModel(parseGraphJson(fixture));
  assert.equal(model.search('authenticate')[0]?.id, 'fn:a');
  assert.equal(model.search('load')[0]?.id, 'fn:b');
  assert.equal(model.nodesForSourceFile('src/auth.ts')[0]?.label, 'authenticate');
});

test('reads source locations defensively', () => {
  const model = new GraphModel(parseGraphJson(fixture));
  assert.equal(sourceLine(model.getNode('fn:a')!), 10);
  assert.equal(sourceLine(model.getNode('doc:c')!), 1);
});

test('accepts the raw edges key and rejects malformed graphs', () => {
  assert.equal(parseGraphJson('{"nodes":[],"edges":[]}').edges.length, 0);
  assert.throws(() => parseGraphJson('{"nodes":[]}'), /links/);
  assert.throws(() => parseGraphJson('{broken'), /valid JSON/);
});
