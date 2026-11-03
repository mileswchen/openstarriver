/// <reference types="vitest/globals" />
import { describe, it, expect, vi, beforeEach } from 'vitest';
import React from 'react';
import { render, screen, fireEvent, waitFor } from '@testing-library/react';
import { I18nProvider } from './i18n';

declare global { interface Window { starriver: any; } }

let executeCb: ((err: string | null, event: any) => void) | null = null;
let executeArgs: any[] = [];

beforeEach(() => {
  executeCb = null;
  executeArgs = [];
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
    getAgentMode: vi.fn().mockResolvedValue('standard'),
    setApprovalMode: vi.fn().mockResolvedValue(undefined),
    setPermissionPreset: vi.fn().mockResolvedValue(undefined),
    getPermission: vi.fn().mockResolvedValue({ sandbox: 'workspace-write', approval: 'manual', preset: 'manual' }),
    confirmPlan: vi.fn().mockResolvedValue(undefined),
    planReviewDecision: vi.fn().mockResolvedValue(undefined),
    listAgentPresets: vi.fn().mockResolvedValue({
      presets: [
        { id: 'coding', agent: 'Starriver Code', name: 'Starriver Code', description: 'builtin', source: 'builtin', broken: null, isDefault: true },
        { id: 'reviewer', agent: 'Reviewer', name: 'Reviewer', description: 'review agent', source: 'user', broken: null, isDefault: false },
      ],
      default: 'coding',
    }),
    listSubagentConfigs: vi.fn().mockResolvedValue([]),
    setSubagentConfig: vi.fn().mockResolvedValue(undefined),
    setAgentActiveModel: vi.fn().mockResolvedValue(undefined),
    setAgentReasoningEffort: vi.fn().mockResolvedValue(undefined),
    approve: vi.fn().mockResolvedValue(undefined),
    execute: vi.fn((...args: any[]) => {
      executeArgs = args;
      executeCb = args[2];
    }),
  };
});

/** Execution-mode picker above the input for new sessions (binding: chosen
 * at creation, locked per session). */
function modeButton(): HTMLButtonElement {
  const btn = document.querySelector('button[aria-label="execution-mode"]') as HTMLButtonElement;
  expect(btn, 'execution-mode picker must sit above the composer in the new-session state').toBeTruthy();
  return btn;
}

/** Pick a mode via the popup menu (click the button → click the menu item). */
function pickMode(label: string) {
  fireEvent.click(modeButton());
  fireEvent.click(screen.getByText(label));
}

/** Session-screen mode label: the bottom status-bar item that sits right of
 *  the workspace directory. The New Task screen has no such item — the picker
 *  above the input IS the control there. */
function statusModeLabel(): string {
  return (document.querySelector('.coding-status-mode') as HTMLElement)?.textContent || '';
}

async function sendTurn() {
  const ta = await screen.findByPlaceholderText('Describe what you want to build…');
  fireEvent.change(ta, { target: { value: 'do the work' } });
  fireEvent.click(screen.getByText('Send'));
  await waitFor(() => expect(executeCb).not.toBeNull());
}

describe('CodingView — 执行模式与 会话绑定(DSH 方案)', () => {
  it('新会话态在输入框上方渲染四档选择器,Standard 为默认', async () => {
    const { CodingView, WorkspaceProvider, WorkspaceSection } = await import('./app');
    render(<I18nProvider><WorkspaceProvider><WorkspaceSection onOpenCoding={() => {}} /><CodingView debugEnabled={false} /></WorkspaceProvider></I18nProvider>);
    await waitFor(() => expect(modeButton().textContent).toContain('Standard'));
    fireEvent.click(modeButton());
    const items = Array.from(document.querySelectorAll('.composer-approval-item'));
    expect(items.map(i => i.textContent?.replace('✓', ''))).toEqual([
      'Standard',
      'Plan (read-only planning)',
      'PTC (batch pipeline)',
      'Goal (autonomous loop)',
    ]);
  });

  it('部署默认作为初始暂存值回显', async () => {
    window.starriver.getAgentMode = vi.fn().mockResolvedValue('plan');
    const { CodingView, WorkspaceProvider, WorkspaceSection } = await import('./app');
    render(<I18nProvider><WorkspaceProvider><WorkspaceSection onOpenCoding={() => {}} /><CodingView debugEnabled={false} /></WorkspaceProvider></I18nProvider>);
    await waitFor(() => expect(modeButton().textContent).toContain('Plan (read-only planning)'));
    // New Task screen: the status bar carries no mode item (picker only).
    expect(document.querySelector('.coding-status-mode')).toBeNull();
  });

  it('选择 Plan 后 send:execute 携带该模式,选择器隐藏,顶部显示 Plan', async () => {
    const { CodingView, WorkspaceProvider, WorkspaceSection } = await import('./app');
    render(<I18nProvider><WorkspaceProvider><WorkspaceSection onOpenCoding={() => {}} /><CodingView debugEnabled={false} /></WorkspaceProvider></I18nProvider>);
    await waitFor(() => expect(modeButton().textContent).toContain('Standard'));
    pickMode('Plan (read-only planning)');
    expect(window.starriver.setAgentMode).not.toHaveBeenCalled();

    await sendTurn();
    expect(executeArgs[3]).toBe('plan');
    expect(document.querySelector('button[aria-label="execution-mode"]')).toBeNull();
    await waitFor(() => expect(statusModeLabel()).toBe('Plan (read-only planning)'));
  });

  it('不选择时 send 携带 standard;继续会话不携带模式参数', async () => {
    const { CodingView, WorkspaceProvider, WorkspaceSection } = await import('./app');
    render(<I18nProvider><WorkspaceProvider><WorkspaceSection onOpenCoding={() => {}} /><CodingView debugEnabled={false} /></WorkspaceProvider></I18nProvider>);
    await waitFor(modeButton);
    await sendTurn();
    expect(executeArgs[3]).toBe('standard');
  });

  it('会话开始后模式锁定:选择器不再出现,slash /plan 被拒', async () => {
    const { CodingView, WorkspaceProvider, WorkspaceSection } = await import('./app');
    render(<I18nProvider><WorkspaceProvider><WorkspaceSection onOpenCoding={() => {}} /><CodingView debugEnabled={false} /></WorkspaceProvider></I18nProvider>);
    await sendTurn();
    executeCb!(null, { type: 'TaskStarted', sessionId: 's1', session_id: 's1' });
    executeCb!(null, { type: 'assistantText', sessionId: 's1', text: 'done' });
    executeCb!(null, { type: 'turnEnd', sessionId: 's1', reason: 'completed', summary: '' });
    await waitFor(() => expect(document.querySelector('select[aria-label="execution-mode"]')).toBeNull());

    const ta = screen.getByPlaceholderText('Describe what you want to build…');
    fireEvent.change(ta, { target: { value: '/plan' } });
    const item = await screen.findByText('Switch to Plan mode (read-only)');
    fireEvent.click(item);
    await waitFor(() =>
      expect(screen.getByText('Execution mode is locked to this session (chosen at creation)')).toBeTruthy()
    );
    expect(window.starriver.setAgentMode).not.toHaveBeenCalled();
  });

  it('加载既有会话:按会话锁定的模式回显(getAgentMode(sessionId))', async () => {
    window.starriver.listSessions = vi.fn().mockResolvedValue([
      { id: 's-plan', title: 'planned task', workspaceId: 'w1', createdAt: '', messageCount: 1 },
    ]);
    window.starriver.listWorkspaces = vi.fn().mockResolvedValue([
      { id: 'w1', path: '/tmp/demo', title: 'demo', createdAt: '', sessionIds: ['s-plan'] },
    ]);
    window.starriver.getSessionMessages = vi.fn().mockResolvedValue([
      { role: 'assistant', content: 'previous turn' },
    ]);
    window.starriver.getAgentMode = vi.fn().mockImplementation(async (sid?: string) =>
      sid === 's-plan' ? 'plan' : 'standard');
    const { CodingView, WorkspaceProvider, WorkspaceSection } = await import('./app');
    render(<I18nProvider><WorkspaceProvider><WorkspaceSection onOpenCoding={() => {}} /><CodingView debugEnabled={false} /></WorkspaceProvider></I18nProvider>);

    fireEvent.click(await screen.findByText('planned task'));
    await waitFor(() =>
      expect(window.starriver.getAgentMode).toHaveBeenCalledWith('s-plan')
    );
    // Mode label echoes the locked binding; the phase suffix derives from the
    // folded sandbox (getPermission → workspace-write ⇒ executing).
    await waitFor(() => expect(statusModeLabel()).toBe('Plan (executing)'));
    expect(document.querySelector('select[aria-label="execution-mode"]')).toBeNull();
  });
});

describe('CodingView — New Task / 会话 双界面拆分', () => {
  const WS = [
    { id: 'w1', path: '/tmp/demo', title: 'demo', createdAt: '', sessionIds: [] },
    { id: 'w2', path: '/tmp/other', title: 'other', createdAt: '', sessionIds: [] },
  ];
  const renderView = async () => {
    const { CodingView, WorkspaceProvider, WorkspaceSection } = await import('./app');
    render(<I18nProvider><WorkspaceProvider><WorkspaceSection onOpenCoding={() => {}} /><CodingView debugEnabled={false} /></WorkspaceProvider></I18nProvider>);
  };

  it('New Task 态:Workspace 选择器在执行模式左边,切换即激活,无轨迹开关', async () => {
    window.starriver.listWorkspaces = vi.fn().mockResolvedValue(WS);
    window.starriver.getActiveWorkspace = vi.fn().mockResolvedValue('w1');
    await renderView();
    await waitFor(() => expect(modeButton().textContent).toContain('Standard'));

    const wsBtn = document.querySelector('button[aria-label="workspace"]') as HTMLButtonElement;
    expect(wsBtn.textContent).toContain('demo');
    // The workspace picker sits LEFT of the execution-mode picker.
    expect(wsBtn.compareDocumentPosition(modeButton()) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    // New Task screen: no Conversation/Trajectory switch.
    expect(document.querySelector('.coding-view-switch')).toBeNull();

    fireEvent.click(wsBtn);
    // The menu item shares its text with the sidebar workspace row — pick
    // the instance inside the dropdown menu.
    const item = screen.getAllByText('other').find(e => e.closest('.composer-approval-item'))!;
    expect(item, 'workspace menu item').toBeTruthy();
    fireEvent.click(item);
    await waitFor(() => expect(window.starriver.setActiveWorkspace).toHaveBeenCalledWith('w2', 'Starriver Code'));
    await waitFor(() =>
      expect((document.querySelector('button[aria-label="workspace"]') as HTMLElement).textContent).toContain('other')
    );
  });

  it('会话态:轨迹开关出现,模式显示在底部状态栏 workspace 目录右侧', async () => {
    window.starriver.listWorkspaces = vi.fn().mockResolvedValue(WS.slice(0, 1));
    window.starriver.getActiveWorkspace = vi.fn().mockResolvedValue('w1');
    await renderView();
    await waitFor(modeButton);
    expect(document.querySelector('.coding-status-mode')).toBeNull();

    await sendTurn();
    executeCb!(null, { type: 'TaskStarted', sessionId: 's1', session_id: 's1' });
    executeCb!(null, { type: 'assistantText', sessionId: 's1', text: 'done' });
    executeCb!(null, { type: 'turnEnd', sessionId: 's1', reason: 'completed', summary: '' });

    // Session screen: the switch is back and the mode lives in the status bar,
    // right of the workspace directory.
    await waitFor(() => expect(document.querySelector('.coding-view-switch')).toBeTruthy());
    await waitFor(() => expect(statusModeLabel()).toBe('Standard'));
    const cwd = document.querySelector('.coding-status-cwd')!;
    const modeEl = document.querySelector('.coding-status-mode')!;
    expect(cwd.compareDocumentPosition(modeEl) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(document.querySelector('.coding-status-bar')!.contains(modeEl)).toBeTruthy();
  });
});

describe('CodingView — Plan review card (plan 收拢, review-driven)', () => {
  async function runTurnAndRequestReview() {
    const { CodingView, WorkspaceProvider, WorkspaceSection } = await import('./app');
    render(<I18nProvider><WorkspaceProvider><WorkspaceSection onOpenCoding={() => {}} /><CodingView debugEnabled={false} /></WorkspaceProvider></I18nProvider>);
    const ta = await screen.findByPlaceholderText('Describe what you want to build…');
    fireEvent.change(ta, { target: { value: 'plan the work' } });
    fireEvent.click(screen.getByText('Send'));
    await waitFor(() => expect(executeCb).not.toBeNull());
    executeCb!(null, { type: 'TaskStarted', sessionId: 's1', session_id: 's1' });
    // The model presents its plan via exit_plan_mode → planReviewRequested.
    executeCb!(null, {
      type: 'planReviewRequested',
      sessionId: 's1',
      requestId: 'pr-1',
      plan: '# Plan\n\n1. first step',
    });
  }

  it('opens on planReviewRequested (not on turn end); the plan body goes to the panel', async () => {
    await runTurnAndRequestReview();
    expect(screen.getByTestId('plan-review-card')).toBeTruthy();
    expect(screen.getByText('Permission required')).toBeTruthy();
    expect(screen.getByText('Implement the plan')).toBeTruthy();

    // Turn end alone must NOT close the card (the decision is still pending).
    executeCb!(null, { type: 'turnEnd', sessionId: 's1', reason: 'completed', summary: '' });
    expect(screen.getByTestId('plan-review-card')).toBeTruthy();
  });

  it('approve rides the preset on plan.reviewDecision; decided closes the card', async () => {
    await runTurnAndRequestReview();
    // Two-step approval: approve-plan opens the mode step, start-implementation
    // (Manual preselected) sends the preset as part of the decision — no
    // separate setPermissionPreset write (the host applies it atomically).
    fireEvent.click(screen.getByText('Approve plan →'));
    fireEvent.click(screen.getByText('Start implementing'));
    await waitFor(() => {
      expect(window.starriver.setPermissionPreset).not.toHaveBeenCalled();
      expect(window.starriver.planReviewDecision).toHaveBeenCalledWith('s1', 'pr-1', true, undefined, 'manual');
    });
    // The host's audit trail lands as planReviewDecided on every surface.
    executeCb!(null, {
      type: 'planReviewDecided',
      sessionId: 's1',
      requestId: 'pr-1',
      approved: true,
    });
    await waitFor(() =>
      expect(screen.queryByTestId('plan-review-card')).toBeNull()
    );
  });

  it('keep-planning sends the typed answer back to the model', async () => {
    await runTurnAndRequestReview();
    const input = await screen.findByPlaceholderText('Type your answer…');
    fireEvent.change(input, { target: { value: 'add a rollback step' } });
    fireEvent.click(screen.getByText('Keep planning'));
    await waitFor(() =>
      expect(window.starriver.planReviewDecision).toHaveBeenCalledWith('s1', 'pr-1', false, 'add a rollback step')
    );
  });

  it('a decided event alone closes the card (multi-UI sync / timeout)', async () => {
    await runTurnAndRequestReview();
    executeCb!(null, {
      type: 'planReviewDecided',
      sessionId: 's1',
      requestId: 'pr-1',
      approved: false,
      feedback: 'answered elsewhere',
    });
    await waitFor(() =>
      expect(screen.queryByTestId('plan-review-card')).toBeNull()
    );
  });
});

describe('CodingView — /explore one-shot plan', () => {
  it('switches to Plan for the turn, then restores and unlocks', async () => {
    const { CodingView, WorkspaceProvider, WorkspaceSection } = await import('./app');
    render(<I18nProvider><WorkspaceProvider><WorkspaceSection onOpenCoding={() => {}} /><CodingView debugEnabled={false} /></WorkspaceProvider></I18nProvider>);
    const ta = await screen.findByPlaceholderText('Describe what you want to build…');
    fireEvent.change(ta, { target: { value: '/explore the rendering pipeline' } });
    fireEvent.click(screen.getByText('Send'));
    await waitFor(() => expect(executeCb).not.toBeNull());

    // During exploration the kernel switches to plan (direct call)
    await waitFor(() =>
      expect(window.starriver.setAgentMode).toHaveBeenCalledWith('plan')
    );
    executeCb!(null, { type: 'TaskStarted', sessionId: 's1', session_id: 's1' });
    executeCb!(null, { type: 'assistantText', sessionId: 's1', text: 'findings…' });
    executeCb!(null, { type: 'turnEnd', sessionId: 's1', reason: 'completed', summary: '' });
    await screen.findByText('findings…');

    // Turn end: restore the pre-explore mode and unlock the session's read-only stamp
    await waitFor(() =>
      expect(window.starriver.setAgentMode).toHaveBeenCalledWith(null)
    );
    await waitFor(() =>
      expect(window.starriver.confirmPlan).toHaveBeenCalledWith('s1')
    );
    // The one-shot exploration never shows the Plan review card
    expect(screen.queryByText('Plan awaiting your review — approve to execute')).toBeNull();
  });
});

describe('CodingView — permission-preset wiring', () => {
  it('forwards the composer permission-preset choice to setPermissionPreset with the session id', async () => {
    const { CodingView, WorkspaceProvider, WorkspaceSection } = await import('./app');
    render(<I18nProvider><WorkspaceProvider><WorkspaceSection onOpenCoding={() => {}} /><CodingView debugEnabled={false} /></WorkspaceProvider></I18nProvider>);
    const ta = await screen.findByPlaceholderText('Describe what you want to build…');
    fireEvent.change(ta, { target: { value: 'hello' } });
    fireEvent.click(screen.getByText('Send'));
    await waitFor(() => expect(executeCb).not.toBeNull());
    executeCb!(null, { type: 'TaskStarted', sessionId: 's1', session_id: 's1' });

    fireEvent.click(screen.getByLabelText('permission-preset'));
    fireEvent.click(screen.getByText('Auto'));
    await waitFor(() =>
      expect(window.starriver.setPermissionPreset).toHaveBeenCalledWith('s1', 'auto')
    );
  });

  it('BUG-113: a preset picked BEFORE the first send rides the creation call — no deferred setPermissionPreset', async () => {
    const { CodingView, WorkspaceProvider, WorkspaceSection } = await import('./app');
    render(<I18nProvider><WorkspaceProvider><WorkspaceSection onOpenCoding={() => {}} /><CodingView debugEnabled={false} /></WorkspaceProvider></I18nProvider>);
    // Fresh-session state: pick a non-default preset BEFORE anything is sent.
    fireEvent.click(screen.getByLabelText('permission-preset'));
    fireEvent.click(screen.getByText('Auto'));

    await sendTurn();
    // The preset rides execute as the creation-time binding (arg #8) — the
    // host stamps both knobs into the fresh session BEFORE the first turn.
    expect(executeArgs[7]).toBe('auto');
    expect(window.starriver.setPermissionPreset).not.toHaveBeenCalled();

    // TaskStarted no longer triggers a best-effort deferred write.
    executeCb!(null, { type: 'TaskStarted', sessionId: 's-new', session_id: 's-new' });
    expect(window.starriver.setPermissionPreset).not.toHaveBeenCalled();
  });
});

describe('SettingsDialog — Coding tab', () => {
  it('renders the subagent delegation section without the retired governance block', async () => {
    window.starriver.listSubagentConfigs = vi.fn().mockResolvedValue([
      { name: 'explorer', description: 'Codebase exploration, read-only', tools: ['read', 'grep'], readOnly: true, enabled: false, model: undefined },
    ]);
    const { SettingsDialog } = await import('./app');
    render(
      <I18nProvider>
        <SettingsDialog onClose={() => {}} />
      </I18nProvider>
    );

    fireEvent.click(screen.getByText('Coding'));

    // Governance switches retired: no checkbox, no host governance reads.
    await waitFor(() => expect(screen.getByText('Subagent Delegation')).toBeTruthy());
    expect(screen.queryByText('Governance')).toBeNull();
    expect(screen.queryByRole('checkbox', { name: 'Allow DangerFullAccess sandbox' })).toBeNull();
    expect(screen.queryByRole('checkbox', { name: 'Allow ByPass approval mode' })).toBeNull();
    // The governance bridge methods are gone from the surface entirely.
    expect(window.starriver.getGovernance).toBeUndefined();
    expect(window.starriver.setGovernance).toBeUndefined();
  });
});


describe('CodingView — New Task mode switch (WI-5, DSH ui-agent-preset parity)', () => {
  async function renderNewTask() {
    const { CodingView, WorkspaceProvider, WorkspaceSection } = await import('./app');
    render(<I18nProvider><WorkspaceProvider><WorkspaceSection onOpenCoding={() => {}} /><CodingView debugEnabled={false} /></WorkspaceProvider></I18nProvider>);
    await waitFor(() =>
      expect(window.starriver.listAgentPresets).toHaveBeenCalled()
    );
  }

  function modeGroup(): HTMLElement {
    return document.querySelector('[aria-label="agent-preset"]') as HTMLElement;
  }

  function pressedPill(): string {
    return modeGroup()?.querySelector('button[aria-pressed="true"]')?.textContent ?? '';
  }

  function clickMode(label: 'Coding' | 'Chat') {
    const btn = [...modeGroup().querySelectorAll('button')].find(b => b.textContent === label);
    expect(btn, `mode button ${label}`).toBeTruthy();
    fireEvent.click(btn!);
  }

  it('renders the fixed Coding / Chat segment with Coding selected by default', async () => {
    await renderNewTask();
    const group = modeGroup();
    expect(group, 'mode switch sits above the composer').toBeTruthy();
    expect(group.textContent).toContain('Coding');
    expect(group.textContent).toContain('Chat');
    await waitFor(() => expect(pressedPill()).toBe('Coding'));
    // the switch replaced the sparkle box as the empty-state hero
    expect(screen.getByText('Type a coding task to start')).toBeTruthy();
  });

  it('rides the default coding preset on the first execute', async () => {
    await renderNewTask();
    const ta = await screen.findByPlaceholderText('Describe what you want to build…');
    fireEvent.change(ta, { target: { value: 'do the work' } });
    fireEvent.click(screen.getByText('Send'));
    await waitFor(() => expect(executeCb).not.toBeNull());
    // execute(task, sessionId, cb, executionMode, agent, isolation, agentPreset)
    const args = window.starriver.execute.mock.calls[0];
    expect(args[4]).toBe('Starriver Code');
    expect(args[6]).toBe('coding');
  });

  it('keeps rendering the fixed switch even when the roster has a single row', async () => {
    window.starriver.listAgentPresets = vi.fn().mockResolvedValue({
      presets: [{ id: 'coding', agent: 'Starriver Code', name: 'Starriver Code', description: 'builtin', source: 'builtin', broken: null, isDefault: true }],
      default: 'coding',
    });
    await renderNewTask();
    expect(modeGroup()).toBeTruthy();
  });

  it('chat mode hides coding-only pickers and rides the Chat Agent bare', async () => {
    window.starriver.listAgentModelBindings = vi.fn().mockResolvedValue([
      { agentName: 'Starriver Code', activeModel: '', boundModels: [], reasoningEffort: '' },
      { agentName: 'Chat Agent', activeModel: 'p/m', boundModels: [{ id: 'p/m', name: 'Model A' }, { id: 'q/n', name: 'Model B' }], reasoningEffort: '' },
    ]);
    await renderNewTask();
    // coding default: workspace / execution-mode / approval / model controls visible
    expect(document.querySelector('[aria-label="workspace"]')).toBeTruthy();
    expect(document.querySelector('button[aria-label="execution-mode"]')).toBeTruthy();
    expect(document.querySelector('[aria-label="permission-preset"]')).toBeTruthy();
    expect(document.querySelector('[aria-label="coding-model"]')).toBeTruthy();
    // switch to Chat → coding-only chrome disappears, hero + placeholder swap
    clickMode('Chat');
    await waitFor(() => expect(pressedPill()).toBe('Chat'));
    expect(screen.getByText('Type a chat task to start')).toBeTruthy();
    // the top-stacked New Task layout (fixed switch position) survives the switch
    expect(document.querySelector('.message-list.new-task')).toBeTruthy();
    expect(document.querySelector('[aria-label="workspace"]')).toBeNull();
    expect(document.querySelector('button[aria-label="execution-mode"]')).toBeNull();
    expect(document.querySelector('[aria-label="permission-preset"]')).toBeNull();
    expect(document.querySelector('[aria-label="coding-model"]')).toBeNull();
    // chat carries its own model/reasoning selects, wired to the Chat Agent
    expect(document.querySelector('[aria-label="chat-model"]')).toBeTruthy();
    expect(document.querySelector('[aria-label="chat-reasoning"]')).toBeTruthy();
    fireEvent.change(document.querySelector('[aria-label="chat-model"]') as HTMLSelectElement, { target: { value: 'q/n' } });
    await waitFor(() => expect(window.starriver.setAgentActiveModel).toHaveBeenCalledWith('Chat Agent', 'q', 'n'));
    const ta = document.querySelector('textarea') as HTMLTextAreaElement;
    expect(ta.placeholder).toBe('Type a message to start chatting');
    // rides the Chat Agent with no execution mode / isolation / preset
    fireEvent.change(ta, { target: { value: 'hello' } });
    fireEvent.click(screen.getByText('Send'));
    await waitFor(() => expect(executeCb).not.toBeNull());
    const args = window.starriver.execute.mock.calls[0];
    expect(args[3]).toBeUndefined();
    expect(args[4]).toBe('Chat Agent');
    expect(args[5]).toBeUndefined();
    expect(args[6]).toBeUndefined();
  });
});
