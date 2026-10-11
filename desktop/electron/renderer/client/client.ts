// FPM P1 protocol runtime client: build envelopes, dispatch through the IPC transport,
// multiplex events per subscription (with pre-subscription buffering to avoid races).
// Types come from the generated client.gen.ts, whose source of truth is the Rust-side

import { PROTOCOL_VERSION, PROTOCOL_METHODS } from './client.gen';

export interface EventEnvelope {
  v: number;
  subscription: string;
  seq: number;
  /** Server-wide global event seq (reconnect replay); older servers leave it unset. */
  global_seq?: number;
  type: string;
  payload: any;
}

export interface ProtocolErrorBody {
  code: string;
  message: string;
  data?: any;
}

export interface ProtocolRequest {
  v: number;
  id: string;
  method: string;
  params: unknown;
}

export interface ProtocolResponse {
  v: number;
  id: string;
  ok: boolean;
  result?: any;
  error?: ProtocolErrorBody;
}

export interface Transport {
  invoke(request: ProtocolRequest): Promise<ProtocolResponse>;
  onEvent(callback: (event: EventEnvelope) => void): () => void;
}

/** Electron built-in IPC transport (the preload's generic bridge). */
export class IpcTransport implements Transport {
  invoke(request: ProtocolRequest): Promise<ProtocolResponse> {
    return window.starriver
      .invoke(JSON.stringify(request))
      .then((text: string) => JSON.parse(text) as ProtocolResponse);
  }

  onEvent(callback: (event: EventEnvelope) => void): () => void {
    return window.starriver.onEvent((event: any) => callback(event as EventEnvelope));
  }
}

/** P4a: loopback HTTP + SSE transport — browsers/third-party fronts use the same protocol client. */
export class HttpSseTransport implements Transport {
  constructor(
    private readonly baseUrl: string,
    private readonly token: string,
  ) {}

  async invoke(request: ProtocolRequest): Promise<ProtocolResponse> {
    const response = await fetch(`${this.baseUrl}/api/v1/invoke`, {
      method: 'POST',
      headers: {
        'Content-Type': 'application/json',
        Authorization: `Bearer ${this.token}`,
      },
      body: JSON.stringify(request),
    });
    if (!response.ok) {
      throw new Error(`protocol HTTP ${response.status}`);
    }
    return (await response.json()) as ProtocolResponse;
  }

  onEvent(callback: (event: EventEnvelope) => void): () => void {
    // EventSource cannot set custom headers; the token rides the query string (loopback + short-lived).
    // Auto-reconnect: send the largest received global_seq (`after=`) so the server replays
    // missed events, avoiding gaps after reconnect; exponential backoff 1s→2s→…→15s.
    let source: EventSource | null = null;
    let closed = false;
    let retry = 0;
    let lastGlobalSeq: number | undefined;

    const connect = () => {
      if (closed) return;
      const url = new URL(`${this.baseUrl}/api/v1/events`);
      url.searchParams.set('token', this.token);
      if (lastGlobalSeq !== undefined) {
        url.searchParams.set('after', String(lastGlobalSeq));
      }
      source = new EventSource(url.toString());
      source.onmessage = (message) => {
        retry = 0;
        let envelope: EventEnvelope;
        try {
          envelope = JSON.parse(message.data) as EventEnvelope;
        } catch {
          return;
        }
        if (typeof envelope.global_seq === 'number') {
          lastGlobalSeq = envelope.global_seq;
        }
        callback(envelope);
      };
      source.onerror = () => {
        source?.close();
        source = null;
        const delay = Math.min(1000 * 2 ** retry, 15_000);
        retry += 1;
        setTimeout(connect, delay);
      };
    };
    connect();
    return () => {
      closed = true;
      source?.close();
    };
  }
}

export interface ExecuteOptions {
  sessionId?: string;
  onEvent?: (event: EventEnvelope) => void;
}

export class ProtocolClient {
  private seq = 0;
  private readonly listeners = new Map<string, Set<(event: EventEnvelope) => void>>();
  private readonly buffers = new Map<string, EventEnvelope[]>();
  private readonly unsubscribeEvents: () => void;

  constructor(private readonly transport: Transport) {
    this.unsubscribeEvents = transport.onEvent((envelope) => {
      const hasListeners = (this.listeners.get(envelope.subscription)?.size ?? 0) > 0;
      if (hasListeners) {
        // The buffer only serves pre-subscription events: with an active subscriber events
        // are already delivered live, and buffering again would double-replay them to late subscribers.
        const set = this.listeners.get(envelope.subscription);
        set?.forEach((callback) => callback(envelope));
        return;
      }
      let buffer = this.buffers.get(envelope.subscription);
      if (!buffer) {
        buffer = [];
        this.buffers.set(envelope.subscription, buffer);
      }
      if (buffer.length >= 1024) buffer.shift();
      buffer.push(envelope);
    });
  }

  dispose(): void {
    this.unsubscribeEvents();
  }

  /** Build a protocol request and dispatch it; `ok=false` throws an Error carrying `code`. */
  async call<T = any>(method: string, params: unknown = {}): Promise<T> {
    const meta = PROTOCOL_METHODS[method];
    if (!meta) throw Object.assign(new Error(`unknown protocol method \`${method}\``), { code: 'METHOD_NOT_FOUND' });
    const id = `req_${Date.now().toString(36)}_${++this.seq}`;
    const request: ProtocolRequest = { v: PROTOCOL_VERSION, id, method, params };
    const response = await this.transport.invoke(request);
    if (!response.ok) {
      const error = new Error(
        response.error?.message || `protocol error ${response.error?.code ?? 'UNKNOWN'}`,
      );
      Object.assign(error, { code: response.error?.code, data: response.error?.data });
      throw error;
    }
    return response.result as T;
  }

  /** Subscribe to a streaming method's subscription; replay buffered events, then forward live. */
  subscribe(subscription: string, callback: (event: EventEnvelope) => void): () => void {
    const buffer = this.buffers.get(subscription);
    if (buffer) buffer.slice().forEach(callback);
    let set = this.listeners.get(subscription);
    if (!set) {
      set = new Set();
      this.listeners.set(subscription, set);
      this.buffers.set(subscription, []);
    }
    set.add(callback);
    return () => {
      set!.delete(callback);
      if (set!.size === 0) {
        this.listeners.delete(subscription);
        this.buffers.delete(subscription);
      }
    };
  }

  /** Session execute (streaming): returns a subscription; events come back via opts.onEvent. */
  execute(task: string, opts: ExecuteOptions = {}): Promise<{ subscription: string }> {
    return this.call<{ subscription: string }>('session.execute', {
      task,
      sessionId: opts.sessionId ?? undefined,
    }).then((result) => {
      if (opts.onEvent) this.subscribe(result.subscription, opts.onEvent);
      return result;
    });
  }

  /** Handshake: protocol version + method catalog (the client's first message). */
  info(): Promise<{ protocolVersion: number; methods: unknown[] }> {
    return this.call('protocol.info', {});
  }
}

/** Create the desktop client over IPC (lazy, to avoid constructing the transport outside Electron). */
export function createIpcClient(): ProtocolClient {
  return new ProtocolClient(new IpcTransport());
}
