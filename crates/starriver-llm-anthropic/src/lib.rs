#![allow(dead_code, clippy::new_without_default, clippy::collapsible_if)]
//! Anthropic Messages adapter (LLP L3).

pub mod wire;

use std::sync::Arc;

use cordis_rslite::{Context, EffectHandle, EventBus, Inject, Plugin};
use futures::stream::BoxStream;
use futures::StreamExt;
use starriver_kernel::llm::{
    GenerateOptions, LlmAdapter, LlmError, LlmProviderInfo, LlmRuntime, RetryPolicy, StreamChunk,
};
use starriver_llm::InMemoryLlmRuntime;

pub struct AnthropicAdapter {
    provider: String,
    base_url: String,
    credential: Option<CredentialRef>,
    credential_resolver: Option<CredentialResolver>,
    client: reqwest::Client,
    idle_timeout_ms: u64,
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
            let url = format!("{}/v1/messages", base_url);
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
                let body_text = response.text().await.unwrap_or_default();
                yield Err(LlmError::Call { code: wire::normalize_status_body(status, &body_text).into(), message: body_text.lines().next().unwrap_or_default().to_string(), status: Some(status), provider_retry_after_ms: None, request_id: None });
                return;
            }
            let mut translator = wire::SseTranslator::new();
            let mut byte_stream = response.bytes_stream().map(|r| r.map_err(|e| e.to_string()));
            // BUG-46: decode per completed FRAME, not per network chunk — a
            // multi-byte UTF-8 char split across chunks would otherwise be
            // corrupted (frame separators are pure ASCII).
            let mut buf: Vec<u8> = Vec::new();
            loop {
                if cancel.as_ref().is_some_and(|t| t.is_cancelled()) { yield Err(aborted()); return; }
                let next = byte_stream.next();
                tokio::pin!(next);
                let frame = tokio::select! {
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
                        source: starriver_kernel::llm::MessageSource::User,
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
}
