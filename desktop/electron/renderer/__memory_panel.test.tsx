/// <reference types="vitest/globals" />
import { describe, it, expect, beforeAll, afterAll, vi, beforeEach } from 'vitest';
import React from 'react';
import { render, screen, waitFor, fireEvent } from '@testing-library/react';
import { MemoryTab, memoryProjectIdFromPath } from './app';
import { I18nProvider } from './i18n';

const originalStarriver = (window as any).starriver;
const listMemories = vi.fn();
const deleteMemory = vi.fn();
const clearMemories = vi.fn();
const listWorkspaces = vi.fn();
const upsertMemory = vi.fn();
const recallMemories = vi.fn();
const memoryStatus = vi.fn();
const ingestMemories = vi.fn();
const exportMemories = vi.fn();
const selectDirectory = vi.fn();

beforeAll(() => {
  (window as any).starriver = {
    listMemories, deleteMemory, clearMemories, listWorkspaces, upsertMemory, recallMemories,
    memoryStatus, ingestMemories, exportMemories, selectDirectory,
  };
});
afterAll(() => { (window as any).starriver = originalStarriver; });

beforeEach(() => {
  localStorage.setItem('starriver-language', 'en');
  vi.clearAllMocks();
  upsertMemory.mockImplementation(async (_p: any, name: string, description: string, body: string) =>
    ({ name, description, body, source: 'extract', source_agent: 'manual', project: '_global', created: 1757200000, updated: 1757200000 }));
  recallMemories.mockResolvedValue([entry('alpha', { updated: 1757100000 })]);
  memoryStatus.mockResolvedValue({
    root: '/tmp/mem', projects: [], sources: [], watermark: true,
    budget: { usedTokens: 1200, limitTokens: 200000, calls: 3, deniedCalls: 0, exceeded: false },
  });
  ingestMemories.mockResolvedValue({ llmRouted: true, sources: [], totalMemories: 5 });
  exportMemories.mockResolvedValue({ project: '_global', written: 2 });
  selectDirectory.mockResolvedValue('/tmp/picked');
  listWorkspaces.mockResolvedValue([
    { id: 'w1', path: '/Users/x/demo', title: 'demo', createdAt: '', sessionIds: [] },
  ]);
  listMemories.mockResolvedValue([
    entry('alpha', { updated: 1757100000 }),
    entry('beta', { source: 'compaction', source_agent: 'codex', updated: 1757000000 }),
  ]);
  deleteMemory.mockResolvedValue(undefined);
  clearMemories.mockResolvedValue(2);
});

function entry(name: string, over: Partial<any> = {}) {
  return {
    name, description: `desc of ${name}`, body: `body of ${name}`,
    source: 'extract', source_agent: 'starriver', project: '_global',
    created: 1757000000, ...over,
  };
}

/** Render the Memory tab (en locale) and wait for the global list to land. */
async function renderTab() {
  render(<I18nProvider><MemoryTab /></I18nProvider>);
  await waitFor(() => expect(screen.getByText('desc of alpha')).toBeTruthy());
}

describe('memoryProjectIdFromPath — host slug 规则镜像', () => {
  it('路径非字母数字折叠为 -,保留 Unicode(与 project_id_from_cwd 一致)', () => {
    expect(memoryProjectIdFromPath('/Users/xiaomo/Documents/ccprj/starriver'))
      .toBe('Users-xiaomo-Documents-ccprj-starriver');
    expect(memoryProjectIdFromPath('/tmp/项目A')).toBe('tmp-项目A');
  });
  it('空 slug 回退 _global', () => {
    expect(memoryProjectIdFromPath('/')).toBe('_global');
    expect(memoryProjectIdFromPath('')).toBe('_global');
  });
});

describe('MemoryTab — Phase 1 只读管理', () => {
  it('拉取 _global 列表,渲染名称/描述/来源徽章与计数', async () => {
    await renderTab();
    // memory.list is called without projectId for the global bucket.
    expect(listMemories).toHaveBeenCalledWith(undefined);
    expect(screen.getByText('alpha')).toBeTruthy();
    expect(screen.getByText('beta')).toBeTruthy();
    expect(screen.getByText('Extract')).toBeTruthy();
    expect(screen.getByText('Compaction')).toBeTruthy();
    expect(screen.getByText('codex')).toBeTruthy();
    expect(screen.getByText('2 memories')).toBeTruthy();
  });

  it('客户端搜索过滤 name/description/body', async () => {
    await renderTab();
    const search = screen.getByLabelText('Search memories…');
    fireEvent.change(search, { target: { value: 'beta' } });
    expect(screen.getByText('beta')).toBeTruthy();
    expect(screen.queryByText('alpha')).toBeNull();
    // body matches too: searching the body text surfaces the entry.
    fireEvent.change(search, { target: { value: 'body of alpha' } });
    expect(screen.getByText('alpha')).toBeTruthy();
    expect(screen.queryByText('beta')).toBeNull();
  });

  it('点击条目显示全文详情与元数据', async () => {
    await renderTab();
    fireEvent.click(screen.getByText('beta'));
    const detail = screen.getByTestId('memory-detail');
    expect(detail.textContent).toContain('body of beta');
    expect(detail.textContent).toContain('Source: Compaction');
    expect(detail.textContent).toContain('Agent: codex');
    expect(detail.textContent).toContain('Project: _global');
  });

  it('删除走两步确认:先武装,取消不调用,确认调 deleteMemory(_global, name) 并刷新', async () => {
    await renderTab();
    fireEvent.click(screen.getByText('alpha'));
    // Arm the inline confirm.
    fireEvent.click(screen.getByRole('button', { name: 'Delete' }));
    expect(screen.getByText('Delete "alpha"?')).toBeTruthy();
    // Cancel disarms without touching the bridge.
    fireEvent.click(screen.getByRole('button', { name: 'Cancel' }));
    expect(screen.queryByText('Delete "alpha"?')).toBeNull();
    expect(deleteMemory).not.toHaveBeenCalled();
    // Arm again and confirm.
    fireEvent.click(screen.getByRole('button', { name: 'Delete' }));
    fireEvent.click(screen.getByRole('button', { name: 'Delete' }));
    await waitFor(() => expect(deleteMemory).toHaveBeenCalledWith('_global', 'alpha'));
    await waitFor(() => expect(listMemories).toHaveBeenCalledTimes(2)); // mount + post-delete refresh
  });

  it('清空走两步确认,确认调 clearMemories(当前项目)', async () => {
    await renderTab();
    fireEvent.click(screen.getByRole('button', { name: 'Clear all' }));
    expect(screen.getByText(/Clear all 2 memories in this project\?/)).toBeTruthy();
    fireEvent.click(screen.getByRole('button', { name: 'Clear all' }));
    await waitFor(() => expect(clearMemories).toHaveBeenCalledWith('_global'));
  });

  it('项目选择器含 _global 与工作区 slug,切换后按 slug 拉取', async () => {
    await renderTab();
    const select = screen.getByLabelText('Project') as HTMLSelectElement;
    const options = Array.from(select.options).map((o) => o.value);
    expect(options).toEqual(['_global', 'Users-x-demo']);
    listMemories.mockResolvedValue([entry('gamma', { project: 'Users-x-demo' })]);
    fireEvent.change(select, { target: { value: 'Users-x-demo' } });
    await waitFor(() => expect(listMemories).toHaveBeenCalledWith('Users-x-demo'));
    await waitFor(() => expect(screen.getByText('desc of gamma')).toBeTruthy());
  });

  it('空列表显示空态文案', async () => {
    listMemories.mockResolvedValue([]);
    render(<I18nProvider><MemoryTab /></I18nProvider>);
    await waitFor(() => expect(screen.getByText(/No memories yet/)).toBeTruthy());
  });

  it('桥接错误显示错误条,不伪造数据', async () => {
    listMemories.mockRejectedValue(new Error('boom'));
    render(<I18nProvider><MemoryTab /></I18nProvider>);
    await waitFor(() => expect(screen.getByTestId('memory-error').textContent).toContain('boom'));
  });
});

describe('MemoryTab — Phase 2 手动写入/召回预览', () => {
  it('写入表单:必填校验 → 提交调 upsertMemory(undefined, name, desc, body) 并按落点刷新', async () => {
    await renderTab();
    fireEvent.click(screen.getByTestId('memory-add'));
    const form = screen.getByTestId('memory-form');
    expect(form).toBeTruthy();
    // Required gate: empty fields keep save disabled.
    expect((screen.getByTestId('memory-save') as HTMLButtonElement).disabled).toBe(true);
    fireEvent.change(screen.getByLabelText('Name'), { target: { value: 'Deploy Keys' } });
    fireEvent.change(screen.getByLabelText('Description'), { target: { value: 'rotation policy' } });
    fireEvent.change(screen.getByLabelText('Fact'), { target: { value: 'rotate every 90 days' } });
    expect((screen.getByTestId('memory-save') as HTMLButtonElement).disabled).toBe(false);
    fireEvent.click(screen.getByTestId('memory-save'));
    await waitFor(() => expect(upsertMemory).toHaveBeenCalledWith(undefined, 'Deploy Keys', 'rotation policy', 'rotate every 90 days'));
    // The view jumps to where the entry landed (slugified project view) and the save banner shows.
    await waitFor(() => expect(screen.getByTestId('memory-saved')).toBeTruthy());
  });

  it('全局开关:勾选后 projectId 传 undefined(host 落 _global)', async () => {
    await renderTab();
    fireEvent.click(screen.getByTestId('memory-add'));
    fireEvent.change(screen.getByLabelText('Name'), { target: { value: 'pref' } });
    fireEvent.change(screen.getByLabelText('Description'), { target: { value: 'd' } });
    fireEvent.change(screen.getByLabelText('Fact'), { target: { value: 'b' } });
    fireEvent.click(screen.getByLabelText('Store globally (all projects)'));
    fireEvent.click(screen.getByTestId('memory-save'));
    await waitFor(() => expect(upsertMemory).toHaveBeenCalledWith(undefined, 'pref', 'd', 'b'));
  });

  it('手动徽章:source_agent=manual 显示为本地化 Manual 标签', async () => {
    listMemories.mockResolvedValue([
      entry('manual-one', { source_agent: 'manual', updated: 1757200000 }),
    ]);
    render(<I18nProvider><MemoryTab /></I18nProvider>);
    await waitFor(() => expect(screen.getByText('desc of manual-one')).toBeTruthy());
    expect(screen.getByText('Manual')).toBeTruthy();
  });

  it('召回预览:输入任务 → 调 recallMemories → 渲染命中列表;空结果显示空态', async () => {
    await renderTab();
    const input = screen.getByLabelText('Type a task to preview which memories would be injected…');
    fireEvent.change(input, { target: { value: 'deploy the service' } });
    fireEvent.click(screen.getByRole('button', { name: 'Preview' }));
    await waitFor(() => expect(recallMemories).toHaveBeenCalledWith(undefined, 'deploy the service'));
    const results = screen.getByTestId('memory-recall-results');
    expect(results.textContent).toContain('alpha');

    // Empty result → empty note.
    recallMemories.mockResolvedValue([]);
    fireEvent.change(input, { target: { value: 'nothing matches' } });
    fireEvent.click(screen.getByRole('button', { name: 'Preview' }));
    await waitFor(() => expect(screen.getByText('No memories match this task.')).toBeTruthy());
  });
});

describe('MemoryTab — Phase 3 运维条(状态/导入/导出)', () => {
  it('挂载拉取 memory.status 并渲染预算读数', async () => {
    await renderTab();
    expect(memoryStatus).toHaveBeenCalled();
    await waitFor(() => expect(screen.getByTestId('memory-budget')).toBeTruthy());
    expect(screen.getByTestId('memory-budget').textContent).toContain('1200 / 200000 tokens');
  });

  it('预算超限显示暂停告警', async () => {
    memoryStatus.mockResolvedValue({
      budget: { usedTokens: 200000, limitTokens: 200000, calls: 9, deniedCalls: 2, exceeded: true },
    });
    await renderTab();
    await waitFor(() => expect(screen.getByTestId('memory-budget').textContent).toContain('exceeded'));
  });

  it('导入按钮调 memory.ingest 并回显导入条数', async () => {
    await renderTab();
    fireEvent.click(screen.getByTestId('memory-ingest'));
    await waitFor(() => expect(ingestMemories).toHaveBeenCalled());
    await waitFor(() => expect(screen.getByTestId('memory-report').textContent).toContain('Imported 5 memories'));
  });

  it('导出按钮走目录选择并回显落点', async () => {
    await renderTab();
    fireEvent.click(screen.getByTestId('memory-export'));
    await waitFor(() => expect(selectDirectory).toHaveBeenCalled());
    await waitFor(() => expect(exportMemories).toHaveBeenCalledWith('/tmp/picked'));
    await waitFor(() => expect(screen.getByTestId('memory-report').textContent).toContain('Exported to /tmp/picked'));
  });
});
