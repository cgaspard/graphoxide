#!/usr/bin/env node
// Measured Rust code coverage with a baseline-preserving ratchet (issue #36).
//
// This measures *executed* code coverage for the Rust workspace with
// `cargo llvm-cov` (line / region / function), separately from the file-admission
// and corpus "coverage" the indexing pipeline reports. It exercises the same
// unit and integration tests as the repository gates and enforces a
// ratchet: coverage may not regress below the recorded baseline.
//
// Usage:
//   node scripts/rust-coverage.mjs                 # measure + enforce ratchet
//   node scripts/rust-coverage.mjs --update        # re-measure and rewrite the baseline
//   node scripts/rust-coverage.mjs --check         # measure + enforce (CI-friendly, no update)
//
// The baseline is committed to scripts/rust-coverage-baseline.json. A regression
// against it fails the script (non-zero exit) and is reported per crate.

import { execFileSync } from 'node:child_process';
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const baselinePath = path.join(
  path.dirname(fileURLToPath(import.meta.url)),
  'rust-coverage-baseline.json',
);
const args = process.argv.slice(2);
const update = args.includes('--update');
const check = args.includes('--check') || !update;

const RATCHET_TOLERANCE = 0.2; // percentage points of allowed drift before a hard fail

function run(command, commandArgs, options = {}) {
  const result = execFileSync(command, commandArgs, {
    cwd: root,
    encoding: 'utf8',
    stdio: ['ignore', 'pipe', 'pipe'],
    ...options,
  });
  return result;
}

function parseTotals(output) {
  // The llvm-cov table ends with a TOTAL row:
  //   TOTAL  <regions> <missedRegions> <regionCover> <functions> <missedFunctions> <fnCover> <lines> <missedLines> <lineCover> ...
  const lines = output.split('\n');
  const totalLine = lines.find((line) => line.trimStart().startsWith('TOTAL'));
  if (!totalLine) {
    fail('could not find the TOTAL coverage row in llvm-cov output');
  }
  const fields = totalLine.trim().split(/\s+/);
  // TOTAL regions missedRegions regionCover% functions missedFunctions fnCover% lines missedLines lineCover%
  const regions = Number(fields[1]);
  const regionCover = Number.parseFloat(fields[3].replace('%', ''));
  const functions = Number(fields[4]);
  const fnCover = Number.parseFloat(fields[6].replace('%', ''));
  const linesCount = Number(fields[7]);
  const lineCover = Number.parseFloat(fields[9].replace('%', ''));
  if ([regions, regionCover, functions, fnCover, linesCount, lineCover].some(Number.isNaN)) {
    fail(`could not parse TOTAL coverage row: ${totalLine}`);
  }
  return {
    regions,
    regionCover,
    functions,
    fnCover,
    lines: linesCount,
    lineCover,
  };
}

function measure() {
  process.stdout.write('\n[rust-coverage] measuring Rust workspace coverage\n');
  const output = run('cargo', [
    'llvm-cov',
    '--workspace',
    '--lib',
    '--bins',
    '--tests',
    '--fail-under-lines',
    '0',
  ]);
  return { output, totals: parseTotals(output) };
}

function loadBaseline() {
  if (!existsSync(baselinePath)) return null;
  return JSON.parse(readFileSync(baselinePath, 'utf8'));
}

function fail(message) {
  process.stderr.write(`\n[rust-coverage] ${message}\n`);
  process.exit(1);
}

const { output, totals } = measure();

const baseline = loadBaseline();

if (update) {
  const record = {
    tool: 'cargo-llvm-cov',
    generated_at: new Date().toISOString(),
    scope: 'rust workspace (lib, bins, tests)',
    metrics: {
      regions: totals.regions,
      region_cover_pct: round2(totals.regionCover),
      functions: totals.functions,
      function_cover_pct: round2(totals.fnCover),
      lines: totals.lines,
      line_cover_pct: round2(totals.lineCover),
    },
    ratchet_tolerance_pct: RATCHET_TOLERANCE,
  };
  writeFileSync(baselinePath, `${JSON.stringify(record, null, 2)}\n`);
  process.stdout.write(
    `\n[rust-coverage] baseline updated: lines ${record.metrics.line_cover_pct}%, regions ${record.metrics.region_cover_pct}%, functions ${record.metrics.function_cover_pct}%\n`,
  );
  process.exit(0);
}

if (check) {
  if (!baseline) {
    fail(`no baseline found at ${path.relative(root, baselinePath)}; run with --update first`);
  }
  const base = baseline.metrics;
  const failures = [];
  for (const [label, current, recorded] of [
    ['line', totals.lineCover, base.line_cover_pct],
    ['region', totals.regionCover, base.region_cover_pct],
    ['function', totals.fnCover, base.function_cover_pct],
  ]) {
    const delta = current - recorded;
    if (delta < -RATCHET_TOLERANCE) {
      failures.push(
        `${label} coverage regressed: ${round2(current)}% (baseline ${recorded}%, delta ${round2(delta)}pp < -${RATCHET_TOLERANCE}pp)`,
      );
    } else {
      process.stdout.write(
        `[rust-coverage] ${label} coverage ${round2(current)}% (baseline ${recorded}%, delta ${delta >= 0 ? '+' : ''}${round2(delta)}pp)\n`,
      );
    }
  }
  if (failures.length > 0) {
    process.stdout.write('\n[rust-coverage] measured totals:\n');
    process.stdout.write(`  lines:    ${totals.lines} (${round2(totals.lineCover)}%)\n`);
    process.stdout.write(`  regions:  ${totals.regions} (${round2(totals.regionCover)}%)\n`);
    process.stdout.write(`  functions:${totals.functions} (${round2(totals.fnCover)}%)\n`);
    fail(
      `coverage ratchet failed:\n  - ${failures.join('\n  - ')}\n  If the change is intentional and coverage genuinely dropped, re-run with --update to re-baseline.`,
    );
  }
  process.stdout.write('[rust-coverage] ratchet passed\n');
}

function round2(value) {
  return Math.round(value * 100) / 100;
}
