/// <reference types="vitest/globals" />
// BUG (2026-10-07): switching conversations while a turn runs printed the old
// session's live stream into the newly opened transcript. The stream callback
// gated on "view is busy" only; the session gate must drop events whose
// session_id ≠ the open session (TaskStarted exempt — it re-engages).
import { describe, it, expect, vi, beforeEach } from 'vitest';
import React from 'react';
import { render, screen, fireEvent, waitFor } from '@testing-library/react';
import { I18nProvider } from './i18n';

declare global { interface Window { starriver: any; } }

let executeCb: ((err: string | null, event: any) => void) | null = null;

beforeEach(() => {
  executeCb = null;
  Element.prototype.scrollIntoView = vi.fn();
  localStorage.setItem('starriver-language', 'en');
  window.starriver = {
    listWorkspaces: vi.fn().mockResolvedValue([
      { id: 'w1', path: '/tmp/ws', title: 'ws', createdAt: '', sessionIds: ['sess-a', 'sess-b'] },
    ]),
    getActiveWorkspace: vi.fn().mockResolvedValue('w1'),
    createWorkspace: vi.fn(),
    renameWorkspace: vi.fn().mockResolvedValue(undefined),
    deleteWorkspace: vi.fn().mockResolvedValue(undefined),
    setActiveWorkspace: vi.fn().mockResolvedValue(undefined),
    listSessions: vi.fn().mockResolvedValue([
      { id: 'sess-a', title: 'running task', workspaceId: 'w1', createdAt: '', messageCount: 2 },
      { id: 'sess-b', title: 'other chat', workspaceId: 'w1', createdAt: '', messageCount: 2 },
    ]),
    getSessionMessages: vi.fn().mockResolvedValue([
      { role: 'user', content: 'hello B' },
    ]),
    listArchivedSessions: vi.fn().mockResolvedValue([]),
    listSkills: vi.fn().mockResolvedValue([]),
    listMcpServers: vi.fn().mockResolvedValue([]),
    listModels: vi.fn().mockResolvedValue([]),
    listAgentModelBindings: vi.fn().mockResolvedValue([]),
    switchAgent: vi.fn().mockResolvedValue(undefined),
    getCurrentAgent: vi.fn().mockResolvedValue(null),
    getWorktreeStatus: vi.fn().mockResolvedValue({ isRepo: false, branch: null, isWorktree: false, dirty: false }),
    setSessionIsolation: vi.fn().mockResolvedValue(undefined),
    readWorkspaceInstructions: vi.fn().mockResolvedValue(null),
    setAgentMode: vi.fn().mockResolvedValue(undefined),
    getAgentMode: vi.fn().mockResolvedValue('standard'),
    setApprovalMode: vi.fn().mockResolvedValue(undefined),
    confirmPlan: vi.fn().mockResolvedValue(undefined),
    setAgentActiveModel: vi.fn().mockResolvedValue(undefined),
    setAgentReasoningEffort: vi.fn().mockResolvedValue(undefined),
    approve: vi.fn().mockResolvedValue(undefined),
    execute: vi.fn((_task: string, _sid: string | undefined, cb: (err: string | null, event: any) => void) => {
      executeCb = cb;
    }),
  };
});

describe('stream events are session-scoped (no cross-conversation leak)', () => {
  it('a running session\'s events stop printing after switching to another conversation', async () => {
    const { CodingView, WorkspaceProvider, WorkspaceSection } = await import('./app');
    render(<I18nProvider><WorkspaceProvider><WorkspaceSection onOpenCoding={() => {}} /><CodingView debugEnabled={false} /></WorkspaceProvider></I18nProvider>);

    // Turn starts in session A (new task view → sessionStarted names it).
    const ta = await screen.findByPlaceholderText('Describe what you want to build…');
    fireEvent.change(ta, { target: { value: 'work on A' } });
    fireEvent.click(screen.getByText('Send'));
    executeCb!(null, { type: 'sessionStarted', session_id: 'sess-a' });
    executeCb!(null, { type: 'thinkingDelta', session_id: 'sess-a', text: 'A thinking' });
    await waitFor(() =>
      expect(document.querySelector('.thinking-row-summary')?.textContent).toContain('A thinking'),
    );

    // Switch to conversation B (idle).
    fireEvent.click(await screen.findByText('other chat'));
    await waitFor(() => expect((window as any).starriver.getSessionMessages).toHaveBeenCalledWith('sess-b'));

    // A's stream keeps running — its events must NOT land in B's transcript.
    executeCb!(null, { type: 'thinkingDelta', session_id: 'sess-a', text: 'A MORE thinking' });
    executeCb!(null, { type: 'assistantText', session_id: 'sess-a', text: 'A answer' });
    executeCb!(null, { type: 'toolCall', session_id: 'sess-a', id: 't1', name: 'Shell' });
    await waitFor(() =>
      expect(document.body.textContent).not.toContain('A MORE thinking'),
    );
    expect(document.body.textContent).not.toContain('A answer');
    // B's own history (from getSessionMessages) is what's on screen.
    expect(document.body.textContent).not.toContain('A thinking');
  });
});
