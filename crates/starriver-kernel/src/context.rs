//! Runtime-context types shared by the system-prompt seam: plugins register
//! named context contributions through `SystemPromptRegistry` (global or
//! per-agent scope); the loop renders them into the pre-step history snapshot
//! via [`RenderContext`] evaluation. Scope key is the existing `Context::tag`;
//! None = global.

use std::sync::Arc;

use crate::agent::AgentOptions;
use crate::session::Session;

/// One named dynamic context contribution.
#[derive(Clone)]
pub struct PromptContext {
    pub name: String,
    pub order: u32,
    pub render: Arc<dyn Fn(&RenderContext) -> String + Send + Sync>,
}

/// Values available while rendering one context contribution.
pub struct RenderContext<'a> {
    pub session: &'a Session,
    pub options: &'a AgentOptions,
}

/// One rendered context contribution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssembledContext {
    pub name: String,
    pub text: String,
}
