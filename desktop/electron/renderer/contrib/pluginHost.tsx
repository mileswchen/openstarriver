// FPM P3 plugin UI host: plugin frames run in sandboxed iframes (no node, no external
// calls), talking to the shell only through a postMessage bridge; the shell filters against
// the manifest permission whitelist before forwarding host calls over the protocol client

import React, { useEffect, useRef } from 'react';
import { createIpcClient, EventEnvelope, ProtocolClient } from '../client/client';
import { PROTOCOL_METHODS } from '../client/client.gen';
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
}

export interface PluginUiInfo {
  entry: string;
  entryUrl: string;
  tabs: PluginUiTabInfo[];
  permissions: string[];
  theme: string | null;
  themeCss: string | null;
}

export interface PluginUiContribution {
  name: string;
  ui?: PluginUiInfo | null;
}

const GENERIC_PLUGIN_ICON =
  '<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round"><path d="M3 3h3.5a1.5 1.5 0 0 1 3 0H13v3.5a1.5 1.5 0 0 1 0 3V13H9.5a1.5 1.5 0 0 1-3 0H3V9.5a1.5 1.5 0 0 1 0-3z"/></svg>';

/** Methods a plugin frame may call = manifest permissions ⊆ required permissions. */
export function canInvoke(permissions: string[], method: string): boolean {
  const required = PROTOCOL_METHODS[method]?.permissions ?? [];
  return required.every((permission) => permissions.includes(permission));
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

      getClient()
        .call(method, params ?? {})
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
      registerTab({
        id,
        labelKey: tab.title,
        icon: GENERIC_PLUGIN_ICON,
        render: () => <PluginTab plugin={plugin} tab={tab} />,
      });
    }
  }
}
