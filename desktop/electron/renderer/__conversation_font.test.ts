// App typography regression guard. Two stacks, one boundary (2026-10-03 用户要求,
// superseding WI-145's mono conversation prose):
//
//  - --font is the app-wide reading face: English & digits in SF Pro
//    (system-ui leads — it resolves to San Francisco on macOS; "SF Pro Text"/
//    "SF Pro" name it where installed), Han falling through to PingFang SC
//    (Regular — prose elements carry no heavier font-weight). Applies to
//    message bubbles and every element that merely inherits, including inside
//    .coding-view. One stack for both UI languages: family lists fall back
//    per glyph, not per language (CDP probe 2026-10-03, lang=en ≡ lang=zh-CN).
//  - --mono stays the code/tool surface stack, 语言无关: JetBrains Mono
//    leads (用户指定), named SF Mono / Menlo on macOS, Cascadia/Consolas on
//    Windows, DejaVu Sans Mono on Linux, CJK falling through to the platform
//    CJK font.
//
// Pins (a) both stacks' entries and order, (b) the rule that puts --font on
// every message bubble, (c) that no blanket mono rule sweeps .coding-view *,
// and (d) that md code spans/blocks keep the mono stack inside prose.
import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const dir = dirname(fileURLToPath(import.meta.url));
const css = readFileSync(join(dir, 'styles.css'), 'utf8');

describe('app typography (2026-10-03 用户要求: SF Pro Latin + PingFang SC Han)', () => {
  it('--font leads with system-ui (the proven SF entry) and falls through to PingFang SC', () => {
    const font = css.match(/--font:\s*([^;]+);/)?.[1] ?? '';
    expect(font, '--font variable must exist').not.toBe('');
    // system-ui must LEAD: it resolves to San Francisco (SF Pro) on macOS and
    // carries no CJK, so 汉字 falls through to the next family. (Probe 2026-10-03:
    // results identical under lang=en and lang=zh-CN — no lang-scoped variants.)
    expect(font.trimStart().startsWith('system-ui')).toBe(true);
    // Named faces catch hosts with SF Pro installed; PingFang SC must sit AFTER
    // the Latin entries — PingFang ships its own Latin glyphs and would capture
    // English/digits/paths if it came first (中文界面同样如此).
    expect(font).toContain('"SF Pro Text"');
    expect(font).toContain('"SF Pro"');
    expect(font.indexOf('"PingFang SC"')).toBeGreaterThan(font.indexOf('"SF Pro"'));
    // -apple-system is a DEAD keyword in this Electron build (probe: alone it
    // renders everything, Latin included, in PingFang) — must not be listed.
    expect(font).not.toContain('-apple-system');
    // Generic sans-serif stays last as the final catch-all.
    expect(font.trimEnd().endsWith('sans-serif')).toBe(true);
  });

  it('message bubbles read in the app font stack', () => {
    expect(css).toMatch(/\.message \.bubble\s*\{[^}]*font-family:\s*var\(--font\)/);
  });

  it('no blanket mono rule sweeps the coding view', () => {
    // The old WI-145 blanket (.coding-view * { font-family: var(--mono) }) made
    // toolbar/composer/Chinese labels mono. It must stay withdrawn; code
    // surfaces pin --mono individually instead.
    expect(css).not.toMatch(/\.coding-view[^{]*\*[^{]*\{[^}]*font-family/);
  });

  it('code spans and blocks inside markdown prose keep the mono stack', () => {
    expect(css).toMatch(/\.md-content code\s*\{[^}]*font-family:\s*var\(--mono\)/);
    expect(css).toMatch(/\.md-content pre::before\s*\{[^}]*font-family:\s*var\(--mono\)/);
  });
});

describe('code/tool mono stack (语言无关, 2026-10-03 用户要求 JetBrains Mono 领衔)', () => {
  it('--mono leads with JetBrains Mono and carries mono-Latin fallbacks per platform', () => {
    const mono = css.match(/--mono:\s*([^;]+);/)?.[1] ?? '';
    expect(mono, '--mono variable must exist').not.toBe('');
    // 用户指定的首选;未安装时顺位到命名 "SF Mono"(本机装有 SF-Mono otf)。
    // ui-monospace 在当前 Electron 里是死关键字(探针实测),不能作为唯一
    // macOS 来源——命名 SF Mono 必须在它之前。
    expect(mono.trimStart().startsWith('"JetBrains Mono"')).toBe(true);
    expect(mono.indexOf('"SF Mono"')).toBeLessThan(mono.indexOf('ui-monospace'));
    expect(mono).toContain('"SFMono-Regular"');
    expect(mono).toContain('Menlo');
    // Windows: Cascadia ships with Terminal/VS, Consolas with the OS.
    expect(mono).toContain('"Cascadia Code"');
    expect(mono).toContain('"Cascadia Mono"');
    expect(mono).toContain('Consolas');
    // Linux: JetBrains Mono where installed, DejaVu Sans Mono nearly universal.
    expect(mono).toContain('"DejaVu Sans Mono"');
  });

  it('--mono falls through to CJK fonts after the Latin entries', () => {
    const mono = css.match(/--mono:\s*([^;]+);/)?.[1] ?? '';
    // The Latin mono fonts carry no CJK glyphs: the CJK entries must exist and
    // come AFTER the Latin ones so English takes the mono, Chinese the system
    // CJK face.
    for (const cjk of ['"PingFang SC"', '"Microsoft YaHei"', '"Noto Sans CJK SC"']) {
      expect(mono.indexOf(cjk)).toBeGreaterThan(mono.indexOf('"SFMono-Regular"'));
    }
    // Generic monospace stays last as the final catch-all.
    expect(mono.trimEnd().endsWith('monospace')).toBe(true);
  });
});
