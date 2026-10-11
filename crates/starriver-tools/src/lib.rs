//! Default in-memory tool registry and guarded execution pipeline.

pub mod ptc;
pub mod worktree;

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll};

use cordis_rslite::{
    Context, EffectHandle, EventBus, EventError, Inject, Plugin, TypedService, Value,
};
use starriver_kernel::approval::{ApprovalHandle, ApprovalOutcome, ApprovalRequest};
use starriver_kernel::json::JsonValue;
use starriver_kernel::llm::{ContentBlock, ToolSchema};
use starriver_kernel::tools::{
    mint_tool_execution_token, CodeDispatchLog, PostToolDecision, PreToolDecision,
    ScheduledToolDispatch, ScheduledToolPreparation, ToolDefinition, ToolError, ToolExecution,
    ToolExecutionInput, ToolExecutionMode, ToolExecutionResult, ToolExecutionToken, ToolFailure,
    ToolGuard, ToolPresentationMeta, ToolRenderer, ToolRunContext, ToolRuntime, ToolsAvailable,
    ToolsCodeDispatchLog, ToolsExecute, ToolsPostExecute, ToolsPreExecute, ToolsResult,
};

/// In-memory tool registry running the full execution pipeline:
/// pre-execute policy -> monotonic guards -> around-dispatch waterfall ->
/// post-execute policy -> durable result notification.
pub struct InMemoryToolRuntime {
    registry: Arc<Mutex<HashMap<String, Arc<ToolDefinition>>>>,
    guards: Arc<Mutex<Vec<ToolGuard>>>,
 /// Pure presentation-metadata hooks keyed by tool name (
    /// `output.presentationMeta` parity). A side table instead of a
    /// `ToolDefinition` field: definition literals number ~100 across
    /// product crates, so a name-keyed map is the zero-churn equivalent —
    /// declarations still live at the tool's registration site.
    presentation_meta: Arc<Mutex<HashMap<String, ToolPresentationMeta>>>,
    /// Scheduler-owned executions awaiting finalize/finish, keyed by the
    /// opaque execution token minted at prepare.
    prepared: Arc<Mutex<HashMap<ToolExecutionToken, Arc<ToolDefinition>>>>,
    /// Per-agent allow restrictions; intersect across entries per agent.
    /// Each entry carries a unique id so its disposer removes exactly it
    /// (LIFO pop would misroute when restrictions are dropped out of order).
    #[allow(clippy::type_complexity)]
    restrictions: Arc<Mutex<HashMap<String, Vec<(u64, Vec<String>)>>>>,
    next_restriction_id: AtomicU64,
    /// agent_id → agent scope Context routing for tools/* waterfall events.
    scopes: Arc<starriver_kernel::agent::AgentScopeRegistry>,
    /// PTC mode presentation/enforcement switch. Presentation (schema
    /// projection) and enforcement (the ptc-only guard) read the same handle
    /// so they can never disagree; the host flips it on execution-mode changes.
    ptc_active: ptc::PtcActive,
    bus: EventBus,
    ctx: Context,
}

impl InMemoryToolRuntime {
    pub fn new(bus: EventBus, ctx: Context) -> Self {
        Self::with_scopes(
            bus,
            ctx,
            Arc::new(starriver_kernel::agent::AgentScopeRegistry::new()),
        )
    }

    pub fn with_scopes(
        bus: EventBus,
        ctx: Context,
        scopes: Arc<starriver_kernel::agent::AgentScopeRegistry>,
    ) -> Self {
        Self {
            registry: Arc::new(Mutex::new(HashMap::new())),
            guards: Arc::new(Mutex::new(Vec::new())),
            presentation_meta: Arc::new(Mutex::new(HashMap::new())),
            prepared: Arc::new(Mutex::new(HashMap::new())),
            restrictions: Arc::new(Mutex::new(HashMap::new())),
            next_restriction_id: AtomicU64::new(0),
            scopes,
            ptc_active: Arc::new(AtomicBool::new(false)),
            bus,
            ctx,
        }
    }

 /// Register a pure `(args, value) → meta` presenter for one tool (
    /// `output.presentationMeta` parity). Re-registering replaces the hook.
    pub fn register_presentation_meta(&self, name: &str, hook: ToolPresentationMeta) {
        self.presentation_meta
            .lock()
            .expect("presentation meta poisoned")
            .insert(name.to_string(), hook);
    }

    /// Flip PTC mode presentation and enforcement. `set_execution_mode`-style
    /// host paths call this; `run_code`'s schema projection, the ptc-only
    /// guard, and the prompt sections all key off this one flag.
    pub fn set_ptc_active(&self, active: bool) {
        self.ptc_active.store(active, Ordering::SeqCst);
    }

    pub fn is_ptc_active(&self) -> bool {
        self.ptc_active.load(Ordering::SeqCst)
    }

    /// The shared flag handle, for guard closures registered alongside.
    pub fn ptc_handle(&self) -> ptc::PtcActive {
        Arc::clone(&self.ptc_active)
    }

    /// Model-facing schema projection under PTC mode: the reserved `run_code`
    /// transport alone, immune to visible-tools whitelists and restrictions.
    fn ptc_transport_schema(&self) -> Vec<ToolSchema> {
        self.all_schemas()
            .into_iter()
            .filter(|schema| schema.name == ptc::RUN_CODE_NAME)
            .collect()
    }

    /// Full schema snapshot including `run_code` — the base for agent-scoped
    /// restriction filtering, which must see the reserved tool even though
    /// restrictions do not apply to it.
    fn all_schemas(&self) -> Vec<ToolSchema> {
        self.registry
            .lock()
            .expect("tool registry poisoned")
            .values()
            .map(|definition| ToolSchema {
                name: definition.name.clone(),
                description: definition.description.clone(),
                parameters: tool_parameters(&definition.parameters),
            })
            .collect()
    }

    fn available_names(&self) -> Vec<String> {
        self.registry
            .lock()
            .expect("tool registry poisoned")
            .keys()
            .cloned()
            .collect()
    }

    /// True when the registered definition for `name` carries
    /// `requires_approval`. Unknown tools are fail-closed `false` (a missing
    /// tool already fails before dispatch).
    pub fn requires_approval(&self, name: &str) -> bool {
        self.registry
            .lock()
            .expect("tool registry poisoned")
            .get(name)
            .map(|d| d.requires_approval)
            .unwrap_or(false)
    }

    /// True when the registered definition for `name` carries
    /// `deny_in_readonly`. Unknown tools are fail-closed `false` (a missing
    /// tool already fails before dispatch).
    pub fn deny_in_readonly(&self, name: &str) -> bool {
        self.registry
            .lock()
            .expect("tool registry poisoned")
            .get(name)
            .map(|d| d.deny_in_readonly)
            .unwrap_or(false)
    }

    fn emit_available(&self) {
        self.bus.emit(Arc::new(ToolsAvailable {
            names: self.available_names(),
        }));
    }
}

impl TypedService for InMemoryToolRuntime {
    const NAME: &'static str = <InMemoryToolRuntime as ToolRuntime>::NAME;
}

impl ToolRuntime for InMemoryToolRuntime {
    fn register(&self, definition: ToolDefinition) -> Result<EffectHandle, ToolError> {
        let name = definition.name.clone();
        let definition = Arc::new(definition);
        {
            let mut registry = self.registry.lock().expect("tool registry poisoned");
            if registry.contains_key(&name) {
                return Err(ToolError::AlreadyRegistered(name));
            }
            registry.insert(name.clone(), Arc::clone(&definition));
        }
        self.emit_available();

        let registry = Arc::clone(&self.registry);
        let bus = self.bus.clone();
        Ok(EffectHandle::new(move || {
            let mut registry = registry.lock().expect("tool registry poisoned");
            registry.remove(&name);
            let names = registry.keys().cloned().collect::<Vec<_>>();
            drop(registry);
            bus.emit(Arc::new(ToolsAvailable { names }));
        }))
    }

    fn guard(&self, guard: ToolGuard) -> Result<EffectHandle, ToolError> {
        {
            let mut guards = self.guards.lock().expect("tool guards poisoned");
            guards.push(Arc::clone(&guard));
        }
        let guards = Arc::clone(&self.guards);
        Ok(EffectHandle::new(move || {
            guards
                .lock()
                .expect("tool guards poisoned")
                .retain(|existing| !Arc::ptr_eq(existing, &guard));
        }))
    }

    fn execution_mode(&self, input: &ToolExecutionInput) -> ToolExecutionMode {
        let definition = self
            .registry
            .lock()
            .expect("tool registry poisoned")
            .get(&input.name)
            .cloned();
        match definition {
            Some(definition) => match &definition.is_concurrency_safe {
                Some(classifier) => match catch_panic(|| classifier(&input.arguments)) {
                    Ok(true) => ToolExecutionMode::Parallel,
                    // Fail-closed: false, exceptions, and panics are exclusive.
                    _ => ToolExecutionMode::Exclusive,
                },
                None => ToolExecutionMode::Exclusive,
            },
            None => ToolExecutionMode::Exclusive,
        }
    }

    fn presentation_meta(
        &self,
        input: &ToolExecutionInput,
        value: &Value,
    ) -> Option<serde_json::Value> {
 // computes presentation metadata for top-level calls only.
        if input.root_call_id.is_some() {
            return None;
        }
        let hook = self
            .presentation_meta
            .lock()
            .expect("presentation meta poisoned")
            .get(&input.name)
            .cloned()?;
        (hook)(&input.arguments, value)
    }

    fn shape_dispatch_log(
        &self,
        dispatch: CodeDispatchLog,
    ) -> cordis_rslite::BoxFuture<Vec<ContentBlock>> {
        let bus = self.bus.clone();
        Box::pin(async move {
            let original = dispatch.content.clone();
            let result = bus
                .waterfall(
                    Arc::new(ToolsCodeDispatchLog {
                        dispatch: dispatch.clone(),
                    }),
                    {
                        let original = original.clone();
                        move || {
                            let original = original.clone();
                            Box::pin(async move { Ok(original) })
                        }
                    },
                )
                .await;
            match result {
                Ok(content) => content,
                Err(error) => {
                    tracing::warn!(
                        tool = %dispatch.name,
                        error = %error,
                        "tools/code-dispatch-log listener failed; logging the original settled content"
                    );
                    original
                }
            }
        })
    }

    fn schemas(&self) -> Vec<ToolSchema> {
        if self.is_ptc_active() {
            return self.ptc_transport_schema();
        }
        self.all_schemas()
            .into_iter()
            .filter(|schema| schema.name != ptc::RUN_CODE_NAME)
            .collect()
    }

    fn schemas_for(&self, agent_id: Option<&str>) -> Vec<ToolSchema> {
        // PTC presentation outranks the capability layers: the reserved
        // `run_code` transport sits OUTSIDE visible-tools whitelists and
 // restrictions (contract), and under PTC it is the ONLY tool
        // presented. Placing this branch before the restriction intersection
        // is load-bearing — the whitelist does not name run_code, so letting
        // the intersection run first would present ZERO tools and strand the
        // model (it can only improvise tool syntax as text).
        if self.is_ptc_active() {
            return self.ptc_transport_schema();
        }
        let Some(agent_id) = agent_id else {
            return self.schemas();
        };
        let all = self.all_schemas();
        let allowed: Option<Vec<String>> =
            self.restrictions
                .lock()
                .unwrap()
                .get(agent_id)
                .map(|entries| {
                    let lists = entries
                        .iter()
                        .map(|(_, allow)| allow.clone())
                        .collect::<Vec<_>>();
                    // Intersection: names present in every allow entry.
                    lists
                        .first()
                        .cloned()
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|name| lists.iter().all(|entry| entry.contains(name)))
                        .collect()
                });
        let filtered = match allowed {
            Some(allowed) => all
                .into_iter()
                .filter(|schema| allowed.contains(&schema.name))
                .collect(),
            None => all,
        };
        // Native presentation hides the reserved transport entirely.
        filtered
            .into_iter()
            .filter(|schema| schema.name != ptc::RUN_CODE_NAME)
            .collect()
    }

    /// Catalog for the PTC program-toolchain prompt section: every callable
    /// tool EXCEPT `run_code` itself (a program cannot nest run_code).
    fn sdk_schemas(&self) -> Vec<ToolSchema> {
        self.registry
            .lock()
            .expect("tool registry poisoned")
            .values()
            .filter(|definition| definition.name != ptc::RUN_CODE_NAME)
            .map(|definition| ToolSchema {
                name: definition.name.clone(),
                description: definition.description.clone(),
                parameters: tool_parameters(&definition.parameters),
            })
            .collect()
    }

    /// PTC presentation/enforcement is live for this runtime.
    fn is_ptc_active(&self) -> bool {
        InMemoryToolRuntime::is_ptc_active(self)
    }

    fn restrict_for(&self, agent_id: &str, allow: Vec<String>) -> Result<EffectHandle, ToolError> {
        if allow.is_empty() {
 // restrict({}) is a no-op and rejects; an empty whitelist
            // would hide every tool, which is almost always a config bug.
            return Err(ToolError::NotFound("<empty allow>".into()));
        }
 // Unknown tool names reject, matching restrict's validation.
        let known: std::collections::HashSet<String> =
            self.registry.lock().unwrap().keys().cloned().collect();
        let unknown = allow
            .iter()
            .filter(|name| !known.contains(*name))
            .cloned()
            .collect::<Vec<_>>();
        if !unknown.is_empty() {
            return Err(ToolError::NotFound(unknown[0].clone()));
        }
        let restrictions = Arc::clone(&self.restrictions);
        let agent_id_owned = agent_id.to_string();
        let restriction_id = self.next_restriction_id.fetch_add(1, Ordering::SeqCst);
        {
            let mut map = restrictions.lock().unwrap();
            map.entry(agent_id_owned.clone())
                .or_default()
                .push((restriction_id, allow));
        }
        Ok(EffectHandle::new(move || {
            let mut map = restrictions.lock().unwrap();
            if let Some(entries) = map.get_mut(&agent_id_owned) {
                entries.retain(|(id, _)| *id != restriction_id);
                if entries.is_empty() {
                    map.remove(&agent_id_owned);
                }
            }
        }))
    }

    fn prepare(
        &self,
        input: ToolExecutionInput,
    ) -> cordis_rslite::BoxFuture<Result<ScheduledToolPreparation, ToolError>> {
        let bus = self.bus.clone();
        let ctx = self.ctx.clone();
        let registry = Arc::clone(&self.registry);
        let guards = Arc::clone(&self.guards);
        let prepared = Arc::clone(&self.prepared);
        let scopes = Arc::clone(&self.scopes);
        Box::pin(async move {
            prepare_execution(&bus, &ctx, &scopes, &registry, &guards, &prepared, input).await
        })
    }

    fn dispatch(
        &self,
        exec: ToolRunContext,
    ) -> cordis_rslite::BoxFuture<Result<ScheduledToolDispatch, ToolError>> {
        let bus = self.bus.clone();
        let ctx = self.ctx.clone();
        let prepared = Arc::clone(&self.prepared);
        let scopes = Arc::clone(&self.scopes);
        Box::pin(async move { dispatch_execution(&bus, &ctx, &scopes, &prepared, exec).await })
    }

    fn finalize(
        &self,
        exec: ToolRunContext,
        result: ToolExecutionResult,
    ) -> cordis_rslite::BoxFuture<Result<ToolExecutionResult, ToolError>> {
        let bus = self.bus.clone();
        let ctx = self.ctx.clone();
        let prepared = Arc::clone(&self.prepared);
        let scopes = Arc::clone(&self.scopes);
        Box::pin(
            async move { finalize_execution(&bus, &ctx, &scopes, &prepared, exec, result).await },
        )
    }

    fn finish(
        &self,
        exec: ToolRunContext,
        result: ToolExecutionResult,
    ) -> Result<ToolExecutionResult, ToolError> {
        finish_execution(&self.bus, &self.prepared, exec, result)
    }

    fn execute(
        &self,
        input: ToolExecutionInput,
    ) -> cordis_rslite::BoxFuture<Result<ToolExecutionResult, ToolError>> {
        let bus = self.bus.clone();
        let ctx = self.ctx.clone();
        let registry = Arc::clone(&self.registry);
        let guards = Arc::clone(&self.guards);
        let prepared = Arc::clone(&self.prepared);
        let scopes = Arc::clone(&self.scopes);
        Box::pin(async move {
            match prepare_execution(&bus, &ctx, &scopes, &registry, &guards, &prepared, input)
                .await?
            {
                ScheduledToolPreparation::Dispatch { exec } => {
                    let finalize_exec = exec.clone();
                    match dispatch_execution(&bus, &ctx, &scopes, &prepared, exec).await? {
                        ScheduledToolDispatch::PostResult { result } => {
                            finalize_execution(
                                &bus,
                                &ctx,
                                &scopes,
                                &prepared,
                                finalize_exec,
                                result,
                            )
                            .await
                        }
                        ScheduledToolDispatch::FinalResult { result } => {
                            finish_execution(&bus, &prepared, finalize_exec, result)
                        }
                    }
                }
                ScheduledToolPreparation::PostResult { exec, result } => {
                    finalize_execution(&bus, &ctx, &scopes, &prepared, exec, result).await
                }
                ScheduledToolPreparation::FinalResult { exec, result } => {
                    finish_execution(&bus, &prepared, exec, result)
                }
            }
        })
    }
}

/// Convert a definition's model-facing schema into the lossless JSON tree.
/// Definitions may store kernel `JsonValue` spellings or raw `serde_json`
/// values; both must reach the model, so unknown spellings fall back to the
/// serde_json downcast before giving up.
fn tool_parameters(parameters: &Value) -> JsonValue {
    if let Ok(json) = JsonValue::from_value(parameters) {
        return json;
    }
    parameters
        .downcast::<serde_json::Value>()
        .ok()
        .map(|value| JsonValue::from_serde_json((*value).clone()))
        .unwrap_or(JsonValue::Null)
}

fn mint_execution(input: ToolExecutionInput) -> ToolExecution {
    ToolExecution {
        cwd: input.cwd.clone(),
        sandbox_mode: input.sandbox_mode,
        call_id: input.call_id.clone(),
        root_call_id: input
            .root_call_id
            .clone()
            .unwrap_or_else(|| input.call_id.clone()),
        name: input.name.clone(),
        arguments: input.arguments.clone(),
        agent_id: input.agent_id.clone(),
        parent: input.parent,
        token: mint_tool_execution_token(),
        cancellation: input.cancellation.clone(),
    }
}

/// Resolve the agent scope Context for one execution, falling back to `None`
/// when the agent is unknown or absent (global event routing).
fn route_scope(
    scopes: &starriver_kernel::agent::AgentScopeRegistry,
    agent_id: Option<&str>,
) -> Option<cordis_rslite::Context> {
    agent_id.and_then(|id| scopes.lookup(id))
}

async fn prepare_execution(
    bus: &EventBus,
    ctx: &Context,
    scopes: &starriver_kernel::agent::AgentScopeRegistry,
    registry: &Mutex<HashMap<String, Arc<ToolDefinition>>>,
    guards: &Mutex<Vec<ToolGuard>>,
    prepared: &Mutex<HashMap<ToolExecutionToken, Arc<ToolDefinition>>>,
    input: ToolExecutionInput,
) -> Result<ScheduledToolPreparation, ToolError> {
    let execution = mint_execution(input);
    let exec = ToolRunContext::new(execution.clone());
    let route_ctx =
        route_scope(scopes, execution.agent_id.as_deref()).unwrap_or_else(|| ctx.clone());
    let Some(definition) = registry
        .lock()
        .expect("tool registry poisoned")
        .get(&execution.name)
        .cloned()
    else {
        return Ok(ScheduledToolPreparation::FinalResult {
            exec,
            result: unknown_tool_result(&execution),
        });
    };
    if is_cancelled(&execution) {
        return Ok(ScheduledToolPreparation::FinalResult {
            exec,
            result: aborted_before_dispatch_result(&execution),
        });
    }
    // Known tools are tracked for the whole staged lifetime so finalize/finish
    // can re-render and apply definition-owned content finalization, matching
 // `contentFinalizers` map populated at execution creation.
    prepared
        .lock()
        .expect("tool prepared map poisoned")
        .insert(execution.token, definition);
    let decision = match bus
        .waterfall_scoped(
            &route_ctx,
            Arc::new(ToolsPreExecute {
                execution: execution.clone(),
            }),
            || Box::pin(async { Ok(PreToolDecision::Allow) }),
        )
        .await
    {
        Ok(decision) => decision,
        Err(error) => {
            return Ok(ScheduledToolPreparation::FinalResult {
                exec,
                result: event_failure_result(&execution, error),
            });
        }
    };
    if let PreToolDecision::Ask { reason } = &decision {
        let Some(approval_value) = ctx.require_any("approval") else {
            return Ok(ScheduledToolPreparation::PostResult {
                exec,
                result: denied_result(
                    &execution,
                    "tool requires approval, but no approval channel is available",
                ),
            });
        };
        let Ok(approval) = approval_value.downcast::<ApprovalHandle>() else {
            return Ok(ScheduledToolPreparation::PostResult {
                exec,
                result: denied_result(
                    &execution,
                    "tool requires approval, but the approval channel is misconfigured",
                ),
            });
        };
        let Some(agent_id) = execution.agent_id.clone() else {
            return Ok(ScheduledToolPreparation::PostResult {
                exec,
                result: denied_result(
                    &execution,
                    "tool requires approval, but the call has no agent",
                ),
            });
        };
        let outcome = approval
            .request(ApprovalRequest {
                agent_id,
                tool_name: execution.name.clone(),
                call_id: Some(execution.call_id.0.clone()),
                reason: reason.clone(),
                signal: execution.cancellation.clone(),
                arguments: Some(execution.arguments.clone()),
            })
            .await
            .unwrap_or(ApprovalOutcome::Unavailable);
        match outcome {
            ApprovalOutcome::AllowedOnce | ApprovalOutcome::AllowedSimilar => {}
            ApprovalOutcome::Rejected => {
                return Ok(ScheduledToolPreparation::PostResult {
                    exec,
                    result: denied_result(&execution, "the user rejected the tool"),
                });
            }
            ApprovalOutcome::Cancelled => {
                return Ok(ScheduledToolPreparation::PostResult {
                    exec,
                    result: denied_result(&execution, "approval was cancelled"),
                });
            }
            ApprovalOutcome::Unavailable => {
                return Ok(ScheduledToolPreparation::PostResult {
                    exec,
                    result: denied_result(
                        &execution,
                        "tool requires approval, but no approval channel is available",
                    ),
                });
            }
        }
    }
    if let PreToolDecision::Deny { reason } = decision {
        return Ok(ScheduledToolPreparation::PostResult {
            exec,
            result: denied_result(&execution, &reason),
        });
    }
    if let Some(reason) = evaluate_guards(guards, &execution) {
        return Ok(ScheduledToolPreparation::PostResult {
            exec,
            result: denied_result(&execution, &reason),
        });
    }
    if is_cancelled(&execution) {
        return Ok(ScheduledToolPreparation::PostResult {
            exec,
            result: aborted_before_dispatch_result(&execution),
        });
    }
    Ok(ScheduledToolPreparation::Dispatch { exec })
}

async fn dispatch_execution(
    bus: &EventBus,
    ctx: &Context,
    scopes: &starriver_kernel::agent::AgentScopeRegistry,
    prepared: &Mutex<HashMap<ToolExecutionToken, Arc<ToolDefinition>>>,
    exec: ToolRunContext,
) -> Result<ScheduledToolDispatch, ToolError> {
    let execution = exec.execution.clone();
    let route_ctx =
        route_scope(scopes, execution.agent_id.as_deref()).unwrap_or_else(|| ctx.clone());
    let Some(definition) = prepared
        .lock()
        .expect("tool prepared map poisoned")
        .get(&execution.token)
        .cloned()
    else {
        return Ok(ScheduledToolDispatch::FinalResult {
            result: event_failure_result(
                &execution,
                EventError::Listener("unprepared execution".into()),
            ),
        });
    };
    let run_ctx = exec.clone();
    let result = match bus
        .waterfall_scoped(
            &route_ctx,
            Arc::new(ToolsExecute {
                execution: execution.clone(),
            }),
            {
                let definition = Arc::clone(&definition);
                let run_ctx = run_ctx.clone();
                move || {
                    let definition = Arc::clone(&definition);
                    let run_ctx = run_ctx.clone();
                    Box::pin(async move {
                        if is_cancelled(&run_ctx.execution) {
                            return Ok(aborted_before_dispatch_result(&run_ctx.execution));
                        }
                        let outcome = match catch_panic_future(
                            definition
                                .executor
                                .execute(run_ctx.execution.arguments.clone(), run_ctx.clone()),
                        )
                        .await
                        {
                            Ok(outcome) => outcome,
                            Err(message) => Err(ToolError::Execution {
                                tool: run_ctx.execution.name.clone(),
                                code: "TOOL_PANIC".into(),
                                message,
                            }),
                        };
                        let result = normalize_body_result(
                            &run_ctx.execution,
                            &definition.render,
                            &run_ctx.execution.arguments,
                            outcome,
                        );
                        Ok(if is_cancelled(&run_ctx.execution) {
                            aborted_result(&run_ctx.execution)
                        } else {
                            result
                        })
                    })
                }
            },
        )
        .await
    {
        Ok(result) => result,
        Err(error) => {
            return Ok(ScheduledToolDispatch::FinalResult {
                result: event_failure_result(&execution, error),
            });
        }
    };
    let mut result = result;
    if let ToolExecutionResult::Success {
        additional_contexts,
        concludes_turn,
        ..
    } = &mut result
    {
        let deferred = run_ctx.takes_deferred();
        additional_contexts.splice(0..0, deferred);
        *concludes_turn |= run_ctx.concludes_turn();
    }
    Ok(ScheduledToolDispatch::PostResult { result })
}

async fn finalize_execution(
    bus: &EventBus,
    ctx: &Context,
    scopes: &starriver_kernel::agent::AgentScopeRegistry,
    prepared: &Mutex<HashMap<ToolExecutionToken, Arc<ToolDefinition>>>,
    exec: ToolRunContext,
    result: ToolExecutionResult,
) -> Result<ToolExecutionResult, ToolError> {
    let execution = exec.execution.clone();
    let route_ctx =
        route_scope(scopes, execution.agent_id.as_deref()).unwrap_or_else(|| ctx.clone());
    let Some(definition) = prepared
        .lock()
        .expect("tool prepared map poisoned")
        .get(&execution.token)
        .cloned()
    else {
        return Ok(event_failure_result(
            &execution,
            EventError::Listener("unprepared execution".into()),
        ));
    };
    let post = match bus
        .waterfall_scoped(
            &route_ctx,
            Arc::new(ToolsPostExecute {
                execution: execution.clone(),
                result: result.clone(),
            }),
            || {
                Box::pin(async {
                    Ok(PostToolDecision::Accept {
                        content: None,
                        value: None,
                        additional_contexts: Vec::new(),
                    })
                })
            },
        )
        .await
    {
        Ok(decision) => decision,
        Err(error) => {
            // Route through finish_execution: returning directly skipped the
            // ToolsResult emission and leaked the prepared-token entry.
            return finish_execution(
                bus,
                prepared,
                exec,
                event_failure_result(&execution, error),
            );
        }
    };
    let mut result = result;
    let adjusted = match post {
        PostToolDecision::Accept {
            content,
            value,
            additional_contexts,
        } => {
            if let ToolExecutionResult::Success {
                content: target_content,
                value: target_value,
                additional_contexts: target_contexts,
                ..
            } = &mut result
            {
                if let Some(content) = content {
                    *target_content = content;
                }
                if let Some(value) = value {
                    *target_value = value;
                    match catch_panic(|| (definition.render)(&execution.arguments, target_value)) {
                        Ok(content) => *target_content = content,
                        Err(message) => {
                            return finish_execution(
                                bus,
                                prepared,
                                exec,
                                invalid_output_result(
                                    &execution,
                                    &format!("render failed: {message}"),
                                ),
                            );
                        }
                    }
                }
                target_contexts.extend(additional_contexts);
            }
            result
        }
        PostToolDecision::Block {
            feedback,
            additional_contexts,
        } => ToolExecutionResult::Failure {
            error: ToolFailure {
                message: failure_message_from_content(&feedback),
                code: "BLOCKED".into(),
            },
            content: feedback,
            additional_contexts,
        },
    };
    finish_execution(bus, prepared, exec, adjusted)
}

fn finish_execution(
    bus: &EventBus,
    prepared: &Mutex<HashMap<ToolExecutionToken, Arc<ToolDefinition>>>,
    exec: ToolRunContext,
    result: ToolExecutionResult,
) -> Result<ToolExecutionResult, ToolError> {
    let execution = exec.execution.clone();
    let definition = prepared
        .lock()
        .expect("tool prepared map poisoned")
        .get(&execution.token)
        .cloned();
    let final_result = match &definition {
        Some(definition) => match &definition.finalize_content {
            Some(finalizer) => {
                let mut final_result = result;
                match catch_panic(|| finalizer(&execution, &final_result)) {
                    Ok(Some(content)) => {
                        if let ToolExecutionResult::Success {
                            content: target, ..
                        } = &mut final_result
                        {
                            *target = content;
                        }
                        final_result
                    }
                    Ok(None) => final_result,
                    Err(message) => return Ok(finalize_failed_result(&execution, &message)),
                }
            }
            None => result,
        },
        // Unknown-tool terminal results never registered a definition; they
        // bypass content finalization and materialize as-is.
        None => result,
    };
    let final_result = if is_cancelled(&execution) && !final_result.is_error() {
        aborted_result(&execution)
    } else {
        final_result
    };
    prepared
        .lock()
        .expect("tool prepared map poisoned")
        .remove(&execution.token);
    bus.emit(Arc::new(ToolsResult {
        execution,
        result: final_result.clone(),
    }));
    Ok(final_result)
}

/// Polls `F` with per-poll panic containment so a panicking tool body is
/// normalized into a tool error result instead of escaping to the driver,
/// matching `dispatchToolBody` exception handling.
struct CatchPanicFuture<F> {
    inner: F,
}

impl<F: Future> Future for CatchPanicFuture<F> {
    type Output = Result<F::Output, String>;
    fn poll(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        let inner = unsafe { self.map_unchecked_mut(|this| &mut this.inner) };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || inner.poll(cx)));
        match result {
            Ok(Poll::Ready(value)) => Poll::Ready(Ok(value)),
            Ok(Poll::Pending) => Poll::Pending,
            Err(payload) => Poll::Ready(Err(panic_message(payload))),
        }
    }
}

async fn catch_panic_future<F: Future>(future: F) -> Result<F::Output, String> {
    CatchPanicFuture { inner: future }.await
}

fn is_cancelled(execution: &ToolExecution) -> bool {
    execution
        .cancellation
        .as_ref()
        .is_some_and(|token| token.is_cancelled())
}

/// Evaluate monotonic guards in registration order, containing panics as
/// denials so one hostile guard cannot crash the pipeline.
fn evaluate_guards(guards: &Mutex<Vec<ToolGuard>>, execution: &ToolExecution) -> Option<String> {
    let guards = guards.lock().expect("tool guards poisoned");
    for guard in guards.iter() {
        match catch_panic(|| guard(execution)) {
            Ok(Some(reason)) => return Some(reason),
            Ok(None) => continue,
            Err(message) => return Some(format!("guard panicked: {message}")),
        }
    }
    None
}

/// Contain a plugin-owned callback panic and surface its message.
fn catch_panic<F, R>(callback: F) -> Result<R, String>
where
    F: FnOnce() -> R,
{
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(callback)) {
        Ok(value) => Ok(value),
        Err(payload) => Err(panic_message(payload)),
    }
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "plugin callback panicked".into()
    }
}

fn unknown_tool_result(execution: &ToolExecution) -> ToolExecutionResult {
    let message = format!("unknown tool \"{}\"", execution.name);
    ToolExecutionResult::Failure {
        error: ToolFailure {
            message: message.clone(),
            code: "UNKNOWN_TOOL".into(),
        },
        content: vec![ContentBlock::Text(message)],
        additional_contexts: Vec::new(),
    }
}

fn denied_result(_execution: &ToolExecution, reason: &str) -> ToolExecutionResult {
    let message = format!("Error: {reason}");
    ToolExecutionResult::Failure {
        error: ToolFailure {
            message: reason.into(),
            code: "DENIED".into(),
        },
        content: vec![ContentBlock::Text(message)],
        additional_contexts: Vec::new(),
    }
}

fn invalid_output_result(execution: &ToolExecution, violation: &str) -> ToolExecutionResult {
    let message = format!(
        "tool \"{}\" returned invalid output: {violation}",
        execution.name
    );
    ToolExecutionResult::Failure {
        error: ToolFailure {
            message: message.clone(),
            code: "INVALID_TOOL_OUTPUT".into(),
        },
        content: vec![ContentBlock::Text(message)],
        additional_contexts: Vec::new(),
    }
}

fn finalize_failed_result(execution: &ToolExecution, message: &str) -> ToolExecutionResult {
    let message = format!(
        "tool \"{}\" finalize_content failed: {message}",
        execution.name
    );
    ToolExecutionResult::Failure {
        error: ToolFailure {
            message: message.clone(),
            code: "FINALIZE_FAILED".into(),
        },
        content: vec![ContentBlock::Text(message)],
        additional_contexts: Vec::new(),
    }
}

fn event_failure_result(execution: &ToolExecution, error: EventError) -> ToolExecutionResult {
    let message = format!("tool \"{}\" pipeline failed: {error}", execution.name);
    ToolExecutionResult::Failure {
        error: ToolFailure {
            message: message.clone(),
            code: "EVENT".into(),
        },
        content: vec![ContentBlock::Text(message)],
        additional_contexts: Vec::new(),
    }
}

fn aborted_before_dispatch_result(execution: &ToolExecution) -> ToolExecutionResult {
    let message = format!("tool \"{}\" aborted before dispatch", execution.name);
    ToolExecutionResult::Failure {
        error: ToolFailure {
            message: message.clone(),
            code: "ABORTED_BEFORE_DISPATCH".into(),
        },
        content: vec![ContentBlock::Text(message)],
        additional_contexts: Vec::new(),
    }
}

fn aborted_result(execution: &ToolExecution) -> ToolExecutionResult {
    let message = format!("tool \"{}\" aborted", execution.name);
    ToolExecutionResult::Failure {
        error: ToolFailure {
            message: message.clone(),
            code: "ABORTED".into(),
        },
        content: vec![ContentBlock::Text(message)],
        additional_contexts: Vec::new(),
    }
}

fn failure_message_from_content(content: &[ContentBlock]) -> String {
    let text = content
        .iter()
        .map(|block| match block {
            ContentBlock::Text(text) => text.clone(),
            other => format!("[{other:?}]"),
        })
        .collect::<Vec<_>>()
        .join("\n");
    if text.is_empty() {
        "tool result blocked by post-execute policy".into()
    } else {
        text
    }
}

fn normalize_body_result(
    execution: &ToolExecution,
    render: &ToolRenderer,
    arguments: &Value,
    outcome: Result<Value, ToolError>,
) -> ToolExecutionResult {
    match outcome {
        Ok(value) => {
            let content = match catch_panic(|| render(arguments, &value)) {
                Ok(content) => content,
                Err(message) => {
                    return invalid_output_result(execution, &format!("render failed: {message}"));
                }
            };
            ToolExecutionResult::Success {
                value: value.clone(),
                content,
                additional_contexts: Vec::new(),
                concludes_turn: false,
            }
        }
        Err(error) => ToolExecutionResult::Failure {
            error: ToolFailure {
                message: error.to_string(),
                code: error.code().to_string(),
            },
            content: vec![ContentBlock::Text(error.to_string())],
            additional_contexts: Vec::new(),
        },
    }
}

pub fn in_memory_tools_plugin() -> impl Plugin {
    struct InMemoryToolsPlugin;

    impl Plugin for InMemoryToolsPlugin {
        fn name(&self) -> &'static str {
            "in-memory-tools"
        }

        fn inject(&self) -> Inject {
            Inject::none()
        }

        fn apply(
            &self,
            ctx: &Context,
            bus: &EventBus,
        ) -> cordis_rslite::BoxFuture<anyhow::Result<Vec<EffectHandle>>> {
            let ctx = ctx.clone();
            let bus = bus.clone();
            Box::pin(async move {
                let effect = ctx.provide::<InMemoryToolRuntime>(Arc::new(
                    InMemoryToolRuntime::new(bus, ctx.clone()),
                ))?;
                Ok(vec![effect])
            })
        }
    }

    InMemoryToolsPlugin
}

#[cfg(test)]
mod tests {
    use super::*;
    use cordis_rslite::{Context, EventBus, PluginRegistry};
    use starriver_kernel::json::JsonValue;
    use starriver_kernel::llm::{
        CallId, ContentBlock, Message, MessageId, MessageRole, MessageSource,
    };
    use starriver_kernel::tools::{ToolExecution, ToolExecutionInput, ToolExecutor};

    struct Echo;

    impl ToolExecutor for Echo {
        fn execute(
            &self,
            arguments: Value,
            _ctx: ToolRunContext,
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

    #[test]
    fn requires_approval_reflects_definition_flag() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = InMemoryToolRuntime::new(bus, ctx.clone());
        let mut def = echo_definition("needs-ok");
        def.requires_approval = true;
        runtime.register(def).unwrap();
        assert!(runtime.requires_approval("needs-ok"));
        assert!(!runtime.requires_approval("missing"));
    }

    #[test]
    fn deny_in_readonly_reflects_definition_flag() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = InMemoryToolRuntime::new(bus, ctx);
        let mut def = echo_definition("write-like");
        def.deny_in_readonly = true;
        runtime.register(def).unwrap();
        assert!(runtime.deny_in_readonly("write-like"));
        assert!(!runtime.deny_in_readonly("missing"));
    }

    fn execution(name: &str) -> ToolExecutionInput {
        ToolExecutionInput {
            cwd: None,
            sandbox_mode: None,
            call_id: CallId("c1".into()),
            root_call_id: None,
            name: name.into(),
            arguments: Value::new(1u64),
            agent_id: None,
            parent: None,
            cancellation: None,
        }
    }

    fn assembled_execution(name: &str) -> ToolExecution {
        ToolExecution {
            cwd: None,
            sandbox_mode: None,
            call_id: CallId("c1".into()),
            root_call_id: CallId("c1".into()),
            name: name.into(),
            arguments: Value::new(1u64),
            agent_id: None,
            parent: None,
            token: mint_tool_execution_token(),
            cancellation: None,
        }
    }

    #[tokio::test]
    async fn execute_pipeline_returns_success() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = InMemoryToolRuntime::new(bus.clone(), ctx.clone());
        let _handle = runtime.register(echo_definition("echo")).unwrap();
        let result = runtime.execute(execution("echo")).await.unwrap();
        assert!(matches!(result, ToolExecutionResult::Success { .. }));
    }

    #[tokio::test]
    async fn unknown_tool_materializes_failure() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = InMemoryToolRuntime::new(bus.clone(), ctx.clone());
        let result = runtime.execute(execution("missing")).await.unwrap();
        let ToolExecutionResult::Failure { error, .. } = result else {
            panic!("expected failure");
        };
        assert_eq!(error.code, "UNKNOWN_TOOL");
    }

    #[tokio::test]
    async fn pre_execute_deny_materializes_failure() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = InMemoryToolRuntime::new(bus.clone(), ctx.clone());
        let _handle = runtime.register(echo_definition("echo")).unwrap();
        let _listener = bus.on_waterfall::<ToolsPreExecute, _>(move |_, _next| {
            Box::pin(async {
                Ok(PreToolDecision::Deny {
                    reason: "blocked by policy".into(),
                })
            })
        });
        let result = runtime.execute(execution("echo")).await.unwrap();
        let ToolExecutionResult::Failure { error, .. } = result else {
            panic!("expected failure");
        };
        assert_eq!(error.code, "DENIED");
        assert!(error.message.contains("blocked by policy"));
    }

    #[tokio::test]
    async fn ask_without_approval_degrades_to_denial() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = InMemoryToolRuntime::new(bus.clone(), ctx.clone());
        let _handle = runtime.register(echo_definition("echo")).unwrap();
        let _listener = bus.on_waterfall::<ToolsPreExecute, _>(move |_, _next| {
            Box::pin(async {
                Ok(PreToolDecision::Ask {
                    reason: Some("requires human".into()),
                })
            })
        });
        let result = runtime.execute(execution("echo")).await.unwrap();
        let ToolExecutionResult::Failure { error, .. } = result else {
            panic!("expected failure");
        };
        assert_eq!(error.code, "DENIED");
        assert!(error.message.contains("no approval channel"));
    }

    struct FakeApproval(starriver_kernel::approval::ApprovalOutcome);

    #[allow(clippy::type_complexity)]
    impl starriver_kernel::approval::ApprovalRuntime for FakeApproval {
        fn request(
            &self,
            _req: ApprovalRequest,
        ) -> cordis_rslite::BoxFuture<
            Result<ApprovalOutcome, starriver_kernel::approval::ApprovalError>,
        > {
            let outcome = self.0;
            Box::pin(async move { Ok(outcome) })
        }
    }

    fn register_fake_approval(ctx: &Context, outcome: ApprovalOutcome) {
        ctx.provide_any(
            "approval",
            Value::from_arc(Arc::new(ApprovalHandle(Arc::new(FakeApproval(outcome))))),
        )
        .unwrap();
    }

    async fn ask_scripted_run(
        ctx: &Context,
        bus: &EventBus,
        runtime: &InMemoryToolRuntime,
        outcome: ApprovalOutcome,
    ) -> ToolExecutionResult {
        register_fake_approval(ctx, outcome);
        let _listener = bus.on_waterfall::<ToolsPreExecute, _>(move |_event, _next| {
            Box::pin(async {
                Ok(PreToolDecision::Ask {
                    reason: Some("human".into()),
                })
            })
        });
        let mut input = execution("echo");
        input.agent_id = Some("s1".into());
        runtime.execute(input).await.unwrap()
    }

    #[tokio::test]
    async fn ask_allowed_once_dispatches() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = InMemoryToolRuntime::new(bus.clone(), ctx.clone());
        let _handle = runtime.register(echo_definition("echo")).unwrap();
        let result = ask_scripted_run(&ctx, &bus, &runtime, ApprovalOutcome::AllowedOnce).await;
        assert!(matches!(result, ToolExecutionResult::Success { .. }));
    }

    #[tokio::test]
    async fn ask_allowed_similar_dispatches() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = InMemoryToolRuntime::new(bus.clone(), ctx.clone());
        let _handle = runtime.register(echo_definition("echo")).unwrap();
        let result = ask_scripted_run(&ctx, &bus, &runtime, ApprovalOutcome::AllowedSimilar).await;
        assert!(matches!(result, ToolExecutionResult::Success { .. }));
    }

    #[tokio::test]
    async fn ask_rejected_maps_to_denial() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = InMemoryToolRuntime::new(bus.clone(), ctx.clone());
        let _handle = runtime.register(echo_definition("echo")).unwrap();
        let result = ask_scripted_run(&ctx, &bus, &runtime, ApprovalOutcome::Rejected).await;
        let ToolExecutionResult::Failure { error, .. } = result else {
            panic!("expected failure");
        };
        assert!(error.message.contains("rejected"));
    }

    #[tokio::test]
    async fn ask_cancelled_maps_to_denial() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = InMemoryToolRuntime::new(bus.clone(), ctx.clone());
        let _handle = runtime.register(echo_definition("echo")).unwrap();
        let result = ask_scripted_run(&ctx, &bus, &runtime, ApprovalOutcome::Cancelled).await;
        let ToolExecutionResult::Failure { error, .. } = result else {
            panic!("expected failure");
        };
        assert!(error.message.contains("cancelled"));
    }

    #[tokio::test]
    async fn ask_unavailable_maps_to_fail_closed() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = InMemoryToolRuntime::new(bus.clone(), ctx.clone());
        let _handle = runtime.register(echo_definition("echo")).unwrap();
        let result = ask_scripted_run(&ctx, &bus, &runtime, ApprovalOutcome::Unavailable).await;
        let ToolExecutionResult::Failure { error, .. } = result else {
            panic!("expected failure");
        };
        assert!(error.message.contains("no approval channel"));
    }

    struct FakeApprovalErr;

    impl starriver_kernel::approval::ApprovalRuntime for FakeApprovalErr {
        fn request(
            &self,
            _req: ApprovalRequest,
        ) -> cordis_rslite::BoxFuture<
            Result<ApprovalOutcome, starriver_kernel::approval::ApprovalError>,
        > {
            Box::pin(async { Err(starriver_kernel::approval::ApprovalError::NoOpenTurn) })
        }
    }

    async fn ask_with_approval_impl(
        ctx: &Context,
        bus: &EventBus,
        runtime: &InMemoryToolRuntime,
        approval: Arc<dyn starriver_kernel::approval::ApprovalRuntime>,
        agent_id: Option<String>,
    ) -> ToolExecutionResult {
        ctx.provide_any(
            "approval",
            Value::from_arc(Arc::new(ApprovalHandle(approval))),
        )
        .unwrap();
        let _listener = bus.on_waterfall::<ToolsPreExecute, _>(move |_event, _next| {
            Box::pin(async {
                Ok(PreToolDecision::Ask {
                    reason: Some("human".into()),
                })
            })
        });
        let mut input = execution("echo");
        input.agent_id = agent_id;
        runtime.execute(input).await.unwrap()
    }

    #[tokio::test]
    async fn ask_misconfigured_approval_denies() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = InMemoryToolRuntime::new(bus.clone(), ctx.clone());
        let _handle = runtime.register(echo_definition("echo")).unwrap();
        // Provide a value under the approval key that is NOT an ApprovalHandle.
        ctx.provide_any("approval", Value::new(42u32)).unwrap();
        let _listener = bus.on_waterfall::<ToolsPreExecute, _>(move |_event, _next| {
            Box::pin(async {
                Ok(PreToolDecision::Ask {
                    reason: Some("human".into()),
                })
            })
        });
        let mut input = execution("echo");
        input.agent_id = Some("s1".into());
        let result = runtime.execute(input).await.unwrap();
        let ToolExecutionResult::Failure { error, .. } = result else {
            panic!("expected failure");
        };
        assert!(error.message.contains("misconfigured"));
    }

    #[tokio::test]
    async fn ask_without_agent_denies() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = InMemoryToolRuntime::new(bus.clone(), ctx.clone());
        let _handle = runtime.register(echo_definition("echo")).unwrap();
        let result = ask_with_approval_impl(
            &ctx,
            &bus,
            &runtime,
            Arc::new(FakeApproval(ApprovalOutcome::AllowedOnce)),
            None,
        )
        .await;
        let ToolExecutionResult::Failure { error, .. } = result else {
            panic!("expected failure");
        };
        assert!(error.message.contains("no agent"));
    }

    #[tokio::test]
    async fn ask_request_error_fails_closed() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = InMemoryToolRuntime::new(bus.clone(), ctx.clone());
        let _handle = runtime.register(echo_definition("echo")).unwrap();
        let result = ask_with_approval_impl(
            &ctx,
            &bus,
            &runtime,
            Arc::new(FakeApprovalErr),
            Some("s1".into()),
        )
        .await;
        let ToolExecutionResult::Failure { error, .. } = result else {
            panic!("expected failure");
        };
        assert!(error.message.contains("no approval channel"));
    }

    #[tokio::test]
    async fn guard_denies_after_pre_execute() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = InMemoryToolRuntime::new(bus.clone(), ctx.clone());
        let _handle = runtime.register(echo_definition("echo")).unwrap();
        let _guard = runtime
            .guard(Arc::new(|execution| {
                (execution.name == "echo").then(|| "guarded".into())
            }))
            .unwrap();
        let result = runtime.execute(execution("echo")).await.unwrap();
        let ToolExecutionResult::Failure { error, .. } = result else {
            panic!("expected failure");
        };
        assert_eq!(error.code, "DENIED");
        assert_eq!(error.message, "guarded");
    }

    #[tokio::test]
    async fn execute_waterfall_wraps_body() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = InMemoryToolRuntime::new(bus.clone(), ctx.clone());
        let _handle = runtime.register(echo_definition("echo")).unwrap();
        let order = Arc::new(Mutex::new(Vec::new()));
        let _listener = bus.on_waterfall::<ToolsExecute, _>({
            let order = Arc::clone(&order);
            move |_, next| {
                let order = Arc::clone(&order);
                Box::pin(async move {
                    order.lock().unwrap().push("wrapper:before");
                    let result = next().await;
                    order.lock().unwrap().push("wrapper:after");
                    result
                })
            }
        });
        let result = runtime.execute(execution("echo")).await.unwrap();
        assert!(matches!(result, ToolExecutionResult::Success { .. }));
        assert_eq!(
            *order.lock().unwrap(),
            vec!["wrapper:before", "wrapper:after"]
        );
    }

    #[tokio::test]
    async fn post_execute_accept_replaces_content() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = InMemoryToolRuntime::new(bus.clone(), ctx.clone());
        let _handle = runtime.register(echo_definition("echo")).unwrap();
        let _listener = bus.on_waterfall::<ToolsPostExecute, _>(move |_, _next| {
            Box::pin(async {
                Ok(PostToolDecision::Accept {
                    content: Some(vec![ContentBlock::Text("replaced".into())]),
                    value: None,
                    additional_contexts: Vec::new(),
                })
            })
        });
        let result = runtime.execute(execution("echo")).await.unwrap();
        let ToolExecutionResult::Success { content, .. } = result else {
            panic!("expected success");
        };
        assert!(matches!(content.as_slice(), [ContentBlock::Text(text)] if text == "replaced"));
    }

    #[tokio::test]
    async fn post_execute_block_returns_failure() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = InMemoryToolRuntime::new(bus.clone(), ctx.clone());
        let _handle = runtime.register(echo_definition("echo")).unwrap();
        let _listener = bus.on_waterfall::<ToolsPostExecute, _>(move |_, _next| {
            Box::pin(async {
                Ok(PostToolDecision::Block {
                    feedback: vec![ContentBlock::Text("corrective feedback".into())],
                    additional_contexts: Vec::new(),
                })
            })
        });
        let result = runtime.execute(execution("echo")).await.unwrap();
        let ToolExecutionResult::Failure {
            error,
            content,
            additional_contexts,
        } = result
        else {
            panic!("expected failure");
        };
        assert_eq!(error.code, "BLOCKED");
        assert_eq!(error.message, "corrective feedback");
        assert!(
            matches!(content.as_slice(), [ContentBlock::Text(text)] if text == "corrective feedback")
        );
        assert!(additional_contexts.is_empty());
    }

    #[tokio::test]
    async fn render_produces_model_content() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = InMemoryToolRuntime::new(bus.clone(), ctx.clone());
        let definition = ToolDefinition {
            requires_approval: false,
            deny_in_readonly: false,
            render: Arc::new(|args, value| {
                vec![ContentBlock::Text(format!(
                    "rendered({args:?} -> {value:?})"
                ))]
            }),
            ..echo_definition("render")
        };
        let _handle = runtime.register(definition).unwrap();
        let result = runtime.execute(execution("render")).await.unwrap();
        let ToolExecutionResult::Success { content, .. } = result else {
            panic!("expected success");
        };
        assert!(
            matches!(content.as_slice(), [ContentBlock::Text(text)] if text.starts_with("rendered("))
        );
    }

    #[tokio::test]
    async fn finalize_content_transforms_final_outcome() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = InMemoryToolRuntime::new(bus.clone(), ctx.clone());
        let definition = ToolDefinition {
            requires_approval: false,
            deny_in_readonly: false,
            finalize_content: Some(Arc::new(|_, _| {
                Some(vec![ContentBlock::Text("finalized".into())])
            })),
            ..echo_definition("finalize")
        };
        let _handle = runtime.register(definition).unwrap();
        let result = runtime.execute(execution("finalize")).await.unwrap();
        let ToolExecutionResult::Success { content, .. } = result else {
            panic!("expected success");
        };
        assert!(matches!(content.as_slice(), [ContentBlock::Text(text)] if text == "finalized"));
    }

    #[tokio::test]
    async fn deferred_context_and_conclude_turn_ride_success() {
        struct Deferring;
        impl ToolExecutor for Deferring {
            fn execute(
                &self,
                arguments: Value,
                ctx: ToolRunContext,
            ) -> cordis_rslite::BoxFuture<Result<Value, ToolError>> {
                Box::pin(async move {
                    ctx.defer_context(Message {
                        id: MessageId("m1".into()),
                        role: MessageRole::User,
                        content: vec![ContentBlock::Text("deferred".into())],
                        source: MessageSource::User,
                    });
                    ctx.conclude_turn();
                    Ok(arguments)
                })
            }
        }

        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = InMemoryToolRuntime::new(bus.clone(), ctx.clone());
        let definition = ToolDefinition {
            requires_approval: false,
            deny_in_readonly: false,
            name: "defer".into(),
            description: "defer".into(),
            parameters: Value::new(()),
            output: Value::new(()),
            render: Arc::new(|_, value| vec![ContentBlock::Text(format!("{value:?}"))]),
            executor: Arc::new(Deferring),
            is_concurrency_safe: None,
            timeout: None,
            finalize_content: None,
        };
        let _handle = runtime.register(definition).unwrap();
        let result = runtime.execute(execution("defer")).await.unwrap();
        let ToolExecutionResult::Success {
            additional_contexts,
            concludes_turn,
            ..
        } = result
        else {
            panic!("expected success");
        };
        assert_eq!(additional_contexts.len(), 1);
        assert!(concludes_turn);
    }

    #[tokio::test]
    async fn execution_mode_classifies_fail_closed() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = InMemoryToolRuntime::new(bus.clone(), ctx.clone());
        let _parallel = runtime.register(ToolDefinition {
            requires_approval: false,
            deny_in_readonly: false,
            is_concurrency_safe: Some(Arc::new(|_| true)),
            ..echo_definition("parallel")
        });
        let _exclusive = runtime.register(echo_definition("exclusive"));
        let _falsey = runtime.register(ToolDefinition {
            requires_approval: false,
            deny_in_readonly: false,
            is_concurrency_safe: Some(Arc::new(|_| false)),
            ..echo_definition("falsey")
        });
        assert_eq!(
            runtime.execution_mode(&execution("parallel")),
            ToolExecutionMode::Parallel
        );
        assert_eq!(
            runtime.execution_mode(&execution("exclusive")),
            ToolExecutionMode::Exclusive
        );
        assert_eq!(
            runtime.execution_mode(&execution("falsey")),
            ToolExecutionMode::Exclusive
        );
        assert_eq!(
            runtime.execution_mode(&execution("missing")),
            ToolExecutionMode::Exclusive
        );
    }

    #[tokio::test]
    async fn code_dispatch_log_reshapes_content() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = InMemoryToolRuntime::new(bus.clone(), ctx.clone());
        let dispatch = CodeDispatchLog {
            execution: assembled_execution("run_code"),
            agent_id: None,
            sub_call_id: CallId("sub1".into()),
            name: "echo".into(),
            is_error: false,
            content: vec![ContentBlock::Text("original".into())],
        };

        let reshaped = runtime.shape_dispatch_log(dispatch.clone()).await;
        assert!(matches!(reshaped.as_slice(), [ContentBlock::Text(text)] if text == "original"));

        let _listener = bus.on_waterfall::<ToolsCodeDispatchLog, _>(move |_, _next| {
            Box::pin(async { Ok(vec![ContentBlock::Text("preview + locator".into())]) })
        });
        let reshaped = runtime.shape_dispatch_log(dispatch.clone()).await;
        assert!(
            matches!(reshaped.as_slice(), [ContentBlock::Text(text)] if text == "preview + locator")
        );
    }

    #[tokio::test]
    async fn cancellation_before_dispatch_skips_body() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = InMemoryToolRuntime::new(bus.clone(), ctx.clone());
        let _handle = runtime.register(echo_definition("echo")).unwrap();
        let token = tokio_util::sync::CancellationToken::new();
        token.cancel();
        let mut execution = execution("echo");
        execution.cancellation = Some(token);
        let result = runtime.execute(execution).await.unwrap();
        let ToolExecutionResult::Failure { error, .. } = result else {
            panic!("expected failure");
        };
        assert_eq!(error.code, "ABORTED_BEFORE_DISPATCH");
    }

    #[tokio::test]
    async fn cancellation_after_body_started_yields_aborted() {
        struct SlowBody {
            started: Arc<tokio::sync::Notify>,
        }
        impl ToolExecutor for SlowBody {
            fn execute(
                &self,
                arguments: Value,
                ctx: ToolRunContext,
            ) -> cordis_rslite::BoxFuture<Result<Value, ToolError>> {
                let started = Arc::clone(&self.started);
                let cancellation = ctx.cancellation();
                Box::pin(async move {
                    started.notify_one();
                    if let Some(token) = cancellation {
                        token.cancelled().await;
                    }
                    Ok(arguments)
                })
            }
        }

        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = Arc::new(InMemoryToolRuntime::new(bus.clone(), ctx.clone()));
        let started = Arc::new(tokio::sync::Notify::new());
        let definition = ToolDefinition {
            requires_approval: false,
            deny_in_readonly: false,
            name: "slow".into(),
            description: "slow".into(),
            parameters: Value::new(()),
            output: Value::new(()),
            render: Arc::new(|_, value| vec![ContentBlock::Text(format!("{value:?}"))]),
            executor: Arc::new(SlowBody {
                started: Arc::clone(&started),
            }),
            is_concurrency_safe: None,
            timeout: None,
            finalize_content: None,
        };
        let _handle = runtime.register(definition).unwrap();

        let token = tokio_util::sync::CancellationToken::new();
        let mut execution = execution("slow");
        execution.cancellation = Some(token.clone());
        let spawned = Arc::clone(&runtime);
        let notified = started.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let task = tokio::spawn(async move { spawned.execute(execution).await });
        notified.await;
        token.cancel();
        let result = task.await.unwrap().unwrap();
        let ToolExecutionResult::Failure { error, .. } = result else {
            panic!("expected failure");
        };
        assert_eq!(error.code, "ABORTED");
    }

    #[tokio::test]
    async fn tools_result_event_observes_final_outcome() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = InMemoryToolRuntime::new(bus.clone(), ctx.clone());
        let _handle = runtime.register(echo_definition("echo")).unwrap();
        let observed = Arc::new(Mutex::new(None));
        let _listener = bus.on::<ToolsResult, _>({
            let observed = Arc::clone(&observed);
            move |event| {
                let observed = Arc::clone(&observed);
                Box::pin(async move {
                    *observed.lock().unwrap() = Some(event.result.clone());
                    Ok(Value::new(()))
                })
            }
        });
        let _ = runtime.execute(execution("echo")).await.unwrap();
        // `emit` is fire-and-forget: give the spawned listener a chance to run.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        let observed = observed.lock().unwrap().clone();
        assert!(matches!(
            observed,
            Some(ToolExecutionResult::Success { .. })
        ));
    }

    #[tokio::test]
    async fn tools_result_event_observes_denied_outcome() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = InMemoryToolRuntime::new(bus.clone(), ctx.clone());
        let _handle = runtime.register(echo_definition("echo")).unwrap();
        let _listener = bus.on_waterfall::<ToolsPreExecute, _>(move |_, _next| {
            Box::pin(async {
                Ok(PreToolDecision::Deny {
                    reason: "not today".into(),
                })
            })
        });
        let observed = Arc::new(Mutex::new(None));
        let _result_observer = bus.on::<ToolsResult, _>({
            let observed = Arc::clone(&observed);
            move |event| {
                let observed = Arc::clone(&observed);
                Box::pin(async move {
                    *observed.lock().unwrap() = Some(event.result.clone());
                    Ok(Value::new(()))
                })
            }
        });
        let _ = runtime.execute(execution("echo")).await.unwrap();
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        let observed = observed.lock().unwrap().clone();
        assert!(matches!(
            observed,
            Some(ToolExecutionResult::Failure { .. })
        ));
    }

    #[tokio::test]
    async fn pre_execute_listener_error_yields_event_failure() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = InMemoryToolRuntime::new(bus.clone(), ctx.clone());
        let _handle = runtime.register(echo_definition("echo")).unwrap();
        let _listener = bus.on_waterfall::<ToolsPreExecute, _>(move |_, _next| {
            Box::pin(async { Err(EventError::Listener("policy crashed".into())) })
        });
        let result = runtime.execute(execution("echo")).await.unwrap();
        let ToolExecutionResult::Failure { error, .. } = result else {
            panic!("expected failure");
        };
        assert_eq!(error.code, "EVENT");
        assert!(error.message.contains("policy crashed"));
    }

    #[tokio::test]
    async fn execute_wrapper_error_yields_event_failure() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = InMemoryToolRuntime::new(bus.clone(), ctx.clone());
        let _handle = runtime.register(echo_definition("echo")).unwrap();
        let _listener = bus.on_waterfall::<ToolsExecute, _>(move |_, _next| {
            Box::pin(async { Err(EventError::Listener("wrapper crashed".into())) })
        });
        let result = runtime.execute(execution("echo")).await.unwrap();
        let ToolExecutionResult::Failure { error, .. } = result else {
            panic!("expected failure");
        };
        assert_eq!(error.code, "EVENT");
    }

    #[tokio::test]
    async fn post_execute_listener_error_yields_event_failure() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = InMemoryToolRuntime::new(bus.clone(), ctx.clone());
        let _handle = runtime.register(echo_definition("echo")).unwrap();
        let _listener = bus.on_waterfall::<ToolsPostExecute, _>(move |_, _next| {
            Box::pin(async { Err(EventError::Listener("post crashed".into())) })
        });
        let result = runtime.execute(execution("echo")).await.unwrap();
        let ToolExecutionResult::Failure { error, .. } = result else {
            panic!("expected failure");
        };
        assert_eq!(error.code, "EVENT");
    }

    #[tokio::test]
    async fn event_failure_still_notifies_tools_result() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = InMemoryToolRuntime::new(bus.clone(), ctx.clone());
        let _handle = runtime.register(echo_definition("echo")).unwrap();
        let _listener = bus.on_waterfall::<ToolsPreExecute, _>(move |_, _next| {
            Box::pin(async { Err(EventError::Listener("boom".into())) })
        });
        let observed = Arc::new(Mutex::new(None));
        let _observer = bus.on::<ToolsResult, _>({
            let observed = Arc::clone(&observed);
            move |event| {
                let observed = Arc::clone(&observed);
                Box::pin(async move {
                    *observed.lock().unwrap() = Some(event.result.clone());
                    Ok(Value::new(()))
                })
            }
        });
        let result = runtime.execute(execution("echo")).await.unwrap();
        assert!(result.is_error());
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        let observed = observed.lock().unwrap().clone();
        let Some(ToolExecutionResult::Failure { error, .. }) = observed else {
            panic!("expected failure notification");
        };
        assert_eq!(error.code, "EVENT");
    }

    #[tokio::test]
    async fn guard_panic_is_contained_as_denial() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = InMemoryToolRuntime::new(bus.clone(), ctx.clone());
        let _handle = runtime.register(echo_definition("echo")).unwrap();
        let _guard = runtime
            .guard(Arc::new(|_| panic!("guard exploded")))
            .unwrap();
        let result = runtime.execute(execution("echo")).await.unwrap();
        let ToolExecutionResult::Failure { error, .. } = result else {
            panic!("expected failure");
        };
        assert_eq!(error.code, "DENIED");
        assert!(error.message.contains("guard panicked"));
        assert!(error.message.contains("guard exploded"));
    }

    #[tokio::test]
    async fn multiple_guards_first_denial_wins() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = InMemoryToolRuntime::new(bus.clone(), ctx.clone());
        let _handle = runtime.register(echo_definition("echo")).unwrap();
        let _first = runtime.guard(Arc::new(|_| Some("first".into()))).unwrap();
        let _second = runtime.guard(Arc::new(|_| Some("second".into()))).unwrap();
        let result = runtime.execute(execution("echo")).await.unwrap();
        let ToolExecutionResult::Failure { error, .. } = result else {
            panic!("expected failure");
        };
        assert_eq!(error.message, "first");
    }

    #[tokio::test]
    async fn guard_dispose_removes_guard() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = InMemoryToolRuntime::new(bus.clone(), ctx.clone());
        let _handle = runtime.register(echo_definition("echo")).unwrap();
        let guard = runtime.guard(Arc::new(|_| Some("guarded".into()))).unwrap();
        let denied = runtime.execute(execution("echo")).await.unwrap();
        assert!(denied.is_error());
        guard.dispose_async().await;
        let allowed = runtime.execute(execution("echo")).await.unwrap();
        assert!(matches!(allowed, ToolExecutionResult::Success { .. }));
    }

    #[tokio::test]
    async fn classifier_panic_is_fail_closed_exclusive() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = InMemoryToolRuntime::new(bus.clone(), ctx.clone());
        let _handle = runtime.register(ToolDefinition {
            requires_approval: false,
            deny_in_readonly: false,
            is_concurrency_safe: Some(Arc::new(|_| panic!("classifier exploded"))),
            ..echo_definition("panic")
        });
        assert_eq!(
            runtime.execution_mode(&execution("panic")),
            ToolExecutionMode::Exclusive
        );
    }

    #[tokio::test]
    async fn render_panic_yields_invalid_output() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = InMemoryToolRuntime::new(bus.clone(), ctx.clone());
        let _handle = runtime.register(ToolDefinition {
            requires_approval: false,
            deny_in_readonly: false,
            render: Arc::new(|_, _| panic!("render exploded")),
            ..echo_definition("panic-render")
        });
        let result = runtime.execute(execution("panic-render")).await.unwrap();
        let ToolExecutionResult::Failure { error, .. } = result else {
            panic!("expected failure");
        };
        assert_eq!(error.code, "INVALID_TOOL_OUTPUT");
        assert!(error.message.contains("render failed"));
    }

    #[tokio::test]
    async fn finalize_content_panic_yields_failure() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = InMemoryToolRuntime::new(bus.clone(), ctx.clone());
        let _handle = runtime.register(ToolDefinition {
            requires_approval: false,
            deny_in_readonly: false,
            finalize_content: Some(Arc::new(|_, _| panic!("finalize exploded"))),
            ..echo_definition("panic-finalize")
        });
        let result = runtime.execute(execution("panic-finalize")).await.unwrap();
        let ToolExecutionResult::Failure { error, .. } = result else {
            panic!("expected failure");
        };
        assert_eq!(error.code, "FINALIZE_FAILED");
        assert!(error.message.contains("finalize exploded"));
    }

    #[tokio::test]
    async fn accept_with_value_rerenders() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = InMemoryToolRuntime::new(bus.clone(), ctx.clone());
        let _handle = runtime.register(ToolDefinition {
            requires_approval: false,
            deny_in_readonly: false,
            render: Arc::new(|_, value| vec![ContentBlock::Text(format!("rendered:{value:?}"))]),
            ..echo_definition("replace")
        });
        let _listener = bus.on_waterfall::<ToolsPostExecute, _>(move |_, _next| {
            Box::pin(async {
                Ok(PostToolDecision::Accept {
                    content: None,
                    value: Some(Value::new("replaced-value".to_string())),
                    additional_contexts: Vec::new(),
                })
            })
        });
        let result = runtime.execute(execution("replace")).await.unwrap();
        let ToolExecutionResult::Success { value, content, .. } = result else {
            panic!("expected success");
        };
        let replaced = value.downcast::<String>().unwrap();
        assert_eq!(replaced.as_str(), "replaced-value");
        assert!(
            matches!(content.as_slice(), [ContentBlock::Text(text)] if text.starts_with("rendered:"))
        );
    }

    #[tokio::test]
    async fn accept_appends_additional_contexts() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = InMemoryToolRuntime::new(bus.clone(), ctx.clone());
        let _handle = runtime.register(echo_definition("echo")).unwrap();
        let _listener = bus.on_waterfall::<ToolsPostExecute, _>(move |_, _next| {
            Box::pin(async {
                Ok(PostToolDecision::Accept {
                    content: None,
                    value: None,
                    additional_contexts: vec![Message {
                        id: MessageId("decision".into()),
                        role: MessageRole::User,
                        content: vec![ContentBlock::Text("decision context".into())],
                        source: MessageSource::User,
                    }],
                })
            })
        });
        let result = runtime.execute(execution("echo")).await.unwrap();
        let ToolExecutionResult::Success {
            additional_contexts,
            ..
        } = result
        else {
            panic!("expected success");
        };
        assert_eq!(additional_contexts.len(), 1);
    }

    #[tokio::test]
    async fn block_discards_deferred_contexts() {
        struct Deferring;
        impl ToolExecutor for Deferring {
            fn execute(
                &self,
                arguments: Value,
                ctx: ToolRunContext,
            ) -> cordis_rslite::BoxFuture<Result<Value, ToolError>> {
                Box::pin(async move {
                    ctx.defer_context(Message {
                        id: MessageId("body".into()),
                        role: MessageRole::User,
                        content: vec![ContentBlock::Text("deferred by body".into())],
                        source: MessageSource::User,
                    });
                    Ok(arguments)
                })
            }
        }
        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = InMemoryToolRuntime::new(bus.clone(), ctx.clone());
        let definition = ToolDefinition {
            requires_approval: false,
            deny_in_readonly: false,
            name: "defer".into(),
            description: "defer".into(),
            parameters: Value::new(()),
            output: Value::new(()),
            render: Arc::new(|_, value| vec![ContentBlock::Text(format!("{value:?}"))]),
            executor: Arc::new(Deferring),
            is_concurrency_safe: None,
            timeout: None,
            finalize_content: None,
        };
        let _handle = runtime.register(definition).unwrap();
        let _listener = bus.on_waterfall::<ToolsPostExecute, _>(move |_, _next| {
            Box::pin(async {
                Ok(PostToolDecision::Block {
                    feedback: vec![ContentBlock::Text("blocked".into())],
                    additional_contexts: Vec::new(),
                })
            })
        });
        let result = runtime.execute(execution("defer")).await.unwrap();
        let ToolExecutionResult::Failure {
            additional_contexts,
            ..
        } = result
        else {
            panic!("expected failure");
        };
        assert!(additional_contexts.is_empty());
    }

    #[tokio::test]
    async fn cancellation_after_policy_runs_post_execute() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = InMemoryToolRuntime::new(bus.clone(), ctx.clone());
        let _handle = runtime.register(echo_definition("echo")).unwrap();
        let token = tokio_util::sync::CancellationToken::new();
        let post_ran = Arc::new(Mutex::new(false));
        let _pre = bus.on_waterfall::<ToolsPreExecute, _>({
            let token = token.clone();
            move |_, _next| {
                let token = token.clone();
                Box::pin(async move {
                    token.cancel();
                    Ok(PreToolDecision::Allow)
                })
            }
        });
        let _post = bus.on_waterfall::<ToolsPostExecute, _>({
            let post_ran = Arc::clone(&post_ran);
            move |_, _next| {
                let post_ran = Arc::clone(&post_ran);
                Box::pin(async move {
                    *post_ran.lock().unwrap() = true;
                    Ok(PostToolDecision::Accept {
                        content: None,
                        value: None,
                        additional_contexts: Vec::new(),
                    })
                })
            }
        });
        let mut execution = execution("echo");
        execution.cancellation = Some(token);
        let result = runtime.execute(execution).await.unwrap();
        let ToolExecutionResult::Failure { error, .. } = result else {
            panic!("expected failure");
        };
        assert_eq!(error.code, "ABORTED_BEFORE_DISPATCH");
        assert!(*post_ran.lock().unwrap());
    }

    #[tokio::test]
    async fn cancellation_during_post_execute_yields_aborted() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = InMemoryToolRuntime::new(bus.clone(), ctx.clone());
        let _handle = runtime.register(echo_definition("echo")).unwrap();
        let token = tokio_util::sync::CancellationToken::new();
        let _post = bus.on_waterfall::<ToolsPostExecute, _>({
            let token = token.clone();
            move |_, _next| {
                let token = token.clone();
                Box::pin(async move {
                    token.cancel();
                    Ok(PostToolDecision::Accept {
                        content: None,
                        value: None,
                        additional_contexts: Vec::new(),
                    })
                })
            }
        });
        let mut execution = execution("echo");
        execution.cancellation = Some(token);
        let result = runtime.execute(execution).await.unwrap();
        let ToolExecutionResult::Failure { error, .. } = result else {
            panic!("expected failure");
        };
        assert_eq!(error.code, "ABORTED");
    }

    #[tokio::test]
    async fn unknown_tool_notifies_tools_result() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = InMemoryToolRuntime::new(bus.clone(), ctx.clone());
        let observed = Arc::new(Mutex::new(None));
        let _observer = bus.on::<ToolsResult, _>({
            let observed = Arc::clone(&observed);
            move |event| {
                let observed = Arc::clone(&observed);
                Box::pin(async move {
                    *observed.lock().unwrap() = Some(event.result.clone());
                    Ok(Value::new(()))
                })
            }
        });
        let result = runtime.execute(execution("missing")).await.unwrap();
        let ToolExecutionResult::Failure { error, .. } = result else {
            panic!("expected failure");
        };
        assert_eq!(error.code, "UNKNOWN_TOOL");
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        let observed = observed.lock().unwrap().clone();
        let Some(ToolExecutionResult::Failure { error, .. }) = observed else {
            panic!("expected failure notification");
        };
        assert_eq!(error.code, "UNKNOWN_TOOL");
    }

    #[tokio::test]
    async fn execute_mints_unique_tokens_and_resolves_root_call_id() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = InMemoryToolRuntime::new(bus.clone(), ctx.clone());
        let _handle = runtime.register(echo_definition("echo")).unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let _observer = bus.on::<ToolsResult, _>({
            let seen = Arc::clone(&seen);
            move |event| {
                let seen = Arc::clone(&seen);
                Box::pin(async move {
                    seen.lock()
                        .unwrap()
                        .push((event.execution.token, event.execution.root_call_id.clone()));
                    Ok(Value::new(()))
                })
            }
        });

        let mut first = execution("echo");
        first.root_call_id = Some(CallId("root-1".into()));
        let _ = runtime.execute(first).await.unwrap();
        let _ = runtime.execute(execution("echo")).await.unwrap();
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 2);
        assert_ne!(seen[0].0, seen[1].0);
        assert_eq!(seen[0].1 .0, "root-1");
        assert_eq!(seen[1].1 .0, "c1");
    }

    #[tokio::test]
    async fn register_emits_tools_available() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = InMemoryToolRuntime::new(bus.clone(), ctx.clone());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let _listener = bus.on_sync::<ToolsAvailable, _>({
            let seen = Arc::clone(&seen);
            move |event| {
                seen.lock().unwrap().push(event.names.clone());
                Ok(Value::new(()))
            }
        });
        let _handle = runtime.register(echo_definition("echo")).unwrap();
        let _second = runtime.register(echo_definition("other")).unwrap();
        let snapshots = seen.lock().unwrap().clone();
        assert_eq!(snapshots.len(), 2);
        assert!(snapshots[0].contains(&"echo".to_string()));
        assert!(snapshots[1].contains(&"other".to_string()));
    }

    #[tokio::test]
    async fn unregister_removes_tool_and_emits_available() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = InMemoryToolRuntime::new(bus.clone(), ctx.clone());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let _listener = bus.on_sync::<ToolsAvailable, _>({
            let seen = Arc::clone(&seen);
            move |event| {
                seen.lock().unwrap().push(event.names.clone());
                Ok(Value::new(()))
            }
        });
        let handle = runtime.register(echo_definition("echo")).unwrap();
        handle.dispose_async().await;
        let result = runtime.execute(execution("echo")).await.unwrap();
        let ToolExecutionResult::Failure { error, .. } = result else {
            panic!("expected failure");
        };
        assert_eq!(error.code, "UNKNOWN_TOOL");
        let snapshots = seen.lock().unwrap().clone();
        assert_eq!(snapshots.len(), 2);
        assert!(snapshots[0].contains(&"echo".to_string()));
        assert!(snapshots[1].is_empty());
    }

    #[tokio::test]
    async fn duplicate_registration_is_rejected() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = InMemoryToolRuntime::new(bus.clone(), ctx.clone());
        let _first = runtime.register(echo_definition("echo")).unwrap();
        let error = match runtime.register(echo_definition("echo")) {
            Ok(_) => panic!("expected duplicate registration to fail"),
            Err(error) => error,
        };
        assert!(matches!(error, ToolError::AlreadyRegistered(name) if name == "echo"));
    }

    #[tokio::test]
    async fn plugin_provides_tool_runtime() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let registry = PluginRegistry::new();
        registry.register(in_memory_tools_plugin());
        let handles = registry.boot(&ctx, &bus).await.unwrap();
        assert!(ctx.require::<InMemoryToolRuntime>().is_ok());
        assert_eq!(handles.len(), 1);
    }

    #[tokio::test]
    async fn staged_prepare_dispatch_success() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = InMemoryToolRuntime::new(bus.clone(), ctx.clone());
        let _handle = runtime.register(echo_definition("echo")).unwrap();

        let ScheduledToolPreparation::Dispatch { exec } =
            runtime.prepare(execution("echo")).await.unwrap()
        else {
            panic!("expected dispatch stage");
        };
        let ScheduledToolDispatch::PostResult { result } = runtime.dispatch(exec).await.unwrap()
        else {
            panic!("expected post-result stage");
        };
        assert!(matches!(result, ToolExecutionResult::Success { .. }));
    }

    #[tokio::test]
    async fn staged_prepare_cancelled_before_dispatch_is_final() {
        use tokio_util::sync::CancellationToken;

        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = InMemoryToolRuntime::new(bus.clone(), ctx.clone());
        let _handle = runtime.register(echo_definition("echo")).unwrap();
        let token = CancellationToken::new();
        token.cancel();
        let mut input = execution("echo");
        input.cancellation = Some(token);

        let preparation = runtime.prepare(input).await.unwrap();
        let ScheduledToolPreparation::FinalResult { result, .. } = preparation else {
            panic!("expected final result");
        };
        let ToolExecutionResult::Failure { error, .. } = result else {
            panic!("expected failure");
        };
        assert_eq!(error.code, "ABORTED_BEFORE_DISPATCH");
    }

    #[tokio::test]
    async fn staged_chain_equals_execute() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = InMemoryToolRuntime::new(bus.clone(), ctx.clone());
        let _handle = runtime.register(echo_definition("echo")).unwrap();

        let direct = runtime.execute(execution("echo")).await.unwrap();
        let preparation = runtime.prepare(execution("echo")).await.unwrap();
        let staged = match preparation {
            ScheduledToolPreparation::Dispatch { exec } => {
                match runtime.dispatch(exec.clone()).await.unwrap() {
                    ScheduledToolDispatch::PostResult { result } => {
                        runtime.finalize(exec, result).await.unwrap()
                    }
                    ScheduledToolDispatch::FinalResult { result } => {
                        runtime.finish(exec, result).unwrap()
                    }
                }
            }
            ScheduledToolPreparation::PostResult { exec, result } => {
                runtime.finalize(exec, result).await.unwrap()
            }
            ScheduledToolPreparation::FinalResult { exec, result } => {
                runtime.finish(exec, result).unwrap()
            }
        };
        assert_eq!(format!("{staged:?}"), format!("{direct:?}"));
    }

    #[tokio::test]
    async fn finish_emits_tools_result() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let results = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&results);
        let ctx = Context::new();
        let bus = EventBus::new();
        let _listener = bus.on::<ToolsResult, _>(move |_| {
            let counter = Arc::clone(&counter);
            Box::pin(async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(Value::new(()))
            })
        });
        let runtime = InMemoryToolRuntime::new(bus.clone(), ctx.clone());
        let _handle = runtime.register(echo_definition("echo")).unwrap();

        let preparation = runtime.prepare(execution("echo")).await.unwrap();
        let ScheduledToolPreparation::Dispatch { exec } = preparation else {
            panic!("expected dispatch stage");
        };
        let ScheduledToolDispatch::PostResult { result } =
            runtime.dispatch(exec.clone()).await.unwrap()
        else {
            panic!("expected post-result stage");
        };
        runtime.finalize(exec, result).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(results.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn schemas_lists_registered_tool_definitions() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = InMemoryToolRuntime::new(bus, ctx);
        let _handle = runtime.register(echo_definition("echo")).unwrap();

        let schemas = runtime.schemas();
        assert_eq!(schemas.len(), 1);
        assert_eq!(schemas[0].name, "echo");
        assert_eq!(schemas[0].description, "echo");
        assert_eq!(schemas[0].parameters, JsonValue::Null);
    }

    #[test]
    fn schemas_carries_serde_json_parameters() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let runtime = InMemoryToolRuntime::new(bus, ctx);
        let _handle = runtime
            .register(ToolDefinition {
                name: "echo".into(),
                description: "echo".into(),
                parameters: Value::new(serde_json::json!({
                    "type": "object",
                    "properties": {"text": {"type": "string"}},
                    "required": ["text"],
                })),
                output: Value::new(()),
                render: Arc::new(|_, _| Vec::new()),
                executor: Arc::new(Echo),
                is_concurrency_safe: None,
                timeout: None,
                finalize_content: None,
                requires_approval: false,
                deny_in_readonly: false,
            })
            .unwrap();
        let schemas = runtime.schemas();
        assert_eq!(schemas.len(), 1);
        assert_ne!(schemas[0].parameters, JsonValue::Null);
        let expected = JsonValue::from_serde_json(serde_json::json!({
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"],
        }));
        assert_eq!(schemas[0].parameters, expected);
    }

    #[tokio::test]
    async fn restrict_for_filters_schemas_by_agent() {
        use starriver_kernel::tools::ToolRuntime;

        let runtime = InMemoryToolRuntime::new(EventBus::new(), Context::new());
        let _h1 = runtime.register(echo_definition("echo")).unwrap();
        let _h2 = runtime.register(echo_definition("bash")).unwrap();
        // No restriction: all visible.
        assert_eq!(runtime.schemas().len(), 2);
        let restrict = runtime.restrict_for("a1", vec!["echo".into()]).unwrap();
        assert_eq!(runtime.schemas_for(Some("a1")).len(), 1);
        assert_eq!(runtime.schemas_for(Some("a1"))[0].name, "echo");
        // Other agent unaffected.
        assert_eq!(runtime.schemas_for(Some("a2")).len(), 2);
        // Disposer lifts the restriction.
        restrict.dispose_async().await;
        assert_eq!(runtime.schemas_for(Some("a1")).len(), 2);
    }

    #[test]
    fn restrict_for_rejects_unknown_tool() {
        use starriver_kernel::tools::ToolRuntime;

        let runtime = InMemoryToolRuntime::new(EventBus::new(), Context::new());
        let _h1 = runtime.register(echo_definition("echo")).unwrap();
        let result = runtime.restrict_for("a1", vec!["nope".into()]);
        match result {
            Err(starriver_kernel::tools::ToolError::NotFound(_)) => {}
            Ok(_) => panic!("unknown tool must reject"),
            Err(other) => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn restrict_for_rejects_empty_allow() {
        use starriver_kernel::tools::ToolRuntime;

        let runtime = InMemoryToolRuntime::new(EventBus::new(), Context::new());
        let _h1 = runtime.register(echo_definition("echo")).unwrap();
 // restrict({}) is a no-op and rejects; an empty whitelist would
        // otherwise hide every tool, which is almost always a config bug.
        let result = runtime.restrict_for("a1", vec![]);
        assert!(
            result.is_err(),
            "empty allow must reject instead of hiding all tools"
        );
    }

    #[tokio::test]
    async fn multiple_restrictions_intersect() {
        use starriver_kernel::tools::ToolRuntime;

        let runtime = InMemoryToolRuntime::new(EventBus::new(), Context::new());
        let _h1 = runtime.register(echo_definition("echo")).unwrap();
        let _h2 = runtime.register(echo_definition("bash")).unwrap();
        let _h3 = runtime.register(echo_definition("grep")).unwrap();
        let _r1 = runtime
            .restrict_for("a1", vec!["echo".into(), "bash".into()])
            .unwrap();
        let _r2 = runtime
            .restrict_for("a1", vec!["echo".into(), "grep".into()])
            .unwrap();
        // Intersection: only "echo" is in both allow sets.
        let schemas = runtime.schemas_for(Some("a1"));
        assert_eq!(schemas.len(), 1);
        assert_eq!(schemas[0].name, "echo");
        // Dropping one restriction widens to the union of the remaining.
        _r1.dispose_async().await;
        let schemas = runtime.schemas_for(Some("a1"));
        assert_eq!(schemas.len(), 2);
        assert!(schemas.iter().any(|s| s.name == "echo"));
        assert!(schemas.iter().any(|s| s.name == "grep"));
    }

    #[test]
    fn schemas_for_unknown_agent_returns_all() {
        use starriver_kernel::tools::ToolRuntime;

        let runtime = InMemoryToolRuntime::new(EventBus::new(), Context::new());
        let _h1 = runtime.register(echo_definition("echo")).unwrap();
        // No restriction registered for "nobody": full visibility.
        assert_eq!(runtime.schemas_for(Some("nobody")).len(), 1);
        assert_eq!(runtime.schemas_for(None).len(), 1);
    }

    #[tokio::test]
    async fn pre_execute_routes_to_agent_scope_when_registered() {
        use starriver_kernel::agent::AgentScopeRegistry;

        let bus = EventBus::new();
        let runtime_ctx = Context::new().with_event_bus(&bus);
        let scopes = Arc::new(AgentScopeRegistry::new());
        let runtime =
            InMemoryToolRuntime::with_scopes(bus.clone(), runtime_ctx.clone(), scopes.clone());
        let _handle = runtime.register(echo_definition("echo")).unwrap();

        // Owner-scoped listeners: an agent-scoped execution must reach the
        // agent ctx's listener and skip the runtime ctx's.
        let agent_ctx = Context::new().with_event_bus(&bus).tagged(Arc::from("a1"));
        scopes.register("a1".into(), agent_ctx.clone());
        let agent_hits = Arc::new(AtomicU64::new(0));
        let runtime_hits = Arc::new(AtomicU64::new(0));
        let _agent_listener = agent_ctx.on_waterfall::<ToolsPreExecute, _>({
            let flag = Arc::clone(&agent_hits);
            move |_, _next| {
                let flag = Arc::clone(&flag);
                Box::pin(async move {
                    flag.fetch_add(1, Ordering::SeqCst);
                    Ok(PreToolDecision::Allow)
                })
            }
        });
        let _runtime_listener = runtime_ctx.on_waterfall::<ToolsPreExecute, _>({
            let flag = Arc::clone(&runtime_hits);
            move |_, _next| {
                let flag = Arc::clone(&flag);
                Box::pin(async move {
                    flag.fetch_add(1, Ordering::SeqCst);
                    Ok(PreToolDecision::Allow)
                })
            }
        });
        let mut input = execution("echo");
        input.agent_id = Some("a1".into());
        let result = runtime.execute(input).await.unwrap();
        assert!(!result.is_error());
        assert_eq!(
            agent_hits.load(Ordering::SeqCst),
            1,
            "agent-scoped execution must reach the agent ctx listener"
        );
        assert_eq!(
            runtime_hits.load(Ordering::SeqCst),
            0,
            "agent-scoped execution must not reach the runtime ctx listener"
        );
    }

    #[tokio::test]
    async fn pre_execute_uses_runtime_scope_without_agent() {
        use starriver_kernel::agent::AgentScopeRegistry;

        let bus = EventBus::new();
        let runtime_ctx = Context::new().with_event_bus(&bus);
        let scopes = Arc::new(AgentScopeRegistry::new());
        let runtime =
            InMemoryToolRuntime::with_scopes(bus.clone(), runtime_ctx.clone(), scopes.clone());
        let _handle = runtime.register(echo_definition("echo")).unwrap();
        let runtime_hits = Arc::new(AtomicU64::new(0));
        let _runtime_listener = runtime_ctx.on_waterfall::<ToolsPreExecute, _>({
            let flag = Arc::clone(&runtime_hits);
            move |_, _next| {
                let flag = Arc::clone(&flag);
                Box::pin(async move {
                    flag.fetch_add(1, Ordering::SeqCst);
                    Ok(PreToolDecision::Allow)
                })
            }
        });
        // No agent_id: falls back to the runtime ctx.
        let result = runtime.execute(execution("echo")).await.unwrap();
        assert!(!result.is_error());
        assert_eq!(
            runtime_hits.load(Ordering::SeqCst),
            1,
            "no agent_id must route to the runtime ctx"
        );
    }

    #[test]
    fn presentation_meta_top_level_only_and_soft_lookup() {
        let ctx = Context::new();
        let runtime = InMemoryToolRuntime::new(EventBus::new(), ctx);
        runtime.register(echo_definition("meta_tool")).unwrap();
        runtime.register_presentation_meta(
            "meta_tool",
            std::sync::Arc::new(|args, _value| {
                let args = starriver_kernel::json::JsonValue::from_value(args).ok()?;
                let q = args.get("q")?.as_str()?.to_string();
                Some(serde_json::json!({ "kind": "echo", "q": q }))
            }),
        );
        let input = |root: Option<starriver_kernel::llm::CallId>| ToolExecutionInput {
            call_id: starriver_kernel::llm::CallId("c1".into()),
            root_call_id: root,
            name: "meta_tool".into(),
            // Production parity: model JSON parses into the lossless tree and
            // rides as a JsonValue payload (tool_calls::parse_arguments).
            arguments: Value::new(starriver_kernel::json::JsonValue::Object(vec![(
                "q".into(),
                starriver_kernel::json::JsonValue::String("hi".into()),
            )])),
            agent_id: None,
            cwd: None,
            sandbox_mode: None,
            parent: None,
            cancellation: None,
        };

        // Top-level call: hook runs.
        let meta = ToolRuntime::presentation_meta(&runtime, &input(None), &Value::new(()));
        assert_eq!(meta.unwrap()["kind"], "echo");

 // Sub-dispatch: gated off (top-level-only rule).
        assert!(ToolRuntime::presentation_meta(
            &runtime,
            &input(Some(starriver_kernel::llm::CallId("root".into()))),
            &Value::new(())
        )
        .is_none());

        // Unregistered tool / unexpected args: soft-None, never panic.
        let mut unknown = input(None);
        unknown.name = "no_such_tool".into();
        assert!(ToolRuntime::presentation_meta(&runtime, &unknown, &Value::new(())).is_none());
        let mut bad = input(None);
        bad.arguments = Value::new(());
        assert!(ToolRuntime::presentation_meta(&runtime, &bad, &Value::new(())).is_none());
    }
}
