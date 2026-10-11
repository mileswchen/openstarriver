//! File-backed secret store (`~/.starriver/secrets.json`, mode 0600).
//!
//! Holds account-scoped API keys for models and providers. Follows the
//! OpenCode `auth.json` pattern: a portable, inspectable plaintext file
//! protected by 0600 + FileVault, instead of the opaque OS keychain — whose
//! entries silently vanish (a macOS failure mode we hit: config kept
//! `!keychain:` refs while the keychain had no matching entries).
//!
//! Config reference scheme: `!file:<account>` (alongside `!keychain:<account>`
//! and `${ENV_VAR}`, which stay supported for backward compatibility). The
//! account is a namespaced id like `model/deepseek` or `provider/openai`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::Context;

/// Path of the secrets file. Honors `$HOME` so tests can isolate it.
pub fn secrets_path() -> PathBuf {
    home().join(".starriver").join("secrets.json")
}

pub fn home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// Serializes all `$HOME` mutation in the lib test binary. `std::env::set_var`
/// is process-global, so parallel test threads that each swap `$HOME` race —
/// a thread's write can land in whatever dir another thread is currently
/// pointing at. Every HOME-swapping test must go through [`with_temp_home`].
#[cfg(test)]
pub(crate) static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Point `$HOME` at a throwaway dir for the duration of `f`, restoring it
/// afterwards. Serialized via [`HOME_LOCK`] so the dir swap is race-free.
#[cfg(test)]
pub(crate) fn with_temp_home<F: FnOnce()>(f: F) {
    let _guard = HOME_LOCK.lock().unwrap_or_else(|e| {
        HOME_LOCK.clear_poison();
        e.into_inner()
    });
    let old = std::env::var("HOME").ok();
    let home = std::env::temp_dir().join(format!("starriver-home-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&home).unwrap();
    std::env::set_var("HOME", &home);
    f();
    let _ = std::fs::remove_dir_all(&home);
    if let Some(h) = old {
        std::env::set_var("HOME", h);
    } else {
        std::env::remove_var("HOME");
    }
}

/// Read all secrets. A missing/empty file yields an empty map.
pub fn load() -> anyhow::Result<BTreeMap<String, String>> {
    let path = secrets_path();
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let content =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    if content.trim().is_empty() {
        return Ok(BTreeMap::new());
    }
    serde_json::from_str(&content).with_context(|| format!("parse {}", path.display()))
}

/// Headless/test escape hatch: `STARRIVER_DISABLE_KEYCHAIN=1` short-circuits
/// every OS-keychain round trip (macOS `securityd` can block unsigned test
/// binaries for many seconds per call). Callers treat it as "no entry".
pub fn keychain_disabled() -> bool {
    std::env::var_os("STARRIVER_DISABLE_KEYCHAIN").is_some()
}

/// Look up one secret by account.
pub fn get(account: &str) -> anyhow::Result<Option<String>> {
    Ok(load()?.remove(account))
}

/// Insert/overwrite a secret, then write the file atomically with 0600 perms.
pub fn set(account: &str, value: &str) -> anyhow::Result<()> {
    let mut map = load()?;
    map.insert(account.to_string(), value.to_string());
    write_all(&map)
}

/// Remove a secret. No-op if absent.
pub fn remove(account: &str) -> anyhow::Result<()> {
    let mut map = load()?;
    if map.remove(account).is_some() {
        write_all(&map)?;
    }
    Ok(())
}

/// Whether a config field is already a reference (`!file:` / `!keychain:` /
/// `${ENV}`) rather than a literal key. Such refs must never be wrapped as a
/// literal credential.
pub fn is_reference(field: &str) -> bool {
    field.starts_with("!file:")
        || field.starts_with("!keychain:")
        || (field.starts_with("${") && field.ends_with("}"))
}

/// Resolve an api_key field to its real value:
/// - `!file:<account>` — read from `~/.starriver/secrets.json`
/// - `!keychain:<account>` — read from the OS keychain (legacy)
/// - `${ENV_VAR}` — read from the environment
/// - plaintext — returned as-is (backward compatible)
pub fn resolve(field: &str) -> anyhow::Result<String> {
    if field.is_empty() {
        return Ok(String::new());
    }
    if let Some(account) = field.strip_prefix("!file:") {
        return load()?.get(account).cloned().ok_or_else(|| {
            anyhow::anyhow!(
                "API key '{account}' missing from {} — re-enter it in the Models panel",
                secrets_path().display()
            )
        });
    }
    if let Some(account) = field.strip_prefix("!keychain:") {
        if keychain_disabled() {
            anyhow::bail!(
                "API key '{account}' lives in the OS keychain and the keychain is disabled"
            );
        }
        let entry = keyring::Entry::new("starriver", account)?;
        return match entry.get_password() {
            Ok(pw) => Ok(pw),
            Err(e) => match &e {
                keyring::Error::NoEntry => Err(anyhow::anyhow!(
                    "API key '{account}' missing from the OS keychain — re-enter it in the Models panel"
                )),
                _ => Err(anyhow::anyhow!("keychain read failed for '{account}': {e}")),
            },
        };
    }
    if let Some(var) = field.strip_prefix("${").and_then(|s| s.strip_suffix("}")) {
        return std::env::var(var).map_err(|_| anyhow::anyhow!("env var '{var}' not set"));
    }
    Ok(field.to_string())
}

/// Atomically write the map to the secrets file with 0600 permissions.
/// Writes to a temp file first, then renames over the target so a concurrent
/// reader never sees a partially-written file.
fn write_all(map: &BTreeMap<String, String>) -> anyhow::Result<()> {
    use std::io::Write;
    let path = secrets_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(map)?;
    let tmp = PathBuf::from(format!("{}.tmp", path.display()));
    {
        let mut f =
            std::fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        f.write_all(json.as_bytes())?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod {}", tmp.display()))?;
    }
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("rename {} → {}", tmp.display(), path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    // `with_temp_home` comes from the parent module (super::* above); it holds
    // the shared HOME_LOCK so HOME-swapping tests can't race each other.

    #[test]
    fn set_get_remove_roundtrip() {
        with_temp_home(|| {
            set("model/deepseek", "sk-1").unwrap();
            assert_eq!(get("model/deepseek").unwrap(), Some("sk-1".into()));
            assert_eq!(get("model/zhipu").unwrap(), None);
            remove("model/deepseek").unwrap();
            assert_eq!(get("model/deepseek").unwrap(), None);
        });
    }

    // POSIX mode semantics — no Windows equivalent assertion (ACLs differ).
    #[cfg(unix)]
    #[test]
    fn file_permissions_are_0600() {
        with_temp_home(|| {
            set("model/x", "sk-x").unwrap();
            let mode = std::fs::metadata(secrets_path())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "secrets file must be owner-only");
        });
    }

    #[test]
    fn missing_file_is_empty_map() {
        with_temp_home(|| {
            assert!(load().unwrap().is_empty());
            assert_eq!(get("anything").unwrap(), None);
        });
    }

    #[test]
    fn resolve_file_account() {
        with_temp_home(|| {
            set("model/deepseek", "sk-ds").unwrap();
            assert_eq!(resolve("!file:model/deepseek").unwrap(), "sk-ds");
            let err = resolve("!file:model/missing").unwrap_err();
            assert!(err.to_string().contains("re-enter"), "{}", err);
        });
    }

    #[test]
    fn is_reference_detects_all_forms() {
        assert!(is_reference("!file:model/x"));
        assert!(is_reference("!keychain:model/x"));
        assert!(is_reference("${X}"));
        assert!(!is_reference("sk-plain"));
        assert!(!is_reference(""));
    }

    #[test]
    fn resolve_plaintext_and_env() {
        std::env::set_var("STARRIVER_SECRETS_TEST", "sk-env");
        with_temp_home(|| {
            assert_eq!(resolve("sk-literal").unwrap(), "sk-literal");
            assert_eq!(resolve("${STARRIVER_SECRETS_TEST}").unwrap(), "sk-env");
        });
        std::env::remove_var("STARRIVER_SECRETS_TEST");
    }

    // -- Boundaries and error paths --

    #[test]
    fn load_malformed_json_errors_not_panics() {
        with_temp_home(|| {
            std::fs::create_dir_all(secrets_path().parent().unwrap()).unwrap();
            std::fs::write(secrets_path(), "not-json{{{").unwrap();
            let err = load().unwrap_err();
            assert!(err.to_string().contains("parse"), "{}", err);
        });
    }

    #[test]
    fn load_whitespace_only_is_empty_map() {
        with_temp_home(|| {
            std::fs::create_dir_all(secrets_path().parent().unwrap()).unwrap();
            std::fs::write(secrets_path(), "  \n\t ").unwrap();
            assert!(load().unwrap().is_empty());
        });
    }

    #[test]
    fn set_overwrites_and_preserves_other_accounts() {
        with_temp_home(|| {
            set("model/a", "ka").unwrap();
            set("model/b", "kb").unwrap();
            set("model/a", "ka2").unwrap();
            let map = load().unwrap();
            assert_eq!(map.get("model/a").unwrap(), "ka2");
            assert_eq!(map.get("model/b").unwrap(), "kb");
            assert_eq!(map.len(), 2, "overwrite must not grow the map");
        });
    }

    #[test]
    fn remove_nonexistent_does_not_create_file() {
        with_temp_home(|| {
            remove("model/never").unwrap();
            assert!(
                !secrets_path().exists(),
                "remove of an absent account must not create the file"
            );
        });
    }

    #[test]
    fn no_tmp_file_leftover_after_write() {
        with_temp_home(|| {
            set("model/x", "kx").unwrap();
            let tmp = PathBuf::from(format!("{}.tmp", secrets_path().display()));
            assert!(!tmp.exists(), "atomic write must not leave a .tmp behind");
        });
    }

    #[test]
    fn is_reference_edge_cases() {
        assert!(
            is_reference("!file:"),
            "empty account after prefix is still a ref form"
        );
        assert!(!is_reference("!file"), "no colon means plaintext");
        assert!(!is_reference("!keychain"), "no colon means plaintext");
        assert!(is_reference("${}"), "empty var is still a ref form");
        assert!(!is_reference("${"), "unclosed brace is not a ref form");
        assert!(!is_reference("$VAR}"), "unopened brace is not a ref form");
        assert!(
            !is_reference("!FILE:model/x"),
            "ref prefixes are case-sensitive"
        );
        assert!(!is_reference("!"), "bare bang is plaintext");
        assert!(!is_reference(""), "empty is not a ref");
        assert!(!is_reference("sk-plain"), "real key is not a ref");
    }

    #[test]
    fn resolve_file_empty_account_errors_actionable() {
        with_temp_home(|| {
            let err = resolve("!file:").unwrap_err();
            assert!(err.to_string().contains("re-enter"), "{}", err);
        });
    }

    #[test]
    fn resolve_keychain_missing_is_actionable() {
        // Read-only: an account that has never existed yields NoEntry (→
        // "re-enter") on a healthy keychain, or a platform failure when the
        // default keychain isn't reachable (e.g. many test processes at once).
        // Both must produce a non-panicking, account-naming error.
        let account = format!("model/never-stored-{}", uuid::Uuid::new_v4());
        let err = resolve(&format!("!keychain:{account}")).unwrap_err();
        let msg = err.to_string();
        assert!(!msg.is_empty(), "resolve error must not be empty");
        assert!(
            msg.contains(&account),
            "error should name the account: {msg}"
        );
    }

    #[test]
    fn resolve_env_missing_errors() {
        let name = format!("STARRIVER_UNSET_{}", uuid::Uuid::new_v4());
        let err = resolve(&format!("${{{name}}}")).unwrap_err();
        assert!(err.to_string().contains(&name), "{}", err);
    }

    #[test]
    fn model_and_provider_namespaces_are_distinct() {
        with_temp_home(|| {
            set("model/openai", "mk").unwrap();
            set("provider/openai", "pk").unwrap();
            assert_eq!(get("model/openai").unwrap().unwrap(), "mk");
            assert_eq!(get("provider/openai").unwrap().unwrap(), "pk");
            remove("model/openai").unwrap();
            assert_eq!(
                get("provider/openai").unwrap().unwrap(),
                "pk",
                "removing a model key must not touch the same-named provider key"
            );
        });
    }

    #[test]
    fn unicode_account_roundtrips() {
        with_temp_home(|| {
            let account = "model/密钥-测试";
            set(account, "sk-u").unwrap();
            assert_eq!(resolve(&format!("!file:{account}")).unwrap(), "sk-u");
            assert_eq!(get(account).unwrap().unwrap(), "sk-u");
        });
    }

    #[test]
    fn home_falls_back_to_dot_when_unset() {
        with_temp_home(|| {
            // Remove HOME under the lock: `home()` falls back to `.`.
            let old = std::env::var("HOME").ok();
            std::env::remove_var("HOME");
            let h = home();
            assert_eq!(h, PathBuf::from("."));
            assert_eq!(
                secrets_path(),
                PathBuf::from(".").join(".starriver").join("secrets.json")
            );
            if let Some(o) = old {
                std::env::set_var("HOME", o);
            }
        });
    }
}
