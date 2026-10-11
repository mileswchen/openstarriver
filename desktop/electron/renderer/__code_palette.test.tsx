/// <reference types="vitest/globals" />
import { describe, it, expect, beforeEach } from 'vitest';
import {
  applyCodeTokens, currentTokenValue, loadCodeTokens, resetCodeTokens, setCodeToken,
  DEFAULT_LIGHT, DEFAULT_DARK,
} from './contrib/codePalette';

const styleEl = () => document.getElementById('starriver-code-tokens');

beforeEach(() => {
  localStorage.clear();
  styleEl()?.remove();
  delete document.documentElement.dataset.theme;
});

describe('code palette overrides (settings → 代码高亮配色)', () => {
  it('light overrides scope to html:not([data-theme="dark"])', () => {
    setCodeToken('light', 'string', '#123456');
    const css = styleEl()!.textContent!;
    expect(css).toContain('html:not([data-theme="dark"])');
    expect(css).toContain('--code-string: #123456');
// Tokens without overrides stay out of the injection (CSS defaults win)
    expect(css).not.toContain('--code-keyword');
  });

  it('dark overrides scope to html[data-theme="dark"]', () => {
    setCodeToken('dark', 'keyword', '#010203');
    const css = styleEl()!.textContent!;
    expect(css).toContain('html[data-theme="dark"]');
    expect(css).toContain('--code-keyword: #010203');
    expect(css).not.toContain('--code-comment');
  });

  it('persists to localStorage and survives applyCodeTokens re-reads', () => {
    setCodeToken('light', 'string', '#123456');
styleEl()!.remove(); // simulate an app restart
    applyCodeTokens();
    expect(loadCodeTokens().light.string).toBe('#123456');
    expect(styleEl()!.textContent).toContain('--code-string: #123456');
  });

  it('setting the default value removes the override (↺ semantics)', () => {
    setCodeToken('light', 'string', '#123456');
    setCodeToken('light', 'string', DEFAULT_LIGHT.string);
    expect(loadCodeTokens().light.string).toBeUndefined();
    expect(styleEl()!.textContent ?? '').not.toContain('--code-string');
  });

  it('ignores non-hex values', () => {
    setCodeToken('light', 'string', 'red');
    expect(loadCodeTokens().light.string).toBeUndefined();
  });

  it('resetCodeTokens clears one group or everything', () => {
    setCodeToken('light', 'string', '#123456');
    setCodeToken('dark', 'keyword', '#010203');
    resetCodeTokens('light');
    expect(loadCodeTokens().light.string).toBeUndefined();
    expect(loadCodeTokens().dark.keyword).toBe('#010203');
    resetCodeTokens();
    expect(loadCodeTokens().dark.keyword).toBeUndefined();
    expect(styleEl()!.textContent).toBe('');
  });

  it('currentTokenValue falls back to defaults', () => {
    expect(currentTokenValue('light', 'comment')).toBe(DEFAULT_LIGHT.comment);
    expect(currentTokenValue('dark', 'comment')).toBe(DEFAULT_DARK.comment);
    setCodeToken('light', 'comment', '#111111');
    expect(currentTokenValue('light', 'comment')).toBe('#111111');
  });
});
