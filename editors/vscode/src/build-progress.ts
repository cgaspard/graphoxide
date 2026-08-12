import { createHash, randomBytes } from 'node:crypto';
import * as path from 'node:path';
import { StringDecoder } from 'node:string_decoder';

export const BUILD_PROGRESS_NONCE_ENV = 'GRAPHOXIDE_PROGRESS_NONCE';
export const BUILD_PROGRESS_PREFIX = '[graphoxide-progress] ';
export const BUILD_PROGRESS_SCHEMA_VERSION = 1;
export const BUILD_PROGRESS_MAX_LINE = 4096;
export const BUILD_PROGRESS_NONCE_HEX_LENGTH = 32;
const BUILD_PROGRESS_MAX_PENDING = BUILD_PROGRESS_MAX_LINE * 2;
const BUILD_SUMMARY_KEY = 'graphoxide.latestBuildSummary.v1';
const BUILD_SUMMARY_RECORD_LIMIT = 8;

export type BuildOperation = 'extract' | 'index' | 'update';
export type BuildMode = 'full' | 'incremental';
export type BuildProgressRunMode = BuildMode | 'adaptive';
export type BuildSuccessStatus = 'rebuilt' | 'unchanged' | 'no_tracked_changes';
export type BuildProgressPhase =
  | 'waiting'
  | 'auditing'
  | 'scanning'
  | 'extracting'
  | 'building'
  | 'clustering'
  | 'publishing';

export interface BuildStartedEvent {
  readonly schema_version: 1;
  readonly run_nonce: string;
  readonly type: 'started';
  readonly operation: BuildOperation;
  readonly mode: BuildProgressRunMode;
}

export interface BuildPhaseEvent {
  readonly schema_version: 1;
  readonly run_nonce: string;
  readonly type: 'phase';
  readonly operation: BuildOperation;
  readonly phase: BuildProgressPhase;
  readonly processed?: number;
  readonly total?: number;
}

export interface BuildCompletedEvent {
  readonly schema_version: 1;
  readonly run_nonce: string;
  readonly type: 'completed';
  readonly operation: BuildOperation;
  readonly mode: BuildMode;
  readonly status: BuildSuccessStatus;
  readonly elapsed_ms: number;
  readonly stages_ms: BuildStageDurations;
  readonly files: BuildFileStats;
  readonly source_bytes?: number;
}

export interface BuildFailedEvent {
  readonly schema_version: 1;
  readonly run_nonce: string;
  readonly type: 'failed';
  readonly operation: BuildOperation;
  readonly mode: BuildProgressRunMode;
}

export interface BuildNotCompletedEvent {
  readonly schema_version: 1;
  readonly run_nonce: string;
  readonly type: 'not_completed';
  readonly operation: BuildOperation;
  readonly mode: BuildMode;
  readonly reason: 'queued' | 'refused_shrink';
}

export type BuildProgressEvent =
  | BuildStartedEvent
  | BuildPhaseEvent
  | BuildCompletedEvent
  | BuildFailedEvent
  | BuildNotCompletedEvent;

export interface BuildStageDurations {
  readonly scan_extract: number;
  readonly detect: number;
  readonly extract: number;
  readonly build: number;
  readonly cluster: number;
  readonly write: number;
}

export interface BuildFileStats {
  readonly indexed: number;
  readonly changed: number;
  readonly deleted: number;
}

export interface LatestBuildSummary {
  readonly operation: BuildOperation;
  readonly mode: BuildMode;
  readonly status: BuildSuccessStatus;
  readonly elapsedMs: number;
  readonly stagesMs: BuildStageDurations;
  readonly files: BuildFileStats;
  readonly sourceBytes?: number;
  readonly completedAt: number;
}

interface PersistedBuildSummary extends LatestBuildSummary {
  readonly targetFingerprint: string;
  readonly graphMtime: number;
  readonly graphSize: number;
}

interface StateStore {
  get<T>(key: string): T | undefined;
  update(key: string, value: unknown): PromiseLike<void>;
}

export interface GraphFileIdentity {
  readonly mtime: number;
  readonly size: number;
}

/** One ordered stderr segment. Only an admitted event may swallow `raw`. */
export interface BuildProgressFrame {
  readonly raw: string;
  readonly event?: BuildProgressEvent;
}

export interface DecodedBuildProgress {
  readonly frames: readonly BuildProgressFrame[];
}

/** Incrementally frames authenticated protocol lines and ordinary stderr. */
export class BuildProgressDecoder {
  private readonly textDecoder = new StringDecoder('utf8');
  private pending = '';
  private passthroughUntilNewline = false;

  constructor(private readonly expectedNonce: string) {
    if (!validRunNonce(expectedNonce)) {
      throw new Error('Build progress nonce must be 32 lowercase hexadecimal characters.');
    }
  }

  push(value: Buffer | string): DecodedBuildProgress {
    const text = this.textDecoder.write(typeof value === 'string' ? Buffer.from(value, 'utf8') : value);
    return { frames: this.consume(text, false) };
  }

  finish(): DecodedBuildProgress {
    const frames = this.consume(this.textDecoder.end(), false);
    frames.push(...this.consume('', true));
    return { frames };
  }

  private consume(value: string, finish: boolean): BuildProgressFrame[] {
    const frames: BuildProgressFrame[] = [];
    if (this.passthroughUntilNewline) {
      const newline = value.indexOf('\n');
      if (newline < 0) {
        if (value) frames.push({ raw: value });
        if (finish) this.passthroughUntilNewline = false;
        return frames;
      }
      frames.push({ raw: value.slice(0, newline + 1) });
      value = value.slice(newline + 1);
      this.passthroughUntilNewline = false;
    }

    this.pending += value;
    for (;;) {
      const newline = this.pending.indexOf('\n');
      if (newline < 0) break;
      const raw = this.pending.slice(0, newline + 1);
      this.pending = this.pending.slice(newline + 1);
      const event = parseBuildProgressLine(raw, this.expectedNonce);
      frames.push(event ? { raw, event } : { raw });
    }
    if (Buffer.byteLength(this.pending, 'utf8') > BUILD_PROGRESS_MAX_PENDING) {
      frames.push({ raw: this.pending });
      this.pending = '';
      this.passthroughUntilNewline = true;
    }
    if (finish && this.pending) {
      const raw = this.pending;
      this.pending = '';
      // JSONL producers always terminate envelopes. A partial final record is
      // an ordinary truncated diagnostic, never an admitted terminal.
      frames.push({ raw });
    }
    return frames;
  }
}

/** Validates one strict started -> monotonic phases -> terminal lifecycle. */
export class BuildProgressRun {
  private state: 'idle' | 'running' | 'terminal' = 'idle';
  private operation?: BuildOperation;
  private mode?: BuildProgressRunMode;
  private runNonce?: string;
  private lastPhase?: BuildProgressPhase;
  private lastProcessed?: number;
  private lastTotal?: number;
  private completion?: BuildCompletedEvent;

  constructor(private readonly expectedOperation?: BuildOperation) {}

  accept(event: BuildProgressEvent): boolean {
    if (this.state === 'idle') {
      if (event.type !== 'started'
        || (this.expectedOperation !== undefined && event.operation !== this.expectedOperation)) return false;
      this.state = 'running';
      this.operation = event.operation;
      this.mode = event.mode;
      this.runNonce = event.run_nonce;
      return true;
    }
    if (this.state !== 'running'
      || event.run_nonce !== this.runNonce
      || event.operation !== this.operation
      || event.type === 'started') return false;

    if (event.type === 'phase') return this.acceptPhase(event);
    if (event.type === 'failed') {
      if (event.mode !== this.mode) return false;
    } else if (this.mode !== 'adaptive' && event.mode !== this.mode) {
      return false;
    }
    this.state = 'terminal';
    this.completion = event.type === 'completed' ? event : undefined;
    return true;
  }

  successfulCompletion(exitCode: number, cancelled: boolean): BuildCompletedEvent | undefined {
    return !cancelled && exitCode === 0 ? this.completion : undefined;
  }

  private acceptPhase(event: BuildPhaseEvent): boolean {
    const previousRank = this.lastPhase === undefined ? -1 : phaseRank(this.lastPhase);
    const nextRank = phaseRank(event.phase);
    if (nextRank < previousRank) return false;
    const hasProcessed = event.processed !== undefined;
    const hasTotal = event.total !== undefined;
    if (hasProcessed !== hasTotal) return false;

    if (nextRank === previousRank) {
      if (!hasProcessed || event.total !== this.lastTotal || event.processed! < (this.lastProcessed ?? 0)) {
        return false;
      }
    } else {
      this.lastProcessed = undefined;
      this.lastTotal = undefined;
    }
    if (hasProcessed) {
      if (event.processed! > event.total!) return false;
      this.lastProcessed = event.processed;
      this.lastTotal = event.total;
    }
    this.lastPhase = event.phase;
    return true;
  }
}

/** Stores only numeric/enumerated data under a digest of the output target. */
export class LatestBuildSummaryStore {
  private writeTail = Promise.resolve();
  private readonly requestedGeneration = new Map<string, number>();
  private nextGeneration = 0;

  constructor(private readonly state: StateStore) {}

  async record(
    outputTarget: string,
    event: BuildCompletedEvent,
    graph: GraphFileIdentity,
    completedAt = Date.now(),
  ): Promise<void> {
    await this.recordWithIdentity(outputTarget, event, async () => graph, completedAt);
  }

  recordWithIdentity(
    outputTarget: string,
    event: BuildCompletedEvent,
    identity: () => Promise<GraphFileIdentity>,
    completedAt = Date.now(),
  ): Promise<boolean> {
    const targetFingerprint = outputTargetFingerprint(outputTarget);
    const generation = this.advanceGeneration(targetFingerprint);
    const work = this.writeTail.then(async () => {
      if (!this.isCurrent(targetFingerprint, generation) || !validCount(completedAt)) return false;
      const graph = await identity();
      if (!this.isCurrent(targetFingerprint, generation) || !validFileIdentity(graph)) return false;
      const previous = [...this.records()];
      const record = persistedRecord(targetFingerprint, event, graph, completedAt);
      const next = [record, ...previous.filter((item) => item.targetFingerprint !== targetFingerprint)]
        .slice(0, BUILD_SUMMARY_RECORD_LIMIT);
      await this.state.update(BUILD_SUMMARY_KEY, next);
      if (this.isCurrent(targetFingerprint, generation)) return true;
      // invalidatePending() may race an already-started Memento update. Restore
      // the last admitted snapshot before a newer serialized generation runs.
      await this.state.update(BUILD_SUMMARY_KEY, previous);
      return false;
    });
    this.writeTail = work.then(() => undefined, () => undefined);
    return work;
  }

  /** Cancels an in-flight graph association while retaining stored success. */
  invalidatePending(outputTarget: string): void {
    this.advanceGeneration(outputTargetFingerprint(outputTarget));
  }

  async latestWithIdentity(
    outputTarget: string,
    identity: () => Promise<GraphFileIdentity>,
  ): Promise<LatestBuildSummary | undefined> {
    const fingerprint = outputTargetFingerprint(outputTarget);
    if (!this.records().some((item) => item.targetFingerprint === fingerprint)) return undefined;
    return this.latest(outputTarget, await identity());
  }

  latest(outputTarget: string, graph: GraphFileIdentity): LatestBuildSummary | undefined {
    if (!validFileIdentity(graph)) return undefined;
    const fingerprint = outputTargetFingerprint(outputTarget);
    const record = this.records().find((item) => item.targetFingerprint === fingerprint);
    if (!record || record.graphMtime !== graph.mtime || record.graphSize !== graph.size) return undefined;
    return {
      operation: record.operation,
      mode: record.mode,
      status: record.status,
      elapsedMs: record.elapsedMs,
      stagesMs: record.stagesMs,
      files: record.files,
      ...(record.sourceBytes === undefined ? {} : { sourceBytes: record.sourceBytes }),
      completedAt: record.completedAt,
    };
  }

  private advanceGeneration(targetFingerprint: string): number {
    const generation = ++this.nextGeneration;
    this.requestedGeneration.delete(targetFingerprint);
    if (this.requestedGeneration.size >= BUILD_SUMMARY_RECORD_LIMIT) {
      const oldest = this.requestedGeneration.keys().next().value as string | undefined;
      if (oldest) this.requestedGeneration.delete(oldest);
    }
    this.requestedGeneration.set(targetFingerprint, generation);
    return generation;
  }

  private isCurrent(targetFingerprint: string, generation: number): boolean {
    return this.requestedGeneration.get(targetFingerprint) === generation;
  }

  private records(): readonly PersistedBuildSummary[] {
    const value = this.state.get<unknown>(BUILD_SUMMARY_KEY);
    if (!Array.isArray(value)) return [];
    return value.slice(0, BUILD_SUMMARY_RECORD_LIMIT).filter(isPersistedBuildSummary);
  }
}

export function graphFileForOutputTarget(outputTarget: string): string {
  return path.join(outputTarget, 'graph.json');
}

/** Generation checks shared by finite close and long-lived watch ingestion. */
export function ownsBuildProgressGeneration(
  activeGeneration: number | undefined,
  candidateGeneration: number,
): boolean {
  return activeGeneration === candidateGeneration;
}

/** A non-source-derived authenticator for one child-process progress stream. */
export function createBuildProgressNonce(): string {
  return randomBytes(BUILD_PROGRESS_NONCE_HEX_LENGTH / 2).toString('hex');
}

export function phaseProgressMessage(event: BuildPhaseEvent): string {
  const label = (() => {
    switch (event.phase) {
      case 'waiting': return 'Waiting for build lock';
      case 'auditing': return 'Auditing index coverage';
      case 'scanning': return 'Scanning inputs';
      case 'extracting': return 'Extracting inputs';
      case 'building': return 'Building graph';
      case 'clustering': return 'Clustering communities';
      case 'publishing': return 'Publishing graph';
    }
  })();
  return event.processed === undefined ? `${label}…` : `${label}… (${event.processed}/${event.total})`;
}

function parseBuildProgressLine(raw: string, expectedNonce: string): BuildProgressEvent | undefined {
  // Bound the complete JSONL record, including its LF or CRLF terminator. This
  // keeps the wire limit independent of how the line happened to be chunked.
  if (Buffer.byteLength(raw, 'utf8') > BUILD_PROGRESS_MAX_LINE) return undefined;
  const line = raw.endsWith('\n') ? raw.slice(0, -1).replace(/\r$/u, '') : raw;
  if (!line.startsWith(BUILD_PROGRESS_PREFIX)) return undefined;
  const payload = line.slice(BUILD_PROGRESS_PREFIX.length);
  if (!payload) return undefined;
  try {
    return validateBuildProgressEvent(JSON.parse(payload), expectedNonce);
  } catch {
    return undefined;
  }
}

function validateBuildProgressEvent(value: unknown, expectedNonce: string): BuildProgressEvent | undefined {
  if (!record(value) || value.schema_version !== BUILD_PROGRESS_SCHEMA_VERSION
    || value.run_nonce !== expectedNonce || !validRunNonce(value.run_nonce)
    || !oneOf(value.operation, ['extract', 'index', 'update'])
    || typeof value.type !== 'string') return undefined;
  const base = { schema_version: 1 as const, run_nonce: value.run_nonce, operation: value.operation };
  if (value.type === 'started' && oneOf(value.mode, ['full', 'incremental', 'adaptive'])
    && exactKeys(value, ['schema_version', 'run_nonce', 'type', 'operation', 'mode'])) {
    return { ...base, type: 'started', mode: value.mode };
  }
  if (value.type === 'failed' && oneOf(value.mode, ['full', 'incremental', 'adaptive'])
    && exactKeys(value, ['schema_version', 'run_nonce', 'type', 'operation', 'mode'])) {
    return { ...base, type: 'failed', mode: value.mode };
  }
  if (value.type === 'phase' && oneOf(value.phase, PHASES)) {
    const withoutCounters = exactKeys(value, ['schema_version', 'run_nonce', 'type', 'operation', 'phase']);
    const withCounters = exactKeys(
      value,
      ['schema_version', 'run_nonce', 'type', 'operation', 'phase', 'processed', 'total'],
    ) && validCount(value.processed) && validCount(value.total) && value.processed <= value.total;
    if (withoutCounters) return { ...base, type: 'phase', phase: value.phase };
    if (withCounters) {
      return {
        ...base,
        type: 'phase',
        phase: value.phase,
        processed: value.processed as number,
        total: value.total as number,
      };
    }
    return undefined;
  }
  if (!oneOf(value.mode, ['full', 'incremental'])) return undefined;
  const common = { ...base, mode: value.mode };
  if (value.type === 'not_completed'
    && exactKeys(value, ['schema_version', 'run_nonce', 'type', 'operation', 'mode', 'reason'])
    && oneOf(value.reason, ['queued', 'refused_shrink'])) {
    return { ...common, type: 'not_completed', reason: value.reason };
  }
  if (value.type !== 'completed'
    || !oneOf(value.status, ['rebuilt', 'unchanged', 'no_tracked_changes'])
    || !validCount(value.elapsed_ms) || !isStageDurations(value.stages_ms)
    || !isFileStats(value.files) || (value.source_bytes !== undefined && !validCount(value.source_bytes))) return undefined;
  const keys = value.source_bytes === undefined
    ? ['schema_version', 'run_nonce', 'type', 'operation', 'mode', 'status', 'elapsed_ms', 'stages_ms', 'files']
    : ['schema_version', 'run_nonce', 'type', 'operation', 'mode', 'status', 'elapsed_ms', 'stages_ms', 'files', 'source_bytes'];
  if (!exactKeys(value, keys)) return undefined;
  return {
    ...common,
    type: 'completed',
    status: value.status,
    elapsed_ms: value.elapsed_ms,
    stages_ms: value.stages_ms,
    files: value.files,
    ...(value.source_bytes === undefined ? {} : { source_bytes: value.source_bytes }),
  };
}

const PHASES: readonly BuildProgressPhase[] = [
  'waiting',
  'auditing',
  'scanning',
  'extracting',
  'building',
  'clustering',
  'publishing',
];

function phaseRank(phase: BuildProgressPhase): number {
  return PHASES.indexOf(phase);
}

function isStageDurations(value: unknown): value is BuildStageDurations {
  return record(value)
    && exactKeys(value, ['scan_extract', 'detect', 'extract', 'build', 'cluster', 'write'])
    && ['scan_extract', 'detect', 'extract', 'build', 'cluster', 'write'].every((key) => validCount(value[key]));
}

function isFileStats(value: unknown): value is BuildFileStats {
  return record(value)
    && exactKeys(value, ['indexed', 'changed', 'deleted'])
    && ['indexed', 'changed', 'deleted'].every((key) => validCount(value[key]));
}

function persistedRecord(
  targetFingerprint: string,
  event: BuildCompletedEvent,
  graph: GraphFileIdentity,
  completedAt: number,
): PersistedBuildSummary {
  return {
    targetFingerprint,
    operation: event.operation,
    mode: event.mode,
    status: event.status,
    elapsedMs: event.elapsed_ms,
    stagesMs: event.stages_ms,
    files: event.files,
    ...(event.source_bytes === undefined ? {} : { sourceBytes: event.source_bytes }),
    completedAt,
    graphMtime: graph.mtime,
    graphSize: graph.size,
  };
}

function isPersistedBuildSummary(value: unknown): value is PersistedBuildSummary {
  if (!record(value) || typeof value.targetFingerprint !== 'string' || !/^[a-f0-9]{32}$/u.test(value.targetFingerprint)
    || !oneOf(value.operation, ['extract', 'index', 'update']) || !oneOf(value.mode, ['full', 'incremental'])
    || !oneOf(value.status, ['rebuilt', 'unchanged', 'no_tracked_changes'])
    || !validCount(value.elapsedMs) || !isStageDurations(value.stagesMs) || !isFileStats(value.files)
    || (value.sourceBytes !== undefined && !validCount(value.sourceBytes)) || !validCount(value.completedAt)
    || !validCount(value.graphMtime) || !validCount(value.graphSize)) return false;
  const keys = value.sourceBytes === undefined
    ? ['targetFingerprint', 'operation', 'mode', 'status', 'elapsedMs', 'stagesMs', 'files', 'completedAt', 'graphMtime', 'graphSize']
    : ['targetFingerprint', 'operation', 'mode', 'status', 'elapsedMs', 'stagesMs', 'files', 'sourceBytes', 'completedAt', 'graphMtime', 'graphSize'];
  return exactKeys(value, keys);
}

function outputTargetFingerprint(value: string): string {
  const normalized = path.resolve(value).normalize();
  return createHash('sha256').update(normalized).digest('hex').slice(0, 32);
}

function validFileIdentity(value: GraphFileIdentity): boolean {
  return validCount(value.mtime) && validCount(value.size);
}

function validRunNonce(value: unknown): value is string {
  return typeof value === 'string' && /^[a-f0-9]{32}$/u.test(value);
}

function validCount(value: unknown): value is number {
  return typeof value === 'number' && Number.isSafeInteger(value) && value >= 0;
}

function record(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function exactKeys(value: Record<string, unknown>, expected: readonly string[]): boolean {
  const keys = Object.keys(value);
  return keys.length === expected.length && expected.every((key) => Object.hasOwn(value, key));
}

function oneOf<const T extends string>(value: unknown, allowed: readonly T[]): value is T {
  return typeof value === 'string' && allowed.some((item) => item === value);
}
