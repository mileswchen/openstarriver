use crate::{
    helpers::{format_time, tool_call_label, truncate_title},
    types, HostRuntime,
};
use starriver_kernel::ui::UiEvent;
use starriver_api_macro::api_method;
use starriver_kernel::agent::AgentRegistry;
use starriver_kernel::json::JsonValue;
use starriver_kernel::llm::{
    ContentBlock, ContextForm, Message as KernelMessage, MessageRole, MessageSource,
};
use starriver_kernel::session::{AppendOptions, SessionEvent, SessionHeader, SessionId};
use std::collections::HashMap;

/// The model-visible surface of a durable log as `(seq, message)` pairs,
/// derived exactly like the kernel's `derive_messages`: only events carrying
/// `SurfaceOp::Append` participate; usage-only assistant anchors (empty
/// content) and non-surface meta events (`session/title`, `session/archived`)
/// are invisible.
pub(crate) async fn load_surface_messages(
    runtime: &crate::RuntimeInner,
    session_id: &str,
) -> anyhow::Result<Vec<(u64, KernelMessage)>> {
    Ok(load_surface_entries(runtime, session_id)
        .await?
        .into_iter()
        .filter_map(|entry| entry.message.map(|message| (entry.seq, message)))
        .collect())
}

/// Per-entry instrumentation metadata (trace view): turn/step indexes,
/// chunk-derived streaming timings, and the active request route.
#[derive(Debug, Clone, Default)]
pub(crate) struct SurfaceMeta {
    pub turn: Option<u32>,
    pub step: Option<u32>,
    /// assistant/message only: first chunk time (TTFT anchor).
    pub first_chunk_ms: Option<u64>,
    /// Same (turn, step)'s step/start time (TTFT origin).
    pub step_start_ms: Option<u64>,
    pub provider: Option<String>,
    pub model: Option<String>,
    /// request/header only.
    pub header_reason: Option<String>,
    pub system: Option<String>,
}

/// One surface-log entry projected for the messages/timeline views: the
/// source event's wall-clock time, its type (`turn/end` entries carry no
/// message), and the event's own `usage` payload when present.
pub(crate) struct SurfaceEntry {
    pub seq: u64,
    pub time: u64,
    pub event_type: String,
    /// (inputTokens, outputTokens) from the event's `usage` payload —
    /// assistant messages only; `None` on legacy entries and other types.
    pub usage: Option<(u64, u64)>,
    pub message: Option<KernelMessage>,
    /// Trace instrumentation: turn/step, streaming timings, request route.
    pub meta: Option<SurfaceMeta>,
    /// Raw event payload. Only read today for `turn/end` failure reasons
    /// (`reason.kind=error` + `failure.message`): the projection surfaces the
    /// error as a system row so a failed turn stays visible in history.
    pub data: JsonValue,
}

/// Surface messages with their source event wall-clock time (ms epoch) —
/// the log already records `time`; this projects it for display timestamps.
pub(crate) async fn load_surface_entries(
    runtime: &crate::RuntimeInner,
    session_id: &str,
) -> anyhow::Result<Vec<SurfaceEntry>> {
    Ok(load_projection_parts(runtime, session_id).await?.0)
}

/// Full projection input: surface entries plus the callId→tool-name registry
/// from raw `tool/call` events (turn file-change attribution needs it — the
/// `tool/result` payload only carries the callId).
/// Model-visible message stream (`types::Message`) for a cold session — the
/// A2a-closeout replacement for the legacy `get_session_history` reads
/// (`/context-stats`, `/export`): the legacy messages table never received
/// post-migration writes, so deriving from the coordinator log is the only
/// correct source.
pub(crate) async fn load_model_messages(
    runtime: &crate::RuntimeInner,
    session_id: &str,
) -> anyhow::Result<Vec<starriver_kernel::types::Message>> {
    ensure_durable(runtime).await?;
    let inspection = runtime
        .persistence
        .coordinator()
        .load(SessionId(session_id.to_string()))
        .map_err(|error| anyhow::anyhow!("load {session_id}: {error}"))?;
    let session = starriver_kernel::session::Session::new(
        SessionId(session_id.to_string()),
        cordis_rslite::EventBus::new(),
    )
    .with_header(inspection.header.clone())
    .map_err(|error| anyhow::anyhow!("header for {session_id}: {error}"))?
    .with_seed(inspection.events.clone())
    .map_err(|error| anyhow::anyhow!("seed replay for {session_id}: {error}"))?;
    let derived = session
        .derive_messages()
        .map_err(|error| anyhow::anyhow!("derive {session_id}: {error}"))?;
    Ok(derived.into_iter().map(llm_message_to_session).collect())
}

/// Project a kernel `llm::Message` onto the legacy session `types::Message`
/// shape the host read paths (token estimates, markdown export) consume.
/// Images and custom payloads have no legacy representation and are dropped —
/// same fidelity the pre-migration store offered.
fn llm_message_to_session(msg: starriver_kernel::llm::Message) -> starriver_kernel::types::Message {
    use starriver_kernel::llm::{ContentBlock as LlmBlock, MessageRole};
    use starriver_kernel::types::{ContentBlock, Message, Role};
    let role = match msg.role {
        MessageRole::System => Role::System,
        MessageRole::User => Role::User,
        MessageRole::Assistant => Role::Assistant,
    };
    let content = msg
        .content
        .into_iter()
        .filter_map(|block| match block {
            LlmBlock::Text(text) => Some(ContentBlock::Text { text }),
            LlmBlock::Reasoning(text) => Some(ContentBlock::Thinking { text }),
            LlmBlock::ToolCall {
                id,
                name,
                arguments,
            } => Some(ContentBlock::ToolUse {
                id: id.0,
                name: name.unwrap_or_default(),
                // WI-47 Phase B: lossless conversion, no re-parse.
                input: arguments.to_serde_json().unwrap_or(serde_json::json!({})),
            }),
            LlmBlock::ToolResult {
                call_id,
                content,
                is_error,
            } => {
                let text = content
                    .into_iter()
                    .filter_map(|b| match b {
                        LlmBlock::Text(text) => Some(text),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                Some(ContentBlock::ToolResult {
                    tool_use_id: call_id.0,
                    content: text,
                    is_error,
                })
            }
            LlmBlock::Image { .. } | LlmBlock::Custom { .. } => None,
        })
        .collect();
    Message {
        role,
        content,
        meta: None,
    }
}

pub(crate) async fn load_projection_parts(
    runtime: &crate::RuntimeInner,
    session_id: &str,
) -> anyhow::Result<(Vec<SurfaceEntry>, HashMap<String, String>)> {
    ensure_durable(runtime).await?;
    let inspection = runtime
        .persistence
        .coordinator()
        .load(SessionId(session_id.to_string()))
        .map_err(|error| anyhow::anyhow!("load {session_id}: {error}"))?;
    // Rebuild an ephemeral session over the balanced log so the kernel's own
    // surface fold applies (the seed is contiguous by construction).
    let session = starriver_kernel::session::Session::new(
        SessionId(session_id.to_string()),
        cordis_rslite::EventBus::new(),
    )
    .with_header(inspection.header.clone())
    .map_err(|error| anyhow::anyhow!("header for {session_id}: {error}"))?
    .with_seed(inspection.events.clone())
    .map_err(|error| anyhow::anyhow!("seed replay for {session_id}: {error}"))?;
    let by_seq: HashMap<u64, &SessionEvent> = inspection
        .events
        .iter()
        .map(|event| (event.seq, event))
        .collect();
    let mut pairs = Vec::new();
    let mut tool_names: HashMap<String, String> = HashMap::new();
    for event in inspection.events.iter() {
        if event.event_type == "tool/call" {
            if let (Some(call_id), Some(name)) = (
                event.data.get("callId").and_then(JsonValue::as_str),
                event.data.get("name").and_then(JsonValue::as_str),
            ) {
                tool_names.insert(call_id.to_string(), name.to_string());
            }
        }
    }
    // Instrumentation inputs: step/start times per (turn, step), first chunk
    // time per assistant/message (via its cited chunk seqs), and the active
    // request route (request/header carries provider/model; its key includes
    // the system string so any change re-logs it).
    let mut step_starts: HashMap<(u32, u32), u64> = HashMap::new();
    let mut route_by_seq: std::collections::BTreeMap<u64, (String, String)> = Default::default();
    let event_time: HashMap<u64, u64> = inspection.events.iter().map(|e| (e.seq, e.time)).collect();
    for event in inspection.events.iter() {
        match event.event_type.as_str() {
            "step/start" => {
                if let (Some(t), Some(st)) = (
                    event.data.get("turn").and_then(JsonValue::as_u64),
                    event.data.get("step").and_then(JsonValue::as_u64),
                ) {
                    step_starts
                        .entry((t as u32, st as u32))
                        .or_insert(event.time);
                }
            }
            "assistant/message" => {
                // First-chunk timing is derived per entry in aux_meta.
            }
            "request/header" => {
                let config = event.data.get("config");
                if let (Some(provider), Some(model)) = (
                    config
                        .and_then(|c| c.get("provider"))
                        .and_then(JsonValue::as_str),
                    config
                        .and_then(|c| c.get("model"))
                        .and_then(JsonValue::as_str),
                ) {
                    route_by_seq.insert(event.seq, (provider.to_string(), model.to_string()));
                }
            }
            _ => {}
        }
    }
    let aux_meta = |seq: u64,
                    event_type: &str,
                    data: &JsonValue,
                    source_seqs: &[u64]|
     -> Option<SurfaceMeta> {
        let mut meta = SurfaceMeta::default();
        if matches!(event_type, "assistant/message" | "tool/result") {
            meta.turn = data
                .get("turn")
                .and_then(JsonValue::as_u64)
                .map(|v| v as u32);
            meta.step = data
                .get("step")
                .and_then(JsonValue::as_u64)
                .map(|v| v as u32);
        }
        if event_type == "assistant/message" {
            if let (Some(turn), Some(step)) = (meta.turn, meta.step) {
                meta.step_start_ms = step_starts.get(&(turn, step)).copied();
            }
            meta.first_chunk_ms = source_seqs
                .iter()
                .filter_map(|sq| event_time.get(sq))
                .min()
                .copied();
            let route_here = route_by_seq
                .range(..=seq)
                .next_back()
                .map(|(_, v)| v.clone());
            if let Some((provider, model)) = route_here {
                meta.provider = Some(provider.clone());
                meta.model = Some(model.clone());
            }
        }
        if event_type == "request/header" {
            meta.header_reason = Some(
                data.get("reason")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("initial")
                    .to_string(),
            );
            meta.system = data
                .get("system")
                .and_then(JsonValue::as_str)
                .map(str::to_string);
        }
        let populated = event_type == "assistant/message"
            || event_type == "request/header"
            || meta.turn.is_some();
        populated.then_some(meta)
    };
    for seq in session.surface().nodes {
        let Some(event) = by_seq.get(&seq) else {
            continue;
        };
        // Turn token totals ride the event's own `usage` payload (assistant
        // messages; Null on legacy entries written before the adapters fed
        // usage through).
        let usage = event.data.get("usage").and_then(|u| {
            Some((
                u.get("inputTokens").and_then(JsonValue::as_u64)?,
                u.get("outputTokens").and_then(JsonValue::as_u64)?,
            ))
        });
        // Decode the message body; `None` = the entry matters without one
        // (usage anchors, turn/end markers).
        let decoded: Option<KernelMessage> = match event.event_type.as_str() {
            "user/message" => match KernelMessage::from_json(event.data.clone()) {
                Ok(message) => Some(message),
                Err(error) => {
                    tracing::warn!(session = %session_id, seq, %error, "undecodable log entry skipped");
                    continue;
                }
            },
            "assistant/message" | "tool/result" => {
                let inner = event
                    .data
                    .get("message")
                    .cloned()
                    .unwrap_or(JsonValue::Null);
                match KernelMessage::from_json(inner) {
                    // Usage anchors carry no renderable content — the entry
                    // survives for its usage, the message is dropped.
                    Ok(message) if message.content.is_empty() => None,
                    Ok(message) => Some(message),
                    Err(error) => {
                        tracing::warn!(session = %session_id, seq, %error, "undecodable log entry skipped");
                        continue;
                    }
                }
            }
            "turn/end" => None,
            _ => continue,
        };
        pairs.push(SurfaceEntry {
            seq,
            time: event.time,
            event_type: event.event_type.clone(),
            usage,
            message: decoded.clone(),
            data: event.data.clone(),
            meta: aux_meta(
                seq,
                &event.event_type,
                &event.data,
                &event.source_event_seqs,
            ),
        });
    }
    // `turn/end` carries no surface op, so it never becomes a surface node —
    // merge those entries in from the raw log (by seq) so the turn projection
    // can close each turn's duration/tokens window. `request/header` is the
    // same (log-only) and matters for the trace: it carries the FULL
    // assembled system prompt actually sent to the LLM.
    let turn_ends = inspection
        .events
        .iter()
        .filter(|event| event.event_type == "turn/end")
        .map(|event| SurfaceEntry {
            seq: event.seq,
            time: event.time,
            event_type: "turn/end".into(),
            usage: None,
            message: None,
            meta: None,
            data: event.data.clone(),
        })
        .collect::<Vec<_>>();
    let headers = inspection
        .events
        .iter()
        .filter(|event| event.event_type == "request/header")
        .filter_map(|event| {
            let system = event.data.get("system").and_then(JsonValue::as_str)?;
            let reason = event
                .data
                .get("reason")
                .and_then(JsonValue::as_str)
                .unwrap_or("initial")
                .to_string();
            Some(SurfaceEntry {
                seq: event.seq,
                time: event.time,
                event_type: "request/header".into(),
                usage: None,
                message: None,
                data: JsonValue::Null,
                meta: Some(SurfaceMeta {
                    header_reason: Some(reason),
                    system: Some(system.to_string()),
                    ..Default::default()
                }),
            })
        })
        .collect::<Vec<_>>();
    let mut aux = turn_ends;
    aux.extend(headers);
    if aux.is_empty() {
        return Ok((pairs, tool_names));
    }
    aux.sort_by_key(|e| e.seq);
    let mut merged = Vec::with_capacity(pairs.len() + aux.len());
    let mut base = pairs.into_iter().peekable();
    let mut ends = aux.into_iter().peekable();
    loop {
        match (base.peek(), ends.peek()) {
            (Some(b), Some(e)) => {
                if b.seq <= e.seq {
                    merged.push(base.next().unwrap());
                } else {
                    merged.push(ends.next().unwrap());
                }
            }
            (Some(_), None) => merged.push(base.next().unwrap()),
            (None, Some(_)) => merged.push(ends.next().unwrap()),
            (None, None) => break,
        }
    }
    Ok((merged, tool_names))
}

fn role_str(role: &MessageRole) -> &'static str {
    match role {
        MessageRole::System => "system",
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
    }
}

/// Flatten rendered content blocks to plain lines, mirroring the legacy
/// presentation: raw text, labeled tool calls, check/cross-headed results.
pub(crate) fn flatten_blocks(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.clone()),
            ContentBlock::Reasoning(_) => None,
            ContentBlock::Image { .. } => None,
            ContentBlock::ToolCall {
                id: _,
                name,
                arguments,
            } => {
                let name = name.clone().unwrap_or_default();
                // WI-47 Phase B: lossless conversion (a String argument —
                // malformed ingress — yields no keys, as before).
                let parsed: Option<serde_json::Value> = arguments.to_serde_json().ok();
                let arg = crate::helpers::tool_call_arg(&name, &parsed);
                Some(tool_call_label(&name, &arg))
            }
            ContentBlock::ToolResult {
                call_id: _,
                content,
                is_error,
            } => {
                let head = if *is_error { "✗ " } else { "✓ " };
                let inner: String = flatten_blocks(content);
                Some(format!("{head}{inner}"))
            }
            ContentBlock::Custom { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Split one surface message into display pieces for `get_session_messages`:
/// prose keeps its own role, while tool calls/results become separate `tool`
/// rows — the shape the frontend's stored-row renderer expects (tool cards with
/// ✓/✗-prefixed result bodies). `flatten_blocks` alone would inline a file-sized
/// tool output into the assistant prose, where markdown collapses every newline.
/// The backtrack timeline keeps using `flatten_blocks` (seq-per-node must stay
/// 1:1 for fork cutoffs); this split is display-only.
/// Append pieces and stamp the source entry's seq onto each (BUG-86).
fn stamp_seq(out: &mut Vec<types::ChatMessage>, pieces: Vec<types::ChatMessage>, seq: u64) {
    for mut piece in pieces {
        piece.seq = Some(seq);
        out.push(piece);
    }
}

pub(crate) fn display_pieces(
    role: &str,
    blocks: &[ContentBlock],
    ts: u64,
) -> Vec<types::ChatMessage> {
    let mut out: Vec<types::ChatMessage> = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text(text) => out.push(types::ChatMessage {
                role: role.to_string(),
                content: text.clone(),
                ts: Some(ts),
                usage: None,
                duration_ms: None,
                done_ts: None,
                file_changes: None,
                seq: None,
            }),
            // WI-1 P1: reference blocks render back as markdown image lines
            // so history keeps showing the image (pre-P0 logs carried the
            // same shape inline in the text). The desktop renderer resolves
            // `sha256:` refs to thumbnails via `attachments.read`; blocks
            // without a fileId (legacy inline) stay dropped — their text
            // already carried the visible ref.
            ContentBlock::Image { attachment } => {
                let Some(file_id) = attachment.as_str_at("fileId") else {
                    continue;
                };
                let media = attachment.as_str_at("mediaType").unwrap_or("image");
                out.push(types::ChatMessage {
                    role: role.to_string(),
                    content: format!("![图片 {media}]({file_id})"),
                    ts: Some(ts),
                    usage: None,
                    duration_ms: None,
                    done_ts: None,
                    file_changes: None,
                    seq: None,
                });
            }
            ContentBlock::ToolCall {
                name, arguments, ..
            } => {
                let name = name.clone().unwrap_or_default();
                // WI-47 Phase B: lossless conversion (a String argument —
                // malformed ingress — yields no keys, as before).
                let parsed: Option<serde_json::Value> = arguments.to_serde_json().ok();
                let arg = crate::helpers::tool_call_arg(&name, &parsed);
                out.push(types::ChatMessage {
                    role: "tool".to_string(),
                    content: tool_call_label(&name, &arg),
                    ts: Some(ts),
                    usage: None,
                    duration_ms: None,
                    done_ts: None,
                    file_changes: None,
                    seq: None,
                });
            }
            ContentBlock::ToolResult {
                content, is_error, ..
            } => {
                let head = if *is_error { "✗" } else { "✓" };
                out.push(types::ChatMessage {
                    role: "tool".to_string(),
                    content: format!("{head}\n{}", flatten_blocks(content)),
                    ts: Some(ts),
                    usage: None,
                    duration_ms: None,
                    done_ts: None,
                    file_changes: None,
                    seq: None,
                });
            }
            // Chain-of-thought blocks ride inside the assistant message and are
            // already persisted in the log — project them as `thinking` rows so
            // the UI can replay them in place (collapsed by default).
            ContentBlock::Reasoning(text) => {
                out.push(types::ChatMessage {
                    role: "thinking".to_string(),
                    content: text.clone(),
                    ts: Some(ts),
                    usage: None,
                    duration_ms: None,
                    done_ts: None,
                    file_changes: None,
                    seq: None,
                });
            }
            _ => {}
        }
    }
    out
}

/// Project surface entries into display messages. Each turn's per-message
/// usage is folded onto its final assistant text piece, and the turn
/// duration is derived from the first-event→turn/end wall-clock window
/// (Option 2: pure projection — the log already carries every timestamp).
/// `turn/end` also supplies the accurate doneTs for that piece, plus the
/// turn's file-change summary parsed from edit/write/patch tool results
/// (latest diff per path; `tool_names` attributes results to their tool).
pub(crate) fn project_chat_messages(
    entries: Vec<SurfaceEntry>,
    tool_names: &HashMap<String, String>,
) -> Vec<types::ChatMessage> {
    let mut out: Vec<types::ChatMessage> = Vec::new();
    let mut turn_first_time: Option<u64> = None;
    let mut turn_tokens: (u64, u64) = (0, 0);
    let mut last_text: Option<usize> = None;
    let mut turn_files: HashMap<String, types::FileChange> = HashMap::new();

    for entry in entries {
        match entry.event_type.as_str() {
            "user/message" => {
                // Plugin-injected model-visible plumbing messages (runtime-context
                // snapshots, time-context samples) are not user input: keep them
                // out of the history timeline and away from the turn-start anchor.
                if let Some(message) = &entry.message {
                    if matches!(message.source, MessageSource::Plugin { .. }) {
                        continue;
                    }
                }
                // A mid-turn user message exists too (the kernel feeds tool
                // results back as user-role input), so this is NOT a turn
                // boundary — only `turn/end` settles the accumulation. The
                // start time is anchored at the first user message only.
                if turn_first_time.is_none() {
                    turn_first_time = Some(entry.time);
                }
                if let Some(message) = entry.message {
                    let role = role_str(&message.role);
                    stamp_seq(&mut out, display_pieces(&role, &message.content, entry.time), entry.seq);
                }
            }
            "assistant/message" => {
                if let Some((input, output)) = entry.usage {
                    turn_tokens.0 += input;
                    turn_tokens.1 += output;
                }
                if let Some(message) = entry.message {
                    let role = role_str(&message.role);
                    let pieces = display_pieces(&role, &message.content, entry.time);
                    for mut piece in pieces {
                        piece.seq = Some(entry.seq);
                        if piece.role == "assistant" && !piece.content.is_empty() {
                            last_text = Some(out.len());
                        }
                        out.push(piece);
                    }
                }
            }
            "tool/result" => {
                if let Some(message) = &entry.message {
                    let role = role_str(&message.role);
                    stamp_seq(&mut out, display_pieces(&role, &message.content, entry.time), entry.seq);
                    for block in &message.content {
                        let ContentBlock::ToolResult {
                            call_id,
                            content,
                            is_error,
                        } = block
                        else {
                            continue;
                        };
                        if *is_error {
                            continue;
                        }
                        let Some(name) = tool_names.get(&call_id.0) else {
                            continue;
                        };
                        if name != "edit" && name != "write" && name != "patch" {
                            continue;
                        }
                        let text = flatten_blocks(content);
                        for change in parse_file_changes(&text) {
                            turn_files.insert(change.path.clone(), change);
                        }
                    }
                }
            }
            "turn/end" => {
                // The turn's final assistant text piece carries the totals.
                if let Some(idx) = last_text {
                    let piece = &mut out[idx];
                    if let Some(start) = turn_first_time {
                        piece.duration_ms = Some(entry.time.saturating_sub(start));
                    }
                    piece.done_ts = Some(entry.time);
                    if turn_tokens != (0, 0) {
                        piece.usage = Some(types::TokenTotals {
                            input_tokens: turn_tokens.0,
                            output_tokens: turn_tokens.1,
                        });
                    }
                    if !turn_files.is_empty() {
                        let mut files: Vec<types::FileChange> =
                            turn_files.values().cloned().collect();
                        files.sort_by(|a, b| a.path.cmp(&b.path));
                        piece.file_changes = Some(files);
                    }
                }
                turn_first_time = None;
                turn_tokens = (0, 0);
                last_text = None;
                turn_files.clear();
                // A failed turn carries its reason in the raw payload
                // (`reason.kind=error` + `failure.message`). Surface it as a
                // system row so the failure (e.g. insufficient balance)
                // stays visible after a reload — without this the failed turn
                // vanishes from history and consecutive failed prompts look
                // like one merged message.
                let failure = entry
                    .data
                    .get("reason")
                    .filter(|r| r.get("kind").and_then(JsonValue::as_str) == Some("error"))
                    .and_then(|r| r.get("failure"))
                    .and_then(|f| f.get("message"))
                    .and_then(JsonValue::as_str)
                    .filter(|m| !m.trim().is_empty());
                if let Some(message) = failure {
                    stamp_seq(
                        &mut out,
                        vec![types::ChatMessage {
                            role: "system".to_string(),
                            content: format!("❌ {message}"),
                            ts: Some(entry.time),
                            usage: None,
                            duration_ms: None,
                            done_ts: None,
                            file_changes: None,
                            seq: Some(entry.seq),
                        }],
                        entry.seq,
                    );
                }
            }
            _ => {
                if let Some(message) = entry.message {
                    let role = role_str(&message.role);
                    stamp_seq(&mut out, display_pieces(&role, &message.content, entry.time), entry.seq);
                }
            }
        }
    }
    out
}

/// Project surface entries into TrajectoryView trace entries: every content
/// Byte-exact live view (open-turn snapshot stream): the open-turn slice.
/// Slices from the last `user/message` (inclusive) to the end — the turn header/tools/
/// assistant content after it are all inside; the full projection and the slice projection
/// agree entry-by-entry on this range. Returns `(turn_index, slice)`;
/// turn_index is the number of user/message entries before the slice (0-based turn number).
/// No user entry (turn not started yet) → None.
pub(crate) fn open_turn_slice(
    mut entries: Vec<SurfaceEntry>,
) -> Option<(u32, Vec<SurfaceEntry>)> {
    let last_user = entries.iter().rposition(|e| e.event_type == "user/message")?;
    let turn_index = entries[..=last_user]
        .iter()
        .filter(|e| e.event_type == "user/message")
        .count() as u32
        - 1;
    let slice = entries.split_off(last_user);
    Some((turn_index, slice))
}

/// Build a TraceSnapshot event from the slice (same projection as `project_trace_entries`;
/// identical-content skipping is done by the caller's hash).
pub(crate) fn build_trace_snapshot(
    session_id: &str,
    epoch: u64,
    turn_index: u32,
    slice: Vec<SurfaceEntry>,
    tool_names: &HashMap<String, String>,
) -> Option<starriver_kernel::ui::UiEvent> {
    let projected = project_trace_entries(slice, tool_names);
    let entries = serde_json::to_value(&projected).ok()?;
    Some(UiEvent::TraceSnapshot {
        version: UiEvent::VERSION,
        session_id: session_id.to_string(),
        epoch,
        turn_index,
        open: true,
        via_remote: false,
        entries,
    })
}

/// block kind becomes its own chronological row (user/assistant text, thinking
/// for reasoning blocks, tool_use with name+input, tool_result with output+ok).
/// Plugin-sourced plumbing messages (runtime-context snapshots) are excluded,
/// matching project_chat_messages.
pub(crate) fn project_trace_entries(
    entries: Vec<SurfaceEntry>,
    tool_names: &HashMap<String, String>,
) -> Vec<types::SessionTraceEntry> {
    let mut out = Vec::new();
    let empty_row = || types::SessionTraceEntry {
        kind: String::new(),
        text: String::new(),
        tool_name: None,
        tool_input: None,
        tool_output: None,
        tool_ok: None,
        seq: 0.0,
        time_ms: 0.0,
        turn: None,
        step: None,
        input_tokens: None,
        output_tokens: None,
        provider: None,
        model: None,
        duration_ms: None,
        ttft_ms: None,
    };
    for entry in entries {
        if entry.event_type == "request/header" {
            let entry_meta = entry.meta.clone();
            let meta = entry_meta.as_ref();
            let reason = meta
                .and_then(|m| m.header_reason.as_deref())
                .unwrap_or("initial");
            let system = meta.and_then(|m| m.system.as_deref()).unwrap_or("");
            let label = if reason == "change" {
                "system prompt (changed)"
            } else {
                "system prompt"
            };
            out.push(types::SessionTraceEntry {
                kind: "system".to_string(),
                text: format!("[{label}]\n\n{system}"),
                tool_name: None,
                tool_input: None,
                tool_output: None,
                tool_ok: None,
                seq: entry.seq as f64,
                time_ms: entry.time as f64,
                turn: None,
                step: None,
                input_tokens: None,
                output_tokens: None,
                provider: None,
                model: None,
                duration_ms: None,
                ttft_ms: None,
            });
            continue;
        }
        let entry_seq = entry.seq;
        let entry_time = entry.time;
        let entry_meta = entry.meta.clone();
        let entry_usage = entry.usage;
        let Some(message) = entry.message else {
            continue;
        };
        // Plugin-sourced snapshot injections (runtime-context, agent
        // instructions, time-context) are model-visible context, not user
        // input — the trajectory shows them as CONTEXT rows (the chat view
        // keeps hiding them).
        let base_fields = |out: &mut types::SessionTraceEntry| {
            out.seq = entry_seq as f64;
            out.time_ms = entry_time as f64;
            if let Some(meta) = &entry_meta {
                out.turn = meta.turn.map(|v| v as f64);
                out.step = meta.step.map(|v| v as f64);
                out.provider = meta.provider.clone();
                out.model = meta.model.clone();
                if let Some(first) = meta.first_chunk_ms {
                    out.ttft_ms =
                        Some(first.saturating_sub(meta.step_start_ms.unwrap_or(first)) as f64);
                    out.duration_ms = Some(entry_time.saturating_sub(first) as f64);
                }
            }
        };
        if let MessageSource::Plugin { plugin, form } = &message.source {
            let text = match form {
                Some(ContextForm::Snapshot { sections, .. }) => {
                    let mut text = format!("[{plugin}]");
                    for section in sections {
                        text.push_str(&format!("\n\n## {}\n{}", section.name, section.text));
                    }
                    text
                }
                Some(ContextForm::Notice { summary }) => format!("[{plugin}] {summary}"),
                _ => format!("[{plugin}]"),
            };
            let mut row = types::SessionTraceEntry {
                kind: "context".to_string(),
                text,
                tool_name: Some(plugin.clone()),
                tool_input: None,
                tool_output: None,
                tool_ok: None,
                seq: 0.0,
                time_ms: 0.0,
                turn: None,
                step: None,
                input_tokens: None,
                output_tokens: None,
                provider: None,
                model: None,
                duration_ms: None,
                ttft_ms: None,
            };
            base_fields(&mut row);
            out.push(row);
            continue;
        }
        let role = role_str(&message.role);
        for block in &message.content {
            match block {
                ContentBlock::Text(text) => {
                    let kind = if role == "user" { "user" } else { "assistant" };
                    let mut row = empty_row();
                    row.kind = kind.to_string();
                    row.text = text.clone();
                    base_fields(&mut row);
                    if role == "assistant" {
                        if let Some((input, output)) = entry_usage {
                            row.input_tokens = Some(input as f64);
                            row.output_tokens = Some(output as f64);
                        }
                    }
                    out.push(row);
                }
                ContentBlock::Reasoning(text) => {
                    let mut row = empty_row();
                    row.kind = "thinking".to_string();
                    row.text = text.clone();
                    base_fields(&mut row);
                    out.push(row);
                }
                ContentBlock::ToolCall {
                    id,
                    name,
                    arguments,
                } => {
                    let name = name
                        .clone()
                        .or_else(|| tool_names.get(&id.0).cloned())
                        .unwrap_or_default();
                    let mut row = empty_row();
                    row.kind = "tool_use".to_string();
                    row.tool_name = Some(name);
                    row.tool_input = arguments.to_json_string().ok();
                    base_fields(&mut row);
                    out.push(row);
                }
                ContentBlock::ToolResult {
                    content, is_error, ..
                } => {
                    let mut row = empty_row();
                    row.kind = "tool_result".to_string();
                    row.tool_output = Some(flatten_blocks(content));
                    row.tool_ok = Some(!*is_error);
                    base_fields(&mut row);
                    out.push(row);
                }
                ContentBlock::Image { attachment } => {
                    // WI-182 (mobile): reference blocks render as markdown
                    // image lines so history keeps the image on the desktop
                    // AND the phone (sha256: refs resolve via attachments.read).
                    // Blocks with neither a fileId nor a url stay dropped.
                    let kind = if role == "user" { "user" } else { "assistant" };
                    let mut row = empty_row();
                    if let Some(file_id) = attachment.as_str_at("fileId") {
                        let media = attachment.as_str_at("mediaType").unwrap_or("image");
                        row.text = format!("![图片 {media}]({file_id})");
                    } else if let Some(url) = attachment.as_str_at("url") {
                        row.text = format!("![image]({url})");
                    } else {
                        continue;
                    }
                    row.kind = kind.to_string();
                    base_fields(&mut row);
                    out.push(row);
                }
                _ => {}
            }
        }
    }
 // The initial system prompt belongs to the session preamble (pins
    // "Initial System Prompt" to the top), but the ledger logs it at
    // first-request time — AFTER turn 1's user message. Hoist the first
    // `reason: "initial"` header row to the front; later "(changed)" headers
    // keep their chronological position.
    if let Some(pos) = out
        .iter()
        .position(|e| e.kind == "system" && e.text.starts_with("[system prompt]\n\n"))
    {
        if pos > 0 {
            let initial = out.remove(pos);
            out.insert(0, initial);
        }
    }
    out
}

/// Parse the unified diff that `append_diff` appends to edit/write/patch
/// tool outputs into per-file summaries. Mirrors the renderer's parseDiff;
/// tolerates the prose prefix ("edited /path" …) and the truncation tail.
/// A deleted line whose text starts with "-- " reads as a new file header —
/// the same limitation as the renderer's parser.
fn parse_file_changes(text: &str) -> Vec<types::FileChange> {
    let mut out: Vec<types::FileChange> = Vec::new();
    let mut cur: Option<(String, usize, usize, Vec<&str>)> = None;
    for line in text.lines() {
        if let Some(path) = line.strip_prefix("--- ") {
            if let Some((path, add, del, lines)) = cur.take() {
                out.push(types::FileChange {
                    path,
                    add: add as u64,
                    del: del as u64,
                    diff: lines.join("\n"),
                });
            }
            cur = Some((path.trim().to_string(), 0, 0, vec![line]));
            continue;
        }
        if line.starts_with("+++ ") {
            if let Some((_, _, _, lines)) = cur.as_mut() {
                lines.push(line);
            }
            continue;
        }
        let Some((path, add, del, lines)) = cur.as_mut() else {
            continue; // prose before the diff block
        };
        if line.starts_with("@@") || line.starts_with(' ') || line.is_empty() {
            lines.push(line);
        } else if line.starts_with('+') {
            lines.push(line);
            *add += 1;
        } else if line.starts_with('-') {
            lines.push(line);
            *del += 1;
        } else {
            // Truncation tail ("(diff truncated, …)") or unexpected prose —
            // keep it with the diff body; the renderer tolerates it.
            lines.push(line);
        }
        let _ = path;
    }
    if let Some((path, add, del, lines)) = cur.take() {
        out.push(types::FileChange {
            path,
            add: add as u64,
            del: del as u64,
            diff: lines.join("\n"),
        });
    }
    out
}

fn session_info_from(
    row: &crate::host_store::ProjcacheRow,
    headers: &[SessionHeader],
) -> types::SessionInfo {
    let header = headers.iter().find(|header| header.id.0 == row.session_id);
    types::SessionInfo {
        id: row.session_id.clone(),
        title: row.title.clone().unwrap_or_default(),
        workspace_id: row.workspace_id.clone(),
        created_at: header
            .map(|header| format_time((header.created_at / 1000) as i64))
            .unwrap_or_default(),
        message_count: row.message_count,
        parent_id: header
            .and_then(|header| header.parent_session.clone())
            .map(|id| id.0),
        fork_point: header
            .and_then(|header| header.seed_length)
            .map(|len| len as i64),
        agent: row.agent.clone(),
        origin: row.origin.clone(),
    }
}

/// Durable write-behind of a child log; test seeds and forks share the route.
async fn persist_child_log(
    runtime: &crate::RuntimeInner,
    session_id: &str,
    parent: Option<String>,
    seed_length: Option<u64>,
    cwd: Option<String>,
    events: Vec<starriver_kernel::session::SessionEvent>,
) -> anyhow::Result<()> {
    let mut header =
        starriver_kernel::session::SessionHeader::new(SessionId(session_id.to_string()));
    header.parent_session = parent.map(SessionId);
    header.seed_length = seed_length;
    header.cwd = cwd;
    runtime
        .persistence
        .coordinator()
        .append(SessionId(session_id.to_string()), header, events)
        .await
        .map_err(|error| anyhow::anyhow!("append log: {error}"))?;
    runtime.persistence.flush().await?;
    Ok(())
}

/// Read-side durability barrier: drain the write-behind window so subsequent
/// coordinator reads see every fact already produced. Idempotent and cheap
/// when the writer is idle.
pub(crate) async fn ensure_durable(runtime: &crate::RuntimeInner) -> anyhow::Result<()> {
    runtime
        .persistence
        .flush()
        .await
        .map_err(|error| anyhow::anyhow!("persistence flush: {error}"))
}

/// Append a host meta event to a session's log, whether the session is live
/// (agent in the registry) or cold (rebuilt through persistence prepare).
pub(crate) async fn append_session_event(
    runtime: &crate::RuntimeInner,
    session_id: &str,
    event_type: &str,
    data: JsonValue,
) -> anyhow::Result<()> {
    let id = SessionId(session_id.to_string());
    if let Some(agent) = runtime.agent.registry().get(&id) {
        agent
            .session()
            .append_with(event_type, data, AppendOptions::default())?;
        return Ok(());
    }
    // A cold session may have just left the live registry with its events
    // still inside the write-behind window — drain before resuming it.
    ensure_durable(runtime).await?;
    let prepared = runtime
        .persistence
        .coordinator()
        .prepare(id, None)
        .await
        .map_err(|error| anyhow::anyhow!("prepare {session_id}: {error}"))?;
    prepared
        .session
        .append_with(event_type, data, AppendOptions::default())?;
    runtime.persistence.flush().await?;
    drop(prepared);
    Ok(())
}

/// Read a session's raw event log: the live agent's session when resident,
/// otherwise the durable log (flushed first so a just-finished run's
/// write-behind window is drained).
pub(crate) async fn session_events(
    runtime: &crate::RuntimeInner,
    session_id: &str,
) -> anyhow::Result<Vec<SessionEvent>> {
    let id = SessionId(session_id.to_string());
    if let Some(agent) = runtime.agent.registry().get(&id) {
        return Ok(agent.session().events());
    }
    ensure_durable(runtime).await?;
    let inspection = runtime
        .persistence
        .coordinator()
        .load(id)
        .map_err(|error| anyhow::anyhow!("load {session_id}: {error}"))?;
    Ok(inspection.events)
}

/// WI-144: the session's plan phase from the log fold (last `plan/mode`
/// event wins; legacy `plan/phase` sessions never activate — same semantics
/// as `plan_mode::effective_plan_mode`, over the live/cold event slice).
/// True only while the planning phase is running: the approval knob is
/// locked then, and an approved exit releases it.
async fn session_plan_planning(runtime: &crate::RuntimeInner, session_id: &str) -> bool {
    match session_events(runtime, session_id).await {
        Ok(events) => {
            let mut active = false;
            for event in &events {
                if event.event_type == crate::plan_mode::PLAN_MODE_EVENT {
                    active = event
                        .data
                        .get("active")
                        .and_then(starriver_kernel::json::JsonValue::as_bool)
                        .unwrap_or(false);
                }
            }
            active
        }
        Err(error) => {
            tracing::warn!(%session_id, %error, "plan phase fold unavailable; treating as not planning");
            false
        }
    }
}

/// Cold-read ladder (spec §3.2): for every durable header with no projection
/// row, refold the log into a replacement row. Only *missing* rows participate
/// — a stale-but-present row keeps serving until a proper staleness check
/// exists (registered in the capability-alignment table; coordinator.list
/// carries no seq tail, so full validation would mean loading everything).
async fn reconcile_missing_projections(runtime: &crate::RuntimeInner) {
    if let Err(error) = ensure_durable(runtime).await {
        tracing::warn!(%error, "projection reconcile skipped: flush failed");
        return;
    }
    let headers = match runtime.persistence.coordinator().list() {
        Ok(headers) => headers,
        Err(error) => {
            tracing::warn!(%error, "projection reconcile skipped: cannot list headers");
            return;
        }
    };
    let known: std::collections::HashSet<String> = runtime
        .host_store
        .list_sessions(true)
        .into_iter()
        .map(|row| row.session_id)
        .collect();
    let missing: Vec<&SessionHeader> = headers
        .iter()
        .filter(|header| !known.contains(&header.id.0))
        .collect();
    if missing.is_empty() {
        return;
    }
    tracing::debug!(count = missing.len(), "rebuilding lost projection rows");
    for header in missing {
        let session_id = header.id.0.clone();
        let events = match runtime.persistence.coordinator().load(header.id.clone()) {
            Ok(inspection) => inspection.events,
            Err(error) => {
                tracing::warn!(session = %session_id, %error, "projection rebuild load failed");
                continue;
            }
        };
        // Sequential replay mirrors the live fold: user-pinned titles win over
        // later fallbacks; archive stores the event time or clears.
        let mut title: Option<String> = None;
        let mut title_source: Option<String> = None;
        let mut archived_at: Option<i64> = None;
        let mut message_count: u32 = 0;
        let mut watermark: i64 = 0;
        for event in &events {
            watermark = watermark.max(event.seq as i64);
            match event.event_type.as_str() {
                "session/title" => {
                    let new_title = event
                        .data
                        .get("title")
                        .and_then(|v| match v {
                            JsonValue::Null => Some(None),
                            other => other.as_str().map(|s| Some(s.to_string())),
                        })
                        .flatten();
                    let source = event
                        .data
                        .get("source")
                        .and_then(JsonValue::as_str)
                        .unwrap_or("fallback")
                        .to_string();
                    match new_title {
                        Some(new_title) => {
                            // Precedence: user pin > provider (LLM-generated)
                            // > fallback (task-text stamp). A fallback must
                            // never override provider/user — re-stamping it
                            // with every new task text churned the sidebar
                            // title across the whole session.
                            let applies = match source.as_str() {
                                "user" => true,
                                "provider" => title_source.as_deref() != Some("user"),
                                _ => title_source.is_none()
                                    || title_source.as_deref() == Some("fallback"),
                            };
                            if applies {
                                title = Some(new_title);
                                title_source = Some(source);
                            }
                        }
                        // User-sourced null clears the pin (rename(None)).
                        None if source == "user" => {
                            title = None;
                            title_source = None;
                        }
                        None => {}
                    }
                }
                "session/archived" => {
                    archived_at = event
                        .data
                        .get("archived")
                        .and_then(JsonValue::as_bool)
                        .unwrap_or(false)
                        .then_some(event.time as i64);
                }
                "assistant/message" => message_count += 1,
                _ => {}
            }
        }
        if let Err(error) = runtime.host_store.restore_session_row(
            &session_id,
            title.as_deref(),
            title_source.as_deref(),
            archived_at,
            message_count,
            watermark,
        ) {
            tracing::warn!(session = %session_id, %error, "projection rebuild write failed");
        }
    }
}

impl HostRuntime {
    /// Test hook: create a persisted kernel session carrying a title event so
    /// admin flows (rename/archive/delete/list) have a session to operate on
    /// without needing an engine-backed execute.
    pub fn seed_test_session(&self, session_id: String, title: String) -> anyhow::Result<()> {
        let inner = self.inner.clone();
        let handle = self.runtime_handle.clone();
        std::thread::spawn(move || {
            handle.block_on(async {
                let l = inner.lock().await;
                let Some(ref runtime) = *l else {
                    return Err(anyhow::anyhow!("runtime not loaded"));
                };
                let bus = runtime.bus.clone();
                let session =
                    starriver_kernel::session::Session::new(SessionId(session_id.clone()), bus);
                session.append_with(
                    "session/title",
                    JsonValue::Object(vec![
                        ("title".into(), JsonValue::String(title)),
                        ("source".into(), JsonValue::String("fallback".into())),
                    ]),
                    AppendOptions::default(),
                )?;
                runtime.persistence.flush().await?;
                Ok(())
            })
        })
        .join()
        .map_err(|_| anyhow::anyhow!("thread panicked"))??;
        Ok(())
    }

    /// Test observation hook: the durable header of a session (`cwd`,
    /// lineage), for asserting workspace-root stamping without exposing the
    /// coordinator.
    #[doc(hidden)]
    pub fn debug_session_header(&self, session_id: String) -> anyhow::Result<SessionHeader> {
        let inner = self.inner.clone();
        let handle = self.runtime_handle.clone();
        std::thread::spawn(move || {
            handle.block_on(async {
                let l = inner.lock().await;
                let Some(ref runtime) = *l else {
                    return Err(anyhow::anyhow!("runtime not loaded"));
                };
                ensure_durable(runtime).await?;
                let inspection = runtime
                    .persistence
                    .coordinator()
                    .load(SessionId(session_id.clone()))
                    .map_err(|error| anyhow::anyhow!("load {session_id}: {error}"))?;
                Ok(inspection.header)
            })
        })
        .join()
        .map_err(|_| anyhow::anyhow!("thread panicked"))?
    }

    /// Test hook (like `seed_test_session`): durably persist an event batch
    /// under a full header, including lineage fields. `fork_session` shares the
    /// same persistence route through `persist_child_log`.
    #[doc(hidden)]
    pub fn seed_test_log(
        &self,
        session_id: String,
        parent: Option<String>,
        seed_length: Option<u64>,
        events: Vec<starriver_kernel::session::SessionEvent>,
    ) -> anyhow::Result<()> {
        let inner = self.inner.clone();
        let handle = self.runtime_handle.clone();
        std::thread::spawn(move || {
            handle.block_on(async {
                let l = inner.lock().await;
                let Some(ref runtime) = *l else {
                    return Err(anyhow::anyhow!("runtime not loaded"));
                };
                persist_child_log(runtime, &session_id, parent, seed_length, None, events).await
            })
        })
        .join()
        .map_err(|_| anyhow::anyhow!("thread panicked"))??;
        Ok(())
    }

    pub fn list_sessions(&self, workspace_id: Option<String>) -> Vec<types::SessionInfo> {
        let inner = self.inner.clone();
        let handle = self.runtime_handle.clone();
        std::thread::spawn(move || {
            handle.block_on(async {
                let l = inner.lock().await;
                let Some(ref runtime) = *l else { return vec![] };
                reconcile_missing_projections(runtime).await;
                let rows = runtime.host_store.list_sessions(false);
                let headers = runtime.persistence.coordinator().list().unwrap_or_default();
                // Sort by creation time, newest first: newly created sessions appear at the top (mirroring the UI's "+ New conversation").
                let mut infos: Vec<(u64, types::SessionInfo)> = rows
                    .into_iter()
                    .filter(|row| {
                        workspace_id
                            .as_deref()
                            .is_none_or(|wid| row.workspace_id.as_deref() == Some(wid))
                    })
                    .map(|row| {
                        let created_at = headers
                            .iter()
                            .find(|header| header.id.0 == row.session_id)
                            .map(|header| header.created_at)
                            .unwrap_or(0);
                        (created_at, session_info_from(&row, &headers))
                    })
                    .collect();
                infos.sort_by(|a, b| b.0.cmp(&a.0));
                infos.into_iter().map(|(_, info)| info).collect()
            })
        })
        .join()
        .unwrap_or_default()
    }

    /// WI-107: full-text search across persisted session payloads.
    /// Backend-agnostic (list + load traversal in the persistence query
    /// layer); newest sessions first, at most `limit` hits.
    pub fn search_session_content(
        &self,
        query: String,
        limit: usize,
    ) -> Vec<serde_json::Value> {
        let inner = self.inner.clone();
        let handle = self.runtime_handle.clone();
        std::thread::spawn(move || {
            handle.block_on(async {
                let l = inner.lock().await;
                let Some(ref runtime) = *l else { return vec![] };
                let store = runtime.persistence.as_ref();
                match starriver_session_persistence::query::search_sessions(
                    store,
                    &query,
                    limit,
                    None,
                )
                .await
                {
                    Ok(hits) => hits
                        .into_iter()
                        .map(|hit| {
                            serde_json::json!({
                                "sessionId": hit.id.0,
                                "createdAt": hit.created_at,
                                "snippet": hit.snippet,
                                "matches": hit.matches,
                            })
                        })
                        .collect(),
                    Err(error) => {
                        tracing::warn!(%error, "session content search failed");
                        vec![]
                    }
                }
            })
        })
        .join()
        .unwrap_or_default()
    }

    /// WI-101: paginated history — the last `limit` messages before (or at)
    /// `before` (a surface seq); `before=None` takes the newest page.
    /// Returns (messages, total) so the UI knows whether more exist.
    pub fn get_session_messages_paged(
        &self,
        session_id: String,
        before: Option<u64>,
        limit: usize,
    ) -> (Vec<types::ChatMessage>, usize, bool) {
        let inner = self.inner.clone();
        let handle = self.runtime_handle.clone();
        std::thread::spawn(move || {
            handle.block_on(async {
                let l = inner.lock().await;
                let Some(ref runtime) = *l else { return (vec![], 0, false) };
                match load_projection_parts(runtime, &session_id).await {
                    Ok((entries, tool_names)) => {
                        let total = entries.len();
                        let (slice, has_more) = match before {
                            Some(b) => {
                                // Entries with seq < b (exclusive of the anchor).
                                let end = entries
                                    .iter()
                                    .rposition(|e| e.seq < b)
                                    .map(|i| i + 1)
                                    .unwrap_or(0);
                                let start = end.saturating_sub(limit);
                                let slice = entries
                                    .into_iter()
                                    .enumerate()
                                    .filter(|(i, _)| *i >= start && *i < end)
                                    .map(|(_, e)| e)
                                    .collect();
                                (slice, start > 0)
                            }
                            None => {
                                let start = total.saturating_sub(limit);
                                let slice = entries
                                    .into_iter()
                                    .enumerate()
                                    .filter(|(i, _)| *i >= start)
                                    .map(|(_, e)| e)
                                    .collect();
                                (slice, start > 0)
                            }
                        };
                        // BUG-86: has_more is "older entries exist beyond this
                        // page's window" — total (entries) and messages.len()
                        // (projected rows) are different dimensions and must
                        // never be compared.
                        let msgs = project_chat_messages(slice, &tool_names);
                        (msgs, total, has_more)
                    }
                    Err(error) => {
                        tracing::warn!(session = %session_id, %error, "session history unavailable");
                        (vec![], 0, false)
                    }
                }
            })
        })
        .join()
        .unwrap_or_default()
    }

    pub fn get_session_messages(&self, session_id: String) -> Vec<types::ChatMessage> {
        let inner = self.inner.clone();
        let handle = self.runtime_handle.clone();
        std::thread::spawn(move || {
            handle.block_on(async {
                let l = inner.lock().await;
                let Some(ref runtime) = *l else { return vec![] };
                match load_projection_parts(runtime, &session_id).await {
                    Ok((entries, tool_names)) => project_chat_messages(entries, &tool_names),
                    Err(error) => {
                        tracing::warn!(session = %session_id, %error, "session history unavailable");
                        vec![]
                    }
                }
            })
        })
        .join()
        .unwrap_or_default()
    }
    /// Chronological model-visible messages with their log seq as the id — the
    /// backtrack timeline. Content flattening mirrors `get_session_messages` so
    /// the two views stay consistent, and the cutoff argument of fork_session is
    /// expressed in these very seq values.
    pub fn get_session_timeline(&self, session_id: String) -> Vec<types::SessionMessage> {
        let inner = self.inner.clone();
        let handle = self.runtime_handle.clone();
        std::thread::spawn(move || {
            handle.block_on(async {
                let l = inner.lock().await;
                let Some(ref runtime) = *l else { return vec![] };
                match load_surface_messages(runtime, &session_id).await {
                    Ok(pairs) => pairs
                        .into_iter()
                        .map(|(seq, message)| types::SessionMessage {
                            id: seq as i64,
                            role: role_str(&message.role).to_string(),
                            content: flatten_blocks(&message.content),
                        })
                        .collect(),
                    Err(error) => {
                        tracing::warn!(session = %session_id, %error, "session timeline unavailable");
                        vec![]
                    }
                }
            })
        })
        .join()
        .unwrap_or_default()
    }
    /// Fork a new thread from a durable log: copy model-visible log entries
    /// with `seq < before_msg_id` into a fresh child session (lineage lives in
    /// the header: `parent_session` + `seed_length`), land it in the parent's
    /// workspace when grouped (bare projection row otherwise), and inherit the
    /// first user text as the default title when no task is given. The cutoff
    /// currency is the timeline seq; out-of-range values clamp to [0, len].
    pub fn fork_session(
        &self,
        source_id: String,
        before_msg_id: i64,
        task: String,
    ) -> anyhow::Result<types::SessionInfo> {
        let inner = self.inner.clone();
        let handle = self.runtime_handle.clone();
        let result = std::thread::spawn(move || {
            handle.block_on(async {
                let l = inner.lock().await;
                let Some(ref runtime) = *l else {
                    return Err(anyhow::anyhow!("runtime not loaded"));
                };
                // Durability barrier first: the source may have just finished a
                // live run whose events are still inside the write-behind window.
                ensure_durable(runtime).await?;
                let inspection = runtime
                    .persistence
                    .coordinator()
                    .load(SessionId(source_id.clone()))
                    .map_err(|error| anyhow::anyhow!("load source {source_id}: {error}"))?;
                let cutoff = ((before_msg_id.max(0)) as usize).min(inspection.events.len());
                let seed = inspection.events[..cutoff].to_vec();

                // Default title falls back to the first user text in the cut.
                let inherited_title = seed
                    .iter()
                    .find(|event| event.event_type == "user/message")
                    .and_then(|event| KernelMessage::from_json(event.data.clone()).ok())
                    .map(|message| flatten_blocks(&message.content))
                    .filter(|text| !text.trim().is_empty())
                    .unwrap_or_else(|| "Forked conversation".into());
                let effective_task = if task.trim().is_empty() {
                    inherited_title
                } else {
                    task
                };

                let child = uuid::Uuid::new_v4().to_string();
                let message_count = {
                    let probe = starriver_kernel::session::Session::new(
                        SessionId(child.clone()),
                        cordis_rslite::EventBus::new(),
                    )
                    .with_header(starriver_kernel::session::SessionHeader::new(SessionId(
                        child.clone(),
                    )))
                    .map_err(|error| anyhow::anyhow!("probe header: {error}"))?
                    .with_seed(seed.clone())
                    .map_err(|error| anyhow::anyhow!("probe seed: {error}"))?;
                    probe.surface().nodes.len() as u32
                };

                persist_child_log(
                    runtime,
                    &child,
                    Some(source_id.clone()),
                    Some(seed.len() as u64),
                    inspection.header.cwd.clone(),
                    seed,
                )
                .await?;

                let parent_row = runtime.host_store.get(&source_id);
                let parent_workspace = parent_row.as_ref().and_then(|row| row.workspace_id.clone());
                let parent_agent = parent_row.as_ref().and_then(|row| row.agent.clone());
                match parent_workspace.as_deref() {
                    Some(workspace) => runtime
                        .host_store
                        .attach_session(workspace, &child)
                        .map_err(|error| anyhow::anyhow!("attach forked child: {error}"))?,
                    None => runtime
                        .host_store
                        .ensure_session_row(&child)
                        .map_err(|error| anyhow::anyhow!("register forked child: {error}"))?,
                }

                Ok(types::SessionInfo {
                    id: child,
                    title: truncate_title(&effective_task, 40),
                    workspace_id: parent_workspace,
                    created_at: format_time(
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs() as i64)
                            .unwrap_or(0),
                    ),
                    message_count,
                    parent_id: Some(source_id),
                    // The clamped cutoff — the same value the header
                    // persists and list_sessions reads back (BUG-64).
                    fork_point: Some(cutoff as i64),
                    agent: parent_agent,
                    origin: None,
                })
            })
        })
        .join()
        .map_err(|_| anyhow::anyhow!("thread panicked"))??;
        Ok(result)
    }
    #[api_method("session.delete", permissions = ["session:write"])]
    pub fn delete_session(&self, session_id: String) -> anyhow::Result<()> {
        let inner = self.inner.clone();
        let handle = self.runtime_handle.clone();
        let sid = session_id.clone();
        // BUG-13: deletion failures propagate — the UI must not be told a
        // session is gone when the durable log or host store kept it.
        // NotFound stays Ok (deleting an unknown id is idempotent).
        let outcome: anyhow::Result<()> = std::thread::spawn(move || {
            handle.block_on(async {
                let l = inner.lock().await;
                let Some(ref runtime) = *l else {
                    return Ok(());
                };
                if let Err(e) = runtime
                    .persistence
                    .coordinator()
                    .remove(SessionId(sid.clone()))
                {
                    match e {
                        starriver_session_persistence::coordinator::CoordinatorError::Persistence(
                            starriver_kernel::persistence::PersistenceError::NotFound(_),
                        ) => {}
                        other => return Err(anyhow::anyhow!("remove session log: {other}")),
                    }
                }
                runtime
                    .host_store
                    .delete_session(&sid)
                    .map_err(|e| anyhow::anyhow!("remove session metadata: {e}"))
            })
        })
        .join()
        .map_err(|_| anyhow::anyhow!("delete thread panicked"))?;
        outcome
    }

    /// /name: name the current session (Some sets an override title, None clears back to task-derived).
    #[api_method("session.rename", permissions = ["session:write"])]
    pub fn rename_session(&self, session_id: String, title: Option<String>) -> anyhow::Result<()> {
        if let Some(title) = title.as_ref() {
            if crate::host_store::HostStore::is_reserved_group_name(title) {
                return Err(anyhow::anyhow!(
                    "`{title}` is a reserved sidebar name — pick another title"
                ));
            }
        }
        let inner = self.inner.clone();
        let handle = self.runtime_handle.clone();
        std::thread::spawn(move || {
            handle.block_on(async {
                let l = inner.lock().await;
                if let Some(ref runtime) = *l {
                    // None clears the override: a user-sourced title event with
                    // a null title unpins the row so task-derived fallback
                    // titles apply again (mirrors /name's documented contract).
                    let title_event = match title {
                        Some(title) => JsonValue::String(title),
                        None => JsonValue::Null,
                    };
                    let _ = append_session_event(
                        runtime,
                        &session_id,
                        "session/title",
                        JsonValue::Object(vec![
                            ("title".into(), title_event),
                            ("source".into(), JsonValue::String("user".into())),
                        ]),
                    )
                    .await;
                }
            })
        })
        .join()
        .ok();
        Ok(())
    }

    /// /archive: soft-archive the session (hidden from the default list, restorable via /unarchive, no physical deletion).
    #[api_method("session.archive", permissions = ["session:write"])]
    pub fn archive_session(&self, session_id: String) -> anyhow::Result<()> {
        self.set_session_archived(session_id, true)
    }

    /// /unarchive: restore an archived session.
    #[api_method("session.unarchive", permissions = ["session:write"])]
    pub fn unarchive_session(&self, session_id: String) -> anyhow::Result<()> {
        self.set_session_archived(session_id, false)
    }

    /// /attachWorkspace: re-home a session under a live workspace — the
    /// recovery path for sessions orphaned by workspace deletion — and
    /// unarchive it, so it renders again in the target workspace group.
    #[api_method("session.attachWorkspace", permissions = ["session:write"])]
    pub fn attach_session_workspace(
        &self,
        session_id: String,
        workspace_id: String,
    ) -> anyhow::Result<()> {
        self.unarchive_session(session_id.clone())?;
        let inner = self.inner.clone();
        let handle = self.runtime_handle.clone();
        std::thread::spawn(move || {
            handle.block_on(async {
                let l = inner.lock().await;
                match l.as_ref() {
                    Some(runtime) => runtime
                        .host_store
                        .attach_session(&workspace_id, &session_id)
                        .map_err(|e| anyhow::anyhow!("attach failed: {e}"))?,
                    // A skipped attach used to be a silent Ok — the caller's
                    // restore click would vanish without effect.
                    None => return Err(anyhow::anyhow!("runtime not loaded")),
                }
                Ok::<(), anyhow::Error>(())
            })
        })
        .join()
        .map_err(|_| anyhow::anyhow!("thread panicked"))?
    }

    fn set_session_archived(&self, session_id: String, archived: bool) -> anyhow::Result<()> {
        let inner = self.inner.clone();
        let handle = self.runtime_handle.clone();
        std::thread::spawn(move || {
            handle.block_on(async {
                let l = inner.lock().await;
                match l.as_ref() {
                    Some(runtime) => append_session_event(
                        runtime,
                        &session_id,
                        "session/archived",
                        JsonValue::Object(vec![("archived".into(), JsonValue::Bool(archived))]),
                    )
                    .await
                    .map_err(|e| anyhow::anyhow!("archive event failed: {e}")),
                    None => Err(anyhow::anyhow!("runtime not loaded")),
                }
            })
        })
        .join()
        .map_err(|_| anyhow::anyhow!("thread panicked"))?
    }

    /// Data source for the /unarchive picker: archived sessions only.
    pub fn list_archived_sessions(&self) -> Vec<types::SessionInfo> {
        let inner = self.inner.clone();
        let handle = self.runtime_handle.clone();
        std::thread::spawn(move || {
            handle.block_on(async {
                let l = inner.lock().await;
                let Some(ref runtime) = *l else { return vec![] };
                reconcile_missing_projections(runtime).await;
                let rows = runtime.host_store.list_sessions(true);
                let headers = runtime.persistence.coordinator().list().unwrap_or_default();
                let mut infos: Vec<(u64, types::SessionInfo)> = rows
                    .into_iter()
                    .filter(|row| row.archived_at.is_some())
                    .map(|row| {
                        let created_at = headers
                            .iter()
                            .find(|header| header.id.0 == row.session_id)
                            .map(|header| header.created_at)
                            .unwrap_or(0);
                        (created_at, session_info_from(&row, &headers))
                    })
                    .collect();
                infos.sort_by(|a, b| b.0.cmp(&a.0));
                infos.into_iter().map(|(_, info)| info).collect()
            })
        })
        .join()
        .unwrap_or_default()
    }

    pub fn list_workspaces(&self) -> Vec<types::WorkspaceInfo> {
        let inner = self.inner.clone();
        let handle = self.runtime_handle.clone();
        std::thread::spawn(move || {
            handle.block_on(async {
                let l = inner.lock().await;
                let Some(ref runtime) = *l else { return vec![] };
                runtime
                    .host_store
                    .list_workspaces()
                    .into_iter()
                    .map(|workspace| types::WorkspaceInfo {
                        id: workspace.id,
                        path: workspace.path,
                        title: workspace.title,
                        created_at: format_time(workspace.created_at / 1000),
                        session_ids: workspace.session_ids,
                    })
                    .collect()
            })
        })
        .join()
        .unwrap_or_default()
    }

    fn workspace_info(&self, id: String) -> Option<types::WorkspaceInfo> {
        let inner = self.inner.clone();
        let handle = self.runtime_handle.clone();
        std::thread::spawn(move || {
            handle.block_on(async {
                let l = inner.lock().await;
                let runtime = (*l).as_ref()?;
                runtime
                    .host_store
                    .list_workspaces()
                    .into_iter()
                    .find(|workspace| workspace.id == id)
                    .map(|workspace| types::WorkspaceInfo {
                        id: workspace.id,
                        path: workspace.path,
                        title: workspace.title,
                        created_at: format_time(workspace.created_at / 1000),
                        session_ids: workspace.session_ids,
                    })
            })
        })
        .join()
        .ok()
        .flatten()
    }

    pub fn create_workspace(&self, path: String) -> anyhow::Result<types::WorkspaceInfo> {
        let inner = self.inner.clone();
        let handle = self.runtime_handle.clone();
        let id = std::thread::spawn(move || {
            handle.block_on(async {
                let l = inner.lock().await;
                let Some(ref runtime) = *l else {
                    return Err(anyhow::anyhow!("runtime not loaded"));
                };
                let (id, _) = runtime
                    .host_store
                    .resolve_workspace(&path)
                    .map_err(|e| anyhow::anyhow!(e))?;
                Ok::<_, anyhow::Error>(id)
            })
        })
        .join()
        .map_err(|_| anyhow::anyhow!("thread panicked"))??;
        self.workspace_info(id)
            .ok_or_else(|| anyhow::anyhow!("workspace not found"))
    }

    pub fn rename_workspace(&self, id: String, title: String) -> anyhow::Result<()> {
        let inner = self.inner.clone();
        let handle = self.runtime_handle.clone();
        // Validation errors (empty title / unknown id) propagate to the caller.
        std::thread::spawn(move || {
            handle.block_on(async {
                let l = inner.lock().await;
                match l.as_ref() {
                    Some(runtime) => runtime
                        .host_store
                        .rename_workspace(&id, &title)
                        .map_err(|e| anyhow::anyhow!(e)),
                    None => Err(anyhow::anyhow!("runtime not loaded")),
                }
            })
        })
        .join()
        .map_err(|_| anyhow::anyhow!("thread panicked"))?
    }

    #[api_method("workspaces.delete", permissions = ["workspaces:write"])]
    pub fn delete_workspace(&self, workspace_id: String) -> anyhow::Result<()> {
        let inner = self.inner.clone();
        let handle = self.runtime_handle.clone();
        let result = std::thread::spawn(move || {
            handle.block_on(async {
                let l = inner.lock().await;
                if let Some(ref runtime) = *l {
                    runtime
                        .host_store
                        .delete_workspace(&workspace_id)
                        .map_err(|e| anyhow::anyhow!("{e}"))
                } else {
                    Ok(())
                }
            })
        })
        .join()
        .map_err(|e| anyhow::anyhow!("thread panicked: {e:?}"))?;
        result
    }

    /// UI worktree-isolation toggle (`/worktree`): `Some(flag)` overrides the
    /// agent's profile default for the runs that follow; the effective flag is
    /// read per execute. Until `execute` carries isolation per call, this is
    /// the toggle's whole state.
    pub fn get_worktree_status(&self, cwd: String) -> types::WorktreeStatus {
        let s = starriver_tools::worktree::status(std::path::Path::new(&cwd));
        types::WorktreeStatus {
            is_repo: s.is_repo,
            branch: s.branch,
            is_worktree: s.is_worktree,
            worktree_path: s.worktree_path.map(|p| p.to_string_lossy().into_owned()),
            dirty: s.dirty,
        }
    }
}

#[cfg(test)]
mod projection_tests {
    //! Pure-function boundary matrix for the Option 2 projection (duration derivation + per-turn token folding + doneTs).

    fn project(entries: Vec<SurfaceEntry>) -> Vec<types::ChatMessage> {
        project_with(entries, HashMap::new())
    }

    #[test]
    fn turn_end_error_surfaces_as_system_row() {
        // The failed turn's reason (kind=error + failure.message) must project into a system row;
        // otherwise failures vanish on history reload and consecutive failed inputs get mis-merged into one bubble.
        let failure = JsonValue::Object(vec![
            ("kind".into(), JsonValue::String("error".into())),
            ("failure".into(), JsonValue::Object(vec![(
                "message".into(),
                JsonValue::String("Insufficient balance (code 1113)".into()),
            )])),
        ]);
        let entries = vec![
            user_entry(1),
            SurfaceEntry {
                seq: 9,
                time: 2,
                event_type: "turn/end".into(),
                usage: None,
                message: None,
                meta: None,
                data: JsonValue::Object(vec![("reason".into(), failure)]),
            },
        ];
        let out = project(entries);
        assert!(
            out.iter()
                .any(|m| m.role == "system" && m.content.contains("Insufficient balance")),
            "failed turn must surface its error: {out:?}"
        );
        // Completed turns add no system row.
        let completed = JsonValue::Object(vec![(
            "reason".into(),
            JsonValue::Object(vec![("kind".into(), JsonValue::String("completed".into()))]),
        )]);
        let ok = project(vec![
            user_entry(1),
            SurfaceEntry {
                seq: 10,
                time: 3,
                event_type: "turn/end".into(),
                usage: None,
                message: None,
                meta: None,
                data: completed,
            },
        ]);
        assert!(
            ok.iter().all(|m| m.role != "system"),
            "completed turn must not add a system row: {ok:?}"
        );
    }

    fn project_with(
        entries: Vec<SurfaceEntry>,
        tool_names: HashMap<String, String>,
    ) -> Vec<types::ChatMessage> {
        project_chat_messages(entries, &tool_names)
    }

    use super::*;
    use starriver_kernel::llm::{CallId, MessageId, MessageSource};

    fn assistant_message(text: &str) -> Option<KernelMessage> {
        Some(KernelMessage {
            id: MessageId("m".into()),
            role: MessageRole::Assistant,
            content: vec![ContentBlock::Text(text.into())],
            source: MessageSource::Model {
                provider: "p".into(),
                model: "m".into(),
                replay_state: None,
            },
        })
    }

    fn tool_call_message() -> Option<KernelMessage> {
        Some(KernelMessage {
            id: MessageId("m".into()),
            role: MessageRole::Assistant,
            content: vec![ContentBlock::ToolCall {
                id: CallId("c".into()),
                name: Some("bash".into()),
                arguments: JsonValue::Object(Vec::new()),
            }],
            source: MessageSource::Model {
                provider: "p".into(),
                model: "m".into(),
                replay_state: None,
            },
        })
    }

    #[test]
    fn user_image_blocks_project_to_markdown_refs() {
        // WI-1 P1: reference blocks (post-P0 logs) render back as markdown
        // image lines so the transcript keeps showing the image; blocks
        // without a fileId (legacy inline) stay dropped.
        let image_with_ref = SurfaceEntry {
            seq: 0,
            time: 1,
            event_type: "user/message".into(),
            usage: None,
            data: JsonValue::Null,
            message: Some(KernelMessage {
                id: MessageId("m".into()),
                role: MessageRole::User,
                content: vec![
                    ContentBlock::Text("look at this".into()),
                    ContentBlock::Image {
                        attachment: JsonValue::Object(vec![
                            ("fileId".into(), JsonValue::String("sha256:abc123".into())),
                            ("mediaType".into(), JsonValue::String("image/png".into())),
                            ("summary".into(), JsonValue::String("报表".into())),
                        ]),
                    },
                ],
                source: MessageSource::User,
            }),
            meta: None,
        };
        let legacy_inline = SurfaceEntry {
            seq: 1,
            time: 2,
            event_type: "user/message".into(),
            usage: None,
            data: JsonValue::Null,
            message: Some(KernelMessage {
                id: MessageId("m2".into()),
                role: MessageRole::User,
                content: vec![ContentBlock::Image {
                    attachment: JsonValue::Object(vec![(
                        "url".into(),
                        JsonValue::String("data:image/png;base64,AAAA".into()),
                    )]),
                }],
                source: MessageSource::User,
            }),
            meta: None,
        };
        let out = project(vec![image_with_ref, legacy_inline]);
        let texts: Vec<&str> = out.iter().map(|m| m.content.as_str()).collect();
        assert!(texts.contains(&"look at this"));
        assert!(
            texts
                .iter()
                .any(|t| *t == "![图片 image/png](sha256:abc123)"),
            "{texts:?}"
        );
        assert_eq!(out.len(), 2, "legacy inline block stays dropped: {texts:?}");
    }

    fn user_entry(time: u64) -> SurfaceEntry {
        SurfaceEntry {
            seq: 0,
            time,
            event_type: "user/message".into(),
            usage: None,
            data: JsonValue::Null,
            message: Some(KernelMessage {
                id: MessageId("m".into()),
                role: MessageRole::User,
                content: vec![ContentBlock::Text("task".into())],
                source: MessageSource::User,
            }),
            meta: None,
        }
    }

    fn assistant_entry(time: u64, text: &str, usage: Option<(u64, u64)>) -> SurfaceEntry {
        SurfaceEntry {
            seq: 0,
            time,
            event_type: "assistant/message".into(),
            data: JsonValue::Null,
            usage,
            message: assistant_message(text),
            meta: None,
        }
    }

    fn anchor_entry(time: u64, usage: (u64, u64)) -> SurfaceEntry {
        SurfaceEntry {
            seq: 0,
            time,
            event_type: "assistant/message".into(),
            data: JsonValue::Null,
            usage: Some(usage),
            message: None,
            meta: None,
        }
    }

    fn end_entry(time: u64) -> SurfaceEntry {
        SurfaceEntry {
            seq: 0,
            time,
            event_type: "turn/end".into(),
            data: JsonValue::Null,
            usage: None,
            message: None,
            meta: None,
        }
    }

    // ── File-change summary (pure projection) ──

    fn tool_result_entry(time: u64, call_id: &str, text: &str, is_error: bool) -> SurfaceEntry {
        SurfaceEntry {
            seq: 0,
            time,
            event_type: "tool/result".into(),
            data: JsonValue::Null,
            usage: None,
            message: Some(KernelMessage {
                id: MessageId("m".into()),
                role: MessageRole::User,
                content: vec![ContentBlock::ToolResult {
                    call_id: CallId(call_id.into()),
                    content: vec![ContentBlock::Text(text.into())],
                    is_error,
                }],
                source: MessageSource::User,
            }),
            meta: None,
        }
    }

    fn names(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(c, n)| (c.to_string(), n.to_string()))
            .collect()
    }

    const EDIT_OUTPUT: &str = "edited /tmp/a.txt\n\n--- /tmp/a.txt\n+++ /tmp/a.txt\n@@ -1,2 +1,2 @@\n old\n-old line\n+new line\n more";

    // ── Trajectory projection (project_trace_entries) ──

    use crate::types::SessionTraceEntry;

    fn project_trace(entries: Vec<SurfaceEntry>) -> Vec<SessionTraceEntry> {
        project_trace_entries(entries, &HashMap::new())
    }

    fn trace_user(time: u64, text: &str) -> SurfaceEntry {
        SurfaceEntry {
            seq: 0,
            time,
            event_type: "user/message".into(),
            usage: None,
            data: JsonValue::Null,
            message: Some(KernelMessage {
                id: MessageId("m".into()),
                role: MessageRole::User,
                content: vec![ContentBlock::Text(text.into())],
                source: MessageSource::User,
            }),
            meta: None,
        }
    }

    fn trace_assistant(
        time: u64,
        blocks: Vec<ContentBlock>,
        turn: Option<u32>,
        usage: Option<(u64, u64)>,
        meta: Option<SurfaceMeta>,
    ) -> SurfaceEntry {
        SurfaceEntry {
            seq: 7,
            time,
            event_type: "assistant/message".into(),
            data: JsonValue::Null,
            usage,
            message: Some(KernelMessage {
                id: MessageId("m".into()),
                role: MessageRole::Assistant,
                content: blocks,
                source: MessageSource::Model {
                    provider: "p".into(),
                    model: "m".into(),
                    replay_state: None,
                },
            }),
            meta,
        }
    }

    fn header_entry(
        seq: u64,
        reason: &str,
        system: &str,
        provider: &str,
        model: &str,
    ) -> SurfaceEntry {
        SurfaceEntry {
            seq,
            time: seq * 10,
            event_type: "request/header".into(),
            data: JsonValue::Null,
            usage: None,
            message: None,
            meta: Some(SurfaceMeta {
                header_reason: Some(reason.into()),
                system: Some(system.into()),
                provider: Some(provider.into()),
                model: Some(model.into()),
                ..Default::default()
            }),
        }
    }

    fn plugin_entry_with_form(
        plugin: &str,
        form: Option<starriver_kernel::llm::ContextForm>,
        text: &str,
    ) -> SurfaceEntry {
        SurfaceEntry {
            seq: 0,
            time: 900,
            event_type: "user/message".into(),
            usage: None,
            data: JsonValue::Null,
            message: Some(KernelMessage {
                id: MessageId("mp".into()),
                role: MessageRole::User,
                content: vec![ContentBlock::Text(text.into())],
                source: MessageSource::Plugin {
                    plugin: plugin.into(),
                    form,
                },
            }),
            meta: None,
        }
    }

    #[test]
    fn trace_header_reason_change_gets_changed_label() {
        let out = project_trace(vec![header_entry(1, "change", "SYS", "prov", "mod")]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, "system");
        assert!(
            out[0].text.starts_with("[system prompt (changed)]"),
            "{}",
            out[0].text
        );
        assert!(out[0].text.contains("SYS"));
        assert_eq!(out[0].seq, 1.0);
        assert_eq!(out[0].time_ms, 10.0);
    }

    #[test]
    fn trace_header_without_system_text_still_renders_default() {
        // Host-side merge drops headers without a system string, but the pure
        // projection tolerates an empty one (belt and braces).
        let out = project_trace(vec![header_entry(1, "initial", "", "p", "m")]);
        assert_eq!(out[0].text, "[system prompt]\n\n");
    }

    #[test]
    fn trace_assistant_without_turn_step_still_renders() {
        let out = project_trace(vec![trace_assistant(
            1_000,
            vec![ContentBlock::Text("ok".into())],
            None,
            None,
            None,
        )]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, "assistant");
        assert_eq!(out[0].text, "ok");
        assert_eq!(out[0].turn, None);
        assert_eq!(out[0].step, None);
    }

    #[test]
    fn trace_meta_fields_flow_into_assistant_rows() {
        let meta = SurfaceMeta {
            turn: Some(2),
            step: Some(3),
            first_chunk_ms: Some(1_200),
            step_start_ms: Some(1_100),
            provider: Some("deepseek".into()),
            model: Some("deepseek-chat".into()),
            ..Default::default()
        };
        let out = project_trace(vec![trace_assistant(
            1_500,
            vec![ContentBlock::Text("done".into())],
            Some(2),
            Some((100, 20)),
            Some(meta),
        )]);
        assert_eq!(out[0].turn, Some(2.0));
        assert_eq!(out[0].step, Some(3.0));
        assert_eq!(out[0].provider.as_deref(), Some("deepseek"));
        assert_eq!(out[0].model.as_deref(), Some("deepseek-chat"));
        assert_eq!(out[0].input_tokens, Some(100.0));
        assert_eq!(out[0].output_tokens, Some(20.0));
        // TTFT = first chunk (1200) − step start (1100); duration = append (1500) − first chunk.
        assert_eq!(out[0].ttft_ms, Some(100.0));
        assert_eq!(out[0].duration_ms, Some(300.0));
    }

    #[test]
    fn trace_first_chunk_without_step_start_zeroes_ttft() {
        let meta = SurfaceMeta {
            first_chunk_ms: Some(1_200),
            step_start_ms: None,
            ..Default::default()
        };
        let out = project_trace(vec![trace_assistant(
            1_500,
            vec![ContentBlock::Text("x".into())],
            Some(1),
            None,
            Some(meta),
        )]);
        assert_eq!(
            out[0].ttft_ms,
            Some(0.0),
            "no step/start anchor → TTFT baseline 0"
        );
        assert_eq!(out[0].duration_ms, Some(300.0));
    }

    #[test]
    fn trace_usage_applies_only_to_assistant_text_rows() {
        let out = project_trace(vec![trace_assistant(
            1_000,
            vec![
                ContentBlock::Reasoning("think".into()),
                ContentBlock::Text("answer".into()),
            ],
            Some(1),
            Some((50, 10)),
            None,
        )]);
        assert_eq!(out[0].kind, "thinking");
        assert_eq!(
            out[0].input_tokens, None,
            "reasoning row must not carry usage"
        );
        assert_eq!(out[1].input_tokens, Some(50.0));
    }

    #[test]
    fn trace_tool_result_ok_and_turn_step() {
        let e = SurfaceEntry {
            seq: 9,
            time: 2_000,
            event_type: "tool/result".into(),
            data: JsonValue::Null,
            usage: None,
            message: Some(KernelMessage {
                id: MessageId("m".into()),
                role: MessageRole::User,
                content: vec![ContentBlock::ToolResult {
                    call_id: CallId("c".into()),
                    content: vec![ContentBlock::Text("out".into())],
                    is_error: false,
                }],
                source: MessageSource::User,
            }),
            meta: Some(SurfaceMeta {
                turn: Some(1),
                step: Some(2),
                ..Default::default()
            }),
        };
        let out = project_trace(vec![e]);
        assert_eq!(out[0].kind, "tool_result");
        assert_eq!(out[0].tool_ok, Some(true), "success side of the red line");
        assert_eq!(out[0].tool_output.as_deref(), Some("out"));
        assert_eq!(out[0].turn, Some(1.0));
        assert_eq!(out[0].step, Some(2.0));
    }

    #[test]
    fn trace_tool_call_prefers_inline_name_over_registry() {
        let inline = SurfaceEntry {
            seq: 1,
            time: 1_000,
            event_type: "assistant/message".into(),
            data: JsonValue::Null,
            usage: None,
            message: Some(KernelMessage {
                id: MessageId("m".into()),
                role: MessageRole::Assistant,
                content: vec![ContentBlock::ToolCall {
                    id: CallId("c1".into()),
                    name: Some("bash".into()),
                    arguments: JsonValue::Object(Vec::new()),
                }],
                source: MessageSource::Model {
                    provider: "p".into(),
                    model: "m".into(),
                    replay_state: None,
                },
            }),
            meta: None,
        };
        let mut registry = HashMap::new();
        registry.insert("c1".to_string(), "should-not-win".to_string());
        let out = project_trace_entries(vec![inline], &registry);
        assert_eq!(out[0].tool_name.as_deref(), Some("bash"));
    }

    #[test]
    fn trace_context_notice_and_formless_variants() {
        let notice = plugin_entry_with_form(
            "approval-policy",
            Some(starriver_kernel::llm::ContextForm::Notice {
                summary: "workspace-write".into(),
            }),
            "ignored body",
        );
        let formless = plugin_entry_with_form("time-context", None, "time body");
        let out = project_trace(vec![notice, formless]);
        assert_eq!(out[0].kind, "context");
        assert_eq!(out[0].text, "[approval-policy] workspace-write");
        assert_eq!(out[0].tool_name.as_deref(), Some("approval-policy"));
        assert_eq!(
            out[1].text, "[time-context]",
            "formless falls back to bare tag"
        );
    }

    #[test]
    fn trace_context_snapshot_renders_sections() {
        let e = plugin_entry_with_form(
            "runtime-context",
            Some(starriver_kernel::llm::ContextForm::Snapshot {
                sections: vec![
                    starriver_kernel::llm::ContextSnapshotSection {
                        name: "workspace".into(),
                        text: "/tmp/demo".into(),
                    },
                    starriver_kernel::llm::ContextSnapshotSection {
                        name: "sandbox".into(),
                        text: "workspace-write".into(),
                    },
                ],
                truncated: false,
                budget_bytes: 0,
                composed_bytes: 0,
            }),
            "fallback",
        );
        let out = project_trace(vec![e]);
        assert!(out[0].text.contains("[runtime-context]"));
        assert!(out[0].text.contains("## workspace\n/tmp/demo"));
        assert!(out[0].text.contains("## sandbox\nworkspace-write"));
    }

    #[test]
    fn trace_skips_usage_anchors_and_turn_ends() {
        let out = project_trace(vec![anchor_entry(1_600, (5, 5)), end_entry(1_700)]);
        assert!(out.is_empty(), "message-less entries produce no trace rows");
    }

    #[test]
    fn trace_second_user_message_opens_second_turn() {
        let out = project_trace(vec![
            trace_user(1_000, "first"),
            trace_assistant(
                1_100,
                vec![ContentBlock::Text("a".into())],
                None,
                None,
                None,
            ),
            trace_user(2_000, "second"),
        ]);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].kind, "user");
        assert_eq!(out[2].kind, "user");
    }

    #[test]
    fn trace_registry_miss_yields_empty_tool_name() {
        let e = SurfaceEntry {
            seq: 1,
            time: 1_000,
            event_type: "assistant/message".into(),
            data: JsonValue::Null,
            usage: None,
            message: Some(KernelMessage {
                id: MessageId("m".into()),
                role: MessageRole::Assistant,
                content: vec![ContentBlock::ToolCall {
                    id: CallId("unknown".into()),
                    name: None,
                    arguments: JsonValue::Object(Vec::new()),
                }],
                source: MessageSource::Model {
                    provider: "p".into(),
                    model: "m".into(),
                    replay_state: None,
                },
            }),
            meta: None,
        };
        let out = project_trace(vec![e]);
        assert_eq!(
            out[0].tool_name.as_deref(),
            Some(""),
            "registry miss degrades to empty, not panic"
        );
    }

    #[test]
    fn trace_initial_system_prompt_is_hoisted_to_the_front() {
        // Real ledger order: the first request/header lands AFTER turn 1's
        // user message (it is written at first-request time, not session
        // creation). The projection must still pin it to the top, while a
        // later "(changed)" header keeps its chronological position.
        let user_before_header = trace_user(1_000, "u1");
        let header = SurfaceEntry {
            seq: 9,
            time: 1_500,
            event_type: "request/header".into(),
            data: JsonValue::Null,
            usage: None,
            message: None,
            meta: Some(SurfaceMeta {
                header_reason: Some("initial".into()),
                system: Some("You are Starriver.".into()),
                ..Default::default()
            }),
        };
        let changed = SurfaceEntry {
            seq: 20,
            time: 2_500,
            event_type: "request/header".into(),
            data: JsonValue::Null,
            usage: None,
            message: None,
            meta: Some(SurfaceMeta {
                header_reason: Some("change".into()),
                system: Some("You are Starriver, v2.".into()),
                ..Default::default()
            }),
        };
        let entries = vec![
            user_before_header,
            trace_assistant(
                1_400,
                vec![ContentBlock::Text("a".into())],
                None,
                None,
                None,
            ),
            header,
            trace_assistant(
                2_000,
                vec![ContentBlock::Text("b".into())],
                None,
                None,
                None,
            ),
            changed,
        ];
        let out = project_trace(entries);
        assert_eq!(out[0].kind, "system");
        assert!(out[0].text.contains("You are Starriver."));
        assert!(
            !out[0].text.contains("changed"),
            "initial label, not the change one"
        );
        // The changed header stays mid-stream where it happened.
        let changed_pos = out.iter().position(|e| e.text.contains("v2")).unwrap();
        assert!(
            changed_pos > 1,
            "changed header keeps chronology, at {}",
            changed_pos
        );
        // Assistant rows keep their relative order after the hoist.
        let texts: Vec<&str> = out
            .iter()
            .filter(|e| e.kind == "assistant")
            .map(|e| e.text.as_str())
            .collect();
        assert_eq!(texts, vec!["a", "b"]);
    }

    #[test]
    fn trace_plugin_snapshot_user_messages_project_as_context_not_user() {
        let out = project_trace(vec![
            plugin_entry_with_form(
                "runtime-context",
                Some(starriver_kernel::llm::ContextForm::Snapshot {
                    sections: vec![starriver_kernel::llm::ContextSnapshotSection {
                        name: "workspace".into(),
                        text: "/tmp/demo".into(),
                    }],
                    truncated: false,
                    budget_bytes: 0,
                    composed_bytes: 0,
                }),
                "snapshot body",
            ),
            trace_user(1_000, "real user input"),
        ]);
        assert_eq!(
            out[0].kind, "context",
            "plugin snapshot is context, not user"
        );
        assert_eq!(out[1].kind, "user", "real user input follows");
    }

    #[test]
    fn plugin_context_user_messages_are_not_user_content() {
        // runtime-context snapshots / time-context samples are model-visible
        // plumbing messages: keep them out of the history timeline and skip them
        // for the turn-start anchor, which must land on real user input.
        fn plugin_entry(time: u64, text: &str) -> SurfaceEntry {
            SurfaceEntry {
                seq: 0,
                time,
                event_type: "user/message".into(),
                usage: None,
                message: Some(KernelMessage {
                    id: MessageId("m".into()),
                    role: MessageRole::User,
                    content: vec![ContentBlock::Text(text.into())],
                    source: MessageSource::Plugin {
                        plugin: "time-context".into(),
                        form: None,
                    },
                }),
                meta: None,
            data: JsonValue::Null,
            }
        }

        let entries = vec![
            plugin_entry(1_000, "Time sampled while preparing turn 1, step 1: x"),
            user_entry(1_100),
            assistant_entry(1_500, "done", None),
        ];
        let out = project(entries);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].role, "user");
        assert_eq!(out[0].content, "task");
        assert_eq!(out[1].role, "assistant");
    }

    #[test]
    fn reasoning_blocks_project_as_thinking_rows() {
        let entries = vec![SurfaceEntry {
            seq: 0,
            time: 1_000,
            event_type: "assistant/message".into(),
            usage: None,
            message: Some(KernelMessage {
                id: MessageId("m".into()),
                role: MessageRole::Assistant,
                content: vec![
                    ContentBlock::Reasoning("先看目录结构".into()),
                    ContentBlock::Text("看完了".into()),
                ],
                source: MessageSource::Model {
                    provider: "p".into(),
                    model: "m".into(),
                    replay_state: None,
                },
            }),
            meta: None,
        data: JsonValue::Null,
        }];
        let out = project(entries);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].role, "thinking");
        assert_eq!(out[0].content, "先看目录结构");
        assert_eq!(out[1].role, "assistant");
    }

    #[test]
    fn file_changes_fold_and_backfill_to_final_piece() {
        let entries = vec![
            user_entry(1_000),
            tool_result_entry(1_100, "c1", EDIT_OUTPUT, false),
            assistant_entry(1_200, "all done", Some((50, 20))),
            end_entry(1_400),
        ];
        let out = project_with(entries, names(&[("c1", "edit")]));
        let last = out.last().unwrap();
        let files = last.file_changes.as_ref().expect("汇总回填最终文本 piece");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "/tmp/a.txt");
        assert_eq!((files[0].add, files[0].del), (1, 1));
        assert!(files[0].diff.starts_with("--- /tmp/a.txt"));
        assert!(files[0].diff.contains("@@ -1,2 +1,2 @@"));
    }

    #[test]
    fn non_edit_tools_and_failed_edits_are_ignored() {
        let entries = vec![
            user_entry(1_000),
            tool_result_entry(1_050, "c-bash", EDIT_OUTPUT, false),
            tool_result_entry(1_060, "c-edit", EDIT_OUTPUT, true),
            assistant_entry(1_100, "done", None),
            end_entry(1_300),
        ];
        let out = project_with(entries, names(&[("c-bash", "bash"), ("c-edit", "edit")]));
        assert!(out.last().unwrap().file_changes.is_none());
    }

    #[test]
    fn same_file_latest_edit_wins() {
        let entries = vec![
            user_entry(1_000),
            tool_result_entry(1_050, "c1", EDIT_OUTPUT, false),
            tool_result_entry(
                1_100,
                "c2",
                "edited /tmp/a.txt\n\n--- /tmp/a.txt\n+++ /tmp/a.txt\n@@ -1 +1 @@\n-x\n+y\n",
                false,
            ),
            assistant_entry(1_200, "done", None),
            end_entry(1_400),
        ];
        let out = project_with(entries, names(&[("c1", "edit"), ("c2", "edit")]));
        let files = out.last().unwrap().file_changes.as_ref().unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!((files[0].add, files[0].del), (1, 1));
        assert!(files[0].diff.contains("-x"));
    }

    #[test]
    fn parser_tolerates_prose_and_truncation_tail() {
        let text = "written: /tmp/big.txt (20480 bytes)\n\n--- /tmp/big.txt\n+++ /tmp/big.txt\n@@ -0,0 +1,3 @@\n+a\n+b\n(diff truncated, 20480 bytes total)";
        let changes = parse_file_changes(text);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, "/tmp/big.txt");
        assert_eq!((changes[0].add, changes[0].del), (2, 0));
        assert!(changes[0].diff.contains("diff truncated"));
    }

    #[test]
    fn parser_returns_empty_for_non_diff_text() {
        assert!(parse_file_changes("edited /tmp/a.txt").is_empty());
        assert!(parse_file_changes("").is_empty());
    }

    #[test]
    fn pure_tool_turn_stays_hidden_without_a_text_piece() {
        // Decision 3: a pure tool turn (no text piece) shows no summary even if it has changes.
        let entries = vec![
            user_entry(1_000),
            tool_result_entry(1_100, "c1", EDIT_OUTPUT, false),
            end_entry(1_300),
        ];
        let out = project_with(entries, names(&[("c1", "edit")]));
        assert!(out.iter().all(|m| m.file_changes.is_none()));
    }

    #[test]
    fn empty_log_projects_nothing() {
        assert!(project(vec![]).is_empty());
    }

    #[test]
    fn full_turn_folds_usage_duration_and_done_ts_across_steps() {
        let out = project(vec![
            user_entry(1_000),
            assistant_entry(1_100, "step one", Some((100, 40))),
            assistant_entry(1_200, "all done", Some((50, 20))),
            end_entry(1_500),
        ]);
        assert_eq!(out.len(), 3, "user + 两步文本");
        let last = out.last().unwrap();
        assert_eq!(last.content, "all done");
        let usage = last.usage.as_ref().expect("回合汇总挂最终文本 piece");
        assert_eq!((usage.input_tokens, usage.output_tokens), (150, 60));
        assert_eq!(last.duration_ms, Some(500));
        assert_eq!(last.done_ts, Some(1_500));
        // Pieces of intermediate steps do not carry the turn summary.
        assert!(out[1].usage.is_none() && out[1].duration_ms.is_none() && out[1].done_ts.is_none());
    }

    #[test]
    fn unfinished_turn_has_no_meta() {
        // Turn without turn/end (log truncated after an interruption): duration/tokens/completion time are all omitted.
        let out = project(vec![
            user_entry(1_000),
            assistant_entry(1_100, "partial", Some((10, 5))),
        ]);
        let last = out.last().unwrap();
        assert!(last.usage.is_none());
        assert!(last.duration_ms.is_none());
        assert!(last.done_ts.is_none());
    }

    #[test]
    fn legacy_messages_keep_duration_but_hide_tokens() {
        // Legacy sessions: assistant/message without usage (decision 5: drop tokens, keep duration).
        let out = project(vec![
            user_entry(1_000),
            assistant_entry(1_100, "old reply", None),
            end_entry(1_400),
        ]);
        let last = out.last().unwrap();
        assert!(last.usage.is_none(), "存量无 usage,Token 段隐藏");
        assert_eq!(last.duration_ms, Some(400));
        assert_eq!(last.done_ts, Some(1_400));
    }

    #[test]
    fn usage_anchor_counts_without_rendering() {
        // A usage anchor (empty content) produces no piece but still counts toward the turn summary.
        let out = project(vec![
            user_entry(1_000),
            anchor_entry(1_050, (30, 10)),
            assistant_entry(1_100, "reply", None),
            end_entry(1_300),
        ]);
        assert_eq!(out.len(), 2, "锚点不渲染");
        let usage = out.last().unwrap().usage.as_ref().unwrap();
        assert_eq!((usage.input_tokens, usage.output_tokens), (30, 10));
    }

    #[test]
    fn consecutive_turns_stay_isolated() {
        let out = project(vec![
            user_entry(1_000),
            assistant_entry(1_100, "first", Some((10, 5))),
            end_entry(1_200),
            user_entry(2_000),
            assistant_entry(2_100, "second", Some((20, 7))),
            end_entry(2_300),
        ]);
        assert_eq!(out.len(), 4);
        let first = &out[1];
        let second = &out[3];
        assert_eq!(first.usage.as_ref().unwrap().input_tokens, 10);
        assert_eq!(second.usage.as_ref().unwrap().input_tokens, 20);
        assert_eq!(first.duration_ms, Some(200));
        assert_eq!(second.duration_ms, Some(300));
        assert_eq!(first.done_ts, Some(1_200));
        assert_eq!(second.done_ts, Some(2_300));
    }

    #[test]
    fn missing_user_start_still_folds_usage_and_done() {
        // fork/seed sessions may lack a preceding user/message: duration is hidden, usage/doneTs still apply.
        let out = project(vec![
            assistant_entry(1_100, "seeded reply", Some((10, 5))),
            end_entry(1_400),
        ]);
        let last = out.last().unwrap();
        assert!(last.duration_ms.is_none());
        assert!(last.usage.is_some());
        assert_eq!(last.done_ts, Some(1_400));
    }

    #[test]
    fn backwards_clock_saturates_duration_to_zero() {
        let out = project(vec![
            user_entry(2_000),
            assistant_entry(2_100, "reply", Some((1, 1))),
            end_entry(1_000),
        ]);
        assert_eq!(out.last().unwrap().duration_ms, Some(0));
    }

    #[test]
    fn tool_pieces_carry_no_turn_meta() {
        let entries = vec![
            user_entry(1_000),
            SurfaceEntry {
                seq: 0,
                time: 1_100,
                event_type: "assistant/message".into(),
                usage: Some((10, 5)),
                message: tool_call_message(),
                meta: None,
            data: JsonValue::Null,
            },
            end_entry(1_500),
        ];
        let out = project(entries);
        assert_eq!(out.len(), 2, "user piece + 纯工具轮的 tool piece");
        let tool = &out[1];
        assert_eq!(tool.role, "tool");
        assert!(tool.usage.is_none() && tool.duration_ms.is_none() && tool.done_ts.is_none());
    }
}

impl HostRuntime {
    /// Plan confirmation (plan fold): records the approved exit —
    /// `plan/mode {active: false}` — and releases the read-only pairing
    /// (`sandbox/mode → workspace-write`). Subsequent turns' fold no longer
    /// renders the `plan:policy` section or re-stamps read-only. Runs through
    /// the live/cold append helper, so it works while the agent is alive and
    /// after it was disposed (the UI shows the card after turn end).
    #[api_method("session.confirmPlan", permissions = ["session:write"])]
    pub fn confirm_plan(&self, session_id: String) -> anyhow::Result<()> {
        let inner = self.inner.clone();
        let handle = self.runtime_handle.clone();
        std::thread::spawn(move || {
            handle.block_on(async {
                let l = inner.lock().await;
                if let Some(ref runtime) = *l {
                    let _ = append_session_event(
                        runtime,
                        &session_id,
                        crate::plan_mode::PLAN_MODE_EVENT,
                        starriver_kernel::json::JsonValue::Object(vec![(
                            "active".into(),
                            starriver_kernel::json::JsonValue::Bool(false),
                        )]),
                    )
                    .await;
                    let _ = append_session_event(
                        runtime,
                        &session_id,
                        "sandbox/mode",
                        JsonValue::Object(vec![(
                            "mode".into(),
                            JsonValue::String("workspace-write".into()),
                        )]),
                    )
                    .await;
                }
            })
        })
        .join()
        .ok();
        Ok(())
    }
}

impl HostRuntime {
    /// Approval mode switch: append an approval/policy event to the given session.
    /// mode: "manual" | "accept-edits" | "auto" | "bypass"
    /// Under Plan the approval knob is locked to Manual — but only while the
    /// session's plan is in the planning phase. The lock follows the SESSION's
    /// `plan/mode` fold (WI-144), not the deployment default: once the plan
    /// review is approved (or confirm_plan releases the pairing), the user's
    /// next-turn choice is the authority.
    #[api_method("session.setApprovalMode", permissions = ["session:write"])]
    pub fn set_approval_mode(&self, session_id: String, mode: String) -> anyhow::Result<()> {
        use starriver_agent_loop::approval::ApprovalMode;
        let m = ApprovalMode::parse(&mode)
            .ok_or_else(|| anyhow::anyhow!("unknown approval mode `{mode}`"))?;
        let inner = self.inner.clone();
        let handle = self.runtime_handle.clone();
        let result = std::thread::spawn(move || {
            handle.block_on(async {
                let l = inner.lock().await;
                let Some(ref runtime) = *l else {
                    return Ok(());
                };
                if session_plan_planning(runtime, &session_id).await {
                    return Err(anyhow::anyhow!(
                        "approval mode is locked to `manual` while the session's plan is in planning"
                    ));
                }
                let _ = crate::sessions::append_session_event(
                    runtime,
                    &session_id,
                    "approval/policy",
                    starriver_kernel::json::JsonValue::Object(vec![(
                        "policy".into(),
                        starriver_kernel::json::JsonValue::String(m.as_str().into()),
                    )]),
                )
                .await;
                Ok(())
            })
        })
        .join()
        .unwrap_or_else(|_| Ok(()));
        result
    }

 /// Permission preset switch (permission-presets parity): append BOTH
    /// knob events — `sandbox/mode` and `approval/policy` — for the preset's
    /// (sandbox, approval) bundle. Presets are the only sanctioned way to
    /// select DangerFullAccess from a client; the high-risk confirmation gate
    /// lives in the UI, not here.
    #[api_method("session.setPermissionPreset", permissions = ["session:write"])]
    pub fn set_permission_preset(&self, session_id: String, preset: String) -> anyhow::Result<()> {
        let p = crate::permission_presets::parse(&preset)
            .ok_or_else(|| anyhow::anyhow!("unknown permission preset `{preset}`"))?;
        let inner = self.inner.clone();
        let handle = self.runtime_handle.clone();
        let result = std::thread::spawn(move || {
            handle.block_on(async {
                let l = inner.lock().await;
                let Some(ref runtime) = *l else {
                    return Ok(());
                };
                // Same session-phase lock as set_approval_mode (WI-144): the
                // review card's preset write must land once the plan is approved.
                if session_plan_planning(runtime, &session_id).await {
                    return Err(anyhow::anyhow!(
                        "permissions are locked by plan mode (sandbox managed per turn, approval locked to `manual`)"
                    ));
                }
                let _ = crate::sessions::append_session_event(
                    runtime,
                    &session_id,
                    "sandbox/mode",
                    starriver_kernel::json::JsonValue::Object(vec![(
                        "mode".into(),
                        starriver_kernel::json::JsonValue::String(p.sandbox.as_str().into()),
                    )]),
                )
                .await;
                let _ = crate::sessions::append_session_event(
                    runtime,
                    &session_id,
                    "approval/policy",
                    starriver_kernel::json::JsonValue::Object(vec![(
                        "policy".into(),
                        starriver_kernel::json::JsonValue::String(p.approval.as_str().into()),
                    )]),
                )
                .await;
                Ok(())
            })
        })
        .join()
        .unwrap_or_else(|_| Ok(()));
        result
    }

    /// Fold the session's permission knobs (live or cold) and resolve them
    /// against the preset table: `{sandbox, approval, preset}` where `preset`
    /// is a table name or the derived `custom` state. Unavailable sessions
    /// yield null knobs (the picker renders its seed).
    #[api_method("session.getPermission", permissions = ["session:read"])]
    pub fn get_permission(&self, session_id: String) -> anyhow::Result<serde_json::Value> {
        use starriver_agent_loop::approval::ApprovalMode;
        use starriver_kernel::sandbox::SandboxMode;
        let inner = self.inner.clone();
        let handle = self.runtime_handle.clone();
        Ok(std::thread::spawn(move || {
            handle.block_on(async {
                let l = inner.lock().await;
                let Some(ref runtime) = *l else {
                    return serde_json::json!({ "sandbox": null, "approval": null, "preset": null });
                };
                let (sandbox, approval) = match crate::sessions::session_events(runtime, &session_id).await {
                    Ok(events) => {
                        // Same last-wins folds the kernel uses, over the
                        // event slice (live log and cold store agree).
                        let mut sandbox = None;
                        let mut approval = None;
                        for event in &events {
                            match event.event_type.as_str() {
                                "sandbox/mode" => {
                                    if let Some(m) = event.data.as_str_at("mode") {
                                        sandbox = SandboxMode::parse(m);
                                    }
                                }
                                "approval/policy" => {
                                    if let Some(m) = event.data.as_str_at("policy") {
                                        approval = ApprovalMode::parse(m);
                                    }
                                }
                                _ => {}
                            }
                        }
                        (sandbox, approval)
                    }
                    Err(error) => {
                        tracing::warn!(session = %session_id, %error, "permission fold unavailable");
                        (None, None)
                    }
                };
                serde_json::json!({
                    "sandbox": sandbox.map(|m| m.as_str()),
                    "approval": approval.map(|m| m.as_str()),
                    "preset": crate::permission_presets::derive_preset(sandbox, approval),
                })
            })
        })
        .join()
        .unwrap_or_else(|_| serde_json::json!({ "sandbox": null, "approval": null, "preset": null })))
    }

    /// WI-16: list a session's remembered approval rules (`approval/rule`
    /// events), revoked rules flagged. Live and cold sessions both resolve;
    /// an unavailable session yields an empty list (timeline parity).
    #[api_method("approval.listRules", permissions = ["session:read"])]
    pub fn list_approval_rules(&self, session_id: String) -> anyhow::Result<serde_json::Value> {
        let inner = self.inner.clone();
        let handle = self.runtime_handle.clone();
        Ok(std::thread::spawn(move || {
            handle.block_on(async {
                let l = inner.lock().await;
                let Some(ref runtime) = *l else {
                    return serde_json::json!([]);
                };
                match crate::sessions::session_events(runtime, &session_id).await {
                    Ok(events) => approval_rules_json(&events),
                    Err(error) => {
                        tracing::warn!(session = %session_id, %error, "approval rules unavailable");
                        serde_json::json!([])
                    }
                }
            })
        })
        .join()
        .unwrap_or_else(|_| serde_json::json!([])))
    }

    /// WI-16: revoke a remembered approval rule by id (appends
    /// `approval/rule-revoked`); the rule stops auto-approving immediately.
    #[api_method("approval.revokeRule", permissions = ["session:write"])]
    pub fn revoke_approval_rule(
        &self,
        session_id: String,
        rule_id: String,
    ) -> anyhow::Result<bool> {
        let inner = self.inner.clone();
        let handle = self.runtime_handle.clone();
        std::thread::spawn(move || {
            handle.block_on(async {
                let l = inner.lock().await;
                let Some(ref runtime) = *l else {
                    return Err(anyhow::anyhow!("runtime not loaded"));
                };
                // Validate against the current fold (live or cold)…
                let events = crate::sessions::session_events(runtime, &session_id).await?;
                let known = starriver_agent_loop::approval::approval_rules_in(&events)
                    .iter()
                    .any(|rule| rule.id == rule_id && !rule.revoked);
                if !known {
                    return Ok(false);
                }
                // …then append the revocation through the same live-or-cold
                // path every other host meta event uses.
                crate::sessions::append_session_event(
                    runtime,
                    &session_id,
                    "approval/rule-revoked",
                    JsonValue::Object(vec![("id".into(), JsonValue::String(rule_id.clone()))]),
                )
                .await?;
                Ok(true)
            })
        })
        .join()
        .map_err(|_| anyhow::anyhow!("revoke rule worker panicked"))?
    }
}

/// Protocol JSON for a remembered-rule fold.
fn approval_rules_json(events: &[SessionEvent]) -> serde_json::Value {
    serde_json::Value::Array(
        starriver_agent_loop::approval::approval_rules_in(events)
            .iter()
            .map(|rule| {
                serde_json::json!({
                    "id": rule.id,
                    "tool": rule.tool,
                    "kind": rule.kind,
                    "prefix": rule.prefix,
                    "command": rule.command,
                    "revoked": rule.revoked,
                })
            })
            .collect(),
    )
}

#[cfg(test)]
mod delete_tests {
    use crate::testutil::with_temp_home;
    use crate::HostRuntime;

    /// BUG-13: deleting an unknown id stays Ok (idempotent — the NotFound
    /// branch of the propagated error path), and the error channels now
    /// exist for real persistence failures to surface through.
    #[test]
    fn delete_unknown_session_is_idempotent_ok() {
        with_temp_home(|_home| {
            let host = HostRuntime::new(None).expect("host boots");
            host.delete_session("no-such-session".into())
                .expect("unknown id deletes cleanly");
        });
    }
}

impl crate::HostRuntime {
    /// Current projection epoch (open-turn snapshot stream): a client epoch mismatch triggers a full re-pull.
    pub fn trace_epoch(&self, session_id: &str) -> u64 {
        *self
            .trace_epochs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(session_id)
            .unwrap_or(&0)
    }

    /// Bumped after compaction/fork/rollback (manual compact path; automatic compaction
    /// mid-run is bumped by the relay when it sees a Compaction event).
    pub fn bump_trace_epoch(&self, session_id: &str) {
        let mut map = self
            .trace_epochs
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let next = map.get(session_id).copied().unwrap_or(0) + 1;
        map.insert(session_id.to_string(), next);
    }
}

#[cfg(test)]
mod trace_snapshot_tests {
    use super::*;

    fn entry(event_type: &str, seq: u64) -> SurfaceEntry {
        SurfaceEntry {
            seq,
            time: seq,
            event_type: event_type.into(),
            data: JsonValue::Null,
            usage: None,
            message: None,
            meta: None,
        }
    }

    /// Slice semantics: start at the last user/message (inclusive); request/header after it
    /// are included too (the turn header belongs to its turn); turn_index = user entries before it.
    #[test]
    fn open_turn_slice_cuts_at_last_user_message_and_counts_turns() {
        let entries = vec![
            entry("request/header", 1),
            entry("user/message", 2),
            entry("assistant/message", 3),
            entry("user/message", 4),
            entry("request/header", 5),
            entry("tool/call", 6),
        ];
        let (turn_index, slice) = open_turn_slice(entries).expect("slice");
        assert_eq!(turn_index, 1, "第二回合（0-based）");
        assert_eq!(slice.len(), 3, "user(4) + header(5) + tool/call(6)");
        assert_eq!(slice[0].seq, 4);
        assert_eq!(slice[2].seq, 6);
    }

    /// No user entry (turn not started / injections only) → no snapshot.
    #[test]
    fn open_turn_slice_without_user_message_is_none() {
        let entries = vec![entry("request/header", 1), entry("context/message", 2)];
        assert!(open_turn_slice(entries).is_none());
    }
}
