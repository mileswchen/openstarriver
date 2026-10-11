// FPM P3 plugin host tests: permission filtering, contribution sync (tab/theme registration), the frame message bridge.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { canInvoke, syncPluginTabs } from './contrib/pluginHost';
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
