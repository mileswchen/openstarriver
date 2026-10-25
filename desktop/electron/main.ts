import { app, BrowserWindow, ipcMain, dialog, protocol, shell, Menu, clipboard } from 'electron';
import { spawn, execFile } from 'child_process';
import * as path from 'path';
import * as fs from 'fs';
import * as os from 'os';
import * as https from 'https';
import * as http from 'http';
import { DaemonClient, daemonInvoke, PROTOCOL_VERSION } from './daemon';
import { acquireSingleInstanceLock } from './single-instance';

// Single instance (2026-10-04): without the lock, any second launch — another
// double-click, or a different copy of the app (dist build / DMG vs the
// installed one; they all share this lock via the shared userData dir) — ran a
// full second instance: two identical dock icons both fighting over the same
// daemon and SQLite WALs. The loser must quit before any window/backend init;
// 'second-instance' (single-instance.ts) raises the winner's window instead.
const isPrimaryInstance = acquireSingleInstanceLock();
if (!isPrimaryInstance) app.quit();

let mainWindow: BrowserWindow | null = null;
let runtime: any = null;
// FPM P4b: with `STARRIVER_MODE=daemon` (or STARRIVER_DAEMON=1) Electron acts as a
// launcher + client: spawn/connect starriverd, protocol requests proxied over HTTP+SSE;
// app exit does not kill the daemon (shared service). Default embedded (in-process NAPI), unchanged.
const DAEMON_MODE = process.env.STARRIVER_MODE === 'daemon' || process.env.STARRIVER_DAEMON === '1';
let daemon: DaemonClient | null = null;
// Set when the NAPI module load or AgentRuntime construction fails. Surfaced to
// the renderer via `runtime:backendHealth` so a dead backend shows an error
// screen instead of silently running a broken (zombie) UI.
let runtimeError: string | null = null;

// FPM P3: plugin UI custom protocol — serves only local assets under ~/.starriver/plugins/<name>/ui,
// path traversal rejected; frame-level CSP on responses (no external calls, no fetch), plugin frames have no network.
protocol.registerSchemesAsPrivileged([
  {
    scheme: 'starriver-plugin',
    privileges: { standard: true, secure: true, supportFetchAPI: true, corsEnabled: false },
  },
]);

function mimeFor(file: string): string {
  const ext = file.split('.').pop()?.toLowerCase() ?? '';
  switch (ext) {
    case 'html': return 'text/html; charset=utf-8';
    case 'js': case 'mjs': return 'text/javascript; charset=utf-8';
    case 'css': return 'text/css; charset=utf-8';
    case 'json': return 'application/json; charset=utf-8';
    case 'png': return 'image/png';
    case 'jpg': case 'jpeg': return 'image/jpeg';
    case 'gif': return 'image/gif';
    case 'svg': return 'image/svg+xml';
    case 'woff2': return 'font/woff2';
    default: return 'application/octet-stream';
  }
}

const PLUGIN_UI_CSP =
  "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; " +
  "img-src 'self' data:; font-src 'self' data:; connect-src 'none'; base-uri 'none'; form-action 'none'";

function registerPluginProtocol(): void {
  const pluginRoot = path.join(os.homedir(), '.starriver', 'plugins');
  protocol.handle('starriver-plugin', (request) => {
    const url = new URL(request.url);
    // WHATWG URL lowercases the host, but plugin directories keep the
    // manifest's case — on case-sensitive filesystems a case-faithful lookup
    // 404s. Match the directory case-insensitively instead.
    const lower = url.hostname;
    if (!lower || /[\\/]/.test(lower) || lower === '.' || lower === '..') {
      return new Response('bad plugin name', { status: 400 });
    }
    let name = lower;
    try {
      const entries = fs.readdirSync(pluginRoot);
      const hit = entries.find((e) => e.toLowerCase() === lower);
      if (hit) name = hit;
    } catch {
      /* plugin root missing — falls through to the 404 below */
    }
    const pluginDir = path.join(pluginRoot, name);
    const rel = decodeURIComponent(url.pathname);
    const resolved = path.normalize(path.join(pluginDir, rel));
    if (!resolved.startsWith(pluginDir + path.sep)) {
      return new Response('forbidden', { status: 403 });
    }
    if (!fs.existsSync(resolved) || fs.statSync(resolved).isDirectory()) {
      return new Response('not found', { status: 404 });
    }
    return new Response(fs.readFileSync(resolved), {
      headers: {
        'Content-Type': mimeFor(resolved),
        'Content-Security-Policy': PLUGIN_UI_CSP,
        'Cache-Control': 'no-store',
      },
    });
  });
}

/** Linux composes the title-bar/dock icon from this window property; the
 *  packaged hicolor icons only cover the Applications menu entry, so without
 *  it the running window falls back to the X11 default glyph (2026-10-08
 *  user report: title bar + taskbar never showed the app logo). Works in dev
 *  and packaged alike — assets/icon.png rides inside the asar (`files`). */
function windowIconPath(): string | undefined {
  if (process.platform !== 'linux') return undefined;
  return path.join(__dirname, 'assets', 'icon.png');
}

function createWindow(): void {
  mainWindow = new BrowserWindow({
    width: 1100,
    height: 750,
    minWidth: 800,
    minHeight: 500,
    backgroundColor: '#f5f5f5',
    icon: windowIconPath(),
    webPreferences: {
      preload: path.join(__dirname, 'preload.js'),
      contextIsolation: true,
      nodeIntegration: false,
    },
    titleBarStyle: "hiddenInset",
  });
  // Null the reference on close: on macOS the app stays resident and a stale
  // destroyed BrowserWindow makes every later webContents.send throw
  // "Object has been destroyed" (an uncaughtException in daemon mode).
  mainWindow.once('closed', () => { mainWindow = null; });

  // Right-click menu: conversation text is selectable — expose Copy where a
  // selection exists, Copy link on links, Paste/Select All in editable fields.
  mainWindow.webContents.on('context-menu', (_event, params) => {
    const items: Electron.MenuItemConstructorOptions[] = [];
    if (params.selectionText.trim().length > 0) {
      items.push({ role: 'copy', label: '复制' });
    }
    if (params.linkURL) {
      // WI-212: http(s) links also open in the system default browser —
      // same whitelist as the click path and the IPC handler.
      if (/^https?:\/\//i.test(params.linkURL)) {
        items.push({
          label: '在浏览器打开',
          click: () => { void shell.openExternal(params.linkURL).catch(() => {}); },
        });
      }
      items.push({
        label: '复制链接',
        click: () => { clipboard.writeText(params.linkURL); },
      });
    }
    if (params.isEditable) {
      items.push({ role: 'paste', label: '粘贴' }, { role: 'selectAll', label: '全选' });
    }
    if (items.length > 0) {
      Menu.buildFromTemplate(items).popup();
    }
  });

  // WI-212: the renderer must never open Electron child windows — any
  // window.open (target=_blank that bypassed the React <a> layer) is denied
  // here, with http(s) re-routed to the system default browser.
  mainWindow.webContents.setWindowOpenHandler(({ url }) => {
    if (/^https?:\/\//i.test(url)) void shell.openExternal(url).catch(() => {});
    return { action: 'deny' };
  });
  // The window loads only the local renderer: navigation is always prevented,
  // and an http(s) navigation attempt (relative href, dragged link, …) is
  // re-routed to the system browser instead of hijacking the app.
  mainWindow.webContents.on('will-navigate', (event, url) => {
    event.preventDefault();
    if (/^https?:\/\//i.test(url)) void shell.openExternal(url).catch(() => {});
  });

  mainWindow.loadFile(path.join(__dirname, 'renderer', 'index.html'));
}

app.whenReady().then(async () => {
  // A second instance may reach 'ready' before its quit lands — never init
  // windows/backend there (the primary already owns the daemon and the DBs).
  if (!isPrimaryInstance) return;
  registerPluginProtocol();

  // Custom titlebar window controls
  // Directory picker
  ipcMain.handle('dialog:selectDirectory', async () => {
    if (!mainWindow) return null;
    const result = await dialog.showOpenDialog(mainWindow, {
      properties: ['openDirectory', 'createDirectory'],
      title: 'Select project directory',
    });
    return result.canceled ? null : result.filePaths[0] || null;
  });

  // Temp file storage for image attachments
  ipcMain.handle('runtime:saveTempFile', (_e, name: string, base64Data: string) => {
    const tmpDir = path.join(os.tmpdir(), 'starriver');
    if (!fs.existsSync(tmpDir)) {
      fs.mkdirSync(tmpDir, { recursive: true });
    }
    // Images now ride the attachment seam; clean stale temp files (e.g. old
    // image uploads) best-effort so the tmp dir does not grow forever.
    try {
      const cutoff = Date.now() - 24 * 60 * 60 * 1000;
      for (const f of fs.readdirSync(tmpDir)) {
        const p = path.join(tmpDir, f);
        try {
          if (fs.statSync(p).mtimeMs < cutoff) fs.unlinkSync(p);
        } catch {
          /* raced or already gone */
        }
      }
    } catch {
      /* cleanup is best-effort */
    }
    const safeName = name.replace(/[^a-zA-Z0-9._-]/g, '_');
    const filePath = path.join(tmpDir, `${Date.now()}_${safeName}`);
    fs.writeFileSync(filePath, base64Data, 'base64');
    return filePath;
  });


  ipcMain.handle("win:close", () => mainWindow?.close());
  ipcMain.handle("win:minimize", () => mainWindow?.minimize());
  ipcMain.handle("win:maximize", () => {
    if (mainWindow?.isMaximized()) mainWindow.unmaximize();
    else mainWindow?.maximize();
  });

  // ── shell:openTerminal / shell:openIDE ──
  // Renderer-initiated "open the workspace elsewhere" actions: spawn the
  // user's terminal emulator / IDE on the session's workspace directory.
  // These run OUTSIDE the agent (user-invoked, main-process spawn) — sandbox
  // does not apply. A settings-provided command template (`apps.*_command`,
  // `{dir}` placeholder) wins over the platform default; `{dir}` is
  // single-quoted before `sh -c` so paths with spaces survive.
  const shellQuote = (value: string) => `'${value.replace(/'/g, `'\\''`)}'`;
  const runCommandTemplate = (template: string, cwd: string) =>
    new Promise<void>((resolve, reject) => {
      const child = spawn('/bin/sh', ['-c', template.replace(/\{dir\}/g, shellQuote(cwd))], {
        cwd,
        stdio: 'ignore',
      });
      child.on('error', reject);
      child.on('exit', (code) =>
        code === 0 ? resolve() : reject(new Error(`command exited with ${code}`)),
      );
    });
  const firstOnPath = (names: string[]): Promise<string | null> =>
    new Promise((resolve) => {
      let index = 0;
      const next = () => {
        if (index >= names.length) return resolve(null);
        const name = names[index++];
        execFile(process.platform === 'win32' ? 'where' : 'which', [name], (err) => (err ? next() : resolve(name)));
      };
      next();
    });

  // WI-212: open a link in the system default browser. The http(s) whitelist
  // lives here (and again in preload + the renderer <a> layer): agent output
  // is untrusted content, so file:/javascript:/custom schemes must never
  // reach shell.openExternal from any path.
  ipcMain.handle('shell:openExternal', async (_e, raw: unknown): Promise<{ ok: boolean }> => {
    const url = typeof raw === 'string' ? raw.trim() : '';
    if (!/^https?:\/\//i.test(url)) return { ok: false };
    let parsed: URL;
    try { parsed = new URL(url); } catch { return { ok: false }; }
    if (parsed.protocol !== 'http:' && parsed.protocol !== 'https:') return { ok: false };
    try {
      await shell.openExternal(parsed.toString());
      return { ok: true };
    } catch {
      return { ok: false };
    }
  });

  ipcMain.handle('shell:openTerminal', async (_e, args: { cwd: string; command?: string }) => {
    const cwd = String(args?.cwd ?? '');
    if (!cwd || !fs.existsSync(cwd)) throw new Error(`directory not found: ${cwd || '(empty)'}`);
    if (args?.command) {
      await runCommandTemplate(args.command, cwd);
      return { ok: true };
    }
    if (process.platform === 'darwin') {
      await new Promise<void>((resolve, reject) => {
        spawn('open', ['-a', 'Terminal', cwd], { stdio: 'ignore' })
          .on('error', reject)
          .on('exit', (code) => (code === 0 ? resolve() : reject(new Error(`open exited with ${code}`))));
      });
      return { ok: true };
    }
    if (process.platform === 'win32') {
      await new Promise<void>((resolve, reject) => {
        spawn('cmd', ['/K', 'cd', `/d "${cwd}"`], { cwd, stdio: 'ignore', shell: true })
          .on('error', reject);
        resolve();
      });
      return { ok: true };
    }
    const terminal = await firstOnPath(['x-terminal-emulator', 'gnome-terminal', 'konsole']);
    if (!terminal) throw new Error('no terminal emulator found on PATH');
    await new Promise<void>((resolve, reject) => {
      spawn(terminal, [cwd], { cwd, stdio: 'ignore', detached: true })
        .on('error', reject)
        .unref();
      resolve();
    });
    return { ok: true };
  });

  // Models settings "open config file": open ~/.starriver/settings.yaml with the system
  // default editor (the model registry persists in its llm.providers section). When the file
  // does not exist yet, write an empty document first so openPath fails loudly on missing paths.
  ipcMain.handle('shell:openModelsConfig', async () => {
    const configPath = path.join(os.homedir(), '.starriver', 'settings.yaml');
    try {
      if (!fs.existsSync(configPath)) {
        fs.mkdirSync(path.dirname(configPath), { recursive: true });
        fs.writeFileSync(configPath, '', 'utf8');
      }
      const error = await shell.openPath(configPath);
      return error ? { ok: false, error } : { ok: true };
    } catch (e: any) {
      return { ok: false, error: String(e?.message || e) };
    }
  });

  ipcMain.handle('shell:openIDE', async (_e, args: { cwd: string; command?: string }) => {
    const cwd = String(args?.cwd ?? '');
    if (!cwd || !fs.existsSync(cwd)) throw new Error(`directory not found: ${cwd || '(empty)'}`);
    if (args?.command) {
      await runCommandTemplate(args.command, cwd);
      return { ok: true };
    }
    // Probe the `code` CLI first; fall back to the macOS app bundle.
    const onPath = await firstOnPath(['code']);
    if (onPath) {
      await new Promise<void>((resolve, reject) => {
        const child = spawn(onPath, [cwd], { cwd, stdio: 'ignore', detached: true });
        child.on('error', reject);
        child.unref();
        resolve();
      });
      return { ok: true };
    }
    if (process.platform === 'darwin') {
      await new Promise<void>((resolve, reject) => {
        spawn('open', ['-a', 'Visual Studio Code', cwd], { stdio: 'ignore' })
          .on('error', reject)
          .on('exit', (code) => (code === 0 ? resolve() : reject(new Error(`open exited with ${code}`))));
      });
      return { ok: true };
    }
    throw new Error('VS Code CLI (`code`) not found on PATH — configure apps.ide_command in settings.yaml');
  });
  if (!DAEMON_MODE) {
  try {
    // Dev: the NAPI dylib sits at <repo>/target/release/. Packaged:
    // electron-builder ships it via `extraResources` into resources/ (macOS:
    // Contents/Resources, win/linux: resources/ next to the binary) — so
    // process.resourcesPath resolves it on every platform.
    const napiPath = app.isPackaged
      ? path.join(process.resourcesPath, 'starriver_desktop.node')
      : path.join(__dirname, '..', '..', 'target', 'release', 'starriver_desktop.node');
    const napiModule = require(napiPath);
    const AgentRuntime = napiModule.AgentRuntime || napiModule;
    runtime = new AgentRuntime();
    console.log('[main] NAPI methods:', Object.keys(Object.getPrototypeOf(runtime)).filter((k: string) => !k.startsWith('_')).sort().join(', '));

  // P4a: embedded protocol server — browsers/third-party fronts connect to the same protocol over loopback HTTP+SSE.
    ipcMain.handle('runtime:httpServerInfo', () => runtime.httpServerInfo?.() ?? null);
    ipcMain.handle('runtime:startHttpServer', () => {
      if (!runtime.startHttpServer) throw new Error('startHttpServer not available — please rebuild the Rust NAPI module');
      return runtime.startHttpServer();
    });
    ipcMain.handle('runtime:stopHttpServer', () => runtime.stopHttpServer?.());
    try {
      const serverInfo = runtime.startHttpServer();
      console.log('[main] protocol HTTP server:', serverInfo);
    } catch (error) {
      console.error('[main] protocol HTTP server start failed:', error);
    }

    // renderer over the existing runtime:event channel (subscription
    // multiplexes concurrent runs). Legacy typed handlers stay for the old UI.
    ipcMain.handle('runtime:invoke', (_e, requestJson: string) => {
      if (!runtime.invoke) {
        console.error('[main] runtime.invoke is undefined — NAPI module may be outdated');
        throw new Error('invoke not available — please rebuild the Rust NAPI module');
      }
      return runtime.invoke(requestJson, (_err: any, event: any) => {
        // One throw here (stale window, destroyed webContents, …) must not
        // kill the relay silently: without the catch every later event is
        // dropped by the Rust side's `let _ =` and the UI freezes while the
        // backend keeps running.
        try {
          if (event && mainWindow) {
            mainWindow.webContents.send('runtime:event', event);
          }
        } catch (error) {
          console.error('[main] runtime:event forward failed:', error);
        }
      });
    });

    console.log('[main] NAPI runtime loaded with all features');
  } catch (e) {
    runtimeError = e instanceof Error ? e.message : String(e);
    console.error('[main] NAPI module not loaded:', runtimeError);
  }
  } else {
  // FPM P4b: daemon mode — Electron is only a launcher + client.
    // DaemonClient owns reuse/spawn and crash auto-restart (child exit + probe + backoff);
    // after a restart the SSE is rebuilt and `daemon:status` (connected) tells upstream to refresh.
    try {
      daemon = new DaemonClient({
        onStatus: (status) => {
          if (status.state === 'connected' || status.state === 'failed') {
            runtimeError = status.state === 'failed' ? status.error : null;
          }
          if (mainWindow) mainWindow.webContents.send('daemon:status', status);
        },
        onRestarted: (info) => {
          // Re-handshake after a restart (the version may have changed); the renderer refreshes
          // session snapshots on daemon:status connected (SQLite-persisted).
          void daemonInvoke(info, 'protocol.info', {})
            .then((handshake: any) => {
              if (handshake.protocolVersion !== PROTOCOL_VERSION) {
                runtimeError = `protocol version mismatch after daemon restart: daemon v${handshake.protocolVersion} vs client v${PROTOCOL_VERSION}`;
              }
            })
            .catch((error) => {
              runtimeError = error instanceof Error ? error.message : String(error);
            });
        },
      });
      // Protocol entry point (FPM acceptance #4: all UI traffic goes through the protocol;
      // daemon mode dispatches over HTTP); events come through DaemonClient's SSE — listeners
      daemon.onEnvelope((envelope) => {
        if (mainWindow) mainWindow.webContents.send('runtime:event', envelope);
      });
      ipcMain.handle('runtime:invoke', async (_e, requestJson: string) => {
        const request = JSON.parse(requestJson);
        const response = await daemon!.invokeRaw(request);
        return JSON.stringify(response);
      });
      ipcMain.handle('runtime:httpServerInfo', () => {
        const info = daemon?.currentInfo();
        return info ? JSON.stringify({ port: info.port, token: info.token }) : null;
      });
      ipcMain.handle('runtime:startHttpServer', () => {
        const info = daemon?.currentInfo();
        return info ? JSON.stringify({ port: info.port, token: info.token }) : null;
      });
      ipcMain.handle('runtime:stopHttpServer', () => undefined);
      await daemon.connect();
      const handshake: any = await daemon.invoke('protocol.info', {});
      if (handshake.protocolVersion !== PROTOCOL_VERSION) {
        throw new Error(
          `protocol version mismatch: daemon v${handshake.protocolVersion} vs client v${PROTOCOL_VERSION} — 请升级 starriverd 或客户端`,
        );
      }
      ipcMain.handle('runtime:daemonStop', async () => {
        if (daemon) {
          await daemon.stop();
          daemon = null;
        }
        return true;
      });
      console.log(
        '[main] daemon mode connected:',
        `http://127.0.0.1:${daemon.currentInfo()?.port} (pid ${daemon.currentInfo()?.pid})`,
      );
    } catch (e) {
      runtimeError = e instanceof Error ? e.message : String(e);
      console.error('[main] daemon not available:', runtimeError);
    }
  }

  // ── Fetch provider models from remote API (pure main-process feature, shared by both modes) ──
  ipcMain.handle('runtime:fetchProviderModels', async (_e: any, baseUrl: string, apiKey?: string, engine?: 'ollama' | 'lmstudio') => {
    interface FetchedModel { id: string; name: string; contextLength?: number; }
    const doRequest = (url: URL): Promise<{ ok: boolean; error?: string; models: FetchedModel[] }> => {
      return new Promise((resolve) => {
        try {
          const mod = url.protocol === 'https:' ? https : http;
          const headers: Record<string, string> = { 'Accept': 'application/json' };
          if (apiKey) headers['Authorization'] = `Bearer ${apiKey}`;
          const req = mod.get(url, { headers, timeout: 10000 }, (res: any) => {
            let body = '';
            res.on('data', (chunk: string) => { body += chunk; });
            res.on('end', () => {
              const code = res.statusCode || 0;
              if (code < 200 || code >= 300) {
                resolve({ ok: false, error: `HTTP ${code}${body ? ': ' + body.slice(0, 200) : ''}`, models: [] });
                return;
              }
              try {
                const json = JSON.parse(body);
                // Ollama /api/tags: { models: [{name: "qwen3-coder:7b", ...}] }
                if (engine === 'ollama') {
                  const models: FetchedModel[] = (json.models || [])
                    .filter((m: any) => m.name)
                    .map((m: any) => ({ id: m.name, name: m.name }));
                  resolve({ ok: true, models });
                  return;
                }
                // LM Studio /api/v0/models reports loaded_context_length (the real
                // runtime limit — e.g. 8192 while the model supports 131072);
                // OpenAI-compat /v1/models reports context_length|max_context_length.
                const models: FetchedModel[] = (json.data || [])
                  .filter((m: any) => m.id)
                  .map((m: any) => {
                    const ctx = m.loaded_context_length ?? m.context_length ?? m.max_context_length;
                    const model: FetchedModel = { id: m.id, name: m.id };
                    if (typeof ctx === 'number' && ctx > 0) model.contextLength = ctx;
                    return model;
                  });
                resolve({ ok: true, models });
              } catch (e: any) {
                resolve({ ok: false, error: 'Invalid JSON response from server', models: [] });
              }
            });
          });
          req.on('error', (e: any) => {
            resolve({ ok: false, error: e?.message || String(e), models: [] });
          });
          req.on('timeout', () => {
            req.destroy();
            resolve({ ok: false, error: 'Request timed out (10s)', models: [] });
          });
        } catch (e: any) {
          resolve({ ok: false, error: e?.message || String(e), models: [] });
        }
      });
    };

    try {
      const base = baseUrl.replace(/\/$/, '');

      // Ollama exposes its native /api/tags (not OpenAI /v1/models). This
      // branch is what makes "Fetch models" work for Ollama at all — the
      // old code only spoke LM Studio / OpenAI-compat endpoints.
      if (engine === 'ollama') {
        const tagsUrl = new URL(base + '/api/tags');
        const res = await doRequest(tagsUrl);
        if (res.ok && res.models.length > 0) return res;
        // /api/tags returns { models: [{name, ...}] } — parse it into the
        // shared FetchedModel shape below; plain text fallback keeps the
        // error surface identical to the other branches.
        return res;
      }

      const v1Url = new URL(base + '/models');
      // For LM Studio (local OR on LAN), prefer the richer /api/v0/models
      // endpoint which reports loaded_context_length (the real runtime KV
      // limit); fall back to OpenAI-compat /v1/models on any failure. No
      // hostname sniffing — a LAN instance serves the same /api/v0/models.
      const v0Url = new URL(base.replace(/\/v1$/, '') + '/api/v0/models');
      const v0 = await doRequest(v0Url);
      if (v0.ok && v0.models.length > 0) return v0;
      return await doRequest(v1Url);
    } catch (e: any) {
      return { ok: false, error: e?.message || String(e), models: [] };
    }
  });

  // Backend health — registered OUTSIDE the try so the renderer can always ask
  // "is the backend alive?" and surface a clear error screen when it isn't.
  // Without this, a failed constructor yields a UI that opens but whose every
  // IPC rejects with "No handler registered" (the silent zombie mode).
  // WI-31: LAN address for the pairing flow (preload declared it, the
  // handler was never registered — every call rejected with "No handler").
  ipcMain.handle('runtime:lanAddress', () => {
    for (const list of Object.values(os.networkInterfaces())) {
      for (const net of list ?? []) {
        if (net.family === 'IPv4' && !net.internal) return net.address;
      }
    }
    return '127.0.0.1';
  });
  // WI-31: retry from the backend error screen. Init failures are resolved
  // outside the renderer (build starriverd, free the port, drop a stale
  // daemon.json) — a supervised relaunch re-runs the whole init sequence in
  // both embedded and daemon modes.
  ipcMain.handle('runtime:retryInit', () => {
    app.relaunch();
    app.exit(0);
    return { ok: true, relaunched: true };
  });
  ipcMain.handle('runtime:backendHealth', () => ({
    ok: runtime !== null || daemon?.isConnected() === true,
    error: runtimeError,
  }));
  ipcMain.handle('runtime:quitApp', () => app.quit());

  createWindow();
});

app.on('window-all-closed', () => {
  if (process.platform !== 'darwin') app.quit();
});

app.on('activate', () => {
  if (BrowserWindow.getAllWindows().length === 0) createWindow();
});

// ── MCP / Scheduled / Trace（embedded only）──
// daemon mode is taken over by daemonCompat's protocol proxy table (same channel names, through MethodRegistry).
if (!DAEMON_MODE) {
  ipcMain.handle('runtime:listMcpServers', () => runtime.listMcpServers?.() ?? []);
  ipcMain.handle('runtime:addMcpServer', (_e: any, name: string, transport: string, command: string, argsJson: string, envJson: string, url: string | null, oauthJson: string | null) =>
    runtime.addMcpServer?.(name, transport, command, argsJson, envJson, url ?? null, oauthJson ?? null));
  ipcMain.handle('runtime:removeMcpServer', (_e: any, name: string) => runtime.removeMcpServer?.(name));
  ipcMain.handle('runtime:refreshMcpTools', (_e: any, name?: string) => runtime.refreshMcpTools?.(name ?? null));
  ipcMain.handle('runtime:testMcpServer', (_e: any, name: string) => runtime.testMcpServer?.(name));
  ipcMain.handle('runtime:testMcpTool', (_e: any, name: string, toolName: string) => runtime.testMcpTool?.(name, toolName));
  ipcMain.handle('runtime:oauthConnect', (_e: any, name: string) => runtime.oauthConnect?.(name));
  ipcMain.handle('runtime:oauthDisconnect', (_e: any, name: string) => runtime.oauthDisconnect?.(name));

  ipcMain.handle('runtime:listScheduledTasks', () => runtime.listScheduledTasks?.() ?? []);
  ipcMain.handle('runtime:addScheduledTask', (_e: any, name: string, modeJson: string, prompt: string, model: string | null, enabled: boolean, startDate?: string, endDate?: string, skills?: string[]) =>
    runtime.addScheduledTask?.(name, modeJson, prompt, model, enabled, startDate ?? null, endDate ?? null, skills ?? []));
  ipcMain.handle('runtime:removeScheduledTask', (_e: any, name: string) => runtime.removeScheduledTask?.(name));
  ipcMain.handle('runtime:updateScheduledTask', (_e: any, name: string, modeJson: string | null, prompt: string | null, model: string | null, enabled: boolean | null, startDate: string | null, endDate: string | null, skills?: string[] | null) =>
    runtime.updateScheduledTask?.(name, modeJson, prompt, model, enabled, startDate, endDate, skills ?? null));
  ipcMain.handle('runtime:runScheduledTaskNow', (_e: any, name: string) => runtime.runScheduledTaskNow?.(name));
  ipcMain.handle('runtime:getTaskRunHistory', (_e: any, name: string) => runtime.getTaskRunHistory?.(name) ?? []);
  ipcMain.handle('runtime:listAllTaskRuns', () => runtime.listAllTaskRuns?.() ?? []);
  ipcMain.handle('runtime:getSessionTrace', (_e: any, sessionId: string) => runtime.getSessionTrace?.(sessionId) ?? { entries: [] });
}

// ── Auto-update ──
import { autoUpdater } from 'electron-updater';
autoUpdater.autoDownload = false;
autoUpdater.autoInstallOnAppQuit = true;

ipcMain.handle('updater:check', async () => {
  try {
    const result = await autoUpdater.checkForUpdates();
    return { available: !!result?.updateInfo, version: result?.updateInfo?.version };
  } catch { return { available: false }; }
});

ipcMain.handle('updater:download', async () => {
  try { await autoUpdater.downloadUpdate(); return { ok: true }; }
  catch (e: any) { return { ok: false, error: e.message }; }
});

autoUpdater.on('update-available', (info) => {
  mainWindow?.webContents.send('updater:status', { status: 'available', version: info.version });
});

autoUpdater.on('download-progress', (progress) => {
  mainWindow?.webContents.send('updater:status', { status: 'downloading', percent: progress.percent });
});

autoUpdater.on('update-downloaded', () => {
  mainWindow?.webContents.send('updater:status', { status: 'downloaded' });
});
