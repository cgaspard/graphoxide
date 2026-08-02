import { readFile, access } from 'node:fs/promises';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
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
