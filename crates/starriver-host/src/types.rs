// napi flat types — convert starriver-core enums to napi-compatible structs

/// Agent event — napi flat version (all fields optional, type field distinguishes event type)
#[derive(Debug, Clone)]
pub struct AgentEvent {
    /// Event type
    pub event_type: String,
    pub session_id: Option<String>,
    pub summary: Option<String>,
    pub error: Option<String>,
    pub text: Option<String>,
    pub tool_id: Option<String>,
    pub tool_name: Option<String>,
    pub tool_input: Option<String>,
    pub tool_output: Option<String>,
    pub tool_success: Option<bool>,
    pub before_tokens: Option<i64>,
    pub after_tokens: Option<i64>,
    pub reason: Option<String>,
}

/// Tool information
#[derive(Debug, Clone)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    pub requires_approval: bool,
}

/// Agent info
#[derive(Debug, Clone)]
pub struct AgentInfo {
    pub name: String,
    pub description: String,
    pub version: String,
}

#[derive(Debug, Clone)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
    pub has_command: bool,
    pub command: Option<String>,
    pub version: Option<String>,
    pub always_on: bool,
    pub has_keyword: bool,
    pub has_suffix: bool,
    pub has_hooks: bool,
    pub enabled: bool,
    /// Full SKILL.md body (frontmatter + body) for preview/editing.
    pub body: String,
}

#[derive(Debug, Clone)]
pub struct SkillUsageRecord {
    pub session_id: Option<String>,
    pub project_id: Option<String>,
    pub skill: String,
    pub version: Option<String>,
    pub mode: String,
    pub ts: i64,
}

#[derive(Debug, Clone)]
pub struct CompactResult {
    pub before: u32,
    pub after: u32,
}

#[derive(Debug, Clone)]
pub struct PluginEntry {
    pub name: String,
    pub description: String,
    pub version: String,
    pub skills: Vec<String>,
    /// FPM P3: frontend UI contribution; None for pure-skill plugins.
    pub ui: Option<PluginUiEntry>,
    pub source_url: String,
    pub installed_at: String,
}

/// Plugin UI manifest (mirrors `crate::plugin::PluginUiEntry`, consumed by the protocol/NAPI layer).
#[derive(Debug, Clone)]
pub struct PluginUiEntry {
    pub entry: String,
    pub tabs: Vec<PluginUiTab>,
    pub permissions: Vec<String>,
    pub theme: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PluginUiTab {
    pub id: String,
    pub title: String,
}

#[derive(Debug, Clone)]
pub struct MarketEntry {
    pub name: String,
    pub description: String,
    pub url: String,
    pub entry_type: String,
    pub category: String,
    pub version: String,
}
#[derive(Debug, Clone)]
pub struct ModelProvider {
    pub id: String,
    pub name: String,
    pub provider_type: String,
    pub base_url: String,
    pub models: Vec<String>,
    pub requires_api_key: bool,
    pub enabled: bool,
    /// Inference API format — "anthropic" (Messages API), "openai-chat"
    /// (completions), or "openai-responses" (Responses API). Mirrors
    /// ModelEntry.generation_config.api_format.
    pub api_format: Option<String>,
    /// Default reasoning effort hint for the preset ("off"|"low"|"high"|"max"). Mirrors ModelEntry.generation_config.reasoning_effort.
    pub reasoning_effort: Option<String>,
}

/// A model variant under a provider
#[derive(Debug, Clone)]
pub struct ModelVariant {
    pub id: String,
    pub name: String,
}

/// Global model registry entry
#[derive(Debug, Clone)]
pub struct ModelEntry {
    pub id: String,
    pub name: String,
    pub model_type: String,
    pub provider_type: String,
    pub base_url: String,
    pub api_key: String,
    pub model_path: Option<String>,
    pub models: Vec<ModelVariant>,
    pub default_model: String,
    pub concurrency: Option<u32>,
    pub max_tokens: Option<u32>,
    pub context_window: Option<u32>,
    pub temperature: Option<f64>,
    pub api_format: String,
    pub reasoning_effort: Option<String>,
    /// Custom model ID used as the selector-name prefix; None = use `name`.
    pub model_label: Option<String>,
    /// Whether the model supports image input (declared in model settings); None = undeclared.
    pub supports_vision: Option<bool>,
    /// Resolved vision support (UI declaration > built-in provider table >
    /// default supported). The composer gate and runtime text-only checks
    /// consume this, not the raw declaration (WI-152 S1).
    pub vision_supported: bool,
}

/// Per-model enabled state within an agent binding
#[derive(Debug, Clone)]
pub struct BoundModelInfo {
    pub id: String,
    pub name: String,
    pub enabled: bool,
}

/// Per-agent model binding summary
#[derive(Debug, Clone)]
pub struct AgentModelBinding {
    pub agent_name: String,
    pub active_model: String,
    pub bound_models: Vec<BoundModelInfo>,
    /// Agent-scoped reasoning effort override; None = follow provider config.
    pub reasoning_effort: Option<String>,
}

/// Directory git status (status bar + isolation toggle availability).
#[derive(Debug, Clone)]
pub struct WorktreeStatus {
    pub is_repo: bool,
    pub branch: Option<String>,
    pub is_worktree: bool,
    pub worktree_path: Option<String>,
    pub dirty: bool,
}

/// Durable reference to one stored normalized image attachment.
#[derive(Debug, Clone)]
pub struct AttachmentRef {
    pub id: String,
    pub media_type: String,
    pub width: u32,
    pub height: u32,
    pub bytes: u32,
    pub name: Option<String>,
    pub original_width: Option<u32>,
    pub original_height: Option<u32>,
}

/// Stored normalized image read back for a model request (base64 payload +
/// scaled dimensions).
#[derive(Debug, Clone)]
pub struct StoredAttachment {
    pub id: String,
    pub media_type: String,
    pub width: u32,
    pub height: u32,
    pub base64: String,
}

/// UI-safe presence/writability facts for one credential record — never the
/// value (`CredentialRecordInfo`).
#[derive(Debug, Clone)]
pub struct CredentialRecordInfo {
    pub configured: bool,
    pub kind: Option<String>,
    pub writable: bool,
}

/// One stored credential record's address and tag, for enumeration.
#[derive(Debug, Clone)]
pub struct CredentialRecordEntry {
    pub key: String,
    pub kind: String,
}

/// One provider the kernel's configurable-provider directory declares,
/// surfaced to the settings UI (`ConfigurablePluginsTab` namespace card).
#[derive(Debug, Clone)]
pub struct ConfigurableProvider {
    pub provider: String,
    pub display_name: String,
    pub settings_ns: String,
    pub settings_path: Vec<String>,
    pub declared: bool,
    pub configured: bool,
}

/// One subagent template's delegation config surfaced to the settings UI.
#[derive(Debug, Clone)]
pub struct SubagentConfig {
    pub name: String,
    pub description: String,
    pub tools: Vec<String>,
    pub read_only: bool,
    pub enabled: bool,
    pub model: Option<String>,
}

/// One job (running or recent) in the unified job panel (jobs parity).
#[derive(Debug, Clone)]
pub struct JobInfo {
    pub task_name: String,
    /// "running" | "completed" | "failed" | "killed"
    pub status: String,
    pub started_at: String,
    pub summary: String,
    pub session_id: Option<String>,
}

/// Workspace registry view (workspace model): a stable id over a
/// directory path plus the manually ordered session account.
#[derive(Debug, Clone)]
pub struct WorkspaceInfo {
    pub id: String,
    pub path: String,
    pub title: String,
    pub created_at: String,
    pub session_ids: Vec<String>,
}

/// Session summary for history sidebar. Model metadata is gone with the
/// legacy store; grouping comes from the workspace registry.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub id: String,
    pub title: String,
    /// Owning workspace id (None → Ungrouped).
    pub workspace_id: Option<String>,
    pub created_at: String,
    pub message_count: u32,
    /// Fork source session id (NULL for root sessions).
    pub parent_id: Option<String>,
    /// Fork cutoff boundary in the currency of the source log's seqs.
    pub fork_point: Option<i64>,
    /// Agent the session started with (stamped write-once at first run;
    /// `None` for sessions created before the stamp existed — the UI falls
    /// back to workspace membership).
    pub agent: Option<String>,
    /// Session origin marker (`"scheduled"` = scheduled-task run; `None` =
    /// plain conversation). Drives the sidebar Tasks grouping (WI-152).
    pub origin: Option<String>,
}

/// Turn-level token totals (input after cache deduction / output), folded
/// from the turn's per-message usage onto its final assistant text piece.
#[derive(Debug, Clone)]
pub struct TokenTotals {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// One file touched by the turn's edit/write/patch tools: add/del counts plus
/// the unified diff full text (rendered by the frontend DiffView on click).
#[derive(Debug, Clone)]
pub struct FileChange {
    pub path: String,
    pub add: u64,
    pub del: u64,
    pub diff: String,
}

/// A single message in a session
#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
    /// Source surface-entry seq (BUG-86): the history page's own cursor —
    /// the renderer uses it for "load earlier" anchoring and fork points
    /// instead of re-aligning against the full timeline.
    pub seq: Option<u64>,
    /// Source event wall-clock time (ms epoch) for display timestamps; the
    /// kernel log already records it, this just projects it to the UI.
    pub ts: Option<u64>,
    /// Turn token totals carried by the turn's final assistant text piece.
    pub usage: Option<TokenTotals>,
    /// Turn duration (ms), derived from the first-event→turn/end wall-clock
    /// window (pure projection — the log already carries every timestamp).
    pub duration_ms: Option<u64>,
    /// Turn completion time = the turn/end event's wall-clock time.
    pub done_ts: Option<u64>,
    /// Files changed by the turn's edit/write/patch tools (latest per path),
    /// carried by the turn's final assistant text piece.
    pub file_changes: Option<Vec<FileChange>>,
}

/// A message carrying its log seq as the id — the backtrack timeline's
/// currency; the fork cutoff is expressed in these very seq values.
#[derive(Debug, Clone)]
pub struct SessionMessage {
    pub id: i64,
    pub role: String,
    pub content: String,
}

/// Workspace instructions banner payload with truncation metadata (WI-35 B).
#[derive(Debug, Clone)]
pub struct WorkspaceInstructionsMeta {
    pub content: Option<String>,
    pub truncated: bool,
    pub bytes: usize,
    pub budget: usize,
}

/// A single cross-session memory entry for the Memory panel. Timestamps are
/// unix seconds as `f64` so the renderer gets a plain JS number (napi maps i64
/// to BigInt, which the panel would have to unwrap).
#[derive(Debug, Clone)]
pub struct MemoryEntry {
    pub name: String,
    pub description: String,
    pub body: String,
    /// "compaction" | "extract"
    pub source: String,
 /// Agent whose session produced this memory ("starriver" | "" |...).
    pub source_agent: String,
    /// project_id, or "_global" for cross-project facts
    pub project: String,
    pub created: f64,
    pub updated: f64,
}

/// OAuth config as surfaced to the renderer for editing. The client secret is masked:
/// only its presence is reported (`has_secret`), never the plaintext value.
#[derive(Debug, Clone)]
pub struct McpOAuthInfo {
    pub authorization_url: String,
    pub token_url: String,
    pub client_id: String,
    pub scopes: Vec<String>,
    pub redirect_uri: String,
    pub has_secret: bool,
}

/// MCP server configuration info
#[derive(Debug, Clone)]
pub struct McpServerInfo {
    pub name: String,
    pub transport: String, // "stdio" | "sse" | "http"
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<String>, // "KEY=value" pairs
    pub url: Option<String>,
    pub oauth_connected: bool,
    pub oauth: Option<McpOAuthInfo>,
    pub tool_count: u32,
    pub status: String, // "connected" | "disconnected" | "error"
}

/// Result of testing an MCP server's connectivity + tool discovery (without registering).
#[derive(Debug, Clone)]
pub struct McpTestResult {
    pub ok: bool,
    pub tool_count: u32,
    pub tools: Vec<String>,
    pub error: Option<String>,
}

/// Result of a single tool trial-run (a real tools/call with sample arguments).
#[derive(Debug, Clone)]
pub struct McpToolTestResult {
    pub ok: bool,
    pub output: Option<String>,
    pub error: Option<String>,
}

/// Scheduled task info
#[derive(Debug, Clone)]
pub struct ScheduledTaskInfo {
    pub name: String,
    pub prompt: String,
    /// Model key "providerId/variantId"; empty = active agent model
    pub model: Option<String>,
    /// Agent the task runs as; None = the built-in Chat Agent
    pub agent: Option<String>,
    /// Workspace the task's runs bind to; None/empty = ungrouped runs
    pub workspace: Option<String>,
    pub enabled: bool,
    /// "periodic" | "interval" | "once"
    pub mode_type: String,
    /// Period: daily/weekly/biweekly/monthly/quarterly/yearly
    pub period: Option<String>,
    /// Time of day "HH:MM" (Periodic)
    pub time: Option<String>,
    /// 0=Sun..6=Sat (Periodic:Weekly)
    pub day_of_week: Option<u8>,
    /// 1-31 (Periodic:Monthly)
    pub day_of_month: Option<u8>,
    /// 1-12 (Periodic:Yearly)
    pub month: Option<u8>,
    /// 1-24 (Interval)
    pub hours: Option<u32>,
    /// 0=Sun..6=Sat (Interval); empty = every day
    pub days_of_week: Vec<u8>,
    /// ISO 8601 (Once)
    pub datetime: Option<String>,
    /// Validity window start (YYYY-MM-DD)
    pub start_date: Option<String>,
    /// Validity window end (YYYY-MM-DD)
    pub end_date: Option<String>,
    /// Skill names injected into the run's system prompt
    pub skills: Vec<String>,
    /// Human-readable schedule description
    pub description: String,
    pub last_run: Option<String>,
    pub next_run: Option<String>,
    pub last_status: Option<String>,
}

/// A single scheduled task run record
#[derive(Debug, Clone)]
pub struct TaskRunRecord {
    pub timestamp: String,
    pub status: String,
    pub summary: String,
    /// Session id under which the full transcript was persisted. None for legacy runs.
    pub session_id: Option<String>,
}

/// A run record tagged with its owning task name (for the cross-task History view).
#[derive(Debug, Clone)]
pub struct TaskRunRecordWithName {
    pub task_name: String,
    pub timestamp: String,
    pub status: String,
    pub summary: String,
    pub session_id: Option<String>,
}

/// One chronological entry in a session's run trace (one per content block).
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionTraceEntry {
    /// "user" | "assistant" | "thinking" | "tool_use" | "tool_result"
    /// | "system" | "context"
    pub kind: String,
    /// Text content for user / assistant / thinking entries.
    pub text: String,
    /// Tool name (tool_use only).
    pub tool_name: Option<String>,
    /// Raw tool input JSON (tool_use only).
    pub tool_input: Option<String>,
    /// Tool output text (tool_result only).
    pub tool_output: Option<String>,
    /// Whether the tool call succeeded (tool_result only).
    pub tool_ok: Option<bool>,
    /// Ledger position and wall-clock time of the source event.
    pub seq: f64,
    pub time_ms: f64,
    /// Turn / step indexes (assistant & tool entries; from the event data).
    pub turn: Option<f64>,
    pub step: Option<f64>,
    /// Token usage (assistant entries).
    pub input_tokens: Option<f64>,
    pub output_tokens: Option<f64>,
    /// Active route when this entry was produced (assistant entries; from the
    /// preceding request/header).
    pub provider: Option<String>,
    pub model: Option<String>,
    /// Assistant entries: decoding duration (last chunk − append) and TTFT
    /// (first chunk − step start), both from persisted chunk/step timings.
    pub duration_ms: Option<f64>,
    pub ttft_ms: Option<f64>,
}

/// Full chronological trace of a session, reconstructed from persisted content blocks.
#[derive(Debug, Clone)]
pub struct SessionTrace {
    pub entries: Vec<SessionTraceEntry>,
}

/// Complete event-ledger projection for a session (session.archive): every
/// persisted entry INCLUDING the spans shadowed by context compaction, plus
/// labeled compaction-checkpoint rows. `compactions` counts the checkpoints.
#[derive(Debug, Clone)]
pub struct SessionArchive {
    pub entries: Vec<SessionTraceEntry>,
    pub compactions: u32,
}
