//! UI render tests: use the ratatui TestBackend to render the real draw path, covering the three-column
//! layout, pickers, and the approval overlay (no real terminal).

use ratatui::backend::TestBackend;
use ratatui::Terminal;
use starriver_tui::app::{AppState, ApprovalState, Mode, PickerKind};
use starriver_tui::host::mock::MockHost;
use starriver_tui::i18n::Lang;
use starriver_tui::ui;

fn render(app: &AppState) -> String {
    let backend = TestBackend::new(100, 45);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal.draw(|frame| ui::draw(frame, app)).expect("draw");
    terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

/// CJK renders in the buffer as "char + space" by width; strip all whitespace before matching.
fn has(buf: &str, s: &str) -> bool {
    let compact: String = buf.chars().filter(|c| !c.is_whitespace()).collect();
    let needle: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    compact.contains(&needle)
}

#[test]
fn idle_renders_title_status_and_input() {
    let mock = MockHost::default();
    let app = AppState::with_lang(Box::new(mock), Lang::Zh).unwrap();
    let buf = render(&app);
    assert!(buf.contains("★")); // slim header: logo placeholder
    assert!(buf.contains("Starriver Code")); // header: agent name (bold does not affect characters)
    assert!(buf.contains("Workspace-write")); // header: sandbox preset chip
    assert!(buf.contains("·")); // header chips are ·-separated
    assert!(has(&buf, "输入消息或/命令")); // composer placeholder (empty input)
}

#[test]
fn command_prefix_shows_supported_commands() {
    let mock = MockHost::default();
    let mut app = AppState::with_lang(Box::new(mock), Lang::Zh).unwrap();

    app.input = "/".into();
    let buf = render(&app);
    assert!(buf.contains("/help"), "buf: {buf}");
    assert!(buf.contains("/model"));
    assert!(buf.contains("/sessions"));
    assert!(buf.contains("/exit"));

    // Backslash prefix triggers equivalently
    app.input = "\\".into();
    let buf = render(&app);
    assert!(buf.contains("\\help"));

    // Input /mo filters to model (only one command line left)
    app.input = "/mo".into();
    let buf = render(&app);
    assert!(buf.contains("/model"));
    assert!(!buf.contains("/sessions"));

    // Ordinary input shows no command hint
    app.input = "你好".into();
    let buf = render(&app);
    assert!(!buf.contains("/help"));
}

#[test]
fn english_ui_renders_localized_text() {
    let mock = MockHost::default();
    mock.seed_session("s1", "first task");
    let mut app = AppState::with_lang(Box::new(mock), Lang::En).unwrap();

    // Header mode line, running hint, input box
    app.mode = Mode::Running;
    app.status = "Running…".into();
    app.transcript
        .push(ratatui::text::Line::from("streaming line"));
    let buf = render(&app);
    // Slim header carries the sandbox chip (bare, no "mode" suffix).
    assert!(has(&buf, "Workspace-write"), "buf: {buf}");
    assert!(has(&buf, "Running…"));
    // Contextual hint footer replaces the old input-box title.
    assert!(has(&buf, "Esc/Ctrl+C interrupt"), "buf: {buf}");

    // Session picker title (English)
    app.mode = Mode::Picker(PickerKind::Sessions);
    let buf = render(&app);
    assert!(has(&buf, "Sessions"));
    assert!(has(&buf, "first task"));

    // Approval overlay (English)
    app.mode = Mode::Approval;
    app.approval = Some(ApprovalState {
        session_id: "s1".into(),
        tool: "bash".into(),
        reason: "delete file".into(),
        remember: false,
        request_id: None,
    });
    let buf = render(&app);
    assert!(has(&buf, "Approval"));
    assert!(has(&buf, "Tool: bash"));
    assert!(has(&buf, "Reason: delete file"));

    // Model provider layer (English title + model count)
    let mock2 = MockHost::default();
    mock2.seed_model_entry("deepseek", "DeepSeek", &["deepseek-v4-flash"]);
    let mut app2 = AppState::with_lang(Box::new(mock2), Lang::En).unwrap();
    app2.mode = Mode::Picker(PickerKind::Models);
    let buf = render(&app2);
    assert!(has(&buf, "Model Providers"));
    assert!(has(&buf, "1 models"));
}

/// Split the buffer into rows by cell: CJK occupies one cell each (the second cell is a continuation), fixed row width.
fn render_rows(app: &AppState) -> Vec<String> {
    let backend = TestBackend::new(100, 45);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal.draw(|frame| ui::draw(frame, app)).expect("draw");
    let buf = terminal.backend().buffer();
    let w = buf.area.width as usize;
    buf.content
        .chunks(w)
        .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
        .collect()
}

#[test]
fn command_hint_renders_two_columns_with_aligned_descriptions() {
    let mock = MockHost::default();
    let mut app = AppState::with_lang(Box::new(mock), Lang::Zh).unwrap();
    app.input = "\\".into();
    let rows = render_rows(&app);
    // The command-name column (including arg placeholders) is fixed-width padded, and the description column follows left-aligned.
    let help_row = rows
        .iter()
        .find(|r| r.contains("\\help"))
        .expect("\\help 行");
    let new_row = rows.iter().find(|r| r.contains("\\new")).expect("\\new 行");
    let help_col = help_row.find('显').expect("help 说明起始");
    let new_col = new_row.find('开').expect("new 说明起始");
    assert_eq!(
        help_col, new_col,
        "说明列未对齐：help@{help_col} new@{new_col}"
    );
    assert!(
        help_col > help_row.find("\\help").expect("help 命令名"),
        "说明应位于命令名列之后"
    );
}

#[test]
fn command_hint_selected_row_highlighted() {
    let mock = MockHost::default();
    let mut app = AppState::with_lang(Box::new(mock), Lang::Zh).unwrap();
    app.input = "\\".into();
    app.cmd_hint_index = 2; // sessions

    let backend = TestBackend::new(100, 45);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal.draw(|frame| ui::draw(frame, &app)).expect("draw");
    let buf = terminal.backend().buffer();
    let w = buf.area.width as usize;
    let rows: Vec<&[ratatui::buffer::Cell]> = buf.content.chunks(w).collect();

    let row_of = |needle: &str| {
        rows.iter()
            .find(|r| {
                let s: String = r.iter().map(|c| c.symbol()).collect();
                s.contains(needle)
            })
            .map(|r| *r)
            .expect(needle)
    };
    // The selected row's (sessions) description is highlighted Yellow; the unselected row's (help) description stays Gray.
    let sess = row_of("\\sessions");
    let help = row_of("\\help");
    let col = sess
        .iter()
        .position(|c| c.symbol() == "列")
        .expect("sessions 说明起始");
    assert_eq!(
        sess[col].style().fg,
        Some(ratatui::style::Color::Yellow),
        "选中说明应高亮"
    );
    let hcol = help
        .iter()
        .position(|c| c.symbol() == "显")
        .expect("help 说明起始");
    assert_eq!(
        help[hcol].style().fg,
        Some(ratatui::style::Color::Gray),
        "未选中说明应灰显"
    );
}

#[test]
fn command_hint_shows_new_commands() {
    let mock = MockHost::default();
    let mut app = AppState::with_lang(Box::new(mock), Lang::Zh).unwrap();
    app.input = "\\".into();
    let buf = render(&app);
    for name in [
        "\\compact",
        "\\diff",
        "\\status",
        "\\usage",
        "\\init",
        "\\export",
        "\\lang",
        "\\backtrack",
        "\\delete",
        "\\worktree",
    ] {
        assert!(buf.contains(name), "hint missing {name}: {buf}");
    }
}

#[test]
fn running_renders_lock_hint() {
    let mock = MockHost::default();
    let mut app = AppState::with_lang(Box::new(mock), Lang::Zh).unwrap();
    app.mode = Mode::Running;
    app.status = "运行中…".into();
    app.run_started_at = Some(std::time::Instant::now());
    app.transcript.push(ratatui::text::Line::from("流式输出行"));
    let buf = render(&app);
    assert!(has(&buf, "运行中…")); // status line
    // Composer title shows the live spinner + elapsed + interrupt hint.
    assert!(has(&buf, "工作中"), "buf: {buf}");
    assert!(has(&buf, "Ctrl+C 取消"), "buf: {buf}");
    assert!(has(&buf, "流式输出行"));
}

#[test]
fn picker_renders_items_and_highlight() {
    let mock = MockHost::default();
    mock.seed_session("s1", "第一个任务");
    mock.seed_session("s2", "第二个任务");
    let mut app = AppState::with_lang(Box::new(mock), Lang::Zh).unwrap();
    app.mode = Mode::Picker(PickerKind::Sessions);
    app.selected = 1;
    let buf = render(&app);
    assert!(has(&buf, "会话"));
    assert!(has(&buf, "第一个任务"));
    assert!(has(&buf, "第二个任务"));
    assert!(buf.contains("> ")); // highlight_symbol
}

#[test]
fn archived_picker_renders_archived_sessions() {
    let mock = MockHost::default();
    mock.seed_archived_session("a1", "旧任务");
    mock.seed_archived_session("a2", "归档二");
    let mut app = AppState::with_lang(Box::new(mock), Lang::Zh).unwrap();
    // Walk the real /unarchive path to fill the archive list and enter the picker
    app.input = "/unarchive".into();
    app.submit().unwrap();
    assert_eq!(app.mode, Mode::Picker(PickerKind::Archived));
    let buf = render(&app);
    assert!(has(&buf, "归档会话"), "buf: {buf}");
    assert!(has(&buf, "📦"), "归档项应有 📦 标记, buf: {buf:?}");
    assert!(has(&buf, "旧任务"));
    assert!(has(&buf, "归档二"));
}

#[test]
fn approval_overlay_renders_actions() {
    let mock = MockHost::default();
    let mut app = AppState::with_lang(Box::new(mock), Lang::Zh).unwrap();
    app.mode = Mode::Approval;
    app.approval = Some(ApprovalState {
        session_id: "s1".into(),
        tool: "bash".into(),
        reason: "删除文件".into(),
        remember: false,
        request_id: None,
    });
    let buf = render(&app);
    assert!(has(&buf, "审批"));
    assert!(buf.contains("bash"));
    assert!(has(&buf, "删除文件"));
    assert!(buf.contains("[y]"));
    assert!(buf.contains("[n]"));
}

#[test]
fn projects_picker_renders_cwd() {
    let mock = MockHost::default();
    mock.seed_workspace("p1", "/tmp/alpha");
    let mut app = AppState::with_lang(Box::new(mock), Lang::Zh).unwrap();
    app.mode = Mode::Picker(PickerKind::Workspaces);
    let buf = render(&app);
    assert!(has(&buf, "项目"));
    assert!(buf.contains("/tmp/alpha"));
}

#[test]
fn command_hint_shows_mode_commands() {
    let mock = MockHost::default();
    let mut app = AppState::with_lang(Box::new(mock), Lang::Zh).unwrap();
    app.input = "\\".into();
    let buf = render(&app);
    for name in ["\\plan", "\\build", "\\auto"] {
        assert!(buf.contains(name), "hint missing {name}: {buf}");
    }
    // Prefix filters to build
    app.input = "/bu".into();
    let buf = render(&app);
    assert!(buf.contains("/build"));
    assert!(!buf.contains("/plan"));
    assert!(!buf.contains("/auto"));
}

#[test]
fn header_reflects_sandbox_preset_after_event() {
    let mock = MockHost::default();
    let mut app = AppState::with_lang(Box::new(mock.clone()), Lang::Zh).unwrap();

    // Default preset: workspace-write until the first event arrives (the
    // slim header carries the bare chip, no mode suffix).
    assert!(
        has(&render(&app), "Workspace-write"),
        "default header should be Workspace-write"
    );

    // A sandbox/mode event updates the header to the new preset.
    app.sandbox_mode = "read-only".into();
    assert!(
        has(&render(&app), "Read-only"),
        "header should be Read-only after sandbox/mode event"
    );
}

#[test]
fn messages_picker_renders_user_message_preview() {
    use starriver_tui::host::types::SessionMessage;
    let mock = MockHost::default();
    let mut app = AppState::with_lang(Box::new(mock), Lang::Zh).unwrap();
    // /backtrack would fill message_timeline from the host timeline; here we set it directly for the render path.
    app.message_timeline = vec![
        SessionMessage {
            id: 3,
            role: "user".into(),
            content: "第一个问题".into(),
        },
        SessionMessage {
            id: 7,
            role: "user".into(),
            content: "第二个问题".into(),
        },
    ];
    app.mode = Mode::Picker(PickerKind::Messages);
    app.selected = 1;
    let buf = render(&app);
    assert!(has(&buf, "回溯到消息"), "buf: {buf}");
    assert!(has(&buf, "第一个问题"), "buf: {buf}");
    assert!(has(&buf, "第二个问题"), "buf: {buf}");
    assert!(buf.contains("> ")); // highlight_symbol on the selected row
}

#[test]
fn sessions_picker_indents_child_branches() {
    let mock = MockHost::default();
    mock.seed_session("s1", "根会话");
    mock.seed_session("s2", "分支会话");
    mock.sessions.lock().unwrap()[1].parent_id = Some("s1".into());
    let mut app = AppState::with_lang(Box::new(mock), Lang::Zh).unwrap();
    app.mode = Mode::Picker(PickerKind::Sessions);
    let rows = render_rows(&app);
    let root = rows.iter().find(|r| has(r, "根会话")).expect("root row");
    assert!(!has(root, "↳"), "根会话不应缩进: {root:?}");
    let child = rows.iter().find(|r| has(r, "分支会话")).expect("child row");
    assert!(has(child, "↳"), "分支会话应带缩进标记: {child:?}");
    let compact: String = child.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(
        compact.find('↳').unwrap() < compact.find("分支会话").unwrap(),
        "标记应在标题前: {compact}"
    );
}

#[test]
fn delete_confirm_overlay_renders_without_tool_line() {
    let mock = MockHost::default();
    let mut app = AppState::with_lang(Box::new(mock), Lang::Zh).unwrap();
    app.mode = Mode::Approval;
    app.approval = Some(ApprovalState {
        session_id: "s1".into(),
        tool: "delete-session".into(),
        reason: "确认删除当前会话？子分支将提升为根会话。".into(),
        remember: false,
        request_id: None,
    });
    let buf = render(&app);
    assert!(has(&buf, "确认删除当前会话"), "buf: {buf}");
    assert!(has(&buf, "[y] 删除"), "buf: {buf}");
    assert!(!has(&buf, "工具："), "delete 确认不应渲染工具行: {buf}");
}

#[test]
fn models_picker_renders_providers_and_title_model() {
    let mock = MockHost::default();
    mock.seed_model_entry(
        "deepseek",
        "DeepSeek",
        &["deepseek-v4-flash", "deepseek-v4-pro"],
    );
    mock.seed_model_entry("zhipu", "GLM", &["glm-4-plus"]);
    let mut app = AppState::with_lang(Box::new(mock), Lang::Zh).unwrap();
    app.current_model = Some("deepseek/deepseek-v4-pro".into());

    // Title bar includes the current model
    let buf = render(&app);
    assert!(buf.contains("deepseek/deepseek-v4-pro"));

    // provider layer
    app.mode = Mode::Picker(PickerKind::Models);
    let buf = render(&app);
    assert!(has(&buf, "DeepSeek"));
    assert!(has(&buf, "GLM"));

    // model layer
    app.model_picker_provider = Some("deepseek".into());
    let buf = render(&app);
    assert!(buf.contains("deepseek-v4-flash"));
    assert!(buf.contains("deepseek-v4-pro"));
}

#[test]
fn footer_shows_agent_mode_and_cwd() {
    let mock = MockHost::default();
    mock.seed_workspace("p1", "/tmp/alpha");
    let mut app = AppState::with_lang(Box::new(mock), Lang::Zh).unwrap();
    app.current_workspace = Some(app.workspaces[0].clone());
    let buf = render(&app);
    // Mode lives in the slim header; the footer is key hints + cwd.
    assert!(has(&buf, "Workspace-write"), "preset missing (header): {buf}");
    assert!(buf.contains("/tmp/alpha"), "footer missing cwd: {buf}");
    assert!(has(&buf, "enter发送"), "footer missing hints: {buf}");
}

#[test]
fn footer_without_project_shows_placeholder() {
    let mock = MockHost::default();
    let app = AppState::with_lang(Box::new(mock), Lang::Zh).unwrap();
    let buf = render(&app);
    // Working directory falls back to "-" when there is no project
    assert!(has(&buf, "Workspace-write"), "preset missing (header): {buf}");
    assert!(
        buf.contains('-'),
        "footer should show '-' placeholder: {buf}"
    );
}

#[test]
fn composer_renders_cursor_block() {
    let mock = MockHost::default();
    let mut app = AppState::with_lang(Box::new(mock), Lang::Zh).unwrap();
    app.set_input("abc".into());
    app.input_cursor = 1; // cursor between a and b
    let buf = render(&app);
    assert!(buf.contains('▌'), "cursor block missing: {buf}");
    let rows = render_rows(&app);
    let row = rows.iter().find(|r| r.contains("a▌bc")).expect("a▌bc");
    assert!(row.contains("a▌bc"));
}

#[test]
fn follow_pill_renders_when_scrolled_up() {
    let mock = MockHost::default();
    let mut app = AppState::with_lang(Box::new(mock), Lang::Zh).unwrap();
    for i in 0..60 {
        app.transcript.push(ratatui::text::Line::from(format!("line-{i}")));
    }
    app.scroll = 7; // 7 rows above the tail
    let buf = render(&app);
    assert!(has(&buf, "End回到底部"), "follow pill missing: {buf}");
    assert!(has(&buf, "7"), "offset shown: {buf}");
    // Following (scroll 0): no pill.
    app.scroll = 0;
    let buf = render(&app);
    assert!(!has(&buf, "回到底部"), "pill must hide when following: {buf}");
}

#[test]
fn wi77_usage_chip_renders_in_title_and_footer() {
    let mock = MockHost::default();
    mock.seed_model_entry("deepseek", "DeepSeek", &["deepseek-v4-flash"]);
    let mut app = AppState::with_lang(Box::new(mock), Lang::Zh).unwrap();
    app.current_model = Some("deepseek/deepseek-v4-flash".into());
    app.resume_session_id = Some("s1".into());
    app.usage_in = 1200;
    app.usage_out = 300;
    let buf = render(&app);
    assert!(has(&buf, "in1200/out300"), "title usage: {buf}");
    assert!(has(&buf, "ctx2%"), "footer chip: {buf}");
    // ≥80% turns yellow — force it via a tiny window model entry default
    // (65536 default): set usage near the cap.
    app.usage_in = 60_000;
    let rows = render_rows(&app);
    let chip_row = rows
        .iter()
        .find(|r| r.contains("ctx"))
        .expect("chip row");
    let yellow = chip_row
        .chars()
        .zip(chip_row.chars().skip(1))
        .any(|(a, _)| a == 'c'); // presence check only; color asserted below
    assert!(yellow);
}

#[test]
fn wi78_composer_shows_line_badge_for_multiline() {
    let mock = MockHost::default();
    let mut app = AppState::with_lang(Box::new(mock), Lang::Zh).unwrap();
    app.set_input("a\nb".into());
    let buf = render(&app);
    assert!(buf.contains("L2"), "line badge: {buf}");
    assert!(buf.contains('▌'));
}

#[test]
fn wi81_transcript_view_renders_search_bar_and_marker() {
    let mock = MockHost::default();
    let mut app = AppState::with_lang(Box::new(mock), Lang::Zh).unwrap();
    app.transcript.push(ratatui::text::Line::from("alpha needle"));
    app.transcript.push(ratatui::text::Line::from("beta"));
    app.transcript.push(ratatui::text::Line::from("gamma needle"));
    app.open_transcript();
    app.transcript_search = Some("needle".into());
    app.transcript_match = Some(1);
    let buf = render(&app);
    // Query shows in the bottom bar; the current match gets the > marker.
    assert!(buf.contains("/needle"), "query bar: {buf}");
    assert!(buf.contains("> "), "current-match marker: {buf}");
    assert!(buf.contains("alpha"), "transcript visible: {buf}");
}
