//! Tool-result pruner (`compaction-tool-result-pruner` parity): once a
//! compaction trigger qualifies, over-budget tool results are durably
//! replaced by a head + "middle pruned" marker + tail form. The full
//! original stays in the session log — the replacement shadows it through a
//! `SurfaceOp::Replace` landed by the engine. Pruning makes no model call
//! and can clear pressure on its own, letting compaction skip the summary.

use starriver_kernel::llm::{ContentBlock, Message};

use crate::policy::PrunePolicy;

/// Marker joined between the retained head and tail.
pub const MIDDLE_PRUNED_MARKER: &str = "…[middle pruned]";

/// One computed replacement: the message at `index` (its surface seq) gets
/// its text collapsed to head + marker + tail; `chars_before`/`chars_after`
/// count Unicode code points for the pruning record.
pub struct PrunedReplacement {
    pub index: usize,
    pub chars_before: usize,
    pub chars_after: usize,
    pub replacement: Message,
}

fn combined_text_len(message: &Message) -> usize {
    message
        .content
        .iter()
        .map(|block| match block {
            ContentBlock::Text(text) => text.chars().count(),
            ContentBlock::ToolResult { content, .. } => content
                .iter()
                .map(|inner| match inner {
                    ContentBlock::Text(text) => text.chars().count(),
                    _ => 0,
                })
                .sum(),
            _ => 0,
        })
        .sum()
}

fn collect_text(message: &Message) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for block in &message.content {
        match block {
            ContentBlock::Text(text) => parts.push(text),
            ContentBlock::ToolResult { content, .. } => {
                for inner in content {
                    if let ContentBlock::Text(text) = inner {
                        parts.push(text);
                    }
                }
            }
            _ => {}
        }
    }
    parts.join("\n")
}

fn trimmed(text: &str, policy: &PrunePolicy) -> String {
    let chars: Vec<char> = text.chars().collect();
    let head: String = chars.iter().take(policy.head_chars).collect();
    // BUG-26: the tail window must start no earlier than the head window
    // ends — with len ≤ head+tail the two windows overlap, "pruning" then
    // grows the text and duplicates the middle.
    let tail_start = chars.len().saturating_sub(policy.tail_chars).max(policy.head_chars);
    let tail: String = chars.iter().skip(tail_start).collect();
    format!("{head}\n{MIDDLE_PRUNED_MARKER}\n{tail}")
}

fn prune_message(message: &Message, policy: &PrunePolicy) -> Message {
    let text = trimmed(&collect_text(message), policy);
    let mut cloned = message.clone();
    // All text collapses into the first prunable slot; later text slots are
    // dropped, non-text blocks keep their order (real-world results carry a
    // single text block).
    let mut new_content: Vec<ContentBlock> = Vec::with_capacity(cloned.content.len());
    let mut inserted = false;
    for block in std::mem::take(&mut cloned.content) {
        match block {
            ContentBlock::Text(_) => {
                if !inserted {
                    new_content.push(ContentBlock::Text(text.clone()));
                    inserted = true;
                }
            }
            ContentBlock::ToolResult { call_id, is_error, .. } => {
                if !inserted {
                    new_content.push(ContentBlock::ToolResult {
                        call_id,
                        content: vec![ContentBlock::Text(text.clone())],
                        is_error,
                    });
                    inserted = true;
                }
            }
            other => new_content.push(other),
        }
    }
    cloned.content = new_content;
    cloned
}

/// Compute replacements for every tool-result message whose combined text
/// exceeds the threshold (Unicode code points, semantics). Pure — the
/// engine lands each replacement as a `SurfaceOp::Replace` and records the
/// aggregate in a log-only `compaction/prune` event.
pub fn compute_prune_replacements(
    messages: &[(u64, Message)],
    policy: &PrunePolicy,
) -> Vec<PrunedReplacement> {
    let mut out = Vec::new();
    for (index, (_, message)) in messages.iter().enumerate() {
        let is_tool_result = message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolResult { .. }));
        if !is_tool_result {
            continue;
        }
        let chars_before = combined_text_len(message);
        if chars_before <= policy.threshold_chars {
            continue;
        }
        let replacement = prune_message(message, policy);
        let chars_after = combined_text_len(&replacement);
        out.push(PrunedReplacement {
            index,
            chars_before,
            chars_after,
            replacement,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use starriver_kernel::llm::{CallId, MessageId, MessageRole, MessageSource};

    fn tool_result(text: &str, is_error: bool) -> Message {
        Message {
            id: MessageId("m".into()),
            role: MessageRole::User,
            content: vec![ContentBlock::ToolResult {
                call_id: CallId("c1".into()),
                content: vec![ContentBlock::Text(text.to_string())],
                is_error,
            }],
            source: MessageSource::Tool {
                call_id: CallId("c1".into()),
            },
        }
    }

    fn policy() -> PrunePolicy {
        PrunePolicy {
            threshold_chars: 100,
            head_chars: 40,
            tail_chars: 20,
        }
    }

    #[test]
    fn over_budget_results_get_head_marker_tail() {
        let text = format!("{}{}", "a".repeat(90), "b".repeat(20));
        let messages = vec![(7u64, tool_result(&text, false))];
        let out = compute_prune_replacements(&messages, &policy());
        assert_eq!(out.len(), 1);
        let ContentBlock::ToolResult { content, .. } = &out[0].replacement.content[0] else {
            panic!("tool result shape kept");
        };
        let ContentBlock::Text(trimmed_text) = &content[0] else {
            panic!("text replacement");
        };
        assert!(trimmed_text.starts_with(&"a".repeat(40)));
        assert!(trimmed_text.contains(MIDDLE_PRUNED_MARKER));
        assert!(trimmed_text.ends_with(&"b".repeat(20)));
        assert_eq!(out[0].chars_before, 110);
        assert!(out[0].chars_after < out[0].chars_before);
    }

    #[test]
    fn within_budget_results_are_untouched() {
        let messages = vec![(1u64, tool_result(&"a".repeat(100), false))];
        assert!(compute_prune_replacements(&messages, &policy()).is_empty());
    }

    #[test]
    fn error_flag_survives_the_replacement() {
        let text = format!("{}", "e".repeat(200));
        let messages = vec![(1u64, tool_result(&text, true))];
        let out = compute_prune_replacements(&messages, &policy());
        assert_eq!(out.len(), 1);
        let ContentBlock::ToolResult { is_error, .. } = &out[0].replacement.content[0] else {
            panic!("tool result shape kept");
        };
        assert!(*is_error, "error flag preserved");
    }

    #[test]
    fn non_tool_results_are_never_pruned() {
        let messages = vec![(1u64, {
            let mut m = tool_result(&"a".repeat(500), false);
            m.content = vec![ContentBlock::Text("a".repeat(500))].into_iter().collect();
            m
        })];
        assert!(compute_prune_replacements(&messages, &policy()).is_empty());
    }

    /// BUG-26 regression: with a hand-tuned policy whose head+tail budgets
    /// meet or exceed the threshold, `take(head)` and `skip(len - tail)`
    /// overlap on short-enough results — the "pruned" replacement repeats
    /// the overlap region (and outgrows the original). The windows must be
    /// disjoint: nothing after the marker may repeat what the head kept.
    #[test]
    fn overlapping_head_tail_never_grows_or_duplicates_the_result() {
        let overlap = PrunePolicy {
            threshold_chars: 100,
            head_chars: 60,
            tail_chars: 60,
        };
        let text = format!("{}{}", "h".repeat(60), "t".repeat(50));
        assert_eq!(text.chars().count(), 110);
        let messages = vec![(1u64, tool_result(&text, false))];
        let out = compute_prune_replacements(&messages, &overlap);
        assert_eq!(out.len(), 1);
        let ContentBlock::ToolResult { content, .. } = &out[0].replacement.content[0] else {
            panic!("tool result shape kept");
        };
        let ContentBlock::Text(pruned) = &content[0] else {
            panic!("text replacement");
        };
        let after_marker = pruned
            .split(MIDDLE_PRUNED_MARKER)
            .nth(1)
            .expect("marker present");
        assert!(
            !after_marker.contains('h'),
            "the overlap region must not repeat after the marker: {pruned}"
        );
        assert!(after_marker.ends_with(&"t".repeat(50)), "tail survives");
        // Content bound: kept head+tail may never exceed the original text.
        assert!(
            pruned.chars().count()
                <= text.chars().count() + MIDDLE_PRUNED_MARKER.chars().count() + 2,
            "pruned output outgrows the original: {pruned}"
        );
        assert!(out[0].chars_before >= 110);
    }
}
