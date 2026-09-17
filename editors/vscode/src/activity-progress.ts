import { BUILD_PROGRESS_MAX_LINE, ProgressLineDecoder } from './build-progress';

export const ACTIVITY_PROGRESS_PREFIX = '[graphoxide-activity] ';
export type ActivityOperation = 'label' | 'wiki';
const PHASE_LABELS = {
  waiting: 'Waiting for operation lock',
  preparing: 'Preparing',
  admitting: 'Reading Wiki sources',
  authoring: 'Writing Wiki pages with AI',
  reviewing: 'Reviewing Wiki pages with AI',
  labeling: 'Naming communities with AI',
  retrying: 'Retrying community names with AI',
  publishing: 'Publishing',
  serving: 'Starting Wiki preview',
} as const;
export type ActivityPhase = keyof typeof PHASE_LABELS;

interface ActivityEventBase {
  readonly schema_version: 1;
  readonly run_nonce: string;
  readonly operation: ActivityOperation;
}

export type ActivityProgressEvent = ActivityEventBase & (
  | { readonly type: 'started' | 'completed' | 'failed' }
  | {
    readonly type: 'phase';
    readonly phase: ActivityPhase;
    readonly processed?: number;
    readonly total?: number;
  }
);

/** Uses the same bounded UTF-8 framing as graph progress, with a separate schema. */
export class ActivityProgressDecoder extends ProgressLineDecoder<ActivityProgressEvent> {
  constructor(nonce: string) {
    super(nonce, parseActivityProgressLine);
  }
}

/** Each owned child has one authenticated lifecycle; phase counters are local. */
export class ActivityProgressRun {
  private state: 'idle' | 'running' | 'terminal' = 'idle';
  private nonce?: string;
  private phase?: ActivityPhase;
  private processed?: number;
  private total?: number;

  constructor(private readonly operation: ActivityOperation) {}

  accept(event: ActivityProgressEvent): boolean {
    if (event.operation !== this.operation) return false;
    if (this.state === 'idle') {
      if (event.type !== 'started') return false;
      this.nonce = event.run_nonce;
      this.state = 'running';
      return true;
    }
    if (this.state !== 'running' || event.run_nonce !== this.nonce || event.type === 'started') return false;
    if (event.type !== 'phase') {
      this.state = 'terminal';
      return true;
    }
    if (event.phase === this.phase && this.total !== undefined
      && (event.total !== this.total || event.processed === undefined || event.processed < (this.processed ?? 0))) return false;
    this.phase = event.phase;
    this.processed = event.processed;
    this.total = event.total;
    return true;
  }
}

export function activityProgressMessage(event: ActivityProgressEvent): string {
  if (event.type !== 'phase') return event.operation === 'label' ? 'Preparing AI community naming…' : 'Preparing Wiki…';
  const label = PHASE_LABELS[event.phase];
  return event.processed === undefined ? `${label}…` : `${label}… (${event.processed}/${event.total})`;
}

function parseActivityProgressLine(raw: string, nonce: string): ActivityProgressEvent | undefined {
  if (!raw.endsWith('\n') || Buffer.byteLength(raw, 'utf8') > BUILD_PROGRESS_MAX_LINE
    || !raw.startsWith(ACTIVITY_PROGRESS_PREFIX)) return undefined;
  let value: unknown;
  try { value = JSON.parse(raw.slice(ACTIVITY_PROGRESS_PREFIX.length)); } catch { return undefined; }
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return undefined;
  const record = value as Record<string, unknown>;
  if (record.schema_version !== 1 || record.run_nonce !== nonce
    || (record.operation !== 'label' && record.operation !== 'wiki')) return undefined;
  const keys = ['schema_version', 'run_nonce', 'operation', 'type'];
  if (record.type === 'phase') {
    if (typeof record.phase !== 'string' || !Object.hasOwn(PHASE_LABELS, record.phase)) return undefined;
    keys.push('phase');
    if (Object.hasOwn(record, 'processed') || Object.hasOwn(record, 'total')) {
      if (!validCount(record.processed) || !validCount(record.total) || record.processed > record.total) return undefined;
      keys.push('processed', 'total');
    }
  } else if (record.type !== 'started' && record.type !== 'completed' && record.type !== 'failed') return undefined;
  if (Object.keys(record).length !== keys.length || !keys.every((key) => Object.hasOwn(record, key))) return undefined;
  return record as unknown as ActivityProgressEvent;
}

function validCount(value: unknown): value is number {
  return typeof value === 'number' && Number.isSafeInteger(value) && value >= 0;
}
