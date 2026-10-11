// Standalone stdio MCP server for the starriver shared memory store
// (docs/37). Any MCP-capable agent — current or future — connects to this
// binary and gets read/write
// access to the same memories starriver extracts.
//
// Wire protocol: JSON-RPC 2.0 over stdio (one message per line), MCP
// initialize / tools/list / tools/call. No SDK dependency — the surface is
// small enough to hand-roll, which keeps the binary dependency-free apart
// from starriver-memory itself.

use std::io::BufRead;
use std::path::{Path, PathBuf};

use starriver_memory::{MemoryEntry, MemorySource, MemoryStore, GLOBAL_PROJECT};
// MemoryConfig is a test-only construction detail (lib code takes the store
// as built); keep it out of the production import surface.
#[cfg(test)]
use starriver_memory::MemoryConfig;

pub const SERVER_NAME: &str = "starriver-memory";
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Project scope: the current working directory at server start (the agent
/// launches the server from its workspace), or `_global` when unset.
pub fn project_from_cwd(cwd: Option<&Path>) -> String {
    cwd.map(starriver_memory::ingest::project_id_from_cwd)
        .unwrap_or_else(|| GLOBAL_PROJECT.to_string())
}

pub fn tool_specs() -> serde_json::Value {
    serde_json::json!([
        {
            "name": "memory_recall",
            "description": "Recall relevant shared memories for a query. Returns the top entries for this project plus global ones.",
            "inputSchema": {
                "type": "object",
                "properties": { "query": { "type": "string", "description": "What you are about to do" } },
                "required": ["query"]
            }
        },
        {
            "name": "memory_write",
            "description": "Persist a durable, cross-session fact into the shared memory store.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Short english slug" },
                    "description": { "type": "string", "description": "One-line summary" },
                    "body": { "type": "string", "description": "The fact (1-3 sentences)" },
                    "global": { "type": "boolean", "description": "Store cross-project instead of this project" }
                },
                "required": ["name", "description", "body"]
            }
        },
        {
            "name": "memory_list",
            "description": "List shared memories for this project (plus global).",
            "inputSchema": {
                "type": "object",
                "properties": { "global": { "type": "boolean" } }
            }
        },
        {
            "name": "memory_delete",
            "description": "Delete one shared memory by slug.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "global": { "type": "boolean" }
                },
                "required": ["name"]
            }
        }
    ])
}

/// Pure request handler: takes a parsed JSON-RPC request plus the store and
/// project scope, returns the response value (already wrapped for transport).
pub fn handle_request(
    request: &serde_json::Value,
    store: &MemoryStore,
    project: &str,
    source_agent: &str,
) -> serde_json::Value {
    let id = request
        .get("id")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let method = request.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let params = request
        .get("params")
        .cloned()
        .unwrap_or(serde_json::json!({}));
    let result = match method {
        "initialize" => serde_json::json!({
            "protocolVersion": params.get("protocolVersion").cloned().unwrap_or(serde_json::json!("2024-11-05")),
            "capabilities": { "tools": {} },
            "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION }
        }),
        "notifications/initialized" | "initialized" => return serde_json::Value::Null,
        "tools/list" => serde_json::json!({ "tools": tool_specs() }),
        "tools/call" => {
            let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or(serde_json::json!({}));
            match call_tool(name, &args, store, project, source_agent) {
                Ok(text) => {
                    serde_json::json!({ "content": [ { "type": "text", "text": text } ], "isError": false })
                }
                Err(e) => {
                    serde_json::json!({ "content": [ { "type": "text", "text": format!("error: {e}") } ], "isError": true })
                }
            }
        }
        "ping" => serde_json::json!({}),
        _ => {
            return serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("method not found: {method}") }
            })
        }
    };
    serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn call_tool(
    name: &str,
    args: &serde_json::Value,
    store: &MemoryStore,
    project: &str,
    source_agent: &str,
) -> anyhow::Result<String> {
    match name {
        "memory_recall" => {
            let query = args.get("query").and_then(|q| q.as_str()).unwrap_or("");
            let now = starriver_memory::now_unix();
            let entries = store.recall(project, query, now);
            Ok(render_entries(&entries))
        }
        "memory_write" => {
            let get = |k: &str| {
                args.get(k)
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| anyhow::anyhow!("missing field: {k}"))
            };
            let scope = if args.get("global").and_then(|g| g.as_bool()) == Some(true) {
                GLOBAL_PROJECT
            } else {
                project
            };
            let entry = MemoryEntry {
                name: starriver_memory::slugify(&get("name")?),
                description: get("description")?,
                body: get("body")?,
                created: starriver_memory::now_unix(),
                updated: starriver_memory::now_unix(),
                source: MemorySource::Extract,
                source_agent: source_agent.to_string(),
                project: scope.to_string(),
            };
            store.upsert(entry)?;
            Ok(format!("stored memory in `{scope}`"))
        }
        "memory_list" => {
            let scope = if args.get("global").and_then(|g| g.as_bool()) == Some(true) {
                GLOBAL_PROJECT
            } else {
                project
            };
            let entries = store.list(scope)?;
            Ok(render_entries(&entries))
        }
        "memory_delete" => {
            let name = args
                .get("name")
                .and_then(|n| n.as_str())
                .ok_or_else(|| anyhow::anyhow!("missing field: name"))?;
            let scope = if args.get("global").and_then(|g| g.as_bool()) == Some(true) {
                GLOBAL_PROJECT
            } else {
                project
            };
            // Writes are keyed by slugify(name) — a delete must normalize
            // the same way or it silently no-ops while still reporting
            // "deleted".
            let slug = starriver_memory::slugify(name);
            if !store.exists(scope, &slug)? {
                return Err(anyhow::anyhow!(
                    "no memory named `{slug}` in `{scope}`"
                ));
            }
            store.delete(scope, &slug)?;
            Ok(format!("deleted `{slug}` from `{scope}`"))
        }
        _ => Err(anyhow::anyhow!("unknown tool: {name}")),
    }
}

fn render_entries(entries: &[MemoryEntry]) -> String {
    if entries.is_empty() {
        return "(no memories)".to_string();
    }
    entries
        .iter()
        .map(|e| {
            format!(
                "- [{}] (from {}) {}\n  {}",
                e.name, e.source_agent, e.description, e.body
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Default store root: `~/.starriver/memory` (the shared cross-agent root).
pub fn default_root() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    Path::new(&home).join(".starriver").join("memory")
}

/// Run the stdio loop. Each input line is one JSON-RPC message; each response
/// is one line on stdout. Notifications produce no output.
pub fn run(store: MemoryStore, project: String, source_agent: String) -> anyhow::Result<()> {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    use std::io::Write;
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(request) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let response = handle_request(&request, &store, &project, &source_agent);
        if response.is_null() {
            continue;
        }
        writeln!(out, "{response}")?;
        out.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, MemoryStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(dir.path().join("m"), MemoryConfig::default());
        (dir, store)
    }

    fn req(method: &str, params: serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params })
    }

    #[test]
    fn initialize_handshake() {
        let (_d, store) = store();
        let resp = handle_request(
            &req(
                "initialize",
                serde_json::json!({ "protocolVersion": "2024-11-05" }),
            ),
            &store,
            "p",
            "codex",
        );
        assert_eq!(resp["result"]["serverInfo"]["name"], SERVER_NAME);
        assert_eq!(resp["id"], 1);
    }

    #[test]
    fn initialized_notification_is_silent() {
        let (_d, store) = store();
        assert!(handle_request(
            &serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
            &store,
            "p",
            "codex"
        )
        .is_null());
    }

    #[test]
    fn tools_list_has_four_tools() {
        let (_d, store) = store();
        let resp = handle_request(
            &req("tools/list", serde_json::json!({})),
            &store,
            "p",
            "codex",
        );
        let tools = resp["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            vec![
                "memory_recall",
                "memory_write",
                "memory_list",
                "memory_delete"
            ]
        );
    }

    #[test]
    fn write_then_recall_round_trip() {
        let (_d, store) = store();
        let write = req(
            "tools/call",
            serde_json::json!({
                "name": "memory_write",
                "arguments": { "name": "Use pnpm", "description": "use pnpm for deps", "body": "deps via pnpm" }
            }),
        );
        let resp = handle_request(&write, &store, "proj", "codex");
        assert_eq!(resp["result"]["isError"], false);
        let recall = req(
            "tools/call",
            serde_json::json!({ "name": "memory_recall", "arguments": { "query": "pnpm" } }),
        );
        let resp = handle_request(&recall, &store, "proj", "claude-code");
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("use-pnpm"));
        assert!(
            text.contains("from codex"),
            "cross-agent provenance visible"
        );
    }

    #[test]
    fn unknown_method_is_error() {
        let (_d, store) = store();
        let resp = handle_request(&req("nope", serde_json::json!({})), &store, "p", "x");
        assert_eq!(resp["error"]["code"], -32601);
    }

    #[test]
    fn missing_tool_argument_is_tool_error() {
        let (_d, store) = store();
        let call = req(
            "tools/call",
            serde_json::json!({ "name": "memory_write", "arguments": { "name": "x" } }),
        );
        let resp = handle_request(&call, &store, "p", "x");
        assert_eq!(resp["result"]["isError"], true);
    }
}
