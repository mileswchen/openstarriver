//! MCP bridge: adapt MCP servers (tools / resources / prompts) onto the
//! kernel `ToolDefinition` seam, with connection supervision.
//!
//! Extracted from `starriver-host::mcp` (WI-23); the host keeps a re-export
//! shim at `starriver_host::mcp` so existing consumers keep compiling.

pub mod adapter;
mod common;
pub mod config;
pub mod definition;
pub mod health;
pub mod oauth;
pub mod transport;
pub mod write_semantics;
