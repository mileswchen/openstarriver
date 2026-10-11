//! Automatic write-semantics negotiation for MCP tools.
//!
//! A bridged MCP tool used to carry `requires_approval: true` and
//! `deny_in_readonly: false` unconditionally (write semantics unknown). This
//! module classifies each discovered tool from two signals:
//!
//! 1. **MCP annotations** (`Tool.annotations`): `readOnlyHint` /
//!    `destructiveHint` — the server's own declaration.
//! 2. **Name heuristics**: the raw tool name tokenized (snake/kebab/spaced +
//!    camelCase boundaries) and matched against a destructive vocabulary.
//!
//! Classification is deliberately fail-closed:
//!
//! - `destructiveHint: true` **or** a destructive name token →
//!   [`WriteSemantics::Destructive`] (manual approval; refused in a
//!   read-only sandbox) — even when `readOnlyHint` contradicts it.
//! - otherwise `readOnlyHint: true` → [`WriteSemantics::ReadOnly`] (auto:
//!   no approval prompt, allowed in read-only sandbox).
//! - anything else → [`WriteSemantics::Unknown`] (manual approval, not
//!   pre-refused in read-only) — exactly the legacy behavior.
//!
//! Name heuristics only ever RAISE risk (toward Destructive); they can never
//! mark a tool read-only, because a mis-named writing tool that slips past
//! approval is far worse than an extra prompt.

use serde::{Deserialize, Serialize};

/// The subset of MCP `Tool.annotations` that informs write semantics.
/// Absent annotations deserialize to `None` per field.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpToolAnnotations {
    #[serde(default, rename = "readOnlyHint")]
    pub read_only_hint: Option<bool>,
    #[serde(default, rename = "destructiveHint")]
    pub destructive_hint: Option<bool>,
    #[serde(default, rename = "idempotentHint")]
    pub idempotent_hint: Option<bool>,
    #[serde(default, rename = "openWorldHint")]
    pub open_world_hint: Option<bool>,
}

/// Negotiated write semantics of one MCP tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteSemantics {
    /// Server-declared read-only: no side effects.
    ReadOnly,
    /// Side-effecting / dangerous: manual approval; refused in read-only
    /// sandboxes before approval.
    Destructive,
    /// Neither signal present: manual approval, not pre-refused (legacy
    /// behavior).
    Unknown,
}

/// Whole-token vocabulary (after tokenization) that marks a tool as having
/// side effects. Curated to read as verbs/operations; a token must match
/// exactly, so `settings` never trips `set`.
const DESTRUCTIVE_NAME_TOKENS: &[&str] = &[
    "delete",
    "remove",
    "destroy",
    "drop",
    "purge",
    "kill",
    "terminate",
    "reset",
    "clear",
    "write",
    "create",
    "insert",
    "add",
    "update",
    "modify",
    "edit",
    "set",
    "push",
    "publish",
    "deploy",
    "release",
    "send",
    "post",
    "put",
    "patch",
    "exec",
    "execute",
    "run",
    "apply",
    "commit",
    "merge",
    "install",
    "uninstall",
    "rename",
    "move",
    "upload",
    "submit",
    "save",
    "grant",
    "revoke",
    "disable",
    "enable",
    "start",
    "stop",
    "restart",
    "cancel",
    "revert",
    "rollback",
    "shutdown",
    "reboot",
];

/// Tokenize a raw MCP tool name into lowercase words: split on
/// non-alphanumerics and camelCase/hump boundaries (`deleteIssue` →
/// `["delete", "issue"]`, `delete-issue` → `["delete", "issue"]`).
fn tokenize_name(name: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut prev_lowercase = false;
    let chars: Vec<char> = name.chars().collect();
    for (i, &ch) in chars.iter().enumerate() {
        if ch.is_ascii_alphanumeric() {
            // BUG-61: acronym boundary — an uppercase that STARTS a word
            // (previous char uppercase, next char lowercase, e.g. the D in
            // SSEDeleteRows) splits too, or the destructive verb is
            // swallowed into the acronym token.
            let acronym_start = ch.is_ascii_uppercase()
                && !prev_lowercase
                && !current.is_empty()
                && chars.get(i + 1).is_some_and(|n| n.is_ascii_lowercase());
            if (ch.is_ascii_uppercase() && prev_lowercase) || acronym_start {
                tokens.push(std::mem::take(&mut current));
            }
            prev_lowercase = ch.is_ascii_lowercase() || ch.is_ascii_digit();
            current.push(ch.to_ascii_lowercase());
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
            prev_lowercase = false;
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// True when any whole token of the raw tool name is in the destructive
/// vocabulary (`settings`, `getter`, `position` must NOT trip `set`/`get`).
pub fn is_destructive_by_name(raw_name: &str) -> bool {
    tokenize_name(raw_name)
        .iter()
        .any(|token| DESTRUCTIVE_NAME_TOKENS.contains(&token.as_str()))
}

/// Negotiate the write semantics of one tool from its annotations and name.
pub fn classify_write_semantics(
    raw_name: &str,
    annotations: &McpToolAnnotations,
) -> WriteSemantics {
    if annotations.destructive_hint == Some(true) || is_destructive_by_name(raw_name) {
        return WriteSemantics::Destructive;
    }
    if annotations.read_only_hint == Some(true) {
        return WriteSemantics::ReadOnly;
    }
    WriteSemantics::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ann(read_only: Option<bool>, destructive: Option<bool>) -> McpToolAnnotations {
        McpToolAnnotations {
            read_only_hint: read_only,
            destructive_hint: destructive,
            idempotent_hint: None,
            open_world_hint: None,
        }
    }

    #[test]
    fn read_only_annotation_negotiates_auto() {
        assert_eq!(
            classify_write_semantics("search", &ann(Some(true), None)),
            WriteSemantics::ReadOnly
        );
    }

    #[test]
    fn destructive_annotation_beats_read_only_contradiction() {
        // Server declares both: fail closed on the destructive signal.
        assert_eq!(
            classify_write_semantics("query", &ann(Some(true), Some(true))),
            WriteSemantics::Destructive
        );
    }

    #[test]
    fn destructive_name_heuristic_raises_risk() {
        let none = ann(None, None);
        assert_eq!(
            classify_write_semantics("delete_issue", &none),
            WriteSemantics::Destructive
        );
        // camelCase boundary
        assert_eq!(
            classify_write_semantics("deleteIssue", &none),
            WriteSemantics::Destructive
        );
        // A destructive token inside a longer word must NOT match.
        assert_eq!(
            classify_write_semantics("issueDeleter", &none),
            WriteSemantics::Unknown
        );
    }

    #[test]
    fn read_only_annotation_with_destructive_name_fails_closed() {
        // Server says read-only but the name says delete → destructive.
        assert_eq!(
            classify_write_semantics("delete_cache", &ann(Some(true), None)),
            WriteSemantics::Destructive
        );
    }

    #[test]
    fn unknown_when_no_signal() {
        assert_eq!(
            classify_write_semantics("fetch_weather", &ann(None, None)),
            WriteSemantics::Unknown
        );
        // Explicit non-hints are not read-only declarations either.
        assert_eq!(
            classify_write_semantics("fetch_weather", &ann(None, Some(false))),
            WriteSemantics::Unknown
        );
    }

    #[test]
    fn benign_tokens_stay_unknown() {
        let none = ann(None, None);
        for name in [
            "get_settings",
            "settings",
            "list_positions",
            "searchRepos",
            "read_file",
        ] {
            assert_eq!(
                classify_write_semantics(name, &none),
                WriteSemantics::Unknown,
                "{name} must not be classified destructive"
            );
        }
    }

    #[test]
    fn annotations_parse_from_mcp_json() {
        let raw = serde_json::json!({
            "readOnlyHint": true,
            "idempotentHint": true,
        });
        let parsed: McpToolAnnotations = serde_json::from_value(raw).unwrap();
        assert_eq!(parsed.read_only_hint, Some(true));
        assert_eq!(parsed.destructive_hint, None);
        assert_eq!(parsed.idempotent_hint, Some(true));
        // Absent entirely → defaults.
        let empty: McpToolAnnotations = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(empty, McpToolAnnotations::default());
    }

    #[test]
    fn destructive_vocabulary_is_all_lowercase_verbs() {
        for token in DESTRUCTIVE_NAME_TOKENS {
            assert_eq!(*token, token.to_ascii_lowercase());
        }
    }
}

// ── BUG-61 regression (round 5): acronym camelCase boundaries ──
#[cfg(test)]
mod round5_tests {
    use super::*;

    /// The heuristic "only raises risk" — an acronym prefix must not swallow
    /// the destructive verb that follows it (SSEDeleteRows lost `delete`
    /// entirely and classified as Unknown instead of Destructive).
    #[test]
    fn acronym_prefix_does_not_swallow_the_verb() {
        assert!(
            is_destructive_by_name("SSEDeleteRows"),
            "acronym + Delete must classify destructive"
        );
        assert!(is_destructive_by_name("HTTPDelete"));
        assert!(is_destructive_by_name("APIPurgeCache"));
        // Control: the plain forms keep working.
        assert!(is_destructive_by_name("deleteIssue"));
        assert!(!is_destructive_by_name("issueDeleter"));
    }
}
