#![allow(dead_code, clippy::new_without_default, clippy::collapsible_if)]
//! OpenAI-family adapter plugin (LLP L2/L2b): chat-completions SSE and
//! responses API against any OpenAI-compatible endpoint, emitting kernel
//! `StreamChunk`s directly.

pub mod responses;
pub mod translate;
pub mod wire;

use std::sync::Arc;

use cordis_rslite::{Context, EffectHandle, EventBus, Inject, Plugin};
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use futures::StreamExt;
use starriver_kernel::llm::{
    GenerateOptions, LlmAdapter, LlmError, LlmModelContext, LlmModelInfo, LlmProviderInfo,
    LlmReasoningEffortInfo, LlmReasoningInfo, LlmResolvedModelInfo, LlmRuntime, RetryPolicy,
    StreamChunk,
};
use starriver_llm::InMemoryLlmRuntime;
use tokio_util::sync::CancellationToken;

pub struct OpenAiCompletionsAdapter {
    provider: String,
    profile: Arc<dyn Fn() -> ProviderProfile + Send + Sync>,
    client: reqwest::Client,
    idle_timeout_ms: u64,
    credential_resolver: Option<CredentialResolver>,
}

/// Cordis plugin mounting one or more OpenAI-compatible routes (LLP L2).
///
/// Routes are re-read through the thunk on every activation, so route or
/// credential changes take effect on the next request without touching the
/// running adapter instances (freeze discipline on the registration side).
pub struct OpenAiCompletionsPlugin {
    profiles: Arc<dyn Fn() -> Vec<ProviderProfile> + Send + Sync>,
}

impl OpenAiCompletionsPlugin {
    pub fn new(profiles: Arc<dyn Fn() -> Vec<ProviderProfile> + Send + Sync>) -> Self {
        Self { profiles }
    }
}

impl Plugin for OpenAiCompletionsPlugin {
    fn name(&self) -> &'static str {
        "llm-openai"
    }

    fn inject(&self) -> Inject {
        Inject::new([<InMemoryLlmRuntime as cordis_rslite::TypedService>::NAME])
    }

    fn apply(
        &self,
        ctx: &Context,
        _bus: &EventBus,
    ) -> cordis_rslite::BoxFuture<anyhow::Result<Vec<EffectHandle>>> {
        let profiles = (self.profiles)();
        match ctx.require::<InMemoryLlmRuntime>() {
            Ok(llm) => Box::pin(async move {
                let mut handles = Vec::new();
                for profile in profiles {
                    let provider = profile.provider.clone();
                    let route = Arc::new(move || profile.clone());
                    let adapter = Arc::new(OpenAiCompletionsAdapter::new(provider.clone(), route));
                    let handle = llm.register_adapter(adapter).map_err(|e| {
                        anyhow::anyhow!("llm-openai plugin: register `{provider}`: {e}")
                    })?;
                    handles.push(handle);
                }
                Ok(handles)
            }),
            Err(e) => Box::pin(async move {
                Err(anyhow::anyhow!(
                    "llm-openai plugin: llm service unavailable: {e}"
                ))
            }),
        }
    }
}

#[derive(Debug, Clone)]
pub enum CredentialRef {
    Literal(String),
    Env(String),
    File(String),
}

impl CredentialRef {
    fn resolve(&self) -> Result<String, LlmError> {
        let missing = |source: String| LlmError::Call {
            code: "MISSING_CREDENTIAL".into(),
            message: format!("credential unavailable: {source}"),
            status: None,
            provider_retry_after_ms: None,
            request_id: None,
        };
        match self {
            CredentialRef::Literal(key) => Ok(key.clone()),
            CredentialRef::Env(name) => {
                std::env::var(name).map_err(|_| missing(format!("env `{name}`")))
            }
            CredentialRef::File(path) => std::fs::read_to_string(path)
                .map(|key| key.trim().to_string())
                .map_err(|_| missing(format!("file `{path}`"))),
        }
    }
}

/// Provider profile for one OpenAI-compatible route, mirroring provider
/// profile: endpoint, credential reference, model directory, retry policy,
/// and adapter-level request defaults (thinking / reasoning effort).
#[derive(Debug, Clone)]
pub struct ProviderProfile {
    /// Provider id served by this route; also the registry key.
    pub provider: String,
    /// Wire protocol: "openai-completions" (default) | "openai-responses".
    pub api: String,
    pub base_url: String,
    pub credential: Option<CredentialRef>,
    pub models: Vec<ModelEntry>,
    pub retry_policy: Option<RetryPolicy>,
    pub idle_timeout_ms: u64,
    pub max_tokens: Option<u64>,
 /// Adapter-level thinking default: "enabled" | "disabled" (
    /// `RequestDefaults.thinking`). None leaves the provider default.
    pub thinking: Option<String>,
    /// Adapter-level reasoning effort default: "off" | "low" | "high" | "max"
 /// (`RequestDefaults.reasoningEffort`). "off" maps to thinking
    /// disabled; the others enable thinking at that effort.
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ModelEntry {
    pub id: String,
    pub name: Option<String>,
    pub context_window: Option<u64>,
    pub max_tokens: Option<u64>,
}

pub type CredentialResolver =
    Arc<dyn Fn(&ProviderProfile) -> Result<String, LlmError> + Send + Sync>;

impl OpenAiCompletionsAdapter {
    pub fn new(provider: String, profile: Arc<dyn Fn() -> ProviderProfile + Send + Sync>) -> Self {
        Self {
            provider,
            profile,
            client: reqwest::Client::new(),
            idle_timeout_ms: 300_000,
            credential_resolver: None,
        }
    }

    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }

    pub fn with_idle_timeout(mut self, ms: u64) -> Self {
        self.idle_timeout_ms = ms;
        self
    }

    /// Attach a host-provided credential resolver. When present it owns
 /// credential resolution (: the plugin never touches literal keys);
    /// without it the profile's [`CredentialRef`] is resolved by the adapter
    /// itself (file/env/literal), which is what edge/mobile and the contract
    /// suite use.
    pub fn with_credential_resolver(mut self, resolver: CredentialResolver) -> Self {
        self.credential_resolver = Some(resolver);
        self
    }

    fn resolve_credential(&self, profile: &ProviderProfile) -> Result<String, LlmError> {
        if let Some(resolver) = self.credential_resolver.as_ref() {
            return resolver(profile);
        }
        match profile.credential.as_ref() {
            Some(cred) => match cred.resolve() {
                Ok(key) if !key.is_empty() => Ok(key),
                Ok(_) => Err(missing_cred("route (empty key)")),
                Err(e) => Err(e),
            },
            None => Err(missing_cred("route (no credential)")),
        }
    }

    /// Query the endpoint's `/models` endpoint for available model entries
    /// (LM Studio compat: local servers expose loaded models here).
    pub async fn detect_models(&self) -> Result<Vec<ModelEntry>, LlmError> {
        let profile = (self.profile)();
        let url = format!("{}/models", profile.base_url.trim_end_matches('/'));
        let mut request = self.client.get(&url);
        // Discovery must not fail on missing credentials (local servers
        // commonly need none); a resolvable key just adds auth.
        if let Ok(key) = self.resolve_credential(&profile) {
            if !key.is_empty() {
                request = request.bearer_auth(&key);
            }
        }
        let response = request.send().await.map_err(|e| LlmError::Call {
            code: "MODEL_DISCOVERY".into(),
            message: format!("models endpoint failed: {e}"),
            status: None,
            provider_retry_after_ms: None,
            request_id: None,
        })?;
        let status = response.status().as_u16();
        if status >= 400 {
            return Err(LlmError::Call {
                code: "MODEL_DISCOVERY".into(),
                message: format!("models endpoint returned HTTP {status}"),
                status: Some(status),
                provider_retry_after_ms: None,
                request_id: None,
            });
        }
        let json: serde_json::Value = response.json().await.map_err(|e| LlmError::Call {
            code: "MODEL_DISCOVERY".into(),
            message: format!("models response parse failed: {e}"),
            status: None,
            provider_retry_after_ms: None,
            request_id: None,
        })?;
        let mut out = Vec::new();
        if let Some(data) = json.get("data").and_then(|d| d.as_array()) {
            for entry in data {
                let id = entry
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                if id.is_empty() {
                    continue;
                }
                let context_window = entry.get("context_window").and_then(|v| v.as_u64());
                let max_tokens = entry.get("max_tokens").and_then(|v| v.as_u64());
                out.push(ModelEntry {
                    id: id.clone(),
                    name: Some(id),
                    context_window,
                    max_tokens,
                });
            }
        }
        Ok(out)
    }
}

/// Convenience: build a [`ProviderProfile`] for an LM Studio local endpoint.
/// No credential is needed for a local server; model discovery is available
/// via `OpenAiCompletionsAdapter::detect_models`.
pub fn lmstudio_profile(provider: &str, base_url: &str, model: &str) -> ProviderProfile {
    ProviderProfile {
        provider: provider.to_string(),
        api: "openai-completions".into(),
        base_url: base_url.trim_end_matches('/').to_string(),
        credential: None,
        models: vec![ModelEntry {
            id: model.to_string(),
            name: Some(model.to_string()),
            context_window: None,
            max_tokens: None,
        }],
        retry_policy: None,
        idle_timeout_ms: 300_000,
        max_tokens: None,
        thinking: None,
        reasoning_effort: None,
    }
}

#[async_trait::async_trait]
impl LlmAdapter for OpenAiCompletionsAdapter {
    fn providers(&self) -> &[String] {
        std::slice::from_ref(&self.provider)
    }
    fn provider_info(&self, _provider: &str) -> LlmProviderInfo {
        LlmProviderInfo {
            id: self.provider.clone(),
            name: "OpenAI-compatible".into(),
        }
    }
    fn provider_retry_policy(&self, _provider: &str) -> Option<RetryPolicy> {
        (self.profile)().retry_policy
    }

    /// Advertise the static model directory declared in the profile (LM
    /// Studio pre-fills it via `detect_models` before registration).
    fn list_models(
        &self,
        _provider: &str,
    ) -> BoxFuture<'static, Result<Vec<LlmModelInfo>, LlmError>> {
        let provider = self.provider.clone();
        let models: Vec<LlmModelInfo> = (self.profile)()
            .models
            .into_iter()
            .map(|entry| LlmModelInfo {
                provider: provider.clone(),
                id: entry.id,
                name: entry.name.unwrap_or_default(),
                description: None,
                input_modalities: None,
            })
            .collect();
        Box::pin(async move { Ok(models) })
    }

    /// Resolve exact-model metadata from the profile directory, with a
    /// generic fallback for undeclared model ids. Reasoning capability is
 /// declared when the profile carries a reasoning-effort default (
    /// `resolveModel`: directory + fallback + reasoning efforts).
    fn resolve_model(
        &self,
        _provider: &str,
        model: &str,
        _cancellation: Option<CancellationToken>,
    ) -> BoxFuture<'static, Result<LlmResolvedModelInfo, LlmError>> {
        let provider = self.provider.clone();
        let profile = (self.profile)();
        let entry = profile.models.iter().find(|entry| entry.id == model);
        let reasoning = profile
            .reasoning_effort
            .as_ref()
            .map(|default_effort| LlmReasoningInfo {
                efforts: ["off", "low", "high", "max"]
                    .into_iter()
                    .map(|id| LlmReasoningEffortInfo {
                        id: id.into(),
                        name: id.into(),
                        description: None,
                    })
                    .collect(),
                default_effort: (default_effort != "off").then(|| default_effort.clone()),
            });
        let info = LlmResolvedModelInfo {
            provider,
            id: model.to_string(),
            name: entry
                .and_then(|entry| entry.name.clone())
                .unwrap_or_else(|| model.to_string()),
            description: None,
            input_modalities: None,
            context: entry
                .and_then(|entry| entry.context_window)
                .map(|context_window| LlmModelContext { context_window }),
            default_max_tokens: entry
                .and_then(|entry| entry.max_tokens)
                .or(profile.max_tokens),
            reasoning,
        };
        Box::pin(async move { Ok(info) })
    }

    fn stream(
        &self,
        options: GenerateOptions,
    ) -> BoxStream<'static, Result<StreamChunk, LlmError>> {
        let profile = (self.profile)();
        let client = self.client.clone();
        let idle = std::time::Duration::from_millis(self.idle_timeout_ms);
        let cancel = options.cancellation.clone();
        let credential = self.resolve_credential(&profile);

        Box::pin(async_stream::stream! {
            let api_key = match credential {
                Ok(key) => key,
                Err(e) => { yield Err(e); return; }
            };
            let is_responses = profile.api == "openai-responses";
            let body = match if is_responses {
                responses::serialize_request(&options, &profile)
            } else {
                wire::serialize_request(&options, &profile)
            } {
                Ok(body) => body,
                Err(e) => { yield Err(e); return; }
            };
            let base = profile.base_url.trim_end_matches('/');
            let url = if is_responses {
                format!("{base}/responses")
            } else {
                format!("{base}/chat/completions")
            };
            let response = match client.post(&url).bearer_auth(&api_key).json(&body).send().await {
                Ok(r) => r,
                Err(e) => {
                    yield Err(transport(&format!(
                        "request to {url} failed: {}",
                        transport_cause_chain(&e)
                    )));
                    return;
                }
            };
            let status = response.status().as_u16();
            if status >= 400 {
                let retry_after = wire::parse_retry_after(response.headers().get(reqwest::header::RETRY_AFTER));
                let request_id = wire::parse_request_id(response.headers());
                let body_text = response.text().await.unwrap_or_default();
                yield Err(LlmError::Call { code: wire::normalize_status(status, &body_text).into(), message: body_text.lines().next().unwrap_or_default().to_string(), status: Some(status), provider_retry_after_ms: retry_after, request_id });
                return;
            }
            let mut translator: Box<dyn translate::StreamTranslator + Send> = if is_responses {
                Box::new(responses::ResponsesTranslator::new())
            } else {
                Box::new(translate::SseTranslator::new())
            };
            // WI-197: capture the request id and stream telemetry up front —
            // a mid-stream death used to surface as a bare top-line message
            // with nothing to reconcile against the provider's request log
            // (the 2026-10-14 incident: TRANSPORT with no cause, no id, no
            // byte count). `bytes_stream` consumes the response, so the
            // headers must be read first.
            let request_id = wire::parse_request_id(response.headers());
            let stream_started = std::time::Instant::now();
            let mut received_bytes = 0usize;
            let mut byte_stream = response.bytes_stream();
            // BUG-46: decode per completed LINE, not per network chunk — a
            // multi-byte UTF-8 char split across chunks would otherwise be
            // replaced by U+FFFD on both sides (a `\n` byte can never occur
            // inside a multi-byte char, so line boundaries are safe).
            let mut buf: Vec<u8> = Vec::new();
            let mut cancel_check = std::pin::pin!(async {
                if let Some(ref t) = cancel { t.cancelled().await; }
                else { std::future::pending::<()>().await; }
            });
            let cancel_check_active = cancel.is_some();
            loop {
                if cancel_check_active {
                    if cancel.as_ref().is_some_and(|t| t.is_cancelled()) {
                        yield Err(aborted()); return;
                    }
                }
                let next = byte_stream.next();
                tokio::pin!(next);
                let outcome = tokio::select! {
                    _ = &mut cancel_check, if cancel_check_active => CancelCheck::Cancelled,
                    r = tokio::time::timeout(idle, &mut next) => match r {
                        Err(_) => CancelCheck::Idle,
                        Ok(None) => CancelCheck::End,
                        Ok(Some(Ok(b))) => CancelCheck::Bytes(b),
                        Ok(Some(Err(e))) => CancelCheck::Transport(e),
                    },
                };
                match outcome {
                    CancelCheck::Cancelled => { yield Err(aborted()); return; }
                    CancelCheck::Idle => {
                        // WI-197: the code stays LLM_STREAM_IDLE_TIMEOUT (it
                        // is deliberately NOT retryable, unlike TRANSPORT);
                        // only the reconciliation facts are added.
                        let mut message = format!("stream idle for over {}ms", idle.as_millis());
                        message.push_str(&stream_telemetry(stream_started, received_bytes));
                        yield Err(idle_timeout_with(message, request_id.clone()));
                        return;
                    }
                    CancelCheck::Transport(e) => {
                        // WI-197: keep the reqwest cause chain — the top-line
                        // Display is just "error decoding response body"; the
                        // actionable facts (unexpected EOF, connection reset,
                        // h2 RST_STREAM) live in the source chain.
                        let mut message = transport_cause_chain(&e);
                        message.push_str(&stream_telemetry(stream_started, received_bytes));
                        yield Err(transport_with(message, request_id.clone()));
                        return;
                    }
                    CancelCheck::End => {
                        for chunk in translator.finish() { yield chunk; }
                        return;
                    }
                    CancelCheck::Bytes(bytes) => {
                        received_bytes += bytes.len();
                        buf.extend_from_slice(&bytes);
                        while let Some(pos) = buf.iter().position(|b| *b == b'\n') {
                            let line_bytes: Vec<u8> = buf.drain(..=pos).collect();
                            let line = String::from_utf8_lossy(&line_bytes).trim().to_string();
                            if line.is_empty() || line.starts_with(':') { continue; }
                            // BUG-55: the SSE spec allows `data:` without the
                            // trailing space — strip at most ONE leading space.
                            let Some(payload) = line
                                .strip_prefix("data:")
                                .map(|rest| rest.strip_prefix(' ').unwrap_or(rest))
                            else { continue };
                            if payload == "[DONE]" {
                                for chunk in translator.finish() { yield chunk; }
                                return;
                            }
                            for chunk in translator.feed(payload) { yield chunk; }
                            if translator.is_done() { return; }
                        }
                    }
                }
            }
        })
    }
}

enum CancelCheck {
    Cancelled,
    Idle,
    End,
    Bytes(bytes::Bytes),
    Transport(reqwest::Error),
}

fn aborted() -> LlmError {
    LlmError::Call {
        code: "ABORTED".into(),
        message: "stream cancelled by caller".into(),
        status: None,
        provider_retry_after_ms: None,
        request_id: None,
    }
}

/// WI-197: a mid-stream death message carries the reconciliation facts —
/// how much arrived and over how long (the incident's deaths clustered
/// 29-31s after the last byte; the message should show that shape).
fn stream_telemetry(started: std::time::Instant, received_bytes: usize) -> String {
    format!(
        "; stream received {received_bytes} bytes over {:.1}s",
        started.elapsed().as_secs_f32()
    )
}

fn transport_with(message: String, request_id: Option<String>) -> LlmError {
    LlmError::Call {
        code: "TRANSPORT".into(),
        message,
        status: None,
        provider_retry_after_ms: None,
        request_id,
    }
}

fn idle_timeout_with(message: String, request_id: Option<String>) -> LlmError {
    LlmError::Call {
        code: "LLM_STREAM_IDLE_TIMEOUT".into(),
        message,
        status: None,
        provider_retry_after_ms: None,
        request_id,
    }
}

/// WI-197: the Display of a reqwest error is only the top line ("error
/// decoding response body") — the actionable facts (unexpected EOF,
/// connection reset by peer, h2 RST_STREAM) live in the source chain.
/// Flatten the whole chain into one message.
fn transport_cause_chain(error: &reqwest::Error) -> String {
    let mut chain = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        chain.push_str("; caused by: ");
        chain.push_str(&cause.to_string());
        source = cause.source();
    }
    chain
}
fn transport(msg: &str) -> LlmError {
    LlmError::Call {
        code: "TRANSPORT".into(),
        message: msg.to_string(),
        status: None,
        provider_retry_after_ms: None,
        request_id: None,
    }
}
fn missing_cred(source: &str) -> LlmError {
    LlmError::Call {
        code: "MISSING_CREDENTIAL".into(),
        message: format!("credential unavailable: {source}"),
        status: None,
        provider_retry_after_ms: None,
        request_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lmstudio_profile_has_no_credential_and_sets_model() {
        let profile = crate::lmstudio_profile("lmstudio", "http://localhost:1234", "qwen3-8b");
        assert_eq!(profile.provider, "lmstudio");
        assert_eq!(profile.base_url, "http://localhost:1234");
        assert!(
            profile.credential.is_none(),
            "local server needs no credential"
        );
        assert_eq!(profile.models.len(), 1);
        assert_eq!(profile.models[0].id, "qwen3-8b");
    }

    #[test]
    fn lmstudio_profile_base_url_trailing_slash_trimmed() {
        let profile = crate::lmstudio_profile("lmstudio", "http://localhost:1234/", "test");
        assert_eq!(profile.base_url, "http://localhost:1234");
    }

    #[tokio::test]
    async fn detect_models_returns_empty_for_unreachable_endpoint() {
        let adapter = OpenAiCompletionsAdapter::new(
            "lmstudio".into(),
            Arc::new(|| crate::lmstudio_profile("lmstudio", "http://127.0.0.1:1", "test")),
        );
        let result = adapter.detect_models().await;
        assert!(result.is_err(), "unreachable endpoint → error, not panic");
    }

    #[test]
    fn adapter_declares_its_provider_route() {
        let adapter = OpenAiCompletionsAdapter::new(
            "deepseek".into(),
            Arc::new(|| crate::lmstudio_profile("deepseek", "http://localhost:1234", "m")),
        );
        assert_eq!(adapter.providers(), &["deepseek"]);
        assert_eq!(adapter.provider_info("deepseek").id, "deepseek");
    }

    #[tokio::test]
    async fn resolve_model_reports_directory_and_reasoning_capability() {
        let mut profile =
            crate::lmstudio_profile("deepseek", "http://localhost:1234", "deepseek-v4-flash");
        profile.models[0].context_window = Some(128_000);
        profile.models[0].max_tokens = Some(8192);
        profile.max_tokens = Some(4096);
        profile.reasoning_effort = Some("high".into());
        let adapter =
            OpenAiCompletionsAdapter::new("deepseek".into(), Arc::new(move || profile.clone()));

        let info = adapter
            .resolve_model("deepseek", "deepseek-v4-flash", None)
            .await
            .expect("resolve");
        assert_eq!(info.id, "deepseek-v4-flash");
        assert_eq!(info.context.unwrap().context_window, 128_000);
        assert_eq!(info.default_max_tokens, Some(8192));
        let reasoning = info.reasoning.expect("reasoning capability");
        assert_eq!(reasoning.default_effort.as_deref(), Some("high"));
        assert!(reasoning.efforts.iter().any(|e| e.id == "max"));

        let generic = adapter
            .resolve_model("deepseek", "undeclared-model", None)
            .await
            .expect("resolve fallback");
        assert_eq!(generic.id, "undeclared-model");
        assert_eq!(generic.default_max_tokens, Some(4096));
    }

    #[test]
    fn plugin_without_profiles_activates_without_routes() {
        let plugin = OpenAiCompletionsPlugin::new(Arc::new(Vec::new));
        assert_eq!(plugin.name(), "llm-openai");
        assert!(plugin.inject().contains("llm"));
    }
}
