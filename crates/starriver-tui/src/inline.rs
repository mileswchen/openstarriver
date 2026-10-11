//! WI-80: inline scrollback rendering. The TUI docks at the bottom of the
//! terminal's NORMAL screen (no alternate screen): finished transcript
//! lines are printed into the native scrollback — where the terminal's own
//! scroll/copy/search work — and only the live tail (active tool rows,
//! spinner, composer, status) is redrawn each frame.
//!
//! Frame protocol (all cursor movement is RELATIVE, so it survives the
//! scrolling the commits themselves cause):
//! 1. `MoveUp(prev_live_h - 1)` + `MoveToColumn(0)` — park at the live
//!    area's top-left (the previous frame left the cursor at the end of its
//!    last row).
//! 2. `Clear(FromCursorDown)` — wipe the old live area.
//! 3. Print any committed lines (each + CRLF) — they flow into scrollback.
//! 4. Print the new live rows (all but the last + CRLF) — the cursor parks
//!    at the end of the last row, ready for the next frame's step 1.

use std::io::Write;

use crossterm::cursor::{MoveToColumn, MoveUp};
use crossterm::style::{
    Attribute as CAttribute, Color as CColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
};
use crossterm::terminal::{Clear as TClear, ClearType};
use ratatui::buffer::{Buffer, Cell};
use ratatui::style::{Color, Modifier};
use ratatui::text::Line;

use crate::app::{AppState, Mode};
use crate::ui;

pub struct InlineRenderer {
    prev_live_h: u16,
}

impl InlineRenderer {
    pub fn new() -> Self {
        Self { prev_live_h: 1 }
    }

    /// Height of the live tail for the current state. The transcript window
    /// is modest while running (it is the ACTIVE tail — full history lives
    /// in the scrollback and in the Ctrl+T browser); overlays get more room.
    pub fn live_height(app: &AppState, screen_h: u16, live_rows: usize) -> u16 {
        let chrome = 2 /* margins */ + 1 /* header */
            + command_hint_height(app)
            + 3 + composer_extra(app)
            + u16::from(!app.status.is_empty())
            + 1 /* footer */;
        let transcript_rows = match app.mode {
            Mode::Picker(_) | Mode::Approval | Mode::Transcript => live_rows.min(24),
            _ => live_rows.min(if app.mode == Mode::Running { 12 } else { 6 }),
        };
        (chrome + transcript_rows as u16 + 2).min(screen_h.saturating_sub(1).max(1))
    }

    /// One frame: flush committed lines, then paint the live area.
    pub fn frame(
        &mut self,
        out: &mut impl Write,
        buf: &Buffer,
        committed: &[Line<'static>],
    ) -> std::io::Result<()> {
        // 1) Park at the previous frame's top-left.
        let up = self.prev_live_h.saturating_sub(1);
        if up > 0 {
            crossterm::queue!(out, MoveUp(up))?;
        }
        crossterm::queue!(out, MoveToColumn(0))?;
        // 2) Wipe the old live area.
        crossterm::queue!(out, TClear(ClearType::FromCursorDown))?;
        // 3) Committed lines flow into the native scrollback.
        for line in committed {
            let ansi = line_to_ansi(line);
            crossterm::queue!(out, crossterm::style::Print(ansi), crossterm::style::Print("\r\n"))?;
        }
        // 4) Live rows; the last row carries no trailing newline so the
        //    cursor parks at its end (the next frame's anchor).
        let rows = buffer_rows(buf);
        for (i, row) in rows.iter().enumerate() {
            crossterm::queue!(out, crossterm::style::Print(row))?;
            if i + 1 < rows.len() {
                crossterm::queue!(out, crossterm::style::Print("\r\n"))?;
            }
        }
        out.flush()?;
        // BUG-94: the anchor must count exactly the rows emitted above —
        // every row is guaranteed to fit the terminal width (see
        // `buffer_rows`), so the live area occupies `rows.len()` physical
        // rows and the next frame's `MoveUp` lands on its true top.
        self.prev_live_h = (rows.len() as u16).max(1);
        Ok(())
    }

    /// Tear-down: clear the live area and leave the cursor on a fresh line.
    pub fn finish(&mut self, out: &mut impl Write) -> std::io::Result<()> {
        let up = self.prev_live_h.saturating_sub(1);
        if up > 0 {
            crossterm::queue!(out, MoveUp(up))?;
        }
        crossterm::queue!(
            out,
            MoveToColumn(0),
            TClear(ClearType::FromCursorDown),
            crossterm::style::Print("\r\n")
        )?;
        out.flush()
    }
}

impl Default for InlineRenderer {
    fn default() -> Self {
        Self::new()
    }
}

/// Render the live tail for the current app state into a fresh buffer.
pub fn render_live(app: &AppState, width: u16, live_h: u16) -> Buffer {
    let mut buf = Buffer::empty(ratatui::layout::Rect::new(0, 0, width, live_h));
    ui::render_to_buffer(&mut buf, app);
    buf
}

fn command_hint_height(app: &AppState) -> u16 {
    crate::commands::hint_docs(&app.input, app.lang)
        .map(|(_, shown)| shown.len() as u16)
        .unwrap_or(0)
}

fn composer_extra(app: &AppState) -> u16 {
    (app.input_line_count().saturating_sub(1)).min(2) as u16
}

/// Convert a styled Line into an ANSI string (colors survive the flush into
/// scrollback).
fn line_to_ansi(line: &Line<'_>) -> String {
    let mut out = String::new();
    let mut cur = Cell::default();
    for span in &line.spans {
        let style = span.style;
        emit_style(&mut out, &cur, style.fg, style.bg, style.add_modifier, style.sub_modifier);
        out.push_str(&span.content);
        cur.set_style(style);
    }
    out.push_str("\x1b[0m");
    out
}

/// Convert the buffer into one ANSI string per row.
///
/// BUG-94 invariant: every emitted row's DISPLAY width must be ≤ the buffer
/// width (== the terminal width). A wide grapheme occupies one ratatui cell
/// but renders two terminal columns, and some rows also pack wide symbols
/// without skipped follower cells — naive concatenation overflows the width,
/// the terminal wraps those rows, and the frame protocol's relative anchor
/// drifts (the live area then leaks its top rows into the scrollback every
/// frame). So each row is emitted under a display-width budget: a grapheme
/// that no longer fits is dropped entirely, and trailing padding is trimmed
/// so typical rows never even approach the auto-wrap boundary.
pub fn buffer_rows(buf: &Buffer) -> Vec<String> {
    let width = buf.area.width as usize;
    let mut rows = Vec::with_capacity(buf.area.height as usize);
    for y in 0..buf.area.height {
        let mut row = String::new();
        let mut cur = Cell::default();
        let mut first = true;
        let mut cols = 0usize;
        // Byte offset just past the last non-space cell — everything after
        // is padding and gets trimmed before the closing reset.
        let mut content_end = 0usize;
        for x in 0..buf.area.width {
            let Some(cell) = buf.cell((x, y)) else {
                break;
            };
            let sym = cell.symbol();
            let sym_w = unicode_width::UnicodeWidthStr::width(sym);
            if sym_w == 0 {
                // Skipped follower of a wide cell (or empty filler).
                continue;
            }
            if cols + sym_w > width {
                break;
            }
            if first {
                // Start every row from a reset baseline.
                row.push_str("\x1b[0m");
                cur = Cell::default();
                first = false;
            }
            if cell.modifier != cur.modifier
                || cell.fg != cur.fg
                || cell.bg != cur.bg
            {
                emit_style(&mut row, &cur, Some(cell.fg), Some(cell.bg), cell.modifier, Modifier::empty());
                cur = Cell::default();
                cur.modifier = cell.modifier;
                cur.fg = cell.fg;
                cur.bg = cell.bg;
            }
            row.push_str(sym);
            cols += sym_w;
            if sym != " " {
                content_end = row.len();
            }
        }
        row.truncate(content_end);
        row.push_str("\x1b[0m");
        rows.push(row);
    }
    rows
}

fn emit_style(
    out: &mut String,
    cur: &Cell,
    fg: Option<Color>,
    bg: Option<Color>,
    add: Modifier,
    _sub: Modifier,
) {
    let reset_needed = cur.modifier != Modifier::empty()
        || cur.fg != Color::Reset
        || cur.bg != Color::Reset;
    if reset_needed {
        out.push_str("\x1b[0m");
    }
    if let Some(fg) = fg {
        if fg != Color::Reset {
            out.push_str(&SetForegroundColor(to_crossterm(fg)).to_string());
        }
    }
    if let Some(bg) = bg {
        if bg != Color::Reset {
            out.push_str(&SetBackgroundColor(to_crossterm(bg)).to_string());
        }
    }
    if add.contains(Modifier::BOLD) {
        out.push_str(&SetAttribute(CAttribute::Bold).to_string());
    }
    if add.contains(Modifier::DIM) {
        out.push_str(&SetAttribute(CAttribute::Dim).to_string());
    }
    if add.contains(Modifier::ITALIC) {
        out.push_str(&SetAttribute(CAttribute::Italic).to_string());
    }
    if add.contains(Modifier::UNDERLINED) {
        out.push_str(&SetAttribute(CAttribute::Underlined).to_string());
    }
}

fn to_crossterm(c: Color) -> CColor {
    match c {
        Color::Reset => CColor::Reset,
        Color::Black => CColor::Black,
        Color::Red => CColor::DarkRed,
        Color::Green => CColor::DarkGreen,
        Color::Yellow => CColor::DarkYellow,
        Color::Blue => CColor::DarkBlue,
        Color::Magenta => CColor::DarkMagenta,
        Color::Cyan => CColor::DarkCyan,
        Color::Gray => CColor::Grey,
        Color::DarkGray => CColor::DarkGrey,
        Color::LightRed => CColor::Red,
        Color::LightGreen => CColor::Green,
        Color::LightYellow => CColor::Yellow,
        Color::LightBlue => CColor::Blue,
        Color::LightMagenta => CColor::Magenta,
        Color::LightCyan => CColor::Cyan,
        Color::White => CColor::White,
        Color::Indexed(i) => CColor::AnsiValue(i),
        Color::Rgb(r, g, b) => CColor::Rgb { r, g, b },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::mock::MockHost;
    use crate::i18n::Lang;
    use ratatui::style::{Style, Stylize};
    use ratatui::text::{Line, Span};

    fn app() -> AppState {
        AppState::with_lang(Box::new(MockHost::default()), Lang::Zh).unwrap()
    }

    #[test]
    fn buffer_rows_emit_color_escapes() {
        let mut buf = Buffer::empty(ratatui::layout::Rect::new(0, 0, 10, 2));
        ratatui::widgets::Widget::render(
            ratatui::widgets::Paragraph::new(Line::from(vec![
                Span::styled("ok", Style::default().fg(Color::Green)),
                Span::raw(" plain"),
            ])),
            buf.area,
            &mut buf,
        );
        let rows = buffer_rows(&buf);
        assert_eq!(rows.len(), 2);
        assert!(rows[0].contains("ok"));
        assert!(rows[0].contains("\x1b[38;5;2m"), "green fg escape: {:?}", rows[0]);
        assert!(rows[0].ends_with("\x1b[0m"));
    }

    /// Visible display width of an emitted row: ANSI escapes stripped, then
    /// summed over grapheme clusters (wide chars count 2).
    fn row_display_width(row: &str) -> usize {
        let plain: String = row
            .split("\x1b[")
            .filter(|s| !s.is_empty())
            .map(|s| match s.find('m') {
                Some(i) => &s[i + 1..],
                None => s,
            })
            .collect();
        unicode_width::UnicodeWidthStr::width(plain.as_str())
    }

    #[test]
    fn buffer_rows_trims_trailing_padding() {
        let mut buf = Buffer::empty(ratatui::layout::Rect::new(0, 0, 20, 1));
        ratatui::widgets::Widget::render(
            ratatui::widgets::Paragraph::new("hi"),
            buf.area,
            &mut buf,
        );
        let rows = buffer_rows(&buf);
        assert_eq!(rows.len(), 1);
        let body = rows[0].trim_end_matches("\x1b[0m");
        assert!(!body.ends_with(' '), "padding trimmed: {}", rows[0]);
        assert_eq!(row_display_width(&rows[0]), 2);
    }

    #[test]
    fn buffer_rows_hard_truncates_wide_cells_to_width() {
        // Pathological row: every cell holds a wide symbol with NO skipped
        // follower — naive concatenation would emit 20 columns into a
        // 10-column terminal (the BUG-94 overflow shape).
        let mut buf = Buffer::empty(ratatui::layout::Rect::new(0, 0, 10, 1));
        for x in 0..10 {
            if let Some(cell) = buf.cell_mut((x, 0)) {
                cell.set_symbol("输");
            }
        }
        let rows = buffer_rows(&buf);
        assert_eq!(rows.len(), 1);
        assert_eq!(row_display_width(&rows[0]), 10, "clamped to width");
        // 5 wide chars survived, not 10.
        assert_eq!(rows[0].matches('输').count(), 5);
    }

    #[test]
    fn live_frames_fit_terminal_width_zh_ui() {
        // BUG-94 regression: the idle Chinese-locale live frame used to emit
        // rows 87/90 columns wide into an 80-column terminal; wrapped rows
        // drift the frame protocol's anchor and flood the scrollback.
        let a = app();
        let live_rows = a.visible_transcript(false).len();
        let live_h = InlineRenderer::live_height(&a, 24, live_rows);
        let buf = render_live(&a, 80, live_h);
        let rows = buffer_rows(&buf);
        assert_eq!(rows.len(), live_h as usize, "one emitted row per buffer row");
        for (i, row) in rows.iter().enumerate() {
            let w = row_display_width(row);
            assert!(w <= 80, "row {i} overflows terminal width: {w} |{row:?}");
        }
    }

    #[test]
    fn frame_anchor_counts_emitted_rows() {
        let mut renderer = InlineRenderer::new();
        let buf = Buffer::empty(ratatui::layout::Rect::new(0, 0, 10, 4));
        let mut sink: Vec<u8> = Vec::new();
        renderer.frame(&mut sink, &buf, &[]).unwrap();
        assert_eq!(renderer.prev_live_h, 4);
        // The next frame walks back exactly the emitted row count before
        // wiping the live area.
        renderer.frame(&mut sink, &buf, &[]).unwrap();
        assert!(std::str::from_utf8(&sink).unwrap().contains("\x1b[3A"));
    }

    #[test]
    fn line_to_ansi_carries_style_then_resets() {
        let line = Line::from(vec![
            Span::styled("A", Style::default().fg(Color::Red).bold()),
            Span::raw("b"),
        ]);
        let ansi = line_to_ansi(&line);
        assert!(ansi.contains('A') && ansi.contains('b'));
        assert!(ansi.contains("\x1b[38;5;1m"), "red: {ansi:?}");
        assert!(ansi.contains("\x1b[1m"), "bold: {ansi:?}");
        assert!(ansi.ends_with("\x1b[0m"));
    }

    #[test]
    fn live_height_clamps_to_screen() {
        let a = app();
        assert!(InlineRenderer::live_height(&a, 10, 100) <= 9);
        assert!(InlineRenderer::live_height(&a, 24, 0) >= 5);
    }

    #[test]
    fn flush_policy_idle_flushes_all_running_keeps_active_rows() {
        let mock = MockHost::default();
        let tx = mock.with_events(32);
        let mut a = app_with_host(mock);
        a.set_input("go".into());
        a.submit().unwrap();
        tx.send(serde_json::json!({"type":"assistantText","sessionId":"s1","text":"hi"})).unwrap();
        tx.send(serde_json::json!({"type":"toolCall","sessionId":"s1","id":"t1","name":"grep"})).unwrap();
        tx.send(serde_json::json!({"type":"toolResult","sessionId":"s1","id":"t1","ok":true,"output":"x"})).unwrap();
        tx.send(serde_json::json!({"type":"toolCall","sessionId":"s1","id":"t2","name":"grep"})).unwrap();
        a.tick().unwrap();
        // Running with an in-flight placeholder at some line L: nothing at
        // or after L flushes; earlier rows beyond the window do.
        let flushed = a.take_committed(2);
        assert!(flushed.len() <= 3, "window keeps rows: {}", flushed.len());
        let placeholder_line = a.running_tools.get("t2").map(|r| r.line).unwrap();
        assert!(a.committed_upto <= placeholder_line);
        // Quiescent: everything flushes.
        tx.send(serde_json::json!({"type":"turnEnd","sessionId":"s1","reason":"completed","summary":"done"})).unwrap();
        a.tick().unwrap();
        let rest = a.take_committed(2);
        assert_eq!(a.committed_upto, a.transcript.len());
        assert!(!rest.is_empty());
        // Visible active window is now empty; full history still browsable.
        assert!(a.visible_transcript(false).is_empty());
        assert!(!a.visible_transcript(true).is_empty());
    }

    fn app_with_host(mock: MockHost) -> AppState {
        AppState::with_lang(Box::new(mock), Lang::Zh).unwrap()
    }
}
