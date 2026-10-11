// WI-150 regression guard: background terminals move into the composer —
// the entry button (terminal glyph) shows ONLY while terminals are running,
// sits right of the approval picker, and opens a popover listing each
// terminal's start clock time and live elapsed, with a kill button.
import { describe, it, expect, vi, beforeEach } from 'vitest';
import React from 'react';
import { render, screen, fireEvent, waitFor } from '@testing-library/react';
import { I18nProvider } from './i18n';

declare global { interface Window { starriver: any; } }

beforeEach(() => {
  Element.prototype.scrollIntoView = vi.fn();
  localStorage.setItem('starriver-language', 'en');
  window.starriver = {
    listWorkspaces: vi.fn().mockResolvedValue([]),
    listSessions: vi.fn().mockResolvedValue([]),
    listArchivedSessions: vi.fn().mockResolvedValue([]),
    listSkills: vi.fn().mockResolvedValue([]),
    listMcpServers: vi.fn().mockResolvedValue([]),
    listModels: vi.fn().mockResolvedValue([]),
    listAgentModelBindings: vi.fn().mockResolvedValue([]),
    listTerminals: vi.fn().mockResolvedValue({
      terminals: [{ id: 't-abc123', status: 'running', startedAtMs: Date.now() - 65_000 }],
    }),
    killTerminal: vi.fn().mockResolvedValue({ killed: true }),
    getCurrentAgent: vi.fn().mockResolvedValue(null),
    getWorktreeStatus: vi.fn().mockResolvedValue({ isRepo: false, branch: null, isWorktree: false, dirty: false }),
    readWorkspaceInstructions: vi.fn().mockResolvedValue(null),
    setAgentMode: vi.fn().mockResolvedValue('standard'),
    getAgentMode: vi.fn().mockResolvedValue('standard'),
    setAgentActiveModel: vi.fn().mockResolvedValue(undefined),
    setAgentReasoningEffort: vi.fn().mockResolvedValue(undefined),
    execute: vi.fn(),
  };
});

async function mount({ expectButton = true }: { expectButton?: boolean } = {}) {
  const { CodingView, WorkspaceProvider, WorkspaceSection } = await import('./app');
  render(<I18nProvider><WorkspaceProvider><WorkspaceSection onOpenCoding={() => {}} /><CodingView debugEnabled={false} /></WorkspaceProvider></I18nProvider>);
  // The composer poll fires immediately on mount; the entry follows the count.
  await waitFor(() => {
    const btn = screen.queryByLabelText('background-terminals');
    if (expectButton) expect(btn).toBeTruthy();
    else expect(btn).toBeNull();
  });
}

describe('background terminals in the composer (WI-150)', () => {
  it('shows the entry only while terminals run, and the popover lists start + elapsed', async () => {
    await mount();

    const btn = screen.getByLabelText('background-terminals');
    // Terminal glyph (square 18×18 frame), right of the approval picker.
    expect(btn.innerHTML).toContain('width="18" height="18"');
    // No popover until clicked.
    expect(screen.queryByTestId('terminals-panel')).toBeNull();

    fireEvent.click(btn);
    const panel = screen.getByTestId('terminals-panel');
    expect(panel.textContent).toContain('running');
    // Elapsed ~1m 5s (started 65s ago); start clock renders as HH:MM:SS.
    expect(panel.textContent).toMatch(/1m/);
    expect(panel.querySelector('.terminal-started')?.textContent ?? '').toMatch(/^\d{2}:\d{2}:\d{2}$/);

    // Kill removes the row; with zero terminals left the entry hides too.
    fireEvent.click(panel.querySelector('.terminal-kill') as HTMLElement);
    await waitFor(() => expect(screen.queryByLabelText('background-terminals')).toBeNull());
  });

  it('no terminals → no entry button at all', async () => {
    window.starriver.listTerminals = vi.fn().mockResolvedValue({ terminals: [] });
    await mount({ expectButton: false });
    expect(screen.queryByLabelText('background-terminals')).toBeNull();
  });

  it('turn end surfaces a one-time hint for idle terminals, quiet afterwards', async () => {
    let executeCb: ((err: null, event: any) => void) | null = null;
    window.starriver.listTerminals = vi.fn().mockResolvedValue({
      terminals: [{ id: 't-idle', status: 'idle', startedAtMs: Date.now() - 400_000 }],
    });
    window.starriver.execute = vi.fn((...args: any[]) => { executeCb = args[2]; });
    await mount();

    // Arm a live stream: a real send captures the event callback.
    const ta = screen.getByPlaceholderText('Describe what you want to build…');
    fireEvent.change(ta, { target: { value: 'hi' } });
    fireEvent.click(screen.getByText('Send'));
    await waitFor(() => expect(executeCb).not.toBeNull());

    const hintCount = () => screen.queryAllByText(/idle for 5\+ minutes/).length;
    // First settle with an idle terminal → the hint toast appears once.
    executeCb!(null, { type: 'turnEnd', reason: 'completed' });
    await waitFor(() => expect(hintCount()).toBeGreaterThan(0));
    const afterFirst = hintCount();

    // A second settle for the same still-idle terminal stays quiet.
    executeCb!(null, { type: 'turnEnd', reason: 'completed' });
    await new Promise((r) => setTimeout(r, 30));
    expect(hintCount()).toBe(afterFirst);
  });
});
