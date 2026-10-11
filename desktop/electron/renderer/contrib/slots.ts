// Slot contribution registry: built-in modules and third-party plugins register render
// contributions into named slots under one contract; the shell renders only registry
// content (the slot/registry model, rewritten per the backlog supplement-6 decisions
//
// Chain-of-responsibility semantics: within a slot, contributions try in ascending
// priority and the first non-null result wins — matching the host's tools/code-dispatch waterfall.

import type { ReactNode } from 'react';

/** Built-in slot axes (conversation flow). New axes append here; the key is the addressing id. */
export const SlotKeys = {
  /** Tool-row detail area (run_code sub-call list, todo card, …). */
  conversationToolDetail: 'conversation.tool-detail',
  /** Tool-row summary area (the collapsed one-line preview). */
  conversationToolSummary: 'conversation.tool-summary',
} as const;

export type SlotKey = (typeof SlotKeys)[keyof typeof SlotKeys];

/** Tool-row context: one tool call's raw payload. */
export interface ToolNodeContext {
  toolName: string;
  input?: string | null;
  output?: string | null;
  ok?: boolean | null;
  /** Structured presentation metadata self-reported by the tool: opaque JSON persisted with the
   *  tool/result; contributions narrow it on demand, degrading to generic rendering on failure
   *  instead of throwing. */
  meta?: unknown;
}

export interface ToolNodeRenderResult {
  /** Main content (detail area or summary one-liner). */
  node: ReactNode;
  /** Terminate the chain on hit (default true); returning false hands off to later contributions. */
  handled?: boolean;
}

export type ToolNodeRenderer = (ctx: ToolNodeContext) => ToolNodeRenderResult | null;

export interface SlotContribution {
  /** Registrant (built-in id or plugin name); plugin unload unregisters by owner in bulk. */
  owner: string;
  /** Dedup id within the slot (owner + id is unique). */
  id: string;
  /** Smaller tries first. */
  priority?: number;
  render: ToolNodeRenderer;
}

interface SlotEntry {
  contribution: SlotContribution;
  handle: { disposed: boolean };
}

const slots = new Map<string, SlotEntry[]>();

/** Register one slot contribution. Returns a handle: `dispose()` unregisters (idempotent). */
export function contribute(slot: SlotKey | string, contribution: SlotContribution): { dispose: () => void } {
  const handle = { disposed: false };
  const entry: SlotEntry = { contribution, handle };
  const list = slots.get(slot) ?? [];
  // Contract: owner+id is unique within a slot — re-registration counts as an update, replacing the entry.
  for (let i = list.length - 1; i >= 0; i--) {
    const existing = list[i].contribution;
    if (existing.owner === contribution.owner && existing.id === contribution.id) {
      list.splice(i, 1);
    }
  }
  list.push(entry);
  list.sort((a, b) =>
    (a.contribution.priority ?? 100) - (b.contribution.priority ?? 100));
  slots.set(slot, list);
  return {
    dispose: () => {
      if (handle.disposed) return;
      handle.disposed = true;
      const list = slots.get(slot);
      if (!list) return;
      const index = list.findIndex((e) => e.handle === handle);
      if (index >= 0) list.splice(index, 1);
      if (list.length === 0) slots.delete(slot);
    },
  };
}

/** Unregister all of one owner's contributions in this slot (the plugin unload path). */
export function removeContributions(slot: SlotKey | string, owner: string): void {
  const list = slots.get(slot);
  if (!list) return;
  slots.set(slot, list.filter((e) => e.contribution.owner !== owner));
}

/** The slot's current contribution chain (priority-sorted; for debugging/test introspection only). */
export function contributors(slot: SlotKey | string): readonly SlotContribution[] {
  return (slots.get(slot) ?? []).map((e) => e.contribution);
}

/** Render along the chain: the first non-null result wins; null when all pass. */
export function renderSlot(
  slot: SlotKey | string,
  ctx: ToolNodeContext,
): ToolNodeRenderResult | null {
  for (const entry of slots.get(slot) ?? []) {
    if (entry.handle.disposed) continue;
    const result = entry.contribution.render(ctx);
    if (result === null || result === undefined) continue;
    if (result.handled === false) continue;
    return { node: result.node, handled: true };
  }
  return null;
}
