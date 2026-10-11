use keyring::Entry;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A variant (specific model) under a provider entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelVariant {
    /// API model name, e.g. "deepseek-chat", "deepseek-reasoner"
    pub id: String,
    /// Display name, e.g. "Flash", "Pro"
    pub name: String,
}

/// A single model entry in the global model registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelEntry {
    pub id: String,
    pub name: String,
    /// "cloud" or "local"
    #[serde(rename = "type")]
    pub model_type: String,
    /// "openai-compat", "ollama", "lmstudio", or "llama"
    pub provider_type: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    /// GGUF file path for local models (e.g. Ollama/LM Studio model file)
    #[serde(default)]
    pub model_path: Option<String>,
    /// Available model variants. Empty vec = use default_model only.
    #[serde(default)]
    pub models: Vec<ModelVariant>,
    /// Default variant ID. Must match one of `models[].id` or used directly if models is empty.
    pub default_model: String,
    /// Optional concurrency override
    #[serde(default)]
    pub concurrency: Option<usize>,
    /// Max output tokens per response. None = 8192 default.
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Model context window (input + output). None = 128000 default. Drives compaction.
    #[serde(default)]
    pub context_window: Option<usize>,
    /// Sampling temperature. None = 0.7 default. Coding tasks usually prefer lower (0.2~0.4).
    /// Note: ignored by DeepSeek while thinking mode is enabled.
    #[serde(default)]
    pub temperature: Option<f32>,
    /// Wire protocol: "openai-chat" (default) | "anthropic"
    #[serde(default = "default_api_format")]
    pub api_format: String,
    /// DeepSeek thinking mode: "off" | "low" | "high" | "max". None = provider default (high).
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

fn default_api_format() -> String {
    "openai-chat".into()
}

impl ModelEntry {
    /// Find a variant by id. No fallback: a miss returns `None` (pinned by
    /// `test_resolve_variant`; see the corrected sibling doc in settings.rs).
    pub fn resolve_variant(&self, variant_id: &str) -> Option<&ModelVariant> {
        self.models.iter().find(|v| v.id == variant_id)
    }

    /// Merge optional config fields into a concrete GenerationConfig.
    pub fn generation_config(&self) -> starriver_kernel::types::GenerationConfig {
        starriver_kernel::types::GenerationConfig {
            max_tokens: self.max_tokens.unwrap_or(8192),
            temperature: self.temperature.unwrap_or(0.7),
            context_window: self.context_window.unwrap_or(128000),
            api_format: self.api_format.clone(),
            reasoning_effort: self.reasoning_effort.clone(),
        }
    }
}

impl Default for ModelEntry {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            model_type: String::new(),
            provider_type: String::new(),
            base_url: String::new(),
            api_key: String::new(),
            model_path: None,
            models: Vec::new(),
            default_model: String::new(),
            concurrency: None,
            max_tokens: None,
            context_window: None,
            temperature: None,
            api_format: default_api_format(),
            reasoning_effort: None,
        }
    }
}

/// Global model registry persisted to ~/.starriver/models.yaml
#[derive(Debug, Clone)]
pub struct ModelRegistry {
    pub models: Vec<ModelEntry>,
    config_path: PathBuf,
}

impl ModelRegistry {
    pub fn new() -> anyhow::Result<Self> {
        let config_path = Self::default_path();
        let models = if config_path.exists() {
            let content = std::fs::read_to_string(&config_path).unwrap_or_default();
            match serde_yaml::from_str(&content) {
                Ok(models) => models,
                Err(e) => {
                    // A later add()/remove() would otherwise save() the
                    // empty list OVER the corrupt file, destroying every
                    // stored entry. Park the corrupt bytes for recovery.
                    let backup = config_path.with_extension("yaml.corrupt");
                    if std::fs::copy(&config_path, &backup).is_ok() {
                        tracing::warn!(backup = %backup.display(), error = %e,
                            "models.yaml corrupt; preserved and starting empty");
                    }
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };
        let mut registry = Self {
            models,
            config_path,
        };
        // Auto-migrate plaintext keys to the secrets file, then legacy OS
        // keychain refs to the file store. Failures are non-fatal and retry
        // next launch.
        if let Err(e) = registry.migrate_plaintext_keys() {
            tracing::warn!(error = %e, "secrets migration failed on startup");
        }
        if let Err(e) = registry.migrate_keychain_refs() {
            tracing::warn!(error = %e, "keychain→file migration failed on startup");
        }
        Ok(registry)
    }

    fn default_path() -> PathBuf {
        let home = std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."));
        home.join(".starriver").join("models.yaml")
    }

    pub fn list(&self) -> &[ModelEntry] {
        &self.models
    }

    pub fn get(&self, id: &str) -> Option<&ModelEntry> {
        self.models.iter().find(|m| m.id == id)
    }

    /// Add or update a model entry. Same id replaces existing.
    pub fn add(&mut self, entry: ModelEntry) -> anyhow::Result<()> {
        // In-place update: a replace must not reorder the registry
        // (the list order is the display + default-pick order).
        if let Some(slot) = self.models.iter_mut().find(|m| m.id == entry.id) {
            *slot = entry;
        } else {
            self.models.push(entry);
        }
        self.save()
    }

    pub fn remove(&mut self, id: &str) -> anyhow::Result<()> {
        self.models.retain(|m| m.id != id);
        self.save()
    }

    fn save(&self) -> anyhow::Result<()> {
        if let Some(parent) = self.config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let yaml = serde_yaml::to_string(&self.models)?;
        std::fs::write(&self.config_path, yaml)?;
        Ok(())
    }

    /// Resolve an api_key field to its real value.
    ///
    /// Supports three formats (see [`starriver_secrets::secrets::resolve`]):
    /// - `!file:<account>` — managed credential records first, then
    ///   `~/.starriver/secrets.json`
    /// - `!keychain:<account>` — read from the OS keychain (legacy)
    /// - `${ENV_VAR}` — read from environment variable
    /// - plaintext — returned as-is (backward compatible)
    pub fn resolve_api_key(api_key_field: &str) -> anyhow::Result<String> {
        starriver_secrets::credentials::CredentialsStore::new()
            .resolve_field(api_key_field)
            .map_err(anyhow::Error::msg)
    }

    /// Store an API key in the managed credential records (legacy fallback:
    /// secrets file). Returns the reference string to store in the YAML
    /// config (e.g. `!file:model/deepseek`).
    pub fn store_api_key(model_id: &str, api_key: &str) -> anyhow::Result<String> {
        if api_key.is_empty() {
            return Ok(String::new());
        }
        // Already a reference (file/keychain ref or env-var placeholder) —
        // return it unchanged instead of wrapping it as a literal key. Prevents
        // re-saving an existing entry (whose key field is a `!file:` ref) from
        // storing the ref string itself as the real credential.
        if starriver_secrets::secrets::is_reference(api_key) {
            return Ok(api_key.to_string());
        }
        let account = format!("model/{}", model_id);
        match starriver_secrets::credentials::CredentialKey::new("model", model_id) {
            Ok(key) => starriver_secrets::credentials::CredentialsStore::new()
                .set_api_key(&key, api_key)
                .map_err(anyhow::Error::msg),
            Err(_) => {
                // Non-addressable id (e.g. contains `/`): legacy secrets path.
                starriver_secrets::secrets::set(&account, api_key)?;
                Ok(format!("!file:{}", account))
            }
        }
    }

    /// Delete an API key from the managed records (and legacy keychain
    /// entry).
    /// Errors are logged but not propagated.
    pub fn delete_api_key(model_id: &str) {
        let account = format!("model/{}", model_id);
        if let Ok(key) = starriver_secrets::credentials::CredentialKey::new("model", model_id) {
            if let Err(e) = starriver_secrets::credentials::CredentialsStore::new().unset(&key) {
                tracing::warn!(account = %account, error = %e, "failed to remove credential record");
            }
        } else if let Err(e) = starriver_secrets::secrets::remove(&account) {
            tracing::warn!(account = %account, error = %e, "failed to remove secret from file");
        }
        // Legacy keychain entry, best-effort. Skipped entirely when the
        // keychain is disabled: macOS securityd can stall unsigned test /
        // headless binaries for tens of seconds per round trip.
        if starriver_secrets::secrets::keychain_disabled() {
            return;
        }
        if let Ok(entry) = Entry::new("starriver", &account) {
            let _ = entry.delete_credential();
        }
    }

    /// Migrate plaintext API keys to the secrets file. Returns the number of
    /// migrated entries. Called automatically from `new()`.
    pub fn migrate_plaintext_keys(&mut self) -> anyhow::Result<usize> {
        let mut count = 0;
        for entry in &mut self.models {
            // Skip empty keys and any existing references (file/keychain/env)
            if entry.api_key.is_empty() || starriver_secrets::secrets::is_reference(&entry.api_key)
            {
                continue;
            }
            // Attempt migration — if the write fails, warn and keep plaintext
            match Self::store_api_key(&entry.id, &entry.api_key) {
                Ok(reference) => {
                    tracing::info!(
                        model_id = %entry.id,
                        "migrated API key to secrets file"
                    );
                    entry.api_key = reference;
                    count += 1;
                }
                Err(e) => {
                    tracing::warn!(
                        model_id = %entry.id,
                        error = %e,
                        "failed to migrate API key to secrets file — key will remain in plaintext"
                    );
                }
            }
        }
        if count > 0 {
            self.save()?;
        }
        Ok(count)
    }

    /// Migrate legacy `!keychain:` refs to the `!file:` store. A ref whose
    /// keychain credential no longer exists is left untouched — resolution
    /// reports it and the user re-enters the key. Returns the migrated count.
    pub fn migrate_keychain_refs(&mut self) -> anyhow::Result<usize> {
        if starriver_secrets::secrets::keychain_disabled() {
            return Ok(0);
        }
        let mut count = 0;
        for entry in &mut self.models {
            let Some(account) = entry.api_key.strip_prefix("!keychain:").map(str::to_string) else {
                continue;
            };
            let key = match Entry::new("starriver", &account).and_then(|e| e.get_password()) {
                Ok(key) => key,
                Err(_) => {
                    tracing::info!(
                        account = %account,
                        "keychain entry missing — leaving ref untouched (user re-enters key)"
                    );
                    continue;
                }
            };
            match starriver_secrets::secrets::set(&account, &key) {
                Ok(()) => {
                    let _ = Entry::new("starriver", &account).and_then(|e| e.delete_credential());
                    entry.api_key = format!("!file:{}", account);
                    count += 1;
                    tracing::info!(account = %account, "migrated keychain ref to secrets file");
                }
                Err(e) => {
                    tracing::warn!(account = %account, error = %e, "failed to migrate keychain ref to file");
                }
            }
        }
        if count > 0 {
            self.save()?;
        }
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("starriver-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // HOME-swapping tests go through `starriver_secrets::secrets::with_temp_home` (holds
    // the shared HOME_LOCK) so parallel threads can't race the global $HOME.

    #[test]
    fn test_add_and_remove() {
        let tmp = temp_dir();
        let path = tmp.join("models.yaml");
        std::fs::write(&path, "[]").unwrap();

        let mut reg = ModelRegistry {
            models: Vec::new(),
            config_path: path.clone(),
        };
        let entry = ModelEntry {
            id: "deepseek".into(),
            name: "DeepSeek".into(),
            model_type: "cloud".into(),
            provider_type: "openai-compat".into(),
            base_url: "https://api.deepseek.com".into(),
            api_key: "sk-test".into(),
            model_path: None,
            models: vec![
                ModelVariant {
                    id: "deepseek-chat".into(),
                    name: "Flash".into(),
                },
                ModelVariant {
                    id: "deepseek-reasoner".into(),
                    name: "Pro".into(),
                },
            ],
            default_model: "deepseek-chat".into(),
            concurrency: None,
            ..Default::default()
        };
        reg.add(entry).unwrap();
        assert_eq!(reg.list().len(), 1);
        assert!(reg.get("deepseek").is_some());

        reg.remove("deepseek").unwrap();
        assert!(reg.list().is_empty());
        assert!(reg.get("deepseek").is_none());
    }

    #[test]
    fn test_persistence() {
        let tmp = temp_dir();
        let path = tmp.join("models.yaml");
        std::fs::write(&path, "[]").unwrap();

        let mut reg = ModelRegistry {
            models: Vec::new(),
            config_path: path.clone(),
        };
        let entry = ModelEntry {
            id: "qwen".into(),
            name: "QWen".into(),
            model_type: "cloud".into(),
            provider_type: "openai-compat".into(),
            base_url: "https://custom.api".into(),
            api_key: "sk-qwen".into(),
            model_path: None,
            models: vec![ModelVariant {
                id: "qwen-turbo".into(),
                name: "Turbo".into(),
            }],
            default_model: "qwen-turbo".into(),
            concurrency: Some(4),
            ..Default::default()
        };
        reg.add(entry).unwrap();
        drop(reg);

        let content = std::fs::read_to_string(&path).unwrap();
        let models: Vec<ModelEntry> = serde_yaml::from_str(&content).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "qwen");
        assert_eq!(models[0].concurrency, Some(4));
    }

    #[test]
    fn test_add_replaces_existing() {
        let tmp = temp_dir();
        let path = tmp.join("models.yaml");
        std::fs::write(&path, "[]").unwrap();

        let mut reg = ModelRegistry {
            models: Vec::new(),
            config_path: path.clone(),
        };
        let e1 = ModelEntry {
            id: "test".into(),
            name: "Test 1".into(),
            model_type: "cloud".into(),
            provider_type: "openai-compat".into(),
            base_url: "".into(),
            api_key: "".into(),
            model_path: None,
            models: vec![],
            default_model: "m1".into(),
            concurrency: None,
            ..Default::default()
        };
        let e2 = ModelEntry {
            id: "test".into(),
            name: "Test 2".into(),
            model_type: "local".into(),
            provider_type: "ollama".into(),
            base_url: "".into(),
            api_key: "".into(),
            model_path: None,
            models: vec![],
            default_model: "m2".into(),
            concurrency: None,
            ..Default::default()
        };
        reg.add(e1).unwrap();
        reg.add(e2).unwrap();
        assert_eq!(reg.list().len(), 1);
        assert_eq!(reg.get("test").unwrap().default_model, "m2");
    }

    #[test]
    fn test_local_model_with_path() {
        let tmp = temp_dir();
        let path = tmp.join("models.yaml");
        std::fs::write(&path, "[]").unwrap();

        let mut reg = ModelRegistry {
            models: Vec::new(),
            config_path: path.clone(),
        };
        let entry = ModelEntry {
            id: "local-qwen".into(),
            name: "Local Qwen".into(),
            model_type: "local".into(),
            provider_type: "ollama".into(),
            base_url: "http://localhost:11434".into(),
            api_key: "".into(),
            model_path: Some("/path/to/model.gguf".into()),
            models: vec![],
            default_model: "qwen3:4b".into(),
            concurrency: None,
            ..Default::default()
        };
        reg.add(entry).unwrap();
        assert_eq!(
            reg.get("local-qwen").unwrap().model_path.as_deref(),
            Some("/path/to/model.gguf")
        );
    }

    #[test]
    fn test_resolve_variant() {
        let entry = ModelEntry {
            id: "deepseek".into(),
            name: "DeepSeek".into(),
            model_type: "cloud".into(),
            provider_type: "openai-compat".into(),
            base_url: "https://api.deepseek.com".into(),
            api_key: "sk-test".into(),
            model_path: None,
            models: vec![
                ModelVariant {
                    id: "deepseek-chat".into(),
                    name: "Flash".into(),
                },
                ModelVariant {
                    id: "deepseek-reasoner".into(),
                    name: "Pro".into(),
                },
            ],
            default_model: "deepseek-chat".into(),
            concurrency: None,
            ..Default::default()
        };
        assert_eq!(
            entry.resolve_variant("deepseek-chat").unwrap().name,
            "Flash"
        );
        assert_eq!(
            entry.resolve_variant("deepseek-reasoner").unwrap().name,
            "Pro"
        );
        assert!(entry.resolve_variant("nonexistent").is_none());
    }

    #[test]
    fn test_empty_registry_has_no_models() {
        let tmp = temp_dir();
        let path = tmp.join("empty.yaml");
        std::fs::write(&path, "[]").unwrap();
        let reg = ModelRegistry {
            models: Vec::new(),
            config_path: path,
        };
        assert!(reg.list().is_empty());
        assert!(reg.get("anything").is_none());
    }

    #[test]
    fn test_add_multiple_entries_and_list() {
        let tmp = temp_dir();
        let path = tmp.join("multi.yaml");
        std::fs::write(&path, "[]").unwrap();
        let mut reg = ModelRegistry {
            models: Vec::new(),
            config_path: path,
        };
        for i in 1..=5 {
            reg.add(ModelEntry {
                id: format!("model-{i}"),
                name: format!("Model {i}"),
                model_type: "cloud".into(),
                provider_type: "openai-compat".into(),
                base_url: "".into(),
                api_key: "".into(),
                model_path: None,
                models: vec![],
                default_model: "m".into(),
                concurrency: None,
                ..Default::default()
            })
            .unwrap();
        }
        assert_eq!(reg.list().len(), 5);
    }

    #[test]
    fn test_remove_nonexistent_is_noop() {
        let tmp = temp_dir();
        let path = tmp.join("remove.yaml");
        std::fs::write(&path, "[]").unwrap();
        let mut reg = ModelRegistry {
            models: Vec::new(),
            config_path: path,
        };
        // Removing non-existent should not error
        reg.remove("nonexistent").unwrap();
        assert!(reg.list().is_empty());
    }

    #[test]
    fn test_resolve_variant_with_empty_models_returns_none() {
        let entry = ModelEntry {
            id: "simple".into(),
            name: "Simple".into(),
            model_type: "cloud".into(),
            provider_type: "openai-compat".into(),
            base_url: "".into(),
            api_key: "".into(),
            model_path: None,
            models: vec![],
            default_model: "simple-chat".into(),
            concurrency: None,
            ..Default::default()
        };
        assert!(entry.resolve_variant("anything").is_none());
    }

    #[test]
    fn legacy_yaml_without_new_fields_deserializes_with_defaults() {
        // Old models.yaml entries (pre max_tokens/context_window/temperature/api_format)
        // must load fine — serde defaults fill the missing fields.
        let yaml = r#"
- id: deepseek
  name: DeepSeek
  type: cloud
  provider_type: openai-compat
  base_url: https://api.deepseek.com
  api_key: sk-old
  models:
    - { id: deepseek-chat, name: Flash }
  default_model: deepseek-chat
"#;
        let entries: Vec<ModelEntry> = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.id, "deepseek");
        // new fields default
        assert_eq!(e.max_tokens, None);
        assert_eq!(e.context_window, None);
        assert_eq!(e.temperature, None);
        assert_eq!(e.api_format, "openai-chat");
        let gen = e.generation_config();
        assert_eq!(gen.max_tokens, 8192);
        assert_eq!(gen.context_window, 128000);
        assert_eq!(gen.temperature, 0.7);
        assert_eq!(gen.api_format, "openai-chat");
    }

    #[test]
    fn generation_config_uses_field_values_and_defaults() {
        let entry = ModelEntry {
            id: "deepseek".into(),
            name: "DeepSeek".into(),
            model_type: "cloud".into(),
            provider_type: "openai-compat".into(),
            base_url: "".into(),
            api_key: "".into(),
            model_path: None,
            models: vec![],
            default_model: "deepseek-chat".into(),
            concurrency: None,
            max_tokens: Some(16384),
            context_window: Some(256000),
            temperature: Some(0.3),
            api_format: "anthropic".into(),
            reasoning_effort: Some("high".into()),
        };
        let cfg = entry.generation_config();
        assert_eq!(cfg.max_tokens, 16384);
        assert_eq!(cfg.context_window, 256000);
        assert_eq!(cfg.temperature, 0.3);
        assert_eq!(cfg.api_format, "anthropic");
        assert_eq!(cfg.reasoning_effort.as_deref(), Some("high"));
    }

    #[test]
    fn generation_config_uses_defaults_when_fields_empty() {
        let entry = ModelEntry::default();
        let cfg = entry.generation_config();
        assert_eq!(cfg.max_tokens, 8192);
        assert_eq!(cfg.context_window, 128000);
        assert_eq!(cfg.temperature, 0.7);
        assert_eq!(cfg.api_format, "openai-chat");
    }

    #[test]
    fn reasoning_effort_roundtrips_through_yaml() {
        let entry = ModelEntry {
            id: "deepseek".into(),
            name: "DeepSeek".into(),
            model_type: "cloud".into(),
            provider_type: "openai-compat".into(),
            base_url: "".into(),
            api_key: "".into(),
            model_path: None,
            models: vec![],
            default_model: "deepseek-v4-flash".into(),
            concurrency: None,
            max_tokens: Some(16384),
            context_window: Some(256000),
            temperature: Some(0.3),
            api_format: "openai-chat".into(),
            reasoning_effort: Some("max".into()),
        };
        let yaml = serde_yaml::to_string(&entry).unwrap();
        let back: ModelEntry = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(back.reasoning_effort.as_deref(), Some("max"));
        assert_eq!(back.max_tokens, Some(16384));
        // legacy YAML without reasoning_effort defaults to None
        let legacy: ModelEntry = serde_yaml::from_str(
            "id: x\nname: X\ntype: cloud\nprovider_type: openai-compat\ndefault_model: m\n",
        )
        .unwrap();
        assert_eq!(legacy.reasoning_effort, None);
        assert_eq!(legacy.generation_config().reasoning_effort, None);
    }

    #[test]
    fn test_add_updates_existing_entry() {
        let tmp = temp_dir();
        let path = tmp.join("update.yaml");
        std::fs::write(&path, "[]").unwrap();
        let mut reg = ModelRegistry {
            models: Vec::new(),
            config_path: path,
        };
        let e1 = ModelEntry {
            id: "test".into(),
            name: "Original".into(),
            model_type: "cloud".into(),
            provider_type: "openai-compat".into(),
            base_url: "https://old.api.com".into(),
            api_key: "old-key".into(),
            model_path: None,
            models: vec![],
            default_model: "m1".into(),
            concurrency: Some(1),
            ..Default::default()
        };
        let e2 = ModelEntry {
            id: "test".into(),
            name: "Updated".into(),
            model_type: "local".into(),
            provider_type: "ollama".into(),
            base_url: "http://localhost".into(),
            api_key: "".into(),
            model_path: None,
            models: vec![],
            default_model: "m2".into(),
            concurrency: Some(2),
            ..Default::default()
        };
        reg.add(e1).unwrap();
        reg.add(e2).unwrap();
        assert_eq!(reg.list().len(), 1);
        let entry = reg.get("test").unwrap();
        assert_eq!(entry.name, "Updated");
        assert_eq!(entry.provider_type, "ollama");
        assert_eq!(entry.concurrency, Some(2));
    }

    #[test]
    fn test_resolve_api_key_plaintext() {
        // Plaintext is returned as-is
        let result = ModelRegistry::resolve_api_key("sk-test-123").unwrap();
        assert_eq!(result, "sk-test-123");
    }

    #[test]
    fn test_resolve_api_key_empty() {
        let result = ModelRegistry::resolve_api_key("").unwrap();
        assert_eq!(result, "");
    }

    #[test]
    fn test_resolve_api_key_env_var() {
        std::env::set_var("STARRIVER_TEST_KEY", "sk-from-env");
        let result = ModelRegistry::resolve_api_key("${STARRIVER_TEST_KEY}").unwrap();
        assert_eq!(result, "sk-from-env");
        std::env::remove_var("STARRIVER_TEST_KEY");
    }

    #[test]
    fn test_resolve_api_key_env_var_missing() {
        let result = ModelRegistry::resolve_api_key("${NONEXISTENT_VAR_FOR_TEST}");
        assert!(result.is_err());
    }

    #[test]
    fn test_migrate_skips_empty_and_prefixed() {
        let tmp = temp_dir();
        let path = tmp.join("migrate_skip.yaml");
        let entries: Vec<ModelEntry> = vec![
            ModelEntry {
                id: "empty".into(),
                name: "E".into(),
                model_type: "cloud".into(),
                provider_type: "openai-compat".into(),
                base_url: "".into(),
                api_key: "".into(),
                model_path: None,
                models: vec![],
                default_model: "m".into(),
                concurrency: None,
                ..Default::default()
            },
            ModelEntry {
                id: "keychain".into(),
                name: "K".into(),
                model_type: "cloud".into(),
                provider_type: "openai-compat".into(),
                base_url: "".into(),
                api_key: "!keychain:model/test".into(),
                model_path: None,
                models: vec![],
                default_model: "m".into(),
                concurrency: None,
                ..Default::default()
            },
            ModelEntry {
                id: "file-ref".into(),
                name: "F".into(),
                model_type: "cloud".into(),
                provider_type: "openai-compat".into(),
                base_url: "".into(),
                api_key: "!file:model/file-ref".into(),
                model_path: None,
                models: vec![],
                default_model: "m".into(),
                concurrency: None,
                ..Default::default()
            },
            ModelEntry {
                id: "env".into(),
                name: "V".into(),
                model_type: "cloud".into(),
                provider_type: "openai-compat".into(),
                base_url: "".into(),
                api_key: "${TEST_VAR}".into(),
                model_path: None,
                models: vec![],
                default_model: "m".into(),
                concurrency: None,
                ..Default::default()
            },
        ];
        let yaml = serde_yaml::to_string(&entries).unwrap();
        std::fs::write(&path, &yaml).unwrap();
        let mut reg = ModelRegistry {
            models: entries,
            config_path: path.clone(),
        };
        let count = reg.migrate_plaintext_keys().unwrap();
        assert_eq!(
            count, 0,
            "should skip empty, keychain/file-ref, and env-var entries"
        );
        // Verify entries unchanged
        let content = std::fs::read_to_string(&path).unwrap();
        let reloaded: Vec<ModelEntry> = serde_yaml::from_str(&content).unwrap();
        assert_eq!(reloaded[0].api_key, "");
        assert_eq!(reloaded[1].api_key, "!keychain:model/test");
        assert_eq!(reloaded[2].api_key, "!file:model/file-ref");
        assert_eq!(reloaded[3].api_key, "${TEST_VAR}");
    }

    #[test]
    fn test_resolve_api_key_malformed_env_var_no_closing_brace() {
        // Starts with ${ but no closing } → treated as plaintext
        let result = ModelRegistry::resolve_api_key("${INCOMPLETE").unwrap();
        assert_eq!(result, "${INCOMPLETE");
    }

    #[test]
    fn test_resolve_api_key_env_var_with_nested_braces() {
        // Only looks at first ${ and last }, so this is valid
        std::env::set_var("NESTED", "resolved-value");
        // The resolver strips outer ${...}, so inner braces are kept as-is
        let result = ModelRegistry::resolve_api_key("${NESTED}").unwrap();
        assert_eq!(result, "resolved-value");
        std::env::remove_var("NESTED");
    }

    #[test]
    fn test_resolve_api_key_with_whitespace_only() {
        // Whitespace is not a special prefix — treated as plaintext
        let result = ModelRegistry::resolve_api_key("   ").unwrap();
        assert_eq!(result, "   ");
    }

    // ── env-var name char-boundary regression tests ──
    // resolve_api_key extracts the var name from `${...}`. The old byte-slice [2..len-1]
    // was actually safe (both ends anchored at 1-byte ASCII `${` / `}`), and the current
    // strip_prefix/strip_suffix form is safe by construction. These tests pin that: a
    // multi-byte env var name must never panic and must be extracted byte-for-byte intact.

    #[test]
    fn test_resolve_api_key_multibyte_env_var_name_does_not_panic() {
        // Unset multi-byte name → clean error naming the full var, never a panic.
        let result = ModelRegistry::resolve_api_key("${TOKEN密钥}");
        assert!(
            result.is_err(),
            "multibyte var name unset must error, not panic"
        );
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("TOKEN密钥"),
            "error must name the full var: {msg}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_resolve_api_key_multibyte_env_var_name_resolves_when_set() {
        // Windows restricts non-ASCII env var names, so this runs on unix only.
        let name = format!("密钥{}", std::process::id());
        let old = std::env::var(&name).ok();
        std::env::set_var(&name, "sk-multibyte-key");
        let result = ModelRegistry::resolve_api_key(&format!("${{{name}}}"));
        std::env::remove_var(&name);
        if let Some(v) = old {
            std::env::set_var(&name, v);
        }
        assert_eq!(result.unwrap(), "sk-multibyte-key");
    }

    #[test]
    fn test_resolve_api_key_env_var_brace_matrix() {
        // Malformed / not-quite-`${...}` forms fall back to plaintext (returned as-is).
        assert_eq!(ModelRegistry::resolve_api_key("${VAR").unwrap(), "${VAR");
        assert_eq!(
            ModelRegistry::resolve_api_key("${VAR}x").unwrap(),
            "${VAR}x"
        );
        assert_eq!(
            ModelRegistry::resolve_api_key("x${VAR}").unwrap(),
            "x${VAR}"
        );
        assert_eq!(ModelRegistry::resolve_api_key("$VAR}").unwrap(), "$VAR}");
        // Empty name → resolves the empty env var, which is never set → clean error.
        let r = ModelRegistry::resolve_api_key("${}");
        assert!(r.is_err());
    }

    #[test]
    fn test_store_api_key_empty_returns_empty() {
        let result = ModelRegistry::store_api_key("test-model", "").unwrap();
        assert_eq!(result, "");
    }

    #[test]
    fn test_store_api_key_non_empty_returns_reference() {
        // Writes to the secrets file under a throwaway HOME.
        crate::testutil::with_temp_home(|_| {
            let reference = ModelRegistry::store_api_key("test-model", "sk-test-123").unwrap();
            assert_eq!(reference, "!file:model/test-model");
            // Resolving the ref reads the key back
            assert_eq!(
                ModelRegistry::resolve_api_key(&reference).unwrap(),
                "sk-test-123"
            );
            ModelRegistry::delete_api_key("test-model");
        });
    }

    #[test]
    fn test_delete_api_key_nonexistent_does_not_panic() {
        // Deleting a key that doesn't exist should not panic
        crate::testutil::with_temp_home(|_| {
            ModelRegistry::delete_api_key("nonexistent-model-12345");
        });
    }

    #[test]
    fn test_delete_api_key_then_resolve_fails() {
        // After delete, resolving the ref must fail (key was removed)
        crate::testutil::with_temp_home(|_| {
            let reference = ModelRegistry::store_api_key("delete-test", "sk-delete-me").unwrap();
            ModelRegistry::delete_api_key("delete-test");
            let resolve_result = ModelRegistry::resolve_api_key(&reference);
            assert!(
                resolve_result.is_err(),
                "resolve should fail after key deleted"
            );
        });
    }

    #[test]
    fn test_migrate_with_plaintext_keys_handles_keychain_failure() {
        // Migration writes to the file store (temp HOME), never panics.
        crate::testutil::with_temp_home(|_| {
            let tmp = temp_dir();
            let path = tmp.join("migrate_graceful.yaml");
            let entries: Vec<ModelEntry> = vec![ModelEntry {
                id: "has-plaintext".into(),
                name: "P".into(),
                model_type: "cloud".into(),
                provider_type: "openai-compat".into(),
                base_url: "https://api.example.com".into(),
                api_key: "sk-plaintext-key".into(),
                model_path: None,
                models: vec![ModelVariant {
                    id: "m1".into(),
                    name: "M1".into(),
                }],
                default_model: "m1".into(),
                concurrency: None,
                ..Default::default()
            }];
            let yaml = serde_yaml::to_string(&entries).unwrap();
            std::fs::write(&path, &yaml).unwrap();
            let mut reg = ModelRegistry {
                models: entries,
                config_path: path.clone(),
            };
            let result = reg.migrate_plaintext_keys();
            assert!(
                result.is_ok(),
                "migrate_plaintext_keys should always return Ok"
            );
            assert_eq!(
                result.unwrap(),
                1,
                "plaintext key must migrate to the file store"
            );
            let reloaded: Vec<ModelEntry> =
                serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            assert_eq!(reloaded[0].api_key, "!file:model/has-plaintext");
            ModelRegistry::delete_api_key("has-plaintext");
        });
    }

    #[test]
    fn store_api_key_passes_through_existing_references() {
        // References must round-trip untouched (no write) so re-saving an
        // existing entry never wraps the ref string as a literal key.
        assert_eq!(
            ModelRegistry::store_api_key("zhipu", "!file:model/zhipu").unwrap(),
            "!file:model/zhipu"
        );
        assert_eq!(
            ModelRegistry::store_api_key("zhipu", "!keychain:model/zhipu").unwrap(),
            "!keychain:model/zhipu"
        );
        assert_eq!(
            ModelRegistry::store_api_key("zhipu", "${ZHIPU_API_KEY}").unwrap(),
            "${ZHIPU_API_KEY}"
        );
        assert_eq!(ModelRegistry::store_api_key("zhipu", "").unwrap(), "");
    }

    #[test]
    fn test_resolve_api_key_file_ref() {
        // `!file:` refs resolve from the secrets file; missing accounts error
        // with an actionable message.
        crate::testutil::with_temp_home(|_| {
            let reference = ModelRegistry::store_api_key("deepseek", "sk-ds").unwrap();
            assert_eq!(ModelRegistry::resolve_api_key(&reference).unwrap(), "sk-ds");
            let err = ModelRegistry::resolve_api_key("!file:model/missing").unwrap_err();
            let msg = format!("{err}");
            assert!(
                msg.contains("re-enter"),
                "error should say how to fix: {msg}"
            );
        });
    }

    #[test]
    fn test_new_without_config_file() {
        // new() should work even when models.yaml doesn't exist
        crate::testutil::with_temp_home(|_| {
            let result = ModelRegistry::new();
            assert!(result.is_ok(), "new() should succeed with empty home dir");
            let reg = result.unwrap();
            assert!(reg.list().is_empty(), "fresh registry should be empty");
        });
    }

    #[test]
    fn test_new_with_existing_config_auto_migrates() {
        // Create a config with plaintext key → new() should migrate it to the
        // secrets file and rewrite the ref.
        crate::testutil::with_temp_home(|_| {
            let starriver_dir = starriver_secrets::secrets::home().join(".starriver");
            std::fs::create_dir_all(&starriver_dir).unwrap();
            let config_path = starriver_dir.join("models.yaml");
            let entries: Vec<ModelEntry> = vec![ModelEntry {
                id: "auto-migrate".into(),
                name: "Auto".into(),
                model_type: "cloud".into(),
                provider_type: "openai-compat".into(),
                base_url: "https://example.com".into(),
                api_key: "sk-auto-migrate".into(),
                model_path: None,
                models: vec![],
                default_model: "m".into(),
                concurrency: None,
                ..Default::default()
            }];
            std::fs::write(&config_path, serde_yaml::to_string(&entries).unwrap()).unwrap();
            let result = ModelRegistry::new();
            assert!(result.is_ok(), "new() should succeed with existing config");
            let reg = result.unwrap();
            assert_eq!(reg.list().len(), 1);
            assert_eq!(reg.list()[0].api_key, "!file:model/auto-migrate");
            ModelRegistry::delete_api_key("auto-migrate");
        });
    }

    #[test]
    fn test_migrate_mixed_entries() {
        // Mix of empty, already-migrated, env-var, and plaintext entries
        crate::testutil::with_temp_home(|_| {
            let tmp = temp_dir();
            let path = tmp.join("migrate_mixed.yaml");
            let entries: Vec<ModelEntry> = vec![
                ModelEntry {
                    id: "empty".into(),
                    name: "E".into(),
                    model_type: "cloud".into(),
                    provider_type: "openai-compat".into(),
                    base_url: "".into(),
                    api_key: "".into(),
                    model_path: None,
                    models: vec![],
                    default_model: "m".into(),
                    concurrency: None,
                    ..Default::default()
                },
                ModelEntry {
                    id: "already-done".into(),
                    name: "A".into(),
                    model_type: "cloud".into(),
                    provider_type: "openai-compat".into(),
                    base_url: "".into(),
                    api_key: "!keychain:model/already-done".into(),
                    model_path: None,
                    models: vec![],
                    default_model: "m".into(),
                    concurrency: None,
                    ..Default::default()
                },
                ModelEntry {
                    id: "to-migrate".into(),
                    name: "M".into(),
                    model_type: "cloud".into(),
                    provider_type: "openai-compat".into(),
                    base_url: "".into(),
                    api_key: "sk-migrate-me".into(),
                    model_path: None,
                    models: vec![],
                    default_model: "m".into(),
                    concurrency: None,
                    ..Default::default()
                },
            ];
            let yaml = serde_yaml::to_string(&entries).unwrap();
            std::fs::write(&path, &yaml).unwrap();
            let mut reg = ModelRegistry {
                models: entries,
                config_path: path.clone(),
            };
            let result = reg.migrate_plaintext_keys();
            assert!(result.is_ok(), "migration should not panic");
            assert_eq!(result.unwrap(), 1, "only the plaintext entry migrates");
            let reloaded: Vec<ModelEntry> =
                serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            assert_eq!(reloaded[0].api_key, "");
            assert_eq!(reloaded[1].api_key, "!keychain:model/already-done");
            assert_eq!(reloaded[2].api_key, "!file:model/to-migrate");
            ModelRegistry::delete_api_key("to-migrate");
        });
    }

    #[test]
    fn test_migrate_keychain_refs_missing_untouched() {
        // A `!keychain:` ref whose credential is gone must survive migration
        // untouched (the user re-enters the key), not be rewritten to `!file:`.
        crate::testutil::with_temp_home(|_| {
            let tmp = temp_dir();
            let path = tmp.join("migrate_keychain.yaml");
            let entries: Vec<ModelEntry> = vec![ModelEntry {
                id: "ghost".into(),
                name: "G".into(),
                model_type: "cloud".into(),
                provider_type: "openai-compat".into(),
                base_url: "".into(),
                api_key: "!keychain:model/ghost".into(),
                model_path: None,
                models: vec![],
                default_model: "m".into(),
                concurrency: None,
                ..Default::default()
            }];
            let yaml = serde_yaml::to_string(&entries).unwrap();
            std::fs::write(&path, &yaml).unwrap();
            let mut reg = ModelRegistry {
                models: entries,
                config_path: path.clone(),
            };
            let count = reg.migrate_keychain_refs().unwrap();
            assert_eq!(count, 0, "missing keychain entry must not migrate");
            let reloaded: Vec<ModelEntry> =
                serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            assert_eq!(reloaded[0].api_key, "!keychain:model/ghost");
        });
    }

    #[test]
    fn test_store_delete_keeps_other_accounts() {
        crate::testutil::with_temp_home(|_| {
            let r1 = ModelRegistry::store_api_key("a", "sk-a").unwrap();
            let r2 = ModelRegistry::store_api_key("b", "sk-b").unwrap();
            assert_eq!(r1, "!file:model/a");
            assert_eq!(r2, "!file:model/b");
            ModelRegistry::delete_api_key("a");
            assert!(
                ModelRegistry::resolve_api_key(&r1).is_err(),
                "deleted key must fail"
            );
            assert_eq!(
                ModelRegistry::resolve_api_key(&r2).unwrap(),
                "sk-b",
                "sibling key must survive"
            );
        });
    }

    #[test]
    fn test_store_api_key_special_char_model_id() {
        // Account is `model/{id}` verbatim — ids with `/`, `:` or unicode must
        // round-trip through the file without mangling.
        crate::testutil::with_temp_home(|_| {
            for id in ["my/model:1", "密钥-model", "a.b_c-d"] {
                let reference = ModelRegistry::store_api_key(id, &format!("sk-{id}")).unwrap();
                assert_eq!(reference, format!("!file:model/{id}"));
                assert_eq!(
                    ModelRegistry::resolve_api_key(&reference).unwrap(),
                    format!("sk-{id}")
                );
            }
        });
    }

    #[test]
    fn test_migrate_plaintext_multiple_entries() {
        crate::testutil::with_temp_home(|_| {
            let tmp = temp_dir();
            let path = tmp.join("migrate_multi.yaml");
            let entries: Vec<ModelEntry> = vec![
                ModelEntry {
                    id: "m1".into(),
                    name: "M1".into(),
                    model_type: "cloud".into(),
                    provider_type: "openai-compat".into(),
                    base_url: "".into(),
                    api_key: "sk-1".into(),
                    model_path: None,
                    models: vec![],
                    default_model: "m".into(),
                    concurrency: None,
                    ..Default::default()
                },
                ModelEntry {
                    id: "m2".into(),
                    name: "M2".into(),
                    model_type: "cloud".into(),
                    provider_type: "openai-compat".into(),
                    base_url: "".into(),
                    api_key: "sk-2".into(),
                    model_path: None,
                    models: vec![],
                    default_model: "m".into(),
                    concurrency: None,
                    ..Default::default()
                },
            ];
            let yaml = serde_yaml::to_string(&entries).unwrap();
            std::fs::write(&path, &yaml).unwrap();
            let mut reg = ModelRegistry {
                models: entries,
                config_path: path.clone(),
            };
            let count = reg.migrate_plaintext_keys().unwrap();
            assert_eq!(count, 2, "both plaintext entries migrate");
            let reloaded: Vec<ModelEntry> =
                serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            assert_eq!(reloaded[0].api_key, "!file:model/m1");
            assert_eq!(reloaded[1].api_key, "!file:model/m2");
            assert_eq!(
                ModelRegistry::resolve_api_key("!file:model/m1").unwrap(),
                "sk-1"
            );
            assert_eq!(
                ModelRegistry::resolve_api_key("!file:model/m2").unwrap(),
                "sk-2"
            );
            ModelRegistry::delete_api_key("m1");
            ModelRegistry::delete_api_key("m2");
        });
    }
}
