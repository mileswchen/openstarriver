/// <reference types="vitest/globals" />
import { describe, it, expect, vi, beforeEach } from 'vitest';
import React, { useRef } from 'react';
import { render, fireEvent } from '@testing-library/react';
import { useTaskStop } from './app';
import { I18nProvider } from './i18n';

function Harness({ initialSid = '', onNotify = () => {}, onNotRunning }: { initialSid?: string; onNotify?: (t: string) => void; onNotRunning?: () => void }) {
  const sessionIdRef = useRef(initialSid);
  const ctl = useTaskStop(sessionIdRef, onNotify, undefined, onNotRunning);
  return (
    <div>
      <button onClick={ctl.requestStop}>request-stop</button>
      <button onClick={() => { sessionIdRef.current = 'sess-1'; ctl.onTaskStarted(); }}>task-started</button>
      <button onClick={ctl.onTerminal}>terminal</button>
      <span data-testid="stopping">{String(ctl.stopping)}</span>
    </div>
  );
}

beforeEach(() => {
  localStorage.setItem('starriver-language', 'zh');
  vi.spyOn(window, 'confirm').mockReturnValue(true);
  (window as any).starriver = { cancel: vi.fn().mockResolvedValue(true) };
});

describe('useTaskStop', () => {
  it('auto-sends cancel once TaskStarted arrives when no session id yet', async () => {
    const { getByText, getByTestId } = render(<I18nProvider><Harness /></I18nProvider>);
    fireEvent.click(getByText('request-stop'));
    expect(getByTestId('stopping').textContent).toBe('true');
    expect((window as any).starriver.cancel).not.toHaveBeenCalled();
    fireEvent.click(getByText('task-started'));
    await vi.waitFor(() => expect((window as any).starriver.cancel).toHaveBeenCalledWith('sess-1'));
    expect((window as any).starriver.cancel).toHaveBeenCalledTimes(1);
  });

  it('confirm-then-cancel with known session; cancel=false resets stopping', async () => {
    (window as any).starriver.cancel = vi.fn().mockResolvedValue(false);
    const { getByText, getByTestId } = render(<I18nProvider><Harness initialSid="sess-9" /></I18nProvider>);
    fireEvent.click(getByText('request-stop'));
    expect(window.confirm).toHaveBeenCalledWith('确定要结束当前任务吗？');
    await vi.waitFor(() => expect(getByTestId('stopping').textContent).toBe('false'));
    expect((window as any).starriver.cancel).toHaveBeenCalledWith('sess-9');
  });

  it('onTerminal resets stopping', () => {
    const { getByText, getByTestId } = render(<I18nProvider><Harness initialSid="sess-1" /></I18nProvider>);
    fireEvent.click(getByText('request-stop'));
    expect(getByTestId('stopping').textContent).toBe('true');
    fireEvent.click(getByText('terminal'));
    expect(getByTestId('stopping').textContent).toBe('false');
  });

  it('uses localized confirm text for en', () => {
    localStorage.setItem('starriver-language', 'en');
    const { getByText } = render(<I18nProvider><Harness initialSid="sess-1" /></I18nProvider>);
    fireEvent.click(getByText('request-stop'));
    expect(window.confirm).toHaveBeenCalledWith('Stop the current task?');
  });

  // 2026-10-12 report: the turn had ended long before (host log 09:02 —
  // both Stop clicks resolved "no live agent"), yet the composer stayed on
  // Stop until the user clicked 新建 task. cancel=false must surface to the
  // owning view so it can drop its stale run state and flip back to Send.
  it('cancel=false fires onNotRunning so a stale Stop state can heal', async () => {
    (window as any).starriver.cancel = vi.fn().mockResolvedValue(false);
    const onNotRunning = vi.fn();
    const { getByText, getByTestId } = render(
      <I18nProvider><Harness initialSid="sess-9" onNotRunning={onNotRunning} /></I18nProvider>
    );
    fireEvent.click(getByText('request-stop'));
    await vi.waitFor(() => expect((window as any).starriver.cancel).toHaveBeenCalledWith('sess-9'));
    await vi.waitFor(() => expect(onNotRunning).toHaveBeenCalledTimes(1));
    expect(getByTestId('stopping').textContent).toBe('false');
  });

  it('onNotRunning is NOT fired when the cancel aborts a live turn (true)', async () => {
    const onNotRunning = vi.fn();
    const { getByText } = render(
      <I18nProvider><Harness initialSid="sess-1" onNotRunning={onNotRunning} /></I18nProvider>
    );
    fireEvent.click(getByText('request-stop'));
    await vi.waitFor(() => expect((window as any).starriver.cancel).toHaveBeenCalledWith('sess-1'));
    expect(onNotRunning).not.toHaveBeenCalled();
  });
});
