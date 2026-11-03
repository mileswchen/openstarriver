//! Compaction policy surface (compaction-basic configuration parity).

/// Compaction ratio defaults (compaction-basic/config.ts parity).
pub const DEFAULT_THRESHOLD_RATIO: f64 = 0.8;
/// Fraction of recent messages preserved verbatim (retainRatio).
pub const DEFAULT_RETAIN_RATIO: f64 = 0.16;
/// 2026-10-26 (WI-201 follow-up): absolute pressure-trigger ceiling. The
/// builtin window table states true capacity (glm-5.3 = 1M), so without this
/// cap a 0.8 ratio would let context bloat to 800k tokens before condensing —
/// the exact failure the 200k-declared windows used to prevent (see the
/// settings.rs table doc for the 2026-10-14 incident). min(ratio × window,
/// cap); bypassed with the ratio for overflow/manual/escalated triggers.
pub const DEFAULT_COMPACTION_CONTEXT_CAP: u64 = 200_000;

/// Why the engine is being asked to condense. Confirmed overflow condenses
/// unconditionally (the threshold comparison is bypassed), while ordinary
/// pressure still compares the estimate against the threshold (
/// `CompactionTrigger`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionTrigger {
    /// Ordinary token pressure at the pre-step checkpoint.
    Pressure,
    /// The provider rejected the request as over the context window.
    ContextOverflow,
    /// A human (or API caller) asked for condensation; threshold bypassed.
    Manual,
    /// WI-199: the same-step retry escalation ladder fires — after 3+
    /// consecutive escalatable failures the retry is diverged from the exact
    /// context that kept failing by condensing first. Threshold bypassed.
    EscalatedRetry,
}

/// Tool-result pruner budgets (compaction-tool-result-pruner defaults:
/// 8192 → head 4096 + marker + tail 1024, in Unicode code points).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrunePolicy {
    /// Trim when a tool result's combined text exceeds this many Unicode
    /// code points.
    pub threshold_chars: usize,
    /// Leading code points retained.
    pub head_chars: usize,
    /// Trailing code points retained.
    pub tail_chars: usize,
}

impl Default for PrunePolicy {
    fn default() -> Self {
        Self {
            threshold_chars: 8192,
            head_chars: 4096,
            tail_chars: 1024,
        }
    }
}

/// One condensation attempt's configuration.
#[derive(Debug, Clone)]
pub struct CompactionPolicy {
    /// Threshold ratio against the routed context window; `0.0` = always due
    /// (manual). [`CompactionTrigger::ContextOverflow`] bypasses it entirely.
    pub threshold_ratio: f64,
    /// Fraction of recent messages preserved verbatim.
    pub retain_ratio: f64,
    /// Tool-result pruning runs after the trigger qualifies and before range
    /// selection; pruning alone may clear pressure and skip the summary.
    pub prune: Option<PrunePolicy>,
    /// Extra condensation attempts when pressure remains above the threshold
 /// after a condense (compactionRetries).
    pub compaction_retries: usize,
    /// 2026-10-26: absolute ceiling on the pressure trigger — the engine
    /// compares against min(threshold_ratio × window, this). See
    /// [`DEFAULT_COMPACTION_CONTEXT_CAP`].
    pub context_cap: u64,
}

impl CompactionPolicy {
    /// Build the policy from kernel tuning knobs (all `Option`; unset fields
 /// keep the defaults). Pruning is enabled whenever the tuning allows
    /// it — the engine only runs it after a trigger qualifies.
    pub fn from_tuning(
        threshold_ratio: f64,
        retain_ratio: f64,
        tuning: Option<&starriver_kernel::agent::CompactionTuning>,
    ) -> Self {
        let default = PrunePolicy::default();
        let (prune, compaction_retries) = match tuning {
            Some(tuning) => (
                Some(PrunePolicy {
                    threshold_chars: tuning.prune_threshold_chars.unwrap_or(default.threshold_chars),
                    head_chars: tuning.prune_head_chars.unwrap_or(default.head_chars),
                    tail_chars: tuning.prune_tail_chars.unwrap_or(default.tail_chars),
                }),
                tuning.compaction_retries.unwrap_or(1) as usize,
            ),
            None => (Some(default), 1),
        };
        let context_cap = tuning
            .and_then(|t| t.compaction_context_cap)
            .unwrap_or(DEFAULT_COMPACTION_CONTEXT_CAP);
        Self {
            threshold_ratio,
            retain_ratio,
            prune,
            compaction_retries,
            context_cap,
        }
    }
}

/// Route facts for the summarize call and the threshold comparison.
#[derive(Debug, Clone)]
pub struct CompactionRoute {
    pub provider: String,
    pub model: String,
    /// The live system prompt — the summarize request reuses the
    /// conversation's prefix (prompt-caching friendly).
    pub system: Option<String>,
    pub session_id: String,
    /// Routed context window the thresholds compare against.
    pub context_window: u64,
    /// Output cap for the summarize request.
    pub max_tokens: u32,
    /// WI-207 (BUG-151): explicit reasoning effort for the summarize call —
    /// `Some("off")` when the model resolves an effort, so a reasoning
    /// model can't burn the whole output budget thinking and return empty
    /// text. `None` keeps the provider default (models without effort
    /// support must not see an explicit effort at all).
    pub summarize_reasoning_effort: Option<String>,
    /// WI-209 (BUG-150): the provider's real input-token count from the last
    /// completed call in this session. The char-based estimate undershoots
    /// real tokenizers (plus system prompt + tool schemas it never sees);
    /// this floors the pressure comparison — erring high just compacts a
    /// little early, erring low lets the context grow unbounded.
    pub observed_input_tokens: Option<u64>,
}
