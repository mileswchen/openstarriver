/// <reference types="vitest/globals" />
import { describe, it, expect, vi, beforeEach } from 'vitest';
import React from 'react';
import { render, screen, fireEvent, waitFor } from '@testing-library/react';
import { I18nProvider } from './i18n';
import { PlanReviewCard } from './app';

// Regression contract (plan-review preset desync, 2026-10-30): the preset
// chosen on the card rides the approval decision itself (plan.reviewDecision
// `preset` param) — the host applies it atomically, so a planning-phase lock
// can never silently drop the write while the composer keeps advertising it.
// The card must surface a failed decision (stay up + show the error) instead
// of approving under the wrong mode.

beforeEach(() => {
  localStorage.setItem('starriver-language', 'en');
  (window as any).starriver = {};
});

function renderCard(currentPreset?: string, onApproveImpl?: (preset: string) => Promise<void>) {
  const onApprove = onApproveImpl ?? vi.fn().mockResolvedValue(undefined);
  const onDecide = vi.fn();
  const utils = render(
    <I18nProvider>
      <PlanReviewCard
        plan="1. Implement the decoration"
        currentPreset={currentPreset}
        onApprove={onApprove}
        onDecide={onDecide}
      />
    </I18nProvider>,
  );
  // Review step → mode step: the submit button advances when no draft text.
  fireEvent.click(utils.container.querySelector('.plan-review-submit')!);
  return { onApprove, onDecide, ...utils };
}

describe('PlanReviewCard mode preselect', () => {
  it('preselects the session’s current preset on the mode step', () => {
    const { container } = renderCard('danger-full-access');
    expect(container.querySelector('[data-step="mode"]')).toBeTruthy();
    const selected = container.querySelector('.plan-review-mode.selected');
    expect(selected).toBeTruthy();
    // Only the danger-full-access entry carries the danger class.
    expect(selected!.className).toContain('danger');
  });

  it('preselects a mid-tier preset verbatim (no silent downgrade either way)', async () => {
    const { container, onApprove } = renderCard('auto');
    expect(container.querySelector('.plan-review-mode.selected.danger')).toBeNull();
    // Submitting on the preselected non-danger mode rides the decision.
    fireEvent.click(container.querySelector('.plan-review-submit')!);
    await waitFor(() => expect(onApprove).toHaveBeenCalledWith('auto'));
  });

  it('falls back to manual when no current preset is known', () => {
    const { container } = renderCard(undefined);
    const selected = container.querySelector('.plan-review-mode.selected');
    expect(selected).toBeTruthy();
    expect(selected!.className).not.toContain('danger');
  });

  it('an unknown current preset value falls back to manual instead of a dead selection', () => {
    const { container } = renderCard('rogue-preset');
    const selected = container.querySelector('.plan-review-mode.selected');
    expect(selected).toBeTruthy();
    expect(selected!.className).not.toContain('danger');
  });
});

describe('PlanReviewCard decision contract', () => {
  it('non-danger submit calls onApprove with the chosen preset', async () => {
    const onApprove = vi.fn().mockResolvedValue(undefined);
    const { container } = renderCard(undefined, onApprove);
    fireEvent.click(screen.getByRole('radio', { name: /Auto/ }));
    fireEvent.click(container.querySelector('.plan-review-submit')!);
    await waitFor(() => expect(onApprove).toHaveBeenCalledWith('auto'));
  });

  it('danger routes through the risk step and then carries danger-full-access', async () => {
    const onApprove = vi.fn().mockResolvedValue(undefined);
    const { container } = renderCard(undefined, onApprove);
    fireEvent.click(screen.getByRole('radio', { name: /Full access/ }));
    fireEvent.click(container.querySelector('.plan-review-submit')!);
    expect(document.querySelector('[data-step="risk"]')).toBeTruthy();
    const enable = screen.getByText('Enable & implement').closest('button') as HTMLButtonElement;
    expect(enable.disabled).toBe(true);
    fireEvent.click(screen.getByLabelText('I understand the risk'));
    fireEvent.click(enable);
    await waitFor(() => expect(onApprove).toHaveBeenCalledWith('danger-full-access'));
  });

  it('a failed decision surfaces the error and keeps the card on the mode step', async () => {
    const onApprove = vi.fn().mockRejectedValue(new Error('permissions are locked by plan mode'));
    const { container } = renderCard(undefined, onApprove);
    fireEvent.click(container.querySelector('.plan-review-submit')!);
    const alert = await screen.findByRole('alert');
    expect(alert.textContent).toContain('permissions are locked by plan mode');
    // Card stays on the mode step for a retry — never a silent wrong-mode run.
    expect(container.querySelector('[data-step="mode"]')).toBeTruthy();
    expect(onApprove).toHaveBeenCalledTimes(1);
  });

  it('clears a previous error on retry', async () => {
    const onApprove = vi
      .fn()
      .mockRejectedValueOnce(new Error('first attempt failed'))
      .mockResolvedValueOnce(undefined);
    const { container } = renderCard(undefined, onApprove);
    fireEvent.click(container.querySelector('.plan-review-submit')!);
    await screen.findByRole('alert');
    fireEvent.click(container.querySelector('.plan-review-submit')!);
    await waitFor(() => expect(screen.queryByRole('alert')).toBeNull());
    expect(onApprove).toHaveBeenCalledTimes(2);
  });
});
