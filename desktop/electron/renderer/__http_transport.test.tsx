// P4a: HttpSseTransport (browsers/third-party frontends reach the protocol via loopback HTTP+SSE).

import { afterEach, describe, expect, it, vi } from 'vitest';
import { EventEnvelope, HttpSseTransport, ProtocolRequest } from './client/client';

afterEach(() => {
  vi.restoreAllMocks();
  vi.useRealTimers();
});

describe('HttpSseTransport', () => {
  it('invokes over POST with bearer token', async () => {
    const fetchMock = vi.fn(async () =>
      new Response(JSON.stringify({ v: 1, id: 'r', ok: true, result: { ok: true } }), {
        status: 200,
        headers: { 'Content-Type': 'application/json' },
      }),
    );
    globalThis.fetch = fetchMock as unknown as typeof fetch;

    const transport = new HttpSseTransport('http://127.0.0.1:9999', 'tok');
    const request: ProtocolRequest = { v: 1, id: 'r', method: 'tools.list', params: {} };
    const response = await transport.invoke(request);
    expect(response.ok).toBe(true);
    const [url, init] = fetchMock.mock.calls[0];
    expect(url).toBe('http://127.0.0.1:9999/api/v1/invoke');
    expect((init as RequestInit).method).toBe('POST');
    expect((init as RequestInit).body).toBe(JSON.stringify(request));
    const headers = (init as RequestInit).headers as Record<string, string>;
    expect(headers.Authorization).toBe('Bearer tok');
  });

  it('invoke throws on non-OK status', async () => {
    const fetchMock = vi.fn(async () => new Response('unauthorized', { status: 401 }));
    globalThis.fetch = fetchMock as unknown as typeof fetch;

    const transport = new HttpSseTransport('http://127.0.0.1:9999', 'tok');
    const request: ProtocolRequest = { v: 1, id: 'r', method: 'tools.list', params: {} };
    await expect(transport.invoke(request)).rejects.toThrow('protocol HTTP 401');

    fetchMock.mockResolvedValueOnce(new Response('boom', { status: 500 }));
    await expect(transport.invoke(request)).rejects.toThrow('protocol HTTP 500');
  });

  it('malformed SSE frames are skipped without breaking the stream', () => {
    let instance: any = null;
    class FakeEventSource {
      onmessage: ((event: any) => void) | null = null;
      close = vi.fn();
      constructor(public url: string) {
        instance = this;
      }
    }
    (globalThis as any).EventSource = FakeEventSource;

    const transport = new HttpSseTransport('http://127.0.0.1:9999', 'tok');
    const events: EventEnvelope[] = [];
    const unsubscribe = transport.onEvent((event) => events.push(event));

    instance.onmessage({ data: 'not json {{{' });
    expect(events).toHaveLength(0);
    // Good frames after a bad frame must still arrive (onmessage catches and continues).
    instance.onmessage({
      data: JSON.stringify({
        v: 1,
        subscription: 'sub_1',
        seq: 1,
        type: 'turnStart',
        payload: {},
      }),
    });
    expect(events).toHaveLength(1);
    expect(events[0].type).toBe('turnStart');
    unsubscribe();
  });

  it('subscribes via EventSource with token query and closes', () => {
    let instance: any = null;
    class FakeEventSource {
      onmessage: ((event: any) => void) | null = null;
      close = vi.fn();
      constructor(public url: string) {
        instance = this;
      }
    }
    (globalThis as any).EventSource = FakeEventSource;

    const transport = new HttpSseTransport('http://127.0.0.1:9999', 'tok');
    const events: EventEnvelope[] = [];
    const unsubscribe = transport.onEvent((event) => events.push(event));

    expect(instance.url).toBe('http://127.0.0.1:9999/api/v1/events?token=tok');
    instance.onmessage({
      data: JSON.stringify({
        v: 1,
        subscription: 'sub_1',
        seq: 1,
        type: 'turnStart',
        payload: {},
      }),
    });
    expect(events).toHaveLength(1);
    expect(events[0].type).toBe('turnStart');

    unsubscribe();
    expect(instance.close).toHaveBeenCalled();
  });

  it('reconnects with after=lastGlobalSeq after a dropped connection', () => {
    const instances: any[] = [];
    class FakeEventSource {
      onmessage: ((event: any) => void) | null = null;
      onerror: (() => void) | null = null;
      close = vi.fn();
      constructor(public url: string) {
        instances.push(this);
      }
    }
    (globalThis as any).EventSource = FakeEventSource;
    vi.useFakeTimers();

    const transport = new HttpSseTransport('http://127.0.0.1:9999', 'tok');
    const events: EventEnvelope[] = [];
    const unsubscribe = transport.onEvent((event) => events.push(event));

    // The first connection receives the event (global_seq=1), then disconnects.
    instances[0].onmessage({
      data: JSON.stringify({
        v: 1,
        subscription: 's1',
        seq: 1,
        global_seq: 1,
        type: 'turnStart',
        payload: {},
      }),
    });
    expect(events).toHaveLength(1);
    instances[0].onerror();
    vi.advanceTimersByTime(1000);

    // The reconnect must carry after=1 (the server replays the missed events).
    expect(instances).toHaveLength(2);
    expect(instances[1].url).toBe('http://127.0.0.1:9999/api/v1/events?token=tok&after=1');

    unsubscribe();
  });
});
