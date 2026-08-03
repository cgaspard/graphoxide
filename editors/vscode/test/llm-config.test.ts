import assert from 'node:assert/strict';
import * as path from 'node:path';
import test from 'node:test';
import {
  AI_PROVIDER_PROFILES,
  AiLabelingConfiguration,
  aiSecretKey,
  credentialForEndpoint,
  encodeStoredCredential,
  isLoopbackUrl,
  labelingArguments,
  labelingEnvironment,
  modelDiscoveryUrl,
  normalizeProviderBaseUrl,
  overlayEnvironment,
  parseDiscoveredModels,
  shouldUseTrustedExecutable,
  trustedExecutableCandidates,
} from '../src/llm/config';

const profile = (id: string) => {
  const match = AI_PROVIDER_PROFILES.find((candidate) => candidate.id === id);
  assert.ok(match, `missing ${id} test profile`);
  return match;
};

test('normalizes local provider URLs and pins official OpenAI', () => {
  assert.equal(normalizeProviderBaseUrl(profile('lm-studio'), 'http://localhost:1234'), 'http://localhost:1234/v1');
  assert.equal(normalizeProviderBaseUrl(profile('ollama'), 'http://127.0.0.1:11434/'), 'http://127.0.0.1:11434/v1');
  assert.equal(
    normalizeProviderBaseUrl(profile('openai'), 'http://localhost:9999/v1'),
    'https://api.openai.com/v1',
  );
});

test('rejects unsafe endpoint forms and non-loopback plaintext HTTP', () => {
  assert.throws(
    () => normalizeProviderBaseUrl(profile('openai-compatible'), 'ftp://example.test/v1'),
    /http or https/u,
  );
  assert.throws(
    () => normalizeProviderBaseUrl(profile('openai-compatible'), 'https://secret@example.test/v1'),
    /Secret Storage/u,
  );
  assert.throws(
    () => normalizeProviderBaseUrl(profile('openai-compatible'), 'https://example.test/v1?key=secret'),
    /query string/u,
  );
  assert.throws(
    () => normalizeProviderBaseUrl(profile('openai-compatible'), 'http://192.0.2.10/v1'),
    /must use https/u,
  );
  assert.equal(normalizeProviderBaseUrl(profile('openai-compatible'), 'https://example.test/v1/'), 'https://example.test/v1');
});

test('recognizes loopback hosts without accepting lookalike DNS names', () => {
  assert.equal(isLoopbackUrl('http://localhost:1234/v1'), true);
  assert.equal(isLoopbackUrl('http://127.99.10.3:1234/v1'), true);
  assert.equal(isLoopbackUrl('http://[::1]:1234/v1'), true);
  assert.equal(isLoopbackUrl('http://127.0.0.1evil:1234/v1'), false);
  assert.equal(isLoopbackUrl('http://127.0.0.1.example:1234/v1'), false);
  assert.equal(isLoopbackUrl('http://localhost.example:1234/v1'), false);
});

test('keeps provider credentials isolated in VS Code Secret Storage keys', () => {
  const keys = [profile('openai'), profile('lm-studio'), profile('ollama')].map(aiSecretKey);
  assert.equal(new Set(keys).size, keys.length);
});

test('binds each stored credential to its normalized endpoint', () => {
  const stored = encodeStoredCredential('http://127.0.0.1:1234/v1', 'lm-secret');
  assert.equal(credentialForEndpoint(stored, 'http://127.0.0.1:1234/v1'), 'lm-secret');
  assert.equal(credentialForEndpoint(stored, 'http://127.0.0.1:9999/v1'), undefined);
  assert.equal(credentialForEndpoint('legacy-unbound-secret', 'http://127.0.0.1:1234/v1'), undefined);
  assert.equal(credentialForEndpoint('{"version":1,"baseUrl":"http://127.0.0.1:1234/v1"}', 'http://127.0.0.1:1234/v1'), undefined);
});

test('builds a sanitized child environment without mutating the extension host', () => {
  const base = {
    PATH: '/safe/bin',
    OPENAI_API_KEY: 'inherited-openai',
    OLLAMA_API_KEY: 'inherited-ollama',
    OPENAI_BASE_URL: 'https://inherited.invalid/v1',
    GRAPHIFY_API_TIMEOUT: '0.01',
    OpenAI_Api_Key: 'mixed-case-inherited-key',
    Graphify_Api_Timeout: '0.02',
    OpenAI_Base_Url: 'https://mixed-case.invalid/v1',
  };
  const environment = labelingEnvironment(profile('lm-studio'), 'http://127.0.0.1:1234/v1');
  const result = overlayEnvironment(base, environment);
  assert.equal(result.PATH, '/safe/bin');
  assert.equal(result.OPENAI_API_KEY, undefined);
  assert.equal(result.OpenAI_Api_Key, undefined);
  assert.equal(result.OLLAMA_API_KEY, undefined);
  assert.equal(result.OPENAI_BASE_URL, undefined);
  assert.equal(result.OpenAI_Base_Url, undefined);
  assert.equal(result.GRAPHIFY_API_TIMEOUT, undefined);
  assert.equal(result.Graphify_Api_Timeout, undefined);
  assert.equal(result.GRAPHOXIDE_LLM_BASE_URL, 'http://127.0.0.1:1234/v1');
  assert.equal(result.GRAPHOXIDE_LLM_PROVIDER, 'lm-studio');
  assert.equal(base.OPENAI_API_KEY, 'inherited-openai');

  const keyed = overlayEnvironment(base, labelingEnvironment(profile('lm-studio'), 'http://127.0.0.1:1234/v1', 'stored-key'));
  assert.equal(keyed.OPENAI_API_KEY, 'stored-key');
  assert.equal(keyed.OpenAI_Api_Key, undefined);
});

test('any API key automatically requires the trusted executable path', () => {
  assert.equal(shouldUseTrustedExecutable(false, { OPENAI_API_KEY: 'secret' }), true);
  assert.equal(shouldUseTrustedExecutable(undefined, { OLLAMA_API_KEY: ' secret ' }), true);
  assert.equal(shouldUseTrustedExecutable(false, { OPENAI_API_KEY: undefined }), false);
  assert.equal(shouldUseTrustedExecutable(true, undefined), true);
});

test('labels the actual graph path and replaces community labels', () => {
  const configuration: AiLabelingConfiguration = {
    profile: profile('openai'),
    baseUrl: 'https://api.openai.com/v1',
    model: 'gpt-4.1-mini',
    maxConcurrency: 5,
    batchSize: 77,
    timeoutSeconds: 600,
  };
  const graphPath = '/workspace/custom-output/custom-graph.json';
  const args = labelingArguments(graphPath, configuration);
  assert.deepEqual(args, [
    'label', graphPath,
    '--backend', 'openai',
    '--model', 'gpt-4.1-mini',
    '--max-concurrency', '5',
    '--batch-size', '77',
    '--timeout-seconds', '600',
  ]);
  assert.equal(args.includes('--missing-only'), false);

  const ollama = labelingArguments(graphPath, { ...configuration, profile: profile('ollama'), maxConcurrency: 9 });
  assert.equal(ollama[ollama.indexOf('--max-concurrency') + 1], '1');
  const lmStudio = labelingArguments(graphPath, { ...configuration, profile: profile('lm-studio') });
  assert.equal(lmStudio[lmStudio.indexOf('--backend') + 1], 'lm-studio');
});

test('discovers LM Studio and Ollama models from their native endpoints', () => {
  assert.equal(modelDiscoveryUrl(profile('lm-studio'), 'http://127.0.0.1:1234/v1'), 'http://127.0.0.1:1234/v1/models');
  assert.equal(modelDiscoveryUrl(profile('ollama'), 'http://127.0.0.1:11434/v1'), 'http://127.0.0.1:11434/api/tags');
  assert.deepEqual(
    parseDiscoveredModels(profile('lm-studio'), { data: [{ id: 'qwen' }, { id: 'llama' }, { id: 'qwen' }] }),
    ['llama', 'qwen'],
  );
  assert.deepEqual(
    parseDiscoveredModels(profile('ollama'), { models: [{ name: 'qwen:latest' }, { model: 'llama:latest' }] }),
    ['llama:latest', 'qwen:latest'],
  );
});

test('trusted executable candidates exclude configured, PATH, and installed-extension escape paths', () => {
  const installed = trustedExecutableCandidates('/Users/example/.vscode/extensions/cgaspard.graphoxide-vscode-0.2.0', 'darwin');
  assert.deepEqual(installed, ['/Users/example/.vscode/extensions/cgaspard.graphoxide-vscode-0.2.0/bin/graphoxide']);

  const sourceExtension = path.resolve(process.cwd());
  const sourceCandidates = trustedExecutableCandidates(sourceExtension, process.platform);
  assert.ok(sourceCandidates.every(path.isAbsolute));
  assert.equal(sourceCandidates[0], path.join(sourceExtension, 'bin', process.platform === 'win32' ? 'graphoxide.exe' : 'graphoxide'));
  assert.equal(sourceCandidates.some((candidate) => candidate.includes('/usr/local/bin')), false);
  assert.equal(sourceCandidates.some((candidate) => candidate.includes('workspace-controlled')), false);
  assert.equal(sourceCandidates.length, 3);
});
