// Built-in theme registry. A theme = one CSS overlay (same ThemeContribution contract
// as plugin themes, applied via contrib/theme's <style id="starriver-theme">). The default
// theme (styles.css's classic cool-gray-blue tokens) is not listed — it IS "inject nothing".
//
// Customizable dimensions = every token styles.css :root exposes: colors, fonts (--font/--mono),
// shadows (--shadow-*), radii (--radius-*); arbitrary selector overrides may also be attached
// (compact demonstrates the density dimension). Display names go through i18n (settings.theme_<name>); the name here is the stable id.

import { registerTheme } from './contrib/registry';

const GREEN = `:root {
  --bg: #f0f7ee;
  --bg-soft: #e6f1e2;
  --sidebar-bg: #ebf5e8;
  --panel-bg: #f5faf3;
}`;

const DARK = `:root {
  --bg: #171921;
  --bg-soft: #1e212c;
  --surface: #232633;
  --sidebar-bg: #1b1e28;
  --panel-bg: #1e212c;
  --bg-inset: #1e212c;
  --border: #2c3040;
  --border-strong: #3a3f52;
  --text: #e6e8ef;
  --text-secondary: #a8aebf;
  --text-muted: #6d7488;
  --accent: #c9d1e0;
  --accent-hover: #e2e6ef;
  --accent-2: #8d96ad;
  --accent-soft: #2b3040;
  --accent-border: #3a3f52;
  --on-accent: #171921;
  --success: #4ade80;
  --success-soft: #14291c;
  --success-border: #1f4d33;
  --warning: #f5a623;
  --danger: #f87171;
  --danger-soft: #2f1518;
  --danger-diff: #4a1d21;
  --danger-border: #5b2529;
  --danger-bg: #1d1013;
  --warning-soft: #2b2313;
  --info: #7cb3f5;
  --info-soft: #14202e;
  --info-border: #24405c;
  --bubble-user: #2b3040;
  --thinking-bg: #2a2318;
  --thinking-border: #4d3f1e;
  --danger-btn: #e05252;
  --danger-btn-hover: #c74545;
  --overlay: rgba(0, 0, 0, 0.55);
  --overlay-strong: rgba(0, 0, 0, 0.6);
  --shadow-sm: 0 1px 2px rgba(0, 0, 0, 0.4);
  --shadow-md: 0 4px 16px rgba(0, 0, 0, 0.45);
  --shadow-lift: 0 6px 20px rgba(0, 0, 0, 0.35);
  --shadow-accent: 0 2px 6px rgba(0, 0, 0, 0.5);
  --scrollbar-thumb: #3a3f52;
  --scrollbar-thumb-hover: #4a5168;
  /* Code syntax — Xcode Default (Dark) palette (first draft, tuned by eye; docs:
     docs/superpowers/specs/2026-10-04-xcode-code-highlight-design.md) */
  --code-comment: #6C7986;
  --code-keyword: #FC5FA3;
  --code-string: #FC6A5D;
  --code-number: #D0BF69;
  --code-link: #6CB6FF;
  --code-meta: #FD8F3F;
  --code-type: #5DD8FF;
  --code-member: #4FB4FF;
  --code-other-type: #BF8FFF;
  --diff-add: #4ade80;
  --diff-del: #f87171;
}`;

const COMPACT = `:root {
  --radius-xs: 3px;
  --radius-sm: 4px;
  --radius-md: 6px;
  --radius-lg: 8px;
}
body { font-size: 13px; }
.nav-item { padding: 5px 10px; font-size: 12.5px; }
.message-list { padding: 12px 16px; }
.message-list.rail-inset { padding-right: 64px; }
.composer-wrap { padding: 2px 16px 10px; }
.composer-wrap.rail-inset { padding-left: 64px; padding-right: 64px; }
.input-bar.stacked textarea { min-height: 40px; }
`;

// Light blue / pink: the same lightweight approach as GREEN — only surface-family tokens are overridden; everything else inherits the defaults.
const LIGHT_BLUE = `:root {
  --bg: #eef3fa;
  --bg-soft: #e4edf7;
  --sidebar-bg: #e8f0f9;
  --panel-bg: #f5f9fd;
  --accent-soft: #e2ecf8;
  --border: #d7e2f0;
  --border-strong: #c2d1e5;
}`;

const PINK = `:root {
  --bg: #fbf0f4;
  --bg-soft: #f7e6ed;
  --sidebar-bg: #f9eaf0;
  --panel-bg: #fdf3f7;
  --accent-soft: #f7e6ee;
  --border: #f0dbe4;
  --border-strong: #e3c8d4;
}`;

const SERIF = `:root {
  --font: "Songti SC", Georgia, "Times New Roman", "Noto Serif CJK SC", serif;
}`;

export const BUILTIN_THEMES: Array<{ name: string; css: string }> = [
  { name: 'green', css: GREEN },
  { name: 'light-blue', css: LIGHT_BLUE },
  { name: 'pink', css: PINK },
  { name: 'dark', css: DARK },
  { name: 'compact', css: COMPACT },
  { name: 'serif', css: SERIF },
];

for (const theme of BUILTIN_THEMES) {
  registerTheme(theme);
}
