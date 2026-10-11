// Pure-function test migration: moved in together with the starriver-desktop → starriver-host extraction
// to keep regression coverage for functions moved into helpers/admin/lib.
use crate::admin::{build_skill_md, set_always_on_in_skill_md};
use crate::helpers::{
    apply_slash_dispatch, builtin_provider_presets, flatten_trace, tool_call_label, truncate_title,
};
use crate::runtime::HostRuntime;
use crate::types::ModelProvider;
use crate::{resolve_agent_model_key, resolve_project_dir, split_model_key};

// ── flatten_trace tests ──

#[test]
fn flatten_trace_maps_every_content_block_kind() {
    use starriver_kernel::types::{ContentBlock, Message, Role};
    let msgs = vec![
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "hello".into(),
            }],
            meta: None,
        },
        Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    text: "let me think".into(),
                },
                ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "search".into(),
                    input: serde_json::json!({"q":"x"}),
                },
                ContentBlock::Text {
                    text: "done".into(),
                },
            ],
            meta: None,
        },
        Message {
            role: Role::Tool,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: "result".into(),
                is_error: false,
            }],
            meta: None,
        },
    ];
    let entries = flatten_trace(msgs);
    assert_eq!(entries.len(), 5);

    assert_eq!(entries[0].kind, "user");
    assert_eq!(entries[0].text, "hello");

    assert_eq!(entries[1].kind, "thinking");
    assert_eq!(entries[1].text, "let me think");

    assert_eq!(entries[2].kind, "tool_use");
    assert_eq!(entries[2].tool_name.as_deref(), Some("search"));
    assert_eq!(entries[2].tool_input.as_deref(), Some("{\"q\":\"x\"}"));

    assert_eq!(entries[3].kind, "assistant");
    assert_eq!(entries[3].text, "done");

    assert_eq!(entries[4].kind, "tool_result");
    assert_eq!(entries[4].tool_output.as_deref(), Some("result"));
    assert_eq!(entries[4].tool_ok, Some(true));
}

#[test]
fn flatten_trace_marks_tool_error() {
    use starriver_kernel::types::{ContentBlock, Message, Role};
    let msgs = vec![Message {
        role: Role::Tool,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: "t1".into(),
            content: "boom".into(),
            is_error: true,
        }],
        meta: None,
    }];
    let entries = flatten_trace(msgs);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].kind, "tool_result");
    assert_eq!(entries[0].tool_ok, Some(false));
    assert_eq!(entries[0].tool_output.as_deref(), Some("boom"));
}

#[test]
fn flatten_trace_empty_for_no_messages() {
    assert!(flatten_trace(vec![]).is_empty());
}

#[test]
fn tool_call_label_flattens_multiline_args() {
    // A multiline bash command must stay on ONE line (escaped) so the frontend
    // renders it as a single bash code block instead of splitting into prose.
    assert_eq!(tool_call_label("bash", ""), "🔧 bash");
    assert_eq!(tool_call_label("read", "a.txt"), "🔧 read: a.txt");
    assert_eq!(
        tool_call_label("bash", "# 注释\nls -la"),
        "🔧 bash: # 注释\\nls -la"
    );
    // No real newline may survive into the label.
    let label = tool_call_label("bash", "a\r\nb");
    assert!(!label.contains('\n') && !label.contains('\r'));
    assert_eq!(label, "🔧 bash: a\\r\\nb");
}

#[test]
fn tool_call_arg_exposes_terminal_params() {
    // terminal-family params must reach the history line: input/shell pick
    // directly; wait/read/kill/signal surface the FULL argument object.
    let write = Some(serde_json::json!({ "id": "t-1", "input": "ls -la\n" }));
    assert_eq!(
        crate::helpers::tool_call_arg("terminal_write", &write),
        "ls -la\n"
    );
    let start = Some(serde_json::json!({ "shell": "bash" }));
    assert_eq!(crate::helpers::tool_call_arg("terminal_start", &start), "bash");
    let wait = Some(serde_json::json!({ "id": "t-1", "timeoutMs": 60000 }));
    assert_eq!(
        crate::helpers::tool_call_arg("terminal_wait", &wait),
        r#"{"id":"t-1","timeoutMs":60000}"#
    );
    let kill = Some(serde_json::json!({ "id": "t-9" }));
    assert_eq!(crate::helpers::tool_call_arg("terminal_kill", &kill), r#"{"id":"t-9"}"#);
    // Non-terminal tools do not consume terminal keys: `id` must not leak
    // UUID noise.
    let other = Some(serde_json::json!({ "id": "abc-uuid" }));
    assert_eq!(crate::helpers::tool_call_arg("read", &other), "");
    // File/command keys behave unchanged.
    let bash = Some(serde_json::json!({ "command": "git status" }));
    assert_eq!(crate::helpers::tool_call_arg("bash", &bash), "git status");
}

#[test]
fn display_pieces_split_tool_blocks_into_tool_rows() {
    // A surface assistant message merges prose with tool_use/tool_result blocks.
    // Display splitting must emit the tool pieces as separate `tool` rows with
    // ✓/✗-prefixed result bodies (newline intact) — flattening them inline would
    // collapse a file-sized output into markdown prose.
    use crate::sessions::display_pieces;
    use starriver_kernel::llm::{CallId, ContentBlock};
    let blocks = vec![
        ContentBlock::Text("写好了。".into()),
        ContentBlock::ToolCall {
            id: CallId("c1".into()),
            name: Some("write".into()),
            arguments: starriver_kernel::json::JsonValue::from_json_str(r#"{"file":"snake.html"}"#)
                .unwrap(),
        },
        ContentBlock::ToolResult {
            call_id: CallId("c1".into()),
            content: vec![ContentBlock::Text("<!DOCTYPE html>\n<html>".into())],
            is_error: false,
        },
    ];
    let pieces = display_pieces("assistant", &blocks, 1_700_000_000_000);
    assert_eq!(pieces.len(), 3);
    assert_eq!(pieces[0].role, "assistant");
    assert_eq!(pieces[0].content, "写好了。");
    assert_eq!(pieces[1].role, "tool");
    assert_eq!(pieces[1].content, "🔧 write: snake.html");
    assert_eq!(pieces[2].role, "tool");
    assert!(
        pieces[2].content.starts_with("✓\n"),
        "marker on its own line"
    );
    assert!(
        pieces[2].content.contains("<!DOCTYPE html>\n<html>"),
        "newline intact"
    );
}

#[test]
fn display_pieces_projects_reasoning_as_thinking_rows() {
    // Chain-of-thought persistence: reasoning blocks inside assistant/message
    // must project to `thinking` rows (interleaved in block order) instead of
    // being silently dropped — otherwise all thinking disappears on restart.
    use crate::sessions::display_pieces;
    use starriver_kernel::llm::ContentBlock;
    let blocks = vec![
        ContentBlock::Reasoning("先看一下目录结构。".into()),
        ContentBlock::Text("看完了。".into()),
        ContentBlock::Reasoning("再补个测试。".into()),
    ];
    let pieces = display_pieces("assistant", &blocks, 1_700_000_000_000);
    assert_eq!(pieces.len(), 3);
    assert_eq!(pieces[0].role, "thinking");
    assert_eq!(pieces[0].content, "先看一下目录结构。");
    assert_eq!(pieces[1].role, "assistant");
    assert_eq!(pieces[2].role, "thinking");
    assert_eq!(pieces[2].content, "再补个测试。");
}

// ── profile_tool_names tests ──

#[test]
fn profile_tool_names_empty_profile_is_empty() {
    // Empty profile `tools:` adds NOTHING to a mode's whitelist — it never
    // becomes "all tools".
    let mut profile = starriver_skills::profile::agent::AgentProfile {
        name: "Chat".into(),
        version: None,
        description: None,
        author: None,
        icon: None,
        skills: vec![],
        tools: vec![],
        mode: None,
        model: None,
        permissions: None,
        hooks: None,
        commands: vec![],
        ui: None,
        memory: None,
        isolate: None,
        body: "".into(),
        preset: None,
    };
    profile.tools = vec![];
    assert_eq!(
        HostRuntime::profile_tool_names(&profile),
        Vec::<String>::new()
    );
}

#[test]
fn profile_tool_names_maps_builtin_and_mcp() {
    let profile = starriver_skills::profile::agent::AgentProfile {
        name: "Coding".into(),
        version: None,
        description: None,
        author: None,
        icon: None,
        skills: vec![],
        mode: None,
        model: None,
        permissions: None,
        hooks: None,
        commands: vec![],
        ui: None,
        memory: None,
        isolate: None,
        body: "".into(),
        tools: vec![
            starriver_skills::profile::types::ToolRef::Builtin("read".into()),
            starriver_skills::profile::types::ToolRef::Builtin("lsp_symbols".into()),
            starriver_skills::profile::types::ToolRef::Mcp("github".into()),
        ],
        preset: None,
    };
    let names = HostRuntime::profile_tool_names(&profile);
    assert_eq!(
        names,
        vec![
            "read".to_string(),
            "lsp_symbols".to_string(),
            "mcp_github".to_string()
        ]
    );
}

// ── resolve_project_dir (New Project requires an existing user-created dir) ──

#[test]
fn resolve_project_dir_requires_cwd() {
    let err = resolve_project_dir(None).unwrap_err();
    assert!(err.contains("required"), "got: {err}");
}

#[test]
fn resolve_project_dir_rejects_empty_cwd() {
    let err = resolve_project_dir(Some(String::new())).unwrap_err();
    assert!(err.contains("empty"), "got: {err}");
    let err = resolve_project_dir(Some("   ".into())).unwrap_err();
    assert!(err.contains("empty"), "got: {err}");
}

#[test]
fn resolve_project_dir_expands_tilde() {
    // Expansion must map "~/x" to "$HOME/x" — the missing dir error carries the
    // expanded path, so no filesystem writes are needed.
    let home = std::env::var("HOME").unwrap();
    let err = resolve_project_dir(Some("~/nonexistent-starriver-xyz".into())).unwrap_err();
    assert!(
        err.contains(&format!("{home}/nonexistent-starriver-xyz")),
        "got: {err}"
    );
}

#[test]
fn resolve_project_dir_accepts_existing_dir() {
    let dir = std::env::temp_dir().join(format!("starriver-proj-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = resolve_project_dir(Some(dir.to_string_lossy().into_owned())).unwrap();
    assert_eq!(path, dir.to_string_lossy());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resolve_project_dir_rejects_missing_dir() {
    let dir = std::env::temp_dir().join(format!("starriver-missing-{}", std::process::id()));
    let err = resolve_project_dir(Some(dir.to_string_lossy().into_owned())).unwrap_err();
    assert!(err.contains("does not exist"), "got: {err}");
}

#[test]
fn resolve_project_dir_errors_when_parent_is_a_file() {
    let file = std::env::temp_dir().join(format!("starriver-blocker-{}", std::process::id()));
    std::fs::write(&file, "x").unwrap();
    assert!(resolve_project_dir(Some(format!("{}/sub", file.to_string_lossy()))).is_err());
    let _ = std::fs::remove_file(&file);
}

// ── build_skill_md ──

#[test]
fn build_skill_md_full_frontmatter() {
    let md = build_skill_md(
        "write-book",
        "Write books & articles",
        Some("/write-book"),
        "# Workflow\n1. outline\n2. write\n",
    )
    .unwrap();
    assert!(md.starts_with("---\nname: write-book\n"));
    assert!(
        md.contains("description: \"Write books & articles\"\n"),
        "description quoted: {md}"
    );
    assert!(md.contains("version: \"0.1.0\"\n"));
    // Command is quoted like description (a ": " in a command used to
    // emit frontmatter serde_yaml rejects).
    assert!(md.contains("command: \"/write-book\"\n"));
    assert!(md.ends_with("---\n\n# Workflow\n1. outline\n2. write\n"));
}

#[test]
fn build_skill_md_minimal() {
    let md = build_skill_md("simple", "", None, "body text").unwrap();
    assert!(md.contains("name: simple\n"));
    assert!(!md.contains("description:"), "empty description omitted");
    assert!(!md.contains("command:"), "no command omitted");
    assert!(md.ends_with("body text"));
}

#[test]
fn build_skill_md_rejects_bad_name() {
    assert!(build_skill_md("../evil", "", None, "x").is_err());
    assert!(build_skill_md("has space", "", None, "x").is_err());
    assert!(build_skill_md("", "", None, "x").is_err());
    assert!(build_skill_md("ok-name_1", "", None, "x").is_ok());
}

#[test]
fn build_skill_md_rejects_empty_body() {
    assert!(build_skill_md("x", "", None, "   ").is_err());
}

// ── apply_slash_dispatch ──

#[test]
fn slash_dispatch_injects_skill_and_strips_command() {
    let profile = starriver_skills::profile::skill::SkillProfile {
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
    };
    let (task, injection) = apply_slash_dispatch("/write-book 写一本关于AI的书", Some(&profile));
    assert_eq!(task, "写一本关于AI的书");
    let inj = injection.unwrap();
    assert!(inj.contains("## Skill: write-book"));
    assert!(inj.contains("1. ask"));
}

#[test]
fn slash_dispatch_empty_args_uses_skill_name() {
    let profile = starriver_skills::profile::skill::SkillProfile {
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
        body: "body".into(),
    };
    let (task, injection) = apply_slash_dispatch("/write-book", Some(&profile));
    assert!(task.contains("write-book"));
    assert!(injection.is_some());
}

#[test]
fn slash_dispatch_unknown_skill_passes_through() {
    let (task, injection) = apply_slash_dispatch("/nope do something", None);
    assert_eq!(task, "/nope do something");
    assert!(injection.is_none());
}

#[test]
fn non_command_task_untouched() {
    let (task, injection) = apply_slash_dispatch("just a normal task", None);
    assert_eq!(task, "just a normal task");
    assert!(injection.is_none());
}

// ── set_always_on_in_skill_md ──

#[test]
fn always_on_adds_flag_preserving_other_fields() {
    let md = "---\nname: write-book\ncommand: /write-book\n---\n\nbody text\n";
    let out = set_always_on_in_skill_md(md, true).unwrap();
    assert!(
        out.starts_with(
            "---\nalways-on: true\nname: write-book\ncommand: /write-book\n---\n\nbody text\n"
        ),
        "got: {out}"
    );
}

#[test]
fn always_on_updates_existing_flag() {
    let md = "---\nalways-on: true\nname: x\n---\n\nbody\n";
    let out = set_always_on_in_skill_md(md, false).unwrap();
    assert!(out.contains("always-on: false\n"));
    assert!(!out.contains("always-on: true"));
    assert!(out.ends_with("\n\nbody\n"));
}

#[test]
fn always_on_rejects_bad_frontmatter() {
    assert!(set_always_on_in_skill_md("no frontmatter", true).is_none());
}

// ── split_model_key ──

#[test]
fn split_model_key_resolves_basic_provider_variant() {
    let ids = ["deepseekv4", "lmstudio"];
    assert_eq!(
        split_model_key("lmstudio/gemma-4-e2b-it-mlx", &ids),
        Some(("lmstudio".into(), "gemma-4-e2b-it-mlx"))
    );
    assert_eq!(
        split_model_key("deepseekv4/deepseek-v4-flash", &ids),
        Some(("deepseekv4".into(), "deepseek-v4-flash"))
    );
}

#[test]
fn split_model_key_keeps_slashed_variant_ids_intact() {
    // OpenRouter keys are "openrouter/<vendor>/<model>" — only the provider is split off.
    let ids = ["openrouter", "lmstudio"];
    assert_eq!(
        split_model_key("openrouter/openai/gpt-4o", &ids),
        Some(("openrouter".into(), "openai/gpt-4o"))
    );
}

#[test]
fn split_model_key_prefers_longest_matching_provider() {
    // Both "deepseek" and "deepseekv4" could prefix, but only a real id + "/" matches.
    let ids = ["deepseek", "deepseekv4"];
    assert_eq!(
        split_model_key("deepseekv4/deepseek-v4-pro", &ids),
        Some(("deepseekv4".into(), "deepseek-v4-pro"))
    );
}

#[test]
fn split_model_key_returns_none_for_unknown_or_bare_key() {
    let ids = ["deepseekv4", "lmstudio"];
    // Provider not in the registry (stale/renamed, e.g. old "gemma4e2b" id)
    assert_eq!(split_model_key("gemma4e2b/gemma-4-e2b-it-mlx", &ids), None);
    // No variant segment at all
    assert_eq!(split_model_key("lmstudio", &ids), None);
    // Not even provider-prefixed
    assert_eq!(split_model_key("ollama/llama3:8b", &ids), None);
}

#[test]
fn resolve_agent_model_key_yields_variant_and_provider_ids() {
    let providers = vec!["deepseekv4".to_string(), "lmstudio".to_string()];
    assert_eq!(
        resolve_agent_model_key(Some("lmstudio/gemma-4-e2b-it-mlx".into()), &providers),
        (Some("gemma-4-e2b-it-mlx".into()), Some("lmstudio".into()))
    );
    // Two agents with different defaults resolve to different pairs (independent).
    assert_eq!(
        resolve_agent_model_key(Some("deepseekv4/deepseek-v4-flash".into()), &providers),
        (Some("deepseek-v4-flash".into()), Some("deepseekv4".into()))
    );
}

#[test]
fn resolve_agent_model_key_handles_absent_or_stale_keys() {
    let providers = vec!["deepseekv4".to_string(), "lmstudio".to_string()];
    // No model configured at all
    assert_eq!(resolve_agent_model_key(None, &providers), (None, None));
    // Provider not in registry (e.g. old "gemma4e2b" id) — must not mis-split
    assert_eq!(
        resolve_agent_model_key(Some("gemma4e2b/gemma-4-e2b-it-mlx".into()), &providers),
        (None, None)
    );
}

// ── builtin_provider_presets ──

fn preset<'a>(presets: &'a [ModelProvider], id: &str) -> &'a ModelProvider {
    presets
        .iter()
        .find(|p| p.id == id)
        .unwrap_or_else(|| panic!("preset {id} missing"))
}

#[test]
fn presets_are_4_cloud_plus_ollama() {
    let presets = builtin_provider_presets(|_| false);
    assert_eq!(presets.len(), 5);
    assert_eq!(
        presets
            .iter()
            .filter(|p| p.provider_type == "cloud")
            .count(),
        4
    );
    assert_eq!(
        presets
            .iter()
            .filter(|p| p.provider_type == "local")
            .count(),
        1
    );
    // Stable order = the card grid order shown to the user.
    let ids: Vec<&str> = presets.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(
        ids,
        ["deepseek", "qwen", "zhipu", "moonshot", "ollama"]
    );
}

#[test]
fn preset_ids_unique_and_names_nonempty() {
    let presets = builtin_provider_presets(|_| false);
    let mut seen = std::collections::HashSet::new();
    for p in &presets {
        assert!(seen.insert(p.id.clone()), "duplicate preset id {}", p.id);
        assert!(!p.name.is_empty(), "empty name for {}", p.id);
    }
}

#[test]
fn deepseek_gets_high_reasoning_effort_and_openai_chat() {
    let presets = builtin_provider_presets(|_| false);
    let p = preset(&presets, "deepseek");
    assert_eq!(p.reasoning_effort.as_deref(), Some("high"));
    assert_eq!(p.api_format.as_deref(), Some("openai-chat"));
    assert_eq!(p.base_url, "https://api.deepseek.com");
    assert_eq!(p.models, ["deepseek-chat", "deepseek-reasoner"]);
}

#[test]
fn other_cloud_presets_are_openai_chat_without_effort_hint() {
    let presets = builtin_provider_presets(|_| false);
    for id in ["qwen", "zhipu", "moonshot"] {
        let p = preset(&presets, id);
        assert_eq!(
            p.api_format.as_deref(),
            Some("openai-chat"),
            "{id} api_format"
        );
        assert_eq!(p.reasoning_effort, None, "{id} reasoning_effort");
        assert!(p.requires_api_key, "{id} requires key");
        assert!(!p.models.is_empty(), "{id} ships default models");
        assert!(p.base_url.starts_with("https://"), "{id} uses https");
    }
}

#[test]
fn cloud_preset_base_urls_are_stable() {
    let presets = builtin_provider_presets(|_| false);
    let expected = [
        ("qwen", "https://dashscope.aliyuncs.com/compatible-mode/v1"),
        ("zhipu", "https://open.bigmodel.cn/api/coding/paas/v4"),
        ("moonshot", "https://api.moonshot.cn/v1"),
    ];
    for (id, url) in expected {
        assert_eq!(preset(&presets, id).base_url, url, "{id} base_url");
    }
}

#[test]
fn ollama_stays_local_without_format_or_key() {
    let presets = builtin_provider_presets(|_| false);
    let p = preset(&presets, "ollama");
    assert_eq!(p.provider_type, "local");
    assert!(!p.requires_api_key);
    assert!(p.enabled, "ollama is always enabled regardless of registry");
    assert_eq!(p.api_format, None);
    assert_eq!(p.reasoning_effort, None);
    assert!(p.models.is_empty());
    assert_eq!(p.base_url, "http://localhost:11434");
}

#[test]
fn enabled_flag_reflects_registry_state() {
    // Simulates ProviderRegistry::is_enabled: only configured ids flip to true.
    let enabled = |id: &str| matches!(id, "deepseek" | "moonshot");
    let presets = builtin_provider_presets(enabled);
    assert!(preset(&presets, "deepseek").enabled);
    assert!(preset(&presets, "moonshot").enabled);
    assert!(!preset(&presets, "qwen").enabled);
    assert!(
        preset(&presets, "ollama").enabled,
        "ollama hardcodes enabled=true"
    );
    // Every provider still listed even when nothing is configured.
    let none = builtin_provider_presets(|_| false);
    assert_eq!(none.len(), 5);
    assert!(none.iter().all(|p| !p.enabled || p.id == "ollama"));
}

// ── truncate_title char-boundary tests ──
//
// The crash this guards: `&trimmed[..max]` byte-slices a UTF-8 string; when `max`
// falls inside a multi-byte character (any script — CJK, Cyrillic, accented Latin,
// emoji, …), Rust panics with "byte index is not a char boundary". list_sessions ran
// this on a napi background thread, and panic=abort took down the whole app (SIGABRT).
//
// Multi-byte strings from many scripts, each long enough to force truncation across
// every byte offset — including every mid-char boundary.
const LANG_CORPUS: &[(&str, &str)] = &[
    (
        "ascii",
        "The quick brown fox jumps over the lazy dog 0123456789 ABCDEFGHIJ",
    ),
    (
        "latin-1",
        "café déjà vu ñoño übel allé über groß straße naïve résumé",
    ),
    (
        "cyrillic",
        "Привет мир как дела сегодня у нас всё хорошо нормально",
    ),
    ("greek", "Καλημέρα κόσμε τι κάνετε σήμερα καλά ευχαριστώ"),
    ("hebrew", "שלום עולם מה שלומך היום בוקר טוב יפה מאוד"),
    (
        "arabic",
        "مرحبا بالعالم كيف حالك اليوم صباح الخير شكرا جزيلا",
    ),
    (
        "chinese",
        "帮我写一个Python脚本来批量重命名文件并把结果输出到日志文件里面去",
    ),
    (
        "japanese",
        "こんにちは世界今日はとてもいい天気ですね元気ですか元気です",
    ),
    (
        "korean",
        "안녕하세요 세계 오늘은 정말 좋은 날씨네요 잘 지내세요 네 좋아요",
    ),
    ("thai", "สวัสดีชาวโลกวันนี้อากาศดีมากสบายดีไหมครับขอบคุณมาก"),
    ("hindi", "नमस्ते दुनिया आज मौसम बहुत अच्छा है सब कैसे हैं धन्यवाद"),
    ("emoji", "🚀✨🌍 Emoji test 🎉🏆🔥💯⚡🚀✨🌍🎉🏆🔥💯"),
    (
        "emoji-zwj",
        "👨👩👧👦 Family ZWJ 👨👩👧👦 sequence with skin tone 👍🏽",
    ),
    (
        "mixed",
        "配置zhipu的API Key🚀 然后测试Coding Agent的中文标题到底怎么截断",
    ),
];

/// Independently compute the expected truncation via chars() (a different
/// code path than truncate_title's chars().take) so the comparison is
/// meaningful. The cap is CHARACTERS (BUG-99: the title contract is the
/// 40-char display cap the model is instructed against, not 40 bytes).
fn expected_truncation(s: &str, max: usize) -> String {
    let trimmed = s.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_string();
    }
    let head: String = trimmed.chars().take(max).collect();
    format!("{head}...")
}

#[test]
fn truncate_title_never_panics_on_any_multibyte_corpus_and_matches_reference() {
    for (lang, s) in LANG_CORPUS {
        // Every byte offset as the cut point — this sweeps every mid-char boundary.
        for max in 0..=(s.len() + 2) {
            let out = truncate_title(s, max);
            assert_eq!(
                out,
                expected_truncation(s, max),
                "lang={lang} max={max} len={}",
                s.len()
            );
            let trimmed = s.trim();
            if trimmed.chars().count() > max {
                assert!(out.ends_with("..."));
                let head = &out[..out.len() - 3];
                assert!(trimmed.starts_with(head));
                assert_eq!(head.chars().count(), max);
            } else {
                assert_eq!(out, trimmed);
            }
        }
    }
}

#[test]
fn truncate_title_chinese_crash_repro_does_not_panic() {
    // Char-cap semantics: the 30-char title over 40 chars gets a clean cut.
    let title = "帮我写一个Python脚本来批量重命名文件并把结果输出到日志文件里面再加长一点超限";
    let out = truncate_title(title, 40);
    assert!(out.ends_with("..."));
    assert!(title.starts_with(&out[..out.len() - 3]));
    assert_eq!(out[..out.len() - 3].chars().count(), 40);
}

#[test]
fn truncate_title_ascii_exact_and_past_boundaries() {
    let s = "abcdefghijklmnopqrstuvwxyz0123456789"; // 36 bytes
    assert_eq!(truncate_title(s, 36), s); // exactly max → unchanged
    assert_eq!(truncate_title(s, 40), s); // max > len → unchanged
    assert_eq!(truncate_title(s, 10), "abcdefghij..."); // all-ASCII, byte 10 is a boundary
    assert_eq!(truncate_title(s, 0), "..."); // max=0 on non-empty
    assert_eq!(truncate_title("", 5), ""); // empty stays empty
}

#[test]
fn truncate_title_max_one_handles_multibyte_first_char() {
    assert_eq!(truncate_title("🙂x", 1), "🙂..."); // char cap keeps the emoji
    assert_eq!(truncate_title("你x", 1), "你...");
    assert_eq!(truncate_title("ax", 1), "a...");
}

#[test]
fn truncate_title_snaps_down_never_up_and_trims_whitespace() {
    // Char-cap semantics: the count is characters, not bytes.
    assert_eq!(truncate_title("他她它", 3), "他她它"); // exactly 3 chars → unchanged
    assert_eq!(truncate_title("他她它", 2), "他她...");
    assert_eq!(truncate_title("他她它", 1), "他...");
    // Leading/trailing whitespace is trimmed BEFORE truncation.
    assert_eq!(truncate_title("  他她它", 2), "他她...");
}

#[test]
fn truncate_title_zwj_emoji_not_split() {
    // A ZWJ sequence is one grapheme but multiple chars; char-cap cuts
    // never split a codepoint. max beyond the char count leaves it whole.
    let family = "👨\u{200d}👩\u{200d}👧\u{200d}👦";
    let char_count = family.chars().count();
    for max in 1..=(char_count + 2) {
        let out = truncate_title(family, max);
        if max >= char_count {
            assert_eq!(out, family, "max={max} keeps it whole");
        } else {
            assert!(out.ends_with("..."), "max={max}");
            assert!(family.starts_with(&out[..out.len() - 3]), "max={max}");
        }
    }
}
