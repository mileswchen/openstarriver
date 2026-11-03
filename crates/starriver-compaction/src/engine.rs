//! The compaction lifecycle engine: one [`compact`] call brackets the whole
//! operation (`compaction/start` … `compaction/end`), runs the tool-result
//! pruner when configured, selects the summarize range (verbatim tail +
//! tool-pair safety), makes the single summarize call through the
//! [`CompactionLlm`] port, and lands the replace checkpoint. Retries
//! (`compaction_retries`) re-run prune+condense when pressure remains.

use futures::future::BoxFuture;
use starriver_kernel::json::JsonValue;
use starriver_kernel::llm::{ContentBlock, GenerateOptions};
use starriver_kernel::session::{AppendOptions, Session, SurfaceOp};

use crate::policy::{CompactionPolicy, CompactionRoute, CompactionTrigger};
use crate::pruner::compute_prune_replacements;
use crate::{
    append_image_manifest, checkpoint_message, compacted_image_manifest, estimate_message_tokens,
    retain_split, summarize_request_message, tool_pair_safe_boundary,
};

/// Port for the one summarize model call per condensation attempt. The
/// implementation folds the streamed reply to its text; errors are strings
/// because the engine only records them into `compaction/end`.
pub trait CompactionLlm: Send + Sync {
    fn summarize(&self, options: GenerateOptions)
        -> BoxFuture<'static, Result<String, String>>;
}

fn trigger_label(trigger: CompactionTrigger) -> &'static str {
    match trigger {
        CompactionTrigger::Pressure => "pressure",
        CompactionTrigger::ContextOverflow => "context-overflow",
        CompactionTrigger::Manual => "manual",
        CompactionTrigger::EscalatedRetry => "escalated-retry",
    }
}

/// WI-206 (BUG-149): what one `compact` run actually did. The legacy
/// `Option<(u64, u64)>` return conflated "nothing needed doing" with "tried
/// and failed", and the WI-199 escalation ladder flattened both into a
/// misleading "nothing to condense" annotation — a session whose summarize
/// call returned empty text looked identical to one with nothing to
/// summarize. Consumers match the variant to report the real reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactionOutcome {
    /// The surface advanced (prune and/or summary checkpoint); (before,
    /// after) token estimates.
    Condensed {
        before: u64,
        after: u64,
        /// Prune alone relieved the trigger — no summary was needed.
        pruned_only: bool,
    },
    /// No condensation attempt was made — the run had nothing to do.
    Skipped(CompactionSkip),
    /// A condensation attempt was made and failed.
    Failed(CompactionFailure),
}

/// Why no condensation attempt was made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionSkip {
    /// Pressure trigger and the token estimate is under the threshold.
    BelowPressure,
    /// Fewer than four derived messages — nothing worth summarizing.
    TooFewMessages,
    /// The route (provider/model) is unset; a summarize call is impossible.
    RouteUnset,
}

/// Why a condensation attempt failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactionFailure {
    /// The session's surface could not be projected into messages.
    SurfaceUnreadable,
    /// Retain/boundary selection produced no summarizable range.
    NoSafeRange,
    /// The summarize call succeeded but produced no text (WI-207 guards the
    /// known cause: a reasoning model burning the output budget on thinking).
    EmptySummary,
    /// The summarize model call errored; carries the error text.
    SummarizeCall(String),
    /// The replace checkpoint could not be appended.
    CheckpointWrite,
}

impl CompactionOutcome {
    /// The (before, after) token estimates when the surface advanced — the
    /// shape of the legacy `Option<(u64, u64)>` return.
    pub fn tokens(&self) -> Option<(u64, u64)> {
        match self {
            CompactionOutcome::Condensed { before, after, .. } => Some((*before, *after)),
            _ => None,
        }
    }
}

/// WI-208 (BUG-148): summarize-request replacement for an Image block whose
/// attachment lost its `url`. Pixels aren't needed to summarize text; the
/// wire serializer would otherwise reject the WHOLE summarize call with
/// `INVALID_ATTACHMENT_REF`, leaving poisoned sessions permanently
/// uncondensable. The surface and live requests are untouched, and the
/// checkpoint's BUG-127 manifest still carries the `fileId` identity.
const UNRESOLVABLE_IMAGE_PLACEHOLDER: &str = "[image omitted from summary: attachment ref unavailable]";

fn image_url_of(attachment: &JsonValue) -> &str {
    match attachment {
        JsonValue::Object(entries) => entries
            .iter()
            .find(|(key, _)| key == "url")
            .and_then(|(_, value)| value.as_str())
            .unwrap_or_default(),
        _ => "",
    }
}

fn strip_unresolvable_images(blocks: Vec<ContentBlock>) -> Vec<ContentBlock> {
    blocks
        .into_iter()
        .map(|block| match block {
            ContentBlock::Image { attachment } if image_url_of(&attachment).is_empty() => {
                ContentBlock::Text(UNRESOLVABLE_IMAGE_PLACEHOLDER.into())
            }
            ContentBlock::ToolResult {
                call_id,
                content,
                is_error,
            } => ContentBlock::ToolResult {
                call_id,
                content: strip_unresolvable_images(content),
                is_error,
            },
            other => other,
        })
        .collect()
}

/// One compaction run: brackets the operation in `compaction/*` events,
/// runs the tool-result pruner when configured, selects the summarize range
/// (verbatim tail + tool-pair safety), makes the summarize call through the
/// [`CompactionLlm`] port, and lands the replace checkpoint. Retries
/// (`compaction_retries`) re-run prune+condense when pressure remains.
pub async fn compact(
    session: &Session,
    llm: &dyn CompactionLlm,
    route: CompactionRoute,
    policy: CompactionPolicy,
    trigger: CompactionTrigger,
) -> CompactionOutcome {
    let messages = match session.derive_messages_with_seqs() {
        Ok(messages) => messages,
        Err(_) => return CompactionOutcome::Failed(CompactionFailure::SurfaceUnreadable),
    };
    if messages.len() < 4 {
        // too few to be worth compacting
        return CompactionOutcome::Skipped(CompactionSkip::TooFewMessages);
    }
    let total_tokens: u64 = messages
        .iter()
        .map(|(_, message)| estimate_message_tokens(message))
        .sum();
    // 2026-10-26 (WI-201 follow-up): the pressure trigger is capped by
    // policy.context_cap — the builtin table states true capacity now
    // (glm-5.3 = 1M), so min(ratio × window, cap) keeps big-window models
    // condensing early. Only the Pressure comparison below consults it;
    // overflow/manual/escalated bypasses are untouched.
    let threshold_tokens = ((policy.threshold_ratio * route.context_window as f64) as u64)
        .min(policy.context_cap);
    // WI-209 (BUG-150): the char-based estimate misses the real tokenizer,
    // the system prompt, and the tool schemas. The provider's last real
    // input count floors the pressure comparison — erring high compacts a
    // little early; erring low is how a 157k-token session grew unbounded.
    let observed = route.observed_input_tokens.unwrap_or(0);
    let pressure_tokens = total_tokens.max(observed);
    // Confirmed overflow condenses unconditionally; ordinary pressure still
    // compares against the threshold; manual and WI-199 escalated-retry
    // always condense.
    if trigger == CompactionTrigger::Pressure && pressure_tokens < threshold_tokens {
        return CompactionOutcome::Skipped(CompactionSkip::BelowPressure);
    }

    let mut start_entries = vec![
        ("estimatedTokens".into(), JsonValue::UInt(total_tokens)),
        ("thresholdTokens".into(), JsonValue::UInt(threshold_tokens)),
        (
            "trigger".into(),
            JsonValue::String(trigger_label(trigger).into()),
        ),
    ];
    if let Some(tokens) = route.observed_input_tokens {
        start_entries.push(("observedInputTokens".into(), JsonValue::UInt(tokens)));
    }
    let _ = session.append("compaction/start", JsonValue::Object(start_entries));

    // Prune phase (before range selection): trimming alone can clear
 // pressure and skip the summary entirely (compaction-basic).
    if let Some(prune_policy) = &policy.prune {
        let replacements = compute_prune_replacements(&messages, prune_policy);
        if !replacements.is_empty() {
            let mut entries: Vec<JsonValue> = Vec::new();
            for replacement in &replacements {
                let seq = messages[replacement.index].0;
                // `tool/result` events wrap their message payload
                // (derive_messages reads `data["message"]`).
                let Ok(inner) = replacement.replacement.to_json() else {
                    continue;
                };
                let data = JsonValue::Object(vec![("message".into(), inner)]);
                let landed = session.append_with(
                    "tool/result",
                    data,
                    AppendOptions {
                        surface_op: Some(SurfaceOp::Replace { start: seq, end: seq }),
                        source_event_seqs: Vec::new(),
                    },
                );
                if landed.is_ok() {
                    entries.push(JsonValue::Object(vec![
                        ("seq".into(), JsonValue::UInt(seq)),
                        (
                            "charsBefore".into(),
                            JsonValue::UInt(replacement.chars_before as u64),
                        ),
                        (
                            "charsAfter".into(),
                            JsonValue::UInt(replacement.chars_after as u64),
                        ),
                    ]));
                }
            }
            let _ = session.append(
                "compaction/prune",
                JsonValue::Object(vec![("pruned".into(), JsonValue::Array(entries))]),
            );
        }
        // Re-measure: pruning alone may clear ordinary pressure. The
        // observed floor scales down with the estimate (prune freed a
        // proportional slice of the real tokens too).
        if trigger == CompactionTrigger::Pressure {
            match session.derive_messages() {
                Ok(kept) => {
                    let now: u64 = kept.iter().map(estimate_message_tokens).sum();
                    let scaled_floor = observed
                        .saturating_mul(now)
                        .checked_div(pressure_tokens.max(1))
                        .unwrap_or(0);
                    if now.max(scaled_floor) < threshold_tokens {
                        append_end(
                            session,
                            true,
                            None,
                            None,
                            None,
                            Some(true), // pruned only — no summary needed
                        );
                        return CompactionOutcome::Condensed {
                            before: total_tokens,
                            after: now,
                            pruned_only: true,
                        };
                    }
                }
                Err(_) => {
                    return CompactionOutcome::Failed(CompactionFailure::SurfaceUnreadable)
                }
            }
        }
    }

    // Condense attempts: 1 + compaction_retries.
    let attempts = 1 + policy.compaction_retries;
    for attempt in 0..attempts {
        let messages = match session.derive_messages_with_seqs() {
            Ok(messages) => messages,
            Err(_) => {
                return CompactionOutcome::Failed(CompactionFailure::SurfaceUnreadable)
            }
        };
        if messages.len() < 4 {
            return CompactionOutcome::Condensed {
                before: total_tokens,
                after: 0,
                pruned_only: true,
            };
        }
        let boundary = retain_split(messages.len(), policy.retain_ratio);
        let end_idx = tool_pair_safe_boundary(&messages, boundary);
        if end_idx < 2 || end_idx >= messages.len() {
            append_end(session, false, Some("no safe compaction range"), None, None, None);
            return CompactionOutcome::Failed(CompactionFailure::NoSafeRange);
        }
        let shadowed_start = messages[0].0;
        let shadowed_end = messages[end_idx - 1].0;

        // BUG-127: the shadowed range's images lose their identity with the
        // summary — collect their file ids and carry them on the checkpoint
        // so reload_image still has a handle after compaction.
        let image_ids = compacted_image_manifest(
            &messages[..end_idx].iter().map(|(_, m)| m.clone()).collect::<Vec<_>>(),
        );

        let mut summarize_messages: Vec<starriver_kernel::llm::Message> = messages
            .into_iter()
            .map(|(_, mut message)| {
                message.content = strip_unresolvable_images(message.content);
                message
            })
            .collect();
        summarize_messages.push(summarize_request_message());

        // WI-207 (BUG-151): a reasoning model at the provider-default effort
        // can burn the whole output budget thinking and return empty text —
        // the 2026-10-14 incident saw 3/3 "empty summary" on a 157k-token
        // session. Try the route's explicit effort first (the loop only
        // sets it on models that support one), then fall back to the
        // provider default; an UNSUPPORTED_REASONING_EFFORT rejection or an
        // empty text advances to the next candidate instead of failing the
        // whole condense.
        let mut effort_candidates: Vec<Option<String>> =
            vec![route.summarize_reasoning_effort.clone(), None];
        effort_candidates.dedup();

        let mut summary = String::new();
        let mut failure: Option<CompactionFailure> = None;
        for (index, effort) in effort_candidates.iter().enumerate() {
            let summarize_options = GenerateOptions {
                provider: route.provider.clone(),
                model: route.model.clone(),
                reasoning_effort: effort.clone(),
                messages: summarize_messages.clone(),
                system: route.system.clone(),
                tools: None,
                temperature: None,
                max_tokens: Some(route.max_tokens as u64),
                stop: None,
                session_id: Some(route.session_id.clone()),
                agent_id: None,
                tool_choice: None,
                cancellation: None,
                purpose: Some(starriver_kernel::llm::CallPurpose::Compaction),
                loop_assembled: true,
            };
            match llm.summarize(summarize_options).await {
                Ok(text) if !text.trim().is_empty() => {
                    summary = text;
                    failure = None;
                    break;
                }
                Ok(_) => {
                    failure = Some(CompactionFailure::EmptySummary);
                }
                Err(error) => {
                    tracing::warn!(error = %error, "compaction summarize call failed");
                    let unsupported = error.contains("UNSUPPORTED_REASONING_EFFORT");
                    failure = Some(CompactionFailure::SummarizeCall(error));
                    if !unsupported {
                        break;
                    }
                }
            }
            let has_next = index + 1 < effort_candidates.len();
            if !has_next {
                break;
            }
        }
        if let Some(failure) = failure {
            let detail = match &failure {
                CompactionFailure::EmptySummary => "empty summary".to_string(),
                CompactionFailure::SummarizeCall(error) => error.clone(),
                _ => unreachable!("summarize phase only produces these two"),
            };
            append_end(session, false, Some(&detail), None, None, None);
            return CompactionOutcome::Failed(failure);
        }

        let _ = session.append(
            "compaction/summary",
            JsonValue::Object(vec![
                (
                    "shadowedStart".into(),
                    JsonValue::UInt(shadowed_start),
                ),
                ("shadowedEnd".into(), JsonValue::UInt(shadowed_end)),
                (
                    "summary".into(),
                    JsonValue::String(summary.clone()),
                ),
                (
                    "provider".into(),
                    JsonValue::String(route.provider.clone()),
                ),
                ("model".into(), JsonValue::String(route.model.clone())),
            ]),
        );
        let mut checkpoint = checkpoint_message(&summary);
        append_image_manifest(&mut checkpoint, &image_ids);
        let Ok(data) = checkpoint.to_json() else {
            return CompactionOutcome::Failed(CompactionFailure::CheckpointWrite);
        };
        let appended = session.append_with(
            "user/message",
            data,
            AppendOptions {
                surface_op: Some(SurfaceOp::Replace {
                    start: shadowed_start,
                    end: shadowed_end,
                }),
                source_event_seqs: Vec::new(),
            },
        );
        if appended.is_err() {
            tracing::warn!("compaction checkpoint append failed");
            append_end(
                session,
                false,
                Some("checkpoint append failed"),
                None,
                None,
                None,
            );
            return CompactionOutcome::Failed(CompactionFailure::CheckpointWrite);
        }
        let Ok(kept) = session.derive_messages() else {
            return CompactionOutcome::Failed(CompactionFailure::SurfaceUnreadable);
        };
        let after_tokens: u64 = kept.iter().map(estimate_message_tokens).sum();
        let relieved = after_tokens < threshold_tokens || trigger != CompactionTrigger::Pressure;
        if relieved || attempt + 1 == attempts {
            append_end(
                session,
                true,
                None,
                Some(shadowed_start),
                Some(shadowed_end),
                None,
            );
            return CompactionOutcome::Condensed {
                before: total_tokens,
                after: after_tokens,
                pruned_only: false,
            };
        }
        // Pressure remains above threshold: another condense round runs.
    }
    // Unreachable: `attempts` is at least 1, so the loop returns on its
    // first iteration. Kept as a safe fallback for future early-continue
    // paths.
    CompactionOutcome::Failed(CompactionFailure::NoSafeRange)
}

fn append_end(
    session: &Session,
    ok: bool,
    error: Option<&str>,
    shadowed_start: Option<u64>,
    shadowed_end: Option<u64>,
    pruned_only: Option<bool>,
) {
    let mut entries = vec![("ok".into(), JsonValue::Bool(ok))];
    if let Some(error) = error {
        entries.push((
            "error".into(),
            JsonValue::String(error.to_string()),
        ));
    }
    if let Some(start) = shadowed_start {
        entries.push(("shadowedStart".into(), JsonValue::UInt(start)));
    }
    if let Some(end) = shadowed_end {
        entries.push(("shadowedEnd".into(), JsonValue::UInt(end)));
    }
    if let Some(pruned_only) = pruned_only {
        entries.push(("prunedOnly".into(), JsonValue::Bool(pruned_only)));
    }
    let _ = session.append("compaction/end", JsonValue::Object(entries));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::PrunePolicy;
    use starriver_kernel::llm::{CallId, ContentBlock, Message, MessageId, MessageRole, MessageSource};
    use starriver_kernel::session::SessionId;

    struct StaticSummary {
        text: String,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl StaticSummary {
        fn new(text: &str) -> Self {
            Self {
                text: text.to_string(),
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    impl CompactionLlm for StaticSummary {
        fn summarize(
            &self,
            _options: GenerateOptions,
        ) -> futures::future::BoxFuture<'static, Result<String, String>> {
            use std::sync::atomic::Ordering;
            self.calls.fetch_add(1, Ordering::SeqCst);
            let text = self.text.clone();
            Box::pin(async move { Ok(text) })
        }
    }

    fn seeded_session(turns: usize) -> Session {
        let bus = cordis_free_bus();
        let session = Session::new(SessionId("s1".into()), bus);
        for i in 0..turns {
            session
                .append_with(
                    "user/message",
                    user_message(&format!("question {i}")).to_json().unwrap(),
                    AppendOptions {
                        surface_op: Some(SurfaceOp::Append),
                        ..AppendOptions::default()
                    },
                )
                .unwrap();
            // assistant/message events wrap the message payload.
            session
                .append_with(
                    "assistant/message",
                    JsonValue::Object(vec![(
                        "message".into(),
                        assistant_message(&format!("answer {i}")).to_json().unwrap(),
                    )]),
                    AppendOptions {
                        surface_op: Some(SurfaceOp::Append),
                        ..AppendOptions::default()
                    },
                )
                .unwrap();
        }
        session
    }

    /// Like [`seeded_session`], but with caller-controlled text length per
    /// message — lets tests land the estimate between the cap and the ratio
    /// line (~4 chars/token in the char-based estimator).
    fn seeded_session_with_text(turns: usize, text_len: usize) -> Session {
        let bus = cordis_free_bus();
        let session = Session::new(SessionId("s1".into()), bus);
        for i in 0..turns {
            session
                .append_with(
                    "user/message",
                    user_message(&format!("{} {i}", "q".repeat(text_len)))
                        .to_json()
                        .unwrap(),
                    AppendOptions {
                        surface_op: Some(SurfaceOp::Append),
                        ..AppendOptions::default()
                    },
                )
                .unwrap();
            session
                .append_with(
                    "assistant/message",
                    JsonValue::Object(vec![(
                        "message".into(),
                        assistant_message(&format!("{} {i}", "a".repeat(text_len)))
                            .to_json()
                            .unwrap(),
                    )]),
                    AppendOptions {
                        surface_op: Some(SurfaceOp::Append),
                        ..AppendOptions::default()
                    },
                )
                .unwrap();
        }
        session
    }

    // Session events for user/assistant messages ride `Message::to_json`.
    fn user_message(text: &str) -> starriver_kernel::llm::Message {
        starriver_kernel::llm::Message {
            id: MessageId("m".into()),
            role: MessageRole::User,
            content: vec![ContentBlock::Text(text.to_string())],
            source: MessageSource::User,
        }
    }

    fn assistant_message(text: &str) -> starriver_kernel::llm::Message {
        starriver_kernel::llm::Message {
            id: MessageId("m".into()),
            role: MessageRole::Assistant,
            content: vec![ContentBlock::Text(text.to_string())],
            source: MessageSource::Model {
                provider: "p".into(),
                model: "m".into(),
                replay_state: None,
            },
        }
    }

    fn cordis_free_bus() -> cordis_rslite::EventBus {
        cordis_rslite::EventBus::new()
    }

    fn big_tool_result(turn_index: usize) -> starriver_kernel::llm::Message {
        starriver_kernel::llm::Message {
            id: MessageId("m".into()),
            role: MessageRole::User,
            content: vec![ContentBlock::ToolResult {
                call_id: CallId(format!("c{turn_index}")),
                content: vec![ContentBlock::Text("x".repeat(40_000))],
                is_error: false,
            }],
            source: MessageSource::Tool {
                call_id: CallId(format!("c{turn_index}")),
            },
        }
    }

    fn route() -> CompactionRoute {
        CompactionRoute {
            provider: "p".into(),
            model: "m".into(),
            system: None,
            session_id: "s1".into(),
            context_window: 8_000,
            max_tokens: 1024,
            summarize_reasoning_effort: None,
            observed_input_tokens: None,
        }
    }

    /// `Trigger` naming lives on `CompactionTrigger`; this asserts the
    /// recorded labels stay stable (they land in `compaction/start`).
    #[test]
    fn trigger_labels_are_stable() {
        assert_eq!(trigger_label(CompactionTrigger::Pressure), "pressure");
        assert_eq!(
            trigger_label(CompactionTrigger::ContextOverflow),
            "context-overflow"
        );
        assert_eq!(trigger_label(CompactionTrigger::Manual), "manual");
        assert_eq!(trigger_label(CompactionTrigger::EscalatedRetry), "escalated-retry");
    }

 // WI-9 behavioral coverage for the two new -parity behaviors.

    #[test]
    fn pruning_alone_can_clear_pressure_and_skip_the_summary() {
        let session = seeded_session(2);
        session
            .append_with(
                "tool/result",
                JsonValue::Object(vec![(
                    "message".into(),
                    big_tool_result(9).to_json().unwrap(),
                )]),
                AppendOptions {
                    surface_op: Some(SurfaceOp::Append),
                    ..AppendOptions::default()
                },
            )
            .unwrap();
        let llm = StaticSummary::new("unused summary");
        let policy = CompactionPolicy {
            threshold_ratio: 0.8,
            retain_ratio: 0.16,
            prune: Some(PrunePolicy::default()),
            compaction_retries: 0,
            context_cap: u64::MAX,
        };
        let outcome = futures::executor::block_on(compact(
            &session,
            &llm,
            route(),
            policy,
            CompactionTrigger::Pressure,
        ));
        let CompactionOutcome::Condensed {
            before, after, pruned_only: true,
        } = outcome
        else {
            panic!("pruning must relieve pressure, got {outcome:?}");
        };
        assert!(after < before);
        assert_eq!(
            llm.calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "summary must be skipped when pruning clears pressure"
        );
        let log: Vec<String> = session
            .events()
            .iter()
            .map(|e| e.event_type.clone())
            .collect();
        assert!(log.contains(&"compaction/prune".to_string()), "{log:?}");
        assert!(log.contains(&"compaction/end".to_string()));
    }

    #[test]
    fn context_overflow_condenses_without_threshold() {
        // Tiny conversation, far below any pressure — a confirmed overflow
        // still condenses (threshold bypassed) and retries are not needed.
        let session = seeded_session(3);
        let llm = StaticSummary::new("## Current Work\nresumed");
        let policy = CompactionPolicy {
            threshold_ratio: 0.8,
            retain_ratio: 0.16,
            prune: None,
            compaction_retries: 0,
            context_cap: u64::MAX,
        };
        let outcome = futures::executor::block_on(compact(
            &session,
            &llm,
            route(),
            policy,
            CompactionTrigger::ContextOverflow,
        ));
        assert!(
            matches!(outcome, CompactionOutcome::Condensed { pruned_only: false, .. }),
            "overflow condenses unconditionally, got {outcome:?}"
        );
        assert_eq!(
            llm.calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "exactly one summarize call"
        );
    }

    /// 2026-10-26 (WI-201 follow-up): the pressure trigger is
    /// min(threshold_ratio × window, context_cap). This session estimates
    /// between the 1_000-token cap and the 0.8 × 8_000 = 6_400 ratio line, so
    /// a capped policy must condense where the uncapped one would skip — that
    /// contrast is what lets big-window models (builtin table now states 1M)
    /// keep condensing early.
    #[test]
    fn context_cap_condenses_below_the_ratio_line() {
        let session = seeded_session_with_text(5, 1_200);
        let llm = StaticSummary::new("## Current Work\nresumed");
        let policy = CompactionPolicy {
            threshold_ratio: 0.8,
            retain_ratio: 0.16,
            prune: None,
            compaction_retries: 0,
            context_cap: 1_000,
        };
        let outcome = futures::executor::block_on(compact(
            &session,
            &llm,
            route(),
            policy,
            CompactionTrigger::Pressure,
        ));
        assert!(
            matches!(outcome, CompactionOutcome::Condensed { pruned_only: false, .. }),
            "cap must force condensation below the ratio line, got {outcome:?}"
        );
    }

    /// Control for `context_cap_condenses_below_the_ratio_line`: the same
    /// input with the cap disabled skips as BelowPressure — proving that
    /// test's condensation came from the cap, not the estimate.
    #[test]
    fn without_the_cap_the_same_session_skips_below_pressure() {
        let session = seeded_session_with_text(5, 1_200);
        let llm = StaticSummary::new("unused");
        let policy = CompactionPolicy {
            threshold_ratio: 0.8,
            retain_ratio: 0.16,
            prune: None,
            compaction_retries: 0,
            context_cap: u64::MAX,
        };
        let outcome = futures::executor::block_on(compact(
            &session,
            &llm,
            route(),
            policy,
            CompactionTrigger::Pressure,
        ));
        assert!(
            matches!(outcome, CompactionOutcome::Skipped(CompactionSkip::BelowPressure)),
            "uncapped must skip below the ratio line, got {outcome:?}"
        );
    }

    /// A failing summarize call must surface as `Failed`, not fold into the
    /// legacy "nothing to condense" bucket (BUG-149).
    #[test]
    fn summarize_failure_reports_the_real_reason() {
        struct Failing;
        impl CompactionLlm for Failing {
            fn summarize(
                &self,
                _options: GenerateOptions,
            ) -> futures::future::BoxFuture<'static, Result<String, String>> {
                Box::pin(async { Err("model call failed: TRANSPORT: timed out".into()) })
            }
        }
        let session = seeded_session(3);
        let policy = CompactionPolicy {
            threshold_ratio: 0.8,
            retain_ratio: 0.16,
            prune: None,
            compaction_retries: 0,
            context_cap: u64::MAX,
        };
        let outcome = futures::executor::block_on(compact(
            &session,
            &Failing,
            route(),
            policy,
            CompactionTrigger::EscalatedRetry,
        ));
        assert_eq!(
            outcome,
            CompactionOutcome::Failed(CompactionFailure::SummarizeCall(
                "model call failed: TRANSPORT: timed out".into()
            ))
        );
    }

    /// WI-207 (BUG-151): empty summary with the route effort set falls back
    /// to the provider default before giving up.
    #[test]
    fn empty_summary_retries_with_the_provider_default_effort() {
        #[derive(Default)]
        struct EffortLog {
            efforts: std::sync::Mutex<Vec<Option<String>>>,
        }
        impl CompactionLlm for EffortLog {
            fn summarize(
                &self,
                options: GenerateOptions,
            ) -> futures::future::BoxFuture<'static, Result<String, String>> {
                self.efforts
                    .lock()
                    .unwrap()
                    .push(options.reasoning_effort.clone());
                // Empty while the explicit effort is set, text at default.
                let empty = options.reasoning_effort.is_some();
                let text = if empty { String::new() } else { "resumed".to_string() };
                Box::pin(async move { Ok(text) })
            }
        }
        let log = EffortLog::default();
        let session = seeded_session(3);
        let policy = CompactionPolicy {
            threshold_ratio: 0.8,
            retain_ratio: 0.16,
            prune: None,
            compaction_retries: 0,
            context_cap: u64::MAX,
        };
        let mut route = route();
        route.summarize_reasoning_effort = Some("off".into());
        let outcome = futures::executor::block_on(compact(
            &session,
            &log,
            route,
            policy,
            CompactionTrigger::EscalatedRetry,
        ));
        assert!(
            matches!(outcome, CompactionOutcome::Condensed { pruned_only: false, .. }),
            "the fallback effort must land the summary, got {outcome:?}"
        );
        assert_eq!(
            log.efforts.lock().unwrap().as_slice(),
            &[Some("off".into()), None]
        );
    }

    /// WI-208 (BUG-148): Image blocks with an empty attachment url — the
    /// poison that failed every summarize call with INVALID_ATTACHMENT_REF —
    /// are replaced by a placeholder in the summarize request only.
    #[test]
    fn unresolvable_images_are_stripped_from_the_summarize_request() {
        use starriver_kernel::json::JsonValue;
        #[derive(Default)]
        struct Capture {
            requests: std::sync::Mutex<Vec<Vec<ContentBlock>>>,
        }
        impl CompactionLlm for Capture {
            fn summarize(
                &self,
                options: GenerateOptions,
            ) -> futures::future::BoxFuture<'static, Result<String, String>> {
                let blocks: Vec<ContentBlock> = options
                    .messages
                    .iter()
                    .flat_map(|message| message.content.iter().cloned())
                    .collect();
                self.requests.lock().unwrap().push(blocks);
                Box::pin(async { Ok("resumed".into()) })
            }
        }
        let capture = Capture::default();
        let session = seeded_session(3);
        // A user message whose Image attachment carries no `url` — the
        // poisoned shape (the file id survives for the BUG-127 manifest).
        session
            .append_with(
                "user/message",
                Message {
                    id: MessageId("m-img".into()),
                    role: MessageRole::User,
                    content: vec![
                        ContentBlock::Image {
                            attachment: JsonValue::Object(vec![(
                                "fileId".into(),
                                JsonValue::String("sha256:abc".into()),
                            )]),
                        },
                        ContentBlock::Text("with a broken image".into()),
                    ],
                    source: MessageSource::User,
                }
                .to_json()
                .unwrap(),
                AppendOptions {
                    surface_op: Some(SurfaceOp::Append),
                    ..AppendOptions::default()
                },
            )
            .unwrap();
        let policy = CompactionPolicy {
            threshold_ratio: 0.8,
            retain_ratio: 0.16,
            prune: None,
            compaction_retries: 0,
            context_cap: u64::MAX,
        };
        let outcome = futures::executor::block_on(compact(
            &session,
            &capture,
            route(),
            policy,
            CompactionTrigger::EscalatedRetry,
        ));
        assert!(matches!(outcome, CompactionOutcome::Condensed { .. }));
        let requests = capture.requests.lock().unwrap();
        let blocks = requests.last().expect("one summarize request");
        assert!(
            blocks.iter().any(|block| matches!(
                block,
                ContentBlock::Text(text)
                    if text.contains("image omitted from summary")
            )),
            "the broken image becomes a placeholder in the summarize request"
        );
        assert!(
            !blocks
                .iter()
                .any(|block| matches!(block, ContentBlock::Image { .. })),
            "no Image block may reach the summarize request in this fixture"
        );
    }
}
