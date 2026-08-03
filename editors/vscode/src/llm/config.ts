import * as fs from 'node:fs';
import { isIP } from 'node:net';
import * as path from 'node:path';

export type AiProviderId = 'openai' | 'openai-compatible' | 'lm-studio' | 'ollama' | 'anthropic';

export interface AiProviderProfile {
  readonly id: AiProviderId;
  readonly label: string;
  readonly description: string;
  readonly backend: 'openai' | 'lm-studio' | 'ollama' | 'claude';
  readonly defaultBaseUrl: string;
  readonly defaultModel: string;
  readonly apiKeyEnvironment: 'OPENAI_API_KEY' | 'OLLAMA_API_KEY' | 'ANTHROPIC_API_KEY';
  readonly editableEndpoint: boolean;
  readonly localPreset: boolean;
}

export interface AiLabelingConfiguration {
  readonly profile: AiProviderProfile;
  readonly baseUrl: string;
  readonly model: string;
  readonly maxConcurrency: number;
  readonly batchSize: number;
  readonly timeoutSeconds: number;
}

export type EnvironmentOverlay = Readonly<Record<string, string | undefined>>;

export const AI_PROVIDER_PROFILES: readonly AiProviderProfile[] = [
  {
    id: 'openai',
    label: 'OpenAI',
    description: 'OpenAI cloud · API key required',
    backend: 'openai',
    defaultBaseUrl: 'https://api.openai.com/v1',
    defaultModel: 'gpt-4.1-mini',
    apiKeyEnvironment: 'OPENAI_API_KEY',
    editableEndpoint: false,
    localPreset: false,
  },
  {
    id: 'lm-studio',
    label: 'LM Studio',
    description: 'Local OpenAI-compatible server · key optional',
    backend: 'lm-studio',
    defaultBaseUrl: 'http://127.0.0.1:1234/v1',
    defaultModel: '',
    apiKeyEnvironment: 'OPENAI_API_KEY',
    editableEndpoint: true,
    localPreset: true,
  },
  {
    id: 'ollama',
    label: 'Ollama',
    description: 'Local Ollama server · key optional',
    backend: 'ollama',
    defaultBaseUrl: 'http://127.0.0.1:11434/v1',
    defaultModel: 'qwen2.5-coder:7b',
    apiKeyEnvironment: 'OLLAMA_API_KEY',
    editableEndpoint: true,
    localPreset: true,
  },
  {
    id: 'openai-compatible',
    label: 'OpenAI-compatible endpoint',
    description: 'Custom local, hosted, or on-premises endpoint',
    backend: 'openai',
    defaultBaseUrl: '',
    defaultModel: '',
    apiKeyEnvironment: 'OPENAI_API_KEY',
    editableEndpoint: true,
    localPreset: false,
  },
  {
    id: 'anthropic',
    label: 'Anthropic',
    description: 'Anthropic Messages API · API key required',
    backend: 'claude',
    defaultBaseUrl: 'https://api.anthropic.com/v1',
    defaultModel: 'claude-sonnet-4-6',
    apiKeyEnvironment: 'ANTHROPIC_API_KEY',
    editableEndpoint: false,
    localPreset: false,
  },
] as const;

const CREDENTIAL_ENVIRONMENT = [
  'OPENAI_API_KEY',
  'OLLAMA_API_KEY',
  'ANTHROPIC_API_KEY',
  'GEMINI_API_KEY',
  'GOOGLE_API_KEY',
  'MOONSHOT_API_KEY',
  'DEEPSEEK_API_KEY',
] as const;

const PROVIDER_ENVIRONMENT = [
  'GRAPHOXIDE_LLM_BASE_URL',
  'GRAPHOXIDE_LLM_PROVIDER',
  'GRAPHIFY_LLM_PROVIDER',
  'OPENAI_BASE_URL',
  'ANTHROPIC_BASE_URL',
  'OLLAMA_BASE_URL',
  'OLLAMA_HOST',
  'GRAPHOXIDE_LLM_TIMEOUT_SECONDS',
  'GRAPHIFY_API_TIMEOUT',
] as const;

export function aiProviderById(value: unknown): AiProviderProfile | undefined {
  return typeof value === 'string'
    ? AI_PROVIDER_PROFILES.find((profile) => profile.id === value)
    : undefined;
}

export function normalizeProviderBaseUrl(profile: AiProviderProfile, configured: string): string {
  const raw = profile.editableEndpoint ? configured.trim() || profile.defaultBaseUrl : profile.defaultBaseUrl;
  if (!raw) throw new Error(`${profile.label} requires a base URL.`);
  let parsed: URL;
  try {
    parsed = new URL(raw);
  } catch {
    throw new Error(`${profile.label} base URL is not a valid URL.`);
  }
  if (parsed.protocol !== 'http:' && parsed.protocol !== 'https:') {
    throw new Error(`${profile.label} base URL must use http or https.`);
  }
  if (parsed.username || parsed.password) {
    throw new Error(`${profile.label} credentials must use VS Code Secret Storage, not the base URL.`);
  }
  if (parsed.search || parsed.hash) {
    throw new Error(`${profile.label} base URL may not contain a query string or fragment.`);
  }
  if (parsed.protocol === 'http:' && !isLoopbackUrl(parsed.toString())) {
    throw new Error(`${profile.label} must use https unless the endpoint is on this computer.`);
  }
  let pathname = parsed.pathname.replace(/\/+$/u, '');
  if (profile.localPreset && !/\/v\d+$/u.test(pathname)) pathname += '/v1';
  parsed.pathname = pathname || '/';
  return parsed.toString().replace(/\/$/u, '');
}

export function isLoopbackUrl(value: string): boolean {
  let hostname: string;
  try {
    hostname = new URL(value).hostname.replace(/^\[|\]$/gu, '').toLocaleLowerCase();
  } catch {
    return false;
  }
  if (hostname === 'localhost' || hostname === '::1') return true;
  if (isIP(hostname) !== 4) return false;
  return hostname.split('.')[0] === '127';
}

export function apiKeyRequired(profile: AiProviderProfile, baseUrl: string): boolean {
  return profile.id === 'openai' || profile.id === 'anthropic' || !isLoopbackUrl(baseUrl);
}

export function aiSecretKey(profile: AiProviderProfile): string {
  return `graphoxide.aiLabeling.apiKey.${profile.id}`;
}

export function encodeStoredCredential(baseUrl: string, apiKey: string): string {
  return JSON.stringify({ version: 1, baseUrl, apiKey });
}

export function credentialForEndpoint(value: string | undefined, expectedBaseUrl: string): string | undefined {
  if (!value) return undefined;
  try {
    const parsed: unknown = JSON.parse(value);
    if (typeof parsed !== 'object' || parsed === null || Array.isArray(parsed)) return undefined;
    const credential = parsed as Record<string, unknown>;
    return credential.version === 1
      && credential.baseUrl === expectedBaseUrl
      && typeof credential.apiKey === 'string'
      && credential.apiKey.trim()
      ? credential.apiKey
      : undefined;
  } catch {
    // Unbound legacy values must never be reused for a newly selected endpoint.
    return undefined;
  }
}

export function labelingEnvironment(
  profile: AiProviderProfile,
  baseUrl: string,
  apiKey?: string,
): EnvironmentOverlay {
  const environment: Record<string, string | undefined> = {};
  for (const name of [...CREDENTIAL_ENVIRONMENT, ...PROVIDER_ENVIRONMENT]) environment[name] = undefined;
  environment.GRAPHOXIDE_LLM_BASE_URL = baseUrl;
  environment.GRAPHOXIDE_LLM_PROVIDER = profile.backend;
  const key = apiKey?.trim();
  if (key) environment[profile.apiKeyEnvironment] = key;
  return environment;
}

export function labelingArguments(
  graphPath: string,
  configuration: AiLabelingConfiguration,
): readonly string[] {
  const concurrency = configuration.profile.id === 'ollama' ? 1 : configuration.maxConcurrency;
  return [
    'label',
    graphPath,
    '--backend',
    configuration.profile.backend,
    '--model',
    configuration.model,
    '--max-concurrency',
    String(Math.max(1, concurrency)),
    '--batch-size',
    String(Math.max(1, configuration.batchSize)),
    '--timeout-seconds',
    String(Math.max(0.001, configuration.timeoutSeconds)),
  ];
}

export function modelDiscoveryUrl(profile: AiProviderProfile, baseUrl: string): string | undefined {
  if (!isLoopbackUrl(baseUrl)) return undefined;
  if (profile.id === 'ollama') {
    const parsed = new URL(baseUrl);
    parsed.pathname = parsed.pathname.replace(/\/v\d+\/?$/u, '');
    return new URL('api/tags', `${parsed.toString().replace(/\/$/u, '')}/`).toString();
  }
  if (profile.id === 'lm-studio' || profile.id === 'openai-compatible') {
    return new URL('models', `${baseUrl.replace(/\/$/u, '')}/`).toString();
  }
  return undefined;
}

export function parseDiscoveredModels(profile: AiProviderProfile, value: unknown): string[] {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return [];
  const root = value as Record<string, unknown>;
  const rows = profile.id === 'ollama' ? root.models : root.data;
  if (!Array.isArray(rows)) return [];
  const models = rows.flatMap((row) => {
    if (typeof row !== 'object' || row === null || Array.isArray(row)) return [];
    const object = row as Record<string, unknown>;
    const id = profile.id === 'ollama' ? object.name ?? object.model : object.id;
    return typeof id === 'string' && id.trim() ? [id.trim()] : [];
  });
  return [...new Set(models)].sort((left, right) => left.localeCompare(right));
}

export function trustedExecutableCandidates(extensionPath: string, platform: NodeJS.Platform): readonly string[] {
  const executable = platform === 'win32' ? 'graphoxide.exe' : 'graphoxide';
  const packaged = path.resolve(extensionPath, 'bin', executable);
  const repository = path.resolve(extensionPath, '..', '..');
  const sourceCheckout = path.basename(extensionPath) === 'vscode'
    && path.basename(path.dirname(extensionPath)) === 'editors'
    && fs.existsSync(path.join(repository, 'Cargo.toml'))
    && fs.existsSync(path.join(repository, 'crates', 'graphoxide-cli', 'Cargo.toml'));
  return sourceCheckout
    ? [
        packaged,
        path.join(repository, 'target', 'release', executable),
        path.join(repository, 'target', 'debug', executable),
      ]
    : [packaged];
}

export function environmentContainsCredential(environment: EnvironmentOverlay | undefined): boolean {
  return Object.entries(environment ?? {}).some(([name, value]) => name.endsWith('_API_KEY') && Boolean(value?.trim()));
}

export function shouldUseTrustedExecutable(
  explicitlyRequested: boolean | undefined,
  environment: EnvironmentOverlay | undefined,
): boolean {
  return explicitlyRequested === true || environmentContainsCredential(environment);
}

export function overlayEnvironment(
  base: NodeJS.ProcessEnv,
  overlay: EnvironmentOverlay | undefined,
): NodeJS.ProcessEnv {
  const result = { ...base };
  for (const [name, value] of Object.entries(overlay ?? {})) {
    const folded = name.toLocaleUpperCase('en-US');
    for (const inherited of Object.keys(result)) {
      if (inherited.toLocaleUpperCase('en-US') === folded) delete result[inherited];
    }
    if (value !== undefined) result[name] = value;
  }
  return result;
}
