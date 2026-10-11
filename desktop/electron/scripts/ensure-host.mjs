// Ensure the NAPI host dylib (`target/release/libstarriver_desktop.dylib`) is
// built from the CURRENT Rust sources before the desktop app loads it.
//
// Why this exists: `sync-napi` only COPIES the dylib into
// `starriver_desktop.node` — it never builds — and the Electron app loads the
// host in-process (NAPI), not via starriverd. A stale dylib therefore runs the
// whole agent silently out of date (this shipped: the PTC mode work was
// invisible until the dylib was manually rebuilt).
//
// Mechanism: hash every Rust input of the workspace (crates/**/*.{rs,toml} +
// Cargo.lock) into a stamp file next to the dylib. Hash unchanged → skip cargo
// entirely; changed or missing → `cargo build --release -p
// starriver-desktop`, then refresh the stamp. Content hashing (not mtimes) so
// git checkouts / branch switches cannot produce false "fresh" or false
// "stale". The following `sync-napi` step always re-copies + re-signs, so the
// .node the app loads can never be older than the dylib.
import { createHash } from 'node:crypto';
import { execSync, spawnSync } from 'node:child_process';
import { existsSync, readdirSync, readFileSync, statSync, writeFileSync, mkdirSync } from 'node:fs';
import { dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const electronDir = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const repoRoot = resolve(electronDir, '..', '..');
const cratesDir = join(repoRoot, 'crates');
const dylib = join(repoRoot, 'target', 'release', 'libstarriver_desktop.dylib');
const stamp = join(repoRoot, 'target', 'release', '.starriver-desktop-build-stamp');

const HASH_VERSION = 1; // bump when the input set or build recipe changes

function collectSources(dir, out) {
  for (const entry of readdirSync(dir, { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name))) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) {
      if (entry.name === 'target' || entry.name === 'node_modules') continue;
      collectSources(path, out);
    } else if (/\.(rs|toml)$/.test(entry.name)) {
      out.push(path);
    }
  }
}

function hashSources() {
  const files = [];
  collectSources(cratesDir, files);
  const lock = join(repoRoot, 'Cargo.lock');
  if (existsSync(lock)) files.push(lock);
  if (files.length === 0) throw new Error(`no Rust sources found under ${cratesDir}`);
  const hash = createHash('sha256');
  hash.update(`v${HASH_VERSION}\n`);
  for (const file of files) {
    hash.update(relative(repoRoot, file));
    hash.update('\u0000');
    hash.update(statSync(file).size.toString());
    hash.update('\u0000');
    hash.update(readFileSync(file));
    hash.update('\u0001');
  }
  return hash.digest('hex');
}

const current = hashSources();
const previous = existsSync(stamp) ? readFileSync(stamp, 'utf8').trim() : '';
const dylibMissing = !existsSync(dylib);

if (current === previous && !dylibMissing) {
  console.log('[ensure-host] host dylib up to date (source hash unchanged), skipping cargo build');
  process.exit(0);
}

if (dylibMissing) {
  console.log('[ensure-host] host dylib missing — building release NAPI module…');
} else {
  console.log('[ensure-host] Rust sources changed since the last dylib build — rebuilding release NAPI module…');
}

// Inherit stdio so the cargo progress is visible in the npm start output; a
// failed build must abort the start (running the stale dylib silently is the
// exact failure this script exists to prevent).
const result = spawnSync('cargo', ['build', '--release', '-p', 'starriver-desktop'], {
  cwd: repoRoot,
  stdio: 'inherit',
});
if (result.status !== 0) {
  console.error('[ensure-host] cargo build --release -p starriver-desktop FAILED — refusing to start on a stale host');
  process.exit(result.status ?? 1);
}

mkdirSync(dirname(stamp), { recursive: true });
writeFileSync(stamp, current + '\n');
console.log('[ensure-host] host dylib rebuilt and stamped; sync-napi will copy + sign it');
