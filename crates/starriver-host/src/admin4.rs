use std::sync::Arc;

use starriver_api_macro::api_method;

use crate::helpers::flatten_mode;
use crate::{types, HostRuntime};

/// System-prompt section text for a run's task-bound skills (BUG-6): the
/// selected skills' bodies, same shape as the always-on skills section.
/// Unknown names are skipped (warned) — a typo must not kill the whole run.
/// None when nothing resolves, so the resident section drops out entirely.
fn task_skills_section(
    manager: &starriver_skills::profile::skill_manager::SkillManager,
    names: &[&str],
) -> Option<String> {
    let mut text = String::from("## Task Skills\n\n");
    let mut any = false;
    for name in names {
        if let Some(skill) = manager.get(name) {
            any = true;
            text.push_str(&format!("### {}\n\n{}\n\n---\n\n", skill.name, skill.body));
        } else {
            tracing::warn!(skill = %name, "scheduled task references an unknown skill");
        }
    }
    any.then_some(text)
}

/// System-prompt section for scheduled (unattended) runs: no user is
/// present, so asking questions only stalls the run until the ask times
/// out. The model must decide autonomously and surface its assumptions in
/// the final answer instead.
pub(crate) const UNATTENDED_SECTION: &str = "## Unattended Run\n\n\
    This turn executes unattended as a scheduled task: no user is present and \
    questions cannot be answered. Do not call `ask_user` and do not wait for \
    any user input. When information is missing, pick the most reasonable \
    default, proceed, and clearly state every assumption you made in the \
    final answer. If the task is impossible without the missing information, \
    end the turn with a short explanation of what is needed instead of \
    asking.";

impl HostRuntime {
    pub fn list_mcp_servers(&self) -> Vec<types::McpServerInfo> {
        let config = self.mcp_config.read().unwrap();
        let tools_guard = self.mcp_tools.read().unwrap();
        config
            .mcp_servers
            .iter()
            .map(|(name, srv)| {
                let tool_count = tools_guard.get(name).map(|v| v.len() as u32).unwrap_or(0);
                let oauth = srv.oauth.as_ref().map(|o| types::McpOAuthInfo {
                    authorization_url: o.authorization_url.clone(),
                    token_url: o.token_url.clone(),
                    client_id: o.client_id.clone(),
                    scopes: o.scopes.clone(),
                    redirect_uri: o.redirect_uri.clone(),
                    has_secret: !o.client_secret.is_empty(),
                });
                types::McpServerInfo {
                    name: name.clone(),
                    transport: format!("{:?}", srv.transport).to_lowercase(),
                    command: srv.command.clone(),
                    args: srv.args.clone(),
                    env: srv.env.iter().map(|(k, v)| format!("{k}={v}")).collect(),
                    url: srv.url.clone(),
                    oauth_connected: crate::mcp::oauth::token(name).is_some(),
                    oauth,
                    tool_count,
                    // A server whose discovery failed (0 registered tools)
                    // must not claim "connected" forever.
                    status: if tool_count > 0 {
                        "connected".into()
                    } else {
                        "error".into()
                    },
                }
            })
            .collect()
    }

    // Legacy napi shell signature; argument count is fixed by the binding.
    #[allow(clippy::too_many_arguments)]
    pub fn add_mcp_server(
        &self,
        name: String,
        transport: String,
        command: String,
        args_json: String,
        env_json: String,
        url: Option<String>,
        oauth_json: Option<String>,
    ) -> anyhow::Result<()> {
        let transport_type = match transport.as_str() {
            "stdio" => crate::mcp::config::TransportType::Stdio,
            "sse" => crate::mcp::config::TransportType::Sse,
            "http" => crate::mcp::config::TransportType::Http,
            other => return Err(anyhow::anyhow!("unknown transport: {other}")),
        };
        // BUG-12: malformed JSON is an error, not a silent empty list — one
        // typo in args_json used to wipe the args/env entirely. Empty or
        // whitespace-only strings stay "nothing configured" (the legacy napi
        // shell passes "" for absent values).
        let parse_json = |raw: &str, field: &str| -> anyhow::Result<String> {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return Ok("null".into());
            }
            serde_json::from_str::<serde_json::Value>(trimmed)
                .map_err(|e| anyhow::anyhow!("invalid {field}: {e}"))?;
            Ok(trimmed.to_string())
        };
        let args: Vec<String> = serde_json::from_str(&parse_json(&args_json, "args_json")?)?;
        let env: std::collections::HashMap<String, String> =
            serde_json::from_str(&parse_json(&env_json, "env_json")?)?;
        let config_guard = self.mcp_config.read().unwrap();
        let existing = config_guard.get_server(&name);
        let oauth = crate::mcp::config::merge_oauth_secret(
            oauth_json
                .map(|s| parse_json(&s, "oauth_json"))
                .transpose()?
                .map(|s| {
                    serde_json::from_str(&s)
                        .map_err(|e| anyhow::anyhow!("invalid oauth_json (shape): {e}"))
                })
                .transpose()?,
            existing,
        );
        drop(config_guard);
        let srv = crate::mcp::config::McpServerConfig {
            transport: transport_type,
            command,
            args,
            env,
            url,
            oauth,
            deny_in_readonly_tools: Vec::new(),
            tool_timeout_secs: None,
            per_tool_timeout_secs: Default::default(),
        };
        {
            let mut config = self.mcp_config.write().unwrap();
            config.add_server(name.clone(), srv);
            if let Some(ref path) = crate::mcp::config::McpConfig::default_path() {
                config
                    .save(path)
                    .map_err(|e| anyhow::anyhow!("save: {e}"))?;
            }
        }
        if let Err(e) = self.discover_and_register_mcp(&name) {
            tracing::warn!(server = %name, error = %e, "MCP tool discovery failed for new server");
        }
        Ok(())
    }

    #[api_method("mcp.remove", permissions = ["mcp:write"])]
    pub fn remove_mcp_server(&self, name: String) -> anyhow::Result<()> {
        self.unregister_mcp_tools(&name);
        {
            let mut config = self.mcp_config.write().unwrap();
            config.remove_server(&name);
            if let Some(ref path) = crate::mcp::config::McpConfig::default_path() {
                config
                    .save(path)
                    .map_err(|e| anyhow::anyhow!("save: {e}"))?;
            }
        }
        Ok(())
    }

    pub fn refresh_mcp_tools(&self, name: Option<String>) -> anyhow::Result<()> {
        if let Some(ref n) = name {
            self.unregister_mcp_tools(n);
            if let Err(e) = self.discover_and_register_mcp(n) {
                return Err(anyhow::anyhow!("refresh {n}: {e}"));
            }
        } else {
            let server_names: Vec<String> = {
                self.mcp_config
                    .read()
                    .unwrap()
                    .mcp_servers
                    .keys()
                    .cloned()
                    .collect()
            };
            for n in server_names {
                self.unregister_mcp_tools(&n);
                if let Err(e) = self.discover_and_register_mcp(&n) {
                    tracing::warn!(server = %n, error = %e, "refresh failed");
                }
            }
        }
        Ok(())
    }

    pub fn test_mcp_server(&self, name: String) -> types::McpTestResult {
        let config_guard = self.mcp_config.read().unwrap();
        let Some(srv) = config_guard.get_server(&name).cloned() else {
            return types::McpTestResult {
                ok: false,
                tool_count: 0,
                tools: vec![],
                error: Some(format!("MCP server not found: {name}")),
            };
        };
        drop(config_guard);
        let inner_guard = self.inner.blocking_lock();
        let Some(inner) = inner_guard.as_ref() else {
            return types::McpTestResult {
                ok: false,
                tool_count: 0,
                tools: vec![],
                error: Some("runtime not initialized".into()),
            };
        };
        let name_owned = name.clone();
        let token_provider: crate::mcp::adapter::TokenProvider =
            Arc::new(move || crate::mcp::oauth::token(&name_owned));
        match inner
            .runtime
            .block_on(crate::mcp::definition::discover_bridge(
                &name,
                &srv,
                token_provider,
            )) {
            Ok(definitions) => {
                let names = definitions.iter().map(|d| d.name.clone()).collect();
                types::McpTestResult {
                    ok: true,
                    tool_count: definitions.len() as u32,
                    tools: names,
                    error: None,
                }
            }
            Err(e) => types::McpTestResult {
                ok: false,
                tool_count: 0,
                tools: vec![],
                error: Some(format!("{e}")),
            },
        }
    }

    pub fn test_mcp_tool(&self, name: String, tool_name: String) -> types::McpToolTestResult {
        let config_guard = self.mcp_config.read().unwrap();
        let Some(srv) = config_guard.get_server(&name).cloned() else {
            return types::McpToolTestResult {
                ok: false,
                output: None,
                error: Some(format!("MCP server not found: {name}")),
            };
        };
        drop(config_guard);
        let inner_guard = self.inner.blocking_lock();
        let Some(inner) = inner_guard.as_ref() else {
            return types::McpToolTestResult {
                ok: false,
                output: None,
                error: Some("runtime not initialized".into()),
            };
        };
        let name_owned = name.clone();
        let token_provider: crate::mcp::adapter::TokenProvider =
            Arc::new(move || crate::mcp::oauth::token(&name_owned));
        match inner.runtime.block_on(crate::mcp::adapter::test_tool_call(
            &name,
            &srv,
            &tool_name,
            token_provider,
        )) {
            Ok(result) => types::McpToolTestResult {
                ok: true,
                output: Some(
                    serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string()),
                ),
                error: None,
            },
            Err(e) => types::McpToolTestResult {
                ok: false,
                output: None,
                error: Some(format!("{e}")),
            },
        }
    }

    pub fn oauth_connect(&self, name: String) -> anyhow::Result<String> {
        let config_guard = self.mcp_config.read().unwrap();
        let srv = config_guard
            .get_server(&name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("MCP server not found: {name}"))?;
        drop(config_guard);
        let oauth = srv
            .oauth
            .clone()
            .ok_or_else(|| anyhow::anyhow!("server '{name}' has no OAuth config"))?;
        let handle = self.runtime_handle.clone();
        handle
            .block_on(crate::mcp::oauth::connect(name, oauth))
            .map_err(|e| anyhow::anyhow!("oauth connect: {e}"))
    }

    pub fn oauth_disconnect(&self, name: String) -> anyhow::Result<()> {
        crate::mcp::oauth::disconnect(&name);
        Ok(())
    }

    pub(crate) fn discover_and_register_mcp(&self, name: &str) -> anyhow::Result<()> {
        let config_guard = self.mcp_config.read().unwrap();
        let srv = config_guard
            .get_server(name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("MCP server not found: {name}"))?;
        drop(config_guard);
        let inner_guard = self.inner.blocking_lock();
        let inner = inner_guard
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("runtime not initialized"))?;
        let name_owned = name.to_string();
        let token_provider: crate::mcp::adapter::TokenProvider =
            Arc::new(move || crate::mcp::oauth::token(&name_owned));
        let definitions = inner
            .runtime
            .block_on(crate::mcp::definition::discover_bridge(
                name,
                &srv,
                token_provider,
            ))
            .map_err(|e| anyhow::anyhow!("discover: {e}"))?;
        let tool_names: Vec<String> = definitions.iter().map(|d| d.name.clone()).collect();
        inner
            .runtime
            .block_on(inner.agent.register_mcp_definitions(name, definitions))
            .map_err(|e| anyhow::anyhow!("kernel register: {e}"))?;
        self.mcp_tools
            .write()
            .unwrap()
            .insert(name.to_string(), tool_names);
        Ok(())
    }

    pub(crate) fn unregister_mcp_tools(&self, name: &str) {
        self.mcp_tools.write().unwrap().remove(name);
        let inner_guard = self.inner.blocking_lock();
        if let Some(inner) = inner_guard.as_ref() {
            inner
                .runtime
                .block_on(inner.agent.unregister_mcp_definitions(name));
        }
    }

    pub fn list_scheduled_tasks(&self) -> Vec<types::ScheduledTaskInfo> {
        let handle = self.runtime_handle.clone();
        let scheduler = self.scheduler.clone();
        handle.block_on(async move {
            scheduler
                .list()
                .await
                .into_iter()
                .map(|t| {
                    let (
                        mode_type,
                        period,
                        time,
                        day_of_week,
                        day_of_month,
                        month,
                        hours,
                        days_of_week,
                        datetime,
                    ) = flatten_mode(&t.mode);
                    types::ScheduledTaskInfo {
                        name: t.name,
                        prompt: t.prompt,
                        model: t.model,
                        agent: t.agent,
                        workspace: t.workspace,
                        enabled: t.enabled,
                        mode_type,
                        period,
                        time,
                        day_of_week,
                        day_of_month,
                        month,
                        hours,
                        days_of_week,
                        datetime,
                        start_date: t.start_date,
                        end_date: t.end_date,
                        skills: t.skills,
                        description: t.mode.describe(),
                        last_run: t.last_run,
                        next_run: t.next_run,
                        last_status: t.last_status,
                    }
                })
                .collect()
        })
    }

    // Legacy napi shell signature; argument count is fixed by the binding.
    #[allow(clippy::too_many_arguments)]
    pub fn add_scheduled_task(
        &self,
        name: String,
        mode_json: String,
        prompt: String,
        model: Option<String>,
        agent: Option<String>,
        workspace: Option<String>,
        enabled: bool,
        start_date: Option<String>,
        end_date: Option<String>,
        skills: Vec<String>,
    ) -> anyhow::Result<()> {
        let mode: crate::scheduler::ScheduleMode =
            serde_json::from_str(&mode_json).map_err(|e| anyhow::anyhow!("invalid mode: {e}"))?;
        // BUG-9: reject out-of-domain fields here — to_cron/compute_next_run
        // degrade to a silent None, so the entry point must be the reporter.
        mode.validate()
            .map_err(|e| anyhow::anyhow!("invalid mode: {e}"))?;
        let next_run =
            crate::scheduler::TaskScheduler::compute_next_run(&mode, start_date.as_deref());
        let task = crate::scheduler::ScheduledTask {
            name,
            mode,
            prompt,
            model: model.filter(|m| !m.is_empty()),
            agent: agent.filter(|a| !a.is_empty()),
            workspace: workspace.filter(|w| !w.is_empty()),
            enabled,
            start_date,
            end_date,
            skills,
            next_run,
            last_run: None,
            last_status: None,
            run_history: vec![],
        };
        let handle = self.runtime_handle.clone();
        let scheduler = self.scheduler.clone();
        handle
            .block_on(async move { scheduler.add(task).await })
            .map_err(|e| anyhow::anyhow!("add: {e}"))
    }

    #[api_method("scheduled.remove", permissions = ["scheduled:write"])]
    pub fn remove_scheduled_task(&self, name: String) -> anyhow::Result<()> {
        let handle = self.runtime_handle.clone();
        let scheduler = self.scheduler.clone();
        handle
            .block_on(async move { scheduler.remove(&name).await })
            .map_err(|e| anyhow::anyhow!("remove: {e}"))
    }

    // Legacy napi shell signature; argument count is fixed by the binding.
    #[allow(clippy::too_many_arguments)]
    pub fn update_scheduled_task(
        &self,
        name: String,
        mode_json: Option<String>,
        prompt: Option<String>,
        model: Option<String>,
        agent: Option<String>,
        workspace: Option<String>,
        enabled: Option<bool>,
        start_date: Option<String>,
        end_date: Option<String>,
        skills: Option<Vec<String>>,
    ) -> anyhow::Result<()> {
        let handle = self.runtime_handle.clone();
        let scheduler = self.scheduler.clone();
        handle.block_on(async move {
            let mut task = scheduler
                .get(&name)
                .await
                .ok_or_else(|| anyhow::anyhow!("task not found: {name}"))?;
            let mut mode_changed = false;
            if let Some(ref mj) = mode_json {
                let mode: crate::scheduler::ScheduleMode =
                    serde_json::from_str(mj).map_err(|e| anyhow::anyhow!("invalid mode: {e}"))?;
                // BUG-9: domain validation at the entry point.
                mode.validate()
                    .map_err(|e| anyhow::anyhow!("invalid mode: {e}"))?;
                task.mode = mode;
                mode_changed = true;
            }
            if let Some(p) = prompt {
                task.prompt = p;
            }
            if let Some(m) = model {
                task.model = if m.is_empty() { None } else { Some(m) };
            }
            if let Some(a) = agent {
                task.agent = if a.is_empty() { None } else { Some(a) };
            }
            if let Some(w) = workspace {
                task.workspace = if w.is_empty() { None } else { Some(w) };
            }
            if let Some(e) = enabled {
                task.enabled = e;
            }
            let start_date_changed = start_date.is_some();
            if start_date_changed {
                task.start_date = start_date;
            }
            if end_date.is_some() {
                task.end_date = end_date;
            }
            if let Some(s) = skills {
                task.skills = s;
            }
            // BUG-10: recompute against the task's EFFECTIVE start_date
            // whenever the mode or the anchor changed — Biweekly's next_run
            // is anchored to start_date, and a mode-only update used to
            // discard the stored anchor (incoming None ≠ "no anchor").
            if mode_changed || start_date_changed {
                task.next_run = crate::scheduler::TaskScheduler::compute_next_run(
                    &task.mode,
                    task.start_date.as_deref(),
                );
            }
            scheduler
                .update(&name, task)
                .await
                .map_err(|e| anyhow::anyhow!("update: {e}"))
        })
    }

    pub fn run_scheduled_task_now(&self, name: String) -> anyhow::Result<String> {
        let handle = self.runtime_handle.clone();
        let scheduler = self.scheduler.clone();
        let name_for_err = name.clone();
        let task = handle
            .block_on(async move { scheduler.get(&name).await })
            .ok_or_else(|| anyhow::anyhow!("task not found: {name_for_err}"))?;
        let task_name = task.name.clone();
        let prompt = task.prompt.clone();
        let model_key = task.model.clone();
        let skill_names: Vec<String> = task.skills.clone();
        if let Some(ref key) = model_key {
            self.activate_model_key(key)?;
        }
        // Scheduled runs execute under the task's own agent (legacy tasks
        // without the field run as the built-in Chat Agent).
        let agent_name = task
            .agent
            .filter(|a| !a.is_empty())
            .unwrap_or_else(|| crate::CHAT_AGENT_NAME.to_string());
        self.ensure_agent_residency(&agent_name)?;
        let agent_config = self.resolve_agent_config(&agent_name);
        // BUG-142/BUG-143: task-pinned key wins (stripped to the bare
        // variant id); without one, the agent's profile default rides —
        // parity with interactive runs, which pass config.model.
        let run_model = scheduled_run_model(model_key.as_deref(), agent_config.model.as_deref());
        // Scheduled runs don't compute a recall injection; make sure a stale
        // section from an earlier interactive coding run can't leak in.
        if let Some(kernel) = self.inner.blocking_lock().as_ref().map(|i| i.agent.clone()) {
            kernel.set_memory_injection(None);
        }
        // A task-bound workspace wins over the agent's own binding: the run
        // works in that directory and the transcript groups under it.
        let task_workspace = task.workspace.clone().filter(|w| !w.is_empty());
        let mut working_dir = agent_config.working_dir;
        if let Some(ref ws_id) = task_workspace {
            if let Some(w) = self.list_workspaces().into_iter().find(|w| &w.id == ws_id) {
                working_dir = Some(w.path);
            }
        }
        let spawn_workspace = task_workspace;
        let spawn_agent_name = agent_name.clone();
        let inner = self.inner.clone();
        let scheduler2 = self.scheduler.clone();
        let skill_runtime = self.skill_runtime.clone();
        handle.spawn(async move {
            let kernel = {
                let l = inner.lock().await;
                l.as_ref().map(|i| i.agent.clone())
            };
            let Some(kernel) = kernel else {
                let _ = scheduler2
                    .record_run(
                        &task_name,
                        "error".into(),
                        "engine unavailable".into(),
                        None,
                    )
                    .await;
                return;
            };
            // BUG-6: the task's skills finally have a carrier — their bodies
            // ride the `task-skills` system-prompt resident for this run
            // (empty list clears the section so nothing leaks across runs).
            let name_refs: Vec<&str> = skill_names.iter().map(|s| s.as_str()).collect();
            kernel.set_skill_injection(task_skills_section(
                skill_runtime.manager().as_ref(),
                &name_refs,
            ));
            // Unattended contract: scheduled runs must never stall on
            // ask_user — the section steers the model away from asking, and
            // the asker-side guard (install_question_tool) fails the tool
            // fast if it tries anyway. Cleared once the turn settles so
            // interactive runs never inherit it.
            kernel.set_unattended_injection(Some(UNATTENDED_SECTION.to_string()));
            let cancel = tokio_util::sync::CancellationToken::new();
            let sid = uuid::Uuid::new_v4().to_string();
            // Bind the transcript to the task's workspace (if any) and stamp
            // the runner agent — same write-once contract as interactive runs,
            // so the sidebar Tasks folder can reopen these with the right
            // agent later.
            {
                let inner = inner.lock().await;
                if let Some(ref runtime) = *inner {
                    if let Some(ref ws_id) = spawn_workspace {
                        if let Err(e) = runtime.host_store.attach_session(ws_id, &sid) {
                            tracing::warn!(session = %sid, error = %e, "attach scheduled run to workspace failed");
                        }
                    }
                    if let Err(e) = runtime.host_store.stamp_session_agent(&sid, &spawn_agent_name)
                    {
                        tracing::warn!(session = %sid, error = %e, "stamp scheduled session agent failed");
                    }
                    // WI-152: durable origin marker — the sidebar groups this
                    // session under Tasks → Scheduled runs even after its
                    // run_history entry ages out of jobs.taskRuns.
                    if let Err(e) = runtime.host_store.stamp_session_origin(&sid, "scheduled") {
                        tracing::warn!(session = %sid, error = %e, "stamp scheduled session origin failed");
                    }
                }
            }
            scheduler2.mark_running(&task_name, &sid);
            let (event_tx, mut event_rx) = tokio::sync::broadcast::channel::<serde_json::Value>(1024);
            // BUG-7: the working dir rides the execute call's `cwd` (stamped
            // into the session and used as the sandbox boundary) — the old
            // process-wide set_current_dir here raced every other session's
            // relative-path resolution and was never restored.
            let cwd = working_dir
                .as_ref()
                .map(|p| std::path::PathBuf::from(p).to_string_lossy().into_owned());
            let result = kernel
                .execute_with_meta_in(
                    prompt,
                    Some(sid.clone()),
                    run_model,
                    event_tx,
                    cancel.clone(),
                    // WI-180: the task name rides the fallback title event into
                    // the host projection — meta=None left scheduled-run
                    // sessions titleless forever (no LLM title upgrade runs
                    // for them either), so every sidebar Tasks row showed the
                    // bare "Chat" fallback.
                    Some(crate::plugin_kernel::HostSessionMeta {
                        title: Some(task_name.clone()),
                        reject_images: false,
                    }),
                    cwd,
                    Vec::new(),
                    None,
                    None,
                    None,
                )
                .await;
            let (status, summary) = match result {
                // execute_with_meta_in returns when the turn is QUEUED —
                // observe its terminal event before recording, or a failed
                // turn (bad provider key, tool crash) lands in the history
                // as success forever.
                Ok(_execution) => {
                    let mut status = "success".to_string();
                    let mut summary = String::new();
                    loop {
                        match event_rx.recv().await {
                            Ok(ev) => {
                                let ty = ev.get("type").and_then(|t| t.as_str()).unwrap_or("");
                                if ty == "turnError" {
                                    status = "error".into();
                                    summary = ev
                                        .get("error")
                                        .and_then(|e| e.as_str())
                                        .map(String::from)
                                        .unwrap_or_else(|| "turn failed".into());
                                    break;
                                }
                                if ty == "cancelled" {
                                    status = "cancelled".into();
                                    summary = "cancelled".into();
                                    break;
                                }
                                if ty == "turnEnd" {
                                    break;
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(_) => break,
                        }
                    }
                    (status, summary)
                }
                Err(e) => ("error".into(), e.to_string()),
            };
            kernel.set_unattended_injection(None);
            let _ = scheduler2
                .record_run(&task_name, status, summary, Some(sid))
                .await;
            scheduler2.mark_finished(&task_name);
        });
        Ok(format!("Task '{name_for_err}' queued for execution"))
    }

    /// One scheduler tick (BUG-5): heal recurring tasks whose `next_run` is
    /// missing/unparseable, then fire every due task through the same path
    /// as [`Self::run_scheduled_task_now`]. `next_run` is advanced BEFORE
    /// firing so a slow run can never double-trigger on the next tick.
    /// Returns the fired task names (tests assert on them; the background
    /// tick just logs). Deterministic — call directly in tests.
    pub fn fire_due_scheduled_tasks(&self) -> Vec<String> {
        let handle = self.runtime_handle.clone();
        let scheduler = self.scheduler.clone();

        // Heal pass: recurring tasks saved while next_run computation was
        // broken (e.g. before the 6-field cron fix) get a fresh value.
        let stale = handle.block_on(async { scheduler.stale_next_runs().await });
        for mut task in stale {
            let name = task.name.clone();
            let next = crate::scheduler::TaskScheduler::compute_next_run(
                &task.mode,
                task.start_date.as_deref(),
            );
            if next.is_some() {
                task.next_run = next;
                if let Err(e) = handle.block_on(async { scheduler.update(&name, task).await }) {
                    tracing::warn!(task = %name, error = %e, "heal next_run failed");
                }
            }
        }

        let due = handle.block_on(async { scheduler.due_tasks().await });
        let mut fired = Vec::new();
        for mut task in due {
            let name = task.name.clone();
            // Once tasks have their single run now; recurring tasks roll to
            // their next occurrence.
            task.next_run = match &task.mode {
                crate::scheduler::ScheduleMode::Once { .. } => None,
                mode => crate::scheduler::TaskScheduler::compute_next_run(
                    mode,
                    task.start_date.as_deref(),
                ),
            };
            if let Err(e) = handle.block_on(async { scheduler.update(&name, task).await }) {
                tracing::warn!(task = %name, error = %e, "advance next_run failed");
                continue;
            }
            match self.run_scheduled_task_now(name.clone()) {
                Ok(_) => fired.push(name),
                Err(e) => tracing::warn!(task = %name, error = %e, "scheduled fire failed"),
            }
        }
        fired
    }

    /// Start the background scheduler tick (BUG-5): every 60s, heal stale
    /// `next_run` values and fire due tasks. Long-lived hosts (desktop
    /// shell, protocol daemon) call this once after wrapping the runtime in
    /// an `Arc`; the loop exits when the runtime is dropped. Idempotent.
    pub fn start_scheduler_tick(self: &Arc<Self>) {
        {
            let mut started = self
                .scheduler_tick_started
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if *started {
                return;
            }
            *started = true;
        }
        let weak = Arc::downgrade(self);
        self.runtime_handle.spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // The first interval tick completes immediately; skip it so a
            // boot-time tick can't fire a just-loaded task twice in a row
            // with the manual "run now" the user may have just clicked.
            interval.tick().await;
            loop {
                interval.tick().await;
                let Some(runtime) = weak.upgrade() else {
                    break;
                };
                // BUG-84: fire_due_scheduled_tasks is synchronous and calls
                // Handle::block_on — on a worker thread of this same runtime
                // that panics. Dispatch to the blocking pool (same shape as
                // http_server's scheduled-task dispatch); a panic in one tick
                // only skips that tick.
                let runtime2 = Arc::clone(&runtime);
                let fired =
                    tokio::task::spawn_blocking(move || runtime2.fire_due_scheduled_tasks())
                        .await
                        .unwrap_or_default();
                if !fired.is_empty() {
                    tracing::info!(tasks = %fired.join(","), "scheduled tasks fired");
                }
            }
        });
    }

    pub(crate) fn activate_model_key(&self, key: &str) -> anyhow::Result<()> {
        let (provider_id, variant_id) = match key.split_once('/') {
            Some((p, v)) if !v.is_empty() => (p.to_string(), v.to_string()),
            _ => (key.to_string(), key.to_string()),
        };
        let entry = {
            let settings = self.settings.read().unwrap();
            settings
                .llm
                .providers
                .iter()
                .find(|p| p.id == provider_id)
                .cloned()
        };
        let Some(entry) = entry else {
            return Err(anyhow::anyhow!("model provider '{provider_id}' not found"));
        };
        let model_name = if entry.models.is_empty() {
            entry.default_model.clone()
        } else {
            entry
                .models
                .iter()
                .find(|v| v.id == variant_id)
                .map(|v| v.id.clone())
                .unwrap_or_else(|| entry.default_model.clone())
        };
        self.resolve_and_activate_model(&entry, &model_name)
    }

    pub fn get_task_run_history(&self, name: String) -> Vec<types::TaskRunRecord> {
        let handle = self.runtime_handle.clone();
        let scheduler = self.scheduler.clone();
        handle.block_on(async move {
            scheduler
                .get(&name)
                .await
                .map(|t| {
                    t.run_history
                        .into_iter()
                        .map(|r| types::TaskRunRecord {
                            timestamp: r.timestamp,
                            status: r.status,
                            summary: r.summary,
                            session_id: r.session_id,
                        })
                        .collect()
                })
                .unwrap_or_default()
        })
    }

    pub fn list_all_task_runs(&self) -> Vec<types::TaskRunRecordWithName> {
        let handle = self.runtime_handle.clone();
        let scheduler = self.scheduler.clone();
        handle.block_on(async move {
            scheduler
                .list_all_runs()
                .await
                .into_iter()
                .map(|(name, r)| types::TaskRunRecordWithName {
                    task_name: name,
                    timestamp: r.timestamp,
                    status: r.status,
                    summary: r.summary,
                    session_id: r.session_id,
                })
                .collect()
        })
    }

    pub fn get_session_trace(&self, session_id: String) -> types::SessionTrace {
        let inner = self.inner.clone();
        let handle = self.runtime_handle.clone();
        std::thread::spawn(move || {
            handle.block_on(async {
                let l = inner.lock().await;
                let Some(ref runtime) = *l else {
                    return types::SessionTrace { entries: vec![] };
                };
                // The trajectory reads the SAME durable event ledger the
                // messages/timeline views project from (the old legacy-store
                // read here always returned empty for plugin-kernel turns —
                // those never write rows into the legacy SessionStore).
                match crate::sessions::load_projection_parts(runtime, &session_id).await {
                    Ok((entries, tool_names)) => types::SessionTrace {
                        entries: crate::sessions::project_trace_entries(entries, &tool_names),
                    },
                    Err(_) => types::SessionTrace { entries: vec![] },
                }
            })
        })
        .join()
        .unwrap_or(types::SessionTrace { entries: vec![] })
    }
}

/// BUG-142: the bare variant id of a stored model key. Scheduled tasks store
/// the `provider/variant` composite; execute_with_meta_in sends its model to
/// the wire as the model NAME, so the composite must be split first
/// (activate_model_key has already pinned the provider by exact match).
fn bare_model_variant(key: &str) -> &str {
    key.split_once('/')
        .map(|(_, variant)| variant)
        .unwrap_or(key)
}

/// BUG-143: the model a scheduled run executes under — the task's pinned
/// key wins (stripped to the bare variant id, BUG-142); a task without its
/// own model rides the agent's profile default, matching interactive runs
/// (which pass `resolve_agent_config(agent).model`). A run that used to fall
/// through with `None` put the kernel's literal "mock" placeholder plus the
/// first registered provider on the wire — Zhipu answers 1211 模型不存在.
fn scheduled_run_model(task_key: Option<&str>, agent_default: Option<&str>) -> Option<String> {
    task_key
        .map(bare_model_variant)
        .map(str::to_owned)
        .or_else(|| agent_default.map(str::to_owned))
}

#[cfg(test)]
mod tests {
    #[test]
    fn bare_model_variant_splits_composite_key() {
        use crate::admin4::bare_model_variant;
        // BUG-142: the composite provider/variant key must not reach the wire.
        assert_eq!(bare_model_variant("zhipu/glm-5.3-flash"), "glm-5.3-flash");
        assert_eq!(
            bare_model_variant("deepseek/deepseek-v4-pro"),
            "deepseek-v4-pro"
        );
        // Bare variant ids pass through unchanged.
        assert_eq!(bare_model_variant("glm-5.3-flash"), "glm-5.3-flash");
    }

    #[test]
    fn scheduled_run_model_falls_back_to_agent_default() {
        use crate::admin4::scheduled_run_model;
        // Task-pinned composite key wins, stripped to the bare variant (BUG-142).
        assert_eq!(
            scheduled_run_model(Some("zhipu/glm-5.3"), Some("zhipu/glm-5.3-flash")).as_deref(),
            Some("glm-5.3")
        );
        // Null-model task (the template default) rides the agent profile
        // default instead of the kernel's "mock" placeholder (BUG-143).
        assert_eq!(
            scheduled_run_model(None, Some("glm-5.3-flash")).as_deref(),
            Some("glm-5.3-flash")
        );
        // Neither set → None (kernel falls back; tests keep the mock engine).
        assert_eq!(scheduled_run_model(None, None), None);
    }

    use crate::testutil::with_temp_home;
    use crate::HostRuntime;

    fn add_interval_task(host: &HostRuntime, name: &str, skills: Vec<String>) {
        host.add_scheduled_task(
            name.into(),
            r#"{"type":"interval","hours":1}"#.into(),
            "say nothing".into(),
            None,
            None,
            None,
            true,
            None,
            None,
            skills,
        )
        .expect("add task");
    }

    async fn task_next_run(host: &HostRuntime, name: &str) -> Option<String> {
        host.scheduler.get(name).await.unwrap().next_run
    }

    /// BUG-5: the tick fires exactly the due tasks, advances next_run BEFORE
    /// firing (no double trigger), heals stale next_run values, and leaves
    /// nothing due for the next pass.
    #[test]
    fn scheduler_tick_fires_due_tasks_and_advances_next_run() {
        with_temp_home(|_home| {
            let host = HostRuntime::new(None).expect("host boots");
            add_interval_task(&host, "due", vec![]);
            add_interval_task(&host, "future", vec![]);
            add_interval_task(&host, "stale", vec![]);

            let handle = host.runtime_handle.clone();
            handle.block_on(async {
                let sched = host.scheduler.clone();
                let mut t = sched.get("due").await.unwrap();
                t.next_run =
                    Some((chrono::Local::now() - chrono::Duration::minutes(5)).to_rfc3339());
                sched.update("due", t).await.unwrap();
                // "stale" keeps its add-computed next_run → strip it to model
                // a task saved while next_run computation was broken.
                let mut t = sched.get("stale").await.unwrap();
                t.next_run = None;
                sched.update("stale", t).await.unwrap();
            });

            let fired = host.fire_due_scheduled_tasks();
            assert_eq!(
                fired,
                vec!["due".to_string()],
                "only the overdue task fires"
            );

            // The fired task advanced to a future occurrence.
            let next = handle
                .block_on(async { task_next_run(&host, "due").await })
                .expect("recurring task keeps a next_run");
            let dt = chrono::DateTime::parse_from_rfc3339(&next).unwrap();
            assert!(
                dt.with_timezone(&chrono::Local) > chrono::Local::now(),
                "advanced next_run: {next}"
            );
            // The heal pass restored a next_run for the stale task.
            let healed = handle
                .block_on(async { task_next_run(&host, "stale").await })
                .expect("stale next_run healed");
            chrono::DateTime::parse_from_rfc3339(&healed).unwrap();
            // Nothing is due a second time.
            assert!(host.fire_due_scheduled_tasks().is_empty());
        });
    }

    /// BUG-6: the task's skills finally reach the model — their bodies ride
    /// the `task-skills` system-prompt resident, and a run without skills
    /// clears the section so nothing leaks between runs.
    #[test]
    fn scheduled_task_skills_reach_the_system_prompt() {
        with_temp_home(|home| {
            // SkillRuntime::new() scans $HOME/.starriver/skills at boot; a
            // skill is a subdirectory carrying SKILL.md.
            let skill_dir = home.join(".starriver").join("skills").join("greeter");
            std::fs::create_dir_all(&skill_dir).unwrap();
            std::fs::write(
                skill_dir.join("SKILL.md"),
                "---\nname: greeter\nversion: \"0.1.0\"\n---\n\nAlways say hi first.\n",
            )
            .unwrap();

            let host = HostRuntime::new(None).expect("host boots");
            let kernel = host
                .inner
                .blocking_lock()
                .as_ref()
                .map(|i| i.agent.clone())
                .expect("kernel");

            add_interval_task(&host, "with-skill", vec!["greeter".into()]);
            host.run_scheduled_task_now("with-skill".into())
                .expect("run");
            let mut seen = false;
            for _ in 0..100 {
                let injection = kernel.debug_skill_injection();
                if injection.contains("greeter") && injection.contains("Always say hi first.") {
                    seen = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            assert!(
                seen,
                "skill body must land in the task-skills resident: {:?}",
                kernel.debug_skill_injection()
            );

            // A run without skills clears the section (no cross-run leak).
            add_interval_task(&host, "plain", vec![]);
            host.run_scheduled_task_now("plain".into()).expect("run");
            let mut cleared = false;
            for _ in 0..100 {
                if kernel.debug_skill_injection().is_empty() {
                    cleared = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            assert!(cleared, "run without skills must clear the resident");
        });
    }

    /// BUG-7: a task-bound workspace rides the execute cwd (stamped into the
    /// session), never a process-wide set_current_dir that every concurrent
    /// session would inherit.
    #[test]
    fn scheduled_run_does_not_touch_process_cwd() {
        with_temp_home(|_home| {
            let host = HostRuntime::new(None).expect("host boots");
            let ws_dir = tempfile::tempdir().unwrap();
            let ws = host
                .create_workspace(ws_dir.path().to_string_lossy().into_owned())
                .expect("workspace");
            host.add_scheduled_task(
                "cwdjob".into(),
                r#"{"type":"interval","hours":1}"#.into(),
                "noop".into(),
                None,
                None,
                Some(ws.id.clone()),
                true,
                None,
                None,
                vec![],
            )
            .expect("add cwd task");

            let before = std::env::current_dir().unwrap();
            host.run_scheduled_task_now("cwdjob".into()).expect("run");
            // Wait for the spawn body to finish (run history lands last).
            let handle = host.runtime_handle.clone();
            let mut done = false;
            for _ in 0..200 {
                let history_len = handle.block_on(async {
                    host.scheduler
                        .get("cwdjob")
                        .await
                        .map(|t| t.run_history.len())
                        .unwrap_or(0)
                });
                if history_len > 0 {
                    done = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            assert!(done, "scheduled run never settled");
            assert_eq!(
                std::env::current_dir().unwrap(),
                before,
                "process cwd must stay untouched by scheduled runs"
            );
        });
    }

    /// WI-179 regression: the first run's session row must be listable the
    /// moment the run starts — the sidebar pokes `session.list` at 0/1/3/6s
    /// after the create, and the user-visible contract is that the row shows
    /// up while the turn is live, not only after it ends (attach_session
    /// writes the projcache row before execute; the fold only enriches it).
    #[test]
    fn scheduled_first_run_session_is_listable_during_the_run() {
        with_temp_home(|_home| {
            let host = HostRuntime::new(None).expect("host boots");
            let ws_dir = tempfile::tempdir().unwrap();
            let ws = host
                .create_workspace(ws_dir.path().to_string_lossy().into_owned())
                .expect("workspace");
            host.add_scheduled_task(
                "sidebark".into(),
                r#"{"type":"interval","hours":1}"#.into(),
                "noop".into(),
                None,
                None,
                Some(ws.id.clone()),
                true,
                None,
                None,
                vec![],
            )
            .expect("add task");

            host.run_scheduled_task_now("sidebark".into()).expect("run");

            // Poll exactly like the sidebar pokes do.
            let start = std::time::Instant::now();
            let mut listed: Vec<crate::types::SessionInfo> = Vec::new();
            while start.elapsed() < std::time::Duration::from_secs(3) {
                listed = host.list_sessions(Some(ws.id.clone()));
                if !listed.is_empty() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            let elapsed = start.elapsed();
            assert!(
                !listed.is_empty(),
                "first-run session must be listable under its workspace while the run is live (waited {elapsed:?})"
            );
            assert!(
                elapsed < std::time::Duration::from_millis(1500),
                "row took {elapsed:?} to appear — sidebar pokes at 0/1s would miss it"
            );
            // The scheduler registry points at the same session the sidebar lists.
            let handle = host.runtime_handle.clone();
            let sid = handle.block_on(async {
                loop {
                    if let Some(run) = host
                        .scheduler
                        .list_active_runs()
                        .iter()
                        .find(|r| r.task_name == "sidebark")
                    {
                        return Some(run.session_id.clone());
                    }
                    // The mock turn may settle before we poll — fall back to
                    // the recorded history's session id.
                    if let Some(t) = host.scheduler.get("sidebark").await {
                        if let Some(rec) =
                            t.run_history.iter().rev().find(|r| r.session_id.is_some())
                        {
                            return rec.session_id.clone();
                        }
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            });
            let sid = sid.expect("run registry carries a session id");
            assert!(
                listed.iter().any(|s| s.id == sid),
                "listed sessions must contain the run's session {sid}: {:?}",
                listed.iter().map(|s| s.id.clone()).collect::<Vec<_>>()
            );
        });
    }

    /// Scheduled runs run unattended: the `run:unattended` slot is clear on
    /// an idle host and is cleared once a run settles — interactive runs
    /// must never inherit the contract. (The section riding the assembled
    /// prompt is pinned by the integration probe
    /// `ask_user_in_scheduled_run_fails_fast_without_reaching_the_bridge`.)
    #[test]
    fn scheduled_run_clears_unattended_section_after_settling() {
        with_temp_home(|_home| {
            let host = HostRuntime::new(None).expect("host boots");
            let kernel = host
                .inner
                .blocking_lock()
                .as_ref()
                .map(|i| i.agent.clone())
                .expect("kernel");
            assert_eq!(
                kernel.debug_unattended_injection(),
                "",
                "idle host has no unattended section"
            );

            add_interval_task(&host, "unattended", vec![]);
            host.run_scheduled_task_now("unattended".into())
                .expect("run");

            // The run settles (history lands last) and must not leave the
            // section behind for the next interactive run.
            let handle = host.runtime_handle.clone();
            let mut settled = false;
            for _ in 0..200 {
                let history_len = handle.block_on(async {
                    host.scheduler
                        .get("unattended")
                        .await
                        .map(|t| t.run_history.len())
                        .unwrap_or(0)
                });
                if history_len > 0 {
                    settled = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            assert!(settled, "scheduled run never settled");
            assert_eq!(
                kernel.debug_unattended_injection(),
                "",
                "unattended section must clear after the run settles"
            );
        });
    }

    /// BUG-10: a start_date-only update recomputes next_run against the new
    /// anchor (Biweekly is anchored to start_date).
    #[test]
    fn update_start_date_alone_recomputes_next_run() {
        with_temp_home(|_home| {
            let host = HostRuntime::new(None).expect("host boots");
            host.add_scheduled_task(
                "biweekly".into(),
                r#"{"type":"periodic","period":"biweekly","time":"09:00"}"#.into(),
                "x".into(),
                None,
                None,
                None,
                true,
                Some("2020-01-01".into()),
                None,
                vec![],
            )
            .expect("add");

            let before = host
                .list_scheduled_tasks()
                .into_iter()
                .find(|t| t.name == "biweekly")
                .unwrap()
                .next_run
                .expect("initial next_run");

            let new_anchor = (chrono::Local::now() + chrono::Duration::days(30))
                .format("%Y-%m-%d")
                .to_string();
            host.update_scheduled_task(
                "biweekly".into(),
                None,
                None,
                None,
                None,
                None,
                None,
                Some(new_anchor),
                None,
                None,
            )
            .expect("update start_date only");

            let after = host
                .list_scheduled_tasks()
                .into_iter()
                .find(|t| t.name == "biweekly")
                .unwrap()
                .next_run
                .expect("recomputed next_run");
            assert_ne!(before, after, "next_run must track the new anchor");
            let dt = chrono::DateTime::parse_from_rfc3339(&after).unwrap();
            assert!(
                dt.with_timezone(&chrono::Local) > chrono::Local::now(),
                "anchored 30 days out: {after}"
            );
        });
    }

    /// BUG-9: out-of-domain fields are rejected at the API entry, with the
    /// offending field named (to_cron/compute_next_run degrade silently).
    #[test]
    fn add_scheduled_task_rejects_out_of_domain_fields() {
        with_temp_home(|_home| {
            let host = HostRuntime::new(None).expect("host boots");
            for (mode, field) in [
                (r#"{"type":"interval","hours":0}"#, "hours"),
                (r#"{"type":"interval","hours":25}"#, "hours"),
                (
                    r#"{"type":"periodic","period":"daily","time":"24:00"}"#,
                    "time",
                ),
                (
                    r#"{"type":"periodic","period":"weekly","time":"09:00","day_of_week":7}"#,
                    "day_of_week",
                ),
                (r#"{"type":"once","datetime":"someday"}"#, "datetime"),
            ] {
                let err = host.add_scheduled_task(
                    "bad".into(),
                    mode.into(),
                    "x".into(),
                    None,
                    None,
                    None,
                    true,
                    None,
                    None,
                    vec![],
                );
                let err = err.expect_err(mode);
                assert!(err.to_string().contains(field), "{mode}: {err}");
            }
        });
    }

    /// BUG-12: malformed MCP json fields error out instead of silently
    /// configuring an empty server.
    #[test]
    fn add_mcp_server_rejects_malformed_json() {
        with_temp_home(|_home| {
            let host = HostRuntime::new(None).expect("host boots");
            let err = host
                .add_mcp_server(
                    "badsrv".into(),
                    "stdio".into(),
                    "echo".into(),
                    "{not json".into(),
                    "{}".into(),
                    None,
                    None,
                )
                .expect_err("malformed args_json");
            assert!(err.to_string().contains("args_json"), "{err}");
            let err = host
                .add_mcp_server(
                    "badsrv".into(),
                    "stdio".into(),
                    "echo".into(),
                    "[]".into(),
                    "{not json".into(),
                    None,
                    None,
                )
                .expect_err("malformed env_json");
            assert!(err.to_string().contains("env_json"), "{err}");
        });
    }
}
