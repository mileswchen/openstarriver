//! Credential-record administration (`describe` / `set` / `unset` seam
//! parity). Configuration surfaces see presence/writability facts — never the
//! stored value.

use starriver_kernel::llm::LlmRuntime;
use starriver_secrets::credentials::{CredentialKey, CredentialsStore};

use crate::{types, HostRuntime};

impl HostRuntime {
    /// UI-safe facts about one credential record (configured / kind /
    /// writable), resolving nothing and returning no value.
    pub fn describe_credential(&self, key: String) -> anyhow::Result<types::CredentialRecordInfo> {
        let key = CredentialKey::parse(&key).map_err(|e| anyhow::anyhow!("{e}"))?;
        let info = CredentialsStore::new()
            .describe_record(&key)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(types::CredentialRecordInfo {
            configured: info.configured,
            kind: info.kind,
            writable: info.writable,
        })
    }

    /// Enumerate stored records (key + kind, never the value).
    pub fn list_credential_records(&self) -> Vec<types::CredentialRecordEntry> {
        CredentialsStore::new()
            .list_records()
            .unwrap_or_default()
            .into_iter()
            .map(|e| types::CredentialRecordEntry {
                key: e.key,
                kind: e.kind,
            })
            .collect()
    }

    /// Write one API-key record; returns the `!file:<key>` reference for
    /// configuration.
    pub fn set_credential_api_key(&self, key: String, value: String) -> anyhow::Result<String> {
        let key = CredentialKey::parse(&key).map_err(|e| anyhow::anyhow!("{e}"))?;
        CredentialsStore::new()
            .set_api_key(&key, &value)
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    /// Read back one credential record's value. Used by the models settings
    /// card to show the REAL stored key on explicit edit (the field is masked
    /// by default and blank-save keeps the stored reference). Resolves
    /// `!file:` records, env fallbacks, and legacy accounts.
    pub fn read_credential(&self, key: String) -> anyhow::Result<String> {
        let key = CredentialKey::parse(&key).map_err(|e| anyhow::anyhow!("{e}"))?;
        let reference = format!("!file:{}", key.as_str());
        CredentialsStore::new()
            .resolve_field(&reference)
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    /// Remove one credential record (records + legacy fallback).
    pub fn unset_credential(&self, key: String) -> anyhow::Result<()> {
        let key = CredentialKey::parse(&key).map_err(|e| anyhow::anyhow!("{e}"))?;
        CredentialsStore::new()
            .unset(&key)
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    /// Surface the kernel's configurable-provider directory to the settings
    /// UI (LLP §4.4 configuration face): every declared provider gets a card,
    /// configured or not, so the configuration surface exists before any
    /// route is activated.
    pub fn list_configurable_providers(&self) -> Vec<types::ConfigurableProvider> {
        let inner = self.inner.blocking_lock();
        let Some(inner) = inner.as_ref() else {
            return Vec::new();
        };
        let settings = self.settings.read().unwrap();
        inner
            .agent
            .llm()
            .list_configurable_providers()
            .into_iter()
            .map(|p| {
                let configured = settings
                    .llm
                    .providers
                    .iter()
                    .find(|entry| entry.id == p.provider)
                    .is_some_and(|entry| !entry.api_key.is_empty() || !entry.base_url.is_empty());
                types::ConfigurableProvider {
                    provider: p.provider,
                    display_name: p.display_name,
                    settings_ns: p.settings_ns,
                    settings_path: p.settings_path,
                    declared: p.declared.unwrap_or(false),
                    configured,
                }
            })
            .collect()
    }
}
