//! UI-facing projection of agent activity. The loop assembles complete
//! assistant messages before logging them, so consumers render whole blocks
//! instead of buffering deltas.

use serde::{Deserialize, Serialize};

/// One UI-visible agent activity fact. Serialized with a `type` tag and
/// camelCase fields; this shape is the TUI/host contract. Every variant
/// carries `version` (current `UiEvent::VERSION`) so consumers can reject
/// unknown wire shapes; legacy events without the field default to 1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum UiEvent {
    SessionStarted {
        #[serde(default = "UiEvent::version")]
        version: u32,
        session_id: String,
    },
    /// One turn opened inside the session (`turn/start`). This is the
    /// between-turns marker: queue Send Now / FIFO auto-claim runs follow-up
    /// turns inside the SAME execute (the agent's own driver claims them),
    /// so there is no second SessionStarted to re-arm live UIs — the busy
    /// state, the queue-card materialization and per-turn buffering all key
    /// on this event. Wire name and field case deliberately match the
    /// legacy AgentEvent convention every client already consumes
    /// (`"TaskStarted"` + snake_case `session_id`), not the enum's camelCase
    /// default.
    #[serde(rename = "TaskStarted")]
    TaskStarted {
        #[serde(default = "UiEvent::version")]
        version: u32,
        #[serde(rename = "session_id")]
        session_id: String,
    },
    AssistantText {
        #[serde(default = "UiEvent::version")]
        version: u32,
        session_id: String,
        text: String,
    },
    /// Incremental assistant-text delta, forwarded from `assistant/chunk`
    /// (`text-delta`) so live UIs can typewriter the reply; the finished
    /// `AssistantText` remains the authoritative block (delta-fed renderers
    /// replace their streamed content with it to dedupe).
    AssistantTextDelta {
        #[serde(default = "UiEvent::version")]
        version: u32,
        session_id: String,
        text: String,
    },
    /// Incremental reasoning delta, forwarded from `assistant/chunk` so live
    /// UIs can stream the thinking bubble instead of waiting for the finished
    /// message (`ThinkingText`).
    ThinkingDelta {
        #[serde(default = "UiEvent::version")]
        version: u32,
        session_id: String,
        text: String,
    },
    ThinkingText {
        #[serde(default = "UiEvent::version")]
        version: u32,
        session_id: String,
        text: String,
    },
    ToolCall {
        #[serde(default = "UiEvent::version")]
        version: u32,
        session_id: String,
        id: String,
        name: String,
        /// JSON arguments text of the call (e.g. `{"command":"ls"}`) when the
        /// source event carried it. Lets UIs show the actual command instead
        /// of the bare tool name; absent on legacy/replayed events.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input: Option<String>,
    },
    ToolResult {
        #[serde(default = "UiEvent::version")]
        version: u32,
        session_id: String,
        id: String,
        ok: bool,
        output: String,
 /// Opaque, tool-owned presentation metadata (`meta` parity):
        /// persisted on the `tool/result` event and forwarded for
        /// client-side card narrowing; never sent to the model. Absent on
        /// failures, sub-dispatches, and tools without a presenter.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        meta: Option<serde_json::Value>,
    },
    TurnEnd {
        #[serde(default = "UiEvent::version")]
        version: u32,
        session_id: String,
        reason: String,
        summary: String,
    },
    TurnError {
        #[serde(default = "UiEvent::version")]
        version: u32,
        session_id: String,
        error: String,
    },
    Cancelled {
        #[serde(default = "UiEvent::version")]
        version: u32,
        session_id: String,
    },
    ApprovalRequested {
        #[serde(default = "UiEvent::version")]
        version: u32,
        session_id: String,
        request_id: String,
        tool_name: String,
        call_id: Option<String>,
        reason: Option<String>,
        /// JSON arguments text of the pending call, so approval UIs can show
        /// the actual command; absent when the source request had none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        params: Option<String>,
    },
    /// Workspace-instructions budget state (WI-35): emitted from the
    /// instructions owned-message when the composite was truncated to fit.
    InstructionsBudget {
        #[serde(default = "UiEvent::version")]
        version: u32,
        session_id: String,
        truncated: bool,
        total_sections: u32,
        budget_bytes: u64,
        composed_bytes: u64,
    },
    /// Agent-initiated question for the user (WI-67 user-questions slice):
    /// emitted from `interaction/question` session events; the UI answers
    /// through `interaction.respond`, which resolves the pending ask.
    QuestionAsked {
        #[serde(default = "UiEvent::version")]
        version: u32,
        session_id: String,
        question_id: String,
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        options: Vec<String>,
        /// Seconds the ask will wait before abandoning (0 = unknown).
        #[serde(default)]
        timeout_secs: u64,
    },
    /// Plan review requested by the `exit_plan_mode` tool (plan fold,
    /// plan-mode parity): the UI presents `plan` for review through the Plan
    /// confirmation card and answers via `plan.reviewDecision`.
    PlanReviewRequested {
        #[serde(default = "UiEvent::version")]
        version: u32,
        session_id: String,
        request_id: String,
        /// The complete markdown plan the model submitted for review.
        plan: String,
    },
    /// A plan review reached a decision (approve, reject with feedback, or
    /// abandonment/timeout) — every surface closes its pending card.
    PlanReviewDecided {
        #[serde(default = "UiEvent::version")]
        version: u32,
        session_id: String,
        request_id: String,
        approved: bool,
        /// Review expired without a decision (plan-review bridge timeout) —
        /// lets remote surfaces (iPhone companion) tell an abandonment apart
        /// from an explicit rejection.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        abandoned: bool,
        /// Free-text feedback from the user on rejection; absent on approval
        /// and abandonment.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        feedback: Option<String>,
    },
    SandboxModeChanged {
        #[serde(default = "UiEvent::version")]
        version: u32,
        session_id: String,
        mode: String,
    },
    /// Per-step token usage reported with each assistant message. Mirrors the
    /// provider `usage` object: cache split (DeepSeek `cached_tokens`) and
    /// `reasoning_tokens` are optional — providers that don't report them
    /// leave them unset.
    Usage {
        #[serde(default = "UiEvent::version")]
        version: u32,
        session_id: String,
        input_tokens: u64,
        output_tokens: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_read_tokens: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_write_tokens: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_tokens: Option<u64>,
    },
 /// Context compaction checkpoint landed (`compaction/summary` parity):
    /// the summarized span was replaced by a checkpoint on the model surface.
    /// `summary` is the checkpoint Markdown for the fold disclosure; the
    /// shadowed seq range identifies what was folded (log seqs, not message
    /// counts).
    Compaction {
        #[serde(default = "UiEvent::version")]
        version: u32,
        session_id: String,
        summary: String,
        shadowed_start: u64,
        shadowed_end: u64,
    },
    /// Goal-loop state broadcast (WI-97/BUG-87): emitted whenever the goal's
    /// accounted tokens change or its status flips, so UIs render the live
    /// budget instead of a stale snapshot. `status` is the host's lowercase
    /// goal status string (active/paused/budgetLimited/complete).
    GoalStatus {
        #[serde(default = "UiEvent::version")]
        version: u32,
        session_id: String,
        objective: String,
        status: String,
        tokens_used: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token_budget: Option<u64>,
    },
    /// 逐字节实时一致（开放回合快照流）：开放回合的 trace 投影切片。
    /// `entries` 与 `session.trace` 条目同构（同一投影函数的输出）——客户端
    /// 以替换语义重渲染该回合（turnIndex 0-based，== 本地回合数表示全新回合），
    /// 实时视图与落定视图由构造保证一致。epoch 在 compaction/fork/回滚时递增，
    /// 不一致即触发客户端全量重拉。
    TraceSnapshot {
        #[serde(default = "UiEvent::version")]
        version: u32,
        session_id: String,
        epoch: u64,
        turn_index: u32,
        open: bool,
        via_remote: bool,
        entries: serde_json::Value,
    },
}

impl UiEvent {
    /// Current wire version; consumers reject unknown versions.
    pub const VERSION: u32 = 1;

    /// Default for the `version` field so legacy events (predating versioning)
    /// still parse as version 1.
    pub fn version() -> u32 {
        UiEvent::VERSION
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_event_round_trips_with_camel_case_tags() {
        let cases = vec![
            UiEvent::SessionStarted {
                version: UiEvent::VERSION,
                session_id: "s1".into(),
            },
            UiEvent::AssistantText {
                version: UiEvent::VERSION,
                session_id: "s1".into(),
                text: "hi".into(),
            },
            UiEvent::ThinkingText {
                version: UiEvent::VERSION,
                session_id: "s1".into(),
                text: "想".into(),
            },
            UiEvent::ThinkingDelta {
                version: UiEvent::VERSION,
                session_id: "s1".into(),
                text: "考".into(),
            },
            UiEvent::ToolCall {
                version: UiEvent::VERSION,
                session_id: "s1".into(),
                id: "c1".into(),
                name: "bash".into(),
                input: Some(r#"{"command":"ls"}"#.into()),
            },
            UiEvent::ToolResult {
                version: UiEvent::VERSION,
                session_id: "s1".into(),
                id: "c1".into(),
                ok: true,
                output: "out".into(),
                meta: None,
            },
            UiEvent::TurnEnd {
                version: UiEvent::VERSION,
                session_id: "s1".into(),
                reason: "completed".into(),
                summary: "done".into(),
            },
            UiEvent::TurnError {
                version: UiEvent::VERSION,
                session_id: "s1".into(),
                error: "boom".into(),
            },
            UiEvent::Cancelled {
                version: UiEvent::VERSION,
                session_id: "s1".into(),
            },
            UiEvent::ApprovalRequested {
                version: UiEvent::VERSION,
                session_id: "s1".into(),
                request_id: "r1".into(),
                tool_name: "bash".into(),
                call_id: None,
                reason: Some("needs human".into()),
                params: Some(r#"{"command":"ls"}"#.into()),
            },
            UiEvent::SandboxModeChanged {
                version: UiEvent::VERSION,
                session_id: "s1".into(),
                mode: "workspace-write".into(),
            },
            UiEvent::Usage {
                version: UiEvent::VERSION,
                session_id: "s1".into(),
                input_tokens: 120,
                output_tokens: 45,
                cache_read_tokens: Some(96),
                cache_write_tokens: None,
                reasoning_tokens: Some(20),
            },
        ];
        for event in cases {
            let json = serde_json::to_value(&event).unwrap();
            let back: UiEvent = serde_json::from_value(json).unwrap();
            assert_eq!(back, event);
        }
    }

    #[test]
    fn ui_event_tags_are_stable() {
        let json = serde_json::to_value(UiEvent::SessionStarted {
            version: UiEvent::VERSION,
            session_id: "s1".into(),
        })
        .unwrap();
        assert_eq!(json["type"], "sessionStarted");
        let json = serde_json::to_value(UiEvent::ToolResult {
            version: UiEvent::VERSION,
            session_id: "s1".into(),
            id: "c1".into(),
            ok: false,
            output: "err".into(),
            meta: None,
        })
        .unwrap();
        assert_eq!(json["type"], "toolResult");
        assert_eq!(json["ok"], false);
    }

    #[test]
    fn ui_event_serializes_version_envelope() {
        let event = UiEvent::ApprovalRequested {
            version: UiEvent::VERSION,
            session_id: "s1".into(),
            request_id: "r1".into(),
            tool_name: "bash".into(),
            call_id: None,
            reason: Some("needs human".into()),
            params: None,
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["version"], 1);
        assert_eq!(json["type"], "approvalRequested");
        assert_eq!(json["requestId"], "r1");
        // None params is omitted rather than serialized as null.
        assert!(json.get("params").is_none());
        let back: UiEvent = serde_json::from_value(json).unwrap();
        assert_eq!(back, event);
    }

    #[test]
    fn instructions_budget_wire_shape_is_locked() {
        let event = UiEvent::InstructionsBudget {
            version: UiEvent::VERSION,
            session_id: "s1".into(),
            truncated: true,
            total_sections: 3,
            budget_bytes: 65_536,
            composed_bytes: 90_000,
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "instructionsBudget");
        assert_eq!(json["truncated"], true);
        assert_eq!(json["totalSections"], 3);
        assert_eq!(json["budgetBytes"], 65_536);
        assert_eq!(json["composedBytes"], 90_000);
        let back: UiEvent = serde_json::from_value(json).unwrap();
        assert_eq!(back, event);
    }

    #[test]
    fn question_asked_wire_shape_is_locked() {
        let event = UiEvent::QuestionAsked {
            version: UiEvent::VERSION,
            session_id: "s1".into(),
            question_id: "q1".into(),
            text: "which db?".into(),
            options: vec!["postgres".into(), "sqlite".into()],
            timeout_secs: 120,
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "questionAsked");
        assert_eq!(json["questionId"], "q1");
        assert_eq!(json["text"], "which db?");
        assert_eq!(json["options"][0], "postgres");
        assert_eq!(json["timeoutSecs"], 120);
        // Empty options drop out rather than serialize as [].
        let minimal = UiEvent::QuestionAsked {
            version: UiEvent::VERSION,
            session_id: "s1".into(),
            question_id: "q1".into(),
            text: "which db?".into(),
            options: vec![],
            timeout_secs: 0,
        };
        let json = serde_json::to_value(&minimal).unwrap();
        assert!(json.get("options").is_none());
        assert_eq!(json["timeoutSecs"], 0);
        let back: UiEvent = serde_json::from_value(serde_json::to_value(&event).unwrap()).unwrap();
        assert_eq!(back, event);
    }

    /// Plan fold contract lock: `planReviewRequested` / `planReviewDecided`
 /// wire shapes, field for field (plan-mode parity).
    #[test]
    fn plan_review_wire_shapes_are_locked() {
        let requested = UiEvent::PlanReviewRequested {
            version: UiEvent::VERSION,
            session_id: "sess-1".into(),
            request_id: "pr-1".into(),
            plan: "# Plan\n\n1. do it".into(),
        };
        let json = serde_json::to_value(&requested).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "type": "planReviewRequested",
                "version": 1,
                "sessionId": "sess-1",
                "requestId": "pr-1",
                "plan": "# Plan\n\n1. do it",
            })
        );
        let back: UiEvent = serde_json::from_value(json).unwrap();
        assert_eq!(back, requested);

        let decided = UiEvent::PlanReviewDecided {
            version: UiEvent::VERSION,
            session_id: "sess-1".into(),
            request_id: "pr-1".into(),
            approved: false,
            abandoned: false,
            feedback: Some("add tests".into()),
        };
        let json = serde_json::to_value(&decided).unwrap();
        assert_eq!(json["type"], "planReviewDecided");
        assert_eq!(json["approved"], false);
        assert_eq!(json["feedback"], "add tests");
        let back: UiEvent = serde_json::from_value(json).unwrap();
        assert_eq!(back, decided);
        // Approval omits feedback rather than serializing null.
        let approved = UiEvent::PlanReviewDecided {
            version: UiEvent::VERSION,
            session_id: "sess-1".into(),
            request_id: "pr-2".into(),
            approved: true,
            abandoned: false,
            feedback: None,
        };
        let json = serde_json::to_value(&approved).unwrap();
        assert!(json.get("feedback").is_none());
    }

    /// WI-17 contract lock: the full `approvalRequested` wire shape, field
    /// for field. Every consumer (TUI, desktop renderer, remote UIs) codes
    /// against this exact JSON; changing any key is a protocol change.
    #[test]
    fn approval_requested_wire_shape_is_locked() {
        let event = UiEvent::ApprovalRequested {
            version: UiEvent::VERSION,
            session_id: "sess-1".into(),
            request_id: "ap-1".into(),
            tool_name: "bash".into(),
            call_id: Some("call-9".into()),
            reason: Some("tool \"bash\" requires approval".into()),
            params: Some("{\"command\":\"ls\"}".into()),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "type": "approvalRequested",
                "version": 1,
                "sessionId": "sess-1",
                "requestId": "ap-1",
                "toolName": "bash",
                "callId": "call-9",
                "reason": "tool \"bash\" requires approval",
                "params": "{\"command\":\"ls\"}",
            })
        );
        // Round-trip must restore the exact event (no field loss).
        let back: UiEvent = serde_json::from_value(json).unwrap();
        assert_eq!(back, event);
    }

    #[test]
    fn legacy_ui_event_without_version_still_parses() {
        let json = serde_json::json!({
            "type": "cancelled",
            "sessionId": "s1",
        });
        let back: UiEvent = serde_json::from_value(json).unwrap();
        assert_eq!(
            back,
            UiEvent::Cancelled {
                version: UiEvent::VERSION,
                session_id: "s1".into()
            }
        );
    }
}
