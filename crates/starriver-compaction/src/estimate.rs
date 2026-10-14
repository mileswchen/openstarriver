//! Rough token estimation for the compaction decision.

use starriver_kernel::llm::{ContentBlock, Message};

/// Rough token estimate. ASCII/code text keeps the ~4 bytes-per-token
/// heuristic; non-ASCII script (CJK in particular) counts ~1 token per char.
/// UTF-8 CJK is 3 bytes/char, so the bytes/4 rule alone underestimates
/// Chinese-heavy sessions by 2.5×+ (BUG-150: a session the provider measured
/// at 157,622 real input tokens estimated 64k here and never crossed the
/// 160k pressure threshold — compaction never fired and the context grew
/// unbounded).
pub fn estimate_tokens(text: &str) -> u64 {
    let mut ascii_bytes: u64 = 0;
    let mut wide_chars: u64 = 0;
    for ch in text.chars() {
        if (ch as u32) < 0x80 {
            ascii_bytes += 1;
        } else {
            wide_chars += 1;
        }
    }
    ascii_bytes / 4 + wide_chars
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_keeps_the_bytes_over_four_heuristic() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcdefgh"), 2);
    }

    /// BUG-150: CJK no longer rides the bytes/4 rule (3 bytes/char → 0.75
    /// tokens/char, a ~2.5× underestimate vs real GLM tokenization).
    #[test]
    fn cjk_counts_about_one_token_per_char() {
        // 12 CJK chars = 36 UTF-8 bytes → the old rule said 9.
        assert_eq!(estimate_tokens("正在编写实现代码所有模式"), 12);
        // Mixed text: ASCII part on bytes/4, CJK part per char.
        assert_eq!(estimate_tokens("使用 inline const 常量"), 3 + 4);
    }
}
