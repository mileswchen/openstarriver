import { describe, it, expect, vi, beforeEach } from 'vitest';
import React from 'react';
import { render, screen, waitFor, fireEvent } from '@testing-library/react';
import { I18nProvider } from './i18n';

declare global { interface Window { starriver: any; } }

let executeCb: ((err: string | null, event: any) => void) | null = null;

const HISTORY = [
  { id: 'm1', role: 'user', content: '第一条指令', ts: 1000 },
  { id: 'm2', role: 'assistant', content: '完成。', ts: 2000 },
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
      { id: 's1', title: 'font task', workspaceId: 'w1', createdAt: Date.now(), messageCount: 2 },
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
    listRunningSessions: vi.fn().mockResolvedValue([]),
    cancel: vi.fn().mockResolvedValue(true),
    queueList: vi.fn().mockResolvedValue([]),
    queueAdd: vi.fn().mockResolvedValue('q-new'),
    queueUpdate: vi.fn().mockResolvedValue(true),
    queueRemove: vi.fn().mockResolvedValue(true),
    queueFlush: vi.fn().mockResolvedValue(true),
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
  fireEvent.click(await screen.findByText('font task'));
  await screen.findByText('完成。');
}

async function startTurn(text: string) {
  const ta = await screen.findByPlaceholderText('Describe what you want to build…');
  fireEvent.change(ta, { target: { value: text } });
  fireEvent.keyDown(ta, { key: 'Enter' });
  await waitFor(() => expect(executeCb).not.toBeNull());
  executeCb!(null, { type: 'TaskStarted', session_id: 's1', sessionId: 's1' });
}

describe('queued-prompt dock (server-backed)', () => {
  it('queues Enter while running through session.queueAdd and renders the server row', async () => {
    await openSession();
    await startTurn('first task');
    window.starriver.queueAdd = vi.fn().mockResolvedValue('q-1');
    const ta = await screen.findByPlaceholderText('Keep typing to queue follow-ups');
    fireEvent.change(ta, { target: { value: 'queued follow-up' } });
    fireEvent.keyDown(ta, { key: 'Enter' });
    await vi.waitFor(() => expect(window.starriver.queueAdd).toHaveBeenCalledWith('s1', 'queued follow-up'));
    const row = await screen.findByTestId('queue-row');
    expect(row.textContent).toContain('queued follow-up');
    expect((ta as HTMLTextAreaElement).value).toBe('');
  });

  it('edits a queued row through session.queueUpdate', async () => {
    await openSession();
    window.starriver.queueList = vi.fn().mockResolvedValue([{ id: 'q-1', text: 'original text' }]);
    await startTurn('first task');
    fireEvent.click(await screen.findByTestId('queue-edit'));
    const editor = await screen.findByTestId('queue-editor');
    fireEvent.change(editor, { target: { value: 'edited text' } });
    fireEvent.click(screen.getByTestId('queue-save'));
    await vi.waitFor(() =>
      expect(window.starriver.queueUpdate).toHaveBeenCalledWith('s1', 'q-1', 'edited text'));
    await waitFor(() => expect(screen.queryByTestId('queue-editor')).toBeNull());
    expect(screen.getByTestId('queue-row').textContent).toContain('edited text');
  });

  it('removes a queued row through session.queueRemove', async () => {
    await openSession();
    window.starriver.queueList = vi.fn().mockResolvedValue([{ id: 'q-1', text: 'to remove' }]);
    await startTurn('first task');
    fireEvent.click(await screen.findByTestId('queue-remove'));
    await vi.waitFor(() => expect(window.starriver.queueRemove).toHaveBeenCalledWith('s1', 'q-1'));
    await waitFor(() => expect(screen.queryByTestId('queue-dock')).toBeNull());
  });

  it('立即 (legacy host) cancels THEN flushes; the next TaskStarted syncs the emptied queue', async () => {
    await openSession();
    let queueCalls = 0;
    window.starriver.queueList = vi.fn(() => Promise.resolve(
      ++queueCalls === 1 ? [{ id: 'q-1', text: 'queued one' }, { id: 'q-2', text: 'queued two' }] : []));
    await startTurn('first task');
    fireEvent.click((await screen.findAllByTestId('queue-now'))[0]);
    await vi.waitFor(() => expect(window.starriver.cancel).toHaveBeenCalledWith('s1'));
    await vi.waitFor(() => expect(window.starriver.queueFlush).toHaveBeenCalledWith('s1'));
    // ORDER matters: cancel must run BEFORE flush. The 2026-10-07 regression
    // ran flush→cancel across two IPC calls — the trailing session.cancel
    // re-parks the durable queue (keep_inbox=true), so the driver never
    // claimed the flushed message: Send Now did nothing and the host round
    // loop wedged waiting for a follow-up terminal that never came. The
    // parked flag must END false, so the flush has to come last. (The
    // atomic session.queueFlushNow replaces this two-call shape when the
    // host provides it.)
    const flushOrder = (window.starriver.queueFlush as any).mock.invocationCallOrder[0];
    const cancelOrder = (window.starriver.cancel as any).mock.invocationCallOrder.find(
      (n: number) => n > 0);
    expect(cancelOrder).toBeLessThan(flushOrder);
    const logSpy = vi.spyOn(console, 'log');
    executeCb!(null, { type: '__probe__' });
    console.log('HANDLER-ALIVE', logSpy.mock.calls.some(c => String(c[0]).includes('[event]')));
    logSpy.mockRestore();
    executeCb!(null, { type: 'TaskStarted', session_id: 's1', sessionId: 's1' });
    await waitFor(() => expect(screen.queryByTestId('queue-dock')).toBeNull());
  });

  it('立即 prefers the atomic session.queueFlushNow (no separate cancel/flush)', async () => {
    await openSession();
    window.starriver.queueList = vi.fn().mockResolvedValue([{ id: 'q-1', text: 'queued one' }]);
    window.starriver.queueFlushNow = vi.fn().mockResolvedValue(true);
    await startTurn('first task');
    fireEvent.click((await screen.findAllByTestId('queue-now'))[0]);
    await vi.waitFor(() => expect(window.starriver.queueFlushNow).toHaveBeenCalledWith('s1'));
    // The atomic call already cancels + flushes host-side; the legacy pair
    // must stay untouched or the cancel would re-park the queue.
    expect(window.starriver.cancel).not.toHaveBeenCalled();
    expect(window.starriver.queueFlush).not.toHaveBeenCalled();
    // The next TaskStarted still syncs the (now drained) queue away.
    executeCb!(null, { type: 'TaskStarted', session_id: 's1', sessionId: 's1' });
    await waitFor(() => expect(screen.queryByTestId('queue-dock')).toBeNull());
  });

  it('立即 resyncs the queue when queueFlushNow reports no live agent', async () => {
    await openSession();
    window.starriver.queueList = vi.fn().mockResolvedValue([{ id: 'q-1', text: 'queued one' }]);
    window.starriver.queueFlushNow = vi.fn().mockResolvedValue(false);
    await startTurn('first task');
    const callsBefore = (window.starriver.queueList as any).mock.calls.length;
    fireEvent.click((await screen.findAllByTestId('queue-now'))[0]);
    // false = no live agent: the dock resyncs; the durable queue runs with
    // the session's next execute.
    await vi.waitFor(() =>
      expect((window.starriver.queueList as any).mock.calls.length).toBeGreaterThan(callsBefore));
  });

  it('renders the queue dock OUTSIDE the input box frame (above it)', async () => {
    await openSession();
    window.starriver.queueList = vi.fn().mockResolvedValue([{ id: 'q-1', text: 'queued msg' }]);
    await startTurn('first task');
    const dock = await screen.findByTestId('queue-dock');
    const inputBar = dock.parentElement?.querySelector('.input-bar');
    expect(inputBar).not.toBeNull();
    // The dock must be a SIBLING of the input bar, not a child of it: rows
    // and their action buttons used to render inside the input box frame.
    expect(dock.closest('.input-bar')).toBeNull();
    expect(dock.nextElementSibling).toBe(inputBar);
  });

  it('restores a parked queue from session.queueList on session load', async () => {
    window.starriver.queueList = vi.fn().mockResolvedValue([{ id: 'q-9', text: 'parked message' }]);
    await openSession();
    await vi.waitFor(() => expect(window.starriver.queueList).toHaveBeenCalledWith('s1'));
    expect((await screen.findByTestId('queue-row')).textContent).toContain('parked message');
  });
});

describe('queue text helpers', () => {
  const t = (key: string, vars?: Record<string, string | number>) => {
    const table: Record<string, string> = {
      'coding.queuedImage': '🖼 image: {name}',
      'coding.queuedImageUntitled': 'untitled image',
    };
    let text = table[key] ?? key;
    if (vars) for (const [k, v] of Object.entries(vars)) text = text.split(`{${k}}`).join(String(v));
    return text;
  };

  it('queueDisplayText summarizes image refs and keeps plain text verbatim', async () => {
    const { queueDisplayText, composeWithAttachments } = await import('./app');
    expect(queueDisplayText('看看 ![截图](sha256:abc) 这张图', t))
      .toBe('看看 🖼 image: 截图 这张图');
    expect(queueDisplayText('no refs here', t)).toBe('no refs here');
    expect(queueDisplayText('![   ](sha256:x)', t)).toBe('🖼 image: untitled image');
  });

  it('composeWithAttachments mirrors the direct-send payload shape', async () => {
    const { composeWithAttachments } = await import('./app');
    expect(composeWithAttachments('任务', { imageRefs: '![a](sha256:1)', docLinks: '', docTexts: '--- Document ---' }))
      .toBe('任务\n![a](sha256:1)\n\n--- Document ---');
    // No attachments → the text passes through untouched (no trailing \n).
    expect(composeWithAttachments('任务', { imageRefs: '', docLinks: '', docTexts: '' })).toBe('任务');
    // Image-only queue (empty typed text) → attachments alone.
    expect(composeWithAttachments('', { imageRefs: '![a](sha256:1)', docLinks: '', docTexts: '' }))
      .toBe('![a](sha256:1)');
  });
});
