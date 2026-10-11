// FPM P2 contribution registry: one entry point for tabs/panels/commands/themes.
//
// Built-in modules and third-party plugins register through the same path; the shell renders only registry content.
// Tab ids degrade from a union type to string — an extension point, not a frozen enum.

import type { ReactNode } from 'react';

export type TabId = string;

export interface TabRenderContext {
  debugEnabled: boolean;
}

export interface TabContribution {
  id: TabId;
  /** i18n key (e.g. `nav.chat`). */
  labelKey: string;
  /** Inline SVG icon (stroke="currentColor"). */
  icon: string;
  render: (ctx: TabRenderContext) => ReactNode;
}

export interface CommandContribution {
  id: string;
  command: string;
  description: string;
  group: string;
}

export interface ThemeContribution {
  name: string;
  /** CSS custom-property overrides (`:root { ... }`). */
  css: string;
}

/** Full-shell replacement (advanced permission `shell:replace`): a plugin takes over the window. */
export interface ShellReplacementContribution {
  id: string;
  name: string;
  render: (ctx: TabRenderContext) => ReactNode;
}

const tabs = new Map<TabId, TabContribution>();
const commands: CommandContribution[] = [];
const themes = new Map<string, ThemeContribution>();
let shellReplacement: ShellReplacementContribution | null = null;

/** Register a tab contribution (built-ins call it at app.tsx load; P3 plugins via the bridge). */
export function registerTab(tab: TabContribution): void {
  tabs.set(tab.id, tab);
}

export function unregisterTab(id: TabId): void {
  tabs.delete(id);
}

export function listTabs(): TabContribution[] {
  return [...tabs.values()];
}

export function getTab(id: TabId): TabContribution | undefined {
  return tabs.get(id);
}

export function registerCommand(command: CommandContribution): void {
  commands.push(command);
}

export function listCommands(): CommandContribution[] {
  return [...commands];
}

export function registerTheme(theme: ThemeContribution): void {
  themes.set(theme.name, theme);
}

export function unregisterTheme(name: string): void {
  themes.delete(name);
}

export function getTheme(name: string): ThemeContribution | undefined {
  return themes.get(name);
}

export function listThemes(): ThemeContribution[] {
  return [...themes.values()];
}

/** Register/unregister a full-shell replacement contribution (only plugins declaring `shell:replace`). */
export function registerShellReplacement(contribution: ShellReplacementContribution): void {
  shellReplacement = contribution;
}

export function unregisterShellReplacement(): void {
  shellReplacement = null;
}

export function getShellReplacement(): ShellReplacementContribution | null {
  return shellReplacement;
}
