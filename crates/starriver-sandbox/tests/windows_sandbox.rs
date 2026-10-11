//! Windows end-to-end: real confined runs through `SystemSandbox` with the
//! `writetoken` backend (write-restricted token + grant ACEs + Job Object,
//! executed via `starriver-sbx-helper`). Child commands use `cmd /c` — the
//! sandbox is shell-agnostic and git-bash's `sh` is not guaranteed on every
//! Windows host. The lexer boundary (`ensure_workspace_writable`) has its
//! own Windows tests in `boundary.rs`.

#![cfg(target_os = "windows")]

use starriver_kernel::sandbox::{SandboxError, SandboxMode, SandboxPolicy, SandboxProvider};
use starriver_sandbox::{denial, SystemSandbox};

/// Confine, or skip the test when the writetoken chain is unavailable because
/// this shell cannot hold the spawn privileges — an unelevated UAC-filtered
/// token gets ERROR_PRIVILEGE_NOT_HELD (os error 1314) at
/// CreateProcessAsUserW, which no code can work around. Any other
/// unavailability is a real failure: CI runners (elevated) must stay green.
fn confine_or_privilege_skip(
    argv: Vec<String>,
    policy: &SandboxPolicy,
) -> Option<starriver_kernel::sandbox::ConfinedArgv> {
    match SystemSandbox::new().confine(argv, policy) {
        Ok(confined) => Some(confined),
        Err(SandboxError::Unavailable { detail, .. }) if detail.contains("os error 1314") => {
            eprintln!("skipped: writetoken needs an elevated shell ({detail})");
            None
        }
        Err(other) => panic!("writetoken must be available on Windows CI: {other:?}"),
    }
}

fn temp_ws(tag: &str) -> tempfile::TempDir {
    tempfile::TempDir::with_prefix(format!("sbx-{tag}-")).expect("temp workspace")
}

fn policy(mode: SandboxMode, root: &std::path::Path) -> SandboxPolicy {
    SandboxPolicy {
        mode,
        workspace_root: root.to_string_lossy().into_owned(),
        session_id: None,
    }
}

fn output_of(
    confined: starriver_kernel::sandbox::ConfinedArgv,
    root: &std::path::Path,
) -> std::process::Output {
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let mut cmd = tokio::process::Command::new(&confined.argv[0]);
        cmd.args(&confined.argv[1..]).current_dir(root);
        if let Some(hook) = &confined.spawn_hook {
            hook(&mut cmd).expect("spawn hook must apply cleanly");
        }
        cmd.output().await.expect("confined spawn")
    })
}

fn runner() -> &'static str {
    // The chain is cached; both "writetoken" or an Unavailable failure are
    // handled by the caller tests via SystemSandbox::confine errors.
    "writetoken"
}

fn echo_argv(text: &str) -> Vec<String> {
    vec!["cmd".into(), "/c".into(), format!("echo {text}")]
}

#[test]
fn confined_echo_runs_under_writetoken() {
    let ws = temp_ws("echo");
    let Some(confined) = confine_or_privilege_skip(
        echo_argv("confined-ok"),
        &policy(SandboxMode::WorkspaceWrite, ws.path()),
    ) else {
        return;
    };
    assert_eq!(confined.runner, "writetoken");
    let output = output_of(confined, ws.path());
    assert!(
        output.status.success(),
        "runner must exec a trivial command: {output:?}"
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("confined-ok"),
        "stdio must flow through the helper: {output:?}"
    );
}

#[test]
fn workspace_write_allows_workspace_and_temp_denies_outside() {
    let ws = temp_ws("ww");

    // Inside the workspace: allowed and the file really appears.
    let inside = ws.path().join("inside.txt");
    // Redirect paths stay unquoted: cmd.exe does not parse MSVC `\"`
    // escapes, so quoted paths arrive as literal `\"...\"` and the
    // redirection fails with "filename syntax is incorrect". TempDir
    // paths on dev boxes and CI runners are space-free, so no quotes
    // are needed.
    let Some(confined) = confine_or_privilege_skip(
        vec![
            "cmd".into(),
            "/c".into(),
            format!("echo x > {}", inside.display()),
        ],
        &policy(SandboxMode::WorkspaceWrite, ws.path()),
    ) else {
        return;
    };
    let output = output_of(confined, ws.path());
    assert!(
        output.status.success(),
        "workspace write must pass: {output:?}"
    );
    assert!(inside.exists(), "file must exist after confined write");

    // %TEMP%: whitelisted, same as the seatbelt/bwrap /tmp grant.
    let temp_file = std::env::temp_dir().join(format!("sbx-ww-{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&temp_file);
    let Some(confined) = confine_or_privilege_skip(
        vec![
            "cmd".into(),
            "/c".into(),
            format!("echo x > {}", temp_file.display()),
        ],
        &policy(SandboxMode::WorkspaceWrite, ws.path()),
    ) else {
        return;
    };
    let output = output_of(confined, ws.path());
    assert!(output.status.success(), "temp write must pass: {output:?}");
    assert!(temp_file.exists());
    let _ = std::fs::remove_file(&temp_file);

    // Outside (user profile root is not a granted root): denied, and the
    // failure normalizes to a sandbox denial, not a plain failure.
    let outside = std::env::var("USERPROFILE")
        .map(std::path::PathBuf::from)
        .expect("USERPROFILE on Windows")
        .join(format!("sbx-outside-{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&outside);
    let Some(confined) = confine_or_privilege_skip(
        vec![
            "cmd".into(),
            "/c".into(),
            format!("echo x > {}", outside.display()),
        ],
        &policy(SandboxMode::WorkspaceWrite, ws.path()),
    ) else {
        return;
    };
    let output = output_of(confined, ws.path());
    assert!(
        !output.status.success(),
        "outside write must fail: {output:?}"
    );
    assert!(!outside.exists(), "outside write must not land");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let sig = denial::classify(runner(), &stderr)
        .unwrap_or_else(|| panic!("outside write must normalize to a sandbox denial: {stderr}"));
    assert_eq!(sig.errno_tag, "EACCES");
    let _ = std::fs::remove_file(&outside);
}

#[test]
fn read_only_denies_writes_everywhere_but_keeps_reads() {
    let ws = temp_ws("ro");

    // Reads work under confinement (WRITE_RESTRICTED never restricts reads).
    let Some(confined) = confine_or_privilege_skip(
        echo_argv("still-readable"),
        &policy(SandboxMode::ReadOnly, ws.path()),
    ) else {
        return;
    };
    let output = output_of(confined, ws.path());
    assert!(output.status.success(), "reads must work: {output:?}");

    // Zero-ACL-mutation proof: even the workspace itself is write-denied.
    let probe = ws.path().join("denied.txt");
    let Some(confined) = confine_or_privilege_skip(
        vec![
            "cmd".into(),
            "/c".into(),
            format!("echo x > {}", probe.display()),
        ],
        &policy(SandboxMode::ReadOnly, ws.path()),
    ) else {
        return;
    };
    let output = output_of(confined, ws.path());
    assert!(!output.status.success(), "read-only write must fail");
    assert!(!probe.exists(), "read-only must not create the probe file");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let sig = denial::classify(runner(), &stderr)
        .unwrap_or_else(|| panic!("read-only write must normalize to a denial: {stderr}"));
    assert_eq!(sig.errno_tag, "EACCES");

    // And %TEMP% too — read-only grants nothing anywhere.
    let temp_probe = std::env::temp_dir().join(format!("sbx-ro-{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&temp_probe);
    let Some(confined) = confine_or_privilege_skip(
        vec![
            "cmd".into(),
            "/c".into(),
            format!("echo x > {}", temp_probe.display()),
        ],
        &policy(SandboxMode::ReadOnly, ws.path()),
    ) else {
        return;
    };
    let output = output_of(confined, ws.path());
    assert!(!output.status.success(), "read-only temp write must fail");
    assert!(!temp_probe.exists());
}

#[test]
fn helper_startup_failure_is_confine_not_denial() {
    // Synthetic: the classifier must distinguish a backend startup failure
    // (helper error line, empty stdout) from an in-sandbox denial.
    let stderr = "starriver-sbx: CreateRestrictedToken: Access is denied.\n";
    assert!(denial::runner_startup_failed("writetoken", "", stderr));
    // Command noise with stdout traffic is not a startup failure.
    assert!(!denial::runner_startup_failed(
        "writetoken",
        "out\n",
        stderr
    ));
    // stderr noise not led by the helper prefix either.
    assert!(!denial::runner_startup_failed("writetoken", "", "error: x"));
}
