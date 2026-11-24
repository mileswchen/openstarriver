/// <reference types="vitest/globals" />
// Scheduled UI Lock: pure logic (window decision / occurrence / passcode),
// settings section (immediate persistence / invalid blocking), lock Gate (cover-not-unmount / no-relock-on-unlock / relock-across-windows).
// Component tests use vitest fake timers to fully control the clock; the tick runs synchronously in the mount effect,
// so assertions can be made synchronously right after render/act returns.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import React from 'react';
import { act, fireEvent, render, screen } from '@testing-library/react';
import { I18nProvider } from './i18n';
import { UiLockGate, UiLockScheduleSection } from './app';
import {
  DEFAULT_UI_LOCK_SCHEDULE, LOCK_CODE_LENGTH, LOCK_TICK_MS, UI_LOCK_APPLY_EVENT, UI_LOCK_CHARSET,
  UI_LOCK_STORAGE_KEY, UI_LOCK_UNLOCKED_KEY,
  evaluateUiLockTick, generateLockCode, isInLockWindow, isValidTime,
  loadUiLockSchedule, lockOccurrenceId, saveUiLockSchedule,
  type UiLockSchedule,
} from './uiLock';

const store = (s: UiLockSchedule) => localStorage.setItem(UI_LOCK_STORAGE_KEY, JSON.stringify(s));
const readStore = (): UiLockSchedule =>
  JSON.parse(localStorage.getItem(UI_LOCK_STORAGE_KEY) || 'null');

beforeEach(() => {
  localStorage.removeItem(UI_LOCK_STORAGE_KEY);
});
afterEach(() => {
  vi.useRealTimers();
  localStorage.clear();
  vi.restoreAllMocks();
});

describe('uiLock 纯逻辑', () => {
  it('isValidTime / 默认日程', () => {
    expect(isValidTime('00:00')).toBe(true);
    expect(isValidTime('23:59')).toBe(true);
    expect(isValidTime('24:00')).toBe(false);
    expect(isValidTime('7:30')).toBe(false);
    expect(isValidTime('')).toBe(false);
    expect(DEFAULT_UI_LOCK_SCHEDULE.enabled).toBe(false);
  });

  it('isInLockWindow：常规窗口边界（含起点、不含终点）', () => {
    const d = (h: number, m: number) => new Date(2026, 2, 1, h, m);
    expect(isInLockWindow(d(9, 0), '09:00', '10:00')).toBe(true);
    expect(isInLockWindow(d(9, 59), '09:00', '10:00')).toBe(true);
    expect(isInLockWindow(d(10, 0), '09:00', '10:00')).toBe(false);
    expect(isInLockWindow(d(8, 59), '09:00', '10:00')).toBe(false);
  });

  it('isInLockWindow：跨午夜窗口', () => {
    const d = (h: number, m: number) => new Date(2026, 2, 1, h, m);
    expect(isInLockWindow(d(22, 0), '22:00', '06:00')).toBe(true);
    expect(isInLockWindow(d(23, 30), '22:00', '06:00')).toBe(true);
    expect(isInLockWindow(d(0, 0), '22:00', '06:00')).toBe(true);
    expect(isInLockWindow(d(5, 59), '22:00', '06:00')).toBe(true);
    expect(isInLockWindow(d(6, 0), '22:00', '06:00')).toBe(false);
    expect(isInLockWindow(d(12, 0), '22:00', '06:00')).toBe(false);
  });

  it('isInLockWindow：start===end 与非法时间永不在窗口（避免永锁）', () => {
    const d = new Date(2026, 2, 1, 12, 0);
    expect(isInLockWindow(d, '09:00', '09:00')).toBe(false);
    expect(isInLockWindow(d, 'bad', '10:00')).toBe(false);
  });

  it('lockOccurrenceId：跨午夜凌晨回溯到昨日起点（解锁记忆跨午夜稳定）', () => {
    const night = new Date(2026, 2, 1, 23, 0);
    const early = new Date(2026, 2, 2, 3, 0); // 次日凌晨，仍属 3 月 1 日开始的窗口
    expect(lockOccurrenceId(night, '22:00', '06:00')).toBe('2026-03-01');
    expect(lockOccurrenceId(early, '22:00', '06:00')).toBe('2026-03-01');
    expect(lockOccurrenceId(new Date(2026, 2, 2, 12, 0), '22:00', '06:00')).toBeNull();
    expect(lockOccurrenceId(new Date(2026, 2, 1, 9, 30), '09:00', '10:00')).toBe('2026-03-01');
  });

  it('generateLockCode：10 位、字符集限定、注入随机确定性', () => {
    const code = generateLockCode();
    expect(code).toHaveLength(LOCK_CODE_LENGTH);
    expect(code).toMatch(/^[A-Za-z0-9]{10}$/);

    // Charset index baseline: index 0→'A', 25→'Z', 52→'0', 61→'9'
    expect(UI_LOCK_CHARSET[0]).toBe('A');
    expect(UI_LOCK_CHARSET[25]).toBe('Z');
    expect(UI_LOCK_CHARSET[61]).toBe('9');

    // [rejection-sample value, 0, 25, 62, 5..10, 0] → 'A'+'Z'+(62%62=0→'A')+'F'..'K'+'A'
    const buf = new Uint32Array([0xFFFFFFFF, 0, 25, 62, 5, 6, 7, 8, 9, 10, 0, 0]);
    expect(generateLockCode(buf)).toBe('AZAFGHIJKA');

    // All sample values fall in the rejection range and the buffer is exhausted → explicit error (not an infinite loop)
    expect(() => generateLockCode(new Uint32Array(LOCK_CODE_LENGTH).fill(0xFFFFFFFF))).toThrow();
  });

  it('loadUiLockSchedule：坏数据回退默认；save/load 往返', () => {
    expect(loadUiLockSchedule()).toEqual(DEFAULT_UI_LOCK_SCHEDULE);
    localStorage.setItem(UI_LOCK_STORAGE_KEY, '{broken json');
    expect(loadUiLockSchedule()).toEqual(DEFAULT_UI_LOCK_SCHEDULE);
    localStorage.setItem(UI_LOCK_STORAGE_KEY, JSON.stringify({ enabled: 'yes', start: '09:00', end: '10:00' }));
    expect(loadUiLockSchedule().enabled).toBe(false); // 字段类型坏 → 默认
    const s: UiLockSchedule = { enabled: true, start: '01:00', end: '02:00' };
    saveUiLockSchedule(s);
    expect(loadUiLockSchedule()).toEqual(s);
  });

  it('evaluateUiLockTick：禁用/窗外/已解锁/新 occurrence', () => {
    const s: UiLockSchedule = { enabled: true, start: '09:00', end: '10:00' };
    const t0930 = new Date(2026, 2, 1, 9, 30);
    const t1030 = new Date(2026, 2, 1, 10, 30);
    expect(evaluateUiLockTick({ ...s, enabled: false }, t0930, null, null)).toBeNull();
    expect(evaluateUiLockTick(s, t1030, null, null)).toBeNull();
    const v1 = evaluateUiLockTick(s, t0930, null, null);
    expect(v1).toEqual({ relock: true, occurrenceId: '2026-03-01' });
    // Same occurrence already unlocked → do not lock
    expect(evaluateUiLockTick(s, t0930, '2026-03-01', '2026-03-01')).toBeNull();
    // Already locked but currentOcc lost (e.g. component remount) → reuse, do not regenerate
    const v2 = evaluateUiLockTick(s, t0930, null, '2026-03-01');
    expect(v2).toEqual({ relock: false, occurrenceId: '2026-03-01' });
  });
});

describe('UiLockScheduleSection — 设置分区', () => {
  it('渲染存储的日程；修改只更新草稿，点「确定」才持久化并派发立即生效事件', () => {
    store({ enabled: true, start: '22:00', end: '06:00' });
    const applied = vi.fn();
    window.addEventListener(UI_LOCK_APPLY_EVENT, applied);
    render(<I18nProvider><UiLockScheduleSection /></I18nProvider>);

    const start = screen.getByLabelText('ui-lock-start') as HTMLInputElement;
    const end = screen.getByLabelText('ui-lock-end') as HTMLInputElement;
    const enabled = screen.getByLabelText('ui-lock-enabled') as HTMLInputElement;
    expect(start.value).toBe('22:00');
    expect(end.value).toBe('06:00');
    expect(enabled.checked).toBe(true);
    expect(screen.getByTestId('ui-lock-error').textContent).toBe('');

    // Draft: edits are not persisted
    fireEvent.change(start, { target: { value: '23:00' } });
    expect(readStore()).toEqual({ enabled: true, start: '22:00', end: '06:00' });

    // OK: persist only after validation passes + dispatch UI_LOCK_APPLY_EVENT (the signal to lock at once inside the window)
    fireEvent.click(screen.getByRole('button', { name: /Done|确定/ }));
    expect(readStore()).toEqual({ enabled: true, start: '23:00', end: '06:00' });
    expect(applied).toHaveBeenCalledTimes(1);
    window.removeEventListener(UI_LOCK_APPLY_EVENT, applied);
  });

  it('start===end：点「确定」阻断保存并显示错误（草稿阶段不报错）', () => {
    store({ enabled: true, start: '22:00', end: '06:00' });
    render(<I18nProvider><UiLockScheduleSection /></I18nProvider>);

    const end = screen.getByLabelText('ui-lock-end') as HTMLInputElement;
    fireEvent.change(end, { target: { value: '22:00' } });
    expect(screen.getByTestId('ui-lock-error').textContent).toBe('');

    fireEvent.click(screen.getByRole('button', { name: /Done|确定/ }));
    expect(screen.getByTestId('ui-lock-error').textContent).not.toBe('');
    // Invalid changes are not persisted
    expect(readStore()).toEqual({ enabled: true, start: '22:00', end: '06:00' });
  });
});

describe('UiLockGate — 锁屏层', () => {
  it('窗口内挂载即锁定：口令 10 位、窗口文案、下方应用树保持挂载', () => {
    vi.useFakeTimers({ now: new Date(2026, 2, 1, 9, 30) });
    store({ enabled: true, start: '09:00', end: '10:00' });
    render(<I18nProvider><><div data-testid="probe">app-shell</div><UiLockGate /></></I18nProvider>);

    // The first tick of the mount effect locks synchronously
    const overlay = screen.getByTestId('ui-lock-overlay');
    // Guarantee that sessions keep running: the lock is only an overlay; the probe (app tree) stays mounted
    expect(screen.getByTestId('probe').textContent).toBe('app-shell');
    const code = screen.getByTestId('ui-lock-code').textContent || '';
    expect(code).toMatch(/^[A-Za-z0-9]{10}$/);
    expect(overlay.textContent).toContain('09:00');
    expect(overlay.textContent).toContain('10:00');
  });

  it('禁用/窗外不锁定', () => {
    vi.useFakeTimers({ now: new Date(2026, 2, 1, 12, 0) });
    store({ enabled: true, start: '09:00', end: '10:00' });
    const { container } = render(<I18nProvider><UiLockGate /></I18nProvider>);
    expect(container.querySelector('[data-testid="ui-lock-overlay"]')).toBeNull();

    store({ enabled: false, start: '00:00', end: '23:59' });
    render(<I18nProvider><UiLockGate /></I18nProvider>);
    expect(document.querySelectorAll('[data-testid="ui-lock-overlay"]')).toHaveLength(0);
  });

  it('错口令报错且保持锁定；对口令解锁后窗口内 tick 不重锁；下一窗口自动重锁（新口令）', () => {
    vi.useFakeTimers({ now: new Date(2026, 2, 1, 9, 30) });
    store({ enabled: true, start: '09:00', end: '10:00' });
    render(<I18nProvider><UiLockGate /></I18nProvider>);

    expect(screen.getByTestId('ui-lock-overlay')).toBeTruthy();
    const code = screen.getByTestId('ui-lock-code').textContent || '';

    // Wrong passcode: error message + still locked
    const input = screen.getByLabelText(/口令以|passcode/i) as HTMLInputElement;
    const unlockBtn = screen.getByRole('button', { name: /解锁|unlock/i });
    fireEvent.change(input, { target: { value: 'wrong-pass' } });
    fireEvent.click(unlockBtn);
    expect(screen.getByTestId('ui-lock-error').textContent).not.toBe('');
    expect(screen.getByTestId('ui-lock-overlay')).toBeTruthy();

    // Correct passcode: unlock
    fireEvent.change(input, { target: { value: code } });
    fireEvent.click(unlockBtn);
    expect(screen.queryByTestId('ui-lock-overlay')).toBeNull();

    // Multiple ticks inside the window: no re-lock (occurrence unlock memory)
    act(() => { vi.advanceTimersByTime(LOCK_TICK_MS * 3); });
    expect(screen.queryByTestId('ui-lock-overlay')).toBeNull();

    // Fast-forward one day (cross the window end + next occurrence arrives): auto-relock and generate a new passcode
    act(() => { vi.advanceTimersByTime(24 * 60 * 60 * 1000); });
    expect(screen.getByTestId('ui-lock-overlay')).toBeTruthy();
    const code2 = screen.getByTestId('ui-lock-code').textContent || '';
    expect(code2).toMatch(/^[A-Za-z0-9]{10}$/);
    expect(code2).not.toBe(code); // 每次锁定随机新口令
  });

  it('日程变更即时生效：窗外改到窗口内，下一个 tick 锁定', () => {
    vi.useFakeTimers({ now: new Date(2026, 2, 1, 12, 0) });
    store({ enabled: false, start: '00:00', end: '23:59' });
    render(<I18nProvider><UiLockGate /></I18nProvider>);
    expect(screen.queryByTestId('ui-lock-overlay')).toBeNull();

    store({ enabled: true, start: '11:00', end: '13:00' }); // 12:00 正处新窗口
    act(() => { vi.advanceTimersByTime(LOCK_TICK_MS); });
    expect(screen.getByTestId('ui-lock-overlay')).toBeTruthy();
  });

  it('窗口内点「确定」（UI_LOCK_APPLY_EVENT）→ 立即锁定，无需等 tick', () => {
    vi.useFakeTimers({ now: new Date(2026, 2, 1, 12, 0) });
    store({ enabled: false, start: '11:00', end: '13:00' });
    render(<I18nProvider><><UiLockScheduleSection /><UiLockGate /></></I18nProvider>);
    expect(screen.queryByTestId('ui-lock-overlay')).toBeNull();

    fireEvent.click(screen.getByLabelText('ui-lock-enabled')); // 草稿启用
    fireEvent.click(screen.getByRole('button', { name: /Done|确定/ })); // 确定并应用
    // 12:00 is inside the 11:00–13:00 window: already locked without advancing the clock
    expect(screen.getByTestId('ui-lock-overlay')).toBeTruthy();
  });

  it('解锁标记持久化：卸载重挂（模拟重启）同 occurrence 不再锁；次日新窗口自动重锁', () => {
    vi.useFakeTimers({ now: new Date(2026, 2, 1, 9, 30) });
    store({ enabled: true, start: '09:00', end: '10:00' });
    const first = render(<I18nProvider><UiLockGate /></I18nProvider>);
    const code = screen.getByTestId('ui-lock-code').textContent || '';

    // Unlock: the marker is persisted per occurrence (window-start date)
    const input = screen.getByLabelText(/口令以|passcode/i) as HTMLInputElement;
    fireEvent.change(input, { target: { value: code } });
    fireEvent.click(screen.getByRole('button', { name: /解锁|unlock/i }));
    expect(screen.queryByTestId('ui-lock-overlay')).toBeNull();
    expect(localStorage.getItem(UI_LOCK_UNLOCKED_KEY)).toBe('2026-03-01');

    // Simulate restart: unmount and remount; the new process restores unlock memory from the persisted marker
    first.unmount();
    render(<I18nProvider><UiLockGate /></I18nProvider>);
    expect(screen.queryByTestId('ui-lock-overlay')).toBeNull(); // 仍在窗口内，但不再锁
    act(() => { vi.advanceTimersByTime(LOCK_TICK_MS * 3); });
    expect(screen.queryByTestId('ui-lock-overlay')).toBeNull();

    // Next-day new occurrence: auto-relock (the stale persisted marker no longer suppresses)
    act(() => { vi.advanceTimersByTime(24 * 60 * 60 * 1000); });
    expect(screen.getByTestId('ui-lock-overlay')).toBeTruthy();
  });

  it('解锁后重新设置（点「确定」）→ 重新武装：窗口内立即再锁', () => {
    vi.useFakeTimers({ now: new Date(2026, 2, 1, 9, 30) });
    store({ enabled: true, start: '09:00', end: '10:00' });
    render(<I18nProvider><><UiLockScheduleSection /><UiLockGate /></></I18nProvider>);

    // Lock → unlock (marker persisted)
    const code = screen.getByTestId('ui-lock-code').textContent || '';
    const input = screen.getByLabelText(/口令以|passcode/i) as HTMLInputElement;
    fireEvent.change(input, { target: { value: code } });
    fireEvent.click(screen.getByRole('button', { name: /解锁|unlock/i }));
    expect(screen.queryByTestId('ui-lock-overlay')).toBeNull();
    expect(localStorage.getItem(UI_LOCK_UNLOCKED_KEY)).toBe('2026-03-01');

    // Clicking OK without changing the schedule = reconfigure / re-arm: clear the unlock marker, re-lock immediately inside the window
    fireEvent.click(screen.getByRole('button', { name: /Done|确定/ }));
    expect(screen.getByTestId('ui-lock-overlay')).toBeTruthy();
    expect(localStorage.getItem(UI_LOCK_UNLOCKED_KEY)).toBeNull();
  });
});
