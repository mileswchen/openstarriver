/// <reference types="vitest/globals" />
import { describe, it, expect, beforeAll, afterAll, vi, beforeEach } from 'vitest';
import React from 'react';
import { render, screen, fireEvent } from '@testing-library/react';
import { BackendErrorScreen } from './app';
import { I18nProvider } from './i18n';

const originalStarriver = (window as any).starriver;
const retryInit = vi.fn();
const quitApp = vi.fn();

beforeAll(() => {
  (window as any).starriver = { retryInit, quitApp };
});
afterAll(() => {
  (window as any).starriver = originalStarriver;
});
beforeEach(() => {
  localStorage.setItem('starriver-language', 'en');
  vi.clearAllMocks();
});

const renderScreen = (error: string) =>
  render(
    <I18nProvider>
      <BackendErrorScreen error={error} />
    </I18nProvider>,
  );

// WI-31 error finalization: every observed failure class renders an
// actionable classified message instead of raw developer English.
describe('BackendErrorScreen (WI-31 error finalization)', () => {
  it('classifies a missing daemon binary and shows the build hint', () => {
    renderScreen('starriverd binary not found — run `cargo build -p starriverd`');
    expect(screen.getByText('The starriverd daemon binary was not found.')).toBeTruthy();
    expect(screen.getByText(/Build the daemon \(cargo build -p starriverd\)/)).toBeTruthy();
  });

  it('classifies spawn failure, hints at the stderr detail and preserves the raw error', () => {
    renderScreen('starriverd exited before reporting (code 1): Address already in use (os error 48)');
    expect(screen.getByText('The starriverd daemon exited during startup.')).toBeTruthy();
    expect(screen.getByText(/port conflicts are listed there/i)).toBeTruthy();
    const raw = document.querySelector('details pre');
    expect(raw?.textContent).toContain('Address already in use (os error 48)');
  });

  it('classifies token mismatch and guides to daemon.json removal', () => {
    renderScreen('missing or invalid token');
    expect(screen.getByText(/token mismatch/i)).toBeTruthy();
    expect(screen.getByText(/~\/\.starriver\/daemon\.json/)).toBeTruthy();
  });

  it('classifies protocol version mismatch', () => {
    renderScreen('protocol version mismatch: daemon v1 vs client v2');
    expect(screen.getByText(/protocol versions do not match/i)).toBeTruthy();
  });

  it('falls back to the generic copy for unknown errors', () => {
    renderScreen('some novel failure');
    expect(screen.getByText('Backend initialization failed.')).toBeTruthy();
  });

  it('retry triggers retryInit and quit triggers quitApp', () => {
    renderScreen('whatever');
    fireEvent.click(screen.getByRole('button', { name: /Retry \(restarts the app\)/ }));
    expect(retryInit).toHaveBeenCalledTimes(1);
    fireEvent.click(screen.getByRole('button', { name: 'Quit Starriver' }));
    expect(quitApp).toHaveBeenCalledTimes(1);
  });
});
