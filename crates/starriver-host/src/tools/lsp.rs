//! LSP tool: one `lsp` tool with nine operations over one language server
//! per workspace root.
//!
//! Operations: goToDefinition / findReferences / goToImplementation / hover
//! (alignment), symbols / diagnostics (legacy capability preserved), and
//! the dynamic set — rename / codeAction / workspaceSymbol — each gated on
//! the capabilities the server returned at initialize (a server without
//! `renameProvider` gets a NOT_FOUND instead of a protocol error).

use std::sync::Arc;
use std::time::Duration;

use cordis_rslite::Value;
use lsp_types::{
    DocumentSymbolResponse, Hover, HoverContents, Location, LocationLink, MarkedString, Range,
    TextEdit,
};
use starriver_kernel::sandbox::SandboxMode;
use starriver_kernel::tools::{ToolDefinition, ToolError, ToolExecutor, ToolRunContext};

use super::common::{invalid_input, json_args, resolve_path, text_content, tool_failure};
use crate::lsp::{file_uri, LspManager};

const DEFAULT_MAX_LOCATIONS: usize = 100;
const TOOL_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Operation {
    GoToDefinition,
    FindReferences,
    GoToImplementation,
    Hover,
    Symbols,
    Diagnostics,
    Rename,
    CodeAction,
    WorkspaceSymbol,
}

impl Operation {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "goToDefinition" => Some(Self::GoToDefinition),
            "findReferences" => Some(Self::FindReferences),
            "goToImplementation" => Some(Self::GoToImplementation),
            "hover" => Some(Self::Hover),
            "symbols" => Some(Self::Symbols),
            "diagnostics" => Some(Self::Diagnostics),
            "rename" => Some(Self::Rename),
            "codeAction" => Some(Self::CodeAction),
            "workspaceSymbol" => Some(Self::WorkspaceSymbol),
            _ => None,
        }
    }

    /// The LSP server capability that must be advertised for this operation
    /// (the dynamic set only; the base six stay unconditional).
    fn required_capability(self) -> Option<&'static str> {
        match self {
            Self::Rename => Some("renameProvider"),
            Self::CodeAction => Some("codeActionProvider"),
            Self::WorkspaceSymbol => Some("workspaceSymbolProvider"),
            _ => None,
        }
    }
}

struct Lsp {
    manager: Arc<LspManager>,
}

impl Lsp {
    fn new(manager: Arc<LspManager>) -> Self {
        Self { manager }
    }
}

/// Does `pos` (0-based) fall inside `r` (inclusive of both endpoints)?
fn contains(r: &Range, line: u32, col: u32) -> bool {
    let pos = (line, col);
    pos >= (r.start.line, r.start.character) && pos <= (r.end.line, r.end.character)
}

/// Path of the innermost symbol containing `pos`, e.g. `main::helper`.
fn path_at(syms: &[lsp_types::DocumentSymbol], line: u32, col: u32) -> Option<String> {
    for s in syms {
        if contains(&s.range, line, col) {
            let mut p = s.name.clone();
            if let Some(children) = &s.children {
                if let Some(inner) = path_at(children, line, col) {
                    p = format!("{p}::{inner}");
                }
            }
            return Some(p);
        }
    }
    None
}

fn format_nested(syms: &[lsp_types::DocumentSymbol], depth: usize, out: &mut Vec<String>) {
    for s in syms {
        out.push(format!(
            "{}{} ({}:{}-{}:{})",
            "  ".repeat(depth),
            s.name,
            s.range.start.line + 1,
            s.range.start.character + 1,
            s.range.end.line + 1,
            s.range.end.character + 1
        ));
        if let Some(children) = &s.children {
            format_nested(children, depth + 1, out);
        }
    }
}

fn format_symbols(result: serde_json::Value, line: Option<u32>, col: Option<u32>) -> String {
    if result.is_null() {
        return "No symbols (server returned null)".into();
    }
    match serde_json::from_value::<DocumentSymbolResponse>(result.clone()) {
        Ok(DocumentSymbolResponse::Nested(syms)) => {
            if let (Some(l), Some(c)) = (line, col) {
                return match path_at(&syms, l.saturating_sub(1), c.saturating_sub(1)) {
                    Some(p) => format!("Symbol at {l}:{c} is `{p}`"),
                    None => format!("No symbol at {l}:{c}"),
                };
            }
            let mut lines = Vec::new();
            format_nested(&syms, 0, &mut lines);
            if lines.is_empty() {
                "No symbols".into()
            } else {
                lines.join("\n")
            }
        }
        Ok(DocumentSymbolResponse::Flat(items)) => {
            let mut lines = Vec::new();
            for s in items {
                lines.push(format!(
                    "{} @ {}:{}",
                    s.name,
                    s.location.range.start.line + 1,
                    s.location.range.start.character + 1
                ));
            }
            if lines.is_empty() {
                "No symbols".into()
            } else {
                lines.join("\n")
            }
        }
        Err(_) => format!("Unexpected documentSymbol response: {result}"),
    }
}

fn format_diagnostics(diags: Vec<lsp_types::Diagnostic>) -> String {
    if diags.is_empty() {
        return "No diagnostics (file is clean, or still analyzing)".into();
    }
    let mut out = Vec::new();
    for d in diags {
        let severity = match d.severity {
            Some(lsp_types::DiagnosticSeverity::ERROR) => "error",
            Some(lsp_types::DiagnosticSeverity::WARNING) => "warning",
            Some(lsp_types::DiagnosticSeverity::INFORMATION) => "info",
            Some(lsp_types::DiagnosticSeverity::HINT) => "hint",
            _ => "diagnostic",
        };
        out.push(format!(
            "{severity} {}:{}–{}:{} {}",
            d.range.start.line + 1,
            d.range.start.character + 1,
            d.range.end.line + 1,
            d.range.end.character + 1,
            d.message
        ));
    }
    out.join("\n")
}

fn display_uri(uri: &str) -> String {
    uri.strip_prefix("file://").unwrap_or(uri).to_string()
}

fn format_location(location: &Location) -> String {
    format!(
        "{}:{}:{}",
        display_uri(location.uri.as_str()),
        location.range.start.line + 1,
        location.range.start.character + 1
    )
}

fn format_location_link(link: &LocationLink) -> String {
    format!(
        "{}:{}:{}",
        display_uri(link.target_uri.as_str()),
        link.target_range.start.line + 1,
        link.target_range.start.character + 1
    )
}

fn format_locations(result: serde_json::Value, limit: usize) -> String {
    let mut out: Vec<String> = Vec::new();
    if let Ok(mut v) = serde_json::from_value::<Vec<Location>>(result.clone()) {
        for l in v.drain(..) {
            out.push(format_location(&l));
        }
    } else if let Ok(mut v) = serde_json::from_value::<Vec<LocationLink>>(result.clone()) {
        for l in v.drain(..) {
            out.push(format_location_link(&l));
        }
    } else if let Ok(l) = serde_json::from_value::<Location>(result) {
        out.push(format_location(&l));
    }
    out.sort();
    out.dedup();
    let total = out.len();
    out.truncate(limit);
    let mut s = if out.is_empty() {
        "No locations found".to_string()
    } else {
        out.join("\n")
    };
    if total > limit {
        s.push_str(&format!("\n[locations truncated at {limit} of {total}]"));
    }
    s
}

fn format_hover(result: serde_json::Value) -> String {
    let Ok(hover) = serde_json::from_value::<Hover>(result) else {
        return "No hover content".into();
    };
    let mut parts = Vec::new();
    match hover.contents {
        HoverContents::Scalar(MarkedString::String(s)) => parts.push(s),
        HoverContents::Scalar(MarkedString::LanguageString(ls)) => parts.push(ls.value),
        HoverContents::Array(items) => {
            for item in items {
                match item {
                    MarkedString::String(s) => parts.push(s),
                    MarkedString::LanguageString(ls) => parts.push(ls.value),
                }
            }
        }
        HoverContents::Markup(m) => parts.push(m.value),
    }
    let text = parts.join("\n");
    if text.trim().is_empty() {
        "No hover content".into()
    } else {
        text
    }
}

// ── dynamic-capability helpers: WorkspaceEdit application ───────────────────

/// Convert an LSP position (0-based line, UTF-16 character offset) to a byte
/// offset in `text`. LSP positions are UTF-16 code units; Rust strings are
/// UTF-8, so each line is walked char by char accumulating `len_utf16`.
fn position_to_byte_offset(text: &str, line: u32, col: u32) -> Option<usize> {
    let mut offset = 0usize;
    let mut line_no = 0u32;
    for l in text.split_inclusive('\n') {
        if line_no == line {
            let mut units = 0u32;
            for (i, ch) in l.char_indices() {
                if units >= col {
                    return Some(offset + i);
                }
                units += ch.len_utf16() as u32;
            }
            return Some(offset + l.trim_end_matches('\n').len());
        }
        offset += l.len();
        line_no += 1;
    }
    // A position on the "ghost line" after the final newline is a legal
    // LSP EOF insertion point — treat it as end-of-text, not "outside".
    if line_no == line {
        return Some(text.len());
    }
    None
}

/// Apply `TextEdit`s to `text`: ranges become byte spans, applied in reverse
/// positional order so earlier edits never shift later ranges.
fn apply_text_edits(text: &str, edits: &[TextEdit]) -> Result<String, String> {
    let mut spans = Vec::with_capacity(edits.len());
    for e in edits {
        let start = position_to_byte_offset(text, e.range.start.line, e.range.start.character)
            .ok_or_else(|| {
                format!(
                    "edit start {}:{} is outside the file",
                    e.range.start.line, e.range.start.character
                )
            })?;
        let end = position_to_byte_offset(text, e.range.end.line, e.range.end.character)
            .ok_or_else(|| {
                format!(
                    "edit end {}:{} is outside the file",
                    e.range.end.line, e.range.end.character
                )
            })?;
        if end < start {
            return Err(format!("edit range {}:{} is inverted", start, end));
        }
        spans.push((start, end, e.new_text.clone()));
    }
    spans.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    let mut out = text.to_string();
    for (start, end, new) in spans {
        if end > out.len() {
            return Err("edit range exceeds file length".into());
        }
        out.replace_range(start..end, &new);
    }
    Ok(out)
}

/// Flatten a `WorkspaceEdit` into (uri, edits) pairs. Prefers
/// `documentChanges` when present (only its `TextDocumentEdit` form is
/// supported; file create/rename/delete operations are rejected).
fn collect_document_edits(
    edit: &serde_json::Value,
) -> Result<Vec<(String, Vec<TextEdit>)>, String> {
    let mut out: Vec<(String, Vec<TextEdit>)> = Vec::new();
    // Servers that populate both forms (or emit an empty documentChanges
    // alongside a populated legacy map) must not lose the legacy edits —
    // only a NON-EMPTY documentChanges wins the precedence.
    if let Some(changes) = edit
        .get("documentChanges")
        .and_then(|v| v.as_array())
        .filter(|a| !a.is_empty())
    {
        for change in changes {
            if let Some(kind) = change.get("kind").and_then(|k| k.as_str()) {
                return Err(format!(
                    "unsupported workspace edit change kind: {kind} (only text edits are applied)"
                ));
            }
            let uri = change
                .get("textDocument")
                .and_then(|d| d.get("uri"))
                .and_then(|u| u.as_str())
                .ok_or("documentChange missing textDocument.uri")?;
            let edits: Vec<TextEdit> = serde_json::from_value(
                change
                    .get("edits")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
            )
            .map_err(|e| format!("bad edits for {uri}: {e}"))?;
            out.push((uri.to_string(), edits));
        }
        return Ok(out);
    }
    if let Some(map) = edit.get("changes").and_then(|c| c.as_object()) {
        for (uri, edits) in map {
            let parsed: Vec<TextEdit> = serde_json::from_value(edits.clone())
                .map_err(|e| format!("bad edits for {uri}: {e}"))?;
            out.push((uri.clone(), parsed));
        }
    }
    Ok(out)
}

/// Percent-decode a `file://` URI body into a path (inverse of
/// `crate::lsp::file_uri`'s encoding).
fn uri_to_path(uri: &str) -> Option<std::path::PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // A non-empty authority (`file://server/share`) is not a local path
    // prefix — strip it or the hostname leaks into the decoded path.
    let rest = match rest.find('/') {
        Some(0) => rest,
        Some(slash) if rest[..slash].is_empty() => rest,
        _ => &rest[rest.find('/')?..],
    };
    let mut bytes = Vec::with_capacity(rest.len());
    let raw = rest.as_bytes();
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%' && i + 2 < raw.len() {
            let hex = |b: u8| (b as char).to_digit(16);
            if let (Some(h), Some(l)) = (hex(raw[i + 1]), hex(raw[i + 2])) {
                bytes.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        bytes.push(raw[i]);
        i += 1;
    }
    // vscode-style Windows URIs (`file:///C:/Users/…`) decode to a path
    // component of `/C:/Users/…`; the leading `/` before a drive letter is
    // URI syntax, not filesystem — drop it.
    if bytes.len() > 2 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b':' {
        bytes.remove(0);
    }
    Some(std::path::PathBuf::from(
        String::from_utf8_lossy(&bytes).to_string(),
    ))
}

/// Whether the edit carries no textual changes.
fn workspace_edit_is_empty(edit: &serde_json::Value) -> bool {
    if edit
        .get("documentChanges")
        .is_some_and(|v| v.as_array().is_some_and(|a| !a.is_empty()))
    {
        return false;
    }
    if edit
        .get("changes")
        .is_some_and(|v| v.as_object().is_some_and(|m| !m.is_empty()))
    {
        return false;
    }
    true
}

/// Apply a `WorkspaceEdit` to disk: read → apply edits → atomic write per
/// file. Sandbox-enforced (read-only rejected before this is reached;
/// workspace-write confines every target). Returns one summary line per file.
async fn apply_workspace_edit(
    edit: &serde_json::Value,
    ctx: &ToolRunContext,
) -> Result<String, ToolError> {
    let per_file = collect_document_edits(edit).map_err(|e| {
        tool_failure(
            "lsp",
            "LSP_ERROR",
            format!("unsupported workspace edit: {e}"),
        )
    })?;
    let mut summary = Vec::new();
    for (uri, edits) in per_file {
        let path = uri_to_path(&uri)
            .ok_or_else(|| tool_failure("lsp", "LSP_ERROR", format!("non-file uri: {uri}")))?;
        crate::sandbox::ensure_workspace_writable(
            &path,
            ctx.execution.sandbox_mode,
            ctx.execution.cwd.as_deref(),
        )
        .map_err(|e| tool_failure("lsp", "OUTSIDE_WORKSPACE", e.to_string()))?;
        let base = tokio::fs::read_to_string(&path).await.map_err(|_| {
            tool_failure(
                "lsp",
                "NOT_FOUND",
                format!("edit target file not found: {}", path.display()),
            )
        })?;
        let updated = apply_text_edits(&base, &edits).map_err(|e| {
            tool_failure(
                "lsp",
                "LSP_ERROR",
                format!("cannot apply edits to {}: {e}", path.display()),
            )
        })?;
        super::common::atomic_write(&path, &updated)
            .await
            .map_err(|e| tool_failure("lsp", "WRITE_FAILED", format!("write failed: {e}")))?;
        summary.push(format!("{}: {} edit(s)", path.display(), edits.len()));
    }
    let mut out = format!("applied edits to {} file(s)", summary.len());
    if !summary.is_empty() {
        out.push_str("\n");
        out.push_str(&summary.join("\n"));
    }
    Ok(out)
}

/// Short human names for the common `SymbolKind` values (fallback: the
/// debug form — lsp-types 0.97 makes the inner i32 private).
fn symbol_kind_name(kind: lsp_types::SymbolKind) -> String {
    use lsp_types::SymbolKind as K;
    let name = match kind {
        k if k == K::FILE => "file",
        k if k == K::MODULE => "module",
        k if k == K::NAMESPACE => "namespace",
        k if k == K::PACKAGE => "package",
        k if k == K::CLASS => "class",
        k if k == K::METHOD => "method",
        k if k == K::PROPERTY => "property",
        k if k == K::FIELD => "field",
        k if k == K::CONSTRUCTOR => "constructor",
        k if k == K::ENUM => "enum",
        k if k == K::INTERFACE => "interface",
        k if k == K::FUNCTION => "function",
        k if k == K::VARIABLE => "variable",
        k if k == K::CONSTANT => "constant",
        k if k == K::ENUM_MEMBER => "enum member",
        k if k == K::STRUCT => "struct",
        k if k == K::EVENT => "event",
        k if k == K::OPERATOR => "operator",
        k if k == K::TYPE_PARAMETER => "type parameter",
        _ => return format!("{kind:?}"),
    };
    name.to_string()
}

impl ToolExecutor for Lsp {
    fn execute(
        &self,
        arguments: Value,
        ctx: ToolRunContext,
    ) -> cordis_rslite::BoxFuture<Result<Value, ToolError>> {
        let manager = Arc::clone(&self.manager);
        Box::pin(async move {
            let json = json_args(&arguments)?;
            let op = json
                .get("operation")
                .and_then(|o| o.as_str())
                .ok_or_else(|| invalid_input("lsp", "missing operation".into()))?;
            let operation = Operation::parse(op)
                .ok_or_else(|| invalid_input("lsp", format!("unknown operation: {op}")))?;
            let raw = json
                .get("path")
                .and_then(|p| p.as_str())
                .ok_or_else(|| invalid_input("lsp", "missing path".into()))?;
            let path = resolve_path(ctx.execution.cwd.as_deref(), raw);
            let abs = tokio::fs::canonicalize(&path).await.map_err(|_| {
                tool_failure(
                    "lsp",
                    "NOT_FOUND",
                    format!("file not found: {}", path.display()),
                )
            })?;
            // Gate on the live registry: settings.yaml `lsp.servers` entries
            // extend/replace the builtins, so a builtin-only check would make
            // every configured custom language unreachable.
            if manager.spec_for_file(&abs).is_none() {
                return Err(tool_failure(
                    "lsp",
                    "UNSUPPORTED_FILE",
                    format!(
                        "unsupported file: {} (registered languages: rust, typescript/javascript, python, go, c/c++; extend via lsp.servers settings)",
                        abs.display()
                    ),
                ));
            }
            let uri = file_uri(&abs).as_str().to_string();
            let (client, _spec) = manager.session_for_file(&abs).await.map_err(|e| {
                tool_failure("lsp", "LSP_UNAVAILABLE", format!("lsp unavailable: {e}"))
            })?;
            let text = tokio::fs::read_to_string(&abs)
                .await
                .map_err(|e| tool_failure("lsp", "READ_FAILED", format!("read failed: {e}")))?;
            client.open_or_update(&uri, text).await.map_err(|e| {
                tool_failure("lsp", "LSP_ERROR", format!("open/update failed: {e}"))
            })?;
            let limit = (json
                .get("limit")
                .and_then(|l| l.as_u64())
                .unwrap_or(DEFAULT_MAX_LOCATIONS as u64) as usize)
                .clamp(1, DEFAULT_MAX_LOCATIONS);
            let in_u32 = |v: i64| (1..=u32::MAX as i64).contains(&v);
            let line = json
                .get("line")
                .and_then(|l| l.as_i64())
                .and_then(|l| in_u32(l).then_some(l as u32));
            let col = json
                .get("column")
                .and_then(|c| c.as_i64())
                .and_then(|c| in_u32(c).then_some(c as u32));
            let requires_pos = matches!(
                operation,
                Operation::GoToDefinition
                    | Operation::FindReferences
                    | Operation::GoToImplementation
                    | Operation::Hover
                    | Operation::Rename
                    | Operation::CodeAction
            );
            if requires_pos && (line.is_none() || col.is_none()) {
                return Err(invalid_input(
                    "lsp",
                    format!("operation {op} requires 1-based line and column"),
                ));
            }
            // Dynamic capability gate: operations the server never advertised
            // fail fast with a locator-style NOT_FOUND instead of a protocol
            // round trip that would error opaquely.
            if let Some(cap) = operation.required_capability() {
                if !client.supports(cap).await {
                    return Err(tool_failure(
                        "lsp",
                        "NOT_FOUND",
                        format!(
                            "this language server does not support {op} (capability `{cap}` not advertised at initialize)"
                        ),
                    ));
                }
            }
            let modifies_files = operation == Operation::Rename
                || (operation == Operation::CodeAction
                    && json.get("apply").and_then(|a| a.as_u64()).is_some());
            if modifies_files && ctx.execution.sandbox_mode == Some(SandboxMode::ReadOnly) {
                return Err(tool_failure(
                    "lsp",
                    "READONLY_MODE",
                    format!("operation {op} modifies files; not allowed in read-only sandbox"),
                ));
            }
            let output = match operation {
                Operation::GoToDefinition => {
                    let result = client
                        .definition(&uri, line.unwrap() - 1, col.unwrap() - 1)
                        .await
                        .map_err(|e| {
                            tool_failure("lsp", "LSP_ERROR", format!("definition: {e}"))
                        })?;
                    format_locations(result, limit)
                }
                Operation::FindReferences => {
                    let result = client
                        .references(&uri, line.unwrap() - 1, col.unwrap() - 1)
                        .await
                        .map_err(|e| {
                            tool_failure("lsp", "LSP_ERROR", format!("references: {e}"))
                        })?;
                    format_locations(result, limit)
                }
                Operation::GoToImplementation => {
                    let result = client
                        .implementation(&uri, line.unwrap() - 1, col.unwrap() - 1)
                        .await
                        .map_err(|e| {
                            tool_failure("lsp", "LSP_ERROR", format!("implementation: {e}"))
                        })?;
                    format_locations(result, limit)
                }
                Operation::Hover => {
                    let result = client
                        .hover(&uri, line.unwrap() - 1, col.unwrap() - 1)
                        .await
                        .map_err(|e| tool_failure("lsp", "LSP_ERROR", format!("hover: {e}")))?;
                    format_hover(result)
                }
                Operation::Symbols => {
                    let result = client
                        .symbols(&uri)
                        .await
                        .map_err(|e| tool_failure("lsp", "LSP_ERROR", format!("symbols: {e}")))?;
                    format_symbols(result, line, col)
                }
                Operation::Diagnostics => {
                    let diags = client.diagnostics(&uri, Duration::from_secs(3)).await;
                    format_diagnostics(diags)
                }
                Operation::Rename => {
                    let new_name = json
                        .get("newName")
                        .and_then(|n| n.as_str())
                        .ok_or_else(|| invalid_input("lsp", "rename requires newName".into()))?;
                    if new_name.trim().is_empty() {
                        return Err(invalid_input("lsp", "newName must be non-empty".into()));
                    }
                    let edit = client
                        .rename(&uri, line.unwrap() - 1, col.unwrap() - 1, new_name)
                        .await
                        .map_err(|e| tool_failure("lsp", "LSP_ERROR", format!("rename: {e}")))?;
                    if workspace_edit_is_empty(&edit) {
                        format!("rename to `{new_name}` produced no edits (no symbol at that position?)")
                    } else {
                        let applied = apply_workspace_edit(&edit, &ctx).await?;
                        format!("renamed to `{new_name}`\n{applied}")
                    }
                }
                Operation::CodeAction => {
                    let result = client
                        .code_actions(&uri, line.unwrap() - 1, col.unwrap() - 1)
                        .await
                        .map_err(|e| {
                            tool_failure("lsp", "LSP_ERROR", format!("codeAction: {e}"))
                        })?;
                    let actions = result.as_array().cloned().unwrap_or_default();
                    if actions.is_empty() {
                        "No code actions available at that position".to_string()
                    } else if let Some(index) = json.get("apply").and_then(|a| a.as_u64()) {
                        let index = index as usize;
                        let action = actions.get(index).ok_or_else(|| {
                            invalid_input(
                                "lsp",
                                format!(
                                    "apply index {index} out of range ({} actions)",
                                    actions.len()
                                ),
                            )
                        })?;
                        let Some(edit) = action.get("edit") else {
                            return Err(tool_failure(
                                "lsp",
                                "LSP_ERROR",
                                "selected action carries no edit (command-only actions are not supported)".into(),
                            ));
                        };
                        let title = action.get("title").and_then(|t| t.as_str()).unwrap_or("");
                        format!(
                            "applied code action: {title}\n{}",
                            apply_workspace_edit(edit, &ctx).await?
                        )
                    } else {
                        let total = actions.len();
                        let mut lines = Vec::new();
                        for (i, a) in actions.iter().enumerate().take(limit) {
                            let title = a
                                .get("title")
                                .and_then(|t| t.as_str())
                                .unwrap_or("(untitled)");
                            let kind = a.get("kind").and_then(|k| k.as_str()).unwrap_or("unknown");
                            let marker = if a.get("edit").is_some() {
                                ""
                            } else {
                                " (no edit)"
                            };
                            lines.push(format!("{i}: {title} [{kind}]{marker}"));
                        }
                        let mut out = lines.join("\n");
                        if total > limit {
                            out.push_str(&format!(
                                "\n[code actions truncated at {limit} of {total}]"
                            ));
                        }
                        out.push_str(&format!(
                            "\napply one with {{\"operation\":\"codeAction\",\"path\":…,\"line\":…,\"column\":…,\"apply\":<index>}}"
                        ));
                        out
                    }
                }
                Operation::WorkspaceSymbol => {
                    let query = json.get("query").and_then(|q| q.as_str()).ok_or_else(|| {
                        invalid_input("lsp", "workspaceSymbol requires query".into())
                    })?;
                    if query.trim().is_empty() {
                        return Err(invalid_input("lsp", "query must be non-empty".into()));
                    }
                    let result = client.workspace_symbols(query).await.map_err(|e| {
                        tool_failure("lsp", "LSP_ERROR", format!("workspaceSymbol: {e}"))
                    })?;
                    match serde_json::from_value::<Vec<lsp_types::SymbolInformation>>(
                        result.clone(),
                    ) {
                        Ok(items) => {
                            if items.is_empty() {
                                format!("No workspace symbols matching {query:?}")
                            } else {
                                let total = items.len();
                                let mut lines = Vec::new();
                                for s in items.iter().take(limit) {
                                    lines.push(format!(
                                        "{} {} {}:{}:{}",
                                        s.name,
                                        symbol_kind_name(s.kind),
                                        display_uri(s.location.uri.as_str()),
                                        s.location.range.start.line + 1,
                                        s.location.range.start.character + 1,
                                    ));
                                }
                                let mut out = lines.join("\n");
                                if total > limit {
                                    out.push_str(&format!(
                                        "\n[symbols truncated at {limit} of {total}]"
                                    ));
                                }
                                out
                            }
                        }
                        Err(_) => format!("Unexpected workspaceSymbol response: {result}"),
                    }
                }
            };
            Ok(Value::new(serde_json::json!({ "output": output })))
        })
    }
}

pub fn lsp_definition(manager: Arc<LspManager>) -> ToolDefinition {
    ToolDefinition {
        name: "lsp".into(),
        description: "Query or edit source through the language server (rust-analyzer for Rust; typescript-language-server, pyright, gopls, clangd when installed; configurable via lsp.servers settings). Parameters: operation (goToDefinition|findReferences|goToImplementation|hover|symbols|diagnostics|rename|codeAction|workspaceSymbol), path, line and column (1-based, required for position-based operations), limit (optional). rename additionally needs newName and rewrites the files the server's edit touches; codeAction optionally takes apply (index from the listed actions); workspaceSymbol needs query and searches the whole workspace. rename/codeAction/workspaceSymbol require the matching server capability, otherwise NOT_FOUND.".into(),
        parameters: Value::new(serde_json::json!({
            "type": "object",
            "properties": {
                "operation": { "type": "string", "enum": ["goToDefinition", "findReferences", "goToImplementation", "hover", "symbols", "diagnostics", "rename", "codeAction", "workspaceSymbol"] },
                "path": { "type": "string" },
                "line": { "type": "integer", "description": "1-based line" },
                "column": { "type": "integer", "description": "1-based UTF-16 column" },
                "limit": { "type": "integer", "description": "max locations (default 100)" },
                "newName": { "type": "string", "description": "new symbol name (rename)" },
                "query": { "type": "string", "description": "symbol query (workspaceSymbol)" },
                "apply": { "type": "integer", "description": "0-based index of the code action to apply (codeAction)" }
            },
            "required": ["operation", "path"]
        })),
        output: Value::new(serde_json::json!({
            "type": "object",
            "properties": { "output": { "type": "string" } }
        })),
        render: Arc::new(text_content),
        executor: Arc::new(Lsp::new(manager)),
        is_concurrency_safe: None,
        timeout: Some(TOOL_TIMEOUT),
        finalize_content: None,
        requires_approval: false,
        deny_in_readonly: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::{client::LspClient, transport};
    use serde_json::json;
    use starriver_kernel::tools::ToolExecution;

    fn manager() -> Arc<LspManager> {
        Arc::new(LspManager::new())
    }

    #[test]
    fn operation_parse_roundtrip() {
        for (s, op) in [
            ("goToDefinition", Operation::GoToDefinition),
            ("findReferences", Operation::FindReferences),
            ("goToImplementation", Operation::GoToImplementation),
            ("hover", Operation::Hover),
            ("symbols", Operation::Symbols),
            ("diagnostics", Operation::Diagnostics),
            ("rename", Operation::Rename),
            ("codeAction", Operation::CodeAction),
            ("workspaceSymbol", Operation::WorkspaceSymbol),
        ] {
            assert_eq!(Operation::parse(s), Some(op));
        }
        assert_eq!(Operation::parse("format"), None);
        assert_eq!(
            Operation::Rename.required_capability(),
            Some("renameProvider")
        );
        assert_eq!(Operation::Hover.required_capability(), None);
    }

    #[test]
    fn lsp_definition_metadata() {
        let def = lsp_definition(manager());
        assert_eq!(def.name, "lsp");
        assert!(!def.requires_approval);
        assert!(!def.deny_in_readonly);
        assert!(def.timeout.is_some());
    }

    #[test]
    fn format_symbols_nested_outline() {
        let syms = json!([{
            "name": "Point", "kind": 5,
            "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 5, "character": 0 } },
            "selectionRange": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 5 } },
            "children": [{
                "name": "new", "kind": 12,
                "range": { "start": { "line": 3, "character": 4 }, "end": { "line": 3, "character": 20 } },
                "selectionRange": { "start": { "line": 3, "character": 8 }, "end": { "line": 3, "character": 11 } }
            }]
        }]);
        let out = format_symbols(syms, None, None);
        assert!(out.starts_with("Point (1:1-6:1)"), "got: {out}");
        assert!(out.contains("  new (4:5-4:21)"), "got: {out}");
    }

    #[test]
    fn format_symbols_position_returns_path() {
        let syms = json!([{
            "name": "main", "kind": 12,
            "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 4, "character": 0 } },
            "selectionRange": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 4 } },
            "children": [{
                "name": "helper", "kind": 12,
                "range": { "start": { "line": 2, "character": 4 }, "end": { "line": 3, "character": 4 } },
                "selectionRange": { "start": { "line": 2, "character": 8 }, "end": { "line": 2, "character": 14 } }
            }]
        }]);
        assert_eq!(
            format_symbols(syms.clone(), Some(3), Some(5)),
            "Symbol at 3:5 is `main::helper`"
        );
        assert_eq!(
            format_symbols(syms.clone(), Some(1), Some(2)),
            "Symbol at 1:2 is `main`"
        );
        assert_eq!(format_symbols(syms, Some(9), Some(1)), "No symbol at 9:1");
    }

    #[test]
    fn format_symbols_flat() {
        let flat = json!([{ "name": "main", "kind": 12,
            "location": { "uri": "file:///x.rs", "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 1, "character": 0 } } } }]);
        assert_eq!(format_symbols(flat, None, None), "main @ 1:1");
    }

    #[test]
    fn format_diagnostics_lists_severity_and_range() {
        let diags = vec![lsp_types::Diagnostic {
            range: Range {
                start: lsp_types::Position {
                    line: 10,
                    character: 17,
                },
                end: lsp_types::Position {
                    line: 10,
                    character: 31,
                },
            },
            severity: Some(lsp_types::DiagnosticSeverity::ERROR),
            code: None,
            code_description: None,
            source: Some("rustc".into()),
            message: "no method named `missing_method` found".into(),
            related_information: None,
            tags: None,
            data: None,
        }];
        assert_eq!(
            format_diagnostics(diags),
            "error 11:18–11:32 no method named `missing_method` found"
        );
    }

    #[test]
    fn format_locations_dedupes_and_caps() {
        let locations = json!([
            { "uri": "file:///a.rs", "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 4 } } },
            { "uri": "file:///a.rs", "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 4 } } },
            { "uri": "file:///b.rs", "range": { "start": { "line": 2, "character": 3 }, "end": { "line": 2, "character": 8 } } },
        ]);
        let out = format_locations(locations, 1);
        assert!(out.contains("/a.rs:1:1"));
        assert!(!out.contains("/b.rs"));
        assert!(out.contains("truncated"));
    }

    #[test]
    fn format_hover_markup_and_scalar() {
        let markup = json!({ "contents": { "kind": "markdown", "value": "fn main()" } });
        assert_eq!(format_hover(markup), "fn main()");
        let scalar = json!({ "contents": "plain hover" });
        assert_eq!(format_hover(scalar), "plain hover");
        assert_eq!(format_hover(json!(null)), "No hover content");
    }

    // ── dynamic-capability helpers (pure) ──────────────────────────────────

    #[test]
    fn position_to_byte_offset_handles_utf16() {
        // Emoji is 4 UTF-8 bytes but 2 UTF-16 units: line 0
        // `let e = "😀";` — the emoji starts at UTF-16 col 9 and byte 9.
        let text = "let e = \"\u{1f600}\";\nfn_old();\n";
        assert_eq!(position_to_byte_offset(text, 0, 0), Some(0));
        assert_eq!(position_to_byte_offset(text, 0, 9), Some(9));
        // col 11 is past the emoji (2 units): byte 13 (after the 4-byte emoji).
        assert_eq!(position_to_byte_offset(text, 0, 11), Some(13));
        assert_eq!(
            position_to_byte_offset(text, 1, 0),
            Some(text.find("fn_old").unwrap())
        );
        // The line's UTF-16 length is 13 (9 ascii + 2 emoji units + `";`):
        // col 12 is the `;` at byte 14; col 13 is the line end; columns at
        // or past the line's UTF-16 length clamp to byte 15 (before `\n`).
        assert_eq!(position_to_byte_offset(text, 0, 12), Some(14));
        assert_eq!(position_to_byte_offset(text, 0, 13), Some(15));
        assert_eq!(position_to_byte_offset(text, 0, 14), Some(15));
        assert_eq!(position_to_byte_offset(text, 9, 0), None);
    }

    fn edit(line0: u32, c0: u32, line1: u32, c1: u32, new_text: &str) -> TextEdit {
        TextEdit {
            range: Range {
                start: lsp_types::Position {
                    line: line0,
                    character: c0,
                },
                end: lsp_types::Position {
                    line: line1,
                    character: c1,
                },
            },
            new_text: new_text.into(),
        }
    }

    #[test]
    fn apply_text_edits_replaces_multibyte_and_keeps_offsets() {
        let text = "let e = \"\u{1f600}\";\nfn_old();\n";
        // Replace the emoji (2 UTF-16 units) with xx.
        let out = apply_text_edits(text, &[edit(0, 9, 0, 11, "xx")]).unwrap();
        assert_eq!(out, "let e = \"xx\";\nfn_old();\n");
        // Two edits on one line apply without offset drift (reverse order).
        let text = "aa bb cc\n";
        let out =
            apply_text_edits(text, &[edit(0, 0, 0, 2, "AA"), edit(0, 6, 0, 8, "CC")]).unwrap();
        assert_eq!(out, "AA bb CC\n");
        assert!(apply_text_edits(text, &[edit(0, 0, 9, 0, "x")]).is_err());
        assert!(apply_text_edits("x\n", &[edit(5, 0, 5, 1, "x")]).is_err());
    }

    #[test]
    fn collect_document_edits_forms_and_rejections() {
        let uri = "file:///w/a.rs";
        let both = json!({
            "documentChanges": [{ "textDocument": { "uri": uri, "version": 1 },
                                  "edits": [{ "range": { "start": { "line": 0, "character": 0 },
                                                          "end": { "line": 0, "character": 1 } },
                                              "newText": "z" }] }],
            "changes": { "file:///w/ignored.rs": [] },
        });
        let collected = collect_document_edits(&both).unwrap();
        assert_eq!(collected.len(), 1);
        assert_eq!(collected[0].0, uri);
        assert_eq!(collected[0].1.len(), 1);

        let legacy = json!({ "changes": { uri: [{ "range": { "start": { "line": 0, "character": 0 },
                                                              "end": { "line": 0, "character": 1 } },
                                                 "newText": "z" }] } });
        assert_eq!(collect_document_edits(&legacy).unwrap().len(), 1);

        let file_op = json!({ "documentChanges": [{ "kind": "rename", "oldUri": "file:///a", "newUri": "file:///b" }] });
        assert!(collect_document_edits(&file_op)
            .unwrap_err()
            .contains("rename"));

        assert!(workspace_edit_is_empty(&json!({ "changes": {} })));
        assert!(!workspace_edit_is_empty(&legacy));
    }

    #[test]
    fn uri_to_path_is_file_uri_inverse() {
        let original = std::path::PathBuf::from("/tmp/my dir/caf\u{e9} file.rs");
        let uri = file_uri(&original);
        assert_eq!(uri_to_path(uri.as_str()).unwrap(), original);
        assert_eq!(
            uri_to_path("file:///plain.rs").unwrap(),
            std::path::PathBuf::from("/plain.rs")
        );
        assert!(uri_to_path("https://example.com/x").is_none());
    }

    // ── full pipeline against a scripted fake server (mock transport) ──────

    struct FakeServer {
        caps: serde_json::Value,
        rename_result: serde_json::Value,
        code_action_result: serde_json::Value,
        workspace_symbol_result: serde_json::Value,
    }

    impl Default for FakeServer {
        fn default() -> Self {
            Self {
                caps: json!({
                    "renameProvider": true,
                    "codeActionProvider": true,
                    "workspaceSymbolProvider": true,
                }),
                rename_result: json!({ "changes": {} }),
                code_action_result: json!([]),
                workspace_symbol_result: json!([]),
            }
        }
    }

    /// A manager whose ("rust", root) session is a client over in-memory
    /// duplex pipes driven by `fake` — no real language server is spawned.
    async fn manager_with_fake(root: &std::path::Path, fake: FakeServer) -> Arc<LspManager> {
        let (to_srv, from_client) = tokio::io::duplex(1 << 16);
        let (to_client, from_srv) = tokio::io::duplex(1 << 16);
        tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(from_client);
            let mut writer = to_client;
            loop {
                let bytes = match transport::read_frame(&mut reader).await {
                    Ok(b) => b,
                    Err(_) => break,
                };
                let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                let method = v
                    .get("method")
                    .and_then(|m| m.as_str())
                    .unwrap_or_default()
                    .to_string();
                let id = v.get("id").cloned();
                macro_rules! respond {
                    ($result:expr) => {
                        if let Some(id) = &id {
                            transport::write_frame(
                                &mut writer,
                                &json!({ "jsonrpc": "2.0", "id": id, "result": $result }),
                            )
                            .await
                            .unwrap();
                        }
                    };
                }
                match method.as_str() {
                    "initialize" => respond!(json!({ "capabilities": fake.caps })),
                    "textDocument/rename" => respond!(fake.rename_result.clone()),
                    "textDocument/codeAction" => respond!(fake.code_action_result.clone()),
                    "workspace/symbol" => respond!(fake.workspace_symbol_result.clone()),
                    "exit" => break,
                    _ => respond!(serde_json::Value::Null),
                }
            }
        });
        let client = LspClient::spawn(root, "rust", Box::new(to_srv), Box::new(from_srv), None)
            .await
            .expect("fake handshake");
        let manager = Arc::new(LspManager::new());
        manager.register_session("rust", root, client).await;
        manager
    }

    /// A cargo workspace with one source file: root/Cargo.toml + src/main.rs.
    fn workspace_with(content: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        let file = tmp.path().join("src/main.rs");
        std::fs::write(&file, content).unwrap();
        (tmp, file)
    }

    async fn run_with(
        manager: Arc<LspManager>,
        args: serde_json::Value,
        cwd: std::path::PathBuf,
        sandbox_mode: Option<SandboxMode>,
    ) -> Result<Value, ToolError> {
        let execution = ToolExecution {
            call_id: starriver_kernel::llm::CallId("c1".into()),
            root_call_id: starriver_kernel::llm::CallId("c1".into()),
            name: "lsp".into(),
            arguments: Value::new(args.clone()),
            agent_id: None,
            cwd: Some(cwd),
            sandbox_mode,
            parent: None,
            token: starriver_kernel::tools::mint_tool_execution_token(),
            cancellation: None,
        };
        let ctx = ToolRunContext::new(execution);
        lsp_definition(manager)
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

    #[tokio::test]
    async fn rename_applies_workspace_edit_to_disk() {
        let content = "fn old_name() {}\nfn main() { old_name(); }\n";
        let (tmp, file) = workspace_with(content);
        let root = tmp.path().canonicalize().unwrap();
        let uri = file_uri(&file).as_str().to_string();
        let mut fake = FakeServer::default();
        fake.rename_result = json!({
            "documentChanges": [{
                "textDocument": { "uri": uri, "version": null },
                "edits": [
                    { "range": { "start": { "line": 0, "character": 3 },
                                  "end": { "line": 0, "character": 11 } },
                      "newText": "new_name" },
                    { "range": { "start": { "line": 1, "character": 12 },
                                  "end": { "line": 1, "character": 20 } },
                      "newText": "new_name" },
                ],
            }]
        });
        let manager = manager_with_fake(&root, fake).await;
        let result = run_with(
            manager,
            json!({ "operation": "rename", "path": "src/main.rs", "line": 1, "column": 4,
                     "newName": "new_name" }),
            root.clone(),
            None,
        )
        .await
        .unwrap();
        let out = out_of(&result);
        assert!(out.contains("renamed to `new_name`"), "{out}");
        assert!(out.contains("2 edit(s)"), "{out}");
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "fn new_name() {}\nfn main() { new_name(); }\n"
        );
    }

    #[tokio::test]
    async fn rename_without_capability_is_not_found() {
        let (tmp, file) = workspace_with("fn a() {}\n");
        let root = tmp.path().canonicalize().unwrap();
        let mut fake = FakeServer::default();
        fake.caps = json!({ "documentSymbolProvider": true });
        let manager = manager_with_fake(&root, fake).await;
        let err = run_with(
            manager,
            json!({ "operation": "rename", "path": "src/main.rs", "line": 1, "column": 4,
                     "newName": "b" }),
            root,
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(err.code(), "NOT_FOUND");
        assert!(err.to_string().contains("renameProvider"), "{err}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "fn a() {}\n");
    }

    #[tokio::test]
    async fn rename_in_readonly_sandbox_rejected() {
        let (tmp, file) = workspace_with("fn a() {}\n");
        let root = tmp.path().canonicalize().unwrap();
        let manager = manager_with_fake(&root, FakeServer::default()).await;
        let err = run_with(
            manager,
            json!({ "operation": "rename", "path": "src/main.rs", "line": 1, "column": 4,
                     "newName": "b" }),
            root,
            Some(SandboxMode::ReadOnly),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code(), "READONLY_MODE");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "fn a() {}\n");
    }

    #[tokio::test]
    async fn rename_targeting_outside_workspace_rejected() {
        let (tmp, _file) = workspace_with("fn a() {}\n");
        let root = tmp.path().canonicalize().unwrap();
        let mut fake = FakeServer::default();
        fake.rename_result = json!({
            "changes": { "file:///etc/hostname": [{
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 3 } },
                "newText": "evil" }] }
        });
        let manager = manager_with_fake(&root, fake).await;
        let err = run_with(
            manager,
            json!({ "operation": "rename", "path": "src/main.rs", "line": 1, "column": 4,
                     "newName": "b" }),
            root,
            Some(SandboxMode::WorkspaceWrite),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code(), "OUTSIDE_WORKSPACE");
    }

    #[tokio::test]
    async fn code_action_lists_then_applies() {
        let (tmp, file) = workspace_with("fn main() { todo!() }\n");
        let root = tmp.path().canonicalize().unwrap();
        let uri = file_uri(&file).as_str().to_string();
        let mut fake = FakeServer::default();
        fake.code_action_result = json!([{
            "title": "Replace todo! with unimplemented!",
            "kind": "quickfix",
            "edit": { "changes": { uri.clone(): [{
                "range": { "start": { "line": 0, "character": 12 }, "end": { "line": 0, "character": 19 } },
                "newText": "unimplemented!()" }] } },
        }]);
        let manager = manager_with_fake(&root, fake).await;
        let listed = run_with(
            manager.clone(),
            json!({ "operation": "codeAction", "path": "src/main.rs", "line": 1, "column": 13 }),
            root.clone(),
            None,
        )
        .await
        .unwrap();
        let out = out_of(&listed);
        assert!(
            out.contains("0: Replace todo! with unimplemented! [quickfix]"),
            "{out}"
        );
        assert!(out.contains("\"apply\""), "{out}");

        let applied = run_with(
            manager,
            json!({ "operation": "codeAction", "path": "src/main.rs", "line": 1, "column": 13,
                     "apply": 0 }),
            root,
            None,
        )
        .await
        .unwrap();
        let out = out_of(&applied);
        assert!(out.contains("applied code action"), "{out}");
        assert!(out.contains("1 edit(s)"), "{out}");
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "fn main() { unimplemented!() }\n"
        );
    }

    #[tokio::test]
    async fn code_action_without_capability_is_not_found() {
        let (tmp, _file) = workspace_with("fn a() {}\n");
        let root = tmp.path().canonicalize().unwrap();
        let mut fake = FakeServer::default();
        fake.caps = json!({});
        let manager = manager_with_fake(&root, fake).await;
        let err = run_with(
            manager,
            json!({ "operation": "codeAction", "path": "src/main.rs", "line": 1, "column": 1 }),
            root,
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(err.code(), "NOT_FOUND");
    }

    #[tokio::test]
    async fn workspace_symbol_lists_and_validates_query() {
        let (tmp, _file) = workspace_with("fn old_name() {}\n");
        let root = tmp.path().canonicalize().unwrap();
        let uri = file_uri(&tmp.path().join("src/main.rs"))
            .as_str()
            .to_string();
        let mut fake = FakeServer::default();
        fake.workspace_symbol_result = json!([{
            "name": "old_name", "kind": 12,
            "location": { "uri": uri, "range": { "start": { "line": 0, "character": 3 },
                                                   "end": { "line": 0, "character": 11 } } },
        }]);
        let manager = manager_with_fake(&root, fake).await;
        let result = run_with(
            manager.clone(),
            json!({ "operation": "workspaceSymbol", "path": "src/main.rs", "query": "old" }),
            root.clone(),
            None,
        )
        .await
        .unwrap();
        let out = out_of(&result);
        // The symbol line renders the (canonical, absolute) file path with
        // 1-based coordinates.
        assert!(out.contains("old_name function "), "{out}");
        assert!(out.ends_with("src/main.rs:1:4"), "{out}");

        let err = run_with(
            manager,
            json!({ "operation": "workspaceSymbol", "path": "src/main.rs" }),
            root,
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(err.code(), "INVALID_INPUT");
    }

    #[tokio::test]
    async fn workspace_symbol_without_capability_is_not_found() {
        let (tmp, _file) = workspace_with("fn a() {}\n");
        let root = tmp.path().canonicalize().unwrap();
        let mut fake = FakeServer::default();
        fake.caps = json!({ "renameProvider": true });
        let manager = manager_with_fake(&root, fake).await;
        let err = run_with(
            manager,
            json!({ "operation": "workspaceSymbol", "path": "src/main.rs", "query": "a" }),
            root,
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(err.code(), "NOT_FOUND");
    }
}
