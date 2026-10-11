// Cross-agent shared memory — extracted from `starriver-core::memory` so it can
// be consumed by the host, the standalone MCP server, and any future crate
// without dragging in the core runtime (docs/36-memory-system-design.md).
//
// This crate deliberately has NO dependency on starriver-core or
// starriver-session: it owns a minimal frontmatter (de)serializer and its own
// chat-turn types, so anything can depend on it cheaply.

pub mod chat;
mod cost;
pub mod export;
mod extract;
mod frontmatter;
pub mod ingest;
pub mod plugin;
mod store;

pub use chat::{ChatBlock, ChatMessage, ChatRole};
pub use cost::{
    estimate_tokens, CostMode, ExceededInfo, TokenBudget, DEFAULT_SESSION_TOKEN_BUDGET,
    TOKEN_BUDGET_ENV,
};
pub use extract::{
    build_selection_prompt, extract_prompt, extractable_text, is_transient_policy_fact,
    llm_select, parse_extract_json, parse_selection_json, MemoryEntryDraft,
};
pub use store::{
    compact_facts_to_entries, now_unix, render_inject, score, slugify, tokenize_query,
    MemoryConfig, MemoryEntry, MemorySource, MemoryStore, DEFAULT_SOURCE_AGENT, GLOBAL_PROJECT,
};
