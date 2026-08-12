import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import path from 'node:path';
import test from 'node:test';
import { pathToFileURL } from 'node:url';
import { promisify } from 'node:util';

const modulePath = pathToFileURL(path.join(process.cwd(), 'scripts', 'browser-process.mjs')).href;
const execute = promisify(execFile);

test('browser process helper accepts timed-out output only after its completion marker', async () => {
  const accepted = await evaluateHelper(`
    resolveKilledBrowserProcess(
      { stdout: '<pre id="gx-result">complete</pre>', stderr: 'bounded warning' },
      (stdout) => stdout.includes('id="gx-result"'),
    )
  `) as { stdout: string; stderr: string };
  assert.equal(accepted.stdout, '<pre id="gx-result">complete</pre>');
  assert.equal(accepted.stderr, 'bounded warning');
  const emptyMarker = await evaluateHelper(`
    resolveKilledBrowserProcess(
      { stdout: '<pre id="gx-result"></pre>', stderr: '' },
      (stdout) => /<pre[^>]*id="gx-result"[^>]*>[\\s\\S]*\\S[\\s\\S]*<\\/pre>/u.test(stdout),
    )
  `) as { error: string };
  assert.equal(emptyMarker.error, 'Chrome timed out after 45000 ms.');
  const controlCenter = await evaluateHelper(`
    resolveKilledBrowserProcess(
      { stdout: '<output id="layout-result">{}</output>', stderr: '' },
      (stdout) => /<output[^>]*id="layout-result"[^>]*>[\\s\\S]*\\S[\\s\\S]*<\\/output>/u.test(stdout),
    )
  `) as { stdout: string };
  assert.equal(controlCenter.stdout, '<output id="layout-result">{}</output>');
  const buildSummary = await evaluateHelper(`
    resolveKilledBrowserProcess(
      { stdout: '<output id="build-summary-result">{}</output>', stderr: '' },
      (stdout) => /<output[^>]*id="build-summary-result"[^>]*>[\\s\\S]*\\S[\\s\\S]*<\\/output>/u.test(stdout),
    )
  `) as { stdout: string };
  assert.equal(buildSummary.stdout, '<output id="build-summary-result">{}</output>');
  const rejected = await evaluateHelper(`
    resolveKilledBrowserProcess(
      { stdout: '', stderr: '' },
      (stdout) => stdout.includes('id="gx-result"'),
    )
  `) as { error: string };
  assert.equal(rejected.error, 'Chrome timed out after 45000 ms.');
});

test('browser process helper bounds timeout diagnostics', async () => {
  const prefix = 'not-retained-';
  const suffix = 'x'.repeat(2_000);
  const rejected = await evaluateHelper(`resolveKilledBrowserProcess({ stderr: ${JSON.stringify(`${prefix}${suffix}`)} })`) as { error: string };
  assert.ok(!rejected.error.includes(prefix));
  assert.ok(rejected.error.endsWith(suffix));
  assert.ok(rejected.error.length < 2_100);
});

async function evaluateHelper(expression: string): Promise<unknown> {
  const program = `
    import { resolveKilledBrowserProcess } from ${JSON.stringify(modulePath)};
    try {
      process.stdout.write(JSON.stringify(${expression}));
    } catch (error) {
      process.stdout.write(JSON.stringify({ error: error instanceof Error ? error.message : 'Unknown error' }));
    }
  `;
  const { stdout } = await execute(process.execPath, ['--input-type=module', '--eval', program], {
    maxBuffer: 8 * 1024,
    timeout: 2_000,
  });
  return JSON.parse(stdout) as unknown;
}
