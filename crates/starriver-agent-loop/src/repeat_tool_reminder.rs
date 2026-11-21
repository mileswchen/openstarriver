//! Advisory repeat-tool reminder (DSH `guard/repeat-tool-reminder` parity).
//!
//! Counts consecutive IDENTICAL tool calls — same tool name plus deep-key-sort
//! canonical arguments — and, when the count reaches a configured threshold,
//! injects a gentle→detailed notice as a surface user message that rides the
//! next assembly. It is purely ADVISORY: it never blocks, skips, or aborts a
//! call. The hard backstops (consecutive-failure breaker, reload storm, the
//! turn tool-call budget) live in `tool_calls`/`agent` and remain the only
//! things that can end a turn early.
//!
//! Why this exists: the retired prose-degeneracy detector (`degenerate_loop`)
//! false-positived on legitimate fixed-template enumeration streamed as prose
//! (see `design/degenerate-loop-false-positive.md`). DSH's answer is to NOT
//! detect prose at all — instead nudge the model when it repeats the *same
//! action* with the *same arguments*, which is the observable signature of a
//! stuck tool loop, and leave prose generation entirely alone.
//!
//! Semantics (mirrors DSH):
//! - A single chain tracks the last tracked call's key + consecutive count.
//! - A different tracked call resets the chain to 1.
//! - Excluded tools are TRANSPARENT: they neither count nor reset the chain.
//! - `include` (when non-empty) restricts tracking to matching tools.
//! - Both successful and failed/denied executions count (a model hammering a
//!   failing call is exactly the stuck shape).
//! - The chain is turn-scoped; `reset_keeping_cursor` clears it per turn while
//!   preserving the absorb cursor (WI-202 parity — no history replay).

use std::collections::HashMap;

use starriver_kernel::agent::{AgentOptions, RepeatReminderOptions};
use starriver_kernel::llm::{
    mint_message_id, ContentBlock, ContextForm, Message, MessageRole, MessageSource,
};
use starriver_kernel::session::Session;

/// Plugin attribution for the injected notice (DSH `source.kind='plugin'`).
pub const REMINDER_PLUGIN: &str = "harness:repeat-tool-reminder";

/// Engine-default thresholds: gentle nudge at 3, detailed at 5 and 8.
pub const DEFAULT_THRESHOLDS: [u32; 3] = [3, 5, 8];
/// Engine-default cap on the detailed reminder's argument-preview text.
pub const DEFAULT_PREVIEW_CHARS: usize = 500;

/// Resolved advisory configuration. All fields concrete (defaults applied).
#[derive(Debug, Clone, PartialEq)]
pub struct RepeatReminderConfig {
    pub enabled: bool,
    pub thresholds: Vec<u32>,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub args_preview_chars: usize,
}

impl Default for RepeatReminderConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            thresholds: DEFAULT_THRESHOLDS.to_vec(),
            include: Vec::new(),
            exclude: Vec::new(),
            args_preview_chars: DEFAULT_PREVIEW_CHARS,
        }
    }
}

impl RepeatReminderConfig {
    /// Build from agent options, applying engine defaults for unset fields.
    pub fn from_options(options: &AgentOptions) -> Self {
        let mut config = Self::default();
        if let Some(ref opts) = options.repeat_reminder {
            apply_options(&mut config, opts);
        }
        config
    }

    /// Fail-loud validation (DSH rejects bad config at load). Thresholds must
    /// be non-empty, each ≥ 2, and unique; the preview cap must be positive.
    pub fn validate(&self) -> Result<(), String> {
        if self.thresholds.is_empty() {
            return Err("repeat-tool-reminder thresholds must not be empty".into());
        }
        if self.thresholds.iter().any(|&t| t < 2) {
            return Err("repeat-tool-reminder thresholds must each be >= 2".into());
        }
        let mut sorted = self.thresholds.clone();
        sorted.sort_unstable();
        for pair in sorted.windows(2) {
            if pair[0] == pair[1] {
                return Err(format!(
                    "repeat-tool-reminder thresholds contain a duplicate: {}",
                    pair[0]
                ));
            }
        }
        if self.args_preview_chars == 0 {
            return Err("repeat-tool-reminder args_preview_chars must be > 0".into());
        }
        Ok(())
    }
}

fn apply_options(config: &mut RepeatReminderConfig, opts: &RepeatReminderOptions) {
    if let Some(enabled) = opts.enabled {
        config.enabled = enabled;
    }
    if let Some(ref thresholds) = opts.thresholds {
        config.thresholds = thresholds.clone();
    }
    if let Some(ref include) = opts.include {
        config.include = include.clone();
    }
    if let Some(ref exclude) = opts.exclude {
        config.exclude = exclude.clone();
    }
    if let Some(chars) = opts.args_preview_chars {
        config.args_preview_chars = chars;
    }
}

/// One executed call awaiting its result row (joined by `source_event_seqs`).
struct PendingCall {
    sig: String,
    name: String,
    preview: String,
}

/// The single consecutive-identical-call chain.
struct Chain {
    key: String,
    name: String,
    preview: String,
    count: u32,
}

/// Per-turn advisory repeat accounting. Owns its own absorb cursor so it scans
/// the session tail independently of the failure breaker.
pub struct RepeatToolReminder {
    cursor: u64,
    pending: HashMap<u64, PendingCall>,
    chain: Option<Chain>,
    config: RepeatReminderConfig,
}

impl RepeatToolReminder {
    /// Construct with validated config. Panics on invalid config — a fail-loud
    /// backstop; the settings boundary validates user input first so this only
    /// fires on a programming error (e.g. a malformed preset).
    pub fn new(config: RepeatReminderConfig) -> Self {
        if let Err(error) = config.validate() {
            panic!("invalid repeat-tool-reminder configuration: {error}");
        }
        Self {
            cursor: 0,
            pending: HashMap::new(),
            chain: None,
            config,
        }
    }

    /// Construct from agent options (defaults applied, fail-loud validated).
    pub fn from_options(options: &AgentOptions) -> Self {
        Self::new(RepeatReminderConfig::from_options(options))
    }

    /// Per-turn reset that KEEPS the absorb cursor (WI-202 parity): zeroing it
    /// would replay the whole session history and re-count prior turns.
    pub fn reset_keeping_cursor(&mut self) {
        self.pending.clear();
        self.chain = None;
    }

    /// Whether a tool name is tracked: exclude wins (transparent), then a
    /// non-empty include whitelist restricts tracking.
    fn tracked(&self, name: &str) -> bool {
        if self.config.exclude.iter().any(|p| glob_match(p, name)) {
            return false;
        }
        if !self.config.include.is_empty()
            && !self.config.include.iter().any(|p| glob_match(p, name))
        {
            return false;
        }
        true
    }

    /// Tail-scan executed tool/call→tool/result pairs, advance the chain, and
    /// return a reminder message when a threshold is crossed. Counts EVERY
    /// executed call (success or failure). Never blocks. When several
    /// thresholds are crossed in one batch the last (most urgent) is returned.
    pub fn absorb(&mut self, session: &Session) -> Option<Message> {
        let mut reminder = None;
        for event in session.events_after(self.cursor) {
            self.cursor = self.cursor.max(event.seq);
            if !self.config.enabled {
                continue;
            }
            match event.event_type.as_str() {
                "tool/call" => {
                    let is_reload = event.data.as_str_at("name")
                        == Some(crate::image_projection::RELOAD_IMAGE_TOOL);
                    let breaker_row =
                        event.data.get("repeatBreak").and_then(|v| v.as_bool()) == Some(true);
                    if is_reload || breaker_row {
                        continue;
                    }
                    if let (Some(name), Some(args)) =
                        (event.data.as_str_at("name"), event.data.get("arguments"))
                    {
                        // Excluded/untracked calls are transparent: leaving
                        // them out of `pending` means their result never
                        // touches the chain.
                        if !self.tracked(name) {
                            continue;
                        }
                        let sig = crate::tool_calls::call_signature(name, args);
                        let preview = truncate_chars(
                            &crate::tool_calls::canonical_arguments(args),
                            self.config.args_preview_chars,
                        );
                        self.pending.insert(
                            event.seq,
                            PendingCall {
                                sig,
                                name: name.to_string(),
                                preview,
                            },
                        );
                    }
                }
                "tool/result" => {
                    if let Some(&linked) = event.source_event_seqs.first() {
                        if let Some(call) = self.pending.remove(&linked) {
                            let count = if self
                                .chain
                                .as_ref()
                                .is_some_and(|chain| chain.key == call.sig)
                            {
                                let chain = self.chain.as_mut().expect("chain present");
                                chain.count += 1;
                                chain.count
                            } else {
                                self.chain = Some(Chain {
                                    key: call.sig,
                                    name: call.name,
                                    preview: call.preview,
                                    count: 1,
                                });
                                1
                            };
                            if self.config.thresholds.contains(&count) {
                                reminder = Some(self.build_reminder(count));
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        reminder
    }

    /// Build the advisory notice. The smallest threshold gets a gentle nudge;
    /// every larger threshold gets a detailed reminder (tool, count, capped
    /// argument preview). Attributed as a plugin notice so the surface treats
    /// it as advisory context, not a human prompt.
    fn build_reminder(&self, count: u32) -> Message {
        let chain = self.chain.as_ref().expect("chain set when reminder fires");
        let first = self
            .config
            .thresholds
            .iter()
            .min()
            .copied()
            .unwrap_or(count);
        let text = if count == first {
            "You are repeating the same tool call with identical arguments. \
             Inspect the latest result first: if the task is not finished, change \
             your approach or arguments rather than repeating the call."
                .to_string()
        } else {
            format!(
                "Repeated tool call: tool={} consecutive={} arguments={}. These \
                 repeats are not making progress. Do not call this tool again with \
                 these exact arguments. Inspect the latest result, take a different \
                 action or use different arguments, or wrap up if the evidence is \
                 sufficient.",
                chain.name, count, chain.preview
            )
        };
        let summary = format!(
            "repeat-tool-reminder: {} identical calls (n={})",
            chain.name, count
        );
        Message {
            id: mint_message_id(),
            role: MessageRole::User,
            content: vec![ContentBlock::Text(text)],
            source: MessageSource::Plugin {
                plugin: REMINDER_PLUGIN.into(),
                form: Some(ContextForm::Notice { summary }),
            },
        }
    }
}

/// Wildcard glob with `*` only (DSH parity): `*` matches any run of characters.
/// A pattern without `*` is an exact match.
fn glob_match(pattern: &str, name: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == name;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    let mut rest = name;
    if let Some(first) = parts.first() {
        if !first.is_empty() {
            if !rest.starts_with(first) {
                return false;
            }
            rest = &rest[first.len()..];
        }
    }
    if let Some(last) = parts.last() {
        if !last.is_empty() {
            if !rest.ends_with(last) {
                return false;
            }
            rest = &rest[..rest.len() - last.len()];
        }
    }
    for middle in &parts[1..parts.len().saturating_sub(1)] {
        if middle.is_empty() {
            continue;
        }
        match rest.find(middle) {
            Some(pos) => rest = &rest[pos + middle.len()..],
            None => return false,
        }
    }
    true
}

/// Cap a preview to `max` Unicode code points, appending an ellipsis when
/// truncated. The detection key always uses the FULL canonical arguments.
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use cordis_rslite::EventBus;
    use starriver_kernel::json::JsonValue;
    use starriver_kernel::session::{AppendOptions, Session, SessionId};

    fn session() -> Session {
        Session::new(SessionId("test-repeat-reminder".into()), EventBus::new())
    }

    fn args(pairs: Vec<(&str, &str)>) -> JsonValue {
        JsonValue::Object(
            pairs
                .into_iter()
                .map(|(k, v)| (k.to_string(), JsonValue::String(v.to_string())))
                .collect(),
        )
    }

    /// Append a tool/call + its joined tool/result (executed, success by
    /// default). Returns nothing; the reminder absorbs by tail-scan.
    fn executed_call(session: &Session, name: &str, arguments: JsonValue, error: bool) {
        let call_seq = session
            .append_with(
                "tool/call",
                JsonValue::Object(vec![
                    ("name".into(), JsonValue::String(name.into())),
                    ("arguments".into(), arguments),
                ]),
                AppendOptions::default(),
            )
            .expect("tool/call append")
            .seq;
        let mut data = vec![("turn".into(), JsonValue::UInt(1))];
        if error {
            data.push((
                "error".into(),
                JsonValue::Object(vec![("code".into(), JsonValue::String("BOOM".into()))]),
            ));
        }
        session
            .append_with(
                "tool/result",
                JsonValue::Object(data),
                AppendOptions {
                    source_event_seqs: vec![call_seq],
                    ..Default::default()
                },
            )
            .expect("tool/result append");
    }

    fn reminder_text(message: &Message) -> String {
        match message.content.first() {
            Some(ContentBlock::Text(text)) => text.clone(),
            _ => String::new(),
        }
    }

    #[test]
    fn canonical_key_is_property_order_independent() {
        let a = args(vec![("x", "1"), ("y", "2")]);
        let b = args(vec![("y", "2"), ("x", "1")]);
        assert_eq!(
            crate::tool_calls::call_signature("t", &a),
            crate::tool_calls::call_signature("t", &b)
        );
        // String contents are preserved (not whitespace-stripped).
        let spaced = args(vec![("q", "a b")]);
        let tight = args(vec![("q", "ab")]);
        assert_ne!(
            crate::tool_calls::call_signature("t", &spaced),
            crate::tool_calls::call_signature("t", &tight)
        );
    }

    #[test]
    fn fires_gentle_at_three_and_detailed_at_five_and_eight() {
        let session = session();
        let mut guard = RepeatToolReminder::new(RepeatReminderConfig::default());
        let arguments = args(vec![("q", "same")]);
        for n in 1..=8u32 {
            executed_call(&session, "search", arguments.clone(), false);
            let reminder = guard.absorb(&session);
            match n {
                3 => {
                    let message = reminder.expect("gentle at 3");
                    assert!(reminder_text(&message).contains("repeating the same tool call"));
                    assert!(!reminder_text(&message).contains("consecutive="));
                }
                5 | 8 => {
                    let message = reminder.expect("detailed at 5/8");
                    let text = reminder_text(&message);
                    assert!(text.contains("consecutive="));
                    assert!(text.contains("search"));
                }
                _ => assert!(reminder.is_none(), "silent at n={n}"),
            }
        }
    }

    #[test]
    fn different_tracked_call_resets_the_chain() {
        let session = session();
        let mut guard = RepeatToolReminder::new(RepeatReminderConfig::default());
        let a = args(vec![("q", "a")]);
        let b = args(vec![("q", "b")]);
        executed_call(&session, "search", a.clone(), false);
        executed_call(&session, "search", a.clone(), false);
        assert!(guard.absorb(&session).is_none());
        // A different argument resets to 1 — no reminder.
        executed_call(&session, "search", b, false);
        assert!(guard.absorb(&session).is_none());
        // Back to `a`: count is 1 again, not 3.
        executed_call(&session, "search", a.clone(), false);
        executed_call(&session, "search", a, false);
        assert!(guard.absorb(&session).is_none(), "reset means only 2 in a row");
    }

    #[test]
    fn excluded_tools_are_transparent() {
        let session = session();
        let config = RepeatReminderConfig {
            exclude: vec!["todo_*".into()],
            ..Default::default()
        };
        let mut guard = RepeatToolReminder::new(config);
        let a = args(vec![("q", "a")]);
        executed_call(&session, "search", a.clone(), false);
        executed_call(&session, "search", a.clone(), false);
        // Excluded calls in between neither count nor reset.
        executed_call(&session, "todo_write", args(vec![("t", "x")]), false);
        executed_call(&session, "todo_write", args(vec![("t", "y")]), false);
        executed_call(&session, "search", a, false);
        let reminder = guard.absorb(&session).expect("3rd search fires despite todos");
        assert!(reminder_text(&reminder).contains("repeating"));
    }

    #[test]
    fn include_whitelist_restricts_tracking() {
        let session = session();
        let config = RepeatReminderConfig {
            include: vec!["search".into()],
            ..Default::default()
        };
        let mut guard = RepeatToolReminder::new(config);
        let a = args(vec![("q", "a")]);
        // Untracked tool repeated 5× — never fires.
        for _ in 0..5 {
            executed_call(&session, "other", a.clone(), false);
        }
        assert!(guard.absorb(&session).is_none());
        // Tracked tool fires at 3.
        for _ in 0..3 {
            executed_call(&session, "search", a.clone(), false);
        }
        assert!(guard.absorb(&session).is_some());
    }

    #[test]
    fn failed_calls_count_too() {
        let session = session();
        let mut guard = RepeatToolReminder::new(RepeatReminderConfig::default());
        let a = args(vec![("q", "a")]);
        for _ in 0..3 {
            executed_call(&session, "search", a.clone(), true);
        }
        assert!(guard.absorb(&session).is_some(), "denied/failing repeats count");
    }

    #[test]
    fn new_turn_reset_keeps_cursor_and_clears_chain() {
        let session = session();
        let mut guard = RepeatToolReminder::new(RepeatReminderConfig::default());
        let a = args(vec![("q", "a")]);
        executed_call(&session, "search", a.clone(), false);
        executed_call(&session, "search", a.clone(), false);
        assert!(guard.absorb(&session).is_none());
        guard.reset_keeping_cursor();
        // After reset the chain is cold; two more calls do not reach 3.
        executed_call(&session, "search", a.clone(), false);
        executed_call(&session, "search", a, false);
        assert!(guard.absorb(&session).is_none(), "prior-turn count not replayed");
    }

    #[test]
    fn disabled_never_fires() {
        let session = session();
        let config = RepeatReminderConfig {
            enabled: false,
            ..Default::default()
        };
        let mut guard = RepeatToolReminder::new(config);
        let a = args(vec![("q", "a")]);
        for _ in 0..10 {
            executed_call(&session, "search", a.clone(), false);
        }
        assert!(guard.absorb(&session).is_none());
    }

    #[test]
    fn preview_is_capped_but_key_uses_full_args() {
        let long = "x".repeat(2000);
        let arguments = args(vec![("q", &long)]);
        let preview = truncate_chars(
            &crate::tool_calls::canonical_arguments(&arguments),
            DEFAULT_PREVIEW_CHARS,
        );
        assert!(preview.chars().count() <= DEFAULT_PREVIEW_CHARS + 1);
        assert!(preview.ends_with('…'));
        // Two calls differing only beyond the preview cap are STILL the same
        // key (full canonical args), so they count as repeats.
        let mut other = long.clone();
        other.push('y');
        let arguments2 = args(vec![("q", &other)]);
        assert_ne!(
            crate::tool_calls::call_signature("t", &arguments),
            crate::tool_calls::call_signature("t", &arguments2)
        );
    }

    #[test]
    fn reminder_is_attributed_as_plugin_notice() {
        let session = session();
        let mut guard = RepeatToolReminder::new(RepeatReminderConfig::default());
        let a = args(vec![("q", "a")]);
        for _ in 0..3 {
            executed_call(&session, "search", a.clone(), false);
        }
        let message = guard.absorb(&session).expect("fires at 3");
        assert_eq!(message.role, MessageRole::User);
        match message.source {
            MessageSource::Plugin { plugin, form } => {
                assert_eq!(plugin, REMINDER_PLUGIN);
                assert!(matches!(form, Some(ContextForm::Notice { .. })));
            }
            other => panic!("expected plugin notice source, got {other:?}"),
        }
    }

    #[test]
    fn glob_matches_star_patterns() {
        assert!(glob_match("search", "search"));
        assert!(!glob_match("search", "search2"));
        assert!(glob_match("todo_*", "todo_write"));
        assert!(glob_match("*_image", "reload_image"));
        assert!(glob_match("mcp__*__read", "mcp__srv__read"));
        assert!(!glob_match("mcp__*__read", "mcp__srv__write"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("a*b", "ab"));
        assert!(glob_match("a*b", "axxb"));
        assert!(!glob_match("a*b", "a"));
    }

    #[test]
    fn config_validation_is_fail_loud() {
        assert!(RepeatReminderConfig::default().validate().is_ok());
        let empty = RepeatReminderConfig {
            thresholds: vec![],
            ..Default::default()
        };
        assert!(empty.validate().is_err());
        let low = RepeatReminderConfig {
            thresholds: vec![1, 3],
            ..Default::default()
        };
        assert!(low.validate().is_err());
        let dup = RepeatReminderConfig {
            thresholds: vec![3, 3, 5],
            ..Default::default()
        };
        assert!(dup.validate().is_err());
        let zero_preview = RepeatReminderConfig {
            args_preview_chars: 0,
            ..Default::default()
        };
        assert!(zero_preview.validate().is_err());
    }

    #[test]
    #[should_panic(expected = "invalid repeat-tool-reminder configuration")]
    fn new_panics_on_invalid_config() {
        let bad = RepeatReminderConfig {
            thresholds: vec![],
            ..Default::default()
        };
        RepeatToolReminder::new(bad);
    }

    #[test]
    fn from_options_applies_defaults_and_overrides() {
        let mut options = AgentOptions::default();
        let config = RepeatReminderConfig::from_options(&options);
        assert_eq!(config, RepeatReminderConfig::default());
        options.repeat_reminder = Some(RepeatReminderOptions {
            enabled: Some(false),
            thresholds: Some(vec![4, 6]),
            include: Some(vec!["search".into()]),
            exclude: Some(vec!["todo_*".into()]),
            args_preview_chars: Some(120),
        });
        let config = RepeatReminderConfig::from_options(&options);
        assert!(!config.enabled);
        assert_eq!(config.thresholds, vec![4, 6]);
        assert_eq!(config.include, vec!["search"]);
        assert_eq!(config.exclude, vec!["todo_*"]);
        assert_eq!(config.args_preview_chars, 120);
    }
}