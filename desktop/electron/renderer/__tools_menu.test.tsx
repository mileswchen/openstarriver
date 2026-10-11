/// <reference types="vitest/globals" />
// Toolbar-right tools dropdown: open the workspace in a terminal / IDE.
// Both actions proxy to main-process spawn — the menu only wires cwd +
// optional settings command templates.
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/react';
import { I18nProvider } from './i18n';

declare global { interface Window { starriver: any; } }

beforeEach(() => {
  localStorage.setItem('starriver-language', 'zh');
  window.starriver = {
    getSettings: vi.fn().mockResolvedValue({
      apps: { terminal_command: "open -a iTerm '{dir}'", ide_command: "cursor wait '{dir}'" },
    }),
    openTerminal: vi.fn().mockResolvedValue({ ok: true }),
    openIDE: vi.fn().mockResolvedValue({ ok: true }),
  };
});

import { ToolsMenu } from './app';

function renderMenu(cwd: string | null, onError = vi.fn()) {
  return render(<I18nProvider><ToolsMenu cwd={cwd} onError={onError} /></I18nProvider>);
}

describe('ToolsMenu', () => {
  it('opens the menu and launches the terminal with cwd + settings command template', async () => {
    renderMenu('/tmp/ws');
    fireEvent.click(screen.getByLabelText('tools-menu'));
    // Settings templates load lazily on first open.
    await screen.findByText('命令行控制台');
    expect(window.starriver.getSettings).toHaveBeenCalled();
    fireEvent.click(screen.getByText('命令行控制台'));
    await Promise.resolve();
    expect(window.starriver.openTerminal).toHaveBeenCalledWith('/tmp/ws', "open -a iTerm '{dir}'");
  });

  it('launches the IDE through the same dropdown', async () => {
    renderMenu('/tmp/ws');
    fireEvent.click(screen.getByLabelText('tools-menu'));
    await screen.findByText('打开 IDE');
    fireEvent.click(screen.getByText('打开 IDE'));
    await Promise.resolve();
    expect(window.starriver.openIDE).toHaveBeenCalledWith('/tmp/ws', "cursor wait '{dir}'");
  });

  it('disables both entries without a workspace and surfaces spawn failures', async () => {
    const onError = vi.fn();
    const first = renderMenu(null, onError);
    fireEvent.click(screen.getByLabelText('tools-menu'));
    await screen.findByText(/未绑定工作区/);
    const items = screen.getAllByText(/命令行控制台|打开 IDE/).filter(
      (el) => el.className === 'tools-menu-label',
    );
    for (const item of items) {
      expect((item.closest('button') as HTMLButtonElement).disabled).toBe(true);
    }
    expect(window.starriver.openTerminal).not.toHaveBeenCalled();
    first.unmount();

    // Failure path: a rejected spawn surfaces via onError.
    renderMenu('/tmp/ws2', onError);
    (window.starriver.openTerminal as ReturnType<typeof vi.fn>).mockRejectedValueOnce(
      new Error('no terminal emulator found on PATH'),
    );
    fireEvent.click(screen.getByLabelText('tools-menu'));
    await screen.findByText('命令行控制台');
    fireEvent.click(screen.getByText('命令行控制台'));
    await Promise.resolve();
    await Promise.resolve();
    expect(onError).toHaveBeenCalledWith(expect.stringContaining('打开失败'));
  });
});
