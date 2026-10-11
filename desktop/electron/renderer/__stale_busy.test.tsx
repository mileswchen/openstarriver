/// <reference types="vitest/globals" />
// BUG-96: a turn that died client-side (renderer reload / hung past its
// last event) leaves the backend single-input-slot held. Opening that
// session must re-arm the composer's Stop affordance (synced from
// `sessions.running`), and a SESSION_BUSY send failure must surface the
// recoverable "previous turn still running" state instead of a dead-end
// error.
import { describe, it, expect, vi, beforeEach } from 'vitest';
import React from 'react';
import { render, screen, waitFor, fireEvent } from '@testing-library/react';
import { I18nProvider } from './i18n';

declare global { interface Window { starriver: any; } }

const WORKSPACE = { id: 'w1', path: '/tmp/ws-one', title: 'ws-one', createdAt: '2026-09-27', sessionIds: ['s-busy'] };
const SESSION = { id: 's-busy', title: 'Busy task', workspaceId: 'w1', createdAt: '2026-09-27', messageCount: 2 };

beforeEach(() => {
  localStorage.setItem('starriver-language', 'en');
  Element.prototype.scrollIntoView = vi.fn();
  window.starriver = {
    listWorkspaces: vi.fn().mockResolvedValue([WORKSPACE]),
    createWorkspace: vi.fn(),
    setActiveWorkspace: vi.fn().mockResolvedValue(undefined),
    listSessions: vi.fn().mockResolvedValue([SESSION]),
    listArchivedSessions: vi.fn().mockResolvedValue([]),
    getSessionMessages: vi.fn().mockResolvedValue([]),
    listSkills: vi.fn().mockResolvedValue([]),
    listMcpServers: vi.fn().mockResolvedValue([]),
    listModels: vi.fn().mockResolvedValue([]),
    listAgentModelBindings: vi.fn().mockResolvedValue([]),
    // Backend truth: the stale turn still holds the slot.
    listRunningSessions: vi.fn().mockResolvedValue(['s-busy']),
    cancel: vi.fn().mockResolvedValue(true),
    getWorktreeStatus: vi.fn().mockResolvedValue({ isRepo: false, branch: '', isWorktree: false, dirty: false }),
    readWorkspaceInstructions: vi.fn().mockResolvedValue(null),
    getCurrentAgent: vi.fn().mockResolvedValue(null),
    listAgentPresets: vi.fn().mockResolvedValue([]),
  };
});

async function mountOpenBusySession() {
  const { WorkspaceProvider, WorkspaceSection, CodingView } = await import('./app');
  render(
    <I18nProvider>
      <WorkspaceProvider>
        <WorkspaceSection onOpenCoding={() => {}} />
        <CodingView debugEnabled={false} />
      </WorkspaceProvider>
    </I18nProvider>,
  );
  // Open the session whose slot the stale turn still holds.
  await waitFor(() => expect(screen.getByText('Busy task')).toBeTruthy());
  fireEvent.click(screen.getByText('Busy task'));
}

describe('BUG-96 stale busy session', () => {
  it('re-arms the Stop affordance from sessions.running on session open', async () => {
    await mountOpenBusySession();
    // Composer flipped to the stop control because the backend holds the slot.
    await waitFor(() => expect(screen.getByText('Stop')).toBeTruthy());
    expect(window.starriver.listRunningSessions).toHaveBeenCalled();
    // The send path is not available while busy.
    expect(screen.queryByText('Send')).toBeNull();
  });

  // BUG-141: staleBusy armed on reload/switch must not outlive the task — a
  // terminal event on a fresh send proves the input slot is free, so the
  // composer must fall back to Send instead of staying on Stop forever.
  it('BUG-141: a completed send clears the stale marker (turnEnd releases the affordance)', async () => {
    let streamCb: ((err: string | null, event: any) => void) | null = null;
    window.starriver.execute = vi.fn((_task: string, _sid: string | undefined, callback: (err: string | null, event: any) => void) => {
      streamCb = callback;
      return () => {};
    });
    await mountOpenBusySession();
    await waitFor(() => expect(screen.getByText('Stop')).toBeTruthy());
    const ta = await screen.findByPlaceholderText('Describe what you want to build…');
    fireEvent.change(ta, { target: { value: 'follow-up' } });
    fireEvent.keyDown(ta, { key: 'Enter' });
    await waitFor(() => expect(streamCb).not.toBeNull());
    (streamCb as any)!(null, { type: 'turnEnd', reason: 'completed' });
    await waitFor(() => expect(screen.getByText('Send')).toBeTruthy());
    expect(screen.queryByText('Stop')).toBeNull();
  });

  it('releases the affordance after Stop cancels the stale turn (poll until slot freed)', async () => {
    await mountOpenBusySession();
    await waitFor(() => expect(screen.getByText('Stop')).toBeTruthy());
    // First poll still busy, second poll free — mirrors the hung-then-killed turn.
    let polls = 0;
    window.starriver.listRunningSessions = vi.fn().mockImplementation(async () => {
      polls += 1;
      return polls >= 2 ? [] : ['s-busy'];
    });
    window.confirm = vi.fn(() => true);
    fireEvent.click(screen.getByText('Stop'));
    await waitFor(() => {
      expect(window.starriver.cancel).toHaveBeenCalledWith('s-busy');
      expect(screen.getByText('Send')).toBeTruthy();
    });
    expect(polls).toBeGreaterThanOrEqual(2);
  });
});
