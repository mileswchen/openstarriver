/// <reference types="vitest/globals" />
// Workspace pin-to-top (WI): the sidebar toggle writes through the bridge
// (workspaces.pin), the HOST owns the order (pinned first, newest pin on
// top, unpin falls back to created_at), and the pinned badge beside the
// name is the always-visible state marker.
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import React from 'react';
import { render, screen, fireEvent, waitFor, act } from '@testing-library/react';
import { I18nProvider } from './i18n';

declare global { interface Window { starriver: any; } }

function baseMock(seed: { id: string; title: string; pinnedAt?: number | null }[] = []) {
  const workspaces = seed.map((w, i) => ({
    id: w.id, path: `/tmp/${w.id}`, title: w.title, createdAt: String(i + 1),
    pinnedAt: w.pinnedAt ?? null, sessionIds: [] as string[],
  }));
  // Deterministic pin clock — Date.now() ties would make the newest-pin
  // ordering ambiguous inside one test tick.
  let clock = 1000;
  const sorted = () => [...workspaces].sort((a, b) => {
    const ap = a.pinnedAt == null ? 1 : 0;
    const bp = b.pinnedAt == null ? 1 : 0;
    if (ap !== bp) return ap - bp;
    if (ap === 0) return (b.pinnedAt as number) - (a.pinnedAt as number);
    return a.createdAt.localeCompare(b.createdAt);
  });
  window.starriver = {
    listWorkspaces: vi.fn().mockImplementation(async () => sorted().map(w => ({ ...w }))),
    pinWorkspace: vi.fn().mockImplementation(async (id: string, pinned: boolean) => {
      const w = workspaces.find(x => x.id === id);
      if (w) w.pinnedAt = pinned ? ++clock : null;
    }),
    listSessions: vi.fn().mockResolvedValue([]),
    listArchivedSessions: vi.fn().mockResolvedValue([]),
    listAllTaskRuns: vi.fn().mockResolvedValue([]),
    listJobs: vi.fn().mockResolvedValue([]),
    listRunningSessions: vi.fn().mockResolvedValue([]),
    getActiveWorkspace: vi.fn().mockResolvedValue(null),
    setActiveWorkspace: vi.fn().mockResolvedValue(undefined),
  };
  return workspaces;
}

beforeEach(() => {
  Element.prototype.scrollIntoView = vi.fn();
  localStorage.setItem('starriver-language', 'en');
});

async function renderSection() {
  const { WorkspaceProvider, WorkspaceSection } = await import('./app');
  return render(
    <I18nProvider>
      <WorkspaceProvider>
        <WorkspaceSection onOpenCoding={() => {}} />
      </WorkspaceProvider>
    </I18nProvider>,
  );
}

const rowTitles = (container: HTMLElement) =>
  [...container.querySelectorAll('.sb-ws-group .sb-ws-name')].map(e => e.textContent);

describe('WorkspaceSection — pin to top', () => {
  it('pin writes through the bridge and hoists the row to the top', async () => {
    baseMock([
      { id: 'w1', title: 'alpha' },
      { id: 'w2', title: 'beta' },
      { id: 'w3', title: 'gamma' },
    ]);
    const { container } = await renderSection();
    await waitFor(() => expect(rowTitles(container)).toEqual(['alpha', 'beta', 'gamma']));

    fireEvent.click(screen.getAllByLabelText('Pin to top')[1]); // beta
    await waitFor(() => expect(window.starriver.pinWorkspace).toHaveBeenCalledWith('w2', true));
    await waitFor(() => expect(rowTitles(container)).toEqual(['beta', 'alpha', 'gamma']));
    // The pinned badge is the always-visible marker (no hover needed).
    expect(container.querySelectorAll('.sb-ws-pin-badge').length).toBe(1);
  });

  it('a newer pin sorts above an older pin', async () => {
    baseMock([
      { id: 'w1', title: 'alpha', pinnedAt: 100 },
      { id: 'w2', title: 'beta' },
      { id: 'w3', title: 'gamma' },
    ]);
    const { container } = await renderSection();
    await waitFor(() => expect(rowTitles(container)).toEqual(['alpha', 'beta', 'gamma']));

    // alpha is already pinned (its button reads "Unpin"), so the "Pin to
    // top" buttons are beta[0] and gamma[1].
    fireEvent.click(screen.getAllByLabelText('Pin to top')[1]); // gamma
    await waitFor(() => expect(window.starriver.pinWorkspace).toHaveBeenCalledWith('w3', true));
    await waitFor(() => expect(rowTitles(container)).toEqual(['gamma', 'alpha', 'beta']));
  });

  it('unpin clears the badge and restores the created_at order', async () => {
    baseMock([
      { id: 'w1', title: 'alpha' },
      { id: 'w2', title: 'beta', pinnedAt: 500 },
      { id: 'w3', title: 'gamma' },
    ]);
    const { container } = await renderSection();
    await waitFor(() => expect(rowTitles(container)).toEqual(['beta', 'alpha', 'gamma']));
    expect(container.querySelectorAll('.sb-ws-pin-badge').length).toBe(1);

    fireEvent.click(screen.getByLabelText('Unpin'));
    await waitFor(() => expect(window.starriver.pinWorkspace).toHaveBeenCalledWith('w2', false));
    await waitFor(() => expect(rowTitles(container)).toEqual(['alpha', 'beta', 'gamma']));
    expect(container.querySelectorAll('.sb-ws-pin-badge').length).toBe(0);
  });

  // Layout regression guard: badge + pin button showing together stole
  // ~40px from the name and truncated long titles ("openst…"). The CSS
  // must swap them — badge hidden while the row is hovered/focused, pin
  // button hidden otherwise — so the row's width budget stays constant.
  // jsdom does no layout, so assert the rules themselves.
  it('CSS swaps badge and pin action to keep the name width budget', () => {
    const css = readFileSync(resolve(__dirname, 'styles.css'), 'utf8');
    expect(css).toMatch(/\.sb-ws-group:hover \.sb-ws-pin-badge,\s*\.sb-ws-group:focus-within \.sb-ws-pin-badge \{ display: none; \}/);
    expect(css).toMatch(/\.sb-ws-pin \{ display: none; \}/);
    expect(css).toMatch(/\.sb-ws-group:hover \.sb-ws-pin,\s*\.sb-ws-group:focus-within \.sb-ws-pin \{ display: inline-flex; \}/);
    // The active (blue pill) row must render the badge in white, not accent.
    expect(css).toMatch(/\.sb-ws-group\.is-active \.sb-ws-pin-badge/);
  });
});

describe('WorkspaceSection — pin hardening (BUG-P14…P19)', () => {
  it('BUG-P14: an older refetch resolving late must not clobber a newer snapshot', async () => {
    const server = baseMock([
      { id: 'w1', title: 'alpha' },
      { id: 'w2', title: 'beta' },
    ]);
    // Take manual control of listWorkspaces: each call captures the server
    // snapshot at query time and parks until the test resolves it, so we can
    // settle two overlapping refetches OUT OF ORDER (the real race: pin A's
    // refetch is slow, pin B's is fast).
    const sortServer = () => [...server].sort((a, b) => {
      const ap = a.pinnedAt == null ? 1 : 0;
      const bp = b.pinnedAt == null ? 1 : 0;
      if (ap !== bp) return ap - bp;
      if (ap === 0) return (b.pinnedAt as number) - (a.pinnedAt as number);
      return a.createdAt.localeCompare(b.createdAt);
    }).map(w => ({ ...w }));
    const parked: Array<{ resolve: (v: any[]) => void; snap: any[] }> = [];
    window.starriver.listWorkspaces = vi.fn().mockImplementation(
      () => new Promise(resolve => { parked.push({ resolve, snap: sortServer() }); }),
    );
    const { container } = await renderSection();
    await act(async () => { parked[0].resolve(parked[0].snap); });
    await waitFor(() => expect(rowTitles(container)).toEqual(['alpha', 'beta']));

    fireEvent.click(screen.getAllByLabelText('Pin to top')[0]); // alpha
    await waitFor(() => expect(window.starriver.listWorkspaces).toHaveBeenCalledTimes(2));
    fireEvent.click(screen.getAllByLabelText('Pin to top')[0]); // beta (alpha now reads "Unpin")
    await waitFor(() => expect(window.starriver.listWorkspaces).toHaveBeenCalledTimes(3));

    // Newer snapshot (both pinned, beta newest) lands first…
    await act(async () => { parked[2].resolve(parked[2].snap); });
    await waitFor(() => expect(rowTitles(container)).toEqual(['beta', 'alpha']));
    // …then the STALE one (only alpha pinned) resolves late. It must be
    // dropped, not applied — otherwise beta's pin silently disappears.
    await act(async () => { parked[1].resolve(parked[1].snap); });
    expect(rowTitles(container)).toEqual(['beta', 'alpha']);
    expect(container.querySelectorAll('.sb-ws-pin-badge').length).toBe(2);
  });

  it('BUG-P15: the pin button exposes its toggle state via aria-pressed', async () => {
    baseMock([
      { id: 'w1', title: 'alpha' },
      { id: 'w2', title: 'beta', pinnedAt: 500 },
    ]);
    await renderSection();
    await waitFor(() => expect(screen.getByLabelText('Unpin')).toBeTruthy());
    expect(screen.getByLabelText('Unpin').getAttribute('aria-pressed')).toBe('true');
    expect(screen.getByLabelText('Pin to top').getAttribute('aria-pressed')).toBe('false');
  });

  it('BUG-P17: pinning mid-rename must not steal focus and discard the typed title', async () => {
    baseMock([{ id: 'w1', title: 'alpha' }]);
    const { container } = await renderSection();
    await waitFor(() => expect(rowTitles(container)).toEqual(['alpha']));
    fireEvent.doubleClick(screen.getByText('alpha'));
    const input = await screen.findByDisplayValue('alpha');
    fireEvent.change(input, { target: { value: 'renamed' } });
    const pin = screen.getByLabelText('Pin to top');
    // In a real browser, mousedown on the button moves focus to it, firing
    // the rename input's onBlur (which cancels the edit). jsdom does not
    // emulate that default action, so assert the protective contract: the
    // button must suppress the mousedown default (preventDefault).
    const notCancelled = fireEvent.mouseDown(pin);
    expect(notCancelled).toBe(false);
    fireEvent.click(pin);
    expect(screen.getByDisplayValue('renamed')).toBeTruthy();
    await waitFor(() => expect(window.starriver.pinWorkspace).toHaveBeenCalledWith('w1', true));
  });

  it('BUG-P18: a pin failure surfaces a localized banner, not hardcoded English', async () => {
    localStorage.setItem('starriver-language', 'zh');
    baseMock([{ id: 'w1', title: 'alpha' }]);
    (window.starriver.pinWorkspace as any).mockRejectedValueOnce(new Error('backend down'));
    const { container } = await renderSection();
    await waitFor(() => expect(rowTitles(container)).toEqual(['alpha']));
    fireEvent.click(screen.getByLabelText('置顶'));
    await waitFor(() => {
      expect(container.querySelector('.sb-ws-error')?.textContent).toBe('置顶失败');
    });
  });

  it('BUG-P19: the pinned state is exposed to assistive tech at rest', async () => {
    baseMock([
      { id: 'w1', title: 'alpha', pinnedAt: 500 },
      { id: 'w2', title: 'beta' },
    ]);
    const { container } = await renderSection();
    await waitFor(() => expect(rowTitles(container)).toEqual(['alpha', 'beta']));
    // At rest the pin BUTTON is display:none and the badge was aria-hidden,
    // so a screen reader had no way to know the row was pinned. The badge
    // must carry an accessible name.
    const badge = container.querySelector('.sb-ws-pin-badge')!;
    expect(badge.getAttribute('aria-hidden')).not.toBe('true');
    expect(screen.getByLabelText('Pinned')).toBe(badge);
  });

  it('BUG-P23: rapid successive toggles serialize so the newest intent wins server-side', async () => {
    baseMock([{ id: 'w1', title: 'alpha' }, { id: 'w2', title: 'beta' }]);
    // Gate the FIRST pin write so it lands AFTER the second — reproducing an
    // older in-flight request overtaking the newest intent (daemon/LAN reorder).
    let releaseFirst!: () => void;
    const firstGate = new Promise<void>((r) => { releaseFirst = r; });
    const server = { pinnedAt: null as number | null };
    let call = 0;
    window.starriver.pinWorkspace = vi.fn().mockImplementation(async (_id: string, pinned: boolean) => {
      call += 1;
      if (call === 1) await firstGate;
      server.pinnedAt = pinned ? 1000 : null;
    });
    window.starriver.listWorkspaces = vi.fn().mockImplementation(async () => [
      { id: 'w1', path: '/tmp/w1', title: 'alpha', createdAt: '1', pinnedAt: server.pinnedAt, sessionIds: [] },
      { id: 'w2', path: '/tmp/w2', title: 'beta', createdAt: '2', pinnedAt: null, sessionIds: [] },
    ]);
    const { container } = await renderSection();
    await waitFor(() => expect(rowTitles(container)).toEqual(['alpha', 'beta']));

    fireEvent.click(screen.getAllByLabelText('Pin to top')[0]);   // intent 1: pin w1 (gated)
    await waitFor(() => expect(screen.getByLabelText('Unpin')).toBeTruthy());
    fireEvent.click(screen.getByLabelText('Unpin'));        // intent 2: unpin (newest)

    await act(async () => { releaseFirst(); });
    // The authoritative server state must reflect the NEWEST intent (unpin),
    // not whichever in-flight request happened to land last.
    await waitFor(async () => {
      const fresh = await window.starriver.listWorkspaces();
      expect(fresh.find((w: any) => w.id === 'w1').pinnedAt).toBeNull();
    }, { timeout: 2500 });
  });
});