/// <reference types="vitest/globals" />
import { describe, it, expect, vi, beforeEach } from 'vitest';
import React from 'react';
import { render, screen, waitFor, fireEvent } from '@testing-library/react';
import { I18nProvider } from './i18n';

declare global { interface Window { starriver: any; } }

const HOOKS = [
  { event: 'PreToolUse', name: 'all.sh', size: 10, modifiedSecs: 2 },
  { event: 'PreToolUse', name: 'bash-audit.sh', size: 42, modifiedSecs: 1 },
  { event: 'PostToolUse', name: 'notify.sh', size: 30, modifiedSecs: 3 },
];

beforeEach(() => {
  localStorage.setItem('starriver-language', 'en');
  vi.stubGlobal('confirm', vi.fn(() => true));
  window.starriver = {
    listHooks: vi.fn().mockImplementation(() => Promise.resolve(HOOKS.map((h) => ({ ...h })))),
    readHook: vi.fn().mockResolvedValue('#!/bin/sh\nexit 0\n'),
    writeHook: vi.fn().mockResolvedValue(undefined),
    deleteHook: vi.fn().mockResolvedValue(undefined),
  };
});

describe('HooksTab', () => {
  it('lists scripts grouped by event with tool badges', async () => {
    const { HooksTab } = await import('./app');
    render(<I18nProvider><HooksTab /></I18nProvider>);
    await waitFor(() => expect(screen.getByText('bash-audit.sh')).toBeTruthy());
    expect(screen.getByText('notify.sh')).toBeTruthy();
    // all.sh matches every tool → the "All tools" badge, no per-tool tokens.
    expect(screen.getByText('All tools')).toBeTruthy();
    expect(screen.getByText('bash')).toBeTruthy();
  });

  it('shows the env-var quick reference when no hooks exist', async () => {
    window.starriver.listHooks = vi.fn().mockResolvedValue([]);
    const { HooksTab } = await import('./app');
    render(<I18nProvider><HooksTab /></I18nProvider>);
    await waitFor(() => expect(screen.getByText('No hook scripts yet')).toBeTruthy());
    expect(screen.getByText(/STARRIVER_PARAMS_FILE/)).toBeTruthy();
    expect(screen.getByText(/STARRIVER_RESULT_FILE/)).toBeTruthy();
  });

  it('creates a hook via the editor modal (auto-appends .sh)', async () => {
    const { HooksTab } = await import('./app');
    render(<I18nProvider><HooksTab /></I18nProvider>);
    await waitFor(() => expect(screen.getByText('+ New Hook')).toBeTruthy());
    fireEvent.click(screen.getByText('+ New Hook'));
    const nameInput = screen.getByPlaceholderText('bash-audit.sh') as HTMLInputElement;
    fireEvent.change(nameInput, { target: { value: 'audit' } });
    const textarea = document.querySelector('textarea') as HTMLTextAreaElement;
    fireEvent.change(textarea, { target: { value: '#!/bin/sh\nexit 0\n' } });
    fireEvent.click(screen.getByText('Save'));
    await waitFor(() => expect(window.starriver.writeHook)
      .toHaveBeenCalledWith('PreToolUse', 'audit.sh', '#!/bin/sh\nexit 0\n'));
  });

  it('edits an existing hook after loading its content', async () => {
    const { HooksTab } = await import('./app');
    render(<I18nProvider><HooksTab /></I18nProvider>);
    await waitFor(() => expect(screen.getByText('bash-audit.sh')).toBeTruthy());
    // First row is all.sh (list order) — edit it.
    fireEvent.click(screen.getAllByText('Edit')[0]);
    await waitFor(() => expect(window.starriver.readHook)
      .toHaveBeenCalledWith('PreToolUse', 'all.sh'));
    const textarea = document.querySelector('textarea') as HTMLTextAreaElement;
    fireEvent.change(textarea, { target: { value: '#!/bin/sh\nexit 1\n' } });
    fireEvent.click(screen.getByText('Save'));
    await waitFor(() => expect(window.starriver.writeHook)
      .toHaveBeenCalledWith('PreToolUse', 'all.sh', '#!/bin/sh\nexit 1\n'));
  });

  it('deletes a hook after confirmation', async () => {
    const { HooksTab } = await import('./app');
    render(<I18nProvider><HooksTab /></I18nProvider>);
    await waitFor(() => expect(screen.getByText('bash-audit.sh')).toBeTruthy());
    fireEvent.click(screen.getAllByText('Delete')[0]);
    await waitFor(() => expect(window.starriver.deleteHook)
      .toHaveBeenCalledWith('PreToolUse', 'all.sh'));
  });
});
