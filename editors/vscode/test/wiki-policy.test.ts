import assert from 'node:assert/strict';
import * as path from 'node:path';
import test from 'node:test';
import {
  MAX_WIKI_INDEX_BYTES,
  MAX_WIKI_BUILD_RESULT_BYTES,
  parseWikiAuthoringProfile,
  parseWikiBuildSummary,
  parseWikiSources,
  validateWikiHttpsUrl,
  wikiBuildArguments,
  wikiBuildCompletionMessage,
  wikiConsentArguments,
  wikiModelDestination,
  wikiModelDisclosure,
  wikiPageRelativePath,
  wikiProjectPath,
} from '../src/wiki-policy';

const root = path.resolve('/workspace/wiki');
const sourceId = `src:${'a'.repeat(64)}`;
const source = {
  source_id: sourceId,
  location: { kind: 'bound-path', binding: 'docs', path: 'design/Overview.md' },
  content_sha256: 'b'.repeat(64),
  bytes: 42,
  status: 'provisional',
};
const index = (sources: readonly unknown[]): string => JSON.stringify({ schema: 'graphoxide.source-index', sources });

test('Wiki build requires explicit model and HTTPS transfer consent', () => {
  const local = path.join(root, 'source file.md');
  assert.throws(() => wikiBuildArguments([local], { allowModelEgress: false, allowNetwork: false }), /explicit permission/);
  assert.throws(() => wikiBuildArguments(['https://example.org/docs'], { allowModelEgress: true, allowNetwork: false }), /network permission/);
  assert.deepEqual(wikiBuildArguments([local], { allowModelEgress: true, allowNetwork: false }), ['wiki', 'source', 'add', '--allow-model-egress', local]);
  assert.deepEqual(wikiBuildArguments([local, 'https://example.org/docs'], { allowModelEgress: true, allowNetwork: true }), [
    'wiki', 'source', 'add', '--allow-model-egress', '--allow-network', local, 'https://example.org/docs',
  ]);
  assert.deepEqual(wikiConsentArguments({ allowModelEgress: true, allowNetwork: true }, false), ['--allow-model-egress']);
});

test('Wiki source arguments cannot be confused with CLI options or carry URL credentials', () => {
  for (const input of ['--force', 'relative/file.md', 'http://example.org/docs', 'https://user:secret@example.org/docs', 'https://example.org/docs?token=secret']) {
    assert.throws(() => wikiBuildArguments([input], { allowModelEgress: true, allowNetwork: true }));
  }
  assert.throws(() => wikiBuildArguments([], { allowModelEgress: true, allowNetwork: true }), /at least one/);
  assert.equal(validateWikiHttpsUrl('https://example.org/docs'), undefined);
  assert.ok(validateWikiHttpsUrl('https://example.org'));
  assert.ok(validateWikiHttpsUrl('https://example.org/docs#section'));
});

test('reads direct-source lifecycle state without retaining raw source content', () => {
  assert.deepEqual(parseWikiSources(index([source])), [{ id: sourceId, label: 'design/Overview.md', status: 'provisional', remote: false }]);
  const https = { ...source, source_id: `src:${'b'.repeat(64)}`, location: { kind: 'https', url: 'https://example.org/docs' }, status: 'ai-reviewed' };
  assert.equal(parseWikiSources(index([https]))[0]?.remote, true);
});

test('rejects malformed, duplicate, and oversized source metadata', () => {
  assert.throws(() => parseWikiSources(index([source, source])), /duplicate/);
  assert.throws(() => parseWikiSources(index([{ ...source, source_id: '../../outside' }])), /source ID/);
  assert.throws(() => parseWikiSources(index([{ ...source, status: 'approved' }])), /source status/);
  assert.throws(() => parseWikiSources(index([{ ...source, location: { kind: 'shell', command: 'untrusted' } }])), /source location/);
  assert.throws(() => parseWikiSources(' '.repeat(MAX_WIKI_INDEX_BYTES + 1)), /size limit/);
  assert.throws(() => parseWikiSources('{}'), /Unsupported/);
});

test('authoring profile requires explicit models, consent, and project-contained provider path', () => {
  const profile = { provider_profile: 'providers/local.json', author_model: 'writer', reviewer_model: 'reviewer', source_egress_consent: 'explicit source consent' };
  assert.deepEqual(parseWikiAuthoringProfile(JSON.stringify(profile), root), { providerProfile: 'providers/local.json', authorModel: 'writer', reviewerModel: 'reviewer' });
  for (const provider_profile of ['../outside.json', '/outside.json', 'providers/../../outside.json', 'providers\\outside.json']) {
    assert.throws(() => parseWikiAuthoringProfile(JSON.stringify({ ...profile, provider_profile }), root), /inside the workspace/);
  }
  assert.throws(() => parseWikiAuthoringProfile(JSON.stringify({ ...profile, source_egress_consent: '' }), root));
  assert.throws(() => parseWikiAuthoringProfile(JSON.stringify({ ...profile, author_model: '' }), root));
  assert.equal(wikiProjectPath(root, 'providers/local.json'), path.join(root, 'providers/local.json'));
});

test('generated page locations derive only from validated source identities', () => {
  const parsed = parseWikiSources(index([source]))[0]!;
  assert.equal(wikiPageRelativePath(parsed), `content/provisional/source-${'a'.repeat(64)}.md`);
  assert.throws(() => wikiPageRelativePath({ ...parsed, id: '../outside' }), /Invalid Wiki source ID/);
});

test('model consent identifies the exact configured endpoint and enabled API model', () => {
  const provider = { endpoint: 'http://127.0.0.1:11434', models: [{ id: 'author', api_model: 'chosen-model:latest' }] };
  assert.deepEqual(wikiModelDestination(JSON.stringify(provider), 'author'), { endpoint: provider.endpoint, model: 'chosen-model:latest' });
  assert.throws(() => wikiModelDestination(JSON.stringify(provider), 'missing'), /unavailable/);
  assert.throws(() => wikiModelDestination(JSON.stringify({ ...provider, endpoint: 'https://user:secret@example.org/' }), 'author'), /without embedded credentials/);
  assert.throws(() => wikiModelDestination(JSON.stringify({ ...provider, models: [{ ...provider.models[0], enabled: false }] }), 'author'), /unavailable/);
});

test('YAML providers require reviewing the exact file instead of guessing YAML fields', () => {
  for (const provider of ['config/provider.yaml', 'config/provider.yml']) {
    const disclosure = wikiModelDisclosure('endpoint: https://example.org\nmodels:\n  - id: author\n', provider, 'author');
    assert.equal(disclosure.reviewFile, true);
    assert.ok(disclosure.description.includes(provider));
    assert.match(disclosure.description, /model alias “author”/u);
    assert.match(disclosure.description, /Review the endpoint and API model/u);
    assert.doesNotMatch(disclosure.description, /https:\/\/example\.org/u);
  }
  const json = JSON.stringify({ endpoint: 'http://127.0.0.1:11434', models: [{ id: 'author', api_model: 'chosen' }] });
  assert.equal(wikiModelDisclosure(json, 'config/provider.json', 'author').reviewFile, false);
});

test('Wiki completion reports partial directory imports from structured outcomes', () => {
  const summary = parseWikiBuildSummary(JSON.stringify({
    sources: [source],
    authored: [{ source_id: sourceId, page_id: `source-${'a'.repeat(64)}`, status: 'provisional' }],
    outcomes: [
      { outcome: 'added', source },
      { outcome: 'error', error: { kind: 'oversized', location: { path: 'too-large.txt' } } },
      { outcome: 'error', error: { kind: 'unsafe-or-unreadable', location: { path: 'unreadable.txt' } } },
      { outcome: 'skipped', skip: { kind: 'structural-file', location: { path: '.gitignore' } } },
    ],
  }));
  assert.deepEqual(summary, { generatedPages: 1, sourceErrors: 2, skippedInputs: 1 });
  assert.equal(wikiBuildCompletionMessage(summary), 'Generated 1 Wiki page. 2 inputs could not be read; 1 input skipped. See Graphoxide Output for details.');
  assert.doesNotMatch(wikiBuildCompletionMessage(summary), /too-large|unreadable\.txt|gitignore/u);
});

test('Wiki completion handles whole success and empty results without inventing pages', () => {
  assert.equal(wikiBuildCompletionMessage({ generatedPages: 3, sourceErrors: 0, skippedInputs: 0 }), 'Generated 3 Wiki pages.');
  const empty = parseWikiBuildSummary('{"sources":[],"authored":[],"outcomes":[]}');
  assert.equal(wikiBuildCompletionMessage(empty), 'No Wiki pages generated.');
  assert.deepEqual(parseWikiBuildSummary(JSON.stringify({ authored: [{ source_id: sourceId, page_id: 'source-page' }], outcomes: [] })), {
    generatedPages: 1, sourceErrors: 0, skippedInputs: 0,
  });
});

test('Wiki completion rejects malformed or oversized output and unknown outcome kinds', () => {
  assert.throws(() => parseWikiBuildSummary('{"authored":{},"outcomes":[]}'), /readable build summary/u);
  assert.throws(() => parseWikiBuildSummary('{"authored":[null],"outcomes":[]}'), /Invalid Wiki metadata/u);
  assert.throws(() => parseWikiBuildSummary('{"authored":[],"outcomes":[{"outcome":"unknown"}]}'), /unknown source outcome/u);
  assert.throws(() => parseWikiBuildSummary(' '.repeat(MAX_WIKI_BUILD_RESULT_BYTES + 1)), /size limit/u);
});
