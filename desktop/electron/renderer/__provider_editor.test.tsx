/// <reference types="vitest/globals" />
// ProviderEditorCard preset-merge and save integration (semantics ported from the
// old AddModelModal cloud_tab tests): entry ∪ preset field-merge precedence,
// advanced-parameter pass-through, cloud baseUrl required gating, local engines
import { describe, it, expect, beforeAll, afterAll, vi } from 'vitest';
import React from 'react';
import { I18nProvider } from './i18n';
import { render, screen, waitFor, fireEvent, cleanup } from '@testing-library/react';
import { ProviderEditorCard } from './app';

const originalStarriver = (window as any).starriver;

const DEEPSEEK_PRESET = {
  id: 'deepseek', name: 'DeepSeek', providerType: 'cloud', baseUrl: 'https://api.deepseek.com',
  models: ['deepseek-chat', 'deepseek-reasoner'], requiresApiKey: true, enabled: true,
  apiFormat: 'openai-chat', reasoningEffort: 'high',
};

const ANTHROPIC_PRESET = {
  id: 'anthropic', name: 'Anthropic', providerType: 'cloud', baseUrl: 'https://api.anthropic.com',
  models: ['claude-sonnet-4-5', 'claude-opus-4-5'], requiresApiKey: true, enabled: true,
  apiFormat: 'anthropic',
};

function deepseekEntry(overrides: any = {}): any {
  return {
    id: 'deepseek', name: 'DeepSeek', modelType: 'cloud', providerType: 'openai-compat',
    baseUrl: 'https://api.deepseek.com', apiKey: '!keychain:provider/deepseek',
    models: [{ id: 'deepseek-chat', name: 'DeepSeek Chat' }, { id: 'deepseek-reasoner', name: 'Reasoner' }],
    defaultModel: 'deepseek-chat', apiFormat: 'openai-chat', reasoningEffort: 'high',
    ...overrides,
  };
}

function renderCard(props: { entry?: any; preset?: any; onClose?: (changed: boolean, name: string) => void }) {
  return render(
    <I18nProvider>
      <ProviderEditorCard
        entry={props.entry}
        preset={props.preset}
        onClose={props.onClose ?? (() => {})}
      />
    </I18nProvider>,
  );
}

beforeAll(() => {
  localStorage.setItem('starriver-language', 'en');
  (window as any).starriver = { addModel: vi.fn().mockResolvedValue(undefined) };
});
afterAll(() => {
  (window as any).starriver = originalStarriver;
  localStorage.removeItem('starriver-language');
});

describe('ProviderEditorCard — entry ∪ preset 合并优先级', () => {
  it('已有条目的 variants/显示名胜出，引用密钥绝不回填', async () => {
    renderCard({ entry: deepseekEntry(), preset: DEEPSEEK_PRESET });
    expect(await screen.findByDisplayValue('DeepSeek Chat')).toBeTruthy();
    expect(screen.getByDisplayValue('Reasoner')).toBeTruthy();
    expect((screen.getByLabelText('API key') as HTMLInputElement).value).toBe('');
  });

  it('baseUrl 空缺时回退到预设值', () => {
    renderCard({ entry: deepseekEntry({ baseUrl: '' }), preset: DEEPSEEK_PRESET });
    expect(screen.getByDisplayValue('https://api.deepseek.com')).toBeTruthy();
  });

  it('高级参数按 entry 预填（并发/maxTokens/contextWindow/temperature）', () => {
    renderCard({
      entry: deepseekEntry({ concurrency: 4, maxTokens: 8192, contextWindow: 128000, temperature: 0.3 }),
      preset: DEEPSEEK_PRESET,
    });
    expect(screen.getByDisplayValue('8192')).toBeTruthy();
    expect(screen.getByDisplayValue('128000')).toBeTruthy();
    expect(screen.getByDisplayValue('0.3')).toBeTruthy();
    expect(screen.getByDisplayValue('4')).toBeTruthy();
  });

  it('apiFormat：entry 显式值 > 预设 > openai-chat', () => {
    const { unmount } = renderCard({
      entry: deepseekEntry({ id: 'anthropic', name: 'Anthropic', apiFormat: undefined }),
      preset: ANTHROPIC_PRESET,
    });
    expect((screen.getByLabelText('API Format') as HTMLSelectElement).value).toBe('anthropic');
    unmount();

    renderCard({ entry: deepseekEntry({ apiFormat: 'anthropic' }), preset: DEEPSEEK_PRESET });
    expect((screen.getByLabelText('API Format') as HTMLSelectElement).value).toBe('anthropic');
  });

  it('reasoningEffort：entry 显式值 > 预设', () => {
    const { unmount } = renderCard({ entry: deepseekEntry({ reasoningEffort: undefined }), preset: DEEPSEEK_PRESET });
    expect((screen.getByLabelText('Reasoning Effort (DeepSeek thinking)') as HTMLSelectElement).value).toBe('high');
    unmount();

    renderCard({ entry: deepseekEntry({ reasoningEffort: 'low' }), preset: DEEPSEEK_PRESET });
    expect((screen.getByLabelText('Reasoning Effort (DeepSeek thinking)') as HTMLSelectElement).value).toBe('low');
  });
});

describe('ProviderEditorCard — 保存集成（models.add 透传）', () => {
  it('Anthropic 预设：cloud/openai-compat + anthropic 协议，密钥透传', async () => {
    const addModel = vi.fn().mockResolvedValue(undefined);
    (window as any).starriver.addModel = addModel;
    const onClose = vi.fn();
    renderCard({ preset: ANTHROPIC_PRESET, onClose });

    fireEvent.change(screen.getByLabelText('API key'), { target: { value: 'sk-test' } });
    fireEvent.click(screen.getByText('Save'));

    await waitFor(() => expect(addModel).toHaveBeenCalledTimes(1));
    const args = addModel.mock.calls[0];
    expect(args.slice(0, 5)).toEqual(['anthropic', 'Anthropic', 'cloud', 'openai-compat', 'https://api.anthropic.com']);
    expect(args[5]).toBe('sk-test');
    expect(args[6]).toBeUndefined();            // modelPath
    expect(args[7]).toBe('claude-sonnet-4-5');  // defaultModel = first preset model
    expect(JSON.parse(args[8])).toEqual([
      { id: 'claude-sonnet-4-5', name: 'claude-sonnet-4-5' },
      { id: 'claude-opus-4-5', name: 'claude-opus-4-5' },
    ]);
    expect(args[13]).toBe('anthropic');   // apiFormat
    expect(args[14]).toBeUndefined();     // reasoningEffort (preset left it unset)
    expect(onClose).toHaveBeenCalledWith(true, 'Anthropic');
  });

  it('DeepSeek 预设的 reasoningEffort=high 透传', async () => {
    const addModel = vi.fn().mockResolvedValue(undefined);
    (window as any).starriver.addModel = addModel;
    renderCard({ preset: DEEPSEEK_PRESET });
    fireEvent.click(screen.getByText('Save'));

    await waitFor(() => expect(addModel).toHaveBeenCalledTimes(1));
    const args = addModel.mock.calls[0];
    expect(args[4]).toBe('https://api.deepseek.com');
    expect(args[7]).toBe('deepseek-chat');
    expect(args[13]).toBe('openai-chat');
    expect(args[14]).toBe('high');
  });

  it('高级参数手工值解析后透传（maxTokens/contextWindow/temperature）', async () => {
    const addModel = vi.fn().mockResolvedValue(undefined);
    (window as any).starriver.addModel = addModel;
    renderCard({ preset: DEEPSEEK_PRESET });

    fireEvent.change(screen.getByLabelText('Max Tokens'), { target: { value: '4096' } });
    fireEvent.change(screen.getByLabelText('Context Window'), { target: { value: '65536' } });
    fireEvent.change(screen.getByLabelText('Temperature'), { target: { value: '0.2' } });
    fireEvent.click(screen.getByText('Save'));

    await waitFor(() => expect(addModel).toHaveBeenCalledTimes(1));
    const args = addModel.mock.calls[0];
    expect(args[9]).toBeUndefined(); // concurrency untouched
    expect(args[10]).toBe(4096);
    expect(args[11]).toBe(65536);
    expect(args[12]).toBe(0.2);
  });

  it('云端 baseUrl 必填：清空后 Save 禁用且不调用 addModel', () => {
    const addModel = vi.fn().mockResolvedValue(undefined);
    (window as any).starriver.addModel = addModel;
    renderCard({ preset: DEEPSEEK_PRESET });

    fireEvent.change(screen.getByLabelText('Base URL'), { target: { value: '' } });
    const save = screen.getByText('Save') as HTMLButtonElement;
    expect(save.disabled).toBe(true);

    fireEvent.click(save);
    expect(addModel).not.toHaveBeenCalled();
  });

  it('目录行校验门控：追加空行使 Save 禁用并按位置提示', () => {
    const addModel = vi.fn().mockResolvedValue(undefined);
    (window as any).starriver.addModel = addModel;
    renderCard({ preset: DEEPSEEK_PRESET });

    fireEvent.click(screen.getByText('＋ Add Model'));
    const save = screen.getByText('Save') as HTMLButtonElement;
    expect(save.disabled).toBe(true);
    expect(screen.getByText('Model 3: Model ID is required.')).toBeTruthy();
    expect(addModel).not.toHaveBeenCalled();

    // Restoring the default model (dropping unfilled rows) restores saveability
    fireEvent.click(screen.getByText('Restore defaults'));
    expect(((screen.getByText('Save') as HTMLButtonElement)).disabled).toBe(false);
  });
});

describe('ProviderEditorCard — 本地引擎（Ollama）', () => {
  it('无密钥字段，展示模型路径；探测 engine=ollama', async () => {
    const fetchModels = vi.fn().mockResolvedValue({ ok: true, models: [{ id: 'llama3:8b', name: 'llama3:8b', contextLength: 131072 }] });
    (window as any).starriver.fetchProviderModels = fetchModels;
    renderCard({
      preset: {
        id: 'ollama', name: 'Ollama', providerType: 'local', baseUrl: 'http://localhost:11434',
        models: [], requiresApiKey: false, enabled: true,
      },
    });
    expect(screen.queryByLabelText('API key')).toBeNull();
    expect(screen.getByLabelText('Model Path')).toBeTruthy();

    fireEvent.click(screen.getByText('Fetch available models'));
    await waitFor(() => expect(fetchModels).toHaveBeenCalledWith('http://localhost:11434', undefined, 'ollama'));
    // Candidate checkbox popover appears with new models checked by default (a Vision checkbox also exists on the page; take the checked one)
    await waitFor(() => expect(screen.getByText('Choose models to add')).toBeTruthy());
    const checked = screen.getAllByRole('checkbox').find((c) => (c as HTMLInputElement).checked) as HTMLInputElement;
    expect(checked?.checked).toBe(true);
    fireEvent.click(screen.getByText('Add selected'));

    const addModel = (window as any).starriver.addModel as vi.Mock;
    fireEvent.click(screen.getByText('Save'));
    await waitFor(() => expect(addModel).toHaveBeenCalledTimes(1));
    const args = addModel.mock.calls[0];
    expect(args[2]).toBe('local');
    expect(args[3]).toBe('ollama');
    expect(JSON.parse(args[8])).toEqual([{ id: 'llama3:8b', name: 'llama3:8b', contextLength: 131072 }]);
    expect(args[7]).toBe('llama3:8b'); // the first adopted model becomes the default
  });
});

// WI-148: GLM Coding Plan - CN — switching the wire protocol jumps the base
// URL to the matching Zhipu endpoint (while the URL stays in the GLM family).
const GLM_PRESET = {
  id: 'zhipu', name: 'GLM Coding Plan - CN', providerType: 'cloud',
  baseUrl: 'https://open.bigmodel.cn/api/coding/paas/v4',
  models: ['glm-4-plus', 'glm-4-flash'], requiresApiKey: true, enabled: true,
  apiFormat: 'openai-chat',
};

describe('ProviderEditorCard — GLM Coding Plan protocol → base URL 联动', () => {
  it('switching the protocol jumps the GLM base URL along', async () => {
    renderCard({ preset: GLM_PRESET });
    const format = await screen.findByLabelText('API Format');
    const url = () => (screen.getByLabelText('Base URL') as HTMLInputElement).value;

    expect(url()).toBe('https://open.bigmodel.cn/api/coding/paas/v4');
    fireEvent.change(format, { target: { value: 'anthropic' } });
    expect(url()).toBe('https://open.bigmodel.cn/api/anthropic');
    fireEvent.change(format, { target: { value: 'openai-responses' } });
    expect(url()).toBe('https://open.bigmodel.cn/api/v1');
    fireEvent.change(format, { target: { value: 'openai-chat' } });
    expect(url()).toBe('https://open.bigmodel.cn/api/coding/paas/v4');
  });

  it('a hand-edited base URL is never overwritten by the protocol switch', async () => {
    renderCard({ preset: GLM_PRESET });
    const format = await screen.findByLabelText('API Format');
    const urlInput = screen.getByLabelText('Base URL') as HTMLInputElement;
    fireEvent.change(urlInput, { target: { value: 'https://my-proxy.example/glm' } });
    fireEvent.change(format, { target: { value: 'anthropic' } });
    expect(urlInput.value).toBe('https://my-proxy.example/glm');
  });

  it('non-GLM providers are unaffected by the protocol switch', async () => {
    renderCard({ entry: deepseekEntry(), preset: DEEPSEEK_PRESET });
    const format = await screen.findByLabelText('API Format');
    const urlInput = screen.getByLabelText('Base URL') as HTMLInputElement;
    fireEvent.change(format, { target: { value: 'anthropic' } });
    expect(urlInput.value).toBe('https://api.deepseek.com');
  });
});
