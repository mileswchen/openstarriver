//! OAuth 2.1 confidential-client (authorization-code) support for remote MCP endpoints.
//!
//! Flow: `connect` binds a local callback listener, opens the provider's authorize URL
//! in the system browser, captures the `code`, exchanges it for a token, and stores the
//! token in the OS keychain under `mcp_oauth/<name>`. `token` is a synchronous keychain
//! read used by `HttpConnection` to inject `Authorization: Bearer`.
//!
//! Refresh-on-expiry is intentionally deferred: `token` returns the stored access token,
//! and an expired token surfaces as an HTTP 401 (the user re-connects via `connect`).

use super::config::OAuthConfig;
use serde::{Deserialize, Serialize};

const KEYRING_SERVICE: &str = "starriver";
const KEYRING_ACCOUNT_PREFIX: &str = "mcp_oauth/";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredToken {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
}

fn keyring_account(name: &str) -> String {
    format!("{KEYRING_ACCOUNT_PREFIX}{name}")
}

/// Synchronous read of a stored access token. Returns None if none stored or unreadable.
pub fn token(name: &str) -> Option<String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, &keyring_account(name)).ok()?;
    let raw = entry.get_password().ok()?;
    serde_json::from_str::<StoredToken>(&raw)
        .ok()
        .map(|t| t.access_token)
}

/// Delete the stored token for a server. Non-fatal.
pub fn disconnect(name: &str) {
    if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, &keyring_account(name)) {
        let _ = entry.delete_credential();
    }
}

fn port_from_redirect(redirect_uri: &str) -> u16 {
    // e.g. "http://localhost:3118/callback" -> 3118. An https redirect
    // without an explicit port defaults to the SCHEME port (443), not the
    // loopback listener port — wiring 3118 there connects the callback to
    // the wrong port.
    let https = redirect_uri.starts_with("https:");
    redirect_uri
        .split(':')
        .nth(2)
        .and_then(|s| s.split('/').next())
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(if https { 443 } else { 3118 })
}

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn();
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    let _ = url;
}

fn build_authorize_url(config: &OAuthConfig, state: &str) -> String {
    let mut url = config.authorization_url.clone();
    let sep = if url.contains('?') { '&' } else { '?' };
    let scope = config.scopes.join(" ");
    url.push(sep);
    // BUG-56: every query parameter is percent-encoded — a client_id or
    // state containing `&`/`=`/space would otherwise corrupt the query.
    url.push_str(&format!(
        "response_type=code&client_id={}&redirect_uri={}&state={}&scope={}",
        urlencoding(&config.client_id),
        urlencoding(&config.redirect_uri),
        urlencoding(state),
        urlencoding(&scope),
    ));
    url
}

/// Minimal URL-encoding (query-safe) for OAuth params; avoids pulling in a
/// full urlencoding dependency. RFC 3986 percent-encoding operates on UTF-8
/// BYTES — encoding the codepoint itself produced Latin-1 for é and illegal
/// escapes for CJK (BUG-56).
fn urlencoding(s: &str) -> String {
    let mut out = String::new();
    for &byte in s.as_bytes() {
        match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

async fn exchange_and_store(name: &str, config: &OAuthConfig, code: &str) -> anyhow::Result<()> {
    let client = reqwest::Client::new();
    let resp = client
        .post(&config.token_url)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("client_id", config.client_id.as_str()),
            ("client_secret", config.client_secret.as_str()),
            ("redirect_uri", config.redirect_uri.as_str()),
        ])
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("token exchange failed: {e}"))?;

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| anyhow::anyhow!("token response parse failed: {e}"))?;

    let access_token = body
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("no access_token in response: {body}"))?
        .to_string();
    let refresh_token = body
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .map(String::from);

    let stored = StoredToken {
        access_token,
        refresh_token,
    };
    let raw = serde_json::to_string(&stored)?;
    let entry = keyring::Entry::new(KEYRING_SERVICE, &keyring_account(name))?;
    entry.set_password(&raw)?;
    Ok(())
}

/// Initiate the OAuth flow. Binds the local callback, opens the browser, and returns the
/// authorize URL (so the frontend can also surface it). The token is exchanged + stored
/// asynchronously after the user approves in the browser.
pub async fn connect(name: String, config: OAuthConfig) -> anyhow::Result<String> {
    let state = uuid::Uuid::new_v4().to_string();
    let port = port_from_redirect(&config.redirect_uri);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|e| anyhow::anyhow!("failed to bind callback on :{port}: {e}"))?;

    let authorize_url = build_authorize_url(&config, &state);

    // Await the callback and exchange the code in the background.
    tokio::spawn(async move {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        let code = read_code_from_stream(&mut stream).await;
        if let Some(code) = code {
            let _ = exchange_and_store(&name, &config, &code).await;
        }
        let body = "OK — you can close this tab.";
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        use tokio::io::AsyncWriteExt;
        let _ = stream.write_all(resp.as_bytes()).await;
        let _ = stream.shutdown().await;
    });

    open_browser(&authorize_url);
    Ok(authorize_url)
}

async fn read_code_from_stream(stream: &mut tokio::net::TcpStream) -> Option<String> {
    use tokio::io::AsyncBufReadExt;
    let mut reader = tokio::io::BufReader::new(stream);
    let mut line = String::new();
    let _ = reader.read_line(&mut line).await;
    // line: "GET /callback?code=abc&state=xyz HTTP/1.1"
    let path = line.split_whitespace().nth(1)?;
    let query = path.split('?').nth(1)?;
    for pair in query.split('&') {
        if let Some(v) = pair.strip_prefix("code=") {
            return Some(v.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_port_from_redirect() {
        assert_eq!(port_from_redirect("http://localhost:3118/callback"), 3118);
        assert_eq!(port_from_redirect("http://127.0.0.1:9000/cb"), 9000);
        assert_eq!(port_from_redirect("no-port"), 3118);
    }

    #[test]
    fn test_oauth_config_roundtrips_through_yaml() {
        let cfg = OAuthConfig {
            authorization_url: "https://slack.com/oauth/v2_user/authorize".into(),
            token_url: "https://slack.com/api/oauth.v2.user.access".into(),
            client_id: "cid".into(),
            client_secret: "csec".into(),
            scopes: vec!["chat:write".into(), "search:read.public".into()],
            redirect_uri: "http://localhost:3118/callback".into(),
        };
        let yaml = serde_yaml::to_string(&cfg).unwrap();
        let back: OAuthConfig = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(back.client_id, "cid");
        assert_eq!(back.scopes.len(), 2);
        assert_eq!(back.redirect_uri, "http://localhost:3118/callback");
    }

    #[test]
    fn test_urlencoding() {
        assert_eq!(
            urlencoding("http://localhost:3118/callback"),
            "http%3A%2F%2Flocalhost%3A3118%2Fcallback"
        );
        assert_eq!(
            urlencoding("chat:write search:read"),
            "chat%3Awrite%20search%3Aread"
        );
    }

    #[test]
    fn test_build_authorize_url_includes_code_and_encoded_params() {
        let cfg = OAuthConfig {
            authorization_url: "https://slack.com/oauth/v2_user/authorize".into(),
            token_url: "https://slack.com/api/oauth.v2.user.access".into(),
            client_id: "cid".into(),
            client_secret: "csec".into(),
            scopes: vec!["chat:write".into(), "search:read.public".into()],
            redirect_uri: "http://localhost:3118/callback".into(),
        };
        let url = build_authorize_url(&cfg, "state123");
        assert!(url.starts_with("https://slack.com/oauth/v2_user/authorize?"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("client_id=cid"));
        assert!(url.contains("state=state123"));
        // redirect_uri and scope are URL-encoded.
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A3118%2Fcallback"));
        assert!(url.contains("scope=chat%3Awrite%20search%3Aread.public"));
    }
}

// ── Round-5 regressions (BUG-56): OAuth URL construction ──
#[cfg(test)]
mod round5_tests {
    use super::*;

    /// RFC 3986 percent-encoding operates on UTF-8 BYTES; encoding the
    /// Unicode codepoint produces Latin-1 for é and outright illegal escapes
    /// for CJK (a parser reads %4E then bare "2D").
    #[test]
    fn urlencoding_percent_encodes_utf8_bytes() {
        assert_eq!(urlencoding("\u{e9}"), "%C3%A9", "é is two UTF-8 bytes");
        assert_eq!(urlencoding("\u{4e2d}"), "%E4%B8%AD", "中 is three UTF-8 bytes");
        // Unreserved characters stay literal.
        assert_eq!(urlencoding("aZ09-_.~"), "aZ09-_.~");
    }

    /// The authorize URL is a query string: client_id and state must be
    /// percent-encoded like every other parameter, or a client_id containing
    /// `&`/`=`/space silently corrupts the request.
    #[test]
    fn authorize_url_encodes_client_id_and_state() {
        let config = OAuthConfig {
            client_id: "a&b=c d".into(),
            client_secret: String::new(),
            authorization_url: "https://auth.example.com/authorize".into(),
            token_url: "https://auth.example.com/token".into(),
            redirect_uri: "http://localhost:3118/callback".into(),
            scopes: vec!["read".into()],
        };
        let url = build_authorize_url(&config, "st ate");
        assert!(
            url.contains("client_id=a%26b%3Dc%20d"),
            "client_id must be encoded (got {url})"
        );
        assert!(
            url.contains("state=st%20ate"),
            "state must be encoded (got {url})"
        );
    }

    /// An https redirect without an explicit port defaults to 443 — wiring
    /// the loopback listener port (3118) as the https default connects the
    /// callback listener to the wrong port.
    #[test]
    fn https_redirect_without_port_defaults_to_443() {
        assert_eq!(
            port_from_redirect("https://auth.example.com/callback"),
            443,
            "scheme default for https is 443"
        );
        assert_eq!(port_from_redirect("http://localhost:3118/callback"), 3118);
        assert_eq!(port_from_redirect("https://host:8443/cb"), 8443);
    }
}
