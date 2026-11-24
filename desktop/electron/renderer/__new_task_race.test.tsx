/// <reference types="vitest/globals" />
// 2026-11-01 "New Task pops the old session": leaving a session via New Task must land
// on the New Task screen and STAY there. Two resurrection vectors were live:
//   1. An in-flight getSessionMessages for the closed session resolved after
//      the clear — its generation guard is only advanced by a NEWER load, so
//      it passed the check and re-opened the conversation
//      (setMessages + setActiveSessionId) over the hero screen.
//   2. TaskStarted/sessionStarted adoption fires whenever the view is empty;
//      the just-closed session's own next turn (queued, scheduled, external
//      harness) therefore pulled the old conversation right back.
// Both are pinned here: the closed session must never resurrect itself, and
// a genuinely NEW session (created by a send from the New Task screen) must
// still adopt.
import { describe, it, expect, vi, beforeEach } from 'vitest';
import React from 'react';
import { render, screen, waitFor, fireEvent } from '@testing-library/react';
import { I18nProvider } from './i18n';

declare global { interface Window { starriver: any; } }

let executeCb: ((err: string | null, event: any) => void) | null = null;

beforeEach(() => {
  executeCb = null;
  Element.prototype.scrollIntoView = vi.fn();
  localStorage.setItem('starriver-language', 'en');
  window.starriver = {
    listWorkspaces: vi.fn().mockResolvedValue([
      { id: 'w1', path: '/tmp/ws-one', title: 'ws-one', createdAt: '2026-11-01', sessionIds: ['s-a'] },
    ]),
    createWorkspace: vi.fn(),
    setActiveWorkspace: vi.fn().mockResolvedValue(undefined),
    listSessions: vi.fn().mockResolvedValue([
      { id: 's-a', title: 'Task A', workspaceId: 'w1', createdAt: '2026-11-01', messageCount: 1 },
    ]),
    listArchivedSessions: vi.fn().mockResolvedValue([]),
    getSessionMessages: vi.fn().mockResolvedValue([]),
    getSessionArchiveMessages: vi.fn().mockResolvedValue([]),
    listSkills: vi.fn().mockResolvedValue([]),
    listMcpServers: vi.fn().mockResolvedValue([]),
    listModels: vi.fn().mockResolvedValue([]),
    listAgentModelBindings: vi.fn().mockResolvedValue([]),
    listRunningSessions: vi.fn().mockResolvedValue([]),
    cancel: vi.fn().mockResolvedValue(true),
    queueAdd: vi.fn().mockResolvedValue(null),
    getWorktreeStatus: vi.fn().mockResolvedValue({ isRepo: false, branch: '', isWorktree: false, dirty: false }),
    readWorkspaceInstructions: vi.fn().mockResolvedValue(null),
    getCurrentAgent: vi.fn().mockResolvedValue(null),
    listAgentPresets: vi.fn().mockResolvedValue([]),
    getAgentMode: vi.fn().mockResolvedValue('standard'),
    getPermission: vi.fn().mockResolvedValue(null),
    execute: vi.fn((_task: string, _sid: string | undefined, cb: (err: string | null, event: any) => void) => {
      executeCb = cb;
    }),
  };
});

async function mount() {
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
}

// Leave the open session via the workspace row's "+" (requestNewConversation).
async function clickNewConversation() {
  const plus = document.querySelector('button[aria-label="New Task"]') as HTMLButtonElement | null;
  expect(plus).not.toBeNull();
  fireEvent.click(plus!);
  // The New Task hero composer must be up.
  await waitFor(() => expect(screen.getByPlaceholderText('Describe what you want to build…')).toBeTruthy());
}

async function send(text: string) {
  const ta = screen.getByPlaceholderText('Describe what you want to build…') as HTMLTextAreaElement;
  fireEvent.change(ta, { target: { value: text } });
  fireEvent.keyDown(ta, { key: 'Enter' });
  await waitFor(() => expect(executeCb).not.toBeNull());
}

describe('new task: the closed session must not pop back up', () => {
  it('an in-flight history load resolving after the close does not re-open the conversation', async () => {
    let resolveA!: (v: any[]) => void;
    window.starriver.getSessionMessages = vi.fn((sid: string) => {
      if (sid === 's-a') {
        return new Promise<any[]>((res) => { resolveA = res; });
      }
      return Promise.resolve([]);
    });
    await mount();

    // Open session A; its history load hangs in flight.
    fireEvent.click(screen.getByText('Task A'));
    await waitFor(() => expect(window.starriver.getSessionMessages).toHaveBeenCalledWith('s-a'));

    // Leave via New Task while the load is still pending.
    await clickNewConversation();

    // The stale load lands now — with the guard bump it must be ignored.
    resolveA([{ role: 'assistant', content: 'A-body-text' }]);
    await new Promise((r) => setTimeout(r, 50));
    expect(screen.queryByText('A-body-text')).toBeNull();
    expect(screen.getByPlaceholderText('Describe what you want to build…')).toBeTruthy();
  });

  it('TaskStarted from the just-closed session does not adopt; a fresh session still does', async () => {
    window.starriver.getSessionMessages = vi.fn((sid: string) => {
      if (sid === 's-new') return Promise.resolve([{ role: 'assistant', content: 'NEW-body' }]);
      return Promise.resolve([{ role: 'assistant', content: 'A-body-text' }]);
    });
    await mount();

    // Arm the event stream: one throwaway send on the fresh New Task view.
    await send('warm the stream');
    // Open A, then leave it via New Task.
    fireEvent.click(screen.getByText('Task A'));
    await waitFor(() => expect(window.starriver.getSessionMessages).toHaveBeenCalledWith('s-a'));
    await clickNewConversation();

    // The closed session's own next turn starts (queued/scheduled/harness) —
    // the empty view must NOT adopt it back. Adoption is observable through
    // its loadSessions() side effect (the only caller in that branch), since
    // the adoption path pins the session ref directly and relies on live
    // stream events rather than a history reload.
    const adoptionsBefore = window.starriver.listSessions.mock.calls.length;
    executeCb!(null, { type: 'TaskStarted', session_id: 's-a' });
    await new Promise((r) => setTimeout(r, 50));
    expect(window.starriver.listSessions.mock.calls.length).toBe(adoptionsBefore);
    expect(screen.getByPlaceholderText('Describe what you want to build…')).toBeTruthy();

    // A genuinely new session (created by this send from the New Task
    // screen) must still adopt: its side effect fires and its live stream
    // events land in the now-open transcript.
    await send('fresh task');
    executeCb!(null, { type: 'TaskStarted', session_id: 's-new' });
    await waitFor(() =>
      expect(window.starriver.listSessions.mock.calls.length).toBeGreaterThan(adoptionsBefore),
    );
    executeCb!(null, { type: 'assistantText', session_id: 's-new', text: 'NEW-body' });
    await waitFor(() => expect(screen.getByText('NEW-body')).toBeTruthy());
  });

  it('the first 新任务 click from another tab lands on the New Task screen, not the restored conversation', async () => {
    const { SidebarNavItems, WorkspaceProvider, WorkspaceSection, CodingView } = await import('./app');
    render(
      <I18nProvider>
        <WorkspaceProvider>
          <WorkspaceSection onOpenCoding={() => {}} />
          <CodingView debugEnabled={false} />
          {/* The App shell renders the nav with the CURRENT tab; simulate
              sitting on 定时任务 (not coding). */}
          <SidebarNavItems tab={'scheduled' as any} onNavigate={() => {}} />
        </WorkspaceProvider>
      </I18nProvider>,
    );
    await waitFor(() => expect(screen.getByText('Task A')).toBeTruthy());

    // Open session A (its conversation fills the view), then click the
    // New Task nav item. The old `tab === 'coding'` gate turned this click into
    // a bare tab switch and CodingView's remount restored the conversation.
    fireEvent.click(screen.getByText('Task A'));
    await waitFor(() => expect(window.starriver.getSessionMessages).toHaveBeenCalledWith('s-a'));
    const navButtons = Array.from(document.querySelectorAll('button.nav-item'));
    const newTaskNav = navButtons.find((b) => b.textContent === 'New Task');
    expect(newTaskNav).toBeTruthy();
    fireEvent.click(newTaskNav!);

    // New Task screen — the closed conversation must NOT come back.
    await waitFor(() => expect(screen.getByPlaceholderText('Describe what you want to build…')).toBeTruthy());
    await new Promise((r) => setTimeout(r, 50));
    expect(screen.queryByText('A-body-text')).toBeNull();
  });

  it('the closed session\'s chat-archive projection does not linger on the New Task screen (2026-11-02 乱界面)', async () => {
    // displayMessages merges `archiveMessages` under `messages`; the close
    // paths used to clear only `messages`, so the hero screen rendered the
    // previous conversation's archived transcript stacked under the composer.
    window.starriver.getSessionArchiveMessages = vi.fn((sid: string) =>
      sid === 's-a'
        ? Promise.resolve([
            { role: 'user', seq: 1, content: 'A-archive-question' },
            { role: 'assistant', seq: 2, content: 'A-archive-body' },
          ])
        : Promise.resolve([]),
    );
    await mount();

    // Open A: the archive projection fills the transcript.
    fireEvent.click(screen.getByText('Task A'));
    await waitFor(() => expect(window.starriver.getSessionArchiveMessages).toHaveBeenCalledWith('s-a'));
    await waitFor(() => expect(screen.getByText('A-archive-body')).toBeTruthy());

    // Leave via New Task: the hero screen shows, and the old transcript must
    // be gone — not stacked below the composer as before the fix.
    await clickNewConversation();
    expect(document.querySelector('.message-list.new-task')).toBeTruthy();
    expect(screen.queryByText('A-archive-body')).toBeNull();
    expect(screen.queryByText('A-archive-question')).toBeNull();

    // Re-opening the same session refetches the archive (the fetch-once
    // watermark reset with the close) and shows it again.
    fireEvent.click(screen.getByText('Task A'));
    await waitFor(() => {
      const calls = window.starriver.getSessionArchiveMessages.mock.calls.filter((c: any[]) => c[0] === 's-a');
      expect(calls.length).toBe(2);
    });
    await waitFor(() => expect(screen.getByText('A-archive-body')).toBeTruthy());
  });
});
