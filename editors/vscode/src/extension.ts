import * as path from 'node:path';
import * as vscode from 'vscode';
import { automaticGraphUpdateArguments, graphBuildDecision, GraphBuildOperation, workspaceGraphMutationAllowed } from './build';
import { GraphoxideCli } from './cli';
import { GraphCodeLensProvider } from './codelens';
import { ControlCenterPanel } from './control-center';
import { GraphNode } from './graph';
import { AiLabelingService, AiLabelingTestConfiguration } from './llm/service';
import { ManagedWorkspaceService } from './managed';
import { repairAbandonedRegistrations } from './mcp/installers';
import { GraphoxideMcpProvider } from './mcp/provider';
import { resolvedInvocation } from './mcp/runtime';
import { GraphMutationSnapshot } from './mutation-coordinator';
import { GraphStore } from './store';
import { communityFromArgument, GraphExplorerProvider, nodeFromArgument, ResultsProvider } from './tree';
import { GraphVisualizer, GraphVisualizerRendererState, GraphVisualizerTestAction } from './visualizer';

interface ExtensionServices {
  readonly store: GraphStore;
  readonly cli: GraphoxideCli;
  readonly explorer: GraphExplorerProvider;
  readonly results: ResultsProvider;
  readonly visualizer: GraphVisualizer;
  readonly statusBar: vscode.StatusBarItem;
  readonly managed: ManagedWorkspaceService;
  readonly aiLabeling: AiLabelingService;
}

export interface GraphoxideExtensionStatus {
  readonly enabled: boolean;
  readonly freshness: 'watch' | 'save' | 'manual';
  readonly watching: boolean;
  readonly graphPath?: string;
  readonly nodes?: number;
  readonly edges?: number;
  readonly mcp?: { readonly command: string; readonly args: readonly string[]; readonly cwd: string };
}

export interface GraphoxideWatchLifecycleStatus {
  readonly phase: 'stopped' | 'starting' | 'ready' | 'stopping';
  readonly generation: number;
  readonly activeGeneration?: number;
  readonly lastExitedGeneration: number;
  readonly processTarget: 'expected' | 'different' | 'none';
  readonly graphTarget: 'expected' | 'different' | 'none';
}

export interface GraphoxideWatchRestartBarrier {
  waitUntilReached(): Promise<GraphoxideWatchLifecycleStatus>;
  release(): void;
}

export interface GraphoxideExtensionApi {
  readonly version: 1;
  readonly test?: {
    configureAi(input: AiLabelingTestConfiguration): Promise<readonly string[]>;
    improveCommunityLabels(): Promise<void>;
    clearAi(): Promise<void>;
    watchLifecycle(): GraphoxideWatchLifecycleStatus;
    holdNextGraphPathRestart(): GraphoxideWatchRestartBarrier;
    waitForWatchRestart(previousGeneration: number): Promise<GraphoxideWatchLifecycleStatus>;
    restartWatchConcurrently(): Promise<GraphoxideWatchLifecycleStatus>;
    mutationLifecycle(): GraphMutationSnapshot;
    runUpdateConcurrently(): Promise<GraphMutationSnapshot>;
    staleEnableFailurePreservesDisable(): Promise<boolean>;
    resumeManagedBehindMutation(): Promise<{
      readonly mutationBefore: GraphMutationSnapshot;
      readonly mutationAfter: GraphMutationSnapshot;
      readonly watch: GraphoxideWatchLifecycleStatus;
    }>;
    resumeManagedAcrossWatchRace(): Promise<{
      readonly mutationBefore: GraphMutationSnapshot;
      readonly mutationAfter: GraphMutationSnapshot;
      readonly watch: GraphoxideWatchLifecycleStatus;
    }>;
    visualizerState(): Promise<GraphVisualizerRendererState>;
    visualizerAction(action: GraphVisualizerTestAction, value?: string): Promise<void>;
  };
  enableWorkspace(freshness?: 'watch' | 'save' | 'manual'): Promise<void>;
  configureFreshness(freshness: 'watch' | 'save' | 'manual'): Promise<void>;
  status(): Promise<GraphoxideExtensionStatus>;
}

export async function activate(context: vscode.ExtensionContext): Promise<GraphoxideExtensionApi> {
  const store = new GraphStore();
  const cli = new GraphoxideCli(context.extensionUri);
  const explorer = new GraphExplorerProvider(store);
  const results = new ResultsProvider();
  const statusBar = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 40);
  statusBar.name = 'Graphoxide';
  statusBar.command = 'graphoxide.openControlCenter';
  const visualizer = new GraphVisualizer(
    context.extensionUri,
    (id) => void revealNodeById(store, id),
    (id) => {
      const node = store.state?.model?.getNode(id);
      if (node) void vscode.commands.executeCommand('graphoxide.explain', node);
    },
    context.extensionMode,
  );
  const managed = new ManagedWorkspaceService(context, store, cli);
  const aiLabeling = new AiLabelingService(context, cli, store);
  const mcpProvider = new GraphoxideMcpProvider(context, (folder) => managed.isEnabled(folder));
  const services: ExtensionServices = { store, cli, explorer, results, visualizer, statusBar, managed, aiLabeling };
  const codeLens = new GraphCodeLensProvider(store);
  let graphPathReload = Promise.resolve();
  let graphPathRestartBarrier: TestGraphPathRestartBarrier | undefined;

  context.subscriptions.push(
    store,
    cli,
    explorer,
    results,
    visualizer,
    codeLens,
    statusBar,
    managed,
    mcpProvider,
    vscode.window.registerTreeDataProvider('graphoxide.explorer', explorer),
    vscode.window.registerTreeDataProvider('graphoxide.results', results),
    vscode.languages.registerCodeLensProvider({ scheme: 'file' }, codeLens),
    store.onDidChange((state) => {
      updateStatusBar(statusBar, state?.model?.snapshot.nodes.length, state?.model?.snapshot.edges.length, state?.error, cli.watching);
      visualizer.refresh(state?.model, state?.error);
    }),
    cli.onDidChangeWatch(() => updateStatusBar(statusBar, store.state?.model?.snapshot.nodes.length, store.state?.model?.snapshot.edges.length, store.state?.error, cli.watching)),
    managed.onDidChangeEnablement(() => mcpProvider.refresh()),
    vscode.workspace.onDidChangeConfiguration((event) => {
      const folder = store.state?.folder;
      if (event.affectsConfiguration('graphoxide.graphPath')
        && (!folder || event.affectsConfiguration('graphoxide.graphPath', folder.uri))) {
        const restartBarrier = graphPathRestartBarrier;
        graphPathRestartBarrier = undefined;
        graphPathReload = graphPathReload
          .then(() => reloadGraphPathConfiguration(store, cli, restartBarrier))
          .catch((error: unknown) => handleError(error));
      }
      if (event.affectsConfiguration('graphoxide.codeLens.enabled')) codeLens.refresh();
    }),
    vscode.window.onDidChangeActiveTextEditor((editor) => {
      const folder = editor ? vscode.workspace.getWorkspaceFolder(editor.document.uri) : undefined;
      if (folder && folder.uri.toString() !== store.state?.folder.uri.toString()) void store.load(folder);
    }),
    ...registerCommands(context, services),
    registerUpdateOnSave(services),
  );

  await vscode.commands.executeCommand('setContext', 'graphoxide.hasResults', false);
  await vscode.commands.executeCommand('setContext', 'graphoxide.watching', false);
  await vscode.commands.executeCommand('setContext', 'graphoxide.hasGraphFile', false);
  await store.initialize();
  updateStatusBar(statusBar, store.state?.model?.snapshot.nodes.length, store.state?.model?.snapshot.edges.length, store.state?.error, false);
  statusBar.show();
  void managed.start();
  void repairRegistrations(context, cli);
  return {
    version: 1,
    ...(context.extensionMode !== vscode.ExtensionMode.Production ? {
      test: {
        configureAi: (input: AiLabelingTestConfiguration) => aiLabeling.configureForTest(input),
        improveCommunityLabels: () => aiLabeling.improveCommunityLabelsForTest(),
        clearAi: () => aiLabeling.clearTestConfiguration(),
        watchLifecycle: () => observeWatchLifecycle(store, cli),
        holdNextGraphPathRestart: () => {
          if (graphPathRestartBarrier) throw new Error('A graph-path restart barrier is already armed.');
          const barrier = new TestGraphPathRestartBarrier();
          graphPathRestartBarrier = barrier;
          return {
            waitUntilReached: async () => {
              await withDeadline(
                barrier.waitUntilReached(),
                10000,
                () => `Timed out after 10000 ms waiting for the graph-path restart barrier; observed ${describeWatchObservation(observeWatchLifecycle(store, cli))}.`,
              );
              return observeWatchLifecycle(store, cli);
            },
            release: () => barrier.release(),
          };
        },
        waitForWatchRestart: async (previousGeneration: number) => {
          // Configuration events are serialized through this promise. Waiting
          // for the current tail prevents a rapid second graph-path change from
          // making an otherwise-ready intermediate generation look final.
          await withDeadline(
            graphPathReload,
            30000,
            () => `Timed out after 30000 ms waiting for graph-path configuration reloads; observed ${describeWatchObservation(observeWatchLifecycle(store, cli))}.`,
          );
          return waitForWatchRestart(store, cli, previousGeneration);
        },
        restartWatchConcurrently: async () => {
          const folder = store.state?.folder ?? await store.preferredFolder(false);
          if (!folder) throw new Error('Cannot restart watch mode without a workspace folder.');
          const output = store.managedOutput(folder);
          const before = observeWatchLifecycle(store, cli, output);
          if (before.phase !== 'ready' || before.activeGeneration === undefined) {
            throw new Error(`Cannot exercise a concurrent restart unless watch mode is ready; observed ${describeWatchObservation(before)}.`);
          }
          cli.stopWatch();
          const first = cli.startWatch(folder, output.environment);
          const second = cli.startWatch(folder, output.environment);
          await Promise.all([first, second]);
          return observeWatchLifecycle(store, cli, output);
        },
        mutationLifecycle: () => cli.mutationLifecycle(),
        runUpdateConcurrently: async () => {
          const before = cli.mutationLifecycle();
          if (before.phase !== 'idle') throw new Error(`Cannot exercise concurrent updates while mutation phase is ${before.phase}.`);
          const barrier = cli.holdNextMutationStart();
          const first = runGraphBuild('update', services);
          try {
            await withDeadline(
              barrier.waitUntilReached(),
              10000,
              () => `Timed out waiting for the mutation barrier; observed ${JSON.stringify(cli.mutationLifecycle())}.`,
            );
            await runGraphBuild('update', services);
          } finally {
            barrier.release();
          }
          await first;
          return cli.mutationLifecycle();
        },
        staleEnableFailurePreservesDisable: async () => {
          const folder = store.state?.folder ?? await store.preferredFolder(false);
          if (!folder) throw new Error('Cannot exercise managed enablement without a workspace folder.');
          if (managed.isEnabled(folder)) {
            throw new Error('Stale enablement regression requires a disabled managed workspace.');
          }
          const barrier = cli.holdNextMutationStart();
          const enabling = managed.enable(folder, 'manual', false);
          try {
            await withDeadline(
              barrier.waitUntilReached(),
              10000,
              () => `Timed out waiting for the stale enablement mutation barrier; observed ${JSON.stringify(cli.mutationLifecycle())}.`,
            );
            await managed.disable(folder);
            barrier.release(new Error('injected stale managed enablement failure'));
          } finally {
            barrier.release();
          }
          await enabling;
          return managed.isEnabled(folder);
        },
        resumeManagedBehindMutation: async () => {
          const folder = store.state?.folder ?? await store.preferredFolder(false);
          if (!folder) throw new Error('Cannot exercise managed resume without a workspace folder.');
          if (!managed.isEnabled(folder) || managed.freshness(folder) !== 'watch') {
            throw new Error('Managed resume regression requires an enabled workspace configured for watch mode.');
          }
          await cli.stopWatchAndWait();
          const mutationBefore = cli.mutationLifecycle();
          if (mutationBefore.phase !== 'idle') {
            throw new Error(`Cannot exercise managed resume while mutation phase is ${mutationBefore.phase}.`);
          }
          const barrier = cli.holdNextMutationStart();
          const manual = runGraphBuild('update', services);
          let resume: Promise<void> | undefined;
          try {
            await withDeadline(
              barrier.waitUntilReached(),
              10000,
              () => `Timed out waiting for the manual-first mutation barrier; observed ${JSON.stringify(cli.mutationLifecycle())}.`,
            );
            resume = managed.start();
          } finally {
            barrier.release();
          }
          await Promise.all([manual, resume ?? Promise.resolve()]);
          return {
            mutationBefore,
            mutationAfter: cli.mutationLifecycle(),
            watch: observeWatchLifecycle(store, cli),
          };
        },
        resumeManagedAcrossWatchRace: async () => {
          const folder = store.state?.folder ?? await store.preferredFolder(false);
          if (!folder) throw new Error('Cannot exercise managed resume without a workspace folder.');
          if (!managed.isEnabled(folder) || managed.freshness(folder) !== 'watch') {
            throw new Error('Managed resume race regression requires an enabled workspace configured for watch mode.');
          }
          await cli.stopWatchAndWait();
          const mutationBefore = cli.mutationLifecycle();
          if (mutationBefore.phase !== 'idle') {
            throw new Error(`Cannot exercise managed resume race while mutation phase is ${mutationBefore.phase}.`);
          }
          const watchBarrier = managed.holdNextResumeWatchStart();
          const resume = managed.start();
          let mutationBarrier: ReturnType<GraphoxideCli['holdNextMutationStart']> | undefined;
          let competing: Promise<void> | undefined;
          try {
            await withDeadline(
              watchBarrier.waitUntilReached(),
              10000,
              () => `Timed out waiting for managed resume to reload before watch start; observed ${JSON.stringify(cli.mutationLifecycle())}.`,
            );
            mutationBarrier = cli.holdNextMutationStart();
            competing = runGraphBuild('update', services);
            await withDeadline(
              mutationBarrier.waitUntilReached(),
              10000,
              () => `Timed out waiting for the competing mutation barrier; observed ${JSON.stringify(cli.mutationLifecycle())}.`,
            );
            watchBarrier.release();
            await withDeadline(
              watchBarrier.waitUntilBusyJoined(),
              10000,
              () => `Managed resume did not join the mutation admitted between reload and watch start; observed ${JSON.stringify(cli.mutationLifecycle())}.`,
            );
          } finally {
            watchBarrier.release();
            mutationBarrier?.release();
          }
          await Promise.all([resume, competing ?? Promise.resolve()]);
          return {
            mutationBefore,
            mutationAfter: cli.mutationLifecycle(),
            watch: observeWatchLifecycle(store, cli),
          };
        },
        visualizerState: () => visualizer.visualizerState(),
        visualizerAction: (action: GraphVisualizerTestAction, value?: string) => visualizer.visualizerAction(action, value),
      },
    } : {}),
    enableWorkspace: (freshness = 'manual') => managed.enable(undefined, freshness, false),
    configureFreshness: (freshness) => managed.configureFreshness(undefined, freshness),
    status: async () => {
      const folder = store.state?.folder ?? await store.preferredFolder(false);
      const state = folder ? await store.load(folder) : undefined;
      return {
        enabled: folder ? managed.isEnabled(folder) : false,
        freshness: folder ? managed.freshness(folder) : 'manual',
        watching: cli.watching,
        ...(state ? { graphPath: state.graphUri.fsPath } : {}),
        ...(state?.model ? { nodes: state.model.snapshot.nodes.length, edges: state.model.snapshot.edges.length } : {}),
        ...(folder ? { mcp: cli.invocation(folder) } : {}),
      };
    },
  };
}

export function deactivate(): void {
  // VS Code disposes everything registered in the extension context.
}

async function reloadGraphPathConfiguration(
  store: GraphStore,
  cli: GraphoxideCli,
  restartBarrier?: TestGraphPathRestartBarrier,
): Promise<void> {
  const previousFolder = store.state?.folder ?? await store.preferredFolder(false);
  const restartWatch = cli.watchActive;
  if (restartWatch) {
    await cli.stopWatchAndWait();
    await restartBarrier?.pause();
  }
  await store.initialize();
  const folder = store.state?.folder ?? previousFolder;
  if (restartWatch && folder) await cli.startWatch(folder, store.managedOutput(folder).environment);
}

async function waitForWatchRestart(
  store: GraphStore,
  cli: GraphoxideCli,
  previousGeneration: number,
): Promise<GraphoxideWatchLifecycleStatus> {
  const folder = store.state?.folder ?? await store.preferredFolder(false);
  if (!folder) throw new Error('Cannot observe a watch restart without a workspace folder.');
  const expectedOutput = store.managedOutput(folder);
  await cli.waitForWatchLifecycle(
    expectedOutput.outputDirectory,
    (snapshot) => {
      const observation = observeWatchLifecycle(store, cli, expectedOutput);
      return snapshot.phase === 'ready'
        && (snapshot.activeGeneration ?? 0) > previousGeneration
        && snapshot.lastExitedGeneration >= previousGeneration
        && observation.processTarget === 'expected'
        && observation.graphTarget === 'expected';
    },
    {
      description: `watch generation after ${previousGeneration} to own the configured graph target`,
      timeoutMs: 10000,
      diagnostics: () => describeWatchObservation(observeWatchLifecycle(store, cli, expectedOutput)),
    },
  );
  return observeWatchLifecycle(store, cli, expectedOutput);
}

function observeWatchLifecycle(
  store: GraphStore,
  cli: GraphoxideCli,
  expectedOutput = (() => {
    const folder = store.state?.folder;
    return folder ? store.managedOutput(folder) : undefined;
  })(),
): GraphoxideWatchLifecycleStatus {
  const lifecycle = cli.watchLifecycle(expectedOutput?.outputDirectory);
  const processTarget = lifecycle.activeGeneration === undefined
    ? 'none'
    : lifecycle.targetMatchesExpected ? 'expected' : 'different';
  const graphTarget = !store.state || !expectedOutput
    ? 'none'
    : store.state.graphUri.fsPath === expectedOutput.graphUri.fsPath ? 'expected' : 'different';
  return {
    phase: lifecycle.phase,
    generation: lifecycle.generation,
    ...(lifecycle.activeGeneration === undefined ? {} : { activeGeneration: lifecycle.activeGeneration }),
    lastExitedGeneration: lifecycle.lastExitedGeneration,
    processTarget,
    graphTarget,
  };
}

function describeWatchObservation(observation: GraphoxideWatchLifecycleStatus): string {
  const active = observation.activeGeneration === undefined ? 'none' : String(observation.activeGeneration);
  return `phase=${observation.phase}, generation=${observation.generation}, active=${active}, lastExited=${observation.lastExitedGeneration}, processTarget=${observation.processTarget}, graphTarget=${observation.graphTarget}`;
}

class TestGraphPathRestartBarrier {
  private reached = false;
  private released = false;
  private readonly reachedPromise: Promise<void>;
  private readonly releasePromise: Promise<void>;
  private markReached!: () => void;
  private markReleased!: () => void;

  constructor() {
    this.reachedPromise = new Promise<void>((resolve) => {
      this.markReached = resolve;
    });
    this.releasePromise = new Promise<void>((resolve) => {
      this.markReleased = resolve;
    });
  }

  async pause(): Promise<void> {
    if (!this.reached) {
      this.reached = true;
      this.markReached();
    }
    await this.releasePromise;
  }

  waitUntilReached(): Promise<void> {
    return this.reachedPromise;
  }

  release(): void {
    if (this.released) return;
    this.released = true;
    this.markReleased();
  }
}

function withDeadline(promise: Promise<void>, timeoutMs: number, message: () => string): Promise<void> {
  return new Promise<void>((resolve, reject) => {
    let settled = false;
    const finish = (error?: Error): void => {
      if (settled) return;
      settled = true;
      clearTimeout(timeout);
      if (error) reject(error);
      else resolve();
    };
    const timeout = setTimeout(() => finish(new Error(message())), timeoutMs);
    void promise.then(() => finish(), (error: unknown) => {
      finish(error instanceof Error ? error : new Error(String(error)));
    });
  });
}

/**
 * Repair registrations pointing at an extension directory a past upgrade removed.
 * Best effort and silent when there is nothing to fix, so a routine activation
 * stays quiet; failures are logged rather than surfaced because the user did not
 * ask for this and the Control Center still reports the entry as stale.
 */
async function repairRegistrations(context: vscode.ExtensionContext, cli: GraphoxideCli): Promise<void> {
  try {
    for (const folder of vscode.workspace.workspaceFolders ?? []) {
      const invocation = await resolvedInvocation(folder, context);
      const repaired = await repairAbandonedRegistrations({ folder, invocation });
      if (repaired.length === 0) continue;
      cli.output.info(`Repaired Graphoxide MCP registrations in ${folder.name} left behind by an extension update: ${repaired.join(', ')}.`);
    }
  } catch (error) {
    cli.output.warn(`Could not repair Graphoxide MCP registrations: ${error instanceof Error ? error.message : String(error)}`);
  }
}

function registerCommands(context: vscode.ExtensionContext, services: ExtensionServices): vscode.Disposable[] {
  const { store, cli, explorer, results, visualizer, managed, aiLabeling } = services;
  const command = (id: string, handler: (...args: unknown[]) => unknown): vscode.Disposable =>
    vscode.commands.registerCommand(id, (...args: unknown[]) => Promise.resolve(handler(...args)).catch((error: unknown) => handleError(error)));
  const openControlCenter = (): void => ControlCenterPanel.show(context, { store, cli, managed, aiLabeling });

  return [
    command('graphoxide.initialize', () => runGraphBuild('build', services)),
    command('graphoxide.update', () => runGraphBuild('update', services)),
    command('graphoxide.rebuild', () => runGraphBuild('rebuild', services)),
    command('graphoxide.startWatch', async () => {
      const folder = await requireFolder(store);
      if (folder) await cli.startWatch(folder, store.managedOutput(folder).environment);
    }),
    command('graphoxide.stopWatch', () => cli.stopWatch()),
    command('graphoxide.refresh', async () => {
      await store.load();
      explorer.refresh();
    }),
    command('graphoxide.openGraph', () => {
      const model = store.state?.model;
      if (!model) return missingGraph();
      visualizer.show(model);
    }),
    command('graphoxide.openGraphBeside', () => {
      const model = store.state?.model;
      if (!model) return missingGraph();
      visualizer.show(model, undefined, 'beside');
    }),
    command('graphoxide.openGraphFile', async () => {
      if (!store.state?.graphFileExists) return missingGraph();
      await vscode.window.showTextDocument(store.state.graphUri, { preview: true });
    }),
    command('graphoxide.query', async () => {
      const state = store.state;
      if (!state?.model) return missingGraph();
      const question = await vscode.window.showInputBox({ title: 'Query Graphoxide', prompt: 'Ask a question about this codebase', placeHolder: 'How does authentication work?', ignoreFocusOut: true });
      if (!question?.trim()) return;
      const config = vscode.workspace.getConfiguration('graphoxide', state.folder.uri);
      const budget = config.get<number>('defaultTokenBudget', 4000);
      const traversal = config.get<string>('queryTraversal', 'breadth-first');
      const args = ['query', question.trim(), '--budget', String(budget), '--graph', state.graphUri.fsPath];
      if (traversal === 'depth-first') args.push('--dfs');
      const result = await cli.run({ title: 'Graphoxide: querying graph…', folder: state.folder, args });
      results.setOutput(question.trim(), result.stdout, nodesMentioned(state.model.snapshot.nodes, result.stdout));
      await vscode.commands.executeCommand('graphoxide.results.focus');
    }),
    command('graphoxide.explain', async (value?: unknown) => {
      const state = store.state;
      if (!state?.model) return missingGraph();
      const node = nodeFromArgument(value) ?? await store.chooseNode({ title: 'Explain a Graphoxide node' });
      if (!node) return;
      const result = await cli.run({ title: `Graphoxide: explaining ${node.label}…`, folder: state.folder, args: ['explain', node.id, '--graph', state.graphUri.fsPath] });
      results.setOutput(`Explain · ${node.label}`, result.stdout, [node, ...nodesMentioned(state.model.snapshot.nodes, result.stdout).filter((other) => other.id !== node.id)]);
      await vscode.commands.executeCommand('graphoxide.results.focus');
    }),
    command('graphoxide.explainAtCursor', async () => {
      const state = store.state;
      const editor = vscode.window.activeTextEditor;
      if (!state?.model || !editor) return missingGraph();
      const relative = safeRelativePath(state.folder.uri.fsPath, editor.document.uri.fsPath);
      if (!relative) return void vscode.window.showInformationMessage('The active file is outside the indexed workspace.');
      const wordRange = editor.document.getWordRangeAtPosition(editor.selection.active);
      const word = wordRange ? editor.document.getText(wordRange) : '';
      const fileNodes = state.model.nodesForSourceFile(relative);
      const exact = fileNodes.filter((node) => node.label.toLocaleLowerCase() === word.toLocaleLowerCase());
      const node = exact[0] ?? (fileNodes.length === 1 ? fileNodes[0] : await store.chooseNode({ title: `Symbols in ${path.basename(relative)}`, nodes: fileNodes }));
      if (!node) return void vscode.window.showInformationMessage('No indexed graph node was found at the cursor.');
      await vscode.commands.executeCommand('graphoxide.explain', node);
    }),
    command('graphoxide.path', async () => {
      const state = store.state;
      if (!state?.model) return missingGraph();
      const source = await store.chooseNode({ title: 'Path: choose the starting node' });
      if (!source) return;
      const target = await store.chooseNode({ title: 'Path: choose the destination node' });
      if (!target) return;
      const result = await cli.run({ title: `Graphoxide: finding path from ${source.label} to ${target.label}…`, folder: state.folder, args: ['path', source.id, target.id, '--graph', state.graphUri.fsPath] });
      results.setOutput(`${source.label} → ${target.label}`, result.stdout, [source, ...nodesMentioned(state.model.snapshot.nodes, result.stdout).filter((node) => node.id !== source.id)]);
      await vscode.commands.executeCommand('graphoxide.results.focus');
    }),
    command('graphoxide.affected', async (value?: unknown) => {
      const state = store.state;
      if (!state?.model) return missingGraph();
      const node = nodeFromArgument(value) ?? await store.chooseNode({ title: 'Find nodes affected by…' });
      if (!node) return;
      const depth = vscode.workspace.getConfiguration('graphoxide', state.folder.uri).get<number>('defaultAffectedDepth', 2);
      const result = await cli.run({ title: `Graphoxide: tracing impact from ${node.label}…`, folder: state.folder, args: ['affected', node.id, '--depth', String(depth), '--graph', state.graphUri.fsPath] });
      results.setOutput(`Affected by ${node.label}`, result.stdout, [node, ...nodesMentioned(state.model.snapshot.nodes, result.stdout).filter((other) => other.id !== node.id)]);
      await vscode.commands.executeCommand('graphoxide.results.focus');
    }),
    command('graphoxide.godNodes', async () => {
      const state = store.state;
      if (!state?.model) return missingGraph();
      const result = await cli.run({ title: 'Graphoxide: finding architectural hubs…', folder: state.folder, args: ['god-nodes', '--top', '25', '--json', '--graph', state.graphUri.fsPath] });
      results.setNodes('Architectural hubs', state.model.hubs(25));
      cli.output.info(result.stdout.trim());
      await vscode.commands.executeCommand('graphoxide.results.focus');
    }),
    command('graphoxide.revealNode', async (value?: unknown) => {
      const node = nodeFromArgument(value) ?? await store.chooseNode({ title: 'Open a graph node in source' });
      if (node) await revealNode(store, node);
    }),
    command('graphoxide.showCommunity', (value?: unknown) => {
      const model = store.state?.model;
      const community = communityFromArgument(value);
      if (model && community) visualizer.show(model, community.id);
    }),
    command('graphoxide.report', async () => {
      const state = store.state;
      if (!state?.model) return missingGraph();
      const defaultUri = vscode.Uri.joinPath(state.folder.uri, 'graphoxide-out', 'GRAPH_REPORT.md');
      const output = await vscode.window.showSaveDialog({ title: 'Save Graphoxide architecture report', defaultUri, filters: { Markdown: ['md'] } });
      if (!output) return;
      await cli.run({ title: 'Graphoxide: generating architecture report…', folder: state.folder, args: ['report', '--graph', state.graphUri.fsPath, '--output', output.fsPath] });
      await vscode.window.showTextDocument(output, { preview: false });
    }),
    command('graphoxide.export', async () => {
      const state = store.state;
      if (!state?.model) return missingGraph();
      const picked = await vscode.window.showQuickPick([
        { label: 'Interactive HTML', format: 'html', extension: 'html' },
        { label: 'Call-flow HTML', format: 'callflow-html', extension: 'html' },
        { label: 'GraphML', format: 'graphml', extension: 'graphml' },
        { label: 'Cypher', format: 'cypher', extension: 'cypher' },
        { label: 'JSON', format: 'json', extension: 'json' },
        { label: 'Obsidian vault', format: 'obsidian', extension: '' },
      ], { title: 'Export Graphoxide graph' });
      if (!picked) return;
      let output: vscode.Uri | undefined;
      if (picked.format === 'obsidian') {
        output = (await vscode.window.showOpenDialog({ title: 'Choose Obsidian vault folder', canSelectFolders: true, canSelectFiles: false, canSelectMany: false }))?.[0];
      } else {
        output = await vscode.window.showSaveDialog({ defaultUri: vscode.Uri.joinPath(state.folder.uri, `graphoxide-export.${picked.extension}`) });
      }
      if (!output) return;
      await cli.run({ title: `Graphoxide: exporting ${picked.label}…`, folder: state.folder, args: ['export', picked.format, output.fsPath, '--graph', state.graphUri.fsPath] });
      void vscode.window.showInformationMessage(`Graphoxide export written to ${output.fsPath}`, 'Open').then(async (choice) => {
        if (choice === 'Open') await vscode.commands.executeCommand('revealFileInOS', output);
      });
    }),
    command('graphoxide.startServer', async () => {
      const folder = await requireFolder(store);
      if (folder) cli.openServerTerminal(folder);
    }),
    command('graphoxide.copyMcpConfig', async () => {
      const folder = await requireFolder(store);
      if (!folder) return;
      const invocation = cli.invocation(folder);
      const value = {
        mcpServers: {
          graphoxide: {
            command: invocation.command,
            args: invocation.args,
            cwd: invocation.cwd,
          },
        },
      };
      await vscode.env.clipboard.writeText(JSON.stringify(value, null, 2));
      void vscode.window.showInformationMessage('Graphoxide MCP configuration copied to the clipboard.');
    }),
    command('graphoxide.openControlCenter', openControlCenter),
    // Compatibility aliases for existing links, keybindings, and callers.
    command('graphoxide.manageMcp', openControlCenter),
    command('graphoxide.configureAiLabeling', () => aiLabeling.configure()),
    command('graphoxide.clearAiCredential', () => aiLabeling.clearCredential()),
    command('graphoxide.improveCommunityLabels', () => aiLabeling.improveCommunityLabels()),
    command('graphoxide.enableWorkspace', (freshness?: unknown) => managed.enable(undefined, isFreshnessMode(freshness) ? freshness : undefined)),
    command('graphoxide.disableWorkspace', () => managed.disable()),
    command('graphoxide.configureFreshness', (freshness?: unknown) => managed.configureFreshness(undefined, isFreshnessMode(freshness) ? freshness : undefined)),
    command('graphoxide.resetWorkspacePrompt', () => managed.resetPrompt()),
    command('graphoxide.showStatus', openControlCenter),
    command('graphoxide.openSettings', () => vscode.commands.executeCommand('workbench.action.openSettings', '@ext:cgaspard.graphoxide-vscode')),
    command('graphoxide.clearResults', () => results.clear()),
  ];
}

async function runGraphBuild(operation: GraphBuildOperation, services: ExtensionServices): Promise<void> {
  const folder = await requireFolder(services.store);
  if (!folder) return;
  if (!workspaceGraphMutationAllowed(vscode.workspace.isTrusted)) {
    void vscode.window.showWarningMessage('Trust this workspace before building or updating its Graphoxide graph.');
    return;
  }

  const state = await services.store.load(folder);
  let environment: Readonly<{ GRAPHOXIDE_OUT: string }>;
  try {
    environment = services.store.managedOutput(folder).environment;
  } catch (error) {
    void vscode.window.showErrorMessage(error instanceof Error ? error.message : String(error));
    return;
  }
  const decision = graphBuildDecision(operation, folder.uri.fsPath, {
    graphFileExists: state?.graphFileExists === true,
    hasValidBaseline: Boolean(state?.model),
  });
  if (decision.kind === 'blocked') {
    const choice = await vscode.window.showInformationMessage(decision.message, decision.suggestedLabel);
    if (choice === decision.suggestedLabel) await vscode.commands.executeCommand(decision.suggestedCommand);
    return;
  }

  if (operation === 'rebuild') {
    const confirmation = await vscode.window.showWarningMessage(
      `Fully rebuild the Graphoxide graph for “${folder.name}”?`,
      {
        modal: true,
        detail: 'This performs a full rescan of every supported input and replaces the existing generated graph after a successful build. Source files are not changed.',
      },
      'Full Rebuild',
    );
    if (confirmation !== 'Full Rebuild') return;
    const stoppedWatch = await services.cli.stopWatchAndWait();
    if (stoppedWatch) {
      void vscode.window.showInformationMessage('Graphoxide watch mode was stopped before the full rebuild. Restart it when you are ready.');
    }
  }

  const outcome = await services.cli.runMutation({
    title: decision.progressTitle,
    folder,
    args: decision.args,
    environment,
    mutationTarget: environment.GRAPHOXIDE_OUT,
    mutationOrigin: 'interactive',
    mutationLabel: operation === 'rebuild' ? 'performing a full rebuild' : `running an interactive ${operation}`,
    suppressAutomaticOnFailure: true,
  });
  if (outcome.kind !== 'completed') return;
  await services.store.load(folder);
  void vscode.window.showInformationMessage(decision.completionMessage);
}

function registerUpdateOnSave(services: ExtensionServices): vscode.Disposable {
  let timer: NodeJS.Timeout | undefined;
  let running = false;
  let pending = false;
  let disposed = false;
  const update = async (): Promise<void> => {
    if (disposed || !workspaceGraphMutationAllowed(vscode.workspace.isTrusted) || services.cli.watchMutationActive) {
      pending = false;
      return;
    }
    if (running) {
      pending = true;
      return;
    }
    const folder = services.store.state?.folder;
    if (!folder) return;
    running = true;
    try {
      const output = services.store.managedOutput(folder);
      // `--force` also authorizes legitimate shrink after source deletion in
      // the current CLI. Removing it here would allow stale deleted facts.
      const outcome = await services.cli.runMutation({
        title: 'Graphoxide: updating after save…',
        folder,
        args: automaticGraphUpdateArguments(folder.uri.fsPath),
        showProgress: false,
        cancellable: false,
        environment: output.environment,
        mutationTarget: output.outputDirectory,
        mutationOrigin: 'automatic',
        mutationLabel: 'updating the graph after save',
        suppressAutomaticOnFailure: true,
      });
      if (outcome.kind === 'completed') {
        await services.store.load(folder);
      } else if (outcome.kind === 'busy') {
        pending = true;
        await services.cli.waitForMutationIdle();
      } else {
        pending = false;
      }
    } catch (error) {
      pending = false;
      if (!disposed && !(error instanceof vscode.CancellationError)) handleError(error);
    } finally {
      running = false;
      if (disposed || services.cli.watchMutationActive) {
        pending = false;
      } else if (pending) {
        pending = false;
        void update();
      }
    }
  };
  const subscription = vscode.workspace.onDidSaveTextDocument((document) => {
    if (!workspaceGraphMutationAllowed(vscode.workspace.isTrusted)) return;
    if (disposed || services.cli.watchMutationActive) return;
    const state = services.store.state;
    const configured = vscode.workspace.getConfiguration('graphoxide', document.uri).get<boolean>('updateOnSave', false);
    const managed = state ? services.managed.freshness(state.folder) === 'save' : false;
    if (!state || (!configured && !managed)) return;
    const relative = safeRelativePath(state.folder.uri.fsPath, document.uri.fsPath);
    if (!relative) return;
    try {
      const outputRelative = path.relative(services.store.managedOutput(state.folder).outputDirectory, document.uri.fsPath);
      if (!outputRelative || (!outputRelative.startsWith(`..${path.sep}`) && outputRelative !== '..' && !path.isAbsolute(outputRelative))) return;
    } catch (error) {
      handleError(error);
      return;
    }
    if (timer) clearTimeout(timer);
    const delay = vscode.workspace.getConfiguration('graphoxide', document.uri).get<number>('updateOnSaveDelay', 1200);
    timer = setTimeout(() => {
      timer = undefined;
      void update();
    }, delay);
  });
  return {
    dispose: () => {
      if (disposed) return;
      disposed = true;
      pending = false;
      if (timer) clearTimeout(timer);
      timer = undefined;
      subscription.dispose();
    },
  };
}

async function requireFolder(store: GraphStore): Promise<vscode.WorkspaceFolder | undefined> {
  const folder = await store.preferredFolder(true);
  if (!folder) void vscode.window.showErrorMessage('Open a folder or workspace to use Graphoxide.');
  return folder;
}

function missingGraph(): void {
  void vscode.window.showInformationMessage('No valid Graphoxide graph was found.', 'Build Graph').then(async (choice) => {
    if (choice) await vscode.commands.executeCommand('graphoxide.initialize');
  });
}

async function revealNodeById(store: GraphStore, id: string): Promise<void> {
  const node = store.state?.model?.getNode(id);
  if (node) await revealNode(store, node);
}

async function revealNode(store: GraphStore, node: GraphNode): Promise<void> {
  const state = store.state;
  if (!state || !node.sourceFile) {
    void vscode.window.showInformationMessage(`${node.label} has no source location.`);
    return;
  }
  const relative = node.sourceFile.replace(/\\/gu, '/');
  if (path.posix.isAbsolute(relative) || relative.split('/').includes('..')) {
    void vscode.window.showErrorMessage(`Graphoxide refused an unsafe source path: ${node.sourceFile}`);
    return;
  }
  const uri = vscode.Uri.joinPath(state.folder.uri, ...relative.split('/'));
  const lineMatch = /^L?(\d+)/u.exec(node.sourceLocation ?? '');
  const requestedLine = Math.max(0, Number.parseInt(lineMatch?.[1] ?? '1', 10) - 1);
  try {
    const document = await vscode.workspace.openTextDocument(uri);
    const line = Math.min(requestedLine, Math.max(0, document.lineCount - 1));
    const editor = await vscode.window.showTextDocument(document, { preview: true });
    const range = document.lineAt(line).range;
    editor.selection = new vscode.Selection(range.start, range.start);
    editor.revealRange(range, vscode.TextEditorRevealType.InCenterIfOutsideViewport);
  } catch (error) {
    throw new Error(`Could not open ${node.sourceFile}: ${error instanceof Error ? error.message : String(error)}`, { cause: error });
  }
}

function nodesMentioned(nodes: readonly GraphNode[], output: string): readonly GraphNode[] {
  const normalized = output.toLocaleLowerCase();
  return nodes.filter((node) => normalized.includes(node.id.toLocaleLowerCase()) || (node.label.length >= 3 && normalized.includes(node.label.toLocaleLowerCase()))).slice(0, 100);
}

function safeRelativePath(root: string, file: string): string | undefined {
  const relative = path.relative(root, file);
  return relative.startsWith('..') || path.isAbsolute(relative) ? undefined : relative.split(path.sep).join('/');
}

function updateStatusBar(item: vscode.StatusBarItem, nodes?: number, edges?: number, error?: string, watching = false): void {
  if (error) {
    item.text = '$(error) Graphoxide';
    item.tooltip = `Graphoxide could not load graph.json: ${error}`;
    item.backgroundColor = new vscode.ThemeColor('statusBarItem.errorBackground');
  } else if (nodes === undefined) {
    item.text = '$(circle-slash) Graphoxide';
    item.tooltip = 'No Graphoxide graph found. Click to get started.';
    item.backgroundColor = undefined;
  } else {
    item.text = `${watching ? '$(eye)' : '$(type-hierarchy)'} Graphoxide ${nodes}`;
    item.tooltip = `${nodes} nodes · ${edges ?? 0} edges${watching ? ' · watch mode active' : ''}`;
    item.backgroundColor = undefined;
  }
}

function handleError(error: unknown): void {
  if (error instanceof vscode.CancellationError) return;
  const message = error instanceof Error ? error.message : String(error);
  void vscode.window.showErrorMessage(`Graphoxide: ${message}`);
}

function isFreshnessMode(value: unknown): value is 'watch' | 'save' | 'manual' {
  return value === 'watch' || value === 'save' || value === 'manual';
}
