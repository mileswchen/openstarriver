//! MCP connection supervision: periodic liveness pings and automatic
//! restart with exponential backoff.
//!
//! One monitor per server pins the same `Arc<Mutex<Connection>>` that the
//! server's tool executors share. When a ping fails, the monitor rebuilds the
//! connection (`reconnect_bridge`: relaunch + handshake + `tools/list`) and
//! swaps it in place, so every existing executor transparently moves to the
//! new connection without re-registration. The re-discovered definitions are
//! handed to a caller hook (host wiring re-registers them into the kernel,
//! because the tool list may have changed across the restart). Giving up
//! after `max_attempts` marks the server dead and emits an
//! `mcp/server_health` event for the UI.

use std::sync::Arc;
use std::time::Duration;

use cordis_rslite::{BoxFuture, EventBus};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use super::adapter::TokenProvider;
use super::config::McpServerConfig;
use super::definition::{discover_bridge_shared, reconnect_bridge};
use super::transport::Connection;
use starriver_kernel::tools::ToolDefinition;

cordis_rslite::cordis_event!(McpServerHealth, "mcp/server_health", {
    server: String,
    // "restarting" | "dead" | "recovered"
    status: String,
    detail: String,
});

/// Tunables for ping cadence and restart backoff. Tests use aggressive
/// values; production takes [`Default`].
#[derive(Debug, Clone)]
pub struct RestartPolicy {
    /// Idle time between liveness pings.
    pub ping_interval: Duration,
    /// A ping slower than this counts as failed.
    pub ping_timeout: Duration,
    /// Backoff before the second restart attempt; doubles per attempt.
    pub initial_backoff: Duration,
    /// Backoff ceiling.
    pub max_backoff: Duration,
    /// Failed restart attempts after which the server is marked dead and
    /// supervision stops.
    pub max_attempts: usize,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            ping_interval: Duration::from_secs(30),
            ping_timeout: Duration::from_secs(5),
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(60),
            max_attempts: 5,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthState {
    /// Supervising; last ping answered (or connection not yet launched).
    Healthy,
    /// Pings failing; inside the restart backoff loop.
    Restarting,
    /// Restart attempts exhausted; supervision has stopped.
    Dead,
}

/// Async callback receiving the re-discovered definitions after a successful
/// restart (host wiring re-registers them into the kernel tool runtime).
pub type RecoveryHook = Arc<dyn Fn(Vec<ToolDefinition>) -> BoxFuture<()> + Send + Sync>;

/// Async callback fired once when supervision gives up (`Dead`) with the
/// give-up detail. The host marks the server unavailable and hands over to
/// long-horizon discovery retry.
pub type DeadHook = Arc<dyn Fn(String) -> BoxFuture<()> + Send + Sync>;

/// Async callback receiving the first successful discovery's definitions
/// plus the shared connection (host wiring registers the definitions and
/// starts a health monitor on the connection).
pub type DiscoverySuccessHook =
    Arc<dyn Fn(Vec<ToolDefinition>, Arc<Mutex<Connection>>) -> BoxFuture<()> + Send + Sync>;

/// Supervises one MCP server's shared connection. Build with [`Self::new`]
/// plus builder methods, then [`Self::start`] (or [`Self::start_on`] from a
/// non-async context holding a runtime handle).
pub struct McpHealthMonitor {
    server: String,
    config: McpServerConfig,
    token_provider: TokenProvider,
    connection: Arc<Mutex<Connection>>,
    policy: RestartPolicy,
    bus: Option<EventBus>,
    on_recovery: Option<RecoveryHook>,
    on_dead: Option<DeadHook>,
    state: Arc<std::sync::Mutex<HealthState>>,
}

impl McpHealthMonitor {
    pub fn new(
        server: String,
        config: McpServerConfig,
        token_provider: TokenProvider,
        connection: Arc<Mutex<Connection>>,
    ) -> Self {
        Self {
            server,
            config,
            token_provider,
            connection,
            policy: RestartPolicy::default(),
            bus: None,
            on_recovery: None,
            on_dead: None,
            state: Arc::new(std::sync::Mutex::new(HealthState::Healthy)),
        }
    }

    pub fn with_policy(mut self, policy: RestartPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Emit `mcp/server_health` transitions on this bus (optional).
    pub fn with_event_bus(mut self, bus: EventBus) -> Self {
        self.bus = Some(bus);
        self
    }

    pub fn on_recovery(mut self, hook: RecoveryHook) -> Self {
        self.on_recovery = Some(hook);
        self
    }

    /// Fire once when restart attempts are exhausted (WI-163: the host then
    /// marks the server unavailable and falls back to long-horizon
    /// discovery retry).
    pub fn on_dead(mut self, hook: DeadHook) -> Self {
        self.on_dead = Some(hook);
        self
    }

    pub fn state(&self) -> HealthState {
        *self.state.lock().unwrap()
    }

    /// Spawn the supervision loop on the current runtime.
    pub fn start(self) -> McpHealthHandle {
        Self::start_on(self, &tokio::runtime::Handle::current())
    }

    /// Spawn the supervision loop on an explicit runtime handle (usable from
    /// synchronous host wiring code).
    pub fn start_on(self, handle: &tokio::runtime::Handle) -> McpHealthHandle {
        let cancel = CancellationToken::new();
        let state = Arc::clone(&self.state);
        let server = self.server.clone();
        let task_cancel = cancel.clone();
        let task_state = Arc::clone(&state);
        handle.spawn(async move {
            self.run(task_cancel).await;
            tracing::info!(server = %server, state = ?task_state.lock().unwrap(), "mcp health monitor stopped");
        });
        McpHealthHandle { cancel, state }
    }

    fn emit(&self, status: &str, detail: String) {
        tracing::warn!(
            server = %self.server, status, detail = %detail,
            "mcp server health transition"
        );
        if let Some(bus) = &self.bus {
            bus.emit(Arc::new(McpServerHealth {
                server: self.server.clone(),
                status: status.to_string(),
                detail,
            }));
        }
    }

    async fn ping_ok(&self) -> bool {
        // The LOCK acquisition is inside the timeout: a connection held for
        // a long tool call would otherwise stall the supervisor forever
        // (lock wait had no deadline at all).
        let mut conn = match tokio::time::timeout(
            self.policy.ping_timeout,
            self.connection.lock(),
        )
        .await
        {
            Ok(conn) => conn,
            Err(_) => {
                tracing::debug!(server = %self.server, "mcp ping could not acquire connection lock");
                return false;
            }
        };
        // A connection nobody has used yet (stdio lazily unlaunched) is
        // healthy by definition; pinging it would spawn a process just to
        // kill-check it.
        if !conn.is_launched() {
            return true;
        }
        match tokio::time::timeout(
            self.policy.ping_timeout,
            conn.send_request("ping", serde_json::json!({})),
        )
        .await
        {
            Ok(Ok(_)) => true,
            Ok(Err(error)) => {
                tracing::debug!(server = %self.server, error = %error, "mcp ping failed");
                false
            }
            Err(_) => false,
        }
    }

    async fn run(self, cancel: CancellationToken) {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(self.policy.ping_interval) => {}
            }
            if self.ping_ok().await {
                continue;
            }
            *self.state.lock().unwrap() = HealthState::Restarting;
            self.emit("restarting", "ping failed; restarting connection".into());
            match self.restart_loop(&cancel).await {
                RestartOutcome::Recovered => {
                    *self.state.lock().unwrap() = HealthState::Healthy;
                }
                RestartOutcome::Dead { detail } => {
                    *self.state.lock().unwrap() = HealthState::Dead;
                    if let Some(hook) = &self.on_dead {
                        hook(detail).await;
                    }
                    return;
                }
                RestartOutcome::Cancelled => return,
            }
        }
    }

    async fn restart_loop(&self, cancel: &CancellationToken) -> RestartOutcome {
        let mut backoff = self.policy.initial_backoff;
        let mut attempt = 0usize;
        loop {
            attempt += 1;
            match reconnect_bridge(
                &self.server,
                &self.config,
                Arc::clone(&self.token_provider),
                &self.connection,
            )
            .await
            {
                Ok(definitions) => {
                    self.emit("recovered", format!("restarted after {attempt} attempt(s)"));
                    if let Some(hook) = &self.on_recovery {
                        hook(definitions).await;
                    }
                    return RestartOutcome::Recovered;
                }
                Err(error) if attempt >= self.policy.max_attempts => {
                    let detail = format!("giving up after {attempt} attempts: {error}");
                    self.emit("dead", detail.clone());
                    return RestartOutcome::Dead { detail };
                }
                Err(error) => {
                    tracing::debug!(
                        server = %self.server, attempt, error = %error,
                        "mcp restart attempt failed; backing off"
                    );
                }
            }
            tokio::select! {
                _ = cancel.cancelled() => return RestartOutcome::Cancelled,
                _ = tokio::time::sleep(backoff) => {}
            }
            backoff = backoff.saturating_mul(2).min(self.policy.max_backoff);
        }
    }
}

enum RestartOutcome {
    Recovered,
    Dead {
        detail: String,
    },
    Cancelled,
}

/// Stops the supervision task on demand (`stop()`); dropping the handle
/// detaches the task (monitor runs for the process lifetime). `state()`
/// reports the supervised server's health even after start.
pub struct McpHealthHandle {
    cancel: CancellationToken,
    state: Arc<std::sync::Mutex<HealthState>>,
}

impl McpHealthHandle {
    pub fn stop(&self) {
        self.cancel.cancel();
    }

    pub fn state(&self) -> HealthState {
        *self.state.lock().unwrap()
    }
}

/// Long-horizon discovery supervision (WI-163): retry a discovery that
/// already failed, with exponential backoff capped at `max_backoff`,
/// forever — a server that was down at process start (or whose health
/// monitor gave up) may come back hours later, and without this the server
/// stayed unregistered for the whole process lifetime. On success the
/// `on_success` hook takes the definitions and the shared connection (host
/// wiring registers them and starts a health monitor). Unlike
/// [`McpHealthMonitor`] there is no attempt cap: one HTTP attempt per
/// interval is cheap, and giving up reintroduces the silent-capability-loss
/// bug this exists to fix.
pub struct McpDiscoveryRetry {
    server: String,
    config: McpServerConfig,
    token_provider: TokenProvider,
    policy: RestartPolicy,
    bus: Option<EventBus>,
    on_success: Option<DiscoverySuccessHook>,
    state: Arc<std::sync::Mutex<HealthState>>,
}

impl McpDiscoveryRetry {
    pub fn new(
        server: String,
        config: McpServerConfig,
        token_provider: TokenProvider,
    ) -> Self {
        Self {
            server,
            config,
            token_provider,
            policy: RestartPolicy::default(),
            bus: None,
            on_success: None,
            state: Arc::new(std::sync::Mutex::new(HealthState::Restarting)),
        }
    }

    /// Backoff schedule: only `initial_backoff`/`max_backoff` apply.
    pub fn with_policy(mut self, policy: RestartPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Emit `mcp/server_health` transitions (`retrying` → `recovered`) on
    /// this bus (optional).
    pub fn with_event_bus(mut self, bus: EventBus) -> Self {
        self.bus = Some(bus);
        self
    }

    pub fn on_success(mut self, hook: DiscoverySuccessHook) -> Self {
        self.on_success = Some(hook);
        self
    }

    pub fn state(&self) -> HealthState {
        *self.state.lock().unwrap()
    }

    /// Spawn the retry loop on an explicit runtime handle. The handle's
    /// `state()` is `Restarting` until the first successful discovery flips
    /// it to `Healthy`.
    pub fn start_on(self, handle: &tokio::runtime::Handle) -> McpHealthHandle {
        let cancel = CancellationToken::new();
        let state = Arc::clone(&self.state);
        let server = self.server.clone();
        let task_cancel = cancel.clone();
        let task_state = Arc::clone(&state);
        handle.spawn(async move {
            self.run(task_cancel).await;
            tracing::info!(server = %server, state = ?task_state.lock().unwrap(), "mcp discovery retry stopped");
        });
        McpHealthHandle { cancel, state }
    }

    fn emit(&self, status: &str, detail: String) {
        tracing::warn!(
            server = %self.server, status, detail = %detail,
            "mcp discovery retry transition"
        );
        if let Some(bus) = &self.bus {
            bus.emit(Arc::new(McpServerHealth {
                server: self.server.clone(),
                status: status.to_string(),
                detail,
            }));
        }
    }

    async fn run(self, cancel: CancellationToken) {
        let mut backoff = self.policy.initial_backoff;
        let mut attempt = 0usize;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(backoff) => {}
            }
            attempt += 1;
            match discover_bridge_shared(
                &self.server,
                &self.config,
                Arc::clone(&self.token_provider),
            )
            .await
            {
                Ok((definitions, connection)) => {
                    *self.state.lock().unwrap() = HealthState::Healthy;
                    self.emit(
                        "recovered",
                        format!("discovered after {attempt} retry attempt(s)"),
                    );
                    if let Some(hook) = &self.on_success {
                        hook(definitions, connection).await;
                    }
                    return;
                }
                Err(error) => {
                    *self.state.lock().unwrap() = HealthState::Restarting;
                    // Transition + a heartbeat every 10 attempts: enough to
                    // see liveness in the log/bus without per-attempt spam.
                    if attempt == 1 || attempt.is_multiple_of(10) {
                        self.emit("retrying", format!("attempt {attempt}: {error}"));
                    } else {
                        tracing::debug!(
                            server = %self.server, attempt, error = %error,
                            "mcp discovery retry failed; backing off"
                        );
                    }
                }
            }
            backoff = backoff.saturating_mul(2).min(self.policy.max_backoff);
        }
    }
}
