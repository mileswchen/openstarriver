//! Compatibility shim: the MCP bridge moved to the `starriver-mcp-bridge`
//! crate (WI-23). All module paths (`crate::mcp::definition::…`,
//! `crate::mcp::health::…`, …) keep resolving through these re-exports.

pub use starriver_mcp_bridge::{
    adapter, config, definition, health, oauth, transport, write_semantics,
};
