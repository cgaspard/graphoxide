import { execFile } from 'node:child_process';
import { promisify } from 'node:util';

const execute = promisify(execFile);
const CHROME_TIMEOUT_MS = 45_000;
const MAX_DIAGNOSTIC_CODE_UNITS = 2_000;

export async function executeBrowserProcess(executable, arguments_, { acceptTimedOutStdout, ...options }) {
  try {
    return await execute(executable, arguments_, {
      ...options,
      timeout: CHROME_TIMEOUT_MS,
      killSignal: 'SIGKILL',
    });
  } catch (error) {
    if (isKilledProcessError(error)) return resolveKilledBrowserProcess(error, acceptTimedOutStdout);
    throw error;
  }
}

export function resolveKilledBrowserProcess(error, acceptTimedOutStdout) {
  const stdout = typeof error.stdout === 'string' ? error.stdout : '';
  const stderr = typeof error.stderr === 'string' ? error.stderr : '';
  if (acceptTimedOutStdout?.(stdout) === true) return { stdout, stderr };
  throw new Error(`Chrome timed out after ${CHROME_TIMEOUT_MS} ms.${boundedDiagnostic(stderr)}`);
}

function isKilledProcessError(error) {
  return error !== null && typeof error === 'object' && error.killed === true;
}

function boundedDiagnostic(stderr) {
  if (stderr === '') return '';
  const tail = stderr.slice(-MAX_DIAGNOSTIC_CODE_UNITS);
  return ` Stderr tail:\n${tail}`;
}
