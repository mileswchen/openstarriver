/// <reference types="vitest/globals" />
// WI-212: in-session links open in the system default browser. The renderer
// <a> layer intercepts http(s) clicks and routes them through the
// shell:openExternal bridge; non-http(s) hrefs are not intercepted (main's
// will-navigate guard blocks them from hijacking the window instead).
import { describe, it, expect, vi, beforeEach } from 'vitest';
import React from 'react';
import { render, fireEvent } from '@testing-library/react';

declare global { interface Window { starriver: any; } }

beforeEach(() => {
  localStorage.setItem('starriver-language', 'en');
  window.starriver = {
    openExternal: vi.fn().mockResolvedValue({ ok: true }),
  };
});

describe('WI-212 — session links route to the system browser', () => {
  it('clicking an http(s) markdown link calls openExternal with the href and prevents navigation', async () => {
    const { MarkdownContent } = await import('./app');
    const { container } = render(<MarkdownContent source={'[docs](https://example.com/guide)'} />);
    const a = container.querySelector('a') as HTMLAnchorElement;
    expect(a.getAttribute('href')).toBe('https://example.com/guide');
    expect(a.getAttribute('target')).toBe('_blank');
    expect(a.getAttribute('rel')).toContain('noopener');

    const click = new MouseEvent('click', { bubbles: true, cancelable: true });
    a.dispatchEvent(click);
    expect(click.defaultPrevented).toBe(true);
    expect(window.starriver.openExternal).toHaveBeenCalledTimes(1);
    expect(window.starriver.openExternal).toHaveBeenCalledWith('https://example.com/guide');
  });

  it('https links behave the same (whitelist is scheme-based, not host-based)', async () => {
    const { MarkdownContent } = await import('./app');
    const { container } = render(<MarkdownContent source={'[local](http://localhost:3000/api)'} />);
    const a = container.querySelector('a') as HTMLAnchorElement;
    fireEvent.click(a);
    expect(window.starriver.openExternal).toHaveBeenCalledWith('http://localhost:3000/api');
  });

  it('non-http(s) hrefs are not intercepted — no openExternal call', async () => {
    const { MarkdownContent } = await import('./app');
    const { container } = render(<MarkdownContent source={'[rel](./notes.md)'} />);
    const a = container.querySelector('a') as HTMLAnchorElement;
    expect(a.getAttribute('href')).toBe('./notes.md');
    fireEvent.click(a);
    expect(window.starriver.openExternal).not.toHaveBeenCalled();
  });

  it('javascript: urls never survive rendering (react-markdown strips the href)', async () => {
    const { MarkdownContent } = await import('./app');
    const { container } = render(<MarkdownContent source={'[evil](javascript:alert(1))'} />);
    const a = container.querySelector('a') as HTMLAnchorElement;
    // defaultUrlTransform neuters the scheme to '' rather than dropping the attr.
    expect(a?.getAttribute('href')).toBe('');
    fireEvent.click(a);
    expect(window.starriver.openExternal).not.toHaveBeenCalled();
  });
});
