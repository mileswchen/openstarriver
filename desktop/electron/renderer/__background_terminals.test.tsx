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
    setActiveWorkspace: vi.fn().mockResolvedValue(undefined),
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

  it('rows show command + purpose + owning session, and the panel is workspace-scoped', async () => {
    window.starriver.listWorkspaces = vi.fn().mockResolvedValue([
      { id: 'w1', path: '/ws/a', title: 'ws-a', createdAt: '', sessionIds: [] },
    ]);
    window.starriver.listSessions = vi.fn().mockResolvedValue([
      { id: 's-1', title: 'Fix login', createdAt: '', messageCount: 0 },
    ]);
    window.starriver.listTerminals = vi.fn().mockResolvedValue({
      terminals: [
        // 本工作区：命令 + 用途 + 归属会话标题可见。
        { id: 't-1', status: 'running', startedAtMs: Date.now() - 10_000, ownerSessionId: 's-1', workspace: '/ws/a', command: { startedAtMs: Date.now() - 9_000, cmd: 'npm run dev' } },
        // 别的工作区：不显示。
        { id: 't-2', status: 'idle', startedAtMs: Date.now() - 10_000, ownerSessionId: 's-2', workspace: '/ws/other', command: { startedAtMs: Date.now() - 5_000, finishedAtMs: Date.now(), exitCode: 0, cmd: 'tail -f app.log' } },
        // 无 workspace 字段（旧数据）：照常显示，不藏数据。
        { id: 't-3', status: 'running', startedAtMs: Date.now() - 30_000 },
      ],
    });
    await mount();

    // 激活工作区 w1（composer 的 workspace picker → 选中 ws-a；侧栏同名
    // 节点也存在，取按钮型匹配）。
    fireEvent.click(screen.getByLabelText('workspace'));
    const wsItem = screen.getAllByTitle('/ws/a')
      .map(el => el.closest('button.composer-approval-item'))
      .find((b): b is HTMLElement => !!b);
    fireEvent.click(wsItem as HTMLElement);
    await waitFor(() => expect(screen.getByLabelText('workspace').textContent).toContain('ws-a'));

    fireEvent.click(screen.getByLabelText('background-terminals'));
    const panel = screen.getByTestId('terminals-panel');
    // 命令文本 + 用途标签（dev server 识别）+ 会话标题。
    expect(panel.textContent).toContain('npm run dev');
    expect(panel.textContent).toContain('Dev server');
    expect(panel.textContent).toContain('Fix login');
    // 他工作区终端被过滤；无 workspace 的旧行保留。
    expect(panel.textContent).not.toContain('tail -f');
    expect(panel.textContent).toContain('PTY · t-3');
    // 计数 = 2（本工作区 1 + 旧数据 1），不含 t-2。
    expect(panel.querySelector('.terminals-count')?.textContent).toBe('2');
    // tooltip 携带归属会话与完整命令。
    const row = panel.querySelector('.terminal-row') as HTMLElement;
    expect(row.getAttribute('title')).toContain('Fix login');
    expect(row.getAttribute('title')).toContain('npm run dev');
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

  it('shell rows: real commands listed in full; bookkeeping terminal_* tools are not', async () => {
    window.starriver.listTerminals = vi.fn().mockResolvedValue({ terminals: [] });
    let cb: ((err: null, event: any) => void) | null = null;
    window.starriver.execute = vi.fn((...args: any[]) => { cb = args[2]; });
    await mount({ expectButton: false });

    // Arm a live stream to drive tool events.
    const ta = screen.getByPlaceholderText('Describe what you want to build…');
    fireEvent.change(ta, { target: { value: 'hi' } });
    fireEvent.click(screen.getByText('Send'));
    await waitFor(() => expect(cb).not.toBeNull());

    // terminal_wait carries no command → no panel entry, icon stays dark.
    cb!(null, { type: 'toolCall', id: 'w1', name: 'terminal_wait', input: { id: 't9' } });
    await waitFor(() => expect(screen.queryByLabelText('background-terminals')).toBeNull());
    // 2026-10-26: the chat card label shows the FULL arguments JSON — no more
    // bare "🔧 terminal_wait..." while the trajectory shows the payload.
    await waitFor(() => expect(document.body.textContent).toContain('"id":"t9"'));

    // A real bash command → panel lists the FULL command with the shared card
    // layout (the \n escape toolCommandOf uses for chat rows is restored to a
    // real newline so the command wraps instead of truncating).
    cb!(null, { type: 'toolCall', id: 'b1', name: 'bash', input: { command: 'cd /ws &&\\nnpm run package:mac' } });
    const btn = await waitFor(() => screen.getByLabelText('background-terminals'));
    fireEvent.click(btn);
    const panel = await waitFor(() => screen.getByTestId('terminals-panel'));
    await waitFor(() => expect(panel.textContent).toContain('cd /ws &&'));
    expect(panel.textContent).toContain('npm run package:mac');
    expect(panel.querySelector('.terminal-row.shell-row')).toBeTruthy();
    // The meta strip is its own line: start clock + status chip render apart.
    expect(panel.querySelector('.shell-row .terminal-started')).toBeTruthy();
    expect(panel.querySelector('.shell-row .terminal-status.running')).toBeTruthy();
    // Cleanup on result: row disappears.
    cb!(null, { type: 'toolResult', id: 'b1', ok: true, output: '' });
    await waitFor(() => expect(screen.queryByLabelText('background-terminals')).toBeNull());
  });

  // ── 2026-10-26: PTY 行的 Table 列与命令生命周期显示 ──
  // 当天线上 bug：构建命令在跑，但面板行只剩 "PTY · id" 回退文案、idle 状态、
  // 错误的首发时间。渲染端契约：command.cmd 存在就必须显示命令文本；
  // finishedAtMs=null → running，有值 → done · exit N；无命令记录才回退
  // PTY · <id 前 8 位> 并透传 host 状态。
  const ptyRow = (command: any, status = 'running') => ({
    id: 'e0b34f93-4270-41be-84e0-92d59684cdab',
    status,
    startedAtMs: Date.now() - 700_000,
    ownerSessionId: 'sess-1',
    workspace: null,
    command,
  });

  it('PTY row with an in-flight command shows the command text and running chip (not the PTY fallback)', async () => {
    window.starriver.listTerminals = vi.fn().mockResolvedValue({
      terminals: [ptyRow({
        startedAtMs: Date.now() - 690_000, finishedAtMs: null, exitCode: null,
        cmd: 'npm run package:mac > /tmp/pkg.log 2>&1; echo PKG_EXIT=$?',
      })],
    });
    await mount();
    fireEvent.click(screen.getByLabelText('background-terminals'));
    const panel = await waitFor(() => screen.getByTestId('terminals-panel'));
    // Table header renders once, 7 columns (dot|command|session|started|elapsed|status|kill).
    expect(panel.querySelector('.terminals-head-row')).toBeTruthy();
    expect(panel.querySelectorAll('.terminals-head-row > *').length).toBe(7);
    const row = panel.querySelector('.terminal-row')!;
    expect(row.textContent).toContain('npm run package:mac');
    expect(row.textContent).not.toContain('PTY · e0b34f93');
    expect(row.querySelector('.terminal-status.running')).toBeTruthy();
    expect(row.querySelector('.terminal-dot.live')).toBeTruthy();
    expect(row.querySelector('.terminal-started')).toBeTruthy();
    expect(row.querySelector('.terminal-elapsed')).toBeTruthy();
    expect(row.querySelector('.terminal-kill')).toBeTruthy();
  });

  it('PTY row with a finished command shows done · exit N and a dim dot', async () => {
    window.starriver.listTerminals = vi.fn().mockResolvedValue({
      terminals: [ptyRow({
        startedAtMs: Date.now() - 800_000, finishedAtMs: Date.now() - 10, exitCode: 0,
        cmd: 'echo hi',
      })],
    });
    await mount();
    fireEvent.click(screen.getByLabelText('background-terminals'));
    const row = await waitFor(() => screen.getByTestId('terminals-panel').querySelector('.terminal-row')!);
    await waitFor(() => expect(row.querySelector('.terminal-status.done')).toBeTruthy());
    expect(row.querySelector('.terminal-status.done')!.textContent).toContain('exit 0');
    expect(row.querySelector('.terminal-dot.off')).toBeTruthy();
    expect(row.textContent).toContain('echo hi');
  });

  it('PTY row without a command record falls back to PTY · <id8> and passes the host status through', async () => {
    window.starriver.listTerminals = vi.fn().mockResolvedValue({
      terminals: [ptyRow(null, 'idle')],
    });
    await mount();
    fireEvent.click(screen.getByLabelText('background-terminals'));
    const row = await waitFor(() => screen.getByTestId('terminals-panel').querySelector('.terminal-row')!);
    expect(row.textContent).toContain('PTY · e0b34f93');
    expect(row.querySelector('.terminal-status.idle')).toBeTruthy();
  });

  it('live cards: terminal_start/kill and mcp__ tools show their FULL args JSON', async () => {
    window.starriver.listTerminals = vi.fn().mockResolvedValue({ terminals: [] });
    let cb: ((err: null, event: any) => void) | null = null;
    window.starriver.execute = vi.fn((...args: any[]) => { cb = args[2]; });
    await mount({ expectButton: false });
    const ta = screen.getByPlaceholderText('Describe what you want to build…');
    fireEvent.change(ta, { target: { value: 'hi' } });
    fireEvent.click(screen.getByText('Send'));
    await waitFor(() => expect(cb).not.toBeNull());

    // ToolRow splits the label into a title + summary span, so the pin is on
    // the FULL args JSON being visible (the pre-fix card showed neither).
    cb!(null, { type: 'toolCall', id: 's1', name: 'terminal_start', input: { shell: 'bash' } });
    await waitFor(() => expect(document.body.textContent).toContain('{"shell":"bash"}'));
    cb!(null, { type: 'toolResult', id: 's1', ok: true, output: '{"id":"t-1"}' });

    cb!(null, { type: 'toolCall', id: 'k1', name: 'terminal_kill', input: { id: 't-1' } });
    await waitFor(() => expect(document.body.textContent).toContain('{"id":"t-1"}'));
    cb!(null, { type: 'toolResult', id: 'k1', ok: true, output: '' });

    // mcp__ tools have no known display key either — full JSON, not a bare name.
    cb!(null, { type: 'toolCall', id: 'm1', name: 'mcp__zhipu__search_app_tags', input: { organizationId: 'org1', search: 'x' } });
    await waitFor(() => expect(document.body.textContent).toContain('"organizationId":"org1"'));
  });
});
