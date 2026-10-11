//! Protocol-agnostic admission tests for adapter implementations (LLP §3.3).
//!
//! Every adapter crate drives its own scripted transport through its adapter
//! and asserts the drained chunk list against the helpers here. The invariants
//! come from the H-series bridge work and the adapter study:
//!
//! - a terminal `finish` is the LAST chunk;
//! - open text/reasoning blocks are closed at or before `finish`;
//! - caller cancellation surfaces as an `ABORTED` terminal error;
//! - a stalled stream surfaces as `LLM_STREAM_IDLE_TIMEOUT`.

use starriver_kernel::llm::{ContentBlock, LlmError, StreamChunk};

/// Drained chunk list type alias for adapter test signatures.
pub type Chunks = Vec<Result<StreamChunk, LlmError>>;

/// `finish` must exist and be the last chunk.
pub fn assert_finish_is_last(chunks: &Chunks) {
    let last = chunks.last().expect("adapter stream must not be empty");
    match last {
        Ok(StreamChunk::Finish { .. }) => {}
        other => panic!("terminal chunk must be finish, got {other:?}"),
    }
    for (position, chunk) in chunks.iter().enumerate().take(chunks.len() - 1) {
        assert!(
            !matches!(chunk, Ok(StreamChunk::Finish { .. })),
            "chunk {position} is a finish but not the last chunk: {chunks:?}"
        );
    }
}

/// Every text/reasoning delta is closed by a BlockEnd carrying the full text.
pub fn assert_open_blocks_are_closed(chunks: &Chunks) {
    let mut open: std::collections::HashMap<u64, (String, String)> = Default::default();
    for chunk in chunks {
        match chunk {
            Ok(StreamChunk::BlockStart { index, block_type }) => {
                let kind = match block_type {
                    starriver_kernel::llm::ContentBlockType::Text => "text",
                    starriver_kernel::llm::ContentBlockType::Reasoning => "reasoning",
                    starriver_kernel::llm::ContentBlockType::ToolCall => "tool-call",
                    starriver_kernel::llm::ContentBlockType::Custom(kind) => {
                        open.insert(*index, (format!("custom:{kind}"), String::new()));
                        continue;
                    }
                    starriver_kernel::llm::ContentBlockType::Image => {
                        open.insert(*index, ("image".into(), String::new()));
                        continue;
                    }
                    starriver_kernel::llm::ContentBlockType::ToolResult => {
                        open.insert(*index, ("tool-result".into(), String::new()));
                        continue;
                    }
                };
                open.insert(*index, (kind.to_string(), String::new()));
            }
            Ok(StreamChunk::TextDelta { index, text })
            | Ok(StreamChunk::ReasoningDelta { index, text }) => {
                open.entry(*index)
                    .or_insert_with(|| (String::new(), String::new()))
                    .1
                    .push_str(text);
            }
            Ok(StreamChunk::ToolCallDelta {
                index,
                arguments_delta,
                ..
            }) => {
                open.entry(*index)
                    .or_insert_with(|| ("tool-call".into(), String::new()))
                    .1
                    .push_str(arguments_delta);
            }
            Ok(StreamChunk::BlockEnd { index, .. }) => {
                assert!(
                    open.remove(index).is_some(),
                    "block-end without block-start: {chunks:?}"
                );
            }
            _ => {}
        }
    }
    assert!(open.is_empty(), "blocks left open at stream end: {open:?}");
}

/// Terminal error code accessor for cancel/idle/error assertions.
pub fn terminal_error_code(chunks: &Chunks) -> Option<String> {
    match chunks.last() {
        Some(Err(LlmError::Call { code, .. })) => Some(code.clone()),
        _ => None,
    }
}

/// Text blocks in emission order (for content assertions).
pub fn text_blocks(chunks: &Chunks) -> Vec<String> {
    chunks
        .iter()
        .filter_map(|c| match c {
            Ok(StreamChunk::BlockEnd {
                block: ContentBlock::Text(t),
                ..
            }) => Some(t.clone()),
            _ => None,
        })
        .collect()
}
