//! Shared HTML text-extraction helpers for the web tools (web_search,
//! web_fetch), a same-origin redirect follower, plus a test-only mock HTTP
//! server.

use std::future::Future;
use std::pin::Pin;

use regex_lite::Regex;

/// Maximum redirect hops web tools follow, on the same origin.
pub const MAX_WEB_REDIRECTS: usize = 5;

/// Errors from [`get_following_same_origin`].
#[derive(Debug)]
pub enum WebRedirectError {
    InvalidUrl(String),
    /// The per-hop callback (SSRF guard) refused the target.
    HopBlocked(String),
    RedirectBlocked(String),
    TooManyRedirects,
    RequestFailed(String),
}

/// Whether two URLs share scheme, host, and effective port.
pub fn same_origin(a: &reqwest::Url, b: &reqwest::Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
}

/// GET `start_url`, following only same-origin redirects up to the hop cap.
/// `on_hop` runs before every request so each hop is re-validated (the SSRF
/// guard in web_fetch; a no-op in web_search).
pub async fn get_following_same_origin<F>(
    client: &reqwest::Client,
    start_url: &str,
    on_hop: F,
) -> Result<reqwest::Response, WebRedirectError>
where
    F: Fn(&str) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>> + Send + Sync,
{
    let mut url = reqwest::Url::parse(start_url)
        .map_err(|e| WebRedirectError::InvalidUrl(format!("invalid URL: {e}")))?;
    for _ in 0..=MAX_WEB_REDIRECTS {
        on_hop(url.as_str())
            .await
            .map_err(WebRedirectError::HopBlocked)?;
        let resp = client.get(url.clone()).send().await.map_err(|e| {
            // Flatten the reqwest error chain: the Display alone only
            // names the URL, hiding the connect/read root cause.
            let mut msg = format!("request failed: {e}");
            let mut src = std::error::Error::source(&e);
            while let Some(s) = src {
                msg.push_str(&format!(": {s}"));
                src = std::error::Error::source(s);
            }
            WebRedirectError::RequestFailed(msg)
        })?;
        match redirect_step(&url, resp)? {
            HopOutcome::Response(resp) => return Ok(resp),
            HopOutcome::Next(next) => url = next,
        }
    }
    Err(WebRedirectError::TooManyRedirects)
}

enum HopOutcome {
    Response(reqwest::Response),
    Next(reqwest::Url),
}

/// Classify one hop response: a redirect (same-origin only) yields the next
/// URL, anything else is the final response.
fn redirect_step(
    url: &reqwest::Url,
    resp: reqwest::Response,
) -> Result<HopOutcome, WebRedirectError> {
    if !resp.status().is_redirection() {
        return Ok(HopOutcome::Response(resp));
    }
    let location = resp
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| {
            WebRedirectError::RedirectBlocked(format!(
                "redirect response (HTTP {}) without a Location header",
                resp.status()
            ))
        })?;
    let next = url
        .join(location)
        .map_err(|e| WebRedirectError::InvalidUrl(format!("invalid redirect Location: {e}")))?;
    // Same-origin is not enough: an embedded-credential redirect target on
    // the SAME origin still violates the tool's "no userinfo" contract.
    if !next.username().is_empty() || next.password().is_some() {
        return Err(WebRedirectError::InvalidUrl(
            "redirect target carries embedded credentials".into(),
        ));
    }
    if !same_origin(url, &next) {
        return Err(WebRedirectError::RedirectBlocked(format!(
            "cross-origin redirect to {next} is not followed automatically; retry against that URL directly"
        )));
    }
    Ok(HopOutcome::Next(next))
}

/// GET `start_url` with DNS pinning (web_fetch's rebinding defense): for every
/// hop, `pin_hop` resolves AND validates the target once, returning the exact
/// address the transport must use; `client_for` builds a per-hop client with
/// that address pinned via `reqwest::ClientBuilder::resolve`, so the guard's
/// resolution and the connection cannot diverge (no TOCTOU window).
pub async fn get_pinned_following_same_origin<F, C>(
    client_for: C,
    start_url: &str,
    pin_hop: F,
) -> Result<reqwest::Response, WebRedirectError>
where
    F: Fn(&str) -> Pin<Box<dyn Future<Output = Result<std::net::SocketAddr, String>> + Send>>
        + Send
        + Sync,
    C: Fn(&str, std::net::SocketAddr) -> Result<reqwest::Client, WebRedirectError> + Send + Sync,
{
    let mut url = reqwest::Url::parse(start_url)
        .map_err(|e| WebRedirectError::InvalidUrl(format!("invalid URL: {e}")))?;
    for _ in 0..=MAX_WEB_REDIRECTS {
        let host = url
            .host_str()
            .ok_or_else(|| WebRedirectError::InvalidUrl("URL is missing a host".into()))?
            .to_string();
        let addr = pin_hop(url.as_str())
            .await
            .map_err(WebRedirectError::HopBlocked)?;
        let client = client_for(&host, addr)?;
        let resp = client.get(url.clone()).send().await.map_err(|e| {
            // Flatten the reqwest error chain: the Display alone only
            // names the URL, hiding the connect/read root cause.
            let mut msg = format!("request failed: {e}");
            let mut src = std::error::Error::source(&e);
            while let Some(s) = src {
                msg.push_str(&format!(": {s}"));
                src = std::error::Error::source(s);
            }
            WebRedirectError::RequestFailed(msg)
        })?;
        match redirect_step(&url, resp)? {
            HopOutcome::Response(resp) => return Ok(resp),
            HopOutcome::Next(next) => url = next,
        }
    }
    Err(WebRedirectError::TooManyRedirects)
}

/// Remove `<script>` and `<style>` blocks entirely (non-greedy,
/// case-insensitive) so non-content text never reaches the model.
pub fn strip_script_style(s: &str) -> String {
    let script = Regex::new(r"(?is)<script[^>]*>.*?</script>").expect("static regex");
    let style = Regex::new(r"(?is)<style[^>]*>.*?</style>").expect("static regex");
    style
        .replace_all(&script.replace_all(s, " "), " ")
        .into_owned()
}

/// Tags become spaces; named and numeric entities decode; runs of whitespace
/// collapse to single spaces.
pub fn clean_html(s: &str) -> String {
    let no_blocks = strip_script_style(s);
    let no_tags = Regex::new(r"<[^>]*>")
        .expect("static regex")
        .replace_all(&no_blocks, " ");
    // BUG-45: entities decode in ONE pass. The previous sequential
    // str::replace list ran `&amp;` first, so `&amp;lt;` double-decoded to
    // `<` while its numeric spelling `&#38;lt;` correctly stayed `&lt;`.
    let entities = Regex::new(r#"&(?:amp|lt|gt|quot|#39|nbsp|ensp|#x([0-9a-fA-F]+)|#(\d+));"#)
        .expect("static regex");
    let decoded = entities.replace_all(&no_tags, |caps: &regex_lite::Captures| {
        // Group 1 = hex numeric reference (&#x4e2d;), group 2 = decimal.
        if let Some(hex) = caps.get(1) {
            return u32::from_str_radix(hex.as_str(), 16)
                .ok()
                .and_then(char::from_u32)
                .map(|c| c.to_string())
                .unwrap_or_else(|| caps[0].to_string());
        }
        if let Some(num) = caps.get(2) {
            return num
                .as_str()
                .parse::<u32>()
                .ok()
                .and_then(char::from_u32)
                .map(|c| c.to_string())
                .unwrap_or_else(|| caps[0].to_string());
        }
        match &caps[0][1..caps[0].len() - 1] {
            "amp" => "&",
            "lt" => "<",
            "gt" => ">",
            "quot" => "\"",
            "#39" => "'",
            _ => " ", // nbsp / ensp
        }
        .to_string()
    });
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Test-only HTTP plumbing shared by the web tool suites and engine-level e2e
/// (serves canned raw responses on a loopback listener).
pub mod test_support {
    use std::sync::{Arc, Mutex};

    /// Serve canned raw HTTP responses on 127.0.0.1 and return the base URL.
    /// Each response is written to exactly one accepted connection, in order.
    pub async fn spawn_http_server(responses: Vec<String>) -> String {
        spawn_http_server_capture(responses).await.0
    }

    /// Like [`spawn_http_server`], but accepts raw byte responses so tests can
    /// serve binary bodies (e.g. non-UTF-8 charsets).
    pub async fn spawn_http_server_bytes(responses: Vec<Vec<u8>>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for resp in responses {
                let (mut sock, _) = listener.accept().await.unwrap();
                // Drain the request head before writing: closing a socket
                // with unread inbound data sends RST instead of FIN and the
                // client sees WSAECONNABORTED (os error 10053 on Windows).
                let mut buf = [0u8; 8192];
                let _ = sock.read(&mut buf).await;
                let _ = sock.write_all(&resp).await;
            }
        });
        format!("http://{addr}")
    }

    /// Like [`spawn_http_server`], but also records each request's raw head
    /// (up to 8 KiB) so tests can assert request headers.
    pub async fn spawn_http_server_capture(
        responses: Vec<String>,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        tokio::spawn(async move {
            for resp in responses {
                let (mut sock, _) = listener.accept().await.unwrap();
                // Drain the FULL request head (GETs carry no body): a single
                // read can leave the tail of a segmented head unread, and
                // closing with unread inbound data sends RST instead of FIN
                // — the client then aborts with WSAECONNABORTED (10053).
                let mut buf = [0u8; 8192];
                let mut head = Vec::new();
                loop {
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    head.extend_from_slice(&buf[..n]);
                    if head.windows(4).any(|w| w == b"\r\n\r\n") || head.len() >= 8192 {
                        break;
                    }
                }
                recorded
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&head).into_owned());
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });
        (format!("http://{addr}"), requests)
    }

    /// Build a minimal `200 OK` raw response with an exact body.
    pub fn http_ok(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{http_ok, spawn_http_server};
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn client() -> reqwest::Client {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap()
    }

    fn noop_hop() -> impl Fn(&str) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>> {
        |_url: &str| Box::pin(async { Ok::<(), String>(()) })
    }

    #[test]
    fn clean_html_strips_blocks_tags_and_entities() {
        let html = "<html><script>alert(1)</script><style>.x{}</style><b>Hello</b> &amp; <i>world</i>&#33;</html>";
        assert_eq!(clean_html(html), "Hello & world !");
    }

    #[test]
    fn clean_html_collapses_whitespace() {
        assert_eq!(clean_html("a\n  b\t c"), "a b c");
    }

    /// BUG-45 regression: entity decoding must be a single pass. The
    /// sequential `str::replace` list ran `&amp;` first, so `&amp;lt;`
    /// (the encoding of the literal text `&lt;`) double-decoded to `<`,
    /// while its numeric spelling `&#38;lt;` correctly stayed `&lt;` —
    /// two encodings of one literal decoded differently.
    #[test]
    fn clean_html_named_entities_decode_in_one_pass() {
        assert_eq!(clean_html("&amp;lt;"), "&lt;", "no double decode");
        assert_eq!(
            clean_html("&amp;lt;"),
            clean_html("&#38;lt;"),
            "named and numeric encodings of & must agree"
        );
        // Plain &amp; still decodes once.
        assert_eq!(clean_html("a &amp; b"), "a & b");
    }

    #[test]
    fn same_origin_normalizes_scheme_host_port() {
        use reqwest::Url;
        assert!(same_origin(
            &Url::parse("http://example.com/a").unwrap(),
            &Url::parse("http://example.com:80/b").unwrap(),
        ));
        assert!(same_origin(
            &Url::parse("https://example.com/a").unwrap(),
            &Url::parse("https://example.com:443/b").unwrap(),
        ));
        assert!(!same_origin(
            &Url::parse("https://example.com/a").unwrap(),
            &Url::parse("http://example.com/b").unwrap(),
        ));
        assert!(!same_origin(
            &Url::parse("http://example.com/a").unwrap(),
            &Url::parse("http://other.com/b").unwrap(),
        ));
        assert!(!same_origin(
            &Url::parse("http://example.com:8080/a").unwrap(),
            &Url::parse("http://example.com/b").unwrap(),
        ));
    }

    #[tokio::test]
    async fn follower_returns_final_response_after_same_origin_redirect() {
        let url = spawn_http_server(vec![
            "HTTP/1.1 302 Found\r\nLocation: /next\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .into(),
            http_ok("landed"),
        ])
        .await;
        let resp = get_following_same_origin(&client(), &format!("{url}/start"), noop_hop())
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        assert_eq!(resp.text().await.unwrap(), "landed");
    }

    #[tokio::test]
    async fn follower_blocks_cross_origin_redirect() {
        let url = spawn_http_server(vec![
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.2:8080/x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .into(),
        ])
        .await;
        let err = get_following_same_origin(&client(), &format!("{url}/start"), noop_hop())
            .await
            .unwrap_err();
        assert!(matches!(err, WebRedirectError::RedirectBlocked(_)));
    }

    #[tokio::test]
    async fn follower_blocks_redirect_without_location() {
        let url = spawn_http_server(vec![
            "HTTP/1.1 302 Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
        ])
        .await;
        let err = get_following_same_origin(&client(), &format!("{url}/start"), noop_hop())
            .await
            .unwrap_err();
        assert!(matches!(err, WebRedirectError::RedirectBlocked(_)));
    }

    #[tokio::test]
    async fn follower_stops_after_max_redirects() {
        let url = spawn_http_server(vec![
            "HTTP/1.1 302 Found\r\nLocation: /again\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .into();
            MAX_WEB_REDIRECTS + 1
        ])
        .await;
        let err = get_following_same_origin(&client(), &format!("{url}/start"), noop_hop())
            .await
            .unwrap_err();
        assert!(matches!(err, WebRedirectError::TooManyRedirects));
    }

    #[tokio::test]
    async fn follower_runs_on_hop_for_every_request() {
        let url = spawn_http_server(vec![
            "HTTP/1.1 302 Found\r\nLocation: /next\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .into(),
            http_ok("x"),
        ])
        .await;
        let hops = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hops);
        let resp = get_following_same_origin(&client(), &format!("{url}/start"), move |_url| {
            let counter = Arc::clone(&counter);
            Box::pin(async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok::<(), String>(())
            })
        })
        .await
        .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        assert_eq!(hops.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn follower_surfaces_on_hop_refusal() {
        let url = spawn_http_server(vec![http_ok("x")]).await;
        let err = get_following_same_origin(&client(), &format!("{url}/a"), |_url| {
            Box::pin(async { Err::<(), String>("refused".into()) })
        })
        .await
        .unwrap_err();
        assert!(matches!(err, WebRedirectError::HopBlocked(_)));
    }

    #[tokio::test]
    async fn follower_rejects_invalid_url() {
        let err = get_following_same_origin(&client(), "not a url", noop_hop())
            .await
            .unwrap_err();
        assert!(matches!(err, WebRedirectError::InvalidUrl(_)));
    }

    #[tokio::test]
    async fn follower_surfaces_request_failure() {
        let err = get_following_same_origin(&client(), "http://127.0.0.1:1/x", noop_hop())
            .await
            .unwrap_err();
        assert!(matches!(err, WebRedirectError::RequestFailed(_)));
    }

    #[tokio::test]
    async fn pinned_follower_requests_the_pinned_address() {
        // The hostname is unresolvable by DNS; only the pinned mock address can
        // serve the request, proving the transport used the pin.
        let url = spawn_http_server(vec![http_ok("pinned!")]).await;
        let addr: std::net::SocketAddr = url
            .trim_start_matches("http://")
            .parse()
            .expect("mock server binds a literal addr");
        let resp = get_pinned_following_same_origin(
            |host, pin| {
                let mut builder =
                    reqwest::Client::builder().redirect(reqwest::redirect::Policy::none());
                builder = builder.resolve(host, pin);
                builder
                    .build()
                    .map_err(|e| WebRedirectError::RequestFailed(format!("client: {e}")))
            },
            &format!("http://rebinding.test:{}/x", addr.port()),
            move |_url| {
                let addr = addr;
                Box::pin(async move { Ok::<std::net::SocketAddr, String>(addr) })
            },
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        assert_eq!(resp.text().await.unwrap(), "pinned!");
    }

    #[tokio::test]
    async fn pinned_follower_surfaces_hop_refusal_and_invalid_url() {
        let err = get_pinned_following_same_origin(
            |_h, _a| Ok(client()),
            "not a url",
            |_url| {
                Box::pin(async {
                    Ok::<std::net::SocketAddr, String>("127.0.0.1:1".parse().unwrap())
                })
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, WebRedirectError::InvalidUrl(_)));

        let err = get_pinned_following_same_origin(
            |_h, _a| Ok(client()),
            "http://rebinding.test/x",
            |_url| Box::pin(async { Err::<std::net::SocketAddr, String>("refused".into()) }),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, WebRedirectError::HopBlocked(_)));
    }
}
