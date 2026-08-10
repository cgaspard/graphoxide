import assert from 'node:assert/strict';
import { mkdtempSync, mkdirSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import test from 'node:test';

import {
  AUDIT_SCOPES,
  EXECUTION_LIMITS,
  collectAuditReports,
  evaluateSecurityAudit,
  parseAdvisoryExceptions,
} from './security-audit.mjs';

const NOW = new Date('2026-08-10T18:30:00Z');
const RATIONALE = 'Temporary exception while the dependency upgrade is reviewed.';

test('clean injected Cargo and npm reports pass with zero exceptions', () => {
  const result = evaluateSecurityAudit({
    reports: cleanReports(),
    exceptionsText: exceptionPolicy([]),
    now: NOW,
  });

  assert.equal(result.findings, 0);
  assert.equal(result.exceptionsUsed, 0);
  assert.deepEqual(
    result.summaries.map(({ scanner, lockfile, findings }) => ({
      scanner,
      lockfile,
      findings,
    })),
    [
      { scanner: 'cargo-audit', lockfile: 'Cargo.lock', findings: 0 },
      { scanner: 'npm-audit', lockfile: 'package-lock.json', findings: 0 },
      {
        scanner: 'npm-audit',
        lockfile: 'editors/vscode/package-lock.json',
        findings: 0,
      },
    ],
  );
});

test('a current exact Cargo exception allows its matching finding', () => {
  const reports = cleanReports();
  reports.cargo = cargoReport({
    vulnerabilities: [cargoVulnerability('RUSTSEC-2026-0001', 'demo-crate')],
  });

  const result = evaluateSecurityAudit({
    reports,
    exceptionsText: exceptionPolicy([
      cargoException({ expires: '2026-08-20' }),
    ]),
    now: NOW,
  });

  assert.equal(result.findings, 1);
  assert.equal(result.exceptionsUsed, 1);
});

test('a current exact npm exception allows a GHSA finding', () => {
  const reports = cleanReports();
  reports['npm-root'] = npmReport({
    yaml: npmVulnerability({
      advisory: 'GHSA-2345-cfgh-jmpq',
      packageName: 'yaml',
    }),
  });

  const result = evaluateSecurityAudit({
    reports,
    exceptionsText: exceptionPolicy([
      {
        advisory: 'GHSA-2345-cfgh-jmpq',
        scanner: 'npm-audit',
        lockfile: 'package-lock.json',
        package: 'yaml',
        rationale: RATIONALE,
        owner: '@security-owner',
        trackingIssue: 'https://github.com/cgaspard/graphoxide/issues/51',
        expires: '2026-09-09',
      },
    ]),
    now: NOW,
  });

  assert.equal(result.findings, 1);
  assert.equal(result.exceptionsUsed, 1);
});

test('npm meta-vulnerabilities resolve to a concrete advisory', () => {
  const reports = cleanReports();
  reports['npm-root'] = npmReport({
    'root-tool': {
      name: 'root-tool',
      severity: 'high',
      via: ['yaml'],
      effects: [],
      range: '*',
      nodes: ['node_modules/root-tool'],
      fixAvailable: true,
    },
    yaml: npmVulnerability({
      advisory: 'GHSA-2345-cfgh-jmpq',
      packageName: 'yaml',
    }),
  });

  const result = evaluateSecurityAudit({
    reports,
    exceptionsText: exceptionPolicy([
      {
        advisory: 'GHSA-2345-cfgh-jmpq',
        scanner: 'npm-audit',
        lockfile: 'package-lock.json',
        package: 'yaml',
        rationale: RATIONALE,
        owner: '@security-owner',
        trackingIssue: '#51',
        expires: '2026-08-20',
      },
    ]),
    now: NOW,
  });

  assert.equal(result.findings, 1);
  assert.equal(result.exceptionsUsed, 1);
});

test('expired exceptions are rejected before reconciliation', () => {
  assert.throws(
    () =>
      parseAdvisoryExceptions(
        exceptionPolicy([cargoException({ expires: '2026-08-09' })]),
        { now: NOW },
      ),
    /exception 1 is expired/,
  );
});

test('unused exceptions are rejected even when all reports are clean', () => {
  assert.throws(
    () =>
      evaluateSecurityAudit({
        reports: cleanReports(),
        exceptionsText: exceptionPolicy([cargoException()]),
        now: NOW,
      }),
    /unused exception: cargo-audit Cargo\.lock RUSTSEC-2026-0001 demo-crate/,
  );
});

test('malformed, unknown, duplicate, and overlong exceptions fail closed', async (t) => {
  await t.test('malformed JSON', () => {
    assert.throws(
      () => parseAdvisoryExceptions('{', { now: NOW }),
      /malformed JSON/,
    );
  });

  await t.test('unknown field', () => {
    assert.throws(
      () =>
        parseAdvisoryExceptions(
          exceptionPolicy([{ ...cargoException(), blanketSuppress: true }]),
          { now: NOW },
        ),
      /missing or unknown fields/,
    );
  });

  await t.test('unsupported scope', () => {
    assert.throws(
      () =>
        parseAdvisoryExceptions(
          exceptionPolicy([
            cargoException({ scanner: 'cargo-audit', lockfile: 'Cargo.toml' }),
          ]),
          { now: NOW },
        ),
      /unsupported scanner or lockfile/,
    );
  });

  await t.test('duplicate', () => {
    assert.throws(
      () =>
        parseAdvisoryExceptions(
          exceptionPolicy([cargoException(), cargoException()]),
          { now: NOW },
        ),
      /duplicates an earlier exception/,
    );
  });

  await t.test('more than 30 days', () => {
    assert.throws(
      () =>
        parseAdvisoryExceptions(
          exceptionPolicy([cargoException({ expires: '2026-09-10' })]),
          { now: NOW },
        ),
      /more than 30 days/,
    );
  });

  await t.test('missing tracking issue', () => {
    assert.throws(
      () =>
        parseAdvisoryExceptions(
          exceptionPolicy([cargoException({ trackingIssue: '' })]),
          { now: NOW },
        ),
      /trackingIssue/,
    );
  });
});

test('unreviewed findings fail without echoing advisory source text', () => {
  const reports = cleanReports();
  reports['npm-root'] = npmReport({
    yaml: npmVulnerability({
      advisory: 'GHSA-2345-cfgh-jmpq',
      packageName: 'yaml',
      title: 'TOP SECRET SOURCE CONTENT',
    }),
  });

  assert.throws(
    () =>
      evaluateSecurityAudit({
        reports,
        exceptionsText: exceptionPolicy([]),
        now: NOW,
      }),
    (error) => {
      assert.match(error.message, /unreviewed finding/);
      assert.doesNotMatch(error.message, /TOP SECRET/);
      return true;
    },
  );
});

test('malformed and oversized scanner reports fail closed', async (t) => {
  await t.test('malformed report', () => {
    const reports = cleanReports();
    reports.cargo = '{';
    assert.throws(
      () =>
        evaluateSecurityAudit({
          reports,
          exceptionsText: exceptionPolicy([]),
          now: NOW,
        }),
      /malformed JSON/,
    );
  });

  await t.test('oversized report', () => {
    const reports = cleanReports();
    reports.cargo = ' '.repeat(EXECUTION_LIMITS.reportBytes + 1);
    assert.throws(
      () =>
        evaluateSecurityAudit({
          reports,
          exceptionsText: exceptionPolicy([]),
          now: NOW,
        }),
      /exceeds the size limit/,
    );
  });

  await t.test('Cargo warning without an advisory ID', () => {
    const reports = cleanReports();
    reports.cargo = cargoReport({
      warnings: {
        yanked: [{ package: { name: 'demo-crate', version: '1.0.0' } }],
      },
    });
    assert.throws(
      () =>
        evaluateSecurityAudit({
          reports,
          exceptionsText: exceptionPolicy([]),
          now: NOW,
        }),
      /unreviewed finding without an advisory ID/,
    );
  });
});

test('scanner commands use locked inputs and bounded, non-shell execution', (t) => {
  const directory = mkdtempSync(path.join(tmpdir(), 'graphoxide-security-audit-'));
  t.after(() => rmSync(directory, { recursive: true, force: true }));
  mkdirSync(path.join(directory, 'editors', 'vscode'), { recursive: true });
  writeFileSync(path.join(directory, 'Cargo.lock'), 'version = 4\n');
  writeFileSync(path.join(directory, 'package-lock.json'), '{}\n');
  writeFileSync(path.join(directory, 'editors', 'vscode', 'package-lock.json'), '{}\n');

  const calls = [];
  const reports = collectAuditReports({
    rootDir: directory,
    spawn(command, args, options) {
      calls.push({ command, args, options });
      return { status: 0, signal: null, stdout: Buffer.from('{}'), stderr: Buffer.alloc(0) };
    },
  });

  assert.deepEqual(Object.keys(reports), ['cargo', 'npm-root', 'npm-vscode']);
  assert.equal(calls.length, 3);
  assert.deepEqual(calls[0].args, [
    'audit',
    '--json',
    '--quiet',
    '--color',
    'never',
    '--file',
    'Cargo.lock',
    '--deny',
    'warnings',
  ]);
  for (const call of calls.slice(1)) {
    assert.ok(call.args.includes('--package-lock-only'));
    assert.ok(call.args.includes('--ignore-scripts'));
    assert.ok(call.args.includes('--audit-level=none'));
  }
  for (const call of calls) {
    assert.equal(call.options.shell, false);
    assert.equal(call.options.timeout, EXECUTION_LIMITS.timeoutMs);
    assert.equal(call.options.maxBuffer, EXECUTION_LIMITS.reportBytes);
    assert.deepEqual(call.options.stdio, ['ignore', 'pipe', 'pipe']);
  }
  assert.deepEqual(
    AUDIT_SCOPES.map(({ scanner, lockfile }) => ({ scanner, lockfile })),
    [
      { scanner: 'cargo-audit', lockfile: 'Cargo.lock' },
      { scanner: 'npm-audit', lockfile: 'package-lock.json' },
      { scanner: 'npm-audit', lockfile: 'editors/vscode/package-lock.json' },
    ],
  );
});

test('scanner failures withhold captured output and error details', (t) => {
  const directory = mkdtempSync(path.join(tmpdir(), 'graphoxide-security-audit-'));
  t.after(() => rmSync(directory, { recursive: true, force: true }));
  mkdirSync(path.join(directory, 'editors', 'vscode'), { recursive: true });
  writeFileSync(path.join(directory, 'Cargo.lock'), 'version = 4\n');
  writeFileSync(path.join(directory, 'package-lock.json'), '{}\n');
  writeFileSync(path.join(directory, 'editors', 'vscode', 'package-lock.json'), '{}\n');

  assert.throws(
    () =>
      collectAuditReports({
        rootDir: directory,
        spawn() {
          return {
            status: null,
            signal: null,
            error: new Error('TOP SECRET PROCESS DETAIL'),
            stdout: Buffer.from('TOP SECRET SOURCE'),
            stderr: Buffer.from('TOP SECRET CREDENTIAL'),
          };
        },
      }),
    (error) => {
      assert.match(error.message, /scanner output was withheld/);
      assert.doesNotMatch(error.message, /TOP SECRET/);
      return true;
    },
  );
});

function cleanReports() {
  return {
    cargo: cargoReport(),
    'npm-root': npmReport(),
    'npm-vscode': npmReport(),
  };
}

function exceptionPolicy(exceptions) {
  return JSON.stringify({ version: 1, exceptions });
}

function cargoException(overrides = {}) {
  return {
    advisory: 'RUSTSEC-2026-0001',
    scanner: 'cargo-audit',
    lockfile: 'Cargo.lock',
    package: 'demo-crate',
    rationale: RATIONALE,
    owner: '@security-owner',
    trackingIssue: 51,
    expires: '2026-08-20',
    ...overrides,
  };
}

function cargoVulnerability(advisory, packageName) {
  return {
    advisory: { id: advisory, title: 'Fixture advisory source text' },
    package: { name: packageName, version: '1.0.0' },
  };
}

function cargoReport({ vulnerabilities = [], warnings = {} } = {}) {
  return JSON.stringify({
    database: { advisoryCount: vulnerabilities.length },
    lockfile: { dependencyCount: 1 },
    settings: {},
    vulnerabilities: {
      found: vulnerabilities.length > 0,
      count: vulnerabilities.length,
      list: vulnerabilities,
    },
    warnings,
  });
}

function npmVulnerability({ advisory, packageName, title = 'Fixture advisory' }) {
  return {
    name: packageName,
    severity: 'high',
    isDirect: true,
    via: [
      {
        source: 12345,
        name: packageName,
        dependency: packageName,
        title,
        url: `https://github.com/advisories/${advisory}`,
        severity: 'high',
        range: '<1.0.0',
      },
    ],
    effects: [],
    range: '<1.0.0',
    nodes: [`node_modules/${packageName}`],
    fixAvailable: true,
  };
}

function npmReport(vulnerabilities = {}) {
  const total = Object.keys(vulnerabilities).length;
  return JSON.stringify({
    auditReportVersion: 2,
    vulnerabilities,
    metadata: {
      vulnerabilities: {
        info: 0,
        low: 0,
        moderate: 0,
        high: total,
        critical: 0,
        total,
      },
      dependencies: { prod: 0, dev: 0, optional: 0, peer: 0, peerOptional: 0, total: 0 },
    },
  });
}
