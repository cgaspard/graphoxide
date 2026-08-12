import { createHash } from 'node:crypto';
import {
  lstat,
  mkdir,
  mkdtemp,
  open,
  readFile,
  realpath,
  rm,
  stat,
  writeFile,
} from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
import { executeBrowserProcess } from './browser-process.mjs';

const extensionRoot = process.cwd();
const sourcePath = process.env.GRAPHOXIDE_CONTROL_CENTER_BUILD_SUMMARY_SOURCE
  ? path.resolve(process.env.GRAPHOXIDE_CONTROL_CENTER_BUILD_SUMMARY_SOURCE)
  : path.join(extensionRoot, 'src', 'control-center.ts');
const temporaryRoot = await mkdtemp(path.join(tmpdir(), 'graphoxide-control-center-build-summary-browser-'));
const artifactRoot = process.env.GRAPHOXIDE_CONTROL_CENTER_BUILD_SUMMARY_ARTIFACTS
  ? path.resolve(process.env.GRAPHOXIDE_CONTROL_CENTER_BUILD_SUMMARY_ARTIFACTS)
  : await mkdtemp(path.join(tmpdir(), 'graphoxide-control-center-build-summary-artifacts-'));
const expectDashboardFlow = process.env.GRAPHOXIDE_CONTROL_CENTER_BUILD_SUMMARY_EXPECT_DASHBOARD === '1';
const chrome = await locateChrome();

try {
  await mkdir(artifactRoot, { recursive: true });
  const artifactMetadata = await stat(artifactRoot);
  if (!artifactMetadata.isDirectory()) throw new Error(`Artifact destination is not a directory: ${artifactRoot}`);

  const source = await readFile(sourcePath, 'utf8');
  const { stylesheet, renderer } = extractProductionWebview(source);
  const harnessPath = path.join(temporaryRoot, 'control-center-build-summary.html');
  await writeFile(harnessPath, harnessHtml(stylesheet, renderer), 'utf8');
  const harnessUrl = pathToFileURL(harnessPath).href;
  const scenarios = [
    { name: 'wide', width: 1440, height: 1800, screenshotScale: 1 },
    { name: 'narrow', width: 500, height: 3000, screenshotScale: 1 },
    // A 1,000-device-pixel editor at 200% zoom has a 500-CSS-pixel layout viewport.
    { name: 'high-zoom', width: 500, height: 2400, screenshotScale: 2 },
  ];
  const reports = [];
  const screenshotHashes = new Map();
  for (const scenario of scenarios) {
    const report = await auditRenderedSummary(harnessUrl, scenario);
    assertRenderedSummary(report, scenario);
    const screenshotHash = await captureScreenshot(harnessUrl, scenario);
    if ([...screenshotHashes.values()].includes(screenshotHash)) {
      throw new Error(`${scenario.name}: screenshot duplicates another browser case.`);
    }
    screenshotHashes.set(scenario.name, screenshotHash);
    reports.push(report);
  }

  process.stdout.write(`${JSON.stringify({
    chrome,
    sourcePath,
    artifactRoot,
    screenshotHashes: Object.fromEntries(screenshotHashes),
    reports,
  }, null, 2)}\n`);
} finally {
  await removeValidatedTemporaryRoot(temporaryRoot);
}

function extractProductionWebview(source) {
  const stylesheetMatch = /  <style>\n([\s\S]*?)  <\/style>/u.exec(source);
  if (!stylesheetMatch?.[1]) {
    throw new Error(`Could not extract the production Control Center stylesheet from ${sourcePath}.`);
  }
  const startMarker = '<script nonce="${nonce}">';
  const scriptStart = source.indexOf(startMarker);
  const scriptEnd = source.indexOf('</script>', scriptStart);
  if (scriptStart < 0 || scriptEnd <= scriptStart) {
    throw new Error(`Could not extract the production Control Center renderer from ${sourcePath}.`);
  }
  return {
    stylesheet: stylesheetMatch[1],
    renderer: source.slice(scriptStart + startMarker.length, scriptEnd),
  };
}

function harnessHtml(stylesheet, renderer) {
  return `<!doctype html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <title>Graphoxide Control Center build summary harness</title>
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
  <header class="header"><div><h1>Graphoxide Control Center</h1><p class="lead">Manage and monitor this workspace's graph, automation, AI labeling, and MCP connections.</p></div><button class="secondary" id="refresh">Refresh status</button></header>
  <div id="error" class="error" role="alert" hidden></div>
  <div id="content" class="loading" aria-live="polite">Loading Graphoxide status…</div>
  <output id="build-summary-result" hidden></output>
  <script>globalThis.acquireVsCodeApi = () => ({ postMessage() {} });</script>
  <script>${renderer}</script>
  <script>
    const fixtureState = {
      type: 'state',
      workspace: { name: 'representative-monorepo', path: '/workspace', trusted: true },
      graph: {
        status: 'ready', exists: true, path: '/workspace/graphoxide-out/graph.json', error: null,
        nodes: 23236, edges: 37312, communities: 1196,
        modified: Date.parse('2026-08-11T20:10:43.000Z'),
        builtAtCommit: '73f3d344f883edcf53486e2d0055fe5ac1a89e71',
        latestIndex: null,
      },
      managed: { enabled: true, freshness: 'watch', watching: true },
      ai: {
        enabled: true, provider: 'LM Studio', endpoint: 'http://127.0.0.1:1234/v1',
        model: 'qwen/qwen3.6-27b', credentialPresent: false, credentialRequired: false,
        executable: '/workspace/bin/graphoxide', executableError: null, configurationError: null,
        timeoutSeconds: 600,
      },
      mcp: {
        nativeEnabled: true, invocation: 'graphoxide serve', configuredScopes: 0,
        staleScopes: 0, rows: [],
      },
    };
    const sendState = data => window.dispatchEvent(new MessageEvent('message', { data }));
    const summaryHeadings = () => [...document.querySelectorAll('h3')]
      .filter(element => element.textContent.trim() === 'Latest index');
    sendState(fixtureState);
    const initialSummaryCount = summaryHeadings().length;
    const completedAt = Date.parse('2026-08-11T20:24:05.000Z');
    sendState({
      ...fixtureState,
      graph: {
        ...fixtureState.graph,
        latestIndex: {
          operation: 'update', mode: 'incremental', status: 'rebuilt', elapsedMs: 12874,
          stagesMs: { scan_extract: 2300, detect: 125, extract: 2400, build: 789, cluster: 1025, write: 55 },
          files: { indexed: 1234, changed: 37, deleted: 5 },
          sourceBytes: 8912896,
          completedAt,
        },
      },
    });

    const rect = element => {
      const value = element.getBoundingClientRect();
      return { left: value.left, top: value.top, right: value.right, bottom: value.bottom, width: value.width, height: value.height };
    };
    const visible = element => {
      const value = element.getBoundingClientRect();
      const style = getComputedStyle(element);
      return value.width > 0 && value.height > 0 && style.display !== 'none' && style.visibility !== 'hidden'
        && Number(style.opacity) !== 0 && value.right > 0 && value.left < innerWidth && value.bottom > 0 && value.top < innerHeight;
    };
    const headings = summaryHeadings();
    const graph = document.querySelector('[aria-labelledby="graph-heading"]');
    const heading = headings[0];
    const summary = heading && heading.nextElementSibling;
    const pairs = {};
    if (summary) {
      for (const term of summary.querySelectorAll('dt')) pairs[term.textContent.trim()] = term.nextElementSibling?.textContent.trim() || '';
    }
    const graphLists = graph ? [...graph.querySelectorAll(':scope > dl')] : [];
    const detail = graph?.querySelector(':scope > p.detail');
    const dashboard = document.querySelector('.dashboard');
    const secondary = document.querySelector('.dashboard-secondary');
    document.getElementById('build-summary-result').textContent = JSON.stringify({
      viewport: { width: document.documentElement.clientWidth, height: document.documentElement.clientHeight },
      scrollWidth: document.documentElement.scrollWidth,
      initialSummaryCount,
      summaryCount: headings.length,
      summaryInsideGraph: Boolean(graph && heading && summary && graph.contains(heading) && graph.contains(summary)),
      headingVisible: Boolean(heading && visible(heading)),
      summaryVisible: Boolean(summary && visible(summary)),
      graph: graph ? rect(graph) : null,
      heading: heading ? rect(heading) : null,
      summary: summary ? rect(summary) : null,
      baseDetails: graphLists[0] ? rect(graphLists[0]) : null,
      detail: detail ? rect(detail) : null,
      pairs,
      expectedCompleted: new Date(completedAt).toLocaleString(),
      dashboard: dashboard ? rect(dashboard) : null,
      secondary: secondary ? rect(secondary) : null,
      secondaryColumns: secondary ? getComputedStyle(secondary).gridTemplateColumns.split(' ').filter(Boolean).length : null,
    });
  </script>
</body>
</html>`;
}

async function auditRenderedSummary(url, scenario) {
  const profile = await mkdtemp(path.join(temporaryRoot, 'profile-'));
  const { stdout, stderr } = await executeChrome(chromeArguments(profile, scenario, url, ['--dump-dom']));
  if (stderr && /(?:uncaught|fatal error)/iu.test(stderr)) throw new Error(stderr);
  const match = /<output[^>]*id="build-summary-result"[^>]*>([\s\S]*?)<\/output>/u.exec(stdout);
  if (!match?.[1]) throw new Error(`Chrome did not return a ${scenario.name} build-summary report.`);
  return JSON.parse(match[1]);
}

function assertRenderedSummary(report, scenario) {
  if (report.scrollWidth > report.viewport.width + 1) {
    throw new Error(`${scenario.name}: horizontal overflow (${report.scrollWidth}px > ${report.viewport.width}px).`);
  }
  if (report.initialSummaryCount !== 0) {
    throw new Error(`${scenario.name}: latestIndex: null rendered a build summary.`);
  }
  if (report.summaryCount !== 1) {
    throw new Error(`${scenario.name}: expected exactly one post-success summary, received ${report.summaryCount}.`);
  }
  if (!report.summaryInsideGraph || !report.headingVisible || !report.summaryVisible) {
    throw new Error(`${scenario.name}: build summary is not visible inside the Workspace graph card.`);
  }
  for (const [key, expected] of Object.entries({
    'Total time': '13 s',
    Operation: 'Incremental update',
    'Indexed inputs': '1234',
    'Indexed source size': '8.5 MiB',
    'Changed / deleted': '37 / 5',
    Completed: report.expectedCompleted,
    Stages: 'scan/extract 2.3 s · detect 125 ms · extract 2.4 s · build 789 ms · cluster 1.0 s · write 55 ms',
  })) {
    if (report.pairs[key] !== expected) {
      throw new Error(`${scenario.name}: ${key} rendered as ${JSON.stringify(report.pairs[key])}; expected ${JSON.stringify(expected)}.`);
    }
  }
  if (!contains(report.graph, report.heading) || !contains(report.graph, report.summary)) {
    throw new Error(`${scenario.name}: build summary crosses the graph-card bounds.`);
  }
  if (!(report.baseDetails.bottom <= report.heading.top + 1 && report.summary.bottom <= report.detail.top + 1)) {
    throw new Error(`${scenario.name}: build summary is not ordered between graph metadata and graph actions.`);
  }
  if (expectDashboardFlow) assertDashboardFlow(report, scenario);
}

function assertDashboardFlow(report, scenario) {
  if (!report.dashboard || !report.secondary) {
    throw new Error(`${scenario.name}: expected the #85 dashboard card-flow structure.`);
  }
  const expectedColumns = scenario.width <= 760 ? 1 : 2;
  if (report.secondaryColumns !== expectedColumns) {
    throw new Error(`${scenario.name}: expected ${expectedColumns} dashboard service columns, received ${report.secondaryColumns}.`);
  }
  if (Math.abs(report.dashboard.width - report.graph.width) > 1) {
    throw new Error(`${scenario.name}: the graph card is not full-width in the dashboard flow.`);
  }
}

async function captureScreenshot(url, scenario) {
  const profile = await mkdtemp(path.join(temporaryRoot, 'profile-'));
  const target = path.join(artifactRoot, `control-center-build-summary-${scenario.name}.png`);
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
  return executeBrowserProcess(chrome, arguments_, {
    env: { ...process.env, LANG: 'en_US.UTF-8', TZ: 'UTC' },
    maxBuffer: 8 * 1024 * 1024,
    acceptTimedOutStdout: arguments_.includes('--dump-dom')
      ? (stdout) => /<output[^>]*id="build-summary-result"[^>]*>[\s\S]*\S[\s\S]*<\/output>/u.test(stdout)
      : () => true,
  });
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
    '--lang=en-US',
    `--force-device-scale-factor=${scenario.screenshotScale}`,
    '--run-all-compositor-stages-before-draw',
    `--user-data-dir=${profile}`,
    `--window-size=${scenario.width},${scenario.height}`,
    ...additions,
    url,
  ];
}

function contains(outer, inner) {
  return outer && inner
    && inner.left >= outer.left - 1 && inner.right <= outer.right + 1
    && inner.top >= outer.top - 1 && inner.bottom <= outer.bottom + 1;
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

async function removeValidatedTemporaryRoot(target) {
  const systemTemporaryRoot = await realpath(tmpdir());
  const resolvedTarget = await realpath(target);
  const metadata = await lstat(resolvedTarget);
  if (metadata.isSymbolicLink() || !metadata.isDirectory()
    || path.dirname(resolvedTarget) !== systemTemporaryRoot
    || !/^graphoxide-control-center-build-summary-browser-[a-zA-Z0-9_-]+$/u.test(path.basename(resolvedTarget))) {
    throw new Error(`Refusing to remove unexpected Control Center temp path: ${resolvedTarget}`);
  }
  await rm(resolvedTarget, { recursive: true, force: true });
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
      await executeBrowserProcess(candidate, ['--version'], { maxBuffer: 1024 * 1024 });
      return candidate;
    } catch {
      // Continue through the fixed local browser candidates.
    }
  }
  throw new Error('A local Chrome/Chromium executable is required. Set CHROME_BIN to run Control Center browser tests.');
}
