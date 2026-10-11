//! `starriverd` — Starriver headless daemon（FPM P4b）。
//!
//! Responsibilities (design doc §6.2): `HostRuntime` + loopback HTTP/SSE
//! protocol server + single-instance control; stdout prints one line of
//! `{"port":…,"token":…,"pid":…,"existing":…}` and writes
//! `~/.starriver/daemon.json` (0600). Electron acts as launcher + client;
//! exiting it does not kill the daemon (shared service).
//!
//! Usage:
//! - `starriverd` to start (without `--config`, `~/.starriver` is the data dir)
//! - `starriverd --config <path>` explicit config (session db, etc.)
//! - `starriverd --shutdown` gracefully stop the running instance
//! - `starriverd --help` / `--version`

use std::path::PathBuf;
use std::process::ExitCode;

use starriver_protocol_server::{http_server, protocol_catalog};
use starriverd::{self, data_dir, info_path, now_unix, Acquire, ControlSocket, DaemonInfo};

const VERSION: &str = env!("CARGO_PKG_VERSION");

struct Args {
    config: Option<PathBuf>,
    shutdown: bool,
    help: bool,
    version: bool,
    /// `ip:port` for the protocol server (LAN/VPN access for remote clients).
    bind: Option<String>,
    /// Fixed bearer token (stable across restarts so paired phones survive).
    token: Option<String>,
}

fn parse_args() -> Args {
    let mut args = Args {
        config: None,
        shutdown: false,
        help: false,
        version: false,
        bind: None,
        token: None,
    };
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--config" | "-c" => args.config = iter.next().map(PathBuf::from),
            "--shutdown" | "shutdown" => args.shutdown = true,
            "--bind" => args.bind = iter.next(),
            "--token" => args.token = iter.next(),
            "--help" | "-h" => args.help = true,
            "--version" | "-V" => args.version = true,
            other => {
                eprintln!("starriverd: unknown argument `{other}` (see --help)");
                args.help = true;
            }
        }
    }
    args
}

fn print_help() {
    println!(
        "starriverd {VERSION} — Starriver headless daemon\n\
         \n\
         USAGE:\n\
           starriverd [--config <path>]\n\
           starriverd --shutdown\n\
         \n\
         OPTIONS:\n\
           -c, --config <path>   RuntimeConfig YAML（缺省时以 ~/.starriver 为数据目录）\n\
               --bind <ip:port>  协议服务器监听地址（默认 127.0.0.1:0；局域网/VPN
\
                                 接入用 0.0.0.0:<port>，也可用 STARRIVER_BIND）
\
               --token <token>   固定 API token（重启不变，便于手机配对；
\
                                 也可用 STARRIVER_TOKEN；缺省持久化到 daemon.json）
\
               --shutdown        优雅关停已运行实例\n\
           -h, --help            print help\n\
           -V, --version         print version"
    );
}

fn main() -> ExitCode {
    let args = parse_args();
    if args.help {
        print_help();
        return ExitCode::SUCCESS;
    }
    if args.version {
        println!("starriverd {VERSION}");
        return ExitCode::SUCCESS;
    }
    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("starriverd: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Args) -> anyhow::Result<()> {
    let dir = data_dir();
    if args.shutdown {
        return run_shutdown(&dir);
    }

    // Single instance: if a daemon is already running → yield its connection
    // info; otherwise bind the control socket.
    let socket = match ControlSocket::acquire(&dir)? {
        Acquire::Existing(info) => {
            println!("{}", info.startup_line(true));
            return Ok(());
        }
        Acquire::Owned(socket) => socket,
    };

    // Without an explicit config, use ~/.starriver as the data dir (session db
    // etc. land in one place, aligned with the relative-path behavior of the
    // desktop embedded mode).
    if args.config.is_none() && std::env::current_dir().map(|c| c != dir).unwrap_or(false) {
        std::env::set_current_dir(&dir)?;
    }

    let host = std::sync::Arc::new(
        starriver_host::HostRuntime::new(
            args.config
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
        )
        .map_err(|e| anyhow::anyhow!("host runtime: {e}"))?,
    );
    let runtime = host.runtime_handle().clone();
    let dispatch = protocol_catalog::dispatcher(protocol_catalog::build(), host);
    // Bind/token resolution: explicit args > env > previous daemon.json token
    // (restart-stable pairing) > fresh random. Bind defaults to loopback.
    let bind = args
        .bind
        .or_else(|| std::env::var("STARRIVER_BIND").ok())
        .unwrap_or_else(|| "127.0.0.1:0".to_string());
    let token = args
        .token
        .or_else(|| std::env::var("STARRIVER_TOKEN").ok())
        .or_else(|| starriverd::read_info(&dir).map(|info| info.token));
    let server = runtime.block_on(http_server::spawn_with(
        dispatch,
        runtime.clone(),
        http_server::SpawnOptions {
            bind: Some(bind),
            token,
            ..http_server::SpawnOptions::default()
        },
    ))?;

    let info = DaemonInfo {
        port: server.port(),
        token: server.token().to_string(),
        pid: std::process::id(),
        version: VERSION.into(),
        protocol_version: starriver_protocol::PROTOCOL_VERSION,
        started_at: now_unix(),
    };
    starriverd::write_info(&dir, &info)?;

    socket.serve(info.clone(), runtime.clone())?;
    let shutdown = socket.shutdown_token();

    // SIGINT / SIGTERM / control-channel shutdown command → graceful exit.
    let signal_shutdown = shutdown.clone();
    runtime.spawn(async move {
        #[cfg(unix)]
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = async {
                #[cfg(unix)]
                {
                    // A failed signal() registration (None) must park like
                    // the non-unix arm — returning instantly made select!
                    // fire this branch and the fresh daemon cancelled its
                    // own shutdown token and exited for no reason.
                    match &mut sigterm {
                        Some(sigterm) => {
                            sigterm.recv().await;
                        }
                        None => std::future::pending::<()>().await,
                    }
                }
                #[cfg(not(unix))]
                {
                    std::future::pending::<()>().await;
                }
            } => {}
        }
        signal_shutdown.cancel();
    });

    println!("{}", info.startup_line(false));
    // The stdout line must land before any subsequent logs: Electron reads the
    // first line to obtain connection info.
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
    eprintln!(
        "starriverd: listening on http://127.0.0.1:{} (pid {})",
        info.port, info.pid
    );

    runtime.block_on(async move { shutdown.cancelled().await });

    server.shutdown();
    socket.cleanup();
    let _ = std::fs::remove_file(info_path(&dir));
    eprintln!("starriverd: shutdown complete");
    Ok(())
}

fn run_shutdown(dir: &std::path::Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        let info = starriverd::request_shutdown(&starriverd::socket_path(dir))
            .map_err(|e| anyhow::anyhow!("shutdown: {e}"))?;
        println!(
            "{}",
            serde_json::json!({ "ok": true, "shutdown": true, "pid": info.pid }).to_string()
        );
        Ok(())
    }
    #[cfg(windows)]
    {
        let info = starriverd::request_shutdown_win(&starriverd::control_pipe_name())
            .map_err(|e| anyhow::anyhow!("shutdown: {e}"))?;
        println!(
            "{}",
            serde_json::json!({ "ok": true, "shutdown": true, "pid": info.pid }).to_string()
        );
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        Err(anyhow::anyhow!(
            "--shutdown is not supported on this platform"
        ))
    }
}
