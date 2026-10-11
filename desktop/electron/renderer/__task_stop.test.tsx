/// <reference types="vitest/globals" />
import { describe, it, expect, vi, beforeEach } from 'vitest';
import React, { useRef } from 'react';
import { render, fireEvent } from '@testing-library/react';
import { useTaskStop } from './app';
import { I18nProvider } from './i18n';

function Harness({ initialSid = '', onNotify = () => {} }: { initialSid?: string; onNotify?: (t: string) => void }) {
  const sessionIdRef = useRef(initialSid);
  const ctl = useTaskStop(sessionIdRef, onNotify);
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
});
