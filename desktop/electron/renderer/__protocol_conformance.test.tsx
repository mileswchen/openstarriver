// FPM design §8: protocol conformance — the same behavioral fixtures run over both the Ipc and HttpSse transports;
// asserting ProtocolClient behaves identically on both transports (envelope construction/response parsing/error
// mapping/subscription events).

import { describe, expect, it, vi } from 'vitest';
import {
  EventEnvelope,
  HttpSseTransport,
  IpcTransport,
  ProtocolClient,
  ProtocolResponse,
} from './client/client';

declare global {
  interface Window { starriver: any; }
}

/** Fake protocol backend: responds per method; `session.execute` streams back a subscription. */
function fakeDispatch(request: { id: string; method: string; params: any }): ProtocolResponse {
  if (request.method === 'tools.list') {
    return { v: 1, id: request.id, ok: true, result: { tools: ['read', 'bash'] } };
  }
  if (request.method === 'session.execute') {
    if (request.params?.task === 'busy') {
      return {
        v: 1, id: request.id, ok: false,
        error: { code: 'SESSION_BUSY', message: 'busy' },
      };
    }
    return { v: 1, id: request.id, ok: true, result: { subscription: 'sub_1' } };
  }
  return { v: 1, id: request.id, ok: true, result: {} };
}

function makeIpcHarness() {
  const onEventCb: ((event: EventEnvelope) => void)[] = [];
  window.starriver = {
    invoke: vi.fn(async (json: string) => {
      const request = JSON.parse(json);
      return JSON.stringify(fakeDispatch(request));
    }),
    onEvent: vi.fn((cb: (event: EventEnvelope) => void) => {
      onEventCb.push(cb);
      return () => {};
    }),
  };
  const client = new ProtocolClient(new IpcTransport());
  const emit = (event: EventEnvelope) => onEventCb.forEach((cb) => cb(event));
  const bodies = () =>
    (window.starriver.invoke as ReturnType<typeof vi.fn>).mock.calls.map((call: any[]) =>
      JSON.parse(call[0]),
    );
  return { client, emit, bodies };
}

function makeHttpHarness() {
  const fetchMock = vi.fn(async (url: string, init: any) => {
    const request = JSON.parse(init.body as string);
    return new Response(JSON.stringify(fakeDispatch(request)), {
      status: 200,
      headers: { 'Content-Type': 'application/json' },
    });
  });
  globalThis.fetch = fetchMock as unknown as typeof fetch;

  let source: any = null;
  class FakeEventSource {
    onmessage: ((event: any) => void) | null = null;
    close = vi.fn();
    constructor(public url: string) {
      source = this;
    }
  }
  (globalThis as any).EventSource = FakeEventSource;

  const client = new ProtocolClient(new HttpSseTransport('http://127.0.0.1:9999', 'tok'));
  const emit = (event: EventEnvelope) => source?.onmessage?.({ data: JSON.stringify(event) });
  const bodies = () =>
    fetchMock.mock.calls.map((call: any[]) => JSON.parse(call[1].body as string));
  return { client, emit, bodies };
}

function runConformance(make: () => { client: ProtocolClient; emit: (e: EventEnvelope) => void; bodies: () => any[] }) {
  it('calls a command and returns the result', async () => {
    const { client } = make();
    const result = await client.call('tools.list', {});
    expect(result).toEqual({ tools: ['read', 'bash'] });
  });

  it('builds unique protocol request ids per call', async () => {
    const { client, bodies } = make();
    await client.call('tools.list', {});
    await client.call('tools.list', {});
    const requests = bodies();
    const ids = requests.map((b) => b.id);
    expect(ids[0]).not.toBe(ids[1]);
    expect(requests[0].v).toBe(1);
    expect(requests[0].method).toBe('tools.list');
  });

  it('maps protocol errors onto thrown Errors with code', async () => {
    const { client } = make();
    await expect(client.call('session.execute', { task: 'busy' })).rejects.toMatchObject({
      code: 'SESSION_BUSY',
    });
  });

  it('streams subscription events through execute()', async () => {
    const { client, emit } = make();
    const events: EventEnvelope[] = [];
    const { subscription } = await client.execute('go', { onEvent: (e) => events.push(e) });
    expect(subscription).toBe('sub_1');
    emit({ v: 1, subscription: 'sub_1', seq: 1, type: 'turnStart', payload: {} });
    emit({ v: 1, subscription: 'sub_1', seq: 2, type: 'TextDelta', payload: { text: 'hi' } });
    expect(events.map((e) => e.type)).toEqual(['turnStart', 'TextDelta']);
  });
}

describe('protocol conformance — IpcTransport', () => {
  runConformance(makeIpcHarness);
});

describe('protocol conformance — HttpSseTransport', () => {
  runConformance(makeHttpHarness);
});
