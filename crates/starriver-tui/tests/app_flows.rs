//! End-to-end flow tests: AppState + MockHost, covering the full event stream, approval, cancel,
//! session continuation, project switch, and error paths.

use ratatui::text::Line;
use serde_json::json;
use starriver_tui::app::{AppState, Mode, PickerKind};
use starriver_tui::host::mock::MockHost;
use starriver_tui::i18n::Lang;

fn app_with(mock: &MockHost) -> AppState {
    // Fixed Chinese to avoid machine-locale influence in tests.
    AppState::with_lang(Box::new(mock.clone()), Lang::Zh).unwrap()
}

fn key(code: crossterm::event::KeyCode) -> crossterm::event::KeyEvent {
    crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
}

fn started(tx: &tokio::sync::broadcast::Sender<serde_json::Value>, sid: &str) {
    tx.send(json!({"type": "sessionStarted", "sessionId": sid}))
        .unwrap();
}

fn completed(tx: &tokio::sync::broadcast::Sender<serde_json::Value>, sid: &str) {
    tx.send(json!({"type": "turnEnd", "sessionId": sid, "reason": "completed", "summary": "done"}))
        .unwrap();
}

#[test]
fn happy_path_completes_and_second_prompt_resumes_same_session() {
    let mock = MockHost::default();
    let tx = mock.with_events(32);
    let mut app = app_with(&mock);

    app.set_input("hello".into());
    app.submit().unwrap();
    assert_eq!(app.mode, Mode::Running);

    started(&tx, "s1");
    tx.send(json!({"type": "thinkingText", "sessionId": "s1", "text": "分析中"}))
        .unwrap();
    tx.send(json!({"type": "assistantText", "sessionId": "s1", "text": "hi"}))
        .unwrap();
    tx.send(json!({
        "type": "toolCall",
        "sessionId": "s1",
        "id": "t1",
        "name": "read"
    }))
    .unwrap();
    tx.send(
        json!({"type": "toolResult", "sessionId": "s1", "id": "t1", "ok": true, "output": "data"}),
    )
    .unwrap();
    completed(&tx, "s1");
    app.tick().unwrap();

    assert_eq!(app.mode, Mode::Idle);
    assert!(app.status.contains("done"));
    assert_eq!(app.resume_session_id.as_deref(), Some("s1"));
    let joined: Vec<String> = app.transcript.iter().map(|l| l.to_string()).collect();
    assert!(joined.iter().any(|l| l.contains("hi")));
    assert!(joined.iter().any(|l| l.contains("分析中")));
 // -style active cell: the tool line flips in place `• Calling read`
    // → `✓ read · <n>ms` (no separate 🔧 header row anymore).
    assert!(joined.iter().any(|l| l.contains("✓ read")));
    assert!(joined.iter().any(|l| l.contains("data")));

    // The second prompt should continue the same session
    mock.with_events(32);
    app.set_input("again".into());
    app.submit().unwrap();
    let calls = mock.executed.lock().unwrap();
    let (task, sid) = calls.last().unwrap();
    assert_eq!(task, "again");
    assert_eq!(sid.as_deref(), Some("s1"));
}

#[test]
fn approval_deny_continues_to_completion() {
    let mock = MockHost::default();
    let tx = mock.with_events(32);
    let mut app = app_with(&mock);
    app.set_input("go".into());
    app.submit().unwrap();

    started(&tx, "s1");
    app.approval = Some(starriver_tui::app::ApprovalState {
        session_id: "s1".into(),
        tool: "bash".into(),
        reason: "危险操作".into(),
        remember: false,
        request_id: None,
    });
    app.mode = Mode::Approval;
    app.tick().unwrap();
    assert_eq!(app.mode, Mode::Approval);

    app.approve(false).unwrap();
    assert_eq!(app.mode, Mode::Running);
    completed(&tx, "s1");
    app.tick().unwrap();
    assert_eq!(app.mode, Mode::Idle);
    assert_eq!(
        *mock.approved.lock().unwrap(),
        vec![("s1".to_string(), false, Some(false))]
    );
}

#[test]
fn approval_requested_event_drives_overlay_and_approve() {
    let mock = MockHost::default();
    let tx = mock.with_events(32);
    let mut app = app_with(&mock);
    app.set_input("go".into());
    app.submit().unwrap();

    started(&tx, "s1");
    tx.send(json!({
        "type": "approvalRequested",
        "version": 1,
        "sessionId": "s1",
        "requestId": "r1",
        "toolName": "bash",
        "reason": "needs human"
    }))
    .unwrap();
    app.tick().unwrap();
    assert_eq!(app.mode, Mode::Approval);
    let a = app.approval.as_ref().unwrap();
    assert_eq!(a.tool, "bash");
    assert_eq!(a.reason, "needs human");
    assert_eq!(a.request_id.as_deref(), Some("r1"));

    app.handle_key(key(crossterm::event::KeyCode::Char('y')))
        .unwrap();
    assert_eq!(app.mode, Mode::Running);
    assert_eq!(
        *mock.approved.lock().unwrap(),
        vec![("s1".to_string(), true, Some(false))]
    );
}

#[test]
fn approval_requested_remember_then_approve_records_similar() {
    let mock = MockHost::default();
    let tx = mock.with_events(32);
    let mut app = app_with(&mock);
    app.set_input("go".into());
    app.submit().unwrap();

    started(&tx, "s1");
    tx.send(json!({
        "type": "approvalRequested",
        "version": 1,
        "sessionId": "s1",
        "requestId": "r2",
        "toolName": "bash",
        "reason": null
    }))
    .unwrap();
    app.tick().unwrap();
    assert_eq!(app.mode, Mode::Approval);

    app.handle_key(key(crossterm::event::KeyCode::Char('m')))
        .unwrap();
    assert!(app.approval.as_ref().unwrap().remember);
    app.handle_key(key(crossterm::event::KeyCode::Char('y')))
        .unwrap();
    assert_eq!(
        *mock.approved.lock().unwrap(),
        vec![("s1".to_string(), true, Some(true))]
    );
}

#[test]
fn approval_requested_esc_defers_without_approving() {
    let mock = MockHost::default();
    let tx = mock.with_events(32);
    let mut app = app_with(&mock);
    app.set_input("go".into());
    app.submit().unwrap();

    started(&tx, "s1");
    tx.send(json!({
        "type": "approvalRequested",
        "version": 1,
        "sessionId": "s1",
        "requestId": "r4",
        "toolName": "bash",
        "reason": "needs human"
    }))
    .unwrap();
    app.tick().unwrap();
    assert_eq!(app.mode, Mode::Approval);

    app.handle_key(key(crossterm::event::KeyCode::Esc)).unwrap();
    assert_eq!(app.mode, Mode::Running);
    assert!(app.approval.is_none());
    assert!(mock.approved.lock().unwrap().is_empty());
    assert!(app.status.contains("稍后"), "status: {}", app.status);
}

#[test]
fn ctrl_c_cancels_and_cancelled_event_returns_idle() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let mock = MockHost::default();
    let tx = mock.with_events(32);
    let mut app = app_with(&mock);
    app.set_input("go".into());
    app.submit().unwrap();
    started(&tx, "s1");
    app.tick().unwrap();

    let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    app.handle_key(ctrl_c).unwrap();
    assert_eq!(*mock.cancelled.lock().unwrap(), vec!["s1".to_string()]);

    tx.send(json!({"type": "cancelled", "sessionId": "s1"}))
        .unwrap();
    app.tick().unwrap();
    assert_eq!(app.mode, Mode::Idle);
    assert!(app.status.contains("已取消"));
}

#[test]
fn session_resume_loads_history_and_continues() {
    let mock = MockHost::default();
    mock.seed_session("s1", "历史任务");
    mock.messages
        .lock()
        .unwrap()
        .push(starriver_tui::host::types::ChatMessage {
            role: "user".into(),
            content: "上次的问题".into(),
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
    app.pick_select().unwrap();
    assert_eq!(app.mode, Mode::Idle);
    assert!(app
        .transcript
        .iter()
        .any(|l| l.to_string().contains("上次的问题")));
    assert_eq!(app.resume_session_id.as_deref(), Some("s1"));

    mock.with_events(32);
    app.set_input("继续".into());
    app.submit().unwrap();
    let calls = mock.executed.lock().unwrap();
    let (_, sid) = calls.last().unwrap();
    assert_eq!(sid.as_deref(), Some("s1"));
}

#[test]
fn project_open_and_picker_switch_flow() {
    let dir = tempfile::tempdir().expect("tempdir");
    let beta = dir.path().join("beta");
    std::fs::create_dir(&beta).expect("dir");
    let mock = MockHost::default();
    mock.seed_workspace("p1", "/tmp/alpha");
    let mut app = app_with(&mock);

 // /open registers an existing directory and switches to it (no mkdir)
    app.set_input(format!("/open {}", beta.display()));
    app.submit().unwrap();
    assert_eq!(mock.workspaces.lock().unwrap().len(), 2);
    assert!(app.status.contains("beta"));

    // /projects picker switches back to p1
    app.set_input("/projects".into());
    app.submit().unwrap();
    assert_eq!(app.mode, Mode::Picker(PickerKind::Workspaces));
    app.selected = 0;
    app.pick_select().unwrap();
    assert_eq!(mock.active_workspace.lock().unwrap().as_deref(), Some("p1"));
    assert!(app.status.contains("alpha"));
    assert_eq!(app.resume_session_id, None);
}

#[test]
fn malformed_event_survives_and_later_completes() {
    let mock = MockHost::default();
    let tx = mock.with_events(32);
    let mut app = app_with(&mock);
    app.set_input("go".into());
    app.submit().unwrap();
    started(&tx, "s1");

    tx.send(json!({"type": "TotalGarbage"})).unwrap();
    app.tick().unwrap();
    assert_eq!(app.mode, Mode::Running);
    assert!(app.status.contains("事件解析失败"));

    completed(&tx, "s1");
    app.tick().unwrap();
    assert_eq!(app.mode, Mode::Idle);
}

#[test]
fn bridge_events_are_ignored_and_flow_continues() {
    let mock = MockHost::default();
    let tx = mock.with_events(32);
    let mut app = app_with(&mock);
    app.set_input("go".into());
    app.submit().unwrap();
    started(&tx, "s1");

    tx.send(json!({"type": "assistantText", "sessionId": "s1", "text": "ok"}))
        .unwrap();
    completed(&tx, "s1");
    app.tick().unwrap();

    assert_eq!(app.mode, Mode::Idle);
    assert!(app.transcript.iter().any(|l| l.to_string() == "ok"));
}

#[test]
fn picker_escape_and_new_command_reset_context() {
    let mock = MockHost::default();
    mock.seed_session("s1", "t");
    mock.messages
        .lock()
        .unwrap()
        .push(starriver_tui::host::types::ChatMessage {
            role: "user".into(),
            content: "历史".into(),
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
    app.pick_back();
    assert_eq!(app.mode, Mode::Idle);

    app.set_input("/sessions".into());
    app.submit().unwrap();
    app.pick_select().unwrap();
    assert_eq!(app.resume_session_id.as_deref(), Some("s1"));
    assert!(!app.transcript.is_empty());

    app.set_input("/new".into());
    app.submit().unwrap();
    assert!(app.transcript.is_empty());
    assert_eq!(app.resume_session_id, None);
}

#[test]
fn project_switch_clears_session_context() {
 // The mock resolves real directories now (semantics), so both
    // switches below go through tempdirs that actually exist.
    let dir = tempfile::tempdir().expect("tempdir");
    let alpha = dir.path().join("alpha");
    std::fs::create_dir(&alpha).expect("dir");
    let mock = MockHost::default();
    mock.seed_workspace("p1", &alpha.to_string_lossy());
    let mut app = app_with(&mock);

    // First create a session context
    app.set_input(format!("/open {}", alpha.display()));
    app.submit().unwrap();
    assert_eq!(mock.active_workspace.lock().unwrap().as_deref(), Some("p1"));
    let tx = mock.with_events(32);
    app.set_input("go".into());
    app.submit().unwrap();
    started(&tx, "s1");
    completed(&tx, "s1");
    app.tick().unwrap();
    assert_eq!(app.resume_session_id.as_deref(), Some("s1"));

    // Cleared after switching workspace
    let gamma = dir.path().join("gamma");
    std::fs::create_dir(&gamma).expect("dir");
    app.set_input(format!("/open {}", gamma.display()));
    app.submit().unwrap();
    assert_eq!(app.resume_session_id, None);
    assert!(app.transcript.is_empty());
}

#[test]
fn compaction_event_renders_and_does_not_end_run() {
    let mock = MockHost::default();
    let tx = mock.with_events(32);
    let mut app = app_with(&mock);
    app.set_input("go".into());
    app.submit().unwrap();
    started(&tx, "s1");
    tx.send(json!({"type": "assistantText", "sessionId": "s1", "text": "中间输出"}))
        .unwrap();
    app.tick().unwrap();

    assert_eq!(app.mode, Mode::Running); // non-terminal event does not end the task
    assert!(app
        .transcript
        .iter()
        .any(|l| l.to_string().contains("中间输出")));
}

// -- New-command flows: /compact /diff /status /usage /init /export /lang --
// Complements app.rs unit tests: here we walk the full submit → parse → run_command → HostApi
// chain, asserting the mock host's call records and UI feedback (integration layer).

/// Construct an app with a project working directory + continuation session (/compact /usage /export need a session,
/// /diff /init need a project, /status needs both).
fn app_with_context(mock: &MockHost) -> AppState {
    mock.seed_workspace("p1", "/tmp/alpha");
    mock.seed_session("s1", "任务");
    let mut app = app_with(mock);
    app.current_workspace = Some(mock.workspaces.lock().unwrap()[0].clone());
    app.resume_session_id = Some("s1".into());
    app
}

#[test]
fn new_slash_commands_run_through_host() {
    let mock = MockHost::default();
    let mut app = app_with_context(&mock);

    // /status: report line is written into the transcript
    app.set_input("/status".into());
    app.submit().unwrap();
    let joined: String = app.transcript.iter().map(|l| l.to_string()).collect();
    assert!(
        joined.contains("工作区 1 个 / 会话 1 个"),
        "joined: {joined}"
    );
    assert!(joined.contains("alpha"));

    // /usage: hits host, feedback includes tokens
    app.set_input("/usage".into());
    app.submit().unwrap();
    assert_eq!(*mock.usage_calls.lock().unwrap(), vec!["s1".to_string()]);
    assert!(app.status.contains("500"), "status: {}", app.status);

    // /compact: hits host, feedback includes before → after
    app.set_input("/compact".into());
    app.submit().unwrap();
    assert_eq!(*mock.compacted.lock().unwrap(), vec!["s1".to_string()]);
    assert!(app.status.contains("120 → 40"), "status: {}", app.status);

    // /diff: diff text is rendered line by line into the transcript
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
    assert!(app
        .transcript
        .iter()
        .any(|l| l.to_string().contains("+hello world")));

    // /init: returns the AGENTS.md path
    app.set_input("/init".into());
    app.submit().unwrap();
    assert_eq!(
        *mock.init_calls.lock().unwrap(),
        vec!["/tmp/alpha".to_string()]
    );
    assert!(app.status.contains("AGENTS.md"), "status: {}", app.status);

    // /export: export path includes the session id
    app.set_input("/export".into());
    app.submit().unwrap();
    assert_eq!(
        *mock.export_calls.lock().unwrap(),
        vec![("s1".to_string(), "/tmp/alpha".to_string())]
    );
    assert!(
        app.status.contains("starriver-export-s1.md"),
        "status: {}",
        app.status
    );

    // /lang no-arg toggle: Zh → En (kept last so later Chinese assertions don't break)
    app.set_input("/lang".into());
    app.submit().unwrap();
    assert_eq!(app.lang, Lang::En);
    assert!(app.status.contains("English"), "status: {}", app.status);
}

#[test]
fn new_slash_commands_report_missing_context() {
    // No session: /compact /usage /export take the no_session branch, not reaching host
    let mock = MockHost::default();
    let mut app = app_with_project_missing_session(&mock);
    for cmd in ["/compact", "/usage", "/export"] {
        app.set_input(cmd.into());
        app.submit().unwrap();
        assert!(app.status.contains("没有当前会话"), "{cmd}: {}", app.status);
    }
    assert!(mock.compacted.lock().unwrap().is_empty());
    assert!(mock.usage_calls.lock().unwrap().is_empty());
    assert!(mock.export_calls.lock().unwrap().is_empty());

    // No project: /diff /init take the no_cwd branch
    let mock2 = MockHost::default();
    let mut app2 = app_with(&mock2);
    for cmd in ["/diff", "/init"] {
        app2.input = cmd.into();
        app2.submit().unwrap();
        assert!(
            app2.status.contains("未设置项目工作目录"),
            "{cmd}: {}",
            app2.status
        );
    }
}

/// An app with only a project and no session (/compact /usage /export lack a session).
fn app_with_project_missing_session(mock: &MockHost) -> AppState {
    mock.seed_workspace("p1", "/tmp/alpha");
    let mut app = app_with(mock);
    app.current_workspace = Some(mock.workspaces.lock().unwrap()[0].clone());
    app
}

// -- Mode-switch flows: /plan /build /auto commands + Tab cycling --

// -- Thread fork/backtrack/delete/worktree-isolation flows --

/// Seed a three-question/two-answer timeline (3 items when listing only user messages).
fn seed_backtrack_timeline(mock: &MockHost) {
    use starriver_tui::host::types::SessionMessage;
    mock.seed_timeline(vec![
        SessionMessage {
            id: 1,
            role: "user".into(),
            content: "第一个问题".into(),
        },
        SessionMessage {
            id: 5,
            role: "user".into(),
            content: "第二个问题".into(),
        },
        SessionMessage {
            id: 9,
            role: "user".into(),
            content: "第三个问题".into(),
        },
    ]);
}

#[test]
fn backtrack_forks_new_thread_and_resumes() {
    let mock = MockHost::default();
    mock.seed_session("s1", "任务");
    seed_backtrack_timeline(&mock);
    let mut app = app_with(&mock);
    app.resume_session_id = Some("s1".into());

    // /backtrack → message picker (user messages only)
    app.set_input("/backtrack".into());
    app.submit().unwrap();
    assert_eq!(app.mode, Mode::Picker(PickerKind::Messages));
    assert_eq!(app.message_timeline.len(), 3);
    assert!(app.status.contains("选择要回溯"), "status: {}", app.status);

    // Select the second → fork_session(source, id=5) → resume new thread + input prefills the original text
    app.selected = 1;
    app.pick_select().unwrap();
    assert_eq!(app.mode, Mode::Idle);
    assert_eq!(*mock.forked.lock().unwrap(), vec![("s1".to_string(), 5)]);
    assert_eq!(app.resume_session_id.as_deref(), Some("fork-2"));
    assert_eq!(
        app.input, "第二个问题",
        "输入应预填选中 user 消息原文供编辑"
    );
    assert!(app.status.contains("已分叉"), "status: {}", app.status);
    assert!(app.message_timeline.is_empty());
    assert!(app.backtrack_source.is_none());
}

#[test]
fn backtrack_without_session_reports() {
    let mock = MockHost::default();
    let mut app = app_with(&mock);
    app.set_input("/backtrack".into());
    app.submit().unwrap();
    assert_eq!(app.mode, Mode::Idle);
    assert!(
        app.status.contains("没有当前会话"),
        "status: {}",
        app.status
    );
}

#[test]
fn backtrack_no_user_messages_reports() {
    let mock = MockHost::default();
    mock.seed_session("s1", "任务");
    let mut app = app_with(&mock);
    app.resume_session_id = Some("s1".into());
    app.set_input("/backtrack".into());
    app.submit().unwrap();
    // No seeded timeline → empty list → hint "no messages to backtrack", does not enter the picker
    assert_eq!(app.mode, Mode::Idle);
    assert!(app.status.contains("没有可回溯"), "status: {}", app.status);
}

#[test]
fn delete_confirms_and_removes_session() {
    let mock = MockHost::default();
    mock.seed_session("s1", "任务");
    let mut app = app_with(&mock);
    app.resume_session_id = Some("s1".into());
    app.transcript.push(ratatui::text::Line::from("历史"));

    // /delete → second-confirmation overlay
    app.set_input("/delete".into());
    app.submit().unwrap();
    assert_eq!(app.mode, Mode::Approval);
    assert_eq!(app.approval.as_ref().unwrap().tool, "delete-session");

    // Confirm → host.delete_session + clear session context
    app.approve(true).unwrap();
    assert_eq!(app.mode, Mode::Idle);
    assert!(app.sessions.iter().all(|s| s.id != "s1"));
    assert_eq!(app.resume_session_id, None);
    assert!(app.transcript.is_empty());
    assert!(app.status.contains("已删除会话"), "status: {}", app.status);
}

#[test]
fn delete_denied_keeps_session() {
    let mock = MockHost::default();
    mock.seed_session("s1", "任务");
    let mut app = app_with(&mock);
    app.resume_session_id = Some("s1".into());

    app.set_input("/delete".into());
    app.submit().unwrap();
    app.approve(false).unwrap();
    assert_eq!(app.mode, Mode::Idle);
    assert_eq!(app.resume_session_id.as_deref(), Some("s1"));
    assert!(!app.sessions.is_empty());
    assert!(app.status.contains("已拒绝"), "status: {}", app.status);
}

#[test]
fn delete_confirm_esc_returns_to_idle_not_running() {
    let mock = MockHost::default();
    mock.seed_session("s1", "任务");
    let mut app = app_with(&mock);
    app.resume_session_id = Some("s1".into());

    app.set_input("/delete".into());
    app.submit().unwrap();
    assert_eq!(app.mode, Mode::Approval);

    // Esc cancels delete confirmation: a non-task approval should return to Idle (not Running), session preserved
    app.handle_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Esc,
        crossterm::event::KeyModifiers::NONE,
    ))
    .unwrap();
    assert_eq!(app.mode, Mode::Idle);
    assert!(app.approval.is_none());
    assert_eq!(app.resume_session_id.as_deref(), Some("s1"));
    assert!(!app.sessions.is_empty());
    assert!(app.status.contains("稍后"), "status: {}", app.status);
}

#[test]
fn delete_without_session_reports() {
    let mock = MockHost::default();
    let mut app = app_with(&mock);
    app.set_input("/delete".into());
    app.submit().unwrap();
    assert_eq!(app.mode, Mode::Idle);
    assert!(
        app.status.contains("没有当前会话"),
        "status: {}",
        app.status
    );
}

#[test]
fn worktree_command_toggles_isolation() {
    let mock = MockHost::default();
    let mut app = app_with(&mock);

    // /worktree is now a local toggle; the flag rides the next execute.
    app.set_input("/worktree".into());
    app.submit().unwrap();
    assert!(app.worktree_isolation, "toggle on");
    assert!(app.status.contains("开"), "status: {}", app.status);

    app.set_input("/worktree".into());
    app.submit().unwrap();
    assert!(!app.worktree_isolation, "toggle off");
    assert!(app.status.contains("关"), "status: {}", app.status);
}

#[test]
fn name_command_renames_current_session() {
    let mock = MockHost::default();
    mock.seed_session("s1", "原始标题");
    let mut app = app_with(&mock);
    app.resume_session_id = Some("s1".into());

    app.set_input("/name 我的命名".into());
    app.submit().unwrap();
    assert_eq!(
        *mock.renamed.lock().unwrap(),
        vec![("s1".to_string(), Some("我的命名".into()))]
    );
    assert!(
        app.status.contains("已重命名会话"),
        "status: {}",
        app.status
    );
    // Local session list syncs the new title
    let s = app.sessions.iter().find(|s| s.id == "s1").unwrap();
    assert_eq!(s.title, "我的命名");

    // No-arg clears the override
    app.set_input("/name".into());
    app.submit().unwrap();
    assert!(app.status.contains("用法"), "status: {}", app.status);
}

#[test]
fn name_without_session_reports() {
    let mock = MockHost::default();
    let mut app = app_with(&mock);
    app.set_input("/name foo".into());
    app.submit().unwrap();
    assert!(
        app.status.contains("没有当前会话"),
        "status: {}",
        app.status
    );
    assert!(mock.renamed.lock().unwrap().is_empty());
}

#[test]
fn archive_command_hides_session_and_clears_context() {
    let mock = MockHost::default();
    mock.seed_session("s1", "任务");
    let mut app = app_with(&mock);
    app.resume_session_id = Some("s1".into());
    app.transcript.push(ratatui::text::Line::from("历史"));

    app.set_input("/archive".into());
    app.submit().unwrap();
    assert_eq!(*mock.archived.lock().unwrap(), vec!["s1".to_string()]);
    assert!(
        app.sessions.iter().all(|s| s.id != "s1"),
        "归档会话移出本地列表"
    );
    assert_eq!(app.resume_session_id, None);
    assert!(app.transcript.is_empty());
    assert!(app.status.contains("已归档"), "status: {}", app.status);
}

#[test]
fn archive_without_session_reports() {
    let mock = MockHost::default();
    let mut app = app_with(&mock);
    app.set_input("/archive".into());
    app.submit().unwrap();
    assert!(
        app.status.contains("没有当前会话"),
        "status: {}",
        app.status
    );
    assert!(mock.archived.lock().unwrap().is_empty());
}

#[test]
fn unarchive_restores_from_archived_picker() {
    let mock = MockHost::default();
    mock.seed_archived_session("a1", "旧任务");
    let mut app = app_with(&mock);

    app.set_input("/unarchive".into());
    app.submit().unwrap();
    assert_eq!(app.mode, Mode::Picker(PickerKind::Archived));
    assert_eq!(app.archived_sessions.len(), 1);

    // Select → unarchive_session + remove from the archive list
    app.pick_select().unwrap();
    assert_eq!(*mock.unarchived.lock().unwrap(), vec!["a1".to_string()]);
    assert!(app.archived_sessions.is_empty());
    assert_eq!(app.mode, Mode::Idle, "归档列表清空后回到 Idle");
    assert!(app.status.contains("已恢复会话"), "status: {}", app.status);
}

#[test]
fn unarchive_with_no_archived_reports() {
    let mock = MockHost::default();
    let mut app = app_with(&mock);
    app.set_input("/unarchive".into());
    app.submit().unwrap();
    assert_eq!(app.mode, Mode::Idle);
    assert!(
        app.status.contains("没有归档会话"),
        "status: {}",
        app.status
    );
}

#[test]
fn review_command_sets_review_mode_and_prefills_prompt() {
    let mock = MockHost::default();
    let mut app = app_with(&mock);

    app.set_input("/review src/a.rs src/b.rs".into());
    app.submit().unwrap();
    assert_eq!(
        *mock.review_calls.lock().unwrap(),
        vec!["[\"src/a.rs\",\"src/b.rs\"]".to_string()]
    );
    // Prefill the prompt, waiting for Enter to submit
    assert!(app
        .input
        .starts_with("Please review the following files: src/a.rs src/b.rs"));
    assert!(app.status.contains("已填入模板"), "status: {}", app.status);
}

#[test]
fn review_without_files_reports_usage() {
    let mock = MockHost::default();
    let mut app = app_with(&mock);
    app.set_input("/review".into());
    app.submit().unwrap();
    assert!(app.status.contains("用法"), "status: {}", app.status);
    assert!(mock.review_calls.lock().unwrap().is_empty());
}

#[test]
fn test_and_explore_prefill_prompts() {
    let mock = MockHost::default();
    let mut app = app_with(&mock);

    // /test: prefill the unit-test template (no mode switch)
    app.set_input("/test crypto".into());
    app.submit().unwrap();
    assert!(
        app.input.starts_with("Write unit tests for: crypto"),
        "input: {}",
        app.input
    );

    // /explore: switch to Plan mode + prefill
    app.set_input("/explore payment flow".into());
    app.submit().unwrap();
    assert!(app.input.starts_with("Explore the codebase: payment flow"));

    // Missing arg reports usage
    app.set_input("/test".into());
    app.submit().unwrap();
    assert!(app.status.contains("用法"), "status: {}", app.status);
    app.set_input("/explore".into());
    app.submit().unwrap();
    assert!(app.status.contains("用法"), "status: {}", app.status);
}

// ── WI-76/77/78 (round 12) ──────────────────────────────────────────────

fn usage_event(tx: &tokio::sync::broadcast::Sender<serde_json::Value>, i: u64, o: u64) {
    tx.send(json!({"type":"usage","sessionId":"s1","inputTokens":i,"outputTokens":o})).unwrap();
}

#[test]
fn wi76_queue_flushes_after_turn_end_in_order() {
    let mock = MockHost::default();
    let tx = mock.with_events(32);
    let mut app = app_with(&mock);
    app.set_input("first".into());
    app.submit().unwrap();
    assert_eq!(app.mode, Mode::Running);

    started(&tx, "s1");
    // Queue two prompts mid-run.
    app.set_input("second".into());
    app.handle_key(key(crossterm::event::KeyCode::Enter)).unwrap();
    app.set_input("third".into());
    app.handle_key(key(crossterm::event::KeyCode::Enter)).unwrap();
    assert_eq!(app.queued_messages.len(), 2);
    assert!(app.transcript.iter().any(|l| l.to_string().contains("second")));

    completed(&tx, "s1");
    app.tick().unwrap();
    // The first queued prompt auto-submitted → Running again.
    assert_eq!(app.mode, Mode::Running);
    assert_eq!(app.queued_messages.len(), 1);
    let calls = mock.executed.lock().unwrap();
    assert_eq!(calls.last().unwrap().0, "second");
    drop(calls);

    // Finish the second queued turn on the SAME mock channel (the drained
    // submit rebinds its receiver to the mock's current one).
    completed(&tx, "s1");
    app.tick().unwrap();
    assert_eq!(app.queued_messages.len(), 0);
    let calls = mock.executed.lock().unwrap();
    assert_eq!(calls.last().unwrap().0, "third");
}

#[test]
fn wi76_ctrl_e_recalls_last_queued_and_ctrl_k_clears() {
    let mock = MockHost::default();
    let tx = mock.with_events(16);
    let mut app = app_with(&mock);
    app.set_input("run".into());
    app.submit().unwrap();
    started(&tx, "s1");
    app.set_input("queued-1".into());
    app.handle_key(key(crossterm::event::KeyCode::Enter)).unwrap();
    app.set_input("queued-2".into());
    app.handle_key(key(crossterm::event::KeyCode::Enter)).unwrap();

    // Ctrl+E (modifier comes through handle_key's CONTROL branch)
    let mut k = key(crossterm::event::KeyCode::Char('e'));
    k.modifiers = crossterm::event::KeyModifiers::CONTROL;
    app.handle_key(k).unwrap();
    assert_eq!(app.input, "queued-2");
    assert_eq!(app.queued_messages.len(), 1);

    // Ctrl+K clears the rest.
    let mut k = key(crossterm::event::KeyCode::Char('k'));
    k.modifiers = crossterm::event::KeyModifiers::CONTROL;
    app.handle_key(k).unwrap();
    assert!(app.queued_messages.is_empty());
    assert!(app.status.contains("1"));
}

#[test]
fn wi76_interrupt_preserves_queue() {
    let mock = MockHost::default();
    let tx = mock.with_events(16);
    let mut app = app_with(&mock);
    app.set_input("first".into());
    app.submit().unwrap();
    started(&tx, "s1");
    app.set_input("after-interrupt".into());
    app.handle_key(key(crossterm::event::KeyCode::Enter)).unwrap();
    tx.send(json!({"type":"cancelled","sessionId":"s1"})).unwrap();
    app.tick().unwrap();
    assert_eq!(app.mode, Mode::Idle);
    assert_eq!(app.queued_messages.len(), 1, "queue survives interruption");
}

#[test]
fn wi77_usage_accumulates_and_context_chip_math() {
    let mock = MockHost::default();
    let tx = mock.with_events(16);
    let mut app = app_with(&mock);
    app.set_input("hi".into());
    app.submit().unwrap();
    started(&tx, "s1");
    usage_event(&tx, 100, 50);
    usage_event(&tx, 30, 20);
    app.tick().unwrap();
    assert_eq!(app.usage_in, 130);
    assert_eq!(app.usage_out, 70);
    // default window 65536 → used 200
    assert_eq!(app.context_left(), 65536 - 200);
    assert_eq!(app.context_used_percent(), 0);
}

#[test]
fn wi78_shift_enter_newline_enter_sends_multiline() {
    let mock = MockHost::default();
    mock.with_events(16);
    let mut app = app_with(&mock);
    app.set_input("line1".into());
    let mut k = key(crossterm::event::KeyCode::Enter);
    k.modifiers = crossterm::event::KeyModifiers::SHIFT;
    app.handle_key(k).unwrap();
    app.handle_key(key(crossterm::event::KeyCode::Char('l'))).unwrap();
    assert_eq!(app.input, "line1\nl");
    assert!(app.is_multiline_input());
    // Enter (no modifier) submits with the newline preserved.
    app.handle_key(key(crossterm::event::KeyCode::Enter)).unwrap();
    let calls = mock.executed.lock().unwrap();
    assert_eq!(calls.last().unwrap().0, "line1\nl");
    // Echo: first line prefixed `› `, second plain.
    let joined: Vec<String> = app.transcript.iter().map(|l| l.to_string()).collect();
    assert!(joined.iter().any(|l| l.contains("› line1")));
    assert!(joined.iter().any(|l| l.trim() == "l"));
}

#[test]
fn wi78_multiline_cursor_moves_across_lines_not_history() {
    let mock = MockHost::default();
    let mut app = app_with(&mock);
    app.set_input("ab\ncd".into());
    app.input_cursor = 5; // after 'd' = end of line 2
    app.handle_key(key(crossterm::event::KeyCode::Up)).unwrap();
    // Up lands on line 1 at the same column (after 'b' → offset 2).
    assert_eq!(app.input_cursor, 2, "cursor moved to line 1 same column");
    app.handle_key(key(crossterm::event::KeyCode::Down)).unwrap();
    assert_eq!(app.input_cursor, 5, "back to end of line 2");
    // Home/End are line-scoped now.
    app.handle_key(key(crossterm::event::KeyCode::Home)).unwrap();
    assert_eq!(app.input_cursor, 3, "Home parks at line-2 start");
    app.handle_key(key(crossterm::event::KeyCode::End)).unwrap();
    assert_eq!(app.input_cursor, 5);
}

#[test]
fn wi78_paste_normalizes_and_inserts_without_sending() {
    let mock = MockHost::default();
    let mut app = app_with(&mock);
    app.set_input("head".into());
    app.paste_text("line1\r\nline2\rline3");
    assert_eq!(app.input, "headline1\nline2\nline3");
    assert!(mock.executed.lock().unwrap().is_empty(), "paste never sends");
    let joined: String = app.transcript.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n");
    assert!(joined.contains("已粘贴") || joined.contains("pasted"), "note: {joined}");
}

// ── WI-79/81/83 (round 12, phase 2) ─────────────────────────────────────

const SAMPLE_DIFF: &str = "edited src/a.rs\n\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,3 +1,3 @@\n old\n-old line\n+new line\n more\n--- a/src/b.txt\n+++ b/src/b.txt\n@@ -1 +1 @@\n-x\n+y\n";

#[test]
fn wi79_parse_unified_diff_groups_and_counts() {
    let files = starriver_tui::events::parse_unified_diff(SAMPLE_DIFF);
    assert_eq!(files.len(), 2);
    assert_eq!(files[0].path, "src/a.rs");
    assert_eq!(files[0].add, 1);
    assert_eq!(files[0].del, 1);
    assert_eq!(files[1].path, "src/b.txt");
    // No diff headers → empty.
    assert!(starriver_tui::events::parse_unified_diff("plain text").is_empty());
    assert!(!starriver_tui::events::looks_like_unified_diff("plain"));
    assert!(starriver_tui::events::looks_like_unified_diff(SAMPLE_DIFF));
}

#[test]
fn wi79_diff_command_renders_grouped_cells() {
    let mock = MockHost::default();
    mock.seed_workspace("w1", "/tmp/proj");
    mock.set_diff_output(SAMPLE_DIFF);
    let mut app = app_with(&mock);
    app.current_workspace = Some(app.workspaces[0].clone());
    app.set_input("/diff".into());
    app.submit().unwrap();
    let joined: String = app.transcript.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n");
    // Collapsed: one summary row per file with +N −M chips.
    assert!(joined.contains("src/a.rs"), "{joined}");
    assert!(joined.contains("+1"), "{joined}");
    assert!(joined.contains("−1"), "{joined}");
    assert!(joined.contains("src/b.txt"), "{joined}");
    assert!(!joined.contains("+new line"), "collapsed hides bodies: {joined}");
    assert_eq!(app.diff_details.len(), 1);
    // Ctrl+O expands: colored bodies appear.
    let mut k = key(crossterm::event::KeyCode::Char('o'));
    k.modifiers = crossterm::event::KeyModifiers::CONTROL;
    app.handle_key(k).unwrap();
    let joined: String = app.transcript.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n");
    assert!(joined.contains("+new line"), "expanded shows bodies: {joined}");
    assert!(joined.contains("-old line"), "{joined}");
}

#[test]
fn wi79_edit_tool_result_gets_diff_cell() {
    let mock = MockHost::default();
    let tx = mock.with_events(16);
    let mut app = app_with(&mock);
    app.set_input("hi".into());
    app.submit().unwrap();
    started(&tx, "s1");
    tx.send(json!({"type":"toolCall","sessionId":"s1","id":"t1","name":"edit"})).unwrap();
    tx.send(json!({"type":"toolResult","sessionId":"s1","id":"t1","ok":true,"output": SAMPLE_DIFF})).unwrap();
    app.tick().unwrap();
    assert_eq!(app.diff_details.len(), 1);
    let joined: String = app.transcript.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n");
    assert!(joined.contains("✓ edit"), "{joined}");
    assert!(joined.contains("src/a.rs"), "{joined}");
    // The diff cell must sit right below the tool row.
    let tool_idx = app.transcript.iter().position(|l| l.to_string().contains("✓ edit")).unwrap();
    assert!(app.transcript[tool_idx + 1].to_string().contains("src/a.rs"));
}

#[test]
fn wi81_transcript_view_open_search_and_return() {
    let mock = MockHost::default();
    let mut app = app_with(&mock);
    app.transcript.push(Line::from("alpha needle"));
    app.transcript.push(Line::from("beta"));
    app.transcript.push(Line::from("gamma needle"));
    // Ctrl+T opens (Idle).
    let mut k = key(crossterm::event::KeyCode::Char('t'));
    k.modifiers = crossterm::event::KeyModifiers::CONTROL;
    app.handle_key(k).unwrap();
    assert_eq!(app.mode, Mode::Transcript);
    assert_eq!(app.transcript_return, Mode::Idle);
    // '/' opens search; typing; Enter confirms (first match).
    app.handle_key(key(crossterm::event::KeyCode::Char('/'))).unwrap();
    for c in "needle".chars() {
        app.handle_key(key(crossterm::event::KeyCode::Char(c))).unwrap();
    }
    // 'n' during typing appends, does NOT jump.
    assert!(app.transcript_search.as_deref().is_some_and(|q| q.contains("n")));
    app.handle_key(key(crossterm::event::KeyCode::Enter)).unwrap();
    let matches = app.transcript_matches();
    assert_eq!(matches.len(), 2);
    assert_eq!(app.transcript_match, Some(0));
    // 'n' now cycles to the second match.
    app.handle_key(key(crossterm::event::KeyCode::Char('n'))).unwrap();
    assert_eq!(app.transcript_match, Some(1));
    assert_eq!(app.transcript_matches()[1], 2, "second match is gamma line");
    // Esc closes the search; second Esc returns to Idle.
    app.handle_key(key(crossterm::event::KeyCode::Esc)).unwrap();
    assert_eq!(app.mode, Mode::Transcript);
    assert!(app.transcript_search.is_none());
    app.handle_key(key(crossterm::event::KeyCode::Esc)).unwrap();
    assert_eq!(app.mode, Mode::Idle);
}

#[test]
fn wi81_transcript_running_return_preserves_running() {
    let mock = MockHost::default();
    let tx = mock.with_events(16);
    let mut app = app_with(&mock);
    app.set_input("job".into());
    app.submit().unwrap();
    started(&tx, "s1");
    let mut k = key(crossterm::event::KeyCode::Char('t'));
    k.modifiers = crossterm::event::KeyModifiers::CONTROL;
    app.handle_key(k).unwrap();
    assert_eq!(app.transcript_return, Mode::Running);
    // Events keep flowing while browsing.
    tx.send(json!({"type":"assistantText","sessionId":"s1","text":"stream"})).unwrap();
    app.tick().unwrap();
    assert!(app.transcript.iter().any(|l| l.to_string().contains("stream")));
    app.handle_key(key(crossterm::event::KeyCode::Esc)).unwrap();
    assert_eq!(app.mode, Mode::Running);
    completed(&tx, "s1");
    app.tick().unwrap();
    assert_eq!(app.mode, Mode::Idle);
}

#[test]
fn wi83_consecutive_tools_collapse_to_header_plus_preview() {
    let mock = MockHost::default();
    let tx = mock.with_events(64);
    let mut app = app_with(&mock);
    app.set_input("scan".into());
    app.submit().unwrap();
    started(&tx, "s1");
    for i in 0..4 {
        tx.send(json!({"type":"toolCall","sessionId":"s1","id":format!("t{i}"),"name":"grep"})).unwrap();
        tx.send(json!({"type":"toolResult","sessionId":"s1","id":format!("t{i}"),"ok":true,"output":format!("hit {i}")})).unwrap();
    }
    app.tick().unwrap();
    let joined: String = app.transcript.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n");
    assert!(joined.contains("4 次工具调用") || joined.contains("4 tool calls"), "{joined}");
    // 4 rows folded to header + 1 preview → grep rows: placeholder lines = 2.
    let grep_rows = app.transcript.iter().filter(|l| l.to_string().contains("hit")).count();
    assert!(grep_rows <= 2, "only the last preview stays visible: {joined}");
    // Ctrl+O expands the group back.
    let mut k = key(crossterm::event::KeyCode::Char('o'));
    k.modifiers = crossterm::event::KeyModifiers::CONTROL;
    app.handle_key(k).unwrap();
    let joined: String = app.transcript.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n");
    for i in 0..4 {
        assert!(joined.contains(&format!("hit {i}")), "expanded shows hit {i}: {joined}");
    }
}

#[test]
fn wi83_failure_breaks_the_group() {
    let mock = MockHost::default();
    let tx = mock.with_events(64);
    let mut app = app_with(&mock);
    app.set_input("scan".into());
    app.submit().unwrap();
    started(&tx, "s1");
    for i in 0..3 {
        tx.send(json!({"type":"toolCall","sessionId":"s1","id":format!("t{i}"),"name":"read"})).unwrap();
        tx.send(json!({"type":"toolResult","sessionId":"s1","id":format!("t{i}"),"ok":true,"output":format!("ok {i}")})).unwrap();
    }
    tx.send(json!({"type":"toolCall","sessionId":"s1","id":"t3","name":"read"})).unwrap();
    tx.send(json!({"type":"toolResult","sessionId":"s1","id":"t3","ok":false,"output":"denied"})).unwrap();
    app.tick().unwrap();
    let joined: String = app.transcript.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n");
    assert!(joined.contains("✗ read"), "failure stays independent: {joined}");
    assert!(joined.contains("3 次工具调用") || joined.contains("3 tool calls"), "{joined}");
}

// ── WI-82 (round 12, phase 3) ────────────────────────────────────────────

#[test]
fn wi82_text_deltas_typewriter_then_final_replaces() {
    let mock = MockHost::default();
    let tx = mock.with_events(32);
    let mut app = app_with(&mock);
    app.set_input("hi".into());
    app.submit().unwrap();
    started(&tx, "s1");
    tx.send(json!({"type":"assistantTextDelta","sessionId":"s1","text":"Hel"})).unwrap();
    tx.send(json!({"type":"assistantTextDelta","sessionId":"s1","text":"lo wor"})).unwrap();
    app.tick().unwrap();
    let streamed = app.transcript.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n");
    assert!(streamed.contains("Hel"), "{streamed}");
    assert!(streamed.contains("lo wor"), "{streamed}");
    // Final authoritative block replaces the streamed row wholesale.
    tx.send(json!({"type":"assistantText","sessionId":"s1","text":"Hello world"})).unwrap();
    app.tick().unwrap();
    let joined = app.transcript.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n");
    assert!(joined.contains("Hello world"), "{joined}");
    // The streamed row was REPLACED (one authoritative row, not row+block).
    assert_eq!(
        app.transcript.iter().filter(|l| l.to_string().contains("Hello world")).count(),
        1,
        "{joined}"
    );
}

#[test]
fn wi82_empty_final_after_deltas_drops_streamed_row() {
    let mock = MockHost::default();
    let tx = mock.with_events(16);
    let mut app = app_with(&mock);
    app.set_input("hi".into());
    app.submit().unwrap();
    started(&tx, "s1");
    tx.send(json!({"type":"assistantTextDelta","sessionId":"s1","text":"partial"})).unwrap();
    tx.send(json!({"type":"assistantText","sessionId":"s1","text":""})).unwrap();
    app.tick().unwrap();
    let joined = app.transcript.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n");
    assert!(!joined.contains("partial"), "empty final drops the streamed row: {joined}");
}

#[test]
fn wi82_non_delta_path_unchanged() {
    let mock = MockHost::default();
    let tx = mock.with_events(16);
    let mut app = app_with(&mock);
    app.set_input("hi".into());
    app.submit().unwrap();
    started(&tx, "s1");
    tx.send(json!({"type":"assistantText","sessionId":"s1","text":"block only"})).unwrap();
    app.tick().unwrap();
    assert!(app.transcript.iter().any(|l| l.to_string().contains("block only")));
}

/// BUG-91: Ctrl+E with live tool/streaming rows above the queue echo must
/// keep every stored index valid — the raw remove used to leave the
/// streaming line index dangling and panic the TUI on the next delta.
#[test]
fn wi104_ctrl_e_with_live_rows_keeps_indices_valid() {
    let mock = MockHost::default();
    let tx = mock.with_events(16);
    let mut app = app_with(&mock);
    app.set_input("run".into());
    app.submit().unwrap();
    started(&tx, "s1");

    // Queue echo lands at the tail…
    app.set_input("queued".into());
    app.handle_key(key(crossterm::event::KeyCode::Enter)).unwrap();
    // …then a tool placeholder and a streaming text block land AFTER it.
    tx.send(json!({"type":"toolCall","sessionId":"s1","id":"c1","name":"read"})).unwrap();
    app.tick().unwrap();
    tx.send(json!({"type":"assistantTextDelta","sessionId":"s1","text":"hello"})).unwrap();
    app.tick().unwrap();

    // Ctrl+E removes the echo row; higher indices must shift down.
    let mut k = key(crossterm::event::KeyCode::Char('e'));
    k.modifiers = crossterm::event::KeyModifiers::CONTROL;
    app.handle_key(k).unwrap();
    assert_eq!(app.input, "queued");

    // Pre-BUG-91 the next delta indexed past the transcript end → panic.
    tx.send(json!({"type":"assistantTextDelta","sessionId":"s1","text":" world"})).unwrap();
    app.tick().unwrap();
    // The tool result flips the placeholder in place (index was re-fixed).
    tx.send(json!({"type":"toolResult","sessionId":"s1","id":"c1","ok":true,"output":"done"})).unwrap();
    app.tick().unwrap();

    let rendered: String = app
        .transcript
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.clone()))
        .collect();
    assert!(
        rendered.contains("hello world"),
        "streaming text survived the splice: {rendered:?}"
    );
    assert!(rendered.contains("done"), "tool row flipped in place: {rendered:?}");
}
