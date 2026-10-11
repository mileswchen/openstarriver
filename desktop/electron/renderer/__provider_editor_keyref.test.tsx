/// <reference types="vitest/globals" />
import { describe, it, expect, beforeAll, afterAll, vi } from 'vitest';
import React from 'react';
import { I18nProvider } from './i18n';
import { render, screen, waitFor, fireEvent } from '@testing-library/react';
import { ProviderEditorCard } from './app';

// ── API-key reference handling (credential parity + real-value readback) ──
// A `!file:`/`!keychain:` ref IS resolved back into the field via
// readCredential (the input is password-masked; 👁 toggles plaintext), and a
// "configured" badge shows presence via describeCredential. A blank save
// (user never touched the field) submits undefined so the host keeps the
// stored reference; an edited value submits the new key. Plaintext keys
// pre-fill directly and never trigger describe/read calls.

const originalStarriver = (window as any).starriver;

function makeEntry(apiKey: string): any {
  return {
    id: 'deepseek', name: 'DeepSeek', modelType: 'cloud', providerType: 'openai-compat',
    baseUrl: 'https://api.deepseek.com', apiKey,
    models: [{ id: 'deepseek-chat', name: 'DeepSeek Chat' }],
    defaultModel: 'deepseek-chat', apiFormat: 'openai-chat',
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

function keyInput(): HTMLInputElement {
  return screen.getByLabelText('API key') as HTMLInputElement;
}

beforeAll(() => {
  localStorage.setItem('starriver-language', 'en');
});
afterAll(() => {
  (window as any).starriver = originalStarriver;
  localStorage.removeItem('starriver-language');
});

describe('ProviderEditorCard — apiKey reference prefill', () => {
  it('describes a !file: ref and prefills the REAL key via readCredential', async () => {
    const describe = vi.fn().mockResolvedValue({ configured: true, kind: 'api-key', writable: true });
    const read = vi.fn().mockResolvedValue('sk-real-stored-key');
    (window as any).starriver = { describeCredential: describe, readCredential: read, addModel: vi.fn() };
    renderCard({ entry: makeEntry('!file:model/deepseek') });
    await waitFor(() => expect(screen.getByText('✓ configured')).toBeTruthy());
    expect(keyInput().value).toBe('sk-real-stored-key');
    expect(describe).toHaveBeenCalledWith('model/deepseek');
    expect(read).toHaveBeenCalledWith('model/deepseek');
  });

  it('shows no badge for a !keychain: ref that fails to describe/read', async () => {
    const describe = vi.fn().mockRejectedValue(new Error('no record'));
    const read = vi.fn().mockRejectedValue(new Error('no record'));
    (window as any).starriver = { describeCredential: describe, readCredential: read, addModel: vi.fn() };
    renderCard({ entry: makeEntry('!keychain:model/deepseek') });
    await waitFor(() => expect(keyInput().value).toBe(''));
    expect(screen.queryByText('✓ configured')).toBeNull();
  });

  it('pre-fills a plaintext key directly without calling describeCredential', async () => {
    const describe = vi.fn();
    (window as any).starriver = { describeCredential: describe, addModel: vi.fn() };
    renderCard({ entry: makeEntry('sk-plain-direct') });
    await waitFor(() => expect(keyInput().value).toBe('sk-plain-direct'));
    expect(describe).not.toHaveBeenCalled();
  });

  it('leaves the field empty when the ref is broken (describe/read reject)', async () => {
    (window as any).starriver = {
      describeCredential: vi.fn().mockRejectedValue(new Error('no key')),
      readCredential: vi.fn().mockRejectedValue(new Error('no key')),
      addModel: vi.fn(),
    };
    renderCard({ entry: makeEntry('!file:model/missing') });
    await waitFor(() => expect(keyInput().value).toBe(''));
  });

  it('a preset-backed card (no entry) renders an empty key field and no describe', async () => {
    const describe = vi.fn();
    const read = vi.fn();
    (window as any).starriver = { describeCredential: describe, readCredential: read, addModel: vi.fn() };
    renderCard({
      preset: {
        id: 'qwen', name: 'QWen', providerType: 'cloud',
        baseUrl: 'https://dashscope.aliyuncs.com/compatible-mode/v1',
        models: ['qwen-turbo'], requiresApiKey: true, enabled: true, apiFormat: 'openai-chat',
      },
    });
    await waitFor(() => expect(keyInput().value).toBe(''));
    expect(describe).not.toHaveBeenCalled();
    expect(read).not.toHaveBeenCalled();
  });

  it('a blank save sends apiKey undefined so the host preserves the stored reference', async () => {
    (window as any).starriver = {
      describeCredential: vi.fn().mockResolvedValue({ configured: true, kind: 'api-key', writable: true }),
      readCredential: vi.fn().mockResolvedValue('sk-stored'),
      addModel: vi.fn().mockResolvedValue(undefined),
    };
    const onClose = vi.fn();
    renderCard({ entry: makeEntry('!file:model/deepseek'), onClose });
    await waitFor(() => expect(screen.getByText('✓ configured')).toBeTruthy());

    fireEvent.click(screen.getByText('Save'));
    await waitFor(() => expect((window as any).starriver.addModel).toHaveBeenCalled());
    expect(onClose).toHaveBeenCalledWith(true, 'DeepSeek');
    const args = (window as any).starriver.addModel.mock.calls[0];
    expect(args[0]).toBe('deepseek');
    // Field was prefilled with the read-back value but never edited — the
    // submit must carry undefined so the host keeps the stored reference.
    expect(keyInput().value).toBe('sk-stored');
    expect(args[5]).toBeUndefined();
  });

  it('a local entry renders no key field at all (engine needs no credential)', () => {
    (window as any).starriver = { describeCredential: vi.fn(), addModel: vi.fn() };
    renderCard({
      entry: {
        id: 'ollama', name: 'Ollama', modelType: 'local', providerType: 'ollama',
        baseUrl: 'http://localhost:11434', apiKey: '', models: [], defaultModel: '',
      },
    });
    expect(screen.queryByLabelText('API key')).toBeNull();
  });
});
