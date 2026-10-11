//! Protocol server shared layer (FPM P4):
//!
//! - [`protocol_catalog`]: host method catalog (`MethodRegistry<HostRuntime>`) —
//!   the single source of truth for the protocol contract, reused by desktop
//!   and daemon to avoid a second source of truth.
//! - [`http_server`]: loopback HTTP + SSE transport (`Dispatcher` is defined
//!   here); the embedded mode (Electron main) and the daemon (`starriverd`)
//!   share the same implementation.

pub mod http_server;
pub mod protocol_catalog;
pub mod ws;
