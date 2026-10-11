//! Web search provider seam: normalized result types, the provider trait, and
//! the Exa / Perplexity / DeepSeek / HTML (DuckDuckGo + Bing) backends.

use std::sync::Arc;
use std::time::Duration;

use cordis_rslite::BoxFuture;

use super::web_common::{clean_html, get_following_same_origin, WebRedirectError};

/// Default upper bound on returned sources per tool call (`8`).
pub const DEFAULT_LIMIT: usize = 5;
/// Hard cap on returned sources per tool call.
pub const MAX_LIMIT: usize = 8;
/// Hard cap on queries per tool call (`WEB_SEARCH_MAX_QUERIES`).
pub const MAX_QUERIES: usize = 4;
/// Per-request HTTP timeout.
pub const SEARCH_TIMEOUT: Duration = Duration::from_secs(20);

const DDG_ENDPOINT: &str = "https://html.duckduckgo.com/html/";
const BING_ENDPOINT: &str = "https://www.bing.com/search";

/// Provider ids accepted by the model-facing `provider` argument (explicit
/// selection). Order mirrors the default attempt order.
pub const PROVIDER_IDS: &[&str] = &["exa", "perplexity", "deepseek", "html"];

/// One normalized search source, omitting absent optional fields.
#[derive(Debug, Clone, PartialEq)]
pub struct WebSearchSource {
    pub url: String,
    pub title: Option<String>,
    pub snippet: Option<String>,
    pub published_at: Option<String>,
}

/// Structured `sources[]` metadata for the canonical tool value: every entry
/// carries `url`; `title` / `snippet` / `published_at` appear only when
/// present, so downstream consumers get fielded data instead of parsing the
/// markdown rendering.
pub fn sources_to_json(sources: &[WebSearchSource]) -> serde_json::Value {
    serde_json::Value::Array(
        sources
            .iter()
            .map(|source| {
                let mut entry = serde_json::Map::new();
                entry.insert("url".into(), source.url.clone().into());
                if let Some(title) = source.title.as_deref().filter(|t| !t.is_empty()) {
                    entry.insert("title".into(), title.into());
                }
                if let Some(snippet) = source.snippet.as_deref().filter(|s| !s.is_empty()) {
                    entry.insert("snippet".into(), snippet.into());
                }
                if let Some(date) = source.published_at.as_deref().filter(|d| !d.is_empty()) {
                    entry.insert("published_at".into(), date.into());
                }
                serde_json::Value::Object(entry)
            })
            .collect(),
    )
}

/// A provider's search outcome: an optional generated answer plus sources.
#[derive(Debug, Clone, PartialEq)]
pub struct WebSearchResult {
    pub content: Option<String>,
    pub sources: Vec<WebSearchSource>,
    pub truncated: bool,
}

/// Provider seam: a backend is selected when `available()` is true.
pub trait WebSearchProvider: Send + Sync {
    fn id(&self) -> &'static str;
    fn available(&self) -> bool;
    fn search(&self, query: &str, limit: usize) -> BoxFuture<Result<WebSearchResult, String>>;
}

/// Resolve the model-facing arguments into a query list: `queries` wins over
/// the legacy `query` string; missing both is an error.
pub fn resolve_queries(json: &serde_json::Value) -> Result<Vec<String>, String> {
    if let Some(queries) = json.get("queries").and_then(|q| q.as_array()) {
        let mut out = Vec::with_capacity(queries.len());
        for query in queries {
            let text = query
                .as_str()
                .ok_or_else(|| "each query must be a string".to_string())?;
            out.push(text.to_string());
        }
        return parse_search_args(out);
    }
    if let Some(query) = json.get("query").and_then(|q| q.as_str()) {
        return parse_search_args(vec![query.to_string()]);
    }
    Err("missing query: pass queries[] or query".into())
}

/// Validate a query batch: non-empty, ≤ `MAX_QUERIES`, non-blank, deduped
/// (first occurrence order), matching `parseSearchArgs`.
pub fn parse_search_args(queries: Vec<String>) -> Result<Vec<String>, String> {
    if queries.is_empty() {
        return Err("queries must contain at least one query".into());
    }
    if queries.len() > MAX_QUERIES {
        return Err(format!(
            "queries must contain at most {MAX_QUERIES} queries"
        ));
    }
    let mut seen: Vec<String> = Vec::new();
    for query in queries {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return Err("each query must be a non-empty string".into());
        }
        if !seen.iter().any(|existing| existing == trimmed) {
            seen.push(trimmed.to_string());
        }
    }
    Ok(seen)
}

/// Merge per-query results in order, dedupe sources by URL (first wins), and
/// truncate to `limit` (marking `truncated` when the cap cut sources).
pub fn merge_batch(results: Vec<WebSearchResult>, limit: usize) -> WebSearchResult {
    let mut content = None;
    let mut sources = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut truncated = false;
    for result in results {
        if content.is_none() {
            content = result.content;
        }
        // The provider already cut at `limit` — that fact must survive the
        // merge or the tool reports truncated:false after an engine cut.
        truncated |= result.truncated;
        for source in result.sources {
            if !seen.insert(source.url.clone()) {
                continue;
            }
            if sources.len() >= limit {
                truncated = true;
            } else {
                sources.push(source);
            }
        }
    }
    WebSearchResult {
        content,
        sources,
        truncated,
    }
}

/// Display label for a source: its title, else its hostname, else the raw URL.
fn source_label(url: &str, title: Option<&str>) -> String {
    if let Some(title) = title.filter(|t| !t.is_empty()) {
        return title.to_string();
    }
    url.parse::<reqwest::Url>()
        .ok()
        .and_then(|parsed| parsed.host_str().map(String::from))
        .unwrap_or_else(|| url.to_string())
}

/// Render a search outcome as one model-facing text block (
/// `formatSearchOutput`): provider answer, markdown source list, truncation
/// note, and a standing cite-your-sources instruction.
pub fn format_search_output(result: &WebSearchResult) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(content) = result.content.as_deref().filter(|c| !c.is_empty()) {
        parts.push(content.to_string());
    }
    if !result.sources.is_empty() {
        let lines = result
            .sources
            .iter()
            .map(|source| {
                let label = source_label(&source.url, source.title.as_deref());
                let mut meta: Vec<String> = Vec::new();
                if let Some(snippet) = source.snippet.as_deref().filter(|s| !s.is_empty()) {
                    meta.push(snippet.to_string());
                }
                if let Some(date) = source.published_at.as_deref().filter(|d| !d.is_empty()) {
                    meta.push(format!("({date})"));
                }
                let suffix = if meta.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", meta.join(" "))
                };
                format!("- [{label}]({}){suffix}", source.url)
            })
            .collect::<Vec<_>>();
        parts.push(format!("Sources:\n{}", lines.join("\n")));
    } else if result.content.as_deref().is_none_or(|c| c.is_empty()) {
        parts.push("No results found.".into());
    }
    if result.truncated {
        parts.push(format!(
            "(Showing the first {} sources. Refine the query for more.)",
            result.sources.len()
        ));
    }
    parts.push("Cite the relevant URLs above as markdown links in your answer.".into());
    parts.join("\n\n")
}

fn non_empty(value: Option<&str>) -> Option<String> {
    value.filter(|v| !v.is_empty()).map(String::from)
}

/// Shared browser-like client used by the HTML engine (keeps the legacy
/// request headers so mock-server tests observe the same fingerprint).
fn search_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(SEARCH_TIMEOUT)
        .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36")
        .default_headers({
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(
                reqwest::header::ACCEPT_LANGUAGE,
                reqwest::header::HeaderValue::from_static("en-US,en;q=0.9,zh-CN;q=0.8"),
            );
            headers
        })
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| format!("client: {e}"))
}

// ── HTML provider (DuckDuckGo primary, Bing fallback) ──

fn percent_decode(s: &str) -> String {
    fn hex(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        if bytes[i] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// DuckDuckGo wraps result URLs in `/l/?uddg=<urlencoded>&...`.
fn unwrap_uddg(href: &str) -> String {
    // Only unwrap when the URL is actually a DDG redirect: the parameter
    // must be `uddg=` at a query-parameter boundary. A bare `a+b` href was
    // being form-decoded to `a b`, and `?q=uddg=evil` hijacked the check.
    // Decide AND extract on segment boundaries: `href.find("uddg=")`
    // matched inside other parameters' values (e.g. `q=auddg=FAKE`) and
    // returned the FAKE value while the real `uddg=` param existed later.
    for seg in href.split(['?', '&']) {
        if let Some(enc) = seg.strip_prefix("uddg=") {
            return percent_decode(enc);
        }
    }
    href.to_string()
}

fn parse_results(html: &str, limit: usize) -> Vec<(String, String, String)> {
    // Attribute order must not matter (BUG-103): match class and href in
    // EITHER order on the anchor.
    let re_link =
        regex_lite::Regex::new(r#"<a[^>]*?(?:class="result__a"[^>]*?href="([^"]+)"|href="([^"]+)"[^>]*?class="result__a")[^>]*>(.*?)</a>"#)
            .unwrap();
    let re_snip =
        regex_lite::Regex::new(r#"<a[^>]*class="result__snippet"[^>]*>(.*?)</a>"#).unwrap();
    // Pair each snippet with the result link that PRECEDES it in the same
    // block (BUG-103: positional i↔i pairing misattributed every snippet
    // after the first snippet-less result).
    let mut out: Vec<(String, String, String)> = Vec::with_capacity(limit);
    for cap in re_link.captures_iter(html).take(limit) {
        let url_raw = cap
            .get(1)
            .or_else(|| cap.get(2))
            .map(|m| m.as_str().to_string())
            .unwrap_or_default();
        let url = unwrap_uddg(&url_raw);
        let title = clean_html(&cap.get(3).map(|m| m.as_str()).unwrap_or(""));
        // The snippet for THIS result: the first result__snippet AFTER the
        // link's end and BEFORE the next result__a (or EOF).
        let link_end = cap.get(0).unwrap().end();
        let next_link_start = re_link
            .find_iter(html)
            .find(|m2| m2.start() >= link_end)
            .map(|m2| m2.start())
            .unwrap_or(html.len());
        let window = &html[link_end.min(html.len())..next_link_start];
        let snippet = re_snip
            .captures(window)
            .map(|s| clean_html(&s[1]))
            .unwrap_or_default();
        out.push((title, url, snippet));
    }
    out
}

/// Bing organic results live in `<li class="b_algo">…</li>` blocks.
fn parse_bing(html: &str, limit: usize) -> Vec<(String, String, String)> {
    let re_item = regex_lite::Regex::new(r#"<li class="b_algo".*?</li>"#).unwrap();
    let re_link =
        regex_lite::Regex::new(r#"<h2[^>]*><a[^>]*href="([^"]+)"[^>]*>(.*?)</a>"#).unwrap();
    let re_snip = regex_lite::Regex::new(r#"<p[^>]*>(.*?)</p>"#).unwrap();
    let mut out = Vec::with_capacity(limit);
    for cap in re_item.captures_iter(html) {
        if out.len() >= limit {
            break;
        }
        let block = &cap[0];
        let Some(link) = re_link.captures(block) else {
            continue;
        };
        let url = clean_html(&link[1]);
        let title = clean_html(&link[2]);
        if url.is_empty() || title.is_empty() {
            continue;
        }
        let snippet = re_snip
            .captures(block)
            .map(|s| clean_html(&s[1]))
            .unwrap_or_default();
        out.push((title, url, snippet));
    }
    out
}

async fn search_ddg(
    client: &reqwest::Client,
    endpoint: &str,
    query: &str,
    limit: usize,
) -> Result<Vec<(String, String, String)>, String> {
    let url = reqwest::Url::parse_with_params(endpoint, &[("q", query)])
        .map_err(|e| format!("invalid search endpoint: {e}"))?;
    let resp = follow_endpoint(client, url.as_str()).await?;
    if resp.status() != reqwest::StatusCode::OK {
        return Err(format!("search engine returned HTTP {}", resp.status()));
    }
    let html = resp.text().await.map_err(|e| format!("read body: {e}"))?;
    Ok(parse_results(&html, limit))
}

async fn search_bing(
    client: &reqwest::Client,
    endpoint: &str,
    query: &str,
    limit: usize,
) -> Result<Vec<(String, String, String)>, String> {
    let url = reqwest::Url::parse_with_params(endpoint, &[("q", query)])
        .map_err(|e| format!("invalid search endpoint: {e}"))?;
    let resp = follow_endpoint(client, url.as_str()).await?;
    if resp.status() != reqwest::StatusCode::OK {
        return Err(format!("bing returned HTTP {}", resp.status()));
    }
    let html = resp
        .text()
        .await
        .map_err(|e| format!("bing read body: {e}"))?;
    Ok(parse_bing(&html, limit))
}

/// GET the endpoint following only same-origin redirects (hardening parity
/// with web_fetch: a compromised/mocked endpoint cannot redirect the client
/// to an internal host).
async fn follow_endpoint(client: &reqwest::Client, url: &str) -> Result<reqwest::Response, String> {
    get_following_same_origin(client, url, |_url: &str| {
        Box::pin(async { Ok::<(), String>(()) })
    })
    .await
    .map_err(|e| match e {
        WebRedirectError::InvalidUrl(m)
        | WebRedirectError::HopBlocked(m)
        | WebRedirectError::RedirectBlocked(m)
        | WebRedirectError::RequestFailed(m) => m,
        WebRedirectError::TooManyRedirects => "search endpoint redirected too many times".into(),
    })
}

/// Try DuckDuckGo first; fall back to Bing when DDG errors or returns zero
/// results. A DDG error is surfaced when Bing also fails or comes back empty.
async fn search_with_fallback<D, B>(
    ddg: D,
    bing: B,
) -> Result<Vec<(String, String, String)>, String>
where
    D: std::future::Future<Output = Result<Vec<(String, String, String)>, String>>,
    B: std::future::Future<Output = Result<Vec<(String, String, String)>, String>>,
{
    match ddg.await {
        Ok(r) if !r.is_empty() => Ok(r),
        Ok(_) => Ok(bing.await.unwrap_or_default()),
        Err(e) => match bing.await {
            Ok(r) if !r.is_empty() => Ok(r),
            _ => Err(e),
        },
    }
}

/// Keyless HTML engine; always available.
pub struct HtmlSearchProvider {
    ddg_endpoint: String,
    bing_endpoint: String,
}

impl HtmlSearchProvider {
    pub fn new(ddg_endpoint: String, bing_endpoint: String) -> Self {
        Self {
            ddg_endpoint,
            bing_endpoint,
        }
    }
}

impl WebSearchProvider for HtmlSearchProvider {
    fn id(&self) -> &'static str {
        "html"
    }

    fn available(&self) -> bool {
        true
    }

    fn search(&self, query: &str, limit: usize) -> BoxFuture<Result<WebSearchResult, String>> {
        let ddg_endpoint = self.ddg_endpoint.clone();
        let bing_endpoint = self.bing_endpoint.clone();
        let query = query.to_string();
        Box::pin(async move {
            let client = search_client()?;
            let results = search_with_fallback(
                search_ddg(&client, &ddg_endpoint, &query, limit),
                search_bing(&client, &bing_endpoint, &query, limit),
            )
            .await?;
            let total = results.len();
            let sources = results
                .into_iter()
                .map(|(title, url, snippet)| WebSearchSource {
                    url,
                    title: Some(title),
                    snippet: Some(snippet),
                    published_at: None,
                })
                .collect();
            // The provider already cut at `limit` — report it so the
            // tool result's "truncated" reflects reality.
            Ok(WebSearchResult {
                content: None,
                sources,
                truncated: total >= limit,
            })
        })
    }
}

// ── Exa provider ──

/// Map an Exa `POST /search` response: first non-blank highlight becomes the
/// snippet; entries without one are dropped (`mapExaResult`).
pub fn map_exa_response(response: &serde_json::Value) -> WebSearchResult {
    let sources = response["results"]
        .as_array()
        .map(|results| {
            results
                .iter()
                .filter_map(|result| {
                    let url = result["url"].as_str()?.to_string();
                    let snippet = result["highlights"].as_array().and_then(|highlights| {
                        highlights
                            .iter()
                            .filter_map(|h| h.as_str())
                            .find(|h| !h.trim().is_empty())
                    });
                    snippet.map(|snippet| WebSearchSource {
                        url,
                        title: non_empty(result["title"].as_str()),
                        snippet: Some(snippet.to_string()),
                        published_at: non_empty(result["publishedDate"].as_str()),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    WebSearchResult {
        content: None,
        sources,
        truncated: false,
    }
}

/// Exa search backend (`POST {base}/search`, `x-api-key` header).
pub struct ExaSearchProvider {
    api_key: String,
    base_url: String,
    highlights_per_result: usize,
}

impl ExaSearchProvider {
    pub fn new(api_key: String, base_url: String, highlights_per_result: usize) -> Self {
        Self {
            api_key,
            base_url,
            highlights_per_result,
        }
    }
}

impl WebSearchProvider for ExaSearchProvider {
    fn id(&self) -> &'static str {
        "exa"
    }

    fn available(&self) -> bool {
        !self.api_key.is_empty()
    }

    fn search(&self, query: &str, limit: usize) -> BoxFuture<Result<WebSearchResult, String>> {
        let api_key = self.api_key.clone();
        let base_url = self.base_url.clone();
        let highlights_per_result = self.highlights_per_result;
        let query = query.to_string();
        Box::pin(async move {
            let client = reqwest::Client::builder()
                .timeout(SEARCH_TIMEOUT)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|e| format!("client: {e}"))?;
            let response = client
                .post(format!("{base_url}/search"))
                .header("x-api-key", api_key)
                .json(&serde_json::json!({
                    "query": query,
                    "type": "auto",
                    "numResults": limit,
                    "highlightsPerUrl": highlights_per_result,
                }))
                .send()
                .await
                .map_err(|e| format!("exa request failed: {e}"))?;
            if !response.status().is_success() {
                return Err(format!("exa returned HTTP {}", response.status()));
            }
            let payload: serde_json::Value = response
                .json()
                .await
                .map_err(|e| format!("exa bad JSON: {e}"))?;
            Ok(map_exa_response(&payload))
        })
    }
}

// ── Perplexity provider ──

/// Map a Perplexity chat-completions response: answer → content, structured
/// `search_results[]` → sources (falling back to URL-only `citations[]`).
pub fn map_perplexity_response(response: &serde_json::Value) -> WebSearchResult {
    let content = non_empty(response["choices"][0]["message"]["content"].as_str());
    let sources =
        if let Some(search_results) = response.get("search_results").and_then(|v| v.as_array()) {
            search_results
                .iter()
                .filter_map(|result| {
                    let url = result["url"].as_str()?;
                    Some(WebSearchSource {
                        url: url.to_string(),
                        title: non_empty(result["title"].as_str()),
                        snippet: non_empty(result["snippet"].as_str()),
                        published_at: non_empty(result["date"].as_str()),
                    })
                })
                .collect()
        } else {
            response
                .get("citations")
                .and_then(|v| v.as_array())
                .map(|citations| {
                    citations
                        .iter()
                        .filter_map(|url| url.as_str())
                        .map(|url| WebSearchSource {
                            url: url.to_string(),
                            title: None,
                            snippet: None,
                            published_at: None,
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
    WebSearchResult {
        content,
        sources,
        truncated: false,
    }
}

/// Perplexity search backend over its OpenAI-compatible chat-completions
/// endpoint (`POST {base}/chat/completions`, bearer auth).
pub struct PerplexitySearchProvider {
    api_key: String,
    base_url: String,
}

impl PerplexitySearchProvider {
    pub fn new(api_key: String, base_url: String) -> Self {
        Self { api_key, base_url }
    }
}

impl WebSearchProvider for PerplexitySearchProvider {
    fn id(&self) -> &'static str {
        "perplexity"
    }

    fn available(&self) -> bool {
        !self.api_key.is_empty()
    }

    fn search(&self, query: &str, _limit: usize) -> BoxFuture<Result<WebSearchResult, String>> {
        let api_key = self.api_key.clone();
        let base_url = self.base_url.clone();
        let query = query.to_string();
        Box::pin(async move {
            let client = reqwest::Client::builder()
                .timeout(SEARCH_TIMEOUT)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|e| format!("client: {e}"))?;
            let response = client
                .post(format!("{base_url}/chat/completions"))
                .bearer_auth(api_key)
                .json(&serde_json::json!({
                    "model": "sonar",
                    "messages": [{"role": "user", "content": query}],
                    "max_tokens": 1024,
                }))
                .send()
                .await
                .map_err(|e| format!("perplexity request failed: {e}"))?;
            if !response.status().is_success() {
                return Err(format!("perplexity returned HTTP {}", response.status()));
            }
            let payload: serde_json::Value = response
                .json()
                .await
                .map_err(|e| format!("perplexity bad JSON: {e}"))?;
            Ok(map_perplexity_response(&payload))
        })
    }
}

// ── DeepSeek provider ──

/// Map a DeepSeek Anthropic-format Messages response: `web_search_tool_result`
/// blocks become sources (URL-deduped), text-block `citations[]` become
/// snippets (`mapAnthropicResponse`).
pub fn map_anthropic_response(response: &serde_json::Value) -> Result<WebSearchResult, String> {
    let blocks = response["content"]
        .as_array()
        .ok_or_else(|| "deepseek response has no content array".to_string())?;
    let mut snippets = std::collections::HashMap::new();
    for block in blocks {
        if block["type"].as_str() != Some("text") {
            continue;
        }
        if let Some(citations) = block.get("citations").and_then(|c| c.as_array()) {
            for citation in citations {
                let url = citation["url"].as_str().unwrap_or("");
                let text = citation["cited_text"].as_str().unwrap_or("");
                if !url.is_empty() && !text.is_empty() {
                    snippets
                        .entry(url.to_string())
                        .or_insert_with(|| text.to_string());
                }
            }
        }
    }
    let mut sources = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for block in blocks {
        if block["type"].as_str() != Some("web_search_tool_result") {
            continue;
        }
        if let Some(items) = block.get("content").and_then(|c| c.as_array()) {
            for item in items {
                if item["type"].as_str() != Some("web_search_result") {
                    continue;
                }
                let url = item["url"].as_str().unwrap_or("");
                if url.is_empty() || !seen.insert(url.to_string()) {
                    continue;
                }
                sources.push(WebSearchSource {
                    url: url.to_string(),
                    title: non_empty(item["title"].as_str()),
                    snippet: snippets.get(url).cloned(),
                    published_at: non_empty(item["page_age"].as_str()),
                });
            }
        }
    }
    if sources.is_empty() {
        return Err(
            "DeepSeek returned no web_search_tool_result blocks; the request may not have triggered native web search"
                .into(),
        );
    }
    Ok(WebSearchResult {
        content: None,
        sources,
        truncated: false,
    })
}

/// DeepSeek search backend over its Anthropic-compatible Messages endpoint
/// (`POST {base}/messages` with the native `web_search_20250305` tool).
pub struct DeepSeekSearchProvider {
    api_key: String,
    base_url: String,
    model: String,
}

impl DeepSeekSearchProvider {
    pub fn new(api_key: String, base_url: String, model: String) -> Self {
        Self {
            api_key,
            base_url,
            model,
        }
    }
}

impl WebSearchProvider for DeepSeekSearchProvider {
    fn id(&self) -> &'static str {
        "deepseek"
    }

    fn available(&self) -> bool {
        !self.api_key.is_empty()
    }

    fn search(&self, query: &str, _limit: usize) -> BoxFuture<Result<WebSearchResult, String>> {
        let api_key = self.api_key.clone();
        let base_url = self.base_url.clone();
        let model = self.model.clone();
        let query = query.to_string();
        Box::pin(async move {
            let client = reqwest::Client::builder()
                .timeout(SEARCH_TIMEOUT)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|e| format!("client: {e}"))?;
            let response = client
                .post(format!("{base_url}/messages"))
                .header("x-api-key", api_key)
                .header("anthropic-version", "2023-06-01")
                .json(&serde_json::json!({
                    "model": model,
                    "max_tokens": 4096,
                    "messages": [{
                        "role": "user",
                        "content": [{"type": "text", "text": query}]
                    }],
                    "tools": [{
                        "type": "web_search_20250305",
                        "name": "web_search",
                        "max_uses": 5,
                    }],
                }))
                .send()
                .await
                .map_err(|e| format!("deepseek request failed: {e}"))?;
            if !response.status().is_success() {
                return Err(format!("deepseek returned HTTP {}", response.status()));
            }
            let payload: serde_json::Value = response
                .json()
                .await
                .map_err(|e| format!("deepseek bad JSON: {e}"))?;
            map_anthropic_response(&payload)
        })
    }
}

/// Build the default provider chain from environment credentials, ending with
/// the always-available HTML engine.
pub fn providers_from_env() -> Vec<Arc<dyn WebSearchProvider>> {
    let mut providers: Vec<Arc<dyn WebSearchProvider>> = Vec::new();
    if let Some(api_key) = std::env::var("EXA_API_KEY")
        .ok()
        .filter(|key| !key.is_empty())
    {
        let base_url =
            std::env::var("EXA_BASE_URL").unwrap_or_else(|_| "https://api.exa.ai".to_string());
        providers.push(Arc::new(ExaSearchProvider::new(api_key, base_url, 1)));
    }
    if let Some(api_key) = std::env::var("PERPLEXITY_API_KEY")
        .ok()
        .filter(|key| !key.is_empty())
    {
        let base_url = std::env::var("PERPLEXITY_BASE_URL")
            .unwrap_or_else(|_| "https://api.perplexity.ai".to_string());
        providers.push(Arc::new(PerplexitySearchProvider::new(api_key, base_url)));
    }
    if let Some(api_key) = std::env::var("DEEPSEEK_API_KEY")
        .ok()
        .filter(|key| !key.is_empty())
    {
        let base_url = std::env::var("DEEPSEEK_BASE_URL")
            .unwrap_or_else(|_| "https://api.deepseek.com/anthropic/v1".to_string());
        let model =
            std::env::var("DEEPSEEK_MODEL").unwrap_or_else(|_| "deepseek-v4-flash".to_string());
        providers.push(Arc::new(DeepSeekSearchProvider::new(
            api_key, base_url, model,
        )));
    }
    providers.push(Arc::new(HtmlSearchProvider::new(
        DDG_ENDPOINT.into(),
        BING_ENDPOINT.into(),
    )));
    providers
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(url: &str) -> WebSearchSource {
        WebSearchSource {
            url: url.into(),
            title: None,
            snippet: None,
            published_at: None,
        }
    }

    #[test]
    fn resolve_queries_prefers_batch_and_falls_back_to_single() {
        let batch = serde_json::json!({"queries": ["a", "b"]});
        assert_eq!(
            resolve_queries(&batch).unwrap(),
            vec!["a".to_string(), "b".to_string()]
        );
        let single = serde_json::json!({"query": "x"});
        assert_eq!(resolve_queries(&single).unwrap(), vec!["x".to_string()]);
        assert!(resolve_queries(&serde_json::json!({})).is_err());
    }

    #[test]
    fn parse_search_args_rejects_over_max_and_blank_and_dedupes() {
        assert_eq!(
            parse_search_args(vec!["a".into(), "b".into(), "a".into()]).unwrap(),
            vec!["a".to_string(), "b".to_string()]
        );
        assert!(parse_search_args(vec![]).is_err());
        assert!(parse_search_args(vec![
            "1".into(),
            "2".into(),
            "3".into(),
            "4".into(),
            "5".into()
        ])
        .is_err());
        assert!(parse_search_args(vec![" ".into()]).is_err());
    }

    #[test]
    fn merge_batch_dedupes_urls_preserving_first_and_truncates() {
        let a = WebSearchResult {
            content: None,
            sources: vec![src("https://a"), src("https://dup")],
            truncated: false,
        };
        let b = WebSearchResult {
            content: Some("answer".into()),
            sources: vec![src("https://dup"), src("https://b")],
            truncated: false,
        };
        let merged = merge_batch(vec![a, b], 2);
        assert_eq!(merged.sources.len(), 2);
        assert_eq!(merged.sources[0].url, "https://a");
        assert_eq!(merged.sources[1].url, "https://dup");
        assert!(merged.truncated);
        assert_eq!(merged.content.as_deref(), Some("answer"));
    }

    #[test]
    fn format_search_output_renders_markdown_sources() {
        let result = WebSearchResult {
            content: None,
            sources: vec![WebSearchSource {
                url: "https://rust-lang.org".into(),
                title: Some("Rust".into()),
                snippet: Some("A language".into()),
                published_at: Some("2026-01-01".into()),
            }],
            truncated: false,
        };
        let out = format_search_output(&result);
        assert!(out.contains("- [Rust](https://rust-lang.org) — A language (2026-01-01)"));
        assert!(out.contains("Cite the relevant URLs"));
    }

    #[test]
    fn format_search_output_no_results() {
        let out = format_search_output(&WebSearchResult {
            content: None,
            sources: vec![],
            truncated: false,
        });
        assert!(out.contains("No results found."));
    }

    #[test]
    fn exa_maps_results_and_drops_snippet_less() {
        let payload = serde_json::json!({
            "results": [
                {"url": "https://a", "title": "A", "highlights": ["snippet a"], "publishedDate": "2026-02-02"},
                {"url": "https://b", "title": "B", "highlights": []},
                {"url": "https://c", "title": "C", "highlights": ["  "], "publishedDate": "2026-03-03"},
            ]
        });
        let result = map_exa_response(&payload);
        assert_eq!(result.sources.len(), 1);
        assert_eq!(result.sources[0].url, "https://a");
        assert_eq!(result.sources[0].snippet.as_deref(), Some("snippet a"));
        assert_eq!(
            result.sources[0].published_at.as_deref(),
            Some("2026-02-02")
        );
        assert_eq!(result.content, None);
    }

    #[test]
    fn exa_available_only_with_key() {
        assert!(!ExaSearchProvider::new(String::new(), "https://api.exa.ai".into(), 1).available());
        assert!(ExaSearchProvider::new("k".into(), "https://api.exa.ai".into(), 1).available());
    }

    #[test]
    fn perplexity_maps_answer_and_search_results() {
        let payload = serde_json::json!({
            "choices": [{"message": {"content": "answer text"}}],
            "search_results": [
                {"url": "https://a", "title": "A", "snippet": "snip", "date": "2026-01-01"},
            ],
            "citations": ["https://ignored"]
        });
        let result = map_perplexity_response(&payload);
        assert_eq!(result.content.as_deref(), Some("answer text"));
        assert_eq!(result.sources.len(), 1);
        assert_eq!(result.sources[0].snippet.as_deref(), Some("snip"));
    }

    #[test]
    fn perplexity_falls_back_to_citations() {
        let payload = serde_json::json!({
            "choices": [{"message": {"content": ""}}],
            "citations": ["https://c"]
        });
        let result = map_perplexity_response(&payload);
        assert_eq!(result.content, None);
        assert_eq!(
            result.sources,
            vec![WebSearchSource {
                url: "https://c".into(),
                title: None,
                snippet: None,
                published_at: None,
            }]
        );
    }

    #[test]
    fn deepseek_maps_anthropic_blocks_and_citation_snippets() {
        let payload = serde_json::json!({
            "content": [
                {"type": "text", "text": "see sources", "citations": [
                    {"url": "https://a", "cited_text": "snippet a"}
                ]},
                {"type": "web_search_tool_result", "content": [
                    {"type": "web_search_result", "url": "https://a", "title": "A", "page_age": "2026-01-01"},
                    {"type": "web_search_result", "url": "https://b", "title": "B"}
                ]}
            ]
        });
        let result = map_anthropic_response(&payload).unwrap();
        assert_eq!(result.sources.len(), 2);
        assert_eq!(result.sources[0].snippet.as_deref(), Some("snippet a"));
        assert_eq!(
            result.sources[0].published_at.as_deref(),
            Some("2026-01-01")
        );
        assert_eq!(result.content, None);
    }

    #[test]
    fn deepseek_mapping_rejects_missing_result_blocks() {
        let payload = serde_json::json!({"content": [{"type": "text", "text": "no search"}]});
        assert!(map_anthropic_response(&payload).is_err());
    }

    #[test]
    fn html_provider_always_available() {
        let provider = HtmlSearchProvider::new("http://ddg.test".into(), "http://bing.test".into());
        assert!(provider.available());
        assert_eq!(provider.id(), "html");
    }

    #[test]
    fn unwraps_uddg_links() {
        assert_eq!(
            unwrap_uddg("//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fx&rut=1"),
            "https://example.com/x"
        );
    }

    #[test]
    fn sources_to_json_fields_present_and_absent_omitted() {
        let sources = vec![
            WebSearchSource {
                url: "https://full".into(),
                title: Some("Full".into()),
                snippet: Some("snip".into()),
                published_at: Some("2026-09-09".into()),
            },
            WebSearchSource {
                url: "https://bare".into(),
                title: None,
                snippet: Some("".into()),
                published_at: None,
            },
        ];
        let json = sources_to_json(&sources);
        let entries = json.as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["url"].as_str().unwrap(), "https://full");
        assert_eq!(entries[0]["title"].as_str().unwrap(), "Full");
        assert_eq!(entries[0]["snippet"].as_str().unwrap(), "snip");
        assert_eq!(entries[0]["published_at"].as_str().unwrap(), "2026-09-09");
        assert_eq!(entries[1]["url"].as_str().unwrap(), "https://bare");
        assert!(entries[1].get("title").is_none());
        assert!(entries[1].get("snippet").is_none(), "empty snippet omitted");
        assert!(entries[1].get("published_at").is_none());
    }

    #[test]
    fn provider_ids_cover_every_backend() {
        assert_eq!(PROVIDER_IDS, &["exa", "perplexity", "deepseek", "html"]);
    }

    #[test]
    fn decodes_percent_encoding() {
        assert_eq!(percent_decode("hello%20world"), "hello world");
        assert_eq!(percent_decode("a%2Bb"), "a+b");
    }

    #[test]
    fn parses_results_from_fixture() {
        let html = r#"
<a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Frust-lang.org&rut=1">Rust</a>
<a class="result__snippet" href="...">A safe &amp; fast language</a>
"#;
        let results = parse_results(html, 5);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "Rust");
        assert_eq!(results[0].1, "https://rust-lang.org");
        assert_eq!(results[0].2, "A safe & fast language");
    }

    #[test]
    fn parses_bing_blocks() {
        let html = r#"<li class="b_algo"><h2><a href="https://rust-lang.org">Rust</a></h2><p>Systems language</p></li>"#;
        let results = parse_bing(html, 5);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "Rust");
        assert_eq!(results[0].2, "Systems language");
    }

    #[tokio::test]
    async fn fallback_prefers_ddg_and_surfaces_error() {
        use std::future::ready;
        let ok: Result<Vec<(String, String, String)>, String> =
            Ok(vec![("t".into(), "u".into(), "s".into())]);
        let empty = Ok(vec![]);
        let err: Result<Vec<(String, String, String)>, String> = Err("ddg down".into());
        let bing_ok: Result<Vec<(String, String, String)>, String> =
            Ok(vec![("b".into(), "u2".into(), "s2".into())]);
        assert_eq!(
            search_with_fallback(ready(ok), ready(err.clone()))
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            search_with_fallback(ready(empty), ready(bing_ok.clone()))
                .await
                .unwrap()[0]
                .0,
            "b"
        );
        assert!(search_with_fallback(ready(err.clone()), ready(err.clone()))
            .await
            .is_err());
    }
}
