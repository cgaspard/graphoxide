import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtemp, readFile, readdir, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import test from 'node:test';

interface ParityCase {
  nodeid: string;
  source: string;
}

interface ParityManifest {
  cases: ParityCase[];
}

test('stages all 133 agent artifacts for distribution', async () => {
  const repositoryRoot = path.resolve(process.cwd(), '..', '..');
  const temporaryRoot = await mkdtemp(path.join(tmpdir(), 'graphoxide-agent-assets-'));
  const output = path.join(temporaryRoot, 'agent-assets');
  try {
    const result = spawnSync(
      process.execPath,
      [path.join(repositoryRoot, 'scripts', 'agent-artifacts.mjs'), '--out', output],
      { encoding: 'utf8' },
    );
    assert.equal(result.status, 0, result.stderr || result.stdout);

    const manifest = JSON.parse(
      await readFile(path.join(repositoryRoot, 'parity', 'manifest.json'), 'utf8'),
    ) as ParityManifest;
    const expected = manifest.cases
      .filter(({ source }) => source === 'tests/test_wheel_packaging.py')
      .map(({ nodeid }) => {
        const match = nodeid.match(/\[([^\n]+)\]$/u);
        assert.ok(match, `missing artifact parameter in ${nodeid}`);
        return match[1];
      })
      .sort();
    const actual = await filesBelow(output);

    assert.equal(expected.length, 133);
    assert.deepEqual(actual, expected);
    let canonicalComparisons = 0;
    for (const relativePath of actual) {
      const content = await readFile(path.join(output, ...relativePath.split('/')), 'utf8');
      assert.ok(content.length >= 120, `${relativePath} is unexpectedly short`);
      assert.match(content, /graphoxide/iu);
      assert.doesNotMatch(content, /graphifyy|graphify-out|import graphify/iu);

      const reference = relativePath.match(/^skills\/[^/]+\/references\/(.+)$/u);
      const canonicalRelativePath = relativePath.startsWith('skill-') || relativePath === 'skill.md'
        ? relativePath
        : reference?.[1]
          ? path.join('references', reference[1])
          : undefined;
      if (canonicalRelativePath) {
        const canonical = await readFile(
          path.join(repositoryRoot, 'crates', 'graphoxide-cli', 'assets', canonicalRelativePath),
          'utf8',
        );
        assert.equal(content, canonical, `${relativePath} drifted from the CLI-embedded source`);
        canonicalComparisons += 1;
      }
    }
    assert.equal(canonicalComparisons, 127);

    const packageJson = JSON.parse(
      await readFile(path.join(process.cwd(), 'package.json'), 'utf8'),
    ) as { files?: string[] };
    assert.ok(packageJson.files?.includes('agent-assets/**'));
    assert.ok(packageJson.files?.includes('dist/webview/**'));
    assert.ok(packageJson.files?.includes('media/graph-visualizer.css'));

    const packageScript = await readFile(path.join(process.cwd(), 'scripts', 'package.mjs'), 'utf8');
    assert.match(packageScript, /stageAgentArtifacts\(agentAssetsDestination\)/u);
    assert.match(packageScript, /extension\/agent-assets/u);
    assert.match(packageScript, /extension\/dist\/webview\/graph-visualizer\.js/u);
    assert.match(packageScript, /extension\/media\/graph-visualizer\.css/u);
    assert.match(packageScript, /is missing \$\{required\} or it is empty/u);

    const releaseWorkflow = await readFile(
      path.join(repositoryRoot, '.github', 'workflows', 'release.yml'),
      'utf8',
    );
    assert.equal(releaseWorkflow.match(/agent-artifacts\.mjs --out/gu)?.length, 2);
  } finally {
    await rm(temporaryRoot, { recursive: true, force: true });
  }
});

test('agent extraction specs carry canonical production node IDs', async () => {
  const repositoryRoot = path.resolve(process.cwd(), '..', '..');
  const temporaryRoot = await mkdtemp(path.join(tmpdir(), 'graphoxide-id-specs-'));
  const output = path.join(temporaryRoot, 'agent-assets');
  try {
    const result = spawnSync(
      process.execPath,
      [path.join(repositoryRoot, 'scripts', 'agent-artifacts.mjs'), '--out', output],
      { encoding: 'utf8' },
    );
    assert.equal(result.status, 0, result.stderr || result.stdout);
    const specs = (await filesBelow(output)).filter((file) => file.endsWith('/extraction-spec.md'));
    assert.equal(specs.length, 14);
    const examples = [
      '`src/auth/session.py` + `ValidateToken` → `src_auth_session_validatetoken`',
      '`lib/utils/helpers.py` + `parse_url` → `lib_utils_helpers_parse_url`',
      '`tests/test_foo.py` + `_helper` → `tests_test_foo_helper`',
      '`docs/v1/api/README.md` + `getUser` → `docs_v1_api_readme_getuser`',
    ];
    for (const spec of specs) {
      const content = await readFile(path.join(output, ...spec.split('/')), 'utf8');
      for (const example of examples) assert.ok(content.includes(example), `${spec}: ${example}`);
      assert.match(content, /full repository-relative source path/iu);
      assert.match(content, /Do not use a filename-only or immediate-parent-only ID/iu);
    }
  } finally {
    await rm(temporaryRoot, { recursive: true, force: true });
  }
});

async function filesBelow(directory: string, prefix = ''): Promise<string[]> {
  const result: string[] = [];
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    const relative = prefix ? `${prefix}/${entry.name}` : entry.name;
    if (entry.isDirectory()) result.push(...await filesBelow(path.join(directory, entry.name), relative));
    else if (entry.isFile()) result.push(relative);
  }
  return result.sort();
}
