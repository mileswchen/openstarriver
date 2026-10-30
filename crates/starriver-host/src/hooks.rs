use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Per-script execution ceiling. Per docs/book ch12 §12.5: hook errors and
/// timeouts NEVER block the main pipeline — hooks are a user-side extension
/// point, not a security mechanism (that is the permission engine's job).
const HOOK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Hook trigger result
#[derive(Debug, Clone)]
pub struct HookResult {
    pub allowed: bool,
    pub output: Option<String>,
}

/// Context handed to hook scripts via environment variables (book ch12
/// §12.5 interface). Large payloads travel as temp FILES, never env vars —
/// env var size limits are a real wall.
#[derive(Debug, Clone, Default)]
pub struct HookContext<'a> {
    /// Canonical event name: `PreToolUse` or `PostToolUse`.
    pub event: &'a str,
    /// Tool name being gated / observed.
    pub tool: &'a str,
    /// Session id (`ToolExecution.agent_id` — in this codebase that IS the
    /// session id).
    pub session: Option<&'a str>,
    /// Workspace root.
    pub workspace: Option<&'a Path>,
    /// Full tool arguments as JSON (PreToolUse).
    pub params_json: Option<&'a str>,
    /// Tool outcome as JSON `{ok, value|error}` (PostToolUse only).
    pub result_json: Option<&'a str>,
}

/// Hook engine — PreToolUse / PostToolUse shell scripts under
/// `~/.starriver/hooks/<Event>/<tool>.sh` (`all.sh` = every tool).
///
/// Semantics (book ch12 §12.5):
/// - Pre: exit 2 = deny (stdout is the reason); exit 0 and anything else
///   (including spawn failure and timeout) = allow with a warn.
/// - Post: observation only — no exit code can alter the result.
pub struct HookEngine {
    hooks_dir: Option<PathBuf>,
}

impl HookEngine {
    pub fn defaults() -> Self {
        // Windows has no HOME by default (USERPROFILE is the convention) —
        // without the fallback hooks silently never load there.
        let home = std::env::var("HOME")
            .ok()
            .or_else(|| std::env::var("USERPROFILE").ok())
            .map(PathBuf::from);
        let dir = home.map(|h| h.join(".starriver").join("hooks"));
        Self { hooks_dir: dir }
    }

    pub fn new(hooks_dir: Option<PathBuf>) -> Self {
        Self { hooks_dir }
    }

    /// Legacy entry kept for compatibility: no context beyond event/tool/cwd.
    pub async fn trigger(&self, event: &str, tool_name: &str, working_dir: &PathBuf) -> HookResult {
        self.trigger_context(
            &HookContext {
                event,
                tool: tool_name,
                ..Default::default()
            },
            working_dir,
        )
        .await
    }

    /// Trigger hook event with the full script-facing context.
    pub async fn trigger_context(&self, ctx: &HookContext<'_>, working_dir: &Path) -> HookResult {
        let post_event = ctx.event == "PostToolUse";
        let Some(ref dir) = self.hooks_dir else {
            return HookResult {
                allowed: true,
                output: None,
            };
        };
        // Event directories match case-insensitively (`PreToolUse` ≡
        // `preToolUse`) — the convention is documented in the book with
        // CamelCase while skill frontmatter and older docs use camelCase.
        let event_dir = match event_dir(dir, ctx.event) {
            Some(dir) => dir,
            None => {
                return HookResult {
                    allowed: true,
                    output: None,
                }
            }
        };

        // Find matching hook scripts
        if let Ok(entries) = std::fs::read_dir(&event_dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                // Match on token boundaries: filename tokens contain the tool
                // name (edit matches edit.sh / pre-edit.sh, not editor_cfg.sh).
                let file_name = entry.file_name();
                let name = file_name.to_string_lossy();
                let stem = name.split('.').next().unwrap_or(&name);
                let matches_tool = stem
                    .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .any(|token| token == ctx.tool);
                if !matches_tool && name != "all.sh" && name != "all" {
                    continue;
                }
                // Windows CreateProcess cannot execute shebang scripts,
                // so route them through `sh` — the same shell the bash
                // tool already depends on being in PATH.
                let mut cmd = if cfg!(windows)
                    && std::fs::read(&path)
                        .map(|b| b.starts_with(b"#!"))
                        .unwrap_or(false)
                {
                    let mut c = tokio::process::Command::new("sh");
                    c.arg(&path);
                    c
                } else {
                    tokio::process::Command::new(&path)
                };
                cmd.current_dir(working_dir)
                    .env("STARRIVER_EVENT", ctx.event)
                    .env("STARRIVER_TOOL", ctx.tool);
                if let Some(session) = ctx.session {
                    cmd.env("STARRIVER_SESSION", session);
                }
                if let Some(workspace) = ctx.workspace {
                    cmd.env("STARRIVER_WORKSPACE", workspace);
                }
                // Params/result travel as temp files (env vars have hard size
                // limits); each file is removed as soon as the script exits.
                let mut params_path = None;
                let mut result_path = None;
                if let Some(params) = ctx.params_json {
                    params_path = write_hook_temp_file("params", params);
                    if let Some(ref p) = params_path {
                        cmd.env("STARRIVER_PARAMS_FILE", p);
                    }
                }
                if post_event {
                    if let Some(result) = ctx.result_json {
                        result_path = write_hook_temp_file("result", result);
                        if let Some(ref r) = result_path {
                            cmd.env("STARRIVER_RESULT_FILE", r);
                        }
                    }
                }
                let spawned = cmd
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .kill_on_drop(true)
                    .spawn();
                let output = match spawned {
                    Ok(child) => {
                        match tokio::time::timeout(HOOK_TIMEOUT, child.wait_with_output()).await {
                            Ok(Ok(output)) => Some(Ok(output)),
                            Ok(Err(e)) => Some(Err(e)),
                            Err(_) => {
                                // kill_on_drop reaps the child when the timeout
                                // drops the wait future — never block the pipeline.
                                tracing::warn!(
                                    hook = %path.display(),
                                    "hook timed out after 10s — continuing"
                                );
                                None
                            }
                        }
                    }
                    Err(e) => Some(Err(e)),
                };
                match output {
                    Some(Ok(output)) => {
                        if output.status.code() == Some(2) && !post_event {
                            let msg = String::from_utf8_lossy(&output.stdout);
                            cleanup_hook_temp_files(&params_path, &result_path);
                            return HookResult {
                                allowed: false,
                                output: Some(msg.to_string()),
                            };
                        }
                        if output.status.code() != Some(0) && output.status.code() != Some(2) {
                            tracing::warn!(
                                hook = %path.display(),
                                code = output.status.code(),
                                "hook exited non-zero — continuing (hooks never block the pipeline)"
                            );
                        }
                    }
                    Some(Err(e)) => {
                        tracing::warn!(hook = %path.display(), error = %e, "Hook execution failed");
                    }
                    // Timed out (already warned): allow.
                    None => {}
                }
                cleanup_hook_temp_files(&params_path, &result_path);
            }
        }
        HookResult {
            allowed: true,
            output: None,
        }
    }
}

fn event_dir(hooks_dir: &Path, event: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(hooks_dir).ok()?;
    for entry in entries.filter_map(|e| e.ok()) {
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir && entry.file_name().eq_ignore_ascii_case(event) {
            return Some(entry.path());
        }
    }
    None
}

// ── WI-115: script store (the GUI/protocol management surface) ──

/// One hook script as listed by [`list_hook_scripts`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct HookScriptInfo {
    /// Canonical event name (`PreToolUse` / `PostToolUse`).
    pub event: String,
    /// Script file name (e.g. `bash.sh`, `all.sh`).
    pub name: String,
    pub size: u64,
    /// File mtime in seconds since the epoch.
    pub modified_secs: u64,
}

/// Canonical event directory names (book ch12 §12.5 convention).
pub const HOOK_EVENTS: [&str; 2] = ["PreToolUse", "PostToolUse"];

/// The user hooks directory (`~/.starriver/hooks`, USERPROFILE fallback).
pub fn hooks_dir() -> Option<PathBuf> {
    let home = std::env::var("HOME")
        .ok()
        .or_else(|| std::env::var("USERPROFILE").ok())?;
    Some(PathBuf::from(home).join(".starriver").join("hooks"))
}

/// Whitelist for script file names: a single path component of
/// `[A-Za-z0-9._-]`, not hidden, not `.`/`..`. Prevents traversal — the
/// protocol surface writes user-supplied names to disk.
pub fn validate_hook_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("hook script name is empty".into());
    }
    if name.starts_with('.') {
        return Err("hook script name must not start with '.'".into());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return Err(format!(
            "hook script name `{name}` contains unsupported characters (allowed: letters, digits, . _ -)"
        ));
    }
    Ok(())
}

fn event_dir_canonical(event: &str) -> anyhow::Result<String> {
    HOOK_EVENTS
        .iter()
        .find(|e| e.eq_ignore_ascii_case(event))
        .map(|e| (*e).to_string())
        .ok_or_else(|| {
            anyhow::anyhow!("unknown hook event `{event}` (expected PreToolUse or PostToolUse)")
        })
}

/// Enumerate every script across the two event directories. Missing hooks
/// dir or event dirs yield an empty list (the convention: absent = empty).
pub fn list_hook_scripts() -> Vec<HookScriptInfo> {
    hooks_dir()
        .as_deref()
        .map(list_hook_scripts_in)
        .unwrap_or_default()
}

/// [`list_hook_scripts`] against an explicit directory (test seam).
pub fn list_hook_scripts_in(dir: &Path) -> Vec<HookScriptInfo> {
    let mut out = Vec::new();
    for event in HOOK_EVENTS {
        let Some(event_path) = event_dir(dir, event) else {
            continue;
        };
        let Ok(entries) = std::fs::read_dir(&event_path) else {
            continue;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let meta = match entry.metadata() {
                Ok(meta) => meta,
                Err(_) => continue,
            };
            let modified_secs = meta
                .modified()
                .ok()
                .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            out.push(HookScriptInfo {
                event: (*event).to_string(),
                name: entry.file_name().to_string_lossy().into_owned(),
                size: meta.len(),
                modified_secs,
            });
        }
    }
    out.sort_by(|a, b| a.event.cmp(&b.event).then(a.name.cmp(&b.name)));
    out
}

pub fn read_hook_script(event: &str, name: &str) -> anyhow::Result<String> {
    let dir = hooks_dir()
        .ok_or_else(|| anyhow::anyhow!("no home directory — hooks directory unknown"))?;
    read_hook_script_in(&dir, event, name)
}

/// [`read_hook_script`] against an explicit directory (test seam).
pub fn read_hook_script_in(dir: &Path, event: &str, name: &str) -> anyhow::Result<String> {
    let event = event_dir_canonical(event)?;
    validate_hook_name(name).map_err(|e| anyhow::anyhow!(e))?;
    Ok(std::fs::read_to_string(dir.join(event).join(name))?)
}

pub fn write_hook_script(event: &str, name: &str, content: &str) -> anyhow::Result<()> {
    let dir = hooks_dir()
        .ok_or_else(|| anyhow::anyhow!("no home directory — hooks directory unknown"))?;
    write_hook_script_in(&dir, event, name, content)
}

/// [`write_hook_script`] against an explicit directory (test seam).
pub fn write_hook_script_in(
    dir: &Path,
    event: &str,
    name: &str,
    content: &str,
) -> anyhow::Result<()> {
    let event = event_dir_canonical(event)?;
    validate_hook_name(name).map_err(|e| anyhow::anyhow!(e))?;
    let event_path = dir.join(event);
    std::fs::create_dir_all(&event_path)?;
    let path = event_path.join(name);
    std::fs::write(&path, content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

pub fn delete_hook_script(event: &str, name: &str) -> anyhow::Result<()> {
    let dir = hooks_dir()
        .ok_or_else(|| anyhow::anyhow!("no home directory — hooks directory unknown"))?;
    delete_hook_script_in(&dir, event, name)
}

/// [`delete_hook_script`] against an explicit directory (test seam).
pub fn delete_hook_script_in(dir: &Path, event: &str, name: &str) -> anyhow::Result<()> {
    let event = event_dir_canonical(event)?;
    validate_hook_name(name).map_err(|e| anyhow::anyhow!(e))?;
    std::fs::remove_file(dir.join(event).join(name))?;
    Ok(())
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn write_hook_temp_file(kind: &str, contents: &str) -> Option<PathBuf> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!(
        "starriver-hook-{}-{}-{}-{}.json",
        kind,
        std::process::id(),
        nanos,
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    match std::fs::write(&path, contents) {
        Ok(()) => Some(path),
        Err(e) => {
            tracing::warn!(error = %e, "failed to write hook temp file — script runs without it");
            None
        }
    }
}

fn cleanup_hook_temp_files(params: &Option<PathBuf>, result: &Option<PathBuf>) {
    for path in [params, result].into_iter().flatten() {
        if let Err(e) = std::fs::remove_file(path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(path = %path.display(), error = %e, "failed to clean hook temp file");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_dir(name: &str) -> std::path::PathBuf {
        static C: AtomicU64 = AtomicU64::new(0);
        let n = C.fetch_add(1, Ordering::Relaxed);
        let mut p = std::env::temp_dir();
        p.push(format!(
            "starriver-hooks-{}-{}-{}",
            name,
            std::process::id(),
            n
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn write_script(path: &std::path::Path, body: &str) {
        std::fs::write(path, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[tokio::test]
    async fn no_hooks_dir_allows() {
        let dir = temp_dir("none");
        let engine = HookEngine::new(None);
        let r = engine.trigger("preToolUse", "bash", &dir).await;
        assert!(r.allowed);
        assert!(r.output.is_none());
    }

    #[tokio::test]
    async fn missing_event_dir_allows() {
        let dir = temp_dir("miss");
        let engine = HookEngine::new(Some(dir.clone()));
        let r = engine.trigger("preToolUse", "bash", &dir).await;
        assert!(r.allowed);
    }

    #[tokio::test]
    async fn empty_event_dir_allows() {
        let dir = temp_dir("empty");
        std::fs::create_dir_all(dir.join("preToolUse")).unwrap();
        let engine = HookEngine::new(Some(dir.clone()));
        let r = engine.trigger("preToolUse", "bash", &dir).await;
        assert!(r.allowed);
        assert!(r.output.is_none());
    }

    #[tokio::test]
    async fn matching_script_exit_two_denies_with_output() {
        let dir = temp_dir("deny");
        let event = dir.join("preToolUse");
        std::fs::create_dir_all(&event).unwrap();
        write_script(
            &event.join("bash.sh"),
            "#!/bin/sh\necho 'blocked by hook'\nexit 2\n",
        );
        let engine = HookEngine::new(Some(dir.clone()));
        let r = engine.trigger("preToolUse", "bash", &dir).await;
        assert!(!r.allowed);
        assert_eq!(r.output.as_deref().map(str::trim), Some("blocked by hook"));
    }

    #[tokio::test]
    async fn matching_script_exit_zero_allows() {
        let dir = temp_dir("allow");
        let event = dir.join("preToolUse");
        std::fs::create_dir_all(&event).unwrap();
        write_script(&event.join("bash.sh"), "#!/bin/sh\necho 'ok'\nexit 0\n");
        let engine = HookEngine::new(Some(dir.clone()));
        let r = engine.trigger("preToolUse", "bash", &dir).await;
        assert!(r.allowed);
        assert!(r.output.is_none(), "exit 0 keeps output None");
    }

    #[tokio::test]
    async fn all_sh_matches_any_tool() {
        let dir = temp_dir("allsh");
        let event = dir.join("postToolResult");
        std::fs::create_dir_all(&event).unwrap();
        write_script(&event.join("all.sh"), "#!/bin/sh\necho 'reject'\nexit 2\n");
        let engine = HookEngine::new(Some(dir.clone()));
        let r = engine.trigger("postToolResult", "read", &dir).await;
        assert!(!r.allowed);
        assert_eq!(r.output.as_deref().map(str::trim), Some("reject"));
    }

    #[tokio::test]
    async fn non_matching_script_is_ignored() {
        let dir = temp_dir("nomatch");
        let event = dir.join("preToolUse");
        std::fs::create_dir_all(&event).unwrap();
        write_script(&event.join("git.sh"), "#!/bin/sh\necho 'nope'\nexit 2\n");
        let engine = HookEngine::new(Some(dir.clone()));
        // tool "bash" doesn't appear in "git.sh" and it's not all.sh → skipped
        let r = engine.trigger("preToolUse", "bash", &dir).await;
        assert!(r.allowed);
        assert!(r.output.is_none());
    }

    #[tokio::test]
    async fn word_boundary_matching_does_not_hit_substrings() {
        // "editor_config.sh" contains "edit" but is NOT an edit hook.
        let dir = temp_dir("boundary");
        let event = dir.join("preToolUse");
        std::fs::create_dir_all(&event).unwrap();
        write_script(
            &event.join("editor_config.sh"),
            "#!/bin/sh\necho hit\nexit 2\n",
        );
        let engine = HookEngine::new(Some(dir.clone()));
        let r = engine.trigger("preToolUse", "edit", &dir).await;
        assert!(r.allowed, "substring filename must not run the hook");

        // Token-exact filenames (dash-separated) DO run.
        let dir2 = temp_dir("boundary2");
        let event2 = dir2.join("preToolUse");
        std::fs::create_dir_all(&event2).unwrap();
        write_script(&event2.join("pre-edit.sh"), "#!/bin/sh\necho hit\nexit 2\n");
        let engine2 = HookEngine::new(Some(dir2.clone()));
        let r = engine2.trigger("preToolUse", "edit", &dir2).await;
        assert!(!r.allowed, "token-exact filename must run the hook");
    }

    #[tokio::test]
    async fn subdirectories_inside_event_dir_are_skipped() {
        let dir = temp_dir("subdir");
        let event = dir.join("preToolUse");
        std::fs::create_dir_all(event.join("nested")).unwrap();
        let engine = HookEngine::new(Some(dir.clone()));
        let r = engine.trigger("preToolUse", "bash", &dir).await;
        assert!(r.allowed, "non-file entries must be skipped");
    }

    // ── WI-115: script-facing context ──

    /// `event_dir` resolves case-insensitively: the book documents CamelCase
    /// directories while the pipeline emits camelCase event names.
    #[test]
    fn event_dir_matching_is_case_insensitive() {
        let dir = temp_dir("case");
        std::fs::create_dir_all(dir.join("PreToolUse")).unwrap();
        let found = event_dir(&dir, "preToolUse");
        assert!(found.is_some(), "camelCase event must find CamelCase dir");
        std::fs::create_dir_all(dir.join("posttooluse")).unwrap();
        assert!(event_dir(&dir, "PostToolUse").is_some());
        assert!(event_dir(&dir, "SessionStart").is_none());
    }

    #[tokio::test]
    async fn scripts_receive_env_context_and_param_file() {
        let dir = temp_dir("env");
        let workdir = temp_dir("env-work");
        let event = dir.join("preToolUse");
        std::fs::create_dir_all(&event).unwrap();
        // Dump the whole env contract into the deny reason so assertions run
        // on the returned output.
        write_script(
            &event.join("bash.sh"),
            r#"#!/bin/sh
echo "event=$STARRIVER_EVENT"
echo "tool=$STARRIVER_TOOL"
echo "session=$STARRIVER_SESSION"
echo "workspace=$STARRIVER_WORKSPACE"
echo "params=$(cat "$STARRIVER_PARAMS_FILE")"
echo "paramspath=$STARRIVER_PARAMS_FILE"
exit 2
"#,
        );
        let engine = HookEngine::new(Some(dir.clone()));
        let r = engine
            .trigger_context(
                &HookContext {
                    event: "PreToolUse",
                    tool: "bash",
                    session: Some("s1"),
                    workspace: Some(&workdir),
                    params_json: Some(r#"{"command":"ls -la"}"#),
                    result_json: None,
                },
                &dir,
            )
            .await;
        assert!(!r.allowed);
        let out = r.output.unwrap();
        assert!(out.contains("event=PreToolUse"), "{out}");
        assert!(out.contains("tool=bash"), "{out}");
        assert!(out.contains("session=s1"), "{out}");
        assert!(out.contains("params=") && out.contains("ls -la"), "{out}");
        assert!(
            out.contains(&format!("workspace={}", workdir.display())),
            "{out}"
        );
        // The params temp file is cleaned up once the script exits.
        let params_path = out
            .lines()
            .find_map(|l| l.strip_prefix("paramspath="))
            .unwrap_or_default();
        assert!(!params_path.is_empty(), "script saw the params path: {out}");
        assert!(
            !std::path::Path::new(params_path).exists(),
            "params file must be removed after the script exits"
        );
    }

    #[tokio::test]
    async fn post_event_is_observation_only_and_gets_result_file() {
        let dir = temp_dir("post");
        let event = dir.join("PostToolUse");
        std::fs::create_dir_all(&event).unwrap();
        // Post hooks are observation-only: no exit code alters the result.
        // Side effects go to a scratch file (deny output is not surfaced).
        let scratch = dir.join("post-scratch.txt");
        let script = event.join("all.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho \"event=$STARRIVER_EVENT result=$(cat \"$STARRIVER_RESULT_FILE\")\" >> {}\nexit 2\n",
                scratch.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let engine = HookEngine::new(Some(dir.clone()));
        let r = engine
            .trigger_context(
                &HookContext {
                    event: "PostToolUse",
                    tool: "read",
                    result_json: Some(r#"{"ok":true,"value":"data"}"#),
                    ..Default::default()
                },
                &dir,
            )
            .await;
        assert!(
            r.allowed,
            "post hooks are observation-only — exit 2 must not deny"
        );
        let out = std::fs::read_to_string(&scratch).unwrap_or_default();
        assert!(out.contains("event=PostToolUse"), "{out}");
        assert!(out.contains("value") && out.contains("data"), "{out}");
    }

    #[tokio::test]
    async fn hanging_hook_is_killed_at_timeout_and_pipeline_continues() {
        let dir = temp_dir("timeout");
        let event = dir.join("preToolUse");
        std::fs::create_dir_all(&event).unwrap();
        write_script(&event.join("all.sh"), "#!/bin/sh\nsleep 30\n");
        let engine = HookEngine::new(Some(dir.clone()));
        let started = std::time::Instant::now();
        let r = engine.trigger("preToolUse", "bash", &dir).await;
        assert!(r.allowed, "timed-out hook must not block the pipeline");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(20),
            "timeout must fire near 10s, took {:?}",
            started.elapsed()
        );
    }

    // ── WI-115: script store ──

    #[test]
    fn validate_hook_name_rejects_traversal_and_hidden() {
        assert!(validate_hook_name("bash.sh").is_ok());
        assert!(validate_hook_name("all.sh").is_ok());
        assert!(validate_hook_name("pre-edit.sh").is_ok());
        assert!(validate_hook_name("../evil").is_err(), "traversal");
        assert!(validate_hook_name("a/b.sh").is_err(), "separator");
        assert!(validate_hook_name("a\\b.sh").is_err(), "separator");
        assert!(validate_hook_name(".hidden").is_err(), "hidden");
        assert!(validate_hook_name("").is_err(), "empty");
        assert!(validate_hook_name("sp ace.sh").is_err(), "space");
    }

    #[test]
    fn store_roundtrip_lists_and_deletes() {
        let dir = temp_dir("store");
        write_hook_script_in(&dir, "preToolUse", "bash.sh", "#!/bin/sh\nexit 0\n").unwrap();
        // camelCase event names normalize onto the canonical dirs.
        write_hook_script_in(&dir, "posttooluse", "audit.sh", "#!/bin/sh\nexit 0\n").unwrap();
        // Engine finds scripts under canonical dirs written via the store.
        assert!(event_dir(&dir, "PostToolUse").is_some());

        let listed = list_hook_scripts_in(&dir);
        assert_eq!(listed.len(), 2, "{listed:?}");
        // Alphabetical by (event, name): PostToolUse sorts before PreToolUse.
        assert_eq!(listed[0].event, "PostToolUse");
        assert_eq!(listed[0].name, "audit.sh");
        assert_eq!(listed[1].event, "PreToolUse");
        assert_eq!(listed[1].name, "bash.sh");

        let content = read_hook_script_in(&dir, "PreToolUse", "bash.sh").unwrap();
        assert!(content.starts_with("#!/bin/sh"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("PreToolUse").join("bash.sh"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111, "written scripts are executable");
        }

        delete_hook_script_in(&dir, "PreToolUse", "bash.sh").unwrap();
        assert_eq!(list_hook_scripts_in(&dir).len(), 1);
        assert!(read_hook_script_in(&dir, "PreToolUse", "bash.sh").is_err());
    }

    #[test]
    fn store_rejects_unknown_events_and_bad_names() {
        let dir = temp_dir("storebad");
        assert!(write_hook_script_in(&dir, "SessionStart", "x.sh", "").is_err());
        assert!(write_hook_script_in(&dir, "PreToolUse", "../x.sh", "").is_err());
        assert!(read_hook_script_in(&dir, "PreToolUse", "../../etc/passwd").is_err());
        // Canonical dir must NOT have been created for rejected writes.
        assert!(!dir.join("SessionStart").exists());
    }
}
