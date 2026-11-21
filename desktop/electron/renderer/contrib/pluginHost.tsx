// FPM P3 plugin UI host: plugin frames run in sandboxed iframes (no node, no external
// calls), talking to the shell only through a postMessage bridge; the shell filters against
// the manifest permission whitelist before forwarding host calls over the protocol client

import React, { useEffect, useRef } from 'react';
import { createIpcClient, EventEnvelope, ProtocolClient } from '../client/client';
import { PROTOCOL_METHODS } from '../client/client.gen';
import { hasTranslation } from '../i18n';
import {
  registerShellReplacement,
  registerTab,
  registerTheme,
  TabId,
  unregisterShellReplacement,
  unregisterTab,
  unregisterTheme,
} from './registry';
import {
  registerConversationNode,
  removeConversationNodesByOwnerPrefix,
  renderTemplateNode,
} from './conversationNodes';

// Lazy creation: importing the module outside Electron (jsdom tests) builds no IPC transport.
let protocolClient: ProtocolClient | null = null;
function getClient(): ProtocolClient {
  protocolClient ??= createIpcClient();
  return protocolClient;
}

export interface PluginUiTabInfo {
  id: string;
  title: string;
  /** Per-language titles from the manifest — the plugin's own i18n for its tab. */
  titles?: Record<string, string>;
}

export interface PluginUiInfo {
  entry: string;
  entryUrl: string;
  tabs: PluginUiTabInfo[];
  permissions: string[];
  theme: string | null;
  themeCss: string | null;
  /** Full `<svg>` markup served from the plugin's manifest icon file; sidebar renders it at 16px with currentColor. */
  iconSvg?: string | null;
  /** P1 manifest opt-in: the frame may spawn NEW host sessions (bare session.execute). Default false. */
  background?: boolean;
  /**
   * P2 manifest autonomy ceiling: the highest permission preset this plugin's
   * UI may request (`pluginTask.run` / `session.execute` /
   * `session.setPermissionPreset`). Undeclared → `manual` (fail-closed).
   */
  maxPermissionPreset?: string | null;
}

export interface PluginUiContribution {
  name: string;
  ui?: PluginUiInfo | null;
}

const GENERIC_PLUGIN_ICON =
  '<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round"><path d="M5.8 2.2v3.3M10.2 2.2v3.3"/><path d="M4.4 5.5h7.2v1.9a3.6 3.6 0 0 1-7.2 0z"/><path d="M8 11.6v2.2"/></svg>';

/** Methods a plugin frame may call = manifest permissions ⊆ required permissions. */
export function canInvoke(permissions: string[], method: string): boolean {
  const required = PROTOCOL_METHODS[method]?.permissions ?? [];
  return required.every((permission) => permissions.includes(permission));
}

/**
 * Plugin platform P1 bridge gate: a bare `session.execute` (no sessionId) spawns
 * a NEW host session — the Chat-sidebar pollution this platform exists to end.
 * Data operations belong on `tools.call`; follow-ups pass an existing sessionId.
 * Spawning is allowed only when the UI manifest declares `background: true`
 * (such sessions are stamped `origin=plugin:<name>` and grouped apart from chat).
 */
export function sessionSpawnDenied(
  ui: Pick<PluginUiInfo, 'background'> | null | undefined,
  method: string,
  params: unknown,
): boolean {
  if (method !== 'session.execute' || ui?.background) return false;
  const sessionId = (params as { sessionId?: unknown } | null | undefined)?.sessionId;
  return !(typeof sessionId === 'string' && sessionId.length > 0);
}

/**
 * Plugin platform P1: params forwarded to the host for a plugin frame call.
 * `session.execute` is branded with the manifest identity — the frame cannot
 * claim another origin (whatever it sends is overwritten). The host stamps
 * write-once at creation; continuations ignore it, so a user chat is never
 * re-branded.
 * P2: every `pluginTask.*` call is branded with the manifest `plugin` identity
 * (same overwrite rule) — the host ownership-checks each run handle against
 * it, so a frame can never address another plugin's runs or a user chat.
 * Other methods pass through untouched.
 */
export function pluginCallParams(pluginName: string, method: string, params: unknown): unknown {
  const base = typeof params === 'object' && params !== null ? params : {};
  if (method.startsWith('pluginTask.')) {
    return { ...base, plugin: pluginName };
  }
  if (method !== 'session.execute') return params ?? {};
  return { ...base, origin: `plugin:${pluginName}` };
}

/** Preset autonomy order (P2 ceiling governance). Unknown names rank null. */
const PRESET_RANK: Record<string, number> = {
  manual: 0,
  'accept-edits': 1,
  auto: 2,
  'danger-full-access': 3,
};

export function presetRank(preset: string): number | null {
  return Object.prototype.hasOwnProperty.call(PRESET_RANK, preset) ? PRESET_RANK[preset] : null;
}

/**
 * Plugin platform P2 bridge gate: the manifest's `maxPermissionPreset` is the
 * plugin's autonomy ceiling — a promise to the user about how much autonomy
 * the plugin can ever ask for. Undeclared → `manual` (fail-closed). Requests
 * above the ceiling (or malformed preset names) are FORBIDDEN here, before
 * the host ever sees them. Covers every escalation path: task runs, bare
 * session spawns, and direct preset changes on an existing session.
 * An omitted preset defaults to `manual` and is always within the ceiling.
 */
export function presetCeilingDenied(
  ui: Pick<PluginUiInfo, 'maxPermissionPreset'> | null | undefined,
  method: string,
  params: unknown,
): boolean {
  const presetParam =
    method === 'pluginTask.run' || method === 'session.execute'
      ? 'permissionPreset'
      : method === 'session.setPermissionPreset'
        ? 'preset'
        : null;
  if (!presetParam) return false;
  const requested = (params as Record<string, unknown> | null | undefined)?.[presetParam];
  if (requested === undefined || requested === null) return false; // omitted → manual
  if (typeof requested !== 'string') return true;
  const requestedRank = presetRank(requested);
  if (requestedRank === null) return true; // unknown preset name → fail-closed
  const ceiling = ui?.maxPermissionPreset ?? 'manual';
  const ceilingRank = presetRank(ceiling) ?? 0; // malformed declaration → manual
  return requestedRank > ceilingRank;
}

interface BridgeMessage {
  kind: 'invoke' | 'unsubscribe' | 'registerNodes';
  id?: string;
  method?: string;
  params?: unknown;
  subscription?: string;
}

function postToFrame(frame: HTMLIFrameElement | null, message: unknown): void {
  frame?.contentWindow?.postMessage(message, '*');
}

export function PluginTab({
  plugin,
  tab,
}: {
  plugin: PluginUiContribution;
  tab: PluginUiTabInfo;
}) {
  const iframeRef = useRef<HTMLIFrameElement>(null);
  const pluginName = plugin.name;

  useEffect(() => {
    const subscriptions = new Map<string, () => void>();
    const onMessage = (event: MessageEvent) => {
      const frame = iframeRef.current;
      if (!frame || event.source !== frame.contentWindow) return;
      const message = event.data as BridgeMessage | undefined;
      if (!message || typeof message !== 'object') return;
      if (message.kind === 'registerNodes') {
        // Declarative conversation node: the plugin contributes { id, toolName, template };
        // template interpolation is HTML-escaped centrally in conversationNodes.renderTemplateNode.
        const nodes = (message as { nodes?: Array<{ id?: string; toolName?: string; template?: string }> }).nodes;
        for (const node of Array.isArray(nodes) ? nodes : []) {
          if (!node?.id || !node.toolName || !node.template) continue;
          registerConversationNode({
            owner: `plugin:${pluginName}`,
            id: node.id,
            toolName: node.toolName,
            priority: 150,
            render: (ctx) => renderTemplateNode(node, ctx),
          });
        }
        return;
      }

      if (message.kind === 'unsubscribe' && message.subscription) {
        subscriptions.get(message.subscription)?.();
        subscriptions.delete(message.subscription);
        return;
      }
      if (message.kind !== 'invoke' || !message.method || !message.id) return;

      const { id, method, params } = message;
      if (!canInvoke(plugin.ui?.permissions ?? [], method)) {
        postToFrame(frame, {
          kind: 'response',
          id,
          error: { code: 'FORBIDDEN', message: `plugin \`${pluginName}\` lacks permission for \`${method}\`` },
        });
        return;
      }
      if (sessionSpawnDenied(plugin.ui, method, params)) {
        postToFrame(frame, {
          kind: 'response',
          id,
          error: {
            code: 'FORBIDDEN',
            message:
              `plugin \`${pluginName}\` may not spawn host sessions: use \`tools.call\` for data operations, ` +
              'pass an existing `sessionId` to continue a session, or declare `"background": true` in ui/manifest.json',
          },
        });
        return;
      }
      if (presetCeilingDenied(plugin.ui, method, params)) {
        const ceiling = plugin.ui?.maxPermissionPreset ?? 'manual';
        postToFrame(frame, {
          kind: 'response',
          id,
          error: {
            code: 'FORBIDDEN',
            message:
              `plugin \`${pluginName}\` requested a permission preset above its manifest ceiling ` +
              `\`maxPermissionPreset: "${ceiling}"\` (undeclared defaults to "manual")`,
          },
        });
        return;
      }

      // P1: plugin-spawned sessions are branded from the manifest identity —
      // the frame cannot claim another origin (the bridge overwrites whatever
      // it sends). The host stamps write-once at creation; continuations of
      // existing sessions ignore it, so a user chat is never re-branded.
      getClient()
        .call(method, pluginCallParams(pluginName, method, params))
        .then((result) => {
          postToFrame(frame, { kind: 'response', id, result });
          // Streaming method: subscribe and forward events to the plugin frame.
          if (PROTOCOL_METHODS[method]?.streaming && result && typeof result === 'object') {
            const subscription = (result as { subscription?: string }).subscription;
            if (subscription) {
              const unsubscribe = getClient().subscribe(subscription, (envelope: EventEnvelope) => {
                postToFrame(frame, { kind: 'event', subscription, envelope });
              });
              subscriptions.set(subscription, unsubscribe);
            }
          }
        })
        .catch((error) => {
          postToFrame(frame, {
            kind: 'response',
            id,
            error: { code: error?.code ?? 'INTERNAL', message: String(error?.message ?? error) },
          });
        });
    };

    window.addEventListener('message', onMessage);
    return () => {
      window.removeEventListener('message', onMessage);
      subscriptions.forEach((unsubscribe) => unsubscribe());
    };
  }, [plugin.ui, pluginName]);

  return (
    <iframe
      ref={iframeRef}
      sandbox="allow-scripts"
      src={plugin.ui?.entryUrl}
      title={tab.title}
      style={{ width: '100%', height: '100%', border: 'none', background: 'transparent' }}
    />
  );
}

const registeredPluginTabIds: TabId[] = [];
const registeredPluginThemeNames: string[] = [];

/** Fetch plugin UI contributions from the host and register tabs/themes (called after install/uninstall). */
export async function syncPluginTabs(): Promise<void> {
  for (const id of registeredPluginTabIds.splice(0)) {
    unregisterTab(id);
  }
  for (const name of registeredPluginThemeNames.splice(0)) {
    unregisterTheme(name);
  }
  unregisterShellReplacement();
  // Plugin node registrations use the `plugin:<name>` owner, cleaned up by prefix — an uninstalled
  // plugin's contributions must not keep rendering in the chain.
  removeConversationNodesByOwnerPrefix('plugin:');
  const plugins = await getClient().call<PluginUiContribution[]>('plugins.list', {});
  for (const plugin of plugins) {
    const ui = plugin.ui;
    if (!ui || ui.tabs.length === 0) continue;
    if (ui.theme && ui.themeCss) {
      const themeName = `plugin:${plugin.name}`;
      registerTheme({ name: themeName, css: ui.themeCss });
      registeredPluginThemeNames.push(themeName);
    }
    // Full-shell replacement: the manifest declares shell:replace and provides a __shell tab.
    const shellTab = ui.permissions.includes('shell:replace')
      ? ui.tabs.find((t) => t.id === '__shell')
      : undefined;
    if (shellTab) {
      registerShellReplacement({
        id: `plugin:${plugin.name}:__shell`,
        name: plugin.name,
        render: () => <PluginTab plugin={plugin} tab={shellTab} />,
      });
    }
    for (const tab of ui.tabs) {
      if (tab.id === '__shell') continue;
      const id = `plugin:${plugin.name}:${tab.id}`;
      registeredPluginTabIds.push(id);
      // Sidebar label precedence: ① manifest `titles` (plugin-owned i18n, passed
      // as `labels`); ② shell dictionary key `pluginTabs.<plugin>.<tabId>` in
      // en.json/zh.json (fallback for first-party plugins whose manifests don't
      // carry translations yet); ③ raw manifest `title` (t() renders it verbatim).
      // No re-sync needed on language switch — the sidebar re-renders via i18n
      // context and picks the language from `labels` at render time.
      const labelKey = `pluginTabs.${plugin.name}.${tab.id}`;
      registerTab({
        id,
        labelKey: hasTranslation(labelKey) ? labelKey : tab.title,
        labels: tab.titles && Object.keys(tab.titles).length > 0 ? tab.titles : undefined,
        icon: ui.iconSvg || GENERIC_PLUGIN_ICON,
        render: () => <PluginTab plugin={plugin} tab={tab} />,
      });
    }
  }
}
