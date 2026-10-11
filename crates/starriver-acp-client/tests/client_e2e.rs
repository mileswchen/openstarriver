//! WI-112 e2e: the ACP client drives the in-crate mock agent end-to-end —
//! handshake, prompt streaming, stopReason, timeout, and spawn failure.

use std::time::Duration;

use starriver_acp_client::{run_prompt, AcpAgentSpec, PermissionPolicy};

fn spec() -> AcpAgentSpec {
    AcpAgentSpec {
        command: env!("CARGO_BIN_EXE_acp-mock-agent").to_string(),
        args: vec![],
        cwd: None,
        permissions: PermissionPolicy::Reject,
    }
}

#[test]
fn prompt_streams_chunks_and_returns_end_turn() {
    let result = run_prompt(&spec(), "what is the answer", Duration::from_secs(20))
        .expect("run completes");
    assert_eq!(result.text, "mock answer");
    assert_eq!(result.stop_reason, "end_turn");
}

#[test]
fn stalled_agent_times_out_with_a_clear_error() {
    let mut stalled = spec();
    stalled.args = vec!["stall".into()];
    let error =
        run_prompt(&stalled, "hello", Duration::from_secs(2)).expect_err("must time out");
    assert!(error.contains("timed out"), "{error}");
}

#[test]
fn missing_binary_reports_spawn_failure() {
    let spec = AcpAgentSpec {
        command: "/definitely/not/a/real/binary".into(),
        args: vec![],
        cwd: None,
        permissions: PermissionPolicy::Reject,
    };
    let error = run_prompt(&spec, "hi", Duration::from_secs(5)).expect_err("must fail");
    assert!(error.contains("spawning ACP agent"), "{error}");
}
