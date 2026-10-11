//! MCP tool bridge: adapt MCP server tools onto the kernel `ToolDefinition`
//! seam so the new agent-loop tool runtime can discover and dispatch them.

use std::sync::Arc;

use cordis_rslite::Value;
use starriver_kernel::tools::{ToolDefinition, ToolError, ToolExecutor, ToolRunContext};
use tokio::sync::Mutex;

use super::adapter::{
    build_connection, fetch_tool_list_on, initialize_params, probe_capabilities,
    sanitize_tool_name, McpCapabilities, McpToolInfo, TokenProvider,
};
use super::config::McpServerConfig;
use super::transport::Connection;
use super::write_semantics::{classify_write_semantics, WriteSemantics};
use crate::common::{json_args, text_content};

/// Lazily launch + handshake a stdio connection so the next method request is
/// protocol-legal (real stdio servers reject pre-initialize calls with
/// -32002). HTTP connections need no launch or handshake here.
async fn ensure_stdio_launched(
    conn: &mut Connection,
    config: &McpServerConfig,
    name: &str,
) -> Result<(), ToolError> {
    let Connection::Stdio(child) = conn else {
        return Ok(());
    };
    if !child.is_launched() {
        child
            .launch(config)
            .await
            .map_err(|error| ToolError::Execution {
                tool: name.to_string(),
                code: "MCP_LAUNCH_FAILED".into(),
                message: format!("launch: {error}"),
            })?;
    }
    if !child.is_initialized() {
        child
            .send_request("initialize", initialize_params())
            .await
            .map_err(|error| ToolError::Execution {
                tool: name.to_string(),
                code: "MCP_INIT_FAILED".into(),
                message: format!("initialize: {error}"),
            })?;
    }
    Ok(())
}

/// Resolve the effective call timeout for one MCP tool: a per-tool override
/// beats the server-level default; neither configured → None (no timeout,
/// legacy behavior).
pub fn resolve_tool_timeout(
    config: &McpServerConfig,
    raw_tool_name: &str,
) -> Option<std::time::Duration> {
    let secs = config
        .per_tool_timeout_secs
        .get(raw_tool_name)
        .copied()
        .or(config.tool_timeout_secs)?;
    Some(std::time::Duration::from_secs(secs))
}

/// Discover MCP tools once and produce kernel `ToolDefinition`s so stdio
/// servers are launched a single time per discovery.
pub async fn discover_bridge(
    server_name: &str,
    config: &McpServerConfig,
    token_provider: TokenProvider,
) -> anyhow::Result<Vec<ToolDefinition>> {
    let (definitions, _connection) =
        discover_bridge_shared(server_name, config, token_provider).await?;
    Ok(definitions)
}

/// Like [`discover_bridge`], but also returns the server-shared connection
/// (already launched and handshoken) so a caller can supervise it with
/// [`crate::health::McpHealthMonitor`]. Every returned definition's
/// executor holds this same `Arc`, so an in-place swap inside the mutex is
/// visible to all of them.
pub async fn discover_bridge_shared(
    server_name: &str,
    config: &McpServerConfig,
    token_provider: TokenProvider,
) -> anyhow::Result<(Vec<ToolDefinition>, Arc<Mutex<Connection>>)> {
    // One shared connection per server: a stdio server launches a single
    // subprocess for all of its tools instead of one per tool. The handshake
    // and tools/list run on this connection so it stays hot (pinnable by a
    // health monitor) after discovery.
    let mut conn = build_connection(server_name, config, token_provider);
    if let Connection::Stdio(child) = &mut conn {
        child.launch(config).await?;
    }
    let tools = fetch_tool_list_on(&mut conn).await?;
    let capabilities = probe_capabilities(&mut conn).await;
    let connection = Arc::new(Mutex::new(conn));
    let mut definitions = Vec::new();
    // Two raw names can sanitize to the SAME tool name (`a.b` vs `a b`) —
    // a duplicate would silently overwrite the earlier registration.
    let mut used_names = std::collections::HashSet::new();
    for tool in tools {
        let mut definition = mcp_tool_definition_with_connection(
            server_name.to_string(),
            tool,
            config.clone(),
            Arc::clone(&connection),
        );
        if used_names.contains(&definition.name) {
            let mut n = 2;
            while used_names.contains(&format!("{}_{n}", definition.name)) {
                n += 1;
            }
            definition.name = format!("{}_{n}", definition.name);
        }
        used_names.insert(definition.name.clone());
        definitions.push(definition);
    }
    definitions.extend(resource_prompt_definitions(
        server_name,
        config,
        Arc::clone(&connection),
        capabilities,
    ));
    Ok((definitions, connection))
}

/// Re-establish a server connection after failure: build a fresh connection,
/// relaunch (stdio) + handshake + `tools/list` on it, then swap it in place
/// inside the shared `Arc<Mutex<Connection>>` so every existing executor
/// transparently moves to the new connection. Returns the re-discovered tool
/// definitions so the caller can re-register them (the tool list may have
/// changed across the restart).
pub async fn reconnect_bridge(
    server_name: &str,
    config: &McpServerConfig,
    token_provider: TokenProvider,
    shared: &Arc<Mutex<Connection>>,
) -> anyhow::Result<Vec<ToolDefinition>> {
    let mut fresh = build_connection(server_name, config, token_provider);
    if let Connection::Stdio(child) = &mut fresh {
        child.launch(config).await?;
    }
    let tools = fetch_tool_list_on(&mut fresh).await?;
    let capabilities = probe_capabilities(&mut fresh).await;
    let mut definitions = Vec::new();
    for tool in tools {
        definitions.push(mcp_tool_definition_with_connection(
            server_name.to_string(),
            tool,
            config.clone(),
            Arc::clone(shared),
        ));
    }
    definitions.extend(resource_prompt_definitions(
        server_name,
        config,
        Arc::clone(shared),
        capabilities,
    ));
    *shared.lock().await = fresh;
    Ok(definitions)
}

/// Build a kernel `ToolDefinition` for one discovered MCP tool.
pub fn mcp_tool_definition(
    server_name: String,
    tool: McpToolInfo,
    config: McpServerConfig,
    token_provider: TokenProvider,
) -> ToolDefinition {
    let connection = Arc::new(Mutex::new(build_connection(
        &server_name,
        &config,
        token_provider,
    )));
    mcp_tool_definition_with_connection(server_name, tool, config, connection)
}

/// Like [`mcp_tool_definition`], but reuses a server-shared connection so a
/// stdio server launches exactly one subprocess for all of its tools.
pub fn mcp_tool_definition_with_connection(
    server_name: String,
    tool: McpToolInfo,
    config: McpServerConfig,
    connection: Arc<Mutex<Connection>>,
) -> ToolDefinition {
    let raw_name = tool.name.clone();
    let name = sanitize_tool_name(&format!("mcp_{server_name}_{raw_name}"));
    // Write-semantics negotiation (WI-22): destructive (annotation or name
    // heuristic) → manual approval + refused in read-only sandboxes;
    // server-declared read-only → auto (no approval prompt); unknown → the
    // legacy always-ask behavior.
    let semantics = classify_write_semantics(&raw_name, &tool.annotations);
    let requires_approval = semantics != WriteSemantics::ReadOnly;
    let deny_in_readonly = config.deny_in_readonly_tools.iter().any(|t| t == &raw_name)
        || semantics == WriteSemantics::Destructive;
    let timeout = resolve_tool_timeout(&config, &raw_name);
    ToolDefinition {
        name: name.clone(),
        description: tool.description,
        parameters: Value::new(tool.input_schema),
        output: Value::new(serde_json::json!({
            "type": "object",
            "properties": { "output": { "type": "string" } }
        })),
        requires_approval,
        deny_in_readonly,
        render: Arc::new(text_content),
        executor: Arc::new(McpToolExecutor {
            name,
            tool_name: raw_name,
            connection,
            config,
            timeout,
        }),
        is_concurrency_safe: None,
        timeout,
        finalize_content: None,
    }
}

/// ToolDefinition executor routing `tools/call` over the MCP transport.
pub struct McpToolExecutor {
    /// Sanitized registry-facing name (used in diagnostics).
    name: String,
    /// Raw tool name as the MCP server exposes it.
    tool_name: String,
    connection: Arc<Mutex<Connection>>,
    config: McpServerConfig,
    /// Enforced in the executor itself (the kernel `tools/execute` timeout
    /// waterfall policy does not exist yet): an elapsed budget aborts the
    /// in-flight request and resets a desynchronized stdio subprocess.
    timeout: Option<std::time::Duration>,
}

impl McpToolExecutor {
    /// Lazy-launch a stdio connection and (re-)handshake if this child has
    /// not yet seen `initialize` — real stdio servers reject `tools/call`
    /// before the handshake with -32002.
    async fn ensure_launched(
        conn: &mut Connection,
        config: &McpServerConfig,
        name: &str,
    ) -> Result<(), ToolError> {
        ensure_stdio_launched(conn, config, name).await
    }
}

impl ToolExecutor for McpToolExecutor {
    fn execute(
        &self,
        arguments: Value,
        _ctx: ToolRunContext,
    ) -> cordis_rslite::BoxFuture<Result<Value, ToolError>> {
        let name = self.name.clone();
        let tool_name = self.tool_name.clone();
        let connection = Arc::clone(&self.connection);
        let config = self.config.clone();
        let timeout = self.timeout;
        Box::pin(async move {
            let arguments = json_args(&arguments)?;
            let call = async {
                let mut conn = connection.lock().await;
                Self::ensure_launched(&mut conn, &config, &name).await?;
                conn.send_request(
                    "tools/call",
                    serde_json::json!({
                        "name": tool_name,
                        "arguments": arguments,
                    }),
                )
                .await
                .map_err(|error| ToolError::Execution {
                    tool: name.clone(),
                    code: "MCP_CALL_FAILED".into(),
                    message: error.to_string(),
                })
            };
            let result = match timeout {
                Some(budget) => match tokio::time::timeout(budget, call).await {
                    Ok(result) => result?,
                    Err(_) => {
                        // Aborted mid-flight: a stdio stream is left with the
                        // response unread (desynchronized), so kill the child
                        // — the next call relaunches and re-handshakes. HTTP
                        // requests are independent; nothing to reset.
                        let mut conn = connection.lock().await;
                        conn.reset_stdio();
                        return Err(ToolError::Execution {
                            tool: name,
                            code: "MCP_TIMEOUT".into(),
                            message: format!("mcp call timed out after {}s", budget.as_secs_f32()),
                        });
                    }
                },
                None => call.await?,
            };
            if mcp_result_is_error(&result) {
                return Err(ToolError::Execution {
                    tool: name,
                    code: "MCP_IS_ERROR".into(),
                    message: render_mcp_result(&result),
                });
            }
            Ok(Value::new(serde_json::json!({
                "output": render_mcp_result(&result),
            })))
        })
    }
}

// ─── MCP resources / prompts bridge (WI-19) ─────────────────────────────

/// Build the bridge ToolDefinitions exposing a server's `resources` and
/// `prompts` surfaces (probed during discovery). Registered alongside the
/// server's tools so the agent gets a uniform tool face:
/// `mcp_<server>_list_resources` / `mcp_<server>_read_resource` /
/// `mcp_<server>_list_prompts` / `mcp_<server>_get_prompt`.
///
/// Exposure decision (mcp-deferrals left it open): the deferrals doc says
/// protocol extension surfaces bridged onto the tool seam — so these are registered
/// as tools on the kernel `InMemoryToolRuntime` like everything else, not as
/// a separate protocol surface. All four are read-only by construction:
/// `requires_approval: false`, `deny_in_readonly: false`.
pub fn resource_prompt_definitions(
    server_name: &str,
    config: &McpServerConfig,
    connection: Arc<Mutex<Connection>>,
    capabilities: McpCapabilities,
) -> Vec<ToolDefinition> {
    let mut definitions = Vec::new();
    if capabilities.resources {
        definitions.push(mcp_method_definition(
            server_name,
            "list_resources",
            "resources/list",
            "List the resources exposed by this MCP server (uri, name, description, mimeType).",
            serde_json::json!({"type": "object", "properties": {}}),
            config,
            Arc::clone(&connection),
            Arc::new(|_args| Ok(serde_json::json!({}))),
            render_pretty_json,
        ));
        definitions.push(mcp_method_definition(
            server_name,
            "read_resource",
            "resources/read",
            "Read one MCP resource by uri and return its text contents.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "uri": {"type": "string", "description": "Resource uri from list_resources"}
                },
                "required": ["uri"],
            }),
            config,
            Arc::clone(&connection),
            Arc::new(|args| {
                let uri = args.get("uri").and_then(|v| v.as_str()).ok_or_else(|| {
                    ToolError::Execution {
                        tool: "read_resource".into(),
                        code: "INVALID_INPUT".into(),
                        message: "missing required string argument `uri`".into(),
                    }
                })?;
                Ok(serde_json::json!({"uri": uri}))
            }),
            render_resource_contents,
        ));
    }
    if capabilities.prompts {
        definitions.push(mcp_method_definition(
            server_name,
            "list_prompts",
            "prompts/list",
            "List the prompt templates exposed by this MCP server (name, description, arguments).",
            serde_json::json!({"type": "object", "properties": {}}),
            config,
            Arc::clone(&connection),
            Arc::new(|_args| Ok(serde_json::json!({}))),
            render_pretty_json,
        ));
        definitions.push(mcp_method_definition(
            server_name,
            "get_prompt",
            "prompts/get",
            "Render one MCP prompt template by name with optional arguments; returns the message list as text.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Prompt name from list_prompts"},
                    "arguments": {
                        "type": "object",
                        "description": "Template argument values",
                        "additionalProperties": true,
                    }
                },
                "required": ["name"],
            }),
            config,
            connection,
            Arc::new(|args| {
                let name = args.get("name").and_then(|v| v.as_str()).ok_or_else(|| {
                    ToolError::Execution {
                        tool: "get_prompt".into(),
                        code: "INVALID_INPUT".into(),
                        message: "missing required string argument `name`".into(),
                    }
                })?;
                let arguments = args
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({}));
                Ok(serde_json::json!({"name": name, "arguments": arguments}))
            }),
            render_prompt_messages,
        ));
    }
    definitions
}

fn mcp_method_definition(
    server_name: &str,
    bridge_name: &str,
    method: &'static str,
    description: &str,
    parameters: serde_json::Value,
    config: &McpServerConfig,
    connection: Arc<Mutex<Connection>>,
    build_params: ParamsBuilder,
    render_result: fn(&serde_json::Value) -> String,
) -> ToolDefinition {
    let name = sanitize_tool_name(&format!("mcp_{server_name}_{bridge_name}"));
    let timeout = resolve_tool_timeout(config, bridge_name);
    ToolDefinition {
        name: name.clone(),
        description: description.to_string(),
        parameters: Value::new(parameters),
        output: Value::new(serde_json::json!({
            "type": "object",
            "properties": { "output": { "type": "string" } }
        })),
        // Read-only protocol surfaces: no side effects by construction.
        requires_approval: false,
        deny_in_readonly: false,
        render: Arc::new(text_content),
        executor: Arc::new(McpMethodExecutor {
            name,
            method,
            connection,
            config: config.clone(),
            timeout,
            build_params,
            render_result,
        }),
        is_concurrency_safe: None,
        timeout,
        finalize_content: None,
    }
}

type ParamsBuilder =
    Arc<dyn Fn(&serde_json::Value) -> Result<serde_json::Value, ToolError> + Send + Sync>;

/// Generic executor for fixed-method MCP protocol surfaces
/// (resources/list, resources/read, prompts/list, prompts/get).
struct McpMethodExecutor {
    name: String,
    method: &'static str,
    connection: Arc<Mutex<Connection>>,
    config: McpServerConfig,
    timeout: Option<std::time::Duration>,
    build_params: ParamsBuilder,
    render_result: fn(&serde_json::Value) -> String,
}

impl ToolExecutor for McpMethodExecutor {
    fn execute(
        &self,
        arguments: Value,
        _ctx: ToolRunContext,
    ) -> cordis_rslite::BoxFuture<Result<Value, ToolError>> {
        let name = self.name.clone();
        let method = self.method;
        let connection = Arc::clone(&self.connection);
        let config = self.config.clone();
        let timeout = self.timeout;
        let build_params = Arc::clone(&self.build_params);
        let render_result = self.render_result;
        Box::pin(async move {
            let arguments = json_args(&arguments)?;
            let params = build_params(&arguments)?;
            let call = async {
                let mut conn = connection.lock().await;
                ensure_stdio_launched(&mut conn, &config, &name).await?;
                conn.send_request(method, params)
                    .await
                    .map_err(|error| ToolError::Execution {
                        tool: name.clone(),
                        code: "MCP_CALL_FAILED".into(),
                        message: error.to_string(),
                    })
            };
            let result = match timeout {
                Some(budget) => match tokio::time::timeout(budget, call).await {
                    Ok(result) => result?,
                    Err(_) => {
                        let mut conn = connection.lock().await;
                        conn.reset_stdio();
                        return Err(ToolError::Execution {
                            tool: name,
                            code: "MCP_TIMEOUT".into(),
                            message: format!("mcp call timed out after {}s", budget.as_secs_f32()),
                        });
                    }
                },
                None => call.await?,
            };
            Ok(Value::new(serde_json::json!({
                "output": render_result(&result),
            })))
        })
    }
}

/// Render a `resources/read` result: text contents joined in order
/// (prefixed by uri when several), pretty JSON fallback (e.g. base64 blobs).
fn render_resource_contents(result: &serde_json::Value) -> String {
    let contents = result["contents"].as_array();
    let texts: Vec<&str> = contents
        .into_iter()
        .flatten()
        .filter_map(|content| content.get("text").and_then(|t| t.as_str()))
        .collect();
    if !texts.is_empty() {
        return texts.join("\n");
    }
    render_pretty_json(result)
}

/// Render a `prompts/get` result: `role: text` per message, pretty JSON
/// fallback when messages carry non-text content.
fn render_prompt_messages(result: &serde_json::Value) -> String {
    let Some(messages) = result["messages"].as_array() else {
        return render_pretty_json(result);
    };
    let mut lines = Vec::new();
    for message in messages {
        let role = message["role"].as_str().unwrap_or("unknown");
        let text = message["content"]["text"].as_str();
        match text {
            Some(text) => lines.push(format!("{role}: {text}")),
            None => {
                // Non-text content (image/audio): fall back to JSON for it.
                let content = serde_json::to_string(&message["content"]).unwrap_or_default();
                lines.push(format!("{role}: {content}"));
            }
        }
    }
    if lines.is_empty() {
        return render_pretty_json(result);
    }
    lines.join("\n")
}

fn render_pretty_json(result: &serde_json::Value) -> String {
    serde_json::to_string_pretty(result).unwrap_or_default()
}

/// Render an MCP `tools/call` result as model-facing text: text content
/// blocks joined in order, then structuredContent as pretty JSON, then the
/// whole result as a fallback.
pub fn render_mcp_result(result: &serde_json::Value) -> String {
    let texts: Vec<String> = result["content"]
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|block| block.get("text").and_then(|text| text.as_str()))
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    if !texts.is_empty() {
        return texts.join("\n");
    }
    if let Some(structured) = result.get("structuredContent") {
        return serde_json::to_string_pretty(structured).unwrap_or_default();
    }
    serde_json::to_string_pretty(result).unwrap_or_default()
}

/// True when the MCP result carries `isError: true`.
pub(crate) fn mcp_result_is_error(result: &serde_json::Value) -> bool {
    result
        .get("isError")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use cordis_rslite::Value;
    use starriver_kernel::llm::ContentBlock;

    use super::*;
    use crate::adapter::{McpToolInfo, TokenProvider};
    use crate::config::{McpServerConfig, TransportType};
    use crate::write_semantics::McpToolAnnotations;

    fn no_token() -> TokenProvider {
        Arc::new(|| None)
    }

    fn stdio_config() -> McpServerConfig {
        McpServerConfig {
            transport: TransportType::Stdio,
            command: "echo".into(),
            args: vec![],
            env: Default::default(),
            url: None,
            oauth: None,
            deny_in_readonly_tools: Vec::new(),
            tool_timeout_secs: None,
            per_tool_timeout_secs: Default::default(),
        }
    }

    #[test]
    fn resolve_tool_timeout_prefers_per_tool_override() {
        let mut config = stdio_config();
        config.tool_timeout_secs = Some(30);
        config
            .per_tool_timeout_secs
            .insert("slow_search".into(), 120);
        assert_eq!(
            resolve_tool_timeout(&config, "slow_search"),
            Some(std::time::Duration::from_secs(120))
        );
        // No per-tool entry → server-level default.
        assert_eq!(
            resolve_tool_timeout(&config, "quick_read"),
            Some(std::time::Duration::from_secs(30))
        );
        // Neither configured → legacy no-timeout (a tool with no override at all).
        config.tool_timeout_secs = None;
        config.per_tool_timeout_secs.clear();
        assert_eq!(resolve_tool_timeout(&config, "slow_search"), None);
        // A per-tool override still wins with no server-level default.
        config
            .per_tool_timeout_secs
            .insert("slow_search".into(), 45);
        assert_eq!(
            resolve_tool_timeout(&config, "slow_search"),
            Some(std::time::Duration::from_secs(45))
        );
    }

    #[test]
    fn definition_carries_per_tool_timeout_override() {
        let mut config = stdio_config();
        config.tool_timeout_secs = Some(30);
        config
            .per_tool_timeout_secs
            .insert("slow_search".into(), 120);
        let slow = mcp_tool_definition(
            "gh".into(),
            McpToolInfo {
                name: "slow_search".into(),
                description: "slow".into(),
                input_schema: serde_json::json!({}),
                annotations: Default::default(),
            },
            config.clone(),
            no_token(),
        );
        let fast = mcp_tool_definition(
            "gh".into(),
            McpToolInfo {
                name: "quick_read".into(),
                description: "fast".into(),
                input_schema: serde_json::json!({}),
                annotations: Default::default(),
            },
            config,
            no_token(),
        );
        assert_eq!(slow.timeout, Some(std::time::Duration::from_secs(120)));
        assert_eq!(fast.timeout, Some(std::time::Duration::from_secs(30)));
    }

    #[test]
    fn mcp_tool_definition_builds_kernel_seam_fields() {
        let def = mcp_tool_definition(
            "gh".into(),
            McpToolInfo {
                name: "search repos".into(),
                description: "Search repositories".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {"q": {"type": "string"}},
                }),
                annotations: Default::default(),
            },
            stdio_config(),
            no_token(),
        );
        assert_eq!(def.name, "mcp_gh_search_repos");
        assert_eq!(def.description, "Search repositories");
        assert!(def.requires_approval);
        assert!(!def.deny_in_readonly);
        assert!(def.is_concurrency_safe.is_none());
        assert!(def.timeout.is_none());
        let parameters = def.parameters.downcast::<serde_json::Value>().unwrap();
        assert_eq!(parameters["properties"]["q"]["type"], "string");
        let blocks = (def.render)(
            &def.parameters,
            &Value::new(serde_json::json!({"output": "hi"})),
        );
        assert!(matches!(
            blocks.as_slice(),
            [ContentBlock::Text(text)] if text == "hi"
        ));
    }

    #[test]
    fn definition_carries_deny_in_readonly_and_timeout_from_config() {
        let mut config = stdio_config();
        config.deny_in_readonly_tools = vec!["git_push".into()];
        config.tool_timeout_secs = Some(90);
        let push_def = mcp_tool_definition(
            "gh".into(),
            McpToolInfo {
                name: "git_push".into(),
                description: "push".into(),
                input_schema: serde_json::json!({}),
                annotations: Default::default(),
            },
            config.clone(),
            no_token(),
        );
        assert!(push_def.deny_in_readonly);
        assert_eq!(push_def.timeout, Some(std::time::Duration::from_secs(90)));

        let read_def = mcp_tool_definition(
            "gh".into(),
            McpToolInfo {
                name: "git_log".into(),
                description: "log".into(),
                input_schema: serde_json::json!({}),
                annotations: Default::default(),
            },
            config,
            no_token(),
        );
        assert!(!read_def.deny_in_readonly);
        assert_eq!(read_def.timeout, Some(std::time::Duration::from_secs(90)));
    }

    #[test]
    fn shared_definition_uses_injected_connection() {
        let connection = Arc::new(Mutex::new(build_connection(
            "gh",
            &stdio_config(),
            no_token(),
        )));
        let shared = mcp_tool_definition_with_connection(
            "gh".into(),
            McpToolInfo {
                name: "git_log".into(),
                description: "log".into(),
                input_schema: serde_json::json!({}),
                annotations: Default::default(),
            },
            stdio_config(),
            Arc::clone(&connection),
        );
        assert_eq!(shared.name, "mcp_gh_git_log");
        assert!(!shared.deny_in_readonly);
        assert!(shared.timeout.is_none());
        // Connection identity is behavior-verified by the shared-connection
        // integration test (both tools of one server execute in order).
        drop(connection);
    }

    #[test]
    fn render_mcp_result_joins_text_content_blocks() {
        let result = serde_json::json!({
            "content": [
                {"type": "text", "text": "one"},
                {"type": "text", "text": "two"},
                {"type": "image", "data": "..."}
            ]
        });
        assert_eq!(render_mcp_result(&result), "one\ntwo");
    }

    #[test]
    fn render_mcp_result_falls_back_to_structured_content() {
        let result = serde_json::json!({
            "content": [],
            "structuredContent": {"ok": true, "n": 3}
        });
        assert_eq!(
            render_mcp_result(&result),
            serde_json::to_string_pretty(&serde_json::json!({"ok": true, "n": 3})).unwrap()
        );
    }

    #[test]
    fn render_mcp_result_returns_pretty_json_when_no_content() {
        let result = serde_json::json!({"content": [], "extra": "x"});
        assert_eq!(
            render_mcp_result(&result),
            serde_json::to_string_pretty(&result).unwrap()
        );
    }

    #[test]
    fn mcp_result_is_error_flags_is_error_results() {
        assert!(mcp_result_is_error(&serde_json::json!({"isError": true})));
        assert!(!mcp_result_is_error(&serde_json::json!({"content": []})));
        assert!(!mcp_result_is_error(&serde_json::json!({"isError": false})));
    }

    #[test]
    fn resource_prompt_definitions_gate_on_capabilities_and_stay_readonly() {
        let connection = Arc::new(Mutex::new(build_connection(
            "mock",
            &stdio_config(),
            no_token(),
        )));
        let mut config = stdio_config();
        config.tool_timeout_secs = Some(20);
        let both = resource_prompt_definitions(
            "mock",
            &config,
            Arc::clone(&connection),
            crate::adapter::McpCapabilities {
                resources: true,
                prompts: true,
            },
        );
        let names: Vec<&str> = both.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "mcp_mock_list_resources",
                "mcp_mock_read_resource",
                "mcp_mock_list_prompts",
                "mcp_mock_get_prompt",
            ]
        );
        for definition in &both {
            assert!(!definition.requires_approval);
            assert!(!definition.deny_in_readonly);
            assert_eq!(definition.timeout, Some(std::time::Duration::from_secs(20)));
        }

        let none = resource_prompt_definitions(
            "mock",
            &config,
            connection,
            crate::adapter::McpCapabilities::default(),
        );
        assert!(none.is_empty());
    }

    #[test]
    fn render_resource_contents_joins_texts_and_falls_back() {
        let result = serde_json::json!({
            "contents": [
                {"uri": "a://x", "text": "one"},
                {"uri": "b://y", "text": "two"},
                {"uri": "c://z", "blob": "AAAA", "mimeType": "image/png"}
            ]
        });
        assert_eq!(render_resource_contents(&result), "one\ntwo");
        let blob_only = serde_json::json!({"contents": [{"blob": "AAAA"}]});
        assert_eq!(
            render_resource_contents(&blob_only),
            serde_json::to_string_pretty(&blob_only).unwrap()
        );
    }

    #[test]
    fn render_prompt_messages_formats_roles_and_falls_back() {
        let result = serde_json::json!({"messages": [
            {"role": "user", "content": {"type": "text", "text": "hello"}},
            {"role": "assistant", "content": {"type": "image", "data": "..."}}
        ]});
        let rendered = render_prompt_messages(&result);
        assert!(rendered.starts_with("user: hello\nassistant: "));
        assert!(rendered.contains("image"));
        assert_eq!(
            render_prompt_messages(&serde_json::json!({})),
            serde_json::to_string_pretty(&serde_json::json!({})).unwrap()
        );
    }

    #[test]
    fn read_only_annotation_negotiates_auto_approval() {
        let def = mcp_tool_definition(
            "gh".into(),
            McpToolInfo {
                name: "search_repos".into(),
                description: "search".into(),
                input_schema: serde_json::json!({}),
                annotations: McpToolAnnotations {
                    read_only_hint: Some(true),
                    ..Default::default()
                },
            },
            stdio_config(),
            no_token(),
        );
        assert!(!def.requires_approval, "read-only tools auto-approve");
        assert!(!def.deny_in_readonly);
    }

    #[test]
    fn destructive_annotation_negotiates_manual_and_readonly_deny() {
        let def = mcp_tool_definition(
            "gh".into(),
            McpToolInfo {
                name: "drop_database".into(),
                description: "drop".into(),
                input_schema: serde_json::json!({}),
                annotations: McpToolAnnotations {
                    destructive_hint: Some(true),
                    ..Default::default()
                },
            },
            stdio_config(),
            no_token(),
        );
        assert!(def.requires_approval, "destructive tools stay manual");
        assert!(
            def.deny_in_readonly,
            "destructive tools are refused pre-approval in read-only sandboxes"
        );
    }

    #[test]
    fn destructive_name_heuristic_without_annotations_stays_manual() {
        // No annotations at all, but the name token is destructive: the
        // heuristic alone raises risk (manual + read-only refuse).
        let def = mcp_tool_definition(
            "gh".into(),
            McpToolInfo {
                name: "delete_branch".into(),
                description: "delete".into(),
                input_schema: serde_json::json!({}),
                annotations: Default::default(),
            },
            stdio_config(),
            no_token(),
        );
        assert!(def.requires_approval);
        assert!(def.deny_in_readonly);
    }

    #[test]
    fn unknown_semantics_keep_legacy_approval_behavior() {
        // No signals: manual approval, not pre-refused in read-only — the
        // pre-negotiation default.
        let def = mcp_tool_definition(
            "gh".into(),
            McpToolInfo {
                name: "fetch_weather".into(),
                description: "weather".into(),
                input_schema: serde_json::json!({}),
                annotations: Default::default(),
            },
            stdio_config(),
            no_token(),
        );
        assert!(def.requires_approval);
        assert!(!def.deny_in_readonly);
    }
}
