// Minimal chat-turn mirror of the core/kernel message types.
//
// The memory crate must not depend on starriver-core, but extraction and
// selection prompts still need a "role + content blocks" shape. `ChatBlock`
// keeps a `Thinking` variant so the extraction red line (thinking must never
// become memory) is enforced HERE, at the boundary, no matter which agent's
// history is being fed in.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatRole {
    System,
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ChatBlock {
    Text {
        text: String,
    },
    /// Reasoning content — always excluded from extraction (red line).
    Thinking {
        text: String,
    },
    /// Tool use / tool result / anything else — always excluded.
    Other,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: Vec<ChatBlock>,
}

impl ChatMessage {
    pub fn system(text: &str) -> Self {
        Self {
            role: ChatRole::System,
            content: vec![ChatBlock::Text {
                text: text.to_string(),
            }],
        }
    }

    pub fn user(text: &str) -> Self {
        Self {
            role: ChatRole::User,
            content: vec![ChatBlock::Text {
                text: text.to_string(),
            }],
        }
    }

    pub fn assistant(text: &str) -> Self {
        Self {
            role: ChatRole::Assistant,
            content: vec![ChatBlock::Text {
                text: text.to_string(),
            }],
        }
    }
}
