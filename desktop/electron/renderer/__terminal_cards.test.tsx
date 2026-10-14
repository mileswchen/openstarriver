/// <reference types="vitest/globals" />
// Regression guard: terminal_* tool cards render through the generic tool-row
// path (upstream WI-146 icon/title model). Terminal tools are NOT special-cased
// upstream: no friendly TOOL_TITLES entry, no `input`/`shell` field in
// toolCommandOf's summary chain — args come through the card label, and the
// placeholder row ("🔧 terminal_start..." — args not arrived yet) must not
// render an args block.
import { describe, it, expect } from 'vitest';
import React from 'react';
import { render } from '@testing-library/react';
import { ToolRow, toolCommandOf } from './app';
import { I18nProvider } from './i18n';

const w = (ui: React.ReactElement) => render(<I18nProvider>{ui}</I18nProvider>);

describe('terminal tool cards (icon + args)', () => {
  it('placeholder row renders a card header and no real args', () => {
    const { container } = w(<ToolRow content="🔧 terminal_start..." collapsible />);
    // Title/summary fall back to the raw capitalized tool name (upstream has
    // no TOOL_TITLES entry for terminal_*); the label doubles as the summary.
    expect(container.querySelector('.tool-row-title')!.textContent).toBe('Terminal_start...');
    expect(container.querySelector('.tool-row-summary-text')!.textContent).toBe('terminal_start...');
    // No bash block — placeholder carries no command (and terminal_* is not a
    // COMMAND_TOOL upstream).
    const block = container.querySelector('pre.tool-row-code');
    if (block) expect(block.dataset.lang).not.toBe('bash');
  });

  it('args row shows the command as plain text (terminal_* is not a COMMAND_TOOL)', () => {
    const { container } = w(
      <ToolRow content={'🔧 terminal_write: bun run dev\\n'} collapsible />,
    );
    const block = container.querySelector('pre.tool-row-code')!;
    expect(block.dataset.lang).toBe('text');
    expect(block.textContent).toContain('bun run dev');
  });

  it('toolCommandOf reads the known summary fields and flattens newlines', () => {
    expect(toolCommandOf(JSON.stringify({ command: 'git status' }))).toBe('git status');
    expect(toolCommandOf(JSON.stringify({ command: 'echo a\nb' }))).toBe('echo a\\nb');
    expect(toolCommandOf(JSON.stringify({ path: '/tmp/x' }))).toBe('/tmp/x');
    // terminal tool params (`input`, `shell`, `id`) are not summary fields
    // upstream → empty; the card label carries the args instead.
    expect(toolCommandOf(JSON.stringify({ id: 't-1', input: 'ls -la\n' }))).toBe('');
    expect(toolCommandOf(JSON.stringify({ shell: 'bash' }))).toBe('');
    expect(toolCommandOf(JSON.stringify({ id: 't-1', timeoutMs: 60000 }))).toBe('');
    expect(toolCommandOf(undefined)).toBe('');
  });
});
