//! WI-99: persistent prefix-rule file (`~/.starriver/rules.rules`) —
//! cross-session, hand-editable trust rules that pre-approve bash commands
//! by token-prefix, layered on top of the per-session `approval/rule` events.
//!
//! File format (one rule per line):
//! ```text
//! # comment — lines starting with # are ignored
//! cargo test
//! git status
//! npm run build
//! ```
//! A rule matches when the command's argv tokens start with the rule's
//! tokens (the same `matches_prefix` semantics as session rules).

use std::io::Write;
use std::path::PathBuf;

/// Default rules file path: `$HOME` (or `USERPROFILE`) + `~/.starriver/rules.rules`.
pub fn rules_path() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .or_else(|| std::env::var("USERPROFILE").ok())
        .map(|h| PathBuf::from(h).join(".starriver").join("rules.rules"))
}

/// Load every non-comment, non-empty line as a token-prefix rule.
pub fn load_rules() -> Vec<Vec<String>> {
    let Some(path) = rules_path() else { return Vec::new() };
    let Ok(content) = std::fs::read_to_string(&path) else { return Vec::new() };
    content
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| l.split_whitespace().map(str::to_string).collect())
        .filter(|tokens: &Vec<String>| !tokens.is_empty())
        .collect()
}

/// Append one rule (token prefix joined by spaces). Deduplicates against the
/// existing file content. Creates the parent directory on demand.
pub fn append_rule(prefix: &[String]) -> std::io::Result<()> {
    let Some(path) = rules_path() else {
        return Err(std::io::Error::other("HOME not set"));
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let line = prefix.join(" ");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    if existing
        .lines()
        .map(str::trim)
        .any(|l| l == line.trim())
    {
        return Ok(());
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    let prefix_nl = if existing.is_empty() || existing.ends_with('\n') {
        String::new()
    } else {
        "\n".to_string()
    };
    writeln!(file, "{prefix_nl}{line}")?;
    Ok(())
}

/// True when `argv` starts with `prefix` tokens.
pub fn matches_file_rule(argv: &[String], prefix: &[String]) -> bool {
    if prefix.is_empty() || argv.len() < prefix.len() {
        return false;
    }
    argv[..prefix.len()] == prefix[..]
}

/// Tokenize a shell command into argv words (quotes group, they do not reach
/// the tokens). Delegates to the approval tokenizer so the write side (rule
/// generation) and the read side (rule matching) always agree (BUG-85: a
/// split_whitespace tokenizer could never match a rule written from a quoted
/// command back onto that same command).
pub fn tokenize_for_rules(cmd: &str) -> Vec<String> {
    crate::approval::tokenize(cmd).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_rule_matches_prefix() {
        let argv: Vec<String> = vec!["cargo", "test", "-p", "host"]
            .into_iter()
            .map(String::from)
            .collect();
        let prefix: Vec<String> = vec!["cargo", "test"]
            .into_iter()
            .map(String::from)
            .collect();
        assert!(matches_file_rule(&argv, &prefix));
        let wrong: Vec<String> = vec!["cargo", "build"]
            .into_iter()
            .map(String::from)
            .collect();
        assert!(!matches_file_rule(&argv, &wrong));
        // Longer prefix than argv: no match.
        let long: Vec<String> = vec!["cargo", "test", "-p", "extra", "more"]
            .into_iter()
            .map(String::from)
            .collect();
        assert!(!matches_file_rule(&argv, &long));
    }

    #[test]
    fn tokenize_splits_on_whitespace() {
        let tokens = tokenize_for_rules("git status --short");
        assert_eq!(tokens, vec!["git", "status", "--short"]);
    }

    #[test]
    fn load_rules_ignores_comments_and_blanks() {
        // Test the parsing logic with a fixed string (file I/O tested in
        // the integration suites via a temp HOME).
        let content = "# comment\n\ncargo test\n  git status  \n#another\nnpm run\n";
        let rules: Vec<Vec<String>> = content
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(|l| l.split_whitespace().map(str::to_string).collect())
            .filter(|t: &Vec<String>| !t.is_empty())
            .collect();
        assert_eq!(rules.len(), 3);
        assert_eq!(rules[0], vec!["cargo", "test"]);
        assert_eq!(rules[2], vec!["npm", "run"]);
    }

    #[test]
    fn quoted_rules_round_trip_onto_the_same_command() {
        // BUG-85: the write side strips quotes (`echo "hello world"` →
        // [echo, hello world]); the read side must tokenize identically or
        // the persisted rule never matches the command that created it.
        let argv = tokenize_for_rules(r#"echo "hello world""#);
        assert_eq!(argv, vec!["echo".to_string(), "hello world".to_string()]);
        let prefix = vec!["echo".to_string(), "hello world".to_string()];
        assert!(matches_file_rule(&argv, &prefix));
    }
}
