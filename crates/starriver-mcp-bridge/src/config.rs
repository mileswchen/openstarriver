use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub transport: TransportType,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
    /// Remote endpoint URL (Streamable HTTP). None for stdio.
    #[serde(default)]
    pub url: Option<String>,
    /// OAuth confidential-client config for remote endpoints. None when unauthenticated
    /// or when auth is handled via `env` (e.g. an API key header).
    #[serde(default)]
    pub oauth: Option<OAuthConfig>,
    /// MCP tool names (raw, as the server exposes them) that write files and
    /// must be refused in a read-only sandbox before approval.
    #[serde(default)]
    pub deny_in_readonly_tools: Vec<String>,
    /// Server-level tool-call timeout; None keeps the legacy no-timeout
    /// behavior.
    #[serde(default)]
    pub tool_timeout_secs: Option<u64>,
    /// Per-tool timeout overrides in seconds, keyed by the raw MCP tool
    /// name. Beats `tool_timeout_secs` for the named tool; tools without an
    /// entry fall back to the server-level value.
    #[serde(default)]
    pub per_tool_timeout_secs: std::collections::HashMap<String, u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransportType {
    Stdio,
    Sse,
    Http,
}

/// OAuth 2.1 confidential-client (authorization-code) settings for a remote MCP endpoint.
/// `client_id`/`client_secret` are filled by the user (they register their own app);
/// the token is stored in the OS keychain under `mcp_oauth/<name>` after the flow completes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthConfig {
    pub authorization_url: String,
    pub token_url: String,
    pub client_id: String,
    pub client_secret: String,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub redirect_uri: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpConfig {
    pub mcp_servers: std::collections::HashMap<String, McpServerConfig>,
}

/// Preserve the previously-stored OAuth `client_secret` when an incoming oauth config has
/// an empty secret (the renderer masks it on edit to avoid round-tripping the plaintext).
/// A `None` incoming config (OAuth explicitly disabled) is passed through unchanged.
pub fn merge_oauth_secret(
    incoming: Option<OAuthConfig>,
    existing: Option<&McpServerConfig>,
) -> Option<OAuthConfig> {
    let mut incoming = incoming?;
    if incoming.client_secret.is_empty() {
        if let Some(old) = existing
            .and_then(|s| s.oauth.as_ref())
            .map(|o| o.client_secret.clone())
        {
            incoming.client_secret = old;
        }
    }
    Some(incoming)
}

impl McpConfig {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        Ok(serde_yaml::from_str(&content)?)
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let yaml = serde_yaml::to_string(self)?;
        // Atomic tmp+rename: a crash mid-write left a truncated mcp.yaml
        // (which then fails the whole config load on next boot).
        let tmp = path.with_extension("yaml.tmp");
        std::fs::write(&tmp, yaml)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn add_server(&mut self, name: String, config: McpServerConfig) {
        self.mcp_servers.insert(name, config);
    }

    pub fn remove_server(&mut self, name: &str) -> Option<McpServerConfig> {
        self.mcp_servers.remove(name)
    }

    pub fn get_server(&self, name: &str) -> Option<&McpServerConfig> {
        self.mcp_servers.get(name)
    }

    pub fn default_path() -> Option<std::path::PathBuf> {
        // USERPROFILE fallback for Windows (HOME is unset in normal Windows
        // processes — without it the config silently lands nowhere).
        std::env::var("HOME")
            .ok()
            .or_else(|| std::env::var("USERPROFILE").ok())
            .map(|h| {
                std::path::PathBuf::from(h)
                    .join(".starriver")
                    .join("mcp.yaml")
            })
    }

    /// Servers that should be discovered/registered at startup: stdio and http.
    /// Sse is not yet wired to a connection, so it remains excluded.
    pub fn enabled_servers(&self) -> Vec<(String, &McpServerConfig)> {
        self.mcp_servers
            .iter()
            .filter(|(_, c)| matches!(c.transport, TransportType::Stdio | TransportType::Http))
            .map(|(name, cfg)| (name.clone(), cfg))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_config_fields_roundtrip_through_yaml() {
        let yaml = r#"
mcp_servers:
  gh:
    transport: stdio
    command: npx
    args: []
    env: {}
    deny_in_readonly_tools: ["git_commit", "git_push"]
    tool_timeout_secs: 90
    per_tool_timeout_secs:
      slow_search: 120
      quick_read: 5
"#;
        let config: McpConfig = serde_yaml::from_str(yaml).unwrap();
        let srv = config.get_server("gh").unwrap();
        assert_eq!(
            srv.deny_in_readonly_tools,
            vec!["git_commit".to_string(), "git_push".to_string()]
        );
        assert_eq!(srv.tool_timeout_secs, Some(90));
        assert_eq!(
            srv.per_tool_timeout_secs.get("slow_search").copied(),
            Some(120)
        );
        assert_eq!(
            srv.per_tool_timeout_secs.get("quick_read").copied(),
            Some(5)
        );
    }

    #[test]
    fn per_tool_timeout_defaults_to_empty_when_absent() {
        let yaml = r#"
mcp_servers:
  gh:
    transport: stdio
    command: npx
    args: []
    env: {}
    tool_timeout_secs: 30
"#;
        let config: McpConfig = serde_yaml::from_str(yaml).unwrap();
        let srv = config.get_server("gh").unwrap();
        assert!(srv.per_tool_timeout_secs.is_empty());
        assert_eq!(srv.tool_timeout_secs, Some(30));
    }

    #[test]
    fn new_config_fields_default_when_absent() {
        let yaml =
            "mcp_servers:\n  gh:\n    transport: stdio\n    command: npx\n    args: []\n    env: {}\n";
        let config: McpConfig = serde_yaml::from_str(yaml).unwrap();
        let srv = config.get_server("gh").unwrap();
        assert!(srv.deny_in_readonly_tools.is_empty());
        assert_eq!(srv.tool_timeout_secs, None);
    }

    #[test]
    fn http_transport_roundtrips_through_yaml() {
        let yaml = r#"
mcp_servers:
  slack:
    transport: http
    command: ""
    args: []
    env: {}
    url: "https://mcp.slack.com/mcp"
    oauth:
      authorization_url: "https://slack.com/oauth/v2_user/authorize"
      token_url: "https://slack.com/api/oauth.v2.user.access"
      client_id: "cid"
      client_secret: "csec"
      scopes: ["chat:write"]
      redirect_uri: "http://localhost:3118/callback"
"#;
        let cfg: McpConfig = serde_yaml::from_str(yaml).unwrap();
        let srv = cfg.get_server("slack").unwrap();
        assert!(matches!(srv.transport, TransportType::Http));
        assert_eq!(srv.url.as_deref(), Some("https://mcp.slack.com/mcp"));
        let oauth = srv.oauth.as_ref().unwrap();
        assert_eq!(
            oauth.token_url,
            "https://slack.com/api/oauth.v2.user.access"
        );
        assert_eq!(oauth.scopes, vec!["chat:write".to_string()]);
    }

    #[test]
    fn legacy_config_defaults_url_and_oauth_to_none() {
        let yaml = r#"
mcp_servers:
  gh:
    transport: stdio
    command: npx
    args: ["-y", "gh"]
    env: {}
"#;
        let cfg: McpConfig = serde_yaml::from_str(yaml).unwrap();
        let srv = cfg.get_server("gh").unwrap();
        assert!(srv.url.is_none());
        assert!(srv.oauth.is_none());
    }

    #[test]
    fn transport_type_serializes_lowercase() {
        assert_eq!(
            serde_yaml::to_string(&TransportType::Http).unwrap().trim(),
            "http"
        );
        assert_eq!(
            serde_yaml::to_string(&TransportType::Stdio).unwrap().trim(),
            "stdio"
        );
        assert_eq!(
            serde_yaml::to_string(&TransportType::Sse).unwrap().trim(),
            "sse"
        );
    }

    fn oauth_config(secret: &str) -> OAuthConfig {
        OAuthConfig {
            authorization_url: "https://a.example.com/auth".into(),
            token_url: "https://a.example.com/token".into(),
            client_id: "cid".into(),
            client_secret: secret.into(),
            scopes: vec!["read".into()],
            redirect_uri: "http://localhost:3118/callback".into(),
        }
    }

    fn stdio_server() -> McpServerConfig {
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
    fn merge_oauth_secret_preserves_existing_secret_when_incoming_empty() {
        let existing = McpServerConfig {
            oauth: Some(oauth_config("old-secret")),
            ..stdio_server()
        };
        let merged = merge_oauth_secret(Some(oauth_config("")), Some(&existing));
        assert_eq!(merged.unwrap().client_secret, "old-secret");
    }

    #[test]
    fn merge_oauth_secret_uses_incoming_secret_when_present() {
        let existing = McpServerConfig {
            oauth: Some(oauth_config("old-secret")),
            ..stdio_server()
        };
        let merged = merge_oauth_secret(Some(oauth_config("new-secret")), Some(&existing));
        assert_eq!(merged.unwrap().client_secret, "new-secret");
    }

    #[test]
    fn merge_oauth_secret_none_stays_none() {
        let existing = McpServerConfig {
            oauth: Some(oauth_config("old-secret")),
            ..stdio_server()
        };
        assert!(merge_oauth_secret(None, Some(&existing)).is_none());
    }

    #[test]
    fn merge_oauth_secret_no_existing_keeps_empty() {
        let merged = merge_oauth_secret(Some(oauth_config("")), Some(&stdio_server()));
        assert_eq!(merged.unwrap().client_secret, "");
    }
}
