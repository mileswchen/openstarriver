//! Compaction boundary selection: verbatim-tail split plus tool-pair
//! safety (`toolPairingBalancedBefore` parity).

use starriver_kernel::llm::{ContentBlock, Message};

/// Split point: the index where verbatim retention ends and summarization
/// begins. Keeps the last `retain_ratio` fraction of messages intact.
pub fn retain_split(surface_len: usize, retain_ratio: f64) -> usize {
    let retain = ((surface_len as f64) * (1.0 - retain_ratio)) as usize;
    retain.min(surface_len.saturating_sub(1))
}

/// Move the compaction boundary back until every tool result in the retained
/// tail keeps its tool call beside it (`toolPairingBalancedBefore`):
/// results always follow their calls, so a result in the tail whose call sits
/// in the summarized prefix would be stranded without its call. Returns 0
/// when no safe boundary exists.
pub fn tool_pair_safe_boundary(messages: &[(u64, Message)], mut end_idx: usize) -> usize {
    let mut call_index: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for (idx, (_, message)) in messages.iter().enumerate() {
        for block in &message.content {
            if let ContentBlock::ToolCall { id, .. } = block {
                call_index.insert(id.0.as_str(), idx);
            }
        }
    }
    // BUG-25: iterate to a fixpoint. Pulling the boundary down exposes
    // messages the previous pass never scanned; results there can strand
    // against calls even earlier. end_idx only decreases, so this
    // terminates (bounded by the message count).
    loop {
        let mut moved = false;
        for (_, message) in messages.iter().skip(end_idx) {
            for block in &message.content {
                if let ContentBlock::ToolResult { call_id, .. } = block {
                    if let Some(call_idx) = call_index.get(call_id.0.as_str()) {
                        if *call_idx < end_idx {
                            end_idx = *call_idx;
                            moved = true;
                        }
                    }
                }
            }
        }
        if !moved {
            break;
        }
    }
    end_idx
}

#[cfg(test)]
mod tests {
    use super::*;
    use starriver_kernel::llm::{CallId, ContentBlock, MessageId, MessageRole, MessageSource};

    fn text(id: &str) -> Message {
        Message {
            id: MessageId(id.into()),
            role: MessageRole::User,
            content: vec![ContentBlock::Text(id.into())],
            source: MessageSource::User,
        }
    }

    fn call(id: &str) -> Message {
        Message {
            id: MessageId(format!("call-{id}")),
            role: MessageRole::Assistant,
            content: vec![ContentBlock::ToolCall {
                id: CallId(id.into()),
                name: Some("bash".into()),
                arguments: starriver_kernel::json::JsonValue::Null,
            }],
            source: MessageSource::Model {
                provider: "p".into(),
                model: "m".into(),
                replay_state: None,
            },
        }
    }

    fn result(call_id: &str) -> Message {
        Message {
            id: MessageId(format!("result-{call_id}")),
            role: MessageRole::User,
            content: vec![ContentBlock::ToolResult {
                call_id: CallId(call_id.into()),
                content: vec![ContentBlock::Text(format!("out of {call_id}"))],
                is_error: false,
            }],
            source: MessageSource::Tool {
                call_id: CallId(call_id.into()),
            },
        }
    }

    #[test]
    fn boundary_pulls_back_to_the_earliest_stranded_call() {
        // Tail [5..8): result B (call at 1) strands → boundary must reach 1.
        let messages = vec![
            (0u64, text("u0")),
            (1u64, call("B")),
            (2u64, text("u2")),
            (3u64, text("u3")),
            (4u64, text("u4")),
            (5u64, result("B")),
            (6u64, text("u6")),
            (7u64, text("u7")),
        ];
        assert_eq!(tool_pair_safe_boundary(&messages, 5), 1);
    }

    /// BUG-25 regression: pulling the boundary back exposes messages that the
    /// original single pass never scanned ([new_end, initial_end)) — results
    /// there can strand against calls even earlier. The function's contract
    /// ("until every tool result in the retained tail keeps its tool call
    /// beside it") requires rescanning to a fixpoint; the one-pass loop
    /// returned 3 here, stranding result B (call at 1) in the retained tail.
    #[test]
    fn boundary_rescans_the_newly_exposed_region() {
        let messages = vec![
            (0u64, text("u0")),
            (1u64, call("B")),
            (2u64, text("u2")),
            (3u64, call("A")),
            (4u64, result("B")),
            (5u64, text("u5")),
            (6u64, result("A")),
            (7u64, text("u7")),
        ];
        assert_eq!(
            tool_pair_safe_boundary(&messages, 6),
            1,
            "result B (call at 1) sits in the newly exposed region and must pull the boundary to its call"
        );
    }

    #[test]
    fn retain_split_keeps_the_configured_tail_fraction() {
        assert_eq!(retain_split(10, 0.16), 8);
        assert_eq!(retain_split(4, 0.16), 3);
        assert_eq!(retain_split(1, 0.5), 0);
        assert_eq!(retain_split(0, 0.5), 0);
    }
}

