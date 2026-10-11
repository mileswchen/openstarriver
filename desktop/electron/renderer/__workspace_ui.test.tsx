/// <reference types="vitest/globals" />
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import React from 'react';
import { render, screen, fireEvent, waitFor } from '@testing-library/react';
import { I18nProvider } from './i18n';

declare global { interface Window { starriver: any; } }

function baseMock() {
  // Mutable so the delete-workspace test can model the host dropping the row.
  const workspaces = [
    { id: 'w1', path: '/tmp/demo', title: 'demo', createdAt: '', sessionIds: ['s1'] },
    { id: 'w2', path: '/tmp/empty', title: 'empty', createdAt: '', sessionIds: [] },
  ];
  window.starriver = {
    listWorkspaces: vi.fn().mockImplementation(async () => workspaces.map(w => ({ ...w }))),
    createWorkspace: vi.fn().mockImplementation(async (path: string) => ({
      id: 'w-new',
      path,
      title: path.split('/').at(-1),
      createdAt: '',
      sessionIds: [],
    })),
    renameWorkspace: vi.fn().mockResolvedValue(undefined),
    deleteWorkspace: vi.fn().mockImplementation(async (id: string) => {
      const i = workspaces.findIndex(w => w.id === id);
      if (i >= 0) workspaces.splice(i, 1);
    }),
    setActiveWorkspace: vi.fn().mockResolvedValue(undefined),
    getActiveWorkspace: vi.fn().mockResolvedValue('w1'),
    selectDirectory: vi.fn(),

    listSessions: vi.fn().mockResolvedValue([
      { id: 's1', title: 'grouped task', workspaceId: 'w1', createdAt: '', messageCount: 1 },
      // Orphaned Coding session: workspace deleted, stale marker kept by the host.
      { id: 's-orphan', title: 'orphaned task', workspaceId: 'w-gone', createdAt: '', messageCount: 2 },
      // No workspaceId at all → Chat conversation; must never render in Coding.
      { id: 's-chat', title: 'chat talk', createdAt: '', messageCount: 3 },
    ]),
    getSessionMessages: vi.fn().mockResolvedValue([]),
    listArchivedSessions: vi.fn().mockResolvedValue([]),

    // Legacy seams must no longer be reached by the CodingView data flow.
    listProjects: vi.fn(async () => { throw new Error('projects retired'); }),
    listKeptWorktrees: vi.fn().mockResolvedValue([
      { sessionId: 'kept', worktreePath: '/tmp/kept', branch: 'kept/branch', status: 'kept', createdAt: 1 },
    ]),
    removeWorktree: vi.fn(),

    listSkills: vi.fn().mockResolvedValue([]),
    listMcpServers: vi.fn().mockResolvedValue([]),
    listModels: vi.fn().mockResolvedValue([]),
    listAgentModelBindings: vi.fn().mockResolvedValue([]),
    switchAgent: vi.fn().mockResolvedValue(undefined),
    getCurrentAgent: vi.fn().mockResolvedValue(null),
    getWorktreeStatus: vi.fn().mockResolvedValue({ isRepo: true, branch: 'main', isWorktree: false, dirty: false }),
    setSessionIsolation: vi.fn().mockResolvedValue(undefined),
    setAgentMode: vi.fn().mockResolvedValue(undefined),
  };
}

beforeEach(() => {
  Element.prototype.scrollIntoView = vi.fn();
  localStorage.setItem('starriver-language', 'en');
  baseMock();
});

async function renderCodingView() {
  const { CodingView, WorkspaceProvider, WorkspaceSection } = await import('./app');
  return render(<I18nProvider><WorkspaceProvider><WorkspaceSection onOpenCoding={() => {}} /><CodingView debugEnabled={false} /></WorkspaceProvider></I18nProvider>);
}

/** The workspace title now renders twice (toolbar chip + sidebar header);
 *  interactions with the sidebar header must pick that instance. */
function sidebarWorkspaceTitle(name: string): HTMLElement {
  const el = screen.getAllByText(name).find(e => e.closest('.coding-group-header'));
  expect(el, `sidebar workspace header "${name}"`).toBeTruthy();
  return el!;
}

describe('CodingView workspace model', () => {
  it('loads the workspace registry and groups sessions without legacy projects', async () => {
    await renderCodingView();

    await waitFor(() => expect(window.starriver.listWorkspaces).toHaveBeenCalled());
    // "demo" appears in the toolbar chip and the sidebar header.
    expect(screen.getAllByText('demo').length).toBeGreaterThanOrEqual(1);
    expect(screen.getByText('empty')).toBeTruthy();
    expect(screen.getByText('grouped task')).toBeTruthy();
    // Sessions whose workspace is gone (stale workspaceId) stay on disk and
    // render under Ungrouped with a restore action — deliberate deviation
 // unlike the reference, which drops them from every group.
    expect(screen.getByText('orphaned task')).toBeTruthy();
    // Chat conversations (no workspaceId) render under the Tasks section's
    // Chats folder — the Chat tab is retired.
    expect(screen.getByText('chat talk')).toBeTruthy();
    expect(screen.getByText('Chats')).toBeTruthy();
    expect(screen.getByText('Tasks')).toBeTruthy();
    expect(screen.queryByText(/kept\/branch/)).toBeNull();
    expect(() => window.starriver.listProjects()).rejects.toThrow();
  });

  it('selects an active workspace before starting a conversation', async () => {
    // Nothing persisted → the click is a fresh activation, not a collapse toggle.
    window.starriver.getActiveWorkspace = vi.fn().mockResolvedValue(null);
    await renderCodingView();
    await waitFor(() => expect(sidebarWorkspaceTitle('demo')).toBeTruthy());

    fireEvent.click(sidebarWorkspaceTitle('demo'));
    expect(window.starriver.setActiveWorkspace).toHaveBeenCalledWith('w1', 'Starriver Code');
  });

  it('restores the last-activated workspace after restart (active in the workspace picker)', async () => {
    await renderCodingView();

    // Restart flow: the host reports the persisted activation — the workspace
    // picker comes back with "demo" active. (The bottom status bar is now a
    // Session-screen affordance and no longer mirrors it on New Task.)
    await waitFor(() => {
      const picker = document.querySelector('button[aria-label="workspace"]');
      expect(picker?.textContent).toContain('demo');
    });
  });

  it('creates a workspace from an existing directory using its basename as the title', async () => {
    window.starriver.selectDirectory = vi.fn().mockResolvedValue('/tmp/picked-project');
    await renderCodingView();

    // "+ New" renders as two text nodes ("+ " + "New"); query by role/name instead.
    fireEvent.click(screen.getByRole('button', { name: 'New Workspace' }));
    fireEvent.click(screen.getByText('Browse...'));
    await waitFor(() => expect(window.starriver.createWorkspace).toHaveBeenCalledWith('/tmp/picked-project'));
    // Shows up in the sidebar and in the active-workspace context chip.
    await waitFor(() => expect(screen.getAllByText('picked-project').length).toBeGreaterThanOrEqual(1));
    expect(window.starriver.setActiveWorkspace).toHaveBeenCalledWith('w-new', 'Starriver Code');
  });

  it('renames a workspace through the registry API', async () => {
    await renderCodingView();
    await waitFor(() => expect(sidebarWorkspaceTitle('demo')).toBeTruthy());

    fireEvent.doubleClick(sidebarWorkspaceTitle('demo'));
    const input = document.querySelector('input[value="demo"]') as HTMLInputElement;
    fireEvent.change(input, { target: { value: 'renamed demo' } });
    fireEvent.keyDown(input, { key: 'Enter' });

    await waitFor(() => expect(window.starriver.renameWorkspace).toHaveBeenCalledWith('w1', 'renamed demo'));
    await waitFor(() => expect(screen.getAllByText('renamed demo').length).toBeGreaterThanOrEqual(1));
  });

  it('deletes a workspace keeping its sessions recoverable under Ungrouped', async () => {
    const { within } = await import('@testing-library/react');
    await renderCodingView();
    await waitFor(() => expect(screen.getAllByText('demo').length).toBeGreaterThanOrEqual(1));

    fireEvent.click(screen.getAllByLabelText('Delete workspace')[0]);
    const dialog = screen.getByText(/Delete workspace/).closest('.modal-card') as HTMLElement;
    fireEvent.click(within(dialog).getByRole('button', { name: 'Delete' }));

    await waitFor(() => expect(window.starriver.deleteWorkspace).toHaveBeenCalledWith('w1'));
    // The registry refetch no longer lists w1; its session falls to the
 // Ungrouped bucket with a restore action (matching delete.desc:
    // "Its sessions will appear under Ungrouped").
    await waitFor(() => expect(screen.getByText('Ungrouped')).toBeTruthy());
    await waitFor(() => expect(screen.getByText('grouped task')).toBeTruthy());
    expect(screen.getAllByLabelText('Restore to current workspace').length).toBeGreaterThanOrEqual(1);
    await waitFor(() => expect(window.starriver.listWorkspaces).toHaveBeenCalledTimes(3));
  });

  it('restore on an orphan re-homes into the active workspace when one exists', async () => {
    window.starriver.assignSessionWorkspace = vi.fn().mockResolvedValue(undefined);
    window.starriver.unarchiveSession = vi.fn().mockResolvedValue(undefined);
    await renderCodingView();
    await waitFor(() => expect(screen.getByText('orphaned task')).toBeTruthy());

    const row = screen.getByText('orphaned task').closest('.coding-session-row') as HTMLElement;
    fireEvent.click(row.querySelector('button[aria-label="Restore to current workspace"]') as HTMLElement);
    await waitFor(() =>
      expect(window.starriver.assignSessionWorkspace).toHaveBeenCalledWith('s-orphan', 'w1')
    );
    // A valid target wins: attach (which unarchives host-side), no separate call.
    expect(window.starriver.unarchiveSession).not.toHaveBeenCalled();
  });

  it('with no workspaces, restore degrades to unarchive-only instead of a silent no-op', async () => {
    window.starriver.listWorkspaces = vi.fn().mockResolvedValue([]);
    window.starriver.getActiveWorkspace = vi.fn().mockResolvedValue(null);
    window.starriver.assignSessionWorkspace = vi.fn().mockResolvedValue(undefined);
    window.starriver.unarchiveSession = vi.fn().mockResolvedValue(undefined);
    await renderCodingView();
    await waitFor(() => expect(screen.getByText('orphaned task')).toBeTruthy());

    const row = screen.getByText('orphaned task').closest('.coding-session-row') as HTMLElement;
    fireEvent.click(row.querySelector('button[aria-label="Restore to current workspace"]') as HTMLElement);
    await waitFor(() =>
      expect(window.starriver.unarchiveSession).toHaveBeenCalledWith('s-orphan')
    );
    expect(window.starriver.assignSessionWorkspace).not.toHaveBeenCalled();
  });

  it('renders a fully-archived orphan group (archived sessions establish the bucket too)', async () => {
    window.starriver.listSessions = vi.fn().mockResolvedValue([
      { id: 's1', title: 'grouped task', workspaceId: 'w1', createdAt: '', messageCount: 1 },
    ]);
    window.starriver.listArchivedSessions = vi.fn().mockResolvedValue([
      { id: 's-arch', title: 'archived orphan', workspaceId: 'w-gone', createdAt: '', messageCount: 1 },
    ]);
    await renderCodingView();
    // Every member archived → the group still renders (previously it vanished).
    await waitFor(() => expect(screen.getByText(/archived orphan/)).toBeTruthy());
  });

  it('keeps the Workspaces label static and folds via the workspace row (no carets)', async () => {
    await renderCodingView();
    await waitFor(() => expect(sidebarWorkspaceTitle('demo')).toBeTruthy());
    // The section label is static: always-expanded body, no folder glyph on the
    // header row, and never ▼/▶ carets anywhere in the sidebar.
    expect(document.querySelector('.sb-ws-body')).toBeTruthy();
    expect(document.querySelector('.sb-ws .sb-ws-header .sb-ws-folder')).toBeNull();
    expect(document.querySelector('.sb-ws')!.textContent).not.toMatch(/[▼▶]/);

    // First click on the workspace header activates it; second click folds its
    // session list (folder glyph flips open/closed). Assertions are scoped to
    // the demo group — the Ungrouped section keeps its own session rows.
    const demoGroup = sidebarWorkspaceTitle('demo').closest('.sb-ws-body > div')!;
    // The host's active-workspace restore (getActiveWorkspace) activates demo
    // shortly after mount; wait for it, then click = fold, click = unfold.
    await waitFor(() => expect(demoGroup.querySelector('.sb-ws-group.is-active')).toBeTruthy());
    fireEvent.click(sidebarWorkspaceTitle('demo'));
    await waitFor(() => expect(demoGroup.querySelector('.sb-ws-session')).toBeNull());
    fireEvent.click(sidebarWorkspaceTitle('demo'));
    await waitFor(() => expect(demoGroup.querySelector('.sb-ws-session')).toBeTruthy());
  });

  it('removes SessionInfo presentation metadata and kept-worktree bridge calls', () => {
    // vitest transforms import.meta.url in jsdom, so resolve from the package cwd.
    const appSource = readFileSync(resolve(process.cwd(), 'renderer/app.tsx'), 'utf8');
    const preloadSource = readFileSync(resolve(process.cwd(), 'preload.ts'), 'utf8');
    const mainSource = readFileSync(resolve(process.cwd(), 'main.ts'), 'utf8');

    // SessionInfo no longer carries model/agent presentation metadata
    // (AgentModelBinding.agentName is a different, legitimate field).
    expect(appSource).not.toMatch(/interface SessionInfo[^}]*agentName/);
    expect(appSource).not.toMatch(/interface SessionInfo[^}]*\bmodel:/);
    // Worktree runtime status stays; the kept-worktree/project admin surface is gone.
    expect(appSource).not.toMatch(/listKeptWorktrees|removeWorktree|readProjectFlycode|setActiveProject\b/);
    expect(preloadSource).not.toMatch(/listKeptWorktrees|removeWorktree|listProjects|createProject|setActiveProject/);
    expect(mainSource).not.toMatch(/runtime:listKeptWorktrees|runtime:removeWorktree|runtime:listProjects|runtime:createProject|runtime:setActiveProject/);
  });
});
