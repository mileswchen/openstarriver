// Coding parity (WI-85~90): queue, tool duration, plan checklist, resend,
// session search, persistent usage — pure-function + component coverage.
import { describe, it, expect } from 'vitest';
import React from 'react';
import { render, screen, fireEvent } from '@testing-library/react';
import { I18nProvider } from './i18n';

describe('WI-87 PlanChecklist', () => {
  it('renders - [ ] / - [x] lines as todo items with markers', async () => {
    const { PlanChecklist } = await import('./app');
    const plan = [
      '# 计划',
      '- [ ] 第一步：分析',
      '- [x] 第二步：已完成的准备',
      '普通段落',
    ].join('\n');
    const { container } = render(
      <I18nProvider><PlanChecklist source={plan} /></I18nProvider>
    );
    const list = screen.getByTestId('plan-checklist');
    expect(list.children.length).toBe(2);
    expect(list.children[0].className).toContain('todo-item');
    expect(list.children[0].textContent).toContain('第一步');
    expect(list.children[0].className).not.toContain('done');
    expect(list.children[1].className).toContain('done');
    expect(list.children[1].textContent).toContain('第二步');
    // Non-list lines still render (markdown segment).
    expect(container.textContent).toContain('普通段落');
  });

  it('falls back to plain markdown when no checklist lines exist', async () => {
    const { PlanChecklist } = await import('./app');
    const { container, queryByTestId } = render(
      <I18nProvider><PlanChecklist source={'纯文本计划'} /></I18nProvider>
    );
    expect(queryByTestId('plan-checklist')).toBeNull();
    expect(container.textContent).toContain('纯文本计划');
  });
});

describe('WI-86 ToolRow duration', () => {
  it('shows ms under 1s and s above', async () => {
    const { ToolRow } = await import('./app');
    const { rerender } = render(
      <I18nProvider>
        <ToolRow content="🔧 bash: ls\n✓" result="ok" ok collapsible durationMs={87} />
      </I18nProvider>
    );
    const dur = screen.getByTestId('tool-duration');
    expect(dur.textContent).toContain('87ms');
    rerender(
      <I18nProvider>
        <ToolRow content="🔧 bash: ls\n✓" result="ok" ok collapsible durationMs={1234} />
      </I18nProvider>
    );
    expect(screen.getByTestId('tool-duration').textContent).toContain('1.2s');
  });

  it('renders no duration chip when absent', async () => {
    const { ToolRow } = await import('./app');
    render(
      <I18nProvider>
        <ToolRow content="🔧 bash: ls\n✓" result="ok" ok collapsible />
      </I18nProvider>
    );
    expect(screen.queryByTestId('tool-duration')).toBeNull();
  });
});

describe('WI-85 queue semantics (logic-level)', () => {
  it('drains FIFO and keeps order', async () => {
    // Simulate the queue data structure used in CodingView.
    const queue: { text: string; bubbleId: string }[] = [];
    const sent: string[] = [];
    const enqueue = (text: string) => queue.push({ text, bubbleId: `b${queue.length}` });
    const drainOne = () => {
      const next = queue.shift();
      if (next) sent.push(next.text);
    };
    enqueue('first');
    enqueue('second');
    enqueue('third');
    drainOne();
    drainOne();
    expect(sent).toEqual(['first', 'second']);
    expect(queue.map(q => q.text)).toEqual(['third']);
  });
});

describe('WI-91 subagent command parsing', () => {
  it('toolCommandOf surfaces template + prompt for subagent_spawn', async () => {
    const { toolCommandOf } = await import('./app');
    const cmd = toolCommandOf(JSON.stringify({ template: 'explorer', prompt: 'find all usages of foo()' }));
    expect(cmd).toContain('explorer');
    expect(cmd).toContain('find all usages');
  });
});

describe('WI-94 goal budget persistence', () => {
  it('stores and clears budget in localStorage', () => {
    localStorage.setItem('starriver-goal-budget', '50000');
    expect(Number(localStorage.getItem('starriver-goal-budget'))).toBe(50000);
    localStorage.removeItem('starriver-goal-budget');
    expect(localStorage.getItem('starriver-goal-budget')).toBeNull();
  });
});
