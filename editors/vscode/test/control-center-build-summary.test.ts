import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import path from 'node:path';
import test from 'node:test';
import { runInNewContext } from 'node:vm';

interface FakeElement {
  className: string;
  disabled: boolean;
  hidden: boolean;
  innerHTML: string;
  textContent: string;
  addEventListener(type: string, listener: () => void): void;
}

type MessageListener = (event: { readonly data: unknown }) => void;

test('Control Center conditionally renders the latest successful index summary with bounded formats', async () => {
  const source = await readFile(path.join(process.cwd(), 'src', 'control-center.ts'), 'utf8');
  const startMarker = '<script nonce="${nonce}">';
  const start = source.indexOf(startMarker);
  const end = source.indexOf('</script>', start);
  assert.ok(start >= 0 && end > start, 'expected the Control Center inline script');
  const script = source.slice(start + startMarker.length, end);

  const elements = new Map<string, FakeElement>();
  const element = (id: string): FakeElement => {
    const existing = elements.get(id);
    if (existing) return existing;
    const created: FakeElement = {
      className: '',
      disabled: false,
      hidden: false,
      innerHTML: '',
      textContent: '',
      addEventListener: () => undefined,
    };
    elements.set(id, created);
    return created;
  };
  let receiveMessage: MessageListener | undefined;
  runInNewContext(script, {
    acquireVsCodeApi: () => ({ postMessage: () => undefined }),
    document: {
      getElementById: element,
      querySelectorAll: () => [],
    },
    window: {
      addEventListener: (type: string, listener: MessageListener) => {
        if (type === 'message') receiveMessage = listener;
      },
    },
  });
  assert.ok(receiveMessage, 'expected a VS Code message listener');

  const state = {
    type: 'state',
    workspace: { name: 'fixture', path: '/fixture', trusted: true },
    graph: {
      status: 'ready', exists: true, path: '/fixture/graphoxide-out/graph.json', error: null,
      nodes: 3, edges: 2, communities: 1, modified: 1, builtAtCommit: null, latestIndex: null,
    },
    managed: { enabled: false, freshness: 'manual', watching: false },
    ai: {
      enabled: false, provider: null, endpoint: null, model: null, credentialPresent: false,
      credentialRequired: false, executable: null, timeoutSeconds: 600,
    },
    mcp: { nativeEnabled: false, invocation: 'graphoxide serve', configuredScopes: 0, staleScopes: 0, rows: [] },
  };
  receiveMessage({ data: state });
  assert.doesNotMatch(element('content').innerHTML, /Latest index|Indexed source size|Stages/u);

  receiveMessage({
    data: {
      ...state,
      graph: {
        ...state.graph,
        latestIndex: {
          operation: 'update', mode: 'incremental', status: 'rebuilt', elapsedMs: 2450,
          stagesMs: { scan_extract: 1200, detect: 0, extract: 0, build: 25, cluster: 0, write: 10 },
          files: { indexed: 5, changed: 2, deleted: 1 },
          sourceBytes: 1536,
          completedAt: 1,
        },
      },
    },
  });
  const rendered = element('content').innerHTML;
  assert.match(rendered, /Latest index/u);
  assert.match(rendered, /<dt>Total time<\/dt><dd>2\.5 s<\/dd>/u);
  assert.match(rendered, /<dt>Operation<\/dt><dd>Incremental update<\/dd>/u);
  assert.match(rendered, /<dt>Indexed inputs<\/dt><dd>5<\/dd>/u);
  assert.match(rendered, /<dt>Indexed source size<\/dt><dd>1\.5 KiB<\/dd>/u);
  assert.match(rendered, /<dt>Changed \/ deleted<\/dt><dd>2 \/ 1<\/dd>/u);
  assert.match(rendered, /scan\/extract 1\.2 s · build 25 ms · write 10 ms/u);
  assert.doesNotMatch(rendered, /codebase size|repository size/iu);

  receiveMessage({
    data: {
      ...state,
      graph: {
        ...state.graph,
        latestIndex: {
          operation: 'extract', mode: 'full', status: 'rebuilt', elapsedMs: 100,
          stagesMs: { scan_extract: 0, detect: 0, extract: 50, build: 25, cluster: 0, write: 10 },
          files: { indexed: 8, changed: 8, deleted: 0 },
          completedAt: 1,
        },
      },
    },
  });
  assert.doesNotMatch(element('content').innerHTML, /Changed \/ deleted/u);
});
