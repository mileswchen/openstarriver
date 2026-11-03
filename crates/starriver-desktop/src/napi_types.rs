// napi flat types — convert starriver-core enums to napi-compatible structs

use napi_derive::napi;

/// Tool information
#[napi(object)]
#[derive(Debug, Clone)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    pub requires_approval: bool,
}

/// Agent info
#[napi(object)]
#[derive(Debug, Clone)]
pub struct AgentInfo {
    pub name: String,
    pub description: String,
    pub version: String,
}

#[napi(object)]
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

#[napi(object)]
#[derive(Debug, Clone)]
pub struct SkillUsageRecord {
    pub session_id: Option<String>,
    pub project_id: Option<String>,
    pub skill: String,
    pub version: Option<String>,
    pub mode: String,
    pub ts: i64,
}

#[napi(object)]
#[derive(Debug, Clone)]
pub struct CompactResult {
    pub before: u32,
    pub after: u32,
}

/// /usage result: session token estimate and message count.
#[napi(object)]
#[derive(Debug, Clone)]
pub struct SessionUsage {
    pub tokens: u32,
    pub count: u32,
}

#[napi(object)]
#[derive(Debug, Clone)]
pub struct PluginEntry {
    pub name: String,
    pub description: String,
    pub version: String,
    pub skills: Vec<String>,
    /// FPM P3: UI contribution manifest (JSON string, keeps the napi object
    /// flat).
    pub ui: Option<String>,
    pub source_url: String,
    pub installed_at: String,
}

#[napi(object)]
#[derive(Debug, Clone)]
pub struct MarketEntry {
    pub name: String,
    pub description: String,
    pub url: String,
    pub entry_type: String,
    pub category: String,
    pub version: String,
}
#[napi(object)]
#[derive(Debug, Clone)]
pub struct ModelProvider {
    pub id: String,
    pub name: String,
    pub provider_type: String,
    pub base_url: String,
    pub models: Vec<String>,
    pub requires_api_key: bool,
    pub enabled: bool,
    /// Inference API format — "anthropic" (Messages API) or "openai-chat" (OpenAI-compat). Mirrors ModelEntry.generation_config.api_format.
    pub api_format: Option<String>,
    /// Default reasoning effort hint for the preset ("off"|"low"|"high"|"max"). Mirrors ModelEntry.generation_config.reasoning_effort.
    pub reasoning_effort: Option<String>,
}

/// A model variant under a provider
#[napi(object)]
#[derive(Debug, Clone)]
pub struct ModelVariant {
    pub id: String,
    pub name: String,
    /// Per-model image-input declaration (dsh pattern): null = undeclared
    /// (permissive); false = declared text-only, image requests fail loudly
    /// at request assembly.
    pub supports_vision: Option<bool>,
}

/// Global model registry entry
#[napi(object)]
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
    /// Custom model ID used as the selector-name prefix; null = use `name`.
    pub model_label: Option<String>,
    /// Legacy, provider-level declaration (no UI, ignored — capability is
    /// declared per model variant now).
    pub supports_vision: Option<bool>,
}

/// Per-model enabled state within an agent binding
#[napi(object)]
#[derive(Debug, Clone)]
pub struct BoundModelInfo {
    pub id: String,
    pub name: String,
    pub enabled: bool,
}

/// Per-agent model binding summary
#[napi(object)]
#[derive(Debug, Clone)]
pub struct AgentModelBinding {
    pub agent_name: String,
    pub active_model: String,
    pub bound_models: Vec<BoundModelInfo>,
    /// Session-scoped reasoning effort override; null = provider default.
    pub reasoning_effort: Option<String>,
}

/// Directory git status (status bar + isolation toggle availability).
#[napi(object)]
#[derive(Debug, Clone)]
pub struct WorktreeStatus {
    pub is_repo: bool,
    pub branch: Option<String>,
    pub is_worktree: bool,
    pub worktree_path: Option<String>,
    pub dirty: bool,
}

/// Durable reference to one stored normalized image attachment.
#[napi(object)]
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

impl From<starriver_host::types::AttachmentRef> for AttachmentRef {
    fn from(value: starriver_host::types::AttachmentRef) -> Self {
        Self {
            id: value.id,
            media_type: value.media_type,
            width: value.width,
            height: value.height,
            bytes: value.bytes,
            name: value.name,
            original_width: value.original_width,
            original_height: value.original_height,
        }
    }
}

/// Stored normalized image read back for a model request.
#[napi(object)]
#[derive(Debug, Clone)]
pub struct StoredAttachment {
    pub id: String,
    pub media_type: String,
    pub width: u32,
    pub height: u32,
    pub base64: String,
}

/// UI-safe presence/writability facts for one credential record — never the
/// value.
#[napi(object)]
#[derive(Debug, Clone)]
pub struct CredentialRecordInfo {
    pub configured: bool,
    pub kind: Option<String>,
    pub writable: bool,
}

impl From<starriver_host::types::CredentialRecordInfo> for CredentialRecordInfo {
    fn from(value: starriver_host::types::CredentialRecordInfo) -> Self {
        Self {
            configured: value.configured,
            kind: value.kind,
            writable: value.writable,
        }
    }
}

/// One stored credential record's address and tag, for enumeration.
#[napi(object)]
#[derive(Debug, Clone)]
pub struct CredentialRecordEntry {
    pub key: String,
    pub kind: String,
}

/// One configurable-provider directory entry, surfaced to the settings UI.
#[napi(object)]
#[derive(Debug, Clone)]
pub struct ConfigurableProvider {
    pub provider: String,
    pub display_name: String,
    pub settings_ns: String,
    pub settings_path: Vec<String>,
    pub declared: bool,
    pub configured: bool,
}

/// One subagent template's delegation config.
#[napi(object)]
#[derive(Debug, Clone)]
pub struct SubagentConfig {
    pub name: String,
    pub description: String,
    pub tools: Vec<String>,
    pub read_only: bool,
    pub enabled: bool,
    pub model: Option<String>,
}

impl From<starriver_host::types::SubagentConfig> for SubagentConfig {
    fn from(value: starriver_host::types::SubagentConfig) -> Self {
        Self {
            name: value.name,
            description: value.description,
            tools: value.tools,
            read_only: value.read_only,
            enabled: value.enabled,
            model: value.model,
        }
    }
}

/// One job (running or recent) in the unified job panel.
#[napi(object)]
#[derive(Debug, Clone)]
pub struct JobInfo {
    pub task_name: String,
    pub status: String,
    pub started_at: String,
    pub summary: String,
    pub session_id: Option<String>,
}

impl From<starriver_host::types::JobInfo> for JobInfo {
    fn from(value: starriver_host::types::JobInfo) -> Self {
        Self {
            task_name: value.task_name,
            status: value.status,
            started_at: value.started_at,
            summary: value.summary,
            session_id: value.session_id,
        }
    }
}

impl From<starriver_host::types::ConfigurableProvider> for ConfigurableProvider {
    fn from(value: starriver_host::types::ConfigurableProvider) -> Self {
        Self {
            provider: value.provider,
            display_name: value.display_name,
            settings_ns: value.settings_ns,
            settings_path: value.settings_path,
            declared: value.declared,
            configured: value.configured,
        }
    }
}

impl From<starriver_host::types::CredentialRecordEntry> for CredentialRecordEntry {
    fn from(value: starriver_host::types::CredentialRecordEntry) -> Self {
        Self {
            key: value.key,
            kind: value.kind,
        }
    }
}

impl From<starriver_host::types::StoredAttachment> for StoredAttachment {
    fn from(value: starriver_host::types::StoredAttachment) -> Self {
        Self {
            id: value.id,
            media_type: value.media_type,
            width: value.width,
            height: value.height,
            base64: value.base64,
        }
    }
}

/// Workspace registry view (workspace model).
#[napi(object)]
#[derive(Debug, Clone)]
pub struct WorkspaceInfo {
    pub id: String,
    pub path: String,
    pub title: String,
    pub created_at: String,
    pub session_ids: Vec<String>,
}

/// Session summary for history sidebar. Presentation metadata (model/agent)
/// is gone with the legacy store; grouping comes from the workspace registry.
#[napi(object)]
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
    /// Fork cutoff boundary: source session messages.id the fork copied before.
    pub fork_point: Option<i64>,
    /// Agent the session started with (write-once stamp; None = legacy row).
    pub agent: Option<String>,
    /// Session origin marker ("scheduled" = scheduled-task run; WI-152).
    pub origin: Option<String>,
}

/// A single message in a session
/// Turn token totals as `f64` (napi maps u64 to BigInt, same precedent as
/// `SessionMessage.id`).
#[napi(object)]
#[derive(Debug, Clone)]
pub struct UsageTotals {
    pub input_tokens: f64,
    pub output_tokens: f64,
}

/// One file touched by the turn's edit/write/patch tools. `add`/`del` are
/// `f64` per the BigInt-avoidance precedent; `diff` is the unified diff text.
#[napi(object)]
#[derive(Debug, Clone)]
pub struct FileChange {
    pub path: String,
    pub add: f64,
    pub del: f64,
    pub diff: String,
}

#[napi(object)]
#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
    /// Source event unix ms as `f64` (plain JS number; napi maps u64 to
    /// BigInt, same precedent as `SessionMessage.id`).
    pub ts: Option<f64>,
    /// Turn token totals on the turn's final assistant text piece.
    pub usage: Option<UsageTotals>,
    /// Turn duration (ms), derived from the first-event→turn/end window.
    pub duration_ms: Option<f64>,
    /// Turn completion time (the turn/end event's unix ms).
    pub done_ts: Option<f64>,
    /// Files changed by the turn's edit/write/patch tools.
    pub file_changes: Option<Vec<FileChange>>,
}

/// A timeline message with its log seq as the id — the backtrack picker
/// source. `id` is `f64` (not `i64`) so the renderer gets a plain JS number:
/// napi maps i64 to BigInt, which the panel would have to unwrap (same
/// precedent as `MemoryEntry.created`). Seqs never exceed 2^53 in practice.
#[napi(object)]
#[derive(Debug, Clone)]
pub struct SessionMessage {
    pub id: f64,
    pub role: String,
    pub content: String,
}

/// A single cross-session memory entry for the Memory panel. Timestamps are
/// unix seconds as `f64` so the renderer gets a plain JS number (napi maps i64
/// to BigInt, which the panel would have to unwrap).
#[napi(object)]
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
#[napi(object)]
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
#[napi(object)]
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
#[napi(object)]
#[derive(Debug, Clone)]
pub struct McpTestResult {
    pub ok: bool,
    pub tool_count: u32,
    pub tools: Vec<String>,
    pub error: Option<String>,
}

/// Result of a single tool trial-run (a real tools/call with sample arguments).
#[napi(object)]
#[derive(Debug, Clone)]
pub struct McpToolTestResult {
    pub ok: bool,
    pub output: Option<String>,
    pub error: Option<String>,
}

/// Scheduled task info
#[napi(object)]
#[derive(Debug, Clone)]
pub struct ScheduledTaskInfo {
    pub name: String,
    pub prompt: String,
    /// Model key "providerId/variantId"; empty = active agent model
    pub model: Option<String>,
    /// Workspace the task's runs bind to (None → ungrouped / Tasks section)
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
#[napi(object)]
#[derive(Debug, Clone)]
pub struct TaskRunRecord {
    pub timestamp: String,
    pub status: String,
    pub summary: String,
    /// Session id under which the full transcript was persisted. None for legacy runs.
    pub session_id: Option<String>,
}

/// A run record tagged with its owning task name (for the cross-task History view).
#[napi(object)]
#[derive(Debug, Clone)]
pub struct TaskRunRecordWithName {
    pub task_name: String,
    pub timestamp: String,
    pub status: String,
    pub summary: String,
    pub session_id: Option<String>,
}

/// One chronological entry in a session's run trace (one per content block).
#[napi(object)]
#[derive(Debug, Clone)]
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
    /// Ledger seq + wall-clock time of the source event.
    pub seq: f64,
    pub time_ms: f64,
    pub turn: Option<f64>,
    pub step: Option<f64>,
    /// Token usage (assistant entries).
    pub input_tokens: Option<f64>,
    pub output_tokens: Option<f64>,
    /// Active request route (assistant entries).
    pub provider: Option<String>,
    pub model: Option<String>,
    /// Streaming timings (assistant entries, from chunk/step timestamps).
    pub duration_ms: Option<f64>,
    pub ttft_ms: Option<f64>,
}

/// Full chronological trace of a session, reconstructed from persisted content blocks.
#[napi(object)]
#[derive(Debug, Clone)]
pub struct SessionTrace {
    pub entries: Vec<SessionTraceEntry>,
}
