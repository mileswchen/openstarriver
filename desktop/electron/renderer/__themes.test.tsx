// Built-in themes: fully registered, injection works, preference persists round-trip.

import { beforeEach, describe, expect, it } from 'vitest';
// Importing registers the four built-in themes (module-level registerTheme).
import './themes';
import { BUILTIN_THEMES } from './themes';
import { applyTheme, availableThemes, saveThemePreference, savedTheme } from './contrib/theme';

describe('builtin themes', () => {
  beforeEach(() => {
    localStorage.clear();
    applyTheme(null);
  });

  it('registers the four builtin themes', () => {
    for (const { name } of BUILTIN_THEMES) {
      expect(availableThemes()).toContain(name);
    }
    expect(BUILTIN_THEMES.map((t) => t.name)).toEqual(['green', 'light-blue', 'pink', 'dark', 'compact', 'serif']);
  });

  it('injects theme CSS into the style tag and clears on null', () => {
    applyTheme('dark');
    const css = (document.getElementById('starriver-theme') as HTMLStyleElement).textContent;
    expect(css).toContain('--bg:');
    expect(css).toContain('--scrollbar-thumb');
    applyTheme(null);
    expect((document.getElementById('starriver-theme') as HTMLStyleElement).textContent).toBe('');
  });

  it('persists and clears the theme preference', () => {
    expect(savedTheme()).toBeNull();
    saveThemePreference('serif');
    expect(savedTheme()).toBe('serif');
    saveThemePreference(null);
    expect(savedTheme()).toBeNull();
  });
});
