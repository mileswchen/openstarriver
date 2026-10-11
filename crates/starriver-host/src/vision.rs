//! Built-in per-provider vision (image input) capability declarations.
//!
//! Replaces the per-provider "supports_vision" checkbox as the primary source
//! of truth: cloud providers whose current model families accept image input
//! are declared `Some(true)` here, text-only families `Some(false)`, and
//! mixed/local/self-hosted providers stay undeclared (`None`) — permissive,
//! with the runtime error memory (WI-152 S4) correcting a route that rejects
//! images in practice. Resolution is fully automatic (WI-152 v2); the stored
//! `supports_vision` declaration is legacy and ignored.

/// Built-in vision declaration for one provider id.
///
/// `Some(true)` = the provider's models accept image input;
/// `Some(false)` = text-only family;
/// `None` = undeclared (local/self-hosted, per-model).
pub fn builtin_vision(provider_id: &str) -> Option<bool> {
    match provider_id {
        // zhipu stays undeclared: the family mixes text-only routes (the GLM
        // Coding Plan endpoints reject every non-text content block with
        // `messages.content.type 参数非法，取值范围 ['text']`) with vision
        // models (glm-*-v). A table-backed `Some(true)` made the route
        // unlearnable — image turns failed on the coding endpoints and the
        // runtime error memory could never correct it. Undeclared routes are
        // permissive but learn from rejections (WI-152 S4).
        "deepseek" | "qwen" | "gemini" | "openai" | "anthropic" | "openrouter" => Some(true),
        "moonshot" => Some(false),
        _ => None,
    }
}

/// Whether a turn error message is a provider rejecting non-text content
/// (the text-only rejection probe for the runtime error memory, WI-152 S4).
/// Matches the phrasings observed from major providers — English keyword
/// families plus the GLM/bigmodel validation error (`content.type` /
/// allowed-range-to-text wording), which the English-only list could never
/// see, leaving such routes failing on every image turn.
pub fn is_text_only_rejection(message: &str) -> bool {
    let msg = message.to_lowercase();
    [
        "image",
        "vision",
        "multimodal",
        "multi-modal",
        "not supported",
        "unsupported",
        "does not support",
        "text-only",
        // GLM/bigmodel 1210: `messages.content.type 参数非法，取值范围 ['text']`
        "content.type",
        "取值范围 ['text']",
        "仅支持文本",
        "只支持文本",
        "不支持图片",
        "不支持图像",
    ]
    .iter()
    .any(|k| msg.contains(k))
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
            "qwen",
            "gemini",
            "openai",
            "anthropic",
            "openrouter",
        ] {
            assert_eq!(builtin_vision(id), Some(true), "{id} should declare vision");
        }
        assert_eq!(builtin_vision("moonshot"), Some(false));
        // Mixed family (text-only coding endpoints + vision models): must stay
        // undeclared so the runtime error memory can learn per route.
        assert_eq!(builtin_vision("zhipu"), None);
        assert_eq!(builtin_vision("ollama"), None);
        assert_eq!(builtin_vision("custom-endpoint"), None);
    }

    /// The exact GLM/bigmodel rejection that used to slip past the
    /// English-only probe — no "image"/"vision"/"unsupported" token in it,
    /// so the runtime never recorded the route as text-only and every image
    /// turn failed identically.
    #[test]
    fn glm_content_type_rejection_is_detected() {
        assert!(is_text_only_rejection(
            "model call failed: INVALID_REQUEST: {\"error\":{\"code\":\"1210\",\"message\":\"messages.content.type 参数非法，取值范围 ['text']\"}}"
        ));
        assert!(is_text_only_rejection(
            "messages[0].content.type 参数非法，取值范围 ['text']"
        ));
        assert!(is_text_only_rejection("该模型仅支持文本输入"));
        assert!(is_text_only_rejection("Input image not supported by this model"));
        assert!(is_text_only_rejection("MODEL IS TEXT-ONLY"));
        // Unrelated failures must not poison the route.
        assert!(!is_text_only_rejection("rate limit exceeded"));
        assert!(!is_text_only_rejection("max_tokens 取值范围非法"));
        assert!(!is_text_only_rejection("invalid api key"));
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
