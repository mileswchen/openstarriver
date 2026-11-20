// Plugin tab sidebar placement (user request 2026-11-13): plugin-contributed
// tabs append to the Agent group — the daily driver — so an installed plugin's
// tab is visible without switching to Hub. Hub stays capability-management-only.
/// <reference types="vitest/globals" />
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import React from 'react';
import { render, screen, cleanup } from '@testing-library/react';
import { I18nProvider } from './i18n';
import { registerTab, unregisterTab } from './contrib/registry';

declare global { interface Window { starriver: any; } }

const PLUGIN_TAB_ID = 'plugin:demo:board';

beforeEach(() => {
  Element.prototype.scrollIntoView = vi.fn();
  localStorage.setItem('starriver-language', 'en');
  window.starriver = {
    listWorkspaces: vi.fn().mockResolvedValue([
      { id: 'w1', path: '/tmp/demo', title: 'demo', createdAt: '', sessionIds: ['s1'] },
    ]),
    getActiveWorkspace: vi.fn().mockResolvedValue('w1'),
    listSessions: vi.fn().mockResolvedValue([]),
    listArchivedSessions: vi.fn().mockResolvedValue([]),
  };
  registerTab({
    id: PLUGIN_TAB_ID,
    labelKey: '种草工作台', // non-i18n label: t() falls back to the raw string
    icon: '<svg width="16" height="16"/>',
    render: () => null,
  });
});

afterEach(() => {
  cleanup();
  unregisterTab(PLUGIN_TAB_ID);
});

async function renderNav(group: 'agent' | 'hub') {
  const { SidebarNavItems, WorkspaceProvider } = await import('./app');
  render(
    <I18nProvider>
      <WorkspaceProvider>
        <SidebarNavItems tab="coding" onNavigate={() => {}} group={group} />
      </WorkspaceProvider>
    </I18nProvider>
  );
}

describe('SidebarNavItems — plugin tab placement', () => {
  it('agent group appends plugin-contributed tabs after the fixed items', async () => {
    await renderNav('agent');
    const items = [...document.querySelectorAll('.sidebar-nav .nav-item')];
    const labels = items.map((item) => item.textContent);
    // Fixed Agent tabs first, then the plugin contribution.
    expect(labels.slice(0, 2)).toEqual(['New Task', 'Scheduled']);
    expect(labels[labels.length - 1]).toBe('种草工作台');
  });

  it('hub group shows no plugin-contributed tabs', async () => {
    await renderNav('hub');
    const labels = [...document.querySelectorAll('.sidebar-nav .nav-item')].map((item) => item.textContent);
    expect(labels).not.toContain('种草工作台');
  });

  it('first-party plugin tab label follows the UI language via the dictionary', async () => {
    // Dictionary key from i18n en.json/zh.json (pluginTabs.starpaper.workbench) —
    // the fallback path for manifests without `titles`.
    const TRANSLATED_TAB_ID = 'plugin:starpaper:workbench';
    registerTab({
      id: TRANSLATED_TAB_ID,
      labelKey: 'pluginTabs.starpaper.workbench',
      icon: '<svg width="16" height="16"/>',
      render: () => null,
    });
    try {
      localStorage.setItem('starriver-language', 'en');
      await renderNav('agent');
      let labels = [...document.querySelectorAll('.sidebar-nav .nav-item')].map((item) => item.textContent);
      expect(labels).toContain('Paper Workbench');

      cleanup();
      localStorage.setItem('starriver-language', 'zh');
      await renderNav('agent');
      labels = [...document.querySelectorAll('.sidebar-nav .nav-item')].map((item) => item.textContent);
      expect(labels).toContain('论文工作台');
    } finally {
      unregisterTab(TRANSLATED_TAB_ID);
    }
  });

  it('manifest labels (plugin-owned i18n) win over the dictionary at render time', async () => {
    const LABELED_TAB_ID = 'plugin:starpaper:workbench';
    registerTab({
      id: LABELED_TAB_ID,
      labelKey: 'pluginTabs.starpaper.workbench',
      labels: { en: 'Paper Workbench', zh: '论文工作台' },
      icon: '<svg width="16" height="16"/>',
      render: () => null,
    });
    try {
      localStorage.setItem('starriver-language', 'en');
      await renderNav('agent');
      let labels = [...document.querySelectorAll('.sidebar-nav .nav-item')].map((item) => item.textContent);
      expect(labels).toContain('Paper Workbench');

      cleanup();
      localStorage.setItem('starriver-language', 'zh');
      await renderNav('agent');
      labels = [...document.querySelectorAll('.sidebar-nav .nav-item')].map((item) => item.textContent);
      expect(labels).toContain('论文工作台');
    } finally {
      unregisterTab(LABELED_TAB_ID);
    }
  });
});
