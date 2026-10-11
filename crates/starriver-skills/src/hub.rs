use crate::profile::agent::AgentProfile;
use crate::profile::parser::parse_document;
use anyhow::Result;
use dashmap::DashMap;
#[allow(unused_imports)]
use std::path::Path;
use std::path::PathBuf;

/// Default agent name (built-in)
pub const DEFAULT_AGENT_NAME: &str = "Coding Assistant";

/// Agent info (for external queries)
#[derive(Debug, Clone)]
pub struct AgentInfo {
    pub name: String,
    pub description: String,
    pub version: String,
    pub path: PathBuf,
}

/// Agent Hub — multi-agent discovery, registry, query
pub struct AgentHub {
    agents: DashMap<String, AgentProfile>,
    /// Path of each agent's directory, kept in sync with `agents` on every scan.
    /// `list()` returns it so callers can write back to AGENT.md on disk.
    paths: DashMap<String, PathBuf>,
    dirs: Vec<PathBuf>,
}

impl AgentHub {
    /// Create an AgentHub with default scan directories
    pub fn with_defaults() -> Self {
        let mut dirs = Vec::new();
        if let Ok(cwd) = std::env::current_dir() {
            dirs.push(cwd.join(".starriver").join("agents"));
        }
        if let Some(home) = dirs_home() {
            dirs.push(home.join(".starriver").join("agents"));
        }
        Self {
            agents: DashMap::new(),
            paths: DashMap::new(),
            dirs,
        }
    }

    pub fn new(dirs: Vec<PathBuf>) -> Self {
        Self {
            agents: DashMap::new(),
            paths: DashMap::new(),
            dirs,
        }
    }

    /// Scan all directories to discover and register agents.
    /// Name conflicts: the version from the earlier directory takes priority.
    pub fn scan(&self) -> Result<Vec<AgentInfo>> {
        let mut discovered = Vec::new();
        for scan_dir in &self.dirs {
            if !scan_dir.exists() {
                continue;
            }
            let entries = match std::fs::read_dir(scan_dir) {
                Ok(e) => e,
                Err(error) => {
                    // WI-164: an unreadable scan directory used to vanish
                    // silently — agents/skills from it just never appeared.
                    tracing::warn!(
                        dir = %scan_dir.display(),
                        error = %error,
                        "skills scan directory unreadable; its agents are skipped"
                    );
                    continue;
                }
            };
            for entry in entries.flatten() {
                let agent_dir = entry.path();
                if !agent_dir.is_dir() {
                    continue;
                }
                let agent_md = agent_dir.join("AGENT.md");
                if !agent_md.exists() {
                    continue;
                }
                // §2.5 sanity cap: a profile over 32 MiB is a misconfiguration
                // (an exported dump, not a persona) — reject it at load.
                const MAX_AGENT_MD_BYTES: u64 = 32 << 20;
                if let Ok(meta) = std::fs::metadata(&agent_md) {
                    if meta.len() > MAX_AGENT_MD_BYTES {
                        tracing::warn!(
                            path = %agent_md.display(),
                            size = meta.len(),
                            cap = MAX_AGENT_MD_BYTES,
                            "AGENT.md exceeds the 32 MiB cap; profile rejected"
                        );
                        continue;
                    }
                }
                let content = match std::fs::read_to_string(&agent_md) {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!(path = %agent_md.display(), error = %e, "Failed to read AGENT.md");
                        continue;
                    }
                };
                let doc = match parse_document(&content) {
                    Ok(d) => d,
                    Err(e) => {
                        tracing::warn!(path = %agent_md.display(), error = %e, "AGENT.md parse failed");
                        continue;
                    }
                };
                let profile = match AgentProfile::try_from(doc) {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!(path = %agent_md.display(), error = %e, "AgentProfile validation failed");
                        continue;
                    }
                };
                let name = profile.name.clone();
                // BUG-16: the earlier directory wins a name conflict (the
                // doc contract; project-local shadows ~/.starriver). A
                // re-read of the SAME directory (get() rescans on every call)
                // still refreshes the entry so on-disk edits are picked up.
                let same_source = self
                    .paths
                    .get(&name)
                    .is_some_and(|registered| *registered == agent_dir);
                if !self.agents.contains_key(&name) || same_source {
                    let is_new = !self.agents.contains_key(&name);
                    self.agents.insert(name.clone(), profile);
                    self.paths.insert(name.clone(), agent_dir.clone());
                    if is_new {
                        discovered.push(AgentInfo {
                            name,
                            description: "".to_string(),
                            version: "0.1.0".to_string(),
                            path: agent_dir,
                        });
                    }
                }
            }
        }
        Ok(discovered)
    }

    pub fn list(&self) -> Vec<AgentInfo> {
        self.agents
            .iter()
            .map(|entry| AgentInfo {
                name: entry.name.clone(),
                description: entry.description.clone().unwrap_or_default(),
                version: entry.version.clone().unwrap_or_else(|| "0.1.0".into()),
                path: self
                    .paths
                    .get(entry.key())
                    .map(|p| p.clone())
                    .unwrap_or_default(),
            })
            .collect()
    }

    pub fn get(&self, name: &str) -> Option<AgentProfile> {
        // Always re-read from disk to pick up AGENT.md changes immediately
        let _ = self.scan();
        self.agents.get(name).map(|entry| entry.value().clone())
    }

    pub fn names(&self) -> Vec<String> {
        self.agents.iter().map(|entry| entry.name.clone()).collect()
    }

    pub fn len(&self) -> usize {
        self.agents.len()
    }

    pub fn is_empty(&self) -> bool {
        self.agents.is_empty()
    }
}

fn dirs_home() -> Option<PathBuf> {
    // Windows processes have no HOME — USERPROFILE is the convention (same
    // fallback the mcp-bridge gained in round 9).
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Scoped tracing capture for this binary: `with_default` installs a
    /// thread-local subscriber for the closure's duration, so asserts work
    /// even though another test may hold the global subscriber.
    fn capture_tracing<F: FnOnce()>(f: F) -> String {
        struct SharedBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for SharedBuf {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let sink: std::sync::Arc<std::sync::Mutex<Vec<u8>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let writer = std::sync::Arc::clone(&sink);
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || SharedBuf(writer.clone()))
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        let bytes = sink.lock().unwrap().clone();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn create_temp_agent(dir: &Path, name: &str, _extra: &str) -> PathBuf {
        let agent_dir = dir.join(name);
        fs::create_dir_all(&agent_dir).unwrap();
        let content = format!(
            "---\nname: {}\nversion: 1.0.0\ndescription: Test agent {}\n---\n\nThis is {}.\n{}",
            name, name, name, _extra
        );
        fs::write(agent_dir.join("AGENT.md"), content).unwrap();
        agent_dir
    }

    #[test]
    fn test_scan_finds_agents() {
        let tmp = tempfile::tempdir().unwrap();
        create_temp_agent(tmp.path(), "agent-a", "");
        create_temp_agent(tmp.path(), "agent-b", "");
        let hub = AgentHub::new(vec![tmp.path().to_path_buf()]);
        let result = hub.scan().unwrap();
        assert_eq!(result.len(), 2);
    }

    /// WI-164: an unreadable scan directory used to vanish silently — the
    /// warn (and skip) must now leave a trace. A regular FILE as the scan
    /// path makes `read_dir` fail deterministically without chmod games.
    #[test]
    fn unreadable_scan_directory_warns_and_skips() {
        let tmp = tempfile::tempdir().unwrap();
        let not_a_dir = tmp.path().join("not-a-dir");
        fs::write(&not_a_dir, "file where a directory was expected").unwrap();
        let hub = AgentHub::new(vec![not_a_dir.clone()]);
        let log = capture_tracing(|| {
            // scan never fails as a whole — the bad directory is skipped.
            let result = hub.scan().unwrap();
            assert!(result.is_empty());
        });
        assert!(
            log.contains("skills scan directory unreadable"),
            "expected the unreadable-directory warn, got: {log}"
        );
        assert!(
            log.contains(not_a_dir.to_str().unwrap()),
            "warn should name the directory: {log}"
        );
    }

    #[test]
    fn test_scan_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let hub = AgentHub::new(vec![tmp.path().to_path_buf()]);
        let result = hub.scan().unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_get_agent_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        create_temp_agent(tmp.path(), "my-agent", "");
        let hub = AgentHub::new(vec![tmp.path().to_path_buf()]);
        hub.scan().unwrap();
        let profile = hub.get("my-agent");
        assert!(profile.is_some());
        assert_eq!(profile.unwrap().name, "my-agent");
    }

    #[test]
    fn test_get_nonexistent_agent() {
        let hub = AgentHub::new(vec![]);
        assert!(hub.get("nonexistent").is_none());
    }

    #[test]
    fn test_list_returns_real_agent_paths() {
        let tmp = tempfile::tempdir().unwrap();
        create_temp_agent(tmp.path(), "agent-a", "");
        create_temp_agent(tmp.path(), "agent-b", "");
        let hub = AgentHub::new(vec![tmp.path().to_path_buf()]);
        hub.scan().unwrap();

        let mut by_name = std::collections::HashMap::new();
        for info in hub.list() {
            by_name.insert(info.name.clone(), info.path);
        }
        for name in ["agent-a", "agent-b"] {
            let p = by_name.get(name).expect("agent must be listed");
            assert_eq!(
                p,
                &tmp.path().join(name),
                "list() must return each agent's on-disk dir so callers can persist back"
            );
            assert!(
                p.join("AGENT.md").exists(),
                "persist path must point at real AGENT.md"
            );
        }
    }

    #[test]
    fn test_first_dir_priority() {
        let tmp1 = tempfile::tempdir().unwrap();
        let tmp2 = tempfile::tempdir().unwrap();
        create_temp_agent(tmp1.path(), "shared", "v1");
        create_temp_agent(tmp2.path(), "shared", "v2");
        let hub = AgentHub::new(vec![tmp1.path().to_path_buf(), tmp2.path().to_path_buf()]);
        hub.scan().unwrap();
        let profile = hub.get("shared").unwrap();
        assert_eq!(profile.version.as_deref(), Some("1.0.0"));
    }

    /// BUG-16 regression: `create_temp_agent` hardcodes `version: 1.0.0`, so
    /// `test_first_dir_priority` above cannot distinguish which directory won.
    /// With distinct versions the doc contract ("the version from the earlier
    /// directory takes priority") is actually observable: the project-local
    /// directory is scanned first and must shadow the `~/.starriver` copy.
    fn create_versioned_agent(
        dir: &Path,
        name: &str,
        version: &str,
        description: &str,
    ) -> PathBuf {
        let agent_dir = dir.join(name);
        fs::create_dir_all(&agent_dir).unwrap();
        let content = format!(
            "---\nname: {name}\nversion: {version}\ndescription: {description}\n---\n\nBody of {name} ({description}).\n"
        );
        fs::write(agent_dir.join("AGENT.md"), content).unwrap();
        agent_dir
    }

    #[test]
    fn earlier_directory_wins_name_conflict() {
        let tmp1 = tempfile::tempdir().unwrap();
        let tmp2 = tempfile::tempdir().unwrap();
        create_versioned_agent(tmp1.path(), "shared", "1.0.0", "project-local");
        create_versioned_agent(tmp2.path(), "shared", "2.0.0", "user-global");
        let hub = AgentHub::new(vec![tmp1.path().to_path_buf(), tmp2.path().to_path_buf()]);
        hub.scan().unwrap();
        // `get` re-scans, so the registry state after any number of scans
        // must still carry the earlier directory's version.
        let profile = hub.get("shared").unwrap();
        assert_eq!(
            profile.version.as_deref(),
            Some("1.0.0"),
            "the earlier directory must win the name conflict"
        );
        assert_eq!(
            profile.description.as_deref(),
            Some("project-local"),
            "the winning profile is the earlier directory's, not a later overwrite"
        );
        let listed = hub.list().into_iter().find(|a| a.name == "shared").unwrap();
        assert_eq!(
            listed.path,
            tmp1.path().join("shared"),
            "list() exposes the earlier directory's path for the winning entry"
        );
    }

    #[test]
    fn test_ignore_missing_agent_md() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("no-agent")).unwrap();
        let hub = AgentHub::new(vec![tmp.path().to_path_buf()]);
        let result = hub.scan().unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_names_method() {
        let tmp = tempfile::tempdir().unwrap();
        create_temp_agent(tmp.path(), "a1", "");
        let hub = AgentHub::new(vec![tmp.path().to_path_buf()]);
        hub.scan().unwrap();
        assert_eq!(hub.names(), vec!["a1"]);
    }

    #[test]
    fn test_len_is_empty() {
        let hub = AgentHub::new(vec![]);
        assert!(hub.is_empty());
        assert_eq!(hub.len(), 0);
    }

    #[test]
    fn test_invalid_agent_md_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("bad-agent");
        fs::create_dir_all(&agent_dir).unwrap();
        fs::write(agent_dir.join("AGENT.md"), "no frontmatter here").unwrap();
        let hub = AgentHub::new(vec![tmp.path().to_path_buf()]);
        let result = hub.scan().unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_oversized_agent_md_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("huge-agent");
        fs::create_dir_all(&agent_dir).unwrap();
        // 32 MiB cap + 1 byte, otherwise well-formed frontmatter.
        fs::write(
            agent_dir.join("AGENT.md"),
            format!(
                "---\nname: huge-agent\n---\n\n{}",
                "x".repeat((32 << 20) + 1)
            ),
        )
        .unwrap();
        let hub = AgentHub::new(vec![tmp.path().to_path_buf()]);
        let result = hub.scan().unwrap();
        assert!(
            result.is_empty(),
            "profile over the 32 MiB cap must be rejected"
        );
        assert!(hub.get("huge-agent").is_none());
    }

    #[test]
    fn test_get_rereads_from_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("test-agent");
        fs::create_dir_all(&agent_dir).unwrap();
        let md1 = "---\nname: test-agent\n---\n\nVersion 1 body\n";
        fs::write(agent_dir.join("AGENT.md"), md1).unwrap();

        let hub = AgentHub::new(vec![tmp.path().to_path_buf()]);
        hub.scan().unwrap();

        let agent1 = hub.get("test-agent").unwrap();
        assert_eq!(agent1.body.trim(), "Version 1 body");

        // Update the file on disk (same agent name, different body)
        let md2 = "---\nname: test-agent\n---\n\nVersion 2 body — updated\n";
        fs::write(agent_dir.join("AGENT.md"), md2).unwrap();

        // hub.get() should auto-scan and return the updated content
        let agent2 = hub.get("test-agent").unwrap();
        assert_eq!(agent2.body.trim(), "Version 2 body — updated");

        tmp.close().ok();
    }

    #[test]
    fn test_scan_empty_directory() {
        let tmp = tempfile::tempdir().unwrap();
        // Empty dir with no subdirectories — scan should return empty
        let hub = AgentHub::new(vec![tmp.path().to_path_buf()]);
        let result = hub.scan().unwrap();
        assert!(result.is_empty());
        tmp.close().ok();
    }

    #[test]
    fn test_scan_directory_with_no_agent_md() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("not-an-agent");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("README.md"), "hello").unwrap();
        let hub = AgentHub::new(vec![tmp.path().to_path_buf()]);
        let result = hub.scan().unwrap();
        assert!(
            result.is_empty(),
            "directory without AGENT.md should be ignored"
        );
        tmp.close().ok();
    }

    #[test]
    fn test_multiple_agents_in_same_scan_dir() {
        let tmp = tempfile::tempdir().unwrap();
        for name in &["agent-a", "agent-b", "agent-c"] {
            let dir = tmp.path().join(name);
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join("AGENT.md"),
                format!("---\nname: {name}\n---\n\n{name} body\n"),
            )
            .unwrap();
        }
        let hub = AgentHub::new(vec![tmp.path().to_path_buf()]);
        let result = hub.scan().unwrap();
        assert_eq!(result.len(), 3);
        assert!(hub.get("agent-a").is_some());
        assert!(hub.get("agent-b").is_some());
        assert!(hub.get("agent-c").is_some());
        tmp.close().ok();
    }

    #[test]
    fn test_scan_with_invalid_yaml_skips_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("bad-agent");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("AGENT.md"), "---\nname: [bad yaml\n---\n\nbody\n").unwrap();
        let hub = AgentHub::new(vec![tmp.path().to_path_buf()]);
        let result = hub.scan().unwrap();
        assert!(
            result.is_empty(),
            "invalid YAML should skip agent, not crash"
        );
        tmp.close().ok();
    }

    #[test]
    fn test_scan_multiple_directories_merges_results() {
        let dir1 = tempfile::tempdir().unwrap();
        let dir2 = tempfile::tempdir().unwrap();
        for name in &["alpha"] {
            let d = dir1.path().join(name);
            fs::create_dir_all(&d).unwrap();
            fs::write(
                d.join("AGENT.md"),
                format!("---\nname: {name}\n---\n\n{name}\n"),
            )
            .unwrap();
        }
        for name in &["beta"] {
            let d = dir2.path().join(name);
            fs::create_dir_all(&d).unwrap();
            fs::write(
                d.join("AGENT.md"),
                format!("---\nname: {name}\n---\n\n{name}\n"),
            )
            .unwrap();
        }
        let hub = AgentHub::new(vec![dir1.path().to_path_buf(), dir2.path().to_path_buf()]);
        let result = hub.scan().unwrap();
        assert_eq!(result.len(), 2);
        assert!(hub.get("alpha").is_some());
        assert!(hub.get("beta").is_some());
        dir1.close().ok();
        dir2.close().ok();
    }
}

// === Install / Uninstall ===

impl AgentHub {
    /// Install an agent from a source path (copies AGENT.md directory)
    pub fn install(&self, source: &Path, name: &str) -> anyhow::Result<AgentInfo> {
        let home = std::env::var("HOME")
            .ok()
            .or_else(|| std::env::var("USERPROFILE").ok())
            .map(PathBuf::from)
            .ok_or_else(|| anyhow::anyhow!("HOME not set"))?;
        let agent_dir = home.join(".starriver").join("agents").join(name);
        std::fs::create_dir_all(&agent_dir)?;

        let source_md = source.join("AGENT.md");
        anyhow::ensure!(source_md.exists(), "Source directory missing AGENT.md");
        std::fs::copy(&source_md, agent_dir.join("AGENT.md"))?;

        // Copy skills/ directory if present
        let source_skills = source.join("skills");
        if source_skills.exists() && source_skills.is_dir() {
            let dest_skills = agent_dir.join("skills");
            std::fs::create_dir_all(&dest_skills)?;
            for entry in std::fs::read_dir(&source_skills)? {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    let dest = dest_skills.join(entry.file_name());
                    if dest.exists() {
                        std::fs::remove_dir_all(&dest)?;
                    }
                    copy_dir(&entry.path(), &dest)?;
                }
            }
        }

        self.scan()?;
        Ok(AgentInfo {
            name: name.to_string(),
            description: "installed".into(),
            version: "0.1.0".into(),
            path: agent_dir,
        })
    }

    /// Uninstall an agent by name
    pub fn uninstall(&self, name: &str) -> anyhow::Result<()> {
        // Remove the dir the agent is ACTUALLY registered from: a
        // project-local registration has no home copy, and the always-re-read
        // `get()` would resurrect it right after a home-only removal.
        if let Some(dir) = self.paths.get(name) {
            std::fs::remove_dir_all(dir.value())?;
        } else {
            let home = std::env::var("HOME")
                .ok()
                .or_else(|| std::env::var("USERPROFILE").ok())
                .map(PathBuf::from)
                .ok_or_else(|| anyhow::anyhow!("HOME not set"))?;
            let agent_dir = home.join(".starriver").join("agents").join(name);
            if agent_dir.exists() {
                std::fs::remove_dir_all(&agent_dir)?;
            }
        }
        self.agents.remove(name);
        self.paths.remove(name);
        Ok(())
    }
}

fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let dest = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &dest)?;
        } else {
            std::fs::copy(entry.path(), &dest)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod sp25_boundary_tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_agent_md_exactly_at_cap_accepted() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("edge-agent");
        fs::create_dir_all(&agent_dir).unwrap();
        let base = "---\nname: edge-agent\n---\n\n".to_string();
        let body = "x".repeat((32 << 20) - base.len());
        fs::write(agent_dir.join("AGENT.md"), format!("{base}{body}")).unwrap();

        let hub = AgentHub::new(vec![tmp.path().to_path_buf()]);
        hub.scan().unwrap();
        assert!(
            hub.get("edge-agent").is_some(),
            "exactly at the cap is valid; only strictly-greater is rejected"
        );
    }
}
