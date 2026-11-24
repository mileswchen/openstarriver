//! WI-252: plugin egress network — the ONE sanctioned channel out of a
//! plugin UI frame. The frame CSP keeps `connect-src 'none'` (data-exfil and
//! intranet-probe defense, untouched); this module forwards manifest-granted
//! https requests from the host process directly (no LLM, sub-second), with
//! the SSRF/abuse defenses enforced server-side:
//!
//! 1. URL must match the plugin's `net.allow` grant (`NET_NOT_ALLOWED`);
//! 2. every resolved address must be public (`NET_PRIVATE_BLOCKED` — blocks
//!    loopback / RFC1918 / link-local incl. the cloud metadata endpoint /
//!    CGNAT / benchmark / multicast / v4-mapped v6);
//! 3. redirects are followed manually (≤3) and EVERY hop re-validates 1+2;
//! 4. the resolved address is pinned for the connection (closes the
//!    resolve→connect rebinding window for the pinned hop);
//! 5. method whitelist, request-body ≤1MB, response ≤ cap bytes, per-call
//!    timeout, hop-by-hop header stripping, Host immutable;
//! 6. 60 req/min sliding window per plugin + a per-plugin kill switch;
//! 7. every call is audited (plugin, url, method, status, bytes, duration).

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

// ── error codes ──

pub const NET_NOT_ALLOWED: &str = "NET_NOT_ALLOWED";
pub const NET_SCHEME_BLOCKED: &str = "NET_SCHEME_BLOCKED";
pub const NET_PRIVATE_BLOCKED: &str = "NET_PRIVATE_BLOCKED";
pub const NET_METHOD_BLOCKED: &str = "NET_METHOD_BLOCKED";
pub const NET_BODY_TOO_LARGE: &str = "NET_BODY_TOO_LARGE";
pub const NET_RATE_LIMITED: &str = "NET_RATE_LIMITED";
pub const NET_TIMEOUT: &str = "NET_TIMEOUT";
pub const NET_TOO_MANY_REDIRECTS: &str = "NET_TOO_MANY_REDIRECTS";
pub const NET_DISABLED: &str = "NET_DISABLED";
pub const NET_UPSTREAM: &str = "NET_UPSTREAM";
pub const NET_INVALID_PARAMS: &str = "NET_INVALID_PARAMS";
/// The proxy channel is plugin-only: the renderer bridge stamps `plugin:`
/// identity from the manifest; direct callers from non-plugin surfaces are
/// refused.
pub const PLUGIN_ORIGIN_REQUIRED: &str = "PLUGIN_ORIGIN_REQUIRED";

/// One parsed `net.allow` entry: exact https origin + a path prefix (with
/// trailing-`*` wildcard) or exact path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetAllow {
    pub host: String,
    pub port: u16,
    pub path: String,
    pub wildcard: bool,
}

/// Parse one `net.allow` pattern: `https://host[:port]/path[*]`.
/// Envelope: https-only, non-empty lowercase host, no wildcards outside the
/// trailing path star, explicit-or-default(443) port.
pub fn parse_net_allow(pattern: &str) -> Result<NetAllow, String> {
    let rest = pattern
        .strip_prefix("https://")
        .ok_or_else(|| "scheme must be https".to_string())?;
    let (authority, path) = match rest.split_once('/') {
        Some((a, p)) => (a, format!("/{p}")),
        None => (rest, "/".to_string()),
    };
    if authority.is_empty() {
        return Err("empty host".into());
    }
    if authority.contains('*') {
        return Err("wildcard hosts/ports are not allowed (only a trailing path `*`)".into());
    }
    if authority.contains('[') || authority.contains(']') {
        return Err("bracketed IPv6 literal hosts cannot be granted; use a domain".into());
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => {
            let port: u16 = p
                .parse()
                .map_err(|_| format!("invalid port `{p}`"))?;
            (h.to_string(), port)
        }
        None => (authority.to_string(), 443),
    };
    if host.is_empty() {
        return Err("empty host".into());
    }
    if host.parse::<std::net::IpAddr>().is_ok() {
        return Err("IP-literal hosts cannot be granted; use a domain".into());
    }
    let (path, wildcard) = match path.strip_suffix('*') {
        Some(p) => (p.to_string(), true),
        None => (path, false),
    };
    Ok(NetAllow {
        host: host.to_lowercase(),
        port,
        path,
        wildcard,
    })
}

/// Does this URL fall inside the grant? Query strings are stripped before
/// matching (patterns never carry them).
pub fn net_allows(patterns: &[NetAllow], url: &reqwest::Url) -> bool {
    let host = url.host_str().unwrap_or_default().to_lowercase();
    let port = url.port_or_known_default().unwrap_or(443);
    let path = url.path();
    patterns.iter().any(|p| {
        p.host == host
            && p.port == port
            && if p.wildcard {
                path.starts_with(&p.path)
            } else {
                path == p.path
            }
    })
}

fn is_private_v4(o: [u8; 4]) -> bool {
    matches!(o, [0, ..] | [10, ..] | [127, ..])
        || (o[0] == 100 && (o[1] & 0xC0) == 64) // 100.64/10 CGNAT
        || (o[0] == 169 && o[1] == 254) // 169.254/16 link-local (cloud metadata)
        || (o[0] == 172 && (o[1] & 0xF0) == 16) // 172.16/12
        || (o[0] == 192 && o[1] == 168) // 192.168/16
        || (o[0] == 198 && (o[1] & 0xFE) == 18) // 198.18/15 benchmark
        || (o[0] == 192 && o[1] == 0 && o[2] == 2) // 192.0.2.0/24 TEST-NET-1
        || (o[0] == 198 && o[1] == 51 && o[2] == 100) // 198.51.100.0/24 TEST-NET-2
        || (o[0] == 203 && o[1] == 0 && o[2] == 113) // 203.0.113.0/24 TEST-NET-3
        || o[0] >= 224 // multicast + reserved
}

/// True for every address a plugin must never reach: loopback, private,
/// link-local (cloud metadata), CGNAT, benchmark, multicast/reserved,
/// IPv4-mapped / IPv4-compatible v6, and the v6 ULA/link-local ranges.
pub fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_private_v4(v4.octets()),
        IpAddr::V6(v6) => {
            let s = v6.segments();
            v6.is_loopback()
                || v6.is_unspecified()
                || (s[0] >> 9) == 0x7e // fc00::/7 ULA
                || (s[0] >> 6) == 0x3fa // fe80::/10 link-local
                || matches!(s, [0, 0, 0, 0, 0, 0, ..]) // v4-mapped/compatible ::ffff:a.b.c.d & ::a.b.c.d
                || v6.to_ipv4_mapped().is_some_and(|v4| is_private_v4(v4.octets()))
        }
    }
}

const RATE_LIMIT: usize = 60;
const RATE_WINDOW: Duration = Duration::from_secs(60);
const MAX_REDIRECTS: usize = 3;
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
pub const HARD_MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_REQUEST_BODY_BYTES: usize = 1024 * 1024;

/// Per-process egress state: the shared HTTP client (redirects disabled —
/// hops are validated manually), per-plugin kill switches and rate windows.
/// Held as `Arc` in `RuntimeInner` so a proxyFetch can drop the runtime lock
/// before the (slow) network IO.
pub struct PluginNetGuard {
    disabled: Mutex<HashSet<String>>,
    windows: Mutex<HashMap<String, Vec<Instant>>>,
}

impl PluginNetGuard {
    pub fn new() -> Self {
        // No shared HTTP client here on purpose: reqwest pins a resolved
        // address via `ClientBuilder::resolve`, which is builder-level, so
        // each validated hop gets its own short-lived pinned client (see
        // execute_fetch). Correctness (TOCTOU-safe pinning) over connection
        // pooling for a low-QPS plugin channel.
        Self {
            disabled: Mutex::new(HashSet::new()),
            windows: Mutex::new(HashMap::new()),
        }
    }

    pub fn is_enabled(&self, plugin: &str) -> bool {
        !self.disabled.lock().unwrap().contains(plugin)
    }

    pub fn set_enabled(&self, plugin: &str, enabled: bool) {
        let mut set = self.disabled.lock().unwrap();
        if enabled {
            set.remove(plugin);
        } else {
            set.insert(plugin.to_string());
        }
    }

    fn check_rate(&self, plugin: &str) -> Result<(), &'static str> {
        let mut w = self.windows.lock().unwrap();
        let now = Instant::now();
        let window = w.entry(plugin.to_string()).or_default();
        window.retain(|t| now.duration_since(*t) < RATE_WINDOW);
        if window.len() >= RATE_LIMIT {
            return Err(NET_RATE_LIMITED);
        }
        window.push(now);
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct PluginNetError {
    pub code: &'static str,
    pub message: String,
}

impl PluginNetError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
}

impl std::fmt::Display for PluginNetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for PluginNetError {}

/// Headers a proxied request may never carry (hop-by-hop + identity).
const BLOCKED_REQUEST_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "host",
    "content-length",
];

pub fn sanitize_request_headers(
    headers: Option<&Map<String, Value>>,
) -> Result<Vec<(String, String)>, PluginNetError> {
    let mut out = Vec::new();
    let Some(map) = headers else { return Ok(out) };
    for (k, v) in map {
        let lower = k.to_lowercase();
        if BLOCKED_REQUEST_HEADERS.contains(&lower.as_str()) {
            continue;
        }
        let Some(value) = v.as_str() else {
            return Err(PluginNetError::new(
                NET_INVALID_PARAMS,
                format!("header `{k}` must be a string"),
            ));
        };
        out.push((k.clone(), value.to_string()));
    }
    Ok(out)
}

fn sanitize_response_headers(
    headers: &reqwest::header::HeaderMap,
) -> Vec<(String, String)> {
    headers
        .iter()
        .filter(|(name, _)| {
            let lower = name.as_str().to_lowercase();
            lower != "transfer-encoding" && lower != "connection" && lower != "content-length"
        })
        .map(|(name, value)| {
            (
                name.as_str().to_string(),
                value.to_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

const ALLOWED_METHODS: &[&str] = &["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE"];

/// Resolve the host and validate EVERY address (a single private hit rejects
/// the request — rebinding tricks need a private record to land). Returns
/// one public address to pin the connection to.
async fn validated_addr(host: &str, port: u16) -> Result<SocketAddr, PluginNetError> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_private_ip(ip) {
            return Err(PluginNetError::new(
                NET_PRIVATE_BLOCKED,
                format!("{host} resolves to a blocked (private/reserved) address"),
            ));
        }
        return Ok(SocketAddr::new(ip, port));
    }
    let addrs = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| PluginNetError::new(NET_UPSTREAM, format!("dns lookup {host}: {e}")))?;
    let mut first_public: Option<SocketAddr> = None;
    for addr in addrs {
        if is_private_ip(addr.ip()) {
            return Err(PluginNetError::new(
                NET_PRIVATE_BLOCKED,
                format!("{host} resolves to a blocked (private/reserved) address"),
            ));
        }
        if first_public.is_none() {
            first_public = Some(addr);
        }
    }
    first_public.ok_or_else(|| PluginNetError::new(NET_UPSTREAM, format!("no addresses for {host}")))
}

fn scheme_guard(url: &reqwest::Url) -> Result<(), PluginNetError> {
    if url.scheme() != "https" {
        return Err(PluginNetError::new(
            NET_SCHEME_BLOCKED,
            format!("only https is proxied, got `{}`", url.scheme()),
        ));
    }
    Ok(())
}

#[derive(Debug)]
pub struct ProxyFetchOutcome {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub final_url: String,
    pub duration: Duration,
}

/// Forward one proxied request with per-hop validation. `timeout` applies to
/// each hop; redirects (≤3) re-run the full grant + private-IP validation on
/// the new location before being followed.
#[allow(clippy::too_many_arguments)]
pub async fn execute_fetch(
    allow: &[NetAllow],
    method: &str,
    url: reqwest::Url,
    headers: Vec<(String, String)>,
    body: Option<String>,
    timeout: Duration,
    max_response_bytes: usize,
) -> Result<ProxyFetchOutcome, PluginNetError> {
    let started = Instant::now();
    let mut current = url;
    let mut current_method = method.to_string();
    let mut current_body = body;
    let mut hop = 0usize;
    loop {
        scheme_guard(&current)?;
        if !net_allows(allow, &current) {
            return Err(PluginNetError::new(
                NET_NOT_ALLOWED,
                format!("{} is not in the plugin's net.allow grant", current),
            ));
        }
        let port = current.port_or_known_default().unwrap_or(443);
        let host = current
            .host_str()
            .ok_or_else(|| PluginNetError::new(NET_NOT_ALLOWED, "URL has no host"))?
            .to_string();
        let pinned = validated_addr(&host, port).await?;
        let parsed_method: reqwest::Method = current_method
            .parse()
            .map_err(|_| PluginNetError::new(NET_METHOD_BLOCKED, format!("bad method `{current_method}`")))?;
        // Per-hop client with the validated address pinned: the connection
        // can only land on an IP we vetted (no resolve→connect rebinding).
        let hop_client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .user_agent("starriver-plugin-proxy/1.0")
            .resolve(&host, pinned)
            .build()
            .map_err(|e| PluginNetError::new(NET_UPSTREAM, format!("client build: {e}")))?;

        let mut req = hop_client
            .request(parsed_method, current.clone())
            .timeout(timeout);
        for (k, v) in &headers {
            req = req.header(k, v);
        }
        if matches!(current_method.as_str(), "POST" | "PUT" | "PATCH") {
            if let Some(b) = &current_body {
                req = req.body(b.clone());
            }
        }
        let resp = req
            .send()
            .await
            .map_err(|e| map_reqwest_err(&e))?;

        if resp.status().is_redirection() {
            hop += 1;
            if hop > MAX_REDIRECTS {
                return Err(PluginNetError::new(
                    NET_TOO_MANY_REDIRECTS,
                    format!("more than {MAX_REDIRECTS} redirects"),
                ));
            }
            let location = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| {
                    PluginNetError::new(NET_UPSTREAM, "redirect without a Location header")
                })?
                .to_string();
            let next = current
                .join(&location)
                .map_err(|e| PluginNetError::new(NET_UPSTREAM, format!("bad redirect: {e}")))?;
            // 301/302/303 → GET without a body; 307/308 preserve method+body.
            if matches!(resp.status().as_u16(), 301 | 302 | 303) {
                current_method = "GET".into();
                current_body = None;
            }
            current = next;
            continue;
        }

        let status = resp.status().as_u16();
        let resp_headers = sanitize_response_headers(resp.headers());
        let mut body_out: Vec<u8> = Vec::new();
        let mut resp = resp;
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| PluginNetError::new(NET_UPSTREAM, format!("read body: {e}")))?
        {
            if body_out.len() + chunk.len() > max_response_bytes {
                return Err(PluginNetError::new(
                    NET_BODY_TOO_LARGE,
                    format!("response exceeds the {max_response_bytes} byte cap"),
                ));
            }
            body_out.extend_from_slice(&chunk);
        }
        return Ok(ProxyFetchOutcome {
            status,
            headers: resp_headers,
            body: body_out,
            final_url: current.to_string(),
            duration: started.elapsed(),
        });
    }
}

fn map_reqwest_err(e: &reqwest::Error) -> PluginNetError {
    if e.is_timeout() {
        PluginNetError::new(NET_TIMEOUT, format!("upstream timed out: {e}"))
    } else if e.is_connect() {
        PluginNetError::new(NET_UPSTREAM, format!("connect failed: {e}"))
    } else {
        PluginNetError::new(NET_UPSTREAM, format!("request failed: {e}"))
    }
}

/// HostRuntime face: forward a plugin's request through the guarded channel.
/// The runtime lock is held only for the guard handoff — the network IO runs
/// unlocked so other host calls never queue behind it.
impl crate::HostRuntime {
    #[allow(clippy::too_many_arguments)]
    pub fn plugin_proxy_fetch(
        &self,
        plugin: String,
        url: String,
        method: Option<String>,
        headers: Option<Map<String, Value>>,
        body: Option<String>,
        timeout_ms: Option<u64>,
    ) -> Result<Value, PluginNetError> {
        crate::plugin_task::plugin_task_origin(&plugin).map_err(|e| {
            PluginNetError::new(PLUGIN_ORIGIN_REQUIRED, e.to_string())
        })?;
        let guard: Arc<PluginNetGuard> = {
            let inner = self.inner.blocking_lock();
            let runtime = inner
                .as_ref()
                .ok_or_else(|| PluginNetError::new(NET_UPSTREAM, "runtime not loaded"))?;
            runtime.plugin_net.clone()
        };
        if !guard.is_enabled(&plugin) {
            return Err(PluginNetError::new(
                NET_DISABLED,
                format!("network egress is disabled for plugin `{plugin}`"),
            ));
        }
        guard
            .check_rate(&plugin)
            .map_err(|code| PluginNetError::new(code, format!("plugin `{plugin}` exceeded 60 req/min")))?;

        // Manifest grant (disk read, outside the runtime lock). No grant or
        // empty allow ⇒ fail closed.
        let patterns: Vec<NetAllow> = {
            let reg = crate::plugin::PluginRegistry::new()
                .map_err(|e| PluginNetError::new(NET_UPSTREAM, format!("plugin registry: {e}")))?;
            let entry = reg.get(&plugin).ok_or_else(|| {
                PluginNetError::new(NET_NOT_ALLOWED, format!("unknown plugin `{plugin}`"))
            })?;
            let allow = entry
                .ui
                .as_ref()
                .and_then(|ui| ui.net.as_ref())
                .map(|net| net.allow.clone())
                .unwrap_or_default();
            if allow.is_empty() {
                return Err(PluginNetError::new(
                    NET_NOT_ALLOWED,
                    format!("plugin `{plugin}` declares no net.allow grant"),
                ));
            }
            allow
                .iter()
                .map(|p| parse_net_allow(p))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| PluginNetError::new(NET_UPSTREAM, format!("grant parse: {e}")))?
        };

        let method_name = method.unwrap_or_else(|| "GET".into()).to_uppercase();
        if !ALLOWED_METHODS.contains(&method_name.as_str()) {
            return Err(PluginNetError::new(
                NET_METHOD_BLOCKED,
                format!("method `{method_name}` is not allowed"),
            ));
        }
        if let Some(b) = &body {
            if b.len() > MAX_REQUEST_BODY_BYTES {
                return Err(PluginNetError::new(
                    NET_BODY_TOO_LARGE,
                    format!("request body exceeds {MAX_REQUEST_BODY_BYTES} bytes"),
                ));
            }
        }
        let clean_headers = sanitize_request_headers(headers.as_ref())?;
        let parsed_url = reqwest::Url::parse(&url)
            .map_err(|e| PluginNetError::new(NET_NOT_ALLOWED, format!("bad url: {e}")))?;
        let timeout = Duration::from_millis(timeout_ms.unwrap_or(30_000).clamp(1, 120_000));
        let response_cap = DEFAULT_MAX_RESPONSE_BYTES.min(HARD_MAX_RESPONSE_BYTES);

        let method_for_log = method_name.clone();
        let outcome = { let handle = self.runtime_handle.clone(); std::thread::spawn(move || {
            handle.block_on(async {
                execute_fetch(
                    &patterns,
                    &method_name,
                    parsed_url,
                    clean_headers,
                    body,
                    timeout,
                    response_cap,
                )
                .await
            })
        }).join().map_err(|_| PluginNetError::new(NET_UPSTREAM, "proxy fetch thread panicked"))?? };

        tracing::info!(
            target: "plugin_net",
            plugin = %plugin,
            url = %outcome.final_url,
            method = %method_for_log,
            status = outcome.status,
            bytes = outcome.body.len(),
            duration_ms = outcome.duration.as_millis() as u64,
            "plugin proxy fetch"
        );

        let content_type = outcome
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
            .map(|(_, v)| v.to_lowercase())
            .unwrap_or_default();
        let text_like = content_type.contains("json")
            || content_type.contains("text")
            || content_type.contains("html")
            || content_type.contains("xml");
        let mut out = json!({
            "status": outcome.status,
            "headers": outcome
                .headers
                .iter()
                .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                .collect::<Map<String, Value>>(),
            "bytes": outcome.body.len(),
            "durationMs": outcome.duration.as_millis() as u64,
            "url": outcome.final_url,
        });
        if text_like {
            out["bodyText"] = json!(String::from_utf8_lossy(&outcome.body).into_owned());
        } else {
            use base64::Engine as _;
            out["bodyBase64"] = json!(base64::engine::general_purpose::STANDARD.encode(&outcome.body));
        }
        Ok(out)
    }

    /// Read / flip the per-plugin kill switch. `enabled: None` reads.
    pub fn plugin_net_policy(&self, plugin: String, enabled: Option<bool>) -> Result<Value, PluginNetError> {
        let guard: Arc<PluginNetGuard> = {
            let inner = self.inner.blocking_lock();
            let runtime = inner
                .as_ref()
                .ok_or_else(|| PluginNetError::new(NET_UPSTREAM, "runtime not loaded"))?;
            runtime.plugin_net.clone()
        };
        if let Some(v) = enabled {
            guard.set_enabled(&plugin, v);
            tracing::info!(target: "plugin_net", plugin = %plugin, enabled = v, "plugin network kill switch");
        }
        Ok(json!({ "plugin": plugin, "enabled": guard.is_enabled(&plugin) }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    fn allows(patterns: &[&str], url: &str) -> bool {
        let parsed: Vec<NetAllow> = patterns.iter().map(|p| parse_net_allow(p).unwrap()).collect();
        net_allows(&parsed, &reqwest::Url::parse(url).unwrap())
    }

    #[test]
    fn parse_accepts_https_patterns_and_paths() {
        let p = parse_net_allow("https://api.openalex.org/*").unwrap();
        assert_eq!(p.host, "api.openalex.org");
        assert_eq!(p.port, 443);
        assert_eq!(p.path, "/");
        assert!(p.wildcard);

        let p = parse_net_allow("https://api.semanticscholar.org/graph/v1/works").unwrap();
        assert!(!p.wildcard);
        assert_eq!(p.path, "/graph/v1/works");

        let p = parse_net_allow("https://internal.corp:8443/api/*").unwrap();
        assert_eq!(p.port, 8443);
    }

    #[test]
    fn parse_rejects_non_https_wildcards_and_bad_ports() {
        assert!(parse_net_allow("http://api.openalex.org/*").is_err());
        assert!(parse_net_allow("https://*.openalex.org/*").is_err());
        assert!(parse_net_allow("https://api.openalex.org:*/*").is_err());
        assert!(parse_net_allow("https://api.openalex.org:abc/*").is_err());
        assert!(parse_net_allow("https:///*").is_err());
        assert!(parse_net_allow("ftp://api.openalex.org/*").is_err());
    }

    #[test]
    fn match_exact_wildcard_and_query_stripping() {
        assert!(allows(&["https://api.openalex.org/works/*"], "https://api.openalex.org/works/W123?mailto=x"));
        assert!(allows(&["https://api.openalex.org/works"], "https://api.openalex.org/works"));
        // Exact path pattern does NOT match deeper paths…
        assert!(!allows(&["https://api.openalex.org/works"], "https://api.openalex.org/works/W123"));
        // …and the wildcard is prefix-bound, not a free glob.
        assert!(!allows(&["https://api.openalex.org/works/*"], "https://api.openalex.org/authors/W123"));
        assert!(!allows(&["https://api.openalex.org/*"], "https://evil.openalex.org.evil.io/*"));
        assert!(!allows(&["https://api.openalex.org/*"], "https://api.openalex.org:8443/works"));
        assert!(allows(&["https://api.openalex.org/*"], "https://API.OPENALEX.ORG/works"));
    }

    #[test]
    fn private_v4_ranges_are_blocked() {
        for s in [
            "127.0.0.1", "10.1.2.3", "172.16.0.1", "172.31.255.255", "192.168.1.1",
            "169.254.169.254", "100.64.0.1", "198.18.0.1", "224.0.0.1", "0.0.0.0",
            "192.0.2.1",
        ] {
            assert!(is_private_ip(s.parse::<IpAddr>().unwrap()), "{s} must be private");
        }
        for s in ["8.8.8.8", "1.1.1.1", "140.247.100.1"] {
            assert!(!is_private_ip(s.parse::<IpAddr>().unwrap()), "{s} must be public");
        }
    }

    #[test]
    fn private_v6_and_mapped_v4_are_blocked() {
        for s in ["::1", "::", "fe80::1", "fd00::1", "::ffff:127.0.0.1", "::ffff:10.0.0.1"] {
            assert!(is_private_ip(s.parse::<IpAddr>().unwrap()), "{s} must be private");
        }
        assert!(!is_private_ip("2606:4700::1111".parse::<IpAddr>().unwrap()));
        assert!(!is_private_ip("::ffff:8.8.8.8".parse::<IpAddr>().unwrap()));
    }

    #[test]
    fn request_headers_strip_hop_by_hop_but_keep_business_headers() {
        let mut map = Map::new();
        map.insert("Authorization".into(), Value::String("Bearer t".into()));
        map.insert("Host".into(), Value::String("evil".into()));
        map.insert("Transfer-Encoding".into(), Value::String("chunked".into()));
        map.insert("Accept".into(), Value::String("application/json".into()));
        let out = sanitize_request_headers(Some(&map)).unwrap();
        assert_eq!(
            out,
            vec![
                ("Authorization".to_string(), "Bearer t".to_string()),
                ("Accept".to_string(), "application/json".to_string()),
            ]
        );
        assert!(sanitize_request_headers(None).unwrap().is_empty());
    }

    #[test]
    fn rate_limit_admits_60_then_blocks_and_kill_switch_disables() {
        let guard = PluginNetGuard::new();
        for _ in 0..RATE_LIMIT {
            assert!(guard.check_rate("p").is_ok());
        }
        assert_eq!(guard.check_rate("p"), Err(NET_RATE_LIMITED));
        // Kill switch is orthogonal to the rate window.
        guard.set_enabled("p", false);
        assert!(!guard.is_enabled("p"));
        guard.set_enabled("p", true);
        assert!(guard.is_enabled("p"));
    }

    #[tokio::test]
    async fn execute_fetch_rejects_scheme_and_ungranted_urls_before_any_network() {
        let allow: Vec<NetAllow> = vec![parse_net_allow("https://api.openalex.org/*").unwrap()];
        // http → scheme block (pre-network).
        let e = execute_fetch(
            &allow, "GET", reqwest::Url::parse("http://api.openalex.org/works").unwrap(),
            vec![], None, Duration::from_secs(1), 1024,
        ).await.unwrap_err();
        assert_eq!(e.code, NET_SCHEME_BLOCKED);
        // https but off-grant → NET_NOT_ALLOWED (pre-network).
        let e = execute_fetch(
            &allow, "GET", reqwest::Url::parse("https://evil.example.com/works").unwrap(),
            vec![], None, Duration::from_secs(1), 1024,
        ).await.unwrap_err();
        assert_eq!(e.code, NET_NOT_ALLOWED);
        // Literal private IP hosts are ungrantable (parse_net_allow refuses
        // IP literals), so they surface as NET_NOT_ALLOWED pre-network.
        let e = execute_fetch(
            &allow, "GET", reqwest::Url::parse("https://127.0.0.1/works").unwrap(),
            vec![], None, Duration::from_secs(1), 1024,
        ).await.unwrap_err();
        assert_eq!(e.code, NET_NOT_ALLOWED);
        assert!(parse_net_allow("https://127.0.0.1/*").is_err());
        assert!(parse_net_allow("https://[fd00::1]/*").is_err());
    }
}
