/// <reference types="vitest/globals" />
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
    listWorkspaces: vi.fn().mockResolvedValue([]),
    createWorkspace: vi.fn(),
    renameWorkspace: vi.fn().mockResolvedValue(undefined),
    deleteWorkspace: vi.fn().mockResolvedValue(undefined),
    setActiveWorkspace: vi.fn().mockResolvedValue(undefined),
    getActiveWorkspace: vi.fn().mockResolvedValue(null),
    listSessions: vi.fn().mockResolvedValue([]),
    getSessionMessages: vi.fn().mockResolvedValue([]),
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

describe('CodingView thinking rows split per turn step', () => {
  it('每一步的 reasoning 各自成行并挂在对应 step 分组内', async () => {
    const { CodingView, WorkspaceProvider, WorkspaceSection } = await import('./app');
    render(<I18nProvider><WorkspaceProvider><WorkspaceSection onOpenCoding={() => {}} /><CodingView debugEnabled={false} /></WorkspaceProvider></I18nProvider>);

    const ta = await screen.findByPlaceholderText('Describe what you want to build…');
    fireEvent.change(ta, { target: { value: 'do two steps' } });
    fireEvent.click(screen.getByText('Send'));

    executeCb!(null, { type: 'sessionStarted', sessionId: 's1', session_id: 's1' });
    executeCb!(null, { type: 'thinkingDelta', text: 'step one thinking' });
    await waitFor(() =>
      expect(document.querySelector('.thinking-row-summary')?.textContent).toContain('step one thinking')
    );
    executeCb!(null, { type: 'assistantText', text: 'answer one' });
    executeCb!(null, { type: 'thinkingDelta', text: 'step two thinking' });
    await waitFor(() => {
      const summaries = Array.from(document.querySelectorAll('.thinking-row-summary'));
      expect(summaries.length).toBe(2);
      expect(summaries[1].textContent).toContain('step two thinking');
    });
    executeCb!(null, { type: 'assistantText', text: 'answer two' });

    await waitFor(() => {
      const steps = Array.from(document.querySelectorAll('.turn-step'));
      expect(steps.length).toBe(2);
      expect(steps[0].querySelector('.thinking-row-summary')?.textContent).toContain('step one thinking');
      expect(steps[0].textContent).toContain('answer one');
      expect(steps[1].querySelector('.thinking-row-summary')?.textContent).toContain('step two thinking');
    });
    // 2026-10-06 fix: while the turn is LIVE the last prose stays inside its
    // step in trajectory order — hoisting it mid-turn pinned the text below
    // the fold while later steps kept stacking above it (timeline inverted).
    const liveSteps = Array.from(document.querySelectorAll('.turn-step'));
    const answerTwo = screen.getByText('answer two');
    expect(liveSteps.map(s => s.contains(answerTwo))).toEqual([false, true]);
    expect(liveSteps[1].compareDocumentPosition(answerTwo) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    // After turnEnd the fold auto-closes and the last prose is hoisted out as
    // the closing summary (WI-147) — still visible below the fold row.
    executeCb!(null, { type: 'turnEnd' });
    await waitFor(() => {
      expect(document.querySelectorAll('.turn-step').length).toBe(0);
      expect(screen.getByText('answer two')).toBeTruthy();
    });
  });

  it('a pure-tool step closes its thinking at the first tool call (no assistantText)', async () => {
    const { CodingView, WorkspaceProvider, WorkspaceSection } = await import('./app');
    render(<I18nProvider><WorkspaceProvider><WorkspaceSection onOpenCoding={() => {}} /><CodingView debugEnabled={false} /></WorkspaceProvider></I18nProvider>);

    const ta = await screen.findByPlaceholderText('Describe what you want to build…');
    fireEvent.change(ta, { target: { value: 'run a tool' } });
    fireEvent.click(screen.getByText('Send'));
    await waitFor(() => expect(executeCb).not.toBeNull());

    // Pure-tool step: reasoning streams, then a tool call lands with NO
    // assistantText — the first toolCall must close the thinking row, and the
    // next step's thinking opens a fresh row instead of appending.
    executeCb!(null, { type: 'sessionStarted', sessionId: 's1', session_id: 's1' });
    executeCb!(null, { type: 'thinkingDelta', text: 'tool step thinking' });
    await waitFor(() =>
      expect(document.querySelector('.thinking-row-summary')?.textContent).toContain('tool step thinking')
    );
    executeCb!(null, { type: 'toolCall', id: 'c1', name: 'bash', input: '{"command":"ls"}' });
    executeCb!(null, { type: 'thinkingDelta', text: 'next step thinking' });
    await waitFor(() => {
      const summaries = Array.from(document.querySelectorAll('.thinking-row-summary'));
      expect(summaries.length).toBe(2);
      expect(summaries[1].textContent).toContain('next step thinking');
    });
    // The first thinking row froze (no running sweep) once the tool call landed.
    const first = document.querySelector('.thinking-row-details > summary');
    expect(first?.getAttribute('data-state')).toBe('ok');
  });

  it('纯工具回合(正文最后才到):thinking/工具按发生顺序交错,正文落在过程行之后', async () => {
    const { CodingView, WorkspaceProvider, WorkspaceSection } = await import('./app');
    render(<I18nProvider><WorkspaceProvider><WorkspaceSection onOpenCoding={() => {}} /><CodingView debugEnabled={false} /></WorkspaceProvider></I18nProvider>);

    const ta = await screen.findByPlaceholderText('Describe what you want to build…');
    fireEvent.change(ta, { target: { value: 'pure tool turn' } });
    fireEvent.click(screen.getByText('Send'));
    await waitFor(() => expect(executeCb).not.toBeNull());

    executeCb!(null, { type: 'sessionStarted', sessionId: 's1', session_id: 's1' });
    executeCb!(null, { type: 'thinkingDelta', text: 'first think' });
    await waitFor(() => expect(document.body.textContent).toContain('first think'));
    executeCb!(null, { type: 'toolCall', id: 'c1', name: 'grep', input: '{"pattern":"x"}' });
    executeCb!(null, { type: 'toolResult', id: 'c1', ok: true, output: 'hit' });
    executeCb!(null, { type: 'thinkingDelta', text: 'second think' });
    await waitFor(() => expect(document.body.textContent).toContain('second think'));
    executeCb!(null, { type: 'toolCall', id: 'c2', name: 'read', input: '{"path":"a.ts"}' });
    executeCb!(null, { type: 'toolResult', id: 'c2', ok: true, output: 'file' });
    executeCb!(null, { type: 'assistantText', text: 'final answer' });
    await waitFor(() => expect(document.body.textContent).toContain('final answer'));

    // Chronological order across the whole conversation: think → tool → think
    // → tool → final prose. The old placeholder hoist collapsed this into
    // "all thinking first, then all tools".
    const list = document.querySelector('.message-list');
    expect(list).not.toBeNull();
    const text = list!.textContent || '';
    if (process.env.DEBUG_TEST) console.log('LIST TEXT:', JSON.stringify(text.slice(0, 800)));
    const pos = (needle: string) => text.indexOf(needle);
    expect(pos('first think')).toBeGreaterThanOrEqual(0);
    expect(pos('first think')).toBeLessThan(pos('Grep'));
    expect(pos('Grep')).toBeLessThan(pos('second think'));
    expect(pos('second think')).toBeLessThan(pos('Read'));
    expect(pos('Read')).toBeLessThan(pos('final answer'));
  });
});
