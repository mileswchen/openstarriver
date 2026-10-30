//! HostRuntime runtime integration tests: temporary DB + empty engine config, covering public API happy,
//! boundary, and error paths (no external services).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use futures::StreamExt as _;

use starriver_host::HostRuntime;
use tempfile::TempDir;

/// Process-wide `$HOME` pinning (WI-48 session, test slowness root cause):
/// `test_host`/`host_in` used to run against the developer's REAL
/// `~/.starriver` — DeepSeek route + agent profiles bound to real models —
/// so every execute streamed from the live API (slow, flaky, token spend).
/// Pinning HOME once per process to a neutral tempdir removes the leak
/// without per-test HOME flapping (which corrupts concurrent readers).
static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn pin_home_once() {
    static PIN: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    PIN.get_or_init(|| {
        // Deliberately leaked: the pinned HOME must outlive every test.
        let tmp = tempfile::tempdir().expect("pinned temp home");
        std::env::set_var("HOME", tmp.path());
        std::mem::forget(tmp);
        // macOS keychain round trips (legacy credential cleanup) stall
        // unsigned test binaries for tens of seconds — disable them.
        std::env::set_var("STARRIVER_DISABLE_KEYCHAIN", "1");
    });
}

fn make_config(dir: &Path) -> PathBuf {
    let db = dir.join("test.db");
    let config = format!(
        "version: 1\nmode: offline\ninference: {{ engines: [] }}\nagent: {{}}\nserver: {{ http: {{ enabled: false, bind: \"127.0.0.1:0\" }} }}\nsession: {{ db_path: \"{}\" }}\n",
        db.display()
    );
    let path = dir.join("config.toml");
    std::fs::write(&path, config).expect("write config");
    path
}

/// Read one session event type, polling briefly for the dispose-time
/// persistence write so tests do not race agent teardown.
fn poll_session_events(
    host: &HostRuntime,
    session_id: &str,
    event_type: &str,
) -> Vec<starriver_kernel::json::JsonValue> {
    for _ in 0..100 {
        let events = host.debug_session_log_event_data(session_id.into(), event_type.into());
        if !events.is_empty() {
            return events;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    host.debug_session_log_event_data(session_id.into(), event_type.into())
}

struct TestHost {
    _dir: TempDir,
    host: HostRuntime,
    workdir: PathBuf,
}

fn test_host() -> TestHost {
    let dir = TempDir::new().expect("tempdir");
    let workdir = dir.path().join("workdir");
    std::fs::create_dir(&workdir).expect("create workdir");
    let config = make_config(dir.path());
    // WI-48 session: pin the process HOME (neutral tempdir) so the
    // developer's real `~/.starriver` never leaks into the provider registry.
    pin_home_once();
    let host = HostRuntime::new(Some(config.to_string_lossy().into_owned())).expect("host boots");
    TestHost {
        _dir: dir,
        host,
        workdir,
    }
}

#[test]
fn list_tools_returns_kernel_builtins_with_approval_meta() {
    let t = test_host();
    let tools = t.host.list_tools();
    let names: Vec<String> = tools.iter().map(|tool| tool.name.clone()).collect();
    assert!(names.contains(&"bash".to_string()));
    assert!(names.contains(&"read".to_string()));
    let bash = tools.iter().find(|tool| tool.name == "bash").unwrap();
    let read = tools.iter().find(|tool| tool.name == "read").unwrap();
    assert!(bash.requires_approval);
    assert!(!read.requires_approval);
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted, "tool list must be sorted for a stable UI");
}

#[test]
fn cancel_unknown_session_returns_false_without_panic() {
    let t = test_host();
    assert!(!t.host.cancel("no-such-session".into()));
}

#[test]
fn list_sessions_starts_empty_on_fresh_runtime() {
    let t = test_host();
    assert!(t.host.list_sessions(None).is_empty());
}

#[test]
fn workspace_crud_roundtrip() {
    let t = test_host();
    let path = t.workdir.join("proj");
    std::fs::create_dir_all(&path).unwrap();

    let created = t
        .host
        .create_workspace(path.to_string_lossy().into_owned())
        .unwrap();
    assert_eq!(created.title, "proj");
    assert_eq!(created.session_ids.len(), 0);

    let workspaces = t.host.list_workspaces();
    assert_eq!(workspaces.len(), 1);
    assert_eq!(workspaces[0].id, created.id);

    t.host
        .rename_workspace(created.id.clone(), "renamed".into())
        .unwrap();
    assert_eq!(t.host.list_workspaces()[0].title, "renamed");

    t.host.delete_workspace(created.id).unwrap();
    assert!(t.host.list_workspaces().is_empty());
}

#[test]
fn malformed_config_returns_err() {
    let dir = TempDir::new().expect("tempdir");
    let bad = dir.path().join("bad.toml");
    // from_file deterministically errors on negative retention (core already has unit tests; verify host-layer passthrough)
    std::fs::write(
        &bad,
        "version: 1\nmode: offline\ninference: { engines: [] }\nagent: {}\nserver: { http: { enabled: false, bind: x } }\nsession: { db_path: \"./x.db\", session_retention_days: -1 }\n",
    )
    .expect("write");
    assert!(HostRuntime::new(Some(bad.to_string_lossy().into_owned())).is_err());
}

#[test]
fn workspace_admin_error_paths() {
    let t = test_host();

    // semantics: only existing directories resolve (no mkdir)
    assert!(t
        .host
        .create_workspace("/nonexistent-dir-xyz".into())
        .is_err());
    assert!(t
        .host
        .create_workspace(t.workdir.to_string_lossy().into_owned())
        .is_ok());

    // rename requires a non-empty title and an existing workspace
    let id = t.host.list_workspaces()[0].id.clone();
    assert!(t.host.rename_workspace(id.clone(), "   ".into()).is_err());
    assert!(t
        .host
        .rename_workspace("missing".into(), "x".into())
        .is_err());

    // deleting an unknown workspace is tolerated (host layer swallows)
    t.host.delete_workspace("missing".into()).unwrap();
    t.host.delete_workspace(id).unwrap();
    assert!(t.host.list_workspaces().is_empty());
}

#[test]
fn session_list_and_delete_semantics() {
    let t = test_host();
    assert!(t.host.list_sessions(None).is_empty());
    assert!(t.host.get_session_messages("missing".into()).is_empty());
    t.host.delete_session("missing".into()).unwrap(); // idempotent
    assert!(t
        .host
        .list_sessions(Some("no-such-workspace".into()))
        .is_empty());
}

#[test]
fn agent_named_execution_and_workspace_activation() {
    with_temp_home(|_home| {
        let t = test_host();
        // No residency until a run names an agent.
        assert!(t.host.debug_resident_agent().is_none());

        // An unknown agent fails the run outright instead of silently
        // falling back.
        assert!(t
            .host
            .execute(
                "hi".into(),
                None,
                None,
                Some("No Such Agent".into()),
                None,
                None,
                None,
            )
            .is_err());
        // Naming an agent installs its persona.
        t.host
            .execute(
                "hi".into(),
                None,
                None,
                Some(starriver_host::CODING_AGENT_NAME.into()),
                None,
                None,
                None,
            )
            .ok();
        assert_eq!(
            t.host.debug_resident_agent().as_deref(),
            Some(starriver_host::CODING_AGENT_NAME)
        );
        assert!(t
            .host
            .list_agents()
            .iter()
            .any(|a| a.name == starriver_host::CODING_AGENT_NAME));

        // unknown workspace: tolerated no-op, activation stays cleared
        t.host
            .set_active_workspace_for(starriver_host::CODING_AGENT_NAME, Some("missing-id".into()))
            .unwrap();
        let path = t.workdir.join("proj");
        std::fs::create_dir(&path).unwrap();
        let ws = t
            .host
            .create_workspace(path.to_string_lossy().into_owned())
            .unwrap();
        t.host
            .set_active_workspace_for(starriver_host::CODING_AGENT_NAME, Some(ws.id.clone()))
            .unwrap();
        assert_eq!(
            t.host.read_workspace_instructions(ws.id.clone()),
            None // no instruction files in the temp dir
        );
        t.host
            .set_active_workspace_for(starriver_host::CODING_AGENT_NAME, None)
            .unwrap(); // clear
    });
}

#[test]
fn execute_without_engine_fails_contract_compliant() {
    let t = test_host();

    // No model route: the event stream ends with turnError and must carry
    // session_id (plugin kernel UiEvent contract).
    let mut rx = t
        .host
        .execute("hi".into(), None, None, None, None, None, None)
        .expect("execute accepts");
    let handle = t.host.runtime_handle();
    let events: Vec<serde_json::Value> = handle.block_on(async move {
        let mut out = Vec::new();
        for _ in 0..20 {
            match rx.recv().await {
                Ok(ev) => {
                    let is_end = ev["type"] == "turnError" || ev["type"] == "turnEnd";
                    out.push(ev);
                    if is_end {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        out
    });
    let last = events.last().expect("got terminal event");
    assert_eq!(last["type"], "turnError");
    let sid = last["sessionId"].as_str().expect("session_id present");
    assert!(!sid.is_empty());

    // Preserve the id when session_id is provided
    let mut rx = t
        .host
        .execute(
            "go".into(),
            Some("custom-session-1".into()),
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
    let handle = t.host.runtime_handle();
    let events: Vec<serde_json::Value> = handle.block_on(async move {
        let mut out = Vec::new();
        for _ in 0..20 {
            match rx.recv().await {
                Ok(ev) => {
                    let is_end = ev["type"] == "turnError" || ev["type"] == "turnEnd";
                    out.push(ev);
                    if is_end {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        out
    });
    assert_eq!(events.last().unwrap()["sessionId"], "custom-session-1");
}

#[test]
fn single_input_slot_returns_session_busy_and_releases_after_run() {
    let t = test_host();
    let sid = "busy-slot".to_string();

    // The first execute synchronously occupies the input slot (the guard inserts into the running set).
    let rx = t
        .host
        .execute(
            "first".into(),
            Some(sid.clone()),
            None,
            None,
            None,
            None,
            None,
        )
        .expect("first execute");
    assert!(
        t.host.is_session_running(&sid),
        "slot must be busy right after execute"
    );

    // A second execute on the same session → SESSION_BUSY (serializing concurrent commands from multiple clients).
    let err = t
        .host
        .execute(
            "second".into(),
            Some(sid.clone()),
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap_err();
    assert!(err.to_string().contains("SESSION_BUSY"), "{err}");

    // Wait for the first run to finish (ends with turnError under an empty engine); the slot should be released.
    let handle = t.host.runtime_handle();
    handle.block_on(async move {
        let mut rx = rx;
        for _ in 0..50 {
            match rx.recv().await {
                Ok(ev) => {
                    let is_end = ev["type"] == "turnError" || ev["type"] == "turnEnd";
                    if is_end {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    for _ in 0..100 {
        if !t.host.is_session_running(&sid) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(
        !t.host.is_session_running(&sid),
        "slot must be released after run finishes"
    );

    // After it finishes, the same session can execute again.
    let _rx2 = t
        .host
        .execute(
            "third".into(),
            Some(sid.clone()),
            None,
            None,
            None,
            None,
            None,
        )
        .expect("execute after release");
    assert!(t.host.is_session_running(&sid));
}

#[test]
fn approve_and_cancel_missing_session_are_noop() {
    let t = test_host();
    assert!(!t.host.cancel("missing".into()));
    // No pending approval: approve returns false (does not panic); the
    // protocol layer maps that to APPROVAL_NOT_PENDING.
    assert!(!t.host.approve("missing".into(), true, None));
    assert!(!t.host.approve("missing".into(), false, Some(true)));
}

#[test]
fn shutdown_is_idempotent_and_runtime_survives() {
    let t = test_host();
    let handle = t.host.runtime_handle();
    handle.block_on(t.host.shutdown()).unwrap();
    handle.block_on(t.host.shutdown()).unwrap();
    // runtime is not dropped: subsequent APIs still work (regression: used to crash when dropped inside its own runtime)
    assert!(t.host.list_workspaces().is_empty());
    let w = t
        .host
        .create_workspace(t.workdir.to_string_lossy().into_owned())
        .unwrap();
    assert_eq!(w.title, w.path.rsplit('/').next().unwrap());
}

/// The legacy-store fork e2e was removed with fork's move to the plugin log;
/// its coverage lives in fork_session_seeds_child_from_log and friends.

// -- Sandbox project-root (workspace stamping + .worktrees promotion) --

#[test]
fn execute_stamps_workspace_root_into_session_header() {
    // Isolate HOME: switching to the Coding Agent reads the model routes in
    // the agent config; without isolation it would activate the developer
    // machine's real LLM (e.g. deepseek) and execute would hang on real
    // network calls.
    with_temp_home(|_home| {
        let t = test_host();
        let proj = t.workdir.join("proj");
        std::fs::create_dir(&proj).unwrap();
        let ws = t
            .host
            .create_workspace(proj.to_string_lossy().into_owned())
            .unwrap();
        t.host
            .set_active_workspace_for(starriver_host::CODING_AGENT_NAME, Some(ws.id.clone()))
            .unwrap();

        let mut rx = t
            .host
            .execute(
                "stamp cwd".into(),
                Some("stamped".into()),
                None,
                Some(starriver_host::CODING_AGENT_NAME.into()),
                None,
                None,
                None,
            )
            .unwrap();
        let handle = t.host.runtime_handle();
        handle.block_on(async move {
            for _ in 0..20 {
                match rx.recv().await {
                    Ok(ev) if ev["type"] == "turnError" || ev["type"] == "turnEnd" => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        });

        let expected = std::fs::canonicalize(&proj)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let header = t
            .host
            .debug_session_header("stamped".into())
            .expect("session exists");
        assert_eq!(
            header.cwd.as_deref(),
            Some(expected.as_str()),
            "the workspace root is stamped into the session header"
        );
    });
}

// -- Workspace activation + grouping end-to-end --

/// Workspace activation end-to-end: a known id points the agent's working dir
/// at the directory. SP3: instruction files no longer string-inject at
/// activation (the loop's agent-instructions snapshot owns model context) —
/// the banner read path reads the same discovery chain.
#[test]
fn activate_workspace_points_dir_and_keeps_instructions_banner() {
    with_temp_home(|_home| {
        let t = test_host();
        let proj = t.workdir.join("proj");
        std::fs::create_dir(&proj).unwrap();
        let marker = "STARRIVER-INSTRUCTIONS-MARKER-42";
        std::fs::write(proj.join("AGENTS.md"), format!("{marker}\n")).unwrap();
        // Canonical path is what the registry stores (macOS /var → /private/var).
        let ws = t
            .host
            .create_workspace(proj.to_string_lossy().into_owned())
            .expect("create");
        assert_eq!(
            ws.path,
            std::fs::canonicalize(&proj).unwrap().to_string_lossy()
        );

        t.host
            .set_active_workspace_for(starriver_host::CODING_AGENT_NAME, Some(ws.id.clone()))
            .unwrap();
        // The activation is the durable source: resolve reads back what the
        // registry holds (and only that — no instruction string-injection any
        // more).
        let working_dir = t
            .host
            .resolve_workspace_dir(starriver_host::CODING_AGENT_NAME);
        assert_eq!(
            working_dir.as_deref(),
            Some(ws.path.as_str()),
            "activation points the agent at the registered directory"
        );

        // Banner source reads the instruction chain (AGENTS.md …).
        let banner = t.host.read_workspace_instructions(ws.id.clone()).unwrap();
        assert!(banner.contains(marker));

        t.host
            .set_active_workspace_for(starriver_host::CODING_AGENT_NAME, None)
            .unwrap();
        assert_eq!(
            t.host
                .resolve_workspace_dir(starriver_host::CODING_AGENT_NAME),
            None,
            "clear removes the working dir"
        );

        // Unknown id: tolerated no-op that leaves state consistent.
        t.host
            .set_active_workspace_for(starriver_host::CODING_AGENT_NAME, Some("missing-id".into()))
            .unwrap();
        assert_eq!(
            t.host
                .resolve_workspace_dir(starriver_host::CODING_AGENT_NAME),
            None
        );
    });
}

/// Execution mode binds to the session (scheme): the mode carried by a new
/// session is stamped and locked (write-once), getExecutionMode(sessionId)
/// replays it; the deployment default stays untouched; a mode carried by a
/// continuing session is ignored (locked).
#[test]
fn execution_mode_binds_to_session_and_locks() {
    with_temp_home(|_home| {
        let t = test_host();

        // A new session carrying plan: the event stream reveals the generated id.
        let mut rx = t
            .host
            .execute(
                "plan it".into(),
                None,
                Some("plan".into()),
                None,
                None,
                None,
                None,
            )
            .expect("execute accepts");
        let sid = {
            let handle = t.host.runtime_handle();
            handle
                .block_on(async move {
                    let mut sid = None;
                    for _ in 0..20 {
                        match rx.recv().await {
                            Ok(ev) => {
                                if let Some(s) = ev["sessionId"].as_str() {
                                    sid = Some(s.to_string());
                                }
                                if ev["type"] == "turnError" || ev["type"] == "turnEnd" {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    sid
                })
                .expect("session id observed")
        };

        // The session's locked value = the plan it was created with; the deployment
        // default stays standard.
        assert_eq!(
            t.host.get_execution_mode(Some(sid.clone())),
            "plan",
            "session-locked mode"
        );
        assert_eq!(
            t.host.get_execution_mode(None),
            "standard",
            "deployment default untouched"
        );

        // The first turn's run task releases the single-input slot a beat
        // after turnEnd (the teardown double-check). A fresh session now
        // holds that slot like any continuing one, so wait it out before the
        // follow-up execute — mirroring the UI's stale-busy recovery.
        for _ in 0..500 {
            if !t.host.running_session_ids().iter().any(|id| id == &sid) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        // A continuing session carrying another mode → ignored (the parameter only
        // applies at creation); the locked value is unchanged.
        let mut rx2 = t
            .host
            .execute(
                "continue".into(),
                Some(sid.clone()),
                Some("goal".into()),
                None,
                None,
                None,
                None,
            )
            .expect("execute accepts");
        let handle = t.host.runtime_handle();
        handle.block_on(async move {
            for _ in 0..20 {
                match rx2.recv().await {
                    Ok(ev) if ev["type"] == "turnError" || ev["type"] == "turnEnd" => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        });
        assert_eq!(
            t.host.get_execution_mode(Some(sid.clone())),
            "plan",
            "write-once: later executes cannot re-stamp"
        );

        // Kernel replay: switch the global default away, then reading the session's
        // locked mode re-applies it — the next turn on this session runs read-only.
        t.host
            .set_execution_mode("standard".into())
            .expect("standard");
        assert_eq!(t.host.get_execution_mode(Some(sid.clone())), "plan");
        execute_and_wait(&t, "again", &sid);
        assert_eq!(
            last_sandbox(&t.host, &sid).as_deref(),
            Some("read-only"),
            "locked plan replays onto the kernel on read"
        );
    });
}

/// A fresh-session execute with an unknown mode value is rejected before any
/// session is created (the picker only sends known values; this guards the
/// protocol surface).
#[test]
fn execute_rejects_unknown_execution_mode() {
    with_temp_home(|_| {
        let t = test_host();
        let err = t
            .host
            .execute(
                "hi".into(),
                None,
                Some("bogus".into()),
                None,
                None,
                None,
                None,
            )
            .expect_err("unknown mode must be rejected");
        assert!(err.to_string().contains("unknown execution mode"), "{err}");
    });
}

/// Grouping full chain driven by the real execute path: activate workspace →
/// run a task (session auto-attaches to the active directory) → filter by
/// workspace → archive hides → unarchive restores → deleting the group leaves
/// the session listed as Ungrouped → deleting the session clears it.
#[test]
fn workspace_grouping_e2e_filter_archive_delete_then_ungroup() {
    with_temp_home(|_home| {
        let t = test_host();
        let proj = t.workdir.join("proj");
        std::fs::create_dir(&proj).unwrap();
        let ws = t
            .host
            .create_workspace(proj.to_string_lossy().into_owned())
            .expect("create");
        t.host
            .set_active_workspace_for(starriver_host::CODING_AGENT_NAME, Some(ws.id.clone()))
            .unwrap();

        // No engine: the event stream ends in turnError but the session log and
        // the workspace attachment are still produced — exactly the production
        // auto-attach path.
        let mut rx = t
            .host
            .execute(
                "seed grouped task".into(),
                Some("s-w".into()),
                None,
                Some(starriver_host::CODING_AGENT_NAME.into()),
                None,
                None,
                None,
            )
            .unwrap();
        let handle = t.host.runtime_handle();
        handle.block_on(async move {
            for _ in 0..20 {
                match rx.recv().await {
                    Ok(ev) => {
                        if ev["type"] == "turnError" || ev["type"] == "turnEnd" {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        // A seeded unattached session plays the Ungrouped counterpart.
        t.host
            .seed_test_session("s-u".into(), "orphan task".into())
            .unwrap();

        let all = t.host.list_sessions(None);
        let grouped = all.iter().find(|s| s.id == "s-w").expect("grouped listed");
        assert_eq!(
            grouped.workspace_id.as_deref(),
            Some(ws.id.as_str()),
            "execute attached the session to the active workspace"
        );
        assert!(grouped.title.contains("seed grouped"));
        assert!(
            all.iter()
                .any(|s| s.id == "s-u" && s.workspace_id.is_none()),
            "unattached session stays Ungrouped"
        );

        // Filter by workspace returns only its members; unknown ids return none.
        let only_ws = t.host.list_sessions(Some(ws.id.clone()));
        assert_eq!(
            only_ws.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            vec!["s-w"]
        );
        assert!(t
            .host
            .list_sessions(Some("no-such-workspace".into()))
            .is_empty());

        // Archive hides from the default list but keeps the group.
        t.host.archive_session("s-w".into()).unwrap();
        assert!(!t.host.list_sessions(None).iter().any(|s| s.id == "s-w"));
        assert!(t
            .host
            .list_archived_sessions()
            .iter()
            .any(|s| s.id == "s-w"));
        t.host.unarchive_session("s-w".into()).unwrap();
        assert!(t.host.list_sessions(None).iter().any(|s| s.id == "s-w"));

        // Deleting the group keeps the session on disk; the stale workspace_id
        // stays so the orphaned Coding session remains distinct from Chat
        // conversations (workspace_id NULL) and shows under Coding's Ungrouped.
        t.host.delete_workspace(ws.id.clone()).unwrap();
        assert!(t.host.list_workspaces().iter().all(|w| w.id != ws.id));
        let survivor = t
            .host
            .list_sessions(None)
            .into_iter()
            .find(|s| s.id == "s-w")
            .expect("session survives group deletion");
        assert_eq!(
            survivor.workspace_id,
            Some(ws.id.clone()),
            "stale marker survives the delete"
        );

        // Deleting the session removes it everywhere (log + host projections).
        t.host.delete_session("s-w".into()).unwrap();
        assert!(!t.host.list_sessions(None).iter().any(|s| s.id == "s-w"));
    });
}

/// Migration: a coding agent with a single registered workspace but no
/// persisted activation adopts it — activations made before the persistence
/// existed never landed in settings.yaml, so restarts kept losing the
/// workspace/branch display. The adoption runs when the coding agent's
/// context refreshes (UI workspace flows); two or more workspaces stay
/// manual, and nothing is adopted without an explicit single candidate.
#[test]
fn single_workspace_adopts_for_coding_agent() {
    with_temp_home(|_home| {
        let t = test_host();
        let proj = t.workdir.join("solo");
        std::fs::create_dir(&proj).unwrap();
        let ws = t
            .host
            .create_workspace(proj.to_string_lossy().into_owned())
            .expect("create");

        // No activation persisted → the coding agent adopts the only workspace.
        t.host.adopt_single_workspace_for_coding_agent_test();
        assert_eq!(
            t.host
                .resolve_workspace_dir(starriver_host::CODING_AGENT_NAME),
            Some(ws.id.clone()),
            "single workspace adopted as the working dir"
        );
        assert_eq!(
            t.host
                .get_active_workspace_for(starriver_host::CODING_AGENT_NAME),
            Some(ws.id.clone())
        );

        // …and the adoption is persisted for the next restart.
        let yaml_path = std::path::Path::new(&std::env::var("HOME").unwrap())
            .join(".starriver")
            .join("settings.yaml");
        let yaml = std::fs::read_to_string(&yaml_path).expect("settings.yaml written");
        assert!(yaml.contains(&ws.id), "adopted activation persisted");

        // Two candidates: no implicit choice — the clear+re-adopt stays inactive.
        t.host
            .set_active_workspace_for(starriver_host::CODING_AGENT_NAME, None)
            .unwrap();
        let other = t.workdir.join("other");
        std::fs::create_dir(&other).unwrap();
        t.host
            .create_workspace(other.to_string_lossy().into_owned())
            .expect("create second");
        t.host.adopt_single_workspace_for_coding_agent_test();
        assert_eq!(
            t.host
                .get_active_workspace_for(starriver_host::CODING_AGENT_NAME),
            None,
            "two workspaces are never auto-adopted"
        );
    });
}

// -- Session read path on the plugin log (P2) --

use starriver_kernel::json::JsonValue;
use starriver_kernel::llm::{ContentBlock, Message, MessageId, MessageRole, MessageSource};
use starriver_kernel::session::{SessionEvent, SurfaceOp};

/// A user/assistant model-visible message payload (`user/message` carries the
/// full Message JSON; `assistant/message` wraps it in `data.message`).
fn msg_json(role: MessageRole, id: &str, blocks: Vec<ContentBlock>) -> JsonValue {
    let source = match role {
        MessageRole::User => MessageSource::User,
        _ => MessageSource::Model {
            provider: "p".into(),
            model: "m".into(),
            replay_state: None,
        },
    };
    Message {
        id: MessageId(id.into()),
        role,
        content: blocks,
        source,
    }
    .to_json()
    .expect("message json")
}

fn plugin_snapshot_json(plugin: &str, section: &str, text: &str) -> JsonValue {
    Message {
        id: MessageId("mp".into()),
        role: MessageRole::User,
        content: vec![ContentBlock::Text(text.into())],
        source: MessageSource::Plugin {
            plugin: plugin.into(),
            form: Some(starriver_kernel::llm::ContextForm::Snapshot {
                sections: vec![starriver_kernel::llm::ContextSnapshotSection {
                    name: section.into(),
                    text: text.into(),
                }],
                truncated: false,
                budget_bytes: 0,
                composed_bytes: 0,
            }),
        },
    }
    .to_json()
    .expect("plugin message json")
}

fn sev(seq: u64, ty: &str, data: JsonValue, surface: bool) -> SessionEvent {
    sev_at(seq, 1_700_000_000_000, ty, data, surface, vec![])
}

fn sev_at(
    seq: u64,
    time: u64,
    ty: &str,
    data: JsonValue,
    surface: bool,
    source_seqs: Vec<u64>,
) -> SessionEvent {
    SessionEvent {
        seq,
        time,
        event_type: ty.into(),
        data,
        surface_op: if surface {
            Some(SurfaceOp::Append)
        } else {
            None
        },
        source_event_seqs: source_seqs,
    }
}

#[test]
fn get_session_trace_projects_the_event_ledger() {
    let t = test_host();

    // seq0 user | seq1 assistant (thinking + text + tool call) | seq2 tool
    // result (error) | seq3 meta. The trajectory must read the SAME durable
    // ledger the timeline projects — the old legacy-store read returned
    // empty for every plugin-kernel turn.
    t.host
        .seed_test_log(
            "log-trace".into(),
            None,
            None,
            vec![
                // The assembled system prompt (request/header) and a runtime
                // context snapshot injection — both model-visible, both must
                // appear in the trajectory (SYSTEM / CONTEXT rows).
                sev(
                    0,
                    "request/header",
                    JsonValue::Object(vec![
                        ("reason".into(), JsonValue::String("initial".into())),
                        (
                            "system".into(),
                            JsonValue::String("You are Starriver.".into()),
                        ),
                    ]),
                    false,
                ),
                sev(
                    1,
                    "user/message",
                    plugin_snapshot_json("runtime-context", "workspace", "/tmp/starriver"),
                    true,
                ),
                sev(
                    2,
                    "user/message",
                    msg_json(
                        MessageRole::User,
                        "m0",
                        vec![ContentBlock::Text("u1".into())],
                    ),
                    true,
                ),
                sev(
                    3,
                    "assistant/message",
                    JsonValue::Object(vec![
                        ("turn".into(), JsonValue::UInt(1)),
                        ("step".into(), JsonValue::UInt(1)),
                        (
                            "usage".into(),
                            JsonValue::from_serde_json(
                                serde_json::json!({"inputTokens": 100, "outputTokens": 20}),
                            ),
                        ),
                        (
                            "message".into(),
                            msg_json(
                                MessageRole::Assistant,
                                "m1",
                                vec![
                                    ContentBlock::Reasoning("plan first".into()),
                                    ContentBlock::Text("answer one".into()),
                                    ContentBlock::ToolCall {
                                        id: starriver_kernel::llm::CallId("c1".into()),
                                        name: None, // resolved via the tool/call registry
                                        arguments:
                                            starriver_kernel::json::JsonValue::from_json_str(
                                                r#"{"command":"ls -la"}"#,
                                            )
                                            .unwrap_or(
                                                starriver_kernel::json::JsonValue::String(
                                                    r#"{"command":"ls -la"}"#.into(),
                                                ),
                                            ),
                                    },
                                ],
                            ),
                        ),
                    ]),
                    true,
                ),
                sev(
                    4,
                    "tool/call",
                    JsonValue::Object(vec![
                        ("callId".into(), JsonValue::String("c1".into())),
                        ("name".into(), JsonValue::String("bash".into())),
                    ]),
                    false,
                ),
                sev(
                    5,
                    "tool/result",
                    JsonValue::Object(vec![(
                        "message".into(),
                        msg_json(
                            MessageRole::User,
                            "m2",
                            vec![ContentBlock::ToolResult {
                                call_id: starriver_kernel::llm::CallId("c1".into()),
                                content: vec![ContentBlock::Text("boom".into())],
                                is_error: true,
                            }],
                        ),
                    )]),
                    true,
                ),
                sev(
                    6,
                    "session/title",
                    JsonValue::Object(vec![
                        ("title".into(), JsonValue::String("meta".into())),
                        ("source".into(), JsonValue::String("fallback".into())),
                    ]),
                    false,
                ),
            ],
        )
        .expect("seed log");

    // Unknown sessions stay empty (legacy behavior contract).
    assert!(t
        .host
        .get_session_trace("missing".into())
        .entries
        .is_empty());

    let trace = t.host.get_session_trace("log-trace".into());
    let kinds: Vec<&str> = trace.entries.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(
        kinds,
        vec![
            "system",
            "context",
            "user",
            "thinking",
            "assistant",
            "tool_use",
            "tool_result"
        ],
        "every block kind gets its own chronological row: {:?}",
        trace
            .entries
            .iter()
            .map(|e| (&e.kind, &e.text))
            .collect::<Vec<_>>()
    );
    assert!(
        trace.entries[0].text.contains("You are Starriver."),
        "full system prompt carried"
    );
    assert_eq!(trace.entries[0].seq, 0.0, "ledger position exposed");
    assert_eq!(
        trace.entries[1].tool_name.as_deref(),
        Some("runtime-context")
    );
    assert!(
        trace.entries[1].text.contains("/tmp/starriver"),
        "context sections rendered"
    );
    assert_eq!(trace.entries[2].text, "u1");
    assert_eq!(trace.entries[3].text, "plan first");
    assert_eq!(trace.entries[4].text, "answer one");
    assert_eq!(
        trace.entries[4].turn,
        Some(1.0),
        "assistant entries carry turn/step"
    );
    assert_eq!(trace.entries[4].step, Some(1.0));
    assert_eq!(
        trace.entries[4].input_tokens,
        Some(100.0),
        "usage tokens surface"
    );
    assert_eq!(trace.entries[4].output_tokens, Some(20.0));
    let tool_use = &trace.entries[5];
    assert_eq!(
        tool_use.tool_name.as_deref(),
        Some("bash"),
        "callId resolves via the tool/call registry"
    );
    assert_eq!(
        tool_use.tool_input.as_deref(),
        Some(r#"{"command":"ls -la"}"#)
    );
    let tool_result = &trace.entries[6];
    assert_eq!(tool_result.tool_output.as_deref(), Some("boom"));
    assert_eq!(
        tool_result.tool_ok,
        Some(false),
        "error results carry ok=false"
    );
}

#[test]
fn get_session_trace_survives_corrupt_events_and_spans_two_turns() {
    let t = test_host();

    t.host
        .seed_test_log(
            "log-trace-2".into(),
            None,
            None,
            vec![
                // Header without `system` → dropped by the trace merge.
                sev(
                    0,
                    "request/header",
                    JsonValue::Object(vec![("reason".into(), JsonValue::String("initial".into()))]),
                    false,
                ),
                // Turn 1 with chunk-timed assistant message (source_event_seqs
                // cite two chunk events; step/start anchors the TTFT origin).
                sev_at(
                    1,
                    1_000,
                    "step/start",
                    JsonValue::from_serde_json(serde_json::json!({"turn":1,"step":1})),
                    false,
                    vec![],
                ),
                sev_at(
                    2,
                    1_500,
                    "assistant/message",
                    JsonValue::Object(vec![
                        ("turn".into(), JsonValue::UInt(1)),
                        ("step".into(), JsonValue::UInt(1)),
                        (
                            "message".into(),
                            msg_json(
                                MessageRole::Assistant,
                                "m0",
                                vec![ContentBlock::Text("turn one".into())],
                            ),
                        ),
                    ]),
                    true,
                    vec![3, 4],
                ),
                // The chunk events the assistant cites (first-chunk time 1300).
                sev_at(3, 1_300, "assistant/chunk", JsonValue::Null, false, vec![]),
                sev_at(4, 1_800, "assistant/chunk", JsonValue::Null, false, vec![]),
                // Corrupt assistant entry: `message` missing → Null decode →
                // skipped without aborting the rest of the log.
                sev(5, "assistant/message", JsonValue::Object(vec![]), true),
                sev(
                    6,
                    "user/message",
                    msg_json(
                        MessageRole::User,
                        "m1",
                        vec![ContentBlock::Text("u2".into())],
                    ),
                    true,
                ),
                sev(
                    7,
                    "assistant/message",
                    JsonValue::Object(vec![
                        ("turn".into(), JsonValue::UInt(2)),
                        ("step".into(), JsonValue::UInt(1)),
                        (
                            "message".into(),
                            msg_json(
                                MessageRole::Assistant,
                                "m2",
                                vec![ContentBlock::Text("turn two".into())],
                            ),
                        ),
                    ]),
                    true,
                ),
            ],
        )
        .expect("seed log");

    let trace = t.host.get_session_trace("log-trace-2".into());
    let kinds: Vec<&str> = trace.entries.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(
        kinds,
        vec!["assistant", "user", "assistant"],
        "system-less header dropped, corrupt entry skipped: {:?}",
        trace
            .entries
            .iter()
            .map(|e| (&e.kind, &e.text))
            .collect::<Vec<_>>()
    );

    // Chunk/step timing on the first assistant entry: TTFT = 1300−1000,
    // duration = 1500−1300.
    assert_eq!(trace.entries[0].text, "turn one");
    assert_eq!(trace.entries[0].ttft_ms, Some(300.0));
    assert_eq!(trace.entries[0].duration_ms, Some(200.0));
    assert_eq!(trace.entries[0].turn, Some(1.0));

    // Second turn is intact after the corrupt entry.
    assert_eq!(trace.entries[1].kind, "user");
    assert_eq!(trace.entries[1].text, "u2");
    assert_eq!(trace.entries[2].text, "turn two");
    assert_eq!(trace.entries[2].turn, Some(2.0));
    assert_eq!(trace.entries[2].time_ms > 0.0, true);
}

#[test]
fn get_session_timeline_reads_new_log_with_seq_ids() {
    let t = test_host();

    // seq0 user u1 | seq1 assistant a1 (text + tool call) | seq2 tool result |
    // seq3 usage-only anchor (empty content → skipped) | seq4 meta event.
    t.host
        .seed_test_log(
            "log-1".into(),
            None,
            None,
            vec![
                sev(
                    0,
                    "user/message",
                    msg_json(
                        MessageRole::User,
                        "m0",
                        vec![ContentBlock::Text("u1".into())],
                    ),
                    true,
                ),
                sev(
                    1,
                    "assistant/message",
                    JsonValue::Object(vec![(
                        "message".into(),
                        msg_json(
                            MessageRole::Assistant,
                            "m1",
                            vec![
                                ContentBlock::Text("answer one".into()),
                                ContentBlock::ToolCall {
                                    id: starriver_kernel::llm::CallId("c1".into()),
                                    name: Some("bash".into()),
                                    arguments: starriver_kernel::json::JsonValue::from_json_str(
                                        r#"{"command":"ls -la"}"#,
                                    )
                                    .unwrap_or(
                                        starriver_kernel::json::JsonValue::String(
                                            r#"{"command":"ls -la"}"#.into(),
                                        ),
                                    ),
                                },
                            ],
                        ),
                    )]),
                    true,
                ),
                sev(
                    2,
                    "tool/result",
                    JsonValue::Object(vec![(
                        "message".into(),
                        msg_json(
                            MessageRole::User,
                            "m2",
                            vec![ContentBlock::ToolResult {
                                call_id: starriver_kernel::llm::CallId("c1".into()),
                                content: vec![ContentBlock::Text("out".into())],
                                is_error: false,
                            }],
                        ),
                    )]),
                    true,
                ),
                sev(
                    3,
                    "assistant/message",
                    JsonValue::Object(vec![(
                        "message".into(),
                        msg_json(MessageRole::Assistant, "m3", vec![]),
                    )]),
                    true,
                ),
                sev(
                    4,
                    "session/title",
                    JsonValue::Object(vec![
                        ("title".into(), JsonValue::String("meta".into())),
                        ("source".into(), JsonValue::String("fallback".into())),
                    ]),
                    false,
                ),
            ],
        )
        .expect("seed log");

    // Unknown sessions and load failures stay empty, mirroring legacy behavior.
    assert!(t.host.get_session_timeline("missing".into()).is_empty());
    assert!(t.host.get_session_messages("missing".into()).is_empty());

    let timeline = t.host.get_session_timeline("log-1".into());
    assert_eq!(
        timeline.len(),
        3,
        "usage anchors and meta events are invisible: {timeline:?}"
    );
    assert_eq!(timeline[0].id, 0, "seq is the timeline id");
    assert_eq!(timeline[0].role, "user");
    assert_eq!(timeline[0].content, "u1");
    assert_eq!(timeline[1].id, 1);
    assert_eq!(timeline[1].role, "assistant");
    assert!(timeline[1].content.contains("answer one"));
    assert!(
        timeline[1].content.contains("bash") && timeline[1].content.contains("ls"),
        "tool call renders its label: {:?}",
        timeline[1].content
    );
    assert_eq!(timeline[2].id, 2);
    assert!(
        timeline[2].content.contains("✓") && timeline[2].content.contains("out"),
        "ok tool result renders with a check head: {:?}",
        timeline[2].content
    );

    // get_session_messages goes through display_pieces: each assistant
    // content block is split into its own entry (Text → assistant text,
    // ToolCall → 🔧 label line, results → ✓/✗ line); the timeline instead
    // merges and renders 1:1 by seq. A usage-only anchor is invisible on
    // both paths.
    let messages = t.host.get_session_messages("log-1".into());
    assert_eq!(
        messages.len(),
        4,
        "text / tool label / result split: {messages:?}"
    );
    assert_eq!(messages[0].role, "user");
    assert_eq!(messages[0].content, "u1");
    assert_eq!(messages[1].role, "assistant");
    assert_eq!(messages[1].content, "answer one");
    assert_eq!(messages[2].role, "tool");
    assert_eq!(messages[2].content, "🔧 bash: ls -la");
    assert_eq!(messages[3].role, "tool");
    assert_eq!(messages[3].content.contains("out"), true);
}

/// Thinking-chain replay (docs/36): reasoning blocks persisted inside
/// assistant/message events project to `thinking` rows on the display path —
/// interleaved in block order, one row per step — so a restart restores the
/// chain in place. The backtrack timeline keeps flattening reasoning into
/// prose-free text (seq 1:1 contract unchanged).
#[test]
fn get_session_messages_replays_reasoning_as_thinking_rows() {
    let t = test_host();

    // Step 1: reasoning + text + tool call; step 2: reasoning only.
    t.host
        .seed_test_log(
            "think-1".into(),
            None,
            None,
            vec![
                sev(
                    0,
                    "user/message",
                    msg_json(
                        MessageRole::User,
                        "m0",
                        vec![ContentBlock::Text("fix it".into())],
                    ),
                    true,
                ),
                sev(
                    1,
                    "assistant/message",
                    JsonValue::Object(vec![(
                        "message".into(),
                        msg_json(
                            MessageRole::Assistant,
                            "m1",
                            vec![
                                ContentBlock::Reasoning("look at the layout first".into()),
                                ContentBlock::Text("done".into()),
                                ContentBlock::ToolCall {
                                    id: starriver_kernel::llm::CallId("c1".into()),
                                    name: Some("bash".into()),
                                    arguments: starriver_kernel::json::JsonValue::from_json_str(
                                        r#"{"command":"ls"}"#,
                                    )
                                    .unwrap_or(
                                        starriver_kernel::json::JsonValue::String(
                                            r#"{"command":"ls"}"#.into(),
                                        ),
                                    ),
                                },
                            ],
                        ),
                    )]),
                    true,
                ),
                sev(
                    2,
                    "tool/result",
                    JsonValue::Object(vec![(
                        "message".into(),
                        msg_json(
                            MessageRole::User,
                            "m2",
                            vec![ContentBlock::ToolResult {
                                call_id: starriver_kernel::llm::CallId("c1".into()),
                                content: vec![ContentBlock::Text("a.txt".into())],
                                is_error: false,
                            }],
                        ),
                    )]),
                    true,
                ),
                sev(
                    3,
                    "assistant/message",
                    JsonValue::Object(vec![(
                        "message".into(),
                        msg_json(
                            MessageRole::Assistant,
                            "m3",
                            vec![ContentBlock::Reasoning("second look".into())],
                        ),
                    )]),
                    true,
                ),
            ],
        )
        .expect("seed log");

    let messages = t.host.get_session_messages("think-1".into());
    let roles: Vec<&str> = messages.iter().map(|m| m.role.as_str()).collect();
    assert_eq!(
        roles,
        vec!["user", "thinking", "assistant", "tool", "tool", "thinking"],
        "reasoning precedes its step's prose/tools, one row per block: {messages:?}"
    );
    assert_eq!(messages[1].content, "look at the layout first");
    assert_eq!(messages[5].content, "second look");
}

// -- Fork on the session log (P2) --

/// Seed the canonical four-surface-message log (u1 a1 u2 a2) used by fork tests.
fn seed_fork_source(t: &TestHost, id: &str) {
    let m = |role: MessageRole, mid: &str, text: &str| {
        msg_json(role, mid, vec![ContentBlock::Text(text.into())])
    };
    t.host
        .seed_test_log(
            id.into(),
            None,
            None,
            vec![
                sev(0, "user/message", m(MessageRole::User, "m0", "u1"), true),
                sev(
                    1,
                    "assistant/message",
                    JsonValue::Object(vec![(
                        "message".into(),
                        m(MessageRole::Assistant, "m1", "a1"),
                    )]),
                    true,
                ),
                sev(2, "user/message", m(MessageRole::User, "m2", "u2"), true),
                sev(
                    3,
                    "assistant/message",
                    JsonValue::Object(vec![(
                        "message".into(),
                        m(MessageRole::Assistant, "m3", "a2"),
                    )]),
                    true,
                ),
            ],
        )
        .expect("seed fork source");
}

#[test]
fn fork_session_seeds_child_from_log() {
    let t = test_host();
    seed_fork_source(&t, "src");

    // Cut before u2 (seq 2): the child inherits [u1, a1].
    let info = t
        .host
        .fork_session("src".into(), 2, "child task".into())
        .expect("fork");
    assert_eq!(info.parent_id.as_deref(), Some("src"));
    assert_eq!(info.fork_point, Some(2), "fork_point is expressed in seq");
    assert_eq!(info.title, "child task");

    let child_tl = t.host.get_session_timeline(info.id.clone());
    let rendered: Vec<(i64, String)> = child_tl.iter().map(|m| (m.id, m.content.clone())).collect();
    assert_eq!(
        rendered,
        vec![(0, "u1".to_string()), (1, "a1".to_string())],
        "child history is exactly the pre-cutoff prefix"
    );

    // The source stays append-only and intact.
    assert_eq!(t.host.get_session_timeline("src".into()).len(), 4);

    // The child is visible in listings with its lineage recorded.
    let listed = t
        .host
        .list_sessions(None)
        .into_iter()
        .find(|s| s.id == info.id)
        .expect("forked session is listed");
    assert_eq!(listed.parent_id.as_deref(), Some("src"));
    assert_eq!(listed.fork_point, Some(2));
}

#[test]
fn fork_boundary_edges_and_missing_source_errors() {
    let t = test_host();
    seed_fork_source(&t, "src");

    // Missing source errors.
    assert!(t.host.fork_session("nope".into(), 1, "x".into()).is_err());

    // cutoff <= 0 creates an empty conversation under the same lineage.
    let empty = t
        .host
        .fork_session("src".into(), 0, "fresh".into())
        .expect("empty fork");
    assert_eq!(empty.fork_point, Some(0));
    assert!(t.host.get_session_timeline(empty.id).is_empty());

    // cutoff beyond the end clamps to the full log.
    let full = t
        .host
        .fork_session("src".into(), 999, "full copy".into())
        .expect("clamped fork");
    assert_eq!(t.host.get_session_timeline(full.id).len(), 4);

    // No explicit task falls back to the source's first user message.
    let inherited = t
        .host
        .fork_session("src".into(), 2, String::new())
        .expect("inherit");
    assert_eq!(
        inherited.title, "u1",
        "first user text seeds the default title"
    );
}

#[test]
fn forked_child_shares_parent_workspace_or_lists_ungrouped() {
    with_temp_home(|_home| {
        let t = test_host();

        // Ungrouped parent: the child still shows up in the default listing.
        seed_fork_source(&t, "src-free");
        let free = t
            .host
            .fork_session("src-free".into(), 2, "c1".into())
            .unwrap();
        assert_eq!(
            t.host
                .list_sessions(None)
                .iter()
                .find(|s| s.id == free.id)
                .expect("listed")
                .workspace_id,
            None,
        );

        // Grouped parent (real execute path attaches): the child joins the group.
        let proj = t.workdir.join("proj");
        std::fs::create_dir(&proj).unwrap();
        let ws = t
            .host
            .create_workspace(proj.to_string_lossy().into_owned())
            .unwrap();
        t.host
            .set_active_workspace_for(starriver_host::CODING_AGENT_NAME, Some(ws.id.clone()))
            .unwrap();
        let mut rx = t
            .host
            .execute(
                "grouped root".into(),
                Some("src-grouped".into()),
                None,
                Some(starriver_host::CODING_AGENT_NAME.into()),
                None,
                None,
                None,
            )
            .unwrap();
        let handle = t.host.runtime_handle();
        handle.block_on(async move {
            for _ in 0..20 {
                match rx.recv().await {
                    Ok(ev) if ev["type"] == "turnError" || ev["type"] == "turnEnd" => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        });
        let grouped = t
            .host
            .fork_session("src-grouped".into(), 99, "c2".into())
            .expect("fork grouped");
        assert_eq!(
            t.host
                .list_sessions(None)
                .iter()
                .find(|s| s.id == grouped.id)
                .expect("listed")
                .workspace_id,
            Some(ws.id.clone()),
            "child lands in the parent's workspace"
        );
        // project-root: the child header inherits the parent's stamped root.
        let expected = std::fs::canonicalize(&proj)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            t.host
                .debug_session_header(grouped.id.clone())
                .unwrap()
                .cwd
                .as_deref(),
            Some(expected.as_str()),
            "forked child inherits the workspace root"
        );
    });
}

// -- Title-LLM (provider session titles) --

mod stub_adapter {
    use futures::stream::{BoxStream, StreamExt};
    use starriver_kernel::llm::{
        ContentBlock, ContentBlockType, FinishReason, GenerateOptions, LlmAdapter, LlmError,
        StreamChunk,
    };
    use std::sync::{Arc, Mutex};

    /// Kernel adapter returning a canned reply to every chat call and
    /// streaming that same text for the agent loop.
    pub struct StubAdapter {
        pub provider: String,
        pub reply: String,
        pub calls: Arc<Mutex<Vec<String>>>,
    }

    impl LlmAdapter for StubAdapter {
        fn providers(&self) -> &[String] {
            std::slice::from_ref(&self.provider)
        }

        fn stream(
            &self,
            _options: GenerateOptions,
        ) -> BoxStream<'static, Result<StreamChunk, LlmError>> {
            let reply = self.reply.clone();
            Box::pin(futures::stream::iter(vec![
                Ok(StreamChunk::BlockStart {
                    index: 0,
                    block_type: ContentBlockType::Text,
                }),
                Ok(StreamChunk::TextDelta {
                    index: 0,
                    text: reply.clone(),
                }),
                Ok(StreamChunk::BlockEnd {
                    index: 0,
                    block: ContentBlock::Text(reply),
                }),
                Ok(StreamChunk::Finish {
                    reason: FinishReason::Stop,
                    replay_state: None,
                }),
            ]))
        }

        fn chat(
            &self,
            options: GenerateOptions,
        ) -> futures::future::BoxFuture<'static, Result<String, LlmError>> {
            // Record the user turn so the test can see the prompt subject.
            let text = options
                .messages
                .iter()
                .flat_map(|message| {
                    message.content.iter().filter_map(|block| match block {
                        ContentBlock::Text(t) => Some(t.clone()),
                        _ => None,
                    })
                })
                .collect::<Vec<_>>()
                .join(" ");
            self.calls.lock().unwrap().push(text);
            let reply = self.reply.clone();
            Box::pin(async move { Ok(reply) })
        }
    }
}

#[test]
fn generate_session_title_writes_provider_title_and_respects_pin() {
    use std::sync::{Arc, Mutex};

    let t = test_host();
    seed_fork_source(&t, "tt");
    // Deliberately wrapped in CJK quote shells — cleaning must strip them.
    let reply = "“Fix fork cutoffs”".to_string();
    let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    t.host
        .debug_register_adapter(Arc::new(stub_adapter::StubAdapter {
            provider: "title-stub".into(),
            reply: reply.clone(),
            calls: calls.clone(),
        }));

    // Happy path: a provider title is written over the fallback.
    let generated = t
        .host
        .generate_session_title("tt".into(), "title-stub".into(), "stub-1".into())
        .expect("run")
        .expect("generated");
    assert_eq!(generated, "Fix fork cutoffs", "quote shells are stripped");
    assert!(
        calls.lock().unwrap()[0].contains("u1"),
        "subject comes from the first user message"
    );
    let listed = t.host.list_sessions(None);
    assert_eq!(
        listed
            .iter()
            .find(|s| s.id == "tt")
            .map(|s| s.title.as_str()),
        Some("Fix fork cutoffs"),
        "the projection serves the provider title"
    );

    // User pin wins: manual rename blocks regeneration.
    t.host
        .rename_session("tt".into(), Some("手动命名".into()))
        .unwrap();
    let blocked = t
        .host
        .generate_session_title("tt".into(), "title-stub".into(), "stub-1".into())
        .expect("run");
    assert_eq!(blocked, None, "pinned titles refuse regeneration");
    assert_eq!(
        t.host
            .list_sessions(None)
            .iter()
            .find(|s| s.id == "tt")
            .map(|s| s.title.as_str()),
        Some("手动命名")
    );
}

#[test]
fn generate_session_title_is_silent_without_engines_or_subject() {
    use std::sync::{Arc, Mutex};

    let t = test_host();
    // No engine registered at all.
    assert_eq!(
        t.host
            .generate_session_title("nothing".into(), "title-stub".into(), "stub-1".into())
            .unwrap(),
        None
    );

    // Engine present but the session does not exist — still silent None.
    seed_fork_source(&t, "quiet");
    t.host
        .debug_register_adapter(Arc::new(stub_adapter::StubAdapter {
            provider: "title-stub".into(),
            reply: "unused".into(),
            calls: Arc::new(Mutex::new(Vec::new())),
        }));
    assert_eq!(
        t.host
            .generate_session_title("nope".into(), "title-stub".into(), "stub-1".into())
            .unwrap(),
        None
    );
}

// -- projcache cold-read self-healing (P2 §3.2 ladder) --

/// A projection-cache row that disappears (fail-soft write loss / manual wipe)
/// must be rebuilt lazily from the durable log at list time — including the
/// pinned title, archive state and the group remembered in the members table.
#[test]
fn list_sessions_rebuilds_missing_projection_rows_from_log() {
    let t = test_host();
    // Grouped history: register the group, then pretend the session was once
    // attached (the members table is the authority and is never lost).
    let proj = t.workdir.join("proj");
    std::fs::create_dir(&proj).unwrap();
    let ws = t
        .host
        .create_workspace(proj.to_string_lossy().into_owned())
        .unwrap();

    seed_fork_source(&t, "lost");
    t.host
        .rename_session("lost".into(), Some("recover me".into()))
        .unwrap();
    {
        let conn =
            rusqlite::Connection::open(t._dir.path().join("test.db.host.db")).expect("host db");
        conn.execute(
            "INSERT INTO workspace_members (workspace_id, session_id, position) VALUES (?1, 'lost', 0)",
            [&ws.id],
        )
        .unwrap();
    }

    // Sanity: listed with its folded state before the wipe.
    assert_eq!(
        t.host
            .list_sessions(None)
            .iter()
            .find(|s| s.id == "lost")
            .expect("listed")
            .title,
        "recover me"
    );

    // Wipe only the projection row — the fail-soft cache loss scenario.
    {
        let conn =
            rusqlite::Connection::open(t._dir.path().join("test.db.host.db")).expect("host db");
        conn.execute(
            "DELETE FROM session_projcache WHERE session_id = 'lost'",
            [],
        )
        .unwrap();
    }

    // The next listing lazily rebuilds the row from the plugin log.
    let rebuilt = t
        .host
        .list_sessions(None)
        .into_iter()
        .find(|s| s.id == "lost")
        .expect("row healed");
    assert_eq!(rebuilt.title, "recover me", "pinned title restored");
    assert_eq!(
        rebuilt.workspace_id.as_deref(),
        Some(ws.id.as_str()),
        "group restored from the members table"
    );
    assert_eq!(rebuilt.message_count, 2, "assistant messages refolded");
}

// -- API-key file storage (!file:) end-to-end --

/// Runs `f` with $HOME pointed at a throwaway dir. Serialized via a static lock
/// because HostRuntime derives ~/.starriver paths from $HOME lazily — key saves
/// (add_model / add_cloud_provider) write the secrets file on every call, so the
/// swap must cover the host's whole lifetime, not just construction.
fn with_temp_home<F: FnOnce(&Path)>(f: F) {
    /// RAII HOME restore: must be restored even if the test panics,
    /// otherwise every subsequent test in this process would read/write
    /// settings pointing at a deleted temp directory.
    struct HomeGuard(Option<String>);
    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match self.0.take() {
                Some(h) => std::env::set_var("HOME", h),
                None => std::env::remove_var("HOME"),
            }
        }
    }
    // Establish the neutral baseline BEFORE taking the lock (pin takes it
    // internally); this body then swaps HOME under the same lock so no two
    // tests can interleave HOME states.
    pin_home_once();
    let _lock = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = TempDir::new().expect("tempdir");
    let old = std::env::var("HOME").ok();
    let _home = HomeGuard(old.clone());
    std::env::set_var("HOME", dir.path());
    f(dir.path());
}

/// Build a HostRuntime whose config + ~/.starriver both live inside `dir`.
fn host_in(dir: &Path) -> HostRuntime {
    pin_home_once();
    let workdir = dir.join("workdir");
    std::fs::create_dir_all(&workdir).expect("workdir");
    let config = make_config(dir);
    HostRuntime::new(Some(config.to_string_lossy().into_owned())).expect("host boots")
}

#[test]
fn add_model_key_roundtrip_preserve_and_remove() {
    with_temp_home(|dir| {
        let host = host_in(dir);

        // Add with a real key → stored as a !file: ref, resolvable end-to-end.
        host.add_model(
            "ds".into(),
            "DeepSeek".into(),
            "cloud".into(),
            "openai-compat".into(),
            Some("https://api.deepseek.com".into()),
            Some("sk-ds".into()),
            None,
            "deepseek-chat".into(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .expect("add model");
        let m = host
            .list_models()
            .into_iter()
            .find(|m| m.id == "ds")
            .expect("ds in list");
        assert_eq!(m.api_key, "!file:model/ds");

        // Re-save WITHOUT a key → existing ref preserved, key still resolves.
        host.add_model(
            "ds".into(),
            "DeepSeek".into(),
            "cloud".into(),
            "openai-compat".into(),
            Some("https://api.deepseek.com".into()),
            None,
            None,
            "deepseek-chat".into(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .expect("re-add model");
        // A brand-new model with no key → api_key stays empty, not a ref.
        host.add_model(
            "nokey".into(),
            "NoKey".into(),
            "cloud".into(),
            "openai-compat".into(),
            None,
            None,
            None,
            "m".into(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .expect("add no-key model");
        let nk = host
            .list_models()
            .into_iter()
            .find(|m| m.id == "nokey")
            .expect("nokey in list");
        assert_eq!(nk.api_key, "");

        // Remove → model + key gone → resolve errors.
        host.remove_model("ds".into()).expect("remove model");
    });
}

/// BUG-106: an LLM route that fails fast (dead endpoint, wrong model id,
/// exhausted quota) must leave the driver idle. Before the fix the
/// ReactLoopAgent driver re-opened turns without any backoff, burning
/// 1-3 cores indefinitely; cancel/delete did not stop it.
#[test]
fn fast_failing_model_does_not_spin_the_driver() {
    with_temp_home(|_dir| {
        let host = host_in(_dir);

        // Instant-failing gateway: answers every request with the same
        // 400 model-not-found body the live repro produced (GLM code 1211).
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind gateway");
        let addr = listener.local_addr().expect("gateway addr");
        let gateway = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let body = br#"{"error":{"code":"1211","message":"model not found"}}"#;
                let head = format!(
                    "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = std::io::Write::write_all(&mut stream, head.as_bytes());
                let _ = std::io::Write::write_all(&mut stream, body);
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        });

        host.add_model(
            "dead".into(),
            "Dead".into(),
            "cloud".into(),
            "openai-compat".into(),
            Some(format!("http://{addr}/v1")),
            Some("sk-x".into()),
            None,
            "m".into(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .expect("add dead model");
        host.set_agent_active_model(
            starriver_host::CODING_AGENT_NAME.into(),
            "dead".into(),
            "m".into(),
        )
        .expect("activate dead model");

        let mut rx = host
            .execute(
                "hi".into(),
                None,
                None,
                Some(starriver_host::CODING_AGENT_NAME.into()),
                None,
                None,
                None,
            )
            .expect("execute accepts");
        let handle = host.runtime_handle();
        let events: Vec<serde_json::Value> = handle.block_on(async move {
            let mut out = Vec::new();
            for _ in 0..100 {
                match tokio::time::timeout(std::time::Duration::from_secs(15), rx.recv()).await {
                    Ok(Ok(ev)) => {
                        let is_end = ev["type"] == "turnError" || ev["type"] == "turnEnd";
                        out.push(ev);
                        if is_end {
                            break;
                        }
                    }
                    _ => break,
                }
            }
            out
        });
        let sid: String = host
            .list_sessions(None)
            .into_iter()
            .map(|s| s.id)
            .next()
            .unwrap_or_default();
        assert!(
            !sid.is_empty(),
            "session must exist after execute; events: {events:?}"
        );

        // After the terminal failure the driver must stay idle: no further
        // turn/start appends may land while nothing new enters the inbox.
        let count = || {
            host.debug_session_log_event_data(sid.clone(), "turn/start".into())
                .len()
        };
        let before = count();
        std::thread::sleep(std::time::Duration::from_millis(1500));
        let after = count();
        assert!(
            after <= before + 1,
            "driver re-opened turns after a fast LLM failure: {before} -> {after}"
        );
        drop(gateway);
    });
}

#[test]
fn title_request_audited_even_without_engine() {
    let t = test_host();
    let mut rx = t
        .host
        .execute(
            "name me".into(),
            Some("audit-t".into()),
            None,
            None,
            None,
            None,
            None,
        )
        .expect("execute accepts");
    let handle = t.host.runtime_handle();
    handle.block_on(async move {
        for _ in 0..20 {
            match rx.recv().await {
                Ok(ev) => {
                    if ev["type"] == "turnError" || ev["type"] == "turnEnd" {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    // Drive the title path synchronously via its public entry point; no
    // engine resolves, but the pre-call audit event must still be on record.
    let generated = t
        .host
        .generate_session_title("audit-t".into(), "missing-provider".into(), "m".into())
        .expect("title path tolerant");
    assert!(generated.is_none(), "no engine → no title");

    let audits = poll_session_events(&t.host, "audit-t", "session/title-llm-request");
    assert!(
        !audits.is_empty(),
        "pre-call audit event must exist even when no engine resolves"
    );
}

#[test]
fn named_execute_and_review_mirror_into_kernel_residents() {
    with_temp_home(|_home| {
        let t = test_host();

        // No runtime kernel yet → the preview is None; after boot it exists.
        // Before any run names an agent, no persona is registered.
        let preview = t.host.debug_kernel_system_preview().expect("kernel live");
        assert!(
            preview.starts_with(starriver_host::plugin_kernel::IDENTITY_PROMPT),
            "identity stays the first resident: {preview}"
        );
        assert!(
            !preview.contains("You are a coding agent."),
            "no persona until a run names an agent: {preview}"
        );

        // Naming the coding agent registers its profile body as the persona.
        t.host
            .execute(
                "warm".into(),
                None,
                None,
                Some(starriver_host::CODING_AGENT_NAME.into()),
                None,
                None,
                None,
            )
            .ok();
        let preview = t.host.debug_kernel_system_preview().expect("kernel live");
        assert!(
            preview.contains("You are a coding agent."),
            "named execute must register the profile body as the persona: {preview}"
        );

        // Review mode replaces the whole system via the complete:true resident.
        t.host
            .start_code_review(r#"["src/lib.rs"]"#.into(), None)
            .expect("review");
        let preview = t.host.debug_kernel_system_preview().expect("kernel live");
        assert!(
            preview.starts_with("You are a code review expert."),
            "complete:true review resident replaces every other section: {preview}"
        );

        // Review outlives same-agent reruns (an explicit user action is not
        // undone by the next turn).
        t.host
            .execute(
                "warm".into(),
                None,
                None,
                Some(starriver_host::CODING_AGENT_NAME.into()),
                None,
                None,
                None,
            )
            .ok();
        let preview = t.host.debug_kernel_system_preview().expect("kernel live");
        assert!(
            preview.starts_with("You are a code review expert."),
            "same-agent run must not disturb the review resident: {preview}"
        );

        // Switching away and back (per-execute agent naming) leaves review
        // mode and re-registers the persona.
        t.host
            .execute(
                "warm".into(),
                None,
                None,
                Some(starriver_host::CHAT_AGENT_NAME.into()),
                None,
                None,
                None,
            )
            .ok();
        t.host
            .execute(
                "warm".into(),
                None,
                None,
                Some(starriver_host::CODING_AGENT_NAME.into()),
                None,
                None,
                None,
            )
            .ok();
        let preview = t.host.debug_kernel_system_preview().expect("kernel live");
        assert!(
            !preview.starts_with("You are a code review expert.")
                && preview.contains("You are a coding agent."),
            "switch must exit review and restore the persona: {preview}"
        );
    });
}

// -- Today-item hardening: title audit / full review chain / error paths --

fn wait_terminal(host: &HostRuntime, rx: tokio::sync::broadcast::Receiver<serde_json::Value>) {
    let handle = host.runtime_handle();
    handle.block_on(async move {
        let mut rx = rx;
        for _ in 0..40 {
            match rx.recv().await {
                Ok(ev) => {
                    if ev["type"] == "turnError" || ev["type"] == "turnEnd" {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
}

#[test]
fn title_audit_skipped_for_user_pinned_sessions() {
    with_temp_home(|_home| {
        let t = test_host();
        let mut rx = t
            .host
            .execute(
                "name me".into(),
                Some("pin-t".into()),
                None,
                None,
                None,
                None,
                None,
            )
            .expect("execute");
        wait_terminal(&t.host, rx);

        t.host
            .rename_session("pin-t".into(), Some("human title".into()))
            .expect("user pin");
        // Baseline may already contain the execute-time detached audit; the pin
        // must keep the explicit generation from appending another one.
        let audits_before = {
            let types = t.host.debug_session_log_event_types("pin-t".into());
            types
                .iter()
                .filter(|t| *t == "session/title-llm-request")
                .count()
        };
        let generated = t
            .host
            .generate_session_title("pin-t".into(), "title-stub".into(), "stub-1".into())
            .expect("tolerant");
        assert!(generated.is_none(), "user pin suppresses generation");
        let audits_after = {
            let types = t.host.debug_session_log_event_types("pin-t".into());
            types
                .iter()
                .filter(|t| *t == "session/title-llm-request")
                .count()
        };
        assert_eq!(
            audits_after, audits_before,
            "no new audit trail for a user-pinned session"
        );
    });
}

#[test]
fn start_code_review_invalid_json_is_rejected_without_kernel_change() {
    with_temp_home(|_home| {
        let t = test_host();
        let before = t.host.debug_kernel_system_preview().expect("kernel live");

        assert!(t.host.start_code_review("not json".into(), None).is_err());
        let after = t.host.debug_kernel_system_preview().expect("kernel live");
        assert_eq!(
            before, after,
            "failed review entry must not touch residents"
        );
    });
}

#[test]
fn review_mode_reaches_the_wire_as_system_and_tool_whitelist() {
    with_temp_home(|_home| {
        let t = test_host();
        t.host
            .start_code_review(r#"["src/lib.rs"]"#.into(), None)
            .expect("review");

        let mut rx = t
            .host
            .execute(
                "review it".into(),
                Some("rev-t".into()),
                None,
                Some(starriver_host::CODING_AGENT_NAME.into()),
                None,
                None,
                None,
            )
            .expect("execute");
        wait_terminal(&t.host, rx);

        let headers = poll_session_events(&t.host, "rev-t", "request/header");
        let header = headers.last().expect("a request must have been assembled");
        let system = header.as_str_at("system").expect("system logged");
        assert!(
            system.starts_with("You are a code review expert."),
            "complete:true review prompt rides the wire: {system}"
        );
        let tools = header
            .get("tools")
            .and_then(starriver_kernel::json::JsonValue::as_array)
            .expect("tools logged");
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|tool| tool.as_str_at("name"))
            .collect();
        assert!(
            !names.is_empty() && names.iter().all(|n| *n == "read" || *n == "grep"),
            "whitelist must reach the wire: {names:?}"
        );
    });
}

/// BUG-1 regression (WI-66): the memory recall→inject chain was severed when
/// the plugin kernel took over the execute path (04b4fc1) — the assembled
/// `agent_prompt` never reached the engine. The restored path hands the
/// recalled facts to the kernel as a trailing system section, and the test
/// proves one seeded fact reaches the wire's system prompt.
#[test]
fn memory_recall_injects_into_the_system_prompt() {
    with_temp_home(|_home| {
        let t = test_host();
        let proj = t.workdir.join("proj");
        std::fs::create_dir(&proj).unwrap();
        let ws = t
            .host
            .create_workspace(proj.to_string_lossy().into_owned())
            .unwrap();
        t.host
            .set_active_workspace_for(starriver_host::CODING_AGENT_NAME, Some(ws.id.clone()))
            .unwrap();

        // Manual write (panel path): name slugifies, source_agent badges manual.
        let entry = t
            .host
            .upsert_memory(
                Some(ws.id.clone()),
                "Deploy Keys".into(),
                "rotate every ninety days".into(),
                "keys are stored in the vault".into(),
            )
            .expect("upsert");
        assert_eq!(entry.name, "deploy-keys", "name slugified");
        assert_eq!(entry.source_agent, "manual");

        let mut rx = t
            .host
            .execute(
                "deploy the service".into(),
                Some("mem-t".into()),
                None,
                Some(starriver_host::CODING_AGENT_NAME.into()),
                None,
                None,
                None,
            )
            .expect("execute");
        wait_terminal(&t.host, rx);

        let headers = poll_session_events(&t.host, "mem-t", "request/header");
        let header = headers.last().expect("a request must have been assembled");
        let system = header.as_str_at("system").expect("system logged");
        assert!(
            system.contains("## Memory"),
            "memory section rides the system prompt: {system}"
        );
        assert!(
            system.contains("rotate every ninety days"),
            "recalled fact reaches the wire: {system}"
        );
    });
}

pub mod ask_user_e2e {
    use futures::stream::BoxStream;
    use starriver_kernel::llm::{
        ContentBlock, FinishReason, GenerateOptions, LlmAdapter, LlmError, StreamChunk,
    };
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// Scripted adapter for the user-questions slice (WI-67): the first LLM
    /// call emits an `ask_user` tool call; every later call returns final text.
    /// `saw_unattended` records whether any assembled request carried the
    /// scheduled-run contract section (probe for the unattended guard).
    pub struct AskUserAdapter {
        pub calls: AtomicUsize,
        pub args_json: Mutex<String>,
        pub saw_unattended: AtomicBool,
    }

    impl LlmAdapter for AskUserAdapter {
        fn providers(&self) -> &[String] {
            static PROVIDERS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
            PROVIDERS.get_or_init(|| vec!["mock".to_string()])
        }

        fn stream(
            &self,
            options: GenerateOptions,
        ) -> BoxStream<'static, Result<StreamChunk, LlmError>> {
            if options
                .system
                .as_deref()
                .is_some_and(|s| s.contains("Unattended Run"))
            {
                self.saw_unattended.store(true, Ordering::SeqCst);
            }
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                let args = self.args_json.lock().unwrap().clone();
                Box::pin(futures::stream::iter(vec![
                    Ok(StreamChunk::ToolCallDelta {
                        index: 0,
                        id: starriver_kernel::llm::CallId("q1".into()),
                        name: Some("ask_user".into()),
                        arguments_delta: args.clone(),
                    }),
                    Ok(StreamChunk::BlockEnd {
                        index: 0,
                        block: ContentBlock::ToolCall {
                            id: starriver_kernel::llm::CallId("q1".into()),
                            name: Some("ask_user".into()),
                            arguments: starriver_kernel::json::JsonValue::from_json_str(&args)
                                .unwrap_or_else(|_| {
                                    starriver_kernel::json::JsonValue::String(args.to_string())
                                }),
                        },
                    }),
                    Ok(StreamChunk::Finish {
                        reason: FinishReason::ToolCalls,
                        replay_state: None,
                    }),
                ]))
            } else {
                Box::pin(futures::stream::iter(vec![
                    Ok(StreamChunk::TextDelta {
                        index: 0,
                        text: "done".into(),
                    }),
                    Ok(StreamChunk::BlockEnd {
                        index: 0,
                        block: ContentBlock::Text("done".into()),
                    }),
                    Ok(StreamChunk::Finish {
                        reason: FinishReason::Stop,
                        replay_state: None,
                    }),
                ]))
            }
        }
    }
} // mod ask_user_e2e

/// The ask→UI→respond round trip: the tool call blocks on the bridge, the
/// question event lands in the session log, `respond_question` resolves it,
/// and the answer reaches the tool output (and thus the next model turn).
#[test]
fn ask_user_tool_resolves_via_interaction_respond() {
    with_temp_home(|dir| {
        let host = host_in(dir);
        host.debug_register_adapter(Arc::new(ask_user_e2e::AskUserAdapter {
            calls: std::sync::atomic::AtomicUsize::new(0),
            args_json: std::sync::Mutex::new(
                r#"{"question":"which db?","options":["postgres","sqlite"],"timeout_secs":300}"#
                    .into(),
            ),
            saw_unattended: std::sync::atomic::AtomicBool::new(false),
        }));

        let mut rx = host
            .execute(
                "ask me".into(),
                Some("ask-t".into()),
                None,
                Some(starriver_host::CODING_AGENT_NAME.into()),
                None,
                None,
                None,
            )
            .expect("execute");

        // The question event lands in the session log (this is what the
        // desktop modal consumes via the UiEvent chain).
        let mut question_id = None;
        for _ in 0..200 {
            let events =
                host.debug_session_log_event_data("ask-t".into(), "interaction/question".into());
            if let Some(data) = events.first() {
                question_id = data.as_str_at("id").map(String::from);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let question_id = question_id.expect("interaction/question in the session log");
        assert!(
            host.respond_question(&question_id, "sqlite"),
            "respond resolves"
        );
        wait_terminal(&host, rx);

        // Audit pair complete + the answer reached the tool output. The read
        // retries briefly: post-turn dispose can race the durable read.
        let mut answer_text = None;
        for _ in 0..80 {
            let events =
                host.debug_session_log_event_data("ask-t".into(), "interaction/answer".into());
            if let Some(d) = events
                .first()
                .and_then(|d| d.as_str_at("answer").map(String::from))
            {
                answer_text = Some(d);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert_eq!(answer_text.as_deref(), Some("sqlite"));
        let results = poll_session_events(&host, "ask-t", "tool/result");
        let rendered = format!("{:?}", results.last().expect("tool result logged"));
        assert!(
            rendered.contains("sqlite"),
            "answer reaches the tool result: {rendered}"
        );
    });
}

/// Degradation: nobody answers within the tool's timeout → the tool reports
/// the miss as content (not an error) and the abandonment is audited.
#[test]
fn ask_user_tool_times_out_and_reports_the_miss() {
    with_temp_home(|dir| {
        let host = host_in(dir);
        host.debug_register_adapter(Arc::new(ask_user_e2e::AskUserAdapter {
            calls: std::sync::atomic::AtomicUsize::new(0),
            args_json: std::sync::Mutex::new(r#"{"question":"why?","timeout_secs":1}"#.into()),
            saw_unattended: std::sync::atomic::AtomicBool::new(false),
        }));

        let mut rx = host
            .execute(
                "ask me".into(),
                Some("ask-dead".into()),
                None,
                Some(starriver_host::CODING_AGENT_NAME.into()),
                None,
                None,
                None,
            )
            .expect("execute");
        wait_terminal(&host, rx);

        let mut abandoned = None;
        for _ in 0..80 {
            let events =
                host.debug_session_log_event_data("ask-dead".into(), "interaction/answer".into());
            if let Some(d) = events
                .first()
                .and_then(|d| d.get("abandoned").and_then(|b| b.as_bool()))
            {
                abandoned = Some(d);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert_eq!(abandoned, Some(true), "abandonment audited");
        let results = poll_session_events(&host, "ask-dead", "tool/result");
        let rendered = format!("{:?}", results.last().expect("tool result logged"));
        assert!(
            rendered.contains("timed out"),
            "miss reaches the tool result: {rendered}"
        );
    });
}

/// Unattended guard: a scheduled run's session never reaches the question
/// bridge — the ask fails immediately (no 120s stall), no
/// `interaction/question` lands in the log, and the tool result tells the
/// model to decide autonomously.
#[test]
fn ask_user_in_scheduled_run_fails_fast_without_reaching_the_bridge() {
    with_temp_home(|dir| {
        let host = host_in(dir);
        let adapter = Arc::new(ask_user_e2e::AskUserAdapter {
            calls: std::sync::atomic::AtomicUsize::new(0),
            args_json: std::sync::Mutex::new(
                r#"{"question":"which location?","timeout_secs":120}"#.into(),
            ),
            saw_unattended: std::sync::atomic::AtomicBool::new(false),
        });
        host.debug_register_adapter(adapter.clone() as Arc<dyn starriver_kernel::llm::LlmAdapter>);

        // A real scheduled run whose prompt makes the scripted model ask.
        host.add_scheduled_task(
            "guardjob".into(),
            r#"{"type":"interval","hours":1}"#.into(),
            "ask me".into(),
            None,
            None,
            None,
            true,
            None,
            None,
            vec![],
        )
        .expect("add task");
        host.run_scheduled_task_now("guardjob".into()).expect("run");

        // Wait for the run to settle; its record carries the session id.
        let mut sid = None;
        for _ in 0..200 {
            let runs = host.get_task_run_history("guardjob".into());
            if let Some(rec) = runs.first() {
                if let Some(id) = rec.session_id.clone() {
                    sid = Some(id);
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let sid = sid.expect("scheduled run settled with a session id");

        // The question never reached the bridge: no pending modal event.
        let events = host.debug_session_log_event_data(sid.clone(), "interaction/question".into());
        assert!(
            events.is_empty(),
            "scheduled-run ask must not surface a question: {events:?}"
        );
        // The tool result reports the unattended miss instead.
        let results = poll_session_events(&host, &sid, "tool/result");
        let rendered = format!("{:?}", results.last().expect("tool result logged"));
        assert!(
            rendered.contains("unattended"),
            "fast-fail note reaches the tool result: {rendered}"
        );
        // And the assembled prompt carried the unattended contract section.
        assert!(
            adapter
                .saw_unattended
                .load(std::sync::atomic::Ordering::SeqCst),
            "scheduled run's prompt must carry the run:unattended section"
        );
    });
}

// ── Plan fold (plan-mode parity): exit_plan_mode end-to-end ──────────────────

pub mod plan_review_e2e {
    use futures::stream::BoxStream;
    use starriver_kernel::llm::{
        ContentBlock, FinishReason, GenerateOptions, LlmAdapter, LlmError, StreamChunk,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// Scripted adapter for the plan-fold slice: the first LLM call emits an
    /// `exit_plan_mode` tool call with the plan markdown; every later call
    /// returns final text (the turn continues after the review resolves).
    pub struct ExitPlanAdapter {
        pub calls: AtomicUsize,
        pub args_json: Mutex<String>,
    }

    impl LlmAdapter for ExitPlanAdapter {
        fn providers(&self) -> &[String] {
            static PROVIDERS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
            PROVIDERS.get_or_init(|| vec!["mock".to_string()])
        }

        fn stream(
            &self,
            _options: GenerateOptions,
        ) -> BoxStream<'static, Result<StreamChunk, LlmError>> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                let args = self.args_json.lock().unwrap().clone();
                Box::pin(futures::stream::iter(vec![
                    Ok(StreamChunk::ToolCallDelta {
                        index: 0,
                        id: starriver_kernel::llm::CallId("p1".into()),
                        name: Some("exit_plan_mode".into()),
                        arguments_delta: args.clone(),
                    }),
                    Ok(StreamChunk::BlockEnd {
                        index: 0,
                        block: ContentBlock::ToolCall {
                            id: starriver_kernel::llm::CallId("p1".into()),
                            name: Some("exit_plan_mode".into()),
                            arguments: starriver_kernel::json::JsonValue::from_json_str(&args)
                                .unwrap_or_else(|_| {
                                    starriver_kernel::json::JsonValue::String(args.to_string())
                                }),
                        },
                    }),
                    Ok(StreamChunk::Finish {
                        reason: FinishReason::ToolCalls,
                        replay_state: None,
                    }),
                ]))
            } else {
                Box::pin(futures::stream::iter(vec![
                    Ok(StreamChunk::TextDelta {
                        index: 0,
                        text: "executing".into(),
                    }),
                    Ok(StreamChunk::BlockEnd {
                        index: 0,
                        block: ContentBlock::Text("executing".into()),
                    }),
                    Ok(StreamChunk::Finish {
                        reason: FinishReason::Stop,
                        replay_state: None,
                    }),
                ]))
            }
        }
    }
} // mod plan_review_e2e

/// The entry → review → approve round trip: the Plan profile stamps the
/// `plan/mode {active}` entry + read-only pairing, the model's
/// `exit_plan_mode` call blocks on the review bridge, the UI's approval
/// resolves it, and the pending exit lands at the next accepted pre-step —
/// flip to inactive plus the workspace-write release, before the model's
/// next request runs.
#[test]
fn plan_review_approval_exits_plan_mode_and_releases_readonly() {
    with_temp_home(|dir| {
        let host = host_in(dir);
        host.set_execution_mode("plan".into()).expect("plan mode");
        host.debug_register_adapter(Arc::new(plan_review_e2e::ExitPlanAdapter {
            calls: std::sync::atomic::AtomicUsize::new(0),
            args_json: Mutex::new(r##"{"plan":"# Plan\n1. add the seam"}"##.into()),
        }));

        let mut rx = host
            .execute(
                "plan it".into(),
                Some("plan-t".into()),
                None,
                Some(starriver_host::CODING_AGENT_NAME.into()),
                None,
                None,
                None,
            )
            .expect("execute");

        // The review request lands in the session log (the Plan card's source).
        let mut request_id = None;
        for _ in 0..200 {
            let events =
                host.debug_session_log_event_data("plan-t".into(), "plan/review-requested".into());
            if let Some(data) = events.first() {
                request_id = data.as_str_at("id").map(String::from);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let request_id = request_id.expect("plan/review-requested in the session log");
        assert!(
            host.respond_plan_review("plan-t".into(), Some(request_id), true, None, None)
                .expect("valid preset"),
            "approval resolves the pending review"
        );
        wait_terminal(&host, rx);

        // The entry flip (active=true) and the approved exit (active=false)
        // are both in the log — the state is a pure fold of the last one.
        let flips = poll_session_events(&host, "plan-t", "plan/mode");
        let acts: Vec<bool> = flips
            .iter()
            .filter_map(|d| d.get("active").and_then(|b| b.as_bool()))
            .collect();
        assert_eq!(
            acts,
            vec![true, false],
            "entry 落账 then pre-step 落账 of the approved exit: {flips:?}"
        );

        // The read-only pairing is released — the LAST sandbox stamp is
        // workspace-write (the pre-step bundles the release).
        let modes = poll_session_events(&host, "plan-t", "sandbox/mode");
        let last = modes
            .last()
            .and_then(|d| d.as_str_at("mode"))
            .map(String::from)
            .expect("sandbox/mode events");
        assert_eq!(last, "workspace-write");

        // The audit pair is complete and the tool result reports approval.
        let decided = poll_session_events(&host, "plan-t", "plan/review-decided");
        assert_eq!(
            decided
                .first()
                .and_then(|d| d.get("approved").and_then(|b| b.as_bool())),
            Some(true)
        );
        let results = poll_session_events(&host, "plan-t", "tool/result");
        let rendered = format!("{:?}", results.last().expect("tool result logged"));
        assert!(
            rendered.contains("approved"),
            "approval reaches the result: {rendered}"
        );
    });
}

/// The review card's preset rides the decision: approving with
/// `danger-full-access` bundles BOTH knobs into the exit landing — the
/// sandbox releases to danger-full-access (not the conservative
/// workspace-write) and the approval folds to bypass — so the execution
/// phase runs without approval prompts, exactly as the card advertised.
#[test]
fn plan_review_approval_with_preset_lands_danger_full_bundle() {
    with_temp_home(|dir| {
        let host = host_in(dir);
        host.set_execution_mode("plan".into()).expect("plan mode");
        host.debug_register_adapter(Arc::new(plan_review_e2e::ExitPlanAdapter {
            calls: std::sync::atomic::AtomicUsize::new(0),
            args_json: Mutex::new(r##"{"plan":"# Plan\n1. add the seam"}"##.into()),
        }));

        let mut rx = host
            .execute(
                "plan it".into(),
                Some("plan-p".into()),
                None,
                Some(starriver_host::CODING_AGENT_NAME.into()),
                None,
                None,
                None,
            )
            .expect("execute");

        let mut request_id = None;
        for _ in 0..200 {
            let events =
                host.debug_session_log_event_data("plan-p".into(), "plan/review-requested".into());
            if let Some(data) = events.first() {
                request_id = data.as_str_at("id").map(String::from);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let request_id = request_id.expect("plan/review-requested in the session log");
        assert!(
            host.respond_plan_review(
                "plan-p".into(),
                Some(request_id),
                true,
                None,
                Some("danger-full-access".into()),
            )
            .expect("valid preset"),
            "approval resolves the pending review"
        );
        wait_terminal(&host, rx);

        // The landing bundled the preset: sandbox = danger-full-access.
        let modes = poll_session_events(&host, "plan-p", "sandbox/mode");
        let last = modes
            .last()
            .and_then(|d| d.as_str_at("mode"))
            .map(String::from)
            .expect("sandbox/mode events");
        assert_eq!(
            last, "danger-full-access",
            "preset sandbox survives: {modes:?}"
        );

        // …and approval = bypass, stamped by the landing itself.
        let policies = poll_session_events(&host, "plan-p", "approval/policy");
        let last_policy = policies
            .last()
            .and_then(|d| d.as_str_at("policy"))
            .map(String::from)
            .expect("approval/policy events");
        assert_eq!(
            last_policy, "bypass",
            "preset approval lands with the release: {policies:?}"
        );

        // The exit still flipped (the decision itself succeeded).
        let flips = poll_session_events(&host, "plan-p", "plan/mode");
        let acts: Vec<bool> = flips
            .iter()
            .filter_map(|d| d.get("active").and_then(|b| b.as_bool()))
            .collect();
        assert_eq!(acts, vec![true, false], "exit lands: {flips:?}");
    });
}

/// An invalid preset name fails the decision BEFORE resolving anything: the
/// review stays pending, no permission events land, and a subsequent call
/// with a valid preset resolves the SAME review.
#[test]
fn plan_review_invalid_preset_fails_without_resolving() {
    with_temp_home(|dir| {
        let host = host_in(dir);
        host.set_execution_mode("plan".into()).expect("plan mode");
        host.debug_register_adapter(Arc::new(plan_review_e2e::ExitPlanAdapter {
            calls: std::sync::atomic::AtomicUsize::new(0),
            args_json: Mutex::new(r##"{"plan":"# Plan"}"##.into()),
        }));

        let mut rx = host
            .execute(
                "plan it".into(),
                Some("plan-bad".into()),
                None,
                Some(starriver_host::CODING_AGENT_NAME.into()),
                None,
                None,
                None,
            )
            .expect("execute");

        let mut request_id = None;
        for _ in 0..200 {
            let events = host
                .debug_session_log_event_data("plan-bad".into(), "plan/review-requested".into());
            if let Some(data) = events.first() {
                request_id = data.as_str_at("id").map(String::from);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let request_id = request_id.expect("plan/review-requested in the session log");

        let error = host
            .respond_plan_review(
                "plan-bad".into(),
                Some(request_id.clone()),
                true,
                None,
                Some("rogue-preset".into()),
            )
            .expect_err("invalid preset must error");
        assert!(error.contains("rogue-preset"), "error: {error}");

        // A later call with a valid preset resolves the still-pending review.
        assert!(
            host.respond_plan_review(
                "plan-bad".into(),
                Some(request_id),
                true,
                None,
                Some("manual".into()),
            )
            .expect("valid preset"),
            "the review was NOT consumed by the failed call"
        );
        wait_terminal(&host, rx);

        // No rogue permission events ever landed; the preset=manual approval
        // releases the pairing to workspace-write (manual's sandbox).
        let modes = poll_session_events(&host, "plan-bad", "sandbox/mode");
        let last = modes
            .last()
            .and_then(|d| d.as_str_at("mode"))
            .map(String::from)
            .expect("sandbox/mode events");
        assert_eq!(last, "workspace-write");
    });
}

/// Rejection keeps the planning state: the failed call carries the user's
/// feedback to the model, the fold stays active, and the read-only pairing
/// holds.
#[test]
fn plan_review_rejection_keeps_plan_mode_and_carries_feedback() {
    with_temp_home(|dir| {
        let host = host_in(dir);
        host.set_execution_mode("plan".into()).expect("plan mode");
        host.debug_register_adapter(Arc::new(plan_review_e2e::ExitPlanAdapter {
            calls: std::sync::atomic::AtomicUsize::new(0),
            args_json: Mutex::new(r##"{"plan":"# Plan"}"##.into()),
        }));

        let mut rx = host
            .execute(
                "plan it".into(),
                Some("plan-r".into()),
                None,
                Some(starriver_host::CODING_AGENT_NAME.into()),
                None,
                None,
                None,
            )
            .expect("execute");

        let mut request_id = None;
        for _ in 0..200 {
            let events =
                host.debug_session_log_event_data("plan-r".into(), "plan/review-requested".into());
            if let Some(data) = events.first() {
                request_id = data.as_str_at("id").map(String::from);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let request_id = request_id.expect("plan/review-requested in the session log");
        assert!(host
            .respond_plan_review(
                "plan-r".into(),
                Some(request_id),
                false,
                Some("add a rollback step".into()),
                None,
            )
            .expect("valid (no) preset"));
        wait_terminal(&host, rx);

        // Only the entry flip exists — the fold stays active.
        let flips = poll_session_events(&host, "plan-r", "plan/mode");
        let acts: Vec<bool> = flips
            .iter()
            .filter_map(|d| d.get("active").and_then(|b| b.as_bool()))
            .collect();
        assert_eq!(acts, vec![true], "no exit flip on rejection: {flips:?}");

        // The read-only pairing holds.
        let modes = poll_session_events(&host, "plan-r", "sandbox/mode");
        let last = modes
            .last()
            .and_then(|d| d.as_str_at("mode"))
            .map(String::from)
            .expect("sandbox/mode events");
        assert_eq!(last, "read-only");

        // The failed call carries the feedback (: keep-planning is a
        // failed call, so the model revises and presents again).
        let results = poll_session_events(&host, "plan-r", "tool/result");
        let rendered = format!("{:?}", results.last().expect("tool result logged"));
        assert!(
            rendered.contains("add a rollback step") && rendered.contains("PLAN_REJECTED"),
            "feedback reaches the failed tool result: {rendered}"
        );
    });
}

/// WI-178: `exit_plan_mode` works in standard (non-plan) mode — the review
/// bridge prompts the desktop panel, an approval resumes the model with
/// `{ approved: true }`, and nothing plan-mode-shaped flips (no active=true
/// fold, sandbox stays workspace-write).
#[test]
fn exit_plan_mode_in_standard_mode_requests_review_and_approval_resumes() {
    with_temp_home(|dir| {
        let host = host_in(dir);
        host.debug_register_adapter(Arc::new(plan_review_e2e::ExitPlanAdapter {
            calls: std::sync::atomic::AtomicUsize::new(0),
            args_json: Mutex::new(r##"{"plan":"# Plan"}"##.into()),
        }));

        let mut rx = host
            .execute(
                "not planning".into(),
                Some("plan-off".into()),
                None,
                Some(starriver_host::CODING_AGENT_NAME.into()),
                None,
                None,
                None,
            )
            .expect("execute");

        // The review IS requested in standard mode now.
        let mut request_id = None;
        for _ in 0..200 {
            let events = host
                .debug_session_log_event_data("plan-off".into(), "plan/review-requested".into());
            if let Some(data) = events.first() {
                request_id = data.as_str_at("id").map(String::from);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let request_id = request_id.expect("plan/review-requested reaches the session log");
        assert!(host
            .respond_plan_review("plan-off".into(), Some(request_id), true, None, None)
            .expect("valid (no) preset"));
        wait_terminal(&host, rx);

        // The tool result carries the approval; no plan-mode flip to active.
        let results = poll_session_events(&host, "plan-off", "tool/result");
        let rendered = format!("{:?}", results.last().expect("tool result logged"));
        assert!(
            rendered.contains("approved"),
            "approval reaches the result: {rendered}"
        );
        let flips = poll_session_events(&host, "plan-off", "plan/mode");
        let acts: Vec<bool> = flips
            .iter()
            .filter_map(|d| d.get("active").and_then(|b| b.as_bool()))
            .collect();
        assert!(
            !acts.contains(&true),
            "standard mode stays inactive: {flips:?}"
        );
        let modes = poll_session_events(&host, "plan-off", "sandbox/mode");
        if let Some(last) = modes.last() {
            assert_eq!(
                last.as_str_at("mode"),
                Some("workspace-write"),
                "no read-only pairing outside plan mode"
            );
        }
    });
}

// ── A1-3 desktop multi-route wiring E2E ──────────────────────────────────

const RESPONSES_SSE: &str = concat!(
    "event: response.output_text.delta\n",
    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello from responses\"}\n\n",
    "event: response.output_text.done\n",
    "data: {\"type\":\"response.output_text.done\",\"text\":\"hello from responses\"}\n\n",
    "event: response.completed\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":3,\"output_tokens\":4}}}\n\n",
);

const COMPLETIONS_SSE: &str = concat!(
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"seen\"},\"finish_reason\":null}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2,\"total_tokens\":7}}\n\n",
    "data: [DONE]\n\n",
);

/// Serve one Responses-API SSE stream and capture the request line + body.
async fn serve_responses_capture() -> (u16, Arc<std::sync::Mutex<Option<(String, String)>>>) {
    serve_capture(RESPONSES_SSE).await
}

async fn serve_capture(
    response_sse: &'static str,
) -> (u16, Arc<std::sync::Mutex<Option<(String, String)>>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let captured: Arc<std::sync::Mutex<Option<(String, String)>>> = Arc::new(Default::default());
    let cap = Arc::clone(&captured);
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = Vec::new();
        let mut tmp = [0u8; 2048];
        let head_end = loop {
            let n = socket.read(&mut tmp).await.unwrap();
            if n == 0 {
                break 0;
            }
            buf.extend_from_slice(&tmp[..n]);
            if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                break pos + 4;
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
        let content_length = head
            .lines()
            .find_map(|line| {
                let lower = line.to_ascii_lowercase();
                lower
                    .strip_prefix("content-length:")
                    .and_then(|v| v.trim().parse::<usize>().ok())
            })
            .unwrap_or(0);
        while buf.len() < head_end + content_length {
            let n = socket.read(&mut tmp).await.unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
        }
        let body = String::from_utf8_lossy(&buf[head_end..head_end + content_length]).to_string();
        let request_line = head.lines().next().unwrap_or("").to_string();
        *cap.lock().unwrap() = Some((request_line, body));
        let response =
            format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n{response_sse}");
        let _ = socket.write_all(response.as_bytes()).await;
        let _ = socket.flush().await;
    });
    (port, captured)
}

#[test]
fn desktop_provider_route_reaches_the_wire_end_to_end() {
    // Isolate HOME: the host's model registry loads ~/.starriver/models.yaml
    // and a developer machine carries real providers there. with_temp_home
    // also serializes, keeping the HOME swap race-free.
    with_temp_home(|dir| {
        let dir = dir.to_path_buf();
        let config = make_config(&dir);
        let host =
            HostRuntime::new(Some(config.to_string_lossy().into_owned())).expect("host boots");
        assert_eq!(
            host.debug_default_provider().as_deref(),
            Some("mock"),
            "no route → mock default"
        );

        // Register a self-declaring kernel adapter route, then run a real
        // execute: the model-facing text must come from the stub through the
        // kernel loop.
        host.debug_register_adapter(Arc::new(stub_adapter::StubAdapter {
            provider: "e2e-prov".into(),
            reply: "hello from the route".into(),
            calls: Arc::new(std::sync::Mutex::new(Vec::new())),
        }));
        assert_eq!(
            host.debug_default_provider().as_deref(),
            Some("e2e-prov"),
            "registered route wins the default provider"
        );

        let mut rx = host
            .execute(
                "say something".into(),
                Some("route-e2e".into()),
                None,
                None,
                None,
                None,
                None,
            )
            .expect("execute");
        wait_terminal(&host, rx);

        // The agent is disposed right after the terminal event; its session
        // events land in durable storage a beat later. Poll briefly so the
        // assertion reads the persisted log instead of racing disposal.
        let headers = poll_session_events(&host, "route-e2e", "request/header");
        let header = headers.last().expect("request header must exist");
        let config_provider = header
            .get("config")
            .and_then(|c| c.get("provider"))
            .and_then(starriver_kernel::json::JsonValue::as_str);
        assert_eq!(
            config_provider,
            Some("e2e-prov"),
            "the request must ride the registered route: {header:?}"
        );

        let mut assistant_texts = Vec::new();
        let events = poll_session_events(&host, "route-e2e", "assistant/message");
        for data in events {
            if let Some(content) = data.get("message").and_then(|m| m.get("content")) {
                if let Some(blocks) = starriver_kernel::json::JsonValue::as_array(&content) {
                    for block in blocks {
                        if block
                            .get("type")
                            .and_then(starriver_kernel::json::JsonValue::as_str)
                            == Some("text")
                        {
                            if let Some(text) = block
                                .get("text")
                                .and_then(starriver_kernel::json::JsonValue::as_str)
                            {
                                assistant_texts.push(text.to_string());
                            }
                        }
                    }
                }
            }
        }
        assert!(
            assistant_texts
                .iter()
                .any(|text| text.contains("hello from the route")),
            "route engine output must reach the session: {assistant_texts:?}"
        );
    })
}

#[test]
fn legacy_models_migrate_into_llm_settings_slice_once() {
    with_temp_home(|dir| {
        // Pre-seed the legacy models.yaml the way older installs carry it.
        // Seeded at the EFFECTIVE HOME (the process-pinned tempdir under the
        // WI-48 test-speed fix), which is where HostRuntime actually reads.
        let starriver_dir =
            std::path::PathBuf::from(std::env::var("HOME").unwrap()).join(".starriver");
        std::fs::create_dir_all(&starriver_dir).unwrap();
        std::fs::write(
            starriver_dir.join("models.yaml"),
            "- id: ds\n  name: DeepSeek\n  type: cloud\n  provider_type: openai-compat\n  base_url: https://api.deepseek.com\n  api_key: \"!file:model/ds\"\n  default_model: deepseek-chat\n  api_format: openai-chat\n",
        )
        .unwrap();

        let host = host_in(dir);
        let models = host.list_models();
        assert_eq!(
            models.len(),
            1,
            "legacy entry must surface through list_models"
        );
        assert_eq!(models[0].id, "ds");
        assert_eq!(models[0].api_key, "!file:model/ds");

        // The route declaration now lives in settings.yaml's llm slice.
        let settings_yaml = std::fs::read_to_string(starriver_dir.join("settings.yaml"))
            .expect("settings.yaml written by migration");
        assert!(
            settings_yaml.contains("llm:") && settings_yaml.contains("providers:"),
            "settings must carry the llm namespace: {settings_yaml}"
        );
        assert!(settings_yaml.contains("!file:model/ds"), "{settings_yaml}");

        // New writes go to settings too (and survive a restart).
        host.add_model(
            "ds2".into(),
            "DeepSeek 2".into(),
            "cloud".into(),
            "openai-compat".into(),
            Some("https://api.deepseek.com".into()),
            Some("sk-2".into()),
            None,
            "deepseek-chat".into(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .expect("add second model");
        let reloaded = starriver_host::host_settings_llm_providers(dir);
        assert_eq!(reloaded.len(), 2);
        assert!(reloaded.iter().any(|p| p.id == "ds2"));
    });
}

#[test]
fn provider_directory_and_discovery_register_from_settings() {
    with_temp_home(|dir| {
        let host = host_in(dir);
        assert!(
            !host.debug_configurable_providers().is_empty(),
            "built-in provider presets declare the directory as dormant mounts"
        );

        host.add_model(
            "ds".into(),
            "DeepSeek".into(),
            "cloud".into(),
            "openai-compat".into(),
            Some("https://api.deepseek.com".into()),
            Some("sk-ds".into()),
            None,
            "deepseek-chat".into(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .expect("add model");
        assert_eq!(
            host.debug_configurable_providers()
                .iter()
                .filter(|p| *p == "ds")
                .count(),
            1,
            "configured provider must be declared in the directory"
        );

        // Discovery is registered (probe hits an unroutable endpoint, so the
        // error is a transport failure — NOT "no discovery is registered").
        let err = host
            .debug_model_discovery("llm.providers.ds".into())
            .expect_err("probe must fail to reach 127.0.0.1:1");
        assert!(
            !err.to_string().contains("NO_DISCOVERY"),
            "discovery must be registered: {err}"
        );

        // Remove → directory entry disappears.
        host.remove_model("ds".into()).expect("remove");
        assert!(
            !host
                .debug_configurable_providers()
                .contains(&"ds".to_string()),
            "removed provider leaves the directory (presets remain)"
        );
    });
}

#[test]
fn missing_credential_fails_the_run_with_missing_credential() {
    with_temp_home(|dir| {
        let host = host_in(dir);
        // A model with no API key at all: the route activates but every
        // request must fail with MISSING_CREDENTIAL (LLP D-C), instead of
        // sending an unauthenticated request or freezing an empty key.
        host.add_model(
            "nokey".into(),
            "NoKey".into(),
            "cloud".into(),
            "openai-compat".into(),
            Some("https://api.deepseek.com".into()),
            None,
            None,
            "deepseek-chat".into(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .expect("add no-key model");
        host.set_agent_active_model(
            starriver_host::CODING_AGENT_NAME.into(),
            "nokey".into(),
            "deepseek-chat".into(),
        )
        .expect("activate model");

        let mut rx = host
            .execute(
                "hello".into(),
                Some("missing-cred".into()),
                None,
                None,
                None,
                None,
                None,
            )
            .expect("execute");
        let handle = host.runtime_handle();
        let blob: String = handle.block_on(async move {
            let mut blob = String::new();
            for _ in 0..40 {
                match rx.recv().await {
                    Ok(ev) => {
                        blob.push_str(&ev.to_string());
                        if ev["type"] == "turnError" || ev["type"] == "turnEnd" {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            blob
        });
        assert!(
            blob.contains("MISSING_CREDENTIAL") || blob.contains("credential"),
            "run must surface the credential failure: {blob}"
        );
    });
}

#[test]
fn image_attachment_admission_stores_and_reads_back() {
    with_temp_home(|dir| {
        let host = host_in(dir);
        // A small opaque PNG → normalized (kept 64×32, re-encoded) and stored
        // content-addressed; read-back returns the scaled dimensions + bytes.
        let mut png = Vec::new();
        let img = image::RgbImage::from_pixel(64, 32, image::Rgb([10, 20, 30]));
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        let json = format!(r#"[{{"name":"photo.png","mediaType":"image/png","data":"{b64}"}}]"#);
        let refs = host.admit_image_attachments(json).expect("admit");
        assert_eq!(refs.len(), 1);
        assert!(refs[0].id.starts_with("sha256:"), "{}", refs[0].id);
        assert_eq!(refs[0].name.as_deref(), Some("photo.png"));
        assert_eq!((refs[0].width, refs[0].height), (64, 32));
        assert!(
            refs[0].original_width.is_none(),
            "no downscale for a small image"
        );

        let stored = host
            .read_image_attachment(refs[0].id.clone())
            .expect("read back");
        assert_eq!(stored.id, refs[0].id);
        assert_eq!(
            (stored.width, stored.height),
            (refs[0].width, refs[0].height)
        );
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&stored.base64)
            .expect("valid base64");
        assert!(!decoded.is_empty());

        // Non-canonical / oversized payloads are refused before any write.
        let bad = r#"[{"name":"x","mediaType":"image/png","data":"@@not-base64@@"}]"#;
        assert!(host.admit_image_attachments(bad.into()).is_err());
    });
}

#[test]
fn image_attachment_reaches_the_wire_as_inline_image_url() {
    with_temp_home(|dir| {
        let host = host_in(dir);
        let (port, captured) = host
            .runtime_handle()
            .block_on(serve_capture(COMPLETIONS_SSE));
        host.add_model(
            "ds".into(),
            "DeepSeek".into(),
            "cloud".into(),
            "openai-compat".into(),
            Some(format!("http://127.0.0.1:{port}")),
            Some("sk-ds".into()),
            None,
            "deepseek-chat".into(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .expect("add model");
        host.set_agent_active_model(
            starriver_host::CODING_AGENT_NAME.into(),
            "ds".into(),
            "deepseek-chat".into(),
        )
        .expect("activate model");

        // Admit one image through the attachment seam → durable ref.
        let mut png = Vec::new();
        let img = image::RgbImage::from_pixel(48, 32, image::Rgb([9, 8, 7]));
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        let refs = host
            .admit_image_attachments(format!(
                r#"[{{"name":"photo.png","mediaType":"image/png","data":"{b64}"}}]"#
            ))
            .expect("admit image");
        let ref_id = refs[0].id.clone();

        // Run one turn referencing the attachment; the wire body must carry an
        // inline image_url part (not OCR text, not a dropped image).
        let mut rx = host
            .execute(
                format!("![photo]({ref_id}) describe this"),
                Some("img-wire".into()),
                None,
                None,
                None,
                None,
                None,
            )
            .expect("execute");
        let handle = host.runtime_handle();
        handle.block_on(async move {
            for _ in 0..60 {
                match rx.recv().await {
                    Ok(ev) if ev["type"] == "turnError" || ev["type"] == "turnEnd" => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        });

        let (_, body) = captured
            .lock()
            .unwrap()
            .clone()
            .expect("mock server captured the request body");
        let v: serde_json::Value = serde_json::from_str(&body).expect("json body");
        let user = v["messages"]
            .as_array()
            .expect("messages array")
            .iter()
            .find(|m| m["role"] == "user")
            .expect("user message");
        let parts = user["content"].as_array().expect("content parts");
        let image = parts
            .iter()
            .find(|p| p["type"] == "image_url")
            .expect("image_url part on the wire");
        let url = image["image_url"]["url"].as_str().expect("image url");
        assert!(url.starts_with("data:image/"), "{url}");
    });
}

#[test]
fn credential_records_describe_set_unset_without_leaking_values() {
    with_temp_home(|dir| {
        let host = host_in(dir);

        // Register a model with a real key → the key lands in the managed
        // credential records, and the model entry carries the !file: ref.
        host.add_model(
            "ds".into(),
            "DeepSeek".into(),
            "cloud".into(),
            "openai-compat".into(),
            Some("https://api.deepseek.com".into()),
            Some("sk-first".into()),
            None,
            "deepseek-chat".into(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .expect("add model");

        let info = host
            .describe_credential("model/ds".into())
            .expect("describe");
        assert!(info.configured);
        assert_eq!(info.kind.as_deref(), Some("api-key"));
        assert!(info.writable);

        // Independent set overwrites the record and takes effect immediately.
        let reference = host
            .set_credential_api_key("model/ds".into(), "sk-second".into())
            .expect("set");
        assert_eq!(reference, "!file:model/ds");

        // Enumeration shows the record without values.
        let entries = host.list_credential_records();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key, "model/ds");

        // unset → gone everywhere.
        host.unset_credential("model/ds".into()).expect("unset");
        let info = host
            .describe_credential("model/ds".into())
            .expect("describe");
        assert!(!info.configured);

        // Invalid keys are rejected, never silently written.
        assert!(host.describe_credential("no-slash".into()).is_err());
        assert!(host
            .set_credential_api_key("Model/x".into(), "sk".into())
            .is_err());
    });
}

#[test]
fn configurable_provider_directory_surfaces_to_settings_ui() {
    with_temp_home(|dir| {
        let host = host_in(dir);

        // No configuration yet → the directory carries the dormant presets.
        let dormant: Vec<_> = host
            .list_configurable_providers()
            .into_iter()
            .filter(|p| p.provider == "deepseek")
            .collect();
        assert_eq!(dormant.len(), 1);
        assert!(
            !dormant[0].configured,
            "preset is declared but unconfigured"
        );

        // Register a model → its provider appears in the directory as
        // configured (the configuration face is the settings UI's source).
        host.add_model(
            "deepseek".into(),
            "DeepSeek".into(),
            "cloud".into(),
            "openai-compat".into(),
            Some("https://api.deepseek.com".into()),
            Some("sk-ds".into()),
            None,
            "deepseek-chat".into(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .expect("add model");
        let providers = host.list_configurable_providers();
        let deepseek = providers
            .iter()
            .find(|p| p.provider == "deepseek")
            .expect("deepseek in directory");
        assert_eq!(deepseek.display_name, "DeepSeek");
        assert_eq!(deepseek.settings_ns, "llm.providers.deepseek");
        assert_eq!(deepseek.settings_path, vec!["llm", "providers", "deepseek"]);
        assert!(deepseek.declared);
        assert!(deepseek.configured);

        // Removing the model keeps the provider declared but unconfigured:
        // the card stays visible as a dormant configuration entry.
        host.remove_model("deepseek".into()).expect("remove");
        let providers = host.list_configurable_providers();
        let deepseek = providers
            .iter()
            .find(|p| p.provider == "deepseek")
            .expect("deepseek stays declared");
        assert!(!deepseek.configured, "dormant mount stays in the directory");
    });
}

#[test]
fn subagent_delegation_config_and_job_panel_roundtrip() {
    with_temp_home(|dir| {
        let host = host_in(dir);

        // Subagent templates ship disabled by default.
        let configs = host.list_subagent_configs();
        assert!(configs.iter().any(|c| c.name == "explorer" && !c.enabled));
        assert!(configs.iter().any(|c| c.name == "reviewer" && c.read_only));
        // Nothing enabled → the model-facing tool description advertises none.
        let description = host
            .debug_subagent_tool_description()
            .expect("tool registered");
        assert!(
            description.contains("none enabled"),
            "description: {description}"
        );

        // Enable explorer and bind a child model route. The tool re-registers
        // (schema-layer parity): the description now advertises explorer
        // and only explorer.
        host.set_subagent_config("explorer".into(), true, Some("ds/deepseek-chat".into()))
            .expect("enable explorer");
        let description = host
            .debug_subagent_tool_description()
            .expect("tool registered");
        assert!(
            description.contains("explorer"),
            "description: {description}"
        );
        assert!(
            !description.contains("reviewer"),
            "description: {description}"
        );
        let explorer = host
            .list_subagent_configs()
            .into_iter()
            .find(|c| c.name == "explorer")
            .expect("explorer present");
        assert!(explorer.enabled);
        assert_eq!(explorer.model.as_deref(), Some("ds/deepseek-chat"));

        // Unknown templates are rejected, never silently created.
        assert!(host.set_subagent_config("nope".into(), true, None).is_err());

        // Disable again → entry disappears from the stored config and the
        // description stops advertising the template.
        host.set_subagent_config("explorer".into(), false, None)
            .expect("disable");
        let description = host
            .debug_subagent_tool_description()
            .expect("tool registered");
        assert!(
            description.contains("none enabled"),
            "description: {description}"
        );
        let explorer = host
            .list_subagent_configs()
            .into_iter()
            .find(|c| c.name == "explorer")
            .expect("explorer template still declared");
        assert!(!explorer.enabled);

        // Jobs: schedule + run now → the panel shows running, then settles.
        host.add_scheduled_task(
            "job1".into(),
            r#"{"type":"interval","hours":1}"#.into(),
            "do nothing".into(),
            None,
            None,
            None,
            true,
            None,
            None,
            vec![],
        )
        .expect("schedule job1");
        host.run_scheduled_task_now("job1".into()).expect("run now");

        for _ in 0..60 {
            let jobs = host.list_jobs();
            if let Some(job) = jobs.iter().find(|j| j.task_name == "job1") {
                if job.status != "running" {
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let jobs = host.list_jobs();
        let job = jobs
            .iter()
            .find(|j| j.task_name == "job1")
            .expect("job1 in the panel");
        assert!(
            job.status == "completed" || job.status == "failed",
            "settled job status: {}",
            job.status
        );
    });
}

#[test]
fn openai_responses_api_format_routes_to_the_responses_protocol() {
    with_temp_home(|dir| {
        let host = host_in(dir);
        // Spawn the mock server on the host runtime so it outlives this scope.
        let (port, captured) = host.runtime_handle().block_on(serve_responses_capture());

        host.add_model(
            "gpt5".into(),
            "GPT-5".into(),
            "cloud".into(),
            "openai-compat".into(),
            Some(format!("http://127.0.0.1:{port}")),
            Some("sk-test".into()),
            None,
            "gpt-5".into(),
            Some(r#"[{"id":"gpt-5","name":"gpt-5"}]"#.into()),
            None,
            None,
            None,
            None,
            Some("openai-responses".into()),
            None,
            None,
            None,
        )
        .expect("add model");
        host.set_agent_active_model(
            starriver_host::CODING_AGENT_NAME.into(),
            "gpt5".into(),
            "gpt-5".into(),
        )
        .expect("activate model");

        let mut rx = host
            .execute(
                "say something".into(),
                Some("resp-e2e".into()),
                None,
                Some(starriver_host::CODING_AGENT_NAME.into()),
                None,
                None,
                None,
            )
            .expect("execute");
        wait_terminal(&host, rx);

        let (request_line, body) = captured
            .lock()
            .unwrap()
            .clone()
            .expect("mock server must receive the request");
        assert!(
            request_line.contains("POST /responses"),
            "openai-responses must hit /responses: {request_line}"
        );
        let json: serde_json::Value = serde_json::from_str(&body).expect("request body json");
        assert_eq!(json["model"], "gpt-5");
        assert!(json["input"].is_array(), "responses uses typed input items");

        let events = poll_session_events(&host, "resp-e2e", "assistant/message");
        let texts: Vec<String> = events
            .iter()
            .filter_map(|data| data.get("message"))
            .filter_map(|message| message.get("content"))
            .filter_map(starriver_kernel::json::JsonValue::as_array)
            .flatten()
            .filter_map(|block| block.get("text"))
            .filter_map(starriver_kernel::json::JsonValue::as_str)
            .map(String::from)
            .collect();
        assert!(
            texts.iter().any(|t| t.contains("hello from responses")),
            "responses output must reach the session: {texts:?}"
        );
    })
}

#[test]
fn registered_cloud_model_route_becomes_default_provider() {
    // Regression: resolve_and_activate_model registered raw protocol adapters
    // that declare no provider ids, so the kernel silently dropped the route
    // and new agents fell back to provider `mock` — a Coding send then ended
    // in turnError "no adapter for provider `mock`" with the UI stuck on
    // "Thinking…". After the fix the activated model must own the route.
    with_temp_home(|dir| {
        let host = host_in(dir);
        host.add_model(
            "deepseek".into(),
            "DeepSeek".into(),
            "cloud".into(),
            "openai-compat".into(),
            Some("https://api.deepseek.com".into()),
            Some("sk-test".into()),
            None,
            "deepseek-v4-flash".into(),
            Some(r#"[{"id":"deepseek-v4-flash","name":"deepseek-v4-flash"}]"#.into()),
            None,
            None,
            None,
            None,
            Some("openai-chat".into()),
            None,
            None,
            None,
        )
        .expect("add model");
        host.set_agent_active_model(
            starriver_host::CODING_AGENT_NAME.into(),
            "deepseek".into(),
            "deepseek-v4-flash".into(),
        )
        .expect("activate model");
        assert_eq!(
            host.debug_default_provider().as_deref(),
            Some("deepseek"),
            "an activated cloud model must become the new-agent default provider"
        );
    })
}

#[test]
fn execute_does_not_leak_ui_subscriptions_into_followup_runs() {
    // Regression: the per-execute UI relay kept its bus subscription alive
    // after the turn (the tx-holding handlers outlived the terminal event),
    // so every later execute on the shared agent bus replayed its events into
    // previous receivers — the desktop UI showed assistant bubbles duplicating
    // once per prior send.
    with_temp_home(|dir| {
        let host = host_in(dir);
        let handle = host.runtime_handle();

        let mut rx1 = host
            .execute(
                "first".into(),
                None,
                None,
                Some(starriver_host::CODING_AGENT_NAME.into()),
                None,
                None,
                None,
            )
            .expect("execute1");
        handle.block_on(async {
            for _ in 0..40 {
                match rx1.recv().await {
                    Ok(ev) if ev["type"] == "turnError" || ev["type"] == "turnEnd" => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        });

        // Let the relay task dispose its subscription after the terminal event.
        std::thread::sleep(std::time::Duration::from_millis(300));

        let mut rx2 = host
            .execute(
                "second".into(),
                None,
                None,
                Some(starriver_host::CODING_AGENT_NAME.into()),
                None,
                None,
                None,
            )
            .expect("execute2");
        handle.block_on(async {
            for _ in 0..40 {
                match rx2.recv().await {
                    Ok(ev) if ev["type"] == "turnError" || ev["type"] == "turnEnd" => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        });

        match rx1.try_recv() {
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {}
            Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {}
            Ok(ev) => panic!("leaked subscription delivered a followup event: {ev:?}"),
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {
                panic!("receiver lagged; subscription still active")
            }
        }
    })
}

#[test]
fn session_messages_project_turn_tokens_duration_and_done_ts() {
    // Option 2 projection: each assistant/message's usage is folded onto the
    // turn's final text piece; duration = wall-clock window from the turn's
    // first event → turn/end; doneTs = the turn/end event time.
    let t = test_host();

    let mut user = sev(
        0,
        "user/message",
        msg_json(
            MessageRole::User,
            "m0",
            vec![ContentBlock::Text("task".into())],
        ),
        true,
    );
    user.time = 1_700_000_000_000;

    let mut step = sev(
        1,
        "assistant/message",
        JsonValue::Object(vec![
            (
                "message".into(),
                msg_json(
                    MessageRole::Assistant,
                    "m1",
                    vec![ContentBlock::ToolCall {
                        id: starriver_kernel::llm::CallId("c1".into()),
                        name: Some("bash".into()),
                        arguments: starriver_kernel::json::JsonValue::from_json_str(
                            r#"{"command":"ls"}"#,
                        )
                        .unwrap_or(
                            starriver_kernel::json::JsonValue::String(r#"{"command":"ls"}"#.into()),
                        ),
                    }],
                ),
            ),
            (
                "usage".into(),
                JsonValue::Object(vec![
                    ("inputTokens".into(), JsonValue::UInt(100)),
                    ("outputTokens".into(), JsonValue::UInt(40)),
                ]),
            ),
        ]),
        true,
    );
    step.time = 1_700_000_050_000;

    let mut final_msg = sev(
        2,
        "assistant/message",
        JsonValue::Object(vec![
            (
                "message".into(),
                msg_json(
                    MessageRole::Assistant,
                    "m2",
                    vec![ContentBlock::Text("done".into())],
                ),
            ),
            (
                "usage".into(),
                JsonValue::Object(vec![
                    ("inputTokens".into(), JsonValue::UInt(50)),
                    ("outputTokens".into(), JsonValue::UInt(20)),
                ]),
            ),
        ]),
        true,
    );
    final_msg.time = 1_700_000_100_000;

    let mut turn_end = sev(
        3,
        "turn/end",
        JsonValue::Object(vec![
            ("turn".into(), JsonValue::UInt(0)),
            (
                "reason".into(),
                JsonValue::Object(vec![("kind".into(), JsonValue::String("completed".into()))]),
            ),
        ]),
        false,
    );
    turn_end.time = 1_700_000_120_000;

    t.host
        .seed_test_log(
            "proj-1".into(),
            None,
            None,
            vec![user, step, final_msg, turn_end],
        )
        .expect("seed log");

    let messages = t.host.get_session_messages("proj-1".into());
    assert_eq!(messages.len(), 3, "user + tool + final text: {messages:?}");
    assert_eq!(messages[0].role, "user");
    assert_eq!(messages[1].role, "tool");

    let last = messages.last().unwrap();
    assert_eq!(last.role, "assistant");
    assert_eq!(last.content, "done");
    let usage = last
        .usage
        .as_ref()
        .expect("turn usage folded onto final piece");
    assert_eq!(
        usage.input_tokens, 150,
        "per-message usage sums across steps"
    );
    assert_eq!(usage.output_tokens, 60);
    assert_eq!(
        last.duration_ms,
        Some(120_000),
        "duration = first event → turn/end wall-clock window"
    );
    assert_eq!(last.done_ts, Some(1_700_000_120_000));
    // Tool pieces of intermediate steps do not carry the turn summary.
    assert!(messages[1].usage.is_none());
}

#[test]
fn session_messages_legacy_seed_keeps_duration_hides_tokens() {
    // Legacy logs: assistant/message has no usage projection field.
    // Duration/completion time still apply; the token section is hidden by decision.
    let t = test_host();

    let mut user = sev(
        0,
        "user/message",
        msg_json(
            MessageRole::User,
            "m0",
            vec![ContentBlock::Text("task".into())],
        ),
        true,
    );
    user.time = 1_700_000_000_000;
    let mut assistant = sev(
        1,
        "assistant/message",
        JsonValue::Object(vec![(
            "message".into(),
            msg_json(
                MessageRole::Assistant,
                "m1",
                vec![ContentBlock::Text("legacy reply".into())],
            ),
        )]),
        true,
    );
    assistant.time = 1_700_000_010_000;
    let mut turn_end = sev(
        2,
        "turn/end",
        JsonValue::Object(vec![("turn".into(), JsonValue::UInt(0))]),
        false,
    );
    turn_end.time = 1_700_000_030_000;

    t.host
        .seed_test_log(
            "legacy-1".into(),
            None,
            None,
            vec![user, assistant, turn_end],
        )
        .expect("seed log");

    let messages = t.host.get_session_messages("legacy-1".into());
    let last = messages.last().unwrap();
    assert_eq!(last.role, "assistant");
    assert!(last.usage.is_none(), "存量无 usage,Token 段隐藏");
    assert_eq!(last.duration_ms, Some(30_000));
    assert_eq!(last.done_ts, Some(1_700_000_030_000));
}

#[test]
fn session_messages_usage_anchor_survives_persistence_and_counts() {
    // A usage anchor (empty content + usage) still counts toward the turn
    // summary after a real persistence round-trip, and is not rendered in
    // the message list.
    let t = test_host();

    let mut user = sev(
        0,
        "user/message",
        msg_json(
            MessageRole::User,
            "m0",
            vec![ContentBlock::Text("task".into())],
        ),
        true,
    );
    user.time = 1_700_000_000_000;
    let mut anchor = sev(
        1,
        "assistant/message",
        JsonValue::Object(vec![
            (
                "message".into(),
                msg_json(MessageRole::Assistant, "m-anchor", vec![]),
            ),
            (
                "usage".into(),
                JsonValue::Object(vec![
                    ("inputTokens".into(), JsonValue::UInt(33)),
                    ("outputTokens".into(), JsonValue::UInt(4)),
                ]),
            ),
        ]),
        true,
    );
    anchor.time = 1_700_000_005_000;
    let mut final_msg = sev(
        2,
        "assistant/message",
        JsonValue::Object(vec![(
            "message".into(),
            msg_json(
                MessageRole::Assistant,
                "m1",
                vec![ContentBlock::Text("reply".into())],
            ),
        )]),
        true,
    );
    final_msg.time = 1_700_000_010_000;
    let mut turn_end = sev(
        3,
        "turn/end",
        JsonValue::Object(vec![("turn".into(), JsonValue::UInt(0))]),
        false,
    );
    turn_end.time = 1_700_000_020_000;

    t.host
        .seed_test_log(
            "anchor-1".into(),
            None,
            None,
            vec![user, anchor, final_msg, turn_end],
        )
        .expect("seed log");

    let messages = t.host.get_session_messages("anchor-1".into());
    assert_eq!(messages.len(), 2, "锚点不渲染: user + 最终文本");
    let usage = messages
        .last()
        .unwrap()
        .usage
        .as_ref()
        .expect("锚点 usage 计入回合汇总");
    assert_eq!((usage.input_tokens, usage.output_tokens), (33, 4));
}

mod usage_e2e {
    //! E2E: register a mock adapter that emits StreamChunk::Usage, run a real
    //! agent turn (adapter → assembler → kernel log → turn/end), then go
    //! through history projection and assert that duration/tokens/completion
    //! time are visible end to end.

    use futures::stream::{BoxStream, StreamExt};
    use starriver_kernel::llm::{
        ContentBlock, ContentBlockType, FinishReason, GenerateOptions, LlmAdapter, LlmError,
        StreamChunk, TokenUsage,
    };
    use std::sync::Arc;

    pub struct UsageStubAdapter {
        pub provider: String,
        pub reply: String,
    }

    impl LlmAdapter for UsageStubAdapter {
        fn providers(&self) -> &[String] {
            std::slice::from_ref(&self.provider)
        }

        fn stream(
            &self,
            _options: GenerateOptions,
        ) -> BoxStream<'static, Result<StreamChunk, LlmError>> {
            let reply = self.reply.clone();
            Box::pin(futures::stream::iter(vec![
                Ok(StreamChunk::BlockStart {
                    index: 0,
                    block_type: ContentBlockType::Text,
                }),
                Ok(StreamChunk::TextDelta {
                    index: 0,
                    text: reply.clone(),
                }),
                Ok(StreamChunk::BlockEnd {
                    index: 0,
                    block: ContentBlock::Text(reply),
                }),
                Ok(StreamChunk::Usage {
                    usage: TokenUsage {
                        input_tokens: 60,
                        output_tokens: 25,
                        cache_read_tokens: None,
                        cache_write_tokens: None,
                        reasoning_tokens: None,
                    },
                }),
                Ok(StreamChunk::Finish {
                    reason: FinishReason::Stop,
                    replay_state: None,
                }),
            ]))
        }
    }
}

#[test]
fn live_turn_persists_usage_and_projects_duration_for_history() {
    use starriver_host::CODING_AGENT_NAME;

    // with_temp_home/host_in (not test_host): isolated global settings so
    // the registered mock route becomes the default provider (same premise
    // as route-e2e).
    with_temp_home(|dir| {
        let t = TestHost {
            _dir: tempfile::TempDir::new().expect("tempdir"),
            host: host_in(dir),
            workdir: dir.join("workdir"),
        };
        t.host
            .debug_register_adapter(Arc::new(usage_e2e::UsageStubAdapter {
                provider: "usage-prov".into(),
                reply: "all done".into(),
            }));
        assert_eq!(
            t.host.debug_default_provider().as_deref(),
            Some("usage-prov"),
            "registered route must become the default provider"
        );

        let rx = t
            .host
            .execute(
                "run it".into(),
                Some("usage-e2e".into()),
                None,
                None,
                None,
                None,
                None,
            )
            .expect("execute");
        wait_terminal(&t.host, rx);

        // The agent disposes right after the terminal event; poll briefly so
        // the projection reads the durable log instead of racing disposal.
        let mut messages = Vec::new();
        for _ in 0..50 {
            messages = t.host.get_session_messages("usage-e2e".into());
            let done = messages
                .iter()
                .any(|m| m.role == "assistant" && m.usage.is_some() && m.done_ts.is_some());
            if done {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }

        let last = messages
            .last()
            .unwrap_or_else(|| panic!("history must not be empty"));
        assert_eq!(last.role, "assistant");
        assert_eq!(last.content, "all done");
        let usage = last
            .usage
            .as_ref()
            .expect("live turn must persist usage through adapter → log → projection");
        assert_eq!((usage.input_tokens, usage.output_tokens), (60, 25));
        let duration = last
            .duration_ms
            .expect("live turn must project a wall-clock duration");
        assert!(duration > 0, "duration must be positive, got {duration}");
        assert!(last.done_ts.is_some(), "turn/end time projects as doneTs");
    })
}

mod file_summary_e2e {
    //! E2E: the mock LLM first sends a real edit tool call (against a temp
    //! file); after approval goes through, the disk is changed; when the turn
    //! ends, go through history projection and assert file_changes
    //! (path/add/del/diff) are visible end to end.

    use futures::stream::BoxStream;
    use starriver_kernel::llm::{
        ContentBlock, ContentBlockType, FinishReason, GenerateOptions, LlmAdapter, LlmError,
        StreamChunk,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    pub struct ScriptedEditAdapter {
        pub calls: AtomicUsize,
        pub args_json: Mutex<String>,
    }

    impl LlmAdapter for ScriptedEditAdapter {
        fn providers(&self) -> &[String] {
            static PROVIDERS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
            PROVIDERS.get_or_init(|| vec!["mock".to_string()])
        }

        fn stream(
            &self,
            _options: GenerateOptions,
        ) -> BoxStream<'static, Result<StreamChunk, LlmError>> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                let args = self.args_json.lock().unwrap().clone();
                Box::pin(futures::stream::iter(vec![
                    Ok(StreamChunk::ToolCallDelta {
                        index: 0,
                        id: starriver_kernel::llm::CallId("c1".into()),
                        name: Some("edit".into()),
                        arguments_delta: args.clone(),
                    }),
                    Ok(StreamChunk::BlockEnd {
                        index: 0,
                        block: ContentBlock::ToolCall {
                            id: starriver_kernel::llm::CallId("c1".into()),
                            name: Some("edit".into()),
                            arguments: starriver_kernel::json::JsonValue::from_json_str(&args)
                                .unwrap_or_else(|_| {
                                    starriver_kernel::json::JsonValue::String(args.to_string())
                                }),
                        },
                    }),
                    Ok(StreamChunk::Finish {
                        reason: FinishReason::ToolCalls,
                        replay_state: None,
                    }),
                ]))
            } else {
                Box::pin(futures::stream::iter(vec![
                    Ok(StreamChunk::TextDelta {
                        index: 0,
                        text: "done".into(),
                    }),
                    Ok(StreamChunk::BlockEnd {
                        index: 0,
                        block: ContentBlock::Text("done".into()),
                    }),
                    Ok(StreamChunk::Finish {
                        reason: FinishReason::Stop,
                        replay_state: None,
                    }),
                ]))
            }
        }
    }
}

#[test]
fn live_edit_turn_persists_file_changes_for_history() {
    use starriver_host::CODING_AGENT_NAME;

    with_temp_home(|dir| {
        let workdir = dir.join("workdir");
        std::fs::create_dir_all(&workdir).expect("workdir");
        let target = workdir.join("note.txt");
        std::fs::write(&target, "one\ntwo\nthree\n").expect("seed file");

        let t = TestHost {
            _dir: tempfile::TempDir::new().expect("tempdir"),
            host: host_in(dir),
            workdir: workdir.clone(),
        };
        let args_json = format!(
            r#"{{"path":"{}","old_string":"two","new_string":"2"}}"#,
            target.display()
        );
        t.host
            .debug_register_adapter(Arc::new(file_summary_e2e::ScriptedEditAdapter {
                calls: std::sync::atomic::AtomicUsize::new(0),
                args_json: std::sync::Mutex::new(args_json),
            }));

        let rx = t
            .host
            .execute(
                "edit the note".into(),
                Some("file-e2e".into()),
                None,
                None,
                None,
                None,
                None,
            )
            .expect("execute");
        // edit needs approval: poll-and-approve (same pattern as the plugin_kernel approver).
        for _ in 0..500 {
            if t.host.approve("file-e2e".into(), true, None) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        wait_terminal(&t.host, rx);

        // The disk actually changed.
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "one\n2\nthree\n",
            "edit must apply to the real file"
        );

        // History projection: poll until it lands in durable storage.
        let mut files = None;
        for _ in 0..50 {
            let messages = t.host.get_session_messages("file-e2e".into());
            if let Some(piece) = messages
                .iter()
                .find(|m| m.role == "assistant" && m.file_changes.is_some())
            {
                files = piece.file_changes.clone();
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let files =
            files.unwrap_or_else(|| panic!("file changes must persist into history projection"));
        assert_eq!(files.len(), 1);
        assert!(
            files[0].path.ends_with("note.txt"),
            "path: {}",
            files[0].path
        );
        assert_eq!((files[0].add, files[0].del), (1, 1));
        assert!(files[0].diff.contains("-two"));
        assert!(files[0].diff.contains("+2"));
    });
}

#[test]
fn session_messages_project_file_changes_from_persisted_log() {
    // Integration: tool/call registry + tool/result diff text go through a real persistence round-trip.
    let t = test_host();

    let mut user = sev(
        0,
        "user/message",
        msg_json(
            MessageRole::User,
            "m0",
            vec![ContentBlock::Text("task".into())],
        ),
        true,
    );
    user.time = 1_700_000_000_000;
    let mut call = sev(
        1,
        "tool/call",
        JsonValue::Object(vec![
            ("turn".into(), JsonValue::UInt(0)),
            ("step".into(), JsonValue::UInt(1)),
            ("callId".into(), JsonValue::String("c1".into())),
            ("name".into(), JsonValue::String("edit".into())),
            ("arguments".into(), JsonValue::String("{}".into())),
        ]),
        false,
    );
    call.time = 1_700_000_010_000;
    let edit_output =
        "edited /tmp/a.txt\n\n--- /tmp/a.txt\n+++ /tmp/a.txt\n@@ -1,2 +1,2 @@\n old\n-old\n+new\n";
    let mut result = sev(
        2,
        "tool/result",
        JsonValue::Object(vec![
            ("turn".into(), JsonValue::UInt(0)),
            ("step".into(), JsonValue::UInt(1)),
            (
                "message".into(),
                msg_json(
                    MessageRole::User,
                    "tr",
                    vec![ContentBlock::ToolResult {
                        call_id: starriver_kernel::llm::CallId("c1".into()),
                        content: vec![ContentBlock::Text(edit_output.into())],
                        is_error: false,
                    }],
                ),
            ),
        ]),
        true,
    );
    result.time = 1_700_000_020_000;
    let mut assistant = sev(
        3,
        "assistant/message",
        JsonValue::Object(vec![(
            "message".into(),
            msg_json(
                MessageRole::Assistant,
                "m1",
                vec![ContentBlock::Text("done".into())],
            ),
        )]),
        true,
    );
    assistant.time = 1_700_000_025_000;
    let mut turn_end = sev(
        4,
        "turn/end",
        JsonValue::Object(vec![("turn".into(), JsonValue::UInt(0))]),
        false,
    );
    turn_end.time = 1_700_000_030_000;

    t.host
        .seed_test_log(
            "files-1".into(),
            None,
            None,
            vec![user, call, result, assistant, turn_end],
        )
        .expect("seed log");

    let messages = t.host.get_session_messages("files-1".into());
    let last = messages.last().unwrap();
    assert_eq!(last.role, "assistant");
    let files = last.file_changes.as_ref().expect("file changes projected");
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].path, "/tmp/a.txt");
    assert_eq!((files[0].add, files[0].del), (1, 1));
    assert!(files[0].diff.contains("@@ -1,2 +1,2 @@"));
}

mod vision_degrade_e2e {
    //! Image gating (dsh pattern): a declared text-only route fails the turn
    //! at request assembly with an explicit error; undeclared routes ride
    //! images and the provider's own rejection (CapturingAdapter's
    //! `reject_images`) is the arbiter.

    use base64::Engine as _;
    use futures::stream::{BoxStream, StreamExt};
    use starriver_kernel::llm::{
        ContentBlock, FinishReason, GenerateOptions, LlmAdapter, LlmError, Message, StreamChunk,
    };
    use std::sync::{Arc, Mutex};

    pub struct CapturingAdapter {
        pub provider: String,
        pub captured: Arc<Mutex<Vec<Vec<Message>>>>,
        /// Fail any request carrying an Image block with the exact
        /// GLM/bigmodel validation error (code 1210).
        pub reject_images: bool,
    }

    impl LlmAdapter for CapturingAdapter {
        fn providers(&self) -> &[String] {
            std::slice::from_ref(&self.provider)
        }

        fn stream(
            &self,
            options: GenerateOptions,
        ) -> BoxStream<'static, Result<StreamChunk, LlmError>> {
            self.captured.lock().unwrap().push(options.messages.clone());
            if self.reject_images
                && options.messages.iter().any(|m| {
                    m.content
                        .iter()
                        .any(|b| matches!(b, ContentBlock::Image { .. }))
                })
            {
                return Box::pin(futures::stream::iter(vec![Err(LlmError::Call {
                    code: "INVALID_REQUEST".into(),
                    message: "{\"error\":{\"code\":\"1210\",\"message\":\"messages.content.type 参数非法，取值范围 ['text']\"}}".into(),
                    status: Some(400),
                    provider_retry_after_ms: None,
                    request_id: None,
                })]));
            }
            Box::pin(futures::stream::iter(vec![
                Ok(StreamChunk::TextDelta {
                    index: 0,
                    text: "ok".into(),
                }),
                Ok(StreamChunk::BlockEnd {
                    index: 0,
                    block: ContentBlock::Text("ok".into()),
                }),
                Ok(StreamChunk::Finish {
                    reason: FinishReason::Stop,
                    replay_state: None,
                }),
            ]))
        }
    }

    pub fn png_base64() -> String {
        let img = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            2,
            2,
            image::Rgb([255, 0, 0]),
        ));
        let mut png = std::io::Cursor::new(Vec::new());
        img.write_to(&mut png, image::ImageFormat::Png).unwrap();
        base64::engine::general_purpose::STANDARD.encode(png.get_ref())
    }
}

#[test]
fn vision_model_still_receives_image_blocks() {
    use starriver_host::CODING_AGENT_NAME;
    with_temp_home(|dir| {
        let host = host_in(dir);
        host.add_model(
            "vision-prov".into(),
            "Vision".into(),
            "cloud".into(),
            "openai-compat".into(),
            Some("https://example.com".into()),
            Some("k".into()),
            None,
            "m1".into(),
            None,
            None,
            None,
            None,
            None,
            Some("openai-chat".into()),
            None,
            None,
            Some(true),
        )
        .expect("add model");
        let captured = Arc::new(Mutex::new(Vec::new()));
        host.debug_register_adapter(Arc::new(vision_degrade_e2e::CapturingAdapter {
            provider: "vision-prov".into(),
            captured: Arc::clone(&captured),
            reject_images: false,
        }));

        let admitted = host
            .admit_image_attachments(format!(
                r#"[{{"name":"shot.png","mediaType":"image/png","data":"{}"}}]"#,
                vision_degrade_e2e::png_base64()
            ))
            .expect("admit");
        let ref_id = admitted[0].id.clone();

        let rx = host
            .execute(
                format!("look at ![shot]({ref_id})"),
                Some("vision-ok".into()),
                None,
                None,
                None,
                None,
                None,
            )
            .expect("execute");
        wait_terminal(&host, rx);

        let guard = captured.lock().unwrap();
        assert!(!guard.is_empty(), "adapter must have been called");
        let has_image = guard.iter().any(|messages| {
            messages.iter().any(|m| {
                m.content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::Image { .. }))
            })
        });
        assert!(has_image, "支持视觉的模型必须收到 Image 块");
    });
}

/// dsh pattern: a variant declared text-only (`supports_vision: false`)
/// fails the turn at request assembly — the adapter is never called and the
/// error names the model. The image stays in the logged message (history
/// renders it); nothing is silently stripped.
#[test]
fn declared_text_only_model_rejects_image_turn() {
    use starriver_host::CODING_AGENT_NAME;
    with_temp_home(|dir| {
        let host = host_in(dir);
        let variants =
            serde_json::json!([{ "id": "m1", "name": "M1", "supports_vision": false }]).to_string();
        host.add_model(
            "text-prov".into(),
            "Text".into(),
            "cloud".into(),
            "openai-compat".into(),
            Some("https://example.com".into()),
            Some("k".into()),
            None,
            "m1".into(),
            Some(variants),
            None,
            None,
            None,
            None,
            Some("openai-chat".into()),
            None,
            None,
            None,
        )
        .expect("add model");
        host.set_agent_active_model(CODING_AGENT_NAME.into(), "text-prov".into(), "m1".into())
            .expect("bind text-only model");
        assert!(
            host.model_declares_text_only("text-prov", "m1"),
            "declaration must resolve before the run"
        );
        let captured = Arc::new(Mutex::new(Vec::new()));
        host.debug_register_adapter(Arc::new(vision_degrade_e2e::CapturingAdapter {
            provider: "text-prov".into(),
            captured: Arc::clone(&captured),
            reject_images: false,
        }));

        let admitted = host
            .admit_image_attachments(format!(
                r#"[{{"name":"shot.png","mediaType":"image/png","data":"{}"}}]"#,
                vision_degrade_e2e::png_base64()
            ))
            .expect("admit");
        let ref_id = admitted[0].id.clone();

        let rx = host
            .execute(
                format!("look at ![shot]({ref_id})"),
                Some("text-only-reject".into()),
                None,
                Some(CODING_AGENT_NAME.into()),
                None,
                None,
                None,
            )
            .expect("execute");
        let handle = host.runtime_handle();
        let terminal = handle.block_on(async move {
            let mut rx = rx;
            let mut terminal = "<no terminal event>".to_string();
            for _ in 0..200 {
                match rx.recv().await {
                    Ok(ev) => {
                        let ty = ev["type"].as_str().unwrap_or_default();
                        if ty == "turnError" || ty == "TaskFailed" || ty == "turnEnd" {
                            terminal = ev.to_string();
                            break;
                        }
                    }
                    Err(_) => {
                        terminal = "<channel closed>".to_string();
                        break;
                    }
                }
            }
            terminal
        });
        assert!(
            terminal.contains("纯文本") && terminal.contains("m1"),
            "terminal event must refuse images and name the model, got: {terminal}"
        );
        let guard = captured.lock().unwrap();
        assert!(
            guard.is_empty(),
            "adapter must never be called, got {} requests",
            guard.len()
        );
    });
}

/// Execute once and wait for the turn to end (terminates with turnError/turnEnd under an empty engine).
fn execute_and_wait(t: &TestHost, task: &str, sid: &str) {
    let rx = t
        .host
        .execute(task.into(), Some(sid.into()), None, None, None, None, None)
        .expect("execute");
    let handle = t.host.runtime_handle();
    handle.block_on(async move {
        let mut rx = rx;
        for _ in 0..200 {
            match rx.recv().await {
                Ok(ev) => {
                    let done = ev["type"] == "turnError"
                        || ev["type"] == "turnEnd"
                        || ev["type"] == "TaskFailed";
                    if done {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    for _ in 0..100 {
        if !t.host.is_session_running(sid) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

fn last_policy(host: &HostRuntime, sid: &str) -> Option<String> {
    poll_session_events(host, sid, "approval/policy")
        .last()
        .and_then(|p| p.as_str_at("policy").map(String::from))
}

fn last_sandbox(host: &HostRuntime, sid: &str) -> Option<String> {
    poll_session_events(host, sid, "sandbox/mode")
        .last()
        .and_then(|p| p.as_str_at("mode").map(String::from))
}

/// Plan strict lock: setApprovalMode is rejected while the session's plan is
/// in planning; a session without a plan log is never locked, and an approved
/// exit (confirm) releases the lock (WI-144).
#[test]
fn approval_mode_locked_to_manual_in_plan_mode() {
    with_temp_home(|_| {
        let t = test_host();
        t.host.set_execution_mode("plan".into()).expect("plan");
        // No plan log → nothing is locked, even under the Plan deployment mode.
        t.host
            .set_approval_mode("s1".into(), "manual".into())
            .expect("session without a plan log is not locked");
        // A planning session is locked.
        execute_and_wait(&t, "plan the work", "s1");
        assert_eq!(last_policy(&t.host, "s1").as_deref(), Some("manual"));
        let err = t
            .host
            .set_approval_mode("s1".into(), "auto".into())
            .unwrap_err();
        assert!(err.to_string().contains("locked"), "{err}");
        // The approved exit releases the lock (confirm stamps may lag behind
        // the write-behind chain — retry until the write is accepted).
        t.host.confirm_plan("s1".into()).expect("confirm");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if t.host.set_approval_mode("s1".into(), "auto".into()).is_ok() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "approval write never unlocked after confirm"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    });
}

/// Presets are stamped only on session creation; resume preserves the user's per-session approval choice.
#[test]
fn execution_mode_presets_stamp_on_create_not_resume() {
    with_temp_home(|_| {
        let t = test_host();
        t.host.set_execution_mode("ptc".into()).expect("ptc");
        let sid = "preset-s1";
        execute_and_wait(&t, "go", sid);
        assert_eq!(last_policy(&t.host, sid).as_deref(), Some("auto"));

        // Under ptc the user switches to manual. Cold-session appends go
        // through the prepare/write-behind chain; an occasional race can
        // silently drop the event (set_approval_mode ignores append errors) —
        // poll and retry until the event is visible, to avoid reporting an
        // infrastructure race as a semantic regression.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let _ = t.host.set_approval_mode(sid.into(), "manual".into());
            if last_policy(&t.host, sid).as_deref() == Some("manual") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "manual policy never landed in the session log"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        // Resume after switching execution mode: must not re-stamp over the user's choice
        t.host
            .set_execution_mode("standard".into())
            .expect("standard");
        execute_and_wait(&t, "again", sid);
        assert_eq!(
            last_policy(&t.host, sid).as_deref(),
            Some("manual"),
            "resume must not restamp the preset over the user's per-session choice"
        );
    });
}

/// Plan force-stamps every turn: approval is manual during planning; sandbox
/// is read-only during planning and workspace-write after confirm. WI-144:
/// once the plan is confirmed the approval knob unlocks — the next turn
/// re-stamps the user's explicit choice instead of forcing manual.
#[test]
fn plan_mode_stamps_manual_and_honors_confirm_unlock() {
    with_temp_home(|_| {
        let t = test_host();
        t.host.set_execution_mode("plan".into()).expect("plan");
        let sid = "plan-s1";
        execute_and_wait(&t, "plan the work", sid);
        assert_eq!(last_policy(&t.host, sid).as_deref(), Some("manual"));
        assert_eq!(last_sandbox(&t.host, sid).as_deref(), Some("read-only"));

        // Changing approval under Plan is rejected
        assert!(t.host.set_approval_mode(sid.into(), "auto".into()).is_err());

        // confirmPlan unlock → next turn sandbox is workspace-write; with no
        // explicit choice yet the folded policy (still manual) is re-stamped.
        // Cold-session event appends go through the prepare/write-behind chain
        // and are occasionally dropped silently (same race as the presets test
        // above): retry confirm + turn as a whole until the unlock stamp is visible.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            t.host.confirm_plan(sid.into()).expect("confirm plan");
            execute_and_wait(&t, "execute the plan", sid);
            if last_sandbox(&t.host, sid).as_deref() == Some("workspace-write") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "confirm unlock must survive the next turn"
            );
        }
        assert_eq!(last_policy(&t.host, sid).as_deref(), Some("manual"));

        // WI-144: after the approved exit the approval knob unlocks — an
        // explicit choice survives the next turn's re-stamp instead of being
        // clobbered back to manual. Confirm stamps may lag behind the
        // write-behind chain, so retry the write until it is accepted.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if t.host.set_approval_mode(sid.into(), "auto".into()).is_ok() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "approval write never unlocked after confirm"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        // The next turn re-stamps the folded policy (the user's choice), not manual.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            execute_and_wait(&t, "execute the plan", sid);
            if last_policy(&t.host, sid).as_deref() == Some("auto") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "post-confirm turns must keep the user's approval choice (WI-144)"
            );
        }
    });
}

/// The review card's preset survives turn restarts: after an explicit
/// danger-full-access choice in the confirmed (execution) phase, the
/// turn-start stamp must re-stamp the folded sandbox instead of clobbering
/// it back to workspace-write (Plan's allowed set covers danger-full).
#[test]
fn plan_confirmed_turn_stamp_preserves_danger_full_sandbox() {
    with_temp_home(|_| {
        let t = test_host();
        t.host.set_execution_mode("plan".into()).expect("plan");
        let sid = "plan-danger";
        execute_and_wait(&t, "plan the work", sid);
        t.host.confirm_plan(sid.into()).expect("confirm plan");
        execute_and_wait(&t, "execute the plan", sid);
        assert_eq!(
            last_sandbox(&t.host, sid).as_deref(),
            Some("workspace-write")
        );

        // Execution-phase explicit choice: accepted (past the planning lock)
        // and must survive the next turn's stamp.
        t.host
            .set_permission_preset(sid.into(), "danger-full-access".into())
            .expect("execution-phase preset write");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            execute_and_wait(&t, "execute the plan", sid);
            if last_sandbox(&t.host, sid).as_deref() == Some("danger-full-access")
                && last_policy(&t.host, sid).as_deref() == Some("bypass")
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "danger-full-access preset must survive turn restarts"
            );
        }
    });
}

/// Plan residue unlock (2026-09-02 game002): sessions created during Plan
/// carry a read-only stamp; after switching back to Standard and resuming
/// the same session, the next turn must re-stamp workspace-write per the
/// current preset — otherwise the Plan confirmation card (rendered only
/// when mode==='plan') never appears and the session is permanently locked
/// read-only (observed: plan stamped at 09:49, still read-only under
/// Standard after restart).
#[test]
fn resumed_session_drops_stale_read_only_stamp_under_standard() {
    with_temp_home(|_| {
        let t = test_host();
        t.host.set_execution_mode("plan".into()).expect("plan");
        let sid = "stale-ro-s1";
        execute_and_wait(&t, "plan the work", sid);
        assert_eq!(last_sandbox(&t.host, sid).as_deref(), Some("read-only"));

        // After switching back to Standard, resume the same session (no new
        // one): the residual read-only stamp is refreshed.
        t.host
            .set_execution_mode("standard".into())
            .expect("standard");
        execute_and_wait(&t, "continue in standard", sid);
        // Write-behind window is 200ms: turn-2's re-stamp may still be
        // pending; a persistence barrier must pass before asserting
        // (independent of the registry/storage dual paths racing the
        // assertion's read point).
        t.host.debug_persistence_flush().expect("flush");
        assert_eq!(
            last_sandbox(&t.host, sid).as_deref(),
            Some("workspace-write"),
            "stale plan-era read-only stamp must be refreshed to the current preset"
        );
    });
}

/// Unknown execution mode → rejected and not persisted.
#[test]
fn set_execution_mode_rejects_unknown_mode() {
    with_temp_home(|_| {
        let t = test_host();
        let err = t.host.set_execution_mode("bogus".into()).unwrap_err();
        assert!(err.to_string().contains("unknown execution mode"), "{err}");
        // Settings not polluted (still the default standard).
        let settings_path = std::env::var("HOME").unwrap() + "/.starriver/settings.yaml";
        let persisted = std::fs::read_to_string(settings_path).unwrap_or_default();
        assert!(
            !persisted.contains("execution_mode: bogus"),
            "rejected mode must not be persisted: {persisted}"
        );
    });
}

/// set_execution_mode persists to ~/.starriver/settings.yaml (the source of truth that survives restarts).
#[test]
fn set_execution_mode_persists_to_settings_file() {
    with_temp_home(|_| {
        let t = test_host();
        t.host.set_execution_mode("goal".into()).expect("goal");
        let settings_path = std::env::var("HOME").unwrap() + "/.starriver/settings.yaml";
        let persisted = std::fs::read_to_string(&settings_path)
            .expect("settings file must exist after set_execution_mode");
        assert!(
            persisted.contains("execution_mode: goal"),
            "mode must be persisted: {persisted}"
        );
    });
}

/// The UI choosing Standard (sending an empty string) must be able to clear
/// the persisted plan: otherwise the kernel stamps every new session
/// read-only each turn — the sandbox stays read-only forever while the UI
/// shows Standard (2026-09-02 bug regression).
#[test]
fn set_execution_mode_empty_clears_persisted_plan_lock() {
    with_temp_home(|_| {
        let t = test_host();
        t.host.set_execution_mode("plan".into()).expect("plan");
        let sid = "clear-s1";
        execute_and_wait(&t, "go", sid);
        assert_eq!(last_sandbox(&t.host, sid).as_deref(), Some("read-only"));

        // Dropdown switches back to Standard (preload normalizes null to standard/empty string)
        t.host
            .set_execution_mode("".into())
            .expect("clear to standard");
        assert_eq!(t.host.get_execution_mode(None), "standard");

        // The new session restores the workspace-write preset (approval manual — the A-1 all-Manual policy)
        let sid2 = "clear-s2";
        execute_and_wait(&t, "go", sid2);
        assert_eq!(
            last_sandbox(&t.host, sid2).as_deref(),
            Some("workspace-write")
        );
        assert_eq!(last_policy(&t.host, sid2).as_deref(), Some("manual"));
    });
}

/// getExecutionMode echoes the persisted state (source of truth for UI mount sync; empty string normalizes to standard).
#[test]
fn get_execution_mode_reflects_persisted_mode() {
    with_temp_home(|_| {
        let t = test_host();
        assert_eq!(
            t.host.get_execution_mode(None),
            "standard",
            "缺省规范化为 standard"
        );
        t.host.set_execution_mode("ptc".into()).unwrap();
        assert_eq!(t.host.get_execution_mode(None), "ptc");
        t.host.set_execution_mode("goal".into()).unwrap();
        assert_eq!(t.host.get_execution_mode(None), "goal");
        t.host.set_execution_mode("".into()).unwrap();
        assert_eq!(t.host.get_execution_mode(None), "standard");
    });
}

/// Permission preset switch (permission-presets parity): one call writes
/// BOTH knob events, unknown presets are rejected, and the fold derives the
/// preset name back. danger-full-access needs no governance authorization —
/// the high-risk confirmation lives in the UI.
#[test]
fn permission_preset_roundtrip_writes_both_knobs_and_derives() {
    with_temp_home(|_| {
        let t = test_host();

        // Unknown preset → rejected outright.
        let err = t
            .host
            .set_permission_preset("perm-t".into(), "nope".into())
            .unwrap_err();
        assert!(
            err.to_string().contains("unknown permission preset"),
            "{err}"
        );

        // Create a real session (a turn with no engine still persists the log).
        let rx = t
            .host
            .execute(
                "hello".into(),
                Some("perm-t".into()),
                None,
                None,
                None,
                None,
                None,
            )
            .expect("execute accepts");
        wait_terminal(&t.host, rx);

        // auto preset → both events land and the fold derives the preset.
        t.host
            .set_permission_preset("perm-t".into(), "auto".into())
            .expect("auto preset");
        let modes = poll_session_events(&t.host, "perm-t", "sandbox/mode");
        assert_eq!(
            modes.last().and_then(|d| d.as_str_at("mode")),
            Some("workspace-write")
        );
        let policies = poll_session_events(&t.host, "perm-t", "approval/policy");
        assert_eq!(
            policies.last().and_then(|d| d.as_str_at("policy")),
            Some("auto")
        );
        let perm = t
            .host
            .get_permission("perm-t".into())
            .expect("get_permission");
        assert_eq!(perm["sandbox"], "workspace-write");
        assert_eq!(perm["approval"], "auto");
        assert_eq!(perm["preset"], "auto");

        // The danger preset needs no governance toggle (the risk confirmation
        // lives in the UI) — both knobs land.
        t.host
            .set_permission_preset("perm-t".into(), "danger-full-access".into())
            .expect("danger preset must not be governance-gated");
        let perm = t
            .host
            .get_permission("perm-t".into())
            .expect("get_permission after danger");
        assert_eq!(perm["sandbox"], "danger-full-access");
        assert_eq!(perm["approval"], "bypass");
        assert_eq!(perm["preset"], "danger-full-access");
    });
}

/// BUG-113 production shape: the composer hands the permission preset to the
/// creation call itself. The fresh session must be stamped BEFORE the first
/// turn runs (fold derives the picked preset), and a preset riding a
/// CONTINUING execute must be ignored — the session keeps its knobs
/// (mid-session switches go through session.setPermissionPreset).
#[test]
fn permission_preset_creation_binding_steers_the_session() {
    with_temp_home(|_| {
        let t = test_host();

        // New session carrying the composer's pick: the fold must derive it.
        let rx = t
            .host
            .execute(
                "hello".into(),
                None,
                None,
                None,
                None,
                None,
                Some("danger-full-access".into()),
            )
            .expect("execute accepts");
        wait_terminal(&t.host, rx);

        let sid = {
            let sessions = t.host.list_sessions(None);
            assert_eq!(sessions.len(), 1, "one fresh session expected");
            sessions[0].id.clone()
        };
        let perm = t.host.get_permission(sid.clone()).expect("get_permission");
        assert_eq!(
            perm["sandbox"], "danger-full-access",
            "fold after creation binding: {perm}"
        );
        assert_eq!(perm["approval"], "bypass");
        assert_eq!(perm["preset"], "danger-full-access");

        // Continuing session: a preset riding execute is ignored — the
        // session keeps its creation-time knobs.
        let rx = t
            .host
            .execute(
                "again".into(),
                Some(sid.clone()),
                None,
                None,
                None,
                None,
                Some("manual".into()),
            )
            .expect("continuing execute accepts");
        wait_terminal(&t.host, rx);
        let perm = t
            .host
            .get_permission(sid.clone())
            .expect("get_permission after ignored preset");
        assert_eq!(
            perm["preset"], "danger-full-access",
            "continuation must keep its binding: {perm}"
        );

        // Unknown preset names fail loudly instead of silently defaulting.
        let err = t
            .host
            .execute(
                "third".into(),
                None,
                None,
                None,
                None,
                None,
                Some("nope".into()),
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("unknown permission preset"),
            "{err}"
        );
    });
}

/// A knob combo outside the preset table folds to the derived `custom` state
/// (preset: null over the wire). The executed session is pinned
/// (workspace-write + manual); switching approval alone to bypass leaves
/// (workspace-write, bypass) — reachable via the raw approval API, not from
/// the preset picker — which derives to custom.
#[test]
fn permission_fold_reports_custom_for_non_table_combo() {
    with_temp_home(|_| {
        let t = test_host();
        // Create the session first (cold appends require an existing log).
        let rx = t
            .host
            .execute(
                "hello".into(),
                Some("perm-c".into()),
                None,
                None,
                None,
                None,
                None,
            )
            .expect("execute accepts");
        wait_terminal(&t.host, rx);

        // The pinned default derives to the manual preset.
        let perm = t
            .host
            .get_permission("perm-c".into())
            .expect("get_permission");
        assert_eq!(perm["preset"], "manual");

        t.host
            .set_approval_mode("perm-c".into(), "bypass".into())
            .expect("approval switch");
        let perm = t
            .host
            .get_permission("perm-c".into())
            .expect("get_permission after switch");
        assert_eq!(perm["sandbox"], "workspace-write");
        assert_eq!(perm["approval"], "bypass");
        assert_eq!(
            perm["preset"],
            serde_json::Value::Null,
            "non-table combo is custom"
        );

        // Unknown session → null knobs, no error.
        let perm = t
            .host
            .get_permission("perm-missing".into())
            .expect("missing session tolerates");
        assert_eq!(perm["sandbox"], serde_json::Value::Null);
        assert_eq!(perm["preset"], serde_json::Value::Null);
    });
}

/// Unknown approval mode → rejected; the four values parse and persist
/// without any governance gate (the danger preset's gate is the UI
/// confirmation).
#[test]
fn set_approval_mode_rejects_unknown_mode_but_allows_known_modes() {
    with_temp_home(|_| {
        let t = test_host();
        let err = t
            .host
            .set_approval_mode("gov-s2".into(), "nope".into())
            .unwrap_err();
        assert!(err.to_string().contains("unknown approval mode"), "{err}");
        for mode in ["manual", "accept-edits", "auto", "bypass"] {
            t.host
                .set_approval_mode("gov-s2".into(), mode.into())
                .unwrap_or_else(|e| panic!("{mode} must not need governance: {e}"));
        }
    });
}

/// Sidebar group names ("Ungrouped"/"Tasks") are reserved: workspace create/
/// rename and session rename reject them, and attach_by_path skips
/// auto-registration instead of minting a colliding group.
#[test]
fn reserved_sidebar_names_are_rejected_and_skipped() {
    with_temp_home(|dir| {
        let t = test_host();

        // create_workspace on a directory literally named Ungrouped → rejected,
        // and no workspace is minted.
        let reserved_dir = dir.join("Ungrouped");
        std::fs::create_dir_all(&reserved_dir).unwrap();
        let err = t
            .host
            .create_workspace(reserved_dir.to_string_lossy().into_owned())
            .unwrap_err();
        assert!(err.to_string().contains("reserved"), "{err}");
        assert!(
            t.host
                .list_workspaces()
                .iter()
                .all(|w| w.title != "Ungrouped"),
            "no colliding workspace may appear"
        );

        // rename_workspace to a reserved name → rejected (case-insensitive).
        let ok_dir = dir.join("proj");
        std::fs::create_dir_all(&ok_dir).unwrap();
        let created = t
            .host
            .create_workspace(ok_dir.to_string_lossy().into_owned())
            .unwrap();
        for name in ["Tasks", "ungrouped"] {
            let err = t
                .host
                .rename_workspace(created.id.clone(), name.into())
                .unwrap_err();
            assert!(err.to_string().contains("reserved"), "{name}: {err}");
        }

        // Session rename to a reserved name → rejected.
        let err = t
            .host
            .rename_session("res-t".into(), Some("Ungrouped".into()))
            .unwrap_err();
        assert!(err.to_string().contains("reserved"), "{err}");
    });
}

/// attach_session_workspace: unarchive + re-home in one call — archived_at
/// clears, membership lands in the target; a missing target is a hard error
/// (FK), never a silent Ok.
#[test]
fn attach_workspace_unarchives_and_rehomes() {
    with_temp_home(|dir| {
        let t = test_host();
        let ws_dir = dir.join("target-ws");
        std::fs::create_dir_all(&ws_dir).unwrap();
        let ws = t
            .host
            .create_workspace(ws_dir.to_string_lossy().into_owned())
            .unwrap();

        // A real session (a turn with no engine still persists the log), archived.
        let rx = t
            .host
            .execute(
                "hello".into(),
                Some("att-t".into()),
                None,
                None,
                None,
                None,
                None,
            )
            .expect("execute");
        wait_terminal(&t.host, rx);
        t.host.archive_session("att-t".into()).expect("archive");

        // Restore into the workspace: unarchive + attach in one call.
        t.host
            .attach_session_workspace("att-t".into(), ws.id.clone())
            .expect("attach restores");
        assert!(
            t.host
                .list_sessions(None)
                .iter()
                .any(|s| s.id == "att-t" && s.workspace_id.as_deref() == Some(ws.id.as_str())),
            "session re-homed and visible"
        );
        assert!(
            t.host
                .list_archived_sessions()
                .iter()
                .all(|s| s.id != "att-t"),
            "unarchived by the restore"
        );

        // A missing target workspace is a hard error.
        let err = t
            .host
            .attach_session_workspace("att-t".into(), "missing-ws".into())
            .unwrap_err();
        assert!(err.to_string().contains("attach failed"), "{err}");
    });
}

mod image_projection_e2e {
    //! WI-1: reference-ized image blocks + cold/hot projection + reload_image.
    //! The session log carries `{fileId, summary}` references (no base64);
    //! requests re-project per step: the trailing window rides as data URLs,
    //! older images shed to summary text, and a `reload_image` tool call
    //! pulls a file id back into the hot set for the rest of the turn.

    use base64::Engine as _;
    use futures::stream::{BoxStream, StreamExt};
    use starriver_kernel::llm::{
        CallId, ContentBlock, ContentBlockType, FinishReason, GenerateOptions, LlmAdapter,
        LlmError, Message, StreamChunk,
    };
    use std::sync::{Arc, Mutex};

    pub fn png_base64(color: u8) -> String {
        let img = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            2,
            2,
            image::Rgb([color, 0, 0]),
        ));
        let mut png = std::io::Cursor::new(Vec::new());
        img.write_to(&mut png, image::ImageFormat::Png).unwrap();
        base64::engine::general_purpose::STANDARD.encode(png.get_ref())
    }

    /// First step of every turn streams a `reload_image` tool call; once a
    /// reload_image ToolCall is already on the surface, later steps answer
    /// plainly so the turn completes. All request messages are captured.
    /// `file_id` is a shared slot — the test fills it in once the asset is
    /// admitted.
    pub struct ReloadCallingAdapter {
        pub provider: String,
        pub file_id: Arc<std::sync::Mutex<String>>,
        pub captured: Arc<Mutex<Vec<Vec<Message>>>>,
    }

    impl LlmAdapter for ReloadCallingAdapter {
        fn providers(&self) -> &[String] {
            std::slice::from_ref(&self.provider)
        }

        fn stream(
            &self,
            options: GenerateOptions,
        ) -> BoxStream<'static, Result<StreamChunk, LlmError>> {
            self.captured.lock().unwrap().push(options.messages.clone());
            let already_called =
                options.messages.iter().any(|message| {
                    message.content.iter().any(|block| matches!(
                    block,
                    ContentBlock::ToolCall { name: Some(name), .. } if name == "reload_image"
                ))
                });
            // Only the "final check" turn pulls the reload; earlier turns
            // answer plainly so the window/cold baseline stays clean.
            let wants_reload = options.messages.iter().any(|message| {
                message.content.iter().any(|block| {
                    matches!(
                        block,
                        ContentBlock::Text(t) if t.contains("final check")
                    )
                })
            });
            if already_called || !wants_reload {
                return Box::pin(futures::stream::iter(vec![
                    Ok(StreamChunk::TextDelta {
                        index: 0,
                        text: "done".into(),
                    }),
                    Ok(StreamChunk::BlockEnd {
                        index: 0,
                        block: ContentBlock::Text("done".into()),
                    }),
                    Ok(StreamChunk::Finish {
                        reason: FinishReason::Stop,
                        replay_state: None,
                    }),
                ]));
            }
            let file_id = self.file_id.lock().unwrap().clone();
            let call_id = CallId("call-reload-1".into());
            let arguments = format!(r#"{{"file_id":"{file_id}"}}"#);
            Box::pin(futures::stream::iter(vec![
                Ok(StreamChunk::BlockStart {
                    index: 0,
                    block_type: ContentBlockType::ToolCall,
                }),
                Ok(StreamChunk::ToolCallDelta {
                    index: 0,
                    id: call_id.clone(),
                    name: Some("reload_image".into()),
                    arguments_delta: arguments.clone(),
                }),
                Ok(StreamChunk::BlockEnd {
                    index: 0,
                    block: ContentBlock::ToolCall {
                        id: call_id,
                        name: Some("reload_image".into()),
                        arguments: starriver_kernel::json::JsonValue::from_json_str(&arguments)
                            .unwrap_or_else(|_| {
                                starriver_kernel::json::JsonValue::String(arguments.clone())
                            }),
                    },
                }),
                Ok(StreamChunk::Finish {
                    reason: FinishReason::ToolCalls,
                    replay_state: None,
                }),
            ]))
        }
    }
}

// POSIX-only fixture retained for parity with the sibling test; nothing here
// needs an OCR helper anymore (WI-152 S3 removed content summaries).
#[cfg(unix)]
#[test]
fn image_projection_log_stays_referenced_and_reload_pulls_hot() {
    with_temp_home(|dir| {
        // WI-159: the projection is budget-driven now — pin a tiny count
        // cap so the three-turn history keeps exactly the newest two hot,
        // preserving this test's window/cold/reload assertions.
        std::fs::create_dir_all(dir.join(".starriver")).unwrap();
        std::fs::write(
            dir.join(".starriver").join("settings.yaml"),
            "agent:\n  image_max_request_images: 2\n",
        )
        .unwrap();
        let host = host_in(dir);
        host.add_model(
            "img-prov".into(),
            "Img".into(),
            "cloud".into(),
            "openai-compat".into(),
            Some("https://example.com".into()),
            Some("k".into()),
            None,
            "m1".into(),
            None,
            None,
            None,
            None,
            None,
            Some("openai-chat".into()),
            None,
            None,
            Some(true),
        )
        .expect("add model");
        let captured = Arc::new(Mutex::new(Vec::new()));
        let file_id_slot = Arc::new(std::sync::Mutex::new(String::new()));
        host.debug_register_adapter(Arc::new(image_projection_e2e::ReloadCallingAdapter {
            provider: "img-prov".into(),
            file_id: Arc::clone(&file_id_slot),
            captured: Arc::clone(&captured),
        }));

        // Admit three distinct images (distinct bytes → distinct file ids)
        // across three turns, so only the trailing two stay hot by window.
        let mut payloads = Vec::new();
        let mut ref_ids = Vec::new();
        for (turn, color) in [10u8, 20, 30].into_iter().enumerate() {
            let payload = image_projection_e2e::png_base64(color);
            let admitted = host
                .admit_image_attachments(format!(
                    r#"[{{"name":"p{turn}.png","mediaType":"image/png","data":"{payload}"}}]"#
                ))
                .expect("admit");
            payloads.push(payload);
            ref_ids.push(admitted[0].id.clone());
        }
        *file_id_slot.lock().unwrap() = ref_ids[0].clone();

        let session = "img-proj";
        for (turn, ref_id) in ref_ids.iter().enumerate() {
            // post-turn dispose can race the next execute (SESSION_BUSY):
            // retry briefly, matching the durable-read polling below.
            let mut rx = None;
            for _ in 0..80 {
                match host.execute(
                    format!("turn {} image ![p{turn}]({ref_id})", turn + 1),
                    Some(session.into()),
                    None,
                    None,
                    None,
                    None,
                    None,
                ) {
                    Ok(receiver) => {
                        rx = Some(receiver);
                        break;
                    }
                    Err(error) if error.to_string().contains("SESSION_BUSY") => {
                        std::thread::sleep(std::time::Duration::from_millis(25));
                    }
                    Err(error) => panic!("execute: {error}"),
                }
            }
            wait_terminal(&host, rx.expect("execute eventually accepted"));
        }

        // The log never carries base64: every user/message event references
        // its image by file id + metadata.
        let log_text: String = host
            .debug_session_log_event_data(session.into(), "user/message".into())
            .iter()
            .map(|data| format!("{data:?}"))
            .collect();
        for payload in &payloads {
            assert!(
                !log_text.contains(payload.as_str()),
                "session log must not carry inline base64"
            );
        }
        assert!(
            log_text.contains("fileId"),
            "logged image blocks must carry the file reference: {log_text}"
        );

        // Turn 4: adapter first calls reload_image(A), then answers plainly.
        let mut rx = None;
        for _ in 0..80 {
            match host.execute(
                "final check".to_string(),
                Some(session.into()),
                None,
                None,
                None,
                None,
                None,
            ) {
                Ok(receiver) => {
                    rx = Some(receiver);
                    break;
                }
                Err(error) if error.to_string().contains("SESSION_BUSY") => {
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
                Err(error) => panic!("execute: {error}"),
            }
        }
        wait_terminal(&host, rx.expect("execute eventually accepted"));

        let guard = captured.lock().unwrap();
        assert!(
            guard.len() >= 5,
            "3 image turns + reload step + post-reload step, got {}",
            guard.len()
        );
        let pre_reload = &guard[guard.len() - 2];
        let post_reload = &guard[guard.len() - 1];

        // URL baselines: turn N's request carries the images admitted so far
        // as hot data URLs (normalized bytes ≠ the uploaded payload, so we
        // match request-to-request rather than against the raw upload).
        let urls_of = |messages: &[Message]| -> Vec<String> {
            messages
                .iter()
                .flat_map(|m| m.content.iter())
                .filter_map(|b| match b {
                    ContentBlock::Image { attachment } => {
                        attachment.as_str_at("url").map(String::from)
                    }
                    _ => None,
                })
                .collect()
        };
        let a_url = urls_of(&guard[0])[0].clone(); // turn 1: only A is on the surface
        let b_url = urls_of(&guard[1])
            .into_iter()
            .find(|u| *u != a_url)
            .expect("turn 2 request carries B");

        // Pre-reload (turn 4, step 1): A is cold — metadata placeholder text
        // carrying the file id + reload hint, no data URL — while B and C ride
        // the trailing window.
        let a_text = pre_reload.iter().any(|m| {
            m.content.iter().any(|b| match b {
                ContentBlock::Text(t) => {
                    t.contains(&format!("file_id={}", ref_ids[0])) && t.contains("reload_image")
                }
                _ => false,
            })
        });
        assert!(a_text, "oldest image must ship as cold placeholder text");
        let pre_urls = urls_of(pre_reload);
        assert_eq!(
            pre_urls.len(),
            2,
            "trailing window keeps exactly B and C hot"
        );
        assert!(
            pre_urls.iter().any(|u| *u == b_url),
            "B must be hot with its data URL; urls={pre_urls:?}"
        );
        assert!(
            !pre_urls.iter().any(|u| *u == a_url),
            "cold A must not ride the request"
        );

        // Post-reload (turn 4, step 2): the reload_image call pulled A back —
        // its data URL is on the wire again.
        let post_urls = urls_of(post_reload);
        assert!(
            post_urls.iter().any(|u| *u == a_url),
            "reload_image must pull the historical image hot for the next step; urls={post_urls:?}"
        );
    });
}

/// WI-159 end-to-end (the user's original scenario): five small screenshots
/// in ONE message all ride the request under the default byte budget — no
/// fixed window, no artificial count trim.
#[cfg(unix)]
#[test]
fn five_small_images_in_one_message_all_ride() {
    with_temp_home(|dir| {
        let host = host_in(dir);
        host.add_model(
            "five-prov".into(),
            "Five".into(),
            "cloud".into(),
            "openai-compat".into(),
            Some("https://example.com".into()),
            Some("k".into()),
            None,
            "m1".into(),
            None,
            None,
            None,
            None,
            None,
            Some("openai-chat".into()),
            None,
            None,
            Some(true),
        )
        .expect("add model");
        let captured = Arc::new(Mutex::new(Vec::new()));
        host.debug_register_adapter(Arc::new(vision_degrade_e2e::CapturingAdapter {
            provider: "five-prov".into(),
            captured: Arc::clone(&captured),
            reject_images: false,
        }));

        let mut refs = Vec::new();
        for turn in 0..5 {
            let payload = image_projection_e2e::png_base64(10 + turn * 10);
            let admitted = host
                .admit_image_attachments(format!(
                    r#"[{{"name":"p{turn}.png","mediaType":"image/png","data":"{payload}"}}]"#
                ))
                .expect("admit");
            refs.push(admitted[0].id.clone());
        }
        let markdown = refs
            .iter()
            .enumerate()
            .map(|(i, r)| format!("![p{i}]({r})"))
            .collect::<Vec<_>>()
            .join(" ");
        let rx = host
            .execute(
                format!("compare {markdown}"),
                Some("five-img".into()),
                None,
                None,
                None,
                None,
                None,
            )
            .expect("execute");
        wait_terminal(&host, rx);

        let guard = captured.lock().unwrap();
        let last = guard.last().expect("adapter called");
        let images = last
            .iter()
            .flat_map(|m| m.content.iter())
            .filter(|b| matches!(b, ContentBlock::Image { .. }))
            .count();
        assert_eq!(images, 5, "all five small screenshots must ride the wire");
    });
}

/// WI-159 end-to-end: a byte budget that fits only the newest two sheds the
/// oldest image to a recovery placeholder.
#[cfg(unix)]
#[test]
fn byte_budget_sheds_oldest_images_e2e() {
    with_temp_home(|dir| {
        let host = host_in(dir);
        host.add_model(
            "budget-prov".into(),
            "Budget".into(),
            "cloud".into(),
            "openai-compat".into(),
            Some("https://example.com".into()),
            Some("k".into()),
            None,
            "m1".into(),
            None,
            None,
            None,
            None,
            None,
            Some("openai-chat".into()),
            None,
            None,
            Some(true),
        )
        .expect("add model");
        let captured = Arc::new(Mutex::new(Vec::new()));
        host.debug_register_adapter(Arc::new(vision_degrade_e2e::CapturingAdapter {
            provider: "budget-prov".into(),
            captured: Arc::clone(&captured),
            reject_images: false,
        }));

        let mut refs = Vec::new();
        for turn in 0..3 {
            let payload = image_projection_e2e::png_base64(30 + turn * 10);
            let admitted = host
                .admit_image_attachments(format!(
                    r#"[{{"name":"p{turn}.png","mediaType":"image/png","data":"{payload}"}}]"#
                ))
                .expect("admit");
            refs.push(admitted[0].id.clone());
        }
        // Wire lengths from the store read path (base64), budget = newest two.
        let wires: Vec<usize> = refs
            .iter()
            .map(|r| host.read_image_attachment(r.clone()).unwrap().base64.len())
            .collect();
        let budget = (wires[1] + wires[2]) as u64;
        std::fs::create_dir_all(dir.join(".starriver")).unwrap();
        std::fs::write(
            dir.join(".starriver").join("settings.yaml"),
            format!("agent:\n  image_max_request_bytes: {budget}\n"),
        )
        .unwrap();

        let markdown = refs
            .iter()
            .enumerate()
            .map(|(i, r)| format!("![p{i}]({r})"))
            .collect::<Vec<_>>()
            .join(" ");
        let rx = host
            .execute(
                format!("compare {markdown}"),
                Some("budget-img".into()),
                None,
                None,
                None,
                None,
                None,
            )
            .expect("execute");
        wait_terminal(&host, rx);

        let guard = captured.lock().unwrap();
        let last = guard.last().expect("adapter called");
        let images = last
            .iter()
            .flat_map(|m| m.content.iter())
            .filter(|b| matches!(b, ContentBlock::Image { .. }))
            .count();
        assert_eq!(images, 2, "budget fits exactly the newest two");
        let texts = last
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ContentBlock::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect::<String>();
        assert!(
            texts.contains(&refs[0]) && texts.contains("如需查看原图"),
            "the shed oldest image keeps its recovery placeholder: {texts}"
        );
    });
}

/// WI-158/160 end-to-end: an in-turn duplicate reload_image lands as the
/// inline stop hint (single execution, both calls recorded).
#[cfg(unix)]
#[test]
fn duplicate_reload_noop_lands_in_the_log_e2e() {
    use futures::stream::{self, BoxStream};
    use starriver_kernel::llm::{
        CallId, ContentBlockType, FinishReason, GenerateOptions, LlmAdapter, LlmError, StreamChunk,
    };

    pub struct ReloadTwiceAdapter {
        pub provider: String,
        pub file_id: Arc<std::sync::Mutex<String>>,
        pub captured: Arc<Mutex<Vec<Vec<Message>>>>,
    }
    impl LlmAdapter for ReloadTwiceAdapter {
        fn providers(&self) -> &[String] {
            std::slice::from_ref(&self.provider)
        }
        fn stream(
            &self,
            options: GenerateOptions,
        ) -> BoxStream<'static, Result<StreamChunk, LlmError>> {
            self.captured.lock().unwrap().push(options.messages.clone());
            let settled = options
                .messages
                .iter()
                .flat_map(|m| m.content.iter())
                .filter(|b| matches!(b, ContentBlock::ToolResult { .. }))
                .count();
            let file_id = self.file_id.lock().unwrap().clone();
            let index = 0u64;
            if settled >= 2 {
                return Box::pin(stream::iter(vec![
                    Ok(StreamChunk::TextDelta {
                        index,
                        text: "done".into(),
                    }),
                    Ok(StreamChunk::BlockEnd {
                        index,
                        block: ContentBlock::Text("done".into()),
                    }),
                    Ok(StreamChunk::Finish {
                        reason: FinishReason::Stop,
                        replay_state: None,
                    }),
                ]));
            }
            let arguments = format!(r#"{{"file_id":"{file_id}"}}"#);
            Box::pin(stream::iter(vec![
                Ok(StreamChunk::ToolCallDelta {
                    index,
                    id: CallId(format!("dup-{settled}")),
                    name: Some("reload_image".into()),
                    arguments_delta: arguments.clone(),
                }),
                Ok(StreamChunk::BlockEnd {
                    index,
                    block: ContentBlock::ToolCall {
                        id: CallId(format!("dup-{settled}")),
                        name: Some("reload_image".into()),
                        arguments: starriver_kernel::json::JsonValue::from_json_str(&arguments)
                            .unwrap(),
                    },
                }),
                Ok(StreamChunk::Finish {
                    reason: FinishReason::ToolCalls,
                    replay_state: None,
                }),
            ]))
        }
    }

    with_temp_home(|dir| {
        let host = host_in(dir);
        host.add_model(
            "dup-prov".into(),
            "Dup".into(),
            "cloud".into(),
            "openai-compat".into(),
            Some("https://example.com".into()),
            Some("k".into()),
            None,
            "m1".into(),
            None,
            None,
            None,
            None,
            None,
            Some("openai-chat".into()),
            None,
            None,
            Some(true),
        )
        .expect("add model");
        let payload = image_projection_e2e::png_base64(77);
        let admitted = host
            .admit_image_attachments(format!(
                r#"[{{"name":"d.png","mediaType":"image/png","data":"{payload}"}}]"#
            ))
            .expect("admit");
        let ref_id = admitted[0].id.clone();
        let captured = Arc::new(Mutex::new(Vec::new()));
        host.debug_register_adapter(Arc::new(ReloadTwiceAdapter {
            provider: "dup-prov".into(),
            file_id: Arc::new(std::sync::Mutex::new(ref_id.clone())),
            captured: Arc::clone(&captured),
        }));

        let rx = host
            .execute(
                format!("look ![d]({ref_id}) then reload"),
                Some("dup-reload".into()),
                None,
                None,
                None,
                None,
                None,
            )
            .expect("execute");
        wait_terminal(&host, rx);

        let results = host.debug_session_log_event_data("dup-reload".into(), "tool/result".into());
        // BUG-121: the durable read must see non-surface rows too — the
        // write-behind flush runs before the load, so tool/call is
        // observable even after the registry dropped the agent.
        let calls = host.debug_session_log_event_data("dup-reload".into(), "tool/call".into());
        let rendered_calls = format!("{calls:?}");
        assert_eq!(
            rendered_calls.matches("reload_image").count(),
            2,
            "both model calls stay recorded: {rendered_calls}"
        );
        assert!(
            rendered_calls.contains("duplicateNoop"),
            "the synthetic call row carries the noop marker: {rendered_calls}"
        );
        let rendered = format!("{results:?}");
        assert!(
            rendered.contains("re-attached"),
            "the first reload executed: {rendered}"
        );
        assert!(
            rendered.contains("无需重复调用"),
            "the repeat landed as the inline stop hint: {rendered}"
        );
        // Both model calls stay recorded — counted on the wire (the later
        // request carries both ToolCall blocks) and in the persisted results.
        let guard = captured.lock().unwrap();
        let last = guard.last().expect("adapter called");
        let reload_calls = last
            .iter()
            .flat_map(|m| m.content.iter())
            .filter(|b| {
                matches!(
                    b,
                    ContentBlock::ToolCall { name: Some(name), .. } if name == "reload_image"
                )
            })
            .count();
        assert_eq!(reload_calls, 2, "both model calls stay recorded");
        let noop_results = rendered.matches("无需重复调用").count();
        assert_eq!(noop_results, 1, "exactly one inline stop hint");
    });
}

/// BUG-115 end-to-end: a reload whose asset was pruned fails NOT_FOUND, the
/// repeat gets the failure-aware stop hint, and the image's placeholder
/// keeps the honest recovery instruction (the failed pull pinned nothing).
#[cfg(unix)]
#[test]
fn pruned_asset_reload_fails_and_repeat_gets_failure_hint_e2e() {
    with_temp_home(|dir| {
        let host = host_in(dir);
        host.add_model(
            "prune-prov".into(),
            "Prune".into(),
            "cloud".into(),
            "openai-compat".into(),
            Some("https://example.com".into()),
            Some("k".into()),
            None,
            "m1".into(),
            None,
            None,
            None,
            None,
            None,
            Some("openai-chat".into()),
            None,
            None,
            Some(true),
        )
        .expect("add model");
        let payload = image_projection_e2e::png_base64(88);
        let admitted = host
            .admit_image_attachments(format!(
                r#"[{{"name":"g.png","mediaType":"image/png","data":"{payload}"}}]"#
            ))
            .expect("admit");
        let ref_id = admitted[0].id.clone();
        // Prune the normalized object: the reference stays on the surface,
        // every read now fails NOT_FOUND.
        let sha = ref_id.trim_start_matches("sha256:").to_string();
        let object = dir
            .join(".starriver")
            .join("attachments")
            .join("v1")
            .join("objects")
            .join(&sha[0..2])
            .join(&sha);
        std::fs::remove_file(&object).expect("prune the stored object");

        let captured = Arc::new(Mutex::new(Vec::new()));
        host.debug_register_adapter(Arc::new(image_projection_e2e::ReloadCallingAdapter {
            provider: "prune-prov".into(),
            file_id: Arc::new(std::sync::Mutex::new(ref_id.clone())),
            captured: Arc::clone(&captured),
        }));
        let rx = host
            .execute(
                format!("final check ![g]({ref_id})"),
                Some("prune-reload".into()),
                None,
                None,
                None,
                None,
                None,
            )
            .expect("execute");
        wait_terminal(&host, rx);

        let results =
            host.debug_session_log_event_data("prune-reload".into(), "tool/result".into());
        let rendered = format!("{results:?}");
        assert!(
            rendered.contains("NOT_FOUND") || rendered.contains("unavailable"),
            "the pruned reload fails with a clear reason: {rendered}"
        );

        // Whatever the projection decided, the placeholder must never claim
        // the pull succeeded (the failure pinned nothing).
        assert!(
            !rendered.contains("已重新加载过"),
            "a failed pull must not be reported as re-attached: {rendered}"
        );
        let guard = captured.lock().unwrap();
        let texts = guard
            .iter()
            .flat_map(|ms| ms.iter())
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ContentBlock::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect::<String>();
        assert!(
            !texts.contains("已重新加载过"),
            "no request placeholder may claim the failed pull re-attached: {texts}"
        );
    });
}

/// WI-160 end-to-end: `image_max_request_bytes: 0` disables images — nothing
/// rides, and the placeholder neither invites a reload nor claims the model
/// is text-only.
#[cfg(unix)]
#[test]
fn zero_byte_budget_disables_images_e2e() {
    with_temp_home(|dir| {
        std::fs::create_dir_all(dir.join(".starriver")).unwrap();
        std::fs::write(
            dir.join(".starriver").join("settings.yaml"),
            "agent:\n  image_max_request_bytes: 0\n",
        )
        .unwrap();
        let host = host_in(dir);
        host.add_model(
            "zero-prov".into(),
            "Zero".into(),
            "cloud".into(),
            "openai-compat".into(),
            Some("https://example.com".into()),
            Some("k".into()),
            None,
            "m1".into(),
            None,
            None,
            None,
            None,
            None,
            Some("openai-chat".into()),
            None,
            None,
            Some(true),
        )
        .expect("add model");
        let captured = Arc::new(Mutex::new(Vec::new()));
        host.debug_register_adapter(Arc::new(vision_degrade_e2e::CapturingAdapter {
            provider: "zero-prov".into(),
            captured: Arc::clone(&captured),
            reject_images: false,
        }));

        let payload = image_projection_e2e::png_base64(99);
        let admitted = host
            .admit_image_attachments(format!(
                r#"[{{"name":"z.png","mediaType":"image/png","data":"{payload}"}}]"#
            ))
            .expect("admit");
        let ref_id = admitted[0].id.clone();
        let rx = host
            .execute(
                format!("look ![z]({ref_id})"),
                Some("zero-budget".into()),
                None,
                None,
                None,
                None,
                None,
            )
            .expect("execute");
        wait_terminal(&host, rx);

        let guard = captured.lock().unwrap();
        let last = guard.last().expect("adapter called");
        let images = last
            .iter()
            .flat_map(|m| m.content.iter())
            .filter(|b| matches!(b, ContentBlock::Image { .. }))
            .count();
        assert_eq!(images, 0, "a zero byte budget carries no image data");
        let texts = last
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ContentBlock::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect::<String>();
        assert!(texts.contains("热图预算"), "{texts}");
        assert!(
            !texts.contains("reload_image"),
            "no reload invitation under a disabled budget: {texts}"
        );
        assert!(
            !texts.contains("仅支持文本输入"),
            "the model is not text-only — the wording must not claim it: {texts}"
        );
    });
}

#[test]
fn feedback_record_is_durable_and_validated() {
    with_temp_home(|dir| {
        let host = host_in(dir);
        host.add_model(
            "fb-prov".into(),
            "Fb".into(),
            "cloud".into(),
            "openai-compat".into(),
            Some("https://example.com".into()),
            Some("k".into()),
            None,
            "m1".into(),
            None,
            None,
            None,
            None,
            None,
            Some("openai-chat".into()),
            None,
            None,
            Some(true),
        )
        .expect("add model");
        host.debug_register_adapter(Arc::new(stub_adapter::StubAdapter {
            provider: "fb-prov".into(),
            reply: "ok".into(),
            calls: Arc::new(Mutex::new(Vec::new())),
        }));
        let rx = host
            .execute(
                "hello".to_string(),
                Some("fb-1".into()),
                None,
                None,
                None,
                None,
                None,
            )
            .expect("execute");
        wait_terminal(&host, rx);

        // Valid record: durable feedback/record event with kind + comment.
        // The read retries briefly: forward_live lands through the
        // write-behind batcher.
        host.record_feedback("fb-1".into(), "down".into(), Some("wrong file".into()))
            .expect("record");
        let mut log_text = String::new();
        for _ in 0..80 {
            log_text = host
                .debug_session_log_event_data("fb-1".into(), "feedback/record".into())
                .iter()
                .map(|d| format!("{d:?}"))
                .collect();
            if log_text.contains("wrong file") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert!(log_text.contains("\"down\""), "{log_text}");
        assert!(log_text.contains("wrong file"), "{log_text}");

        // Invalid kind is rejected before it reaches the log.
        assert!(host
            .record_feedback("fb-1".into(), "meh".into(), None)
            .is_err());
    });
}

// ── WI-5: agent_preset session binding ───────────────────────────────────────────

/// The built-in coding composition always heads the roster, healthy; the hub's
/// backfilled AGENT.md twins dedupe into it (no duplicate agent names), and
/// Chat Agent follows right after the coding row.
#[test]
fn agent_preset_roster_contains_the_builtin_coding_row() {
    let t = test_host();
    let rows = t.host.list_agent_presets();
    let coding = rows
        .iter()
        .find(|row| row.id == "coding")
        .expect("builtin coding row on the roster");
    assert_eq!(coding.source, "builtin");
    assert_eq!(coding.broken, None);
    assert!(
        rows.first().map(|r| r.id == "coding").unwrap_or(false),
        "builtin row first"
    );
    for (i, row) in rows.iter().enumerate() {
        assert!(
            !rows[..i]
                .iter()
                .any(|r| r.agent.eq_ignore_ascii_case(&row.agent)),
            "duplicate agent `{}` on the roster",
            row.agent
        );
    }
    if let Some(chat) = rows
        .iter()
        .position(|r| r.agent == starriver_host::CHAT_AGENT_NAME)
    {
        assert_eq!(chat, 1, "Chat Agent follows the built-in coding row");
    }
}

/// Unknown preset id on a NEW session → AGENT_PRESET_NOT_FOUND.
#[test]
fn execute_rejects_unknown_agent_preset() {
    let t = test_host();
    let err = t
        .host
        .execute(
            "hi".into(),
            None,
            None,
            Some(starriver_host::CODING_AGENT_NAME.into()),
            None,
            Some("no-such-preset".into()),
            None,
        )
        .unwrap_err();
    assert!(
        err.to_string().contains("AGENT_PRESET_NOT_FOUND"),
        "got: {err}"
    );
}

/// Continuing a session with a preset param → AGENT_PRESET_LOCKED (the
/// binding is creation-locked,).
#[test]
fn execute_rejects_agent_preset_on_continuing_session() {
    let t = test_host();
    // First run creates the session (no preset).
    let mut rx = t
        .host
        .execute(
            "hello".into(),
            Some("pres-lock".into()),
            None,
            Some(starriver_host::CODING_AGENT_NAME.into()),
            None,
            None,
            None,
        )
        .expect("first run");
    wait_terminal(&t.host, rx);
    // The lock is durability-based: wait until the session's row is visible,
    // then the single input slot (spawn teardown) before the second run.
    let mut durable = false;
    for _ in 0..200 {
        if t.host
            .list_sessions(None)
            .iter()
            .any(|s| s.id == "pres-lock")
        {
            durable = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(durable, "session row must become visible");
    let mut err = None;
    for _ in 0..100 {
        match t.host.execute(
            "again".into(),
            Some("pres-lock".into()),
            None,
            Some(starriver_host::CODING_AGENT_NAME.into()),
            None,
            Some("coding".into()),
            None,
        ) {
            Err(e) if e.to_string().contains("AGENT_PRESET_LOCKED") => {
                err = Some(e);
                break;
            }
            Err(e) if e.to_string().contains("SESSION_BUSY") => {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            Err(e) => panic!("unexpected error: {e}"),
            Ok(_) => panic!("continuation with preset must be rejected"),
        }
    }
    let err = err.expect("AGENT_PRESET_LOCKED within the poll window");
    assert!(
        err.to_string().contains("AGENT_PRESET_LOCKED"),
        "got: {err}"
    );
}

/// A NEW session naming the healthy builtin preset composes without error —
/// and a second run without the param stays unlocked-free (control).
#[test]
fn execute_accepts_the_builtin_coding_preset_on_new_sessions() {
    let t = test_host();
    let mut rx = t
        .host
        .execute(
            "planned".into(),
            Some("pres-ok".into()),
            None,
            Some(starriver_host::CODING_AGENT_NAME.into()),
            None,
            Some("coding".into()),
            None,
        )
        .expect("execute with builtin preset");
    wait_terminal(&t.host, rx);
    assert!(
        !t.host.list_sessions(None).is_empty(),
        "session created under the preset"
    );
}

/// BUG-64 regression (round 5): the cutoff clamps to [0, len] — the RETURNED
/// fork_point must be the clamped value (what the header persists and
/// list_sessions reads back), not the raw out-of-range argument.
#[test]
fn fork_point_reports_the_clamped_cutoff() {
    let t = test_host();
    seed_fork_source(&t, "src");

    let over = t
        .host
        .fork_session("src".into(), 100, "over".into())
        .expect("fork with out-of-range cutoff clamps");
    assert_eq!(
        over.fork_point,
        Some(4),
        "the returned fork_point matches the persisted seed_length (len=4), not the raw 100"
    );
    let listed_over = t
        .host
        .list_sessions(None)
        .into_iter()
        .find(|s| s.id == over.id)
        .expect("listed");
    assert_eq!(
        listed_over.fork_point,
        Some(4),
        "listing reads the clamped seed_length"
    );

    let negative = t
        .host
        .fork_session("src".into(), -3, "neg".into())
        .expect("negative cutoff clamps to 0");
    assert_eq!(negative.fork_point, Some(0));
    // The listing maps a zero seed length to None (existing display
    // semantics); the pinned contract is the RETURNED fork_point clamp.
}

/// WI-92: session.steer injects into a running turn's next-step inbox —
/// the steer call succeeds while the agent is resident.
#[test]
fn steer_injects_into_live_session() {
    let t = test_host();
    let rx = t
        .host
        .execute("count from 1".into(), None, None, None, None, None, None)
        .expect("execute");
    // Wait for the session to exist, then steer into it.
    let handle = t.host.runtime_handle();
    let sid = handle
        .block_on(async {
            use tokio::sync::broadcast;
            let mut rx = rx;
            loop {
                match rx.recv().await {
                    Ok(ev) => {
                        if let Some(s) = ev.get("sessionId").and_then(|v| v.as_str()) {
                            return Some(s.to_string());
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => return None,
                    _ => {}
                }
            }
        })
        .expect("session id observed");
    let steered = t.host.steer(sid.clone(), "focus on even numbers".into());
    assert!(steered, "steer must succeed on a resident agent");
    // Unknown session: false, no panic.
    assert!(!t.host.steer("no-such-session".into(), "x".into()));
}

// -- WI-103: mid-run model switch lands on the next LLM call --

mod gated_route_adapter {
    use futures::stream::BoxStream;
    use futures::StreamExt;
    use starriver_kernel::llm::{
        ContentBlock, FinishReason, GenerateOptions, LlmAdapter, LlmError, StreamChunk,
    };
    use std::sync::{Arc, Mutex};

    /// Serves both providers of the mid-run-switch test with a scripted
    /// three-call turn (read tool → gated read tool → final text), recording
    /// each call's (provider, model) so the test can assert exactly where
    /// the switch landed.
    pub struct GatedRouteAdapter {
        pub calls: Arc<Mutex<Vec<(String, String)>>>,
        pub gate: Arc<tokio::sync::Notify>,
    }

    fn tool_call_chunks(call: &str, args: &str) -> Vec<Result<StreamChunk, LlmError>> {
        vec![
            Ok(StreamChunk::ToolCallDelta {
                index: 0,
                id: starriver_kernel::llm::CallId(call.into()),
                name: Some("read".into()),
                arguments_delta: args.to_string(),
            }),
            Ok(StreamChunk::BlockEnd {
                index: 0,
                block: ContentBlock::ToolCall {
                    id: starriver_kernel::llm::CallId(call.into()),
                    name: Some("read".into()),
                    arguments: starriver_kernel::json::JsonValue::from_json_str(args)
                        .unwrap_or_else(|_| starriver_kernel::json::JsonValue::String(args.into())),
                },
            }),
            Ok(StreamChunk::Finish {
                reason: FinishReason::ToolCalls,
                replay_state: None,
            }),
        ]
    }

    fn text_chunks(text: &str) -> Vec<Result<StreamChunk, LlmError>> {
        vec![
            Ok(StreamChunk::TextDelta {
                index: 0,
                text: text.into(),
            }),
            Ok(StreamChunk::BlockEnd {
                index: 0,
                block: ContentBlock::Text(text.into()),
            }),
            Ok(StreamChunk::Finish {
                reason: FinishReason::Stop,
                replay_state: None,
            }),
        ]
    }

    impl LlmAdapter for GatedRouteAdapter {
        fn providers(&self) -> &[String] {
            static PROVIDERS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
            PROVIDERS.get_or_init(|| vec!["p-old".into(), "p-new".into()])
        }

        fn stream(
            &self,
            options: GenerateOptions,
        ) -> BoxStream<'static, Result<StreamChunk, LlmError>> {
            let mut calls = self.calls.lock().unwrap();
            let idx = calls.len();
            calls.push((options.provider.clone(), options.model.clone()));
            match idx {
                0 => Box::pin(futures::stream::iter(tool_call_chunks(
                    "c1",
                    r#"{"path":"missing-1.txt"}"#,
                ))),
                1 => {
                    // Park mid-turn: the test switches the model while this
                    // call is in flight, then releases the gate.
                    let gate = Arc::clone(&self.gate);
                    Box::pin(
                        futures::stream::once(async move {
                            gate.notified().await;
                            futures::stream::iter(tool_call_chunks(
                                "c2",
                                r#"{"path":"missing-2.txt"}"#,
                            ))
                        })
                        .flatten(),
                    )
                }
                _ => Box::pin(futures::stream::iter(text_chunks("done"))),
            }
        }
    }
}

#[test]
fn mid_run_model_switch_takes_effect_on_the_next_llm_call() {
    use starriver_host::CODING_AGENT_NAME;

    with_temp_home(|dir| {
        let host = host_in(dir);

        // Both providers carry a deliberately invalid api_format: the
        // engine-activation leg of set_agent_active_model then fails (a
        // tolerated, warn-only path), so a switch can never replace the mock
        // route registered below — while the AGENT.md write and the
        // desired-route record still happen. The agent profile keeps its
        // seeded (model-less) form, so this execute seeds the default route
        // (the first registered one, `p-old`) instead of activating.
        for (pid, model) in [("p-old", "old-m"), ("p-new", "new-m")] {
            host.add_model(
                pid.into(),
                pid.into(),
                "cloud".into(),
                "openai-compat".into(),
                Some("https://example.invalid/v1".into()),
                Some(format!("sk-{pid}")),
                None,
                model.into(),
                None,
                None,
                None,
                None,
                None,
                Some("bogus-format".into()),
                None,
                None,
                None,
            )
            .expect("add provider");
        }

        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<(String, String)>::new()));
        let gate = std::sync::Arc::new(tokio::sync::Notify::new());
        host.debug_register_adapter(std::sync::Arc::new(
            gated_route_adapter::GatedRouteAdapter {
                calls: calls.clone(),
                gate: gate.clone(),
            },
        ));

        let handle = host.runtime_handle();
        let mut rx = host
            .execute(
                "map the crate".into(),
                None,
                None,
                Some(CODING_AGENT_NAME.into()),
                None,
                None,
                None,
            )
            .expect("execute");

        // The second LLM call enters once the first read tool settled; it is
        // parked on the gate from entry.
        for _ in 0..500 {
            if calls.lock().unwrap().len() >= 2 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(calls.lock().unwrap().len(), 2, "second call never started");

        // Switch mid-run from a plain thread (the host path blocks on locks).
        host.set_agent_active_model(CODING_AGENT_NAME.into(), "p-new".into(), "new-m".into())
            .expect("switch mid-run");

        gate.notify_one();
        handle.block_on(async {
            for _ in 0..200 {
                match rx.recv().await {
                    Ok(ev) if ev["type"] == "turnEnd" || ev["type"] == "turnError" => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        });

        let recorded = calls.lock().unwrap().clone();
        // The seeded route (default provider `p-old`, its declared
        // default_model — a null-model run resolves the provider default
        // instead of the literal "mock" placeholder, BUG-143) serves the
        // first two calls; the switch lands on call three.
        assert_eq!(
            recorded,
            vec![
                ("p-old".to_string(), "old-m".to_string()),
                ("p-old".to_string(), "old-m".to_string()),
                ("p-new".to_string(), "new-m".to_string()),
            ],
            "in-flight call keeps its route; the switch lands on the next call"
        );
    })
}

/// BUG-86: has_more must reflect the entry window (older entries exist), not
/// compare entry totals against projected row counts; paged rows carry their
/// surface seq so the renderer anchors "load earlier" without re-aligning
/// against the full timeline.
#[test]
fn paged_history_reports_entry_window_and_stamps_seq() {
    let t = test_host();
    let events: Vec<SessionEvent> = vec![
        sev(
            0,
            "assistant/message",
            JsonValue::Object(vec![(
                "message".into(),
                msg_json(
                    MessageRole::Assistant,
                    "m0",
                    vec![ContentBlock::Text("answer 0".into())],
                ),
            )]),
            true,
        ),
        sev(
            1,
            "assistant/message",
            JsonValue::Object(vec![(
                "message".into(),
                msg_json(
                    MessageRole::Assistant,
                    "m1",
                    vec![ContentBlock::Text("answer 1".into())],
                ),
            )]),
            true,
        ),
        sev(
            2,
            "assistant/message",
            JsonValue::Object(vec![(
                "message".into(),
                msg_json(
                    MessageRole::Assistant,
                    "m2",
                    vec![ContentBlock::Text("answer 2".into())],
                ),
            )]),
            true,
        ),
        sev(
            3,
            "assistant/message",
            JsonValue::Object(vec![(
                "message".into(),
                msg_json(
                    MessageRole::Assistant,
                    "m3",
                    vec![ContentBlock::Text("answer 3".into())],
                ),
            )]),
            true,
        ),
        sev(
            4,
            "assistant/message",
            JsonValue::Object(vec![(
                "message".into(),
                msg_json(
                    MessageRole::Assistant,
                    "m4",
                    vec![ContentBlock::Text("answer 4".into())],
                ),
            )]),
            true,
        ),
        sev(
            5,
            "assistant/message",
            JsonValue::Object(vec![(
                "message".into(),
                msg_json(
                    MessageRole::Assistant,
                    "m5",
                    vec![ContentBlock::Text("answer 5".into())],
                ),
            )]),
            true,
        ),
    ];
    t.host
        .seed_test_log("page-1".into(), None, None, events)
        .expect("seed log");

    let (msgs, total, has_more) = t.host.get_session_messages_paged("page-1".into(), None, 4);
    assert_eq!(total, 6);
    assert!(has_more, "older entries exist beyond the window");
    assert_eq!(msgs.len(), 4);
    assert_eq!(msgs[0].seq, Some(2), "oldest row of the newest page");
    assert_eq!(msgs[3].seq, Some(5), "newest row");

    let (older, _total, has_more_head) =
        t.host
            .get_session_messages_paged("page-1".into(), Some(2), 4);
    assert_eq!(older.len(), 2);
    assert!(!has_more_head, "reached the head of history");
    assert_eq!(older[0].seq, Some(0));
}

/// BUG-87: the goal loop must actually drive rounds — a fresh execute per
/// round, live usage accounting, and a hard stop at the budget.
#[test]
fn goal_loop_runs_rounds_and_stops_at_budget() {
    use futures::stream::BoxStream;
    use starriver_host::CODING_AGENT_NAME;
    use starriver_kernel::llm::{
        ContentBlock, ContentBlockType, FinishReason, GenerateOptions, LlmAdapter, LlmError,
        StreamChunk, TokenUsage,
    };

    struct UsageTextAdapter {
        calls: std::sync::Arc<std::sync::Mutex<usize>>,
    }
    impl LlmAdapter for UsageTextAdapter {
        fn providers(&self) -> &[String] {
            static PROVIDERS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
            PROVIDERS.get_or_init(|| vec!["mock".into()])
        }
        fn stream(
            &self,
            _options: GenerateOptions,
        ) -> BoxStream<'static, Result<StreamChunk, LlmError>> {
            *self.calls.lock().unwrap() += 1;
            Box::pin(futures::stream::iter(vec![
                Ok(StreamChunk::Usage {
                    usage: TokenUsage {
                        input_tokens: 40,
                        output_tokens: 60,
                        cache_read_tokens: None,
                        cache_write_tokens: None,
                        reasoning_tokens: None,
                    },
                }),
                Ok(StreamChunk::BlockStart {
                    index: 0,
                    block_type: ContentBlockType::Text,
                }),
                Ok(StreamChunk::TextDelta {
                    index: 0,
                    text: "step done".into(),
                }),
                Ok(StreamChunk::BlockEnd {
                    index: 0,
                    block: ContentBlock::Text("step done".into()),
                }),
                Ok(StreamChunk::Finish {
                    reason: FinishReason::Stop,
                    replay_state: None,
                }),
            ]))
        }
        fn chat(
            &self,
            _options: GenerateOptions,
        ) -> futures::future::BoxFuture<'static, Result<String, LlmError>> {
            Box::pin(async { Ok("step done".into()) })
        }
    }

    with_temp_home(|dir| {
        let host = host_in(dir);
        let calls = std::sync::Arc::new(std::sync::Mutex::new(0usize));
        host.debug_register_adapter(std::sync::Arc::new(UsageTextAdapter {
            calls: calls.clone(),
        }));
        let handle = host.runtime_handle();

        // Round 0: create the session and learn its id.
        let mut rx = host
            .execute(
                "start".into(),
                None,
                None,
                Some(CODING_AGENT_NAME.into()),
                None,
                None,
                None,
            )
            .expect("execute");
        let mut sid = String::new();
        handle.block_on(async {
            loop {
                match rx.recv().await {
                    Ok(ev) => {
                        if ev["type"] == "sessionStarted" {
                            sid = ev["sessionId"].as_str().unwrap_or_default().to_string();
                        }
                        if ev["type"] == "turnEnd" || ev["type"] == "turnError" {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        assert!(!sid.is_empty(), "session id captured");

        // The session log lands at dispose time — wait until it is readable
        // so the resuming execute below finds THIS session instead of
        // silently creating a new one (same race poll_session_events exists
        // for).
        let mut durable = false;
        for _ in 0..500 {
            if !host.get_session_timeline(sid.clone()).is_empty() {
                durable = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(durable, "session log never became durable");

        // Budget 250 tokens at 100 per round: rounds 1..3 run, the third
        // crosses the budget mid-round and the loop stops after it.
        host.set_goal(sid.clone(), "finish the sweep".into(), Some(250))
            .expect("set goal");
        let before = *calls.lock().unwrap();
        let mut rx = host
            .execute(
                "continue".into(),
                Some(sid.clone()),
                None,
                Some(CODING_AGENT_NAME.into()),
                None,
                None,
                None,
            )
            .expect("execute");
        let mut saw_budget_limited = false;
        handle.block_on(async {
            loop {
                match rx.recv().await {
                    Ok(ev) => {
                        if ev["type"] == "goalStatus"
                            && ev["status"] == "budgetLimited"
                            && ev["sessionId"].as_str() == Some(sid.as_str())
                        {
                            saw_budget_limited = true;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        assert!(
            saw_budget_limited,
            "goalStatus broadcast reached the stream"
        );
        let ran = *calls.lock().unwrap() - before;
        // The accounting is asynchronous by design (the watcher consumes the
        // relayed usage stream), so under scheduler load the BudgetLimited
        // flip can land one round late — 3 is the mathematical minimum
        // (250 budget at 100/round), and anything past 8 means the loop
        // effectively ignored the budget (the hard cap is 25).
        assert!(
            (3..=8).contains(&ran),
            "budget stopped the loop in a sane number of rounds (ran {ran})"
        );
    })
}

/// WI-107: full-text search over persisted payloads — hit carries a snippet
/// and only matching sessions surface.
#[test]
fn search_session_content_finds_matching_sessions_with_snippet() {
    let t = test_host();
    t.host
        .seed_test_log(
            "search-a".into(),
            None,
            None,
            vec![sev(
                0,
                "user/message",
                msg_json(
                    MessageRole::User,
                    "m0",
                    vec![ContentBlock::Text(
                        "please grep the repo for zstd caches".into(),
                    )],
                ),
                true,
            )],
        )
        .expect("seed a");
    t.host
        .seed_test_log(
            "search-b".into(),
            None,
            None,
            vec![sev(
                0,
                "user/message",
                msg_json(
                    MessageRole::User,
                    "m0",
                    vec![ContentBlock::Text("an unrelated question".into())],
                ),
                true,
            )],
        )
        .expect("seed b");

    let hits = t.host.search_session_content("zstd".into(), 20);
    assert_eq!(hits.len(), 1, "only the matching session surfaces");
    assert_eq!(hits[0]["sessionId"], "search-a");
    let snippet = hits[0]["snippet"].as_str().unwrap_or_default();
    assert!(
        snippet.contains("zstd"),
        "snippet carries the needle: {snippet}"
    );
    assert_eq!(hits[0]["matches"], 1);

    assert!(t
        .host
        .search_session_content("quantum".into(), 20)
        .is_empty());
}

/// WI-108: cancelling while the LLM stream is silent (hung provider) takes
/// effect immediately — the chunk wait races the abort token instead of
/// waiting for the next chunk.
#[test]
fn cancel_during_silent_stream_takes_effect_immediately() {
    use futures::stream::BoxStream;
    use futures::StreamExt as _;
    use starriver_host::CODING_AGENT_NAME;
    use starriver_kernel::llm::{
        ContentBlockType, GenerateOptions, LlmAdapter, LlmError, StreamChunk,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Yields the opening block-start chunk, then parks forever — a hung
    /// provider with a silent stream.
    struct SilentThenParkAdapter {
        entered: Arc<AtomicUsize>,
    }
    impl LlmAdapter for SilentThenParkAdapter {
        fn providers(&self) -> &[String] {
            static PROVIDERS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
            PROVIDERS.get_or_init(|| vec!["mock".into()])
        }
        fn stream(
            &self,
            _options: GenerateOptions,
        ) -> BoxStream<'static, Result<StreamChunk, LlmError>> {
            self.entered.fetch_add(1, Ordering::SeqCst);
            let stream = futures::stream::once(async {
                // One chunk, then nothing — the stream future is dropped
                // before completion, so the loop sits in stream.next().
                Ok::<_, LlmError>(StreamChunk::BlockStart {
                    index: 0,
                    block_type: ContentBlockType::Text,
                })
            });
            // Chain a never-resolving item after the first chunk: the
            // stream yields one block-start, then sits silent forever.
            let silent =
                futures::stream::once(futures::future::pending::<Result<StreamChunk, LlmError>>());
            Box::pin(stream.chain(silent))
        }
        fn chat(
            &self,
            _options: GenerateOptions,
        ) -> futures::future::BoxFuture<'static, Result<String, LlmError>> {
            Box::pin(async { Ok("ok".into()) })
        }
    }

    with_temp_home(|dir| {
        let host = host_in(dir);
        let entered = Arc::new(AtomicUsize::new(0));
        host.debug_register_adapter(Arc::new(SilentThenParkAdapter {
            entered: Arc::clone(&entered),
        }));
        let handle = host.runtime_handle();
        let mut rx = host
            .execute(
                "park mid-stream".into(),
                None,
                None,
                Some(CODING_AGENT_NAME.into()),
                None,
                None,
                None,
            )
            .expect("execute");

        // Wait until the turn has actually entered the stream (one chunk in).
        for _ in 0..500 {
            if entered.load(Ordering::SeqCst) > 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            entered.load(Ordering::SeqCst) > 0,
            "turn never started streaming"
        );
        // Drain any queued events (blockStart etc.) so only the terminal one
        // is awaited below.
        let mut session_id = String::new();
        handle.block_on(async {
            loop {
                match rx.try_recv() {
                    Ok(ev) => {
                        if ev["type"] == "sessionStarted" {
                            session_id = ev["sessionId"].as_str().unwrap_or_default().to_string();
                        }
                    }
                    Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                    Err(_) => break,
                }
            }
        });
        assert!(!session_id.is_empty());

        // Cancel while the stream is silent — the cancelled event must
        // arrive WITHOUT any further chunk from the adapter.
        assert!(host.cancel(session_id.clone()));
        let mut got_cancelled = false;
        handle.block_on(async {
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                match tokio::time::timeout_at(deadline, rx.recv()).await {
                    Ok(Ok(ev)) if ev["type"] == "cancelled" => {
                        got_cancelled = true;
                        break;
                    }
                    Ok(_) => continue,
                    Err(_) => break, // timeout — fix regression
                }
            }
        });
        assert!(
            got_cancelled,
            "cancel during a silent stream must take effect immediately"
        );
    })
}

/// BUG-99: a provider stream that ENDS without a finish chunk (connection
/// dropped mid-generation) must fail the turn visibly (PROVIDER_STREAM_
/// TRUNCATED) — never synthesize a clean Stop that leaves the session with
/// a silent, empty completion.
#[test]
fn truncated_llm_stream_fails_the_turn_visibly() {
    use futures::stream::BoxStream;
    use starriver_kernel::llm::{
        ContentBlockType, GenerateOptions, LlmAdapter, LlmError, StreamChunk,
    };
    use std::sync::Arc;

    /// Streams a reasoning block start + one delta, then the byte stream
    /// just ends — no Finish chunk at all (truncated connection).
    struct TruncatedAdapter;
    impl LlmAdapter for TruncatedAdapter {
        fn providers(&self) -> &[String] {
            static PROVIDERS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
            PROVIDERS.get_or_init(|| vec!["mock".into()])
        }
        fn stream(
            &self,
            _options: GenerateOptions,
        ) -> BoxStream<'static, Result<StreamChunk, LlmError>> {
            let stream = futures::stream::iter(vec![
                Ok(StreamChunk::BlockStart {
                    index: 0,
                    block_type: ContentBlockType::Reasoning,
                }),
                Ok(StreamChunk::BlockEnd {
                    index: 0,
                    block: starriver_kernel::llm::ContentBlock::Reasoning("think".into()),
                }),
            ]);
            Box::pin(stream)
        }
        fn chat(
            &self,
            _options: GenerateOptions,
        ) -> futures::future::BoxFuture<'static, Result<String, LlmError>> {
            Box::pin(async { Ok("ok".into()) })
        }
    }

    with_temp_home(|dir| {
        let host = host_in(dir);
        host.debug_register_adapter(Arc::new(TruncatedAdapter));
        let handle = host.runtime_handle();
        let mut rx = host
            .execute(
                "truncated stream".into(),
                Some("trunc-99".into()),
                None,
                Some(starriver_host::CODING_AGENT_NAME.into()),
                None,
                None,
                None,
            )
            .expect("execute");
        let mut got_error = None;
        handle.block_on(async {
            // 2026-10-04: PROVIDER_STREAM_TRUNCATED is now retried under the
            // default policy (5 retries, 500ms→10s backoff ≈ 15.5s) before
            // the turn may fail — the turnError arrives only after the ladder
            // is exhausted, so the wait must outlast it.
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
            loop {
                match tokio::time::timeout_at(deadline, rx.recv()).await {
                    Ok(Ok(ev)) => {
                        if ev["type"] == "turnError" {
                            got_error = Some(ev);
                            break;
                        }
                        if ev["type"] == "turnEnd" {
                            panic!("truncated stream must NOT end as a clean turn: {ev}");
                        }
                    }
                    _ => break,
                }
            }
        });
        let ev = got_error.expect("turn must fail visibly on a truncated stream");
        let text = ev.to_string();
        assert!(
            text.contains("PROVIDER_STREAM_TRUNCATED") || text.contains("finish reason"),
            "turnError must carry the truncation failure: {text}"
        );
    })
}

/// BUG-96: the single input slot must be observable from the backend —
/// held while the turn lives (even a hung one), released after it ends.
/// This is the source of truth UIs sync from after a renderer reload.
#[test]
fn running_session_ids_tracks_the_single_input_slot() {
    use futures::stream::BoxStream;
    use starriver_kernel::llm::{
        ContentBlockType, GenerateOptions, LlmAdapter, LlmError, StreamChunk,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct ParkAdapter {
        entered: Arc<AtomicUsize>,
    }
    impl LlmAdapter for ParkAdapter {
        fn providers(&self) -> &[String] {
            static PROVIDERS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
            PROVIDERS.get_or_init(|| vec!["mock".into()])
        }
        fn stream(
            &self,
            _options: GenerateOptions,
        ) -> BoxStream<'static, Result<StreamChunk, LlmError>> {
            self.entered.fetch_add(1, Ordering::SeqCst);
            let stream = futures::stream::once(async {
                Ok::<_, LlmError>(StreamChunk::BlockStart {
                    index: 0,
                    block_type: ContentBlockType::Text,
                })
            });
            let silent =
                futures::stream::once(futures::future::pending::<Result<StreamChunk, LlmError>>());
            Box::pin(stream.chain(silent))
        }
        fn chat(
            &self,
            _options: GenerateOptions,
        ) -> futures::future::BoxFuture<'static, Result<String, LlmError>> {
            Box::pin(async { Ok("ok".into()) })
        }
    }

    with_temp_home(|dir| {
        let host = host_in(dir);
        // Fresh host: no session holds the slot.
        assert!(host.running_session_ids().is_empty());

        let entered = Arc::new(AtomicUsize::new(0));
        host.debug_register_adapter(Arc::new(ParkAdapter {
            entered: Arc::clone(&entered),
        }));
        let handle = host.runtime_handle();
        // The slot guards NAMED sessions (callers may pre-choose the id for a
        // fresh session) — the same shape as the reported bug: continuing an
        // existing conversation that a stale turn still occupies.
        let session_id = "slot-96".to_string();
        let mut rx = host
            .execute(
                "park mid-stream".into(),
                Some(session_id.clone()),
                None,
                Some(starriver_host::CODING_AGENT_NAME.into()),
                None,
                None,
                None,
            )
            .expect("execute");
        for _ in 0..500 {
            if entered.load(Ordering::SeqCst) > 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            entered.load(Ordering::SeqCst) > 0,
            "turn never started streaming"
        );
        let _ = &mut rx;

        // While the (hung) turn lives, the backend reports the slot held —
        // this is what the UI's Stop affordance re-arms from after a reload.
        let mut observed_held = false;
        for _ in 0..100 {
            if host
                .running_session_ids()
                .iter()
                .any(|id| id == &session_id)
            {
                observed_held = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            observed_held,
            "running_session_ids must report the live turn"
        );

        // Cancel → the slot releases (guard drop), i.e. the session is
        // usable again — exactly what the frontend's recovery flow waits for.
        assert!(host.cancel(session_id.clone()));
        let mut released = false;
        for _ in 0..200 {
            if !host
                .running_session_ids()
                .iter()
                .any(|id| id == &session_id)
            {
                released = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            released,
            "slot must release after the stale turn is cancelled"
        );
    })
}

/// 2026-10-05 sidebar gap: the FIRST turn of a brand-new session is sent
/// with no session id, so the single-input-slot guard was constructed empty
/// and `sessions.running` never reported the session — the sidebar's WI-179
/// poll saw no running id, its missing-row refresh never fired, and the new
/// task only showed up under its workspace at turn end whenever the
/// one-shot TaskStarted refresh missed. Contract pinned here: during a live
/// fresh-session turn the minted sid (a) holds the slot and (b) the row
/// lists under the workspace with its fallback title already stamped.
#[test]
fn fresh_session_first_turn_holds_slot_and_lists_during_the_run() {
    use futures::stream::BoxStream;
    use starriver_kernel::llm::{
        ContentBlockType, GenerateOptions, LlmAdapter, LlmError, StreamChunk,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct ParkAdapter {
        entered: Arc<AtomicUsize>,
    }
    impl LlmAdapter for ParkAdapter {
        fn providers(&self) -> &[String] {
            static PROVIDERS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
            PROVIDERS.get_or_init(|| vec!["mock".into()])
        }
        fn stream(
            &self,
            _options: GenerateOptions,
        ) -> BoxStream<'static, Result<StreamChunk, LlmError>> {
            self.entered.fetch_add(1, Ordering::SeqCst);
            let stream = futures::stream::once(async {
                Ok::<_, LlmError>(StreamChunk::BlockStart {
                    index: 0,
                    block_type: ContentBlockType::Text,
                })
            });
            let silent =
                futures::stream::once(futures::future::pending::<Result<StreamChunk, LlmError>>());
            Box::pin(stream.chain(silent))
        }
        fn chat(
            &self,
            _options: GenerateOptions,
        ) -> futures::future::BoxFuture<'static, Result<String, LlmError>> {
            Box::pin(async { Ok("ok".into()) })
        }
    }

    with_temp_home(|dir| {
        let host = host_in(dir);
        let entered = Arc::new(AtomicUsize::new(0));
        host.debug_register_adapter(Arc::new(ParkAdapter {
            entered: Arc::clone(&entered),
        }));
        // A workspace must be active for the coding agent so the fresh row
        // files under it (attach_by_path before the first execution spawns).
        let proj = dir.join("ws-fresh-first-turn");
        std::fs::create_dir(&proj).unwrap();
        let ws = host
            .create_workspace(proj.to_string_lossy().into_owned())
            .expect("create workspace");
        host.set_active_workspace_for(starriver_host::CODING_AGENT_NAME, Some(ws.id.clone()))
            .expect("activate workspace");

        // session_id = None: the exact shape of the first Send on the
        // New Task screen — the id does not exist yet and is minted inside
        // the run task.
        let mut rx = host
            .execute(
                "park mid-stream fresh".into(),
                None,
                None,
                Some(starriver_host::CODING_AGENT_NAME.into()),
                None,
                None,
                None,
            )
            .expect("execute");
        for _ in 0..500 {
            if entered.load(Ordering::SeqCst) > 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            entered.load(Ordering::SeqCst) > 0,
            "turn never started streaming"
        );
        let _ = &mut rx;

        // (a) The minted sid holds the single input slot during the turn —
        // this is what the sidebar poll consumes to force the row refresh.
        let mut held: Option<String> = None;
        for _ in 0..500 {
            if let Some(id) = host.running_session_ids().first() {
                held = Some(id.clone());
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let sid = held.expect("fresh-session turn must adopt the minted sid into the slot");

        // (b) The user-visible contract: the row lists under its workspace
        // with the fallback title (truncated task text) already stamped —
        // while the turn is still live, not after it settles.
        let mut listed = false;
        for _ in 0..500 {
            let hit = host
                .list_sessions(Some(ws.id.clone()))
                .into_iter()
                .find(|s| s.id == sid);
            if hit.map(|s| !s.title.is_empty()).unwrap_or(false) {
                listed = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            listed,
            "fresh first-turn row must list under its workspace with a title during the run"
        );

        // Cancel → the adopted slot releases (guard drop removes the sid).
        assert!(host.cancel(sid.clone()));
        let mut released = false;
        for _ in 0..200 {
            if !host.running_session_ids().iter().any(|id| id == &sid) {
                released = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            released,
            "adopted slot must release after the turn is cancelled"
        );
    })
}

/// WI-112: an external-ACP subagent template parses from settings and the
/// spawner routes it to the child-process client (a bogus command fails
/// with the client's spawn error — proving the branch, not just parsing).
#[test]
fn acp_subagent_template_parses_and_routes_to_the_client() {
    let t = test_host();
    let patch = serde_json::json!({
        "subagents": {"entries": [
            {"name": "external-coder", "enabled": true,
             "acp": {"command": "/definitely/not/a/real/binary", "args": [], "trust": false}}
        ]}
    });
    t.host.update_settings(patch).expect("patch settings");

    // The spawn routes to the ACP client branch — the bogus command
    // surfaces the client's spawn error verbatim (a plain in-process
    // template would instead fail with a model/route error, so the error
    // text proves which branch ran).
    let host = std::sync::Arc::new(t.host);
    let error = host
        .debug_spawn_subagent("external-coder", "do the thing")
        .expect_err("bogus binary must fail");
    assert!(
        error.contains("spawning ACP agent"),
        "routed to the ACP client: {error}"
    );
}

/// Send Now regression (2026-10-07 user report): a message queued during a
/// live turn and flushed with `session.queueFlushNow` is claimed by the
/// agent's OWN driver as the next turn — its events must stay visible on
/// the execute stream after turn 1's cancelled, and the session must keep
/// running to completion instead of going silent.
#[test]
fn send_now_followup_turn_stays_visible_on_execute_stream() {
    use futures::stream::BoxStream;
    use futures::StreamExt as _;
    use starriver_host::CODING_AGENT_NAME;
    use starriver_kernel::llm::{
        ContentBlock, ContentBlockType, FinishReason, GenerateOptions, LlmAdapter, LlmError,
        StreamChunk,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Call 1 parks silently (live turn in flight); every later call
    /// completes immediately so the follow-up turn can finish on its own.
    struct ParkThenCompleteAdapter {
        entered: Arc<AtomicUsize>,
    }
    impl LlmAdapter for ParkThenCompleteAdapter {
        fn providers(&self) -> &[String] {
            static PROVIDERS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
            PROVIDERS.get_or_init(|| vec!["mock".into()])
        }
        fn stream(
            &self,
            _options: GenerateOptions,
        ) -> BoxStream<'static, Result<StreamChunk, LlmError>> {
            let n = self.entered.fetch_add(1, Ordering::SeqCst) + 1;
            if n == 1 {
                let stream = futures::stream::once(async {
                    Ok::<_, LlmError>(StreamChunk::BlockStart {
                        index: 0,
                        block_type: ContentBlockType::Text,
                    })
                });
                let silent = futures::stream::once(futures::future::pending::<
                    Result<StreamChunk, LlmError>,
                >());
                Box::pin(stream.chain(silent))
            } else {
                let stream = futures::stream::iter(vec![
                    Ok(StreamChunk::BlockStart {
                        index: 0,
                        block_type: ContentBlockType::Text,
                    }),
                    Ok(StreamChunk::TextDelta {
                        index: 0,
                        text: "follow-up answered".into(),
                    }),
                    Ok(StreamChunk::BlockEnd {
                        index: 0,
                        block: ContentBlock::Text("follow-up answered".into()),
                    }),
                    Ok(StreamChunk::Finish {
                        reason: FinishReason::Stop,
                        replay_state: None,
                    }),
                ]);
                Box::pin(stream)
            }
        }
        fn chat(
            &self,
            _options: GenerateOptions,
        ) -> futures::future::BoxFuture<'static, Result<String, LlmError>> {
            Box::pin(async { Ok("ok".into()) })
        }
    }

    with_temp_home(|dir| {
        let host = host_in(dir);
        let entered = Arc::new(AtomicUsize::new(0));
        host.debug_register_adapter(Arc::new(ParkThenCompleteAdapter {
            entered: Arc::clone(&entered),
        }));
        let handle = host.runtime_handle();
        let mut rx = host
            .execute(
                "first task".into(),
                None,
                None,
                Some(CODING_AGENT_NAME.into()),
                None,
                None,
                None,
            )
            .expect("execute");

        // Wait until turn 1 has actually entered the stream.
        for _ in 0..500 {
            if entered.load(Ordering::SeqCst) > 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            entered.load(Ordering::SeqCst) > 0,
            "turn never started streaming"
        );

        let mut session_id = String::new();
        handle.block_on(async {
            loop {
                match rx.try_recv() {
                    Ok(ev) => {
                        if ev["type"] == "sessionStarted" {
                            session_id = ev["sessionId"].as_str().unwrap_or_default().to_string();
                        }
                    }
                    _ => break,
                }
            }
        });
        assert!(!session_id.is_empty(), "sessionStarted never surfaced");

        // The user types a new message mid-turn (queues) and hits Send Now.
        let queued = host.queue_add(session_id.clone(), "follow-up question".into());
        assert!(queued.is_some(), "queue_add on a live session must succeed");
        assert!(host.queue_flush_now(session_id.clone()), "queueFlushNow");

        // Turn 1 aborts (cancelled), then the follow-up turn must RUN AND
        // STREAM: its assistant text and terminal land on the same stream.
        let mut saw_cancelled = false;
        let mut saw_followup_text = false;
        let mut saw_followup_terminal = false;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        handle.block_on(async {
            loop {
                if std::time::Instant::now() >= deadline {
                    break;
                }
                match tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv()).await {
                    Ok(Ok(ev)) => {
                        let ty = ev["type"].as_str().unwrap_or_default().to_string();
                        if ty == "cancelled" {
                            saw_cancelled = true;
                        }
                        if ty == "assistantText"
                            && ev["text"]
                                .as_str()
                                .unwrap_or_default()
                                .contains("follow-up answered")
                        {
                            saw_followup_text = true;
                        }
                        if ty == "turnEnd" && saw_cancelled {
                            saw_followup_terminal = true;
                            break;
                        }
                    }
                    _ => {}
                }
            }
        });

        assert!(
            entered.load(Ordering::SeqCst) >= 2,
            "the queued message must be claimed and stream a second LLM call (backend truth)"
        );
        assert!(saw_cancelled, "turn 1 aborts (cancelled)");
        assert!(
            saw_followup_text,
            "the follow-up turn's reply must be visible on the execute stream"
        );
        assert!(
            saw_followup_terminal,
            "the follow-up turn's terminal must be visible on the execute stream"
        );
    })
}

/// Every turn opening projects a legacy-shaped TaskStarted onto the execute
/// stream (`"TaskStarted"` + snake_case `session_id`) — the between-turns
/// marker live UIs re-arm on. Wire shape is load-bearing: the desktop's
/// TaskStarted handler reads `event.session_id`.
#[test]
fn turn_start_projects_legacy_task_started_event() {
    use futures::stream::BoxStream;
    use starriver_host::CODING_AGENT_NAME;
    use starriver_kernel::llm::{
        ContentBlock, ContentBlockType, FinishReason, GenerateOptions, LlmAdapter, LlmError,
        StreamChunk,
    };
    use std::sync::Arc;

    struct OneShotAdapter;
    impl LlmAdapter for OneShotAdapter {
        fn providers(&self) -> &[String] {
            static PROVIDERS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
            PROVIDERS.get_or_init(|| vec!["mock".into()])
        }
        fn stream(
            &self,
            _options: GenerateOptions,
        ) -> BoxStream<'static, Result<StreamChunk, LlmError>> {
            Box::pin(futures::stream::iter(vec![
                Ok(StreamChunk::BlockStart {
                    index: 0,
                    block_type: ContentBlockType::Text,
                }),
                Ok(StreamChunk::TextDelta {
                    index: 0,
                    text: "done".into(),
                }),
                Ok(StreamChunk::BlockEnd {
                    index: 0,
                    block: ContentBlock::Text("done".into()),
                }),
                Ok(StreamChunk::Finish {
                    reason: FinishReason::Stop,
                    replay_state: None,
                }),
            ]))
        }
        fn chat(
            &self,
            _options: GenerateOptions,
        ) -> futures::future::BoxFuture<'static, Result<String, LlmError>> {
            Box::pin(async { Ok("ok".into()) })
        }
    }

    with_temp_home(|dir| {
        let host = host_in(dir);
        host.debug_register_adapter(Arc::new(OneShotAdapter));
        let handle = host.runtime_handle();
        let mut rx = host
            .execute(
                "task".into(),
                None,
                None,
                Some(CODING_AGENT_NAME.into()),
                None,
                None,
                None,
            )
            .expect("execute");
        let mut saw_started = false;
        let mut session_id = String::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        handle.block_on(async {
            loop {
                if std::time::Instant::now() >= deadline {
                    break;
                }
                match tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv()).await {
                    Ok(Ok(ev)) => {
                        if ev["type"] == "TaskStarted" {
                            saw_started = true;
                            session_id = ev["session_id"].as_str().unwrap_or_default().to_string();
                            break;
                        }
                    }
                    _ => {}
                }
            }
        });
        assert!(saw_started, "turn/start must project a TaskStarted event");
        assert!(
            !session_id.is_empty(),
            "TaskStarted carries snake_case session_id (the desktop handler's read)"
        );
    })
}

/// Queue FIFO auto-claim (no Send Now): a message queued mid-stream runs as
/// the next turn when the live one finishes naturally — and its reply must
/// stay visible on the execute stream (same relay contract as Send Now).
#[test]
fn queued_message_auto_claim_streams_followup_turn() {
    use futures::stream::BoxStream;
    use futures::StreamExt as _;
    use starriver_host::CODING_AGENT_NAME;
    use starriver_kernel::llm::{
        ContentBlock, ContentBlockType, FinishReason, GenerateOptions, LlmAdapter, LlmError,
        StreamChunk,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Call 1 streams one chunk, then idles ~400ms before finishing — a
    /// live turn to queue into. Later calls finish immediately.
    struct SlowThenCompleteAdapter {
        entered: Arc<AtomicUsize>,
    }
    impl LlmAdapter for SlowThenCompleteAdapter {
        fn providers(&self) -> &[String] {
            static PROVIDERS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
            PROVIDERS.get_or_init(|| vec!["mock".into()])
        }
        fn stream(
            &self,
            _options: GenerateOptions,
        ) -> BoxStream<'static, Result<StreamChunk, LlmError>> {
            let n = self.entered.fetch_add(1, Ordering::SeqCst) + 1;
            if n == 1 {
                let stream = futures::stream::once(async {
                    Ok::<_, LlmError>(StreamChunk::BlockStart {
                        index: 0,
                        block_type: ContentBlockType::Text,
                    })
                });
                // Finite tail: one delayed Finish, then the stream ENDS —
                // the step's chunk loop exits on stream end, never on the
                // Finish chunk itself.
                let tail = futures::stream::once(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                    Ok::<_, LlmError>(StreamChunk::Finish {
                        reason: FinishReason::Stop,
                        replay_state: None,
                    })
                });
                Box::pin(stream.chain(tail))
            } else {
                Box::pin(futures::stream::iter(vec![
                    Ok(StreamChunk::BlockStart {
                        index: 0,
                        block_type: ContentBlockType::Text,
                    }),
                    Ok(StreamChunk::TextDelta {
                        index: 0,
                        text: "queued answer".into(),
                    }),
                    Ok(StreamChunk::BlockEnd {
                        index: 0,
                        block: ContentBlock::Text("queued answer".into()),
                    }),
                    Ok(StreamChunk::Finish {
                        reason: FinishReason::Stop,
                        replay_state: None,
                    }),
                ]))
            }
        }
        fn chat(
            &self,
            _options: GenerateOptions,
        ) -> futures::future::BoxFuture<'static, Result<String, LlmError>> {
            Box::pin(async { Ok("ok".into()) })
        }
    }

    with_temp_home(|dir| {
        let host = host_in(dir);
        let entered = Arc::new(AtomicUsize::new(0));
        host.debug_register_adapter(Arc::new(SlowThenCompleteAdapter {
            entered: Arc::clone(&entered),
        }));
        let handle = host.runtime_handle();
        let mut rx = host
            .execute(
                "first task".into(),
                None,
                None,
                Some(CODING_AGENT_NAME.into()),
                None,
                None,
                None,
            )
            .expect("execute");
        for _ in 0..500 {
            if entered.load(Ordering::SeqCst) > 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let mut session_id = String::new();
        handle.block_on(async {
            loop {
                match rx.try_recv() {
                    Ok(ev) => {
                        if ev["type"] == "sessionStarted" {
                            session_id = ev["sessionId"].as_str().unwrap_or_default().to_string();
                        }
                    }
                    _ => break,
                }
            }
        });
        assert!(!session_id.is_empty());
        // Queue while turn 1 streams; it must claim at the turn boundary.
        assert!(host
            .queue_add(session_id, "queued follow-up".into())
            .is_some());

        let mut saw_followup_text = false;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        handle.block_on(async {
            loop {
                if std::time::Instant::now() >= deadline {
                    break;
                }
                match tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv()).await {
                    Ok(Ok(ev)) => {
                        if ev["type"] == "assistantText"
                            && ev["text"]
                                .as_str()
                                .unwrap_or_default()
                                .contains("queued answer")
                        {
                            saw_followup_text = true;
                            break;
                        }
                    }
                    _ => {}
                }
            }
        });
        assert!(
            entered.load(Ordering::SeqCst) >= 2,
            "the queued message must run as a second LLM call"
        );
        assert!(
            saw_followup_text,
            "the auto-claimed follow-up turn must stream on the execute channel"
        );
    })
}
