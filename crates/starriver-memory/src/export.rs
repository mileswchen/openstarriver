// Instruction-file export: render memories into a marked section of a
// project's AGENTS.md / CLAUDE.md so any other instruction-file agent
// consumes them via its own mechanism.
//
// The merge only ever touches the marked block — user content around it is
// byte-preserved, and the section is idempotent across repeated exports.

use crate::store::{render_inject, MemoryEntry, MemoryStore, GLOBAL_PROJECT};

pub const MARKER_START: &str = "<!-- starriver-memory:start -->";
pub const MARKER_END: &str = "<!-- starriver-memory:end -->";

/// Render the marked section body (without the surrounding markers).
pub fn render_section(entries: &[MemoryEntry]) -> String {
    let hooks = render_inject(entries, usize::MAX / 4);
    if hooks.is_empty() {
        return format!("{MARKER_START}\n{MARKER_END}");
    }
    // Strip the "## Memory" heading: inside AGENTS.md the context is implicit.
    let body = hooks.trim_start_matches("## Memory").trim();
    let agents: Vec<&str> = entries
        .iter()
        .map(|e| e.source_agent.as_str())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    format!(
        "{MARKER_START}\n# Shared memory (from {sources})\n\n{body}\n{MARKER_END}",
        sources = agents.join(", "),
    )
}

/// Replace (or insert) the marked section in `content`. Returns `None` when
/// the result would be byte-identical. A file with no markers gets the
/// section appended at the end.
pub fn merge_marked_section(content: &str, section: &str) -> Option<String> {
    let (start, end) = (content.find(MARKER_START), content.find(MARKER_END));
    let merged = match (start, end) {
        (Some(s), Some(e)) if e >= s => {
            let mut out = String::with_capacity(content.len());
            out.push_str(&content[..s]);
            out.push_str(section);
            out.push_str(&content[e + MARKER_END.len()..]);
            out
        }
        _ => {
            let mut out = content.to_string();
            if !out.is_empty() && !out.ends_with("\n\n") {
                while !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push('\n');
            }
            out.push_str(section);
            out.push('\n');
            out
        }
    };
    (merged != content).then_some(merged)
}

/// Export a project's memories (+ `_global`) into both instruction files in
/// `dir` (AGENTS.md first, then CLAUDE.md). Missing files are created
/// Code). Missing files are created containing only the marked section.
/// Returns which files were written.
pub fn export_to_instruction_files(
    store: &MemoryStore,
    project: &str,
    dir: &std::path::Path,
) -> anyhow::Result<Vec<String>> {
    let mut entries = store.list(project).unwrap_or_default();
    if project != GLOBAL_PROJECT {
        entries.extend(store.list(GLOBAL_PROJECT).unwrap_or_default());
    }
    let section = render_section(&entries);
    let mut written = Vec::new();
    for name in ["AGENTS.md", "CLAUDE.md"] {
        let path = dir.join(name);
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        let Some(merged) = merge_marked_section(&existing, &section) else {
            continue;
        };
        std::fs::write(&path, merged)?;
        written.push(name.to_string());
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{MemoryConfig, MemorySource, DEFAULT_SOURCE_AGENT};

    fn entry(name: &str, desc: &str, project: &str, agent: &str) -> MemoryEntry {
        MemoryEntry {
            name: name.into(),
            description: desc.into(),
            body: format!("{desc} body"),
            created: 1,
            updated: 1,
            source: MemorySource::Extract,
            source_agent: agent.into(),
            project: project.into(),
        }
    }

    #[test]
    fn section_lists_hooks_and_sources() {
        let entries = vec![
            entry("a", "use pnpm", "p", "starriver"),
            entry("b", "port 8080", "p", "codex"),
        ];
        let s = render_section(&entries);
        assert!(s.starts_with(MARKER_START));
        assert!(s.ends_with(MARKER_END));
        assert!(s.contains("use pnpm"));
        assert!(s.contains("from codex, starriver"), "sources listed");
    }

    #[test]
    fn merge_replaces_only_marked_block() {
        let file = "line one\n<!-- starriver-memory:start -->\nold\n<!-- starriver-memory:end -->\nline two";
        let section = "<!-- starriver-memory:start -->\nnew\n<!-- starriver-memory:end -->";
        let out = merge_marked_section(file, section).unwrap();
        assert!(out.starts_with("line one\n"));
        assert!(out.ends_with("line two"));
        assert!(out.contains("new"));
        assert!(!out.contains("old"));
        // Idempotent: same section again → no change.
        assert!(merge_marked_section(&out, section).is_none());
    }

    #[test]
    fn merge_appends_when_no_markers() {
        let out = merge_marked_section("my notes", "SEC").unwrap();
        assert!(out.starts_with("my notes\n\nSEC"));
    }

    #[test]
    fn export_writes_both_instruction_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(dir.path().join("m"), MemoryConfig::default());
        store
            .upsert(entry("a", "use pnpm", "p1", "starriver"))
            .unwrap();
        let target = dir.path().join("proj");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("AGENTS.md"), "# My project\n\nkeep this\n").unwrap();
        let written = export_to_instruction_files(&store, "p1", &target).unwrap();
        assert_eq!(written, vec!["AGENTS.md", "CLAUDE.md"]);
        let agents = std::fs::read_to_string(target.join("AGENTS.md")).unwrap();
        assert!(agents.contains("# My project"));
        assert!(agents.contains("keep this"));
        assert!(agents.contains("use pnpm"));
        assert!(agents.contains(MARKER_START));
        let claude = std::fs::read_to_string(target.join("CLAUDE.md")).unwrap();
        assert!(claude.contains("use pnpm"));
        // Second export is a no-op.
        assert!(export_to_instruction_files(&store, "p1", &target)
            .unwrap()
            .is_empty());
    }
}
