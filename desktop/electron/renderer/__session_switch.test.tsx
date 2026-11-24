/// <reference types="vitest/globals" />
// 2026-10-07 session-switch sweep: per-session view state leaked across an
// A→B switch. Two pinned regressions:
//   1. Session A's run state kept the composer on Stop after switching to an
//      IDLE session B — the button keys on `sending || staleBusy`, and
//      `sending` belonged to A's execute (fixed: the switch effect resets
//      sending and re-derives the button from `sessions.running` only).
//   2. The stop/approval ref (sessionIdRef) only filled from live
//      TaskStarted/sessionStarted, so after a restart / remount Stop armed
//      the 20s watchdog and NEVER sent a cancel (host log, session
//      e3d6121b: zero cancels while the user clicked Stop twice). The open
//      session id is now mirrored into the ref on switch, and the hook
//      falls back to it (matching stopStaleTurn).
import { describe, it, expect, vi, beforeEach } from 'vitest';
import React from 'react';
import { render, screen, waitFor, fireEvent } from '@testing-library/react';
import { I18nProvider } from './i18n';

declare global { interface Window { starriver: any; } }

const WORKSPACE = {
  id: 'w1', path: '/tmp/ws-one', title: 'ws-one', createdAt: '2026-09-27',
  sessionIds: ['s-a', 's-b'],
};
const SESSION_A = { id: 's-a', title: 'Task A', workspaceId: 'w1', createdAt: '2026-09-27', messageCount: 2 };
const SESSION_B = { id: 's-b', title: 'Task B', workspaceId: 'w1', createdAt: '2026-09-27', messageCount: 0 };

beforeEach(() => {
  localStorage.setItem('starriver-language', 'en');
  Element.prototype.scrollIntoView = vi.fn();
  window.starriver = {
    listWorkspaces: vi.fn().mockResolvedValue([WORKSPACE]),
    createWorkspace: vi.fn(),
    setActiveWorkspace: vi.fn().mockResolvedValue(undefined),
    listSessions: vi.fn().mockResolvedValue([SESSION_A, SESSION_B]),
    listArchivedSessions: vi.fn().mockResolvedValue([]),
    getSessionMessages: vi.fn().mockResolvedValue([]),
    listSkills: vi.fn().mockResolvedValue([]),
    listMcpServers: vi.fn().mockResolvedValue([]),
    listModels: vi.fn().mockResolvedValue([]),
    listAgentModelBindings: vi.fn().mockResolvedValue([]),
    // Backend truth: session A holds the slot, session B is idle.
    listRunningSessions: vi.fn().mockResolvedValue(['s-a']),
    cancel: vi.fn().mockResolvedValue(true),
    queueAdd: vi.fn().mockResolvedValue(null),
    getWorktreeStatus: vi.fn().mockResolvedValue({ isRepo: false, branch: '', isWorktree: false, dirty: false }),
    readWorkspaceInstructions: vi.fn().mockResolvedValue(null),
    getCurrentAgent: vi.fn().mockResolvedValue(null),
    listAgentPresets: vi.fn().mockResolvedValue([]),
  };
});

async function mountTwoSessions() {
  const { WorkspaceProvider, WorkspaceSection, CodingView } = await import('./app');
  render(
    <I18nProvider>
      <WorkspaceProvider>
        <WorkspaceSection onOpenCoding={() => {}} />
        <CodingView debugEnabled={false} />
      </WorkspaceProvider>
    </I18nProvider>,
  );
  await waitFor(() => expect(screen.getByText('Task A')).toBeTruthy());
  fireEvent.click(screen.getByText('Task A'));
  await waitFor(() => expect(screen.getByText('Task A')).toBeTruthy());
}

describe('session switch: per-session composer state', () => {
  it('an idle session shows 发送 even when the previous session still streams (sending must not leak)', async () => {
    let streamCb: ((err: string | null, event: any) => void) | null = null;
    window.starriver.execute = vi.fn((_task: string, _sid: string | undefined, callback: (err: string | null, event: any) => void) => {
      streamCb = callback;
      return () => {};
    });
    await mountTwoSessions();
    // Arm a live turn on A: send, then TaskStarted re-arms the busy state.
    const ta = await screen.findByPlaceholderText('Describe what you want to build…');
    fireEvent.change(ta, { target: { value: 'run on a' } });
    fireEvent.keyDown(ta, { key: 'Enter' });
    await waitFor(() => expect(streamCb).not.toBeNull());
    (streamCb as any)!(null, { type: 'TaskStarted', session_id: 's-a' });
    await waitFor(() => expect(screen.getByText('Stop')).toBeTruthy());

    // Switch to the idle session B — the composer must fall back to Send.
    fireEvent.click(screen.getByText('Task B'));
    await waitFor(() => expect(screen.getByText('Send')).toBeTruthy());
    expect(screen.queryByText('Stop')).toBeNull();
  });

  it('switching back to the running session re-arms Stop and cancel targets the right id', async () => {
    let streamCb: ((err: string | null, event: any) => void) | null = null;
    window.starriver.execute = vi.fn((_task: string, _sid: string | undefined, callback: (err: string | null, event: any) => void) => {
      streamCb = callback;
      return () => {};
    });
    window.confirm = vi.fn(() => true);
    await mountTwoSessions();
    const ta = await screen.findByPlaceholderText('Describe what you want to build…');
    fireEvent.change(ta, { target: { value: 'run on a' } });
    fireEvent.keyDown(ta, { key: 'Enter' });
    await waitFor(() => expect(streamCb).not.toBeNull());
    (streamCb as any)!(null, { type: 'TaskStarted', session_id: 's-a' });
    await waitFor(() => expect(screen.getByText('Stop')).toBeTruthy());

    // B (idle) → Send, then back to A (backend says running) → Stop again.
    fireEvent.click(screen.getByText('Task B'));
    await waitFor(() => expect(screen.getByText('Send')).toBeTruthy());
    fireEvent.click(screen.getByText('Task A'));
    await waitFor(() => expect(screen.getByText('Stop')).toBeTruthy());
    fireEvent.click(screen.getByText('Stop'));
    await waitFor(() => expect(window.starriver.cancel).toHaveBeenCalledWith('s-a'));
  });
});
