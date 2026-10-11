/// <reference types="vitest/globals" />
// WI-179: sidebar session rows show a blinking dot (.sb-run-dot) while the
// session has a live turn — the running set merges jobs.list active runs
// (scheduled) and sessions.running (interactive).
import { describe, it, expect, beforeAll, afterAll, vi } from 'vitest';
import React from 'react';
import { render, screen, waitFor } from '@testing-library/react';
import { WorkspaceProvider, WorkspaceSection } from './app';
import { I18nProvider } from './i18n';

const SESSIONS = [
  { id: 's-run', title: 'Run Task', workspaceId: 'w1', createdAt: '2026-10-04T10:00:00Z', messageCount: 2 },
  { id: 's-idle', title: 'Idle Task', workspaceId: 'w1', createdAt: '2026-10-04T10:01:00Z', messageCount: 1 },
];

const baseMock: Record<string, any> = {
  listWorkspaces: () => Promise.resolve([
    { id: 'w1', path: '/tmp/ws', title: 'WS', createdAt: '2026-10-04T09:00:00Z', sessionIds: ['s-run', 's-idle'] },
  ]),
  listSessions: () => Promise.resolve(SESSIONS),
  listArchivedSessions: () => Promise.resolve([]),
  listRunningSessions: () => Promise.resolve([]),
  listAllTaskRuns: () => Promise.resolve([]),
  listJobs: () => Promise.resolve([
    { taskName: 'Hourly Weather', status: 'running', startedAt: '2026-10-04T10:02:00Z', summary: '', sessionId: 's-run' },
  ]),
  setActiveWorkspace: () => Promise.resolve(),
  toggleWorkspace: undefined,
};

const originalStarriver = (window as any).starriver;
beforeAll(() => {
  (window as any).starriver = baseMock;
});
afterAll(() => {
  (window as any).starriver = originalStarriver;
});

function renderSidebar() {
  return render(
    <I18nProvider>
      <WorkspaceProvider>
        <WorkspaceSection onOpenCoding={() => {}} />
      </WorkspaceProvider>
    </I18nProvider>,
  );
}

function rowFor(title: string): HTMLElement | null {
  return Array.from(document.querySelectorAll('.sb-ws-session'))
    .find(row => row.textContent?.includes(title)) ?? null;
}

describe('WI-179 — sidebar running-session blinking dot', () => {
  it('renders .sb-run-dot on sessions with an active run (jobs.list)', async () => {
    renderSidebar();
    await waitFor(() => {
      expect(screen.getByText('Run Task')).toBeTruthy();
    });
    await waitFor(() => {
      expect(rowFor('Run Task')?.querySelector('.sb-run-dot')).toBeTruthy();
    });
  });

  it('does not render the dot on idle sessions', async () => {
    renderSidebar();
    await waitFor(() => {
      expect(screen.getByText('Idle Task')).toBeTruthy();
    });
    await waitFor(() => {
      // The poll has landed (the running row carries its dot).
      expect(rowFor('Run Task')?.querySelector('.sb-run-dot')).toBeTruthy();
    });
    expect(rowFor('Idle Task')?.querySelector('.sb-run-dot')).toBeNull();
  });

  it('the dot clears on the next poll after the run settles', async () => {
    (window as any).starriver = {
      ...baseMock,
      listJobs: vi.fn()
        .mockResolvedValueOnce([
          { taskName: 'Hourly Weather', status: 'running', startedAt: '', summary: '', sessionId: 's-run' },
        ])
        .mockResolvedValue([]),
    };
    renderSidebar();
    await waitFor(() => {
      expect(rowFor('Run Task')?.querySelector('.sb-run-dot')).toBeTruthy();
    });
    // The 3s poll re-reads an empty registry — the dot must go away without
    // any user action.
    await waitFor(() => {
      expect(rowFor('Run Task')?.querySelector('.sb-run-dot')).toBeNull();
    }, { timeout: 6000 });
  });
});

describe('WI-180 — Tasks rows fall back to the owning task name', () => {
  it('renders the task name for a session whose title is empty', async () => {
    (window as any).starriver = {
      ...baseMock,
      listSessions: () => Promise.resolve([
        { id: 's-legacy', title: '', workspaceId: undefined, createdAt: '2026-10-04T11:00:00Z', messageCount: 3 },
      ]),
      listAllTaskRuns: () => Promise.resolve([
        { taskName: 'Hourly Weather', timestamp: '2026-10-04T11:00:05Z', status: 'success', summary: '', sessionId: 's-legacy' },
      ]),
    };
    renderSidebar();
    await waitFor(() => {
      // The legacy titleless session renders under Tasks with the task's
      // name instead of the generic "Chat".
      expect(rowFor('Hourly Weather')).toBeTruthy();
    });
    expect(rowFor('Hourly')?.textContent).not.toBe('Chat');
  });
});

describe('WI-179 follow-up — a run with no poke still materializes its sidebar row', () => {
  it('a scheduler-fired run refreshes the session list when the running id has no row yet', async () => {
    // Simulate the acceptance failure: the sidebar's session list was
    // fetched BEFORE the run started (no row for the new session), then the
    // scheduler fires the task — jobs.list carries the fresh sessionId but
    // nothing pokes loadSessions. The 3s running-poll must notice the
    // missing row and refresh the list itself.
    const listSessions = vi.fn()
      .mockResolvedValueOnce(SESSIONS) // pre-run snapshot: no 's-fired' row
      .mockResolvedValue([
        ...SESSIONS,
        { id: 's-fired', title: 'Fired Task', workspaceId: 'w1', createdAt: '2026-10-05T09:00:00Z', messageCount: 1 },
      ]);
    const listJobs = vi.fn()
      .mockResolvedValueOnce([]) // first poll: run not yet registered
      .mockResolvedValue([
        { taskName: 'Fired Task', status: 'running', startedAt: '', summary: '', sessionId: 's-fired' },
      ]);
    (window as any).starriver = {
      ...baseMock,
      listSessions,
      listJobs,
    };
    renderSidebar();
    await waitFor(() => {
      expect(rowFor('Idle Task')).toBeTruthy();
    });
    expect(rowFor('Fired Task')).toBeNull();
    // Within the next poll tick the row appears — no create/Run-Now poke.
    await waitFor(() => {
      expect(rowFor('Fired Task')).toBeTruthy();
    }, { timeout: 6000 });
    expect(rowFor('Fired Task')?.querySelector('.sb-run-dot')).toBeTruthy();
  });

  it('the run settling triggers one final list refresh so titles land', async () => {
    const listSessions = vi.fn().mockResolvedValue(SESSIONS);
    (window as any).starriver = {
      ...baseMock,
      listSessions,
      listJobs: vi.fn()
        .mockResolvedValueOnce([
          { taskName: 'Hourly Weather', status: 'running', startedAt: '', summary: '', sessionId: 's-run' },
        ])
        .mockResolvedValue([]),
    };
    renderSidebar();
    await waitFor(() => {
      expect(rowFor('Run Task')?.querySelector('.sb-run-dot')).toBeTruthy();
    });
    const callsAtRun = listSessions.mock.calls.length;
    // The poll sees the registry empty (run settled): the non-empty → empty
    // transition must refresh the list once more (final titles/counts).
    await waitFor(() => {
      expect(listSessions.mock.calls.length).toBeGreaterThan(callsAtRun);
    }, { timeout: 6000 });
  });
});
