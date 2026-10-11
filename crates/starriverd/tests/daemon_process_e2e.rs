//! Process-level E2E for the `starriverd` binary: start → /health → second
//! start yields existing → protocol invoke over HTTP → `--shutdown` cleans up
//! socket + daemon.json. Runs against the real binary with an isolated HOME.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[cfg(unix)]
fn daemon_bin() -> &'static str {
    env!("CARGO_BIN_EXE_starriverd")
}

#[cfg(unix)]
fn spawn_daemon(home: &std::path::Path) -> (Child, DaemonInfo) {
    let mut child = Command::new(daemon_bin())
        .env("HOME", home)
        .current_dir(home)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn starriverd");
    let stdout = child.stdout.take().expect("stdout");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match reader.read_line(&mut line) {
            Ok(0) => panic!("starriverd exited before printing startup line"),
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => panic!("read startup line: {error}"),
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for daemon startup line"
        );
    }
    let info: DaemonInfo = serde_json::from_str(line.trim())
        .unwrap_or_else(|e| panic!("bad startup line {line:?}: {e}"));
    (child, info)
}

#[cfg(unix)]
fn wait_exit(child: &mut Child, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            assert!(status.success(), "daemon exited with {status}");
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct DaemonInfo {
    port: u16,
    token: String,
    pid: u32,
    #[serde(default)]
    existing: bool,
}

#[cfg(unix)]
#[test]
fn daemon_lifecycle_start_yield_health_invoke_shutdown() {
    let home = tempfile::tempdir().expect("tempdir");
    let starriver_dir = home.path().join(".starriver");
    std::fs::create_dir_all(&starriver_dir).unwrap();

    // 1. First start owns the instance and prints connection info.
    let (mut daemon, info) = spawn_daemon(home.path());
    assert!(!info.existing, "first start must own");
    assert!(info.port > 0);
    assert!(!info.token.is_empty());
    assert!(starriver_dir.join("daemon.json").exists());

    // 2. /health responds without auth.
    let client = reqwest::blocking::Client::new();
    let health = client
        .get(format!("http://127.0.0.1:{}/health", info.port))
        .send()
        .expect("health request");
    assert_eq!(health.status(), 200);
    assert_eq!(health.text().unwrap(), "ok");

    // 3. Protocol invoke over HTTP with Bearer token.
    let response = client
        .post(format!("http://127.0.0.1:{}/api/v1/invoke", info.port))
        .bearer_auth(&info.token)
        .json(&serde_json::json!({
            "v": 1,
            "id": "e2e-1",
            "method": "tools.list",
            "params": {}
        }))
        .send()
        .expect("invoke request");
    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().unwrap();
    assert_eq!(body["ok"], true, "{body}");

    // 4. Second start yields the existing instance instead of double-binding.
    let (mut second, info2) = spawn_daemon(home.path());
    assert!(info2.existing, "second start must report existing");
    assert_eq!(info2.port, info.port);
    // The yielding process exits immediately.
    assert!(
        wait_exit(&mut second, Duration::from_secs(10)),
        "second start must exit quickly"
    );

    // 5. --shutdown cleanly stops the daemon and removes control artifacts.
    let shutdown = Command::new(daemon_bin())
        .env("HOME", home.path())
        .current_dir(home.path())
        .arg("--shutdown")
        .output()
        .expect("run --shutdown");
    assert!(
        shutdown.status.success(),
        "--shutdown failed: {}",
        String::from_utf8_lossy(&shutdown.stderr)
    );
    assert!(
        wait_exit(&mut daemon, Duration::from_secs(15)),
        "daemon did not exit after --shutdown"
    );
    assert!(
        !starriver_dir.join("daemon.json").exists(),
        "daemon.json must be removed on shutdown"
    );
    assert!(
        !starriver_dir.join("starriverd.sock").exists(),
        "control socket must be removed on shutdown"
    );

    // 6. After shutdown, /health is gone (server no longer listening).
    let after = client
        .get(format!("http://127.0.0.1:{}/health", info.port))
        .send();
    assert!(after.is_err(), "port must be closed after shutdown");
}

#[cfg(unix)]
#[test]
fn daemon_startup_line_is_valid_json() {
    let home = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(home.path().join(".starriver")).unwrap();
    let (mut daemon, info) = spawn_daemon(home.path());
    assert!(!info.existing);
    let shutdown = Command::new(daemon_bin())
        .env("HOME", home.path())
        .current_dir(home.path())
        .arg("--shutdown")
        .output()
        .expect("run --shutdown");
    assert!(shutdown.status.success());
    assert!(wait_exit(&mut daemon, Duration::from_secs(15)));
}

#[cfg(not(unix))]
#[test]
fn daemon_e2e_is_unix_only() {
    // Platform-specific test: skipped on non-unix platforms (Windows named
    // pipe is covered by separate lib unit tests).
}
