import { constants } from 'node:fs';
import { open, realpath } from 'node:fs/promises';
import * as path from 'node:path';
import * as vscode from 'vscode';
import { GraphoxideCli, RunResult } from './cli';
import { GraphStore } from './store';
import {
  MAX_WIKI_INDEX_BYTES,
  MAX_WIKI_PROFILE_BYTES,
  parseWikiAuthoringProfile,
  parseWikiBuildSummary,
  parseWikiSources,
  validateWikiHttpsUrl,
  WikiConsent,
  WikiBuildSummary,
  WikiSource,
  wikiBuildArguments,
  wikiBuildCompletionMessage,
  wikiConsentArguments,
  wikiModelDisclosure,
  wikiPageRelativePath,
  wikiProjectPath,
} from './wiki-policy';

export interface WikiStatus {
  readonly initialized: boolean;
  readonly sources: readonly WikiSource[];
  readonly previewing: boolean;
  readonly error?: string;
}

/** Project-scoped direct-source authoring; credentials remain in the CLI provider environment. */
export class WikiService implements vscode.Disposable {
  private readonly changeEmitter = new vscode.EventEmitter<void>();
  readonly onDidChange = this.changeEmitter.event;
  private readonly subscriptions: vscode.Disposable[] = [];
  private readonly previews = new Map<string, vscode.Terminal>();
  private active = false;

  constructor(
    private readonly context: vscode.ExtensionContext,
    private readonly cli: GraphoxideCli,
    private readonly store: GraphStore,
  ) {
    const watcher = vscode.workspace.createFileSystemWatcher('**/sources/index.json');
    this.subscriptions.push(watcher, watcher.onDidChange(() => this.changeEmitter.fire()),
      watcher.onDidCreate(() => this.changeEmitter.fire()), watcher.onDidDelete(() => this.changeEmitter.fire()),
      vscode.window.onDidCloseTerminal((terminal) => {
        for (const [key, preview] of this.previews) {
          if (preview === terminal) { this.previews.delete(key); this.changeEmitter.fire(); }
        }
      }));
  }

  dispose(): void {
    for (const terminal of this.previews.values()) terminal.dispose();
    this.previews.clear();
    for (const subscription of this.subscriptions) subscription.dispose();
    this.changeEmitter.dispose();
  }

  async status(folder?: vscode.WorkspaceFolder): Promise<WikiStatus> {
    const previewing = Boolean(folder && this.previews.has(folder.uri.toString()));
    if (!folder || folder.uri.scheme !== 'file') return { initialized: false, sources: [], previewing };
    try {
      const sources = parseWikiSources(await this.readProjectFile(folder, 'sources/index.json', MAX_WIKI_INDEX_BYTES));
      return { initialized: true, sources, previewing };
    } catch (error) {
      if (isMissing(error)) return { initialized: false, sources: [], previewing };
      return { initialized: true, sources: [], previewing, error: errorMessage(error) };
    }
  }

  async initialize(): Promise<void> {
    await this.exclusive(async () => {
      const folder = await this.requireFolder();
      if (folder) await this.ensureInitialized(folder);
    });
  }

  async build(): Promise<void> {
    await this.exclusive(async () => {
      const folder = await this.requireFolder();
      if (!folder || !await this.ensureInitialized(folder)) return;
      const kind = await vscode.window.showQuickPick([
        { label: 'Choose files…', sourceKind: 'files', description: 'Generate pages from selected local documents or source files' },
        ...(process.platform !== 'win32' ? [{ label: 'Choose folder…', sourceKind: 'folder', description: 'Generate pages from supported sources in a selected folder' }] : []),
        { label: 'Add HTTPS source…', sourceKind: 'https', description: 'Fetch one explicitly selected public URL' },
      ], { title: 'Build Wiki: choose sources' });
      if (!kind) return;
      let inputs: string[];
      if (kind.sourceKind === 'https') {
        const url = await vscode.window.showInputBox({ title: 'Wiki source URL', prompt: 'A public HTTPS URL without query parameters', validateInput: validateWikiHttpsUrl });
        if (!url) return;
        inputs = [url];
      } else {
        const selected = await vscode.window.showOpenDialog({
          title: kind.sourceKind === 'files' ? 'Choose Wiki source files' : 'Choose a Wiki source folder',
          defaultUri: folder.uri, canSelectFiles: kind.sourceKind === 'files', canSelectFolders: kind.sourceKind === 'folder', canSelectMany: kind.sourceKind === 'files',
        });
        if (!selected?.length) return;
        if (selected.some((uri) => uri.scheme !== 'file')) throw new Error('Wiki sources must be local files.');
        inputs = selected.map((uri) => uri.fsPath);
      }
      const consent = await this.authoringConsent(folder, 'Build Wiki', inputs.some((input) => input.startsWith('https://')), inputs);
      if (!consent) return;
      const summary = await this.addSources(folder, inputs, consent);
      const message = wikiBuildCompletionMessage(summary);
      if (summary.sourceErrors > 0) void vscode.window.showWarningMessage(message);
      else void vscode.window.showInformationMessage(message);
    });
  }

  /** Shared command path, also used by the Extension Host integration tests. */
  async addSources(folder: vscode.WorkspaceFolder, inputs: readonly string[], consent: WikiConsent): Promise<WikiBuildSummary> {
    this.requireTrust();
    const result = await this.run(folder, 'Graphoxide: building Wiki…', wikiBuildArguments(inputs, consent));
    return parseWikiBuildSummary(result.stdout);
  }

  /** Initialize the selected profile through the same validated path as the setup wizard. */
  async initializeFromProfile(folder: vscode.WorkspaceFolder, profilePath: string): Promise<void> {
    this.requireTrust();
    const relative = path.relative(folder.uri.fsPath, profilePath).split(path.sep).join('/');
    parseWikiAuthoringProfile(await this.readProjectFile(folder, relative, MAX_WIKI_PROFILE_BYTES), folder.uri.fsPath);
    await this.run(folder, 'Graphoxide: initializing Wiki…', ['wiki', 'init', '--authoring-profile', profilePath]);
    await this.context.workspaceState.update(`graphoxide.wiki.authoringProfile:${folder.uri.toString()}`, relative);
  }

  async manageSources(): Promise<void> {
    await this.exclusive(async () => {
      const folder = await this.requireFolder();
      if (!folder) return;
      const status = await this.status(folder);
      if (status.error) throw new Error(status.error);
      if (!status.sources.length) {
        void vscode.window.showInformationMessage('No Wiki sources yet. Use Graphoxide: Build Wiki from Sources.');
        return;
      }
      const picked = await vscode.window.showQuickPick(status.sources.map((source) => ({
        label: source.label, description: source.status.replaceAll('-', ' '), detail: source.id, source,
      })), { title: 'Wiki sources', matchOnDescription: true, matchOnDetail: true });
      if (!picked) return;
      const source = picked.source;
      const actions = [
        { label: 'Open generated page', action: 'open' },
        { label: 'Refresh source and rebuild page…', action: 'refresh' },
        { label: 'Review page with AI…', action: 'review' },
        ...(source.status === 'ai-reviewed' ? [{ label: 'Confirm reviewed page…', action: 'confirm' }] : []),
        { label: 'Retire source…', action: 'retire' },
      ];
      const action = await vscode.window.showQuickPick(actions, { title: source.label, placeHolder: `Status: ${source.status.replaceAll('-', ' ')}` });
      if (!action) return;
      if (action.action === 'open') { await this.openPage(folder, source); return; }
      let args = ['wiki', 'source', action.action, source.id];
      if (action.action === 'refresh' || action.action === 'review') {
        const consent = await this.authoringConsent(folder, action.action === 'review' ? 'Review Wiki page' : 'Refresh Wiki source', source.remote, [source.label]);
        if (!consent) return;
        args = [...args, ...wikiConsentArguments(consent, source.remote)];
      } else {
        if (action.action === 'confirm') await this.openPage(folder, source);
        const label = action.action === 'retire' ? 'Retire source' : 'Confirm page';
        const detail = action.action === 'retire'
          ? 'Remove this source pointer and its generated Wiki artifacts. The original source is preserved.'
          : 'Record your human confirmation of this AI-reviewed page. Read the generated page before confirming.';
        if (await vscode.window.showWarningMessage(`${label}: ${source.label}?`, { modal: true, detail }, label) !== label) return;
      }
      await this.run(folder, `Graphoxide: ${action.action} Wiki source…`, args);
    });
  }

  async preview(): Promise<void> {
    const folder = await this.requireFolder();
    if (!folder) return;
    const status = await this.status(folder);
    if (status.error) throw new Error(status.error);
    if (!status.initialized) { void vscode.window.showInformationMessage('Build a Wiki before starting its preview.'); return; }
    const existing = this.previews.get(folder.uri.toString());
    if (existing) { existing.show(); return; }
    const invocation = this.cli.trustedInvocation(folder);
    const terminal = vscode.window.createTerminal({
      name: `Graphoxide Wiki: ${folder.name}`, shellPath: invocation.command,
      shellArgs: [...invocation.args, 'wiki', 'live', folder.uri.fsPath, '--open'], cwd: folder.uri,
    });
    this.previews.set(folder.uri.toString(), terminal);
    terminal.show();
    this.changeEmitter.fire();
  }

  async stopPreview(): Promise<void> {
    const folder = await this.store.preferredFolder();
    if (!folder) return;
    this.previews.get(folder.uri.toString())?.dispose();
    this.previews.delete(folder.uri.toString());
    this.changeEmitter.fire();
  }

  private async ensureInitialized(folder: vscode.WorkspaceFolder): Promise<boolean> {
    const status = await this.status(folder);
    if (status.error) throw new Error(status.error);
    if (status.initialized) return true;
    const selected = await vscode.window.showOpenDialog({
      title: 'Initialize Wiki: select an existing authoring profile in this Git workspace',
      defaultUri: folder.uri, canSelectFiles: true, canSelectFolders: false, canSelectMany: false, filters: { JSON: ['json'] },
    });
    const profile = selected?.[0];
    if (!profile) return false;
    if (profile.scheme !== 'file') throw new Error('Select a local Wiki authoring profile.');
    const relative = path.relative(folder.uri.fsPath, profile.fsPath).split(path.sep).join('/');
    const details = parseWikiAuthoringProfile(await this.readProjectFile(folder, relative, MAX_WIKI_PROFILE_BYTES), folder.uri.fsPath);
    const choice = await vscode.window.showInformationMessage(`Initialize Wiki in ${folder.name}?`, {
      modal: true,
      detail: `Workspace: ${folder.uri.fsPath}\nAuthor model: ${details.authorModel}\nReviewer model: ${details.reviewerModel}\nProvider profile: ${details.providerProfile}\n\nCreates sources, taxonomy, and authoring configuration in this Git worktree. Model credentials must already be available through the provider profile's environment variable.`,
    }, 'Initialize Wiki');
    if (choice !== 'Initialize Wiki') return false;
    await this.initializeFromProfile(folder, profile.fsPath);
    return true;
  }

  private async authoringConsent(folder: vscode.WorkspaceFolder, title: string, remote: boolean, inputs: readonly string[]): Promise<WikiConsent | undefined> {
    const profile = parseWikiAuthoringProfile(await this.readProjectFile(folder, 'config/authoring-profile.json', MAX_WIKI_PROFILE_BYTES), folder.uri.fsPath);
    const model = title.startsWith('Review') ? profile.reviewerModel : profile.authorModel;
    const disclosure = wikiModelDisclosure(await this.readProjectFile(folder, profile.providerProfile, MAX_WIKI_PROFILE_BYTES), profile.providerProfile, model);
    if (disclosure.reviewFile) {
      await vscode.window.showTextDocument(vscode.Uri.file(wikiProjectPath(folder.uri.fsPath, profile.providerProfile)), { preview: true });
    }
    const detail = `Sources:\n${inputs.join('\n')}\n\nSend source text to ${disclosure.description}. This can incur provider charges. Generated pages are written inside this workspace.${remote ? '\n\nThis also permits fetching the selected HTTPS source for this operation.' : ''}`;
    const label = remote ? 'Allow fetch and model use' : 'Allow model use';
    if (await vscode.window.showWarningMessage(`${title}?`, { modal: true, detail }, label) !== label) return undefined;
    return { allowModelEgress: true, allowNetwork: remote };
  }

  private async openPage(folder: vscode.WorkspaceFolder, source: WikiSource): Promise<void> {
    const relative = wikiPageRelativePath(source);
    await this.readProjectFile(folder, relative, MAX_WIKI_PROFILE_BYTES);
    await vscode.window.showTextDocument(vscode.Uri.file(wikiProjectPath(folder.uri.fsPath, relative)), { preview: true });
  }

  private async run(folder: vscode.WorkspaceFolder, title: string, args: readonly string[]): Promise<RunResult> {
    this.requireTrust();
    try {
      return await this.cli.run({ title, folder, args, cancellable: true, trustedExecutable: true });
    } finally {
      this.changeEmitter.fire();
    }
  }

  private async exclusive(action: () => Promise<void>): Promise<void> {
    if (this.active) { void vscode.window.showInformationMessage('A Wiki operation is already running. Wait for it to finish or cancel it.'); return; }
    this.active = true;
    try { await action(); } finally { this.active = false; }
  }

  private requireTrust(): void {
    if (!vscode.workspace.isTrusted) throw new Error('Trust this workspace before running Wiki commands.');
  }

  private async requireFolder(): Promise<vscode.WorkspaceFolder | undefined> {
    this.requireTrust();
    const folder = await this.store.preferredFolder();
    if (!folder) { void vscode.window.showInformationMessage('Open a Git workspace to build a Wiki.'); return undefined; }
    if (folder.uri.scheme !== 'file') throw new Error('Wiki commands require a local Git workspace.');
    return folder;
  }

  private async readProjectFile(folder: vscode.WorkspaceFolder, relative: string, maxBytes: number): Promise<string> {
    const root = await realpath(folder.uri.fsPath);
    const target = wikiProjectPath(root, relative);
    const actual = await realpath(target);
    if (actual !== target) throw new Error('Wiki metadata and generated pages must not use symbolic links.');
    const file = await open(target, constants.O_RDONLY | constants.O_NOFOLLOW);
    try {
      const stat = await file.stat();
      if (!stat.isFile() || stat.size > maxBytes) throw new Error('Wiki metadata is not a regular file within the size limit.');
      const buffer = Buffer.alloc(Math.min(stat.size + 1, maxBytes + 1));
      const { bytesRead } = await file.read(buffer, 0, buffer.length, 0);
      if (bytesRead > maxBytes || bytesRead !== stat.size) throw new Error('Wiki metadata changed while reading or exceeds its size limit.');
      return buffer.subarray(0, bytesRead).toString('utf8');
    } finally { await file.close(); }
  }
}

function isMissing(error: unknown): boolean {
  return typeof error === 'object' && error !== null && 'code' in error && error.code === 'ENOENT';
}

function errorMessage(error: unknown): string { return error instanceof Error ? error.message : String(error); }
