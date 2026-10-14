use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use starriver_kernel::ui::UiEvent;

use crate::i18n::{self, Lang};

const MAX_TOOL_OUTPUT: usize = 200;
/// Ctrl+O verbose preview cap (`transcript_lines` full-detail idea).
const VERBOSE_TOOL_OUTPUT: usize = 2000;

/// -style user-input echo cell: `› ` bold prefix on the submitted text.
pub fn user_echo_line(text: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            "› ",
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        ),
        Span::styled(text.to_string(), Style::default().fg(Color::White)),
    ])
}

// ── WI-79: unified-diff parsing + grouped rendering ─────────────────────

/// One parsed unified-diff file section.
#[derive(Debug, Clone, PartialEq)]
pub struct FileDiff {
    pub path: String,
    pub add: usize,
    pub del: usize,
    pub lines: Vec<DiffLineKind>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DiffLineKind {
    Context(String),
    Add(String),
    Del(String),
    Hunk(String),
}

/// Cap on rendered body lines per file in the expanded view.
pub const DIFF_MAX_BODY_LINES: usize = 40;

/// True when the text carries a `--- ` / `+++ ` header pair (a unified diff).
pub fn looks_like_unified_diff(text: &str) -> bool {
    text.lines().any(|l| l.starts_with("--- "))
        && text.lines().any(|l| l.starts_with("+++ "))
}

/// Parse a unified diff into per-file sections. Prose before the first
/// `--- ` header and the `diff --git` chrome are ignored.
pub fn parse_unified_diff(text: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut current: Option<FileDiff> = None;
    for line in text.lines() {
        if let Some(a) = line.strip_prefix("--- ") {
            // A new file header flushes the previous section.
            if let Some(f) = current.take() {
                files.push(f);
            }
            let _ = a; // path preference: +++ line (matches host semantics)
            current = Some(FileDiff {
                path: String::new(),
                add: 0,
                del: 0,
                lines: Vec::new(),
            });
            continue;
        }
        if let Some(b) = line.strip_prefix("+++ ") {
            if let Some(f) = current.as_mut() {
                if f.path.is_empty() {
                    // Strip b/ prefix like git does.
                    f.path = b.trim().strip_prefix("b/").unwrap_or(b.trim()).to_string();
                }
            }
            continue;
        }
        let Some(f) = current.as_mut() else { continue };
        if line.starts_with("@@") {
            f.lines.push(DiffLineKind::Hunk(line.to_string()));
        } else if let Some(body) = line.strip_prefix('+') {
            f.add += 1;
            f.lines.push(DiffLineKind::Add(body.to_string()));
        } else if let Some(body) = line.strip_prefix('-') {
            f.del += 1;
            f.lines.push(DiffLineKind::Del(body.to_string()));
        } else {
            let body = line.strip_prefix(' ').unwrap_or(line);
            f.lines.push(DiffLineKind::Context(body.to_string()));
        }
    }
    if let Some(f) = current.take() {
        files.push(f);
    }
    files
}

/// Render parsed files into transcript lines.
/// Collapsed: one summary row per file (`{path} +a −d`).
/// Expanded: header + colored body lines (capped, truncation note).
pub fn diff_lines(lang: Lang, files: &[FileDiff], verbose: bool) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for f in files {
        if !verbose {
            out.push(Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(
                    f.path.clone(),
                    Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  +{}", f.add),
                    Style::default().fg(Color::Green),
                ),
                Span::styled(format!(" −{}", f.del), Style::default().fg(Color::Red)),
            ]));
            continue;
        }
        out.push(Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(
                f.path.clone(),
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  +{}", f.add),
                Style::default().fg(Color::Green),
            ),
            Span::styled(format!(" −{}", f.del), Style::default().fg(Color::Red)),
        ]));
        let total = f.lines.len();
        for l in f.lines.iter().take(DIFF_MAX_BODY_LINES) {
            let (text, color) = match l {
                DiffLineKind::Add(t) => (format!("  +{t}"), Color::Green),
                DiffLineKind::Del(t) => (format!("  -{t}"), Color::Red),
                DiffLineKind::Hunk(t) => (format!("  {t}"), Color::Blue),
                DiffLineKind::Context(t) => (format!("   {t}"), Color::DarkGray),
            };
            out.push(Line::styled(text, Style::default().fg(color)));
        }
        if total > DIFF_MAX_BODY_LINES {
            out.push(Line::styled(
                i18n::fmt!(i18n::tr(lang, "diff.truncated"), total),
                Style::default().fg(Color::DarkGray),
            ));
        }
    }
    out
}

/// User-input echo cell(s): `› ` bold prefix on the FIRST line of the
/// (possibly multi-line) draft; continuation lines render plain.
pub fn user_echo_lines(text: &str) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (i, part) in text.split('\n').enumerate() {
        if i == 0 {
            lines.push(user_echo_line(part));
        } else {
            lines.push(Line::styled(
                part.to_string(),
                Style::default().fg(Color::White),
            ));
        }
    }
    lines
}

/// `fmt_elapsed_compact`: `0s` / `59s` / `1m 03s` / `2h 03m 09s`.
pub fn fmt_elapsed_compact(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h {:02}m {:02}s", secs / 3600, (secs % 3600) / 60, secs % 60)
    }
}

/// Braille spinner frames (`activity_indicator`, 100ms cadence).
pub const SPINNER_FRAMES: [&str; 10] = [
    "⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏",
];

pub fn spinner_frame(elapsed_ms: u128) -> &'static str {
    SPINNER_FRAMES[((elapsed_ms / 100) % SPINNER_FRAMES.len() as u128) as usize]
}

/// Render complete-block UiEvents into display lines. The loop assembles full
/// messages before logging, so no delta buffering is needed.
#[derive(Debug)]
pub struct EventRenderer {
    /// Language of event line text (fixed at construction; see [`EventRenderer::with_lang`]).
    lang: Lang,
}

impl EventRenderer {
    pub fn new() -> Self {
        Self::with_lang(i18n::current_lang())
    }

    /// Specify the language (tests use a fixed language to avoid machine locale).
    pub fn with_lang(lang: Lang) -> Self {
        Self { lang }
    }

    pub fn push(&mut self, ev: &UiEvent) -> Vec<Line<'static>> {
        let lang = self.lang;
        let mut out = Vec::new();
        match ev {
            UiEvent::SessionStarted { session_id, .. } => out.push(Line::styled(
                i18n::fmt!(i18n::tr(lang, "event.task_started"), session_id),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            // Between-turns marker (driver-claimed queue turns): no extra
            // transcript line — the app-state match re-arms the run state and
            // the follow-up's deltas render as usual.
            UiEvent::TaskStarted { .. } => {}
            UiEvent::AssistantText { text, .. } => {
                let line = text.clone();
                if !line.trim().is_empty() {
                    out.push(Line::from(line));
                }
            }
            UiEvent::ThinkingText { text, .. } => {
                let line = text.clone();
                if !line.trim().is_empty() {
                    out.push(Line::from(vec![Span::styled(
                        line,
                        Style::default().fg(Color::Gray).add_modifier(Modifier::DIM),
                    )]));
                }
            }
            // Deltas stream per token; the finished blocks render once the
            // message lands — the app's typewriter owns the live tail rows.
            UiEvent::ThinkingDelta { .. } | UiEvent::AssistantTextDelta { .. } => {}
            // ToolCall/ToolResult are driven by the app's active-cell
            // lifecycle (placeholder line flipped in place on completion —
 // `DynamicToolCallCell` behavior), not by this renderer.
            UiEvent::ToolCall { name, .. } => out.push(self.tool_calling(name)),
            UiEvent::ToolResult {
                ok, output, id, ..
            } => {
                // Fallback for results with no matching placeholder (e.g. a
                // resumed stream): standalone row without duration.
                let _ = id;
                out.push(self.tool_done("", *ok, output, false, 0));
            }
            UiEvent::Compaction { summary, .. } => {
                // Context compaction marker: one dim fold line (the summary
                // body stays inspectable in the session log).
                let head = summary.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
                out.push(Line::styled(
                    format!("🗜 {} …", truncate(head, 60)),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            UiEvent::TurnEnd { summary, .. } => out.push(Line::styled(
                i18n::fmt!(i18n::tr(lang, "event.task_done"), summary),
                Style::default().fg(Color::Green),
            )),
            UiEvent::TurnError { error, .. } => out.push(Line::styled(
                i18n::fmt!(i18n::tr(lang, "event.task_failed"), error),
                Style::default().fg(Color::Red),
            )),
            UiEvent::Cancelled { .. } => out.push(Line::styled(
                i18n::tr(lang, "event.task_cancelled"),
                Style::default().fg(Color::Yellow),
            )),
            UiEvent::ApprovalRequested {
                tool_name, params, ..
            } => {
                let detail = match params.as_deref() {
                    Some(p) if !p.trim().is_empty() => format!(" {}", truncate(p, 120)),
                    _ => String::new(),
                };
                out.push(Line::styled(
                    format!("❓ {tool_name} 需要审批{detail}"),
                    Style::default().fg(Color::Magenta),
                ))
            }
            UiEvent::Usage {
                input_tokens,
                output_tokens,
                reasoning_tokens,
                ..
            } => {
                let mut line = format!("⇅ tokens: in {input_tokens} / out {output_tokens}");
                if let Some(rt) = reasoning_tokens {
                    line.push_str(&format!(" (thinking {rt})"));
                }
                out.push(Line::styled(
                    line,
                    Style::default().fg(Color::Gray).add_modifier(Modifier::DIM),
                ))
            }
            UiEvent::SandboxModeChanged { mode, .. } => out.push(Line::styled(
                format!("🛡 sandbox: {mode}"),
                Style::default().fg(Color::Blue),
            )),
            UiEvent::InstructionsBudget {
                truncated,
                budget_bytes,
                composed_bytes,
                ..
            } => {
                if *truncated {
                    out.push(Line::styled(
                        format!(
                            "📎 instructions clipped to {budget_bytes} bytes (composed {composed_bytes})"
                        ),
                        Style::default().fg(Color::Yellow),
                    ));
                }
            }
            UiEvent::QuestionAsked { text, options, .. } => out.push(Line::styled(
                format!(
                    "❓ {text}{}",
                    if options.is_empty() {
                        String::new()
                    } else {
                        format!(" [{}]", options.join("/"))
                    }
                ),
                Style::default().fg(Color::Yellow),
            )),
            // Plan fold: the TUI renders the review notice read-
            // only; the interactive decision card lives in the desktop client.
            UiEvent::PlanReviewRequested { plan, .. } => {
                let head: String = plan.lines().next().unwrap_or("").to_string();
                out.push(Line::styled(
                    format!("📋 计划待评审: {head}"),
                    Style::default().fg(Color::Yellow),
                ))
            }
            UiEvent::PlanReviewDecided {
                approved, feedback, ..
            } => out.push(Line::styled(
                if *approved {
                    "📋 计划已批准，退出规划模式".to_string()
                } else {
                    format!(
                        "📋 计划未通过{}",
                        feedback
                            .as_deref()
                            .map(|f| format!(": {f}"))
                            .unwrap_or_default()
                    )
                },
                Style::default().fg(Color::Yellow),
            )),
            // Goal-loop state broadcasts (BUG-87): one status line so the
            // TUI shows the live budget too.
            UiEvent::GoalStatus {
                objective,
                status,
                tokens_used,
                token_budget,
                ..
            } => out.push(Line::styled(
                match token_budget {
                    Some(budget) if *tokens_used > 0 => {
                        format!("🎯 {objective} · {status} · {tokens_used}/{budget}")
                    }
                    Some(budget) => format!("🎯 {objective} · {status} · 0/{budget}"),
                    None => format!("🎯 {objective} · {status}"),
                },
                Style::default().fg(Color::Cyan),
            )),
            // 逐字节实时一致（开放回合快照流）：TUI 是同机富客户端，无需
            // 快照重渲染（事件流已含全部结构内容）。
            UiEvent::TraceSnapshot { .. } => {}
        }
        out
    }
}

impl EventRenderer {
    /// Placeholder line for a started tool call: `• Calling bash…` (the live
    /// row flips to [`EventRenderer::tool_done`] in place when the result
 /// lands — position stable, status/color changes, -style).
    pub fn tool_calling(&self, name: &str) -> Line<'static> {
        Line::styled(
            i18n::fmt!(i18n::tr(self.lang, "tool.calling"), "•", name),
            Style::default().fg(Color::Cyan),
        )
    }

    /// Completed row: `✓ bash · 87ms  {preview}` (green/red by outcome).
    pub fn tool_done(
        &self,
        name: &str,
        ok: bool,
        output: &str,
        verbose: bool,
        elapsed_ms: u128,
    ) -> Line<'static> {
        let (mark, color) = if ok {
            ("✓", Color::Green)
        } else {
            ("✗", Color::Red)
        };
        let cap = if verbose {
            VERBOSE_TOOL_OUTPUT
        } else {
            MAX_TOOL_OUTPUT
        };
        let body = truncate(output, cap);
        let head = i18n::fmt!(
            i18n::tr(self.lang, "tool.finished"),
            mark,
            name,
            elapsed_ms
        );
        let spans = if body.is_empty() {
            vec![Span::styled(head, Style::default().fg(color))]
        } else {
            vec![
                Span::styled(head, Style::default().fg(color)),
                Span::styled(
                    format!("  {body}"),
                    Style::default().fg(Color::DarkGray),
                ),
            ]
        };
        Line::from(spans)
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}…")
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use starriver_kernel::ui::UiEvent;

    #[test]
    fn assistant_text_renders_whole_block() {
        let mut r = EventRenderer::new();
        let lines = r.push(&UiEvent::AssistantText {
            version: UiEvent::VERSION,
            session_id: "s1".into(),
            text: "hello".into(),
        });
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].to_string(), "hello");
    }

    #[test]
    fn empty_assistant_text_emits_nothing() {
        let mut r = EventRenderer::new();
        assert!(r
            .push(&UiEvent::AssistantText {
                version: UiEvent::VERSION,
                session_id: "s1".into(),
                text: "   ".into(),
            })
            .is_empty());
    }

    #[test]
    fn thinking_is_dimmed() {
        let mut r = EventRenderer::new();
        let lines = r.push(&UiEvent::ThinkingText {
            version: UiEvent::VERSION,
            session_id: "s1".into(),
            text: "想".into(),
        });
        assert_eq!(lines.len(), 1);
        assert!(lines[0].to_string().contains("想"));
    }

    #[test]
    fn task_lifecycle_lines() {
        let mut r = EventRenderer::new();
        let started = r.push(&UiEvent::SessionStarted {
            version: UiEvent::VERSION,
            session_id: "s1".into(),
        });
        assert!(started[0].to_string().contains("▶"));
        let done = r.push(&UiEvent::TurnEnd {
            version: UiEvent::VERSION,
            session_id: "s1".into(),
            reason: "completed".into(),
            summary: "搞定".into(),
        });
        assert!(done[0].to_string().contains("✓"));
        let failed = r.push(&UiEvent::TurnError {
            version: UiEvent::VERSION,
            session_id: "s1".into(),
            error: "boom".into(),
        });
        assert!(failed[0].to_string().contains("✗"));
        let cancelled = r.push(&UiEvent::Cancelled {
            version: UiEvent::VERSION,
            session_id: "s1".into(),
        });
        assert!(cancelled[0].to_string().contains("⏹"));
    }

    #[test]
    fn tool_result_marks_and_truncation() {
        let mut r = EventRenderer::new();
        let ok = r.push(&UiEvent::ToolResult {
            version: UiEvent::VERSION,
            session_id: "s1".into(),
            id: "t1".into(),
            ok: true,
            output: "x".repeat(300),
            meta: None,
        });
        assert!(ok[0].to_string().contains("✓"));
        assert!(ok[0].to_string().contains('…'));
        let err = r.push(&UiEvent::ToolResult {
            version: UiEvent::VERSION,
            session_id: "s1".into(),
            id: "t1".into(),
            ok: false,
            output: "denied".into(),
            meta: None,
        });
        assert!(err[0].to_string().contains("✗"));
    }

    #[test]
    fn tool_call_line() {
        let mut r = EventRenderer::new();
        let lines = r.push(&UiEvent::ToolCall {
            version: UiEvent::VERSION,
            session_id: "s1".into(),
            id: "c1".into(),
            name: "bash".into(),
            input: None,
        });
 // -style active-cell placeholder: `• Calling bash`
        assert!(lines[0].to_string().contains("•"));
        assert!(lines[0].to_string().contains("bash"));
    }

    #[test]
    fn tool_done_line_carries_duration_and_mark() {
        let r = EventRenderer::new();
        let line = r.tool_done("bash", true, "ok", false, 87);
        let text = line.to_string();
        assert!(text.contains("✓"), "{text}");
        assert!(text.contains("bash"), "{text}");
        assert!(text.contains("87ms"), "{text}");
        assert!(text.contains("ok"), "{text}");
        let failed = r.tool_done("edit", false, "no match", true, 3);
        assert!(failed.to_string().contains("✗"));
    }

    #[test]
    fn elapsed_compact_formats() {
        assert_eq!(fmt_elapsed_compact(0), "0s");
        assert_eq!(fmt_elapsed_compact(59), "59s");
        assert_eq!(fmt_elapsed_compact(63), "1m 03s");
        assert_eq!(fmt_elapsed_compact(7389), "2h 03m 09s");
    }

    #[test]
    fn empty_tool_output_renders_mark_only() {
        let mut r = EventRenderer::new();
        let lines = r.push(&UiEvent::ToolResult {
            version: UiEvent::VERSION,
            session_id: "s1".into(),
            id: "t1".into(),
            ok: true,
            output: String::new(),
            meta: None,
        });
        assert_eq!(lines.len(), 1);
        assert!(lines[0].to_string().contains("✓"));
    }
}
