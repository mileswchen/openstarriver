use keyring::Entry;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A persisted cloud provider configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub id: String,
    pub api_key: String,
    #[serde(default)]
    pub base_url: Option<String>,
}

/// Registry that persists to ~/.starriver/providers.yaml
pub struct ProviderRegistry {
    providers: Vec<ProviderConfig>,
    config_path: PathBuf,
}

impl ProviderRegistry {
    pub fn new() -> anyhow::Result<Self> {
        let config_path = Self::default_path();
        let providers = if config_path.exists() {
            let content = std::fs::read_to_string(&config_path).unwrap_or_default();
            match serde_yaml::from_str(&content) {
                Ok(providers) => providers,
                Err(e) => {
                    // Preserve corrupt bytes — a later add() would otherwise
                    // overwrite the file with an empty list.
                    let backup = config_path.with_extension("yaml.corrupt");
                    if std::fs::copy(&config_path, &backup).is_ok() {
                        tracing::warn!(backup = %backup.display(), error = %e,
                            "providers.yaml corrupt; preserved and starting empty");
                    }
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };
        let mut registry = Self {
            providers,
            config_path,
        };
        // Auto-migrate plaintext keys to the secrets file, then legacy OS
        // keychain refs to the file store. Failures are non-fatal and retry
        // next launch.
        if let Err(e) = registry.migrate_plaintext_keys() {
            tracing::warn!(error = %e, "provider secrets migration failed on startup");
        }
        if let Err(e) = registry.migrate_keychain_refs() {
            tracing::warn!(error = %e, "provider keychain→file migration failed on startup");
        }
        Ok(registry)
    }

    fn default_path() -> PathBuf {
        let home = std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."));
        home.join(".starriver").join("providers.yaml")
    }

    pub fn list(&self) -> &[ProviderConfig] {
        &self.providers
    }

    pub fn is_enabled(&self, id: &str) -> bool {
        self.providers.iter().any(|p| p.id == id)
    }

    pub fn add(
        &mut self,
        id: String,
        api_key: String,
        base_url: Option<String>,
    ) -> anyhow::Result<()> {
        // In-place update (order is display + default order).
        if let Some(slot) = self.providers.iter_mut().find(|p| p.id == id) {
            slot.api_key = api_key;
            slot.base_url = base_url;
        } else {
            self.providers.push(ProviderConfig {
                id,
                api_key,
                base_url,
            });
        }
        self.save()
    }

    pub fn remove(&mut self, id: &str) -> anyhow::Result<()> {
        self.providers.retain(|p| p.id != id);
        self.save()
    }

    fn save(&self) -> anyhow::Result<()> {
        if let Some(parent) = self.config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let yaml = serde_yaml::to_string(&self.providers)?;
        std::fs::write(&self.config_path, yaml)?;
        Ok(())
    }

    /// Resolve an api_key field to its real value (`!file:` / `!keychain:` /
    /// `${ENV}` / plaintext).
    pub fn resolve_api_key(api_key_field: &str) -> anyhow::Result<String> {
        starriver_secrets::credentials::CredentialsStore::new()
            .resolve_field(api_key_field)
            .map_err(anyhow::Error::msg)
    }

    /// Store an API key in the managed credential records. Returns the
    /// reference string for YAML.
    pub fn store_api_key(provider_id: &str, api_key: &str) -> anyhow::Result<String> {
        if api_key.is_empty() {
            return Ok(String::new());
        }
        // Already a reference (file/keychain ref or env-var placeholder) — return
        // it unchanged instead of wrapping it as a literal key. Prevents re-saving
        // an existing entry (whose key field is a `!file:`/`!keychain:` ref) from
        // storing the ref string itself as the real credential.
        if starriver_secrets::secrets::is_reference(api_key) {
            return Ok(api_key.to_string());
        }
        let account = format!("provider/{}", provider_id);
        match starriver_secrets::credentials::CredentialKey::new("provider", provider_id) {
            Ok(key) => starriver_secrets::credentials::CredentialsStore::new()
                .set_api_key(&key, api_key)
                .map_err(anyhow::Error::msg),
            Err(_) => {
                starriver_secrets::secrets::set(&account, api_key)?;
                Ok(format!("!file:{}", account))
            }
        }
    }

    /// Delete an API key from the managed records (and any legacy
    /// OS-keychain entry). Non-fatal.
    pub fn delete_api_key(provider_id: &str) {
        let account = format!("provider/{}", provider_id);
        if let Ok(key) = starriver_secrets::credentials::CredentialKey::new("provider", provider_id)
        {
            let _ = starriver_secrets::credentials::CredentialsStore::new().unset(&key);
        } else {
            let _ = starriver_secrets::secrets::remove(&account);
        }
        // Legacy keychain cleanup — skipped when the keychain is disabled
        // (macOS securityd stalls unsigned test / headless binaries).
        if starriver_secrets::secrets::keychain_disabled() {
            return;
        }
        if let Ok(entry) = Entry::new("starriver", &account) {
            let _ = entry.delete_credential();
        }
    }

    /// Migrate plaintext API keys to the secrets file. Returns migrated count.
    pub fn migrate_plaintext_keys(&mut self) -> anyhow::Result<usize> {
        let mut count = 0;
        for entry in &mut self.providers {
            if entry.api_key.is_empty() || starriver_secrets::secrets::is_reference(&entry.api_key)
            {
                continue;
            }
            match Self::store_api_key(&entry.id, &entry.api_key) {
                Ok(reference) => {
                    tracing::info!(provider_id = %entry.id, "migrated provider API key to secrets file");
                    entry.api_key = reference;
                    count += 1;
                }
                Err(e) => {
                    tracing::warn!(provider_id = %entry.id, error = %e,
                        "failed to migrate provider API key to secrets file — key will remain in plaintext");
                }
            }
        }
        if count > 0 {
            self.save()?;
        }
        Ok(count)
    }

    /// Migrate legacy OS-keychain refs (`!keychain:provider/…`) to the secrets
    /// file. Entries whose keychain credential is gone are left untouched so
    /// the user can re-enter the key without a destructive rewrite.
    pub fn migrate_keychain_refs(&mut self) -> anyhow::Result<usize> {
        if starriver_secrets::secrets::keychain_disabled() {
            return Ok(0);
        }
        let mut count = 0;
        for entry in &mut self.providers {
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

    #[test]
    fn test_add_and_remove() {
        crate::testutil::with_temp_home(|dir| {
            let path = dir.join("providers.yaml");
            // Create an empty file at the expected path
            std::fs::write(&path, "[]").unwrap();

            let mut reg = ProviderRegistry {
                providers: Vec::new(),
                config_path: path.clone(),
            };
            reg.add("deepseek".into(), "sk-test".into(), None).unwrap();
            assert!(reg.is_enabled("deepseek"));
            assert_eq!(reg.list().len(), 1);

            reg.remove("deepseek").unwrap();
            assert!(!reg.is_enabled("deepseek"));
            assert!(reg.list().is_empty());
        });
    }

    #[test]
    fn test_persistence() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("providers.yaml");
        std::fs::write(&path, "[]").unwrap();

        let mut reg = ProviderRegistry {
            providers: Vec::new(),
            config_path: path.clone(),
        };
        reg.add(
            "qwen".into(),
            "sk-qwen".into(),
            Some("https://custom.api".into()),
        )
        .unwrap();
        drop(reg);

        let content = std::fs::read_to_string(&path).unwrap();
        let providers: Vec<ProviderConfig> = serde_yaml::from_str(&content).unwrap();
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].id, "qwen");
    }

    #[test]
    fn test_add_replaces_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("providers.yaml");
        std::fs::write(&path, "[]").unwrap();

        let mut reg = ProviderRegistry {
            providers: Vec::new(),
            config_path: path.clone(),
        };
        reg.add("test".into(), "key1".into(), None).unwrap();
        reg.add("test".into(), "key2".into(), None).unwrap();
        assert_eq!(reg.list().len(), 1);
    }

    // ── Keychain API key resolution tests ──

    #[test]
    fn test_resolve_api_key_plaintext() {
        let result = ProviderRegistry::resolve_api_key("sk-provider-test-456").unwrap();
        assert_eq!(result, "sk-provider-test-456");
    }

    #[test]
    fn test_resolve_api_key_empty() {
        let result = ProviderRegistry::resolve_api_key("").unwrap();
        assert_eq!(result, "");
    }

    #[test]
    fn test_resolve_api_key_env_var() {
        std::env::set_var("STARRIVER_PROVIDER_TEST_KEY", "sk-from-env-provider");
        let result = ProviderRegistry::resolve_api_key("${STARRIVER_PROVIDER_TEST_KEY}").unwrap();
        assert_eq!(result, "sk-from-env-provider");
        std::env::remove_var("STARRIVER_PROVIDER_TEST_KEY");
    }

    #[test]
    fn test_resolve_api_key_env_var_missing() {
        let result = ProviderRegistry::resolve_api_key("${STARRIVER_NONEXISTENT_VAR}");
        assert!(result.is_err());
    }

    #[test]
    fn test_resolve_api_key_malformed_env_var_no_closing_brace() {
        let result = ProviderRegistry::resolve_api_key("${MISSING_CLOSE").unwrap();
        assert_eq!(result, "${MISSING_CLOSE");
    }

    // ── env-var name char-boundary regression tests ──
    // Mirrors the model_registry suite: a multi-byte env var name must never panic and
    // must be extracted byte-for-byte intact.

    #[test]
    fn test_resolve_api_key_multibyte_env_var_name_does_not_panic() {
        let result = ProviderRegistry::resolve_api_key("${PROVIDER密钥}");
        assert!(
            result.is_err(),
            "multibyte var name unset must error, not panic"
        );
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("PROVIDER密钥"),
            "error must name the full var: {msg}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_resolve_api_key_multibyte_env_var_name_resolves_when_set() {
        let name = format!("密钥{}", std::process::id());
        let old = std::env::var(&name).ok();
        std::env::set_var(&name, "sk-multibyte-provider");
        let result = ProviderRegistry::resolve_api_key(&format!("${{{name}}}"));
        std::env::remove_var(&name);
        if let Some(v) = old {
            std::env::set_var(&name, v);
        }
        assert_eq!(result.unwrap(), "sk-multibyte-provider");
    }

    #[test]
    fn test_resolve_api_key_env_var_brace_matrix() {
        assert_eq!(ProviderRegistry::resolve_api_key("${VAR").unwrap(), "${VAR");
        assert_eq!(
            ProviderRegistry::resolve_api_key("${VAR}x").unwrap(),
            "${VAR}x"
        );
        assert_eq!(
            ProviderRegistry::resolve_api_key("x${VAR}").unwrap(),
            "x${VAR}"
        );
        assert_eq!(ProviderRegistry::resolve_api_key("$VAR}").unwrap(), "$VAR}");
        let r = ProviderRegistry::resolve_api_key("${}");
        assert!(r.is_err());
    }

    #[test]
    fn test_store_api_key_empty_returns_empty() {
        let result = ProviderRegistry::store_api_key("test-provider", "").unwrap();
        assert_eq!(result, "");
    }

    #[test]
    fn test_store_api_key_non_empty_returns_reference() {
        crate::testutil::with_temp_home(|_dir| {
            let reference =
                ProviderRegistry::store_api_key("test-provider", "sk-provider-test").unwrap();
            assert_eq!(reference, "!file:provider/test-provider");
            assert_eq!(
                ProviderRegistry::resolve_api_key(&reference).unwrap(),
                "sk-provider-test"
            );
            ProviderRegistry::delete_api_key("test-provider");
        });
    }

    #[test]
    fn test_store_api_key_passes_through_existing_references() {
        for existing in [
            "!file:provider/test-provider",
            "!keychain:provider/test-provider",
            "${PROV_PASSTHROUGH}",
        ] {
            let result = ProviderRegistry::store_api_key("test-provider", existing).unwrap();
            assert_eq!(result, existing, "reference must pass through unchanged");
        }
    }

    #[test]
    fn test_delete_api_key_nonexistent_does_not_panic() {
        crate::testutil::with_temp_home(|_dir| {
            ProviderRegistry::delete_api_key("nonexistent-provider-99999");
        });
    }

    #[test]
    fn test_delete_api_key_then_resolve_fails() {
        crate::testutil::with_temp_home(|_dir| {
            let reference =
                ProviderRegistry::store_api_key("delete-prov-test", "sk-delete-prov").unwrap();
            ProviderRegistry::delete_api_key("delete-prov-test");
            let resolve_result = ProviderRegistry::resolve_api_key(&reference);
            assert!(
                resolve_result.is_err(),
                "resolve should fail after key deleted"
            );
        });
    }

    #[test]
    fn test_resolve_api_key_file_ref_missing_is_actionable() {
        crate::testutil::with_temp_home(|_dir| {
            let err = ProviderRegistry::resolve_api_key("!file:provider/never-stored").unwrap_err();
            assert!(
                err.to_string().contains("re-enter"),
                "error should say how to fix: {err}"
            );
        });
    }

    #[test]
    fn test_migrate_skips_empty_and_prefixed() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("prov_skip.yaml");
        let entries: Vec<ProviderConfig> = vec![
            ProviderConfig {
                id: "empty".into(),
                api_key: "".into(),
                base_url: None,
            },
            ProviderConfig {
                id: "file".into(),
                api_key: "!file:provider/test".into(),
                base_url: None,
            },
            ProviderConfig {
                id: "keychain".into(),
                api_key: "!keychain:provider/test".into(),
                base_url: None,
            },
            ProviderConfig {
                id: "env".into(),
                api_key: "${SKIP_ME}".into(),
                base_url: None,
            },
        ];
        let yaml = serde_yaml::to_string(&entries).unwrap();
        std::fs::write(&path, &yaml).unwrap();
        let mut reg = ProviderRegistry {
            providers: entries,
            config_path: path.clone(),
        };
        let count = reg.migrate_plaintext_keys().unwrap();
        assert_eq!(
            count, 0,
            "should skip empty, file/keychain-ref, and env-var entries"
        );
    }

    #[test]
    fn test_migrate_plaintext_keys_to_file() {
        crate::testutil::with_temp_home(|dir| {
            let path = dir.join("prov_migrate.yaml");
            let entries: Vec<ProviderConfig> = vec![ProviderConfig {
                id: "plain".into(),
                api_key: "sk-provider-plaintext".into(),
                base_url: Some("https://api.example.com".into()),
            }];
            let yaml = serde_yaml::to_string(&entries).unwrap();
            std::fs::write(&path, &yaml).unwrap();
            let mut reg = ProviderRegistry {
                providers: entries,
                config_path: path.clone(),
            };
            let count = reg.migrate_plaintext_keys().unwrap();
            assert_eq!(count, 1, "the plaintext entry should migrate");
            let reloaded: Vec<ProviderConfig> =
                serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            assert_eq!(reloaded[0].api_key, "!file:provider/plain");
            // The key itself landed in the secrets file, resolvable via the ref.
            assert_eq!(
                ProviderRegistry::resolve_api_key(&reloaded[0].api_key).unwrap(),
                "sk-provider-plaintext"
            );
            ProviderRegistry::delete_api_key("plain");
        });
    }

    #[test]
    fn test_migrate_keychain_refs_leaves_missing_untouched() {
        // A `!keychain:` ref whose credential is gone must survive migration
        // untouched (the user re-enters the key), not be rewritten to `!file:`.
        crate::testutil::with_temp_home(|dir| {
            let path = dir.join("prov_keychain.yaml");
            let entries: Vec<ProviderConfig> = vec![ProviderConfig {
                id: "ghost".into(),
                api_key: "!keychain:provider/ghost".into(),
                base_url: None,
            }];
            let yaml = serde_yaml::to_string(&entries).unwrap();
            std::fs::write(&path, &yaml).unwrap();
            let mut reg = ProviderRegistry {
                providers: entries,
                config_path: path.clone(),
            };
            let count = reg.migrate_keychain_refs().unwrap();
            assert_eq!(count, 0, "missing keychain entry must not migrate");
            let reloaded: Vec<ProviderConfig> =
                serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            assert_eq!(reloaded[0].api_key, "!keychain:provider/ghost");
        });
    }

    #[test]
    fn test_new_migrates_plaintext_to_file() {
        // ProviderRegistry::new() auto-migrates a plaintext key to `!file:` and
        // the key lands in the secrets file, resolvable via the ref.
        crate::testutil::with_temp_home(|dir| {
            let starriver_dir = dir.join(".starriver");
            std::fs::create_dir_all(&starriver_dir).unwrap();
            let path = starriver_dir.join("providers.yaml");
            let entries: Vec<ProviderConfig> = vec![ProviderConfig {
                id: "plain".into(),
                api_key: "sk-provider-plain".into(),
                base_url: None,
            }];
            std::fs::write(&path, serde_yaml::to_string(&entries).unwrap()).unwrap();
            let reg = ProviderRegistry::new().expect("new boots");
            assert_eq!(reg.list()[0].api_key, "!file:provider/plain");
            assert_eq!(
                ProviderRegistry::resolve_api_key(&reg.list()[0].api_key).unwrap(),
                "sk-provider-plain"
            );
            ProviderRegistry::delete_api_key("plain");
        });
    }

    #[test]
    fn test_new_preserves_file_refs() {
        // A config already using `!file:` refs must not be re-migrated.
        crate::testutil::with_temp_home(|dir| {
            let starriver_dir = dir.join(".starriver");
            std::fs::create_dir_all(&starriver_dir).unwrap();
            let path = starriver_dir.join("providers.yaml");
            let entries: Vec<ProviderConfig> = vec![ProviderConfig {
                id: "ref".into(),
                api_key: "!file:provider/ref".into(),
                base_url: None,
            }];
            std::fs::write(&path, serde_yaml::to_string(&entries).unwrap()).unwrap();
            let reg = ProviderRegistry::new().expect("new boots");
            assert_eq!(reg.list()[0].api_key, "!file:provider/ref");
        });
    }

    #[test]
    fn test_resolve_api_key_keychain_missing_is_actionable() {
        // Read-only for a never-used account: a healthy keychain yields NoEntry
        // (→ "re-enter"), but under heavy parallel load the macOS default
        // keychain may be unreachable and report a platform failure instead.
        // Both must produce a non-panicking, account-naming error. The NoEntry
        // "re-enter" path itself is covered deterministically in the core
        // secrets suite (same code path via secrets::resolve).
        let account = format!("provider/never-stored-{}", uuid::Uuid::new_v4());
        let err = ProviderRegistry::resolve_api_key(&format!("!keychain:{account}")).unwrap_err();
        let msg = err.to_string();
        assert!(!msg.is_empty(), "resolve error must not be empty");
        assert!(
            msg.contains(&account),
            "error should name the account: {msg}"
        );
    }
}
