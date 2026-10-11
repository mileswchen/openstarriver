// FPM P2 theme loader: injects theme contributions (CSS custom-property overrides) at runtime.
// Built-in theme = inject nothing (styles.css default tokens); plugin themes apply
// through registerTheme. New code must not inline hex colors (see __theme_lint.test.tsx).

import { getTheme, listThemes } from './registry';

const STYLE_ID = 'starriver-theme';

/** Apply the theme named `name`; `null` restores the built-in theme. */
export function applyTheme(name: string | null): void {
  let el = document.getElementById(STYLE_ID) as HTMLStyleElement | null;
  if (!el) {
    el = document.createElement('style');
    el.id = STYLE_ID;
    document.head.appendChild(el);
  }
  el.textContent = name ? (getTheme(name)?.css ?? '') : '';
  // Expose the active theme for scoped overrides (codePalette's dark group keys
  // off html[data-theme="dark"]); removing the attr returns to the light scope.
  if (name) document.documentElement.dataset.theme = name;
  else delete document.documentElement.dataset.theme;
}

/** Currently available theme names (built-in excluded). */
export function availableThemes(): string[] {
  return listThemes().map((theme) => theme.name);
}

// Theme preference persistence (localStorage, mirroring i18n's starriver-language key pattern).
const PREF_KEY = 'starriver-theme';

/** Last chosen theme name; `null` = nothing saved or the default was chosen. */
export function savedTheme(): string | null {
  try {
    return localStorage.getItem(PREF_KEY);
  } catch {
    return null;
  }
}

/** Remember the theme choice; `null` clears (back to the default theme). */
export function saveThemePreference(name: string | null): void {
  try {
    if (name) localStorage.setItem(PREF_KEY, name);
    else localStorage.removeItem(PREF_KEY);
  } catch {
    /* storage unavailable — theme still applies for this session */
  }
}
