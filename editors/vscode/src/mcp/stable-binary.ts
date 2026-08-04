import * as fs from 'node:fs';
import * as path from 'node:path';

/**
 * VS Code installs every extension release into its own versioned directory and
 * deletes the previous one on upgrade, so any path under it is valid only until
 * the next update. In-process spawns re-resolve on each launch and never notice.
 * MCP registrations cannot: they are read by external clients (Claude Code,
 * Codex, OpenCode) that have no way to call back into the extension host, so the
 * path they hold has to outlive the directory it was resolved from.
 *
 * The bundled binary is therefore linked into a location keyed by extension id
 * instead of version — VS Code's per-profile global storage — and that is the
 * path written to disk. A hard link costs no extra space and keeps working after
 * the original extension directory is gone, because a file's contents survive
 * until the last link to them is removed.
 */

/** Sidecar recording which build the linked binary came from. */
const VERSION_FILE = 'graphoxide.version';

/** Suffix for a link that could not be deleted yet; swept on a later run. */
const STALE_SUFFIX = '.stale-';

export interface StableBinaryResult {
  readonly path: string;
  /** True when this call (re)created the link rather than finding it current. */
  readonly linked: boolean;
  /** Set when hard linking was impossible and the bytes were copied instead. */
  readonly copied?: boolean;
}

export function executableName(platform: NodeJS.Platform): string {
  return platform === 'win32' ? 'graphoxide.exe' : 'graphoxide';
}

export function bundledBinary(extensionPath: string, platform: NodeJS.Platform): string {
  return path.join(extensionPath, 'bin', executableName(platform));
}

/**
 * Link the bundled binary into `stableDir` and return the version-independent
 * path to it. Returns undefined when the extension ships no binary or the link
 * cannot be established, leaving callers to fall back to the versioned path.
 */
export function ensureStableBinary(
  extensionPath: string,
  stableDir: string,
  platform: NodeJS.Platform,
): StableBinaryResult | undefined {
  const source = bundledBinary(extensionPath, platform);
  if (!isFile(source)) return undefined;
  const target = path.join(stableDir, executableName(platform));
  const identity = buildIdentity(source, path.join(extensionPath, 'bin', VERSION_FILE));
  try {
    if (isFile(target) && readIdentity(path.join(stableDir, VERSION_FILE)) === identity) {
      return { path: target, linked: false };
    }
    fs.mkdirSync(stableDir, { recursive: true });
    sweepStaleLinks(stableDir);
    if (!clearTarget(target)) return undefined;
    let copied = false;
    try {
      fs.linkSync(source, target);
    } catch {
      // Separate volume, or a filesystem without hard links: real bytes still
      // work, they just are not free.
      fs.copyFileSync(source, target);
      fs.chmodSync(target, 0o755);
      copied = true;
    }
    // Written last so a crash mid-link leaves a mismatch and simply relinks.
    fs.writeFileSync(path.join(stableDir, VERSION_FILE), `${identity}\n`, 'utf8');
    return { path: target, linked: true, ...(copied ? { copied } : {}) };
  } catch {
    return undefined;
  }
}

/**
 * True when `command` points into an extension directory belonging to some
 * Graphoxide install. Used to distinguish a path this extension once wrote from
 * one the user chose deliberately.
 */
export function isExtensionScopedBinary(command: string): boolean {
  const segments = command.split(/[\\/]/u);
  const extensions = segments.lastIndexOf('extensions');
  return extensions >= 0 && segments.slice(extensions + 1).some((segment) => /graphoxide-vscode/iu.test(segment));
}

/**
 * True when `command` is a registration left behind by an extension directory
 * that no longer exists. Requiring both conditions keeps the repair pass off
 * paths a user configured on purpose, including a source build not yet compiled.
 */
export function isAbandonedExtensionBinary(command: string, exists: (file: string) => boolean = isFile): boolean {
  return isExtensionScopedBinary(command) && !exists(command);
}

/**
 * Prefer the version file shipped beside the binary; fall back to size and mtime
 * so a locally rebuilt binary carrying no version file still invalidates.
 */
function buildIdentity(source: string, versionFile: string): string {
  const declared = readIdentity(versionFile);
  if (declared) return declared;
  const stats = fs.statSync(source);
  return `size=${stats.size} mtime=${Math.trunc(stats.mtimeMs)}`;
}

function readIdentity(file: string): string | undefined {
  try {
    const lines = fs.readFileSync(file, 'utf8').split(/\r?\n/u).map((line) => line.trim()).filter(Boolean);
    return lines.length > 0 ? lines.join(' ') : undefined;
  } catch {
    return undefined;
  }
}

/** Make `target` free for a new link, or report that it cannot be replaced. */
function clearTarget(target: string): boolean {
  if (!fs.existsSync(target)) return true;
  try {
    fs.unlinkSync(target);
    return true;
  } catch {
    // Windows refuses to unlink a running executable but does allow renaming
    // it, which frees the name for the new link.
    try {
      const aside = `${target}${STALE_SUFFIX}${process.pid}`;
      fs.renameSync(target, aside);
      return true;
    } catch {
      return false;
    }
  }
}

/** Best-effort removal of links renamed aside by an earlier activation. */
function sweepStaleLinks(stableDir: string): void {
  try {
    for (const entry of fs.readdirSync(stableDir)) {
      if (!entry.includes(STALE_SUFFIX)) continue;
      try {
        fs.unlinkSync(path.join(stableDir, entry));
      } catch {
        // Still held open; a later activation retries.
      }
    }
  } catch {
    // Directory unreadable or absent: nothing to sweep.
  }
}

function isFile(file: string): boolean {
  try {
    return fs.statSync(file).isFile();
  } catch {
    return false;
  }
}
