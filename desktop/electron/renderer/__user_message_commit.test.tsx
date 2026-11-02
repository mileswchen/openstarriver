/// <reference types="vitest/globals" />
// WI-216 regression: "new session's first message renders twice".
//
// Root cause (fixed in WI-216): the composer's optimistic echo had no
// server identity (seq == null), the fresh-session adoption path skips
// loadSessionMessages (ref-first guard), yet the archive effect still
// fetched the ledger — so the server user row AND the echo rendered as two
// identical bubbles. The fix gives the echo a protocol identity: send()
// mints a clientRequestId, the durable user/message carries it back as a
// `userMessageCommitted` UiEvent with the ledger seq, and stamping the echo
// makes the display filter (`seq == null || seq > maxSeq`) collapse it into
// the archive row.
//
// These tests pin the whole contract:
//   1. happy path — receipt retires the echo, exactly ONE bubble;
//   2. late receipt — the echo collapses even when the archive landed first;
//   3. no receipt (old server) — degraded to the pre-fix rendering, the echo
//      must NOT be silently dropped;
//   4. mismatched receipts (wrong rpcId / wrong sessionId) — negative
//      control: matching is by identity, a loose match would false-retire;
//   5. resending the same text — identity dedup must never eat a distinct
//      message; both renders stay visible.
// If any of these fail, the double-bubble bug is back or the retirement
// path grew teeth it shouldn't have.
import { describe, it, expect, vi, beforeEach } from 'vitest';
import React from 'react';
import { render, screen, waitFor, fireEvent } from '@testing-library/react';
import { I18nProvider } from './i18n';

declare global { interface Window { starriver: any; } }

let executeCbs: ((err: string | null, event: any) => void)[] = [];
let archiveRows: any[] = [];

const freshHero = 'Describe what you want to build…';

beforeEach(() => {
  executeCbs = [];
  archiveRows = [];
  Element.prototype.scrollIntoView = vi.fn();
  localStorage.setItem('starriver-language', 'en');
  window.starriver = {
    listWorkspaces: vi.fn().mockResolvedValue([
      { id: 'w1', path: '/tmp/ws-one', title: 'ws-one', createdAt: '2026-11-01', sessionIds: ['s-a'] },
    ]),
    createWorkspace: vi.fn(),
    setActiveWorkspace: vi.fn().mockResolvedValue(undefined),
    listSessions: vi.fn().mockResolvedValue([
      { id: 's-a', title: 'Task A', workspaceId: 'w1', createdAt: '2026-11-01', messageCount: 1 },
    ]),
    listArchivedSessions: vi.fn().mockResolvedValue([]),
    getSessionMessages: vi.fn().mockResolvedValue([]),
    // Late-binding: the ledger snapshot lands on a MACROTASK after the
    // fetch call, so tests can stage `archiveRows` around the adopt step
    // without racing the microtask queue.
    getSessionArchiveMessages: vi.fn(
      () => new Promise((res) => setTimeout(() => res(archiveRows), 5)),
    ),
    listSkills: vi.fn().mockResolvedValue([]),
    listMcpServers: vi.fn().mockResolvedValue([]),
    listModels: vi.fn().mockResolvedValue([]),
    listAgentModelBindings: vi.fn().mockResolvedValue([]),
    listRunningSessions: vi.fn().mockResolvedValue([]),
    cancel: vi.fn().mockResolvedValue(true),
    queueAdd: vi.fn().mockResolvedValue(null),
    getWorktreeStatus: vi.fn().mockResolvedValue({ isRepo: false, branch: '', isWorktree: false, dirty: false }),
    readWorkspaceInstructions: vi.fn().mockResolvedValue(null),
    getCurrentAgent: vi.fn().mockResolvedValue(null),
    listAgentPresets: vi.fn().mockResolvedValue([]),
    getAgentMode: vi.fn().mockResolvedValue('standard'),
    getPermission: vi.fn().mockResolvedValue(null),
    execute: vi.fn((_task: string, _sid: string | undefined, cb: (err: string | null, event: any) => void) => {
      executeCbs.push(cb);
    }),
  };
});

async function mount() {
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
}

async function send(text: string) {
  const ta = screen.getByPlaceholderText(freshHero) as HTMLTextAreaElement;
  fireEvent.change(ta, { target: { value: text } });
  fireEvent.keyDown(ta, { key: 'Enter' });
  await waitFor(() => expect(executeCbs.length >= 1).toBe(true));
}

/** Adopt the just-sent turn as a fresh session, like the host would. */
function adopt(sessionId: string) {
  executeCbs[executeCbs.length - 1](null, { type: 'sessionStarted', session_id: sessionId });
}

/** Receipt exactly as the UiEvent bridge serializes it (camelCase wire). */
function receipt(sessionId: string, rpcId: string, seq: number, messageId = 'm-x') {
  executeCbs[executeCbs.length - 1](null, {
    type: 'userMessageCommitted',
    version: 1,
    sessionId,
    rpcId,
    messageId,
    seq,
  });
}

const lastRequestId = (): string => {
  const calls = (window.starriver.execute as any).mock.calls;
  const id = calls[calls.length - 1][8];
  expect(id, 'execute must carry clientRequestId as its 9th argument').toEqual(expect.any(String));
  return id;
};

const userBubbles = () => screen.getAllByText('hello world').length;
const sameBubbles = () => screen.getAllByText('same text').length;
const settle = () => new Promise((r) => setTimeout(r, 50));

describe('WI-216: user prompt echo retirement by clientRequestId', () => {
  it('fresh session: receipt + archive collapse to exactly ONE bubble', async () => {
    await mount();
    await send('hello world');

    // The echo carries the correlation id both ways: up as the execute
    // param, down… eventually. Before any receipt the echo renders once.
    const rpcId = lastRequestId();
    await waitFor(() => expect(userBubbles()).toBe(1));

    archiveRows = [{ role: 'user', content: 'hello world', seq: 9 }];
    adopt('s-fresh');
    await waitFor(() =>
      expect(window.starriver.getSessionArchiveMessages).toHaveBeenCalledWith('s-fresh'),
    );

    // Receipt BEFORE the archive resolves: the echo is stamped with the
    // ledger seq, so the archive row absorbs it.
    receipt('s-fresh', rpcId, 9);
    await settle();

    await waitFor(() => expect(userBubbles()).toBe(1));
  });

  it('late receipt: the echo collapses even when the archive landed first', async () => {
    await mount();
    await send('hello world');
    const rpcId = lastRequestId();
    // Archive first (echo has no seq yet → both render — the bug window).
    archiveRows = [{ role: 'user', content: 'hello world', seq: 9 }];
    adopt('s-fresh');
    await waitFor(() =>
      expect(window.starriver.getSessionArchiveMessages).toHaveBeenCalledWith('s-fresh'),
    );
    await waitFor(() => expect(userBubbles()).toBe(2));

    // The receipt arrives late and must still retire the echo.
    receipt('s-fresh', rpcId, 9);
    await waitFor(() => expect(userBubbles()).toBe(1));
  });

  it('no receipt (old server): degraded to the old rendering, echo never dropped', async () => {
    await mount();
    await send('hello world');
    // An old server never echoes the correlation id: the archive row and
    // the echo coexist — exactly the pre-WI-216 behavior. The retirement
    // path must not "help" by dropping the echo on its own.
    archiveRows = [{ role: 'user', content: 'hello world', seq: 9 }];
    adopt('s-fresh');
    await waitFor(() =>
      expect(window.starriver.getSessionArchiveMessages).toHaveBeenCalledWith('s-fresh'),
    );
    await waitFor(() => expect(userBubbles()).toBe(2));
  });

  it('mismatched receipts must NOT retire the echo (identity matching)', async () => {
    await mount();
    await send('hello world');
    const rpcId = lastRequestId();
    // Wrong correlation id for the right session…
    receipt('s-fresh', 'not-my-request', 9);
    // …and the right id for the wrong session. Neither may stamp the echo.
    receipt('s-other', rpcId, 9);

    archiveRows = [{ role: 'user', content: 'hello world', seq: 9 }];
    adopt('s-fresh');
    await waitFor(() =>
      expect(window.starriver.getSessionArchiveMessages).toHaveBeenCalledWith('s-fresh'),
    );
    // Both rows render: the echo was not falsely retired.
    await waitFor(() => expect(userBubbles()).toBe(2));
  });

  it('resending the same text: identity dedup never eats a distinct message', async () => {
    await mount();
    await send('same text');
    const rpcId1 = lastRequestId();
    archiveRows = [{ role: 'user', content: 'same text', seq: 9 }];
    adopt('s-fresh');
    await waitFor(() =>
      expect(window.starriver.getSessionArchiveMessages).toHaveBeenCalledWith('s-fresh'),
    );
    receipt('s-fresh', rpcId1, 9);
    await waitFor(() => expect(sameBubbles()).toBe(1));

    // Free the input slot, then resend the EXACT same text.
    executeCbs[executeCbs.length - 1](null, {
      type: 'turnEnd', session_id: 's-fresh', reason: 'end_turn', summary: '',
    });
    await settle(); // let the turnEnd setSending(false) commit before re-typing
    await send('same text');
    const rpcId2 = lastRequestId();
    expect(rpcId2).not.toBe(rpcId1);
    // The second echo coexists with the first turn's row (seq 12 > max 9),
    // then its own receipt stamps it — still two DISTINCT messages visible.
    receipt('s-fresh', rpcId2, 12);
    await waitFor(() => expect(sameBubbles()).toBe(2));
  });
});
