// Integration: the sandboxed preload exposes today's WI-243 pin wiring and the
// model-fetch passthrough. electron is mocked; the stale committed
// legacyBridge.js (Vite resolves .js before .ts) is redirected to the current
// .ts source so the generic apiMethods loop includes pinWorkspace.
import { describe, it, expect, vi, beforeAll } from 'vitest';

const { invoke, exposeInMainWorld } = vi.hoisted(() => ({
  invoke: vi.fn(async (channel: string) =>
    channel === 'runtime:invoke'
      ? JSON.stringify({ v: 1, id: 'r', ok: true, result: {} })
      : { ok: true, models: [] }),
  exposeInMainWorld: vi.fn(),
}));

vi.mock('electron', () => ({
  ipcRenderer: { invoke, on: vi.fn(), removeListener: vi.fn() },
  contextBridge: { exposeInMainWorld },
}));
// Force the current mappings regardless of .js/.ts resolution order.
vi.mock('../legacyBridge.js', async () => await vi.importActual('../legacyBridge.ts'));

let api: any;
beforeAll(async () => {
  await import('../preload.ts');
  const call = exposeInMainWorld.mock.calls.find((c: any[]) => c[0] === 'starriver');
  api = call![1];
});

describe('preload passthrough — model fetch + pin', () => {
  it('exposes the starriver API surface', () => {
    expect(api).toBeTruthy();
    expect(typeof api.fetchProviderModels).toBe('function');
    expect(typeof api.pinWorkspace).toBe('function');
  });

  it('fetchProviderModels invokes the dedicated channel with all four args', async () => {
    invoke.mockClear();
    const res = await api.fetchProviderModels('http://x', 'sk-1', 'ollama', 'anthropic');
    expect(invoke).toHaveBeenCalledWith('runtime:fetchProviderModels', 'http://x', 'sk-1', 'ollama', 'anthropic');
    expect(res).toEqual({ ok: true, models: [] });
  });

  it('pinWorkspace builds a workspaces.pin envelope via runtime:invoke', async () => {
    invoke.mockClear();
    await api.pinWorkspace('ws-1', true);
    expect(invoke).toHaveBeenCalledTimes(1);
    const [channel, payload] = invoke.mock.calls[0];
    expect(channel).toBe('runtime:invoke');
    const req = JSON.parse(payload);
    expect(req.v).toBe(1);
    expect(req.method).toBe('workspaces.pin');
    expect(req.params).toEqual({ workspaceId: 'ws-1', pinned: true });
  });

  it('unpin passes pinned=false through the same envelope', async () => {
    invoke.mockClear();
    await api.pinWorkspace('ws-2', false);
    const req = JSON.parse(invoke.mock.calls[0][1]);
    expect(req.params).toEqual({ workspaceId: 'ws-2', pinned: false });
  });
});