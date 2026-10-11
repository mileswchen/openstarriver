//! Compaction policy surface (compaction-basic configuration parity).

/// Compaction ratio defaults (compaction-basic/config.ts parity).
pub const DEFAULT_THRESHOLD_RATIO: f64 = 0.8;
/// Fraction of recent messages preserved verbatim (retainRatio).
pub const DEFAULT_RETAIN_RATIO: f64 = 0.16;

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
        Self {
            threshold_ratio,
            retain_ratio,
            prune,
            compaction_retries,
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
}
