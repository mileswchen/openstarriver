/// <reference types="vitest/globals" />
// CodingView thread commands: /backtrack (fork-before backtrack) + /delete (delete the current session).
// Mirrors the pattern in __coding_commands.test.tsx: sendText goes through the Composer Send button
// (onSend → send()), bypassing the two-Enter interaction of the `/` palette.

import { describe, it, expect, vi, beforeEach } from 'vitest';
import React from 'react';
import { render, screen, fireEvent, waitFor, within } from '@testing-library/react';
import { I18nProvider } from './i18n';

declare global { interface Window { starriver: any; } }

function baseMock() {
  window.starriver = {
    listWorkspaces: vi.fn().mockResolvedValue([
      { id: 'w1', path: '/tmp/demo', title: 'demo', createdAt: '', sessionIds: [] },
    ]),
    createWorkspace: vi.fn(),
    renameWorkspace: vi.fn().mockResolvedValue(undefined),
    deleteWorkspace: vi.fn().mockResolvedValue(undefined),
    listSessions: vi.fn().mockResolvedValue([]),
    listSkills: vi.fn().mockResolvedValue([]),
    listMcpServers: vi.fn().mockResolvedValue([]),
    listModels: vi.fn().mockResolvedValue([]),
    listAgentModelBindings: vi.fn().mockResolvedValue([]),
    switchAgent: vi.fn().mockResolvedValue(undefined),
    getCurrentAgent: vi.fn().mockResolvedValue(null),
    getSessionMessages: vi.fn().mockResolvedValue([]),
    // -- thread command bridge methods --
    getSessionTimeline: vi.fn().mockResolvedValue([]),
    forkSession: vi.fn().mockResolvedValue({
      id: 'fork-1', title: 'task',
      createdAt: '', messageCount: 2, parentId: 's1',
    }),
    deleteSession: vi.fn().mockResolvedValue(undefined),
    renameSession: vi.fn().mockResolvedValue(undefined),
    archiveSession: vi.fn().mockResolvedValue(undefined),
    unarchiveSession: vi.fn().mockResolvedValue(undefined),
    listArchivedSessions: vi.fn().mockResolvedValue([]),
    getWorktreeStatus: vi.fn().mockResolvedValue({ isRepo: true, branch: 'main', isWorktree: false, dirty: false }),
    setSessionIsolation: vi.fn().mockResolvedValue(undefined),
    readWorkspaceInstructions: vi.fn().mockResolvedValue(null),
    setAgentMode: vi.fn().mockResolvedValue(undefined),
    setActiveWorkspace: vi.fn().mockResolvedValue(undefined),
    workspaceDiff: vi.fn().mockResolvedValue(''),
    sessionUsage: vi.fn().mockResolvedValue({ tokens: 0, count: 0 }),
    initProjectAgents: vi.fn().mockResolvedValue(''),
    exportSession: vi.fn().mockResolvedValue(''),
    // Task execution: the synchronous TaskStarted callback seeds session id (s1) and then completes
    execute: vi.fn().mockImplementation((_task: string, _sid: string | undefined, cb: (err: any, ev: any) => void) => {
      cb(null, { type: 'TaskStarted', session_id: 's1' });
      cb(null, { type: 'TaskCompleted', session_id: 's1', summary: 'done' });
    }),
  };
}

beforeEach(() => {
  Element.prototype.scrollIntoView = vi.fn();
  localStorage.setItem('starriver-language', 'en'); // pin English so placeholders and assertions stay stable
  baseMock();
});

async function renderCodingView() {
  const { CodingView, WorkspaceProvider, WorkspaceSection } = await import('./app');
  return render(<I18nProvider><WorkspaceProvider><WorkspaceSection onOpenCoding={() => {}} /><CodingView debugEnabled={false} /></WorkspaceProvider></I18nProvider>);
}

function textarea(): HTMLTextAreaElement {
  const ta = document.querySelector('textarea') as HTMLTextAreaElement | null;
  if (!ta) throw new Error('no composer textarea in DOM');
  return ta;
}

/// Enter a slash command and click Send (onSend → send(), no palette).
function sendText(cmd: string) {
  const ta = textarea();
  fireEvent.change(ta, { target: { value: cmd } });
  fireEvent.click(screen.getByRole('button', { name: 'Send' }));
}

/// Run an ordinary task so the execute mock seeds active session s1.
async function seedActiveSession() {
  await renderCodingView();
  await waitFor(() => expect(screen.getByText('demo')).toBeTruthy());
  fireEvent.click(screen.getByText('demo'));
  await waitFor(() => expect(window.starriver.setActiveWorkspace).toHaveBeenCalledWith('w1', 'Starriver Code'));
  sendText('hello');
  await waitFor(() => expect(window.starriver.execute).toHaveBeenCalled());
}

describe('CodingView thread commands', () => {
  it('lists /backtrack and /delete in the / palette', async () => {
    await renderCodingView();
    const ta = textarea();
    fireEvent.change(ta, { target: { value: '/' } });
    await waitFor(() => expect(screen.getAllByText(/\/backtrack/).length).toBeGreaterThan(0));
    expect(screen.getAllByText(/\/delete/).length).toBeGreaterThan(0);
  });

  it('reports missing session for /backtrack and /delete', async () => {
    await renderCodingView();
    await waitFor(() => expect(screen.getByText('demo')).toBeTruthy());
    // No task started → no active session

    sendText('/backtrack');
    await waitFor(() => expect(screen.getAllByText(/No active session/).length).toBeGreaterThanOrEqual(1));
    expect(window.starriver.getSessionTimeline).not.toHaveBeenCalled();

    sendText('/delete');
    await waitFor(() => expect(screen.getAllByText(/No active session/).length).toBeGreaterThanOrEqual(2));
    expect(window.starriver.deleteSession).not.toHaveBeenCalled();
  });

  it('opens the backtrack picker listing only user messages', async () => {
    window.starriver.getSessionTimeline = vi.fn().mockResolvedValue([
      { id: 3, role: 'user', content: 'first question' },
      { id: 7, role: 'assistant', content: 'first answer' },
      { id: 11, role: 'user', content: 'second question' },
    ]);
    await seedActiveSession();

    sendText('/backtrack');
    await waitFor(() => expect(window.starriver.getSessionTimeline).toHaveBeenCalledWith('s1'));
    await waitFor(() => expect(screen.getByText('Backtrack to a message')).toBeTruthy());
    expect(screen.getByText('first question')).toBeTruthy();
    expect(screen.getByText('second question')).toBeTruthy();
    expect(screen.queryByText('first answer')).toBeNull(); // assistant messages are not listed
  });

  it('selecting a message forks a new thread and switches to it', async () => {
    window.starriver.getSessionTimeline = vi.fn().mockResolvedValue([
      { id: 3, role: 'user', content: 'first question' },
      { id: 11, role: 'user', content: 'second question' },
    ]);
    window.starriver.forkSession = vi.fn().mockResolvedValue({
      id: 'fork-1', title: 'second question',
      createdAt: '', messageCount: 2, parentId: 's1',
    });
    window.starriver.getSessionMessages = vi.fn().mockImplementation((id: string) => {
      if (id === 'fork-1') {
        return Promise.resolve([
          { role: 'user', content: 'first question' },
          { role: 'assistant', content: 'first answer' },
        ]);
      }
      return Promise.resolve([]);
    });
    await seedActiveSession();

    sendText('/backtrack');
    await waitFor(() => expect(screen.getByText('second question')).toBeTruthy());

    fireEvent.click(screen.getByText('second question'));
    await waitFor(() => expect(window.starriver.forkSession).toHaveBeenCalledWith('s1', 11, 'second question'));

    // The new thread history is loaded into the view
    expect(await screen.findByText('first answer')).toBeTruthy();
    expect(screen.queryByText('Backtrack to a message')).toBeNull(); // picker closed
    // The input box prefills the backtrack-point original text
    await waitFor(() => expect(textarea().value).toBe('second question'));
    // Status feedback
    await waitFor(() => expect(screen.getAllByText(/Forked new thread/).length).toBeGreaterThanOrEqual(1));
  });

  it('confirms /delete and removes the current session', async () => {
    await seedActiveSession();

    sendText('/delete');
    await waitFor(() => expect(screen.getByText('Delete this session?')).toBeTruthy());

    const card = screen.getByText('Delete this session?').closest('.modal-card') as HTMLElement;
    fireEvent.click(within(card).getByRole('button', { name: 'Delete' }));

    await waitFor(() => expect(window.starriver.deleteSession).toHaveBeenCalledWith('s1'));
    await waitFor(() => expect(screen.getAllByText(/Session deleted/).length).toBeGreaterThanOrEqual(1));
  });

  it('cancel on /delete keeps the session', async () => {
    await seedActiveSession();

    sendText('/delete');
    await waitFor(() => expect(screen.getByText('Delete this session?')).toBeTruthy());

    const card = screen.getByText('Delete this session?').closest('.modal-card') as HTMLElement;
    fireEvent.click(within(card).getByRole('button', { name: 'Cancel' }));

    await waitFor(() => expect(screen.queryByText('Delete this session?')).toBeNull());
    expect(window.starriver.deleteSession).not.toHaveBeenCalled();
  });

  it('lists /name /archive /unarchive /help in the / palette', async () => {
    await renderCodingView();
    const ta = textarea();
    fireEvent.change(ta, { target: { value: '/' } });
    await waitFor(() => expect(screen.getAllByText(/\/name/).length).toBeGreaterThan(0));
    expect(screen.getAllByText(/\/archive/).length).toBeGreaterThan(0);
    expect(screen.getAllByText(/\/unarchive/).length).toBeGreaterThan(0);
    expect(screen.getAllByText(/\/help/).length).toBeGreaterThan(0);
  });

  it('/name reports missing session', async () => {
    await renderCodingView();
    await waitFor(() => expect(screen.getByText('demo')).toBeTruthy());

    sendText('/name');
    await waitFor(() => expect(screen.getAllByText(/No active session/).length).toBeGreaterThanOrEqual(1));
    expect(window.starriver.renameSession).not.toHaveBeenCalled();
  });

  it('/name with no title shows usage on an active session', async () => {
    await seedActiveSession();

    sendText('/name');
    await waitFor(() => expect(screen.getAllByText(/Usage: \/name/).length).toBeGreaterThanOrEqual(1));
    expect(window.starriver.renameSession).not.toHaveBeenCalled();
  });

  it('/name renames the active session and refreshes', async () => {
    await seedActiveSession();

    sendText('/name 我的命名');
    await waitFor(() => expect(window.starriver.renameSession).toHaveBeenCalledWith('s1', '我的命名'));
    await waitFor(() => expect(screen.getAllByText(/Session renamed/).length).toBeGreaterThanOrEqual(1));
  });

  it('/archive archives the active session and clears the view', async () => {
    await seedActiveSession();

    sendText('/archive');
    await waitFor(() => expect(window.starriver.archiveSession).toHaveBeenCalledWith('s1'));
    await waitFor(() => expect(screen.getAllByText(/Session archived/).length).toBeGreaterThanOrEqual(1));
    // View cleared: no residual user messages
    expect(screen.queryByText('hello')).toBeNull();
  });

  it('/unarchive opens the archive modal and restores a session', async () => {
    window.starriver.listArchivedSessions = vi.fn().mockResolvedValue([
      { id: 'a1', title: 'old task', createdAt: '', messageCount: 3 },
    ]);
    await renderCodingView();
    await waitFor(() => expect(screen.getByText('demo')).toBeTruthy());

    sendText('/unarchive');
    await waitFor(() => expect(window.starriver.listArchivedSessions).toHaveBeenCalled());
    await waitFor(() => expect(screen.getByText('Archived Sessions')).toBeTruthy());
    fireEvent.click(screen.getByText('old task'));
    await waitFor(() => expect(window.starriver.unarchiveSession).toHaveBeenCalledWith('a1'));
    await waitFor(() => expect(screen.getAllByText(/Session restored/).length).toBeGreaterThanOrEqual(1));
  });

  it('/help renders the command list', async () => {
    await renderCodingView();
    await waitFor(() => expect(screen.getByText('demo')).toBeTruthy());

    sendText('/help');
    await waitFor(() => expect(screen.getAllByText(/Available Commands/).length).toBeGreaterThanOrEqual(1));
    expect(screen.getAllByText(/\/backtrack —/).length).toBeGreaterThanOrEqual(1);
  });

  it('sidebar double-click renames a session inline', async () => {
    window.starriver.listSessions = vi.fn().mockResolvedValue([
      { id: 's1', title: 'old title', workspaceId: 'w1', createdAt: '', messageCount: 1 },
    ]);
    window.starriver.listArchivedSessions = vi.fn().mockResolvedValue([]);
    await renderCodingView();
    await waitFor(() => expect(screen.getByText('demo')).toBeTruthy());
    // Session rows become visible only after expanding a project
    fireEvent.click(screen.getByText('demo'));
    await waitFor(() => expect(screen.getByText('old title')).toBeTruthy());

    fireEvent.doubleClick(screen.getByText('old title'));
    const input = document.querySelector('input[value="old title"]') as HTMLInputElement | null;
    expect(input).toBeTruthy();
    fireEvent.change(input!, { target: { value: 'new name' } });
    fireEvent.keyDown(input!, { key: 'Enter' });
    await waitFor(() => expect(window.starriver.renameSession).toHaveBeenCalledWith('s1', 'new name'));
  });
});
