import { existsSync } from 'node:fs';
import { join } from 'node:path';
import { createRequire } from 'node:module';

let runtime = null;

function loadNative() {
  // Find the workspace root (4 dirs up from packages/tui/src/)
  const rootDir = join(import.meta.dir, '..', '..', '..', '..');

  // Cargo builds libstarriver_desktop.dylib. Copy to .node for Bun.
  const dylib = join(rootDir, 'target', 'debug', 'libstarriver_desktop.dylib');
  const nodeFile = join(rootDir, 'target', 'debug', 'starriver_desktop.node');

  if (!existsSync(dylib)) {
    throw new Error(`Rust library not found at ${dylib}. Run: cargo build -p starriver-desktop`);
  }

  // Copy dylib → .node so Bun recognizes it as a napi addon
  const { copyFileSync } = require('node:fs');
  copyFileSync(dylib, nodeFile);

  // Bun loads .node napi-rs addons via process.dlopen
  const mod = { exports: {} };
  process.dlopen(mod, nodeFile);
  return mod.exports;
}

function getRuntime(configPath) {
  if (!runtime) {
    const addon = loadNative();
    const { AgentRuntime } = addon;
    runtime = new AgentRuntime(configPath || null);
    // Install the script executor: the Rust ScriptRuntime delivers
    // {id, path, params} via a ThreadsafeFunction; without an executor the script tool fails silently. The real
    // consumers (app.tsx / gui/server.ts) import this directly — it must be wired here.
    setupScriptExecutor(runtime);
  }
  return runtime;
}

async function shutdown() {
  if (runtime) { await runtime.shutdown(); runtime = null; }
}

export { getRuntime, shutdown };

// --- Script Executor ---
// Rust ScriptRuntime sends {id, path, params} via ThreadsafeFunction.
// This handler imports the user's script, calls execute(), and returns the result.
function setupScriptExecutor(run) {
  run.setScriptExecutor((err, msg) => {
    if (err) { console.error('[script-executor]', err); return; }
    const { id, path, params } = msg;
    import(path).then(mod => {
      return mod.default.execute(typeof params === 'string' ? JSON.parse(params) : params);
    }).then(result => {
      run.resolveScriptResult(id, JSON.stringify(result));
    }).catch(e => {
      run.resolveScriptResult(id, JSON.stringify({ error: e.message }));
    });
  });
}
