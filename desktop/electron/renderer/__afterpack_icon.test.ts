// Integration tests for the macOS afterPack icon hook (scripts/afterPack-mac.js).
// The hook is CJS and reads its deps via require(), so we spy on the REAL
// builtin export objects obtained through createRequire (ESM namespace
// mocks do not reach require consumers). No real actool/PlistBuddy runs.
// BUG-P20/P21 are regression tests for defects found in the 2026-11-20
// temp-dir/cleanup hardening of this hook.
import { describe, it, expect, vi, beforeAll, beforeEach, afterEach } from 'vitest';
import { createRequire } from 'module';

const nodeRequire = createRequire(import.meta.url);
const fs = nodeRequire('fs');
const cp = nodeRequire('child_process');

// The hook destructures execFileSync at module load, so the spy must exist
// BEFORE the hook is imported, and must never be restored (the captured
// binding would fall through to the real spawnSync).
let afterPack: (ctx: any) => Promise<void>;
beforeAll(async () => {
  vi.spyOn(cp, 'execFileSync').mockImplementation(() => Buffer.from(''));
  afterPack = (await import('../scripts/afterPack-mac.js')).default;
});

const ctx = {
  electronPlatformName: 'darwin',
  appOutDir: '/out',
  packager: { appInfo: { productFilename: 'Starriver' } },
};

let carExists = true;
let warnSpy: ReturnType<typeof vi.spyOn>;
let logSpy: ReturnType<typeof vi.spyOn>;

beforeEach(() => {
  carExists = true;
  cp.execFileSync.mockImplementation(() => Buffer.from(''));
  vi.spyOn(fs, 'existsSync').mockImplementation((p: any) => {
    const s = String(p);
    if (s.endsWith('AppIcon.icon')) return true;
    if (s.endsWith('Assets.car')) return carExists;
    return false;
  });
  vi.spyOn(fs, 'mkdtempSync').mockReturnValue('/fake-tmp/starriver-icon-x');
  vi.spyOn(fs, 'copyFileSync').mockImplementation(() => {});
  vi.spyOn(fs, 'rmSync').mockImplementation(() => {});
  warnSpy = vi.spyOn(console, 'warn').mockImplementation(() => {});
  logSpy = vi.spyOn(console, 'log').mockImplementation(() => {});
});

afterEach(() => {
  cp.execFileSync.mockReset(); // keep the spy wrapper; drop impl + calls
  fs.existsSync.mockRestore();
  fs.mkdtempSync.mockRestore();
  fs.copyFileSync.mockRestore();
  fs.rmSync.mockRestore();
  warnSpy.mockRestore();
  logSpy.mockRestore();
});

describe('afterPack icon hook — happy paths', () => {
  it('compiles AppIcon.icon, copies Assets.car and sets CFBundleIconName', async () => {
    await afterPack(ctx);
    const calls = cp.execFileSync.mock.calls;
    expect(calls[0][0]).toBe('xcrun');
    expect(calls[0][1]).toEqual(expect.arrayContaining(['actool', '--app-icon', 'AppIcon']));
    expect(fs.copyFileSync).toHaveBeenCalledWith(
      '/fake-tmp/starriver-icon-x/Assets.car',
      '/out/Starriver.app/Contents/Resources/Assets.car',
    );
    expect(calls[1][0]).toBe('/usr/libexec/PlistBuddy');
    expect(calls[1][1]).toEqual(['-c', 'Set :CFBundleIconName AppIcon', '/out/Starriver.app/Contents/Info.plist']);
    expect(logSpy).toHaveBeenCalledWith(expect.stringContaining('modern icon'));
    expect(fs.rmSync).toHaveBeenCalledWith('/fake-tmp/starriver-icon-x', { recursive: true, force: true });
  });

  it('falls back to PlistBuddy Add when Set fails (key absent)', async () => {
    cp.execFileSync.mockImplementation((cmd: any, args: any) => {
      if (cmd === '/usr/libexec/PlistBuddy' && args[1].startsWith('Set')) throw new Error('key not found');
      return Buffer.from('');
    });
    await afterPack(ctx);
    const plistCalls = cp.execFileSync.mock.calls.filter((c: any[]) => c[0] === '/usr/libexec/PlistBuddy');
    expect(plistCalls).toHaveLength(2);
    expect(plistCalls[1][1]).toEqual(['-c', 'Add :CFBundleIconName string AppIcon', '/out/Starriver.app/Contents/Info.plist']);
  });

  it('skips non-darwin platforms entirely', async () => {
    await afterPack({ ...ctx, electronPlatformName: 'win32' });
    expect(cp.execFileSync).not.toHaveBeenCalled();
  });

  it('skips silently when AppIcon.icon is absent (legacy icns path)', async () => {
    fs.existsSync.mockReturnValue(false);
    await afterPack(ctx);
    expect(cp.execFileSync).not.toHaveBeenCalled();
    expect(warnSpy).not.toHaveBeenCalled();
  });

  it('warns and keeps the legacy icon when actool produces no Assets.car', async () => {
    carExists = false;
    await afterPack(ctx);
    expect(fs.copyFileSync).not.toHaveBeenCalled();
    expect(warnSpy).toHaveBeenCalledWith(expect.stringContaining('no Assets.car'));
  });
});

describe('afterPack icon hook — BUG-P20/P21 regressions', () => {
  it('BUG-P20: a failing temp-dir cleanup must never break packaging', async () => {
    fs.rmSync.mockImplementation(() => { throw new Error('EBUSY: resource busy'); });
    // The hook's contract: "packaging never breaks because of this hook".
    // A throw in the finally block used to reject the whole afterPack.
    await expect(afterPack(ctx)).resolves.toBeUndefined();
    expect(fs.copyFileSync).toHaveBeenCalled(); // the icon work itself succeeded
  });

  it('BUG-P21: actool failure diagnostics (stderr) reach the warning log', async () => {
    const err: any = new Error('Command failed: xcrun actool');
    err.stderr = Buffer.from('actool: error: cannot decode AppIcon.icon');
    cp.execFileSync.mockImplementation(() => { throw err; });
    await afterPack(ctx);
    expect(warnSpy).toHaveBeenCalledWith(expect.stringContaining('cannot decode AppIcon.icon'));
  });
});