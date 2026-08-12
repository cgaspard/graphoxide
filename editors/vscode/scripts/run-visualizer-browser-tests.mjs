import { execFile } from 'node:child_process';
import { createServer } from 'node:http';
import { mkdir, mkdtemp, readFile, rm, stat } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { promisify } from 'node:util';
import { createRequire } from 'node:module';
import { inflateSync } from 'node:zlib';

const execute = promisify(execFile);
const extensionRoot = process.cwd();
const chrome = await locateChrome();
const temporaryRoot = await mkdtemp(path.join(tmpdir(), 'graphoxide-visualizer-browser-'));
const artifactRoot = process.env.GRAPHOXIDE_VISUALIZER_ARTIFACTS
  ? path.resolve(process.env.GRAPHOXIDE_VISUALIZER_ARTIFACTS)
  : path.join(temporaryRoot, 'artifacts');
const require = createRequire(import.meta.url);
const { visualizerFixture } = require(path.join(extensionRoot, 'dist', 'test', 'visualizer-fixtures.js'));
const fixtureNames = new Set(['small', 'medium', 'dense', 'maximum', 'maximum-one-community', 'maximum-singletons']);
const fixtureCache = new Map();
const server = createServer(async (request, response) => {
  try {
    const url = new URL(request.url ?? '/', 'http://127.0.0.1');
    if (url.pathname === '/fixture.json') {
      const name = url.searchParams.get('name') ?? '';
      if (!fixtureNames.has(name)) return respond(response, 404, 'text/plain', 'Unknown fixture.');
      let fixture = fixtureCache.get(name);
      if (!fixture) {
        fixture = JSON.stringify(visualizerFixture(name));
        fixtureCache.set(name, fixture);
      }
      return respond(response, 200, 'application/json; charset=utf-8', fixture);
    }
    const relative = url.pathname === '/'
      ? 'test/browser/graph-visualizer-harness.html'
      : url.pathname.slice(1);
    if (!/^(?:media|dist\/webview|test\/browser)\/[a-zA-Z0-9._/-]+$/u.test(relative) || relative.includes('..')) {
      return respond(response, 404, 'text/plain', 'Not found.');
    }
    const file = path.resolve(extensionRoot, relative);
    if (!file.startsWith(`${extensionRoot}${path.sep}`)) return respond(response, 404, 'text/plain', 'Not found.');
    const content = await readFile(file);
    const type = file.endsWith('.html') ? 'text/html; charset=utf-8'
      : file.endsWith('.css') ? 'text/css; charset=utf-8'
        : file.endsWith('.js') ? 'text/javascript; charset=utf-8'
          : 'application/octet-stream';
    respond(response, 200, type, content);
  } catch (error) {
    respond(response, 500, 'text/plain', error instanceof Error ? error.message : String(error));
  }
});

try {
  await mkdir(artifactRoot, { recursive: true });
  const address = await listen(server);
  const origin = `http://127.0.0.1:${address.port}`;
  const cases = [
    ['small', 42, 1440, 960],
    ['medium', 240, 1440, 960],
    ['dense', 750, 1440, 960],
    ['dense', 750, 720, 900],
    ['maximum', 5_000, 1440, 960],
    ['maximum-one-community', 5_000, 1440, 960],
    ['maximum-singletons', 5_000, 1440, 960],
    ['maximum-singletons', 5_000, 720, 900],
  ];
  const selectedFixture = process.env.GRAPHOXIDE_VISUALIZER_FIXTURE;
  const selectedCases = selectedFixture
    ? cases.filter(([fixture]) => fixture === selectedFixture)
    : cases;
  if (selectedCases.length === 0) {
    throw new Error(`Unknown GRAPHOXIDE_VISUALIZER_FIXTURE: ${selectedFixture}`);
  }
  const reports = [];
  for (const [fixture, nodes, width, height] of selectedCases) {
    const report = await runAudit(origin, fixture, nodes, width, height);
    auditGeometry(report, `${fixture}@${width}x${height}`);
    reports.push(summarize(report));
  }
  if (!selectedFixture || selectedFixture === 'dense') {
    const repeat = await runAudit(origin, 'dense', 750, 1440, 960);
    const firstDense = await runAudit(origin, 'dense', 750, 1440, 960);
    assertEqualGeometry(firstDense, repeat, 'dense deterministic geometry');
  }

  if (!selectedFixture) {
    const animatedTrace = await runScenarioAudit(origin, 'dense', 750, 1440, 960, 'trace', []);
    assertScenario(animatedTrace, 'dense-trace-animation', 'trace');
  }

  const screenshots = [
    ['dense-default', 'dense', 750, 1440, 960, 'default', []],
    ['dense-lens', 'dense', 750, 1440, 960, 'lens', []],
    ['dense-filtered', 'dense', 750, 1440, 960, 'filtered', []],
    ['dense-narrow', 'dense', 750, 720, 900, 'default', []],
    ['dense-reduced-motion', 'dense', 750, 1440, 960, 'trace', ['--force-prefers-reduced-motion']],
    ['dense-forced-colors', 'dense', 750, 1440, 960, 'default', ['--force-high-contrast']],
  ];
  if (!selectedFixture) {
    for (const [name, fixture, nodes, width, height, scenario, flags] of screenshots) {
      await captureScreenshot(origin, name, fixture, nodes, width, height, scenario, flags);
    }
  }
  process.stdout.write(`${JSON.stringify({ chrome, artifactRoot, reports }, null, 2)}\n`);
} finally {
  await new Promise((resolve) => server.close(resolve));
  const expectedPrefix = path.join(tmpdir(), 'graphoxide-visualizer-browser-');
  if (!temporaryRoot.startsWith(expectedPrefix)) throw new Error(`Refusing to remove unexpected visualizer temp path: ${temporaryRoot}`);
  await rm(temporaryRoot, { recursive: true, force: true });
}

async function runAudit(origin, fixture, nodes, width, height) {
  const profile = await mkdtemp(path.join(temporaryRoot, 'profile-'));
  const url = `${origin}/?fixture=${encodeURIComponent(fixture)}&nodes=${nodes}&scenario=default&scheduler=timer`;
  const { stdout, stderr } = await executeChrome(chromeArguments(profile, width, height, url, [
    '--dump-dom',
    '--virtual-time-budget=6000',
  ]), 16 * 1024 * 1024);
  if (stderr && /(?:uncaught|fatal error)/iu.test(stderr)) throw new Error(stderr);
  const match = /<pre[^>]*id="gx-result"[^>]*>([\s\S]*?)<\/pre>/u.exec(stdout);
  if (!match?.[1]) throw new Error(`Chrome did not return a visualizer audit result for ${fixture}@${width}x${height}. Tail:\n${stdout.slice(-2_000)}`);
  return JSON.parse(decodeHtmlText(match[1]));
}

async function captureScreenshot(origin, name, fixture, nodes, width, height, scenario, flags) {
  const report = await runScenarioAudit(origin, fixture, nodes, width, height, scenario, flags);
  assertScenario(report, name, scenario);
  const profile = await mkdtemp(path.join(temporaryRoot, 'profile-'));
  const target = path.join(artifactRoot, `${name}.png`);
  const url = `${origin}/?fixture=${encodeURIComponent(fixture)}&nodes=${nodes}&scenario=${encodeURIComponent(scenario)}&fixtureLoad=sync`;
  const { stderr } = await executeChrome(chromeArguments(profile, width, height, url, [
    `--screenshot=${target}`,
    '--virtual-time-budget=6000',
    ...flags,
  ]), 4 * 1024 * 1024);
  if (stderr && /(?:uncaught|fatal error)/iu.test(stderr)) throw new Error(stderr);
  const metadata = await stat(target);
  if (!metadata.isFile() || metadata.size === 0) throw new Error(`Chrome did not create ${name}.png.`);
  if (metadata.size > 24 * 1024 * 1024) throw new Error(`${name}.png exceeded the bounded screenshot size.`);
  await assertScreenshotPainted(target, width, height, name);
}

function assertScenario(report, name, scenario) {
  if (report.errors.length > 0) throw new Error(`${name}: browser errors: ${report.errors.join('\n')}`);
  if (scenario === 'trace' && report.rendererState?.traceActive !== true) throw new Error(`${name}: trace did not activate.`);
  if (scenario === 'lens' && report.rendererState?.mode !== 'focus') throw new Error(`${name}: Lens did not open.`);
  if (scenario === 'filtered' && report.rendererState?.communityFilter === null) throw new Error(`${name}: community filter did not activate.`);
}

async function assertScreenshotPainted(target, expectedWidth, expectedHeight, name) {
  const png = decodeChromePng(await readFile(target));
  if (png.width !== expectedWidth || png.height !== expectedHeight) {
    throw new Error(`${name}.png has ${png.width}x${png.height} pixels; expected ${expectedWidth}x${expectedHeight}.`);
  }
  const bounds = expectedWidth <= 800
    ? { left: 0.02, right: 0.98, top: 0.40, bottom: 0.90 }
    : { left: 0.15, right: 0.83, top: 0.14, bottom: 0.88 };
  const colors = new Map();
  forEachScreenshotPixel(png, bounds, (red, green, blue) => {
    const key = (red << 16) | (green << 8) | blue;
    colors.set(key, (colors.get(key) ?? 0) + 1);
  });
  let dominant = 0;
  let dominantCount = -1;
  for (const [color, count] of colors) {
    if (count > dominantCount) {
      dominant = color;
      dominantCount = count;
    }
  }
  const dominantRed = dominant >>> 16;
  const dominantGreen = (dominant >>> 8) & 0xff;
  const dominantBlue = dominant & 0xff;
  let paintedPixels = 0;
  forEachScreenshotPixel(png, bounds, (red, green, blue) => {
    if (Math.abs(red - dominantRed) + Math.abs(green - dominantGreen) + Math.abs(blue - dominantBlue) > 35) {
      paintedPixels += 1;
    }
  });
  const minimumPaintedPixels = expectedWidth <= 800 ? 10_000 : 750;
  if (paintedPixels < minimumPaintedPixels) {
    throw new Error(`${name}.png contains only ${paintedPixels} painted graph-stage pixels; the canvas was likely captured before rendering.`);
  }
}

function forEachScreenshotPixel(png, bounds, callback) {
  const startX = Math.floor(png.width * bounds.left);
  const endX = Math.floor(png.width * bounds.right);
  const startY = Math.floor(png.height * bounds.top);
  const endY = Math.floor(png.height * bounds.bottom);
  for (let y = startY; y < endY; y += 1) {
    for (let x = startX; x < endX; x += 1) {
      const offset = (y * png.width + x) * png.channels;
      callback(png.pixels[offset], png.pixels[offset + 1], png.pixels[offset + 2]);
    }
  }
}

function decodeChromePng(bytes) {
  const signature = Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]);
  if (bytes.length < signature.length || !bytes.subarray(0, signature.length).equals(signature)) {
    throw new Error('Chrome screenshot is not a PNG file.');
  }
  let offset = signature.length;
  let width = 0;
  let height = 0;
  let channels = 0;
  const imageData = [];
  while (offset + 12 <= bytes.length) {
    const length = bytes.readUInt32BE(offset);
    const type = bytes.toString('ascii', offset + 4, offset + 8);
    const dataStart = offset + 8;
    const dataEnd = dataStart + length;
    if (dataEnd + 4 > bytes.length) throw new Error('Chrome screenshot has a truncated PNG chunk.');
    const data = bytes.subarray(dataStart, dataEnd);
    if (type === 'IHDR') {
      width = data.readUInt32BE(0);
      height = data.readUInt32BE(4);
      const bitDepth = data[8];
      const colorType = data[9];
      const compression = data[10];
      const filter = data[11];
      const interlace = data[12];
      channels = colorType === 2 ? 3 : colorType === 6 ? 4 : 0;
      if (bitDepth !== 8 || channels === 0 || compression !== 0 || filter !== 0 || interlace !== 0) {
        throw new Error('Chrome screenshot uses an unsupported PNG encoding.');
      }
      if (width === 0 || height === 0 || width * height > 4_000_000) {
        throw new Error('Chrome screenshot dimensions exceed the bounded audit surface.');
      }
    } else if (type === 'IDAT') {
      imageData.push(data);
    } else if (type === 'IEND') {
      break;
    }
    offset = dataEnd + 4;
  }
  if (width === 0 || height === 0 || imageData.length === 0) throw new Error('Chrome screenshot is missing required PNG chunks.');
  const encoded = inflateSync(Buffer.concat(imageData));
  const stride = width * channels;
  if (encoded.length !== height * (stride + 1)) throw new Error('Chrome screenshot has an unexpected PNG payload size.');
  const pixels = Buffer.allocUnsafe(height * stride);
  let encodedOffset = 0;
  let previous = Buffer.alloc(stride);
  for (let y = 0; y < height; y += 1) {
    const filter = encoded[encodedOffset];
    encodedOffset += 1;
    const row = pixels.subarray(y * stride, (y + 1) * stride);
    for (let x = 0; x < stride; x += 1) {
      const value = encoded[encodedOffset];
      encodedOffset += 1;
      const left = x >= channels ? row[x - channels] : 0;
      const above = previous[x];
      const upperLeft = x >= channels ? previous[x - channels] : 0;
      let predictor;
      if (filter === 0) predictor = 0;
      else if (filter === 1) predictor = left;
      else if (filter === 2) predictor = above;
      else if (filter === 3) predictor = Math.floor((left + above) / 2);
      else if (filter === 4) predictor = paeth(left, above, upperLeft);
      else throw new Error(`Chrome screenshot uses unknown PNG filter ${filter}.`);
      row[x] = (value + predictor) & 0xff;
    }
    previous = row;
  }
  return { width, height, channels, pixels };
}

function paeth(left, above, upperLeft) {
  const prediction = left + above - upperLeft;
  const leftDistance = Math.abs(prediction - left);
  const aboveDistance = Math.abs(prediction - above);
  const upperLeftDistance = Math.abs(prediction - upperLeft);
  if (leftDistance <= aboveDistance && leftDistance <= upperLeftDistance) return left;
  return aboveDistance <= upperLeftDistance ? above : upperLeft;
}

async function runScenarioAudit(origin, fixture, nodes, width, height, scenario, flags) {
  const profile = await mkdtemp(path.join(temporaryRoot, 'profile-'));
  const url = `${origin}/?fixture=${encodeURIComponent(fixture)}&nodes=${nodes}&scenario=${encodeURIComponent(scenario)}&scheduler=timer`;
  const { stdout, stderr } = await executeChrome(chromeArguments(profile, width, height, url, [
    '--dump-dom',
    '--virtual-time-budget=6000',
    ...flags,
  ]), 16 * 1024 * 1024);
  if (stderr && /(?:uncaught|fatal error)/iu.test(stderr)) throw new Error(stderr);
  const match = /<pre[^>]*id="gx-result"[^>]*>([\s\S]*?)<\/pre>/u.exec(stdout);
  if (!match?.[1]) throw new Error(`Chrome did not complete ${fixture}/${scenario} at ${width}x${height}.`);
  return JSON.parse(decodeHtmlText(match[1]));
}

async function executeChrome(arguments_, maxBuffer) {
  try {
    return await execute(chrome, arguments_, { maxBuffer, timeout: 8_000, killSignal: 'SIGKILL' });
  } catch (error) {
    if (error && typeof error === 'object' && error.killed === true && typeof error.stdout === 'string') {
      return { stdout: error.stdout, stderr: typeof error.stderr === 'string' ? error.stderr : '' };
    }
    throw error;
  }
}

function chromeArguments(profile, width, height, url, additions) {
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
    '--force-device-scale-factor=1',
    '--enable-precise-memory-info',
    '--run-all-compositor-stages-before-draw',
    `--user-data-dir=${profile}`,
    `--window-size=${width},${height}`,
    ...additions,
    url,
  ];
}

function auditGeometry(report, name) {
  if (report.errors.length > 0) {
    throw new Error(`${name}: browser errors: ${report.errors.join('\n')}\nState: ${JSON.stringify({
      diagnostics: report.diagnostics,
      rendererState: report.rendererState,
    })}`);
  }
  const { diagnostics } = report;
  if (diagnostics.glyphs.length !== diagnostics.visibleNodes) {
    throw new Error(`${name}: projected ${diagnostics.glyphs.length} of ${diagnostics.visibleNodes} glyphs at scale ${diagnostics.scale} (fit ${diagnostics.fittedScale}).`);
  }
  for (const glyph of diagnostics.glyphs) assertFiniteRectangle(glyph, `${name} glyph`);
  for (const label of diagnostics.labels) assertFiniteRectangle(label, `${name} label`);
  const ordinary = diagnostics.glyphs.filter((glyph) => !glyph.emphasized).sort((left, right) => left.left - right.left || left.top - right.top);
  auditNonOverlapping(ordinary, name, 'glyph');
  auditNonOverlapping(diagnostics.labels, name, 'label');
  for (const label of diagnostics.labels) {
    for (const glyph of ordinary) {
      if (label.kind === 'node' && label.itemIndex === glyph.nodeIndex) continue;
      if (rectanglesOverlap(label, glyph)) throw new Error(`${name}: ${label.kind} label overlaps node ${glyph.nodeIndex}.`);
    }
  }
  const { width, height } = diagnostics.viewport;
  for (const glyph of ordinary) {
    if (glyph.left < -0.5 || glyph.top < -0.5 || glyph.right > width + 0.5 || glyph.bottom > height + 0.5) {
      throw new Error(`${name}: fitted glyph ${glyph.nodeIndex} is clipped.`);
    }
  }
  if (report.domElements > 1_200) throw new Error(`${name}: DOM surface exceeded its bounded canvas/card budget.`);
}

function auditNonOverlapping(rectangles, name, kind) {
  const active = [];
  for (const rectangle of rectangles) {
    while (active.length > 0 && active[0].right <= rectangle.left) active.shift();
    for (const other of active) {
      if (rectanglesOverlap(rectangle, other)) {
        throw new Error(`${name}: ${kind} rectangles overlap: ${JSON.stringify(other)} and ${JSON.stringify(rectangle)}.`);
      }
    }
    const insertion = active.findIndex((entry) => entry.right > rectangle.right);
    if (insertion < 0) active.push(rectangle);
    else active.splice(insertion, 0, rectangle);
  }
}

function rectanglesOverlap(left, right) {
  return left.left < right.right && left.right > right.left && left.top < right.bottom && left.bottom > right.top;
}

function assertFiniteRectangle(rectangle, name) {
  for (const key of ['left', 'top', 'right', 'bottom']) {
    if (!Number.isFinite(rectangle[key])) throw new Error(`${name} has a non-finite ${key}.`);
  }
  if (rectangle.left > rectangle.right || rectangle.top > rectangle.bottom) throw new Error(`${name} is inverted.`);
}

function assertEqualGeometry(left, right, name) {
  const select = (report) => ({
    scale: report.diagnostics.scale,
    fittedScale: report.diagnostics.fittedScale,
    glyphs: report.diagnostics.glyphs,
    labels: report.diagnostics.labels,
  });
  if (JSON.stringify(select(left)) !== JSON.stringify(select(right))) throw new Error(`${name} changed between identical Chrome runs.`);
}

function summarize(report) {
  const diagnostics = report.diagnostics;
  return {
    fixture: report.fixture,
    viewport: diagnostics.viewport,
    nodes: diagnostics.visibleNodes,
    labels: diagnostics.labels.length,
    scale: diagnostics.scale,
    usedJsHeapBytes: report.usedJsHeapBytes,
  };
}

function decodeHtmlText(value) {
  return value.replaceAll('&amp;', '&').replaceAll('&lt;', '<').replaceAll('&gt;', '>');
}

function respond(response, status, type, body) {
  response.writeHead(status, { 'content-type': type, 'cache-control': 'no-store', 'x-content-type-options': 'nosniff' });
  response.end(body);
}

async function listen(httpServer) {
  await new Promise((resolve, reject) => {
    httpServer.once('error', reject);
    httpServer.listen(0, '127.0.0.1', resolve);
  });
  const address = httpServer.address();
  if (!address || typeof address === 'string') throw new Error('Visualizer browser server did not bind to TCP.');
  return address;
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
  throw new Error('A local Chrome/Chromium executable is required. Set CHROME_BIN to run visualizer browser tests.');
}
