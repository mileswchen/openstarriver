/// <reference types="vitest/globals" />
// WI-67 user-questions desktop slice: a `questionAsked` event renders the
// question card; an option button (or free text) calls respondQuestion and
// clears the card.

import { describe, it, expect, vi, beforeEach } from 'vitest';
import React from 'react';
import { render, screen, fireEvent, waitFor } from '@testing-library/react';
import { I18nProvider } from './i18n';

declare global { interface Window { starriver: any; } }

const respondQuestion = vi.fn().mockResolvedValue(undefined);

function baseMock() {
  window.starriver = {
    listWorkspaces: vi.fn().mockResolvedValue([
      { id: 'w1', path: '/tmp/demo', title: 'demo', createdAt: '', sessionIds: [] },
    ]),
    getActiveWorkspace: vi.fn().mockResolvedValue(null),
    listSessions: vi.fn().mockResolvedValue([]),
    listSkills: vi.fn().mockResolvedValue([]),
    listMcpServers: vi.fn().mockResolvedValue([]),
    listModels: vi.fn().mockResolvedValue([]),
    listAgentModelBindings: vi.fn().mockResolvedValue([]),
    listArchivedSessions: vi.fn().mockResolvedValue([]),
    getSessionMessages: vi.fn().mockResolvedValue([]),
    getSessionTrace: vi.fn().mockResolvedValue({ entries: [] }),
    getAgentMode: vi.fn().mockResolvedValue('standard'),
    getWorktreeStatus: vi.fn().mockResolvedValue({ isRepo: false, branch: null, isWorktree: false, dirty: false }),
    readWorkspaceInstructions: vi.fn().mockResolvedValue(null),
    setAgentMode: vi.fn().mockResolvedValue(undefined),
    setActiveWorkspace: vi.fn().mockResolvedValue(undefined),
    createWorkspace: vi.fn(),
    renameWorkspace: vi.fn().mockResolvedValue(undefined),
    deleteWorkspace: vi.fn().mockResolvedValue(undefined),
    setAgentActiveModel: vi.fn().mockResolvedValue(undefined),
    setAgentReasoningEffort: vi.fn().mockResolvedValue(undefined),
    respondQuestion,
    execute: vi.fn().mockImplementation((_task: string, _sid: string | undefined, cb: (err: any, ev: any) => void) => {
      cb(null, { type: 'TaskStarted', session_id: 's-q' });
      cb(null, {
        type: 'questionAsked',
        questionId: 'q-123',
        text: 'which database should we use?',
        options: ['postgres', 'sqlite'],
      });
    }),
  };
}

beforeEach(() => {
  Element.prototype.scrollIntoView = vi.fn();
  localStorage.setItem('starriver-language', 'en');
  vi.clearAllMocks();
  respondQuestion.mockClear();
  baseMock();
});

async function renderCodingView() {
  const { CodingView, WorkspaceProvider, WorkspaceSection } = await import('./app');
  render(<I18nProvider><WorkspaceProvider><WorkspaceSection onOpenCoding={() => {}} /><CodingView debugEnabled={false} /></WorkspaceProvider></I18nProvider>);
}

function textarea(): HTMLTextAreaElement {
  const ta = document.querySelector('textarea') as HTMLTextAreaElement | null;
  if (!ta) throw new Error('no composer textarea in DOM');
  return ta;
}

describe('CodingView question card (WI-67 user-questions)', () => {
  it('renders the asked question with option buttons', async () => {
    await renderCodingView();
    fireEvent.change(textarea(), { target: { value: 'do the thing' } });
    fireEvent.click(screen.getByRole('button', { name: 'Send' }));

    await waitFor(() => expect(screen.getByTestId('question-card')).toBeTruthy());
    expect(screen.getByText('which database should we use?')).toBeTruthy();
    expect(screen.getByTestId('question-option-postgres')).toBeTruthy();
    expect(screen.getByTestId('question-option-sqlite')).toBeTruthy();
  });

  it('clicking an option answers with the option text via respondQuestion', async () => {
    await renderCodingView();
    fireEvent.change(textarea(), { target: { value: 'do the thing' } });
    fireEvent.click(screen.getByRole('button', { name: 'Send' }));
    await waitFor(() => expect(screen.getByTestId('question-card')).toBeTruthy());

    fireEvent.click(screen.getByTestId('question-option-sqlite'));
    await waitFor(() => expect(respondQuestion).toHaveBeenCalledWith('s-q', 'q-123', 'sqlite'));
    // Answered: the card clears.
    await waitFor(() => expect(screen.queryByTestId('question-card')).toBeNull());
  });

  it('free text is submitted through the answer input', async () => {
    await renderCodingView();
    fireEvent.change(textarea(), { target: { value: 'do the thing' } });
    fireEvent.click(screen.getByRole('button', { name: 'Send' }));
    await waitFor(() => expect(screen.getByTestId('question-card')).toBeTruthy());

    const input = screen.getByLabelText('which database should we use?') as HTMLInputElement;
    fireEvent.change(input, { target: { value: 'mongodb actually' } });
    fireEvent.click(screen.getByRole('button', { name: 'Answer' }));
    await waitFor(() => expect(respondQuestion).toHaveBeenCalledWith('s-q', 'q-123', 'mongodb actually'));
  });
});
