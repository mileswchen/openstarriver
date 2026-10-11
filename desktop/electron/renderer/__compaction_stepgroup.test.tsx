/// <reference types="vitest/globals" />
import { describe, it, expect } from 'vitest';
import React from 'react';
import { render, screen, fireEvent } from '@testing-library/react';
import { I18nProvider } from './i18n';
import { CompactionRow } from './contrib/CompactionRow';
import { groupProcessRows } from './app';

describe('CompactionRow — compaction 折叠行', () => {
  const SUMMARY = [
    '## Primary Request and Intent',
    'Port the conversation components',
    '## Key Technical Concepts',
    '(none)',
  ].join('\n');

  it('渲染折叠行,展开后显示 checkpoint 摘要', () => {
    render(
      React.createElement(I18nProvider, null, React.createElement(CompactionRow, { summary: SUMMARY })),
    );
    const row = screen.getByTestId('compaction-row');
    expect(row).toBeTruthy();
    // Collapsed: the first-section preview is the title; the summary body is not in the DOM.
    expect(screen.getByText('Port the conversation components')).toBeTruthy();
    expect(screen.queryByText(/Key Technical Concepts/)).toBeNull();
    // Expanded: the disclosure shows the summary Markdown.
    fireEvent.click(screen.getByRole('button'));
    expect(screen.getByText(/Key Technical Concepts/)).toBeTruthy();
  });

  it('空摘要不可展开', () => {
    render(
      React.createElement(I18nProvider, null, React.createElement(CompactionRow, { summary: '  ' })),
    );
    const button = screen.getByRole('button') as HTMLButtonElement;
    expect(button.disabled).toBe(true);
  });
});

describe('groupProcessRows — 连续 tool/thinking 折叠成组', () => {
  const msg = (id: string, role: any, extra: Partial<any> = {}) =>
    ({ id, role, content: `${role}-${id}`, toolName: role === 'tool' ? id : undefined, ...extra });

  it('≥2 连续过程行成组,单独一行不成组', () => {
    const messages = [
      msg('u1', 'user'),
      msg('t1', 'thinking'),
      msg('tool1', 'tool'),
      msg('tool2', 'tool'),
      msg('a1', 'assistant'),
      msg('tool3', 'tool'),
    ];
    const segs = groupProcessRows(messages);
    expect(segs.map((s: any) => s.kind)).toEqual(['single', 'group', 'single', 'single']);
    const group = segs.find((s: any) => s.kind === 'group') as any;
    expect(group.rows.map((r: any) => r.id)).toEqual(['t1', 'tool1', 'tool2']);
  });

  it('compaction/user/assistant 都是过程组的边界', () => {
    const messages = [
      msg('tool1', 'tool'),
      msg('c1', 'compaction'),
      msg('tool2', 'tool'),
      msg('u1', 'user'),
      msg('tool3', 'tool'),
    ];
    const segs = groupProcessRows(messages);
    // tool1 single row; compaction standalone; tool2 single row; user standalone; tool3 single row.
    expect(segs.every((s: any) => s.kind === 'single')).toBe(true);
    expect(segs.length).toBe(5);
  });
});
