import { readdir, readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

export const PACKAGES = [
  ['graphoxide-core', 'core_integration'],
  ['graphoxide-extract', 'extract_integration'],
  ['graphoxide-graph', 'graph_integration'],
  ['graphoxide-export', 'export_integration'],
  ['graphoxide-query', 'query_integration'],
  ['graphoxide-skillgen', 'skillgen_integration'],
  ['graphoxide-cli', 'knowledgebase_cli'],
].map(([crate, aggregator]) => ({ crate, aggregator, testName: aggregator }));

function sorted(values) {
  return [...values].sort();
}

function same(values, expected) {
  return values.length === expected.length && values.every((value, index) => value === expected[index]);
}

function testTargets(cargo) {
  return cargo.split(/^\[\[test\]\]\s*$/m).slice(1);
}

export async function checkConsolidation(root, packages = PACKAGES) {
  const errors = [];
  for (const { crate, aggregator, testName } of packages) {
    const crateDir = join(root, 'crates', crate);
    const testsDir = join(crateDir, 'tests');
    const cargo = await readFile(join(crateDir, 'Cargo.toml'), 'utf8');
    if (!/^autotests\s*=\s*false\s*$/m.test(cargo)) errors.push(`${crate}: missing autotests = false`);

    const targets = testTargets(cargo);
    if (targets.length !== 1) {
      errors.push(`${crate}: expected exactly one [[test]] target, found ${targets.length}`);
    } else {
      const target = targets[0];
      if (!new RegExp(`^name\\s*=\\s*"${testName}"\\s*$`, 'm').test(target)) errors.push(`${crate}: explicit test name must be ${testName}`);
      if (!new RegExp(`^path\\s*=\\s*"tests/${aggregator}\\.rs"\\s*$`, 'm').test(target)) errors.push(`${crate}: explicit test path must be tests/${aggregator}.rs`);
    }

    const sourceRoots = sorted((await readdir(testsDir, { withFileTypes: true }))
      .filter((entry) => entry.isFile() && entry.name.endsWith('.rs') && entry.name !== `${aggregator}.rs`)
      .map((entry) => entry.name));
    const aggregatorSource = await readFile(join(testsDir, `${aggregator}.rs`), 'utf8');
    const aggregatorPaths = [];
    for (const match of aggregatorSource.matchAll(/^\s*#\[path\s*=\s*"([^"]+)"\]\s*\r?\n\s*mod\s+\w+\s*;/gm)) aggregatorPaths.push(match[1]);
    const actualPaths = sorted(aggregatorPaths);
    if (!same(actualPaths, sourceRoots)) errors.push(`${crate}: aggregator paths differ; expected ${sourceRoots.join(', ')}, found ${actualPaths.join(', ')}`);
  }
  if (errors.length) throw new Error(errors.join('\n'));
  return errors;
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const root = dirname(dirname(fileURLToPath(import.meta.url)));
  await checkConsolidation(root);
  console.log('test consolidation verification passed');
}
