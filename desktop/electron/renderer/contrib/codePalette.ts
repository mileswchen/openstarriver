// User-tunable code-highlight palette (WI-177 follow-up, 2026-10-04 scheme:
// docs/superpowers/specs/2026-10-04-code-palette-config-design.md).
// The --code-* tokens consumed by styles.css are ordinary CSS custom properties,
// so a palette = one injected <style> with scoped overrides. Light overrides are
// scoped to html:not([data-theme="dark"]), dark overrides to html[data-theme="dark"]
// (the attribute is set by contrib/theme.applyTheme) — the attribute selector also
// out-specifies the theme's own :root rules, so a dark override wins over the DARK
// theme regardless of <style> order. v1: one user override set; built-in 'dark'
// theme drives the dark group (plugin themes keep the light group).
// Default values live in ../codePaletteDefaults (contrib/ is linted hex-free).

import { DEFAULT_DARK, DEFAULT_LIGHT } from '../codePaletteDefaults';

export type { CodeTokenKey, CodePaletteMode, CodePalette } from '../codePaletteDefaults';
export { DEFAULT_DARK, DEFAULT_LIGHT };

const DEFAULTS: Record<CodePaletteMode, Record<CodeTokenKey, string>> = {
  light: DEFAULT_LIGHT,
  dark: DEFAULT_DARK,
};

const VAR: Record<CodeTokenKey, string> = {
  comment: '--code-comment',
  keyword: '--code-keyword',
  string: '--code-string',
  number: '--code-number',
  link: '--code-link',
  meta: '--code-meta',
  type: '--code-type',
  member: '--code-member',
  'other-type': '--code-other-type',
  'diff-add': '--diff-add',
  'diff-del': '--diff-del',
};

const STORE_KEY = 'starriver-code-tokens';
const STYLE_ID = 'starriver-code-tokens';
const LIGHT_SELECTOR = 'html:not([data-theme="dark"])';
const DARK_SELECTOR = 'html[data-theme="dark"]';
const HEX = /^#[0-9a-fA-F]{6}$/;
const KEYS = Object.keys(VAR) as CodeTokenKey[];

/** Stored overrides only (invalid/unknown entries dropped); {} when untouched. */
export function loadCodeTokens(): { light: CodePalette; dark: CodePalette } {
  try {
    const raw = JSON.parse(localStorage.getItem(STORE_KEY) || '{}') as { light?: CodePalette; dark?: CodePalette };
    const pick = (p?: CodePalette): CodePalette => {
      const out: CodePalette = {};
      for (const k of KEYS) {
        const v = p?.[k];
        if (typeof v === 'string' && HEX.test(v)) out[k] = v;
      }
      return out;
    };
    return { light: pick(raw.light), dark: pick(raw.dark) };
  } catch {
    return { light: {}, dark: {} };
  }
}

function saveCodeTokens(tokens: { light: CodePalette; dark: CodePalette }): void {
  try {
    if (Object.keys(tokens.light).length || Object.keys(tokens.dark).length) {
      localStorage.setItem(STORE_KEY, JSON.stringify(tokens));
    } else {
      localStorage.removeItem(STORE_KEY);
    }
  } catch {
    /* storage unavailable — overrides stay session-only via the injected style */
  }
}

/** Effective value shown by the settings swatch: override ?? default. */
export function currentTokenValue(mode: CodePaletteMode, key: CodeTokenKey): string {
  return loadCodeTokens()[mode][key] ?? DEFAULTS[mode][key];
}

/** Set one token; setting the default value removes the override (↺ semantics).
 *  Non-#rrggbb values are ignored. Persists + re-applies. */
export function setCodeToken(mode: CodePaletteMode, key: CodeTokenKey, value: string): void {
  if (!HEX.test(value)) return;
  const tokens = loadCodeTokens();
  if (value.toLowerCase() === DEFAULTS[mode][key].toLowerCase()) delete tokens[mode][key];
  else tokens[mode][key] = value;
  saveCodeTokens(tokens);
  applyCodeTokens(tokens);
}

/** Clear one group ('light'/'dark') or everything; persists + re-applies. */
export function resetCodeTokens(mode?: CodePaletteMode): void {
  const tokens = loadCodeTokens();
  if (mode) tokens[mode] = {};
  else {
    tokens.light = {};
    tokens.dark = {};
  }
  saveCodeTokens(tokens);
  applyCodeTokens(tokens);
}

function cssBlock(selector: string, mode: CodePaletteMode, palette: CodePalette): string {
  const lines = KEYS.filter((k) => palette[k]).map((k) => `  ${VAR[k]}: ${palette[k]};`);
  return `${selector} {\n${lines.join('\n')}\n}`;
}

/** (Re)inject the override <style>. Idempotent; the element is re-appended so it
 *  stays last in <head> even after theme styles are created later. */
export function applyCodeTokens(tokens = loadCodeTokens()): void {
  let el = document.getElementById(STYLE_ID) as HTMLStyleElement | null;
  if (!el) {
    el = document.createElement('style');
    el.id = STYLE_ID;
  }
  const parts: string[] = [];
  if (Object.keys(tokens.light).length) parts.push(cssBlock(LIGHT_SELECTOR, 'light', tokens.light));
  if (Object.keys(tokens.dark).length) parts.push(cssBlock(DARK_SELECTOR, 'dark', tokens.dark));
  el.textContent = parts.length ? parts.join('\n') : '';
  document.head.appendChild(el);
}
