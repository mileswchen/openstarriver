//! Default in-memory LLM runtime: adapter registry, capability resolution,
//! prepared calls, provider directory, model discovery, and the `llm/stream`
//! waterfall, matching `LlmRuntime`.

pub mod contract_tests;
pub mod token_meter;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use cordis_rslite::{Context, EffectHandle, EventBus, Inject, Plugin, TypedService};
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use indexmap::IndexMap;
use starriver_kernel::llm::{
    dispatch_llm_stream, resolve_retry_policy, GenerateOptions, LlmAdapter, LlmAdaptersUpdated,
    LlmCallConfig, LlmCallConfigAdapterDefaults, LlmConfigurableProvider, LlmDiscoveredModel,
    LlmError, LlmModelDiscovery, LlmModelDiscoveryRequest, LlmModelInfo, LlmProviderInfo,
    LlmResolvedModelInfo, LlmRuntime, PreparedLlmCall, RetryPolicy, StreamChunk,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct AdapterRegistration {
    adapter: Arc<dyn LlmAdapter>,
    retry_policy: RetryPolicy,
    provider_info: LlmProviderInfo,
}

/// In-memory adapter registry routing `llm/stream` through the waterfall.
#[derive(Clone)]
pub struct InMemoryLlmRuntime {
    adapters: Arc<Mutex<IndexMap<String, AdapterRegistration>>>,
    configurable: Arc<Mutex<HashMap<String, LlmConfigurableProvider>>>,
    discoveries: Arc<Mutex<HashMap<String, LlmModelDiscovery>>>,
    bus: EventBus,
}

impl InMemoryLlmRuntime {
    /// Test/rebind hook: remove the adapter serving `provider` so a
    /// replacement can register (A1: mobile rebinds the pool bridge in
    /// tests; desktop can use it when the active route changes).
    pub fn unregister_provider(&self, provider: &str) {
        self.adapters
            .lock()
            .expect("llm adapter registry poisoned")
            .shift_remove(provider);
    }

    /// All provider routes currently served, in registration order. The first
    /// entry is the natural default for new agents.
    pub fn registered_providers(&self) -> Vec<String> {
        self.adapters
            .lock()
            .expect("llm adapter registry poisoned")
            .keys()
            .cloned()
            .collect()
    }

    pub fn new(bus: EventBus) -> Self {
        Self {
            adapters: Arc::new(Mutex::new(IndexMap::new())),
            configurable: Arc::new(Mutex::new(HashMap::new())),
            discoveries: Arc::new(Mutex::new(HashMap::new())),
            bus,
        }
    }

    fn provider_names(&self) -> Vec<String> {
        self.adapters
            .lock()
            .expect("llm adapter registry poisoned")
            .keys()
            .cloned()
            .collect()
    }

    fn emit_adapters_updated(&self) {
        self.bus.emit(Arc::new(LlmAdaptersUpdated {
            providers: self.provider_names(),
        }));
    }

    fn registration(&self, provider: &str) -> Result<AdapterRegistration, LlmError> {
        self.adapters
            .lock()
            .expect("llm adapter registry poisoned")
            .get(provider)
            .cloned()
            .ok_or_else(|| LlmError::NoAdapter(provider.into()))
    }
}

impl TypedService for InMemoryLlmRuntime {
    const NAME: &'static str = <InMemoryLlmRuntime as LlmRuntime>::NAME;
}

impl LlmRuntime for InMemoryLlmRuntime {
    fn register_adapter(&self, adapter: Arc<dyn LlmAdapter>) -> Result<EffectHandle, LlmError> {
        let providers = adapter.providers().to_vec();
        if providers.is_empty() {
            return Err(LlmError::InvalidAdapter(
                "adapter must declare at least one provider".into(),
            ));
        }
        {
            let mut adapters = self.adapters.lock().expect("llm adapter registry poisoned");
            // Stage first, insert last: a mid-list validation failure must
            // not leave earlier providers registered (the old loop inserted
            // as it validated, so a DuplicateAdapter on provider #2 left
            // provider #1 live with no handle to remove it).
            let mut staged: Vec<(String, AdapterRegistration)> = Vec::new();
            for provider in &providers {
                if provider.is_empty() {
                    return Err(LlmError::InvalidAdapter(
                        "provider name must be non-empty".into(),
                    ));
                }
                if adapters.contains_key(provider) {
                    return Err(LlmError::DuplicateAdapter(provider.clone()));
                }
                let info = adapter.provider_info(provider);
                if info.id != *provider || info.name.is_empty() {
                    return Err(LlmError::InvalidAdapter(format!(
                        "adapter metadata for provider \"{provider}\" must preserve its id and have a non-empty name"
                    )));
                }
                let retry_policy = resolve_retry_policy(
                    adapter.provider_retry_policy(provider),
                    &format!("llm: provider \"{provider}\" retryPolicy"),
                )?;
                staged.push((
                    provider.clone(),
                    AdapterRegistration {
                        adapter: Arc::clone(&adapter),
                        retry_policy,
                        provider_info: info,
                    },
                ));
            }
            for (provider, registration) in staged {
                adapters.insert(provider, registration);
            }
        }
        self.emit_adapters_updated();

        let adapters = Arc::clone(&self.adapters);
        let bus = self.bus.clone();
        Ok(EffectHandle::new(move || {
            let mut adapters = adapters.lock().expect("llm adapter registry poisoned");
            for provider in &providers {
                adapters.shift_remove(provider);
            }
            let remaining = adapters.keys().cloned().collect::<Vec<_>>();
            drop(adapters);
            bus.emit(Arc::new(LlmAdaptersUpdated {
                providers: remaining,
            }));
        }))
    }

    fn replace_adapter(
        &self,
        providers: Vec<String>,
        adapter: Arc<dyn LlmAdapter>,
    ) -> Result<EffectHandle, LlmError> {
        let new_providers = adapter.providers().to_vec();
        if new_providers.is_empty() {
            return Err(LlmError::InvalidAdapter(
                "adapter must declare at least one provider".into(),
            ));
        }
        {
            let mut adapters = self.adapters.lock().expect("llm adapter registry poisoned");
            // Stage the removal so the replaced names can be re-registered;
            // roll the entries back if the new routes fail validation.
            let removed = providers
                .iter()
                .filter_map(|provider| {
                    adapters
                        .shift_remove(provider)
                        .map(|registration| (provider.clone(), registration))
                })
                .collect::<Vec<_>>();
            let mut staged = Vec::new();
            let validation = (|| -> Result<(), LlmError> {
                for provider in &new_providers {
                    if provider.is_empty() {
                        return Err(LlmError::InvalidAdapter(
                            "provider name must be non-empty".into(),
                        ));
                    }
                    if adapters.contains_key(provider) {
                        return Err(LlmError::DuplicateAdapter(provider.clone()));
                    }
                    let info = adapter.provider_info(provider);
                    if info.id != *provider || info.name.is_empty() {
                        return Err(LlmError::InvalidAdapter(format!(
                            "adapter metadata for provider \"{provider}\" must preserve its id and have a non-empty name"
                        )));
                    }
                    let retry_policy = resolve_retry_policy(
                        adapter.provider_retry_policy(provider),
                        &format!("llm: provider \"{provider}\" retryPolicy"),
                    )?;
                    staged.push((
                        provider.clone(),
                        AdapterRegistration {
                            adapter: Arc::clone(&adapter),
                            retry_policy,
                            provider_info: info,
                        },
                    ));
                }
                Ok(())
            })();
            if let Err(error) = validation {
                // Roll back IN PLACE: rebuild the original order by
                // re-inserting every surviving entry after restoring the
                // removed ones (a plain append reorders the map and changes
                // the default-route pick on a FAILED call).
                let survivors = adapters
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect::<Vec<(String, AdapterRegistration)>>();
                adapters.clear();
                for (provider, registration) in removed {
                    adapters.insert(provider, registration);
                }
                for (provider, registration) in survivors {
                    adapters.insert(provider, registration);
                }
                return Err(error);
            }
            for (provider, registration) in staged {
                adapters.insert(provider, registration);
            }
        }
        self.emit_adapters_updated();

        let adapters = Arc::clone(&self.adapters);
        let bus = self.bus.clone();
        Ok(EffectHandle::new(move || {
            let mut adapters = adapters.lock().expect("llm adapter registry poisoned");
            for provider in &new_providers {
                adapters.shift_remove(provider);
            }
            let remaining = adapters.keys().cloned().collect::<Vec<_>>();
            drop(adapters);
            bus.emit(Arc::new(LlmAdaptersUpdated {
                providers: remaining,
            }));
        }))
    }

    fn list_providers(&self) -> Vec<LlmProviderInfo> {
        self.adapters
            .lock()
            .expect("llm adapter registry poisoned")
            .values()
            .map(|registration| registration.provider_info.clone())
            .collect()
    }

    fn provider_retry_policy(&self, provider: &str) -> Result<RetryPolicy, LlmError> {
        Ok(self.registration(provider)?.retry_policy.clone())
    }

    fn list_models(
        &self,
        provider: &str,
    ) -> BoxFuture<'static, Result<Vec<LlmModelInfo>, LlmError>> {
        let registration = match self.registration(provider) {
            Ok(registration) => registration,
            Err(error) => return Box::pin(async move { Err(error) }),
        };
        let provider = provider.to_string();
        Box::pin(async move {
            let models = registration.adapter.list_models(&provider).await?;
            let mut seen = HashSet::new();
            let mut out = Vec::new();
            for model in models {
                if model.provider != provider
                    || model.id.is_empty()
                    || model.name.is_empty()
                    || !seen.insert(model.id.clone())
                {
                    return Err(LlmError::Call {
                        code: "INVALID_CATALOG".into(),
                        message: format!(
                            "adapter returned invalid or duplicate model metadata for provider \"{provider}\""
                        ),
                        status: None,
                        provider_retry_after_ms: None,
                        request_id: None,
                    });
                }
                out.push(model);
            }
            Ok(out)
        })
    }

    fn resolve_model_info(
        &self,
        provider: &str,
        model: &str,
        cancellation: Option<CancellationToken>,
    ) -> BoxFuture<'static, Result<LlmResolvedModelInfo, LlmError>> {
        let registration = match self.registration(provider) {
            Ok(registration) => registration,
            Err(error) => return Box::pin(async move { Err(error) }),
        };
        let provider = provider.to_string();
        let model = model.to_string();
        Box::pin(async move {
            let resolved = registration
                .adapter
                .resolve_model(&provider, &model, cancellation)
                .await?;
            if resolved.provider != provider || resolved.id != model || resolved.name.is_empty() {
                return Err(LlmError::Call {
                    code: "INVALID_MODEL_INFO".into(),
                    message: format!(
                        "adapter returned invalid exact model metadata for provider \"{provider}\" model \"{model}\""
                    ),
                    status: None,
                    provider_retry_after_ms: None,
                    request_id: None,
                });
            }
            if resolved
                .context
                .is_some_and(|context| context.context_window == 0)
            {
                return Err(LlmError::Call {
                    code: "INVALID_MODEL_CONTEXT".into(),
                    message: format!(
                        "adapter returned invalid context metadata for provider \"{provider}\" model \"{model}\""
                    ),
                    status: None,
                    provider_retry_after_ms: None,
                    request_id: None,
                });
            }
            if resolved
                .default_max_tokens
                .is_some_and(|max_tokens| max_tokens == 0)
            {
                return Err(LlmError::Call {
                    code: "INVALID_MODEL_MAX_TOKENS".into(),
                    message: format!(
                        "adapter returned invalid default maxTokens for provider \"{provider}\" model \"{model}\""
                    ),
                    status: None,
                    provider_retry_after_ms: None,
                    request_id: None,
                });
            }
            if let Some(reasoning) = &resolved.reasoning {
                if reasoning.efforts.is_empty() {
                    return Err(invalid_reasoning(&provider, &model));
                }
                let mut seen = HashSet::new();
                for effort in &reasoning.efforts {
                    if effort.id.is_empty()
                        || effort.name.is_empty()
                        || !seen.insert(effort.id.clone())
                    {
                        return Err(invalid_reasoning(&provider, &model));
                    }
                }
                if let Some(default) = &reasoning.default_effort {
                    if !reasoning.efforts.iter().any(|effort| &effort.id == default) {
                        return Err(invalid_reasoning(&provider, &model));
                    }
                }
            }
            Ok(resolved)
        })
    }

    fn resolve_call_config(
        &self,
        config: LlmCallConfig,
        cancellation: Option<CancellationToken>,
    ) -> BoxFuture<'static, Result<LlmCallConfig, LlmError>> {
        let this = self.clone();
        Box::pin(async move {
            let info = this
                .resolve_model_info(&config.provider, &config.model, cancellation)
                .await?;
            resolve_with_model_info(config, &info)
        })
    }

    fn prepare_call(
        &self,
        config: LlmCallConfig,
        cancellation: Option<CancellationToken>,
    ) -> BoxFuture<'static, Result<PreparedLlmCall, LlmError>> {
        let this = self.clone();
        Box::pin(async move {
            let registration = this.registration(&config.provider)?;
            let info = this
                .resolve_model_info(&config.provider, &config.model, cancellation)
                .await?;
            let adapter_defaults = LlmCallConfigAdapterDefaults {
                reasoning_effort: config.reasoning_effort.is_none() && info.reasoning.is_some(),
                max_tokens: config.max_tokens.is_none() && info.default_max_tokens.is_some(),
            };
            let resolved = resolve_with_model_info(config, &info)?;
            Ok(PreparedLlmCall::new(
                resolved,
                registration.retry_policy.clone(),
                registration.adapter.clone(),
                this.bus.clone(),
            )
            .with_resolution(adapter_defaults, info.context))
        })
    }

    fn stream(
        &self,
        options: GenerateOptions,
    ) -> BoxStream<'static, Result<StreamChunk, LlmError>> {
        let candidate = {
            let adapters = self.adapters.lock().expect("llm adapter registry poisoned");
            adapters
                .get(&options.provider)
                .map(|reg| reg.adapter.clone())
        };
        let adapter = match candidate {
            Some(adapter) => adapter,
            None => {
                return Box::pin(futures::stream::once(async move {
                    Err(LlmError::NoAdapter(options.provider.clone()))
                }))
            }
        };
        dispatch_llm_stream(&self.bus, options, adapter)
    }

    fn chat(&self, options: GenerateOptions) -> BoxFuture<'static, Result<String, LlmError>> {
        // Same evented path as `stream` (the kernel contract: "every Usage
        // chunk crossing this path is evented"): the direct adapter call
        // bypassed the waterfall AND the token meter, so title/memory/chat
        // usage was never counted.
        let candidate = {
            let adapters = self.adapters.lock().expect("llm adapter registry poisoned");
            adapters
                .get(&options.provider)
                .map(|reg| reg.adapter.clone())
        };
        match candidate {
            Some(adapter) => adapter.chat(options),
            None => Box::pin(async move { Err(LlmError::NoAdapter(options.provider.clone())) }),
        }
    }

    fn register_configurable_providers(
        &self,
        entries: Vec<LlmConfigurableProvider>,
    ) -> Result<EffectHandle, LlmError> {
        if entries.is_empty() {
            return Err(LlmError::InvalidDirectory(
                "a configurable-provider registration must declare at least one provider".into(),
            ));
        }
        let mut detached: Vec<LlmConfigurableProvider> = Vec::new();
        {
            let mut directory = self
                .configurable
                .lock()
                .expect("provider directory poisoned");
            for entry in &entries {
                if entry.provider.is_empty()
                    || entry.display_name.is_empty()
                    || entry.settings_ns.is_empty()
                {
                    return Err(LlmError::InvalidDirectory(
                        "configurable providers need a non-empty provider, displayName, and settingsNs"
                            .into(),
                    ));
                }
                if entry.settings_path.iter().any(|segment| segment.is_empty()) {
                    return Err(LlmError::InvalidDirectory(
                        "configurable provider settingsPath must not contain empty segments".into(),
                    ));
                }
                if directory.contains_key(&entry.provider)
                    || detached.iter().any(|seen| seen.provider == entry.provider)
                {
                    return Err(LlmError::Call {
                        code: "DUPLICATE_DIRECTORY".into(),
                        message: format!(
                            "configurable provider \"{}\" is already declared",
                            entry.provider
                        ),
                        status: None,
                        provider_retry_after_ms: None,
                        request_id: None,
                    });
                }
                detached.push(entry.clone());
            }
            for entry in &detached {
                directory.insert(entry.provider.clone(), entry.clone());
            }
        }
        self.emit_adapters_updated();

        let configurable = Arc::clone(&self.configurable);
        let adapters = Arc::clone(&self.adapters);
        let bus = self.bus.clone();
        Ok(EffectHandle::new(move || {
            let mut directory = configurable.lock().expect("provider directory poisoned");
            for entry in &detached {
                directory.remove(&entry.provider);
            }
            drop(directory);
            let providers = adapters
                .lock()
                .expect("llm adapter registry poisoned")
                .keys()
                .cloned()
                .collect::<Vec<_>>();
            bus.emit(Arc::new(LlmAdaptersUpdated { providers }));
        }))
    }

    fn list_configurable_providers(&self) -> Vec<LlmConfigurableProvider> {
        self.configurable
            .lock()
            .expect("provider directory poisoned")
            .values()
            .cloned()
            .collect()
    }

    fn register_model_discovery(
        &self,
        settings_ns: String,
        discover: LlmModelDiscovery,
    ) -> Result<EffectHandle, LlmError> {
        if settings_ns.is_empty() {
            return Err(LlmError::Call {
                code: "INVALID_DISCOVERY".into(),
                message: "model discovery needs a non-empty settings namespace".into(),
                status: None,
                provider_retry_after_ms: None,
                request_id: None,
            });
        }
        {
            let mut discoveries = self.discoveries.lock().expect("discoveries poisoned");
            if discoveries.contains_key(&settings_ns) {
                return Err(LlmError::Call {
                    code: "DUPLICATE_DISCOVERY".into(),
                    message: format!("model discovery for \"{settings_ns}\" is already registered"),
                    status: None,
                    provider_retry_after_ms: None,
                    request_id: None,
                });
            }
            discoveries.insert(settings_ns.clone(), discover);
        }
        let discoveries = Arc::clone(&self.discoveries);
        Ok(EffectHandle::new(move || {
            discoveries
                .lock()
                .expect("discoveries poisoned")
                .remove(&settings_ns);
        }))
    }

    fn discover_models(
        &self,
        settings_ns: String,
        request: LlmModelDiscoveryRequest,
    ) -> BoxFuture<'static, Result<Vec<LlmDiscoveredModel>, LlmError>> {
        let discover = match self
            .discoveries
            .lock()
            .expect("discoveries poisoned")
            .get(&settings_ns)
        {
            Some(discover) => Arc::clone(discover),
            None => {
                return Box::pin(async move {
                    Err(LlmError::Call {
                        code: "NO_DISCOVERY".into(),
                        message: format!("no model discovery is registered for \"{settings_ns}\""),
                        status: None,
                        provider_retry_after_ms: None,
                        request_id: None,
                    })
                })
            }
        };
        if request.provider.as_deref().unwrap_or("").is_empty()
            && request.base_url.as_deref().unwrap_or("").is_empty()
        {
            return Box::pin(async move {
                Err(LlmError::Call {
                    code: "INVALID_DISCOVERY".into(),
                    message: "model discovery needs a provider route or a baseURL".into(),
                    status: None,
                    provider_retry_after_ms: None,
                    request_id: None,
                })
            });
        }
        Box::pin(async move {
            let discovered = discover(request).await?;
            let mut seen = HashSet::new();
            let mut models = Vec::new();
            for model in discovered {
                if model.id.is_empty() || !seen.insert(model.id.clone()) {
                    continue;
                }
                models.push(model);
            }
            Ok(models)
        })
    }
}

fn invalid_reasoning(provider: &str, model: &str) -> LlmError {
    LlmError::Call {
        code: "INVALID_MODEL_REASONING".into(),
        message: format!(
            "adapter returned invalid or duplicate reasoning metadata for provider \"{provider}\" model \"{model}\""
        ),
        status: None,
        provider_retry_after_ms: None,
        request_id: None,
    }
}

/// Materialize adapter-configured defaults against exact-model capability and
/// reject unsupported explicit reasoning efforts, mirroring
/// `resolveCallFor`.
fn resolve_with_model_info(
    mut config: LlmCallConfig,
    info: &LlmResolvedModelInfo,
) -> Result<LlmCallConfig, LlmError> {
    if config.max_tokens.is_none() {
        config.max_tokens = info.default_max_tokens;
    }
    match &info.reasoning {
        None => {
            if config.reasoning_effort.is_some() {
                return Err(LlmError::Call {
                    code: "UNSUPPORTED_REASONING_EFFORT".into(),
                    message: format!(
                        "provider \"{}\" model \"{}\" does not support reasoning effort",
                        info.provider, info.id
                    ),
                    status: None,
                    provider_retry_after_ms: None,
                    request_id: None,
                });
            }
        }
        Some(reasoning) => {
            let effective = config
                .reasoning_effort
                .clone()
                .or_else(|| reasoning.default_effort.clone());
            if let Some(effective) = effective {
                if !reasoning
                    .efforts
                    .iter()
                    .any(|effort| effort.id == effective)
                {
                    return Err(LlmError::Call {
                        code: "UNSUPPORTED_REASONING_EFFORT".into(),
                        message: format!(
                            "provider \"{}\" model \"{}\" does not support reasoning effort \"{effective}\"",
                            info.provider, info.id
                        ),
                        status: None,
                        provider_retry_after_ms: None,
                        request_id: None,
                    });
                }
                config.reasoning_effort = Some(effective);
            }
        }
    }
    Ok(config)
}

pub fn in_memory_llm_plugin() -> impl Plugin {
    struct InMemoryLlmPlugin;

    impl Plugin for InMemoryLlmPlugin {
        fn name(&self) -> &'static str {
            "in-memory-llm"
        }

        fn inject(&self) -> Inject {
            Inject::none()
        }

        fn provides(&self) -> &[&'static str] {
            static PROVIDES: [&str; 1] = [<InMemoryLlmRuntime as TypedService>::NAME];
            &PROVIDES
        }

        fn apply(
            &self,
            ctx: &Context,
            bus: &EventBus,
        ) -> cordis_rslite::BoxFuture<anyhow::Result<Vec<EffectHandle>>> {
            let ctx = ctx.clone();
            let bus = bus.clone();
            Box::pin(async move {
                let effect =
                    ctx.provide::<InMemoryLlmRuntime>(Arc::new(InMemoryLlmRuntime::new(bus)))?;
                Ok(vec![effect])
            })
        }
    }

    InMemoryLlmPlugin
}

#[cfg(test)]
mod tests {
    use super::*;
    use cordis_rslite::{PluginRegistry, Value};
    use futures::StreamExt;
    use starriver_kernel::llm::{
        ContentBlock, ContentBlockType, FinishReason, LlmModelContext, LlmModelInfo,
        LlmReasoningEffortInfo, LlmReasoningInfo, LlmResolvedModelInfo, LlmStreamRequest,
        ModelModality, RetryBackoff, StreamChunk,
    };

    struct FakeAdapter;

    impl LlmAdapter for FakeAdapter {
        fn providers(&self) -> &[String] {
            static PROVIDERS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
            PROVIDERS.get_or_init(|| vec!["fake".into()])
        }

        fn stream(
            &self,
            _options: GenerateOptions,
        ) -> BoxStream<'static, Result<StreamChunk, LlmError>> {
            Box::pin(async_stream::stream! {
                yield Ok(StreamChunk::BlockStart {
                    index: 0,
                    block_type: ContentBlockType::Text,
                });
                yield Ok(StreamChunk::TextDelta {
                    index: 0,
                    text: "hello".into(),
                });
                yield Ok(StreamChunk::BlockEnd {
                    index: 0,
                    block: ContentBlock::Text("hello".into()),
                });
                yield Ok(StreamChunk::Finish {
                    reason: FinishReason::Stop,
                    replay_state: None,
                });
            })
        }
    }

    struct CapableAdapter;

    impl LlmAdapter for CapableAdapter {
        fn providers(&self) -> &[String] {
            static PROVIDERS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
            PROVIDERS.get_or_init(|| vec!["capable".into()])
        }

        fn stream(
            &self,
            _options: GenerateOptions,
        ) -> BoxStream<'static, Result<StreamChunk, LlmError>> {
            Box::pin(futures::stream::empty())
        }

        fn provider_retry_policy(&self, _provider: &str) -> Option<RetryPolicy> {
            Some(RetryPolicy::Normal {
                max_retries: 2,
                retryable_codes: vec!["RATE_LIMIT".into()],
                backoff: Default::default(),
            })
        }

        fn list_models(
            &self,
            provider: &str,
        ) -> BoxFuture<'static, Result<Vec<LlmModelInfo>, LlmError>> {
            let provider = provider.to_string();
            Box::pin(async move {
                Ok(vec![LlmModelInfo {
                    provider,
                    id: "m".into(),
                    name: "M".into(),
                    description: None,
                    input_modalities: Some(vec![ModelModality::Text]),
                }])
            })
        }

        fn resolve_model(
            &self,
            provider: &str,
            model: &str,
            _cancellation: Option<CancellationToken>,
        ) -> BoxFuture<'static, Result<LlmResolvedModelInfo, LlmError>> {
            let provider = provider.to_string();
            let model = model.to_string();
            Box::pin(async move {
                Ok(LlmResolvedModelInfo {
                    provider,
                    id: model.clone(),
                    name: model,
                    description: None,
                    input_modalities: None,
                    context: Some(LlmModelContext {
                        context_window: 32_000,
                    }),
                    default_max_tokens: Some(4096),
                    reasoning: Some(LlmReasoningInfo {
                        efforts: vec![
                            LlmReasoningEffortInfo {
                                id: "low".into(),
                                name: "Low".into(),
                                description: None,
                            },
                            LlmReasoningEffortInfo {
                                id: "high".into(),
                                name: "High".into(),
                                description: None,
                            },
                        ],
                        default_effort: Some("low".into()),
                    }),
                })
            })
        }
    }

    fn options(provider: &str) -> GenerateOptions {
        GenerateOptions {
            provider: provider.into(),
            model: "m".into(),
            reasoning_effort: None,
            messages: Vec::new(),
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
        }
    }

    fn config(provider: &str) -> LlmCallConfig {
        LlmCallConfig {
            provider: provider.into(),
            model: "m".into(),
            reasoning_effort: None,
            temperature: None,
            max_tokens: None,
            stop: None,
        }
    }

    #[tokio::test]
    async fn register_adapter_resolves_provider() {
        let bus = EventBus::new();
        let runtime = InMemoryLlmRuntime::new(bus);
        let _handle = runtime.register_adapter(Arc::new(FakeAdapter)).unwrap();
        assert!(runtime
            .resolve_call_config(config("fake"), None)
            .await
            .is_ok());
        assert!(runtime.prepare_call(config("fake"), None).await.is_ok());
    }

    #[tokio::test]
    async fn unknown_provider_is_rejected() {
        let bus = EventBus::new();
        let runtime = InMemoryLlmRuntime::new(bus);
        assert!(matches!(
            runtime.resolve_call_config(config("missing"), None).await,
            Err(LlmError::NoAdapter(provider)) if provider == "missing"
        ));
        assert!(matches!(
            runtime.prepare_call(config("missing"), None).await,
            Err(LlmError::NoAdapter(_))
        ));
        let chunks = runtime.stream(options("missing")).collect::<Vec<_>>().await;
        assert!(
            matches!(chunks.as_slice(), [Err(LlmError::NoAdapter(provider))] if provider == "missing")
        );
    }

    #[tokio::test]
    async fn duplicate_and_invalid_registration_are_rejected() {
        let bus = EventBus::new();
        let runtime = InMemoryLlmRuntime::new(bus);
        let _first = runtime.register_adapter(Arc::new(FakeAdapter)).unwrap();
        let error = match runtime.register_adapter(Arc::new(FakeAdapter)) {
            Ok(_) => panic!("expected duplicate registration to fail"),
            Err(error) => error,
        };
        assert!(matches!(error, LlmError::DuplicateAdapter(provider) if provider == "fake"));
    }

    #[tokio::test]
    async fn adapters_updated_announces_provider_changes() {
        let bus = EventBus::new();
        let runtime = InMemoryLlmRuntime::new(bus.clone());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let _listener = bus.on_sync::<LlmAdaptersUpdated, _>({
            let seen = Arc::clone(&seen);
            move |event| {
                seen.lock().unwrap().push(event.providers.clone());
                Ok(Value::new(()))
            }
        });
        let handle = runtime.register_adapter(Arc::new(FakeAdapter)).unwrap();
        handle.dispose_async().await;
        let snapshots = seen.lock().unwrap().clone();
        assert_eq!(snapshots.len(), 2);
        assert_eq!(snapshots[0], vec!["fake".to_string()]);
        assert!(snapshots[1].is_empty());
    }

    #[tokio::test]
    async fn unregister_removes_provider() {
        let bus = EventBus::new();
        let runtime = InMemoryLlmRuntime::new(bus);
        let handle = runtime.register_adapter(Arc::new(FakeAdapter)).unwrap();
        handle.dispose_async().await;
        assert!(matches!(
            runtime.resolve_call_config(config("fake"), None).await,
            Err(LlmError::NoAdapter(_))
        ));
    }

    #[tokio::test]
    async fn stream_emits_adapter_chunks() {
        let bus = EventBus::new();
        let runtime = InMemoryLlmRuntime::new(bus);
        let _handle = runtime.register_adapter(Arc::new(FakeAdapter)).unwrap();
        let chunks = runtime.stream(options("fake")).collect::<Vec<_>>().await;
        assert_eq!(chunks.len(), 4);
        assert!(matches!(
            chunks.as_slice(),
            [Ok(StreamChunk::BlockStart { .. }), Ok(StreamChunk::TextDelta { text, .. }), Ok(StreamChunk::BlockEnd { .. }), Ok(StreamChunk::Finish { reason: FinishReason::Stop, .. })]
            if text == "hello"
        ));
    }

    #[tokio::test]
    async fn waterfall_wrapper_replaces_stream() {
        let bus = EventBus::new();
        let runtime = InMemoryLlmRuntime::new(bus.clone());
        let _handle = runtime.register_adapter(Arc::new(FakeAdapter)).unwrap();
        let _listener = bus.on_waterfall::<LlmStreamRequest, _>(move |_, _next| {
            Box::pin(async {
                let stream: BoxStream<'static, Result<StreamChunk, LlmError>> =
                    Box::pin(async_stream::stream! {
                        yield Ok(StreamChunk::TextDelta {
                            index: 0,
                            text: "wrapped".into(),
                        });
                        yield Ok(StreamChunk::Finish {
                            reason: FinishReason::Stop,
                            replay_state: None,
                        });
                    });
                Ok(Arc::new(tokio::sync::Mutex::new(stream)))
            })
        });
        let chunks = runtime.stream(options("fake")).collect::<Vec<_>>().await;
        assert!(matches!(
            chunks.as_slice(),
            [Ok(StreamChunk::TextDelta { text, .. }), Ok(StreamChunk::Finish { .. })]
            if text == "wrapped"
        ));
    }

    #[tokio::test]
    async fn prepared_call_goes_through_waterfall() {
        let bus = EventBus::new();
        let runtime = InMemoryLlmRuntime::new(bus.clone());
        let _handle = runtime.register_adapter(Arc::new(FakeAdapter)).unwrap();
        let _listener = bus.on_waterfall::<LlmStreamRequest, _>(move |_, _next| {
            Box::pin(async {
                let stream: BoxStream<'static, Result<StreamChunk, LlmError>> =
                    Box::pin(async_stream::stream! {
                        yield Ok(StreamChunk::TextDelta {
                            index: 0,
                            text: "prepared-wrapped".into(),
                        });
                        yield Ok(StreamChunk::Finish {
                            reason: FinishReason::Stop,
                            replay_state: None,
                        });
                    });
                Ok(Arc::new(tokio::sync::Mutex::new(stream)))
            })
        });
        let prepared = runtime.prepare_call(config("fake"), None).await.unwrap();
        let chunks = prepared.stream(options("fake")).collect::<Vec<_>>().await;
        assert!(matches!(
            chunks.as_slice(),
            [Ok(StreamChunk::TextDelta { text, .. }), Ok(StreamChunk::Finish { .. })]
            if text == "prepared-wrapped"
        ));
    }

    #[tokio::test]
    async fn replace_adapter_swaps_same_provider_routes() {
        struct ReplacedAdapter;
        impl LlmAdapter for ReplacedAdapter {
            fn providers(&self) -> &[String] {
                static PROVIDERS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
                PROVIDERS.get_or_init(|| vec!["fake".into()])
            }

            fn stream(
                &self,
                _options: GenerateOptions,
            ) -> BoxStream<'static, Result<StreamChunk, LlmError>> {
                Box::pin(async_stream::stream! {
                    yield Ok(StreamChunk::TextDelta {
                        index: 0,
                        text: "replaced".into(),
                    });
                    yield Ok(StreamChunk::Finish {
                        reason: FinishReason::Stop,
                        replay_state: None,
                    });
                })
            }
        }

        let bus = EventBus::new();
        let runtime = InMemoryLlmRuntime::new(bus);
        let _first = runtime.register_adapter(Arc::new(FakeAdapter)).unwrap();
        let _second = runtime
            .replace_adapter(vec!["fake".into()], Arc::new(ReplacedAdapter))
            .unwrap();
        assert_eq!(runtime.list_providers().len(), 1);
        let chunks = runtime.stream(options("fake")).collect::<Vec<_>>().await;
        assert!(matches!(
            chunks.as_slice(),
            [Ok(StreamChunk::TextDelta { text, .. }), ..] if text == "replaced"
        ));
    }

    #[tokio::test]
    async fn list_providers_preserves_registration_order() {
        struct SecondAdapter;
        impl LlmAdapter for SecondAdapter {
            fn providers(&self) -> &[String] {
                static PROVIDERS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
                PROVIDERS.get_or_init(|| vec!["second".into()])
            }

            fn stream(
                &self,
                _options: GenerateOptions,
            ) -> BoxStream<'static, Result<StreamChunk, LlmError>> {
                Box::pin(futures::stream::empty())
            }
        }

        let bus = EventBus::new();
        let runtime = InMemoryLlmRuntime::new(bus);
        let _first = runtime.register_adapter(Arc::new(FakeAdapter)).unwrap();
        let _second = runtime.register_adapter(Arc::new(SecondAdapter)).unwrap();
        let providers = runtime.list_providers();
        assert_eq!(
            providers
                .iter()
                .map(|info| info.id.as_str())
                .collect::<Vec<_>>(),
            vec!["fake", "second"]
        );
    }

    #[tokio::test]
    async fn invalid_provider_retry_policy_is_rejected() {
        struct BadPolicyAdapter;
        impl LlmAdapter for BadPolicyAdapter {
            fn providers(&self) -> &[String] {
                static PROVIDERS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
                PROVIDERS.get_or_init(|| vec!["bad".into()])
            }

            fn stream(
                &self,
                _options: GenerateOptions,
            ) -> BoxStream<'static, Result<StreamChunk, LlmError>> {
                Box::pin(futures::stream::empty())
            }

            fn provider_retry_policy(&self, _provider: &str) -> Option<RetryPolicy> {
                Some(RetryPolicy::Normal {
                    max_retries: 1,
                    retryable_codes: vec!["RATE_LIMIT".into(), "RATE_LIMIT".into()],
                    backoff: RetryBackoff {
                        initial_delay_ms: 10_000,
                        max_delay_ms: 1_000,
                        jitter_ratio: 0.5,
                    },
                })
            }
        }

        let bus = EventBus::new();
        let runtime = InMemoryLlmRuntime::new(bus);
        let error = match runtime.register_adapter(Arc::new(BadPolicyAdapter)) {
            Ok(_) => panic!("expected invalid retry policy to be rejected"),
            Err(error) => error,
        };
        assert!(matches!(error, LlmError::InvalidAdapter(message) if message.contains("backoff")));
    }

    #[tokio::test]
    async fn prepared_call_dispatches_once() {
        let bus = EventBus::new();
        let runtime = InMemoryLlmRuntime::new(bus);
        let _handle = runtime.register_adapter(Arc::new(FakeAdapter)).unwrap();
        let prepared = runtime.prepare_call(config("fake"), None).await.unwrap();
        assert!(prepared.config.provider == "fake");
        let chunks = prepared.stream(options("fake")).collect::<Vec<_>>().await;
        assert!(!chunks.is_empty());
        let chunks = prepared.stream(options("fake")).collect::<Vec<_>>().await;
        assert!(matches!(
            chunks.as_slice(),
            [Err(LlmError::Call { code, .. })] if code == "INVALID_PREPARED_CALL"
        ));
    }

    #[tokio::test]
    async fn prepared_call_rejects_config_change() {
        let bus = EventBus::new();
        let runtime = InMemoryLlmRuntime::new(bus);
        let _handle = runtime.register_adapter(Arc::new(FakeAdapter)).unwrap();
        let prepared = runtime.prepare_call(config("fake"), None).await.unwrap();
        let mut changed = options("fake");
        changed.model = "different".into();
        let chunks = prepared.stream(changed).collect::<Vec<_>>().await;
        assert!(matches!(
            chunks.as_slice(),
            [Err(LlmError::Call { code, .. })] if code == "INVALID_PREPARED_CALL"
        ));
    }

    #[tokio::test]
    async fn capability_surface_materializes_defaults_and_context() {
        let bus = EventBus::new();
        let runtime = InMemoryLlmRuntime::new(bus);
        let _handle = runtime.register_adapter(Arc::new(CapableAdapter)).unwrap();

        assert_eq!(runtime.list_providers().len(), 1);
        let policy = runtime.provider_retry_policy("capable").unwrap();
        assert!(matches!(policy, RetryPolicy::Normal { max_retries: 2, .. }));

        let models = runtime.list_models("capable").await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].input_modalities, Some(vec![ModelModality::Text]));

        let info = runtime
            .resolve_model_info("capable", "m", None)
            .await
            .unwrap();
        assert_eq!(info.context.unwrap().context_window, 32_000);
        assert_eq!(info.default_max_tokens, Some(4096));

        let resolved = runtime
            .resolve_call_config(config("capable"), None)
            .await
            .unwrap();
        assert_eq!(resolved.max_tokens, Some(4096));
        assert_eq!(resolved.reasoning_effort.as_deref(), Some("low"));

        let prepared = runtime.prepare_call(config("capable"), None).await.unwrap();
        assert!(prepared.adapter_defaults.max_tokens);
        assert!(prepared.adapter_defaults.reasoning_effort);
        assert_eq!(prepared.context.unwrap().context_window, 32_000);
    }

    #[tokio::test]
    async fn unsupported_reasoning_effort_is_rejected() {
        let bus = EventBus::new();
        let runtime = InMemoryLlmRuntime::new(bus);
        let _handle = runtime.register_adapter(Arc::new(CapableAdapter)).unwrap();
        let mut config = config("capable");
        config.reasoning_effort = Some("extreme".into());
        let error = runtime.resolve_call_config(config, None).await.unwrap_err();
        assert!(matches!(
            error,
            LlmError::Call { code, .. } if code == "UNSUPPORTED_REASONING_EFFORT"
        ));
    }

    #[tokio::test]
    async fn configurable_providers_and_discovery_roundtrip() {
        let bus = EventBus::new();
        let runtime = InMemoryLlmRuntime::new(bus);
        let _handle = runtime
            .register_configurable_providers(vec![LlmConfigurableProvider {
                provider: "gateway".into(),
                display_name: "Gateway".into(),
                settings_ns: "providers".into(),
                settings_path: vec!["gateway".into()],
                declared: Some(true),
            }])
            .unwrap();
        assert_eq!(runtime.list_configurable_providers().len(), 1);

        let _discovery = runtime
            .register_model_discovery(
                "providers".into(),
                Arc::new(|_request| {
                    Box::pin(async move {
                        Ok(vec![
                            LlmDiscoveredModel {
                                id: "model-a".into(),
                                name: Some("Model A".into()),
                                context_window: Some(8000),
                                max_tokens: None,
                            },
                            LlmDiscoveredModel {
                                id: "model-a".into(),
                                name: None,
                                context_window: None,
                                max_tokens: None,
                            },
                        ])
                    })
                }),
            )
            .unwrap();
        let models = runtime
            .discover_models(
                "providers".into(),
                LlmModelDiscoveryRequest {
                    provider: Some("gateway".into()),
                    base_url: None,
                    api: None,
                    api_key: None,
                    cancellation: None,
                },
            )
            .await
            .unwrap();
        // Duplicate ids are deduplicated.
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "model-a");
    }

    #[tokio::test]
    async fn plugin_provides_llm_runtime() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let registry = PluginRegistry::new();
        registry.register(in_memory_llm_plugin());
        let handles = registry.boot(&ctx, &bus).await.unwrap();
        assert!(ctx.require::<InMemoryLlmRuntime>().is_ok());
        assert_eq!(handles.len(), 1);
    }
}
