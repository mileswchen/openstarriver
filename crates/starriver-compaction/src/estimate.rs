//! Rough token estimation for the compaction decision (bytes/4 heuristic).

use starriver_kernel::llm::{ContentBlock, Message};

/// Rough token estimate: ~4 bytes per token for English/code text.
pub fn estimate_tokens(text: &str) -> u64 {
    (text.len() as u64) / 4
}

/// Estimate one projected message's tokens: visible text, reasoning text,
/// tool-call argument payloads, and the text contents of tool results. Other
/// block kinds carry no summarizable payload.
pub fn estimate_message_tokens(message: &Message) -> u64 {
    message
        .content
        .iter()
        .map(|block| match block {
            ContentBlock::Text(text) => estimate_tokens(text),
            ContentBlock::Reasoning(text) => estimate_tokens(text),
            ContentBlock::ToolCall { arguments, .. } => {
                estimate_tokens(&arguments.to_json_string().unwrap_or_default())
            }
            ContentBlock::ToolResult { content, .. } => content
                .iter()
                .map(|inner| match inner {
                    ContentBlock::Text(text) => estimate_tokens(text),
                    _ => 0,
                })
                .sum(),
            _ => 0,
        })
        .sum()
}

/// Decide whether compaction should run.
pub fn should_compact(total_tokens: u64, context_window: u64, threshold_ratio: f64) -> bool {
    total_tokens as f64 > threshold_ratio * context_window as f64
}
