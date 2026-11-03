/// <reference types="vitest/globals" />
// Tasks-section session delete: the 对话 (Chats) and 定时运行 (Scheduled runs)
// rows must let the user hard-delete a session — trash button arms a
// confirmation modal, confirm fires session.delete, cancel is a no-op.
// A live run's session is not deletable until the run settles (the button
// disables so an in-flight session isn't deleted under the stream).
import { describe, it, expect, vi, beforeEach } from 'vitest';
import React from 'react';
import { render, screen, waitFor, fireEvent, within } from '@testing-library/react';
import { I18nProvider } from './i18n';

declare global { interface Window { starriver: any; } }

const CHAT = { id: 'chat-1', title: 'Weekly note', createdAt: '2026-11-01', messageCount: 2 };
const RUN = { id: 'run-1', title: 'Daily backup', createdAt: '2026-11-01', messageCount: 5, origin: 'scheduled' };

beforeEach(() => {
  localStorage.setItem('starriver-language', 'en');
  Element.prototype.scrollIntoView = vi.fn();
  window.starriver = {
    listWorkspaces: vi.fn().mockResolvedValue([]),
    listSessions: vi.fn().mockResolvedValue([CHAT, RUN]),
    listArchivedSessions: vi.fn().mockResolvedValue([]),
    listJobs: vi.fn().mockResolvedValue([]),
    listRunningSessions: vi.fn().mockResolvedValue([]),
    deleteSession: vi.fn().mockResolvedValue(undefined),
  };
});

async function mountSidebar() {
  const { WorkspaceProvider, WorkspaceSection } = await import('./app');
  render(
    <I18nProvider>
      <WorkspaceProvider>
        <WorkspaceSection onOpenCoding={() => {}} />
      </WorkspaceProvider>
    </I18nProvider>,
  );
  await waitFor(() => expect(screen.getByText('Weekly note')).toBeTruthy());
}

const rowButton = (row: HTMLElement) =>
  within(row).getByLabelText('Delete') as HTMLButtonElement;
const rowOf = (title: string) =>
  screen.getByText(title).closest('.coding-session-row') as HTMLElement;

describe('Tasks-section session delete', () => {
  it('chat and scheduled-run rows both carry a delete button', async () => {
    await mountSidebar();
    expect(rowButton(rowOf('Weekly note')).disabled).toBe(false);
    expect(rowButton(rowOf('Daily backup')).disabled).toBe(false);
  });

  it('confirm deletes the session and refreshes the sidebar list', async () => {
    await mountSidebar();
    fireEvent.click(rowButton(rowOf('Weekly note')));
    // The modal names the target before the destructive call.
    await waitFor(() => expect(
      screen.getByText('This will permanently delete “Weekly note” and all of its history. This action cannot be undone.'),
    ).toBeTruthy());
    expect(window.starriver.deleteSession).not.toHaveBeenCalled();

    fireEvent.click(screen.getByTestId('confirm-delete-session'));
    await waitFor(() => expect(window.starriver.deleteSession).toHaveBeenCalledWith('chat-1'));
    // Provider reloads the projection so the row disappears.
    await waitFor(() =>
      expect((window.starriver.listSessions as ReturnType<typeof vi.fn>).mock.calls.length).toBeGreaterThan(1),
    );
  });

  it('cancel leaves the session untouched', async () => {
    await mountSidebar();
    fireEvent.click(rowButton(rowOf('Daily backup')));
    await waitFor(() => expect(screen.getByTestId('confirm-delete-session')).toBeTruthy());
    fireEvent.click(screen.getByText('Cancel'));
    await waitFor(() => expect(screen.queryByTestId('confirm-delete-session')).toBeNull());
    expect(window.starriver.deleteSession).not.toHaveBeenCalled();
    expect(screen.getByText('Daily backup')).toBeTruthy();
  });

  it("a live run's session is not deletable until the run settles", async () => {
    window.starriver.listRunningSessions = vi.fn().mockResolvedValue(['run-1']);
    await mountSidebar();
    await waitFor(() => expect(rowButton(rowOf('Daily backup')).disabled).toBe(true));
    expect(rowButton(rowOf('Weekly note')).disabled).toBe(false);
  });
});
