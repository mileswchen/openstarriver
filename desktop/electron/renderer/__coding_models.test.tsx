/// <reference types="vitest/globals" />
import { describe, it, expect, vi, beforeEach } from 'vitest';
import React from 'react';
import { render, screen, waitFor, fireEvent } from '@testing-library/react';
import { I18nProvider } from './i18n';

declare global { interface Window { starriver: any; } }

const DEEPSEEK_MODELS = [
  { id: 'deepseek/deepseek-chat', name: 'DeepSeek · deepseek-chat', enabled: true },
  { id: 'deepseek/deepseek-v4-flash', name: 'DeepSeek · deepseek-v4-flash', enabled: true },
];

beforeEach(() => {
  Element.prototype.scrollIntoView = vi.fn();
  localStorage.setItem('starriver-language', 'en');
  window.starriver = {
    listWorkspaces: vi.fn().mockResolvedValue([]),
    createWorkspace: vi.fn(),
    renameWorkspace: vi.fn().mockResolvedValue(undefined),
    deleteWorkspace: vi.fn().mockResolvedValue(undefined),
    setActiveWorkspace: vi.fn().mockResolvedValue(undefined),
    listSessions: vi.fn().mockResolvedValue([]),
    getSessionMessages: vi.fn().mockResolvedValue([]),
    listArchivedSessions: vi.fn().mockResolvedValue([]),
    listSkills: vi.fn().mockResolvedValue([]),
    listMcpServers: vi.fn().mockResolvedValue([]),
    listModels: vi.fn().mockResolvedValue([
      { id: 'deepseek', name: 'DeepSeek', modelType: 'cloud', providerType: 'openai-compat',
        baseUrl: 'https://api.deepseek.com', apiKey: '', models: [
          { id: 'deepseek-chat', name: 'deepseek-chat' },
          { id: 'deepseek-v4-flash', name: 'deepseek-v4-flash' },
        ], defaultModel: 'deepseek-v4-flash', apiFormat: 'openai-chat', reasoningEffort: null },
    ]),
    listAgentModelBindings: vi.fn().mockResolvedValue([
      { agentName: 'Starriver Code', activeModel: 'deepseek/deepseek-v4-flash', boundModels: DEEPSEEK_MODELS, reasoningEffort: null },
    ]),
    setAgentActiveModel: vi.fn().mockResolvedValue(undefined),
    setAgentReasoningEffort: vi.fn().mockResolvedValue(undefined),
    getCurrentAgent: vi.fn().mockResolvedValue(null),
    getWorktreeStatus: vi.fn().mockResolvedValue({ isRepo: true, branch: 'main', isWorktree: false, dirty: false }),
    readWorkspaceInstructions: vi.fn().mockResolvedValue(null),
    setAgentMode: vi.fn().mockResolvedValue(undefined),
  };
});

describe('CodingView model dropdown', () => {
  it('switches to the backend coding agent and lists its bound models', async () => {
    const { CodingView, WorkspaceProvider, WorkspaceSection } = await import('./app');
    render(<I18nProvider><WorkspaceProvider><WorkspaceSection onOpenCoding={() => {}} /><CodingView debugEnabled={false} /></WorkspaceProvider></I18nProvider>);

    await waitFor(() => {
    });

    // The model selector lives in the composer's control row (right end,
    // next to the reasoning-level select) and lists the agent's bound models.
    await waitFor(async () => {
      const modelSelect = document.querySelector('select[aria-label="coding-model"]') as HTMLSelectElement;
      expect(modelSelect).toBeTruthy();
      const options = Array.from(modelSelect.querySelectorAll('option')).map(o => o.textContent);
      expect(options).toEqual(expect.arrayContaining(['DeepSeek · deepseek-v4-flash']));
      // Every model: only explicit levels — no Default/Off — and the default
      // selection is High.
      const reasoning = document.querySelector('select[aria-label="coding-reasoning"]') as HTMLSelectElement;
      expect(Array.from(reasoning!.querySelectorAll('option')).map(o => o.value))
        .toEqual(['low', 'high', 'max']);
      expect(reasoning!.value).toBe('high');
      // Legacy stored value (null) migrates to High so the engine matches the UI.
      await waitFor(() =>
        expect(window.starriver.setAgentReasoningEffort).toHaveBeenCalledWith('Starriver Code', 'high')
      );
    });
  });

  it('hides Default/Off for every model and defaults the select to High', async () => {
    window.starriver.listAgentModelBindings = vi.fn().mockResolvedValue([
      { agentName: 'Starriver Code', activeModel: 'openai/gpt-x', boundModels: [
        { id: 'openai/gpt-x', name: 'OpenAI · gpt-x', enabled: true },
      ], reasoningEffort: null },
    ]);
    const { CodingView, WorkspaceProvider, WorkspaceSection } = await import('./app');
    render(<I18nProvider><WorkspaceProvider><WorkspaceSection onOpenCoding={() => {}} /><CodingView debugEnabled={false} /></WorkspaceProvider></I18nProvider>);

    await waitFor(() => {
      const reasoning = document.querySelector('select[aria-label="coding-reasoning"]') as HTMLSelectElement;
      expect(Array.from(reasoning.querySelectorAll('option')).map(o => o.value))
        .toEqual(['low', 'high', 'max']);
      expect(reasoning.value).toBe('high');
    });
  });

  it('persists a reasoning effort choice through the bindings API', async () => {
    window.starriver.listAgentModelBindings = vi.fn().mockResolvedValue([
      { agentName: 'Starriver Code', activeModel: 'deepseek/deepseek-v4-flash',
        boundModels: DEEPSEEK_MODELS, reasoningEffort: 'low' },
    ]);
    const { CodingView, WorkspaceProvider, WorkspaceSection } = await import('./app');
    render(<I18nProvider><WorkspaceProvider><WorkspaceSection onOpenCoding={() => {}} /><CodingView debugEnabled={false} /></WorkspaceProvider></I18nProvider>);

    const reasoning = await waitFor(() =>
      document.querySelector('select[aria-label="coding-reasoning"]') as HTMLSelectElement
    );
    expect(reasoning.value).toBe('low');
    window.starriver.setAgentReasoningEffort.mockClear();
    fireEvent.change(reasoning, { target: { value: 'high' } });
    await waitFor(() =>
      expect(window.starriver.setAgentReasoningEffort).toHaveBeenCalledWith('Starriver Code', 'high')
    );
  });
});
