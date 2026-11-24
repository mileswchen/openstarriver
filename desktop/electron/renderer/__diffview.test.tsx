/// <reference types="vitest/globals" />
import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent } from '@testing-library/react';
import { isDiffContent, parseDiff, DiffView } from './diffview';
import { I18nProvider } from './i18n';

const SINGLE = '--- a.txt\n+++ a.txt\n@@ -1,3 +1,4 @@\n one\n-two\n+two!\n three\n+four\n';

describe('isDiffContent', () => {
  it('detects unified diffs and rejects plain text', () => {
    expect(isDiffContent(SINGLE)).toBe(true);
    expect(isDiffContent('written: a.txt (4 bytes)')).toBe(false);
    expect(isDiffContent('plain text')).toBe(false);
  });
});

describe('parseDiff', () => {
  it('parses one file with numbered rows', () => {
    const files = parseDiff(SINGLE);
    expect(files.length).toBe(1);
    expect(files[0].path).toBe('a.txt');
    expect(files[0].add).toBe(2);
    expect(files[0].del).toBe(1);
    const rows = files[0].hunks[0].lines;
    expect(rows[0]).toMatchObject({ kind: 'ctx', newNum: 1 });
    expect(rows[1]).toMatchObject({ kind: 'del', oldNum: 2 });
    expect(rows[2]).toMatchObject({ kind: 'add', newNum: 2 });
    expect(rows[3]).toMatchObject({ kind: 'ctx', newNum: 3 });
    expect(rows[4]).toMatchObject({ kind: 'add', newNum: 4 });
  });

  it('parses multiple files', () => {
    const text = SINGLE + '\n--- b.txt\n+++ b.txt\n@@ -1,1 +1,1 @@\n-x\n+y\n';
    const files = parseDiff(text);
    expect(files.length).toBe(2);
    expect(files[1].path).toBe('b.txt');
  });

  it('returns [] for non-diff input', () => {
    expect(parseDiff('hello world')).toEqual([]);
  });
});

describe('DiffView', () => {
  it('renders layout A rows with add/del classes', () => {
    const { container } = render(<DiffView text={SINGLE} />);
    expect(container.querySelector('.diff-file-path')?.textContent).toBe('a.txt');
    expect(container.querySelector('.diff-file-stats')?.textContent).toBe('+2 −1');
    expect(container.querySelectorAll('.diff-row.add').length).toBe(2);
    expect(container.querySelectorAll('.diff-row.del').length).toBe(1);
    expect(container.querySelector('.diff-hunk-header')?.textContent).toContain('@@');
    // Add/del counts are colored (green +, red −) via dedicated stat spans.
    expect(container.querySelector('.diff-stat-add')?.textContent).toBe('+2');
    expect(container.querySelector('.diff-stat-del')?.textContent).toBe('−1');
  });

  it('truncates long diffs with a footer', () => {
    const lines = Array.from({ length: 60 }, (_, i) => `+line ${i}`).join('\n');
    const text = '--- a.txt\n+++ a.txt\n@@ -0,0 +1,60 @@\n' + lines;
    const { container } = render(<DiffView text={text} />);
    expect(container.querySelectorAll('.diff-row').length).toBe(50);
    expect(container.querySelector('.diff-trunc')?.textContent).toContain('共 60 行');
  });

  it('renders nothing for non-diff text', () => {
    const { container } = render(<DiffView text="plain" />);
    expect(container.querySelector('.diff-view')).toBeNull();
  });

  it('accepts the future split mode prop without breaking single', () => {
    const { container } = render(<DiffView text={SINGLE} mode="split" />);
    // Split mode is not implemented yet; falls back to single layout A.
    expect(container.querySelectorAll('.diff-row.add').length).toBe(2);
  });
});

describe('TurnFileSummary', () => {
  localStorage.setItem('starriver-language', 'zh');
  const DIFF = '--- crates/x.rs\n+++ crates/x.rs\n@@ -1,1 +1,2 @@\n one\n+two\n';
  const FILES = [{ path: 'crates/x.rs', add: 1, del: 0, text: DIFF }];

  it('opens the clicked file diff in the side panel (审查)', async () => {
    const { TurnFileSummary } = await import('./app');
    const onOpen = vi.fn();
    const { getByText } = render(
      <I18nProvider>
        <TurnFileSummary files={FILES} onOpen={onOpen} />
      </I18nProvider>,
    );
    fireEvent.click(getByText('crates/x.rs'));
    expect(onOpen).toHaveBeenCalledWith('crates/x.rs', DIFF);
    fireEvent.click(getByText('审查'));
    expect(onOpen).toHaveBeenCalledTimes(2);
  });

  it('renders rows without an onOpen handler (history projection)', async () => {
    const { TurnFileSummary } = await import('./app');
    const { getByText, queryByText } = render(
      <I18nProvider>
        <TurnFileSummary files={FILES} />
      </I18nProvider>,
    );
    expect(getByText('crates/x.rs')).toBeTruthy();
    expect(queryByText('审查')).toBeNull();
  });

  it('打开 sits right of 审查 and hands the row to onOpenExternal', async () => {
    const { TurnFileSummary } = await import('./app');
    const onOpen = vi.fn();
    const onOpenExternal = vi.fn();
    const { getByText } = render(
      <I18nProvider>
        <TurnFileSummary files={FILES} onOpen={onOpen} onOpenExternal={onOpenExternal} />
      </I18nProvider>,
    );
    const review = getByText('审查');
    const open = getByText('打开');
    // Open renders AFTER Review inside the same row (right-hand neighbour).
    expect(review.compareDocumentPosition(open) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    fireEvent.click(open);
    expect(onOpenExternal).toHaveBeenCalledWith('crates/x.rs', DIFF);
    expect(onOpen).not.toHaveBeenCalled();
  });

  it('no 打开 button without an onOpenExternal handler', async () => {
    const { TurnFileSummary } = await import('./app');
    const { queryByText } = render(
      <I18nProvider>
        <TurnFileSummary files={FILES} onOpen={vi.fn()} />
      </I18nProvider>,
    );
    expect(queryByText('打开')).toBeNull();
  });
});

describe('openFileWithEditor (打开: configured editor vs panel fallback)', () => {
  localStorage.setItem('starriver-language', 'zh');
  const PATH = 'crates/x.rs';
  const TEXT = 'diff text';

  it('launches the configured editor (apps.ide_command) and skips the panel', async () => {
    const { openFileWithEditor } = await import('./app');
    const openFile = vi.fn().mockResolvedValue({ ok: true });
    (window as any).starriver = {
      getSettings: vi.fn().mockResolvedValue({ apps: { ide_command: "cursor wait '{dir}'" } }),
      openFile,
    };
    const openInPanel = vi.fn();
    const showError = vi.fn();
    const where = await openFileWithEditor(PATH, TEXT, { openInPanel, showError, label: (k) => k });
    expect(where).toBe('editor');
    expect(openFile).toHaveBeenCalledWith(PATH, "cursor wait '{dir}'");
    expect(openInPanel).not.toHaveBeenCalled();
    expect(showError).not.toHaveBeenCalled();
  });

  it('no editor configured → the right-hand panel is the default editor', async () => {
    const { openFileWithEditor } = await import('./app');
    const openFile = vi.fn();
    (window as any).starriver = {
      getSettings: vi.fn().mockResolvedValue({ apps: { ide_command: null } }),
      openFile,
    };
    const openInPanel = vi.fn();
    const where = await openFileWithEditor(PATH, TEXT, { openInPanel, showError: vi.fn(), label: (k) => k });
    expect(where).toBe('panel');
    expect(openInPanel).toHaveBeenCalledWith(PATH, TEXT);
    expect(openFile).not.toHaveBeenCalled();
  });

  it('settings unavailable → panel fallback', async () => {
    const { openFileWithEditor } = await import('./app');
    (window as any).starriver = {
      getSettings: vi.fn().mockRejectedValue(new Error('bridge gone')),
    };
    const openInPanel = vi.fn();
    const where = await openFileWithEditor(PATH, TEXT, { openInPanel, showError: vi.fn(), label: (k) => k });
    expect(where).toBe('panel');
    expect(openInPanel).toHaveBeenCalledWith(PATH, TEXT);
  });

  it('a configured editor that fails to launch surfaces the error (no silent panel)', async () => {
    const { openFileWithEditor } = await import('./app');
    (window as any).starriver = {
      getSettings: vi.fn().mockResolvedValue({ apps: { ide_command: 'gone-editor {file}' } }),
      openFile: vi.fn().mockRejectedValue(new Error('spawn ENOENT')),
    };
    const openInPanel = vi.fn();
    const showError = vi.fn();
    const where = await openFileWithEditor(PATH, TEXT, { openInPanel, showError, label: (k) => k });
    expect(where).toBe('editor');
    expect(showError).toHaveBeenCalledTimes(1);
    expect(String(showError.mock.calls[0][0])).toContain('coding.openFailed');
    expect(String(showError.mock.calls[0][0])).toContain('spawn ENOENT');
    expect(openInPanel).not.toHaveBeenCalled();
  });
});

// ── Round-6 regressions (BUG-84 / BUG-85 / BUG-86 / BUG-90) ──
describe('round6 diff parsing', () => {
  it('BUG-84: deleted-file path is the --- path, not /dev/null', () => {
    const text = '--- a/gone.txt\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-hello\n-world\n';
    const files = parseDiff(text);
    expect(files.length).toBe(1);
    expect(files[0].path).toBe('gone.txt');
    expect(files[0].del).toBe(2);
  });

  it('BUG-85: blank context lines survive and advance line numbers', () => {
    const text = '--- a.txt\n+++ a.txt\n@@ -1,3 +1,3 @@\n line1\n \n line3\n';
    const files = parseDiff(text);
    const rows = files[0].hunks[0].lines;
    expect(rows.length).toBe(3, 'the blank context line is kept');
    expect(rows[2]).toMatchObject({ kind: 'ctx', oldNum: 3, newNum: 3 });
  });

  it('BUG-86: omitted-count hunks (@@ -3 +3 @@) are diffs', () => {
    expect(isDiffContent('@@ -3 +3 @@\n-old\n+new\n')).toBe(true);
  });

  it('BUG-90: a deleted "-- note" line is hunk content, not a file header', () => {
    const text = '--- a/m.md\n+++ b/m.md\n@@ -1,2 +1,1 @@\n--- note\n rest\n';
    const files = parseDiff(text);
    expect(files.length).toBe(1, 'no phantom second file');
    expect(files[0].del).toBe(1, 'the deleted comment counts');
  });
});
