//! Compaction capability seam (`packages/compaction` parity): a loop-
//! triggered, session-operating condensation engine. The loop keeps only the
//! trigger points (pre-step pressure, request-error overflow recovery,
//! manual `force_compact`); everything else — the `compaction/*` event
//! bracket, tool-result pruning, summarize range selection, the replace
//! checkpoint, and retries — lives here and mutates the surface exclusively
//! through `SurfaceOp::Replace` (the full original always stays in the log).
//!
//! The summarize model call crosses a minimal port ([`CompactionLlm`]) so
//! this crate depends on kernel vocabulary only, never on a concrete loop
//! runtime (: the interface package does not depend on a runtime either).

pub mod boundary;
pub mod engine;
pub mod estimate;
pub mod policy;
pub mod pruner;
pub mod summarize;

pub use boundary::{retain_split, tool_pair_safe_boundary};
pub use estimate::{estimate_message_tokens, estimate_tokens, should_compact};
pub use policy::{
    CompactionPolicy, CompactionRoute, CompactionTrigger, PrunePolicy, DEFAULT_RETAIN_RATIO,
    DEFAULT_THRESHOLD_RATIO,
};
pub use pruner::{compute_prune_replacements, PrunedReplacement};
pub use summarize::{
    append_image_manifest, checkpoint_message, compacted_image_manifest, summarize_request_message,
    CHECKPOINT_PREAMBLE, COMPACTION_INSTRUCTION,
};

pub use engine::{compact, CompactionLlm};
