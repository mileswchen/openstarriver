/// <reference types="vitest/globals" />
// PluginsTab install-input tests: the Browse… button fills the install input
// from the native directory picker (dialog:selectDirectory); cancelling the
// dialog keeps whatever the user had typed.
import { describe, it, expect, vi, afterEach } from 'vitest';
import { render, fireEvent } from '@testing-library/react';
import { PluginsTab } from './app';

const PLACEHOLDER = 'Install from local dir or git URL';

function mockBridge(selectDirectory: ReturnType<typeof vi.fn>) {
  (window as any).starriver = {
    listPlugins: vi.fn().mockResolvedValue([]),
    marketList: vi.fn().mockResolvedValue([]),
    installPlugin: vi.fn().mockResolvedValue([]),
    uninstallPlugin: vi.fn().mockResolvedValue([]),
    selectDirectory,
  };
}

afterEach(() => {
  delete (window as any).starriver;
});

describe('PluginsTab Browse… (pick plugin directory from the UI)', () => {
  it('fills the install input with the picked directory', async () => {
    const selectDirectory = vi.fn().mockResolvedValue('/Users/me/ccprj/openstarriver/plugins/xhs-seeding');
    mockBridge(selectDirectory);
    const { getByPlaceholderText, getByText, getByDisplayValue } = render(<PluginsTab />);
    // Browse sits between the input and Install in the installed tab.
    const browse = getByText('Browse…');
    expect(browse.compareDocumentPosition(getByPlaceholderText(PLACEHOLDER)) & Node.DOCUMENT_POSITION_PRECEDING).toBeTruthy();
    fireEvent.click(browse);
    await vi.waitFor(() => {
      expect(
        (getByDisplayValue('/Users/me/ccprj/openstarriver/plugins/xhs-seeding') as HTMLInputElement).value,
      ).toBe('/Users/me/ccprj/openstarriver/plugins/xhs-seeding');
    });
    expect(selectDirectory).toHaveBeenCalledTimes(1);
  });

  it('keeps the typed path when the dialog is cancelled (null)', async () => {
    const selectDirectory = vi.fn().mockResolvedValue(null);
    mockBridge(selectDirectory);
    const { getByPlaceholderText, getByText } = render(<PluginsTab />);
    const input = getByPlaceholderText(PLACEHOLDER) as HTMLInputElement;
    fireEvent.change(input, { target: { value: 'https://github.com/me/some-plugin' } });
    fireEvent.click(getByText('Browse…'));
    await vi.waitFor(() => expect(selectDirectory).toHaveBeenCalledTimes(1));
    expect(input.value).toBe('https://github.com/me/some-plugin');
  });
});
