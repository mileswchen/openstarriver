// FPM acceptance #4: the legacy bridge (LEGACY_METHODS + envelope construction/response parsing).
// Covers: every legacy method the UI uses maps to a protocol method, key param/result mappings,
// and protocol envelope construction/response parsing.

import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

// NOTE: the explicit `.ts` matters — a stale committed `legacyBridge.js` bundle
// sits next to the source and Vite resolves `.js` first, so the bare specifier
// silently tested an outdated build.
import {
  LEGACY_METHODS,
  parseProtocolResponse,
  protocolRequest,
} from '../legacyBridge.ts';
import { PROTOCOL_METHODS } from './client/client.gen.ts';

/** The committed preload.js must stay an esbuild self-contained bundle: the
 *  sandboxed preload cannot load relative modules, and any tsc-emitted output
 *  (with require("./legacyBridge")) white-screens. */
it('preload.js stays a sandbox-safe self-contained bundle', () => {
  const src = readFileSync(resolve(process.cwd(), 'preload.js'), 'utf8');
  expect(src).not.toContain('require("./legacyBridge")');
  expect(src).toContain('getActiveWorkspace');
});

/** The legacy method surface the current UI (app.tsx) actually uses (statically locked to catch missing mappings). */
const UI_SURFACE = [
  'listTools', 'listAgents', 'scanAgents',
  'listSkills', 'scanSkills', 'createSkill', 'updateSkill', 'deleteSkill',
  'setSkillAlwaysOn', 'listSkillUsage', 'compactSession',
  'sessionUsage', 'workspaceDiff', 'initProjectAgents', 'exportSession',
  'listPlugins', 'installPlugin', 'uninstallPlugin', 'listModelProviders',
  'listModels', 'removeModel',
  'addModel', 'listAgentModelBindings',
  'setAgentActiveModel', 'listSessions', 'getSessionMessages',
  'getSessionTimeline', 'forkSession', 'deleteSession', 'renameSession',
  'archiveSession', 'unarchiveSession', 'listArchivedSessions',
  'listWorkspaces', 'createWorkspace', 'renameWorkspace', 'pinWorkspace', 'deleteWorkspace',
  'setActiveWorkspace', 'getWorktreeStatus',
  'listMemories', 'deleteMemory', 'clearMemories', 'startCodeReview',
  'listMcpServers', 'addMcpServer', 'removeMcpServer', 'refreshMcpTools',
  'testMcpServer', 'testMcpTool', 'oauthConnect', 'oauthDisconnect',
  'listScheduledTasks', 'addScheduledTask', 'removeScheduledTask',
  'updateScheduledTask', 'runScheduledTaskNow', 'getTaskRunHistory',
  'listAllTaskRuns', 'getSessionTrace', 'marketList',
  'listConfigurableProviders', 'listJobs', 'listSubagentConfigs',
  'setSubagentConfig', 'describeCredential', 'admitImageAttachments',
  'readWorkspaceInstructions', 'approve',
  'cancel',
];

describe('LEGACY_METHODS', () => {
  it('covers the whole UI surface with protocol methods', () => {
    for (const method of UI_SURFACE) {
      const spec = LEGACY_METHODS[method];
      expect(spec, `missing legacy mapping for ${method}`).toBeDefined();
      expect(spec.method).toMatch(/^[a-z][a-zA-Z0-9]*(\.[a-zA-Z0-9]+)+$/);
    }
  });

  it('maps addModel positional args (variantsJson string → variants array)', () => {
    const spec = LEGACY_METHODS.addModel;
    expect(spec.params!([
      'm1', 'Model', 'chat', 'openai', 'http://x', 'sk-1', undefined, 'default',
      '[{"id":"v1","name":"V1"}]', 4, 8192, 65536, 0.3, 'openai', 'low', 'CN', true,
    ])).toEqual({
      id: 'm1', name: 'Model', modelType: 'chat', providerType: 'openai',
      baseUrl: 'http://x', apiKey: 'sk-1', modelPath: undefined,
      defaultModel: 'default', variants: [{ id: 'v1', name: 'V1' }],
      concurrency: 4, maxTokens: 8192, contextWindow: 65536,
      temperature: 0.3, apiFormat: 'openai', reasoningEffort: 'low',
      modelLabel: 'CN', supportsVision: true,
    });
  });

  it('maps JSON-string args for mcp / scheduled / review / attachments', () => {
    expect(LEGACY_METHODS.addMcpServer.params!([
      'files', 'stdio', 'npx', '["-y","x"]', '{"A":"1"}', null, null,
    ])).toEqual({
      name: 'files', transport: 'stdio', command: 'npx',
      args: ['-y', 'x'], env: { A: '1' }, url: undefined, oauth: undefined,
    });
    expect(LEGACY_METHODS.startCodeReview.params!(['["a.rs"]', '["perf"]'])).toEqual({
      files: ['a.rs'], dimensions: ['perf'],
    });
    expect(LEGACY_METHODS.admitImageAttachments.params!(['[{"name":"x.png"}]'])).toEqual({
      images: [{ name: 'x.png' }],
    });
    expect(LEGACY_METHODS.addScheduledTask.params!([
      't1', '{"type":"daily"}', 'p', 'm1', 'agent-x', 'ws-1', true, '2026-09-01', undefined, ['s'],
    ])).toEqual({
      name: 't1', mode: { type: 'daily' }, prompt: 'p', model: 'm1', agent: 'agent-x',
      workspace: 'ws-1', enabled: true,
      startDate: '2026-09-01', endDate: undefined, skills: ['s'],
    });
  });

  it('maps special result sub-fields', () => {
    expect(LEGACY_METHODS.workspaceDiff.result!({ diff: '+++a' })).toBe('+++a');
    expect(LEGACY_METHODS.exportSession.result!({ path: '/tmp/x.md' })).toBe('/tmp/x.md');
    expect(LEGACY_METHODS.initProjectAgents.result!({ result: 'ok' })).toBe('ok');
    expect(LEGACY_METHODS.readWorkspaceInstructions.result!({ content: 'hi' })).toBe('hi');
    expect(LEGACY_METHODS.cancel.result!({ cancelled: true })).toBe(true);
  });

  it('keeps camelCase results intact (app.tsx reads camelCase fields)', () => {
    const session = LEGACY_METHODS.listSessions;
    const worktree = LEGACY_METHODS.getWorktreeStatus;
    const jobs = LEGACY_METHODS.listJobs;
    // No result mapping = the protocol result passes through as-is (camelCase).
    expect(session.result).toBeUndefined();
    expect(worktree.result).toBeUndefined();
    expect(jobs.result).toBeUndefined();
  });
});

describe('protocol envelope helpers', () => {
  it('builds a versioned request with a unique id', () => {
    const a = protocolRequest('session.list', { workspaceId: 'w1' });
    const b = protocolRequest('session.list', {});
    expect(a.v).toBe(1);
    expect(a.method).toBe('session.list');
    expect(a.params).toEqual({ workspaceId: 'w1' });
    expect(a.id).not.toBe(b.id);
    expect(protocolRequest('x', undefined).params).toEqual({});
  });

  it('parses ok responses into result and throws protocol errors', () => {
    expect(parseProtocolResponse(JSON.stringify({ v: 1, id: 'r', ok: true, result: { ok: true } })))
      .toEqual({ ok: true });
    expect(() =>
      parseProtocolResponse(JSON.stringify({
        v: 1, id: 'r', ok: false,
        error: { code: 'SESSION_BUSY', message: 'busy' },
      })),
    ).toThrowError(/busy/);
  });
});

// WI-243 pin-to-top: the legacy bridge mapping and the generated typed-client
// schema must agree on method, params and permissions (both paths reach the
// same `workspaces.pin` host handler).
describe('pinWorkspace — legacy bridge ↔ typed client parity', () => {
  it('maps positional args to workspaces.pin {workspaceId, pinned}', () => {
    const spec = LEGACY_METHODS.pinWorkspace;
    expect(spec.method).toBe('workspaces.pin');
    expect(spec.params!(['ws-1', true])).toEqual({ workspaceId: 'ws-1', pinned: true });
    expect(spec.params!(['ws-2', false])).toEqual({ workspaceId: 'ws-2', pinned: false });
    // No result mapping: the protocol `{}` passes through unchanged.
    expect(spec.result).toBeUndefined();
  });

  it('agrees with the generated typed-client schema', () => {
    const meta = PROTOCOL_METHODS['workspaces.pin'];
    expect(meta, 'workspaces.pin must exist in the typed client').toBeDefined();
    expect(meta.permissions).toEqual(['workspaces:write']);
    expect(meta.streaming).toBe(false);
    const params = meta.params as any;
    expect(params.required).toEqual(['workspaceId', 'pinned']);
    expect(params.properties.workspaceId.type).toBe('string');
    expect(params.properties.pinned.type).toBe('boolean');
    // The legacy mapping emits exactly the schema's required keys.
    const mapped = LEGACY_METHODS.pinWorkspace.params!(['ws-1', true]) as Record<string, unknown>;
    expect(Object.keys(mapped).sort()).toEqual([...params.required].sort());
  });
});
