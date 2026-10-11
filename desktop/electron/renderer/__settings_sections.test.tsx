/// <reference types="vitest/globals" />
// WI-4 settings hot-update sections: agent/prompt patch semantics (null =
// clear), provider read-modify-write, plugin inventory rendering/uninstall.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import React from 'react';
import { render, screen, fireEvent } from '@testing-library/react';
import { I18nProvider } from './i18n';
import { SettingsAgentSection, SettingsModelsSection, SettingsPluginsSection } from './app';

vi.mock('./contrib/pluginHost', () => ({ syncPluginTabs: vi.fn(async () => {}) }));

const agentSettings = () => ({
  agent: { max_parallel_tool_calls: 4, turn_tool_call_budget: 666, context_window: 65536 },
  prompt: { include_identity: true, include_runtime_context: true },
  llm: { providers: [], execution_mode: 'standard' },
});

const providerDoc = () => ({
  agent: {},
  prompt: {},
  llm: {
    execution_mode: 'standard',
    providers: [
      {
        id: 'deepseek', name: 'DeepSeek', model_type: 'cloud', provider_type: 'openai-compat',
        base_url: 'https://api.deepseek.com', api_key: '!file:model/deepseek', model_path: null,
        models: [{ id: 'deepseek-chat', name: 'Chat' }], default_model: 'deepseek-chat',
        concurrency: null, max_tokens: null, context_window: null, temperature: null,
        api_format: 'openai-chat', reasoning_effort: null, supports_vision: false,
      },
      {
        id: 'ollama', name: 'Ollama', model_type: 'local', provider_type: 'ollama',
        base_url: 'http://localhost:11434', api_key: '', model_path: null,
        models: [], default_model: 'qwen3', concurrency: null, max_tokens: null,
        context_window: null, temperature: null, api_format: 'openai-chat',
        reasoning_effort: null, supports_vision: null,
      },
    ],
  },
});

beforeEach(() => {
  (window as any).confirm = vi.fn(() => true);
});
afterEach(() => {
  delete (window as any).starriver;
  delete (window as any).confirm;
  vi.restoreAllMocks();
});

describe('SettingsAgentSection — 设置分区热更新', () => {
  it('读 settings.get,保存发送深合并 patch(空输入 = null 清除覆盖)', async () => {
    const updateSettings = vi.fn(async () => agentSettings());
    (window as any).starriver = {
      getSettings: vi.fn(async () => agentSettings()),
      updateSettings,
    };
    render(<I18nProvider><SettingsAgentSection /></I18nProvider>);

    const maxParallel = await screen.findByLabelText('agent-max-parallel') as HTMLInputElement;
    expect(maxParallel.value).toBe('4');
    // BUG-138 settings seam: the repeat breaker's per-turn tool-call budget
    // round-trips through the same form (default 666).
    const turnBudget = await screen.findByLabelText('agent-turn-budget') as HTMLInputElement;
    expect(turnBudget.value).toBe('666');
    // context_window has no field here any more — per-provider context windows
    // live in the Models settings; the global default is not editable from
    // Agent & Prompt and must not be touched by the patch.
    expect(screen.queryByLabelText('agent-context-window')).toBeNull();

    fireEvent.change(maxParallel, { target: { value: '8' } });
    fireEvent.change(turnBudget, { target: { value: '999' } });
    fireEvent.click(screen.getByLabelText('agent-save'));

    expect(updateSettings).toHaveBeenCalledTimes(1);
    expect(updateSettings).toHaveBeenCalledWith({
      agent: {
        max_parallel_tool_calls: 8,
        turn_tool_call_budget: 999,
        compaction_threshold_ratio: null,
        time_refresh_interval_ms: null,
      },
      prompt: {
        include_identity: true,
        include_runtime_context: true,
        instructions_max_bytes: null,
        persona_global: null,
      },
    });
    expect(await screen.findByTestId('settings-save-status')).toBeTruthy();
  });

  it('非法数字阻断保存并提示', async () => {
    const updateSettings = vi.fn(async () => agentSettings());
    (window as any).starriver = {
      getSettings: vi.fn(async () => agentSettings()),
      updateSettings,
    };
    render(<I18nProvider><SettingsAgentSection /></I18nProvider>);

    fireEvent.change(await screen.findByLabelText('agent-max-parallel'), { target: { value: 'abc' } });
    fireEvent.click(screen.getByLabelText('agent-save'));

    expect(updateSettings).not.toHaveBeenCalled();
    expect(await screen.findByTestId('settings-save-status')).toBeTruthy();
  });

  it('host 拒绝补丁时保留表单并显示错误', async () => {
    (window as any).starriver = {
      getSettings: vi.fn(async () => agentSettings()),
      updateSettings: vi.fn(async () => { throw new Error('invalid settings patch: boom'); }),
    };
    render(<I18nProvider><SettingsAgentSection /></I18nProvider>);

    fireEvent.click(await screen.findByLabelText('agent-save'));
    const status = await screen.findByTestId('settings-save-status');
    expect(status.textContent).toContain('invalid settings patch: boom');
  });
});

describe('SettingsModelsSection — model-provider 路由配置', () => {
  it('read-modify-write:只改目标 provider,其余路由原样透传', async () => {
    const doc = providerDoc();
    const updateSettings = vi.fn(async () => doc);
    (window as any).starriver = {
      getSettings: vi.fn(async () => doc),
      updateSettings,
    };
    render(<I18nProvider><SettingsModelsSection /></I18nProvider>);

    const baseUrl = await screen.findByLabelText('provider-deepseek-base-url') as HTMLInputElement;
    expect(baseUrl.value).toBe('https://api.deepseek.com');
    expect((screen.getByLabelText('provider-deepseek-api-key') as HTMLInputElement).value)
      .toBe('!file:model/deepseek');

    fireEvent.change(baseUrl, { target: { value: 'https://proxy.example.com' } });
    fireEvent.click(screen.getByLabelText('provider-deepseek-save'));

    expect(updateSettings).toHaveBeenCalledTimes(1);
    const patch = updateSettings.mock.calls[0][0] as any;
    expect(Object.keys(patch)).toEqual(['llm']);
    expect(patch.llm.providers).toHaveLength(2);
    const [savedDs, untouchedOllama] = patch.llm.providers;
    expect(savedDs).toEqual(expect.objectContaining({
      id: 'deepseek',
      base_url: 'https://proxy.example.com',
      api_key: '!file:model/deepseek',
      api_format: 'openai-chat',
      supports_vision: false,
    }));
    expect(savedDs.models).toEqual([{ id: 'deepseek-chat', name: 'Chat' }]);
    expect(untouchedOllama).toEqual(doc.llm.providers[1]);
  });

  it('空 provider 列表显示引导文案', async () => {
    (window as any).starriver = {
      getSettings: vi.fn(async () => ({ llm: { providers: [] } })),
    };
    render(<I18nProvider><SettingsModelsSection /></I18nProvider>);
    await screen.findByText(/No provider routes configured|尚未配置任何 provider/);
  });
});

describe('SettingsPluginsSection — 插件清单', () => {
  const plugin = {
    name: 'demo', description: 'Demo plugin', version: '1.2.0', skills: ['greet'],
    sourceUrl: '', installedAt: '2026-09-08T10:00:00Z',
    ui: {
      tabs: [{ id: 'main', title: 'Demo Panel' }],
      permissions: ['session:read'],
      theme: null,
    },
  };

  it('渲染贡献物(UI 标签页)与权限清单', async () => {
    (window as any).starriver = { listPlugins: vi.fn(async () => [plugin]) };
    render(<I18nProvider><SettingsPluginsSection /></I18nProvider>);

    const card = await screen.findByTestId('plugin-card-demo');
    expect(card.textContent).toContain('Demo Panel');
    expect(card.textContent).toContain('session:read');
    expect(card.textContent).toContain('greet');
    expect(card.textContent).toContain('1.2.0');
  });

  it('卸载:确认后调用 uninstallPlugin 并刷新清单', async () => {
    const listPlugins = vi.fn()
      .mockResolvedValueOnce([plugin])
      .mockResolvedValueOnce([]);
    const uninstallPlugin = vi.fn(async () => []);
    (window as any).starriver = { listPlugins, uninstallPlugin };

    render(<I18nProvider><SettingsPluginsSection /></I18nProvider>);
    const card = await screen.findByTestId('plugin-card-demo');
    fireEvent.click(card.querySelector('button')!);

    expect(uninstallPlugin).toHaveBeenCalledWith('demo');
    await screen.findByText(/No plugins installed|尚未安装插件/);
  });

  it('取消确认则不卸载', async () => {
    (window as any).confirm = vi.fn(() => false);
    const uninstallPlugin = vi.fn(async () => []);
    (window as any).starriver = { listPlugins: vi.fn(async () => [plugin]), uninstallPlugin };

    render(<I18nProvider><SettingsPluginsSection /></I18nProvider>);
    const card = await screen.findByTestId('plugin-card-demo');
    fireEvent.click(card.querySelector('button')!);

    expect(uninstallPlugin).not.toHaveBeenCalled();
  });
});
