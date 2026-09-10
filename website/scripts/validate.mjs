import { readFile, access } from 'node:fs/promises';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const repositoryRoot = resolve(root, '..');
const pageNames = ['index.html', 'knowledgebase.html'];
const pages = await Promise.all(pageNames.map(async (name) => [name, await readFile(join(root, name), 'utf8')]));
const errors = [];

for (const [name, html] of pages) {
  const ids = new Set([...html.matchAll(/\bid="([^"]+)"/g)].map((match) => match[1]));
  for (const match of html.matchAll(/\bhref="#([^"]+)"/g)) {
    if (!ids.has(match[1])) errors.push(`${name}: missing in-page target #${match[1]}`);
  }
  const localAssets = [...html.matchAll(/\b(?:src|href)="((?:assets\/|styles\.css|app\.js)[^"]*)"/g)]
    .map((match) => match[1]);
  for (const asset of localAssets) {
    try {
      await access(join(root, asset));
    } catch {
      errors.push(`${name}: missing local asset ${asset}`);
    }
  }
  const localPages = [...html.matchAll(/\bhref="([^"#?]+\.html)"/g)].map((match) => match[1]);
  for (const page of localPages) {
    try {
      await access(join(root, page));
    } catch {
      errors.push(`${name}: missing local page ${page}`);
    }
  }
  for (const tag of html.matchAll(/<(?:script|link|img)\b[^>]*(?:src|href)="(https?:\/\/[^"\s]+)"[^>]*>/g)) {
    errors.push(`${name}: remote page dependency ${tag[1]}`);
  }
  for (const img of html.matchAll(/<img\b[^>]*>/g)) {
    if (!/\balt="[^"]*"/.test(img[0])) errors.push(`${name}: image without alt text ${img[0]}`);
  }
}

const indexHtml = pages[0][1];
const knowledgebaseHtml = pages[1][1];
const knowledgebaseDoc = await readFile(join(repositoryRoot, 'docs/knowledgebase.md'), 'utf8');
if (!indexHtml.includes('the original Graphify project')) errors.push('Missing top Graphify attribution');
if (!indexHtml.includes('not affiliated with Graphify Labs')) errors.push('Missing independence statement');
if (!indexHtml.includes('Licensed under Apache-2.0; portions originally MIT.')) errors.push('Missing license attribution');
for (const phrase of [
  'graphoxide wiki init',
  'graphoxide wiki source add',
  'graphoxide wiki source refresh',
  'graphoxide wiki source review',
  'graphoxide wiki source status',
  'graphoxide wiki source retire',
  'graphoxide wiki live',
  '--allow-wiki-network',
  '--allow-wiki-model-egress',
]) {
  if (!knowledgebaseHtml.includes(phrase)) errors.push(`Missing direct knowledgebase workflow: ${phrase}`);
}
for (const phrase of [
  'graphoxide wiki init',
  'graphoxide wiki source add',
  'graphoxide wiki source refresh',
  'graphoxide wiki source review',
  'graphoxide wiki source status',
  'graphoxide wiki source retire',
  'graphoxide wiki live',
  'pointer-only',
  'transiently',
]) {
  if (!knowledgebaseDoc.includes(phrase)) errors.push(`Missing direct knowledgebase documentation: ${phrase}`);
}
for (const [name, prose] of [
  ['website/knowledgebase.html', knowledgebaseHtml],
  ['docs/knowledgebase.md', knowledgebaseDoc],
  ['README.md', await readFile(join(repositoryRoot, 'README.md'), 'utf8')],
]) {
  for (const phrase of ['Knowledgebase v2', 'technical-v2', 'source-store', 'wiki source sync', 'wiki plan', 'wiki draft']) {
    if (prose.includes(phrase)) errors.push(`${name}: retired knowledgebase phrase ${phrase}`);
  }
}
const publishedProse = [
  ...pages.map(([name, html]) => [`website/${name}`, html]),
  ['README.md', await readFile(join(repositoryRoot, 'README.md'), 'utf8')],
  ['HANDOFF.md', await readFile(join(repositoryRoot, 'HANDOFF.md'), 'utf8')],
  ['BENCHMARKS.md', await readFile(join(repositoryRoot, 'BENCHMARKS.md'), 'utf8')],
];
const performanceTerm = String.raw`(?:faster|slower|speedup|throughput|latency|performance|query\s+time|build\s+time|startup|first\s+instruction)`;
const numericRatio = String.raw`(?:\d+(?:\.\d+)?\s*(?:%|[x×])|twice)`;
const numericDuration = String.raw`(?:~\s*)?\d+(?:\.\d+)?\s*(?:µs|us|ms|milliseconds?|s|seconds?)`;
const unsupportedPerformanceClaims = [
  new RegExp(`${numericRatio}[^\\n]{0,100}\\b${performanceTerm}\\b`, 'i'),
  new RegExp(`\\b${performanceTerm}\\b[^\\n]{0,100}${numericRatio}`, 'i'),
  new RegExp(`${numericDuration}[^\\n]{0,100}\\b${performanceTerm}\\b`, 'i'),
  new RegExp(`\\b${performanceTerm}\\b[^\\n]{0,100}${numericDuration}`, 'i'),
  /\b\d+\s*[-–]\s*\d+\s*x\s+faster\b/i,
  /\btwice\s+as\s+(?:fast|slow)\b/i,
  /measured\s+(?:differential\s+and\s+)?performance\s+results/i,
  /measured\s+results\s+and\s+methodology/i,
];
const claimRegressionFixtures = [
  '42% faster indexing',
  '2× throughput',
  'latency improved by 2×',
  '3.72× faster full extraction',
  '6.22× lower cold-query latency',
  'A binary is ~5 ms to first instruction',
  'twice as fast for queries',
];
const methodologyRegressionFixtures = [
  'The admission ceiling is 16× the source count.',
  '<animateMotion dur="2.8s" repeatCount="indefinite" />',
  'Debounce 3 s before rebuilding.',
  'Retry-After must be no greater than 30 seconds.',
];
for (const fixture of claimRegressionFixtures) {
  if (!unsupportedPerformanceClaims.some((pattern) => pattern.test(fixture))) {
    errors.push(`Performance-claim regression fixture was not rejected: ${fixture}`);
  }
}
for (const fixture of methodologyRegressionFixtures) {
  if (unsupportedPerformanceClaims.some((pattern) => pattern.test(fixture))) {
    errors.push(`Methodology regression fixture was incorrectly rejected: ${fixture}`);
  }
}
for (const [name, prose] of publishedProse) {
  for (const pattern of unsupportedPerformanceClaims) {
    if (pattern.test(prose)) errors.push(`Unsupported performance claim in ${name}: ${pattern}`);
  }
}
if (!indexHtml.includes('prefers-reduced-motion')) {
  const css = await readFile(join(root, 'styles.css'), 'utf8');
  if (!css.includes('prefers-reduced-motion')) errors.push('Missing reduced-motion styles');
}

if (errors.length) {
  console.error(errors.map((error) => `- ${error}`).join('\n'));
  process.exitCode = 1;
} else {
  console.log(`Website validation passed (${pages.length} pages).`);
}
