//! The compaction lifecycle engine: one [`compact`] call brackets the whole
//! operation (`compaction/start` … `compaction/end`), runs the tool-result
//! pruner when configured, selects the summarize range (verbatim tail +
//! tool-pair safety), makes the single summarize call through the
//! [`CompactionLlm`] port, and lands the replace checkpoint. Retries
//! (`compaction_retries`) re-run prune+condense when pressure remains.

use futures::future::BoxFuture;
use starriver_kernel::json::JsonValue;
use starriver_kernel::llm::GenerateOptions;
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
    }
}

/// One compaction run. `Some((before, after))` = the surface advanced
/// (prune, checkpoint, or both) with the estimated token delta; `None` =
/// nothing changed (below pressure, guards, or failed summarize).
pub async fn compact(
    session: &Session,
    llm: &dyn CompactionLlm,
    route: CompactionRoute,
    policy: CompactionPolicy,
    trigger: CompactionTrigger,
) -> Option<(u64, u64)> {
    let messages = match session.derive_messages_with_seqs() {
        Ok(messages) => messages,
        Err(_) => return None,
    };
    if messages.len() < 4 {
        return None; // too few to be worth compacting
    }
    let total_tokens: u64 = messages
        .iter()
        .map(|(_, message)| estimate_message_tokens(message))
        .sum();
    let threshold_tokens = (policy.threshold_ratio * route.context_window as f64) as u64;
    // Confirmed overflow condenses unconditionally; ordinary pressure still
    // compares against the threshold; manual always condenses.
    if trigger == CompactionTrigger::Pressure && total_tokens < threshold_tokens {
        return None;
    }

    let _ = session.append(
        "compaction/start",
        JsonValue::Object(vec![
            ("estimatedTokens".into(), JsonValue::UInt(total_tokens)),
            ("thresholdTokens".into(), JsonValue::UInt(threshold_tokens)),
            (
                "trigger".into(),
                JsonValue::String(trigger_label(trigger).into()),
            ),
        ]),
    );

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
        // Re-measure: pruning alone may clear ordinary pressure.
        if trigger == CompactionTrigger::Pressure {
            match session.derive_messages() {
                Ok(kept) => {
                    let now: u64 = kept.iter().map(estimate_message_tokens).sum();
                    if now < threshold_tokens {
                        append_end(
                            session,
                            true,
                            None,
                            None,
                            None,
                            Some(true), // pruned only — no summary needed
                        );
                        return Some((total_tokens, now));
                    }
                }
                Err(_) => return None,
            }
        }
    }

    // Condense attempts: 1 + compaction_retries.
    let attempts = 1 + policy.compaction_retries;
    for attempt in 0..attempts {
        let messages = match session.derive_messages_with_seqs() {
            Ok(messages) => messages,
            Err(_) => return None,
        };
        if messages.len() < 4 {
            return Some((total_tokens, 0));
        }
        let boundary = retain_split(messages.len(), policy.retain_ratio);
        let end_idx = tool_pair_safe_boundary(&messages, boundary);
        if end_idx < 2 || end_idx >= messages.len() {
            append_end(session, false, Some("no safe compaction range"), None, None, None);
            return None;
        }
        let shadowed_start = messages[0].0;
        let shadowed_end = messages[end_idx - 1].0;

        // BUG-127: the shadowed range's images lose their identity with the
        // summary — collect their file ids and carry them on the checkpoint
        // so reload_image still has a handle after compaction.
        let image_ids = compacted_image_manifest(
            &messages[..end_idx].iter().map(|(_, m)| m.clone()).collect::<Vec<_>>(),
        );

        let mut summarize_messages: Vec<starriver_kernel::llm::Message> =
            messages.into_iter().map(|(_, message)| message).collect();
        summarize_messages.push(summarize_request_message());
        let summarize_options = GenerateOptions {
            provider: route.provider.clone(),
            model: route.model.clone(),
            reasoning_effort: None,
            messages: summarize_messages,
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

        let summary = match llm.summarize(summarize_options).await {
            Ok(summary) if !summary.trim().is_empty() => summary,
            Ok(_) => {
                append_end(session, false, Some("empty summary"), None, None, None);
                return None;
            }
            Err(error) => {
                tracing::warn!(error = %error, "compaction summarize call failed");
                append_end(session, false, Some(&error), None, None, None);
                return None;
            }
        };

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
            return None;
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
            return None;
        }
        let Ok(kept) = session.derive_messages() else {
            return None;
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
            return Some((total_tokens, after_tokens));
        }
        // Pressure remains above threshold: another condense round runs.
    }
    None
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
    use starriver_kernel::llm::{CallId, ContentBlock, MessageId, MessageRole, MessageSource};
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
        };
        let outcome = futures::executor::block_on(compact(
            &session,
            &llm,
            route(),
            policy,
            CompactionTrigger::Pressure,
        ));
        let (before, after) = outcome.expect("pruning must relieve pressure");
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
        };
        let outcome = futures::executor::block_on(compact(
            &session,
            &llm,
            route(),
            policy,
            CompactionTrigger::ContextOverflow,
        ));
        assert!(outcome.is_some(), "overflow condenses unconditionally");
        assert_eq!(
            llm.calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "exactly one summarize call"
        );
    }
}
