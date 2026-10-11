// WI-147 regression guard: the iOS-style Turn layout. A completed turn renders
// as user bubble (right) → work-fold row («Worked Xm Ys ›», steps hidden) →
// turn summary (always visible). Expanding the fold reveals the steps flat in
// trajectory order; the summary stays put. The nav-rail tooltip reads steps.
import { describe, it, expect, vi, beforeEach } from 'vitest';
import React from 'react';
import { render, screen, waitFor, fireEvent } from '@testing-library/react';
import { I18nProvider } from './i18n';

declare global { interface Window { starriver: any; } }

let executeCb: ((err: string | null, event: any) => void) | null = null;

const HISTORY = [
  { id: 'm1', role: 'user', content: '帮我查一下', ts: 1000 },
  { id: 'm2', role: 'thinking', content: '先看目录结构', ts: 1100 },
  { id: 'm3', role: 'tool', content: '🔧 bash: ls\nsrc\n', ts: 1200 },
  { id: 'm4', role: 'assistant', content: '查完了：目录结构如下。', ts: 1300, doneTs: 61000, durationMs: 60000 },
  { id: 'm5', role: 'user', content: '第二条指令', ts: 62000 },
  { id: 'm6', role: 'thinking', content: '第二条思考', ts: 62500 },
  { id: 'm7', role: 'tool', content: '🔧 bash: cat foo\nbar\n', ts: 63000 },
  { id: 'm8', role: 'assistant', content: '第二条完成。', ts: 63500, doneTs: 65500, durationMs: 2000 },
];

beforeEach(() => {
  executeCb = null;
  Element.prototype.scrollIntoView = vi.fn();
  localStorage.setItem('starriver-language', 'en');
  window.starriver = {
    listWorkspaces: vi.fn().mockResolvedValue([{ id: 'w1', path: '/tmp/w', title: 'w' }]),
    createWorkspace: vi.fn(),
    renameWorkspace: vi.fn().mockResolvedValue(undefined),
    deleteWorkspace: vi.fn().mockResolvedValue(undefined),
    setActiveWorkspace: vi.fn().mockResolvedValue(undefined),
    getActiveWorkspace: vi.fn().mockResolvedValue(null),
    listSessions: vi.fn().mockResolvedValue([
      { id: 's1', title: 'font task', workspaceId: 'w1', createdAt: Date.now(), messageCount: 4 },
    ]),
    getSessionMessages: vi.fn().mockResolvedValue(HISTORY),
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
    setPermissionPreset: vi.fn().mockResolvedValue(undefined),
    getPermission: vi.fn().mockResolvedValue({ sandbox: 'workspace-write', approval: 'manual', preset: 'manual' }),
    confirmPlan: vi.fn().mockResolvedValue(undefined),
    planReviewDecision: vi.fn().mockResolvedValue(undefined),
    setAgentActiveModel: vi.fn().mockResolvedValue(undefined),
    setAgentReasoningEffort: vi.fn().mockResolvedValue(undefined),
    approve: vi.fn().mockResolvedValue(undefined),
    execute: vi.fn((_task: string, _sid: string | undefined, cb: (err: string | null, event: any) => void) => {
      executeCb = cb;
    }),
  };
});

async function openSession() {
  const { CodingView, WorkspaceProvider, WorkspaceSection } = await import('./app');
  render(
    <I18nProvider>
      <WorkspaceProvider>
        <WorkspaceSection onOpenCoding={() => {}} />
        <CodingView debugEnabled={false} />
      </WorkspaceProvider>
    </I18nProvider>
  );
  // Enter the session from the sidebar (loads the history projection).
  fireEvent.click(await screen.findByText('font task'));
  await screen.findByText('第二条完成。');
}

describe('Turn work fold (WI-147)', () => {
  it('completed turn: steps hidden behind the work fold, summary always visible', async () => {
    await openSession();
    // Every turn now lays out flat: each carries its own work fold (steps
    // hidden), its summary always visible. Two turns → two fold rows.
    const folds = document.querySelectorAll('[data-testid="work-fold-row"]');
    expect(folds.length).toBe(2);
    for (const f of folds) expect(f.getAttribute('aria-expanded')).toBe('false');
    // Steps hidden behind their folds…
    expect(screen.queryByText('先看目录结构')).toBeNull();
    expect(screen.queryByText('第二条思考')).toBeNull();
    // …both summaries visible.
    expect(screen.getByText('查完了：目录结构如下。')).toBeTruthy();
    expect(screen.getByText('第二条完成。')).toBeTruthy();
  });

  it('expanding the fold reveals the steps in trajectory order; summary stays', async () => {
    await openSession();
    const folds = document.querySelectorAll('[data-testid="work-fold-row"]');
    fireEvent.click(folds[0]);
    const fold = document.querySelectorAll('[data-testid="work-fold-row"]')[0];
    expect(fold.getAttribute('aria-expanded')).toBe('true');
    // Turn 1's steps are visible now — thinking then tool (trajectory order).
    expect(screen.getAllByText('先看目录结构').length).toBeGreaterThan(0);
    expect(document.querySelector('.turn-step')).toBeTruthy();
    // Both summaries stay visible below their folds.
    expect(screen.getByText('查完了：目录结构如下。')).toBeTruthy();
    expect(screen.getByText('第二条完成。')).toBeTruthy();
    // Collapse again.
    fireEvent.click(fold);
    expect(screen.queryByText('先看目录结构')).toBeNull();
    expect(screen.getByText('查完了：目录结构如下。')).toBeTruthy();
  });

  it('a live (streaming) turn is force-expanded until the turn ends', async () => {
    await openSession();
    const ta = await screen.findByPlaceholderText('Describe what you want to build…');
    fireEvent.change(ta, { target: { value: 'do more work' } });
    fireEvent.click(screen.getByText('Send'));
    await waitFor(() => expect(executeCb).not.toBeNull());

    executeCb!(null, { type: 'TaskStarted', session_id: 's1', sessionId: 's1' });
    executeCb!(null, { type: 'thinkingDelta', text: 'live thinking' });
    // Streaming: the steps are visible WITHOUT touching the fold.
    await waitFor(() =>
      expect(document.querySelector('.thinking-row-summary')?.textContent).toContain('live thinking')
    );

    // Turn ends → the work fold collapses; the summary stays visible.
    executeCb!(null, { type: 'assistantText', text: '全部完成。' });
    executeCb!(null, { type: 'turnEnd', sessionId: 's1', reason: 'completed', summary: '' });
    await waitFor(() => {
      expect(screen.queryByText('live thinking')).toBeNull();
      expect(screen.getByText('全部完成。')).toBeTruthy();
    });
  });

  it('nav-rail tooltip reads steps, not rows', async () => {
    await openSession();
    const tick = document.querySelector('.turn-tick') as HTMLElement;
    expect(tick).toBeTruthy();
    fireEvent.mouseEnter(tick);
    await waitFor(() => {
      const tip = document.querySelector('.tick-tip .no') as HTMLElement;
      expect(tip?.textContent).toContain('steps');
      expect(tip?.textContent).not.toContain('rows');
    });
  });
});
