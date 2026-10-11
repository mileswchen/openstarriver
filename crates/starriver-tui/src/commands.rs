use std::path::PathBuf;

use ratatui::text::Line;

/// A parsed slash command; `None` means an ordinary prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Help,
    New,
    Sessions,
    Projects,
    Project(Option<String>),
    Model(Option<String>, Option<String>),
    Open(PathBuf),
    Compact,
    Diff,
    Status,
    Usage,
    Init,
    Export,
    Lang(Option<String>),
    /// Backtrack to a historical message (fork a new thread before the selected user message).
    Backtrack,
    /// Delete the current session (executed after a second confirmation).
    Delete,
    /// Toggle session-level worktree isolation (when on, new sessions auto-create a worktree).
    Worktree,
    /// Name the current session (Some sets an override title, None clears back to task-derived).
    Name(Option<String>),
    Memory(String),
    /// Soft-archive the current session (restorable via /unarchive).
    Archive,
    /// Restore a session from the archive list (picker).
    Unarchive,
    /// Review the given files (review mode + read/grep whitelist + prefilled prompt).
    Review(String),
    /// Generate unit tests for a target (prefilled prompt template).
    Test(String),
    /// Explore the codebase (Plan mode + prefilled prompt template).
    Explore(String),
    /// Sandbox preset picker: no arg opens the picker; a known preset id
    /// (standard|plan|ptc|goal) switches directly.
    Preset(Option<String>),
    Quit,
    Unknown(String),
}

/// Input line → command; returns `None` (ordinary prompt) when it does not start with `/` (or `\`).
/// Strip one layer of matching surrounding quotes from a path argument
/// (`/open "/tmp/foo bar"` — split_whitespace keeps the quote characters).
fn strip_matching_quotes(s: &str) -> &str {
    let bytes = s.as_bytes();
    if bytes.len() >= 2 {
        let (first, last) = (bytes[0], bytes[bytes.len() - 1]);
        if first == last && (first == b'"' || first == b'\'') {
            return &s[1..s.len() - 1];
        }
    }
    s
}

pub fn parse(input: &str) -> Option<Command> {
    let trimmed = input.trim();
    if !trimmed.starts_with('/') && !trimmed.starts_with('\\') {
        return None;
    }
    // A `\` prefix is normalized to `/`; the two spellings are equivalent.
    let normalized = if trimmed.starts_with('\\') {
        format!("/{}", &trimmed[1..])
    } else {
        trimmed.to_string()
    };
    let mut parts = normalized.split_whitespace();
    let name = parts.next().unwrap_or_default().to_lowercase();
    let rest: Vec<String> = parts.map(str::to_string).collect();
    let arg = rest.join(" ");
    let cmd = match name.as_str() {
        "/help" => Command::Help,
        "/new" => Command::New,
        "/sessions" | "/resume" | "/continue" => Command::Sessions,
        "/projects" => Command::Projects,
        "/project" => Command::Project(if arg.is_empty() { None } else { Some(arg) }),
        "/model" => {
            let mut tokens = arg.split_whitespace();
            Command::Model(
                tokens.next().map(str::to_string),
                tokens.next().map(str::to_string),
            )
        }
        "/open" => Command::Open(PathBuf::from(strip_matching_quotes(&arg))),
        "/compact" => Command::Compact,
        "/diff" => Command::Diff,
        "/status" => Command::Status,
        "/usage" => Command::Usage,
        "/init" => Command::Init,
        "/export" => Command::Export,
        "/lang" => Command::Lang(if arg.is_empty() { None } else { Some(arg) }),
        "/backtrack" => Command::Backtrack,
        "/delete" => Command::Delete,
        "/worktree" => Command::Worktree,
        "/name" => Command::Name(if arg.is_empty() { None } else { Some(arg) }),
        "/memory" => Command::Memory(arg.to_string()),
        "/archive" => Command::Archive,
        "/unarchive" => Command::Unarchive,
        "/review" => Command::Review(arg),
        "/test" => Command::Test(arg),
        "/explore" => Command::Explore(arg),
        "/preset" | "/sandbox" => Command::Preset(if arg.is_empty() { None } else { Some(arg) }),
        // BUG-71: the help/hint docs advertise these as mode switches — the
        // parser must honor them (they are aliases of `/preset <mode>`).
        "/plan" => Command::Preset(Some("plan".into())),
        "/build" => Command::Preset(Some("build".into())),
        "/auto" => Command::Preset(Some("auto".into())),
        "/exit" => Command::Quit,
        other => Command::Unknown(other.to_string()),
    };
    Some(cmd)
}

/// UI language is unified in [`crate::i18n`]; this re-export keeps the old reference (`commands::Lang`) working.
pub use crate::i18n::{current_lang, Lang};

/// Command docs (single source of truth: shared by `/help` and the input-box command hints), with bilingual descriptions.
struct CmdDoc {
    name: &'static str,
    zh: &'static str,
    en: &'static str,
}

const CMD_DOCS: &[CmdDoc] = &[
    CmdDoc {
        name: "help",
        zh: "显示本帮助（`\\` 前缀等价于 `/`）",
        en: "Show this help (`\\` prefix equals `/`)",
    },
    CmdDoc {
        name: "new",
        zh: "开新会话（清空视图）",
        en: "Start a new session (clear view)",
    },
    CmdDoc {
        name: "sessions",
        zh: "列出会话并选择续聊（别名 /resume /continue）",
        en: "List sessions to continue (aliases: /resume /continue)",
    },
    CmdDoc {
        name: "projects",
        zh: "列出项目",
        en: "List projects",
    },
    CmdDoc {
        name: "project [id|name]",
        zh: "查看/切换当前项目",
        en: "Show/switch current project",
    },
    CmdDoc {
        name: "model [provider] [model]",
        zh: "切换模型（无参进入选择器）",
        en: "Switch model (no args opens picker)",
    },
    CmdDoc {
        name: "plan",
        zh: "切到 Plan 模式（只读规划，不写代码）",
        en: "Switch to Plan mode (read-only planning)",
    },
    CmdDoc {
        name: "build",
        zh: "切到 Build 模式（全工具授权）",
        en: "Switch to Build mode (full tools)",
    },
    CmdDoc {
        name: "auto",
        zh: "切到 Auto 模式（自动裁决审批）",
        en: "Switch to Auto mode (auto approval)",
    },
    CmdDoc {
        name: "open <path>",
        zh: "切换工作目录/项目（全新会话）",
        en: "Switch working dir/project (fresh session)",
    },
    CmdDoc {
        name: "compact",
        zh: "压缩当前会话上下文（保留锚点与最近轮）",
        en: "Compact current session context (keep anchors + recent turns)",
    },
    CmdDoc {
        name: "diff",
        zh: "显示当前项目工作区 git diff",
        en: "Show git diff of the current project workspace",
    },
    CmdDoc {
        name: "status",
        zh: "显示当前状态（模式/模型/项目/会话）",
        en: "Show current status (mode/model/project/session)",
    },
    CmdDoc {
        name: "usage",
        zh: "显示当前会话 token 用量与消息数",
        en: "Show current session token usage & message count",
    },
    CmdDoc {
        name: "init",
        zh: "为当前项目生成 AGENTS.md",
        en: "Generate AGENTS.md for the current project",
    },
    CmdDoc {
        name: "export",
        zh: "导出当前会话为 Markdown 文件",
        en: "Export current session to a Markdown file",
    },
    CmdDoc {
        name: "lang [zh|en]",
        zh: "切换界面语言（无参则切换）",
        en: "Switch UI language (toggle if no arg)",
    },
    CmdDoc {
        name: "backtrack",
        zh: "回溯到历史消息（分叉新线程，原会话不动）",
        en: "Backtrack to a past message (fork a new thread; source stays)",
    },
    CmdDoc {
        name: "delete",
        zh: "删除当前会话（二次确认；子分支自动提升为根）",
        en: "Delete current session (confirm; child branches become roots)",
    },
    CmdDoc {
        name: "worktree",
        zh: "开关会话级 worktree 隔离（新会话自动建 worktree）",
        en: "Toggle session worktree isolation (new sessions get a worktree)",
    },
    CmdDoc {
        name: "name <title>",
        zh: "命名当前会话（自定义标题）",
        en: "Name the current session (custom title)",
    },
    CmdDoc {
        name: "archive",
        zh: "软归档当前会话（可 /unarchive 恢复）",
        en: "Archive the current session (restore with /unarchive)",
    },
    CmdDoc {
        name: "unarchive",
        zh: "从归档列表恢复会话",
        en: "Restore a session from the archive",
    },
    CmdDoc {
        name: "review <files>",
        zh: "审查指定文件（review 模式 + 白名单）",
        en: "Review specified files (review mode + whitelist)",
    },
    CmdDoc {
        name: "test <target>",
        zh: "为目标生成单测（预填 prompt）",
        en: "Write unit tests for a target (prefilled prompt)",
    },
    CmdDoc {
        name: "explore <query>",
        zh: "探索代码库（Plan 模式 + 预填 prompt）",
        en: "Explore the codebase (Plan mode + prefilled prompt)",
    },
    CmdDoc {
        name: "preset [name]",
        zh: "选择沙箱预设（无参进入选择器；plan=只读规划）",
        en: "Pick sandbox preset (no args opens picker; plan=read-only)",
    },
    CmdDoc {
        name: "exit",
        zh: "退出",
        en: "Quit",
    },
];

/// Command name + brief description in the given language (name has no prefix; may include arg placeholders).
pub fn command_docs(lang: Lang) -> Vec<(String, String)> {
    CMD_DOCS
        .iter()
        .map(|d| {
            let desc = match lang {
                Lang::Zh => d.zh,
                Lang::En => d.en,
            };
            (d.name.to_string(), desc.to_string())
        })
        .collect()
}

/// The lines shown by `/help` (in the given language).
pub fn help_lines(lang: Lang) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = command_docs(lang)
        .iter()
        .map(|(name, desc)| Line::from(format!("{:<28}{}", format!("/{name}"), desc)))
        .collect();
    lines.push(Line::from(match lang {
        Lang::Zh => "普通输入即 prompt；Running 中 Ctrl+C 取消任务。",
        Lang::En => "Plain input is a prompt; Ctrl+C cancels while running.",
    }));
    lines
}

/// Command hint docs: when input starts with `/` or `\`, return `(prefix, displayed command list)`, otherwise `None`.
///
/// Prefix matching shows only matching items; no matches falls back to the full list (to avoid an empty area).
/// Height, rendering, and up/down selection share this function so the "displayed list" equals the "selectable list".
pub fn hint_docs(input: &str, lang: Lang) -> Option<(String, Vec<(String, String)>)> {
    let t = input.trim();
    if t.is_empty() {
        return None;
    }
    let token = t.split_whitespace().next()?;
    let (prefix, name) = if let Some(stripped) = token.strip_prefix('\\') {
        ("\\".to_string(), stripped.to_string())
    } else if let Some(stripped) = token.strip_prefix('/') {
        ("/".to_string(), stripped.to_string())
    } else {
        return None;
    };
    let docs = command_docs(lang);
    let needle = name.to_lowercase();
    let matching: Vec<(String, String)> = if needle.is_empty() {
        docs.clone()
    } else {
        // parse() lowercases the command token, so the hint filter must too —
        // /PLA parses to /plan but a case-sensitive filter showed no hint.
        docs.iter()
            .filter(|(n, _)| {
                n.split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_lowercase()
                    .starts_with(&needle)
            })
            .cloned()
            .collect()
    };
    let shown = if matching.is_empty() { docs } else { matching };
    Some((prefix, shown))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_prompt_is_none() {
        assert_eq!(parse("写个测试"), None);
        assert_eq!(parse("  cargo build "), None);
    }

    #[test]
    fn sessions_aliases_normalize() {
        assert_eq!(parse("/sessions"), Some(Command::Sessions));
        assert_eq!(parse("/resume"), Some(Command::Sessions));
        assert_eq!(parse("/continue"), Some(Command::Sessions));
    }

    #[test]
    fn project_with_and_without_arg() {
        assert_eq!(parse("/project"), Some(Command::Project(None)));
        assert_eq!(
            parse("/project alpha"),
            Some(Command::Project(Some("alpha".into())))
        );
    }

    #[test]
    fn open_keeps_spaces_in_path() {
        assert_eq!(
            parse("/open /tmp/foo bar"),
            Some(Command::Open(PathBuf::from("/tmp/foo bar")))
        );
    }

    #[test]
    fn unknown_command_is_preserved() {
        assert_eq!(parse("/bogus"), Some(Command::Unknown("/bogus".into())));
    }

    #[test]
    fn bare_slash_is_unknown() {
        assert_eq!(parse("/"), Some(Command::Unknown("/".into())));
    }

    #[test]
    fn backslash_prefix_normalizes_to_slash() {
        assert_eq!(parse("\\help"), Some(Command::Help));
        assert_eq!(
            parse("\\model deepseek"),
            Some(Command::Model(Some("deepseek".into()), None))
        );
        assert_eq!(parse("\\sessions"), Some(Command::Sessions));
        assert_eq!(parse("\\bogus"), Some(Command::Unknown("/bogus".into())));
        assert_eq!(parse("\\"), Some(Command::Unknown("/".into())));
    }

    #[test]
    fn uppercase_command_normalized() {
        assert_eq!(parse("/SESSIONS"), Some(Command::Sessions));
        assert_eq!(parse("/New"), Some(Command::New));
    }

    #[test]
    fn surrounding_whitespace_trimmed() {
        assert_eq!(parse("  /exit  "), Some(Command::Quit));
    }

    #[test]
    fn unknown_keeps_first_token_only() {
        assert_eq!(parse("/bogus x y"), Some(Command::Unknown("/bogus".into())));
    }

    #[test]
    fn new_commands_parse() {
        assert_eq!(parse("/compact"), Some(Command::Compact));
        assert_eq!(parse("/diff"), Some(Command::Diff));
        assert_eq!(parse("/status"), Some(Command::Status));
        assert_eq!(parse("/usage"), Some(Command::Usage));
        assert_eq!(parse("/init"), Some(Command::Init));
        assert_eq!(parse("/export"), Some(Command::Export));
        // Backslash prefix is equivalent
        assert_eq!(parse("\\compact"), Some(Command::Compact));
    }

    #[test]
    fn thread_commands_parse() {
        assert_eq!(parse("/backtrack"), Some(Command::Backtrack));
        assert_eq!(parse("\\backtrack"), Some(Command::Backtrack));
        assert_eq!(parse("/delete"), Some(Command::Delete));
        assert_eq!(parse("\\delete"), Some(Command::Delete));
        assert_eq!(parse("/worktree"), Some(Command::Worktree));
        assert_eq!(parse("\\worktree"), Some(Command::Worktree));
        // Extra args are ignored
        assert_eq!(parse("/delete now"), Some(Command::Delete));
    }

    #[test]
    fn name_archive_review_commands_parse() {
        assert_eq!(parse("/name"), Some(Command::Name(None)));
        assert_eq!(
            parse("/name 我的命名"),
            Some(Command::Name(Some("我的命名".into())))
        );
        assert_eq!(parse("/archive"), Some(Command::Archive));
        assert_eq!(parse("/unarchive"), Some(Command::Unarchive));
        assert_eq!(
            parse("/review src/a.rs src/b.rs"),
            Some(Command::Review("src/a.rs src/b.rs".into()))
        );
        assert_eq!(parse("/test crypto"), Some(Command::Test("crypto".into())));
        assert_eq!(
            parse("/explore payment flow"),
            Some(Command::Explore("payment flow".into()))
        );
        // Backslash is equivalent
        assert_eq!(parse("\\name foo"), Some(Command::Name(Some("foo".into()))));
        assert_eq!(parse("\\archive"), Some(Command::Archive));
    }

    #[test]
    fn name_archive_review_appear_in_help_and_hint() {
        let zh = command_docs(Lang::Zh);
        let names: Vec<&str> = zh.iter().map(|(n, _)| n.as_str()).collect();
        for n in [
            "name <title>",
            "archive",
            "unarchive",
            "review <files>",
            "test <target>",
            "explore <query>",
        ] {
            assert!(names.contains(&n), "missing {n} in docs");
        }
        let (_, shown) = hint_docs("/arc", Lang::Zh).unwrap();
        assert!(shown.iter().any(|(n, _)| n == "archive"));
        let (_, shown) = hint_docs("/una", Lang::Zh).unwrap();
        assert!(shown.iter().any(|(n, _)| n == "unarchive"));
        let (_, shown) = hint_docs("/nam", Lang::Zh).unwrap();
        assert!(shown.iter().any(|(n, _)| n.starts_with("name")));
    }
    #[test]
    fn mode_commands_appear_in_help_and_hint() {
        let zh = command_docs(Lang::Zh);
        let names: Vec<&str> = zh.iter().map(|(n, _)| n.as_str()).collect();
        for n in ["plan", "build", "auto"] {
            assert!(names.contains(&n), "missing {n} in docs");
        }
        // hint: prefix filtering hits the new command (/b also hits build)
        let (_, shown) = hint_docs("/plan", Lang::Zh).unwrap();
        assert!(shown.iter().any(|(n, _)| n == "plan"));
        let (_, shown) = hint_docs("/auto", Lang::Zh).unwrap();
        assert_eq!(shown[0].0, "auto");
    }
    #[test]
    fn mode_hint_prefix_filters() {
        // Single-letter prefix hits: /b → build + backtrack, /a → auto
        let (_, shown) = hint_docs("/b", Lang::Zh).unwrap();
        let names: Vec<&str> = shown.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"build"));
        assert!(names.contains(&"backtrack"));
        let (_, shown) = hint_docs("/bu", Lang::Zh).unwrap();
        assert_eq!(shown[0].0, "build");
        let (_, shown) = hint_docs("/a", Lang::Zh).unwrap();
        let names: Vec<&str> = shown.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"auto"));
        assert!(names.contains(&"archive"));
        // /p hits plan + project(s) (project has an arg placeholder), not build/auto
        let (_, shown) = hint_docs("/p", Lang::Zh).unwrap();
        let names: Vec<&str> = shown.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"plan"));
        assert!(names.iter().any(|n| n.starts_with("project")));
        assert!(names.contains(&"projects"));
        assert!(!names.contains(&"build"));
        assert!(!names.contains(&"auto"));
        // Backslash is equivalent
        let (_, shown) = hint_docs("\\pl", Lang::Zh).unwrap();
        assert_eq!(shown[0].0, "plan");
    }

    #[test]
    fn mode_docs_localized_and_in_help() {
        let zh = command_docs(Lang::Zh);
        let en = command_docs(Lang::En);
        let zh_mode = |n: &str| zh.iter().find(|(nn, _)| nn == n).unwrap().1.clone();
        let en_mode = |n: &str| en.iter().find(|(nn, _)| nn == n).unwrap().1.clone();
        for n in ["plan", "build", "auto"] {
            assert!(zh_mode(n).contains("模式"), "zh {n}: {}", zh_mode(n));
            assert!(
                en_mode(n).to_lowercase().contains("mode"),
                "en {n}: {}",
                en_mode(n)
            );
        }
        // Chinese and English command names are identical (language only changes descriptions)
        let zh_names: Vec<&str> = zh.iter().map(|(n, _)| n.as_str()).collect();
        let en_names: Vec<&str> = en.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(zh_names, en_names);
        // /help lists the mode commands
        let lines = help_lines(Lang::Zh);
        for n in ["/plan", "/build", "/auto"] {
            assert!(
                lines.iter().any(|l| l.to_string().contains(n)),
                "help missing {n}"
            );
        }
    }

    // ── Round-10 regressions ──
    #[test]
    fn hint_filter_is_case_insensitive_like_the_parser() {
        let (_, shown) = hint_docs("/PLA", Lang::Zh).unwrap();
        assert!(shown.iter().any(|(n, _)| n == "plan"), "/PLA must hint plan");
    }

    #[test]
    fn open_strips_one_layer_of_matching_quotes() {
        assert_eq!(
            parse("/open \"/tmp/foo bar\""),
            Some(Command::Open(PathBuf::from("/tmp/foo bar")))
        );
        assert_eq!(
            parse("/open '/tmp/a b'"),
            Some(Command::Open(PathBuf::from("/tmp/a b")))
        );
        // Unpaired quotes stay literal.
        assert_eq!(
            parse("/open /tmp/\"weird"),
            Some(Command::Open(PathBuf::from("/tmp/\"weird")))
        );
    }

    #[test]
    fn lang_command_variants() {
        assert_eq!(parse("/lang"), Some(Command::Lang(None)));
        assert_eq!(parse("/lang zh"), Some(Command::Lang(Some("zh".into()))));
        assert_eq!(parse("/lang en"), Some(Command::Lang(Some("en".into()))));
        assert_eq!(parse("\\lang en"), Some(Command::Lang(Some("en".into()))));
    }

    #[test]
    fn new_commands_appear_in_help_and_hint() {
        let zh = command_docs(Lang::Zh);
        let names: Vec<&str> = zh.iter().map(|(n, _)| n.as_str()).collect();
        for n in [
            "compact",
            "diff",
            "status",
            "usage",
            "init",
            "export",
            "lang [zh|en]",
            "backtrack",
            "delete",
            "worktree",
        ] {
            assert!(names.contains(&n), "missing {n} in docs");
        }
        // hint: prefix filtering hits the new command
        let (_, shown) = hint_docs("/co", Lang::Zh).unwrap();
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].0, "compact");
        let (_, shown) = hint_docs("/e", Lang::Zh).unwrap();
        assert!(shown.iter().any(|(n, _)| n == "export"));
        assert!(shown.iter().any(|(n, _)| n.starts_with("explore")));
        let (_, shown) = hint_docs("\\lang", Lang::Zh).unwrap();
        assert_eq!(shown[0].0, "lang [zh|en]");
        // /b hits both build + backtrack (prefix match)
        let (_, shown) = hint_docs("/b", Lang::Zh).unwrap();
        let names: Vec<&str> = shown.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"build"));
        assert!(names.contains(&"backtrack"));
    }

    #[test]
    fn new_command_docs_localized() {
        let zh = command_docs(Lang::Zh);
        let en = command_docs(Lang::En);
        assert_eq!(zh.len(), en.len());
        assert!(zh.iter().any(|(n, d)| n == "compact" && d.contains("压缩")));
        assert!(en
            .iter()
            .any(|(n, d)| n == "compact" && d.contains("Compact")));
        assert!(zh
            .iter()
            .any(|(n, d)| n == "lang [zh|en]" && d.contains("语言")));
        assert!(en
            .iter()
            .any(|(n, d)| n == "lang [zh|en]" && d.contains("language")));
    }

    #[test]
    fn project_blank_arg_is_none() {
        assert_eq!(parse("/project   "), Some(Command::Project(None)));
    }

    #[test]
    fn model_variants() {
        assert_eq!(parse("/model"), Some(Command::Model(None, None)));
        assert_eq!(
            parse("/model deepseek"),
            Some(Command::Model(Some("deepseek".into()), None))
        );
        assert_eq!(
            parse("/model deepseek deepseek-v4-pro"),
            Some(Command::Model(
                Some("deepseek".into()),
                Some("deepseek-v4-pro".into())
            ))
        );
        // Extra args are ignored
        assert_eq!(
            parse("/model deepseek deepseek-v4-pro extra"),
            Some(Command::Model(
                Some("deepseek".into()),
                Some("deepseek-v4-pro".into())
            ))
        );
    }

    #[test]
    fn help_lists_commands() {
        let lines = help_lines(Lang::Zh);
        assert!(lines.len() >= 6);
        assert!(lines.iter().any(|l| l.to_string().contains("/sessions")));
    }

    #[test]
    fn command_docs_localized() {
        let zh = command_docs(Lang::Zh);
        let en = command_docs(Lang::En);
        assert_eq!(zh.len(), en.len());
        assert!(zh
            .iter()
            .any(|(n, d)| n == "help" && d.contains("显示本帮助")));
        assert!(en
            .iter()
            .any(|(n, d)| n == "help" && d.contains("Show this help")));
        // Command names are language-independent
        let zh_names: Vec<&str> = zh.iter().map(|(n, _)| n.as_str()).collect();
        let en_names: Vec<&str> = en.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(zh_names, en_names);
    }

    #[test]
    fn current_lang_respects_starriver_lang() {
        std::env::set_var("STARRIVER_LANG", "en-US");
        assert_eq!(current_lang(), Lang::En);
        std::env::set_var("STARRIVER_LANG", "zh-CN");
        assert_eq!(current_lang(), Lang::Zh);
        std::env::remove_var("STARRIVER_LANG");
    }
}

// ── BUG-71 regression (round 5): documented commands must parse ──
#[cfg(test)]
mod round5_tests {
    use super::*;

    /// CMD_DOCS lists plan/build/auto (and hint_docs surfaces them for
    /// autocomplete) — the parser must not answer "unknown command".
    #[test]
    fn documented_mode_commands_parse_to_preset_switches() {
        assert_eq!(parse("/plan"), Some(Command::Preset(Some("plan".into()))));
        assert_eq!(parse("/build"), Some(Command::Preset(Some("build".into()))));
        assert_eq!(parse("/auto"), Some(Command::Preset(Some("auto".into()))));
        // The generic preset path is untouched.
        assert_eq!(
            parse("/preset plan"),
            Some(Command::Preset(Some("plan".into())))
        );
    }
}
