// Sandbox-proof TMPDIR wrapper (WI-152 插曲修复).
//
// Execution environments that confine writes to the workspace
// (workspace-write sandboxes) break tooling that mkdtemp()s in the system
// temp — vitest workers and electron-builder both die with EPERM before
// doing any work. This wrapper probes the system tmp once; when it is not
// writable it points TMPDIR/TEMP/TMP at <repo>/.tmp-build/tmp and runs the
// wrapped command. When the system tmp works it is a transparent no-op.
//
// Usage: node scripts/with-workspace-tmp.mjs <command> [args…]
import { spawnSync } from 'node:child_process';
import { mkdtempSync, rmSync, mkdirSync } from 'node:fs';
import * as os from 'node:os';
import { join, resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const command = process.argv.slice(2);
if (command.length === 0) {
  console.error('usage: node with-workspace-tmp.mjs <command> [args…]');
  process.exit(2);
}

let systemTmpOk = true;
try {
  const probe = mkdtempSync(join(os.tmpdir(), 'starriver-tmp-probe-'));
  rmSync(probe, { recursive: true, force: true });
} catch {
  systemTmpOk = false;
}

const env = { ...process.env };
if (!systemTmpOk) {
  const fallback = join(repoRoot, '.tmp-build', 'tmp');
  mkdirSync(fallback, { recursive: true });
  for (const key of ['TMPDIR', 'TEMP', 'TMP']) env[key] = fallback;
  console.error(`[with-workspace-tmp] system tmp not writable — TMPDIR -> ${fallback}`);
}

const result = spawnSync(command[0], command.slice(1), {
  stdio: 'inherit',
  env,
  shell: process.platform === 'win32',
});
process.exit(result.status ?? 1);
