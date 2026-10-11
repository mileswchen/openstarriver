/// <reference types="vitest/globals" />
import { describe, it, expect, beforeAll, afterAll, vi } from 'vitest';
import React from 'react';
import { render, screen, waitFor } from '@testing-library/react';
import { SubagentDelegationSection } from './app';

// Models-page configurable-provider catalog behavior moved to __models_tab.test.tsx
// (configured row cards + add-provider dropdown). This file keeps the
// Settings → Coding subagent delegation card tests.

const originalStarriver = (window as any).starriver;
beforeAll(() => {
  (window as any).starriver = {
    listModels: vi.fn().mockResolvedValue([]),
    listConfigurableProviders: vi.fn(),
    listSubagentConfigs: vi.fn(),
    setSubagentConfig: vi.fn(),
    removeModel: vi.fn(),
  };
});
afterAll(() => {
  (window as any).starriver = originalStarriver;
});

describe('Subagent delegation cards (Settings → Coding)', () => {
  it('renders subagent delegation cards with enable + model binding', async () => {
    (window as any).starriver.listModels = vi.fn().mockResolvedValue([
      {
        id: 'deepseek', name: 'DeepSeek', modelType: 'cloud', providerType: 'openai-compat',
        baseUrl: 'https://api.deepseek.com', apiKey: '', models: [
          { id: 'deepseek-chat', name: 'DeepSeek Chat' },
        ], defaultModel: 'deepseek-chat', apiFormat: 'openai-chat',
      },
    ]);
    (window as any).starriver.listSubagentConfigs = vi.fn().mockResolvedValue([
      {
        name: 'explorer', description: 'Codebase exploration, read-only',
        tools: ['read', 'grep'], readOnly: true, enabled: false, model: undefined,
      },
      {
        name: 'reviewer', description: 'Code review, read-only',
        tools: ['read', 'grep'], readOnly: true, enabled: true, model: 'deepseek/deepseek-chat',
      },
    ]);
    render(<SubagentDelegationSection />);
    await waitFor(() => expect(screen.getByText('Subagent Delegation')).toBeTruthy());

    expect(screen.getByText('explorer')).toBeTruthy();
    expect(screen.getByText('reviewer')).toBeTruthy();
    // The enabled template exposes its bound model in the select.
    const select = screen.getByDisplayValue('deepseek/deepseek-chat') as HTMLSelectElement;
    expect(select).toBeTruthy();
    // Two enable checkboxes (one per template).
    expect(screen.getAllByRole('checkbox').length).toBe(2);
  });
});
