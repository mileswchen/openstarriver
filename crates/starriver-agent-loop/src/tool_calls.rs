//! Tool-call scheduler: exclusive barriers + bounded parallel pool with
//! model-order commit, matching `the agent loop/tool-calls`.

use cordis_rslite::Value;
use futures::future::BoxFuture;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use starriver_kernel::json::JsonValue;
use starriver_kernel::llm::{
    mint_message_id, CallId, ContentBlock, Message, MessageRole, MessageSource,
};
use starriver_kernel::session::Session;
use starriver_kernel::tools::{
    ScheduledToolDispatch, ScheduledToolPreparation, ToolError, ToolExecutionInput,
    ToolExecutionMode, ToolExecutionResult, ToolRunContext,
};
use tokio_util::sync::CancellationToken;

use crate::services::ToolRuntimeView;

/// Per-step scheduling context shared by every call in one step.
pub struct ToolCallStep<'a> {
    pub tools: &'a dyn ToolRuntimeView,
    pub session: &'a Session,
    pub agent_id: &'a str,
    pub turn: u64,
    pub step: u64,
}

/// One model call ready to schedule.
#[derive(Clone)]
struct PlannedCall {
    block: ContentBlock,
    exec: ToolExecutionInput,
}

struct Slot {
    exec: ToolRunContext,
    result: ToolExecutionResult,
    needs_post: bool,
}

struct GroupOutcome {
    consumed: usize,
    aborted: bool,
    concluded: bool,
}

type InFlight = FuturesUnordered<
    BoxFuture<
        'static,
        (
            usize,
            ToolRunContext,
            Result<ScheduledToolDispatch, ToolError>,
        ),
    >,
>;

/// Default per-step parallel-tool-call budget (`maxParallelToolCalls`
/// production default). Settings override via
/// `AgentOptions.max_parallel_tool_calls`.
pub const DEFAULT_MAX_PARALLEL_TOOL_CALLS: usize = 10;

/// Schedule one assistant step's tool calls by their live concurrency mode.
/// Returns whether any result concluded the turn.
pub async fn execute_tool_calls(
    step: ToolCallStep<'_>,
    tool_calls: Vec<ContentBlock>,
    signal: &CancellationToken,
    max_parallel: usize,
    accept_context: &(dyn Fn(Message) + Send + Sync),
) -> Result<bool, ToolError> {
    let planned = tool_calls
        .into_iter()
        .map(|block| {
            let (call_id, name, arguments) = match &block {
                ContentBlock::ToolCall {
                    id,
                    name,
                    arguments,
                } => (id.clone(), name.clone(), arguments.clone().to_value()),
                _ => (CallId(String::new()), None, Value::new(())),
            };
            PlannedCall {
                exec: ToolExecutionInput {
                    cwd: step
                        .session
                        .header
                        .cwd
                        .clone()
                        .map(std::path::PathBuf::from),
                    sandbox_mode: starriver_kernel::sandbox::effective_sandbox_mode(step.session),
                    call_id,
                    root_call_id: None,
                    name: name.unwrap_or_default(),
                    arguments,
                    agent_id: Some(step.agent_id.into()),
                    parent: None,
                    cancellation: Some(signal.clone()),
                },
                block,
            }
        })
        .collect::<Vec<_>>();

    let mut next = 0;
    let mut concluded = false;
    while next < planned.len() {
        let mode = step.tools.execution_mode(&planned[next].exec);
        let group: Vec<PlannedCall> = if mode == ToolExecutionMode::Parallel {
            planned[next..].to_vec()
        } else {
            vec![planned[next].clone()]
        };
        let outcome = run_group(&step, group, max_parallel, signal, accept_context).await?;
        next += outcome.consumed;
        concluded |= outcome.concluded;
        if outcome.aborted {
            for call in planned[next..].iter() {
                append_skipped(step.session, step.turn, step.step, call);
            }
            return Ok(concluded);
        }
    }
    Ok(concluded)
}

async fn run_group(
    step: &ToolCallStep<'_>,
    group: Vec<PlannedCall>,
    max_parallel: usize,
    signal: &CancellationToken,
    accept_context: &(dyn Fn(Message) + Send + Sync),
) -> Result<GroupOutcome, ToolError> {
    let mut slots: Vec<Option<Slot>> = group.iter().map(|_| None).collect();
    let mut call_seqs = vec![0u64; group.len()];
    let mut next_to_start = 0;
    let mut committed = 0;
    let mut started = 0;
    let mut aborted = signal.is_cancelled();
    let mut concluded = false;
    // In-flight dispatches return the prepared exec so results can be
    // finalized in model order.
    let mut in_flight = InFlight::new();

    loop {
        while !aborted && next_to_start < group.len() && in_flight.len() < max_parallel {
            if next_to_start > 0
                && step.tools.execution_mode(&group[next_to_start].exec)
                    != ToolExecutionMode::Parallel
            {
                break;
            }
            call_seqs[next_to_start] =
                append_tool_call(step.session, step.turn, step.step, &group[next_to_start]);
            started += 1;
            let prepared = match step.tools.prepare(group[next_to_start].exec.clone()).await {
                Ok(prepared) => prepared,
                Err(error) => {
                    drain_in_flight(&mut in_flight).await;
                    return Err(error);
                }
            };
            match prepared {
                ScheduledToolPreparation::Dispatch { exec } => {
                    let dispatch_exec = exec.clone();
                    let dispatch = step.tools.dispatch(exec);
                    let index = next_to_start;
                    in_flight.push(Box::pin(async move {
                        let result = dispatch.await;
                        (index, dispatch_exec, result)
                    }));
                }
                ScheduledToolPreparation::PostResult { exec, result } => {
                    slots[next_to_start] = Some(Slot {
                        exec,
                        result,
                        needs_post: true,
                    });
                }
                ScheduledToolPreparation::FinalResult { exec, result } => {
                    slots[next_to_start] = Some(Slot {
                        exec,
                        result,
                        needs_post: false,
                    });
                }
            }
            next_to_start += 1;
            if let Err(error) = commit_ready(
                step,
                &group,
                &mut slots,
                &call_seqs,
                &mut committed,
                &mut concluded,
                accept_context,
            )
            .await
            {
                drain_in_flight(&mut in_flight).await;
                return Err(error);
            }
            if signal.is_cancelled() {
                aborted = true;
            }
        }
        if in_flight.is_empty() {
            break;
        }
        // The wait races the cancel token: a cancelled turn must not sit on a
        // hung dispatch (a wedged bash child, a deadlocked helper…). Without
        // this race the driver parks in `.await` forever and Stop never lands.
        let (index, exec, result) = tokio::select! {
            completed = in_flight.next() => completed.expect("non-empty in-flight"),
            _ = signal.cancelled() => {
                aborted = true;
                break;
            }
        };
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                // Scheduler failure: drain started dispatches, do not fabricate
                // results, and surface the first failure to the turn boundary.
                drain_in_flight(&mut in_flight).await;
                return Err(error);
            }
        };
        match result {
            ScheduledToolDispatch::PostResult { result } => {
                slots[index] = Some(Slot {
                    exec,
                    result,
                    needs_post: true,
                });
            }
            ScheduledToolDispatch::FinalResult { result } => {
                slots[index] = Some(Slot {
                    exec,
                    result,
                    needs_post: false,
                });
            }
        }
        if let Err(error) = commit_ready(
            step,
            &group,
            &mut slots,
            &call_seqs,
            &mut committed,
            &mut concluded,
            accept_context,
        )
        .await
        {
            drain_in_flight(&mut in_flight).await;
            return Err(error);
        }
        if signal.is_cancelled() {
            aborted = true;
        }
    }

    if aborted {
        // Best effort: commit any prefix that already completed (out-of-order
        // results may have landed just before the cancel). Failures here must
        // not mask the abort — the markers below still close every call.
        let _ = commit_ready(
            step,
            &group,
            &mut slots,
            &call_seqs,
            &mut committed,
            &mut concluded,
            accept_context,
        )
        .await;
        // Started-but-uncommitted dispatches were dropped mid-run (their tool/
        // call row already exists): close each with an aborted result so the
        // transcript has no dangling calls.
        for index in committed..started {
            append_aborted_result(step.session, step.turn, step.step, &group[index], call_seqs[index]);
        }
        for call in group.iter().skip(started) {
            append_skipped(step.session, step.turn, step.step, call);
        }
        return Ok(GroupOutcome {
            consumed: group.len(),
            aborted: true,
            concluded,
        });
    }
    Ok(GroupOutcome {
        consumed: started,
        aborted: false,
        concluded,
    })
}

async fn drain_in_flight(in_flight: &mut InFlight) {
    while in_flight.next().await.is_some() {}
}

async fn commit_ready(
    step: &ToolCallStep<'_>,
    group: &[PlannedCall],
    slots: &mut [Option<Slot>],
    call_seqs: &[u64],
    committed: &mut usize,
    concluded: &mut bool,
    accept_context: &(dyn Fn(Message) + Send + Sync),
) -> Result<(), ToolError> {
    while *committed < group.len() {
        let Some(slot) = slots[*committed].take() else {
            break;
        };
        let call = &group[*committed];
        let result = if slot.needs_post {
            step.tools.finalize(slot.exec, slot.result).await?
        } else {
            step.tools.finish(slot.exec, slot.result)?
        };
 // presentation metadata is computed only for successful
        // top-level calls (the runtime re-gates sub-dispatches) and persisted
        // verbatim on the `tool/result` event.
        let meta = match &result {
            ToolExecutionResult::Success { value, .. } => {
                step.tools.presentation_meta(&call.exec, value)
            }
            _ => None,
        };
        append_tool_result(
            step.session,
            step.turn,
            step.step,
            call,
            result.clone(),
            call_seqs[*committed],
            meta,
        );
        for context in result.additional_contexts() {
            accept_context(context.clone());
        }
        *concluded |= result.concludes_turn();
        *committed += 1;
    }
    Ok(())
}

/// WI-47 Phase B: session `tool/call` events carry the lossless arguments
/// object (old logs hold a string; `event_mapper` reads both formats).
fn block_arguments(block: &ContentBlock) -> JsonValue {
    match block {
        ContentBlock::ToolCall { arguments, .. } => arguments.clone(),
        _ => JsonValue::Null,
    }
}

fn append_tool_call(session: &Session, turn: u64, step: u64, call: &PlannedCall) -> u64 {
    let event = session
        .append_with(
            "tool/call",
            JsonValue::Object(vec![
                ("turn".into(), JsonValue::UInt(turn)),
                ("step".into(), JsonValue::UInt(step)),
                (
                    "callId".into(),
                    JsonValue::String(call.exec.call_id.0.clone()),
                ),
                ("name".into(), JsonValue::String(call.exec.name.clone())),
                ("arguments".into(), block_arguments(&call.block)),
            ]),
            starriver_kernel::session::AppendOptions::default(),
        )
        .expect("tool/call append failed");
    event.seq
}

fn append_tool_result(
    session: &Session,
    turn: u64,
    step: u64,
    call: &PlannedCall,
    result: ToolExecutionResult,
    call_seq: u64,
    meta: Option<serde_json::Value>,
) {
    let message = Message {
        id: mint_message_id(),
        role: MessageRole::User,
        content: vec![ContentBlock::ToolResult {
            call_id: call.exec.call_id.clone(),
            content: result.content().to_vec(),
            is_error: result.is_error(),
        }],
        source: MessageSource::Tool {
            call_id: call.exec.call_id.clone(),
        },
    };
    let mut data = vec![
        ("turn".into(), JsonValue::UInt(turn)),
        ("step".into(), JsonValue::UInt(step)),
        ("message".into(), message.to_json().expect("message json")),
    ];
    if let Some(error) = result.error() {
        data.push((
            "error".into(),
            JsonValue::Object(vec![
                ("message".into(), JsonValue::String(error.message.clone())),
                ("code".into(), JsonValue::String(error.code.clone())),
            ]),
        ));
    }
    if let Some(meta) = meta {
 // Opaque, tool-owned presentation payload (`meta` parity):
        // persisted for client-side card narrowing, never sent to the model.
        data.push(("meta".into(), JsonValue::from_serde_json(meta)));
    }
    session
        .append_with(
            "tool/result",
            JsonValue::Object(data),
            starriver_kernel::session::AppendOptions {
                surface_op: Some(starriver_kernel::session::SurfaceOp::Append),
                source_event_seqs: vec![call_seq],
            },
        )
        .expect("tool/result append failed");
}

fn append_skipped(session: &Session, turn: u64, step: u64, call: &PlannedCall) {
    let call_seq = append_tool_call(session, turn, step, call);
    let result = ToolExecutionResult::Failure {
        error: starriver_kernel::tools::ToolFailure {
            message: "tool call aborted before dispatch".into(),
            code: "ABORTED_BEFORE_DISPATCH".into(),
        },
        content: vec![ContentBlock::Text(
            "Error: tool call aborted before dispatch".into(),
        )],
        additional_contexts: Vec::new(),
    };
    append_tool_result(session, turn, step, call, result, call_seq, None);
}

/// BUG-135: per-turn `reload_image` storm accounting. Pattern-locked models
/// ignore the noop stop hint and re-emit the same call every step — the noop
/// pairs keep the turn alive forever. Two budgets feed the breaker: noop
/// repeats per file id, and the turn's total reload calls (executed or
/// noop'd — fabricated-id NOT_FOUND storms burn the same total).
#[derive(Default)]
pub struct ReloadStorm {
    noop_by_id: std::collections::HashMap<String, u32>,
    total: u32,
}

/// Same-id noop pairs tolerated before the breaker fires (the 3rd repeats).
pub const RELOAD_NOOP_LIMIT_PER_ID: u32 = 2;
/// Total reload_image calls (executed + noop'd) per turn before the breaker
/// fires regardless of id.
pub const RELOAD_TURN_CALL_BUDGET: u32 = 8;

impl ReloadStorm {
    /// Count one reload call (executed or noop'd); returns the turn total.
    pub fn note_call(&mut self) -> u32 {
        self.total = self.total.saturating_add(1);
        self.total
    }
    /// The turn's reload call total so far.
    pub fn total(&self) -> u32 {
        self.total
    }
    /// Count one noop pair for `file_id`; returns its noop count.
    pub fn note_noop(&mut self, file_id: &str) -> u32 {
        let count = self.noop_by_id.entry(file_id.to_string()).or_insert(0);
        *count += 1;
        *count
    }
}

/// BUG-138: per-turn repeat accounting for every NON-reload tool (reload_image
/// keeps its dedicated BUG-135 semantics and is excluded at the call site).
/// Two budgets feed the generalized breaker: consecutive identical-call
/// failures (a tool erroring on the same arguments — unregistered tool, bad
/// input, dead endpoint — is the observed echo-storm shape), and the turn's
/// total non-reload calls as a varied-args storm backstop.
#[derive(Default)]
pub struct TurnRepeatGuard {
    cursor: u64,
    /// tool/call seq → signature, awaiting its result row.
    pending: std::collections::HashMap<u64, String>,
    /// signature → consecutive failed executions; a success resets to 0, so
    /// legitimate re-runs after fixing inputs never trip the breaker.
    consecutive_errors: std::collections::HashMap<String, u32>,
    total: u32,
}

/// Same-signature consecutive failures tolerated before the breaker fires
/// (two real attempts; the 3rd identical call is paired without dispatch).
pub const REPEAT_FAILURE_LIMIT: u32 = 2;

/// WI-202: transport-class tool failures — infrastructure unavailable (MCP
/// server down / request dropped / timeout / launch-handshake failed). They
/// never feed the repeat-failure breaker: retrying after a while is the
/// CORRECT recovery for a transient outage, and no change of arguments can
/// fix it (that is what the breaker's "identical arguments" heuristic is
/// FOR). Hammering a dead endpoint is still backstopped by the turn's total
/// tool-call budget, and the MCP availability notice announces the outage.
fn is_transport_failure(error: &JsonValue) -> bool {
    matches!(
        error.as_str_at("code"),
        Some("MCP_CALL_FAILED")
            | Some("MCP_TIMEOUT")
            | Some("MCP_LAUNCH_FAILED")
            | Some("MCP_INIT_FAILED")
    )
}

/// Total non-reload tool calls per turn before the breaker fires (the
/// `agent.turn_tool_call_budget` settings default).
pub const TURN_TOOL_CALL_BUDGET: u32 = 666;

impl TurnRepeatGuard {
    /// Count one non-reload call; returns the turn total.
    pub fn note_call(&mut self) -> u32 {
        self.total = self.total.saturating_add(1);
        self.total
    }
    pub fn over_budget(&self, budget: u32) -> bool {
        self.total > budget
    }
    pub fn consecutive_errors(&self, sig: &str) -> u32 {
        self.consecutive_errors.get(sig).copied().unwrap_or(0)
    }

    /// WI-202: per-turn reset that keeps the absorb cursor. Zeroing the
    /// cursor made the next `absorb_results` replay the WHOLE session
    /// history, re-counting prior turns' failures into the fresh turn's
    /// "consecutive" tally — the documented per-turn semantics leaked
    /// session-wide (observed: a single retry of a tool that failed once
    /// LAST turn tripped the breaker immediately).
    pub fn reset_keeping_cursor(&mut self) {
        let cursor = self.cursor;
        *self = Self {
            cursor,
            ..Default::default()
        };
    }
    /// Tail-scan tool/call + tool/result rows: recompute each executed call's
    /// signature and adjust its consecutive-error count (error → +1, success
    /// → remove).
    pub fn absorb_results(&mut self, session: &Session) {
        for event in session.events_after(self.cursor) {
            self.cursor = self.cursor.max(event.seq);
            match event.event_type.as_str() {
                "tool/call" => {
                    let is_reload = event.data.as_str_at("name")
                        == Some(crate::image_projection::RELOAD_IMAGE_TOOL);
                    let breaker_row =
                        event.data.get("repeatBreak").and_then(|v| v.as_bool()) == Some(true);
                    if is_reload || breaker_row {
                        continue;
                    }
                    if let (Some(name), Some(args)) =
                        (event.data.as_str_at("name"), event.data.get("arguments"))
                    {
                        self.pending.insert(event.seq, call_signature(name, args));
                    }
                }
                "tool/result" => {
                    if let Some(&linked) = event.source_event_seqs.first() {
                        if let Some(sig) = self.pending.remove(&linked) {
                            let failed = event.data.get("error").is_some();
                            if failed {
                                // WI-202: transport-class failures (dead MCP
                                // endpoint, dropped request, timeout) are
                                // infrastructure, not model-fixable input —
                                // they never feed the repeat breaker.
                                if !event
                                    .data
                                    .get("error")
                                    .is_some_and(|error| is_transport_failure(error))
                                {
                                    *self.consecutive_errors.entry(sig).or_insert(0) += 1;
                                }
                            } else {
                                self.consecutive_errors.remove(&sig);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// Whitespace-normalized tool+arguments identity: re-serialized variants of
/// the same call count as repeats.
pub fn call_signature(name: &str, arguments: &JsonValue) -> String {
    let flat = arguments
        .to_json_string()
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>();
    format!("{name}\u{0}{flat}")
}

/// BUG-138: the generalized breaker's terminal pair — an error result telling
/// the model the repeat loop is over AND ending the turn (the caller pairs
/// every remaining call as skipped, then returns Completed).
pub fn append_repeat_storm_break(
    session: &Session,
    turn: u64,
    step: u64,
    call: &ContentBlock,
    reason: &str,
) {
    let ContentBlock::ToolCall {
        id: call_id,
        name,
        arguments,
    } = call
    else {
        return;
    };
    let event = session
        .append_with(
            "tool/call",
            JsonValue::Object(vec![
                ("turn".into(), JsonValue::UInt(turn)),
                ("step".into(), JsonValue::UInt(step)),
                ("callId".into(), JsonValue::String(call_id.0.clone())),
                (
                    "name".into(),
                    JsonValue::String(name.clone().unwrap_or_default()),
                ),
                ("arguments".into(), arguments.clone()),
                ("repeatBreak".into(), JsonValue::Bool(true)),
            ]),
            starriver_kernel::session::AppendOptions::default(),
        )
        .expect("tool/call append failed");
    let message = Message {
        id: mint_message_id(),
        role: MessageRole::User,
        content: vec![ContentBlock::ToolResult {
            call_id: call_id.clone(),
            content: vec![ContentBlock::Text(format!(
                "Error: repeat-call breaker fired ({reason}) — the turn was terminated early. \
                 Do not retry the same call; summarize the progress and report the blocker."
            ))],
            is_error: true,
        }],
        source: MessageSource::Tool {
            call_id: call_id.clone(),
        },
    };
    session
        .append_with(
            "tool/result",
            JsonValue::Object(vec![
                ("turn".into(), JsonValue::UInt(turn)),
                ("step".into(), JsonValue::UInt(step)),
                (
                    "error".into(),
                    JsonValue::Object(vec![
                        (
                            "message".into(),
                            JsonValue::String("repeated-call limit reached; turn terminated".into()),
                        ),
                        ("code".into(), JsonValue::String("REPEAT_STORM".into())),
                    ]),
                ),
                ("message".into(), message.to_json().expect("message json")),
            ]),
            starriver_kernel::session::AppendOptions {
                surface_op: Some(starriver_kernel::session::SurfaceOp::Append),
                source_event_seqs: vec![event.seq],
            },
        )
        .expect("tool/result append failed");
}

/// BUG-135: the breaker's terminal pair — an error result that both tells
/// the model the loop is over AND (via the loop's `concluded`) ends the
/// turn, so a hint-immune model cannot keep the turn alive with noops.
pub fn append_reload_storm_break(
    session: &Session,
    turn: u64,
    step: u64,
    call: &ContentBlock,
    file_id: &str,
) {
    let ContentBlock::ToolCall {
        id: call_id,
        name,
        arguments,
    } = call
    else {
        return;
    };
    let event = session
        .append_with(
            "tool/call",
            JsonValue::Object(vec![
                ("turn".into(), JsonValue::UInt(turn)),
                ("step".into(), JsonValue::UInt(step)),
                ("callId".into(), JsonValue::String(call_id.0.clone())),
                (
                    "name".into(),
                    JsonValue::String(name.clone().unwrap_or_default()),
                ),
                ("arguments".into(), arguments.clone()),
                ("duplicateNoop".into(), JsonValue::Bool(true)),
            ]),
            starriver_kernel::session::AppendOptions::default(),
        )
        .expect("tool/call append failed");
    let message = Message {
        id: mint_message_id(),
        role: MessageRole::User,
        content: vec![ContentBlock::ToolResult {
            call_id: call_id.clone(),
            content: vec![ContentBlock::Text(format!(
                "Error: reload_image:{file_id} 重复调用已达上限,本回合已提前终止。\
                 图片状态没有变化——请直接基于当前上下文回答;若确实需要该图,\
                 请向用户说明并让其重新发送。"
            ))],
            is_error: true,
        }],
        source: MessageSource::Tool {
            call_id: call_id.clone(),
        },
    };
    session
        .append_with(
            "tool/result",
            JsonValue::Object(vec![
                ("turn".into(), JsonValue::UInt(turn)),
                ("step".into(), JsonValue::UInt(step)),
                (
                    "error".into(),
                    JsonValue::Object(vec![
                        (
                            "message".into(),
                            JsonValue::String(
                                "reload_image repeated-call limit reached; turn terminated"
                                    .into(),
                            ),
                        ),
                        ("code".into(), JsonValue::String("RELOAD_STORM".into())),
                    ]),
                ),
                ("message".into(), message.to_json().expect("message json")),
            ]),
            starriver_kernel::session::AppendOptions {
                surface_op: Some(starriver_kernel::session::SurfaceOp::Append),
                source_event_seqs: vec![event.seq],
            },
        )
        .expect("tool/result append failed");
}

/// BUG-135: pair a call the breaker dropped without running it (the
/// scheduler never saw it) — the generic ABORTED_BEFORE_DISPATCH shape so
/// the assistant message keeps a result for every tool_use.
pub fn append_call_skipped_by_breaker(
    session: &Session,
    turn: u64,
    step: u64,
    call: &ContentBlock,
) {
    let ContentBlock::ToolCall {
        id: call_id,
        name,
        arguments,
    } = call
    else {
        return;
    };
    let event = session
        .append_with(
            "tool/call",
            JsonValue::Object(vec![
                ("turn".into(), JsonValue::UInt(turn)),
                ("step".into(), JsonValue::UInt(step)),
                ("callId".into(), JsonValue::String(call_id.0.clone())),
                (
                    "name".into(),
                    JsonValue::String(name.clone().unwrap_or_default()),
                ),
                ("arguments".into(), arguments.clone()),
            ]),
            starriver_kernel::session::AppendOptions::default(),
        )
        .expect("tool/call append failed");
    let message = Message {
        id: mint_message_id(),
        role: MessageRole::User,
        content: vec![ContentBlock::ToolResult {
            call_id: call_id.clone(),
            content: vec![ContentBlock::Text(
                "Error: tool call aborted before dispatch (reload storm breaker)".into(),
            )],
            is_error: true,
        }],
        source: MessageSource::Tool {
            call_id: call_id.clone(),
        },
    };
    session
        .append_with(
            "tool/result",
            JsonValue::Object(vec![
                ("turn".into(), JsonValue::UInt(turn)),
                ("step".into(), JsonValue::UInt(step)),
                (
                    "error".into(),
                    JsonValue::Object(vec![
                        (
                            "message".into(),
                            JsonValue::String(
                                "turn terminated by reload storm breaker before dispatch"
                                    .into(),
                            ),
                        ),
                        (
                            "code".into(),
                            JsonValue::String("ABORTED_BEFORE_DISPATCH".into()),
                        ),
                    ]),
                ),
                ("message".into(), message.to_json().expect("message json")),
            ]),
            starriver_kernel::session::AppendOptions {
                surface_op: Some(starriver_kernel::session::SurfaceOp::Append),
                source_event_seqs: vec![event.seq],
            },
        )
        .expect("tool/result append failed");
}

/// BUG-114/115: answer an in-turn duplicate `reload_image` call without
/// touching the executor — the file id was already attempted this turn.
/// `confirmed` says whether that attempt SUCCEEDED: a confirmed repeat is
/// told the image is already on the wire (or pinned out by the budget,
/// which its placeholder states), a failed one is told to stop retrying
/// the same id. Re-executing would either return the same "re-attached"
/// success and re-prime the model's repetition loop, or hammer a NOT_FOUND
/// forever — so the `tool/call` + `tool/result` pair lands inline instead.
/// The call row carries `duplicateNoop: true` so the loop's confirmation
/// scan never mistakes it for a real reload result. `aborted` (BUG-131)
/// lands the pair in the ABORTED_BEFORE_DISPATCH shape, matching the
/// generic scheduler's marker for calls that never ran.
pub fn append_duplicate_reload_noop(
    session: &Session,
    turn: u64,
    step: u64,
    call: &ContentBlock,
    file_id: &str,
    confirmed: bool,
    aborted: bool,
) {
    let ContentBlock::ToolCall {
        id: call_id,
        name,
        arguments,
    } = call
    else {
        return;
    };
    let event = session
        .append_with(
            "tool/call",
            JsonValue::Object(vec![
                ("turn".into(), JsonValue::UInt(turn)),
                ("step".into(), JsonValue::UInt(step)),
                ("callId".into(), JsonValue::String(call_id.0.clone())),
                (
                    "name".into(),
                    JsonValue::String(name.clone().unwrap_or_default()),
                ),
                ("arguments".into(), arguments.clone()),
                ("duplicateNoop".into(), JsonValue::Bool(true)),
            ]),
            starriver_kernel::session::AppendOptions::default(),
        )
        .expect("tool/call append failed");
    let (text, is_error) = if aborted {
        (
            format!(
                "Error: reload_image:{file_id} 未执行——回合已中止(该调用为本回合重复调用,\
                 无需补发)"
            ),
            true,
        )
    } else if confirmed {
        (
            format!(
                "reload_image:{file_id} 已在本回合重新加载过,无需重复调用——图片已在上下文中,\
                 或已因热图上限被挤出(对应占位符有说明)。"
            ),
            false,
        )
    } else {
        (
            format!(
                "reload_image:{file_id} 本回合已调用过且未成功(见上一次工具结果,如 NOT_FOUND);\
                 请勿用同一 file_id 重试——核对 file_id 是否来自图片占位符,或请用户重新附加图片。"
            ),
            false,
        )
    };
    let mut result_data = vec![
        ("turn".into(), JsonValue::UInt(turn)),
        ("step".into(), JsonValue::UInt(step)),
    ];
    if aborted {
        result_data.push((
            "error".into(),
            JsonValue::Object(vec![
                (
                    "message".into(),
                    JsonValue::String("turn aborted before dispatch".into()),
                ),
                ("code".into(), JsonValue::String("ABORTED_BEFORE_DISPATCH".into())),
            ]),
        ));
    }
    let message = Message {
        id: mint_message_id(),
        role: MessageRole::User,
        content: vec![ContentBlock::ToolResult {
            call_id: call_id.clone(),
            content: vec![ContentBlock::Text(text)],
            is_error,
        }],
        source: MessageSource::Tool {
            call_id: call_id.clone(),
        },
    };
    result_data.push(("message".into(), message.to_json().expect("message json")));
    session
        .append_with(
            "tool/result",
            JsonValue::Object(result_data),
            starriver_kernel::session::AppendOptions {
                surface_op: Some(starriver_kernel::session::SurfaceOp::Append),
                source_event_seqs: vec![event.seq],
            },
        )
        .expect("tool/result append failed");
}

/// Close a dispatch that was cancelled mid-run: the `tool/call` row already
/// exists (written at start), so only the failure result is appended, linked
/// to that row's seq. The dropped future's own cleanup (bash kill_on_drop +
/// process-group guard) reaps the actual child tree.
fn append_aborted_result(
    session: &Session,
    turn: u64,
    step: u64,
    call: &PlannedCall,
    call_seq: u64,
) {
    let result = ToolExecutionResult::Failure {
        error: starriver_kernel::tools::ToolFailure {
            message: "tool call aborted while running".into(),
            code: "ABORTED_WHILE_RUNNING".into(),
        },
        content: vec![ContentBlock::Text(
            "Error: tool call aborted while running".into(),
        )],
        additional_contexts: Vec::new(),
    };
    append_tool_result(session, turn, step, call, result, call_seq, None);
}

#[cfg(test)]
mod tests {
    use super::*;
    use cordis_rslite::{Context, EventBus};
    use starriver_kernel::llm::ToolSchema;
    use starriver_kernel::session::SessionId;
    use starriver_kernel::tools::{
        ToolDefinition, ToolExecutionInput, ToolExecutionMode, ToolExecutionResult, ToolRunContext,
        ToolRuntime,
    };
    use std::sync::Arc;

    struct FailingTools;
    impl ToolRuntimeView for FailingTools {
        fn schemas(&self) -> Vec<ToolSchema> {
            Vec::new()
        }
        fn schemas_for(&self, _agent_id: Option<&str>) -> Vec<ToolSchema> {
            Vec::new()
        }
        fn sdk_schemas(&self) -> Vec<ToolSchema> {
            Vec::new()
        }
        fn is_ptc_active(&self) -> bool {
            false
        }
        fn restrict_for(
            &self,
            _agent_id: &str,
            _allow: Vec<String>,
        ) -> Result<cordis_rslite::EffectHandle, ToolError> {
            Ok(cordis_rslite::EffectHandle::none())
        }
        fn execution_mode(&self, _input: &ToolExecutionInput) -> ToolExecutionMode {
            ToolExecutionMode::Exclusive
        }
        fn prepare(
            &self,
            _input: ToolExecutionInput,
        ) -> cordis_rslite::BoxFuture<Result<ScheduledToolPreparation, ToolError>> {
            Box::pin(async { Err(ToolError::NotFound("boom".into())) })
        }
        fn dispatch(
            &self,
            _exec: ToolRunContext,
        ) -> cordis_rslite::BoxFuture<Result<ScheduledToolDispatch, ToolError>> {
            Box::pin(async { Err(ToolError::NotFound("unused".into())) })
        }
        fn finalize(
            &self,
            _exec: ToolRunContext,
            _result: ToolExecutionResult,
        ) -> cordis_rslite::BoxFuture<Result<ToolExecutionResult, ToolError>> {
            Box::pin(async { Err(ToolError::NotFound("unused".into())) })
        }
        fn finish(
            &self,
            _exec: ToolRunContext,
            _result: ToolExecutionResult,
        ) -> Result<ToolExecutionResult, ToolError> {
            Err(ToolError::NotFound("unused".into()))
        }

        fn presentation_meta(
            &self,
            _input: &ToolExecutionInput,
            _value: &cordis_rslite::Value,
        ) -> Option<serde_json::Value> {
            None
        }
    }

    struct Echo;
    impl starriver_kernel::tools::ToolExecutor for Echo {
        fn execute(
            &self,
            arguments: Value,
            _ctx: starriver_kernel::tools::ToolRunContext,
        ) -> cordis_rslite::BoxFuture<Result<Value, ToolError>> {
            Box::pin(async { Ok(arguments) })
        }
    }

    fn echo_definition(name: &str) -> ToolDefinition {
        ToolDefinition {
            requires_approval: false,
            deny_in_readonly: false,
            name: name.into(),
            description: "echo".into(),
            parameters: Value::new(()),
            output: Value::new(()),
            render: Arc::new(|_, value| vec![ContentBlock::Text(format!("{value:?}"))]),
            executor: Arc::new(Echo),
            is_concurrency_safe: None,
            timeout: None,
            finalize_content: None,
        }
    }

    fn tool_call(id: &str) -> ContentBlock {
        ContentBlock::ToolCall {
            id: CallId(id.into()),
            name: Some("echo".into()),
            arguments: JsonValue::Object(Vec::new()),
        }
    }

    #[tokio::test]
    async fn single_tool_call_appends_pair_and_accepts_context() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let tools = starriver_tools::InMemoryToolRuntime::new(bus.clone(), ctx.clone());
        let _handle = tools.register(echo_definition("echo")).unwrap();
        let session = Session::new(SessionId("s1".into()), bus);
        let signal = CancellationToken::new();
        let accepted = Arc::new(std::sync::Mutex::new(0usize));
        let counter = Arc::clone(&accepted);

        let concluded = execute_tool_calls(
            ToolCallStep {
                tools: &tools,
                session: &session,
                agent_id: "a1",
                turn: 1,
                step: 1,
            },
            vec![tool_call("c1")],
            &signal,
            10,
            &move |_context| {
                *counter.lock().unwrap() += 1;
            },
        )
        .await;

        assert!(!concluded.unwrap());
        let events = session.events();
        assert_eq!(
            events
                .iter()
                .filter(|e| e.event_type == "tool/call")
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| e.event_type == "tool/result")
                .count(),
            1
        );
        let result = events
            .iter()
            .find(|e| e.event_type == "tool/result")
            .unwrap();
        assert!(result.surface_op.is_some());
        assert_eq!(result.source_event_seqs.len(), 1);
    }

    /// BUG-138: the breaker's turn budget is a settings-overridable limit —
    /// `over_budget` compares against the SUPPLIED budget (the
    /// `agent.turn_tool_call_budget` engine default is 666), not a global.
    #[test]
    fn turn_budget_compares_against_supplied_limit() {
        let mut guard = TurnRepeatGuard::default();
        for _ in 0..150 {
            guard.note_call();
        }
        assert_eq!(guard.over_budget(666), false, "engine default: 150 calls stay under");
        assert_eq!(guard.over_budget(149), true, "tighter settings budget trips past 149");
        assert_eq!(guard.over_budget(150), false, "boundary: fires only strictly past the budget");
        guard.note_call();
        assert_eq!(guard.over_budget(150), true, "151st call crosses a 150 budget");
        // The engine default constant itself.
        assert_eq!(TURN_TOOL_CALL_BUDGET, 666);
    }

    #[test]
    fn duplicate_reload_noop_appends_ordered_pair_with_stop_hint() {
        let session = Session::new(SessionId("s1".into()), EventBus::new());
        let call = ContentBlock::ToolCall {
            id: CallId("c9".into()),
            name: Some("reload_image".into()),
            arguments: JsonValue::from_json_str(r#"{"file_id":"sha256:aa"}"#).unwrap(),
        };
        append_duplicate_reload_noop(&session, 3, 2, &call, "sha256:aa", true, false);

        let events = session.events();
        let tool_call = events
            .iter()
            .find(|e| e.event_type == "tool/call")
            .expect("tool/call appended");
        assert_eq!(
            tool_call.data.get("callId").and_then(|v| v.as_str()),
            Some("c9")
        );
        assert_eq!(
            tool_call.data.get("name").and_then(|v| v.as_str()),
            Some("reload_image")
        );
        // The noop marker keeps the loop's confirmation scan from pinning
        // the id off a synthetic pair (BUG-115).
        assert_eq!(
            tool_call.data.get("duplicateNoop").and_then(|v| v.as_bool()),
            Some(true)
        );
        let tool_result = events
            .iter()
            .find(|e| e.event_type == "tool/result")
            .expect("tool/result appended");
        assert!(tool_result.surface_op.is_some(), "rides the surface");
        assert_eq!(tool_result.source_event_seqs, vec![tool_call.seq]);
        assert!(
            tool_result.data.get("error").is_none(),
            "a no-op duplicate is not an error"
        );
        let rendered = format!("{:?}", tool_result.data.get("message").unwrap());
        assert!(rendered.contains("sha256:aa"), "{rendered}");
        assert!(rendered.contains("无需重复调用"), "{rendered}");
        assert!(
            rendered.contains("\"isError\":false" ) || rendered.contains("Bool(false)"),
            "not flagged as an error: {rendered}"
        );
    }

    /// BUG-115: a repeat of a FAILED reload gets the failure-aware wording
    /// (never claims an "already reloaded" success).
    #[test]
    fn duplicate_reload_noop_after_failure_uses_failure_wording() {
        let session = Session::new(SessionId("s1".into()), EventBus::new());
        let call = ContentBlock::ToolCall {
            id: CallId("c1".into()),
            name: Some("reload_image".into()),
            arguments: JsonValue::from_json_str(r#"{"file_id":"sha256:bb"}"#).unwrap(),
        };
        append_duplicate_reload_noop(&session, 1, 2, &call, "sha256:bb", false, false);
        let events = session.events();
        let tool_result = events
            .iter()
            .find(|e| e.event_type == "tool/result")
            .expect("tool/result appended");
        let rendered = format!("{:?}", tool_result.data.get("message").unwrap());
        assert!(rendered.contains("未成功"), "{rendered}");
        assert!(rendered.contains("请勿用同一 file_id 重试"), "{rendered}");
        assert!(!rendered.contains("已重新加载过"), "{rendered}");
    }

    /// BUG-131: after the turn aborts, the remaining noop pairs land in the
    /// ABORTED_BEFORE_DISPATCH shape (error marker, no success-looking
    /// stop hint) — matching the generic scheduler's semantics.
    #[test]
    fn duplicate_reload_noop_after_abort_uses_the_aborted_shape() {
        let session = Session::new(SessionId("s1".into()), EventBus::new());
        let call = ContentBlock::ToolCall {
            id: CallId("c7".into()),
            name: Some("reload_image".into()),
            arguments: JsonValue::from_json_str(r#"{"file_id":"sha256:cc"}"#).unwrap(),
        };
        append_duplicate_reload_noop(&session, 2, 4, &call, "sha256:cc", true, true);
        let events = session.events();
        let tool_result = events
            .iter()
            .find(|e| e.event_type == "tool/result")
            .expect("tool/result appended");
        assert_eq!(
            tool_result
                .data
                .get("error")
                .and_then(|e| e.as_str_at("code")),
            Some("ABORTED_BEFORE_DISPATCH"),
            "the aborted pair carries the scheduler's marker"
        );
        let rendered = format!("{:?}", tool_result.data.get("message").unwrap());
        assert!(rendered.contains("回合已中止"), "{rendered}");
        assert!(!rendered.contains("已重新加载过"), "{rendered}");
    }

    #[tokio::test]
    async fn scheduler_failure_propagates_without_fabrication() {
        let session = Session::new(SessionId("s1".into()), EventBus::new());
        let signal = CancellationToken::new();

        let error = execute_tool_calls(
            ToolCallStep {
                tools: &FailingTools,
                session: &session,
                agent_id: "a1",
                turn: 1,
                step: 1,
            },
            vec![tool_call("c1")],
            &signal,
            10,
            &|_| {},
        )
        .await
        .unwrap_err();

        assert!(matches!(error, ToolError::NotFound(_)));
        // The started tool/call stays recorded, but no result is fabricated.
        assert_eq!(
            session
                .events()
                .iter()
                .filter(|e| e.event_type == "tool/call")
                .count(),
            1
        );
        assert_eq!(
            session
                .events()
                .iter()
                .filter(|e| e.event_type == "tool/result")
                .count(),
            0
        );
    }

    /// Meta view: presentation_meta returns a fixed payload for c1.
    struct MetaTools;
    impl ToolRuntimeView for MetaTools {
        fn schemas(&self) -> Vec<ToolSchema> {
            Vec::new()
        }
        fn schemas_for(&self, _a: Option<&str>) -> Vec<ToolSchema> {
            Vec::new()
        }
        fn sdk_schemas(&self) -> Vec<ToolSchema> {
            Vec::new()
        }
        fn is_ptc_active(&self) -> bool {
            false
        }
        fn restrict_for(
            &self,
            _a: &str,
            _allow: Vec<String>,
        ) -> Result<cordis_rslite::EffectHandle, ToolError> {
            Ok(cordis_rslite::EffectHandle::none())
        }
        fn execution_mode(&self, _input: &ToolExecutionInput) -> ToolExecutionMode {
            ToolExecutionMode::Parallel
        }
        fn prepare(
            &self,
            input: ToolExecutionInput,
        ) -> cordis_rslite::BoxFuture<Result<ScheduledToolPreparation, ToolError>> {
            let exec = ToolRunContext::new(starriver_kernel::tools::ToolExecution {
                cwd: None,
                sandbox_mode: None,
                call_id: input.call_id.clone(),
                root_call_id: input.call_id.clone(),
                name: input.name,
                arguments: input.arguments,
                agent_id: input.agent_id,
                parent: input.parent,
                token: starriver_kernel::tools::mint_tool_execution_token(),
                cancellation: input.cancellation,
            });
            Box::pin(async move { Ok(ScheduledToolPreparation::Dispatch { exec }) })
        }
        fn dispatch(
            &self,
            _exec: ToolRunContext,
        ) -> cordis_rslite::BoxFuture<Result<ScheduledToolDispatch, ToolError>> {
            Box::pin(async move {
                Ok(ScheduledToolDispatch::PostResult {
                    result: success_result(),
                })
            })
        }
        fn finalize(
            &self,
            _exec: ToolRunContext,
            result: ToolExecutionResult,
        ) -> cordis_rslite::BoxFuture<Result<ToolExecutionResult, ToolError>> {
            Box::pin(async { Ok(result) })
        }
        fn finish(
            &self,
            _exec: ToolRunContext,
            result: ToolExecutionResult,
        ) -> Result<ToolExecutionResult, ToolError> {
            Ok(result)
        }
        fn presentation_meta(
            &self,
            input: &ToolExecutionInput,
            _value: &cordis_rslite::Value,
        ) -> Option<serde_json::Value> {
            Some(serde_json::json!({ "kind": "todo", "for": input.call_id.0 }))
        }
    }

    #[tokio::test]
    async fn presentation_meta_lands_on_tool_result_event() {
        let session = Session::new(SessionId("s1".into()), EventBus::new());
        let signal = CancellationToken::new();

        execute_tool_calls(
            ToolCallStep {
                tools: &MetaTools,
                session: &session,
                agent_id: "a1",
                turn: 1,
                step: 1,
            },
            vec![tool_call("c1")],
            &signal,
            4,
            &|_| {},
        )
        .await
        .unwrap();

        let events = session.events();
        let result_event = events
            .iter()
            .find(|e| e.event_type == "tool/result")
            .expect("tool/result appended");
        let meta = result_event.data.get("meta").expect("meta payload present");
        assert!(meta.as_str_at("kind") == Some("todo"), "{meta:?}");
    }

    fn success_result() -> ToolExecutionResult {
        ToolExecutionResult::Success {
            value: Value::new(()),
            content: Vec::new(),
            additional_contexts: Vec::new(),
            concludes_turn: false,
        }
    }

    struct ParallelFailTools;
    impl ToolRuntimeView for ParallelFailTools {
        fn schemas(&self) -> Vec<ToolSchema> {
            Vec::new()
        }
        fn schemas_for(&self, _agent_id: Option<&str>) -> Vec<ToolSchema> {
            Vec::new()
        }
        fn sdk_schemas(&self) -> Vec<ToolSchema> {
            Vec::new()
        }
        fn is_ptc_active(&self) -> bool {
            false
        }
        fn restrict_for(
            &self,
            _agent_id: &str,
            _allow: Vec<String>,
        ) -> Result<cordis_rslite::EffectHandle, ToolError> {
            Ok(cordis_rslite::EffectHandle::none())
        }
        fn execution_mode(&self, _input: &ToolExecutionInput) -> ToolExecutionMode {
            ToolExecutionMode::Parallel
        }
        fn prepare(
            &self,
            input: ToolExecutionInput,
        ) -> cordis_rslite::BoxFuture<Result<ScheduledToolPreparation, ToolError>> {
            let exec = ToolRunContext::new(starriver_kernel::tools::ToolExecution {
                cwd: None,
                sandbox_mode: None,
                call_id: input.call_id.clone(),
                root_call_id: input.call_id.clone(),
                name: input.name,
                arguments: input.arguments,
                agent_id: input.agent_id,
                parent: input.parent,
                token: starriver_kernel::tools::mint_tool_execution_token(),
                cancellation: input.cancellation,
            });
            Box::pin(async move { Ok(ScheduledToolPreparation::Dispatch { exec }) })
        }
        fn dispatch(
            &self,
            exec: ToolRunContext,
        ) -> cordis_rslite::BoxFuture<Result<ScheduledToolDispatch, ToolError>> {
            let id = exec.execution.call_id.0.clone();
            Box::pin(async move {
                if id == "c1" {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    Ok(ScheduledToolDispatch::PostResult {
                        result: success_result(),
                    })
                } else {
                    Err(ToolError::NotFound("boom".into()))
                }
            })
        }
        fn finalize(
            &self,
            _exec: ToolRunContext,
            result: ToolExecutionResult,
        ) -> cordis_rslite::BoxFuture<Result<ToolExecutionResult, ToolError>> {
            Box::pin(async { Ok(result) })
        }
        fn finish(
            &self,
            _exec: ToolRunContext,
            result: ToolExecutionResult,
        ) -> Result<ToolExecutionResult, ToolError> {
            Ok(result)
        }

        fn presentation_meta(
            &self,
            _input: &ToolExecutionInput,
            _value: &cordis_rslite::Value,
        ) -> Option<serde_json::Value> {
            None
        }
    }

    #[tokio::test]
    async fn parallel_dispatch_failure_drains_and_propagates() {
        let session = Session::new(SessionId("s1".into()), EventBus::new());
        let signal = CancellationToken::new();

        let error = execute_tool_calls(
            ToolCallStep {
                tools: &ParallelFailTools,
                session: &session,
                agent_id: "a1",
                turn: 1,
                step: 1,
            },
            vec![tool_call("c1"), tool_call("c2")],
            &signal,
            10,
            &|_| {},
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ToolError::NotFound(_)));

        let events = session.events();
        // Both calls started (recorded); the failed one never settles a result.
        assert_eq!(
            events
                .iter()
                .filter(|e| e.event_type == "tool/call")
                .count(),
            2
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| e.event_type == "tool/result")
                .count(),
            0,
            "drained in-flight results must not be committed after a scheduler failure"
        );
    }

    struct FinalizeFailTools;
    impl ToolRuntimeView for FinalizeFailTools {
        fn schemas(&self) -> Vec<ToolSchema> {
            Vec::new()
        }
        fn schemas_for(&self, _agent_id: Option<&str>) -> Vec<ToolSchema> {
            Vec::new()
        }
        fn sdk_schemas(&self) -> Vec<ToolSchema> {
            Vec::new()
        }
        fn is_ptc_active(&self) -> bool {
            false
        }
        fn restrict_for(
            &self,
            _agent_id: &str,
            _allow: Vec<String>,
        ) -> Result<cordis_rslite::EffectHandle, ToolError> {
            Ok(cordis_rslite::EffectHandle::none())
        }
        fn execution_mode(&self, _input: &ToolExecutionInput) -> ToolExecutionMode {
            ToolExecutionMode::Exclusive
        }
        fn prepare(
            &self,
            input: ToolExecutionInput,
        ) -> cordis_rslite::BoxFuture<Result<ScheduledToolPreparation, ToolError>> {
            let exec = ToolRunContext::new(starriver_kernel::tools::ToolExecution {
                cwd: None,
                sandbox_mode: None,
                call_id: input.call_id.clone(),
                root_call_id: input.call_id.clone(),
                name: input.name,
                arguments: input.arguments,
                agent_id: input.agent_id,
                parent: input.parent,
                token: starriver_kernel::tools::mint_tool_execution_token(),
                cancellation: input.cancellation,
            });
            Box::pin(async move { Ok(ScheduledToolPreparation::Dispatch { exec }) })
        }
        fn dispatch(
            &self,
            _exec: ToolRunContext,
        ) -> cordis_rslite::BoxFuture<Result<ScheduledToolDispatch, ToolError>> {
            Box::pin(async {
                Ok(ScheduledToolDispatch::PostResult {
                    result: success_result(),
                })
            })
        }
        fn finalize(
            &self,
            _exec: ToolRunContext,
            _result: ToolExecutionResult,
        ) -> cordis_rslite::BoxFuture<Result<ToolExecutionResult, ToolError>> {
            Box::pin(async { Err(ToolError::NotFound("finalize boom".into())) })
        }
        fn finish(
            &self,
            _exec: ToolRunContext,
            result: ToolExecutionResult,
        ) -> Result<ToolExecutionResult, ToolError> {
            Ok(result)
        }

        fn presentation_meta(
            &self,
            _input: &ToolExecutionInput,
            _value: &cordis_rslite::Value,
        ) -> Option<serde_json::Value> {
            None
        }
    }

    #[tokio::test]
    async fn finalize_failure_propagates_without_result() {
        let session = Session::new(SessionId("s1".into()), EventBus::new());
        let signal = CancellationToken::new();

        let error = execute_tool_calls(
            ToolCallStep {
                tools: &FinalizeFailTools,
                session: &session,
                agent_id: "a1",
                turn: 1,
                step: 1,
            },
            vec![tool_call("c1")],
            &signal,
            10,
            &|_| {},
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ToolError::NotFound(_)));

        let events = session.events();
        assert_eq!(
            events
                .iter()
                .filter(|e| e.event_type == "tool/call")
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| e.event_type == "tool/result")
                .count(),
            0
        );
    }

    struct SlowParallelTools;
    impl ToolRuntimeView for SlowParallelTools {
        fn schemas(&self) -> Vec<ToolSchema> {
            Vec::new()
        }
        fn schemas_for(&self, _agent_id: Option<&str>) -> Vec<ToolSchema> {
            Vec::new()
        }
        fn sdk_schemas(&self) -> Vec<ToolSchema> {
            Vec::new()
        }
        fn is_ptc_active(&self) -> bool {
            false
        }
        fn restrict_for(
            &self,
            _agent_id: &str,
            _allow: Vec<String>,
        ) -> Result<cordis_rslite::EffectHandle, ToolError> {
            Ok(cordis_rslite::EffectHandle::none())
        }
        fn execution_mode(&self, _input: &ToolExecutionInput) -> ToolExecutionMode {
            ToolExecutionMode::Parallel
        }
        fn prepare(
            &self,
            input: ToolExecutionInput,
        ) -> cordis_rslite::BoxFuture<Result<ScheduledToolPreparation, ToolError>> {
            let exec = ToolRunContext::new(starriver_kernel::tools::ToolExecution {
                cwd: None,
                sandbox_mode: None,
                call_id: input.call_id.clone(),
                root_call_id: input.call_id.clone(),
                name: input.name,
                arguments: input.arguments,
                agent_id: input.agent_id,
                parent: input.parent,
                token: starriver_kernel::tools::mint_tool_execution_token(),
                cancellation: input.cancellation,
            });
            Box::pin(async move { Ok(ScheduledToolPreparation::Dispatch { exec }) })
        }
        fn dispatch(
            &self,
            _exec: ToolRunContext,
        ) -> cordis_rslite::BoxFuture<Result<ScheduledToolDispatch, ToolError>> {
            Box::pin(async {
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                Ok(ScheduledToolDispatch::PostResult {
                    result: success_result(),
                })
            })
        }
        fn finalize(
            &self,
            _exec: ToolRunContext,
            result: ToolExecutionResult,
        ) -> cordis_rslite::BoxFuture<Result<ToolExecutionResult, ToolError>> {
            Box::pin(async { Ok(result) })
        }
        fn finish(
            &self,
            _exec: ToolRunContext,
            result: ToolExecutionResult,
        ) -> Result<ToolExecutionResult, ToolError> {
            Ok(result)
        }

        fn presentation_meta(
            &self,
            _input: &ToolExecutionInput,
            _value: &cordis_rslite::Value,
        ) -> Option<serde_json::Value> {
            None
        }
    }

    /// Cancel-path fixture: "fast" commits in ~1ms, any other call id never
    /// resolves — the wedged-dispatch shape (hung bash child) the driver must
    /// still be able to abort out of.
    struct HangTools;
    impl ToolRuntimeView for HangTools {
        fn schemas(&self) -> Vec<ToolSchema> {
            Vec::new()
        }
        fn schemas_for(&self, _agent_id: Option<&str>) -> Vec<ToolSchema> {
            Vec::new()
        }
        fn sdk_schemas(&self) -> Vec<ToolSchema> {
            Vec::new()
        }
        fn is_ptc_active(&self) -> bool {
            false
        }
        fn restrict_for(
            &self,
            _agent_id: &str,
            _allow: Vec<String>,
        ) -> Result<cordis_rslite::EffectHandle, ToolError> {
            Ok(cordis_rslite::EffectHandle::none())
        }
        fn execution_mode(&self, _input: &ToolExecutionInput) -> ToolExecutionMode {
            ToolExecutionMode::Parallel
        }
        fn prepare(
            &self,
            input: ToolExecutionInput,
        ) -> cordis_rslite::BoxFuture<Result<ScheduledToolPreparation, ToolError>> {
            let exec = ToolRunContext::new(starriver_kernel::tools::ToolExecution {
                cwd: None,
                sandbox_mode: None,
                call_id: input.call_id.clone(),
                root_call_id: input.call_id.clone(),
                name: input.name,
                arguments: input.arguments,
                agent_id: input.agent_id,
                parent: input.parent,
                token: starriver_kernel::tools::mint_tool_execution_token(),
                cancellation: input.cancellation,
            });
            Box::pin(async move { Ok(ScheduledToolPreparation::Dispatch { exec }) })
        }
        fn dispatch(
            &self,
            exec: ToolRunContext,
        ) -> cordis_rslite::BoxFuture<Result<ScheduledToolDispatch, ToolError>> {
            let is_fast = exec.execution.call_id.0 == "fast";
            Box::pin(async move {
                if !is_fast {
                    std::future::pending::<()>().await;
                }
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                Ok(ScheduledToolDispatch::PostResult {
                    result: success_result(),
                })
            })
        }
        fn finalize(
            &self,
            _exec: ToolRunContext,
            result: ToolExecutionResult,
        ) -> cordis_rslite::BoxFuture<Result<ToolExecutionResult, ToolError>> {
            Box::pin(async move { Ok(result) })
        }
        fn finish(
            &self,
            _exec: ToolRunContext,
            result: ToolExecutionResult,
        ) -> Result<ToolExecutionResult, ToolError> {
            Ok(result)
        }
        fn presentation_meta(
            &self,
            _input: &ToolExecutionInput,
            _value: &cordis_rslite::Value,
        ) -> Option<serde_json::Value> {
            None
        }
    }

    #[tokio::test]
    async fn abort_mid_flight_aborts_started_and_skips_remaining() {
        let session = Session::new(SessionId("s1".into()), EventBus::new());
        let signal = CancellationToken::new();

        let run = execute_tool_calls(
            ToolCallStep {
                tools: &SlowParallelTools,
                session: &session,
                agent_id: "a1",
                turn: 1,
                step: 1,
            },
            vec![tool_call("c1"), tool_call("c2")],
            &signal,
            1,
            &|_| {},
        );
        let cancel_task = async {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            signal.cancel();
        };
        let (concluded, ()) = tokio::join!(run, cancel_task);
        let concluded = concluded.unwrap();
        assert!(!concluded);

        let events = session.events();
        assert_eq!(
            events
                .iter()
                .filter(|e| e.event_type == "tool/call")
                .count(),
            2
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| e.event_type == "tool/result")
                .count(),
            2
        );
        let results = events
            .iter()
            .filter(|e| e.event_type == "tool/result")
            .collect::<Vec<_>>();
        // Cancel lands while c1 is in flight (30ms dispatch, 10ms cancel): the
        // started call is dropped and closed with the mid-run code; c2 never
        // started and gets the synthetic before-dispatch code.
        assert_eq!(
            results[0]
                .data
                .get("error")
                .and_then(|e| e.as_str_at("code")),
            Some("ABORTED_WHILE_RUNNING")
        );
        assert_eq!(
            results[1]
                .data
                .get("error")
                .and_then(|e| e.as_str_at("code")),
            Some("ABORTED_BEFORE_DISPATCH")
        );
    }

    /// Wedge regression: a dispatch that never resolves (hung bash child) must
    /// not park the driver — the cancel race ends the group and the call.
    #[tokio::test]
    async fn abort_returns_promptly_when_dispatch_never_resolves() {
        let session = Session::new(SessionId("s1".into()), EventBus::new());
        let signal = CancellationToken::new();
        let cancel_signal = signal.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            cancel_signal.cancel();
        });

        // "fast" commits in ~1ms; "hung" never resolves. Parallel mode, both
        // start together: the committed prefix survives, the hung one is
        // closed as aborted mid-run.
        let run = execute_tool_calls(
            ToolCallStep {
                tools: &HangTools,
                session: &session,
                agent_id: "a1",
                turn: 1,
                step: 1,
            },
            vec![tool_call("fast"), tool_call("hung")],
            &signal,
            2,
            &|_| {},
        );
        let concluded = tokio::time::timeout(std::time::Duration::from_secs(5), run)
            .await
            .expect("driver wedged on never-resolving dispatch")
            .unwrap();
        assert!(!concluded);

        let events = session.events();
        let results = events
            .iter()
            .filter(|e| e.event_type == "tool/result")
            .collect::<Vec<_>>();
        assert_eq!(results.len(), 2);
        assert!(results[0].data.get("error").is_none());
        assert_eq!(
            results[1]
                .data
                .get("error")
                .and_then(|e| e.as_str_at("code")),
            Some("ABORTED_WHILE_RUNNING")
        );
    }

    #[tokio::test]
    async fn abort_before_dispatch_skips_all_calls() {
        let session = Session::new(SessionId("s1".into()), EventBus::new());
        let signal = CancellationToken::new();
        signal.cancel();

        let concluded = execute_tool_calls(
            ToolCallStep {
                tools: &SlowParallelTools,
                session: &session,
                agent_id: "a1",
                turn: 1,
                step: 1,
            },
            vec![tool_call("c1"), tool_call("c2")],
            &signal,
            1,
            &|_| {},
        )
        .await
        .unwrap();
        assert!(!concluded);

        let events = session.events();
        assert_eq!(
            events
                .iter()
                .filter(|e| e.event_type == "tool/result")
                .filter(|e| {
                    e.data
                        .get("error")
                        .and_then(|x| x.as_str_at("code"))
                        .is_some_and(|c| c == "ABORTED_BEFORE_DISPATCH")
                })
                .count(),
            2
        );
    }

    // WI-47 Phase B: parsing moved to the kernel ingress
    // (`tool_call_arguments_from_text`); the loop only bridges
    // JsonValue → rslite Value. The old parse_arguments semantics are
    // re-asserted through that bridge.
    #[test]
    fn ingress_arguments_empty_maps_to_empty_object() {
        let value = starriver_kernel::llm::tool_call_arguments_from_text("").to_value();
        let json = JsonValue::from_value(&value).unwrap();
        assert_eq!(json, JsonValue::Object(Vec::new()));
    }

    #[test]
    fn ingress_arguments_valid_json_maps_primitives() {
        let value =
            starriver_kernel::llm::tool_call_arguments_from_text(r#"{"count": 3, "name": "x"}"#)
                .to_value();
        let json = JsonValue::from_value(&value).unwrap();
        assert_eq!(json.get("count").and_then(JsonValue::as_u64), Some(3));
        assert_eq!(json.get("name").and_then(JsonValue::as_str), Some("x"));
    }

    #[test]
    fn ingress_arguments_invalid_json_keeps_text() {
        let value = starriver_kernel::llm::tool_call_arguments_from_text("not json").to_value();
        assert!(value.downcast::<String>().is_ok());
    }

    /// WI-202 guard-level fixture: append one tool/call + one linked
    /// tool/result row the way the scheduler writes them.
    fn record_call_result(
        session: &starriver_kernel::session::Session,
        name: &str,
        arguments: &str,
        error: Option<(&str, &str)>,
    ) {
        use starriver_kernel::json::JsonValue;
        use starriver_kernel::session::{AppendOptions, SurfaceOp};
        let call_seq = session
            .append_with(
                "tool/call",
                JsonValue::Object(vec![
                    ("name".into(), JsonValue::String(name.into())),
                    (
                        "arguments".into(),
                        JsonValue::from_json_str(arguments).unwrap(),
                    ),
                ]),
                AppendOptions::default(),
            )
            .unwrap()
            .seq;
        let mut data = Vec::new();
        if let Some((code, message)) = error {
            data.push((
                "error".into(),
                JsonValue::Object(vec![
                    ("code".into(), JsonValue::String(code.into())),
                    ("message".into(), JsonValue::String(message.into())),
                ]),
            ));
        }
        session
            .append_with(
                "tool/result",
                JsonValue::Object(data),
                AppendOptions {
                    surface_op: Some(SurfaceOp::Append),
                    source_event_seqs: vec![call_seq],
                },
            )
            .unwrap();
    }

    fn sig_of(name: &str, arguments: &str) -> String {
        call_signature(name, &starriver_kernel::json::JsonValue::from_json_str(arguments).unwrap())
    }

    /// WI-202: transport-class failures (dead MCP endpoint, dropped request,
    /// timeout) must never feed the repeat-failure breaker — retrying after
    /// a while is the correct recovery for a transient outage.
    #[test]
    fn transport_failures_do_not_feed_the_repeat_breaker() {
        let bus = EventBus::new();
        let session = starriver_kernel::session::Session::new(SessionId("g1".into()), bus);
        let mut guard = TurnRepeatGuard::default();
        for _ in 0..3 {
            record_call_result(
                &session,
                "mcp_nexplan_call",
                r#"{"batch":"WI-199 regression"}"#,
                Some(("MCP_TIMEOUT", "mcp call timed out after 30s")),
            );
            guard.absorb_results(&session);
        }
        let sig = sig_of("mcp_nexplan_call", r#"{"batch":"WI-199 regression"}"#);
        assert_eq!(
            guard.consecutive_errors(&sig),
            0,
            "transport failures must not count toward the breaker"
        );
    }

    /// WI-202: the exemption is narrow — a tool that EXECUTED and returned a
    /// business error still counts, and a success still clears the tally.
    #[test]
    fn business_failures_still_count_and_success_clears() {
        let bus = EventBus::new();
        let session = starriver_kernel::session::Session::new(SessionId("g2".into()), bus);
        let mut guard = TurnRepeatGuard::default();
        let args = r#"{"path":"/x"}"#;
        record_call_result(&session, "probe", args, Some(("MCP_IS_ERROR", "server said no")));
        record_call_result(&session, "probe", args, Some(("MCP_IS_ERROR", "server said no")));
        guard.absorb_results(&session);
        let sig = sig_of("probe", args);
        assert_eq!(guard.consecutive_errors(&sig), 2);
        record_call_result(&session, "probe", args, None);
        guard.absorb_results(&session);
        assert_eq!(guard.consecutive_errors(&sig), 0, "success clears the tally");
    }

    /// WI-202: the per-turn reset must keep the absorb cursor — zeroing it
    /// replayed the whole session history on the next absorb, re-counting
    /// prior turns' failures into the fresh turn's tally (a single retry of
    /// a tool that failed once LAST turn tripped the breaker immediately).
    #[test]
    fn turn_reset_keeps_the_absorb_cursor() {
        let bus = EventBus::new();
        let session = starriver_kernel::session::Session::new(SessionId("g3".into()), bus);
        let args = r#"{"path":"/x"}"#;
        record_call_result(&session, "probe", args, Some(("NOT_FOUND", "gone")));
        let mut guard = TurnRepeatGuard::default();
        guard.absorb_results(&session);
        let sig = sig_of("probe", args);
        assert_eq!(guard.consecutive_errors(&sig), 1);

        // Turn end: reset. The tally clears (a fresh turn starts cold), and
        // — the WI-202 fix — the kept cursor means the re-absorb must NOT
        // replay history: the old Default reset rewound the cursor to 0 and
        // resurrected the prior failure into the fresh tally (count would
        // come back as 1 here).
        guard.reset_keeping_cursor();
        guard.absorb_results(&session);
        assert_eq!(
            guard.consecutive_errors(&sig),
            0,
            "prior-turn failures must not resurrect after the reset"
        );

        // A NEW failure in the fresh turn counts normally.
        record_call_result(&session, "probe", args, Some(("NOT_FOUND", "gone")));
        guard.absorb_results(&session);
        assert_eq!(guard.consecutive_errors(&sig), 1);
    }
}
