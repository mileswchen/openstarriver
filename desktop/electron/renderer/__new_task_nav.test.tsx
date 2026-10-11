/// <reference types="vitest/globals" />
import { describe, it, expect, vi, beforeEach } from 'vitest';
import React from 'react';
import { render, screen, fireEvent, waitFor } from '@testing-library/react';
import { I18nProvider } from './i18n';

declare global { interface Window { starriver: any; } }

beforeEach(() => {
  Element.prototype.scrollIntoView = vi.fn();
  localStorage.setItem('starriver-language', 'en');
  window.starriver = {
    getSessionTrace: vi.fn().mockResolvedValue({ entries: [] }),
    listWorkspaces: vi.fn().mockResolvedValue([
      { id: 'w1', path: '/tmp/demo', title: 'demo', createdAt: '', sessionIds: ['s1'] },
    ]),
    getActiveWorkspace: vi.fn().mockResolvedValue('w1'),
    listSessions: vi.fn().mockResolvedValue([
      { id: 's1', title: 'blue button', workspaceId: 'w1', createdAt: '', messageCount: 3 },
    ]),
    getSessionMessages: vi.fn().mockResolvedValue([
      { role: 'user', content: '把按钮改成蓝色' },
      { role: 'assistant', content: '改好了。' },
    ]),
    getAgentMode: vi.fn().mockResolvedValue('standard'),
    listArchivedSessions: vi.fn().mockResolvedValue([]),
    listSkills: vi.fn().mockResolvedValue([]),
    listMcpServers: vi.fn().mockResolvedValue([]),
    listModels: vi.fn().mockResolvedValue([]),
    listAgentModelBindings: vi.fn().mockResolvedValue([]),
    getCurrentAgent: vi.fn().mockResolvedValue(null),
    getWorktreeStatus: vi.fn().mockResolvedValue({ isRepo: false, branch: null, isWorktree: false, dirty: false }),
    readWorkspaceInstructions: vi.fn().mockResolvedValue(null),
    setAgentMode: vi.fn().mockResolvedValue(undefined),
    setActiveWorkspace: vi.fn().mockResolvedValue(undefined),
    createWorkspace: vi.fn(), renameWorkspace: vi.fn().mockResolvedValue(undefined), deleteWorkspace: vi.fn().mockResolvedValue(undefined),
    setAgentActiveModel: vi.fn().mockResolvedValue(undefined),
    setAgentReasoningEffort: vi.fn().mockResolvedValue(undefined),
    execute: vi.fn(),
  };
});

// Regression: on the Coding session screen the sidebar item is still labeled
// "New Task" (that IS the coding tab). Re-clicking it must return to the New
// Task screen instead of being a no-op setTab.
describe('SidebarNavItems — re-click active New Task tab', () => {
  it('returns to the New Task screen from an open session', async () => {
    const { CodingView, WorkspaceProvider, WorkspaceSection, SidebarNavItems } = await import('./app');
    render(
      <I18nProvider>
        <WorkspaceProvider>
          <SidebarNavItems tab="coding" onNavigate={() => {}} />
          <WorkspaceSection onOpenCoding={() => {}} />
          <CodingView debugEnabled={false} />
        </WorkspaceProvider>
      </I18nProvider>
    );

    // Enter the session: the Conversation/Trajectory switch appears.
    fireEvent.click(await screen.findByText('blue button'));
    await screen.findByRole('tab', { name: 'Conversation' });

    // Re-click the active "New Task" nav item. Scoped to SidebarNavItems —
    // the workspace row's + button shares the "New Task" accessible name.
    const navItem = document.querySelector('.nav-item.active') as HTMLElement;
    expect(navItem?.textContent).toContain('New Task');
    fireEvent.click(navItem);

    // Back on the New Task screen: the session chrome is gone and the
    // top-stacked New Task layout (mode switch + hero) is back, and the
    // provider performed the fresh-conversation reset.
    await waitFor(() => expect(screen.queryByRole('tab', { name: 'Conversation' })).toBeNull());
    expect(document.querySelector('.message-list.new-task')).toBeTruthy();
    expect(document.querySelector('[aria-label="agent-preset"]')).toBeTruthy();
  });
});
