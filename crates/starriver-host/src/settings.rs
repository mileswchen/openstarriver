//! Settings file loading: reads `~/.starriver/settings.yaml`, merges with
//! defaults, and produces the typed settings consumed by the kernel.
//!
//! `packages/settings/settings-file` layered model (base + user),
//! simplified to a single user file for starriver.

fn default_true() -> bool {
    true
}

/// Prompt composition settings from the `prompt:` namespace.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct PromptSection {
    #[serde(default = "default_true")]
    pub include_identity: bool,
    #[serde(default = "default_true")]
    pub include_runtime_context: bool,
    pub source_path: Option<String>,
    pub persona_global: Option<String>,
    pub instructions_max_bytes: Option<usize>,
    /// Pin the reply language for every session (e.g. `zh`). Unset → the
    /// `language` resident asks the model to follow the user's messages.
    pub response_language: Option<String>,
 /// `toolOrder` parity: presentation order for `<tool>:guide` sections.
    /// Must contain `<unlisted-tools>` exactly once (validated on write).
    pub tool_order: Option<Vec<String>>,
}

impl Default for PromptSection {
    fn default() -> Self {
        Self {
            include_identity: true,
            include_runtime_context: true,
            source_path: None,
            persona_global: None,
            instructions_max_bytes: None,
            response_language: None,
            tool_order: None,
        }
    }
}

/// Agent-level settings from the `agent:` namespace.
#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct AgentSection {
    pub time_refresh_interval_ms: Option<u64>,
    pub context_window: Option<u64>,
    pub compaction_threshold_ratio: Option<f64>,
    /// Per-step parallel tool-call budget (backlog #7 hot settings).
    pub max_parallel_tool_calls: Option<usize>,
    /// BUG-157: extra environment for tool subprocesses (bash). Values
    /// support one-level `$VAR` / `${VAR}` expansion against the daemon
    /// process environment, applied once at tool registration.
    pub tool_env: Option<std::collections::BTreeMap<String, String>>,
    /// BUG-138 repeat breaker: total non-reload tool calls allowed per turn
    /// (turnToolCallBudget). Unset keeps the engine default (666).
    pub turn_tool_call_budget: Option<u32>,
    /// WI-199: escalation rung ③ fallback route, `provider/model`. When the
    /// SAME step fails 3+ times the ladder condenses, then downgrades
    /// reasoning, then reroutes here. Unset = no fallback rung.
    pub fallback_route: Option<String>,
 /// WI-10: compaction retain ratio (retainRatio — passed straight to
    /// the retain split; default 0.16). Must be 0.0..=1.0.
    pub compaction_retain_ratio: Option<f64>,
 /// WI-10: tool-output spill threshold in bytes (maxInlineBytes).
    /// Unset keeps the 32 KiB engine default.
    pub spill_threshold_bytes: Option<usize>,
    /// WI-159: image request budget — count cap on hot images per request
 /// (reload-pulls included; maxImagesPerRequest). Unset keeps the
    /// engine default (8).
    pub image_max_request_images: Option<usize>,
    /// WI-159: image request budget — byte cap counted in base64 wire
 /// length (maxRequestFilesBytes parity). Unset keeps the engine
    /// default (4 MiB).
    pub image_max_request_bytes: Option<u64>,
    /// WI-1: register the `reload_image` tool (models can pull a historical
    /// image back into context). Unset = enabled.
    pub enable_reload_image: Option<bool>,
    /// WI-9 pruner: trim a tool result when its text exceeds this many
 /// Unicode code points (thresholdChars, default 8192).
    pub prune_threshold_chars: Option<usize>,
 /// WI-9 pruner: leading code points retained (headChars, 4096).
    pub prune_head_chars: Option<usize>,
 /// WI-9 pruner: trailing code points retained (tailChars, 1024).
    pub prune_tail_chars: Option<usize>,
    /// WI-9: extra condense attempts when pressure remains after one
 /// condense (compactionRetries, default 1).
    pub compaction_retries: Option<u32>,
 /// WI-9: retries after a confirmed context-window overflow (
    /// maxOverflowRetries, default 1; 0 disables recovery).
    pub max_overflow_retries: Option<u32>,
    /// WI-152 S4: how long a learned "this route rejects images" memory stays
    /// in force (hours). Unset → 168 (7 days); `0` keeps it forever.
    pub vision_error_memory_ttl_hours: Option<u64>,
    /// WI-7 @file: per-file expansion budget in bytes (default 32 KiB).
    pub file_ref_max_bytes: Option<usize>,
    /// WI-7 @file: cumulative expansion budget per message (default 96 KiB).
    pub file_ref_total_max_bytes: Option<usize>,
    /// WI-6: register the `session_reference` tool (models can quote another
    /// conversation's surface). Unset = enabled.
    pub enable_session_reference: Option<bool>,
    /// Pending-approval timeout in seconds (WI-16): a request no client
    /// answers within this window is denied. Unset → the bridge default
    /// (300 s); `0` disables the timeout (wait indefinitely).
    pub approval_timeout_secs: Option<u64>,
    /// BUG-101: default bash tool timeout in seconds. Unset → 120 (2 min).
    pub bash_timeout_secs: Option<u64>,
    /// BUG-101: hard cap the model-requested bash timeout is clamped to.
    /// Unset → 1800 (30 min).
    pub bash_timeout_max_secs: Option<u64>,
    /// Each agent's last activated workspace id (agent name → workspace id).
    /// switch_agent re-seeds the working dir from it after a restart, and the UI
    /// restores its toolbar/context display from it too.
    pub active_workspaces: std::collections::HashMap<String, String>,
}

impl AgentSection {
    /// WI-9: fold the pruner/retry knobs into the kernel tuning struct.
    /// `None` when nothing is configured (engine defaults apply).
    pub fn compaction_tuning(&self) -> Option<starriver_kernel::agent::CompactionTuning> {
        let has_any = self.prune_threshold_chars.is_some()
            || self.prune_head_chars.is_some()
            || self.prune_tail_chars.is_some()
            || self.compaction_retries.is_some()
            || self.max_overflow_retries.is_some();
        if !has_any {
            return None;
        }
        Some(starriver_kernel::agent::CompactionTuning {
            prune_threshold_chars: self.prune_threshold_chars,
            prune_head_chars: self.prune_head_chars,
            prune_tail_chars: self.prune_tail_chars,
            compaction_retries: self.compaction_retries,
            max_overflow_retries: self.max_overflow_retries,
        })
    }
}

/// Provider-route declarations from the `llm:` namespace.
///
/// LLP §6 settings slice: adapter routes are declared in the
/// plugin's own settings area and the legacy model registry (`models.yaml`)
/// only keeps activation state. Each entry mirrors a `ProviderProfile`
/// (route + credential reference + model directory); the credential field
/// stores a reference (`!file:<account>` / `${ENV}`) rather than a literal
/// key wherever possible.
#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct LlmSection {
    pub providers: Vec<LlmProviderSettings>,
    /// Current execution mode (executionMode): standard / plan / ptc / goal.
    pub execution_mode: String,
    /// WI-102: image-generation route `providerId/variantId`. The
    /// `generate_image` tool resolves this before falling back to the first
    /// cloud provider's default model.
    pub image_model: Option<String>,
}

/// Subagent delegation preferences from the `subagents:` namespace (
/// `subagent authorize selectable child models` parity): each built-in
/// template can be enabled and bound to a model key (`provider/variant`).
#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct SubagentSection {
    pub entries: Vec<SubagentSettings>,
}

#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct SubagentSettings {
    pub name: String,
    pub enabled: bool,
    /// Model key `providerId/variantId`; empty = the active agent model.
    /// Ignored for `acp` entries (the external agent carries its own model).
    pub model: Option<String>,
    /// WI-112: external ACP agent declaration — when present, this template
    /// spawns the given binary as a child process (ACP over stdio) instead
    /// of an in-process sub-session.
    pub acp: Option<SubagentAcpAgent>,
}

/// One external ACP agent (WI-112): `command args…`, permission policy.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq)]
#[serde(default)]
pub struct SubagentAcpAgent {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Answer the agent's permission asks with allow_once instead of reject.
    /// Default false — an untrusted binary gets no tool access.
    pub trust: bool,
}

impl Default for SubagentAcpAgent {
    fn default() -> Self {
        Self {
            command: String::new(),
            args: Vec::new(),
            trust: false,
        }
    }
}

/// One provider route declaration (the settings-side twin of the adapter
/// `ProviderProfile` and the UI `ModelEntry`).
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct LlmProviderSettings {
    pub id: String,
    pub name: String,
    /// "cloud" | "local"
    pub model_type: String,
    /// "openai-compat" | "ollama" | "lmstudio" | "llama"
    pub provider_type: String,
    pub base_url: String,
    /// Credential reference (`!file:<account>` / `${ENV}`) or a literal key
    /// (legacy; the host resolves references through the credential resolver).
    pub api_key: String,
    pub model_path: Option<String>,
    pub models: Vec<LlmVariantSettings>,
    pub default_model: String,
    pub concurrency: Option<usize>,
    pub max_tokens: Option<u32>,
    pub context_window: Option<usize>,
    pub temperature: Option<f32>,
    /// "openai-chat" (default) | "openai-responses" | "anthropic"
    pub api_format: String,
    pub reasoning_effort: Option<String>,
    /// Custom model ID shown as the PREFIX of model names in selectors
    /// (composer dropdown et al.), replacing the full provider display name.
    /// None/blank = fall back to `name`.
    pub model_label: Option<String>,
    /// Legacy, ignored (WI-152 v2): image-input capability is resolved
    /// automatically from the built-in provider table; this stored value has
    /// no effect. Kept so existing settings files and napi signatures stay
    /// stable.
    pub supports_vision: Option<bool>,
}

impl Default for LlmProviderSettings {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            model_type: "cloud".into(),
            provider_type: "openai-compat".into(),
            base_url: String::new(),
            api_key: String::new(),
            model_path: None,
            models: Vec::new(),
            default_model: String::new(),
            concurrency: None,
            max_tokens: None,
            context_window: None,
            temperature: None,
            api_format: "openai-chat".into(),
            reasoning_effort: None,
            model_label: None,
            supports_vision: None,
        }
    }
}

/// WI-201: builtin context windows for KNOWN cloud models, consulted when a
/// provider entry does not declare `context_window` itself. The 128k platform
/// default lied about GLM Coding Plan routes: the official spec for
/// glm-5.3/glm-5.3-flash/glm-5.3-flashx is a 1M window with 128K max output
/// (docs.bigmodel.cn, GLM-5.3-Flash/FlashX), and a live probe accepted
/// 1,001,125 prompt tokens with 200 OK. The DECLARED values are
/// deliberately bounded for the agent loop — it re-sends the full context on
/// every call, and a 1M-derived compaction threshold would let context bloat
/// to 800k tokens before condensing (the 2026-10-14 incident's 102k poisoned
/// context amplified its failure cost 8×). 200k keeps the 0.8× trigger at
/// 160k with a ≥32k output reserve. An explicit entry declaration still wins.
pub fn builtin_context_window(provider: &str, model: &str) -> Option<u64> {
    let model = model.to_ascii_lowercase();
    let window: u64 = match provider {
        // helpers.rs preset id ("zhipu") and the admin-created alias ("glm",
        // "GLM Coding Plan - CN") must resolve identically.
        "zhipu" | "glm" => match model.as_str() {
            "glm-5.3" | "glm-5.3-flash" | "glm-5.3-flashx" => 200_000,
            "glm-4-plus" | "glm-4-flash" => 128_000,
            _ => return None,
        },
        _ => return None,
    };
    Some(window)
}

impl LlmProviderSettings {
    /// Merge optional fields into a concrete GenerationConfig (same semantics
    /// as the legacy `model_registry::ModelEntry::generation_config`).
    pub fn generation_config(&self) -> starriver_kernel::types::GenerationConfig {
        starriver_kernel::types::GenerationConfig {
            max_tokens: self.max_tokens.unwrap_or(8192),
            temperature: self.temperature.unwrap_or(0.7),
            context_window: self
                .context_window
                .or_else(|| {
                    builtin_context_window(&self.id, &self.default_model).map(|w| w as usize)
                })
                .unwrap_or(starriver_kernel::llm::DEFAULT_MODEL_CONTEXT_WINDOW),
            api_format: self.api_format.clone(),
            reasoning_effort: self.reasoning_effort.clone(),
        }
    }

    /// Find a variant by id. No fallback: a miss returns `None` (callers
    /// that want the provider's default apply `unwrap_or(default_model)`
    /// themselves — see admin2's model-binding projection).
    pub fn resolve_variant(&self, variant_id: &str) -> Option<&LlmVariantSettings> {
        self.models.iter().find(|v| v.id == variant_id)
    }
}

/// One model variant under a provider route's directory.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct LlmVariantSettings {
    pub id: String,
    pub name: String,
}

/// Language-server registry overrides (`lsp.servers` in settings.yaml).
/// An entry replaces the builtin with the same id; others extend the
/// registry. Extensions are dot-less ("ts"), root markers mark a project
/// root (topmost ancestor wins).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct LspSettings {
    #[serde(default)]
    pub servers: Vec<LspServerSettings>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LspServerSettings {
    pub id: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub extensions: Vec<String>,
    #[serde(default)]
    pub root_markers: Vec<String>,
    /// LSP language id for didOpen (defaults to the server id).
    #[serde(default)]
    pub language_id: Option<String>,
}

/// External-app launch commands (`apps:` namespace): templates the desktop
/// shell uses when the toolbar asks to open the workspace in a terminal / IDE.
/// `{dir}` is the workspace directory placeholder; unset → platform defaults
/// (Terminal.app / `code` CLI probe). Electron main performs the spawn.
#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct AppsSection {
    /// e.g. `open -a iTerm '{dir}'`.
    pub terminal_command: Option<String>,
    /// e.g. `cursor wait '{dir}'`.
    pub ide_command: Option<String>,
}

/// Root settings document.
#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct Settings {
    pub prompt: PromptSection,
    pub agent: AgentSection,
    pub llm: LlmSection,
    pub lsp: LspSettings,
    pub subagents: SubagentSection,
    pub apps: AppsSection,
}

impl Settings {
    /// Load from `~/.starriver/settings.yaml`. Missing file → all defaults.
    pub fn load() -> Self {
        Self::from_file(&Self::default_path())
    }

    pub fn from_file(path: &std::path::Path) -> Self {
        let Ok(content) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        match serde_yaml::from_str::<Settings>(&content) {
            Ok(settings) => settings,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "settings parse failed, using defaults");
                // A later save() would otherwise silently OVERWRITE the
                // corrupt file with defaults+patch (providers etc. gone).
                // Park the corrupt bytes next to it for manual recovery.
                let backup = path.with_extension("yaml.corrupt");
                if std::fs::copy(path, &backup).is_ok() {
                    tracing::warn!(backup = %backup.display(), "corrupt settings preserved");
                }
                Self::default()
            }
        }
    }

    fn default_path() -> std::path::PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        std::path::PathBuf::from(home)
            .join(".starriver")
            .join("settings.yaml")
    }

    /// Persist the settings document back to its default path. Provider
    /// route declarations are written here (LLP §6 settings slice).
    pub fn save(&self) -> anyhow::Result<()> {
        self.save_to(&Self::default_path())
    }

    /// Hot settings update (backlog #7): deep-merge `patch` into the current
    /// document, re-validate by deserializing back into [`Settings`], and
    /// return the merged settings. The caller swaps the merged value into the
    /// live lock and persists; invalid patches are rejected wholesale (the
    /// current settings stay untouched).
    pub fn update_from_patch(&self, patch: &serde_json::Value) -> anyhow::Result<Settings> {
        let current = serde_json::to_value(self)?;
        let merged = merge_json_patch(current, patch);
        let updated: Settings = serde_json::from_value(merged)
            .map_err(|e| anyhow::anyhow!("invalid settings patch: {e}"))?;
        updated.validate()?;
        Ok(updated)
    }

 /// Cross-field validation beyond serde (toolOrder parity: the kernel
    /// assembler rejects a guide order without the `<unlisted-tools>` rest
    /// sentinel exactly once — catch it here so a bad patch is rejected
    /// wholesale instead of silently falling back at every run).
    fn validate(&self) -> anyhow::Result<()> {
        if let Some(order) = &self.prompt.tool_order {
            let rest = order
                .iter()
                .filter(|name| **name == starriver_kernel::system_prompt::UNLISTED_TOOLS)
                .count();
            if rest != 1 {
                anyhow::bail!(
                    "prompt.tool_order must contain \"{}\" exactly once (found {rest})",
                    starriver_kernel::system_prompt::UNLISTED_TOOLS
                );
            }
        }
        Ok(())
    }

    /// Persist the settings document to an explicit path (tests / migration).
    pub fn save_to(&self, path: &std::path::Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let yaml = serde_yaml::to_string(self)?;
        std::fs::write(path, yaml)?;
        Ok(())
    }

    /// Convert to kernel `PromptSettings` for AgentOptions.
    pub fn to_prompt_settings(&self) -> starriver_kernel::system_prompt::PromptSettings {
        starriver_kernel::system_prompt::PromptSettings {
            include_identity: self.prompt.include_identity,
            include_runtime_context: self.prompt.include_runtime_context,
            source_path: self.prompt.source_path.clone(),
            persona_global: self.prompt.persona_global.clone(),
            instructions_max_bytes: self.prompt.instructions_max_bytes,
            response_language: self.prompt.response_language.clone(),
            tool_order: self.prompt.tool_order.clone(),
        }
    }

    /// Instructions budget override, if set.
    pub fn instructions_max_bytes(&self) -> Option<usize> {
        self.prompt.instructions_max_bytes
    }

    /// Time-context interval from settings.
    pub fn time_refresh_interval_ms(&self) -> Option<u64> {
        self.agent.time_refresh_interval_ms
    }

    /// Context window for compaction.
    pub fn context_window(&self) -> Option<u64> {
        self.agent.context_window
    }

    /// Compaction threshold ratio.
    pub fn compaction_threshold_ratio(&self) -> Option<f64> {
        self.agent.compaction_threshold_ratio
    }
}

/// Load settings from the default path (or a temp dir if HOME is unset).
/// Recursive JSON merge (RFC 7386 flavor): objects merge key-by-key, every
/// other patch type replaces the base value wholesale; `null` in the patch
/// sets an explicit null (settings fields are `Option`/bool, so null means
/// "clear").
pub fn merge_json_patch(
    mut base: serde_json::Value,
    patch: &serde_json::Value,
) -> serde_json::Value {
    match (base.as_object_mut(), patch.as_object()) {
        (Some(base_map), Some(patch_map)) => {
            for (key, value) in patch_map {
                match base_map.get_mut(key) {
                    Some(existing) if existing.is_object() && value.is_object() => {
                        let existing = existing.take();
                        base_map.insert(key.clone(), merge_json_patch(existing, value));
                    }
                    _ => {
                        base_map.insert(key.clone(), value.clone());
                    }
                }
            }
            base
        }
        _ => patch.clone(),
    }
}

pub fn load_settings() -> Settings {
    let home = std::env::var("HOME").ok().map(std::path::PathBuf::from);
    let path = home
        .map(|h| h.join(".starriver").join("settings.yaml"))
        .unwrap_or_else(|| std::path::PathBuf::from(".starriver/settings.yaml"));
    Settings::from_file(&path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_returns_defaults() {
        let settings = Settings::from_file(std::path::Path::new("/nonexistent/settings.yaml"));
        assert!(settings.prompt.include_identity);
        assert!(settings.prompt.include_runtime_context);
    }

    #[test]
    fn yaml_parsing_extracts_all_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.yaml");
        std::fs::write(&path, "prompt:\n  include_identity: false\n  include_runtime_context: false\n  source_path: /src\n  persona_global: |\n    You are helpful.\n  instructions_max_bytes: 32768\nagent:\n  time_refresh_interval_ms: 120000\n  context_window: 65536\n  compaction_threshold_ratio: 0.9\nllm:\n  providers:\n    - id: deepseek\n      name: DeepSeek\n      model_type: cloud\n      provider_type: openai-compat\n      base_url: https://api.deepseek.com\n      api_key: \"!file:model/deepseek\"\n      default_model: deepseek-chat\n      api_format: openai-chat\n").unwrap();
        let settings = Settings::from_file(&path);
        assert!(!settings.prompt.include_identity);
        assert!(!settings.prompt.include_runtime_context);
        assert_eq!(settings.prompt.source_path.as_deref(), Some("/src"));
        assert_eq!(
            settings.prompt.persona_global.as_deref(),
            Some("You are helpful.\n")
        );
        assert_eq!(settings.prompt.instructions_max_bytes, Some(32768));
        assert_eq!(settings.agent.time_refresh_interval_ms, Some(120000));
        assert_eq!(settings.agent.context_window, Some(65536));
        assert_eq!(settings.agent.compaction_threshold_ratio, Some(0.9));
        assert_eq!(settings.llm.providers.len(), 1);
        assert_eq!(settings.llm.providers[0].id, "deepseek");
        assert_eq!(settings.llm.providers[0].api_key, "!file:model/deepseek");
        assert_eq!(settings.llm.providers[0].api_format, "openai-chat");
    }

    #[test]
    fn to_prompt_settings_maps_correctly() {
        let settings = Settings {
            lsp: Default::default(),
            prompt: PromptSection {
                include_identity: false,
                include_runtime_context: true,
                source_path: Some("/src".into()),
                persona_global: Some("global".into()),
                instructions_max_bytes: Some(1024),
                response_language: Some("zh".into()),
                tool_order: None,
            },
            agent: AgentSection::default(),
            llm: LlmSection::default(),
            subagents: SubagentSection::default(),
            apps: AppsSection {
                ide_command: Some("code '{dir}'".into()),
                ..Default::default()
            },
        };
        let ps = settings.to_prompt_settings();
        assert_eq!(settings.apps.ide_command.as_deref(), Some("code '{dir}'"));
        assert!(!ps.include_identity);
        assert!(ps.include_runtime_context);
        assert_eq!(ps.source_path.as_deref(), Some("/src"));
        assert_eq!(ps.persona_global.as_deref(), Some("global"));
    }

    #[test]
    fn save_roundtrip_preserves_llm_providers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.yaml");
        let settings = Settings {
            lsp: Default::default(),
            prompt: PromptSection::default(),
            agent: AgentSection::default(),
            apps: AppsSection::default(),
            llm: LlmSection {
                execution_mode: "standard".into(),
                image_model: None,
                providers: vec![LlmProviderSettings {
                    id: "ds".into(),
                    name: "DeepSeek".into(),
                    model_type: "cloud".into(),
                    provider_type: "openai-compat".into(),
                    base_url: "https://api.deepseek.com".into(),
                    api_key: "!file:model/ds".into(),
                    model_path: None,
                    models: vec![LlmVariantSettings {
                        id: "deepseek-chat".into(),
                        name: "Chat".into(),
                    }],
                    default_model: "deepseek-chat".into(),
                    concurrency: None,
                    max_tokens: None,
                    context_window: None,
                    temperature: None,
                    api_format: "openai-chat".into(),
                    reasoning_effort: None,
                    model_label: Some("DS".into()),
                    supports_vision: Some(false),
                }],
            },
            subagents: SubagentSection::default(),
        };
        settings.save_to(&path).unwrap();
        let reloaded = Settings::from_file(&path);
        assert_eq!(reloaded.llm.providers.len(), 1);
        assert_eq!(reloaded.llm.providers[0].api_key, "!file:model/ds");
        assert_eq!(reloaded.llm.providers[0].models[0].id, "deepseek-chat");
        assert_eq!(reloaded.llm.providers[0].model_label.as_deref(), Some("DS"));
    }

    fn default_true() -> bool {
        true
    }

    #[test]
    fn merge_json_patch_merges_objects_and_replaces_scalars() {
        // Settings keys use the snake_case file dialect (settings.yaml keys).
        let base = serde_json::json!({
            "agent": { "max_parallel_tool_calls": 4, "approval_timeout_secs": 300 },
            "prompt": { "include_identity": true }
        });
        let merged = super::merge_json_patch(
            base,
            &serde_json::json!({
                "agent": { "max_parallel_tool_calls": 8 },
                "prompt": { "include_identity": false, "source_path": "/src" }
            }),
        );
        assert_eq!(merged["agent"]["max_parallel_tool_calls"], 8);
        assert_eq!(
            merged["agent"]["approval_timeout_secs"], 300,
            "unrelated keys survive"
        );
        assert_eq!(merged["prompt"]["include_identity"], false);
        assert_eq!(merged["prompt"]["source_path"], "/src");
    }

    /// BUG-101: the bash timeout knobs parse from the agent section and
    /// survive a patch round-trip.
    #[test]
    fn agent_tool_env_parses_and_overrides() {
        // BUG-157: agent.tool_env parses from the agent section and follows
        // the existing override semantics on merge.
        let parsed = serde_json::from_value::<super::AgentSection>(serde_json::json!({
            "tool_env": { "PATH": "$HOME/.cargo/bin:$PATH", "CARGO_TARGET_DIR": "/tmp/h2target" }
        }))
        .expect("tool_env parses");
        let env = parsed.tool_env.expect("some");
        assert_eq!(env.get("PATH").unwrap(), "$HOME/.cargo/bin:$PATH");
        assert_eq!(env.get("CARGO_TARGET_DIR").unwrap(), "/tmp/h2target");

        // Unset → None (historical behavior).
        let empty = serde_json::from_value::<super::AgentSection>(serde_json::json!({}))
            .expect("empty agent section parses");
        assert!(empty.tool_env.is_none());

        let merged = super::merge_json_patch(
            serde_json::json!({ "agent": { "tool_env": { "A": "1", "B": "2" } } }),
            &serde_json::json!({ "agent": { "tool_env": { "B": "3" } } }),
        );
        assert_eq!(merged["agent"]["tool_env"]["A"], "1", "unrelated keys survive");
        assert_eq!(merged["agent"]["tool_env"]["B"], "3", "patched key overrides");
    }
    /// BUG-101: the bash timeout knobs parse from the agent section and
    /// survive a patch round-trip.
    #[test]
    fn bash_timeout_fields_parse_and_patch() {
        let base = serde_json::json!({
            "agent": { "bash_timeout_secs": 120, "bash_timeout_max_secs": 1800 }
        });
        let parsed: Settings = serde_json::from_value(base).expect("parse");
        assert_eq!(parsed.agent.bash_timeout_secs, Some(120));
        assert_eq!(parsed.agent.bash_timeout_max_secs, Some(1800));

        let settings = Settings::default();
        assert_eq!(settings.agent.bash_timeout_secs, None, "unset = engine default");
        let updated = settings
            .update_from_patch(&serde_json::json!({
                "agent": { "bash_timeout_max_secs": 600 }
            }))
            .expect("patch");
        assert_eq!(updated.agent.bash_timeout_max_secs, Some(600));
        assert_eq!(
            updated.agent.bash_timeout_secs, None,
            "untouched field survives"
        );
    }

    /// BUG-138: the repeat breaker's per-turn tool-call budget parses from
    /// the agent section and survives a patch round-trip (default 666).
    #[test]
    fn turn_tool_call_budget_parses_and_patches() {
        let parsed: Settings =
            serde_json::from_value(serde_json::json!({ "agent": { "turn_tool_call_budget": 200 } }))
                .expect("parse");
        assert_eq!(parsed.agent.turn_tool_call_budget, Some(200));

        let settings = Settings::default();
        assert_eq!(
            settings.agent.turn_tool_call_budget, None,
            "unset = engine default (666)"
        );
        let updated = settings
            .update_from_patch(&serde_json::json!({ "agent": { "turn_tool_call_budget": 42 } }))
            .expect("patch");
        assert_eq!(updated.agent.turn_tool_call_budget, Some(42));
        assert!(
            updated.agent.max_parallel_tool_calls.is_none(),
            "untouched sibling field survives"
        );
    }

    #[test]
    fn update_from_patch_merges_and_rejects_invalid() {
        let mut settings = Settings::default();
        settings.agent.max_parallel_tool_calls = Some(4);
        let updated = settings
            .update_from_patch(&serde_json::json!({
                "agent": { "max_parallel_tool_calls": 6, "context_window": 32768 }
            }))
            .unwrap();
        assert_eq!(updated.agent.max_parallel_tool_calls, Some(6));
        assert_eq!(updated.agent.context_window, Some(32768));
        // Invalid patches are rejected wholesale.
        let bad = settings.update_from_patch(
            &serde_json::json!({ "prompt": { "include_identity": "yes-please" } }),
        );
        assert!(bad.is_err());
        assert_eq!(settings.prompt.include_identity, true, "original untouched");
    }

 /// toolOrder parity: the guide order must carry the rest sentinel
    /// exactly once — a bad order is rejected at write time, not silently
    /// dropped at every run.
    #[test]
    fn update_from_patch_validates_tool_order() {
        let settings = Settings::default();
        let good = settings
            .update_from_patch(&serde_json::json!({
                "prompt": { "tool_order": ["grep", "<unlisted-tools>", "bash"] }
            }))
            .unwrap();
        assert_eq!(
            good.prompt.tool_order,
            Some(vec![
                "grep".into(),
                "<unlisted-tools>".into(),
                "bash".into()
            ])
        );

        for bad_order in [
            serde_json::json!(["grep", "bash"]), // no rest sentinel
            serde_json::json!(["<unlisted-tools>", "<unlisted-tools>"]), // twice
        ] {
            let bad = settings.update_from_patch(&serde_json::json!({
                "prompt": { "tool_order": bad_order }
            }));
            assert!(bad.is_err(), "must reject order: {bad_order}");
        }
        assert!(settings.prompt.tool_order.is_none(), "original untouched");
    }

    /// External-app commands (toolbar terminal/IDE) ride the settings hot path.
    #[test]
    fn update_from_patch_maps_apps_commands() {
        let settings = Settings::default();
        let updated = settings
            .update_from_patch(&serde_json::json!({
                "apps": {
                    "terminal_command": "open -a iTerm '{dir}'",
                    "ide_command": "cursor wait '{dir}'",
                }
            }))
            .unwrap();
        assert_eq!(
            updated.apps.terminal_command.as_deref(),
            Some("open -a iTerm '{dir}'")
        );
        assert_eq!(
            updated.apps.ide_command.as_deref(),
            Some("cursor wait '{dir}'")
        );
        assert_eq!(Settings::default().apps.terminal_command, None);
    }

    /// WI-159/WI-160: the new budget keys parse; legacy window keys left in
    /// an old settings.yaml are tolerated (ignored) instead of failing the
    /// whole document.
    #[test]
    fn image_budget_keys_parse_and_legacy_keys_are_tolerated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.yaml");
        std::fs::write(
            &path,
            "agent:\n  image_hot_window: 2\n  image_max_hot: 3\n  image_max_request_images: 5\n  image_max_request_bytes: 1048576\n",
        )
        .unwrap();
        let settings = Settings::from_file(&path);
        assert_eq!(settings.agent.image_max_request_images, Some(5));
        assert_eq!(settings.agent.image_max_request_bytes, Some(1_048_576));
        assert_eq!(Settings::default().agent.image_max_request_images, None);
        assert_eq!(Settings::default().agent.image_max_request_bytes, None);
    }

    /// A corrupted settings.yaml must degrade to defaults, not panic or
    /// propagate the parse error into startup.
    #[test]
    fn corrupt_yaml_falls_back_to_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.yaml");
        std::fs::write(&path, "prompt:\n  include_identity: [unclosed\n").unwrap();
        let settings = Settings::from_file(&path);        assert!(
            settings.prompt.include_identity,
            "defaults after parse failure"
        );
        assert_eq!(settings.llm.providers.len(), 0);
    }

    #[test]
    fn generation_config_folds_defaults_and_overrides() {
        let defaults = LlmProviderSettings::default();
        let gc = defaults.generation_config();
        assert_eq!(gc.max_tokens, 8192);
        assert_eq!(gc.temperature, 0.7);
        assert_eq!(gc.context_window, 128000);
        assert_eq!(gc.api_format, "openai-chat");
        assert_eq!(gc.reasoning_effort, None);

        let tuned = LlmProviderSettings {
            max_tokens: Some(4096),
            temperature: Some(0.2),
            context_window: Some(65536),
            api_format: "anthropic".into(),
            reasoning_effort: Some("high".into()),
            ..Default::default()
        };
        let gc = tuned.generation_config();
        assert_eq!(gc.max_tokens, 4096);
        assert_eq!(gc.temperature, 0.2);
        assert_eq!(gc.context_window, 65536);
        assert_eq!(gc.api_format, "anthropic");
        assert_eq!(gc.reasoning_effort.as_deref(), Some("high"));
    }

    /// WI-201: known GLM Coding Plan models resolve their real window from
    /// the builtin table when the entry declares none — the 128k default
    /// lied about that route (official spec 1M; declared 200k as the
    /// agent-loop compaction bound, see builtin_context_window's doc).
    #[test]
    fn builtin_context_window_declares_glm_routes() {
        // Both preset spellings of the GLM Coding Plan route resolve alike.
        assert_eq!(
            builtin_context_window("zhipu", "glm-5.3-flash"),
            Some(200_000)
        );
        assert_eq!(builtin_context_window("glm", "glm-5.3-flashx"), Some(200_000));
        assert_eq!(builtin_context_window("zhipu", "GLM-5.3"), Some(200_000));
        // The 4.x catalog models pin the documented 128K.
        assert_eq!(builtin_context_window("zhipu", "glm-4-plus"), Some(128_000));
        // Unknown model/provider → no builtin claim (platform default wins).
        assert_eq!(builtin_context_window("zhipu", "glm-9.9"), None);
        assert_eq!(builtin_context_window("deepseek", "glm-5.3-flash"), None);
    }

    #[test]
    fn generation_config_uses_builtin_window_before_the_default() {
        // An entry WITHOUT a declared window picks the builtin table value.
        let mut entry = LlmProviderSettings {
            id: "zhipu".into(),
            default_model: "glm-5.3-flash".into(),
            ..Default::default()
        };
        assert_eq!(entry.generation_config().context_window, 200_000);

        // An EXPLICIT declaration still outranks the builtin table.
        entry.context_window = Some(4096);
        assert_eq!(entry.generation_config().context_window, 4096);

        // Unknown providers keep the platform default.
        let other = LlmProviderSettings {
            id: "deepseek".into(),
            default_model: "deepseek-chat".into(),
            ..Default::default()
        };
        assert_eq!(other.generation_config().context_window, 128_000);
    }

    /// Pins resolve_variant's contract: an exact-id hit, and NO default_model
    /// fallback on a miss (callers compensate — admin2's binding projection
    /// wraps it in unwrap_or(default_model)).
    #[test]
    fn resolve_variant_exact_hit_only_no_default_fallback() {
        let p = LlmProviderSettings {
            id: "ds".into(),
            models: vec![
                LlmVariantSettings {
                    id: "deepseek-chat".into(),
                    name: "Chat".into(),
                },
                LlmVariantSettings {
                    id: "deepseek-reasoner".into(),
                    name: "Reasoner".into(),
                },
            ],
            default_model: "deepseek-chat".into(),
            ..Default::default()
        };
        assert_eq!(
            p.resolve_variant("deepseek-reasoner")
                .map(|v| v.id.as_str()),
            Some("deepseek-reasoner")
        );
        assert_eq!(
            p.resolve_variant("does-not-exist").map(|v| v.id.as_str()),
            None,
            "a miss returns None even though default_model exists"
        );
    }

    #[test]
    fn compaction_tuning_none_until_any_knob_is_set() {
        assert!(AgentSection::default().compaction_tuning().is_none());
        let partial = AgentSection {
            prune_threshold_chars: Some(4096),
            ..Default::default()
        };
        let tuning = partial.compaction_tuning().unwrap();
        assert_eq!(tuning.prune_threshold_chars, Some(4096));
        assert_eq!(
            tuning.prune_head_chars, None,
            "unset knobs stay None for engine defaults"
        );
        assert_eq!(tuning.compaction_retries, None);
    }
}
