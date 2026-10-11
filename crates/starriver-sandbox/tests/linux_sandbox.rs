//! Linux end-to-end: real confined runs through `SystemSandbox` (whichever
//! runner the probe chain selects — bubblewrap or Landlock) plus a direct
//! Landlock-runner pass, which must work even where unprivileged userns are
//! gated. WI-11's reference environment: Debian 13 + kernel 6.12 + bwrap 0.12.

#![cfg(target_os = "linux")]

use starriver_kernel::sandbox::{SandboxMode, SandboxPolicy, SandboxProvider};
use starriver_sandbox::{denial, SystemSandbox};

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

#[test]
fn confined_echo_runs_under_the_selected_runner() {
    let ws = temp_ws("echo");
    let confined = SystemSandbox::new()
        .confine(
            vec!["sh".into(), "-c".into(), "echo confined-ok".into()],
            &policy(SandboxMode::WorkspaceWrite, ws.path()),
        )
        .unwrap();
    let output = output_of(confined, ws.path());
    assert!(
        output.status.success(),
        "runner must exec a trivial command: {output:?}"
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("confined-ok"));
}

#[test]
fn workspace_write_denies_outside_and_allows_inside() {
    let ws = temp_ws("ww");
    let confined = SystemSandbox::new()
        .confine(
            vec![
                "sh".into(),
                "-c".into(),
                "touch /usr/denied-by-sandbox".into(),
            ],
            &policy(SandboxMode::WorkspaceWrite, ws.path()),
        )
        .unwrap();
    let runner = confined.runner.clone();
    let output = output_of(confined, ws.path());
    assert!(
        !output.status.success(),
        "outside write must fail (runner={runner})"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let sig = denial::classify(&runner, &stderr)
        .unwrap_or_else(|| panic!("outside write must normalize to a sandbox denial: {stderr}"));
    assert_eq!(sig.runner, runner);

    let inside = ws.path().join("inside.txt");
    let confined = SystemSandbox::new()
        .confine(
            vec![
                "sh".into(),
                "-c".into(),
                format!("touch {}", inside.display()),
            ],
            &policy(SandboxMode::WorkspaceWrite, ws.path()),
        )
        .unwrap();
    let output = output_of(confined, ws.path());
    assert!(
        output.status.success(),
        "workspace write must pass: {output:?}"
    );
    assert!(inside.exists());
}

#[test]
fn read_only_denies_writes_but_keeps_reads() {
    let ws = temp_ws("ro");
    let confined = SystemSandbox::new()
        .confine(
            vec!["sh".into(), "-c".into(), "echo still-readable".into()],
            &policy(SandboxMode::ReadOnly, ws.path()),
        )
        .unwrap();
    let output = output_of(confined, ws.path());
    assert!(output.status.success(), "reads must work: {output:?}");

    let probe = std::env::temp_dir().join(format!("sbx-ro-probe-{}", std::process::id()));
    let confined = SystemSandbox::new()
        .confine(
            vec![
                "sh".into(),
                "-c".into(),
                format!("touch {}", probe.display()),
            ],
            &policy(SandboxMode::ReadOnly, ws.path()),
        )
        .unwrap();
    let runner = confined.runner.clone();
    let output = output_of(confined, ws.path());
    assert!(!output.status.success(), "read-only write must fail");
    assert!(!probe.exists(), "read-only must not create the probe file");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        denial::classify(&runner, &stderr).is_some(),
        "read-only write must normalize to a denial: {stderr}"
    );
}

#[test]
fn landlock_runner_confines_directly_without_userns() {
    // Deterministic native-runner pass: works even when the chain picked
    // bubblewrap, and on hosts where userns are gated entirely.
    let ws = temp_ws("ll");
    let confined = starriver_sandbox::linux::landlock::confine(
        vec!["sh".into(), "-c".into(), "echo landlock-ok".into()],
        SandboxMode::WorkspaceWrite,
        ws.path().to_str().unwrap(),
    )
    .unwrap();
    assert_eq!(confined.runner, "landlock");
    assert!(confined.spawn_hook.is_some());
    let output = output_of(confined, ws.path());
    assert!(output.status.success(), "landlock run: {output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("landlock-ok"));

    let confined = starriver_sandbox::linux::landlock::confine(
        vec![
            "sh".into(),
            "-c".into(),
            "touch /usr/denied-by-landlock".into(),
        ],
        SandboxMode::WorkspaceWrite,
        ws.path().to_str().unwrap(),
    )
    .unwrap();
    let output = output_of(confined, ws.path());
    assert!(
        !output.status.success(),
        "landlock must deny outside writes"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let sig = denial::classify("landlock", &stderr)
        .unwrap_or_else(|| panic!("landlock denial must normalize: {stderr}"));
    assert_eq!(sig.errno_tag, "EACCES");
}

#[test]
fn denial_startup_failure_signature_is_recognized() {
    // Synthetic: the classifier must distinguish a backend startup failure
    // from an in-sandbox denial without needing a broken host.
    let stderr = "bwrap: Setting up uid map: Permission denied\n";
    assert!(denial::runner_startup_failed("bwrap", "", stderr));
    assert!(!denial::runner_startup_failed("bwrap", "out\n", stderr));
}
