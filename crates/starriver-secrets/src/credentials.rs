//! File-backed credential records + references (`credentials` /
//! `credentials-local` parity).
//!
//! `~/.starriver/.credentials.yaml` holds a versioned document with two
//! sections:
//! - `refs` — environment-variable-name → value entries (`CredentialRef`)
//! - `records` — `<scope>/<id>` → tagged records (`api-key` / opaque `grant`)
//!
//! Resolution is layered (order): the writable managed store wins over
//! the legacy `secrets.json` fallback, and env-var references resolve from the
//! process environment. Configuration surfaces only ever see
//! [`CredentialRecordInfo`] — never the value.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

const DOCUMENT_VERSION: u32 = 1;

/// Address of one stored credential record: `<scope>/<id>`. The scope is the
/// owning plugin's name (`model` / `provider` today); the id is that owner's
/// own addressing unit (a provider route key).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialKey {
    scope: String,
    id: String,
}

impl CredentialKey {
 /// Build a key from a scope + id with key grammar: scope must be a
    /// lowercase hyphenated identifier; id must be non-empty and contain no
 /// `/` or control characters (kept looser than so existing model ids
    /// like `deepseek-chat` and user-chosen ids keep working).
    pub fn new(scope: impl Into<String>, id: impl Into<String>) -> Result<Self, String> {
        let scope = scope.into();
        let id = id.into();
        let scope_ok = !scope.is_empty()
            && scope.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
            && scope
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        let id_ok = !id.is_empty() && !id.contains('/') && id.chars().all(|c| !c.is_control());
        if !scope_ok {
            return Err(format!(
                "credential key scope `{scope}` must be a lowercase hyphenated identifier"
            ));
        }
        if !id_ok {
            return Err(
                "credential key id must be non-empty and contain no `/` or control characters"
                    .into(),
            );
        }
        Ok(Self { scope, id })
    }

    /// Parse a joined `<scope>/<id>` string.
    pub fn parse(value: &str) -> Result<Self, String> {
        let (scope, id) = value
            .split_once('/')
            .ok_or_else(|| format!("credential key `{value}` must be `<scope>/<id>`"))?;
        Self::new(scope, id)
    }

    pub fn as_str(&self) -> String {
        format!("{}/{}", self.scope, self.id)
    }

    pub fn scope(&self) -> &str {
        &self.scope
    }

    pub fn id(&self) -> &str {
        &self.id
    }
}

impl std::fmt::Display for CredentialKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.scope, self.id)
    }
}

/// An API-key record: a secret value, provider environment values, or both.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<BTreeMap<String, String>>,
}

/// One durable credential record, tagged by what the seam may do with it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum CredentialRecord {
    ApiKey(ApiKeyRecord),
    Grant {
        /// Owner-defined JSON/YAML payload, opaque to the seam.
        payload: serde_yaml::Value,
    },
}

impl CredentialRecord {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::ApiKey(_) => "api-key",
            Self::Grant { .. } => "grant",
        }
    }

    pub fn is_configured(&self) -> bool {
        match self {
            Self::ApiKey(record) => {
                record.key.as_deref().is_some_and(|k| !k.is_empty())
                    || record.env.as_ref().is_some_and(|e| !e.is_empty())
            }
            Self::Grant { .. } => true,
        }
    }
}

/// Presence and writability facts for one record — safe for configuration
/// UIs, never the value (`CredentialRecordInfo`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialRecordInfo {
    pub configured: bool,
    pub kind: Option<String>,
    pub writable: bool,
}

/// One stored record's address and tag, for enumeration — never its value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialRecordEntry {
    pub key: String,
    pub kind: String,
}

/// Versioned credentials document (`CredentialsDocument`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CredentialsDocument {
    pub refs: BTreeMap<String, String>,
    pub records: BTreeMap<String, CredentialRecord>,
}

impl CredentialsDocument {
    fn with_version(&self) -> serde_yaml::Value {
        let mut map = serde_yaml::Mapping::new();
        map.insert(
            serde_yaml::Value::String("version".into()),
            serde_yaml::Value::Number(DOCUMENT_VERSION.into()),
        );
        map.insert(
            serde_yaml::Value::String("refs".into()),
            serde_yaml::to_value(&self.refs).unwrap_or_default(),
        );
        map.insert(
            serde_yaml::Value::String("records".into()),
            serde_yaml::to_value(&self.records).unwrap_or_default(),
        );
        serde_yaml::Value::Mapping(map)
    }
}

/// File-backed credentials store over `~/.starriver/.credentials.yaml`.
/// Writes re-read the document, patch only the touched entry, and persist
/// atomically with 0600 (`credentials-local`).
#[derive(Debug, Clone)]
pub struct CredentialsStore {
    path: PathBuf,
}

impl Default for CredentialsStore {
    fn default() -> Self {
        Self::new()
    }
}

impl CredentialsStore {
    pub fn new() -> Self {
        Self {
            path: Self::default_path(),
        }
    }

    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    fn default_path() -> PathBuf {
        home().join(".starriver").join(".credentials.yaml")
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    fn load_document(&self) -> Result<CredentialsDocument, String> {
        if !self.path.exists() {
            return Ok(CredentialsDocument::default());
        }
        let content = std::fs::read_to_string(&self.path).map_err(|e| e.to_string())?;
        if content.trim().is_empty() {
            return Ok(CredentialsDocument::default());
        }
        let value: serde_yaml::Value = serde_yaml::from_str(&content).map_err(|e| e.to_string())?;
        let mapping = value
            .as_mapping()
            .ok_or("credentials document must be a mapping")?;
        if mapping.is_empty() {
            return Ok(CredentialsDocument::default());
        }
        let version = mapping
            .get(serde_yaml::Value::String("version".into()))
            .and_then(serde_yaml::Value::as_u64)
            .ok_or("credentials document must declare `version`")?;
        if version != DOCUMENT_VERSION as u64 {
            return Err(format!(
                "credentials document declares version {version}; this build reads {DOCUMENT_VERSION}"
            ));
        }
        let doc: CredentialsDocument = serde_yaml::from_value(value)
            .map_err(|e| format!("parse credentials document: {e}"))?;
        Ok(doc)
    }

    fn save_document(&self, doc: &CredentialsDocument) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let yaml = serde_yaml::to_string(&doc.with_version()).map_err(|e| e.to_string())?;
        let tmp = PathBuf::from(format!("{}.tmp", self.path.display()));
        std::fs::write(&tmp, yaml).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| e.to_string())?;
        }
        std::fs::rename(&tmp, &self.path).map_err(|e| e.to_string())?;
        Ok(())
    }

    /// UI-safe presence/writability facts for one record (never the value).
    pub fn describe_record(&self, key: &CredentialKey) -> Result<CredentialRecordInfo, String> {
        let doc = self.load_document()?;
        Ok(match doc.records.get(&key.as_str()) {
            Some(record) => CredentialRecordInfo {
                configured: record.is_configured(),
                kind: Some(record.kind().to_string()),
                writable: true,
            },
            None => CredentialRecordInfo {
                configured: false,
                kind: None,
                writable: true,
            },
        })
    }

    /// Enumerate stored records (key + kind, never the value).
    pub fn list_records(&self) -> Result<Vec<CredentialRecordEntry>, String> {
        let doc = self.load_document()?;
        Ok(doc
            .records
            .iter()
            .map(|(key, record)| CredentialRecordEntry {
                key: key.clone(),
                kind: record.kind().to_string(),
            })
            .collect())
    }

    /// Write an API-key record. Returns the `!file:<key>` reference string to
    /// store in configuration (existing `!file:` grammar preserved).
    pub fn set_api_key(&self, key: &CredentialKey, value: &str) -> Result<String, String> {
        if value.is_empty() {
            return Err("credential value must not be empty".into());
        }
        let mut doc = self.load_document()?;
        doc.records.insert(
            key.as_str(),
            CredentialRecord::ApiKey(ApiKeyRecord {
                key: Some(value.to_string()),
                env: None,
            }),
        );
        self.save_document(&doc)?;
        Ok(format!("!file:{}", key.as_str()))
    }

    /// Store an opaque grant record (owner format; only a YAML round-trip is
    /// guaranteed).
    pub fn set_grant(&self, key: &CredentialKey, payload: serde_yaml::Value) -> Result<(), String> {
        let mut doc = self.load_document()?;
        doc.records
            .insert(key.as_str(), CredentialRecord::Grant { payload });
        self.save_document(&doc)
    }

    /// Remove one record (and its legacy `secrets.json` fallback so the
    /// reference truly resolves to nothing).
    pub fn unset(&self, key: &CredentialKey) -> Result<(), String> {
        let mut doc = self.load_document()?;
        // BUG-27: removing an absent record is a no-op — writing the document
        // anyway materialized the credentials file out of thin air (the
        // secrets::remove contract below is no-op-safe the same way).
        if doc.records.remove(&key.as_str()).is_some() {
            self.save_document(&doc)?;
        }
        // Legacy fallback cleanup: the same account in secrets.json must not
        // resurrect the credential.
        let _ = crate::secrets::remove(&key.as_str());
        Ok(())
    }

 /// Resolve one stored credential field with layering:
    /// - `!file:<key>` → managed records first, then legacy secrets fallback
    /// - `${ENV}` / `!keychain:` / plaintext → legacy semantics
    pub fn resolve_field(&self, field: &str) -> Result<String, String> {
        if let Some(key) = field.strip_prefix("!file:") {
            if let Ok(key) = CredentialKey::parse(key) {
                let doc = self.load_document()?;
                if let Some(record) = doc.records.get(&key.as_str()) {
                    if let CredentialRecord::ApiKey(api) = record {
                        if let Some(value) = api.key.as_deref().filter(|k| !k.is_empty()) {
                            return Ok(value.to_string());
                        }
                    }
                    if record.is_configured() {
                        return Err(format!(
                            "credential `{key}` is configured via env/grant, not a resolvable api key"
                        ));
                    }
                }
            }
            // Legacy fallback: secrets.json account / keychain / env.
            return crate::secrets::resolve(field).map_err(|e| e.to_string());
        }
        crate::secrets::resolve(field).map_err(|e| e.to_string())
    }
}

fn home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_grammar_accepts_and_rejects() {
        assert!(CredentialKey::new("model", "deepseek-chat").is_ok());
        assert!(CredentialKey::new("provider", "openai-compat").is_ok());
        assert!(
            CredentialKey::new("Model", "x").is_err(),
            "scope must be lowercase"
        );
        assert!(
            CredentialKey::new("model", "").is_err(),
            "id must be non-empty"
        );
        assert!(
            CredentialKey::new("model", "a/b").is_err(),
            "id must not contain /"
        );
        let parsed = CredentialKey::parse("model/deepseek-chat").unwrap();
        assert_eq!(parsed.as_str(), "model/deepseek-chat");
        assert!(CredentialKey::parse("no-slash").is_err());
    }

    #[test]
    fn set_describe_list_unset_roundtrip() {
        crate::secrets::with_temp_home(|| {
            let store = CredentialsStore::new();
            let key = CredentialKey::new("model", "ds").unwrap();
            assert!(!store.describe_record(&key).unwrap().configured);

            let reference = store.set_api_key(&key, "sk-ds").unwrap();
            assert_eq!(reference, "!file:model/ds");

            let info = store.describe_record(&key).unwrap();
            assert!(info.configured);
            assert_eq!(info.kind.as_deref(), Some("api-key"));
            assert!(info.writable);

            let entries = store.list_records().unwrap();
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].key, "model/ds");
            assert_eq!(entries[0].kind, "api-key");

            assert_eq!(store.resolve_field("!file:model/ds").unwrap(), "sk-ds");

            store.unset(&key).unwrap();
            assert!(!store.describe_record(&key).unwrap().configured);
            let err = store.resolve_field("!file:model/ds").unwrap_err();
            assert!(
                err.contains("re-enter"),
                "legacy fallback surfaces the actionable message: {err}"
            );
        });
    }

    #[test]
    fn records_win_over_legacy_secrets_fallback() {
        crate::secrets::with_temp_home(|| {
            let store = CredentialsStore::new();
            let key = CredentialKey::new("model", "ds").unwrap();
            // Legacy secrets.json has a stale value.
            crate::secrets::set("model/ds", "stale").unwrap();
            assert_eq!(store.resolve_field("!file:model/ds").unwrap(), "stale");
            // Managed record wins after a write.
            store.set_api_key(&key, "fresh").unwrap();
            assert_eq!(store.resolve_field("!file:model/ds").unwrap(), "fresh");
        });
    }

    #[test]
    fn file_is_0600_and_versioned() {
        crate::secrets::with_temp_home(|| {
            let store = CredentialsStore::new();
            store
                .set_api_key(&CredentialKey::new("model", "x").unwrap(), "sk-x")
                .unwrap();
            // POSIX mode is unix-only; the versioned-content shape below is
            // platform-independent and keeps running on Windows.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(store.path())
                    .unwrap()
                    .permissions()
                    .mode();
                assert_eq!(mode & 0o777, 0o600);
            }
            let content = std::fs::read_to_string(store.path()).unwrap();
            assert!(content.contains("version: 1"), "{content}");
            assert!(content.contains("records:"), "{content}");
        });
    }

    #[test]
    fn env_refs_resolve_from_environment() {
        crate::secrets::with_temp_home(|| {
            let unique = format!("STARRIVER_CRED_TEST_{}", uuid::Uuid::new_v4().simple());
            std::env::set_var(&unique, "sk-env");
            let store = CredentialsStore::new();
            assert_eq!(
                store.resolve_field(&format!("${{{unique}}}")).unwrap(),
                "sk-env"
            );
            std::env::remove_var(&unique);
        });
    }

    /// BUG-27 regression: `unset` on a key that was never stored is a no-op
    /// (same contract as `secrets::remove`, which is tested to not create
    /// files on a missing key) — it must not materialize the credentials
    /// file out of thin air on a fresh install.
    #[test]
    fn unset_missing_key_does_not_create_the_store() {
        crate::secrets::with_temp_home(|| {
            let store = CredentialsStore::new();
            assert!(!store.path().exists());
            let key = CredentialKey::new("model", "never-existed").unwrap();
            store.unset(&key).unwrap();
            assert!(
                !store.path().exists(),
                "a no-op unset must not create {}",
                store.path().display()
            );
        });
    }
}
