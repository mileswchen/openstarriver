//! Protocol method catalog (FPM P1): registers the host's typed methods into
//! the `starriver-protocol` registry, serving as the authoritative dispatch
//! source for the generic `invoke`. desktop (NAPI) and the daemon
//! (`starriverd`) share the same catalog; all handlers forward to existing
//! `starriver-host` methods — no business logic is rewritten at this layer.

use serde_json::{json, Value};
use starriver_host::protocol_json::skill_info_json;
use starriver_host::types;
use starriver_host::HostRuntime;
use starriver_protocol::permission::{
    PERM_AGENTS_READ, PERM_AGENTS_WRITE, PERM_APPROVAL_RESPOND, PERM_ATTACHMENTS_READ,
    PERM_ATTACHMENTS_WRITE, PERM_CREDENTIALS_READ, PERM_CREDENTIALS_WRITE, PERM_JOBS_READ,
    PERM_MARKET_READ, PERM_MCP_READ, PERM_MCP_WRITE, PERM_MEMORY_READ, PERM_MEMORY_WRITE,
    PERM_HOOKS_READ, PERM_HOOKS_WRITE, PERM_MODELS_READ, PERM_MODELS_WRITE, PERM_PLUGINS_READ,
    PERM_PLUGINS_WRITE, PERM_PROJECT_WRITE, PERM_REVIEW_START, PERM_SCHEDULED_READ,
    PERM_SCHEDULED_WRITE, PERM_SESSION_EXECUTE, PERM_SESSION_READ, PERM_SESSION_WRITE,
    PERM_SKILLS_READ, PERM_SKILLS_WRITE, PERM_TOOLS_READ, PERM_WORKSPACES_READ,
    PERM_WORKSPACES_WRITE,
};
use starriver_protocol::schema::{
    any_schema, array_schema, boolean_schema, integer_schema, number_schema, object, string_schema,
};
use starriver_protocol::wire::ApiError;
use starriver_protocol::{MethodCall, MethodRegistry, MethodSpec};

/// Alias for the desktop catalog's call context.
type HostCall<'a> = MethodCall<'a, HostRuntime>;

/// Builds the desktop protocol catalog.
pub fn build() -> MethodRegistry<HostRuntime> {
    MethodRegistry::new(vec![
        // ── Sessions ──
        session_list(),
        session_messages(),
        session_fork(),
        HostRuntime::__api_session_delete(),
        HostRuntime::__api_session_rename(),
        HostRuntime::__api_session_archive(),
        HostRuntime::__api_session_unarchive(),
        HostRuntime::__api_session_attachWorkspace(),
        session_list_archived(),
        HostRuntime::__api_session_execute(),
        session_cancel(),
        session_steer(),
        session_queue_list(),
        session_queue_add(),
        session_queue_update(),
        session_queue_remove(),
        session_queue_flush(),
        session_queue_flush_now(),
        terminal_list(),
        terminal_kill(),
        doctor_run(),
        review_structured(),
        goal_set(),
        goal_get(),
        goal_clear(),
        HostRuntime::__api_session_setExecutionMode(),
        HostRuntime::__api_session_getExecutionMode(),
        HostRuntime::__api_session_setApprovalMode(),
        HostRuntime::__api_session_setPermissionPreset(),
        HostRuntime::__api_session_getPermission(),
        settings_get(),
        settings_update(),
        HostRuntime::__api_session_confirmPlan(),
        session_search(),
        sessions_running(),
        session_timeline(),
        session_trace(),
        session_compact(),
        session_usage_method(),
        session_export(),
        approval_respond(),
        plan_review_decide(),
        interaction_respond(),
        HostRuntime::__api_approval_listRules(),
        HostRuntime::__api_approval_revokeRule(),
        agent_presets_list(),
        // ── Read-only catalogs ──
        tools_list(),
        agents_list(),
        agents_scan(),
        agents_create(),
        agents_export(),
        agents_bindings(),
        agents_bindings_set_active(),
        agents_bindings_set_reasoning_effort(),
        skills_list(),
        skills_scan(),
        skills_create(),
        skills_update(),
        HostRuntime::__api_skills_delete(),
        skills_set_always_on(),
        skills_usage(),
        hooks_list(),
        hooks_read(),
        hooks_write(),
        hooks_delete(),
        plugins_list(),
        plugins_install(),
        plugins_uninstall(),
        models_list(),
        models_providers_list(),
        models_add(),
        HostRuntime::__api_models_remove(),
        configurable_providers_list(),
        subagents_list(),
        subagents_set(),
        credentials_describe(),
        credentials_read(),
        credentials_list(),
        credentials_set_api_key(),
        credentials_unset(),
        memory_list(),
        memory_recall(),
        memory_upsert(),
        memory_delete(),
        memory_clear(),
        memory_ingest(),
        memory_status(),
        memory_export(),
        mcp_list(),
        mcp_add(),
        HostRuntime::__api_mcp_remove(),
        mcp_refresh(),
        mcp_oauth_connect(),
        mcp_oauth_disconnect(),
        mcp_test_server(),
        mcp_test_tool(),
        scheduled_list(),
        scheduled_add(),
        scheduled_update(),
        HostRuntime::__api_scheduled_remove(),
        scheduled_run_now(),
        jobs_list(),
        jobs_task_runs(),
        jobs_task_run_history(),
        market_list(),
        workspace_instructions(),
        workspace_diff(),
        workspace_diff_stat(),
        workspace_worktree_status(),
        project_agents_init(),
        review_start(),
        attachments_admit(),
        attachments_read(),
        feedback_record(),
        fs_suggest(),
        // ── Workspaces ──
        workspaces_list(),
        workspaces_create(),
        workspaces_rename(),
        workspaces_active(),
        HostRuntime::__api_workspaces_delete(),
        workspaces_activate(),
    ])
}

fn session_list() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "session.list",
        object(&[("workspaceId", string_schema())], &[]),
        array_schema(any_schema()),
        &[PERM_SESSION_READ],
        false,
        |call: HostCall<'_>| {
            let workspace_id = call.params.get("workspaceId").and_then(Value::as_str);
            let sessions = call.host.list_sessions(workspace_id.map(String::from));
            Ok(Value::Array(
                sessions.iter().map(session_info_json).collect(),
            ))
        },
    )
}

fn session_search() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "session.search",
        object(
            &[
                ("query", string_schema()),
                ("limit", integer_schema()),
            ],
            &["query"],
        ),
        any_schema(),
        &[PERM_SESSION_READ],
        false,
        |call: HostCall<'_>| {
            let query = required_string(&call.params, "query")?;
            let limit = optional_u32(&call.params, "limit")
                .map(|l| l.max(1) as usize)
                .unwrap_or(20);
            Ok(Value::Array(call.host.search_session_content(query, limit)))
        },
    )
}

fn session_messages() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "session.messages",
        object(
            &[
                ("sessionId", string_schema()),
                ("limit", integer_schema()),
                ("before", integer_schema()),
            ],
            &["sessionId"],
        ),
        any_schema(),
        &[PERM_SESSION_READ],
        false,
        |call: HostCall<'_>| {
            let session_id = required_string(&call.params, "sessionId")?;
            // WI-101: pagination — `limit` (default 50) caps the page;
            // `before` (a timeline seq) pages earlier. Without `limit`,
            // full history (legacy behavior).
            let limit = optional_u32(&call.params, "limit")
                .map(|l| (l.max(1)) as usize);
            let before = optional_u32(&call.params, "before").map(u64::from);
            match (limit, before) {
                (Some(limit), _) => {
                    let (messages, total, has_more) =
                        call.host.get_session_messages_paged(session_id, before, limit);
                    Ok(json!({
                        "messages": messages.iter().map(chat_message_json).collect::<Vec<_>>(),
                        "total": total,
                        "hasMore": has_more,
                    }))
                }
                (None, _) => {
                    let messages = call.host.get_session_messages(session_id);
                    Ok(Value::Array(
                        messages.iter().map(chat_message_json).collect(),
                    ))
                }
            }
        },
    )
}

fn session_fork() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "session.fork",
        object(
            &[
                ("sourceId", string_schema()),
                ("beforeMsgId", integer_schema()),
                ("task", string_schema()),
            ],
            &["sourceId", "beforeMsgId", "task"],
        ),
        any_schema(),
        &[PERM_SESSION_WRITE],
        false,
        |call: HostCall<'_>| {
            let source_id = required_string(&call.params, "sourceId")?;
            let before = required_i64(&call.params, "beforeMsgId")?;
            let task = required_string(&call.params, "task")?;
            let session = call
                .host
                .fork_session(source_id, before, task)
                .map_err(|e| ApiError::internal(e.to_string()))?;
            Ok(session_info_json(&session))
        },
    )
}

fn session_list_archived() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "session.listArchived",
        object(&[], &[]),
        array_schema(any_schema()),
        &[PERM_SESSION_READ],
        false,
        |call: HostCall<'_>| {
            let sessions = call.host.list_archived_sessions();
            Ok(Value::Array(
                sessions.iter().map(session_info_json).collect(),
            ))
        },
    )
}

fn session_cancel() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "session.cancel",
        object(&[("sessionId", string_schema())], &["sessionId"]),
        any_schema(),
        &[PERM_SESSION_EXECUTE],
        false,
        |call: HostCall<'_>| {
            let session_id = required_string(&call.params, "sessionId")?;
            let cancelled = call.host.cancel(session_id);
            Ok(json!({ "cancelled": cancelled }))
        },
    )
}

fn session_steer() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "session.steer",
        object(
            &[
                ("sessionId", string_schema()),
                ("task", string_schema()),
            ],
            &["sessionId", "task"],
        ),
        any_schema(),
        &[PERM_SESSION_EXECUTE],
        false,
        |call: HostCall<'_>| {
            let session_id = required_string(&call.params, "sessionId")?;
            let task = required_string(&call.params, "task")?;
            let steered = call.host.steer(session_id, task);
            Ok(json!({ "steered": steered }))
        },
    )
}

fn session_queue_list() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "session.queueList",
        object(&[("sessionId", string_schema())], &["sessionId"]),
        array_schema(object(&[("id", string_schema()), ("text", string_schema())], &[])),
        &[PERM_SESSION_EXECUTE],
        false,
        |call: HostCall<'_>| {
            let session_id = required_string(&call.params, "sessionId")?;
            Ok(Value::Array(
                call.host
                    .queue_list(session_id)
                    .into_iter()
                    .map(|(id, text)| json!({ "id": id, "text": text }))
                    .collect(),
            ))
        },
    )
}

fn session_queue_add() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "session.queueAdd",
        object(
            &[
                ("sessionId", string_schema()),
                ("text", string_schema()),
            ],
            &["sessionId", "text"],
        ),
        object(&[("id", string_schema())], &["id"]),
        &[PERM_SESSION_EXECUTE],
        false,
        |call: HostCall<'_>| {
            let session_id = required_string(&call.params, "sessionId")?;
            let text = required_string(&call.params, "text")?;
            let id = call
                .host
                .queue_add(session_id, text)
                .ok_or_else(|| {
                    ApiError::new("SESSION_NOT_FOUND", "session has no resident agent")
                })?;
            Ok(json!({ "id": id }))
        },
    )
}

fn session_queue_update() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "session.queueUpdate",
        object(
            &[
                ("sessionId", string_schema()),
                ("messageId", string_schema()),
                ("text", string_schema()),
            ],
            &["sessionId", "messageId", "text"],
        ),
        object(&[("updated", boolean_schema())], &["updated"]),
        &[PERM_SESSION_EXECUTE],
        false,
        |call: HostCall<'_>| {
            let session_id = required_string(&call.params, "sessionId")?;
            let message_id = required_string(&call.params, "messageId")?;
            let text = required_string(&call.params, "text")?;
            let updated = call.host.queue_update(session_id, message_id, text);
            Ok(json!({ "updated": updated }))
        },
    )
}

fn session_queue_remove() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "session.queueRemove",
        object(
            &[
                ("sessionId", string_schema()),
                ("messageId", string_schema()),
            ],
            &["sessionId", "messageId"],
        ),
        object(&[("removed", boolean_schema())], &["removed"]),
        &[PERM_SESSION_EXECUTE],
        false,
        |call: HostCall<'_>| {
            let session_id = required_string(&call.params, "sessionId")?;
            let message_id = required_string(&call.params, "messageId")?;
            let removed = call.host.queue_remove(session_id, message_id);
            Ok(json!({ "removed": removed }))
        },
    )
}

fn session_queue_flush() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "session.queueFlush",
        object(&[("sessionId", string_schema())], &["sessionId"]),
        object(&[("flushed", boolean_schema())], &["flushed"]),
        &[PERM_SESSION_EXECUTE],
        false,
        |call: HostCall<'_>| {
            let session_id = required_string(&call.params, "sessionId")?;
            let flushed = call.host.queue_flush(session_id);
            Ok(json!({ "flushed": flushed }))
        },
    )
}

/// Send Now: abort the live turn (if any) and hand the durable queue to the
/// driver atomically — cancel first (park + abort), flush second (unpark +
/// wake), so the queue always ends UNPARKED and the driver claims it. The
/// old renderer choreography (queueFlush then cancel across two IPC calls)
/// let the trailing cancel re-park the queue: Send Now did nothing and the
/// host round loop wedged waiting for a follow-up terminal that never came.
fn session_queue_flush_now() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "session.queueFlushNow",
        object(&[("sessionId", string_schema())], &["sessionId"]),
        object(&[("flushed", boolean_schema())], &["flushed"]),
        &[PERM_SESSION_EXECUTE],
        false,
        |call: HostCall<'_>| {
            let session_id = required_string(&call.params, "sessionId")?;
            let flushed = call.host.queue_flush_now(session_id);
            Ok(json!({ "flushed": flushed }))
        },
    )
}

/// WI-97: goal protocol surface.
fn goal_set() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "goal.set",
        object(
            &[
                ("sessionId", string_schema()),
                ("objective", string_schema()),
                ("tokenBudget", integer_schema()),
            ],
            &["sessionId", "objective"],
        ),
        any_schema(),
        &[PERM_SESSION_EXECUTE],
        false,
        |call: HostCall<'_>| {
            let session_id = required_string(&call.params, "sessionId")?;
            let objective = required_string(&call.params, "objective")?;
            let budget = optional_u32(&call.params, "tokenBudget").map(u64::from);
            call.host
                .set_goal(session_id, objective, budget)
                .map_err(|e| ApiError::internal(e.to_string()))?;
            Ok(json!({ "set": true }))
        },
    )
}

fn goal_get() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "goal.get",
        object(&[("sessionId", string_schema())], &["sessionId"]),
        any_schema(),
        &[PERM_SESSION_READ],
        false,
        |call: HostCall<'_>| {
            let session_id = required_string(&call.params, "sessionId")?;
            Ok(call.host.get_goal(&session_id).unwrap_or(Value::Null))
        },
    )
}

fn goal_clear() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "goal.clear",
        object(&[("sessionId", string_schema())], &["sessionId"]),
        any_schema(),
        &[PERM_SESSION_EXECUTE],
        false,
        |call: HostCall<'_>| {
            let session_id = required_string(&call.params, "sessionId")?;
            Ok(json!({ "cleared": call.host.clear_goal(&session_id) }))
        },
    )
}

/// WI-98: read-only doctor checks (config / model route / sandbox / disk / db).
fn doctor_run() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "doctor.run",
        object(&[], &[]),
        any_schema(),
        &[PERM_SESSION_READ],
        false,
        |call: HostCall<'_>| {
            let checks = call.host.doctor();
            Ok(json!({ "checks": checks }))
        },
    )
}

/// WI-96: structured review — resolve target, run child-thread review,
/// return findings.
fn review_structured() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "review.run",
        object(
            &[
                ("target", any_schema()),
                ("dimensions", any_schema()),
                ("cwd", string_schema()),
            ],
            &["target"],
        ),
        any_schema(),
        &[PERM_SESSION_EXECUTE],
        false,
        |call: HostCall<'_>| {
            let target_val = call.params.get("target").cloned().unwrap_or(Value::Null);
            let target: starriver_host::prompts::review::ReviewTarget =
                serde_json::from_value(target_val)
                    .map_err(|e| ApiError::invalid_params(format!("target: {e}")))?;
            let dimensions: Vec<String> = call
                .params
                .get("dimensions")
                .and_then(|d| d.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let cwd = call
                .params
                .get("cwd")
                .and_then(|c| c.as_str())
                .map(std::path::PathBuf::from);
            call.host
                .run_structured_review_sync(target, dimensions, cwd)
                .map_err(|e| ApiError::internal(format!("review failed: {e}")))
        },
    )
}

/// WI-93: background terminal panel — list active PTYs / kill one.
fn terminal_list() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "terminal.list",
        object(&[], &[]),
        any_schema(),
        &[PERM_SESSION_READ],
        false,
        |call: HostCall<'_>| {
            let terminals = call
                .host
                .terminals()
                .into_iter()
                .map(|(id, status, started_at_ms, command)| {
                    json!({
                        "id": id, "status": status, "startedAtMs": started_at_ms,
                        "command": command.map(|c| json!({
                            "startedAtMs": c.started_ms,
                            "finishedAtMs": c.finished_ms,
                            "exitCode": c.exit_code,
                            "text": c.text,
                        })),
                    })
                })
                .collect::<Vec<_>>();
            Ok(json!({ "terminals": terminals }))
        },
    )
}

fn terminal_kill() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "terminal.kill",
        object(&[("id", string_schema())], &["id"]),
        any_schema(),
        &[PERM_SESSION_EXECUTE],
        false,
        |call: HostCall<'_>| {
            let id = required_string(&call.params, "id")?;
            let killed = call.host.terminal_kill(id);
            Ok(json!({ "killed": killed }))
        },
    )
}

fn settings_get() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "settings.get",
        object(&[], &[]),
        any_schema(),
        &[PERM_SESSION_READ],
        false,
        |call: HostCall<'_>| Ok(call.host.get_settings_json()),
    )
}

fn settings_update() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "settings.update",
        object(&[("patch", json!({ "type": "object" }))], &["patch"]),
        any_schema(),
        &[PERM_MODELS_WRITE],
        false,
        |call: HostCall<'_>| {
            let patch = call.params.get("patch").cloned().unwrap_or(json!({}));
            call.host.update_settings(patch).map_err(|e| {
                ::starriver_protocol::ApiError::with_data(
                    "SETTINGS_INVALID",
                    e.to_string(),
                    ::serde_json::Value::Null,
                )
            })
        },
    )
}

/// WI-67 user-questions: the UI answers an agent-initiated question
/// (`ask_user` tool). Unknown/already-resolved ids report `resolved:false`
/// rather than erroring — late responders are told the outcome.
fn interaction_respond() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "interaction.respond",
        object(
            &[
                ("sessionId", string_schema()),
                ("questionId", string_schema()),
                ("answer", string_schema()),
            ],
            &["sessionId", "questionId", "answer"],
        ),
        any_schema(),
        &[PERM_APPROVAL_RESPOND],
        false,
        |call: HostCall<'_>| {
            let _session_id = required_string(&call.params, "sessionId")?;
            let question_id = required_string(&call.params, "questionId")?;
            let answer = required_string(&call.params, "answer")?;
            let resolved = call.host.respond_question(&question_id, &answer);
            Ok(json!({ "resolved": resolved }))
        },
    )
}

fn approval_respond() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "approval.respond",
        object(
            &[
                ("sessionId", string_schema()),
                ("allowed", boolean_schema()),
                ("remember", boolean_schema()),
            ],
            &["sessionId", "allowed"],
        ),
        any_schema(),
        &[PERM_APPROVAL_RESPOND],
        false,
        |call: HostCall<'_>| {
            let session_id = required_string(&call.params, "sessionId")?;
            let allowed = call.params["allowed"].as_bool().unwrap_or(false);
            let remember = call.params.get("remember").and_then(Value::as_bool);
            let responded = call.host.approve(session_id.clone(), allowed, remember);
            if !responded {
                // First-come-first-served (WI-16 arbitration): no pending
                // approval — already responded to by another client (the
                // final decision rides along so late responders converge) or
                // never requested.
                let mut data = json!({ "sessionId": session_id });
                if let Some(decision) = call.host.approval_final_decision(session_id.clone()) {
                    data["finalDecision"] = json!(decision.as_str());
                    data["finalOutcome"] = json!(decision.to_outcome().as_str());
                }
                return Err(ApiError::with_data(
                    "APPROVAL_NOT_PENDING",
                    format!(
                        "no pending approval for session `{session_id}` (already responded or no request)"
                    ),
                    data,
                ));
            }
            Ok(json!({}))
        },
    )
}

/// Plan fold (plan-mode parity): the UI's answer to a pending plan
/// review raised by `exit_plan_mode` (the Plan confirmation card).
/// `requestId` addresses a specific review; when absent the session's
/// latest pending review is answered. No pending review →
/// PLAN_REVIEW_NOT_PENDING (another client already answered, or none was
/// requested).
fn plan_review_decide() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "plan.reviewDecision",
        object(
            &[
                ("sessionId", string_schema()),
                ("requestId", string_schema()),
                ("approved", boolean_schema()),
                ("feedback", string_schema()),
            ],
            &["sessionId", "approved"],
        ),
        any_schema(),
        &[PERM_SESSION_WRITE],
        false,
        |call: HostCall<'_>| {
            let session_id = required_string(&call.params, "sessionId")?;
            let approved = call.params["approved"].as_bool().unwrap_or(false);
            let request_id = call
                .params
                .get("requestId")
                .and_then(Value::as_str)
                .map(String::from);
            let feedback = call
                .params
                .get("feedback")
                .and_then(Value::as_str)
                .map(String::from);
            if !call
                .host
                .respond_plan_review(session_id.clone(), request_id, approved, feedback)
            {
                return Err(ApiError::with_data(
                    "PLAN_REVIEW_NOT_PENDING",
                    format!(
                        "no pending plan review for session `{session_id}` (already answered or none requested)"
                    ),
                    json!({ "sessionId": session_id }),
                ));
            }
            Ok(json!({ "ok": true }))
        },
    )
}
/// WI-5 (ui-agent-preset parity): the selectable agent-preset roster.
/// `default` names the deployment default (settings `agent.default_agent_preset`);
/// `broken` rows render but cannot be selected (the run path rejects them).
fn agent_presets_list() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "agentPresets.list",
        object(&[], &[]),
        object(
            &[
                (
                    "presets",
                    array_schema(object(
                        &[
                            ("id", string_schema()),
                            ("agent", string_schema()),
                            ("name", string_schema()),
                            ("description", string_schema()),
                            ("source", string_schema()),
                            ("broken", string_schema()),
                            ("isDefault", boolean_schema()),
                        ],
                        &["id", "agent", "name", "description", "source", "isDefault"],
                    )),
                ),
                ("default", string_schema()),
            ],
            &["presets", "default"],
        ),
        &[PERM_AGENTS_READ],
        false,
        |call: HostCall<'_>| {
            let default = call
                .host
                .get_settings_json()
                .get("agent")
                .and_then(|agent| agent.get("default_agent_preset"))
                .and_then(Value::as_str)
                .unwrap_or("coding")
                .to_string();
            let presets: Vec<Value> = call
                .host
                .list_agent_presets()
                .into_iter()
                .map(|row| {
                    json!({
                        "id": row.id,
                        "agent": row.agent,
                        "name": row.name,
                        "description": row.description,
                        "source": row.source,
                        "broken": row.broken,
                        "isDefault": row.id == default,
                    })
                })
                .collect();
            Ok(json!({ "presets": presets, "default": default }))
        },
    )
}

fn tools_list() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "tools.list",
        object(&[], &[]),
        array_schema(any_schema()),
        &[PERM_TOOLS_READ],
        false,
        |call: HostCall<'_>| {
            let tools = call.host.list_tools();
            Ok(Value::Array(tools.iter().map(tool_info_json).collect()))
        },
    )
}

fn agents_list() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "agents.list",
        object(&[], &[]),
        array_schema(any_schema()),
        &[PERM_AGENTS_READ],
        false,
        |call: HostCall<'_>| {
            let agents = call.host.list_agents();
            Ok(Value::Array(agents.iter().map(agent_info_json).collect()))
        },
    )
}

/// BUG-96: backend source of truth for "which sessions hold the single
/// input slot". UIs sync their run state from here after a reload — the
/// renderer's own state dies with the page while the turn task lives on.
fn sessions_running() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "sessions.running",
        object(&[], &[]),
        array_schema(string_schema()),
        &[PERM_SESSION_READ],
        false,
        |call: HostCall<'_>| {
            Ok(Value::Array(
                call.host
                    .running_session_ids()
                    .into_iter()
                    .map(Value::String)
                    .collect(),
            ))
        },
    )
}

fn skills_list() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "skills.list",
        object(&[], &[]),
        array_schema(any_schema()),
        &[PERM_SKILLS_READ],
        false,
        |call: HostCall<'_>| {
            let skills = call.host.list_skills();
            Ok(Value::Array(skills.iter().map(skill_info_json).collect()))
        },
    )
}

fn plugins_list() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "plugins.list",
        object(&[], &[]),
        array_schema(any_schema()),
        &[PERM_PLUGINS_READ],
        false,
        |call: HostCall<'_>| {
            let plugins = call.host.list_plugins();
            Ok(Value::Array(
                plugins.iter().map(plugin_entry_json).collect(),
            ))
        },
    )
}

fn models_list() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "models.list",
        object(&[], &[]),
        array_schema(any_schema()),
        &[PERM_MODELS_READ],
        false,
        |call: HostCall<'_>| {
            let models = call.host.list_models();
            Ok(Value::Array(models.iter().map(model_entry_json).collect()))
        },
    )
}

fn workspaces_list() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "workspaces.list",
        object(&[], &[]),
        array_schema(any_schema()),
        &[PERM_WORKSPACES_READ],
        false,
        |call: HostCall<'_>| {
            let workspaces = call.host.list_workspaces();
            Ok(Value::Array(
                workspaces.iter().map(workspace_info_json).collect(),
            ))
        },
    )
}

fn workspaces_create() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "workspaces.create",
        object(&[("path", string_schema())], &["path"]),
        any_schema(),
        &[PERM_WORKSPACES_WRITE],
        false,
        |call: HostCall<'_>| {
            let path = required_string(&call.params, "path")?;
            let workspace = call
                .host
                .create_workspace(path)
                .map_err(|e| ApiError::internal(e.to_string()))?;
            Ok(workspace_info_json(&workspace))
        },
    )
}

fn workspaces_rename() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "workspaces.rename",
        object(
            &[("workspaceId", string_schema()), ("title", string_schema())],
            &["workspaceId", "title"],
        ),
        any_schema(),
        &[PERM_WORKSPACES_WRITE],
        false,
        |call: HostCall<'_>| {
            let id = required_string(&call.params, "workspaceId")?;
            let title = required_string(&call.params, "title")?;
            call.host
                .rename_workspace(id, title)
                .map_err(|e| ApiError::internal(e.to_string()))?;
            Ok(json!({}))
        },
    )
}

fn workspaces_active() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "workspaces.active",
        object(&[("agent", string_schema())], &[]),
        any_schema(),
        &[PERM_WORKSPACES_READ],
        false,
        |call: HostCall<'_>| {
            let agent = optional_string(&call.params, "agent")
                .unwrap_or_else(|| starriver_host::CODING_AGENT_NAME.to_string());
            Ok(json!({ "workspaceId": call.host.get_active_workspace_for(&agent) }))
        },
    )
}

fn workspaces_activate() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "workspaces.activate",
        object(
            &[("workspaceId", string_schema()), ("agent", string_schema())],
            &[],
        ),
        any_schema(),
        &[PERM_WORKSPACES_WRITE],
        false,
        |call: HostCall<'_>| {
            let id = call
                .params
                .get("workspaceId")
                .and_then(Value::as_str)
                .map(String::from);
            let agent = optional_string(&call.params, "agent")
                .unwrap_or_else(|| starriver_host::CODING_AGENT_NAME.to_string());
            call.host
                .set_active_workspace_for(&agent, id)
                .map_err(|e| ApiError::internal(e.to_string()))?;
            Ok(json!({}))
        },
    )
}

// ── Session extras ──

fn session_timeline() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "session.timeline",
        object(&[("sessionId", string_schema())], &["sessionId"]),
        array_schema(any_schema()),
        &[PERM_SESSION_READ],
        false,
        |call: HostCall<'_>| {
            let session_id = required_string(&call.params, "sessionId")?;
            let entries = call.host.get_session_timeline(session_id);
            Ok(Value::Array(
                entries.iter().map(session_message_json).collect(),
            ))
        },
    )
}

fn session_trace() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "session.trace",
        object(&[("sessionId", string_schema())], &["sessionId"]),
        any_schema(),
        &[PERM_SESSION_READ],
        false,
        |call: HostCall<'_>| {
            let session_id = required_string(&call.params, "sessionId")?;
            let mut trace = session_trace_json(&call.host.get_session_trace(session_id.clone()));
            // Byte-exact live view (open-turn snapshot stream): send the projection epoch with the full hydration.
            if let Some(obj) = trace.as_object_mut() {
                obj.insert("epoch".into(), json!(call.host.trace_epoch(&session_id)));
            }
            Ok(trace)
        },
    )
}

fn session_compact() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "session.compact",
        object(
            &[
                ("sessionId", string_schema()),
                ("keepRecent", integer_schema()),
            ],
            &["sessionId"],
        ),
        any_schema(),
        &[PERM_SESSION_WRITE],
        false,
        |call: HostCall<'_>| {
            let session_id = required_string(&call.params, "sessionId")?;
            let keep_recent = call.params.get("keepRecent").and_then(Value::as_u64);
            // Byte-exact live view: history is about to be rewritten — bump the projection epoch first (clients fully re-pull;
            // the re-pull happens after the rewrite completes).
            call.host.bump_trace_epoch(&session_id);
            let result = call
                .host
                .compact_session(session_id, keep_recent.filter(|v| *v <= u32::MAX as u64).map(|v| v as u32))
                .map_err(|e| ApiError::internal(e.to_string()))?;
            Ok(compact_result_json(&result))
        },
    )
}

fn session_usage_method() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "session.usage",
        object(&[("sessionId", string_schema())], &["sessionId"]),
        any_schema(),
        &[PERM_SESSION_READ],
        false,
        |call: HostCall<'_>| {
            let session_id = required_string(&call.params, "sessionId")?;
            let (tokens, count) = call
                .host
                .session_usage(&session_id)
                .map_err(|e| ApiError::internal(e.to_string()))?;
            Ok(json!({ "tokens": tokens as u32, "count": count as u32 }))
        },
    )
}

fn session_export() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "session.export",
        object(
            &[("sessionId", string_schema()), ("cwd", string_schema())],
            &["sessionId"],
        ),
        any_schema(),
        &[PERM_SESSION_WRITE],
        false,
        |call: HostCall<'_>| {
            let session_id = required_string(&call.params, "sessionId")?;
            let cwd = call
                .params
                .get("cwd")
                .and_then(Value::as_str)
                .unwrap_or(".");
            let path = call
                .host
                .export_session(&session_id, cwd)
                .map_err(|e| ApiError::internal(e.to_string()))?;
            Ok(json!({ "path": path }))
        },
    )
}

// ── Skills ──

fn skills_scan() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "skills.scan",
        object(&[], &[]),
        array_schema(any_schema()),
        &[PERM_SKILLS_READ],
        false,
        |call: HostCall<'_>| {
            let skills = call.host.scan_skills();
            Ok(Value::Array(skills.iter().map(skill_info_json).collect()))
        },
    )
}

fn skills_create() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "skills.create",
        object(
            &[
                ("name", string_schema()),
                ("description", string_schema()),
                ("command", string_schema()),
                ("body", string_schema()),
            ],
            &["name", "description", "body"],
        ),
        array_schema(any_schema()),
        &[PERM_SKILLS_WRITE],
        false,
        |call: HostCall<'_>| {
            let name = required_string(&call.params, "name")?;
            let description = required_string(&call.params, "description")?;
            let command = call
                .params
                .get("command")
                .and_then(Value::as_str)
                .map(String::from);
            let body = required_string(&call.params, "body")?;
            let skills = call
                .host
                .create_skill(name, description, command, body)
                .map_err(|e| ApiError::internal(e.to_string()))?;
            Ok(Value::Array(skills.iter().map(skill_info_json).collect()))
        },
    )
}

fn skills_update() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "skills.update",
        object(
            &[
                ("name", string_schema()),
                ("description", string_schema()),
                ("command", string_schema()),
                ("body", string_schema()),
            ],
            &["name", "body"],
        ),
        array_schema(any_schema()),
        &[PERM_SKILLS_WRITE],
        false,
        |call: HostCall<'_>| {
            let name = required_string(&call.params, "name")?;
            let description = call
                .params
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let command = call
                .params
                .get("command")
                .and_then(Value::as_str)
                .map(String::from);
            let body = required_string(&call.params, "body")?;
            let skills = call
                .host
                .update_skill(name, description, command, body)
                .map_err(|e| ApiError::internal(e.to_string()))?;
            Ok(Value::Array(skills.iter().map(skill_info_json).collect()))
        },
    )
}

fn skills_set_always_on() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "skills.setAlwaysOn",
        object(
            &[("name", string_schema()), ("alwaysOn", boolean_schema())],
            &["name", "alwaysOn"],
        ),
        array_schema(any_schema()),
        &[PERM_SKILLS_WRITE],
        false,
        |call: HostCall<'_>| {
            let name = required_string(&call.params, "name")?;
            let always_on = call.params["alwaysOn"].as_bool().unwrap_or(false);
            let skills = call
                .host
                .set_skill_always_on(name, always_on)
                .map_err(|e| ApiError::internal(e.to_string()))?;
            Ok(Value::Array(skills.iter().map(skill_info_json).collect()))
        },
    )
}

fn skills_usage() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "skills.usage",
        object(&[("limit", integer_schema())], &[]),
        array_schema(any_schema()),
        &[PERM_SKILLS_READ],
        false,
        |call: HostCall<'_>| {
            let limit = call
                .params
                .get("limit")
                .and_then(Value::as_u64)
                .map(|v| v as u32);
            let records = call.host.list_skill_usage(limit);
            Ok(Value::Array(records.iter().map(skill_usage_json).collect()))
        },
    )
}

// ── WI-115: hooks script store ──

fn hook_info_json(h: &starriver_host::hooks::HookScriptInfo) -> Value {
    serde_json::json!({
        "event": h.event,
        "name": h.name,
        "size": h.size,
        "modifiedSecs": h.modified_secs,
    })
}

fn hooks_list() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "hooks.list",
        object(&[], &[]),
        array_schema(any_schema()),
        &[PERM_HOOKS_READ],
        false,
        |call: HostCall<'_>| {
            let hooks = call.host.list_hooks();
            Ok(Value::Array(hooks.iter().map(hook_info_json).collect()))
        },
    )
}

fn hooks_read() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "hooks.read",
        object(
            &[("event", string_schema()), ("name", string_schema())],
            &["event", "name"],
        ),
        object(&[("content", string_schema())], &["content"]),
        &[PERM_HOOKS_READ],
        false,
        |call: HostCall<'_>| {
            let event = required_string(&call.params, "event")?;
            let name = required_string(&call.params, "name")?;
            let content = call
                .host
                .read_hook(event, name)
                .map_err(|e| ApiError::internal(e.to_string()))?;
            Ok(serde_json::json!({ "content": content }))
        },
    )
}

fn hooks_write() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "hooks.write",
        object(
            &[
                ("event", string_schema()),
                ("name", string_schema()),
                ("content", string_schema()),
            ],
            &["event", "name", "content"],
        ),
        object(&[], &[]),
        &[PERM_HOOKS_WRITE],
        false,
        |call: HostCall<'_>| {
            let event = required_string(&call.params, "event")?;
            let name = required_string(&call.params, "name")?;
            let content = required_string(&call.params, "content")?;
            call.host
                .write_hook(event, name, content)
                .map_err(|e| ApiError::internal(e.to_string()))?;
            Ok(Value::Object(serde_json::Map::new()))
        },
    )
}

fn hooks_delete() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "hooks.delete",
        object(
            &[("event", string_schema()), ("name", string_schema())],
            &["event", "name"],
        ),
        object(&[], &[]),
        &[PERM_HOOKS_WRITE],
        false,
        |call: HostCall<'_>| {
            let event = required_string(&call.params, "event")?;
            let name = required_string(&call.params, "name")?;
            call.host
                .delete_hook(event, name)
                .map_err(|e| ApiError::internal(e.to_string()))?;
            Ok(Value::Object(serde_json::Map::new()))
        },
    )
}

// ── Agents ──

fn agents_scan() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "agents.scan",
        object(&[], &[]),
        array_schema(any_schema()),
        &[PERM_AGENTS_READ],
        false,
        |call: HostCall<'_>| {
            let agents = call.host.scan_agents();
            Ok(Value::Array(agents.iter().map(agent_info_json).collect()))
        },
    )
}

fn agents_create() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "agents.create",
        object(
            &[("name", string_schema()), ("description", string_schema())],
            &["name", "description"],
        ),
        any_schema(),
        &[PERM_AGENTS_WRITE],
        false,
        |call: HostCall<'_>| {
            let name = required_string(&call.params, "name")?;
            let description = required_string(&call.params, "description")?;
            let agent = call
                .host
                .create_agent(name, description)
                .map_err(|e| ApiError::internal(e.to_string()))?;
            Ok(agent_info_json(&agent))
        },
    )
}

fn agents_export() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "agents.export",
        object(&[("name", string_schema())], &["name"]),
        any_schema(),
        &[PERM_AGENTS_READ],
        false,
        |call: HostCall<'_>| {
            let name = required_string(&call.params, "name")?;
            let path = call
                .host
                .export_agent(name)
                .map_err(|e| ApiError::internal(e.to_string()))?;
            Ok(json!({ "path": path }))
        },
    )
}

fn agents_bindings() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "agents.bindings",
        object(&[], &[]),
        array_schema(any_schema()),
        &[PERM_MODELS_READ],
        false,
        |call: HostCall<'_>| {
            let bindings = call.host.list_agent_model_bindings();
            Ok(Value::Array(bindings.iter().map(binding_json).collect()))
        },
    )
}

fn agents_bindings_set_active() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "agents.bindings.setActive",
        object(
            &[
                ("agentName", string_schema()),
                ("providerId", string_schema()),
                ("variantId", string_schema()),
            ],
            &["agentName", "providerId", "variantId"],
        ),
        any_schema(),
        &[PERM_MODELS_WRITE],
        false,
        |call: HostCall<'_>| {
            let agent = required_string(&call.params, "agentName")?;
            let provider = required_string(&call.params, "providerId")?;
            let variant = required_string(&call.params, "variantId")?;
            call.host
                .set_agent_active_model(agent, provider, variant)
                .map_err(|e| ApiError::internal(e.to_string()))?;
            Ok(json!({}))
        },
    )
}

fn agents_bindings_set_reasoning_effort() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "agents.bindings.setReasoningEffort",
        object(
            &[("agentName", string_schema()), ("effort", string_schema())],
            &["agentName", "effort"],
        ),
        any_schema(),
        &[PERM_MODELS_WRITE],
        false,
        |call: HostCall<'_>| {
            let agent = required_string(&call.params, "agentName")?;
            let effort = required_string(&call.params, "effort")?;
            call.host
                .set_agent_reasoning_effort(agent, effort)
                .map_err(|e| ApiError::internal(e.to_string()))?;
            Ok(json!({}))
        },
    )
}

// ── Models / providers / subagents ──

fn models_providers_list() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "models.providers.list",
        object(&[], &[]),
        array_schema(any_schema()),
        &[PERM_MODELS_READ],
        false,
        |call: HostCall<'_>| {
            let providers = call.host.list_model_providers();
            Ok(Value::Array(
                providers.iter().map(model_provider_json).collect(),
            ))
        },
    )
}

fn models_add() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "models.add",
        object(
            &[
                ("id", string_schema()),
                ("name", string_schema()),
                ("modelType", string_schema()),
                ("providerType", string_schema()),
                ("baseUrl", string_schema()),
                ("apiKey", string_schema()),
                ("modelPath", string_schema()),
                ("defaultModel", string_schema()),
                ("variants", array_schema(any_schema())),
                ("concurrency", integer_schema()),
                ("maxTokens", integer_schema()),
                ("contextWindow", integer_schema()),
                ("temperature", number_schema()),
                ("apiFormat", string_schema()),
                ("reasoningEffort", string_schema()),
                ("modelLabel", string_schema()),
                ("supportsVision", boolean_schema()),
            ],
            &["id", "name", "modelType", "providerType", "defaultModel"],
        ),
        any_schema(),
        &[PERM_MODELS_WRITE],
        false,
        |call: HostCall<'_>| {
            let id = required_string(&call.params, "id")?;
            let name = required_string(&call.params, "name")?;
            let model_type = required_string(&call.params, "modelType")?;
            let provider_type = required_string(&call.params, "providerType")?;
            let default_model = required_string(&call.params, "defaultModel")?;
            let variants_json = call
                .params
                .get("variants")
                .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "[]".into()));
            call.host
                .add_model(
                    id,
                    name,
                    model_type,
                    provider_type,
                    optional_string(&call.params, "baseUrl"),
                    optional_string(&call.params, "apiKey"),
                    optional_string(&call.params, "modelPath"),
                    default_model,
                    variants_json,
                    optional_u32(&call.params, "concurrency"),
                    optional_u32(&call.params, "maxTokens"),
                    optional_u32(&call.params, "contextWindow"),
                    optional_f64(&call.params, "temperature"),
                    optional_string(&call.params, "apiFormat"),
                    optional_string(&call.params, "reasoningEffort"),
                    optional_string(&call.params, "modelLabel"),
                    call.params.get("supportsVision").and_then(Value::as_bool),
                )
                .map_err(|e| ApiError::internal(e.to_string()))?;
            Ok(json!({}))
        },
    )
}

fn configurable_providers_list() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "configurableProviders.list",
        object(&[], &[]),
        array_schema(any_schema()),
        &[PERM_MODELS_READ],
        false,
        |call: HostCall<'_>| {
            let providers = call.host.list_configurable_providers();
            Ok(Value::Array(
                providers.iter().map(configurable_provider_json).collect(),
            ))
        },
    )
}

fn subagents_list() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "subagents.list",
        object(&[], &[]),
        array_schema(any_schema()),
        &[PERM_AGENTS_READ],
        false,
        |call: HostCall<'_>| {
            let configs = call.host.list_subagent_configs();
            Ok(Value::Array(
                configs.iter().map(subagent_config_json).collect(),
            ))
        },
    )
}

fn subagents_set() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "subagents.set",
        object(
            &[
                ("name", string_schema()),
                ("enabled", boolean_schema()),
                ("model", string_schema()),
            ],
            &["name", "enabled"],
        ),
        any_schema(),
        &[PERM_AGENTS_WRITE],
        false,
        |call: HostCall<'_>| {
            let name = required_string(&call.params, "name")?;
            let enabled = call.params["enabled"].as_bool().unwrap_or(false);
            call.host
                .set_subagent_config(name, enabled, optional_string(&call.params, "model"))
                .map_err(|e| ApiError::internal(e.to_string()))?;
            Ok(json!({}))
        },
    )
}

// ── Credentials (only expose existence; never read values back) ──

fn credentials_describe() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "credentials.describe",
        object(&[("key", string_schema())], &["key"]),
        any_schema(),
        &[PERM_CREDENTIALS_READ],
        false,
        |call: HostCall<'_>| {
            let key = required_string(&call.params, "key")?;
            let info = call
                .host
                .describe_credential(key)
                .map_err(|e| ApiError::internal(e.to_string()))?;
            Ok(credential_info_json(&info))
        },
    )
}

fn credentials_list() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "credentials.list",
        object(&[], &[]),
        array_schema(any_schema()),
        &[PERM_CREDENTIALS_READ],
        false,
        |call: HostCall<'_>| {
            let records = call.host.list_credential_records();
            Ok(Value::Array(
                records.iter().map(credential_entry_json).collect(),
            ))
        },
    )
}

/// Read back one credential value (settings show the REAL key; the input is
/// password-masked and blank-save keeps the stored reference).
fn credentials_read() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "credentials.read",
        object(&[("key", string_schema())], &["key"]),
        string_schema(),
        &[PERM_CREDENTIALS_READ],
        false,
        |call: HostCall<'_>| {
            let key = required_string(&call.params, "key")?;
            let value = call
                .host
                .read_credential(key)
                .map_err(|e| ApiError::internal(e.to_string()))?;
            Ok(Value::String(value))
        },
    )
}

fn credentials_set_api_key() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "credentials.setApiKey",
        object(
            &[("key", string_schema()), ("value", string_schema())],
            &["key", "value"],
        ),
        any_schema(),
        &[PERM_CREDENTIALS_WRITE],
        false,
        |call: HostCall<'_>| {
            let key = required_string(&call.params, "key")?;
            let value = required_string(&call.params, "value")?;
            let reference = call
                .host
                .set_credential_api_key(key, value)
                .map_err(|e| ApiError::internal(e.to_string()))?;
            Ok(json!({ "reference": reference }))
        },
    )
}

fn credentials_unset() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "credentials.unset",
        object(&[("key", string_schema())], &["key"]),
        any_schema(),
        &[PERM_CREDENTIALS_WRITE],
        false,
        |call: HostCall<'_>| {
            let key = required_string(&call.params, "key")?;
            call.host
                .unset_credential(key)
                .map_err(|e| ApiError::internal(e.to_string()))?;
            Ok(json!({}))
        },
    )
}

// ── Memory ──

fn memory_list() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "memory.list",
        object(&[("projectId", string_schema())], &[]),
        array_schema(any_schema()),
        &[PERM_MEMORY_READ],
        false,
        |call: HostCall<'_>| {
            let project_id = optional_string(&call.params, "projectId");
            let memories = call.host.list_memories(project_id);
            Ok(Value::Array(
                memories.iter().map(memory_entry_json).collect(),
            ))
        },
    )
}

fn memory_recall() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "memory.recall",
        object(
            &[("projectId", string_schema()), ("query", string_schema())],
            &["query"],
        ),
        array_schema(any_schema()),
        &[PERM_MEMORY_READ],
        false,
        |call: HostCall<'_>| {
            let project_id = optional_string(&call.params, "projectId");
            let query = required_string(&call.params, "query")?;
            let memories = call.host.recall_memories(project_id, query);
            Ok(Value::Array(
                memories.iter().map(memory_entry_json).collect(),
            ))
        },
    )
}

fn memory_upsert() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "memory.upsert",
        object(
            &[
                ("projectId", string_schema()),
                ("name", string_schema()),
                ("description", string_schema()),
                ("body", string_schema()),
            ],
            &["name", "description", "body"],
        ),
        any_schema(),
        &[PERM_MEMORY_WRITE],
        false,
        |call: HostCall<'_>| {
            let project_id = optional_string(&call.params, "projectId");
            let name = required_string(&call.params, "name")?;
            let description = required_string(&call.params, "description")?;
            let body = required_string(&call.params, "body")?;
            let entry = call
                .host
                .upsert_memory(project_id, name, description, body)
                .map_err(|e| ApiError::internal(e.to_string()))?;
            Ok(memory_entry_json(&entry))
        },
    )
}

fn memory_delete() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "memory.delete",
        object(
            &[("projectId", string_schema()), ("name", string_schema())],
            &["projectId", "name"],
        ),
        any_schema(),
        &[PERM_MEMORY_WRITE],
        false,
        |call: HostCall<'_>| {
            let project = required_string(&call.params, "projectId")?;
            let name = required_string(&call.params, "name")?;
            call.host
                .delete_memory(project, name)
                .map_err(|e| ApiError::internal(e.to_string()))?;
            Ok(json!({}))
        },
    )
}

fn memory_clear() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "memory.clear",
        object(&[("projectId", string_schema())], &["projectId"]),
        any_schema(),
        &[PERM_MEMORY_WRITE],
        false,
        |call: HostCall<'_>| {
            let project = required_string(&call.params, "projectId")?;
            let removed = call
                .host
                .clear_memories(project)
                .map_err(|e| ApiError::internal(e.to_string()))?;
            Ok(json!({ "removed": removed }))
        },
    )
}

fn memory_ingest() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "memory.ingest",
        object(&[], &[]),
        any_schema(),
        &[PERM_MEMORY_WRITE],
        false,
        |call: HostCall<'_>| Ok(call.host.ingest_memories()),
    )
}

fn memory_status() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "memory.status",
        object(&[], &[]),
        any_schema(),
        &[PERM_MEMORY_READ],
        false,
        |call: HostCall<'_>| Ok(call.host.memory_status()),
    )
}

fn memory_export() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "memory.export",
        object(&[("path", string_schema())], &["path"]),
        any_schema(),
        &[PERM_MEMORY_WRITE],
        false,
        |call: HostCall<'_>| {
            let path = required_string(&call.params, "path")?;
            call.host
                .export_memories(path)
                .map_err(|e| ApiError::internal(e.to_string()))
        },
    )
}

// ── MCP ──

fn mcp_list() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "mcp.list",
        object(&[], &[]),
        array_schema(any_schema()),
        &[PERM_MCP_READ],
        false,
        |call: HostCall<'_>| {
            let servers = call.host.list_mcp_servers();
            Ok(Value::Array(servers.iter().map(mcp_server_json).collect()))
        },
    )
}

fn mcp_add() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "mcp.add",
        object(
            &[
                ("name", string_schema()),
                ("transport", string_schema()),
                ("command", string_schema()),
                ("args", array_schema(string_schema())),
                ("env", any_schema()),
                ("url", string_schema()),
                ("oauth", any_schema()),
            ],
            &["name", "transport"],
        ),
        any_schema(),
        &[PERM_MCP_WRITE],
        false,
        |call: HostCall<'_>| {
            let name = required_string(&call.params, "name")?;
            let transport = required_string(&call.params, "transport")?;
            let command = call
                .params
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if transport == "stdio" && command.trim().is_empty() {
                return Err(ApiError::invalid_params(
                    "stdio transport requires a non-empty \"command\"",
                ));
            }
            let args_json = call
                .params
                .get("args")
                .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "[]".into()))
                .unwrap_or_else(|| "[]".into());
            let env_json = call
                .params
                .get("env")
                .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "{}".into()))
                .unwrap_or_else(|| "{}".into());
            let oauth_json = call
                .params
                .get("oauth")
                .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "null".into()));
            call.host
                .add_mcp_server(
                    name,
                    transport,
                    command,
                    args_json,
                    env_json,
                    optional_string(&call.params, "url"),
                    oauth_json,
                )
                .map_err(|e| ApiError::internal(e.to_string()))?;
            Ok(json!({}))
        },
    )
}

fn mcp_refresh() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "mcp.refresh",
        object(&[("name", string_schema())], &[]),
        any_schema(),
        &[PERM_MCP_WRITE],
        false,
        |call: HostCall<'_>| {
            call.host
                .refresh_mcp_tools(optional_string(&call.params, "name"))
                .map_err(|e| ApiError::internal(e.to_string()))?;
            Ok(json!({}))
        },
    )
}

fn mcp_oauth_connect() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "mcp.oauth.connect",
        object(&[("name", string_schema())], &["name"]),
        any_schema(),
        &[PERM_MCP_WRITE],
        false,
        |call: HostCall<'_>| {
            let name = required_string(&call.params, "name")?;
            let url = call
                .host
                .oauth_connect(name)
                .map_err(|e| ApiError::internal(e.to_string()))?;
            Ok(json!({ "authorizationUrl": url }))
        },
    )
}

fn mcp_oauth_disconnect() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "mcp.oauth.disconnect",
        object(&[("name", string_schema())], &["name"]),
        any_schema(),
        &[PERM_MCP_WRITE],
        false,
        |call: HostCall<'_>| {
            let name = required_string(&call.params, "name")?;
            call.host
                .oauth_disconnect(name)
                .map_err(|e| ApiError::internal(e.to_string()))?;
            Ok(json!({}))
        },
    )
}

fn mcp_test_server() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "mcp.testServer",
        object(&[("name", string_schema())], &["name"]),
        any_schema(),
        &[PERM_MCP_READ],
        false,
        |call: HostCall<'_>| {
            let name = required_string(&call.params, "name")?;
            Ok(mcp_test_json(&call.host.test_mcp_server(name)))
        },
    )
}

fn mcp_test_tool() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "mcp.testTool",
        object(
            &[("name", string_schema()), ("toolName", string_schema())],
            &["name", "toolName"],
        ),
        any_schema(),
        &[PERM_MCP_READ],
        false,
        |call: HostCall<'_>| {
            let name = required_string(&call.params, "name")?;
            let tool = required_string(&call.params, "toolName")?;
            Ok(mcp_tool_test_json(&call.host.test_mcp_tool(name, tool)))
        },
    )
}

// ── Scheduled tasks / jobs ──

fn scheduled_list() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "scheduled.list",
        object(&[], &[]),
        array_schema(any_schema()),
        &[PERM_SCHEDULED_READ],
        false,
        |call: HostCall<'_>| {
            let tasks = call.host.list_scheduled_tasks();
            Ok(Value::Array(
                tasks.iter().map(scheduled_task_json).collect(),
            ))
        },
    )
}

fn scheduled_add() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "scheduled.add",
        object(
            &[
                ("name", string_schema()),
                ("mode", any_schema()),
                ("prompt", string_schema()),
                ("model", string_schema()),
                ("agent", string_schema()),
                ("workspace", string_schema()),
                ("enabled", boolean_schema()),
                ("startDate", string_schema()),
                ("endDate", string_schema()),
                ("skills", array_schema(string_schema())),
            ],
            &["name", "mode", "prompt"],
        ),
        any_schema(),
        &[PERM_SCHEDULED_WRITE],
        false,
        |call: HostCall<'_>| {
            let name = required_string(&call.params, "name")?;
            let mode_json = serde_json::to_string(call.params.get("mode").unwrap_or(&Value::Null))
                .unwrap_or_else(|_| "{}".into());
            let prompt = required_string(&call.params, "prompt")?;
            let skills = call
                .params
                .get("skills")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(String::from)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            call.host
                .add_scheduled_task(
                    name,
                    mode_json,
                    prompt,
                    optional_string(&call.params, "model"),
                    optional_string(&call.params, "agent"),
                    optional_string(&call.params, "workspace"),
                    call.params["enabled"].as_bool().unwrap_or(true),
                    optional_string(&call.params, "startDate"),
                    optional_string(&call.params, "endDate"),
                    skills,
                )
                .map_err(|e| ApiError::internal(e.to_string()))?;
            Ok(json!({}))
        },
    )
}

fn scheduled_update() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "scheduled.update",
        object(
            &[
                ("name", string_schema()),
                ("mode", any_schema()),
                ("prompt", string_schema()),
                ("model", string_schema()),
                ("agent", string_schema()),
                ("workspace", string_schema()),
                ("enabled", boolean_schema()),
                ("startDate", string_schema()),
                ("endDate", string_schema()),
                ("skills", array_schema(string_schema())),
            ],
            &["name"],
        ),
        any_schema(),
        &[PERM_SCHEDULED_WRITE],
        false,
        |call: HostCall<'_>| {
            let name = required_string(&call.params, "name")?;
            let mode_json = call
                .params
                .get("mode")
                .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "{}".into()));
            let skills = call
                .params
                .get("skills")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(String::from)
                        .collect::<Vec<_>>()
                });
            call.host
                .update_scheduled_task(
                    name,
                    mode_json,
                    optional_string(&call.params, "prompt"),
                    optional_string(&call.params, "model"),
                    optional_string(&call.params, "agent"),
                    optional_string(&call.params, "workspace"),
                    call.params.get("enabled").and_then(Value::as_bool),
                    optional_string(&call.params, "startDate"),
                    optional_string(&call.params, "endDate"),
                    skills,
                )
                .map_err(|e| ApiError::internal(e.to_string()))?;
            Ok(json!({}))
        },
    )
}

fn scheduled_run_now() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "scheduled.runNow",
        object(&[("name", string_schema())], &["name"]),
        any_schema(),
        &[PERM_SCHEDULED_WRITE],
        false,
        |call: HostCall<'_>| {
            let name = required_string(&call.params, "name")?;
            let run_id = call
                .host
                .run_scheduled_task_now(name)
                .map_err(|e| ApiError::internal(e.to_string()))?;
            Ok(json!({ "runId": run_id }))
        },
    )
}

fn jobs_list() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "jobs.list",
        object(&[], &[]),
        array_schema(any_schema()),
        &[PERM_JOBS_READ],
        false,
        |call: HostCall<'_>| {
            let jobs = call.host.list_jobs();
            Ok(Value::Array(jobs.iter().map(job_info_json).collect()))
        },
    )
}

fn jobs_task_runs() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "jobs.taskRuns",
        object(&[], &[]),
        array_schema(any_schema()),
        &[PERM_JOBS_READ],
        false,
        |call: HostCall<'_>| {
            let runs = call.host.list_all_task_runs();
            Ok(Value::Array(
                runs.iter().map(task_run_with_name_json).collect(),
            ))
        },
    )
}

fn jobs_task_run_history() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "jobs.taskRun.history",
        object(&[("name", string_schema())], &["name"]),
        array_schema(any_schema()),
        &[PERM_JOBS_READ],
        false,
        |call: HostCall<'_>| {
            let name = required_string(&call.params, "name")?;
            let runs = call.host.get_task_run_history(name);
            Ok(Value::Array(runs.iter().map(task_run_json).collect()))
        },
    )
}

// ── Plugins / sources / market ──

fn plugins_install() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "plugins.install",
        object(&[("source", string_schema())], &["source"]),
        any_schema(),
        &[PERM_PLUGINS_WRITE],
        false,
        |call: HostCall<'_>| {
            let source = required_string(&call.params, "source")?;
            let plugin = call
                .host
                .install_plugin(source)
                .map_err(|e| ApiError::internal(e.to_string()))?;
            Ok(plugin_entry_json(&plugin))
        },
    )
}

fn plugins_uninstall() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "plugins.uninstall",
        object(&[("name", string_schema())], &["name"]),
        array_schema(any_schema()),
        &[PERM_PLUGINS_WRITE],
        false,
        |call: HostCall<'_>| {
            let name = required_string(&call.params, "name")?;
            let plugins = call
                .host
                .uninstall_plugin(name)
                .map_err(|e| ApiError::internal(e.to_string()))?;
            Ok(Value::Array(
                plugins.iter().map(plugin_entry_json).collect(),
            ))
        },
    )
}

fn market_list() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "market.list",
        object(&[], &[]),
        array_schema(any_schema()),
        &[PERM_MARKET_READ],
        false,
        |call: HostCall<'_>| {
            let entries = call.host.market_list();
            Ok(Value::Array(
                entries.iter().map(market_entry_json).collect(),
            ))
        },
    )
}

// ── Workspace instructions banner / project / hooks / review / attachments ──

fn workspace_instructions() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "workspace.instructions",
        object(&[("workspaceId", string_schema())], &["workspaceId"]),
        any_schema(),
        &[PERM_WORKSPACES_READ],
        false,
        |call: HostCall<'_>| {
            let workspace_id = required_string(&call.params, "workspaceId")?;
            match call.host.workspace_instructions_meta(workspace_id) {
                Some(meta) => Ok(json!({
                    "content": meta.content,
                    "truncated": meta.truncated,
                    "bytes": meta.bytes,
                    "budget": meta.budget,
                })),
                None => Ok(json!({ "content": null, "truncated": false })),
            }
        },
    )
}

fn workspace_diff() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "workspace.diff",
        object(&[("cwd", string_schema())], &["cwd"]),
        any_schema(),
        &[PERM_WORKSPACES_READ],
        false,
        |call: HostCall<'_>| {
            let cwd = required_string(&call.params, "cwd")?;
            let diff = call
                .host
                .workspace_diff(&cwd)
                .map_err(|e| ApiError::internal(e.to_string()))?;
            Ok(json!({ "diff": diff }))
        },
    )
}

fn workspace_diff_stat() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "workspace.diffStat",
        object(&[("cwd", string_schema())], &["cwd"]),
        any_schema(),
        &[PERM_WORKSPACES_READ],
        false,
        |call: HostCall<'_>| {
            let cwd = required_string(&call.params, "cwd")?;
            let stat = call
                .host
                .workspace_diff_stat(&cwd)
                .map_err(|e| ApiError::internal(e.to_string()))?;
            Ok(json!({
                "branch": stat.branch,
                "files": stat.files,
                "totalAdditions": stat.total_additions,
                "totalDeletions": stat.total_deletions,
            }))
        },
    )
}

fn workspace_worktree_status() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "workspace.worktreeStatus",
        object(&[("cwd", string_schema())], &["cwd"]),
        any_schema(),
        &[PERM_WORKSPACES_READ],
        false,
        |call: HostCall<'_>| {
            let cwd = required_string(&call.params, "cwd")?;
            Ok(worktree_status_json(&call.host.get_worktree_status(cwd)))
        },
    )
}

fn project_agents_init() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "project.agents.init",
        object(&[("cwd", string_schema())], &["cwd"]),
        any_schema(),
        &[PERM_PROJECT_WRITE],
        false,
        |call: HostCall<'_>| {
            let cwd = required_string(&call.params, "cwd")?;
            let result = call
                .host
                .init_project_agents(&cwd)
                .map_err(|e| ApiError::internal(e.to_string()))?;
            Ok(json!({ "result": result }))
        },
    )
}

fn review_start() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "review.start",
        object(
            &[
                ("files", array_schema(string_schema())),
                ("dimensions", array_schema(string_schema())),
            ],
            &["files"],
        ),
        any_schema(),
        &[PERM_REVIEW_START],
        false,
        |call: HostCall<'_>| {
            let files = call
                .params
                .get("files")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(String::from)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let dimensions = call
                .params
                .get("dimensions")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(String::from)
                        .collect::<Vec<_>>()
                });
            let files_json = serde_json::to_string(&files)
                .map_err(|e| ApiError::invalid_params(e.to_string()))?;
            let dimensions_json = dimensions
                .map(|d| {
                    serde_json::to_string(&d).map_err(|e| ApiError::invalid_params(e.to_string()))
                })
                .transpose()?;
            call.host
                .start_code_review(files_json, dimensions_json)
                .map_err(|e| ApiError::internal(e.to_string()))?;
            Ok(json!({}))
        },
    )
}

fn attachments_admit() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "attachments.admit",
        object(&[("images", array_schema(any_schema()))], &["images"]),
        array_schema(any_schema()),
        &[PERM_ATTACHMENTS_WRITE],
        false,
        |call: HostCall<'_>| {
            let images = call
                .params
                .get("images")
                .ok_or_else(|| ApiError::invalid_params("images is required"))?;
            let images_json = serde_json::to_string(images)
                .map_err(|e| ApiError::invalid_params(e.to_string()))?;
            let refs = call
                .host
                .admit_image_attachments(images_json)
                .map_err(|e| ApiError::internal(e.to_string()))?;
            Ok(Value::Array(refs.iter().map(attachment_ref_json).collect()))
        },
    )
}

/// WI-48 feedback slice: record user 👍/👎 (with optional comment) on an
/// assistant reply. Durable in the session log; also feeds the feedback
/// resident that injects recent feedback into later requests.
fn feedback_record() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "feedback.record",
        object(
            &[
                ("sessionId", string_schema()),
                ("kind", string_schema()),
                ("comment", string_schema()),
            ],
            &["sessionId", "kind"],
        ),
        object(&[("ok", starriver_protocol::schema::boolean_schema())], &[]),
        &[PERM_SESSION_WRITE],
        false,
        |call: HostCall<'_>| {
            let session_id = required_string(&call.params, "sessionId")?;
            let kind = required_string(&call.params, "kind")?;
            let comment = call
                .params
                .get("comment")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            call.host
                .record_feedback(session_id, kind, comment)
                .map_err(|e| ApiError::internal(e.to_string()))?;
            Ok(json!({ "ok": true }))
        },
    )
}

/// WI-7 @file completion: bounded workspace file listing for the composer's
/// `@` popup (VCS/build/hidden entries excluded, capped results).
fn fs_suggest() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "fs.suggest",
        object(
            &[
                ("workspaceId", string_schema()),
                ("prefix", string_schema()),
            ],
            &["workspaceId"],
        ),
        any_schema(),
        &[PERM_WORKSPACES_READ],
        false,
        |call: HostCall<'_>| {
            let workspace_id = required_string(&call.params, "workspaceId")?;
            let prefix = call
                .params
                .get("prefix")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let files = call.host.suggest_files(&workspace_id, &prefix, 20);
            Ok(Value::Array(
                files.into_iter().map(|path| Value::String(path)).collect(),
            ))
        },
    )
}

fn attachments_read() -> MethodSpec<HostRuntime> {
    MethodSpec::new(
        "attachments.read",
        object(&[("id", string_schema())], &["id"]),
        any_schema(),
        &[PERM_ATTACHMENTS_READ],
        false,
        |call: HostCall<'_>| {
            let id = required_string(&call.params, "id")?;
            let attachment = call
                .host
                .read_image_attachment(id)
                .map_err(|e| ApiError::internal(e.to_string()))?;
            Ok(stored_attachment_json(&attachment))
        },
    )
}

// ── Parameter-reading helpers ──

fn required_string(params: &Value, key: &str) -> Result<String, ApiError> {
    params
        .get(key)
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| ApiError::invalid_params(format!("{key} must be a string")))
}

fn required_i64(params: &Value, key: &str) -> Result<i64, ApiError> {
    params
        .get(key)
        .and_then(Value::as_i64)
        .ok_or_else(|| ApiError::invalid_params(format!("{key} must be an integer")))
}

fn optional_string(params: &Value, key: &str) -> Option<String> {
    params.get(key).and_then(Value::as_str).map(String::from)
}

fn optional_u32(params: &Value, key: &str) -> Option<u32> {
    params.get(key).and_then(Value::as_u64).map(|v| v as u32)
}

fn optional_f64(params: &Value, key: &str) -> Option<f64> {
    params.get(key).and_then(Value::as_f64)
}

// ── host types → JSON (protocol results never carry api_key) ──

fn session_info_json(s: &types::SessionInfo) -> Value {
    json!({
        "id": s.id,
        "title": s.title,
        "workspaceId": s.workspace_id,
        "createdAt": s.created_at,
        "messageCount": s.message_count,
        "parentId": s.parent_id,
        "forkPoint": s.fork_point,
        "agent": s.agent,
        "origin": s.origin,
    })
}

fn chat_message_json(m: &types::ChatMessage) -> Value {
    json!({
        "role": m.role,
        "content": m.content,
        // BUG-86: the page's own cursor — anchors "load earlier" and the
        // fork point without re-aligning against the full timeline.
        "seq": m.seq,
        "ts": m.ts,
        "usage": m.usage.as_ref().map(|u| json!({
            "inputTokens": u.input_tokens,
            "outputTokens": u.output_tokens,
        })),
        "durationMs": m.duration_ms,
        "doneTs": m.done_ts,
        "fileChanges": m.file_changes.as_ref().map(|files| files.iter().map(|f| json!({
            "path": f.path,
            "add": f.add,
            "del": f.del,
            "diff": f.diff,
        })).collect::<Vec<_>>()),
    })
}

fn session_message_json(m: &types::SessionMessage) -> Value {
    json!({ "id": m.id, "role": m.role, "content": m.content })
}

fn session_trace_json(trace: &types::SessionTrace) -> Value {
    json!({
        "entries": trace.entries.iter().map(|entry| {
            json!({
                "kind": entry.kind,
                "text": entry.text,
                "toolName": entry.tool_name,
                "toolInput": entry.tool_input,
                "toolOutput": entry.tool_output,
                "toolOk": entry.tool_ok,
                // WI-182 (mobile turn fold): wall-clock time of the source
                // event — lets the phone compute a real per-turn duration.
                "timeMs": entry.time_ms,
            })
        }).collect::<Vec<_>>(),
    })
}

fn compact_result_json(result: &types::CompactResult) -> Value {
    json!({ "before": result.before, "after": result.after })
}

fn skill_usage_json(record: &types::SkillUsageRecord) -> Value {
    json!({
        "sessionId": record.session_id,
        "projectId": record.project_id,
        "skill": record.skill,
        "version": record.version,
        "mode": record.mode,
        "ts": record.ts,
    })
}

fn binding_json(binding: &types::AgentModelBinding) -> Value {
    json!({
        "agentName": binding.agent_name,
        "activeModel": binding.active_model,
        "reasoningEffort": binding.reasoning_effort,
        "boundModels": binding.bound_models.iter().map(|m| {
            json!({ "id": m.id, "name": m.name, "enabled": m.enabled })
        }).collect::<Vec<_>>(),
    })
}

fn model_provider_json(provider: &types::ModelProvider) -> Value {
    json!({
        "id": provider.id,
        "name": provider.name,
        "providerType": provider.provider_type,
        "baseUrl": provider.base_url,
        "models": provider.models,
        "requiresApiKey": provider.requires_api_key,
        "enabled": provider.enabled,
        "apiFormat": provider.api_format,
        "reasoningEffort": provider.reasoning_effort,
    })
}

fn configurable_provider_json(provider: &types::ConfigurableProvider) -> Value {
    json!({
        "provider": provider.provider,
        "displayName": provider.display_name,
        "settingsNs": provider.settings_ns,
        "settingsPath": provider.settings_path,
        "declared": provider.declared,
        "configured": provider.configured,
    })
}

fn subagent_config_json(config: &types::SubagentConfig) -> Value {
    json!({
        "name": config.name,
        "description": config.description,
        "tools": config.tools,
        "readOnly": config.read_only,
        "enabled": config.enabled,
        "model": config.model,
    })
}

fn credential_info_json(info: &types::CredentialRecordInfo) -> Value {
    json!({
        "configured": info.configured,
        "kind": info.kind,
        "writable": info.writable,
    })
}

fn credential_entry_json(entry: &types::CredentialRecordEntry) -> Value {
    json!({ "key": entry.key, "kind": entry.kind })
}

fn memory_entry_json(memory: &types::MemoryEntry) -> Value {
    json!({
        "name": memory.name,
        "description": memory.description,
        "body": memory.body,
        "source": memory.source,
        "sourceAgent": memory.source_agent,
        "project": memory.project,
        "created": memory.created,
        "updated": memory.updated,
    })
}

fn mcp_server_json(server: &types::McpServerInfo) -> Value {
    json!({
        "name": server.name,
        "transport": server.transport,
        "command": server.command,
        "args": server.args,
        "env": server.env,
        "url": server.url,
        "oauthConnected": server.oauth_connected,
        "toolCount": server.tool_count,
        "status": server.status,
    })
}

fn mcp_test_json(result: &types::McpTestResult) -> Value {
    json!({
        "ok": result.ok,
        "toolCount": result.tool_count,
        "tools": result.tools,
        "error": result.error,
    })
}

fn mcp_tool_test_json(result: &types::McpToolTestResult) -> Value {
    json!({
        "ok": result.ok,
        "output": result.output,
        "error": result.error,
    })
}

fn scheduled_task_json(task: &types::ScheduledTaskInfo) -> Value {
    json!({
        "name": task.name,
        "prompt": task.prompt,
        "model": task.model,
        "agent": task.agent,
        "workspace": task.workspace,
        "enabled": task.enabled,
        "modeType": task.mode_type,
        "period": task.period,
        "time": task.time,
        "dayOfWeek": task.day_of_week,
        "dayOfMonth": task.day_of_month,
        "month": task.month,
        "hours": task.hours,
        "daysOfWeek": task.days_of_week,
        "datetime": task.datetime,
        "startDate": task.start_date,
        "endDate": task.end_date,
        "skills": task.skills,
        "description": task.description,
        "lastRun": task.last_run,
        "nextRun": task.next_run,
        "lastStatus": task.last_status,
    })
}

fn job_info_json(job: &types::JobInfo) -> Value {
    json!({
        "taskName": job.task_name,
        "status": job.status,
        "startedAt": job.started_at,
        "summary": job.summary,
        "sessionId": job.session_id,
    })
}

fn task_run_json(run: &types::TaskRunRecord) -> Value {
    json!({
        "timestamp": run.timestamp,
        "status": run.status,
        "summary": run.summary,
        "sessionId": run.session_id,
    })
}

fn task_run_with_name_json(run: &types::TaskRunRecordWithName) -> Value {
    json!({
        "taskName": run.task_name,
        "timestamp": run.timestamp,
        "status": run.status,
        "summary": run.summary,
        "sessionId": run.session_id,
    })
}

fn market_entry_json(entry: &types::MarketEntry) -> Value {
    json!({
        "name": entry.name,
        "description": entry.description,
        "url": entry.url,
        "entryType": entry.entry_type,
        "category": entry.category,
        "version": entry.version,
    })
}

fn attachment_ref_json(attachment: &types::AttachmentRef) -> Value {
    json!({
        "id": attachment.id,
        "mediaType": attachment.media_type,
        "width": attachment.width,
        "height": attachment.height,
        "bytes": attachment.bytes,
        "name": attachment.name,
        "originalWidth": attachment.original_width,
        "originalHeight": attachment.original_height,
    })
}

fn stored_attachment_json(attachment: &types::StoredAttachment) -> Value {
    json!({
        "id": attachment.id,
        "mediaType": attachment.media_type,
        "width": attachment.width,
        "height": attachment.height,
        "base64": attachment.base64,
    })
}

fn workspace_info_json(w: &types::WorkspaceInfo) -> Value {
    json!({
        "id": w.id,
        "path": w.path,
        "title": w.title,
        "createdAt": w.created_at,
        "sessionIds": w.session_ids,
    })
}

fn worktree_status_json(status: &types::WorktreeStatus) -> Value {
    json!({
        "isRepo": status.is_repo,
        "branch": status.branch,
        "isWorktree": status.is_worktree,
        "worktreePath": status.worktree_path,
        "dirty": status.dirty,
    })
}

fn tool_info_json(t: &types::ToolInfo) -> Value {
    json!({
        "name": t.name,
        "description": t.description,
        "requiresApproval": t.requires_approval,
    })
}

fn agent_info_json(a: &types::AgentInfo) -> Value {
    json!({
        "name": a.name,
        "description": a.description,
        "version": a.version,
    })
}

fn plugin_entry_json(p: &types::PluginEntry) -> Value {
    let ui = p.ui.as_ref().map(|ui| {
        let entry_url = format!("starriver-plugin://{}/ui/{}", p.name, ui.entry);
        let theme_css = ui.theme.as_deref().and_then(|theme| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            let path = std::path::Path::new(&home)
                .join(".starriver")
                .join("plugins")
                .join(&p.name)
                .join("ui")
                .join(theme);
            std::fs::read_to_string(&path)
                .ok()
                .filter(|css| css.len() <= 64 * 1024)
        });
        json!({
            "entry": ui.entry,
            "entryUrl": entry_url,
            "tabs": ui.tabs.iter().map(|t| json!({ "id": t.id, "title": t.title })).collect::<Vec<_>>(),
            "permissions": ui.permissions,
            "theme": ui.theme,
            "themeCss": theme_css,
        })
    });
    json!({
        "name": p.name,
        "description": p.description,
        "version": p.version,
        "skills": p.skills,
        "ui": ui,
        "sourceUrl": p.source_url,
        "installedAt": p.installed_at,
    })
}

/// Model entry: **strips api_key**, exposing only whether one is configured.
fn model_entry_json(m: &types::ModelEntry) -> Value {
    json!({
        "id": m.id,
        "name": m.name,
        "modelType": m.model_type,
        "providerType": m.provider_type,
        "baseUrl": m.base_url,
        "hasApiKey": !m.api_key.is_empty(),
        "modelPath": m.model_path,
        "models": m.models.iter().map(|v| json!({ "id": v.id, "name": v.name })).collect::<Vec<_>>(),
        "defaultModel": m.default_model,
        "concurrency": m.concurrency,
        "maxTokens": m.max_tokens,
        "contextWindow": m.context_window,
        "temperature": m.temperature,
        "apiFormat": m.api_format,
        "reasoningEffort": m.reasoning_effort,
        "modelLabel": m.model_label,
        "supportsVision": m.supports_vision,
        "visionSupported": m.vision_supported,
    })
}

/// Full protocol schema (version + method catalog + per-method
/// parameter/result schemas), used to export `desktop/electron/protocol.schema.json`
/// and to generate the TS client.
pub fn protocol_schema() -> serde_json::Value {
    build().to_schema()
}

pub fn protocol_schema_json() -> String {
    serde_json::to_string_pretty(&protocol_schema()).expect("schema serializes")
}

/// Builds the dispatcher shared by HTTP/daemon from the registry + host.
pub fn dispatcher(
    registry: MethodRegistry<HostRuntime>,
    host: std::sync::Arc<HostRuntime>,
) -> crate::http_server::Dispatcher {
    use starriver_protocol::wire::{ApiError, EventEnvelope, Request, Response};
    // BUG-5: the daemon is a long-lived host — its due scheduled tasks
    // auto-fire on the background tick.
    host.start_scheduler_tick();
    std::sync::Arc::new(
        move |request: &Request,
              emit: Option<std::sync::Arc<dyn Fn(EventEnvelope) + Send + Sync>>|
              -> Result<Response, ApiError> {
            registry.dispatch(host.as_ref(), request, None, emit)
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http_server;
    use starriver_protocol::wire::Request;

    fn request_with_id(id: &str, method: &str, params: Value) -> Request {
        Request {
            v: starriver_protocol::PROTOCOL_VERSION,
            id: id.into(),
            method: method.into(),
            params,
        }
    }

    #[test]
    fn catalog_exposes_core_methods() {
        let registry = build();
        for name in [
            "protocol.info",
            "session.execute",
            "session.list",
            "sessions.running",
            "approval.respond",
            "interaction.respond",
            "tools.list",
            "plugins.list",
            "workspaces.create",
        ] {
            assert!(registry.get(name).is_some(), "missing method {name}");
        }
    }

    /// WI-31 regression: `settings.update` must accept a real patch object —
    /// the old schema (`object(&[], &["*"])`) required a literal `*` key while
    /// forbidding every other property, so no caller could ever succeed.
    #[test]
    fn settings_update_accepts_free_form_patch() {
        let spec = build()
            .get("settings.update")
            .expect("settings.update")
            .clone();
        let schema = spec.params_schema.clone();
        let ok = serde_json::json!({ "patch": { "prompt": { "include_identity": true } } });
        assert!(
            starriver_protocol::schema::validate(&schema, &ok).is_ok(),
            "real patch must validate"
        );
        let missing = serde_json::json!({});
        assert!(
            starriver_protocol::schema::validate(&schema, &missing).is_err(),
            "patch itself stays required"
        );
    }

    /// Locks down the full product method surface: every UI-facing method on
    /// the NAPI side should have a protocol counterpart.
    #[test]
    fn catalog_covers_full_product_surface() {
        let registry = build();
        let expected = [
            "session.list",
            "session.messages",
            "session.timeline",
            "session.trace",
            "session.fork",
            "session.delete",
            "session.rename",
            "session.archive",
            "session.unarchive",
            "session.attachWorkspace",
            "session.listArchived",
            "session.execute",
            "session.cancel",
            "session.compact",
            "session.usage",
            "session.export",
            "session.setApprovalMode",
            "session.setPermissionPreset",
            "session.getPermission",
            "approval.respond",
            "plan.reviewDecision",
            "interaction.respond",
            "tools.list",
            "agentPresets.list",
            "agents.list",
            "agents.scan",
            "agents.create",
            "agents.export",
            "agents.bindings",
            "agents.bindings.setActive",
            "skills.list",
            "skills.scan",
            "skills.create",
            "skills.update",
            "skills.delete",
            "skills.setAlwaysOn",
            "skills.usage",
            "plugins.list",
            "plugins.install",
            "plugins.uninstall",
            "models.list",
            "models.providers.list",
            "models.add",
            "models.remove",
            "configurableProviders.list",
            "subagents.list",
            "subagents.set",
            "credentials.describe",
            "credentials.read",
            "credentials.list",
            "credentials.setApiKey",
            "credentials.unset",
            "memory.list",
            "memory.recall",
            "memory.upsert",
            "memory.delete",
            "memory.clear",
            "memory.ingest",
            "memory.status",
            "memory.export",
            "mcp.list",
            "mcp.add",
            "mcp.remove",
            "mcp.refresh",
            "mcp.oauth.connect",
            "mcp.oauth.disconnect",
            "mcp.testServer",
            "mcp.testTool",
            "scheduled.list",
            "scheduled.add",
            "scheduled.update",
            "scheduled.remove",
            "scheduled.runNow",
            "jobs.list",
            "jobs.taskRuns",
            "jobs.taskRun.history",
            "market.list",
            "workspace.instructions",
            "workspace.diff",
            "workspace.worktreeStatus",
            "workspaces.list",
            "workspaces.create",
            "workspaces.rename",
            "workspaces.delete",
            "workspace.diffStat",
            "workspaces.activate",
            "project.agents.init",
            "review.start",
            "attachments.admit",
            "attachments.read",
        ];
        let missing: Vec<&str> = expected
            .iter()
            .copied()
            .filter(|name| registry.get(name).is_none())
            .collect();
        assert!(missing.is_empty(), "missing protocol methods: {missing:?}");
    }

    #[test]
    fn models_result_must_not_leak_api_key() {
        let schema = protocol_schema();
        let models = schema["methods"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["name"] == "models.list")
            .expect("models.list in schema");
        let serialized = serde_json::to_string(&models).unwrap();
        assert!(
            !serialized.contains("apiKey"),
            "models.list schema mentions apiKey"
        );
    }

    #[test]
    fn committed_protocol_schema_is_up_to_date() {
        let expected = protocol_schema_json();
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../desktop/electron/protocol.schema.json");
        let actual = std::fs::read_to_string(&path).unwrap_or_else(|_| {
            panic!(
                "missing {} — regenerate with: cargo run -p starriver-desktop --bin dump_protocol_schema",
                path.display()
            )
        });
        assert_eq!(
            actual.trim().replace("\r\n", "\n"),
            expected.trim(),
            "protocol.schema.json is stale — regenerate with: cargo run -p starriver-desktop --bin dump_protocol_schema"
        );
    }

    /// Catalog entries generated by `#[api_method]` run a behavior loop
    /// against the real host: create session → `session.rename` (with an
    /// Option parameter) → `session.delete`, verifying that the macro's
    /// parameter extraction / error mapping / result serialization matches
    /// the hand-written catalog's behavior.
    #[test]
    fn macro_generated_methods_dispatch_against_real_host() {
        use tempfile::TempDir;

        let dir = TempDir::new().expect("tempdir");
        let db = dir.path().join("test.db");
        let config = format!(
            "version: 1\nmode: offline\ninference: {{ engines: [] }}\nagent: {{}}\nserver: {{ http: {{ enabled: false, bind: \"127.0.0.1:0\" }} }}\nsession: {{ db_path: \"{}\" }}\n",
            db.display().to_string().replace('\\', "/")
        );
        let host = std::sync::Arc::new(
            starriver_host::HostRuntime::new(Some(config)).expect("host boots"),
        );
        let registry = build();

        // 1) With no engines, execute creates a session (turnError terminal
        // state; the session is still persisted).
        let mut rx = host
            .execute(
                "hello".into(),
                Some("macro-session".into()),
                None,
                None,
                None,
                None,
                None,
            )
            .expect("execute");
        let handle = host.runtime_handle();
        handle.block_on(async move {
            let mut rx = rx;
            for _ in 0..50 {
                match rx.recv().await {
                    Ok(ev) => {
                        if ev["type"] == "turnError" || ev["type"] == "turnEnd" {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        assert!(
            host.list_sessions(None)
                .iter()
                .any(|s| s.id == "macro-session"),
            "session must exist after execute"
        );

        // 2) session.rename: macro-generated, title is an Option parameter.
        let response = registry
            .dispatch(
                &host,
                &request_with_id(
                    "r-rename",
                    "session.rename",
                    json!({ "sessionId": "macro-session", "title": "New Title" }),
                ),
                None,
                None,
            )
            .unwrap();
        assert!(response.ok, "{response:?}");
        let renamed = host
            .list_sessions(None)
            .into_iter()
            .find(|s| s.id == "macro-session")
            .expect("session still listed");
        assert_eq!(renamed.title, "New Title", "macro rename must apply");

        // 3) session.delete: macro-generated, result is {}.
        let response = registry
            .dispatch(
                &host,
                &request_with_id(
                    "r-delete",
                    "session.delete",
                    json!({ "sessionId": "macro-session" }),
                ),
                None,
                None,
            )
            .unwrap();
        assert!(response.ok, "{response:?}");
        assert!(
            !host
                .list_sessions(None)
                .iter()
                .any(|s| s.id == "macro-session"),
            "macro delete must remove the session"
        );

        // 4) Missing required parameter: schema validation intercepts before
        //    the handler (same as hand-written).
        let error = registry
            .dispatch(
                &host,
                &request_with_id("r-missing", "session.delete", json!({})),
                None,
                None,
            )
            .unwrap_err();
        assert_eq!(error.code, "INVALID_PARAMS");

        // 5) skills.delete (#[api_method] result attribute): host returns
        //    Vec<SkillInfo> → protocol JSON array (camelCase); the result
        //    conversion function takes effect.
        let response = registry
            .dispatch(
                &host,
                &request_with_id(
                    "r-skills-delete",
                    "skills.delete",
                    json!({ "name": "no-such-skill" }),
                ),
                None,
                None,
            )
            .unwrap();
        assert!(response.ok, "{response:?}");
        let skills = response.result.unwrap();
        assert!(skills.is_array(), "skills.delete result must be an array");
    }

    /// FPM acceptance #5: two HTTP clients operate on the same session
    /// concurrently — single input slot (second execute gets SESSION_BUSY),
    /// SSE events broadcast by subscription, replay via `after=` on
    /// reconnect; respond gets APPROVAL_NOT_PENDING when no approval is pending.
    #[test]
    fn multi_client_single_input_slot_and_event_broadcast() {
        use futures::StreamExt as _;
        use tempfile::TempDir;

        let dir = TempDir::new().expect("tempdir");
        let db = dir.path().join("test.db");
        let config = format!(
            "version: 1\nmode: offline\ninference: {{ engines: [] }}\nagent: {{}}\nserver: {{ http: {{ enabled: false, bind: \"127.0.0.1:0\" }} }}\nsession: {{ db_path: \"{}\" }}\n",
            db.display().to_string().replace('\\', "/")
        );
        let host = std::sync::Arc::new(
            starriver_host::HostRuntime::new(Some(config)).expect("host boots"),
        );
        let dispatch = dispatcher(build(), std::sync::Arc::clone(&host));
        let handle = host.runtime_handle();
        handle.block_on(async {
            let server = http_server::spawn(dispatch, handle.clone())
                .await
                .expect("http server");
            let base = format!("http://127.0.0.1:{}", server.port());
            let token = server.token().to_string();
            let client = reqwest::Client::new();

            async fn invoke(
                client: &reqwest::Client,
                base: &str,
                token: &str,
                id: &str,
                method: &str,
                params: serde_json::Value,
            ) -> serde_json::Value {
                let body =
                    serde_json::json!({ "v": 1, "id": id, "method": method, "params": params });
                let response = client
                    .post(format!("{base}/api/v1/invoke"))
                    .bearer_auth(token)
                    .json(&body)
                    .send()
                    .await
                    .expect("invoke");
                response.json::<serde_json::Value>().await.expect("json")
            }

            // Client A: SSE subscribe + execute session "mc" (no engines;
            // events end in a turnError terminal state).
            let sse = client
                .get(format!("{base}/api/v1/events?token={token}"))
                .send()
                .await
                .expect("sse connect");
            let mut sse = sse.bytes_stream();
            let first = invoke(
                &client,
                &base,
                &token,
                "r-a",
                "session.execute",
                serde_json::json!({ "task": "a", "sessionId": "mc" }),
            )
            .await;
            assert_eq!(first["ok"], true, "{first}");
            let sub_a = first["result"]["subscription"]
                .as_str()
                .unwrap()
                .to_string();

            // Client B: concurrent execute on the same session →
            // SESSION_BUSY (single input slot).
            let second = invoke(
                &client,
                &base,
                &token,
                "r-b",
                "session.execute",
                serde_json::json!({ "task": "b", "sessionId": "mc" }),
            )
            .await;
            assert_eq!(second["ok"], false, "{second}");
            assert_eq!(second["error"]["code"], "SESSION_BUSY", "{second}");

            // SSE events broadcast by subscription: wait for A's terminal
            // run event (carrying sub_a).
            let mut saw_sub_a = false;
            let mut saw_terminal = false;
            let mut buffer = String::new();
            // Poll with a deadline instead of bailing on timeout: under a
            // fully parallel run, CPU contention may delay the first SSE
            // chunk past 200ms — on timeout keep polling (only exit when
            // the stream ends).
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while std::time::Instant::now() < deadline && !(saw_sub_a && saw_terminal) {
                match tokio::time::timeout(std::time::Duration::from_millis(200), sse.next()).await
                {
                    Ok(Some(chunk)) => {
                        let chunk = chunk.expect("sse chunk");
                        buffer.push_str(&String::from_utf8_lossy(&chunk));
                        let frame: Vec<String> =
                            buffer.split("\n\n").map(|part| part.to_string()).collect();
                        buffer = frame.last().cloned().unwrap_or_default();
                        for part in &frame[..frame.len().saturating_sub(1)] {
                            for line in part.lines() {
                                if let Some(data) = line.strip_prefix("data: ") {
                                    let value: serde_json::Value =
                                        serde_json::from_str(data).unwrap_or_default();
                                    if value["subscription"] == sub_a {
                                        saw_sub_a = true;
                                    }
                                    if matches!(
                                        value["type"].as_str(),
                                        Some("turnEnd" | "turnError" | "cancelled")
                                    ) {
                                        saw_terminal = true;
                                    }
                                }
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(_) => {}
                }
            }
            assert!(saw_sub_a, "SSE must broadcast sub_a events");
            assert!(saw_terminal, "SSE must carry the terminal event");

            // Reconnect replay: the after parameter resends (a new
            // connection carries events after the last received global_seq).
            let sse_replay = client
                .get(format!("{base}/api/v1/events?token={token}&after=1"))
                .send()
                .await
                .expect("sse replay");
            // SSE is a continuous stream (the server does not close it), so
            // we can't call `text()` and wait for EOF; read the first chunk
            // then disconnect, checking that the replayed history does not
            // include the already-received global_seq=1.
            let mut replay_buf = String::new();
            {
                let mut replay_stream = sse_replay.bytes_stream();
                if let Ok(Some(chunk)) = tokio::time::timeout(
                    std::time::Duration::from_millis(500),
                    replay_stream.next(),
                )
                .await
                {
                    if let Ok(bytes) = chunk {
                        replay_buf.push_str(&String::from_utf8_lossy(&bytes));
                    }
                }
            }
            assert!(
                !replay_buf.contains("\"global_seq\":1"),
                "after=1 must not replay seen events"
            );

            // Approval first-come-first-served is observable: respond fails
            // explicitly when nothing is pending.
            let approval = invoke(
                &client,
                &base,
                &token,
                "r-ap",
                "approval.respond",
                serde_json::json!({ "sessionId": "no-pending", "allowed": true }),
            )
            .await;
            assert_eq!(approval["ok"], false, "{approval}");
            assert_eq!(
                approval["error"]["code"], "APPROVAL_NOT_PENDING",
                "{approval}"
            );

            server.shutdown();
        });
    }
}

/// WI-17 napi/protocol contract tests: snapshot-lock the approval-cluster
/// method specs (params/result schemas, permissions, streaming flag) so any
/// change to the method surface is a deliberate test update, not silent
/// drift. The desktop NAPI surface and the daemon share this exact catalog,
/// so these snapshots lock both transports at once.
#[cfg(test)]
mod approval_contract_tests {
    use super::*;

    /// (method name, expected spec fields). Result schemas: the catalog
    /// currently declares `any_schema()` for non-streaming results except
    /// where noted — the *declared* shape is what clients generate from, so
    /// even `any` is part of the contract.
    fn locked_specs() -> Vec<(&'static str, Value)> {
        vec![
            (
                "approval.respond",
                json!({
                    "name": "approval.respond",
                    "params": {
                        "type": "object",
                        "properties": {
                            "sessionId": { "type": "string" },
                            "allowed": { "type": "boolean" },
                            "remember": { "type": "boolean" },
                        },
                        "required": ["sessionId", "allowed"],
                        "additionalProperties": false,
                    },
                    "result": {},
                    "permissions": ["approval:respond"],
                    "streaming": false,
                }),
            ),
            (
                "approval.listRules",
                json!({
                    "name": "approval.listRules",
                    "params": {
                        "type": "object",
                        "properties": { "sessionId": { "type": "string" } },
                        "required": ["sessionId"],
                        "additionalProperties": false,
                    },
                    "result": {},
                    "permissions": ["session:read"],
                    "streaming": false,
                }),
            ),
            (
                "approval.revokeRule",
                json!({
                    "name": "approval.revokeRule",
                    "params": {
                        "type": "object",
                        "properties": {
                            "sessionId": { "type": "string" },
                            "ruleId": { "type": "string" },
                        },
                        "required": ["sessionId", "ruleId"],
                        "additionalProperties": false,
                    },
                    "result": {},
                    "permissions": ["session:write"],
                    "streaming": false,
                }),
            ),
            (
                "session.setApprovalMode",
                json!({
                    "name": "session.setApprovalMode",
                    "params": {
                        "type": "object",
                        "properties": {
                            "sessionId": { "type": "string" },
                            "mode": { "type": "string" },
                        },
                        "required": ["sessionId", "mode"],
                        "additionalProperties": false,
                    },
                    "result": {},
                    "permissions": ["session:write"],
                    "streaming": false,
                }),
            ),
            (
                "session.setExecutionMode",
                json!({
                    "name": "session.setExecutionMode",
                    "params": {
                        "type": "object",
                        "properties": { "mode": { "type": "string" } },
                        "required": ["mode"],
                        "additionalProperties": false,
                    },
                    "result": {},
                    "permissions": ["session:write"],
                    "streaming": false,
                }),
            ),
            (
                "session.getExecutionMode",
                json!({
                    "name": "session.getExecutionMode",
                    "params": {
                        "type": "object",
                        "properties": { "sessionId": { "type": "string" } },
                        "required": [],
                        "additionalProperties": false,
                    },
                    "result": {},
                    "permissions": ["session:read"],
                    "streaming": false,
                }),
            ),
            (
                "session.confirmPlan",
                json!({
                    "name": "session.confirmPlan",
                    "params": {
                        "type": "object",
                        "properties": { "sessionId": { "type": "string" } },
                        "required": ["sessionId"],
                        "additionalProperties": false,
                    },
                    "result": {},
                    "permissions": ["session:write"],
                    "streaming": false,
                }),
            ),
            (
                "plan.reviewDecision",
                json!({
                    "name": "plan.reviewDecision",
                    "params": {
                        "type": "object",
                        "properties": {
                            "sessionId": { "type": "string" },
                            "requestId": { "type": "string" },
                            "approved": { "type": "boolean" },
                            "feedback": { "type": "string" },
                        },
                        "required": ["sessionId", "approved"],
                        "additionalProperties": false,
                    },
                    "result": {},
                    "permissions": ["session:write"],
                    "streaming": false,
                }),
            ),
            (
                "session.setPermissionPreset",
                json!({
                    "name": "session.setPermissionPreset",
                    "params": {
                        "type": "object",
                        "properties": {
                            "sessionId": { "type": "string" },
                            "preset": { "type": "string" },
                        },
                        "required": ["sessionId", "preset"],
                        "additionalProperties": false,
                    },
                    "result": {},
                    "permissions": ["session:write"],
                    "streaming": false,
                }),
            ),
            (
                "session.getPermission",
                json!({
                    "name": "session.getPermission",
                    "params": {
                        "type": "object",
                        "properties": {
                            "sessionId": { "type": "string" },
                        },
                        "required": ["sessionId"],
                        "additionalProperties": false,
                    },
                    "result": {},
                    "permissions": ["session:read"],
                    "streaming": false,
                }),
            ),
        ]
    }

    #[test]
    fn approval_cluster_method_specs_match_the_locked_contract() {
        let registry = build();
        let schema = registry.to_schema();
        let methods: std::collections::HashMap<&str, &Value> = schema["methods"]
            .as_array()
            .expect("methods array")
            .iter()
            .map(|spec| (spec["name"].as_str().expect("name"), spec))
            .collect();
        for (name, expected) in locked_specs() {
            let actual: &Value = *methods
                .get(name)
                .unwrap_or_else(|| panic!("method `{name}` missing from the catalog"));
            assert_eq!(
                actual, &expected,
                "method spec for `{name}` drifted from the locked contract"
            );
        }
    }

    #[test]
    fn approval_cluster_lock_covers_every_approval_method() {
        // Guard the guard: every catalog method whose name carries the
        // approval vocabulary must appear in the locked set (so adding a new
        // approval method forces a deliberate contract entry).
        let registry = build();
        let names: Vec<String> = registry
            .catalog()
            .iter()
            .filter_map(|spec| spec["name"].as_str().map(String::from))
            .filter(|name| {
                name.starts_with("approval.")
                    || name.to_lowercase().contains("approvalmode")
                    || name.to_lowercase().contains("executionmode")
            })
            .collect();
        let locked: Vec<&str> = locked_specs().iter().map(|(name, _)| *name).collect();
        for name in names {
            assert!(
                locked.contains(&name.as_str()),
                "approval-cluster method `{name}` has no locked contract entry"
            );
        }
    }

    #[test]
    fn approval_respond_rejects_unknown_params_at_the_schema_layer() {
        // The contract is not just documentation: the declared schema is
        // enforced at dispatch (napi and HTTP share this path). Params are
        // validated before the handler runs, so an offline host suffices.
        let dir = tempfile::TempDir::new().expect("tempdir");
        let config = format!(
            "version: 1\nmode: offline\ninference: {{ engines: [] }}\nagent: {{}}\nserver: {{ http: {{ enabled: false, bind: \"127.0.0.1:0\" }} }}\nsession: {{ db_path: \"{}\" }}\n",
            dir.path()
                .join("test.db")
                .display()
                .to_string()
                .replace('\\', "/")
        );
        let config_path = dir.path().join("settings.yaml");
        std::fs::write(&config_path, config).expect("write config");
        let registry = build();
        let host =
            starriver_host::HostRuntime::new(Some(config_path.to_string_lossy().into_owned()))
                .expect("host boots");
        let error = registry
            .dispatch(
                &host,
                &starriver_protocol::wire::Request {
                    v: starriver_protocol::wire::PROTOCOL_VERSION,
                    id: "r-unknown-param".into(),
                    method: "approval.respond".into(),
                    params: json!({ "sessionId": "s1", "allowed": true, "hacker": 1 }),
                },
                None,
                None,
            )
            .unwrap_err();
        assert_eq!(error.code, "INVALID_PARAMS");
    }

    #[test]
    fn approval_rule_methods_dispatch_on_offline_host() {
        // WI-16 wiring: listRules/revokeRule are dispatchable through the
        // shared catalog (napi + HTTP). On an unknown session listing is
        // tolerant (empty) and revocation fails loudly.
        let dir = tempfile::TempDir::new().expect("tempdir");
        let config = format!(
            "version: 1\nmode: offline\ninference: {{ engines: [] }}\nagent: {{}}\nserver: {{ http: {{ enabled: false, bind: \"127.0.0.1:0\" }} }}\nsession: {{ db_path: \"{}\" }}\n",
            dir.path()
                .join("test.db")
                .display()
                .to_string()
                .replace('\\', "/")
        );
        let config_path = dir.path().join("settings.yaml");
        std::fs::write(&config_path, config).expect("write config");
        let registry = build();
        let host =
            starriver_host::HostRuntime::new(Some(config_path.to_string_lossy().into_owned()))
                .expect("host boots");
        let request = |method: &str, params: Value| starriver_protocol::wire::Request {
            v: starriver_protocol::wire::PROTOCOL_VERSION,
            id: format!("r-{method}"),
            method: method.into(),
            params,
        };
        let list = registry
            .dispatch(
                &host,
                &request("approval.listRules", json!({ "sessionId": "no-such" })),
                None,
                None,
            )
            .expect("listRules dispatches");
        assert!(list.ok, "{list:?}");
        assert_eq!(list.result.unwrap(), json!([]));
        let revoke = registry.dispatch(
            &host,
            &request(
                "approval.revokeRule",
                json!({ "sessionId": "no-such", "ruleId": "ar-x" }),
            ),
            None,
            None,
        );
        assert!(revoke.is_err(), "revoking on an unknown session must fail");
    }
}
