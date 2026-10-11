//! Global UI i18n support: language resolution + key translation table.
//!
//! Single source of truth: all UI text (header/status line/pickers/approval/event stream) is written as
//! keys in `MESSAGES`, looked up by [`tr`] for the current language. Unknown keys are returned as-is to surface missing translations.

/// UI language.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Zh,
    En,
}

/// Current language: `STARRIVER_LANG` first, then system locale (LC_ALL/LC_MESSAGES/LANG), defaulting to Chinese.
pub fn current_lang() -> Lang {
    if let Ok(v) = std::env::var("STARRIVER_LANG") {
        let v = v.to_lowercase();
        if v.starts_with("zh") {
            return Lang::Zh;
        }
        if v.starts_with("en") {
            return Lang::En;
        }
    }
    for var in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(v) = std::env::var(var) {
            let v = v.to_lowercase();
            if v.contains("zh") {
                return Lang::Zh;
            }
            if v.contains("en") {
                return Lang::En;
            }
        }
    }
    Lang::Zh
}

/// Look up the translation table by language; unknown keys are returned as-is (to surface missing translations).
/// The return value borrows the static table or `key` itself, with the shorter lifetime of the two.
pub fn tr(lang: Lang, key: &str) -> &str {
    MESSAGES
        .iter()
        .find(|entry| entry.0 == key)
        .map(|entry| match lang {
            Lang::Zh => entry.1,
            Lang::En => entry.2,
        })
        .unwrap_or(key)
}

/// Replace `{}` placeholders in the template in order and return a `String`.
///
/// `std::format!` requires a literal format string, but translation templates are runtime values, so a macro expansion does positional replacement.
/// When the template has fewer `{}` than args, extra args are ignored; when it has more, the remaining template is kept as-is.
macro_rules! fmt {
    ($template:expr, $($arg:expr),* $(,)?) => {{
        let template: &str = $template;
        let mut out = String::with_capacity(template.len() + 16);
        let mut rest = template;
        $(
            if let Some((head, tail)) = rest.split_once("{}") {
                out.push_str(head);
                out.push_str(&$arg.to_string());
                rest = tail;
            }
            // No more placeholders: extra args are IGNORED and the remaining
            // template stays — blanking `rest` dropped the whole tail when a
            // locale's template carried fewer {} than the call passed.
        )*
        out.push_str(rest);
        out
    }};
}
pub(crate) use fmt;

/// Translation table: (key, Chinese, English). Format placeholders stay consistent on both sides.
const MESSAGES: &[(&str, &str, &str)] = &[
    // -- Header --
    ("header.mode_suffix", "模式", "mode"),
    ("mode.picker", "选择", "Select"),
    ("mode.approval", "审批", "Approve"),
    // -- Pickers --
    ("picker.sessions.title", " 会话 ", " Sessions "),
    ("picker.workspaces.title", " 项目 ", " Projects "),
    (
        "picker.providers.title",
        " 模型 provider ",
        " Model Providers ",
    ),
    ("picker.models_for.title", " {} 模型 ", " {} Models "),
    ("picker.model_count", "{} 个模型", "{} models"),
    (
        "picker.messages.title",
        " 回溯到消息 ",
        " Backtrack to message ",
    ),
    ("picker.archived.title", " 归档会话 ", " Archived Sessions "),
    ("picker.presets.title", " 沙箱预设 ", " Sandbox Presets "),
    (
        "picker.preset_approval.title",
        " {} · 审批模式 ",
        " {} · Approval ",
    ),
    // -- Sandbox preset picker (/preset) --
    ("preset.standard.name", "标准", "Standard"),
    ("preset.plan.name", "规划", "Plan"),
    ("preset.ptc.name", "批量", "PTC"),
    ("preset.goal.name", "目标", "Goal"),
    (
        "preset.standard.desc",
        "日常编码：工作区可写 + 手动确认",
        "Daily coding: workspace-write + manual approval",
    ),
    (
        "preset.plan.desc",
        "先规划后执行：规划阶段锁定只读 + 手动确认",
        "Plan first: read-only planning phase + manual approval",
    ),
    (
        "preset.ptc.desc",
        "批量任务：工作区可写 + 智能放行",
        "Batch tasks: workspace-write + auto approval",
    ),
    (
        "preset.goal.desc",
        "长程目标：工作区可写 + 智能放行",
        "Long-run goals: workspace-write + auto approval",
    ),
    (
        "preset.approval.default",
        "跟随预设（{}）",
        "Follow preset ({})",
    ),
    ("preset.approval.manual", "手动确认", "Manual"),
    (
        "preset.approval.accept-edits",
        "自动接受编辑",
        "Accept edits",
    ),
    ("preset.approval.auto", "智能放行", "Auto"),
    (
        "preset.approval.bypass",
        "完全放行（需授权）",
        "Bypass (needs authorization)",
    ),
    (
        "status.pick_preset",
        "↑↓ 选择预设，Enter 确认，Esc 返回",
        "↑↓ pick a preset, Enter confirm, Esc back",
    ),
    (
        "status.pick_preset_approval",
        "↑↓ 选择审批模式，Enter 确认，Esc 返回",
        "↑↓ pick an approval mode, Enter confirm, Esc back",
    ),
    (
        "status.preset_switched",
        "预设已切换：{}（sandbox {} · approval {}）",
        "Preset switched: {} (sandbox {} · approval {})",
    ),
    (
        "status.preset_switch_failed",
        "切换预设失败：{}",
        "Switch preset failed: {}",
    ),
    (
        "status.preset_not_found",
        "未知预设：{}（standard | plan | ptc | goal）",
        "Unknown preset: {} (standard | plan | ptc | goal)",
    ),
    (
        "status.preset_usage",
        "用法：/preset [standard|plan|ptc|goal]",
        "Usage: /preset [standard|plan|ptc|goal]",
    ),
    (
        "status.preset_label",
        "预设：{}（sandbox {}）",
        "Preset: {} (sandbox {})",
    ),
    // -- Input box --
    (
        "input.running_title",
        " 运行中… Ctrl+C 取消 ",
        " Running… Ctrl+C to cancel ",
    ),
    // -- Bottom status line --
    ("footer.mode_cwd", "{}  目录：{}", "{}  cwd: {}"),
 // -- -style composer chrome (round 11) --
    (
        "input.placeholder",
        "输入消息或 / 命令…",
        "Type a message or / command…",
    ),
    (
        "spinner.working",
        "{} 工作中 {}",
        "{} Working {}",
    ),
    (
        "spinner.hint",
        "Ctrl+C 取消",
        "Ctrl+C to interrupt",
    ),
    (
        "hint.idle",
        "enter 发送 · ↑↓ 历史 · Ctrl+O 输出详情 · Esc 清空",
        "enter send · ↑↓ history · Ctrl+O tool detail · Esc clear",
    ),
    (
        "hint.running",
        "Esc/Ctrl+C 中断 · Ctrl+O 输出详情 · ↑↓ 滚动",
        "Esc/Ctrl+C interrupt · Ctrl+O tool detail · ↑↓ scroll",
    ),
    (
        "hint.back_to_bottom",
        "↓ End 回到底部（已上滚 {} 行）",
        "↓ End back to bottom (scrolled up {} lines)",
    ),
    (
        "status.verbose_on",
        "工具输出：完整模式",
        "Tool output: verbose",
    ),
    (
        "status.verbose_off",
        "工具输出：摘要模式",
        "Tool output: compact",
    ),
    ("tool.calling", "{} 正在调用 {}", "{} Calling {}"),
    // -- WI-76/77/78 (round 12) --
    (
        "queue.entered",
        "⋯ 已排队：{}",
        "⋯ queued: {}",
    ),
    (
        "queue.cleared",
        "已清空 {} 条排队消息",
        "cleared {} queued message(s)",
    ),
    (
        "queue.dropped_session_gone",
        "排队消息已作废（会话已切换）",
        "queued messages dropped (session switched)",
    ),
    (
        "status.pasted",
        "已粘贴 {} 行",
        "pasted {} lines",
    ),
    (
        "usage.summary",
        "in {} / out {}",
        "in {} / out {}",
    ),
    (
        "diff.truncated",
        "…（截断，共 {} 行）",
        "…(truncated, {} lines total)",
    ),
    (
        "tool.grouped",
        "⋯ {} 次工具调用 · {}",
        "⋯ {} tool calls · {}",
    ),
    (
        "transcript.hint",
        "Esc 返回 · / 搜索 · n/N 跳转 · ↑↓ 滚动",
        "Esc back · / search · n/N jump · ↑↓ scroll",
    ),
    (
        "transcript.searching",
        "/{}",
        "/{}",
    ),
    (
        "transcript.no_match",
        "无匹配",
        "no match",
    ),
    (
        "transcript.search_hint",
        " 输入搜索词，Enter 确认 ",
        " type query, Enter to confirm ",
    ),
    (
        "usage.ctx_left",
        "ctx {}% · 余 {}",
        "ctx {}% · {} left",
    ),
    ("tool.finished", "{} {} · {}ms", "{} {} · {}ms"),
    // -- Approval overlay --
    ("approval.title", " 审批 ", " Approval "),
    ("approval.tool", "工具：{}", "Tool: {}"),
    ("approval.reason", "原因：{}", "Reason: {}"),
    (
        "approval.actions",
        "[y] 允许   [n] 拒绝   [m] 记住({})   [Esc] 稍后",
        "[y] allow   [n] deny   [m] remember({})   [Esc] later",
    ),
    // -- Status line --
    ("status.running", "运行中…", "Running…"),
    ("status.exec_failed", "执行失败：{}", "Execute failed: {}"),
    (
        "status.events_lagged",
        "事件流落后，部分事件已丢弃",
        "Event stream lagged; some events dropped",
    ),
    (
        "status.stream_closed",
        "任务事件流已关闭",
        "Task event stream closed",
    ),
    (
        "status.event_parse_failed",
        "事件解析失败：{}",
        "Event parse failed: {}",
    ),
    ("status.task_done", "完成：{}", "Done: {}"),
    ("status.task_failed", "失败：{}", "Failed: {}"),
    ("status.cancelled", "已取消", "Cancelled"),
    ("status.cancelling", "取消中…", "Cancelling…"),
    ("status.allowed", "已允许", "Allowed"),
    ("status.denied", "已拒绝", "Denied"),
    (
        "status.approval_deferred",
        "审批稍后处理",
        "Approval deferred",
    ),
    ("status.diff_done", "diff 已输出到对话", "Diff written to the conversation"),
    ("status.help_shown", "帮助已显示", "Help shown"),
    ("status.new_session", "新会话", "New session"),
    (
        "status.pick_session",
        "↑↓ 选择会话，Enter 续聊，Esc 返回",
        "↑↓ select session, Enter continue, Esc back",
    ),
    (
        "status.pick_project",
        "↑↓ 选择项目，Enter 切换，Esc 返回",
        "↑↓ select project, Enter switch, Esc back",
    ),
    (
        "status.model_switched",
        "模型已切换：{}",
        "Model switched: {}",
    ),
    (
        "status.model_switch_failed",
        "切换模型失败：{}",
        "Switch model failed: {}",
    ),
    (
        "status.mode_switched",
        "模式已切换：{}",
        "Mode switched: {}",
    ),
    (
        "status.mode_switch_failed",
        "切换模式失败：{}",
        "Switch mode failed: {}",
    ),
    (
        "status.pick_model_for",
        "选择 {} 的模型：↑↓ 移动，Enter 切换，Esc 返回",
        "Select model for {}: ↑↓ move, Enter switch, Esc back",
    ),
    (
        "status.provider_not_found",
        "provider 不存在：{}",
        "Provider not found: {}",
    ),
    (
        "status.pick_provider",
        "选择 provider：↑↓ 移动，Enter 进入，Esc 返回",
        "Select provider: ↑↓ move, Enter open, Esc back",
    ),
    (
        "status.model_usage",
        "用法：/model [provider] [model]",
        "Usage: /model [provider] [model]",
    ),
    (
        "status.project_not_found",
        "项目不存在：{}",
        "Project not found: {}",
    ),
    (
        "status.current_workspace",
        "当前工作区：{}（{}）",
        "Current workspace: {} ({})",
    ),
    (
        "status.no_workspace",
        "当前工作区：无",
        "Current workspace: none",
    ),
    (
        "status.unknown_cmd",
        "未知命令：{}（/help 查看）",
        "Unknown command: {} (see /help)",
    ),
    (
        "status.session_selected",
        "已选择会话 {} — 输入 prompt 续聊（/new 开新会话）",
        "Session {} selected — type prompt to continue (/new for new)",
    ),
    ("status.back", "已返回", "Back"),
    ("status.project", "项目：{}", "Project: {}"),
    ("role.user", "你", "You"),
    ("role.assistant", "助手", "Assistant"),
    // -- New-command feedback (/status /compact /usage /init /export /lang /diff) --
    ("status.status_title", "状态", "Status"),
    ("status.agent", "Agent：{}", "Agent: {}"),
    ("status.model_label", "模型：{}", "Model: {}"),
    ("status.mode_label", "模式：{}", "Mode: {}"),
    ("status.session", "会话：{}", "Session: {}"),
    (
        "status.counts",
        "工作区 {} 个 / 会话 {} 个",
        "{} workspaces / {} sessions",
    ),
    (
        "status.compact_done",
        "已压缩：{} → {} tokens",
        "Compacted: {} → {} tokens",
    ),
    (
        "status.no_session",
        "没有当前会话（先运行任务，或用 /sessions 选择）",
        "No current session (run a task, or pick one with /sessions)",
    ),
    (
        "status.usage_line",
        "会话 {}：{} tokens / {} 条消息",
        "Session {}: {} tokens / {} messages",
    ),
    ("status.init_done", "项目文档：{}", "Project doc: {}"),
    (
        "memory.status_title",
        "共享记忆状态",
        "Shared memory status",
    ),
    (
        "memory.ingest_done",
        "摄取完成：{} 条新记忆",
        "Ingestion done: {} new memories",
    ),
    ("memory.export_done", "已写入：{}", "Written: {}"),
    (
        "memory.export_noop",
        "无变更（记忆已是最新）",
        "No changes (memories up to date)",
    ),
    (
        "memory.unknown_sub",
        "未知子命令：{}（可用：status | ingest | export）",
        "Unknown subcommand: {} (use: status | ingest | export)",
    ),
    ("status.export_done", "已导出：{}", "Exported: {}"),
    (
        "status.no_cwd",
        "未设置项目工作目录（用 /open 或 /project 选择）",
        "No project working directory (use /open or /project)",
    ),
    (
        "status.diff_empty",
        "工作区无改动",
        "No changes in workspace",
    ),
    ("status.lang_switched", "语言：{}", "Language: {}"),
    (
        "status.lang_unknown",
        "未知语言：{}（zh / en）",
        "Unknown language: {} (zh / en)",
    ),
    ("status.lang_zh", "中文", "Chinese"),
    ("status.lang_en", "English", "English"),
    // -- Thread fork/backtrack/delete/worktree isolation --
    ("status.forked", "已分叉新线程：{}", "Forked new thread: {}"),
    ("status.fork_failed", "分叉失败：{}", "Fork failed: {}"),
    (
        "status.backtrack_no_session",
        "没有当前会话（先运行任务，或用 /sessions 选择）",
        "No current session (run a task, or pick one with /sessions)",
    ),
    (
        "status.backtrack_no_messages",
        "当前会话没有可回溯的 user 消息",
        "No user messages to backtrack",
    ),
    (
        "status.pick_backtrack",
        "↑↓ 选择要回溯到的消息，Enter 分叉新线程，Esc 返回",
        "↑↓ pick a message to backtrack to, Enter forks, Esc back",
    ),
    (
        "status.session_deleted",
        "已删除会话：{}",
        "Session deleted: {}",
    ),
    (
        "status.session_delete_failed",
        "删除会话失败：{}",
        "Delete session failed: {}",
    ),
    (
        "status.delete_confirm",
        "确认删除？[y] 删除 [n] 取消",
        "Confirm delete? [y] delete [n] cancel",
    ),
    (
        "approval.delete_session",
        "确认删除当前会话？子分支将提升为根会话。",
        "Delete the current session? Child branches become roots.",
    ),
    (
        "status.worktree_toggled",
        "worktree 隔离：{}",
        "Worktree isolation: {}",
    ),
    ("status.on", "开", "on"),
    ("status.off", "关", "off"),
    ("status.renamed", "已重命名会话：{}", "Renamed session: {}"),
    (
        "status.rename_failed",
        "重命名失败：{}",
        "Rename failed: {}",
    ),
    (
        "status.name_usage",
        "用法：/name <标题>（无参清除覆盖）",
        "Usage: /name <title> (no arg clears the override)",
    ),
    (
        "status.archived",
        "已归档会话（/unarchive 可恢复）",
        "Session archived (restore with /unarchive)",
    ),
    (
        "status.archive_failed",
        "归档失败：{}",
        "Archive failed: {}",
    ),
    (
        "status.unarchived",
        "已恢复会话：{}",
        "Restored session: {}",
    ),
    (
        "status.unarchive_failed",
        "恢复失败：{}",
        "Restore failed: {}",
    ),
    ("status.no_archived", "没有归档会话", "No archived sessions"),
    (
        "status.pick_unarchive",
        "↑↓ 选择要恢复的会话，Enter 恢复，Esc 返回",
        "↑↓ pick a session to restore, Enter restores, Esc back",
    ),
    (
        "status.prefilled",
        "已填入模板，Enter 提交",
        "Prompt prefilled — press Enter to run",
    ),
    (
        "status.review_usage",
        "用法：/review <文件…>",
        "Usage: /review <files…>",
    ),
    (
        "status.test_usage",
        "用法：/test <目标>",
        "Usage: /test <target>",
    ),
    (
        "status.explore_usage",
        "用法：/explore <查询>",
        "Usage: /explore <query>",
    ),
    // -- Event stream (lines rendered by events.rs) --
    (
        "event.task_started",
        "▶ 任务开始 [{}]",
        "▶ Task started [{}]",
    ),
    ("event.task_done", "✓ 完成：{}", "✓ Done: {}"),
    ("event.task_failed", "✗ 失败：{}", "✗ Failed: {}"),
    ("event.task_cancelled", "⏹ 已取消", "⏹ Cancelled"),
    ("event.tool_done", "  {} 完成 ({})", "  {} done ({})"),
    (
        "event.compaction",
        "↻ 压缩 {}: {} → {} tokens",
        "↻ Compact {}: {} → {} tokens",
    ),
    (
        "event.approval_required",
        "⏳ 等待审批：{} — {}",
        "⏳ Approval needed: {} — {}",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tr_returns_localized_text() {
        assert_eq!(tr(Lang::Zh, "status.running"), "运行中…");
        assert_eq!(tr(Lang::En, "status.running"), "Running…");
        assert_eq!(
            tr(Lang::En, "status.project_not_found"),
            "Project not found: {}"
        );
    }

    #[test]
    fn mode_keys_localized() {
        assert_eq!(tr(Lang::Zh, "status.mode_switched"), "模式已切换：{}");
        assert_eq!(tr(Lang::En, "status.mode_switched"), "Mode switched: {}");
        assert_eq!(
            tr(Lang::Zh, "status.mode_switch_failed"),
            "切换模式失败：{}"
        );
        assert_eq!(
            tr(Lang::En, "status.mode_switch_failed"),
            "Switch mode failed: {}"
        );
    }

    #[test]
    fn tr_unknown_key_passthrough() {
        assert_eq!(tr(Lang::Zh, "status.missing_key"), "status.missing_key");
        assert_eq!(tr(Lang::En, "status.missing_key"), "status.missing_key");
    }

    #[test]
    fn all_keys_have_both_languages() {
        for (key, zh, en) in MESSAGES {
            assert!(!key.is_empty(), "empty key");
            assert!(!zh.is_empty(), "zh empty for {key}");
            assert!(!en.is_empty(), "en empty for {key}");
            // Placeholders stay consistent: the Chinese and English format strings should have the same number of {}, otherwise format! panics.
            let zh_placeholders = zh.matches("{}").count();
            let en_placeholders = en.matches("{}").count();
            assert_eq!(
                zh_placeholders, en_placeholders,
                "placeholder mismatch for {key}: zh={zh:?} en={en:?}"
            );
        }
    }

    #[test]
    fn keys_are_unique() {
        let mut keys: Vec<&str> = MESSAGES.iter().map(|(k, _, _)| *k).collect();
        keys.sort_unstable();
        for w in keys.windows(2) {
            assert_ne!(w[0], w[1], "duplicate key {}", w[0]);
        }
    }

    #[test]
    fn current_lang_prefers_starriver_lang() {
        std::env::set_var("STARRIVER_LANG", "en-US");
        assert_eq!(current_lang(), Lang::En);
        std::env::set_var("STARRIVER_LANG", "zh-CN");
        assert_eq!(current_lang(), Lang::Zh);
        std::env::remove_var("STARRIVER_LANG");
    }
}
