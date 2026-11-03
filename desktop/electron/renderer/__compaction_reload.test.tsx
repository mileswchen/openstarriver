/// <reference types="vitest/globals" />
// 2026-10-26 回归守卫：压缩检查点（user/message Replace）遮蔽的历史 Turn 在
// 重新进入会话时必须全部渲染。当天线上事故：5 个检查点吞掉 1648 条内容节点，
// 重载后只剩检查点之后的 484 条。host 投影已钉住 seq 顺序
// (sessions.rs::compacted_turns_stay_in_display_projection)；这里钉渲染端
// 契约——复活的消息、折叠行、检查点之后的消息按序全部出现，一行不丢。
import { describe, it, expect, vi, beforeEach } from 'vitest';
import React from 'react';
import { render, screen, waitFor, fireEvent } from '@testing-library/react';
import { I18nProvider } from './i18n';

declare global { interface Window { starriver: any; } }

// host getSessionMessages 的投影输出形状（ChatMessage[]，role 含 'compaction'）。
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
    // 五条内容全部在 DOM：被遮蔽的历史 + 检查点后的新内容。
    expect(screen.getByText('压缩前的回答')).toBeTruthy();
    expect(screen.getByText('压缩后的请求')).toBeTruthy();
    expect(screen.getByText('压缩后的回答')).toBeTruthy();
    // 折叠行渲染（🗜 CompactionRow），预览标题取结构化摘要首节首行。
    const fold = document.querySelector('[data-testid="compaction-row"]');
    expect(fold).toBeTruthy();
    expect(fold!.textContent).toContain('压缩摘要：修复终端面板');
    // 顺序契约：折叠行夹在复活的历史与检查点后的内容之间。
    const body = document.body.textContent ?? '';
    const iBefore = body.indexOf('压缩前的回答');
    const iFold = body.indexOf('压缩摘要：修复终端面板');
    const iAfter = body.indexOf('压缩后的请求');
    expect(iBefore).toBeGreaterThanOrEqual(0);
    expect(iFold).toBeGreaterThan(iBefore);
    expect(iAfter).toBeGreaterThan(iFold);
    // 展开后完整结构化摘要可见。
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
    // 空摘要不可展开（按钮 disabled）。
    expect((fold!.querySelector('.compaction-button') as HTMLButtonElement).disabled).toBe(true);
  });
});
