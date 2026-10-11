/// <reference types="vitest/globals" />
import { describe, it, expect, vi, beforeEach } from 'vitest';
import React from 'react';
import { render, screen, fireEvent } from '@testing-library/react';
import { ConfirmDeleteModal } from './app';
import { I18nProvider } from './i18n';

const WORKSPACE = { id: 'w1', path: '/tmp/my-app', title: 'My App', createdAt: '2026-08-19', sessionIds: ['s1', 's2', 's3'] };

beforeEach(() => {
  localStorage.clear(); // defaults to en
});

describe('ConfirmDeleteModal', () => {
  it('shows workspace name and session count', () => {
    render(<I18nProvider><ConfirmDeleteModal workspace={WORKSPACE} sessionCount={3} onConfirm={() => {}} onCancel={() => {}} /></I18nProvider>);
    expect(screen.getByText(/My App/)).toBeTruthy();
    expect(screen.getByText(/3 conversation/)).toBeTruthy();
    // The directory itself is not touched — the copy states sessions are
    // hidden from the sidebar but kept on disk.
    expect(screen.getByText(/hidden from the sidebar/)).toBeTruthy();
  });

  it('Confirm triggers onConfirm', () => {
    const onConfirm = vi.fn();
    render(<I18nProvider><ConfirmDeleteModal workspace={WORKSPACE} sessionCount={3} onConfirm={onConfirm} onCancel={() => {}} /></I18nProvider>);
    fireEvent.click(screen.getByText('Delete'));
    expect(onConfirm).toHaveBeenCalledTimes(1);
  });

  it('Cancel triggers onCancel', () => {
    const onCancel = vi.fn();
    render(<I18nProvider><ConfirmDeleteModal workspace={WORKSPACE} sessionCount={3} onConfirm={() => {}} onCancel={onCancel} /></I18nProvider>);
    fireEvent.click(screen.getByText('Cancel'));
    expect(onCancel).toHaveBeenCalledTimes(1);
  });
});
