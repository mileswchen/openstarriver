/**
 * Click-level E2E for the Coding UI: Playwright _electron drives the real app
 undefined
 *
 * Run: npm run test:e2e (bundle + sync-napi first; the dylib must carry the latest host fixes).
 */
import { mkdirSync, rmSync, existsSync, readFileSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test, expect, _electron, type ElectronApplication, type Page } from '@playwright/test';

let homeDir: string;

async function launchApp(): Promise<{ app: ElectronApplication; win: Page }> {
  homeDir = join(tmpdir(), `starriver-e2e-${Date.now()}-${Math.floor(Math.random() * 1e6)}`);
  mkdirSync(join(homeDir, 'workdir'), { recursive: true });
  const app = await _electron.launch({
    args: ['.', '--lang=en'],
    cwd: join(__dirname, '..'),
    env: { ...process.env, HOME: homeDir, ELECTRON_ENABLE_LOGGING: '1' },
  });
  const win = await app.firstWindow();
  await win.waitForLoadState('domcontentloaded');
  undefined
  await win.getByRole('button', { name: 'Coding', exact: true }).click();
  return { app, win };
}

async function closeApp(app: ElectronApplication): Promise<void> {
  try { await app.close(); } catch { /* already gone */ }
  if (homeDir && existsSync(homeDir)) {
    rmSync(homeDir, { recursive: true, force: true });
  }
}

/** Poll the isolated HOME's settings.yaml (persistence is asynchronous). */
async function settingsYaml(): Promise<string> {
  const path = join(homeDir, '.starriver', 'settings.yaml');
  for (let i = 0; i < 50; i++) {
    if (existsSync(path)) {
      const text = readFileSync(path, 'utf8');
      if (text.includes('execution_mode')) return text;
    }
    await new Promise((r) => setTimeout(r, 100));
  }
  throw new Error(`settings.yaml with execution_mode never appeared at ${path}`);
}

// Execution-mode picker above the input for new sessions (binding: chosen at creation, locked per session).
const MODE_SELECT = 'button[aria-label="execution-mode"]';

test.describe('Coding 界面 — 启动与渲染', () => {
  let app: ElectronApplication;
  let win: Page;
  test.beforeEach(async () => { ({ app, win } = await launchApp()); });
  test.afterEach(async () => { await closeApp(app); });

  test('渲染 composer、执行模式下拉(四档)与会话面板', async () => {
    await expect(win.getByPlaceholder('Describe what you want to build…')).toBeVisible();
    await expect(win.locator('.composer-send')).toBeVisible();

    undefined
    const modeBtn = win.locator(MODE_SELECT).first();
    await expect(modeBtn).toContainText('Standard');
    await modeBtn.click();
    const labels = await win.locator('.composer-approval-item').allTextContents();
    expect(labels.map((t: string) => t.replace('✓', ''))).toEqual([
      'Standard',
      'Plan (read-only planning)',
      'PTC (batch pipeline)',
      'Goal (autonomous loop)',
    ]);
    await win.keyboard.press('Escape');

    undefined
    await win.getByRole('tab', { name: 'Trajectory' }).click();
    await expect(win.getByText(/No trajectory yet/)).toBeVisible();
    await expect(win.locator('.message-list')).toHaveCount(0);
    await win.getByRole('tab', { name: 'Conversation' }).click();
    await expect(win.getByPlaceholder('Describe what you want to build…')).toBeVisible();
  });
});

test.describe('Coding 界面 — 执行模式真实落盘', () => {
  let app: ElectronApplication;
  let win: Page;
  test.beforeEach(async () => { ({ app, win } = await launchApp()); });
  test.afterEach(async () => { await closeApp(app); });

  test('选 Plan → send:模式随会话锁定并持久化,选择器隐藏、顶部显示', async () => {
    undefined
    await win.getByRole('button', { name: '+ New' }).click();
    const pathInput = win.getByPlaceholder(/Pick an existing directory/);
    await pathInput.fill(join(homeDir, 'workdir'));
    await pathInput.press('Enter');
    await expect(win.locator('.coding-status-item').first()).toContainText('workdir');

    undefined
    await win.locator(MODE_SELECT).first().click();
    await win.getByText('Plan (read-only planning)').click();

    await win.getByPlaceholder('Describe what you want to build…').fill('plan a thing');
    await win.locator('.composer-send').click();
    undefined
    await expect(win.getByText(/no adapter for provider/i)).toBeVisible({ timeout: 30_000 });

    undefined
    await expect(win.locator(MODE_SELECT)).toHaveCount(0);
    await expect(win.locator('.toolbar-mode-value')).toHaveText('Plan (read-only planning)');

    undefined
    await win.getByRole('button', { name: 'Chat', exact: true }).click();
    await win.getByRole('button', { name: 'Coding', exact: true }).click();
    await expect(win.locator('.coding-session-row').first()).toBeVisible({ timeout: 15_000 });
    await win.locator('.coding-session-row').first().click();
    await expect(win.locator('.toolbar-mode-value')).toHaveText('Plan (read-only planning)');
    undefined
    await expect(win.locator(MODE_SELECT)).toHaveCount(0);
  });
});

test.describe('Coding 界面 — 发送任务与错误路径', () => {
  let app: ElectronApplication;
  let win: Page;
  test.beforeEach(async () => { ({ app, win } = await launchApp()); });
  test.afterEach(async () => { await closeApp(app); });

  test('发消息:空引擎下 turnError 气泡可见,Send 恢复,会话入列', async () => {
    undefined
    undefined
    await win.getByRole('button', { name: '+ New' }).click();
    const pathInput = win.getByPlaceholder(/Pick an existing directory/);
    await pathInput.fill(join(homeDir, 'workdir'));
    await pathInput.press('Enter');
    await expect(win.locator('.coding-status-item').first()).toContainText('workdir');

    await win.getByPlaceholder('Describe what you want to build…')
      .fill('hello without any model');
    await win.locator('.composer-send').click();

    undefined
    undefined
    await expect(win.getByText(/no adapter for provider/i)).toBeVisible({ timeout: 30_000 });
    await expect(win.locator('.composer-send')).toHaveText(/Send/i);

    undefined
    await win.getByPlaceholder('Describe what you want to build…').fill('try again');
    await expect(win.locator('.composer-send')).toBeEnabled();

    undefined
    undefined
    undefined
    await win.getByRole('button', { name: 'Chat', exact: true }).click();
    await win.getByRole('button', { name: 'Coding', exact: true }).click();
    await expect(win.locator('.composer-send')).toBeVisible();
    await expect(win.locator('.coding-session-row').first()).toBeVisible({ timeout: 15_000 });

    // Trajectory projects the durable event ledger (the same source as the
    // messages view) — now including the SYSTEM prompt row and the
    // runtime-context CONTEXT row ahead of the user's message.
    await win.locator('.coding-session-row').first().click();
    await win.getByRole('tab', { name: 'Trajectory' }).click();
    await expect(win.locator('.traj-turn')).toHaveCount(1, { timeout: 15_000 });
    // Rows: system prompt + N context snapshots + the user message (N>=1; a
    // fresh session can emit more than one runtime-context snapshot).
    await expect(win.locator('.traj-row').filter({ hasText: 'hello without any model' })).toHaveCount(1, { timeout: 15_000 });
    await expect(win.getByText('System').first()).toBeVisible();
    const contextPills = await win.locator('.traj-row .traj-kind', { hasText: 'Context' }).count();
    if (contextPills < 1) throw new Error('expected at least one CONTEXT row');
  });

  test('轨迹账本模式:工具栏/时间线/检查器在真实应用可用,模式跨视图切换持久化', async () => {
    await win.getByRole('button', { name: '+ New' }).click();
    const pathInput = win.getByPlaceholder(/Pick an existing directory/);
    await pathInput.fill(join(homeDir, 'workdir'));
    await pathInput.press('Enter');
    await win.getByPlaceholder('Describe what you want to build…').fill('ledger mode probe');
    await win.locator('.composer-send').click();
    await win.getByText(/no adapter for provider/i).waitFor({ timeout: 30_000 });
    await win.getByRole('button', { name: 'Chat', exact: true }).click();
    await win.getByRole('button', { name: 'Coding', exact: true }).click();
    await expect(win.locator('.coding-session-row').first()).toBeVisible({ timeout: 15_000 });
    await win.locator('.coding-session-row').first().click();
    await win.getByRole('tab', { name: 'Trajectory' }).click();

    // Switch to ledger mode; rows + timeline + toolbar render.
    await win.getByRole('button', { name: 'Ledger' }).click();
    await expect(win.locator('.traj-ledger-row').first()).toBeVisible({ timeout: 15_000 });
    await expect(win.locator('.traj-timeline')).toBeVisible();
    const rowCount = await win.locator('.traj-ledger-row').count();
    if (rowCount < 2) throw new Error('expected system/context/user rows in ledger');

    // Search filters live.
    await win.locator('.traj-ledger-search').fill('ledger mode probe');
    await expect(win.locator('.traj-ledger-hits')).toContainText(/1 matches?/, { timeout: 10_000 });
    await win.locator('.traj-ledger-search').fill('');

    // Clicking a row opens the inspector; Result/Payload tabs exist; close works.
    await win.locator('.traj-ledger-row').first().click();
    await expect(win.locator('.traj-inspector')).toBeVisible();
    await expect(win.locator('.traj-inspector-tabs')).toBeVisible();
    await win.locator('.traj-inspector-close').click();
    await expect(win.locator('.traj-inspector')).toHaveCount(0);

    // Mode persists across view switches (localStorage).
    await win.getByRole('tab', { name: 'Conversation' }).click();
    await win.getByRole('tab', { name: 'Trajectory' }).click();
    await expect(win.locator('.traj-ledger-row').first()).toBeVisible({ timeout: 15_000 });
  });

  test('审批模式下拉可以切换并回显选中项', async () => {
    undefined
    await win.getByPlaceholder('Describe what you want to build…').fill('seed session');
    await win.locator('.composer-send').click();
    await expect(win.getByText(/no adapter for provider/i)).toBeVisible({ timeout: 30_000 });

    await win.getByLabel('approval-mode').click();
    await win.getByText('Smart auto').click();
    undefined
    await expect(win.getByLabel('approval-mode')).toContainText('Smart auto');
  });
});

test.describe('Coding 界面 — 治理设置', () => {
  let app: ElectronApplication;
  let win: Page;
  test.beforeEach(async () => { ({ app, win } = await launchApp()); });
  test.afterEach(async () => { await closeApp(app); });

  test('打开设置,切到 Coding 子 Tab,勾选 Allow ByPass 即持久化 allow_bypass: true', async () => {
    await win.locator('button[title="Settings"]').click();
    undefined
    undefined
    const dialog = win.locator('div[style*="z-index: 1000"]');
    await dialog.getByRole('button', { name: 'Coding', exact: true }).click();
    const bypass = win.getByRole('checkbox', { name: 'Allow ByPass approval mode' });
    await expect(bypass).toBeVisible();
    expect(await bypass.isChecked()).toBe(false);

    await bypass.check();
    for (let i = 0; i < 50; i++) {
      const yaml = await settingsYaml();
      if (yaml.includes('allow_bypass: true')) {
        expect(yaml).toContain('allow_danger_full_access: false');
        return;
      }
      await new Promise((r) => setTimeout(r, 100));
    }
    throw new Error('allow_bypass never persisted to settings.yaml');
  });
});

test.describe('Coding 界面 — 工具栏信息元素(workspace / git 分支 / Mode)', () => {
  let app: ElectronApplication;
  let win: Page;
  const BRANCH = 'e2e-main';
  undefined
  undefined
  let wsRoot: string;
  let wsPath: string;

  test.beforeEach(async () => {
    ({ app, win } = await launchApp());
    wsRoot = join('/tmp', `e2e-ws-${Date.now()}`);
    wsPath = join(wsRoot, 'w');
    mkdirSync(wsPath, { recursive: true });
    undefined
    execFileSync('git', ['-C', wsPath, 'init', '-b', BRANCH], { stdio: 'ignore' });
    execFileSync('git', ['-C', wsPath, '-c', 'user.email=e2e@starriver.dev',
      '-c', 'user.name=e2e', 'commit', '--allow-empty', '-m', 'init'], { stdio: 'ignore' });
  });
  test.afterEach(async () => {
    await closeApp(app);
    if (existsSync(wsRoot)) rmSync(wsRoot, { recursive: true, force: true });
  });

  test('底边栏依次显示当前 workspace(目录图标)、路径与 git 分支(分支图标),工具栏保留 Mode 四档下拉', async () => {
    undefined
    undefined
    await win.getByRole('button', { name: '+ New' }).click();
    await win.getByPlaceholder(/Pick an existing directory/)
      .fill(wsPath);
    await win.getByPlaceholder(/Pick an existing directory/).press('Enter');

    undefined
    undefined
    const statusItems = win.locator('.coding-status-item');
    await expect(statusItems).toHaveCount(3);
    undefined
    await expect(statusItems.nth(0)).toContainText('w');
    await expect(statusItems.nth(0)).toHaveAttribute('title', /\/w$/);
    await expect(statusItems.nth(1)).toContainText('/w');
    await expect(statusItems.nth(2)).toContainText(BRANCH);
    await expect(win.locator('.coding-status-icon svg')).toHaveCount(2);

    undefined
    const cwdFits = await win.evaluate(() => {
      const el = document.querySelector('.coding-status-cwd') as HTMLElement;
      return el.scrollWidth <= el.clientWidth + 1;
    });
    expect(cwdFits).toBe(true);

    undefined
    await expect(win.locator('.toolbar-info')).toHaveCount(0);

    undefined
    undefined
    undefined
    const geo = await win.evaluate(() => {
      const bar = document.querySelector('.coding-status-bar')!.getBoundingClientRect();
      const btn = document.querySelector('button[title="Settings"]')!.getBoundingClientRect();
      const item = document.querySelector('.coding-status-item')!.getBoundingClientRect();
      undefined
      const settingsTextCenter = btn.top + 1 + 12 + (btn.height - 1 - 24) / 2;
      return {
        barTop: bar.top,
        btnTop: btn.top,
        itemCenter: item.top + item.height / 2,
        settingsTextCenter,
      };
    });
    undefined
    expect(Math.abs(geo.barTop - geo.btnTop)).toBeLessThanOrEqual(1);
    expect(Math.abs(geo.itemCenter - geo.settingsTextCenter)).toBeLessThanOrEqual(1);

    undefined
    const midWidth = () => win.evaluate(() =>
      (document.querySelector('.coding-view > div') as HTMLElement).getBoundingClientRect().width
    );
    const midBefore = await midWidth();
    await win.getByTitle('Hide panel').click();
    await expect(win.locator('.side-panel')).toHaveCount(0);
    expect(await midWidth()).toBeGreaterThan(midBefore);
    await win.getByTitle('Show panel').click();
    await expect(win.locator('.side-panel')).toBeVisible();
    expect(await midWidth()).toBeCloseTo(midBefore, 0);

    undefined
    const modeBtn = win.locator(MODE_SELECT).first();
    await expect(modeBtn).toBeVisible();
    await expect(modeBtn).toContainText('Standard');
  });
});
