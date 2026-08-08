import { readFile, access } from 'node:fs/promises';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const repositoryRoot = resolve(root, '..');
const html = await readFile(join(root, 'index.html'), 'utf8');
const errors = [];

const ids = new Set([...html.matchAll(/\bid="([^"]+)"/g)].map((match) => match[1]));
for (const match of html.matchAll(/\bhref="#([^"]+)"/g)) {
  if (!ids.has(match[1])) errors.push(`Missing in-page target: #${match[1]}`);
}

const localAssets = [...html.matchAll(/\b(?:src|href)="((?:assets\/|styles\.css|app\.js)[^"]*)"/g)]
  .map((match) => match[1]);
for (const asset of localAssets) {
  try {
    await access(join(root, asset));
  } catch {
    errors.push(`Missing local asset: ${asset}`);
  }
}

for (const tag of html.matchAll(/<(?:script|link|img)\b[^>]*(?:src|href)="(https?:\/\/[^"\s]+)"[^>]*>/g)) {
  errors.push(`Remote page dependency: ${tag[1]}`);
}

for (const img of html.matchAll(/<img\b[^>]*>/g)) {
  if (!/\balt="[^"]*"/.test(img[0])) errors.push(`Image without alt text: ${img[0]}`);
}

if (!html.includes('the original Graphify project')) errors.push('Missing top Graphify attribution');
if (!html.includes('not affiliated with Graphify Labs')) errors.push('Missing independence statement');
if (!html.includes('Licensed under Apache-2.0; portions originally MIT.')) errors.push('Missing license attribution');
const publishedProse = [
  ['website/index.html', html],
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
if (!html.includes('prefers-reduced-motion')) {
  const css = await readFile(join(root, 'styles.css'), 'utf8');
  if (!css.includes('prefers-reduced-motion')) errors.push('Missing reduced-motion styles');
}

if (errors.length) {
  console.error(errors.map((error) => `- ${error}`).join('\n'));
  process.exitCode = 1;
} else {
  console.log(`Website validation passed (${ids.size} anchors, ${localAssets.length} local assets).`);
}
