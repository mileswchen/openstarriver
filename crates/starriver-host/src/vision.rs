//! Built-in per-provider vision (image input) capability declarations.
//!
//! Replaces the per-provider "supports_vision" checkbox as the primary source
//! of truth: cloud providers whose current model families accept image input
//! are declared `Some(true)` here, text-only families `Some(false)`, and
//! local/self-hosted providers stay undeclared (`None`, decided per model by
//! the user). The user's explicit UI declaration always wins over this table
//! (WI-152 S1, /`inputModalities` alignment).

/// Built-in vision declaration for one provider id.
///
/// `Some(true)` = the provider's models accept image input;
/// `Some(false)` = text-only family;
/// `None` = undeclared (local/self-hosted, per-model).
pub fn builtin_vision(provider_id: &str) -> Option<bool> {
    match provider_id {
        "deepseek" | "zhipu" | "qwen" | "gemini" | "openai" | "anthropic" | "openrouter" => {
            Some(true)
        }
        "moonshot" => Some(false),
        _ => None,
    }
}

/// Effective vision support for one provider route (WI-152 v2: fully
/// automatic — /-style `inputModalities` semantics, no user setting).
/// The built-in table decides for known providers; unknown providers (custom
/// endpoints, local runtimes) default to supported — a truly text-only
/// endpoint surfaces the provider error, and the runtime error memory
/// (`runtime::text_only_providers`) then routes future sends away. The
/// stored `supports_vision` declaration is ignored everywhere.
pub fn effective_vision_support(provider_id: &str) -> bool {
    builtin_vision(provider_id).unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_table_covers_the_cloud_presets() {
        for id in [
            "deepseek",
            "zhipu",
            "qwen",
            "gemini",
            "openai",
            "anthropic",
            "openrouter",
        ] {
            assert_eq!(builtin_vision(id), Some(true), "{id} should declare vision");
        }
        assert_eq!(builtin_vision("moonshot"), Some(false));
        assert_eq!(builtin_vision("ollama"), None);
        assert_eq!(builtin_vision("custom-endpoint"), None);
    }

    #[test]
    fn resolution_is_automatic_table_then_supported() {
        // Known text-only family → gated.
        assert!(!effective_vision_support("moonshot"));
        // Known vision family → allowed.
        assert!(effective_vision_support("deepseek"));
        // Unknown provider → permissive (backend strips if it turns out
        // text-only; WI-152 v2: capability is automatic, no manual setting).
        assert!(effective_vision_support("my-openai-compat"));
    }
}
