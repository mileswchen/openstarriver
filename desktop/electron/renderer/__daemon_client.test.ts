// FPM P4b: daemon HTTP/SSE client unit tests (electron mocked; a real loopback HTTP server).

import { createServer, type Server } from 'http';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('electron', () => ({ app: { isPackaged: false } }));

import { daemonEventStream, daemonInvokeRaw, type DaemonInfo } from '../daemon';

function fakeInfo(port: number): DaemonInfo {
  return { port, token: 'tok-1', pid: 1, version: '0.1.0', protocolVersion: 1 };
}

describe('daemonInvokeRaw', () => {
  let server: Server;
  let port: number;
  let received: { path: string; auth: string | undefined; body: string } | null = null;

  beforeEach(async () => {
    server = createServer((req, res) => {
      let body = '';
      req.on('data', (c) => (body += c));
      req.on('end', () => {
        received = {
          path: req.url ?? '',
          auth: req.headers.authorization,
          body,
        };
        res.setHeader('Content-Type', 'application/json');
        res.end(JSON.stringify({ v: 1, id: 'r1', ok: true, result: { echo: 'hi' } }));
      });
    });
    await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve));
    port = (server.address() as any).port;
  });

  afterEach(() => new Promise<void>((resolve) => server.close(() => resolve())));

  it('posts to /api/v1/invoke with Bearer token and returns the full response', async () => {
    const response = await daemonInvokeRaw(fakeInfo(port), {
      v: 1,
      id: 'r1',
      method: 'tools.list',
      params: {},
    });
    expect(response).toEqual({ v: 1, id: 'r1', ok: true, result: { echo: 'hi' } });
    expect(received?.path).toBe('/api/v1/invoke');
    expect(received?.auth).toBe('Bearer tok-1');
    expect(JSON.parse(received!.body)).toEqual({ v: 1, id: 'r1', method: 'tools.list', params: {} });
  });

  it('rejects protocol errors with code attached', async () => {
    server.close();
    server = createServer((_req, res) => {
      res.setHeader('Content-Type', 'application/json');
      res.end(
        JSON.stringify({
          v: 1,
          id: 'r2',
          ok: false,
          error: { code: 'SESSION_BUSY', message: 'busy' },
        }),
      );
    });
    await new Promise<void>((resolve) => server.listen(port, '127.0.0.1', resolve));
    await expect(
      daemonInvokeRaw(fakeInfo(port), { v: 1, id: 'r2', method: 'session.execute', params: {} }),
    ).rejects.toMatchObject({ code: 'SESSION_BUSY', message: 'busy' });
  });
});

describe('daemonEventStream', () => {
  let server: Server;
  let port: number;

  beforeEach(async () => {
    server = createServer((req, res) => {
      if (req.url?.includes('token=tok-1')) {
        res.writeHead(200, {
          'Content-Type': 'text/event-stream',
          'Transfer-Encoding': 'chunked',
        });
        res.write('data: {"v":1,"subscription":"s1","seq":1,"type":"turnStart","payload":{}}\n\n');
        res.write('data: {"v":1,"subscription":"s1","seq":2,"type":"TextDelta","payload":{"text":"hi"}}\n\n');
        setTimeout(() => res.end(), 50);
      } else {
        res.writeHead(401);
        res.end();
      }
    });
    await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve));
    port = (server.address() as any).port;
  });

  afterEach(() => new Promise<void>((resolve) => server.close(() => resolve())));

  it('parses SSE frames into envelopes (chunked transfer decoded by node http)', async () => {
    const envelopes: any[] = [];
    const dispose = daemonEventStream(fakeInfo(port), (envelope) => envelopes.push(envelope));
    await new Promise<void>((resolve) =>
      setTimeout(() => {
        dispose();
        resolve();
      }, 200),
    );
    expect(envelopes).toEqual([
      { v: 1, subscription: 's1', seq: 1, type: 'turnStart', payload: {} },
      { v: 1, subscription: 's1', seq: 2, type: 'TextDelta', payload: { text: 'hi' } },
    ]);
  });

  it('reconnects with after=lastGlobalSeq when the server closes the stream', async () => {
    server.close();
    const seen: string[] = [];
    let connections = 0;
    server = createServer((req, res) => {
      connections += 1;
      seen.push(req.url ?? '');
      res.writeHead(200, {
        'Content-Type': 'text/event-stream',
        'Transfer-Encoding': 'chunked',
      });
      if (connections === 1) {
        res.write(
          'data: {"v":1,"subscription":"s1","seq":1,"global_seq":1,"type":"a","payload":{}}\n\n',
        );
        setTimeout(() => res.end(), 20); // server closes → triggers reconnect
      } else {
        res.write(
          'data: {"v":1,"subscription":"s1","seq":2,"global_seq":2,"type":"b","payload":{}}\n\n',
        );
      }
    });
    await new Promise<void>((resolve) => server.listen(port, '127.0.0.1', resolve));

    const envelopes: any[] = [];
    const dispose = daemonEventStream(fakeInfo(port), (envelope) => envelopes.push(envelope));
    // Wait for the 1s backoff reconnect + the second event to arrive.
    await new Promise<void>((resolve) => setTimeout(resolve, 2200));
    dispose();

    expect(envelopes.map((e) => e.type)).toEqual(['a', 'b']);
    expect(seen.length).toBeGreaterThanOrEqual(2);
    expect(seen[1]).toContain('after=1');
  });
});
