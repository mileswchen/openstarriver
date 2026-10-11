use anyhow::Result;
use ratatui::text::Line;
use serde_json::Value;
use starriver_host::helpers::truncate_title;
use starriver_host::types::{AgentInfo, ModelEntry, SessionInfo, SessionMessage, WorkspaceInfo};
use starriver_host::CODING_AGENT_NAME;
use starriver_kernel::ui::UiEvent;

use crate::commands::{parse, Command};
use crate::events::EventRenderer;
use crate::host::HostApi;
use crate::i18n::{self, Lang};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Idle,
    Running,
    Picker(PickerKind),
    Approval,
    /// WI-81: full-screen transcript browser (Ctrl+T).
    Transcript,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerKind {
    Sessions,
    Workspaces,
    Models,
    /// /backtrack message-backtrack picker (lists only user messages).
    Messages,
    /// /unarchive archived-session restore picker.
    Archived,
    /// /preset sandbox preset picker (level 1 = execution presets).
    Presets,
}

/// Switchable variant id of a model entry: use variants when present, else default_model.
/// Cursor coordinates inside a multi-line input (line index, char column).
pub fn cursor_line_col(input: &str, cursor_chars: usize) -> (usize, usize) {
    let mut line = 0;
    let mut col = 0;
    for (i, c) in input.chars().enumerate() {
        if i == cursor_chars {
            return (line, col);
        }
        if c == '\n' {
            line += 1;
            col = 0;
        } else {
            col += 1;
        }
    }
    (line, col)
}

/// Char offset of the START of line `target` (0-based) in `input`.
/// Line 0 always starts at 0; line N starts right after the (N-1)-th newline.
fn line_start_char(input: &str, target: usize) -> usize {
    if target == 0 {
        return 0;
    }
    let mut line = 0;
    for (i, c) in input.chars().enumerate() {
        if c == '\n' {
            line += 1;
            if line == target {
                return i + 1;
            }
        }
    }
    input.chars().count()
}

/// Char offset of the END of line `target` (before its newline, or EOF).
fn line_end_char(input: &str, target: usize) -> usize {
    let mut line = 0;
    for (i, c) in input.chars().enumerate() {
        if c == '\n' {
            if line == target {
                return i;
            }
            line += 1;
        }
    }
    input.chars().count()
}

impl AppState {
    /// Number of lines in the composer draft.
    pub fn input_line_count(&self) -> usize {
        self.input.split('\n').count()
    }

    pub fn is_multiline_input(&self) -> bool {
        self.input.contains('\n')
    }

    /// Shift+Enter / Alt+Enter: insert a newline at the cursor.
    fn input_newline(&mut self) {
        let byte_idx = char_to_byte(&self.input, self.input_cursor);
        self.input.insert(byte_idx, '\n');
        self.input_cursor += 1;
        self.history_browse = None;
        self.cmd_hint_index = 0;
    }

    /// Bracketed-paste / clipboard text: normalize CRLF→CR→LF and insert at
    /// the cursor as a whole (never auto-submits).
    pub fn input_paste(&mut self, text: &str) {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        let extra = normalized.chars().count();
        if extra == 0 {
            return;
        }
        let byte_idx = char_to_byte(&self.input, self.input_cursor);
        self.input.insert_str(byte_idx, &normalized);
        self.input_cursor += extra;
        self.history_browse = None;
    }

    /// ↑/↓ across lines for a multi-line draft: returns true when consumed.
    fn input_vertical_move(&mut self, up: bool) -> bool {
        if !self.is_multiline_input() {
            return false;
        }
        let (line, col) = cursor_line_col(&self.input, self.input_cursor);
        let target = if up { line.saturating_sub(1) } else { line + 1 };
        if target == line && up {
            return false; // already on the first line
        }
        if target >= self.input_line_count() {
            return false; // past the last line
        }
        let start = line_start_char(&self.input, target);
        let end = line_end_char(&self.input, target);
        let len = end.saturating_sub(start);
        self.input_cursor = start + col.min(len);
        true
    }

    /// Set the composer text and park the cursor at its end — every
    /// programmatic write (prefill, completion, history) goes through here
    /// so `input_cursor` never goes stale.
    pub fn set_input(&mut self, text: String) {
        self.input_cursor = text.chars().count();
        self.input = text;
        self.history_browse = None;
        self.cmd_hint_index = 0;
    }

    /// Bracketed-paste entry from the terminal loop: normalize newlines,
    /// insert at the cursor; multi-line pastes also show a transcript note.
    pub fn paste_text(&mut self, text: &str) {
        let lines = text.lines().count();
        self.input_paste(text);
        if lines > 1 {
            self.transcript.push(Line::styled(
                i18n::fmt!(self.t("status.pasted"), lines),
                ratatui::style::Style::default().fg(ratatui::style::Color::DarkGray),
            ));
        }
    }

    /// Clear the composer (cursor home, exits history browsing).
    pub fn reset_input(&mut self) {
        self.input.clear();
        self.input_cursor = 0;
        self.history_browse = None;
        self.cmd_hint_index = 0;
    }
}

/// Byte offset of the `idx`-th char in `s` (input is short; linear scan).
fn char_to_byte(s: &str, idx: usize) -> usize {
    s.char_indices()
        .nth(idx)
        .map(|(b, _)| b)
        .unwrap_or(s.len())
}

pub fn model_variants(entry: &ModelEntry) -> Vec<String> {
    if entry.models.is_empty() {
        vec![entry.default_model.clone()]
    } else {
        entry.models.iter().map(|v| v.id.clone()).collect()
    }
}

/// One in-flight tool call: its placeholder line in the transcript (flipped
/// in place on completion — `DynamicToolCallCell` semantics) and the
/// start time for the `· 87ms` duration suffix.
#[derive(Debug, Clone)]
pub struct RunningTool {
    pub line: usize,
    pub started: std::time::Instant,
    pub name: String,
}

/// Completed tool row kept for the Ctrl+O verbose toggle: re-render the same
/// transcript line from the full output on demand.
#[derive(Debug, Clone)]
pub struct ToolDetail {
    pub line: usize,
    pub name: String,
    pub ok: bool,
    pub output: String,
}

/// WI-79: a diff cell block rendered below a completed edit/write/patch row;
/// Ctrl+O re-splices the block between collapsed/expanded forms.
#[derive(Debug, Clone)]
pub struct DiffDetail {
    pub start: usize,
    pub count: usize,
    pub files: Vec<crate::events::FileDiff>,
}

/// WI-83: a collapsed run of consecutive same-name successful tool calls.
/// `header_line` carries the summary row (it replaced member 1's row);
/// `preview_line` is the last member's row (always visible); `originals`
/// holds every removed member row (members 1..n-1 plus member 1's original
/// at index 0) for the verbose expansion.
#[derive(Debug, Clone)]
pub struct ToolGroup {
    pub header_line: usize,
    pub preview_line: usize,
    pub count: usize,
    pub name: String,
    pub originals: Vec<Line<'static>>,
}

pub struct ApprovalState {
    pub session_id: String,
    pub tool: String,
    pub reason: String,
    pub remember: bool,
    /// Approval request id from the UiEvent; reserved for the UiChannel
    /// respond path (host legacy `approve` still routes by session).
    pub request_id: Option<String>,
}

pub struct AppState {
    pub mode: Mode,
    /// UI language: resolved from the environment at startup; all text goes through [`Self::t`] / i18n.
    pub lang: Lang,
    pub transcript: Vec<Line<'static>>,
    pub input: String,
    pub status: String,
    pub workspaces: Vec<WorkspaceInfo>,
    pub sessions: Vec<SessionInfo>,
    /// Actually configured models (model_registry / models.yaml)
    pub model_entries: Vec<ModelEntry>,
    /// Provider id currently selected in the model picker; None means selecting the provider layer.
    pub model_picker_provider: Option<String>,
    pub current_model: Option<String>,
    pub current_workspace: Option<WorkspaceInfo>,
    pub current_agent: Option<AgentInfo>,
    /// Current sandbox preset label (from `sandbox/mode` events; default
    /// workspace-write until the first event arrives).
    pub sandbox_mode: String,
    /// Current execution-mode preset id ("standard" until switched; the
    /// sandbox label above tracks the effective sandbox).
    pub execution_mode: String,
    /// /preset approval level: Some(execution id) = second level (approval
    /// choices for that preset); None = preset list level.
    pub preset_approval_for: Option<String>,
    pub running_session_id: Option<String>,
    pub resume_session_id: Option<String>,
    pub receiver: Option<tokio::sync::broadcast::Receiver<Value>>,
    pub selected: usize,
 /// -style bottom-anchored scroll: rows ABOVE the newest line
    /// (0 = follow the tail; new content stays visible automatically).
    pub scroll: usize,
    /// In-flight tool calls by id → placeholder transcript line.
    pub running_tools: std::collections::HashMap<String, RunningTool>,
    /// Completed tool rows addressable by Ctrl+O (verbose re-render).
    pub tool_details: Vec<ToolDetail>,
    /// Ctrl+O detail mode (compact 200-char preview ↔ verbose multi-line tail).
    pub verbose_tools: bool,
    /// Turn clock for the spinner/elapsed title (cleared on terminal events).
    pub run_started_at: Option<std::time::Instant>,
    /// Cursor position (in chars) inside `input`.
    pub input_cursor: usize,
    /// Submitted prompts (Up/Down recall when the composer is empty).
    pub input_history: Vec<String>,
    /// History recall position while browsing (None = live editing).
    pub history_browse: Option<usize>,
    /// WI-76: prompts submitted while Running, sent in order once the turn
    /// ends; each entry also has its `⋯ queued` transcript line index.
    pub queued_messages: Vec<(String, usize)>,
    /// WI-77: cumulative session usage (tokens), updated by Usage events.
    pub usage_in: u64,
    pub usage_out: u64,
    /// WI-79: rendered diff blocks (Ctrl+O re-splice targets).
    pub diff_details: Vec<DiffDetail>,
    /// WI-83: the run currently accumulating (None between runs), plus all
    /// committed groups (kept for the verbose expansion after turn end).
    pub active_tool_group: Option<ToolGroup>,
    pub tool_groups: Vec<ToolGroup>,
    /// WI-81: transcript-browser state. `transcript_search` = Some(query)
    /// while typing/active; `transcript_match` = index into the match list.
    pub transcript_search: Option<String>,
    pub transcript_match: Option<usize>,
    /// The mode to restore when the transcript view closes.
    pub transcript_return: Mode,
    /// WI-80: transcript lines below this index were already flushed into
    /// the terminal's native scrollback (inline mode); they are immutable
    /// now — stored records referencing them are purged at flush time.
    pub committed_upto: usize,
    /// WI-82: transcript line currently accumulating assistant-text deltas
    /// (typewriter). The final AssistantText replaces it wholesale.
    streaming_text_line: Option<usize>,
    pub approval: Option<ApprovalState>,
    pub should_quit: bool,
    /// Selected index in the command hint list (when input starts with `/` or `\`).
    pub cmd_hint_index: usize,
    /// /backtrack message picker data (user messages only).
    pub message_timeline: Vec<SessionMessage>,
    /// Source session id pending backtrack in /backtrack.
    pub backtrack_source: Option<String>,
    /// /unarchive archived-session picker data.
    pub archived_sessions: Vec<SessionInfo>,
    /// /worktree session-level isolation toggle (local state, defaults false at startup).
    pub worktree_isolation: bool,
    renderer: EventRenderer,
    host: Box<dyn HostApi>,
}

impl AppState {
    pub fn new(host: Box<dyn HostApi>) -> Result<Self> {
        Self::with_lang(host, i18n::current_lang())
    }

    /// Construct with a given language (tests use a fixed language to avoid machine locale).
    pub fn with_lang(host: Box<dyn HostApi>, lang: Lang) -> Result<Self> {
        // The TUI owns its agent choice (no host-side "current agent"): it
        // runs as the built-in coding agent and names it on every execute.
        let current_agent: Option<AgentInfo> = Some(AgentInfo {
            name: CODING_AGENT_NAME.into(),
            description: "".into(),
            version: "1".into(),
        });
        let workspaces = host.list_workspaces();
        let sessions = host.list_sessions(None);
        let model_entries = host.list_models();
        // At startup read the current active model from the Starriver Code binding
        let current_model = host
            .list_agent_model_bindings()
            .into_iter()
            .find(|b| b.agent_name == CODING_AGENT_NAME)
            .map(|b| b.active_model)
            .filter(|m| !m.is_empty());
        // Status line starts empty: agent/mode/model info is carried by the three header lines; it fills only with transient feedback (running/done/error).
        let status = String::new();
        Ok(Self {
            mode: Mode::Idle,
            lang,
            transcript: Vec::new(),
            input: String::new(),
            status,
            workspaces,
            sessions,
            model_entries,
            model_picker_provider: None,
            current_model,
            current_workspace: None,
            current_agent,
            running_session_id: None,
            resume_session_id: None,
            sandbox_mode: "workspace-write".into(),
            execution_mode: "standard".into(),
            preset_approval_for: None,
            receiver: None,
            selected: 0,
            scroll: 0,
            running_tools: Default::default(),
            tool_details: Vec::new(),
            verbose_tools: false,
            run_started_at: None,
            input_cursor: 0,
            input_history: Vec::new(),
            history_browse: None,
            queued_messages: Vec::new(),
            usage_in: 0,
            usage_out: 0,
            diff_details: Vec::new(),
            active_tool_group: None,
            tool_groups: Vec::new(),
            transcript_search: None,
            transcript_match: None,
            transcript_return: Mode::Idle,
            committed_upto: 0,
            streaming_text_line: None,
            approval: None,
            should_quit: false,
            cmd_hint_index: 0,
            message_timeline: Vec::new(),
            backtrack_source: None,
            archived_sessions: Vec::new(),
            worktree_isolation: false,
            renderer: EventRenderer::with_lang(lang),
            host,
        })
    }

    /// WI-77: context window of the current provider's model entry
    /// (default 65536 when the provider doesn't declare one).
    pub fn context_window(&self) -> u64 {
        let provider = self
            .current_model
            .as_deref()
            .and_then(|m| m.split('/').next());
        provider
            .and_then(|pid| {
                self.model_entries
                    .iter()
                    .find(|e| e.id == pid)
                    .and_then(|e| e.context_window).map(u64::from)
            })
            .unwrap_or(65_536)
    }

    /// Estimated context fill ratio (0..=100) from cumulative usage.
    pub fn context_used_percent(&self) -> u64 {
        let window = self.context_window();
        if window == 0 {
            return 0;
        }
        let used = self.usage_in.saturating_add(self.usage_out);
        ((used.saturating_mul(100)) / window).min(100)
    }

    /// Estimated remaining context tokens.
    pub fn context_left(&self) -> u64 {
        self.context_window()
            .saturating_sub(self.usage_in.saturating_add(self.usage_out))
    }

    /// Text in the current language (i18n table lookup).
    pub fn t<'a>(&self, key: &'a str) -> &'a str {
        i18n::tr(self.lang, key)
    }

    /// Submit when Idle: slash command → run_command; ordinary input → execute.
    pub fn submit(&mut self) -> Result<()> {
        if self.mode != Mode::Idle {
            return Ok(());
        }
        let input = std::mem::take(&mut self.input);
        self.input_cursor = 0;
        self.history_browse = None;
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Ok(());
        }
        if let Some(cmd) = parse(trimmed) {
            return self.run_command(cmd);
        }
 // UserHistoryCell parity: echo the submitted prompt as a `› `
        // cell and keep it recallable with ↑/↓.
        for line in crate::events::user_echo_lines(trimmed) {
            self.transcript.push(line);
        }
        self.input_history.push(trimmed.to_string());
        self.input_cursor = 0;
        self.history_browse = None;
        match self.host.execute(
            trimmed.to_string(),
            self.resume_session_id.clone(),
            None,
            Some(self.agent_name()),
            Some(self.worktree_isolation),
        ) {
            Ok(rx) => {
                self.receiver = Some(rx);
                self.mode = Mode::Running;
                self.run_started_at = Some(std::time::Instant::now());
                self.scroll = 0; // follow the tail once a new turn starts
                self.status = self.t("status.running").into();
                Ok(())
            }
            Err(e) => {
                self.status = i18n::fmt!(self.t("status.exec_failed"), e);
                Ok(())
            }
        }
    }

    /// Wipe the transcript AND the metadata that indexes into it (running
    /// tool lines, verbose-toggle rows) — a stale index would corrupt rows.
    pub fn clear_transcript(&mut self) {
        self.transcript.clear();
        self.running_tools.clear();
        self.tool_details.clear();
        self.queued_messages.clear();
        self.diff_details.clear();
        self.active_tool_group = None;
        self.tool_groups.clear();
        self.committed_upto = 0;
        self.scroll = 0;
    }

    /// WI-80: the transcript range the ACTIVE area still owns — everything
    /// from `committed_upto` down. The fullscreen browser shows the full
    /// log regardless (`full = true`).
    pub fn visible_transcript(&self, full: bool) -> std::ops::Range<usize> {
        let start = if full { 0 } else { self.committed_upto };
        start..self.transcript.len()
    }

    /// WI-80: flush finished transcript lines into the native scrollback.
    /// Returns the flushed block (the inline renderer prints it verbatim).
    ///
    /// Flush policy: when the app is quiescent (Idle, nothing in flight)
    /// everything flushes; while running, only rows BELOW the oldest live
    /// record (in-flight tool placeholders, active group, queue echoes, diff
    /// blocks) and outside the live window flush — in-place updates can
    /// still reach everything that stays.
    pub fn take_committed(&mut self, live_window: usize) -> Vec<Line<'static>> {
        let len = self.transcript.len();
        let target = if self.mode == Mode::Idle
            && self.running_tools.is_empty()
            && self.queued_messages.is_empty()
        {
            len
        } else {
            // Oldest row any live record still points at.
            let mut oldest = len;
            for rt in self.running_tools.values() {
                oldest = oldest.min(rt.line);
            }
            if let Some(g) = &self.active_tool_group {
                oldest = oldest.min(g.header_line).min(g.preview_line);
            }
            for d in &self.diff_details {
                oldest = oldest.min(d.start);
            }
            for (_, idx) in &self.queued_messages {
                oldest = oldest.min(*idx);
            }
            // Keep at most `live_window` rows in the active area.
            len.saturating_sub(live_window).min(oldest)
        };
        let new_upto = self.committed_upto.max(target.min(len));
        if new_upto <= self.committed_upto {
            return Vec::new();
        }
        let flushed = self.transcript[self.committed_upto..new_upto].to_vec();
        self.committed_upto = new_upto;
        // Records fully below the boundary are now frozen in scrollback —
        // purge them so Ctrl+O / future splices only touch the live area.
        let upto = new_upto;
        self.tool_details.retain(|d| d.line >= upto);
        self.diff_details.retain(|d| d.start < upto + d.count || d.start >= upto);
        // ^ keep any record whose block starts at/after the boundary;
        //   blocks are never split (they were in `oldest` above).
        self.diff_details.retain(|d| d.start >= upto);
        self.tool_groups
            .retain(|g| g.header_line >= upto || g.preview_line >= upto);
        flushed
    }

    /// WI-81: open the transcript browser, remembering where to return.
    pub fn open_transcript(&mut self) {
        if self.mode == Mode::Transcript {
            return;
        }
        self.transcript_return = self.mode;
        self.transcript_search = None;
        self.transcript_match = None;
        self.mode = Mode::Transcript;
    }

    /// WI-81: transcript lines matching the active query (case-insensitive).
    pub fn transcript_matches(&self) -> Vec<usize> {
        let Some(q) = &self.transcript_search else {
            return Vec::new();
        };
        if q.is_empty() {
            return Vec::new();
        }
        let lower = q.to_lowercase();
        self.transcript
            .iter()
            .enumerate()
            .filter(|(_, l)| l.to_string().to_lowercase().contains(&lower))
            .map(|(i, _)| i)
            .collect()
    }

    /// WI-81: jump the bottom-anchored scroll so `line` lands at the bottom
    /// of the viewport (approximate: logical-line granularity).
    fn transcript_jump_to(&mut self, line: usize) {
        let total = self.transcript.len();
        self.scroll = total.saturating_sub(line + 1);
    }

    /// WI-81: cycle the current match (forward with `n`, back with `N`).
    fn transcript_match_step(&mut self, forward: bool) {
        let matches = self.transcript_matches();
        if matches.is_empty() {
            self.status = self.t("transcript.no_match").into();
            self.transcript_match = None;
            return;
        }
        let idx = match self.transcript_match {
            None => 0,
            Some(i) => {
                if forward {
                    (i + 1) % matches.len()
                } else {
                    i.checked_sub(1).unwrap_or(matches.len() - 1)
                }
            }
        };
        self.transcript_match = Some(idx);
        self.transcript_jump_to(matches[idx]);
    }

    /// WI-83 grouping engine. Called for every completed tool row in
    /// transcript order. Consecutive same-name successful calls (≥3)
    /// collapse to a summary header + the LAST member's preview row.
    fn update_tool_group(&mut self, name: String, ok: bool, line: usize) {
        let adjacent = self
            .active_tool_group
            .as_ref()
            .is_some_and(|g| g.preview_line + 1 == line);
        if !ok || !adjacent || self.active_tool_group.as_ref().is_some_and(|g| g.name != name) {
            // Failure or a break in the run commits the current group.
            if let Some(g) = self.active_tool_group.take() {
                self.tool_groups.push(g);
            }
            if !ok {
                return;
            }
            // Start a fresh candidate run.
            self.active_tool_group = Some(ToolGroup {
                header_line: line,
                preview_line: line,
                count: 1,
                name,
                originals: Vec::new(),
            });
            return;
        }
        // Extend the run.
        let mut g = self.active_tool_group.take().unwrap();
        g.count += 1;
        if g.count == 2 {
            // Second member: nothing to collapse yet; it becomes the preview.
            g.preview_line = line;
            self.active_tool_group = Some(g);
            return;
        }
        // Third and later members: fold the previous preview row away and
        // move the summary onto the first member's row.
        let removed = self.transcript[g.preview_line].clone();
        g.originals.push(removed);
        // Header row text = summary (it holds member 1's slot only from the
        // third member on; stash member-1's original the first time).
        if g.originals.len() == 1 {
            let first = self.transcript[g.header_line].clone();
            g.originals.insert(0, first);
        }
        self.splice_transcript(g.preview_line, 1, Vec::new());
        // preview_line now points at the new last row (same index after
        // removing the row before it... the removed row WAS the preview;
        // after removal the new row sits at that index unless it was last).
        g.preview_line = line.saturating_sub(1);
        self.transcript[g.header_line] = Line::styled(
            i18n::fmt!(self.t("tool.grouped"), g.count, g.name),
            ratatui::style::Style::default().fg(ratatui::style::Color::DarkGray),
        );
        self.active_tool_group = Some(g);
    }

    /// Commit the active group (turn end) without expanding it.
    fn commit_tool_group(&mut self) {
        if let Some(g) = self.active_tool_group.take() {
            // A run of 1–2 never collapsed: drop it instead of keeping a
            // degenerate record.
            if g.count >= 3 {
                self.tool_groups.push(g);
            }
        }
    }

    /// Remove `remove` lines at `start` and insert `insert` in their place,
    /// fixing EVERY stored transcript index (tool rows, diff blocks, queue
    /// echo rows, group records) for the net delta. The one primitive all
    /// transcript restructuring goes through.
    fn splice_transcript(
        &mut self,
        start: usize,
        remove: usize,
        insert: Vec<Line<'static>>,
    ) {
        let delta = insert.len() as isize - remove as isize;
        self.transcript
            .splice(start..start + remove, insert);
        let fix = |idx: &mut usize| {
            if *idx >= start + remove {
                *idx = (*idx as isize + delta) as usize;
            } else if *idx >= start {
                // BUG-91: an index INSIDE the removed range referenced a
                // deleted row — clamp to the splice point instead of
                // leaving it pointing at whatever now occupies the slot
                // (two details sharing one line, out-of-bounds on pure
                // deletions).
                *idx = start;
            }
        };
        for d in &mut self.tool_details {
            fix(&mut d.line);
        }
        for d in &mut self.diff_details {
            if d.start >= start + remove {
                d.start = (d.start as isize + delta) as usize;
            }
        }
        for (_, idx) in &mut self.queued_messages {
            fix(idx);
        }
        for rt in self.running_tools.values_mut() {
            fix(&mut rt.line);
        }
        // BUG-91: the streaming line index rides along too — a splice that
        // deletes rows above it left it dangling and the next delta indexed
        // out of bounds. If the streamed row ITSELF was removed, the next
        // delta restarts a fresh line.
        if let Some(idx) = self.streaming_text_line.as_mut() {
            if *idx >= start + remove {
                *idx = (*idx as isize + delta) as usize;
            } else if *idx >= start {
                self.streaming_text_line = None;
            }
        }
        for g in self.active_tool_group.iter_mut().chain(self.tool_groups.iter_mut()) {
            if g.header_line >= start + remove {
                g.header_line = (g.header_line as isize + delta) as usize;
            }
            if g.preview_line >= start + remove {
                g.preview_line = (g.preview_line as isize + delta) as usize;
            }
        }
    }

    /// WI-76: enqueue a prompt submitted while a turn is running. The
    /// `⋯ queued` echo row lands at the transcript tail; its index rides
    /// with the entry so removal can fix up later indices.
    fn enqueue_prompt(&mut self, text: String) {
        let preview: String = text.chars().take(48).collect();
        let line = Line::styled(
            i18n::fmt!(self.t("queue.entered"), preview),
            ratatui::style::Style::default().fg(ratatui::style::Color::Yellow)
                .add_modifier(ratatui::style::Modifier::DIM),
        );
        self.transcript.push(line);
        let idx = self.transcript.len() - 1;
        self.queued_messages.push((text, idx));
    }

    /// WI-76: Ctrl+E — take the last queued message back into the composer
    /// and remove its echo row. BUG-91: the removal must go through
    /// splice_transcript — a bare remove left every later index
    /// (streaming line, running-tool placeholders, tool details) dangling,
    /// panicking the TUI on the next delta.
    fn queue_edit_last(&mut self) {
        if let Some((text, idx)) = self.queued_messages.pop() {
            if idx < self.transcript.len() {
                self.splice_transcript(idx, 1, Vec::new());
            }
            self.set_input(text);
        }
    }

    /// WI-76: Ctrl+K — drop every queued message and its echo rows (via
    /// splice_transcript so every stored index stays valid — BUG-91).
    fn queue_clear(&mut self) {
        let count = self.queued_messages.len();
        if count == 0 {
            return;
        }
        let mut indices: Vec<usize> =
            self.queued_messages.iter().map(|(_, i)| *i).collect();
        indices.sort_unstable();
        for idx in indices.iter().rev() {
            if *idx < self.transcript.len() {
                self.splice_transcript(*idx, 1, Vec::new());
            }
        }
        self.queued_messages.clear();
        self.status = i18n::fmt!(self.t("queue.cleared"), count);
    }

    /// WI-76: after a turn ends (Idle + queue non-empty) send the next
    /// queued prompt through the normal submit path.
    fn drain_queue(&mut self) {
        if self.mode != Mode::Idle || self.queued_messages.is_empty() {
            return;
        }
        let (text, idx) = self.queued_messages.remove(0);
        if idx < self.transcript.len() {
            self.transcript.remove(idx);
            for (_, later) in self.queued_messages.iter_mut() {
                if *later > idx {
                    *later -= 1;
                }
            }
        }
        // Re-enter through set_input + submit so user echo/history/queueing
        // semantics are identical to a manual send.
        self.set_input(text);
        let _ = self.submit();
    }

    /// Ctrl+O: re-render every completed tool row from its full output with
    /// the opposite preview cap, re-splice every diff cell, and expand or
    /// re-collapse every tool-call group (the compact/full detail toggle).
    pub fn toggle_verbose(&mut self) {
        self.verbose_tools = !self.verbose_tools;
        let verbose = self.verbose_tools;
        let details = std::mem::take(&mut self.tool_details);
        for d in &details {
            if d.line < self.transcript.len() {
                self.transcript[d.line] = self.renderer.tool_done(
                    &d.name,
                    d.ok,
                    &d.output,
                    verbose,
                    0,
                );
            }
        }
        self.tool_details = details;
        // WI-79: diff cells re-splice between summary and colored body.
        let diffs = std::mem::take(&mut self.diff_details);
        for d in &diffs {
            if d.start <= self.transcript.len() {
                let lines =
                    crate::events::diff_lines(self.lang, &d.files, verbose);
                let remove = d.count.min(self.transcript.len() - d.start);
                self.splice_transcript(d.start, remove, lines);
            }
        }
        // NOTE: starts were fixed by splice_transcript; only counts changed.
        let diffs_fixed = diffs
            .into_iter()
            .map(|d| {
                let collapsed = !verbose;
                let count = if collapsed {
                    d.files.len()
                } else {
                    d.files
                        .iter()
                        .map(|f| {
                            1 + f.lines.len().min(crate::events::DIFF_MAX_BODY_LINES)
                                + usize::from(f.lines.len() > crate::events::DIFF_MAX_BODY_LINES)
                        })
                        .sum()
                };
                DiffDetail { count, ..d }
            })
            .collect();
        self.diff_details = diffs_fixed;
        // WI-83: groups expand (originals re-inserted) / re-collapse.
        let groups = std::mem::take(&mut self.tool_groups);
        let mut rest = self.active_tool_group.take();
        for g in groups.into_iter().chain(rest.take()) {
            if verbose {
                self.expand_group(&g);
            } else {
                self.collapse_group(&g);
            }
            // Keep the record with refreshed geometry.
            let g2 = if verbose {
                ToolGroup {
                    header_line: g.header_line,
                    preview_line: g.preview_line + g.originals.len() - 1,
                    count: g.count,
                    name: g.name,
                    originals: g.originals,
                }
            } else {
                ToolGroup {
                    header_line: g.header_line,
                    preview_line: g.preview_line.saturating_sub(g.originals.len() - 1),
                    count: g.count,
                    name: g.name,
                    originals: g.originals,
                }
            };
            self.tool_groups.push(g2);
        }
        self.active_tool_group = None;
        self.status = if verbose {
            self.t("status.verbose_on").into()
        } else {
            self.t("status.verbose_off").into()
        };
    }

    /// WI-83: re-insert a group's original member rows (verbose mode).
    fn expand_group(&mut self, g: &ToolGroup) {
        // originals[0] = member 1's row (currently the summary header),
        // originals[1..] = folded interior members.
        if g.originals.is_empty() {
            return;
        }
        self.transcript[g.header_line] = g.originals[0].clone();
        let interior: Vec<Line<'static>> = g.originals[1..].to_vec();
        if !interior.is_empty() {
            self.splice_transcript(g.preview_line, 0, interior);
        }
    }

    /// WI-83: fold a group back to summary + preview (compact mode).
    fn collapse_group(&mut self, g: &ToolGroup) {
        if g.originals.len() < 2 {
            return;
        }
        self.transcript[g.header_line] = Line::styled(
            i18n::fmt!(self.t("tool.grouped"), g.count, g.name),
            ratatui::style::Style::default().fg(ratatui::style::Color::DarkGray),
        );
        // Remove interior members: they sit between header and preview.
        let interior_start = g.header_line + 1;
        let interior_len = g.originals.len() - 1;
        if interior_start + interior_len <= self.transcript.len() {
            self.splice_transcript(interior_start, interior_len, Vec::new());
        }
    }

    /// Insert a char at the cursor (typing exits history browsing).
    fn input_insert(&mut self, c: char) {
        let byte_idx = char_to_byte(&self.input, self.input_cursor);
        self.input.insert(byte_idx, c);
        self.input_cursor += 1;
        self.history_browse = None;
        self.cmd_hint_index = 0;
    }

    fn input_backspace(&mut self) {
        if self.input_cursor == 0 {
            return;
        }
        self.input_cursor -= 1;
        let byte_idx = char_to_byte(&self.input, self.input_cursor);
        self.input.remove(byte_idx);
        self.history_browse = None;
        self.cmd_hint_index = 0;
    }

    /// ↑/↓ recall of submitted prompts when no command hint is visible and
    /// the composer is empty (or already browsing history).
    fn history_step(&mut self, older: bool) -> bool {
        if !self.input_history.is_empty()
            && match self.history_browse {
                Some(_) => true,
                None => self.input.is_empty(),
            }
        {
            let len = self.input_history.len();
            let next = match (self.history_browse, older) {
                (None, true) => Some(len.saturating_sub(1)),
                (None, false) => None,
                (Some(i), true) => Some(i.saturating_sub(1)),
                (Some(i), false) => {
                    if i + 1 >= len {
                        None
                    } else {
                        Some(i + 1)
                    }
                }
            };
            match next {
                Some(i) => {
                    self.history_browse = Some(i);
                    self.set_input(self.input_history[i].clone());
                    self.history_browse = Some(i); // set_input clears browse; restore
                    self.input_cursor = self.input.chars().count();
                }
                None => {
                    self.history_browse = None;
                    self.reset_input();
                }
            }
            return true;
        }
        false
    }

    /// Called each UI loop iteration: drain per-run events.
    /// Note: take the receiver first to avoid a borrow conflict between `&self.receiver` and `&mut self`.
    pub fn tick(&mut self) -> Result<()> {
        let Some(mut rx) = self.receiver.take() else {
            return Ok(());
        };
        loop {
            match rx.try_recv() {
                Ok(value) => {
                    if self.apply_event(value)? {
                        return Ok(()); // task terminated; do not restore receiver
                    }
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {
                    self.status = self.t("status.events_lagged").into();
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                    self.run_started_at = None;
                    if self.mode == Mode::Running || self.mode == Mode::Approval {
                        self.status = self.t("status.stream_closed").into();
                        self.mode = Mode::Idle;
                    }
                    return Ok(());
                }
            }
        }
        self.receiver = Some(rx);
        // WI-76: covers the stream-closed path (and any terminal event that
        // left the app Idle with a non-empty queue).
        self.drain_queue();
        Ok(())
    }

    /// Apply a single event; returns true when the task has ended (receiver not restored).
    fn apply_event(&mut self, value: Value) -> Result<bool> {
        let ev: UiEvent = match serde_json::from_value(value) {
            Ok(ev) => ev,
            Err(e) => {
                // Bad events must not crash the TUI: show a status hint and continue.
                self.status = i18n::fmt!(self.t("status.event_parse_failed"), e);
                return Ok(false);
            }
        };
        // WI-82 typewriter: text deltas append to a live tail row; the
        // final AssistantText replaces it with the authoritative block.
        if let UiEvent::AssistantTextDelta { text, .. } = &ev {
            if !text.is_empty() {
                match self.streaming_text_line {
                    Some(idx) => {
                        let mut spans = self.transcript[idx].spans.clone();
                        spans.push(ratatui::text::Span::raw(text.clone()));
                        self.transcript[idx] = Line::from(spans);
                    }
                    None => {
                        self.transcript
                            .push(Line::from(text.clone()));
                        self.streaming_text_line = Some(self.transcript.len() - 1);
                    }
                }
            }
            return Ok(false);
        }
 // active-cell lifecycle: a ToolCall pushes a `• Calling…`
        // placeholder; the matching ToolResult flips THAT line in place
        // (status + color + `· 87ms` duration) instead of appending a row.
        match &ev {
            UiEvent::ToolCall { id, name, .. } => {
                let line = self.renderer.tool_calling(name);
                self.transcript.push(line);
                self.running_tools.insert(
                    id.clone(),
                    RunningTool {
                        line: self.transcript.len() - 1,
                        started: std::time::Instant::now(),
                        name: name.clone(),
                    },
                );
                return Ok(false);
            }
            UiEvent::ToolResult { id, ok, output, .. } => {
                if let Some(rt) = self.running_tools.remove(id) {
                    let elapsed = rt.started.elapsed().as_millis();
                    let line =
                        self.renderer
                            .tool_done(&rt.name, *ok, output, self.verbose_tools, elapsed);
                    if rt.line < self.transcript.len() {
                        self.transcript[rt.line] = line;
                    }
                    self.tool_details.push(ToolDetail {
                        line: rt.line,
                        name: rt.name.clone(),
                        ok: *ok,
                        output: output.clone(),
                    });
                    // WI-79: edit/write/patch results carrying a unified
                    // diff get a grouped diff cell right below the tool row.
                    let tool_line = rt.line;
                    if *ok
                        && matches!(rt.name.as_str(), "edit" | "write" | "patch")
                        && crate::events::looks_like_unified_diff(output)
                    {
                        let files = crate::events::parse_unified_diff(output);
                        if !files.is_empty() {
                            let lines =
                                crate::events::diff_lines(self.lang, &files, self.verbose_tools);
                            let count = lines.len();
                            self.splice_transcript(tool_line + 1, 0, lines);
                            self.diff_details.push(DiffDetail {
                                start: tool_line + 1,
                                count,
                                files,
                            });
                        }
                    }
                    // WI-83: same-name consecutive successful calls group.
                    self.update_tool_group(rt.name, *ok, tool_line);
                } else {
                    // No placeholder (resumed mid-run): standalone row.
                    self.transcript
                        .extend(self.renderer.push(&ev));
                }
                return Ok(false);
            }
            _ => {}
        }
        if let UiEvent::AssistantText { text, .. } = &ev {
            if !text.trim().is_empty() {
                if let Some(idx) = self.streaming_text_line.take() {
                    if idx < self.transcript.len() {
                        // The authoritative block replaces the streamed row
                        // (deltas may be partially deduped by adapters).
                        self.transcript[idx] = Line::from(text.clone());
                    }
                    return Ok(false);
                }
            } else {
                // Empty final block after deltas: drop the streamed row
                // (splice also fixes every stored index above it).
                if let Some(idx) = self.streaming_text_line.take() {
                    if idx < self.transcript.len() {
                        self.splice_transcript(idx, 1, Vec::new());
                    }
                }
                return Ok(false);
            }
        } else if !matches!(ev, UiEvent::AssistantText { .. }) {
            self.streaming_text_line = None;
        }
        let lines = self.renderer.push(&ev);
        self.transcript.extend(lines);
        match ev {
            UiEvent::SessionStarted { session_id, .. } => {
                self.running_session_id = Some(session_id.clone());
                self.resume_session_id = Some(session_id);
                Ok(false)
            }
            // Between-turns marker: the driver claimed the queued message as
            // the next turn of the SAME execute — re-arm the run state so the
            // status bar and cancel binding track the live follow-up.
            UiEvent::TaskStarted { session_id, .. } => {
                self.running_session_id = Some(session_id.clone());
                self.resume_session_id = Some(session_id);
                self.run_started_at = self.run_started_at.or(Some(std::time::Instant::now()));
                Ok(false)
            }
            UiEvent::TurnEnd {
                session_id,
                summary,
                ..
            } => {
                self.running_session_id = None;
                self.run_started_at = None;
                self.running_tools.clear();
                self.mode = Mode::Idle;
                self.status = i18n::fmt!(self.t("status.task_done"), summary);
                self.resume_session_id = Some(session_id);
                // WI-83: the turn ended — commit the active group collapsed.
                self.commit_tool_group();
                // WI-76: the turn is over — release the next queued prompt.
                self.drain_queue();
                Ok(true)
            }
            UiEvent::TurnError {
                session_id, error, ..
            } => {
                self.running_session_id = None;
                self.run_started_at = None;
                self.mode = Mode::Idle;
                self.status = i18n::fmt!(self.t("status.task_failed"), error);
                self.resume_session_id = Some(session_id);
                // WI-83: commit the active group collapsed.
                self.commit_tool_group();
                // WI-76: failed turns also release the queue.
                self.drain_queue();
                Ok(true)
            }
            UiEvent::Cancelled { .. } => {
                self.running_session_id = None;
                self.run_started_at = None;
                self.running_tools.clear();
                self.mode = Mode::Idle;
                self.status = self.t("status.cancelled").into();
                // WI-76: after an explicit interrupt the queue SURVIVES (the
                // user may still want it) — only auto-drain on natural ends.
                Ok(true)
            }
            UiEvent::AssistantText { .. }
            | UiEvent::AssistantTextDelta { .. }
            | UiEvent::ThinkingText { .. }
            | UiEvent::ThinkingDelta { .. }
            | UiEvent::ToolCall { .. }
            | UiEvent::ToolResult { .. }
            | UiEvent::Compaction { .. }
            // Goal-loop state broadcasts render in the desktop app; the TUI
            // ignores them (its /goal card reads state on demand).
            | UiEvent::GoalStatus { .. } => Ok(false),
            UiEvent::Usage {
                input_tokens,
                output_tokens,
                ..
            } => {
                // WI-77: cumulative counters drive the composer-title and
                // footer context readouts.
                self.usage_in = self.usage_in.saturating_add(input_tokens.max(0) as u64);
                self.usage_out = self.usage_out.saturating_add(output_tokens.max(0) as u64);
                Ok(false)
            }
            UiEvent::SandboxModeChanged { mode, .. } => {
                self.sandbox_mode = mode;
                Ok(false)
            }
            // User-questions slice 1: the TUI has no question UI yet — the
            // ask times out host-side and the model adapts (spec §5).
            UiEvent::QuestionAsked { .. } => Ok(false),
            // Plan fold: plan reviews render via the event renderer; the
            // interactive decision card is the desktop client's. No answerer
 // here → the review times out host-side (parity: the call
            // fails rather than silently leaving plan mode).
            UiEvent::PlanReviewRequested { .. } | UiEvent::PlanReviewDecided { .. } => Ok(false),
            // WI-35: instructions budget state already renders via the event
            // renderer; no app-state change.
            UiEvent::InstructionsBudget { .. } => Ok(false),
            UiEvent::ApprovalRequested {
                session_id,
                request_id,
                tool_name,
                reason,
                ..
            } => {
                self.approval = Some(ApprovalState {
                    session_id,
                    tool: tool_name,
                    reason: reason.unwrap_or_default(),
                    remember: false,
                    request_id: Some(request_id),
                });
                self.mode = Mode::Approval;
                Ok(false)
            }
// Byte-exact live view (open-turn snapshot stream): the TUI is an on-machine rich client — ignored.
            UiEvent::TraceSnapshot { .. } => Ok(false),

        }
    }

    pub fn cancel_run(&mut self) {
        if let Some(sid) = self.running_session_id.clone() {
            self.host.cancel(sid);
            self.status = self.t("status.cancelling").into();
        }
    }

    pub fn approve(&mut self, allowed: bool) -> Result<()> {
        let Some(a) = self.approval.take() else {
            return Ok(());
        };
        // /delete's second confirmation reuses the approval overlay: routed by a tool marker, not through host.approve.
        if a.tool == "delete-session" {
            self.mode = Mode::Idle;
            if allowed {
                let sid = a.session_id.clone();
                match self.host.delete_session(sid.clone()) {
                    Ok(()) => {
                        self.sessions.retain(|s| s.id != sid);
                        self.clear_transcript();
                        self.resume_session_id = None;
                        self.status = i18n::fmt!(self.t("status.session_deleted"), sid);
                    }
                    Err(e) => self.status = i18n::fmt!(self.t("status.session_delete_failed"), e),
                }
            } else {
                self.status = self.t("status.denied").into();
            }
            return Ok(());
        }
        // Returning false = no pending approval / already answered by another client (first come, first served); the UI does not block.
        let _ = self.host.approve(a.session_id, allowed, Some(a.remember));
        self.mode = Mode::Running;
        self.status = if allowed {
            self.t("status.allowed").into()
        } else {
            self.t("status.denied").into()
        };
        Ok(())
    }

    pub fn toggle_remember(&mut self) {
        if let Some(a) = &mut self.approval {
            a.remember = !a.remember;
        }
    }

    pub fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> Result<()> {
        use crossterm::event::{KeyCode, KeyModifiers};
        match self.mode {
            Mode::Idle => match key.code {
                KeyCode::Char(c) => {
                    if key.modifiers.contains(KeyModifiers::CONTROL) {
                        match c {
                            'c' | 'd' => self.should_quit = true,
 // Ctrl+O: -style tool-output detail toggle.
                            'o' => self.toggle_verbose(),
                            // Ctrl+T: transcript browser (WI-81).
                            't' => self.open_transcript(),
                            _ => {}
                        }
                    } else {
                        self.input_insert(c);
                    }
                }
                KeyCode::Enter
                    if key.modifiers.contains(KeyModifiers::SHIFT)
                        || key.modifiers.contains(KeyModifiers::ALT) =>
                {
                    // WI-78: Shift/Alt+Enter inserts a newline (multi-line
                    // draft); bare Enter below still submits.
                    self.input_newline();
                }
                KeyCode::Enter => {
                    // When the command hint is visible and the input is not yet an executable command (bare prefix/partial name),
                    // complete with the currently selected command and submit; otherwise submit as-is.
                    if matches!(parse(self.input.trim()), Some(Command::Unknown(_))) {
                        if let Some((prefix, shown)) =
                            crate::commands::hint_docs(&self.input, self.lang)
                        {
                            let sel = self.cmd_hint_index.min(shown.len().saturating_sub(1));
                            if let Some((name, _)) = shown.get(sel) {
                                let cmd_word =
                                    name.split_whitespace().next().unwrap_or("").to_string();
                                self.set_input(format!("{prefix}{cmd_word}"));
                            }
                        }
                    }
                    self.submit()?;
                }
                KeyCode::Backspace => self.input_backspace(),
                KeyCode::Delete => {
                    if self.input_cursor < self.input.chars().count() {
                        let byte_idx = char_to_byte(&self.input, self.input_cursor);
                        self.input.remove(byte_idx);
                        self.history_browse = None;
                    }
                }
                KeyCode::Left => {
                    self.input_cursor = self.input_cursor.saturating_sub(1);
                }
                KeyCode::Right => {
                    if self.input_cursor < self.input.chars().count() {
                        self.input_cursor += 1;
                    }
                }
                KeyCode::Home => {
                    // WI-78: bare Home/End park at the CURRENT line's
                    // bounds; Ctrl+Home goes to the very start.
                    let (line, _) = cursor_line_col(&self.input, self.input_cursor);
                    self.input_cursor = line_start_char(&self.input, line);
                }
                KeyCode::Home if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.input_cursor = 0;
                }
                KeyCode::End => {
                    let (line, _) = cursor_line_col(&self.input, self.input_cursor);
                    self.input_cursor = line_end_char(&self.input, line);
                }
                KeyCode::Esc => self.reset_input(),
                // When the command hint is visible, up/down move the selection; otherwise scroll the transcript as usual.
                KeyCode::Up | KeyCode::Down => {
                    let up = matches!(key.code, KeyCode::Up);
                    // WI-78: in a multi-line draft ↑/↓ move the cursor
                    // across lines (hint nav and history recall are
                    // single-line concepts; hints only fire on `/`-prefixed
                    // first lines anyway).
                    if self.is_multiline_input() && self.input_vertical_move(up) {
                        // consumed
                    } else if let Some((_, shown)) =
                        crate::commands::hint_docs(&self.input, self.lang)
                    {
                        let len = shown.len();
                        if len > 0 {
                            let delta: isize = if up { -1 } else { 1 };
                            self.cmd_hint_index = (self.cmd_hint_index as isize + delta)
                                .rem_euclid(len as isize)
                                as usize;
                        }
                    } else if self.history_step(up) {
                        // Prompt recall consumed the key.
                    } else {
                        // Bottom-anchored scroll: ↑ moves away from the tail,
                        // ↓ walks back toward it (0 = follow the newest line).
                        self.scroll = if up {
                            self.scroll.saturating_add(1)
                        } else {
                            self.scroll.saturating_sub(1)
                        };
                    }
                }
                KeyCode::PageUp => self.scroll = self.scroll.saturating_add(10),
                KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(10),
                // Home/End moved to input-cursor editing; transcript paging
                // anchors on End below.
                // Ctrl+End jumps the transcript back to the bottom; bare End
                // keeps its input-cursor meaning (handled above).
                KeyCode::End if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.scroll = 0;
                }
                _ => {}
            },
            Mode::Running => {
                // WI-76: the composer stays editable while a turn runs.
                // Enter queues the prompt; Ctrl+E/Ctrl+K manage the queue;
                // Esc/Ctrl+C still interrupt (queue survives interruption).
                if matches!(key.code, KeyCode::Esc) {
                    self.cancel_run();
                } else if matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                {
                    self.cancel_run();
                } else if matches!(key.code, KeyCode::Char('o') | KeyCode::Char('O'))
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                {
                    self.toggle_verbose();
                } else if matches!(key.code, KeyCode::Char('t') | KeyCode::Char('T'))
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                {
                    // Reading while running: the turn keeps streaming into
                    // the transcript behind the browser.
                    self.open_transcript();
                } else if matches!(key.code, KeyCode::Char('e') | KeyCode::Char('E'))
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                {
                    self.queue_edit_last();
                } else if matches!(key.code, KeyCode::Char('k') | KeyCode::Char('K'))
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                {
                    self.queue_clear();
                } else if matches!(key.code, KeyCode::Enter)
                    && !key.modifiers.contains(KeyModifiers::SHIFT)
                    && !key.modifiers.contains(KeyModifiers::ALT)
                {
                    let input = std::mem::take(&mut self.input);
                    self.input_cursor = 0;
                    self.history_browse = None;
                    let trimmed = input.trim().to_string();
                    if !trimmed.is_empty() {
                        self.enqueue_prompt(trimmed);
                    }
                } else if matches!(key.code, KeyCode::Enter)
                    && (key.modifiers.contains(KeyModifiers::SHIFT)
                        || key.modifiers.contains(KeyModifiers::ALT))
                {
                    self.input_newline();
                } else if let KeyCode::Char(c) = key.code {
                    if !key.modifiers.contains(KeyModifiers::CONTROL) {
                        self.input_insert(c);
                    }
                } else if matches!(key.code, KeyCode::Backspace) {
                    self.input_backspace();
                } else if matches!(key.code, KeyCode::Delete) {
                    if self.input_cursor < self.input.chars().count() {
                        let byte_idx = char_to_byte(&self.input, self.input_cursor);
                        self.input.remove(byte_idx);
                        self.history_browse = None;
                    }
                } else if matches!(key.code, KeyCode::Left) {
                    self.input_cursor = self.input_cursor.saturating_sub(1);
                } else if matches!(key.code, KeyCode::Right) {
                    if self.input_cursor < self.input.chars().count() {
                        self.input_cursor += 1;
                    }
                } else if matches!(key.code, KeyCode::Home) {
                    let (line, _) = cursor_line_col(&self.input, self.input_cursor);
                    self.input_cursor = line_start_char(&self.input, line);
                } else if matches!(key.code, KeyCode::End) {
                    let (line, _) = cursor_line_col(&self.input, self.input_cursor);
                    self.input_cursor = line_end_char(&self.input, line);
                } else {
 // parity: the transcript stays readable while a
                    // turn runs — ↑/↓/PageUp walk back from the tail,
                    // Ctrl+End drops back to following.
                    match key.code {
                        KeyCode::Up => self.scroll = self.scroll.saturating_add(1),
                        KeyCode::Down => self.scroll = self.scroll.saturating_sub(1),
                        KeyCode::PageUp => self.scroll = self.scroll.saturating_add(10),
                        KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(10),
                        KeyCode::End
                            if key.modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            self.scroll = 0;
                        }
                        _ => {}
                    }
                }
            }
            Mode::Transcript => match key.code {
                KeyCode::Esc => {
                    if self.transcript_search.is_some() {
                        // First Esc closes the search, keeps the view.
                        self.transcript_search = None;
                        self.transcript_match = None;
                    } else {
                        let back = self.transcript_return;
                        self.mode = back;
                    }
                }
                KeyCode::Char('/') if self.transcript_search.is_none() => {
                    self.transcript_search = Some(String::new());
                    self.transcript_match = None;
                }
                KeyCode::Enter => {
                    // Confirm the query: jump to the first match.
                    self.transcript_match = None;
                    self.transcript_match_step(true);
                }
                // n/N cycle matches EXCEPT while typing the query (search
                // active but unconfirmed) — there they compose into it.
                KeyCode::Char('n')
                    if self.transcript_search.is_none()
                        || self.transcript_match.is_some() =>
                {
                    self.transcript_match_step(true)
                }
                KeyCode::Char('N')
                    if self.transcript_search.is_none()
                        || self.transcript_match.is_some() =>
                {
                    self.transcript_match_step(false)
                }
                KeyCode::Char(c) => {
                    if let Some(q) = &mut self.transcript_search {
                        q.push(c);
                        self.transcript_match = None;
                    }
                }
                KeyCode::Backspace => {
                    if let Some(q) = &mut self.transcript_search {
                        q.pop();
                        self.transcript_match = None;
                    }
                }
                KeyCode::Up => self.scroll = self.scroll.saturating_add(1),
                KeyCode::Down => self.scroll = self.scroll.saturating_sub(1),
                KeyCode::PageUp => self.scroll = self.scroll.saturating_add(10),
                KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(10),
                KeyCode::End
                    if key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    self.scroll = 0;
                }
                KeyCode::Char('t') | KeyCode::Char('T')
                    if key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    let back = self.transcript_return;
                    self.mode = back;
                }
                _ => {}
            },
            Mode::Picker(_) => match key.code {
                KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
                KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
                KeyCode::Enter => self.pick_select()?,
                KeyCode::Esc => {
                    if matches!(self.mode, Mode::Picker(PickerKind::Models)) {
                        self.model_picker_back();
                    } else if matches!(self.mode, Mode::Picker(PickerKind::Presets)) {
                        self.preset_picker_back();
                    } else {
                        self.pick_back();
                    }
                }
                _ => {}
            },
            Mode::Approval => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => self.approve(true)?,
                KeyCode::Char('n') | KeyCode::Char('N') => self.approve(false)?,
                KeyCode::Char('m') | KeyCode::Char('M') => self.toggle_remember(),
                KeyCode::Esc => {
                    // /delete's second confirmation is a non-task approval: Esc returns to Idle, not Running.
                    let is_delete = self
                        .approval
                        .as_ref()
                        .map(|a| a.tool == "delete-session")
                        .unwrap_or(false);
                    self.approval = None;
                    self.mode = if is_delete { Mode::Idle } else { Mode::Running };
                    self.status = self.t("status.approval_deferred").into();
                }
                KeyCode::Char('c') | KeyCode::Char('C')
                    if key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    self.cancel_run();
                }
                _ => {}
            },
        }
        Ok(())
    }

    fn run_command(&mut self, cmd: Command) -> Result<()> {
        match cmd {
            Command::Help => {
                self.transcript
                    .extend(crate::commands::help_lines(self.lang));
                self.status = self.t("status.help_shown").into();
            }
            Command::New => {
                self.clear_transcript();
                self.reset_input();
                self.resume_session_id = None;
                self.scroll = 0;
                self.status = self.t("status.new_session").into();
            }
            Command::Sessions => {
                let pid = self.current_workspace.as_ref().map(|p| p.id.clone());
                self.sessions = self.host.list_sessions(pid);
                self.selected = 0;
                self.mode = Mode::Picker(PickerKind::Sessions);
                self.status = self.t("status.pick_session").into();
            }
            Command::Projects => {
                self.workspaces = self.host.list_workspaces();
                self.selected = 0;
                self.mode = Mode::Picker(PickerKind::Workspaces);
                self.status = self.t("status.pick_project").into();
            }
            Command::Model(provider, model) => match (provider, model) {
                // /model p m: switch directly
                (Some(provider), Some(model)) => {
                    let agent = self.agent_name();
                    match self
                        .host
                        .set_agent_active_model(agent, provider.clone(), model.clone())
                    {
                        Ok(()) => {
                            self.current_model = Some(format!("{provider}/{model}"));
                            self.status = i18n::fmt!(
                                self.t("status.model_switched"),
                                format!("{provider}/{model}")
                            );
                        }
                        Err(e) => self.status = i18n::fmt!(self.t("status.model_switch_failed"), e),
                    }
                }
                // /model p: enter model selection for that provider
                (Some(provider), None) => {
                    self.model_entries = self.host.list_models();
                    if self.model_entries.iter().any(|p| p.id == provider) {
                        self.model_picker_provider = Some(provider.clone());
                        self.selected = 0;
                        self.mode = Mode::Picker(PickerKind::Models);
                        self.status = i18n::fmt!(self.t("status.pick_model_for"), provider);
                    } else {
                        self.status = i18n::fmt!(self.t("status.provider_not_found"), provider);
                    }
                }
                // /model: two-level picker
                (None, None) => {
                    self.model_entries = self.host.list_models();
                    self.model_picker_provider = None;
                    self.selected = 0;
                    self.mode = Mode::Picker(PickerKind::Models);
                    self.status = self.t("status.pick_provider").into();
                }
                (None, Some(_)) => {
                    self.status = self.t("status.model_usage").into();
                }
            },
            // /plan /build /auto
            Command::Project(arg) => match arg {
                Some(arg) => {
                    let found = self
                        .workspaces
                        .iter()
                        .find(|p| p.id == arg || p.title == arg)
                        .cloned();
                    match found {
                        Some(p) => {
                            self.host
                                .set_active_workspace_for(&self.agent_name(), Some(p.id.clone()))?;
                            self.after_workspace_switch(p);
                        }
                        None => self.status = i18n::fmt!(self.t("status.project_not_found"), arg),
                    }
                }
                None => {
                    self.status = match &self.current_workspace {
                        Some(p) => i18n::fmt!(self.t("status.current_workspace"), p.title, p.path),
                        None => self.t("status.no_workspace").into(),
                    };
                }
            },
            Command::Open(path) => {
                let path_str = path.to_string_lossy().into_owned();
 // semantics: the registry only resolves existing directories;
                // the display title defaults to the basename.
                let p = self.host.create_workspace(path_str)?;
                self.host
                    .set_active_workspace_for(&self.agent_name(), Some(p.id.clone()))?;
                self.after_workspace_switch(p);
            }
            Command::Compact => {
                let Some(sid) = self.resume_session_id.clone() else {
                    self.status = self.t("status.no_session").into();
                    return Ok(());
                };
                match self.host.compact_session(sid) {
                    Ok((before, after)) => {
                        let line = i18n::fmt!(self.t("status.compact_done"), before, after);
                        self.transcript.push(Line::from(line.clone()));
                        self.status = line;
                    }
                    Err(e) => self.status = i18n::fmt!(self.t("status.exec_failed"), e),
                }
            }
            Command::Diff => {
                let Some(p) = self.current_workspace.clone() else {
                    self.status = self.t("status.no_cwd").into();
                    return Ok(());
                };
                match self.host.workspace_diff(p.path.clone()) {
                    Ok(text) => {
                        if text.trim().is_empty() {
                            self.transcript
                                .push(Line::from(self.t("status.diff_empty")));
                            self.status = self.t("status.diff_empty").into();
                        } else if crate::events::looks_like_unified_diff(&text) {
                            // WI-79: grouped, colored, Ctrl+O-toggleable cells.
                            let files = crate::events::parse_unified_diff(&text);
                            let lines =
                                crate::events::diff_lines(self.lang, &files, self.verbose_tools);
                            let count = lines.len();
                            let start = self.transcript.len();
                            self.transcript.extend(lines);
                            if count > 0 {
                                self.diff_details.push(DiffDetail { start, count, files });
                            }
                            self.status = self.t("status.diff_done").into();
                        } else {
                            self.transcript
                                .extend(text.lines().map(|l| Line::from(l.to_string())));
                            self.status = self.t("status.diff_done").into();
                        }
                    }
                    Err(e) => self.status = i18n::fmt!(self.t("status.exec_failed"), e),
                }
            }
            Command::Memory(sub) => {
                let sub = sub.trim().to_string();
                match sub.as_str() {
                    "" | "status" => match self.host.memory_status() {
                        Ok(v) => {
                            let line =
                                format!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
                            for l in line.lines() {
                                self.transcript.push(Line::from(l.to_string()));
                            }
                            self.status = self.t("memory.status_title").into();
                        }
                        Err(e) => self.status = i18n::fmt!(self.t("status.exec_failed"), e),
                    },
                    "ingest" => match self.host.memory_ingest() {
                        Ok(v) => {
                            let total =
                                v.get("totalMemories").and_then(|t| t.as_u64()).unwrap_or(0);
                            self.status = i18n::fmt!(self.t("memory.ingest_done"), total);
                            let line =
                                format!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
                            for l in line.lines() {
                                self.transcript.push(Line::from(l.to_string()));
                            }
                        }
                        Err(e) => self.status = i18n::fmt!(self.t("status.exec_failed"), e),
                    },
                    "export" => {
                        let Some(p) = self.current_workspace.clone() else {
                            self.status = self.t("status.no_cwd").into();
                            return Ok(());
                        };
                        match self.host.memory_export(p.path.clone()) {
                            Ok(v) => {
                                let written: Vec<String> = v
                                    .get("written")
                                    .and_then(|w| w.as_array())
                                    .map(|a| {
                                        a.iter()
                                            .filter_map(|x| x.as_str().map(str::to_string))
                                            .collect()
                                    })
                                    .unwrap_or_default();
                                self.status = if written.is_empty() {
                                    self.t("memory.export_noop").into()
                                } else {
                                    i18n::fmt!(self.t("memory.export_done"), written.join(", "))
                                };
                            }
                            Err(e) => self.status = i18n::fmt!(self.t("status.exec_failed"), e),
                        }
                    }
                    other => {
                        self.status = i18n::fmt!(self.t("memory.unknown_sub"), other);
                    }
                }
            }
            Command::Status => {
                self.transcript.extend(self.status_report_lines());
                self.status = self.t("status.status_title").into();
            }
            Command::Usage => {
                let Some(sid) = self.resume_session_id.clone() else {
                    self.status = self.t("status.no_session").into();
                    return Ok(());
                };
                match self.host.session_usage(sid.clone()) {
                    Ok((tokens, count)) => {
                        let line = i18n::fmt!(self.t("status.usage_line"), sid, tokens, count);
                        self.transcript.push(Line::from(line.clone()));
                        self.status = line;
                    }
                    Err(e) => self.status = i18n::fmt!(self.t("status.exec_failed"), e),
                }
            }
            Command::Init => {
                let Some(p) = self.current_workspace.clone() else {
                    self.status = self.t("status.no_cwd").into();
                    return Ok(());
                };
                match self.host.init_project_agents(p.path.clone()) {
                    Ok(path) => {
                        let line = i18n::fmt!(self.t("status.init_done"), path.display());
                        self.transcript.push(Line::from(line.clone()));
                        self.status = line;
                    }
                    Err(e) => self.status = i18n::fmt!(self.t("status.exec_failed"), e),
                }
            }
            Command::Export => {
                let (Some(sid), Some(p)) = (
                    self.resume_session_id.clone(),
                    self.current_workspace.clone(),
                ) else {
                    self.status = self.t("status.no_session").into();
                    return Ok(());
                };
                match self.host.export_session(sid, p.path.clone()) {
                    Ok(path) => {
                        let line = i18n::fmt!(self.t("status.export_done"), path.display());
                        self.transcript.push(Line::from(line.clone()));
                        self.status = line;
                    }
                    Err(e) => self.status = i18n::fmt!(self.t("status.exec_failed"), e),
                }
            }
            Command::Lang(arg) => {
                let next = match arg.as_deref() {
                    Some("zh") | Some("中文") => Lang::Zh,
                    Some("en") | Some("english") => Lang::En,
                    Some(other) => {
                        self.status = i18n::fmt!(self.t("status.lang_unknown"), other);
                        return Ok(());
                    }
                    None => match self.lang {
                        Lang::Zh => Lang::En,
                        Lang::En => Lang::Zh,
                    },
                };
                self.lang = next;
                self.renderer = EventRenderer::with_lang(next);
                self.status = i18n::fmt!(self.t("status.lang_switched"), self.lang_name());
            }
            Command::Backtrack => {
                let Some(sid) = self.resume_session_id.clone() else {
                    self.status = self.t("status.backtrack_no_session").into();
                    return Ok(());
                };
                let timeline = self.host.get_session_timeline(sid.clone());
                let users: Vec<SessionMessage> =
                    timeline.into_iter().filter(|m| m.role == "user").collect();
                if users.is_empty() {
                    self.status = self.t("status.backtrack_no_messages").into();
                    return Ok(());
                }
                self.message_timeline = users;
                self.backtrack_source = Some(sid);
                self.selected = 0;
                self.mode = Mode::Picker(PickerKind::Messages);
                self.status = self.t("status.pick_backtrack").into();
            }
            Command::Delete => {
                let Some(sid) = self.resume_session_id.clone() else {
                    self.status = self.t("status.no_session").into();
                    return Ok(());
                };
                // Reuse the approval overlay for destructive-action second confirmation (tool field marks the delete route).
                self.approval = Some(ApprovalState {
                    session_id: sid,
                    tool: "delete-session".into(),
                    reason: self.t("approval.delete_session").into(),
                    remember: false,
                    request_id: None,
                });
                self.mode = Mode::Approval;
                self.status = self.t("status.delete_confirm").into();
            }
            Command::Worktree => {
                // Local toggle: the flag rides the next execute of this TUI.
                self.worktree_isolation = !self.worktree_isolation;
                let state = if self.worktree_isolation {
                    self.t("status.on")
                } else {
                    self.t("status.off")
                };
                self.status = i18n::fmt!(self.t("status.worktree_toggled"), state);
            }
            Command::Name(title) => match title {
                None => self.status = self.t("status.name_usage").into(),
                Some(title) => {
                    let Some(sid) = self.resume_session_id.clone() else {
                        self.status = self.t("status.no_session").into();
                        return Ok(());
                    };
                    match self.host.rename_session(sid.clone(), Some(title.clone())) {
                        Ok(()) => {
                            // Sync the local session list's new title to keep the /sessions view consistent
                            if let Some(s) = self.sessions.iter_mut().find(|s| s.id == sid) {
                                s.title = truncate_title(&title, 40);
                            }
                            self.status = i18n::fmt!(self.t("status.renamed"), title);
                        }
                        Err(e) => self.status = i18n::fmt!(self.t("status.rename_failed"), e),
                    }
                }
            },
            Command::Archive => {
                let Some(sid) = self.resume_session_id.clone() else {
                    self.status = self.t("status.no_session").into();
                    return Ok(());
                };
                // Soft archive is reversible (/unarchive restore), no second confirmation.
                match self.host.archive_session(sid.clone()) {
                    Ok(()) => {
                        self.sessions.retain(|s| s.id != sid);
                        self.clear_transcript();
                        self.resume_session_id = None;
                        self.status = self.t("status.archived").into();
                    }
                    Err(e) => self.status = i18n::fmt!(self.t("status.archive_failed"), e),
                }
            }
            Command::Unarchive => {
                self.archived_sessions = self.host.list_archived_sessions();
                if self.archived_sessions.is_empty() {
                    self.status = self.t("status.no_archived").into();
                    return Ok(());
                }
                self.selected = 0;
                self.mode = Mode::Picker(PickerKind::Archived);
                self.status = self.t("status.pick_unarchive").into();
            }
            Command::Review(files) => {
                if files.trim().is_empty() {
                    self.status = self.t("status.review_usage").into();
                    return Ok(());
                }
                let files_json =
                    match serde_json::to_string(&files.split_whitespace().collect::<Vec<_>>()) {
                        Ok(j) => j,
                        Err(e) => {
                            self.status = i18n::fmt!(self.t("status.exec_failed"), e);
                            return Ok(());
                        }
                    };
                match self.host.start_code_review(files_json, None) {
                    Ok(()) => {
                        // TUI owns the agent choice; /review keeps it.
                        self.prefill_prompt(format!(
                            "Please review the following files: {}",
                            files.trim()
                        ));
                    }
                    Err(e) => self.status = i18n::fmt!(self.t("status.exec_failed"), e),
                }
            }
            Command::Test(target) => {
                if target.trim().is_empty() {
                    self.status = self.t("status.test_usage").into();
                    return Ok(());
                }
                self.prefill_prompt(format!(
                    "Write unit tests for: {}. Generate comprehensive test cases covering normal cases, edge cases, and error handling.",
                    target.trim()
                ));
            }
            Command::Explore(query) => {
                if query.trim().is_empty() {
                    self.status = self.t("status.explore_usage").into();
                    return Ok(());
                }
                self.prefill_prompt(format!(
                    "Explore the codebase: {}. Use read and grep to understand the code. Do not modify any files.",
                    query.trim()
                ));
            }
            Command::Preset(arg) => match arg {
                // /preset <name>: switch directly (preset-default approval).
                // The documented mode aliases map onto the preset ids the
                // host actually knows: /build = full-tools standard,
                // /auto = goal (auto-adjudicated).
                Some(name) => {
                    let canonical = match name.as_str() {
                        "build" => "standard",
                        "auto" => "goal",
                        other => other,
                    };
                    match crate::preset::find(canonical) {
                        Some(p) => self.apply_preset(&p, None),
                        None => {
                            self.status =
                                i18n::fmt!(self.t("status.preset_not_found"), canonical);
                        }
                    }
                }
                // /preset: open the two-level preset picker.
                None => {
                    self.preset_approval_for = None;
                    self.selected = 0;
                    self.mode = Mode::Picker(PickerKind::Presets);
                    self.status = self.t("status.pick_preset").into();
                }
            },
            Command::Quit => self.should_quit = true,
            Command::Unknown(name) => {
                self.status = i18n::fmt!(self.t("status.unknown_cmd"), name);
            }
        }
        Ok(())
    }

    /// Picker confirmation: session → load history and continue; project → switch the active project.
    pub fn pick_select(&mut self) -> Result<()> {
        let kind = match self.mode {
            Mode::Picker(k) => k,
            _ => return Ok(()),
        };
        match kind {
            PickerKind::Sessions => {
                let Some(s) = self.sessions.get(self.selected).cloned() else {
                    return Ok(());
                };
                // WI-76: switching sessions invalidates queued prompts.
                if !self.queued_messages.is_empty() {
                    self.queued_messages.clear();
                    self.status = self.t("queue.dropped_session_gone").into();
                }
                let messages = self.host.get_session_messages(s.id.clone());
                self.clear_transcript();
                for m in messages {
 // parity: user rows echo with the `› ` prefix;
                    // assistant rows render plain.
                    if m.role == "user" {
                        self.transcript
                            .push(crate::events::user_echo_line(&m.content));
                    } else {
                        self.transcript.push(Line::from(m.content));
                    }
                }
                self.resume_session_id = Some(s.id.clone());
                self.mode = Mode::Idle;
                self.status = i18n::fmt!(self.t("status.session_selected"), s.title);
            }
            PickerKind::Workspaces => {
                let Some(p) = self.workspaces.get(self.selected).cloned() else {
                    return Ok(());
                };
                self.host
                    .set_active_workspace_for(&self.agent_name(), Some(p.id.clone()))?;
                self.after_workspace_switch(p);
            }
            PickerKind::Models => {
                let Some(provider_id) = self.model_picker_provider.clone() else {
                    // First level: choose provider
                    let Some(p) = self.model_entries.get(self.selected).cloned() else {
                        return Ok(());
                    };
                    self.model_picker_provider = Some(p.id.clone());
                    self.selected = 0;
                    self.status = i18n::fmt!(self.t("status.pick_model_for"), p.name);
                    return Ok(());
                };
                // Second level: choose a model and switch
                let Some(p) = self
                    .model_entries
                    .iter()
                    .find(|p| p.id == provider_id)
                    .cloned()
                else {
                    self.model_picker_provider = None;
                    self.mode = Mode::Idle;
                    return Ok(());
                };
                let variants = model_variants(&p);
                let Some(variant) = variants.get(self.selected).cloned() else {
                    return Ok(());
                };
                let agent = self.agent_name();
                match self
                    .host
                    .set_agent_active_model(agent, p.id.clone(), variant.clone())
                {
                    Ok(()) => {
                        self.current_model = Some(format!("{}/{}", p.id, variant));
                        self.model_picker_provider = None;
                        self.mode = Mode::Idle;
                        self.status = i18n::fmt!(
                            self.t("status.model_switched"),
                            format!("{}/{}", p.id, variant)
                        );
                    }
                    Err(e) => self.status = i18n::fmt!(self.t("status.model_switch_failed"), e),
                }
            }
            PickerKind::Messages => {
                let Some(src) = self.backtrack_source.clone() else {
                    self.mode = Mode::Idle;
                    return Ok(());
                };
                let Some(msg) = self.message_timeline.get(self.selected).cloned() else {
                    return Ok(());
                };
                // Truncation point = selected user message id (content before it is copied; the message itself is not, the user re-enters)
                let task = msg.content.clone();
                match self.host.fork_session(src, msg.id, task.clone()) {
                    Ok(info) => {
                        let messages = self.host.get_session_messages(info.id.clone());
                        self.clear_transcript();
                        for m in messages {
                            let role = if m.role == "user" {
                                self.t("role.user")
                            } else {
                                self.t("role.assistant")
                            };
                            self.transcript
                                .push(Line::from(format!("[{role}] {}", m.content)));
                        }
                        self.resume_session_id = Some(info.id.clone());
                        self.set_input(task); // prefill the original user message for editing and resubmission
                        self.message_timeline.clear();
                        self.backtrack_source = None;
                        self.mode = Mode::Idle;
                        self.status = i18n::fmt!(self.t("status.forked"), info.id);
                    }
                    Err(e) => self.status = i18n::fmt!(self.t("status.fork_failed"), e),
                }
            }
            PickerKind::Archived => {
                let Some(s) = self.archived_sessions.get(self.selected).cloned() else {
                    return Ok(());
                };
                match self.host.unarchive_session(s.id.clone()) {
                    Ok(()) => {
                        self.archived_sessions.retain(|x| x.id != s.id);
                        self.status = i18n::fmt!(self.t("status.unarchived"), s.title);
                        if self.archived_sessions.is_empty() {
                            self.mode = Mode::Idle;
                        }
                    }
                    Err(e) => self.status = i18n::fmt!(self.t("status.unarchive_failed"), e),
                }
            }
            // Two-level sandbox preset picker: level 1 = execution presets,
            // level 2 = approval override for the chosen preset (locked
            // presets such as plan are applied in one step).
            PickerKind::Presets => {
                if let Some(pid) = self.preset_approval_for.clone() {
                    let Some(p) = crate::preset::find(&pid) else {
                        self.preset_picker_back();
                        return Ok(());
                    };
                    let opts = crate::preset::approval_options(&p);
                    // Option 0 is "follow the preset default" (`None`) —
                    // `flatten()` erased the distinction between "selected
                    // the default row" and "no such row", making the
                    // documented first entry a dead key.
                    match opts.get(self.selected).copied() {
                        Some(choice) => self.apply_preset(&p, choice),
                        None => return Ok(()),
                    }
                } else {
                    let presets = crate::preset::presets();
                    let Some(p) = presets.get(self.selected) else {
                        return Ok(());
                    };
                    if crate::preset::is_single_step(p) {
                        self.apply_preset(p, None);
                    } else {
                        self.preset_approval_for = Some(p.id.to_string());
                        self.selected = 0;
                        self.status = i18n::fmt!(self.t("status.pick_preset_approval"), p.id);
                    }
                }
            }
        }
        Ok(())
    }

    pub fn pick_back(&mut self) {
        if matches!(self.mode, Mode::Picker(_)) {
            self.mode = Mode::Idle;
            self.status = self.t("status.back").into();
        }
    }

    pub fn move_selection(&mut self, delta: isize) {
        let len = match self.mode {
            Mode::Picker(PickerKind::Sessions) => self.sessions.len(),
            Mode::Picker(PickerKind::Workspaces) => self.workspaces.len(),
            Mode::Picker(PickerKind::Models) => {
                if self.model_picker_provider.is_some() {
                    self.model_entries
                        .iter()
                        .find(|p| Some(&p.id) == self.model_picker_provider.as_ref())
                        .map(model_variants)
                        .map(|v| v.len())
                        .unwrap_or(0)
                } else {
                    self.model_entries.len()
                }
            }
            Mode::Picker(PickerKind::Messages) => self.message_timeline.len(),
            Mode::Picker(PickerKind::Archived) => self.archived_sessions.len(),
            Mode::Picker(PickerKind::Presets) => match &self.preset_approval_for {
                Some(pid) => crate::preset::find(pid)
                    .map(|p| crate::preset::approval_options(&p).len())
                    .unwrap_or(0),
                None => crate::preset::presets().len(),
            },
            _ => return,
        };
        if len == 0 {
            return;
        }
        let next = self.selected as isize + delta;
        self.selected = next.rem_euclid(len as isize) as usize;
    }

    /// Unified cleanup after switching project: refresh the list and clear session context.
    fn after_workspace_switch(&mut self, p: WorkspaceInfo) {
        let pid = p.id.clone();
        self.current_workspace = Some(p);
        self.workspaces = self.host.list_workspaces();
        self.sessions = self.host.list_sessions(Some(pid));
        self.clear_transcript();
        self.resume_session_id = None;
        self.reset_input();
        self.mode = Mode::Idle;
        self.status = i18n::fmt!(
            self.t("status.project"),
            self.current_workspace
                .as_ref()
                .map(|x| x.title.as_str())
                .unwrap_or("-")
        );
    }

    /// Model picker back one level: model layer → provider layer → Idle.
    pub fn model_picker_back(&mut self) {
        if !matches!(self.mode, Mode::Picker(PickerKind::Models)) {
            return;
        }
        if self.model_picker_provider.is_some() {
            self.model_picker_provider = None;
            self.selected = 0;
            self.status = self.t("status.pick_provider").into();
        } else {
            self.mode = Mode::Idle;
            self.status = self.t("status.back").into();
        }
    }

    /// Preset picker back one level: approval level → preset level → Idle.
    pub fn preset_picker_back(&mut self) {
        if !matches!(self.mode, Mode::Picker(PickerKind::Presets)) {
            return;
        }
        if self.preset_approval_for.take().is_some() {
            self.selected = 0;
            self.status = self.t("status.pick_preset").into();
        } else {
            self.mode = Mode::Idle;
            self.status = self.t("status.back").into();
        }
    }

    /// Apply a sandbox preset: switch the execution mode (`session.setExecutionMode`
    /// — settings + kernel push), then optionally override the live session's
    /// approval mode (`session.setApprovalMode`). Locked presets (plan) ignore
    /// the approval argument; without a live session the preset default simply
    /// applies to future sessions.
    fn apply_preset(&mut self, p: &crate::preset::PresetInfo, approval: Option<&str>) {
        let approval = if crate::preset::is_single_step(p) {
            None
        } else {
            approval
        };
        match self.host.set_execution_mode(p.id.to_string()) {
            Ok(()) => {
                self.execution_mode = p.id.to_string();
                self.sandbox_mode = p.sandbox.to_string();
                self.preset_approval_for = None;
                self.mode = Mode::Idle;
                self.status = i18n::fmt!(
                    self.t("status.preset_switched"),
                    p.id,
                    p.sandbox,
                    approval.unwrap_or(p.approval)
                );
                if let (Some(mode), Some(sid)) = (
                    approval,
                    self.running_session_id
                        .clone()
                        .or_else(|| self.resume_session_id.clone()),
                ) {
                    if let Err(e) = self.host.set_approval_mode(sid, mode.to_string()) {
                        self.status = i18n::fmt!(self.t("status.preset_switch_failed"), e);
                    }
                }
            }
            Err(e) => self.status = i18n::fmt!(self.t("status.preset_switch_failed"), e),
        }
    }

    fn agent_name(&self) -> String {
        self.current_agent
            .as_ref()
            .map(|a| a.name.clone())
            .unwrap_or_else(|| CODING_AGENT_NAME.into())
    }

    /// /review /test /explore: prefill a prompt template into the input box and wait for Enter.
    fn prefill_prompt(&mut self, template: String) {
        self.set_input(template);
        self.cmd_hint_index = 0;
        self.status = self.t("status.prefilled").into();
    }

    /// Current language name (for `/lang` status feedback).
    fn lang_name(&self) -> &str {
        match self.lang {
            Lang::Zh => self.t("status.lang_zh"),
            Lang::En => self.t("status.lang_en"),
        }
    }

    /// `/status` report: mode/agent/model/project/session/counts, all through i18n.
    fn status_report_lines(&self) -> Vec<Line<'static>> {
        let mode: String = match self.mode {
            Mode::Idle => "Idle".to_string(),
            Mode::Running => "Running".to_string(),
            Mode::Picker(_) => self.t("mode.picker").to_string(),
            Mode::Approval => self.t("mode.approval").to_string(),
            Mode::Transcript => "Transcript".to_string(),
        };
        let mut lines = vec![Line::from(self.t("status.status_title"))];
        lines.push(Line::from(i18n::fmt!(
            self.t("status.agent"),
            self.agent_name()
        )));
        lines.push(Line::from(i18n::fmt!(self.t("status.mode_label"), mode)));
        lines.push(Line::from(i18n::fmt!(
            self.t("status.model_label"),
            self.current_model.clone().unwrap_or_else(|| "-".into())
        )));
        lines.push(Line::from(i18n::fmt!(
            self.t("status.preset_label"),
            self.execution_mode,
            self.sandbox_mode
        )));
        lines.push(Line::from(match &self.current_workspace {
            Some(p) => i18n::fmt!(self.t("status.current_workspace"), p.title, p.path),
            None => self.t("status.no_workspace").into(),
        }));
        lines.push(Line::from(match &self.resume_session_id {
            Some(sid) => i18n::fmt!(self.t("status.session"), sid),
            None => self.t("status.no_session").into(),
        }));
        lines.push(Line::from(i18n::fmt!(
            self.t("status.counts"),
            self.workspaces.len(),
            self.sessions.len()
        )));
        lines
    }

    pub fn shutdown(&mut self) {
        let _ = self.host.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::mock::MockHost;
    use serde_json::json;

    fn app_with(mock: &MockHost) -> AppState {
        // Fixed Chinese to avoid machine-locale influence in tests.
        AppState::with_lang(Box::new(mock.clone()), Lang::Zh).unwrap()
    }

    #[test]
    fn preset_command_switches_directly() {
        let mock = MockHost::default();
        let mut app = app_with(&mock);
        app.run_command(crate::commands::parse("/preset plan").unwrap())
            .unwrap();
        assert!(matches!(app.mode, Mode::Idle));
        assert_eq!(app.execution_mode, "plan");
        assert_eq!(app.sandbox_mode, "workspace-write");
        assert_eq!(
            mock.execution_modes.lock().unwrap().clone(),
            vec!["plan".to_string()]
        );
    }

    #[test]
    fn preset_picker_two_levels_and_back() {
        let mock = MockHost::default();
        let mut app = app_with(&mock);
        app.run_command(crate::commands::parse("/preset").unwrap())
            .unwrap();
        assert!(matches!(app.mode, Mode::Picker(PickerKind::Presets)));
        assert!(app.preset_approval_for.is_none());
        // Plan (row 1) is approval-locked → applied in one step.
        app.selected = 1;
        app.pick_select().unwrap();
        assert!(matches!(app.mode, Mode::Idle));
        assert_eq!(app.execution_mode, "plan");
        // Reopen: Standard (row 0) → second level (approval choices).
        app.run_command(crate::commands::parse("/preset").unwrap())
            .unwrap();
        app.selected = 0;
        app.pick_select().unwrap();
        assert_eq!(app.preset_approval_for.as_deref(), Some("standard"));
        // Esc from the approval level returns to the preset list; again → Idle.
        app.preset_picker_back();
        assert!(app.preset_approval_for.is_none());
        assert!(matches!(app.mode, Mode::Picker(PickerKind::Presets)));
        app.preset_picker_back();
        assert!(matches!(app.mode, Mode::Idle));
    }

    #[test]
    fn preset_picker_approval_choice_applies_override() {
        let mock = MockHost::default();
        let mut app = app_with(&mock);
        app.resume_session_id = Some("s1".into());
        app.run_command(crate::commands::parse("/preset").unwrap())
            .unwrap();
        app.selected = 0; // standard
        app.pick_select().unwrap(); // → approval level
        app.selected = 1; // "manual" (first choice after "follow preset")
        app.pick_select().unwrap();
        assert!(matches!(app.mode, Mode::Idle));
        assert_eq!(app.execution_mode, "standard");
        assert_eq!(
            mock.execution_modes.lock().unwrap().clone(),
            vec!["standard".to_string()]
        );
        assert_eq!(
            mock.approval_modes.lock().unwrap().clone(),
            vec![("s1".to_string(), "manual".to_string())]
        );
    }

    #[test]
    fn preset_switch_failure_and_unknown_name() {
        let mock = MockHost::default();
        *mock.fail_modes.lock().unwrap() = true;
        let mut app = app_with(&mock);
        app.run_command(crate::commands::parse("/preset goal").unwrap())
            .unwrap();
        assert_eq!(app.execution_mode, "standard"); // unchanged on failure
        assert!(app.status.contains("失败"));

        let mock2 = MockHost::default();
        let mut app2 = app_with(&mock2);
        app2.run_command(crate::commands::parse("/preset bogus").unwrap())
            .unwrap();
        assert!(app2.status.contains("bogus"));
        assert!(mock2.execution_modes.lock().unwrap().is_empty());
    }

    #[test]
    fn submit_prompt_starts_run() {
        let mock = MockHost::default();
        mock.with_events(16);
        let mut app = app_with(&mock);
        app.set_input("写个函数".into());
        app.submit().unwrap();
        assert_eq!(app.mode, Mode::Running);
        assert!(app.receiver.is_some());
    }

    #[test]
    fn execute_failure_stays_idle_with_status() {
        let mock = MockHost::default(); // no tx configured → execute fails
        let mut app = app_with(&mock);
        app.set_input("hi".into());
        app.submit().unwrap();
        assert_eq!(app.mode, Mode::Idle);
        assert!(app.status.contains("执行失败"));
    }

    #[test]
    fn events_render_and_task_completed_returns_idle() {
        let mock = MockHost::default();
        let tx = mock.with_events(16);
        let mut app = app_with(&mock);
        app.set_input("go".into());
        app.submit().unwrap();

        tx.send(json!({"type": "sessionStarted", "sessionId": "s1"}))
            .unwrap();
        tx.send(json!({"type": "assistantText", "sessionId": "s1", "text": "ok"}))
            .unwrap();
        tx.send(
            json!({"type": "turnEnd", "sessionId": "s1", "reason": "completed", "summary": "done"}),
        )
        .unwrap();
        app.tick().unwrap();

        assert_eq!(app.mode, Mode::Idle);
        assert!(app.status.contains("done"));
        assert!(app.transcript.iter().any(|l| l.to_string() == "ok"));
        assert_eq!(app.resume_session_id.as_deref(), Some("s1"));
    }

    #[test]
    fn cancel_records_session() {
        let mock = MockHost::default();
        mock.with_events(16);
        let mut app = app_with(&mock);
        app.set_input("go".into());
        app.submit().unwrap();
        let tx = mock.tx.lock().unwrap().clone().unwrap();
        tx.send(json!({"type": "sessionStarted", "sessionId": "s1"}))
            .unwrap();
        app.tick().unwrap();

        app.cancel_run();
        assert_eq!(*mock.cancelled.lock().unwrap(), vec!["s1".to_string()]);
    }

    #[test]
    fn approval_flow_roundtrip() {
        let mock = MockHost::default();
        let mut app = app_with(&mock);
        app.set_input("go".into());
        app.submit().unwrap();
        // Approval events are not projected in this slice; manual approve
        // still forwards to the host gate.
        app.approval = Some(ApprovalState {
            session_id: "s1".into(),
            tool: "bash".into(),
            reason: "危险操作".into(),
            remember: false,
            request_id: None,
        });
        app.approve(true).unwrap();
        assert_eq!(app.mode, Mode::Running);
        assert_eq!(
            *mock.approved.lock().unwrap(),
            vec![("s1".to_string(), true, Some(false))]
        );
    }

    #[test]
    fn approval_requested_sets_overlay_state() {
        let mock = MockHost::default();
        let mut app = app_with(&mock);
        let event = UiEvent::ApprovalRequested {
            version: UiEvent::VERSION,
            session_id: "s1".into(),
            request_id: "r1".into(),
            tool_name: "bash".into(),
            call_id: None,
            reason: Some("human".into()),
            params: None,
        };
        let applied = app
            .apply_event(serde_json::to_value(&event).unwrap())
            .unwrap();
        assert!(!applied);
        let a = app.approval.as_ref().unwrap();
        assert_eq!(a.session_id, "s1");
        assert_eq!(a.tool, "bash");
        assert_eq!(a.request_id.as_deref(), Some("r1"));
        assert!(!a.remember);
        assert_eq!(app.mode, Mode::Approval);
    }

    #[test]
    fn approval_requested_without_reason_defaults_empty() {
        let mock = MockHost::default();
        let mut app = app_with(&mock);
        let event = UiEvent::ApprovalRequested {
            version: UiEvent::VERSION,
            session_id: "s1".into(),
            request_id: "r2".into(),
            tool_name: "git".into(),
            call_id: None,
            reason: None,
            params: None,
        };
        app.apply_event(serde_json::to_value(&event).unwrap())
            .unwrap();
        let a = app.approval.as_ref().unwrap();
        assert_eq!(a.reason, "");
        assert_eq!(a.tool, "git");
    }

    #[test]
    fn approval_remember_toggle_keeps_request_id() {
        let mock = MockHost::default();
        let mut app = app_with(&mock);
        let event = UiEvent::ApprovalRequested {
            version: UiEvent::VERSION,
            session_id: "s1".into(),
            request_id: "r3".into(),
            tool_name: "bash".into(),
            call_id: None,
            reason: None,
            params: None,
        };
        app.apply_event(serde_json::to_value(&event).unwrap())
            .unwrap();
        app.toggle_remember();
        assert!(app.approval.as_ref().unwrap().remember);
        assert_eq!(
            app.approval.as_ref().unwrap().request_id.as_deref(),
            Some("r3")
        );
    }

    #[test]
    fn help_new_quit_commands() {
        let mock = MockHost::default();
        let mut app = app_with(&mock);
        app.set_input("/help".into());
        app.submit().unwrap();
        assert!(app.transcript.len() >= 6);

        app.set_input("/new".into());
        app.submit().unwrap();
        assert!(app.transcript.is_empty());
        assert_eq!(app.resume_session_id, None);

        app.set_input("/exit".into());
        app.submit().unwrap();
        assert!(app.should_quit);
    }

    #[test]
    fn sessions_command_opens_picker_and_select_resumes() {
        let mock = MockHost::default();
        mock.seed_session("s1", "第一个任务");
        mock.messages
            .lock()
            .unwrap()
            .push(starriver_host::types::ChatMessage {
                role: "user".into(),
                content: "你好".into(),
                ts: None,
                usage: None,
                duration_ms: None,
                done_ts: None,
                file_changes: None,
                seq: None,
            });
        let mut app = app_with(&mock);

        app.set_input("/sessions".into());
        app.submit().unwrap();
        assert_eq!(app.mode, Mode::Picker(PickerKind::Sessions));
        assert_eq!(app.sessions.len(), 1);

        app.pick_select().unwrap();
        assert_eq!(app.mode, Mode::Idle);
        assert_eq!(app.resume_session_id.as_deref(), Some("s1"));
        assert!(app
            .transcript
            .iter()
            .any(|l| l.to_string().contains("你好")));
    }

    #[test]
    fn projects_command_and_select_switches_project() {
        let mock = MockHost::default();
        mock.seed_workspace("p1", "/tmp/alpha");
        let mut app = app_with(&mock);

        app.set_input("/projects".into());
        app.submit().unwrap();
        assert_eq!(app.mode, Mode::Picker(PickerKind::Workspaces));

        app.pick_select().unwrap();
        assert_eq!(app.mode, Mode::Idle);
        assert_eq!(mock.active_workspace.lock().unwrap().as_deref(), Some("p1"));
        assert!(app.status.contains("alpha"));
        assert_eq!(app.resume_session_id, None);
    }

    #[test]
    fn project_by_name_switches_and_noarg_shows_current() {
        let mock = MockHost::default();
        mock.seed_workspace("p1", "/tmp/alpha");
        let mut app = app_with(&mock);

        app.set_input("/project alpha".into());
        app.submit().unwrap();
        assert_eq!(mock.active_workspace.lock().unwrap().as_deref(), Some("p1"));

        app.set_input("/project".into());
        app.submit().unwrap();
        assert!(app.status.contains("alpha"));
    }

    #[test]
    fn open_existing_dir_reuses_workspace() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("alpha");
        std::fs::create_dir(&path).expect("dir");
        let mock = MockHost::default();
        mock.seed_workspace("w1", &path.to_string_lossy());
        let mut app = app_with(&mock);

        app.set_input(format!("/open {}", path.display()));
        app.submit().unwrap();
        assert_eq!(mock.active_workspace.lock().unwrap().as_deref(), Some("w1"));
        assert_eq!(mock.workspaces.lock().unwrap().len(), 1); // idempotent: same path not registered twice
    }

    #[test]
    fn open_missing_dir_registers_nothing() {
        let mock = MockHost::default();
        let mut app = app_with(&mock);

 // semantics: only existing directories resolve (no mkdir).
        app.set_input("/open /nonexistent-starriver-dir-xyz".into());
        app.submit().unwrap_err(); // run_command propagates the registration error
        assert!(mock.workspaces.lock().unwrap().is_empty());
    }

    #[test]
    fn picker_navigation_wraps_and_back_returns_idle() {
        let mock = MockHost::default();
        mock.seed_workspace("p1", "/tmp/a");
        mock.seed_workspace("p2", "/tmp/b");
        let mut app = app_with(&mock);
        app.set_input("/projects".into());
        app.submit().unwrap();

        app.move_selection(-1); // 0 → 1 (wraps)
        assert_eq!(app.selected, 1);
        app.move_selection(1); // 1 → 0
        assert_eq!(app.selected, 0);

        app.pick_back();
        assert_eq!(app.mode, Mode::Idle);
    }

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn idle_keys_edit_input_and_submit() {
        let mock = MockHost::default();
        mock.with_events(16);
        let mut app = app_with(&mock);

        app.handle_key(key(KeyCode::Char('a'))).unwrap();
        app.handle_key(key(KeyCode::Char('b'))).unwrap();
        assert_eq!(app.input, "ab");
        app.handle_key(key(KeyCode::Backspace)).unwrap();
        assert_eq!(app.input, "a");

        app.handle_key(key(KeyCode::Enter)).unwrap();
        assert_eq!(app.mode, Mode::Running);
    }

    #[test]
    fn ctrl_c_quits_when_idle_and_cancels_when_running() {
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);

        // Idle: Ctrl+C exits
        let mock1 = MockHost::default();
        let mut app = app_with(&mock1);
        app.handle_key(ctrl_c).unwrap();
        assert!(app.should_quit);

        // Running: Ctrl+C cancels the task instead of exiting
        let mock2 = MockHost::default();
        let tx2 = mock2.with_events(16);
        let mut app2 = app_with(&mock2);
        app2.input = "go".into();
        app2.submit().unwrap();
        tx2.send(json!({"type": "sessionStarted", "sessionId": "s9"}))
            .unwrap();
        app2.tick().unwrap();
        app2.handle_key(ctrl_c).unwrap();
        assert!(!app2.should_quit);
        assert_eq!(*mock2.cancelled.lock().unwrap(), vec!["s9".to_string()]);
    }

    #[test]
    fn approval_keys_allow_deny_and_remember() {
        let mock = MockHost::default();
        let tx = mock.with_events(16);
        let mut app = app_with(&mock);
        app.set_input("go".into());
        app.submit().unwrap();
        app.approval = Some(ApprovalState {
            session_id: "s1".into(),
            tool: "edit".into(),
            reason: "改文件".into(),
            remember: false,
            request_id: None,
        });
        app.mode = Mode::Approval;
        app.handle_key(key(KeyCode::Char('m'))).unwrap();
        assert!(app.approval.as_ref().unwrap().remember);
        app.handle_key(key(KeyCode::Char('y'))).unwrap();
        assert_eq!(app.mode, Mode::Running);
        assert_eq!(mock.approved.lock().unwrap()[0].2, Some(true));
    }

    #[test]
    fn picker_keys_navigate_and_escape() {
        let mock = MockHost::default();
        mock.seed_workspace("p1", "/tmp/a");
        mock.seed_workspace("p2", "/tmp/b");
        let mut app = app_with(&mock);
        app.set_input("/projects".into());
        app.submit().unwrap();

        app.handle_key(key(KeyCode::Down)).unwrap();
        assert_eq!(app.selected, 1);
        app.handle_key(key(KeyCode::Esc)).unwrap();
        assert_eq!(app.mode, Mode::Idle);
    }

    // -- Command hint: up/down select + Enter execute --

    #[test]
    fn command_hint_arrows_select_and_enter_runs() {
        let mock = MockHost::default();
        let mut app = app_with(&mock);
        app.set_input("\\".into());
        // Full list: help(0) new(1) sessions(2)…
        app.handle_key(key(KeyCode::Down)).unwrap();
        app.handle_key(key(KeyCode::Down)).unwrap();
        assert_eq!(app.cmd_hint_index, 2);
        app.handle_key(key(KeyCode::Enter)).unwrap();
        assert_eq!(app.mode, Mode::Picker(PickerKind::Sessions));
    }

    #[test]
    fn command_hint_up_wraps_to_last() {
        let mock = MockHost::default();
        let mut app = app_with(&mock);
        app.set_input("/".into());
        app.handle_key(key(KeyCode::Up)).unwrap();
        app.handle_key(key(KeyCode::Enter)).unwrap();
        assert!(app.should_quit); // full list: Up wraps to the last item, exit
    }

    #[test]
    fn command_hint_typing_resets_selection_and_filters() {
        let mock = MockHost::default();
        let mut app = app_with(&mock);
        app.set_input("\\".into());
        app.handle_key(key(KeyCode::Down)).unwrap(); // selects new
        app.handle_key(key(KeyCode::Char('m'))).unwrap(); // typing "\m" resets selection to 0 and filters
        assert_eq!(app.cmd_hint_index, 0);
        app.handle_key(key(KeyCode::Enter)).unwrap();
        assert_eq!(app.mode, Mode::Picker(PickerKind::Models));
    }

    #[test]
    fn command_hint_full_command_with_args_runs_as_is() {
        let mock = MockHost::default();
        mock.seed_model_entry("deepseek", "DeepSeek", &["deepseek-v4-flash"]);
        let mut app = app_with(&mock);
        app.set_input("/model deepseek deepseek-v4-flash".into());
        app.handle_key(key(KeyCode::Enter)).unwrap();
        // An already-executable command is not overridden by the selection
        assert_eq!(app.mode, Mode::Idle);
        assert_eq!(
            app.current_model.as_deref(),
            Some("deepseek/deepseek-v4-flash")
        );
    }

    #[test]
    fn arrows_scroll_transcript_when_hint_hidden() {
        let mock = MockHost::default();
        let mut app = app_with(&mock);
 // Bottom-anchored (-style): ↑ moves AWAY from the tail,
        // ↓ walks back toward it; 0 = follow the newest line.
        app.scroll = 5;
        app.handle_key(key(KeyCode::Up)).unwrap();
        assert_eq!(app.scroll, 6);
        app.handle_key(key(KeyCode::Down)).unwrap();
        assert_eq!(app.scroll, 5);
        assert_eq!(app.cmd_hint_index, 0); // non-command input does not move the selection
    }

    // -- State machine boundaries --

    #[test]
    fn submit_while_running_is_noop_and_keeps_input() {
        let mock = MockHost::default();
        mock.with_events(16);
        let mut app = app_with(&mock);
        app.set_input("first".into());
        app.submit().unwrap();
        assert_eq!(app.mode, Mode::Running);

        app.set_input("second".into());
        app.submit().unwrap();
        assert_eq!(app.mode, Mode::Running);
        assert_eq!(app.input, "second"); // not consumed
        assert_eq!(mock.executed.lock().unwrap().len(), 1);
    }

    #[test]
    fn submit_empty_input_is_noop() {
        let mock = MockHost::default();
        let mut app = app_with(&mock);
        app.set_input("   ".into());
        app.submit().unwrap();
        assert_eq!(app.mode, Mode::Idle);
        assert!(mock.executed.lock().unwrap().is_empty());
    }

    #[test]
    fn unknown_command_sets_status() {
        let mock = MockHost::default();
        let mut app = app_with(&mock);
        app.set_input("/bogus".into());
        app.submit().unwrap();
        assert_eq!(app.mode, Mode::Idle);
        assert!(app.status.contains("未知命令"));
    }

    #[test]
    fn tick_without_receiver_is_noop() {
        let mock = MockHost::default();
        let mut app = app_with(&mock);
        app.status = "保持".into();
        app.tick().unwrap();
        assert_eq!(app.status, "保持");
        assert_eq!(app.mode, Mode::Idle);
    }

    #[test]
    fn cancel_run_while_idle_is_noop() {
        let mock = MockHost::default();
        let mut app = app_with(&mock);
        app.cancel_run();
        assert!(mock.cancelled.lock().unwrap().is_empty());
        assert_eq!(app.mode, Mode::Idle);
    }

    #[test]
    fn approve_and_toggle_without_approval_are_noop() {
        let mock = MockHost::default();
        let mut app = app_with(&mock);
        app.approve(true).unwrap();
        app.toggle_remember();
        assert!(mock.approved.lock().unwrap().is_empty());
        assert!(app.approval.is_none());
    }

    #[test]
    fn pick_select_outside_picker_is_noop() {
        let mock = MockHost::default();
        let mut app = app_with(&mock);
        app.pick_select().unwrap();
        assert_eq!(app.mode, Mode::Idle);
    }

    #[test]
    fn pick_select_empty_sessions_stays_in_picker() {
        let mock = MockHost::default();
        let mut app = app_with(&mock);
        app.set_input("/sessions".into());
        app.submit().unwrap();
        assert!(app.sessions.is_empty());
        app.pick_select().unwrap();
        assert_eq!(app.mode, Mode::Picker(PickerKind::Sessions));
    }

    #[test]
    fn move_selection_empty_or_non_picker_is_noop() {
        let mock = MockHost::default();
        let mut app = app_with(&mock);
        app.move_selection(1);
        assert_eq!(app.selected, 0);

        app.set_input("/sessions".into());
        app.submit().unwrap();
        app.move_selection(-1);
        assert_eq!(app.selected, 0); // empty list: no movement
        app.move_selection(1);
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn running_ignores_other_keys() {
        let mock = MockHost::default();
        mock.with_events(16);
        let mut app = app_with(&mock);
        app.set_input("go".into());
        app.submit().unwrap();
        app.handle_key(key(KeyCode::Char('x'))).unwrap();
        app.handle_key(key(KeyCode::Enter)).unwrap();
        assert_eq!(app.mode, Mode::Running);
        assert!(mock.cancelled.lock().unwrap().is_empty());
    }

    #[test]
    fn malformed_event_sets_status_and_survives() {
        let mock = MockHost::default();
        let tx = mock.with_events(16);
        let mut app = app_with(&mock);
        app.set_input("go".into());
        app.submit().unwrap();
        tx.send(json!({"type": "NotARealEvent"})).unwrap();
        app.tick().unwrap();
        assert_eq!(app.mode, Mode::Running); // did not crash
        assert!(app.status.contains("事件解析失败"));

        tx.send(
            json!({"type": "turnEnd", "sessionId": "s1", "reason": "completed", "summary": "done"}),
        )
        .unwrap();
        app.tick().unwrap();
        assert_eq!(app.mode, Mode::Idle);
    }

    #[test]
    fn receiver_lagged_sets_status() {
        let mock = MockHost::default();
        let tx = mock.with_events(2); // small capacity to trigger Lagged
        let mut app = app_with(&mock);
        app.set_input("go".into());
        app.submit().unwrap();
        tx.send(json!({"type": "sessionStarted", "sessionId": "s1"}))
            .unwrap();
        tx.send(json!({"type": "assistantText", "sessionId": "s1", "text": "a"}))
            .unwrap();
        tx.send(json!({"type": "assistantText", "sessionId": "s1", "text": "b"}))
            .unwrap();
        app.tick().unwrap();
        assert!(app.status.contains("落后") || app.status.contains("丢弃"));
        assert_eq!(app.mode, Mode::Running);
    }

    #[test]
    fn receiver_closed_returns_idle() {
        let mock = MockHost::default();
        let tx = mock.with_events(16);
        let mut app = app_with(&mock);
        app.set_input("go".into());
        app.submit().unwrap();
        tx.send(json!({"type": "sessionStarted", "sessionId": "s1"}))
            .unwrap();
        app.tick().unwrap();

        // Close all senders: the external handle + the handle stored in the mock
        *mock.tx.lock().unwrap() = None;
        drop(tx);
        app.tick().unwrap();
        assert_eq!(app.mode, Mode::Idle);
        assert!(app.status.contains("关闭"));
    }

    #[test]
    fn task_failed_sets_resume_session() {
        let mock = MockHost::default();
        let tx = mock.with_events(16);
        let mut app = app_with(&mock);
        app.set_input("go".into());
        app.submit().unwrap();
        tx.send(json!({"type": "sessionStarted", "sessionId": "s7"}))
            .unwrap();
        tx.send(json!({"type": "turnError", "sessionId": "s7", "error": "boom"}))
            .unwrap();
        app.tick().unwrap();
        assert_eq!(app.mode, Mode::Idle);
        assert_eq!(app.resume_session_id.as_deref(), Some("s7"));
        assert!(app.status.contains("失败"));
    }

    #[test]
    fn task_cancelled_keeps_resume_session() {
        let mock = MockHost::default();
        let tx = mock.with_events(16);
        let mut app = app_with(&mock);
        app.set_input("go".into());
        app.submit().unwrap();
        tx.send(json!({"type": "sessionStarted", "sessionId": "s7"}))
            .unwrap();
        tx.send(json!({"type": "cancelled", "sessionId": "s7"}))
            .unwrap();
        app.tick().unwrap();
        assert_eq!(app.mode, Mode::Idle);
        assert_eq!(app.resume_session_id.as_deref(), Some("s7"));
        assert!(app.status.contains("已取消"));
    }

    #[test]
    fn approval_without_task_started_uses_empty_session() {
        let mock = MockHost::default();
        let tx = mock.with_events(16);
        let mut app = app_with(&mock);
        app.set_input("go".into());
        app.approval = Some(ApprovalState {
            session_id: String::new(),
            tool: "bash".into(),
            reason: "r".into(),
            remember: false,
            request_id: None,
        });
        assert_eq!(app.approval.as_ref().unwrap().session_id, "");
    }

    #[test]
    fn resume_prompt_passes_session_id_to_execute() {
        let mock = MockHost::default();
        mock.seed_session("s1", "t");
        let mut app = app_with(&mock);
        app.set_input("/sessions".into());
        app.submit().unwrap();
        app.pick_select().unwrap();

        mock.with_events(16);
        app.set_input("继续".into());
        app.submit().unwrap();
        let calls = mock.executed.lock().unwrap();
        let (task, sid) = calls.last().unwrap();
        assert_eq!(task, "继续");
        assert_eq!(sid.as_deref(), Some("s1"));
    }

    #[test]
    fn project_not_found_sets_status() {
        let mock = MockHost::default();
        let mut app = app_with(&mock);
        app.set_input("/project nope".into());
        app.submit().unwrap();
        assert!(app.status.contains("项目不存在"));
    }

    #[test]
    fn open_empty_path_registers_nothing() {
        let mock = MockHost::default();
        let mut app = app_with(&mock);
        app.set_input("/open".into());
        // An empty path is not an existing directory → registration fails, no workspace created
        app.submit().unwrap_err();
        assert!(mock.workspaces.lock().unwrap().is_empty());
    }

    #[test]
    fn project_noarg_without_current_shows_none() {
        let mock = MockHost::default();
        let mut app = app_with(&mock);
        app.set_input("/project".into());
        app.submit().unwrap();
        assert!(app.status.contains("当前工作区：无"));
    }

    #[test]
    fn picker_selects_second_session() {
        let mock = MockHost::default();
        mock.seed_session("s1", "a");
        mock.seed_session("s2", "b");
        let mut app = app_with(&mock);
        app.set_input("/sessions".into());
        app.submit().unwrap();
        app.selected = 1;
        app.pick_select().unwrap();
        assert_eq!(app.resume_session_id.as_deref(), Some("s2"));
    }

    // -- /model command --

    #[test]
    fn model_command_opens_two_level_picker_and_switches() {
        let mock = MockHost::default();
        mock.seed_model_entry(
            "deepseek",
            "DeepSeek",
            &["deepseek-v4-flash", "deepseek-v4-pro"],
        );
        mock.seed_model_entry("zhipu", "GLM", &["glm-4-plus"]);
        let mut app = app_with(&mock);

        app.set_input("/model".into());
        app.submit().unwrap();
        assert_eq!(app.mode, Mode::Picker(PickerKind::Models));
        assert_eq!(app.model_picker_provider, None);

        // First level: choose DeepSeek
        app.pick_select().unwrap();
        assert_eq!(app.model_picker_provider.as_deref(), Some("deepseek"));
        assert_eq!(app.selected, 0);

        // Second level: choose deepseek-v4-pro
        app.selected = 1;
        app.pick_select().unwrap();
        assert_eq!(app.mode, Mode::Idle);
        assert_eq!(
            *mock.set_active.lock().unwrap(),
            vec![(
                CODING_AGENT_NAME.to_string(),
                "deepseek".to_string(),
                "deepseek-v4-pro".to_string()
            )]
        );
        assert_eq!(
            app.current_model.as_deref(),
            Some("deepseek/deepseek-v4-pro")
        );
        assert!(app.status.contains("deepseek-v4-pro"));
    }

    #[test]
    fn model_direct_switch_with_args() {
        let mock = MockHost::default();
        mock.seed_model_entry("deepseek", "DeepSeek", &["deepseek-v4-flash"]);
        let mut app = app_with(&mock);

        app.set_input("/model deepseek deepseek-v4-flash".into());
        app.submit().unwrap();
        assert_eq!(app.mode, Mode::Idle);
        assert_eq!(
            *mock.set_active.lock().unwrap(),
            vec![(
                CODING_AGENT_NAME.to_string(),
                "deepseek".to_string(),
                "deepseek-v4-flash".to_string()
            )]
        );
        assert_eq!(
            app.current_model.as_deref(),
            Some("deepseek/deepseek-v4-flash")
        );
    }

    #[test]
    fn model_provider_arg_enters_model_level() {
        let mock = MockHost::default();
        mock.seed_model_entry(
            "deepseek",
            "DeepSeek",
            &["deepseek-v4-flash", "deepseek-v4-pro"],
        );
        let mut app = app_with(&mock);

        app.set_input("/model deepseek".into());
        app.submit().unwrap();
        assert_eq!(app.mode, Mode::Picker(PickerKind::Models));
        assert_eq!(app.model_picker_provider.as_deref(), Some("deepseek"));
    }

    #[test]
    fn model_unknown_provider_sets_status() {
        let mock = MockHost::default();
        let mut app = app_with(&mock);
        app.set_input("/model nope".into());
        app.submit().unwrap();
        assert_eq!(app.mode, Mode::Idle);
        assert!(app.status.contains("provider 不存在"));
    }
    #[test]
    fn model_picker_esc_walks_back_to_idle() {
        let mock = MockHost::default();
        mock.seed_model_entry("deepseek", "DeepSeek", &["deepseek-v4-flash"]);
        let mut app = app_with(&mock);
        app.set_input("/model".into());
        app.submit().unwrap();

        app.pick_select().unwrap(); // enter the model layer
        assert_eq!(app.model_picker_provider.as_deref(), Some("deepseek"));
        app.model_picker_back(); // back to the provider layer
        assert_eq!(app.model_picker_provider, None);
        assert_eq!(app.mode, Mode::Picker(PickerKind::Models));
        app.model_picker_back(); // back to Idle
        assert_eq!(app.mode, Mode::Idle);
    }

    #[test]
    fn current_model_loaded_from_bindings_at_startup() {
        let mock = MockHost::default();
        mock.bindings
            .lock()
            .unwrap()
            .push(starriver_host::types::AgentModelBinding {
                agent_name: CODING_AGENT_NAME.into(),
                active_model: "deepseek/deepseek-v4-pro".into(),
                bound_models: vec![],
                reasoning_effort: None,
            });
        let app = app_with(&mock);
        assert_eq!(
            app.current_model.as_deref(),
            Some("deepseek/deepseek-v4-pro")
        );
    }

    // -- New commands: /compact /diff /status /usage /init /export /lang --

    /// Construct an app with a project working directory (required by /diff and /init).
    fn app_with_project(mock: &MockHost, cwd: &str) -> AppState {
        mock.seed_workspace("p1", cwd);
        let mut app = app_with(mock);
        app.current_workspace = Some(mock.workspaces.lock().unwrap()[0].clone());
        app
    }

    #[test]
    fn compact_command_compacts_resume_session() {
        let mock = MockHost::default();
        let mut app = app_with(&mock);
        app.resume_session_id = Some("s1".into());
        app.set_input("/compact".into());
        app.submit().unwrap();
        assert_eq!(*mock.compacted.lock().unwrap(), vec!["s1".to_string()]);
        assert!(app.status.contains("120 → 40"), "status: {}", app.status);
        assert!(app
            .transcript
            .iter()
            .any(|l| l.to_string().contains("120 → 40")));
    }

    #[test]
    fn compact_without_session_sets_status() {
        let mock = MockHost::default();
        let mut app = app_with(&mock);
        app.set_input("/compact".into());
        app.submit().unwrap();
        assert!(mock.compacted.lock().unwrap().is_empty());
        assert!(app.status.contains("没有当前会话"));
    }

    #[test]
    fn diff_command_renders_workspace_diff() {
        let mock = MockHost::default();
        let mut app = app_with_project(&mock, "/tmp/alpha");
        app.set_input("/diff".into());
        app.submit().unwrap();
        assert_eq!(
            *mock.diff_calls.lock().unwrap(),
            vec!["/tmp/alpha".to_string()]
        );
        assert!(app
            .transcript
            .iter()
            .any(|l| l.to_string().contains("diff --git")));
    }

    #[test]
    fn diff_without_project_sets_status() {
        let mock = MockHost::default();
        let mut app = app_with(&mock);
        app.set_input("/diff".into());
        app.submit().unwrap();
        assert!(app.status.contains("未设置项目工作目录"));
    }

    #[test]
    fn status_command_renders_report() {
        let mock = MockHost::default();
        mock.seed_workspace("p1", "/tmp/alpha");
        mock.seed_session("s1", "t");
        let mut app = app_with(&mock);
        app.current_workspace = Some(mock.workspaces.lock().unwrap()[0].clone());
        app.resume_session_id = Some("s1".into());
        app.set_input("/status".into());
        app.submit().unwrap();
        let joined: String = app.transcript.iter().map(|l| l.to_string()).collect();
        assert!(joined.contains("状态"));
        assert!(joined.contains("Starriver Code"));
        assert!(joined.contains("alpha"));
        assert!(
            joined.contains("工作区 1 个 / 会话 1 个"),
            "joined: {joined}"
        );
    }

    #[test]
    fn usage_command_reports_usage() {
        let mock = MockHost::default();
        let mut app = app_with(&mock);
        app.resume_session_id = Some("s1".into());
        app.set_input("/usage".into());
        app.submit().unwrap();
        assert_eq!(*mock.usage_calls.lock().unwrap(), vec!["s1".to_string()]);
        assert!(app.status.contains("500"), "status: {}", app.status);
    }

    #[test]
    fn usage_without_session_sets_status() {
        let mock = MockHost::default();
        let mut app = app_with(&mock);
        app.set_input("/usage".into());
        app.submit().unwrap();
        assert!(mock.usage_calls.lock().unwrap().is_empty());
        assert!(app.status.contains("没有当前会话"));
    }

    #[test]
    fn init_command_generates_agents_md() {
        let mock = MockHost::default();
        let mut app = app_with_project(&mock, "/tmp/alpha");
        app.set_input("/init".into());
        app.submit().unwrap();
        assert_eq!(
            *mock.init_calls.lock().unwrap(),
            vec!["/tmp/alpha".to_string()]
        );
        assert!(app.status.contains("AGENTS.md"), "status: {}", app.status);
    }

    #[test]
    fn export_command_exports_session() {
        let mock = MockHost::default();
        let mut app = app_with_project(&mock, "/tmp/alpha");
        app.resume_session_id = Some("s1".into());
        app.set_input("/export".into());
        app.submit().unwrap();
        assert_eq!(
            *mock.export_calls.lock().unwrap(),
            vec![("s1".to_string(), "/tmp/alpha".to_string())]
        );
        assert!(app.status.contains("已导出"), "status: {}", app.status);
        assert!(
            app.status.contains("starriver-export-s1.md"),
            "status: {}",
            app.status
        );
    }

    #[test]
    fn export_without_session_sets_status() {
        let mock = MockHost::default();
        let mut app = app_with_project(&mock, "/tmp/alpha");
        app.set_input("/export".into());
        app.submit().unwrap();
        assert!(mock.export_calls.lock().unwrap().is_empty());
        assert!(app.status.contains("没有当前会话"));
    }

    #[test]
    fn lang_command_toggles_and_sets() {
        let mock = MockHost::default();
        let mut app = app_with(&mock); // Zh

        // No arg: toggle → En
        app.set_input("/lang".into());
        app.submit().unwrap();
        assert_eq!(app.lang, Lang::En);
        assert!(app.status.contains("English"), "status: {}", app.status);

        // Explicit zh
        app.set_input("/lang zh".into());
        app.submit().unwrap();
        assert_eq!(app.lang, Lang::Zh);
        assert!(app.status.contains("中文"), "status: {}", app.status);

        // Unknown language
        app.set_input("/lang de".into());
        app.submit().unwrap();
        assert_eq!(app.lang, Lang::Zh); // unchanged
        assert!(app.status.contains("未知语言"));
    }

    #[test]
    fn diff_command_error_path_sets_status() {
        let mock = MockHost::default();
        let mut app = app_with_project(&mock, "/tmp/norepo");
        app.set_input("/diff".into());
        app.submit().unwrap();
        assert!(app.status.contains("执行失败"), "status: {}", app.status);
        assert!(app.status.contains("not a git repository"));
    }
}
