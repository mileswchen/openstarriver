//! `session-inspect <path>`: non-destructive read-only diagnostics for a
//! Starriver session library (WI-28). Prints statistics and the canonical
//! schema drift report. Exit codes: 0 clean, 1 drift found, 2 unreadable.

use std::path::PathBuf;

fn main() {
    let path = match std::env::args().nth(1) {
        Some(path) => PathBuf::from(path),
        None => {
            eprintln!("usage: session-inspect <sessions.db>");
            std::process::exit(2);
        }
    };
    match starriver_session_persistence::inspect::inspect_library(&path) {
        Ok(report) => {
            print!("{}", report.summary());
            if report.drift.is_empty() {
                std::process::exit(0);
            }
            std::process::exit(1);
        }
        Err(error) => {
            eprintln!("session-inspect: {error}");
            std::process::exit(2);
        }
    }
}
