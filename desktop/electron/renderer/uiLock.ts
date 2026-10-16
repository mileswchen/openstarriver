/**
 * 界面定时锁（Scheduled UI Lock）— 纯逻辑模块。
 *
 * 这是一个 renderer 侧的自律/防沉迷摩擦功能：用户配置每日锁定期
 * （开始/结束 HH:MM），锁定期内 App 全屏覆盖锁屏层，必须键入本次随机
 * 生成的 10 位字母+数字口令才能解锁。锁屏层只覆盖不卸载应用树，
 * 后台会话（daemon 流式任务）不受影响。
 *
 * 设计决策：
 * - 持久化走 localStorage（`starriver.uiLock`），与 renderer 侧 UI 偏好
 *   （trajMode 等）同一模式：纯窗口级 UI 偏好，embedded/daemon 零差异。
 * - 非安全边界：口令展示在锁屏上（防无意识继续使用，而非防他人）；
 *   DevTools 移除覆盖层不在防御范围。
 * - "occurrence"（一次锁定窗口实例）= 当前窗口起点的日期（跨午夜窗口
 *   凌晨时段回溯到昨日起点）。解锁按 occurrence 记忆：本窗口内不再
 *   重锁，下一次 occurrence 到来自动重锁并生成新口令。自 2026-10-16
 *   （用户决策）起，解锁记忆持久化到 localStorage（`starriver.uiLock.unlocked`），
 *   跨启动生效——当天解锁过就不再锁；重新设置（设置弹窗点「确定」）
 *   或次日新 occurrence 自动恢复锁定。
 */

export interface UiLockSchedule {
  enabled: boolean;
  /** 开始时间，格式 HH:MM（24 小时制） */
  start: string;
  /** 结束时间，格式 HH:MM（24 小时制）；与 start 相同视为无效日程 */
  end: string;
}

export const UI_LOCK_STORAGE_KEY = 'starriver.uiLock';
export const LOCK_CODE_LENGTH = 10;
/** 主循环 tick：锁进入/退出的最大感知延迟 */
export const LOCK_TICK_MS = 15_000;
/** 设置弹窗点「确定」后派发：useUiLock 监听并立即重判（窗口内即刻锁屏，不必等 15s tick）。 */
export const UI_LOCK_APPLY_EVENT = 'starriver.uiLock-apply';
/** 持久化"本 occurrence 已解锁"标记（用户决策 2026-10-16）：解锁后跨启动不再重锁；
 *  重新设置（弹窗点「确定」）或次日新 occurrence 自动恢复锁定。 */
export const UI_LOCK_UNLOCKED_KEY = 'starriver.uiLock.unlocked';

export const UI_LOCK_CHARSET = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789';

export const DEFAULT_UI_LOCK_SCHEDULE: UiLockSchedule = {
  enabled: false,
  start: '22:00',
  end: '06:00',
};

/** HH:MM（24 小时制）格式校验 */
export function isValidTime(t: string): boolean {
  return /^([01]\d|2[0-3]):[0-5]\d$/.test(t);
}

/** "HH:MM" → 当日分钟数 */
export function minutesOfDay(t: string): number {
  const [h, m] = t.split(':').map(Number);
  return h * 60 + m;
}

/**
 * now 是否处于锁定期内。支持跨午夜窗口（start > end 视为跨越 00:00，
 * 如 22:00–06:00 覆盖 22:00–24:00 与 00:00–06:00）。
 * start === end 或时间非法一律视为不在窗口（避免永锁）。
 */
export function isInLockWindow(now: Date, start: string, end: string): boolean {
  if (!isValidTime(start) || !isValidTime(end) || start === end) return false;
  const n = now.getHours() * 60 + now.getMinutes();
  const s = minutesOfDay(start);
  const e = minutesOfDay(end);
  return s < e ? (n >= s && n < e) : (n >= s || n < e);
}

/**
 * 当前锁定期 occurrence 的身份标识（窗口起点所在日期 YYYY-MM-DD），
 * 窗外返回 null。跨午夜窗口的凌晨时段回溯到昨日起点，保证同一次
 * 窗口实例在午夜前后身份稳定（解锁记忆因此跨午夜有效）。
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
 * 生成 10 位字母+数字口令。使用 crypto.getRandomValues + 拒绝采样
 * （62 不整除 2^32，取模直接截断会带来轻微偏差）。
 * `bytes` 参数仅供测试注入确定性随机源。
 */
export function generateLockCode(bytes?: Uint32Array): string {
  const buf = bytes ?? new Uint32Array(LOCK_CODE_LENGTH);
  if (!bytes) {
    const g = globalThis.crypto;
    if (!g?.getRandomValues) throw new Error('crypto.getRandomValues unavailable');
    g.getRandomValues(buf);
  }
  // 拒绝采样上限：62 * 69,088,469 ≈ 2^32（取小于 2^32 的最大 62 倍数）
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

/** 读取本地日程；缺失/损坏/字段不全时回退默认（禁用）。 */
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
    /* storage 不可用或 JSON 损坏 — 回退默认 */
  }
  return { ...DEFAULT_UI_LOCK_SCHEDULE };
}

/** 写入本地日程；storage 不可用时静默（与现有 localStorage 偏好的防御一致）。 */
export function saveUiLockSchedule(schedule: UiLockSchedule, storage?: Storage): void {
  const store = storage ?? globalThis.localStorage;
  try {
    store?.setItem(UI_LOCK_STORAGE_KEY, JSON.stringify(schedule));
  } catch {
    /* storage 不可用 — 忽略 */
  }
}

/** 读取持久化的"本 occurrence 已解锁"标记；缺失/格式非法回退 null。 */
export function loadUiLockUnlockedOccurrence(storage?: Storage): string | null {
  const store = storage ?? globalThis.localStorage;
  try {
    const raw = store?.getItem(UI_LOCK_UNLOCKED_KEY);
    return typeof raw === 'string' && /^\d{4}-\d{2}-\d{2}$/.test(raw) ? raw : null;
  } catch {
    return null;
  }
}

/** 写入/清除"本 occurrence 已解锁"标记；occ 为 null 即清除。storage 不可用时静默。 */
export function saveUiLockUnlockedOccurrence(occ: string | null, storage?: Storage): void {
  const store = storage ?? globalThis.localStorage;
  try {
    if (occ == null) store?.removeItem(UI_LOCK_UNLOCKED_KEY);
    else store?.setItem(UI_LOCK_UNLOCKED_KEY, occ);
  } catch {
    /* storage 不可用 — 忽略 */
  }
}

/**
 * 一次锁屏 tick 的判定：给定日程与当前时间，返回应处的锁屏状态。
 * - 返回 null：不锁定。
 * - 返回 { relock, occurrenceId }：应锁定；`relock` 表示进入了一个
 *   新的 occurrence（需要生成新口令），否则沿用当前口令。
 * 调用方（useUiLock）持有 `unlockedOccurrence` 与 `currentOcc` 记忆：
 * occurrence === 已解锁的那个 → 不锁；occurrence 变化 → 重锁。
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
