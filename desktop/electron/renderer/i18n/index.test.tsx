/// <reference types="vitest/globals" />
import { describe, it, expect, beforeEach, afterEach } from 'vitest';
import React from 'react';
import { render, screen, act } from '@testing-library/react';
import { I18nProvider, useI18n } from './index';
import enData from './en.json';
import zhData from './zh.json';

// ── Pure function tests (no React) ──

// Re-export for testing — resolveKey is private, test via t() with known dict
function resolveForTest(key: string): string {
  const parts = key.split('.');
  let current: any = enData;
  for (const p of parts) {
    if (current == null) return key;
    current = current[p];
  }
  return typeof current === 'string' ? current : key;
}

describe('resolveKey (pure unit)', () => {
  it('resolves a simple nav key', () => {
    expect(resolveForTest('nav.chat')).toBe('Chat');
  });
  it('resolves a nested settings key', () => {
    expect(resolveForTest('settings.language')).toBe('Language');
  });
  it('returns raw key for missing path', () => {
    expect(resolveForTest('nonexistent.key.path')).toBe('nonexistent.key.path');
  });
  it('handles empty string', () => {
    expect(resolveForTest('')).toBe('');
  });
  it('returns key when intermediate is not an object', () => {
    expect(resolveForTest('nav.chat.extra')).toBe('nav.chat.extra');
  });
});

// ── resolveKey (tested indirectly through t()) ──

function Tester() {
  const { t } = useI18n();
  return React.createElement('div', { 'data-testid': 'out' }, t('nav.chat'));
}

describe('I18nProvider', () => {
  beforeEach(() => {
    localStorage.clear();
  });
  afterEach(() => {
    localStorage.clear();
  });

  it('renders children and resolves a key', () => {
    render(React.createElement(I18nProvider, {}, React.createElement(Tester)));
    expect(screen.getByTestId('out').textContent).toBe('Chat');
  });

  it('falls back to key string when key is missing', () => {
    function BadKey() {
      const { t } = useI18n();
      return React.createElement('span', { 'data-testid': 'out' }, t('nonexistent.deep.key'));
    }
    render(React.createElement(I18nProvider, {}, React.createElement(BadKey)));
    expect(screen.getByTestId('out').textContent).toBe('nonexistent.deep.key');
  });

  it('interpolates variables', () => {
    function VarTester() {
      const { t } = useI18n();
      return React.createElement('span', { 'data-testid': 'out' }, t('chat.debugEntries', { count: 5 }));
    }
    render(React.createElement(I18nProvider, {}, React.createElement(VarTester)));
    expect(screen.getByTestId('out').textContent).toBe('API Debug (5 entries)');
  });

  it('detects system language from navigator', () => {
    const original = Object.getOwnPropertyDescriptor(navigator, 'language');
    Object.defineProperty(navigator, 'language', { value: 'zh-CN', configurable: true });
    function ShowLang() {
      const { lang } = useI18n();
      return React.createElement('span', { 'data-testid': 'out' }, lang);
    }
    render(React.createElement(I18nProvider, {}, React.createElement(ShowLang)));
    expect(screen.getByTestId('out').textContent).toBe('zh');
    if (original) Object.defineProperty(navigator, 'language', original);
    else delete (navigator as any).language;
  });

  it('switches language and persists to localStorage', () => {
    function Switcher() {
      const { lang, setLang, t } = useI18n();
      return React.createElement('div', {},
        React.createElement('span', { 'data-testid': 'lang' }, lang),
        React.createElement('button', { 'data-testid': 'btn', onClick: () => setLang('zh') }, t('nav.chat')),
      );
    }
    const { getByTestId } = render(React.createElement(I18nProvider, {}, React.createElement(Switcher)));
    expect(getByTestId('lang').textContent).toBe('en');
    act(() => { getByTestId('btn').click(); });
    expect(getByTestId('lang').textContent).toBe('zh');
    expect(localStorage.getItem('starriver-language')).toBe('zh');
  });
});

describe('useI18n outside provider', () => {
  it('throws an error', () => {
    function Bad() {
      useI18n();
      return React.createElement('div');
    }
    expect(() => render(React.createElement(Bad))).toThrow('useI18n must be used inside I18nProvider');
  });
});

describe('resolveKey edge cases', () => {
  function Resolver({ lookupKey }: { lookupKey: string }) {
    const { t } = useI18n();
    return React.createElement('span', { 'data-testid': 'out' }, t(lookupKey));
  }

  it('handles top-level keys', () => {
    render(React.createElement(I18nProvider, {}, React.createElement(Resolver, { lookupKey: 'nav.chat' })));
    expect(screen.getByTestId('out').textContent).toBe('Chat');
  });

  it('handles deeply nested keys', () => {
    render(React.createElement(I18nProvider, {}, React.createElement(Resolver, { lookupKey: 'models.providerId' })));
    expect(screen.getByTestId('out').textContent).toBe('Provider ID');
  });

  it('handles empty key', () => {
    render(React.createElement(I18nProvider, {}, React.createElement(Resolver, { lookupKey: '' })));
    expect(screen.getByTestId('out').textContent).toBe('');
  });

  it('handles partial match gracefully', () => {
    render(React.createElement(I18nProvider, {}, React.createElement(Resolver, { lookupKey: 'nav' })));
    const text = screen.getByTestId('out').textContent;
    expect(text).toBe('nav');
  });
});

describe('project edit modal keys', () => {
  function resolveIn(dict: any, key: string): string {
    const parts = key.split('.');
    let cur: any = dict;
    for (const p of parts) {
      if (cur == null) return key;
      cur = cur[p];
    }
    return typeof cur === 'string' ? cur : key;
  }

  it('en resolves all new keys', () => {
    expect(resolveForTest('coding.editProject')).toBe('Edit project');
    expect(resolveForTest('coding.deleteProject')).toBe('Delete project');
    expect(resolveForTest('coding.confirmDeleteTitle')).toBe('Delete project?');
    expect(resolveForTest('coding.confirmDeleteBody')).toContain('{name}');
    expect(resolveForTest('coding.confirmDeleteBody')).toContain('{count}');
    expect(resolveForTest('coding.projectName')).toBe('Project name');
    expect(resolveForTest('coding.projectDir')).toBe('Project directory');
    expect(resolveForTest('coding.new')).toBe('New');
    expect(resolveForTest('coding.workingDir')).toBe('Working directory');
    expect(resolveForTest('common.create')).toBe('Create');
  });

  it('zh resolves all new keys', () => {
    expect(resolveIn(zhData, 'coding.editProject')).toBe('编辑项目');
    expect(resolveIn(zhData, 'coding.deleteProject')).toBe('删除项目');
    expect(resolveIn(zhData, 'coding.confirmDeleteTitle')).toBe('删除项目？');
    expect(resolveIn(zhData, 'coding.confirmDeleteBody')).toContain('{name}');
    expect(resolveIn(zhData, 'coding.confirmDeleteBody')).toContain('{count}');
    expect(resolveIn(zhData, 'coding.projectName')).toBe('项目名');
    expect(resolveIn(zhData, 'coding.projectDir')).toBe('项目目录');
    expect(resolveIn(zhData, 'coding.new')).toBe('新建');
    expect(resolveIn(zhData, 'coding.workingDir')).toBe('工作目录');
    expect(resolveIn(zhData, 'common.create')).toBe('创建');
  });
});
