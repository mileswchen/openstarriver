// starriver-desktop — napi thin shell
//
// This crate no longer owns execution logic: AgentRuntime wraps starriver-host's HostRuntime,
// all #[napi] methods forward to host, and return types are converted between napi_types
// (#[napi(object)]) and host types. execute events are relayed via host's per-run receiver to a
// ThreadsafeFunction, keeping the Electron main-process IPC contract byte-for-byte identical.
//
// script_runtime (the Bun/TS script bridge) stays in desktop: set_script_executor /
// resolve_script_result are part of the Electron contract; ScriptRuntime is also wrapped as
// host's ScriptExecutor trait injection (reserved on the host side; the current execute path does
// not consume the script tool yet — a legacy state predating the extraction; the shell swap keeps that semantics).

#[macro_use]
extern crate napi_derive;

mod napi_types;
// FPM P4: the protocol catalog and HTTP/SSE server were extracted into a
// shared crate reused by desktop and daemon — one contract and dispatcher.
// External paths are unchanged (`starriver_desktop::protocol_catalog::…`).
pub use starriver_protocol_server::{http_server, protocol_catalog};
mod script_runtime;

use std::sync::atomic::Ordering;
use std::sync::{Arc, RwLock};

use napi::bindgen_prelude::{Error, Result};
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};

use starriver_host::types as host_types;
use starriver_host::ScriptExecutor;

use crate::script_runtime::ScriptRuntime;

fn to_napi_err(e: anyhow::Error) -> Error {
    Error::from_reason(format!("{e}"))
}

// -- host types → napi_types conversion --
//
// host types and desktop napi_types have one-to-one fields (the host side has no #[napi] annotations).
// Pure scalar structs use a macro-generated From; the four structs with convertible Vec/Option are handwritten.

macro_rules! impl_from_host {
    ($t:ident { $($field:ident),+ $(,)? }) => {
        impl From<starriver_host::types::$t> for napi_types::$t {
            fn from(v: starriver_host::types::$t) -> Self {
                Self { $($field: v.$field),+ }
            }
        }
    };
}

impl_from_host!(ToolInfo {
    name,
    description,
    requires_approval
});
impl_from_host!(AgentInfo {
    name,
    description,
    version
});
impl_from_host!(SkillInfo {
    name,
    description,
    has_command,
    command,
    version,
    always_on,
    has_keyword,
    has_suffix,
    has_hooks,
    enabled,
    body,
});
impl_from_host!(SkillUsageRecord {
    session_id,
    project_id,
    skill,
    version,
    mode,
    ts
});
impl_from_host!(CompactResult { before, after });
impl From<starriver_host::types::PluginEntry> for napi_types::PluginEntry {
    fn from(v: starriver_host::types::PluginEntry) -> Self {
        Self {
            name: v.name,
            description: v.description,
            version: v.version,
            skills: v.skills,
            ui: v.ui.map(|ui| {
                serde_json::json!({
                    "entry": ui.entry,
                    "tabs": ui.tabs.iter().map(|t| serde_json::json!({ "id": t.id, "title": t.title })).collect::<Vec<_>>(),
                    "permissions": ui.permissions,
                    "theme": ui.theme,
                })
                .to_string()
            }),
            source_url: v.source_url,
            installed_at: v.installed_at,
        }
    }
}
impl_from_host!(MarketEntry {
    name,
    description,
    url,
    entry_type,
    category,
    version,
});
impl_from_host!(ModelProvider {
    id,
    name,
    provider_type,
    base_url,
    models,
    requires_api_key,
    enabled,
    api_format,
    reasoning_effort,
});
impl_from_host!(ModelVariant { id, name });
impl_from_host!(BoundModelInfo { id, name, enabled });
impl_from_host!(WorktreeStatus {
    is_repo,
    branch,
    is_worktree,
    worktree_path,
    dirty,
});
impl_from_host!(WorkspaceInfo {
    id,
    path,
    title,
    created_at,
    session_ids
});
impl_from_host!(SessionInfo {
    id,
    title,
    workspace_id,
    created_at,
    message_count,
    parent_id,
    fork_point,
    agent,
    origin,
});
// ChatMessage converts manually: ts needs a u64 → f64 narrowing for JS.
impl From<starriver_host::types::ChatMessage> for napi_types::ChatMessage {
    fn from(v: starriver_host::types::ChatMessage) -> Self {
        Self {
            role: v.role,
            content: v.content,
            ts: v.ts.map(|t| t as f64),
            usage: v.usage.map(|u| napi_types::UsageTotals {
                input_tokens: u.input_tokens as f64,
                output_tokens: u.output_tokens as f64,
            }),
            duration_ms: v.duration_ms.map(|d| d as f64),
            done_ts: v.done_ts.map(|t| t as f64),
            file_changes: v.file_changes.map(|files| {
                files
                    .into_iter()
                    .map(|f| napi_types::FileChange {
                        path: f.path,
                        add: f.add as f64,
                        del: f.del as f64,
                        diff: f.diff,
                    })
                    .collect()
            }),
        }
    }
}

impl From<host_types::SessionMessage> for napi_types::SessionMessage {
    fn from(v: host_types::SessionMessage) -> Self {
        // Convert id to f64: avoid napi mapping i64 to BigInt (same precedent as MemoryEntry.created).
        Self {
            id: v.id as f64,
            role: v.role,
            content: v.content,
        }
    }
}
impl_from_host!(MemoryEntry {
    name,
    description,
    body,
    source,
    source_agent,
    project,
    created,
    updated,
});
impl_from_host!(McpOAuthInfo {
    authorization_url,
    token_url,
    client_id,
    scopes,
    redirect_uri,
    has_secret,
});
impl_from_host!(McpTestResult {
    ok,
    tool_count,
    tools,
    error
});
impl_from_host!(McpToolTestResult { ok, output, error });
impl_from_host!(ScheduledTaskInfo {
    name,
    prompt,
    model,
    workspace,
    enabled,
    mode_type,
    period,
    time,
    day_of_week,
    day_of_month,
    month,
    hours,
    days_of_week,
    datetime,
    start_date,
    end_date,
    skills,
    description,
    last_run,
    next_run,
    last_status,
});
impl_from_host!(TaskRunRecord {
    timestamp,
    status,
    summary,
    session_id
});
impl_from_host!(TaskRunRecordWithName {
    task_name,
    timestamp,
    status,
    summary,
    session_id,
});
impl_from_host!(SessionTraceEntry {
    kind,
    text,
    tool_name,
    tool_input,
    tool_output,
    tool_ok,
    seq,
    time_ms,
    turn,
    step,
    input_tokens,
    output_tokens,
    provider,
    model,
    duration_ms,
    ttft_ms,
});

impl From<host_types::ModelEntry> for napi_types::ModelEntry {
    fn from(v: host_types::ModelEntry) -> Self {
        Self {
            id: v.id,
            name: v.name,
            model_type: v.model_type,
            provider_type: v.provider_type,
            base_url: v.base_url,
            api_key: v.api_key,
            model_path: v.model_path,
            models: v.models.into_iter().map(Into::into).collect(),
            default_model: v.default_model,
            concurrency: v.concurrency,
            max_tokens: v.max_tokens,
            context_window: v.context_window,
            temperature: v.temperature,
            api_format: v.api_format,
            reasoning_effort: v.reasoning_effort,
            model_label: v.model_label,
            supports_vision: v.supports_vision,
            vision_supported: v.vision_supported,
        }
    }
}

impl From<host_types::AgentModelBinding> for napi_types::AgentModelBinding {
    fn from(v: host_types::AgentModelBinding) -> Self {
        Self {
            agent_name: v.agent_name,
            active_model: v.active_model,
            bound_models: v.bound_models.into_iter().map(Into::into).collect(),
            reasoning_effort: v.reasoning_effort,
        }
    }
}

impl From<host_types::McpServerInfo> for napi_types::McpServerInfo {
    fn from(v: host_types::McpServerInfo) -> Self {
        Self {
            name: v.name,
            transport: v.transport,
            command: v.command,
            args: v.args,
            env: v.env,
            url: v.url,
            oauth_connected: v.oauth_connected,
            oauth: v.oauth.map(Into::into),
            tool_count: v.tool_count,
            status: v.status,
        }
    }
}

impl From<host_types::SessionTrace> for napi_types::SessionTrace {
    fn from(v: host_types::SessionTrace) -> Self {
        Self {
            entries: v.entries.into_iter().map(Into::into).collect(),
        }
    }
}

// -- ScriptRuntime → host ScriptExecutor bridge --

/// Wrap desktop's ScriptRuntime (bidirectional TS communication via ThreadsafeFunction) as
/// host's ScriptExecutor. host does not consume the script tool today; this bridge ensures desktop
/// injects the Bun/TS implementation when wired later (TUI injects an error stub).
struct ScriptRuntimeBridge {
    rt: Arc<RwLock<Option<Arc<ScriptRuntime>>>>,
}

impl ScriptExecutor for ScriptRuntimeBridge {
    fn execute(
        &self,
        id: String,
        path: String,
        params: serde_json::Value,
        resolve: Box<dyn Fn(serde_json::Value) + Send + Sync>,
    ) {
        let _ = id;
        let Some(rt) = self.rt.read().unwrap().clone() else {
            resolve(serde_json::json!({ "error": "script executor not registered" }));
            return;
        };
        std::thread::spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(r) => r,
                Err(_) => {
                    resolve(serde_json::json!({ "error": "tokio runtime unavailable" }));
                    return;
                }
            };
            match runtime.block_on(rt.call_script(&path, &params)) {
                Ok(s) => {
                    let v =
                        serde_json::from_str(&s).unwrap_or_else(|_| serde_json::Value::String(s));
                    resolve(v);
                }
                Err(e) => resolve(serde_json::json!({ "error": e.to_string() })),
            }
        });
    }
}

// -- napi thin shell --

#[napi]
pub struct AgentRuntime {
    host: std::sync::Arc<starriver_host::HostRuntime>,
    protocol: starriver_protocol::MethodRegistry<starriver_host::HostRuntime>,
    http_server: std::sync::Mutex<Option<http_server::HttpServer>>,
    script_runtime: Arc<RwLock<Option<Arc<ScriptRuntime>>>>,
    /// host's tokio handle: execute's per-run receiver relay task is spawned here.
    runtime_handle: tokio::runtime::Handle,
}

#[napi]
impl AgentRuntime {
    #[napi(constructor)]
    pub fn new(config_path: Option<String>) -> Result<Self> {
        let host = std::sync::Arc::new(
            starriver_host::HostRuntime::new(config_path)
                .map_err(|e| Error::from_reason(format!("{e}")))?,
        );
        // BUG-5: the desktop shell is a long-lived host — its due scheduled
        // tasks auto-fire on the background tick.
        host.start_scheduler_tick();
        let runtime_handle = host.runtime_handle();
        let script_runtime = Arc::new(RwLock::new(None));
        host.set_script_executor(Arc::new(ScriptRuntimeBridge {
            rt: script_runtime.clone(),
        }));
        Ok(Self {
            host,
            protocol: protocol_catalog::build(),
            http_server: std::sync::Mutex::new(None),
            script_runtime,
            runtime_handle,
        })
    }

    /// Generic protocol entry (FPM P1): `requestJson` is the full request
    /// envelope; streaming-method events are delivered via an optional
    /// callback (each event is an `EventEnvelope` JSON).
    #[napi(
        ts_args_type = "requestJson: string, callback?: (err: null | Error, event: any) => void"
    )]
    pub fn invoke(
        &self,
        request_json: String,
        callback: Option<ThreadsafeFunction<serde_json::Value>>,
    ) -> Result<String> {
        use starriver_protocol::{EventEnvelope, Request, Response};
        let request: Request = serde_json::from_str(&request_json)
            .map_err(|e| Error::from_reason(format!("bad protocol request: {e}")))?;
        // Embedded-mode bridge: the desktop UI talks to the runtime in-process,
        // so its envelopes would otherwise reach ONLY the renderer. Tee them
        // into the embedded HTTP server's SSE/WS bus too — the paired phone
        // subscribes there and its real-time sync starves without it.
        let http_emit: Option<Arc<dyn Fn(EventEnvelope) + Send + Sync>> = self
            .http_server
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|server| server.emit_handle());
        let js_emit = callback.map(|cb| -> Arc<dyn Fn(EventEnvelope) + Send + Sync> {
            // WI-165: a dead JS-side forwarder used to drop every event
            // silently (`let _ =` swallowed the call error) — the UI froze
            // while the backend kept running. Surface the failure (once per
            // burst, into host.log) so the break is diagnosable live.
            let failures = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let forwarded = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            Arc::new(move |envelope: EventEnvelope| {
                let event = serde_json::to_value(envelope)
                    .unwrap_or_else(|_| serde_json::json!({ "type": "event" }));
                let n = forwarded.fetch_add(1, Ordering::Relaxed) + 1;
                if cb.call(Ok(event), ThreadsafeFunctionCallMode::NonBlocking) != napi::Status::Ok {
                    let burst = failures.fetch_add(1, Ordering::Relaxed) + 1;
                    if burst == 1 || burst.is_multiple_of(50) {
                        tracing::warn!(
                            forwarded = n,
                            failures = burst,
                            "protocol event forward to JS failed; UI is not receiving events"
                        );
                    }
                }
            })
        });
        let emit = match (js_emit, http_emit) {
            (Some(js), Some(http)) => Some(Arc::new(move |envelope: EventEnvelope| {
                http(envelope.clone());
                js(envelope);
            }) as Arc<dyn Fn(EventEnvelope) + Send + Sync>),
            (only_js, None) => only_js,
            (None, only_http) => only_http,
        };
        let response = self
            .protocol
            .dispatch(self.host.as_ref(), &request, None, emit)
            .unwrap_or_else(|error| Response::err(&request, error));
        serde_json::to_string(&response)
            .map_err(|e| Error::from_reason(format!("encode protocol response: {e}")))
    }

    /// P4a: start the embedded HTTP/SSE protocol server, returning
    /// `{port, token}`. Binds all interfaces so the iPhone companion can pair
    /// over LAN/Tailscale (loopback clients keep working; every protocol
    /// request still requires the bearer token). Browsers and third-party
    /// frontends connect to the same protocol via `HttpSseTransport`.
    ///
    /// 配对跨重启：`{port, token}` 持久化在 `~/.starriver/http_server.json`
    /// 并在下次启动复用——否则每次重启桌面都随机换端口+Token，手机配对全部
    /// 失效、必须重新扫码。端口被占时回退随机端口并更新持久化状态。
    #[napi]
    pub fn start_http_server(&self) -> Result<String> {
        let dispatch = protocol_catalog::dispatcher(self.protocol.clone(), self.host.clone());
        let runtime = self.runtime_handle.clone();

        let state_path = std::env::var("HOME")
            .map(|h| std::path::PathBuf::from(h).join(".starriver/http_server.json"))
            .unwrap_or_else(|_| std::path::PathBuf::from(".starriver/http_server.json"));
        let saved: Option<(u16, String)> = std::fs::read_to_string(&state_path)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| {
                let port = v.get("port").and_then(serde_json::Value::as_u64)? as u16;
                let token = v.get("token").and_then(serde_json::Value::as_str)?.to_string();
                Some((port, token))
            });
        let (bind, token) = match &saved {
            Some((port, tok)) => (
                Some(format!("0.0.0.0:{port}")),
                Some(tok.clone()),
            ),
            None => (Some("0.0.0.0:0".to_string()), None),
        };

        let spawned = runtime.block_on(http_server::spawn_with(
            dispatch.clone(),
            runtime.clone(),
            http_server::SpawnOptions {
                bind,
                token,
                ..http_server::SpawnOptions::default()
            },
        ));
        let server = match spawned {
            Ok(server) => server,
            // 持久化端口被占（异常残留/并发占用）→ 回退随机端口+新 Token。
            Err(_) if saved.is_some() => runtime
                .block_on(http_server::spawn_with(
                    dispatch,
                    runtime.clone(),
                    http_server::SpawnOptions::default(),
                ))
                .map_err(|e| Error::from_reason(format!("start http server: {e}")))?,
            Err(e) => return Err(Error::from_reason(format!("start http server: {e}"))),
        };
        let info =
            serde_json::json!({ "port": server.port(), "token": server.token() }).to_string();
        if let Some(dir) = state_path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(state) = serde_json::to_string(&serde_json::json!({
            "port": server.port(),
            "token": server.token(),
        })) {
            let _ = std::fs::write(&state_path, state);
        }
        *self.http_server.lock().unwrap() = Some(server);
        Ok(info)
    }

    #[napi]
    pub fn stop_http_server(&self) -> Result<()> {
        if let Some(server) = self.http_server.lock().unwrap().take() {
            server.shutdown();
        }
        Ok(())
    }

    /// Current HTTP server info (returns None if not started).
    #[napi]
    pub fn http_server_info(&self) -> Option<String> {
        self.http_server.lock().unwrap().as_ref().map(|server| {
            serde_json::json!({ "port": server.port(), "token": server.token() }).to_string()
        })
    }

    /// Execute a task and relay per-run events to Electron via ThreadsafeFunction.
    /// Signature is identical to before the extraction: `(task, sessionId, callback)`.
    #[napi(
        ts_args_type = "task: string, sessionId: string | undefined | null, callback: (err: null | Error, event: any) => void, executionMode?: string | undefined | null, agent?: string | undefined | null, isolation?: boolean | undefined | null, agentPreset?: string | undefined | null, permissionPreset?: string | undefined | null"
    )]
    pub fn execute(
        &self,
        task: String,
        session_id: Option<String>,
        callback: ThreadsafeFunction<serde_json::Value>,
        execution_mode: Option<String>,
        agent: Option<String>,
        isolation: Option<bool>,
        agent_preset: Option<String>,
        permission_preset: Option<String>,
    ) -> Result<()> {
        let mut rx = self
            .host
            .execute(
                task,
                session_id,
                execution_mode,
                agent,
                isolation,
                agent_preset,
                permission_preset,
            )
            .map_err(|e| Error::from_reason(format!("{e}")))?;
        // Embedded-mode bridge: NAPI execute runs in-process, its events reach
        // only the Electron renderer. Tee each host event into the embedded
        // HTTP server's SSE/WS bus (same envelope shape the api-macro's
        // streaming dispatch produces) so remote clients (paired phone) see
        // desktop-originated turns in real time.
        let http_emit: Option<Arc<dyn Fn(starriver_protocol::EventEnvelope) + Send + Sync>> =
            self.http_server
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .map(|server| server.emit_handle());
        let sub = format!("napi-run-{}", uuid::Uuid::new_v4().simple());
        let event_seq = Arc::new(std::sync::atomic::AtomicU64::new(0));
        self.runtime_handle.spawn(async move {
            // WI-165: closing the channel used to end the relay silently —
            // the renderer then never learns the run is over. Log the close
            // (event count) so a wedged stream is visible in host.log.
            let mut forwarded: u64 = 0;
            while let Ok(ev) = rx.recv().await {
                if let Some(emit) = &http_emit {
                    let event_type = ev
                        .get("type")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("event")
                        .to_string();
                    let seq = event_seq.fetch_add(1, Ordering::Relaxed) + 1;
                    emit(starriver_protocol::EventEnvelope::new(
                        sub.clone(),
                        seq,
                        event_type,
                        ev.clone(),
                    ));
                }
                forwarded += 1;
                callback.call(Ok(ev), ThreadsafeFunctionCallMode::NonBlocking);
            }
            tracing::warn!(forwarded, "session event relay channel closed");
        });
        Ok(())
    }

    // ── Tools / Providers ──

    #[napi]
    pub fn list_tools(&self) -> Vec<napi_types::ToolInfo> {
        self.host.list_tools().into_iter().map(Into::into).collect()
    }

    #[napi]
    pub fn list_model_providers(&self) -> Vec<napi_types::ModelProvider> {
        self.host
            .list_model_providers()
            .into_iter()
            .map(Into::into)
            .collect()
    }

    // ── Approval / Cancel ──

    #[napi]
    pub fn respond_question(&self, question_id: String, answer: String) -> Result<bool> {
        Ok(self.host.respond_question(&question_id, &answer))
    }

    #[napi]
    pub fn approve(&self, session_id: String, allowed: bool, remember: Option<bool>) -> bool {
        self.host.approve(session_id, allowed, remember)
    }

    #[napi]
    pub fn cancel(&self, session_id: String) -> bool {
        self.host.cancel(session_id)
    }

    // -- Script executor (desktop-only bridge, contract preserved) --

    #[napi]
    pub fn set_script_executor(
        &self,
        executor: ThreadsafeFunction<serde_json::Value>,
    ) -> Result<()> {
        *self.script_runtime.write().unwrap() = Some(Arc::new(ScriptRuntime::new(executor)));
        Ok(())
    }

    #[napi]
    pub fn resolve_script_result(&self, id: String, result: String) -> Result<()> {
        if let Some(ref rt) = *self.script_runtime.read().unwrap() {
            rt.resolve_result(
                &id,
                serde_json::from_str(&result)
                    .map_err(|e| Error::from_reason(format!("json: {e}")))?,
            );
        }
        Ok(())
    }

    // ── Agents ──

    #[napi]
    pub fn list_agents(&self) -> Vec<napi_types::AgentInfo> {
        self.host
            .list_agents()
            .into_iter()
            .map(Into::into)
            .collect()
    }

    #[napi]
    pub fn scan_agents(&self) -> Vec<napi_types::AgentInfo> {
        self.host
            .scan_agents()
            .into_iter()
            .map(Into::into)
            .collect()
    }

    #[napi]
    pub fn set_active_workspace_for(
        &self,
        agent_name: String,
        workspace_id: Option<String>,
    ) -> Result<()> {
        self.host
            .set_active_workspace_for(&agent_name, workspace_id)
            .map_err(to_napi_err)
    }

    #[napi]
    pub fn start_code_review(
        &self,
        files_json: String,
        dimensions_json: Option<String>,
    ) -> Result<()> {
        self.host
            .start_code_review(files_json, dimensions_json)
            .map_err(to_napi_err)
    }

    #[napi]
    pub fn read_workspace_instructions(&self, workspace_id: String) -> Option<String> {
        self.host.read_workspace_instructions(workspace_id)
    }

    // ── Skills ──

    #[napi]
    pub fn list_skills(&self) -> Vec<napi_types::SkillInfo> {
        self.host
            .list_skills()
            .into_iter()
            .map(Into::into)
            .collect()
    }

    #[napi]
    pub fn scan_skills(&self) -> Vec<napi_types::SkillInfo> {
        self.host
            .scan_skills()
            .into_iter()
            .map(Into::into)
            .collect()
    }

    #[napi]
    pub fn create_skill(
        &self,
        name: String,
        description: String,
        command: Option<String>,
        body: String,
    ) -> Result<Vec<napi_types::SkillInfo>> {
        self.host
            .create_skill(name, description, command, body)
            .map(|v| v.into_iter().map(Into::into).collect())
            .map_err(to_napi_err)
    }

    #[napi]
    pub fn update_skill(
        &self,
        name: String,
        description: String,
        command: Option<String>,
        body: String,
    ) -> Result<Vec<napi_types::SkillInfo>> {
        self.host
            .update_skill(name, description, command, body)
            .map(|v| v.into_iter().map(Into::into).collect())
            .map_err(to_napi_err)
    }

    #[napi]
    pub fn delete_skill(&self, name: String) -> Result<Vec<napi_types::SkillInfo>> {
        self.host
            .delete_skill(name)
            .map(|v| v.into_iter().map(Into::into).collect())
            .map_err(to_napi_err)
    }

    #[napi]
    pub fn set_skill_always_on(
        &self,
        name: String,
        always_on: bool,
    ) -> Result<Vec<napi_types::SkillInfo>> {
        self.host
            .set_skill_always_on(name, always_on)
            .map(|v| v.into_iter().map(Into::into).collect())
            .map_err(to_napi_err)
    }

    #[napi]
    pub fn compact_session(
        &self,
        session_id: String,
        keep_recent: Option<u32>,
    ) -> Result<napi_types::CompactResult> {
        self.host
            .compact_session(session_id, keep_recent)
            .map(Into::into)
            .map_err(to_napi_err)
    }

    /// /usage: session token estimate and message count.
    #[napi]
    pub fn session_usage(&self, session_id: String) -> Result<napi_types::SessionUsage> {
        let (tokens, count) = self.host.session_usage(&session_id).map_err(to_napi_err)?;
        Ok(napi_types::SessionUsage {
            tokens: tokens as u32,
            count: count as u32,
        })
    }

    /// /diff: git diff text of the current project workspace (empty string when no changes).
    #[napi]
    pub fn workspace_diff(&self, cwd: String) -> Result<String> {
        self.host.workspace_diff(&cwd).map_err(to_napi_err)
    }

    /// /init: generate an AGENTS.md for the current project (does not overwrite if present), returning the path.
    #[napi]
    pub fn init_project_agents(&self, cwd: String) -> Result<String> {
        self.host
            .init_project_agents(&cwd)
            .map(|p| p.to_string_lossy().into_owned())
            .map_err(to_napi_err)
    }

    /// /export: export a session to a Markdown file, returning the path.
    #[napi]
    pub fn export_session(&self, session_id: String, cwd: String) -> Result<String> {
        self.host
            .export_session(&session_id, &cwd)
            .map(|p| p.to_string_lossy().into_owned())
            .map_err(to_napi_err)
    }

    #[napi]
    pub fn list_skill_usage(&self, limit: Option<u32>) -> Vec<napi_types::SkillUsageRecord> {
        self.host
            .list_skill_usage(limit)
            .into_iter()
            .map(Into::into)
            .collect()
    }

    // ── Plugins ──

    #[napi]
    pub fn list_plugins(&self) -> Vec<napi_types::PluginEntry> {
        self.host
            .list_plugins()
            .into_iter()
            .map(Into::into)
            .collect()
    }

    #[napi]
    pub fn install_plugin(&self, source_url: String) -> Result<napi_types::PluginEntry> {
        self.host
            .install_plugin(source_url)
            .map(Into::into)
            .map_err(to_napi_err)
    }

    #[napi]
    pub fn uninstall_plugin(&self, name: String) -> Result<Vec<napi_types::PluginEntry>> {
        self.host
            .uninstall_plugin(name)
            .map(|v| v.into_iter().map(Into::into).collect())
            .map_err(to_napi_err)
    }

    // ── Market / Source ──

    #[napi]
    pub fn market_list(&self) -> Vec<napi_types::MarketEntry> {
        self.host
            .market_list()
            .into_iter()
            .map(Into::into)
            .collect()
    }

    // ── Workspace / Hooks / Instructions / Agent CRUD ──

    #[napi]
    pub fn export_agent(&self, name: String) -> Result<String> {
        self.host.export_agent(name).map_err(to_napi_err)
    }

    #[napi]
    pub fn create_agent(&self, name: String, description: String) -> Result<napi_types::AgentInfo> {
        self.host
            .create_agent(name, description)
            .map(Into::into)
            .map_err(to_napi_err)
    }

    // ── Model Registry / Bindings ──

    #[napi]
    pub fn list_models(&self) -> Vec<napi_types::ModelEntry> {
        self.host
            .list_models()
            .into_iter()
            .map(Into::into)
            .collect()
    }

    #[napi]
    pub fn set_execution_mode(&self, mode: String) -> Result<()> {
        self.host.set_execution_mode(mode).map_err(to_napi_err)
    }

    #[napi]
    pub fn get_execution_mode(&self, session_id: Option<String>) -> Result<String> {
        Ok(self.host.get_execution_mode(session_id))
    }

    #[napi]
    pub fn confirm_plan(&self, session_id: String) -> Result<()> {
        self.host.confirm_plan(session_id).map_err(to_napi_err)
    }

    #[napi]
    pub fn set_permission_preset(&self, session_id: String, preset: String) -> Result<()> {
        self.host
            .set_permission_preset(session_id, preset)
            .map_err(to_napi_err)
    }

    /// Returns `{ sandbox, approval, preset }` (preset is a table name or
    /// null for the derived custom state).
    #[napi]
    pub fn get_permission(&self, session_id: String) -> Result<serde_json::Value> {
        self.host.get_permission(session_id).map_err(to_napi_err)
    }

    #[napi]
    pub fn set_approval_mode(&self, session_id: String, mode: String) -> Result<()> {
        self.host
            .set_approval_mode(session_id, mode)
            .map_err(to_napi_err)
    }

    #[napi]
    #[allow(clippy::too_many_arguments)]
    pub fn add_model(
        &self,
        id: String,
        name: String,
        model_type: String,
        provider_type: String,
        base_url: Option<String>,
        api_key: Option<String>,
        model_path: Option<String>,
        default_model: String,
        variants_json: Option<String>,
        concurrency: Option<u32>,
        max_tokens: Option<u32>,
        context_window: Option<u32>,
        temperature: Option<f64>,
        api_format: Option<String>,
        reasoning_effort: Option<String>,
        model_label: Option<String>,
        supports_vision: Option<bool>,
    ) -> Result<()> {
        self.host
            .add_model(
                id,
                name,
                model_type,
                provider_type,
                base_url,
                api_key,
                model_path,
                default_model,
                variants_json,
                concurrency,
                max_tokens,
                context_window,
                temperature,
                api_format,
                reasoning_effort,
                model_label,
                supports_vision,
            )
            .map_err(to_napi_err)
    }

    #[napi]
    pub fn remove_model(&self, id: String) -> Result<()> {
        self.host.remove_model(id).map_err(to_napi_err)
    }

    #[napi]
    pub fn admit_image_attachments(
        &self,
        images_json: String,
    ) -> Result<Vec<napi_types::AttachmentRef>> {
        self.host
            .admit_image_attachments(images_json)
            .map(|refs| refs.into_iter().map(Into::into).collect())
            .map_err(to_napi_err)
    }

    #[napi]
    pub fn read_image_attachment(&self, id: String) -> Result<napi_types::StoredAttachment> {
        self.host
            .read_image_attachment(id)
            .map(Into::into)
            .map_err(to_napi_err)
    }

    // ── Credential records (describe/set/unset, never the value) ──

    #[napi]
    pub fn describe_credential(&self, key: String) -> Result<napi_types::CredentialRecordInfo> {
        self.host
            .describe_credential(key)
            .map(Into::into)
            .map_err(to_napi_err)
    }

    #[napi]
    pub fn list_credential_records(&self) -> Vec<napi_types::CredentialRecordEntry> {
        self.host
            .list_credential_records()
            .into_iter()
            .map(Into::into)
            .collect()
    }

    #[napi]
    pub fn set_credential_api_key(&self, key: String, value: String) -> Result<String> {
        self.host
            .set_credential_api_key(key, value)
            .map_err(to_napi_err)
    }

    #[napi]
    pub fn unset_credential(&self, key: String) -> Result<()> {
        self.host.unset_credential(key).map_err(to_napi_err)
    }

    #[napi]
    pub fn list_configurable_providers(&self) -> Vec<napi_types::ConfigurableProvider> {
        self.host
            .list_configurable_providers()
            .into_iter()
            .map(Into::into)
            .collect()
    }

    // ── Subagent delegation + unified job panel ──

    #[napi]
    pub fn list_subagent_configs(&self) -> Vec<napi_types::SubagentConfig> {
        self.host
            .list_subagent_configs()
            .into_iter()
            .map(Into::into)
            .collect()
    }

    #[napi]
    pub fn set_subagent_config(
        &self,
        name: String,
        enabled: bool,
        model: Option<String>,
    ) -> Result<()> {
        self.host
            .set_subagent_config(name, enabled, model)
            .map_err(to_napi_err)
    }

    #[napi]
    pub fn list_jobs(&self) -> Vec<napi_types::JobInfo> {
        self.host.list_jobs().into_iter().map(Into::into).collect()
    }

    #[napi]
    pub fn list_agent_model_bindings(&self) -> Vec<napi_types::AgentModelBinding> {
        self.host
            .list_agent_model_bindings()
            .into_iter()
            .map(Into::into)
            .collect()
    }

    #[napi]
    pub fn set_agent_active_model(
        &self,
        agent_name: String,
        provider_id: String,
        variant_id: String,
    ) -> Result<()> {
        self.host
            .set_agent_active_model(agent_name, provider_id, variant_id)
            .map_err(to_napi_err)
    }

    #[napi]
    pub fn get_active_workspace_for(&self, agent_name: String) -> Option<String> {
        self.host.get_active_workspace_for(&agent_name)
    }

    #[napi]
    pub fn set_agent_reasoning_effort(&self, agent_name: String, effort: String) -> Result<()> {
        self.host
            .set_agent_reasoning_effort(agent_name, effort)
            .map_err(to_napi_err)
    }

    // ── MCP ──

    #[napi]
    pub fn list_mcp_servers(&self) -> Vec<napi_types::McpServerInfo> {
        self.host
            .list_mcp_servers()
            .into_iter()
            .map(Into::into)
            .collect()
    }

    #[napi]
    pub fn add_mcp_server(
        &self,
        name: String,
        transport: String,
        command: String,
        args_json: String,
        env_json: String,
        url: Option<String>,
        oauth_json: Option<String>,
    ) -> Result<()> {
        self.host
            .add_mcp_server(
                name, transport, command, args_json, env_json, url, oauth_json,
            )
            .map_err(to_napi_err)
    }

    #[napi]
    pub fn remove_mcp_server(&self, name: String) -> Result<()> {
        self.host.remove_mcp_server(name).map_err(to_napi_err)
    }

    #[napi]
    pub fn refresh_mcp_tools(&self, name: Option<String>) -> Result<()> {
        self.host.refresh_mcp_tools(name).map_err(to_napi_err)
    }

    #[napi]
    pub fn oauth_connect(&self, name: String) -> Result<String> {
        self.host.oauth_connect(name).map_err(to_napi_err)
    }

    #[napi]
    pub fn oauth_disconnect(&self, name: String) -> Result<()> {
        self.host.oauth_disconnect(name).map_err(to_napi_err)
    }

    #[napi]
    pub fn test_mcp_server(&self, name: String) -> napi_types::McpTestResult {
        self.host.test_mcp_server(name).into()
    }

    #[napi]
    pub fn test_mcp_tool(&self, name: String, tool_name: String) -> napi_types::McpToolTestResult {
        self.host.test_mcp_tool(name, tool_name).into()
    }

    // ── Scheduled Tasks ──

    #[napi]
    pub fn list_scheduled_tasks(&self) -> Vec<napi_types::ScheduledTaskInfo> {
        self.host
            .list_scheduled_tasks()
            .into_iter()
            .map(Into::into)
            .collect()
    }

    #[napi]
    pub fn add_scheduled_task(
        &self,
        name: String,
        mode_json: String,
        prompt: String,
        model: Option<String>,
        agent: Option<String>,
        workspace: Option<String>,
        enabled: bool,
        start_date: Option<String>,
        end_date: Option<String>,
        skills: Vec<String>,
    ) -> Result<()> {
        self.host
            .add_scheduled_task(
                name, mode_json, prompt, model, agent, workspace, enabled, start_date, end_date,
                skills,
            )
            .map_err(to_napi_err)
    }

    #[napi]
    pub fn remove_scheduled_task(&self, name: String) -> Result<()> {
        self.host.remove_scheduled_task(name).map_err(to_napi_err)
    }

    #[napi]
    pub fn update_scheduled_task(
        &self,
        name: String,
        mode_json: Option<String>,
        prompt: Option<String>,
        model: Option<String>,
        agent: Option<String>,
        workspace: Option<String>,
        enabled: Option<bool>,
        start_date: Option<String>,
        end_date: Option<String>,
        skills: Option<Vec<String>>,
    ) -> Result<()> {
        self.host
            .update_scheduled_task(
                name, mode_json, prompt, model, agent, workspace, enabled, start_date, end_date,
                skills,
            )
            .map_err(to_napi_err)
    }

    #[napi]
    pub fn run_scheduled_task_now(&self, name: String) -> Result<String> {
        self.host.run_scheduled_task_now(name).map_err(to_napi_err)
    }

    #[napi]
    pub fn get_task_run_history(&self, name: String) -> Vec<napi_types::TaskRunRecord> {
        self.host
            .get_task_run_history(name)
            .into_iter()
            .map(Into::into)
            .collect()
    }

    #[napi]
    pub fn list_all_task_runs(&self) -> Vec<napi_types::TaskRunRecordWithName> {
        self.host
            .list_all_task_runs()
            .into_iter()
            .map(Into::into)
            .collect()
    }

    #[napi]
    pub fn get_session_trace(&self, session_id: String) -> napi_types::SessionTrace {
        self.host.get_session_trace(session_id).into()
    }

    // ── Sessions / Workspaces / Memory ──

    #[napi(ts_args_type = "workspaceId?: string")]
    pub fn list_sessions(&self, workspace_id: Option<String>) -> Vec<napi_types::SessionInfo> {
        self.host
            .list_sessions(workspace_id)
            .into_iter()
            .map(Into::into)
            .collect()
    }

    #[napi]
    pub fn get_session_messages(&self, session_id: String) -> Vec<napi_types::ChatMessage> {
        self.host
            .get_session_messages(session_id)
            .into_iter()
            .map(Into::into)
            .collect()
    }

    /// Backtrack timeline: chronologically ordered model-visible log entries with their seq (including tool-call
    /// conversation blocks), for the Electron backtrack picker to locate "which user message was selected".
    #[napi]
    pub fn get_session_timeline(&self, session_id: String) -> Vec<napi_types::SessionMessage> {
        self.host
            .get_session_timeline(session_id)
            .into_iter()
            .map(Into::into)
            .collect()
    }

    /// Fork a new thread: copy the source session's durable log entries with
    /// `seq < before_msg_id` into a new child (lineage in the header), landing
    /// it in the parent's workspace when grouped. The cutoff currency is the
    /// backtrack timeline seq. Returns the new session's SessionInfo.
    #[napi]
    pub fn fork_session(
        &self,
        source_id: String,
        before_msg_id: f64,
        task: String,
    ) -> Result<napi_types::SessionInfo> {
        self.host
            .fork_session(source_id, before_msg_id as i64, task)
            .map(Into::into)
            .map_err(to_napi_err)
    }

    #[napi]
    pub fn delete_session(&self, session_id: String) -> Result<()> {
        self.host.delete_session(session_id).map_err(to_napi_err)
    }

    /// Name a session: title=Some sets a custom title override (used by the list); None clears the
    /// override back to the task-derived title.
    #[napi]
    pub fn rename_session(&self, session_id: String, title: Option<String>) -> Result<()> {
        self.host
            .rename_session(session_id, title)
            .map_err(to_napi_err)
    }

    /// Soft-archive the session (removed from the default list, restorable via /unarchive; no physical deletion).
    #[napi]
    pub fn archive_session(&self, session_id: String) -> Result<()> {
        self.host.archive_session(session_id).map_err(to_napi_err)
    }

    /// Restore a session from the archive (back to the default list).
    #[napi]
    pub fn unarchive_session(&self, session_id: String) -> Result<()> {
        self.host.unarchive_session(session_id).map_err(to_napi_err)
    }

    /// Archived sessions only (data source for the restore picker).
    #[napi]
    pub fn list_archived_sessions(&self) -> Vec<napi_types::SessionInfo> {
        self.host
            .list_archived_sessions()
            .into_iter()
            .map(Into::into)
            .collect()
    }

    #[napi]
    pub fn get_worktree_status(&self, cwd: String) -> napi_types::WorktreeStatus {
        self.host.get_worktree_status(cwd).into()
    }

    #[napi]
    pub fn list_workspaces(&self) -> Vec<napi_types::WorkspaceInfo> {
        self.host
            .list_workspaces()
            .into_iter()
            .map(Into::into)
            .collect()
    }

    #[napi]
    pub fn create_workspace(&self, path: String) -> Result<napi_types::WorkspaceInfo> {
        self.host
            .create_workspace(path)
            .map(Into::into)
            .map_err(to_napi_err)
    }

    #[napi]
    pub fn rename_workspace(&self, workspace_id: String, title: String) -> Result<()> {
        self.host
            .rename_workspace(workspace_id, title)
            .map_err(to_napi_err)
    }

    #[napi]
    pub fn delete_workspace(&self, workspace_id: String) -> Result<()> {
        self.host
            .delete_workspace(workspace_id)
            .map_err(to_napi_err)
    }

    #[napi(
        ts_args_type = "projectId?: string | null, name: string, description: string, body: string"
    )]
    pub fn upsert_memory(
        &self,
        project_id: Option<String>,
        name: String,
        description: String,
        body: String,
    ) -> Result<napi_types::MemoryEntry> {
        self.host
            .upsert_memory(project_id, name, description, body)
            .map(Into::into)
            .map_err(|e| Error::from_reason(format!("{e}")))
    }

    #[napi(ts_args_type = "projectId?: string | null, query: string")]
    pub fn recall_memories(
        &self,
        project_id: Option<String>,
        query: String,
    ) -> Vec<napi_types::MemoryEntry> {
        self.host
            .recall_memories(project_id, query)
            .into_iter()
            .map(Into::into)
            .collect()
    }

    #[napi(ts_args_type = "projectId?: string")]
    pub fn list_memories(&self, project_id: Option<String>) -> Vec<napi_types::MemoryEntry> {
        self.host
            .list_memories(project_id)
            .into_iter()
            .map(Into::into)
            .collect()
    }

    #[napi]
    pub fn delete_memory(&self, project_id: String, name: String) -> Result<()> {
        self.host
            .delete_memory(project_id, name)
            .map_err(to_napi_err)
    }

    #[napi]
    pub fn clear_memories(&self, project_id: String) -> Result<u32> {
        self.host.clear_memories(project_id).map_err(to_napi_err)
    }

    /// Release the runtime (checkpoint + drop the tokio runtime). Non-napi, for the Rust side.
    pub async fn shutdown(&self) -> Result<()> {
        self.host.shutdown().await.map_err(to_napi_err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Type conversion: fields map one-to-one, and nested Vec/Option convert correctly.
    #[test]
    fn host_to_napi_conversions_map_all_fields() {
        let tool = host_types::ToolInfo {
            name: "bash".into(),
            description: "run a command".into(),
            requires_approval: true,
        };
        let n: napi_types::ToolInfo = tool.into();
        assert_eq!(n.name, "bash");
        assert_eq!(n.description, "run a command");
        assert!(n.requires_approval);

        let trace = host_types::SessionTrace {
            entries: vec![host_types::SessionTraceEntry {
                kind: "tool_use".into(),
                text: String::new(),
                tool_name: Some("bash".into()),
                tool_input: Some(r#"{"command":"ls"}"#.into()),
                tool_output: None,
                tool_ok: None,
                ..Default::default()
            }],
        };
        let n: napi_types::SessionTrace = trace.into();
        assert_eq!(n.entries.len(), 1);
        assert_eq!(n.entries[0].tool_name.as_deref(), Some("bash"));

        let mcp = host_types::McpServerInfo {
            name: "srv".into(),
            transport: "stdio".into(),
            command: "cmd".into(),
            args: vec![],
            env: vec![],
            url: None,
            oauth_connected: false,
            oauth: Some(host_types::McpOAuthInfo {
                authorization_url: "a".into(),
                token_url: "t".into(),
                client_id: "c".into(),
                scopes: vec!["read".into()],
                redirect_uri: "r".into(),
                has_secret: true,
            }),
            tool_count: 3,
            status: "connected".into(),
        };
        let n: napi_types::McpServerInfo = mcp.into();
        assert_eq!(n.tool_count, 3);
        assert_eq!(n.oauth.as_ref().unwrap().has_secret, true);
    }

    #[test]
    fn to_napi_err_maps_reason() {
        let err = to_napi_err(anyhow::anyhow!("boom: disk full"));
        assert_eq!(err.reason, "boom: disk full");
        assert_eq!(err.status, napi::Status::GenericFailure);
    }

    /// Cover representative structs for both handwritten From (nested Vec/Option) and macro-generated From.
    #[test]
    fn conversions_cover_nested_and_optional_structs() {
        let entry = host_types::ModelEntry {
            id: "m1".into(),
            name: "deepseek".into(),
            model_type: "cloud".into(),
            provider_type: "openai-compat".into(),
            base_url: "https://api.deepseek.com".into(),
            api_key: "k".into(),
            model_path: None,
            models: vec![
                host_types::ModelVariant {
                    id: "v1".into(),
                    name: "flash".into(),
                },
                host_types::ModelVariant {
                    id: "v2".into(),
                    name: "pro".into(),
                },
            ],
            default_model: "flash".into(),
            concurrency: Some(2),
            max_tokens: Some(8192),
            context_window: Some(128_000),
            temperature: Some(0.3),
            api_format: "openai-chat".into(),
            reasoning_effort: Some("high".into()),
            model_label: Some("DS".into()),
            supports_vision: Some(true),
            vision_supported: true,
        };
        let n: napi_types::ModelEntry = entry.into();
        assert_eq!(n.models.len(), 2);
        assert_eq!(n.models[1].name, "pro");
        assert_eq!(n.max_tokens, Some(8192));
        assert_eq!(n.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(n.model_label.as_deref(), Some("DS"));
        assert!(n.vision_supported);

        let binding = host_types::AgentModelBinding {
            agent_name: "Coding Agent".into(),
            active_model: "v1".into(),
            bound_models: vec![host_types::BoundModelInfo {
                id: "b1".into(),
                name: "pro".into(),
                enabled: true,
            }],
            reasoning_effort: Some("high".into()),
        };
        let n: napi_types::AgentModelBinding = binding.into();
        assert_eq!(n.bound_models[0].enabled, true);
        assert_eq!(n.active_model, "v1");
        assert_eq!(n.reasoning_effort.as_deref(), Some("high"));

        let ws = host_types::WorktreeStatus {
            is_repo: true,
            branch: Some("codex/tui".into()),
            is_worktree: true,
            worktree_path: Some("/tmp/wt".into()),
            dirty: true,
        };
        let n: napi_types::WorktreeStatus = ws.into();
        assert_eq!(n.branch.as_deref(), Some("codex/tui"));
        assert!(n.dirty);

        let ws = host_types::WorkspaceInfo {
            id: "/tmp/a".into(),
            path: "/tmp/a".into(),
            title: "alpha".into(),
            created_at: "t".into(),
            session_ids: vec!["s1".into(), "s2".into()],
        };
        let n: napi_types::WorkspaceInfo = ws.into();
        assert_eq!(n.session_ids, vec!["s1".to_string(), "s2".to_string()]);

        let sess = host_types::SessionInfo {
            id: "s1".into(),
            title: "任务".into(),
            workspace_id: Some("/tmp/a".into()),
            created_at: "t".into(),
            message_count: 2,
            parent_id: None,
            fork_point: None,
            agent: None,
            origin: None,
        };
        let n: napi_types::SessionInfo = sess.into();
        assert_eq!(n.workspace_id.as_deref(), Some("/tmp/a"));

        let chat = host_types::ChatMessage {
            role: "user".into(),
            content: "hi".into(),
            ts: Some(1_700_000_000_000),
            usage: Some(host_types::TokenTotals {
                input_tokens: 120,
                output_tokens: 45,
            }),
            duration_ms: Some(83_210),
            done_ts: Some(1_700_000_000_500),
            seq: None,
            file_changes: Some(vec![host_types::FileChange {
                path: "/tmp/a.txt".into(),
                add: 1,
                del: 1,
                diff: "--- /tmp/a.txt\n+++ /tmp/a.txt".into(),
            }]),
        };
        let n: napi_types::ChatMessage = chat.into();
        assert_eq!(n.role, "user");
        assert_eq!(n.ts, Some(1_700_000_000_000.0));
        let usage = n.usage.expect("usage converts");
        assert_eq!(usage.input_tokens, 120.0);
        assert_eq!(usage.output_tokens, 45.0);
        assert_eq!(n.duration_ms, Some(83_210.0));
        assert_eq!(n.done_ts, Some(1_700_000_000_500.0));
        let files = n.file_changes.expect("file changes convert");
        assert_eq!(files.len(), 1);
        assert_eq!((files[0].add, files[0].del), (1.0, 1.0));

        // Backtrack timeline messages: convert id from host i64 to JS-safe f64 (avoid BigInt)
        let tm = host_types::SessionMessage {
            id: 7,
            role: "user".into(),
            content: "第二个问题".into(),
        };
        let n: napi_types::SessionMessage = tm.into();
        assert_eq!(n.id, 7.0);
        assert_eq!(n.role, "user");
        assert_eq!(n.content, "第二个问题");

        let mem = host_types::MemoryEntry {
            name: "n".into(),
            description: "d".into(),
            body: "b".into(),
            source: "s".into(),
            source_agent: "starriver".into(),
            project: "p1".into(),
            created: 1.0,
            updated: 2.0,
        };
        let n: napi_types::MemoryEntry = mem.into();
        assert_eq!(n.project, "p1");

        let skill = host_types::SkillInfo {
            name: "s".into(),
            description: "d".into(),
            has_command: true,
            command: Some("/go".into()),
            version: None,
            always_on: true,
            has_keyword: false,
            has_suffix: false,
            has_hooks: false,
            enabled: true,
            body: "body".into(),
        };
        let n: napi_types::SkillInfo = skill.into();
        assert_eq!(n.command.as_deref(), Some("/go"));
        assert!(n.always_on);

        let task = host_types::ScheduledTaskInfo {
            name: "t".into(),
            prompt: "p".into(),
            model: None,
            agent: None,
            workspace: None,
            enabled: true,
            mode_type: "periodic".into(),
            period: Some("daily".into()),
            time: Some("09:00".into()),
            day_of_week: None,
            day_of_month: None,
            month: None,
            hours: None,
            days_of_week: vec![],
            datetime: None,
            start_date: None,
            end_date: None,
            skills: vec!["s1".into()],
            description: "d".into(),
            last_run: None,
            next_run: Some("2026-08-21".into()),
            last_status: None,
        };
        let n: napi_types::ScheduledTaskInfo = task.into();
        assert_eq!(n.period.as_deref(), Some("daily"));
        assert_eq!(n.next_run.as_deref(), Some("2026-08-21"));

        let mcp_test = host_types::McpTestResult {
            ok: true,
            tool_count: 1,
            tools: vec!["read".into()],
            error: None,
        };
        let n: napi_types::McpTestResult = mcp_test.into();
        assert_eq!(n.tools, vec!["read".to_string()]);

        let tool = host_types::McpToolTestResult {
            ok: true,
            output: Some("o".into()),
            error: None,
        };
        let n: napi_types::McpToolTestResult = tool.into();
        assert_eq!(n.output.as_deref(), Some("o"));

        let run = host_types::TaskRunRecordWithName {
            task_name: "t".into(),
            timestamp: "ts".into(),
            status: "ok".into(),
            summary: "s".into(),
            session_id: Some("s1".into()),
        };
        let n: napi_types::TaskRunRecordWithName = run.into();
        assert_eq!(n.session_id.as_deref(), Some("s1"));
    }

    /// Integration smoke of the napi facade + real host (temporary DB, empty engine, no external services).
    #[test]
    fn napi_facade_runtime_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let workdir = dir.path().join("workdir");
        std::fs::create_dir(&workdir).expect("workdir");
        let config = format!(
            "version: 1\nmode: offline\ninference: {{ engines: [] }}\nagent: {{}}\nserver: {{ http: {{ enabled: false, bind: \"127.0.0.1:0\" }} }}\nsession: {{ db_path: \"{}\" }}\n",
            dir.path()
                .join("t.db")
                .display()
                .to_string()
                .replace('\\', "/")
        );
        let cfg_path = dir.path().join("config.toml");
        std::fs::write(&cfg_path, config).expect("write config");

        let rt = AgentRuntime::new(Some(cfg_path.to_string_lossy().into_owned())).expect("new");

        // agents
        assert!(rt
            .list_agents()
            .iter()
            .any(|a| a.name == starriver_host::CODING_AGENT_NAME));
        // Unknown agents never enter the catalog: execute names one → hard error.
        assert!(rt.list_agents().iter().all(|a| a.name != "Missing"));

 // Workspace CRUD (semantics: existing dir only, title defaults to basename)
        assert!(rt.list_workspaces().is_empty());
        let w = rt
            .create_workspace(workdir.to_string_lossy().into_owned())
            .expect("create");
        assert_eq!(w.title, "workdir");
        assert!(rt
            .list_workspaces()
            .iter()
            .any(|x| x.id == w.id && x.title == "workdir"));
        assert!(rt.create_workspace("/nonexistent-dir-xyz".into()).is_err());
        // Validation errors surface through the napi shell as Err, not swallowed.
        assert!(rt.rename_workspace(w.id.clone(), "   ".into()).is_err());
        assert!(rt.rename_workspace("missing".into(), "x".into()).is_err());
        rt.rename_workspace(w.id.clone(), "renamed".into()).unwrap();
        assert!(rt.list_workspaces().iter().any(|x| x.title == "renamed"));
        rt.set_active_workspace_for(starriver_host::CODING_AGENT_NAME.into(), Some(w.id.clone()))
            .unwrap();
        assert_eq!(
            rt.get_active_workspace_for(starriver_host::CODING_AGENT_NAME.into()),
            Some(w.id.clone())
        );
        rt.delete_workspace(w.id.clone()).unwrap();
        assert!(rt.list_workspaces().is_empty());

        // Session list + no-op semantics with no pending approvals/cancels
        assert!(rt.list_sessions(None).is_empty());
        assert!(!rt.cancel("missing".into()));
        assert!(!rt.approve("missing".into(), true, None));

        rt.set_active_workspace_for(starriver_host::CODING_AGENT_NAME.into(), None)
            .unwrap();

        // Exit does not panic (regression: used to crash when dropped inside its own runtime)
        rt.runtime_handle.block_on(rt.shutdown()).expect("shutdown");
    }

    /// NAPI facade end-to-end backtrack passthrough: seed 3 user/assistant turns on the plugin log →
    /// get_session_timeline lists them with seq ids → fork_session copies the pre-cutoff history into a
    /// child whose lineage is recorded. No model network needed.
    #[test]
    fn napi_facade_fork_backtracks_to_user_message() {
        use starriver_kernel::json::JsonValue;
        use starriver_kernel::llm::{ContentBlock, Message, MessageId, MessageRole, MessageSource};
        use starriver_kernel::session::{SessionEvent, SurfaceOp};

        fn msg_json(role: MessageRole, mid: &str, text: &str) -> JsonValue {
            let source = match role {
                MessageRole::User => MessageSource::User,
                _ => MessageSource::Model {
                    provider: "p".into(),
                    model: "m".into(),
                    replay_state: None,
                },
            };
            Message {
                id: MessageId(mid.into()),
                role,
                content: vec![ContentBlock::Text(text.into())],
                source,
            }
            .to_json()
            .expect("message json")
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let config = format!(
            "version: 1\nmode: offline\ninference: {{ engines: [] }}\nagent: {{}}\nserver: {{ http: {{ enabled: false, bind: \"127.0.0.1:0\" }} }}\nsession: {{ db_path: \"{}\" }}\n",
            dir.path()
                .join("t.db")
                .display()
                .to_string()
                .replace('\\', "/")
        );
        let cfg_path = dir.path().join("config.toml");
        std::fs::write(&cfg_path, config).expect("write config");

        let rt = AgentRuntime::new(Some(cfg_path.to_string_lossy().into_owned())).expect("new");

        // Three user/assistant turns, alternating surface messages seq 0..6.
        let mut events: Vec<SessionEvent> = Vec::new();
        for i in 0..6u64 {
            let (role, text) = if i % 2 == 0 {
                (MessageRole::User, format!("第{}问", i / 2 + 1))
            } else {
                (MessageRole::Assistant, format!("第{}答", i / 2 + 1))
            };
            let data = if role == MessageRole::Assistant {
                JsonValue::Object(vec![(
                    "message".into(),
                    msg_json(role, &format!("m{i}"), &text),
                )])
            } else {
                msg_json(role, &format!("m{i}"), &text)
            };
            events.push(SessionEvent {
                seq: i,
                time: 1_700_000_000_000,
                event_type: if role == MessageRole::User {
                    "user/message".into()
                } else {
                    "assistant/message".into()
                },
                data,
                surface_op: Some(SurfaceOp::Append),
                source_event_seqs: vec![],
            });
        }
        rt.host
            .seed_test_log("s1".into(), None, None, events)
            .expect("seed log");

        // Backtrack timeline: 3 user messages; ids are log seqs (f64-safe).
        // The first user message is seq 0 — valid JS number, just not > 0.
        let tl = rt.get_session_timeline("s1".into());
        assert_eq!(tl.len(), 6);
        let users: Vec<_> = tl.iter().filter(|m| m.role == "user").collect();
        assert_eq!(users.len(), 3);
        assert!(users.iter().all(|m| m.id.is_finite()), "id 应为 JS number");
        assert_eq!(users[0].id, 0.0);

        // Backtrack to the 2nd user message (seq 2) → fork a new thread
        let second = users[1];
        let fork = rt
            .fork_session("s1".into(), second.id, second.content.clone())
            .expect("fork");
        assert_ne!(fork.id, "s1");
        assert_eq!(fork.parent_id.as_deref(), Some("s1"));
        assert_eq!(fork.fork_point, Some(second.id as i64));

        // New thread timeline = pre-truncation [Q1, A1], excluding turns 2/3
        let f_tl = rt.get_session_timeline(fork.id.clone());
        let roles: Vec<&str> = f_tl.iter().map(|m| m.role.as_str()).collect();
        let contents: Vec<&str> = f_tl.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(roles, vec!["user", "assistant"]);
        assert_eq!(contents, vec!["第1问", "第1答"]);

        // The source session is untouched (append-only)
        assert_eq!(rt.get_session_timeline("s1".into()).len(), 6);

        rt.runtime_handle.block_on(rt.shutdown()).expect("shutdown");
    }
}
