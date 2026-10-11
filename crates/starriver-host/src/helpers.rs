use starriver_skills::profile::skill::SkillProfile;

/// Built-in cloud provider presets (host version, returns crate::types::ModelProvider).
pub fn builtin_provider_presets(
    enabled: impl Fn(&str) -> bool,
) -> Vec<crate::types::ModelProvider> {
    let cloud = |id: &str,
                 name: &str,
                 base_url: &str,
                 models: Vec<&str>,
                 api_format: &str,
                 reasoning_effort: Option<&str>| crate::types::ModelProvider {
        id: id.into(),
        name: name.into(),
        provider_type: "cloud".into(),
        base_url: base_url.into(),
        models: models.into_iter().map(String::from).collect(),
        requires_api_key: true,
        enabled: enabled(id),
        api_format: Some(api_format.into()),
        reasoning_effort: reasoning_effort.map(String::from),
    };
    vec![
        cloud(
            "deepseek",
            "DeepSeek",
            "https://api.deepseek.com",
            vec!["deepseek-chat", "deepseek-reasoner"],
            "openai-chat",
            Some("high"),
        ),
        cloud(
            "qwen",
            "QWen",
            "https://dashscope.aliyuncs.com/compatible-mode/v1",
            vec!["qwen-turbo", "qwen-plus", "qwen-max"],
            "openai-chat",
            None,
        ),
        cloud(
            "zhipu",
            "GLM Coding Plan - CN",
            "https://open.bigmodel.cn/api/coding/paas/v4",
            vec!["glm-4-plus", "glm-4-flash"],
            "openai-chat",
            None,
        ),
        cloud(
            "moonshot",
            "Moonshot",
            "https://api.moonshot.cn/v1",
            vec!["moonshot-v1-8k", "moonshot-v1-32k", "moonshot-v1-128k"],
            "openai-chat",
            None,
        ),
        crate::types::ModelProvider {
            id: "ollama".into(),
            name: "Ollama".into(),
            provider_type: "local".into(),
            base_url: "http://localhost:11434".into(),
            models: vec![],
            requires_api_key: false,
            enabled: true,
            api_format: None,
            reasoning_effort: None,
        },
    ]
}

/// Flattened ScheduleMode fields for the napi shell.
pub type FlattenedMode = (
    String,
    Option<String>,
    Option<String>,
    Option<u8>,
    Option<u8>,
    Option<u8>,
    Option<u32>,
    Vec<u8>,
    Option<String>,
);

/// Flatten ScheduleMode into NAPI fields (host version, returns a tuple).
pub fn flatten_mode(mode: &crate::scheduler::ScheduleMode) -> FlattenedMode {
    match mode {
        crate::scheduler::ScheduleMode::Periodic {
            period,
            time,
            day_of_week,
            day_of_month,
            month,
        } => (
            "periodic".into(),
            Some(format!("{:?}", period).to_lowercase()),
            Some(time.clone()),
            *day_of_week,
            *day_of_month,
            *month,
            None,
            vec![],
            None,
        ),
        crate::scheduler::ScheduleMode::Interval {
            hours,
            days_of_week,
        } => (
            "interval".into(),
            None,
            None,
            None,
            None,
            None,
            Some(*hours),
            days_of_week.clone(),
            None,
        ),
        crate::scheduler::ScheduleMode::Once { datetime } => (
            "once".into(),
            None,
            None,
            None,
            None,
            None,
            None,
            vec![],
            Some(datetime.clone()),
        ),
    }
}

/// Flatten session messages into timeline entries (host version, returns types::SessionTraceEntry).
pub fn flatten_trace(
    msgs: Vec<starriver_kernel::types::Message>,
) -> Vec<crate::types::SessionTraceEntry> {
    use starriver_kernel::types::ContentBlock;
    let mut entries = Vec::new();
    for m in msgs {
        let role_str = format!("{:?}", m.role).to_lowercase();
        for b in m.content {
            let e = match b {
                ContentBlock::Text { text } => crate::types::SessionTraceEntry {
                    kind: role_str.clone(),
                    text,
                    ..Default::default()
                },
                ContentBlock::Thinking { text } => crate::types::SessionTraceEntry {
                    kind: "thinking".into(),
                    text,
                    ..Default::default()
                },
                ContentBlock::ToolUse { name, input, .. } => crate::types::SessionTraceEntry {
                    kind: "tool_use".into(),
                    text: String::new(),
                    tool_name: Some(name),
                    tool_input: Some(serde_json::to_string(&input).unwrap_or_default()),
                    ..Default::default()
                },
                ContentBlock::ToolResult {
                    tool_use_id: _,
                    content,
                    is_error,
                } => crate::types::SessionTraceEntry {
                    kind: "tool_result".into(),
                    text: String::new(),
                    tool_output: Some(content),
                    tool_ok: Some(!is_error),
                    ..Default::default()
                },
            };
            entries.push(e);
        }
    }
    entries
}

/// Compress "🔧 name: arg" into a single-line label (escaping newlines) for history display reuse.
pub fn tool_call_label(name: &str, arg: &str) -> String {
    if arg.is_empty() {
        format!("🔧 {name}")
    } else {
        let flat = arg.replace('\n', "\\n").replace('\r', "\\r");
        format!("🔧 {name}: {flat}")
    }
}

/// Argument summary for a tool-call display line: the first string value
/// among the known keys, in order. Terminal-family tools expose input/shell
/// directly; when no single command field matches (terminal wait/read/kill/
/// signal), surface the FULL argument object so nothing is omitted (capped).
/// Terminal keys are gated on the name prefix so an unrelated tool's `id`
/// never leaks as noise.
pub fn tool_call_arg(name: &str, parsed: &Option<serde_json::Value>) -> String {
    let terminal = name.starts_with("terminal");
    let mut keys: Vec<&str> = vec!["file", "path", "command"];
    if terminal {
        keys.extend_from_slice(&["input", "shell"]);
    }
    if let Some(hit) = keys.iter().find_map(|key| {
        parsed
            .as_ref()
            .and_then(|value| value.get(*key))
            .and_then(|value| value.as_str())
    }) {
        if !hit.is_empty() {
            return hit.to_string();
        }
    }
    if terminal {
        if let Some(value) = parsed {
            let compact = value.to_string();
            let mut out: String = compact.chars().take(400).collect();
            if compact.chars().count() > 400 {
                out.push('…');
            }
            return out;
        }
    }
    String::new()
}

/// Slash command dispatch: when a task starts with `/skill`, inject the skill body and drop the command token.
pub fn apply_slash_dispatch(task: &str, skill: Option<&SkillProfile>) -> (String, Option<String>) {
    let Some(cmd) = task.split_whitespace().next() else {
        return (task.to_string(), None);
    };
    if !cmd.starts_with('/') {
        return (task.to_string(), None);
    }
    match skill {
        Some(s) => {
            let injection = format!("\n\n## Skill: {}\n{}", s.name, s.body);
            // Slice the rest off the trimmed task: `cmd` came from
            // split_whitespace, so indexing the raw task by cmd.len() would
            // eat leading-whitespace bytes as if they were part of the token.
            let rest = task.trim_start()[cmd.len()..].trim();
            let cleaned = if rest.is_empty() {
                format!("Use the {} skill to complete this task.", s.name)
            } else {
                rest.to_string()
            };
            (cleaned, Some(injection))
        }
        None => (task.to_string(), None),
    }
}

/// WI-7: expand `@path` file references in the task text into fenced code
/// blocks containing the file contents (workspace-rooted, budgeted).
///
/// Zero-false-positive policy: a token is rewritten only when it resolves to
/// an existing UTF-8 file inside the workspace root (canonical
/// containment); emails, unknown paths, escaping paths and binary files all
/// stay verbatim. Budgets: a single file over `max_file_bytes` is truncated
/// at a char boundary; once the cumulative `total_max_bytes` budget is
/// exhausted, remaining tokens stay verbatim behind an over-budget marker
/// note appended.
/// Conservative email-shape guard: `example.com` (label.label, no slash,
/// at least one dot inside the last two segments) never expands.
fn is_email_like(path: &str) -> bool {
    !path.contains('/')
        && path
            .rsplit_once('.')
            .is_some_and(|(label, _)| label.contains('.'))
}

pub fn expand_file_references(
    task: &str,
    cwd: Option<&std::path::Path>,
    max_file_bytes: usize,
    total_max_bytes: usize,
) -> String {
    let Some(cwd) = cwd else {
        return task.to_string();
    };
    if !task.contains('@') {
        return task.to_string();
    }
    let Ok(root) = cwd.canonicalize() else {
        return task.to_string();
    };
    let re = regex::Regex::new(r"@[^\s]+").unwrap();
    let mut remaining = total_max_bytes;
    let mut replacements: Vec<(std::ops::Range<usize>, String)> = Vec::new();
    for m in re.find_iter(task) {
        let range = m.range();
        let token = m.as_str();
        let path = &token[1..];
        if path.is_empty() {
            continue;
        }
        // Zero-false-positive contract: an `@` embedded mid-word (no
        // whitespace before it) is an email address, not a file mention —
        // it stays verbatim even when a same-named file exists.
        let at_word_start = range.start == 0
            || task[..range.start]
                .chars()
                .next_back()
                .is_some_and(|prev| prev.is_whitespace());
        if !at_word_start {
            continue;
        }
        if is_email_like(path) {
            continue;
        }
        let candidate = std::path::Path::new(path);
        let resolved = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            cwd.join(candidate)
        };
        // Canonical containment: existing file, inside the workspace root.
        let Ok(canonical) = resolved.canonicalize() else {
            continue;
        };
        if !canonical.is_file() || !canonical.starts_with(&root) {
            continue;
        }
        let Ok(bytes_len) = std::fs::metadata(&canonical).map(|meta| meta.len() as usize) else {
            continue;
        };
        if bytes_len > remaining {
            replacements.push((range, format!("{token} [未展开: 超出预算]")));
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&canonical) else {
            continue; // binary / non-UTF-8 stays verbatim
        };
        remaining = remaining.saturating_sub(bytes_len);
        let (body, note) = if bytes_len > max_file_bytes {
            let mut cut = max_file_bytes;
            while cut > 0 && !content.is_char_boundary(cut) {
                cut -= 1;
            }
            (
                &content[..cut],
                format!("\n[truncated: 共 {bytes_len} 字节]"),
            )
        } else {
            (content.as_str(), String::new())
        };
        // Grow the fence when the file itself contains triple backticks.
        let fence = if content.contains("```") {
            "````"
        } else {
            "```"
        };
        replacements.push((range, format!("{fence}path:{path}\n{body}{note}\n{fence}")));
    }
    if replacements.is_empty() {
        return task.to_string();
    }
    let mut result = task.to_string();
    for (range, text) in replacements.into_iter().rev() {
        result.replace_range(range, &text);
    }
    result
}

/// Title truncation (on character boundaries, to avoid UTF-8 out-of-bounds panic).
pub fn truncate_title(s: &str, max: usize) -> String {
    let trimmed = s.trim();
    // The title contract is a CHAR cap (the title system prompt tells the
    // model "at most 40 characters") — a byte cap cut valid 40-char CJK
    // titles down to 13 chars.
    if trimmed.chars().count() > max {
        let head: String = trimmed.chars().take(max).collect();
        format!("{head}...")
    } else {
        trimmed.to_string()
    }
}

/// Relative time formatting ("just now"/"5m ago"/"3h ago"/"2d ago").
pub fn format_time(ts: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let diff = now - ts;
    if diff < 60 {
        "just now".into()
    } else if diff < 3600 {
        format!("{}m ago", diff / 60)
    } else if diff < 86400 {
        format!("{}h ago", diff / 3600)
    } else {
        format!("{}d ago", diff / 86400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_file(dir: &std::path::Path, rel: &str, content: &str) {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn file_refs_expand_hits_into_fenced_blocks() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/main.rs", "fn main() {}");
        let out = expand_file_references(
            "look at @src/main.rs please",
            Some(dir.path()),
            32 * 1024,
            96 * 1024,
        );
        assert!(out.contains("```path:src/main.rs"), "{out}");
        assert!(out.contains("fn main() {}"), "{out}");
        assert!(!out.contains('@'), "{out}");
    }

    #[test]
    fn file_refs_misses_stay_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let task = "see @nope.txt and @missing/dir.txt";
        let out = expand_file_references(task, Some(dir.path()), 1024, 4096);
        assert_eq!(out, task);
    }

    #[test]
    fn emails_are_never_touched() {
        let dir = tempfile::tempdir().unwrap();
        let task = "contact me at a@b.com anytime";
        let out = expand_file_references(task, Some(dir.path()), 1024, 4096);
        assert_eq!(out, task);
    }

    #[test]
    fn escaping_paths_stay_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/in.rs", "inside");
        let outside = tempfile::tempdir().unwrap();
        write_file(outside.path(), "out.txt", "outside");
        let task = format!(
            "@../{}/out.txt",
            outside.path().file_name().unwrap().to_str().unwrap()
        );
        let out = expand_file_references(&task, Some(dir.path()), 1024, 4096);
        assert_eq!(out, task, "workspace escape must not expand");
    }

    #[test]
    fn single_file_budget_truncates_at_char_boundary() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "big.txt", &"x".repeat(500));
        let out = expand_file_references("read @big.txt", Some(dir.path()), 100, 4096);
        assert!(out.contains("[truncated: 共 500 字节]"), "{out}");
        assert!(out.contains("```path:big.txt"));
    }

    #[test]
    fn total_budget_leaves_later_refs_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let body = "y".repeat(600);
        write_file(dir.path(), "a.txt", &body);
        write_file(dir.path(), "b.txt", &body);
        let out = expand_file_references("first @a.txt then @b.txt", Some(dir.path()), 1024, 700);
        assert!(out.contains("```path:a.txt"), "{out}");
        assert!(out.contains("@b.txt [未展开: 超出预算]"), "{out}");
    }

    #[test]
    fn fence_grows_when_file_contains_triple_backticks() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "md.txt", "before\n```\ninner fence\n```\nafter");
        let out = expand_file_references("read @md.txt", Some(dir.path()), 4096, 8192);
        assert!(out.contains("````path:md.txt"), "fence must grow: {out}");
        assert!(out.ends_with("````"), "closing fence matches: {out}");
        assert!(out.contains("inner fence"), "{out}");
    }

    // ── flatten_mode ──

    #[test]
    fn flatten_mode_periodic_carries_period_name_and_fields() {
        let mode = crate::scheduler::ScheduleMode::Periodic {
            period: crate::scheduler::Period::Weekly,
            time: "09:00".into(),
            day_of_week: Some(1),
            day_of_month: Some(15),
            month: Some(3),
        };
        let (kind, period, time, dow, dom, month, hours, days, once) = flatten_mode(&mode);
        assert_eq!(kind, "periodic");
        assert_eq!(period.as_deref(), Some("weekly"));
        assert_eq!(time.as_deref(), Some("09:00"));
        assert_eq!(dow, Some(1));
        assert_eq!(dom, Some(15));
        assert_eq!(month, Some(3));
        assert_eq!(hours, None);
        assert!(days.is_empty());
        assert_eq!(once, None);
    }

    #[test]
    fn flatten_mode_interval_and_once() {
        let (kind, period, time, _, _, _, hours, days, once) =
            flatten_mode(&crate::scheduler::ScheduleMode::Interval {
                hours: 4,
                days_of_week: vec![0, 3],
            });
        assert_eq!(kind, "interval");
        assert_eq!(period, None);
        assert_eq!(time, None);
        assert_eq!(hours, Some(4));
        assert_eq!(days, vec![0, 3]);
        assert_eq!(once, None);

        let (kind, period, time, _, _, _, hours, days, once) =
            flatten_mode(&crate::scheduler::ScheduleMode::Once {
                datetime: "2026-01-02T09:00:00Z".into(),
            });
        assert_eq!(kind, "once");
        assert_eq!(period, None);
        assert_eq!(time, None);
        assert_eq!(hours, None);
        assert!(days.is_empty());
        assert_eq!(once.as_deref(), Some("2026-01-02T09:00:00Z"));
    }

    // ── format_time ──

    #[test]
    fn format_time_buckets_relative_ages() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert_eq!(format_time(now), "just now");
        assert_eq!(format_time(now - 30), "just now");
        assert_eq!(format_time(now - 5 * 60), "5m ago");
        assert_eq!(format_time(now - 3 * 3600), "3h ago");
        assert_eq!(format_time(now - 2 * 86400), "2d ago");
        // A future timestamp falls into the "< 60s" bucket (diff is negative).
        assert_eq!(format_time(now + 10), "just now");
    }

    // ── apply_slash_dispatch ──

    fn book_skill() -> SkillProfile {
        SkillProfile {
            name: "write-book".into(),
            version: None,
            description: None,
            disable_model_invocation: false,
            user_invocable: false,
            command: Some("/write-book".into()),
            command_aliases: vec![],
            always_on: false,
            conditions: None,
            tools: vec![],
            allowed_tools: None,
            depends_on: vec![],
            exclusive: false,
            hooks: None,
            min_platform_version: None,
            body: "1. ask\n2. write\n".into(),
        }
    }

    #[test]
    fn slash_dispatch_leading_whitespace_keeps_rest_intact() {
        // Regression: cmd comes from split_whitespace, but the rest used to be
        // sliced off the RAW task, so leading whitespace shifted the cut —
        // "  /write-book rest" produced the task "ok rest"-style garbage.
        let (task, injection) =
            apply_slash_dispatch("  /write-book 写一本关于AI的书", Some(&book_skill()));
        assert_eq!(task, "写一本关于AI的书");
        assert!(injection.unwrap().contains("## Skill: write-book"));

        // Full-width space (common in CJK input) is whitespace too.
        let (task, _) = apply_slash_dispatch("　/write-book 写书", Some(&book_skill()));
        assert_eq!(task, "写书");

        // Leading whitespace without a slash command stays verbatim.
        let (task, injection) = apply_slash_dispatch("  plain task", Some(&book_skill()));
        assert_eq!(task, "  plain task");
        assert!(injection.is_none());
    }

    #[test]
    fn slash_dispatch_leading_whitespace_empty_args_uses_skill_name() {
        let (task, injection) = apply_slash_dispatch("   /write-book", Some(&book_skill()));
        assert_eq!(task, "Use the write-book skill to complete this task.");
        assert!(injection.is_some());
    }
}


// ── Round-6 regressions (BUG-99 / BUG-101) ──
#[cfg(test)]
mod round6_tests {
    use super::*;

    #[test]
    fn truncate_title_caps_characters_not_bytes() {
        let cjk: String = "\u{68a6}".repeat(40); // 40 chars, 120 bytes
        let out = truncate_title(&cjk, 40);
        assert_eq!(out.chars().count(), 40, "a 40-char CJK title survives");
        assert!(!out.ends_with("..."));
        let over: String = "\u{68a6}".repeat(50);
        let cut = truncate_title(&over, 40);
        assert!(cut.chars().count() <= 43);
        assert!(cut.ends_with("..."));
    }

    #[test]
    fn email_stays_verbatim_even_with_a_same_named_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("example.com"), "oops").unwrap();
        let out = expand_file_references(
            "mail me at a@example.com",
            Some(dir.path()),
            1024,
            4096,
        );
        assert_eq!(
            out, "mail me at a@example.com",
            "zero-false-positive contract holds even when the domain names an existing file"
        );
    }
}
