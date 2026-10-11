/// <reference types="vitest/globals" />
// CodingView slash commands: /diff /usage /init /export /lang walk the full Composer →
// send() → window.starriver bridge call chain. Mirrors the pattern in __worktree_ui.test.tsx.
// The input-box query uses a tag (placeholder varies by language); sending uses the Composer Send button
// (onSend → send()), bypassing the two-Enter interaction of the `/` palette.

import { describe, it, expect, vi, beforeEach } from 'vitest';
import React from 'react';
import { render, screen, fireEvent, waitFor } from '@testing-library/react';
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
    setActiveWorkspace: vi.fn().mockResolvedValue(undefined),
    readWorkspaceInstructions: vi.fn().mockResolvedValue(null),
    listSessions: vi.fn().mockResolvedValue([]),
    listSkills: vi.fn().mockResolvedValue([]),
    listMcpServers: vi.fn().mockResolvedValue([]),
    listModels: vi.fn().mockResolvedValue([]),
    listAgentModelBindings: vi.fn().mockResolvedValue([]),
    switchAgent: vi.fn().mockResolvedValue(undefined),
    getCurrentAgent: vi.fn().mockResolvedValue(null),
    getSessionMessages: vi.fn().mockResolvedValue([]),
    getWorktreeStatus: vi.fn().mockResolvedValue({ isRepo: true, branch: 'main', isWorktree: false, dirty: false }),
    setSessionIsolation: vi.fn().mockResolvedValue(undefined),
    setAgentMode: vi.fn().mockResolvedValue(undefined),
    // -- new-command bridge methods --
    workspaceDiff: vi.fn().mockResolvedValue('diff --git a/f.rs b/f.rs\n+hello world\n'),
    sessionUsage: vi.fn().mockResolvedValue({ tokens: 500, count: 12 }),
    initProjectAgents: vi.fn().mockResolvedValue('/tmp/demo/AGENTS.md'),
    exportSession: vi.fn().mockResolvedValue('/tmp/demo/starriver-export-s1.md'),
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

describe('CodingView slash commands', () => {
  it('lists the new commands in the / palette', async () => {
    await renderCodingView();
    const ta = textarea();
    fireEvent.change(ta, { target: { value: '/' } });
    await waitFor(() => expect(screen.getAllByText(/\/diff/).length).toBeGreaterThan(0));
    expect(screen.getByText(/\/usage/)).toBeTruthy();
    expect(screen.getByText(/\/init/)).toBeTruthy();
    expect(screen.getByText(/\/export/)).toBeTruthy();
    expect(screen.getByText(/\/lang/)).toBeTruthy();
  });

  it('runs /diff against the active workspace', async () => {
    await renderCodingView();
    await waitFor(() => expect(screen.getByText('demo')).toBeTruthy());
    fireEvent.click(screen.getByText('demo')); // activate the workspace → activeWorkspaceId = w1
    await waitFor(() => expect(window.starriver.setActiveWorkspace).toHaveBeenCalledWith('w1', 'Starriver Code'));

    sendText('/diff');
    await waitFor(() => expect(window.starriver.workspaceDiff).toHaveBeenCalledWith('/tmp/demo'));
    expect(await screen.findByText(/hello world/)).toBeTruthy();
  });

  it('reports missing workspace for /diff and /init', async () => {
    await renderCodingView();
    await waitFor(() => expect(screen.getByText('demo')).toBeTruthy());
    // No workspace activated → activeWorkspaceId is null

    sendText('/diff');
    await waitFor(() => expect(screen.getAllByText(/No workspace directory/).length).toBeGreaterThanOrEqual(1));
    expect(window.starriver.workspaceDiff).not.toHaveBeenCalled();

    sendText('/init');
    await waitFor(() => expect(screen.getAllByText(/No workspace directory/).length).toBeGreaterThanOrEqual(2));
    expect(window.starriver.initProjectAgents).not.toHaveBeenCalled();
  });

  it('runs /usage and /export after a session starts', async () => {
    await renderCodingView();
    await waitFor(() => expect(screen.getByText('demo')).toBeTruthy());
    fireEvent.click(screen.getByText('demo')); // /export needs the workspace path
    await waitFor(() => expect(window.starriver.setActiveWorkspace).toHaveBeenCalledWith('w1', 'Starriver Code'));

    // First run an ordinary task → the execute mock synchronously seeds session id s1
    sendText('hello');
    await waitFor(() => expect(window.starriver.execute).toHaveBeenCalled());

    sendText('/usage');
    await waitFor(() => expect(window.starriver.sessionUsage).toHaveBeenCalledWith('s1'));
    await waitFor(() => expect(screen.getAllByText(/500 tokens/).length).toBeGreaterThanOrEqual(1));

    sendText('/export');
    await waitFor(() => expect(window.starriver.exportSession).toHaveBeenCalledWith('s1', '/tmp/demo'));
    await waitFor(() => expect(screen.getAllByText(/starriver-export-s1.md/).length).toBeGreaterThanOrEqual(1));
  });

  it('reports missing session for /usage and /export', async () => {
    await renderCodingView();
    await waitFor(() => expect(screen.getByText('demo')).toBeTruthy());

    sendText('/usage');
    await waitFor(() => expect(screen.getAllByText(/No active session/).length).toBeGreaterThanOrEqual(1));
    expect(window.starriver.sessionUsage).not.toHaveBeenCalled();

    sendText('/export');
    await waitFor(() => expect(screen.getAllByText(/No active session/).length).toBeGreaterThanOrEqual(2));
    expect(window.starriver.exportSession).not.toHaveBeenCalled();
  });

  it('toggles UI language via /lang', async () => {
    await renderCodingView();
    await waitFor(() => expect(screen.getByText('demo')).toBeTruthy());

    sendText('/lang');
    await waitFor(() => expect(localStorage.getItem('starriver-language')).toBe('zh'));
    await waitFor(() => expect(screen.getAllByText(/Language switched to/).length).toBeGreaterThanOrEqual(1));
  });
});
