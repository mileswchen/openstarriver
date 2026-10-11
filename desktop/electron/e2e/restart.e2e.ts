/**
 * Diagnostic probe: register a workspace → quit → second launch; verifies the active-workspace persistence chain.
 undefined
 */
import { mkdirSync, rmSync, existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test, expect, _electron, type ElectronApplication, type Page } from '@playwright/test';

let homeDir: string;

async function launch(reuseHome = false): Promise<{ app: ElectronApplication; win: Page }> {
  if (!reuseHome) {
    homeDir = join(tmpdir(), `starriver-restart-${Date.now()}-${Math.floor(Math.random() * 1e6)}`);
  }
  mkdirSync(join(homeDir, 'workdir'), { recursive: true });
  const app = await _electron.launch({
    args: ['.', '--lang=en'],
    cwd: join(__dirname, '..'),
    env: { ...process.env, HOME: homeDir, ELECTRON_ENABLE_LOGGING: '1' },
  });
  const win = await app.firstWindow();
  await win.waitForLoadState('domcontentloaded');
  await win.getByRole('button', { name: 'Coding', exact: true }).click();
  return { app, win };
}

test('注册工作区后重启,工具栏 chip 恢复显示', async () => {
  test.setTimeout(180_000);

  undefined
  const first = await launch();
  await first.win.getByRole('button', { name: '+ New' }).click();
  const input = first.win.getByPlaceholder(/Pick an existing directory/);
  await input.fill(join(homeDir, 'workdir'));
  await input.press('Enter');
  await expect(first.win.locator('.coding-status-item').first()).toContainText('workdir');
  const id1 = await first.win.evaluate(() => window.starriver.getActiveWorkspace());
  console.log('[probe] id1 =', JSON.stringify(id1));
  await first.app.close();

  undefined
  const second = await launch(true);
  const id2 = await second.win.evaluate(() => window.starriver.getActiveWorkspace());
  console.log('[probe] id2 =', JSON.stringify(id2));
  const infos = second.win.locator('.coding-status-item');
  await expect(infos.first()).toContainText('workdir', { timeout: 15_000 });
  await second.app.close();

  expect(id1).toBe(id2);
  expect(id2).toContain('workdir');
});

test.afterAll(() => {
  if (homeDir && existsSync(homeDir)) rmSync(homeDir, { recursive: true, force: true });
});
