// Lightweight i18n: zero dependencies, React Context + static JSON imports.
// Usage: import { useI18n } from './i18n'; const { t } = useI18n();

import React, { createContext, useContext, useState, useCallback, useEffect, useMemo } from 'react';
import enData from './en.json';
import zhData from './zh.json';

type Lang = 'en' | 'zh';

interface I18nContextType {
  lang: Lang;
  setLang: (l: Lang) => void;
  t: (key: string, vars?: Record<string, string | number>) => string;
}

const I18nContext = createContext<I18nContextType | null>(null);
const LS_KEY = 'starriver-language';

const DICTS: Record<Lang, Record<string, any>> = { en: enData, zh: zhData };

function detectSystemLang(): Lang {
  try {
    const stored = localStorage.getItem(LS_KEY);
    if (stored === 'en' || stored === 'zh') return stored;
  } catch {}
  if (typeof navigator !== 'undefined') {
    const nav = navigator.language?.toLowerCase() || '';
    if (nav.startsWith('zh')) return 'zh';
  }
  return 'en';
}

function resolveKey(obj: any, key: string): string {
  if (!key || !obj) return key;
  const parts = key.split('.');
  let current = obj;
  for (const p of parts) {
    if (current == null) return key;
    current = current[p];
  }
  return typeof current === 'string' ? current : key;
}

export function I18nProvider({ children }: { children: React.ReactNode }) {
  const [lang, setLangRaw] = useState<Lang>(detectSystemLang);

// User request 2026-10-03: <html lang> follows the UI language (zh → zh-CN). Font resolution was
// probe-verified under Electron CDP to be lang-independent (see the styles.css --font comment); what is
// synced here is the semantic language tag — screen-reader pronunciation, hyphenation, and translation hints depend on it.
  useEffect(() => {
    document.documentElement.lang = lang === 'zh' ? 'zh-CN' : 'en';
  }, [lang]);

  const dict = DICTS[lang];

  const setLang = useCallback((l: Lang) => {
    setLangRaw(l);
    try { localStorage.setItem(LS_KEY, l); } catch {}
  }, []);

  const t = useCallback((key: string, vars?: Record<string, string | number>) => {
    let text = resolveKey(dict, key);
    if (vars) {
      for (const [k, v] of Object.entries(vars)) {
        // Function replacement: neutralizes `$&`/`$'` patterns in values,
        // and split+join replaces EVERY occurrence of the placeholder.
        text = text.split(`{${k}}`).join(String(v));
      }
    }
    return text;
  }, [dict]);

  const value = useMemo(() => ({ lang, setLang, t }), [lang, setLang, t]);

  return React.createElement(I18nContext.Provider, { value }, children);
}

export function useI18n(): I18nContextType {
  const ctx = useContext(I18nContext);
  if (!ctx) throw new Error('useI18n must be used inside I18nProvider');
  return ctx;
}

export type { Lang };
