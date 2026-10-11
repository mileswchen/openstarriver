//! Live diagnostic (not part of the test suite): run the exact discovery
//! path the host runtime uses against a configured server, printing the
//! outcome or the failure. Usage:
//!   cargo run -p starriver-mcp-bridge --example live_discovery -- [server]
//! Defaults to reading ~/.starriver/mcp.yaml and probing every enabled
//! server (or just the named one).
use starriver_mcp_bridge::config::McpConfig;
use starriver_mcp_bridge::definition::discover_bridge_shared;
use std::sync::Arc;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let want = std::env::args().nth(1);
    let path = McpConfig::default_path().expect("no HOME for mcp.yaml lookup");
    let config = match McpConfig::load(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("load {} failed: {e}", path.display());
            std::process::exit(2);
        }
    };
    eprintln!("config: {}", path.display());
    let mut failed = false;
    for (name, srv) in config.enabled_servers() {
        if let Some(ref w) = want {
            if &name != w {
                continue;
            }
        }
        let started = std::time::Instant::now();
        match discover_bridge_shared(&name, srv, Arc::new(|| None)).await {
            Ok((defs, _conn)) => {
                eprintln!(
                    "OK   {name}: {} tools in {:?} — first: {}",
                    defs.len(),
                    started.elapsed(),
                    defs.first().map(|d| d.name.clone()).unwrap_or_default()
                );
            }
            Err(e) => {
                failed = true;
                eprintln!("FAIL {name} after {:?}: {e:#}", started.elapsed());
            }
        }
    }
    if failed {
        std::process::exit(1);
    }
}
