/// <reference types="vitest/globals" />
import { describe, it, expect, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import React from 'react';
import { render, fireEvent, waitFor } from '@testing-library/react';
import { ToolRow, AssistantContent } from './app';
import { I18nProvider } from './i18n';

const w = (ui: React.ReactElement) => render(<I18nProvider>{ui}</I18nProvider>);

/** Xcode-palette code-block syntax highlighting (2026-10-04 design):
 *  the colors themselves are the styles.css --code-* → hljs-* CSS mapping; here we only verify the presence of
 *  hljs token spans in the DOM and the fallback paths (plain text, over-length skip, unknown language does not crash). */
describe('code block syntax highlighting (Xcode palette)', () => {
  it('highlights the numbered bash command block with hljs spans', () => {
    const { container } = w(<ToolRow content={'🔧 bash: echo "hi" # greet'} />);
    const block = container.querySelector('pre.tool-row-code.code-numbered')!;
    expect(block.querySelector('.hljs-string')).not.toBeNull();
    expect(block.querySelector('.hljs-comment')).not.toBeNull();
  });

  it('highlights markdown fenced blocks (keyword/number/comment)', () => {
    const md = '```js\nconst n = 42; // answer\n```';
    const { container } = w(<AssistantContent content={md} />);
    const code = container.querySelector('.md-content pre code')!;
    expect(code.querySelector('.hljs-keyword')).not.toBeNull();
    expect(code.querySelector('.hljs-number')).not.toBeNull();
    expect(code.querySelector('.hljs-comment')).not.toBeNull();
  });

  it('markdown fenced blocks carry a copy button that copies the exact code', async () => {
    // rehype-highlight splits the code into token spans — the chip must copy
    // the FLATTENED raw text, not a fragment.
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, 'clipboard', { value: { writeText }, configurable: true });
    const md = '```bash\ncd /x && git push origin feat/tool-env\n```';
    const { container } = w(<AssistantContent content={md} />);
    const wrap = container.querySelector('.md-pre-wrap')!;
    expect(wrap).not.toBeNull();
    const chip = wrap.querySelector('.copy-chip') as HTMLButtonElement;
    expect(chip).not.toBeNull();
    fireEvent.click(chip);
    await waitFor(() =>
      expect(writeText).toHaveBeenCalledWith('cd /x && git push origin feat/tool-env\n'),
    );
  });

  it('highlights whitelisted tool output (json keys → hljs-attr)', () => {
    const { container } = w(<ToolRow content={'🔧 read: pkg.json\n✓ {"name": "x", "n": 1}'} />);
    const pre = container.querySelector('pre.tool-row-code:not(.code-numbered)')!;
    expect(pre.dataset.lang).toBe('json');
    expect(pre.querySelector('.hljs-attr')).not.toBeNull();
  });

  it('skips highlighting for oversized bodies (>100k chars) — plain text', () => {
    const big = 'x'.repeat(100_001);
    const { container } = w(<ToolRow content={'🔧 read: big.xml\n✓ <a>' + big + '</a>'} />);
    const pre = container.querySelector('pre.tool-row-code:not(.code-numbered)')!;
    expect(pre.querySelector('[class*="hljs"]')).toBeNull();
    expect(pre.textContent).toContain('x');
  });

  it('leaves unknown fence languages plain without crashing', () => {
    const { container } = w(<AssistantContent content={'```whitespace\nfoo bar\n```'} />);
    const code = container.querySelector('.md-content pre code')!;
    expect(code.textContent).toContain('foo bar');
    expect(code.querySelector('[class*="hljs"]')).toBeNull();
  });

  it('highlights file-content tool results by path extension (read *.tsx)', () => {
    const { container } = w(
      <ToolRow content={'🔧 read: /x/app.tsx'} result={'const n = 42; // answer'} ok collapsible />,
    );
    const body = container.querySelector('.tool-row-result-body')!;
    expect(body.querySelector('.hljs-keyword')).not.toBeNull();
    expect(body.querySelector('.hljs-number')).not.toBeNull();
    expect(body.querySelector('.hljs-comment')).not.toBeNull();
  });

  it('keeps non-code tool results plain (read *.log → no language)', () => {
    const { container } = w(
      <ToolRow content={'🔧 read: build.log'} result={'Compiling x v0.1.0\n[exit code: 0]'} ok collapsible />,
    );
    const body = container.querySelector('.tool-row-result-body')!;
    expect(body.querySelector('[class*="hljs"]')).toBeNull();
    expect(body.textContent).toContain('[exit code: 0]');
  });

  it('highlights diff rows by the file extension (edit *.tsx)', () => {
    const diff = [
      '--- a/app.tsx',
      '+++ b/app.tsx',
      '@@ -1,2 +1,2 @@',
      ' const n = 1;',
      '-const old = 2;',
      '+const fresh: number = 3; // done',
    ].join('\n');
    const { container } = w(<ToolRow content={'🔧 edit: /x/app.tsx'} result={diff} ok collapsible />);
    const rows = Array.from(container.querySelectorAll('.diff-text'));
    expect(rows.length).toBeGreaterThanOrEqual(3);
    expect(rows.some((r) => r.querySelector('.hljs-keyword'))).toBe(true);
  });

  it('BUG-143: multi-line node -e string stays colored across lines (no re-tokenization)', () => {
    // Historical storage shape: multi-line commands are flattened into content with literal \n; CodeBlock restores them before rendering
    const { container } = w(
      <ToolRow content={'🔧 bash: node -e "const a=1;\\nconsole.log(a)"'} collapsible />,
    );
    const lines = Array.from(container.querySelectorAll('.code-text'));
    expect(lines.length).toBe(2);
    const s1 = lines[0].querySelector('.hljs-string');
    const s2 = lines[1].querySelector('.hljs-string');
    expect(s1?.textContent).toContain('const a=1;');
    // The continuation line is still the same string: console.log must not be re-split into built_in
    expect(s2?.textContent).toContain('console.log(a)');
    expect(lines[1].querySelector('.hljs-built_in')).toBeNull();
  });

  it('BUG-143 companion: bash heuristic wraps grep/node/cargo/git, skips flags/assignments/paths', () => {
    const { container } = w(
      <ToolRow
        content={'🔧 bash: grep -n theme app.tsx && node -e "x" | head; cargo build 2>/dev/null; PATH=/opt/bin:$PATH git push'}
        collapsible
      />,
    );
    const builtins = Array.from(container.querySelectorAll('.code-text .hljs-built_in')).map((s) => s.textContent);
    expect(builtins).toContain('grep');
    expect(builtins).toContain('node');
    expect(builtins).toContain('cargo');
    expect(builtins).toContain('git');
    expect(builtins).not.toContain('dev'); // 2>/dev/null 的 dev 不是命令
    expect(builtins).not.toContain('PATH'); // 行首赋值不着色
  });

  it('hljs 调色板作用域覆盖 .diff-text(会话 edit 块/Review/IDE 面板的 diff 行)', () => {
    // DiffView renders hljs token spans into .diff-text; if the styles.css coloring rules do not include
    // .diff-text in the :is(...) scope, the spans are colorless → diff rows degrade to plain text (2026-10-05).
    const css = readFileSync('renderer/styles.css', 'utf8'); // cwd = vitest 根(desktop/electron)
    const stale = css.match(/:is\(\.md-content pre code, \.tool-row-code\)(?!, \.diff-text)/g) || [];
    expect(stale).toEqual([]);
    expect((css.match(/:is\(\.md-content pre code, \.tool-row-code, \.diff-text\)/g) || []).length)
      .toBeGreaterThanOrEqual(12); // 11 条规则 12 个选择器行(member 规则双选择器)
  });
});
