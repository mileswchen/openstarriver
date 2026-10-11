//! `starriverd` shared logic: single-instance control channel (Unix domain
//! socket / Windows named pipe) and the
//! `~/.starriver/daemon.json` info file.
//!
//! Single-instance semantics (design doc §6.2): on repeated startup, send
//! `ping` to the already-running instance and yield with its port/token
//! (`existing`); stale sockets left behind by crashes are reclaimed
//! automatically; the control channel also carries the `shutdown` command,
//! guaranteeing a single SQLite writer.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(windows)]
use std::sync::Mutex;
#[cfg(windows)]
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

pub const DAEMON_JSON: &str = "daemon.json";
pub const CONTROL_SOCKET: &str = "starriverd.sock";

/// Connection info exposed by the daemon (written to both a stdout line and
/// `daemon.json`; Electron may read the file first, falling back to spawn).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct DaemonInfo {
    pub port: u16,
    pub token: String,
    pub pid: u32,
    pub version: String,
    pub protocol_version: u32,
    pub started_at: u64,
}

impl DaemonInfo {
    /// stdout contract: a single JSON line read by Electron `child_process`.
    pub fn startup_line(&self, existing: bool) -> String {
        let mut value = serde_json::to_value(self).expect("DaemonInfo serializes");
        value["existing"] = serde_json::json!(existing);
        value.to_string()
    }
}

pub fn home_dir() -> PathBuf {
    // Windows processes have no HOME (USERPROFILE is the convention) — the
    // "." fallback scattered daemon.json into the launch CWD and never
    // matched the Electron client's %USERPROFILE%\.starriver read.
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn data_dir() -> PathBuf {
    home_dir().join(".starriver")
}

pub fn info_path(dir: &Path) -> PathBuf {
    dir.join(DAEMON_JSON)
}

pub fn socket_path(dir: &Path) -> PathBuf {
    dir.join(CONTROL_SOCKET)
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Write `daemon.json` (0600), creating the directory if missing.
pub fn write_info(dir: &Path, info: &DaemonInfo) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = info_path(dir);
    let json = serde_json::to_vec_pretty(info).map_err(std::io::Error::other)?;
    write_private(&path, &json)
}

/// Write with 0600 (permission bits on unix; plain write on other platforms).
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    #[cfg(unix)]
    let mut file = {
        use std::os::unix::fs::OpenOptionsExt;
        // Mode at CREATION: creating 0644 first and chmod'ing after the
        // write leaves the auth token world-readable in between.
        std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(path)?
    };
    #[cfg(not(unix))]
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)?;
    file.write_all(bytes)?;
    file.flush()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

pub fn read_info(dir: &Path) -> Option<DaemonInfo> {
    let bytes = std::fs::read(info_path(dir)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Result of acquiring the single instance.
pub enum Acquire {
    /// This process owns control (bound the control socket / lock file).
    Owned(ControlSocket),
    /// A daemon is already running; its connection info is returned.
    Existing(DaemonInfo),
}

/// Control channel: single-instance lock + `ping`/`shutdown` commands.
///
/// Unix uses a domain socket (0600, preferred); Windows uses a named pipe
/// (`\\.\pipe\starriverd-<user>`, `first_pipe_instance` ensures a single
/// instance); both share the same command-line format (JSON lines) and
/// `DaemonInfo`.
pub struct ControlSocket {
    shutdown: tokio_util::sync::CancellationToken,
    #[cfg(unix)]
    unix: UnixControl,
    #[cfg(windows)]
    win: WindowsControl,
}

#[cfg(unix)]
struct UnixControl {
    path: PathBuf,
    listener: std::os::unix::net::UnixListener,
}

#[cfg(windows)]
struct WindowsControl {
    pipe: String,
    server: Mutex<Option<NamedPipeServer>>,
}

impl ControlSocket {
    /// Try to acquire single-instance control; if an instance is already
    /// running, return its info.
    pub fn acquire(dir: &Path) -> std::io::Result<Acquire> {
        std::fs::create_dir_all(dir)?;
        #[cfg(unix)]
        {
            let path = socket_path(dir);
            if path.exists() {
                if let Some(info) = ping(&path) {
                    return Ok(Acquire::Existing(info));
                }
                // Stale socket: connect failed (no listener), reclaim and rebind.
                let _ = std::fs::remove_file(&path);
            }
            let listener = match std::os::unix::net::UnixListener::bind(&path) {
                Ok(listener) => listener,
                // Two simultaneous first-starts race past the exists() check;
                // the loser must yield like any other repeated start, not die
                // with a raw "address already in use".
                Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
                    if let Some(info) = ping(&path) {
                        return Ok(Acquire::Existing(info));
                    }
                    return Err(error);
                }
                Err(error) => return Err(error),
            };
            // tokio `from_std` requires the fd to be non-blocking, else it
            // panics on registration.
            listener.set_nonblocking(true)?;
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            Ok(Acquire::Owned(ControlSocket {
                shutdown: tokio_util::sync::CancellationToken::new(),
                unix: UnixControl { path, listener },
            }))
        }
        #[cfg(windows)]
        {
            let pipe = control_pipe_name();
            match ServerOptions::new().first_pipe_instance(true).create(&pipe) {
                Ok(server) => Ok(Acquire::Owned(ControlSocket {
                    shutdown: tokio_util::sync::CancellationToken::new(),
                    win: WindowsControl {
                        pipe,
                        server: Mutex::new(Some(server)),
                    },
                })),
                Err(_) => {
                    // Pipe already exists: another instance is running →
                    // ping for its info and yield.
                    if let Some(info) = ping_win(&pipe) {
                        return Ok(Acquire::Existing(info));
                    }
                    Err(std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        "control pipe in use but no daemon responded",
                    ))
                }
            }
        }
    }

    pub fn shutdown_token(&self) -> tokio_util::sync::CancellationToken {
        self.shutdown.clone()
    }

    /// Start the control-channel accept loop on the current tokio runtime; a
    /// `shutdown` command cancels the token, which the daemon uses to exit.
    /// The socket file is cleaned up on exit.
    pub fn serve(&self, info: DaemonInfo, runtime: tokio::runtime::Handle) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            let listener = self.unix.listener.try_clone()?;
            let path = self.unix.path.clone();
            let shutdown = self.shutdown.clone();
            runtime.spawn(async move {
                let listener = match tokio::net::UnixListener::from_std(listener) {
                    Ok(listener) => listener,
                    Err(error) => {
                        eprintln!("starriverd: control socket serve failed: {error}");
                        return;
                    }
                };
                loop {
                    tokio::select! {
                        _ = shutdown.cancelled() => break,
                        accepted = listener.accept() => {
                            let Ok((stream, _)) = accepted else { continue };
                            let info = info.clone();
                            let shutdown = shutdown.clone();
                            tokio::spawn(async move {
                                handle_control_connection(stream, info, shutdown).await;
                            });
                        }
                    }
                }
                let _ = std::fs::remove_file(&path);
            });
            Ok(())
        }
        #[cfg(windows)]
        {
            let Some(server) = self.win.server.lock().unwrap().take() else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "control pipe server already taken",
                ));
            };
            let shutdown = self.shutdown.clone();
            runtime.spawn(async move {
                serve_windows_pipe(server, info, shutdown).await;
            });
            Ok(())
        }
    }

    pub fn cleanup(&self) {
        #[cfg(unix)]
        let _ = std::fs::remove_file(&self.unix.path);
        // Windows: the named pipe is destroyed automatically when the process
        // exits; no cleanup needed.
        #[cfg(windows)]
        let _ = &self.win.pipe;
    }
}

/// Query the running instance for its connection info (single-instance yield
/// path).
#[cfg(unix)]
pub fn ping(path: &Path) -> Option<DaemonInfo> {
    control_request(path, "ping")
}

/// Ask the running instance to shut down gracefully; returns the stopped
/// instance's info.
#[cfg(unix)]
pub fn request_shutdown(path: &Path) -> std::io::Result<DaemonInfo> {
    control_request(path, "shutdown").ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no running starriverd on control socket",
        )
    })
}

/// Windows control-channel name (per-user isolation to avoid multi-user
/// conflicts).
#[cfg(windows)]
pub fn control_pipe_name() -> String {
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "default".into());
    format!(r"\\.\pipe\starriverd-{user}")
}

#[cfg(windows)]
pub fn ping_win(pipe: &str) -> Option<DaemonInfo> {
    control_request_win(pipe, "ping")
}

#[cfg(windows)]
pub fn request_shutdown_win(pipe: &str) -> std::io::Result<DaemonInfo> {
    control_request_win(pipe, "shutdown").ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no running starriverd on control pipe",
        )
    })
}

#[cfg(windows)]
fn control_request_win(pipe: &str, cmd: &str) -> Option<DaemonInfo> {
    // CLI actions (--shutdown / yielding on second startup) have no resident
    // runtime in main, so build a current_thread runtime in place to complete
    // a single request.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    rt.block_on(async move {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let mut stream = tokio::net::windows::named_pipe::ClientOptions::new()
            .open(pipe)
            .ok()?;
        let payload = serde_json::json!({ "cmd": cmd }).to_string();
        stream.write_all(payload.as_bytes()).await.ok()?;
        stream.write_all(b"\n").await.ok()?;
        stream.flush().await.ok()?;
        let mut line = String::new();
        BufReader::new(&mut stream)
            .read_line(&mut line)
            .await
            .ok()?;
        let value: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
        if value["ok"].as_bool() != Some(true) {
            return None;
        }
        serde_json::from_value(value["info"].clone()).ok()
    })
}

#[cfg(unix)]
fn control_request(path: &Path, cmd: &str) -> Option<DaemonInfo> {
    use std::os::unix::net::UnixStream;
    let mut stream = UnixStream::connect(path).ok()?;
    // A cold-starting first daemon accepts the connection (the listener
    // backlog exists) but answers only after HostRuntime::new finishes —
    // without a deadline a wedged first instance hangs every later start.
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .ok()?;
    let payload = serde_json::json!({ "cmd": cmd }).to_string();
    stream.write_all(payload.as_bytes()).ok()?;
    stream.write_all(b"\n").ok()?;
    stream.flush().ok()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).ok()?;
    let value: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    if value["ok"].as_bool() != Some(true) {
        return None;
    }
    serde_json::from_value(value["info"].clone()).ok()
}

/// Platform-independent control-command handling: `ping` returns connection
/// info, `shutdown` cancels the token and flags the response; returns one
/// JSON line (with newline) reused by both unix and windows sides.
fn handle_command_line(
    line: &str,
    info: &DaemonInfo,
    shutdown: &tokio_util::sync::CancellationToken,
) -> String {
    let cmd = serde_json::from_str::<serde_json::Value>(line.trim())
        .ok()
        .and_then(|v| v["cmd"].as_str().map(String::from))
        .unwrap_or_default();
    let mut response = serde_json::json!({ "ok": true, "info": info });
    if cmd == "shutdown" {
        response["shutdown"] = serde_json::json!(true);
        shutdown.cancel();
    }
    format!("{}\n", response)
}

#[cfg(unix)]
async fn handle_control_connection(
    mut stream: tokio::net::UnixStream,
    info: DaemonInfo,
    shutdown: tokio_util::sync::CancellationToken,
) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let mut line = String::new();
    let read = BufReader::new(&mut stream)
        .read_line(&mut line)
        .await
        .unwrap_or(0);
    if read == 0 {
        return;
    }
    let payload = handle_command_line(&line, &info, &shutdown);
    let _ = stream.write_all(payload.as_bytes()).await;
    let _ = stream.flush().await;
}

#[cfg(windows)]
async fn serve_windows_pipe(
    mut server: NamedPipeServer,
    info: DaemonInfo,
    shutdown: tokio_util::sync::CancellationToken,
) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            connected = server.connect() => {
                if connected.is_err() {
                    break;
                }
                let mut line = String::new();
                let read = {
                    let mut reader = BufReader::new(&mut server);
                    reader.read_line(&mut line).await.unwrap_or(0)
                };
                if read == 0 {
                    let _ = server.disconnect();
                    continue;
                }
                let payload = handle_command_line(&line, &info, &shutdown);
                let _ = server.write_all(payload.as_bytes()).await;
                let _ = server.flush().await;
                let _ = server.disconnect();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_info() -> DaemonInfo {
        DaemonInfo {
            port: 42000,
            token: "tok-123".into(),
            pid: 4242,
            version: "0.1.0".into(),
            protocol_version: 1,
            started_at: 123,
        }
    }

    #[test]
    fn info_file_roundtrip_is_0600() {
        let dir = tempfile::tempdir().unwrap();
        let info = sample_info();
        write_info(dir.path(), &info).unwrap();
        assert_eq!(read_info(dir.path()), Some(info.clone()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(info_path(dir.path()))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "daemon.json must be 0600");
        }
        assert!(std::fs::metadata(dir.path().join(DAEMON_JSON)).is_ok());
    }

    #[test]
    fn startup_line_marks_existing() {
        let line = sample_info().startup_line(false);
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["port"], 42000);
        assert_eq!(value["existing"], false);
        let line = sample_info().startup_line(true);
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["existing"], true);
    }

    #[test]
    fn command_line_ping_and_shutdown() {
        let info = sample_info();
        let token = tokio_util::sync::CancellationToken::new();

        let response = handle_command_line("{\"cmd\":\"ping\"}\n", &info, &token);
        let value: serde_json::Value = serde_json::from_str(response.trim()).unwrap();
        assert_eq!(value["ok"], true);
        assert_eq!(value["info"]["port"], 42000);
        assert!(!token.is_cancelled(), "ping must not cancel");

        let response = handle_command_line("{\"cmd\":\"shutdown\"}\n", &info, &token);
        let value: serde_json::Value = serde_json::from_str(response.trim()).unwrap();
        assert_eq!(value["ok"], true);
        assert_eq!(value["shutdown"], true);
        assert!(token.is_cancelled(), "shutdown must cancel the token");

        // Unknown command: ok, but leave the token untouched.
        let token2 = tokio_util::sync::CancellationToken::new();
        let response = handle_command_line("garbage\n", &info, &token2);
        let value: serde_json::Value = serde_json::from_str(response.trim()).unwrap();
        assert_eq!(value["ok"], true);
        assert!(!token2.is_cancelled());
    }

    #[cfg(unix)]
    #[test]
    fn stale_socket_is_recovered() {
        let dir = tempfile::tempdir().unwrap();
        let path = socket_path(dir.path());
        // Simulate a crash leftover: bind a socket with no listener.
        let _stale = std::os::unix::net::UnixListener::bind(&path).unwrap();
        drop(_stale);
        match ControlSocket::acquire(dir.path()).unwrap() {
            Acquire::Owned(socket) => {
                socket.cleanup();
            }
            Acquire::Existing(_) => panic!("stale socket must not report existing"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn second_acquire_returns_existing_info() {
        let dir = tempfile::tempdir().unwrap();
        let first = match ControlSocket::acquire(dir.path()).unwrap() {
            Acquire::Owned(socket) => socket,
            Acquire::Existing(_) => panic!("first acquire must own"),
        };
        // Must use multi_thread: the accept loop runs on a background worker
        // so the test thread can connect synchronously without starving the
        // event loop (current_thread would deadlock).
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        first
            .serve(sample_info(), runtime.handle().clone())
            .unwrap();
        // Wait for the accept loop to be ready.
        std::thread::sleep(std::time::Duration::from_millis(100));
        match ControlSocket::acquire(dir.path()).unwrap() {
            Acquire::Existing(info) => {
                assert_eq!(info, sample_info());
            }
            Acquire::Owned(_) => panic!("second acquire must report existing"),
        }
        first.shutdown_token().cancel();
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_command_cancels_token() {
        let dir = tempfile::tempdir().unwrap();
        let socket = match ControlSocket::acquire(dir.path()).unwrap() {
            Acquire::Owned(socket) => socket,
            Acquire::Existing(_) => panic!("first acquire must own"),
        };
        let token = socket.shutdown_token();
        let runtime = tokio::runtime::Handle::current();
        socket.serve(sample_info(), runtime).unwrap();
        let shutdown = request_shutdown(&socket_path(dir.path())).unwrap();
        assert_eq!(shutdown, sample_info());
        tokio::time::timeout(std::time::Duration::from_secs(3), token.cancelled())
            .await
            .expect("shutdown token must cancel");
        socket.cleanup();
    }

    #[cfg(unix)]
    #[test]
    fn ping_missing_socket_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(ping(&socket_path(dir.path())).is_none());
        assert!(request_shutdown(&socket_path(dir.path())).is_err());
    }
}
