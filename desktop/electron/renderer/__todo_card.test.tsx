/// <reference types="vitest/globals" />
import { describe, it, expect, vi, beforeEach } from 'vitest';
import React from 'react';
import { render, screen } from '@testing-library/react';
import { I18nProvider } from './i18n';

declare global { interface Window { starriver: any; } }

beforeEach(() => {
  Element.prototype.scrollIntoView = vi.fn();
  localStorage.setItem('starriver-language', 'en');
  window.starriver = new Proxy({}, { get: () => vi.fn().mockResolvedValue(null) });
});

describe('parseTodoState — 从工具输出解析 todo 状态', () => {
  it('解析原生 todo_write 的多行渲染', async () => {
    const { parseTodoState } = await import('./app');
    const todo = parseTodoState(
      '## TODO\n- [x] read code\n- [-] write tests (in progress)\n- [ ] ship it — v1',
    );
    expect(todo).not.toBeNull();
    expect(todo!.items).toEqual([
      { done: true, inProgress: false, title: 'read code' },
      { done: false, inProgress: true, title: 'write tests (in progress)' },
      { done: false, inProgress: false, title: 'ship it — v1' },
    ]);
  });

  it('解析 run_code 子调用日志的压扁单行形态', async () => {
    const { parseTodoState } = await import('./app');
    const flattened =
      '[2/2] ok todo: todo_write: ## TODO - [x] Create chess engine - [x] Implement AI opponent - [ ] Run validation';
    const todo = parseTodoState(flattened);
    expect(todo).not.toBeNull();
    expect(todo!.items.map((item) => item.title)).toEqual([
      'Create chess engine',
      'Implement AI opponent',
      'Run validation',
    ]);
    expect(todo!.items.filter((item) => item.done).length).toBe(2);
  });

  it('非 todo 文本返回 null', async () => {
    const { parseTodoState } = await import('./app');
    expect(parseTodoState('plain tool output')).toBeNull();
    expect(parseTodoState(null)).toBeNull();
  });
});

describe('TodoCard — 会话 todo 卡片', () => {
  it('渲染进度计数与单项状态标记', async () => {
    const { TodoCard } = await import('./app');
    render(
      <I18nProvider>
        <TodoCard
          todo={{
            items: [
              { done: true, inProgress: false, title: 'Create chess engine' },
              { done: false, inProgress: true, title: 'Run validation' },
            ],
          }}
        />
      </I18nProvider>,
    );
    expect(screen.getByText('Todos 1/2')).toBeTruthy();
    expect(screen.getByText('Create chess engine')).toBeTruthy();
    expect(screen.getByText('Run validation')).toBeTruthy();
    expect(document.querySelectorAll('.todo-item.done').length).toBe(1);
    expect(document.querySelectorAll('.todo-item.in-progress').length).toBe(1);
  });
});

// ── BUG-83 regression (round 6) ──
describe('parseTodoState uppercase checkbox', () => {
  it('accepts [X] as done without swallowing the item', async () => {
    const { parseTodoState } = await import('./app');
    const parsed = parseTodoState('## TODO\n- [x] First\n- [X] Second\n- [ ] Third\n');
    expect(parsed).not.toBeNull();
    expect(parsed!.items.length).toBe(3);
    expect(parsed!.items[1]).toMatchObject({ done: true, title: 'Second' });
    expect(parsed!.items[2]).toMatchObject({ done: false, title: 'Third' });
  });
});
