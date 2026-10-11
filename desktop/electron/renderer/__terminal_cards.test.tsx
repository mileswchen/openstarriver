/// <reference types="vitest/globals" />
// Regression guard: terminal_* tool cards must resolve the shell icon and a
// friendly title (not "Terminal_start"), surface their args in the TEXT
// block, and a placeholder row ("🔧 terminal_start..." — args not arrived
// yet) must not render the tool name as an args block.
import { describe, it, expect } from 'vitest';
import React from 'react';
import { render } from '@testing-library/react';
import { ToolRow, toolCommandOf } from './app';
import { I18nProvider } from './i18n';

const w = (ui: React.ReactElement) => render(<I18nProvider>{ui}</I18nProvider>);

const TERMINAL_ICON_MARK = 'rect x="3" y="3" width="18" height="18" rx="3"';

describe('terminal tool cards (icon + args)', () => {
  it('placeholder row resolves shell icon + friendly title, no TEXT block', () => {
    const { container } = w(<ToolRow content="🔧 terminal_start..." collapsible />);
    // Icon: the disclosure-leading holds the terminal frame, not the generic
    // square (rect x=3.5).
    const icon = container.querySelector('.disclosure-icon')!;
    expect(icon.innerHTML).toContain(TERMINAL_ICON_MARK);
    expect(icon.innerHTML).not.toContain('rect x="3.5"');
    // Title: TOOL_TITLES hit — no "Terminal_start" fallback.
    expect(container.querySelector('.tool-row-title')!.textContent).toBe('Terminal Start');
    // Placeholder: no args block, and the summary does not repeat the name.
    expect(container.querySelector('.tool-row-summary-text')!.textContent).toBe('');
    expect(container.querySelector('pre.tool-row-code')).toBeNull();
  });

  it('args row shows the command with bash highlighting', () => {
    const { container } = w(
      <ToolRow content={'🔧 terminal_write: bun run dev\\n'} collapsible />,
    );
    expect(container.querySelector('.tool-row-title')!.textContent).toBe('Terminal Write');
    const block = container.querySelector('pre.tool-row-code')!;
    expect(block.dataset.lang).toBe('bash');
    expect(block.textContent).toContain('bun run dev');
  });

  it('toolCommandOf picks terminal params and falls back to the full object', () => {
    expect(toolCommandOf(JSON.stringify({ id: 't-1', input: 'ls -la\n' }))).toBe('ls -la\\n');
    expect(toolCommandOf(JSON.stringify({ shell: 'bash' }))).toBe('bash');
    // wait/read/kill/signal have no single command field → full args JSON.
    expect(toolCommandOf(JSON.stringify({ id: 't-1', timeoutMs: 60000 }))).toBe('{"id":"t-1","timeoutMs":60000}');
    expect(toolCommandOf(JSON.stringify({ id: 't-1' }))).toBe('{"id":"t-1"}');
    expect(toolCommandOf(JSON.stringify({ id: 't-1', signal: 'SIGINT' }))).toBe('{"id":"t-1","signal":"SIGINT"}');
    // Original keys behave unchanged.
    expect(toolCommandOf(JSON.stringify({ command: 'git status' }))).toBe('git status');
    expect(toolCommandOf(undefined)).toBe('');
  });
});
