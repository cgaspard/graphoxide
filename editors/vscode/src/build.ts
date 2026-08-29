import path from 'node:path';

export type GraphBuildOperation = 'build' | 'update' | 'rebuild';

export interface GraphBuildState {
  readonly graphFileExists: boolean;
  readonly hasValidBaseline: boolean;
}

/** Resolve the optional project-scoped Registry v1 binding for CLI builds. */
export function registryBindingArguments(workspacePath: string, binding: unknown): readonly string[] {
  if (binding === undefined || binding === null) return [];
  if (typeof binding !== 'object' || Array.isArray(binding)) {
    throw new Error('Graphoxide: Registry Binding must be an object with string tree and origin fields.');
  }
  const record = binding as Record<string, unknown>;
  if (Object.keys(record).some((key) => key !== 'tree' && key !== 'origin')
    || typeof record.tree !== 'string' || !record.tree.trim()
    || typeof record.origin !== 'string' || !record.origin.trim()
    || record.tree.includes('\0') || record.origin.includes('\0')) {
    throw new Error('Graphoxide: Registry Binding must contain only non-empty string tree and origin fields.');
  }
  return ['--registry', path.resolve(workspacePath, record.tree), '--registry-origin', record.origin];
}

export interface GraphBuildRun {
  readonly kind: 'run';
  readonly args: readonly string[];
  readonly progressTitle: string;
  readonly completionMessage: string;
}

export interface GraphBuildBlocked {
  readonly kind: 'blocked';
  readonly message: string;
  readonly suggestedCommand: 'graphoxide.initialize' | 'graphoxide.rebuild';
  readonly suggestedLabel: string;
}

export type GraphBuildDecision = GraphBuildRun | GraphBuildBlocked;

/**
 * Automatic updates must currently authorize a legitimate graph reduction
 * after source deletion. The CLI's `--force` flag also bypasses extraction
 * caches; keep this policy centralized until those controls are separated.
 */
export function automaticGraphUpdateArguments(workspacePath: string): readonly string[] {
  return ['update', workspacePath, '--force'];
}

/** Central policy for commands and background jobs that can mutate graph artifacts. */
export function workspaceGraphMutationAllowed(workspaceTrusted: boolean): boolean {
  return workspaceTrusted;
}

/** Resolve and validate the CLI-managed directory for a configured graph path. */
export function graphBuildOutputDirectory(workspacePath: string, graphPath: string): string {
  if (path.basename(graphPath) !== 'graph.json') {
    throw new Error('Graphoxide build controls require Graphoxide: Graph Path to end in graph.json.');
  }
  const outputDirectory = path.resolve(path.dirname(graphPath));
  const workspaceRelativeToOutput = path.relative(outputDirectory, path.resolve(workspacePath));
  const outputContainsWorkspace = workspaceRelativeToOutput === ''
    || (workspaceRelativeToOutput !== '..'
      && !workspaceRelativeToOutput.startsWith(`..${path.sep}`)
      && !path.isAbsolute(workspaceRelativeToOutput));
  if (outputContainsWorkspace) {
    throw new Error('Graphoxide build controls require Graphoxide: Graph Path to be inside a dedicated output directory, not the workspace root or one of its ancestors.');
  }
  return outputDirectory;
}

/** Pure command policy shared by the VS Code handlers and focused tests. */
export function graphBuildDecision(
  operation: GraphBuildOperation,
  workspacePath: string,
  state: GraphBuildState,
): GraphBuildDecision {
  if (operation === 'build') {
    if (state.graphFileExists) {
      return {
        kind: 'blocked',
        message: 'A Graphoxide graph already exists. Update it incrementally or choose a full rebuild.',
        suggestedCommand: 'graphoxide.rebuild',
        suggestedLabel: 'Full Rebuild',
      };
    }
    return {
      kind: 'run',
      args: ['extract', workspacePath],
      progressTitle: 'Graphoxide: building graph…',
      completionMessage: 'Graphoxide graph build complete.',
    };
  }

  if (operation === 'update') {
    if (!state.hasValidBaseline) {
      if (state.graphFileExists) {
        return {
          kind: 'blocked',
          message: 'The existing Graphoxide graph could not be loaded, so it cannot be updated incrementally. Run a full rebuild instead.',
          suggestedCommand: 'graphoxide.rebuild',
          suggestedLabel: 'Full Rebuild',
        };
      }
      return {
        kind: 'blocked',
        message: 'Build a Graphoxide graph before running an incremental update.',
        suggestedCommand: 'graphoxide.initialize',
        suggestedLabel: 'Build Graph',
      };
    }
    return {
      kind: 'run',
      args: ['update', workspacePath],
      progressTitle: 'Graphoxide: updating graph incrementally…',
      completionMessage: 'Graphoxide incremental update complete.',
    };
  }

  if (!state.graphFileExists) {
    return {
      kind: 'blocked',
      message: 'No Graphoxide graph exists yet. Build the graph first.',
      suggestedCommand: 'graphoxide.initialize',
      suggestedLabel: 'Build Graph',
    };
  }
  return {
    kind: 'run',
    args: ['extract', workspacePath, '--force'],
    progressTitle: 'Graphoxide: rebuilding full graph…',
    completionMessage: 'Graphoxide full rebuild complete.',
  };
}
