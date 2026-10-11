// FPM P1 protocol client tests:
// 1) client.gen.ts matches protocol.schema.json (the Rust MethodRegistry is the source of truth);
// 2) ProtocolClient: envelope construction / error mapping / subscription event filtering and buffering.

import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { describe, expect, it, vi } from 'vitest';
import { generate } from '../scripts/gen-client.mjs';
import { PROTOCOL_METHODS, PROTOCOL_VERSION } from './client/client.gen';
import {
  EventEnvelope,
  ProtocolClient,
  ProtocolRequest,
  ProtocolResponse,
  Transport,
} from './client/client';

const rendererDir = join(__dirname);

describe('protocol client codegen', () => {
  it('client.gen.ts matches protocol.schema.json', () => {
    const schema = readFileSync(join(rendererDir, '..', 'protocol.schema.json'), 'utf8');
    const committed = readFileSync(join(rendererDir, 'client', 'client.gen.ts'), 'utf8');
    expect(generate(schema).trim()).toBe(committed.trim());
  });

  it('generated catalog mirrors schema methods', () => {
    const schema = JSON.parse(
      readFileSync(join(rendererDir, '..', 'protocol.schema.json'), 'utf8'),
    );
    expect(PROTOCOL_VERSION).toBe(schema.protocolVersion);
    expect(Object.keys(PROTOCOL_METHODS).sort()).toEqual(
      schema.methods.map((m: any) => m.name).sort(),
    );
    expect(PROTOCOL_METHODS['session.execute'].streaming).toBe(true);
    expect(PROTOCOL_METHODS['session.execute'].permissions).toContain('session:execute');
  });
});

describe('ProtocolClient', () => {
  function fakeTransport(respond: (request: ProtocolRequest) => ProtocolResponse) {
    const listeners = new Set<(event: EventEnvelope) => void>();
    const transport: Transport = {
      invoke: vi.fn(async (request: ProtocolRequest) => respond(request)),
      onEvent: vi.fn((callback: (event: EventEnvelope) => void) => {
        listeners.add(callback);
        return () => listeners.delete(callback);
      }),
    };
    const emit = (event: EventEnvelope) => listeners.forEach((cb) => cb(event));
    return { transport, emit };
  }

  it('builds a versioned envelope and returns the result', async () => {
    const seen: ProtocolRequest[] = [];
    const { transport } = fakeTransport((request) => {
      seen.push(request);
      return {
        v: 1,
        id: request.id,
        ok: true,
        result: { name: 'tools' },
      };
    });
    const client = new ProtocolClient(transport);
    const result = await client.call('tools.list', {});
    expect(result).toEqual({ name: 'tools' });
    expect(seen[0].v).toBe(PROTOCOL_VERSION);
    expect(seen[0].method).toBe('tools.list');
    expect(seen[0].id).toMatch(/^req_/);
    client.dispose();
  });

  it('throws a coded error when ok=false', async () => {
    const { transport } = fakeTransport((request) => ({
      v: 1,
      id: request.id,
      ok: false,
      error: { code: 'INVALID_PARAMS', message: 'task is required' },
    }));
    const client = new ProtocolClient(transport);
    await expect(client.call('session.execute', {})).rejects.toMatchObject({
      code: 'INVALID_PARAMS',
      message: 'task is required',
    });
    client.dispose();
  });

  it('surfaces a generic coded error when ok=false has no error body', async () => {
    const { transport } = fakeTransport(() => ({ v: 1, id: 'x', ok: false }));
    const client = new ProtocolClient(transport);
    await expect(client.call('session.execute', {})).rejects.toThrow('protocol error UNKNOWN');
    try {
      await client.call('session.execute', {});
    } catch (error: any) {
      expect(error.code).toBeUndefined();
      expect(error.data).toBeUndefined();
    }
    client.dispose();
  });

  it('rejects unknown methods before hitting the transport', async () => {
    const { transport } = fakeTransport(() => ({ v: 1, id: 'x', ok: true, result: null }));
    const client = new ProtocolClient(transport);
    await expect(client.call('nope.nope', {})).rejects.toMatchObject({
      code: 'METHOD_NOT_FOUND',
    });
    expect(transport.invoke).not.toHaveBeenCalled();
    client.dispose();
  });

  it('execute subscribes and filters events by subscription', async () => {
    const { transport, emit } = fakeTransport((request) => ({
      v: 1,
      id: request.id,
      ok: true,
      result: { subscription: 'sub_1' },
    }));
    const client = new ProtocolClient(transport);
    const events: EventEnvelope[] = [];
    const result = await client.execute('build it', {
      sessionId: 's1',
      onEvent: (event) => events.push(event),
    });
    expect(result.subscription).toBe('sub_1');

    // Events for the same subscription arrive; other subscriptions are filtered.
    emit({ v: 1, subscription: 'sub_1', seq: 1, type: 'turnStart', payload: {} });
    emit({ v: 1, subscription: 'sub_2', seq: 1, type: 'turnStart', payload: {} });
    expect(events).toHaveLength(1);
    expect(events[0].type).toBe('turnStart');
    client.dispose();
  });

  it('replays events buffered before subscribe', async () => {
    const { transport, emit } = fakeTransport((request) => ({
      v: 1,
      id: request.id,
      ok: true,
      result: { subscription: 'sub_1' },
    }));
    const client = new ProtocolClient(transport);
    await client.call('session.execute', { task: 'x' });
    // Events arriving before the subscription is established go into the buffer.
    emit({ v: 1, subscription: 'sub_1', seq: 1, type: 'toolStart', payload: {} });
    const events: EventEnvelope[] = [];
    client.subscribe('sub_1', (event) => events.push(event));
    expect(events).toHaveLength(1);
    expect(events[0].type).toBe('toolStart');
    client.dispose();
  });

  it('unsubscribe stops delivery and clears subscription state', async () => {
    const { transport, emit } = fakeTransport((request) => ({
      v: 1,
      id: request.id,
      ok: true,
      result: { subscription: 'sub_1' },
    }));
    const client = new ProtocolClient(transport);
    await client.call('session.execute', { task: 'x' });
    emit({ v: 1, subscription: 'sub_1', seq: 1, type: 'toolStart', payload: {} });

    const events: EventEnvelope[] = [];
    const unsubscribe = client.subscribe('sub_1', (event) => events.push(event));
    expect(events).toHaveLength(1); // buffered replay
    unsubscribe();

    emit({ v: 1, subscription: 'sub_1', seq: 2, type: 'turnEnd', payload: {} });
    expect(events).toHaveLength(1, 'after unsubscribe no further delivery');
    client.dispose();
  });
});
