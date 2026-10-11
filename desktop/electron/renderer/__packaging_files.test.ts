/// <reference types="vitest/globals" />
// BUG-95 regression guard: every local module required by the compiled main
// process (main.js → daemon.js → protocol.schema.json) must be listed in
// electron-builder.yml `files`, or the packaged app crashes on launch with
// "Cannot find module" while the dev machine (loose files on disk) never
// notices.
import { readFileSync } from 'node:fs';
import { join } from 'node:path';

const HERE = join(__dirname, '..');

function localRequires(file: string): string[] {
  const src = readFileSync(join(HERE, file), 'utf8');
  return [...src.matchAll(/require\("\.\/([^"]+)"\)/g)].map((m) => m[1]);
}

function builderFiles(): string[] {
  const yml = readFileSync(join(HERE, 'electron-builder.yml'), 'utf8');
  const lines = yml.split('\n');
  const start = lines.findIndex((l) => l === 'files:');
  const out: string[] = [];
  for (let i = start + 1; i < lines.length; i++) {
    if (/^  #/.test(lines[i])) continue; // comment inside the block
    const m = lines[i].match(/^  - (.+)$/);
    if (!m) break;
    out.push(m[1].trim());
  }
  return out;
}

/** require("./daemon") may be satisfied by a `daemon.js`/`daemon.json` entry. */
function isListed(files: string[], dep: string): boolean {
  return (
    files.includes(dep) ||
    files.includes(`${dep}.js`) ||
    files.includes(`${dep}.json`)
  );
}

describe('packaging: electron-builder files cover every local require', () => {
  it('asar `files` includes main.js local deps and their transitive json loads', () => {
    const files = builderFiles();
    expect(files).toContain('main.js');
    expect(files).toContain('preload.js');
    for (const file of ['main.js', 'preload.js']) {
      for (const dep of localRequires(file)) {
        expect(isListed(files, dep), `${file} requires ./${dep}`).toBe(true);
      }
    }
    // daemon.js loads the schema via require("./protocol.schema.json") too —
    // and it is itself required by main.js, so both must be packed.
    for (const dep of localRequires('daemon.js')) {
      expect(isListed(files, dep), `daemon.js requires ./${dep}`).toBe(true);
    }
  });
});
