import { describe, expect, it } from 'vitest';
import { render } from '@testing-library/react';
import { I18nProvider } from './i18n';
import { TurnUsageMeta } from './app';

/** Hide only what is missing: duration and token sections are judged independently; each missing piece affects only itself. */
function msg(overrides: Record<string, unknown> = {}) {
  return {
    id: 'm1',
    role: 'assistant',
    content: 'done',
    durationMs: 83_000,
    usage: { inputTokens: 1_234, outputTokens: 567 },
    ...overrides,
  } as any;
}

function renderMeta(m: any, live: any = null) {
  return render(
    <I18nProvider>
      <TurnUsageMeta m={m} live={live} />
    </I18nProvider>,
  );
}

describe('TurnUsageMeta', () => {
  it('history: renders duration and token totals', () => {
    const { container } = renderMeta(msg());
    // WI-147 user feedback: elapsed time no longer renders; only tokens stay.
    const spans = container.querySelectorAll('.msg-meta-time');
    expect(spans.length).toBe(1);
    expect(container.textContent).not.toContain('1m 23s');
    expect(container.textContent).toContain('↑1,234');
    expect(container.textContent).toContain('↓567 tokens');
  });

  it('live accumulator wins over projected fields when forId matches', () => {
    const live = { durationMs: 5_000, input: 10, output: 3, forId: 'm1' };
    const { container } = renderMeta(msg({ durationMs: 83_000 }), live);
    expect(container.textContent).not.toContain('5s');
    expect(container.textContent).toContain('↑10');
    expect(container.textContent).not.toContain('83s');
  });

  it('live forId mismatch falls back to history fields', () => {
    const live = { durationMs: 5_000, input: 10, output: 3, forId: 'other' };
    const { container } = renderMeta(msg(), live);
    expect(container.textContent).not.toContain('1m 23s');
    expect(container.textContent).toContain('↑1,234');
  });

  it('hides the duration segment when missing but keeps tokens', () => {
    const { container } = renderMeta(msg({ durationMs: undefined }));
    const spans = container.querySelectorAll('.msg-meta-time');
    expect(spans.length).toBe(1);
    expect(container.textContent).toContain('tokens');
    expect(container.textContent).not.toContain('Took');
  });

  it('renders nothing when usage is missing (duration no longer shown)', () => {
    const { container } = renderMeta(msg({ usage: undefined }));
    const spans = container.querySelectorAll('.msg-meta-time');
    expect(spans.length).toBe(0);
    expect(container.textContent).not.toContain('tokens');
    expect(container.textContent).not.toContain('1m 23s');
  });

  it('renders nothing when neither duration nor usage exists', () => {
    const { container } = renderMeta(msg({ durationMs: undefined, usage: undefined }));
    expect(container.querySelectorAll('.msg-meta-time').length).toBe(0);
  });

  it('renders nothing for user messages', () => {
    const { container } = renderMeta(msg({ role: 'user' }));
    expect(container.querySelectorAll('.msg-meta-time').length).toBe(0);
  });
});
