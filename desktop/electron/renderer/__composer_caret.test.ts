/// <reference types="vitest/globals" />
// Regression guard for the composer caret: the model / reasoning selects in
// the Chat & Coding input rows must show a dropdown caret styled like the
// approval-mode button's angle bracket (drawn strokes, not a filled shape).
//
// History: the first attempt drew the caret as an svg data-URI background,
// which Chromium/Electron silently drops on <select> inside the composer flex
// row (renders fine on a bare page). The second attempt used two
// linear-gradient halves — visible, but they form a SOLID triangle, not the
// hollow chevron the design uses elsewhere. A <select> cannot contain child
// elements, so the final structure is a wrapper span carrying the caret span
// (same border-rotation trick as .composer-approval-caret).
import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

// fileURLToPath, not a bare 'file://' strip: on Windows the naive replace
// leaves '/D:/...' and join() produces 'D:\D:\...' which ENOENTs.
const dir = dirname(fileURLToPath(import.meta.url));
const css = readFileSync(join(dir, 'styles.css'), 'utf8');
const app = readFileSync(join(dir, 'app.tsx'), 'utf8');

function ruleBlock(selector: string): string {
  // Match the RULE ("selector { ... }"), not prose mentions of the class in
  // comments (indexOf would hit those and then swallow the next rule's body).
  const m = css.match(new RegExp(selector.replace('.', String.raw`\.`) + String.raw`\s*\{([^}]*)\}`));
  expect(m, `styles.css must contain a ${selector} rule`).not.toBeNull();
  return m![1];
}

describe('composer mini-select caret', () => {
  const selectRule = ruleBlock('.composer-mini-select');
  const caretRule = ruleBlock('.composer-mini-caret');
  const wrapRule = ruleBlock('.composer-mini-wrap');
  const approvalCaretRule = ruleBlock('.composer-approval-caret');

  it('caret is the same border-rotated chevron as the approval-mode caret', () => {
    // Stroke geometry must match .composer-approval-caret: 8x8 box, 1.5px
    // right+bottom borders rotated 45deg, ~0.7 opacity. Only placement
    // (absolute, centered) and color (theme token) may differ.
    for (const part of ['width: 8px', 'height: 8px', 'border-right: 1.5px solid', 'border-bottom: 1.5px solid', 'rotate(45deg)']) {
      expect(caretRule).toContain(part);
      expect(approvalCaretRule).toContain(part);
    }
    expect(caretRule).toContain('opacity: 0.7');
    // Hollow, not filled: the chevron must NOT paint any background.
    expect(caretRule).not.toContain('background');
  });

  it('never falls back to background-image carets (svg or gradient) on the select', () => {
    // Both proven-wrong mechanisms: url(svg) is dropped by Electron on
    // <select> in the composer row; gradients render as a solid triangle.
    expect(selectRule).not.toMatch(/background-image:\s*url\(/);
    expect(selectRule).not.toContain('linear-gradient');
    expect(selectRule).toContain('appearance: none');
    expect(selectRule).toContain('-webkit-appearance: none');
  });

  it('wrapper owns sizing: caret zone reserved, empty-option collapse floored', () => {
    // padding-right must clear the absolutely-positioned caret (right 4px + 8px).
    const pad = selectRule.match(/padding:\s*0\s+(\d+)px\s+0\s+(\d+)px/);
    expect(pad, 'select padding shorthand present').not.toBeNull();
    expect(Number(pad![1])).toBeGreaterThanOrEqual(16);
    // Fresh-HOME case: zero bound models collapses the intrinsic width; the
    // wrapper floor keeps the control (and caret) alive. max-width caps growth.
    expect(wrapRule).toContain('min-width: 44px');
    expect(wrapRule).toContain('max-width: 180px');
    expect(wrapRule).toContain('position: relative');
    // The caret must not eat clicks meant for the select.
    expect(caretRule).toContain('pointer-events: none');
  });

  it('app renders MiniSelect wrappers with caret spans for the model/reasoning selects', () => {
    // MiniSelect component: select + caret span inside a wrapper.
    expect(app).toContain('<select className="composer-mini-select" {...props} />');
    expect(app).toContain('className="composer-mini-caret"');
    // CodingView stages either agent: one MiniSelect pair whose labels follow
    // the New Task mode. The chat workspace picker is the shared custom
    // picker (folder icons + placeholder), passing its aria-label through.
    expect(app).toContain("aria-label={stagedChat ? 'chat-model' : 'coding-model'}");
    expect(app).toContain("aria-label={stagedChat ? 'chat-reasoning' : 'coding-reasoning'}");
    expect(app).toContain('ariaLabel="chat-workspace"');
  });
});
