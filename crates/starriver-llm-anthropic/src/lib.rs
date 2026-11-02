#![allow(dead_code, clippy::new_without_default, clippy::collapsible_if)]
//! Anthropic Messages adapter (LLP L3).

pub mod wire;

use std::sync::Arc;

use cordis_rslite::{Context, EffectHandle, EventBus, Inject, Plugin};
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use futures::StreamExt;
use starriver_kernel::llm::{
    GenerateOptions, LlmAdapter, LlmError, LlmModelContext, LlmProviderInfo, LlmReasoningEffortInfo,
    LlmReasoningInfo, LlmResolvedModelInfo, LlmRuntime, RetryPolicy, StreamChunk,
};
use starriver_llm::InMemoryLlmRuntime;
use tokio_util::sync::CancellationToken;

pub struct AnthropicAdapter {
    provider: String,
    base_url: String,
    credential: Option<CredentialRef>,
    credential_resolver: Option<CredentialResolver>,
    client: reqwest::Client,
    idle_timeout_ms: u64,
    /// Route-level default reasoning effort (BUG-163): advertises the
    /// {off,low,high,max} vocabulary through `resolve_model` and seeds the
    /// thinking budget ladder when the caller leaves the effort unset.
    reasoning_effort: Option<String>,
    /// Route-level model defaults (BUG-168): without a model directory the
    /// resolved info carried no context window and no output cap, leaving
    /// compaction budgeting and `max_tokens` fallbacks blind.
    context_window: Option<u64>,
    max_tokens: Option<u64>,
}

/// Credential reference for one Anthropic route: a literal key, an
/// environment variable, or a secrets-file account (`!file:<account>`).
/// Mirrors the credential-ref model; the host supplies a resolver so the
/// plugin never handles literal keys.
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

/// Host-provided credential resolver. Zero-arg so the adapter stays provider
/// agnostic; the host closure captures the route's credential reference.
pub type CredentialResolver = Arc<dyn Fn() -> Result<String, LlmError> + Send + Sync>;

/// One Anthropic Messages route to mount through the cordis plugin.
#[derive(Debug, Clone)]
pub struct AnthropicRoute {
    pub provider: String,
    pub base_url: String,
    pub credential: Option<CredentialRef>,
}

/// Cordis plugin mounting one or more Anthropic Messages routes (LLP L3).
pub struct AnthropicPlugin {
    routes: Arc<dyn Fn() -> Vec<AnthropicRoute> + Send + Sync>,
}

impl AnthropicPlugin {
    pub fn new(routes: Arc<dyn Fn() -> Vec<AnthropicRoute> + Send + Sync>) -> Self {
        Self { routes }
    }
}

impl Plugin for AnthropicPlugin {
    fn name(&self) -> &'static str {
        "llm-anthropic"
    }

    fn inject(&self) -> Inject {
        Inject::new([<InMemoryLlmRuntime as cordis_rslite::TypedService>::NAME])
    }

    fn apply(
        &self,
        ctx: &Context,
        _bus: &EventBus,
    ) -> cordis_rslite::BoxFuture<anyhow::Result<Vec<EffectHandle>>> {
        let routes = (self.routes)();
        match ctx.require::<InMemoryLlmRuntime>() {
            Ok(llm) => Box::pin(async move {
                let mut handles = Vec::new();
                for route in routes {
                    let provider = route.provider.clone();
                    let adapter = Arc::new(AnthropicAdapter::new(
                        route.provider.clone(),
                        route.base_url,
                        route.credential,
                    ));
                    let handle = llm.register_adapter(adapter).map_err(|e| {
                        anyhow::anyhow!("llm-anthropic plugin: register `{provider}`: {e}")
                    })?;
                    handles.push(handle);
                }
                Ok(handles)
            }),
            Err(e) => Box::pin(async move {
                Err(anyhow::anyhow!(
                    "llm-anthropic plugin: llm service unavailable: {e}"
                ))
            }),
        }
    }
}

impl AnthropicAdapter {
    pub fn new(provider: String, base_url: String, credential: Option<CredentialRef>) -> Self {
        Self {
            provider,
            base_url: base_url.trim_end_matches('/').to_string(),
            credential,
            credential_resolver: None,
            client: reqwest::Client::new(),
            idle_timeout_ms: 300_000,
            reasoning_effort: None,
            context_window: None,
            max_tokens: None,
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
    pub fn with_credential_resolver(mut self, resolver: CredentialResolver) -> Self {
        self.credential_resolver = Some(resolver);
        self
    }
    /// BUG-163: route-level reasoning default; the host applies the user's
    /// per-agent override here.
    pub fn with_reasoning_effort(mut self, effort: Option<String>) -> Self {
        self.reasoning_effort = effort;
        self
    }
    /// BUG-168: route-level model defaults from provider settings.
    pub fn with_model_defaults(
        mut self,
        context_window: Option<u64>,
        max_tokens: Option<u64>,
    ) -> Self {
        self.context_window = context_window;
        self.max_tokens = max_tokens;
        self
    }

    fn resolve_credential(&self) -> Result<String, LlmError> {
        if let Some(resolver) = self.credential_resolver.as_ref() {
            return resolver();
        }
        match self.credential.as_ref() {
            Some(cred) => match cred.resolve() {
                Ok(key) if !key.is_empty() => Ok(key),
                Ok(_) => Err(missing_cred("route (empty key)")),
                Err(e) => Err(e),
            },
            None => Err(missing_cred("route (no credential)")),
        }
    }
}

#[async_trait::async_trait]
impl LlmAdapter for AnthropicAdapter {
    fn providers(&self) -> &[String] {
        std::slice::from_ref(&self.provider)
    }
    fn provider_info(&self, _provider: &str) -> LlmProviderInfo {
        LlmProviderInfo {
            id: self.provider.clone(),
            name: "Anthropic".into(),
        }
    }
    fn provider_retry_policy(&self, _provider: &str) -> Option<RetryPolicy> {
        None
    }

    /// BUG-163/BUG-168: exact-model metadata with the route's reasoning
    /// vocabulary and model defaults. Without this the adapter fell back to
    /// `reasoning: None`, so (a) an explicit caller effort was rejected with
    /// UNSUPPORTED_REASONING_EFFORT by `resolve_with_model_info`, and (b)
    /// context-window/output-cap budgeting saw nothing.
    fn resolve_model(
        &self,
        _provider: &str,
        model: &str,
        _cancellation: Option<CancellationToken>,
    ) -> BoxFuture<'static, Result<LlmResolvedModelInfo, LlmError>> {
        let provider = self.provider.clone();
        let reasoning = self.reasoning_effort.clone();
        let context_window = self.context_window;
        let max_tokens = self.max_tokens;
        let model = model.to_string();
        Box::pin(async move {
            Ok(LlmResolvedModelInfo {
                provider,
                id: model.clone(),
                name: model,
                description: None,
                input_modalities: None,
                context: context_window
                    .map(|context_window| LlmModelContext { context_window }),
                default_max_tokens: max_tokens,
                reasoning: reasoning.map(|default_effort| LlmReasoningInfo {
                    efforts: ["off", "low", "high", "max"]
                        .into_iter()
                        .map(|id| LlmReasoningEffortInfo {
                            id: id.into(),
                            name: id.into(),
                            description: None,
                        })
                        .collect(),
                    default_effort: (default_effort != "off").then(|| default_effort),
                }),
            })
        })
    }

    fn stream(
        &self,
        options: GenerateOptions,
    ) -> BoxStream<'static, Result<StreamChunk, LlmError>> {
        let base_url = self.base_url.clone();
        let client = self.client.clone();
        let idle = std::time::Duration::from_millis(self.idle_timeout_ms);
        let cancel = options.cancellation.clone();
        let credential = self.resolve_credential();

        Box::pin(async_stream::stream! {
            let api_key = match credential {
                Ok(key) => key,
                Err(e) => { yield Err(e); return; }
            };
            // A malformed image attachment must fail the stream, not ride the
            // request silently degraded.
            let body = match wire::serialize_request(&options) {
                Ok(body) => body,
                Err(e) => { yield Err(e); return; }
            };
            let url = messages_url(&base_url);
            let response = match client.post(&url)
                .header("x-api-key", &api_key)
                .header("anthropic-version", "2023-06-01")
                .json(&body).send().await
            {
                Ok(r) => r,
                Err(e) => { yield Err(transport(&format!("request to {url} failed: {e}"))); return; }
            };
            let status = response.status().as_u16();
            if status >= 400 {
                // BUG-167: the provider-advised backoff window and request id
                // must be read off the headers BEFORE the body consumes the
                // response (parity with the OpenAI adapter).
                let retry_after = wire::parse_retry_after(response.headers().get(reqwest::header::RETRY_AFTER));
                let request_id = wire::parse_request_id(response.headers());
                let body_text = response.text().await.unwrap_or_default();
                // Undeclared (`supports_vision: null`) models let the provider
                // arbitrate images. When the verdict is the GLM 1210 image
                // rejection, classify it under the same UNSUPPORTED_CONTENT
                // contract a declared text-only route enforces at assembly,
                // with the remedy attached — a bare INVALID_REQUEST 400 gives
                // the user nothing actionable (2026-11-01: mid-session model
                // switch to glm-5.3 in an image-bearing session).
                let image_rejected = wire::unsupported_image_rejection(status, &body_text);
                yield Err(LlmError::Call {
                    code: if image_rejected {
                        "UNSUPPORTED_CONTENT".into()
                    } else {
                        wire::normalize_status_body(status, &body_text).into()
                    },
                    message: if image_rejected {
                        format!(
                            "{}\n模型 \"{}\" 的端点不接受图片输入（provider 1210：messages.content.type 仅接受 text）；请将该模型声明为纯文本（supports_vision: false）、改用支持视觉的模型（如 glm-5.3-flash），或移除会话中的图片。",
                            body_text.lines().next().unwrap_or_default(),
                            options.model,
                        )
                    } else {
                        body_text.lines().next().unwrap_or_default().to_string()
                    },
                    status: Some(status),
                    provider_retry_after_ms: retry_after,
                    request_id,
                });
                return;
            }
            let mut translator = wire::SseTranslator::new();
            let mut byte_stream = response.bytes_stream().map(|r| r.map_err(|e| e.to_string()));
            // BUG-46: decode per completed FRAME, not per network chunk — a
            // multi-byte UTF-8 char split across chunks would otherwise be
            // corrupted (frame separators are pure ASCII).
            let mut buf: Vec<u8> = Vec::new();
            // Cancel rides the select (parity with the OpenAI adapter): the
            // old loop-top poll only ran between chunk yields, so a Stop
            // pressed while the endpoint had gone silent waited out the whole
            // idle window before the abort surfaced (2026-11-01 session
            // a806fe8c: silence from a stalled zhipu Anthropic stream, Stop
            // at +14s, adapter still parked).
            let mut cancel_check = std::pin::pin!(async {
                if let Some(ref t) = cancel { t.cancelled().await; }
                else { std::future::pending::<()>().await; }
            });
            let cancel_check_active = cancel.is_some();
            loop {
                let next = byte_stream.next();
                tokio::pin!(next);
                let frame = tokio::select! {
                    _ = &mut cancel_check, if cancel_check_active => { yield Err(aborted()); return; }
                    r = tokio::time::timeout(idle, &mut next) => match r {
                        Err(_) => { yield Err(idle_timeout(idle)); return; }
                        Ok(None) => break,
                        Ok(Some(Ok(b))) => b,
                        Ok(Some(Err(e))) => { yield Err(transport(&e)); return; }
                    },
                };
                buf.extend_from_slice(&frame);
                // BUG-40: the SSE spec allows `\r\n` line endings (frame
                // separator `\r\n\r\n`) — accept both separators so CRLF
                // servers are not swallowed whole. BUG-55: `data:` without
                // the trailing space is legal too (strip ONE leading space).
                loop {
                    let find_sub = |hay: &[u8], needle: &[u8]| -> Option<usize> {
                        hay.windows(needle.len()).position(|w| w == needle)
                    };
                    let rel = match (
                        find_sub(&buf, b"\n\n"),
                        find_sub(&buf, b"\r\n\r\n"),
                    ) {
                        (Some(a), Some(b)) => Some(if b < a { b } else { a }),
                        (Some(a), None) => Some(a),
                        (None, Some(b)) => Some(b),
                        (None, None) => None,
                    };
                    let Some(rel) = rel else { break };
                    let sep_len = if buf[rel..].starts_with(b"\r\n\r\n") { 4 } else { 2 };
                    let frame_bytes: Vec<u8> = buf.drain(..rel + sep_len).collect();
                    let frame_text = String::from_utf8_lossy(&frame_bytes);
                    let Some(data_line) = frame_text.lines().find_map(|l| {
                        l.strip_prefix("data:")
                            .map(|rest| rest.strip_prefix(' ').unwrap_or(rest))
                    }) else {
                        continue;
                    };
                    let trimmed = data_line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    for chunk in translator.feed_frame(trimmed) {
                        yield chunk;
                    }
                    if translator.is_done() {
                        return;
                    }
                }
            }
            for chunk in translator.finish() { yield chunk; }
        })
    }
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

/// BUG-169: build the Messages endpoint. OpenAI-family routes expect the
/// `/v1` prefix INSIDE `base_url` while this adapter used to append
/// `/v1/messages` unconditionally — copying an OpenAI-style base
/// (`https://host/v1`) produced `/v1/v1/messages` 404s. A base already
/// ending in `/v1` now only appends `/messages`.
fn messages_url(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/v1") {
        format!("{base}/messages")
    } else {
        format!("{base}/v1/messages")
    }
}
fn idle_timeout(d: std::time::Duration) -> LlmError {
    LlmError::Call {
        code: "LLM_STREAM_IDLE_TIMEOUT".into(),
        message: format!("stream idle for over {}ms", d.as_millis()),
        status: None,
        provider_retry_after_ms: None,
        request_id: None,
    }
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
    fn adapter_declares_its_provider_route() {
        let adapter = AnthropicAdapter::new(
            "claude".into(),
            "https://api.example.com".into(),
            Some(CredentialRef::Literal("k".into())),
        );
        assert_eq!(adapter.providers(), &["claude"]);
        assert_eq!(adapter.provider_info("claude").id, "claude");
    }

    #[test]
    fn missing_credential_fails_before_http() {
        let adapter =
            AnthropicAdapter::new("claude".into(), "https://api.example.com".into(), None);
        let err = futures::executor::block_on(async {
            let chunks: Vec<_> = adapter
                .stream(starriver_kernel::llm::GenerateOptions {
                    provider: "claude".into(),
                    model: "claude-3-5-sonnet".into(),
                    reasoning_effort: None,
                    messages: vec![starriver_kernel::llm::Message {
                        id: starriver_kernel::llm::MessageId("m1".into()),
                        role: starriver_kernel::llm::MessageRole::User,
                        content: vec![starriver_kernel::llm::ContentBlock::Text("hi".into())],
                        source: starriver_kernel::llm::MessageSource::user(),
                    }],
                    system: None,
                    tools: None,
                    temperature: None,
                    max_tokens: None,
                    stop: None,
                    session_id: None,
                    agent_id: None,
                    tool_choice: None,
                    cancellation: None,
                    purpose: None,
                    loop_assembled: false,
                })
                .collect()
                .await;
            chunks.into_iter().find_map(|c| match c {
                Ok(_) => None,
                Err(e) => Some(e),
            })
        });
        let err = err.expect("stream must fail");
        assert_eq!(err.code(), "MISSING_CREDENTIAL");
    }

    #[test]
    fn plugin_declares_llm_dependency() {
        let plugin = AnthropicPlugin::new(Arc::new(Vec::new));
        assert_eq!(plugin.name(), "llm-anthropic");
        assert!(plugin.inject().contains("llm"));
    }

    // ── BUG-169: /v1 base normalization ───────────────────────────────

    #[test]
    fn messages_url_normalizes_v1_suffix() {
        assert_eq!(
            messages_url("https://api.anthropic.com"),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            messages_url("https://relay.example.com/v1"),
            "https://relay.example.com/v1/messages",
            "a base already carrying /v1 must not duplicate it"
        );
        assert_eq!(
            messages_url("https://relay.example.com/v1/"),
            "https://relay.example.com/v1/messages",
            "trailing slashes are trimmed before the check"
        );
    }

    // ── BUG-163/BUG-168: reasoning + model defaults in resolve_model ──

    #[tokio::test]
    async fn resolve_model_advertises_reasoning_and_defaults() {
        let adapter = AnthropicAdapter::new(
            "claude".into(),
            "https://api.example.com".into(),
            Some(CredentialRef::Literal("k".into())),
        )
        .with_reasoning_effort(Some("high".into()))
        .with_model_defaults(Some(200_000), Some(8_192));
        let info = adapter.resolve_model("claude", "claude-sonnet-4", None).await.unwrap();
        assert_eq!(info.context.unwrap().context_window, 200_000);
        assert_eq!(info.default_max_tokens, Some(8_192));
        let reasoning = info.reasoning.expect("reasoning capability advertised");
        assert_eq!(reasoning.default_effort.as_deref(), Some("high"));
        assert!(reasoning.efforts.iter().any(|e| e.id == "off"));
        assert!(reasoning.efforts.iter().any(|e| e.id == "max"));
    }

    #[tokio::test]
    async fn resolve_model_without_reasoning_default_advertises_none() {
        let adapter = AnthropicAdapter::new(
            "claude".into(),
            "https://api.example.com".into(),
            Some(CredentialRef::Literal("k".into())),
        );
        let info = adapter.resolve_model("claude", "claude-sonnet-4", None).await.unwrap();
        assert!(info.reasoning.is_none(), "no route default → no advertised efforts");
        assert!(info.context.is_none());
    }
}
