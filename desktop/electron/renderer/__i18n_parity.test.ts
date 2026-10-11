/// <reference types="vitest/globals" />
// i18n parity guard: en and zh dictionaries must carry exactly the same key
// paths, and no value may stay empty — a key missing on one side renders the
// raw key (or blanks) in that locale.
import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

function flatten(obj: Record<string, unknown>, prefix = ''): string[] {
  return Object.entries(obj).flatMap(([k, v]) =>
    v && typeof v === 'object'
      ? flatten(v as Record<string, unknown>, `${prefix}${k}.`)
      : [`${prefix}${k}`],
  );
}

const en = JSON.parse(readFileSync(resolve(process.cwd(), 'renderer/i18n/en.json'), 'utf8'));
const zh = JSON.parse(readFileSync(resolve(process.cwd(), 'renderer/i18n/zh.json'), 'utf8'));
const enKeys = new Set(flatten(en));
const zhKeys = new Set(flatten(zh));

describe('i18n en/zh parity', () => {
  it('both locales carry the exact same key set', () => {
    expect([...enKeys].filter(k => !zhKeys.has(k)), 'keys missing in zh').toEqual([]);
    expect([...zhKeys].filter(k => !enKeys.has(k)), 'keys missing in en').toEqual([]);
  });

  it('no empty values in either locale', () => {
    for (const locale of [en, zh]) {
      const empties = flatten(locale).filter(k => {
        const v = k.split('.').reduce<unknown>((acc, part) => (acc as Record<string, unknown>)?.[part], locale);
        return typeof v !== 'string' || v.trim() === '';
      });
      expect(empties, 'empty i18n values').toEqual([]);
    }
  });

  it('interpolation placeholders match across locales', () => {
    const ph = (s: string) => (s.match(/\{(\w+)\}/g) || []).sort().join(',');
    const mismatched: string[] = [];
    for (const k of enKeys) {
      const get = (o: unknown) => k.split('.').reduce<unknown>((acc, part) => (acc as Record<string, unknown>)?.[part], o);
      const e = get(en) as string, z = get(zh) as string;
      if (typeof e === 'string' && typeof z === 'string' && ph(e) !== ph(z)) mismatched.push(k);
    }
    expect(mismatched, 'placeholder mismatches').toEqual([]);
  });
});
