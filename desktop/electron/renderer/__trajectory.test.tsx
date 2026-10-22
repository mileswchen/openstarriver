/// <reference types="vitest/globals" />
import { describe, it, expect, vi, beforeEach } from 'vitest';
import React from 'react';
import { render, screen, fireEvent, waitFor } from '@testing-library/react';
import { I18nProvider } from './i18n';

declare global { interface Window { starriver: any; } }

const TRACE = {
  entries: [
    { kind: 'system', text: '[system prompt]\n\nYou are Starriver, an interactive coding agent.', seq: 0, timeMs: 1_000 },
    { kind: 'context', text: '[runtime-context]\n\n## workspace\n/tmp/demo', toolName: 'runtime-context', seq: 1, timeMs: 1_100 },
    { kind: 'user', text: '把按钮改成蓝色', seq: 2, timeMs: 1_200 },
    { kind: 'thinking', text: '先找到样式文件。\n再看变量。', seq: 3, timeMs: 1_300 },
    { kind: 'assistant', text: '改好了。', seq: 5, timeMs: 1_700, turn: 1, step: 1, inputTokens: 100, outputTokens: 20, provider: 'deepseek', model: 'deepseek-chat', ttftMs: 250, durationMs: 400 },
    { kind: 'tool_use', toolName: 'bash', toolInput: '{"command":"ls"}', toolOutput: null, toolOk: null, seq: 6, timeMs: 1_800, turn: 1, step: 1 },
    { kind: 'tool_result', toolName: null, toolInput: null, toolOutput: 'a.txt\nb.txt', toolOk: true, seq: 7, timeMs: 1_900, turn: 1, step: 1 },
    { kind: 'tool_use', toolName: 'edit', toolInput: '{"file":"App.tsx"}', toolOutput: null, toolOk: null, seq: 8, timeMs: 2_000, turn: 1, step: 2 },
    { kind: 'tool_result', toolName: null, toolInput: null, toolOutput: 'old_string not found', toolOk: false, seq: 9, timeMs: 2_100, turn: 1, step: 2 },
  ],
};

beforeEach(() => {
  Element.prototype.scrollIntoView = vi.fn();
  localStorage.setItem('starriver-language', 'en');
  window.starriver = {
    getSessionTrace: vi.fn().mockResolvedValue(TRACE),
    listWorkspaces: vi.fn().mockResolvedValue([]),
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
    setActiveWorkspace: vi.fn().mockResolvedValue(undefined),
    getActiveWorkspace: vi.fn().mockResolvedValue(null),
    createWorkspace: vi.fn(), renameWorkspace: vi.fn().mockResolvedValue(undefined), deleteWorkspace: vi.fn().mockResolvedValue(undefined),
    setAgentActiveModel: vi.fn().mockResolvedValue(undefined),
    setAgentReasoningEffort: vi.fn().mockResolvedValue(undefined),
    execute: vi.fn(),
  };
});

describe('CodingView — 会话/轨迹 视图切换', () => {
  it('New Task 态无分段开关、无标题栏无状态栏;进入会话后开关渲染、模式在底部状态栏目录右侧', async () => {
    window.starriver.listWorkspaces = vi.fn().mockResolvedValue([
      { id: 'w1', path: '/tmp/demo', title: 'demo', createdAt: '', sessionIds: ['s1'] },
    ]);
    // Activated workspace so the status bar renders the directory the mode
    // item must sit right of.
    window.starriver.getActiveWorkspace = vi.fn().mockResolvedValue('w1');
    window.starriver.listSessions = vi.fn().mockResolvedValue([
      { id: 's1', title: 'blue button', workspaceId: 'w1', createdAt: '', messageCount: 3 },
    ]);
    window.starriver.getSessionMessages = vi.fn().mockResolvedValue([
      { role: 'user', content: '把按钮改成蓝色' },
      { role: 'assistant', content: '改好了。' },
    ]);
    window.starriver.getAgentMode = vi.fn().mockImplementation(async (sid?: string) =>
      sid === 's1' ? 'plan' : 'standard');
    const { CodingView, WorkspaceProvider, WorkspaceSection } = await import('./app');
    render(<I18nProvider><WorkspaceProvider><WorkspaceSection onOpenCoding={() => {}} /><CodingView debugEnabled={false} /></WorkspaceProvider></I18nProvider>);

    // New Task screen: no Conversation/Trajectory switch, no title bar, no
    // status bar at all — the mode switch + hero are the whole screen.
    await waitFor(() => expect(document.querySelector('.message-list.new-task')).toBeTruthy());
    expect(screen.queryByRole('tab', { name: 'Conversation' })).toBeNull();
    expect(screen.queryByRole('tab', { name: 'Trajectory' })).toBeNull();
    expect(document.querySelector('.coding-status-bar')).toBeNull();

    // Entering the session brings the switch back and pins the mode in the
    // bottom status bar, right of the workspace directory.
    fireEvent.click(await screen.findByText('blue button'));
    const chat = await screen.findByRole('tab', { name: 'Conversation' });
    const traj = screen.getByRole('tab', { name: 'Trajectory' });
    expect(chat.getAttribute('aria-selected')).toBe('true');
    expect(traj.getAttribute('aria-selected')).toBe('false');
    const modeEl = document.querySelector('.coding-status-mode')!;
    await waitFor(() => expect(modeEl.textContent).toContain('Plan (read-only planning)'));
    const cwd = document.querySelector('.coding-status-cwd')!;
    expect(cwd.compareDocumentPosition(modeEl) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(document.querySelector('.coding-status-bar')!.contains(modeEl)).toBeTruthy();
  });

  it('点击轨迹切换到账本视图:user 分组起 Turn,tool_use+result 合并一行可展开', async () => {
    window.starriver.listWorkspaces = vi.fn().mockResolvedValue([
      { id: 'w1', path: '/tmp/demo', title: 'demo', createdAt: '', sessionIds: ['s1'] },
    ]);
    window.starriver.listSessions = vi.fn().mockResolvedValue([
      { id: 's1', title: 'blue button', workspaceId: 'w1', createdAt: '', messageCount: 3 },
    ]);
    window.starriver.getSessionMessages = vi.fn().mockResolvedValue([
      { role: 'user', content: '把按钮改成蓝色' },
      { role: 'assistant', content: '改好了。' },
    ]);
    const { CodingView, WorkspaceProvider, WorkspaceSection } = await import('./app');
    render(<I18nProvider><WorkspaceProvider><WorkspaceSection onOpenCoding={() => {}} /><CodingView debugEnabled={false} /></WorkspaceProvider></I18nProvider>);
    fireEvent.click(await screen.findByText('blue button'));
    await waitFor(() => expect(window.starriver.getSessionMessages).toHaveBeenCalled());
    fireEvent.click(screen.getByRole('tab', { name: 'Trajectory' }));

    await waitFor(() => expect(window.starriver.getSessionTrace).toHaveBeenCalled());
    expect(document.querySelector('.message-list')).toBeNull();
    await waitFor(() => expect(document.querySelectorAll('.traj-row').length).toBe(7));
    expect(screen.getByText('Turn 1').textContent).toContain('1');
    expect(screen.getByText(/bash/).textContent).toContain('✓');
    const toolRow = document.querySelectorAll('.traj-row')[5] as HTMLDetailsElement;
    expect(toolRow.open).toBe(false);
    fireEvent.click(toolRow.querySelector('summary')!);
    await waitFor(() => expect(toolRow.open).toBe(true));
    expect(toolRow.querySelector('.traj-code')?.textContent).toContain('ls');
    expect(toolRow.querySelectorAll('.traj-code')[1]?.textContent).toContain('a.txt');
    fireEvent.click(screen.getByRole('tab', { name: 'Conversation' }));
    expect(document.querySelector('.traj-view')).toBeNull();
    expect(document.querySelector('.message-list')).toBeTruthy();
  });

  it('账本模式:密集事件表,类型标签+工具行合并结果,点击行展开详情,选择持久化', async () => {
    window.starriver.listWorkspaces = vi.fn().mockResolvedValue([
      { id: 'w1', path: '/tmp/demo', title: 'demo', createdAt: '', sessionIds: ['s1'] },
    ]);
    window.starriver.listSessions = vi.fn().mockResolvedValue([
      { id: 's1', title: 'blue button', workspaceId: 'w1', createdAt: '', messageCount: 3 },
    ]);
    const { CodingView, WorkspaceProvider, WorkspaceSection } = await import('./app');
    render(<I18nProvider><WorkspaceProvider><WorkspaceSection onOpenCoding={() => {}} /><CodingView debugEnabled={false} /></WorkspaceProvider></I18nProvider>);
    fireEvent.click(await screen.findByText('blue button'));
    fireEvent.click(screen.getByRole('tab', { name: 'Trajectory' }));
    await waitFor(() => expect(document.querySelectorAll('.traj-row').length).toBe(7));

    // Switch to ledger mode.
    fireEvent.click(screen.getByRole('button', { name: 'Ledger' }));
    await waitFor(() => expect(document.querySelectorAll('.traj-ledger-row').length).toBe(7));
    expect(document.querySelector('.traj-view')).toBeNull();
    expect(localStorage.getItem('starriver.trajMode')).toBe('ledger');

    // Kind tags carry data-kind for styling; the turn's first row is marked.
    const rows = Array.from(document.querySelectorAll('.traj-ledger-row')) as HTMLElement[];
    expect(rows.map((r) => r.dataset.kind)).toEqual(['system', 'context', 'user', 'thinking', 'assistant', 'tool', 'tool']);
    expect(rows[2].dataset.turnStart).toBe('true', 'the user row starts its own turn after the context preamble group');
    // (the two tool rows trail the assistant inside the same turn group)
    expect(rows[0].dataset.turnStart).toBeUndefined('preamble rows (SYSTEM/CONTEXT) carry no Turn marker');
    expect(rows[1].dataset.turnStart).toBeUndefined();
    // No Turn label anywhere in the preamble; the user turn's gutter (row 3)
    // carries "Turn 1" — turn numbering counts only real turns.
    expect(rows[0].querySelector('.traj-ledger-turnlabel')).toBeNull();
    expect(rows[2].querySelector('.traj-ledger-turnlabel')?.textContent).toContain('1');

    // Tool row merges call + result on one line.
    const toolRow = rows[5];
    expect(toolRow.querySelector('.traj-ledger-toolname')?.textContent).toBe('bash');
    expect(toolRow.querySelector('.traj-ledger-payload')?.textContent).toContain('ls');
    expect(toolRow.querySelector('.traj-ledger-result')?.textContent).toContain('a.txt');

    // Click expands the inline detail; click again collapses.
    fireEvent.click(toolRow);
    await waitFor(() => expect(document.querySelector('.traj-ledger-detail')).toBeTruthy());
    const codes = document.querySelectorAll('.traj-ledger-detail .traj-code');
    expect(codes[0]?.textContent).toContain('ls');
    expect(codes[1]?.textContent).toContain('a.txt');
    fireEvent.click(toolRow);
    await waitFor(() => expect(document.querySelector('.traj-ledger-detail')).toBeNull());

    // Switching back to list restores the grouped cards and persists 'list'.
    fireEvent.click(screen.getByRole('button', { name: 'List' }));
    await waitFor(() => expect(document.querySelectorAll('.traj-row').length).toBe(7));
    expect(localStorage.getItem('starriver.trajMode')).toBe('list');
  });

  it('账本工具栏:折叠轮次/调用、搜索过滤、右侧检查器(摘要/载荷/结果/计时)', async () => {
    window.starriver.listWorkspaces = vi.fn().mockResolvedValue([
      { id: 'w1', path: '/tmp/demo', title: 'demo', createdAt: '', sessionIds: ['s1'] },
    ]);
    window.starriver.listSessions = vi.fn().mockResolvedValue([
      { id: 's1', title: 'blue button', workspaceId: 'w1', createdAt: '', messageCount: 3 },
    ]);
    const { CodingView, WorkspaceProvider, WorkspaceSection } = await import('./app');
    render(<I18nProvider><WorkspaceProvider><WorkspaceSection onOpenCoding={() => {}} /><CodingView debugEnabled={false} /></WorkspaceProvider></I18nProvider>);
    fireEvent.click(await screen.findByText('blue button'));
    fireEvent.click(screen.getByRole('tab', { name: 'Trajectory' }));
    fireEvent.click(await screen.findByRole('button', { name: 'Ledger' }));
    await waitFor(() => expect(document.querySelectorAll('.traj-ledger-row').length).toBe(7));

    // Search narrows rows to matches (assistant text contains 'done').
    const search = document.querySelector('.traj-ledger-search') as HTMLInputElement;
    fireEvent.change(search, { target: { value: '改好了' } });
    await waitFor(() => expect(document.querySelectorAll('.traj-ledger-row').length).toBe(1));
    fireEvent.change(search, { target: { value: '' } });
    await waitFor(() => expect(document.querySelectorAll('.traj-ledger-row').length).toBe(7));

    // Collapse calls: tool rows fold into "… 1 tool call" summaries.
    fireEvent.click(screen.getByRole('button', { name: 'Collapse calls' }));
    await waitFor(() => expect(document.querySelectorAll('.traj-ledger-row').length).toBe(5));
    // The summary re-expands the group.
    fireEvent.click(document.querySelector('.traj-ledger-fold')!);
    await waitFor(() => expect(document.querySelectorAll('.traj-ledger-row').length).toBe(7));

    // Inspector: click the tool row → Result tab carries the output.
    const toolRow = Array.from(document.querySelectorAll('.traj-ledger-row'))
      .find((r) => r.getAttribute('data-kind') === 'tool') as HTMLElement;
    fireEvent.click(toolRow);
    await waitFor(() => expect(document.querySelector('.traj-inspector')).toBeTruthy());
    fireEvent.click(screen.getByRole('tab', { name: 'Result' }));
    await waitFor(() => expect(document.querySelector('.traj-inspector-body')?.textContent).toContain('a.txt'));
    fireEvent.click(screen.getByRole('tab', { name: 'Summary' }));
    expect(document.querySelector('.traj-inspector-dl')?.textContent).toContain('#');
    // Close.
    fireEvent.click(document.querySelector('.traj-inspector-close')!);
    await waitFor(() => expect(document.querySelector('.traj-inspector')).toBeNull());
  });

  it('时间线小地图:泳道分类、错误色、点击跳转选中', async () => {
    window.starriver.listWorkspaces = vi.fn().mockResolvedValue([
      { id: 'w1', path: '/tmp/demo', title: 'demo', createdAt: '', sessionIds: ['s1'] },
    ]);
    window.starriver.listSessions = vi.fn().mockResolvedValue([
      { id: 's1', title: 'blue button', workspaceId: 'w1', createdAt: '', messageCount: 3 },
    ]);
    const { CodingView, WorkspaceProvider, WorkspaceSection } = await import('./app');
    render(<I18nProvider><WorkspaceProvider><WorkspaceSection onOpenCoding={() => {}} /><CodingView debugEnabled={false} /></WorkspaceProvider></I18nProvider>);
    fireEvent.click(await screen.findByText('blue button'));
    fireEvent.click(screen.getByRole('tab', { name: 'Trajectory' }));
    fireEvent.click(await screen.findByRole('button', { name: 'Ledger' }));
    await waitFor(() => expect(document.querySelectorAll('.traj-tl-bar').length).toBe(7));

    const bars = Array.from(document.querySelectorAll('.traj-tl-bar')) as HTMLElement[];
    // lane mapping: user/context → lane-0, system/thinking/assistant → lane-1, tool → lane-2
    const lanes = bars.map((b) => b.className.includes('lane-'));
    expect(lanes.every(Boolean)).toBe(true);
    expect(bars[0].className).toContain('lane-1'); // system → model lane
    expect(bars[2].className).toContain('lane-0'); // user → input lane
    expect(bars[5].className).toContain('lane-2'); // tool → tools lane
    // The errored edit tool bar carries the err class (last tool row, index 6).
    expect(bars[6].className).toContain('err');
    // Clicking a bar selects the row and opens the inspector.
    fireEvent.click(bars[5]);
    await waitFor(() => expect(document.querySelector('.traj-inspector')).toBeTruthy());
  });

  it('检查器 Timing 页:TTFT/解码时长渲染,无时间数据时该页缺席', async () => {
    window.starriver.listWorkspaces = vi.fn().mockResolvedValue([
      { id: 'w1', path: '/tmp/demo', title: 'demo', createdAt: '', sessionIds: ['s1'] },
    ]);
    window.starriver.listSessions = vi.fn().mockResolvedValue([
      { id: 's1', title: 'blue button', workspaceId: 'w1', createdAt: '', messageCount: 3 },
    ]);
    const { CodingView, WorkspaceProvider, WorkspaceSection } = await import('./app');
    render(<I18nProvider><WorkspaceProvider><WorkspaceSection onOpenCoding={() => {}} /><CodingView debugEnabled={false} /></WorkspaceProvider></I18nProvider>);
    fireEvent.click(await screen.findByText('blue button'));
    fireEvent.click(screen.getByRole('tab', { name: 'Trajectory' }));
    fireEvent.click(await screen.findByRole('button', { name: 'Ledger' }));
    await waitFor(() => expect(document.querySelectorAll('.traj-ledger-row').length).toBe(7));

    // Assistant row (has ttftMs/durationMs) → Timing tab with formatted values.
    const assistantRow = Array.from(document.querySelectorAll('.traj-ledger-row'))
      .find((r) => r.getAttribute('data-kind') === 'assistant') as HTMLElement;
    fireEvent.click(assistantRow);
    await waitFor(() => expect(document.querySelector('.traj-inspector')).toBeTruthy());
    fireEvent.click(screen.getByRole('tab', { name: 'Timing' }));
    const body = document.querySelector('.traj-inspector-body')!;
    expect(body.textContent).toContain('250ms');           // fmtMs < 1000 → raw ms
    expect(body.textContent).toContain('400ms');
    fireEvent.click(screen.getByRole('tab', { name: 'Summary' }));
    expect(body.textContent).toContain('deepseek-chat');
    expect(body.textContent).toContain('100');

    // No-row click → close the panel.
    fireEvent.click(document.querySelector('.traj-inspector-close')!);
    await waitFor(() => expect(document.querySelector('.traj-inspector')).toBeNull());
  });

  it('折叠轮次:工具栏开关产生折叠摘要,点击摘要仅展开该轮', async () => {
    window.starriver.listWorkspaces = vi.fn().mockResolvedValue([
      { id: 'w1', path: '/tmp/demo', title: 'demo', createdAt: '', sessionIds: ['s1'] },
    ]);
    window.starriver.listSessions = vi.fn().mockResolvedValue([
      { id: 's1', title: 'blue button', workspaceId: 'w1', createdAt: '', messageCount: 3 },
    ]);
    const { CodingView, WorkspaceProvider, WorkspaceSection } = await import('./app');
    render(<I18nProvider><WorkspaceProvider><WorkspaceSection onOpenCoding={() => {}} /><CodingView debugEnabled={false} /></WorkspaceProvider></I18nProvider>);
    fireEvent.click(await screen.findByText('blue button'));
    fireEvent.click(screen.getByRole('tab', { name: 'Trajectory' }));
    fireEvent.click(await screen.findByRole('button', { name: 'Ledger' }));
    await waitFor(() => expect(document.querySelectorAll('.traj-ledger-row').length).toBe(7));

    fireEvent.click(screen.getByRole('button', { name: 'Collapse turns' }));
    // Two groups (the implicit preamble + the user turn), each folded to its
    // first row; groups without tool calls get no fold summary.
    await waitFor(() => expect(document.querySelectorAll('.traj-ledger-row').length).toBe(2));
    expect(document.querySelectorAll('.traj-ledger-fold').length).toBe(1);
    expect(document.querySelector('.traj-ledger-fold')?.textContent).toContain('2 tool calls');
    // Expanding one fold re-expands ONLY that group: the preamble group stays
    // folded (1 row) while the user turn's 5 rows return.
    fireEvent.click(document.querySelector('.traj-ledger-fold')!);
    await waitFor(() => expect(document.querySelectorAll('.traj-ledger-row').length).toBe(6));
    expect(Array.from(document.querySelectorAll('.traj-ledger-row')).some((r) => r.textContent?.includes('把按钮改成蓝色'))).toBe(true);
    // Partially collapsed → the toolbar still offers "Collapse turns";
    // toggling twice (collapse-all, then expand-all) returns every row.
    fireEvent.click(screen.getByRole('button', { name: 'Collapse turns' }));
    await waitFor(() => expect(document.querySelectorAll('.traj-ledger-row').length).toBe(2));
    fireEvent.click(screen.getByRole('button', { name: 'Expand turns' }));
    await waitFor(() => expect(document.querySelectorAll('.traj-ledger-row').length).toBe(7));
  });

  it('搜索无匹配:0 行 + 命中计数,清空恢复', async () => {
    window.starriver.listWorkspaces = vi.fn().mockResolvedValue([
      { id: 'w1', path: '/tmp/demo', title: 'demo', createdAt: '', sessionIds: ['s1'] },
    ]);
    window.starriver.listSessions = vi.fn().mockResolvedValue([
      { id: 's1', title: 'blue button', workspaceId: 'w1', createdAt: '', messageCount: 3 },
    ]);
    const { CodingView, WorkspaceProvider, WorkspaceSection } = await import('./app');
    render(<I18nProvider><WorkspaceProvider><WorkspaceSection onOpenCoding={() => {}} /><CodingView debugEnabled={false} /></WorkspaceProvider></I18nProvider>);
    fireEvent.click(await screen.findByText('blue button'));
    fireEvent.click(screen.getByRole('tab', { name: 'Trajectory' }));
    fireEvent.click(await screen.findByRole('button', { name: 'Ledger' }));
    await waitFor(() => expect(document.querySelectorAll('.traj-ledger-row').length).toBe(7));
    const search = document.querySelector('.traj-ledger-search') as HTMLInputElement;
    fireEvent.change(search, { target: { value: 'no-such-string-xyz' } });
    await waitFor(() => expect(document.querySelectorAll('.traj-ledger-row').length).toBe(0));
    expect(document.querySelector('.traj-ledger-hits')?.textContent).toContain('0');
    fireEvent.change(search, { target: { value: '' } });
    await waitFor(() => expect(document.querySelectorAll('.traj-ledger-row').length).toBe(7));
  });

  it('模式持久化:localStorage 预置 ledger 直接渲染账本,未知值回落列表', async () => {
    window.starriver.listWorkspaces = vi.fn().mockResolvedValue([
      { id: 'w1', path: '/tmp/demo', title: 'demo', createdAt: '', sessionIds: ['s1'] },
    ]);
    window.starriver.listSessions = vi.fn().mockResolvedValue([
      { id: 's1', title: 'blue button', workspaceId: 'w1', createdAt: '', messageCount: 3 },
    ]);
    const { CodingView, WorkspaceProvider, WorkspaceSection } = await import('./app');

    // Pre-set 'ledger' → ledger renders without clicking Ledger.
    localStorage.setItem('starriver.trajMode', 'ledger');
    const first = render(<I18nProvider><WorkspaceProvider><WorkspaceSection onOpenCoding={() => {}} /><CodingView debugEnabled={false} /></WorkspaceProvider></I18nProvider>);
    fireEvent.click(await screen.findByText('blue button'));
    fireEvent.click(screen.getByRole('tab', { name: 'Trajectory' }));
    await waitFor(() => expect(document.querySelectorAll('.traj-ledger-row').length).toBe(7));
    expect(document.querySelector('.traj-view')).toBeNull();
    first.unmount();

    // Unknown stored value falls back to list.
    localStorage.setItem('starriver.trajMode', 'bogus');
    const second = render(<I18nProvider><WorkspaceProvider><WorkspaceSection onOpenCoding={() => {}} /><CodingView debugEnabled={false} /></WorkspaceProvider></I18nProvider>);
    fireEvent.click(await screen.findByText('blue button'));
    fireEvent.click(screen.getByRole('tab', { name: 'Trajectory' }));
    await waitFor(() => expect(document.querySelectorAll('.traj-row').length).toBe(7));
    expect(document.querySelectorAll('.traj-ledger-row').length).toBe(0);
    second.unmount();
    localStorage.removeItem('starriver.trajMode');
  });

  it('trace 拉取失败:静默回退空态,不崩溃', async () => {
    window.starriver.listWorkspaces = vi.fn().mockResolvedValue([
      { id: 'w1', path: '/tmp/demo', title: 'demo', createdAt: '', sessionIds: ['s1'] },
    ]);
    window.starriver.listSessions = vi.fn().mockResolvedValue([
      { id: 's1', title: 'blue button', workspaceId: 'w1', createdAt: '', messageCount: 3 },
    ]);
    window.starriver.getSessionTrace = vi.fn().mockRejectedValue(new Error('boom'));
    const { CodingView, WorkspaceProvider, WorkspaceSection } = await import('./app');
    render(<I18nProvider><WorkspaceProvider><WorkspaceSection onOpenCoding={() => {}} /><CodingView debugEnabled={false} /></WorkspaceProvider></I18nProvider>);
    fireEvent.click(await screen.findByText('blue button'));
    fireEvent.click(await screen.findByRole('tab', { name: 'Trajectory' }));
    await waitFor(() =>
      expect(screen.getByText('No trajectory yet — run a task first.')).toBeTruthy()
    );
  });

  it('New Task 态没有轨迹入口,不拉取 trace', async () => {
    const { CodingView, WorkspaceProvider, WorkspaceSection } = await import('./app');
    render(<I18nProvider><WorkspaceProvider><WorkspaceSection onOpenCoding={() => {}} /><CodingView debugEnabled={false} /></WorkspaceProvider></I18nProvider>);
    await waitFor(() => expect(document.querySelector('.message-list.new-task')).toBeTruthy());
    expect(screen.queryByRole('tab', { name: 'Trajectory' })).toBeNull();
    expect(window.starriver.getSessionTrace).not.toHaveBeenCalled();
  });

  it('完整归档: 归档源恢复折叠段并显示压缩行;默认当前上下文不拉 archive', async () => {
    window.starriver.listWorkspaces = vi.fn().mockResolvedValue([
      { id: 'w1', path: '/tmp/demo', title: 'demo', createdAt: '', sessionIds: ['s1'] },
    ]);
    window.starriver.listSessions = vi.fn().mockResolvedValue([
      { id: 's1', title: 'blue button', workspaceId: 'w1', createdAt: '', messageCount: 3 },
    ]);
    const ARCHIVE = {
      entries: [
        { kind: 'user', text: 'earlier task before compaction', seq: 2, timeMs: 900 },
        { kind: 'assistant', text: 'done earlier', seq: 3, timeMs: 950 },
        { kind: 'compaction', text: '[context compaction]\n\nearlier turns summarized\n\n(entries 0–3 folded out of the live context; they remain in the ledger and are shown around this row.)', seq: 4, timeMs: 1_000 },
        ...TRACE.entries,
      ],
      compactions: 1,
    };
    window.starriver.getSessionArchive = vi.fn().mockResolvedValue(ARCHIVE);
    const { CodingView, WorkspaceProvider, WorkspaceSection } = await import('./app');
    render(<I18nProvider><WorkspaceProvider><WorkspaceSection onOpenCoding={() => {}} /><CodingView debugEnabled={false} /></WorkspaceProvider></I18nProvider>);
    fireEvent.click(await screen.findByText('blue button'));
    fireEvent.click(screen.getByRole('tab', { name: 'Trajectory' }));
    await waitFor(() => expect(window.starriver.getSessionTrace).toHaveBeenCalled());

    // Default source is the live-context projection: no archive fetch, no
    // folded rows.
    expect(window.starriver.getSessionArchive).not.toHaveBeenCalled();
    expect(screen.queryByText('earlier task before compaction')).toBeNull();

    // Toggle to the full archive: the shadowed span comes back with its
    // labeled compaction checkpoint, and the toggle shows the count.
    fireEvent.click(screen.getByRole('button', { name: 'Full archive' }));
    await waitFor(() => expect(window.starriver.getSessionArchive).toHaveBeenCalledWith('s1'));
    await waitFor(() => expect(screen.getAllByText('earlier task before compaction').length).toBeGreaterThan(0));
    expect(screen.getAllByText('done earlier').length).toBeGreaterThan(0);
    expect(screen.getByText('Compaction')).toBeTruthy();
    await waitFor(() => expect(screen.getByRole('button', { name: 'Full archive ·1' })).toBeTruthy());

    // Back to the live context: archive-only rows hide again.
    fireEvent.click(screen.getByRole('button', { name: 'Live context' }));
    expect(screen.queryByText('earlier task before compaction')).toBeNull();
  });
});
