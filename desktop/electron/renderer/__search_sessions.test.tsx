// WI-149: the New Task search box searches SESSIONS — typing filters the
// session list by title, picking a result opens that session (provider
// activeSessionId + onOpenSession → Coding tab).
import { describe, it, expect, vi, beforeEach } from 'vitest';
import React from 'react';
import { render, screen, fireEvent, waitFor } from '@testing-library/react';
import { I18nProvider } from './i18n';
import { SearchBox, WorkspaceProvider, useWorkspaces } from './app';

declare global { interface Window { starriver: any; } }

const SESSIONS = [
  { id: 's1', title: 'font task' },
  { id: 's2', title: '打包脚本修复' },
];

// Captures the provider's setActiveSessionId call (the session-open side effect).
let openedId: string | null = null;
function Harness({ onOpenSession }: { onOpenSession: () => void }) {
  return (
    <I18nProvider>
      <WorkspaceProvider>
        <Probe />
        <SearchBox onOpenSession={onOpenSession} />
      </WorkspaceProvider>
    </I18nProvider>
  );
}

// SearchBox uses useWorkspaces internally — the probe reads the same context
// instance to capture the session-open side effect.
function Probe() {
  const ws = useWorkspaces();
  return <button data-testid="probe" onClick={() => { openedId = ws.activeSessionId ?? ''; }} />;
}

beforeEach(() => {
  Element.prototype.scrollIntoView = vi.fn();
  localStorage.setItem('starriver-language', 'en');
  window.starriver = {
    listSessions: vi.fn().mockResolvedValue(SESSIONS),
  };
});

describe('New Task search box — session search (WI-149)', () => {
  it('filters sessions by title and opens the picked one', async () => {
    const onOpenSession = vi.fn();
    render(<Harness onOpenSession={onOpenSession} />);

    const input = screen.getByPlaceholderText('Search sessions...');
    fireEvent.change(input, { target: { value: 'font' } });
    const hit = await screen.findByText('font task');
    expect(screen.queryByText('打包脚本修复')).toBeNull();

    fireEvent.mouseDown(hit);
    expect(onOpenSession).toHaveBeenCalled();
    // The provider recorded the session-open (probe reads the same context).
    fireEvent.click(screen.getByTestId('probe'));
    expect(openedId).toBe('s1');
  });

  it('no match renders no dropdown rows', async () => {
    render(<Harness onOpenSession={vi.fn()} />);
    const input = screen.getByPlaceholderText('Search sessions...');
    fireEvent.change(input, { target: { value: 'nope' } });
    await waitFor(() => expect(screen.queryByText('font task')).toBeNull());
  });
});
