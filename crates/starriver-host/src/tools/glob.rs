//! Glob tool: filesystem discovery with shell-glob patterns behind a
//! pluggable traversal backend, with mtime filtering and result spill.
//!
//! Backends ([`GlobBackend`]):
//! - `walkdir` (default): sequential std-fs walk — the batch-2 behavior,
//!   kept unchanged as the default.
//! - `ripgrep`: the ripgrep kernel crates (`ignore::WalkBuilder`) —
//!   gitignore-aware, skips hidden files. Selected via
//!   `STARRIVER_GLOB_BACKEND=ripgrep` (deployment-level choice; no new
//!   dependencies, `ignore` is already in the tree for the grep tool).
//!
//! mtime filtering (`newer_than`/`older_than`, relative seconds or ISO
//! timestamp): entries failing the filter are skipped and directories are
//! not descended into. Best-effort by design: a directory's mtime only
//! reflects direct child add/remove, so an in-place edit deep inside does
//! not bump ancestor directory mtimes (documented tradeoff — file-level
//! filtering is authoritative for every file actually visited).
//!
//! Spill: when matches exceed `MAX_RESULTS` the full list (bounded by
//! `COLLECT_CAP`) is persisted to `~/.starriver/spill/glob-<id>.json` and
//! the in-context output becomes the first page plus a locator; the same
//! tool retrieves pages with `spill`/`offset`/`limit`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use cordis_rslite::Value;
use starriver_kernel::tools::{ToolDefinition, ToolError, ToolExecutor, ToolRunContext};

use super::common::{invalid_input, json_args, resolve_path, text_content, tool_failure};

/// Directories never descended into — binary/vendor noise.
const SKIP_DIRS: &[&str] = &[".git", "node_modules", "target"];

/// Cap on matched results returned in-context per call (page size).
const MAX_RESULTS: usize = 200;

/// Hard cap on collected matches for one search (bounds spill size and
/// traversal cost on huge trees). Reaching it marks the result truncated.
const COLLECT_CAP: usize = 20_000;

/// Upper bound on a spill-retrieval page.
const MAX_PAGE: usize = 1000;

/// Translate a shell glob (`**`, `*`, `?`, `[abc]`) into a regex matching the
/// slash-joined relative path. `*` never crosses `/`; `**/` matches zero or
/// more directory segments (ripgrep semantics).
fn glob_to_regex(pattern: &str) -> Result<regex_lite::Regex, String> {
    let mut re = String::from("^");
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' => {
                if chars.get(i + 1) == Some(&'*') {
                    if chars.get(i + 2) == Some(&'/') {
                        // `**/` — zero or more directory segments.
                        re.push_str("(?:.*/)?");
                        i += 2;
                    } else {
                        // Bare trailing `**` — everything below the root.
                        re.push_str(".*");
                        i += 1;
                    }
                } else {
                    re.push_str("[^/]*");
                }
            }
            '?' => re.push_str("[^/]"),
            '[' => {
                let mut cls = String::new();
                let mut j = i + 1;
                if chars.get(j) == Some(&'!') {
                    cls.push('^');
                    j += 1;
                }
                let mut closed = false;
                while j < chars.len() {
                    let c = chars[j];
                    if c == ']' {
                        closed = true;
                        break;
                    }
                    cls.push(c);
                    j += 1;
                }
                if !closed {
                    return Err(format!("unclosed character class in glob: {pattern}"));
                }
                re.push('[');
                re.push_str(&cls);
                re.push(']');
                i = j;
            }
            c if "+^$(){}|\\.".contains(c) => {
                re.push('\\');
                re.push(c);
            }
            _ => re.push(chars[i]),
        }
        i += 1;
    }
    re.push('$');
    regex_lite::Regex::new(&re).map_err(|e| format!("invalid glob {pattern}: {e}"))
}

/// Inclusive time window derived from `newer_than`/`older_than`. `newer_than`
/// keeps entries with `mtime > newer_than`; `older_than` keeps `mtime <
/// older_than` (open bounds on both sides).
#[derive(Debug, Clone, Copy)]
struct MtimeFilter {
    newer_than: Option<SystemTime>,
    older_than: Option<SystemTime>,
}

impl MtimeFilter {
    fn passes(&self, mtime: SystemTime) -> bool {
        if let Some(bound) = self.newer_than {
            if mtime <= bound {
                return false;
            }
        }
        if let Some(bound) = self.older_than {
            if mtime >= bound {
                return false;
            }
        }
        true
    }
}

/// Parse one time bound: a non-negative integer (seconds before now) or an
/// ISO timestamp (RFC3339, `YYYY-MM-DD HH:MM:SS`, or `YYYY-MM-DD`, naive
/// forms interpreted as UTC).
fn parse_time_bound(name: &str, v: &serde_json::Value) -> Result<Option<SystemTime>, String> {
    match v {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::Number(n) => {
            let secs = n.as_u64().ok_or_else(|| {
                format!("{name} must be a non-negative number of seconds or an ISO timestamp")
            })?;
            Ok(SystemTime::now()
                .checked_sub(Duration::from_secs(secs))
                .or(Some(SystemTime::UNIX_EPOCH)))
        }
        serde_json::Value::String(s) => {
            if s.trim().is_empty() {
                return Err(format!("{name} must not be empty"));
            }
            let parsed = chrono::DateTime::parse_from_rfc3339(s)
                .ok()
                .map(|dt| dt.with_timezone(&chrono::Utc))
                .or_else(|| {
                    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
                        .ok()
                        .and_then(|d| d.and_hms_opt(0, 0, 0))
                        .map(|t| t.and_utc())
                })
                .or_else(|| {
                    chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
                        .ok()
                        .map(|t| t.and_utc())
                });
            let dt = parsed.ok_or_else(|| {
                format!(
                    "{name}: cannot parse {s:?} as seconds or ISO time (RFC3339, YYYY-MM-DD[ HH:MM:SS])"
                )
            })?;
            // Saturate pre-epoch timestamps at the epoch.
            let secs = dt.timestamp().max(0) as u64;
            Ok(Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs)))
        }
        _ => Err(format!(
            "{name} must be a number of seconds or an ISO timestamp string"
        )),
    }
}

/// One resolved glob search.
struct GlobRequest {
    base: PathBuf,
    pattern: String,
    regex: regex_lite::Regex,
    mtime: Option<MtimeFilter>,
}

/// Collected matches (relative, `/`-joined paths) plus whether collection
/// stopped at [`COLLECT_CAP`].
struct GlobResult {
    matches: Vec<String>,
    truncated: bool,
}

/// Pluggable traversal backend. Selection is deployment-level
/// (`STARRIVER_GLOB_BACKEND`); the tool surface is identical either way.
trait GlobBackend: Send + Sync {
    fn find(&self, req: &GlobRequest) -> Result<GlobResult, String>;
}

/// The default backend: sequential std-fs walk (batch-2 behavior).
struct WalkdirBackend;

fn walk(
    dir: &Path,
    rel_prefix: &str,
    req: &GlobRequest,
    out: &mut Vec<String>,
    limit: usize,
) -> bool {
    // Returns false when collection is full — propagate to stop the walk.
    let Ok(entries) = std::fs::read_dir(dir) else {
        return true;
    };
    for entry in entries.flatten() {
        if out.len() >= limit {
            return false;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if SKIP_DIRS.contains(&name.as_str()) {
            continue;
        }
        let rel = if rel_prefix.is_empty() {
            name.clone()
        } else {
            format!("{rel_prefix}/{name}")
        };
        let path = entry.path();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if let Some(filter) = &req.mtime {
            let Ok(mtime) = meta.modified() else {
                continue;
            };
            if !filter.passes(mtime) {
                // Prune: directories failing the filter are not descended
                // into (best-effort; see module docs).
                continue;
            }
        }
        if req.regex.is_match(&rel) {
            out.push(rel.clone());
        }
        if meta.is_dir() && !walk(&path, &rel, req, out, limit) {
            return false;
        }
    }
    true
}

impl GlobBackend for WalkdirBackend {
    fn find(&self, req: &GlobRequest) -> Result<GlobResult, String> {
        let mut matches = Vec::new();
        let complete = walk(&req.base, "", req, &mut matches, COLLECT_CAP);
        Ok(GlobResult {
            matches,
            truncated: !complete,
        })
    }
}

/// The ripgrep-kernel backend: `ignore::WalkBuilder` traversal — respects
/// `.gitignore`/`.ignore` (even without a `.git` directory) and skips hidden
/// files, mirroring the grep tool's walk semantics.
struct RipgrepBackend;

impl GlobBackend for RipgrepBackend {
    fn find(&self, req: &GlobRequest) -> Result<GlobResult, String> {
        let mtime = req.mtime;
        let mut builder = ignore::WalkBuilder::new(&req.base);
        builder.require_git(false);
        if mtime.is_some() {
            // Prune directories failing the mtime filter (best-effort; see
            // module docs) on top of the noise-dir filter.
            builder.filter_entry(move |entry| {
                if entry.depth() == 0 {
                    return true;
                }
                let name = entry.file_name().to_str().unwrap_or_default();
                if SKIP_DIRS.contains(&name) {
                    return false;
                }
                if entry.file_type().is_some_and(|t| t.is_dir()) {
                    let passes = entry
                        .metadata()
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .is_some_and(|mtime_val| mtime.unwrap().passes(mtime_val));
                    return passes;
                }
                true
            });
        } else {
            builder.filter_entry(|entry| {
                entry.depth() == 0
                    || !SKIP_DIRS.contains(&entry.file_name().to_str().unwrap_or_default())
            });
        }
        let mut matches = Vec::new();
        let mut truncated = false;
        for entry in builder.build() {
            let Ok(entry) = entry else {
                continue;
            };
            if matches.len() >= COLLECT_CAP {
                truncated = true;
                break;
            }
            if let Some(filter) = &req.mtime {
                let Some(mtime_val) = entry.metadata().ok().and_then(|m| m.modified().ok()) else {
                    continue;
                };
                if !filter.passes(mtime_val) {
                    continue;
                }
            }
            let Ok(rel) = entry.path().strip_prefix(&req.base) else {
                continue;
            };
            let rel = rel
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/");
            if req.regex.is_match(&rel) {
                matches.push(rel);
            }
        }
        Ok(GlobResult { matches, truncated })
    }
}

/// Choose the traversal backend from `STARRIVER_GLOB_BACKEND`
/// (`walkdir` default | `ripgrep`); unknown values fall back with a trace.
fn select_backend() -> Box<dyn GlobBackend> {
    match std::env::var("STARRIVER_GLOB_BACKEND").as_deref() {
        Ok("ripgrep") => Box::new(RipgrepBackend),
        other => {
            if let Some(v) = other.ok().filter(|v| !v.is_empty() && *v != "walkdir") {
                tracing::debug!(value = %v, "STARRIVER_GLOB_BACKEND unknown; using walkdir");
            }
            Box::new(WalkdirBackend)
        }
    }
}

// ── spill: persist oversized results and retrieve pages ─────────────────────

static SPILL_SEQ: AtomicU64 = AtomicU64::new(0);

fn spill_dir() -> PathBuf {
    let home = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    home.join(".starriver").join("spill")
}

/// Write the full match list as JSON under a fresh id.
fn write_spill(req: &GlobRequest, matches: &[String]) -> Option<String> {
    let id = format!(
        "glob-{}-{}",
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
        SPILL_SEQ.fetch_add(1, Ordering::Relaxed)
    );
    let dir = spill_dir();
    std::fs::create_dir_all(&dir).ok()?;
    let body = serde_json::json!({
        "tool": "glob",
        "pattern": req.pattern,
        "base": req.base.display().to_string(),
        "total": matches.len(),
        "matches": matches,
    });
    std::fs::write(dir.join(format!("{id}.json")), body.to_string()).ok()?;
    Some(id)
}

/// Validate a caller-supplied spill id (no path separators / traversal).
fn sanitize_spill_id(id: &str) -> Result<(), ToolError> {
    let ok = !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        Err(invalid_input(
            "glob",
            format!("invalid spill id: {id:?} (expected an id from a previous glob spill note)"),
        ))
    }
}

/// Read one page from a spilled result. Returns (header, page, total).
fn read_spill(
    id: &str,
    offset: usize,
    limit: usize,
) -> Result<(String, Vec<String>, usize), ToolError> {
    let path = spill_dir().join(format!("{id}.json"));
    let text = std::fs::read_to_string(&path).map_err(|_| {
        tool_failure(
            "glob",
            "NOT_FOUND",
            format!("spill file not found: {}", path.display()),
        )
    })?;
    let body: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| invalid_input("glob", format!("spill file is corrupt: {e}")))?;
    let total = body
        .get("total")
        .and_then(|t| t.as_u64())
        .ok_or_else(|| invalid_input("glob", "spill file is corrupt: missing total".into()))?
        as usize;
    let empty = Vec::new();
    let matches = body
        .get("matches")
        .and_then(|m| m.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or(empty);
    if offset >= matches.len() {
        return Ok((
            format!("no results at offset {offset} (total {total})"),
            Vec::new(),
            total,
        ));
    }
    let end = (offset + limit).min(matches.len());
    let page = matches[offset..end].to_vec();
    let header = format!(
        "[glob spill {id}: showing {}-{} of {total}]",
        offset + 1,
        end,
    );
    Ok((header, page, total))
}

struct Glob;

impl ToolExecutor for Glob {
    fn execute(
        &self,
        arguments: Value,
        ctx: ToolRunContext,
    ) -> cordis_rslite::BoxFuture<Result<Value, ToolError>> {
        Box::pin(async move {
            let json = json_args(&arguments)?;

            // Spill-retrieval mode: page a previous oversized result.
            if let Some(spill_id) = json.get("spill").and_then(|s| s.as_str()) {
                sanitize_spill_id(spill_id)?;
                let offset = json.get("offset").and_then(|o| o.as_u64()).unwrap_or(0) as usize;
                let limit = (json
                    .get("limit")
                    .and_then(|l| l.as_u64())
                    .unwrap_or(MAX_RESULTS as u64) as usize)
                    .clamp(1, MAX_PAGE);
                let (header, page, total) = read_spill(spill_id, offset, limit)?;
                let mut output = header;
                if !page.is_empty() {
                    output.push('\n');
                    output.push_str(&page.join("\n"));
                }
                if total > offset + page.len() {
                    output.push_str(&format!(
                        "\n[glob spill {spill_id}: more results — offset {}, limit {}]",
                        offset + page.len(),
                        limit
                    ));
                }
                return Ok(Value::new(serde_json::json!({ "output": output })));
            }

            let pattern = json
                .get("pattern")
                .and_then(|p| p.as_str())
                .ok_or_else(|| invalid_input("glob", "missing pattern".into()))?;
            if pattern.trim().is_empty() {
                return Err(invalid_input("glob", "pattern must be non-empty".into()));
            }
            let re = glob_to_regex(pattern).map_err(|e| invalid_input("glob", e))?;
            let base = json
                .get("path")
                .and_then(|p| p.as_str())
                .map(|p| resolve_path(ctx.execution.cwd.as_deref(), p))
                .unwrap_or_else(|| ctx.execution.cwd.clone().unwrap_or_default());
            if !base.is_dir() {
                return Err(tool_failure(
                    "glob",
                    "NOT_FOUND",
                    format!("directory not found: {}", base.display()),
                ));
            }
            let mtime =
                MtimeFilter {
                    newer_than: match json.get("newer_than") {
                        Some(v) => parse_time_bound("newer_than", v)
                            .map_err(|e| invalid_input("glob", e))?,
                        None => None,
                    },
                    older_than: match json.get("older_than") {
                        Some(v) => parse_time_bound("older_than", v)
                            .map_err(|e| invalid_input("glob", e))?,
                        None => None,
                    },
                };
            let req = GlobRequest {
                base,
                pattern: pattern.to_string(),
                regex: re,
                mtime: (mtime.newer_than.is_some() || mtime.older_than.is_some()).then_some(mtime),
            };
            let backend = select_backend();
            let mut result = backend
                .find(&req)
                .map_err(|e| tool_failure("glob", "GLOB_FAILED", e))?;
            result.matches.sort();
            result.matches.dedup();

            if result.matches.is_empty() {
                let mut output = format!("no matches for {pattern} in {}", req.base.display());
                if req.mtime.is_some() {
                    output.push_str(" (within the requested mtime window)");
                }
                return Ok(Value::new(serde_json::json!({ "output": output })));
            }
            if result.matches.len() <= MAX_RESULTS {
                return Ok(Value::new(serde_json::json!({
                    "output": result.matches.join("\n")
                })));
            }

            // Oversized: persist the full list, return the first page and a
            // locator. When persistence fails, degrade to a marked page.
            let total = result.matches.len();
            let page: Vec<String> = result.matches[..MAX_RESULTS].to_vec();
            let mut output = page.join("\n");
            match write_spill(&req, &result.matches) {
                Some(id) => {
                    output.push_str(&format!(
                        "\n[glob truncated at {MAX_RESULTS} of {total} matches; full result spilled: id={id}; retrieve with {{\"spill\":\"{id}\",\"offset\":{MAX_RESULTS},\"limit\":{MAX_RESULTS}}}]"
                    ));
                    if result.truncated {
                        output.push_str(&format!(
                            "\n[collection stopped at {COLLECT_CAP} matches — tree may hold more]"
                        ));
                    }
                }
                None => {
                    output.push_str(&format!(
                        "\n[glob truncated at {MAX_RESULTS} of {total} matches; spill write failed — narrow the pattern or path]"
                    ));
                }
            }
            Ok(Value::new(serde_json::json!({ "output": output })))
        })
    }
}

/// tool-guide parity (`tool:glob` → `glob:guide` @ 140).
pub fn guide() -> (&'static str, i32, &'static str) {
    (
        "glob",
        140,
        "Use the glob tool — not shell find or ls — to discover files by name pattern. It skips .git, node_modules and target; narrow with path when one subtree is enough.",
    )
}

pub fn glob_definition() -> ToolDefinition {
    ToolDefinition {
        name: "glob".into(),
        description: "Find files matching a glob pattern. Parameters: pattern (e.g. **/*.rs), path (optional directory, default cwd), newer_than/older_than (optional: seconds ago or ISO timestamp, filters by modification time), spill/offset/limit (retrieve a page from a previous oversized result using the id in its truncation note). Skips .git/node_modules/target.".into(),
        parameters: Value::new(serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "glob pattern such as src/**/*.rs" },
                "path": { "type": "string", "description": "directory to search, relative to cwd" },
                "newer_than": { "type": ["integer", "string"], "description": "keep entries modified after this bound: seconds ago (e.g. 3600) or ISO timestamp" },
                "older_than": { "type": ["integer", "string"], "description": "keep entries modified before this bound: seconds ago or ISO timestamp" },
                "spill": { "type": "string", "description": "id of a previous oversized result, to read a page instead of searching" },
                "offset": { "type": "integer", "description": "0-based page offset (with spill, or ignored when searching)" },
                "limit": { "type": "integer", "description": "page size for spill retrieval (default 200, max 1000)" }
            },
            "required": []
        })),
        output: Value::new(serde_json::json!({
            "type": "object",
            "properties": { "output": { "type": "string" } }
        })),
        render: Arc::new(text_content),
        executor: Arc::new(Glob),
        is_concurrency_safe: None,
        timeout: None,
        finalize_content: None,
        requires_approval: false,
        deny_in_readonly: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use starriver_kernel::tools::ToolExecution;
    use std::collections::HashSet;

    async fn run(args: serde_json::Value, cwd: std::path::PathBuf) -> Result<Value, ToolError> {
        let execution = ToolExecution {
            call_id: starriver_kernel::llm::CallId("c1".into()),
            root_call_id: starriver_kernel::llm::CallId("c1".into()),
            name: "glob".into(),
            arguments: Value::new(args.clone()),
            agent_id: None,
            cwd: Some(cwd),
            sandbox_mode: None,
            parent: None,
            token: starriver_kernel::tools::mint_tool_execution_token(),
            cancellation: None,
        };
        let ctx = ToolRunContext::new(execution);
        glob_definition()
            .executor
            .execute(Value::new(args), ctx)
            .await
    }

    fn out_of(result: &Value) -> String {
        result.downcast::<serde_json::Value>().unwrap()["output"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn req(pattern: &str, base: &Path) -> GlobRequest {
        GlobRequest {
            base: base.to_path_buf(),
            pattern: pattern.into(),
            regex: glob_to_regex(pattern).unwrap(),
            mtime: None,
        }
    }

    // ── existing behavior (regression net) ─────────────────────────────────

    #[tokio::test]
    async fn glob_matches_nested_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/lib")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main(){}").unwrap();
        std::fs::write(dir.path().join("src/lib/mod.rs"), "mod;").unwrap();
        std::fs::write(dir.path().join("README.md"), "# hi").unwrap();
        let result = run(
            serde_json::json!({ "pattern": "**/*.rs", "path": "." }),
            dir.path().to_path_buf(),
        )
        .await
        .unwrap();
        let out = out_of(&result);
        assert!(out.contains("src/main.rs"));
        assert!(out.contains("src/lib/mod.rs"));
        assert!(!out.contains("README.md"));
    }

    #[tokio::test]
    async fn glob_single_star_does_not_cross_separator() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a")).unwrap();
        std::fs::write(dir.path().join("a/b.rs"), "").unwrap();
        std::fs::write(dir.path().join("x.rs"), "").unwrap();
        let result = run(
            serde_json::json!({ "pattern": "*.rs", "path": "." }),
            dir.path().to_path_buf(),
        )
        .await
        .unwrap();
        let out = out_of(&result);
        assert!(out.contains("x.rs"));
        assert!(!out.contains("a/b.rs"));
    }

    #[tokio::test]
    async fn glob_skips_noise_directories() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("node_modules/pkg")).unwrap();
        std::fs::write(dir.path().join("node_modules/pkg/index.js"), "").unwrap();
        std::fs::write(dir.path().join("keep.rs"), "").unwrap();
        let result = run(
            serde_json::json!({ "pattern": "**/*", "path": "." }),
            dir.path().to_path_buf(),
        )
        .await
        .unwrap();
        let out = out_of(&result);
        assert!(out.contains("keep.rs"));
        assert!(!out.contains("node_modules"));
    }

    #[tokio::test]
    async fn glob_missing_pattern_is_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let result = run(serde_json::json!({}), dir.path().to_path_buf()).await;
        assert!(
            matches!(result, Err(ToolError::Execution { code, .. }) if code == "INVALID_INPUT")
        );
    }

    #[tokio::test]
    async fn glob_missing_dir_fails() {
        let dir = tempfile::tempdir().unwrap();
        let result = run(
            serde_json::json!({ "pattern": "**/*.rs", "path": "nope" }),
            dir.path().to_path_buf(),
        )
        .await;
        assert!(matches!(result, Err(ToolError::Execution { code, .. }) if code == "NOT_FOUND"));
    }

    #[test]
    fn glob_to_regex_translates_globs() {
        let re = glob_to_regex("**/*.rs").unwrap();
        assert!(re.is_match("src/main.rs"));
        assert!(re.is_match("main.rs"));
        let single = glob_to_regex("src/?.rs").unwrap();
        assert!(single.is_match("src/a.rs"));
        assert!(!single.is_match("src/ab.rs"));
        assert!(glob_to_regex("[abc").is_err());
    }

    // ── backends ───────────────────────────────────────────────────────────

    #[test]
    fn backends_agree_on_a_plain_tree() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/deep")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "").unwrap();
        std::fs::write(dir.path().join("src/deep/mod.rs"), "").unwrap();
        std::fs::write(dir.path().join("x.txt"), "").unwrap();
        let walkdir_out = WalkdirBackend.find(&req("**/*.rs", dir.path())).unwrap();
        let rg_out = RipgrepBackend.find(&req("**/*.rs", dir.path())).unwrap();
        assert_eq!(walkdir_out.matches, vec!["src/deep/mod.rs", "src/main.rs"]);
        assert_eq!(walkdir_out.matches, rg_out.matches);
    }

    #[test]
    fn ripgrep_backend_respects_gitignore_and_hidden() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("keep.rs"), "").unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored.rs\n").unwrap();
        std::fs::write(dir.path().join("ignored.rs"), "").unwrap();
        std::fs::write(dir.path().join(".hidden.rs"), "").unwrap();
        let out = RipgrepBackend.find(&req("**/*", dir.path())).unwrap();
        let set: HashSet<_> = out.matches.into_iter().collect();
        assert!(set.contains("keep.rs"));
        // .gitignore and .hidden.rs are dotfiles (hidden) and skipped by the
        // ripgrep walk; ignored.rs is skipped by the .gitignore rule.
        assert!(!set.contains(".gitignore"), "hidden entries are skipped");
        assert!(
            !set.contains("ignored.rs"),
            "gitignored entries are skipped"
        );
        assert!(!set.contains(".hidden.rs"), "hidden entries are skipped");
    }

    // ── mtime filtering ────────────────────────────────────────────────────

    #[tokio::test]
    async fn newer_than_keeps_fresh_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("fresh.rs"), "").unwrap();
        // "within the last hour" must include a file just written.
        let out = out_of(
            &run(
                serde_json::json!({ "pattern": "*.rs", "path": ".", "newer_than": 3600 }),
                dir.path().to_path_buf(),
            )
            .await
            .unwrap(),
        );
        assert!(out.contains("fresh.rs"), "{out}");
    }

    #[tokio::test]
    async fn older_than_excludes_fresh_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("fresh.rs"), "").unwrap();
        let out = out_of(
            &run(
                serde_json::json!({ "pattern": "*.rs", "path": ".", "older_than": 3600 }),
                dir.path().to_path_buf(),
            )
            .await
            .unwrap(),
        );
        assert!(out.contains("no matches"), "{out}");
    }

    #[tokio::test]
    async fn window_combines_both_bounds() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("fresh.rs"), "").unwrap();
        // newer_than=3600 (mtime > now-1h) AND older_than=7200 (mtime <
        // now-2h) is an empty window for a just-written file.
        let out = out_of(
            &run(
                serde_json::json!({ "pattern": "*.rs", "path": ".", "newer_than": 3600, "older_than": 7200 }),
                dir.path().to_path_buf(),
            )
            .await
            .unwrap(),
        );
        assert!(out.contains("no matches"), "{out}");
    }

    #[tokio::test]
    async fn iso_timestamp_bounds_are_accepted() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "").unwrap();
        let out = out_of(
            &run(
                serde_json::json!({ "pattern": "*.rs", "path": ".", "newer_than": "2000-01-01T00:00:00Z" }),
                dir.path().to_path_buf(),
            )
            .await
            .unwrap(),
        );
        assert!(out.contains("a.rs"), "{out}");
        let out = out_of(
            &run(
                serde_json::json!({ "pattern": "*.rs", "path": ".", "newer_than": "2999-01-01" }),
                dir.path().to_path_buf(),
            )
            .await
            .unwrap(),
        );
        assert!(out.contains("no matches"), "{out}");
    }

    #[tokio::test]
    async fn bad_time_bound_is_invalid_input() {
        let dir = tempfile::tempdir().unwrap();
        let err = run(
            serde_json::json!({ "pattern": "*.rs", "path": ".", "newer_than": "not a time" }),
            dir.path().to_path_buf(),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code(), "INVALID_INPUT");
    }

    #[tokio::test]
    async fn directory_pruning_skips_stale_subtrees_walkdir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/a.rs"), "").unwrap();
        std::fs::write(dir.path().join("top.rs"), "").unwrap();
        // older_than=1: any directory or file touched "now" fails, so the
        // whole fresh tree is pruned (files AND directories fail the bound).
        let out = out_of(
            &run(
                serde_json::json!({ "pattern": "**/*.rs", "path": ".", "older_than": 1 }),
                dir.path().to_path_buf(),
            )
            .await
            .unwrap(),
        );
        assert!(out.contains("no matches"), "{out}");
    }

    #[test]
    fn parse_time_bound_shapes() {
        let rel = parse_time_bound("newer_than", &serde_json::json!(0))
            .unwrap()
            .unwrap();
        // 0 seconds ago ≈ now: compare against a timestamp taken after the
        // call so a clock tick between the two samplings cannot fail this.
        assert!(rel <= SystemTime::now());
        assert!(parse_time_bound("x", &serde_json::Value::Null)
            .unwrap()
            .is_none());
        assert!(parse_time_bound("x", &serde_json::json!(-5)).is_err());
        assert!(parse_time_bound("x", &serde_json::json!(3.5)).is_err());
        assert!(parse_time_bound("x", &serde_json::json!(true)).is_err());
        assert!(parse_time_bound("x", &serde_json::json!("2000-01-01 00:00:00")).is_ok());
    }

    // ── spill ──────────────────────────────────────────────────────────────
    // HOME is process-global: with_temp_home holds the shared HOME lock, so
    // the async executor must be driven inline (multi-thread runtime makes
    // block_in_place legal inside the closure).

    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn oversized_results_spill_and_page() {
        crate::testutil::with_temp_home(|home| {
            let dir = tempfile::tempdir().unwrap();
            for i in 0..(MAX_RESULTS + 50) {
                std::fs::write(dir.path().join(format!("f{i:04}.rs")), "").unwrap();
            }
            let out = out_of(
                &block_on(run(
                    serde_json::json!({ "pattern": "*.rs", "path": "." }),
                    dir.path().to_path_buf(),
                ))
                .unwrap(),
            );
            assert!(
                out.contains(&format!(
                    "truncated at {MAX_RESULTS} of {}",
                    MAX_RESULTS + 50
                )),
                "{out}"
            );
            let id_start = out.find("id=glob-").expect("locator") + "id=".len();
            let id_end = out[id_start..].find(';').expect(";") + id_start;
            let id = out[id_start..id_end].to_string();

            // Spill file exists and carries the full list.
            let spill =
                std::fs::read_to_string(home.join(".starriver/spill").join(format!("{id}.json")))
                    .unwrap();
            let body: serde_json::Value = serde_json::from_str(&spill).unwrap();
            assert_eq!(body["total"], (MAX_RESULTS + 50) as u64);
            assert_eq!(body["matches"].as_array().unwrap().len(), MAX_RESULTS + 50);

            // Second page retrieval.
            let page = out_of(
                &block_on(run(
                    serde_json::json!({ "spill": id, "offset": MAX_RESULTS, "limit": 20 }),
                    dir.path().to_path_buf(),
                ))
                .unwrap(),
            );
            assert!(
                page.contains(&format!("showing {}-{}", MAX_RESULTS + 1, MAX_RESULTS + 20)),
                "{page}"
            );
            assert!(page.contains("f0200.rs"), "{page}");
            assert!(!page.contains("f0000.rs"), "{page}");
        });
    }

    #[tokio::test]
    async fn spill_retrieval_missing_file_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let err = run(
            serde_json::json!({ "spill": "glob-1-1" }),
            dir.path().to_path_buf(),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code(), "NOT_FOUND");
    }

    #[tokio::test]
    async fn spill_id_rejects_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let err = run(
            serde_json::json!({ "spill": "../evil" }),
            dir.path().to_path_buf(),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code(), "INVALID_INPUT");
    }
}
