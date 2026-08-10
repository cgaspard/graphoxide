#!/usr/bin/env node

import { spawnSync } from 'node:child_process';
import { lstatSync, readFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { TextDecoder } from 'node:util';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

export const EXECUTION_LIMITS = Object.freeze({
  exceptionBytes: 64 * 1024,
  reportBytes: 4 * 1024 * 1024,
  timeoutMs: 5 * 60 * 1000,
  jsonNodes: 50_000,
  jsonDepth: 64,
  exceptions: 100,
  findings: 10_000,
});

export const AUDIT_SCOPES = Object.freeze([
  Object.freeze({
    id: 'cargo',
    scanner: 'cargo-audit',
    lockfile: 'Cargo.lock',
    cwd: '.',
    command: 'cargo',
    args: Object.freeze([
      'audit',
      '--json',
      '--quiet',
      '--color',
      'never',
      '--file',
      'Cargo.lock',
      '--deny',
      'warnings',
    ]),
  }),
  Object.freeze({
    id: 'npm-root',
    scanner: 'npm-audit',
    lockfile: 'package-lock.json',
    cwd: '.',
    command: 'npm',
    args: Object.freeze([
      'audit',
      '--json',
      '--package-lock-only',
      '--ignore-scripts',
      '--audit-level=none',
    ]),
  }),
  Object.freeze({
    id: 'npm-vscode',
    scanner: 'npm-audit',
    lockfile: 'editors/vscode/package-lock.json',
    cwd: 'editors/vscode',
    command: 'npm',
    args: Object.freeze([
      'audit',
      '--json',
      '--package-lock-only',
      '--ignore-scripts',
      '--audit-level=none',
    ]),
  }),
]);

const EXCEPTION_PATH = '.github/security/advisory-exceptions.json';
const EXCEPTION_FIELDS = new Set([
  'advisory',
  'scanner',
  'lockfile',
  'package',
  'rationale',
  'owner',
  'trackingIssue',
  'expires',
]);
const WARNING_KINDS = new Set(['notice', 'unmaintained', 'unsound', 'yanked']);
const SCOPE_IDS = new Set(AUDIT_SCOPES.map((scope) => scope.id));
const SCOPE_PAIRS = new Set(
  AUDIT_SCOPES.map((scope) => `${scope.scanner}\0${scope.lockfile}`),
);
const PACKAGE_PATTERN = /^(?:@[A-Za-z0-9][A-Za-z0-9._~-]*\/)?[A-Za-z0-9][A-Za-z0-9._~-]*$/;
const OWNER_PATTERN = /^@[A-Za-z\d](?:[A-Za-z\d-]{0,37}[A-Za-z\d])?$/;
const GHSA_PATTERN = /^GHSA-[23456789CFGHJMPQRVWX]{4}-[23456789CFGHJMPQRVWX]{4}-[23456789CFGHJMPQRVWX]{4}$/;
const CVE_PATTERN = /^CVE-\d{4}-\d{4,}$/;
const NPM_PATTERN = /^NPM-\d+$/;
const RUSTSEC_PATTERN = /^RUSTSEC-\d{4}-\d{4}$/;
const ISSUE_URL_PATTERN = /^https:\/\/github\.com\/[A-Za-z\d_.-]+\/[A-Za-z\d_.-]+\/issues\/[1-9]\d*$/;
const DAY_MS = 24 * 60 * 60 * 1000;

export class SecurityAuditError extends Error {
  constructor(message) {
    super(message);
    this.name = 'SecurityAuditError';
  }
}

export function collectAuditReports({ rootDir = root, spawn = spawnSync } = {}) {
  const reports = {};

  for (const scope of AUDIT_SCOPES) {
    assertRegularFile(
      path.join(rootDir, ...scope.lockfile.split('/')),
      `${scope.lockfile} must be a regular, non-symbolic-link lockfile`,
    );

    const result = spawn(scope.command, [...scope.args], {
      cwd: path.resolve(rootDir, scope.cwd),
      shell: false,
      windowsHide: true,
      stdio: ['ignore', 'pipe', 'pipe'],
      timeout: EXECUTION_LIMITS.timeoutMs,
      maxBuffer: EXECUTION_LIMITS.reportBytes,
    });

    if (result.error || result.signal || ![0, 1].includes(result.status)) {
      throw new SecurityAuditError(
        `${scope.scanner} could not complete for ${scope.lockfile}; scanner output was withheld`,
      );
    }
    if (!Buffer.isBuffer(result.stdout) && typeof result.stdout !== 'string') {
      throw new SecurityAuditError(
        `${scope.scanner} returned no usable report for ${scope.lockfile}`,
      );
    }
    if (Buffer.byteLength(result.stdout) > EXECUTION_LIMITS.reportBytes) {
      throw new SecurityAuditError(
        `${scope.scanner} report exceeded the size limit for ${scope.lockfile}`,
      );
    }
    reports[scope.id] = result.stdout;
  }

  return reports;
}

export function parseAdvisoryExceptions(input, { now = new Date() } = {}) {
  const policy = parseBoundedJson(
    input,
    'advisory exception policy',
    EXECUTION_LIMITS.exceptionBytes,
  );
  requireRecord(policy, 'advisory exception policy');
  requireExactKeys(policy, new Set(['version', 'exceptions']), 'advisory exception policy');

  if (policy.version !== 1) {
    throw new SecurityAuditError('advisory exception policy version must be 1');
  }
  if (!Array.isArray(policy.exceptions)) {
    throw new SecurityAuditError('advisory exception policy exceptions must be an array');
  }
  if (policy.exceptions.length > EXECUTION_LIMITS.exceptions) {
    throw new SecurityAuditError('advisory exception policy contains too many exceptions');
  }

  const today = utcDay(now);
  const seen = new Set();

  return policy.exceptions.map((entry, offset) => {
    const index = offset + 1;
    requireRecord(entry, `exception ${index}`);
    requireExactKeys(entry, EXCEPTION_FIELDS, `exception ${index}`);

    requireString(entry.advisory, `exception ${index} advisory`);
    requireString(entry.scanner, `exception ${index} scanner`);
    requireString(entry.lockfile, `exception ${index} lockfile`);
    requireString(entry.package, `exception ${index} package`);
    requireString(entry.rationale, `exception ${index} rationale`);
    requireString(entry.owner, `exception ${index} owner`);
    requireString(entry.expires, `exception ${index} expires`);

    if (!SCOPE_PAIRS.has(`${entry.scanner}\0${entry.lockfile}`)) {
      throw new SecurityAuditError(`exception ${index} has an unsupported scanner or lockfile`);
    }

    const advisory = normalizeAdvisory(entry.advisory, entry.scanner);
    const packageName = normalizePackage(entry.package, `exception ${index} package`);

    if (
      entry.rationale !== entry.rationale.trim() ||
      entry.rationale.length < 20 ||
      entry.rationale.length > 500 ||
      hasControlCharacters(entry.rationale)
    ) {
      throw new SecurityAuditError(
        `exception ${index} rationale must be 20-500 trimmed printable characters`,
      );
    }
    if (!OWNER_PATTERN.test(entry.owner)) {
      throw new SecurityAuditError(`exception ${index} owner must be a GitHub @owner`);
    }
    if (!validTrackingIssue(entry.trackingIssue)) {
      throw new SecurityAuditError(
        `exception ${index} trackingIssue must be an issue number or GitHub issue URL`,
      );
    }

    const expires = parseUtcDate(entry.expires, `exception ${index} expires`);
    const remainingDays = (expires - today) / DAY_MS;
    if (remainingDays < 0) {
      throw new SecurityAuditError(`exception ${index} is expired`);
    }
    if (remainingDays > 30) {
      throw new SecurityAuditError(`exception ${index} expires more than 30 days from today`);
    }

    const normalized = {
      advisory,
      scanner: entry.scanner,
      lockfile: entry.lockfile,
      package: packageName,
      rationale: entry.rationale,
      owner: entry.owner,
      trackingIssue: entry.trackingIssue,
      expires: entry.expires,
    };
    const key = findingKey(normalized);
    if (seen.has(key)) {
      throw new SecurityAuditError(`exception ${index} duplicates an earlier exception`);
    }
    seen.add(key);
    return normalized;
  });
}

export function evaluateSecurityAudit({
  reports,
  exceptionsText,
  now = new Date(),
} = {}) {
  requireRecord(reports, 'injected audit reports');
  requireExactKeys(reports, SCOPE_IDS, 'injected audit reports');

  const exceptions = parseAdvisoryExceptions(exceptionsText, { now });
  const findings = [];
  const nonAdvisoryFindings = [];
  const summaries = [];

  for (const scope of AUDIT_SCOPES) {
    const report = parseBoundedJson(
      reports[scope.id],
      `${scope.scanner} report for ${scope.lockfile}`,
      EXECUTION_LIMITS.reportBytes,
    );
    const parsed =
      scope.scanner === 'cargo-audit'
        ? parseCargoReport(report, scope)
        : parseNpmReport(report, scope);
    findings.push(...parsed.findings);
    nonAdvisoryFindings.push(...parsed.nonAdvisoryFindings);
    summaries.push({
      scanner: scope.scanner,
      lockfile: scope.lockfile,
      findings: parsed.findings.length + parsed.nonAdvisoryFindings.length,
    });
  }

  if (findings.length + nonAdvisoryFindings.length > EXECUTION_LIMITS.findings) {
    throw new SecurityAuditError('audit reports contain too many actionable findings');
  }

  const exceptionByKey = new Map(
    exceptions.map((exception) => [findingKey(exception), exception]),
  );
  const used = new Set();
  const problems = [];

  for (const finding of findings) {
    const key = findingKey(finding);
    if (exceptionByKey.has(key)) {
      used.add(key);
    } else {
      problems.push(
        `unreviewed finding: ${finding.scanner} ${finding.lockfile} ${finding.advisory} ${finding.package}`,
      );
    }
  }
  for (const finding of nonAdvisoryFindings) {
    problems.push(
      `unreviewed finding without an advisory ID: ${finding.scanner} ${finding.lockfile} ${finding.kind} ${finding.package}`,
    );
  }
  for (const exception of exceptions) {
    const key = findingKey(exception);
    if (!used.has(key)) {
      problems.push(
        `unused exception: ${exception.scanner} ${exception.lockfile} ${exception.advisory} ${exception.package}`,
      );
    }
  }

  if (problems.length > 0) {
    throw new SecurityAuditError(formatProblems(problems));
  }

  return {
    findings: findings.length + nonAdvisoryFindings.length,
    exceptionsUsed: used.size,
    summaries,
  };
}

export function runSecurityAudit({ rootDir = root, now = new Date(), spawn = spawnSync } = {}) {
  const exceptionFile = path.join(rootDir, ...EXCEPTION_PATH.split('/'));
  const exceptionsText = readBoundedRegularFile(
    exceptionFile,
    EXECUTION_LIMITS.exceptionBytes,
    'advisory exception policy must be a bounded regular file',
  );

  // Validate policy before scanners make network requests.
  parseAdvisoryExceptions(exceptionsText, { now });
  const reports = collectAuditReports({ rootDir, spawn });
  return evaluateSecurityAudit({ reports, exceptionsText, now });
}

function parseCargoReport(report, scope) {
  requireRecord(report, `${scope.scanner} report`);
  requireRecord(report.vulnerabilities, `${scope.scanner} vulnerabilities`);

  const vulnerabilities = report.vulnerabilities;
  if (
    typeof vulnerabilities.found !== 'boolean' ||
    !Number.isSafeInteger(vulnerabilities.count) ||
    vulnerabilities.count < 0 ||
    !Array.isArray(vulnerabilities.list) ||
    vulnerabilities.count !== vulnerabilities.list.length ||
    vulnerabilities.found !== (vulnerabilities.count > 0)
  ) {
    throw new SecurityAuditError(`${scope.scanner} returned an inconsistent vulnerability report`);
  }

  const findings = vulnerabilities.list.map((item) =>
    cargoFinding(item, scope, 'vulnerability'),
  );
  const nonAdvisoryFindings = [];

  requireRecord(report.warnings, `${scope.scanner} warnings`);
  for (const [kind, warnings] of Object.entries(report.warnings)) {
    if (!WARNING_KINDS.has(kind) || !Array.isArray(warnings)) {
      throw new SecurityAuditError(`${scope.scanner} returned an unknown warning category`);
    }
    for (const warning of warnings) {
      requireRecord(warning, `${scope.scanner} warning`);
      const packageName = packageFromCargoItem(warning, `${scope.scanner} warning`);
      if (isRecord(warning.advisory) && typeof warning.advisory.id === 'string') {
        findings.push(cargoFinding(warning, scope, kind));
      } else {
        nonAdvisoryFindings.push({
          scanner: scope.scanner,
          lockfile: scope.lockfile,
          kind,
          package: packageName,
        });
      }
    }
  }

  return { findings: deduplicateFindings(findings), nonAdvisoryFindings };
}

function cargoFinding(item, scope, label) {
  requireRecord(item, `${scope.scanner} ${label}`);
  requireRecord(item.advisory, `${scope.scanner} ${label} advisory`);
  requireString(item.advisory.id, `${scope.scanner} ${label} advisory ID`);
  return {
    scanner: scope.scanner,
    lockfile: scope.lockfile,
    advisory: normalizeAdvisory(item.advisory.id, scope.scanner),
    package: packageFromCargoItem(item, `${scope.scanner} ${label}`),
  };
}

function packageFromCargoItem(item, label) {
  requireRecord(item.package, `${label} package`);
  requireString(item.package.name, `${label} package name`);
  return normalizePackage(item.package.name, `${label} package name`);
}

function parseNpmReport(report, scope) {
  requireRecord(report, `${scope.scanner} report`);
  if (report.auditReportVersion !== 2) {
    throw new SecurityAuditError(`${scope.scanner} report version must be 2`);
  }
  requireRecord(report.vulnerabilities, `${scope.scanner} vulnerabilities`);
  requireRecord(report.metadata, `${scope.scanner} metadata`);
  requireRecord(report.metadata.vulnerabilities, `${scope.scanner} vulnerability metadata`);

  const entries = Object.entries(report.vulnerabilities);
  const total = report.metadata.vulnerabilities.total;
  if (!Number.isSafeInteger(total) || total < 0 || total !== entries.length) {
    throw new SecurityAuditError(`${scope.scanner} returned inconsistent vulnerability totals`);
  }

  const findings = [];
  const references = new Map();
  const entriesWithAdvisories = new Set();

  for (const [name, vulnerability] of entries) {
    const packageName = normalizePackage(name, `${scope.scanner} vulnerability package`);
    requireRecord(vulnerability, `${scope.scanner} vulnerability`);
    if (vulnerability.name !== packageName || !Array.isArray(vulnerability.via)) {
      throw new SecurityAuditError(`${scope.scanner} returned a malformed vulnerability entry`);
    }

    const links = [];
    let hasAdvisory = false;
    for (const via of vulnerability.via) {
      if (typeof via === 'string') {
        links.push(normalizePackage(via, `${scope.scanner} vulnerability reference`));
        continue;
      }
      requireRecord(via, `${scope.scanner} advisory`);
      const advisory = npmAdvisoryId(via);
      const affectedPackage = normalizePackage(
        typeof via.dependency === 'string'
          ? via.dependency
          : typeof via.name === 'string'
            ? via.name
            : packageName,
        `${scope.scanner} advisory package`,
      );
      findings.push({
        scanner: scope.scanner,
        lockfile: scope.lockfile,
        advisory,
        package: affectedPackage,
      });
      hasAdvisory = true;
    }
    if (hasAdvisory) entriesWithAdvisories.add(packageName);
    references.set(packageName, links);
  }

  const reverseReferences = new Map(
    [...references.keys()].map((name) => [name, []]),
  );
  for (const [name, links] of references) {
    for (const link of links) {
      if (!references.has(link)) {
        throw new SecurityAuditError(`${scope.scanner} returned an unknown vulnerability reference`);
      }
      reverseReferences.get(link).push(name);
    }
  }

  const reachesAdvisory = new Set(entriesWithAdvisories);
  const queue = [...entriesWithAdvisories];
  for (let index = 0; index < queue.length; index += 1) {
    for (const dependent of reverseReferences.get(queue[index])) {
      if (!reachesAdvisory.has(dependent)) {
        reachesAdvisory.add(dependent);
        queue.push(dependent);
      }
    }
  }
  for (const name of references.keys()) {
    if (!reachesAdvisory.has(name)) {
      throw new SecurityAuditError(
        `${scope.scanner} returned a vulnerability without an advisory identifier`,
      );
    }
  }

  return { findings: deduplicateFindings(findings), nonAdvisoryFindings: [] };
}

function npmAdvisoryId(via) {
  for (const candidate of [via.id, via.url]) {
    if (typeof candidate !== 'string') continue;
    const match = candidate
      .toUpperCase()
      .match(/(?:GHSA-[23456789CFGHJMPQRVWX]{4}-[23456789CFGHJMPQRVWX]{4}-[23456789CFGHJMPQRVWX]{4}|CVE-\d{4}-\d{4,}|NPM-\d+)/);
    if (match) return normalizeAdvisory(match[0], 'npm-audit');
  }
  if (Number.isSafeInteger(via.source) && via.source > 0) {
    return `NPM-${via.source}`;
  }
  throw new SecurityAuditError('npm-audit returned an advisory without a usable identifier');
}

function normalizeAdvisory(value, scanner) {
  const advisory = value.toUpperCase();
  const valid =
    scanner === 'cargo-audit'
      ? RUSTSEC_PATTERN.test(advisory)
      : scanner === 'npm-audit' &&
        (GHSA_PATTERN.test(advisory) || CVE_PATTERN.test(advisory) || NPM_PATTERN.test(advisory));
  if (!valid) {
    throw new SecurityAuditError(`${scanner} advisory identifier is malformed`);
  }
  return advisory;
}

function normalizePackage(value, label) {
  requireString(value, label);
  if (value.length > 214 || !PACKAGE_PATTERN.test(value)) {
    throw new SecurityAuditError(`${label} is malformed`);
  }
  return value;
}

function deduplicateFindings(findings) {
  return [...new Map(findings.map((finding) => [findingKey(finding), finding])).values()];
}

function findingKey(value) {
  return `${value.scanner}\0${value.lockfile}\0${value.advisory}\0${value.package}`;
}

function validTrackingIssue(value) {
  return (
    (Number.isSafeInteger(value) && value > 0) ||
    (typeof value === 'string' &&
      (/^#[1-9]\d*$/.test(value) || ISSUE_URL_PATTERN.test(value)))
  );
}

function parseUtcDate(value, label) {
  const match = /^(\d{4})-(\d{2})-(\d{2})$/.exec(value);
  if (!match) throw new SecurityAuditError(`${label} must use YYYY-MM-DD`);
  const year = Number(match[1]);
  const month = Number(match[2]);
  const day = Number(match[3]);
  const parsed = Date.UTC(year, month - 1, day);
  const date = new Date(parsed);
  if (
    date.getUTCFullYear() !== year ||
    date.getUTCMonth() !== month - 1 ||
    date.getUTCDate() !== day
  ) {
    throw new SecurityAuditError(`${label} is not a valid calendar date`);
  }
  return parsed;
}

function utcDay(now) {
  const parsed = now instanceof Date ? now : new Date(now);
  if (Number.isNaN(parsed.getTime())) {
    throw new SecurityAuditError('current audit date is invalid');
  }
  return Date.UTC(parsed.getUTCFullYear(), parsed.getUTCMonth(), parsed.getUTCDate());
}

function parseBoundedJson(input, label, maximumBytes) {
  let buffer;
  if (Buffer.isBuffer(input)) {
    buffer = input;
  } else if (typeof input === 'string') {
    buffer = Buffer.from(input, 'utf8');
  } else {
    throw new SecurityAuditError(`${label} must be UTF-8 JSON`);
  }
  if (buffer.length === 0 || buffer.length > maximumBytes) {
    throw new SecurityAuditError(`${label} is empty or exceeds the size limit`);
  }

  let text;
  try {
    text = new TextDecoder('utf-8', { fatal: true }).decode(buffer);
  } catch {
    throw new SecurityAuditError(`${label} is not valid UTF-8`);
  }

  let value;
  try {
    value = JSON.parse(text);
  } catch {
    throw new SecurityAuditError(`${label} is malformed JSON`);
  }
  assertBoundedJsonTree(value, label);
  return value;
}

function assertBoundedJsonTree(rootValue, label) {
  const stack = [{ value: rootValue, depth: 0 }];
  let nodes = 0;
  while (stack.length > 0) {
    const { value, depth } = stack.pop();
    nodes += 1;
    if (nodes > EXECUTION_LIMITS.jsonNodes) {
      throw new SecurityAuditError(`${label} exceeds the JSON node limit`);
    }
    if (value !== null && typeof value === 'object') {
      if (depth >= EXECUTION_LIMITS.jsonDepth) {
        throw new SecurityAuditError(`${label} exceeds the JSON depth limit`);
      }
      for (const child of Array.isArray(value) ? value : Object.values(value)) {
        stack.push({ value: child, depth: depth + 1 });
      }
    }
  }
}

function requireRecord(value, label) {
  if (!isRecord(value)) throw new SecurityAuditError(`${label} must be an object`);
}

function isRecord(value) {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

function requireExactKeys(value, expected, label) {
  const keys = Object.keys(value);
  if (keys.length !== expected.size || keys.some((key) => !expected.has(key))) {
    throw new SecurityAuditError(`${label} contains missing or unknown fields`);
  }
}

function requireString(value, label) {
  if (typeof value !== 'string' || value.length === 0) {
    throw new SecurityAuditError(`${label} must be a non-empty string`);
  }
}

function hasControlCharacters(value) {
  return /[\u0000-\u001F\u007F]/.test(value);
}

function assertRegularFile(file, message) {
  let metadata;
  try {
    metadata = lstatSync(file);
  } catch {
    throw new SecurityAuditError(message);
  }
  if (metadata.isSymbolicLink() || !metadata.isFile()) {
    throw new SecurityAuditError(message);
  }
  return metadata;
}

function readBoundedRegularFile(file, maximumBytes, message) {
  const metadata = assertRegularFile(file, message);
  if (metadata.size === 0 || metadata.size > maximumBytes) {
    throw new SecurityAuditError(message);
  }
  const input = readFileSync(file);
  if (input.length === 0 || input.length > maximumBytes) {
    throw new SecurityAuditError(message);
  }
  return input;
}

function formatProblems(problems) {
  const visible = problems.slice(0, 20);
  const remainder = problems.length - visible.length;
  return [
    `security advisory policy rejected ${problems.length} item(s):`,
    ...visible.map((problem) => `- ${problem}`),
    ...(remainder > 0 ? [`- ${remainder} additional item(s) withheld`] : []),
  ].join('\n');
}

function main() {
  try {
    const result = runSecurityAudit();
    for (const summary of result.summaries) {
      process.stdout.write(
        `[security-audit] ${summary.scanner} ${summary.lockfile}: ${summary.findings} actionable finding(s)\n`,
      );
    }
    process.stdout.write(
      `[security-audit] passed: ${result.findings} finding(s), ${result.exceptionsUsed} reviewed exception(s)\n`,
    );
  } catch (error) {
    const message =
      error instanceof SecurityAuditError
        ? error.message
        : 'unexpected internal error; details were withheld';
    process.stderr.write(`[security-audit] failed: ${message}\n`);
    process.exitCode = 1;
  }
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main();
}
