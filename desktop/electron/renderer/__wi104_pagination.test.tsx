/// <reference types="vitest/globals" />
// WI-126 (superseding WI-104/BUG-86 paging): history loads IN FULL up front —
// server-stamped seqs ride the rows (fork anchors), stale fetches for session
// A never write into session B's view, and no "load earlier" button exists.
import { describe, it, expect, vi, beforeEach } from 'vitest';
import React from 'react';
import { render, screen, fireEvent, waitFor } from '@testing-library/react';
import { I18nProvider } from './i18n';

declare global { interface Window { starriver: any; } }

beforeEach(() => {
  Element.prototype.scrollIntoView = vi.fn();
  localStorage.setItem('starriver-language', 'en');
  window.starriver = {
    listWorkspaces: vi.fn().mockResolvedValue([
      { id: 'w1', path: '/tmp/demo', title: 'demo', createdAt: '', sessionIds: ['s1', 's2'] },
    ]),
    createWorkspace: vi.fn(),
    renameWorkspace: vi.fn().mockResolvedValue(undefined),
    deleteWorkspace: vi.fn().mockResolvedValue(undefined),
    setActiveWorkspace: vi.fn().mockResolvedValue(undefined),
    listSessions: vi.fn().mockResolvedValue([
      { id: 's1', title: 'paged session', workspaceId: 'w1', createdAt: '', messageCount: 4 },
      { id: 's2', title: 'quick session', workspaceId: 'w1', createdAt: '', messageCount: 1 },
    ]),
    getSessionMessages: vi.fn().mockResolvedValue([]),
    listArchivedSessions: vi.fn().mockResolvedValue([]),
    listSkills: vi.fn().mockResolvedValue([]),
    listMcpServers: vi.fn().mockResolvedValue([]),
    listModels: vi.fn().mockResolvedValue([]),
    listAgentModelBindings: vi.fn().mockResolvedValue([]),
    switchAgent: vi.fn().mockResolvedValue(undefined),
    getCurrentAgent: vi.fn().mockResolvedValue(null),
    getWorktreeStatus: vi.fn().mockResolvedValue({ isRepo: true, branch: 'main', isWorktree: false, dirty: false }),
    setSessionIsolation: vi.fn().mockResolvedValue(undefined),
    readWorkspaceInstructions: vi.fn().mockResolvedValue(null),
    setAgentMode: vi.fn().mockResolvedValue(undefined),
    approve: vi.fn().mockResolvedValue(undefined),
    execute: vi.fn(),
  };
});

describe('WI-126 full history load', () => {
  it('loads every message (with server seqs) in one call — no load-earlier button', async () => {
    window.starriver.getSessionMessages = vi.fn().mockResolvedValue([
      { role: 'user', content: 'u-seq-0', seq: 0 },
      { role: 'assistant', content: 'a-seq-1', seq: 1 },
      { role: 'user', content: 'u-seq-2', seq: 2 },
      { role: 'assistant', content: 'a-seq-3', seq: 3 },
      { role: 'user', content: 'u-seq-4', seq: 4 },
    ]);

    const { CodingView, WorkspaceProvider, WorkspaceSection } = await import('./app');
    render(<I18nProvider><WorkspaceProvider><WorkspaceSection onOpenCoding={() => {}} /><CodingView debugEnabled={false} /></WorkspaceProvider></I18nProvider>);

    fireEvent.click(await screen.findByText('paged session'));
    await waitFor(() => expect(screen.getByText('u-seq-4')).toBeTruthy());
    // Whole history in one shot: oldest and newest both rendered, no button.
    expect(screen.getByText('u-seq-0')).toBeTruthy();
    expect(screen.queryByTestId('load-earlier')).toBeNull();
    // The paged method must not be consulted anymore.
    expect(window.starriver.getSessionMessagesPage ?? null).toBeNull();
    expect(window.starriver.getSessionMessages).toHaveBeenCalledWith('s1');
  });

  it('a slow history fetch for A never writes into the view after switching to B', async () => {
    let releaseA: (() => void) | null = null;
    window.starriver.getSessionMessages = vi.fn((sid: string) => {
      if (sid === 's1') {
        return new Promise((resolve) => { releaseA = () => resolve([
          { role: 'user', content: 'LATE-FROM-A', seq: 0 },
        ]); });
      }
      return Promise.resolve([
        { role: 'user', content: 'b-latest', seq: 0 },
      ]);
    });

    const { CodingView, WorkspaceProvider, WorkspaceSection } = await import('./app');
    render(<I18nProvider><WorkspaceProvider><WorkspaceSection onOpenCoding={() => {}} /><CodingView debugEnabled={false} /></WorkspaceProvider></I18nProvider>);

    // Open A (slow), then switch to B before A's fetch resolves.
    fireEvent.click(await screen.findByText('paged session'));
    await waitFor(() => expect(releaseA).toBeTruthy());
    fireEvent.click(screen.getByText('quick session'));
    await waitFor(() => expect(screen.getByText('b-latest')).toBeTruthy());
    // Release A's stale fetch — it must not overwrite B's view.
    releaseA?.();
    await waitFor(() => {
      const text = document.querySelector('.message-list')?.textContent ?? '';
      expect(text).not.toContain('LATE-FROM-A');
    });
  });
});
