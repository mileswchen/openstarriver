// Daemon build-identity takeover: a reachable daemon whose build_id mismatches
// (or predates) the bundled binary must be gracefully replaced, never silently
// reused. Covers: matching id → reuse; legacy/missing id → takeover; unreadable
// bundled binary → skip check + reuse; refusal to die → loud failure.

import { EventEmitter } from 'events';
import { createHash } from 'crypto';
import * as fs from 'fs';
import * as os from 'os';
import * as path from 'path';
import { createServer, type Server } from 'http';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('electron', () => ({ app: { isPackaged: false } }));

import {
  bundledDaemonBuildId,
  DaemonClient,
  daemonReachable,
  takeOverStaleDaemon,
  type DaemonInfo,
  type DaemonStatus,
} from '../daemon';

function hashOf(file: string): string {
  return createHash('sha256').update(fs.readFileSync(file)).digest('hex').slice(0, 16);
}

interface FakeChild {
  child: any;
  stdout: EventEmitter;
  announce: () => void;
}
function fakeChild(info: DaemonInfo): FakeChild {
  const stdout = new EventEmitter();
  const child: any = new EventEmitter();
  child.stdout = stdout;
  child.kill = vi.fn();
  const announce = () => stdout.emit('data', JSON.stringify({ ...info, existing: false }) + '\n');
  return { child, stdout, announce };
}

async function makeServer(): Promise<{ server: Server; port: number; close: () => Promise<void> }> {
  const server = createServer((req, res) => {
    if (req.url?.startsWith('/health')) {
      res.writeHead(200, { 'Content-Type': 'text/plain' });
      res.end('ok');
      return;
    }
    if (req.url?.startsWith('/api/v1/events')) {
      res.writeHead(200, { 'Content-Type': 'text/event-stream', 'Transfer-Encoding': 'chunked' });
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

describe('bundledDaemonBuildId', () => {
  it('returns the sha256 prefix of the binary', () => {
    const file = path.join(os.tmpdir(), `starriverd-id-${Date.now()}`);
    fs.writeFileSync(file, 'build identity');
    expect(bundledDaemonBuildId(file)).toBe(hashOf(file));
    fs.rmSync(file, { force: true });
  });

  it('returns empty for unreadable/null binary (check skipped downstream)', () => {
    expect(bundledDaemonBuildId(null)).toBe('');
    expect(bundledDaemonBuildId('/definitely/not/here/starriverd')).toBe('');
  });
});

describe('DaemonClient build takeover', () => {
  let tmp: string;
  let infoPath: string;
  let binaryPath: string;
  let spawnCount: number;
  let shutdownCalls: string[][];

  beforeEach(() => {
    tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'daemon-takeover-'));
    infoPath = path.join(tmp, 'daemon.json');
    binaryPath = path.join(tmp, 'starriverd-fake');
    fs.writeFileSync(binaryPath, 'fake daemon binary');
    spawnCount = 0;
    shutdownCalls = [];
  });

  afterEach(() => {
    fs.rmSync(tmp, { recursive: true, force: true });
  });

  function writeInfoFile(info: DaemonInfo): void {
    fs.writeFileSync(infoPath, JSON.stringify(info));
  }

  function makeClient(spawnQueue: FakeChild[]) {
    return new DaemonClient({
      binaryPath: () => binaryPath,
      spawnFn: (() => {
        spawnCount += 1;
        const fake = spawnQueue.shift()!;
        setImmediate(() => fake.announce());
        return fake.child;
      }) as any,
      reachable: daemonReachable,
      dataDir: tmp,
      infoPath,
      execFn: (_file, args, _opts, cb) => {
        shutdownCalls.push(args);
        cb(null);
      },
      killFn: vi.fn(),
      backoffBaseMs: 1,
      maxConsecutiveFailures: 3,
      failureWindowMs: 60_000,
    });
  }

  it('reuses a reachable daemon whose build_id matches the bundled binary', async () => {
    const old = await makeServer();
    writeInfoFile({ port: old.port, token: 't-old', pid: 1, build_id: hashOf(binaryPath) });
    const client = makeClient([]);
    await client.connect();
    expect(client.currentInfo()?.port).toBe(old.port);
    expect(shutdownCalls).toEqual([]);
    expect(spawnCount).toBe(0);
    client.dispose();
    await old.close();
  });

  it('takes over a legacy daemon (daemon.json without build_id) and spawns the bundled binary', async () => {
    const old = await makeServer();
    writeInfoFile({ port: old.port, token: 't-old', pid: 1 }); // legacy: no build_id
    const fresh = await makeServer();
    const child = fakeChild({
      port: fresh.port,
      token: 't-new',
      pid: 2,
      build_id: hashOf(binaryPath),
    });
    const client = makeClient([child]);
    // The old daemon "dies" when its control socket receives --shutdown.
    const originalExec = client['opts'].execFn;
    client['opts'].execFn = (file, args, opts, cb) => {
      originalExec(file, args, opts, cb);
      setTimeout(() => void old.close(), 20);
    };
    await client.connect();
    expect(shutdownCalls).toEqual([['--shutdown']]);
    expect(spawnCount).toBe(1);
    expect(client.currentInfo()?.port).toBe(fresh.port);
    expect(client.currentInfo()?.build_id).toBe(hashOf(binaryPath));
    expect(fs.existsSync(infoPath)).toBe(false); // old daemon.json cleaned
    client.dispose();
    await fresh.close();
  });

  it('takes over on a build_id mismatch (upgraded in place)', async () => {
    const old = await makeServer();
    writeInfoFile({ port: old.port, token: 't-old', pid: 1, build_id: 'deadbeefdeadbeef' });
    const fresh = await makeServer();
    const child = fakeChild({
      port: fresh.port,
      token: 't-new',
      pid: 2,
      build_id: hashOf(binaryPath),
    });
    const client = makeClient([child]);
    const statuses: DaemonStatus[] = [];
    client['opts'].onStatus = (status) => statuses.push(status);
    const originalExec = client['opts'].execFn;
    client['opts'].execFn = (file, args, opts, cb) => {
      originalExec(file, args, opts, cb);
      setTimeout(() => void old.close(), 20);
    };
    await client.connect();
    expect(client.currentInfo()?.port).toBe(fresh.port);
    expect(
      statuses.some(
        (status) => status.state === 'restarting' && status.error.includes('build mismatch'),
      ),
    ).toBe(true);
    client.dispose();
    await fresh.close();
  });

  it('skips the identity check when the bundled binary is unreadable (reuse)', async () => {
    const old = await makeServer();
    writeInfoFile({ port: old.port, token: 't-old', pid: 1 }); // no build_id
    const client = makeClient([]);
    client['opts'].binaryPath = () => path.join(tmp, 'missing-binary');
    await client.connect();
    expect(client.currentInfo()?.port).toBe(old.port);
    expect(shutdownCalls).toEqual([]);
    expect(spawnCount).toBe(0);
    client.dispose();
    await old.close();
  });

  it('fails loudly when the stale daemon refuses to die (no silent spawn)', async () => {
    const old = await makeServer();
    writeInfoFile({ port: old.port, token: 't-old', pid: 999999, build_id: 'aaaaaaaaaaaaaaaa' });
    const client = makeClient([]); // empty spawn queue: a spawn attempt would crash the test
    client['opts'].reachable = async () => true; // never goes down
    client['opts'].execFn = (_file, _args, _opts, cb) => cb(null); // shutdown "succeeds", daemon ignores it
    client['opts'].takeOverGraceMs = 300;
    client['opts'].takeOverEscalationMs = 150;
    await expect(client.connect()).rejects.toThrow(/pkill -f starriverd/);
    expect(spawnCount).toBe(0);
    client.dispose();
    await old.close();
  });
});

describe('takeOverStaleDaemon (unit, short timeouts)', () => {
  it('escalates SIGTERM → SIGKILL then throws with a manual-fix hint', async () => {
    const kills: string[] = [];
    let probeCount = 0;
    await expect(
      takeOverStaleDaemon({
        binary: '/fake/starriverd',
        info: { port: 1, token: 't', pid: 12345 },
        dataDir: os.tmpdir(),
        infoPath: path.join(os.tmpdir(), 'no-such-daemon.json'),
        execFn: (_file, _args, _opts, cb) => cb(null),
        killFn: (_pid, signal) => kills.push(signal),
        reachable: async () => {
          probeCount += 1;
          return true; // daemon never dies
        },
        waitClosedMs: 300,
        escalationMs: 200,
      }),
    ).rejects.toThrow(/pkill -f starriverd/);
    expect(kills).toEqual(['SIGTERM', 'SIGKILL']);
    expect(probeCount).toBeGreaterThanOrEqual(3);
  });

  it('succeeds when the port closes after the graceful shutdown', async () => {
    let alive = true;
    await takeOverStaleDaemon({
      binary: '/fake/starriverd',
      info: { port: 2, token: 't', pid: 1 },
      dataDir: os.tmpdir(),
      infoPath: path.join(os.tmpdir(), 'no-such-daemon.json'),
      execFn: (_file, _args, _opts, cb) => {
        setTimeout(() => {
          alive = false;
          cb(null);
        }, 50);
      },
      killFn: () => {},
      reachable: async () => alive,
      waitClosedMs: 2000,
      escalationMs: 200,
    });
  });
});
