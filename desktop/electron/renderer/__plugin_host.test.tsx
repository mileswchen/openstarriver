// FPM P3 plugin host tests: permission filtering, contribution sync (tab/theme registration), the frame message bridge.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { canInvoke, pluginCallParams, presetCeilingDenied, presetRank, sessionSpawnDenied, syncPluginTabs } from './contrib/pluginHost';
import { getShellReplacement, getTab, listTabs, listThemes } from './contrib/registry';

function fakeStarriver(plugins: unknown[]) {
  const listeners = new Set<(event: any) => void>();
  (window as any).starriver = {
    invoke: vi.fn(async (requestJson: string) => {
      const request = JSON.parse(requestJson);
      const result = request.method === 'plugins.list' ? plugins : null;
      return JSON.stringify({
        v: 1,
        id: request.id,
        ok: result !== null,
        result,
        error: result === null ? { code: 'METHOD_NOT_FOUND', message: 'x' } : undefined,
      });
    }),
    onEvent: vi.fn((callback: (event: any) => void) => {
      listeners.add(callback);
      return () => listeners.delete(callback);
    }),
  };
  return listeners;
}

afterEach(() => {
  delete (window as any).starriver;
});

describe('canInvoke permission filter', () => {
  it('allows methods whose permissions are fully covered', () => {
    expect(canInvoke(['session:read'], 'session.list')).toBe(true);
    expect(canInvoke(['session:read', 'session:execute'], 'session.execute')).toBe(true);
    expect(canInvoke([], 'protocol.info')).toBe(true);
  });

  it('denies methods with missing permissions', () => {
    expect(canInvoke(['session:read'], 'session.execute')).toBe(false);
    expect(canInvoke([], 'session.list')).toBe(false);
    expect(canInvoke([], 'nope.nope')).toBe(true); // unregistered method: no extra allow semantics (see host)
  });
});

describe('sessionSpawnDenied bridge gate (plugin platform P1)', () => {
  it('denies bare session.execute without the background opt-in', () => {
    expect(sessionSpawnDenied(undefined, 'session.execute', {})).toBe(true);
    expect(sessionSpawnDenied(null, 'session.execute', undefined)).toBe(true);
    expect(sessionSpawnDenied({}, 'session.execute', { prompt: 'hi' })).toBe(true);
    expect(sessionSpawnDenied({ background: false }, 'session.execute', {})).toBe(true);
    // Empty / non-string sessionId is not a continuation — still a spawn.
    expect(sessionSpawnDenied({}, 'session.execute', { sessionId: '' })).toBe(true);
    expect(sessionSpawnDenied({}, 'session.execute', { sessionId: 42 })).toBe(true);
  });

  it('allows continuation of an existing session without any opt-in', () => {
    expect(sessionSpawnDenied({}, 'session.execute', { sessionId: 's-1', prompt: 'follow-up' })).toBe(false);
    expect(sessionSpawnDenied(undefined, 'session.execute', { sessionId: 's-1' })).toBe(false);
  });

  it('allows spawning only with the manifest background declaration', () => {
    expect(sessionSpawnDenied({ background: true }, 'session.execute', {})).toBe(false);
    expect(sessionSpawnDenied({ background: true }, 'session.execute', { sessionId: 's-1' })).toBe(false);
  });

  it('never gates other methods — tools.call is the session-less data path', () => {
    expect(sessionSpawnDenied({}, 'tools.call', { name: 'glob', arguments: {} })).toBe(false);
    expect(sessionSpawnDenied({}, 'session.list', {})).toBe(false);
    expect(sessionSpawnDenied(undefined, 'plugins.list', undefined)).toBe(false);
  });
});

describe('pluginCallParams origin branding (plugin platform P1)', () => {
  it('brands session.execute with the manifest identity, overwriting frame claims', () => {
    expect(pluginCallParams('demo', 'session.execute', { task: 't' })).toEqual({
      task: 't',
      origin: 'plugin:demo',
    });
    // A frame claiming another origin (or none) cannot escape its identity.
    expect(pluginCallParams('demo', 'session.execute', { task: 't', origin: 'scheduled' })).toEqual({
      task: 't',
      origin: 'plugin:demo',
    });
    expect(pluginCallParams('demo', 'session.execute', undefined)).toEqual({ origin: 'plugin:demo' });
    // Continuations keep their sessionId — the host ignores origin for them.
    expect(pluginCallParams('demo', 'session.execute', { sessionId: 's-1', task: 't' })).toEqual({
      sessionId: 's-1',
      task: 't',
      origin: 'plugin:demo',
    });
  });

  it('passes other methods through untouched', () => {
    const params = { name: 'glob', arguments: { pattern: '*.rs' } };
    expect(pluginCallParams('demo', 'tools.call', params)).toBe(params);
    expect(pluginCallParams('demo', 'session.list', undefined)).toEqual({});
  });
});

describe('pluginCallParams pluginTask identity branding (plugin platform P2)', () => {
  it('brands every pluginTask.* call with the manifest identity, overwriting frame claims', () => {
    for (const method of [
      'pluginTask.run',
      'pluginTask.wait',
      'pluginTask.messages',
      'pluginTask.cancel',
      'pluginTask.approve',
      'pluginTask.list',
    ]) {
      expect(pluginCallParams('demo', method, { runId: 'r-1' })).toEqual({
        runId: 'r-1',
        plugin: 'demo',
      });
      // A frame claiming another plugin cannot escape its identity.
      expect(pluginCallParams('demo', method, { plugin: 'other', runId: 'r-1' })).toEqual({
        runId: 'r-1',
        plugin: 'demo',
      });
    }
    expect(pluginCallParams('demo', 'pluginTask.run', undefined)).toEqual({ plugin: 'demo' });
  });

  it('keeps session.execute origin branding alongside the new task branding', () => {
    expect(pluginCallParams('demo', 'session.execute', { task: 't' })).toEqual({
      task: 't',
      origin: 'plugin:demo',
    });
    // Non-task, non-execute methods stay untouched.
    const params = { query: 'x' };
    expect(pluginCallParams('demo', 'session.search', params)).toBe(params);
  });
});

describe('presetRank / presetCeilingDenied (plugin platform P2)', () => {
  it('ranks presets by autonomy and rejects unknown names', () => {
    expect(presetRank('manual')).toBe(0);
    expect(presetRank('accept-edits')).toBe(1);
    expect(presetRank('auto')).toBe(2);
    expect(presetRank('danger-full-access')).toBe(3);
    expect(presetRank('yolo')).toBeNull();
    expect(presetRank('')).toBeNull();
  });

  it('undeclared ceiling is manual (fail-closed): anything above manual is denied', () => {
    for (const ui of [undefined, null, {}]) {
      expect(presetCeilingDenied(ui, 'pluginTask.run', { task: 't' })).toBe(false); // omitted → manual
      expect(presetCeilingDenied(ui, 'pluginTask.run', { task: 't', permissionPreset: 'manual' })).toBe(false);
      expect(presetCeilingDenied(ui, 'pluginTask.run', { task: 't', permissionPreset: 'accept-edits' })).toBe(true);
      expect(presetCeilingDenied(ui, 'pluginTask.run', { task: 't', permissionPreset: 'auto' })).toBe(true);
      expect(presetCeilingDenied(ui, 'session.execute', { task: 't', permissionPreset: 'danger-full-access' })).toBe(true);
    }
  });

  it('declared ceiling allows up to its rank and denies above', () => {
    const ui = { maxPermissionPreset: 'auto' };
    expect(presetCeilingDenied(ui, 'pluginTask.run', { permissionPreset: 'manual' })).toBe(false);
    expect(presetCeilingDenied(ui, 'pluginTask.run', { permissionPreset: 'accept-edits' })).toBe(false);
    expect(presetCeilingDenied(ui, 'pluginTask.run', { permissionPreset: 'auto' })).toBe(false);
    expect(presetCeilingDenied(ui, 'pluginTask.run', { permissionPreset: 'danger-full-access' })).toBe(true);
    const max = { maxPermissionPreset: 'danger-full-access' };
    expect(presetCeilingDenied(max, 'pluginTask.run', { permissionPreset: 'danger-full-access' })).toBe(false);
  });

  it('malformed requests and declarations fail closed', () => {
    const ui = { maxPermissionPreset: 'auto' };
    expect(presetCeilingDenied(ui, 'pluginTask.run', { permissionPreset: 'yolo' })).toBe(true);
    expect(presetCeilingDenied(ui, 'pluginTask.run', { permissionPreset: 42 })).toBe(true);
    // A malformed ceiling declaration degrades to manual, never to unlimited.
    const bad = { maxPermissionPreset: 'superuser' };
    expect(presetCeilingDenied(bad, 'pluginTask.run', { permissionPreset: 'auto' })).toBe(true);
    expect(presetCeilingDenied(bad, 'pluginTask.run', { permissionPreset: 'manual' })).toBe(false);
  });

  it('gates the direct preset-change path too (session.setPermissionPreset)', () => {
    const ui = { maxPermissionPreset: 'accept-edits' };
    expect(presetCeilingDenied(ui, 'session.setPermissionPreset', { sessionId: 's', preset: 'auto' })).toBe(true);
    expect(presetCeilingDenied(ui, 'session.setPermissionPreset', { sessionId: 's', preset: 'accept-edits' })).toBe(false);
    expect(presetCeilingDenied(undefined, 'session.setPermissionPreset', { sessionId: 's', preset: 'auto' })).toBe(true);
  });

  it('never gates methods without a preset parameter', () => {
    expect(presetCeilingDenied(undefined, 'pluginTask.wait', { runId: 'r', permissionPreset: 'auto' })).toBe(false);
    expect(presetCeilingDenied(undefined, 'pluginTask.cancel', { runId: 'r' })).toBe(false);
    expect(presetCeilingDenied(undefined, 'tools.call', { name: 'bash' })).toBe(false);
    expect(presetCeilingDenied(undefined, 'pluginTask.run', undefined)).toBe(false);
    expect(presetCeilingDenied(undefined, 'pluginTask.run', null)).toBe(false);
  });
});

describe('syncPluginTabs', () => {
  beforeEach(() => {
    for (const tab of listTabs()) {
      if (tab.id.startsWith('plugin:')) {
        // unregister is handled inside syncPluginTabs; only clear the theme cache here
      }
    }
  });

  it('registers plugin tabs and themes from plugins.list', async () => {
    fakeStarriver([
      {
        name: 'demo',
        ui: {
          entry: 'index.html',
          entryUrl: 'starriver-plugin://demo/ui/index.html',
          tabs: [{ id: 'board', title: 'Board' }],
          permissions: ['session:read'],
          theme: 'theme.css',
          themeCss: ':root { --bg: #111; }',
        },
      },
      { name: 'skills-only', ui: null },
    ]);

    await syncPluginTabs();

    const tab = getTab('plugin:demo:board');
    expect(tab).toBeDefined();
    expect(tab!.labelKey).toBe('Board');
    expect(listThemes().some((t) => t.name === 'plugin:demo')).toBe(true);
    expect(getTab('plugin:skills-only:whatever')).toBeUndefined();
  });

  it('prefers the shell dictionary label for a first-party plugin tab', async () => {
    // starpaper/workbench carries translations in i18n en.json/zh.json
    // (`pluginTabs.starpaper.workbench`); the manifest title stays Chinese and
    // must not leak into the labelKey.
    fakeStarriver([
      {
        name: 'starpaper',
        ui: {
          entry: 'index.html',
          entryUrl: 'starriver-plugin://starpaper/ui/index.html',
          tabs: [{ id: 'workbench', title: '论文工作台' }],
          permissions: [],
          theme: null,
          themeCss: null,
        },
      },
    ]);
    await syncPluginTabs();
    expect(getTab('plugin:starpaper:workbench')!.labelKey).toBe('pluginTabs.starpaper.workbench');
  });

  it('delivers manifest titles as registry labels (plugin-owned i18n wins)', async () => {
    fakeStarriver([
      {
        name: 'starpaper',
        ui: {
          entry: 'index.html',
          entryUrl: 'starriver-plugin://starpaper/ui/index.html',
          tabs: [{ id: 'workbench', title: '论文工作台', titles: { en: 'Paper Workbench', zh: '论文工作台' } }],
          permissions: [],
          theme: null,
          themeCss: null,
        },
      },
    ]);
    await syncPluginTabs();
    const tab = getTab('plugin:starpaper:workbench');
    expect(tab!.labels).toEqual({ en: 'Paper Workbench', zh: '论文工作台' });
    // A title-less tab registers no labels (dictionary/raw-title fallback path).
    fakeStarriver([
      {
        name: 'bare',
        ui: {
          entry: 'index.html',
          entryUrl: 'starriver-plugin://bare/ui/index.html',
          tabs: [{ id: 't', title: 'Raw' }],
          permissions: [],
          theme: null,
          themeCss: null,
        },
      },
    ]);
    await syncPluginTabs();
    expect(getTab('plugin:bare:t')!.labels).toBeUndefined();
  });

  it('re-sync removes previously registered plugin tabs', async () => {
    fakeStarriver([
      {
        name: 'demo',
        ui: {
          entry: 'index.html',
          entryUrl: 'starriver-plugin://demo/ui/index.html',
          tabs: [{ id: 'board', title: 'Board' }],
          permissions: [],
          theme: null,
          themeCss: null,
        },
      },
    ]);
    await syncPluginTabs();
    expect(getTab('plugin:demo:board')).toBeDefined();

    fakeStarriver([]);
    await syncPluginTabs();
    expect(getTab('plugin:demo:board')).toBeUndefined();
  });

  it('registers shell replacement only with shell:replace permission', async () => {
    fakeStarriver([
      {
        name: 'shell-plugin',
        ui: {
          entry: 'index.html',
          entryUrl: 'starriver-plugin://shell-plugin/ui/index.html',
          tabs: [
            { id: '__shell', title: 'Shell' },
            { id: 'extra', title: 'Extra' },
          ],
          permissions: ['shell:replace', 'session:read'],
          theme: null,
          themeCss: null,
        },
      },
    ]);
    await syncPluginTabs();
    const shell = getShellReplacement();
    expect(shell).not.toBeNull();
    expect(shell!.name).toBe('shell-plugin');
    // __shell takes no regular tab slot; regular tabs register as usual.
    expect(getTab('plugin:shell-plugin:__shell')).toBeUndefined();
    expect(getTab('plugin:shell-plugin:extra')).toBeDefined();

    fakeStarriver([]);
    await syncPluginTabs();
    expect(getShellReplacement()).toBeNull();
  });

  it('does not register shell replacement without the permission', async () => {
    fakeStarriver([
      {
        name: 'no-shell',
        ui: {
          entry: 'index.html',
          entryUrl: 'starriver-plugin://no-shell/ui/index.html',
          tabs: [{ id: '__shell', title: 'Shell' }],
          permissions: ['session:read'],
          theme: null,
          themeCss: null,
        },
      },
    ]);
    await syncPluginTabs();
    expect(getShellReplacement()).toBeNull();
  });
});
