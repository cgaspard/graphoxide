import * as path from 'node:path';

export const MAX_WIKI_INDEX_BYTES = 16 * 1024 * 1024;
export const MAX_WIKI_PROFILE_BYTES = 256 * 1024;
// Add output includes the source index, authored-page metadata, and directory outcomes.
export const MAX_WIKI_BUILD_RESULT_BYTES = 4 * MAX_WIKI_INDEX_BYTES;

export interface WikiBuildSummary {
  readonly generatedPages: number;
  readonly sourceErrors: number;
  readonly skippedInputs: number;
}

export type WikiSourceStatus = 'provisional' | 'ai-reviewed' | 'human-confirmed' | 'stale-error';

export interface WikiSource {
  readonly id: string;
  readonly label: string;
  readonly status: WikiSourceStatus;
  readonly remote: boolean;
}

export interface WikiConsent {
  readonly allowModelEgress: boolean;
  readonly allowNetwork: boolean;
}

export interface WikiAuthoringProfile {
  readonly providerProfile: string;
  readonly authorModel: string;
  readonly reviewerModel: string;
}

function record(value: unknown): Record<string, unknown> {
  if (!value || typeof value !== 'object' || Array.isArray(value)) throw new Error('Invalid Wiki metadata.');
  return value as Record<string, unknown>;
}

function requiredString(value: unknown): string {
  if (typeof value !== 'string' || !value.trim() || hasControlCharacters(value)) {
    throw new Error('Invalid Wiki metadata text.');
  }
  return value;
}

function hasControlCharacters(value: string): boolean {
  for (const character of value) {
    const code = character.charCodeAt(0);
    if (code < 32 || code === 127) return true;
  }
  return false;
}

/** Only aggregate outcomes enter the completion UI; source locators stay in Output. */
export function parseWikiBuildSummary(json: string): WikiBuildSummary {
  if (Buffer.byteLength(json) > MAX_WIKI_BUILD_RESULT_BYTES) throw new Error('Wiki build summary exceeds its size limit. See Graphoxide Output for the command result.');
  const result = record(JSON.parse(json));
  if (!Array.isArray(result.authored) || !Array.isArray(result.outcomes)) throw new Error('Wiki command completed without a readable build summary. See Graphoxide Output for details.');
  for (const authored of result.authored) {
    const page = record(authored);
    requiredString(page.source_id);
    requiredString(page.page_id);
  }
  let sourceErrors = 0;
  let skippedInputs = 0;
  for (const outcome of result.outcomes) {
    const kind = record(outcome).outcome;
    if (kind === 'error') sourceErrors += 1;
    else if (kind === 'skipped') skippedInputs += 1;
    else if (kind !== 'added') throw new Error('Wiki command completed with an unknown source outcome. See Graphoxide Output for details.');
  }
  return { generatedPages: result.authored.length, sourceErrors, skippedInputs };
}

export function wikiBuildCompletionMessage(summary: WikiBuildSummary): string {
  const pages = summary.generatedPages === 0 ? 'No Wiki pages generated.' : `Generated ${summary.generatedPages} Wiki ${summary.generatedPages === 1 ? 'page' : 'pages'}.`;
  const details = [
    ...(summary.sourceErrors ? [`${summary.sourceErrors} ${summary.sourceErrors === 1 ? 'input' : 'inputs'} could not be read`] : []),
    ...(summary.skippedInputs ? [`${summary.skippedInputs} ${summary.skippedInputs === 1 ? 'input' : 'inputs'} skipped`] : []),
  ];
  return details.length ? `${pages} ${details.join('; ')}. See Graphoxide Output for details.` : pages;
}

/** Paths in project metadata never grant access outside that project. */
export function wikiProjectPath(root: string, relative: string): string {
  if (path.isAbsolute(relative) || relative.includes('\\') || relative.split('/').some((part) => !part || part === '.' || part === '..')) {
    throw new Error('Wiki profile paths must be relative paths inside the workspace.');
  }
  const resolved = path.resolve(root, relative);
  if (!path.relative(root, resolved) || path.relative(root, resolved).startsWith(`..${path.sep}`)) {
    throw new Error('Wiki profile paths must be inside the workspace.');
  }
  return resolved;
}

export function validateWikiHttpsUrl(value: string): string | undefined {
  try {
    const url = new URL(value);
    if (url.protocol !== 'https:' || !url.hostname || url.username || url.password || url.search || url.hash || url.href !== value) {
      return 'Use a complete HTTPS URL without credentials, query parameters, or a fragment.';
    }
  } catch {
    return 'Enter a complete HTTPS URL.';
  }
  return undefined;
}

export function parseWikiSources(json: string): readonly WikiSource[] {
  if (Buffer.byteLength(json) > MAX_WIKI_INDEX_BYTES) throw new Error('Wiki source index exceeds its size limit.');
  const index = record(JSON.parse(json));
  if (index.schema !== 'graphoxide.source-index' || !Array.isArray(index.sources)) throw new Error('Unsupported Wiki source index.');
  const seen = new Set<string>();
  return index.sources.map((value: unknown) => {
    const source = record(value);
    const id = requiredString(source.source_id);
    if (!/^src:[0-9a-f]{64}$/u.test(id) || seen.has(id)) throw new Error('Invalid or duplicate Wiki source ID.');
    seen.add(id);
    if (!['provisional', 'ai-reviewed', 'human-confirmed', 'stale-error'].includes(String(source.status))) {
      throw new Error('Invalid Wiki source status.');
    }
    const location = record(source.location);
    const remote = location.kind === 'https';
    if (!remote && location.kind !== 'git' && location.kind !== 'bound-path') throw new Error('Unsupported Wiki source location.');
    const label = requiredString(remote ? location.url : location.path);
    if (remote && validateWikiHttpsUrl(label)) throw new Error('Invalid Wiki source URL.');
    return { id, label, status: source.status as WikiSourceStatus, remote };
  }).sort((left, right) => left.id.localeCompare(right.id));
}

export function parseWikiAuthoringProfile(json: string, root: string): WikiAuthoringProfile {
  if (Buffer.byteLength(json) > MAX_WIKI_PROFILE_BYTES) throw new Error('Wiki authoring profile exceeds its size limit.');
  const profile = record(JSON.parse(json));
  const providerProfile = requiredString(profile.provider_profile);
  wikiProjectPath(root, providerProfile);
  requiredString(profile.source_egress_consent);
  return {
    providerProfile,
    authorModel: requiredString(profile.author_model),
    reviewerModel: requiredString(profile.reviewer_model),
  };
}

export function wikiModelDestination(json: string, modelId: string): { readonly endpoint: string; readonly model: string } {
  if (Buffer.byteLength(json) > MAX_WIKI_PROFILE_BYTES) throw new Error('Wiki provider profile exceeds its size limit.');
  const provider = record(JSON.parse(json));
  const endpoint = requiredString(provider.endpoint);
  const url = new URL(endpoint);
  if (!['http:', 'https:'].includes(url.protocol) || url.username || url.password || url.search || url.hash) {
    throw new Error('Wiki provider endpoint must be an HTTP or HTTPS URL without embedded credentials.');
  }
  if (!Array.isArray(provider.models)) throw new Error('Wiki provider profile requires configured models.');
  const model = provider.models.map(record).find((candidate) => candidate.id === modelId);
  if (!model || model.enabled === false) throw new Error(`Wiki model “${modelId}” is unavailable in the provider profile.`);
  return { endpoint, model: requiredString(model.api_model) };
}

/** The CLI owns YAML parsing; review that exact file instead of guessing its effective values. */
export function wikiModelDisclosure(body: string, providerProfile: string, modelId: string): { readonly reviewFile: boolean; readonly description: string } {
  if (Buffer.byteLength(body) > MAX_WIKI_PROFILE_BYTES) throw new Error('Wiki provider profile exceeds its size limit.');
  if (['.yaml', '.yml'].includes(path.extname(providerProfile))) {
    return {
      reviewFile: true,
      description: `model alias “${modelId}” in ${providerProfile}. Review the endpoint and API model in the opened provider profile before allowing model use`,
    };
  }
  const destination = wikiModelDestination(body, modelId);
  return { reviewFile: false, description: `model “${destination.model}” at ${destination.endpoint} using ${providerProfile}` };
}

export function wikiBuildArguments(inputs: readonly string[], consent: WikiConsent): string[] {
  if (!inputs.length) throw new Error('Choose at least one Wiki source.');
  const remote = inputs.some((input) => input.startsWith('https://'));
  for (const input of inputs) {
    if (input.startsWith('https://')) {
      const error = validateWikiHttpsUrl(input);
      if (error) throw new Error(error);
    } else if (!path.isAbsolute(input) || hasControlCharacters(input)) {
      throw new Error('Wiki sources must be absolute local paths or HTTPS URLs.');
    }
  }
  return ['wiki', 'source', 'add', ...wikiConsentArguments(consent, remote), ...inputs];
}

export function wikiConsentArguments(consent: WikiConsent, remote: boolean): string[] {
  if (!consent.allowModelEgress) throw new Error('Wiki authoring requires explicit permission to send source text to the configured model.');
  if (remote && !consent.allowNetwork) throw new Error('HTTPS Wiki sources require explicit network permission.');
  return ['--allow-model-egress', ...(remote ? ['--allow-network'] : [])];
}

export function wikiPageRelativePath(source: WikiSource): string {
  if (!/^src:[0-9a-f]{64}$/u.test(source.id)) throw new Error('Invalid Wiki source ID.');
  return `content/provisional/source-${source.id.slice(4)}.md`;
}
