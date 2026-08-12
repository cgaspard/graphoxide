import { execFile } from 'node:child_process';
import { createHash } from 'node:crypto';
import { mkdir, mkdtemp, open, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
import { promisify } from 'node:util';

const execute = promisify(execFile);
const extensionRoot = process.cwd();
const chrome = await locateChrome();
const temporaryRoot = await mkdtemp(path.join(tmpdir(), 'graphoxide-control-center-browser-'));
const artifactRoot = process.env.GRAPHOXIDE_CONTROL_CENTER_ARTIFACTS
  ? path.resolve(process.env.GRAPHOXIDE_CONTROL_CENTER_ARTIFACTS)
  : await mkdtemp(path.join(tmpdir(), 'graphoxide-control-center-artifacts-'));

try {
  await mkdir(artifactRoot, { recursive: true });
  const source = await readFile(path.join(extensionRoot, 'src', 'control-center.ts'), 'utf8');
  const stylesheet = extractStylesheet(source);
  const harnessPath = path.join(temporaryRoot, 'control-center.html');
  await writeFile(harnessPath, harnessHtml(stylesheet), 'utf8');
  const harnessUrl = pathToFileURL(harnessPath);

  const cases = [
    { name: 'wide', width: 1440, height: 1800, screenshotScale: 1, expectedColumns: 2, query: '', flags: [] },
    { name: 'narrow', width: 500, height: 3000, screenshotScale: 1, expectedColumns: 1, query: '', flags: [] },
    // A 1,000-device-pixel editor at 200% zoom has a 500-CSS-pixel layout viewport.
    { name: 'high-zoom', width: 500, height: 2400, screenshotScale: 2, expectedColumns: 1, query: '', flags: [] },
    { name: 'forced-colors', width: 1440, height: 1800, screenshotScale: 1, expectedColumns: 2, query: '', flags: ['--force-high-contrast'] },
  ];
  const reports = [];
  const screenshotHashes = new Map();
  for (const scenario of cases) {
    const url = new URL(harnessUrl);
    url.search = scenario.query;
    const report = await auditLayout(url.href, scenario);
    assertLayout(report, scenario);
    const screenshotHash = await captureScreenshot(url.href, scenario);
    if ([...screenshotHashes.values()].includes(screenshotHash)) {
      throw new Error(`${scenario.name}: screenshot duplicates another browser case.`);
    }
    screenshotHashes.set(scenario.name, screenshotHash);
    reports.push(report);
  }
  process.stdout.write(`${JSON.stringify({
    chrome,
    artifactRoot,
    screenshotHashes: Object.fromEntries(screenshotHashes),
    reports,
  }, null, 2)}\n`);
} finally {
  const temporaryName = path.basename(temporaryRoot);
  if (path.dirname(temporaryRoot) !== path.resolve(tmpdir())
    || !/^graphoxide-control-center-browser-[a-zA-Z0-9_-]+$/u.test(temporaryName)) {
    throw new Error(`Refusing to remove unexpected Control Center temp path: ${temporaryRoot}`);
  }
  await rm(temporaryRoot, { recursive: true, force: true });
}

function extractStylesheet(source) {
  const match = /  <style>\n([\s\S]*?)  <\/style>/u.exec(source);
  if (!match?.[1]) throw new Error('Could not extract the production Control Center stylesheet.');
  return match[1];
}

function harnessHtml(stylesheet) {
  const longRoot = '/Users/example/Projects/a-very-large-monorepo-with-a-long-name/';
  const longGraphPath = `${longRoot}${'nested-package/'.repeat(10)}graphoxide-out/graph.json`;
  const longExecutable = `${longRoot}${'toolchain-segment/'.repeat(9)}bin/graphoxide`;
  const longConfig = `${longRoot}${'configuration-segment/'.repeat(8)}.vscode/mcp.json`;
  return `<!doctype html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <title>Graphoxide Control Center layout harness</title>
  <style>
    :root {
      --vscode-foreground: #f3f4f6;
      --vscode-descriptionForeground: #b7bdca;
      --vscode-editor-background: #252a3d;
      --vscode-editorWidget-background: #30364d;
      --vscode-sideBar-background: #292f43;
      --vscode-panel-border: #49516d;
      --vscode-testing-iconPassed: #74d99f;
      --vscode-editorWarning-foreground: #efc56d;
      --vscode-errorForeground: #f48771;
      --vscode-inputValidation-errorBorder: #f48771;
      --vscode-inputValidation-errorBackground: #4d2830;
      --vscode-inputValidation-errorForeground: #ffd7d2;
      --vscode-button-foreground: #ffffff;
      --vscode-button-background: #7951bd;
      --vscode-button-hoverBackground: #8a61cc;
      --vscode-button-secondaryForeground: #f3f4f6;
      --vscode-button-secondaryBackground: #252b3e;
      --vscode-button-secondaryHoverBackground: #3b425b;
      --vscode-button-border: #49516d;
      --vscode-textLink-foreground: #6cb6ff;
      --vscode-textLink-activeForeground: #9dccff;
      --vscode-focusBorder: #8d6ad1;
      --vscode-font-family: -apple-system, BlinkMacSystemFont, sans-serif;
      --vscode-editor-font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
    }
${stylesheet}
  </style>
</head>
<body>
  <header class="header"><div><h1>Graphoxide Control Center</h1><p class="lead">Manage and monitor this workspace's graph, automation, AI labeling, and MCP connections.</p></div><button class="secondary">Refresh status</button></header>
  <div class="overview" aria-label="Integration overview">
    <span class="chip good"><span class="dot"></span>Graph ready</span>
    <span class="chip good"><span class="dot"></span>Managed workspace</span>
    <span class="chip good"><span class="dot"></span>AI · LM Studio</span>
    <span class="chip good"><span class="dot"></span>3 MCP scopes</span>
  </div>
  <main class="dashboard">
    <section class="card" id="graph"><div class="card-head"><div><h2>Workspace graph</h2><p class="muted">representative-monorepo</p></div><span class="badge good">Ready</span></div>
      <div class="metrics"><div class="metric"><strong>23236</strong><span>Nodes</span></div><div class="metric"><strong>37312</strong><span>Edges</span></div><div class="metric"><strong>1196</strong><span>Communities</span></div></div>
      <dl><dt>Graph path</dt><dd><button class="link">${longGraphPath}</button></dd><dt>Last updated</dt><dd>8/11/2026, 4:10:43 PM</dd><dt>Source commit</dt><dd>73f3d344f883edcf53486e2d0055fe5ac1a89e71</dd></dl>
      <p class="detail">Incremental update refreshes the existing graph. Full rebuild rescans every supported input and replaces the generated graph.</p>
      <div class="actions"><button>Update incrementally</button><button class="secondary">Full rebuild…</button><button class="secondary">Open graph</button><button class="secondary">Open graph.json</button></div>
    </section>
    <div class="dashboard-secondary" id="secondary">
      <section class="card" id="managed"><div class="card-head"><div><h2>Workspace management</h2><p class="muted">Keep graph data current while you work.</p></div><span class="badge good">Enabled</span></div>
        <dl><dt>Update mode</dt><dd>Continuous watch</dd><dt>Watcher</dt><dd>Stopped</dd><dt>Workspace trust</dt><dd>Trusted</dd></dl>
        <div class="actions"><button>Change update mode</button><button class="secondary">Start watcher</button><button class="secondary">Disable management</button></div>
      </section>
      <section class="card" id="ai"><div class="card-head"><div><h2>AI community labeling</h2><p class="muted">Provider credentials stay in VS Code Secret Storage.</p></div><span class="badge good">Configured</span></div>
        <dl><dt>Provider</dt><dd>LM Studio</dd><dt>Model</dt><dd>qwen/qwen3.6-27b-with-a-representative-long-model-identifier</dd><dt>Endpoint</dt><dd>http://192.168.10.10:1234/v1</dd><dt>Credential</dt><dd>Not stored · optional</dd><dt>Request timeout</dt><dd>600 seconds</dd><dt>Trusted executable</dt><dd>${longExecutable}</dd></dl>
        <div class="actions"><button>Change AI configuration</button><button class="secondary">Improve community names</button><button class="secondary">Advanced settings</button></div>
      </section>
    </div>
    <section class="card" id="mcp"><div class="card-head"><div><h2>MCP integrations</h2><p class="muted">Connect this workspace’s Graphoxide graph to coding assistants.</p></div><span class="badge good">3 installed</span></div>
      <p class="section-intro">Each project registration starts a local stdio server in this workspace. All-project installation is no longer offered.</p>
      <div class="native"><div><h3>VS Code native MCP</h3><p class="detail">Provided directly by this extension when managed workspace mode is enabled.</p><p class="path">${longExecutable}</p></div><span class="badge good">Active</span></div>
      <div class="integrations"><article class="integration"><div class="card-head"><div><h3>Representative coding assistant</h3><p class="detail">Project-scoped configuration with a deliberately long location.</p></div><span class="badge good">Detected</span></div><div class="scope-grid"><div class="scope"><div class="scope-head"><div><h3>This project</h3><span class="detail">Project scope</span></div><span class="badge good">Installed</span></div><p class="path">${longConfig}</p><p class="detail">Configuration is current.</p><div class="actions"><button class="secondary">Remove</button><button class="secondary">Open config</button></div></div></div></article></div>
    </section>
  </main>
  <output id="layout-result" hidden></output>
  <script>
    const rectangle = id => {
      const value = document.getElementById(id).getBoundingClientRect();
      return { left: value.left, top: value.top, right: value.right, bottom: value.bottom, width: value.width, height: value.height };
    };
    const secondary = document.getElementById('secondary');
    document.getElementById('layout-result').textContent = JSON.stringify({
      viewport: { width: document.documentElement.clientWidth, height: document.documentElement.clientHeight },
      scrollWidth: document.documentElement.scrollWidth,
      columns: getComputedStyle(secondary).gridTemplateColumns.split(' ').filter(Boolean).length,
      graph: rectangle('graph'),
      secondary: rectangle('secondary'),
      managed: rectangle('managed'),
      ai: rectangle('ai'),
      mcp: rectangle('mcp'),
    });
  </script>
</body>
</html>`;
}

async function auditLayout(url, scenario) {
  const profile = await mkdtemp(path.join(temporaryRoot, 'profile-'));
  const { stdout, stderr } = await executeChrome(chromeArguments(profile, scenario, url, ['--dump-dom']));
  if (stderr && /(?:uncaught|fatal error)/iu.test(stderr)) throw new Error(stderr);
  const match = /<output[^>]*id="layout-result"[^>]*>([\s\S]*?)<\/output>/u.exec(stdout);
  if (!match?.[1]) throw new Error(`Chrome did not return a ${scenario.name} Control Center layout report.`);
  return JSON.parse(match[1]);
}

function assertLayout(report, scenario) {
  if (report.scrollWidth > report.viewport.width + 1) {
    throw new Error(`${scenario.name}: horizontal overflow (${report.scrollWidth}px > ${report.viewport.width}px).`);
  }
  if (report.columns !== scenario.expectedColumns) {
    throw new Error(`${scenario.name}: expected ${scenario.expectedColumns} secondary columns, received ${report.columns}.`);
  }
  if (scenario.expectedColumns === 1 && report.viewport.width > 760) {
    throw new Error(`${scenario.name}: the effective CSS viewport did not cross the responsive breakpoint.`);
  }
  assertClose(report.graph.width, report.secondary.width, 1, `${scenario.name}: graph and secondary widths`);
  assertClose(report.graph.width, report.mcp.width, 1, `${scenario.name}: graph and MCP widths`);
  if (!(report.graph.bottom < report.secondary.top && report.secondary.bottom < report.mcp.top)) {
    throw new Error(`${scenario.name}: dashboard sections are not in graph, services, MCP order.`);
  }
  if (Math.abs(report.managed.height - report.ai.height) < 8) {
    throw new Error(`${scenario.name}: secondary cards appear stretched to an equal height.`);
  }
  if (scenario.expectedColumns === 2) {
    assertClose(report.managed.top, report.ai.top, 1, `${scenario.name}: secondary row alignment`);
    if (!(report.managed.right < report.ai.left)) throw new Error(`${scenario.name}: secondary cards did not render side by side.`);
  } else {
    assertClose(report.managed.left, report.ai.left, 1, `${scenario.name}: secondary column alignment`);
    assertClose(report.managed.width, report.ai.width, 1, `${scenario.name}: secondary card widths`);
    if (!(report.managed.bottom < report.ai.top)) throw new Error(`${scenario.name}: secondary cards did not stack in reading order.`);
  }
}

async function captureScreenshot(url, scenario) {
  const profile = await mkdtemp(path.join(temporaryRoot, 'profile-'));
  const target = path.join(artifactRoot, `control-center-${scenario.name}.png`);
  const { stderr } = await executeChrome(chromeArguments(profile, scenario, url, [`--screenshot=${target}`]));
  if (stderr && /(?:uncaught|fatal error)/iu.test(stderr)) throw new Error(stderr);
  const bytes = await readBoundedScreenshot(target);
  const signature = Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]);
  if (bytes.length < 24 || !bytes.subarray(0, signature.length).equals(signature)) {
    throw new Error(`${target} is not a complete PNG screenshot.`);
  }
  const expectedWidth = scenario.width * scenario.screenshotScale;
  const expectedHeight = scenario.height * scenario.screenshotScale;
  const width = bytes.readUInt32BE(16);
  const height = bytes.readUInt32BE(20);
  if (width !== expectedWidth || height !== expectedHeight) {
    throw new Error(`${scenario.name}: screenshot is ${width}x${height}; expected ${expectedWidth}x${expectedHeight}.`);
  }
  return createHash('sha256').update(bytes).digest('hex');
}

async function executeChrome(arguments_) {
  try {
    return await execute(chrome, arguments_, { maxBuffer: 8 * 1024 * 1024, timeout: 8_000, killSignal: 'SIGKILL' });
  } catch (error) {
    if (error && typeof error === 'object' && error.killed === true && typeof error.stdout === 'string') {
      return { stdout: error.stdout, stderr: typeof error.stderr === 'string' ? error.stderr : '' };
    }
    throw error;
  }
}

function chromeArguments(profile, scenario, url, additions) {
  return [
    '--headless=new',
    '--hide-scrollbars',
    '--disable-background-networking',
    '--disable-component-update',
    '--disable-default-apps',
    '--disable-sync',
    '--metrics-recording-only',
    '--no-first-run',
    '--no-default-browser-check',
    `--force-device-scale-factor=${scenario.screenshotScale}`,
    '--run-all-compositor-stages-before-draw',
    `--user-data-dir=${profile}`,
    `--window-size=${scenario.width},${scenario.height}`,
    ...scenario.flags,
    ...additions,
    url,
  ];
}

function assertClose(left, right, tolerance, message) {
  if (Math.abs(left - right) > tolerance) throw new Error(`${message}: ${left}px vs ${right}px.`);
}

async function readBoundedScreenshot(target) {
  const handle = await open(target, 'r');
  try {
    const metadata = await handle.stat();
    if (!metadata.isFile() || metadata.size < 2_048) throw new Error(`Chrome did not paint ${target}.`);
    if (metadata.size > 16 * 1024 * 1024) throw new Error(`${target} exceeded the bounded screenshot size.`);
    const bytes = Buffer.alloc(metadata.size);
    let offset = 0;
    while (offset < bytes.length) {
      const { bytesRead } = await handle.read(bytes, offset, bytes.length - offset, offset);
      if (bytesRead === 0) throw new Error(`${target} changed while its screenshot was read.`);
      offset += bytesRead;
    }
    const trailing = Buffer.alloc(1);
    const { bytesRead: trailingBytes } = await handle.read(trailing, 0, 1, bytes.length);
    if (trailingBytes !== 0 || (await handle.stat()).size !== metadata.size) {
      throw new Error(`${target} changed while its screenshot was read.`);
    }
    return bytes;
  } finally {
    await handle.close();
  }
}

async function locateChrome() {
  const candidates = [
    process.env.CHROME_BIN,
    '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
    '/Applications/Chromium.app/Contents/MacOS/Chromium',
    '/usr/bin/google-chrome',
    '/usr/bin/google-chrome-stable',
    '/usr/bin/chromium',
    '/usr/bin/chromium-browser',
  ].filter(Boolean);
  for (const candidate of candidates) {
    try {
      await execute(candidate, ['--version']);
      return candidate;
    } catch {
      // Continue through the fixed local browser candidates.
    }
  }
  throw new Error('A local Chrome/Chromium executable is required. Set CHROME_BIN to run Control Center browser tests.');
}
