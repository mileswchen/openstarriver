// FPM P4b: starriverd client (Electron main side).
//
// Responsibilities: discover/spawn the daemon, HTTP invoke, SSE event stream, graceful shutdown.
// Design constraint: Electron exit does NOT kill the daemon (shared service); connection info is
// read from ~/.starriver/daemon.json first, falling back to spawn and the first stdout line for {port,token}.
// Upgrade takeover: before reusing a reachable daemon the client compares the daemon's reported
// `build_id` (sha256 prefix of the running executable) with the bundled binary's — a mismatch or
// an absent id (legacy daemon.json) means the running daemon predates this app build, so the
// client gracefully shuts it down (`--shutdown`, control-socket protocol is version-stable) and
// spawns the bundled binary instead. A daemon that refuses to die fails LOUDLY: silently talking
// to the wrong build hides new features behind misleading UI (seen: lan.state METHOD_NOT_FOUND
// surfacing as "消息服务未启用").

import { spawn, execFile, type ChildProcess } from 'child_process';
import { createHash } from 'crypto';
import * as http from 'http';
import * as fs from 'fs';
import * as os from 'os';
import * as path from 'path';
import { app } from 'electron';
import schema from './protocol.schema.json';

/** Matches starriver-protocol `wire::PROTOCOL_VERSION` (the schema is the single source of truth). */
export const PROTOCOL_VERSION: number = (schema as any).protocolVersion;

export interface DaemonInfo {
  port: number;
  token: string;
  pid: number;
  version?: string;
  protocolVersion?: number;
  /** sha256 prefix (16 hex) of the daemon's own executable; absent on legacy daemon.json. */
  build_id?: string;
  existing?: boolean;
}

export interface DaemonHandle {
  info: DaemonInfo;
  /** Child spawned by this process and still running; null when reusing an existing instance. */
  child: ChildProcess | null;
  spawned: boolean;
}

export type DaemonStatus =
  | { state: 'connecting' }
  | { state: 'connected'; info: DaemonInfo }
  | { state: 'restarting'; attempt: number; error?: string }
  | { state: 'failed'; error: string };

export interface DaemonClientOptions {
  onEnvelope?: (envelope: any) => void;
  onStatus?: (status: DaemonStatus) => void;
  onRestarted?: (info: DaemonInfo) => void;
  /** Test injection: binary path resolution (defaults to packaged/dev paths). */
  binaryPath?: () => string | null;
  /** Test injection: spawn implementation. */
  spawnFn?: (
    binary: string,
    args: string[],
    opts: { cwd: string; stdio: ('ignore' | 'pipe' | 'inherit')[] },
  ) => ChildProcess;
  /** Test injection: /health readiness probe. */
  reachable?: (info: DaemonInfo) => Promise<boolean>;
  /** Test injection: control-channel exec (`--shutdown` during takeover). */
  execFn?: ExecFileFn;
  /** Test injection: process kill (SIGTERM/SIGKILL escalation during takeover). */
  killFn?: KillFn;
  /** Test tuning: takeover waits (defaults TAKEOVER_GRACE_MS / 2s per signal). */
  takeOverGraceMs?: number;
  takeOverEscalationMs?: number;
  dataDir?: string;
  infoPath?: string;
  backoffBaseMs?: number;
  /** Rapid-crash cap (beyond it within the window, enter failed and stop restarting). */
  maxConsecutiveFailures?: number;
  failureWindowMs?: number;
}

export function daemonDataDir(): string {
  const dir = path.join(os.homedir(), '.starriver');
  fs.mkdirSync(dir, { recursive: true });
  return dir;
}

export function daemonInfoPath(): string {
  return path.join(daemonDataDir(), 'daemon.json');
}

export function daemonBinaryPath(): string | null {
  const name = process.platform === 'win32' ? 'starriverd.exe' : 'starriverd';
  if (app.isPackaged) {
    const packaged = path.join(process.resourcesPath, name);
    return fs.existsSync(packaged) ? packaged : null;
  }
  const repoRoot = path.join(__dirname, '..', '..');
  const candidates = [
    path.join(repoRoot, 'target', 'debug', name),
    path.join(repoRoot, 'target', 'release', name),
  ];
  return candidates.find((candidate) => fs.existsSync(candidate)) ?? null;
}

/** Injected control-channel exec (`--shutdown`); callback-style for easy fakes. */
export type ExecFileFn = (
  file: string,
  args: string[],
  opts: { cwd: string },
  cb: (error: Error | null) => void,
) => void;
/** Injected process killer (SIGTERM/SIGKILL escalation). */
export type KillFn = (pid: number, signal: NodeJS.Signals) => void;

const defaultExecFile: ExecFileFn = (file, args, opts, cb) => {
  execFile(file, args, opts, (error) => cb(error));
};
const defaultKill: KillFn = (pid, signal) => {
  try {
    process.kill(pid, signal);
  } catch {
    // ESRCH: the recorded pid is long dead — nothing to escalate.
  }
};

const buildIdCache = new Map<string, string>();

/**
 * sha256 prefix (16 hex) of the bundled binary — the identity a reused daemon
 * must match. Empty string when the binary is missing/unreadable, which makes
 * callers SKIP the identity check (graceful degradation: the reuse path keeps
 * working; a spawn would fail anyway).
 */
export function bundledDaemonBuildId(binary: string | null): string {
  if (!binary) return '';
  const cached = buildIdCache.get(binary);
  if (cached !== undefined) return cached;
  let id = '';
  try {
    id = createHash('sha256')
      .update(fs.readFileSync(binary))
      .digest('hex')
      .slice(0, 16);
  } catch {
    // Unreadable → '' → caller skips the check.
  }
  buildIdCache.set(binary, id);
  return id;
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/** Graceful-shutdown window before SIGTERM/SIGKILL escalation. */
export const TAKEOVER_GRACE_MS = 6_000;
const ESCALATION_STEP_MS = 2_000;

export interface TakeOverOptions {
  /** The bundled binary (its `--shutdown` dials the old daemon's control socket). */
  binary: string;
  info: DaemonInfo;
  dataDir: string;
  infoPath: string;
  execFn: ExecFileFn;
  killFn: KillFn;
  reachable: (info: DaemonInfo) => Promise<boolean>;
  /** Graceful-shutdown wait; tests shrink this. */
  waitClosedMs?: number;
  /** Per-signal escalation wait; tests shrink this. */
  escalationMs?: number;
}

/**
 * Replace a running daemon of a different build:
 * 1. `--shutdown` over the control socket — the one-line JSON protocol is
 *    version-stable, so the NEW binary can politely stop the OLD daemon;
 * 2. poll /health until the port is actually closed (spawning immediately
 *    would hit "address already in use" or yield to the dying instance);
 * 3. escalate SIGTERM → SIGKILL on the recorded pid (may be stale — ESRCH is
 *    harmless), then finally clean a leftover daemon.json.
 * Throws (with a manual-fix hint) when the old daemon refuses to die: the
 * caller must NOT fall through to spawn, where single-instance would just
 * yield the old daemon back and silently reuse the wrong build.
 */
export async function takeOverStaleDaemon(opts: TakeOverOptions): Promise<void> {
  const describe = () =>
    `stale daemon on port ${opts.info.port} (build ${opts.info.build_id || 'legacy/unknown'})`;
  const cleanupInfoFile = () => {
    try {
      fs.rmSync(opts.infoPath, { force: true });
    } catch {
      // Best effort — spawn overwrites it anyway.
    }
  };
  const waitClosed = async (timeoutMs: number): Promise<boolean> => {
    const deadline = Date.now() + timeoutMs;
    while (Date.now() < deadline) {
      if (!(await opts.reachable(opts.info))) {
        cleanupInfoFile();
        return true;
      }
      await sleep(250);
    }
    return false;
  };

  // 1) Graceful. Errors are expected when no control socket exists (crashed
  // daemon left none) — escalation covers it.
  try {
    await new Promise<void>((resolve, reject) => {
      opts.execFn(opts.binary, ['--shutdown'], { cwd: opts.dataDir }, (error) =>
        error ? reject(error) : resolve(),
      );
    });
  } catch {
    // Fall through to the liveness wait / escalation.
  }
  // 2) + 3) Port must actually close; escalate if it does not.
  if (await waitClosed(opts.waitClosedMs ?? TAKEOVER_GRACE_MS)) return;
  for (const signal of ['SIGTERM', 'SIGKILL'] as const) {
    opts.killFn(opts.info.pid, signal);
    if (await waitClosed(opts.escalationMs ?? ESCALATION_STEP_MS)) return;
  }
  throw new Error(
    `${describe()} did not exit after a shutdown request and escalation — ` +
      'kill it manually (`pkill -f starriverd`) and start the app again',
  );
}

export interface StopDaemonOptions {
  dataDir?: string;
  infoPath?: string;
  binaryPath?: string | null;
  execFn?: ExecFileFn;
  killFn?: KillFn;
  reachable?: (info: DaemonInfo) => Promise<boolean>;
  /** Graceful-shutdown wait; tests shrink this. */
  waitClosedMs?: number;
  /** Per-signal escalation wait; tests shrink this. */
  escalationMs?: number;
}

export interface StopDaemonResult {
  /** A reachable daemon was found and asked to stop. */
  stopped: boolean;
  /** The port actually went down (or nothing was running). */
  confirmed: boolean;
  /** Set when the daemon refused to die — surface with the manual hint. */
  hint?: string;
}

/**
 * Explicit user action ("停止后台服务并退出" / macOS 卸载): gracefully stop the
 * shared daemon and clean runtime handoff files. Unlike the upgrade takeover
 * there is NO build-identity check — the user asked the daemon to stop,
 * regardless of which build is running.
 */
export async function stopDaemonAndCleanup(opts: StopDaemonOptions = {}): Promise<StopDaemonResult> {
  const dataDir = opts.dataDir ?? daemonDataDir();
  const infoPath = opts.infoPath ?? daemonInfoPath();
  const execFn = opts.execFn ?? defaultExecFile;
  const killFn = opts.killFn ?? defaultKill;
  const reachable = opts.reachable ?? daemonReachable;
  const waitClosedMs = opts.waitClosedMs ?? TAKEOVER_GRACE_MS;
  const escalationMs = opts.escalationMs ?? ESCALATION_STEP_MS;
  const cleanupInfoFile = () => {
    try {
      fs.rmSync(infoPath, { force: true });
    } catch {
      // Best effort — a leftover daemon.json only causes one failed reuse probe.
    }
  };
  const waitClosed = async (timeoutMs: number): Promise<boolean> => {
    const deadline = Date.now() + timeoutMs;
    while (Date.now() < deadline) {
      if (!(await reachable(info))) {
        cleanupInfoFile();
        return true;
      }
      await sleep(250);
    }
    return false;
  };

  // Nothing running (no info file, or the recorded port is already closed)?
  let info: DaemonInfo;
  try {
    info = JSON.parse(fs.readFileSync(infoPath, 'utf8')) as DaemonInfo;
    if (typeof info.port !== 'number' || typeof info.token !== 'string') throw new Error('bad info');
    if (!(await reachable(info))) {
      cleanupInfoFile();
      return { stopped: false, confirmed: true };
    }
  } catch {
    return { stopped: false, confirmed: true };
  }

  const binary = opts.binaryPath !== undefined ? opts.binaryPath : daemonBinaryPath();
  if (!binary) {
    // No bundled binary to speak to the control socket with — try pid escalation only.
    const down = await escalate(info, killFn, reachable, escalationMs, cleanupInfoFile);
    return {
      stopped: true,
      confirmed: down,
      hint: down ? undefined : 'pkill -f starriverd',
    };
  }
  try {
    await new Promise<void>((resolve) => {
      execFn(binary, ['--shutdown'], { cwd: dataDir }, () => resolve());
    });
  } catch {
    // No control socket / refused → escalation covers it.
  }
  if (await waitClosed(waitClosedMs)) return { stopped: true, confirmed: true };
  const down = await escalate(info, killFn, reachable, escalationMs, cleanupInfoFile);
  return { stopped: true, confirmed: down, hint: down ? undefined : 'pkill -f starriverd' };
}

async function escalate(
  info: DaemonInfo,
  killFn: KillFn,
  reachable: (info: DaemonInfo) => Promise<boolean>,
  escalationMs: number,
  cleanupInfoFile: () => void,
): Promise<boolean> {
  for (const signal of ['SIGTERM', 'SIGKILL'] as const) {
    killFn(info.pid, signal);
    const deadline = Date.now() + escalationMs;
    while (Date.now() < deadline) {
      if (!(await reachable(info))) {
        cleanupInfoFile();
        return true;
      }
      await sleep(250);
    }
  }
  return false;
}

/** /health readiness probe (the daemon.json reuse path). */
export function daemonReachable(info: DaemonInfo, timeoutMs = 1500): Promise<boolean> {
  return new Promise((resolve) => {
    const req = http.get(
      { host: '127.0.0.1', port: info.port, path: '/health', timeout: timeoutMs },
      (res) => {
        res.resume();
        resolve(res.statusCode === 200);
      },
    );
    req.on('error', () => resolve(false));
    req.on('timeout', () => {
      req.destroy();
      resolve(false);
    });
  });
}

function readStartupLine(child: ChildProcess, timeoutMs: number): Promise<DaemonInfo> {
  return new Promise((resolve, reject) => {
    let buffer = '';
    let settled = false;
    const finish = (fn: () => void) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      child.stdout?.off('data', onData);
      child.off('exit', onExit);
      fn();
    };
    const timer = setTimeout(
      () => finish(() => reject(new Error('starriverd startup timed out'))),
      timeoutMs,
    );
    const onData = (chunk: string) => {
      buffer += chunk;
      const newline = buffer.indexOf('\n');
      if (newline === -1) return;
      const line = buffer.slice(0, newline);
      try {
        const info = JSON.parse(line);
        if (typeof info.port === 'number' && typeof info.token === 'string') {
          finish(() => resolve(info as DaemonInfo));
        }
      } catch {
        // First line is not JSON (an error log) → keep waiting for the next line.
      }
    };
    // WI-31: keep the last stderr line — "exited before reporting (code 1)"
    // alone is not actionable when the real reason (e.g. "Address already in
    // use") only reaches stderr.
    let stderrTail = '';
    const onStderr = (chunk: string) => {
      stderrTail = String(chunk).trim().split('\n').pop() || stderrTail;
    };
    const onExit = (code: number | null) =>
      finish(() =>
        reject(
          new Error(
            stderrTail
              ? `starriverd exited before reporting (code ${code}): ${stderrTail}`
              : `starriverd exited before reporting (code ${code})`,
          ),
        ),
      );
    child.stdout?.on('data', onData);
    child.stderr?.on('data', onStderr);
    child.once('exit', onExit);
  });
}

/** Connect to an existing daemon or spawn a new instance; both paths return usable connection info. */
export async function connectOrSpawnDaemon(): Promise<DaemonHandle> {
  // 1) Existing daemon.json → /health reachable → reuse — same build-identity
  // takeover semantics as DaemonClient.obtainInfo (see that method's comment).
  let stale: { info: DaemonInfo; binary: string } | null = null;
  try {
    const raw = fs.readFileSync(daemonInfoPath(), 'utf8');
    const info = JSON.parse(raw) as DaemonInfo;
    if (typeof info.port === 'number' && typeof info.token === 'string' && (await daemonReachable(info))) {
      const binary = daemonBinaryPath();
      const bundled = bundledDaemonBuildId(binary);
      if (!bundled || info.build_id === bundled) {
        return { info, child: null, spawned: false };
      }
      stale = { info, binary: binary! };
    }
  } catch {
    // File missing/corrupt → spawn.
  }
  if (stale) {
    await takeOverStaleDaemon({
      binary: stale.binary,
      info: stale.info,
      dataDir: daemonDataDir(),
      infoPath: daemonInfoPath(),
      execFn: defaultExecFile,
      killFn: defaultKill,
      reachable: daemonReachable,
    });
  }

  // 2) Spawn; the daemon runs with ~/.starriver as cwd (without --config the session db lands there too).
  const binary = daemonBinaryPath();
  if (!binary) {
    throw new Error(
      'starriverd binary not found — run `cargo build -p starriverd` (dev) or ship it in resources/ (packaged)',
    );
  }
  const child = spawn(binary, [], {
    cwd: daemonDataDir(),
    stdio: ['ignore', 'pipe', 'inherit'],
  });
  const info = await readStartupLine(child, 30_000).catch((error: unknown) => {
          // A half-started daemon holds the single-instance control socket —
          // leaving it alive wedges every later retry into `existing: true`.
          try { child.kill(); } catch { /* already gone */ }
          throw error;
        });
  if (info.existing) {
      // Existing instance: the child only prints connection info and exits; wait for it to be reaped.
    await new Promise<void>((resolve) => child.once('exit', () => resolve()));
    return { info, child: null, spawned: false };
  }
  return { info, child, spawned: true };
}

export interface ProtocolResponse {
  v: number;
  id: string;
  ok: boolean;
  result?: any;
  error?: { code?: string; message?: string; data?: unknown };
}

/** Full protocol response (streaming methods' subscriptions are caller-managed). */
export function daemonInvokeRaw(info: DaemonInfo, request: unknown): Promise<ProtocolResponse> {
  return new Promise((resolve, reject) => {
    const payload = JSON.stringify(request);
    const req = http.request(
      {
        host: '127.0.0.1',
        port: info.port,
        path: '/api/v1/invoke',
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
          'Content-Length': Buffer.byteLength(payload),
          Authorization: `Bearer ${info.token}`,
        },
      },
      (res) => {
        let body = '';
        res.setEncoding('utf8');
        res.on('data', (chunk: string) => {
          body += chunk;
        });
        res.on('end', () => {
          try {
            const parsed = JSON.parse(body) as ProtocolResponse;
            if (parsed.ok) resolve(parsed);
            else
              reject(
                Object.assign(
                  new Error(parsed.error?.message || `protocol error ${parsed.error?.code ?? 'UNKNOWN'}`),
                  parsed.error,
                  { isProtocolError: true },
                ),
              );
          } catch (error) {
            reject(error);
          }
        });
      },
    );
    req.on('error', reject);
    // A wedged daemon (handler blocked on a stuck DB lock) would otherwise
    // leave every invoke pending forever — the failure must flow into
    // handleInvokeError's probe/restart path instead.
    req.setTimeout(120_000, () => {
      req.destroy(new Error('daemon invoke timed out'));
    });
    req.write(payload);
    req.end();
  });
}

/** Convenience variant: returns only the result on success. */
export async function daemonInvoke(info: DaemonInfo, method: string, params: unknown): Promise<any> {
  const response = await daemonInvokeRaw(info, {
    v: PROTOCOL_VERSION,
    id: `req_${Date.now().toString(36)}_${Math.random().toString(36).slice(2, 8)}`,
    method,
    params: params ?? {},
  });
  return response.result;
}

/** SSE event stream: main forwards to the renderer (`runtime:event`) and parses terminal states for legacy streams. */
export function daemonEventStream(
  info: DaemonInfo,
  onEnvelope: (envelope: any) => void,
  onConnectionLost?: () => void,
): () => void {
  // Auto-reconnect: send the largest received global_seq (`after=`) so the server replays missed events;
  // exponential backoff 1s→2s→…→15s, matching the browser HttpSseTransport client.
  let closed = false;
  let retry = 0;
  let lastGlobalSeq: number | undefined;
  let req: http.ClientRequest | null = null;

  const scheduleReconnect = () => {
    if (closed) return;
    const delay = Math.min(1000 * 2 ** retry, 15_000);
    retry += 1;
    setTimeout(connect, delay).unref?.();
  };

  const connect = () => {
    if (closed) return;
    const path = `/api/v1/events?token=${encodeURIComponent(info.token)}${
      lastGlobalSeq !== undefined ? `&after=${lastGlobalSeq}` : ''
    }`;
    req = http.get({ host: '127.0.0.1', port: info.port, path }, (res) => {
      if (res.statusCode !== 200) {
        res.resume();
        onConnectionLost?.();
        scheduleReconnect();
        return;
      }
      retry = 0;
      let buffer = '';
      res.setEncoding('utf8');
      res.on('data', (chunk: string) => {
        buffer += chunk;
        // Normalize CRLF to LF (a trailing lone CR waits for the next chunk
        // before it can pair) — SSE allows both frame separators, and a CRLF
        // server's frames ("\r\n\r\n") never matched the plain "\n\n"
        // split, so no event was ever parsed and the buffer grew unboundedly.
        buffer = buffer.replace(/\r\n/g, '\n');
        let split;
        while ((split = buffer.indexOf('\n\n')) !== -1) {
          const frame = buffer.slice(0, split);
          buffer = buffer.slice(split + 2);
          for (const line of frame.split('\n')) {
            // "data:" without the space is legal SSE — a space-only prefix
            // match dropped those frames entirely.
            if (line.startsWith('data:')) {
              const payload = line[5] === ' ' ? line.slice(6) : line.slice(5);
              try {
                const envelope = JSON.parse(payload);
                if (typeof envelope.global_seq === 'number') {
                  lastGlobalSeq = envelope.global_seq;
                }
                onEnvelope(envelope);
              } catch {
                // Skip bad frames.
              }
            }
          }
        }
      });
      res.on('end', () => scheduleReconnect());
      res.on('error', () => {
        onConnectionLost?.();
        scheduleReconnect();
      });
    });
    req.on('error', () => {
      onConnectionLost?.();
      scheduleReconnect();
    });
  };

  connect();
  return () => {
    closed = true;
    req?.destroy();
  };
}

/** Gracefully shut the daemon down (explicit user action, not app exit). */
export function requestDaemonShutdown(): Promise<void> {
  const binary = daemonBinaryPath();
  if (!binary) {
    return Promise.reject(new Error('starriverd binary not found'));
  }
  return new Promise((resolve, reject) => {
    execFile(binary, ['--shutdown'], { cwd: daemonDataDir() }, (error) => {
      if (error) reject(error);
      else resolve();
    });
  });
}

/**
 * Self-healing daemon client (FPM P4b crash-restart):
 * - connect/reuse: read `daemon.json` + `/health` first, fall back to spawn (first stdout line);
 *   the reuse additionally requires a matching `build_id` — mismatch/absent (legacy) triggers
 *   a graceful takeover (`--shutdown` → wait port close → SIGTERM/SIGKILL escalation → spawn bundled);
 *   a daemon that refuses to die fails the connect loudly instead of silently reusing the wrong build;
 * - process watch: an abnormally exited child we spawned → backoff auto-restart (1s→2s→…→15s);
 *   rapid crashes past the cap enter `failed` (no infinite restart);
 * - connection faults: an invoke connection error (not a protocol error) probes first, restarts if unreachable;
 * - after restart: new port/token (`daemon.json` updated) → rebuild SSE, handshake redone upstream,
 *   session snapshots persist in SQLite; `onRestarted` tells upstream to refresh.
 */
export class DaemonClient {
  private readonly opts: Required<DaemonClientOptions>;
  private info: DaemonInfo | null = null;
  private child: ChildProcess | null = null;
  private owned = false;
  private sseDispose: (() => void) | null = null;
  private stopped = false;
  private connecting = false;
  private restartTimer: NodeJS.Timeout | null = null;
  private healthCheckPending = false;
  private consecutiveFailures = 0;
  private firstFailureAt = 0;
  private readonly envelopeListeners = new Set<(envelope: any) => void>();
  private readonly restartListeners = new Set<(info: DaemonInfo) => void>();

  constructor(opts: DaemonClientOptions = {}) {
    this.opts = {
      onEnvelope: opts.onEnvelope ?? (() => {}),
      onStatus: opts.onStatus ?? (() => {}),
      onRestarted: opts.onRestarted ?? (() => {}),
      binaryPath: opts.binaryPath ?? daemonBinaryPath,
      spawnFn: opts.spawnFn ?? spawn,
      reachable: opts.reachable ?? daemonReachable,
      execFn: opts.execFn ?? defaultExecFile,
      killFn: opts.killFn ?? defaultKill,
      takeOverGraceMs: opts.takeOverGraceMs ?? TAKEOVER_GRACE_MS,
      takeOverEscalationMs: opts.takeOverEscalationMs ?? ESCALATION_STEP_MS,
      dataDir: opts.dataDir ?? daemonDataDir(),
      infoPath: opts.infoPath ?? daemonInfoPath(),
      backoffBaseMs: opts.backoffBaseMs ?? 1000,
      maxConsecutiveFailures: opts.maxConsecutiveFailures ?? 5,
      failureWindowMs: opts.failureWindowMs ?? 60_000,
    };
  }

  currentInfo(): DaemonInfo | null {
    return this.info;
  }

  isConnected(): boolean {
    return this.info !== null && !this.stopped;
  }

  /** Register an event listener (broadcasts once SSE is up; may register before connect without losing early events). */
  onEnvelope(callback: (envelope: any) => void): () => void {
    this.envelopeListeners.add(callback);
    return () => this.envelopeListeners.delete(callback);
  }

  /** Register the restart callback (session snapshots may be stale; upstream should refresh). */
  onRestarted(callback: (info: DaemonInfo) => void): () => void {
    this.restartListeners.add(callback);
    return () => this.restartListeners.delete(callback);
  }

  /** Connect to an existing daemon or spawn (idempotent: returns immediately if connecting/connected). */
  async connect(): Promise<void> {
    if (this.stopped) throw new Error('daemon client stopped');
    if (this.connecting || this.info) return;
    this.connecting = true;
    try {
      this.setStatus({ state: 'connecting' });
      const info = await this.obtainInfo();
      this.info = info;
      this.startSse(info);
      this.setStatus({ state: 'connected', info });
    } finally {
      this.connecting = false;
    }
  }

  /** Convenience invoke: connection faults auto-probe/restart; protocol errors propagate as-is. */
  async invoke(method: string, params: unknown): Promise<any> {
    const info = this.requireInfo();
    try {
      return await daemonInvoke(info, method, params);
    } catch (error) {
      this.handleInvokeError(error);
      throw error;
    }
  }

  async invokeRaw(request: unknown): Promise<ProtocolResponse> {
    const info = this.requireInfo();
    try {
      return await daemonInvokeRaw(info, request);
    } catch (error) {
      this.handleInvokeError(error);
      throw error;
    }
  }

  /** Explicit shutdown (`starriverd --shutdown`); no auto-restart afterwards. */
  async stop(): Promise<void> {
    this.stopped = true;
    this.clearRestartTimer();
    this.sseDispose?.();
    this.sseDispose = null;
    const binary = this.opts.binaryPath();
    if (!binary) throw new Error('starriverd binary not found');
    await new Promise<void>((resolve, reject) => {
      execFile(binary, ['--shutdown'], { cwd: this.opts.dataDir }, (error) => {
        if (error) reject(error);
        else resolve();
      });
    });
  }

  dispose(): void {
    // App exit: do not kill the daemon (shared service); only stop this client's watcher/connection.
    this.stopped = true;
    this.clearRestartTimer();
    this.sseDispose?.();
    this.sseDispose = null;
  }

  private async obtainInfo(): Promise<DaemonInfo> {
    // 1) Existing daemon.json → /health reachable → reuse — but only when the
    // running daemon IS this app's build. Otherwise (upgrade in place, or a
    // legacy daemon.json predating build_id) the client takes the daemon over:
    // a graceful `--shutdown`, then fall through to spawning the bundled
    // binary. Reuse-by-reachability alone silently pinned the app to whatever
    // old daemon happened to be running (missing features, misleading UI).
    let stale: { info: DaemonInfo; binary: string } | null = null;
    try {
      const raw = fs.readFileSync(this.opts.infoPath, 'utf8');
      const parsed = JSON.parse(raw) as DaemonInfo;
      if (
        typeof parsed.port === 'number' &&
        typeof parsed.token === 'string' &&
        (await this.opts.reachable(parsed))
      ) {
        const binary = this.opts.binaryPath();
        const bundled = bundledDaemonBuildId(binary);
        if (!bundled || parsed.build_id === bundled) {
          this.owned = false;
          this.child = null;
          return parsed;
        }
        stale = { info: parsed, binary: binary! };
      }
    } catch {
      // File missing/corrupt → spawn.
    }
    if (stale) {
      this.setStatus({
        state: 'restarting',
        attempt: 1,
        error: `daemon build mismatch (running ${stale.info.build_id || 'legacy/unknown'}, bundled ${bundledDaemonBuildId(stale.binary)}) — taking over`,
      });
      // Throws on refusal-to-die: must propagate, NOT fall through to spawn
      // (single-instance would yield the old daemon right back).
      await takeOverStaleDaemon({
        binary: stale.binary,
        info: stale.info,
        dataDir: this.opts.dataDir,
        infoPath: this.opts.infoPath,
        execFn: this.opts.execFn,
        killFn: this.opts.killFn,
        reachable: this.opts.reachable,
        waitClosedMs: this.opts.takeOverGraceMs,
        escalationMs: this.opts.takeOverEscalationMs,
      });
    }

    // 2) Spawn; the daemon runs with the data dir as cwd (without --config the session db lands there too).
    const binary = this.opts.binaryPath();
    if (!binary) {
      throw new Error(
        'starriverd binary not found — run `cargo build -p starriverd` (dev) or ship it in resources/ (packaged)',
      );
    }
    const child = this.opts.spawnFn(binary, [], {
      cwd: this.opts.dataDir,
      stdio: ['ignore', 'pipe', 'inherit'],
    });
    const info = await readStartupLine(child, 30_000).catch((error: unknown) => {
          // A half-started daemon holds the single-instance control socket —
          // leaving it alive wedges every later retry into `existing: true`.
          try { child.kill(); } catch { /* already gone */ }
          throw error;
        });
    if (info.existing) {
      // Existing instance: the child only prints connection info and exits; wait for it to be reaped.
      await new Promise<void>((resolve) => child.once('exit', () => resolve()));
      this.owned = false;
      this.child = null;
      return info;
    }
    this.owned = true;
    this.child = child;
    this.attachChildWatcher(child);
    return info;
  }

  private attachChildWatcher(child: ChildProcess): void {
    child.once('exit', (code, signal) => {
      this.child = null;
      if (this.stopped) return;
      this.teardownConnection();
      this.scheduleRestart(`daemon exited (code ${code}, signal ${signal ?? 'none'})`);
    });
  }

  private startSse(info: DaemonInfo): void {
    this.sseDispose?.();
    this.sseDispose = daemonEventStream(info, (envelope) => {
      for (const listener of this.envelopeListeners) listener(envelope);
    }, () => this.scheduleConnectionCheck());
  }

  private requireInfo(): DaemonInfo {
    if (!this.info) {
      throw Object.assign(new Error('daemon not connected'), { code: 'DAEMON_NOT_CONNECTED' });
    }
    return this.info;
  }

  private handleInvokeError(error: any): void {
      // Protocol errors (SESSION_BUSY / APPROVAL_NOT_PENDING …) are not connection faults and never restart.
    if (error?.isProtocolError) return;
    this.scheduleConnectionCheck();
  }

  private scheduleConnectionCheck(): void {
    if (this.stopped || this.connecting || this.healthCheckPending || !this.info) return;
    this.healthCheckPending = true;
    void this.opts.reachable(this.info)
      .then((alive) => {
        if (this.stopped || this.connecting) return;
        if (!alive) this.scheduleRestart('daemon unreachable after connection failure');
      })
      .finally(() => {
        this.healthCheckPending = false;
      });
  }

  private scheduleRestart(reason: string): void {
    if (this.stopped || this.connecting) return;
    const now = Date.now();
    if (now - this.firstFailureAt > this.opts.failureWindowMs) {
      this.consecutiveFailures = 0;
      this.firstFailureAt = now;
    }
    this.consecutiveFailures += 1;
    if (this.consecutiveFailures > this.opts.maxConsecutiveFailures) {
      this.setStatus({
        state: 'failed',
        error: `daemon crashed ${this.consecutiveFailures} times in a row — giving up`,
      });
      return;
    }
    const delay = Math.min(
      this.opts.backoffBaseMs * 2 ** (this.consecutiveFailures - 1),
      15_000,
    );
    this.setStatus({
      state: 'restarting',
      attempt: this.consecutiveFailures,
      error: reason,
    });
    this.clearRestartTimer();
    this.restartTimer = setTimeout(() => {
      this.restartTimer = null;
      void this.restart();
    }, delay);
  }

  private async restart(): Promise<void> {
    if (this.stopped) return;
    this.teardownConnection();
    try {
      // connect() manages its own connecting guard; do not preset it here or it will block itself.
      await this.connect();
      if (this.info) {
        for (const listener of this.restartListeners) listener(this.info);
        this.opts.onRestarted(this.info);
      }
    } catch (error) {
      this.scheduleRestart(error instanceof Error ? error.message : String(error));
    }
  }

  private teardownConnection(): void {
    this.sseDispose?.();
    this.sseDispose = null;
    this.info = null;
    // Do not kill the child: the exit event drives it; if the process is dead the watcher already fired.
  }

  private clearRestartTimer(): void {
    if (this.restartTimer) {
      clearTimeout(this.restartTimer);
      this.restartTimer = null;
    }
  }

  private setStatus(status: DaemonStatus): void {
    this.opts.onStatus(status);
  }
}
