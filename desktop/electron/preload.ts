import { contextBridge, ipcRenderer } from 'electron';
import { LEGACY_METHODS, parseProtocolResponse, protocolRequest } from './legacyBridge';

// FPM acceptance #4: the built-in UI goes through the protocol client.
// Typed methods (window.starriver.listSessions etc.) build protocol envelopes
// inside the bridge and dispatch via the generic `runtime:invoke` — the UI and
// UI tests need zero changes, and embedded/daemon share one protocol path;
// system integration (directory dialogs, windows, temp files, remote model
// fetching) stays out of the protocol.

// FPM P4b: in daemon mode `runtime:event` carries a protocol envelope (the
// legacy event inside payload); in embedded mode it is the legacy event itself.
// Merge both into the shape the old UI expects.
const toLegacyEvent = (event: any): any =>
  event && typeof event === 'object' && 'payload' in event
    ? { ...event, ...event.payload }
    : event;

/** Protocol call: build the envelope → runtime:invoke → returns result on
 *  success, throws a protocol error on failure. */
async function call(method: string, params: unknown): Promise<any> {
  const request = protocolRequest(method, params);
  const text = await ipcRenderer.invoke('runtime:invoke', JSON.stringify(request));
  return parseProtocolResponse(text);
}

// Typed methods generated from LEGACY_METHODS: params go through the mapping
// table → named protocol params; results pass through.
const apiMethods: Record<string, (...args: any[]) => Promise<any>> = {};
for (const [name, spec] of Object.entries(LEGACY_METHODS)) {
  apiMethods[name] = async (...args: any[]) => {
    const result = await call(spec.method, spec.params ? spec.params(args) : {});
    return spec.result ? spec.result(result) : result;
  };
}

contextBridge.exposeInMainWorld('starriver', {
  ...apiMethods,

  // ── Streaming execution: session.execute → subscription; events merge via runtime:event ──
 // executionMode only applies to NEW sessions (locked at creation,
  // Chat never sends it. agent names the run's agent (omitted = built-in Chat
  // Agent); isolation is the view's per-session worktree toggle (omitted =
  // the agent profile's `isolate:` default). agentPreset is the WI-5
  // creation-time binding — rejected with AGENT_PRESET_LOCKED on continuations.
  // permissionPreset is the BUG-113 creation-time binding: the composer's
  // pre-send pick rides execute and is stamped into the fresh session BEFORE
  // the first turn runs (the old post-TaskStarted best-effort write is gone).
  execute: (task: string, sessionId: string | undefined, callback: (err: string | null, event: any) => void, executionMode?: string, agent?: string, isolation?: boolean, agentPreset?: string, permissionPreset?: string) => {
    // The listener deliberately OUTLIVES this execute's terminal event: the
    // agent driver auto-claims queued messages as the next turn right after
    // the previous one settles, and those events stream with no second
    // execute to re-arm a listener. Unsubscribing at the first terminal made
    // every driver-claimed turn invisible (queue card stuck, no Stop, no
    // stream — 2026-10-05 session 85a4410f). The next execute() re-arms via
    // the removeAllListeners below; a failed call removes the handler.
    const handler = (_e: any, event: any) => {
      callback(null, toLegacyEvent(event));
    };
    ipcRenderer.removeAllListeners('runtime:event');
    ipcRenderer.on('runtime:event', handler);
    void call('session.execute', {
      task,
      sessionId: sessionId ?? undefined,
      executionMode: executionMode ?? undefined,
      agent: agent ?? undefined,
      isolation: isolation ?? undefined,
      agentPreset: agentPreset ?? undefined,
      permissionPreset: permissionPreset ?? undefined,
    }).catch((err: any) => {
      ipcRenderer.removeListener('runtime:event', handler);
      callback(err?.message ?? String(err), null);
    });
  },
  executeStream: (task: string, callback: (event: any) => void) => {
    ipcRenderer.removeAllListeners('runtime:event');
    const handler = (_e: any, event: any) => callback(toLegacyEvent(event));
    ipcRenderer.on('runtime:event', handler);
    void call('session.execute', { task }).catch((err: any) => {
      // A failed execute must not leave the channel listener registered
      // forever (every later event double-fires into this dead callback).
      ipcRenderer.removeListener('runtime:event', handler);
      callback({ type: 'TaskFailed', error: err?.message ?? String(err) });
    });
  },

  // ── Generic protocol entry (renderer client / plugin frames) ──
  invoke: (requestJson: string): Promise<string> =>
    ipcRenderer.invoke('runtime:invoke', requestJson),
  onEvent: (callback: (event: any) => void): (() => void) => {
    const handler = (_e: any, event: any) => callback(event);
    ipcRenderer.removeAllListeners('runtime:event');
    ipcRenderer.on('runtime:event', handler);
    return () => ipcRenderer.removeListener('runtime:event', handler);
  },
  httpServerInfo: (): Promise<string | null> => ipcRenderer.invoke('runtime:httpServerInfo'),
  lanAddress: (): Promise<string> => ipcRenderer.invoke('runtime:lanAddress'),
  retryInit: (): Promise<{ ok: boolean; relaunched?: boolean }> => ipcRenderer.invoke('runtime:retryInit'),
  startHttpServer: (): Promise<string> => ipcRenderer.invoke('runtime:startHttpServer'),
  stopHttpServer: (): Promise<void> => ipcRenderer.invoke('runtime:stopHttpServer'),

  // The Standard dropdown passes null → normalized to "standard": on the host,
  // an empty string and the standard value both mean "clear".
  // With a sessionId: returns that session's creation-locked execution mode and
 // replays it to the kernel (binding).
  getAgentMode: (sessionId?: string) => call('session.getExecutionMode', { sessionId: sessionId ?? undefined }).then((r: any) => (r && typeof r === 'object' ? r?.mode ?? 'standard' : r ?? 'standard')),
  setAgentMode: (mode: string | null) => call('session.setExecutionMode', { mode: mode ?? 'standard' }),
  confirmPlan: (sessionId: string) => call('session.confirmPlan', { sessionId }),
  // WI-5: the agent-preset roster (id/name/description/broken/isDefault) plus
  // the deployment default id — drives the new-session preset picker.
  listAgentPresets: () => call('agentPresets.list', {}),
  // Plan fold: answer a pending exit_plan_mode review (Plan confirm card).
  // requestId may be omitted to answer the session's latest pending review.
  planReviewDecision: (sessionId: string, requestId: string, approved: boolean, feedback?: string) =>
    call('plan.reviewDecision', { sessionId, requestId: requestId || undefined, approved, feedback: feedback || undefined }),
  setApprovalMode: (sessionId: string, mode: string) => call('session.setApprovalMode', { sessionId, mode }),
 // Permission presets (permission-presets parity): one preset writes both
  // knob events; getPermission folds {sandbox, approval, preset} (preset is
  // null for the derived custom state).
  setPermissionPreset: (sessionId: string, preset: string) => call('session.setPermissionPreset', { sessionId, preset }),
  getPermission: (sessionId: string): Promise<{ sandbox: string | null; approval: string | null; preset: string | null }> =>
    call('session.getPermission', { sessionId }),
  // Backlog #7 settings hot-update: deep-merge patch, applies without restart.
  getSettings: (): Promise<Record<string, unknown>> => call('settings.get', {}),
  updateSettings: (patch: Record<string, unknown>): Promise<Record<string, unknown>> =>
    call('settings.update', { patch }),

  // ── System integration (stays in Electron main, outside the protocol) ──
  selectDirectory: () => ipcRenderer.invoke('dialog:selectDirectory'),
  saveTempFile: (name: string, base64Data: string): Promise<string> =>
    ipcRenderer.invoke('runtime:saveTempFile', name, base64Data),
  fetchProviderModels: (baseUrl: string, apiKey?: string, engine?: 'ollama' | 'lmstudio') =>
    ipcRenderer.invoke('runtime:fetchProviderModels', baseUrl, apiKey, engine),
  // Open the workspace directory in the user's terminal / IDE (main-process
  // spawn; `command` = settings template with a {dir} placeholder).
  openTerminal: (cwd: string, command?: string): Promise<{ ok: boolean }> =>
    ipcRenderer.invoke('shell:openTerminal', { cwd, command }),
  openIDE: (cwd: string, command?: string): Promise<{ ok: boolean }> =>
    ipcRenderer.invoke('shell:openIDE', { cwd, command }),
  // Models page "open config file": open ~/.starriver/settings.yaml in the system editor.
  openModelsConfig: (): Promise<{ ok: boolean; error?: string }> =>
    ipcRenderer.invoke('shell:openModelsConfig'),
  onDaemonStatus: (callback: (status: any) => void): (() => void) => {
    const handler = (_e: any, status: any) => callback(status);
    ipcRenderer.on('daemon:status', handler);
    return () => ipcRenderer.removeListener('daemon:status', handler);
  },
  close: () => ipcRenderer.invoke('win:close'),
  minimize: () => ipcRenderer.invoke('win:minimize'),
  maximize: () => ipcRenderer.invoke('win:maximize'),
  backendHealth: () => ipcRenderer.invoke('runtime:backendHealth'),
  quitApp: () => ipcRenderer.invoke('runtime:quitApp'),
});

// Auto-update
contextBridge.exposeInMainWorld('updater', {
  check: () => ipcRenderer.invoke('updater:check'),
  download: () => ipcRenderer.invoke('updater:download'),
  onStatus: (callback: (status: any) => void) => {
    ipcRenderer.on('updater:status', (_e, status) => callback(status));
  },
});
