/// <reference types="vitest/globals" />
// Workspace flow edge cases + a full create→activate→task e2e chain.
// Complements __workspace_ui.test.tsx (which covers the happy-path registry
// UI): here the bridge rejects, the active group is deleted, workspace-instructions banners
// render, and IPC routes are asserted statically on preload/main.

import { describe, it, expect, vi, beforeEach } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import React from 'react';
import { render, screen, fireEvent, waitFor, within } from '@testing-library/react';
import { I18nProvider } from './i18n';

declare global { interface Window { starriver: any; } }

function baseMock() {
  window.starriver = {
    listWorkspaces: vi.fn().mockResolvedValue([
      { id: 'w1', path: '/tmp/demo', title: 'demo', createdAt: '', sessionIds: [] },
    ]),
    createWorkspace: vi.fn(),
    renameWorkspace: vi.fn().mockResolvedValue(undefined),
    deleteWorkspace: vi.fn().mockResolvedValue(undefined),
    setActiveWorkspace: vi.fn().mockResolvedValue(undefined),
    // Fresh state: no persisted activation at mount (the tests click to activate).
    getActiveWorkspace: vi.fn().mockResolvedValue(null),
    readWorkspaceInstructions: vi.fn().mockResolvedValue(null),
    readWorkspaceInstructionsMeta: vi.fn().mockResolvedValue({ content: null, truncated: false }),
    selectDirectory: vi.fn(),

    listSessions: vi.fn().mockResolvedValue([]),
    getSessionMessages: vi.fn().mockResolvedValue([]),
    listArchivedSessions: vi.fn().mockResolvedValue([]),

    listSkills: vi.fn().mockResolvedValue([]),
    listMcpServers: vi.fn().mockResolvedValue([]),
    listModels: vi.fn().mockResolvedValue([]),
    listAgentModelBindings: vi.fn().mockResolvedValue([]),
    getCurrentAgent: vi.fn().mockResolvedValue(null),
    getWorktreeStatus: vi.fn().mockResolvedValue({ isRepo: true, branch: 'main', isWorktree: false, dirty: false }),
    setAgentMode: vi.fn().mockResolvedValue(undefined),

    execute: vi.fn().mockImplementation((_task: string, _sid: string | undefined, cb: (err: any, ev: any) => void) => {
      cb(null, { type: 'TaskStarted', session_id: 's-new' });
      cb(null, { type: 'TaskCompleted', session_id: 's-new', summary: 'done' });
    }),
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

describe('CodingView workspace error paths', () => {
  it('shows an error and does not activate when createWorkspace rejects', async () => {
    window.starriver.selectDirectory = vi.fn().mockResolvedValue('/tmp/picked-project');
    window.starriver.createWorkspace = vi.fn().mockRejectedValue(new Error('workspace path must be an existing directory'));
    await renderCodingView();
    await waitFor(() => expect(screen.getByText('demo')).toBeTruthy());

    fireEvent.click(screen.getByRole('button', { name: 'New Workspace' }));
    fireEvent.click(screen.getByText('Browse...'));

    await waitFor(() => expect(screen.getAllByText(/workspace path must be an existing directory/).length).toBeGreaterThanOrEqual(1));
    expect(window.starriver.setActiveWorkspace).not.toHaveBeenCalled();
    // Dialog stays open so the user can pick another directory.
    expect(screen.getByText('Browse...')).toBeTruthy();
  });

  it('shows an error and keeps the old title when renameWorkspace rejects', async () => {
    window.starriver.renameWorkspace = vi.fn().mockRejectedValue(new Error('workspace title must be non-empty'));
    await renderCodingView();
    await waitFor(() => expect(screen.getByText('demo')).toBeTruthy());

    fireEvent.doubleClick(screen.getByText('demo'));
    const input = document.querySelector('input[value="demo"]') as HTMLInputElement;
    fireEvent.change(input, { target: { value: 'renamed demo' } });
    fireEvent.keyDown(input, { key: 'Enter' });

    await waitFor(() => expect(window.starriver.renameWorkspace).toHaveBeenCalledWith('w1', 'renamed demo'));
    await waitFor(() => expect(screen.getAllByText(/Failed to rename workspace/).length).toBeGreaterThanOrEqual(1));
    // Optimistic local update is rolled back — the header still shows the old title.
    await waitFor(() => expect(screen.getByText('demo')).toBeTruthy());
    expect(screen.queryByText('renamed demo')).toBeNull();
  });

  it('clears the conversation view when the ACTIVE workspace is deleted', async () => {
    let sessions = [
      { id: 's1', title: 'grouped task', workspaceId: 'w1', createdAt: '', messageCount: 1 },
    ];
    window.starriver.listSessions = vi.fn().mockImplementation(async () => sessions);
    // The registry drops w1 after the delete. Keyed off the delete call, not
    // a fetch counter — CodingView's restore effect now also lists workspaces
    // at mount, so "first fetch" is no longer the provider's.
    let deleted = false;
    window.starriver.deleteWorkspace = vi.fn().mockImplementation(async () => { deleted = true; });
    window.starriver.listWorkspaces = vi.fn().mockImplementation(async () =>
      deleted ? [] : [{ id: 'w1', path: '/tmp/demo', title: 'demo', createdAt: '', sessionIds: [] }],
    );
    window.starriver.getSessionMessages = vi.fn().mockResolvedValue([
      { role: 'user', content: 'earlier message' },
    ]);
    const { within } = await import('@testing-library/react');
    await renderCodingView();
    await waitFor(() => expect(screen.getByText('demo')).toBeTruthy());

    // Open a session so the view carries state tied to the group.
    fireEvent.click(screen.getByText('demo'));
    fireEvent.click(await screen.findByText('grouped task'));
    await waitFor(() => expect(screen.getByText('earlier message')).toBeTruthy());

    fireEvent.click(screen.getAllByLabelText('Delete workspace')[0]);
    const dialog = screen.getByText(/Delete workspace/).closest('.modal-card') as HTMLElement;
    fireEvent.click(within(dialog).getByRole('button', { name: 'Delete' }));

    await waitFor(() => expect(window.starriver.deleteWorkspace).toHaveBeenCalledWith('w1'));
    await waitFor(() => expect(window.starriver.setActiveWorkspace).toHaveBeenCalledWith(null, 'Starriver Code'));
    // Conversation state resets with the deleted group.
    await waitFor(() => expect(screen.queryByText('earlier message')).toBeNull());
    // The deleted workspace's session stays on disk and remains recoverable:
    // it renders under Ungrouped with a restore-into-active-workspace action
 // (a deliberate deviation — the reference drops it from every group).
    await waitFor(() => expect(screen.getByText('Ungrouped')).toBeTruthy());
    await waitFor(() => expect(screen.getByText('grouped task')).toBeTruthy());
    expect(await screen.findByLabelText('Restore to current workspace')).toBeTruthy();
  });
});

describe('CodingView workspace-instructions banner', () => {
  it('renders workspace instructions after activation', async () => {
    window.starriver.readWorkspaceInstructionsMeta = vi.fn().mockResolvedValue({
      content: '# Project Rules\nUse pnpm.',
      truncated: false,
    });
    await renderCodingView();
    await waitFor(() => expect(screen.getByText('demo')).toBeTruthy());

    fireEvent.click(screen.getByText('demo'));
    await waitFor(() => expect(screen.getAllByText(/Workspace instructions/).length).toBeGreaterThanOrEqual(1));
    expect(await screen.findByText(/Use pnpm\./)).toBeTruthy();
  });

  it('renders no banner for workspaces without instructions', async () => {
    await renderCodingView();
    await waitFor(() => expect(screen.getByText('demo')).toBeTruthy());

    fireEvent.click(screen.getByText('demo'));
    await waitFor(() => expect(window.starriver.readWorkspaceInstructionsMeta).toHaveBeenCalledWith('w1'));
    await new Promise(r => setTimeout(r, 0));
    expect(screen.queryByText(/Project Context/)).toBeNull();
  });

  it('fetches workspace instructions when creating a workspace too (same activation path)', async () => {
    window.starriver.selectDirectory = vi.fn().mockResolvedValue('/tmp/picked');
    window.starriver.createWorkspace = vi.fn().mockResolvedValue({
      id: 'w-new', path: '/tmp/picked', title: 'picked', createdAt: '', sessionIds: [],
    });
    window.starriver.readWorkspaceInstructionsMeta = vi.fn().mockResolvedValue({
      content: null,
      truncated: false,
    });
    await renderCodingView();

    fireEvent.click(screen.getByRole('button', { name: 'New Workspace' }));
    fireEvent.click(screen.getByText('Browse...'));
    await waitFor(() => expect(window.starriver.readWorkspaceInstructionsMeta).toHaveBeenCalledWith('w-new'));
    expect(screen.getAllByText('picked').length).toBeGreaterThanOrEqual(1);
  });
});

describe('CodingView workspace e2e chain', () => {
  it('browse → register → activate → task → session appears under its group', async () => {
    // The session store starts empty; after the task completes the refreshed
    // list contains the new session attached to the freshly created workspace.
    let sessions: any[] = [];
    window.starriver.listSessions = vi.fn().mockImplementation(async () => sessions);
    window.starriver.selectDirectory = vi.fn().mockResolvedValue('/tmp/picked-e2e');
    window.starriver.createWorkspace = vi.fn().mockResolvedValue({
      id: 'w-new', path: '/tmp/picked-e2e', title: 'picked-e2e', createdAt: '', sessionIds: [],
    });

    await renderCodingView();
    await waitFor(() => expect(screen.getByText('demo')).toBeTruthy());

    fireEvent.click(screen.getByRole('button', { name: 'New Workspace' }));
    fireEvent.click(screen.getByText('Browse...'));
    await waitFor(() => expect(screen.getAllByText('picked-e2e').length).toBeGreaterThanOrEqual(1));
    expect(window.starriver.setActiveWorkspace).toHaveBeenCalledWith('w-new', 'Starriver Code');

    // Send a task through the composer Send button.
    const ta = document.querySelector('textarea') as HTMLTextAreaElement;
    fireEvent.change(ta, { target: { value: 'do the thing' } });
    fireEvent.click(screen.getByRole('button', { name: 'Send' }));
    await waitFor(() => expect(window.starriver.execute).toHaveBeenCalledWith(
      // agentPreset (7th) rides along as undefined when nothing staged —
      // arity changed with the WI-5 creation-time binding. permissionPreset
      // (8th, BUG-113) carries the picker's current value (default manual).
      'do the thing', undefined, expect.any(Function), 'standard', 'Starriver Code', false, undefined, 'manual',
    ));

    // TaskCompleted triggers a session refresh that now shows the grouped row.
    sessions = [{ id: 's-new', title: 'do the thing', workspaceId: 'w-new', createdAt: '', messageCount: 1 }];
    await waitFor(() => expect(screen.getAllByText('do the thing').length).toBeGreaterThanOrEqual(1));
  }, 15000);
});

describe('workspace IPC contract (static)', () => {
  it('legacy bridge maps workspace/session APIs onto protocol methods', () => {
    const bridgeSource = readFileSync(resolve(process.cwd(), 'legacyBridge.ts'), 'utf8');
    const preloadSource = readFileSync(resolve(process.cwd(), 'preload.ts'), 'utf8');
    const mainSource = readFileSync(resolve(process.cwd(), 'main.ts'), 'utf8');

    // preload generates typed methods from LEGACY_METHODS (the UI uses protocol envelopes, no direct NAPI).
    expect(preloadSource).toMatch(/import \{[^}]*LEGACY_METHODS[^}]*\} from '\.\/legacyBridge'/);
    expect(preloadSource).toMatch(/Object\.entries\(LEGACY_METHODS\)/);

    // Every workspace/session legacy method maps to a protocol method.
    for (const [legacy, protocol] of [
      ['listWorkspaces', 'workspaces.list'],
      ['createWorkspace', 'workspaces.create'],
      ['renameWorkspace', 'workspaces.rename'],
      ['deleteWorkspace', 'workspaces.delete'],
      ['setActiveWorkspace', 'workspaces.activate'],
      ['listSessions', 'session.list'],
      ['deleteSession', 'session.delete'],
      ['getSessionMessages', 'session.messages'],
    ]) {
      expect(bridgeSource).toMatch(
        new RegExp(`${legacy}: \\{\\s*method: '${protocol}'`),
        `${legacy} must map onto ${protocol}`,
      );
    }

    // main only keeps the generic runtime:invoke dispatch; typed routes were deleted with daemonCompat.
    expect(mainSource).toMatch(/'runtime:invoke'/);
    for (const route of ['runtime:listWorkspaces', 'runtime:createWorkspace', 'runtime:listSessions']) {
      expect(mainSource).not.toMatch(new RegExp(`'${route}'`));
    }
  });
});

// WI-152 guard (2026-10-05): with a session open, clicking another workspace
// group is navigation (fold/unfold) — it must not steal the active-workspace
// binding. That binding drives the bottom status bar AND the open session's
// next-turn cwd (the host resolves working_dir from the coding agent's active
// workspace), so the old click-to-activate flipped the status bar to nexplan
// while a starriver session was open and would have mis-filed the next turn.
describe('workspace group click with an open session', () => {
  function twoWorkspaceMock() {
    window.starriver = {
      ...window.starriver,
      listWorkspaces: vi.fn().mockResolvedValue([
        { id: 'w1', path: '/tmp/starriver', title: 'starriver', createdAt: '', sessionIds: [] },
        { id: 'w2', path: '/tmp/nexplan', title: 'nexplan', createdAt: '', sessionIds: [] },
      ]),
      // The durable binding starts on w1 — the session-switch path activated it.
      getActiveWorkspace: vi.fn().mockResolvedValue('w1'),
      listSessions: vi.fn().mockResolvedValue([
        { id: 's1', title: 'fix bug', workspaceId: 'w1', createdAt: '', messageCount: 2 },
      ]),
      setActiveWorkspace: vi.fn().mockResolvedValue(undefined),
    };
  }

  it('clicking another group does not re-activate — binding stays on the open session', async () => {
    twoWorkspaceMock();
    const { WorkspaceProvider, WorkspaceSection, useWorkspaces } = await import('./app');
    function Probe() {
      const ws = useWorkspaces();
      return (
        <div>
          <button data-testid="open-s1" onClick={() => ws.setActiveSessionId('s1')} />
          {/* The provider starts unbound; CodingView's mount restores the binding.
              The session-switch path re-activates the session's workspace, so a
              bound open session means both of these are set — mirror that. */}
          <button data-testid="open-s1-bound" onClick={() => {
            ws.setActiveWorkspaceId('w1');
            ws.setActiveSessionId('s1');
          }} />
          <button data-testid="read-state" onClick={() => {
            (window as any).__probeState = {
              activeWorkspaceId: ws.activeWorkspaceId,
              activeSessionId: ws.activeSessionId,
            };
          }} />
        </div>
      );
    }
    render(
      <I18nProvider>
        <WorkspaceProvider>
          <WorkspaceSection onOpenCoding={() => {}} />
          <Probe />
        </WorkspaceProvider>
      </I18nProvider>,
    );
    await waitFor(() => expect(screen.getByText('nexplan')).toBeTruthy());

    // Open session s1 bound to w1 (post-session-switch state), then read state.
    fireEvent.click(screen.getByTestId('open-s1-bound'));
    fireEvent.click(screen.getByTestId('read-state'));
    expect((window as any).__probeState).toMatchObject({ activeSessionId: 's1', activeWorkspaceId: 'w1' });

    // Click the OTHER workspace group: navigation only — no activation write.
    fireEvent.click(screen.getByText('nexplan'));
    await new Promise(r => setTimeout(r, 20));
    const activated = window.starriver.setActiveWorkspace.mock.calls.map((c: any[]) => c[0]);
    expect(activated).not.toContain('w2');

    fireEvent.click(screen.getByTestId('read-state'));
    expect((window as any).__probeState).toMatchObject({ activeWorkspaceId: 'w1', activeSessionId: 's1' });
  });

  it('no open session: group click still activates (New Task default pick)', async () => {
    twoWorkspaceMock();
    const { WorkspaceProvider, WorkspaceSection } = await import('./app');
    render(
      <I18nProvider>
        <WorkspaceProvider>
          <WorkspaceSection onOpenCoding={() => {}} />
        </WorkspaceProvider>
      </I18nProvider>,
    );
    await waitFor(() => expect(screen.getByText('nexplan')).toBeTruthy());

    fireEvent.click(screen.getByText('nexplan'));
    await waitFor(() =>
      expect(window.starriver.setActiveWorkspace).toHaveBeenCalledWith('w2', 'Starriver Code'),
    );
  });
});
