//! First-party browser automation service (terminal-service parity for
//! pages): owner-scoped, CDP-driven Chromium sessions that survive across
//! model tool calls.
//!
//! Lifecycle mirrors `TerminalService`: [`BrowserService::start`] spawns a
//! browser subprocess (`--headless=new` by default) with a per-session
//! profile dir under `~/.starriver/browser/<id>`, handshakes CDP, and parks
//! the session in a registry; every action bumps `last_used`; an idle
//! reaper (default 15 min) and host shutdown ([`BrowserService::kill_all`])
//! reap processes and profile dirs.
//!
//! The model-facing grounding model is snapshot-ref (Playwright aria
//! snapshot / chrome-devtools-mcp style): [`scripts::snapshot_js`] walks the
//! DOM, assigns stable `eN` refs to interactive elements, and
//! [`axtree::render_snapshot`] renders the item list as text; click/type
//! resolve refs against the in-page map, with a stale-ref error steering
//! the model back to a fresh snapshot after navigation.

pub mod axtree;
pub mod discover;
pub mod scripts;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::page::CaptureScreenshotFormat;
use chromiumoxide::handler::Handler;
use chromiumoxide::page::ScreenshotParams;
use futures::StreamExt;
use thiserror::Error;

/// Default idle reaping budget: a session untouched for this long is closed
/// (headless browsers leak fast — profile dirs, sockets, processes).
pub const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 15 * 60;
/// Concurrent session cap: browsers are heavyweight; the model should close
/// sessions it no longer needs (browser_list surfaces them).
pub const DEFAULT_MAX_SESSIONS: usize = 4;
/// Default viewport (a common laptop page area; tweak per call).
pub const DEFAULT_VIEWPORT: (u32, u32) = (1280, 800);

#[derive(Debug, Error)]
pub enum BrowserError {
    #[error("no browser binary: {0}")]
    NoBrowser(String),
    #[error("browser launch failed: {0}")]
    Launch(String),
    #[error("too many browser sessions ({current}/{max}); close one with browser_close first")]
    TooMany { current: usize, max: usize },
    #[error("session not found: {0}")]
    SessionNotFound(String),
    #[error("invalid url: {0}")]
    InvalidUrl(String),
    #[error("navigation failed: {0}")]
    Navigate(String),
    #[error("snapshot failed: {0}")]
    Snapshot(String),
    #[error("action failed: {0}")]
    Action(String),
    #[error("stale element ref: {0}")]
    StaleRef(String),
    #[error("eval failed: {0}")]
    Eval(String),
    #[error("screenshot failed: {0}")]
    Screenshot(String),
    #[error("io: {0}")]
    Io(String),
}

impl BrowserError {
    /// Stable machine-facing code (the host tool layer maps these to
    /// ToolError codes).
    pub fn code(&self) -> &'static str {
        match self {
            BrowserError::NoBrowser(_) => "NO_BROWSER",
            BrowserError::Launch(_) => "LAUNCH_FAILED",
            BrowserError::TooMany { .. } => "TOO_MANY_SESSIONS",
            BrowserError::SessionNotFound(_) => "SESSION_NOT_FOUND",
            BrowserError::InvalidUrl(_) => "INVALID_URL",
            BrowserError::Navigate(_) => "NAVIGATE_FAILED",
            BrowserError::Snapshot(_) => "SNAPSHOT_FAILED",
            BrowserError::Action(_) => "ACTION_FAILED",
            BrowserError::StaleRef(_) => "STALE_REF",
            BrowserError::Eval(_) => "EVAL_FAILED",
            BrowserError::Screenshot(_) => "SCREENSHOT_FAILED",
            BrowserError::Io(_) => "IO_FAILED",
        }
    }
}

/// Service-level defaults, folded from settings at construction.
#[derive(Debug, Clone)]
pub struct BrowserServiceConfig {
    pub browser_path: Option<String>,
    pub headless: bool,
    pub viewport: (u32, u32),
    pub idle_timeout_secs: u64,
    pub max_sessions: usize,
    pub profile_root: PathBuf,
}

impl Default for BrowserServiceConfig {
    fn default() -> Self {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        Self {
            browser_path: None,
            headless: true,
            viewport: DEFAULT_VIEWPORT,
            idle_timeout_secs: DEFAULT_IDLE_TIMEOUT_SECS,
            max_sessions: DEFAULT_MAX_SESSIONS,
            profile_root: PathBuf::from(home).join(".starriver").join("browser"),
        }
    }
}

/// Per-call start options (tool parameters over service defaults).
#[derive(Debug, Clone, Default)]
pub struct StartOptions {
    pub owner: Option<String>,
    pub browser_path: Option<String>,
    pub headless: Option<bool>,
    pub viewport: Option<(u32, u32)>,
}

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub id: String,
    pub owner: Option<String>,
    pub headless: bool,
    pub browser_path: String,
    pub url: String,
    pub title: String,
    pub age_secs: u64,
    pub idle_secs: u64,
}

#[derive(Debug)]
struct Session {
    id: String,
    owner: Option<String>,
    headless: bool,
    browser_path: PathBuf,
    profile_dir: PathBuf,
    /// Dropping the (last) Browser handle kills the child process; the Arc
    /// keeps it alive exactly as long as the session lives in the registry.
    _browser: Browser,
    /// Page guard: serializes actions on the page (tool-level exclusivity
    /// is fail-closed, but sessions can also be driven from the desktop).
    page: tokio::sync::Mutex<chromiumoxide::Page>,
    _pump: tokio::task::JoinHandle<()>,
    created_at: Instant,
    last_used: Mutex<Instant>,
    last_url: Mutex<String>,
    last_title: Mutex<String>,
}

impl Session {
    fn touch(&self) {
        if let Ok(mut t) = self.last_used.lock() {
            *t = Instant::now();
        }
    }

    fn note_nav(&self, url: &str, title: &str) {
        if let Ok(mut u) = self.last_url.lock() {
            u.clear();
            u.push_str(url);
        }
        if let Ok(mut t) = self.last_title.lock() {
            t.clear();
            t.push_str(title);
        }
    }

    fn info(&self) -> SessionInfo {
        let now = Instant::now();
        let idle = self
            .last_used
            .lock()
            .map(|t| now.duration_since(*t).as_secs())
            .unwrap_or(0);
        SessionInfo {
            id: self.id.clone(),
            owner: self.owner.clone(),
            headless: self.headless,
            browser_path: self.browser_path.to_string_lossy().into_owned(),
            url: self.last_url.lock().map(|s| s.clone()).unwrap_or_default(),
            title: self
                .last_title
                .lock()
                .map(|s| s.clone())
                .unwrap_or_default(),
            age_secs: now.duration_since(self.created_at).as_secs(),
            idle_secs: idle,
        }
    }
}

/// One page state round trip (used for navigation settle + list freshness).
#[derive(serde::Deserialize)]
struct PageState {
    url: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    ready: String,
}

/// Session registry + idle reaper + process reaping.
pub struct BrowserService {
    sessions: Arc<Mutex<HashMap<String, Arc<Session>>>>,
    config: BrowserServiceConfig,
    reaper_spawned: AtomicBool,
}

impl Default for BrowserService {
    fn default() -> Self {
        Self::new(BrowserServiceConfig::default())
    }
}

fn new_session_id() -> String {
    let uuid = uuid::Uuid::new_v4().simple().to_string();
    format!("br-{}", &uuid[..12])
}

/// Pump the chromiumoxide handler until the browser connection ends.
/// HandlerEvent errors break the loop; the browser drop kills the child.
async fn pump_handler(mut handler: Handler) {
    while let Some(event) = handler.next().await {
        if event.is_err() {
            break;
        }
    }
}

/// Evaluate a JS expression that returns a JSON string, and parse it.
async fn eval_json<T: serde::de::DeserializeOwned>(
    page: &chromiumoxide::Page,
    js: &str,
) -> Result<T, BrowserError> {
    let raw = page
        .evaluate(js)
        .await
        .map_err(|e| BrowserError::Eval(e.to_string()))?
        .into_value::<String>()
        .map_err(|e| BrowserError::Eval(format!("eval did not return a string: {e}")))?;
    serde_json::from_str(&raw).map_err(|e| BrowserError::Eval(format!("page result parse: {e}")))
}

/// Run a click/type action script and normalize its `{ok,error}` envelope.
async fn run_action(
    page: &chromiumoxide::Page,
    js: &str,
    action: &str,
) -> Result<(), BrowserError> {
    #[derive(serde::Deserialize)]
    struct Outcome {
        ok: bool,
        #[serde(default)]
        error: Option<String>,
    }
    let outcome: Outcome = eval_json(page, js)
        .await
        .map_err(|e| BrowserError::Action(format!("{action}: {e}")))?;
    if !outcome.ok {
        let error = outcome.error.unwrap_or_else(|| "unknown".into());
        if error == "stale_ref" {
            return Err(BrowserError::StaleRef(format!(
                "{action}: ref no longer resolves (page changed) — take a fresh browser_snapshot"
            )));
        }
        return Err(BrowserError::Action(format!("{action}: {error}")));
    }
    Ok(())
}

/// Truncate a rendered string on a char boundary, appending an ellipsis
/// note when cut (eval outputs; snapshots truncate inside the renderer).
fn cap_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}

pub struct NavOutcome {
    pub final_url: String,
    pub title: String,
    pub status: Option<u16>,
}

impl BrowserService {
    pub fn new(config: BrowserServiceConfig) -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            config,
            reaper_spawned: AtomicBool::new(false),
        }
    }

    fn session(&self, id: &str) -> Result<Arc<Session>, BrowserError> {
        self.sessions
            .lock()
            .expect("browser sessions map poisoned")
            .get(id)
            .cloned()
            .ok_or_else(|| BrowserError::SessionNotFound(id.to_string()))
    }

    fn spawn_reaper_once(&self) {
        if self.reaper_spawned.swap(true, Ordering::SeqCst) {
            return;
        }
        let sessions = Arc::clone(&self.sessions);
        let idle_secs = self.config.idle_timeout_secs;
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            loop {
                tick.tick().await;
                let reaped: Vec<Arc<Session>> = {
                    let mut map = sessions.lock().expect("browser sessions map poisoned");
                    let now = Instant::now();
                    let expired: Vec<String> = map
                        .iter()
                        .filter(|(_, s)| {
                            s.last_used
                                .lock()
                                .map(|t| now.duration_since(*t).as_secs() > idle_secs)
                                .unwrap_or(false)
                        })
                        .map(|(id, _)| id.clone())
                        .collect();
                    expired.iter().filter_map(|id| map.remove(id)).collect()
                };
                for session in reaped {
                    tracing::info!(session = %session.id, "idle browser session reaped");
                    reap_session(session).await;
                }
            }
        });
    }

    /// Launch a browser and register the session.
    pub async fn start(&self, opts: StartOptions) -> Result<SessionInfo, BrowserError> {
        self.spawn_reaper_once();
        let headless = opts.headless.unwrap_or(self.config.headless);
        let viewport = opts.viewport.unwrap_or(self.config.viewport);
        {
            let map = self.sessions.lock().expect("browser sessions map poisoned");
            if map.len() >= self.config.max_sessions {
                return Err(BrowserError::TooMany {
                    current: map.len(),
                    max: self.config.max_sessions,
                });
            }
        }
        let browser_path = discover::discover_browser(
            opts.browser_path
                .as_deref()
                .or(self.config.browser_path.as_deref()),
        )
        .map_err(BrowserError::NoBrowser)?;

        let id = new_session_id();
        let profile_dir = self.config.profile_root.join(&id);
        std::fs::create_dir_all(&profile_dir)
            .map_err(|e| BrowserError::Io(format!("creating {}: {e}", profile_dir.display())))?;

        let mut builder = BrowserConfig::builder()
            .chrome_executable(browser_path.clone())
            .user_data_dir(profile_dir.clone())
            .arg(format!("--window-size={},{}", viewport.0, viewport.1))
            .arg("--disable-gpu".to_string())
            .arg("--disable-extensions".to_string())
            .arg("--disable-component-update".to_string())
            .arg("--disable-background-networking".to_string())
            .arg("--disable-sync".to_string())
            .arg("--no-first-run".to_string())
            .arg("--no-default-browser-check".to_string())
            .arg("--mute-audio".to_string())
            .arg("--disable-dev-shm-usage".to_string());
        if !headless {
            builder = builder.with_head();
        }
        let config = builder
            .build()
            .map_err(|e| BrowserError::Launch(format!("browser config: {e}")))?;
        let (browser, handler) = Browser::launch(config)
            .await
            .map_err(|e| BrowserError::Launch(e.to_string()))?;
        let pump = tokio::spawn(pump_handler(handler));
        let page = browser
            .new_page("about:blank")
            .await
            .map_err(|e| BrowserError::Launch(format!("initial page: {e}")))?;

        let session = Arc::new(Session {
            id: id.clone(),
            owner: opts.owner,
            headless,
            browser_path,
            profile_dir,
            _browser: browser,
            page: tokio::sync::Mutex::new(page),
            _pump: pump,
            created_at: Instant::now(),
            last_used: Mutex::new(Instant::now()),
            last_url: Mutex::new("about:blank".into()),
            last_title: Mutex::new(String::new()),
        });
        self.sessions
            .lock()
            .expect("browser sessions map poisoned")
            .insert(id, session.clone());
        Ok(session.info())
    }

    /// Navigate to an http(s) URL and settle until `load` (or the budget).
    pub async fn navigate(
        &self,
        id: &str,
        url: &str,
        wait_ms: u64,
    ) -> Result<NavOutcome, BrowserError> {
        // Validate arguments before touching session state: bad input
        // never leaks which sessions exist.
        let url = axtree::validate_navigate_url(url).map_err(BrowserError::InvalidUrl)?;
        let session = self.session(id)?;
        session.touch();
        let page = session.page.lock().await;
        let status: Option<u16> = None; // 0.9 goto returns the page, not the response
        page.goto(url)
            .await
            .map_err(|e| BrowserError::Navigate(e.to_string()))?;
        // Settle: poll readyState to `complete` inside the wait budget
        // (1s floor keeps trivial pages from racing; 60s ceiling bounds the
        // tool's own timeout).
        let wait_ms = wait_ms.clamp(1_000, 60_000);
        let deadline = Instant::now() + Duration::from_millis(wait_ms);
        let mut state: Option<PageState> = None;
        while Instant::now() < deadline {
            let candidate = eval_json::<PageState>(&page, scripts::page_state_js()).await;
            match candidate {
                Ok(s) if s.ready == "complete" => {
                    state = Some(s);
                    break;
                }
                Ok(s) => state = Some(s),
                Err(_) => {}
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        let state = match state {
            Some(s) => s,
            None => eval_json::<PageState>(&page, scripts::page_state_js()).await?,
        };
        session.note_nav(&state.url, &state.title);
        Ok(NavOutcome {
            final_url: state.url,
            title: state.title,
            status,
        })
    }

    /// Take a fresh ref-annotated accessibility-lite snapshot.
    pub async fn snapshot(&self, id: &str, max_chars: usize) -> Result<String, BrowserError> {
        let session = self.session(id)?;
        session.touch();
        let page = session.page.lock().await;
        let payload: serde_json::Value = eval_json(&page, scripts::snapshot_js())
            .await
            .map_err(|e| BrowserError::Snapshot(e.to_string()))?;
        let rendered = axtree::render_snapshot(&payload, max_chars);
        session.note_nav(
            payload.get("url").and_then(|v| v.as_str()).unwrap_or(""),
            payload.get("title").and_then(|v| v.as_str()).unwrap_or(""),
        );
        Ok(rendered)
    }

    pub async fn click(&self, id: &str, r#ref: &str) -> Result<(), BrowserError> {
        validate_ref(r#ref)?;
        let session = self.session(id)?;
        session.touch();
        let page = session.page.lock().await;
        run_action(&page, &scripts::click_js(r#ref), "click").await
    }

    pub async fn type_text(
        &self,
        id: &str,
        r#ref: &str,
        text: &str,
        submit: bool,
    ) -> Result<(), BrowserError> {
        validate_ref(r#ref)?;
        let session = self.session(id)?;
        session.touch();
        let page = session.page.lock().await;
        run_action(&page, &scripts::type_js(r#ref, text, submit), "type").await
    }

    pub async fn eval(
        &self,
        id: &str,
        expression: &str,
        max_chars: usize,
    ) -> Result<String, BrowserError> {
        let session = self.session(id)?;
        session.touch();
        let page = session.page.lock().await;
        #[derive(serde::Deserialize)]
        struct Outcome {
            ok: bool,
            #[serde(default)]
            value: Option<serde_json::Value>,
            #[serde(default)]
            error: Option<String>,
        }
        let outcome: Outcome = eval_json(&page, &scripts::eval_js(expression))
            .await
            .map_err(|e| BrowserError::Eval(e.to_string()))?;
        if !outcome.ok {
            return Err(BrowserError::Eval(
                outcome.error.unwrap_or_else(|| "unknown error".into()),
            ));
        }
        let value = outcome
            .value
            .map(|v| v.to_string())
            .unwrap_or_else(|| "null".into());
        Ok(cap_chars(&value, max_chars))
    }

    /// PNG screenshot bytes of the current page.
    pub async fn screenshot(&self, id: &str, full_page: bool) -> Result<Vec<u8>, BrowserError> {
        let session = self.session(id)?;
        session.touch();
        let page = session.page.lock().await;
        let params = ScreenshotParams::builder()
            .format(CaptureScreenshotFormat::Png)
            .capture_beyond_viewport(full_page)
            .build();
        page.screenshot(params)
            .await
            .map_err(|e| BrowserError::Screenshot(e.to_string()))
    }

    pub fn list(&self) -> Vec<SessionInfo> {
        self.sessions
            .lock()
            .expect("browser sessions map poisoned")
            .values()
            .map(|s| s.info())
            .collect()
    }

    /// Close one session: kill the child, stop the pump, clean the profile.
    pub async fn close(&self, id: &str) -> Result<(), BrowserError> {
        let session = self
            .sessions
            .lock()
            .expect("browser sessions map poisoned")
            .remove(id)
            .ok_or_else(|| BrowserError::SessionNotFound(id.to_string()))?;
        reap_session(session).await;
        Ok(())
    }

    /// Close every session (host shutdown parity with terminal kill_all).
    pub async fn kill_all(&self) -> usize {
        let drained: Vec<Arc<Session>> = {
            let mut map = self.sessions.lock().expect("browser sessions map poisoned");
            map.drain().map(|(_, s)| s).collect()
        };
        let count = drained.len();
        if count > 0 {
            let reaps: Vec<_> = drained.into_iter().map(reap_session).collect();
            futures::future::join_all(reaps).await;
        }
        count
    }
}

/// Refs come only from our snapshot walker (`e` + counter); reject anything
/// else before it reaches the page (defense in depth on top of JSON
/// escaping).
fn validate_ref(r#ref: &str) -> Result<(), BrowserError> {
    let body = r#ref.strip_prefix('e').ok_or_else(|| {
        BrowserError::Action(format!(
            "invalid ref `{ref}` — use eN from browser_snapshot"
        ))
    })?;
    if body.is_empty() || !body.bytes().all(|b| b.is_ascii_digit()) || body.len() > 5 {
        return Err(BrowserError::Action(format!(
            "invalid ref `{ref}` — use eN from browser_snapshot"
        )));
    }
    Ok(())
}

/// Session teardown shared by close/kill_all/reaper: abort the pump, drop
/// the browser (kills the child), then best-effort remove the profile dir
/// after the process has had a beat to release its locks.
async fn reap_session(session: Arc<Session>) {
    session._pump.abort();
    // Capture the dir before the session is dropped; force the page guard
    // open first so in-flight ops finish before teardown.
    let profile_dir = session.profile_dir.clone();
    {
        let _page = session.page.lock().await;
    }
    drop(session);
    // Chromium needs a moment to release SingletonLock etc.
    tokio::time::sleep(Duration::from_millis(150)).await;
    if let Err(e) = std::fs::remove_dir_all(&profile_dir) {
        if profile_dir.is_dir() {
            tracing::debug!(
                dir = %profile_dir.display(),
                error = %e,
                "browser profile cleanup skipped (still locked)"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ref_validation_only_accepts_walker_shaped_refs() {
        assert!(validate_ref("e1").is_ok());
        assert!(validate_ref("e42").is_ok());
        assert!(validate_ref("e99999").is_ok());
        assert!(validate_ref("e").is_err());
        assert!(validate_ref("e0x1").is_err());
        assert!(validate_ref("E1").is_err());
        assert!(validate_ref("evil; drop").is_err());
        assert!(validate_ref("e999999").is_err(), "5 digits max");
    }

    #[test]
    fn cap_chars_truncates_on_char_boundary() {
        assert_eq!(cap_chars("hello", 10), "hello");
        assert_eq!(cap_chars("你好世界你好世界", 5), "你好世界…");
    }

    #[test]
    fn session_ids_are_prefixed_and_bounded() {
        let id = new_session_id();
        assert!(id.starts_with("br-"), "{id}");
        assert_eq!(id.len(), 3 + 12);
        assert!(id != new_session_id());
    }

    #[tokio::test]
    async fn unknown_session_operations_fail_with_stable_code() {
        let service = BrowserService::default();
        let err = service.snapshot("missing", 8000).await.unwrap_err();
        assert_eq!(err.code(), "SESSION_NOT_FOUND");
        let err = service.close("missing").await.unwrap_err();
        assert_eq!(err.code(), "SESSION_NOT_FOUND");
    }

    #[tokio::test]
    async fn start_with_missing_browser_fails_actionably() {
        let mut config = BrowserServiceConfig::default();
        config.profile_root = test_scratch_dir("start-missing-browser");
        let service = BrowserService::new(config);
        let err = service
            .start(StartOptions {
                owner: None,
                browser_path: Some("/nonexistent-browser-xyz".into()),
                headless: Some(true),
                viewport: None,
            })
            .await
            .unwrap_err();
        assert_eq!(err.code(), "NO_BROWSER");
        assert!(err.to_string().contains("/nonexistent-browser-xyz"));
        assert!(service.list().is_empty());
    }

    /// Scratch dir for tests that must never touch the real home. Honors
    /// `STARRIVER_TEST_TMP` (write-restricted sandboxes), falls back to the
    /// platform temp dir; unique per test name so parallel runs don't
    /// collide.
    #[cfg(test)]
    fn test_scratch_dir(name: &str) -> PathBuf {
        let base = std::env::var("STARRIVER_TEST_TMP")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir());
        let dir = base.join(format!("starriver-browser-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("test scratch dir");
        dir
    }
}
