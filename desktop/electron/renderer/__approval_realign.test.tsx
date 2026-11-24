/// <reference types="vitest/globals" />
// WI-247 approval re-alignment: the pending approval must be recoverable from
// the host's audit-log fold (`approval.pendingState`) when the live
// `approvalRequested` relay is missed. Three pinned regressions:
//   1. Switching to a session whose approval was asked while we were away
//      restores the approve panel (2026-11-21: turn wedged forever, no
//      panel, no recovery).
//   2. A session with nothing pending stays cleared (no phantom panel).
//   3. An alignment result that resolves AFTER the user switched away is
//      dropped — the panel must never surface for the wrong session.
//   4. A live relay lost mid-turn (toolCall without a following
//      approvalRequested) recovers within the delayed re-alignment window.
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

const PENDING = {
  pending: true, requestId: 'ap-9', toolName: 'write',
  reason: 'outside workspace', arguments: '{"path":"/tmp/out.txt"}',
};

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
    listRunningSessions: vi.fn().mockResolvedValue([]),
    cancel: vi.fn().mockResolvedValue(true),
    queueAdd: vi.fn().mockResolvedValue(null),
    queueList: vi.fn().mockResolvedValue([]),
    getWorktreeStatus: vi.fn().mockResolvedValue({ isRepo: false, branch: '', isWorktree: false, dirty: false }),
    readWorkspaceInstructions: vi.fn().mockResolvedValue(null),
    getCurrentAgent: vi.fn().mockResolvedValue(null),
    listAgentPresets: vi.fn().mockResolvedValue([]),
    getPermission: vi.fn().mockResolvedValue({ preset: 'manual', sandbox: 'workspace' }),
    approve: vi.fn().mockResolvedValue(undefined),
    appendDiag: vi.fn(),
    // Default: nothing pending anywhere.
    pendingApprovalState: vi.fn().mockResolvedValue({ pending: false }),
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

describe('WI-247 approval re-alignment', () => {
  it('switching to a session with a pending approval restores the panel from the audit fold', async () => {
    window.starriver.pendingApprovalState = vi.fn((sid: string) =>
      sid === 's-b'
        ? Promise.resolve(PENDING)
        : Promise.resolve({ pending: false }),
    );
    await mountTwoSessions();
    expect(screen.queryByText('Deny')).toBeNull();
    fireEvent.click(screen.getByText('Task B'));
    await waitFor(() => expect(screen.getByText('Deny')).toBeTruthy());
    expect(window.starriver.pendingApprovalState).toHaveBeenCalledWith('s-b');
  });

  it('a session with nothing pending keeps the panel cleared', async () => {
    await mountTwoSessions();
    fireEvent.click(screen.getByText('Task B'));
    await waitFor(() =>
      expect(window.starriver.pendingApprovalState).toHaveBeenCalledWith('s-b'),
    );
    // Give any (wrong) restore a beat to surface.
    await new Promise((r) => setTimeout(r, 250));
    expect(screen.queryByText('Deny')).toBeNull();
  });

  it('an alignment result that resolves after switching away is dropped', async () => {
    let resolveB: ((v: any) => void) | null = null;
    window.starriver.pendingApprovalState = vi.fn((sid: string) => {
      if (sid !== 's-b') return Promise.resolve({ pending: false });
      return new Promise((resolve) => { resolveB = resolve; });
    });
    await mountTwoSessions();
    fireEvent.click(screen.getByText('Task B'));
    await waitFor(() => expect(resolveB).not.toBeNull());
    // Leave B before its alignment resolves.
    fireEvent.click(screen.getByText('Task A'));
    await waitFor(() => expect(screen.getByText('Task A')).toBeTruthy());
    resolveB!(PENDING);
    await new Promise((r) => setTimeout(r, 300));
    expect(screen.queryByText('Deny')).toBeNull();
  });

  it('a live relay lost mid-turn recovers via the delayed fold', async () => {
    let streamCb: ((err: string | null, event: any) => void) | null = null;
    window.starriver.execute = vi.fn((_task: string, _sid: string | undefined, callback: (err: string | null, event: any) => void) => {
      streamCb = callback;
      return () => {};
    });
    await mountTwoSessions();
    const ta = await screen.findByPlaceholderText('Describe what you want to build…');
    fireEvent.change(ta, { target: { value: 'write something' } });
    fireEvent.keyDown(ta, { key: 'Enter' });
    await waitFor(() => expect(streamCb).not.toBeNull());
    (streamCb as any)!(null, { type: 'TaskStarted', session_id: 's-a' });
    // The ask arrives… but the approvalRequested envelope never does.
    (streamCb as any)!(null, { type: 'toolCall', name: 'write', id: 'c-1', input: { path: '/tmp/out.txt' } });
    await new Promise((r) => setTimeout(r, 200));
    expect(screen.queryByText('Deny')).toBeNull();
    // The audit fold reports the ask as still pending.
    window.starriver.pendingApprovalState = vi.fn().mockResolvedValue(PENDING);
    await waitFor(() => expect(screen.getByText('Deny')).toBeTruthy(), { timeout: 4000 });
  });
});
