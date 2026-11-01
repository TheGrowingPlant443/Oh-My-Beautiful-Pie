import path from 'node:path';
import { pathToFileURL } from 'node:url';
import { installLocalModelPolicy, isLocalModel } from './local-ai-models.mjs';
import { literalAnalysisPrompt } from './omp-visualization-guard.mjs';

// This is a source-SDK root, not the CLI: --no-tools alone still discovers MCP.
export async function runUtility(sdkCli, argv) {
  if (argv[0] !== '--settings-snapshot' || !argv[1]) throw new Error('Utility generation requires a private settings snapshot');
  const snapshot = await Bun.file(argv[1]).json();
  if (snapshot?.version !== 1 || typeof snapshot.cwd !== 'string' || !snapshot.overrides || typeof snapshot.overrides !== 'object' || Array.isArray(snapshot.overrides)) {
    throw new Error('Invalid utility settings snapshot');
  }
  argv = argv.slice(2);
  const sdk = path.dirname(path.dirname(path.resolve(sdkCli)));
  const load = file => import(pathToFileURL(path.join(sdk, file)).href);
  const { parseArgs } = await load('src/cli/args.ts');
  const parsed = parseArgs(argv);
  if (parsed.unrecognizedFlags.length || parsed.invalidFlagValues.length || !parsed.print ||
      !parsed.noSession || !parsed.noTools || !parsed.noExtensions || !parsed.noSkills ||
      !parsed.noRules || !parsed.noLsp || !parsed.noTitle || !parsed.model ||
      parsed.extensions?.length || parsed.hooks?.length || parsed.trustedExtensions?.length || parsed.tools?.length) {
    throw new Error('Utility generation requires a fixed model and a tool-free, non-persistent print invocation');
  }
  const dirs = await import(pathToFileURL(Bun.resolveSync('@oh-my-pi/pi-utils/dirs', sdk)).href);
  dirs.setProfile(parsed.profile || 'default');
  const cwd = path.resolve(parsed.cwd || process.cwd());
  process.chdir(cwd);
  process.env.PI_NO_TITLE = '1';
  process.env.PI_NO_PTY = '1';
  const { Settings } = await load('src/config/settings.ts');
  const { createAgentSession, discoverAuthStorage } = await load('src/sdk.ts');
  const { ModelRegistry } = await load('src/config/model-registry.ts');
  const { resolveCliModel } = await load('src/config/model-resolver.ts');
  const { SessionManager } = await load('src/session/session-manager.ts');
  const { processFileArguments } = await load('src/cli/file-processor.ts');
  const { buildInitialMessage } = await load('src/cli/initial-message.ts');
  const { runPrintMode } = await load('src/modes/print-mode.ts');
  const sourceSettings = Settings.isolated(snapshot.overrides);
  await sourceSettings.reloadForCwd(snapshot.cwd);
  const settings = sourceSettings.overlay({ 'retry.modelFallback': false, 'contextPromotion.enabled': false });
  const auth = await discoverAuthStorage(dirs.getAgentDir(), { settings, cwd });
  let session;
  try {
    const models = new ModelRegistry(auth, path.join(dirs.getAgentDir(), 'models.yml'), { settings });
    await models.hydrateCredentialScopedModelCaches();
    const resolved = resolveCliModel({ cliModel: parsed.model, modelRegistry: models, settings });
    if (resolved.error || !resolved.model || `${resolved.model.provider}/${resolved.model.id}` !== parsed.model) {
      throw new Error(resolved.error || 'Utility model must be an exact provider/model selector');
    }
    if (isLocalModel(resolved.model)) installLocalModelPolicy(ModelRegistry, Settings, settings);
    settings.overrideModelRoles({ default: parsed.model });
    const files = parsed.fileArgs.length ? await processFileArguments(parsed.fileArgs) : undefined;
    const prompt = buildInitialMessage({ parsed, fileText: files?.text, fileImages: files?.images });
    ({ session } = await createAgentSession({
      cwd, agentDir: dirs.getAgentDir(), settings, authStorage: auth, modelRegistry: models,
      model: resolved.model, thinkingLevel: resolved.thinkingLevel, rebindModelAfterDiscovery: false,
      sessionManager: SessionManager.inMemory(cwd),
      deadline: parsed.maxTime === undefined ? undefined : Date.now() + parsed.maxTime * 1000,
      systemPrompt: parsed.systemPrompt === undefined ? undefined : literalAnalysisPrompt(parsed.systemPrompt),
      appendSystemPrompt: parsed.appendSystemPrompt === undefined ? undefined : literalAnalysisPrompt(parsed.appendSystemPrompt),
      toolNames: [], customTools: [], restrictToolNames: true, enableMCP: false,
      disableExtensionDiscovery: true, extensions: [], preloadedExtensionPaths: [],
      preloadedCustomToolPaths: [], skills: [], rules: [], contextFiles: [],
      promptTemplates: [], slashCommands: [], enableLsp: false, enableIrc: false,
      skipPythonPreflight: true, suppressAutoPrewalk: true, autoTitle: false, cacheWarming: false,
      hasUI: false,
    }));
    return await runPrintMode(session, {
      mode: 'text', ...prompt,
      initialMessage: prompt.initialMessage === undefined ? undefined : literalAnalysisPrompt(prompt.initialMessage),
      messages: parsed.messages.map(literalAnalysisPrompt),
    });
  } finally {
    try {
      if (session && !session.isDisposed) await session.dispose();
    } finally { auth.close(); }
  }
}

if (import.meta.main) {
  try {
    const [sdkCli, ...argv] = process.argv.slice(2);
    if (!sdkCli) throw new Error('Utility generation requires the source SDK CLI path');
    process.exitCode = await runUtility(sdkCli, argv);
  } catch (error) {
    process.stderr.write(`${error instanceof Error ? error.message : String(error)}\n`);
    process.exitCode = 1;
  }
}
