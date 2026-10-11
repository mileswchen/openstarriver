use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A registry entry for an agent or skill
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryEntry {
    pub name: String,
    pub description: String,
    pub url: String,
    #[serde(rename = "type")]
    pub entry_type: String, // "agent" | "skill"
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub version: String,
}

/// Local market registry — persisted to ~/.starriver/registry.json
pub struct MarketRegistry {
    entries: Vec<RegistryEntry>,
    path: PathBuf,
}

impl MarketRegistry {
    pub fn load() -> anyhow::Result<Self> {
        // Windows has no HOME by default — USERPROFILE is the convention;
        // falling back to "." would scatter registry.json into the CWD.
        let home = std::env::var("HOME")
            .ok()
            .or_else(|| std::env::var("USERPROFILE").ok())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let path = home.join(".starriver").join("registry.json");
        let entries: Vec<RegistryEntry> = if path.exists() {
            let content = std::fs::read_to_string(&path)?;
            match serde_json::from_str(&content) {
                Ok(entries) => entries,
                Err(error) => {
                    // A corrupt registry must not masquerade as empty — the
                    // next save() would overwrite the file and lose whatever
                    // a manual repair could have recovered. Keep a backup.
                    let backup = path.with_extension("json.corrupt");
                    let _ = std::fs::copy(&path, &backup);
                    tracing::warn!(path = %path.display(), %error, backup = %backup.display(), "registry.json corrupt; starting from seed (backup kept)");
                    Self::seed_entries()
                }
            }
        } else {
            Self::seed_entries()
        };
        Ok(Self { entries, path })
    }

    fn seed_entries() -> Vec<RegistryEntry> {
        vec![
            RegistryEntry {
                name: "编码助手".into(),
                description: "Built-in coding agent".into(),
                url: "builtin".into(),
                entry_type: "agent".into(),
                category: "coding".into(),
                version: "1.0.0".into(),
            },
            RegistryEntry {
                name: "code-review".into(),
                description: "Review code in 5 dimensions".into(),
                url: "builtin".into(),
                entry_type: "skill".into(),
                category: "coding".into(),
                version: "1.2.0".into(),
            },
        ]
    }

    pub fn list(&self) -> &[RegistryEntry] {
        &self.entries
    }

    pub fn search(&self, query: &str) -> Vec<&RegistryEntry> {
        let q = query.to_lowercase();
        self.entries
            .iter()
            .filter(|e| {
                e.name.to_lowercase().contains(&q) || e.description.to_lowercase().contains(&q)
            })
            .collect()
    }

    pub fn filter_by_type(&self, entry_type: &str) -> Vec<&RegistryEntry> {
        self.entries
            .iter()
            .filter(|e| e.entry_type == entry_type)
            .collect()
    }

    pub fn publish(
        &mut self,
        name: &str,
        description: &str,
        url: &str,
        entry_type: &str,
        category: &str,
    ) -> anyhow::Result<()> {
        if self.entries.iter().any(|e| e.name == name) {
            anyhow::bail!("entry '{name}' already exists");
        }
        self.entries.push(RegistryEntry {
            name: name.into(),
            description: description.into(),
            url: url.into(),
            entry_type: entry_type.into(),
            category: category.into(),
            version: "0.1.0".into(),
        });
        self.save()?;
        Ok(())
    }

    pub fn remove(&mut self, name: &str) -> anyhow::Result<()> {
        let len_before = self.entries.len();
        self.entries.retain(|e| e.name != name);
        if self.entries.len() == len_before {
            anyhow::bail!("entry '{name}' not found");
        }
        self.save()?;
        Ok(())
    }

    fn save(&self) -> anyhow::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&self.path, serde_json::to_string_pretty(&self.entries)?)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_registry_load_seeded() {
        crate::testutil::with_temp_home(|_dir| {
            let reg = MarketRegistry::load().unwrap();
            assert!(reg.list().len() >= 2);
        });
    }

    #[test]
    fn corrupt_registry_keeps_backup_and_seeds() {
        // Round-10 regression: a corrupt registry.json must not masquerade as
        // empty (the next save would destroy the file).
        crate::testutil::with_temp_home(|home| {
            let dir = home.join(".starriver");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("registry.json"), "{not json").unwrap();
            let reg = MarketRegistry::load().unwrap();
            assert!(!reg.list().is_empty(), "seeded entries");
            assert!(dir.join("registry.json.corrupt").exists(), "backup kept");
        });
    }

    #[test]
    fn test_registry_search() {
        crate::testutil::with_temp_home(|_dir| {
            let reg = MarketRegistry::load().unwrap();
            let results = reg.search("code");
            assert!(!results.is_empty());
        });
    }

    #[test]
    fn test_registry_filter_by_type() {
        crate::testutil::with_temp_home(|_dir| {
            let reg = MarketRegistry::load().unwrap();
            let agents = reg.filter_by_type("agent");
            let skills = reg.filter_by_type("skill");
            assert!(!agents.is_empty());
            assert!(!skills.is_empty());
        });
    }

    #[test]
    fn test_registry_publish_and_remove() {
        crate::testutil::with_temp_home(|_dir| {
            let mut reg = MarketRegistry::load().unwrap();
            reg.publish(
                "test-agent",
                "A test agent",
                "https://git.example.com/test-agent",
                "agent",
                "testing",
            )
            .unwrap();
            assert!(reg.search("test-agent").len() == 1);
            reg.remove("test-agent").unwrap();
            assert!(reg.search("test-agent").is_empty());
        });
    }
}

// === Remote sync ===

impl MarketRegistry {
    /// Merge entries from a remote JSON string.
    /// New entries from remote are added; existing local entries are preserved.
    /// A payload that is not valid JSON is an error (BUG-28) — a truncated
    /// body or a proxy error page must not masquerade as "0 new entries".
    pub fn merge_json(&mut self, json: &str) -> anyhow::Result<usize> {
        let remote: Vec<RegistryEntry> = serde_json::from_str(json)?;
        let mut added = 0usize;
        for entry in remote {
            if !self.entries.iter().any(|e| e.name == entry.name) {
                self.entries.push(entry);
                added += 1;
            }
        }
        if added > 0 {
            self.save()?;
        }
        Ok(added)
    }
}

#[cfg(test)]
mod sync_tests {
    use super::*;

    #[test]
    fn test_merge_json_adds_new_entries() {
        crate::testutil::with_temp_home(|_dir| {
            let mut reg = MarketRegistry::load().unwrap();
            let before = reg.list().len();
            let remote = r#"[{"name":"remote-agent","description":"Remote","url":"https://git.example.com/remote-agent","type":"agent","category":"coding","version":"1.0.0"}]"#;
            let added = reg.merge_json(remote).unwrap();
            assert_eq!(added, 1);
            assert_eq!(reg.list().len(), before + 1);
        });
    }

    /// BUG-28 regression: `merge_json` returns `anyhow::Result` — a corrupt
    /// or truncated remote payload (proxy error page, half-read body) must
    /// surface as `Err`, not silently masquerade as "0 new entries" the way
    /// `unwrap_or_default` made it.
    #[test]
    fn merge_json_rejects_a_corrupt_remote_payload() {
        crate::testutil::with_temp_home(|_dir| {
            let mut reg = MarketRegistry::load().unwrap();
            assert!(
                reg.merge_json("<html>proxy error page</html>").is_err(),
                "corrupt remote JSON must error, not report 0 additions"
            );
            assert!(
                reg.merge_json(r#"[{"name": "half-written","#).is_err(),
                "truncated JSON must error"
            );
            // An empty array is a valid "nothing new", not an error.
            assert_eq!(reg.merge_json("[]").unwrap(), 0);
        });
    }
}
