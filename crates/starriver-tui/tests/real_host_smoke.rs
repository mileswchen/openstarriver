//! Real-host smoke tests (not run by default):
//! requires local config/DB, makes no model calls, and only verifies real HostRuntime wiring.
//! Run: cargo test -p starriver-tui --test real_host_smoke -- --ignored

use std::path::PathBuf;

use std::time::Duration;

use starriver_tui::app::{AppState, Mode};
use starriver_tui::host::{Host, HostApi};
use starriver_tui::i18n::Lang;

/// Generate a minimal config with empty engines and a DB in a temporary directory.
/// Returns (temp dir, config path, DB path) — the DB path lets tests seed session data directly.
fn temp_config() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("smoke.db");
    let config = format!(
        "version: 1\nmode: offline\ninference: {{ engines: [] }}\nagent: {{}}\nserver: {{ http: {{ enabled: false, bind: \"127.0.0.1:0\" }} }}\nsession: {{ db_path: \"{}\" }}\n",
        db.display()
    );
    let path = dir.path().join("config.toml");
    std::fs::write(&path, config).expect("write config");
    (dir, path, db)
}

#[test]
#[ignore = "需要本地文件系统写临时 DB；不接入模型网络"]
fn real_host_boots_and_execute_fails_without_engine() {
    let (_dir, config, _db) = temp_config();
    let host = Host::new(Some(config.to_string_lossy().into_owned())).expect("host boots");

    // Empty DB list is empty
    assert!(host.list_workspaces().is_empty());
    assert!(host.list_sessions(None).is_empty());

    // Execution with no engine config must fail and not panic
    let mut app = AppState::with_lang(Box::new(host), Lang::Zh).expect("app state");
    app.input = "hi".into();
    app.submit().expect("submit handled");
    // The no-engine failure event is async; poll for the terminal state
    for _ in 0..50 {
        app.tick().expect("tick handled");
        if app.mode != Mode::Running {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(app.mode, Mode::Idle);
    assert!(
        app.status.contains("失败"),
        "unexpected status: {}",
        app.status
    );
    app.shutdown(); // verify the exit path does not panic (previously crashed due to runtime self-drop)
}

/// On the real host verify /compact /usage /export (needs a real DB-seeded session) and
/// /init /diff (needs a real filesystem and git repo). No model calls.
#[test]
#[ignore = "需要临时 DB、临时 git 仓库；不接入模型网络"]
fn real_host_slash_commands_against_temp_db_and_repo() {
    let (dir, config, db) = temp_config();

    // Seed a session directly in the DB. Anchors (first user + last 2 user) take 3,
    // 16 alternating messages → 13 non-anchor > keep_recent 10, so /compact should really drop some.
    use starriver_kernel::config::SessionConfig;
    use starriver_kernel::types::Message;
    use starriver_session::legacy::SessionStore;

    let cfg = SessionConfig {
        db_path: db.display().to_string(),
        max_sessions: 100,
        checkpoint_interval_secs: 30,
        wal_checkpoint_interval_secs: 300,
        write_retry_max: 3,
        write_retry_base_ms: 10,
        session_retention_days: 30,
    };
    let rt = tokio::runtime::Runtime::new().expect("seed runtime");
    let store = rt.block_on(SessionStore::open(&cfg)).expect("open store");
    rt.block_on(store.create_session("s1", "deepseek/deepseek-v4-flash", "seed", "Starriver Code"))
        .expect("create session");
    let owned: Vec<(usize, Message)> = (0..16)
        .map(|i| {
            let m = if i % 2 == 0 {
                Message::user(&format!("第 {i} 条问题"))
            } else {
                Message::assistant(&format!("第 {i} 条回复"))
            };
            (i, m)
        })
        .collect();
    let refs: Vec<(usize, &Message)> = owned.iter().map(|(i, m)| (*i, m)).collect();
    rt.block_on(store.save_messages_batch("s1", &refs))
        .expect("seed messages");
    drop(store); // release connections to avoid lock contention with the host write path

    let host = Host::new(Some(config.to_string_lossy().into_owned())).expect("host boots");

    // /usage: real token estimate and message count
    let (tokens, count) = host.session_usage("s1".into()).expect("usage");
    assert_eq!(count, 16, "消息条数应等于种子数");
    assert!(tokens > 0, "token 估算应为正");

    // /export: write a Markdown file to the working directory
    let cwd = dir.path().join("repo");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let exp = host
        .export_session("s1".into(), cwd.to_string_lossy().into_owned())
        .expect("export");
    assert!(exp.exists(), "导出文件应存在");
    let md = std::fs::read_to_string(&exp).expect("read export");
    assert!(
        md.contains("## user") && md.contains("## assistant"),
        "md: {md}"
    );

    // /compact: real compaction — non-anchor 13 > keep 10, should drop messages and lower tokens
    let (before, after) = host.compact_session("s1".into()).expect("compact");
    assert!(before > after, "compact 应减少 token：{before} → {after}");
    assert!(after > 0);
    let (_, count2) = host
        .session_usage("s1".into())
        .expect("usage after compact");
    assert!(count2 < 16, "compact 应丢消息：{count2}");
    assert!(count2 >= 10, "至少保留最近 10 条：{count2}");

    // /init: generate AGENTS.md; a second call does not overwrite
    let p = host
        .init_project_agents(cwd.to_string_lossy().into_owned())
        .expect("init");
    assert!(p.exists() && p.ends_with("AGENTS.md"));
    std::fs::write(&p, "custom").expect("overwrite agents");
    let p2 = host
        .init_project_agents(cwd.to_string_lossy().into_owned())
        .expect("init again");
    assert_eq!(std::fs::read_to_string(&p2).expect("read"), "custom");

    // /diff: git repo + untracked file → non-empty diff
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(&cwd)
        .args(["init", "-q"])
        .output()
        .expect("git init");
    assert!(out.status.success(), "git init failed");
    std::fs::write(cwd.join("a.txt"), "hello").expect("write file");
    let diff = host
        .workspace_diff(cwd.to_string_lossy().into_owned())
        .expect("diff");
    assert!(diff.contains("a.txt"), "diff 应提到 a.txt：{diff}");

    host.shutdown().expect("shutdown");
}

/// On the real host verify the full /backtrack path: seed 3 user/assistant turns → backtrack to the 2nd
/// user → fork a new thread → assert the new session history contains only turn 1 and input prefills Q2.
#[test]
#[ignore = "需要临时 DB；不接入模型网络"]
fn real_host_fork_backtracks_to_user_message() {
    use starriver_kernel::config::SessionConfig;
    use starriver_kernel::types::Message;
    use starriver_session::legacy::SessionStore;
    use starriver_tui::app::{Mode, PickerKind};

    let (_dir, config, db) = temp_config();
    let cfg = SessionConfig {
        db_path: db.display().to_string(),
        max_sessions: 100,
        checkpoint_interval_secs: 30,
        wal_checkpoint_interval_secs: 300,
        write_retry_max: 3,
        write_retry_base_ms: 10,
        session_retention_days: 30,
    };
    let rt = tokio::runtime::Runtime::new().expect("seed runtime");
    let store = rt.block_on(SessionStore::open(&cfg)).expect("open store");
    rt.block_on(store.create_session(
        "s1",
        "deepseek/deepseek-v4-flash",
        "种子任务",
        "Starriver Code",
    ))
    .expect("create session");
    let owned: Vec<(usize, Message)> = (0..6)
        .map(|i| {
            let m = if i % 2 == 0 {
                Message::user(&format!("第{}问", i / 2 + 1))
            } else {
                Message::assistant(&format!("第{}答", i / 2 + 1))
            };
            (i, m)
        })
        .collect();
    let refs: Vec<(usize, &Message)> = owned.iter().map(|(i, m)| (*i, m)).collect();
    rt.block_on(store.save_messages_batch("s1", &refs))
        .expect("seed messages");
    drop(store);

    let host = Host::new(Some(config.to_string_lossy().into_owned())).expect("host boots");
    let mut app = AppState::with_lang(Box::new(host), Lang::Zh).expect("app state");
    app.resume_session_id = Some("s1".into());

    // /backtrack → the message picker lists only 3 user messages
    app.input = "/backtrack".into();
    app.submit().expect("backtrack");
    assert_eq!(app.mode, Mode::Picker(PickerKind::Messages));
    assert_eq!(app.message_timeline.len(), 3, "user 消息应为 3 条");

    // Select the 2nd user → real host fork
    app.selected = 1;
    app.pick_select().expect("fork");
    assert_eq!(app.mode, Mode::Idle);
    let new_id = app.resume_session_id.clone().expect("forked session");
    assert_ne!(new_id, "s1", "应切换到新线程");
    assert_eq!(app.input, "第2问", "输入应预填第 2 问原文");

    // The new thread history contains only turn 1 (u1/a1), not turns 2/3
    let joined: String = app.transcript.iter().map(|l| l.to_string()).collect();
    assert!(
        joined.contains("第1问") && joined.contains("第1答"),
        "joined: {joined}"
    );
    assert!(
        !joined.contains("第2答") && !joined.contains("第3问"),
        "fork 复制了多余消息: {joined}"
    );

    // DB-side check: fork session parent_id = s1, timeline = [u1, a1]
    let host2 = Host::new(Some(config.to_string_lossy().into_owned())).expect("verify host");
    let all = host2.list_sessions(None);
    let fork = all.iter().find(|s| s.id == new_id).expect("fork in list");
    assert_eq!(fork.parent_id.as_deref(), Some("s1"));
    let tl = host2.get_session_timeline(new_id.clone());
    let roles: Vec<&str> = tl.iter().map(|m| m.role.as_str()).collect();
    let contents: Vec<&str> = tl.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(roles, vec!["user", "assistant"]);
    assert_eq!(contents, vec!["第1问", "第1答"]);

    app.shutdown();
}

/// On the real host verify /delete's second-confirmation delete: seed → /delete → confirm →
/// assert list_sessions excludes it and resume clears.
#[test]
#[ignore = "需要临时 DB；不接入模型网络"]
fn real_host_delete_session() {
    use starriver_kernel::config::SessionConfig;
    use starriver_kernel::types::Message;
    use starriver_session::legacy::SessionStore;

    let (_dir, config, db) = temp_config();
    let cfg = SessionConfig {
        db_path: db.display().to_string(),
        max_sessions: 100,
        checkpoint_interval_secs: 30,
        wal_checkpoint_interval_secs: 300,
        write_retry_max: 3,
        write_retry_base_ms: 10,
        session_retention_days: 30,
    };
    let rt = tokio::runtime::Runtime::new().expect("seed runtime");
    let store = rt.block_on(SessionStore::open(&cfg)).expect("open store");
    rt.block_on(store.create_session("s1", "m", "任务", "Starriver Code"))
        .expect("create");
    let u1 = Message::user("你好");
    rt.block_on(store.save_messages_batch("s1", &[(0usize, &u1)]))
        .expect("seed");
    drop(store);

    let host = Host::new(Some(config.to_string_lossy().into_owned())).expect("host boots");
    let mut app = AppState::with_lang(Box::new(host), Lang::Zh).expect("app state");
    app.resume_session_id = Some("s1".into());

    app.input = "/delete".into();
    app.submit().expect("delete");
    assert_eq!(app.mode, Mode::Approval, "应先进入二次确认浮层");
    app.approve(true).expect("confirm");
    assert_eq!(app.mode, Mode::Idle);
    assert_eq!(app.resume_session_id, None, "删除后应清空当前会话");

    let host2 = Host::new(Some(config.to_string_lossy().into_owned())).expect("verify host");
    let all = host2.list_sessions(None);
    assert!(!all.iter().any(|s| s.id == "s1"), "list 不应含已删会话");

    app.shutdown();
}

/// On the real host verify the full /name /archive /unarchive path: seed → /name sets title override
/// → /archive moves out of the default list into the archive → /unarchive picker restores back →
/// host API with None clears the override back to the derived title.
#[test]
#[ignore = "需要临时 DB；不接入模型网络"]
fn real_host_rename_and_archive() {
    use starriver_kernel::config::SessionConfig;
    use starriver_session::legacy::SessionStore;
    use starriver_tui::app::PickerKind;

    let (_dir, config, db) = temp_config();
    let cfg = SessionConfig {
        db_path: db.display().to_string(),
        max_sessions: 100,
        checkpoint_interval_secs: 30,
        wal_checkpoint_interval_secs: 300,
        write_retry_max: 3,
        write_retry_base_ms: 10,
        session_retention_days: 30,
    };
    let rt = tokio::runtime::Runtime::new().expect("seed runtime");
    let store = rt.block_on(SessionStore::open(&cfg)).expect("open store");
    rt.block_on(store.create_session("s1", "m", "种子任务", "Starriver Code"))
        .expect("create");
    drop(store);

    let verify_title = |expected: &str| {
        let h = Host::new(Some(config.to_string_lossy().into_owned())).expect("verify host");
        let s = h
            .list_sessions(None)
            .into_iter()
            .find(|s| s.id == "s1")
            .expect("s1");
        assert_eq!(s.title, expected, "标题应为 {expected}");
    };
    let verify_in_default = |present: bool| {
        let h = Host::new(Some(config.to_string_lossy().into_owned())).expect("verify host");
        let in_default = h.list_sessions(None).iter().any(|s| s.id == "s1");
        assert_eq!(in_default, present, "默认列表应{present} s1");
    };

    let host = Host::new(Some(config.to_string_lossy().into_owned())).expect("host boots");
    let mut app = AppState::with_lang(Box::new(host), Lang::Zh).expect("app state");
    app.resume_session_id = Some("s1".into());

    // /name sets the title override → default list title updates
    app.input = "/name 我的命名".into();
    app.submit().expect("name");
    assert!(app.status.contains("已重命名"), "status: {}", app.status);
    verify_title("我的命名");

    // /archive → hidden from the default list, visible in the archive list, local resume clears
    app.input = "/archive".into();
    app.submit().expect("archive");
    assert!(app.status.contains("已归档"), "status: {}", app.status);
    assert_eq!(app.resume_session_id, None, "归档后应清空当前会话");
    verify_in_default(false);
    {
        let h = Host::new(Some(config.to_string_lossy().into_owned())).expect("verify host");
        assert!(
            h.list_archived_sessions().iter().any(|s| s.id == "s1"),
            "归档列表应含 s1"
        );
    }

    // /unarchive → archive picker → pick restores → back to the default list
    app.input = "/unarchive".into();
    app.submit().expect("unarchive");
    assert_eq!(app.mode, Mode::Picker(PickerKind::Archived));
    assert_eq!(app.archived_sessions.len(), 1, "选择器应列 1 条归档");
    app.pick_select().expect("restore");
    assert!(app.status.contains("已恢复会话"), "status: {}", app.status);
    assert_eq!(app.mode, Mode::Idle, "归档列表清空后回到 Idle");
    verify_in_default(true);
    {
        let h = Host::new(Some(config.to_string_lossy().into_owned())).expect("verify host");
        assert!(h.list_archived_sessions().is_empty(), "归档列表应清空");
    }

    // host API with None clears the override → back to the derived title (task text preserved)
    {
        let h = Host::new(Some(config.to_string_lossy().into_owned())).expect("clear host");
        h.rename_session("s1".into(), None).expect("clear override");
    }
    verify_title("种子任务");

    app.shutdown();
}
