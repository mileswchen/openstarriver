// Context compaction — simplified (no hardware dependencies)

use starriver_kernel::types::{ContentBlock, Message, MessageMeta, Role};

/// Tier of the progressive context-compression pipeline.
pub fn compact_messages(messages: &[Message], keep_recent: usize) -> Vec<Message> {
    let system_idx = messages.iter().position(|m| matches!(m.role, Role::System));
    let mut result = vec![];
    if let Some(idx) = system_idx {
        result.push(messages[idx].clone());
    }
    let start = messages.len().saturating_sub(keep_recent);
    let tail = &messages[start..];
    let tail = trim_dangling_tool_results(tail);
    for msg in tail {
        result.push(msg.clone());
    }
    result
}

pub fn force_truncate_messages(messages: &[Message], max_keep: usize) -> Vec<Message> {
    compact_messages(messages, std::cmp::min(max_keep, 5))
}

/// Drops leading `role=Tool` messages (orphaned tool results). Used by the keep-recent
/// window and by history reload: when a truncation/history LIMIT cuts between an
/// assistant tool_calls and its result, the kept window starts with a bare tool message
/// — most chat APIs reject that ("Messages with role 'tool' must be a response to a
/// preceding message with 'tool_calls'").
pub(crate) fn trim_dangling_tool_results(tail: &[Message]) -> &[Message] {
    let mut start = 0;
    while start < tail.len() {
        let m = &tail[start];
        let is_tool = m.role == Role::Tool
            && m.content
                .iter()
                .all(|b| matches!(b, ContentBlock::ToolResult { .. }));
        if is_tool {
            start += 1;
        } else {
            break;
        }
    }
    &tail[start..]
}

/// Level 0 — lossless field truncation of old tool-result messages.
///
/// Cuts `ToolResult.content` to `max_tool_chars` (keeping the head, where structure
/// and key facts usually live) and appends a truncation marker. Never deletes a whole
/// message and never touches anchors. Only tool messages before `prune_before_idx`
/// are eligible, so the recent tail stays intact.
pub fn prune_tool_results(
    messages: &[Message],
    anchors: &std::collections::HashSet<usize>,
    max_tool_chars: usize,
    prune_before_idx: usize,
) -> Vec<Message> {
    messages
        .iter()
        .enumerate()
        .map(|(i, m)| {
            if anchors.contains(&i) || m.role != Role::Tool || i >= prune_before_idx {
                return m.clone();
            }
            let mut out = m.clone();
            for block in &mut out.content {
                if let ContentBlock::ToolResult {
                    content, is_error, ..
                } = block
                {
                    // BUG-36: one metric governs both the guard and the
                    // cut — a char budget checked in bytes made multi-byte
                    // content either grow (nothing cut, marker appended) or
                    // blow the byte budget.
                    if content.chars().count() > max_tool_chars {
                        let head: String = content.chars().take(max_tool_chars).collect();
                        let marker = if *is_error { " [error]" } else { "" };
                        *content = format!("{head}{marker}… [truncated by compaction]");
                    }
                }
            }
            out
        })
        .collect()
}

/// Level 2 — safe eviction of CLOSED tool-call pairs.
///
/// Drops the pair `(assistant ToolUse message, its ToolResult message)` together when
/// it lies entirely before `evict_before_idx` — i.e. old, already absorbed by later
/// turns. The pair is removed as a UNIT: most chat APIs require a ToolResult to follow
/// every ToolUse, so keeping one half would break the tool-call sequence. Never touches
/// anchors or dangling (unclosed) pairs.
pub fn compact_messages_anchored(
    messages: &[Message],
    anchors: &std::collections::HashSet<usize>,
    keep_recent: usize,
    pairs: &[(usize, usize)],
) -> Vec<Message> {
    use std::collections::HashSet;
    let n = messages.len();
    let mut selected: HashSet<usize> = anchors.clone();
    // recent non-anchor tail
    let mut recent: Vec<usize> = (0..n)
        .filter(|i| !anchors.contains(i))
        .rev()
        .take(keep_recent)
        .collect();
    recent.sort_unstable();
    for i in recent {
        selected.insert(i);
    }
    // Pair closure: if one half is selected, keep the other so the API
    // sequence stays valid. Iterate to a fixpoint — parallel tool calls share
    // one assistant index ((1,2),(1,3)), and a single pass can close (1,3)
    // after (1,2) was already visited, dropping the other tool result (the
    // BUG-25 class; /compact then PERSISTS the broken sequence).
    loop {
        let before = selected.len();
        for (a, t) in pairs {
            if selected.contains(a) {
                selected.insert(*t);
            }
            if selected.contains(t) {
                selected.insert(*a);
            }
        }
        if selected.len() == before {
            break;
        }
    }
    (0..n)
        .filter(|i| selected.contains(i))
        .map(|i| messages[i].clone())
        .collect()
}

/// Phase-1 tiered executor: applies the lossless actions for the chosen level and all
/// lower tiers, re-deriving anchors/pairs after each mutation (indices shift).
///
/// Level 1 (LLM structured summarization) is phase 2 and a no-op here — a Level-1
/// decision still reduces via Level 0 pruning + Level 2 eviction.
pub fn estimate_msg_tokens(msg: &Message) -> usize {
    let mut total = 0usize;
    for block in &msg.content {
        let text = match block {
            ContentBlock::Text { text } => text.as_str(),
            ContentBlock::Thinking { text } => text.as_str(),
            ContentBlock::ToolResult { content, .. } => content.as_str(),
            ContentBlock::ToolUse { input, .. } => {
                total += serde_json::to_string(input)
                    .map(|s| s.len() / 2)
                    .unwrap_or(0);
                continue;
            }
        };
        let divisor = match msg.role {
            Role::System => 1.8,
            Role::User => 1.5,
            Role::Assistant => 2.0,
            Role::Tool => 2.2,
        };
        total += (text.len() as f64 / divisor) as usize;
    }
    total
}

/// Estimate tokens across a message list (sum of `estimate_msg_tokens`).
pub fn estimate_messages_tokens(messages: &[Message]) -> usize {
    messages.iter().map(estimate_msg_tokens).sum()
}

/// Estimate tokens (simple version: 4 chars ≈ 1 token) — kept for plain-text callers.
pub fn estimate_tokens(text: &str) -> usize {
    text.len() / 4
}

/// Collect message indices that must NEVER be compressed or dropped.
///
/// Anchor set:
/// 1. System prompts (complete preservation)
/// 2. First user message (initial goal — never summarize into a summary)
/// 3. Recent user messages (last `recent_user_keep`)
/// 4. Unclosed tool-call messages: an assistant message carrying a `ToolUse` whose id
///    has no matching `ToolResult` anywhere → the causal chain is still open, so the
///    whole assistant message is protected.
///
/// Closed pairs (ToolUse ↔ ToolResult) are NOT anchored here — they are handled by
/// `pair_tool_calls` so eviction can decide to keep or drop BOTH sides together.
pub fn collect_anchors(
    messages: &[Message],
    recent_user_keep: usize,
) -> std::collections::HashSet<usize> {
    use std::collections::HashSet;
    let mut anchors = HashSet::new();
    let mut first_user: Option<usize> = None;

    for (i, m) in messages.iter().enumerate() {
        match m.role {
            Role::System => {
                anchors.insert(i);
            }
            Role::User if first_user.is_none() => first_user = Some(i),
            _ => {}
        }
    }
    if let Some(f) = first_user {
        anchors.insert(f);
    }

    // Recent user messages — walk backwards, keep last N user slots.
    let user_indices: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| matches!(m.role, Role::User))
        .map(|(i, _)| i)
        .collect();
    for idx in user_indices.iter().rev().take(recent_user_keep) {
        anchors.insert(*idx);
    }

    // Unclosed tool-call messages — same FIFO topology as eviction/closure: any assistant
    // message carrying a ToolUse with no matched ToolResult is a live causal chain.
    let topo = pair_tool_calls(messages);
    for i in topo.dangling_uses {
        anchors.insert(i);
    }
    anchors
}

/// Complete tool-call topology: closed pairs, unclosed assistant uses, and orphan results.
///
/// - `closed`: `(assistant_msg_idx, tool_result_msg_idx)` pairs — both sides of a completed
///   call. Eviction must treat these as a unit: drop/keep both, never split.
/// - `dangling_uses`: assistant messages carrying at least one ToolUse with no matched
///   ToolResult. Live causal chains — anchored and never evictable.
/// - `orphan_results`: ToolResult messages with no matched ToolUse. Truly unmatched after
///   FIFO pairing (unlike last-wins, which silently orphaned the first duplicate result).
#[derive(Debug, Default)]
pub struct ToolCallTopology {
    pub closed: Vec<(usize, usize)>,
    pub dangling_uses: Vec<usize>,
    pub orphan_results: Vec<usize>,
}

/// Split tool-call topology by TIME LOCALITY, not global id uniqueness.
///
/// Each ToolUse consumes the first unconsumed ToolResult with the same id that appears
/// AFTER it (chat APIs require a ToolResult to follow its ToolUse). A repeated id across
/// messages therefore pairs FIFO: use₁→result₁, use₂→result₂. With last-wins (a HashMap
/// overwrite), every duplicate use collapsed onto the LAST result and the first result
/// became an orphan that L2/L3 could drop on its own, splitting the pair. FIFO pairing
/// keeps every pair whole and reports only genuinely-unmatched halves as dangling/orphan.
pub fn pair_tool_calls(messages: &[Message]) -> ToolCallTopology {
    use std::collections::{HashMap, HashSet};
    let mut uses_by_id: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut results_by_id: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, m) in messages.iter().enumerate() {
        for block in &m.content {
            match block {
                ContentBlock::ToolUse { id, .. } => {
                    uses_by_id.entry(id.as_str()).or_default().push(i)
                }
                ContentBlock::ToolResult { tool_use_id, .. } => results_by_id
                    .entry(tool_use_id.as_str())
                    .or_default()
                    .push(i),
                _ => {}
            }
        }
    }

    let mut consumed_results: HashSet<usize> = HashSet::new();
    let mut closed = Vec::new();
    let mut dangling_uses = Vec::new();
    for (i, m) in messages.iter().enumerate() {
        if m.role != Role::Assistant {
            continue;
        }
        for block in &m.content {
            if let ContentBlock::ToolUse { id, .. } = block {
                let matched = results_by_id.get(id.as_str()).and_then(|res| {
                    res.iter()
                        .find(|r| **r > i && !consumed_results.contains(*r))
                        .copied()
                });
                match matched {
                    Some(r) => {
                        closed.push((i, r));
                        consumed_results.insert(r);
                    }
                    None => dangling_uses.push(i),
                }
            }
        }
    }

    let mut orphan_results = Vec::new();
    for res in results_by_id.values() {
        for r in res {
            if !consumed_results.contains(r) {
                orphan_results.push(*r);
            }
        }
    }
    dangling_uses.sort_unstable();
    dangling_uses.dedup();
    orphan_results.sort_unstable();
    ToolCallTopology {
        closed,
        dangling_uses,
        orphan_results,
    }
}

// =============================================================================
// Phase 2: LLM structured summarization (L1 / L3 enhancement pass, §5.3.4–5.3.5)
// =============================================================================

/// Structured summary that the LLM must produce for a message block (§5.3.4).
///
/// The summarizer call replaces a range of messages with ONE `Role::System`
/// message whose body is the JSON serialization of this struct. Everything the
/// agent needs to continue — goals, decisions, open threads — must survive here.
pub fn assign_msg_ids(messages: &mut [Message]) {
    // BUG-37: pre-scan EVERY existing id — a pass-local `used` set only saw
    // earlier messages, so an auto id could equal a pre-set id on a LATER
    // message and both kept the same value.
    let mut used: std::collections::HashSet<String> = messages
        .iter()
        .filter_map(|m| {
            m.meta
                .as_ref()
                .and_then(|mt| mt.msg_id.clone())
                .filter(|id| !id.is_empty())
        })
        .collect();
    for (i, m) in messages.iter_mut().enumerate() {
        let existing = m.meta.as_ref().and_then(|mt| mt.msg_id.clone());
        match existing {
            Some(id) if !id.is_empty() => {
                used.insert(id);
            }
            _ => {
                let mut id = format!("m{i}");
                let mut n = 0;
                while used.contains(&id) {
                    id = format!("m{i}_{n}");
                    n += 1;
                }
                used.insert(id.clone());
                match &mut m.meta {
                    Some(mt) => mt.msg_id = Some(id),
                    None => {
                        m.meta = Some(MessageMeta {
                            msg_id: Some(id),
                            ..Default::default()
                        })
                    }
                }
            }
        }
    }
}

/// Select the message ranges to summarize.
///
/// - L1: the single EARLIEST run of consecutive non-anchored, non-compressed
///   messages (before the recent tail).
/// - L3: every non-anchored, non-compressed run outside the recent tail.
///
/// Ranges never include anchors, System messages, already-compressed messages,
/// or the recent tail. Tool-call pairs are kept whole: a range never contains
/// half of a closed `(ToolUse, ToolResult)` pair — the partner is pulled in when
/// unprotected, or the edge message is itself excluded when the partner is
/// protected (fixpoint below).
#[cfg(test)]
mod tests {
    // NOTE: keep this glob — cfg(test) code is invisible to `cargo fix --lib`,
    // which would otherwise strip this import again and break the tests.
    #[allow(unused_imports)]
    use super::*;
    use starriver_kernel::types::{ContentBlock, Message, Role};

    fn sys(s: &str) -> Message {
        Message::system(s)
    }
    fn user(s: &str) -> Message {
        Message::user(s)
    }
    fn asst(s: &str) -> Message {
        Message::assistant(s)
    }
    fn tool_msg(id: &str) -> Message {
        Message::tool_result(id, "ok", false)
    }

    #[test]
    fn estimate_tokens_is_len_div_4() {
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abc"), 0);
        assert_eq!(estimate_tokens("abcdefgh"), 2);
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn estimate_msg_tokens_zero_content() {
        // Empty message → 0 tokens regardless of role
        assert_eq!(
            estimate_msg_tokens(&Message {
                meta: None,
                role: Role::User,
                content: vec![]
            }),
            0
        );
    }

    #[test]
    fn trim_dangling_tool_results_drops_leading_tool_messages() {
        // Reproduces the resume-window boundary bug: the history LIMIT cut between an
        // assistant tool_calls and its result, so the kept window opens with a bare
        // role='tool' message — OpenAI rejects that. Leading tool messages must go.
        let msgs = vec![
            tool_msg("call_cut"),
            tool_msg("call_cut2"),
            user("next task"),
            asst("ok"),
        ];
        let trimmed = trim_dangling_tool_results(&msgs);
        assert_eq!(trimmed.len(), 2);
        assert!(matches!(trimmed[0].role, Role::User));
        assert!(matches!(trimmed[1].role, Role::Assistant));
    }

    #[test]
    fn trim_dangling_tool_results_keeps_regular_history() {
        let msgs = vec![user("hi"), asst("hello"), tool_msg("c1")];
        let trimmed = trim_dangling_tool_results(&msgs);
        assert_eq!(trimmed.len(), 3);
    }

    #[test]
    fn estimate_msg_tokens_counts_text_and_thinking() {
        let m = Message {
            meta: None,
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "x".repeat(20),
                }, // 20 / 2.0 = 10
                ContentBlock::Thinking {
                    text: "y".repeat(20),
                }, // 20 / 2.0 = 10
            ],
        };
        assert_eq!(estimate_msg_tokens(&m), 20);
    }

    #[test]
    fn estimate_msg_tokens_counts_tool_result_content() {
        // ToolResult content previously missed by the loop's Text-only estimate.
        // 220 / 2.2 in f64 truncates to 99 (not 100) — assert the actual value.
        let m = Message {
            meta: None,
            role: Role::Tool,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: "x".repeat(220),
                is_error: false,
            }],
        };
        assert_eq!(estimate_msg_tokens(&m), 99);
    }

    #[test]
    fn estimate_msg_tokens_tool_use_json_args() {
        // `{"path":"abcdefghij"}` serializes to 21 bytes → 21/2 = 10
        let m = Message {
            meta: None,
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "t1".into(),
                name: "read".into(),
                input: serde_json::json!({"path": "abcdefghij"}),
            }],
        };
        assert_eq!(estimate_msg_tokens(&m), 10);
    }

    #[test]
    fn estimate_msg_tokens_role_weights_differ() {
        // Same 300 chars: user (Chinese) denser than tool (JSON-ish).
        let user = Message {
            meta: None,
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "a".repeat(300),
            }],
        };
        let tool = Message {
            meta: None,
            role: Role::Tool,
            content: vec![ContentBlock::Text {
                text: "a".repeat(300),
            }],
        };
        let u = estimate_msg_tokens(&user);
        let t = estimate_msg_tokens(&tool);
        assert_eq!(u, 200); // 300 / 1.5
        assert_eq!(t, 136); // 300 / 2.2 = 136.36 → 136
        assert!(u > t, "user role should estimate higher density");
    }

    #[test]
    fn estimate_messages_tokens_sums_all() {
        let msgs = vec![
            Message {
                meta: None,
                role: Role::System,
                content: vec![ContentBlock::Text {
                    text: "a".repeat(18),
                }],
            }, // 10
            Message {
                meta: None,
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "b".repeat(15),
                }],
            }, // 10
        ];
        assert_eq!(estimate_messages_tokens(&msgs), 20);
    }

    // ── Anchor collection ──

    fn asst_tool_use(id: &str) -> Message {
        Message {
            meta: None,
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: id.to_string(),
                name: "read".into(),
                input: serde_json::json!({"path": "f.txt"}),
            }],
        }
    }

    #[test]
    fn anchors_keep_system_first_user_and_recent_users() {
        let msgs = vec![
            sys("s"),
            user("goal"),
            asst("a1"),
            user("u2"),
            asst("a2"),
            user("u3"),
        ];
        let anchors = collect_anchors(&msgs, 2);
        assert!(anchors.contains(&0), "system must be anchored"); // sys
        assert!(
            anchors.contains(&1),
            "first user (initial goal) must be anchored"
        ); // goal
        assert!(anchors.contains(&3), "recent user u2 must be anchored");
        assert!(anchors.contains(&5), "recent user u3 must be anchored");
        assert!(!anchors.contains(&2), "assistant a1 must not be anchored");
        assert!(!anchors.contains(&4), "assistant a2 must not be anchored");
    }

    #[test]
    fn anchors_without_user_keep_no_user() {
        let msgs = vec![user("u1"), user("u2"), user("u3")];
        let anchors = collect_anchors(&msgs, 0);
        // no system, recent_user_keep=0 → only first user anchored
        assert_eq!(anchors.len(), 1);
        assert!(anchors.contains(&0));
    }

    #[test]
    fn anchors_protect_unclosed_tool_call() {
        // assistant carries ToolUse "t1" but no ToolResult anywhere → message must be anchored
        let msgs = vec![user("goal"), asst_tool_use("t1")];
        let anchors = collect_anchors(&msgs, 1);
        assert!(
            anchors.contains(&1),
            "dangling tool-call assistant must be anchored"
        );
    }

    #[test]
    fn anchors_do_not_protect_closed_tool_pair() {
        let msgs = vec![user("goal"), asst_tool_use("t1"), tool_msg("t1")];
        let anchors = collect_anchors(&msgs, 1);
        assert!(
            !anchors.contains(&1),
            "closed tool-call assistant must NOT be anchored here"
        );
        assert!(!anchors.contains(&2), "tool result must NOT be anchored");
    }

    // ── Tool-call pair topology ──

    #[test]
    fn tool_pairs_closed_pair_matches_assistant_and_result() {
        let msgs = vec![user("goal"), asst_tool_use("t1"), tool_msg("t1")];
        let topo = pair_tool_calls(&msgs);
        assert_eq!(topo.closed, vec![(1, 2)]);
        assert!(topo.dangling_uses.is_empty());
        assert!(topo.orphan_results.is_empty());
    }

    #[test]
    fn tool_pairs_dangling_when_no_result() {
        let msgs = vec![user("goal"), asst_tool_use("t1")];
        let topo = pair_tool_calls(&msgs);
        assert!(topo.closed.is_empty());
        assert_eq!(topo.dangling_uses, vec![1]);
        assert!(topo.orphan_results.is_empty());
    }

    #[test]
    fn tool_pairs_multiple_closed_and_dangling_mixed() {
        // t1 closed (result exists), t2 dangling (no result), t3 closed
        let msgs = vec![
            user("goal"),
            asst_tool_use("t1"),
            tool_msg("t1"),
            asst_tool_use("t2"),
            asst_tool_use("t3"),
            tool_msg("t3"),
        ];
        let topo = pair_tool_calls(&msgs);
        assert_eq!(topo.closed.len(), 2);
        assert!(topo.closed.contains(&(1, 2)), "t1 pair: {:?}", topo.closed);
        assert!(topo.closed.contains(&(4, 5)), "t3 pair: {:?}", topo.closed);
        assert_eq!(topo.dangling_uses, vec![3], "t2 assistant is dangling");
        assert!(topo.orphan_results.is_empty());
    }

    #[test]
    fn tool_pairs_plain_text_messages_ignored() {
        let msgs = vec![sys("s"), user("u"), asst("a"), tool_msg("t1")];
        let topo = pair_tool_calls(&msgs);
        assert!(topo.closed.is_empty());
        assert!(
            topo.dangling_uses.is_empty(),
            "no assistant ToolUse blocks, nothing to pair"
        );
        assert_eq!(
            topo.orphan_results,
            vec![3],
            "unmatched tool result is reported as orphan"
        );
    }

    #[test]
    fn compact_keeps_system_plus_recent_n() {
        let msgs = vec![sys("s"), user("u1"), asst("a1"), user("u2"), asst("a2")];
        let out = compact_messages(&msgs, 2);
        assert_eq!(out.len(), 3);
        assert!(matches!(out[0].role, Role::System));
        assert!(matches!(out[1].role, Role::User)); // u2
        assert!(matches!(out[2].role, Role::Assistant)); // a2
                                                         // verify the last two are kept
        match &out[1].content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "u2"),
            _ => panic!("expected Text block"),
        }
    }

    #[test]
    fn compact_no_system_keeps_only_recent_n() {
        let msgs = vec![user("u1"), asst("a1"), user("u2")];
        let out = compact_messages(&msgs, 2);
        assert_eq!(out.len(), 2);
        // without system, take last keep_recent items; start=1 → tail=[a1,u2]
        assert!(matches!(out[0].role, Role::Assistant));
        assert!(matches!(out[1].role, Role::User));
    }

    #[test]
    fn compact_trims_dangling_tool_results_from_tail_head() {
        // system + u1 + tool_result + u2 + a2, keep_recent=3 → tail starts from tool_result
        let msgs = vec![sys("s"), user("u1"), tool_msg("t1"), user("u2"), asst("a2")];
        let out = compact_messages(&msgs, 3);
        // system + (tool_result trimmed) + u2 + a2 = 3 messages
        assert_eq!(out.len(), 3);
        assert!(matches!(out[0].role, Role::System));
        assert!(matches!(out[1].role, Role::User)); // u2
        assert!(matches!(out[2].role, Role::Assistant)); // a2
    }

    #[test]
    fn compact_keep_recent_exceeds_len_keeps_all() {
        let msgs = vec![sys("s"), user("u1")];
        let out = compact_messages(&msgs, 10);
        // when keep_recent > len, tail covers all; system is explicitly pushed then copied again by tail (known behavior)
        assert_eq!(out.len(), 3);
        assert!(matches!(out[0].role, Role::System));
    }

    #[test]
    fn force_truncate_caps_at_five() {
        let msgs: Vec<Message> = (0..8).map(|i| user(format!("u{i}").as_str())).collect();
        // max_keep=10 → got keep_recent=min(10,5)=5
        let out = force_truncate_messages(&msgs, 10);
        assert_eq!(out.len(), 5);
        // max_keep=2 → keep_recent=min(2,5)=2
        let out = force_truncate_messages(&msgs, 2);
        assert_eq!(out.len(), 2);
    }

    fn tool_result_msg(id: &str, text: &str, is_error: bool) -> Message {
        Message {
            role: Role::Tool,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: id.to_string(),
                content: text.to_string(),
                is_error,
            }],
            meta: None,
        }
    }

    #[test]
    fn prune_tool_results_truncates_old_tool_output() {
        let big = "x".repeat(5000);
        let msgs = vec![user("goal"), tool_result_msg("t1", &big, false), asst("a1")];
        let anchors = collect_anchors(&msgs, 1); // goal anchored, tool/assistant not
        let out = prune_tool_results(&msgs, &anchors, 2000, usize::MAX);
        // t1 content truncated
        let t = &out[1];
        if let ContentBlock::ToolResult { content, .. } = &t.content[0] {
            assert!(content.contains("truncated by compaction"));
            assert!(content.len() < big.len());
        } else {
            panic!("expected ToolResult");
        }
        // goal message untouched
        match &out[0].content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "goal"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn prune_tool_results_skips_recent_tail() {
        let big = "x".repeat(5000);
        let msgs = vec![user("goal"), asst("a1"), tool_result_msg("t1", &big, false)];
        let anchors = collect_anchors(&msgs, 1);
        // prune_before_idx = 2 → the tool message at idx 2 is in the recent tail → untouched
        let out = prune_tool_results(&msgs, &anchors, 2000, 2);
        if let ContentBlock::ToolResult { content, .. } = &out[2].content[0] {
            assert_eq!(content.len(), 5000, "recent tail must not be truncated");
        } else {
            panic!("expected ToolResult");
        }
    }

    /// BUG-36 regression: the over-budget guard measured BYTES while the cut
    /// kept CHARS — multi-byte content under the char budget but over the
    /// byte budget was "truncated" to itself plus a marker (growing the
    /// content), and content over the char budget but multi-byte-light blew
    /// the byte budget. One metric must govern both.
    #[test]
    fn prune_tool_results_uses_one_metric_for_guard_and_cut() {
        // 3 chars / 9 bytes, max_tool_chars = 5 (chars): under the char
        // budget → must stay untouched (no marker, no growth).
        let text = "\u{4f60}".repeat(3);
        assert_eq!(text.chars().count(), 3);
        assert_eq!(text.len(), 9);
        let msgs = vec![user("goal"), tool_result_msg("t1", &text, false)];
        let anchors = collect_anchors(&msgs, 1);
        let out = prune_tool_results(&msgs, &anchors, 5, usize::MAX);
        if let ContentBlock::ToolResult { content, .. } = &out[1].content[0] {
            assert_eq!(
                content, &text,
                "within the char budget there is nothing to cut (got {content:?})"
            );
            assert!(!content.contains("truncated by compaction"));
        } else {
            panic!("expected ToolResult");
        }

        // 6 chars / 18 bytes, max = 5 chars: the cut keeps 5 chars and the
        // marker follows — the kept head must respect the char budget.
        let text = "\u{4f60}".repeat(6);
        let msgs = vec![user("goal"), tool_result_msg("t1", &text, false)];
        let out = prune_tool_results(&msgs, &anchors, 5, usize::MAX);
        if let ContentBlock::ToolResult { content, .. } = &out[1].content[0] {
            assert!(content.contains("truncated by compaction"));
            let head_chars = content
                .split('…')
                .next()
                .map(|h| h.chars().count())
                .unwrap_or_default();
            assert!(
                head_chars <= 5,
                "kept head must respect the char budget (head {head_chars} chars, content {content:?})"
            );
            assert!(
                content.chars().count() < text.chars().count() + 30,
                "truncated payload stays on the order of the budget, not the original"
            );
        } else {
            panic!("expected ToolResult");
        }
    }

    #[test]
    fn compact_anchored_keeps_anchors_plus_recent() {
        let msgs = vec![
            sys("s"),
            user("goal"),
            asst("a1"),
            user("u2"),
            asst("a2"),
            user("u3"),
            asst("a3"),
        ];
        let anchors = collect_anchors(&msgs, 2);
        let out = compact_messages_anchored(&msgs, &anchors, 3, &[]);
        // anchors: sys(0), goal(1), u2(3), u3(5)  + recent 3 non-anchor: a3(6), a2(4), a1(2)
        // → all 7 kept (K=3 ≥ non-anchor count)
        assert_eq!(out.len(), 7);
        // order preserved
        assert!(matches!(out[0].role, Role::System));
        assert!(matches!(out[6].role, Role::Assistant));
    }

    #[test]
    fn compact_anchored_drops_middle_when_k_small() {
        let msgs = vec![
            sys("s"),
            user("goal"),
            asst("a1"),
            user("u2"),
            asst("a2"),
            user("u3"),
            asst("a3"),
            user("u4"),
            asst("a4"),
        ];
        let anchors = collect_anchors(&msgs, 1);
        // anchors: sys(0), goal(1), recent user u4(7). keep_recent=1 → +a4(8)
        let out = compact_messages_anchored(&msgs, &anchors, 1, &[]);
        let roles: Vec<String> = out
            .iter()
            .map(|m| format!("{:?}", m.role).to_lowercase())
            .collect();
        assert_eq!(
            roles,
            vec!["system", "user", "user", "assistant"],
            "sys + goal + recent user(u4) + recent assistant(a4): {roles:?}"
        );
    }

    #[test]
    fn compact_anchored_keeps_tool_pair_closure() {
        // recent K selects asst(t1) but not its tool result → pair closure adds tool result
        let msgs = vec![
            user("goal"),
            asst_tool_use("t1"),
            tool_msg("t1"),
            asst("old1"),
            asst("old2"),
            asst("old3"),
            asst("tail"),
        ];
        let anchors = collect_anchors(&msgs, 1); // goal only
        let pairs = pair_tool_calls(&msgs).closed; // closed (1,2)
        let out = compact_messages_anchored(&msgs, &anchors, 3, &pairs);
        // anchors {0}; recent non-anchor tail 3: old2, old3, tail (indices 4,5,6)
        // pair (1,2): neither half selected → dropped together
        let roles: Vec<String> = out
            .iter()
            .map(|m| format!("{:?}", m.role).to_lowercase())
            .collect();
        assert!(
            !roles.contains(&"tool".into()),
            "dropped pair must not leave a dangling tool result: {roles:?}"
        );
        assert_eq!(out.len(), 4, "goal + 3 recent: {roles:?}");
    }

    #[test]
    fn compact_anchored_parallel_tool_calls_close_to_fixpoint() {
        // Round-10 regression: one assistant with TWO parallel tool uses —
        // (1,2),(1,3). Recent tail selects tool(t2) only; the single-pass
        // closure re-added the assistant but never revisited (1,2), so
        // tool(t1) was dropped while its use survived (persisted corruption
        // on the /compact path).
        // One assistant turn carrying TWO parallel tool uses (index 1),
        // results at 2 and 3 — pairs [(1,2),(1,3)].
        let msgs = vec![
            user("goal"),
            asst_tool_use("t1"),
            tool_msg("t1"),
            tool_msg("t2"),
            asst("tail"),
        ];
        let anchors = collect_anchors(&msgs, 1);
        let pairs = vec![(1usize, 2usize), (1usize, 3usize)];
        // keep_recent 2 selects non-anchor indices 4,3 → {0,3,4}; the
        // fixpoint closure pulls in assistant@1 via (1,3), then REVISITS
        // (1,2) to save result@2 — a single pass left it dropped.
        let out = compact_messages_anchored(&msgs, &anchors, 2, &pairs);
        let roles: Vec<String> = out
            .iter()
            .map(|m| format!("{:?}", m.role).to_lowercase())
            .collect();
        assert_eq!(
            out.len(),
            msgs.len(),
            "parallel-use closure must keep both results: {roles:?}"
        );
    }

    #[test]
    fn estimate_messages_tokens_empty_list_is_zero() {
        assert_eq!(estimate_messages_tokens(&[]), 0);
    }

    #[test]
    fn estimate_msg_tokens_short_text_floors_at_zero() {
        // 1 char under a role divisor → f64 truncation floors to 0 (never negative)
        let sys1 = Message {
            meta: None,
            role: Role::System,
            content: vec![ContentBlock::Text { text: "a".into() }],
        };
        assert_eq!(estimate_msg_tokens(&sys1), 0); // 1/1.8
        let usr1 = Message {
            meta: None,
            role: Role::User,
            content: vec![ContentBlock::Text { text: "a".into() }],
        };
        assert_eq!(estimate_msg_tokens(&usr1), 0); // 1/1.5
    }

    #[test]
    fn estimate_msg_tokens_tool_use_empty_input() {
        let m = Message {
            meta: None,
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "t1".into(),
                name: "read".into(),
                input: serde_json::json!({}),
            }],
        };
        assert_eq!(estimate_msg_tokens(&m), 1); // "{}" = 2 bytes → 2/2
    }

    #[test]
    fn estimate_msg_tokens_mixed_text_and_tool_use() {
        let m = Message {
            meta: None,
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "a".repeat(20),
                }, // 20/2.0 = 10
                ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "read".into(),
                    input: serde_json::json!({"path": "f.txt"}),
                }, // 16 bytes /2 = 8
            ],
        };
        assert_eq!(estimate_msg_tokens(&m), 18);
    }

    #[test]
    fn anchors_empty_messages() {
        assert!(collect_anchors(&[], 2).is_empty());
    }

    #[test]
    fn anchors_system_only_keeps_system() {
        let anchors = collect_anchors(&[sys("s"), sys("s2")], 2);
        assert_eq!(anchors.len(), 2);
        assert!(anchors.contains(&0) && anchors.contains(&1));
    }

    #[test]
    fn anchors_recent_user_keep_exceeding_user_count() {
        // keep saturates: 2 users, keep 10 → both anchored (goal + u2)
        let msgs = vec![sys("s"), user("goal"), asst("a1"), user("u2")];
        let anchors = collect_anchors(&msgs, 10);
        assert!(anchors.contains(&1), "first user (initial goal)");
        assert!(anchors.contains(&3), "recent user u2");
        assert!(!anchors.contains(&2), "assistant not anchored");
    }

    #[test]
    fn anchors_partial_tool_closure_anchors_whole_message() {
        // assistant carries t1 (closed) + t2 (dangling) → ANY unclosed use anchors the msg
        let mut asst_partial = asst_tool_use("t1");
        asst_partial.content.push(ContentBlock::ToolUse {
            id: "t2".into(),
            name: "read".into(),
            input: serde_json::json!({}),
        });
        let msgs = vec![user("goal"), asst_partial, tool_msg("t1")];
        let anchors = collect_anchors(&msgs, 1);
        assert!(
            anchors.contains(&1),
            "partially-closed assistant must be anchored"
        );
        assert!(
            !anchors.contains(&2),
            "closed tool result not anchored on its own"
        );
    }

    #[test]
    fn tool_pairs_empty_messages() {
        let topo = pair_tool_calls(&[]);
        assert!(
            topo.closed.is_empty()
                && topo.dangling_uses.is_empty()
                && topo.orphan_results.is_empty()
        );
    }

    #[test]
    fn tool_pairs_orphan_tool_result_reported() {
        // a ToolResult with no assistant ToolUse: not closed, not dangling, but reported orphan
        let msgs = vec![user("goal"), tool_msg("ghost")];
        let topo = pair_tool_calls(&msgs);
        assert!(topo.closed.is_empty());
        assert!(topo.dangling_uses.is_empty());
        assert_eq!(topo.orphan_results, vec![1]);
    }

    #[test]
    fn tool_pairs_duplicate_results_fifo() {
        // duplicate tool_use_id across two ToolResult messages → the use pairs with the
        // FIRST result that follows it; the second is a true orphan (not silently hidden).
        let msgs = vec![
            user("goal"),
            asst_tool_use("t1"),
            tool_msg("t1"),
            tool_msg("t1"),
        ];
        let topo = pair_tool_calls(&msgs);
        assert_eq!(topo.closed, vec![(1, 2)], "FIFO pairs use→first result");
        assert_eq!(topo.orphan_results, vec![3], "second result is the orphan");
        assert!(topo.dangling_uses.is_empty());
    }

    #[test]
    fn tool_pairs_partially_closed_assistant_pairs_closed_and_dangles_rest() {
        // asst carries t1 (result exists) + t2 (no result): FIFO pairs t1 precisely and
        // reports ONLY the unmatched t2 use as dangling — while the message-level anchor
        // (anchors_partial_tool_closure_anchors_whole_message) still protects the whole msg.
        let mut asst_mixed = asst_tool_use("t1");
        asst_mixed.content.push(ContentBlock::ToolUse {
            id: "t2".into(),
            name: "grep".into(),
            input: serde_json::json!({"q": "x"}),
        });
        let msgs = vec![user("goal"), asst_mixed, tool_msg("t1")];
        let topo = pair_tool_calls(&msgs);
        assert_eq!(topo.closed, vec![(1, 2)], "t1 is closed precisely");
        assert_eq!(topo.dangling_uses, vec![1], "t2 is the dangling use");
        assert!(topo.orphan_results.is_empty());
    }

    // ── FIFO pairing: repeated tool_use_id across messages ──

    #[test]
    fn tool_pairs_fifo_pairs_duplicate_uses_and_results() {
        // The exact scenario that broke last-wins: each use pairs with its OWN result.
        let msgs = vec![
            user("goal"),
            asst_tool_use("t1"),
            tool_msg("t1"),
            asst_tool_use("t1"),
            tool_msg("t1"),
        ];
        let topo = pair_tool_calls(&msgs);
        assert_eq!(topo.closed, vec![(1, 2), (3, 4)], "one-to-one FIFO pairing");
        assert!(topo.dangling_uses.is_empty());
        assert!(topo.orphan_results.is_empty());
    }

    #[test]
    fn tool_pairs_more_uses_than_results() {
        let msgs = vec![
            user("goal"),
            asst_tool_use("t1"),
            tool_msg("t1"),
            asst_tool_use("t1"),
        ];
        let topo = pair_tool_calls(&msgs);
        assert_eq!(topo.closed, vec![(1, 2)], "first use pairs");
        assert_eq!(
            topo.dangling_uses,
            vec![3],
            "second use has nothing left to pair with"
        );
        assert!(topo.orphan_results.is_empty());
    }

    #[test]
    fn tool_pairs_result_before_use_is_dangling() {
        // Time locality: a result that precedes its use never satisfies it (API requires
        // ToolResult to follow ToolUse), so the use is dangling and the result orphaned.
        let msgs = vec![user("goal"), tool_msg("t1"), asst_tool_use("t1")];
        let topo = pair_tool_calls(&msgs);
        assert!(topo.closed.is_empty());
        assert_eq!(topo.dangling_uses, vec![2]);
        assert_eq!(topo.orphan_results, vec![1]);
    }

    #[test]
    fn tool_pairs_fifo_consumes_results_in_order_across_messages() {
        // Three uses, two results, same id: FIFO consumes results in order; the third use
        // dangles. Never two uses pointing at the same result.
        let msgs = vec![
            user("goal"),
            asst_tool_use("t1"),
            tool_msg("t1"),
            asst_tool_use("t1"),
            tool_msg("t1"),
            asst_tool_use("t1"),
        ];
        let topo = pair_tool_calls(&msgs);
        assert_eq!(topo.closed, vec![(1, 2), (3, 4)]);
        assert_eq!(topo.dangling_uses, vec![5]);
        assert!(topo.orphan_results.is_empty());
    }

    #[test]
    fn anchors_anchor_repeated_id_extra_use() {
        // The repeated-id extra use (no result left) is a live causal chain → anchored;
        // the first, fully-closed use is NOT anchored (it belongs to a closed pair).
        let msgs = vec![
            user("goal"),
            asst_tool_use("t1"),
            tool_msg("t1"),
            asst_tool_use("t1"),
        ];
        let anchors = collect_anchors(&msgs, 1);
        assert!(
            anchors.contains(&3),
            "unmatched second use must be anchored"
        );
        assert!(
            !anchors.contains(&1),
            "closed first use must not be anchored"
        );
    }

    #[test]
    fn prune_tool_results_preserves_error_flag() {
        let big = "e".repeat(5000);
        let msgs = vec![user("goal"), tool_result_msg("t1", &big, true)];
        let anchors = collect_anchors(&msgs, 1);
        let out = prune_tool_results(&msgs, &anchors, 2000, usize::MAX);
        if let ContentBlock::ToolResult {
            content, is_error, ..
        } = &out[1].content[0]
        {
            assert!(*is_error, "error flag must be preserved");
            assert!(
                content.contains("[error]"),
                "error marker must survive: {content}"
            );
            assert!(content.contains("truncated by compaction"));
        } else {
            panic!("expected ToolResult");
        }
    }

    #[test]
    fn prune_tool_results_within_budget_untouched() {
        let msgs = vec![
            user("goal"),
            tool_result_msg("t1", &"x".repeat(2000), false),
        ];
        let anchors = collect_anchors(&msgs, 1);
        let out = prune_tool_results(&msgs, &anchors, 2000, usize::MAX);
        if let ContentBlock::ToolResult { content, .. } = &out[1].content[0] {
            assert_eq!(content.len(), 2000, "exactly at budget → untouched");
            assert!(!content.contains("truncated"));
        } else {
            panic!("expected ToolResult");
        }
    }

    #[test]
    fn prune_tool_results_anchor_message_untouched() {
        let msgs = vec![
            user("goal"),
            tool_result_msg("t1", &"a".repeat(5000), false),
            asst("a1"),
        ];
        let anchors: std::collections::HashSet<usize> = [0usize, 1].into_iter().collect(); // anchor the tool msg too
        let out = prune_tool_results(&msgs, &anchors, 2000, usize::MAX);
        if let ContentBlock::ToolResult { content, .. } = &out[1].content[0] {
            assert_eq!(
                content.len(),
                5000,
                "anchored tool result must not be truncated"
            );
        } else {
            panic!("expected ToolResult");
        }
    }

    #[test]
    fn prune_tool_results_ignores_non_tool_messages() {
        let big = "b".repeat(5000);
        let msgs = vec![user(&big), asst(&big)];
        let anchors = collect_anchors(&msgs, 1);
        let out = prune_tool_results(&msgs, &anchors, 2000, usize::MAX);
        assert_eq!(out.len(), 2, "prune never deletes messages");
        match &out[0].content[0] {
            ContentBlock::Text { text } => assert_eq!(text.len(), 5000),
            _ => panic!(),
        }
        match &out[1].content[0] {
            ContentBlock::Text { text } => assert_eq!(text.len(), 5000),
            _ => panic!(),
        }
    }

    #[test]
    fn compact_anchored_empty_messages() {
        let out = compact_messages_anchored(&[], &Default::default(), 3, &[]);
        assert!(out.is_empty());
    }

    #[test]
    fn compact_anchored_all_anchored_keeps_everything() {
        let msgs = vec![sys("s"), user("goal"), asst("a1")];
        let anchors: std::collections::HashSet<usize> = (0..3).collect();
        let out = compact_messages_anchored(&msgs, &anchors, 0, &[]);
        assert_eq!(out.len(), 3, "fully anchored → all kept regardless of K");
        assert!(matches!(out[0].role, Role::System));
    }

    #[test]
    fn compact_anchored_keep_recent_zero_keeps_only_anchors() {
        let msgs = vec![user("goal"), asst("a1"), user("u2"), asst("a2")];
        let anchors: std::collections::HashSet<usize> = [0usize].into_iter().collect();
        let out = compact_messages_anchored(&msgs, &anchors, 0, &[]);
        assert_eq!(out.len(), 1, "K=0 → only the initial goal survives");
        match &out[0].content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "goal"),
            _ => panic!(),
        }
    }

    #[test]
    fn compact_anchored_pair_closure_adds_assistant_back() {
        // recent tail picks the tool result@2 but not its assistant@1 → closure re-adds asst@1
        let msgs = vec![
            user("goal"),
            asst_tool_use("t1"),
            tool_msg("t1"),
            asst("x"),
            asst("y"),
        ];
        let anchors = collect_anchors(&msgs, 1); // {0}
        let pairs = pair_tool_calls(&msgs).closed; // closed (1,2)
        let out = compact_messages_anchored(&msgs, &anchors, 3, &pairs);
        assert_eq!(
            out.len(),
            5,
            "asst@1 must be added back by pair closure: {:?}",
            out.iter().map(|m| m.role.clone()).collect::<Vec<_>>()
        );
        let tool_uses = out
            .iter()
            .filter(|m| {
                m.content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::ToolUse { .. }))
            })
            .count();
        let tool_results = out
            .iter()
            .filter(|m| {
                m.content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
            })
            .count();
        assert_eq!(tool_uses, 1, "ToolUse kept with its result");
        assert_eq!(tool_results, 1, "ToolResult kept with its ToolUse");
    }

    #[test]
    fn assign_msg_ids_is_stable_and_idempotent() {
        let mut msgs = vec![sys("s"), user("u0"), asst("a1")];
        assign_msg_ids(&mut msgs);
        let ids: Vec<String> = msgs
            .iter()
            .map(|m| m.meta.as_ref().unwrap().msg_id.clone().unwrap())
            .collect();
        assert_eq!(ids, vec!["m0", "m1", "m2"]);
        // Idempotent: a second pass must not renumber or collide.
        assign_msg_ids(&mut msgs);
        let again: Vec<String> = msgs
            .iter()
            .map(|m| m.meta.as_ref().unwrap().msg_id.clone().unwrap())
            .collect();
        assert_eq!(ids, again);
    }

    #[test]
    fn assign_msg_ids_avoids_collision_with_existing() {
        // idx0 carries a pre-existing id that the auto id for idx1 would collide with.
        let mut msgs = vec![
            Message::user("a").with_meta(MessageMeta {
                msg_id: Some("m1".into()),
                ..Default::default()
            }),
            user("b"),
            user("c"),
        ];
        assign_msg_ids(&mut msgs);
        let ids: Vec<String> = msgs
            .iter()
            .map(|m| m.meta.as_ref().unwrap().msg_id.clone().unwrap())
            .collect();
        assert_eq!(ids, vec!["m1", "m1_0", "m2"]);
    }

    /// BUG-37 regression: the collision check only saw ids of EARLIER
    /// messages, so an auto id could equal a pre-set id on a LATER message —
    /// two messages sharing one id breaks the "never collide" contract the
    /// idempotency test above claims.
    #[test]
    fn assign_msg_ids_never_collides_with_later_existing_ids() {
        let mut msgs = vec![
            Message::user("a").with_meta(MessageMeta {
                msg_id: Some("m1".into()),
                ..Default::default()
            }),
            user("b"), // auto id picks "m1_0" after dodging the earlier "m1" …
            Message::user("c").with_meta(MessageMeta {
                msg_id: Some("m1_0".into()),
                ..Default::default()
            }),
        ];
        assign_msg_ids(&mut msgs);
        let ids: Vec<String> = msgs
            .iter()
            .map(|m| m.meta.as_ref().unwrap().msg_id.clone().unwrap())
            .collect();
        let unique: std::collections::HashSet<&String> = ids.iter().collect();
        assert_eq!(
            unique.len(),
            ids.len(),
            "ids must be unique across the whole list (got {ids:?})"
        );
    }

    #[test]
    fn assign_msg_ids_preserves_existing_id_when_no_collision() {
        // A pre-existing id that doesn't collide with any auto-assigned id must be
        // kept verbatim — the contract is "set once at construction, never changed".
        let mut msgs = vec![
            user("a").with_meta(MessageMeta {
                msg_id: Some("custom-42".into()),
                ..Default::default()
            }),
            user("b"),
        ];
        assign_msg_ids(&mut msgs);
        assert_eq!(
            msgs[0].meta.as_ref().unwrap().msg_id.as_deref(),
            Some("custom-42")
        );
        assert_eq!(msgs[1].meta.as_ref().unwrap().msg_id.as_deref(), Some("m1"));
    }

    #[test]
    fn assign_msg_ids_treats_empty_string_id_as_missing() {
        // `msg_id: Some("")` is indistinguishable from absent — must be re-assigned,
        // not left empty (a blank id would break source_msg_ids traceability).
        let mut msgs = vec![
            user("a").with_meta(MessageMeta {
                msg_id: Some(String::new()),
                ..Default::default()
            }),
            user("b"),
        ];
        assign_msg_ids(&mut msgs);
        let ids: Vec<String> = msgs
            .iter()
            .map(|m| m.meta.as_ref().unwrap().msg_id.clone().unwrap())
            .collect();
        assert_eq!(
            ids,
            vec!["m0".to_string(), "m1".to_string()],
            "empty id must be replaced with a fresh auto id"
        );
    }
}
