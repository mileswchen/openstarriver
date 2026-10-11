// FPM P2: the contrib registry, theme loader, and the "no inline hex colors in new code" lint invariant.
// Old app.tsx / styles.css are exempt (funneling them to tokens is a backlog item).

import { readdirSync, readFileSync, statSync } from 'node:fs';
import { join } from 'node:path';
import { describe, expect, it } from 'vitest';
// Load built-in tab contributions (module-level registerTab; bootstrap skips automatically when #root is absent).
import './app';
import {
  getTab,
  listTabs,
  registerTab,
  registerTheme,
  unregisterTab,
} from './contrib/registry';
import { applyTheme, availableThemes } from './contrib/theme';

const rendererDir = join(__dirname);

describe('contribution registry', () => {
  it('registers, lists, and unregisters tabs', () => {
    registerTab({
      id: 'test-tab',
      labelKey: 'nav.test',
      icon: '<svg/>',
      render: () => null,
    });
    expect(getTab('test-tab')).toBeDefined();
    expect(listTabs().some((t) => t.id === 'test-tab')).toBe(true);
    unregisterTab('test-tab');
    expect(getTab('test-tab')).toBeUndefined();
  });

  it('built-in tabs are registered (shell is registry-driven)', () => {
    const ids = listTabs().map((t) => t.id);
    // 'chat' retired: New Task's Chat mode covers it (same Chat Agent).
    for (const id of ['coding', 'skills', 'plugins', 'models', 'mcp', 'scheduled']) {
      expect(ids).toContain(id);
    }
  });
});

describe('theme loader', () => {
  it('applies and clears a theme style element', () => {
    document.getElementById('starriver-theme')?.remove();
    registerTheme({ name: 'dark', css: ':root { --bg: #101010; }' });
    applyTheme('dark');
    const el = document.getElementById('starriver-theme') as HTMLStyleElement;
    expect(el).not.toBeNull();
    expect(el.textContent).toContain('--bg');
    expect(availableThemes()).toContain('dark');
    applyTheme(null);
    expect((document.getElementById('starriver-theme') as HTMLStyleElement).textContent).toBe('');
  });
});

describe('new-code theming lint', () => {
  function walk(dir: string, out: string[]): void {
    for (const entry of readdirSync(dir)) {
      const path = join(dir, entry);
      if (statSync(path).isDirectory()) walk(path, out);
      else if (entry.endsWith('.ts') || entry.endsWith('.tsx')) out.push(path);
    }
  }

  it('client/, contrib/ and app.tsx contain no inline hex colors', () => {
    const files: string[] = [];
    walk(join(rendererDir, 'client'), files);
    walk(join(rendererDir, 'contrib'), files);
    files.push(join(rendererDir, 'app.tsx'));
    const offenders = files
      .map((path) => ({ path, source: readFileSync(path, 'utf8') }))
      .filter(({ source }) => /#[0-9a-fA-F]{3,6}\b/.test(source));
    expect(offenders.map((o) => o.path)).toEqual([]);
  });
});
