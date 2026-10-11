/// <reference types="vitest/globals" />
import { describe, it, expect, vi, beforeEach } from 'vitest';
import React from 'react';
import { render, screen } from '@testing-library/react';
import { I18nProvider } from './i18n';

declare global { interface Window { starriver: any; } }

beforeEach(() => {
  Element.prototype.scrollIntoView = vi.fn();
  localStorage.setItem('starriver-language', 'en');
  window.starriver = new Proxy({}, { get: () => vi.fn().mockResolvedValue(null) });
});

const PROGRAM = JSON.stringify({
  steps: [
    { as: 'list', call: 'bash', args: { command: 'ls src' } },
    { as: 'read', call: 'read_file', args: { path: '$list.0' } },
    { call: 'boom', args: {} },
  ],
  return: '$read',
});

function ptcInput(): string {
  return JSON.stringify({ program: PROGRAM, description: 'Inspect sources in one call' });
}

function ptcOutput(): string {
  return [
    '[1/3] ok list: bash: ls src',
    '[2/3] ok read: read_file: src/main.rs 1 lines',
    '[3/3] ERR boom: exploded',
  ].join('\n');
}

describe('PtcProgramDetail — run_code 嵌套子调用渲染', () => {
  it('把 program 的 steps 渲染成子调用清单,并按日志标注 ✓/✗', async () => {
    const { PtcProgramDetail } = await import('./app');
    render(
      <I18nProvider>
        <PtcProgramDetail input={ptcInput()} output={ptcOutput()} />
      </I18nProvider>,
    );
    // The title carries the sub-call count
    expect(screen.getByText('PTC program (3 sub-calls)')).toBeTruthy();
    // label: call form + positional-args preview
    expect(screen.getByText('list: bash')).toBeTruthy();
    expect(screen.getByText('read: read_file')).toBeTruthy();
    // Steps without a label show the bare tool name
    expect(screen.getByText('boom')).toBeTruthy();
    // Per-step status marks: two ✓ and one ✗
    expect(document.querySelectorAll('.traj-ok').length).toBe(2);
    expect(document.querySelectorAll('.traj-err').length).toBe(1);
    // The failed step's row carries the error style
    expect(document.querySelectorAll('.ptc-step-err').length).toBe(1);
  });

  it('程序无法解析时回退到原始 payload 块', async () => {
    const { PtcProgramDetail } = await import('./app');
    render(
      <I18nProvider>
        <PtcProgramDetail input="not json" output="(run_code completed with no output)" />
      </I18nProvider>,
    );
    expect(document.querySelectorAll('.ptc-program').length).toBe(0);
    expect(screen.getByText('not json')).toBeTruthy();
    expect(screen.getByText('(run_code completed with no output)')).toBeTruthy();
  });
});
