// Provider model-catalog probe (main process). Extracted verbatim from the
// ipcMain handler in main.ts so the wire behavior is integration-testable
// against a local HTTP server (renderer/__provider_probe.test.ts).
//
// Hardening (2026-11-20 bug hunt, BUG-P4…P13):
//  - anthropic bare-path fallback only on 404, and never retries the
//    identical /v1 URL when the base already carries /v1;
//  - OpenAI-compat bases get the same /v1 normalization as anthropic,
//    with a bare /models fallback for non-standard mounts;
//  - trailing slashes are stripped wholesale (no double-slash paths);
//  - response bodies decode as UTF-8 across chunk boundaries, are capped
//    at 10 MB, and obey an overall deadline (a slow-drip server cannot
//    hang the probe forever);
//  - 3xx redirects are followed (bounded, auth dropped cross-host);
//  - non-array catalogs and non-string ids are shape errors, not crashes.
import * as https from 'https';
import * as http from 'http';

export interface FetchedModel { id: string; name: string; contextLength?: number; }
export interface ProbeResult { ok: boolean; error?: string; models: FetchedModel[]; }

const MAX_BODY_BYTES = 10 * 1024 * 1024;
const REDIRECT_CODES = new Set([301, 302, 303, 307, 308]);

export async function fetchProviderModels(
  baseUrl: string,
  apiKey?: string,
  engine?: 'ollama' | 'lmstudio',
  apiFormat?: string,
  timeoutMs = 10_000,
): Promise<ProbeResult> {
  const timeoutLabel = timeoutMs >= 1000 ? `${Math.round(timeoutMs / 1000)}s` : `${timeoutMs}ms`;
  // BUG-P25: one deadline for the WHOLE probe. Each doRequest used to arm a
  // fresh timeoutMs timer, so a slow first leg + fallback chain (v0→v1→bare,
  // anthropic /v1→bare, redirects) could take up to 3× (with redirects 12×)
  // the configured timeout. Every leg now draws from the same budget.
  const deadlineAt = Date.now() + timeoutMs;
  const doRequest = (
    url: URL,
    authHeaders?: Record<string, string>,
    redirectsLeft = 3,
    sendAuth = true,
  ): Promise<ProbeResult> => {
    const remaining = deadlineAt - Date.now();
    if (remaining <= 0) {
      return Promise.resolve({ ok: false, error: `Request timed out (${timeoutLabel})`, models: [] });
    }
    return new Promise((resolve) => {
      let req: ReturnType<typeof http.get> | undefined;
      let settled = false;
      const finish = (r: ProbeResult) => {
        if (settled) return;
        settled = true;
        clearTimeout(deadline);
        resolve(r);
      };
      // BUG-P13: overall deadline — the socket 'timeout' only fires on
      // inactivity, so a server dripping bytes forever used to hang us.
      const deadline = setTimeout(() => {
        req?.destroy();
        finish({ ok: false, error: `Request timed out (${timeoutLabel})`, models: [] });
      }, remaining);
      try {
        const mod = url.protocol === 'https:' ? https : http;
        const headers: Record<string, string> = { 'Accept': 'application/json', ...(authHeaders ?? {}) };
        if (sendAuth && apiKey && !authHeaders) headers['Authorization'] = `Bearer ${apiKey}`;
        req = mod.get(url, { headers, timeout: remaining }, (res: any) => {
          const code = res.statusCode || 0;
          // BUG-P8: follow bounded redirects; never leak credentials to a
          // different host than the one the user configured.
          if (REDIRECT_CODES.has(code) && res.headers.location && redirectsLeft > 0) {
            // BUG-P22: a malformed Location must surface a graceful error, not
            // throw inside the async response callback — that is an uncaught
            // exception (main-process crash) and leaves the probe hanging.
            let next: URL;
            try {
              next = new URL(res.headers.location, url);
            } catch {
              res.resume();
              finish({ ok: false, error: `HTTP ${code}: invalid redirect target`, models: [] });
              return;
            }
            if (next.protocol !== 'http:' && next.protocol !== 'https:') {
              res.resume();
              finish({ ok: false, error: `HTTP ${code}: unsupported redirect target`, models: [] });
              return;
            }
            const crossHost = next.host !== url.host;
            res.resume();
            settled = true;
            clearTimeout(deadline);
            doRequest(next, crossHost ? undefined : authHeaders, redirectsLeft - 1, !crossHost).then(resolve);
            return;
          }
          if (code < 200 || code >= 300) {
            let errBody = '';
            res.setEncoding('utf8');
            res.on('data', (chunk: string) => {
              if (errBody.length < 200) errBody += chunk;
              // BUG-P24: once enough detail is captured, stop waiting for the
              // (possibly endless) body. A large/slow error body must surface
              // as the real HTTP error promptly, not be misreported as a
              // deadline timeout — and must not keep downloading unbounded.
              if (errBody.length >= 200) {
                req?.destroy();
                finish({ ok: false, error: `HTTP ${code}: ${errBody.slice(0, 200)}`, models: [] });
              }
            });
            res.on('end', () => {
              finish({ ok: false, error: `HTTP ${code}${errBody ? ': ' + errBody.slice(0, 200) : ''}`, models: [] });
            });
            res.on('error', () => finish({ ok: false, error: `HTTP ${code}`, models: [] }));
            return;
          }
          // BUG-P7: setEncoding decodes multi-byte UTF-8 across chunk
          // boundaries; raw Buffer += string coerced per chunk mojibake'd
          // CJK/emoji model names.
          res.setEncoding('utf8');
          let body = '';
          let size = 0;
          res.on('data', (chunk: string) => {
            // BUG-P10: cap the buffer — a hostile/buggy endpoint must not
            // be able to exhaust main-process memory.
            size += Buffer.byteLength(chunk, 'utf8');
            if (size > MAX_BODY_BYTES) {
              req?.destroy();
              finish({ ok: false, error: 'Response too large (>10MB)', models: [] });
              return;
            }
            body += chunk;
          });
          res.on('error', (e: any) => {
            finish({ ok: false, error: e?.message || String(e), models: [] });
          });
          res.on('end', () => {
            try {
              const json = JSON.parse(body);
              // Ollama /api/tags: { models: [{name: "qwen3-coder:7b", ...}] }
              if (engine === 'ollama') {
                // BUG-P6/P9: validate shape and id types instead of letting
                // .filter throw (misreported as "Invalid JSON") or leaking
                // numeric ids into a string-typed pipeline.
                if (!Array.isArray(json.models)) {
                  finish({ ok: false, error: 'Unexpected response shape: models is not an array', models: [] });
                  return;
                }
                const models: FetchedModel[] = json.models
                  .filter((m: any) => typeof m?.name === 'string' && m.name)
                  .map((m: any) => ({ id: m.name, name: m.name }));
                finish({ ok: true, models });
                return;
              }
              // LM Studio /api/v0/models reports loaded_context_length (the real
              // runtime limit — e.g. 8192 while the model supports 131072);
              // OpenAI-compat /v1/models reports context_length|max_context_length.
              // Anthropic /v1/models shares the data[].id shape and adds display_name.
              if (!Array.isArray(json.data)) {
                finish({ ok: false, error: 'Unexpected response shape: data is not an array', models: [] });
                return;
              }
              const models: FetchedModel[] = json.data
                .filter((m: any) => typeof m?.id === 'string' && m.id)
                .map((m: any) => {
                  const ctx = m.loaded_context_length ?? m.context_length ?? m.max_context_length;
                  const name = typeof m.display_name === 'string' && m.display_name ? m.display_name : m.id;
                  const model: FetchedModel = { id: m.id, name };
                  if (typeof ctx === 'number' && ctx > 0 && Number.isFinite(ctx)) model.contextLength = ctx;
                  return model;
                });
              finish({ ok: true, models });
            } catch (e: any) {
              finish({ ok: false, error: 'Invalid JSON response from server', models: [] });
            }
          });
        });
        req.on('error', (e: any) => {
          finish({ ok: false, error: e?.message || String(e), models: [] });
        });
        req.on('timeout', () => {
          req?.destroy();
          finish({ ok: false, error: `Request timed out (${timeoutLabel})`, models: [] });
        });
      } catch (e: any) {
        finish({ ok: false, error: e?.message || String(e), models: [] });
      }
    });
  };

  try {
    // BUG-P11: strip ALL trailing slashes, not just one.
    const base = baseUrl.replace(/\/+$/, '');

    // Ollama exposes its native /api/tags (not OpenAI /v1/models).
    if (engine === 'ollama') {
      return await doRequest(new URL(base + '/api/tags'));
    }

    // Anthropic wire protocol: the catalog lives at {base}/v1/models and
    // auth is x-api-key + anthropic-version (NOT OpenAI's Bearer). A base
    // already ending in /v1 only appends /models — same dedup rule as the
    // Rust adapter's messages_url (BUG-169).
    if (apiFormat === 'anthropic' && !engine) {
      const auth: Record<string, string> = { 'anthropic-version': '2023-06-01' };
      if (apiKey) auth['x-api-key'] = apiKey;
      const anthropicUrl = new URL((base.endsWith('/v1') ? base : base + '/v1') + '/models');
      const res = await doRequest(anthropicUrl, auth);
      if (res.ok) return res;
      // BUG-P5: only a 404 means "maybe mounted elsewhere" — auth/network
      // errors must surface immediately, not trigger a second request.
      // BUG-P4: the bare path strips /v1 so it is never the same URL again.
      if ((res.error || '').startsWith('HTTP 404')) {
        const bare = await doRequest(new URL(base.replace(/\/v1$/, '') + '/models'), auth);
        if (bare.ok) return bare;
        // BUG-P26: only a second 404 supports the "not implemented" hint. Any
        // other bare failure (timeout / 5xx / auth) must surface its real
        // reason instead of being masked by the original 404 + hint.
        if ((bare.error || '').startsWith('HTTP 404')) {
          return {
            ok: false,
            error: (res.error || 'fetch failed')
              + ' — this Anthropic-compatible endpoint may not implement model listing (/v1/models); add models manually',
            models: [],
          };
        }
        return bare;
      }
      return res;
    }

    // BUG-P12: normalize /v1 exactly like the anthropic branch; keep a bare
    // /models fallback for non-standard mounts that worked before.
    const v1Url = new URL((base.endsWith('/v1') ? base : base + '/v1') + '/models');
    // For LM Studio (local OR on LAN), prefer the richer /api/v0/models
    // endpoint which reports loaded_context_length (the real runtime KV
    // limit); fall back to OpenAI-compat /v1/models on any failure. No
    // hostname sniffing — a LAN instance serves the same /api/v0/models.
    const v0Url = new URL(base.replace(/\/v1$/, '') + '/api/v0/models');
    const v0 = await doRequest(v0Url);
    if (v0.ok && v0.models.length > 0) return v0;
    const v1 = await doRequest(v1Url);
    if (v1.ok || base.endsWith('/v1') || !(v1.error || '').startsWith('HTTP 404')) return v1;
    return await doRequest(new URL(base + '/models'));
  } catch (e: any) {
    return { ok: false, error: e?.message || String(e), models: [] };
  }
}