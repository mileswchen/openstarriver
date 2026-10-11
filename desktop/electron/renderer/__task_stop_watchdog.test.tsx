/// <reference types="vitest/globals" />
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import React from 'react';
import { renderHook, act } from '@testing-library/react';
import { useTaskStop } from './app';
import { I18nProvider } from './i18n';

/** WI-165 additions over the base useTaskStop contract: Stop must reach the
 * backend WITHOUT the incoming event pipe (the 2026-10-02 freeze: a dead
 * runtime:event stream left Stop armed forever waiting for TaskStarted),
 * and the UI must never stay wedged when confirmations cannot arrive. */

function wrapper({ children }: { children?: React.ReactNode }) {
  return <I18nProvider>{children}</I18nProvider>;
}

beforeEach(() => {
  vi.useFakeTimers();
  vi.spyOn(window, 'confirm').mockReturnValue(true);
  (window as any).starriver = { cancel: vi.fn().mockResolvedValue(true) };
});

afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
  delete (window as any).starriver;
});

describe('useTaskStop watchdog/retry (WI-165)', () => {
  it('retries on a timer when no session id exists — no TaskStarted needed', async () => {
    const ref: React.RefObject<string> = { current: null };
    const notify = vi.fn();
    const { result } = renderHook(() => useTaskStop(ref, notify), { wrapper });
    act(() => result.current.requestStop());
    expect(result.current.stopping).toBe(true);
    // Before the id appears: silence, no cancel.
    await act(async () => { await vi.advanceTimersByTimeAsync(6000); });
    expect((window as any).starriver.cancel).not.toHaveBeenCalled();
    // The id arrives WITHOUT any TaskStarted event (dead event pipe) — the
    // retry loop must still deliver the cancel.
    ref.current = 's9';
    await act(async () => { await vi.advanceTimersByTimeAsync(2000); });
    expect((window as any).starriver.cancel).toHaveBeenCalledWith('s9');
  });

  it('resets via the 20s watchdog when no terminal confirmation arrives', async () => {
    const ref: React.RefObject<string> = { current: 's1' };
    const notify = vi.fn();
    const { result } = renderHook(() => useTaskStop(ref, notify), { wrapper });
    act(() => result.current.requestStop());
    await act(async () => { await vi.advanceTimersByTimeAsync(0); });
    // cancel resolved true but TaskCancelled never arrives (dead pipe).
    await act(async () => { await vi.advanceTimersByTimeAsync(20000); });
    expect(result.current.stopping).toBe(false);
    expect(notify).toHaveBeenCalledTimes(1);
  });

  it('a terminal event cancels the watchdog — no false alarm', async () => {
    const ref: React.RefObject<string> = { current: 's1' };
    const notify = vi.fn();
    const { result } = renderHook(() => useTaskStop(ref, notify), { wrapper });
    act(() => result.current.requestStop());
    await act(async () => { await vi.advanceTimersByTimeAsync(0); });
    act(() => result.current.onTerminal());
    expect(result.current.stopping).toBe(false);
    await act(async () => { await vi.advanceTimersByTimeAsync(25000); });
    expect(notify).not.toHaveBeenCalled();
  });

  it('cancels immediately when the session id is known', async () => {
    const ref: React.RefObject<string> = { current: 's1' };
    const notify = vi.fn();
    const { result } = renderHook(() => useTaskStop(ref, notify), { wrapper });
    act(() => result.current.requestStop());
    await act(async () => { await vi.advanceTimersByTimeAsync(0); });
    expect((window as any).starriver.cancel).toHaveBeenCalledWith('s1');
    expect(result.current.stopping).toBe(true);
  });
});
