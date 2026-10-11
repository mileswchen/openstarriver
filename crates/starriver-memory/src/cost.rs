// Cost accounting for the memory pipeline's LLM calls (title/summary
// extraction — docs/36 §12.1 risk item).
//
// Every auto extraction call charges its (estimated) prompt + completion
// tokens against a session-level cumulative budget. Once the budget is
// exhausted, AUTO memory stops (silent skip + warn log — memory is a bonus,
// never a dependency) while MANUAL paths (the `memory.ingest` sweep, the
// memory-server MCP write) keep working unmetered-gate-wise: their usage is
// still recorded for the panel, but never denied.
//
// Token counts are ESTIMATES (~4 chars/token, same heuristic `render_inject`
// uses) because the `ExtractLlm` seam is plain-text in / plain-text out; when
// a provider reports real usage the host can `charge()` those numbers instead.
//
// Limit configuration (deliberately NOT a host-settings struct field — that
// file belongs to another workstream): the host reads `TokenBudget::from_env()`
// once per process (session) and hands it to `Extractor::with_budget`.

use std::sync::atomic::{AtomicU64, Ordering};

/// Env var overriding the session-level auto-memory token budget.
/// Absent → `DEFAULT_SESSION_TOKEN_BUDGET`; `0` → unlimited.
pub const TOKEN_BUDGET_ENV: &str = "STARRIVER_MEMORY_TOKEN_BUDGET";

/// Default cumulative cap on AUTO memory LLM usage per session (~12 extraction
/// calls at the current 60k-char conversation + 2048-token reply bound).
pub const DEFAULT_SESSION_TOKEN_BUDGET: u64 = 200_000;

/// Rough token estimate: ~4 characters per token, rounded up. Zero chars → 0.
pub fn estimate_tokens(chars: usize) -> u64 {
    chars.div_ceil(4) as u64
}

/// Which pipeline a metered LLM call belongs to. Auto = session-exit
/// background extraction (budget-gated); Manual = user-triggered sweeps and
/// writes (recorded, never denied).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CostMode {
    Auto,
    Manual,
}

/// Snapshot returned once the budget has been blown — the payload for the
/// host-side "memory cost limit reached" event/alarm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExceededInfo {
    pub used: u64,
    pub limit: u64,
}

/// Session-level cumulative meter for memory-pipeline LLM usage.
/// Interior-mutability (`AtomicU64`) so it can be shared as `Arc<TokenBudget>`
/// across concurrent extraction tasks without any lock on the read side.
#[derive(Debug)]
pub struct TokenBudget {
    /// `None` = unlimited (manual-only sessions, or explicit opt-out).
    limit: Option<u64>,
    used: AtomicU64,
    calls: AtomicU64,
    denied_calls: AtomicU64,
}

impl TokenBudget {
    pub fn unlimited() -> Self {
        Self {
            limit: None,
            used: AtomicU64::new(0),
            calls: AtomicU64::new(0),
            denied_calls: AtomicU64::new(0),
        }
    }

    pub fn with_limit(limit: u64) -> Self {
        Self {
            limit: Some(limit),
            ..Self::unlimited()
        }
    }

    /// Session budget from the environment: absent → default cap, `0` →
    /// unlimited, parse failure → default cap (fail-safe, never blocks boot).
    pub fn from_env() -> Self {
        match std::env::var(TOKEN_BUDGET_ENV) {
            Ok(v) => match v.trim().parse::<u64>() {
                Ok(0) => Self::unlimited(),
                Ok(n) => Self::with_limit(n),
                Err(_) => Self::with_limit(DEFAULT_SESSION_TOKEN_BUDGET),
            },
            Err(_) => Self::with_limit(DEFAULT_SESSION_TOKEN_BUDGET),
        }
    }

    pub fn limit(&self) -> Option<u64> {
        self.limit
    }

    pub fn used(&self) -> u64 {
        self.used.load(Ordering::Relaxed)
    }

    /// Total metered LLM calls (auto + manual).
    pub fn calls(&self) -> u64 {
        self.calls.load(Ordering::Relaxed)
    }

    /// AUTO calls refused because the budget was already exhausted.
    pub fn denied_calls(&self) -> u64 {
        self.denied_calls.load(Ordering::Relaxed)
    }

    /// May an AUTO call proceed? Checked BEFORE the LLM call; the call that
    /// crosses the line is still allowed to finish (its cost is then recorded
    /// and every subsequent auto call is denied).
    pub fn allows(&self) -> bool {
        self.limit.map_or(true, |limit| self.used() < limit)
    }

    /// Record one completed call's usage (both auto and manual). Returns the
    /// post-charge snapshot; `Some(_)` from `exceeded()` afterwards means the
    /// event/alarm should fire.
    pub fn charge(&self, prompt_tokens: u64, completion_tokens: u64) {
        let total = prompt_tokens.saturating_add(completion_tokens);
        // CAS loop so the cumulative counter saturates instead of wrapping if
        // a provider ever reports absurd usage numbers.
        let mut current = self.used.load(Ordering::Relaxed);
        loop {
            let new = current.saturating_add(total);
            match self.used.compare_exchange_weak(
                current,
                new,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
        self.calls.fetch_add(1, Ordering::Relaxed);
        tracing::trace!(
            prompt_tokens,
            completion_tokens,
            total_used = self.used(),
            "memory llm usage recorded"
        );
    }

    /// Count one refused AUTO attempt (the alarm-side counter).
    pub fn note_denied(&self) -> u64 {
        self.denied_calls.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// `Some` once cumulative usage reached the limit — poll this (or check
    /// after each charge) to emit the host-side cost-limit event exactly once.
    pub fn exceeded(&self) -> Option<ExceededInfo> {
        let limit = self.limit?;
        let used = self.used();
        (used >= limit).then_some(ExceededInfo { used, limit })
    }

    /// JSON shape for `memory.status` / the memory panel's cost readout.
    pub fn usage_json(&self) -> serde_json::Value {
        serde_json::json!({
            "usedTokens": self.used(),
            "limitTokens": self.limit,
            "calls": self.calls(),
            "deniedCalls": self.denied_calls(),
            "exceeded": self.exceeded().is_some(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_tokens_rounds_up_per_four_chars() {
        assert_eq!(estimate_tokens(0), 0);
        assert_eq!(estimate_tokens(1), 1);
        assert_eq!(estimate_tokens(4), 1);
        assert_eq!(estimate_tokens(5), 2);
        assert_eq!(estimate_tokens(60_000), 15_000);
    }

    #[test]
    fn unlimited_budget_never_denies() {
        let b = TokenBudget::unlimited();
        for _ in 0..100 {
            b.charge(1_000, 1_000);
        }
        assert!(b.allows());
        assert_eq!(b.exceeded(), None);
        assert_eq!(b.used(), 200_000);
        assert_eq!(b.calls(), 100);
    }

    #[test]
    fn charge_accumulates_and_denies_at_limit() {
        let b = TokenBudget::with_limit(100);
        assert!(b.allows());
        b.charge(60, 39); // 99 < 100 — the call that finishes still records
        assert!(b.allows());
        assert_eq!(b.exceeded(), None);
        b.charge(1, 1); // 101 ≥ 100 — crossed the line
        assert!(!b.allows());
        assert_eq!(
            b.exceeded(),
            Some(ExceededInfo {
                used: 101,
                limit: 100
            })
        );
        let denied = b.note_denied();
        assert_eq!(denied, 1);
        assert_eq!(b.denied_calls(), 1);
    }

    #[test]
    fn charge_saturates_on_overflow() {
        let b = TokenBudget::unlimited();
        b.charge(u64::MAX, u64::MAX);
        assert_eq!(b.used(), u64::MAX);
        b.charge(1, 1);
        assert_eq!(b.used(), u64::MAX);
    }

    #[test]
    fn usage_json_shape() {
        let b = TokenBudget::with_limit(10);
        b.charge(4, 4);
        let j = b.usage_json();
        assert_eq!(j["usedTokens"], 8);
        assert_eq!(j["limitTokens"], 10);
        assert_eq!(j["calls"], 1);
        assert_eq!(j["deniedCalls"], 0);
        assert_eq!(j["exceeded"], false);
    }

    // Env-var tests touch process-global state; the env name is unique to this
    // module so parallel tests cannot race on it.
    #[test]
    fn from_env_parses_override_and_zero() {
        std::env::set_var(TOKEN_BUDGET_ENV, "1234");
        assert_eq!(TokenBudget::from_env().limit(), Some(1234));
        std::env::set_var(TOKEN_BUDGET_ENV, "0");
        assert_eq!(TokenBudget::from_env().limit(), None);
        std::env::set_var(TOKEN_BUDGET_ENV, "not-a-number");
        assert_eq!(
            TokenBudget::from_env().limit(),
            Some(DEFAULT_SESSION_TOKEN_BUDGET)
        );
        std::env::remove_var(TOKEN_BUDGET_ENV);
        assert_eq!(
            TokenBudget::from_env().limit(),
            Some(DEFAULT_SESSION_TOKEN_BUDGET)
        );
    }
}
