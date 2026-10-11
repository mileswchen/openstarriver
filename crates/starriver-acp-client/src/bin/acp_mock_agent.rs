//! A minimal scripted ACP agent used by the client crate's e2e tests:
//! handshake → session/new → session/prompt → two text chunks + final →
//! end_turn. Answers every permission ask with the scripted outcome chosen
//! by argv (`allow`/`reject`), and can stall (`stall`) to exercise timeouts.

use std::io::{BufRead, Write};

fn send(stdout: &mut impl Write, message: &str) {
    let _ = stdout.write_all(message.as_bytes());
    let _ = stdout.write_all(b"\n");
    let _ = stdout.flush();
}

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let stdin = std::io::stdin();
    let mut prompt_id = String::new();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(message) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let id = message.get("id").cloned();
        match message.get("method").and_then(|m| m.as_str()) {
            Some("initialize") => {
                send(
                    &mut out,
                    &serde_json::json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": {
                            "protocolVersion": 1,
                            "agentCapabilities": {"loadSession": false},
                            "authMethods": [],
                        }
                    })
                    .to_string(),
                );
            }
            Some("session/new") => {
                send(
                    &mut out,
                    &serde_json::json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": {"sessionId": "mock-session"}
                    })
                    .to_string(),
                );
            }
            Some("session/prompt") => {
                if mode == "stall" {
                    // Never answer — the client's timeout must fire.
                    continue;
                }
                prompt_id = id.map(|v| v.to_string()).unwrap_or_default();
                let session = message
                    .pointer("/params/sessionId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let update = |update: serde_json::Value| {
                    serde_json::json!({
                        "jsonrpc": "2.0", "method": "session/update",
                        "params": {"sessionId": session, "update": update}
                    })
                    .to_string()
                };
                send(
                    &mut out,
                    &update(serde_json::json!({
                        "type": "agent_message_chunk",
                        "content": [{"type": "text", "text": "mock "}]
                    })),
                );
                send(
                    &mut out,
                    &update(serde_json::json!({
                        "type": "agent_message_chunk",
                        "content": [{"type": "text", "text": "answer"}]
                    })),
                );
                let prompt_id = prompt_id.parse::<u64>().unwrap_or(0);
                send(
                    &mut out,
                    &serde_json::json!({
                        "jsonrpc": "2.0", "id": prompt_id,
                        "result": {"stopReason": "end_turn"}
                    })
                    .to_string(),
                );
            }
            Some("session/request_permission") => {
                let choice = if mode == "allow" { "allow_once" } else { "reject" };
                send(
                    &mut out,
                    &serde_json::json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": {"outcome": {"id": choice}}
                    })
                    .to_string(),
                );
            }
            _ => {}
        }
    }
}
