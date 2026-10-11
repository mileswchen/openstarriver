/// <reference types="vitest/globals" />
import { describe, it, expect, beforeAll, afterAll, vi } from 'vitest';
import React from 'react';
import { render, screen, waitFor, fireEvent } from '@testing-library/react';
import { I18nProvider } from './i18n';
import { ModelsTab } from './app';

// Models settings page: registry ∪ catalog configured entries render as row
// cards (name + key-status dot + edit/delete); unconfigured presets feed the
// add-provider dropdown; add-custom-provider opens its own creation card; delete keeps the WI-31 two-step confirm.

const originalStarriver = (window as any).starriver;

function entry(id: string, name: string, overrides: any = {}): any {
  return {
    id, name, modelType: 'cloud', providerType: 'openai-compat', baseUrl: '', apiKey: '',
    models: [], defaultModel: id, ...overrides,
  };
}

const PRESETS = [
  { id: 'deepseek', name: 'DeepSeek', providerType: 'cloud', baseUrl: 'https://api.deepseek.com',
    models: ['deepseek-chat', 'deepseek-reasoner'], requiresApiKey: true, enabled: true, apiFormat: 'openai-chat' },
  { id: 'openai', name: 'OpenAI', providerType: 'cloud', baseUrl: 'https://api.openai.com/v1',
    models: ['gpt-4o'], requiresApiKey: true, enabled: true, apiFormat: 'openai-chat' },
  { id: 'qwen', name: 'QWen', providerType: 'cloud', baseUrl: 'https://dashscope.aliyuncs.com/compatible-mode/v1',
    models: ['qwen-turbo', 'qwen-plus'], requiresApiKey: true, enabled: true, apiFormat: 'openai-chat' },
  { id: 'ollama', name: 'Ollama', providerType: 'local', baseUrl: 'http://localhost:11434',
    models: [], requiresApiKey: false, enabled: true },
];

function directoryOf(overrides: Record<string, boolean> = {}) {
  return PRESETS.map((p) => ({
    provider: p.id, displayName: p.name, settingsNs: `llm.providers.${p.id}`,
    settingsPath: ['llm', 'providers', p.id], declared: true,
    configured: overrides[p.id] ?? false,
  }));
}

function baseStarriver() {
  return {
    listModels: vi.fn().mockResolvedValue([]),
    listModelProviders: vi.fn().mockResolvedValue(PRESETS),
    listConfigurableProviders: vi.fn().mockResolvedValue(directoryOf()),
    describeCredential: vi.fn().mockResolvedValue({ configured: true, kind: 'api-key', writable: true }),
    // Empty by default: edit-card tests assert the field stays blank (no read-back prefill).
    readCredential: vi.fn().mockResolvedValue(''),
    addModel: vi.fn().mockResolvedValue(undefined),
    removeModel: vi.fn().mockResolvedValue(undefined),
    openModelsConfig: vi.fn().mockResolvedValue({ ok: true }),
  };
}

function renderTab() {
  return render(<I18nProvider><ModelsTab /></I18nProvider>);
}

beforeAll(() => {
  localStorage.setItem('starriver-language', 'en');
  (window as any).starriver = baseStarriver();
});
afterAll(() => {
  (window as any).starriver = originalStarriver;
  localStorage.removeItem('starriver-language');
});

describe('ModelsTab — provider rows from registry ∪ directory', () => {
  it('renders configured entries as rows; dormant presets stay in the add select', async () => {
    (window as any).starriver = baseStarriver();
    (window as any).starriver.listModels = vi.fn().mockResolvedValue([
      entry('deepseek', 'DeepSeek', {
        baseUrl: 'https://api.deepseek.com', apiKey: '!file:model/deepseek',
        models: [{ id: 'deepseek-chat', name: 'deepseek-chat' }], defaultModel: 'deepseek-chat',
      }),
    ]);
    (window as any).starriver.listConfigurableProviders = vi.fn().mockResolvedValue(
      directoryOf({ deepseek: true }));

    renderTab();
    await waitFor(() => expect(screen.getByText('DeepSeek')).toBeTruthy());

    // One row card → exactly one Edit/one Delete pair.
    expect(screen.getAllByText('Edit').length).toBe(1);
    expect(screen.getAllByText('Delete').length).toBe(1);
    // Dormant presets are not rows; they wait inside the add-provider select.
    expect(screen.queryByText('＋ Add provider')).toBeTruthy();
    expect(screen.queryByText('OpenAI')).toBeNull();
  });

  it('shows the empty hint when nothing is configured', async () => {
    (window as any).starriver = baseStarriver();
    renderTab();
    await waitFor(() => expect(screen.getByText('No models configured.')).toBeTruthy());
    expect(screen.getByText('＋ Add provider')).toBeTruthy();
  });

  it('protocol models.list strips the key: hasApiKey still arms badge, row dot, and read-back prefill', async () => {
    (window as any).starriver = baseStarriver();
    (window as any).starriver.readCredential = vi.fn().mockResolvedValue('sk-glm-xyz');
    (window as any).starriver.listModels = vi.fn().mockResolvedValue([
      entry('zhipu', 'GLM Coding Plan - CN', {
        baseUrl: 'https://open.bigmodel.cn/api/coding/paas/v4',
        models: [{ id: 'glm-5.3', name: 'glm-5.3' }], defaultModel: 'glm-5.3',
        hasApiKey: true, // protocol models.list: apiKey value stripped
      }),
    ]);
    (window as any).starriver.listConfigurableProviders = vi.fn().mockResolvedValue(
      directoryOf({ zhipu: true }));

    renderTab();
    // Row dot: configured (describe confirms the record exists).
    await waitFor(() => expect(screen.getByLabelText('API key configured')).toBeTruthy());

    fireEvent.click(screen.getByText('Edit'));
    // The stored secret is read back into the (masked) field and badged.
    await waitFor(() => expect((screen.getByLabelText('API key') as HTMLInputElement).value).toBe('sk-glm-xyz'));
    expect(screen.getByText('✓ configured')).toBeTruthy();
    expect((window as any).starriver.readCredential).toHaveBeenCalledWith('model/zhipu');
  });

  it('credential dot follows describeCredential: configured / missing / none', async () => {
    (window as any).starriver = baseStarriver();
    (window as any).starriver.listModels = vi.fn().mockResolvedValue([
      entry('deepseek', 'DeepSeek', { apiKey: '!file:model/deepseek' }),
      entry('broken', 'Broken Ref', { apiKey: '!keychain:model/broken' }),
      entry('local', 'Ollama Server', { modelType: 'local', providerType: 'ollama' }),
    ]);
    (window as any).starriver.listConfigurableProviders = vi.fn().mockResolvedValue([
      { provider: 'deepseek', displayName: 'DeepSeek', settingsNs: 'llm.providers.deepseek',
        settingsPath: ['llm', 'providers', 'deepseek'], declared: true, configured: true },
      { provider: 'broken', displayName: 'Broken Ref', settingsNs: 'llm.providers.broken',
        settingsPath: ['llm', 'providers', 'broken'], declared: true, configured: true },
      { provider: 'local', displayName: 'Ollama Server', settingsNs: 'llm.providers.local',
        settingsPath: ['llm', 'providers', 'local'], declared: true, configured: true },
    ]);
    (window as any).starriver.describeCredential = vi.fn()
      .mockImplementation((key: string) => key === 'model/deepseek'
        ? Promise.resolve({ configured: true, kind: 'api-key', writable: true })
        : Promise.resolve({ configured: false, kind: 'api-key', writable: true }));

    renderTab();
    await waitFor(() => expect(screen.getByLabelText('API key configured')).toBeTruthy());
    expect(screen.getByLabelText('API key missing')).toBeTruthy();
    // A keyless entry (env auth / local engine) renders no dot at all.
    expect(screen.queryByLabelText('API key missing')).toBeTruthy();
    expect(screen.getAllByRole('img').length).toBe(2);
    // References are described — never resolved into the UI.
    expect((window as any).starriver.describeCredential).toHaveBeenCalledWith('model/deepseek');
    expect((window as any).starriver.describeCredential).toHaveBeenCalledWith('model/broken');
  });

  it('delete is armed: first click arms, second click removes', async () => {
    (window as any).starriver = baseStarriver();
    (window as any).starriver.listModels = vi.fn().mockResolvedValue([
      entry('deepseek', 'DeepSeek', { baseUrl: 'https://api.deepseek.com', apiKey: '!file:model/deepseek' }),
    ]);
    (window as any).starriver.listConfigurableProviders = vi.fn().mockResolvedValue(
      directoryOf({ deepseek: true }));

    renderTab();
    await waitFor(() => expect(screen.getByText('Delete')).toBeTruthy());

    fireEvent.click(screen.getByText('Delete'));
    // Armed: the button re-labels; removeModel is NOT called yet.
    await waitFor(() => expect(screen.getByText('Confirm delete (API key also deleted)')).toBeTruthy());
    expect((window as any).starriver.removeModel).not.toHaveBeenCalled();

    fireEvent.click(screen.getByText('Confirm delete (API key also deleted)'));
    await waitFor(() => expect((window as any).starriver.removeModel).toHaveBeenCalledWith('deepseek'));
  });

  it('Edit opens the inline editor; Save merges through models.add (blank key preserves the ref)', async () => {
    (window as any).starriver = baseStarriver();
    const deepseek = entry('deepseek', 'DeepSeek', {
      baseUrl: 'https://api.deepseek.com', apiKey: '!file:model/deepseek',
      models: [{ id: 'deepseek-chat', name: 'Chat', contextLength: 131072 }],
      defaultModel: 'deepseek-chat', contextWindow: 131072,
    });
    (window as any).starriver.listModels = vi.fn().mockResolvedValue([deepseek]);
    (window as any).starriver.listConfigurableProviders = vi.fn().mockResolvedValue(
      directoryOf({ deepseek: true }));

    renderTab();
    await waitFor(() => expect(screen.getByText('Edit')).toBeTruthy());
    fireEvent.click(screen.getByText('Edit'));

    // The stored reference never prefills the field; the badge shows instead.
    const keyInput = await screen.findByLabelText('API key') as HTMLInputElement;
    await waitFor(() => expect(screen.getByText('✓ configured')).toBeTruthy());
    expect(keyInput.value).toBe('');

    fireEvent.click(screen.getByText('Save'));
    await waitFor(() => expect((window as any).starriver.addModel).toBeTruthy());
    const args = (window as any).starriver.addModel.mock.calls[0];
    expect(args[0]).toBe('deepseek');
    expect(args[1]).toBe('DeepSeek');
    expect(args[2]).toBe('cloud');
    expect(args[3]).toBe('openai-compat');
    expect(args[4]).toBe('https://api.deepseek.com');
    // Blank key field → undefined → host preserves the stored reference.
    expect(args[5]).toBeUndefined();
    expect(args[7]).toBe('deepseek-chat');
    expect(JSON.parse(args[8])).toEqual([{ id: 'deepseek-chat', name: 'Chat', contextLength: 131072 }]);
  });

  it('custom model ID: prefilled above the key field and saved as the selector prefix', async () => {
    (window as any).starriver = baseStarriver();
    (window as any).starriver.listModels = vi.fn().mockResolvedValue([
      entry('glm', 'GLM Coding Plan - CN', {
        baseUrl: 'https://open.bigmodel.cn/api/coding/paas/v4',
        models: [{ id: 'glm-5.3', name: 'glm-5.3' }], defaultModel: 'glm-5.3',
        modelLabel: 'CN',
      }),
    ]);
    (window as any).starriver.listConfigurableProviders = vi.fn().mockResolvedValue(
      directoryOf({ glm: true }));

    renderTab();
    await waitFor(() => expect(screen.getByText('Edit')).toBeTruthy());
    fireEvent.click(screen.getByText('Edit'));

    const labelInput = await screen.findByLabelText('Custom model ID') as HTMLInputElement;
    expect(labelInput.value).toBe('CN');
    // The field sits ABOVE the API key input (screenshot parity).
    const keyInput = screen.getByLabelText('API key');
    expect(!!(labelInput.compareDocumentPosition(keyInput) & Node.DOCUMENT_POSITION_FOLLOWING)).toBe(true);

    fireEvent.click(screen.getByText('Save'));
    await waitFor(() => expect((window as any).starriver.addModel).toHaveBeenCalled());
    const args = (window as any).starriver.addModel.mock.calls[0];
    expect(args[15]).toBe('CN');
    expect(args[16]).toBeUndefined();
  });

  it('custom model ID: blank on an unlabeled provider saves undefined (name fallback)', async () => {
    (window as any).starriver = baseStarriver();
    (window as any).starriver.listModels = vi.fn().mockResolvedValue([
      entry('deepseek', 'DeepSeek', { baseUrl: 'https://api.deepseek.com' }),
    ]);
    (window as any).starriver.listConfigurableProviders = vi.fn().mockResolvedValue(
      directoryOf({ deepseek: true }));

    renderTab();
    await waitFor(() => expect(screen.getByText('Edit')).toBeTruthy());
    fireEvent.click(screen.getByText('Edit'));

    const labelInput = await screen.findByLabelText('Custom model ID') as HTMLInputElement;
    expect(labelInput.value).toBe('');

    fireEvent.click(screen.getByText('Save'));
    await waitFor(() => expect((window as any).starriver.addModel).toHaveBeenCalled());
    const args = (window as any).starriver.addModel.mock.calls[0];
    expect(args[15]).toBeUndefined();
  });

  it('add-provider card: presets selectable, Save writes the preset defaults', async () => {
    (window as any).starriver = baseStarriver();
    renderTab();
    fireEvent.click(await screen.findByText('＋ Add provider'));

    // The add card opens with the provider select; the first addable preset
    // (OpenAI) is preselected and switching re-targets the editor.
    const select = (await screen.findByLabelText('Provider')) as HTMLSelectElement;
    const options = Array.from(select.querySelectorAll('option')).map((o) => o.textContent);
    expect(options).toEqual(['DeepSeek', 'OpenAI', 'QWen', 'Ollama']);

    fireEvent.change(select, { target: { value: 'qwen' } });
    const baseUrl = await screen.findByDisplayValue('https://dashscope.aliyuncs.com/compatible-mode/v1');
    expect(baseUrl).toBeTruthy();

    fireEvent.click(screen.getByText('Save'));
    await waitFor(() => expect((window as any).starriver.addModel).toHaveBeenCalled());
    const args = (window as any).starriver.addModel.mock.calls[0];
    expect(args[0]).toBe('qwen');
    expect(args[1]).toBe('QWen');
    expect(args[4]).toBe('https://dashscope.aliyuncs.com/compatible-mode/v1');
    expect(JSON.parse(args[8])).toEqual([
      { id: 'qwen-turbo', name: 'qwen-turbo' },
      { id: 'qwen-plus', name: 'qwen-plus' },
    ]);
    expect(args[7]).toBe('qwen-turbo'); // default = first preset model
  });

  it('custom provider card: route validation gates Create, then writes an openai-compat entry', async () => {
    (window as any).starriver = baseStarriver();
    renderTab();
    fireEvent.click(await screen.findByText('＋ Add a custom provider'));

    const create = () => screen.getByText('Create provider') as HTMLButtonElement;
    expect(create().disabled).toBe(true);

    const route = screen.getByLabelText('Provider ID') as HTMLInputElement;
    fireEvent.change(route, { target: { value: 'Bad_ID' } });
    expect(screen.getByText('Start with a lowercase letter; then lowercase letters, digits, and dashes.')).toBeTruthy();

    fireEvent.change(route, { target: { value: 'acme-gateway' } });
    expect(screen.getByText('Lowercase identifier, starting with a letter, that uniquely names this provider in requests and as its credential name.')).toBeTruthy();
    // Still blocked: no base URL, no models.
    expect(create().disabled).toBe(true);
    expect(screen.getByText('A custom provider needs a base URL.')).toBeTruthy();

    fireEvent.change(screen.getByLabelText('Base URL'), { target: { value: 'https://gateway.example/v1' } });
    expect(screen.getByText('A custom provider needs at least one model.')).toBeTruthy();

    fireEvent.click(screen.getByText('＋ Add Model'));
    fireEvent.change(screen.getByLabelText('Model ID 1'), { target: { value: 'glm-x' } });
    expect(create().disabled).toBe(false);

    fireEvent.click(create());
    await waitFor(() => expect((window as any).starriver.addModel).toHaveBeenCalled());
    const args = (window as any).starriver.addModel.mock.calls[0];
    expect(args[0]).toBe('acme-gateway');
    expect(args[2]).toBe('cloud');
    expect(args[3]).toBe('openai-compat');
    expect(args[4]).toBe('https://gateway.example/v1');
    expect(args[13]).toBe('openai-chat');
  });

  it('打开配置文件 goes through the openModelsConfig IPC; failures surface as an alert', async () => {
    (window as any).starriver = baseStarriver();
    renderTab();
    fireEvent.click(await screen.findByText('Open config file'));
    await waitFor(() => expect((window as any).starriver.openModelsConfig).toHaveBeenCalled());
    expect(screen.queryByRole('alert')).toBeNull();

    (window as any).starriver.openModelsConfig = vi.fn().mockResolvedValue({ ok: false, error: 'denied' });
    fireEvent.click(screen.getByText('Open config file'));
    await waitFor(() => expect(screen.getByRole('alert').textContent).toBe('denied'));
  });
});
