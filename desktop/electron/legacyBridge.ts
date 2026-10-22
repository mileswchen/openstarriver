// FPM acceptance #4: the built-in UI goes through the protocol client.
//
// The preload's typed methods (window.starriver.listSessions etc.) build
// protocol envelopes inside the bridge and dispatch via the generic
// `runtime:invoke`, so the UI and UI tests need zero changes; this table is the
// single source of truth for "legacy method name → protocol method +
// param/result mapping" (the daemon compatibility layer's 87-entry proxy table
// was merged here and then deleted).
//
// Result shape: protocol results are already camelCase, matching both the
// app.tsx interface declarations and the actual reads, so they pass through
// untouched; only a few methods unwrap a sub-field (diff/path/content/cancelled).

export interface LegacySpec {
  /** Protocol method name. */
  method: string;
  /** Positional args → named protocol params. */
  params?: (args: any[]) => Record<string, unknown>;
  /** Protocol result → legacy result (defaults to pass-through). */
  result?: (result: any) => any;
}

export const PROTOCOL_VERSION = 1;
let requestSeq = 0;

/** Build a protocol request envelope (monotonic idempotent ids). */
export function protocolRequest(
  method: string,
  params: unknown,
): { v: number; id: string; method: string; params: unknown } {
  return {
    v: PROTOCOL_VERSION,
    id: `req_${Date.now().toString(36)}_${++requestSeq}`,
    method,
    params: params ?? {},
  };
}

/** Parse the protocol response JSON returned by `runtime:invoke`; throws a
 *  coded Error on failure. */
export function parseProtocolResponse(text: string): any {
  const response = JSON.parse(text);
  if (!response.ok) {
    throw Object.assign(
      new Error(response.error?.message || `protocol error ${response.error?.code ?? 'UNKNOWN'}`),
      response.error,
    );
  }
  return response.result;
}

function parseJson(raw: unknown, fallback: unknown): unknown {
  if (typeof raw !== 'string' || raw === '') return fallback;
  return JSON.parse(raw);
}

/** Strict variant for user-authored JSON args: a malformed payload is an
 * error (BUG-12 convention), never a silently-empty list. */
function parseJsonStrict(name: string, raw: unknown): unknown {
  if (typeof raw !== 'string' || raw === '') return undefined;
  try {
    return JSON.parse(raw);
  } catch (e) {
    throw new Error(`invalid ${name} JSON: ${e instanceof Error ? e.message : e}`);
  }
}

/** legacy typed method → protocol method mapping (covers every channel the current UI uses). */
export const LEGACY_METHODS: Record<string, LegacySpec> = {
  listTools: { method: 'tools.list' },
  listAgents: { method: 'agents.list' },
  respondQuestion: {
    method: 'interaction.respond',
    params: ([sessionId, questionId, answer]) => ({ sessionId, questionId, answer }),
  },
  scanAgents: { method: 'agents.scan' },
  createAgent: {
    method: 'agents.create',
    params: ([name, description]) => ({ name, description }),
  },
  exportAgent: { method: 'agents.export', params: ([name]) => ({ name }) },
  listSkills: { method: 'skills.list' },
  scanSkills: { method: 'skills.scan' },
  createSkill: {
    method: 'skills.create',
    params: ([name, description, command, body]) => ({ name, description, command, body }),
  },
  updateSkill: {
    method: 'skills.update',
    params: ([name, description, command, body]) => ({ name, description, command, body }),
  },
  deleteSkill: { method: 'skills.delete', params: ([name]) => ({ name }) },
  setSkillAlwaysOn: {
    method: 'skills.setAlwaysOn',
    params: ([name, alwaysOn]) => ({ name, alwaysOn }),
  },
  listSkillUsage: { method: 'skills.usage', params: ([limit]) => ({ limit }) },
  listRunningSessions: { method: 'sessions.running' },
  listHooks: { method: 'hooks.list' },
  readHook: {
    method: 'hooks.read',
    params: ([event, name]) => ({ event, name }),
    result: (r) => r?.content,
  },
  writeHook: {
    method: 'hooks.write',
    params: ([event, name, content]) => ({ event, name, content }),
  },
  deleteHook: {
    method: 'hooks.delete',
    params: ([event, name]) => ({ event, name }),
  },
  compactSession: {
    method: 'session.compact',
    params: ([sessionId, keepRecent]) => ({ sessionId, keepRecent }),
  },
  sessionUsage: { method: 'session.usage', params: ([sessionId]) => ({ sessionId }) },
  workspaceDiff: {
    method: 'workspace.diff',
    params: ([cwd]) => ({ cwd }),
    result: (r) => r?.diff,
  },
  initProjectAgents: {
    method: 'project.agents.init',
    params: ([cwd]) => ({ cwd }),
    result: (r) => r?.result,
  },
  exportSession: {
    method: 'session.export',
    params: ([sessionId, cwd]) => ({ sessionId, cwd }),
    result: (r) => r?.path,
  },
  listPlugins: { method: 'plugins.list' },
  installPlugin: { method: 'plugins.install', params: ([sourceUrl]) => ({ source: sourceUrl }) },
  uninstallPlugin: { method: 'plugins.uninstall', params: ([name]) => ({ name }) },
  listModelProviders: { method: 'models.providers.list' },
  listModels: { method: 'models.list' },
  removeModel: { method: 'models.remove', params: ([id]) => ({ id }) },
  addModel: {
    method: 'models.add',
    params: ([
      id, name, modelType, providerType, baseUrl, apiKey, modelPath, defaultModel,
      variantsJson, concurrency, maxTokens, contextWindow, temperature, apiFormat, reasoningEffort,
      modelLabel, supportsVision,
    ]) => ({
      id, name, modelType, providerType,
      baseUrl: baseUrl ?? undefined,
      apiKey: apiKey ?? undefined,
      modelPath: modelPath ?? undefined,
      defaultModel,
      variants: parseJsonStrict('variants', variantsJson),
      concurrency: concurrency ?? undefined,
      maxTokens: maxTokens ?? undefined,
      contextWindow: contextWindow ?? undefined,
      temperature: temperature ?? undefined,
      apiFormat: apiFormat ?? undefined,
      reasoningEffort: reasoningEffort ?? undefined,
      modelLabel: modelLabel ?? undefined,
      supportsVision: supportsVision ?? undefined,
    }),
  },
  listAgentModelBindings: { method: 'agents.bindings' },
  setAgentActiveModel: {
    method: 'agents.bindings.setActive',
    params: ([agentName, providerId, variantId]) => ({ agentName, providerId, variantId }),
  },
  setAgentReasoningEffort: {
    method: 'agents.bindings.setReasoningEffort',
    params: ([agentName, effort]) => ({ agentName, effort }),
  },
  listSessions: {
    method: 'session.list',
    params: ([workspaceId]) => ({ workspaceId: workspaceId ?? undefined }),
  },
  getSessionMessages: {
    method: 'session.messages',
    params: ([sessionId]) => ({ sessionId }),
  },
  getSessionTimeline: {
    method: 'session.timeline',
    params: ([sessionId]) => ({ sessionId }),
  },
  forkSession: {
    method: 'session.fork',
    params: ([sourceId, beforeMsgId, task]) => ({ sourceId, beforeMsgId, task }),
  },
  deleteSession: { method: 'session.delete', params: ([sessionId]) => ({ sessionId }) },
  renameSession: {
    method: 'session.rename',
    params: ([sessionId, title]) => ({ sessionId, title }),
  },
  archiveSession: { method: 'session.archive', params: ([sessionId]) => ({ sessionId }) },
  unarchiveSession: { method: 'session.unarchive', params: ([sessionId]) => ({ sessionId }) },
  assignSessionWorkspace: { method: 'session.attachWorkspace', params: ([sessionId, workspaceId]) => ({ sessionId, workspaceId }) },
  listArchivedSessions: { method: 'session.listArchived' },
  listWorkspaces: { method: 'workspaces.list' },
  createWorkspace: { method: 'workspaces.create', params: ([path]) => ({ path }) },
  renameWorkspace: {
    method: 'workspaces.rename',
    params: ([workspaceId, title]) => ({ workspaceId, title }),
  },
  deleteWorkspace: {
    method: 'workspaces.delete',
    params: ([workspaceId]) => ({ workspaceId }),
  },
  setActiveWorkspace: {
    method: 'workspaces.activate',
    // Workspace activation is per agent; the desktop views are Coding-scoped.
    params: ([workspaceId, agent]) => ({
      workspaceId: workspaceId ?? undefined,
      agent: agent ?? 'Starriver Code',
    }),
  },
  getActiveWorkspace: {
    method: 'workspaces.active',
    params: ([agent]) => ({ agent: agent ?? 'Starriver Code' }),
    // The protocol returns { workspaceId }; unwrap it to string | null per the typed interface.
    result: (r) => (r && typeof r === 'object' ? (r as any).workspaceId ?? null : r ?? null),
  },
  getWorktreeStatus: {
    method: 'workspace.worktreeStatus',
    params: ([cwd]) => ({ cwd }),
  },
  listMemories: { method: 'memory.list', params: ([projectId]) => ({ projectId }) },
  recallMemories: {
    method: 'memory.recall',
    params: ([projectId, query]) => ({ projectId: projectId ?? undefined, query }),
  },
  upsertMemory: {
    method: 'memory.upsert',
    params: ([projectId, name, description, body]) => ({
      projectId: projectId ?? undefined,
      name,
      description,
      body,
    }),
  },
  memoryStatus: { method: 'memory.status' },
  ingestMemories: { method: 'memory.ingest' },
  exportMemories: { method: 'memory.export', params: ([path]) => ({ path }) },
  deleteMemory: {
    method: 'memory.delete',
    params: ([projectId, name]) => ({ projectId, name }),
  },
  clearMemories: { method: 'memory.clear', params: ([projectId]) => ({ projectId }) },
  startCodeReview: {
    method: 'review.start',
    params: ([filesJson, dimensionsJson]) => ({
      // Malformed JSON must error (a typo silently reviewed ZERO files).
      files: (parseJsonStrict('files', filesJson) as string[] | undefined) ?? [],
      dimensions: parseJsonStrict('dimensions', dimensionsJson) as string[] | undefined,
    }),
  },
  listMcpServers: { method: 'mcp.list' },
  addMcpServer: {
    method: 'mcp.add',
    params: ([name, transport, command, argsJson, envJson, url, oauthJson]) => ({
      name,
      transport,
      command: command ?? undefined,
      args: parseJson(argsJson, undefined) as string[] | undefined,
      env: parseJson(envJson, undefined) as Record<string, string> | undefined,
      url: url ?? undefined,
      oauth: oauthJson ? (parseJson(oauthJson, undefined) as unknown) : undefined,
    }),
  },
  removeMcpServer: { method: 'mcp.remove', params: ([name]) => ({ name }) },
  refreshMcpTools: {
    method: 'mcp.refresh',
    params: ([name]) => ({ name: name ?? undefined }),
  },
  testMcpServer: { method: 'mcp.testServer', params: ([name]) => ({ name }) },
  testMcpTool: {
    method: 'mcp.testTool',
    params: ([name, toolName]) => ({ name, toolName }),
  },
  oauthConnect: { method: 'mcp.oauth.connect', params: ([name]) => ({ name }) },
  oauthDisconnect: { method: 'mcp.oauth.disconnect', params: ([name]) => ({ name }) },
  listScheduledTasks: { method: 'scheduled.list' },
  addScheduledTask: {
    method: 'scheduled.add',
    // The renderer calls with 10 positional args (name, modeJson, prompt,
    // model, agent, workspace, enabled, startDate, endDate, skills) — the
    // old 8-slot destructure mapped agent→enabled (string!), workspace→
    // startDate, enabled→endDate, startDate→skills, and silently dropped
    // agent/workspace entirely.
    params: ([name, modeJson, prompt, model, agent, workspace, enabled, startDate, endDate, skills]) => ({
      name,
      mode: parseJson(modeJson, {}),
      prompt,
      model: model ?? undefined,
      agent: agent ?? undefined,
      workspace: workspace ?? undefined,
      enabled,
      startDate: startDate ?? undefined,
      endDate: endDate ?? undefined,
      skills: skills ?? undefined,
    }),
  },
  removeScheduledTask: {
    method: 'scheduled.remove',
    params: ([name]) => ({ name }),
  },
  updateScheduledTask: {
    method: 'scheduled.update',
    params: ([name, modeJson, prompt, model, agent, workspace, enabled, startDate, endDate, skills]) => ({
      name,
      mode: modeJson ? parseJson(modeJson, undefined) : undefined,
      prompt: prompt ?? undefined,
      model: model ?? undefined,
      agent: agent ?? undefined,
      workspace: workspace ?? undefined,
      enabled: enabled ?? undefined,
      startDate: startDate ?? undefined,
      endDate: endDate ?? undefined,
      skills: skills ?? undefined,
    }),
  },
  runScheduledTaskNow: {
    method: 'scheduled.runNow',
    params: ([name]) => ({ name }),
  },
  getTaskRunHistory: {
    method: 'jobs.taskRun.history',
    params: ([name]) => ({ name }),
  },
  listAllTaskRuns: { method: 'jobs.taskRuns' },
  getSessionTrace: { method: 'session.trace', params: ([sessionId]) => ({ sessionId }) },
  getSessionArchive: { method: 'session.archive', params: ([sessionId]) => ({ sessionId }) },
  marketList: { method: 'market.list' },
  listConfigurableProviders: { method: 'configurableProviders.list' },
  listJobs: { method: 'jobs.list' },
  listSubagentConfigs: { method: 'subagents.list' },
  setSubagentConfig: {
    method: 'subagents.set',
    params: ([name, enabled, model]) => ({ name, enabled, model: model ?? undefined }),
  },
  describeCredential: {
    method: 'credentials.describe',
    params: ([key]) => ({ key }),
  },
  readCredential: {
    method: 'credentials.read',
    params: ([key]) => ({ key }),
  },
  admitImageAttachments: {
    method: 'attachments.admit',
    params: ([imagesJson]) => ({ images: (parseJson(imagesJson, []) as unknown[]) ?? [] }),
  },
  readImageAttachment: {
    method: 'attachments.read',
    params: ([id]) => ({ id }),
  },
  suggestFiles: {
    method: 'fs.suggest',
    params: ([workspaceId, prefix]) => ({ workspaceId, prefix: prefix ?? '' }),
    // The protocol returns a bare string array; older builds wrapped it.
    result: (r) => (Array.isArray(r) ? r : (r?.files ?? [])),
  },
  recordFeedback: {
    method: 'feedback.record',
    params: ([sessionId, kind, comment]) => ({
      sessionId,
      kind,
      comment: comment ?? null,
    }),
    result: () => true,
  },
  readWorkspaceInstructions: {
    method: 'workspace.instructions',
    params: ([workspaceId]) => ({ workspaceId }),
    result: (r) => r?.content,
  },
  readWorkspaceInstructionsMeta: {
    method: 'workspace.instructions',
    params: ([workspaceId]) => ({ workspaceId }),
  },
  approve: {
    method: 'approval.respond',
    params: ([sessionId, allowed, remember]) => ({ sessionId, allowed, remember: remember ?? false }),
  },
  searchSessions: {
    method: 'session.search',
    params: ([query, limit]) => ({ query, limit: limit ?? 20 }),
  },
  getSettings: { method: 'settings.get' },
  updateSettings: {
    method: 'settings.update',
    params: ([patch]) => ({ patch }),
  },
  getSessionMessagesPage: {
    method: 'session.messages',
    // WI-101: explicit limit triggers the paged response form.
    params: ([sessionId, limit, before]) => ({
      sessionId,
      limit: limit ?? 50,
      ...(before != null ? { before } : {}),
    }),
    // The paged form returns { messages, total, hasMore }.
    result: (r) => r,
  },
  listTerminals: { method: 'terminal.list' },
  killTerminal: {
    method: 'terminal.kill',
    params: ([id]) => ({ id }),
  },
  runReview: {
    method: 'review.run',
    params: ([target, dimensions, cwd]) => ({ target, dimensions, cwd }),
  },
  runDoctor: { method: 'doctor.run' },
  setGoal: {
    method: 'goal.set',
    params: ([sessionId, objective, tokenBudget]) => ({ sessionId, objective, tokenBudget }),
  },
  getGoal: {
    method: 'goal.get',
    params: ([sessionId]) => ({ sessionId }),
  },
  clearGoal: {
    method: 'goal.clear',
    params: ([sessionId]) => ({ sessionId }),
  },
  steerSession: {
    method: 'session.steer',
    params: ([sessionId, task]) => ({ sessionId, task }),
  },
  queueList: {
    method: 'session.queueList',
    params: ([sessionId]) => ({ sessionId }),
  },
  queueAdd: {
    method: 'session.queueAdd',
    params: ([sessionId, text]) => ({ sessionId, text }),
    result: (r: any) => r?.id,
  },
  queueUpdate: {
    method: 'session.queueUpdate',
    params: ([sessionId, messageId, text]) => ({ sessionId, messageId, text }),
    result: (r: any) => r?.updated === true,
  },
  queueRemove: {
    method: 'session.queueRemove',
    params: ([sessionId, messageId]) => ({ sessionId, messageId }),
    result: (r: any) => r?.removed === true,
  },
  queueFlush: {
    method: 'session.queueFlush',
    params: ([sessionId]) => ({ sessionId }),
    result: (r: any) => r?.flushed === true,
  },
  queueFlushNow: {
    method: 'session.queueFlushNow',
    params: ([sessionId]) => ({ sessionId }),
    result: (r: any) => r?.flushed === true,
  },
  cancel: {
    method: 'session.cancel',
    params: ([sessionId]) => ({ sessionId }),
    result: (r) => r?.cancelled,
  },
};
