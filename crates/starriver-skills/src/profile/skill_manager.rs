// SkillManager — loads, indexes, activates, and injects skills into agent system prompts.
//
// Four activation modes:
//   A. Always-on    — always_on: true -> injected into every system prompt
//   B. Slash command — command: /review -> bound to user-triggered slash commands
//   C. LLM intent   — descriptions exposed to LLM for progressive disclosure
//   D. Conditional  — file suffix matching -> auto-activate
//
// Loading: scan directories for SKILL.md files, parse frontmatter, index by activation mode.

use dashmap::DashMap;
use std::path::Path;

use super::parser::parse_document;
use super::skill::SkillProfile;

/// SkillManager — thread-safe skill registry with activation resolution
#[derive(Debug)]
pub struct SkillManager {
    skills: DashMap<String, SkillProfile>,
    always_on: DashMap<String, ()>,
    commands: DashMap<String, String>,
    suffix_map: DashMap<String, Vec<String>>,
    /// Keyword/regex trigger pattern -> (skill name, pattern is regex)
    keyword_triggers: DashMap<String, Vec<(String, bool)>>,
    /// Hook type -> skill names (e.g. preToolUse)
    hooks_map: DashMap<String, Vec<String>>,
}

impl SkillManager {
    pub fn new() -> Self {
        Self {
            skills: DashMap::new(),
            always_on: DashMap::new(),
            commands: DashMap::new(),
            suffix_map: DashMap::new(),
            keyword_triggers: DashMap::new(),
            hooks_map: DashMap::new(),
        }
    }

    // ── Loading ──

    pub fn load_skill(&self, content: &str) -> anyhow::Result<String> {
        let doc = parse_document(content)?;
        let profile = SkillProfile::try_from(doc)?;
        self.register(profile)
    }

    pub fn load_dir(&self, dir: &Path) -> anyhow::Result<Vec<String>> {
        let mut names = Vec::new();
        if !dir.exists() || !dir.is_dir() {
            return Ok(names);
        }
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let subdir = entry.path();
                if !subdir.is_dir() {
                    continue;
                }
                let skill_md = subdir.join("SKILL.md");
                if skill_md.exists() {
                    if let Ok(content) = std::fs::read_to_string(&skill_md) {
                        if let Ok(name) = self.load_skill(&content) {
                            names.push(name);
                        }
                    }
                }
            }
        }
        Ok(names)
    }

    fn register(&self, profile: SkillProfile) -> anyhow::Result<String> {
        let name = profile.name.clone();
        // BUG-18: the three index maps must reflect exactly the latest
        // configuration of this name — drop its previous entries first, or a
        // re-register (load_dir rescan, disk reload) duplicates resolutions
        // and stale suffixes/hooks from the old version linger.
        for mut entry in self.suffix_map.iter_mut() {
            entry.value_mut().retain(|n| n != &name);
        }
        for mut entry in self.hooks_map.iter_mut() {
            entry.value_mut().retain(|n| n != &name);
        }
        // BUG-18 follow-up: commands index must be cleared too — a re-registered
            // skill that changed/removed its command left the old entry live.
            let cmd_keys: Vec<String> = self
                .commands
                .iter()
                .filter(|entry| entry.value() == &name)
                .map(|entry| entry.key().clone())
                .collect();
            for k in cmd_keys {
                self.commands.remove(&k);
            }
            for mut entry in self.keyword_triggers.iter_mut() {
            entry
                .value_mut()
                .retain(|(n, _)| n != &name);
        }
        if profile.always_on {
            self.always_on.insert(name.clone(), ());
        } else {
            self.always_on.remove(&name);
        }
        if let Some(ref cmd) = profile.command {
            self.commands.insert(cmd.clone(), name.clone());
            for alias in &profile.command_aliases {
                self.commands.insert(alias.clone(), name.clone());
            }
        }
        if let Some(ref conditions) = profile.conditions {
            for suffix in &conditions.match_file_suffix {
                self.suffix_map
                    .entry(suffix.clone())
                    .or_default()
                    .push(name.clone());
            }
            if let Some(ref trigger) = conditions.trigger {
                let is_regex = trigger.trigger_type.eq_ignore_ascii_case("regex");
                for pattern in &trigger.patterns {
                    self.keyword_triggers
                        .entry(pattern.clone())
                        .or_default()
                        .push((name.clone(), is_regex));
                }
            }
            if let Some(ref hooks) = conditions.hooks {
                if !hooks.pre_tool_use.is_empty() {
                    self.hooks_map
                        .entry("preToolUse".into())
                        .or_default()
                        .push(name.clone());
                }
                if !hooks.post_tool_result.is_empty() {
                    self.hooks_map
                        .entry("postToolResult".into())
                        .or_default()
                        .push(name.clone());
                }
            }
        }
        self.skills.insert(name.clone(), profile);
        Ok(name)
    }

    // ── Lookup ──

    pub fn get(&self, name: &str) -> Option<SkillProfile> {
        self.skills.get(name).map(|r| r.value().clone())
    }

    pub fn list_all(&self) -> Vec<SkillProfile> {
        self.skills.iter().map(|r| r.value().clone()).collect()
    }

    pub fn len(&self) -> usize {
        self.skills.len()
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    // ── Activation modes ──

    pub fn always_on_skills(&self) -> Vec<SkillProfile> {
        // Sorted by name: DashMap shard order is process-random — the
        // injected system prompt must be deterministic run-to-run (prompt
        // cache hits, reproducibility).
        let mut out: Vec<SkillProfile> = self
            .always_on
            .iter()
            .filter_map(|r| self.get(r.key()))
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    pub fn resolve_by_command(&self, cmd: &str) -> Option<SkillProfile> {
        self.commands.get(cmd).and_then(|r| self.get(r.value()))
    }

    pub fn descriptions_for_llm(&self) -> Vec<SkillDescription> {
        // Same determinism rule as always_on_skills.
        let mut out: Vec<SkillDescription> = self
            .skills
            .iter()
            .filter(|r| !r.disable_model_invocation)
            .map(|r| {
                let val = r.value();
                SkillDescription {
                    name: val.name.clone(),
                    description: val.description.clone().unwrap_or_default(),
                    tools: val.tools.iter().map(|t| format!("{t:?}")).collect(),
                }
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    pub fn resolve_by_file_suffix(&self, suffix: &str) -> Vec<SkillProfile> {
        self.suffix_map
            .get(suffix)
            .map(|r| r.iter().filter_map(|name| self.get(name)).collect())
            .unwrap_or_default()
    }

    /// Mode D3: Resolve skills by keyword/regex trigger matching on user input.
    /// Checks trigger_type: "keyword" uses contains, "regex" uses regex match
    /// (case-insensitive; unparseable patterns are skipped with a warning).
    pub fn resolve_by_keyword(&self, input: &str) -> Vec<SkillProfile> {
        let mut results: Vec<SkillProfile> = Vec::new();
        let input_lower = input.to_lowercase();
        for entry in self.keyword_triggers.iter() {
            let pattern = entry.key();
            let skill_names = entry.value();
            // BUG-17: the trigger_type is honored per pattern — regex
            // patterns match by pattern semantics, keywords by substring.
            // BUG-17 (per-pattern semantics), extended per PAIR: one
            // pattern can back several skills with different trigger types —
            // only the pairs whose own semantics matched activate.
            for (name, is_regex) in skill_names {
                let matched = if *is_regex {
                    match regex_lite::RegexBuilder::new(pattern)
                        .case_insensitive(true)
                        .build()
                    {
                        Ok(re) => re.is_match(input),
                        Err(error) => {
                            tracing::warn!(
                                pattern = %pattern,
                                error = %error,
                                "skill trigger pattern is not a valid regex; skipped"
                            );
                            false
                        }
                    }
                } else {
                    input_lower.contains(&pattern.to_lowercase())
                };
                if matched {
                    if let Some(p) = self.get(name) {
                        if !results.iter().any(|r| r.name == p.name) {
                            results.push(p);
                        }
                    }
                }
            }
        }
        results
    }

    /// Mode E: Resolve skills by lifecycle hook type (e.g. "preToolUse", "postToolResult").
    pub fn resolve_by_hook(&self, hook_type: &str) -> Vec<SkillProfile> {
        self.hooks_map
            .get(hook_type)
            .map(|r| r.iter().filter_map(|name| self.get(name)).collect())
            .unwrap_or_default()
    }

    pub fn has_command_skills(&self) -> bool {
        !self.commands.is_empty()
    }
    pub fn has_conditional_skills(&self) -> bool {
        !self.suffix_map.is_empty()
            || !self.keyword_triggers.is_empty()
            || !self.hooks_map.is_empty()
    }

    // ── Prompt injection ──

    pub fn inject_to_system_prompt(
        &self,
        base_prompt: &str,
        active_skill_names: &[&str],
    ) -> String {
        let mut prompt = base_prompt.to_string();
        // Always-active skills (full body)
        let always_active = self.always_on_skills();
        if !always_active.is_empty() {
            prompt.push_str("\n\n## Always-Active Skills\n\n");
            for skill in &always_active {
                prompt.push_str(&format!("### {}\n\n{}\n\n---\n\n", skill.name, skill.body));
            }
        }
        // Explicitly activated skills
        let mut injected = std::collections::HashSet::new();
        for &name in active_skill_names {
            if injected.contains(name) {
                continue;
            }
            if let Some(skill) = self.get(name) {
                if skill.always_on {
                    continue;
                }
                prompt.push_str(&format!(
                    "\n\n## Skill: {}\n\n{}\n\n---\n\n",
                    skill.name, skill.body
                ));
                injected.insert(name);
            }
        }
        // Available skill descriptions (for LLM matching)
        let descriptions = self.descriptions_for_llm();
        if !descriptions.is_empty() {
            prompt.push_str("\n\n## Available Skills\n\n");
            for desc in &descriptions {
                let tools_str = if desc.tools.is_empty() {
                    String::from(" (declarative only)")
                } else {
                    format!(" (uses: {})", desc.tools.join(", "))
                };
                prompt.push_str(&format!(
                    "- **{}**: {}{}\n",
                    desc.name, desc.description, tools_str
                ));
            }
        }
        prompt
    }
}

impl Default for SkillManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Lightweight skill description for LLM progressive disclosure
#[derive(Debug, Clone)]
pub struct SkillDescription {
    pub name: String,
    pub description: String,
    pub tools: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const SKILL_CODE_REVIEW: &str = r#"---
name: code-review
description: Review code quality and security
command: /review
command-aliases: ["/cr"]
tools:
  - builtin:read
  - builtin:grep
---

## Review Process
1. Check correctness
2. Check security
3. Report findings
"#;

    const SKILL_RUST_STYLE: &str = r#"---
name: rust-style
description: Enforce Rust style guidelines
always-on: true
---

## Rust Style Rules
- Use snake_case for functions
- Use CamelCase for types
"#;

    const SKILL_RUST_TEST: &str = r#"---
name: rust-unit-test
description: Auto-generate unit tests for Rust files
conditions:
  match_file_suffix: [".rs"]
  requires-tools: ["file-read", "file-write"]
tools:
  - builtin:read
  - builtin:write
---

## Unit Test Generation
- Write tests in a #[cfg(test)] module
"#;

    const SKILL_SECRET: &str = r#"---
name: secret-internal
description: Internal tooling
disable-model-invocation: true
---

Secret content.
"#;

    #[test]
    fn skill_manager_loads_and_lists_skill() {
        let mgr = SkillManager::new();
        let name = mgr.load_skill(SKILL_CODE_REVIEW).unwrap();
        assert_eq!(name, "code-review");
        assert_eq!(mgr.len(), 1);
        assert!(mgr.get("code-review").is_some());
    }

    #[test]
    fn skill_manager_resolve_by_command() {
        let mgr = SkillManager::new();
        mgr.load_skill(SKILL_CODE_REVIEW).unwrap();
        let skill = mgr.resolve_by_command("/review");
        assert!(skill.is_some());
        assert_eq!(skill.unwrap().name, "code-review");
        let skill = mgr.resolve_by_command("/cr");
        assert!(skill.is_some());
        assert_eq!(skill.unwrap().name, "code-review");
        assert!(mgr.resolve_by_command("/unknown").is_none());
    }

    #[test]
    fn skill_manager_always_on() {
        let mgr = SkillManager::new();
        mgr.load_skill(SKILL_RUST_STYLE).unwrap();
        mgr.load_skill(SKILL_CODE_REVIEW).unwrap();
        let always = mgr.always_on_skills();
        assert_eq!(always.len(), 1);
        assert_eq!(always[0].name, "rust-style");
    }

    #[test]
    fn skill_manager_llm_descriptions() {
        let mgr = SkillManager::new();
        mgr.load_skill(SKILL_CODE_REVIEW).unwrap();
        mgr.load_skill(SKILL_RUST_STYLE).unwrap();
        mgr.load_skill(SKILL_SECRET).unwrap();
        let descs = mgr.descriptions_for_llm();
        assert_eq!(descs.len(), 2);
        assert!(descs.iter().any(|d| d.name == "code-review"));
        assert!(!descs.iter().any(|d| d.name == "secret-internal"));
    }

    #[test]
    fn skill_manager_file_suffix_match() {
        let mgr = SkillManager::new();
        mgr.load_skill(SKILL_RUST_TEST).unwrap();
        mgr.load_skill(SKILL_CODE_REVIEW).unwrap();
        let skills = mgr.resolve_by_file_suffix(".rs");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "rust-unit-test");
        assert!(mgr.resolve_by_file_suffix(".py").is_empty());
    }

    #[test]
    fn skill_manager_inject_to_prompt() {
        let mgr = SkillManager::new();
        mgr.load_skill(SKILL_CODE_REVIEW).unwrap();
        mgr.load_skill(SKILL_RUST_STYLE).unwrap();
        let base = "You are a coding assistant.";
        let prompt = mgr.inject_to_system_prompt(base, &["code-review"]);
        assert!(prompt.contains("coding assistant"));
        assert!(prompt.contains("Always-Active Skills"));
        assert!(prompt.contains("Skill: code-review"));
        assert!(prompt.contains("Available Skills"));
    }

    #[test]
    fn skill_manager_inject_no_always_on() {
        let mgr = SkillManager::new();
        mgr.load_skill(SKILL_CODE_REVIEW).unwrap();
        let prompt = mgr.inject_to_system_prompt("Base.", &[]);
        assert!(prompt.contains("Available Skills"));
        assert!(!prompt.contains("Always-Active Skills"));
    }

    #[test]
    fn skill_manager_load_dir() {
        let dir = std::env::temp_dir().join("starriver-skill-test-load");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let sub = dir.join("my-skill");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("SKILL.md"), SKILL_CODE_REVIEW).unwrap();
        let mgr = SkillManager::new();
        let names = mgr.load_dir(&dir).unwrap();
        assert_eq!(names, vec!["code-review"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn skill_manager_load_dir_empty() {
        let dir = std::env::temp_dir().join("starriver-skill-empty-load");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mgr = SkillManager::new();
        let names = mgr.load_dir(&dir).unwrap();
        assert!(names.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn skill_manager_handles_invalid_skill() {
        let mgr = SkillManager::new();
        let result = mgr.load_skill("Not a valid skill markdown");
        assert!(result.is_err());
        assert_eq!(mgr.len(), 0);
    }

    #[test]
    fn skill_manager_command_binding_without_model_invocation() {
        let mgr = SkillManager::new();
        mgr.load_skill(SKILL_SECRET).unwrap();
        let descs = mgr.descriptions_for_llm();
        assert!(!descs.iter().any(|d| d.name == "secret-internal"));
        assert!(mgr.get("secret-internal").is_some());
    }

    #[test]
    fn skill_manager_loads_duplicate_skill_overwrites() {
        let mgr = SkillManager::new();
        mgr.load_skill(SKILL_CODE_REVIEW).unwrap();
        assert_eq!(mgr.len(), 1);
        // Loading same skill again should overwrite (same name)
        let name = mgr.load_skill(SKILL_CODE_REVIEW).unwrap();
        assert_eq!(name, "code-review");
        assert_eq!(mgr.len(), 1, "Duplicate load should not increase count");
    }

    const SKILL_GRILL_ME: &str = r#"---
name: grill-me
description: Deep-dive requirement analysis tool
conditions:
  trigger:
    trigger_type: keyword
    patterns: ["demand analysis", "deep dive", "grill"]
---

## Deep-dive Analysis Process
1. Identify core requirements
2. Question assumptions
3. Suggest alternatives
"#;

    #[test]
    fn skill_manager_resolve_by_keyword() {
        let mgr = SkillManager::new();
        mgr.load_skill(SKILL_GRILL_ME).unwrap();
        mgr.load_skill(SKILL_CODE_REVIEW).unwrap();

        // Keyword match
        let skills = mgr.resolve_by_keyword("I need demand analysis for this project");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "grill-me");

        // Case-insensitive match
        let skills = mgr.resolve_by_keyword("Let's do a DEEP DIVE into the code");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "grill-me");

        // No match
        let skills = mgr.resolve_by_keyword("write some code");
        assert!(skills.is_empty());
    }

    #[test]
    fn skill_manager_keyword_match_does_not_trigger_wrong_skill() {
        let mgr = SkillManager::new();
        mgr.load_skill(SKILL_GRILL_ME).unwrap();
        mgr.load_skill(SKILL_CODE_REVIEW).unwrap();

        // "review" should not trigger "grill-me"
        let skills = mgr.resolve_by_keyword("please review this code");
        assert!(skills.is_empty(), "review should not trigger grill-me");
    }

    const SKILL_HOOK: &str = r#"---
name: security-audit
description: Audit tool usage for security
conditions:
  hooks:
    preToolUse:
      - matcher: ["bash", "write"]
        script: audit.sh
    postToolResult:
      - matcher: ["bash"]
        script: verify.sh
---

## Security Audit
- Monitor bash and write tool usage
- Verify outputs for security issues
"#;

    #[test]
    fn skill_manager_resolve_by_hook() {
        let mgr = SkillManager::new();
        mgr.load_skill(SKILL_HOOK).unwrap();
        mgr.load_skill(SKILL_CODE_REVIEW).unwrap();

        // preToolUse hook should return security-audit
        let skills = mgr.resolve_by_hook("preToolUse");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "security-audit");

        // postToolResult hook should also work
        let skills = mgr.resolve_by_hook("postToolResult");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "security-audit");

        // Unknown hook type returns empty
        assert!(mgr.resolve_by_hook("unknownHook").is_empty());
    }

    // ── BUG-17: regex trigger_type was declared but never honored ──

    const SKILL_DEPLOY_REGEX: &str = r#"---
name: deploy-guard
description: Guard production deployments
conditions:
  trigger:
    trigger_type: regex
    patterns: ["^deploy\\s+prod"]
---

## Deployment Guard
- Require change-plan reference before deploy prod
"#;

    #[test]
    fn regex_trigger_matches_by_pattern_semantics() {
        let mgr = SkillManager::new();
        mgr.load_skill(SKILL_DEPLOY_REGEX).unwrap();

        // `^deploy\s+prod` must match input that starts with "deploy prod"
        // (with any whitespace between) — literal `contains` cannot.
        let skills = mgr.resolve_by_keyword("deploy\tprod now please");
        assert_eq!(
            skills.len(),
            1,
            "regex trigger_type must match by pattern, not literal substring"
        );
        assert_eq!(skills[0].name, "deploy-guard");

        // Mid-sentence mention must NOT match (`^` anchor).
        assert!(
            mgr.resolve_by_keyword("please deploy prod").is_empty(),
            "the ^ anchor must exclude mid-sentence matches"
        );

        // The pattern text itself must not literal-match as a false positive.
        assert!(
            mgr.resolve_by_keyword("docs mention ^deploy\\s+prod literally").is_empty(),
            "regex metacharacters must not be matched as literal text"
        );
    }

    // ── BUG-18: duplicate register duplicated suffix/hook resolutions ──

    #[test]
    fn reregistered_skill_resolves_once_per_mode() {
        let mgr = SkillManager::new();
        // Reloading the same skill (same name) overwrites the profile entry
        // (`skill_manager_loads_duplicate_skill_overwrites` pins len()==1)…
        mgr.load_skill(SKILL_RUST_TEST).unwrap();
        mgr.load_skill(SKILL_RUST_TEST).unwrap();
        assert_eq!(mgr.len(), 1);
        // …so each activation mode must resolve it exactly once too.
        assert_eq!(
            mgr.resolve_by_file_suffix(".rs").len(),
            1,
            "duplicate register must not duplicate suffix resolutions"
        );

        mgr.load_skill(SKILL_HOOK).unwrap();
        mgr.load_skill(SKILL_HOOK).unwrap();
        assert_eq!(
            mgr.resolve_by_hook("preToolUse").len(),
            1,
            "duplicate register must not duplicate hook resolutions"
        );
        assert_eq!(mgr.resolve_by_hook("postToolResult").len(), 1);
    }
}
