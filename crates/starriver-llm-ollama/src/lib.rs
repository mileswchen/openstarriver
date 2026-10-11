#![allow(dead_code, clippy::new_without_default, clippy::collapsible_if)]
//! Ollama adapter (LLP L4): NDJSON stream against the Ollama `/api/chat`
//! endpoint, emitting kernel `StreamChunk`s directly.

pub mod wire;

use std::sync::Arc;

use cordis_rslite::{Context, EffectHandle, EventBus, Inject, Plugin};
use futures::stream::BoxStream;
use futures::StreamExt;
use starriver_kernel::llm::{
    GenerateOptions, LlmAdapter, LlmError, LlmProviderInfo, LlmRuntime, RetryPolicy, StreamChunk,
};
use starriver_llm::InMemoryLlmRuntime;

pub struct OllamaAdapter {
    provider: String,
    base_url: String,
    client: reqwest::Client,
    idle_timeout_ms: u64,
}

/// One Ollama route to mount through the cordis plugin.
#[derive(Debug, Clone)]
pub struct OllamaRoute {
    pub provider: String,
    pub base_url: String,
}

/// Cordis plugin mounting one or more Ollama routes (LLP L4).
pub struct OllamaPlugin {
    routes: Arc<dyn Fn() -> Vec<OllamaRoute> + Send + Sync>,
}

impl OllamaPlugin {
    pub fn new(routes: Arc<dyn Fn() -> Vec<OllamaRoute> + Send + Sync>) -> Self {
        Self { routes }
    }
}

impl Plugin for OllamaPlugin {
    fn name(&self) -> &'static str {
        "llm-ollama"
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
                    let adapter =
                        Arc::new(OllamaAdapter::new(route.provider.clone(), route.base_url));
                    let handle = llm.register_adapter(adapter).map_err(|e| {
                        anyhow::anyhow!("llm-ollama plugin: register `{provider}`: {e}")
                    })?;
                    handles.push(handle);
                }
                Ok(handles)
            }),
            Err(e) => Box::pin(async move {
                Err(anyhow::anyhow!(
                    "llm-ollama plugin: llm service unavailable: {e}"
                ))
            }),
        }
    }
}

impl OllamaAdapter {
    pub fn new(provider: String, base_url: String) -> Self {
        Self {
            provider,
            base_url,
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

    /// Query the endpoint's `/api/tags` for the locally installed models
    /// (directory discovery; no credential needed).
    pub async fn detect_models(&self) -> Result<Vec<ModelEntry>, LlmError> {
        let url = format!("{}/api/tags", self.base_url.trim_end_matches('/'));
        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| LlmError::Call {
                code: "MODEL_DISCOVERY".into(),
                message: format!("ollama tags endpoint failed: {e}"),
                status: None,
                provider_retry_after_ms: None,
                request_id: None,
            })?;
        let status = response.status().as_u16();
        if status >= 400 {
            return Err(LlmError::Call {
                code: "MODEL_DISCOVERY".into(),
                message: format!("ollama tags endpoint returned HTTP {status}"),
                status: Some(status),
                provider_retry_after_ms: None,
                request_id: None,
            });
        }
        let json: serde_json::Value = response.json().await.map_err(|e| LlmError::Call {
            code: "MODEL_DISCOVERY".into(),
            message: format!("ollama tags response parse failed: {e}"),
            status: None,
            provider_retry_after_ms: None,
            request_id: None,
        })?;
        let mut out = Vec::new();
        if let Some(models) = json.get("models").and_then(|m| m.as_array()) {
            for entry in models {
                let id = entry
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                if id.is_empty() {
                    continue;
                }
                out.push(ModelEntry {
                    id: id.clone(),
                    name: Some(id),
                    context_window: None,
                    max_tokens: None,
                });
            }
        }
        Ok(out)
    }
}

/// One model discovered from an endpoint (provider directory entry).
#[derive(Debug, Clone)]
pub struct ModelEntry {
    pub id: String,
    pub name: Option<String>,
    pub context_window: Option<u64>,
    pub max_tokens: Option<u64>,
}

#[async_trait::async_trait]
impl LlmAdapter for OllamaAdapter {
    fn providers(&self) -> &[String] {
        std::slice::from_ref(&self.provider)
    }
    fn provider_info(&self, _provider: &str) -> LlmProviderInfo {
        LlmProviderInfo {
            id: self.provider.clone(),
            name: "Ollama".into(),
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

        Box::pin(async_stream::stream! {
            // A malformed image attachment must fail the stream, not ride the
            // request silently degraded.
            let body = match wire::serialize_request(&options) {
                Ok(body) => body,
                Err(e) => { yield Err(e); return; }
            };
            let url = format!("{}/api/chat", base_url.trim_end_matches('/'));
            let response = match client.post(&url).json(&body).send().await {
                Ok(r) => r,
                Err(e) => { yield Err(transport(&format!("request to {url} failed: {e}"))); return; }
            };
            let status = response.status().as_u16();
            if status >= 400 {
                let body_text = response.text().await.unwrap_or_default();
                yield Err(LlmError::Call { code: wire::normalize_status_body(status, &body_text).into(), message: body_text.lines().next().unwrap_or_default().to_string(), status: Some(status), provider_retry_after_ms: None, request_id: None });
                return;
            }
            let mut translator = wire::NdjsonTranslator::new();
            let mut byte_stream = response.bytes_stream().map(|r| r.map_err(|e| e.to_string()));
            // BUG-46: decode per completed LINE, not per network chunk (a
            // `\n` byte never occurs inside a multi-byte UTF-8 char).
            let mut buf: Vec<u8> = Vec::new();
            loop {
                if cancel.as_ref().is_some_and(|t| t.is_cancelled()) { yield Err(aborted()); return; }
                let next = byte_stream.next();
                tokio::pin!(next);
                let line = tokio::select! {
                    r = tokio::time::timeout(idle, &mut next) => match r {
                        Err(_) => { yield Err(idle_timeout(idle)); return; }
                        Ok(None) => break,
                        Ok(Some(Ok(b))) => b,
                        Ok(Some(Err(e))) => { yield Err(transport(&e)); return; }
                    },
                };
                buf.extend_from_slice(&line);
                while let Some(idx) = buf.iter().position(|b| *b == b'\n') {
                    let line_bytes: Vec<u8> = buf.drain(..=idx).collect();
                    let line_text = String::from_utf8_lossy(&line_bytes).trim().to_string();
                    if line_text.is_empty() { continue; }
                    for chunk in translator.feed_line(&line_text) { yield chunk; }
                    if translator.is_done() { return; }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_declares_its_provider_route() {
        let adapter = OllamaAdapter::new("ollama".into(), "http://localhost:11434".into());
        assert_eq!(adapter.providers(), &["ollama"]);
        assert_eq!(adapter.provider_info("ollama").id, "ollama");
    }

    #[test]
    fn plugin_declares_llm_dependency() {
        let plugin = OllamaPlugin::new(Arc::new(Vec::new));
        assert_eq!(plugin.name(), "llm-ollama");
        assert!(plugin.inject().contains("llm"));
    }
}
