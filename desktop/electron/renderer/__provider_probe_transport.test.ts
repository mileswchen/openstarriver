// Transport-level defensive handlers in providerProbe.ts that a real local
// server cannot deterministically reach:
//  - the socket 'timeout' handler races the overall deadline (BUG-P13) and
//    only fires on pure inactivity, so a real drip server hits the deadline
//    first;
//  - the inner catch only triggers when http.get throws SYNCHRONOUSLY, which
//    a validated URL never does over a real socket;
//  - the `e?.message || String(e)` fallbacks only fire when the transport
//    emits a NON-Error payload, which Node's real sockets never do;
//  - the https branch needs a TLS endpoint.
// All are exercised here by mocking http.get / https.get (the probe imports
// them as ESM `import * as`, which vi.mock intercepts). The anthropic branch
// issues a single request, keeping each assertion unambiguous.
import { EventEmitter } from 'events';
import { afterEach, describe, expect, it, vi } from 'vitest';

const { mockGet, mockGetHttps } = vi.hoisted(() => ({ mockGet: vi.fn(), mockGetHttps: vi.fn() }));

vi.mock('http', async (importOriginal) => {
  const actual = await importOriginal<typeof import('http')>();
  return { ...actual, get: (...args: any[]) => mockGet(...args) };
});
vi.mock('https', async (importOriginal) => {
  const actual = await importOriginal<typeof import('https')>();
  return { ...actual, get: (...args: any[]) => mockGetHttps(...args) };
});

import { fetchProviderModels } from '../providerProbe';

afterEach(() => {
  mockGet.mockReset();
  mockGetHttps.mockReset();
});

describe('provider probe — transport defensive handlers', () => {
  it('socket inactivity timeout surfaces a timed-out error and destroys the request', async () => {
    const destroy = vi.fn();
    mockGet.mockImplementation((_url: any, _opts: any, _cb: any) => {
      const req: any = new EventEmitter();
      req.destroy = destroy;
      process.nextTick(() => req.emit('timeout'));
      return req;
    });
    const out = await fetchProviderModels('http://127.0.0.1:1', 'sk', undefined, 'anthropic', 10_000);
    expect(out.ok).toBe(false);
    expect(out.error).toMatch(/Request timed out \(10s\)/);
    expect(destroy).toHaveBeenCalled();
  });

  it('a synchronous transport throw is caught and surfaced as the error', async () => {
    mockGet.mockImplementation(() => {
      throw new Error('boom');
    });
    const out = await fetchProviderModels('http://127.0.0.1:1', 'sk', undefined, 'anthropic', 10_000);
    expect(out.ok).toBe(false);
    expect(out.error).toBe('boom');
  });

  it('a non-Error synchronous throw falls back to String(e)', async () => {
    mockGet.mockImplementation(() => {
      throw 'raw-transport-error';
    });
    const out = await fetchProviderModels('http://127.0.0.1:1', 'sk', undefined, 'anthropic', 10_000);
    expect(out.ok).toBe(false);
    expect(out.error).toBe('raw-transport-error');
  });

  it('a request error with a non-Error payload falls back to String(e)', async () => {
    mockGet.mockImplementation((_url: any, _opts: any, _cb: any) => {
      const req: any = new EventEmitter();
      req.destroy = vi.fn();
      process.nextTick(() => req.emit('error', 'raw-req-error'));
      return req;
    });
    const out = await fetchProviderModels('http://127.0.0.1:1', 'sk', undefined, 'anthropic', 10_000);
    expect(out.ok).toBe(false);
    expect(out.error).toBe('raw-req-error');
  });

  it('a response error with a non-Error payload falls back to String(e)', async () => {
    mockGet.mockImplementation((_url: any, _opts: any, cb: any) => {
      const req: any = new EventEmitter();
      req.destroy = vi.fn();
      const res: any = new EventEmitter();
      res.statusCode = 200;
      res.headers = {};
      res.setEncoding = vi.fn();
      res.resume = vi.fn();
      process.nextTick(() => {
        cb(res);
        process.nextTick(() => res.emit('error', 'raw-res-error'));
      });
      return req;
    });
    const out = await fetchProviderModels('http://127.0.0.1:1', 'sk', undefined, 'anthropic', 10_000);
    expect(out.ok).toBe(false);
    expect(out.error).toBe('raw-res-error');
  });

  it('an https:// base selects the https transport', async () => {
    mockGetHttps.mockImplementation(() => {
      throw new Error('tls-boom');
    });
    const out = await fetchProviderModels('https://example.test', 'sk', undefined, 'anthropic', 10_000);
    expect(out.ok).toBe(false);
    expect(out.error).toBe('tls-boom');
    expect(mockGetHttps).toHaveBeenCalled();
    expect(mockGet).not.toHaveBeenCalled();
  });

  it('a stream error while reading a non-2xx error body yields the bare status', async () => {
    mockGet.mockImplementation((_url: any, _opts: any, cb: any) => {
      const req: any = new EventEmitter();
      req.destroy = vi.fn();
      const res: any = new EventEmitter();
      res.statusCode = 503;
      res.headers = {};
      res.setEncoding = vi.fn();
      res.resume = vi.fn();
      process.nextTick(() => {
        cb(res);
        process.nextTick(() => res.emit('error', new Error('stream-cut')));
      });
      return req;
    });
    const out = await fetchProviderModels('http://127.0.0.1:1', 'sk', undefined, 'anthropic', 10_000);
    expect(out.ok).toBe(false);
    expect(out.error).toBe('HTTP 503');
  });
});