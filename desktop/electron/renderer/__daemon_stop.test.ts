// stopDaemonAndCleanup: explicit user stop ("停止后台服务并退出" / macOS 卸载).
// No build-identity check — the daemon stops regardless of build. Covers:
// nothing running; stale info file (port closed); graceful --shutdown success;
// refusal → SIGTERM/SIGKILL escalation + manual hint; no bundled binary →
// pid escalation only.

import * as fs from 'fs';
import * as os from 'os';
import * as path from 'path';
import { createServer, type Server } from 'http';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('electron', () => ({ app: { isPackaged: false } }));

import { daemonReachable, stopDaemonAndCleanup, type DaemonInfo } from '../daemon';

async function makeServer(): Promise<{ server: Server; port: number; close: () => Promise<void> }> {
  const server = createServer((req, res) => {
    if (req.url?.startsWith('/health')) {
      res.writeHead(200, { 'Content-Type': 'text/plain' });
      res.end('ok');
      return;
    }
    res.writeHead(404);
    res.end();
  });
  await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve));
  const port = (server.address() as any).port;
  return {
    server,
    port,
    close: () => {
      (server as any).closeAllConnections?.();
      return new Promise<void>((resolve) => server.close(() => resolve()));
    },
  };
}

describe('stopDaemonAndCleanup', () => {
  let tmp: string;
  let infoPath: string;
  let binaryPath: string;

  beforeEach(() => {
    tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'daemon-stop-'));
    infoPath = path.join(tmp, 'daemon.json');
    binaryPath = path.join(tmp, 'starriverd-fake');
    fs.writeFileSync(binaryPath, 'fake daemon binary');
  });

  afterEach(() => {
    fs.rmSync(tmp, { recursive: true, force: true });
  });

  it('reports nothing-to-do when no info file exists', async () => {
    const result = await stopDaemonAndCleanup({ infoPath });
    expect(result).toEqual({ stopped: false, confirmed: true });
  });

  it('cleans a stale info file whose port is already closed', async () => {
    fs.writeFileSync(
      infoPath,
      JSON.stringify({ port: 1, token: 't', pid: 1 } satisfies DaemonInfo),
    );
    const result = await stopDaemonAndCleanup({ infoPath });
    expect(result).toEqual({ stopped: false, confirmed: true });
    expect(fs.existsSync(infoPath)).toBe(false);
  });

  it('gracefully stops a reachable daemon and removes the info file', async () => {
    const live = await makeServer();
    const info: DaemonInfo = { port: live.port, token: 't', pid: 1 };
    fs.writeFileSync(infoPath, JSON.stringify(info));
    const shutdownCalls: string[][] = [];
    const result = await stopDaemonAndCleanup({
      infoPath,
      binaryPath,
      execFn: (_file, args, _opts, cb) => {
        shutdownCalls.push(args);
        setTimeout(() => void live.close(), 20);
        cb(null);
      },
      killFn: () => {
        throw new Error('escalation must not run on graceful success');
      },
      waitClosedMs: 2000,
    });
    expect(shutdownCalls).toEqual([['--shutdown']]);
    expect(result).toEqual({ stopped: true, confirmed: true });
    expect(fs.existsSync(infoPath)).toBe(false);
  });

  it('escalates with the manual hint when the daemon refuses to die', async () => {
    const live = await makeServer();
    const info: DaemonInfo = { port: live.port, token: 't', pid: 999999 };
    fs.writeFileSync(infoPath, JSON.stringify(info));
    const kills: string[] = [];
    const result = await stopDaemonAndCleanup({
      infoPath,
      binaryPath,
      execFn: (_file, _args, _opts, cb) => cb(null), // shutdown "sent", daemon ignores it
      killFn: (_pid, signal) => kills.push(signal),
      reachable: (i) => daemonReachable(i, 200), // stays up
      waitClosedMs: 300,
      escalationMs: 200,
    });
    await live.close();
    expect(kills).toEqual(['SIGTERM', 'SIGKILL']);
    expect(result.stopped).toBe(true);
    expect(result.confirmed).toBe(false);
    expect(result.hint).toBe('pkill -f starriverd');
    // A refusing daemon keeps its info file — the next launch will retry the takeover.
    expect(fs.existsSync(infoPath)).toBe(true);
  });

  it('pid escalation alone when no bundled binary exists', async () => {
    const live = await makeServer();
    const info: DaemonInfo = { port: live.port, token: 't', pid: 424242 };
    fs.writeFileSync(infoPath, JSON.stringify(info));
    const kills: string[] = [];
    const result = await stopDaemonAndCleanup({
      infoPath,
      binaryPath: null,
      killFn: (_pid, signal) => {
        kills.push(signal);
        setTimeout(() => void live.close(), 20);
      },
      waitClosedMs: 1000,
      escalationMs: 1000,
    });
    expect(kills).toEqual(['SIGTERM']);
    expect(result).toEqual({ stopped: true, confirmed: true });
    expect(fs.existsSync(infoPath)).toBe(false);
  });
});
