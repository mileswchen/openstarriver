/// <reference types="vitest/globals" />
import { describe, expect, it } from 'vitest';
import { render } from '@testing-library/react';
import { I18nProvider } from './i18n';
import { ThinkingRow, MessageRow } from './app';

describe('ThinkingRow — Coding thinking rows', () => {
  it('renders one collapsible row per reasoning message, frozen to the first line', () => {
    const { container } = render(<I18nProvider><ThinkingRow m={{ id: 't1', role: 'thinking', content: '先看目录结构\n再写代码\n最后跑测试' }} /></I18nProvider>);
    const rows = container.querySelectorAll('.thinking-row-details');
    expect(rows.length).toBe(1);
    expect(container.querySelector('.thinking-row-summary')?.textContent).toContain('先看目录结构');
    expect(container.querySelector('.thinking-row-body')?.textContent).toContain('最后跑测试');
    expect((rows[0] as HTMLDetailsElement).open).toBe(false);
    expect(container.querySelector('.disclosure-icon svg')).toBeTruthy();
    expect(container.querySelector('.disclosure-chevron')).toBeTruthy();
    expect(container.querySelector('.thinking-row-title')?.textContent).toBe('Think');
    expect(container.querySelector('.thinking-row-sep')).toBeTruthy();
  });

  it('streams the summary at the last line with the running sweep state', () => {
    const { container } = render(<I18nProvider><ThinkingRow m={{ id: 't2', role: 'thinking', content: '第一行思考\n第二行思考', streaming: true }} /></I18nProvider>);
    expect(container.querySelector('.thinking-row-summary')?.textContent).toContain('第二行思考');
    expect(container.querySelector('summary')?.getAttribute('data-state')).toBe('running');
    expect(container.querySelector('.thinking-indicator')).toBeNull();
  });

  it('shows an ellipsis placeholder before any thinking text arrives', () => {
    const { container } = render(<I18nProvider><ThinkingRow m={{ id: 't3', role: 'thinking', content: '', streaming: true }} /></I18nProvider>);
    expect(container.querySelector('.thinking-row-summary')?.textContent).toBe('…');
  });
});

describe('MessageRow — ChatTab 消息路径换代(WI-51)', () => {
  it('thinking 消息渲染 ThinkingRow 单行折叠,旧式 🧠 展开气泡不复存在', () => {
    const { container } = render(<I18nProvider><MessageRow m={{ id: 'm1', role: 'thinking', content: '思考第一行\n思考第二行' }} /></I18nProvider>);
 // ChatTab renders through the shared ThinkingRow component (scheme):
    // details.thinking-row-details + brain icon + chevron morph slot.
    expect(container.querySelectorAll('.thinking-row-details').length).toBe(1);
    expect(container.querySelector('.thinking-row-summary')?.textContent).toContain('思考第一行');
    expect(container.querySelector('.thinking-row-body')?.textContent).toContain('思考第二行');
    expect(container.querySelector('.disclosure-icon svg')).toBeTruthy();
    expect(container.querySelector('.disclosure-chevron')).toBeTruthy();
    // The legacy open bubble (🧠 emoji summary, default-open details) is gone.
    expect(container.textContent).not.toContain('🧠');
    expect((container.querySelector('.thinking-row-details') as HTMLDetailsElement).open).toBe(false);
    // The bubble wrapper stays (chat message layout) — the unboxing is CSS.
    expect(container.querySelector('.message.thinking .bubble')).toBeTruthy();
  });
});
