/// <reference types="vitest/globals" />
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
    listWorkspaces: vi.fn().mockResolvedValue([]),
    createWorkspace: vi.fn(),
    renameWorkspace: vi.fn().mockResolvedValue(undefined),
    deleteWorkspace: vi.fn().mockResolvedValue(undefined),
    setActiveWorkspace: vi.fn().mockResolvedValue(undefined),
    listSessions: vi.fn().mockResolvedValue([]),
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
    execute: vi.fn((_task: string, _sid: string | undefined, cb: (err: string | null, event: any) => void) => {
      executeCb = cb;
    }),
  };
});

describe('CodingView turnError handling', () => {
  it('surfaces the error and leaves the Thinking state instead of hanging', async () => {
    const { CodingView, WorkspaceProvider, WorkspaceSection } = await import('./app');
    render(<I18nProvider><WorkspaceProvider><WorkspaceSection onOpenCoding={() => {}} /><CodingView debugEnabled={false} /></WorkspaceProvider></I18nProvider>);

    const ta = await screen.findByPlaceholderText('Describe what you want to build…');
    fireEvent.change(ta, { target: { value: 'hello' } });
    fireEvent.click(screen.getByText('Send'));

    // The turn starts; the button is in its stopping state.
    await waitFor(() => expect(executeCb).not.toBeNull());

    // Kernel emits a terminal turnError (no TaskFailed arrives).
    executeCb!(null, { type: 'turnError', sessionId: 's1', error: 'no adapter for provider `mock`' });

    await screen.findByText(/no adapter for provider/);
    await waitFor(() => {
      const buttons = screen.getAllByRole('button').map(b => b.textContent);
      expect(buttons).toContain('Send');
      expect(buttons).not.toContain('Stop');
    });
  });

  it('renders assistant text and settles on the modern event contract', async () => {
    const { CodingView, WorkspaceProvider, WorkspaceSection } = await import('./app');
    render(<I18nProvider><WorkspaceProvider><WorkspaceSection onOpenCoding={() => {}} /><CodingView debugEnabled={false} /></WorkspaceProvider></I18nProvider>);

    const ta = await screen.findByPlaceholderText('Describe what you want to build…');
    fireEvent.change(ta, { target: { value: 'hello' } });
    fireEvent.click(screen.getByText('Send'));
    await waitFor(() => expect(executeCb).not.toBeNull());

    executeCb!(null, { type: 'sessionStarted', sessionId: 's1' });
    executeCb!(null, { type: 'assistantText', sessionId: 's1', text: 'hello world' });
    executeCb!(null, { type: 'turnEnd', sessionId: 's1', reason: 'completed', summary: '' });

    await screen.findByText('hello world');
    await waitFor(() => {
      const buttons = screen.getAllByRole('button').map(b => b.textContent);
      expect(buttons).toContain('Send');
      expect(buttons).not.toContain('Stop');
    });
  });

  it('renders exactly one assistant bubble when thinking precedes the text', async () => {
    const { CodingView, WorkspaceProvider, WorkspaceSection } = await import('./app');
    render(<I18nProvider><WorkspaceProvider><WorkspaceSection onOpenCoding={() => {}} /><CodingView debugEnabled={false} /></WorkspaceProvider></I18nProvider>);

    const ta = await screen.findByPlaceholderText('Describe what you want to build…');
    fireEvent.change(ta, { target: { value: 'hello' } });
    fireEvent.click(screen.getByText('Send'));
    await waitFor(() => expect(executeCb).not.toBeNull());

    executeCb!(null, { type: 'sessionStarted', sessionId: 's1' });
    executeCb!(null, { type: 'thinkingText', sessionId: 's1', text: 'reasoning here' });
    executeCb!(null, { type: 'assistantText', sessionId: 's1', text: 'final answer' });
    executeCb!(null, { type: 'turnEnd', sessionId: 's1', reason: 'completed', summary: '' });

    await screen.findByText('final answer');
    await waitFor(() => {
      const assistantBubbles = screen
        .getAllByText('final answer')
        .filter(el => el.closest('.message')?.classList.contains('assistant'));
      expect(assistantBubbles.length).toBe(1);
    });
  });

  it('appends the error to the LAST assistant bubble in the modern assistantText flow', async () => {
    // 2026-10-04 regression (session 72a13c58 turn 26): a provider 500 after
    // several completed steps ended the turn with NO error marker — the
    // turnError case wrote into the send-time placeholder via a stale
    // closure id, while the visible bubble (step 11's mid-sentence text, its
    // assistantText already nulled assistantIdRef) stayed untouched. The
    // error must append to the last assistant bubble and keep its text.
    const { CodingView, WorkspaceProvider, WorkspaceSection } = await import('./app');
    render(<I18nProvider><WorkspaceProvider><WorkspaceSection onOpenCoding={() => {}} /><CodingView debugEnabled={false} /></WorkspaceProvider></I18nProvider>);

    const ta = await screen.findByPlaceholderText('Describe what you want to build…');
    fireEvent.change(ta, { target: { value: 'hello' } });
    fireEvent.click(screen.getByText('Send'));
    await waitFor(() => expect(executeCb).not.toBeNull());

    executeCb!(null, { type: 'sessionStarted', sessionId: 's1' });
    executeCb!(null, { type: 'assistantText', sessionId: 's1', text: 'partial fix write-up before the tool call' });
    // No turnEnd: the next model call fails after the retry ladder.
    executeCb!(null, { type: 'turnError', sessionId: 's1', error: 'model call failed: SERVER: 网络错误' });

    await screen.findByText(/网络错误/);
    await waitFor(() => {
      const bubble = screen
        .getAllByText(/网络错误/)[0]
        .closest('.message')!;
      expect(bubble.classList.contains('assistant')).toBe(true);
      // The streamed text is preserved and shares the same bubble.
      expect(bubble.textContent).toContain('partial fix write-up before the tool call');
    });
    await waitFor(() => {
      const buttons = screen.getAllByRole('button').map(b => b.textContent);
      expect(buttons).toContain('Send');
      expect(buttons).not.toContain('Stop');
    });
  });

  it('answers approval requests with the modern camelCase session id', async () => {
    const { CodingView, WorkspaceProvider, WorkspaceSection } = await import('./app');
    render(<I18nProvider><WorkspaceProvider><WorkspaceSection onOpenCoding={() => {}} /><CodingView debugEnabled={false} /></WorkspaceProvider></I18nProvider>);

    const ta = await screen.findByPlaceholderText('Describe what you want to build…');
    fireEvent.change(ta, { target: { value: 'hello' } });
    fireEvent.click(screen.getByText('Send'));
    await waitFor(() => expect(executeCb).not.toBeNull());

    // Modern UiEvent contract uses camelCase fields: sessionId, requestId,
    // toolName. The approve call must carry the session id.
    executeCb!(null, { type: 'sessionStarted', sessionId: 's1' });
    executeCb!(null, {
      type: 'approvalRequested',
      sessionId: 's1',
      requestId: 'r1',
      toolName: 'bash',
      reason: 'tool "bash" requires approval',
    });

    fireEvent.click(await screen.findByText('Allow Once'));

    await waitFor(() => {
      expect(window.starriver.approve).toHaveBeenCalledWith('s1', true, false);
    });
  });
});


describe('WI-35 instructionsBudget event', () => {
  it('renders a compaction-style line when the instructions composite is clipped', async () => {
    const { CodingView, WorkspaceProvider, WorkspaceSection } = await import('./app');
    render(<I18nProvider><WorkspaceProvider><WorkspaceSection onOpenCoding={() => {}} /><CodingView debugEnabled={false} /></WorkspaceProvider></I18nProvider>);

    const ta = await screen.findByPlaceholderText('Describe what you want to build…');
    fireEvent.change(ta, { target: { value: 'hello' } });
    fireEvent.click(screen.getByText('Send'));
    await waitFor(() => expect(executeCb).not.toBeNull());

    executeCb!(null, { type: 'sessionStarted', sessionId: 's1' });
    executeCb!(null, {
      type: 'instructionsBudget',
      sessionId: 's1',
      truncated: true,
      totalSections: 4,
      budgetBytes: 32768,
      composedBytes: 51200,
    });
    executeCb!(null, { type: 'assistantText', sessionId: 's1', text: 'done' });
    executeCb!(null, { type: 'turnEnd', sessionId: 's1', reason: 'completed', summary: '' });

    // The fold row lands in the transcript (compaction-style) and carries
    // both byte counts via i18n interpolation once expanded.
    await screen.findByText(/done/);
    const row = screen.getByTestId('compaction-row');
    fireEvent.click(row.querySelector('button')!);
    await waitFor(() => {
      const body = row.textContent || '';
      expect(body).toContain('32768');
      expect(body).toContain('51200');
    });
  });
});
