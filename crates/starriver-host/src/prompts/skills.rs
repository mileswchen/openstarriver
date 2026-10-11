use dashmap::DashMap;
use starriver_skills::profile::parser::parse_document;
use starriver_skills::profile::skill::SkillProfile;
use std::path::{Path, PathBuf};

pub struct SkillManager {
    skills: DashMap<String, SkillProfile>,
}

impl SkillManager {
    pub fn new() -> Self {
        Self {
            skills: DashMap::new(),
        }
    }

    /// Scan default skill directories (~/.starriver/skills/ and .starriver/skills/)
    pub fn scan_defaults(&self, cwd: &Path) -> anyhow::Result<Vec<String>> {
        let home = std::env::var("HOME").ok().map(PathBuf::from);
        let mut names = Vec::new();
        for dir in &[
            Some(cwd.join(".starriver").join("skills")),
            home.as_ref().map(|h| h.join(".starriver").join("skills")),
        ] {
            let Some(dir) = dir else { continue };
            if !dir.exists() {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let skill_dir = entry.path();
                if !skill_dir.is_dir() {
                    continue;
                }
                let skill_md = skill_dir.join("SKILL.md");
                let Ok(content) = std::fs::read_to_string(&skill_md) else {
                    continue;
                };
                let Ok(doc) = parse_document(&content) else {
                    continue;
                };
                let Ok(profile) = SkillProfile::try_from(doc) else {
                    continue;
                };
                let name = profile.name.clone();
                self.skills.insert(name.clone(), profile);
                names.push(name);
            }
        }
        Ok(names)
    }

    pub fn get(&self, name: &str) -> Option<SkillProfile> {
        self.skills.get(name).map(|e| e.value().clone())
    }

    pub fn names(&self) -> Vec<String> {
        self.skills.iter().map(|e| e.name.clone()).collect()
    }

    /// Build enhanced system prompt with skill descriptions and always-on bodies
    pub fn inject_to_system_prompt(&self, base: &str) -> String {
        let mut prompt = base.to_string();

        // Always-on skills: full body injection
        let always_on: Vec<_> = self.skills.iter().filter(|e| e.always_on).collect();
        if !always_on.is_empty() {
            prompt.push_str("\n\n## Always-Active Skills\n\n");
            for entry in &always_on {
                prompt.push_str(&format!("### {}\n\n{}\n\n---\n\n", entry.name, entry.body));
            }
        }

        // Available skills: descriptions for LLM intent matching
        let available: Vec<_> = self
            .skills
            .iter()
            .filter(|e| !e.disable_model_invocation)
            .collect();
        if !available.is_empty() {
            prompt.push_str("\n## Available Skills\n\n");
            prompt.push_str("Activate a skill by name. Its full instructions will be loaded.\n\n");
            for entry in &available {
                let desc = entry.description.as_deref().unwrap_or_default();
                prompt.push_str(&format!("- **{}**: {}\n", entry.name, desc));
            }
        }

        prompt
    }

    pub fn len(&self) -> usize {
        self.skills.len()
    }
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }
}

impl Default for SkillManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn create_skill_dir(parent: &Path, skill_name: &str, frontmatter: &str, body: &str) {
        let skill_dir = parent.join(".starriver").join("skills").join(skill_name);
        fs::create_dir_all(&skill_dir).unwrap();
        let content = format!("---\n{}\n---\n{}", frontmatter.trim(), body);
        fs::write(skill_dir.join("SKILL.md"), content).unwrap();
    }

    #[test]
    fn test_skill_manager_empty() {
        let mgr = SkillManager::new();
        assert_eq!(mgr.len(), 0);
        assert!(mgr.is_empty());
        assert!(mgr.names().is_empty());
    }

    #[test]
    fn test_skill_manager_scan_finds_skills() {
        let tmp = tempfile::tempdir().unwrap();
        create_skill_dir(
            tmp.path(),
            "test-skill",
            "name: test-skill",
            "Body content.",
        );
        let mgr = SkillManager::new();
        let names = mgr.scan_defaults(tmp.path()).unwrap();
        // scan_defaults also reads the real ~/.starriver/skills, so assert the temp skill
        // is present (not that it's the only one).
        assert!(names.iter().any(|n| n == "test-skill"));
        assert!(mgr.get("test-skill").is_some());
    }

    #[test]
    fn test_skill_manager_get_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        create_skill_dir(
            tmp.path(),
            "my-skill",
            "name: my-skill\ndescription: Does things",
            "Skill body text.",
        );
        let mgr = SkillManager::new();
        mgr.scan_defaults(tmp.path()).unwrap();
        let profile = mgr.get("my-skill");
        assert!(profile.is_some());
        let p = profile.unwrap();
        assert_eq!(p.name, "my-skill");
        assert_eq!(p.description.as_deref(), Some("Does things"));
        assert_eq!(p.body, "Skill body text.");
    }

    #[test]
    fn test_skill_manager_get_nonexistent() {
        let mgr = SkillManager::new();
        assert!(mgr.get("nonexistent").is_none());
    }

    #[test]
    fn test_skill_manager_inject_always_on() {
        let tmp = tempfile::tempdir().unwrap();
        let fm = "name: always-skill\nalways-on: true";
        create_skill_dir(tmp.path(), "always-skill", fm, "Always-on body.");
        let mgr = SkillManager::new();
        mgr.scan_defaults(tmp.path()).unwrap();

        let prompt = mgr.inject_to_system_prompt("Base prompt.");
        assert!(prompt.contains("Base prompt."));
        assert!(prompt.contains("## Always-Active Skills"));
        assert!(prompt.contains("### always-skill"));
        assert!(prompt.contains("Always-on body."));
    }

    #[test]
    fn test_skill_manager_inject_descriptions() {
        // Isolate from user's ~/.starriver/skills so global always-on skills
        // don't leak into the test and trigger unexpected headings.
        crate::testutil::with_temp_home(|dir| {
            let fm = "name: available-skill\ndescription: Helps with stuff";
            create_skill_dir(dir, "available-skill", fm, "Some body.");

            let mgr = SkillManager::new();
            mgr.scan_defaults(dir).unwrap();

            let prompt = mgr.inject_to_system_prompt("Base prompt.");
            assert!(prompt.contains("Base prompt."));
            assert!(prompt.contains("## Available Skills"));
            assert!(prompt.contains("**available-skill**"));
            assert!(prompt.contains("Helps with stuff"));
            assert!(!prompt.contains("## Always-Active Skills"));
        });
    }

    #[test]
    fn test_skill_manager_inject_both() {
        let tmp = tempfile::tempdir().unwrap();
        create_skill_dir(
            tmp.path(),
            "always-skill",
            "name: always-skill\nalways-on: true",
            "Always body.",
        );
        create_skill_dir(
            tmp.path(),
            "avail-skill",
            "name: avail-skill\ndescription: Avail desc",
            "Avail body.",
        );
        let mgr = SkillManager::new();
        mgr.scan_defaults(tmp.path()).unwrap();

        let prompt = mgr.inject_to_system_prompt("Base.");
        assert!(prompt.contains("## Always-Active Skills"));
        assert!(prompt.contains("### always-skill"));
        assert!(prompt.contains("Always body."));
        assert!(prompt.contains("## Available Skills"));
        assert!(prompt.contains("**avail-skill**"));
        assert!(prompt.contains("Avail desc"));
    }

    #[test]
    fn test_skill_manager_inject_skips_disabled_invocation() {
        let tmp = tempfile::tempdir().unwrap();
        let fm =
            "name: hidden-skill\ndescription: Should not appear\ndisable-model-invocation: true";
        create_skill_dir(tmp.path(), "hidden-skill", fm, "Hidden body.");
        let mgr = SkillManager::new();
        mgr.scan_defaults(tmp.path()).unwrap();

        let prompt = mgr.inject_to_system_prompt("Base.");
        assert!(prompt.contains("Base."));
        // The disabled skill must not appear in the available-skills list. (The section
        // itself may still be present if the environment has other enabled skills.)
        assert!(!prompt.contains("hidden-skill"));
        assert!(!prompt.contains("Should not appear"));
    }

    #[test]
    fn test_skill_manager_scan_invalid_skillmd() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp
            .path()
            .join(".starriver")
            .join("skills")
            .join("bad-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: bad-skill\n{{{ broken\n---\nBody.",
        )
        .unwrap();

        let mgr = SkillManager::new();
        let names = mgr.scan_defaults(tmp.path()).unwrap();
        // The invalid skill must be skipped (scan_defaults also reads the real
        // ~/.starriver/skills, so `names` may include environment skills — we only
        // assert the bad one is absent).
        assert!(!names.iter().any(|n| n == "bad-skill"));
        assert!(mgr.get("bad-skill").is_none());
    }

    #[test]
    fn test_skill_manager_scan_missing_name() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp
            .path()
            .join(".starriver")
            .join("skills")
            .join("no-name-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\ndescription: No name here\n---\nBody.",
        )
        .unwrap();

        let mgr = SkillManager::new();
        let names = mgr.scan_defaults(tmp.path()).unwrap();
        // The name-less skill is skipped; environment skills may still load.
        assert!(!names.iter().any(|n| n == "no-name-skill"));
        assert!(mgr.get("no-name-skill").is_none());
    }

    #[test]
    fn test_skill_manager_scan_multiple_skills() {
        let tmp = tempfile::tempdir().unwrap();
        create_skill_dir(tmp.path(), "skill-a", "name: skill-a", "A body.");
        create_skill_dir(tmp.path(), "skill-b", "name: skill-b", "B body.");
        create_skill_dir(tmp.path(), "skill-c", "name: skill-c", "C body.");
        let mgr = SkillManager::new();
        let names = mgr.scan_defaults(tmp.path()).unwrap();
        for n in ["skill-a", "skill-b", "skill-c"] {
            assert!(names.iter().any(|x| x == n), "missing {n}: {names:?}");
            assert!(mgr.get(n).is_some());
        }
    }

    #[test]
    fn test_skill_manager_scan_skips_subdirs_without_skillmd() {
        let tmp = tempfile::tempdir().unwrap();
        let empty_dir = tmp
            .path()
            .join(".starriver")
            .join("skills")
            .join("empty-dir");
        fs::create_dir_all(&empty_dir).unwrap();
        fs::write(empty_dir.join("README.md"), "just a readme").unwrap();
        create_skill_dir(tmp.path(), "valid", "name: valid", "Valid body.");

        let mgr = SkillManager::new();
        let names = mgr.scan_defaults(tmp.path()).unwrap();
        // empty-dir (no SKILL.md) is skipped; valid is loaded.
        assert!(names.iter().any(|n| n == "valid"));
        assert!(mgr.get("valid").is_some());
    }

    #[test]
    fn test_skill_manager_scan_skips_files_in_skills_root() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join(".starriver").join("skills");
        fs::create_dir_all(&skills_dir).unwrap();
        fs::write(
            skills_dir.join("orphan.md"),
            "---\nname: orphan\n---\nBody.",
        )
        .unwrap();

        let mgr = SkillManager::new();
        let names = mgr.scan_defaults(tmp.path()).unwrap();
        // orphan.md at the skills root (not a subdir) is skipped.
        assert!(!names.iter().any(|n| n == "orphan"));
        assert!(mgr.get("orphan").is_none());
    }
}
