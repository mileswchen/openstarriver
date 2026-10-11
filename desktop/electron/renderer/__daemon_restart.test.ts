// FPM P4b: DaemonClient process-level crash auto-restart.
// Covers: abnormal child exit → backoff restart (new port/token), the crash-loop cap entering
// failed, invoke connection failures trigger a liveness restart, protocol errors do not.

import { EventEmitter } from 'events';
import { createServer, type Server } from 'http';
import * as os from 'os';
import * as path from 'path';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('electron', () => ({ app: { isPackaged: false } }));

import { DaemonClient, daemonReachable, type DaemonInfo, type DaemonStatus } from '../daemon';

interface FakeChild {
  child: any;
  stdout: EventEmitter;
  announce: () => void;
  emitExit: (code?: number) => void;
}

function fakeChild(info: DaemonInfo): FakeChild {
  const stdout = new EventEmitter();
  const child: any = new EventEmitter();
  child.stdout = stdout;
  child.kill = vi.fn();
  const announce = () =>
    stdout.emit('data', JSON.stringify({ ...info, existing: false }) + '\n');
  const emitExit = (code = 0) => child.emit('exit', code, null);
  return { child, stdout, announce, emitExit };
}

async function makeServer(
  handler?: (req: any, body: string) => { status: number; body: string },
): Promise<{ server: Server; port: number; close: () => Promise<void> }> {
  const server = createServer((req, res) => {
    if (req.url?.startsWith('/health')) {
      res.writeHead(200, { 'Content-Type': 'text/plain' });
      res.end('ok');
      return;
    }
    if (req.url?.startsWith('/api/v1/invoke')) {
      let body = '';
      req.on('data', (c) => (body += c));
      req.on('end', () => {
        const out = handler
          ? handler(req, body)
          : { status: 200, body: JSON.stringify({ v: 1, id: 'r', ok: true, result: {} }) };
        res.writeHead(out.status, { 'Content-Type': 'application/json' });
        res.end(out.body);
      });
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
      // An open SSE connection keeps server.close() from ever calling back; cut active connections first.
      (server as any).closeAllConnections?.();
      return new Promise<void>((resolve) => server.close(() => resolve()));
    },
  };
}

async function waitFor(cond: () => boolean, timeoutMs = 4000): Promise<void> {
  const start = Date.now();
  while (!cond()) {
    if (Date.now() - start > timeoutMs) throw new Error('waitFor timeout');
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
}

describe('DaemonClient crash restart', () => {
  let tmp: string;
  let infoPath: string;
  let spawnQueue: FakeChild[];
  let spawnCount: number;

  beforeEach(() => {
    tmp = fs_mkdtemp();
    infoPath = path.join(tmp, 'daemon.json');
    spawnQueue = [];
    spawnCount = 0;
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  function makeClient(overrides: Partial<ConstructorParameters<typeof DaemonClient>[0]> = {}) {
    return new DaemonClient({
      binaryPath: () => '/fake/starriverd',
      spawnFn: (() => {
        spawnCount += 1;
        const fake = spawnQueue.shift()!;
        setImmediate(() => fake.announce());
        return fake.child;
      }) as any,
      reachable: daemonReachable,
      dataDir: tmp,
      infoPath,
      backoffBaseMs: 1,
      maxConsecutiveFailures: 3,
      failureWindowMs: 60_000,
      ...overrides,
    });
  }

  it('restarts with a new daemon after the child crashes', async () => {
    const s1 = await makeServer();
    const child1 = fakeChild({ port: s1.port, token: 't1', pid: 1 });
    spawnQueue.push(child1);
    const restarted = vi.fn();
    const client = makeClient();
    client.onRestarted(restarted);

    await client.connect();
    expect(client.currentInfo()?.port).toBe(s1.port);
    expect(spawnCount).toBe(1);

    // Stage the new daemon first (crash backoff is 1ms; the queue must already have content when restart fires).
    const s2 = await makeServer();
    spawnQueue.push(fakeChild({ port: s2.port, token: 't2', pid: 2 }));

    // Daemon crash: child exits + the old port unreachable (simulating process death).
    child1.emitExit(1);
    await s1.close();

    await waitFor(() => client.currentInfo()?.port === s2.port);

    expect(spawnCount).toBe(2);
    expect(client.currentInfo()?.token).toBe('t2');
    expect(restarted).toHaveBeenCalledTimes(1);
    expect(restarted.mock.calls[0][0].port).toBe(s2.port);
    await s2.close();
    client.dispose();
  });

  it('gives up after repeated fast crashes (crash loop → failed)', async () => {
    // Every spawned daemon crashes immediately (exits right after announce).
    for (let i = 1; i <= 8; i += 1) {
      const server = await makeServer();
      const child = fakeChild({ port: server.port, token: `t${i}`, pid: i });
      const originalAnnounce = child.announce;
      child.announce = () => {
        originalAnnounce();
        setImmediate(() => child.emitExit(1));
      };
      spawnQueue.push(child);
    }
    const statuses: DaemonStatus[] = [];
    const client = makeClient({ onStatus: (status) => statuses.push(status) });

    await client.connect();
    await waitFor(() => statuses.some((status) => status.state === 'failed'));

    expect(statuses.at(-1)?.state).toBe('failed');
    // max=3: give up after the 4th crash (1+1+1+1 restart attempts, the last one failed).
    expect(spawnCount).toBeLessThanOrEqual(5);
    client.dispose();
  });

  it('probes health and restarts when invoke hits a connection error', async () => {
    const s1 = await makeServer();
    const child1 = fakeChild({ port: s1.port, token: 't1', pid: 1 });
    spawnQueue.push(child1);
    const client = makeClient();
    await client.connect();

    // Daemon dead: port closed → invoke throws a connection error → liveness fails → restart.
    const s2 = await makeServer();
    spawnQueue.push(fakeChild({ port: s2.port, token: 't2', pid: 2 }));
    await s1.close();
    await expect(client.invoke('tools.list', {})).rejects.toThrow();
    await waitFor(() => client.currentInfo()?.port === s2.port);
    expect(spawnCount).toBe(2);
    await s2.close();
    client.dispose();
  });

  it('does not restart on protocol errors (SESSION_BUSY etc.)', async () => {
    const s1 = await makeServer((_req, _body) => ({
      status: 200,
      body: JSON.stringify({
        v: 1,
        id: 'r',
        ok: false,
        error: { code: 'SESSION_BUSY', message: 'busy' },
      }),
    }));
    spawnQueue.push(fakeChild({ port: s1.port, token: 't1', pid: 1 }));
    const client = makeClient();
    await client.connect();

    await expect(client.invoke('session.execute', { task: 'x' })).rejects.toMatchObject({
      code: 'SESSION_BUSY',
    });
    // No restart triggered.
    await new Promise((resolve) => setTimeout(resolve, 80));
    expect(spawnCount).toBe(1);
    await s1.close();
    client.dispose();
  });
});

function fs_mkdtemp(): string {
  const fs = require('fs') as typeof import('fs');
  return fs.mkdtempSync(path.join(os.tmpdir(), 'daemon-restart-'));
}
