//! Starriver foreground protocol (FPM P1) — the Rust-side source of truth
//! for the versioned contract.
//!
//! Three responsibilities:
//! - [`wire`]: request/response/event envelopes and the error model
//!   (transport-agnostic wire format).
//! - [`schema`]: a JSON Schema subset for parameters/results, plus the
//!   validator.
//! - [`registry`]: the method registry (the authoritative dispatch point
//!   for `invoke`) and the caller permission model.
//!
//! Design constraint: the contract is add-only (new methods/fields; never
//! remove or change semantics); `protocol.info` exposes the version and
//! method catalog to clients as the handshake and discovery entry point.

pub mod permission;
pub mod registry;
pub mod schema;
pub mod wire;

pub use permission::CallerScope;
pub use registry::{MethodCall, MethodHandler, MethodRegistry, MethodSpec};
pub use wire::{ApiError, EventEnvelope, Request, Response, PROTOCOL_VERSION};
