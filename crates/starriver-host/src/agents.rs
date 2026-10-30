use std::sync::Arc;

use starriver_kernel::agent::AgentRegistry;
use starriver_kernel::agent::{AgentCancelCause, AgentCancelKind};
use starriver_kernel::tools::ToolRuntime;

use crate::{types, HostRuntime, ScriptExecutor};

impl HostRuntime {
    /// Activate a workspace for `agent_name`: resolve the directory from the
    /// host workspace registry (single source; the legacy projects table is
    /// gone) and point the agent's working dir at it. `None` deactivates back
    /// to no working dir; an unknown id is a tolerated no-op that leaves the
    /// activation cleared. SP3: instruction files no longer string-inject at
    /// activation — workspace instructions reach the model through the loop's
    /// agent-instructions snapshots now; the banner read path stays.
    ///
    /// The activation is persisted per agent in settings.yaml — the source
    /// `resolve_agent_config` reads on every run, so the working dir (and the
    /// UI's workspace/branch display) survive an app restart.
    pub fn set_active_workspace_for(
        &self,
        agent_name: &str,
        workspace_id: Option<String>,
    ) -> anyhow::Result<()> {
        if self.hub.get(agent_name).is_none() {
            anyhow::bail!("Agent not found: {agent_name}");
        }
        self.persist_workspace_activation(agent_name, workspace_id.as_deref());
        Ok(())
    }

    /// Workspace currently activated for `agent_name` (the id the UI restores
    /// its workspace/branch display from after a restart). Validated against
    /// the registry: a stale activation reads as none.
    pub fn get_active_workspace_for(&self, agent_name: &str) -> Option<String> {
        self.resolve_workspace_binding(agent_name).map(|(id, _)| id)
    }

    pub fn start_code_review(
        &self,
        files_json: String,
        dimensions_json: Option<String>,
    ) -> anyhow::Result<()> {
        let files: Vec<String> = serde_json::from_str(&files_json)
            .map_err(|e| anyhow::anyhow!("invalid files JSON: {e}"))?;
        let dimensions: Vec<String> = match dimensions_json {
            // Malformed JSON is an error, not a silently empty list (BUG-12
            // convention) — a typo must not quietly review zero dimensions.
            Some(d) if !d.trim().is_empty() => serde_json::from_str(&d)
                .map_err(|e| anyhow::anyhow!("invalid dimensions JSON: {e}"))?,
            _ => Vec::new(),
        };
        let options = crate::prompts::review::ReviewOptions { files, dimensions };
        let review_prompt = crate::prompts::review::review_system_prompt(&options);
        // E7 wiring: the review replacement enters the kernel as the
        // complete:true resident (whole-system swap while active).
        let inner_arc = self.inner.clone();
        let handle = self.runtime_handle.clone();
        handle.block_on(async move {
            let l = inner_arc.lock().await;
            if let Some(runtime) = l.as_ref() {
                let whitelist = vec!["read".to_string(), "grep".to_string()];
                if let Err(error) = runtime
                    .agent
                    .enter_review_mode(review_prompt, Some(whitelist))
                    .await
                {
                    tracing::warn!(error = %error, "kernel review registration failed");
                }
            }
        });
        Ok(())
    }

    /// Test observation hook: event types of a session's log — live registry
    /// first, durable storage otherwise (includes log-only events like
    /// `session/title-llm-request`).
    #[doc(hidden)]
    pub fn debug_session_log_event_types(&self, session_id: String) -> Vec<String> {
        let inner = self.inner.blocking_lock();
        let Some(inner) = inner.as_ref() else {
            return Vec::new();
        };
        let id = starriver_kernel::session::SessionId(session_id);
        if let Some(agent) = inner.agent.registry().get(&id) {
            return agent
                .session()
                .events()
                .iter()
                .map(|e| e.event_type.clone())
                .collect();
        }
        let persistence = Arc::clone(&inner.persistence);
        let inspection = self
            .runtime_handle
            .block_on(async move {
                starriver_kernel::persistence::SessionPersistence::load(
                    persistence.as_ref(),
                    id,
                    None,
                )
                .await
            })
            .ok();
        inspection
            .map(|inspection| {
                inspection
                    .events
                    .iter()
                    .map(|e| e.event_type.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Test observation hook: the kernel's current global system assembly
    /// (identity/persona/skills/review), rendered against a scratch session.
    #[doc(hidden)]
    pub fn debug_kernel_system_preview(&self) -> Option<String> {
        let inner = self.inner.blocking_lock();
        let inner = inner.as_ref()?;
        let kernel = Arc::clone(&inner.agent);
        self.runtime_handle.block_on(async move {
            let session = starriver_kernel::session::Session::new(
                starriver_kernel::session::SessionId("preview".into()),
                cordis_rslite::EventBus::new(),
            );
            let options = starriver_kernel::agent::AgentOptions::default();
            let render = starriver_kernel::context::RenderContext {
                session: &session,
                options: &options,
            };
            kernel
                .system_prompts()
                .assemble(&cordis_rslite::Context::new(), &render)
                .ok()
                .map(|assembled| assembled.system)
        })
    }

    /// Test observation hook: explicit durability barrier — drain all
    /// write-behind appends to storage.
    #[doc(hidden)]
    pub fn debug_persistence_flush(&self) -> Result<(), String> {
        let inner = self.inner.blocking_lock();
        let Some(inner) = inner.as_ref() else {
            return Err("runtime unavailable".into());
        };
        self.runtime_handle
            .block_on(inner.persistence.flush())
            .map_err(|e| e.to_string())
    }

    /// Test observation hook: data payloads of one event type in a session's
    /// log (live registry first, durable storage otherwise).
    #[doc(hidden)]
    pub fn debug_session_log_event_data(
        &self,
        session_id: String,
        event_type: String,
    ) -> Vec<starriver_kernel::json::JsonValue> {
        let inner = self.inner.blocking_lock();
        let Some(inner) = inner.as_ref() else {
            return Vec::new();
        };
        let id = starriver_kernel::session::SessionId(session_id);
        let pick = |events: &[starriver_kernel::session::SessionEvent]| {
            events
                .iter()
                .filter(|e| e.event_type == event_type)
                .map(|e| e.data.clone())
                .collect::<Vec<_>>()
        };
        if let Some(agent) = inner.agent.registry().get(&id) {
            return pick(&agent.session().events());
        }
        let persistence = Arc::clone(&inner.persistence);
        let inspection = self
            .runtime_handle
            .block_on(async move {
                // BUG-121: the write-behind batcher can still hold recent
                // events when the registry has already dropped the agent —
                // flush before reading so the durable view is not silently
                // empty or stale (observed as tool/call rows vanishing
                // between two consecutive debug queries).
                persistence.flush().await.ok();
                starriver_kernel::persistence::SessionPersistence::load(
                    persistence.as_ref(),
                    id,
                    None,
                )
                .await
            })
            .ok();
        inspection
            .map(|inspection| pick(&inspection.events))
            .unwrap_or_default()
    }

    /// Test/ops hook: register a self-declaring kernel adapter route
    /// (replaces the same provider id when present).
    #[doc(hidden)]
    pub fn debug_register_adapter(&self, adapter: Arc<dyn starriver_kernel::llm::LlmAdapter>) {
        let inner = self.inner.blocking_lock();
        if let Some(inner) = inner.as_ref() {
            inner.agent.register_provider_adapter(adapter);
        }
    }

    /// Test observation hook: all registered provider routes, in order.
    #[doc(hidden)]
    pub fn debug_provider_routes(&self) -> Vec<String> {
        let inner = self.inner.blocking_lock();
        inner
            .as_ref()
            .map(|inner| inner.agent.debug_provider_routes())
            .unwrap_or_default()
    }

    /// Test observation hook: the provider new agents will use.
    #[doc(hidden)]
    pub fn debug_default_provider(&self) -> Option<String> {
        let inner = self.inner.blocking_lock();
        inner.as_ref().map(|inner| inner.agent.default_provider())
    }

    pub fn list_agents(&self) -> Vec<types::AgentInfo> {
        self.hub
            .list()
            .into_iter()
            .map(|a| {
                let name = a.name;
                types::AgentInfo {
                    name,
                    description: a.description,
                    version: a.version,
                }
            })
            .collect()
    }

    pub fn scan_agents(&self) -> Vec<types::AgentInfo> {
        self.hub
            .scan()
            .unwrap_or_default()
            .into_iter()
            .map(|a| {
                let name = a.name;
                types::AgentInfo {
                    name,
                    description: a.description,
                    version: a.version,
                }
            })
            .collect()
    }

    /// WI-7 @file: suggest workspace files for the composer's `@` completion.
    /// Walks the workspace root (skipping VCS/build dirs and hidden entries),
    /// returning up to `limit` relative paths whose string contains `prefix`
    /// (case-insensitive). Bounded scan keeps huge repos responsive.
    pub fn suggest_files(&self, workspace_id: &str, prefix: &str, limit: usize) -> Vec<String> {
        let inner = self.inner.clone();
        let handle = self.runtime_handle.clone();
        let prefix = prefix.to_string();
        handle.block_on(async move {
            let l = inner.lock().await;
            let Some(runtime) = l.as_ref() else {
                return Vec::new();
            };
            let Some(workspace) = runtime
                .host_store
                .list_workspaces()
                .into_iter()
                .find(|workspace| workspace.id == workspace_id)
            else {
                return Vec::new();
            };
            let root = std::path::PathBuf::from(&workspace.path);
            let prefix_lower = prefix.to_ascii_lowercase();
            let skip_dirs = [
                ".git",
                "target",
                "node_modules",
                "dist",
                "build",
                ".next",
                "coverage",
            ];
            let mut out: Vec<String> = Vec::new();
            let mut scanned = 0usize;
            let mut stack = vec![root.clone()];
            while let Some(dir) = stack.pop() {
                let Ok(read_dir) = std::fs::read_dir(&dir) else {
                    continue;
                };
                for entry in read_dir.flatten() {
                    scanned += 1;
                    if scanned > 20_000 || out.len() >= limit {
                        return out;
                    }
                    let path = entry.path();
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name.starts_with('.') {
                        continue;
                    }
                    if path.is_dir() {
                        if !skip_dirs.contains(&name.as_str()) {
                            stack.push(path);
                        }
                        continue;
                    }
                    let Ok(rel) = path.strip_prefix(&root) else {
                        continue;
                    };
                    let rel = rel.to_string_lossy().replace('\\', "/");
                    if prefix_lower.is_empty() || rel.to_ascii_lowercase().contains(&prefix_lower) {
                        out.push(rel);
                    }
                }
            }
            out
        })
    }

    /// Workspace instructions banner (took over after flycode retired): same
    /// agent-instructions discovery chain as the agent loop (user-global layer +
    /// project chain); the UI banner and the model context read the same file
    /// set, the same byte budget (`prompt.instructions_max_bytes`, engine
    /// default otherwise), and the standalone FLYCODE.md channel is gone.
    pub fn read_workspace_instructions(&self, workspace_id: String) -> Option<String> {
        self.workspace_instructions_meta(workspace_id)
            .and_then(|meta| meta.content)
    }

    /// Banner + truncation metadata (WI-35 B): the budget honors settings —
    /// the same source the loop's projection uses — so the banner and the
    /// model context can no longer disagree about what fits.
    pub fn workspace_instructions_meta(
        &self,
        workspace_id: String,
    ) -> Option<crate::types::WorkspaceInstructionsMeta> {
        let budget = self
            .settings
            .read()
            .unwrap()
            .prompt
            .instructions_max_bytes
            .unwrap_or(starriver_agent_loop::instructions::MAX_BYTES);
        let inner = self.inner.clone();
        let handle = self.runtime_handle.clone();
        handle.block_on(async {
            let l = inner.lock().await;
            let runtime = l.as_ref()?;
            let workspace = runtime
                .host_store
                .list_workspaces()
                .into_iter()
                .find(|workspace| workspace.id == workspace_id)?;
            let cwd = std::path::PathBuf::from(&workspace.path);
            let home = std::env::var("HOME").ok().map(std::path::PathBuf::from);
            let mut files: Vec<starriver_agent_loop::instructions::InstructionFile> = Vec::new();
            if let Some(global) = starriver_agent_loop::instructions::global_layer(home.as_deref())
            {
                files.push(global);
            }
            files.extend(starriver_agent_loop::instructions::discover(&cwd));
            let raw = starriver_agent_loop::instructions::compose(&files);
            let truncated = raw.len() > budget;
            let composed =
                starriver_agent_loop::instructions::compose_budgeted_with(&files, budget);
            // BUG-63: the banner renders under the SAME budget as the model
            // context (the meta doc's whole promise), not the hardcoded
            // MAX_BYTES default.
            Some(crate::types::WorkspaceInstructionsMeta {
                content: render_workspace_instructions_with(&files, budget),
                truncated,
                bytes: composed.len(),
                budget,
            })
        })
    }

    pub fn list_tools(&self) -> Vec<types::ToolInfo> {
        let inner = self.inner.blocking_lock();
        let Some(inner) = inner.as_ref() else {
            return vec![];
        };
        let tools = inner.agent.tools();
        let mut list: Vec<types::ToolInfo> = tools
            .schemas()
            .into_iter()
            .map(|schema| types::ToolInfo {
                name: schema.name.clone(),
                description: schema.description.clone(),
                requires_approval: tools.requires_approval(&schema.name),
            })
            .collect();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        list
    }

    pub fn approve(&self, session_id: String, allowed: bool, remember: Option<bool>) -> bool {
        let inner = self.inner.blocking_lock();
        let Some(inner) = inner.as_ref() else {
            return false;
        };
        inner
            .agent
            .approval_bridge
            .approve(&session_id, allowed, remember.unwrap_or(false))
    }

    /// Plan fold (plan-mode parity): answer the session's pending plan
    /// review raised by `exit_plan_mode`. `request_id` addresses a specific
    /// review; `None` takes the session's latest (same convenience as
    /// approvals). Approval flips the logged plan state at the next accepted
    /// in-turn pre-step; rejection carries the feedback back to the model as
    /// the failed tool call.
    ///
    /// `preset` is the permission bundle chosen on the review card, applied
    /// atomically with the approval: under the Plan profile it is staged and
    /// lands bundled with the sandbox release (the read-only pairing holds
    /// until the pre-step lands); under other profiles it is appended
    /// immediately (same semantics as `session.setPermissionPreset`, minus
    /// the planning lock — no in-plan review exists there). An invalid preset
    /// name fails the call BEFORE anything is resolved or written.
    pub fn respond_plan_review(
        &self,
        session_id: String,
        request_id: Option<String>,
        approved: bool,
        feedback: Option<String>,
        preset: Option<String>,
    ) -> Result<bool, String> {
        // Validate first: an unknown preset must leave the review pending and
        // the session log untouched.
        let parsed = match preset.as_deref() {
            None => None,
            Some(name) => Some(
                crate::permission_presets::parse(name)
                    .map(|p| crate::preset::Preset {
                        sandbox: p.sandbox,
                        approval: p.approval,
                    })
                    .ok_or_else(|| format!("unknown permission preset `{name}`"))?,
            ),
        };
        if approved {
            if let Some(preset) = parsed {
                // Cheap pre-lock peek at the execution profile to decide
                // stage-vs-append; re-checked under the inner lock below.
                let plan_locked = {
                    let inner = self.inner.blocking_lock();
                    inner.as_ref().is_some_and(|inner| {
                        inner.agent.execution_mode()
                            == Some(crate::execution_mode::ExecutionMode::Plan)
                    })
                };
                if plan_locked {
                    let inner = self.inner.blocking_lock();
                    if let Some(inner) = inner.as_ref() {
                        inner
                            .agent
                            .plan_controller()
                            .stage_exit_preset(&session_id, preset);
                    }
                } else {
                    // Standard/unset profile: no read-only pairing to hold —
                    // write both knobs now, before the decision resolves.
                    // Same persistence path as session.setPermissionPreset
                    // (confirm_plan's thread+block_on pattern).
                    let inner = self.inner.clone();
                    let handle = self.runtime_handle.clone();
                    let append_session = session_id.clone();
                    let _ = std::thread::spawn(move || {
                        handle.block_on(async {
                            let l = inner.lock().await;
                            let Some(runtime) = l.as_ref() else {
                                return;
                            };
                            for (event, field, value) in [
                                ("sandbox/mode", "mode", preset.sandbox.as_str()),
                                ("approval/policy", "policy", preset.approval.as_str()),
                            ] {
                                let _ = crate::sessions::append_session_event(
                                    runtime,
                                    &append_session,
                                    event,
                                    starriver_kernel::json::JsonValue::Object(vec![(
                                        field.into(),
                                        starriver_kernel::json::JsonValue::String(value.into()),
                                    )]),
                                )
                                .await;
                            }
                        });
                    })
                    .join();
                }
            }
        } else if parsed.is_some() {
            // Rejected review: drop anything staged (defensive — nothing
            // should be staged for a rejection path).
            let inner = self.inner.blocking_lock();
            if let Some(inner) = inner.as_ref() {
                inner
                    .agent
                    .plan_controller()
                    .discard_staged_preset(&session_id);
            }
        }
        let inner = self.inner.blocking_lock();
        let Some(inner) = inner.as_ref() else {
            return Ok(false);
        };
        let bridge = &inner.agent.plan_review_bridge;
        let id = match request_id {
            Some(id) => id,
            None => match bridge.session_latest_request(&session_id) {
                Some(id) => id,
                None => return Ok(false),
            },
        };
        let decision = if approved {
            crate::plan_review_bridge::PlanDecision::Approved
        } else {
            crate::plan_review_bridge::PlanDecision::Rejected { feedback }
        };
        Ok(bridge.respond(&id, decision))
    }

    /// Final decision of the session's latest approval request when it was
    /// already resolved by another client (WI-16 multi-UI arbitration): the
    /// first resolution wins and late responders are told the outcome.
    pub fn approval_final_decision(
        &self,
        session_id: String,
    ) -> Option<crate::approval_bridge::ApprovalDecision> {
        let inner = self.inner.blocking_lock();
        let Some(inner) = inner.as_ref() else {
            return None;
        };
        let bridge = &inner.agent.approval_bridge;
        let request_id = bridge.session_latest_request(&session_id)?;
        bridge.resolution_of(&request_id)
    }

    /// WI-97: goal state (session-scoped, in-memory; the loop honors it).
    pub fn set_goal(
        &self,
        session_id: String,
        objective: String,
        token_budget: Option<u64>,
    ) -> anyhow::Result<()> {
        let inner = self.inner.blocking_lock();
        let Some(inner) = inner.as_ref() else {
            anyhow::bail!("runtime not loaded");
        };
        inner.goals.lock().unwrap().insert(
            session_id,
            crate::GoalState {
                objective,
                token_budget,
                tokens_used: 0,
                status: crate::GoalStatus::Active,
            },
        );
        Ok(())
    }

    pub fn get_goal(&self, session_id: &str) -> Option<serde_json::Value> {
        let inner = self.inner.blocking_lock();
        let value = {
            let inner = inner.as_ref()?;
            let goals = inner.goals.lock().unwrap();
            goals.get(session_id).map(|g| {
                serde_json::json!({
                    "objective": g.objective,
                    "tokenBudget": g.token_budget,
                    "tokensUsed": g.tokens_used,
                    "status": g.status.as_str(),
                })
            })
        };
        value
    }

    pub fn clear_goal(&self, session_id: &str) -> bool {
        let inner = self.inner.blocking_lock();
        let Some(inner) = inner.as_ref() else {
            return false;
        };
        let removed = inner.goals.lock().unwrap().remove(session_id).is_some();
        removed
    }

    /// WI-97: called from the usage-event path — accumulates tokens into
    /// the goal and flips BudgetLimited when over budget.
    pub(crate) fn goal_account_usage(&self, session_id: &str, tokens: u64) {
        let inner = self.inner.blocking_lock();
        let Some(inner) = inner.as_ref() else { return };
        let mut goals = inner.goals.lock().unwrap();
        if let Some(g) = goals.get_mut(session_id) {
            g.tokens_used = g.tokens_used.saturating_add(tokens);
            if let Some(budget) = g.token_budget {
                if g.tokens_used >= budget && g.status == crate::GoalStatus::Active {
                    g.status = crate::GoalStatus::BudgetLimited;
                }
            }
        }
    }

    /// WI-97: turnEnd check — Active + budget remaining → continue prompt.
    /// Returns Some(continuation-prompt) when the goal should drive on.
    pub(crate) fn goal_continuation(&self, session_id: &str) -> Option<String> {
        let inner = self.inner.blocking_lock();
        inner
            .as_ref()
            .and_then(|i| goal_continuation_for(i, session_id))
    }

    /// WI-96: structured review — resolve the target (git diff), run the
    /// review on a child thread (REVIEW prompt, no web), parse findings.
    /// Synchronous: the protocol handler parks its own thread.
    pub fn run_structured_review_sync(
        &self,
        target: crate::prompts::review::ReviewTarget,
        dimensions: Vec<String>,
        cwd: Option<std::path::PathBuf>,
    ) -> anyhow::Result<serde_json::Value> {
        let workdir = cwd.unwrap_or_else(|| {
            std::env::var("HOME")
                .ok()
                .or_else(|| std::env::var("USERPROFILE").ok())
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| std::path::PathBuf::from("."))
        });
        // BUG-93: resolve runs up to 4 git subprocesses — do it BEFORE taking
        // the global inner lock so the host API stays live while the diff is
        // being assembled.
        let (files, diff) = target
            .resolve(&workdir)
            .map_err(|e| anyhow::anyhow!("review target: {e}"))?;
        if files.is_empty() && diff.is_empty() {
            return Ok(serde_json::json!({
                "findings": [],
                "note": "target has no changes",
            }));
        }
        let prompt = crate::prompts::review::structured_review_prompt(&files, &diff, &dimensions);
        let kernel = {
            let inner = self.inner.blocking_lock();
            let Some(inner) = inner.as_ref() else {
                anyhow::bail!("runtime not loaded");
            };
            inner.agent.clone()
        };
        let handle = self.runtime_handle.clone();
        // BUG-93: a hung subagent must not park the protocol thread forever.
        const REVIEW_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);
        let output = handle
            .block_on(async move {
                match tokio::time::timeout(
                    REVIEW_TIMEOUT,
                    kernel.spawn_subagent(prompt, None, None),
                )
                .await
                {
                    Ok(result) => result,
                    Err(_) => Err(anyhow::anyhow!(
                        "structured review timed out after {}s",
                        REVIEW_TIMEOUT.as_secs()
                    )),
                }
            })
            .map_err(|e| anyhow::anyhow!("review thread failed: {e}"))?;
        let findings = crate::prompts::review::parse_review_findings(&output);
        Ok(serde_json::json!({
            "findings": findings,
            "files": files,
            "raw": output,
        }))
    }

    /// WI-98: read-only doctor checks.
    pub fn doctor(&self) -> Vec<serde_json::Value> {
        let mut checks: Vec<serde_json::Value> = Vec::new();
        let push =
            |checks: &mut Vec<serde_json::Value>, name: &str, status: &str, detail: String| {
                checks.push(serde_json::json!({
                    "name": name, "status": status, "detail": detail,
                }));
            };

        // 1) Config load.
        let settings = {
            self.settings
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        };
        let providers = &settings.llm.providers;
        if providers.is_empty() {
            push(
                &mut checks,
                "config",
                "FAIL",
                "no LLM providers configured".into(),
            );
        } else {
            push(
                &mut checks,
                "config",
                "PASS",
                format!("{} provider(s) loaded", providers.len()),
            );
        }

        // 2) Model route: default model exists?
        let has_default = providers.iter().any(|p| !p.default_model.is_empty());
        if has_default {
            push(
                &mut checks,
                "model-route",
                "PASS",
                "default model present".into(),
            );
        } else {
            push(
                &mut checks,
                "model-route",
                "WARN",
                "no default model on any provider".into(),
            );
        }

        // 3) Sandbox availability: confine a trivial command; rejection
        // means the OS backend is unavailable (approval-only fallback).
        let probe_policy = starriver_kernel::sandbox::SandboxPolicy {
            mode: starriver_kernel::sandbox::SandboxMode::WorkspaceWrite,
            workspace_root: std::env::temp_dir().to_string_lossy().into_owned(),
            session_id: None,
        };
        match <crate::sandbox::SystemSandbox as starriver_kernel::sandbox::SandboxProvider>::confine(
            &crate::sandbox::SystemSandbox::new(),
            vec!["true".into()],
            &probe_policy,
        ) {
            Ok(_) => push(
                &mut checks,
                "sandbox",
                "PASS",
                "OS sandbox available".into(),
            ),
            Err(e) => push(
                &mut checks,
                "sandbox",
                "WARN",
                format!("sandbox degraded (approval-only fallback): {e}"),
            ),
        }

        // 4) Disk: data directory size + volume free space.
        let data_dir = std::env::var("HOME")
            .ok()
            .or_else(|| std::env::var("USERPROFILE").ok())
            .map(|h| std::path::PathBuf::from(h).join(".starriver"))
            .unwrap_or_else(|| std::path::PathBuf::from(".starriver"));
        let dir_size = dir_size_bytes(&data_dir);
        let free = fs2_free_bytes(&data_dir);
        push(
            &mut checks,
            "disk",
            if dir_size > 8 * 1024 * 1024 * 1024 {
                "WARN"
            } else {
                "PASS"
            },
            format!(
                "data dir {:.1} MB{}",
                dir_size as f64 / 1024.0 / 1024.0,
                free.map(|f| format!(
                    ", volume free {:.1} GB",
                    f as f64 / 1024.0 / 1024.0 / 1024.0
                ))
                .unwrap_or_default(),
            ),
        );

        // 5) Session store integrity: SQLite quick_check.
        let db_path = data_dir.join("starriver.db");
        if db_path.exists() {
            match rusqlite::Connection::open_with_flags(
                &db_path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .and_then(|conn| {
                conn.query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0))
            }) {
                Ok(result) if result == "ok" => {
                    push(&mut checks, "session-db", "PASS", "quick_check ok".into())
                }
                Ok(other) => push(&mut checks, "session-db", "FAIL", other),
                Err(e) => push(&mut checks, "session-db", "FAIL", format!("{e}")),
            }
        } else {
            push(
                &mut checks,
                "session-db",
                "WARN",
                "database not found yet".into(),
            );
        }

        checks
    }

    /// WI-92 mid-turn steering: inject a user message into the RUNNING
    /// turn's next-step inbox (the kernel `steer` path — same semantics as
    /// the steering e2e test). Returns false when the session has no agent
    /// (never started / already disposed); a not-running agent still accepts
    /// the message via next-step (it runs on the next turn).
    pub fn steer(&self, session_id: String, text: String) -> bool {
        let inner = self.inner.blocking_lock();
        let Some(inner) = inner.as_ref() else {
            return false;
        };
        let Some(agent) = inner
            .agent
            .registry()
            .get(&starriver_kernel::session::SessionId(session_id))
        else {
            return false;
        };
        agent.steer(starriver_kernel::llm::Message {
            id: starriver_kernel::llm::MessageId(format!("steer-{}", uuid::Uuid::new_v4())),
            role: starriver_kernel::llm::MessageRole::User,
            content: vec![starriver_kernel::llm::ContentBlock::Text(text)],
            source: starriver_kernel::llm::MessageSource::User,
        });
        true
    }

    pub fn queue_list(&self, session_id: String) -> Vec<(String, String)> {
        let inner = self.inner.blocking_lock();
        let Some(inner) = inner.as_ref() else {
            return Vec::new();
        };
        let Some(agent) = inner
            .agent
            .registry()
            .get(&starriver_kernel::session::SessionId(session_id))
        else {
            return Vec::new();
        };
        agent
            .pending_messages()
            .next_turn
            .into_iter()
            .map(|m| (m.id.0.clone(), Self::message_queue_text(&m)))
            .collect()
    }

    /// Queue-surface text of a message: Text blocks verbatim, Image blocks
    /// reconstructed back to their markdown reference so the dock row shows
    /// (and an edit round-trips) the same `![name](sha256:…)` shape the
    /// renderer enqueued. Non-text non-image blocks contribute nothing.
    fn message_queue_text(m: &starriver_kernel::llm::Message) -> String {
        m.content
            .iter()
            .filter_map(|block| match block {
                starriver_kernel::llm::ContentBlock::Text(text) => Some(text.clone()),
                starriver_kernel::llm::ContentBlock::Image { attachment } => {
                    let file_id = attachment.as_str_at("fileId").unwrap_or("");
                    if file_id.is_empty() {
                        return None;
                    }
                    let name = attachment.as_str_at("name").unwrap_or("image");
                    Some(format!("![{name}]({file_id})"))
                }
                _ => None,
            })
            .collect::<Vec<String>>()
            .join("\n")
    }

    pub fn queue_add(&self, session_id: String, text: String) -> Option<String> {
        let inner = self.inner.blocking_lock();
        let Some(inner) = inner.as_ref() else {
            return None;
        };
        let Some(agent) = inner
            .agent
            .registry()
            .get(&starriver_kernel::session::SessionId(session_id))
        else {
            return None;
        };
        let id = format!("queue-{}", uuid::Uuid::new_v4());
        // Queued messages keep their images: resolve markdown image refs
        // exactly like the direct-send execute path, so the claimed message
        // carries real Image blocks (the request-time projection re-hydrates
        // them) instead of dead markdown the model can never see. The store
        // is content-addressed and stateless, so opening the canonical root
        // here is coherent with the runtime's instance.
        let store = crate::attachments::open_attachment_store();
        let (_stripped, content) = crate::attachments::extract_image_blocks(&text, &store);
        agent.send(
            starriver_kernel::llm::Message {
                id: starriver_kernel::llm::MessageId(id.clone()),
                role: starriver_kernel::llm::MessageRole::User,
                content,
                source: starriver_kernel::llm::MessageSource::User,
            },
            starriver_kernel::agent::InboxTarget::NextTurn,
            false,
        );
        Some(id)
    }

    pub fn queue_remove(&self, session_id: String, id: String) -> bool {
        let inner = self.inner.blocking_lock();
        let Some(inner) = inner.as_ref() else {
            return false;
        };
        let Some(agent) = inner
            .agent
            .registry()
            .get(&starriver_kernel::session::SessionId(session_id))
        else {
            return false;
        };
        agent.pending_remove(&id)
    }

    pub fn queue_update(&self, session_id: String, id: String, text: String) -> bool {
        let inner = self.inner.blocking_lock();
        let Some(inner) = inner.as_ref() else {
            return false;
        };
        let Some(agent) = inner
            .agent
            .registry()
            .get(&starriver_kernel::session::SessionId(session_id))
        else {
            return false;
        };
        // Same admission as queue_add: the edited text goes through image
        // extraction so editing a queued message never strips its images
        // (queue_list round-trips `![name](sha256:…)` refs into the editor).
        let store = crate::attachments::open_attachment_store();
        let (_stripped, content) = crate::attachments::extract_image_blocks(&text, &store);
        agent.pending_replace(&id, content)
    }

    pub fn queue_flush(&self, session_id: String) -> bool {
        let inner = self.inner.blocking_lock();
        let Some(inner) = inner.as_ref() else {
            return false;
        };
        let Some(agent) = inner
            .agent
            .registry()
            .get(&starriver_kernel::session::SessionId(session_id))
        else {
            return false;
        };
        agent.pending_flush();
        true
    }

    /// Send Now (queue dock 「立即」): abort the live turn, if any, and hand
    /// the durable queue to the driver in ONE host call. Cancel FIRST, flush
    /// SECOND: `cancel(keep_inbox=true)` parks the queue and aborts the
    /// running turn; `pending_flush` then unparks and wakes, so the parked
    /// flag ENDS false and the driver claims the queued message as the next
    /// turn with a fresh cancellation token. The old UI order (flush →
    /// cancel) re-parked the queue right after the flush — Send Now did
    /// nothing, and with a live turn the host round loop wedged forever
    /// waiting for a follow-up terminal that never came, leaking the session
    /// input slot. Returns false when the session has no live agent (the
    /// durable queue still runs with the session's next execute).
    pub fn queue_flush_now(&self, session_id: String) -> bool {
        let inner = self.inner.blocking_lock();
        let Some(inner) = inner.as_ref() else {
            return false;
        };
        let Some(agent) = inner
            .agent
            .registry()
            .get(&starriver_kernel::session::SessionId(session_id))
        else {
            return false;
        };
        agent.cancel(
            AgentCancelCause::with_reason(AgentCancelKind::User, "session.queueFlushNow"),
            true,
        );
        agent.pending_flush();
        true
    }

    /// WI-93/150: active background terminals (full snapshots — owner +
    /// workspace + command ride along for the workspace-scoped panel).
    pub fn terminals(&self) -> Vec<crate::terminal::TerminalInfo> {
        let inner = self.inner.blocking_lock();
        let Some(inner) = inner.as_ref() else {
            return Vec::new();
        };
        inner.agent.terminal_service.list()
    }

    /// WI-93: kill one background terminal by id. UI 入口走宿主权限
    /// （owner = None），不受会话归属限制。
    pub fn terminal_kill(&self, id: String) -> bool {
        let inner = self.inner.blocking_lock();
        let Some(inner) = inner.as_ref() else {
            return false;
        };
        inner.agent.terminal_service.kill(&id, None).is_ok()
    }

    /// Session ids currently holding the single input slot (an execute is
    /// in flight). BUG-96: this is the backend source of truth for UIs —
    /// a renderer reload loses its own run state while the backend task
    /// (and the slot) lives on, so "can I send?" must be answered from here.
    pub fn running_session_ids(&self) -> Vec<String> {
        self.running_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    pub fn cancel(&self, session_id: String) -> bool {
        let inner = self.inner.blocking_lock();
        let Some(inner) = inner.as_ref() else {
            return false;
        };
        let Some(agent) = inner
            .agent
            .registry()
            .get(&starriver_kernel::session::SessionId(session_id.clone()))
        else {
            // Observable so a cancel aimed at a dead/unknown session id
            // leaves a trace too (it used to be a silent no-op).
            tracing::info!(session = %session_id, source = "session.cancel", "cancel requested: no live agent");
            return false;
        };
        // Observable so an aborted turn can be attributed after the fact:
        // cause=user alone (turn/end) does not say WHO asked — UI Stop,
        // queue send-now and external clients all funnel here.
        tracing::info!(session = %session_id, source = "session.cancel", "cancel requested");
        // Forward whether a live turn was actually aborted: an idle agent
        // returns false so the UI can reset its "stopping" affordance instead
        // of waiting forever for a TaskCancelled that will never arrive.
        // keep_inbox = true: a manual stop parks the durable queue (the
        // driver must not auto-continue into it); the next waking send or
        // session.queueFlush resumes it FIFO.
        agent.cancel(
            AgentCancelCause::with_reason(AgentCancelKind::User, "session.cancel"),
            true,
        )
    }

    pub fn set_script_executor(&self, executor: Arc<dyn ScriptExecutor>) {
        *self.script_executor.write().unwrap() = Some(executor);
    }
}

fn dir_size_bytes(dir: &std::path::Path) -> u64 {
    let mut total = 0u64;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_file() {
            total += meta.len();
        } else if meta.is_dir() {
            total += dir_size_bytes(&entry.path());
        }
    }
    total
}

fn fs2_free_bytes(dir: &std::path::Path) -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        // macOS: statfs via libc.
        use std::ffi::CString;
        let c = CString::new(dir.to_string_lossy().as_bytes()).ok()?;
        let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::statfs(c.as_ptr(), &mut stat) };
        if rc != 0 {
            return None;
        }
        Some((stat.f_bavail as u64) * (stat.f_bsize as u64))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // Linux: statvfs.
        use std::ffi::CString;
        let c = CString::new(dir.to_string_lossy().as_bytes()).ok()?;
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::statvfs(c.as_ptr(), &mut st) };
        if rc != 0 {
            return None;
        }
        Some((st.f_bavail as u64) * (st.f_frsize as u64))
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        None
    }
}

/// Render the instruction chain as banner text (`[label]\ncontent` joined,
/// dropping trailing blocks once the same MAX_BYTES budget as the model side is
/// exceeded), so the banner and the model context see identical content.
fn render_workspace_instructions(
    files: &[starriver_agent_loop::instructions::InstructionFile],
) -> Option<String> {
    render_workspace_instructions_with(files, starriver_agent_loop::instructions::MAX_BYTES)
}

/// [`render_workspace_instructions`] under an explicit byte budget — the
/// settings seam (`prompt.instructions_max_bytes`) the meta path passes so
/// the banner and the model context agree on what fits (BUG-63).
fn render_workspace_instructions_with(
    files: &[starriver_agent_loop::instructions::InstructionFile],
    budget: usize,
) -> Option<String> {
    let mut out = String::new();
    for file in files {
        let block = format!("[{}]\n{}\n\n", file.label, file.content);
        if out.len() + block.len() > budget {
            break;
        }
        out.push_str(&block);
    }
    if out.is_empty() {
        None
    } else {
        Some(out.trim_end().to_string())
    }
}

#[cfg(test)]
mod instructions_banner_tests {
    use super::*;
    use starriver_agent_loop::instructions::InstructionFile;

    fn file(label: &str, content: &str) -> InstructionFile {
        InstructionFile {
            label: label.into(),
            content: content.into(),
        }
    }

    #[test]
    fn renders_layered_chain_with_labels() {
        let text = render_workspace_instructions(&[
            file("AGENTS.md (global)", "global rules"),
            file("AGENTS.md", "use pnpm"),
        ])
        .unwrap();
        assert!(text.contains("[AGENTS.md (global)]"));
        assert!(text.contains("global rules"));
        assert!(text.contains("[AGENTS.md]"));
        assert!(text.contains("use pnpm"));
    }

    #[test]
    fn empty_chain_renders_none() {
        assert!(render_workspace_instructions(&[]).is_none());
    }

    #[test]
    fn respects_model_byte_budget() {
        // The "[AGENTS.md]\n" label is 12 bytes: 65_520+12 = 65_532 ≤ MAX_BYTES;
        // one more trailing block (18 bytes) busts the budget → the tail is dropped.
        let big = "x".repeat(65_520);
        let text =
            render_workspace_instructions(&[file("AGENTS.md", &big), file("CLAUDE.md", "tail")])
                .unwrap();
        assert!(text.len() <= starriver_agent_loop::instructions::MAX_BYTES);
        assert!(!text.contains("tail"), "over-budget trailing block dropped");
    }
}

// ── BUG-63 regression (round 5): the banner honors the settings budget ──
#[cfg(test)]
mod round5_tests {
    use super::*;
    use starriver_agent_loop::instructions::InstructionFile;

    fn file(content: &str) -> InstructionFile {
        InstructionFile {
            label: "AGENTS.md".into(),
            content: content.into(),
        }
    }

    /// The meta doc promises "the banner and the model context can no longer
    /// disagree about what fits" — both must run under the SAME budget. The
    /// renderer hardcoded MAX_BYTES, so a settings budget below the default
    /// truncated the model context while the banner kept the full text.
    #[test]
    fn banner_render_respects_the_configured_budget() {
        let small = file("keep me");
        let big = file(&"x".repeat(8_000));
        let files = vec![small, big];
        let banner = render_workspace_instructions_with(&files, 4_000).expect("banner content");
        assert!(
            banner.contains("keep me"),
            "the fitting block stays (got {} bytes)",
            banner.len()
        );
        assert!(
            !banner.contains(&"x".repeat(100)),
            "the over-budget block is dropped under the configured budget"
        );
        assert!(banner.len() <= 4_000);
    }
}

/// WI-97/BUG-87: shared goal-continuation predicate — the runtime's
/// post-turn loop and [`crate::agents::RuntimeHost::goal_continuation`] both
/// ask "should the goal drive on". Active goals with budget headroom do;
/// Paused/BudgetLimited/Complete or absent goals do not.
pub(crate) fn goal_continuation_for(
    inner: &crate::RuntimeInner,
    session_id: &str,
) -> Option<String> {
    let goals = inner.goals.lock().unwrap();
    let g = goals.get(session_id)?;
    match g.status {
        crate::GoalStatus::Active => Some(format!(
            "Continue working toward the goal: {}. Assess current progress, decide the next concrete step, and do it. When the goal is fully achieved, state GOAL COMPLETE and stop.",
            g.objective
        )),
        _ => None,
    }
}
