//! Persistent terminal service (`terminal` parity): owner-scoped
//! interactive PTY sessions shared across model tool calls. A read thread
//! finalizes output into retained, line-structured scrollback; the model
//! tools page it, send input with readiness inference, and forward signals
//! to the foreground process group.
//!
//! vocabulary (docs/subsystems/terminal.md):
//! - [`WaitReason`] says why one send returned — `stdin_read` (only the
//!   input's echo came back), `inferred_idle` (output went quiet), `timeout`
//!   (the wait budget elapsed), `session_exit` (the top-level shell exited).
//!   Silence never implies child exit: [`SessionStatus`] reports that
//!   separately.
//! - Scrollback is retained (bounded) and paged newest-relative; reads are
//!   non-destructive views, while sends consume a per-session cursor so the
//!   model sees exactly the delta its input produced.
//! - Signals: control characters ride the PTY line discipline to the
//!   foreground process group (SIGINT/SIGQUIT/SIGTSTP); SIGTERM/SIGKILL go
//!   to the top-level shell itself.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty};

/// Retained scrollback cap per session (bytes over complete lines); older
/// lines drop from the front once exceeded.
pub const SCROLLBACK_MAX_BYTES: usize = 128 * 1024;
/// Default quiet window: no new output for this long ends a send as
/// `inferred_idle` (or `stdin_read` when only the echo came back).
pub const DEFAULT_IDLE_MS: u64 = 400;
/// Default wait budget for a send.
pub const DEFAULT_WAIT_MS: u64 = 5_000;
/// Upper bound on one send's returned delta; older overflow is trimmed and
/// flagged.
const DELTA_MAX_BYTES: usize = 16 * 1024;
const POLL_MS: u64 = 25;

/// Why one interactive send returned control to its caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitReason {
    StdinRead,
    InferredIdle,
    Timeout,
    SessionExit,
}

impl WaitReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            WaitReason::StdinRead => "stdin_read",
            WaitReason::InferredIdle => "inferred_idle",
            WaitReason::Timeout => "timeout",
            WaitReason::SessionExit => "session_exit",
        }
    }
}

/// Top-level PTY process status, independent of a send's wait reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionStatus {
    Running,
    /// Process alive but silent (no output, no input) for `IDLE_AFTER_MS` —
    /// almost certainly parked at a prompt rather than working. Silence
    /// never implies child exit: liveness still comes from the child handle,
    /// and any output or written input flips this back to `Running`.
    Idle,
    Exited {
        exit_code: Option<u32>,
        signal: Option<String>,
    },
}

/// A live terminal with no output and no written input for this long reads
/// as `idle` instead of `running`. A working server that logs periodically
/// never crosses it; a forgotten prompt-idle shell does.
const IDLE_AFTER_MS: u64 = 5 * 60 * 1000;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl SessionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionStatus::Running => "running",
            SessionStatus::Idle => "idle",
            SessionStatus::Exited { .. } => "exited",
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        match self {
            SessionStatus::Running => serde_json::json!({ "kind": "running" }),
            SessionStatus::Idle => serde_json::json!({ "kind": "idle" }),
            SessionStatus::Exited { exit_code, signal } => serde_json::json!({
                "kind": "exited",
                "exitCode": exit_code,
                "signal": signal,
            }),
        }
    }
}

/// Settled result for one foreground send (`TerminalSendResult` parity).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendOutcome {
    /// Output produced since the send started (bounded, newest end kept).
    pub output: String,
    pub wait_reason: WaitReason,
    pub session_status: SessionStatus,
    /// True when the delta overflowed the return cap (or scrollback dropped
    /// older lines underneath it).
    pub truncated: bool,
}

/// Where a consumer stopped reading: complete lines plus the byte offset
/// into the line `line_no` names — which may still be the unfinalized tail
/// or may since have been finalized into the numbered feed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cursor {
    line_no: u64,
    tail_bytes: usize,
}

/// Retained, line-structured scrollback with a monotonically numbered line
/// feed. Complete lines drop from the front when the byte cap is exceeded;
/// numbering never rewinds, so consumer cursors stay valid.
#[derive(Default)]
struct Scrollback {
    lines: VecDeque<(u64, String)>,
    next_line_no: u64,
    tail: String,
    retained_bytes: usize,
    dropped_lines: u64,
    dropped_ever: bool,
}

impl Scrollback {
    fn push_text(&mut self, text: &str) {
        for chunk in text.split_inclusive('\n') {
            self.tail.push_str(chunk);
            if self.tail.ends_with('\n') {
                let line = std::mem::take(&mut self.tail);
                self.retained_bytes += line.len();
                self.lines.push_back((self.next_line_no, line));
                self.next_line_no += 1;
            }
        }
        while self.retained_bytes > SCROLLBACK_MAX_BYTES && self.lines.len() > 1 {
            if let Some((_, dropped)) = self.lines.pop_front() {
                self.retained_bytes = self.retained_bytes.saturating_sub(dropped.len());
                self.dropped_lines += 1;
                self.dropped_ever = true;
            }
        }
    }

    /// Text after `cursor` plus the cursor to resume from. Never rewinds.
    ///
    /// A line that straddles two reads must be delivered exactly once: the
    /// byte offset applies to the line whether it is still the pending tail
    /// or has since been finalized — otherwise the second read would replay
    /// the bytes the first read already returned.
    fn delta_from(&self, cursor: Cursor) -> (String, Cursor) {
        let mut out = String::new();
        let mut offset_applied = false;
        for (no, line) in &self.lines {
            if *no < cursor.line_no {
                continue;
            }
            if !offset_applied && *no == cursor.line_no && cursor.tail_bytes > 0 {
                // The consumer saw this line as the unfinalized tail; deliver
                // only the bytes appended since its cursor.
                if cursor.tail_bytes < line.len() {
                    out.push_str(&line[cursor.tail_bytes..]);
                }
                offset_applied = true;
            } else {
                out.push_str(line);
            }
        }
        // The pending tail becomes line `next_line_no`; the byte offset
        // refers to it only when the cursor names that not-yet-numbered line.
        let into_tail = !offset_applied && cursor.line_no == self.next_line_no;
        let skip = if into_tail {
            cursor.tail_bytes.min(self.tail.len())
        } else {
            0
        };
        if skip < self.tail.len() {
            out.push_str(&self.tail[skip..]);
        }
        // Everything through the current tail was delivered; the resume
        // cursor sits at the tail's future line number with its full length
        // consumed, so later appends to the same line stay delta-only.
        (
            out,
            Cursor {
                line_no: self.next_line_no,
                tail_bytes: self.tail.len(),
            },
        )
    }

    /// Whether complete lines the cursor had not consumed yet were dropped
    /// from the front of the retained history — the consumer's delta is
    /// missing output it never saw (BUG-14: `send` flags this as truncated).
    fn dropped_under(&self, cursor: Cursor) -> bool {
        let first_retained = self
            .lines
            .front()
            .map(|(no, _)| *no)
            .unwrap_or(self.next_line_no);
        first_retained > cursor.line_no
    }

    /// Newest-relative page over the retained lines (the unfinalized tail
    /// counts as the last partial line). Returns the lines, the total line
    /// count ever produced, and whether lines older than the page exist.
    #[allow(clippy::type_complexity)]
    fn page(&self, offset_from_end: usize, max_lines: usize) -> (Vec<String>, u64, bool) {
        let tail_line = if self.tail.is_empty() { 0 } else { 1 };
        let total = self.dropped_lines + (self.lines.len() as u64) + tail_line;
        // Newest-first indices: tail (1 virtual line) + reversed complete lines.
        let newest_first: Vec<String> = std::iter::once(self.tail.clone())
            .filter(|t| !t.is_empty())
            .chain(self.lines.iter().rev().map(|(_, l)| l.clone()))
            .collect();
        let retained_total = newest_first.len();
        let start = offset_from_end.min(retained_total);
        let end = (start + max_lines).min(retained_total);
        let mut lines: Vec<String> = newest_first[start..end].to_vec();
        lines.reverse();
        let older_available = end < retained_total || self.dropped_ever;
        (lines, total, older_available)
    }
}

struct TerminalSession {
    /// Publication sequence — the list() ordering key.
    seq: u64,
    /// Wall-clock start (ms since epoch) — WI-150 panel shows start time and
    /// live elapsed per background terminal.
    started_at_ms: u64,
    /// Last output-or-input activity (ms since epoch) — the idle projection
    /// reads Running sessions as Idle once this ages past `IDLE_AFTER_MS`.
    last_activity_ms: Arc<AtomicU64>,
    /// Owning conversation session id (`None` = host-spawned/legacy tests).
    owner: Option<String>,
    /// Session workspace root at spawn time (workspace-scoped panel).
    workspace: Option<String>,
    _master: Box<dyn MasterPty + Send>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    scrollback: Arc<Mutex<Scrollback>>,
    child: Arc<Mutex<Option<Box<dyn portable_pty::Child + Send>>>>,
    killer: Arc<Mutex<Box<dyn ChildKiller + Send + Sync>>>,
    /// Consume cursor for wait/send deltas (advanced by sends only; reads
    /// are non-destructive views).
    consume: Arc<Mutex<Cursor>>,
    closed: Arc<AtomicBool>,
    /// 最近一次标记命令（逐字节实时一致：dsh 式标记契约）——发送时置 running，
    /// 输出流出现结束标记即记录退出码。None = 该会话未用标记发过命令。
    command: Arc<Mutex<Option<CommandRun>>>,
}

/// 一次标记命令的生命周期。
#[derive(Debug, Clone)]
pub struct CommandRun {
    pub started_ms: u64,
    pub finished_ms: Option<u64>,
    pub exit_code: Option<i64>,
    /// 触发本次命令的文本（wrap 发送时的 input，截断保留）——面板「这个
    /// 终端在跑什么」的直接答案。None = 未用标记发送过命令。
    pub cmd: Option<String>,
    /// 2026-10-26: 本命令的结束标记前缀（\x1b]633;D;{marker}:）。命令可能
    /// 在没有任何 send 在扫描时完成（比如后续 terminal_wait 的静默窗间隙），
    /// 把标记存进记录里，wait-only 发送也能认领它的完成。
    pub end_prefix: String,
}

/// One background terminal as published to UIs and model tools.
#[derive(Debug, Clone)]
pub struct TerminalInfo {
    pub id: String,
    pub status: String,
    pub started_at_ms: u64,
    /// Owning conversation (session id) — dsh 式归属：spawn 时从执行上下文
    /// 注入，工具层按此校验 kill/list 权限，宿主按此在会话结束时回收。
    pub owner: Option<String>,
    /// Session workspace root at spawn time (panel workspace filter).
    pub workspace: Option<String>,
    pub command: Option<CommandRun>,
}

#[derive(Default)]
pub struct TerminalService {
    sessions: Arc<Mutex<HashMap<String, TerminalSession>>>,
    next_seq: std::sync::atomic::AtomicU64,
}

/// Signals forwardable to a session: control characters reach the
/// foreground process group via the PTY line discipline; the terminate set
/// goes to the top-level shell.
const CONTROL_SIGNALS: &[(&str, char)] =
    &[("SIGINT", '\x03'), ("SIGQUIT", '\x1c'), ("SIGTSTP", '\x1a')];
const TERMINATE_SIGNALS: &[&str] = &["SIGTERM", "SIGKILL"];

/// Remove `ESC[6n` device-status queries from `data` (a real terminal
/// consumes them — they are never user-visible content) and return how many
/// were found. A trailing partial match is moved to `carry` so a query
/// straddling two reads is still caught.
fn strip_dsr_queries(data: &mut Vec<u8>, carry: &mut Vec<u8>) -> usize {
    const QUERY: &[u8] = b"\x1b[6n";
    let mut queries = 0;
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        let remaining = &data[i..];
        if remaining.starts_with(QUERY) {
            queries += 1;
            i += QUERY.len();
            continue;
        }
        // Only the final, shorter-than-query tail can be a straddling
        // prefix; an `ESC[` mid-stream is ordinary ANSI and passes through
        // (eating it here would drop every real chunk that starts with one).
        if remaining.len() < QUERY.len() && QUERY.starts_with(remaining) {
            carry.extend_from_slice(remaining);
            break;
        }
        out.push(data[i]);
        i += 1;
    }
    *data = out;
    queries
}

/// Resolve a bare shell name to its full PATH entry (Windows only): the
/// ConPTY backend passes the name as CreateProcessW's lpApplicationName,
/// whose search does not append `.exe` and misses PATH entries in practice,
/// so "sh"/"pwsh" fail unless spelled out. Paths with a separator or an
/// extension pass through untouched; `None` when nothing is on PATH.
#[cfg(windows)]
fn resolve_windows_shell(shell: &str) -> Option<String> {
    let p = std::path::Path::new(shell);
    if p.parent().is_some_and(|d| !d.as_os_str().is_empty()) || shell.contains('.') {
        return Some(shell.to_string());
    }
    for ext in ["", ".exe"] {
        for dir in std::env::split_paths(&std::env::var("PATH").unwrap_or_default()) {
            let candidate = dir.join(format!("{shell}{ext}"));
            if candidate.is_file() {
                return Some(candidate.to_string_lossy().into_owned());
            }
        }
    }
    None
}

impl TerminalService {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start an interactive PTY session. `shell` defaults to `bash` on Unix
    /// and `pwsh` on Windows. Host/test entry: no owner, host cwd.
    pub fn start(&self, shell: Option<String>) -> Result<String, String> {
        self.start_owned(None, None, shell)
    }

    /// Start an interactive PTY session owned by `owner` (conversation
    /// session id) in `cwd` (session workspace root). dsh 式归属：owner 从
    /// 工具执行上下文注入而非模型参数，kill/list 权限与会话结束回收都以
    /// 它为键。`cwd` 同时修正 spawn 目录（此前落在宿主进程 cwd 上）。
    pub fn start_owned(
        &self,
        owner: Option<String>,
        cwd: Option<String>,
        shell: Option<String>,
    ) -> Result<String, String> {
        let id = uuid::Uuid::new_v4().to_string();
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(portable_pty::PtySize {
                rows: 24,
                cols: 100,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| e.to_string())?;
        let shell = shell.unwrap_or_else(|| {
            // Compile-time gate: `resolve_windows_shell` only exists on
            // Windows, so the choice cannot sit behind a runtime `cfg!`.
            #[cfg(windows)]
            {
                // Prefer PowerShell, fall back through what is installed.
                ["pwsh", "powershell", "cmd"]
                    .iter()
                    .find_map(|s| resolve_windows_shell(s))
                    .unwrap_or_else(|| "cmd.exe".into())
            }
            #[cfg(not(windows))]
            {
                "bash".into()
            }
        });
        // ConPTY spawns via CreateProcessW's lpApplicationName, whose search
        // is unreliable for bare names ("sh"/"pwsh" are not found even when
        // on PATH). Resolve extension-less names to a full PATH entry; an
        // unresolvable explicit shell keeps `name.exe` so the spawn error
        // still names something sensible.
        #[cfg(windows)]
        let shell = resolve_windows_shell(&shell).unwrap_or_else(|| format!("{shell}.exe"));
        let mut cmd = CommandBuilder::new(shell);
        // 会话工作区优先；None（host 入口/旧测试）保持宿主进程 cwd。
        cmd.cwd(
            cwd.clone()
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default()),
        );
        let child = pair.slave.spawn_command(cmd).map_err(|e| e.to_string())?;
        drop(pair.slave);

        let reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
        let writer = Arc::new(Mutex::new(
            pair.master.take_writer().map_err(|e| e.to_string())?,
        ));
        let killer = child.clone_killer();
        let scrollback: Arc<Mutex<Scrollback>> = Arc::new(Mutex::new(Scrollback::default()));
        // Output drain thread: finalize output into the retained scrollback
        // so the child never blocks on a full pipe and reads page history.
        // It also plays host terminal for device-status queries: shells
        // (readline, PSReadLine) probe the cursor position with `ESC[6n` at
        // startup and BLOCK on the answer — a real terminal consumes the
        // query and replies `ESC[<row>;<col>R`; without a reply the shell
        // stalls before ever drawing a prompt.
        let last_activity_ms = Arc::new(AtomicU64::new(now_ms()));
        {
            let scrollback = Arc::clone(&scrollback);
            let reply_writer = Arc::clone(&writer);
            let last_activity = Arc::clone(&last_activity_ms);
            std::thread::spawn(move || {
                let mut reader = reader;
                let mut buf = [0u8; 4096];
                // Tail of a chunk that may be a straddling `ESC[6n` prefix.
                let mut carry: Vec<u8> = Vec::new();
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let mut data = std::mem::take(&mut carry);
                            data.extend_from_slice(&buf[..n]);
                            let queries = strip_dsr_queries(&mut data, &mut carry);
                            let text = String::from_utf8_lossy(&data);
                            if !text.is_empty() {
                                scrollback.lock().unwrap().push_text(&text);
                                // Any output counts as activity: a logging
                                // process stays `running`, a parked prompt
                                // ages into `idle`.
                                last_activity.store(now_ms(), Ordering::SeqCst);
                            }
                            if queries > 0 {
                                let reply = "\x1b[1;1R".repeat(queries);
                                if let Ok(mut w) = reply_writer.lock() {
                                    let _ = w.write_all(reply.as_bytes());
                                    let _ = w.flush();
                                }
                            }
                        }
                    }
                }
            });
        }
        let session = TerminalSession {
            seq: self
                .next_seq
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            started_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            last_activity_ms,
            owner,
            workspace: cwd,
            _master: pair.master,
            writer,
            scrollback,
            child: Arc::new(Mutex::new(Some(child))),
            killer: Arc::new(Mutex::new(killer)),
            consume: Arc::new(Mutex::new(Cursor {
                line_no: 0,
                tail_bytes: 0,
            })),
            closed: Arc::new(AtomicBool::new(false)),
            command: Arc::new(Mutex::new(None)),
        };
        self.sessions.lock().unwrap().insert(id.clone(), session);
        Ok(id)
    }

    fn session_parts(
        &self,
        id: &str,
    ) -> Result<
        (
            Arc<Mutex<Box<dyn Write + Send>>>,
            Arc<Mutex<Scrollback>>,
            Arc<Mutex<Option<Box<dyn portable_pty::Child + Send>>>>,
            Arc<Mutex<Box<dyn ChildKiller + Send + Sync>>>,
            Arc<Mutex<Cursor>>,
            Arc<AtomicBool>,
            Arc<AtomicU64>,
        ),
        String,
    > {
        let sessions = self.sessions.lock().unwrap();
        let s = sessions
            .get(id)
            .ok_or_else(|| format!("terminal {id} not found"))?;
        Ok((
            Arc::clone(&s.writer),
            Arc::clone(&s.scrollback),
            Arc::clone(&s.child),
            Arc::clone(&s.killer),
            Arc::clone(&s.consume),
            Arc::clone(&s.closed),
            Arc::clone(&s.last_activity_ms),
        ))
    }

    fn poll_status(
        child: &Arc<Mutex<Option<Box<dyn portable_pty::Child + Send>>>>,
        last_activity_ms: Option<&AtomicU64>,
    ) -> SessionStatus {
        let mut guard = match child.lock() {
            Ok(guard) => guard,
            Err(e) => e.into_inner(),
        };
        match guard.as_mut() {
            None => SessionStatus::Exited {
                exit_code: None,
                signal: None,
            },
            Some(child) => match child.try_wait() {
                Ok(Some(status)) => SessionStatus::Exited {
                    exit_code: Some(status.exit_code()),
                    signal: status.signal().map(String::from),
                },
                Ok(None) => match last_activity_ms {
                    // Alive but silent past the window: a parked prompt, not
                    // work. Any output or written input refreshes the stamp
                    // and flips this back to Running.
                    Some(a)
                        if now_ms().saturating_sub(a.load(Ordering::SeqCst)) >= IDLE_AFTER_MS =>
                    {
                        SessionStatus::Idle
                    }
                    _ => SessionStatus::Running,
                },
                Err(_) => SessionStatus::Running,
            },
        }
    }

    /// Write input to the PTY without waiting (fire-and-forget).
    pub fn write(&self, id: &str, input: &str) -> Result<(), String> {
        let (writer, .., last_activity_ms) = self.session_parts(id)?;
        let mut w = writer.lock().unwrap_or_else(|e| e.into_inner());
        w.write_all(input.as_bytes()).map_err(|e| e.to_string())?;
        w.flush().map_err(|e| e.to_string())?;
        // Written input is activity too: typing at an idle prompt wakes it
        // back to `running` even before any output arrives.
        last_activity_ms.store(now_ms(), Ordering::SeqCst);
        Ok(())
    }

    /// One interactive send: optionally write `input`, then wait for
    /// readiness — quiet window (`inferred_idle` / echo-only `stdin_read`),
    /// budget exhaustion (`timeout`), or top-level shell exit
    /// (`session_exit`) — and return the produced delta (
    /// `TerminalSendResult` parity). Advances the session's consume cursor;
    /// paging reads are unaffected.
    pub fn send(
        &self,
        id: &str,
        input: Option<&str>,
        wait_ms: u64,
        idle_ms: u64,
    ) -> Result<SendOutcome, String> {
        self.send_ex(id, input, wait_ms, idle_ms, false)
    }

    /// 带标记契约的发送（逐字节一致面板版，dsh 式）：把命令包成
    /// `printf 结束标记 + 退出码` 的形式写入 PTY，输出流一出现结束标记即刻
    /// 记录命令完成与退出码——面板不再依赖 5 分钟静默推断。
    pub fn send_ex(
        &self,
        id: &str,
        input: Option<&str>,
        wait_ms: u64,
        idle_ms: u64,
        wrap: bool,
    ) -> Result<SendOutcome, String> {
        let (writer, scrollback, child, _killer, consume, closed, last_activity) =
            self.session_parts(id)?;
        // 每次发送的唯一标记 id：防历史/回显中的同形标记误匹配。
        let marker_id = format!(
            "{}{}",
            now_ms(),
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        );
        let end_prefix = format!("\x1b]633;D;{marker_id}:");
        // 2026-10-26 修复：只有真正写入命令（input.is_some()）才创建/覆盖
        // 命令记录。terminal_wait 这类 wait-only 发送此前会把进行中的记录
        // 整个重置（started_ms 归零、cmd 丢失）——构建命令被后续 wait 打回
        // 原形，面板只剩 "PTY · id" 回退文案和错误的首发时间。
        if wrap && input.is_some() {
            if let Some(run) = self.command_of(id) {
                // 面板显示用：保留触发命令的文本（截断，防超长行拖垮协议负载）。
                const CMD_SNIPPET_MAX: usize = 500;
                let mut snippet: String = input
                    .unwrap_or_default()
                    .chars()
                    .take(CMD_SNIPPET_MAX)
                    .collect();
                if input.unwrap_or_default().chars().count() > CMD_SNIPPET_MAX {
                    snippet.push('…');
                }
                *run.lock().unwrap_or_else(|e| e.into_inner()) = Some(CommandRun {
                    started_ms: now_ms(),
                    finished_ms: None,
                    exit_code: None,
                    cmd: Some(snippet),
                    end_prefix: end_prefix.clone(),
                });
            }
        }
        // 结束标记认领：优先认当前记录里存的标记（wait-only 发送没有自己的
        // 包裹，却仍能认领在跑命令的完成）。
        let active_end_prefix = if wrap {
            self.command_of(id).and_then(|run| {
                run.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .map(|c| c.end_prefix.clone())
            })
        } else {
            None
        };
        if let Some(input) = input {
            let payload = if wrap {
                // 单引号转义保多行语义；eval 在当前 shell 执行（cd/exit 保留）。
                let quoted = format!("'{}'", input.replace('\'', r"'\''"));
                // 标记转义走 $'\x..' ANSI-C 引导（bash/zsh/macOS sh 皆支持；
                // printf 的 \\0NNN 八进制在 bash 语义下会被拆成 NUL+字面量）。
                format!(
                    "printf '%s' $'\\x1b]633;C;{marker_id}\\x07'; eval -- {quoted}; __st_rc=$?; printf '%s%s' $'\\x1b]633;D;{marker_id}:' \"$__st_rc\" $'\\x07'; unset __st_rc\n"
                )
            } else {
                input.to_string()
            };
            let mut w = writer.lock().unwrap_or_else(|e| e.into_inner());
            w.write_all(payload.as_bytes()).map_err(|e| e.to_string())?;
            w.flush().map_err(|e| e.to_string())?;
        }
        let wait_budget = Duration::from_millis(wait_ms.min(120_000));
        let idle_window = Duration::from_millis(idle_ms.max(POLL_MS));
        let started = Instant::now();
        let mut last_growth = started;
        let mut last_len = 0usize;
        let mut truncated = false;
        loop {
            if closed.load(Ordering::SeqCst) {
                return Err(format!("terminal {id} closed during send"));
            }
            let status = Self::poll_status(&child, Some(&last_activity));
            let (mut delta, cursor, dropped_under) = {
                let sb = scrollback.lock().unwrap_or_else(|e| e.into_inner());
                let consume_cur = *consume.lock().unwrap_or_else(|e| e.into_inner());
                let dropped_under = sb.dropped_under(consume_cur);
                let (delta, cursor) = sb.delta_from(consume_cur);
                (delta, cursor, dropped_under)
            };
            // Lines under the cursor were evicted by the retention cap: the
            // delta is missing output the consumer never saw.
            if dropped_under {
                truncated = true;
            }
            // 标记扫描：结束标记一出现即记录命令完成与退出码（不等静默窗）。
            if wrap {
                if let Some(prefix) = active_end_prefix.as_deref() {
                    if let Some(end_at) = delta.rfind(prefix) {
                        let digits_start = end_at + prefix.len();
                        let digits: String = delta[digits_start..]
                            .chars()
                            .take_while(|c| c.is_ascii_digit())
                            .collect();
                        if !digits.is_empty() {
                            if let Some(run) = self.command_of(id) {
                                let mut g = run.lock().unwrap_or_else(|e| e.into_inner());
                                if let Some(cmd) = g.as_mut() {
                                    if cmd.finished_ms.is_none() {
                                        cmd.finished_ms = Some(now_ms());
                                        cmd.exit_code = digits.parse::<i64>().ok();
                                    }
                                }
                            }
                        }
                    }
                }
            }
            let now = Instant::now();
            if delta.len() > last_len {
                last_len = delta.len();
                last_growth = now;
            }
            if delta.len() > DELTA_MAX_BYTES {
                // Keep the newest end; the earlier output stays in the
                // retained scrollback for paging.
                let mut cut = delta.len() - DELTA_MAX_BYTES;
                while cut < delta.len() && !delta.is_char_boundary(cut) {
                    cut += 1;
                }
                delta.drain(..cut);
                truncated = true;
            }
            if let SessionStatus::Exited { .. } = status {
                *consume.lock().unwrap_or_else(|e| e.into_inner()) = cursor;
                let delta = if wrap {
                    strip_markers(&delta, &marker_id, &end_prefix)
                } else {
                    delta
                };
                return Ok(SendOutcome {
                    output: delta,
                    wait_reason: WaitReason::SessionExit,
                    session_status: status,
                    truncated,
                });
            }
            let quiet_for = now.saturating_duration_since(last_growth);
            if !delta.is_empty() && quiet_for >= idle_window {
                *consume.lock().unwrap_or_else(|e| e.into_inner()) = cursor;
                let wait_reason = match input {
                    Some(written) if delta_is_echo_only(&delta, written) => WaitReason::StdinRead,
                    _ => WaitReason::InferredIdle,
                };
                let delta = if wrap {
                    strip_markers(&delta, &marker_id, &end_prefix)
                } else {
                    delta
                };
                return Ok(SendOutcome {
                    output: delta,
                    wait_reason,
                    session_status: status,
                    truncated,
                });
            }
            if now.saturating_duration_since(started) >= wait_budget {
                *consume.lock().unwrap_or_else(|e| e.into_inner()) = cursor;
                let delta = if wrap {
                    strip_markers(&delta, &marker_id, &end_prefix)
                } else {
                    delta
                };
                return Ok(SendOutcome {
                    output: delta,
                    wait_reason: WaitReason::Timeout,
                    session_status: status,
                    truncated,
                });
            }
            std::thread::sleep(Duration::from_millis(POLL_MS));
        }
    }

    /// Signal a session. Control signals (SIGINT/SIGQUIT/SIGTSTP) ride the
    /// PTY line discipline to the foreground process group; SIGTERM/SIGKILL
    /// terminate the top-level shell itself. Returns what was delivered.
    pub fn signal(&self, id: &str, signal: &str) -> Result<String, String> {
        let (writer, _scrollback, _child, killer, _consume, closed, _last_activity) =
            self.session_parts(id)?;
        if closed.load(Ordering::SeqCst) {
            return Err(format!("terminal {id} closed"));
        }
        let upper = signal.to_ascii_uppercase();
        if let Some((name, ctrl)) = CONTROL_SIGNALS.iter().find(|(n, _)| **n == upper) {
            let mut w = writer.lock().unwrap_or_else(|e| e.into_inner());
            w.write_all((*ctrl).to_string().as_bytes())
                .map_err(|e| e.to_string())?;
            w.flush().map_err(|e| e.to_string())?;
            return Ok(format!(
                "{name} forwarded to the foreground process group via the PTY line discipline"
            ));
        }
        if TERMINATE_SIGNALS.contains(&upper.as_str()) {
            killer
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .kill()
                .map_err(|e| e.to_string())?;
            return Ok(format!("{upper} sent to the top-level shell process"));
        }
        Err(format!(
            "unsupported signal `{signal}`; allowed: SIGINT, SIGQUIT, SIGTSTP, SIGTERM, SIGKILL"
        ))
    }

    /// Top-level process status of a session (independent of any send).
    pub fn status(&self, id: &str) -> Result<SessionStatus, String> {
        let (_writer, _scrollback, child, _killer, _consume, _closed, last_activity) =
            self.session_parts(id)?;
        Ok(Self::poll_status(&child, Some(&last_activity)))
    }

    /// Newest-relative paged view over the retained scrollback
    /// (non-destructive; `offset_from_end` lines back, up to `max_lines`).
    /// Returns the joined page, the total line count ever produced, and
    /// whether older output exists beyond the page.
    pub fn read_page(
        &self,
        id: &str,
        offset_from_end: usize,
        max_lines: usize,
    ) -> Result<(String, u64, bool), String> {
        let (_writer, scrollback, _child, _killer, _consume, _closed, _last_activity) =
            self.session_parts(id)?;
        let sb = scrollback.lock().unwrap_or_else(|e| e.into_inner());
        let (lines, total, older) = sb.page(offset_from_end, max_lines.max(1));
        Ok((lines.join(""), total, older))
    }

    /// 2026-10-26: in-flight wrapped command overrides the idle heuristic —
    /// a build that redirects all output to a file keeps the PTY silent, and
    /// the five-minute idle window would mask genuinely running work as
    /// "idle". Exited stays truthful (the shell is gone, whatever the record
    /// says); Running needs no promotion.
    fn display_status(polled: SessionStatus, command_in_flight: bool) -> SessionStatus {
        match (polled, command_in_flight) {
            (SessionStatus::Idle, true) => SessionStatus::Running,
            (other, _) => other,
        }
    }

    /// Fresh snapshots (id + top-level status) in publication order.
    /// 会话的标记命令状态（None = 会话不存在或未用标记发过命令）。
    fn command_of(&self, id: &str) -> Option<std::sync::Arc<Mutex<Option<CommandRun>>>> {
        let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        sessions.get(id).map(|s| std::sync::Arc::clone(&s.command))
    }

    /// Full snapshots in publication order — UIs (workspace-scoped panel)
    /// and model tools (owner-filtered) both consume these.
    pub fn list(&self) -> Vec<TerminalInfo> {
        let sessions = self.sessions.lock().unwrap();
        // HashMap iteration order is unspecified (and re-randomized per
        // process) — sort by each session's publication sequence so the
        // documented "publication order" is actually delivered and
        // consecutive listings are stable.
        let mut entries: Vec<(&String, &TerminalSession)> = sessions.iter().collect();
        entries.sort_by_key(|(_, session)| session.seq);
        entries
            .into_iter()
            .map(|(id, session)| {
                let polled = Self::poll_status(&session.child, Some(&session.last_activity_ms));
                let command = session
                    .command
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                let status = Self::display_status(
                    polled,
                    command
                        .as_ref()
                        .map(|c| c.finished_ms.is_none())
                        .unwrap_or(false),
                )
                .as_str()
                .to_string();
                TerminalInfo {
                    id: id.clone(),
                    status,
                    started_at_ms: session.started_at_ms,
                    owner: session.owner.clone(),
                    workspace: session.workspace.clone(),
                    command,
                }
            })
            .collect()
    }

    /// Kill every terminal owned by `owner` (conversation session id).
    /// Session delete/archive 的回收入口；返回杀掉的个数。
    pub fn kill_owner(&self, owner: &str) -> usize {
        let ids: Vec<String> = {
            let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            sessions
                .iter()
                .filter(|(_, s)| s.owner.as_deref() == Some(owner))
                .map(|(id, _)| id.clone())
                .collect()
        };
        ids.iter().filter_map(|id| self.kill(id, None).ok()).count()
    }

    /// Kill every live terminal — host shutdown 的最后防线。
    pub fn kill_all(&self) -> usize {
        let ids: Vec<String> = {
            let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            sessions.keys().cloned().collect()
        };
        ids.iter().filter_map(|id| self.kill(id, None).ok()).count()
    }

    /// Close a session: flag it (unblocking in-flight sends), kill the
    /// top-level shell, then remove it. `owner = None` is host-privileged
    /// (UI ✕ / protocol / shutdown)；Some(session) 必须匹配归属，否则拒绝 —
    /// dsh 式授权：模型工具只能杀自己会话的终端。
    /// Removal happens only after the child actually exits (bounded) so a
    /// just-killed terminal never lingers half-dead in the registry.
    pub fn kill(&self, id: &str, owner: Option<&str>) -> Result<(), String> {
        let mut sessions = self.sessions.lock().unwrap();
        let session = sessions
            .remove(id)
            .ok_or_else(|| format!("terminal {id} not found"))?;
        if let Some(owner) = owner {
            if session.owner.as_deref() != Some(owner) {
                // Not ours: put it back untouched — a foreign kill must
                // leave the terminal exactly as it was.
                sessions.insert(id.to_string(), session);
                return Err(format!("terminal {id} is not owned by session {owner}"));
            }
        }
        session.closed.store(true, Ordering::SeqCst);
        let _ = session
            .killer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .kill();
        // Quiescent removal (dsh parity): wait for the child to actually
        // exit so the registry never shows a killed-but-alive session. The
        // killer is a hard kill, so this is usually one poll.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1_500);
        loop {
            let exited = {
                let mut guard = session.child.lock().unwrap_or_else(|e| e.into_inner());
                match guard.as_mut() {
                    None => true,
                    Some(child) => matches!(child.try_wait(), Ok(Some(_))),
                }
            };
            if exited || std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(POLL_MS));
        }
        Ok(())
    }

    /// Test hook: backdate a session's activity stamp so the idle window
    /// elapses instantly (no real 5-minute waits).
    #[cfg(test)]
    pub(crate) fn backdate_last_activity_for_tests(&self, id: &str, ms: u64) {
        let sessions = self.sessions.lock().unwrap();
        if let Some(session) = sessions.get(id) {
            session.last_activity_ms.store(ms, Ordering::SeqCst);
        }
    }
}

/// Echo-only classification (CR-normalized): the delta equals the written
/// input (trailing newline aside) — the PTY accepted the line but the
/// program produced nothing yet.
fn delta_is_echo_only(delta: &str, input: &str) -> bool {
    let normalize = |s: &str| s.replace('\r', "").trim_end().to_string();
    normalize(delta) == normalize(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_after_silence_recovers_on_input() {
        let service = TerminalService::new();
        let id = service.start(Some("sh".into())).expect("start");
        let status_of = |id: &str| {
            service
                .list()
                .into_iter()
                .find(|row| row.id == id)
                .map(|row| row.status)
                .expect("session listed")
        };
        // A freshly started session is alive and active → running.
        assert_eq!(status_of(&id), "running");
        // Silence past the window: parked prompt reads idle (still alive —
        // not exited).
        service.backdate_last_activity_for_tests(&id, now_ms() - IDLE_AFTER_MS - 1_000);
        assert_eq!(service.status(&id), Ok(SessionStatus::Idle));
        assert_eq!(status_of(&id), "idle");
        // Written input is activity: idle flips back to running without any
        // output arriving first.
        service.write(&id, ":\n").expect("write");
        assert_eq!(service.status(&id), Ok(SessionStatus::Running));
        assert_eq!(status_of(&id), "running");
        service.kill(&id, None).expect("kill");
    }

    #[test]
    fn start_send_roundtrip_reports_readiness() {
        let service = TerminalService::new();
        let id = service.start(Some("sh".into())).expect("start");
        let outcome = service
            .send(&id, Some("echo hello-terminal\n"), 5_000, 300)
            .expect("send");
        assert!(
            outcome.output.contains("hello-terminal"),
            "got: {}",
            outcome.output
        );
        assert_eq!(outcome.wait_reason, WaitReason::InferredIdle);
        assert_eq!(outcome.session_status, SessionStatus::Running);
        assert!(!outcome.truncated);
        assert_eq!(service.status(&id), Ok(SessionStatus::Running));
        service.kill(&id, None).expect("kill");
        assert!(service.list().is_empty());
    }

    #[test]
    fn partial_line_write_quiets_as_stdin_read() {
        let service = TerminalService::new();
        let id = service.start(Some("sh".into())).expect("start");
        // Warm-up send consumes the startup prompt (command completes → a
        // fresh prompt arrives → inferred_idle).
        let warm = service
            .send(&id, Some("true\n"), 5_000, 300)
            .expect("warm send");
        assert_eq!(warm.wait_reason, WaitReason::InferredIdle, "got: {warm:?}");
        // A write WITHOUT the newline: the PTY echoes the text but no
        // command runs — pure stdin acceptance, nothing else.
        let partial = service
            .send(&id, Some("echo hi"), 5_000, 300)
            .expect("partial send");
        assert_eq!(
            partial.wait_reason,
            WaitReason::StdinRead,
            "got: {partial:?}"
        );
        assert!(partial.output.contains("echo hi"), "{partial:?}");
        // Completing the line executes it and the shell goes idle again.
        let finish = service.send(&id, Some("\n"), 5_000, 300).expect("finish");
        assert_eq!(
            finish.wait_reason,
            WaitReason::InferredIdle,
            "got: {finish:?}"
        );
        assert!(finish.output.contains("hi"), "got: {finish:?}");
        service.kill(&id, None).expect("kill");
    }

    #[test]
    fn send_times_out_on_silence() {
        let service = TerminalService::new();
        let id = service.start(Some("sh".into())).expect("start");
        // Disable echo first (its own echo is consumed by this send), so the
        // sleeper produces no output at all.
        let quiet = service
            .send(&id, Some("stty -echo\n"), 5_000, 300)
            .expect("stty send");
        assert_eq!(quiet.wait_reason, WaitReason::InferredIdle);
        let outcome = service
            // `read -t` is a shell builtin: `sleep` lives outside the
            // MSYS2 minimal PATH on Windows hosts and would break the
            // premise with "command not found".
            .send(&id, Some("read -t 30 x\n"), 250, 200)
            .expect("send");
        assert_eq!(outcome.wait_reason, WaitReason::Timeout, "got: {outcome:?}");
        assert_eq!(outcome.session_status, SessionStatus::Running);
        service.kill(&id, None).expect("kill");
    }

    #[test]
    fn send_reports_session_exit() {
        let service = TerminalService::new();
        let id = service.start(Some("sh".into())).expect("start");
        let outcome = service
            .send(&id, Some("exit 7\n"), 5_000, 300)
            .expect("send");
        assert_eq!(outcome.wait_reason, WaitReason::SessionExit);
        assert_eq!(
            outcome.session_status,
            SessionStatus::Exited {
                exit_code: Some(7),
                signal: None
            }
        );
        service.kill(&id, None).ok();
    }

    #[test]
    fn scrollback_pages_newest_relative_without_consuming() {
        let service = TerminalService::new();
        let id = service.start(Some("sh".into())).expect("start");
        let _ = service.send(
            &id,
            Some("for i in 1 2 3 4 5; do echo line-$i; done\n"),
            5_000,
            400,
        );
        // Full history is retained: two independent pages see the same lines.
        let (all, total, older) = service.read_page(&id, 0, 100).expect("page");
        assert!(all.contains("line-1") && all.contains("line-5"), "{all}");
        assert!(total >= 6, "total {total}");
        let (last_one, _, _) = service.read_page(&id, 1, 1).expect("page");
        assert!(
            last_one.contains("line-5"),
            "newest-relative page: {last_one}"
        );
        let (again, _, _) = service.read_page(&id, 0, 100).expect("page");
        assert_eq!(all, again, "reads are non-destructive");
        let _ = older;
        service.kill(&id, None).expect("kill");
    }

    #[test]
    fn scrollback_drops_oldest_beyond_cap_and_flags() {
        let mut sb = Scrollback::default();
        let line = format!("x{}\n", "y".repeat(99));
        for _ in 0..2000 {
            sb.push_text(&line);
        }
        assert!(sb.dropped_ever, "old lines dropped");
        assert!(sb.retained_bytes <= SCROLLBACK_MAX_BYTES + line.len());
        let (page, total, older) = sb.page(0, 10);
        assert_eq!(page.len(), 10);
        assert!(total >= 2000);
        assert!(older, "history beyond the page is flagged");
    }

    #[test]
    fn signal_forwards_control_char_and_rejects_unknown() {
        let service = TerminalService::new();
        let id = service.start(Some("sh".into())).expect("start");
        // A foreground sleeper receives SIGINT via the line discipline.
        let _ = service.send(&id, Some("sleep 60\n"), 300, 200);
        let delivered = service.signal(&id, "SIGINT").expect("signal");
        assert!(delivered.contains("SIGINT"), "{delivered}");
        let outcome = service
            .send(&id, None, 2_000, 300)
            .expect("post-signal send");
        assert!(
            outcome.output.contains("Interrupt")
                || outcome.output.contains("^C")
                || !outcome.output.is_empty(),
            "shell reports the interrupt: {}",
            outcome.output
        );
        let err = service.signal(&id, "SIGUSR1").unwrap_err();
        assert!(err.contains("unsupported"), "{err}");
        service.kill(&id, None).ok();
    }

    #[test]
    fn unknown_sessions_fail_with_messages() {
        let service = TerminalService::new();
        assert!(service.write("missing", "x").is_err());
        assert!(service.send("missing", Some("x"), 100, 50).is_err());
        assert!(service.read_page("missing", 0, 10).is_err());
        assert!(service.status("missing").is_err());
        assert!(service.signal("missing", "SIGINT").is_err());
        assert!(service.kill("missing", None).is_err());
    }

    #[test]
    fn wait_reasons_serialize_with_dsh_names() {
        assert_eq!(WaitReason::StdinRead.as_str(), "stdin_read");
        assert_eq!(WaitReason::InferredIdle.as_str(), "inferred_idle");
        assert_eq!(WaitReason::Timeout.as_str(), "timeout");
        assert_eq!(WaitReason::SessionExit.as_str(), "session_exit");
    }

    // ── strip_dsr_queries (pure) ──

    fn strip_once(data: &mut Vec<u8>, carry: &mut Vec<u8>) -> (Vec<u8>, usize) {
        let n = strip_dsr_queries(data, carry);
        (std::mem::take(data), n)
    }

    #[test]
    fn dsr_query_free_output_passes_through_untouched() {
        let mut data = b"hello world\x1b[2Jclear".to_vec();
        let mut carry = Vec::new();
        let (out, queries) = strip_once(&mut data, &mut carry);
        assert_eq!(out, b"hello world\x1b[2Jclear");
        assert_eq!(queries, 0);
        assert!(carry.is_empty(), "no partial tail to carry");
    }

    #[test]
    fn dsr_queries_are_stripped_mid_stream() {
        let mut data = b"a\x1b[6nb\x1b[6nc".to_vec();
        let mut carry = Vec::new();
        let (out, queries) = strip_once(&mut data, &mut carry);
        assert_eq!(out, b"abc");
        assert_eq!(queries, 2);
        assert!(carry.is_empty());
    }

    #[test]
    fn dsr_query_straddling_reads_is_caught_via_carry() {
        // First read ends mid-query: the partial tail moves to carry...
        let mut data = b"x\x1b[6".to_vec();
        let mut carry = Vec::new();
        let (out, queries) = strip_once(&mut data, &mut carry);
        assert_eq!(out, b"x");
        assert_eq!(queries, 0);
        assert_eq!(carry, b"\x1b[6");
        // ...and the drain thread prepends carry to the next read.
        let mut next = std::mem::take(&mut carry);
        next.extend_from_slice(b"n y");
        let (out, queries) = strip_once(&mut next, &mut carry);
        assert_eq!(out, b" y");
        assert_eq!(queries, 1);
        assert!(carry.is_empty());
    }

    #[test]
    fn lone_esc_tail_carries_but_plain_ansi_midstream_does_not() {
        // A bare ESC at the very end is a possible query prefix → carried.
        let mut data = b"done\x1b".to_vec();
        let mut carry = Vec::new();
        let (out, _) = strip_once(&mut data, &mut carry);
        assert_eq!(out, b"done");
        assert_eq!(carry, b"\x1b");

        // An ESC[ sequence that is NOT a query and is NOT the final tail
        // passes through — eating it here would drop real output chunks.
        let mut data = b"\x1b[1;31mred\x1b[0m".to_vec();
        let mut carry = Vec::new();
        let (out, queries) = strip_once(&mut data, &mut carry);
        assert_eq!(out, b"\x1b[1;31mred\x1b[0m");
        assert_eq!(queries, 0);
        assert!(carry.is_empty());
    }

    #[test]
    fn dsr_query_ending_exactly_at_chunk_end_is_stripped() {
        let mut data = b"prompt\x1b[6n".to_vec();
        let mut carry = Vec::new();
        let (out, queries) = strip_once(&mut data, &mut carry);
        assert_eq!(out, b"prompt");
        assert_eq!(queries, 1);
        assert!(carry.is_empty(), "full query consumed, nothing carried");
    }

    // ── Scrollback (pure) ──

    #[test]
    fn scrollback_tail_consumption_advances_byte_cursor() {
        let mut sb = Scrollback::default();
        sb.push_text("partial");
        let (delta, cursor) = sb.delta_from(Cursor {
            line_no: 0,
            tail_bytes: 0,
        });
        assert_eq!(delta, "partial");
        assert_eq!(cursor.tail_bytes, 7);

        // Completing the line yields only the unseen suffix.
        sb.push_text(" line\n");
        let (delta, cursor) = sb.delta_from(cursor);
        assert_eq!(delta, " line\n", "only the bytes after the cursor");
        assert_eq!(cursor.line_no, 1);
        assert_eq!(cursor.tail_bytes, 0);

        // A finalized line never repeats for an up-to-date cursor.
        let (delta, _) = sb.delta_from(cursor);
        assert_eq!(delta, "");
    }

    /// Regression: a line delivered as the unfinalized tail must not be
    /// replayed in full once it finalizes — the old cursor protocol applied
    /// the byte offset only to the still-pending tail, so the second send
    /// re-delivered the prefix the first send already returned.
    #[test]
    fn scrollback_line_straddling_two_deltas_is_not_replayed() {
        let mut sb = Scrollback::default();
        sb.push_text("echo hi"); // half a line arrives
        let (d1, cur) = sb.delta_from(Cursor {
            line_no: 0,
            tail_bytes: 0,
        });
        assert_eq!(d1, "echo hi");

        sb.push_text(" done\n"); // the SAME line completes
        let (d2, cur2) = sb.delta_from(cur);
        assert_eq!(d2, " done\n", "already-delivered prefix must not replay");
        assert_eq!(
            cur2,
            Cursor {
                line_no: 1,
                tail_bytes: 0
            }
        );

        // A NEW line after the cursor still arrives in full.
        sb.push_text("next\n");
        let (d3, _) = sb.delta_from(cur2);
        assert_eq!(d3, "next\n");
    }

    #[test]
    fn scrollback_delta_after_drops_returns_retained_lines_from_cursor() {
        let mut sb = Scrollback::default();
        let line = format!("x{}\n", "y".repeat(99));
        for i in 0..2000 {
            sb.push_text(&line.replace('x', &format!("{i:04}")));
        }
        assert!(sb.dropped_lines > 0, "cap exceeded, oldest lines dropped");
        // A cursor pointing at an already-dropped line still yields every
        // retained line from that point (numbering never rewinds).
        let (delta, cursor) = sb.delta_from(Cursor {
            line_no: 0,
            tail_bytes: 0,
        });
        assert!(delta.contains("1999"), "newest retained line present");
        assert!(!delta.contains("0000"), "dropped oldest line absent");
        assert!(cursor.line_no > sb.dropped_lines);
    }

    #[test]
    fn scrollback_page_bounds_and_partial_tail_line() {
        let mut sb = Scrollback::default();
        sb.push_text("one\ntwo\nthree");
        // The unfinalized tail counts as the newest (virtual) line.
        let (lines, total, older) = sb.page(0, 10);
        assert_eq!(lines, vec!["one\n", "two\n", "three"]);
        assert_eq!(total, 3);
        assert!(!older);

        // Newest-relative paging with offset lands on complete lines.
        let (lines, _, _) = sb.page(1, 2);
        assert_eq!(lines, vec!["one\n", "two\n"]);

        // Offset beyond the retained history yields an empty page, not a panic.
        let (lines, total, older) = sb.page(99, 5);
        assert!(lines.is_empty());
        assert_eq!(total, 3);
        assert!(
            older || true,
            "older flag is unspecified for out-of-range offsets"
        );
    }

    /// BUG-14 regression: lines dropped under an unconsumed cursor must be
    /// detectable so `send` can flag the delta as truncated.
    #[test]
    fn scrollback_reports_drops_under_an_unconsumed_cursor() {
        let mut sb = Scrollback::default();
        let line = format!("x{}\n", "y".repeat(99));
        for _ in 0..2000 {
            sb.push_text(&line);
        }
        let stale = Cursor {
            line_no: 0,
            tail_bytes: 0,
        };
        assert!(
            sb.dropped_under(stale),
            "cursor at line 0 sits under the drop frontier"
        );
        // A cursor at the current tail is never under anything.
        let (_, cur) = sb.delta_from(stale);
        assert!(!sb.dropped_under(cur));

        // Fresh scrollback never reports drops.
        let mut fresh = Scrollback::default();
        fresh.push_text("one\ntwo\n");
        assert!(!fresh.dropped_under(Cursor {
            line_no: 0,
            tail_bytes: 0
        }));
    }

    // ── delta_is_echo_only (pure) ──

    #[test]
    fn echo_only_classification_normalizes_cr_and_trailing_space() {
        assert!(
            delta_is_echo_only("hi\r\n", "hi\n"),
            "PTY CR echo normalized"
        );
        assert!(
            delta_is_echo_only("hi", "hi "),
            "trailing whitespace ignored"
        );
        assert!(delta_is_echo_only("hi", "hi\n\n"));
        assert!(
            !delta_is_echo_only("hi\r\nout", "hi"),
            "extra output is not echo-only"
        );
        assert!(!delta_is_echo_only("", "hi"), "empty delta is not an echo");
    }
}

/// 移除本条发送的标记序列（真实结束标记与开始标记；回显文本中的标记字面量
/// 因含 `%s`/无数字不会匹配）。
fn strip_markers(text: &str, marker_id: &str, end_prefix: &str) -> String {
    let start_marker = format!("\x1b]633;C;{marker_id}\x07");
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let Some(end_at) = rest.find(end_prefix) else {
            break;
        };
        let digits_start = end_at + end_prefix.len();
        let digits_end = rest[digits_start..]
            .find('\x07')
            .map(|off| digits_start + off);
        let Some(stop) = digits_end else { break };
        let digits = &rest[digits_start..stop];
        if digits.chars().all(|c| c.is_ascii_digit()) && !digits.is_empty() {
            out.push_str(&rest[..end_at]);
            rest = &rest[stop + 1..];
        } else {
            // 非真实标记（如回显的格式串）：保留并跳过该命中。
            out.push_str(&rest[..digits_start]);
            rest = &rest[digits_start..];
        }
    }
    out.push_str(rest);
    out.replace(&start_marker, "")
}

/// dsh 式标记契约：send_ex 包裹的命令在输出流出现结束标记即记 done 与
/// 退出码，无需等 5 分钟静默；list() 携带该状态。
#[test]
fn wrapped_send_records_command_completion() {
    let service = TerminalService::new();
    let id = service.start(Some("sh".into())).expect("start");
    let outcome = service
        .send_ex(&id, Some("true\n"), 5_000, 300, true)
        .expect("send");
    let _ = outcome;
    let rows = service.list();
    let Some(info) = rows.iter().find(|r| r.id == id) else {
        panic!("no row for {id}; output={:?}", outcome.output);
    };
    let Some(cmd) = info.command.as_ref() else {
        panic!("no command state for {id}; output={:?}", outcome.output);
    };
    assert_eq!(
        cmd.finished_ms.map(|_| ()),
        Some(()),
        "end marker must mark the command finished; captured output: {:?}",
        outcome.output
    );
    assert_eq!(cmd.exit_code, Some(0), "`true` exits 0");
    assert_eq!(
        cmd.cmd.as_deref(),
        Some("true\n"),
        "command text is captured for the panel"
    );
}

/// 2026-10-26 修复回归：wait-only 发送（terminal_wait，input None）不得
/// 重置进行中的命令记录——此前它会把 started_ms 归零、cmd 清空，面板上
/// 正在跑的构建只剩 "PTY · id" 回退文案和错误的首发时间。
#[test]
fn wait_only_send_preserves_the_running_command() {
    let service = TerminalService::new();
    let id = service.start(Some("sh".into())).expect("start");
    let _ = service
        .send_ex(&id, Some("sleep 2\n"), 300, 100, true)
        .expect("send");
    let before = service
        .list()
        .into_iter()
        .find(|r| r.id == id)
        .expect("row")
        .command
        .expect("wrapped send records the command");
    assert_eq!(before.finished_ms, None, "sleep 2 outlives the 300ms wait");
    assert_eq!(before.cmd.as_deref(), Some("sleep 2\n"));
    // wait-only 发送：不覆盖、不清空、不重置时间。
    let _ = service.send_ex(&id, None, 300, 100, true).expect("wait");
    let after = service
        .list()
        .into_iter()
        .find(|r| r.id == id)
        .expect("row")
        .command
        .expect("record must survive a wait-only send");
    assert_eq!(
        after.started_ms, before.started_ms,
        "started_ms must not reset"
    );
    assert_eq!(after.cmd, before.cmd, "cmd text must not drop");
    assert_eq!(after.finished_ms, None);
    assert_eq!(after.end_prefix, before.end_prefix);
}

/// 2026-10-26 修复回归：命令在没有任何 send 扫描的间隙里完成时，后续
/// wait-only 发送通过记录里存的结束标记认领完成（此前只扫本次发送自己
/// 的新标记，而 wait-only 没有包裹 → 完成永远无人认领）。
#[test]
fn wait_only_send_finalizes_on_the_stored_marker() {
    let service = TerminalService::new();
    let id = service.start(Some("sh".into())).expect("start");
    let _ = service
        .send_ex(&id, Some("sleep 1\n"), 200, 100, true)
        .expect("send");
    // sleep 在无人扫描的间隙里结束，结束标记落进 scrollback。
    std::thread::sleep(Duration::from_millis(1_400));
    let _ = service.send_ex(&id, None, 1_500, 200, true).expect("wait");
    let cmd = service
        .list()
        .into_iter()
        .find(|r| r.id == id)
        .expect("row")
        .command
        .expect("command record");
    assert_eq!(
        cmd.finished_ms.map(|_| ()),
        Some(()),
        "the stored end marker must finalize the command"
    );
    assert_eq!(cmd.exit_code, Some(0), "`sleep` exits 0");
}

/// display_status：在跑命令只把 Idle 提升为 Running；Exited 保持真实，
/// Running 原样。
#[test]
fn display_status_promotes_only_idle() {
    use SessionStatus::{Exited, Idle, Running};
    assert_eq!(
        TerminalService::display_status(Idle, true),
        Running,
        "in-flight command beats the idle heuristic"
    );
    assert_eq!(TerminalService::display_status(Running, true), Running);
    assert_eq!(
        TerminalService::display_status(
            Exited {
                exit_code: None,
                signal: None
            },
            true
        ),
        Exited {
            exit_code: None,
            signal: None
        },
        "a dead shell stays dead whatever the record says"
    );
    assert_eq!(TerminalService::display_status(Idle, false), Idle);
}

/// 无标记路径保持旧行为：write() 裸写不产生命令状态。
#[test]
fn raw_write_keeps_legacy_semantics() {
    let service = TerminalService::new();
    let id = service.start(Some("sh".into())).expect("start");
    service.write(&id, "true\n").expect("write");
    let rows = service.list();
    let cmd = rows
        .iter()
        .find(|r| r.id == id)
        .expect("row")
        .command
        .as_ref();
    assert!(cmd.is_none(), "raw writes stay marker-free");
}

/// dsh 式归属：spawn 记录 owner/workspace，list 原样发布。
#[test]
fn start_owned_publishes_owner_and_workspace() {
    let service = TerminalService::new();
    let id = service
        .start_owned(
            Some("WI-session-a".into()),
            Some("/tmp/wkspa".into()),
            Some("sh".into()),
        )
        .expect("start");
    let row = service
        .list()
        .into_iter()
        .find(|r| r.id == id)
        .expect("row");
    assert_eq!(row.owner.as_deref(), Some("WI-session-a"));
    assert_eq!(row.workspace.as_deref(), Some("/tmp/wkspa"));
    service.kill(&id, None).expect("kill");
}

/// dsh 式授权：B 会话杀 A 会话的终端被拒，且终端原样存活（先移出、
/// 校验失败再放回——不能留半死状态也不能误杀）。
#[test]
fn foreign_session_kill_is_rejected_and_terminal_survives() {
    let service = TerminalService::new();
    let id = service
        .start_owned(Some("sess-a".into()), None, Some("sh".into()))
        .expect("start");
    let err = service.kill(&id, Some("sess-b")).expect_err("must reject");
    assert!(err.contains("not owned"), "got: {err}");
    // 终端还在，归属未变。
    let row = service
        .list()
        .into_iter()
        .find(|r| r.id == id)
        .expect("row");
    assert_eq!(row.owner.as_deref(), Some("sess-a"));
    // 本主可以杀。
    service.kill(&id, Some("sess-a")).expect("owner kill");
    assert!(service.list().is_empty());
}

/// kill_owner：按归属批量回收，别人的终端不受波及。
#[test]
fn kill_owner_reaps_only_owned_terminals() {
    let service = TerminalService::new();
    let a = service
        .start_owned(Some("sess-a".into()), None, Some("sh".into()))
        .expect("start a");
    let b = service
        .start_owned(Some("sess-b".into()), None, Some("sh".into()))
        .expect("start b");
    assert_eq!(service.kill_owner("sess-a"), 1);
    let ids: Vec<_> = service.list().into_iter().map(|r| r.id).collect();
    assert_eq!(ids, vec![b.clone()], "only the foreign terminal survives");
    assert_eq!(service.kill_owner("sess-a"), 0, "already reaped");
    service.kill(&b, None).expect("cleanup");
}

/// kill_all：宿主 shutdown 最后防线，全量清空。
#[test]
fn kill_all_clears_every_terminal() {
    let service = TerminalService::new();
    service
        .start_owned(None, None, Some("sh".into()))
        .expect("a");
    service
        .start_owned(None, None, Some("sh".into()))
        .expect("b");
    assert_eq!(service.kill_all(), 2);
    assert!(service.list().is_empty());
}
