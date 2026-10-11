use serde::{Deserialize, Serialize};
use std::path::Path;

/// Runtime config — core portion shared by Edge and Desktop
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditConfig {
    pub enabled: bool,
    #[serde(default = "default_db_path")]
    pub db_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeConfig {
    #[serde(default)]
    pub version: u32,
    #[serde(default = "default_mode")]
    pub mode: String,
    #[serde(default)]
    pub inference: InferenceConfig,
    #[serde(default)]
    pub agent: AgentConfig,
    #[serde(default)]
    pub tools: ToolsConfig,
    #[serde(default)]
    pub runner: RunnerConfig,
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub session: SessionConfig,
    /// Optional audit logging config
    #[serde(default)]
    pub audit: Option<AuditConfig>,
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// Desktop-specific: permission rules
    #[serde(default)]
    pub permissions: Vec<PermissionRule>,
}

fn default_mode() -> String {
    "hybrid".into()
}

/// Default session DB location: `~/.starriver/starriver.db`, shared by all
/// entry points (desktop NAPI, TUI, daemon); falls back to a cwd-relative
/// path when HOME is missing. The old relative default `./starriver.db` was
/// resolved against the process cwd — the desktop app (main.js constructs
/// AgentRuntime without passing a config path) wrote the session DB into the
/// repo's desktop/electron directory, and E2E's isolated HOME had no effect
/// on it (verified 2026-09-02).
fn default_db_path() -> String {
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() => {
            format!("{}/.starriver/starriver.db", h.trim_end_matches('/'))
        }
        _ => "./starriver.db".into(),
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InferenceConfig {
    pub engines: Vec<EngineConfig>,
    #[serde(default)]
    pub local_engine_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineConfig {
    pub id: String,
    #[serde(rename = "type")]
    pub engine_type: String,
    pub base_url: Option<String>,
    pub model_path: Option<String>,
    pub api_key: Option<String>,
    pub default_model: String,
    pub priority: Option<u32>,
    #[serde(default)]
    pub concurrency: Option<usize>,
    /// Max output tokens. None = 8192 default.
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Model context window. None = 128000 default. Drives compaction.
    #[serde(default)]
    pub context_window: Option<usize>,
    /// Sampling temperature. None = 0.7 default. Ignored by DeepSeek in thinking mode.
    #[serde(default)]
    pub temperature: Option<f32>,
    /// Wire protocol: "openai-chat" (default) | "openai-responses" |
    /// "anthropic"
    #[serde(default)]
    pub api_format: Option<String>,
    /// DeepSeek thinking mode: "off" | "low" | "high" | "max". None = provider default.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

impl EngineConfig {
    /// Merge optional fields into a concrete GenerationConfig.
    pub fn generation_config(&self) -> crate::types::GenerationConfig {
        crate::types::GenerationConfig {
            max_tokens: self.max_tokens.unwrap_or(8192),
            temperature: self.temperature.unwrap_or(0.7),
            context_window: self.context_window.unwrap_or(128000),
            api_format: self
                .api_format
                .clone()
                .unwrap_or_else(|| "openai-chat".to_string()),
            reasoning_effort: self.reasoning_effort.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    #[serde(default = "default_max_turns")]
    pub max_turns: usize,
    #[serde(default = "default_compaction_threshold")]
    pub compaction_threshold: f32,
    #[serde(default = "default_turn_timeout")]
    pub turn_timeout_secs: u64,
    #[serde(default = "default_no_progress_turns")]
    pub no_progress_turns: usize,
    #[serde(default = "default_max_wall_time")]
    pub max_wall_time_secs: u64,
    #[serde(default = "default_approval_wait")]
    pub max_approval_wait_secs: u64,
    /// Auto mode: allow the LLM backstop (`classify_llm`) to rescue rule-unknown
    /// tool calls — run them without confirmation when a judge model replies SAFE.
    /// Off by default: unknown calls confirm (conservative fail-closed).
    #[serde(default = "default_llm_backstop")]
    pub auto_llm_backstop: bool,
    pub model: Option<String>,
}

fn default_llm_backstop() -> bool {
    false
}

fn default_max_turns() -> usize {
    50
}
fn default_compaction_threshold() -> f32 {
    0.92
}
fn default_turn_timeout() -> u64 {
    300
}
fn default_no_progress_turns() -> usize {
    3
}
/// Hard backstop on total task wall-clock time. Generous on purpose: the check
/// only runs between turns, so a legit long task with slow cloud API calls
/// (large contexts, reasoning models) can legitimately exceed a few minutes.
/// Runaway loops are already bounded by `max_turns` + `no_progress_turns`;
/// wall-time is the last-resort cap, not the primary limiter.
fn default_max_wall_time() -> u64 {
    3600
}
fn default_approval_wait() -> u64 {
    300
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolsConfig {
    pub packages: Vec<String>,
    pub output_budget: Option<OutputBudgetConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputBudgetConfig {
    pub defaults: BudgetDefaults,
    pub per_tool: Option<std::collections::HashMap<String, PerToolBudget>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetDefaults {
    pub max_bytes: usize,
    pub strategy: String,
    pub head_lines: Option<usize>,
    pub tail_lines: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerToolBudget {
    pub max_bytes: usize,
    pub strategy: Option<String>,
    pub head_lines: Option<usize>,
    pub tail_lines: Option<usize>,
}

/// Runner config — runtime environment (sandbox, hardware, etc.)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunnerConfig {
    pub sandbox: Option<SandboxConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxConfig {
    pub backend: String,
    pub policy: Option<SandboxPolicyConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxPolicyConfig {
    pub network: Option<NetworkPolicy>,
    pub filesystem: Option<FsPolicy>,
    pub resources: Option<ResourceLimits>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkPolicy {
    pub allow_outbound: bool,
    pub allowed_hosts: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsPolicy {
    pub writable_paths: Vec<String>,
    pub blocked_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceLimits {
    pub max_memory_mb: usize,
    pub max_cpu_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRule {
    pub tool: String,
    pub pattern: Option<String>,
    pub path: Option<String>,
    pub action: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServerConfig {
    pub http: Option<HttpServerConfig>,
    pub unix_socket: Option<UnixSocketConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpServerConfig {
    pub enabled: bool,
    pub bind: String,
    /// Optional rate limiting for HTTP API
    pub rate_limit: Option<crate::rate_limit::RateLimitConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnixSocketConfig {
    pub enabled: bool,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionConfig {
    pub db_path: String,
    #[serde(default = "default_max_sessions")]
    pub max_sessions: usize,
    #[serde(default = "default_checkpoint_interval")]
    pub checkpoint_interval_secs: u64,
    #[serde(default = "default_wal_checkpoint_interval")]
    pub wal_checkpoint_interval_secs: u64,
    #[serde(default = "default_write_retry_max")]
    pub write_retry_max: u32,
    #[serde(default = "default_write_retry_base_ms")]
    pub write_retry_base_ms: u64,
    #[serde(default = "default_session_retention_days")]
    pub session_retention_days: i32,
}

fn default_max_sessions() -> usize {
    100
}
fn default_checkpoint_interval() -> u64 {
    30
}
fn default_wal_checkpoint_interval() -> u64 {
    300
}
fn default_write_retry_max() -> u32 {
    3
}
fn default_write_retry_base_ms() -> u64 {
    10
}
fn default_session_retention_days() -> i32 {
    30
}

impl RuntimeConfig {
    pub fn from_file(path: &Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let expanded = Self::expand_env_vars(&content);
        let config: Self = serde_yaml::from_str(&expanded)?;
        if config.session.session_retention_days < 0 {
            return Err(anyhow::anyhow!(
                "session.session_retention_days cannot be negative, current value: {}",
                config.session.session_retention_days
            ));
        }
        Ok(config)
    }

    pub fn load(path: Option<&Path>) -> anyhow::Result<Self> {
        match path {
            Some(p) if p.exists() => Self::from_file(p),
            _ => Ok(Self::default()),
        }
    }

    fn expand_env_vars(content: &str) -> String {
        let re = regex_lite::Regex::new(r"\$\{(\w+)\}").unwrap();
        re.replace_all(content, |caps: &regex_lite::Captures| {
            let var = &caps[1];
            std::env::var(var).unwrap_or_else(|_| format!("${{{var}}}"))
        })
        .to_string()
    }
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_turns: default_max_turns(),
            compaction_threshold: default_compaction_threshold(),
            turn_timeout_secs: default_turn_timeout(),
            no_progress_turns: default_no_progress_turns(),
            max_wall_time_secs: default_max_wall_time(),
            max_approval_wait_secs: default_approval_wait(),
            auto_llm_backstop: default_llm_backstop(),
            model: None,
        }
    }
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            db_path: default_db_path(),
            max_sessions: default_max_sessions(),
            checkpoint_interval_secs: default_checkpoint_interval(),
            wal_checkpoint_interval_secs: default_wal_checkpoint_interval(),
            write_retry_max: default_write_retry_max(),
            write_retry_base_ms: default_write_retry_base_ms(),
            session_retention_days: default_session_retention_days(),
        }
    }
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            version: 1,
            mode: default_mode(),
            inference: InferenceConfig {
                engines: vec![EngineConfig {
                    id: "local".into(),
                    engine_type: "ollama".into(),
                    base_url: Some("http://localhost:11434".into()),
                    model_path: None,
                    api_key: None,
                    default_model: "qwen3:4b".into(),
                    priority: Some(10),
                    concurrency: None,
                    max_tokens: None,
                    context_window: None,
                    temperature: None,
                    api_format: None,
                    reasoning_effort: None,
                }],
                local_engine_id: None,
            },
            agent: AgentConfig {
                max_turns: default_max_turns(),
                compaction_threshold: default_compaction_threshold(),
                turn_timeout_secs: default_turn_timeout(),
                no_progress_turns: default_no_progress_turns(),
                max_wall_time_secs: default_max_wall_time(),
                max_approval_wait_secs: default_approval_wait(),
                auto_llm_backstop: default_llm_backstop(),
                model: None,
            },
            tools: ToolsConfig::default(),
            runner: RunnerConfig::default(),
            server: ServerConfig {
                http: Some(HttpServerConfig {
                    enabled: true,
                    bind: "0.0.0.0:8800".into(),
                    rate_limit: None,
                }),
                unix_socket: None,
            },
            session: SessionConfig {
                db_path: default_db_path(),
                max_sessions: default_max_sessions(),
                checkpoint_interval_secs: default_checkpoint_interval(),
                wal_checkpoint_interval_secs: default_wal_checkpoint_interval(),
                write_retry_max: default_write_retry_max(),
                write_retry_base_ms: default_write_retry_base_ms(),
                session_retention_days: default_session_retention_days(),
            },
            audit: None,
            system_prompt: None,
            permissions: vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Generate a process-unique temp file path (avoid parallel test conflicts). Caller must clean up.
    fn temp_yaml(name: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut p = std::env::temp_dir();
        p.push(format!(
            "starriver-cfg-{}-{}-{}.yaml",
            name,
            std::process::id(),
            n
        ));
        p
    }

    fn write_yaml(path: &std::path::Path, body: &str) {
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn from_file_reads_and_parses() {
        let path = temp_yaml("valid");
        // RuntimeConfig inference/agent/server/session fields are required on the struct
        write_yaml(&path, "version: 1\nmode: offline\ninference: { engines: [] }\nagent: {}\nserver: { http: { enabled: false, bind: x } }\nsession: { db_path: \"./x.db\" }\n");
        let cfg = RuntimeConfig::from_file(&path).unwrap();
        assert_eq!(cfg.version, 1);
        assert_eq!(cfg.mode, "offline");
        assert!(cfg.inference.engines.is_empty()); // YAML provided no engines
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn from_file_applies_session_defaults() {
        let path = temp_yaml("defaults");
        write_yaml(&path, "version: 1\ninference: { engines: [] }\nagent: { max_turns: 5 }\nserver: { http: { enabled: false, bind: \"127.0.0.1:9\" } }\nsession: { db_path: \"./x.db\" }\n");
        let cfg = RuntimeConfig::from_file(&path).unwrap();
        // unspecified SessionConfig fields should use defaults
        assert_eq!(cfg.session.max_sessions, 100);
        assert_eq!(cfg.session.checkpoint_interval_secs, 30);
        assert_eq!(cfg.session.wal_checkpoint_interval_secs, 300);
        assert_eq!(cfg.session.write_retry_max, 3);
        assert_eq!(cfg.session.session_retention_days, 30);
        // explicitly provided fields override defaults
        assert_eq!(cfg.agent.max_turns, 5);
        assert_eq!(cfg.server.http.as_ref().unwrap().enabled, false);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn from_file_rejects_negative_retention() {
        let path = temp_yaml("neg");
        write_yaml(&path, "version: 1\ninference: { engines: [] }\nagent: {}\nserver: { http: { enabled: false, bind: x } }\nsession: { db_path: \"./x.db\", session_retention_days: -1 }\n");
        let err = RuntimeConfig::from_file(&path).unwrap_err();
        assert!(err.to_string().contains("cannot be negative"), "got: {err}");
        assert!(err.to_string().contains("-1"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn from_file_expands_env_vars() {
        std::env::set_var("STARRIVER_TEST_DB", "/from-env.db");
        let path = temp_yaml("env");
        write_yaml(&path, "version: 1\ninference: { engines: [] }\nagent: {}\nserver: { http: { enabled: false, bind: x } }\nsession: { db_path: \"${STARRIVER_TEST_DB}\" }\n");
        let cfg = RuntimeConfig::from_file(&path).unwrap();
        assert_eq!(cfg.session.db_path, "/from-env.db");
        let _ = std::fs::remove_file(&path);
        std::env::remove_var("STARRIVER_TEST_DB");
    }

    #[test]
    fn from_file_keeps_unset_env_var_literal() {
        let path = temp_yaml("envunset");
        write_yaml(&path, "version: 1\ninference: { engines: [] }\nagent: {}\nserver: { http: { enabled: false, bind: x } }\nsession: { db_path: \"${STARRIVER_NEVER_SET_VAR}\" }\n");
        let cfg = RuntimeConfig::from_file(&path).unwrap();
        assert_eq!(cfg.session.db_path, "${STARRIVER_NEVER_SET_VAR}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn expand_env_vars_substitutes_and_preserves_unset() {
        std::env::set_var("STARRIVER_E1", "v1");
        assert_eq!(
            RuntimeConfig::expand_env_vars("a-${STARRIVER_E1}-b"),
            "a-v1-b"
        );
        assert_eq!(
            RuntimeConfig::expand_env_vars("${STARRIVER_NEVER2}"),
            "${STARRIVER_NEVER2}"
        );
        std::env::remove_var("STARRIVER_E1");
    }

    #[test]
    fn load_none_returns_default() {
        let cfg = RuntimeConfig::load(None).unwrap();
        assert_eq!(cfg.version, 1);
        assert_eq!(cfg.mode, "hybrid");
        assert_eq!(cfg.server.http.as_ref().unwrap().bind, "0.0.0.0:8800");
        assert_eq!(cfg.inference.engines.len(), 1);
        assert_eq!(cfg.inference.engines[0].engine_type, "ollama");
        assert_eq!(cfg.agent.max_turns, 50);
    }

    #[test]
    fn load_missing_path_falls_back_to_default() {
        let cfg =
            RuntimeConfig::load(Some(std::path::Path::new("/no/such/starriver.yaml"))).unwrap();
        assert_eq!(cfg.mode, "hybrid"); // default value
    }

    #[test]
    fn default_db_path_lives_under_home() {
        // With HOME: the session DB defaults to ~/.starriver (shared by all
        // entry points), no longer cwd-relative — the relative default once
        // wrote the desktop app's session DB into the repo's desktop/electron
        // directory.
        if let Ok(h) = std::env::var("HOME") {
            assert!(!h.is_empty(), "test HOME must be non-empty");
            let expected = format!("{}/.starriver/starriver.db", h.trim_end_matches('/'));
            assert_eq!(RuntimeConfig::default().session.db_path, expected);
            assert_eq!(SessionConfig::default().db_path, expected);
        }
    }

    #[test]
    fn local_engine_id_parsed_and_default() {
        assert!(RuntimeConfig::default().inference.local_engine_id.is_none());
        let path = temp_yaml("leid");
        write_yaml(&path, "version: 1\ninference: { local_engine_id: lmstudio, engines: [] }\nagent: {}\nserver: { http: { enabled: false, bind: x } }\nsession: { db_path: ./x.db }\n");
        let cfg = RuntimeConfig::from_file(&path).unwrap();
        assert_eq!(cfg.inference.local_engine_id.as_deref(), Some("lmstudio"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn engine_generation_config_fields_parsed() {
        let path = temp_yaml("gencfg");
        write_yaml(&path, "version: 1\ninference: { engines: [ { id: ds, type: openai-compat, default_model: m, max_tokens: 16384, context_window: 256000, temperature: 0.3, api_format: anthropic } ] }\nagent: {}\nserver: { http: { enabled: false, bind: x } }\nsession: { db_path: ./x.db }\n");
        let cfg = RuntimeConfig::from_file(&path).unwrap();
        let e = &cfg.inference.engines[0];
        assert_eq!(e.max_tokens, Some(16384));
        assert_eq!(e.context_window, Some(256000));
        assert_eq!(e.temperature, Some(0.3));
        assert_eq!(e.api_format.as_deref(), Some("anthropic"));
        let gen = e.generation_config();
        assert_eq!(gen.max_tokens, 16384);
        assert_eq!(gen.temperature, 0.3);
        assert_eq!(gen.api_format, "anthropic");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn engine_generation_config_defaults() {
        let e = EngineConfig {
            id: "x".into(),
            engine_type: "ollama".into(),
            base_url: None,
            model_path: None,
            api_key: None,
            default_model: "m".into(),
            priority: None,
            concurrency: None,
            max_tokens: None,
            context_window: None,
            temperature: None,
            api_format: None,
            reasoning_effort: None,
        };
        let gen = e.generation_config();
        assert_eq!(gen.max_tokens, 8192);
        assert_eq!(gen.context_window, 128000);
        assert_eq!(gen.temperature, 0.7);
        assert_eq!(gen.api_format, "openai-chat");
    }

    #[test]
    fn engine_concurrency_parsed_and_default() {
        assert!(RuntimeConfig::default().inference.engines[0]
            .concurrency
            .is_none());
        let path = temp_yaml("conc");
        write_yaml(&path, "version: 1\ninference: { engines: [ { id: lm, type: lmstudio, default_model: m, concurrency: 2 } ] }\nagent: {}\nserver: { http: { enabled: false, bind: x } }\nsession: { db_path: ./x.db }\n");
        let cfg = RuntimeConfig::from_file(&path).unwrap();
        assert_eq!(cfg.inference.engines[0].concurrency, Some(2));
        let _ = std::fs::remove_file(&path);
    }
}
