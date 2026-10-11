// Conversation node registry: tool-node detail rendering goes through a contribution
// registry instead of a hardcoded switch. Built-in contributions (run_code program
// list / todo card) and plugin contributions (declarative templates) share one
// registration path; rewritten per the backlog supplement-6 decisions on the
//
// Render chain: exact toolName matches beat wildcards; ties break by registration order.

import React from 'react';
import {
  contribute,
  renderSlot,
  SlotContribution,
  SlotKeys,
  ToolNodeContext,
} from './slots';

export interface ConversationNodeContext extends ToolNodeContext {
  /** Session id (cross-row aggregating contributions — the todo card — need it as a dedup/persistence key). */
  sessionId?: string;
}

export interface ConversationNodeDefinition {
  /** Registrant (built-in id or plugin name); unloading unregisters by owner in bulk. */
  owner: string;
  id: string;
  /** Exact tool name (e.g. run_code / todo_write); absent = wildcard fallback. */
  toolName?: string;
  /** Smaller tries first; same owner+id updates in place. */
  priority?: number;
  render: (ctx: ConversationNodeContext) => React.ReactNode | null;
}

interface NodeEntry {
  definition: ConversationNodeDefinition;
}

const byToolName = new Map<string, NodeEntry[]>();
const wildcard: NodeEntry[] = [];

function bucketFor(definition: ConversationNodeDefinition): NodeEntry[] {
  return definition.toolName
    ? byToolName.get(definition.toolName) ?? registerBucket(definition.toolName)
    : wildcard;
}

function registerBucket(toolName: string): NodeEntry[] {
  const bucket: NodeEntry[] = [];
  byToolName.set(toolName, bucket);
  return bucket;
}

/** Register one conversation node definition. Returns a handle: `dispose()` unregisters. */
export function registerConversationNode(
  definition: ConversationNodeDefinition,
): { dispose: () => void } {
  const bucket = bucketFor(definition);
  // Contract: same owner+id re-registration overwrites (plugin frame reload/remount resends declarations).
  for (let i = bucket.length - 1; i >= 0; i--) {
    const existing = bucket[i].definition;
    if (existing.owner === definition.owner && existing.id === definition.id) {
      bucket.splice(i, 1);
    }
  }
  const entry: NodeEntry = { definition };
  bucket.push(entry);
  bucket.sort((a, b) =>
    (a.definition.priority ?? 100) - (b.definition.priority ?? 100));
  return {
    dispose: () => {
      const index = bucket.findIndex((e) => e.definition === definition);
      if (index >= 0) bucket.splice(index, 1);
    },
  };
}

/** Unregister every node definition of one owner (the plugin unload path). */
export function removeConversationNodes(owner: string): void {
  for (const [toolName, bucket] of byToolName) {
    byToolName.set(toolName, bucket.filter((e) => e.definition.owner !== owner));
    if (byToolName.get(toolName)?.length === 0) byToolName.delete(toolName);
  }
  for (let i = wildcard.length - 1; i >= 0; i--) {
    if (wildcard[i].definition.owner === owner) wildcard.splice(i, 1);
  }
  for (const key of [...slotBridge.keys()]) {
    if (key.startsWith(`${owner}\u0000`)) slotBridge.delete(key);
  }
}

/** Unregister nodes matching an owner prefix (bulk cleanup of `plugin:`-prefixed contributions). */
export function removeConversationNodesByOwnerPrefix(prefix: string): void {
  const owners = new Set<string>();
  for (const bucket of byToolName.values()) {
    for (const entry of bucket) {
      if (entry.definition.owner.startsWith(prefix)) owners.add(entry.definition.owner);
    }
  }
  for (const entry of wildcard) {
    if (entry.definition.owner.startsWith(prefix)) owners.add(entry.definition.owner);
  }
  for (const owner of owners) removeConversationNodes(owner);
}

/** Render a tool node's detail area. No contribution hit → null (caller falls back to the default pre block). */
export function renderToolNode(ctx: ConversationNodeContext): React.ReactNode | null {
  const exact = byToolName.get(ctx.toolName);
  if (exact) {
    for (const entry of exact) {
      const node = entry.definition.render(ctx);
      if (node !== null && node !== undefined) return node;
    }
  }
  for (const entry of wildcard) {
    const node = entry.definition.render(ctx);
    if (node !== null && node !== undefined) return node;
  }
  return null;
}

// ── slot bridge: wire tool-detail slot contributions into the node registry ──
// slots.ts is the generic slot axis; conversationNodes is its first named consumer —
// each tool-detail slot contribution wraps into one wildcard node definition.

const slotBridge = new Map<string, SlotContribution>();

export function bridgeSlotContribution(
  contribution: SlotContribution & { slot: string },
): void {
  if (contribution.slot !== SlotKeys.conversationToolDetail) return;
  // The dedup key is owner+id (different owners may share an id); removeConversationNodes
  // clears the bridge table by owner prefix, so a remounted owner+id can register again.
  const key = `${contribution.owner}\u0000${contribution.id}`;
  if (slotBridge.has(key)) return;
  slotBridge.set(key, contribution);
  registerConversationNode({
    owner: contribution.owner,
    id: contribution.id,
    priority: contribution.priority,
    render: (ctx) => {
      const result = contribution.render(ctx);
      return result === null ? null : result.node;
    },
  });
}

export { SlotKeys };
export type { ToolNodeContext };

// ── Declarative template nodes (the sandbox iframe plugin contribution form) ──
// Template placeholders {{input}}/{{output}}/{{toolName}} are HTML-escaped before
// substitution; escaping happens here once, bypassable neither by plugins nor by renderers.

export function escapeHtml(text: string): string {
  return text.replace(/[&<>"]/g, (ch) => (
    { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[ch] ?? ch));
}

export interface DeclarativeNode {
  id: string;
  toolName: string;
  template: string;
}

export function renderTemplateNode(node: DeclarativeNode, ctx: ConversationNodeContext): React.ReactNode {
  const fill = (raw: string | null | undefined) =>
    escapeHtml(String(raw ?? ''));
  // Function-form substitution: $&, $`, $' in the replacement string are special patterns;
  // escaped tool output must insert literally (else `cost: $&` would echo the {{output}} placeholder).
  const html = node.template
    .replace(/\{\{input\}\}/g, () => fill(ctx.input))
    .replace(/\{\{output\}\}/g, () => fill(ctx.output))
    .replace(/\{\{toolName\}\}/g, () => fill(ctx.toolName));
  return React.createElement('div', {
    className: 'plugin-conversation-node',
    dangerouslySetInnerHTML: { __html: html },
  });
}
