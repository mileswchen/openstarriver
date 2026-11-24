/// <reference types="vitest/globals" />
// BUG-204: the chat transcript froze during a turn while the trajectory kept
// updating. Root cause: the idle/remote-turn chat replay (traceSnapshot →
// loadSessionMessages) lived inside the execute send-callback — the preload
// `turnHandler` SINGLE slot. With no local execute armed (opening an existing
// session while a queued/remote/resumed turn runs, or another view's execute
// replaced the slot), the chat received no events and never re-fetched. The
// trajectory re-fetches its ledger on mount, so it stayed current.
//
// Fix: the replay rides the independent runtime:event fan-out (`onEvent`),
// alive regardless of the turn slot. This test pins that an IDLE session
// (sending=false, no execute called) still replays its history when a
// traceSnapshot for it arrives via onEvent.
import { describe, it, expect, vi, beforeEach } from 'vitest';
import React from 'react';
import { render, screen, waitFor, fireEvent } from '@testing-library/react';
import { I18nProvider } from './i18n';

declare global { interface Window { starriver: any; } }

const WORKSPACE = {
  id: 'w1', path: '/tmp/ws-one', title: 'ws-one', createdAt: '2026-09-27',
  sessionIds: ['s-a'],
};
const SESSION_A = { id: 's-a', title: 'Task A', workspaceId: 'w1', createdAt: '2026-09-27', messageCount: 2 };

let eventSub: ((e: any) => void) | null = null;

beforeEach(() => {
  localStorage.setItem('starriver-language', 'en');
  Element.prototype.scrollIntoView = vi.fn();
  eventSub = null;
  window.starriver = {
    listWorkspaces: vi.fn().mockResolvedValue([WORKSPACE]),
    createWorkspace: vi.fn(),
    setActiveWorkspace: vi.fn().mockResolvedValue(undefined),
    listSessions: vi.fn().mockResolvedValue([SESSION_A]),
    listArchivedSessions: vi.fn().mockResolvedValue([]),
    getSessionMessages: vi.fn().mockResolvedValue([]),
    getSessionArchive: vi.fn().mockResolvedValue({ entries: [], compactions: 0 }),
    listSkills: vi.fn().mockResolvedValue([]),
    listMcpServers: vi.fn().mockResolvedValue([]),
    listModels: vi.fn().mockResolvedValue([]),
    listAgentModelBindings: vi.fn().mockResolvedValue([]),
    // Idle: no session holds the local slot → sending stays false.
    listRunningSessions: vi.fn().mockResolvedValue([]),
    cancel: vi.fn().mockResolvedValue(true),
    queueAdd: vi.fn().mockResolvedValue(null),
    getWorktreeStatus: vi.fn().mockResolvedValue({ isRepo: false, branch: '', isWorktree: false, dirty: false }),
    readWorkspaceInstructions: vi.fn().mockResolvedValue(null),
    getCurrentAgent: vi.fn().mockResolvedValue(null),
    listAgentPresets: vi.fn().mockResolvedValue([]),
    // The fan-out subscriber the fix rides. Capture it so the test can fire
    // envelopes exactly as preload's runtime:event dispatcher would.
    onEvent: vi.fn((cb: (e: any) => void) => { eventSub = cb; return () => { eventSub = null; }; }),
    appendDiag: vi.fn(),
  };
});

async function mountIdleSession() {
  const { WorkspaceProvider, WorkspaceSection, CodingView } = await import('./app');
  render(
    <I18nProvider>
      <WorkspaceProvider>
        <WorkspaceSection onOpenCoding={() => {}} />
        <CodingView debugEnabled={false} />
      </WorkspaceProvider>
    </I18nProvider>,
  );
  await waitFor(() => expect(screen.getByText('Task A')).toBeTruthy());
  fireEvent.click(screen.getByText('Task A'));
  // The open load fetches history once.
  await waitFor(() => expect(window.starriver.getSessionMessages).toHaveBeenCalledWith('s-a'));
}

describe('BUG-204: idle chat resync rides the onEvent fan-out, not the execute slot', () => {
  it('replays history on a traceSnapshot even though no execute armed the turn slot', async () => {
    await mountIdleSession();
    // No execute() was called — the preload turnHandler slot is empty. The
    // independent fan-out subscriber must still be registered.
    expect(window.starriver.onEvent).toHaveBeenCalled();
    expect(eventSub).toBeTruthy();

    // Clear the open-load call so the assertion isolates the replay.
    window.starriver.getSessionMessages.mockClear();

    // A remote/queued turn broadcasts an open-turn snapshot for this session.
    eventSub!({ type: 'traceSnapshot', subscription: 'x', seq: 1, payload: { sessionId: 's-a', epoch: 0, turnIndex: 1, open: true, entries: [] } });

    // Debounced 800ms → the chat re-fetches the session history.
    await waitFor(() => expect(window.starriver.getSessionMessages).toHaveBeenCalledWith('s-a'), { timeout: 3000 });
  });

  it('ignores a traceSnapshot for a foreign session', async () => {
    await mountIdleSession();
    window.starriver.getSessionMessages.mockClear();
    eventSub!({ type: 'traceSnapshot', subscription: 'x', seq: 1, payload: { sessionId: 'some-other-session', epoch: 0, turnIndex: 1, open: true, entries: [] } });
    // Give the debounce window room; no replay must fire for a foreign sid.
    await new Promise((r) => setTimeout(r, 1100));
    expect(window.starriver.getSessionMessages).not.toHaveBeenCalled();
  });
});

describe('BUG-204: onEvent subscriber guard branches', () => {
  async function mountIdle() {
    const { WorkspaceProvider, WorkspaceSection, CodingView } = await import('./app');
    const utils = render(
      <I18nProvider>
        <WorkspaceProvider>
          <WorkspaceSection onOpenCoding={() => {}} />
          <CodingView debugEnabled={false} />
        </WorkspaceProvider>
      </I18nProvider>,
    );
    await waitFor(() => expect(screen.getByText('Task A')).toBeTruthy());
    fireEvent.click(screen.getByText('Task A'));
    await waitFor(() => expect(window.starriver.getSessionMessages).toHaveBeenCalledWith('s-a'));
    window.starriver.getSessionMessages.mockClear();
    return utils;
  }

  const snap = (payload: any) => ({ type: 'traceSnapshot', subscription: 'x', seq: 1, payload });

  it('ignores a null or undefined envelope', async () => {
    await mountIdle();
    eventSub!(null);
    eventSub!(undefined);
    await new Promise((r) => setTimeout(r, 1000));
    expect(window.starriver.getSessionMessages).not.toHaveBeenCalled();
  });

  it('ignores events that are not traceSnapshot', async () => {
    await mountIdle();
    eventSub!({ type: 'assistantText', payload: { sessionId: 's-a', text: 'hi' } });
    eventSub!({ type: 'toolCall', payload: { sessionId: 's-a' } });
    await new Promise((r) => setTimeout(r, 1000));
    expect(window.starriver.getSessionMessages).not.toHaveBeenCalled();
  });

  it('falls back to the snake_case session_id field', async () => {
    await mountIdle();
    // Some relays carry session_id instead of sessionId; the subscriber must
    // accept either spelling.
    eventSub!(snap({ session_id: 's-a', epoch: 0, turnIndex: 1, open: true, entries: [] }));
    await waitFor(() => expect(window.starriver.getSessionMessages).toHaveBeenCalledWith('s-a'), { timeout: 3000 });
  });

  it('ignores a snapshot carrying no session id at all', async () => {
    await mountIdle();
    eventSub!(snap({ epoch: 0, turnIndex: 1, open: true, entries: [] }));
    await new Promise((r) => setTimeout(r, 1000));
    expect(window.starriver.getSessionMessages).not.toHaveBeenCalled();
  });

  it('ignores a traceSnapshot whose payload is null', async () => {
    await mountIdle();
    // payload ?? {} → no session id → the subscriber bails before arming.
    eventSub!(snap(null));
    await new Promise((r) => setTimeout(r, 1000));
    expect(window.starriver.getSessionMessages).not.toHaveBeenCalled();
  });

  it('coalesces rapid snapshots into a single debounced reload', async () => {
    await mountIdle();
    // Three snapshots inside the 800ms window: the timer is armed once, the
    // rest see it already set and skip → exactly one reload.
    eventSub!(snap({ sessionId: 's-a', epoch: 0, turnIndex: 1, open: true, entries: [] }));
    eventSub!(snap({ sessionId: 's-a', epoch: 0, turnIndex: 2, open: true, entries: [] }));
    eventSub!(snap({ sessionId: 's-a', epoch: 0, turnIndex: 3, open: true, entries: [] }));
    await waitFor(() => expect(window.starriver.getSessionMessages).toHaveBeenCalled(), { timeout: 3000 });
    // Give any (incorrect) extra timer room to fire, then assert exactly one.
    await new Promise((r) => setTimeout(r, 1000));
    expect(window.starriver.getSessionMessages).toHaveBeenCalledTimes(1);
  });

  it('unmount cancels a pending debounced reload', async () => {
    const utils = await mountIdle();
    eventSub!(snap({ sessionId: 's-a', epoch: 0, turnIndex: 1, open: true, entries: [] }));
    // Unmount before the 800ms debounce elapses: the cleanup must clear the
    // timer so no reload fires against an unmounted view.
    utils.unmount();
    await new Promise((r) => setTimeout(r, 1100));
    expect(window.starriver.getSessionMessages).not.toHaveBeenCalled();
  });

  it('skips the fan-out replay while a local turn is sending', async () => {
    // A local execute armed the turn slot and never emits a terminal event,
    // so sending stays true. Local turns render optimistically via
    // assistantText — the fan-out replay must NOT also fire (sending gate).
    if (!(globalThis.crypto as any).randomUUID) {
      (globalThis.crypto as any).randomUUID = () => 'uuid-test';
    }
    window.starriver.execute = vi.fn();
    await mountIdle();
    const ta = screen.getByPlaceholderText('Describe what you want to build…');
    fireEvent.change(ta, { target: { value: 'do a thing' } });
    fireEvent.click(screen.getByText('Send'));
    await waitFor(() => expect(window.starriver.execute).toHaveBeenCalled());
    window.starriver.getSessionMessages.mockClear();
    eventSub!(snap({ sessionId: 's-a', epoch: 0, turnIndex: 1, open: true, entries: [] }));
    await new Promise((r) => setTimeout(r, 1100));
    expect(window.starriver.getSessionMessages).not.toHaveBeenCalled();
  });
});
