/**
 * Scheduled UI Lock — pure-logic module.
 *
 * This is a renderer-side self-discipline / anti-addiction friction feature: the user configures a daily lock window
 * (start/end HH:MM); during the window the App is covered by a full-screen lock layer, and a randomly generated
 * 10-char letter+digit passcode for this occurrence must be typed to unlock. The lock layer only covers, never unmounts the app tree;
 * background sessions (daemon streaming tasks) are unaffected.
 *
 * Design decisions:
 * - Persistence uses localStorage (`starriver.uiLock`), the same pattern as renderer-side UI preferences
 *   (trajMode etc.): a pure window-level UI preference, zero difference between embedded/daemon.
 * - Not a security boundary: the passcode is shown on the lock screen (guards against mindless continued use, not against others);
 *   removing the overlay via DevTools is out of scope for defense.
 * - An "occurrence" (one lock-window instance) = the date of the current window's start (a cross-midnight window's
 *   early-morning part traces back to yesterday's start). Unlock is remembered per occurrence: no re-lock within this
 *   window; the next occurrence auto-relocks and generates a new passcode. Since 2026-10-16
 *   (user decision), the unlock memory is persisted to localStorage (`starriver.uiLock.unlocked`),
 *   surviving restarts — once unlocked for the day it does not re-lock; reconfiguring (OK in the settings dialog)
 *   or the next day's new occurrence automatically restores the lock.
 */

export interface UiLockSchedule {
  enabled: boolean;
  /** Start time, format HH:MM (24-hour) */
  start: string;
  /** End time, format HH:MM (24-hour); equal to start is treated as an invalid schedule */
  end: string;
}

export const UI_LOCK_STORAGE_KEY = 'starriver.uiLock';
export const LOCK_CODE_LENGTH = 10;
/** Main-loop tick: the maximum perceived latency for lock enter/exit */
export const LOCK_TICK_MS = 15_000;
/** Dispatched after clicking OK in the settings dialog: useUiLock listens and re-decides immediately (locks at once inside the window, no need to wait for the 15s tick). */
export const UI_LOCK_APPLY_EVENT = 'starriver.uiLock-apply';
/** Persist the "this occurrence unlocked" marker (user decision 2026-10-16): after unlock, no re-lock across restarts;
 *  reconfiguring (OK in the dialog) or the next day's new occurrence automatically restores the lock. */
export const UI_LOCK_UNLOCKED_KEY = 'starriver.uiLock.unlocked';

export const UI_LOCK_CHARSET = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789';

export const DEFAULT_UI_LOCK_SCHEDULE: UiLockSchedule = {
  enabled: false,
  start: '22:00',
  end: '06:00',
};

/** HH:MM (24-hour) format validation */
export function isValidTime(t: string): boolean {
  return /^([01]\d|2[0-3]):[0-5]\d$/.test(t);
}

/** "HH:MM" → minutes since midnight */
export function minutesOfDay(t: string): number {
  const [h, m] = t.split(':').map(Number);
  return h * 60 + m;
}

/**
 * Whether now is inside the lock window. Supports cross-midnight windows (start > end means crossing 00:00,
 * e.g. 22:00–06:00 covers 22:00–24:00 and 00:00–06:00).
 * start === end or invalid times are always treated as outside the window (avoids a permanent lock).
 */
export function isInLockWindow(now: Date, start: string, end: string): boolean {
  if (!isValidTime(start) || !isValidTime(end) || start === end) return false;
  const n = now.getHours() * 60 + now.getMinutes();
  const s = minutesOfDay(start);
  const e = minutesOfDay(end);
  return s < e ? (n >= s && n < e) : (n >= s || n < e);
}

/**
 * The identity of the current lock-window occurrence (the date YYYY-MM-DD of the window start),
 * null outside the window. A cross-midnight window's early-morning part traces back to yesterday's start, so the
 * same window instance has a stable identity across midnight (the unlock memory is therefore valid across midnight).
 */
export function lockOccurrenceId(now: Date, start: string, end: string): string | null {
  if (!isInLockWindow(now, start, end)) return null;
  const d = new Date(now);
  if (minutesOfDay(start) > minutesOfDay(end) && (now.getHours() * 60 + now.getMinutes()) < minutesOfDay(end)) {
    d.setDate(d.getDate() - 1); // 跨午夜：凌晨处于昨晚开始的窗口
  }
  const p = (x: number) => String(x).padStart(2, '0');
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}`;
}

/**
 * Generate a 10-char letter+digit passcode. Uses crypto.getRandomValues + rejection sampling
 * (62 does not divide 2^32; a direct modulo truncation would introduce a slight bias).
 * The `bytes` parameter is only for tests to inject a deterministic random source.
 */
export function generateLockCode(bytes?: Uint32Array): string {
  const buf = bytes ?? new Uint32Array(LOCK_CODE_LENGTH);
  if (!bytes) {
    const g = globalThis.crypto;
    if (!g?.getRandomValues) throw new Error('crypto.getRandomValues unavailable');
    g.getRandomValues(buf);
  }
  // Rejection-sampling ceiling: 62 * 69,088,469 ≈ 2^32 (the largest multiple of 62 below 2^32)
  const limit = Math.floor(0x1_0000_0000 / UI_LOCK_CHARSET.length) * UI_LOCK_CHARSET.length;
  let out = '';
  let i = 0;
  while (out.length < LOCK_CODE_LENGTH) {
    if (i >= buf.length) throw new Error('generateLockCode: random buffer exhausted');
    const v = buf[i++];
    if (v < limit) out += UI_LOCK_CHARSET[v % UI_LOCK_CHARSET.length];
  }
  return out;
}

/** Read the local schedule; fall back to the default (disabled) when missing/corrupt/incomplete. */
export function loadUiLockSchedule(storage?: Storage): UiLockSchedule {
  const store = storage ?? globalThis.localStorage;
  try {
    const raw = store?.getItem(UI_LOCK_STORAGE_KEY);
    if (!raw) return { ...DEFAULT_UI_LOCK_SCHEDULE };
    const parsed = JSON.parse(raw) as Partial<UiLockSchedule>;
    if (
      parsed &&
      typeof parsed.enabled === 'boolean' &&
      isValidTime(String(parsed.start ?? '')) &&
      isValidTime(String(parsed.end ?? ''))
    ) {
      return { enabled: parsed.enabled, start: String(parsed.start), end: String(parsed.end) };
    }
  } catch {
    /* storage unavailable or JSON corrupt — fall back to default */
  }
  return { ...DEFAULT_UI_LOCK_SCHEDULE };
}

/** Write the local schedule; silent when storage is unavailable (consistent with existing localStorage-preference defenses). */
export function saveUiLockSchedule(schedule: UiLockSchedule, storage?: Storage): void {
  const store = storage ?? globalThis.localStorage;
  try {
    store?.setItem(UI_LOCK_STORAGE_KEY, JSON.stringify(schedule));
  } catch {
    /* storage unavailable — ignore */
  }
}

/** Read the persisted "this occurrence unlocked" marker; fall back to null when missing/invalid. */
export function loadUiLockUnlockedOccurrence(storage?: Storage): string | null {
  const store = storage ?? globalThis.localStorage;
  try {
    const raw = store?.getItem(UI_LOCK_UNLOCKED_KEY);
    return typeof raw === 'string' && /^\d{4}-\d{2}-\d{2}$/.test(raw) ? raw : null;
  } catch {
    return null;
  }
}

/** Write/clear the "this occurrence unlocked" marker; occ null means clear. Silent when storage is unavailable. */
export function saveUiLockUnlockedOccurrence(occ: string | null, storage?: Storage): void {
  const store = storage ?? globalThis.localStorage;
  try {
    if (occ == null) store?.removeItem(UI_LOCK_UNLOCKED_KEY);
    else store?.setItem(UI_LOCK_UNLOCKED_KEY, occ);
  } catch {
    /* storage unavailable — ignore */
  }
}

/**
 * The decision of one lock tick: given the schedule and the current time, return the lock state to be in.
 * - Return null: do not lock.
 * - Return { relock, occurrenceId }: should lock; `relock` means a new occurrence was entered
 *   (a new passcode must be generated), otherwise reuse the current passcode.
 * The caller (useUiLock) holds the `unlockedOccurrence` and `currentOcc` memory:
 * occurrence === the unlocked one → do not lock; occurrence changed → re-lock.
 */
export function evaluateUiLockTick(
  schedule: UiLockSchedule | null,
  now: Date,
  unlockedOccurrence: string | null,
  currentOcc: string | null,
): { relock: boolean; occurrenceId: string } | null {
  if (!schedule || !schedule.enabled) return null;
  const occ = lockOccurrenceId(now, schedule.start, schedule.end);
  if (!occ || occ === unlockedOccurrence) return null;
  return { relock: occ !== currentOcc, occurrenceId: occ };
}
