// Unit tests for the Rust coverage ratchet (issue #36). These exercise the
// pure parsing/comparison logic without invoking `cargo llvm-cov`, so they run
// in milliseconds and are safe to include in the fast gate.

import test from 'node:test';
import assert from 'node:assert/strict';

// Mirror of the parser in rust-coverage.mjs, kept in sync so the ratchet logic
// is independently testable.
function parseTotalsLine(line) {
  const fields = line.trim().split(/\s+/);
  const regions = Number(fields[1]);
  const regionCover = Number.parseFloat(fields[3].replace('%', ''));
  const functions = Number(fields[4]);
  const fnCover = Number.parseFloat(fields[6].replace('%', ''));
  const linesCount = Number(fields[7]);
  const lineCover = Number.parseFloat(fields[9].replace('%', ''));
  return { regions, regionCover, functions, fnCover, lines: linesCount, lineCover };
}

function ratchetDelta(current, recorded, tolerance = 0.2) {
  const delta = current - recorded;
  return delta < -tolerance;
}

test('parses the llvm-cov TOTAL row into metrics', () => {
  const line =
    'TOTAL  182743 20486 88.79% 10140 1119 88.96% 115055 11839 89.71% 0 0 -';
  const totals = parseTotalsLine(line);
  assert.equal(totals.regions, 182743);
  assert.equal(totals.regionCover, 88.79);
  assert.equal(totals.functions, 10140);
  assert.equal(totals.fnCover, 88.96);
  assert.equal(totals.lines, 115055);
  assert.equal(totals.lineCover, 89.71);
});

test('ratchet passes when coverage is unchanged', () => {
  assert.equal(ratchetDelta(89.71, 89.71), false);
});

test('ratchet passes for small improvements', () => {
  assert.equal(ratchetDelta(90.5, 89.71), false);
});

test('ratchet tolerates a small drift within the tolerance', () => {
  // A 0.1pp drop is within the 0.2pp tolerance.
  assert.equal(ratchetDelta(89.61, 89.71), false);
});

test('ratchet fails on a regression beyond the tolerance', () => {
  // A 0.5pp drop exceeds the 0.2pp tolerance.
  assert.equal(ratchetDelta(89.2, 89.71), true);
});

test('ratchet fails on a large regression', () => {
  assert.equal(ratchetDelta(80.0, 89.71), true);
});
