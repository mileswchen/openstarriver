//! PTC (programmatic tool calls) transport: the `run_code` tool, the ptc-only
//! guard, and the model-facing program-toolchain prompt texts.
//!
//! Programs call the registry's tools through nested `ToolRuntime::execute`
//! runs, so every sub-call walks the same guarded pipeline (sandbox, approval,
//! guards, dispatch waterfall) as a native call, and each settled sub-dispatch
//! is shaped through `tools/code-dispatch-log` while only the outer curated
//! result enters model history — the `ptc.ts` contract.
//!
//! The program dialect is a JSON document, not an embedded scripting engine:
//! the Rust core carries no JS/Python runtime dependency, and a deterministic
//! document keeps sub-call boundaries explicit (each step is exactly one tool
//! execution), which is what approval auditing and the dispatch log reconstruct.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

use cordis_rslite::{EffectHandle, Value};
use starriver_kernel::llm::{CallId, ContentBlock};
use starriver_kernel::tools::{
    mint_tool_execution_token, CodeDispatchLog, ToolDefinition, ToolError, ToolExecution,
    ToolExecutionInput, ToolExecutionResult, ToolExecutor, ToolGuard, ToolRunContext, ToolRuntime,
};

// The prompt vocabulary lives in the kernel (the agent loop renders it from
// there without a tools-crate edge); re-exported so callers of the tools
// crate see one PTC surface.
pub use starriver_kernel::ptc::{sdk_section_text, PTC_ONLY_TEXT, RUN_CODE_NAME};

/// Upper bound on program steps: a batch, not an unbounded script.
const MAX_STEPS: usize = 32;
/// Reject absurd programs before parsing rather than mid-run.
const MAX_PROGRAM_BYTES: usize = 256 * 1024;
/// Per-step log preview cap in the model-facing result text.
const STEP_LOG_PREVIEW_CAP: usize = 512;
/// Cap on the curated return value serialized into the result text.
const RETURN_TEXT_CAP: usize = 64 * 1024;

/// Shared PTC-active flag: presentation (schema projection, prompt sections)
/// and enforcement (the ptc-only guard) read one handle, so they can never
/// disagree about whether the mode is on.
pub type PtcActive = Arc<AtomicBool>;

// ── program document ────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ProgramStep {
    pub label: Option<String>,
    pub call: String,
    pub args: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct Program {
    pub steps: Vec<ProgramStep>,
    /// Optional `return` reference curating the program result.
    pub ret: Option<StepRef>,
}

/// A `$label` / `$index` reference with its optional `.field.path` navigation.
#[derive(Debug, Clone, PartialEq)]
pub struct StepRef {
    pub head: RefHead,
    pub path: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RefHead {
    Label(String),
    Index(usize),
}

fn invalid_program(message: impl Into<String>) -> ToolError {
    ToolError::Execution {
        tool: RUN_CODE_NAME.into(),
        code: "INVALID_PROGRAM".into(),
        message: message.into(),
    }
}

fn parse_program(program: &str) -> Result<Program, ToolError> {
    if program.len() > MAX_PROGRAM_BYTES {
        return Err(invalid_program(format!(
            "program exceeds the {} KiB budget",
            MAX_PROGRAM_BYTES / 1024
        )));
    }
    let doc: serde_json::Value = serde_json::from_str(program)
        .map_err(|error| invalid_program(format!("program is not valid JSON: {error}")))?;
    let doc = doc
        .as_object()
        .ok_or_else(|| invalid_program("program must be a JSON object"))?;

    let steps_json = doc
        .get("steps")
        .and_then(|value| value.as_array())
        .ok_or_else(|| invalid_program("program.steps must be an array"))?;
    if steps_json.is_empty() {
        return Err(invalid_program("program.steps must not be empty"));
    }
    if steps_json.len() > MAX_STEPS {
        return Err(invalid_program(format!(
            "program.steps exceeds the {MAX_STEPS}-step budget"
        )));
    }
    let mut steps = Vec::with_capacity(steps_json.len());
    let mut labels: HashMap<String, usize> = HashMap::new();
    for (index, step_json) in steps_json.iter().enumerate() {
        let step = step_json
            .as_object()
            .ok_or_else(|| invalid_program(format!("steps[{index}] must be an object")))?;
        let call = step
            .get("call")
            .and_then(|value| value.as_str())
            .ok_or_else(|| invalid_program(format!("steps[{index}].call must be a tool name")))?;
        if call.is_empty() {
            return Err(invalid_program(format!(
                "steps[{index}].call must not be empty"
            )));
        }
        if call == RUN_CODE_NAME {
            return Err(invalid_program(
                "run_code cannot be called from inside a program",
            ));
        }
        let args = step.get("args").cloned().unwrap_or(serde_json::Value::Null);
        if !args.is_null() && !args.is_object() {
            return Err(invalid_program(format!(
                "steps[{index}].args must be an object"
            )));
        }
        let label = match step.get("as") {
            None => None,
            Some(value) => {
                let label = value.as_str().ok_or_else(|| {
                    invalid_program(format!("steps[{index}].as must be a string"))
                })?;
                validate_label(label)
                    .map_err(|reason| invalid_program(format!("steps[{index}].as {reason}")))?;
                if labels.contains_key(label) {
                    return Err(invalid_program(format!("duplicate step label {label:?}")));
                }
                labels.insert(label.to_string(), index);
                Some(label.to_string())
            }
        };
        steps.push(ProgramStep {
            label,
            call: call.to_string(),
            args,
        });
    }
    let ret = match doc.get("return") {
        None => None,
        Some(value) => {
            let text = value
                .as_str()
                .ok_or_else(|| invalid_program("program.return must be a reference string"))?;
            Some(parse_ref(text).map_err(invalid_program)?)
        }
    };
    Ok(Program { steps, ret })
}

/// Labels share one namespace with `$index` references: a digit-leading label
/// would be unreferenceable (parsed as an index), so labels start letter/`_`.
fn validate_label(label: &str) -> Result<&str, String> {
    let first = label.chars().next();
    let valid = first.is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && label.len() <= 32
        && label
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if valid {
        Ok(label)
    } else {
        Err("must match [a-z_][a-z0-9_]* and stay under 32 characters".into())
    }
}

/// Parse one `$ref` occurrence at `text`'s start. Returns the ref and the
/// consumed byte length, or `None` when the text does not begin a ref. Heads
/// and path segments are ASCII, so byte scanning is exact.
fn scan_ref(text: &str) -> Option<(StepRef, usize)> {
    let bytes = text.as_bytes();
    if bytes.first() != Some(&b'$') {
        return None;
    }
    let head_start = 1;
    let mut i = head_start;
    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
        i += 1;
    }
    if i == head_start {
        return None;
    }
    let head = &text[head_start..i];
    let head_ref = if head.bytes().all(|b| b.is_ascii_digit()) {
        RefHead::Index(head.parse().ok()?)
    } else {
        RefHead::Label(head.to_string())
    };
    let mut path = Vec::new();
    while i < bytes.len() && bytes[i] == b'.' {
        let segment_start = i + 1;
        let mut j = segment_start;
        while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
            j += 1;
        }
        if j == segment_start {
            return None;
        }
        path.push(text[segment_start..j].to_string());
        i = j;
    }
    Some((
        StepRef {
            head: head_ref,
            path,
        },
        i,
    ))
}

/// Parse a whole-string reference (`$label`, `$3`, `$label.files.0`).
fn parse_ref(text: &str) -> Result<StepRef, String> {
    let (reference, consumed) =
        scan_ref(text).ok_or_else(|| format!("{text:?} is not a $reference"))?;
    if consumed != text.len() {
        return Err(format!("{text:?} is not a whole-string $reference"));
    }
    Ok(reference)
}

/// Substitute `$references` throughout one args tree. A whole-string reference
/// splices the referenced value with its type; an embedded reference renders
/// the leaf (strings as-is, everything else as compact JSON).
fn substitute_refs(
    value: &serde_json::Value,
    resolve: &dyn Fn(&StepRef) -> Result<serde_json::Value, String>,
) -> Result<serde_json::Value, String> {
    match value {
        serde_json::Value::String(text) => substitute_in_string(text, resolve),
        serde_json::Value::Array(items) => items
            .iter()
            .map(|item| substitute_refs(item, resolve))
            .collect::<Result<Vec<_>, String>>()
            .map(serde_json::Value::Array),
        serde_json::Value::Object(map) => map
            .iter()
            .map(|(key, item)| substitute_refs(item, resolve).map(|item| (key.clone(), item)))
            .collect::<Result<serde_json::Map<String, serde_json::Value>, String>>()
            .map(serde_json::Value::Object),
        other => Ok(other.clone()),
    }
}

fn substitute_in_string(
    text: &str,
    resolve: &dyn Fn(&StepRef) -> Result<serde_json::Value, String>,
) -> Result<serde_json::Value, String> {
    if let Ok(reference) = parse_ref(text) {
        return resolve(&reference);
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(dollar) = rest.find('$') {
        match scan_ref(&rest[dollar..]) {
            Some((reference, consumed)) => {
                out.push_str(&rest[..dollar]);
                let leaf = resolve(&reference)?;
                match leaf {
                    serde_json::Value::String(leaf_text) => out.push_str(&leaf_text),
                    other => out.push_str(&serde_json::to_string(&other).unwrap_or_default()),
                }
                rest = &rest[dollar + consumed..];
            }
            None => {
                out.push_str(&rest[..dollar + 1]);
                rest = &rest[dollar + 1..];
            }
        }
    }
    out.push_str(rest);
    Ok(serde_json::Value::String(out))
}

/// Navigate one resolved reference through its `.field.path`.
fn navigate(mut value: serde_json::Value, path: &[String]) -> Result<serde_json::Value, String> {
    for segment in path {
        value = match &value {
            serde_json::Value::Object(map) => map
                .get(segment)
                .cloned()
                .ok_or_else(|| format!("reference path .{segment} not found"))?,
            serde_json::Value::Array(items) => {
                let index: usize = segment
                    .parse()
                    .map_err(|_| format!("reference path .{segment} is not an array index"))?;
                items
                    .get(index)
                    .cloned()
                    .ok_or_else(|| format!("reference path .{segment} out of bounds"))?
            }
            other => {
                return Err(format!(
                    "reference path .{segment} cannot navigate into {}",
                    json_kind(other)
                ))
            }
        };
    }
    Ok(value)
}

fn json_kind(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

fn compact_json(value: &serde_json::Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

// ── executor ────────────────────────────────────────────────────────────────

struct RunCodeExecutor {
    /// The owning runtime: sub-calls dispatch through its full pipeline and
    /// each settled sub-dispatch shapes through its dispatch-log waterfall.
    /// Weak — the runtime owns this definition, so a strong handle would cycle.
    runtime: Weak<crate::InMemoryToolRuntime>,
}

/// Normalize one cordis Value into plain JSON. The agent loop carries model
/// arguments in kernel JsonValue's RUNTIME spellings (`Vec<(String,
/// JsonValue)>` for objects, `Arc<JsonValue>`, ...), not as serde_json — the
/// same reason host tools parse through `starriver_kernel::json::JsonValue::from_value` first.
fn to_json_value(value: &Value) -> Option<serde_json::Value> {
    if let Ok(json) = starriver_kernel::json::JsonValue::from_value(value) {
        if let Ok(serde) = json.to_serde_json() {
            return Some(serde);
        }
    }
    value
        .downcast::<serde_json::Value>()
        .ok()
        .map(|arc| (*arc).clone())
}

fn json_args(arguments: &Value) -> Result<serde_json::Value, ToolError> {
    to_json_value(arguments)
        .ok_or_else(|| invalid_program("run_code arguments must be a JSON object"))
}

impl ToolExecutor for RunCodeExecutor {
    fn execute(
        &self,
        arguments: Value,
        ctx: ToolRunContext,
    ) -> cordis_rslite::BoxFuture<Result<Value, ToolError>> {
        let runtime = self.runtime.clone();
        Box::pin(async move {
            let runtime = runtime.upgrade().ok_or_else(|| ToolError::Execution {
                tool: RUN_CODE_NAME.into(),
                code: "CODE_RUN_FAILED".into(),
                message: "tool runtime is gone".into(),
            })?;

            let args = json_args(&arguments)?;
            // Description is a UI label, not a contract: a missing or empty
            // one degrades to a default instead of failing the whole call.
            let _description = args
                .get("description")
                .and_then(|value| value.as_str())
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .unwrap_or("run_code program");
            let program_text = match args.get("program") {
                Some(serde_json::Value::String(text)) => text.clone(),
                // Lenient: an unescaped object document is accepted as-is.
                Some(object @ serde_json::Value::Object(_)) => compact_json(object),
                Some(other) => {
                    return Err(invalid_program(format!(
                        "program must be a JSON document string, got {}",
                        json_kind(other)
                    )))
                }
                None => return Err(invalid_program("missing program")),
            };
            let program = parse_program(&program_text)?;

            // Outputs of settled steps, in step order; `None` while unset.
            // Mutex (not RefCell): the executor future must stay Send across
            // the sub-dispatch await, and `resolve` reads settled outputs
            // inside the args tree while the loop writes each result back.
            let outputs = std::sync::Mutex::new(Vec::<Option<serde_json::Value>>::with_capacity(
                program.steps.len(),
            ));
            let mut labels: HashMap<String, usize> = HashMap::new();
            for (index, step) in program.steps.iter().enumerate() {
                if let Some(label) = &step.label {
                    labels.insert(label.clone(), index);
                }
                outputs.lock().unwrap().push(None);
            }
            let resolve = |reference: &StepRef| -> Result<serde_json::Value, String> {
                let index = match &reference.head {
                    RefHead::Index(index) => *index,
                    RefHead::Label(label) => labels
                        .get(label)
                        .copied()
                        .ok_or_else(|| format!("unknown step label {label:?}"))?,
                };
                let settled = outputs
                    .lock()
                    .expect("ptc step outputs poisoned")
                    .get(index)
                    .and_then(|slot| slot.as_ref())
                    .cloned()
                    .ok_or_else(|| {
                        format!(
                            "step {index} has no output to reference (it failed or has not run)"
                        )
                    })?;
                navigate(settled, &reference.path)
            };

            let mut logs: Vec<String> = Vec::with_capacity(program.steps.len());
            let mut any_failed = false;
            let total = program.steps.len();
            for (index, step) in program.steps.iter().enumerate() {
                if ctx.cancellation().is_some_and(|token| token.is_cancelled()) {
                    return Err(ToolError::Execution {
                        tool: RUN_CODE_NAME.into(),
                        code: "CODE_RUN_FAILED".into(),
                        message: format!("program aborted before step {}/{total}", index + 1,),
                    });
                }
                // Unresolvable references SKIP the step instead of aborting:
                // the failing dependency is already logged as ERR, and the
                // skip keeps independent later steps runnable.
                let step_args = match substitute_refs(&step.args, &resolve) {
                    Ok(args) => args,
                    Err(reason) => {
                        logs.push(format!(
                            "[{}/{total}] SKIP {}: {reason}",
                            index + 1,
                            step_label(step),
                        ));
                        continue;
                    }
                };
                let call_id = CallId(format!("{}-s{index}", ctx.execution.call_id.0));
                // The logging twin of the execution minted inside the pipeline:
                // identity fields match, only the opaque token differs.
                let sub_execution = ToolExecution {
                    call_id: call_id.clone(),
                    root_call_id: ctx.execution.root_call_id.clone(),
                    name: step.call.clone(),
                    arguments: Value::new(step_args.clone()),
                    agent_id: ctx.execution.agent_id.clone(),
                    cwd: ctx.execution.cwd.clone(),
                    sandbox_mode: ctx.execution.sandbox_mode,
                    parent: Some(ctx.execution.token),
                    token: mint_tool_execution_token(),
                    cancellation: ctx.execution.cancellation.clone(),
                };
                let input = ToolExecutionInput {
                    call_id: call_id.clone(),
                    root_call_id: Some(ctx.execution.root_call_id.clone()),
                    name: step.call.clone(),
                    arguments: Value::new(step_args),
                    agent_id: ctx.execution.agent_id.clone(),
                    cwd: ctx.execution.cwd.clone(),
                    sandbox_mode: ctx.execution.sandbox_mode,
                    parent: Some(ctx.execution.token),
                    cancellation: ctx.execution.cancellation.clone(),
                };
                let outcome = runtime.execute(input).await;
                let (ok, is_error, content, value) = match outcome {
                    Ok(result) => match result {
                        ToolExecutionResult::Success { value, content, .. } => {
                            let json = to_json_value(&value).unwrap_or(serde_json::Value::Null);
                            (true, false, content, Some(json))
                        }
                        ToolExecutionResult::Failure { content, .. } => {
                            (false, true, content, None)
                        }
                    },
                    Err(error) => (
                        false,
                        true,
                        vec![ContentBlock::Text(error.to_string())],
                        None,
                    ),
                };
 // each settled sub-dispatch is shaped through the
                // dispatch-log waterfall; a failing listener never fails the run.
                let shaped = runtime
                    .shape_dispatch_log(CodeDispatchLog {
                        execution: sub_execution,
                        agent_id: ctx.execution.agent_id.clone(),
                        sub_call_id: call_id,
                        name: step.call.clone(),
                        is_error,
                        content,
                    })
                    .await;
                let preview = log_preview(&shaped);
                logs.push(format!(
                    "[{}/{}] {} {}: {preview}",
                    index + 1,
                    total,
                    if ok { "ok" } else { "ERR" },
                    step_label(step),
                ));
                outputs.lock().unwrap()[index] = value;
 // a failed sub-call surfaces as an error RESULT,
                // but it does not abort the program — the log carries the
                // output (e.g. a test run's PASS/FAIL lines), and only steps
                // referencing the failed output are skipped.
                if !ok {
                    any_failed = true;
                }
            }

            if any_failed {
                return Err(ToolError::Execution {
                    tool: RUN_CODE_NAME.into(),
                    code: "CODE_STEP_FAILED".into(),
                    message: format!(
                        "program finished with failures:\n{}",
                        capped(logs.join("\n"), RETURN_TEXT_CAP),
                    ),
                });
            }
            let result = match &program.ret {
                Some(reference) => resolve(reference).map_err(|reason| ToolError::Execution {
                    tool: RUN_CODE_NAME.into(),
                    code: "CODE_STEP_FAILED".into(),
                    message: format!("program.return failed: {reason}"),
                })?,
                None => serde_json::Value::Null,
            };
            Ok(Value::new(serde_json::json!({
                "logs": logs,
                "result": result,
            })))
        })
    }
}

/// `label: call` presentation for one log line.
fn step_label(step: &ProgramStep) -> String {
    match &step.label {
        Some(label) => format!("{label}: {}", step.call),
        None => step.call.clone(),
    }
}

/// Cap a log/message body, keeping the tail informative.
fn capped(text: String, cap: usize) -> String {
    if text.chars().count() <= cap {
        text
    } else {
        // BUG-20: the failure summary's value is in the LAST steps (the ERR
        // lines that explain the failure), so over-budget bodies keep the
        // tail, not the head.
        let tail: String = text
            .chars()
            .skip(text.chars().count() - cap)
            .collect();
        format!("…{tail}")
    }
}

fn log_preview(content: &[ContentBlock]) -> String {
    let text = content
        .iter()
        .map(|block| match block {
            ContentBlock::Text(text) => text.clone(),
            _ => String::new(),
        })
        .collect::<Vec<_>>()
        .join(" ");
    let flat = text.replace(['\n', '\r', '\t'], " ");
    if flat.chars().count() > STEP_LOG_PREVIEW_CAP {
        let truncated: String = flat.chars().take(STEP_LOG_PREVIEW_CAP).collect();
        format!("{truncated}…")
    } else {
        flat
    }
}

// ── definition + registration ───────────────────────────────────────────────

fn run_code_definition(runtime: Weak<crate::InMemoryToolRuntime>) -> ToolDefinition {
    ToolDefinition {
        name: RUN_CODE_NAME.into(),
        description: "Execute a batch program against the available tools. Takes two required \
            arguments: `program`, a JSON document whose `steps` call tools in sequence (each \
            step runs through the normal pipeline: sandbox, approval, guards), and \
            `description`, a short summary of what the program does. Reference earlier step \
            outputs inside any args string as `$label` (typed splice) or `$label.field.path` \
            (navigated; embedded references are JSON-encoded). A failed step is logged \
            (ERR) and does not abort the program — later steps referencing its output are \
            skipped, and if any step failed the run_code result is an error carrying the full \
            log. `return` curates the program result. \
            Only the step log and the return value are program output — curate it."
            .into(),
        parameters: Value::new(serde_json::json!({
            "type": "object",
            "properties": {
                "program": {
                    "type": "string",
                    "description": "The program: a JSON document {\"steps\": [{\"as\": \"label\", \
                        \"call\": \"<tool>\", \"args\": {...}}, ...], \"return\": \"$label\"}. \
                        1-32 steps; reference outputs as $label or $label.path."
                },
                "description": {
                    "type": "string",
                    "description": "Clear, concise description of what this program does in \
                        active voice, 5-10 words (shown in the UI)."
                }
            },
            "required": ["program", "description"]
        })),
        output: Value::new(serde_json::json!({
            "type": "object",
            "properties": {
                "logs": { "type": "array", "items": { "type": "string" } },
                "result": {}
            }
        })),
        requires_approval: false,
        deny_in_readonly: false,
        render: Arc::new(|_arguments, value| {
            let text = rendered_output(value);
            vec![ContentBlock::Text(text)]
        }),
        executor: Arc::new(RunCodeExecutor { runtime }),
        is_concurrency_safe: None,
        timeout: None,
        finalize_content: None,
    }
}

/// Model-facing output text: the step log, then the curated return value.
fn rendered_output(value: &Value) -> String {
    let json = to_json_value(value).unwrap_or(serde_json::Value::Null);
    let logs = json
        .get("logs")
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    let result = json.get("result").unwrap_or(&serde_json::Value::Null);
    let rendered_result = match result {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(text) => {
            if text.chars().count() > RETURN_TEXT_CAP {
                let truncated: String = text.chars().take(RETURN_TEXT_CAP).collect();
                format!("{truncated}…")
            } else {
                text.clone()
            }
        }
        other => {
            let compact = compact_json(other);
            if compact.chars().count() > RETURN_TEXT_CAP {
                let truncated: String = compact.chars().take(RETURN_TEXT_CAP).collect();
                format!("{truncated}…")
            } else {
                compact
            }
        }
    };
    let candidates = [logs, rendered_result];
    let parts: Vec<&str> = candidates
        .iter()
        .map(|part| part.as_str())
        .filter(|part| !part.is_empty())
        .collect();
    if parts.is_empty() {
        "(run_code completed with no output)".into()
    } else {
        parts.join("\n")
    }
}

/// Register the `run_code` definition and the ptc-only guard on `runtime`.
/// Presentation stays inert until `set_ptc_active(true)`; the definition is
/// reserved presentation infrastructure and lives outside schema filtering.
pub fn register_run_code(
    runtime: &Arc<crate::InMemoryToolRuntime>,
) -> Result<EffectHandle, ToolError> {
    let definition = run_code_definition(Arc::downgrade(runtime));
    let registration = runtime.register(definition)?;
    let active = runtime.ptc_handle();
    let guard: ToolGuard = Arc::new(move |execution| {
        // Sub-calls (`parent` set) are the program's own dispatches; the deny
        // targets direct model calls only. The runtime is gone → fail open;
        // nothing would execute it anyway.
        if active.load(Ordering::SeqCst)
            && execution.name != RUN_CODE_NAME
            && execution.parent.is_none()
        {
            Some(format!(
                "PTC mode: `{}` cannot be called directly — `run_code` is the only direct \
                 tool; call `{}` from inside the program.",
                execution.name, execution.name
            ))
        } else {
            None
        }
    });
    let guard_handle = runtime.guard(guard)?;
    // Disposing the returned handle must remove BOTH the guard and the tool
    // registration — the registration's disposer was dropped here before,
    // leaving run_code permanently registered (a PTC off/on cycle then
    // failed with AlreadyRegistered forever).
    Ok(EffectHandle::new_async(move || {
        Box::pin(async move {
            registration.dispose_async().await;
            guard_handle.dispose_async().await;
        })
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cordis_rslite::EventBus;
    use starriver_kernel::llm::CallId;
    use starriver_kernel::tools::{ToolExecutionInput, ToolExecutionResult, ToolsCodeDispatchLog};
    use std::sync::Mutex;

    fn echo_tool() -> ToolDefinition {
        ToolDefinition {
            name: "echo".into(),
            description: "Echo the input text back.".into(),
            parameters: Value::new(serde_json::json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"]
            })),
            output: Value::new(serde_json::json!({ "type": "object" })),
            requires_approval: false,
            deny_in_readonly: false,
            render: Arc::new(|_arguments, value| {
                let text = value
                    .downcast::<serde_json::Value>()
                    .map(|arc| (*arc).clone())
                    .ok()
                    .and_then(|json| {
                        json.get("output")
                            .and_then(|o| o.as_str())
                            .map(String::from)
                    })
                    .unwrap_or_default();
                vec![ContentBlock::Text(text)]
            }),
            executor: Arc::new(EchoExecutor),
            is_concurrency_safe: None,
            timeout: None,
            finalize_content: None,
        }
    }

    struct EchoExecutor;

    impl ToolExecutor for EchoExecutor {
        fn execute(
            &self,
            arguments: Value,
            _ctx: ToolRunContext,
        ) -> cordis_rslite::BoxFuture<Result<Value, ToolError>> {
            Box::pin(async move {
                let json = arguments
                    .downcast::<serde_json::Value>()
                    .map(|arc| (*arc).clone())
                    .unwrap_or(serde_json::Value::Null);
                Ok(Value::new(serde_json::json!({
                    "output": json.get("text").cloned().unwrap_or(serde_json::Value::Null),
                    "text": json.get("text").cloned().unwrap_or(serde_json::Value::Null),
                })))
            })
        }
    }

    fn failing_tool() -> ToolDefinition {
        ToolDefinition {
            name: "boom".into(),
            description: "Always fails.".into(),
            parameters: Value::new(serde_json::json!({ "type": "object" })),
            output: Value::new(serde_json::json!({ "type": "object" })),
            requires_approval: false,
            deny_in_readonly: false,
            render: Arc::new(|_arguments, _value| vec![ContentBlock::Text(String::new())]),
            executor: Arc::new(BoomExecutor),
            is_concurrency_safe: None,
            timeout: None,
            finalize_content: None,
        }
    }

    struct BoomExecutor;

    impl ToolExecutor for BoomExecutor {
        fn execute(
            &self,
            _arguments: Value,
            _ctx: ToolRunContext,
        ) -> cordis_rslite::BoxFuture<Result<Value, ToolError>> {
            Box::pin(async move {
                Err(ToolError::Execution {
                    tool: "boom".into(),
                    code: "BOOM".into(),
                    message: "exploded".into(),
                })
            })
        }
    }

    fn setup() -> (Arc<crate::InMemoryToolRuntime>, EventBus) {
        let bus = EventBus::new();
        let runtime = Arc::new(crate::InMemoryToolRuntime::new(
            bus.clone(),
            cordis_rslite::Context::new(),
        ));
        runtime.register(echo_tool()).unwrap();
        runtime.register(failing_tool()).unwrap();
        register_run_code(&runtime).unwrap();
        (runtime, bus)
    }

    fn run_input(name: &str, arguments: serde_json::Value) -> ToolExecutionInput {
        ToolExecutionInput {
            call_id: CallId("t1".into()),
            root_call_id: None,
            name: name.into(),
            arguments: Value::new(arguments),
            agent_id: None,
            cwd: None,
            sandbox_mode: None,
            parent: None,
            cancellation: None,
        }
    }

    fn run_input_cordis(name: &str, arguments: Value) -> ToolExecutionInput {
        let mut input = run_input(name, serde_json::Value::Null);
        input.arguments = arguments;
        input
    }

    async fn run_ok(
        runtime: &crate::InMemoryToolRuntime,
        program: serde_json::Value,
    ) -> serde_json::Value {
        let result = runtime
            .execute(run_input(
                RUN_CODE_NAME,
                serde_json::json!({ "program": compact_json(&program), "description": "test program" }),
            ))
            .await
            .unwrap();
        match result {
            ToolExecutionResult::Success { value, .. } => value
                .downcast::<serde_json::Value>()
                .map(|arc| (*arc).clone())
                .unwrap(),
            other => panic!("expected success, got {other:?}"),
        }
    }

    /// Executor errors surface through the pipeline as `Ok(Failure)` results,
    /// never as `Err` — assert on the canonical failure code and text.
    async fn run_failed(
        runtime: &crate::InMemoryToolRuntime,
        arguments: serde_json::Value,
    ) -> (String, String) {
        let result = runtime
            .execute(run_input(RUN_CODE_NAME, arguments))
            .await
            .unwrap();
        match result {
            ToolExecutionResult::Failure { error, .. } => (error.code, error.message),
            other => panic!("expected failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn program_runs_steps_and_splices_references() {
        let (runtime, _bus) = setup();
        let program = serde_json::json!({
            "steps": [
                { "as": "first", "call": "echo", "args": { "text": "hello" } },
                { "as": "second", "call": "echo", "args": { "text": "said $first.text!" } },
                { "call": "echo", "args": { "text": "$first" } }
            ],
            "return": "$second.text"
        });
        let output = run_ok(&runtime, program).await;
        let logs = output
            .get("logs")
            .and_then(|value| value.as_array())
            .unwrap();
        assert_eq!(logs.len(), 3);
        assert!(logs[0].as_str().unwrap().starts_with("[1/3] ok"));
        assert!(logs[1].as_str().unwrap().contains("said hello!"));
        // `$second.text` navigates into the step output value.
        assert_eq!(
            output.get("result").and_then(|value| value.as_str()),
            Some("said hello!")
        );
    }

    /// The agent loop wraps model arguments in kernel JsonValue's runtime
    /// spellings (this is how production arguments arrive — the host tools'
    /// `json_args` documents the same contract). Regression: parse them.
    #[tokio::test]
    async fn parses_kernel_jsonvalue_argument_spellings() {
        let (runtime, _bus) = setup();
        let program_json = serde_json::json!({
            "steps": [{ "call": "echo", "args": { "text": "spelled" } }],
            "return": "$0.text"
        });
        let arguments = Value::new(starriver_kernel::json::JsonValue::Object(vec![
            (
                "program".into(),
                starriver_kernel::json::JsonValue::String(compact_json(&program_json)),
            ),
            (
                "description".into(),
                starriver_kernel::json::JsonValue::String("runtime spelling".into()),
            ),
        ]));
        let result = runtime
            .execute(run_input_cordis(RUN_CODE_NAME, arguments))
            .await
            .unwrap();
        match result {
            ToolExecutionResult::Success { content, .. } => {
                let text = content
                    .iter()
                    .map(|block| match block {
                        ContentBlock::Text(text) => text.clone(),
                        _ => String::new(),
                    })
                    .collect::<String>();
                assert!(text.contains("spelled"), "output: {text}");
            }
            other => panic!("expected success, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn failed_step_aborts_with_committed_digest() {
        let (runtime, _bus) = setup();
        let program = serde_json::json!({
            "steps": [
                { "as": "ok_step", "call": "echo", "args": { "text": "fine" } },
                { "call": "boom", "args": {} }
            ]
        });
        let (code, message) = run_failed(
            &runtime,
            serde_json::json!({ "program": compact_json(&program), "description": "boom program" }),
        )
        .await;
        assert_eq!(code, "CODE_STEP_FAILED");
        assert!(message.contains("boom"), "message: {message}");
        assert!(message.contains("ok_step"), "digest missing: {message}");
    }

    #[tokio::test]
    async fn independent_steps_run_after_a_failure() {
        let (runtime, _bus) = setup();
        // boom fails (its output is unreferenceable), but the independent
        // echo step still runs — the failure no longer aborts the program.
        let program = serde_json::json!({
            "steps": [
                { "call": "boom", "args": {} },
                { "call": "echo", "args": { "text": "still ran" } }
            ]
        });
        let (code, message) = run_failed(
            &runtime,
            serde_json::json!({ "program": compact_json(&program), "description": "partial failure" }),
        )
        .await;
        assert_eq!(code, "CODE_STEP_FAILED");
        assert!(message.contains("[1/2] ERR"), "message: {message}");
        assert!(message.contains("[2/2] ok"), "message: {message}");
    }

    #[tokio::test]
    async fn missing_description_defaults_instead_of_failing() {
        let (runtime, _bus) = setup();
        let result = runtime
            .execute(run_input(
                RUN_CODE_NAME,
                serde_json::json!({ "program": compact_json(&serde_json::json!({
                    "steps": [{ "call": "echo", "args": { "text": "hi" } }]
                })) }),
            ))
            .await
            .unwrap();
        assert!(matches!(result, ToolExecutionResult::Success { .. }));
    }

    #[tokio::test]
    async fn dispatch_log_fires_per_sub_call() {
        let (runtime, bus) = setup();
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let _listener = bus.on_waterfall::<ToolsCodeDispatchLog, _>(move |event, next| {
            let sink = Arc::clone(&sink);
            Box::pin(async move {
                sink.lock().unwrap().push(event.dispatch.name.clone());
                next().await
            })
        });
        let program = serde_json::json!({
            "steps": [
                { "call": "echo", "args": { "text": "a" } },
                { "call": "echo", "args": { "text": "b" } }
            ]
        });
        run_ok(&runtime, program).await;
        assert_eq!(
            &*seen.lock().unwrap(),
            &["echo".to_string(), "echo".to_string()]
        );
    }

    #[tokio::test]
    async fn ptc_active_denies_native_calls_and_hides_schemas() {
        let (runtime, _bus) = setup();
        runtime.set_ptc_active(true);

        // Presentation: only run_code reaches the model.
        let names = runtime
            .schemas()
            .into_iter()
            .map(|schema| schema.name)
            .collect::<Vec<_>>();
        assert_eq!(names, vec![RUN_CODE_NAME.to_string()]);

        // Enforcement: a direct native call is denied by the guard.
        let result = runtime
            .execute(run_input("echo", serde_json::json!({ "text": "hi" })))
            .await
            .unwrap();
        let failure = match result {
            ToolExecutionResult::Failure { error, .. } => error,
            other => panic!("expected denial, got {other:?}"),
        };
        assert!(failure.message.contains("run_code"), "error: {failure:?}");

        // Sub-calls pass: the program still works while the mode is active.
        let program = serde_json::json!({
            "steps": [{ "call": "echo", "args": { "text": "still works" } }]
        });
        run_ok(&runtime, program).await;

        // Deactivating restores native presentation (run_code hidden again)
        // and calls.
        runtime.set_ptc_active(false);
        let mut names = runtime
            .schemas()
            .into_iter()
            .map(|schema| schema.name)
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(names, vec!["boom".to_string(), "echo".to_string()]);
        runtime
            .execute(run_input("echo", serde_json::json!({ "text": "hi" })))
            .await
            .unwrap();
    }

    /// The production failure shape: the coding preset's visible-tools
    /// whitelist names native tools only, so the restriction intersection
    /// must not strand PTC mode with zero tools — the reserved transport
    /// outranks the whitelist and stays the sole presented tool.
    #[tokio::test]
    async fn ptc_presentation_outranks_visible_tools_whitelist() {
        let (runtime, _bus) = setup();
        runtime
            .restrict_for("coding", vec!["echo".into(), "boom".into()])
            .unwrap();
        runtime.set_ptc_active(true);

        let names = runtime
            .schemas_for(Some("coding"))
            .into_iter()
            .map(|schema| schema.name)
            .collect::<Vec<_>>();
        assert_eq!(names, vec![RUN_CODE_NAME.to_string()]);

        // The whitelist still governs native presentation once PTC is off:
        // run_code stays hidden from the model.
        runtime.set_ptc_active(false);
        let mut names = runtime
            .schemas_for(Some("coding"))
            .into_iter()
            .map(|schema| schema.name)
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(names, vec!["boom".to_string(), "echo".to_string()]);
    }

    #[tokio::test]
    async fn invalid_programs_reject() {
        let (runtime, _bus) = setup();
        let cases: Vec<String> = vec![
            "not json".to_string(),
            "{}".to_string(),
            serde_json::json!({ "steps": [] }).to_string(),
            serde_json::json!({ "steps": [{ "call": "run_code", "args": {} }] }).to_string(),
            serde_json::json!({ "steps": [
                { "as": "a", "call": "echo", "args": {} },
                { "as": "a", "call": "echo", "args": {} }
            ] })
            .to_string(),
            serde_json::json!({ "steps": [{ "call": "echo", "args": {} }], "return": "label" })
                .to_string(),
        ];
        for program in &cases {
            let (code, _) = run_failed(
                &runtime,
                serde_json::json!({ "program": program, "description": "invalid" }),
            )
            .await;
            assert_eq!(code, "INVALID_PROGRAM", "program: {program}");
        }
    }

    #[tokio::test]
    async fn unknown_reference_skips_the_step() {
        let (runtime, _bus) = setup();
        let program = serde_json::json!({
            "steps": [{ "call": "echo", "args": { "text": "$nope" } }]
        });
        // An unresolvable reference skips its step (logged SKIP) instead of
        // aborting; with no other failures the program still succeeds.
        let output = run_ok(
            &runtime,
            serde_json::json!({ "steps": [{ "call": "echo", "args": { "text": "$nope" } }] }),
        )
        .await;
        let logs = output
            .get("logs")
            .and_then(|value| value.as_array())
            .unwrap();
        assert!(logs[0].as_str().unwrap().contains("SKIP"), "logs: {logs:?}");

        // ...but a failed dependency still fails the run when it happens.
        let program = serde_json::json!({
            "steps": [
                { "as": "gone", "call": "boom", "args": {} },
                { "call": "echo", "args": { "text": "$gone" } }
            ]
        });
        let (code, message) = run_failed(
            &runtime,
            serde_json::json!({ "program": compact_json(&program), "description": "dep failure" }),
        )
        .await;
        assert_eq!(code, "CODE_STEP_FAILED");
        assert!(message.contains("SKIP"), "message: {message}");
    }

    /// BUG-20 regression: the CODE_STEP_FAILED summary caps the joined log —
    /// "keeping the tail informative" (the fn's own contract) because the
    /// last steps carry the actual failure; dropping them hides the reason
    /// the program failed.
    #[test]
    fn capped_keeps_the_informative_tail() {
        let head = "x".repeat(RETURN_TEXT_CAP + 64);
        let text = format!("{head}\nFINAL-FAILURE-DETAIL: assertion failed");
        let out = capped(text.clone(), RETURN_TEXT_CAP);
        assert!(
            out.contains("FINAL-FAILURE-DETAIL"),
            "the cap must keep the tail (recent steps), not the head"
        );
        assert!(out.starts_with('…'), "truncation marker leads the kept tail");
        assert!(out.chars().count() <= RETURN_TEXT_CAP + 1);
        // Within budget: unchanged, no marker.
        assert_eq!(capped("short".to_string(), RETURN_TEXT_CAP), "short");
    }
}
