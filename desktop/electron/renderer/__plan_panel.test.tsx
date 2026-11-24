/// <reference types="vitest/globals" />
// Plan-mode UI trio: the right-side Plan panel (markdown, kept after approval), the
// (review read-only → executing after approval), and the Todo resident widget (collapsed summary + expanded list).
import { describe, it, expect, vi, beforeEach } from 'vitest';
import React from 'react';
import { render, screen, fireEvent, waitFor, within } from '@testing-library/react';
import { I18nProvider } from './i18n';

declare global { interface Window { starriver: any; } }

let executeCb: ((err: null | Error, event: any) => void) | null = null;

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
    getCurrentAgent: vi.fn().mockResolvedValue(null),
    getWorktreeStatus: vi.fn().mockResolvedValue({ isRepo: false, branch: null, isWorktree: false, dirty: false }),
    readWorkspaceInstructions: vi.fn().mockResolvedValue(null),
    setAgentMode: vi.fn().mockResolvedValue(undefined),
    getAgentMode: vi.fn().mockResolvedValue('plan'),
    setApprovalMode: vi.fn().mockResolvedValue(undefined),
    setPermissionPreset: vi.fn().mockResolvedValue(undefined),
    getPermission: vi.fn().mockResolvedValue({ sandbox: 'read-only', approval: 'manual', preset: null }),
    confirmPlan: vi.fn().mockResolvedValue(undefined),
    planReviewDecision: vi.fn().mockResolvedValue(undefined),
    listAgentPresets: vi.fn().mockResolvedValue({ presets: [], default: 'coding' }),
    getGovernance: vi.fn().mockResolvedValue({ allowDangerFullAccess: false, allowBypass: false }),
    setGovernance: vi.fn().mockResolvedValue(undefined),
    listSubagentConfigs: vi.fn().mockResolvedValue([]),
    setSubagentConfig: vi.fn().mockResolvedValue(undefined),
    setAgentActiveModel: vi.fn().mockResolvedValue(undefined),
    setAgentReasoningEffort: vi.fn().mockResolvedValue(undefined),
    approve: vi.fn().mockResolvedValue(undefined),
    execute: vi.fn((...args: any[]) => { executeCb = args[2]; }),
  };
});

async function sendInPlanSession() {
  const { CodingView, WorkspaceProvider, WorkspaceSection } = await import('./app');
  render(<I18nProvider><WorkspaceProvider><WorkspaceSection onOpenCoding={() => {}} /><CodingView debugEnabled={false} /></WorkspaceProvider></I18nProvider>);
  // Pick Plan on the New Task screen so the session mode (and status label) is plan.
  fireEvent.click(document.querySelector('button[aria-label="execution-mode"]') as HTMLElement);
  fireEvent.click(screen.getByText('Plan (read-only planning)'));
  const ta = await screen.findByPlaceholderText('Describe what you want to build…');
  fireEvent.change(ta, { target: { value: 'hello' } });
  fireEvent.click(screen.getByText('Send'));
  await waitFor(() => expect(executeCb).not.toBeNull());
  executeCb!(null, { type: 'TaskStarted', sessionId: 's-plan', session_id: 's-plan' });
}

describe('Plan panel + status phase + Todo widget', () => {
  it('auto-opens the Plan panel on planReviewRequested with the plan markdown', async () => {
    await sendInPlanSession();
    executeCb!(null, {
      type: 'planReviewRequested', requestId: 'r1',
      plan: '# My Plan\n\n- step one\n- step two',
    });
    // Panel opens automatically, badge starts at In review, markdown renders.
    await waitFor(() => expect(screen.getByText('In review')).toBeTruthy());
    const panel = document.querySelector('.plan-panel') as HTMLElement;
    expect(within(panel).getByText('My Plan')).toBeTruthy();
    expect(within(panel).getByText('step one')).toBeTruthy();
  });

  it('approving keeps the panel, flips the badge, and the status bar shows executing', async () => {
    await sendInPlanSession();
    executeCb!(null, { type: 'planReviewRequested', requestId: 'r1', plan: '# My Plan' });
    await waitFor(() => expect(screen.getByText('In review')).toBeTruthy());
    // Planning phase: the status bar still reads read-only planning.
    expect(document.querySelector('.coding-status-mode')?.textContent).toContain('read-only planning');

    executeCb!(null, { type: 'planReviewDecided', requestId: 'r1', approved: true });
    await waitFor(() => expect(screen.getByText('Approved')).toBeTruthy());
    // The panel stays as the execution reference, and the status bar flips.
    expect(screen.getByText('My Plan')).toBeTruthy();
    await waitFor(() =>
      expect(document.querySelector('.coding-status-mode')?.textContent).toContain('Plan (executing)')
    );
  });

  it('mounts the panel in the row wrapper with the status bar full-width below', async () => {
    await sendInPlanSession();
    executeCb!(null, { type: 'planReviewRequested', requestId: 'r1', plan: '# My Plan' });
    await waitFor(() => expect(screen.getByText('In review')).toBeTruthy());

    const view = document.querySelector('.coding-view') as HTMLElement;
    const panel = document.querySelector('.plan-panel') as HTMLElement;
    // The panel is a direct child of the row wrapper (sibling of the coding
    // column), and the status bar is a direct child of the view (full width).
    expect(panel.parentElement).toBeTruthy();
    expect(panel.parentElement!.className).not.toContain('coding-view');
    expect(Array.from(view.children).map(c => c.className)).toContain('coding-status-bar');
  });

  it('renders the Todo widget with progress summary and an expandable list', async () => {
    await sendInPlanSession();
    executeCb!(null, { type: 'toolCall', id: 't1', name: 'todo_write' });
    executeCb!(null, {
      type: 'toolResult', id: 't1', ok: true,
      output: '## TODO\n- [x] setup\n- [-] style (in progress)\n- [ ] rules',
    });
    // Collapsed: summary line with progress count.
    await waitFor(() => expect(screen.getByText('1/3')).toBeTruthy());
    // Expand: the full checklist renders.
    fireEvent.click(screen.getByLabelText('todo-widget'));
    await waitFor(() => expect(screen.getByText('setup')).toBeTruthy());
    expect(screen.getByText('style (in progress)')).toBeTruthy();
    expect(screen.getByText('rules')).toBeTruthy();
  });

  it('hides a completed todo list when a new turn starts, shows a fresh one', async () => {
    await sendInPlanSession();
    // Turn 1 ends with a fully-completed todo list.
    executeCb!(null, { type: 'toolCall', id: 't1', name: 'todo_write' });
    executeCb!(null, {
      type: 'toolResult', id: 't1', ok: true,
      output: '## TODO\n- [x] one\n- [x] two',
    });
    await waitFor(() => expect(screen.getByText('2/2')).toBeTruthy());
    executeCb!(null, { type: 'turnEnd', sessionId: 's-plan', reason: 'completed', summary: '' });

    // Turn 2 starts: the stale completed list is suppressed…
    executeCb!(null, { type: 'TaskStarted', sessionId: 's-plan', session_id: 's-plan' });
    await waitFor(() => expect(screen.queryByText('2/2')).toBeNull());

    // …and a fresh todo_write in this turn renders again.
    executeCb!(null, { type: 'toolCall', id: 't2', name: 'todo_write' });
    executeCb!(null, {
      type: 'toolResult', id: 't2', ok: true,
      output: '## TODO\n- [ ] fresh step',
    });
    await waitFor(() => expect(screen.getByText('0/1')).toBeTruthy());
  });

  it('keeps an in-progress todo list visible across turns', async () => {
    await sendInPlanSession();
    executeCb!(null, { type: 'toolCall', id: 't1', name: 'todo_write' });
    executeCb!(null, {
      type: 'toolResult', id: 't1', ok: true,
      output: '## TODO\n- [x] one\n- [ ] two',
    });
    await waitFor(() => expect(screen.getByText('1/2')).toBeTruthy());

    executeCb!(null, { type: 'TaskStarted', sessionId: 's-plan', session_id: 's-plan' });
    // Not everything is done — the list still matters, keep it up.
    expect(screen.getByText('1/2')).toBeTruthy();
  });

  it('renders the three-step card: review → mode (Manual preselected) → start', async () => {
    await sendInPlanSession();
    executeCb!(null, { type: 'planReviewRequested', requestId: 'r1', plan: '# My Plan' });
    const card = await screen.findByTestId('plan-review-card');
    expect(screen.getByText('Permission required')).toBeTruthy();
    expect(screen.getByText('Implement the plan')).toBeTruthy();
    // The card is a Composer overlay (covers the input bar) showing the plan
    // summary; the full markdown lives in the right-hand Plan panel.
    const wrap = card.closest('.composer-wrap') as HTMLElement;
    expect(wrap).toBeTruthy();
    expect(wrap.querySelector('.input-bar')).toBeTruthy();
    expect(within(card).getByText('My Plan')).toBeTruthy();

    // Step 2 opens on approve-plan with Manual preselected (design decision:
    // always Manual, never the session's previous preset).
    fireEvent.click(screen.getByText('Approve plan →'));
    expect(card.getAttribute('data-step')).toBe('mode');
    const manual = screen.getByRole('radio', { name: /Manual/ });
    expect(manual.getAttribute('aria-checked')).toBe('true');

    // Start-implementation rides the preset on the approval decision itself
    // (plan.reviewDecision `preset` param — atomic host-side application).
    fireEvent.click(screen.getByText('Start implementing'));
    await waitFor(() => {
      expect(window.starriver.setPermissionPreset).not.toHaveBeenCalled();
      expect(window.starriver.planReviewDecision).toHaveBeenCalledWith('s-plan', 'r1', true, undefined, 'manual');
    });
  });

  it('typed answer keeps planning with feedback; empty Keep Planning rejects bare', async () => {
    await sendInPlanSession();
    executeCb!(null, { type: 'planReviewRequested', requestId: 'r1', plan: '# My Plan' });
    await screen.findByTestId('plan-review-card');

    const input = screen.getByPlaceholderText('Type your answer…') as HTMLInputElement;
    fireEvent.change(input, { target: { value: 'add a rollback step' } });
    fireEvent.click(screen.getByText('Keep planning'));
    await waitFor(() =>
      expect(window.starriver.planReviewDecision).toHaveBeenCalledWith('s-plan', 'r1', false, 'add a rollback step')
    );

    // A later review: continue-planning without text keeps planning bare.
    executeCb!(null, { type: 'planReviewDecided', requestId: 'r1', approved: false });
    await waitFor(() => expect(screen.queryByTestId('plan-review-card')).toBeNull());
    executeCb!(null, { type: 'planReviewRequested', requestId: 'r2', plan: '# My Plan v2' });
    await screen.findByTestId('plan-review-card');
    fireEvent.click(screen.getByText('Keep planning'));
    await waitFor(() =>
      expect(window.starriver.planReviewDecision).toHaveBeenCalledWith('s-plan', 'r2', false, undefined)
    );
  });

  it('Full access routes through the risk confirmation before enabling', async () => {
    await sendInPlanSession();
    executeCb!(null, { type: 'planReviewRequested', requestId: 'r1', plan: '# My Plan' });
    await screen.findByTestId('plan-review-card');

    fireEvent.click(screen.getByText('Approve plan →'));
    fireEvent.click(screen.getByRole('radio', { name: /Full access/ }));
    fireEvent.click(screen.getByText('Start implementing'));
    // Risk step gates the danger preset behind the acknowledge checkbox.
    expect(document.querySelector('[data-step="risk"]')).toBeTruthy();
    const enable = screen.getByText('Enable & implement').closest('button') as HTMLButtonElement;
    expect(enable.disabled).toBe(true);

    fireEvent.click(screen.getByLabelText('I understand the risk'));
    expect(enable.disabled).toBe(false);
    fireEvent.click(enable);
    await waitFor(() => {
      expect(window.starriver.setPermissionPreset).not.toHaveBeenCalled();
      expect(window.starriver.planReviewDecision).toHaveBeenCalledWith('s-plan', 'r1', true, undefined, 'danger-full-access');
    });
  });
});

/* ─── Doc-viewer panel: multi-document tabs + zoom + hide/restore ─── */
describe('Doc-viewer panel: multi-doc tabs, zoom, hide', () => {
  // Diff bodies avoid JS/TS keywords ('new' etc.) — the panel's per-line
  // syntax highlighting wraps keywords in spans, which breaks exact-text
  // queries (getNodeText only reads direct text nodes).
  const DIFF_A = '--- a/one.ts\n+++ b/one.ts\n@@ -1,2 +1,2 @@\n-draft alpha\n+fresh alpha\n ctx line';
  const DIFF_B = '--- a/two.ts\n+++ b/two.ts\n@@ -1 +1 @@\n-draft beta\n+fresh beta';

  /** Send an edit tool result carrying a unified diff and click its Review
      button — the exact user path that opens a document tab. `nth` is the
      1-based number of Review buttons expected AFTER this row renders (each
      edit tool row carries exactly one), so the wait cannot be satisfied by
      an earlier row's button. */
  async function openDiffDoc(id: string, diff: string, nth: number) {
    executeCb!(null, { type: 'toolCall', id, name: 'edit' });
    executeCb!(null, { type: 'toolResult', id, ok: true, output: diff });
    const btns = await waitFor(() => {
      const found = screen.getAllByText('Review');
      expect(found.length).toBeGreaterThanOrEqual(nth);
      return found;
    });
    fireEvent.click(btns[btns.length - 1]);
  }

  it('opens multiple documents as separate tabs and switches between them', async () => {
    await sendInPlanSession();
    await openDiffDoc('d1', DIFF_A, 1);
    await openDiffDoc('d2', DIFF_B, 2);

    const panel = document.querySelector('.plan-panel') as HTMLElement;
    expect(panel).toBeTruthy();
    // Two doc tabs; the newest one is active (two.ts content shown).
    expect(panel.querySelectorAll('.plan-panel-doc-tab').length).toBe(2);
    expect(within(panel).getByText('fresh beta')).toBeTruthy();

    // Switch back to the first document — its content replaces the view.
    fireEvent.click(within(panel).getByText('one.ts'));
    expect(within(panel).getByText('fresh alpha')).toBeTruthy();
    expect(within(panel).queryByText('fresh beta')).toBeNull();
  });

  it('closing one document tab keeps the others and activates a neighbour', async () => {
    await sendInPlanSession();
    await openDiffDoc('d1', DIFF_A, 1);
    await openDiffDoc('d2', DIFF_B, 2);

    // Close the ACTIVE doc (two.ts) — one.ts becomes the active tab.
    fireEvent.click(screen.getByLabelText('close-doc-two.ts'));
    const panel = document.querySelector('.plan-panel') as HTMLElement;
    expect(panel.querySelectorAll('.plan-panel-doc-tab').length).toBe(1);
    expect(within(panel).getByText('fresh alpha')).toBeTruthy();
    expect(screen.queryByLabelText('close-doc-two.ts')).toBeNull();

    // Closing the last tab with no plan around collapses the panel.
    fireEvent.click(screen.getByLabelText('close-doc-one.ts'));
    expect(document.querySelector('.plan-panel')).toBeNull();
  });

  it('zooms the panel body via the footer control and resets on % click', async () => {
    await sendInPlanSession();
    await openDiffDoc('d1', DIFF_A, 1);

    const panel = document.querySelector('.plan-panel') as HTMLElement;
    expect(panel.style.getPropertyValue('--panel-zoom')).toBe('1');
    expect(screen.getByLabelText('panel-zoom-reset').textContent).toBe('100%');

    fireEvent.click(screen.getByLabelText('panel-zoom-in'));
    expect(panel.style.getPropertyValue('--panel-zoom')).toBe('1.1');
    expect(screen.getByLabelText('panel-zoom-reset').textContent).toBe('110%');

    fireEvent.click(screen.getByLabelText('panel-zoom-out'));
    fireEvent.click(screen.getByLabelText('panel-zoom-out'));
    expect(panel.style.getPropertyValue('--panel-zoom')).toBe('0.9');

    // Clamped: 30 more zoom-ins stop at 200%.
    for (let i = 0; i < 30; i += 1) fireEvent.click(screen.getByLabelText('panel-zoom-in'));
    expect(panel.style.getPropertyValue('--panel-zoom')).toBe('2');

    fireEvent.click(screen.getByLabelText('panel-zoom-reset'));
    expect(panel.style.getPropertyValue('--panel-zoom')).toBe('1');
  });

  it('hides the panel without dropping tabs; the toolbar button restores it', async () => {
    await sendInPlanSession();
    await openDiffDoc('d1', DIFF_A, 1);
    await openDiffDoc('d2', DIFF_B, 2);

    // Header ✕ hides the whole panel…
    fireEvent.click(screen.getByLabelText('close-plan-panel'));
    expect(document.querySelector('.plan-panel')).toBeNull();

    // …but the toolbar master switch stays (docs are open) and restores
    // the panel with BOTH tabs and the previously active document.
    const toggle = screen.getByLabelText('plan-panel');
    expect(toggle.getAttribute('aria-pressed')).toBe('false');
    fireEvent.click(toggle);
    const panel = document.querySelector('.plan-panel') as HTMLElement;
    expect(panel).toBeTruthy();
    expect(panel.querySelectorAll('.plan-panel-doc-tab').length).toBe(2);
    expect(within(panel).getByText('fresh beta')).toBeTruthy();

    // Toggle again hides; toggle again shows — a pure visibility switch.
    fireEvent.click(toggle);
    expect(document.querySelector('.plan-panel')).toBeNull();
    fireEvent.click(toggle);
    expect(document.querySelector('.plan-panel')).toBeTruthy();
  });

  it('the toggle is persistent: opens an empty panel with a hint before any content exists', async () => {
    await sendInPlanSession();
    // No plan, no documents yet — the button is ALWAYS there (persistent).
    const toggle = screen.getByLabelText('plan-panel');
    expect(toggle.getAttribute('aria-pressed')).toBe('false');
    expect(document.querySelector('.plan-panel')).toBeNull();

    // Clicking opens the panel in its empty state with the hint.
    fireEvent.click(toggle);
    const panel = document.querySelector('.plan-panel') as HTMLElement;
    expect(panel).toBeTruthy();
    expect(within(panel).getByText(/No documents open yet/)).toBeTruthy();

    // And it hides again — a pure visibility switch.
    fireEvent.click(screen.getByLabelText('close-plan-panel'));
    expect(document.querySelector('.plan-panel')).toBeNull();
  });

  it('plan tab and document tabs coexist; planReviewRequested still auto-opens the plan', async () => {
    await sendInPlanSession();
    await openDiffDoc('d1', DIFF_A, 1);
    // The plan arrives mid-viewing: the panel switches to the plan tab.
    executeCb!(null, { type: 'planReviewRequested', requestId: 'r1', plan: '# My Plan\n\n- step one' });
    const panel = await waitFor(() => {
      const p = document.querySelector('.plan-panel') as HTMLElement;
      expect(within(p).getByText('My Plan')).toBeTruthy();
      return p;
    });
    // Both kinds of tabs are present; the doc tab is still one click away.
    expect(within(panel).getByText('Plan')).toBeTruthy();
    expect(within(panel).getByText('one.ts')).toBeTruthy();
    fireEvent.click(within(panel).getByText('one.ts'));
    expect(within(panel).getByText('fresh alpha')).toBeTruthy();
    expect(within(panel).queryByText('step one')).toBeNull();
  });
});
