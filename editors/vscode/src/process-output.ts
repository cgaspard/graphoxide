export const STDERR_CAPTURE_LIMIT = 64 * 1024;
export const ERROR_DIAGNOSTIC_LIMIT = 2048;
export const AUTOMATIC_UPDATES_PAUSED = 'Automatic graph updates are paused for this workspace until you retry Build, Update, or Full Rebuild.';

export function compactCommandDiagnostic(stderr: string, stdout: string, exitCode: number): string {
  const source = stderr.trim() || stdout.trim();
  if (!source) return `Graphoxide exited with code ${exitCode}.`;
  const lines = source.split(/\r?\n/u).map((line) => line.trim()).filter(Boolean);
  const diagnostic = [...lines].reverse().find((line) => /^error(?:\b|:)/iu.test(line))
    ?? lines.at(-1)
    ?? `Graphoxide exited with code ${exitCode}.`;
  return compactError(diagnostic);
}

export function compactError(error: unknown): string {
  return truncateDiagnostic(error instanceof Error ? error.message : String(error), ERROR_DIAGNOSTIC_LIMIT);
}

export function compactGuidedError(error: unknown, guidance?: string): string {
  const detail = compactError(error);
  if (!guidance) return detail;
  const compactGuidance = compactError(guidance);
  const suffix = ` ${compactGuidance}`;
  if (suffix.length >= ERROR_DIAGNOSTIC_LIMIT) return compactGuidance;
  return `${truncateDiagnostic(detail, ERROR_DIAGNOSTIC_LIMIT - suffix.length)}${suffix}`;
}

function truncateDiagnostic(value: string, limit: number): string {
  const normalized = value.trim().replace(/\s+/gu, ' ');
  if (normalized.length <= limit) return normalized;
  return `${normalized.slice(0, limit - 1)}…`;
}

export class BoundedTextTail {
  private text = '';
  private truncated = false;

  constructor(private readonly limit: number) {
    if (!Number.isSafeInteger(limit) || limit < 2) throw new Error('Bounded text tail limit must be at least 2.');
  }

  append(value: string): void {
    this.text += value;
    if (this.text.length <= this.limit) return;
    this.text = this.text.slice(-this.limit);
    this.truncated = true;
  }

  value(): string {
    if (!this.truncated) return this.text;
    return `…${this.text.slice(1)}`;
  }
}
