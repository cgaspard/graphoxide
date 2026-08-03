import * as path from 'node:path';
import * as vscode from 'vscode';
import { GraphoxideCli } from '../cli';
import { GraphStore } from '../store';
import {
  AI_PROVIDER_PROFILES,
  AiLabelingConfiguration,
  AiProviderProfile,
  aiProviderById,
  aiSecretKey,
  apiKeyRequired,
  credentialForEndpoint,
  encodeStoredCredential,
  labelingArguments,
  labelingEnvironment,
  modelDiscoveryUrl,
  normalizeProviderBaseUrl,
  parseDiscoveredModels,
} from './config';

export interface AiLabelingTestConfiguration {
  readonly provider: string;
  readonly baseUrl: string;
  readonly model: string;
  readonly apiKey?: string;
  readonly timeoutSeconds?: number;
}

interface ModelDiscovery {
  readonly models: readonly string[];
  readonly error?: string;
}

interface ProviderPick extends vscode.QuickPickItem {
  readonly profile?: AiProviderProfile;
  readonly disable?: true;
}

export class AiLabelingService {
  constructor(
    private readonly context: vscode.ExtensionContext,
    private readonly cli: GraphoxideCli,
    private readonly store: GraphStore,
  ) {}

  configuredProvider(): AiProviderProfile | undefined {
    return aiProviderById(this.settings().get<string>('llm.provider', 'none'));
  }

  async configure(): Promise<boolean> {
    const current = this.configuredProvider();
    const choices: ProviderPick[] = [
      ...AI_PROVIDER_PROFILES.map((profile) => ({
        label: `${current?.id === profile.id ? '$(check) ' : ''}${profile.label}`,
        description: profile.description,
        profile,
      })),
      {
        label: '$(circle-slash) Disable AI community labeling',
        description: 'Keep any stored credentials until explicitly cleared',
        disable: true,
      },
    ];
    const selected = await vscode.window.showQuickPick(choices, {
      title: 'Configure AI community labeling',
      placeHolder: 'Choose the service Graphoxide will use to name graph communities.',
      ignoreFocusOut: true,
    });
    if (!selected) return false;
    if (selected.disable) {
      await this.settings().update('llm.provider', 'none', vscode.ConfigurationTarget.Global);
      void vscode.window.showInformationMessage(
        'AI community labeling is disabled. Stored API keys were not deleted.',
        'Clear stored credential',
      ).then(async (choice) => {
        if (choice === 'Clear stored credential') await vscode.commands.executeCommand('graphoxide.clearAiCredential');
      });
      return true;
    }

    const profile = selected.profile;
    if (!profile) return false;
    const settings = this.settings();
    const sameProfile = current?.id === profile.id;
    const initialBaseUrl = sameProfile
      ? settings.get<string>('llm.baseUrl', profile.defaultBaseUrl)
      : profile.defaultBaseUrl;
    const configuredBaseUrl = profile.editableEndpoint
      ? await vscode.window.showInputBox({
          title: `${profile.label} endpoint`,
          prompt: 'OpenAI-compatible API base URL. HTTP is accepted only for this computer.',
          value: initialBaseUrl,
          placeHolder: profile.defaultBaseUrl || 'https://llm.example.com/v1',
          ignoreFocusOut: true,
          validateInput: (value) => validationMessage(() => normalizeProviderBaseUrl(profile, value)),
        })
      : profile.defaultBaseUrl;
    if (configuredBaseUrl === undefined) return false;
    const baseUrl = normalizeProviderBaseUrl(profile, configuredBaseUrl);

    const secretName = aiSecretKey(profile);
    const storedCredential = await this.context.secrets.get(secretName);
    const existingKey = credentialForEndpoint(storedCredential, baseUrl);
    const keyInput = await vscode.window.showInputBox({
      title: `${profile.label} API key`,
      prompt: existingKey
        ? 'A key is stored for this endpoint. Leave empty to keep it; it may be sent to this endpoint to list models.'
        : apiKeyRequired(profile, baseUrl)
          ? 'Required. The key is stored in VS Code Secret Storage and sent only to this endpoint for model discovery and labeling.'
          : 'Optional for this local endpoint. A key is sent as Bearer authentication; leave empty for keyless access.',
      placeHolder: existingKey ? 'Leave empty to keep the stored key' : 'API key (input hidden)',
      password: true,
      ignoreFocusOut: true,
      validateInput: (value) => apiKeyRequired(profile, baseUrl) && !existingKey && !value.trim()
        ? `${profile.label} requires an API key for this endpoint.`
        : undefined,
    });
    if (keyInput === undefined) return false;
    const newKey = keyInput.trim();
    const effectiveKey = newKey || existingKey;
    if (apiKeyRequired(profile, baseUrl) && !effectiveKey) {
      throw new Error(`${profile.label} requires an API key for this endpoint.`);
    }

    const initialModel = sameProfile
      ? settings.get<string>('llm.model', profile.defaultModel).trim() || profile.defaultModel
      : profile.defaultModel;
    const model = await this.chooseModel(profile, baseUrl, effectiveKey, initialModel);
    if (model === undefined) return false;

    // Store the secret before metadata so a configured provider is never left
    // pointing at a key that failed to persist.
    if (newKey) {
      await this.context.secrets.store(secretName, encodeStoredCredential(baseUrl, newKey));
    } else if (storedCredential && !existingKey) {
      // Discard an endpoint-mismatched or legacy unbound value instead of
      // leaving a credential that a later configuration could accidentally reuse.
      await this.context.secrets.delete(secretName);
    }
    await settings.update('llm.baseUrl', baseUrl, vscode.ConfigurationTarget.Global);
    await settings.update('llm.model', model, vscode.ConfigurationTarget.Global);
    await settings.update('llm.provider', profile.id, vscode.ConfigurationTarget.Global);
    void vscode.window.showInformationMessage(`${profile.label} is configured for Graphoxide community labeling.`);
    return true;
  }

  async clearCredential(): Promise<void> {
    const stored = (await Promise.all(AI_PROVIDER_PROFILES.map(async (profile) => ({
      profile,
      present: Boolean(await this.context.secrets.get(aiSecretKey(profile))),
    })))).filter(({ present }) => present);
    if (stored.length === 0) {
      void vscode.window.showInformationMessage('Graphoxide has no stored AI labeling credentials.');
      return;
    }
    const selected = await vscode.window.showQuickPick(
      stored.map(({ profile }) => ({ label: profile.label, description: 'Stored in VS Code Secret Storage', profile })),
      { title: 'Clear a stored AI labeling credential', ignoreFocusOut: true },
    );
    if (!selected) return;
    const confirmation = await vscode.window.showWarningMessage(
      `Delete the stored ${selected.profile.label} API key?`,
      { modal: true },
      'Delete key',
    );
    if (confirmation !== 'Delete key') return;
    await this.context.secrets.delete(aiSecretKey(selected.profile));
    void vscode.window.showInformationMessage(`${selected.profile.label} API key deleted.`);
  }

  async improveCommunityLabels(): Promise<void> {
    await this.runCommunityLabeling(false);
  }

  async configureForTest(input: AiLabelingTestConfiguration): Promise<readonly string[]> {
    this.requireDevelopmentHost();
    const profile = aiProviderById(input.provider);
    if (!profile) throw new Error(`Unknown AI provider ${input.provider}.`);
    const baseUrl = normalizeProviderBaseUrl(profile, input.baseUrl);
    const model = input.model.trim();
    if (!model) throw new Error(`${profile.label} requires a model ID.`);
    const apiKey = input.apiKey?.trim();
    if (apiKeyRequired(profile, baseUrl) && !apiKey) {
      throw new Error(`${profile.label} requires an API key for this endpoint.`);
    }
    const secretName = aiSecretKey(profile);
    if (apiKey) await this.context.secrets.store(secretName, encodeStoredCredential(baseUrl, apiKey));
    else await this.context.secrets.delete(secretName);
    const settings = this.settings();
    await settings.update('llm.baseUrl', baseUrl, vscode.ConfigurationTarget.Global);
    await settings.update('llm.model', model, vscode.ConfigurationTarget.Global);
    await settings.update(
      'llm.timeoutSeconds',
      clampInteger(input.timeoutSeconds ?? 600, 30, 3600),
      vscode.ConfigurationTarget.Global,
    );
    await settings.update('llm.provider', profile.id, vscode.ConfigurationTarget.Global);
    const discovery = await this.discoverModels(profile, baseUrl, apiKey);
    if (discovery.error) throw new Error(discovery.error);
    return discovery.models;
  }

  async improveCommunityLabelsForTest(): Promise<void> {
    this.requireDevelopmentHost();
    await this.runCommunityLabeling(true);
  }

  async clearTestConfiguration(): Promise<void> {
    this.requireDevelopmentHost();
    await Promise.all(AI_PROVIDER_PROFILES.map((profile) => this.context.secrets.delete(aiSecretKey(profile))));
    const settings = this.settings();
    await settings.update('llm.provider', undefined, vscode.ConfigurationTarget.Global);
    await settings.update('llm.baseUrl', undefined, vscode.ConfigurationTarget.Global);
    await settings.update('llm.model', undefined, vscode.ConfigurationTarget.Global);
    await settings.update('llm.timeoutSeconds', undefined, vscode.ConfigurationTarget.Global);
  }

  private async runCommunityLabeling(skipConfirmation: boolean): Promise<void> {
    if (!vscode.workspace.isTrusted) {
      void vscode.window.showWarningMessage('Trust this workspace before running AI community labeling.');
      return;
    }
    const folder = await this.store.preferredFolder(true);
    if (!folder) {
      void vscode.window.showErrorMessage('Open a folder or workspace to use Graphoxide.');
      return;
    }
    let state = await this.store.load(folder);
    if (!state?.model) {
      const choice = await vscode.window.showInformationMessage(
        'Extract this workspace before naming its communities with AI.',
        'Extract workspace',
      );
      if (choice !== 'Extract workspace') return;
      const graphUri = state?.graphUri ?? this.store.graphUri(folder);
      if (path.basename(graphUri.fsPath) !== 'graph.json') {
        throw new Error('Automatic extraction requires Graphoxide: Graph Path to end in graph.json. Extract the configured graph before labeling it.');
      }
      await this.cli.run({
        title: 'Graphoxide: extracting workspace…',
        folder,
        args: ['extract', folder.uri.fsPath],
        environment: { GRAPHOXIDE_OUT: path.dirname(graphUri.fsPath) },
      });
      state = await this.store.load(folder);
    }
    if (!state?.model) throw new Error(`No graph was found at ${state?.graphUri.fsPath ?? this.store.graphUri(folder).fsPath}.`);

    let configuration = this.configuration();
    if (!configuration) {
      const configured = await this.configure();
      if (!configured) return;
      configuration = this.configuration();
    }
    if (!configuration) return;
    let key = credentialForEndpoint(
      await this.context.secrets.get(aiSecretKey(configuration.profile)),
      configuration.baseUrl,
    );
    if (apiKeyRequired(configuration.profile, configuration.baseUrl) && !key) {
      const choice = await vscode.window.showWarningMessage(
        `${configuration.profile.label} needs an API key before Graphoxide can label communities.`,
        'Configure AI labeling',
      );
      if (choice !== 'Configure AI labeling' || !await this.configure()) return;
      configuration = this.configuration();
      if (!configuration) return;
      key = credentialForEndpoint(
        await this.context.secrets.get(aiSecretKey(configuration.profile)),
        configuration.baseUrl,
      );
      if (apiKeyRequired(configuration.profile, configuration.baseUrl) && !key) return;
    }

    const invocation = this.cli.trustedInvocation(folder);
    const detail = [
      `Endpoint: ${configuration.baseUrl}`,
      `Model: ${configuration.model}`,
      `Request timeout: ${configuration.timeoutSeconds} seconds`,
      `Graph: ${state.graphUri.fsPath}`,
      `Executable: ${invocation.command}`,
      '',
      'Graphoxide sends up to 12 graph node labels per community. Labels can include source-derived identifiers, filenames, and truncated comments or docstrings. Full files and source_file metadata are not included.',
      'This replaces community names in graph.json, writes the label sidecar, and regenerates GRAPH_REPORT.md beside the graph.',
    ].join('\n');
    if (!skipConfirmation) {
      const confirmation = await vscode.window.showWarningMessage(
        `Use ${configuration.profile.label} to replace all Graphoxide community names?`,
        { modal: true, detail },
        'Label communities',
      );
      if (confirmation !== 'Label communities') return;
    }

    await this.cli.run({
      title: `Graphoxide: labeling communities with ${configuration.profile.label}…`,
      folder,
      args: labelingArguments(state.graphUri.fsPath, configuration),
      environment: labelingEnvironment(configuration.profile, configuration.baseUrl, key),
      trustedExecutable: true,
    });
    await this.store.load(folder);
    const report = vscode.Uri.file(path.join(path.dirname(state.graphUri.fsPath), 'GRAPH_REPORT.md'));
    if (!skipConfirmation) {
      const choice = await vscode.window.showInformationMessage(
        'Graphoxide community names and architecture report were updated.',
        'Open report',
      );
      if (choice === 'Open report') await vscode.window.showTextDocument(report, { preview: false });
    }
  }

  private configuration(): AiLabelingConfiguration | undefined {
    const profile = this.configuredProvider();
    if (!profile) return undefined;
    const settings = this.settings();
    const baseUrl = normalizeProviderBaseUrl(profile, settings.get<string>('llm.baseUrl', profile.defaultBaseUrl));
    const model = settings.get<string>('llm.model', profile.defaultModel).trim() || profile.defaultModel;
    if (!model) throw new Error(`${profile.label} requires a model ID.`);
    return {
      profile,
      baseUrl,
      model,
      maxConcurrency: clampInteger(settings.get<number>('llm.maxConcurrency', 4), 1, 32),
      batchSize: clampInteger(settings.get<number>('llm.batchSize', 100), 1, 1000),
      timeoutSeconds: clampInteger(settings.get<number>('llm.timeoutSeconds', 600), 30, 3600),
    };
  }

  private async chooseModel(
    profile: AiProviderProfile,
    baseUrl: string,
    apiKey: string | undefined,
    initialModel: string,
  ): Promise<string | undefined> {
    const discovery = await this.discoverModels(profile, baseUrl, apiKey);
    if (discovery.error) void vscode.window.showWarningMessage(discovery.error);
    if (discovery.models.length > 0) {
      const models = initialModel && !discovery.models.includes(initialModel)
        ? [initialModel, ...discovery.models]
        : discovery.models;
      const selected = await vscode.window.showQuickPick([
        ...models.map((model) => ({ label: model, model })),
        { label: '$(edit) Enter another model ID…', model: undefined },
      ], {
        title: `${profile.label} model`,
        placeHolder: `${discovery.models.length} model${discovery.models.length === 1 ? '' : 's'} discovered`,
        ignoreFocusOut: true,
      });
      if (!selected) return undefined;
      if (selected.model) return selected.model;
    }
    const entered = await vscode.window.showInputBox({
      title: `${profile.label} model`,
      prompt: 'Enter the exact model ID exposed by this endpoint.',
      value: initialModel,
      placeHolder: profile.defaultModel || 'model-id',
      ignoreFocusOut: true,
      validateInput: (value) => value.trim() ? undefined : 'A model ID is required.',
    });
    return entered?.trim() || undefined;
  }

  private async discoverModels(
    profile: AiProviderProfile,
    baseUrl: string,
    apiKey: string | undefined,
  ): Promise<ModelDiscovery> {
    const url = modelDiscoveryUrl(profile, baseUrl);
    if (!url) return { models: [] };
    const controller = new AbortController();
    const timeout = setTimeout(() => controller.abort(), 4000);
    try {
      const headers: Record<string, string> = { Accept: 'application/json' };
      if (apiKey) headers.Authorization = `Bearer ${apiKey}`;
      const response = await fetch(url, { headers, signal: controller.signal });
      if (response.status === 401 || response.status === 403) {
        return { models: [], error: `${profile.label} rejected the stored API key while listing models.` };
      }
      if (!response.ok) {
        return { models: [], error: `${profile.label} model discovery returned HTTP ${response.status}. Enter a model ID manually.` };
      }
      const payload: unknown = await response.json();
      return { models: parseDiscoveredModels(profile, payload) };
    } catch (error) {
      const timedOut = controller.signal.aborted;
      const reason = timedOut ? 'timed out' : error instanceof Error ? error.message : String(error);
      return { models: [], error: `Could not list ${profile.label} models (${reason}). Enter a model ID manually.` };
    } finally {
      clearTimeout(timeout);
    }
  }

  private settings(): vscode.WorkspaceConfiguration {
    // AI provider metadata is intentionally global/machine-scoped so an
    // untrusted repository cannot select an endpoint or model.
    return vscode.workspace.getConfiguration('graphoxide');
  }

  private requireDevelopmentHost(): void {
    if (this.context.extensionMode === vscode.ExtensionMode.Production) {
      throw new Error('Graphoxide AI test controls are unavailable in a production extension host.');
    }
  }
}

function validationMessage(operation: () => unknown): string | undefined {
  try {
    operation();
    return undefined;
  } catch (error) {
    return error instanceof Error ? error.message : String(error);
  }
}

function clampInteger(value: number, minimum: number, maximum: number): number {
  return Math.min(maximum, Math.max(minimum, Number.isFinite(value) ? Math.trunc(value) : minimum));
}
