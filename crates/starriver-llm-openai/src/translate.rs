//! Pure state machine: feed SSE data payloads, collect kernel chunks.

use starriver_kernel::llm::{
    CallId, ContentBlock, ContentBlockType, FinishReason, LlmError, StreamChunk, TokenUsage,
};
use std::collections::HashMap;

/// Shared SSE payload → kernel chunk seam used by the adapter's streaming
/// loop (chat completions and responses share one byte-reading skeleton).
pub trait StreamTranslator {
    fn is_done(&self) -> bool;
    fn feed(&mut self, payload: &str) -> Vec<Result<StreamChunk, LlmError>>;
    fn finish(&mut self) -> Vec<Result<StreamChunk, LlmError>>;
}

struct OpenBlock {
    index: u64,
    kind: Kind,
    text: String,
    call_id: String,
    name: String,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Text,
    Reasoning,
    ToolCall,
}

pub struct SseTranslator {
    blocks: Vec<OpenBlock>,
    tool_wire_to_block: HashMap<u32, u64>,
    next_index: u64,
    pending_finish: Option<FinishReason>,
    pending_usage: Option<TokenUsage>,
    done: bool,
}

impl StreamTranslator for SseTranslator {
    fn is_done(&self) -> bool {
        SseTranslator::is_done(self)
    }

    fn feed(&mut self, payload: &str) -> Vec<Result<StreamChunk, LlmError>> {
        SseTranslator::feed(self, payload)
    }

    fn finish(&mut self) -> Vec<Result<StreamChunk, LlmError>> {
        SseTranslator::finish(self)
    }
}

impl SseTranslator {
    pub fn new() -> Self {
        Self {
            blocks: Vec::new(),
            tool_wire_to_block: HashMap::new(),
            next_index: 0,
            pending_finish: None,
            pending_usage: None,
            done: false,
        }
    }

    pub fn is_done(&self) -> bool {
        self.done
    }

    pub fn feed(&mut self, payload: &str) -> Vec<Result<StreamChunk, LlmError>> {
        if payload == "[DONE]" {
            self.done = true;
            return self.finish();
        }
        let Ok(wire) = serde_json::from_str::<WireChunk>(payload) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        // With `stream_options.include_usage` the usage rides the chunk ROOT
        // (choices empty), not per-choice — read both to tolerate providers
        // that deviate.
        if let Some(usage) = &wire.usage {
            self.pending_usage = Some(map_usage(usage));
        }
        for choice in &wire.choices {
            if let Some(reason) = &choice.finish_reason {
                self.pending_finish = Some(map_finish(reason));
            }
            if let Some(usage) = &choice.usage {
                self.pending_usage = Some(map_usage(usage));
            }
            let Some(delta) = &choice.delta else { continue };
            if let Some(text) = &delta.reasoning_content {
                if !text.is_empty() {
                    let idx = self.ensure_open(Kind::Reasoning);
                    if let Some(b) = self.blocks.iter_mut().find(|b| b.index == idx) {
                        b.text.push_str(text);
                    }
                    out.push(Ok(StreamChunk::ReasoningDelta {
                        index: idx,
                        text: text.clone(),
                    }));
                }
            }
            if let Some(text) = &delta.content {
                if !text.is_empty() {
                    let idx = self.ensure_open(Kind::Text);
                    if let Some(b) = self.blocks.iter_mut().find(|b| b.index == idx) {
                        b.text.push_str(text);
                    }
                    out.push(Ok(StreamChunk::TextDelta {
                        index: idx,
                        text: text.clone(),
                    }));
                }
            }
            for call in delta.tool_calls.iter().flatten() {
                let wire_index = call.index.unwrap_or(0);
                // BUG-49: captured BEFORE the map insert below — the creation
                // arm already seeds id/name from this chunk.
                let is_new_block = !self.tool_wire_to_block.contains_key(&wire_index);
                let block_index = match self.tool_wire_to_block.get(&wire_index) {
                    Some(e) => *e,
                    None => {
                        let assigned = self.next_index;
                        self.next_index += 1;
                        self.tool_wire_to_block.insert(wire_index, assigned);
                        let name = call
                            .function
                            .as_ref()
                            .and_then(|f| f.name.clone())
                            .unwrap_or_default();
                        self.blocks.push(OpenBlock {
                            index: assigned,
                            kind: Kind::ToolCall,
                            text: String::new(),
                            call_id: call.id.clone().unwrap_or_default(),
                            name,
                        });
                        out.push(Ok(StreamChunk::BlockStart {
                            index: assigned,
                            block_type: ContentBlockType::ToolCall,
                        }));
                        assigned
                    }
                };
                // BUG-49: later chunks refine the block the same way the
                // kernel assembler does (BUG-42 contract) — a non-empty id
                // updates the recorded id, a name fragment CONTINUES the
                // recorded name (gateways may split function.name across
                // chunks); fragments flow as deltas even without an id field.
                if call.id.is_some() || call.function.as_ref().is_some_and(|f| f.name.is_some()) {
                    let name = call.function.as_ref().and_then(|f| f.name.clone());
                    out.push(Ok(StreamChunk::ToolCallDelta {
                        index: block_index,
                        id: CallId(call.id.clone().unwrap_or_default()),
                        name: name.clone(),
                        arguments_delta: String::new(),
                    }));
                    if !is_new_block {
                        if let Some(b) = self.blocks.iter_mut().find(|b| b.index == block_index) {
                            if let Some(id) = call.id.as_deref().filter(|i| !i.is_empty()) {
                                b.call_id = id.to_string();
                            }
                            if let Some(name) = name.filter(|n| !n.is_empty()) {
                                b.name.push_str(&name);
                            }
                        }
                    }
                }
                if let Some(args) = call.function.as_ref().and_then(|f| f.arguments.as_deref()) {
                    if !args.is_empty() {
                        out.push(Ok(StreamChunk::ToolCallDelta {
                            index: block_index,
                            id: CallId(String::new()),
                            name: None,
                            arguments_delta: args.to_string(),
                        }));
                        if let Some(b) = self.blocks.iter_mut().find(|b| b.index == block_index) {
                            b.text.push_str(args);
                        }
                    }
                }
            }
        }
        out
    }

    pub fn finish(&mut self) -> Vec<Result<StreamChunk, LlmError>> {
        let mut out = Vec::new();
        for block in self.blocks.drain(..) {
            out.push(Ok(StreamChunk::BlockEnd {
                index: block.index,
                block: close_block(block),
            }));
        }
        // BUG-99: no [DONE]/finish_reason before the byte stream ended =
        // truncated response — an error, never a clean stop.
        let reason = self.pending_finish.take().unwrap_or_else(FinishReason::truncated_stream);
        if let Some(usage) = self.pending_usage.take() {
            out.push(Ok(StreamChunk::Usage { usage }));
        }
        out.push(Ok(StreamChunk::Finish {
            reason,
            replay_state: None,
        }));
        out
    }

    fn ensure_open(&mut self, kind: Kind) -> u64 {
        if let Some(b) = self.blocks.iter().find(|b| b.kind == kind) {
            return b.index;
        }
        let idx = self.next_index;
        self.next_index += 1;
        self.blocks.push(OpenBlock {
            index: idx,
            kind,
            text: String::new(),
            call_id: String::new(),
            name: String::new(),
        });
        idx
    }
}

fn close_block(b: OpenBlock) -> ContentBlock {
    match b.kind {
        Kind::Text => ContentBlock::Text(b.text),
        Kind::Reasoning => ContentBlock::Reasoning(b.text),
        Kind::ToolCall => ContentBlock::ToolCall {
            id: CallId(b.call_id),
            name: Some(b.name),
            // WI-47 Phase B: the one text→JsonValue parse at block close.
            arguments: starriver_kernel::llm::tool_call_arguments_from_text(&b.text),
        },
    }
}

fn map_usage(w: &serde_json::Value) -> TokenUsage {
    let num = |k: &str| w.get(k).and_then(|v| v.as_u64());
    let detail = |c: &str, k: &str| w.get(c).and_then(|d| d.get(k)).and_then(|v| v.as_u64());
    let cache_read =
        detail("prompt_tokens_details", "cached_tokens").or_else(|| num("prompt_cache_hit_tokens"));
    TokenUsage {
        input_tokens: num("prompt_tokens")
            .unwrap_or(0)
            .saturating_sub(cache_read.unwrap_or(0)),
        output_tokens: num("completion_tokens").unwrap_or(0),
        cache_read_tokens: cache_read,
        cache_write_tokens: None,
        reasoning_tokens: detail("completion_tokens_details", "reasoning_tokens"),
    }
}

fn map_finish(reason: &str) -> FinishReason {
    match reason {
        "stop" => FinishReason::Stop,
        "tool_calls" => FinishReason::ToolCalls,
        "length" => FinishReason::MaxTokens,
        other => {
            let failure = starriver_kernel::llm::LlmFailure {
                message: format!("model stopped: {other}"),
                code: other.to_uppercase(),
                status: None,
                provider_retry_after_ms: None,
                request_id: None,
            };
            FinishReason::Error { failure }
        }
    }
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct WireChunk {
    #[serde(default)]
    choices: Vec<WireChoice>,
    #[serde(default)]
    usage: Option<serde_json::Value>,
}
#[derive(Debug, Deserialize)]
struct WireChoice {
    delta: Option<WireDelta>,
    finish_reason: Option<String>,
    #[serde(default)]
    usage: Option<serde_json::Value>,
}
#[derive(Debug, Deserialize)]
struct WireDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<WireToolCall>>,
    #[serde(default)]
    reasoning_content: Option<String>,
}
#[derive(Debug, Deserialize)]
struct WireToolCall {
    #[serde(default)]
    index: Option<u32>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<WireFunction>,
}
#[derive(Debug, Deserialize)]
struct WireFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

use serde::Deserialize;

#[cfg(test)]
mod tests {
    use super::*;
    use starriver_kernel::llm::{ContentBlock, FinishReason, StreamChunk};

    #[test]
    fn root_level_stream_usage_is_captured() {
        // With stream_options.include_usage the usage arrives on the chunk
        // ROOT with an empty choices array (OpenAI spec) — it must reach the
        // final Usage chunk, not be dropped because no choice carries it.
        let mut t = SseTranslator::new();
        t.feed(r#"{"choices":[{"delta":{"content":"hi"}}]}"#);
        t.feed(r#"{"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":7}}"#);
        let mut usage = None;
        for chunk in t.finish() {
            if let Ok(StreamChunk::Usage { usage: u }) = chunk {
                usage = Some(u);
            }
        }
        let usage = usage.expect("root-level usage must be captured");
        assert_eq!(usage.input_tokens, 12);
        assert_eq!(usage.output_tokens, 7);
    }

    #[test]
    fn finish_carries_accumulated_text_and_reasoning() {
        let mut t = SseTranslator::new();
        t.feed(r#"{"choices":[{"delta":{"reasoning_content":" think"}}]}"#);
        t.feed(r#"{"choices":[{"delta":{"content":"hello"}}]}"#);
        t.feed(r#"{"choices":[{"delta":{"content":" world"}}]}"#);
        t.feed(r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#);

        let mut blocks = Vec::new();
        for chunk in t.finish() {
            match chunk {
                Ok(StreamChunk::BlockEnd { block, .. }) => blocks.push(block),
                Ok(StreamChunk::Finish { reason, .. }) => {
                    assert_eq!(reason, FinishReason::Stop);
                }
                _ => {}
            }
        }
        assert!(
            blocks
                .iter()
                .any(|b| matches!(b, ContentBlock::Text(t) if t == "hello world")),
            "text deltas must accumulate into the closed block: {blocks:?}"
        );
        assert!(
            blocks
                .iter()
                .any(|b| matches!(b, ContentBlock::Reasoning(t) if t == " think")),
            "reasoning deltas must accumulate into the closed block: {blocks:?}"
        );
    }

    /// BUG-99: byte-stream end without [DONE]/finish_reason = truncated
    /// response — the synthesized finish must be an ERROR, never Stop.
    #[test]
    fn stream_end_without_finish_reason_is_truncation_not_stop() {
        let mut t = SseTranslator::new();
        t.feed(r#"{"choices":[{"delta":{"reasoning_content":" think"}}]}"#);
        let mut got = None;
        for chunk in t.finish() {
            if let Ok(StreamChunk::Finish { reason, .. }) = chunk {
                got = Some(reason);
            }
        }
        let reason = got.expect("finish() must always emit a terminal chunk");
        match reason {
            FinishReason::Error { failure } => {
                assert_eq!(failure.code, "PROVIDER_STREAM_TRUNCATED");
            }
            other => panic!("truncated stream must surface an error, got {other:?}"),
        }
    }
}

// ── BUG-49 regression (round 5): later chunks refine id/name ──
#[cfg(test)]
mod round5_tests {
    use super::*;
    use starriver_kernel::llm::{ContentBlock, StreamChunk};

    /// The kernel assembler contract (BUG-42 fix): a delta with a non-empty
    /// id updates the block id, a delta with a name updates the name — later
    /// chunks carrying id/name fragments must be folded into the block the
    /// same way arguments are, or the closed BlockEnd carries stale values.
    #[test]
    fn later_id_and_name_fragments_merge_into_the_block() {
        let mut t = SseTranslator::new();
        let mut chunks = Vec::new();
        chunks.extend(t.feed(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"get_weath","arguments":""}}]}}]}"#,
        ));
        // Name continuation on a chunk WITHOUT an id (the "no id update"
        // convention) — the fragment must at least flow as a delta.
        chunks.extend(t.feed(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"er","arguments":"{}"}}]}}]}"#,
        ));
        chunks.extend(t.finish());

        let name_delta = chunks.iter().any(|c| matches!(c,
            Ok(StreamChunk::ToolCallDelta { name: Some(n), .. }) if n == "er"));
        assert!(name_delta, "a name fragment on a later chunk must surface as a delta: {chunks:?}");

        let block_name = chunks.iter().find_map(|c| match c {
            Ok(StreamChunk::BlockEnd {
                block: ContentBlock::ToolCall { name, .. },
                ..
            }) => name.clone(),
            _ => None,
        });
        assert_eq!(
            block_name.as_deref(),
            Some("get_weather"),
            "the closed block must carry the merged name (got {block_name:?})"
        );
    }
}
