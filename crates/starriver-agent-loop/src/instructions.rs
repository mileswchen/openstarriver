//! Workspace-instructions engine: discovers workspace instruction files in a
//! fixed order, composes one framed snapshot, and enforces the byte budgets
//! from the prompt-parity design (§2.3):
//!
//! - discovery order `AGENTS.md` → `CLAUDE.md` → `STARRIVER.md`, each with its
//!   optional `*.local.md` overlay directly behind it;
//! - a single candidate larger than 1 MiB is skipped with a warn;
//! - the composite snapshot is capped at 64 KiB — over budget, whole trailing
//!   blocks are dropped keeping the longest fitting head.
//!
//! Pure functions only; wiring (pre-step projection, digest dedup) lives in
//! [`crate::agent`].

use std::collections::{hash_map::DefaultHasher, HashMap};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

/// Composite snapshot budget (production value, cordis.yml).
pub const MAX_BYTES: usize = 65_536;
/// Single candidate read cap; larger files are skipped (anti-misconfig
/// guard).
pub const MAX_SINGLE_FILE_BYTES: u64 = 1 << 20;

/// One discovered instruction candidate: bracket label plus trimmed content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionFile {
    pub label: String,
    pub content: String,
}

fn read_capped(root: &Path, name: &str) -> Option<InstructionFile> {
    let path = root.join(name);
    let Ok(meta) = std::fs::metadata(&path) else {
        return None;
    };
    if !meta.is_file() {
        return None;
    }
    if meta.len() > MAX_SINGLE_FILE_BYTES {
        tracing::warn!(file = %name, size = meta.len(), cap = MAX_SINGLE_FILE_BYTES,
            "instruction file exceeds the single-file cap; skipped");
        return None;
    }
    match std::fs::read_to_string(&path) {
        Ok(content) => Some(InstructionFile {
            label: name.to_string(),
            content: content.trim_end().to_string(),
        }),
        Err(error) => {
            tracing::warn!(file = %name, error = %error, "instruction file unreadable; skipped");
            None
        }
    }
}

/// Discover candidates under `root` in canonical order. Missing files are
/// simply absent. Same-directory duplicates collapse: a later candidate whose
/// trimmed content matches an
/// already-kept one is dropped.
pub fn discover(root: &Path) -> Vec<InstructionFile> {
    let (files, _seen) = discover_inner(root);
    files
}

fn discover_inner(root: &Path) -> (Vec<InstructionFile>, HashMap<String, ()>) {
    let mut files = Vec::new();
    let mut seen: HashMap<String, ()> = HashMap::new();
    let push = |files: &mut Vec<InstructionFile>,
                seen: &mut HashMap<String, ()>,
                file: InstructionFile| {
        let key = file.content.trim().to_string();
        if seen.contains_key(&key) {
            return;
        }
        seen.insert(key, ());
        files.push(file);
    };
    for name in [
        "AGENTS.md",
        "AGENTS.local.md",
        "CLAUDE.md",
        "CLAUDE.local.md",
        "STARRIVER.md",
        "STARRIVER.local.md",
    ] {
        if let Some(file) = read_capped(root, name) {
            push(&mut files, &mut seen, file);
        }
    }
    (files, seen)
}

/// The user-global layer (the global counterpart of the per-project file): a single fixed
/// `$HOME/.starriver/AGENTS.md`, absent when HOME is unset or the file is
/// missing/oversized. Label carries the `(global)` marker.
pub fn global_layer(home: Option<&Path>) -> Option<InstructionFile> {
    let home = home?;
    let mut file = read_capped(&home.join(".starriver"), "AGENTS.md")?;
    file.label = "AGENTS.md (global)".into();
    Some(file)
}

/// Layered discovery: user-global first, then the project set. Duplicates are
/// NOT collapsed across layers (different directories, different scopes).
pub fn discover_layered(home: Option<&Path>, root: &Path) -> Vec<InstructionFile> {
    let mut files = Vec::new();
    if let Some(global) = global_layer(home) {
        files.push(global);
    }
    files.extend(discover(root));
    files
}

/// Model-facing notice appended when the composite exceeded the budget
/// (COMPACT_WORKSPACE_CONTEXT_INTRO parity).
pub const TRUNCATION_NOTICE: &str =
    "Workspace instructions were omitted or truncated to fit the configured byte budget.";

/// Compose under the budget: over-budget composites keep the longest fitting
/// head and end with the truncation notice (the notice itself is budgeted).
pub fn compose_budgeted(files: &[InstructionFile]) -> String {
    compose_budgeted_with(files, MAX_BYTES)
}

/// [`compose_budgeted`] with an explicit byte budget (settings seam: the
/// `prompt.instructions_max_bytes` override).
pub fn compose_budgeted_with(files: &[InstructionFile], max_bytes: usize) -> String {
    let composed = compose(files);
    if composed.len() <= max_bytes {
        return composed;
    }
    let head_budget = max_bytes.saturating_sub(TRUNCATION_NOTICE.len() + 2);
    let cut = truncate_to_budget(&composed, head_budget);
    format!("{cut}\n\n{TRUNCATION_NOTICE}")
}

/// Frame preamble (AGENTS.md-spec parity): what these files are, how
/// scope and precedence work, and that the content is already in context —
/// the model must not spend tool calls re-reading instruction files. Lives in
/// the frame itself so it always co-occurs with the content it describes.
pub const FRAME_PREAMBLE: &str = "Repository instruction files collected from this workspace and its parents \
(AGENTS.md / AGENTS.local.md / CLAUDE.md / CLAUDE.local.md / STARRIVER.md and the global layer). \
Each file's scope is the directory tree containing it; when instructions conflict, deeper files \
take precedence over shallower ones, and system-level rules (sandbox, approval, plan mode) \
outrank any file here. Everything below is already in your context — do not re-read these \
files with tools.";

/// Compose the framed `<system-reminder>` snapshot. No candidates → empty
/// string (nothing is projected this step).
pub fn compose(files: &[InstructionFile]) -> String {
    if files.is_empty() {
        return String::new();
    }
    let mut out = String::from("<system-reminder>\n# Workspace instructions\n\n");
    out.push_str(FRAME_PREAMBLE);
    out.push_str("\n\n");
    for file in files {
        out.push_str(&format!("[{}]\n{}\n\n", file.label, file.content));
    }
    out.push_str("</system-reminder>");
    out
}

/// Enforce the composite budget: keep the largest prefix of `\n\n`-separated
/// blocks that fits; a lone first block wider than the budget is hard-cut at
/// the nearest char boundary below it.
pub fn truncate_to_budget(text: &str, budget: usize) -> String {
    if text.len() <= budget {
        return text.to_string();
    }
    let mut end = 0usize;
    for (index, _) in text.match_indices("\n\n") {
        if index > budget {
            break;
        }
        end = index;
    }
    if end == 0 {
        // No block boundary fits inside the budget: byte-level head cut.
        let mut safe = budget.min(text.len());
        while !text.is_char_boundary(safe) {
            safe -= 1;
        }
        return text[..safe].to_string();
    }
    text[..end].to_string()
}

/// Stable within-process digest for change detection across pre-steps.
pub fn digest(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// Message-source tag used by the projection (restored on resume).
pub const INSTRUCTIONS_PLUGIN: &str = "agent-instructions";
/// Sent when a previously instructed workspace stops providing any files.
pub const INSTRUCTIONS_CLEARED: &str =
    "Current workspace instructions: none. Earlier workspace-instruction snapshots no longer apply.";

use starriver_kernel::llm::{
    mint_message_id, ContentBlock, ContextForm, Message, MessageRole, MessageSource,
};
use starriver_kernel::session::{Session, SessionEvent};

/// Change-only projector for the composite snapshot: recomputes discovery at
/// every pre-step (covers edits through any tool, D4) and emits only when the
/// digest of the composed text differs from the last projected one.
pub struct InstructionsProjection {
    /// None = no owned message yet; Some(digest) includes the empty state so
    /// cleared→cleared transitions stay silent.
    last: Option<u64>,
    /// Composite byte budget (settings seam); defaults to [`MAX_BYTES`].
    max_bytes: usize,
}

impl InstructionsProjection {
    pub fn new(session: &Session) -> Self {
        Self::with_budget(session, None)
    }

    /// `budget` comes from `prompt_settings.instructions_max_bytes`; `None`
    /// keeps the engine default.
    pub fn with_budget(session: &Session, budget: Option<usize>) -> Self {
        Self {
            last: restore_digest(session),
            max_bytes: budget.unwrap_or(MAX_BYTES),
        }
    }

    pub fn project(&mut self, root: &Path) -> Option<Message> {
        let home = std::env::var("HOME").ok().map(PathBuf::from);
        let files = discover_layered(home.as_deref(), root);
        let composed = compose_budgeted_with(&files, self.max_bytes);
        let next = digest(&composed);
        match self.last {
            None if composed.is_empty() => return None,
            Some(prev) if prev == next => return None,
            _ => {}
        }
        self.last = Some(next);
        let payload = if composed.is_empty() {
            INSTRUCTIONS_CLEARED.to_string()
        } else {
            composed.clone()
        };
        let form = if composed.is_empty() {
            None
        } else {
            // WI-35: truncation metadata rides the form so the session
            // event (and any UI consumer) can show the budget state.
            // BUG-33: the flag must compare the UNbudgeted composite —
            // `composed` is already the budgeted text, so comparing it
            // against the budget was always false.
            let truncated = compose(&files).len() > self.max_bytes;
            Some(ContextForm::Snapshot {
                sections: files
                    .iter()
                    .map(|f| starriver_kernel::llm::ContextSnapshotSection {
                        name: f.label.clone(),
                        text: f.content.clone(),
                    })
                    .collect(),
                truncated,
                budget_bytes: self.max_bytes as u64,
                composed_bytes: composed.len() as u64,
            })
        };
        Some(Message {
            id: mint_message_id(),
            role: MessageRole::User,
            content: vec![ContentBlock::Text(payload)],
            source: MessageSource::Plugin {
                plugin: INSTRUCTIONS_PLUGIN.into(),
                form,
            },
        })
    }
}

fn owned_text(event: &SessionEvent) -> Option<(String, bool)> {
    if event.event_type != "user/message" {
        return None;
    }
    let message = Message::from_json(event.data.clone()).ok()?;
    let owned = matches!(&message.source,
        MessageSource::Plugin { plugin, .. } if plugin == INSTRUCTIONS_PLUGIN);
    if !owned {
        return None;
    }
    let on_surface = true; // resolved by caller against surface nodes
    let text = message
        .content
        .first()
        .and_then(|b| match b {
            ContentBlock::Text(t) => Some(t.clone()),
            _ => None,
        })
        .unwrap_or_default();
    Some((text, on_surface))
}

/// Find the last owned instruction message: a cleared/off-surface tail maps to
/// the empty digest so resume never resends it.
fn restore_digest(session: &Session) -> Option<u64> {
    let events = session.events();
    for event in events.iter().rev() {
        let Some((text, _on_surface)) = owned_text(event) else {
            continue;
        };
        // Cleared payloads — and snapshots that later left the surface — both
        // collapse to the empty state.
        let cleared_or_off_surface =
            text == INSTRUCTIONS_CLEARED || { !session.surface().nodes.contains(&event.seq) };
        return Some(if cleared_or_off_surface {
            digest("")
        } else {
            digest(&text)
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(dir: &Path, name: &str, content: &str) {
        std::fs::write(dir.join(name), content).unwrap();
    }

    #[test]
    fn discovery_absent_when_no_candidates() {
        let dir = tempfile::tempdir().unwrap();
        assert!(discover(dir.path()).is_empty());
    }

    #[test]
    fn discovery_order_base_then_overlay() {
        let dir = tempfile::tempdir().unwrap();
        for (name, body) in [
            ("CLAUDE.md", "claude"),
            ("AGENTS.local.md", "agents-local"),
            ("AGENTS.md", "agents"),
            ("STARRIVER.local.md", "sr-local"),
        ] {
            touch(dir.path(), name, body);
        }
        let files = discover(dir.path());
        let labels: Vec<&str> = files.iter().map(|f| f.label.as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "AGENTS.md",
                "AGENTS.local.md",
                "CLAUDE.md",
                "STARRIVER.local.md",
            ],
            "base before overlay"
        );
    }

    #[test]
    fn legacy_flycode_md_is_fully_invisible() {
        // WI-58 closure lock: the FLYCODE.md compatibility tail (last slot +
        // warn-once migration hint) is retired — a leftover file must not
        // reach discovery in any form, so a stale project file can never
        // re-enter the model context.
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "STARRIVER.md", "current");
        touch(dir.path(), "FLYCODE.md", "legacy body");
        let files = discover(dir.path());
        let labels: Vec<&str> = files.iter().map(|f| f.label.as_str()).collect();
        assert_eq!(labels, vec!["STARRIVER.md"]);
        assert!(
            files.iter().all(|f| !f.content.contains("legacy body")),
            "legacy content must not leak into the snapshot"
        );
    }

    #[test]
    fn compose_frames_labels_inside_the_reminder_shell() {
        let files = vec![
            InstructionFile {
                label: "AGENTS.md".into(),
                content: "use pnpm".into(),
            },
            InstructionFile {
                label: "CLAUDE.md".into(),
                content: "no unwrap".into(),
            },
        ];
        let composed = compose(&files);
        assert!(
            composed.starts_with(
                "<system-reminder>\n# Workspace instructions\n\nRepository instruction files"
            ),
            "frame preamble must sit under the heading: {composed}"
        );
        assert!(composed.contains("[AGENTS.md]\nuse pnpm"));
        assert!(composed.contains("[CLAUDE.md]\nno unwrap"));
        assert!(composed.ends_with("</system-reminder>"));
    }

 /// AGENTS.md-spec parity: the frame itself carries provenance,
    /// scope/precedence, and the no-re-read rule.
    #[test]
    fn frame_preamble_carries_scope_precedence_and_no_reread() {
        assert!(FRAME_PREAMBLE.contains("deeper files"));
        assert!(FRAME_PREAMBLE.contains("take precedence over shallower ones"));
        assert!(FRAME_PREAMBLE.contains("outrank any file here"));
        assert!(FRAME_PREAMBLE.contains("do not re-read these"));
    }

    #[test]
    fn compose_empty_without_files() {
        assert_eq!(compose(&[]), "");
    }

    #[test]
    fn oversized_single_file_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        // 1 MiB cap + a byte → skipped; small neighbour survives.
        touch(
            dir.path(),
            "AGENTS.md",
            &"x".repeat(MAX_SINGLE_FILE_BYTES as usize + 1),
        );
        touch(dir.path(), "CLAUDE.md", "tiny");
        let files = discover(dir.path());
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].label, "CLAUDE.md");
    }

    #[test]
    fn truncation_keeps_whole_head_blocks_within_budget() {
        let blocks = ["aaaa".to_string(), "bbbb".to_string(), "cccc".to_string()];
        let text = blocks.join("\n\n"); // 4+2+4+2+4 = 16 bytes
                                        // Budget fits two blocks plus their single separator.
        let cut = truncate_to_budget(&text, 10);
        assert_eq!(cut, "aaaa\n\nbbbb");
        // Whole-text fits: unchanged.
        assert_eq!(truncate_to_budget(&text, MAX_BYTES), text);
    }

    #[test]
    fn truncation_hard_cuts_when_no_boundary_fits_and_stays_char_safe() {
        let text = "萝萝萝萝萝"; // multi-byte, no "\n\n"
        let cut = truncate_to_budget(text, 7);
        assert!(cut.len() <= 7);
        assert!(cut.is_char_boundary(cut.len()));
        assert_eq!(cut.chars().count(), 2, "two full chars fit in 7 bytes");
    }

    #[test]
    fn digest_is_sensitive_and_stable() {
        assert_eq!(digest("alpha"), digest("alpha"));
        assert_ne!(digest("alpha"), digest("beta"));
    }

    // ── InstructionsProjection (change-only state machine) ─────────────────

    use starriver_kernel::session::{AppendOptions, SessionId, SurfaceOp};

    fn fresh_session() -> Session {
        Session::new(SessionId("s1".into()), cordis_rslite::EventBus::new())
    }

    fn append_surface(session: &Session, message: &Message) {
        session
            .append_with(
                "user/message",
                message.to_json().unwrap(),
                AppendOptions {
                    surface_op: Some(SurfaceOp::Append),
                    source_event_seqs: Vec::new(),
                },
            )
            .unwrap();
    }

    fn first_text(message: &Message) -> String {
        match message.content.first() {
            Some(ContentBlock::Text(t)) => t.clone(),
            other => format!("{other:?}"),
        }
    }

    #[test]
    fn projection_baseline_silent_change_cleared_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "AGENTS.md", "rule one");
        let session = fresh_session();
        let mut projection = InstructionsProjection::new(&session);

        let baseline = projection.project(dir.path()).expect("first baseline");
        assert!(first_text(&baseline).contains("rule one"));

        // Unchanged → silent.
        assert!(projection.project(dir.path()).is_none());

        // All files removed → one CLEARED tail, then silence.
        std::fs::remove_file(dir.path().join("AGENTS.md")).unwrap();
        let cleared = projection.project(dir.path()).expect("cleared tail");
        assert_eq!(first_text(&cleared), INSTRUCTIONS_CLEARED);
        assert!(projection.project(dir.path()).is_none());
    }

    /// BUG-33 regression (WI-35): the snapshot's `truncated` flag must report
    /// whether the UNbudgeted composite exceeded the budget. The projection
    /// compared the already-truncated text against the budget, so the flag
    /// was always false and the UI could never show the truncation state.
    #[test]
    fn projection_reports_truncated_when_composite_exceeds_budget() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "AGENTS.md", &"rule ".repeat(20_000));
        let session = fresh_session();
        let mut projection = InstructionsProjection::with_budget(&session, Some(8 * 1024));

        let message = projection.project(dir.path()).expect("baseline snapshot");
        let MessageSource::Plugin { form: Some(form), .. } = &message.source else {
            panic!("plugin snapshot form expected, got {:?}", message.source);
        };
        let ContextForm::Snapshot { truncated, composed_bytes, .. } = form else {
            panic!("Snapshot form expected, got {form:?}");
        };
        assert!(
            *composed_bytes <= 8 * 1024,
            "composed text respects the budget (composed_bytes={composed_bytes})"
        );
        assert!(
            *truncated,
            "a 100KB composite under an 8KB budget must be flagged truncated"
        );
        // And a composite within budget reports false.
        let dir2 = tempfile::tempdir().unwrap();
        touch(dir2.path(), "AGENTS.md", "short");
        let session2 = fresh_session();
        let mut projection2 = InstructionsProjection::with_budget(&session2, Some(64 * 1024));
        let message2 = projection2.project(dir2.path()).unwrap();
        let MessageSource::Plugin { form: Some(ContextForm::Snapshot { truncated, .. }), .. } =
            &message2.source
        else {
            panic!("snapshot form expected");
        };
        assert!(!*truncated);
    }

    #[test]
    fn restore_from_surface_prevents_resend_on_restart() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "AGENTS.md", "rule one");
        let session = fresh_session();

        let mut first = InstructionsProjection::new(&session);
        let baseline = first.project(dir.path()).unwrap();
        append_surface(&session, &baseline);

        // A "restarted" projector over the same session must stay silent.
        let mut restarted = InstructionsProjection::new(&session);
        assert!(
            restarted.project(dir.path()).is_none(),
            "resume must not resend the retained snapshot"
        );
        // And the cleared transition still fires exactly once afterwards.
        std::fs::remove_file(dir.path().join("AGENTS.md")).unwrap();
        let cleared = restarted.project(dir.path()).unwrap();
        assert_eq!(first_text(&cleared), INSTRUCTIONS_CLEARED);
        append_surface(&session, &cleared);
        assert!(restarted.project(dir.path()).is_none());
    }

    #[test]
    fn off_surface_owned_history_maps_to_cleared_state() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "AGENTS.md", "rule one");
        let session = fresh_session();
        // Owned message appended WITHOUT surface membership (compacted away).
        let mut first = InstructionsProjection::new(&session);
        let baseline = first.project(dir.path()).unwrap();
        session
            .append_with(
                "user/message",
                baseline.to_json().unwrap(),
                AppendOptions::default(),
            )
            .unwrap();

        let mut restarted = InstructionsProjection::new(&session);
        let resent = restarted
            .project(dir.path())
            .expect("off-surface tail resends");
        assert!(
            first_text(&resent).contains("rule one"),
            "off-surface history must be treated as cleared, not retained"
        );
    }

 // ── global layer / dedup / truncation notice ───────────

    #[test]
    fn global_layer_reads_home_and_marks_the_label() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".starriver")).unwrap();
        std::fs::write(
            home.path().join(".starriver").join("AGENTS.md"),
            "personal rules",
        )
        .unwrap();

        let file = global_layer(Some(home.path())).expect("global file");
        assert_eq!(file.label, "AGENTS.md (global)");
        assert_eq!(file.content, "personal rules");
        assert!(global_layer(None).is_none(), "unset HOME → no global layer");

        let empty = tempfile::tempdir().unwrap();
        assert!(global_layer(Some(empty.path())).is_none());
    }

    #[test]
    fn layered_discovery_puts_global_first_without_cross_layer_dedup() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".starriver")).unwrap();
        std::fs::write(
            home.path().join(".starriver").join("AGENTS.md"),
            "personal rules",
        )
        .unwrap();
        let project = tempfile::tempdir().unwrap();
        touch(project.path(), "AGENTS.md", "personal rules"); // same content, other scope

        let files = discover_layered(Some(home.path()), project.path());
        let labels: Vec<&str> = files.iter().map(|f| f.label.as_str()).collect();
        assert_eq!(
            labels,
            vec!["AGENTS.md (global)", "AGENTS.md"],
            "global first; dedup never crosses layers"
        );
    }

    #[test]
    fn same_directory_duplicates_collapse_to_the_earliest() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "AGENTS.md", "shared body");
        touch(dir.path(), "CLAUDE.md", "shared body");
        touch(dir.path(), "STARRIVER.md", "shared body");
        touch(dir.path(), "CLAUDE.local.md", "distinct overlay");

        let files = discover(dir.path());
        let labels: Vec<&str> = files.iter().map(|f| f.label.as_str()).collect();
        assert_eq!(
            labels,
            vec!["AGENTS.md", "CLAUDE.local.md"],
            "only the earliest same-content candidate survives"
        );
    }

    #[test]
    fn over_budget_composite_carries_the_truncation_notice() {
        let mut files = Vec::new();
        for index in 0..2_000 {
            files.push(InstructionFile {
                label: format!("f{index}.md"),
                content: "block ".repeat(40),
            });
        }
        let composed = compose_budgeted(&files);
        assert!(composed.len() <= MAX_BYTES, "notice is budgeted");
        assert!(
            composed.ends_with(TRUNCATION_NOTICE),
            "the model must see why content is missing"
        );
        // Under budget: untouched.
        assert_eq!(compose_budgeted(&[]), "");
        let small = compose(&[InstructionFile {
            label: "AGENTS.md".into(),
            content: "tiny".into(),
        }]);
        assert_eq!(
            compose_budgeted(&[InstructionFile {
                label: "AGENTS.md".into(),
                content: "tiny".into(),
            }]),
            small
        );
    }

    #[test]
    fn explicit_budget_override_trims_and_passes_through() {
        let files = vec![
            InstructionFile {
                label: "AGENTS.md".into(),
                content: "first block".into(),
            },
            InstructionFile {
                label: "CLAUDE.md".into(),
                content: "second block".into(),
            },
        ];
        // Tight budget (above the notice floor): the composite keeps the
        // fitting head and the notice, total within budget.
        let composed = compose_budgeted_with(&files, 100);
        assert!(composed.len() <= 100, "explicit budget is enforced");
        assert!(composed.ends_with(TRUNCATION_NOTICE));
        // Generous budget: untouched.
        assert_eq!(compose_budgeted_with(&files, MAX_BYTES), compose(&files));
    }
}
