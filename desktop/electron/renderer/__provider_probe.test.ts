// Integration tests for the provider model-catalog probe (main process,
// extracted to providerProbe.ts). Every case runs against a REAL local HTTP
// server — no transport mocks — so URL construction, auth headers, redirect
// behavior, chunk decoding and error surfaces are exercised end to end.
//
// BUG-P4…BUG-P13 are regression tests for defects found in the 2026-11-20
// apiFormat=anthropic probe work and the shared request helper it rides on.

import { createServer, type IncomingMessage, type Server, type ServerResponse } from 'http';
import { afterEach, describe, expect, it } from 'vitest';
import { fetchProviderModels } from '../providerProbe';

interface Seen { path: string; headers: Record<string, string | string[] | undefined>; }

let server: Server | null = null;
let seen: Seen[] = [];

async function start(handler: (req: IncomingMessage, res: ServerResponse) => void): Promise<number> {
  seen = [];
  server = createServer((req, res) => {
    seen.push({ path: req.url ?? '', headers: req.headers });
    handler(req, res);
  });
  await new Promise<void>((resolve) => server!.listen(0, '127.0.0.1', resolve));
  return (server!.address() as { port: number }).port;
}

afterEach(async () => {
  if (server) {
    const s = server;
    server = null;
    await new Promise<void>((resolve) => s.close(() => resolve()));
  }
});

const json = (res: ServerResponse, code: number, body: unknown) => {
  res.writeHead(code, { 'Content-Type': 'application/json' });
  res.end(JSON.stringify(body));
};
const notFound = (res: ServerResponse) => json(res, 404, { error: 'not found' });

describe('provider probe — happy paths (four wire formats)', () => {
  it('openai-compat: /v1/models with Bearer auth and context_length', async () => {
    const port = await start((req, res) => {
      if (req.url === '/v1/models') {
        json(res, 200, { data: [{ id: 'gpt-x', context_length: 32000 }] });
      } else notFound(res);
    });
    const out = await fetchProviderModels(`http://127.0.0.1:${port}/v1`, 'sk-key');
    expect(out.ok).toBe(true);
    expect(out.models).toEqual([{ id: 'gpt-x', name: 'gpt-x', contextLength: 32000 }]);
    const hit = seen.find((s) => s.path === '/v1/models')!;
    expect(hit.headers.authorization).toBe('Bearer sk-key');
  });

  it('lmstudio: /api/v0/models preferred, loaded_context_length wins', async () => {
    const port = await start((req, res) => {
      if (req.url === '/api/v0/models') {
        json(res, 200, { data: [{ id: 'local', loaded_context_length: 8192, context_length: 131072 }] });
      } else if (req.url === '/v1/models') {
        json(res, 200, { data: [{ id: 'local', context_length: 131072 }] });
      } else notFound(res);
    });
    const out = await fetchProviderModels(`http://127.0.0.1:${port}/v1`, undefined, 'lmstudio');
    expect(out.ok).toBe(true);
    expect(out.models[0].contextLength).toBe(8192);
    expect(seen[0].path).toBe('/api/v0/models');
  });

  it('ollama: native /api/tags names become ids', async () => {
    const port = await start((req, res) => {
      if (req.url === '/api/tags') json(res, 200, { models: [{ name: 'qwen3-coder:7b' }, { name: 'llama3:8b' }] });
      else notFound(res);
    });
    const out = await fetchProviderModels(`http://127.0.0.1:${port}`, undefined, 'ollama');
    expect(out.ok).toBe(true);
    expect(out.models.map((m) => m.id)).toEqual(['qwen3-coder:7b', 'llama3:8b']);
  });

  it('anthropic: x-api-key + anthropic-version, NO Bearer; display_name maps to name', async () => {
    const port = await start((req, res) => {
      if (req.url === '/v1/models') json(res, 200, { data: [{ id: 'claude-x', display_name: 'Claude X' }] });
      else notFound(res);
    });
    const out = await fetchProviderModels(`http://127.0.0.1:${port}`, 'sk-ant', undefined, 'anthropic');
    expect(out.ok).toBe(true);
    expect(out.models).toEqual([{ id: 'claude-x', name: 'Claude X' }]);
    const hit = seen.find((s) => s.path === '/v1/models')!;
    expect(hit.headers['x-api-key']).toBe('sk-ant');
    expect(hit.headers['anthropic-version']).toBe('2023-06-01');
    expect(hit.headers.authorization).toBeUndefined();
  });

  it('anthropic: base already ending in /v1 is not doubled', async () => {
    const port = await start((req, res) => {
      if (req.url === '/v1/models') json(res, 200, { data: [{ id: 'c1' }] });
      else notFound(res);
    });
    const out = await fetchProviderModels(`http://127.0.0.1:${port}/v1`, 'k', undefined, 'anthropic');
    expect(out.ok).toBe(true);
    expect(seen.every((s) => s.path === '/v1/models')).toBe(true);
  });

  it('anthropic: bare /models fallback when the gateway omits /v1', async () => {
    const port = await start((req, res) => {
      if (req.url === '/models') json(res, 200, { data: [{ id: 'bare-claude' }] });
      else notFound(res);
    });
    const out = await fetchProviderModels(`http://127.0.0.1:${port}`, 'k', undefined, 'anthropic');
    expect(out.ok).toBe(true);
    expect(out.models[0].id).toBe('bare-claude');
  });

  it('anthropic: double 404 surfaces the manual-models hint', async () => {
    const port = await start((_req, res) => notFound(res));
    const out = await fetchProviderModels(`http://127.0.0.1:${port}`, 'k', undefined, 'anthropic');
    expect(out.ok).toBe(false);
    expect(out.error).toContain('HTTP 404');
    expect(out.error).toContain('may not implement model listing');
  });

  it('empty catalog is a success with zero models', async () => {
    const port = await start((req, res) => {
      if (req.url === '/v1/models') json(res, 200, { data: [] });
      else notFound(res);
    });
    const out = await fetchProviderModels(`http://127.0.0.1:${port}/v1`);
    expect(out).toEqual({ ok: true, models: [] });
  });

  it('openai-compat: non-standard bare /models mount still works via fallback', async () => {
    const port = await start((req, res) => {
      if (req.url === '/models') json(res, 200, { data: [{ id: 'bare-mount' }] });
      else notFound(res);
    });
    const out = await fetchProviderModels(`http://127.0.0.1:${port}`);
    expect(out.ok).toBe(true);
    expect(out.models[0].id).toBe('bare-mount');
  });

  it('redirects to another host drop the credentials', async () => {
    const other = createServer((req, res) => {
      seen.push({ path: 'OTHER:' + req.url, headers: req.headers });
      json(res, 200, { data: [{ id: 'cross' }] });
    });
    await new Promise<void>((resolve) => other.listen(0, '127.0.0.1', resolve));
    const otherPort = (other.address() as { port: number }).port;
    try {
      const port = await start((req, res) => {
        if (req.url === '/v1/models') {
          res.writeHead(302, { Location: `http://localhost:${otherPort}/catalog` });
          res.end();
        } else notFound(res);
      });
      const out = await fetchProviderModels(`http://127.0.0.1:${port}/v1`, 'secret-key');
      expect(out.ok).toBe(true);
      const cross = seen.find((s) => s.path.startsWith('OTHER:'))!;
      expect(cross.headers.authorization).toBeUndefined();
    } finally {
      await new Promise<void>((resolve) => other.close(() => resolve()));
    }
  });
});

describe('provider probe — error surfaces', () => {
  it('HTTP 500 carries status and a body snippet', async () => {
    const port = await start((_req, res) => {
      res.writeHead(500, { 'Content-Type': 'text/plain' });
      res.end('boom');
    });
    const out = await fetchProviderModels(`http://127.0.0.1:${port}/v1`);
    expect(out.ok).toBe(false);
    expect(out.error).toContain('HTTP 500');
    expect(out.error).toContain('boom');
  });

  it('malformed JSON is reported as such', async () => {
    const port = await start((_req, res) => {
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end('{oops');
    });
    const out = await fetchProviderModels(`http://127.0.0.1:${port}/v1`);
    expect(out.ok).toBe(false);
    expect(out.error).toBe('Invalid JSON response from server');
  });

  it('connection refused rejects gracefully', async () => {
    const out = await fetchProviderModels('http://127.0.0.1:1/v1');
    expect(out.ok).toBe(false);
    expect(out.error).toBeTruthy();
  });

  it('idle socket hits the request timeout', async () => {
    const port = await start(() => { /* accept, never answer */ });
    const out = await fetchProviderModels(`http://127.0.0.1:${port}/v1`, undefined, undefined, undefined, 300);
    expect(out.ok).toBe(false);
    expect(out.error).toMatch(/timed out/i);
  }, 4000);
});

describe('provider probe — BUG-P4…P13 regressions', () => {
  it('BUG-P4: anthropic bare fallback must not retry the identical /v1 URL', async () => {
    const port = await start((req, res) => {
      if (req.url === '/models') json(res, 200, { data: [{ id: 'bare' }] });
      else notFound(res);
    });
    const out = await fetchProviderModels(`http://127.0.0.1:${port}/v1`, 'k', undefined, 'anthropic');
    expect(out.ok).toBe(true);
    expect(out.models[0].id).toBe('bare');
    const paths = seen.map((s) => s.path);
    expect(new Set(paths).size).toBe(paths.length); // no duplicate request
  });

  it('BUG-P5: anthropic auth failure must surface, not fall through to the bare path', async () => {
    const port = await start((req, res) => {
      if (req.url === '/v1/models') json(res, 401, { error: 'bad key' });
      else if (req.url === '/models') json(res, 200, { data: [] });
      else notFound(res);
    });
    const out = await fetchProviderModels(`http://127.0.0.1:${port}`, 'bad', undefined, 'anthropic');
    expect(out.ok).toBe(false);
    expect(out.error).toContain('401');
    expect(seen.length).toBe(1); // no wasteful second request
  });

  it('BUG-P6: non-array data/models is a shape error, not "Invalid JSON"', async () => {
    const port = await start((req, res) => {
      if (req.url === '/v1/models') json(res, 200, { data: { object: 'list' } });
      else notFound(res);
    });
    const out = await fetchProviderModels(`http://127.0.0.1:${port}/v1`);
    expect(out.ok).toBe(false);
    expect(out.error).not.toBe('Invalid JSON response from server');
    expect(out.error).toMatch(/shape|array/i);
  });

  it('BUG-P6b: ollama non-array models is a shape error too', async () => {
    const port = await start((req, res) => {
      if (req.url === '/api/tags') json(res, 200, { models: { name: 'x' } });
      else notFound(res);
    });
    const out = await fetchProviderModels(`http://127.0.0.1:${port}`, undefined, 'ollama');
    expect(out.ok).toBe(false);
    expect(out.error).toMatch(/shape|array/i);
  });

  it('BUG-P7: multi-byte UTF-8 split across chunks must not mojibake model names', async () => {
    const full = JSON.stringify({ data: [{ id: 'm1', display_name: '模型🚀' }] });
    const buf = Buffer.from(full, 'utf8');
    // Cut inside the first byte sequence of the 3-byte CJK test codepoint.
    const charStart = Buffer.byteLength(full.slice(0, full.indexOf('模')), 'utf8');
    const split = charStart + 1;
    const port = await start((req, res) => {
      if (req.url !== '/v1/models') return notFound(res);
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.write(buf.subarray(0, split));
      setTimeout(() => { res.end(buf.subarray(split)); }, 20);
    });
    const out = await fetchProviderModels(`http://127.0.0.1:${port}/v1`);
    expect(out.ok).toBe(true);
    expect(out.models[0].name).toBe('模型🚀');
  });

  it('BUG-P8: HTTP redirects are followed (gateways love 301/302)', async () => {
    const port = await start((req, res) => {
      if (req.url === '/v1/models') {
        res.writeHead(302, { Location: `http://127.0.0.1:${port}/catalog` });
        res.end();
      } else if (req.url === '/catalog') {
        json(res, 200, { data: [{ id: 'redirected' }] });
      } else notFound(res);
    });
    const out = await fetchProviderModels(`http://127.0.0.1:${port}/v1`);
    expect(out.ok).toBe(true);
    expect(out.models[0].id).toBe('redirected');
  });

  it('BUG-P9: ollama non-string names never leak numeric ids', async () => {
    const port = await start((req, res) => {
      if (req.url === '/api/tags') json(res, 200, { models: [{ name: 123 }, { name: 'ok' }, { name: '' }] });
      else notFound(res);
    });
    const out = await fetchProviderModels(`http://127.0.0.1:${port}`, undefined, 'ollama');
    expect(out.ok).toBe(true);
    expect(out.models.every((m) => typeof m.id === 'string' && m.id.length > 0)).toBe(true);
    expect(out.models.map((m) => m.id)).toEqual(['ok']);
  });

  it('BUG-P10: an oversized response is rejected instead of buffered without bound', async () => {
    const pad = 'a'.repeat(11 * 1024 * 1024);
    const port = await start((req, res) => {
      if (req.url !== '/v1/models') return notFound(res);
      json(res, 200, { data: [{ id: 'm' }], pad });
    });
    const out = await fetchProviderModels(`http://127.0.0.1:${port}/v1`);
    expect(out.ok).toBe(false);
    expect(out.error).toMatch(/large|size/i);
  }, 20000);

  it('BUG-P11: multiple trailing slashes must not produce double-slash paths', async () => {
    const port = await start((req, res) => {
      if (req.url === '/models' || req.url === '/v1/models') json(res, 200, { data: [{ id: 'slash' }] });
      else notFound(res);
    });
    const out = await fetchProviderModels(`http://127.0.0.1:${port}//`);
    expect(out.ok).toBe(true);
    expect(seen.every((s) => !s.path.startsWith('//'))).toBe(true);
  });

  it('BUG-P12: bare-host OpenAI-compat base gets /v1 normalization like anthropic', async () => {
    const port = await start((req, res) => {
      if (req.url === '/v1/models') json(res, 200, { data: [{ id: 'normalized' }] });
      else notFound(res);
    });
    const out = await fetchProviderModels(`http://127.0.0.1:${port}`);
    expect(out.ok).toBe(true);
    expect(out.models[0].id).toBe('normalized');
  });

  it('BUG-P13: a slow-drip server cannot hang the probe past the overall deadline', async () => {
    const port = await start((req, res) => {
      if (req.url !== '/v1/models') return notFound(res);
      res.writeHead(200, { 'Content-Type': 'application/json' });
      const timer = setInterval(() => res.write(' '), 40); // never idle, never ends
      res.on('close', () => clearInterval(timer));
    });
    const out = await fetchProviderModels(`http://127.0.0.1:${port}/v1`, undefined, undefined, undefined, 400);
    expect(out.ok).toBe(false);
    expect(out.error).toMatch(/timed out/i);
  }, 3000);

  it('a redirect to a non-http(s) scheme is rejected, not followed', async () => {
    const port = await start((_req, res) => {
      res.writeHead(302, { Location: 'ftp://evil.example/x' });
      res.end();
    });
    const out = await fetchProviderModels(`http://127.0.0.1:${port}/v1`);
    expect(out.ok).toBe(false);
    expect(out.error).toMatch(/unsupported redirect target/);
  });

  it('an invalid base URL surfaces a parse error instead of crashing', async () => {
    const out = await fetchProviderModels('not-a-valid-url');
    expect(out.ok).toBe(false);
    expect(out.error).toMatch(/Invalid URL/i);
  });

  it('BUG-P22: a malformed redirect Location surfaces an error, not an uncaught crash', async () => {
    const port = await start((_req, res) => {
      res.writeHead(302, { Location: 'http://[invalid' });
      res.end();
    });
    const out = await fetchProviderModels(`http://127.0.0.1:${port}`, undefined, 'ollama', undefined, 2000);
    expect(out.ok).toBe(false);
    // A graceful parse error, NOT the deadline timeout that would mask a crash.
    expect(out.error).toMatch(/invalid redirect target/i);
    expect(out.error).not.toMatch(/timed out/i);
  }, 4000);

  it('BUG-P25: the timeout is a whole-probe budget, not a fresh one per fallback request', async () => {
    const port = await start((req, res) => {
      if (req.url === '/api/v0/models') {
        // Burn most of the budget, then 404 → falls through to /v1/models.
        setTimeout(() => notFound(res), 350);
      }
      // /v1/models: accept the connection, never answer.
    });
    const t0 = Date.now();
    const out = await fetchProviderModels(`http://127.0.0.1:${port}`, 'k', undefined, undefined, 600);
    const elapsed = Date.now() - t0;
    expect(out.ok).toBe(false);
    expect(out.error).toMatch(/timed out/i);
    // Buggy: 350ms + a FRESH 600ms budget ≈ 950ms. Fixed: one shared 600ms
    // budget for the whole chain (+200ms scheduling slack).
    expect(elapsed).toBeLessThan(800);
  }, 5000);

  it('BUG-P26: a non-404 bare-fallback failure must surface its real reason, not the 404 hint', async () => {
    const port = await start((req, res) => {
      if (req.url === '/models') json(res, 500, { error: 'upstream exploded' });
      else notFound(res);
    });
    const out = await fetchProviderModels(`http://127.0.0.1:${port}`, 'k', undefined, 'anthropic');
    expect(out.ok).toBe(false);
    // The bare mount failed with 500 — that is the actionable reason. The
    // buggy code replaced it with the original 404 + "may not implement
    // model listing" hint, hiding the real server failure.
    expect(out.error).toMatch(/HTTP 500/);
    expect(out.error).toContain('upstream exploded');
    expect(out.error).not.toMatch(/may not implement/);
  });

  it('BUG-P24: a non-2xx error with enough body detail must not wait for the body to end', async () => {
    const port = await start((_req, res) => {
      res.writeHead(500, { 'Content-Type': 'text/plain' });
      res.write('x'.repeat(250)); // >=200 chars of detail, then never end()
    });
    const out = await fetchProviderModels(`http://127.0.0.1:${port}`, undefined, 'ollama', undefined, 1200);
    expect(out.ok).toBe(false);
    expect(out.error).toMatch(/^HTTP 500/);
  }, 4000);
});

describe('provider probe — error-body & auth edge paths', () => {
  it('a non-2xx response with an empty body yields a bare HTTP status error', async () => {
    const port = await start((_req, res) => {
      res.writeHead(500);
      res.end();
    });
    const out = await fetchProviderModels(`http://127.0.0.1:${port}/v1`);
    expect(out.ok).toBe(false);
    expect(out.error).toBe('HTTP 500');
  });

  it('an error body longer than 200 chars is truncated, later chunks dropped', async () => {
    const port = await start((_req, res) => {
      res.writeHead(400);
      res.write('x'.repeat(210));
      setTimeout(() => res.end('y'.repeat(20)), 25);
    });
    const out = await fetchProviderModels(`http://127.0.0.1:${port}/v1`);
    expect(out.ok).toBe(false);
    expect(out.error).toBe('HTTP 400: ' + 'x'.repeat(200));
  }, 3000);

  it('anthropic without an API key sends anthropic-version but no x-api-key', async () => {
    const port = await start((req, res) => {
      if (req.url === '/v1/models') json(res, 200, { data: [{ id: 'anon' }] });
      else notFound(res);
    });
    const out = await fetchProviderModels(`http://127.0.0.1:${port}`, undefined, undefined, 'anthropic');
    expect(out.ok).toBe(true);
    const hit = seen.find((s) => s.path === '/v1/models')!;
    expect(hit.headers['x-api-key']).toBeUndefined();
    expect(hit.headers['anthropic-version']).toBe('2023-06-01');
    expect(hit.headers.authorization).toBeUndefined();
  });
});