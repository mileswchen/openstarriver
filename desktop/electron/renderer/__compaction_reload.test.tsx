/// <reference types="vitest/globals" />
// 2026-10-26 regression guard: history Turns shadowed by a compaction checkpoint (user/message Replace) must
// all render when re-entering the session. Live incident that day: 5 checkpoints swallowed 1648 content nodes,
// leaving only the 484 after the last checkpoint on reload. The host projection already pins seq order
// (sessions.rs::compacted_turns_stay_in_display_projection); this pins the renderer
// contract — revived messages, folded rows, and post-checkpoint messages all appear in order, not one row lost.
import { describe, it, expect, vi, beforeEach } from 'vitest';
import React from 'react';
import { render, screen, waitFor, fireEvent } from '@testing-library/react';
import { I18nProvider } from './i18n';

declare global { interface Window { starriver: any; } }

// The projection output shape of host getSessionMessages (ChatMessage[], role includes 'compaction').
const HISTORY = [
  { role: 'user', content: '压缩前的请求', seq: 0 },
  { role: 'assistant', content: '压缩前的回答', seq: 1 },
  { role: 'compaction', content: '## Primary Request and Intent\n\n压缩摘要：修复终端面板', seq: 2 },
  { role: 'user', content: '压缩后的请求', seq: 3 },
  { role: 'assistant', content: '压缩后的回答', seq: 4 },
];

const WORKSPACE = {
  id: 'w1', path: '/tmp/ws-one', title: 'ws-one', createdAt: '2026-09-27',
  sessionIds: ['s-c'],
};

beforeEach(() => {
  localStorage.setItem('starriver-language', 'en');
  Element.prototype.scrollIntoView = vi.fn();
  window.starriver = {
    listWorkspaces: vi.fn().mockResolvedValue([WORKSPACE]),
    setActiveWorkspace: vi.fn().mockResolvedValue(undefined),
    listSessions: vi.fn().mockResolvedValue([
      { id: 's-c', title: 'Compacted task', workspaceId: 'w1', createdAt: '2026-09-27', messageCount: 5 },
    ]),
    listArchivedSessions: vi.fn().mockResolvedValue([]),
    getSessionMessages: vi.fn().mockResolvedValue(HISTORY),
    getSessionTrace: vi.fn().mockResolvedValue({ entries: [] }),
    listSkills: vi.fn().mockResolvedValue([]),
    listMcpServers: vi.fn().mockResolvedValue([]),
    listModels: vi.fn().mockResolvedValue([]),
    listAgentModelBindings: vi.fn().mockResolvedValue([]),
    listRunningSessions: vi.fn().mockResolvedValue([]),
    listTerminals: vi.fn().mockResolvedValue({ terminals: [] }),
    queueAdd: vi.fn().mockResolvedValue(null),
    getWorktreeStatus: vi.fn().mockResolvedValue({ isRepo: false, branch: '', isWorktree: false, dirty: false }),
    readWorkspaceInstructions: vi.fn().mockResolvedValue(null),
    getCurrentAgent: vi.fn().mockResolvedValue(null),
    listAgentPresets: vi.fn().mockResolvedValue([]),
    getAgentMode: vi.fn().mockResolvedValue('standard'),
    setAgentMode: vi.fn().mockResolvedValue('standard'),
    setAgentActiveModel: vi.fn().mockResolvedValue(undefined),
    setAgentReasoningEffort: vi.fn().mockResolvedValue(undefined),
    execute: vi.fn(),
  };
});

async function mountOpenSession(waitForText = '压缩前的请求') {
  const { WorkspaceProvider, WorkspaceSection, CodingView } = await import('./app');
  render(
    <I18nProvider>
      <WorkspaceProvider>
        <WorkspaceSection onOpenCoding={() => {}} />
        <CodingView debugEnabled={false} />
      </WorkspaceProvider>
    </I18nProvider>,
  );
  await waitFor(() => expect(screen.getByText('Compacted task')).toBeTruthy());
  fireEvent.click(screen.getByText('Compacted task'));
  await waitFor(() => expect(screen.getByText(waitForText)).toBeTruthy());
}

describe('reloaded history after a compaction checkpoint (2026-10-26)', () => {
  it('renders resurrected turns, the fold row, and post-checkpoint turns — nothing dropped', async () => {
    await mountOpenSession();
    // All five content pieces are in the DOM: the shadowed history + the new post-checkpoint content.
    expect(screen.getByText('压缩前的回答')).toBeTruthy();
    expect(screen.getByText('压缩后的请求')).toBeTruthy();
    expect(screen.getByText('压缩后的回答')).toBeTruthy();
    // The folded row renders (🗜 CompactionRow); the preview title takes the first line of the first section of the structured summary.
    const fold = document.querySelector('[data-testid="compaction-row"]');
    expect(fold).toBeTruthy();
    expect(fold!.textContent).toContain('压缩摘要：修复终端面板');
    // Order contract: the folded row sits between the revived history and the post-checkpoint content.
    const body = document.body.textContent ?? '';
    const iBefore = body.indexOf('压缩前的回答');
    const iFold = body.indexOf('压缩摘要：修复终端面板');
    const iAfter = body.indexOf('压缩后的请求');
    expect(iBefore).toBeGreaterThanOrEqual(0);
    expect(iFold).toBeGreaterThan(iBefore);
    expect(iAfter).toBeGreaterThan(iFold);
    // After expanding, the full structured summary is visible.
    fireEvent.click(fold!.querySelector('.compaction-button')!);
    await waitFor(() => expect(document.body.textContent).toContain('Primary Request and Intent'));
  });

  it('an empty-summary checkpoint stays a non-expanding row, content still renders', async () => {
    window.starriver.getSessionMessages = vi.fn().mockResolvedValue([
      { role: 'user', content: '压缩前的请求', seq: 0 },
      { role: 'compaction', content: '   ', seq: 1 },
      { role: 'user', content: '后', seq: 2 },
    ]);
    await mountOpenSession('压缩前的请求');
    expect(screen.getByText('后')).toBeTruthy();
    const fold = document.querySelector('[data-testid="compaction-row"]') as HTMLElement | null;
    expect(fold).toBeTruthy();
    // An empty summary cannot be expanded (button disabled).
    expect((fold!.querySelector('.compaction-button') as HTMLButtonElement).disabled).toBe(true);
  });
});
