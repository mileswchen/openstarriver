use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Widget, Wrap};
use ratatui::Frame;

use crate::app::{model_variants, AppState, Mode, PickerKind};
use crate::commands;
use crate::i18n;

pub fn draw(frame: &mut Frame, app: &AppState) {
    draw_regions(frame.area(), frame.buffer_mut(), app);
}

/// Render one frame into an arbitrary buffer — the inline renderer (WI-80)
/// builds its live-tail Buffer with the exact same pipeline.
pub fn render_to_buffer(buf: &mut ratatui::buffer::Buffer, app: &AppState) {
    draw_regions(*buf.area(), buf, app);
}

fn draw_regions(area: ratatui::layout::Rect, buf: &mut ratatui::buffer::Buffer, app: &AppState) {
 // -style vertical stack: a slim 1-row header, the transcript
    // (flex), the command palette ABOVE the composer, the composer, then
    // transient status and the contextual hint footer.
    let transcript_h =
        if !app.transcript.is_empty() || matches!(app.mode, Mode::Picker(_) | Mode::Approval) {
            Constraint::Min(0)
        } else {
            Constraint::Length(0)
        };
    let status_h = if app.status.is_empty() { 0u16 } else { 1u16 };
    let hint_h = command_hint_height(app);
    // WI-78: the composer grows with a multi-line draft (max +2 rows; the
    // draft itself renders its last 3 lines).
    let composer_extra =
        (app.input_line_count().saturating_sub(1)).min(2) as u16;

    if app.mode == Mode::Transcript {
        // WI-81: the browser takes the whole screen — transcript + search bar.
        let chunks = Layout::vertical([
            Constraint::Length(1), // title row
            Constraint::Min(0),    // transcript
            Constraint::Length(1), // search bar / hints
        ])
        .margin(1)
        .split(area);
        draw_transcript_view(chunks[0], chunks[1], chunks[2], buf, app);
        return;
    }

    let chunks = Layout::vertical([
        Constraint::Length(1),      // slim header: brand · agent · model · sandbox
        transcript_h,               // transcript / picker
        Constraint::Length(hint_h), // slash-command palette (above the composer)
        Constraint::Length(3 + composer_extra), // composer (bordered textarea)
        Constraint::Length(status_h),
        Constraint::Length(1),      // contextual key hints + cwd
    ])
    .margin(1)
    .split(area);

    draw_header(chunks[0], buf, app);
    match app.mode {
        Mode::Picker(kind) => draw_picker(chunks[1], kind, buf, app),
        _ => {
            draw_output(chunks[1], buf, app);
        }
    }
    draw_command_hint(chunks[2], buf, app);
    draw_input(chunks[3], buf, app);
    draw_status(chunks[4], buf, app);
    draw_footer(chunks[5], buf, app);

    if app.mode == Mode::Approval {
        draw_approval(chunks[1], buf, app);
    }
}

fn model_name(app: &AppState) -> String {
    app.current_model.clone().unwrap_or_else(|| "-".into())
}

pub fn mode_label(app: &AppState) -> String {
    match app.mode {
        Mode::Idle => "Idle".into(),
        Mode::Running => "Running".into(),
        Mode::Picker(_) => app.t("mode.picker").into(),
        Mode::Approval => app.t("mode.approval").into(),
        Mode::Transcript => "Transcript".into(),
    }
}

/// Effective TOP line for the transcript paragraph under the -style
/// bottom-anchored model: `app.scroll` = rows above the newest line (0 =
/// follow), clamped against the viewport so an over-scroll parks at the top.
fn transcript_top(app: &AppState, viewport_rows: u16) -> usize {
    let total = app.transcript.len();
    let viewport = viewport_rows.max(1) as usize;
    let max_offset = total.saturating_sub(viewport);
    let offset = app.scroll.min(max_offset);
    total.saturating_sub(viewport).saturating_sub(offset)
}

/// Rows scrolled up past the newest line (drives the "back to bottom" pill).
fn scroll_offset(app: &AppState, viewport_rows: u16) -> usize {
    let total = app.transcript.len();
    let viewport = viewport_rows.max(1) as usize;
    app.scroll.min(total.saturating_sub(viewport))
}

/// Starriver Code brand LOGO (placeholder; replaceable later).
const LOGO: &str = "★";

fn draw_header(area: Rect, buf: &mut ratatui::buffer::Buffer, app: &AppState) {
 // One compact context row (keeps its chrome near the composer; we
    // keep identity here): `★ agent · model · sandbox · reasoning`.
    let agent = app.current_agent.as_ref();
    let name = agent.map(|a| a.name.as_str()).unwrap_or("-");
    let mode = agent_mode_label(app);
    let reasoning = reasoning_level(app);
    let sep = Span::styled(" · ", Style::default().fg(Color::DarkGray));
    let line = Line::from(vec![
        Span::styled(
            LOGO,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(name, Style::default().add_modifier(Modifier::BOLD)),
        sep.clone(),
        Span::styled(model_name(app), Style::default().fg(Color::Cyan)),
        sep.clone(),
        Span::styled(mode, Style::default().fg(Color::Green)),
        sep.clone(),
        Span::styled(reasoning, Style::default().fg(Color::DarkGray)),
    ]);
    (Paragraph::new(line)).render(area, buf);
}

fn mode_display(mode: &str) -> String {
    let mut chars = mode.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Display name of the current sandbox preset; defaults to workspace-write
/// until the first `sandbox/mode` event arrives.
fn agent_mode_label(app: &AppState) -> String {
    mode_display(&app.sandbox_mode)
}

fn reasoning_level(app: &AppState) -> String {
    let Some(model) = &app.current_model else {
        return "-".into();
    };
    let provider = model.split('/').next().unwrap_or("");
    app.model_entries
        .iter()
        .find(|e| e.id == provider)
        .and_then(|e| e.reasoning_effort.clone())
        .unwrap_or_else(|| "off".into())
}

fn draw_output(area: Rect, buf: &mut ratatui::buffer::Buffer, app: &AppState) {
    let top = transcript_top(app, area.height);
    let top_u16 = u16::try_from(top).unwrap_or(u16::MAX);
    let paragraph = Paragraph::new(app.transcript.clone())
        .wrap(Wrap { trim: false })
        .scroll((top_u16, 0));
    (paragraph).render(area, buf);

 // follow-control pill: when the user scrolled away from the tail,
    // float a "back to bottom" hint at the bottom-right of the transcript.
    let offset = scroll_offset(app, area.height);
    if offset > 0 && area.height >= 1 && area.width > 24 {
        let text = i18n::fmt!(app.t("hint.back_to_bottom"), offset);
        let width = text.chars().count() as u16 + 2;
        let pill = Rect::new(
            area.right().saturating_sub(width),
            area.bottom().saturating_sub(1),
            width.min(area.width),
            1,
        );
        (Clear).render(pill, buf);
        Paragraph::new(Line::styled(
            text,
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ))
        .render(pill, buf);
    }
}

fn draw_picker(area: Rect, kind: PickerKind, buf: &mut ratatui::buffer::Buffer, app: &AppState) {
    let (title, items): (String, Vec<ListItem>) = match kind {
        PickerKind::Sessions => (
            app.t("picker.sessions.title").into(),
            app.sessions
                .iter()
                .map(|s| {
                    // Branch sessions (with parent_id) get an indent marker to show the parent-child hierarchy.
                    let marker = if s.parent_id.is_some() { "  ↳ " } else { "" };
                    ListItem::new(Line::from(format!(
                        "{marker}{}  ({} msgs)  {}",
                        s.title, s.message_count, s.created_at
                    )))
                })
                .collect(),
        ),
        PickerKind::Workspaces => (
            app.t("picker.workspaces.title").into(),
            app.workspaces
                .iter()
                .map(|p| ListItem::new(Line::from(format!("{}  {}", p.title, p.path))))
                .collect(),
        ),
        PickerKind::Models => {
            if let Some(pid) = &app.model_picker_provider {
                let name = app
                    .model_entries
                    .iter()
                    .find(|p| &p.id == pid)
                    .map(|p| p.name.as_str())
                    .unwrap_or(pid);
                (
                    i18n::fmt!(app.t("picker.models_for.title"), name),
                    app.model_entries
                        .iter()
                        .find(|p| &p.id == pid)
                        .map(|p| {
                            model_variants(p)
                                .into_iter()
                                .map(|m| ListItem::new(Line::from(m)))
                                .collect()
                        })
                        .unwrap_or_default(),
                )
            } else {
                (
                    app.t("picker.providers.title").into(),
                    app.model_entries
                        .iter()
                        .map(|p| {
                            let count =
                                i18n::fmt!(app.t("picker.model_count"), model_variants(p).len());
                            ListItem::new(Line::from(format!("{}  ({count})", p.name)))
                        })
                        .collect(),
                )
            }
        }
        // Backtrack picker: list user messages with a truncated preview (selecting forks before that message).
        PickerKind::Messages => (
            app.t("picker.messages.title").into(),
            app.message_timeline
                .iter()
                .enumerate()
                .map(|(i, m)| {
                    let preview: String = m.content.chars().take(32).collect();
                    ListItem::new(Line::from(format!("[{}]  {}", i + 1, preview)))
                })
                .collect(),
        ),
        // Archive-restore picker: list archived sessions (with an archived status marker).
        PickerKind::Archived => (
            app.t("picker.archived.title").into(),
            app.archived_sessions
                .iter()
                .map(|s| {
                    ListItem::new(Line::from(format!(
                        "📦 {}  ({} msgs)",
                        s.title, s.message_count
                    )))
                })
                .collect(),
        ),
        // Two-level sandbox preset picker: level 1 = execution presets (with a
        // dim description line), level 2 = the per-preset approval override.
        PickerKind::Presets => {
            if let Some(pid) = &app.preset_approval_for {
                match crate::preset::find(pid) {
                    Some(p) => {
                        let title = i18n::fmt!(
                            app.t("picker.preset_approval.title"),
                            crate::preset::preset_name(&p, app.lang)
                        );
                        let items = crate::preset::approval_options(&p)
                            .into_iter()
                            .map(|o| {
                                ListItem::new(Line::from(crate::preset::approval_label(
                                    o, &p, app.lang,
                                )))
                            })
                            .collect();
                        (title, items)
                    }
                    None => (app.t("picker.presets.title").into(), Vec::new()),
                }
            } else {
                let items = crate::preset::presets()
                    .iter()
                    .map(|p| {
                        let mut lines = vec![Line::from(crate::preset::row_label(
                            p,
                            app.lang,
                            p.id == app.execution_mode,
                        ))];
                        lines.push(Line::styled(
                            i18n::tr(app.lang, p.desc_key),
                            Style::default().fg(Color::DarkGray),
                        ));
                        ListItem::new(lines)
                    })
                    .collect();
                (app.t("picker.presets.title").into(), items)
            }
        }
    };
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");
    let mut state = ListState::default();
    let len = match kind {
        PickerKind::Sessions => app.sessions.len(),
        PickerKind::Workspaces => app.workspaces.len(),
        PickerKind::Models => {
            if app.model_picker_provider.is_some() {
                app.model_entries
                    .iter()
                    .find(|p| Some(&p.id) == app.model_picker_provider.as_ref())
                    .map(|p| model_variants(p).len())
                    .unwrap_or(0)
            } else {
                app.model_entries.len()
            }
        }
        PickerKind::Messages => app.message_timeline.len(),
        PickerKind::Archived => app.archived_sessions.len(),
        PickerKind::Presets => match &app.preset_approval_for {
            Some(pid) => crate::preset::find(pid)
                .map(|p| crate::preset::approval_options(&p).len())
                .unwrap_or(0),
            None => crate::preset::presets().len(),
        },
    };
    let sel = if len == 0 {
        None
    } else {
        Some(app.selected.min(len - 1))
    };
    state.select(sel);
    ratatui::widgets::StatefulWidget::render(list, area, buf, &mut state);
}

/// WI-81: full-screen transcript browser. Matched lines tint yellow; the
/// current match gets a `> ` marker; the bottom bar carries the search
/// query (while typing) or the key hints.
fn draw_transcript_view(
    title_area: Rect,
    area: Rect,
    bar_area: Rect,
    buf: &mut ratatui::buffer::Buffer,
    app: &AppState,
) {
    let matches = app.transcript_matches();
    let current = app
        .transcript_match
        .and_then(|i| matches.get(i).copied());
    let view: Vec<Line> = app
        .transcript
        .iter()
        .enumerate()
        .map(|(i, l)| {
            if Some(i) == current {
                let mut spans = vec![Span::styled(
                    "> ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )];
                spans.extend(l.spans.iter().cloned());
                Line::from(spans)
            } else if matches.contains(&i) {
                let mut spans = vec![Span::raw("  ")];
                spans.extend(l.spans.iter().cloned());
                Line::from(spans).patch_style(Style::default().fg(Color::Yellow))
            } else {
                let mut l2 = l.clone();
                l2.spans.insert(0, Span::raw("  "));
                l2
            }
        })
        .collect();
    // Bottom-anchored math on the real transcript (view is 1:1 with it).
    let top = transcript_top(app, area.height);
    let top_u16 = u16::try_from(top).unwrap_or(u16::MAX);
    let paragraph = Paragraph::new(view)
        .wrap(Wrap { trim: false })
        .scroll((top_u16, 0));
    (paragraph).render(area, buf);

    // Title row: transcript browser + match counter.
    let title = if app.transcript_search.is_some() {
        format!(
            "📜 {} ({}/{})",
            "transcript",
            current.map(|c| matches.iter().position(|&m| m == c).unwrap_or(0) + 1)
                .unwrap_or(0),
            matches.len()
        )
    } else {
        "📜 transcript".to_string()
    };
    Paragraph::new(Line::styled(
        title,
        Style::default().add_modifier(Modifier::BOLD),
    ))
    .render(title_area, buf);

    // Bottom bar: query while searching, hints otherwise.
    let bar = match &app.transcript_search {
        Some(q) => Line::from(vec![
            Span::styled("/", Style::default().fg(Color::Cyan)),
            Span::styled(q.clone(), Style::default().fg(Color::White)),
            Span::styled("▌", Style::default().fg(Color::White)),
        ]),
        None => Line::styled(
            app.t("transcript.hint"),
            Style::default().fg(Color::DarkGray),
        ),
    };
    (Paragraph::new(bar)).render(bar_area, buf);
}

fn draw_status(area: Rect, buf: &mut ratatui::buffer::Buffer, app: &AppState) {
    let style = match app.mode {
        Mode::Running => Style::default().fg(Color::Cyan),
        Mode::Approval => Style::default().fg(Color::Magenta),
        _ => Style::default(),
    };
    let status = Line::styled(&app.status, style);
    (Paragraph::new(status)).render(area, buf);
}

/// Command hint area height: equals the number of displayed command list rows.
fn command_hint_height(app: &AppState) -> u16 {
    match commands::hint_docs(&app.input, app.lang) {
        Some((_, shown)) => shown.len() as u16,
        None => 0,
    }
}

/// When input starts with `/` or `\`, list command name + brief description below the input box;
/// the up/down-selected row is fully highlighted (command name Yellow+BOLD, description Yellow),
/// while other command names are Cyan and descriptions Gray.
fn draw_command_hint(area: Rect, buf: &mut ratatui::buffer::Buffer, app: &AppState) {
    let Some((prefix, shown)) = commands::hint_docs(&app.input, app.lang) else {
        return;
    };
    let sel = app.cmd_hint_index.min(shown.len().saturating_sub(1));
    // Two-column layout: the first column (command name, including arg placeholders) is fixed-width left-aligned,
    // and the description column follows left-aligned. Command names are ASCII, so `{:<w$}` char padding is the cell width.
    let cell_w = shown.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
    let lines: Vec<Line> = shown
        .iter()
        .enumerate()
        .map(|(i, (n, desc))| {
            let selected = i == sel;
            Line::from(vec![
                Span::styled(
                    format!("{prefix}{n:<cell_w$}  "),
                    if selected {
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Cyan)
                    },
                ),
                Span::styled(
                    desc.clone(),
                    if selected {
                        Style::default().fg(Color::Yellow)
                    } else {
                        Style::default().fg(Color::Gray)
                    },
                ),
            ])
        })
        .collect();
    (Paragraph::new(lines)).render(area, buf);
}

/// Composer title: identity while idle, live spinner + elapsed + interrupt
/// hint while a turn is running (`StatusIndicatorWidget` placement).
fn composer_title(app: &AppState) -> String {
    if app.mode == Mode::Running || app.mode == Mode::Approval {
        if let Some(started) = app.run_started_at {
            let frame_char = crate::events::spinner_frame(
                started.elapsed().as_millis(),
            );
            let elapsed = crate::events::fmt_elapsed_compact(started.elapsed().as_secs());
            return i18n::fmt!(
                app.t("spinner.working"),
                frame_char,
                format!("{elapsed} · {}", app.t("spinner.hint"))
            );
        }
    }
    // Idle identity chip: `★ mode · model` (the agent name lives in the
    // slim header; repeating it here would be redundant).
    let mode = agent_mode_label(app);
    let usage = i18n::fmt!(app.t("usage.summary"), app.usage_in, app.usage_out);
    format!("★ {} · {} · {}", mode, model_name(app), usage)
}

fn draw_input(area: Rect, buf: &mut ratatui::buffer::Buffer, app: &AppState) {
 // Cursor rendering (TextArea parity): a `▌` block sits at the char
    // cursor; an empty idle composer shows the dim placeholder. A multi-line
    // draft renders its LAST 3 lines (scroll-with-tail), `L{n}` badge in the
    // title.
    let chars: Vec<char> = app.input.chars().collect();
    let cursor = app.input_cursor.min(chars.len());
    let mut lines: Vec<Line> = Vec::new();
    if chars.is_empty() {
        let text = if app.mode == Mode::Idle {
            app.t("input.placeholder").to_string()
        } else {
            String::new()
        };
        let style = if app.mode == Mode::Idle {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default()
        };
        lines.push(Line::styled(text, style));
    } else {
        // Build per-line strings with the cursor block injected at the char
        // cursor position (cursor counts across newlines).
        let mut current = String::new();
        let mut rendered_lines: Vec<String> = Vec::new();
        let mut cursor_line: Option<usize> = None;
        let mut cursor_col = 0usize;
        for (i, c) in chars.iter().enumerate() {
            if i == cursor {
                cursor_line = Some(rendered_lines.len());
                cursor_col = current.chars().count();
                current.push('▌');
            }
            if *c == '\n' {
                rendered_lines.push(std::mem::take(&mut current));
            } else {
                current.push(*c);
            }
        }
        if cursor_line.is_none() {
            cursor_line = Some(rendered_lines.len());
            cursor_col = current.chars().count();
            current.push('▌');
        }
        rendered_lines.push(current);
        let _ = cursor_col;
        // Show at most the last 3 lines (older lines scroll out).
        let keep = 3;
        let start = rendered_lines.len().saturating_sub(keep);
        let cursor_visible = cursor_line.is_some_and(|l| l >= start);
        for (i, text) in rendered_lines.iter().enumerate().skip(start) {
            let style = if cursor_visible && Some(i) == cursor_line {
                Style::default().fg(Color::White)
            } else {
                Style::default().fg(Color::White)
            };
            lines.push(Line::styled(text.clone(), style));
        }
    }
    // Title carries identity/spinner; multi-line adds the L{n} badge.
    let mut title = composer_title(app);
    if app.is_multiline_input() {
        title = format!("{title} · L{}", app.input_line_count());
    }
    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(if app.mode == Mode::Running {
                    Style::default().fg(Color::Cyan)
                } else {
                    Style::default().fg(Color::Gray)
                })
                .title(title),
        )
        .wrap(Wrap { trim: false });
    (paragraph).render(area, buf);
}

/// Contextual key-hint footer (`footer.rs`): per-mode hints + cwd.
fn draw_footer(area: Rect, buf: &mut ratatui::buffer::Buffer, app: &AppState) {
    let hint = match app.mode {
        Mode::Running | Mode::Approval => app.t("hint.running"),
        _ => app.t("hint.idle"),
    };
    let cwd = app
        .current_workspace
        .as_ref()
        .map(|p| p.path.as_str())
        .unwrap_or("-");

    // WI-77: context chip — color escalates at 80% / 95% fill. Width-tight
    // terminals drop it first (hints + cwd stay).
    let ctx_span = if app.resume_session_id.is_some() {
        let pct = app.context_used_percent();
        let color = if pct >= 95 {
            Color::Red
        } else if pct >= 80 {
            Color::Yellow
        } else {
            Color::DarkGray
        };
        Some(Span::styled(
            i18n::fmt!(app.t("usage.ctx_left"), pct, app.context_left()),
            Style::default().fg(color),
        ))
    } else {
        None
    };

    let mut spans: Vec<Span> = Vec::new();
    if area.width >= 60 {
        if let Some(ctx) = ctx_span {
            spans.push(ctx);
            spans.push(Span::styled("  ·  ", Style::default().fg(Color::DarkGray)));
        }
    }
    spans.push(Span::styled(hint, Style::default().fg(Color::DarkGray)));
    spans.push(Span::styled("  ·  ", Style::default().fg(Color::DarkGray)));
    spans.push(Span::styled(cwd, Style::default().fg(Color::DarkGray)));
    (Paragraph::new(Line::from(spans))).render(area, buf);
}

fn draw_approval(area: Rect, buf: &mut ratatui::buffer::Buffer, app: &AppState) {
    let Some(a) = &app.approval else {
        return;
    };
    let w = area.width.saturating_mul(3) / 5;
    let h = 7u16.min(area.height);
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let rect = Rect::new(x, y, w, h);

    (Clear).render(rect, buf);
    // /delete's second confirmation reuses the overlay: no tool row, only confirmation text + key hints.
    let text = if a.tool == "delete-session" {
        vec![
            Line::from(i18n::fmt!(app.t("approval.reason"), a.reason)),
            Line::from(""),
            Line::from(app.t("status.delete_confirm")),
        ]
    } else {
        let remember = if a.remember { "on" } else { "off" };
        vec![
            Line::from(i18n::fmt!(app.t("approval.tool"), a.tool)),
            Line::from(i18n::fmt!(app.t("approval.reason"), a.reason)),
            Line::from(""),
            Line::from(i18n::fmt!(app.t("approval.actions"), remember)),
        ]
    };
    let paragraph = Paragraph::new(text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(app.t("approval.title")),
        )
        .wrap(Wrap { trim: false });
    (paragraph).render(rect, buf);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::AppState;
    use crate::host::mock::MockHost;

    #[test]
    fn mode_labels() {
        let mut app =
            AppState::with_lang(Box::new(MockHost::default()), crate::i18n::Lang::Zh).unwrap();
        assert_eq!(mode_label(&app), "Idle");
        app.mode = Mode::Running;
        assert_eq!(mode_label(&app), "Running");
        app.mode = Mode::Picker(PickerKind::Sessions);
        assert_eq!(mode_label(&app), "选择");
        app.mode = Mode::Approval;
        assert_eq!(mode_label(&app), "审批");
    }

    #[test]
    fn scroll_anchors_to_bottom_and_follows() {
        let mut app = AppState::new(Box::new(MockHost::default())).unwrap();
        for i in 0..50 {
            app.transcript.push(Line::from(format!("l{i}")));
        }
        // 0 = follow the tail: the top line shows the LAST viewport rows.
        assert_eq!(transcript_top(&app, 10), 40);
        // Scrolled 5 rows up: the viewport shifts up by 5.
        app.scroll = 5;
        assert_eq!(transcript_top(&app, 10), 35);
        assert_eq!(scroll_offset(&app, 10), 5);
        // Over-scroll parks at the top (offset clamps to total-viewport).
        app.scroll = usize::MAX;
        assert_eq!(transcript_top(&app, 10), 0);
    }

    #[test]
    fn short_transcript_never_scrolls_negative() {
        let mut app = AppState::new(Box::new(MockHost::default())).unwrap();
        app.transcript.push(Line::from("a"));
        app.scroll = 9;
        assert_eq!(transcript_top(&app, 10), 0);
        assert_eq!(scroll_offset(&app, 10), 0);
    }

    #[test]
    fn composer_title_switches_by_mode() {
        let mut app =
            AppState::with_lang(Box::new(MockHost::default()), crate::i18n::Lang::Zh).unwrap();
        assert!(composer_title(&app).contains("模型") || composer_title(&app).contains("·"));
        app.mode = Mode::Running;
        app.run_started_at = Some(std::time::Instant::now());
        let title = composer_title(&app);
        assert!(title.contains("0s"), "elapsed shows immediately: {title}");
        assert!(title.contains("工作中"), "{title}");
    }

    #[test]
    fn spinner_frames_rotate() {
        let a = crate::events::spinner_frame(0);
        let b = crate::events::spinner_frame(100);
        assert_ne!(a, b);
    }
}
