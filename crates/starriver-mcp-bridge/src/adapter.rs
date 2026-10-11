use super::transport::{Connection, HttpConnection, StdioConnection};
use crate::config::{McpServerConfig, TransportType};
use std::sync::Arc;

/// Resolves an OAuth bearer token for a remote server at request time.
pub type TokenProvider = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// Client half of the MCP `initialize` handshake, shared by discovery,
/// the tool executor's lazy re-launch path, and health restarts.
pub(crate) fn initialize_params() -> serde_json::Value {
    serde_json::json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {},
        "clientInfo": {"name": "starriver-desktop", "version": "0.1.0"},
    })
}

/// OpenAI function names must match `^[a-zA-Z0-9_-]+$`; MCP server/tool names are
/// user-configured and can legally contain dots, spaces, slashes, etc. Sanitize the
/// registry-facing tool name so a bad char never breaks the API request. Routing back
/// to the server uses the original `server_name`/`tool_name`, so the sanitized name is
/// only ever a registry/request label.
pub(crate) fn sanitize_tool_name(s: &str) -> String {
    // Same normalization core::tools::registry used for OpenAI function names:
    // keep `^[a-zA-Z0-9_-]+$`, non-empty.
    let mut out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        out.push_str("tool");
    }
    out
}

/// Build a transport-appropriate connection for a server config.
pub(crate) fn build_connection(
    server_name: &str,
    config: &McpServerConfig,
    token_provider: TokenProvider,
) -> Connection {
    match config.transport {
        TransportType::Stdio => Connection::Stdio(StdioConnection::new(server_name.to_string())),
        TransportType::Http => {
            let mut h = HttpConnection::new(server_name.to_string(), token_provider);
            h.set_url(config.url.clone().unwrap_or_default());
            Connection::Http(h)
        }
        // Sse is not wired to a connection yet; fall back to an empty stdio.
        TransportType::Sse => Connection::Stdio(StdioConnection::new(server_name.to_string())),
    }
}

/// One tool discovered from an MCP server, before it is bound to a
/// transport-specific adapter.
pub struct McpToolInfo {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    /// Server-declared behavioral hints (readOnlyHint/destructiveHint/...),
    /// defaulted when the server omits them. Drives write-semantics
    /// negotiation at definition-build time.
    pub annotations: crate::write_semantics::McpToolAnnotations,
}

/// Send the MCP `initialize` handshake over an already-launched connection.
pub(crate) async fn handshake(conn: &mut Connection) -> anyhow::Result<()> {
    conn.send_request("initialize", initialize_params()).await?;
    Ok(())
}

/// Handshake + `tools/list` over a caller-owned connection so discovery,
/// restart, and health-check paths can all share one connection lifecycle.
/// A stdio connection must already be launched; HTTP is launch-free.
pub(crate) async fn fetch_tool_list_on(conn: &mut Connection) -> anyhow::Result<Vec<McpToolInfo>> {
    handshake(conn).await?;

    let result = conn
        .send_request("tools/list", serde_json::json!({}))
        .await?;
    let tools = result["tools"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("tools/list returned no tools array"))?;

    Ok(tools
        .iter()
        .map(|tool| McpToolInfo {
            name: tool["name"].as_str().unwrap_or("unknown").to_string(),
            description: tool["description"].as_str().unwrap_or("").to_string(),
            input_schema: tool
                .get("inputSchema")
                .cloned()
                .unwrap_or(serde_json::json!({})),
            annotations: tool
                .get("annotations")
                .cloned()
                .and_then(|raw| serde_json::from_value(raw).ok())
                .unwrap_or_default(),
        })
        .collect())
}

/// Protocol extension capabilities observed during discovery: whether the
/// server answered `resources/list` / `prompts/list`. Servers that reply with
/// a JSON-RPC error (typically method not found) simply lack the surface.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct McpCapabilities {
    pub resources: bool,
    pub prompts: bool,
}

/// Probe `resources/list` and `prompts/list` once on an already-handshoken
/// connection. Each probe is one request; failures are swallowed into
/// `false` (capability absent), never fatal to discovery.
pub(crate) async fn probe_capabilities(conn: &mut Connection) -> McpCapabilities {
    let resources = conn
        .send_request("resources/list", serde_json::json!({}))
        .await
        .is_ok();
    let prompts = conn
        .send_request("prompts/list", serde_json::json!({}))
        .await
        .is_ok();
    McpCapabilities { resources, prompts }
}

/// Infer a minimal, schema-valid sample value for a single JSON-Schema property.
fn sample_value(prop: &serde_json::Value) -> serde_json::Value {
    if let Some(e) = prop.get("enum").and_then(|e| e.as_array()) {
        if let Some(first) = e.first() {
            return first.clone();
        }
    }
    match prop.get("type").and_then(|t| t.as_str()) {
        Some("string") => serde_json::json!("test"),
        Some("integer") => serde_json::json!(1),
        Some("number") => serde_json::json!(1),
        Some("boolean") => serde_json::json!(false),
        Some("array") => {
            // A single sample element beats an empty array for tools that require a
            // non-empty list (e.g. a `sources` param of platform IDs).
            if let Some(items) = prop.get("items") {
                serde_json::json!([sample_value(items)])
            } else {
                serde_json::json!([])
            }
        }
        Some("object") => {
            if let Some(props) = prop.get("properties").and_then(|p| p.as_object()) {
                let mut obj = serde_json::Map::new();
                for (k, v) in props {
                    obj.insert(k.clone(), sample_value(v));
                }
                serde_json::Value::Object(obj)
            } else {
                serde_json::json!({})
            }
        }
        _ => {
            if let Some(any_of) = prop.get("anyOf").and_then(|a| a.as_array()) {
                if let Some(first) = any_of.first() {
                    return sample_value(first);
                }
            }
            serde_json::json!("test")
        }
    }
}

/// Build a minimal arguments object for a tool from its JSON Schema (properties only).
pub fn sample_params(schema: &serde_json::Value) -> serde_json::Value {
    let props = schema.get("properties").and_then(|p| p.as_object());
    let mut obj = serde_json::Map::new();
    if let Some(props) = props {
        for (key, prop) in props {
            obj.insert(key.clone(), sample_value(prop));
        }
    }
    serde_json::Value::Object(obj)
}

/// Test a single tool call end-to-end: initialize → tools/list (to find the tool's
/// inputSchema) → tools/call with schema-inferred sample arguments. Returns the raw
/// result JSON (the tool's actual output) or an error.
pub async fn test_tool_call(
    server_name: &str,
    config: &McpServerConfig,
    tool_name: &str,
    token_provider: TokenProvider,
) -> anyhow::Result<serde_json::Value> {
    let mut conn = build_connection(server_name, config, token_provider);
    if let Connection::Stdio(c) = &mut conn {
        c.launch(config).await?;
    }

    conn.send_request("initialize", initialize_params()).await?;

    let result = conn
        .send_request("tools/list", serde_json::json!({}))
        .await?;
    let tools = result["tools"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("tools/list returned no tools array"))?;
    let schema = tools
        .iter()
        .find(|t| t["name"].as_str() == Some(tool_name))
        .and_then(|t| t.get("inputSchema"))
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let args = sample_params(&schema);

    conn.send_request(
        "tools/call",
        serde_json::json!({
            "name": tool_name,
            "arguments": args,
        }),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_token() -> TokenProvider {
        Arc::new(|| None)
    }

    #[test]
    fn test_parse_mcp_config() {
        let config: crate::config::McpConfig = serde_yaml::from_str(
            r#"
mcp_servers:
  github:
    transport: stdio
    command: npx
    args:
      - "-y"
      - "@anthropic/mcp-server-github"
    env:
      GITHUB_TOKEN: "ghp_xxx"
        "#,
        )
        .unwrap();
        assert_eq!(config.mcp_servers.len(), 1);
        let gh = config.mcp_servers.get("github").unwrap();
        assert_eq!(gh.command, "npx");
        assert_eq!(gh.args.len(), 2);
    }

    #[test]
    fn test_enabled_servers_filter() {
        let config: crate::config::McpConfig = serde_yaml::from_str(
            r#"
mcp_servers:
  gh:
    transport: stdio
    command: npx
    args: []
    env: {}
  search:
    transport: sse
    command: ""
    args: []
    env: {}
  remote:
    transport: http
    command: ""
    args: []
    env: {}
    url: "https://mcp.example.com/mcp"
        "#,
        )
        .unwrap();
        let enabled = config.enabled_servers();
        assert_eq!(enabled.len(), 2);
        // HashMap iteration order is non-deterministic — check membership, not position.
        let names: std::collections::HashSet<&str> =
            enabled.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains("gh"));
        assert!(names.contains("remote"));
        assert!(!names.contains("search"));
    }

    #[test]
    fn test_mcp_config_parse_with_env() {
        let config: crate::config::McpConfig = serde_yaml::from_str(
            r#"
mcp_servers:
  api:
    transport: stdio
    command: python
    args: ["-m", "my_server"]
    env:
      API_KEY: "secret123"
        "#,
        )
        .unwrap();
        let api = config.mcp_servers.get("api").unwrap();
        assert_eq!(api.env.get("API_KEY").unwrap(), "secret123");
    }

    #[test]
    fn test_mcp_config_default_path() {
        let path = crate::config::McpConfig::default_path();
        assert!(path.is_some());
    }

    #[test]
    fn test_discover_tools_not_launched() {
        let config = crate::config::McpServerConfig {
            transport: crate::config::TransportType::Stdio,
            command: "/nonexistent/binary".into(),
            args: vec![],
            env: Default::default(),
            url: None,
            oauth: None,
            deny_in_readonly_tools: Vec::new(),
            tool_timeout_secs: None,
            per_tool_timeout_secs: Default::default(),
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(crate::definition::discover_bridge(
            "test",
            &config,
            no_token(),
        ));
        assert!(result.is_err());
    }

    #[test]
    fn test_mcp_tool_builds_http_connection_for_http_transport() {
        let cfg = crate::config::McpServerConfig {
            transport: crate::config::TransportType::Http,
            command: String::new(),
            args: vec![],
            env: Default::default(),
            url: Some("https://mcp.slack.com/mcp".into()),
            oauth: None,
            deny_in_readonly_tools: Vec::new(),
            tool_timeout_secs: None,
            per_tool_timeout_secs: Default::default(),
        };
        let conn = build_connection("slack", &cfg, no_token());
        assert!(matches!(conn, crate::transport::Connection::Http(_)));
    }

    #[test]
    fn test_mcp_tool_name_sanitized_for_openai_pattern() {
        // Dots/spaces/slashes in a server or tool name must not leak into the
        // registry name (OpenAI requires ^[a-zA-Z0-9_-]+$ for function names).
        let cfg = crate::config::McpServerConfig {
            transport: crate::config::TransportType::Stdio,
            command: "echo".into(),
            args: vec![],
            env: Default::default(),
            url: None,
            oauth: None,
            deny_in_readonly_tools: Vec::new(),
            tool_timeout_secs: None,
            per_tool_timeout_secs: Default::default(),
        };
        let name = sanitize_tool_name(&format!("mcp_{}_{}", "my context.tools/2", "get user!"));
        let _ = cfg;
        assert!(
            name.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
            "registry name {:?} must match ^[a-zA-Z0-9_-]+$",
            name
        );
    }

    #[test]
    fn test_sample_params_infers_each_type() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "limit": {"type": "integer"},
                "ratio": {"type": "number"},
                "verbose": {"type": "boolean"},
                "tags": {"type": "array"},
                "sources": {"type": "array", "items": {"type": "integer"}},
                "mode": {"type": "string", "enum": ["fast", "slow"]},
                "nested": {"type": "object", "properties": {"x": {"type": "integer"}}}
            }
        });
        let args = sample_params(&schema);
        assert_eq!(args["query"], "test");
        assert_eq!(args["limit"], 1);
        assert_eq!(args["ratio"], 1);
        assert_eq!(args["verbose"], false);
        assert_eq!(args["tags"], serde_json::json!([])); // array without items → empty
        assert_eq!(args["sources"], serde_json::json!([1])); // array of integer → [1]
        assert_eq!(args["mode"], "fast"); // enum → first value
        assert_eq!(args["nested"]["x"], 1); // nested object recursion
    }

    #[test]
    fn test_sample_params_array_of_enum_takes_first_item() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "sources": {"type": "array", "items": {"type": "integer", "enum": [1, 3, 7]}}
            }
        });
        let args = sample_params(&schema);
        assert_eq!(args["sources"], serde_json::json!([1]));
    }

    #[test]
    fn test_sample_params_empty_without_properties() {
        assert_eq!(sample_params(&serde_json::json!({})), serde_json::json!({}));
    }

    #[test]
    fn test_mcp_config_save_and_load_roundtrip() {
        use crate::config::{McpConfig, TransportType};
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mcp.yaml");
        let mut cfg = McpConfig {
            mcp_servers: Default::default(),
        };
        cfg.add_server(
            "test-srv".into(),
            McpServerConfig {
                transport: TransportType::Stdio,
                command: "echo".into(),
                args: vec!["hello".into()],
                env: [("KEY".into(), "val".into())].into(),
                url: None,
                oauth: None,
                deny_in_readonly_tools: Vec::new(),
                tool_timeout_secs: None,
                per_tool_timeout_secs: Default::default(),
            },
        );
        cfg.save(&path).unwrap();
        let loaded = McpConfig::load(&path).unwrap();
        assert!(loaded.get_server("test-srv").is_some());
        assert_eq!(loaded.get_server("test-srv").unwrap().command, "echo");
    }

    #[test]
    fn test_mcp_config_add_remove_server() {
        use crate::config::{McpConfig, TransportType};
        let mut cfg = McpConfig {
            mcp_servers: Default::default(),
        };
        cfg.add_server(
            "s1".into(),
            McpServerConfig {
                transport: TransportType::Stdio,
                command: "cmd".into(),
                args: vec![],
                env: Default::default(),
                url: None,
                oauth: None,
                deny_in_readonly_tools: Vec::new(),
                tool_timeout_secs: None,
                per_tool_timeout_secs: Default::default(),
            },
        );
        assert!(cfg.get_server("s1").is_some());
        assert!(cfg.remove_server("s1").is_some());
        assert!(cfg.get_server("s1").is_none());
        assert!(cfg.remove_server("nonexistent").is_none());
    }
}
