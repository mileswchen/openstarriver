//! `starriver-acp` — ACP (Agent Client Protocol) server binary (WI-70).
//!
//! Editors and scripts spawn this process and drive starriver over
//! newline-delimited JSON-RPC on stdio (stdout is protocol-only; all logs
//! go to stderr). One process = one host = one agent workspace, matching
//! the ACP per-project process model.
//!
//! Usage:
//! - `starriver-acp` (reads `~/.starriver` as the data dir)
//! - `starriver-acp --config <path>` explicit settings file
//! - `starriver-acp --version` / `--help`

use std::process::ExitCode;
use std::sync::Arc;

use starriver_acp::{AcpServer, StdioRead, StdioWrite};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> ExitCode {
    let mut config: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" | "-c" => config = args.next(),
            "--version" | "-V" => {
                println!("starriver-acp {VERSION}");
                return ExitCode::SUCCESS;
            }
            "--help" | "-h" => {
                println!("starriver-acp {VERSION} — ACP server (JSON-RPC over stdio)");
                println!();
                println!("Usage: starriver-acp [--config <path>]");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("unknown argument: {other}");
                return ExitCode::from(2);
            }
        }
    }

    let host = match starriver_host::HostRuntime::new(config) {
        Ok(host) => host,
        Err(error) => {
            eprintln!("starriver-acp: host runtime failed to boot: {error}");
            return ExitCode::FAILURE;
        }
    };
    Arc::new(AcpServer::new(
        Arc::new(host),
        Box::new(StdioRead),
        Arc::new(StdioWrite),
    ))
    .run();
    ExitCode::SUCCESS
}
