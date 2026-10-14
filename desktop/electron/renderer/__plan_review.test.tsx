/// <reference types="vitest/globals" />
import { describe, it, expect, vi, beforeEach } from 'vitest';
import React from 'react';
import { render, screen, fireEvent } from '@testing-library/react';
import { I18nProvider } from './i18n';
import { PlanReviewCard } from './app';

// 2026-10-14 regression (hyprland-aos cpp edits): the review card hardcoded
// the `manual` mode preselect, so re-approving a plan after a restart let a
// click-through downgrade a danger-full-access session to workspace-write —
// and the composer picker kept advertising the stale broader mode while the
// engine confined every out-of-workspace write. The card must preselect the
// session's folded preset so approval is a no-op unless the user deliberately
// picks another mode.

beforeEach(() => {
  localStorage.setItem('starriver-language', 'en');
  (window as any).starriver = {};
});

function renderCard(currentPreset?: string) {
  const onApplyPreset = vi.fn();
  const onDecide = vi.fn();
  const utils = render(
    <I18nProvider>
      <PlanReviewCard
        plan="1. Implement the decoration"
        currentPreset={currentPreset}
        onApplyPreset={onApplyPreset}
        onDecide={onDecide}
      />
    </I18nProvider>,
  );
  // Review step → mode step: the submit button advances when no draft text.
  fireEvent.click(utils.container.querySelector('.plan-review-submit')!);
  return { onApplyPreset, onDecide, ...utils };
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

  it('preselects a mid-tier preset verbatim (no silent downgrade either way)', () => {
    const { container, onApplyPreset } = renderCard('auto');
    expect(container.querySelector('.plan-review-mode.selected.danger')).toBeNull();
    // Submitting on the preselected non-danger mode applies it directly.
    fireEvent.click(container.querySelector('.plan-review-submit')!);
    expect(onApplyPreset).toHaveBeenCalledWith('auto');
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
