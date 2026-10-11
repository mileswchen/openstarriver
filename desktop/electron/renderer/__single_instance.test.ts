// Single-instance lock unit tests (electron mocked — no real app lifecycle).

import { beforeEach, describe, expect, it, vi } from 'vitest';

const state = vi.hoisted(() => ({
  lockResult: true,
  listeners: new Map<string, Array<() => void>>(),
  windows: [] as Array<{ isDestroyed: () => boolean; show: ReturnType<typeof vi.fn>; focus: ReturnType<typeof vi.fn> }>,
}));

vi.mock('electron', () => ({
  app: {
    requestSingleInstanceLock: () => state.lockResult,
    on: (ev: string, fn: () => void) => {
      const arr = state.listeners.get(ev) ?? [];
      arr.push(fn);
      state.listeners.set(ev, arr);
    },
    quit: vi.fn(),
  },
  BrowserWindow: {
    getAllWindows: () => state.windows,
  },
}));

// Explicit .ts: tsc emits single-instance.js next to the source (checked in,
// shipped in the asar), and Vite's extension order would otherwise resolve the
// compiled CJS file, whose bare require('electron') bypasses the mock.
import { acquireSingleInstanceLock } from '../single-instance.ts';

function fireSecondInstance() {
  for (const fn of state.listeners.get('second-instance') ?? []) fn();
}

const liveWin = () => ({ isDestroyed: () => false, show: vi.fn(), focus: vi.fn() });
const destroyedWin = () => ({ isDestroyed: () => true, show: vi.fn(), focus: vi.fn() });

beforeEach(() => {
  state.lockResult = true;
  state.listeners.clear();
  state.windows = [];
});

describe('acquireSingleInstanceLock', () => {
  it('primary instance acquires the lock and subscribes to second-instance', () => {
    expect(acquireSingleInstanceLock()).toBe(true);
    expect(state.listeners.get('second-instance')?.length).toBe(1);
  });

  it('second instance is denied the lock and registers no listener', () => {
    state.lockResult = false;
    expect(acquireSingleInstanceLock()).toBe(false);
    expect(state.listeners.get('second-instance')).toBeUndefined();
  });

  it('second-instance event shows and focuses every live window, skips destroyed ones', () => {
    acquireSingleInstanceLock();
    const a = liveWin();
    const b = liveWin();
    const dead = destroyedWin();
    state.windows = [a, b, dead];
    fireSecondInstance();
    expect(a.show).toHaveBeenCalled();
    expect(a.focus).toHaveBeenCalled();
    expect(b.show).toHaveBeenCalled();
    expect(b.focus).toHaveBeenCalled();
    expect(dead.show).not.toHaveBeenCalled();
    expect(dead.focus).not.toHaveBeenCalled();
  });
});
