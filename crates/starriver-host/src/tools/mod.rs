pub mod bash;
pub mod bash_readonly;
pub mod code_runtime;
pub mod common;
pub mod edit;
pub mod exit_plan_mode;
pub mod git;
pub mod glob;
pub mod goal;
pub mod grep;
pub mod imagegen;
pub mod lsp;
pub mod patch;
pub mod plan;
pub mod question;
pub mod read;
pub mod reload_image;
pub mod session_reference;
pub mod spill;
pub mod subagent;
pub mod system_service;
pub mod terminal;
pub mod todo;
pub mod web_cache;
pub mod web_common;
pub mod web_fetch;
pub mod web_render;
pub mod web_search;
pub mod web_search_provider;
pub mod write;

/// tool-guide parity: one division-of-labor section per first-party tool
/// family, registered at boot into the kernel's reserved 100..=199 band as
/// `<tool>:guide` (TOOL_GUIDE_ORDER_MIN/MAX). keeps the same shape — each
/// tool plugin registers a `tool:<name>` section with a sparse order — and the
/// texts here are starriver adaptations of the originals: inspect with
/// read (not cat), search with grep (not shell grep/rg), find files with glob
/// (not find), edit over wholesale write, PTY hygiene, untrusted web data.
/// Each tuple is `(tool name, order, text)`.
pub fn tool_guide_sections() -> Vec<(&'static str, i32, &'static str)> {
    vec![
        bash::guide(),
        read::guide(),
        write::guide(),
        edit::guide(),
        glob::guide(),
        grep::guide(),
        terminal::guide(),
        web_search::guide(),
        web_fetch::guide(),
    ]
}

#[cfg(test)]
mod guide_tests {
    use super::tool_guide_sections;
    use starriver_kernel::system_prompt::{TOOL_GUIDE_ORDER_MAX, TOOL_GUIDE_ORDER_MIN};

    /// The game004 regression: the model fell back to `Shell grep` because no
    /// prompt section told it the grep tool exists for that. Every first-party
    /// family must carry its guide, inside the reserved band, with a unique
    /// name — the assembler renders `<tool>:guide` sections by order.
    #[test]
    fn every_first_party_family_has_a_guide_in_the_reserved_band() {
        let guides = tool_guide_sections();
        for (tool, order, text) in &guides {
            assert!(
                (TOOL_GUIDE_ORDER_MIN..=TOOL_GUIDE_ORDER_MAX).contains(order),
                "{tool}:guide order {order} outside the reserved band"
            );
            assert!(!text.trim().is_empty(), "{tool}:guide has empty text");
        }
        let mut names: Vec<&str> = guides.iter().map(|(t, _, _)| *t).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), guides.len(), "duplicate guide tool names");

        for tool in ["bash", "read", "write", "edit", "glob", "grep"] {
            assert!(
                names.contains(&tool),
                "{tool} has no guide section — shell-fallback regression risk"
            );
        }
    }

    /// The steering lines that fix the shell-fallback failure mode must stay
    /// verbatim: the model is told the dedicated tool exists INSTEAD of the
    /// shell command it habitually reaches for.
    #[test]
    fn search_and_inspect_guides_name_the_shell_alternative() {
        let guides = tool_guide_sections();
        let get = |tool: &str| {
            guides
                .iter()
                .find(|(t, _, _)| *t == tool)
                .map(|(_, _, text)| *text)
                .unwrap()
        };
        assert!(get("grep").contains("not shell grep or rg"));
        assert!(get("read").contains("not shell commands like cat"));
        assert!(get("glob").contains("not shell find or ls"));
        assert!(get("bash").contains("prefer the dedicated tools"));
    }
}
