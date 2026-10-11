/// <reference types="vitest/globals" />
// Model-catalog pure functions: capacity K/M parse and write-back, catalog row
// validation, and the custom-route id pattern.
import { describe, it, expect } from 'vitest';
import { parseCapacity, formatCapacity, validateCatalog, MODEL_ROUTE_PATTERN } from './app';

describe('parseCapacity / formatCapacity — K/M 容量词汇', () => {
  it('blank → undefined (unset)；纯数字原样', () => {
    expect(parseCapacity('')).toBeUndefined();
    expect(parseCapacity('  ')).toBeUndefined();
    expect(parseCapacity('131072')).toBe(131072);
  });

  it('K/M 后缀按 1000 进位（1M = 1000K，与模型容量惯用口径一致）', () => {
    expect(parseCapacity('256K')).toBe(256_000);
    expect(parseCapacity('256k')).toBe(256_000);
    expect(parseCapacity('1M')).toBe(1_000_000);
    expect(parseCapacity('1.5K')).toBe(1500);
  });

  it('无法读写的文本返回 NaN，由校验层拦截', () => {
    expect(parseCapacity('abc')).toBeNaN();
    expect(parseCapacity('12x')).toBeNaN();
  });

  it('formatCapacity 以最短形式回写并与 parseCapacity 往返一致', () => {
    expect(formatCapacity(256_000)).toBe('256K');
    expect(formatCapacity(1_000_000)).toBe('1M');
    expect(formatCapacity(131072)).toBe('131072'); // non-multiple-of-K stays verbatim
    expect(formatCapacity(-5)).toBe('-5');
    expect(formatCapacity(parseCapacity('256K')!)).toBe('256K');
  });
});

describe('validateCatalog — 目录行校验（按位置命名）', () => {
  it('通过合法目录', () => {
    expect(validateCatalog([
      { id: 'a', name: 'A', contextLength: 131072 },
      { id: 'b', name: '' },
    ])).toBeUndefined();
  });

  it('空 ID / 重复 ID / 非正容量分别返回对应行与键', () => {
    expect(validateCatalog([{ id: ' ', name: '' }])).toEqual({ index: 0, key: 'modelIdRequired' });
    expect(validateCatalog([{ id: 'a', name: '' }, { id: 'a', name: '' }]))
      .toEqual({ index: 1, key: 'modelIdDuplicate' });
    expect(validateCatalog([{ id: 'a', name: '', contextLength: 0 }]))
      .toEqual({ index: 0, key: 'modelContextInvalid' });
    expect(validateCatalog([{ id: 'a', name: '', contextLength: 1.5 }]))
      .toEqual({ index: 0, key: 'modelContextInvalid' });
  });

  it('ID 比较忽略首尾空白（粘贴残留不算重复）', () => {
    expect(validateCatalog([{ id: 'a', name: '' }, { id: ' a ', name: '' }]))
      .toEqual({ index: 1, key: 'modelIdDuplicate' });
  });
});

describe('MODEL_ROUTE_PATTERN — 自定义路由 ID', () => {
  it('接受小写字母开头、小写字母/数字/短横线组合', () => {
    expect(MODEL_ROUTE_PATTERN.test('acme-gateway')).toBe(true);
    expect(MODEL_ROUTE_PATTERN.test('a')).toBe(true);
    expect(MODEL_ROUTE_PATTERN.test('a1b2')).toBe(true);
  });

  it('拒绝数字开头、大写、下划线、首尾短横线', () => {
    expect(MODEL_ROUTE_PATTERN.test('1abc')).toBe(false);
    expect(MODEL_ROUTE_PATTERN.test('Bad_ID')).toBe(false);
    expect(MODEL_ROUTE_PATTERN.test('a_b')).toBe(false);
    expect(MODEL_ROUTE_PATTERN.test('-ab')).toBe(false);
    expect(MODEL_ROUTE_PATTERN.test('ab-')).toBe(false);
  });
});
