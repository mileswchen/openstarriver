import React, { useState, useRef, useEffect, useCallback, useMemo, useContext, createContext, memo } from 'react';
import { createRoot } from 'react-dom/client';
import { I18nProvider, useI18n } from './i18n';
import ReactMarkdown, { defaultUrlTransform } from 'react-markdown';
import remarkGfm from 'remark-gfm';
import rehypeHighlight from 'rehype-highlight';
import {
  fileResultLang, highlightCodeHtml, highlightCodeLines, langFromLabel,
} from './codeLang';
import { DiffView, isDiffContent, parseDiff } from './diffview';
import {
  getShellReplacement,
  getTab,
  listTabs,
  registerTab,
  type TabId,
} from './contrib/registry';
import { applyTheme } from './contrib/theme';
import {
  applyCodeTokens, currentTokenValue, resetCodeTokens, setCodeToken,
  DEFAULT_LIGHT,
  type CodeTokenKey,
} from './contrib/codePalette';
import { syncPluginTabs } from './contrib/pluginHost';
import { registerConversationNode, renderToolNode } from './contrib/conversationNodes';
import { CompactionRow } from './contrib/CompactionRow';
import {
  DEFAULT_UI_LOCK_SCHEDULE, LOCK_TICK_MS,
  evaluateUiLockTick, generateLockCode, isValidTime,
  loadUiLockSchedule, saveUiLockSchedule,
  type UiLockSchedule,
} from './uiLock';
import './themes';

/* ─── types ─── */

interface Message {
  id: string;
  role: 'user' | 'assistant' | 'tool' | 'system' | 'thinking' | 'compaction';
  content: string;
  streaming?: boolean;
  toolName?: string;
  durationMsTool?: number;
  seq?: number;
  isSubagent?: boolean;
  /** Wall-clock creation time (ms epoch); shown beside user/assistant bubbles. */
  ts?: number;
  /** Turn completion time (ms epoch), stamped on the assistant bubble at turnEnd. */
  doneTs?: number;
  /** Turn token totals (history projection; live turns use turnUsage). */
  usage?: { inputTokens: number; outputTokens: number };
  /** Turn duration (ms) from history projection (first-event→turn/end). */
  durationMs?: number;
  /** Files changed by the turn's edit/write/patch tools (history projection;
   *  live turns use the turnSummary accumulator). */
  fileChanges?: { path: string; add: number; del: number; diff: string }[];
  /** Live tool execution output + outcome, filled by ToolCallResult (Coding). */
  result?: string;
  ok?: boolean | null;
  /** Opaque tool presentation metadata (backlog #6): tool/result `meta`
      payload for client-side card narrowing. */
  meta?: unknown;
  /** WI-86: tool-call wall time (ms) — recorded at toolCall, shown at
   *  toolResult. */
  durationMsTool?: number;
  /** WI-88: timeline seq (history-loaded user messages only) — the fork
   *  point for "resend from here". */
  seq?: number;
  /** WI-91: this tool row is a subagent spawn (renders nested). */
  isSubagent?: boolean;
}

interface Attachment {
  id: string;
  name: string;
  kind: 'image' | 'document';
  dataUrl: string;      // base64 data URL (images + binary documents)
  text?: string;        // extracted plain text (text documents)
  ref?: string;         // durable attachment ref (sha256:...) once admitted
  path?: string;        // temp-file path (binary documents) once saved
  state: 'pending' | 'saved' | 'error';
}

const IMAGE_MIME_TYPES = ['image/png', 'image/jpeg', 'image/gif', 'image/bmp', 'image/webp', 'image/tiff'];
const IMAGE_EXTENSIONS = ['png', 'jpg', 'jpeg', 'gif', 'bmp', 'webp', 'tiff'];
const TEXT_EXTENSIONS = ['txt', 'md', 'markdown', 'csv', 'json', 'yaml', 'yml', 'log'];

// WI-180: staged composer attachments outlive CodingView — switching tabs
// unmounts the composer, and component state died with it (pasted images
// vanished on every tab round-trip). Module singleton; CodingView adopts it
// on mount and mirrors its state back (see the attachments state there).
const stagedAttachmentsStash: Attachment[] = [];

// Backend-canonical coding agent name (host CODING_AGENT_NAME). The desktop
// UI must query/switch/bind by this exact name or the model dropdown stays
// empty and the Coding view can never activate an agent.
const CODING_AGENT_NAME = 'Starriver Code';
// The built-in chat persona (host: lib.rs CHAT_AGENT_NAME) — New Task's chat
// mode runs it with no workspace / execution-mode / approval chrome.
const CHAT_AGENT_NAME = 'Chat Agent';

function fileToDataUrl(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(reader.result as string);
    reader.onerror = reject;
    reader.readAsDataURL(file);
  });
}

function fileToText(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(reader.result as string);
    reader.onerror = reject;
    reader.readAsText(file);
  });
}

function base64FromDataUrl(dataUrl: string): string {
  // indexOf, not split+||: an empty payload (`data:...;base64,`) must yield
  // '' (rejected upstream as an empty file), not the whole data URL.
  const i = dataUrl.indexOf(',');
  return i === -1 ? dataUrl : dataUrl.slice(i + 1);
}

/** Infer a raster media type from a file name (backend vocabulary: png/jpeg/webp). */
function imageMimeFromName(name: string): string {
  const ext = (name.split('.').pop() || '').toLowerCase();
  if (ext === 'png') return 'image/png';
  if (ext === 'webp') return 'image/webp';
  return 'image/jpeg';
}

/**
 * Persist pending attachments and build task references:
 * - images → admitted through the backend attachment seam → `![name](sha256:…)`
 * - binary documents → temp-file path links
 * - text documents → extracted text injection
 */
export async function saveAttachments(attachments: Attachment[]): Promise<{
  saved: Attachment[];
  imageRefs: string;
  docLinks: string;
  docTexts: string;
}> {
  const saved: Attachment[] = [];
  const refs: string[] = [];
  const links: string[] = [];
  const texts: string[] = [];
  const pendingImages: Attachment[] = [];

  for (const att of attachments) {
    if (att.kind === 'document' && att.text) {
      texts.push(`--- Document: ${att.name} ---\n${att.text}`);
      saved.push({ ...att, state: 'saved' as const });
      continue;
    }
    if (att.state === 'saved' && att.ref) {
      // Re-send path: an already-admitted image keeps its durable ref.
      saved.push(att);
      refs.push(`![${att.name}](${att.ref})`);
      continue;
    }
    if (att.state === 'pending' && att.kind === 'image') {
      pendingImages.push(att);
      continue;
    }
    if (att.state === 'pending') {
      try {
        const b64 = base64FromDataUrl(att.dataUrl);
        const path = await window.starriver.saveTempFile(att.name, b64);
        saved.push({ ...att, state: 'saved' as const, path });
        links.push(`[${att.name}](${path})`);
      } catch {
        saved.push({ ...att, state: 'error' as const });
      }
      continue;
    }
    // A saved binary document (no ref, no text) must re-emit its temp-file
    // link on resend — the chip shows but the model received no reference.
    if (att.state === 'saved' && att.path) {
      links.push(`[${att.name}](${att.path})`);
      continue;
    }
    saved.push(att);
  }

  if (pendingImages.length > 0) {
    const imagesJson = JSON.stringify(
      pendingImages.map(a => ({
        name: a.name,
        // The dataUrl prefix carries the sniffed truth — deriving from the file
      // NAME mismatches the bytes (a PNG saved as .jpg → ImageTypeMismatch).
      mediaType: (/^data:(image\/(?:png|jpeg|webp));/.exec(a.dataUrl)?.[1]) ?? imageMimeFromName(a.name),
        data: base64FromDataUrl(a.dataUrl),
      })),
    );
    try {
      const admitted = await window.starriver.admitImageAttachments(imagesJson);
      for (let i = 0; i < pendingImages.length; i++) {
        const att = pendingImages[i];
        const ref = admitted[i];
        if (ref?.id) {
          saved.push({ ...att, state: 'saved' as const, ref: ref.id });
          refs.push(`![${att.name}](${ref.id})`);
        } else {
          saved.push({ ...att, state: 'error' as const });
        }
      }
    } catch {
      for (const att of pendingImages) {
        saved.push({ ...att, state: 'error' as const });
      }
    }
  }

  return {
    saved,
    imageRefs: refs.join('\n'),
    docLinks: links.join('\n'),
    docTexts: texts.join('\n\n'),
  };
}

/**
 * Display text for a queued row: markdown image refs summarize into a
 * readable marker instead of raw `![alt](sha256:…)` noise. Display only —
 * the row editor keeps the raw text so edits round-trip the refs.
 */
export function queueDisplayText(text: string, t: (key: string, vars?: Record<string, string | number>) => string): string {
  return text.replace(/!\[([^\]]*)\]\(([^)]+)\)/g, (_match, alt: string) => {
    const name = alt.trim();
    return t('coding.queuedImage', { name: name || t('coding.queuedImageUntitled') });
  });
}

/**
 * Compose the outgoing task text from the typed text and the saved
 * attachment references (image refs first, then document links, then
 * extracted text — the shape UserMessageContent renders as thumbnails).
 * Single source of truth for BOTH the direct send and the queued path, so a
 * queued message reaches the model with exactly the same attachment payload.
 */
export function composeWithAttachments(
  text: string,
  refs: { imageRefs: string; docLinks: string; docTexts: string },
): string {
  const attachmentText = [refs.imageRefs, refs.docLinks, refs.docTexts].filter(Boolean).join('\n\n');
  if (!attachmentText) return text;
  return text ? `${text}\n${attachmentText}` : attachmentText;
}

/** Classify an uploaded file as an image vs a document (by mime or extension). */
export function detectAttachmentKind(name: string, mime: string): 'image' | 'document' {
  const ext = (name.split('.').pop() || '').toLowerCase();
  if (mime.startsWith('image/') || IMAGE_EXTENSIONS.includes(ext)) return 'image';
  return 'document';
}

/** Plain-text documents are extracted client-side (zero deps); binary docs attach by path. */
export function isTextDocument(name: string, mime: string): boolean {
  const ext = (name.split('.').pop() || '').toLowerCase();
  return mime.startsWith('text/') || TEXT_EXTENSIONS.includes(ext) || mime === 'application/json';
}

async function processFile(file: File): Promise<Attachment> {
  const id = crypto.randomUUID();
  const name = file.name || `file_${Date.now()}`;
  if (detectAttachmentKind(name, file.type) === 'image') {
    const dataUrl = await fileToDataUrl(file);
    return { id, name, kind: 'image', dataUrl, state: 'pending' };
  }
  if (isTextDocument(name, file.type)) {
    const text = await fileToText(file);
    return { id, name, kind: 'document', dataUrl: '', text, state: 'pending' };
  }
  const dataUrl = await fileToDataUrl(file);
  return { id, name, kind: 'document', dataUrl, state: 'pending' };
}

/* ─── AttachmentStrip ─── */

/* ─── UserMessageContent (WI-1 P1) ─── */
// History images: user-message markdown image refs (pre-P0 logs carry them
// inline in the text; post-P0 the projection rebuilds them from Image
// blocks) render as thumbnails. `sha256:` refs resolve lazily through
// attachments.read and cache per file id; `data:` URLs render directly.

const imageDataUrlCache = new Map<string, string>();

export function parseImageRefs(text: string): Array<string | { alt: string; ref: string }> {
  const parts: Array<string | { alt: string; ref: string }> = [];
  const re = /!\[([^\]]*)\]\((sha256:[^)\s]+|data:image\/[^)\s]+)\)/g;
  let last = 0;
  let m: RegExpExecArray | null;
  while ((m = re.exec(text))) {
    if (m.index > last) parts.push(text.slice(last, m.index));
    parts.push({ alt: m[1], ref: m[2] });
    last = m.index + m[0].length;
  }
  if (last < text.length) parts.push(text.slice(last));
  return parts;
}

export function UserImage({ fileRef, alt }: { fileRef: string; alt: string }) {
  const initial = fileRef.startsWith('data:') ? fileRef : imageDataUrlCache.get(fileRef) ?? null;
  // BUG-92: react-markdown reuses this component instance when the src at
  // the same position changes — re-derive when fileRef moves (the standard
  // adjust-state-during-render pattern) instead of showing the old image or
  // a stale failure placeholder.
  const [state, setState] = useState<{ ref: string; url: string | null; failed: boolean }>({
    ref: fileRef, url: initial, failed: false,
  });
  if (state.ref !== fileRef) {
    const next = fileRef.startsWith('data:') ? fileRef : imageDataUrlCache.get(fileRef) ?? null;
    setState({ ref: fileRef, url: next, failed: false });
  }
  const { url, failed } = state;
  useEffect(() => {
    if (url) return;
    let cancelled = false;
    Promise.resolve(window.starriver.readImageAttachment?.(fileRef))
      .then((a) => {
        if (cancelled || !a?.base64) return;
        const dataUrl = `data:${a.mediaType || 'image/png'};base64,${a.base64}`;
        imageDataUrlCache.set(fileRef, dataUrl);
        setState((s) => (s.ref === fileRef ? { ...s, url: dataUrl } : s));
      })
      .catch(() => {
        if (!cancelled) setState((s) => (s.ref === fileRef ? { ...s, failed: true } : s));
      });
    return () => {
      cancelled = true;
    };
  }, [fileRef, url]);
  if (url) {
    return (
      <img
        src={url}
        alt={alt}
        loading="lazy"
        style={{ maxWidth: 280, maxHeight: 200, borderRadius: 6, display: 'block', marginTop: 4 }}
      />
    );
  }
  return (
    <span style={{ display: 'inline-flex', alignItems: 'center', gap: 6, color: 'var(--text-secondary)', fontSize: 13, padding: '2px 0' }}>
      🖼 {failed ? alt || fileRef : alt || '…'}
    </span>
  );
}

export function UserMessageContent({ text }: { text: string }) {
  const parts = parseImageRefs(text);
  if (!parts.some((p) => typeof p !== 'string')) return <>{text}</>;
  // Thumbnails stack ABOVE the typed text (composer send order: refs first,
  // message second); interleaved text between images is preserved after them.
  const images = parts.filter((p): p is { alt: string; ref: string } => typeof p !== 'string');
  const textAfter = parts
    .filter((p): p is string => typeof p === 'string')
    .join(' ')
    .trim();
  return (
    <>
      <div className="user-msg-images">
        {images.map((p, i) => (
          <UserImage key={`${p.ref}-${i}`} fileRef={p.ref} alt={p.alt} />
        ))}
      </div>
      {textAfter && <span>{textAfter}</span>}
    </>
  );
}

/* ─── FeedbackRow (WI-48 feedback slice) ─── */

export function FeedbackRow({
  onRecord,
}: {
  onRecord: (kind: 'up' | 'down', comment?: string) => void;
}) {
  const { t } = useI18n();
  const [picked, setPicked] = useState<'up' | 'down' | null>(null);
  const [sent, setSent] = useState(false);
  const [comment, setComment] = useState('');
  if (sent) {
    return (
      <div className="feedback-row" style={{ fontSize: 12, color: 'var(--text-muted)', marginTop: 4 }}>
        {t('feedback.thanks')}
      </div>
    );
  }
  const record = (kind: 'up' | 'down') => {
    onRecord(kind, comment.trim() || undefined);
    setPicked(kind);
    setSent(true);
  };
  return (
    <div className="feedback-row" style={{ fontSize: 12, color: 'var(--text-muted)', marginTop: 4 }}>
      {picked === null ? (
        <>
          <span>{t('feedback.title')}</span>
          <button
            type="button"
            aria-label={t('feedback.up')}
            title={t('feedback.up')}
            onClick={() => record('up')}
            style={{ border: 'none', background: 'transparent', cursor: 'pointer', fontSize: 12 }}
          >
            👍
          </button>
          <button
            type="button"
            aria-label={t('feedback.down')}
            title={t('feedback.down')}
            onClick={() => setPicked('down')}
            style={{ border: 'none', background: 'transparent', cursor: 'pointer', fontSize: 12 }}
          >
            👎
          </button>
        </>
      ) : (
        <>
          <span>{t('feedback.downPlaceholder')}</span>
          <input
            className="toolbar-input"
            value={comment}
            onChange={(e) => setComment(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter') record('down');
            }}
            placeholder={t('feedback.downPlaceholder')}
            style={{ fontSize: 13, padding: '2px 6px', width: 220, marginLeft: 4 }}
          />
          <button
            type="button"
            onClick={() => record('down')}
            style={{ border: '1px solid var(--border-strong)', background: 'var(--surface)', borderRadius: 6, cursor: 'pointer', fontSize: 12, padding: '1px 8px' }}
          >
            {t('feedback.submit')}
          </button>
        </>
      )}
    </div>
  );
}

function AttachmentStrip({ attachments, onRemove }: { attachments: Attachment[]; onRemove: (id: string) => void }) {
  const { t } = useI18n();
  const [zoom, setZoom] = useState<Attachment | null>(null);
  useEffect(() => {
    if (!zoom) return;
    const onKey = (e: KeyboardEvent) => { if (e.key === 'Escape') setZoom(null); };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [zoom]);
  if (attachments.length === 0) return null;
  return (
    <div className="attachment-strip">
      {attachments.map(att => (
        <div
          key={att.id}
          className={`attachment-thumb${att.kind === 'image' ? ' clickable' : ''}`}
          onClick={att.kind === 'image' ? () => setZoom(att) : undefined}
          title={att.kind === 'image' ? t('common.previewImage') : undefined}
        >
          {att.kind === 'image' ? (
            <img src={att.dataUrl} alt={att.name} />
          ) : (
            <div className="attachment-doc">
              <span className="doc-icon">📄</span>
              {!att.text && <span className="doc-meta">{t('common.noTextExtracted')}</span>}
            </div>
          )}
          {att.state === 'error' && <div className="thumb-overlay" style={{ color: 'var(--danger-soft)' }}>Error</div>}
          <button className="thumb-remove" onClick={(e) => { e.stopPropagation(); onRemove(att.id); }}>×</button>
          <div className="thumb-name">
            {Array.from(att.name).length > 12 ? Array.from(att.name).slice(0, 10).join('') + '..' : att.name}
          </div>
        </div>
      ))}
      {zoom && (
        <div className="img-lightbox" onClick={() => setZoom(null)}>
          <div className="img-lightbox-head">
            <span className="img-lightbox-name">{zoom.name}</span>
            <button
              type="button"
              className="img-lightbox-close"
              aria-label="close-image-preview"
              onClick={() => setZoom(null)}
            >✕</button>
          </div>
          <img className="img-lightbox-img" src={zoom.dataUrl} alt={zoom.name} onClick={(e) => e.stopPropagation()} />
        </div>
      )}
    </div>
  );
}

/* ─── Shared composer (input bar) — Chat & Coding render identical UI ─── */

function autosize(el: HTMLTextAreaElement) {
  el.style.height = 'auto';
  // Honor the CSS min-height so variant layouts (e.g. Coding's taller stacked
  // input) set the resting size; the 38px floor covers the inline default.
  const floor = parseInt(getComputedStyle(el).minHeight, 10) || 38;
  el.style.height = Math.max(floor, Math.min(el.scrollHeight, 160)) + 'px';
}

/* ─── approval panel helpers ─── */

/** Minimal terminal glyph for the approval panel header (icon parity with the reference).
 *  Squarish: a rounded-square frame around a chevron prompt. */
const TERMINAL_ICON = '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="3" width="18" height="18" rx="3"/><path d="m7.5 9 2.75 2.75L7.5 14.5"/><path d="M12.75 14.5h3.75"/></svg>';

/** Permission presets (permission-presets parity): each preset bundles a
 *  sandbox mode with an approval mode, so the composer surfaces a single
 *  picker. The danger preset carries the risk-confirmation gate; custom
 *  combos (e.g. legacy logs) render as the derived Custom state. */
const PERMISSION_PRESETS: { value: string; labelKey: string; danger?: boolean }[] = [
  { value: 'manual', labelKey: 'common.permissionManual' },
  { value: 'accept-edits', labelKey: 'common.permissionAcceptEdits' },
  { value: 'auto', labelKey: 'common.permissionAuto' },
  { value: 'danger-full-access', labelKey: 'common.permissionFullAccess', danger: true },
];

/** Human title for a tool name (bash → "Terminal", write → "Editor"). */
function approvalTitle(tool: string): string {
  const map: Record<string, string> = {
    bash: 'Terminal', shell: 'Terminal', sh: 'Terminal', powershell: 'Terminal',
    write: 'Editor', edit: 'Editor', patch: 'Editor', delete: 'Editor',
    read: 'View', web_search: 'Web', web_search_fetch: 'Web',
  };
  return map[tool] || tool;
}

/** The command/payload shown under the reason line, extracted from tool params. */
function approvalCommand(approval: { tool: string; params?: any }): string {
  // Wire params is the tool-call JSON arguments text; tolerate object shapes
  // from older event paths.
  let p = approval.params;
  if (typeof p === 'string') {
    try { p = JSON.parse(p); } catch { return p; }
  }
  if (!p || typeof p !== 'object') return '{}';
  const pick = p.command ?? p.file_path ?? p.path ?? p.query ?? p.url ?? p.filename;
  if (typeof pick === 'string' && pick.trim()) return pick;
  try { return JSON.stringify(p); } catch { return ''; }
}

/* ─── Tool-message icons (replacing the 🔧 emoji) ───
   Unified monochrome set (white glyphs drawn on the near-black chip) with square/
   circular glyphs only — no elongated strokes like a pencil or wrench. */
const TOOL_ICON_ADD = '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="3" width="18" height="18" rx="4"/><path d="M12 8v8M8 12h8"/></svg>';
const TOOL_ICON_DELETE = '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="3" width="18" height="18" rx="4"/><path d="m9.5 9.5 5 5m0-5-5 5"/></svg>';
const TOOL_ICON_WEB = '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10"/><path d="M2 12h20"/><path d="M12 2a14.5 14.5 0 0 1 0 20 14.5 14.5 0 0 1 0-20"/></svg>';
/* WI-146 icon refresh: read-family → magnifier; MCP → plug; git read/write →
   branch/commit; the previously unmapped tools get dedicated glyphs. All in
   the house style: viewBox 24, stroke currentColor, 2px, round caps. */
const TOOL_ICON_MAGNIFIER = '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="11" cy="11" r="8"/><path d="m21 21-4.3-4.3"/></svg>';
const TOOL_ICON_MCP = '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 22v-5"/><path d="M9 8V2"/><path d="M15 8V2"/><path d="M18 8v5a4 4 0 0 1-4 4h-4a4 4 0 0 1-4-4V8Z"/></svg>';
const GIT_ICON_BRANCH = '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><line x1="6" x2="6" y1="3" y2="15"/><circle cx="18" cy="6" r="3"/><circle cx="6" cy="18" r="3"/><path d="M18 9a9 9 0 0 1-9 9"/></svg>';
const GIT_ICON_COMMIT = '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="3.5"/><path d="M3 12h5.5"/><path d="M15.5 12H21"/></svg>';
/** WI-174 review pick (A · ListTodo): checkbox + todo rows — reads as a
 *  to-do list, matching the todo_write session card. */
const TOOL_ICON_LIST_TODO = '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="5" width="6" height="6" rx="1"/><path d="m3 17 2 2 4-4"/><path d="M13 6h8"/><path d="M13 12h8"/><path d="M13 18h8"/></svg>';
const TOOL_ICON_QUESTION = '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M7.9 20A9 9 0 1 0 4 16.1L2 22Z"/><path d="M9.09 9a3 3 0 0 1 5.83 1c0 2-3 3-3 3"/><path d="M12 17h.01"/></svg>';
const TOOL_ICON_CLIPBOARD_CHECK = '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect width="8" height="4" x="8" y="2" rx="1"/><path d="M16 4h2a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2h2"/><path d="m9 14 2 2 4-4"/></svg>';
const TOOL_ICON_SQUARE_PEN = '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 3H5a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2v-7"/><path d="M18.4 2.6a2.1 2.1 0 1 1 3 3L12 15l-4 1 1-4Z"/></svg>';
const TOOL_ICON_TARGET = '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10"/><circle cx="12" cy="12" r="6"/><circle cx="12" cy="12" r="2"/></svg>';
const TOOL_ICON_IMAGE = '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect width="18" height="18" x="3" y="3" rx="2"/><circle cx="9" cy="9" r="2"/><path d="m21 15-3.1-3.1a2 2 0 0 0-2.8 0L6 21"/></svg>';
const TOOL_ICON_RELOAD = '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8"/><path d="M3 3v5h5"/></svg>';
const TOOL_ICON_MESSAGES = '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M14 9a2 2 0 0 1-2 2H6l-4 4V4c0-1.1.9-2 2-2h8a2 2 0 0 1 2 2z"/><path d="M18 9h2a2 2 0 0 1 2 2v11l-4-4h-6a2 2 0 0 1-2-2v-1"/></svg>';
const TOOL_ICON_BOT = '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 8V4H8"/><rect width="16" height="12" x="4" y="8" rx="2"/><path d="M2 14h2"/><path d="M20 14h2"/><path d="M15 13v2"/><path d="M9 13v2"/></svg>';
const TOOL_ICON_CODE = '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m16 18 6-6-6-6"/><path d="m8 6-6 6 6 6"/></svg>';
const TOOL_ICON_SQUARE = '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="3.5" y="3.5" width="17" height="17" rx="4"/></svg>';
const ICON_ARCHIVE = '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="2" y="3" width="20" height="5" rx="1"/><path d="M4 8v11a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8"/><path d="M10 12h4"/></svg>';


const MODE_ICON_MANUAL = '<svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M18 11V6a2 2 0 0 0-2-2a2 2 0 0 0-2 2"/><path d="M14 10V4a2 2 0 0 0-2-2a2 2 0 0 0-2 2v2"/><path d="M10 10.5V6a2 2 0 0 0-2-2a2 2 0 0 0-2 2v8"/><path d="m7 15-1.76-1.76a2 2 0 0 0-2.83 2.82l3.6 3.6C7.5 21.14 9.2 22 12 22h2a8 8 0 0 0 8-8V7a2 2 0 0 0-2-2a2 2 0 0 0-2 2v4"/></svg>';
const MODE_ICON_ACCEPT_EDITS = '<svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M17 3a2.85 2.83 0 1 1 4 4L7.5 20.5 2 22l1.5-5.5Z"/><path d="m15 5 4 4"/></svg>';
const MODE_ICON_AUTO = '<svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 22s8-4 8-10V5l-8-3-8 3v7c0 5 3.5 9.5 8 10z"/><path d="M9 12l2 2 4-4"/></svg>';
const MODE_ICON_BY_PASS = '<svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M10.29 3.86L1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0z"/><path d="M12 9v4"/><path d="M12 17h.01"/></svg>';
const MODE_ICONS: Record<string, string> = {
  manual: MODE_ICON_MANUAL,
  "accept-edits": MODE_ICON_ACCEPT_EDITS,
  auto: MODE_ICON_AUTO,
  bypass: MODE_ICON_BY_PASS,
  // The danger preset reuses the warning glyph; its bundle value is not the
  // legacy bypass approval item but the full-access preset.
  "danger-full-access": MODE_ICON_BY_PASS,
};
const ICON_UNARCHIVE = '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="2" y="3" width="20" height="5" rx="1"/><path d="M4 8v11a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8"/><path d="m9 13 3-3 3 3"/><path d="M12 10v7"/></svg>';
/* Coding toolbar info chips (workspace / git branch) — same stroke style as MODE_ICONS. */
const ICON_PENCIL = '<svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M17 3a2.85 2.83 0 1 1 4 4L7.5 20.5 2 22l1.5-5.5Z"/></svg>';
const ICON_TRASH = '<svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M3 6h18"/><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6"/><path d="M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/></svg>';
const ICON_WORKSPACE_FOLDER = '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V7z"/></svg>';
// Fold indicators for the sidebar Workspace section: open/closed folder glyphs
// replace the old triangle-arrow carets (▼/▶ are banned in the sidebar).
const ICON_FOLDER_CLOSED = '<svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V7z"/></svg>';
/** WI-155: VS Code-style fold chevron — thin angle bracket left of the folder
 *  icon; CSS rotates it 90° when open (› closed → ⌄ open). */
const ICON_CHEVRON_FOLD = '<svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m9 18 6-6-6-6"/></svg>';
const ICON_FOLDER_OPEN = '<svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V7z"/></svg>';
const ICON_GIT_BRANCH = '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><line x1="6" y1="3" x2="6" y2="15"/><circle cx="18" cy="6" r="3"/><circle cx="6" cy="18" r="3"/><path d="M18 9a9 9 0 0 1-9 9"/></svg>';
/* Sidebar phone button: opens the Remote Access (iPhone pairing) dialog. */
const ICON_PHONE = '<svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect width="14" height="20" x="5" y="2" rx="2" ry="2"/><path d="M12 18h.01"/></svg>';
/* Thinking-row icon: monochrome stroke brain (Lucide), same style/size as the tool icons. */
const ICON_SYSTEM = '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 0 1 0 2.83 2 2 0 0 1-2.83 0l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-2 2 2 2 0 0 1-2-2v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 0 1-2.83 0 2 2 0 0 1 0-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1-2-2 2 2 0 0 1 2-2h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 0 1 0-2.83 2 2 0 0 1 2.83 0l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 2-2 2 2 0 0 1 2 2v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 0 1 2.83 0 2 2 0 0 1 0 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 2 2 2 2 0 0 1-2 2h-.09a1.65 1.65 0 0 0-1.51 1z"/></svg>';
const ICON_CONTEXT = '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z"/></svg>';
const ICON_THINKING = '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 5a3 3 0 1 0-5.997.125 4 4 0 0 0-2.526 5.77 4 4 0 0 0 .556 6.588A4 4 0 1 0 12 18Z"/><path d="M12 5a3 3 0 1 1 5.997.125 4 4 0 0 1 2.526 5.77 4 4 0 0 1-.556 6.588A4 4 0 1 1 12 18Z"/><path d="M15 13a4.5 4.5 0 0 1-3-4 4.5 4.5 0 0 1-3 4"/><path d="M17.599 6.5a3 3 0 0 0 .399-1.375"/><path d="M6.003 5.125A3 3 0 0 0 6.401 6.5"/><path d="M3.477 10.896a4 4 0 0 1 .585-.396"/><path d="M19.938 10.5a4 4 0 0 1 .585.396"/><path d="M6 18a4 4 0 0 1-1.967-.516"/><path d="M19.964 17.484A4 4 0 0 0 18 18"/></svg>';
/* Execution-mode icons (monochrome stroke, same style as the approval/tool icons). */
const EXEC_MODE_ICONS: Record<string, string> = {
  standard: '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="4" y="4" width="16" height="16" rx="3"/><path d="m8.5 12 2.5 2.5 4.5-5"/></svg>',
  plan: '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="5" y="4" width="14" height="16" rx="2"/><path d="M9 9h6M9 13h6M9 17h3"/></svg>',
  ptc: '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m12 3 9 5-9 5-9-5 9-5z"/><path d="m3 13 9 5 9-5"/></svg>',
  goal: '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="9"/><circle cx="12" cy="12" r="5"/><circle cx="12" cy="12" r="1"/></svg>',
};

/** Reasoning Level options: the composer only exposes explicit levels
 *  (Low/High/Max). 'Default'/'off' are no longer offered — legacy persisted
 *  values ('' = provider default, 'off') are normalized to High on load. */
const REASONING_EFFORTS = ['low', 'high', 'max'] as const;

function normalizeReasoningEffort(raw?: string | null): string {
  return (REASONING_EFFORTS as readonly string[]).includes(raw ?? '') ? raw! : 'high';
}

function reasoningOptionsFor(): { value: string; label: string }[] {
  return REASONING_EFFORTS.map(value => ({
    value, label: value.charAt(0).toUpperCase() + value.slice(1),
  }));
}

const TOOL_ICONS: Record<string, string> = {
  bash: TERMINAL_ICON, shell: TERMINAL_ICON, sh: TERMINAL_ICON, powershell: TERMINAL_ICON,
  python_exec: TERMINAL_ICON,
  terminal: TERMINAL_ICON, terminal_start: TERMINAL_ICON, terminal_list: TERMINAL_ICON, terminal_read: TERMINAL_ICON,
  terminal_write: TERMINAL_ICON, terminal_signal: TERMINAL_ICON, terminal_wait: TERMINAL_ICON,
  terminal_kill: TERMINAL_ICON,
  patch: TOOL_ICON_ADD, write: TOOL_ICON_ADD, edit: TOOL_ICON_ADD, delete: TOOL_ICON_DELETE,
  read: TOOL_ICON_MAGNIFIER, grep: TOOL_ICON_MAGNIFIER, glob: TOOL_ICON_MAGNIFIER,
  ls: TOOL_ICON_MAGNIFIER, find: TOOL_ICON_MAGNIFIER,
  lsp_symbols: TOOL_ICON_MAGNIFIER, lsp_diagnostics: TOOL_ICON_MAGNIFIER,
  web_search: TOOL_ICON_WEB, web_search_fetch: TOOL_ICON_WEB, web_fetch: TOOL_ICON_WEB,
  todo_write: TOOL_ICON_LIST_TODO,
  ask_user: TOOL_ICON_QUESTION,
  exit_plan_mode: TOOL_ICON_CLIPBOARD_CHECK,
  plan_write: TOOL_ICON_SQUARE_PEN,
  goal_write: TOOL_ICON_TARGET,
  generate_image: TOOL_ICON_IMAGE,
  reload_image: TOOL_ICON_RELOAD,
  session_reference: TOOL_ICON_MESSAGES,
  subagent_spawn: TOOL_ICON_BOT,
};

/** Git subcommands that MUTATE the repository → commit icon; every other
 *  subcommand (status/log/diff/show/blame/branch…) and an absent/unknown hint
 *  read as inspection → branch icon. (WI-146) */
const GIT_WRITE_RE = /\b(commit|push|pull|merge|rebase|checkout|switch|reset|restore|stash|cherry-pick|revert|tag|am|apply|add|rm|mv|clean|bisect)\b/;

/** Icon for a tool card, by tool name plus an optional hint (the command line
 *  or arguments) used only by git's read/write split. MCP tools carry the
 *  bridge's `mcp_<server>_<tool>` naming. (WI-146) */
export function toolIconFor(name: string, hint?: string): string {
  if (name.startsWith('mcp_')) return TOOL_ICON_MCP;
  if (name === 'git') {
    return hint && GIT_WRITE_RE.test(hint.toLowerCase()) ? GIT_ICON_COMMIT : GIT_ICON_BRANCH;
  }
  return TOOL_ICONS[name] || TOOL_ICON_SQUARE;
}

/** Display titles for the collapsible card header ("Shell", "Read", …). */
const TOOL_TITLES: Record<string, string> = {
  bash: 'Shell', shell: 'Shell', sh: 'Shell', zsh: 'Shell',
  powershell: 'Shell', pwsh: 'Shell', cmd: 'Shell',
  read: 'Read', write: 'Write', edit: 'Edit', patch: 'Patch', delete: 'Delete',
  git: 'Git', grep: 'Grep', glob: 'Glob', ls: 'List', find: 'Find',
  lsp_symbols: 'Symbols', lsp_diagnostics: 'Diagnostics',
  web_search: 'Web Search', web_search_fetch: 'Web Fetch',
};

/** Collapse whitespace and cap a one-line summary at n chars with an ellipsis. */
function truncateOneLine(s: string, n: number): string {
  const chars = Array.from(s);
  if (chars.length <= n) return s;
  return chars.slice(0, n).join('') + '…';
}

/** Tool name from a tool row's label ("🔧 patch: x" / "patch: x" → "patch"). */
function toolNameOf(content: string): string {
  const m = content.replace(/^🔧\s*/, '').match(/^([^:\s]+)/);
  return m ? m[1] : '';
}

/** WI-151 follow-up: code/output block copy button — clipboard API first,
 *  execCommand fallback; briefly turns ✓ on success. */
export function CopyChip({ text }: { text: string }) {
  const { t } = useI18n();
  const [ok, setOk] = useState(false);
  const onCopy = async () => {
    try {
      await navigator.clipboard.writeText(text);
    } catch {
      const ta = document.createElement('textarea');
      ta.value = text;
      ta.style.position = 'fixed';
      ta.style.opacity = '0';
      document.body.appendChild(ta);
      ta.select();
      document.execCommand('copy');
      ta.remove();
    }
    setOk(true);
    setTimeout(() => setOk(false), 900);
  };
  return (
    <button
      type="button" className={'copy-chip' + (ok ? ' ok' : '')}
      aria-label={t('common.copy')} title={t('common.copy')}
      onClick={onCopy}
    >
      {ok ? '✓' : (
        <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor"
          stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
          <rect x="9" y="9" width="13" height="13" rx="2" />
          <path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1" />
        </svg>
      )}
    </button>
  );
}

/** Code-block syntax highlighting helpers moved to ./codeLang (shared with
 *  diffview.tsx; colors live in styles.css --code-*). */

/** WI-151: numbered code block — per-line spans with a CSS-counter
 *  gutter (row numbers live in ::before, so textContent stays clean), row
 *  hover highlight, and wrapping lines for narrow cards.
 *  BUG-143: highlight the WHOLE code then split the HTML per line (span
 *  state carried across newlines) — multi-line strings/comments stay colored
 *  and continuation lines are no longer re-tokenized into fake strings. */
function CodeBlock({ lang, code }: { lang: string; code: string }) {
  const htmlLines = highlightCodeLines(code, lang);
  const lines = htmlLines ?? code.replace(/\n$/, '').split('\n');
  return (
    <pre className="tool-row-code code-numbered" data-lang={lang}>
      <code>
        {lines.map((ln, i) => (
          <span key={i} className="code-line">
            <span
              className="code-text"
              dangerouslySetInnerHTML={htmlLines !== null ? { __html: ln || ' ' } : undefined}
            >
              {htmlLines === null ? (ln || ' ') : undefined}
            </span>
          </span>
        ))}
      </code>
      {/* WI-151 follow-up: one-click command copy (hovering at the right end of the language label row). */}
      <span className="code-copy-slot"><CopyChip text={code} /></span>
    </pre>
  );
}

/** Tool-call JSON arguments → the human-relevant field (command/path/file/…)
 *  flattened to one line for the tool card label; '' when absent/unparseable. */
export function toolCommandOf(input: unknown): string {
  if (!input) return '';
  try {
    const params = typeof input === 'string' ? JSON.parse(input) : input;
    const cmd = params.command || params.path || params.file || params.pattern || params.query || params.url
      || params.old_string?.slice(0, 60)
      || (params.template ? `${params.template}: ${(params.prompt || '').slice(0, 60)}` : '')
      || '';
    if (typeof cmd !== 'string') return '';
    // Flatten newlines so a multiline bash command stays on one row and
    // ToolRow renders it as a single code block (it unescapes on display).
    return cmd.replace(/\n/g, '\\n').replace(/\r/g, '\\r');
  } catch { return ''; }
}

/** Tool output body → a language hint when it looks like code (XML/JSON), else null. */
function detectOutputLanguage(body: string): string | null {
  const b = body.replace(/^[✓✗]\s*/, '').trim();
  if (!b) return null;
  if (b.startsWith('<?') || /^<[a-zA-Z!][^\[\{]/.test(b)) return 'xml';
  if (b.startsWith('{') || b.startsWith('[')) return 'json';
  return null;
}

/** Whether a body is real source code (multi-line + syntax structure) as opposed to
 *  a short status line like "✓ created /path/file.java". */
function looksLikeCode(body: string): boolean {
  const b = body.replace(/^[✓✗]\s*/, '').trim();
  if (!b || b.length < 40 || !b.includes('\n')) return false;
  if (/[{}\[\]();]/.test(b)) return true;
  return /^\s*(package|import|using|namespace|module|#include|def\s|class\s|public\s|private\s|protected\s|function\s|const\s|let\s|var\s|static\s|return\s|export\s)/m.test(b);
}

/** Green check / red cross status badge replacing the plain-text "✓"/"✗" glyph. */
function StatusIcon({ ok }: { ok: boolean }) {
  return (
    <svg className={`status-icon ${ok ? 'status-ok' : 'status-err'}`} width="15" height="15"
      viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.4"
      strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <circle cx="12" cy="12" r="10" />
      {ok ? <path d="m8.5 12.5 2.5 2.5 5-6" /> : <path d="m9 9 6 6m0-6-6 6" />}
    </svg>
  );
}

/** Tools whose argument is a shell command — rendered as a "bash" code block so the
 *  command gets its own line instead of running together with the label text. */
const COMMAND_TOOLS = new Set(['bash', 'shell', 'sh', 'zsh', 'powershell', 'pwsh', 'cmd']);

/** A tool execution card.
 *
 *  Collapsible mode (Coding) renders three parts:
 *    1. a header row — command icon + tool title ("Shell") + one-line command
 *       summary + a fold/unfold chevron on the right;
 *    2. the full command in a code block;
 *    3. the execution result in a boxed block, with a green ✓ (or red ✗) marker.
 *  Completed cards fold by default; clicking the header reveals the detail.
 *
 *  Non-collapsible mode keeps the compact label + output layout (Chat).
 *
 *  `result`/`ok` are passed by the Coding view when a stored tool-result message
 *  is attached to its preceding tool card; otherwise the row's own inline markers
 *  (live " ✓"/" ✗" or stored "✓ …"/"✗ …" prefixes) are used. */

/* ─── per-message meta: completion time + copy-to-clipboard ─── */

/** Compact wall-clock span: "42s" under a minute, "3m 25s" above. */
function fmtDuration(ms: number): string {
  const s = Math.round(ms / 1000);
  if (s < 60) return `${s}s`;
  return `${Math.floor(s / 60)}m ${s % 60}s`;
}

function MessageMeta({ ts, text, extra }: { ts?: number; text: string; extra?: React.ReactNode }) {
  const { t } = useI18n();
  const [copied, setCopied] = React.useState(false);
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(text);
    } catch {
      // Fallback for contexts where the async clipboard API is unavailable.
      const ta = document.createElement('textarea');
      ta.value = text;
      ta.style.position = 'fixed';
      ta.style.opacity = '0';
      document.body.appendChild(ta);
      ta.select();
      document.execCommand('copy');
      ta.remove();
    }
    setCopied(true);
    window.setTimeout(() => setCopied(false), 1500);
  };
  // Footer timestamp: date + time — time-only turned ambiguous once a
  // conversation spans days (user feedback 2026-10-04). ISO-style matches the
  // scheduled panel's ⏭ nextRun format.
  const time = ts
    ? (() => {
        const d = new Date(ts);
        const date = `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`;
        const hhmm = d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
        return `${date} ${hhmm}`;
      })()
    : null;
  if (!time && !text && !extra) return null;
  return (
    <div className="msg-meta">
      {time && <span className="msg-meta-time">{time}</span>}
      {text && (
        <button className="msg-meta-copy" data-tip={t('common.copy')} onClick={copy}>
          {copied ? '✓' : '⧉'}
        </button>
      )}
      {extra}
    </div>
  );
}

/** Duration + token spend for the meta row: live turns read the accumulator,
 *  history turns read the projected message fields — missing pieces stay hidden. */
export function TurnUsageMeta({
  m,
  live,
}: {
  m: Message;
  live: { durationMs: number; input: number; output: number; forId: string } | null;
}) {
  const { t } = useI18n();
  if (m.role !== 'assistant') return null;
  if (live && live.forId === m.id) {
    return (
      <>
        <span className="msg-meta-time">
          ↑{live.input.toLocaleString()} ↓{live.output.toLocaleString()} tokens
        </span>
      </>
    );
  }
  const { usage } = m;
  if (!usage) return null;
  return (
    <>
      {usage && (
        <span className="msg-meta-time">
          ↑{usage.inputTokens.toLocaleString()} ↓{usage.outputTokens.toLocaleString()} tokens
        </span>
      )}
    </>
  );
}

/** WI-147 work-fold row (iOS parity): one "worked 9m 2s ›" line; collapsed it
 *  hides the turn's steps (thinking + tools, time-ordered flat); the turn summary stays
 *  visible below it. The streaming turn is forced expanded; pure Q&A turns render no steps and no row. */
function WorkFoldRow({ group, expanded, onToggle }: {
  group: { id: string; durationMs?: number; liveFrom?: number };
  expanded: boolean;
  onToggle: () => void;
}) {
  const { t } = useI18n();
  // A streaming turn's duration ticks every second; finished ones use the static projected duration.
  const [, setTick] = useState(0);
  const live = group.liveFrom != null;
  useEffect(() => {
    if (!live) return;
    const timer = setInterval(() => setTick(n => n + 1), 1000);
    return () => clearInterval(timer);
  }, [live]);
  const elapsed = live
    ? fmtDuration(Math.max(0, Date.now() - group.liveFrom!))
    : group.durationMs != null ? fmtDuration(group.durationMs) : null;
  return (
    <button
      type="button"
      className={'work-fold-row' + (expanded ? ' expanded' : '')}
      data-testid="work-fold-row"
      data-work-fold={group.id}
      aria-expanded={expanded}
      title={expanded ? t('coding.turnFoldCollapse') : t('coding.turnFoldExpand')}
      onClick={onToggle}
    >
      {elapsed != null && (
        <span className="work-fold-elapsed">{t('coding.turnFoldElapsed', { duration: elapsed })}</span>
      )}
      {/* Angle-bracket chevron (»-style `>`), hairline-colored per user review. */}
      <span
        className="work-fold-chev" aria-hidden="true"
        dangerouslySetInnerHTML={{ __html: expanded
          ? '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="m5 9 7 7 7-7"/></svg>'
          : '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="m9 5 7 7-7 7"/></svg>' }}
      />
    </button>
  );
}

/** WI-150: one background-terminal row — start clock time + live elapsed
 *  (ticks every second while mounted). */
function TerminalRow({ term, onKill }: {
  term: { id: string; status: string; startedAtMs: number; command?: { startedAtMs: number; finishedAtMs?: number; exitCode?: number } | null };
  onKill: () => void;
}) {
  const { t } = useI18n();
  const [, setTick] = useState(0);
  useEffect(() => {
    const timer = setInterval(() => setTick(n => n + 1), 1000);
    return () => clearInterval(timer);
  }, []);
  // 逐字节实时一致（dsh 式标记契约）：命令有精确生命周期——发出即 running，
  // 结束标记一出现即 done · exit N；无标记信息时回退 host 会话状态。
  const cmd = term.command;
  const baseMs = cmd?.startedAtMs ?? term.startedAtMs;
  const startedText = new Date(baseMs).toLocaleTimeString(undefined, { hour12: false });
  const endMs = cmd?.finishedAtMs ?? null;
  const elapsed = fmtDuration(Math.max(0, (endMs ?? Date.now()) - baseMs));
  const done = cmd?.finishedAtMs != null;
  const statusText = done ? `done · exit ${cmd?.exitCode ?? 0}` : term.status;
  const statusClass = done ? 'done' : term.status;
  return (
    <div className="terminal-row" title={term.id}>
      <span className="terminal-started">{startedText}</span>
      <span className="terminal-elapsed">{elapsed}</span>
      <span className={`terminal-status ${statusClass}`}>{statusText}</span>
      <button type="button" className="terminal-kill" title={t('coding.terminalKill')} onClick={onKill}>✕</button>
    </div>
  );
}

/** WI-157: one running shell tool call in the terminals popover — command plus
 *  a live elapsed stopwatch (1s tick, same rhythm as TerminalRow). */
function ShellCmdRow({ entry }: { entry: { cmd: string; startedAt: number } }) {
  const [, setTick] = useState(0);
  useEffect(() => {
    const timer = setInterval(() => setTick(n => n + 1), 1000);
    return () => clearInterval(timer);
  }, []);
  const startedText = new Date(entry.startedAt).toLocaleTimeString(undefined, { hour12: false });
  const elapsed = fmtDuration(Math.max(0, Date.now() - entry.startedAt));
  return (
    <div className="terminals-shell-cmd" title={entry.cmd}>
      <span className="terminals-shell-dot" aria-hidden />
      <span className="terminals-shell-text">{entry.cmd}</span>
      <span className="terminals-shell-meta">{startedText} · {elapsed}</span>
    </div>
  );
}

/* ─── turn-level file change summary ─── */

/** Per-turn changed-files card. Each row opens the file's diff in the
    right-hand panel (审查) — the inline `<details>` expansion was replaced by
    the panel so diffs get the full width and coexist with the Plan tab. */
export function TurnFileSummary({
  files,
  onOpen,
}: {
  files: { path: string; add: number; del: number; text: string }[];
  onOpen?: (path: string, text: string) => void;
}) {
  const { t } = useI18n();
  const add = files.reduce((n, f) => n + f.add, 0);
  const del = files.reduce((n, f) => n + f.del, 0);
  const count = (t('coding.turnFilesChanged') as string).replace('{count}', String(files.length));
  return (
    <div className="turn-files">
      <div className="turn-files-head">
        <span>{count}</span>
        <span className="turn-files-total">
          <span className="diff-stat-add">+{add}</span>
          <span className="diff-stat-del">−{del}</span>
        </span>
      </div>
      {files.map((f) => (
        <div key={f.path} className="turn-file-item">
          <button
            type="button"
            className="turn-file-row"
            title={f.path}
            onClick={() => onOpen?.(f.path, f.text)}
          >
            <span className="turn-file-path">{f.path}</span>
            <span className="turn-file-stats">
              <span className="diff-stat-add">+{f.add}</span>
              <span className="diff-stat-del">−{f.del}</span>
            </span>
          </button>
          {onOpen && (
            <button
              type="button"
              className="turn-file-review"
              onClick={() => onOpen(f.path, f.text)}
            >
              {t('coding.turnFileReview')}
            </button>
          )}
        </div>
      ))}
    </div>
  );
}

/* ─── toolbar tools menu: open the workspace in a terminal / IDE ─── */

const IDE_ICON = '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="16 18 22 12 16 6"/><polyline points="8 6 2 12 8 18"/></svg>';

/** Toolbar-right dropdown (approved wireframe 2026-09-13): open the session's
    workspace directory in the user's terminal or IDE. Both actions spawn
    OUTSIDE the agent (main-process IPC) — sandbox does not apply. An
    `apps.*_command` template from settings.yaml wins over the platform
    default; `{dir}` is substituted by the main process. */
export function ToolsMenu({ cwd, onError }: { cwd: string | null; onError: (message: string) => void }) {
  const { t } = useI18n();
  const [open, setOpen] = useState(false);
  const [commands, setCommands] = useState<{ terminal?: string; ide?: string; loaded?: boolean }>({});
  const wrapRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const onDown = (e: MouseEvent) => {
      if (wrapRef.current && wrapRef.current.contains(e.target as Node)) return;
      setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setOpen(false);
    };
    document.addEventListener('mousedown', onDown);
    document.addEventListener('keydown', onKey);
    return () => {
      document.removeEventListener('mousedown', onDown);
      document.removeEventListener('keydown', onKey);
    };
  }, [open]);

  // Lazy-load the configured command templates once per menu life.
  useEffect(() => {
    if (!open || commands.loaded) return;
    Promise.resolve(window.starriver.getSettings?.())
      .then((s) => {
        const apps = (s as { apps?: { terminal_command?: string; ide_command?: string } })?.apps ?? {};
        setCommands({ terminal: apps.terminal_command, ide: apps.ide_command, loaded: true });
      })
      .catch(() => setCommands({ loaded: true }));
  }, [open, commands.loaded]);

  const launch = async (kind: 'terminal' | 'ide') => {
    if (!cwd) return;
    try {
      const fn = kind === 'terminal' ? window.starriver.openTerminal : window.starriver.openIDE;
      if (!fn) throw new Error('not supported by this build');
      await fn(cwd, kind === 'terminal' ? commands.terminal : commands.ide);
      setOpen(false);
    } catch (e) {
      onError(`${t('coding.openFailed')}: ${String((e as Error)?.message ?? e)}`);
      setOpen(false);
    }
  };

  const disabled = !cwd;
  return (
    <div ref={wrapRef} style={{ position: 'relative', marginLeft: 'auto' }}>
      <button
        type="button"
        className={`tools-btn${open ? ' open' : ''}`}
        aria-label="tools-menu"
        aria-expanded={open}
        onClick={() => setOpen((o) => !o)}
      >
        <span dangerouslySetInnerHTML={{ __html: TERMINAL_ICON }} />
        {t('coding.toolsMenu')}
        <span className="tools-btn-caret">▾</span>
      </button>
      {open && (
        <div className="tools-menu">
          <button
            type="button"
            className="tools-menu-item"
            disabled={disabled}
            title={disabled ? t('coding.toolsNoWorkspace') : undefined}
            onClick={() => launch('terminal')}
          >
            <span className="tools-menu-icon" dangerouslySetInnerHTML={{ __html: TERMINAL_ICON }} />
            <span className="tools-menu-copy">
              <span className="tools-menu-label">{t('coding.openConsole')}</span>
              <span className="tools-menu-sub">{t('coding.openConsoleSub')}</span>
            </span>
          </button>
          <button
            type="button"
            className="tools-menu-item"
            disabled={disabled}
            title={disabled ? t('coding.toolsNoWorkspace') : undefined}
            onClick={() => launch('ide')}
          >
            <span className="tools-menu-icon" dangerouslySetInnerHTML={{ __html: IDE_ICON }} />
            <span className="tools-menu-copy">
              <span className="tools-menu-label">{t('coding.openIDE')}</span>
              <span className="tools-menu-sub">{t('coding.openIDESub')}</span>
            </span>
          </button>
          <div className="tools-menu-note">
            {disabled ? t('coding.toolsNoWorkspace') : t('coding.toolsMenuNote')}
          </div>
        </div>
      )}
    </div>
  );
}

/** Trajectory view — two display modes over the same event ledger:
 *  - list (default): turn-grouped cards, one folding row per entry;
 * - ledger (-style): a dense fixed-height event table — kind-tag gutter,
 *    one-line ellipsized previews, `tool {args} → result` merged tool rows,
 *    turn rails/labels; clicking a row expands its detail inline.
 *  The pick persists in localStorage. */
export function TrajectoryView({ sessionId }: { sessionId: string }) {
  const { t } = useI18n();
  const [entries, setEntries] = useState<TraceEntry[] | null>(null);
  const [mode, setMode] = useState<'list' | 'ledger'>(() =>
    localStorage.getItem('starriver.trajMode') === 'ledger' ? 'ledger' : 'list');
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const switchMode = (m: 'list' | 'ledger') => {
    setMode(m);
    setExpanded(new Set());
    localStorage.setItem('starriver.trajMode', m);
  };
  useEffect(() => {
    setEntries(null);
    if (!sessionId) { setEntries([]); return; }
    // Guard against stale responses: a slow session-A fetch resolving after
    // session-B's would render A's ledger under B's id.
    let stale = false;
    window.starriver.getSessionTrace(sessionId)
      .then((r) => { if (!stale) setEntries(r?.entries ?? []); })
      .catch(() => { if (!stale) setEntries([]); });
    // Positional keys (`e3`, `turn-1`) are meaningless across sessions —
    // reset the open/collapsed/selection state or session B inherits A's.
    setExpanded(new Set());
    setCollapsedTurns(new Set());
    setCollapsedCalls(new Set());
    setSelected(null);
    return () => { stale = true; };
  }, [sessionId]);

  type TrajRow = TraceEntry & { key: string; kind: TraceEntry['kind']; merged?: boolean };
  const turns = useMemo(() => {
    const turns: { key: string; rows: TrajRow[]; hasUser: boolean }[] = [];
    let n = 0;
    let cur: { key: string; rows: TrajRow[] } | null = null;
    const list = entries ?? [];
    const carry = (e: TraceEntry, idx: number): TrajRow => ({ ...e, key: `e${idx}` });
    for (let i = 0; i < list.length; i++) {
      const e = list[i];
      if (e.kind === 'user') {
        cur = { key: `turn-${n++}`, rows: [], hasUser: true };
        turns.push(cur);
      } else if (!cur) {
        // Preamble group (SYSTEM/CONTEXT before the first user message):
        // carries no Turn label — it is session preface, not a turn.
        cur = { key: `preamble`, rows: [], hasUser: false };
        turns.push(cur);
      }
      if (e.kind === 'tool_use') {
        const next = list[i + 1];
        const merged = next && next.kind === 'tool_result';
        const mergedOk = merged ? next.toolOk : null;
        const mergedOut = merged ? next.toolOutput : null;
        cur.rows.push({
          ...carry(e), key: `e${i}`, kind: 'tool', merged,
          toolName: e.toolName || 'tool',
          toolInput: e.toolInput,
          toolOutput: mergedOut != null ? mergedOut : e.toolOutput ?? null,
          toolOk: merged ? mergedOk : e.toolOk ?? null,
        });
        if (merged) i++;
      } else if (e.kind === 'tool_result') {
        cur.rows.push({ ...carry(e, i), kind: 'tool', toolName: 'tool' });
      } else {
        cur.rows.push({ ...carry(e, i) });
      }
    }
    return turns;
  }, [entries]);

 // ── Ledger-mode instrumentation state (toolbar parity) ──
  const [collapsedTurns, setCollapsedTurns] = useState<Set<string>>(new Set());
  const [collapsedCalls, setCollapsedCalls] = useState<Set<string>>(new Set());
  const [search, setSearch] = useState('');
  const [selected, setSelected] = useState<string | null>(null);
  const [inspectorTab, setInspectorTab] = useState<'summary' | 'payload' | 'result' | 'timing'>('summary');
  const allTurnsCollapsed = turns.length > 0 && turns.every((tu) => collapsedTurns.has(tu.key));
  const allCallsCollapsed = turns.length > 0 && turns.every(
    (tu) => tu.rows.filter((r) => r.kind === 'tool').every((r) => collapsedCalls.has(r.key)));
  const q = search.trim().toLowerCase();
  const matches = (r: TrajRow) =>
    !q ||
    r.text?.toLowerCase().includes(q) ||
    r.toolName?.toLowerCase().includes(q) ||
    r.toolInput?.toLowerCase().includes(q) ||
    r.toolOutput?.toLowerCase().includes(q);
  const selectedRow: TrajRow | null =
    turns.flatMap((tu) => tu.rows).find((r) => r.key === selected) ?? null;
  const scrollToRow = (key: string) => {
    document.querySelector(`[data-row-key="${key}"]`)?.scrollIntoView({ block: 'center' });
  };
  const fmtMs = (ms: number | null | undefined) =>
    ms == null ? '—' : ms >= 1000 ? `${(ms / 1000).toFixed(2)}s` : `${Math.round(ms)}ms`;
  const fmtTime = (ms: number | null | undefined) =>
    ms == null ? '—' : new Date(ms).toLocaleTimeString(undefined, { hour12: false });

  if (entries === null) {
    return <div className="traj-empty">{t('coding.trajLoading')}</div>;
  }
  if (entries.length === 0) {
    return <div className="traj-empty">{t('coding.trajEmpty')}</div>;
  }

  const kindLabel = (k: TrajRow['kind']) =>
    k === 'user' ? t('coding.trajUser') : k === 'assistant' ? t('coding.trajAssistant')
    : k === 'thinking' ? t('coding.trajThinking') : k === 'system' ? t('coding.trajSystem')
    : k === 'context' ? t('coding.trajContext') : t('coding.trajTool');
  const kindIcon = (k: TrajRow['kind'], name?: string, hint?: string) => {
    if (k === 'system') return ICON_SYSTEM;
    if (k === 'context') return ICON_CONTEXT;
    if (k === 'thinking') return ICON_THINKING;
    if (k === 'tool') return toolIconFor(name || '', hint);
    if (k === 'user') return tabIcons.chat || '';
    return tabIcons.coding || '';
  };
  const preview = (text: string) => truncateOneLine(text.split('\n').map((l: string) => l.trim()).find((l: string) => l) || '', 110);

 // ledger label: uppercase the kind tag in CSS, not here.
  const ledgerPreview = (text: string | null | undefined, cap = 160) =>
    truncateOneLine((text ?? '').split('\n').map((l: string) => l.trim()).find((l: string) => l) || '', cap);

  const modeSwitch = (
    <div className="traj-mode-switch" role="group" aria-label={t('coding.trajMode')}>
      <button type="button" className={mode === 'list' ? 'active' : ''} aria-pressed={mode === 'list'} onClick={() => switchMode('list')}>{t('coding.trajModeList')}</button>
      <button type="button" className={mode === 'ledger' ? 'active' : ''} aria-pressed={mode === 'ledger'} onClick={() => switchMode('ledger')}>{t('coding.trajModeLedger')}</button>
    </div>
  );

  if (mode === 'ledger') {
 // Assistant rows followed by same-group tool rows get a
    // "… N tool calls · names" fold summary (the Calls toggle flips them).
    const toolRowsOf = (turn: typeof turns[number], fromIdx: number): TrajRow[] => {
      const out: TrajRow[] = [];
      for (let j = fromIdx + 1; j < turn.rows.length; j++) {
        if (turn.rows[j].kind !== 'tool') break;
        out.push(turn.rows[j]);
      }
      return out;
    };
    const totalRows = turns.reduce((a, tu) => a + tu.rows.filter(matches).length, 0);

    const inspector = selectedRow && (() => {
      const isTool = selectedRow.kind === 'tool';
      const tabs: { id: typeof inspectorTab; label: string; enabled: boolean }[] = [
        { id: 'summary', label: t('coding.trajTabSummary'), enabled: true },
        { id: 'payload', label: t('coding.trajTabPayload'), enabled: isTool ? !!selectedRow.toolInput : !!selectedRow.text },
        { id: 'result', label: t('coding.trajTabResult'), enabled: isTool && selectedRow.toolOutput != null },
        { id: 'timing', label: t('coding.trajTabTiming'), enabled: selectedRow.timeMs != null },
      ];
      return (
        <aside className="traj-inspector" aria-label={t('coding.trajInspector')}>
          <div className="traj-inspector-head">
            <span className={`traj-ledger-kind k-${selectedRow.kind}`}>{kindLabel(selectedRow.kind)}</span>
            <span className="traj-inspector-loc">
              {t('coding.trajTurnShort', { n: (selectedRow.turn ?? 1) })}
              {selectedRow.step != null ? ` · ${t('coding.trajStep', { n: selectedRow.step })}` : ''}
            </span>
            <button className="traj-inspector-close" onClick={() => setSelected(null)} aria-label={t('common.close')}>✕</button>
          </div>
          <div className="traj-inspector-tabs" role="tablist">
            {tabs.filter((tb) => tb.enabled).map((tb) => (
              <button key={tb.id} role="tab" aria-selected={inspectorTab === tb.id}
                className={inspectorTab === tb.id ? 'active' : ''}
                onClick={() => setInspectorTab(tb.id)}>{tb.label}</button>
            ))}
          </div>
          <div className="traj-inspector-body" role="tabpanel">
            {inspectorTab === 'summary' && (
              <dl className="traj-inspector-dl">
                <dt>{t('coding.trajStatus')}</dt>
                <dd>{selectedRow.kind === 'tool'
                  ? (selectedRow.toolOk === false ? t('coding.trajStatusError') : t('coding.trajStatusDone'))
                  : t('coding.trajStatusDone')}</dd>
                {selectedRow.provider && <dt>Provider</dt>}
                {selectedRow.provider && <dd>{selectedRow.provider}</dd>}
                {selectedRow.model && <dt>{t('common.model')}</dt>}
                {selectedRow.model && <dd>{selectedRow.model}</dd>}
                {(selectedRow.inputTokens != null || selectedRow.outputTokens != null) && <dt>{t('coding.trajTokens')}</dt>}
                {(selectedRow.inputTokens != null || selectedRow.outputTokens != null) &&
                  <dd>{selectedRow.inputTokens ?? 0} → {selectedRow.outputTokens ?? 0}</dd>}
                <dt>{t('coding.trajSeq')}</dt>
                <dd>#{selectedRow.seq ?? '—'} · {fmtTime(selectedRow.timeMs)}</dd>
              </dl>
            )}
            {inspectorTab === 'payload' && (
              isTool
                ? <pre className="traj-code">{selectedRow.toolInput ?? ''}</pre>
                : <div className="traj-text">{selectedRow.text}</div>
            )}
            {inspectorTab === 'result' && <pre className="traj-code traj-code-out">{selectedRow.toolOutput ?? ''}</pre>}
            {inspectorTab === 'timing' && (
              <dl className="traj-inspector-dl">
                <dt>{t('coding.trajStart')}</dt><dd>{fmtTime(selectedRow.timeMs)}</dd>
                {selectedRow.ttftMs != null && <><dt>TTFT</dt><dd>{fmtMs(selectedRow.ttftMs)}</dd></>}
                {selectedRow.durationMs != null && <><dt>{t('coding.trajDecode')}</dt><dd>{fmtMs(selectedRow.durationMs)}</dd></>}
              </dl>
            )}
          </div>
        </aside>
      );
    })();

    return (
      <div className="traj-ledger-wrap">
          <div className="traj-ledger-toolbar">
            <span className="traj-ledger-title">{t('coding.trajLedgerTitle')}</span>
            <div className="traj-ledger-controls">
              <button type="button" className="traj-ledger-ctl" aria-pressed={allTurnsCollapsed}
                onClick={() => setCollapsedTurns(allTurnsCollapsed ? new Set() : new Set(turns.map((tu) => tu.key)))}>
                {allTurnsCollapsed ? t('coding.trajExpandTurns') : t('coding.trajCollapseTurns')}
              </button>
              <button type="button" className="traj-ledger-ctl" aria-pressed={allCallsCollapsed}
                onClick={() => {
                  if (allCallsCollapsed) { setCollapsedCalls(new Set()); return; }
                  const next = new Set<string>();
                  turns.forEach((tu) => tu.rows.forEach((r, ri) => { if (r.kind === 'tool') {
                    const tools = toolRowsOf(tu, ri);
                    void tools;
                  } }));
                  // Fold every tool row individually (group boundary = previous assistant row).
                  turns.forEach((tu) => tu.rows.forEach((r) => { if (r.kind === 'tool') next.add(r.key); }));
                  setCollapsedCalls(next);
                }}>
                {allCallsCollapsed ? t('coding.trajExpandCalls') : t('coding.trajCollapseCalls')}
              </button>
              <input className="traj-ledger-search" type="search" value={search}
                onChange={(e) => setSearch(e.target.value)}
                placeholder={t('coding.trajSearch')} aria-label={t('coding.trajSearch')} />
              {modeSwitch}
            </div>
          </div>
          {q && (
            <div className="traj-ledger-hits">{t('coding.trajSearchHits', { n: totalRows })}</div>
          )}
          {/* DSH timeline minimap: three lanes (Input / Model / Tools), one
              bar per row, equal-width sequence positions; click scrolls. */}
          <div className="traj-timeline" aria-hidden="true">
            <div className="traj-timeline-labels">
              <span>{t('coding.trajLaneInput')}</span>
              <span>{t('coding.trajLaneModel')}</span>
              <span>{t('coding.trajLaneTools')}</span>
            </div>
            <div className="traj-timeline-track">
              {(() => {
                const flat = turns.flatMap((tu) => tu.rows).filter(matches);
                const laneOf = (k: TrajRow['kind']) =>
                  k === 'user' || k === 'context' ? 0 : k === 'tool' ? 2 : 1;
                return flat.map((r, i) => (
                  <span key={r.key}
                    className={`traj-tl-bar lane-${laneOf(r.kind)}${r.toolOk === false ? ' err' : ''}`}
                    style={{ left: `${flat.length > 1 ? (i / (flat.length - 1)) * 100 : 0}%` }}
                    title={`${kindLabel(r.kind)}${r.toolName && r.kind === 'tool' ? ` · ${r.toolName}` : ''} · ${fmtTime(r.timeMs)}`}
                    onClick={() => { setSelected(r.key); scrollToRow(r.key); }} />
                ));
              })()}
            </div>
          </div>
          <div className="traj-ledger-split">
            <div className="traj-ledger" role="table" aria-rowcount={totalRows}>
            {turns.map((turn, ti) => {
              const turnCollapsed = collapsedTurns.has(turn.key);
 // collapse: the turn folds to its FIRST row plus a "… N tool
              // calls" summary; search still applies within that.
              const visible = turnCollapsed
                ? turn.rows.filter((r) => matches(r) && r === turn.rows.find((x) => matches(x)))
                : turn.rows.filter(matches);
              const firstVisible = turn.rows.find((r) => matches(r));
              // Turn numbering counts only real turns; the preamble group
              // (SYSTEM/CONTEXT before the first user message) gets none.
              const userTurnNo = turns.slice(0, ti).filter((t2) => t2.hasUser).length + 1;
              const turnCalls = turn.rows.filter((r) => r.kind === 'tool');
              const foldedNames = Array.from(new Set(turnCalls.map((r) => r.toolName || 'tool'))).join(', ');
              return (
                <>
                  {visible.map((row, ri) => {
                    const isTurnStart = row.key === firstVisible?.key;
                    const callCollapsed = collapsedCalls.has(row.key);
                    const isOpen = expanded.has(row.key);
                    const toolsAfter = row.kind === 'assistant' ? toolRowsOf(turn, turn.rows.indexOf(row)) : [];
                    // "Collapsed" for the assistant's summary = every one of its
                    // following tool rows is folded.
                    const groupCollapsed = toolsAfter.length > 0 && toolsAfter.every((tr) => collapsedCalls.has(tr.key));
                    const hideToolRow = row.kind === 'tool' && callCollapsed;
                    if (hideToolRow) return null;
                    return (
                      <div key={row.key} className="traj-ledger-rowbox">
                        <div
                          className={`traj-ledger-row${isOpen || selected === row.key ? ' selected' : ''}`}
                          role="row"
                          data-kind={row.kind}
                          data-row-key={row.key}
                          data-turn-start={isTurnStart && turn.hasUser ? 'true' : undefined}
                          aria-expanded={isOpen}
                          onClick={() => { setSelected(row.key); setExpanded((prev) => {
                            const next = new Set(prev);
                            if (next.has(row.key)) next.delete(row.key); else next.add(row.key);
                            return next;
                          }); }}
                        >
                          <span className="traj-ledger-gutter">
                            {isTurnStart && turn.hasUser && <span className="traj-ledger-turnlabel">{t('coding.trajTurnShort', { n: userTurnNo })}</span>}
                            <span className={`traj-ledger-kind k-${row.kind}`}>{kindLabel(row.kind)}</span>
                          </span>
                          <span className="traj-ledger-content">
                            {row.kind === 'tool' ? (
                              <>
                                <span className="traj-ledger-toolname">{row.toolName}</span>
                                <span className="traj-ledger-payload">{ledgerPreview(row.toolInput)}</span>
                                <span className={`traj-ledger-arrow${row.toolOk === false ? ' err' : ''}`}>→</span>
                                <span className={`traj-ledger-result${row.toolOk === false ? ' err' : ''}${row.toolOutput == null ? ' none' : ''}`}>
                                  {row.toolOk === false ? '' : row.toolOutput == null ? t('coding.trajNoOutput') : ledgerPreview(row.toolOutput)}
                                  {row.toolOk === false ? '✗' : row.toolOk ? '✓' : ''}
                                </span>
                              </>
                            ) : (
                              <span className="traj-ledger-text" title={row.text}>{ledgerPreview(row.text)}</span>
                            )}
                          </span>
                        </div>
                        {/* DSH collapsed-calls summary row under the assistant */}
                        {row.kind === 'assistant' && groupCollapsed && (
                          <div className="traj-ledger-fold" role="row"
                            onClick={() => setCollapsedCalls((prev) => {
                              const next = new Set(prev);
                              toolsAfter.forEach((tr) => next.delete(tr.key));
                              return next;
                            })}>
                            … {t('coding.trajNCalls', { n: toolsAfter.length })} · {foldedNames}
                          </div>
                        )}
                        {isOpen && (
                          <div className="traj-ledger-detail" role="region">
                            {row.kind === 'tool' ? (
                              <>
                                {row.toolInput && <pre className="traj-code">{row.toolInput}</pre>}
                                {row.toolOutput != null && <pre className="traj-code traj-code-out">{row.toolOutput}</pre>}
                              </>
                            ) : (
                              <div className="traj-text">{row.text}</div>
                            )}
                          </div>
                        )}
                      </div>
                    );
                  })}
                  {turnCollapsed && firstVisible && turnCalls.length > 0 && (
                    <div className="traj-ledger-fold" role="row"
                      onClick={() => setCollapsedTurns((prev) => { const n2 = new Set(prev); n2.delete(turn.key); return n2; })}>
                      … {t('coding.trajNCalls', { n: turnCalls.length })} · {foldedNames}
                    </div>
                  )}
                </>
              );
            })}
            </div>
            {inspector}
          </div>
        </div>
    );
  }

  return (
    <div className="traj-view">
      <div className="traj-list-toolbar">{modeSwitch}</div>
      {(() => {
        const todo = latestTodoFromTexts(entries.map((e) => e.toolOutput ?? (e.kind === 'tool_result' ? e.text : null)));
        return todo ? <TodoCard todo={todo} /> : null;
      })()}
      {turns.map((turn, ti) => {
        const hasUser = turn.hasUser ?? true;
        const userTurnNo = turns.slice(0, ti).filter((t2) => (t2.hasUser ?? true)).length + 1;
        return (
        <div key={turn.key} className="traj-turn">
          {hasUser && (
          <div className="traj-turn-header">
            <span className="traj-turn-title">{t('coding.trajTurn', { n: userTurnNo })}</span>
            <span className="traj-turn-count">{turn.rows.length}</span>
          </div>
          )}
          {turn.rows.map((row) => (
            <details key={row.key} className="traj-row">
              <summary>
                <span className="disclosure-leading">
                  <span className="disclosure-icon" dangerouslySetInnerHTML={{ __html: kindIcon(row.kind, row.kind === 'tool' ? row.toolName : undefined, row.toolInput ?? undefined) }} />
                  <span className="disclosure-chevron" aria-hidden="true" />
                </span>
                <span className="traj-kind">{kindLabel(row.kind)}</span>
                {row.kind === 'tool' ? (
                  <span className="traj-preview traj-preview-tool">
                    {row.toolName}
                    {row.toolOk !== null && row.toolOk !== undefined && (
                      <span className={row.toolOk ? 'traj-ok' : 'traj-err'}>{row.toolOk ? ' ✓' : ' ✗'}</span>
                    )}
                  </span>
                ) : (
                  <span className="traj-preview">{preview(row.text)}</span>
                )}
              </summary>
              <div className="traj-detail">
                {row.kind === 'tool' ? (
                  (() => {
                    const node = renderToolNode({
                      toolName: row.toolName || '',
                      input: row.toolInput,
                      output: row.toolOutput,
                      ok: row.toolOk,
                    });
                    return node ?? (
                      <>
                        {row.toolInput && <pre className="traj-code">{row.toolInput}</pre>}
                        {row.toolOutput != null && <pre className="traj-code traj-code-out">{row.toolOutput}</pre>}
                      </>
                    );
                  })()
                ) : (
                  <div className="traj-text">{row.text}</div>
                )}
              </div>
            </details>
          ))}
        </div>
        );
      })}
    </div>
  );
}

/** Coding thinking row (scheme): one row per reasoning block. The leading
 *  16px slot morphs between the think icon and a down chevron (chevron on
 *  hover / persistent while open); the summary shows the last line with
 *  horizontal tail-following while streaming, freezing to the first line when
 *  done; a light band sweeps across the row while streaming. */
/** PTC `run_code` row body: renders the program's sub-calls as a structured
 *  step list (label, call, compact args) joined with the per-step status from
 *  the deterministic `[i/n] ok|ERR …` log the transport emits, so one
 *  run_code call reads as the nested dispatches it performed. Falls back to
 *  the raw payload blocks when the program does not parse. */
export function PtcProgramDetail({ input, output }: { input?: string | null; output?: string | null }) {
  const { t } = useI18n();
  const steps = React.useMemo(() => parsePtcSteps(input), [input]);
  const statuses = React.useMemo(() => parsePtcLog(output ?? undefined), [output]);
  if (!steps) {
    return (
      <>
        {input && <pre className="traj-code">{input}</pre>}
        {output != null && <pre className="traj-code traj-code-out">{output}</pre>}
      </>
    );
  }
  return (
    <div className="ptc-program">
      <div className="ptc-program-title">{t('coding.ptcProgram', { n: steps.length })}</div>
      <ol className="ptc-steps">
        {steps.map((step, i) => {
          const st = statuses[i];
          return (
            <li key={i} className={st && !st.ok ? 'ptc-step ptc-step-err' : 'ptc-step'}>
              <span className="ptc-step-call">{step.as ? `${step.as}: ` : ''}{step.call}</span>
              <code className="ptc-step-args">{step.argsPreview}</code>
              {st && <span className={st.ok ? 'traj-ok' : 'traj-err'}>{st.ok ? ' ✓' : ' ✗'}</span>}
            </li>
          );
        })}
      </ol>
      {output != null && <pre className="traj-code traj-code-out">{output}</pre>}
    </div>
  );
}

function parsePtcSteps(input?: string | null): { as?: string; call: string; argsPreview: string }[] | null {
  if (!input) return null;
  try {
    const args = JSON.parse(input);
    const program = typeof args?.program === 'string' ? JSON.parse(args.program) : args?.program;
    const steps = program?.steps;
    if (!Array.isArray(steps) || steps.length === 0) return null;
    return steps.map((s: any) => ({
      as: typeof s?.as === 'string' ? s.as : undefined,
      call: typeof s?.call === 'string' ? s.call : '?',
      argsPreview: compactArgsPreview(s?.args),
    }));
  } catch {
    return null;
  }
}

function compactArgsPreview(args: any): string {
  if (args == null) return '';
  try {
    const text = JSON.stringify(args);
    const cps = Array.from(text);
    return cps.length > 96 ? `${cps.slice(0, 96).join('')}…` : text;
  } catch {
    return '';
  }
}

function parsePtcLog(output?: string): { ok: boolean }[] {
  if (!output) return [];
  const out: { ok: boolean }[] = [];
  for (const line of output.split('\n')) {
    const m = /^\[\d+\/\d+\] (ok|ERR) /.exec(line);
    if (m) out.push({ ok: m[1] === 'ok' });
  }
  return out;
}
/** Session todo state parsed from the latest `## TODO` rendering. todo_write
 *  renders the list into its tool result — direct calls AND run_code sub-call
 *  logs both land in tool output text, so the card parses text, not events. */
export function parseTodoState(text?: string | null): { items: { done: boolean; inProgress: boolean; title: string }[] } | null {
  if (!text) return null;
  const marker = text.indexOf('## TODO');
  if (marker < 0) return null;
  // Sub-call log previews flatten newlines; normalize so both shapes parse.
  const flat = text.slice(marker + 7).replace(/\s+/g, ' ').trim();
  const items: { done: boolean; inProgress: boolean; title: string }[] = [];
  // Markdown checkboxes accept both [x] and [X] (BUG-83).
  const itemRe = /-\s\[([xX\- ])\]\s*([\s\S]*?)(?=\s-\s\[[xX\- ]\]|$)/g;
  let match: RegExpExecArray | null;
  while ((match = itemRe.exec(flat)) !== null) {
    const title = match[2].trim();
    if (title) items.push({ done: match[1] === 'x' || match[1] === 'X', inProgress: match[1] === '-', title });
  }
  return items.length > 0 ? { items } : null;
}

export function latestTodoFromTexts(texts: (string | null | undefined)[]) {
  for (let i = texts.length - 1; i >= 0; i--) {
    const parsed = parseTodoState(texts[i]);
    if (parsed) return parsed;
  }
  return null;
}

/** Session todo card: mirrors the model's latest todo_write state so progress
 *  is visible without opening tool rows — including run_code sub-calls. */
export function TodoCard({ todo }: { todo: { items: { done: boolean; inProgress: boolean; title: string }[] } }) {
  const { t } = useI18n();
  const done = todo.items.filter((item) => item.done).length;
  return (
    <div className="todo-card">
      <div className="todo-card-title">{t('coding.todoCard', { done, n: todo.items.length })}</div>
      <ul className="todo-card-items">
        {todo.items.map((item, i) => (
          <li key={i} className={item.done ? 'todo-item done' : item.inProgress ? 'todo-item in-progress' : 'todo-item'}>
            <span className="todo-marker">{item.done ? '✓' : item.inProgress ? '▸' : '○'}</span>
            <span className="todo-item-title">{item.title}</span>
          </li>
        ))}
      </ul>
    </div>
  );
}

// ── built-in conversation node contributions ──
// The run_code program list and the todo card render via the registry; third-party plugins contribute through the same path.
registerConversationNode({
  owner: 'builtin',
  id: 'run_code-program',
  toolName: 'run_code',
  render: ({ input, output }) => (
    <PtcProgramDetail input={input} output={output} />
  ),
});
registerConversationNode({
  owner: 'builtin',
  id: 'todo_write-card',
  toolName: 'todo_write',
  render: ({ output }) => {
    const todo = parseTodoState(output);
    return todo ? <TodoCard todo={todo} /> : null;
  },
});

/** Workspace picker for the New Task screen: sits LEFT of the execution-mode
 *  picker above the input and picks the directory the new task will run in.
 *  Selecting goes through the registry activation the sidebar uses, so the
 *  status bar / git chip / session list follow along; the menu's last row
 *  opens the shared add-workspace dialog. Chat mode (no onNew, noneLabel set)
 *  swaps that row for an explicit "not in a workspace" choice and shows a
 *  placeholder until a workspace is picked. */
export function WorkspacePicker({ workspaces, activeId, onSelect, onNew, noneLabel, placeholderLabel, ariaLabel = 'workspace' }: {
  workspaces: WorkspaceInfo[];
  activeId: string | null;
  onSelect: (id: string) => void;
  onNew?: () => void;
  /** Label for the explicit empty choice (activeId === ''). */
  noneLabel?: string;
  /** Shown when activeId is null and no workspace matches. */
  placeholderLabel?: string;
  ariaLabel?: string;
}) {
  const { t } = useI18n();
  const [open, setOpen] = useState(false);
  const wrapRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!open) return;
    const close = () => setOpen(false);
    const onDown = (e: MouseEvent) => {
      if (wrapRef.current && wrapRef.current.contains(e.target as Node)) return;
      close();
    };
    const onKey = (e: KeyboardEvent) => { if (e.key === 'Escape') close(); };
    document.addEventListener('mousedown', onDown);
    document.addEventListener('keydown', onKey);
    return () => { document.removeEventListener('mousedown', onDown); document.removeEventListener('keydown', onKey); };
  }, [open]);
  const active = workspaces.find(w => w.id === activeId) ?? null;
  const label = active ? active.title
    : activeId === '' && noneLabel ? noneLabel
    : placeholderLabel ?? t('coding.workspacePickerLabel');
  return (
    <div className="composer-approval-wrap" ref={wrapRef}>
      <button
        type="button"
        className="composer-approval-btn"
        aria-label={ariaLabel}
        aria-expanded={open}
        onClick={() => setOpen(o => !o)}
      >
        <span className="composer-approval-icon" dangerouslySetInnerHTML={{ __html: ICON_WORKSPACE_FOLDER }} />
        <span className="composer-approval-label">{label}</span>
        <span className="composer-approval-caret" aria-hidden="true" />
      </button>
      {open && (
        <div className="composer-approval-menu workspace-picker-menu">
          {workspaces.map(w => (
            <button
              key={w.id}
              type="button"
              className={`composer-approval-item${w.id === activeId ? ' active' : ''}`}
              title={w.path}
              onClick={() => { setOpen(false); if (w.id !== activeId) onSelect(w.id); }}
            >
              <span className="composer-approval-icon" dangerouslySetInnerHTML={{ __html: ICON_WORKSPACE_FOLDER }} />
              <span className="composer-approval-label">{w.title}</span>
              {w.id === activeId && <span className="composer-approval-check">✓</span>}
            </button>
          ))}
          {noneLabel !== undefined && (
            <button
              type="button"
              className={`composer-approval-item workspace-picker-none${activeId === '' ? ' active' : ''}`}
              onClick={() => { setOpen(false); if (activeId !== '') onSelect(''); }}
            >
              <span className="composer-approval-icon" dangerouslySetInnerHTML={{ __html: '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" stroke-dasharray="4 3"><rect x="3" y="3" width="18" height="18" rx="3"/></svg>' }} />
              <span className="composer-approval-label">{noneLabel}</span>
              {activeId === '' && <span className="composer-approval-check">✓</span>}
            </button>
          )}
          {onNew && (
            <button
              type="button"
              className="composer-approval-item workspace-picker-new"
              onClick={() => { setOpen(false); onNew(); }}
            >
              <span className="composer-approval-icon" dangerouslySetInnerHTML={{ __html: ICON_WORKSPACE_FOLDER }} />
              <span className="composer-approval-label">＋ {t('coding.workspaceNew')}</span>
            </button>
          )}
        </div>
      )}
    </div>
  );
}

/** Execution-mode picker for new sessions: approval-mode look — inline icon +
 *  current value + a caret hugging the title, borderless transparent button,
 *  upward menu with a check on the selected item. */
export function ExecutionModePicker({ value, onChange, disabled }: {
  value: string | null;
  onChange: (v: string | null) => void;
  disabled?: boolean;
}) {
  const { t } = useI18n();
  const [open, setOpen] = useState(false);
  const wrapRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!open) return;
    const close = () => setOpen(false);
    const onDown = (e: MouseEvent) => {
      if (wrapRef.current && wrapRef.current.contains(e.target as Node)) return;
      close();
    };
    const onKey = (e: KeyboardEvent) => { if (e.key === 'Escape') close(); };
    document.addEventListener('mousedown', onDown);
    document.addEventListener('keydown', onKey);
    return () => { document.removeEventListener('mousedown', onDown); document.removeEventListener('keydown', onKey); };
  }, [open]);
  const options = [
    { value: '', label: t('coding.executionStandard'), icon: EXEC_MODE_ICONS.standard },
    { value: 'plan', label: t('coding.executionPlan'), icon: EXEC_MODE_ICONS.plan },
    { value: 'ptc', label: t('coding.executionPtc'), icon: EXEC_MODE_ICONS.ptc },
    { value: 'goal', label: t('coding.executionGoal'), icon: EXEC_MODE_ICONS.goal },
  ];
  const current = options.find(o => o.value === (value ?? '')) ?? options[0];
  return (
    <div className="composer-approval-wrap" ref={wrapRef}>
      <button
        type="button"
        className="composer-approval-btn"
        aria-label="execution-mode"
        aria-expanded={open}
        disabled={disabled}
        onClick={() => setOpen(o => !o)}
      >
        <span className="composer-approval-icon" dangerouslySetInnerHTML={{ __html: current.icon }} />
        <span className="composer-approval-label">{current.label}</span>
        <span className="composer-approval-caret" aria-hidden="true" />
      </button>
      {open && (
        <div className="composer-approval-menu">
          {options.map(o => (
            <button
              key={o.value}
              type="button"
              className={`composer-approval-item${o.value === (value ?? '') ? ' active' : ''}`}
              onClick={() => { setOpen(false); if (o.value !== (value ?? '')) onChange(o.value || null); }}
            >
              <span className="composer-approval-icon" dangerouslySetInnerHTML={{ __html: o.icon }} />
              <span className="composer-approval-label">{o.label}</span>
              {o.value === (value ?? '') && <span className="composer-approval-check">✓</span>}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}

/** New Task mode switch icons — fixed Coding / Chat identities, not roster
 *  agent names. */
function AgentPresetIcon({ name }: { name: 'Coding' | 'Chat' }) {
  const common = {
    viewBox: '0 0 16 16',
    fill: 'none',
    stroke: 'currentColor',
    strokeWidth: 1.6,
    strokeLinecap: 'round' as const,
    strokeLinejoin: 'round' as const,
    'aria-hidden': true,
  };
  if (name === 'Coding') {
    // Rounded-square logo with code brackets inside.
    return (
      <svg {...common}>
        <rect x="2" y="2" width="12" height="12" rx="3.2" />
        <path d="M6.9 6.3 5.2 8l1.7 1.7M9.1 6.3 10.8 8l-1.7 1.7" />
      </svg>
    );
  }
  return (
    <svg {...common}>
      <path d="M13.5 8a5.5 5.5 0 0 1-8.1 4.84L2.5 13.5l.7-2.8A5.5 5.5 0 1 1 13.5 8Z" />
    </svg>
  );
}

/** New Task mode switch: a fixed two-slot segmented bar — Coding and Chat
 *  always sit in the same place with the same labels, regardless of the agent
 *  roster. Chat maps to the built-in Chat Agent; Coding keeps the staged
 *  preset ride (WI-5) with the roster default. */
export function AgentPresetPicker({ mode, onChange, disabled }: {
  mode: 'coding' | 'chat';
  onChange: (m: 'coding' | 'chat') => void;
  disabled?: boolean;
}) {
  return (
    <div className="agent-seg" role="group" aria-label="agent-preset">
      <button
        type="button"
        disabled={disabled}
        aria-pressed={mode === 'coding'}
        className={`agent-seg-pill${mode === 'coding' ? ' active' : ''}`}
        onClick={() => { if (mode !== 'coding') onChange('coding'); }}
      >
        <AgentPresetIcon name="Coding" />
        <span>Coding</span>
      </button>
      <button
        type="button"
        disabled={disabled}
        aria-pressed={mode === 'chat'}
        className={`agent-seg-pill${mode === 'chat' ? ' active' : ''}`}
        onClick={() => { if (mode !== 'chat') onChange('chat'); }}
      >
        <AgentPresetIcon name="Chat" />
        <span>Chat</span>
      </button>
    </div>
  );
}

export function ThinkingRow({ m }: { m: Message }) {
  const { t } = useI18n();
  const text = m.content || '';
  const lines = text.split('\n').map(l => l.trim()).filter(Boolean);
  const summary = m.streaming
    ? (lines[lines.length - 1] ?? '')
    : (lines[0] ?? '');
  const summaryRef = useRef<HTMLSpanElement>(null);
 // Summary tail-follows horizontally: the text scrolls left like
  // a ticker, always showing the newest content.
  useEffect(() => {
    const el = summaryRef.current;
    if (el && m.streaming) el.scrollLeft = el.scrollWidth - el.clientWidth;
  }, [summary, m.streaming]);
  return (
    <details className="thinking-row-details">
      <summary data-state={m.streaming ? 'running' : 'ok'}>
        <span className="disclosure-leading">
          <span className="disclosure-icon" dangerouslySetInnerHTML={{ __html: ICON_THINKING }} />
          <span className="disclosure-chevron" aria-hidden="true" />
        </span>
        <span className="thinking-row-title">{t('coding.think')}</span>
        <span className="thinking-row-sep" aria-hidden />
        <span ref={summaryRef} className={m.streaming ? 'thinking-row-live thinking-row-summary' : 'thinking-row-summary'}>
          {summary || (m.streaming ? '…' : '')}
        </span>
      </summary>
      <div className="thinking-row-body">{text}</div>
    </details>
  );
}

export function ToolRow({
  content, result, ok, streaming, collapsible, onOpenDiff, durationMs,
}: {
  content: string;
  result?: string;
  ok?: boolean | null;
  streaming?: boolean;
  collapsible?: boolean;
  /** WI-86: wall time (ms) recorded from toolCall → toolResult. */
  durationMs?: number;
  /** Diff-bearing results (edit/write/patch): open the file diff in the
      right-hand panel (审查). */
  onOpenDiff?: (path: string, text: string) => void;
}) {
  const { t } = useI18n();
  const labeled = content.startsWith('🔧');
  const icon = toolIconFor(toolNameOf(content), content);
  const raw = labeled ? content.replace(/^🔧\s*/, '') : content;

  // Code output that starts on the first line — e.g. a stored tool result
  // "✓ <?xml version="1.0"?>\n<project>…" puts the declaration right after the
  // status marker. Detect code on the WHOLE content so the first line becomes part
  // of the code block instead of a separate label row.
  const statusLead = raw.match(/^([✓✗])\s*/);
  const whole = raw.replace(/^[✓✗]\s*/, '').trim();
  const wholeLang = detectOutputLanguage(whole);
  if (wholeLang) {
    const wholeOk = statusLead ? statusLead[1] === '✓' : (ok ?? null);
    const wholeHtml = highlightCodeHtml(whole, wholeLang);
    if (collapsible) {
      const wholeFlat = whole.replace(/\s+/g, ' ').trim();
      const wholeSummary = truncateOneLine(whole, 64);
      return (
        <div className="tool-row">
          <div className="tool-row-text">
            <details className="tool-row-details" open={!!streaming}>
              <summary
                className="tool-row-summary"
                data-tip={wholeFlat !== wholeSummary ? wholeFlat : undefined}
              >
                <span className="disclosure-leading">
                  {labeled && <span className="disclosure-icon" dangerouslySetInnerHTML={{ __html: icon }} />}
                  <span className="disclosure-chevron" aria-hidden="true" />
                </span>
                <span className="tool-row-title">{wholeLang.toUpperCase()}</span>
                <span className="tool-row-summary-text">{wholeSummary}</span>
              </summary>
              <div className="tool-row-detail">
                <div className="tool-row-result">
                  <div className="tool-row-result-head">
                    {wholeOk !== null && <StatusIcon ok={wholeOk} />}
                    <span>{wholeOk === false ? t('tool.failed') : t('tool.result')}</span>
                    <span className="tool-row-head-copy"><CopyChip text={whole} /></span>
                  </div>
                  <div
                    className="tool-row-result-body"
                    dangerouslySetInnerHTML={wholeHtml !== null ? { __html: wholeHtml } : undefined}
                  >
                    {wholeHtml === null ? whole : undefined}
                  </div>
                </div>
              </div>
            </details>
          </div>
        </div>
      );
    }
    return (
      <div className="tool-row">
        {labeled && <span className="tool-row-icon" dangerouslySetInnerHTML={{ __html: icon }} />}
        <div className="tool-row-text">
          <div className="tool-row-code-wrap">
            {statusLead?.[1] === '✗' && <StatusIcon ok={false} />}
            <CodeBlock lang={wholeLang} code={whole} />
          </div>
        </div>
      </div>
    );
  }

  const nl = raw.indexOf('\n');
  let label = nl === -1 ? raw : raw.slice(0, nl);
  let body = nl === -1 ? '' : raw.slice(nl + 1).trim();

  // Locate the ✓/✗ status marker so it can be swapped for a badge.
  // Live rows carry it at the label end ("bash: git commit ✓"); bare fallback rows
  // carry it at the label start ("✓ created pom.xml"); stored rows on the body
  // ("\n✓ created /path/file.java"). Errors append text after the ✗, so an
  // unmatched ✗ mid-label stays as text.
  let labelOk: boolean | null = null;
  let bodyOk: boolean | null = null;
  const tail = label.match(/\s*([✓✗])$/);
  if (tail) { labelOk = tail[1] === '✓'; label = label.replace(/\s*[✓✗]$/, ''); }
  else {
    // Bare fallback rows carry the marker at the start ("✓ created pom.xml").
    const lead = label.match(/^([✓✗])\s*/);
    if (lead) { labelOk = lead[1] === '✓'; label = label.replace(/^[✓✗]\s*/, ''); }
    else {
      // Live failures append " ✗ {error}" mid-label ("read: x ✗ ENOENT: no such
      // file"); split so the error reads below the label beside the red badge.
      const fm = label.match(/\s✗\s+/);
      if (fm) {
        const err = label.slice(fm.index + fm[0].length);
        label = label.slice(0, fm.index);
        if (err) { body = err; bodyOk = false; }
      }
    }
  }
  const bm = body.match(/^([✓✗])\s*/);
  if (bm) { bodyOk = bm[1] === '✓'; body = body.replace(/^[✓✗]\s*/, ''); }
  // Remember whether the body was a marker-prefixed stored output ("✓ …"): such a
  // body is output below the command block, never a command continuation.
  const bodyIsStoredOutput = bm !== null;

  // Command tools ("bash: git commit") split into a bare tool label and the command
  // itself, which renders as its own bash code block.
  const isCmd = COMMAND_TOOLS.has(toolNameOf(content));
  let command: string | null = null;
  if (isCmd) {
    const cm = label.match(/^[^:]+:\s*(.+)$/);
    if (cm) {
      command = cm[1];
      label = label.slice(0, label.indexOf(':') + 1);
      // Multiline commands: the rest of the row is the command, not separate prose.
      // Stored tool output is prefixed "✓ "/"✗ " (history reconstruction), so a body
      // starting with the marker is output and stays below the command block.
      if (body && !bodyIsStoredOutput) {
        const bt = body.match(/\s*([✓✗])$/);
        if (bt) { bodyOk = bt[1] === '✓'; body = body.replace(/\s*[✓✗]$/, ''); }
        if (body) { command += '\n' + body; body = ''; }
      }
    } else if (label.endsWith(':')) {
      // "bash:" alone on the first line — the command starts on the next line.
      if (body && !bodyIsStoredOutput) { command = body; body = ''; }
    }
  }

  // Detect code by content first (XML/JSON), then by the file extension in the
  // label — but only when the body actually looks like code, so a one-line status
  // ("✓ created File.java") doesn't get wrapped in a bogus code block.
  const lang = detectOutputLanguage(body) || (looksLikeCode(body) ? langFromLabel(label) : null);

  // ── Card data (collapsible mode) ──
  const tname = toolNameOf(content);
  const title = TOOL_TITLES[tname] || (
    tname && /^[a-zA-Z]/.test(tname) ? tname[0].toUpperCase() + tname.slice(1) : 'Tool'
  );
  // For shell tools the command is the whole command; for other tools it is the
  // parameter after "tool:" (path/query/url…), or the label itself as a fallback.
  const cmdText = command ?? (/^[A-Za-z][\w.-]*:/.test(label) ? label.replace(/^[^:]+:\s*/, '') : label);
  const unescaped = (s: string) => s.replace(/\\n/g, '\n').replace(/\\r/g, '\r');
  // WI-150: no character cap here — .tool-row-summary-text ellipsizes via CSS,
  // so wide windows show more of the command and narrow ones truncate cleanly.
  const summaryFull = (cmdText || label || whole).replace(/\s+/g, ' ').trim();
  const finalOk = ok ?? (result !== undefined ? null : (labelOk ?? bodyOk));
  const resultText = result !== undefined ? result : body;
  // 文件内容型工具(read/write/edit/patch)的结果体按路径扩展名高亮(2026-10-04 方案补充)。
  const resultHtml = isDiffContent(resultText) ? null : highlightCodeHtml(resultText, fileResultLang(tname, cmdText));
  const hasResult = result !== undefined || bodyOk !== null || body.length > 0 ||
    labelOk !== null || streaming;

  return (
    <div className="tool-row">
      {/* Collapsible cards carry the icon inside the summary header; the outer
          icon is only for the compact non-collapsible layout (Chat). */}
      {!collapsible && labeled && <span className="tool-row-icon" dangerouslySetInnerHTML={{ __html: icon }} />}
      <div className="tool-row-text">
        {collapsible ? (
          <details className="tool-row-details" open={!!streaming}>
            <summary className="tool-row-summary" title={summaryFull}>
              <span className="disclosure-leading">
                {labeled && <span className="disclosure-icon" dangerouslySetInnerHTML={{ __html: icon }} />}
                <span className="disclosure-chevron" aria-hidden="true" />
              </span>
              <span className="tool-row-title">{title}</span>
              <span className="tool-row-summary-text">{summaryFull}</span>
            </summary>
            <div className="tool-row-detail">
              {cmdText && (
                <CodeBlock lang={COMMAND_TOOLS.has(tname) ? 'bash' : (lang || 'text')} code={unescaped(cmdText)} />
              )}
              {hasResult && (
                <div className={`tool-row-result${finalOk === false ? ' is-err' : ''}`}>
                  <div className="tool-row-result-head">
                    {finalOk !== null && <StatusIcon ok={finalOk} />}
                    <span>{finalOk === false ? t('tool.failed') : (finalOk === true ? t('tool.success') : t('tool.result'))}</span>
                    {durationMs != null && (
                      <span className="tool-duration" data-testid="tool-duration">
                        · {durationMs < 1000 ? `${durationMs}ms` : `${(durationMs / 1000).toFixed(1)}s`}
                      </span>
                    )}
                    {onOpenDiff && isDiffContent(resultText) && (() => {
                      const first = parseDiff(resultText)[0];
                      if (!first) return null;
                      return (
                        <button
                          type="button"
                          className="tool-row-open-panel"
                          onClick={() => onOpenDiff(first.path, resultText)}
                        >
                          {t('coding.turnFileReview')}
                        </button>
                      );
                    })()}
                    <span className="tool-row-head-copy"><CopyChip text={resultText || ''} /></span>
                  </div>
                  <div
                    className="tool-row-result-body"
                    dangerouslySetInnerHTML={resultHtml !== null ? { __html: resultHtml } : undefined}
                  >
                    {resultHtml === null
                      ? (isDiffContent(resultText) ? <DiffView text={resultText} /> : (resultText || (streaming ? '(processing...)' : '')))
                      : undefined}
                  </div>
                </div>
              )}
            </div>
          </details>
        ) : (
          <>
            {label && <div className="tool-row-label">{label}{labelOk === false && <StatusIcon ok={labelOk} />}</div>}
            {command !== null &&
              <CodeBlock lang="bash" code={command.replace(/\\n/g, '\n').replace(/\\r/g, '\r')} />}
            {lang ? (
              <div className="tool-row-code-wrap">
                {bodyOk === false && <StatusIcon ok={bodyOk} />}
                {(() => {
                  const html = highlightCodeHtml(body, lang);
                  return html !== null
                    ? <pre className="tool-row-code" data-lang={lang} dangerouslySetInnerHTML={{ __html: html }} />
                    : <pre className="tool-row-code" data-lang={lang}>{body}</pre>;
                })()}
              </div>
            ) : (
              <span className="tool-row-status">
                {bodyOk === false && <StatusIcon ok={bodyOk} />}
                {body || (streaming && !label && command === null ? '(processing...)' : '')}
              </span>
            )}
          </>
        )}
      </div>
    </div>
  );
}

/** Composer control-row select with the drawn angle-bracket caret (wrapper
 *  span because <select> cannot contain child elements). */
/** Memoized one-message row (ChatTab): streaming deltas re-render only the
 *  updated message — untouched rows keep object identity and skip re-render
 * (streaming-tail isolation: the live tail moves, frozen history
 *  doesn't). Exported for the ChatTab thinking-row regression test. */
export const MessageRow = memo(function MessageRow({ m }: { m: Message }) {
  const { t } = useI18n();
  return (
    <div className={`message ${m.role}`}>
      <div className="bubble">
        {m.role === 'compaction' ? (
          <CompactionRow summary={m.content} />
        ) : m.role === 'thinking' ? (
          <ThinkingRow m={m} />
        ) : m.role === 'assistant' && m.streaming && !m.content ? (
          <span className="thinking-indicator">
            <span className="thinking-dot" />
            <span className="thinking-dot" />
            <span className="thinking-dot" />
            <span style={{ marginLeft: 6, color: 'var(--text-muted)' }}>{t('common.thinkingDots')}</span>
          </span>
        ) : m.role === 'assistant' ? (
          <AssistantContent content={m.content || ''} />
        ) : m.role === 'tool' ? (
          <ToolRow content={m.content || ''} streaming={m.streaming} />
        ) : (
          m.content
        )}
        {m.streaming && m.role !== 'thinking' && m.content && <span className="cursor-blink">|</span>}
      </div>
    </div>
  );
});

/** Partition the flat message list into render segments: consecutive
 * tool/thinking runs of ≥2 become one step group (turn-process parity
 *  "N tool calls · names" fold), everything else stays standalone. Runs of 1
 *  keep the plain row. */
export function groupProcessRows(
  messages: Message[],
): Array<{ kind: 'single'; m: Message } | { kind: 'group'; rows: Message[] }> {
  const out: Array<{ kind: 'single'; m: Message } | { kind: 'group'; rows: Message[] }> = [];
  let bucket: Message[] = [];
  const flush = () => {
    if (bucket.length >= 2) out.push({ kind: 'group', rows: bucket });
    else if (bucket.length === 1) out.push({ kind: 'single', m: bucket[0] });
    bucket = [];
  };
  for (const m of messages) {
    if (m.role === 'tool' || m.role === 'thinking') {
      bucket.push(m);
      continue;
    }
    flush();
    out.push({ kind: 'single', m });
  }
  flush();
  return out;
}

/** Foldable process group (TurnProcessNodeView parity): auto-open while
 *  the turn is live, auto-collapse when it lands; a manual toggle wins until
 *  the next auto transition. The custom memo comparator keeps the group off
 *  the render path while its members are untouched (streaming-tail
 *  isolation). */
const StepGroup = memo(function StepGroup({ rows, live }: { rows: Message[]; live: boolean }) {
  const { t } = useI18n();
  const [manual, setManual] = useState<boolean | null>(null);
  const open = manual ?? live;
  const toolCount = rows.filter((r) => r.role === 'tool').length;
  const names = [...new Set(rows.map((r) => r.toolName).filter(Boolean))] as string[];
  const namesLabel =
    names.slice(0, 3).join(', ') + (names.length > 3 ? ` +${names.length - 3}` : '');
  return (
    <div className="step-group" data-live={live ? 'true' : undefined}>
      <details
        open={open}
        onToggle={(e) => setManual((e.currentTarget as HTMLDetailsElement).open)}
      >
        <summary className="step-group-summary">
          <span className="step-group-count">
            {t('chat.stepGroup', { count: toolCount, names: namesLabel })}
          </span>
          {live && <span className="step-group-live" aria-hidden>⋯</span>}
        </summary>
        <div className="step-group-rows">
          {rows.map((r) => (
            <MessageRow key={r.id} m={r} />
          ))}
        </div>
      </details>
    </div>
  );
}, (prev, next) =>
  prev.live === next.live &&
  prev.rows.length === next.rows.length &&
  prev.rows.every((r, i) => r === next.rows[i])
);

export function MiniSelect(props: React.SelectHTMLAttributes<HTMLSelectElement>) {
  return (
    <span className="composer-mini-wrap">
      <select className="composer-mini-select" {...props} />
      <span className="composer-mini-caret" aria-hidden="true" />
    </span>
  );
}

/* Plan review card — three-step takeover at the composer's bottom edge
   (design approved 2026-09-13, bottom-card form, Manual default):
   step "review": plan summary (markdown, scroll-capped; the full plan lives in
     the right-hand Plan panel), a feedback line, 继续规划 / 批准计划. Enter
     with text keeps planning with that feedback; empty Enter opens step 2.
   step "mode": the four permission presets — same vocabulary as the composer's
     permission dropdown — with Manual preselected. 开始实施 hands the preset
     to onApplyPreset; the host writes it BEFORE the plan decision. Full
     access routes through step "risk" first.
   step "risk": the existing risk-confirmation copy + acknowledge checkbox
     gating 开启并实施 (danger preset cannot be enabled blind). */
const PLAN_REVIEW_MODES: { value: string; labelKey: string; descKey: string; danger?: boolean }[] = [
  { value: 'manual', labelKey: 'common.permissionManual', descKey: 'coding.planReviewModeDescManual' },
  { value: 'accept-edits', labelKey: 'common.permissionAcceptEdits', descKey: 'coding.planReviewModeDescAcceptEdits' },
  { value: 'auto', labelKey: 'common.permissionAuto', descKey: 'coding.planReviewModeDescAuto' },
  { value: 'danger-full-access', labelKey: 'common.permissionFullAccess', descKey: 'coding.planReviewModeDescFull', danger: true },
];

/** WI-96: structured review findings — a checklist card with priority
 *  badges, confidence, and file:line (clickable to open the diff panel). */
export function FindingsCard({ findings, onOpen }: {
  findings: { title: string; body: string; priority: number; confidence: number; file: string; lineStart: number; lineEnd: number }[];
  onOpen?: (file: string) => void;
}) {
  const { t } = useI18n();
  const [checked, setChecked] = useState<Set<number>>(new Set());
  if (findings.length === 0) {
    return <div className="findings-card empty" data-testid="findings-empty">{t('coding.findingsNone')}</div>;
  }
  const prioLabel = (p: number) => p === 0 ? 'P0' : p === 1 ? 'P1' : 'P2';
  const prioClass = (p: number) => p === 0 ? 'p0' : p === 1 ? 'p1' : 'p2';
  return (
    <div className="findings-card" data-testid="findings-card">
      <div className="findings-head">
        <span>🔍 {t('coding.findingsTitle')}</span>
        <span className="findings-count">{findings.length}</span>
      </div>
      {findings.map((f, i) => (
        <div key={i} className={`finding-row${checked.has(i) ? ' checked' : ''}`} data-testid="finding-row">
          <label className="finding-check">
            <input
              type="checkbox"
              checked={checked.has(i)}
              onChange={(e) => {
                setChecked(prev => {
                  const next = new Set(prev);
                  if (e.target.checked) next.add(i); else next.delete(i);
                  return next;
                });
              }}
            />
            <span className={`finding-prio ${prioClass(f.priority)}`}>{prioLabel(f.priority)}</span>
            <span className="finding-title">{f.title}</span>
            <span className="finding-confidence">{Math.round(f.confidence * 100)}%</span>
          </label>
          {f.body && <div className="finding-body">{f.body}</div>}
          {f.file && (
            <button
              type="button"
              className="finding-location"
              onClick={() => onOpen?.(f.file)}
            >📄 {f.file}{f.lineStart > 0 ? `:${f.lineStart}${f.lineEnd > f.lineStart ? `-${f.lineEnd}` : ''}` : ''}</button>
          )}
        </div>
      ))}
    </div>
  );
}

/** WI-87: plan text as a status checklist — `- [ ]`/`- [x]` (case-insensitive
 *  x) lines become todo-marker items; non-list lines keep markdown rendering. */
export function PlanChecklist({ source }: { source: string }) {
  const lines = source.split('\n');
  const items: { done: boolean; title: string }[] = [];
  for (const line of lines) {
    const m = line.match(/^\s*[-*]\s\[([ xX])\]\s*(.*)$/);
    if (m) items.push({ done: m[1].toLowerCase() === 'x', title: m[2] });
  }
  if (items.length === 0) return <MarkdownContent source={source} />;
  // Split the source into markdown segments and checklist blocks, keeping
  // ordering; consecutive checklist lines group into one <ul>.
  const out: React.ReactNode[] = [];
  let md: string[] = [];
  let list: { done: boolean; title: string }[] = [];
  const flushMd = () => {
    if (md.length) { out.push(<MarkdownContent key={`md${out.length}`} source={md.join('\n')} />); md = []; }
  };
  const flushList = () => {
    if (list.length) {
      out.push(
        <ul className="todo-card-items plan-checklist" key={`ul${out.length}`} data-testid="plan-checklist">
          {list.map((it, i) => (
            <li key={i} className={it.done ? 'todo-item done' : 'todo-item'}>
              <span className="todo-marker">{it.done ? '✓' : '○'}</span>
              <span className="todo-item-title">{it.title}</span>
            </li>
          ))}
        </ul>
      );
      list = [];
    }
  };
  for (const line of lines) {
    const m = line.match(/^\s*[-*]\s\[([ xX])\]\s*(.*)$/);
    if (m) { flushMd(); list.push({ done: m[1].toLowerCase() === 'x', title: m[2] }); }
    else { flushList(); md.push(line); }
  }
  flushMd();
  flushList();
  return <>{out}</>;
}

export function PlanReviewCard({ plan, currentPreset, onApplyPreset, onDecide }: {
  plan: string;
  /** The session's folded permission preset — preselected on the mode step so
   *  re-approving a plan never silently downgrades the session (2026-10-14:
   *  the card hardcoded `manual`, the user clicked through to 标准, and the
   *  write downgraded a danger-full-access session to workspace-write, which
   *  then refused out-of-workspace edits mid-task). */
  currentPreset?: string;
  /** Approve + apply the chosen permission preset; the host writes the
      preset first and only then sends the approval decision. */
  onApplyPreset: (preset: string) => void;
  /** Keep planning, optionally with typed feedback. */
  onDecide: (feedback?: string) => void;
}) {
  const { t } = useI18n();
  const [step, setStep] = useState<'review' | 'mode' | 'risk'>('review');
  const [mode, setMode] = useState(
    currentPreset && PLAN_REVIEW_MODES.some((m) => m.value === currentPreset)
      ? currentPreset
      : 'manual',
  );
  const [draft, setDraft] = useState('');
  const [riskChecked, setRiskChecked] = useState(false);

  if (step === 'review') {
    const text = draft.trim();
    return (
      <div className="plan-review" data-testid="plan-review-card" data-step="review">
        <div className="plan-review-head">
          <span className="plan-review-badge">{t('coding.planReviewBadge')}</span>
          <span className="plan-review-title">{t('coding.planReviewTitle')}</span>
          <span className="plan-review-step">{t('coding.planReviewStepReview')}</span>
        </div>
        {plan.trim() !== '' && (
          <div className="plan-review-summary" data-testid="plan-review-summary">
            <PlanChecklist source={plan} />
          </div>
        )}
        <span className="plan-review-fulllink">{t('coding.planReviewFullPlan')}</span>
        <input
          className="plan-review-answer"
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          onKeyDown={(e) => {
            if (e.nativeEvent.isComposing || e.keyCode === 229) return;
            if (e.key === 'Enter') {
              e.preventDefault();
              if (text) onDecide(text);
              else setStep('mode');
            }
          }}
          placeholder={t('coding.planReviewAnswerPlaceholder')}
        />
        <div className="plan-review-foot">
          <span className="plan-review-hint">ⓘ {t('coding.planReviewHintReview')}</span>
          <span className="plan-review-actions">
            <button type="button" className="plan-review-ignore" onClick={() => onDecide(text || undefined)}>
              {t('coding.planReviewKeepPlanning')}
            </button>
            <button type="button" className="plan-review-submit" onClick={() => setStep('mode')}>
              {t('coding.planReviewApprove')}
            </button>
          </span>
        </div>
      </div>
    );
  }

  if (step === 'mode') {
    return (
      <div className="plan-review" data-testid="plan-review-card" data-step="mode">
        <div className="plan-review-head">
          <span className="plan-review-badge ok">{t('coding.planReviewApprovedBadge')}</span>
          <span className="plan-review-title">{t('coding.planReviewModeTitle')}</span>
          <span className="plan-review-step">{t('coding.planReviewStepMode')}</span>
        </div>
        <div
          className="plan-review-modes"
          role="radiogroup"
          aria-label={t('coding.planReviewModeTitle')}
          onKeyDown={(e) => {
            if (e.key !== 'ArrowDown' && e.key !== 'ArrowUp') return;
            e.preventDefault();
            const index = PLAN_REVIEW_MODES.findIndex((m) => m.value === mode);
            const delta = e.key === 'ArrowDown' ? 1 : PLAN_REVIEW_MODES.length - 1;
            setMode(PLAN_REVIEW_MODES[(index + delta) % PLAN_REVIEW_MODES.length]!.value);
          }}
        >
          {PLAN_REVIEW_MODES.map((m, i) => (
            <button
              key={m.value}
              type="button"
              role="radio"
              aria-checked={mode === m.value}
              className={`plan-review-mode${mode === m.value ? ' selected' : ''}${m.danger ? ' danger' : ''}`}
              onClick={() => setMode(m.value)}
            >
              <span className="plan-review-mode-num">{i + 1}.</span>
              <span className="plan-review-mode-label">{t(m.labelKey)}</span>
              <span className="plan-review-mode-desc">{t(m.descKey)}</span>
            </button>
          ))}
        </div>
        <div className="plan-review-foot">
          <span className="plan-review-hint">ⓘ {t('coding.planReviewHintMode')}</span>
          <span className="plan-review-actions">
            <button type="button" className="plan-review-ignore" onClick={() => setStep('review')}>
              {t('coding.planReviewBack')}
            </button>
            <button
              type="button"
              className="plan-review-submit"
              onClick={() => (mode === 'danger-full-access' ? setStep('risk') : onApplyPreset(mode))}
            >
              {t('coding.planReviewStart')}
            </button>
          </span>
        </div>
      </div>
    );
  }

  return (
    <div className="plan-review" data-testid="plan-review-card" data-step="risk">
      <div className="plan-review-head">
        <span className="plan-review-badge danger">{t('coding.planReviewRiskBadge')}</span>
        <span className="plan-review-title danger">{t('common.permissionRiskTitle')}</span>
      </div>
      <div className="plan-review-risk">
        {t('common.permissionRiskBody')}
        <label className="plan-review-ack">
          <input
            type="checkbox"
            checked={riskChecked}
            onChange={(e) => setRiskChecked(e.target.checked)}
          />
          {t('common.permissionRiskAcknowledge')}
        </label>
      </div>
      <div className="plan-review-foot">
        <span className="plan-review-hint">ⓘ {t('coding.planReviewHintRisk')}</span>
        <span className="plan-review-actions">
          <button type="button" className="plan-review-ignore" onClick={() => setStep('mode')}>
            {t('common.permissionRiskCancel')}
          </button>
          <button
            type="button"
            className="plan-review-submit danger"
            disabled={!riskChecked}
            onClick={() => {
              if (riskChecked) onApplyPreset('danger-full-access');
            }}
          >
            {t('coding.planReviewEnableStart')}
          </button>
        </span>
      </div>
    </div>
  );
}

function QuestionCard({ question, onAnswer }: {
  question: { questionId: string; text: string; options: string[] };
  onAnswer: (answer: string) => void;
}) {
  const { t } = useI18n();
  const [draft, setDraft] = useState('');
  const submit = (answer: string) => {
    if (!answer.trim()) return;
    onAnswer(answer.trim());
    setDraft('');
  };
  return (
    <div className="approve-panel" data-testid="question-card">
      <div className="approve-header">
        <span className="approve-header-title">{t('question.title')}</span>
      </div>
      <div className="approve-reason">{question.text}</div>
      {question.options.length > 0 && (
        <div style={{ display: 'flex', gap: 6, flexWrap: 'wrap', margin: '8px 0' }}>
          {question.options.map((o) => (
            <button key={o} className="btn btn-secondary btn-small" data-testid={`question-option-${o}`}
              onClick={() => submit(o)}>
              {o}
            </button>
          ))}
        </div>
      )}
      <div className="approve-footer" style={{ display: 'flex', gap: 6 }}>
        <input
          aria-label={question.text}
          placeholder={t('question.freeTextPlaceholder')}
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          onKeyDown={(e) => { if (e.key === 'Enter') submit(draft); }}
          style={{ flex: 1, padding: '6px 8px', borderRadius: 6, border: '1px solid var(--border)', background: 'var(--surface)', color: 'var(--text)', fontSize: 12 }}
        />
        <button className="approve-allow" disabled={!draft.trim()} onClick={() => submit(draft)}>
          {t('question.answer')}
        </button>
      </div>
    </div>
  );
}

/** WI-152 S2: resolved image-accept for one active route key — the active
 *  provider's `visionSupported` decides; unknown providers stay permissive
 *  (the backend strips images for truly text-only routes). */
export function imagesAcceptedForRoute(models: ModelEntry[], activeModelKey: string): boolean {
  const providerId = activeModelKey.split('/')[0] || '';
  const provider = models.find(m => m.id === providerId);
  return provider ? provider.visionSupported !== false : true;
}

export function Composer(props: {
  value: string;
  onChange: (v: string) => void;
  onKeyDown: (e: React.KeyboardEvent<HTMLTextAreaElement>) => void;
  onPaste: (e: React.ClipboardEvent) => void;
  onDrop: (e: React.DragEvent) => void;
  placeholder: string;
  disabled: boolean;
  queueDock?: React.ReactNode;
  inputRef: React.RefObject<HTMLTextAreaElement>;
  attachments: Attachment[];
  onRemoveAttachment: (id: string) => void;
  onFileSelect: (e: React.ChangeEvent<HTMLInputElement>) => void;
  /** WI-152 S2: the active model cannot accept images — the file picker
   *  excludes image types and a notice explains why pasted/dropped images
   *  are refused. */
  imagesDisabled?: boolean;
  imagesNotice?: string | null;
  onSend: () => void;
  onStop: () => void;
  canSend: boolean;
  sending?: boolean;
  stopping?: boolean;
  skills?: SkillInfo[];
  mcpServers?: McpServerInfo[];
  /** Called just before a skill token is inserted so the parent can suppress its own
   *  `/` slash palette (otherwise the inserted `/<token>` would reopen it). */
  onInsertSkill?: (token: string) => void;
  /** Inline tool-approval panel rendered above the input bar (Coding).
 *: a header (icon + tool title), the reason line, the command to run
   *  (from `params`), then Deny on the left and Allow Once ▾ on the right. The caret
   *  opens "Allow Similar Commands" (remember = true) — the modern replacement for
   *  the old flat two-Allow buttons. */
  approval?: { tool: string; reason: string; params?: any; onAllow: () => void; onDeny: () => void; onAllowSimilar?: () => void } | null;
  /** WI-67 user-questions: an agent ask rendered as a composer-side card. */
  question?: { questionId: string; text: string; options: string[]; onAnswer: (answer: string) => void } | null;
  /** Plan review takeover: rendered as an overlay pinned to the composer's
   *  bottom edge (same anchor as the tool-approval panel), covering the input
   *  bar — mid-review the composer has nothing to offer. */
  planReviewCard?: React.ReactNode;
  /** WI-153: true when the Coding view shows the Turn rail — the composer's
   *  left edge then aligns with the message column (rail 48px + content 24px),
   *  keeping it flush with the work-fold hairline above. */
  railInset?: boolean;
  /** Current permission preset (manual / accept-edits / auto /
      danger-full-access, or the derived `custom`). The callback may return a
      promise — a rejected write surfaces as a transient error line. */
  permissionPreset?: string;
  onPermissionPresetChange?: (preset: string) => void | Promise<void>;
  /** Plan locks the bundle (manual + plan-managed sandbox): dropdown disabled with a hint. */
  permissionLocked?: boolean;
  /** Shared layout: the text area takes its own full-width row (left edge
   *  aligned with the ＋ button) with the ＋ / Send controls on a row below,
   *  and a taller resting input — identical in Chat and Coding. */
  stacked?: boolean;
  /** Controls (model / reasoning selects) rendered at the right end of the
   *  control row, on the same horizontal line as the approval-mode dropdown;
   *  Send stays rightmost. */
  trailingControls?: React.ReactNode;
  /** WI-152: controls rendered immediately AFTER the approval-mode dropdown
   *  (right of "Full Access ▾"), before trailingControls — used by the
   *  background-terminals entry so it doesn't crowd Send/Stop. */
  leadingControls?: React.ReactNode;
  /** WI-7 @file completion: when present (workspace-bound composers), typing
   *  `@` opens a workspace file suggestion popup above the input. */
  fileSuggest?: { workspaceId: string } | null;
}) {
  const { t } = useI18n();
  const fileInputRef = useRef<HTMLInputElement>(null);
  const dragDepth = useRef(0);
  const [dragActive, setDragActive] = useState(false);
  const [menuOpen, setMenuOpen] = useState(false);
  const [openPanel, setOpenPanel] = useState<'skills' | 'mcp' | null>(null);
  const [skillFilter, setSkillFilter] = useState('');
  const [skillIndex, setSkillIndex] = useState(0);
  const skillWrapRef = useRef<HTMLDivElement>(null);
 // approval: the caret next to Allow toggles an "Allow similar" menu.
  const [allowMenuOpen, setAllowMenuOpen] = useState(false);
  const allowWrapRef = useRef<HTMLDivElement>(null);
  // Approval-mode dropdown (custom, monochrome SVG icons + i18n labels).
  const [approvalOpen, setApprovalOpen] = useState(false);
  const approvalWrapRef = useRef<HTMLDivElement>(null);
 // High-risk confirmation for the danger preset (RiskConfirmation parity):
  // the write happens only after the acknowledge checkbox + Enable.
  const [riskOpen, setRiskOpen] = useState(false);
  const [riskAcknowledged, setRiskAcknowledged] = useState(false);
  const [permissionError, setPermissionError] = useState<string | null>(null);
  useEffect(() => {
    if (!approvalOpen) return;
    const close = () => setApprovalOpen(false);
    const onDown = (e: MouseEvent) => {
      if (approvalWrapRef.current && approvalWrapRef.current.contains(e.target as Node)) return;
      close();
    };
    const onKey = (e: KeyboardEvent) => { if (e.key === 'Escape') close(); };
    document.addEventListener('mousedown', onDown);
    document.addEventListener('keydown', onKey);
    return () => { document.removeEventListener('mousedown', onDown); document.removeEventListener('keydown', onKey); };
  }, [approvalOpen]);
  useEffect(() => {
    if (!allowMenuOpen) return;
    const close = () => setAllowMenuOpen(false);
    const onDown = (e: MouseEvent) => {
      if (allowWrapRef.current && allowWrapRef.current.contains(e.target as Node)) return;
      close();
    };
    const onKey = (e: KeyboardEvent) => { if (e.key === 'Escape') close(); };
    document.addEventListener('mousedown', onDown);
    document.addEventListener('keydown', onKey);
    return () => { document.removeEventListener('mousedown', onDown); document.removeEventListener('keydown', onKey); };
  }, [allowMenuOpen]);

  // Close the + menu on outside click / Escape. Escape collapses a flyout first.
  useEffect(() => {
    if (!menuOpen) return;
    const close = () => { setMenuOpen(false); setOpenPanel(null); };
    const onDown = (e: MouseEvent) => {
      // Menu-internal mousedowns stopPropagation, so this only sees genuine outside clicks.
      const inside = !!skillWrapRef.current && skillWrapRef.current.contains(e.target as Node);
      if (inside) return;
      close();
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== 'Escape') return;
      if (openPanel) setOpenPanel(null);
      else close();
    };
    document.addEventListener('mousedown', onDown);
    document.addEventListener('keydown', onKey);
    return () => { document.removeEventListener('mousedown', onDown); document.removeEventListener('keydown', onKey); };
  }, [menuOpen, openPanel]);

  const filteredSkills = useMemo(() => {
    const f = skillFilter.toLowerCase();
    return (props.skills || []).filter(s => {
      if (!f) return true;
      const tok = skillTokenOf(s);
      return tok.toLowerCase().includes(f) || s.name.toLowerCase().includes(f) || (s.description || '').toLowerCase().includes(f);
    });
  }, [props.skills, skillFilter]);
  useEffect(() => { setSkillIndex(0); }, [skillFilter, openPanel]);

  // WI-7 @file completion: composer-anchored popup listing workspace files.
  const [atOpen, setAtOpen] = useState(false);
  const [atQuery, setAtQuery] = useState('');
  const [atIndex, setAtIndex] = useState(0);
  const [atFiles, setAtFiles] = useState<string[]>([]);
  useEffect(() => { setAtIndex(0); }, [atQuery, atOpen]);
  useEffect(() => {
    if (!atOpen || !props.fileSuggest) return;
    let cancelled = false;
    Promise.resolve(window.starriver.suggestFiles?.(props.fileSuggest.workspaceId, atQuery))
      .then((files) => { if (!cancelled && Array.isArray(files)) setAtFiles(files); })
      .catch(() => { if (!cancelled) setAtFiles([]); });
    return () => { cancelled = true; };
  }, [atOpen, atQuery, props.fileSuggest?.workspaceId]);
  const insertAtFile = (path: string) => {
    const ta = props.inputRef.current;
    const caret = ta ? (ta.selectionStart ?? props.value.length) : props.value.length;
    const before = props.value.slice(0, caret);
    const m = before.match(/@([^\s@]*)$/);
    const tokenStart = m ? caret - m[0].length : caret;
    const ins = `@${path} `;
    const next = props.value.slice(0, tokenStart) + ins + props.value.slice(caret);
    props.onChange(next);
    setAtOpen(false);
    requestAnimationFrame(() => { if (ta) { ta.focus(); ta.setSelectionRange(tokenStart + ins.length, tokenStart + ins.length); } });
  };
  const onTextareaChange = (e: React.ChangeEvent<HTMLTextAreaElement>) => {
    autosize(e.target);
    props.onChange(e.target.value);
    if (!props.fileSuggest) return;
    const caret = e.target.selectionStart ?? e.target.value.length;
    const before = e.target.value.slice(0, caret);
    const m = before.match(/@([^\s@]*)$/);
    if (m) { setAtOpen(true); setAtQuery(m[1]); } else { setAtOpen(false); }
  };
  const onTextareaKeyDown = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    // Enter/arrows during IME composition belong to the IME (see the
    // composer's own guard) — committing a candidate must not splice a file
    // path into the composition.
    if ((e.nativeEvent as KeyboardEvent).isComposing || e.keyCode === 229) {
      props.onKeyDown?.(e);
      return;
    }
    if (atOpen && atFiles.length > 0) {
      if (e.key === 'Escape') { e.preventDefault(); setAtOpen(false); return; }
      if (e.key === 'Enter' || e.key === 'Tab') { e.preventDefault(); insertAtFile(atFiles[atIndex]); return; }
      if (e.key === 'ArrowDown') { e.preventDefault(); setAtIndex(i => Math.min(i + 1, atFiles.length - 1)); return; }
      if (e.key === 'ArrowUp') { e.preventDefault(); setAtIndex(i => Math.max(i - 1, 0)); return; }
    }
    props.onKeyDown(e);
  };

  // Reset the textarea height whenever the value is cleared (after send).
  const prevValue = useRef(props.value);
  useEffect(() => {
    if (props.value === '' && prevValue.current !== '') {
      const el = props.inputRef.current;
      if (el) el.style.height = '';
      // External clear (Send) fires no change event — close the @file popup
      // or the stale suggestion list floats over an empty input.
      setAtOpen(false);
    }
    prevValue.current = props.value;
  }, [props.value, props.inputRef]);

  // Insert `/<token> ` at the caret and hand the new value up via onChange.
  const insertSkill = (token: string) => {
    props.onInsertSkill?.(token);
    const ta = props.inputRef.current;
    const start = ta ? (ta.selectionStart ?? props.value.length) : props.value.length;
    const end = ta ? (ta.selectionEnd ?? start) : start;
    const ins = `/${token} `;
    const next = props.value.slice(0, start) + ins + props.value.slice(end);
    props.onChange(next);
    setMenuOpen(false); setOpenPanel(null); setSkillFilter('');
    requestAnimationFrame(() => { if (ta) { ta.focus(); ta.setSelectionRange(start + ins.length, start + ins.length); } });
  };

  const closeMenu = () => { setMenuOpen(false); setOpenPanel(null); };

  return (
    <div
      className={`composer-wrap${dragActive ? ' drop-active' : ''}${props.railInset ? ' rail-inset' : ''}`}
      onDragEnter={(e) => { e.preventDefault(); dragDepth.current += 1; setDragActive(true); }}
      onDragLeave={(e) => { e.preventDefault(); dragDepth.current -= 1; if (dragDepth.current <= 0) setDragActive(false); }}
      onDragOver={(e) => { e.preventDefault(); e.dataTransfer.dropEffect = 'copy'; setDragActive(true); }}
      onDrop={(e) => { e.preventDefault(); dragDepth.current = 0; setDragActive(false); props.onDrop(e); }}
    >
      {props.question && (
        <QuestionCard
          question={props.question}
          onAnswer={(answer) => props.question!.onAnswer(answer)}
        />
      )}
      {props.planReviewCard}
      {props.approval && (
        <div className="approve-panel">
          <div className="approve-header">
            <span className="approve-header-icon" dangerouslySetInnerHTML={{ __html: toolIconFor(props.approval.tool, approvalCommand(props.approval)) }} />
            <span className="approve-header-title">{approvalTitle(props.approval.tool)}</span>
          </div>
          {props.approval.reason && <div className="approve-reason">{props.approval.reason}</div>}
          <pre className="approve-cmd"><code>{approvalCommand(props.approval)}</code></pre>
          <div className="approve-footer">
            <button className="approve-deny" onClick={props.approval.onDeny}>{t('approval.deny')}</button>
            <div className="approve-allow-wrap" ref={allowWrapRef}>
              <button className="approve-allow" onClick={props.approval.onAllow}>{t('approval.allowOnce')}</button>
              {props.approval.onAllowSimilar && (
                <>
                  <button
                    className="approve-allow-caret"
                    aria-label="more-allow-options"
                    onClick={(e) => { e.stopPropagation(); setAllowMenuOpen(o => !o); }}
                  >▾</button>
                  {allowMenuOpen && (
                    <div className="approve-menu">
                      <button
                        onClick={() => { props.approval!.onAllow(); setAllowMenuOpen(false); }}
                      >{t('approval.allowOnce')}</button>
                      {props.approval.onAllowSimilar && (
                        <button
                        onClick={() => { props.approval!.onAllowSimilar!(); setAllowMenuOpen(false); }}
                        >{t('approval.allowSimilar')}</button>
                      )}
                    </div>
                  )}
                </>
              )}
            </div>
          </div>
        </div>
      )}
      {props.queueDock}
      <div className={`input-bar${props.stacked ? ' stacked' : ''}`}>
        {/* Pending attachments render INSIDE the input box, on their own row
            above the textarea (the strip is a real image preview, so the CSP
            must admit data: URLs — index.html img-src). */}
        {/* WI-152 S2 image gate notice (reuses the transient alert line). */}
        {props.imagesNotice && (
          <div className="composer-permission-error" role="alert">{props.imagesNotice}</div>
        )}
        <AttachmentStrip attachments={props.attachments} onRemove={props.onRemoveAttachment} />
        <div ref={skillWrapRef} style={{ position: 'relative' }}>
          <button type="button" className="composer-btn" aria-label="add-menu" onClick={() => { setMenuOpen(o => !o); setOpenPanel(null); }} data-tip={t("common.attach")}>＋</button>
          {props.permissionPreset !== undefined && props.onPermissionPresetChange && (() => {
            const current = PERMISSION_PRESETS.find(m => m.value === props.permissionPreset);
            const custom = current === undefined;
            const shown = current ?? { value: 'custom', labelKey: 'common.permissionCustom', danger: false };
            // Write through the parent; a rejected bridge call surfaces as a
            // transient line under the picker.
            const write = (preset: string) => {
              const result = props.onPermissionPresetChange?.(preset);
              if (result && typeof (result as Promise<void>).catch === 'function') {
                (result as Promise<void>).catch((e) => setPermissionError(String(e?.message ?? e)));
              }
            };
            return (
              <div className="composer-approval-wrap" ref={approvalWrapRef}>
                <button
                  type="button"
                  className={`composer-approval-btn${shown.danger ? ' danger' : ''}`}
                  aria-label="permission-preset"
                  disabled={props.permissionLocked}
                  title={props.permissionLocked ? undefined : t('common.permissionModeTip')}
                  data-tip={props.permissionLocked ? t('common.permissionLockedTip') : undefined}
                  onClick={() => setApprovalOpen(o => !o)}
                >
                  <span className="composer-approval-icon" dangerouslySetInnerHTML={{ __html: custom ? MODE_ICON_BY_PASS : (MODE_ICONS[shown.value] || '') }} />
                  <span className="composer-approval-label">{t(shown.labelKey)}</span>
                  {!props.permissionLocked && <span className="composer-approval-caret" aria-hidden="true" />}
                </button>
                {approvalOpen && (
                  <div className="composer-approval-menu">
                    {PERMISSION_PRESETS.map(m => (
                      <button
                        key={m.value}
                        type="button"
                        className={`composer-approval-item${m.value === props.permissionPreset ? ' active' : ''}${m.danger ? ' danger' : ''}`}
                        onClick={() => {
                          setApprovalOpen(false);
                          if (m.value === props.permissionPreset) return;
                          if (m.danger) {
                            // High-risk preset: confirm before writing (the
                            // host accepts it unconditionally).
                            setRiskAcknowledged(false);
                            setRiskOpen(true);
                          } else {
                            setPermissionError(null);
                            write(m.value);
                          }
                        }}
                      >
                        <span className="composer-approval-icon" dangerouslySetInnerHTML={{ __html: MODE_ICONS[m.value] || '' }} />
                        <span className="composer-approval-label">{t(m.labelKey)}</span>
                        {m.value === props.permissionPreset && <span className="composer-approval-check">✓</span>}
                      </button>
                    ))}
                  </div>
                )}
                {permissionError && (
                  <div className="composer-permission-error" role="alert">{permissionError}</div>
                )}
                <PermissionRiskModal
                  open={riskOpen}
                  onAcknowledgedChange={setRiskAcknowledged}
                  acknowledged={riskAcknowledged}
                  onCancel={() => setRiskOpen(false)}
                  onConfirm={() => {
                    setRiskOpen(false);
                    setPermissionError(null);
                    write('danger-full-access');
                  }}
                />
              </div>
            );
          })()}
          {/* WI-152: terminals entry sits right of the approval-mode dropdown. */}
          {props.leadingControls}
          {menuOpen && (
            <div className="attach-popup">
              <div
                className={`attach-menu-item has-sub${openPanel === 'skills' ? ' open' : ''}`}
                onMouseEnter={() => setOpenPanel('skills')}
                onMouseLeave={() => setOpenPanel(null)}
                onMouseDown={(e) => { e.preventDefault(); e.stopPropagation(); setOpenPanel('skills'); }}
              >
                <span className="attach-menu-icon" dangerouslySetInnerHTML={{ __html: tabIcons.skills }} />
                {t('common.attachSkill')}
                <span className="attach-caret">▶</span>
                {openPanel === 'skills' && (
                  <div className="attach-submenu" onMouseDown={(e) => e.stopPropagation()}>
                    <input className="toolbar-input" value={skillFilter} onChange={e => setSkillFilter(e.target.value)} placeholder={t('common.skillSearch')} style={{ margin: '8px 8px 6px', width: 'calc(100% - 16px)', fontSize: 12 }} />
                    <div className="attach-scroll">
                      {filteredSkills.length === 0 && <div style={{ padding: '10px 12px', color: 'var(--text-muted)', fontSize: 12 }}>—</div>}
                      {filteredSkills.map((s, i) => (
                        <div key={s.name} onMouseDown={(e) => { e.preventDefault(); e.stopPropagation(); insertSkill(skillTokenOf(s)); }} style={{ padding: '7px 12px', cursor: 'pointer', display: 'flex', flexDirection: 'column', gap: 1, background: i === skillIndex ? 'var(--accent-soft)' : 'transparent' }}>
                          <div style={{ fontSize: 14, fontWeight: 600, color: 'var(--text)' }}>{`/${skillTokenOf(s)}`}</div>
                          {s.description && <div style={{ fontSize: 12, color: 'var(--text-muted)', whiteSpace: 'nowrap', overflow: 'hidden', textOverflow: 'ellipsis' }}>{s.description}</div>}
                        </div>
                      ))}
                    </div>
                  </div>
                )}
              </div>
              <div
                className={`attach-menu-item has-sub${openPanel === 'mcp' ? ' open' : ''}`}
                onMouseEnter={() => setOpenPanel('mcp')}
                onMouseLeave={() => setOpenPanel(null)}
                onMouseDown={(e) => { e.preventDefault(); e.stopPropagation(); setOpenPanel('mcp'); }}
              >
                <span className="attach-menu-icon" dangerouslySetInnerHTML={{ __html: tabIcons.mcp }} />
                MCP
                <span className="attach-caret">▶</span>
                {openPanel === 'mcp' && (
                  <div className="attach-submenu" onMouseDown={(e) => e.stopPropagation()}>
                    <div className="attach-scroll">
                      {(props.mcpServers || []).length === 0 && <div style={{ padding: '10px 12px', color: 'var(--text-muted)', fontSize: 12 }}>{t('mcp.noServers')}</div>}
                      {(props.mcpServers || []).map(s => (
                        <div key={s.name} className="attach-mcp-row">
                          <span className="attach-mcp-name">{s.name}</span>
                          <span className="attach-mcp-meta">{t('mcp.toolCount', { count: s.toolCount })}</span>
                          <span className={`attach-mcp-status ${s.status === 'connected' ? 'ok' : 'bad'}`}>{s.status}</span>
                        </div>
                      ))}
                    </div>
                  </div>
                )}
              </div>
              <div className="attach-menu-item" onMouseDown={(e) => { e.preventDefault(); e.stopPropagation(); closeMenu(); fileInputRef.current?.click(); }}>
                <span className="attach-menu-icon" dangerouslySetInnerHTML={{ __html: tabIcons.file }} />
                {t('common.attachFile')}
              </div>
            </div>
          )}
        </div>
        {atOpen && atFiles.length > 0 && (
          <div style={{ position: 'absolute', left: 16, right: 16, bottom: 'calc(100% + 6px)', zIndex: 40, background: 'var(--surface)', border: '1px solid var(--border-strong)', borderRadius: 8, boxShadow: 'var(--shadow-md)', maxHeight: 220, overflowY: 'auto' }}>
            {atFiles.map((path, i) => (
              <div key={path} onMouseDown={(e) => { e.preventDefault(); insertAtFile(path); }} style={{ padding: '6px 12px', cursor: 'pointer', fontSize: 14, fontFamily: 'monospace', color: 'var(--text)', background: i === atIndex ? 'var(--accent-soft)' : 'transparent', whiteSpace: 'nowrap', overflow: 'hidden', textOverflow: 'ellipsis' }}>
                @{path}
              </div>
            ))}
          </div>
        )}
        <textarea
          ref={props.inputRef}
          value={props.value}
          onChange={onTextareaChange}
          onKeyDown={onTextareaKeyDown}
          onPaste={props.onPaste}
          placeholder={props.placeholder}
          disabled={props.disabled}
        />
        <input type="file" multiple
          accept={props.imagesDisabled
            ? '.txt,.md,.markdown,.csv,.json,.yaml,.yml,.log,.pdf,.docx,.xlsx'
            : 'image/*,.txt,.md,.markdown,.csv,.json,.yaml,.yml,.log,.pdf,.docx,.xlsx'}
          ref={fileInputRef} style={{ display: 'none' }}
          onChange={props.onFileSelect} />
        {props.trailingControls && (
          <div className="composer-trailing">{props.trailingControls}</div>
        )}
        {props.sending ? (
          <button
            className={`composer-send composer-stop${props.stopping ? ' stopping' : ''}`}
            onClick={props.onStop}
            disabled={props.stopping}
          >
            {props.stopping ? t('common.stopping') : t('common.stop')}
          </button>
        ) : (
          <button className="composer-send" onClick={props.onSend} disabled={props.disabled || !props.canSend}>{t('common.send')}</button>
        )}
      </div>
    </div>
  );
}

/**
 * Task stop control shared by ChatTab and CodingView.
 * - requestStop: confirm, then ask the backend to cancel by session id —
 *   IMMEDIATELY when the id is known, and via a decoupled retry loop (2s
 *   interval) when it is not. The cancel path is a plain protocol invoke and
 *   must never depend on the incoming event pipe: a dead runtime:event
 *   stream once left Stop armed forever waiting for a TaskStarted that
 *   could never arrive (2026-10-02 session ed397c5b).
 * - cancel() resolving false (nothing running / unknown session) resets
 *   `stopping` right away — and calls `onNotRunning` so the owning view can
 *   drop its own stale run state (2026-10-12 09:02 host log: the turn had
 *   ended long before, the composer still showed Stop, and both Stop clicks
 *   hit "no live agent"; resetting only `stopping` left the button wedged
 *   on Stop until 新建 task reset the whole view).
 * - onTerminal must be called on TaskCompleted / TaskFailed / TaskCancelled;
 *   it also stops the retry loop.
 * - Hard watchdog (20s): if no terminal confirmation arrived, reset
 *   `stopping` and tell the user honestly — the event stream is likely
 *   broken while the backend may still be running.
 */
export function useTaskStop(
  sessionIdRef: React.RefObject<string>,
  notify: (text: string) => void,
  fallbackSessionRef?: React.RefObject<string>,
  onNotRunning?: () => void,
) {
  const { t } = useI18n();
  const [stopping, setStopping] = useState(false);
  const cancelSentRef = useRef(false);
  const retryTimerRef = useRef<ReturnType<typeof setInterval> | null>(null);
  const watchdogRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  // Stop targets the OPEN session. The primary ref only fills from live
  // TaskStarted/sessionStarted events, so after an app restart / tab switch /
  // session remount it reads "" while the view still shows an open session
  // (the remount path mirrors the sidebar id into sessionIdRef2 only) — the
  // 2026-10-07 21:48 report: Stop armed, clicked, and the request NEVER left
  // the renderer (host log: zero cancels), the 20s watchdog fired, and the
  // turn ran on. Fall back to the open-session ref, matching stopStaleTurn.
  const stopTarget = () => sessionIdRef.current || fallbackSessionRef?.current || '';

  const clearTimers = useCallback(() => {
    if (retryTimerRef.current !== null) {
      clearInterval(retryTimerRef.current);
      retryTimerRef.current = null;
    }
    if (watchdogRef.current !== null) {
      clearTimeout(watchdogRef.current);
      watchdogRef.current = null;
    }
  }, []);

  useEffect(() => clearTimers, [clearTimers]);

  const sendCancel = useCallback(async () => {
    const sid = stopTarget();
    if (!sid || cancelSentRef.current) return;
    cancelSentRef.current = true;
    try {
      const ok = await window.starriver.cancel(sid);
      if (!ok && stopTarget() === sid) {
        // The host has no live turn for this session: the composer's run
        // state is stale (the turn's terminal event never reached us). Drop
        // ALL of it — not just `stopping` — or the button stays "Stop" while
        // every further click hits an idle backend. clearTimers first so the
        // 20s watchdog cannot fire a misleading stopNoConfirm afterwards.
        cancelSentRef.current = false;
        clearTimers();
        setStopping(false);
        onNotRunning?.();
      }
    } catch {
      cancelSentRef.current = false;
      setStopping(false);
    }
  }, [sessionIdRef, fallbackSessionRef, clearTimers, onNotRunning]);

  const onTerminal = useCallback(() => {
    cancelSentRef.current = false;
    setStopping(false);
    clearTimers();
  }, [clearTimers]);

  const requestStop = useCallback(() => {
    if (stopping) return;
    if (!confirm(t('common.confirmStop'))) return;
    setStopping(true);
    if (stopTarget()) {
      sendCancel();
    } else {
      // No session id yet — retry on a timer instead of waiting for the
      // TaskStarted event: the event pipe may be exactly what is broken.
      retryTimerRef.current = setInterval(() => {
        if (cancelSentRef.current) {
          clearTimers();
          return;
        }
        if (stopTarget()) {
          clearTimers();
          sendCancel();
        }
      }, 2000);
    }
    // Hard watchdog: without a terminal confirmation within 20s the UI
    // state must not stay wedged — reset and surface the truth.
    watchdogRef.current = setTimeout(() => {
      cancelSentRef.current = false;
      setStopping(false);
      clearTimers();
      notify(t('common.stopNoConfirm'));
    }, 20000);
  }, [stopping, sendCancel, sessionIdRef, fallbackSessionRef, clearTimers, notify, t]);

  const onTaskStarted = useCallback(() => {
    if (stopping && !cancelSentRef.current) sendCancel();
  }, [stopping, sendCancel]);

  // Session switch / remount: drop the previous session's stop state instead
  // of letting 停止中/stopping leak into the newly opened one.
  const reset = useCallback(() => {
    cancelSentRef.current = false;
    setStopping(false);
    clearTimers();
  }, [clearTimers]);

  return { stopping, requestStop, onTaskStarted, onTerminal, reset };
}

/* ─── Shared hook: resizable panel ─── */

function useResizable(initialWidth: number, minWidth: number, maxWidth: number, side: 'left' | 'right' = 'left') {
  const [width, setWidth] = useState(initialWidth);
  const dragging = useRef(false);
  const startX = useRef(0);
  const startWidth = useRef(initialWidth);

  const onMouseDown = useCallback((e: React.MouseEvent) => {
    e.preventDefault();
    dragging.current = true;
    startX.current = e.clientX;
    startWidth.current = width;
    document.body.style.cursor = 'col-resize';
    document.body.style.userSelect = 'none';
  }, [width]);

  useEffect(() => {
    const onMouseMove = (e: MouseEvent) => {
      if (!dragging.current) return;
      const delta = side === 'left' ? e.clientX - startX.current : startX.current - e.clientX;
      const newWidth = Math.min(maxWidth, Math.max(minWidth, startWidth.current + delta));
      setWidth(newWidth);
    };
    const onMouseUp = () => {
      if (!dragging.current) return;
      dragging.current = false;
      document.body.style.cursor = '';
      // Release-to-outside-the-window must still restore selection: a stuck
      // `user-select: none` on <body> silently kills text selection in the
      // whole conversation until the next successful drag.
      document.body.style.userSelect = '';
    };
    window.addEventListener('mousemove', onMouseMove);
    window.addEventListener('mouseup', onMouseUp);
    // Releasing the drag outside the window (or an app-level focus loss)
    // never fires window mouseup — end the drag there too.
    window.addEventListener('blur', onMouseUp);
    return () => {
      window.removeEventListener('mousemove', onMouseMove);
      window.removeEventListener('mouseup', onMouseUp);
      window.removeEventListener('blur', onMouseUp);
    };
  }, [minWidth, maxWidth, side]);

  return { width, onMouseDown };
}

/* ─── Lightweight Markdown → HTML ─── */

/* ─── Markdown rendering ───
   Assistant content renders via react-markdown + remark-gfm, so GFM works out of
   the box (tables, strikethrough, task lists, autolinks, footnotes). This replaces
   the old hand-rolled regex renderer, which supported none of those. react-markdown
   is also safe by default — raw HTML is escaped unless rehype-raw is added — so no
   separate escaping pass is needed. */

/** The default markdown URL transform only allows web protocols; extend it
 *  with the attachment refs (sha256: / data:image/) so image refs survive
 *  sanitization and reach the img override below (WI-102). */
function mdUrlTransform(url: string): string {
  if (url.startsWith('sha256:') || url.startsWith('data:image/')) return url;
  return defaultUrlTransform(url);
}

function MarkdownContent({ source }: { source: string }) {
  return (
    <div className="md-content">
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        rehypePlugins={[[rehypeHighlight, { ignoreUnknown: true }]]}
        urlTransform={mdUrlTransform}
        components={{
          // .md-content pre::before draws the language label from data-lang.
          // react-markdown puts the language class on <code>; hoist it up to <pre>.
          pre({ node: _node, children }) {
            const el = React.Children.toArray(children)[0] as any;
            const cls: string | undefined = el?.props?.className;
            const lang = /language-([\w+-]+)/.exec(cls || '')?.[1];
            return <pre data-lang={lang || 'code'}>{children}</pre>;
          },
          // External links open in a new tab.
          a({ node: _node, ...rest }) {
            return <a {...rest} target="_blank" rel="noopener noreferrer" />;
          },
          // Attachment refs (WI-102 generated images echoed by the assistant;
          // same refs user uploads carry) resolve through the lazy
          // attachments.read path — a native <img src="sha256:…"> would 404.
          img({ node: _node, src, alt, ...rest }) {
            const ref = typeof src === 'string' ? src : '';
            if (ref.startsWith('sha256:') || ref.startsWith('data:image/')) {
              return <UserImage fileRef={ref} alt={alt || ''} />;
            }
            return <img src={src} alt={alt} loading="lazy" {...rest} />;
          },
        }}
      >
        {source || ''}
      </ReactMarkdown>
    </div>
  );
}

/** Split an assistant message body into prose and tool-call segments. History
 *  reloads reattach tool calls as "🔧 {tool}: {arg}" lines (see desktop
 *  get_session_history / tool_call_label), so a line starting with "🔧 " is a tool
 *  row and everything else is markdown prose. */
export function splitAssistantContent(content: string): { kind: 'text' | 'tool'; text: string }[] {
  const segs: { kind: 'text' | 'tool'; text: string }[] = [];
  let buf: string[] = [];
  const flush = () => { if (buf.length) { segs.push({ kind: 'text', text: buf.join('\n') }); buf = []; } };
  for (const ln of content.split('\n')) {
    if (ln.startsWith('🔧')) { flush(); segs.push({ kind: 'tool', text: ln }); }
    else buf.push(ln);
  }
  flush();
  return segs;
}

/** Assistant message body. History reloads reattach tool calls to the assistant
 *  bubble as "🔧 {tool}: {arg}" lines; render those as ToolRows so the wrench emoji
 *  never shows as text and the label/command get proper code styling, keeping the
 *  surrounding prose as markdown. */
export function AssistantContent({ content }: { content: string }) {
  const segs = splitAssistantContent(content);
  if (segs.length === 0) return <MarkdownContent source={content} />;
  return (
    <div className="assistant-content">
      {segs.map((s, i) =>
        s.kind === 'tool'
          ? <ToolRow key={i} content={s.text} />
          : <MarkdownContent key={i} source={s.text} />
      )}
    </div>
  );
}

/* ─── types ─── */

interface Message {
  id: string;
  role: 'user' | 'assistant' | 'tool' | 'system' | 'thinking' | 'compaction';
  content: string;
  streaming?: boolean;
  toolName?: string;
  durationMsTool?: number;
  seq?: number;
  isSubagent?: boolean;
  meta?: unknown;
}

/* ─── Shared hook: resizable panel ─── */


/* ─── types ─── */

interface AgentInfo {
  name: string;
  description: string;
  version: string;
  mode: string | null;
  default_mode: string;
}

interface SkillInfo {
  name: string;
  description: string;
  has_command: boolean;
  command?: string;
  version?: string;
  always_on?: boolean;
  has_keyword?: boolean;
  has_suffix?: boolean;
  has_hooks?: boolean;
  enabled?: boolean;
  body?: string;
}

interface SkillUsageRecord {
  session_id?: string;
  project_id?: string;
  skill: string;
  version?: string;
  mode: string;
  ts: number;
}

interface PluginEntry {
  name: string;
  description: string;
  version: string;
  skills: string[];
  source_url: string;
  installed_at: string;
  /** Wire shape (plugin_entry_json) uses camelCase; older fields above stay
   *  for the legacy market view. */
  sourceUrl?: string;
  installedAt?: string;
  ui?: {
    tabs?: { id: string; title: string }[];
    permissions?: string[];
    theme?: string | null;
  } | null;
}

interface ModelProvider {
  id: string;
  name: string;
  providerType: string;
  baseUrl: string;
  models: string[];
  requiresApiKey: boolean;
  enabled: boolean;
  /** Inference API format — "anthropic" (Messages API) or "openai-chat" (OpenAI-compat). */
  apiFormat?: string;
  /** Default reasoning effort hint ("off"|"low"|"high"|"max"). */
  reasoningEffort?: string;
}

/** One configurable-provider directory entry surfaced to the Models tab. */
interface ConfigurableProvider {
  provider: string;
  displayName: string;
  settingsNs: string;
  settingsPath: string[];
  declared: boolean;
  configured: boolean;
}

/** One subagent template's delegation config. */
interface SubagentConfig {
  name: string;
  description: string;
  tools: string[];
  readOnly: boolean;
  enabled: boolean;
  model?: string;
}

/** One job (running or recent) in the unified job panel. */
interface JobInfo {
  taskName: string;
  status: string;
  startedAt: string;
  summary: string;
  sessionId?: string;
}

interface ModelVariant {
  id: string;
  name: string;
  /** Real context window reported by the provider (/v1/models context_length), if any. */
  contextLength?: number;
}

interface ModelEntry {
  id: string;
  name: string;
  modelType: string;
  providerType: string;
  baseUrl: string;
  /** Napi path carries the raw value/reference; the protocol path strips it (hasApiKey instead). */
  apiKey: string;
  /** Protocol `models.list`: a key/reference is configured (value never sent). */
  hasApiKey?: boolean;
  modelPath?: string;
  models: ModelVariant[];
  defaultModel: string;
  concurrency?: number;
  maxTokens?: number;
  contextWindow?: number;
  temperature?: number;
  apiFormat?: string;
  reasoningEffort?: string;
  /** Custom model ID used as the prefix of selector names; blank = provider name. */
  modelLabel?: string;
  /** Legacy stored declaration (no UI, ignored — WI-152 v2 resolves capability automatically). */
  supportsVision?: boolean;
  /** Resolved vision support (built-in provider table > supported; automatic).
   *  The composer image gate consumes this (WI-152 S1). */
  visionSupported?: boolean;
}

interface BoundModelInfo {
  id: string;
  name: string;
  enabled: boolean;
}

interface AgentModelBinding {
  agentName: string;
  activeModel: string;
  boundModels: BoundModelInfo[];
  /** Session-scoped reasoning effort override (''|'off'|'low'|'high'|'max'). */
  reasoningEffort?: string | null;
}

/** One session.trace event (mirrors host types::SessionTraceEntry). */
interface TraceEntry {
  kind: 'user' | 'assistant' | 'thinking' | 'tool_use' | 'tool_result' | 'system' | 'context';
  text: string;
  toolName?: string | null;
  toolInput?: string | null;
  toolOutput?: string | null;
  toolOk?: boolean | null;
  seq?: number;
  timeMs?: number;
  turn?: number | null;
  step?: number | null;
  inputTokens?: number | null;
  outputTokens?: number | null;
  provider?: string | null;
  model?: string | null;
  durationMs?: number | null;
  ttftMs?: number | null;
}

interface WorktreeStatus {
  isRepo: boolean;
  branch?: string;
  isWorktree: boolean;
  worktreePath?: string;
  dirty: boolean;
}

/** Workspace registry view (model): stable id over a directory path. */
interface WorkspaceInfo {
  id: string;
  path: string;
  title: string;
  createdAt: string;
  sessionIds: string[];
}

interface SessionInfo {
  id: string;
  title: string;
  /** Owning workspace id (undefined → Ungrouped). */
  workspaceId?: string;
  createdAt: string;
  messageCount: number;
  parentId?: string;
  forkPoint?: number;
  /** Agent the session started with (write-once stamp; undefined = legacy
   *  row — the UI falls back to workspace membership). */
  agent?: string;
  /** Session origin marker ("scheduled" = scheduled-task run; WI-152). */
  origin?: string;
}

interface ChatMessage {
  role: string;
  content: string;
  /** Source event unix ms — persisted in the session log, shown by history. */
  ts?: number;
  /** Turn token totals (projected onto the turn's final assistant piece). */
  usage?: { inputTokens: number; outputTokens: number };
  /** Turn duration (ms), derived from the first-event→turn/end window. */
  durationMs?: number;
  /** Turn completion time = the turn/end event's unix ms. */
  doneTs?: number;
}

/** Backtrack timeline message: the id is the session-log seq (NAPI passes it as
 *  f64 to avoid BigInt); the fork cutoff shares the same origin. */
interface SessionMessage {
  id: number;
  role: string;
  content: string;
}

/** A single cross-session memory entry shown in the Memory panel. */
interface MemoryEntry {
  name: string;
  description: string;
  body: string;
  source: 'compaction' | 'extract' | string;
 /** Agent whose session produced this memory ("starriver" | "" | …);
   *  absent on older host payloads. */
  source_agent?: string;
  project: string;
  created: number;
  updated: number;
}

/** OAuth config surfaced by the backend for editing. The secret is masked (`has_secret`). */
interface McpOAuthInfo {
  authorization_url: string;
  token_url: string;
  client_id: string;
  scopes: string[];
  redirect_uri: string;
  has_secret: boolean;
}

/** Editable OAuth form state — scopes are held as comma-separated text while editing. */
interface McpOAuthForm {
  authorization_url: string;
  token_url: string;
  client_id: string;
  client_secret: string;
  scopes: string;
  redirect_uri: string;
}

interface McpServerInfo {
  name: string;
  transport: string;
  command: string;
  args: string[];
  env: string[];
  url?: string;
  oauthConnected?: boolean;
  oauth?: McpOAuthInfo;
  toolCount: number;
  status: string;
}

interface McpTestResult {
  ok: boolean;
  toolCount: number;
  tools: string[];
  error?: string;
}

interface McpToolTestResult {
  ok: boolean;
  output?: string;
  error?: string;
}

interface ScheduledTaskInfo {
  name: string;
  prompt: string;
  model?: string;            // "providerId/variantId"; empty = active agent model
  agent?: string;            // Agent the task runs as; empty = built-in Chat Agent
  workspace?: string;        // Workspace the runs bind to; empty = ungrouped (Tasks)
  enabled: boolean;
  modeType: string;          // "periodic" | "interval" | "once"
  period?: string;           // daily/weekly/biweekly/monthly/quarterly/yearly
  time?: string;             // "HH:MM" (Periodic)
  dayOfWeek?: number;        // 0=Sun..6=Sat (Periodic:Weekly)
  dayOfMonth?: number;       // 1-31 (Periodic:Monthly)
  month?: number;            // 1-12 (Periodic:Yearly)
  hours?: number;            // 1-24 (Interval)
  daysOfWeek: number[];      // 0=Sun..6=Sat (Interval); empty = every day
  datetime?: string;         // ISO 8601 (Once)
  startDate?: string;        // YYYY-MM-DD
  endDate?: string;          // YYYY-MM-DD
  skills: string[];          // resolved skill names to inject at run time
  description: string;       // human-readable
  lastRun?: string;
  nextRun?: string;
  lastStatus?: string;
}

interface TaskRunRecord {
  timestamp: string;
  status: string;
  summary: string;
  sessionId?: string;
}

interface TaskRunRecordWithName {
  taskName: string;
  timestamp: string;
  status: string;
  summary: string;
  sessionId?: string;
}

interface SessionTraceEntry {
  kind: string;            // "user" | "assistant" | "thinking" | "tool_use" | "tool_result"
  text: string;
  toolName?: string;
  toolInput?: string;
  toolOutput?: string;
  toolOk?: boolean;
}

interface SessionTrace {
  entries: SessionTraceEntry[];
}

// NOTE: declared via type aliases — esbuild 0.28's TSX parser mis-reads
// `prop?: (): T` shapes in interfaces (parses `(` as JSX).
type LanAddressFn = () => Promise<string>;
type StartHttpServerFn = () => Promise<string>;
type HttpServerInfoFn = () => Promise<string | null>;
type GetPermissionFn = (sessionId: string) => Promise<{ sandbox: string | null; approval: string | null; preset: string | null }>;
interface StarriverBridge {
  lanAddress?: LanAddressFn;
  retryInit?(): Promise<{ ok: boolean; relaunched?: boolean }>;
  startHttpServer?: StartHttpServerFn;
  httpServerInfo?: HttpServerInfoFn;
  execute(task: string, sessionId: string | undefined, cb: (err: string | null, event: any) => void, executionMode?: string, agent?: string, isolation?: boolean, agentPreset?: string, permissionPreset?: string): void;
  approve(sessionId: string, allowed: boolean, remember?: boolean): Promise<void>;
  /** Plan fold: answer a pending exit_plan_mode review (Plan confirm card). */
  planReviewDecision(sessionId: string, requestId: string, approved: boolean, feedback?: string): Promise<void>;
  /** WI-5: agent-preset roster + deployment default for new sessions. */
  listAgentPresets?(): Promise<{ presets: { id: string; agent: string; name: string; description: string; source: string; broken: string | null; isDefault: boolean }[]; default: string }>;
  listTools(): Promise<any[]>;
  listAgents(): Promise<AgentInfo[]>;
  respondQuestion(sessionId: string, questionId: string, answer: string): Promise<void>;
  /** WI-92: inject a user message into the RUNNING turn (kernel steer). */
  steerSession(sessionId: string, task: string): Promise<{ steered: boolean }>;
  /** WI-93: background terminal panel. */
  listTerminals?(): Promise<{ terminals: { id: string; status: string; command?: { startedAtMs: number; finishedAtMs?: number; exitCode?: number } | null }[] }>;
  killTerminal?(id: string): Promise<{ killed: boolean }>;
  /** WI-101: paginated session messages. */
  getSessionMessagesPage?(sessionId: string, limit?: number, before?: number): Promise<{ messages: ChatMessage[]; total: number; hasMore: boolean }>;
  /** WI-107: full-text search over session payloads; hits carry a snippet. */
  searchSessions?(query: string, limit?: number): Promise<Array<{ sessionId: string; createdAt: number; snippet: string; matches: number }>>;
  /** WI-105: raw settings read/patch (settings.get / settings.update). */
  getSettings?(): Promise<any>;
  updateSettings?(patch: any): Promise<any>;
  /** WI-96: structured review. */
  runReview?(target: unknown, dimensions?: string[], cwd?: string): Promise<{ findings: unknown[]; files?: string[]; raw?: string; note?: string }>;
  /** WI-98: doctor checks. */
  runDoctor?(): Promise<{ checks: { name: string; status: string; detail: string }[] }>;
  /** WI-97: goal surface. */
  setGoal?(sessionId: string, objective: string, tokenBudget?: number): Promise<{ set: boolean }>;
  getGoal?(sessionId: string): Promise<{ objective: string; tokenBudget?: number; tokensUsed: number; status: string } | null>;
  clearGoal?(sessionId: string): Promise<{ cleared: boolean }>;
  scanAgents(): Promise<AgentInfo[]>;
  listSkills(): Promise<SkillInfo[]>;
  scanSkills(): Promise<SkillInfo[]>;
  createSkill(name: string, description: string, command: string | undefined, body: string): Promise<SkillInfo[]>;
  updateSkill(name: string, description: string, command: string | undefined, body: string): Promise<SkillInfo[]>;
  deleteSkill(name: string): Promise<SkillInfo[]>;
  setSkillAlwaysOn(name: string, alwaysOn: boolean): Promise<SkillInfo[]>;
  listSkillUsage(limit?: number): Promise<SkillUsageRecord[]>;
  compactSession(sessionId: string, keepRecent?: number): Promise<{ before: number; after: number }>;
  listPlugins(): Promise<PluginEntry[]>;
  installPlugin(sourceUrl: string): Promise<PluginEntry>;
  uninstallPlugin(name: string): Promise<PluginEntry[]>;
  listModelProviders(): Promise<ModelProvider[]>;
  marketList(): Promise<any[]>;
  // New model registry methods
  listModels(): Promise<ModelEntry[]>;
  removeModel(id: string): Promise<void>;
  listAgentModelBindings(): Promise<AgentModelBinding[]>;
  setAgentActiveModel(agentName: string, providerId: string, variantId: string): Promise<void>;
  setAgentReasoningEffort(agentName: string, effort: string): Promise<void>;
  addModel(id: string, name: string, modelType: string, providerType: string,
           baseUrl?: string, apiKey?: string, modelPath?: string,
           defaultModel?: string, variantsJson?: string, concurrency?: number,
           maxTokens?: number, contextWindow?: number,
           temperature?: number, apiFormat?: string, reasoningEffort?: string,
           modelLabel?: string, supportsVision?: boolean): Promise<void>;
  readImageAttachment?(id: string): Promise<{ id: string; mediaType: string; width: number; height: number; base64: string }>;
  suggestFiles?(workspaceId: string, prefix?: string): Promise<string[]>;
  listSessions(workspaceId?: string): Promise<SessionInfo[]>;
  getSessionMessages(id: string): Promise<ChatMessage[]>;
  getSessionTimeline(id: string): Promise<SessionMessage[]>;
  /** Trajectory ledger (session.trace): chronological event stream rendered by TrajectoryView. */
  getSessionTrace(id: string): Promise<{ entries: TraceEntry[] }>;
  forkSession(sourceId: string, beforeMsgId: number, task: string): Promise<SessionInfo>;
  deleteSession(id: string): Promise<void>;
  renameSession(id: string, title: string | null): Promise<void>;
  archiveSession(id: string): Promise<void>;
  unarchiveSession(id: string): Promise<void>;
  assignSessionWorkspace(id: string, workspaceId: string): Promise<void>;
  listArchivedSessions(): Promise<SessionInfo[]>;
  fetchProviderModels(baseUrl: string, apiKey?: string, engine?: 'ollama' | 'lmstudio'): Promise<{ ok: boolean; error?: string; models: { id: string; name: string; contextLength?: number }[] }>;

 // Workspaces (registry — pick an existing directory, no mkdir)
  listWorkspaces(): Promise<WorkspaceInfo[]>;
  createWorkspace(path: string): Promise<WorkspaceInfo>;
  renameWorkspace(workspaceId: string, title: string): Promise<void>;
  deleteWorkspace(workspaceId: string): Promise<void>;
  setActiveWorkspace(workspaceId: string | null, agent?: string): Promise<void>;
  getActiveWorkspace(agent?: string): Promise<string | null>;
  setAgentMode(mode: string | null): Promise<void>;
  /** No arg: deployment default (initial staged value for new sessions); with a
   *  sessionId: that session's locked mode (the host also replays it to the kernel). */
  getAgentMode(sessionId?: string): Promise<string>;
  setPermissionPreset(sessionId: string, preset: string): Promise<void>;
  getPermission?: GetPermissionFn;
  /** Backlog #7 settings seam: full settings document (snake_case keys) and
   *  deep-merge hot update — the merged document is returned. */
  getSettings(): Promise<Record<string, unknown>>;
  updateSettings(patch: Record<string, unknown>): Promise<Record<string, unknown>>;
  startCodeReview(filesJson: string, dimensionsJson?: string): Promise<void>;
  selectDirectory(): Promise<string | null>;

  // Git Worktree (runtime isolation/status only; the kept-worktree admin panel is retired)
  getWorktreeStatus(cwd: string): Promise<WorktreeStatus | null>;

  // Memory (Coding Agent cross-session facts — Memory panel)
  listMemories(projectId?: string): Promise<MemoryEntry[]>;
  recallMemories(projectId: string | undefined, query: string): Promise<MemoryEntry[]>;
  upsertMemory(projectId: string | undefined, name: string, description: string, body: string): Promise<MemoryEntry>;
  memoryStatus(): Promise<any>;
  ingestMemories(): Promise<any>;
  exportMemories(path: string): Promise<any>;
  deleteMemory(projectId: string, name: string): Promise<void>;
  clearMemories(projectId: string): Promise<number>;

  // Attachments
  saveTempFile(name: string, base64Data: string): Promise<string>;

  // Open the workspace directory externally (main-process spawn)
  openTerminal?(cwd: string, command?: string): Promise<{ ok: boolean }>;
  openIDE?(cwd: string, command?: string): Promise<{ ok: boolean }>;
  /** Models page "open config file": open ~/.starriver/settings.yaml in the system editor. */
  openModelsConfig?(): Promise<{ ok: boolean; error?: string }>;

  // MCP Servers
  listMcpServers(): Promise<McpServerInfo[]>;
  addMcpServer(name: string, transport: string, command: string, argsJson: string, envJson: string, url?: string | null, oauthJson?: string | null): Promise<void>;
  removeMcpServer(name: string): Promise<void>;
  refreshMcpTools(name?: string | null): Promise<void>;
  oauthConnect(name: string): Promise<string>;
  oauthDisconnect(name: string): Promise<void>;
  testMcpServer(name: string): Promise<McpTestResult>;
  testMcpTool(name: string, toolName: string): Promise<McpToolTestResult>;

  // Scheduled Tasks
  listScheduledTasks(): Promise<ScheduledTaskInfo[]>;
  addScheduledTask(name: string, modeJson: string, prompt: string, model?: string | null, agent?: string | null, workspace?: string | null, enabled?: boolean, startDate?: string | null, endDate?: string | null, skills?: string[]): Promise<void>;
  removeScheduledTask(name: string): Promise<void>;
  updateScheduledTask(name: string, modeJson: string | null, prompt: string | null, model?: string | null, agent?: string | null, workspace?: string | null, enabled?: boolean | null, startDate?: string | null, endDate?: string | null, skills?: string[] | null): Promise<void>;
  runScheduledTaskNow(name: string): Promise<string>;
  getTaskRunHistory(name: string): Promise<TaskRunRecord[]>;
  listAllTaskRuns(): Promise<TaskRunRecordWithName[]>;
  /** WI-179: jobs registry — active scheduled runs carry `sessionId`. */
  listJobs?(): Promise<JobInfo[]>;
  getSessionTrace(sessionId: string): Promise<SessionTrace>;

  // Backend health — renderer asks "is the Rust backend alive?" on mount and
  // shows a full-screen error instead of a silent zombie UI when it isn't.
  backendHealth(): Promise<{ ok: boolean; error?: string | null }>;
  quitApp(): Promise<void>;
}

declare global {
  interface Window {
    starriver: StarriverBridge;
  }
}

/* ─── SkillsTab ─── */

type SkillFilter = 'all' | 'command' | 'keyword' | 'suffix';

function SkillsTab() {
  const [skills, setSkills] = useState<SkillInfo[]>([]);
  const [modal, setModal] = useState<{ mode: 'create' } | { mode: 'edit'; skill: SkillInfo } | null>(null);
  const [search, setSearch] = useState('');
  const [filter, setFilter] = useState<SkillFilter>('all');
  const [showUsage, setShowUsage] = useState(false);
  const [usage, setUsage] = useState<SkillUsageRecord[]>([]);

  const refresh = useCallback(async () => {
    try { setSkills(await window.starriver.listSkills()); } catch (e) { console.error(e); }
  }, []);

  useEffect(() => { refresh(); }, [refresh]);
  useEffect(() => {
    if (showUsage) window.starriver.listSkillUsage(100).then(setUsage).catch(() => {});
  }, [showUsage]);

  const handleScan = async () => {
    try { await window.starriver.scanSkills(); await refresh(); } catch (e) { console.error(e); }
  };

  // Toggle = "always-on" (resident): full body injected every session. Skills are otherwise
  // on-demand (description in prompt, body loads via slash command).
  const handleAlwaysOn = async (s: SkillInfo) => {
    try { setSkills(await window.starriver.setSkillAlwaysOn(s.name, !s.always_on)); } catch (e) { console.error(e); }
  };

  const handleDelete = async (s: SkillInfo) => {
    if (!confirm(`Delete skill "${s.name}"?`)) return;
    try { setSkills(await window.starriver.deleteSkill(s.name)); } catch (e) { console.error(e); }
  };

  const q = search.trim().toLowerCase();
  const filtered = skills.filter((s) => {
    if (q && !s.name.toLowerCase().includes(q) && !s.description.toLowerCase().includes(q)) return false;
    if (filter === 'command' && !s.has_command) return false;
    if (filter === 'keyword' && !s.has_keyword) return false;
    if (filter === 'suffix' && !s.has_suffix) return false;
    return true;
  });

  const filterBtn = (f: SkillFilter, label: string) => (
    <button onClick={() => setFilter(f)} style={{
      padding: '3px 10px', fontSize: 12, cursor: 'pointer', borderRadius: 4,
      border: `1px solid ${filter === f ? 'var(--accent)' : 'var(--border-strong)'}`,
      background: filter === f ? 'var(--accent-soft)' : 'var(--surface)', color: filter === f ? 'var(--accent)' : 'var(--text-secondary)',
      fontWeight: 600,
    }}>{label}</button>
  );

  return (
    <div className="skills-container">
      <div className="skills-header">
        <h3>Skills</h3>
        <div style={{ display: 'flex', gap: 6 }}>
          <button className="btn btn-secondary btn-small" onClick={() => setShowUsage(v => !v)}>
            {showUsage ? 'Skills' : 'Usage'}
          </button>
          <button className="btn btn-primary btn-small" onClick={() => setModal({ mode: 'create' })}>+ New Skill</button>
          <button className="btn btn-secondary" onClick={handleScan}>Scan</button>
        </div>
      </div>
      {showUsage && (
        <div style={{ margin: '0 24px 12px', fontSize: 12 }}>
          {usage.length === 0 && <p className="empty-text">No skill usage recorded yet.</p>}
          {usage.map((r, i) => (
            <div key={i} style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', padding: '6px 10px', borderBottom: '1px solid var(--border)', background: 'var(--surface)' }}>
              <div>
                <span style={{ fontWeight: 600, color: 'var(--accent)' }}>{r.skill}</span>
                {r.version && <span style={{ color: 'var(--text-muted)', fontSize: 11, marginLeft: 4 }}>v{r.version}</span>}
                <span style={{ color: 'var(--text-muted)', fontSize: 12, marginLeft: 8 }}>{r.mode}</span>
              </div>
              <span style={{ color: 'var(--text-muted)', fontSize: 11 }}>
                {r.ts ? new Date(r.ts * 1000).toLocaleString() : ''}
              </span>
            </div>
          ))}
        </div>
      )}
      <div style={{ display: 'flex', gap: 6, alignItems: 'center', marginBottom: 8, padding: '0 24px' }}>
        <input value={search} onChange={(e) => setSearch(e.target.value)} placeholder="Search skills..."
          style={{ padding: '5px 10px', border: '1px solid var(--border-strong)', borderRadius: 4, fontSize: 13, outline: 'none', width: 180 }} />
        {filterBtn('all', 'All')}
        {filterBtn('command', '/ command')}
        {filterBtn('keyword', 'keyword')}
        {filterBtn('suffix', '.suffix')}
      </div>
      {modal && (
        <CreateSkillModal
          key={modal.mode === 'edit' ? modal.skill.name : 'new'}
          initial={modal.mode === 'edit' ? modal.skill : undefined}
          onClose={() => setModal(null)}
          onSaved={(list) => { setModal(null); setSkills(list); }}
        />
      )}
      {filtered.length === 0 && <p className="empty-text">No skills found. Click "+ New Skill" to create one.</p>}
      <ul className="skill-list">
        {filtered.map((s) => (
          <li key={s.name} className="skill-item">
            <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', gap: 8 }}>
              <div style={{ display: 'flex', alignItems: 'center', gap: 6, flex: 1, minWidth: 0 }}>
                <span className="skill-name">{s.name}</span>
                {s.version && <span style={{ fontSize: 11, color: 'var(--text-muted)' }}>v{s.version}</span>}
              </div>
              {/* Always-on toggle: body injected every session. Off = on-demand. */}
              <div onClick={() => handleAlwaysOn(s)} role="switch" aria-checked={!!s.always_on} title="Always-on: inject full body into every prompt"
                style={{ width: 34, height: 19, borderRadius: 10, cursor: 'pointer', flexShrink: 0, background: s.always_on ? 'var(--accent)' : 'var(--border-strong)', transition: 'background 0.2s', position: 'relative' }}>
                <div style={{ width: 15, height: 15, borderRadius: 8, background: 'var(--surface)', position: 'absolute', top: 2, left: s.always_on ? 17 : 2, transition: 'left 0.2s' }} />
              </div>
            </div>
            <div className="skill-desc">{s.description}</div>
            <div style={{ display: 'flex', gap: 4, marginTop: 6, flexWrap: 'wrap' }}>
              {s.always_on && <span className="badge">always-on</span>}
              {s.has_command && <span className="badge">/ command</span>}
              {s.has_keyword && <span className="badge">keyword</span>}
              {s.has_suffix && <span className="badge">.suffix</span>}
              {s.has_hooks && <span className="badge">hooks</span>}
            </div>
            {s.body && (
              <details style={{ marginTop: 6 }}>
                <summary style={{ cursor: 'pointer', fontSize: 12, color: 'var(--accent)' }}>Preview instructions</summary>
                <pre style={{ fontSize: 11, whiteSpace: 'pre-wrap', wordBreak: 'break-all', background: 'var(--panel-bg)', padding: 8, borderRadius: 4, margin: '6px 0 0', maxHeight: 200, overflowY: 'auto' }}>{s.body}</pre>
              </details>
            )}
            <div style={{ display: 'flex', gap: 6, marginTop: 6 }}>
              <button className="btn btn-secondary btn-small" onClick={() => setModal({ mode: 'edit', skill: s })}>Edit</button>
              <button className="btn btn-secondary btn-small" style={{ color: 'var(--danger)' }} onClick={() => handleDelete(s)}>Delete</button>
            </div>
          </li>
        ))}
      </ul>
    </div>
  );
}

/* ─── CreateSkillModal (create + edit) ─── */

function CreateSkillModal({ onClose, onSaved, initial }: {
  onClose: () => void;
  onSaved: (list: SkillInfo[]) => void;
  initial?: SkillInfo;
}) {
  const [name, setName] = useState(initial?.name ?? '');
  const [description, setDescription] = useState(initial?.description ?? '');
  const [command, setCommand] = useState('');
  const [body, setBody] = useState(initial?.body ?? '');
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const editing = !!initial;

  // Extract command from the SKILL.md body frontmatter (best-effort) for edit mode.
  useEffect(() => {
    if (!initial || !initial.body) return;
    const m = initial.body.match(/^command:\s*(\S+)/m);
    if (m) setCommand(m[1]);
  }, [initial]);

  const handleSave = async () => {
    setError(null);
    if (!name.trim()) { setError('Skill name is required'); return; }
    if (!body.trim()) { setError('Skill instructions (body) cannot be empty'); return; }
    setSaving(true);
    try {
      const list = editing
        ? await window.starriver.updateSkill(name.trim(), description.trim(), command.trim() || undefined, body)
        : await window.starriver.createSkill(name.trim(), description.trim(), command.trim() || undefined, body);
      onSaved(list);
    } catch (e: any) {
      setError(e?.message || String(e));
    } finally {
      setSaving(false);
    }
  };

  const label: React.CSSProperties = { fontSize: 12, color: 'var(--text-secondary)', display: 'block', marginBottom: 4 };
  const input: React.CSSProperties = { width: '100%', padding: '6px 10px', border: '1px solid var(--border-strong)', borderRadius: 5, fontSize: 14, outline: 'none', fontFamily: 'inherit', boxSizing: 'border-box' };

  return (
    <div style={{
      position: 'fixed', inset: 0, background: 'var(--overlay-strong)',
      display: 'flex', alignItems: 'center', justifyContent: 'center', zIndex: 1000,
    }} onClick={onClose}>
      <div style={{
        background: 'var(--surface)', borderRadius: 10, width: 540, maxHeight: '85vh', overflowY: 'auto',
        boxShadow: 'var(--shadow-md)',
      }} onClick={(e) => e.stopPropagation()}>
        <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', padding: '16px 20px 0' }}>
          <h3 style={{ margin: 0, fontSize: 15 }}>{editing ? 'Edit Skill' : 'New Skill'}</h3>
          <button onClick={onClose} style={{ background: 'none', border: 'none', fontSize: 18, cursor: 'pointer', color: 'var(--text-muted)' }}>✕</button>
        </div>

        <div style={{ padding: '16px 20px', display: 'flex', flexDirection: 'column', gap: 10 }}>
          {error && (
            <div style={{ padding: '8px 12px', background: 'var(--danger-soft)', border: '1px solid var(--danger-border)', borderRadius: 6, color: 'var(--danger)', fontSize: 13 }}>{error}</div>
          )}
          <div>
            <label style={label}>Name <span style={{ color: 'var(--danger)' }}>*</span></label>
            <input value={name} onChange={(e) => setName(e.target.value)} placeholder="write-book (letters, digits, - _)" style={input} disabled={editing} />
            {editing && <div style={{ fontSize: 11, color: 'var(--text-muted)', marginTop: 2 }}>Name is the directory and cannot be renamed — recreate the skill instead.</div>}
          </div>
          <div>
            <label style={label}>Description</label>
            <input value={description} onChange={(e) => setDescription(e.target.value)} placeholder="Write books and long articles with a structured workflow" style={input} />
          </div>
          <div>
            <label style={label}>Slash Command (optional)</label>
            <input value={command} onChange={(e) => setCommand(e.target.value)} placeholder="/write-book" style={input} />
          </div>
          <div>
            <label style={label}>Instructions (body) <span style={{ color: 'var(--danger)' }}>*</span></label>
            <textarea value={body} onChange={(e) => setBody(e.target.value)} rows={10} placeholder={'## Workflow\n\n1. Ask about topic, audience, length\n2. Outline chapters\n3. Write each chapter to its own file\n4. Merge and review\n\n## Output\n\nWrite into a book/ directory...'}
              style={{ ...input, fontFamily: 'monospace', resize: 'vertical' }} />
          </div>
        </div>

        <div style={{ display: 'flex', justifyContent: 'flex-end', gap: 8, padding: '0 20px 16px' }}>
          <button className="btn btn-secondary btn-small" onClick={onClose}>Cancel</button>
          <button className="btn btn-primary btn-small" onClick={handleSave} disabled={saving}
            style={{ opacity: saving ? 0.6 : 1, cursor: saving ? 'wait' : 'pointer' }}>
            {saving ? 'Saving...' : editing ? 'Save Changes' : 'Create Skill'}
          </button>
        </div>
      </div>
    </div>
  );
}

/* ─── PluginsTab ─── */

function PluginsTab() {
  const [plugins, setPlugins] = useState<PluginEntry[]>([]);
  const [segment, setSegment] = useState<'installed' | 'market'>('installed');
  const [market, setMarket] = useState<any[]>([]);
  const [installUrl, setInstallUrl] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const refreshPlugins = useCallback(async () => {
    try { setPlugins(await window.starriver.listPlugins()); } catch (e) { console.error(e); }
  }, []);
  const refreshMarket = useCallback(async () => {
    try { setMarket(await window.starriver.marketList()); } catch (e) { console.error(e); }
  }, []);
  useEffect(() => { refreshPlugins(); refreshMarket(); }, [refreshPlugins, refreshMarket]);

  const handleInstall = async (url: string) => {
    if (!url.trim()) return;
    setBusy(true); setError(null);
    try {
      await window.starriver.installPlugin(url.trim());
      await refreshPlugins();
      await syncPluginTabs();
    } catch (e: any) { setError(e?.message || String(e)); }
    finally { setBusy(false); }
  };

  const handleUninstall = async (p: PluginEntry) => {
    if (!confirm(`Uninstall plugin "${p.name}" and remove its ${p.skills.length} skill(s)?`)) return;
    try {
      setPlugins(await window.starriver.uninstallPlugin(p.name));
      await syncPluginTabs();
    } catch (e) { console.error(e); }
  };

  const segBtn = (s: 'installed' | 'market', label: string) => (
    <button onClick={() => setSegment(s)} style={{
      padding: '5px 14px', fontSize: 13, cursor: 'pointer', borderRadius: '4px 4px 0 0',
      border: 'none', borderBottom: `2px solid ${segment === s ? 'var(--accent)' : 'transparent'}`,
      background: 'none', color: segment === s ? 'var(--accent)' : 'var(--text-muted)', fontWeight: 600,
    }}>{label}</button>
  );

  return (
    <div style={{ padding: '16px 24px', height: '100%', overflowY: 'auto' }}>
      <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginBottom: 8 }}>
        <h3 style={{ margin: 0, fontSize: 16 }}>Plugins</h3>
        <button className="btn btn-secondary btn-small" onClick={refreshPlugins}>Refresh</button>
      </div>
      <div style={{ display: 'flex', gap: 4, borderBottom: '1px solid var(--border)', marginBottom: 12 }}>
        {segBtn('installed', `Installed (${plugins.length})`)}
        {segBtn('market', 'Market')}
      </div>
      {error && <div style={{ padding: '8px 12px', marginBottom: 8, background: 'var(--danger-soft)', border: '1px solid var(--danger-border)', borderRadius: 6, color: 'var(--danger)', fontSize: 12 }}>{error}</div>}

      {segment === 'installed' && (
        <>
          <div style={{ display: 'flex', gap: 6, marginBottom: 12 }}>
            <input value={installUrl} onChange={(e) => setInstallUrl(e.target.value)} placeholder="Install from local dir or git URL"
              style={{ flex: 1, padding: '6px 10px', border: '1px solid var(--border-strong)', borderRadius: 5, fontSize: 13, outline: 'none' }} />
            <button className="btn btn-primary btn-small" disabled={busy} onClick={() => handleInstall(installUrl)}>{busy ? 'Installing...' : 'Install'}</button>
          </div>
          {plugins.length === 0 && <p className="empty-text">No plugins installed. Install from the Market or a URL above.</p>}
          {plugins.map((p) => (
            <div key={p.name} style={{ padding: '10px 14px', marginBottom: 6, borderRadius: 6, background: 'var(--surface)', border: '1px solid var(--border)' }}>
              <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between' }}>
                <div>
                  <span style={{ fontWeight: 600, fontSize: 13 }}>{p.name}</span>
                  {p.version && <span style={{ fontSize: 11, color: 'var(--text-muted)', marginLeft: 6 }}>v{p.version}</span>}
                  {(p as any).ui && <span style={{ fontSize: 11, color: 'var(--accent)', marginLeft: 6, background: 'var(--accent-soft)', borderRadius: 3, padding: '1px 5px' }}>UI</span>}
                </div>
                <button className="btn btn-secondary btn-small" style={{ fontSize: 12, padding: '3px 10px', color: 'var(--danger)' }} onClick={() => handleUninstall(p)}>Uninstall</button>
              </div>
              <div style={{ fontSize: 12, color: 'var(--text-muted)', marginTop: 4 }}>{p.description}</div>
              <div style={{ fontSize: 12, color: 'var(--text-muted)', marginTop: 2 }}>{p.skills.length} skill(s): {p.skills.join(', ')}</div>
            </div>
          ))}
        </>
      )}

      {segment === 'market' && (
        <>
          {market.length === 0 && <p className="empty-text">Market is empty.</p>}
          {market.map((e) => (
            <div key={e.name} style={{ padding: '10px 14px', marginBottom: 6, borderRadius: 6, background: 'var(--surface)', border: '1px solid var(--border)' }}>
              <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between' }}>
                <div>
                  <span style={{ fontWeight: 600, fontSize: 13 }}>{e.name}</span>
                  <span style={{ fontSize: 11, color: 'var(--text-muted)', marginLeft: 6 }}>{e.entry_type}</span>
                  {e.version && <span style={{ fontSize: 11, color: 'var(--text-muted)', marginLeft: 4 }}>v{e.version}</span>}
                </div>
                <button className="btn btn-primary btn-small" disabled={busy} onClick={() => handleInstall(e.url)}>Install</button>
              </div>
              <div style={{ fontSize: 12, color: 'var(--text-muted)', marginTop: 4 }}>{e.description}</div>
            </div>
          ))}
        </>
      )}
    </div>
  );
}

/* ─── ModelsTab ─── */


/* ─── Models settings page (ui-settings-models parity) ─── */

// Black-and-white stroke icons for the API Key show/hide toggle (CC Switch style).
// Ciphertext (masked): ICON_EYE — open eye, click to reveal. Plaintext (visible): ICON_EYE_SLASH —
// open eye + backslash, click to hide.
const ICON_EYE =
  '<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round"><path d="M1.5 8S4 3.5 8 3.5 14.5 8 14.5 8 12 12.5 8 12.5 1.5 8 1.5 8z"/><circle cx="8" cy="8" r="2.2"/></svg>';
const ICON_EYE_SLASH =
  '<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round"><path d="M1.5 8S4 3.5 8 3.5 14.5 8 14.5 8 12 12.5 8 12.5 1.5 8 1.5 8z"/><circle cx="8" cy="8" r="2.2"/><path d="M3.5 3.5L12.5 12.5"/></svg>';

/** Accepted capacity spellings: a decimal count with an optional K/M suffix. */
const CAPACITY_PATTERN = /^(\d+(?:\.\d+)?)([km])?$/i;
/** Decimal suffix scales — `1M` is 1000K, matching how model capacities are quoted. */
const CAPACITY_SCALE = { k: 1_000, m: 1_000_000 } as const;

/** Read a typed capacity (`256K`/`1M`) as a token count; `undefined` = blank (unset). */
export function parseCapacity(text: string): number | undefined {
  const trimmed = text.trim();
  if (trimmed.length === 0) return undefined;
  const match = CAPACITY_PATTERN.exec(trimmed);
  if (match === null) return Number.NaN;
  const suffix = match[2]?.toLowerCase();
  const scale = suffix === 'k' || suffix === 'm' ? CAPACITY_SCALE[suffix] : 1;
  const scaled = Number(match[1]) * scale;
  // A decimal multiple is exact in intent but not in binary floating point,
  // so an integral intent snaps back.
  const rounded = Math.round(scaled);
  return Math.abs(scaled - rounded) < 1e-6 ? rounded : scaled;
}

/** Spell a stored count in the shortest K/M form that survives parseCapacity. */
export function formatCapacity(value: number): string {
  if (!Number.isInteger(value) || value <= 0) return String(value);
  if (value % CAPACITY_SCALE.m === 0) return `${value / CAPACITY_SCALE.m}M`;
  if (value % CAPACITY_SCALE.k === 0) return `${value / CAPACITY_SCALE.k}K`;
  return String(value);
}

/** One catalog row's validation failure, named by position. */
export interface CatalogFailure {
  index: number;
  key: 'modelIdRequired' | 'modelIdDuplicate' | 'modelContextInvalid';
}

/** Constraints the form enforces before any write (same rules as). */
export function validateCatalog(models: ModelVariant[]): CatalogFailure | undefined {
  const seen = new Set<string>();
  for (const [index, model] of models.entries()) {
    // Compared trimmed: surrounding whitespace is a paste artifact the
    // provider would never match.
    const id = model.id.trim();
    if (id.length === 0) return { index, key: 'modelIdRequired' };
    if (seen.has(id)) return { index, key: 'modelIdDuplicate' };
    seen.add(id);
    if (model.contextLength !== undefined
      && (!Number.isInteger(model.contextLength) || model.contextLength <= 0)) {
      return { index, key: 'modelContextInvalid' };
    }
  }
  return undefined;
}

/**
 * A provider route id usable as a registry id AND as the stem of a credential
 * name (`store_api_key` derives `model/<id>` records from it) — the leading
 * letter keeps derived shell identifiers valid.
 */
export const MODEL_ROUTE_PATTERN = /^[a-z][a-z0-9]*(?:-[a-z0-9]+)*$/;

/** credential parity: a stored reference is never resolved back into the UI. */
const isKeyRef = (s: string) => s.startsWith('!file:') || s.startsWith('!keychain:') || s.startsWith('${');

/** Row-joined key fact behind a provider name: green = configured, red = missing. */
function CredentialDot({ state }: { state: 'configured' | 'missing' | undefined }) {
  const { t } = useI18n();
  if (!state) return null;
  const configured = state === 'configured';
  const label = configured ? t('models.keyConfigured') : t('models.keyMissing');
  return (
    <span role="img" aria-label={label} title={label} style={{
      display: 'inline-block', width: 8, height: 8, borderRadius: '50%', flex: 'none', marginLeft: 7,
      background: configured ? 'var(--success)' : 'var(--danger)',
    }} />
  );
}

/** What the fetch-available-models probe sends: the form's live endpoint facts. */
interface CatalogProbe { baseUrl: string; apiKey: string; engine?: 'ollama' | 'lmstudio' }

/** Per-field editing-buffer key (row index + field), so keystrokes survive re-renders. */
const capBufferKey = (index: number) => `${index}:contextLength`;

/**
 * Model catalog editor (unified list editor): a row = ID +
 * display name + default radio + capacity collapse + delete; "fetch available models" asks the
 * endpoint currently shown in the form, results land as checkable candidates (never silently
 * rewriting configured rows); an empty catalog shows the dashed placeholder.
 */
export function ModelCatalogEditor(props: {
  variants: ModelVariant[];
  /** Inherited preset catalog ids; undefined = the user owns the whole array. */
  presetModels: string[] | undefined;
  defaultVariant: string;
  disabled: boolean;
  probe: CatalogProbe;
  onChange: (variants: ModelVariant[]) => void;
  onReset: () => void;
  onDefaultChange: (id: string) => void;
}) {
  const { t } = useI18n();
  const [fetching, setFetching] = useState(false);
  const [fetchError, setFetchError] = useState<string | null>(null);
  const [candidates, setCandidates] = useState<{ id: string; name: string; contextLength?: number }[] | null>(null);
  const [picked, setPicked] = useState<ReadonlySet<string>>(new Set());
  const [expanded, setExpanded] = useState<ReadonlySet<number>>(new Set());
  // Capacity fields are edited as text: keystrokes are buffered here so
  // `1000` is not rewritten to `1K` mid-word; unreadable text stays on screen
  // past blur so the rejection names a row the user can still see.
  const [capText, setCapText] = useState<ReadonlyMap<string, string>>(new Map());

  // The user owns the array the moment its id set drifts from the preset's.
  // The user owns the array the moment its id set drifts from the preset's;
  // a preset without catalog defaults (Ollama) has nothing to inherit.
  const overridden = props.presetModels === undefined || props.presetModels.length === 0
    || props.variants.length !== props.presetModels.length
    || props.variants.some((v) => !props.presetModels!.includes(v.id));

  const patch = (index: number, next: Partial<ModelVariant>) => {
    props.onChange(props.variants.map((v, at) => (at === index ? { ...v, ...next } : v)));
  };

  const updateCapacity = (index: number, text: string) => {
    setCapText((current) => new Map(current).set(capBufferKey(index), text));
    const parsed = parseCapacity(text);
    patch(index, { contextLength: parsed === undefined || Number.isNaN(parsed) ? undefined : parsed });
  };

  const removeRow = (index: number) => {
    const removed = props.variants[index];
    props.onChange(props.variants.filter((_, at) => at !== index));
    if (removed && props.defaultVariant === removed.id) props.onDefaultChange('');
    // Both auxiliary stores are keyed by position — shift them with the rows.
    setExpanded((current) => {
      const next = new Set<number>();
      for (const at of current) { if (at < index) next.add(at); else if (at > index) next.add(at - 1); }
      return next;
    });
    setCapText((current) => {
      const next = new Map<string, string>();
      for (const [key, text] of current) {
        const at = Number(key.slice(0, key.indexOf(':')));
        if (at === index) continue;
        next.set(at > index ? key.replace(/^\d+/, String(at - 1)) : key, text);
      }
      return next;
    });
  };

  const reset = () => {
    setCapText(new Map());
    setExpanded(new Set());
    props.onReset();
  };

  // Ask the endpoint the form currently shows — including a key typed but not
  // yet saved — so adding a provider is one pass instead of save-then-return.
  const askable = props.probe.baseUrl.trim().length > 0;
  const handleFetch = async () => {
    setFetching(true);
    setFetchError(null);
    try {
      if (!window.starriver?.fetchProviderModels) {
        setFetchError('fetchProviderModels not available — please restart the app');
        return;
      }
      const result = await window.starriver.fetchProviderModels(
        props.probe.baseUrl.trim(), props.probe.apiKey.trim() || undefined, props.probe.engine);
      if (!result.ok) { setFetchError(result.error || t('models.fetchEmpty')); return; }
      if (result.models.length === 0) { setFetchError(t('models.fetchEmpty')); return; }
      // Everything already configured starts unchecked, so adopting a
      // selection never silently rewrites a row the user corrected.
      const known = new Set(props.variants.map((v) => v.id));
      setCandidates(result.models);
      setPicked(new Set(result.models.filter((m) => !known.has(m.id)).map((m) => m.id)));
    } catch (e: any) {
      setFetchError(String(e?.message || e));
    } finally {
      setFetching(false);
    }
  };

  const closePicker = () => { setCandidates(null); setPicked(new Set()); };

  const adoptPicked = () => {
    if (!candidates) return;
    // A row the user already tuned wins over the provider's own numbers.
    const byId = new Map(props.variants.map((v) => [v.id, v]));
    let firstNew = '';
    for (const c of candidates) {
      if (!picked.has(c.id) || byId.has(c.id)) continue;
      if (!firstNew) firstNew = c.id;
      byId.set(c.id, { id: c.id, name: c.name || c.id, contextLength: c.contextLength });
    }
    const next = [...byId.values()];
    props.onChange(next);
    if (!props.defaultVariant && next.length > 0) props.onDefaultChange(firstNew || next[0].id);
    closePicker();
  };

  const toggleCandidate = (id: string) => {
    setPicked((current) => {
      const next = new Set(current);
      if (!next.delete(id)) next.add(id);
      return next;
    });
  };

  const allPicked = (candidates ?? []).length > 0 && (candidates ?? []).every((c) => picked.has(c.id));

  return (
    <section aria-label={t('models.catalog')} style={{
      display: 'flex', flexDirection: 'column', gap: 10, borderTop: '1px solid var(--border)', paddingTop: 10,
    }}>
      <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'flex-start', gap: 12 }}>
        <div>
          <div style={{ fontSize: 13, fontWeight: 600, color: 'var(--text-secondary)' }}>{t('models.catalog')}</div>
          {props.presetModels !== undefined && (
            <div style={{ fontSize: 12, color: 'var(--text-muted)', marginTop: 2 }}>
              {overridden ? t('models.modelsCustomized') : t('models.modelsInherited')}
            </div>
          )}
        </div>
        <div style={{ display: 'flex', gap: 4 }}>
          {overridden && props.presetModels !== undefined && (
            <button type="button" className="btn btn-secondary btn-small" disabled={props.disabled}
              onClick={reset} style={{ fontSize: 12, padding: '2px 8px' }}>
              {t('models.resetModels')}
            </button>
          )}
          <button type="button" className="btn btn-secondary btn-small" title={
            !askable ? t('models.fetchNeedsBaseUrl') : undefined
          } disabled={props.disabled || fetching || !askable}
            onClick={() => { void handleFetch(); }} style={{ fontSize: 12, padding: '2px 8px' }}>
            {fetching ? t('models.fetching') : t('models.fetchModels')}
          </button>
        </div>
      </div>

      {props.variants.length === 0 && (
        <p style={{
          margin: 0, padding: 12, border: '1px dashed var(--border-strong)', borderRadius: 8,
          textAlign: 'center', fontSize: 13, color: 'var(--text-muted)',
        }}>{t('models.modelsEmpty')}</p>
      )}

      {props.variants.map((v, index) => (
        <div key={index} style={{ border: '1px solid var(--border)', borderRadius: 8, padding: 6 }}>
          <div style={{ display: 'grid', gridTemplateColumns: 'minmax(0,1.4fr) minmax(0,1fr) auto auto auto', gap: 6, alignItems: 'center' }}>
            <input placeholder={t('models.modelId')} value={v.id} aria-label={`${t('models.modelId')} ${index + 1}`}
              disabled={props.disabled} style={{ ...inputStyle, height: 30 }}
              onChange={(e) => patch(index, { id: e.target.value })}
              onBlur={(e) => {
                const trimmed = e.target.value.trim();
                if (trimmed !== e.target.value) patch(index, { id: trimmed });
              }} />
            <input placeholder={t('models.modelNamePlaceholder')} value={v.name}
              aria-label={`${t('models.modelName')} ${index + 1}`} disabled={props.disabled}
              style={{ ...inputStyle, height: 30 }}
              onChange={(e) => patch(index, { name: e.target.value })} />
            <input type="radio" name="catalog-default-variant" title={t('models.defaultModel')}
              aria-label={`${t('models.defaultModel')} ${index + 1}`} checked={props.defaultVariant === v.id}
              onChange={() => {
                props.onDefaultChange(v.id);
                if (v.contextLength) patch(index, { contextLength: v.contextLength });
              }} />
            <button type="button" aria-label={`${t('models.advancedToggle')} ${index + 1}`}
              aria-expanded={expanded.has(index)} title={t('models.advancedToggle')} disabled={props.disabled}
              onClick={() => setExpanded((current) => {
                const next = new Set(current);
                if (!next.delete(index)) next.add(index);
                return next;
              })}
              style={{ background: 'none', border: 'none', cursor: 'pointer', color: 'var(--text-muted)', fontSize: 13, padding: '0 2px' }}>
              {expanded.has(index) ? '▾' : '▸'}
            </button>
            <button type="button" aria-label={`${t('models.removeModel')} ${index + 1}`} title={t('models.removeModel')}
              disabled={props.disabled} onClick={() => removeRow(index)}
              style={{ background: 'none', border: 'none', cursor: 'pointer', color: 'var(--danger)', fontSize: 14, padding: '0 2px' }}>
              ✕
            </button>
          </div>
          {expanded.has(index) && (
            <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(160px, 1fr))', gap: 8, padding: '8px 4px 2px' }}>
              <label style={{ display: 'flex', flexDirection: 'column', gap: 4 }}>
                <span style={{ fontSize: 12, color: 'var(--text-muted)' }}>{t('models.contextWindow')}</span>
                <input type="text" inputMode="numeric" disabled={props.disabled} style={{ ...inputStyle, height: 30 }}
                  value={capText.get(capBufferKey(index)) ?? (v.contextLength != null ? formatCapacity(v.contextLength) : '')}
                  placeholder={t('models.contextWindowPlaceholder')}
                  aria-label={`${t('models.contextWindow')} ${index + 1}`}
                  onChange={(e) => updateCapacity(index, e.target.value)}
                  onBlur={() => {
                    // Unreadable text stays until the user fixes it; readable
                    // text settles back to the stored spelling.
                    const key = capBufferKey(index);
                    const text = capText.get(key);
                    if (text === undefined) return;
                    const parsed = parseCapacity(text);
                    if (parsed !== undefined && Number.isNaN(parsed)) return;
                    setCapText((current) => {
                      const next = new Map(current);
                      next.delete(key);
                      return next;
                    });
                  }} />
              </label>
            </div>
          )}
        </div>
      ))}

      <button type="button" className="btn btn-secondary btn-small" disabled={props.disabled}
        onClick={() => { props.onChange([...props.variants, { id: '', name: '' }]); }}
        style={{ alignSelf: 'flex-start', fontSize: 12, padding: '3px 10px' }}>
        ＋ {t('models.addModel')}
      </button>

      {fetchError && <p role="alert" style={{ margin: 0, fontSize: 13, color: 'var(--danger)' }}>{fetchError}</p>}

      {candidates && (
        <div style={{
          position: 'fixed', inset: 0, background: 'var(--overlay-strong)', zIndex: 1100,
          display: 'flex', alignItems: 'center', justifyContent: 'center',
        }} onClick={closePicker}>
          <div style={{
            background: 'var(--surface)', borderRadius: 10, width: 460, maxWidth: '92vw',
            maxHeight: '80vh', overflowY: 'auto', boxShadow: 'var(--shadow-md)', padding: 16,
          }} onClick={(e) => e.stopPropagation()}>
            <h4 style={{ margin: '0 0 4px', fontSize: 14 }}>{t('models.fetchTitle')}</h4>
            <p style={{ margin: '0 0 8px', fontSize: 13, color: 'var(--text-muted)' }}>{t('models.fetchDescription')}</p>
            <div style={{ display: 'flex', justifyContent: 'flex-end', marginBottom: 6 }}>
              <button type="button" className="btn btn-secondary btn-small" style={{ fontSize: 11 }}
                onClick={() => setPicked(allPicked ? new Set() : new Set(candidates.map((c) => c.id)))}>
                {allPicked ? t('models.fetchDeselectAll') : t('models.fetchSelectAll')}
              </button>
            </div>
            <ul style={{ listStyle: 'none', margin: 0, padding: 0, maxHeight: 300, overflowY: 'auto' }}>
              {candidates.map((c) => (
                <li key={c.id}>
                  <label style={{ display: 'flex', alignItems: 'center', gap: 8, padding: '5px 6px', cursor: 'pointer' }}>
                    <input type="checkbox" checked={picked.has(c.id)} onChange={() => toggleCandidate(c.id)} />
                    <span style={{ fontFamily: 'var(--font-mono, monospace)', fontSize: 13, overflowWrap: 'anywhere' }}>{c.id}</span>
                  </label>
                </li>
              ))}
            </ul>
            <div style={{ display: 'flex', justifyContent: 'flex-end', gap: 8, marginTop: 12 }}>
              <button className="btn btn-secondary btn-small" onClick={closePicker}>{t('models.cancel')}</button>
              <button className="btn btn-primary btn-small" onClick={adoptPicked}>{t('models.fetchAdopt')}</button>
            </div>
          </div>
        </div>
      )}
    </section>
  );
}

/** Shared footer for the two editor cards. */
function EditorFooter({ cancelLabel, submitLabel, submitBusy, busy, disabled, onCancel, onSubmit }: {
  cancelLabel: string; submitLabel: string; submitBusy: string;
  busy: boolean; disabled: boolean; onCancel: () => void; onSubmit: () => void;
}) {
  return (
    <div style={{ display: 'flex', justifyContent: 'flex-end', gap: 8 }}>
      <button className="btn btn-secondary btn-small" disabled={busy} onClick={onCancel}>{cancelLabel}</button>
      <button className="btn btn-primary btn-small" disabled={busy || disabled}
        style={{ opacity: busy ? 0.6 : 1, cursor: busy ? 'wait' : 'pointer' }} onClick={onSubmit}>
        {busy ? submitBusy : submitLabel}
      </button>
    </div>
  );
}

const fieldLabelStyle: React.CSSProperties = {
  fontSize: 13, fontWeight: 500, color: 'var(--text-secondary)', display: 'block', marginBottom: 4,
};

/**
 * One provider's edit card: the only primary field is the API key
 * input (saving it empty keeps the stored reference); the collapsed "custom settings" holds on-demand extras (API
 * URL, custom-route display name/protocol, Ollama model paths) and the model catalog; generation knobs
 * like reasoning effort live under "advanced". Saving goes through the models.add registry merge.
 */
export function ProviderEditorCard({ entry, preset, hideTitle, onClose }: {
  entry: ModelEntry | undefined;
  preset: ModelProvider | undefined;
  hideTitle?: boolean;
  onClose: (changed: boolean, name: string) => void;
}) {
  const { t } = useI18n();
  const id = entry?.id ?? preset?.id ?? '';
  const isLocal = (entry?.modelType ?? (preset?.providerType === 'local' ? 'local' : 'cloud')) === 'local';
  const providerType = entry?.providerType ?? (preset?.providerType === 'local' && preset ? preset.id : 'openai-compat');
  const engine = providerType === 'ollama' ? 'ollama' as const : providerType === 'lmstudio' ? 'lmstudio' as const : undefined;
  // Only custom routes (registry entries outside the adapter catalog) carry their own display name/protocol.
  const isCustom = preset === undefined;

  const [displayName, setDisplayName] = useState(entry?.name ?? preset?.name ?? id);
  const [baseUrl, setBaseUrl] = useState(entry?.baseUrl || preset?.baseUrl || '');
  const [modelPath, setModelPath] = useState(entry?.modelPath ?? '');
  const [variants, setVariants] = useState<ModelVariant[]>(
    entry?.models ?? (preset?.models ?? []).map((mid) => ({ id: mid, name: mid })));
  const [defaultVariant, setDefaultVariant] = useState(entry?.defaultModel || preset?.models?.[0] || '');
  const [concurrency, setConcurrency] = useState(entry?.concurrency != null ? String(entry.concurrency) : '');
  const [maxTokens, setMaxTokens] = useState(entry?.maxTokens != null ? String(entry.maxTokens) : '');
  const [contextWindow, setContextWindow] = useState(entry?.contextWindow != null ? String(entry.contextWindow) : '');
  const [temperature, setTemperature] = useState(entry?.temperature != null ? String(entry.temperature) : '');
  const [apiFormat, setApiFormat] = useState(entry?.apiFormat || preset?.apiFormat || 'openai-chat');
  const [reasoningEffort, setReasoningEffort] = useState(entry?.reasoningEffort ?? preset?.reasoningEffort ?? '');

  // GLM Coding Plan - CN (WI-148): the three Zhipu wire protocols map to
  // different base URLs. Switching the protocol jumps the base URL along —
  // but only while the URL still belongs to the GLM endpoint family, so a
  // hand-edited URL is never overwritten.
  const isGlmBaseUrl = (url: string) => url.startsWith('https://open.bigmodel.cn/');
  const glmUrlFor = (format: string) =>
    format === 'anthropic' ? 'https://open.bigmodel.cn/api/anthropic'
      : format === 'openai-responses' ? 'https://open.bigmodel.cn/api/v1'
        : 'https://open.bigmodel.cn/api/coding/paas/v4';
  const onApiFormatChange = (format: string) => {
    setApiFormat(format);
    if (isGlmBaseUrl(baseUrl) && glmUrlFor(format) !== baseUrl) setBaseUrl(glmUrlFor(format));
  };

 // credential parity: plaintext keys prefill; references are described,
  // never resolved back into the UI. Blank save preserves the stored reference.
  const [modelLabel, setModelLabel] = useState(entry?.modelLabel ?? '');
  const [keyDraft, setKeyDraft] = useState(() => {
    const k = entry?.apiKey ?? '';
    return k && !isKeyRef(k) ? k : '';
  });
  const [showKey, setShowKey] = useState(false);
  const [keyConfigured, setKeyConfigured] = useState(false);
  // WI: show the real secret — referenced keys are read back via credentials.read and prefilled (the input
  // defaults to masked, 👁 toggles). A new value is submitted only after an edit; untouched fields submit
  // empty and the host keeps the stored reference unchanged.
  const [keyTouched, setKeyTouched] = useState(false);
  useEffect(() => {
    const k = entry?.apiKey;
    // Plaintext keys prefill directly (napi path) — no credential lookup.
    if (k && !isKeyRef(k)) { setKeyConfigured(!!k); return; }
    // Protocol `models.list` strips the value (hasApiKey only); the napi path
    // carries the raw reference. Both arm describe/read-back so the stored
    // secret is prefilled and its status shown. A describe failure stays
    // optimistic via the protocol (presence is already declared by
    // hasApiKey) and pessimistic when we hold the raw reference (the record
    // itself may be gone).
    const optimistic = !k;
    if ((k || entry?.hasApiKey) && entry?.id) {
      window.starriver?.describeCredential?.(`model/${entry.id}`)
        .then((info) => setKeyConfigured(info ? !!info.configured : optimistic))
        .catch(() => setKeyConfigured(optimistic));
      window.starriver?.readCredential?.(`model/${entry.id}`)
        .then((value) => {
          if (value && !keyTouched) { setKeyDraft(value); setKeyConfigured(true); }
        })
        .catch(() => {});
    } else {
      setKeyConfigured(!!k);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [entry?.id]);

  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);

  const modelFailure = validateCatalog(variants);
  const modelFailureText = modelFailure
    ? `${t('models.modelItem')} ${modelFailure.index + 1}: ${t(`models.${modelFailure.key}`)}`
    : undefined;
  const keyPlaceholder = keyConfigured
    ? t('models.keyStored')
    // A saved reference must never read as "not configured": describe
    // failures stay neutral but still say a credential reference exists.
    : isKeyRef(entry?.apiKey ?? '')
      ? t('models.keyRefSaved')
      : t('models.keyPlaceholderNative');
  const baseUrlMissing = !isLocal && baseUrl.trim().length === 0;

  const handleSave = async () => {
    setSaveError(null);
    if (baseUrlMissing) { setSaveError(t('models.baseUrlRequired')); return; }
    if (modelFailure) return;
    // Prefer the selected/first variant's real id as the default model.
    const effectiveDefault = defaultVariant || variants[0]?.id || id;
    setSaving(true);
    try {
      await window.starriver.addModel(
        id, displayName.trim() || id,
        isLocal ? 'local' : 'cloud', providerType,
        baseUrl.trim() || undefined,
        // An untouched key field submits undefined: the host keeps the stored reference (the read-back
        // plaintext is display-only and must not rewrite the secret).
        (keyTouched ? keyDraft.trim() : '') || undefined,
        modelPath.trim() || undefined, effectiveDefault,
        variants.length > 0 ? JSON.stringify(variants) : undefined,
        concurrency ? parseInt(concurrency, 10) : undefined,
        maxTokens ? parseInt(maxTokens, 10) : undefined,
        contextWindow ? parseInt(contextWindow, 10) : undefined,
        temperature ? parseFloat(temperature) : undefined,
        apiFormat || undefined, reasoningEffort || undefined,
        modelLabel.trim() || undefined, undefined,
      );
      onClose(true, displayName.trim() || id);
    } catch (e: any) {
      setSaveError(String(e?.message || e) || 'Failed to save');
    } finally {
      setSaving(false);
    }
  };

  const title = preset?.name ?? entry?.name ?? id;
  const detailsSummary = (label: string) => (
    <summary style={{
      display: 'flex', alignItems: 'center', gap: 6, cursor: 'pointer', width: 'fit-content',
      fontSize: 13, fontWeight: 500, color: 'var(--text-secondary)', listStyle: 'none',
    }}>▸ {label}</summary>
  );

  return (
    <div style={{
      background: 'var(--surface)', borderRadius: 8, padding: '12px 14px', marginTop: 10,
      display: 'flex', flexDirection: 'column', gap: 12, border: '1px solid var(--border)',
    }}>
      {!hideTitle && (
        <div style={{ display: 'flex', alignItems: 'baseline', gap: 8 }}>
          <span style={{ fontWeight: 600, fontSize: 13 }}>{title}</span>
          {id !== title && <span style={{ fontSize: 12, color: 'var(--text-muted)' }}>{id}</span>}
        </div>
      )}
      {saveError && <p role="alert" style={{ margin: 0, fontSize: 13, color: 'var(--danger)' }}>{saveError}</p>}

      <div>
        <label style={fieldLabelStyle}>{t('models.modelLabel')}</label>
        <input value={modelLabel} aria-label={t('models.modelLabel')} disabled={saving}
          placeholder={t('models.modelLabelPlaceholder')} onChange={(e) => setModelLabel(e.target.value)} style={inputStyle} />
        <p style={{ margin: '4px 0 0', fontSize: 12, color: 'var(--text-muted)' }}>{t('models.modelLabelHint')}</p>
      </div>

      {!isLocal && (
        <div>
          <label style={fieldLabelStyle}>
            {t('models.keyInput')}
            {keyConfigured && (
              <span style={{ marginLeft: 8, fontSize: 12, color: 'var(--success)', fontWeight: 600 }}>
                {t('models.keyConfiguredBadge')}
              </span>
            )}
          </label>
          <div style={{ display: 'flex', gap: 8 }}>
            <input type={showKey ? 'text' : 'password'} autoComplete="off" placeholder={keyPlaceholder}
              aria-label={t('models.keyInput')} value={keyDraft} disabled={saving}
              onChange={(e) => { setKeyDraft(e.target.value); setKeyTouched(true); }} style={{ ...inputStyle, flex: 1 }} />
            <button type="button" onClick={() => setShowKey((s) => !s)} title={showKey ? '隐藏明文' : '显示明文'}
              style={{
                padding: '5px 10px', border: '1px solid var(--border-strong)', borderRadius: 5, cursor: 'pointer',
                background: 'var(--surface)', color: 'var(--text)', display: 'flex', alignItems: 'center',
              }}>
              <span dangerouslySetInnerHTML={{ __html: showKey ? ICON_EYE_SLASH : ICON_EYE }} />
            </button>
          </div>
        </div>
      )}

      <details>
        {detailsSummary(t('models.customized'))}
        <div style={{ display: 'flex', flexDirection: 'column', gap: 12, paddingTop: 10 }}>
          {isCustom && (
            <div>
              <label style={fieldLabelStyle}>{t('models.customDisplayName')}</label>
              <input value={displayName} aria-label={t('models.customDisplayName')} disabled={saving}
                placeholder={id} onChange={(e) => setDisplayName(e.target.value)} style={inputStyle} />
            </div>
          )}
          <div>
            <label style={fieldLabelStyle}>{t('models.baseUrl')}</label>
            <input value={baseUrl} aria-label={t('models.baseUrl')} disabled={saving}
              placeholder={preset?.baseUrl || t('models.baseUrlDefault')}
              onChange={(e) => setBaseUrl(e.target.value)} style={inputStyle} />
          </div>
          {providerType === 'ollama' && (
            <div>
              <label style={fieldLabelStyle}>{t('models.modelPath')}</label>
              <input placeholder="/path/to/model.gguf" value={modelPath} aria-label={t('models.modelPath')}
                disabled={saving} onChange={(e) => setModelPath(e.target.value)} style={inputStyle} />
            </div>
          )}

          <ModelCatalogEditor
            variants={variants}
            presetModels={preset?.models}
            defaultVariant={defaultVariant}
            disabled={saving}
            probe={{ baseUrl, apiKey: keyDraft, engine }}
            onChange={setVariants}
            onReset={() => setVariants((preset?.models ?? []).map((mid) => ({ id: mid, name: mid })))}
            onDefaultChange={setDefaultVariant}
          />
        </div>
      </details>

      <details>
        {detailsSummary(t('models.advancedToggle'))}
        <div style={{ display: 'grid', gridTemplateColumns: '1fr 1fr', gap: 10, paddingTop: 10 }}>
          <div>
            <label style={fieldLabelStyle}>{t('models.maxTokens')}</label>
            <input type="number" placeholder="8192" value={maxTokens} aria-label={t('models.maxTokens')}
              disabled={saving} onChange={(e) => setMaxTokens(e.target.value)} style={inputStyle} />
          </div>
          <div>
            <label style={fieldLabelStyle}>{t('models.contextWindow')}</label>
            <input type="number" placeholder="128000" value={contextWindow} aria-label={t('models.contextWindow')}
              disabled={saving} onChange={(e) => setContextWindow(e.target.value)} style={inputStyle} />
          </div>
          <div>
            <label style={fieldLabelStyle}>{t('models.temperature')}</label>
            <input type="number" step="0.1" min="0" max="2" placeholder="0.7" value={temperature}
              aria-label={t('models.temperature')} disabled={saving}
              onChange={(e) => setTemperature(e.target.value)} style={inputStyle} />
          </div>
          <div>
            <label style={fieldLabelStyle}>{t('models.concurrency')}</label>
            <input type="number" placeholder="4" value={concurrency} aria-label={t('models.concurrency')}
              disabled={saving} onChange={(e) => setConcurrency(e.target.value)} style={inputStyle} />
          </div>
          <div>
            <label style={fieldLabelStyle}>{t('models.apiFormat')}</label>
            <select value={apiFormat} aria-label={t('models.apiFormat')} disabled={saving}
              onChange={(e) => onApiFormatChange(e.target.value)} style={inputStyle}>
              <option value="openai-chat">OpenAI Chat</option>
              <option value="openai-responses">OpenAI Responses</option>
              <option value="anthropic">Anthropic Messages</option>
            </select>
          </div>
          <div>
            <label style={fieldLabelStyle}>{t('models.reasoningLabel')}</label>
            <select value={reasoningEffort} aria-label={t('models.reasoningLabel')} disabled={saving}
              onChange={(e) => setReasoningEffort(e.target.value)} style={inputStyle}>
              <option value="">{t('models.providerDefault')}</option>
              <option value="off">Off</option>
              <option value="low">Low</option>
              <option value="high">High</option>
              <option value="max">Max</option>
            </select>
          </div>
          <div style={{ display: 'flex', alignItems: 'end' }}>
            {/* WI-152 v2: image-input capability is automatic (built-in provider
                table) — the manual checkbox is gone. */}
          </div>
        </div>
        {reasoningEffort && reasoningEffort !== 'off' && (
          <div style={{ marginTop: 8, padding: '5px 8px', borderRadius: 6, fontSize: 12, color: 'var(--warning)', background: 'var(--warning-soft)' }}>
            {t('models.thinkingWarn')}
          </div>
        )}
      </details>

      {modelFailureText && <p style={{ margin: 0, fontSize: 13, color: 'var(--text-muted)' }}>{modelFailureText}</p>}

      <EditorFooter
        cancelLabel={t('models.cancel')}
        submitLabel={t('models.save')}
        submitBusy={t('models.applying')}
        busy={saving}
        disabled={!!modelFailure || baseUrlMissing}
        onCancel={() => onClose(false, displayName)}
        onSubmit={() => { void handleSave(); }}
      />
    </div>
  );
}

/**
 * Custom provider creation card: creation, not editing —
 * the route id is being picked, and endpoint, protocol, and at least one model are required;
 * whatever is missing is called out above the submit button; the key may stay empty (env-based auth).
 */
export function CustomProviderForm({ taken, onClose }: {
  taken: readonly string[];
  onClose: (changed: boolean, name: string) => void;
}) {
  const { t } = useI18n();
  const [route, setRoute] = useState('');
  const [displayName, setDisplayName] = useState('');
  const [baseUrl, setBaseUrl] = useState('');
  const [apiFormat, setApiFormat] = useState('openai-chat');
  const [modelLabel, setModelLabel] = useState('');
  const [keyDraft, setKeyDraft] = useState('');
  const [showKey, setShowKey] = useState(false);
  const [variants, setVariants] = useState<ModelVariant[]>([]);
  const [defaultVariant, setDefaultVariant] = useState('');
  const [saving, setSaving] = useState(false);
  const [failure, setFailure] = useState<string | null>(null);

  const routeInvalid = route.length > 0 && !MODEL_ROUTE_PATTERN.test(route);
  const routeTaken = taken.includes(route);
  const modelFailure = validateCatalog(variants);
  const ready = route.length > 0 && !routeInvalid && !routeTaken
    && baseUrl.trim().length > 0 && variants.length > 0 && modelFailure === undefined;
  // The one blocked gate worth a line under the form; a satisfied card (or a
  // field already showing its own failure) says nothing here.
  const hint = failure !== null || route.length === 0 || routeInvalid || routeTaken
    ? undefined
    : baseUrl.trim().length === 0
      ? t('models.customNeedsBaseUrl')
      : modelFailure
        ? `${t('models.modelItem')} ${modelFailure.index + 1}: ${t(`models.${modelFailure.key}`)}`
        : variants.length === 0 ? t('models.customNeedsModels') : undefined;

  const handleCreate = async () => {
    setFailure(null);
    setSaving(true);
    try {
      await window.starriver.addModel(
        route, displayName.trim() || route, 'cloud', 'openai-compat',
        baseUrl.trim() || undefined, keyDraft.trim() || undefined, undefined,
        defaultVariant || variants[0]?.id || route,
        variants.length > 0 ? JSON.stringify(variants) : undefined,
        undefined, undefined, undefined, undefined,
        apiFormat || undefined, undefined, modelLabel.trim() || undefined, false,
      );
      onClose(true, displayName.trim() || route);
    } catch (e: any) {
      setFailure(String(e?.message || e) || 'Failed to create');
    } finally {
      setSaving(false);
    }
  };

  return (
    <div style={{
      background: 'var(--surface)', borderRadius: 8, padding: '12px 14px',
      display: 'flex', flexDirection: 'column', gap: 12, border: '1px solid var(--border)',
    }}>
      <div style={{ fontWeight: 600, fontSize: 13 }}>{t('models.customTitle')}</div>
      {failure && <p role="alert" style={{ margin: 0, fontSize: 13, color: 'var(--danger)' }}>{failure}</p>}
      <div>
        <label style={fieldLabelStyle}>{t('models.customRoute')}</label>
        <input value={route} placeholder="acme-gateway" aria-label={t('models.customRoute')} disabled={saving}
          onChange={(e) => setRoute(e.target.value)} style={inputStyle} />
        {routeInvalid || routeTaken
          ? <p style={{ margin: '4px 0 0', fontSize: 13, color: 'var(--danger)' }}>
              {t(routeInvalid ? 'models.customRouteInvalid' : 'models.customRouteTaken')}
            </p>
          : <p style={{ margin: '4px 0 0', fontSize: 13, color: 'var(--text-muted)' }}>{t('models.customRouteHint')}</p>}
      </div>
      <div>
        <label style={fieldLabelStyle}>{t('models.customDisplayName')}</label>
        <input value={displayName} placeholder={route.length === 0 ? t('models.customDisplayName') : route}
          aria-label={t('models.customDisplayName')} disabled={saving}
          onChange={(e) => setDisplayName(e.target.value)} style={inputStyle} />
      </div>
      <div>
        <label style={fieldLabelStyle}>{t('models.baseUrl')}</label>
        <input value={baseUrl} placeholder={t('models.customBaseUrlPlaceholder')} aria-label={t('models.baseUrl')}
          disabled={saving} onChange={(e) => setBaseUrl(e.target.value)} style={inputStyle} />
      </div>
      <div>
        <label style={fieldLabelStyle}>{t('models.customApi')}</label>
        <select value={apiFormat} aria-label={t('models.customApi')} disabled={saving}
          onChange={(e) => setApiFormat(e.target.value)} style={{ ...inputStyle, maxWidth: 300 }}>
          <option value="openai-chat">OpenAI Chat</option>
          <option value="openai-responses">OpenAI Responses</option>
          <option value="anthropic">Anthropic Messages</option>
        </select>
      </div>
      <div>
        <label style={fieldLabelStyle}>{t('models.modelLabel')}</label>
        <input value={modelLabel} aria-label={t('models.modelLabel')} disabled={saving}
          placeholder={t('models.modelLabelPlaceholder')} onChange={(e) => setModelLabel(e.target.value)} style={inputStyle} />
        <p style={{ margin: '4px 0 0', fontSize: 12, color: 'var(--text-muted)' }}>{t('models.modelLabelHint')}</p>
      </div>
      <div>
        <label style={fieldLabelStyle}>{t('models.keyInput')}</label>
        <div style={{ display: 'flex', gap: 8 }}>
          <input type={showKey ? 'text' : 'password'} autoComplete="off" placeholder={t('models.keyPlaceholder')}
            aria-label={t('models.keyInput')} value={keyDraft} disabled={saving}
            onChange={(e) => setKeyDraft(e.target.value)} style={{ ...inputStyle, flex: 1 }} />
          <button type="button" onClick={() => setShowKey((s) => !s)} title={showKey ? '隐藏明文' : '显示明文'}
            style={{
              padding: '5px 10px', border: '1px solid var(--border-strong)', borderRadius: 5, cursor: 'pointer',
              background: 'var(--surface)', color: 'var(--text)', display: 'flex', alignItems: 'center',
            }}>
            <span dangerouslySetInnerHTML={{ __html: showKey ? ICON_EYE_SLASH : ICON_EYE }} />
          </button>
        </div>
      </div>
      <ModelCatalogEditor
        variants={variants}
        presetModels={undefined}
        defaultVariant={defaultVariant}
        disabled={saving}
        probe={{ baseUrl, apiKey: keyDraft }}
        onChange={setVariants}
        onReset={() => {}}
        onDefaultChange={setDefaultVariant}
      />
      {hint && <p style={{ margin: 0, fontSize: 13, color: 'var(--text-muted)' }}>{hint}</p>}
      <EditorFooter
        cancelLabel={t('models.cancel')}
        submitLabel={t('models.create')}
        submitBusy={t('models.creating')}
        busy={saving}
        disabled={!ready}
        onCancel={() => onClose(false, '')}
        onSubmit={() => { void handleCreate(); }}
      />
    </div>
  );
}

/**
 * Models settings page (screenshot parity): configured providers render as row cards
 * (name + key-status dot + edit/delete), with a pair of dashed entries at the bottom ("add provider /
 * add custom provider"); at most one card is expanded at a time, and closing one never drops another's draft.
 * Open config file: the system editor opens ~/.starriver/settings.yaml (the llm.providers
 * section is the persisted form of the model registry).
 */
export function ModelsTab() {
  const { t } = useI18n();
  const [models, setModels] = useState<ModelEntry[]>([]);
  const [presets, setPresets] = useState<ModelProvider[]>([]);
  const [directory, setDirectory] = useState<ConfigurableProvider[]>([]);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [adding, setAdding] = useState(false);
  const [addId, setAddId] = useState('');
  const [declaring, setDeclaring] = useState(false);
  const [armRemoveId, setArmRemoveId] = useState<string | null>(null);
  const [savedName, setSavedName] = useState<string | null>(null);
  const [configError, setConfigError] = useState<string | null>(null);
  const [keyStates, setKeyStates] = useState<Record<string, 'configured' | 'missing' | undefined>>({});

  const refresh = useCallback(async () => {
    try { setModels(await window.starriver.listModels()); } catch (e) { console.error(e); }
    try { setPresets(await window.starriver.listModelProviders?.() ?? []); } catch (e) { console.error(e); }
    try { setDirectory(await window.starriver.listConfigurableProviders?.() ?? []); } catch (e) { console.error(e); }
  }, []);
  useEffect(() => { refresh(); }, [refresh]);

  // Row-card key dot: plaintext keys are configured outright; referenced keys (napi) and the
  // protocol shape (`hasApiKey`, value stripped) are confirmed via describeCredential — a failed
  // describe stays quiet, same rule as the edit card's placeholder; empty = no dot, the provider
  // can use env auth or is a local engine that needs no key.
  useEffect(() => {
    let stale = false;
    const next: Record<string, 'configured' | 'missing' | undefined> = {};
    const pending: Promise<void>[] = [];
    for (const m of models) {
      if (!m.apiKey && !m.hasApiKey) continue;
      if (m.apiKey && !isKeyRef(m.apiKey)) { next[m.id] = 'configured'; continue; }
      // Referenced keys count as configured by default; describe only downgrades to missing when the record is truly gone.
      next[m.id] = 'configured';
      pending.push(
        Promise.resolve(window.starriver?.describeCredential?.(`model/${m.id}`))
          .then((info) => { if (info && !info.configured) next[m.id] = 'missing'; })
          .catch(() => {}),
      );
    }
    void Promise.all(pending).then(() => { if (!stale) setKeyStates(next); });
    return () => { stale = true; };
  }, [models]);

  // Rows = registry entries ∪ catalog entries marked configured; the catalog may be empty before the runtime is ready.
  const configuredDir = directory.filter((p) => p.configured);
  const rowIds: string[] = [];
  for (const m of models) if (!rowIds.includes(m.id)) rowIds.push(m.id);
  for (const p of configuredDir) if (!rowIds.includes(p.provider)) rowIds.push(p.provider);

  // Addable = catalog presets not yet configured; an empty catalog falls back to the built-in preset list (minus registered ones).
  const addable: { provider: string; displayName: string }[] = [];
  {
    const seen = new Set(rowIds);
    const source = directory.length > 0
      ? directory.filter((p) => !p.configured).map((p) => ({ provider: p.provider, displayName: p.displayName }))
      : presets.map((p) => ({ provider: p.id, displayName: p.name }));
    for (const c of source) {
      if (!seen.has(c.provider) && !addable.some((a) => a.provider === c.provider)) addable.push(c);
    }
  }

  // WI-31: removing a provider also deletes its stored API key (host
  // delete_api_key) — an irreversible side effect that must be armed
  // explicitly, never a single-click or a native confirm().
  const handleRemoveModel = async (id: string) => {
    if (armRemoveId !== id) { setArmRemoveId(id); return; }
    setArmRemoveId(null);
    try { await window.starriver.removeModel(id); await refresh(); } catch (e) { console.error(e); }
  };

  // One card at a time: opening one closes the others, so closing either
  // never discards a draft in another.
  const openEdit = (id: string) => {
    setSavedName(null);
    setAdding(false);
    setDeclaring(false);
    setEditingId((cur) => (cur === id ? null : id));
  };
  const openAdd = () => {
    setSavedName(null);
    setEditingId(null);
    setDeclaring(false);
    setAdding(true);
    setAddId((cur) => (cur && addable.some((a) => a.provider === cur) ? cur : addable[0]?.provider ?? ''));
  };
  const openDeclare = () => {
    setSavedName(null);
    setAdding(false);
    setEditingId(null);
    setDeclaring(true);
  };
  const closeCard = (changed: boolean, name: string) => {
    setEditingId(null);
    setAdding(false);
    setDeclaring(false);
    if (changed) {
      setSavedName(name);
      void refresh();
    }
  };

  const openConfig = async () => {
    setConfigError(null);
    try {
      const r = await window.starriver?.openModelsConfig?.();
      if (r && !r.ok) setConfigError(r.error || t('models.openConfigError'));
    } catch {
      setConfigError(t('models.openConfigError'));
    }
  };

  const dashedButton = (label: string, onClick: () => void, disabled = false): React.ReactNode => (
    <button type="button" disabled={disabled} onClick={onClick} style={{
      flex: '1 1 0', minWidth: 180, height: 44, borderRadius: 10, cursor: disabled ? 'default' : 'pointer',
      border: '1px dashed var(--border-strong)', background: 'transparent', color: 'var(--text-secondary)',
      fontSize: 14, fontWeight: 500,
    }}>＋ {label}</button>
  );

  return (
    <div style={{ padding: '16px 24px', height: '100%', overflowY: 'auto', maxWidth: 780 }}>
      <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'flex-start', gap: 12 }}>
        <div>
          <h3 style={{ margin: 0, fontSize: 16 }}>{t('models.title')}</h3>
          <p style={{ margin: '4px 0 0 0', fontSize: 14, color: 'var(--text-muted)' }}>{t('models.intro')}</p>
        </div>
        <button className="btn btn-secondary btn-small" style={{ flex: 'none' }} onClick={() => { void openConfig(); }}>
          {t('models.openConfig')}
        </button>
      </div>

      {configError && <p role="alert" style={{ margin: '8px 0 0', fontSize: 13, color: 'var(--danger)' }}>{configError}</p>}
      {savedName && (
        <p role="status" style={{ margin: '8px 0 0', fontSize: 13, color: 'var(--success)' }}>
          {t('models.savedProvider', { provider: savedName })}
        </p>
      )}

      <div style={{ display: 'flex', flexDirection: 'column', gap: 8, marginTop: 16 }}>
        {rowIds.map((id) => {
          const entry = models.find((m) => m.id === id);
          const dir = directory.find((p) => p.provider === id);
          const name = dir?.displayName ?? entry?.name ?? id;
          const custom = !presets.some((p) => p.id === id);
          const open = editingId === id;
          return (
            <div key={id} style={{
              border: '1px solid var(--border)', borderRadius: 10, padding: '10px 14px',
              display: 'flex', flexDirection: 'column',
            }}>
              <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', gap: 10 }}>
                <div style={{ display: 'flex', alignItems: 'center', minWidth: 0 }}>
                  <span style={{ fontWeight: 600, fontSize: 13 }}>{name}</span>
                  {custom && (
                    <span style={{
                      marginLeft: 8, padding: '1px 6px', border: '1px solid var(--border-strong)',
                      borderRadius: 4, fontSize: 12, color: 'var(--text-secondary)', flex: 'none',
                    }}>{t('models.customTag')}</span>
                  )}
                  <CredentialDot state={keyStates[id]} />
                </div>
                <div style={{ display: 'flex', gap: 6 }}>
                  <button className="btn btn-secondary btn-small" style={{ fontSize: 12, padding: '3px 10px' }}
                    aria-label={`${t('models.edit')} ${name}`} onClick={() => openEdit(id)}>
                    {t('models.edit')}
                  </button>
                  <button
                    className={armRemoveId === id ? 'btn btn-danger btn-small' : 'btn btn-secondary btn-small'}
                    style={{ fontSize: 12, padding: '3px 10px' }}
                    aria-label={`${t('models.remove')} ${name}`}
                    title={armRemoveId === id ? t('models.removeConfirmArmed') : undefined}
                    onClick={() => handleRemoveModel(id)}
                    onBlur={() => setArmRemoveId((prev) => (prev === id ? null : prev))}
                  >{armRemoveId === id ? t('models.removeConfirmArmed') : t('models.remove')}</button>
                </div>
              </div>
              {open && (
                <ProviderEditorCard
                  entry={entry}
                  preset={presets.find((p) => p.id === id)}
                  onClose={(changed, cardName) => closeCard(changed, cardName || name)}
                />
              )}
            </div>
          );
        })}
      </div>

      {rowIds.length === 0 && (
        <p style={{ color: 'var(--text-muted)', textAlign: 'center', marginTop: 32, fontSize: 13 }}>
          {t('models.noModels')}
        </p>
      )}

      <div style={{ marginTop: 14 }}>
        {adding ? (
          <div style={{
            background: 'var(--surface)', borderRadius: 10, padding: '12px 14px',
            display: 'flex', flexDirection: 'column', gap: 12, border: '1px solid var(--border)',
          }}>
            <div>
              <label style={fieldLabelStyle}>{t('models.provider')}</label>
              <select value={addId} aria-label={t('models.provider')}
                onChange={(e) => {
                  const row = addable.find((a) => a.provider === e.target.value);
                  if (row) setAddId(row.provider);
                }}
                style={{ ...inputStyle, maxWidth: 320 }}>
                {addable.map((a) => <option key={a.provider} value={a.provider}>{a.displayName}</option>)}
              </select>
            </div>
            {addId && (
              <ProviderEditorCard
                key={addId}
                entry={models.find((m) => m.id === addId)}
                preset={presets.find((p) => p.id === addId)}
                hideTitle
                onClose={(changed, cardName) => closeCard(changed, cardName || addId)}
              />
            )}
          </div>
        ) : declaring ? (
          <CustomProviderForm
            taken={[...new Set([...rowIds, ...directory.map((p) => p.provider), ...presets.map((p) => p.id)])]}
            onClose={closeCard}
          />
        ) : (
          <div style={{ display: 'flex', gap: 10, flexWrap: 'wrap' }}>
            {dashedButton(t('models.add'), openAdd, addable.length === 0)}
            {dashedButton(t('models.customAdd'), openDeclare)}
          </div>
        )}
      </div>
    </div>
  );
}

const inputStyle: React.CSSProperties = {
  width: '100%', padding: '6px 10px', border: '1px solid var(--border-strong)', borderRadius: 5,
  fontSize: 14, outline: 'none', fontFamily: 'inherit', boxSizing: 'border-box',
};
const labelStyle: React.CSSProperties = { fontSize: 12, color: 'var(--text-secondary)', display: 'block', marginBottom: 4 };

/* ─── App ─── */
/* ─── McpTab ─── */
/** A curated one-click MCP server preset. `env` entries with empty values mean the
 *  user must supply their own key (the key name is prefilled in the form). */
interface McpServerPreset {
  id: string;
  icon: string;
  region: 'domestic' | 'overseas';
  transport: string;            // 'stdio' | 'http'
  command: string;
  args: string[];
  env: Record<string, string>;  // keys with '' values = user must fill
  url?: string;                 // remote Streamable HTTP endpoint (transport 'http')
  oauth?: { authorization_url: string; token_url: string; scopes: string[] };
  name: { en: string; zh: string };
  desc: { en: string; zh: string };
  homepage?: string;
}

// Popular MCP servers — overseas (official + community, stdio) + domestic (Amap).
// Configs verified against the official servers repo + provider docs (Aug 2026).
const MCP_PRESETS: McpServerPreset[] = [
  // ── Overseas: official reference / community (stdio via npx) ──
  { id: 'filesystem', icon: '📁', region: 'overseas', transport: 'stdio', command: 'npx',
    args: ['-y', '@modelcontextprotocol/server-filesystem', '/path/to/allowed/dir'], env: {},
    name: { en: 'Filesystem', zh: '文件系统' },
    desc: { en: 'Read/write local files (set allowed dir)', zh: '读写本地文件（设置允许的目录）' },
    homepage: 'https://github.com/modelcontextprotocol/servers' },
  { id: 'fetch', icon: '🌐', region: 'overseas', transport: 'stdio', command: 'npx',
    args: ['-y', '@modelcontextprotocol/server-fetch'], env: {},
    name: { en: 'Fetch', zh: '网页抓取' },
    desc: { en: 'Fetch & convert web content for the LLM', zh: '抓取网页并转换供模型使用' } },
  { id: 'memory', icon: '🧠', region: 'overseas', transport: 'stdio', command: 'npx',
    args: ['-y', '@modelcontextprotocol/server-memory'], env: {},
    name: { en: 'Memory', zh: '记忆图谱' },
    desc: { en: 'Knowledge-graph based persistent memory', zh: '基于知识图谱的持久记忆' } },
  { id: 'sequential-thinking', icon: '🧩', region: 'overseas', transport: 'stdio', command: 'npx',
    args: ['-y', '@modelcontextprotocol/server-sequential-thinking'], env: {},
    name: { en: 'Sequential Thinking', zh: '逐步思考' },
    desc: { en: 'Dynamic, reflective problem-solving', zh: '动态反思式逐步推理' } },
  { id: 'time', icon: '🕐', region: 'overseas', transport: 'stdio', command: 'npx',
    args: ['-y', '@modelcontextprotocol/server-time'], env: {},
    name: { en: 'Time', zh: '时间时区' },
    desc: { en: 'Time & timezone conversion', zh: '时间与时区转换' } },
  { id: 'github', icon: '🐙', region: 'overseas', transport: 'stdio', command: 'npx',
    args: ['-y', '@github/mcp-server'], env: { GITHUB_PERSONAL_ACCESS_TOKEN: '' },
    name: { en: 'GitHub', zh: 'GitHub' },
    desc: { en: 'Repos, files & GitHub API (needs token)', zh: '仓库/文件/GitHub API（需 Token）' },
    homepage: 'https://github.com/settings/tokens' },
  { id: 'git', icon: '🔀', region: 'overseas', transport: 'stdio', command: 'uvx',
    args: ['mcp-server-git', '--repository', '/path/to/repo'], env: {},
    name: { en: 'Git', zh: 'Git' },
    desc: { en: 'Read/search Git repos (needs uv installed)', zh: '读取/搜索 Git 仓库（需先装 uv）' } },
  { id: 'postgres', icon: '🐘', region: 'overseas', transport: 'stdio', command: 'uvx',
    args: ['postgres-mcp', '--access-mode=restricted'], env: { DATABASE_URI: 'postgresql://user:pass@localhost:5432/dbname' },
    name: { en: 'PostgreSQL', zh: 'PostgreSQL' },
    desc: { en: 'SQL + EXPLAIN + index tuning (crystaldba, needs uv)', zh: 'SQL/EXPLAIN/索引调优（crystaldba，需先装 uv）' },
    homepage: 'https://github.com/crystaldba/postgres-mcp' },
  { id: 'sqlite', icon: '🗄️', region: 'overseas', transport: 'stdio', command: 'npx',
    args: ['-y', '@modelcontextprotocol/server-sqlite', '--db-path', '/path/to/db.sqlite'], env: {},
    name: { en: 'SQLite', zh: 'SQLite' },
    desc: { en: 'Query a SQLite database (set path)', zh: '查询 SQLite 数据库（设置路径）' } },
  { id: 'brave-search', icon: '🔍', region: 'overseas', transport: 'stdio', command: 'npx',
    args: ['-y', '@brave/brave-search-mcp-server'], env: { BRAVE_API_KEY: '' },
    name: { en: 'Brave Search', zh: 'Brave 搜索' },
    desc: { en: 'Web & local search (needs API key)', zh: '网页/本地搜索（需 API Key）' },
    homepage: 'https://brave.com/search/api/' },
  { id: 'playwright', icon: '🎭', region: 'overseas', transport: 'stdio', command: 'npx',
    args: ['-y', '@playwright/mcp@latest'], env: {},
    name: { en: 'Playwright', zh: 'Playwright' },
    desc: { en: 'Browser automation & testing', zh: '浏览器自动化与测试' },
    homepage: 'https://github.com/microsoft/playwright-mcp' },
  { id: 'chrome-devtools', icon: '🛠️', region: 'overseas', transport: 'stdio', command: 'npx',
    args: ['-y', 'chrome-devtools-mcp@latest'], env: {},
    name: { en: 'Chrome DevTools', zh: 'Chrome DevTools' },
    desc: { en: 'Drive Chrome DevTools for debugging & perf (Google official)', zh: '驱动 Chrome DevTools 调试/性能（Google 官方）' },
    homepage: 'https://github.com/ChromeDevTools/chrome-devtools-mcp' },
  { id: 'sentry', icon: '🛡️', region: 'overseas', transport: 'stdio', command: 'npx',
    args: ['-y', '@sentry/mcp-server'], env: { SENTRY_AUTH_TOKEN: '' },
    name: { en: 'Sentry', zh: 'Sentry' },
    desc: { en: 'Retrieve & analyze Sentry issues (needs token)', zh: '检索/分析 Sentry 问题（需 Token）' } },
  { id: 'gitlab', icon: '🦊', region: 'overseas', transport: 'stdio', command: 'npx',
    args: ['-y', '@modelcontextprotocol/server-gitlab'], env: { GITLAB_PERSONAL_ACCESS_TOKEN: '' },
    name: { en: 'GitLab', zh: 'GitLab' },
    desc: { en: 'Repos, MRs & issues via GitLab API (needs token)', zh: '仓库/MR/Issue（需 GitLab Token）' },
    homepage: 'https://docs.gitlab.com/user/profile/personal_access_tokens/' },
  { id: 'notion', icon: '📝', region: 'overseas', transport: 'stdio', command: 'npx',
    args: ['-y', '@notionhq/mcp-server'], env: { OPENAPI_MCP_HEADERS: '{"Authorization": "Bearer <your-token>", "Notion-Version": "2022-06-28"}' },
    name: { en: 'Notion', zh: 'Notion' },
    desc: { en: 'Notion pages, databases & blocks (needs token)', zh: 'Notion 页面/数据库/块（需 Token）' },
    homepage: 'https://www.notion.so/my-integrations' },
  // -- Domestic: Amap --
  { id: 'amap', icon: '🗺️', region: 'domestic', transport: 'stdio', command: 'npx',
    args: ['-y', '@amap/amap-maps-mcp-server'], env: { AMAP_MAPS_API_KEY: '' },
    name: { en: 'Amap Maps', zh: '高德地图' },
    desc: { en: 'Amap location/search/routing/weather (needs key)', zh: '高德地点搜索/路径规划/天气（需 Key）' },
    homepage: 'https://lbs.amap.com/api/mcp-server/gettingstarted' },
  { id: 'baidu-map', icon: '🧭', region: 'domestic', transport: 'stdio', command: 'uvx',
    args: ['mcp-server-baidu-maps'], env: { BAIDU_MAPS_API_KEY: '' },
    name: { en: 'Baidu Maps', zh: '百度地图' },
    desc: { en: 'Baidu Maps geocoding & routing (needs key + uv)', zh: '百度地图地理编码/路径规划（需 Key + 先装 uv）' },
    homepage: 'https://lbsyun.baidu.com/apiconsole/key' },
  { id: 'mcp-notify', icon: '🔔', region: 'domestic', transport: 'stdio', command: 'uvx',
    args: ['mcp-notify'], env: { FEISHU_BOT_KEY: '', DINGTALK_BOT_KEY: '', WEWORK_BOT_KEY: '' },
    name: { en: 'Notify', zh: '消息通知' },
    desc: { en: 'Push to Feishu/DingTalk/WeCom bots (needs bot key + uv)', zh: '飞书/钉钉/企业微信机器人推送（填对应 Bot Key + 先装 uv）' },
    homepage: 'https://github.com/aahl/mcp-notify' },
  { id: 'netease-mail', icon: '✉️', region: 'domestic', transport: 'stdio', command: 'npx',
    args: ['mcp-email'], env: { EMAIL_USER: '', EMAIL_PASSWORD: '', EMAIL_TYPE: '163' },
    name: { en: '163 Mail', zh: '网易邮箱' },
    desc: { en: 'NetEase mail via IMAP/SMTP (needs auth code)', zh: '网易邮箱收发邮件（需客户端授权码）' },
    homepage: 'https://github.com/TimeCyber/email-mcp' },
  { id: 'alibaba-devops', icon: '☁️', region: 'domestic', transport: 'stdio', command: 'npx',
    args: ['-y', 'alibabacloud-devops-mcp-server'], env: { YUNXIAO_ACCESS_TOKEN: '' },
    name: { en: 'Alibaba DevOps', zh: '阿里云云效' },
    desc: { en: 'Official Alibaba Cloud DevOps (Codeup/Flow/Projex…, needs token)', zh: '阿里云云效官方 MCP（Codeup/流水线/项目协作等，需访问令牌）' },
    homepage: 'https://github.com/aliyun/alibabacloud-devops-mcp-server' },
  // ── Remote (Streamable HTTP + OAuth) ──
  { id: 'slack', icon: '💬', region: 'overseas', transport: 'http', command: '',
    args: [], env: {}, url: 'https://mcp.slack.com/mcp',
    oauth: { authorization_url: 'https://slack.com/oauth/v2_user/authorize', token_url: 'https://slack.com/api/oauth.v2.user.access', scopes: ['chat:write', 'search:read.public', 'channels:history', 'users:read'] },
    name: { en: 'Slack', zh: 'Slack' },
    desc: { en: 'Remote Slack MCP (OAuth — register your own app)', zh: '远程 Slack MCP（OAuth — 需自建应用）' },
    homepage: 'https://mcp.slack.com' },
  { id: 'gmail', icon: '📧', region: 'overseas', transport: 'http', command: '',
    args: [], env: {}, url: 'https://gmailmcp.googleapis.com/mcp/v1',
    oauth: { authorization_url: 'https://accounts.google.com/o/oauth2/v2/auth', token_url: 'https://oauth2.googleapis.com/token', scopes: ['https://www.googleapis.com/auth/gmail.readonly', 'https://www.googleapis.com/auth/gmail.compose', 'https://www.googleapis.com/auth/gmail.modify'] },
    name: { en: 'Gmail', zh: 'Gmail' },
    desc: { en: 'Official Gmail MCP (OAuth — Google Cloud project)', zh: 'Gmail 官方 MCP（OAuth — 需 Google Cloud 项目）' },
    homepage: 'https://developers.google.com/workspace/gmail/api/reference/mcp' },
];

/** True when a server instance already exists for a template preset — matched by its
 *  localized name (either language) or stable id. Drives the template-card `+` visibility:
 *  hidden once created, restored when the instance is deleted. Pure — unit-testable. */
export function isPresetCreated(p: McpServerPreset, servers: McpServerInfo[]): boolean {
  return servers.some(s => s.name === p.name.en || s.name === p.name.zh || s.name === p.id);
}

export function McpTab() {
  const { t, lang } = useI18n();
  const [servers, setServers] = useState<McpServerInfo[]>([]);
  const [subTab, setSubTab] = useState<'templates' | 'servers'>('templates');
  const renderPresetCard = (p: McpServerPreset) => {
    const created = isPresetCreated(p, servers);
    return (
      <div key={p.id} onClick={() => applyPreset(p)} title={p.desc[lang]} style={{ background: 'var(--card-bg, var(--bg-soft))', borderRadius: 8, padding: '9px 11px', cursor: 'pointer', display: 'flex', gap: 8, alignItems: 'flex-start', border: '1px solid transparent', transition: 'border-color 0.15s' }} onMouseEnter={e => { e.currentTarget.style.borderColor = 'var(--accent)'; }} onMouseLeave={e => { e.currentTarget.style.borderColor = 'transparent'; }}>
        <div style={{ fontSize: 18, lineHeight: '22px' }}>{p.icon}</div>
        <div style={{ minWidth: 0, flex: 1 }}>
          <div style={{ display: 'flex', alignItems: 'center', gap: 6 }}>
            <span style={{ fontSize: 14, fontWeight: 600, color: 'var(--text)' }}>{p.name[lang]}</span>
            {Object.keys(p.env).length > 0 && <span title={t('mcp.needsKey')} style={{ fontSize: 11 }}>🔑</span>}
          </div>
          <div style={{ fontSize: 12, color: 'var(--text-muted)', marginTop: 2 }}>{p.desc[lang]}</div>
        </div>
        <div style={{ display: 'flex', gap: 4, alignItems: 'center', flexShrink: 0 }}>
          {p.homepage && <a href={p.homepage} target="_blank" rel="noreferrer" onClick={e => e.stopPropagation()} style={{ fontSize: 12, color: 'var(--accent)' }}>🔗</a>}
          {created
            ? <span title={t('mcp.created')} style={{ color: 'var(--success)', fontSize: 14, fontWeight: 700 }}>✓</span>
            : <button className="btn btn-secondary btn-small" aria-label={`create-${p.id}`} title={t('mcp.createFromTemplate')} onClick={e => { e.stopPropagation(); applyPreset(p); }} style={{ fontSize: 15, fontWeight: 700, lineHeight: 1, padding: '2px 8px', borderRadius: 4, color: 'var(--accent)', cursor: 'pointer' }}>+</button>}
        </div>
      </div>
    );
  };
  const [showForm, setShowForm] = useState(false);
  const [editing, setEditing] = useState<McpServerInfo | null>(null);
  const [name, setName] = useState('');
  const [transport, setTransport] = useState('stdio');
  const [command, setCommand] = useState('');
  const [argsText, setArgsText] = useState('');
  const [envText, setEnvText] = useState('');
  const [url, setUrl] = useState('');
  const [useOAuth, setUseOAuth] = useState(false);
  const [oauth, setOAuth] = useState<McpOAuthForm | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [testTarget, setTestTarget] = useState<McpServerInfo | null>(null);
  const refresh = useCallback(async () => {
    try { setServers(await (window as any).starriver.listMcpServers()); } catch (e: any) { setError(e.message); }
  }, []);
  useEffect(() => { refresh(); }, [refresh]);
  const resetForm = () => { setName(''); setTransport('stdio'); setCommand(''); setArgsText(''); setEnvText(''); setUrl(''); setUseOAuth(false); setOAuth(null); setError(null); setEditing(null); };
  const openAdd = () => { resetForm(); setShowForm(true); };
  const openEdit = (s: McpServerInfo) => {
    setName(s.name); setTransport(s.transport); setCommand(s.command);
    setArgsText(s.args.join('\n')); setEnvText(s.env.join('\n')); setUrl(s.url || '');
    setUseOAuth(!!s.oauth);
    setOAuth(s.oauth ? { authorization_url: s.oauth.authorization_url, token_url: s.oauth.token_url, client_id: s.oauth.client_id, client_secret: '', scopes: s.oauth.scopes.join(', '), redirect_uri: s.oauth.redirect_uri } : null);
    setEditing(s); setShowForm(true);
  };
  // Prefill the form from a popular-server preset so the user can fill API keys / paths before saving.
  const applyPreset = (p: McpServerPreset) => {
    resetForm();
    setName(p.name[lang]);
    setTransport(p.transport);
    setCommand(p.command);
    setArgsText(p.args.join('\n'));
    setEnvText(Object.entries(p.env).map(([k, v]) => `${k}=${v}`).join('\n'));
    setUrl(p.url || '');
    setUseOAuth(!!p.oauth);
    setOAuth(p.oauth ? { authorization_url: p.oauth.authorization_url, token_url: p.oauth.token_url, client_id: '', client_secret: '', scopes: p.oauth.scopes.join(', '), redirect_uri: 'http://localhost:3118/callback' } : null);
    setShowForm(true);
  };
  const handleSave = async () => {
    if (!name.trim()) { setError('Name is required.'); return; }
    if (transport === 'http') {
      if (!url.trim()) { setError('Endpoint URL is required for HTTP.'); return; }
    } else if (!command.trim()) {
      setError('Command is required.'); return;
    }
    setBusy(true); setError(null);
    try {
      const args = argsText.split('\n').filter(a => a.trim());
      const envLines = envText.split('\n').filter(e => e.trim());
      const env: Record<string, string> = {};
      for (const line of envLines) { const eq = line.indexOf('='); if (eq > 0) env[line.slice(0, eq).trim()] = line.slice(eq + 1).trim(); }
      const oauthJson = (transport === 'http' && useOAuth && oauth)
        ? JSON.stringify({
            authorization_url: oauth.authorization_url.trim(),
            token_url: oauth.token_url.trim(),
            client_id: oauth.client_id.trim(),
            client_secret: oauth.client_secret.trim(),
            scopes: oauth.scopes.split(',').map(s => s.trim()).filter(Boolean),
            redirect_uri: oauth.redirect_uri.trim() || 'http://localhost:3118/callback',
          })
        : null;
      await (window as any).starriver.addMcpServer(name.trim(), transport, command.trim(), JSON.stringify(args), JSON.stringify(env), url.trim() || null, oauthJson);
      setShowForm(false); resetForm(); refresh();
    } catch (e: any) { setError(e.message); }
    setBusy(false);
  };
  const handleRemove = async (srvName: string) => {
    if (!confirm(t('mcp.removeConfirm') || 'Remove this MCP server?')) return;
    try { await (window as any).starriver.removeMcpServer(srvName); refresh(); } catch (e: any) { setError(e.message); }
  };
  const handleRefresh = async (srvName?: string) => {
    try { await (window as any).starriver.refreshMcpTools(srvName ?? null); refresh(); } catch (e: any) { setError(e.message); }
  };
  const handleOAuthConnect = async (srvName: string, connected: boolean) => {
    try {
      if (connected) {
        await (window as any).starriver.oauthDisconnect(srvName);
        refresh();
      } else {
        // Backend opens the provider's authorize URL in the browser; poll until the
        // token lands in the keychain and oauthConnected flips true.
        await (window as any).starriver.oauthConnect(srvName);
        for (let i = 0; i < 30; i++) {
          await new Promise(r => setTimeout(r, 1000));
          const servers = await (window as any).starriver.listMcpServers();
          if (servers.find((x: McpServerInfo) => x.name === srvName)?.oauthConnected) break;
        }
        refresh();
        await (window as any).starriver.refreshMcpTools(srvName);
        refresh();
      }
    } catch (e: any) { setError(e.message); }
  };
  return (
    <div style={{ padding: '16px 24px', height: '100%', overflowY: 'auto' }}>
      <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginBottom: 12 }}>
        <h3 style={{ margin: 0, fontSize: 16 }}>{t('mcp.title')}</h3>
        <div style={{ display: 'flex', gap: 6 }}>
          <button className="btn btn-secondary btn-small" onClick={() => handleRefresh()}>{t('mcp.refreshAll')}</button>
          <button className="btn btn-primary btn-small" onClick={openAdd}>{t('mcp.addServer')}</button>
        </div>
      </div>
      {/* Sub-tab segmented control */}
      <div style={{ display: 'flex', gap: 4, background: 'var(--bg-soft)', borderRadius: 8, padding: 3, marginBottom: 14 }}>
        {(['templates', 'servers'] as const).map(st => (
          <button key={st} type="button" onClick={() => setSubTab(st)} style={{ flex: 1, padding: '6px 0', border: 'none', borderRadius: 6, fontSize: 13, cursor: 'pointer', fontWeight: 600, background: subTab === st ? 'var(--surface)' : 'transparent', color: subTab === st ? 'var(--accent)' : 'var(--text-secondary)', boxShadow: subTab === st ? 'var(--shadow-sm)' : 'none' }}>{t(`mcp.tab_${st}`)}</button>
        ))}
      </div>
      {error && <div style={{ color: 'var(--danger)', marginBottom: 8, fontSize: 13 }}>{error}</div>}
      {/* Templates tab — the popular-server gallery; `+` creates an instance from a preset */}
      {subTab === 'templates' && (
        <div style={{ marginBottom: 16 }}>
          <div style={{ fontSize: 12, color: 'var(--text-muted)', textTransform: 'uppercase', letterSpacing: '0.6px', marginBottom: 8 }}>{t('mcp.popular')}</div>
          <div style={{ display: 'grid', gridTemplateColumns: 'repeat(3, 1fr)', gap: 8 }}>
            {MCP_PRESETS.filter(p => p.region === 'overseas').map(renderPresetCard)}
          </div>
          <div style={{ borderTop: '1px solid var(--border)', margin: '16px 0' }} />
          <div style={{ display: 'grid', gridTemplateColumns: 'repeat(3, 1fr)', gap: 8 }}>
            {MCP_PRESETS.filter(p => p.region === 'domestic').map(renderPresetCard)}
          </div>
        </div>
      )}
      {/* Servers tab — created instances */}
      {subTab === 'servers' && (
        <div style={{ display: 'grid', gap: 8 }}>
          {servers.length === 0 && <div style={{ color: 'var(--text-muted)', fontSize: 14 }}>{t('mcp.noServers')}</div>}
          {servers.map(s => (
            <div key={s.name} style={{ background: 'var(--card-bg, var(--bg-soft))', borderRadius: 8, padding: '10px 14px' }}>
              <div style={{ display: 'flex', alignItems: 'center', gap: 10 }}>
                <span style={{ fontSize: 13, background: s.transport === 'stdio' ? 'var(--success)' : 'var(--accent)', color: 'var(--surface)', borderRadius: 4, padding: '1px 6px' }}>{s.transport}</span>
                <strong style={{ flex: 1, fontSize: 14 }}>{s.name}</strong>
                <span style={{ color: 'var(--text-muted)', fontSize: 13, maxWidth: 200, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>{s.transport === 'http' ? (s.url || '') : s.command}</span>
                {/* OAuth button only makes sense for servers that actually have an
                    OAuth config — plain HTTP servers connect automatically (the
                    tool count proves it), and clicking here would just error with
                    "has no OAuth config" (admin4.oauth_connect). */}
                {s.transport === 'http' && s.oauth && (
                  <button className="btn btn-secondary btn-small" onClick={() => handleOAuthConnect(s.name, !!s.oauthConnected)} style={{ fontSize: 11 }}>{s.oauthConnected ? t('mcp.oauthConnected') : t('mcp.oauthConnect')}</button>
                )}
                <span style={{ color: s.toolCount > 0 ? 'var(--success)' : 'var(--text-muted)', fontSize: 12 }}>{t('mcp.toolCount', { count: String(s.toolCount) })}</span>
                <div style={{ display: 'flex', gap: 4 }}>
                  <button className="btn btn-secondary btn-small" onClick={() => setTestTarget(s)} style={{ fontSize: 11 }}>{t('mcp.test')}</button>
                  <button className="btn btn-secondary btn-small" onClick={() => openEdit(s)} style={{ fontSize: 11 }}>{t('common.edit')}</button>
                  <button className="btn btn-secondary btn-small" onClick={() => handleRefresh(s.name)} style={{ fontSize: 11 }} title={t('mcp.discoverTools')}>🔍</button>
                  <button className="btn btn-secondary btn-small" onClick={() => handleRemove(s.name)} style={{ fontSize: 12, color: 'var(--danger)' }}>{t('common.remove')}</button>
                </div>
              </div>
            </div>
          ))}
        </div>
      )}
      {showForm && (
        <div style={{ position: 'fixed', inset: 0, background: 'var(--overlay-strong)', display: 'flex', alignItems: 'center', justifyContent: 'center', zIndex: 100 }}>
          <div style={{ background: 'var(--surface)', borderRadius: 12, padding: 24, width: 440, maxHeight: '80vh', overflowY: 'auto', boxShadow: 'var(--shadow-md)' }}>
            <h4 style={{ margin: '0 0 16px', fontSize: 15 }}>{editing ? t('mcp.editServer') : t('mcp.addServer')}</h4>
            {error && <div style={{ color: 'var(--danger)', marginBottom: 8, fontSize: 13 }}>{error}</div>}
            <div style={{ display: 'flex', flexDirection: 'column', gap: 10 }}>
              <label style={{ fontSize: 13, color: 'var(--text-secondary)' }}>{t('mcp.name')}<input className="toolbar-input" value={name} onChange={e => setName(e.target.value)} placeholder="github" style={{ width: '100%', marginTop: 2 }} /></label>
              <label style={{ fontSize: 13, color: 'var(--text-secondary)' }}>{t('mcp.transport')}<select className="toolbar-select" value={transport} onChange={e => setTransport(e.target.value)} style={{ width: '100%', marginTop: 2 }}><option value="stdio">Stdio</option><option value="sse">SSE</option><option value="http">HTTP</option></select></label>
              {transport === 'http' && (
                <>
                  <label style={{ fontSize: 13, color: 'var(--text-secondary)' }}>{t('mcp.url')}<input className="toolbar-input" value={url} onChange={e => setUrl(e.target.value)} placeholder="https://mcp.slack.com/mcp" style={{ width: '100%', marginTop: 2 }} /></label>
                  <label style={{ fontSize: 13, color: 'var(--text-secondary)', display: 'flex', alignItems: 'center', gap: 6 }}>
                    <input type="checkbox" checked={useOAuth} onChange={e => { setUseOAuth(e.target.checked); if (e.target.checked && !oauth) setOAuth({ authorization_url: '', token_url: '', client_id: '', client_secret: '', scopes: '', redirect_uri: 'http://localhost:3118/callback' }); }} />
                    {t('mcp.useOAuth')}
                  </label>
                </>
              )}
              {transport !== 'http' && <label style={{ fontSize: 13, color: 'var(--text-secondary)' }}>{t('mcp.command')}<input className="toolbar-input" value={command} onChange={e => setCommand(e.target.value)} placeholder="npx" style={{ width: '100%', marginTop: 2 }} /></label>}
              {transport === 'http' && useOAuth && oauth && (
                <>
                  <label style={{ fontSize: 13, color: 'var(--text-secondary)' }}>{t('mcp.authUrl')}<input className="toolbar-input" value={oauth.authorization_url} onChange={e => setOAuth({ ...oauth, authorization_url: e.target.value })} style={{ width: '100%', marginTop: 2 }} /></label>
                  <label style={{ fontSize: 13, color: 'var(--text-secondary)' }}>{t('mcp.tokenUrl')}<input className="toolbar-input" value={oauth.token_url} onChange={e => setOAuth({ ...oauth, token_url: e.target.value })} style={{ width: '100%', marginTop: 2 }} /></label>
                  <label style={{ fontSize: 13, color: 'var(--text-secondary)' }}>{t('mcp.clientId')}<input className="toolbar-input" value={oauth.client_id} onChange={e => setOAuth({ ...oauth, client_id: e.target.value })} style={{ width: '100%', marginTop: 2 }} /></label>
                  <label style={{ fontSize: 13, color: 'var(--text-secondary)' }}>{t('mcp.clientSecret')}<input className="toolbar-input" value={oauth.client_secret} onChange={e => setOAuth({ ...oauth, client_secret: e.target.value })} placeholder={editing?.oauth?.has_secret ? t('mcp.secretSaved') : ''} style={{ width: '100%', marginTop: 2 }} /></label>
                  <label style={{ fontSize: 13, color: 'var(--text-secondary)' }}>{t('mcp.scopes')}<input className="toolbar-input" value={oauth.scopes} onChange={e => setOAuth({ ...oauth, scopes: e.target.value })} style={{ width: '100%', marginTop: 2 }} /></label>
                  <label style={{ fontSize: 13, color: 'var(--text-secondary)' }}>{t('mcp.redirectUri')}<input className="toolbar-input" value={oauth.redirect_uri} onChange={e => setOAuth({ ...oauth, redirect_uri: e.target.value })} style={{ width: '100%', marginTop: 2 }} /></label>
                </>
              )}
              <label style={{ fontSize: 13, color: 'var(--text-secondary)' }}>{t('mcp.args')}<textarea className="toolbar-input" value={argsText} onChange={e => setArgsText(e.target.value)} placeholder={t('mcp.argsHint')} rows={3} style={{ width: '100%', marginTop: 2, resize: 'vertical' }} /></label>
              <label style={{ fontSize: 13, color: 'var(--text-secondary)' }}>{t('mcp.env')}<textarea className="toolbar-input" value={envText} onChange={e => setEnvText(e.target.value)} placeholder={t('mcp.envHint')} rows={3} style={{ width: '100%', marginTop: 2, resize: 'vertical' }} /></label>
            </div>
            <div style={{ marginTop: 16, display: 'flex', justifyContent: 'flex-end', gap: 8 }}>
              <button className="btn btn-secondary btn-small" onClick={() => { setShowForm(false); resetForm(); }}>{t('mcp.cancel')}</button>
              <button className="btn btn-primary btn-small" onClick={handleSave} disabled={busy}>{busy ? '...' : t('mcp.save')}</button>
            </div>
          </div>
        </div>
      )}
      {testTarget && <McpTestWindow server={testTarget} onClose={() => setTestTarget(null)} />}
    </div>
  );
}

/* ─── McpTestWindow ─── */
/** A dedicated test panel for a created MCP instance: test the connection (initialize →
 *  tools/list) and trial-run any discovered tool (tools/call with sample arguments). */
function McpTestWindow({ server, onClose }: { server: McpServerInfo; onClose: () => void }) {
  const { t } = useI18n();
  const [result, setResult] = useState<McpTestResult | 'testing' | null>(null);
  const [toolTests, setToolTests] = useState<Record<string, McpToolTestResult | 'testing'>>({});
  const runTest = async () => {
    setResult('testing');
    try {
      setResult(await (window as any).starriver.testMcpServer(server.name));
    } catch (e: any) {
      setResult({ ok: false, toolCount: 0, tools: [], error: e.message });
    }
  };
  const runTool = async (tool: string) => {
    const key = `${server.name}::${tool}`;
    setToolTests(prev => ({ ...prev, [key]: 'testing' }));
    try {
      const r = await (window as any).starriver.testMcpTool(server.name, tool);
      setToolTests(prev => ({ ...prev, [key]: r }));
    } catch (e: any) {
      setToolTests(prev => ({ ...prev, [key]: { ok: false, error: e.message } }));
    }
  };
  const ok = result !== null && result !== 'testing' && result.ok;
  return (
    <div data-testid="mcp-test-window" style={{ position: 'fixed', inset: 0, background: 'var(--overlay-strong)', display: 'flex', alignItems: 'center', justifyContent: 'center', zIndex: 110 }}>
      <div style={{ background: 'var(--surface)', borderRadius: 12, padding: 24, width: 560, maxHeight: '80vh', overflowY: 'auto', boxShadow: 'var(--shadow-md)' }}>
        <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginBottom: 16 }}>
          <h4 style={{ margin: 0, fontSize: 15 }}>
            <span style={{ fontSize: 13, background: server.transport === 'stdio' ? 'var(--success)' : 'var(--accent)', color: 'var(--surface)', borderRadius: 4, padding: '1px 6px', marginRight: 8 }}>{server.transport}</span>
            {server.name}
          </h4>
          <button className="btn btn-secondary btn-small" onClick={onClose}>{t('mcp.cancel')}</button>
        </div>
        <div style={{ marginBottom: 14 }}>
          <div style={{ fontSize: 13, fontWeight: 600, color: 'var(--text)', marginBottom: 6 }}>{t('mcp.testConnection')}</div>
          <button className="btn btn-primary btn-small" onClick={runTest} disabled={result === 'testing'}>{result === 'testing' ? '…' : t('mcp.test')}</button>
          {result !== null && result !== 'testing' && (
            <div style={{ marginTop: 8, fontSize: 13, color: result.ok ? 'var(--success)' : 'var(--danger)' }}>
              {result.ok
                ? `✓ ${t('mcp.toolCount', { count: String(result.toolCount) })}`
                : `✗ ${result.error || 'failed'}`}
            </div>
          )}
        </div>
        {ok && result.tools.length > 0 && (
          <div>
            <div style={{ fontSize: 13, fontWeight: 600, color: 'var(--text)', marginBottom: 6 }}>{t('mcp.usage')}</div>
            <div style={{ padding: '6px 10px', background: 'var(--panel-bg)', borderRadius: 6 }}>
              {result.tools.map(tool => {
                const key = `${server.name}::${tool}`;
                const tr = toolTests[key];
                return (
                  <div key={tool} style={{ borderBottom: '1px solid var(--border)', padding: '4px 0', fontSize: 12 }}>
                    <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
                      <span style={{ color: 'var(--text-secondary)', flex: 1 }}>{tool}</span>
                      <button className="btn btn-secondary btn-small" onClick={() => runTool(tool)} style={{ fontSize: 10 }} disabled={tr === 'testing'}>{tr === 'testing' ? '…' : t('mcp.runTool')}</button>
                    </div>
                    {tr && tr !== 'testing' && (
                      <div style={{ marginTop: 4, whiteSpace: 'pre-wrap', wordBreak: 'break-word', color: (tr as McpToolTestResult).ok ? 'var(--text)' : 'var(--danger)', maxHeight: 160, overflowY: 'auto' }}>
                        {(tr as McpToolTestResult).ok
                          ? (tr as McpToolTestResult).output
                          : `✗ ${(tr as McpToolTestResult).error || 'failed'}`}
                      </div>
                    )}
                  </div>
                );
              })}
            </div>
          </div>
        )}
      </div>
    </div>
  );
}

/* ─── ScheduledTasksTab ─── */
/** The slash token that represents a skill in prompt text: its command if it has one, else its name. */
function skillTokenOf(s: SkillInfo): string {
  return s.has_command && s.command ? s.command : s.name;
}

/**
 * Parse `/skill-token` tokens out of a prompt and resolve each to its skill NAME
 * (matching by command first, then by name). Unknown tokens are ignored; the
 * result is deduped, order-preserving. Pure — unit-testable without the DOM.
 * This is what gets stored on the task and injected at run time.
 */
export function parseSkillTokens(prompt: string, skills: SkillInfo[]): string[] {
  const byCommand = new Map<string, string>();
  const byName = new Map<string, string>();
  for (const s of skills) {
    byName.set(s.name, s.name);
    if (s.has_command && s.command) byCommand.set(s.command, s.name);
  }
  const seen = new Set<string>();
  const out: string[] = [];
  const re = /\/([\w-]+)/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(prompt)) !== null) {
    const tok = m[1];
    const name = byCommand.get(tok) || byName.get(tok);
    if (name && !seen.has(name)) { seen.add(name); out.push(name); }
  }
  return out;
}

/** A one-click automation task template. Localized name/desc/prompt by `lang`. */
interface TaskTemplate {
  id: string;
  icon: string;
  modeType: 'periodic' | 'interval';
  period?: string;       // periodic
  time?: string;         // periodic "HH:MM"
  dayOfWeek?: number;    // periodic weekly (0=Sun..6=Sat)
  hours?: number;        // interval
  daysOfWeek?: number[]; // interval; [] or all-7 = every day
  name: { en: string; zh: string };
  desc: { en: string; zh: string };
  prompt: { en: string; zh: string };
}

// 15 automation templates across topics people commonly care about.
const TASK_TEMPLATES: TaskTemplate[] = [
  {
    id: 'weather', icon: '🌤️', modeType: 'interval', hours: 1, daysOfWeek: [0,1,2,3,4,5,6],
    name: { en: 'Hourly Weather', zh: '每小时天气' },
    desc: { en: 'Next 24h forecast, every hour', zh: '每小时查询未来24小时天气' },
    prompt: { en: 'Look up the weather forecast for the next 24 hours and summarize today\'s key conditions (temperature, precipitation, severe-weather alerts).', zh: '查询未来24小时的天气预报，总结今日要点（气温、降水、预警信息）。' },
  },
  {
    id: 'finance', icon: '📈', modeType: 'interval', hours: 1, daysOfWeek: [0,1,2,3,4,5,6],
    name: { en: 'Market Pulse', zh: '金融速报' },
    desc: { en: 'Latest finance news, every hour', zh: '每小时推送最新金融信息' },
    prompt: { en: 'Summarize the latest financial-market moves and important economic news from the past hour.', zh: '汇总过去一小时金融市场动态与重要财经新闻。' },
  },
  {
    id: 'crypto', icon: '🪙', modeType: 'interval', hours: 1, daysOfWeek: [0,1,2,3,4,5,6],
    name: { en: 'Crypto Prices', zh: '加密货币行情' },
    desc: { en: 'Major coin prices, every hour', zh: '每小时查询主要币种行情' },
    prompt: { en: 'Report the latest prices and 24h change of BTC, ETH and other major cryptocurrencies.', zh: '查询 BTC、ETH 等主要加密货币的最新价格与24小时涨跌幅。' },
  },
  {
    id: 'tech-news', icon: '💻', modeType: 'periodic', period: 'daily', time: '08:30',
    name: { en: 'Daily Tech News', zh: '每日科技要闻' },
    desc: { en: 'Top tech headlines each morning', zh: '每天汇总科技行业新闻' },
    prompt: { en: 'Gather today\'s most important technology-industry news and list the top 5 with one-line summaries.', zh: '汇总今日科技行业重要新闻，列出前5条并各用一句话总结。' },
  },
  {
    id: 'github', icon: '🐙', modeType: 'periodic', period: 'daily', time: '09:00',
    name: { en: 'GitHub Trending', zh: 'GitHub 趋势' },
    desc: { en: 'Trending repos each morning', zh: '每天查看热门开源项目' },
    prompt: { en: 'Check today\'s GitHub trending repositories and highlight the 3 most interesting ones, explaining what each does.', zh: '查看今日 GitHub 趋势项目，挑出最值得关注的3个并说明用途。' },
  },
  {
    id: 'competitor', icon: '🔍', modeType: 'periodic', period: 'daily', time: '10:00',
    name: { en: 'Competitor Watch', zh: '竞品监控' },
    desc: { en: 'Daily competitor updates', zh: '每天检查竞品动态' },
    prompt: { en: 'Check competitor websites and products for new updates or releases today and summarize the changes.', zh: '检查竞品官网/产品今日是否有更新或新版本发布，汇总变化。' },
  },
  {
    id: 'exchange', icon: '💱', modeType: 'periodic', period: 'daily', time: '09:30',
    name: { en: 'Exchange Rates', zh: '汇率速查' },
    desc: { en: 'Major currency rates daily', zh: '每天查询主要货币汇率' },
    prompt: { en: 'Look up today\'s exchange rates for USD, EUR, CNY and JPY and flag any significant moves.', zh: '查询今日美元、欧元、人民币、日元等主要货币汇率，标注明显波动。' },
  },
  {
    id: 'sports', icon: '⚽', modeType: 'periodic', period: 'daily', time: '20:00',
    name: { en: 'Daily Sports', zh: '每日体育' },
    desc: { en: 'Results & fixtures daily', zh: '汇总赛果与赛程' },
    prompt: { en: 'Summarize today\'s major sports results and list tomorrow\'s key fixtures.', zh: '汇总今日重要体育赛事结果与明日关键赛程。' },
  },
  {
    id: 'social', icon: '📱', modeType: 'interval', hours: 1, daysOfWeek: [0,1,2,3,4,5,6],
    name: { en: 'Social Trends', zh: '社媒热点' },
    desc: { en: 'Trending topics, every hour', zh: '每小时汇总社交平台热点' },
    prompt: { en: 'Summarize the current trending topics across the major social platforms.', zh: '汇总主要社交平台当前的热点话题。' },
  },
  {
    id: 'jobs', icon: '💼', modeType: 'periodic', period: 'daily', time: '09:00',
    name: { en: 'Job Listings', zh: '招聘信息' },
    desc: { en: 'New relevant jobs daily', zh: '每天查找相关职位' },
    prompt: { en: 'Find new job postings matching my role and list the top 5 with company and location.', zh: '查找符合我岗位方向的新招聘信息，列出前5条（公司、地点）。' },
  },
  {
    id: 'research', icon: '📚', modeType: 'periodic', period: 'daily', time: '08:00',
    name: { en: 'Research Papers', zh: '论文速递' },
    desc: { en: 'Latest papers in my field daily', zh: '每天追踪领域最新论文' },
    prompt: { en: 'Find the latest research papers in my field and summarize 3 noteworthy abstracts.', zh: '查找我所在领域的最新论文，总结3篇值得关注的摘要。' },
  },
  {
    id: 'digest', icon: '📝', modeType: 'periodic', period: 'daily', time: '18:00',
    name: { en: 'Daily Digest', zh: '每日总结' },
    desc: { en: 'End-of-day work summary', zh: '生成今日工作日报' },
    prompt: { en: 'Generate a concise end-of-day summary of today\'s work, including completed items and outstanding to-dos.', zh: '生成今日工作总结日报，列出完成事项与待办。' },
  },
  {
    id: 'security', icon: '🛡️', modeType: 'periodic', period: 'weekly', time: '09:00', dayOfWeek: 1,
    name: { en: 'Code Security Scan', zh: '代码安全扫描' },
    desc: { en: 'Weekly vulnerability scan', zh: '每周扫描项目安全漏洞' },
    prompt: { en: 'Scan the project and its dependencies for known security vulnerabilities and report the risks.', zh: '扫描项目依赖与代码的已知安全漏洞，报告风险项。' },
  },
  {
    id: 'health', icon: '🌐', modeType: 'interval', hours: 1, daysOfWeek: [0,1,2,3,4,5,6],
    name: { en: 'Website Health', zh: '网站健康检查' },
    desc: { en: 'Uptime check, every hour', zh: '每小时检查关键网站' },
    prompt: { en: 'Check that the key websites are reachable and report response times or any failures.', zh: '检查关键网站是否可访问，报告响应时间或异常。' },
  },
  {
    id: 'watchlist', icon: '🎬', modeType: 'periodic', period: 'weekly', time: '18:00', dayOfWeek: 5,
    name: { en: 'Weekly Watchlist', zh: '每周观影' },
    desc: { en: 'New movies & shows weekly', zh: '每周推荐新片新剧' },
    prompt: { en: 'Recommend the new movies and shows worth watching this week.', zh: '推荐本周值得观看的新电影与新剧集。' },
  },
];

/* ─── Run trace viewer (History → click a run) ─── */

function TraceEntryView({ e }: { e: SessionTraceEntry }) {
  if (e.kind === 'tool_use') {
    return (
      <div style={{ margin: '6px 0', border: '1px solid var(--accent-border)', borderRadius: 8, overflow: 'hidden' }}>
        <div style={{ background: 'var(--panel-bg)', padding: '5px 10px', fontSize: 13, color: 'var(--info)', fontWeight: 600 }}>🔧 {e.toolName}</div>
        {e.toolInput && (
          <details style={{ fontSize: 12 }}>
            <summary style={{ padding: '4px 10px', color: 'var(--text-muted)', cursor: 'pointer' }}>input</summary>
            <pre style={{ margin: 0, padding: '8px 10px', background: 'var(--panel-bg)', fontSize: 12, whiteSpace: 'pre-wrap', wordBreak: 'break-word' }}>{e.toolInput}</pre>
          </details>
        )}
      </div>
    );
  }
  if (e.kind === 'tool_result') {
    return (
      <div style={{ margin: '4px 0 8px 0', border: '1px solid var(--accent-border)', borderRadius: 8, overflow: 'hidden' }}>
        <div style={{ padding: '5px 10px', fontSize: 13, fontWeight: 600, color: e.toolOk ? 'var(--success)' : 'var(--danger)' }}>{e.toolOk ? '✓ result' : '✗ error'}</div>
        {e.toolOutput && (
          <details open style={{ fontSize: 12 }}>
            <summary style={{ padding: '4px 10px', color: 'var(--text-muted)', cursor: 'pointer' }}>output</summary>
            <div style={{ padding: '4px 10px' }}><MarkdownContent source={e.toolOutput} /></div>
          </details>
        )}
      </div>
    );
  }
  if (e.kind === 'thinking') {
    return (
      <details style={{ margin: '6px 0', fontSize: 13, color: 'var(--text-muted)' }}>
        <summary style={{ cursor: 'pointer', fontStyle: 'italic' }}>💭 thinking</summary>
        <div style={{ padding: '6px 10px', whiteSpace: 'pre-wrap', fontStyle: 'italic', color: 'var(--text-secondary)' }}>{e.text}</div>
      </details>
    );
  }
  // user / assistant
  const isAssistant = e.kind === 'assistant';
  return (
    <div className={`message ${isAssistant ? 'assistant' : 'user'}`} style={{ margin: '8px 0' }}>
      <div className="bubble">
        {isAssistant ? <MarkdownContent source={e.text} /> : <span style={{ whiteSpace: 'pre-wrap' }}>{e.text}</span>}
      </div>
    </div>
  );
}

/* ─── History tab: all runs across tasks ─── */

/* ─── Memory panel (WI-52 Phase 1: read-only management) ─── */

/** Mirror of the host's `project_id_from_cwd` (starriver-memory ingest): the
 *  store keys projects by the workspace path with non-alphanumerics folded to
 *  '-' (Unicode letters/numbers kept); an empty slug falls back to `_global`.
 *  The panel must address the same bucket the agent writes, so the slug rule
 *  is replicated client-side (Phase 3 can replace this with memory.status). */
export function memoryProjectIdFromPath(path: string): string {
  // Iterate CODE POINTS (host uses Rust chars()): split('') yields UTF-16
  // units and astral chars produced two '-' each, addressing a bucket the
  // host never writes.
  const slug = Array.from(path.replace(/^\/+/, ''))
    .map((c) => (/[\p{L}\p{N}]/u.test(c) ? c : '-'))
    .join('')
    .replace(/^-+|-+$/g, '');
  return slug || '_global';
}

const MEMORY_GLOBAL = '_global';

/** Memory tab: browse / search / inspect / delete the agent's cross-session
 *  memories (compaction checkpoints + extracted facts). Phase 1 is read-only
 *  management over the three bridged protocol methods (memory.list/delete/
 *  clear); manual write (memory.upsert) and status/cost (memory.status) land
 *  with Phase 2/3 once the host exposes them. */
export function MemoryTab() {
  const { t } = useI18n();
  const [workspaces, setWorkspaces] = useState<WorkspaceInfo[]>([]);
  // Selected project bucket: `_global` (cross-project facts) or a workspace slug.
  const [project, setProject] = useState<string>(MEMORY_GLOBAL);
  const [entries, setEntries] = useState<MemoryEntry[]>([]);
  const [loaded, setLoaded] = useState(false);
  const [search, setSearch] = useState('');
  const [selected, setSelected] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // Two-step inline confirmations (no window.confirm): the armed entry name
  // and the armed clear-all flag each swap in a confirm/cancel pair.
  const [armDelete, setArmDelete] = useState<string | null>(null);
  const [armClear, setArmClear] = useState(false);
  // Phase 2: manual write form + recall preview.
  const [showForm, setShowForm] = useState(false);
  const [formName, setFormName] = useState('');
  const [formDesc, setFormDesc] = useState('');
  const [formBody, setFormBody] = useState('');
  const [formGlobal, setFormGlobal] = useState(false);
  const [savedName, setSavedName] = useState<string | null>(null);
  const [recallQuery, setRecallQuery] = useState('');
  const [recallResults, setRecallResults] = useState<MemoryEntry[] | null>(null);
  // Phase 3 ops: shared-store status (counts per project + token budget) and
  // one-shot ingest/export feedback.
  const [statusInfo, setStatusInfo] = useState<any>(null);
  const [ingestReport, setIngestReport] = useState<string | null>(null);

  useEffect(() => {
    window.starriver.listWorkspaces?.().then((ws) => setWorkspaces(ws ?? [])).catch(() => {});
  }, []);

  const projectOptions = useMemo(() => {
    const opts: { id: string; label: string }[] = [{ id: MEMORY_GLOBAL, label: t('memory.globalProject') }];
    const seen = new Set([MEMORY_GLOBAL]);
    for (const w of workspaces) {
      const id = memoryProjectIdFromPath(w.path);
      if (seen.has(id)) continue;
      seen.add(id);
      opts.push({ id, label: w.title || w.path });
    }
    return opts;
  }, [workspaces, t]);

  const memtabGen = useRef(0);
  const refresh = useCallback(async (pid: string) => {
    const gen = ++memtabGen.current;
    setBusy(true);
    setError(null);
    try {
      // memory.list without projectId resolves to `_global` host-side.
      const list = await window.starriver.listMemories(pid === MEMORY_GLOBAL ? undefined : pid);
      // Out-of-order responses must not repaint another project's entries
      // under the current one (delete/clear would act on the wrong bucket).
      if (gen !== memtabGen.current) return;
      setEntries(list ?? []);
      setLoaded(true);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }, []);

  useEffect(() => {
    setSelected(null);
    setArmDelete(null);
    setArmClear(false);
    void refresh(project);
    window.starriver.memoryStatus?.().then(setStatusInfo).catch(() => {});
  }, [project, refresh]);

  // Client-side filter (single project ≤200 entries): case-insensitive across
  // name/description/body, newest first.
  const filtered = useMemo(() => {
    const q = search.trim().toLowerCase();
    const sorted = [...entries].sort((a, b) => (b.updated ?? 0) - (a.updated ?? 0));
    if (!q) return sorted;
    return sorted.filter((e) =>
      (e.name || '').toLowerCase().includes(q) ||
      (e.description || '').toLowerCase().includes(q) ||
      (e.body || '').toLowerCase().includes(q));
  }, [entries, search]);

  const current = entries.find((e) => e.name === selected) ?? null;

  const sourceLabel = (s: string) =>
    s === 'compaction' ? t('memory.sourceCompaction')
      : s === 'extract' ? t('memory.sourceExtract')
        : s;
  const agentLabel = (a: string) => (a === 'manual' ? t('memory.manual') : a);

  const fmtTime = (ts: number) => {
    if (!ts) return '';
    const d = new Date(ts * 1000);
    return Number.isNaN(d.getTime()) ? '' : d.toLocaleString();
  };

  const doDelete = async () => {
    if (!current) return;
    const pid = project === MEMORY_GLOBAL ? MEMORY_GLOBAL : project;
    try {
      await window.starriver.deleteMemory(pid, current.name);
      setArmDelete(null);
      setSelected(null);
      await refresh(project);
    } catch (e) {
      setError(String(e));
    }
  };

  const doClear = async () => {
    const pid = project === MEMORY_GLOBAL ? MEMORY_GLOBAL : project;
    try {
      await window.starriver.clearMemories(pid);
      setArmClear(false);
      setSelected(null);
      await refresh(project);
    } catch (e) {
      setError(String(e));
    }
  };

  const doSave = async () => {
    if (!formName.trim() || !formDesc.trim() || !formBody.trim()) return;
    setBusy(true);
    setError(null);
    try {
      // `undefined` projectId = host `_global` when the global toggle is on.
      const pid = formGlobal ? undefined : (project === MEMORY_GLOBAL ? undefined : project);
      const entry = await window.starriver.upsertMemory(pid, formName.trim(), formDesc.trim(), formBody);
      setSavedName(entry.name);
      setFormName(''); setFormDesc(''); setFormBody(''); setFormGlobal(false);
      setShowForm(false);
      // Jump the view to where the entry actually landed (a global save from
      // a project view lands in `_global`, and the name may carry a fork suffix).
      setProject(entry.project);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const doRecall = async () => {
    if (!recallQuery.trim()) return;
    setBusy(true);
    setError(null);
    try {
      const pid = project === MEMORY_GLOBAL ? undefined : project;
      setRecallResults(await window.starriver.recallMemories(pid, recallQuery.trim()));
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const doIngest = async () => {
    setBusy(true);
    setError(null);
    try {
      const report = await window.starriver.ingestMemories();
      if (report && report.llmRouted === false) {
        setIngestReport(t('memory.ingestNoRoute'));
      } else {
        setIngestReport(t('memory.ingestDone', { n: report?.totalMemories ?? 0 }));
      }
      await refresh(project);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const doExport = async () => {
    let dir: string | null = null;
    try {
      dir = await window.starriver.selectDirectory?.();
    } catch { /* picker unavailable → fall through */ }
    if (!dir) return;
    setBusy(true);
    setError(null);
    try {
      await window.starriver.exportMemories(dir);
      setIngestReport(t('memory.exportDone', { path: dir }));
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const badgeStyle: React.CSSProperties = {
    display: 'inline-block', padding: '1px 6px', borderRadius: 4, fontSize: 12,
    background: 'var(--bg-soft)', color: 'var(--text-muted)', marginRight: 4,
  };
  const selectStyle: React.CSSProperties = {
    padding: '6px 8px', borderRadius: 6, border: '1px solid var(--border)',
    background: 'var(--surface)', color: 'var(--text)', fontSize: 13,
  };

  return (
    <div style={{ padding: '16px 24px', height: '100%', overflowY: 'auto' }}>
      <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginBottom: 12, gap: 8, flexWrap: 'wrap' }}>
        <div>
          <h3 style={{ margin: 0, fontSize: 16 }}>{t('memory.title')}</h3>
          <p style={{ margin: '4px 0 0 0', fontSize: 13, color: 'var(--text-muted)' }}>
            {t('memory.count', { n: filtered.length })}
          </p>
        </div>
        <div style={{ display: 'flex', gap: 8, alignItems: 'center', flexWrap: 'wrap' }}>
          <select aria-label={t('memory.project')} value={project} onChange={(e) => setProject(e.target.value)} style={selectStyle}>
            {projectOptions.map((o) => <option key={o.id} value={o.id}>{o.label}</option>)}
          </select>
          <input
            aria-label={t('memory.searchPlaceholder')}
            placeholder={t('memory.searchPlaceholder')}
            value={search}
            onChange={(e) => setSearch(e.target.value)}
            style={{ ...selectStyle, width: 220 }}
          />
          <button data-testid="memory-add" className="btn btn-secondary btn-small" onClick={() => { setShowForm(true); setSavedName(null); }}>
            {t('memory.add')}
          </button>
          <button data-testid="memory-ingest" className="btn btn-secondary btn-small" disabled={busy} onClick={doIngest}>
            {t('memory.ingest')}
          </button>
          <button data-testid="memory-export" className="btn btn-secondary btn-small" disabled={busy} onClick={doExport}>
            {t('memory.export')}
          </button>
          {armClear ? (
            <span style={{ display: 'inline-flex', gap: 6, alignItems: 'center', fontSize: 12 }}>
              <span style={{ color: 'var(--danger)' }}>{t('memory.clearConfirm', { n: entries.length })}</span>
              <button className="btn btn-secondary btn-small" onClick={() => setArmClear(false)}>{t('common.cancel')}</button>
              <button className="modal-btn danger" onClick={doClear}>{t('memory.clearAll')}</button>
            </span>
          ) : (
            <button className="btn btn-secondary btn-small" disabled={!loaded || entries.length === 0} onClick={() => setArmClear(true)}>
              {t('memory.clearAll')}
            </button>
          )}
        </div>
      </div>

      {error && (
        <div data-testid="memory-error" style={{ padding: '8px 12px', marginBottom: 10, background: 'var(--danger-soft)', border: '1px solid var(--danger-border)', borderRadius: 6, color: 'var(--danger)', fontSize: 12 }}>{error}</div>
      )}

      {loaded && entries.length === 0 ? (
        <div className="empty-state"><p>{t('memory.empty')}</p></div>
      ) : (
        <div style={{ display: 'flex', gap: 16, alignItems: 'flex-start' }}>
          {/* Entry list (newest first) */}
          <div style={{ flex: '1 1 55%', minWidth: 0, display: 'flex', flexDirection: 'column', gap: 6 }} data-testid="memory-list">
            {filtered.map((e) => (
              <div
                key={`${e.project}/${e.name}`}
                onClick={() => { setSelected(e.name); setArmDelete(null); }}
                style={{
                  padding: '8px 12px', borderRadius: 6, cursor: 'pointer',
                  border: `1px solid ${selected === e.name ? 'var(--accent-border)' : 'var(--border)'}`,
                  background: selected === e.name ? 'var(--accent-soft)' : 'var(--surface)',
                }}
              >
                <div style={{ fontSize: 14, fontWeight: 600, color: 'var(--text)', wordBreak: 'break-all' }}>{e.name}</div>
                <div style={{ fontSize: 13, color: 'var(--text-secondary)', marginTop: 2 }}>{e.description}</div>
                <div style={{ marginTop: 4, display: 'flex', alignItems: 'center', gap: 4, flexWrap: 'wrap' }}>
                  <span style={badgeStyle}>{sourceLabel(e.source)}</span>
                  {!!e.source_agent && <span style={badgeStyle}>{agentLabel(e.source_agent)}</span>}
                  {!!e.updated && <span style={{ fontSize: 12, color: 'var(--text-muted)' }}>{t('memory.updated')} {fmtTime(e.updated)}</span>}
                </div>
              </div>
            ))}
            {loaded && filtered.length === 0 && entries.length > 0 && (
              <div style={{ padding: 12, fontSize: 13, color: 'var(--text-muted)' }}>{t('memory.emptySearch')}</div>
            )}
          </div>
          {/* Detail side panel: body full text + metadata */}
          <div style={{ flex: '1 1 45%', minWidth: 0 }} data-testid="memory-detail">
            {current ? (
              <div style={{ border: '1px solid var(--border)', borderRadius: 8, background: 'var(--surface)', padding: 14 }}>
                <div style={{ fontSize: 15, fontWeight: 600, color: 'var(--text)', wordBreak: 'break-all' }}>{current.name}</div>
                <div style={{ fontSize: 13, color: 'var(--text-secondary)', margin: '4px 0 8px' }}>{current.description}</div>
                <pre style={{ margin: 0, fontSize: 13, whiteSpace: 'pre-wrap', wordBreak: 'break-word', color: 'var(--text)', background: 'var(--bg-soft)', padding: 10, borderRadius: 6, maxHeight: 320, overflowY: 'auto' }}>{current.body}</pre>
                <div style={{ marginTop: 8, display: 'flex', gap: 10, flexWrap: 'wrap', fontSize: 12, color: 'var(--text-muted)' }}>
                  <span>{t('memory.source')}: {sourceLabel(current.source)}</span>
                  {!!current.source_agent && <span>{t('memory.sourceAgent')}: {agentLabel(current.source_agent)}</span>}
                  <span>{t('memory.project')}: {current.project}</span>
                  {!!current.created && <span>{t('memory.created')}: {fmtTime(current.created)}</span>}
                  {!!current.updated && <span>{t('memory.updated')}: {fmtTime(current.updated)}</span>}
                </div>
                <div style={{ marginTop: 12, display: 'flex', gap: 8, alignItems: 'center', flexWrap: 'wrap' }}>
                  {armDelete === current.name ? (
                    <>
                      <span style={{ fontSize: 13, color: 'var(--danger)' }}>{t('memory.confirmDelete', { name: current.name })}</span>
                      <button className="btn btn-secondary btn-small" onClick={() => setArmDelete(null)}>{t('common.cancel')}</button>
                      <button className="modal-btn danger" onClick={doDelete}>{t('memory.delete')}</button>
                    </>
                  ) : (
                    <button className="btn btn-secondary btn-small" onClick={() => setArmDelete(current.name)}>{t('memory.delete')}</button>
                  )}
                </div>
              </div>
            ) : (
              <div style={{ padding: 14, fontSize: 13, color: 'var(--text-muted)', border: '1px dashed var(--border)', borderRadius: 8 }}>{t('memory.emptyDetail')}</div>
            )}
          </div>
        </div>
      )}
      {busy && <div style={{ marginTop: 8, fontSize: 12, color: 'var(--text-muted)' }}>…</div>}

      {/* Ops status bar (Phase 3): budget meter from memory.status. */}
      {statusInfo?.budget && (
        <div data-testid="memory-budget" style={{ marginTop: 14, display: 'flex', gap: 10, alignItems: 'center', flexWrap: 'wrap', fontSize: 13, color: 'var(--text-muted)' }}>
          {statusInfo.budget.exceeded ? (
            <span style={{ color: 'var(--danger)' }}>{t('memory.tokensExceeded')}</span>
          ) : (
            <span>{t('memory.tokens', { used: statusInfo.budget.usedTokens, limit: statusInfo.budget.limitTokens ?? '∞' })}</span>
          )}
        </div>
      )}
      {ingestReport && (
        <div data-testid="memory-report" style={{ marginTop: 8, fontSize: 13, color: 'var(--accent)' }}>{ingestReport}</div>
      )}

      {/* Recall preview: what the run path would inject for a task (debug view). */}
      <div data-testid="memory-recall" style={{ marginTop: 18, borderTop: '1px solid var(--border)', paddingTop: 12 }}>
        <div style={{ fontSize: 14, fontWeight: 600, color: 'var(--text)', marginBottom: 6 }}>{t('memory.recall')}</div>
        <div style={{ display: 'flex', gap: 8 }}>
          <input
            aria-label={t('memory.recallPlaceholder')}
            placeholder={t('memory.recallPlaceholder')}
            value={recallQuery}
            onChange={(e) => setRecallQuery(e.target.value)}
            onKeyDown={(e) => { if (e.key === 'Enter') void doRecall(); }}
            style={{ ...selectStyle, flex: 1 }}
          />
          <button className="btn btn-secondary btn-small" disabled={!recallQuery.trim() || busy} onClick={doRecall}>
            {t('memory.recallRun')}
          </button>
        </div>
        {recallResults !== null && (
          recallResults.length === 0 ? (
            <div style={{ marginTop: 8, fontSize: 13, color: 'var(--text-muted)' }}>{t('memory.recallEmpty')}</div>
          ) : (
            <ul data-testid="memory-recall-results" style={{ margin: '8px 0 0 0', padding: 0, listStyle: 'none', display: 'flex', flexDirection: 'column', gap: 4 }}>
              {recallResults.map((e) => (
                <li key={`${e.project}/${e.name}`} style={{ padding: '6px 10px', borderRadius: 6, border: '1px solid var(--border)', background: 'var(--surface)', fontSize: 12 }}>
                  <span style={{ fontWeight: 600, color: 'var(--text)' }}>{e.name}</span>
                  <span style={{ color: 'var(--text-secondary)', marginLeft: 8 }}>{e.description}</span>
                </li>
              ))}
            </ul>
          )
        )}
      </div>

      {/* Manual write form (Phase 2): name slugs host-side, conflicts fork. */}
      {showForm && (
        <div style={{ position: 'fixed', inset: 0, background: 'rgba(0,0,0,0.4)', display: 'flex', alignItems: 'center', justifyContent: 'center', zIndex: 60 }}>
          <div className="modal-card" data-testid="memory-form" style={{ width: 460, background: 'var(--surface)', border: '1px solid var(--border)', borderRadius: 10, padding: 16 }}>
            <div style={{ fontSize: 15, fontWeight: 600, marginBottom: 10 }}>{t('memory.add')}</div>
            <label style={{ display: 'block', fontSize: 13, marginBottom: 8 }}>
              {t('memory.name')}
              <input aria-label={t('memory.name')} value={formName} onChange={(e) => setFormName(e.target.value)} style={{ ...selectStyle, width: '100%', marginTop: 2 }} />
              <span style={{ display: 'block', fontSize: 12, color: 'var(--text-muted)', marginTop: 2 }}>{t('memory.nameHint')}</span>
            </label>
            <label style={{ display: 'block', fontSize: 13, marginBottom: 8 }}>
              {t('memory.description')}
              <input aria-label={t('memory.description')} value={formDesc} onChange={(e) => setFormDesc(e.target.value)} style={{ ...selectStyle, width: '100%', marginTop: 2 }} />
            </label>
            <label style={{ display: 'block', fontSize: 13, marginBottom: 8 }}>
              {t('memory.body')}
              <textarea aria-label={t('memory.body')} value={formBody} onChange={(e) => setFormBody(e.target.value)} rows={5} style={{ ...selectStyle, width: '100%', marginTop: 2, resize: 'vertical' }} />
            </label>
            <label style={{ display: 'flex', alignItems: 'center', gap: 6, fontSize: 13, marginBottom: 12 }}>
              <input type="checkbox" aria-label={t('memory.global')} checked={formGlobal} onChange={(e) => setFormGlobal(e.target.checked)} />
              {t('memory.global')}
            </label>
            <div style={{ display: 'flex', justifyContent: 'flex-end', gap: 8 }}>
              <button className="btn btn-secondary btn-small" onClick={() => { setShowForm(false); setSavedName(null); }}>{t('common.cancel')}</button>
              <button data-testid="memory-save" className="btn btn-primary btn-small" disabled={!formName.trim() || !formDesc.trim() || !formBody.trim() || busy} onClick={doSave}>
                {t('memory.save')}
              </button>
            </div>
          </div>
        </div>
      )}
      {savedName && <div data-testid="memory-saved" style={{ marginTop: 8, fontSize: 13, color: 'var(--accent)' }}>{t('memory.saved')}: {savedName}</div>}
    </div>
  );
}

export function ScheduledTasksTab() {
  const { t, lang } = useI18n();
  // WI-179: first-run refresh needs the provider's session list + running set
  // (this tab renders inside WorkspaceProvider, so the context is available).
  const ws = useWorkspaces();
  // Localized weekday names (index 0 = Sunday) and short month labels so the
  // frequency controls never mix languages with the surrounding UI.
  const DAY_NAMES = lang === 'zh'
    ? ['周日','周一','周二','周三','周四','周五','周六']
    : ['Sun','Mon','Tue','Wed','Thu','Fri','Sat'];
  const MONTH_SHORT = (m: number) => lang === 'zh'
    ? `${m}月`
    : ['Jan','Feb','Mar','Apr','May','Jun','Jul','Aug','Sep','Oct','Nov','Dec'][m - 1];
  const [tasks, setTasks] = useState<ScheduledTaskInfo[]>([]);
  const [showForm, setShowForm] = useState(false);
  const [editing, setEditing] = useState<ScheduledTaskInfo | null>(null);
  // WI-174: templates live in a popover off the "New Task from Template" button —
  // the page itself has no sub-tabs anymore.
  const [tplOpen, setTplOpen] = useState(false);
  const tplPopRef = useRef<HTMLDivElement | null>(null);
  // WI-179: deferred sidebar pokes after a create-then-run — cleared on
  // unmount so they can't fire into a dead provider.
  const firstRunTimersRef = useRef<ReturnType<typeof setTimeout>[]>([]);
  useEffect(() => () => {
    for (const id of firstRunTimersRef.current.splice(0)) clearTimeout(id);
  }, []);
  const tplBtnRef = useRef<HTMLButtonElement | null>(null);
  // Form fields
  const [tname, setTName] = useState('');
  const [modeType, setModeType] = useState('periodic');
  const [period, setPeriod] = useState('daily');
  const [time, setTime] = useState('09:00');
  const [dayOfWeek, setDayOfWeek] = useState(1);
  const [dayOfMonth, setDayOfMonth] = useState(1);
  const [sMonth, setSMonth] = useState(1);
  const [hours, setHours] = useState(3);
  const [daysOfWeek, setDaysOfWeek] = useState<number[]>([1,2,3,4,5]);
  const [datetime, setDatetime] = useState('');
  const [startDate, setStartDate] = useState('');
  const [endDate, setEndDate] = useState('');
  const [prompt, setPrompt] = useState('');
  const [model, setModel] = useState('');   // "providerId/variantId"; '' = active agent model
  const [enabled, setEnabled] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  // Model options for the task's model selector (all configured models).
  const [models, setModels] = useState<ModelEntry[]>([]);
  useEffect(() => {
    window.starriver.listModels().then(setModels).catch(() => {});
  }, []);
  const modelOptions = useMemo(() => {
    const opts: { id: string; name: string }[] = [];
    for (const m of models) {
      // Prefer the custom model ID as the selector prefix (parity with the
      // host-composed agent-binding names).
      const prefix = (m.modelLabel ?? '').trim() || m.name;
      if (m.models.length === 0) {
        opts.push({ id: `${m.id}/${m.defaultModel}`, name: `${prefix} · ${m.defaultModel}` });
      } else {
        for (const v of m.models) opts.push({ id: `${m.id}/${v.id}`, name: `${prefix} · ${v.name}` });
      }
    }
    return opts;
  }, [models]);
  const modelNameById = useMemo(() => {
    const map: Record<string, string> = {};
    for (const o of modelOptions) map[o.id] = o.name;
    return map;
  }, [modelOptions]);
  // Installed skills available to attach to a task prompt.
  const [skillsList, setSkillsList] = useState<SkillInfo[]>([]);
  useEffect(() => { window.starriver.listSkills().then(setSkillsList).catch(() => {}); }, []);
  // WI-64 Phase B: a task can name the agent it runs as (empty = Chat default).
  const [taskAgent, setTaskAgent] = useState('');
  const [agents, setAgents] = useState<AgentInfo[]>([]);
  useEffect(() => { window.starriver.listAgents().then(setAgents).catch(() => {}); }, []);
  // Tasks can bind to a workspace: runs group under it instead of Tasks.
  const [taskWorkspace, setTaskWorkspace] = useState('');
  const [wsList, setWsList] = useState<WorkspaceInfo[]>([]);
  useEffect(() => { Promise.resolve(window.starriver.listWorkspaces?.() ?? []).then(setWsList).catch(() => {}); }, []);
  // Skill picker (button + slash autocomplete) state.
  const promptRef = useRef<HTMLTextAreaElement>(null);
  const skillBtnRef = useRef<HTMLButtonElement>(null);
  const insertPosRef = useRef<{ start: number; end: number }>({ start: 0, end: 0 });
  const [pickerOpen, setPickerOpen] = useState(false);
  const [pickerMode, setPickerMode] = useState<'button' | 'slash'>('button');
  const [pickerFilter, setPickerFilter] = useState('');
  const [pickerIndex, setPickerIndex] = useState(0);
  // Distance from the textarea wrapper's bottom edge at which the button-mode popup
  // should sit (its bottom edge), so the popup opens just above the Skill button.
  const [btnPopupBottom, setBtnPopupBottom] = useState(30);
  const [caretPos, setCaretPos] = useState<{ top: number; left: number; bottom: number }>({ top: 0, left: 0, bottom: 0 });
  const filteredSkills = useMemo(() => {
    const f = pickerFilter.toLowerCase();
    return skillsList.filter(s => {
      if (!f) return true;
      const tok = skillTokenOf(s);
      return tok.toLowerCase().includes(f) || s.name.toLowerCase().includes(f) || (s.description || '').toLowerCase().includes(f);
    });
  }, [skillsList, pickerFilter]);
  useEffect(() => { setPickerIndex(0); }, [pickerFilter, pickerOpen]);
  // Measure the caret's offset within the textarea's wrapper (for the slash popup).
  const measureCaretIn = (ta: HTMLTextAreaElement, caret: number): { top: number; left: number; bottom: number } => {
    const wrap = ta.parentNode as HTMLElement | null;
    if (!wrap) return { top: 0, left: 0, bottom: 0 };
    const cs = window.getComputedStyle(ta);
    const div = document.createElement('div');
    Object.assign(div.style, {
      boxSizing: cs.boxSizing, width: cs.width,
      paddingTop: cs.paddingTop, paddingRight: cs.paddingRight, paddingBottom: cs.paddingBottom, paddingLeft: cs.paddingLeft,
      borderTopWidth: cs.borderTopWidth, borderRightWidth: cs.borderRightWidth, borderBottomWidth: cs.borderBottomWidth, borderLeftWidth: cs.borderLeftWidth,
      fontFamily: cs.fontFamily, fontSize: cs.fontSize, fontWeight: cs.fontWeight, lineHeight: cs.lineHeight, letterSpacing: cs.letterSpacing,
      whiteSpace: 'pre-wrap', wordWrap: 'break-word',
      position: 'absolute', top: '0px', left: '0px', visibility: 'hidden',
    } as Record<string, string>);
    div.textContent = ta.value.slice(0, caret);
    const marker = document.createElement('span');
    marker.textContent = '​';
    div.appendChild(marker);
    wrap.appendChild(div);
    const wrapH = wrap.clientHeight;
    const top = marker.offsetTop - ta.scrollTop;
    const left = marker.offsetLeft - ta.scrollLeft;
    wrap.removeChild(div);
    return { top, left, bottom: wrapH - top };
  };
  const insertSkillToken = (token: string) => {
    const { start, end } = insertPosRef.current;
    const insert = `/${token} `;
    const next = prompt.slice(0, start) + insert + prompt.slice(end);
    setPrompt(next);
    setPickerOpen(false);
    const pos = start + insert.length;
    requestAnimationFrame(() => {
      const ta = promptRef.current;
      if (ta) { ta.focus(); ta.setSelectionRange(pos, pos); }
    });
  };
  const onPromptChange = (e: React.ChangeEvent<HTMLTextAreaElement>) => {
    const val = e.target.value;
    setPrompt(val);
    const ta = e.target;
    const caret = ta.selectionStart ?? val.length;
    const before = val.slice(0, caret);
    const m = before.match(/\/([\w-]*)$/);
    if (m) {
      const slashStart = caret - m[1].length - 1;
      insertPosRef.current = { start: slashStart, end: caret };
      setPickerFilter(m[1]);
      setPickerMode('slash');
      setCaretPos(measureCaretIn(ta, caret));
      setPickerOpen(true);
    } else if (pickerOpen && pickerMode === 'slash') {
      setPickerOpen(false);
    }
  };
  const onPromptKeyDown = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if (!pickerOpen) return;
    if (e.key === 'Escape') { e.preventDefault(); setPickerOpen(false); return; }
    if (pickerMode === 'slash' && filteredSkills.length > 0) {
      if (e.key === 'Enter') { e.preventDefault(); insertSkillToken(skillTokenOf(filteredSkills[pickerIndex])); return; }
      if (e.key === 'ArrowDown') { e.preventDefault(); setPickerIndex(i => Math.min(i + 1, filteredSkills.length - 1)); return; }
      if (e.key === 'ArrowUp') { e.preventDefault(); setPickerIndex(i => Math.max(i - 1, 0)); return; }
    }
  };
  const openButtonPicker = () => {
    const ta = promptRef.current;
    const start = ta ? (ta.selectionStart ?? prompt.length) : prompt.length;
    const end = ta ? (ta.selectionEnd ?? start) : start;
    insertPosRef.current = { start, end };
    // Anchor the popup so its bottom edge sits just above the Skill button.
    const btn = skillBtnRef.current;
    const wrap = ta?.parentElement as HTMLElement | null;
    if (btn && wrap) setBtnPopupBottom(wrap.clientHeight - btn.offsetTop + 2);
    setPickerMode('button'); setPickerFilter(''); setPickerOpen(o => !o);
  };
  const refresh = useCallback(async () => {
    try { setTasks(await (window as any).starriver.listScheduledTasks()); } catch (e: any) { setError(e.message); }
  }, []);
  useEffect(() => { refresh(); }, [refresh]);
  // WI-174: the template popover closes on outside click / Esc.
  useEffect(() => {
    if (!tplOpen) return;
    const onDown = (e: MouseEvent) => {
      const target = e.target as Node;
      if (tplPopRef.current?.contains(target) || tplBtnRef.current?.contains(target)) return;
      setTplOpen(false);
    };
    const onKey = (e: KeyboardEvent) => { if (e.key === 'Escape') setTplOpen(false); };
    document.addEventListener('mousedown', onDown);
    document.addEventListener('keydown', onKey);
    return () => {
      document.removeEventListener('mousedown', onDown);
      document.removeEventListener('keydown', onKey);
    };
  }, [tplOpen]);
  const resetForm = () => {
    setTName(''); setModeType('periodic'); setPeriod('daily'); setTime('09:00'); setDayOfWeek(1);
    setDayOfMonth(1); setSMonth(1); setHours(3); setDaysOfWeek([1,2,3,4,5]); setDatetime('');
    setStartDate(''); setEndDate(''); setPrompt(''); setModel(''); setTaskAgent(''); setTaskWorkspace(''); setEnabled(true); setError(null); setEditing(null);
  };
  const openAdd = () => { resetForm(); setShowForm(true); };
  // Prefill the form from a template so the user can tweak (model, location, etc.) before saving.
  const applyTemplate = (tpl: TaskTemplate) => {
    resetForm();
    setTName(tpl.name[lang]);
    setModeType(tpl.modeType);
    setPrompt(tpl.prompt[lang]);
    if (tpl.modeType === 'periodic') {
      setPeriod(tpl.period || 'daily');
      setTime(tpl.time || '09:00');
      if (tpl.dayOfWeek != null) setDayOfWeek(tpl.dayOfWeek);
    } else {
      setHours(tpl.hours || 1);
      setDaysOfWeek(tpl.daysOfWeek && tpl.daysOfWeek.length ? tpl.daysOfWeek : [0,1,2,3,4,5,6]);
    }
    // WI-174: the picker popover closes as the form opens (no stacked layers).
    setTplOpen(false);
    setShowForm(true);
  };
  const openEdit = (tk: ScheduledTaskInfo) => {
    setTName(tk.name); setPrompt(tk.prompt); setModel(tk.model || ''); setTaskAgent(tk.agent || ''); setTaskWorkspace(tk.workspace || ''); setEnabled(tk.enabled); setEditing(tk);
    setModeType(tk.modeType); setStartDate(tk.startDate || ''); setEndDate(tk.endDate || '');
    if (tk.modeType === 'periodic') {
      setPeriod(tk.period || 'daily'); setTime(tk.time || '09:00');
      setDayOfWeek(tk.dayOfWeek ?? 1); setDayOfMonth(tk.dayOfMonth ?? 1); setSMonth(tk.month ?? 1);
    } else if (tk.modeType === 'interval') {
      setHours(tk.hours || 3);
      // [] = every day (host contract) — prefill all-7, NOT Mon-Fri.
      setDaysOfWeek(tk.daysOfWeek.length ? tk.daysOfWeek : [0,1,2,3,4,5,6]);
    } else {
      setDatetime(tk.datetime || '');
    }
    setShowForm(true);
  };
  const buildMode = () => {
    if (modeType === 'periodic') {
      const m: any = { type: 'periodic', period, time };
      if (period === 'weekly') m.day_of_week = dayOfWeek;
      if (period === 'monthly' || period === 'quarterly') m.day_of_month = dayOfMonth;
      if (period === 'quarterly') m.month = sMonth;
      if (period === 'yearly') { m.month = sMonth; m.day_of_month = dayOfMonth; }
      return m;
    } else if (modeType === 'interval') {
      return { type: 'interval', hours, days_of_week: daysOfWeek };
    } else {
      return { type: 'once', datetime };
    }
  };
  const handleSave = async () => {
    if (!tname.trim() || !prompt.trim()) { setError('Name and prompt are required.'); return; }
    if (modeType === 'once' && !datetime) { setError('Date and time are required for one-shot tasks.'); return; }
    setBusy(true); setError(null);
    try {
      const modeJson = JSON.stringify(buildMode());
      const sd = startDate || null; const ed = endDate || null;
      const attached = parseSkillTokens(prompt, skillsList);
      // Host contract: empty string CLEARS the binding, null/undefined KEEPS
      // it — on edit, an explicit "(default)"/"(none)" selection must send ''.
      const m = editing ? (model ?? '') : (model || null);
      const agentArg = editing ? (taskAgent ?? '') : (taskAgent || null);
      const workspaceArg = editing ? (taskWorkspace ?? '') : (taskWorkspace || null);
      let firstRunError: string | null = null;
      if (editing) {
        await (window as any).starriver.updateScheduledTask(editing.name, modeJson, prompt.trim(), m, agentArg, workspaceArg, enabled, sd, ed, attached);
      } else {
        await (window as any).starriver.addScheduledTask(tname.trim(), modeJson, prompt.trim(), m, agentArg, workspaceArg, enabled, sd, ed, attached);
        // WI-179: create-then-run — the first run creates the session right
        // away (a bound workspace files it under that workspace; otherwise
        // the sidebar Tasks folder), and the blinking dot marks it live.
        // A failed start never rolls back the saved task: the panel's
        // lastStatus/error surfaces carry the outcome.
        try {
          await (window as any).starriver.runScheduledTaskNow(tname.trim());
        } catch (runErr: any) {
          // Set AFTER resetForm() below — reset clears the shared error state.
          firstRunError = t('scheduled.firstRunFailed') + (runErr?.message ? `: ${runErr.message}` : '');
        }
      }
      setShowForm(false); resetForm(); refresh();
      if (firstRunError) setError(firstRunError);
      // WI-179: the run's session id lands asynchronously (scheduler
      // mark_running); re-poll the sidebar session list + running set so the
      // new row and its dot appear without waiting the next 3s tick.
      const pokeSidebar = () => { void ws.loadSessions(); void ws.refreshRunningSessions(); };
      pokeSidebar();
      [1000, 3000, 6000].forEach(delay => firstRunTimersRef.current.push(setTimeout(pokeSidebar, delay)));
    } catch (e: any) { setError(e.message); }
    setBusy(false);
  };
  const handleRemove = async (taskName: string) => {
    if (!confirm(t('scheduled.removeConfirm') || 'Remove this task?')) return;
    try { await (window as any).starriver.removeScheduledTask(taskName); refresh(); } catch (e: any) { setError(e.message); }
  };
  // WI-180: newest recorded run timestamp for the task — the jump poll uses
  // it to tell the freshly triggered run apart from history.
  const newestRunTs = async (taskName: string): Promise<number> => {
    try {
      const runs = (await Promise.resolve((window as any).starriver.listAllTaskRuns?.() ?? [])) as TaskRunRecordWithName[];
      return runs
        .filter(r => r.taskName === taskName)
        .map(r => Date.parse(r.timestamp) || 0)
        .reduce((a, b) => Math.max(a, b), 0);
    } catch { return 0; }
  };
  // Poll for the run's session: jobs.list carries the sessionId the moment
  // the scheduler marks the run active; a fast run that already finished
  // shows up in jobs.taskRuns instead. First hit navigates (Coding view) and
  // pokes the sidebar so the row + running dot are current.
  const jumpToRunSession = async (taskName: string, baselineTs: number): Promise<void> => {
    for (let i = 0; i < 20; i++) {
      await new Promise(r => setTimeout(r, 400));
      try {
        const jobs = (await Promise.resolve((window as any).starriver.listJobs?.() ?? [])) as JobInfo[];
        const active = jobs.find(j => j.taskName === taskName && j.status === 'running' && j.sessionId);
        if (active?.sessionId) { requestOpenSession(active.sessionId); void ws.loadSessions(); void ws.refreshRunningSessions(); return; }
        const runs = (await Promise.resolve((window as any).starriver.listAllTaskRuns?.() ?? [])) as TaskRunRecordWithName[];
        const fresh = runs
          .filter(r => r.taskName === taskName && r.sessionId && (Date.parse(r.timestamp) || 0) > baselineTs)
          .sort((a, b) => (Date.parse(b.timestamp) || 0) - (Date.parse(a.timestamp) || 0))[0];
        if (fresh?.sessionId) { requestOpenSession(fresh.sessionId); void ws.loadSessions(); void ws.refreshRunningSessions(); return; }
      } catch { /* registry blip — keep polling */ }
    }
  };
  const handleRunNow = async (taskName: string) => {
    const baselineTs = await newestRunTs(taskName);
    try { await (window as any).starriver.runScheduledTaskNow(taskName); } catch (e: any) { setError(e.message); return; }
    refresh();
    void jumpToRunSession(taskName, baselineTs);
  };
  const toggleDay = (d: number) => {
    setDaysOfWeek(prev => prev.includes(d) ? prev.filter(x => x !== d) : [...prev, d].sort());
  };
  const handleToggle = async (tk: ScheduledTaskInfo) => {
    const modeJson = JSON.stringify(buildModeForTask(tk));
    try { await (window as any).starriver.updateScheduledTask(tk.name, modeJson, null, null, null, null, !tk.enabled, null, null); refresh(); } catch (e: any) { setError(e.message); }
  };
  const buildModeForTask = (tk: ScheduledTaskInfo) => {
    if (tk.modeType === 'periodic') {
      const m: any = { type: 'periodic', period: tk.period, time: tk.time };
      if (tk.period === 'weekly') m.day_of_week = tk.dayOfWeek;
      if (tk.period === 'monthly' || tk.period === 'quarterly') m.day_of_month = tk.dayOfMonth;
      if (tk.period === 'quarterly') m.month = tk.month;
      if (tk.period === 'yearly') { m.month = tk.month; m.day_of_month = tk.dayOfMonth; }
      return m;
    } else if (tk.modeType === 'interval') {
      return { type: 'interval', hours: tk.hours, days_of_week: tk.daysOfWeek };
    } else {
      return { type: 'once', datetime: tk.datetime };
    }
  };
  const statusColor = (s?: string) => s === 'error' ? 'var(--danger)' : 'var(--success)';
  return (
    <div style={{ padding: '16px 24px', height: '100%', overflowY: 'auto' }}>
      <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginBottom: 12 }}>
        <h3 style={{ margin: 0, fontSize: 16 }}>{t('scheduled.title')}</h3>
        <div style={{ display: 'flex', gap: 8, position: 'relative' }}>
          <button ref={tplBtnRef} className="btn btn-secondary btn-small" onClick={() => setTplOpen(o => !o)}>{t('scheduled.newTaskFromTemplate')}</button>
          <button className="btn btn-primary btn-small" onClick={openAdd}>{t('scheduled.newTask')}</button>
          {/* WI-174: template picker popover — picking a template closes it and
              opens the Add Task modal prefilled (applyTemplate). */}
          {tplOpen && (
            <div ref={tplPopRef} data-testid="tpl-popover" style={{ position: 'absolute', right: 0, top: 'calc(100% + 6px)', width: 680, maxWidth: 'calc(100vw - 48px)', background: 'var(--panel-bg)', border: '1px solid var(--border)', borderRadius: 14, boxShadow: 'var(--shadow-md)', padding: 14, zIndex: 30 }}>
              <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'baseline', marginBottom: 10, padding: '0 2px' }}>
                <span style={{ fontSize: 14, fontWeight: 600 }}>{t('scheduled.templatesTitle')}</span>
                <button onClick={() => setTplOpen(false)} style={{ background: 'none', border: 'none', color: 'var(--text-muted)', cursor: 'pointer', fontSize: 15, lineHeight: 1 }}>✕</button>
              </div>
              <div style={{ display: 'grid', gridTemplateColumns: 'repeat(3, 1fr)', gap: 8, maxHeight: 420, overflowY: 'auto' }}>
                {TASK_TEMPLATES.map(tpl => (
                  <div key={tpl.id} className="tpl-card" onClick={() => applyTemplate(tpl)} title={tpl.desc[lang]} style={{ background: 'var(--card-bg, var(--bg-soft))', borderRadius: 8, padding: '9px 11px', cursor: 'pointer', display: 'flex', gap: 8, alignItems: 'flex-start', border: '1px solid transparent', transition: 'border-color 0.15s, background 0.15s' }} onMouseEnter={e => { e.currentTarget.style.borderColor = 'var(--accent)'; }} onMouseLeave={e => { e.currentTarget.style.borderColor = 'transparent'; }}>
                    <div style={{ fontSize: 18, lineHeight: '22px' }}>{tpl.icon}</div>
                    <div style={{ minWidth: 0 }}>
                      <div style={{ fontSize: 14, fontWeight: 600, color: 'var(--text)' }}>{tpl.name[lang]}</div>
                      <div style={{ fontSize: 12, color: 'var(--text-muted)', marginTop: 2 }}>{tpl.desc[lang]}</div>
                    </div>
                  </div>
                ))}
              </div>
            </div>
          )}
        </div>
      </div>
      {error && <div style={{ color: 'var(--danger)', marginBottom: 8, fontSize: 13 }}>{error}</div>}

      {/* Task list (WI-174: the only section — templates live in the header popover) */}
      <div style={{ display: 'grid', gap: 8 }}>
        {tasks.length === 0 && !showForm && (
          <div style={{ color: 'var(--text-muted)', fontSize: 14 }}>
            {t('scheduled.noTasks')}
            <div style={{ fontSize: 13, marginTop: 6 }}>{t('scheduled.emptyHint')}</div>
          </div>
        )}
          {tasks.map(tk => (
            <div key={tk.name} style={{ background: 'var(--card-bg, var(--bg-soft))', borderRadius: 8, padding: '10px 14px' }}>
              <div style={{ display: 'flex', alignItems: 'center', gap: 10, marginBottom: 4 }}>
                <strong style={{ flex: 1, fontSize: 14 }}>{tk.name}</strong>
                <span onClick={() => handleToggle(tk)} style={{ fontSize: 12, background: tk.enabled ? 'var(--success)' : 'var(--text-muted)', color: 'var(--surface)', borderRadius: 4, padding: '1px 6px', cursor: 'pointer' }}>{tk.enabled ? 'ON' : 'OFF'}</span>
              </div>
              <div style={{ fontSize: 13, color: 'var(--text-secondary)', display: 'flex', gap: 16, flexWrap: 'wrap' }}>
                <span>🔄 {tk.description || tk.modeType}</span>
                {tk.model && <span title={tk.model}>🤖 {modelNameById[tk.model] || tk.model}</span>}
                {tk.agent && <span title={tk.agent}>👤 {tk.agent}</span>}
                {tk.nextRun && <span>⏭ {tk.nextRun.replace('T',' ').slice(0,16)}</span>}
                {tk.lastStatus && <span style={{ color: statusColor(tk.lastStatus) }}>● {tk.lastStatus}</span>}
              </div>
              {(tk.startDate || tk.endDate) && <div style={{ fontSize: 12, color: 'var(--text-muted)', marginTop: 2 }}>📅 {tk.startDate || '—'} ~ {tk.endDate || '—'}</div>}
              <div style={{ display: 'flex', gap: 4, marginTop: 6 }}>
                <button className="btn btn-secondary btn-small" onClick={() => openEdit(tk)} style={{ fontSize: 11 }}>{t('common.edit')}</button>
                <button className="btn btn-secondary btn-small" onClick={() => handleRunNow(tk.name)} style={{ fontSize: 11 }}>{t('scheduled.runNow')}</button>
                <button className="btn btn-secondary btn-small" onClick={() => handleRemove(tk.name)} style={{ fontSize: 12, color: 'var(--danger)' }}>{t('common.remove')}</button>
              </div>
            </div>
          ))}
      </div>

      {showForm && (
        <div style={{ position: 'fixed', inset: 0, background: 'var(--overlay-strong)', display: 'flex', alignItems: 'center', justifyContent: 'center', zIndex: 1000 }} onClick={() => { setShowForm(false); resetForm(); }}>
          <div style={{ background: 'var(--surface)', borderRadius: 12, width: 560, height: 620, maxHeight: '90vh', display: 'flex', flexDirection: 'column', boxShadow: 'var(--shadow-md)' }} onClick={(e) => e.stopPropagation()}>
            {/* Header */}
            <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', padding: '16px 20px 12px', borderBottom: '1px solid var(--border)', flexShrink: 0 }}>
              <h3 style={{ margin: 0, fontSize: 15 }}>{editing ? t('scheduled.editTask') : t('scheduled.addTask')}</h3>
              <button onClick={() => { setShowForm(false); resetForm(); }} style={{ background: 'none', border: 'none', fontSize: 18, cursor: 'pointer', color: 'var(--text-muted)', lineHeight: 1 }}>✕</button>
            </div>
            {/* Error banner */}
            {error && <div style={{ padding: '8px 12px', margin: '10px 20px 0', background: 'var(--danger-soft)', border: '1px solid var(--danger-border)', borderRadius: 6, color: 'var(--danger)', fontSize: 14, flexShrink: 0 }}>{error}</div>}
            {/* Body */}
            <div style={{ padding: '16px 20px', overflowY: 'auto', display: 'flex', flexDirection: 'column', gap: 16, flex: 1, minHeight: 0 }}>
              {/* Section: Basic Info */}
              <div>
                <div style={{ fontSize: 12, color: 'var(--text-muted)', textTransform: 'uppercase', letterSpacing: '0.6px', marginBottom: 6 }}>{t('scheduled.basicInfo')}</div>
                <div style={{ display: 'flex', gap: 10 }}>
                  <div style={{ flex: 1 }}>
                    <label style={{ fontSize: 13, color: 'var(--text-secondary)', display: 'block', marginBottom: 4 }}>{t('scheduled.name')}</label>
                    <input className="toolbar-input" value={tname} onChange={e => setTName(e.target.value)} placeholder={t('scheduled.namePlaceholder')} style={{ width: '100%' }} />
                  </div>
                </div>
                <div style={{ marginTop: 10 }}>
                  <label style={{ fontSize: 13, color: 'var(--text-secondary)', display: 'block', marginBottom: 4 }}>{t('scheduled.model')}</label>
                  <select className="toolbar-select" value={model} onChange={e => setModel(e.target.value)} style={{ width: '100%' }}>
                    <option value="">{t('scheduled.modelDefault')}</option>
                    {modelOptions.map(o => <option key={o.id} value={o.id}>{o.name}</option>)}
                  </select>
                </div>
                <div style={{ marginTop: 10 }}>
                  <label style={{ fontSize: 13, color: 'var(--text-secondary)', display: 'block', marginBottom: 4 }}>{t('scheduled.agent')}</label>
                  <select data-testid="scheduled-agent" className="toolbar-select" value={taskAgent} onChange={e => setTaskAgent(e.target.value)} style={{ width: '100%' }}>
                    <option value="">{t('scheduled.agentDefault')}</option>
                    {agents.map(a => <option key={a.name} value={a.name}>{a.name}</option>)}
                  </select>
                </div>
                <div style={{ marginTop: 10 }}>
                  <label style={{ fontSize: 13, color: 'var(--text-secondary)', display: 'block', marginBottom: 4 }}>{t('scheduled.workspace')}</label>
                  <select data-testid="scheduled-workspace" className="toolbar-select" value={taskWorkspace} onChange={e => setTaskWorkspace(e.target.value)} style={{ width: '100%' }}>
                    <option value="">{t('scheduled.workspaceNone')}</option>
                    {wsList.map(w => <option key={w.id} value={w.id}>{w.title}</option>)}
                  </select>
                </div>
              </div>
              {/* Section: Frequency */}
              <div>
                <div style={{ fontSize: 12, color: 'var(--text-muted)', textTransform: 'uppercase', letterSpacing: '0.6px', marginBottom: 6 }}>{t('scheduled.frequency')}</div>
                {/* Segmented mode selector */}
                <div style={{ display: 'flex', gap: 4, background: 'var(--bg-soft)', borderRadius: 8, padding: 3, marginBottom: 10 }}>
                  {['periodic','interval','once'].map(mt => (
                    <button key={mt} type="button" onClick={() => setModeType(mt)} style={{ flex: 1, padding: '6px 0', border: 'none', borderRadius: 6, fontSize: 13, cursor: 'pointer', fontWeight: 600, background: modeType === mt ? 'var(--surface)' : 'transparent', color: modeType === mt ? 'var(--accent)' : 'var(--text-secondary)', boxShadow: modeType === mt ? 'var(--shadow-sm)' : 'none' }}>{t(`scheduled.${mt}`)}</button>
                  ))}
                </div>
                {/* Periodic mode */}
                {modeType === 'periodic' && (
                  <div style={{ background: 'var(--panel-bg)', borderRadius: 8, padding: 12, display: 'flex', flexDirection: 'column', gap: 8, minHeight: 92, boxSizing: 'border-box' }}>
                    <div style={{ display: 'flex', gap: 8, alignItems: 'center', flexWrap: 'wrap' }}>
                      <select className="toolbar-select" value={period} onChange={e => setPeriod(e.target.value)} style={{ flex: 1, minWidth: 100 }}>
                        {['daily','weekly','biweekly','monthly','quarterly','yearly'].map(p => <option key={p} value={p}>{t(`scheduled.${p}`)}</option>)}
                      </select>
                      {(period === 'weekly') && <select className="toolbar-select" value={dayOfWeek} onChange={e => setDayOfWeek(parseInt(e.target.value))} style={{ width: 80 }}>{DAY_NAMES.map((d,i) => <option key={i} value={i}>{d}</option>)}</select>}
                      {(period === 'monthly' || period === 'quarterly') && <input className="toolbar-input" type="number" min={1} max={31} value={dayOfMonth} onChange={e => setDayOfMonth(parseInt(e.target.value)||1)} style={{ width: 60, textAlign: 'center' }} placeholder={t('scheduled.dayUnit')} />}
                      {period === 'quarterly' && <select className="toolbar-select" value={sMonth} onChange={e => setSMonth(parseInt(e.target.value))} style={{ width: 80 }}>{[1,4,7,10].map(m => <option key={m} value={m}>{MONTH_SHORT(m)}</option>)}</select>}
                      {period === 'yearly' && <><select className="toolbar-select" value={sMonth} onChange={e => setSMonth(parseInt(e.target.value))} style={{ width: 80 }}>{Array.from({length:12},(_,i)=>i+1).map(m=><option key={m} value={m}>{MONTH_SHORT(m)}</option>)}</select><input className="toolbar-input" type="number" min={1} max={31} value={dayOfMonth} onChange={e => setDayOfMonth(parseInt(e.target.value)||1)} style={{ width: 60, textAlign: 'center' }} placeholder={t('scheduled.dayUnit')} /></>}
                      <input className="toolbar-input" type="time" value={time} onChange={e => setTime(e.target.value)} style={{ width: 100 }} />
                    </div>
                  </div>
                )}
                {/* Interval mode */}
                {modeType === 'interval' && (
                  <div style={{ background: 'var(--panel-bg)', borderRadius: 8, padding: 12, display: 'flex', flexDirection: 'column', gap: 8, minHeight: 92, boxSizing: 'border-box' }}>
                    <div style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
                      <span style={{ fontSize: 13, color: 'var(--text-secondary)' }}>{t('scheduled.every')}</span>
                      <input className="toolbar-input" type="number" min={1} max={24} value={hours} onChange={e => setHours(parseInt(e.target.value)||1)} style={{ width: 60, textAlign: 'center' }} />
                      <span style={{ fontSize: 13, color: 'var(--text-secondary)' }}>{t('scheduled.hoursUnit')}</span>
                    </div>
                    <div style={{ display: 'flex', gap: 4, flexWrap: 'wrap' }}>
                      {DAY_NAMES.map((d, i) => (
                        <span key={i} onClick={() => toggleDay(i)} style={{ fontSize: 12, padding: '3px 10px', borderRadius: 12, cursor: 'pointer', background: daysOfWeek.includes(i) ? 'var(--success)' : 'var(--border)', color: daysOfWeek.includes(i) ? 'var(--surface)' : 'var(--text-secondary)', userSelect: 'none' }}>{d}</span>
                      ))}
                    </div>
                  </div>
                )}
                {/* Once mode */}
                {modeType === 'once' && (
                  <div style={{ background: 'var(--panel-bg)', borderRadius: 8, padding: 12, minHeight: 92, boxSizing: 'border-box' }}>
                    <div style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
                      <input
                        className="toolbar-input"
                        type="date"
                        value={datetime.slice(0, 10)}
                        onChange={e => {
                          const d = e.target.value;
                          const t = datetime.slice(11, 16) || '09:00';
                          setDatetime(d ? `${d}T${t}` : '');
                        }}
                        style={{ flex: 1, minWidth: 0 }}
                      />
                      <span style={{ fontSize: 13, color: 'var(--text-muted)', flexShrink: 0 }}>·</span>
                      <input
                        className="toolbar-input"
                        type="time"
                        value={datetime.slice(11, 16)}
                        onChange={e => {
                          const t = e.target.value;
                          const d = datetime.slice(0, 10);
                          setDatetime(d ? `${d}T${t}` : '');
                        }}
                        style={{ flex: 1, minWidth: 0 }}
                      />
                    </div>
                  </div>
                )}
              </div>
              {/* Section: Validity */}
              <div>
                <div style={{ fontSize: 12, color: 'var(--text-muted)', textTransform: 'uppercase', letterSpacing: '0.6px', marginBottom: 6 }}>{t('scheduled.validity')}</div>
                <div style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
                  <input className="toolbar-input" type="date" value={startDate} onChange={e => setStartDate(e.target.value)} style={{ flex: 1, minWidth: 0, fontSize: 12 }} />
                  <span style={{ fontSize: 13, color: 'var(--text-muted)' }}>~</span>
                  <input className="toolbar-input" type="date" value={endDate} onChange={e => setEndDate(e.target.value)} style={{ flex: 1, minWidth: 0, fontSize: 12 }} />
                </div>
              </div>
              {/* Section: Task Prompt */}
              <div>
                <div style={{ fontSize: 12, color: 'var(--text-muted)', textTransform: 'uppercase', letterSpacing: '0.6px', marginBottom: 6 }}>{t('scheduled.prompt')}</div>
                <div style={{ position: 'relative' }}>
                  <textarea ref={promptRef} className="toolbar-input" value={prompt} onChange={onPromptChange} onKeyDown={onPromptKeyDown} placeholder={t('scheduled.promptPlaceholder')} rows={4} style={{ width: '100%', boxSizing: 'border-box', resize: 'vertical', minHeight: 88, paddingBottom: 30, fontFamily: 'inherit', lineHeight: 1.5 }} />
                  <button ref={skillBtnRef} type="button" onClick={openButtonPicker} title={t('scheduled.attachSkill')} style={{ position: 'absolute', left: 6, bottom: 6, border: '1px solid var(--border-strong)', background: 'var(--surface)', borderRadius: 7, padding: '2px 9px', fontSize: 13, cursor: 'pointer', color: 'var(--text-secondary)', display: 'flex', alignItems: 'center', gap: 3 }}>＋ {t('scheduled.attachSkill')}</button>
                  {pickerOpen && (
                    <div style={{ position: 'absolute', zIndex: 30, ...(pickerMode === 'slash' ? { bottom: caretPos.bottom + 4, left: Math.min(caretPos.left, 180) } : { bottom: btnPopupBottom, left: 0 }), background: 'var(--surface)', border: '1px solid var(--border-strong)', borderRadius: 8, boxShadow: 'var(--shadow-md)', width: 270, maxHeight: 280, display: 'flex', flexDirection: 'column', overflow: 'hidden' }}>
                      <div style={{ fontSize: 12, color: 'var(--text-muted)', padding: '6px 12px 2px', textTransform: 'uppercase', letterSpacing: '0.5px' }}>{t('scheduled.attachSkill')}</div>
                      <div style={{ overflowY: 'auto', flex: 1, paddingBottom: 4 }}>
                        {filteredSkills.length === 0 && <div style={{ padding: '10px 12px', color: 'var(--text-muted)', fontSize: 12 }}>—</div>}
                        {filteredSkills.map((s, i) => (
                          <div key={s.name} onMouseDown={(e) => { e.preventDefault(); insertSkillToken(skillTokenOf(s)); }} style={{ padding: '7px 12px', cursor: 'pointer', display: 'flex', flexDirection: 'column', gap: 1, background: i === pickerIndex && pickerMode === 'slash' ? 'var(--accent-soft)' : 'transparent' }}>
                            <div style={{ display: 'flex', alignItems: 'center', gap: 6 }}>
                              <span style={{ fontSize: 14, fontWeight: 600, color: 'var(--text)' }}>{s.has_command && s.command ? `/${s.command}` : `/${s.name}`}</span>
                              {s.always_on && <span style={{ fontSize: 10, background: 'var(--info-soft)', color: 'var(--info)', padding: '0 5px', borderRadius: 6 }}>always-on</span>}
                            </div>
                            {s.description && <div style={{ fontSize: 12, color: 'var(--text-muted)', whiteSpace: 'nowrap', overflow: 'hidden', textOverflow: 'ellipsis' }}>{s.description}</div>}
                          </div>
                        ))}
                      </div>
                    </div>
                  )}
                </div>
                {parseSkillTokens(prompt, skillsList).length > 0 && (
                  <div style={{ fontSize: 12, color: 'var(--text-muted)', marginTop: 6 }}>{t('scheduled.attached')}: {parseSkillTokens(prompt, skillsList).map(n => `/${n}`).join('  ')}</div>
                )}
              </div>
            </div>
            {/* Footer */}
            <div style={{ padding: '12px 20px', borderTop: '1px solid var(--border)', display: 'flex', alignItems: 'center', gap: 8, flexShrink: 0 }}>
              {/* Enabled toggle — pinned in the footer so it is always visible */}
              <div onClick={() => setEnabled(!enabled)} style={{ display: 'flex', alignItems: 'center', gap: 8, cursor: 'pointer', userSelect: 'none', marginRight: 'auto' }}>
                <div style={{ width: 34, height: 19, borderRadius: 10, cursor: 'pointer', flexShrink: 0, background: enabled ? 'var(--accent)' : 'var(--border-strong)', transition: 'background 0.2s', position: 'relative' }}>
                  <div style={{ width: 15, height: 15, borderRadius: 8, background: 'var(--surface)', position: 'absolute', top: 2, left: enabled ? 17 : 2, transition: 'left 0.2s' }} />
                </div>
                <span style={{ fontSize: 13, color: 'var(--text-secondary)' }}>{t('scheduled.enabled')}</span>
              </div>
              <button className="btn btn-secondary btn-small" onClick={() => { setShowForm(false); resetForm(); }}>{t('scheduled.cancel')}</button>
              <button className="btn btn-primary btn-small" onClick={handleSave} disabled={busy}>{busy ? '...' : t('scheduled.save')}</button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

// FPM P2: tab ids moved from a hard-coded union to plain strings — driven by
// the contrib registry, built-in and plugin tabs share one registration path
// (see contrib/registry.ts).
type TabId = string;

/** Sidebar group switch (product dropdown): a pill trigger with
 *  the active group's name, opening a downward menu of name + description
 *  rows with a check on the active one. */
function SidebarGroupSwitch({ group, onChange }: { group: 'agent' | 'hub'; onChange: (g: 'agent' | 'hub') => void }) {
  const { t } = useI18n();
  const [open, setOpen] = useState(false);
  const wrapRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!open) return;
    const close = () => setOpen(false);
    const onDown = (e: MouseEvent) => {
      if (wrapRef.current && wrapRef.current.contains(e.target as Node)) return;
      close();
    };
    const onKey = (e: KeyboardEvent) => { if (e.key === 'Escape') close(); };
    document.addEventListener('mousedown', onDown);
    document.addEventListener('keydown', onKey);
    return () => { document.removeEventListener('mousedown', onDown); document.removeEventListener('keydown', onKey); };
  }, [open]);
  const meta: Record<'agent' | 'hub', { name: string; desc: string }> = {
    agent: { name: 'Starriver Agent', desc: t('sidebar.agentDesc') },
    hub: { name: 'Starriver Hub', desc: t('sidebar.hubDesc') },
  };
  return (
    <div className="sidebar-group-wrap" ref={wrapRef}>
      <button
        type="button"
        className="sidebar-group-btn"
        aria-label="sidebar-group"
        aria-expanded={open}
        onClick={() => setOpen(o => !o)}
      >
        <span>{meta[group].name}</span>
        <span className="composer-approval-caret" aria-hidden="true" />
      </button>
      {open && (
        <div className="sidebar-group-menu">
          {(['agent', 'hub'] as const).map(g => (
            <button
              key={g}
              type="button"
              className="sidebar-group-item"
              onClick={() => { setOpen(false); if (g !== group) onChange(g); }}
            >
              <span className="sidebar-group-item-name">{meta[g].name}</span>
              <span className="sidebar-group-item-desc">{meta[g].desc}</span>
              {g === group && <span className="composer-approval-check">✓</span>}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}

export function SearchBox({ onOpenSession }: { onOpenSession: () => void }) {
  // WI-149: search and open a session — typing filters by title, clicking a result opens it.
  const [query, setQuery] = useState('');
  const [open, setOpen] = useState(false);
  const [sessions, setSessions] = useState<Array<{ id: string; title?: string | null }>>([]);
  const ws = useWorkspaces();

  useEffect(() => {
    window.starriver.listSessions?.()
      .then((rows: Array<{ id: string; title?: string | null }>) => setSessions(rows ?? []))
      .catch(() => {});
  }, []);

  const q = query.trim().toLowerCase();
  const results = q
    ? sessions.filter((s) => (s.title || '').toLowerCase().includes(q)).slice(0, 8)
    : [];

  return (
    <div className="search-box">
      <input type="text" value={query} onChange={(e) => { setQuery(e.target.value); setOpen(e.target.value.trim().length >= 1); }} placeholder="Search sessions..." className="search-input" onBlur={() => setTimeout(() => setOpen(false), 200)} />
      {open && results.length > 0 && (
        <div className="search-dropdown">
          {results.map((r) => (
            <div key={r.id} className="search-result" onMouseDown={() => {
              setOpen(false);
              setQuery('');
              ws.setActiveSessionId(r.id);
              onOpenSession();
            }}>
              <span className="search-result-type">Session</span>
              <span className="search-result-name">{r.title || 'Chat'}</span>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

/* ─── CodingView ─── */

/** Backtrack picker: lists the session's user messages; picking one forks a new
 *  thread from just before that message. */
function BacktrackModal({ messages, loading, onSelect, onCancel }: {
  messages: SessionMessage[];
  loading: boolean;
  onSelect: (m: SessionMessage) => void;
  onCancel: () => void;
}) {
  const { t } = useI18n();
  return (
    <div className="modal-overlay" onClick={onCancel}>
      <div className="modal-card" style={{ maxHeight: '70%', display: 'flex', flexDirection: 'column' }} onClick={(e) => e.stopPropagation()}>
        <div className="modal-title">{t('thread.backtrackTitle')}</div>
        <div className="modal-body" style={{ flex: 1, overflowY: 'auto', fontSize: 13, display: 'flex', flexDirection: 'column', gap: 2 }}>
          {loading && <div style={{ color: 'var(--text-muted)', padding: 8 }}>{t('thread.loading')}</div>}
          {!loading && messages.length === 0 && (
            <div style={{ color: 'var(--text-muted)', padding: 8 }}>{t('thread.backtrackEmpty')}</div>
          )}
          {messages.map((m, i) => (
            <div
              key={m.id}
              onClick={() => onSelect(m)}
              style={{
                padding: '6px 8px', cursor: 'pointer', borderRadius: 4, color: 'var(--text)',
                overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap',
              }}
              onMouseEnter={(e) => { e.currentTarget.style.background = 'var(--bg-soft)'; }}
              onMouseLeave={(e) => { e.currentTarget.style.background = 'transparent'; }}
              title={`#${i + 1}: ${m.content}`}>
              <span style={{ color: 'var(--text-muted)', marginRight: 6, fontSize: 11 }}>#{i + 1}</span>
              {m.content.length > 60 ? m.content.slice(0, 60) + '…' : m.content}
            </div>
          ))}
        </div>
        <div className="modal-actions">
          <button className="modal-btn" onClick={onCancel}>{t('common.cancel')}</button>
        </div>
      </div>
    </div>
  );
}

/** /delete confirmation popover. */
function SessionDeleteModal({ onConfirm, onCancel }: {
  onConfirm: () => void;
  onCancel: () => void;
}) {
  const { t } = useI18n();
  return (
    <div className="modal-overlay" onClick={onCancel}>
      <div className="modal-card" onClick={(e) => e.stopPropagation()}>
        <div className="modal-title">{t('thread.deleteConfirmTitle')}</div>
        <div className="modal-body">{t('thread.deleteConfirmBody')}</div>
        <div className="modal-actions">
          <button className="modal-btn" onClick={onCancel}>{t('common.cancel')}</button>
          <button className="modal-btn danger" onClick={onConfirm}>{t('coding.delete')}</button>
        </div>
      </div>
    </div>
  );
}

/** /unarchive popover: lists archived sessions; clicking one restores it. */
function ArchiveModal({ sessions, onRestore, onCancel }: {
  sessions: SessionInfo[];
  onRestore: (s: SessionInfo) => void;
  onCancel: () => void;
}) {
  const { t } = useI18n();
  return (
    <div className="modal-overlay" onClick={onCancel}>
      <div className="modal-card" style={{ maxHeight: '70%', display: 'flex', flexDirection: 'column' }} onClick={(e) => e.stopPropagation()}>
        <div className="modal-title">{t('thread.archiveTitle')}</div>
        <div className="modal-body" style={{ flex: 1, overflowY: 'auto', fontSize: 13, display: 'flex', flexDirection: 'column', gap: 2 }}>
          {sessions.length === 0 && (
            <div style={{ color: 'var(--text-muted)', padding: 8 }}>{t('thread.archiveEmpty')}</div>
          )}
          {sessions.map((s, i) => (
            <div
              key={s.id}
              onClick={() => onRestore(s)}
              style={{
                padding: '6px 8px', cursor: 'pointer', borderRadius: 4, color: 'var(--text)',
                overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap',
              }}
              onMouseEnter={(e) => { e.currentTarget.style.background = 'var(--bg-soft)'; }}
              onMouseLeave={(e) => { e.currentTarget.style.background = 'transparent'; }}
              data-tip={s.title || 'Coding task'}>
              <span style={{ color: 'var(--text-muted)', marginRight: 6, display: 'inline-flex', alignItems: 'center' }} dangerouslySetInnerHTML={{ __html: ICON_ARCHIVE }} />
              {s.title || 'Coding task'} <span style={{ color: 'var(--text-muted)', fontSize: 11 }}>({s.messageCount})</span>
            </div>
          ))}
        </div>
        <div className="modal-actions">
          <button className="modal-btn" onClick={onCancel}>{t('common.cancel')}</button>
        </div>
      </div>
    </div>
  );
}

/** Confirm workspace-group deletion: the directory and session logs stay on
 *  disk; the grouping is removed and its active sessions no longer render in
 * this sidebar (a deliberate deviation — the reference files them under
 *  Ungrouped). Archived ones stay reachable for unarchive. */
export function ConfirmDeleteModal(props: {
  workspace: WorkspaceInfo;
  sessionCount: number;
  onConfirm: () => void;
  onCancel: () => void;
}) {
  const { t } = useI18n();
  return (
    <div className="modal-overlay" onClick={props.onCancel}>
      <div className="modal-card" onClick={(e) => e.stopPropagation()}>
        <div className="modal-title">{t('workspace.deleteTitle')}</div>
        <div className="modal-body">
          {t('workspace.deleteBody', { name: props.workspace.title, count: props.sessionCount })}
        </div>
        <div className="modal-actions">
          <button className="modal-btn" onClick={props.onCancel}>{t('common.cancel')}</button>
          <button className="modal-btn danger" onClick={props.onConfirm}>{t('common.delete')}</button>
        </div>
      </div>
    </div>
  );
}

/** Risk confirmation for the danger permission preset (RiskConfirmation
    parity): Enable stays disabled until the acknowledge checkbox is ticked. */
export function PermissionRiskModal(props: {
  open: boolean;
  acknowledged: boolean;
  onAcknowledgedChange: (v: boolean) => void;
  onConfirm: () => void;
  onCancel: () => void;
}) {
  const { t } = useI18n();
  if (!props.open) return null;
  return (
    <div className="modal-overlay" onClick={props.onCancel}>
      <div className="modal-card" onClick={(e) => e.stopPropagation()}>
        <div className="modal-title">{t('common.permissionRiskTitle')}</div>
        <div className="modal-body">{t('common.permissionRiskBody')}</div>
        <label style={{ display: 'flex', alignItems: 'center', gap: 8, fontSize: 13, cursor: 'pointer', padding: '0 16px' }}>
          <input
            type="checkbox"
            checked={props.acknowledged}
            onChange={(e) => props.onAcknowledgedChange(e.target.checked)}
            aria-label="risk-acknowledge"
          />
          {t('common.permissionRiskAcknowledge')}
        </label>
        <div className="modal-actions">
          <button className="modal-btn" onClick={props.onCancel}>{t('common.permissionRiskCancel')}</button>
          <button
            className="modal-btn danger"
            disabled={!props.acknowledged}
            style={{ opacity: props.acknowledged ? undefined : 0.5, cursor: props.acknowledged ? 'pointer' : 'not-allowed' }}
            onClick={() => { if (props.acknowledged) props.onConfirm(); }}
          >
            {t('common.permissionRiskConfirm')}
          </button>
        </div>
      </div>
    </div>
  );
}

// WI-152: composer draft (module-level, survives CodingView unmount/remount).
const composerDrafts: Record<string, string> = {};

export function CodingView({ debugEnabled }: { debugEnabled: boolean }) {
  const { t, lang, setLang } = useI18n();
  const [messages, setMessages] = useState<Message[]>([]);
  // Stale-todo watermark for the Todo widget: TaskStarted on a fresh turn
  // hides the previous turn's fully-completed todo list until this turn
  // writes a fresh todo_write (in-progress lists stay visible across turns).
  const [todoHideUpTo, setTodoHideUpTo] = useState(0);
  const [input, setInput] = useState(() => composerDrafts['coding-view'] ?? '');
  // WI-152: composer draft — switching tabs unmounts CodingView and its input state with it;
  // mirror to a module-level store and restore verbatim on return (sending clears both).
  useEffect(() => { composerDrafts['coding-view'] = input; }, [input]);
  const [sending, setSending] = useState(false);
  // WI-88: pending fork point for "resend from here" (user-message seq).
  const resendFromRef = useRef<number | null>(null);
  // WI-93: background terminals panel.
  const [terminalsOpen, setTerminalsOpen] = useState(false);
  // WI-97: goal object for the current session (from goal.get).
  const [goalInfo, setGoalInfo] = useState<{ objective: string; tokenBudget?: number; tokensUsed: number; status: string } | null>(null);
  // WI-101: pagination — whether earlier messages exist beyond the loaded page.
  // WI-126: full history loads up front — the "load earlier" paging state
  // (hasEarlier/earliestSeqRef/loadingEarlierRef) is gone.

  // BUG-86: session-load generation — a slow history fetch for session A must
  // not write into the view after the user has already switched to B.
  const loadGenRef = useRef(0);
  // WI-98: doctor report rows.
  const [doctorReport, setDoctorReport] = useState<{ name: string; status: string; detail: string }[] | null>(null);
  // WI-94: goal-mode token budget (persisted; empty = unlimited). The used
  // side reuses the WI-90 sessionUsage accumulator.
  const [goalBudget, setGoalBudget] = useState<number>(() => {
    try { return Number(localStorage.getItem('starriver-goal-budget')) || 0; } catch { return 0; }
  });
  const setGoalBudgetPersisted = (n: number) => {
    setGoalBudget(n);
    try {
      if (n > 0) localStorage.setItem('starriver-goal-budget', String(n));
      else localStorage.removeItem('starriver-goal-budget');
    } catch { /* storage unavailable */ }
  };
  const [terminals, setTerminals] = useState<{ id: string; status: string; startedAtMs: number; command?: { startedAtMs: number; finishedAtMs?: number; exitCode?: number } | null }[]>([]);
  // Terminal ids whose idle state was already surfaced at a turn's settle —
  // one reminder per terminal per idle episode (re-armed once it shows
  // activity again).
  const idleNotifiedRef = useRef<Set<string>>(new Set());
  // WI-152: visibility of running bash/terminal commands — the bash tool is a one-shot process, not a
  // PTY session, so listTerminals can't see it; running shell-type tool calls are tracked separately
  // so the entry icon stays lit while cargo and friends run.
  const shellCallsRef = useRef<Map<string, { cmd: string; startedAt: number }>>(new Map()); // callId → command + start clock (WI-157 stopwatch)
  const [shellBusy, setShellBusy] = useState(false);
  const [shellCmds, setShellCmds] = useState<{ cmd: string; startedAt: number }[]>([]);
  const syncShellState = useCallback(() => {
    const cmds = Array.from(shellCallsRef.current.values());
    setShellCmds(cmds);
    setShellBusy(cmds.length > 0);
  }, []);
  // WI-150: resident polling — the entry icon's visibility depends on live background terminals, so it cannot
  // refresh only while the popover is open; 5s is cheap enough (listTerminals reads an in-memory table).
  useEffect(() => {
    let alive = true;
    const refresh = () => {
      window.starriver.listTerminals?.()
        .then((r) => { if (alive) setTerminals(r?.terminals ?? []); })
        .catch(() => {});
    };
    refresh();
    const timer = window.setInterval(refresh, 5000);
    return () => { alive = false; window.clearInterval(timer); };
  }, []);
  // WI-85: prompts queued while a turn runs. `sendingRef` mirrors `sending`
  // so the Enter handler never reads a stale closure.
  const sendingRef = useRef(false);
  sendingRef.current = sending;
  // 远端回合重放的防抖计时器（traceSnapshot 驱动）。
  const remoteResyncTimerRef = useRef<number | null>(null);
  const queueSeqRef = useRef(0);
  const [queueItems, setQueueItems] = useState<{ id: string; text: string }[]>([]);
  const queueItemsRef = useRef(queueItems);
  const [editingQueueId, setEditingQueueId] = useState<string | null>(null);
  const [queueEditDraft, setQueueEditDraft] = useState('');
  const setQueue = (next: { id: string; text: string }[]) => {
    queueItemsRef.current = next;
    setQueueItems(next);
  };
  const refreshQueueRef = useRef<(sid: string) => void>(() => {});
  const sessionIdRef = useRef<string>("");
  const bottomRef = useRef<HTMLDivElement>(null);
  const assistantIdRef = useRef<string | null>(null);
  const thinkingIdRef = useRef<string | null>(null);
  const toolNameRef = useRef<string>("");
  const toolMsgIdRef = useRef<Record<string, string>>({});
  /** toolCallId → tool name, so toolResult can classify edit/write/patch outputs. */
  const toolNameByIdRef = useRef<Record<string, string>>({});
  /** Per-file diff (latest edit wins) accumulated across the running turn. */
  const turnDiffsRef = useRef<Map<string, { add: number; del: number; text: string }>>(new Map());
  // WI-86: tool wall-clock starts (callId → epoch ms).
  const toolStartRef = useRef<Record<string, number>>({});
  // WI-86: expanded full-output tool rows (message id set).
  const [expandedTools, setExpandedTools] = useState<Set<string>>(new Set());
  // WI-90: cumulative session usage (reset on session switch).
  const [sessionUsage, setSessionUsage] = useState<{ in: number; out: number }>({ in: 0, out: 0 });
  /** Turn-end file-change summary block; null = hidden or superseded. */
  const [turnSummary, setTurnSummary] = useState<{
    files: { path: string; add: number; del: number; text: string }[];
    add: number;
    del: number;
    forId: string;
  } | null>(null);
  /** Turn start (ms epoch) + per-step usage accumulation for the cost line. */
  const turnStartRef = useRef<number>(0);
  const turnTokensRef = useRef<{ input: number; output: number }>({ input: 0, output: 0 });
  const [turnUsage, setTurnUsage] = useState<{
    durationMs: number;
    input: number;
    output: number;
    forId: string;
  } | null>(null);
  /** Id of the most recent assistant bubble. assistantIdRef is nulled by the
   *  modern assistantText flow, so turnEnd keys off this instead. */
  const lastAssistantIdRef = useRef<string | null>(null);
  /** Streaming reasoning: delta buffer + rAF scheduling + dedupe flag. */
  const thinkingBufRef = useRef('');
  const thinkingRafRef = useRef(0);
  const thinkingViaDeltaRef = useRef(false);
  const inputRef = useRef<HTMLTextAreaElement>(null);

  // Attachment state. WI-180: the stash survives CodingView unmounts —
  // switching tabs (Scheduled/Settings/…) disposes the composer and the
  // staged images died with it. `splice(0)` on init ADOPTS the stash (and
  // empties it); the effect mirrors state back so a later remount restores
  // exactly what was staged. Sending/clearing flows through the same path.
  const [attachments, setAttachments] = useState<Attachment[]>(() => stagedAttachmentsStash.splice(0));
  useEffect(() => {
    stagedAttachmentsStash.splice(0, stagedAttachmentsStash.length, ...attachments);
  }, [attachments]);
  // WI-152 S2 image gate: the staged agent's active route's resolved vision
  // support decides whether the composer accepts images at all. Computed in
  // an effect once `stagedChat`/model state land; the paste/drop/file/send
  // handlers read the ref so declaration order stays irrelevant.
  const [models, setModels] = useState<ModelEntry[]>([]);
  const [imagesDisabled, setImagesDisabled] = useState(false);
  const imagesDisabledRef = useRef(false);
  const [imagesNotice, setImagesNotice] = useState<string | null>(null);
  const rejectImages = useCallback(() => {
    setImagesNotice(t('coding.imagesDisabled'));
    window.setTimeout(() => setImagesNotice(null), 4000);
  }, [t]);

  const removeAttachment = useCallback((id: string) => {
    setAttachments(prev => prev.filter(a => a.id !== id));
  }, []);

  const handlePaste = useCallback(async (e: React.ClipboardEvent) => {
    const items = e.clipboardData?.items;
    if (!items) return;
    let hasImage = false;
    for (let i = 0; i < items.length; i++) {
      if (items[i].type.startsWith('image/')) { hasImage = true; break; }
    }
    if (!hasImage) return;
    if (imagesDisabledRef.current) { e.preventDefault(); rejectImages(); return; }
    e.preventDefault();
    for (let i = 0; i < items.length; i++) {
      if (items[i].type.startsWith('image/')) {
        const file = items[i].getAsFile();
        if (file) {
          const att = await processFile(file);
          setAttachments(prev => [...prev, att]);
        }
      }
    }
  }, [rejectImages]);

  const handleDrop = useCallback(async (e: React.DragEvent) => {
    e.preventDefault();
    const files = e.dataTransfer?.files;
    if (!files) return;
    if (imagesDisabledRef.current && Array.from(files).some(f => f.type.startsWith('image/'))) {
      rejectImages();
      return;
    }
    for (let i = 0; i < files.length; i++) {
      if (files[i].type.startsWith('image/')) {
        const att = await processFile(files[i]);
        setAttachments(prev => [...prev, att]);
      }
    }
  }, [rejectImages]);

  const handleFileSelect = useCallback(async (e: React.ChangeEvent<HTMLInputElement>) => {
    const files = e.target.files;
    if (!files) return;
    for (let i = 0; i < files.length; i++) {
      const f = files[i];
      if (imagesDisabledRef.current && detectAttachmentKind(f.name, f.type) === 'image') {
        rejectImages();
        continue;
      }
      const att = await processFile(f);
      setAttachments(prev => [...prev, att]);
    }
    e.target.value = '';
  }, [rejectImages]);

  // API Debug (controlled by Settings → Coding). Ref stays in sync for the send() closure.
  const debugEnabledRef = useRef(debugEnabled);
  const [debugLog, setDebugLog] = useState<{ turn: number; kind: 'request' | 'response'; data: any }[]>([]);
  useEffect(() => { debugEnabledRef.current = debugEnabled; if (!debugEnabled) setDebugLog([]); }, [debugEnabled]);

  // Slash command palette: /model, /plan|/build|/auto, skill commands.
  // group: 'coding' = Coding Agent built-ins, 'skills' = SKILL.md commands (isolated).
  type SlashItem = { id: string; cmd: string; desc: string; group: 'coding' | 'skills'; kind: 'mode' | 'model-cmd' | 'model' | 'skill' | 'fill' | 'client'; value?: string | null; name?: string };
  const [skills, setSkills] = useState<SkillInfo[]>([]);
  const [mcpServers, setMcpServers] = useState<McpServerInfo[]>([]);
  const [slashOpen, setSlashOpen] = useState(false);
  const [slashIndex, setSlashIndex] = useState(0);
  const slashBoxRef = useRef<HTMLDivElement>(null);
  const suppressSlashRef = useRef(false);
  useEffect(() => {
    window.starriver.listSkills().then(setSkills).catch(() => {});
  }, []);
  useEffect(() => {
    window.starriver.listMcpServers().then(setMcpServers).catch(() => {});
  }, []);

  // Close the palette when clicking outside the input/dropdown box.
  useEffect(() => {
    const onDocDown = (e: MouseEvent) => {
      if (slashBoxRef.current && !slashBoxRef.current.contains(e.target as Node)) setSlashOpen(false);
    };
    document.addEventListener('mousedown', onDocDown);
    return () => document.removeEventListener('mousedown', onDocDown);
  }, []);

  const buildSlashItems = (query: string): SlashItem[] => {
    // After "/model " → list the staged agent's bound models as sub-options
    // (chat mode runs the Chat Agent, whose binding is independent).
    if (query.startsWith('model ')) {
      const modelAgent = stagedChat ? CHAT_AGENT_NAME : CODING_AGENT_NAME;
      return (bindings.find(b => b.agentName === modelAgent)?.boundModels ?? [])
        .map(m => ({ id: `model:${m.id}`, cmd: `/model ${m.id}`, desc: m.name, group: 'coding' as const, kind: 'model' as const }));
    }
    const items: SlashItem[] = [
      { id: 'mode:plan', cmd: '/plan', desc: 'Switch to Plan mode (read-only)', group: 'coding', kind: 'mode', value: 'plan' },
      { id: 'mode:ptc', cmd: '/ptc', desc: 'Switch to PTC mode (batch pipeline)', group: 'coding', kind: 'mode', value: 'ptc' },
      { id: 'mode:goal', cmd: '/goal', desc: 'Switch to Goal mode (autonomous loop)', group: 'coding', kind: 'mode', value: 'goal' },
      { id: 'mode:build', cmd: '/build', desc: 'Switch to Build mode (full tools)', group: 'coding', kind: 'mode', value: 'build' },
      { id: 'mode:auto', cmd: '/auto', desc: 'Switch to Auto mode', group: 'coding', kind: 'mode', value: null },
      { id: 'model-cmd', cmd: '/model', desc: 'Switch model', group: 'coding', kind: 'model-cmd' },
      { id: 'compact', cmd: '/compact', desc: 'Compact context (summarize history)', group: 'coding', kind: 'client' },
      { id: 'status', cmd: '/status', desc: 'Show session ID, model, turns, token usage', group: 'coding', kind: 'client' },
      { id: 'diff', cmd: '/diff', desc: 'Show git diff of the current project workspace', group: 'coding', kind: 'client' },
      { id: 'usage', cmd: '/usage', desc: 'Show session token usage & message count', group: 'coding', kind: 'client' },
      { id: 'init', cmd: '/init', desc: 'Generate AGENTS.md for the current project', group: 'coding', kind: 'client' },
      { id: 'export', cmd: '/export', desc: 'Export current session to a Markdown file', group: 'coding', kind: 'client' },
      { id: 'backtrack', cmd: '/backtrack', desc: 'Fork a new thread from an earlier user message', group: 'coding', kind: 'client' },
      { id: 'delete', cmd: '/delete', desc: 'Delete the current session', group: 'coding', kind: 'client' },
      { id: 'lang', cmd: '/lang', desc: 'Switch UI language (zh/en)', group: 'coding', kind: 'client' },
      { id: 'worktree', cmd: '/worktree', desc: 'Toggle worktree isolation for this session', group: 'coding', kind: 'client' },
      { id: 'name', cmd: '/name', desc: 'Rename the current session', group: 'coding', kind: 'fill' },
      { id: 'archive', cmd: '/archive', desc: 'Archive the current session (restorable)', group: 'coding', kind: 'client' },
      { id: 'unarchive', cmd: '/unarchive', desc: 'Restore an archived session', group: 'coding', kind: 'client' },
      { id: 'help', cmd: '/help', desc: 'Show available commands', group: 'coding', kind: 'client' },
      { id: 'review', cmd: '/review', desc: 'Review specified files', group: 'coding', kind: 'fill' },
      { id: 'review-uncommitted', cmd: '/review uncommitted', desc: 'Structured review of all uncommitted changes', group: 'coding', kind: 'fill' },
      { id: 'goal', cmd: '/goal <objective> | <budget>', desc: 'Set autonomous goal (with optional token budget)', group: 'coding', kind: 'fill' },
      { id: 'goal-clear', cmd: '/goal clear', desc: 'Clear the session goal', group: 'coding', kind: 'client' },
      { id: 'doctor', cmd: '/doctor', desc: 'Run read-only diagnostics (config/sandbox/disk/db)', group: 'coding', kind: 'client' },
      { id: 'review-branch', cmd: '/review branch <name>', desc: 'Structured review vs a base branch', group: 'coding', kind: 'fill' },
      { id: 'explore', cmd: '/explore', desc: 'Explore the codebase (Plan mode)', group: 'coding', kind: 'fill' },
      { id: 'test', cmd: '/test', desc: 'Write unit tests', group: 'coding', kind: 'fill' },
      ...skills.filter(s => s.command).map(s => ({ id: `skill:${s.name}`, cmd: s.command!, desc: s.description || s.name, group: 'skills' as const, kind: 'skill' as const, name: s.name })),
    ];
    if (!query) return items;
    const q = query.toLowerCase();
    return items.filter(i => i.cmd.toLowerCase().includes(q) || i.desc.toLowerCase().includes(q));
  };

  const handleSlashSelect = (item: SlashItem) => {
    setSlashOpen(false);
    if (item.kind === 'mode') { handleModeChange(item.value); setInput(''); return; }
    if (item.kind === 'model') { (stagedChat ? handleChatModelChange : handleModelChange)(item.id.replace('model:', '')); setInput(''); return; }
    if (item.kind === 'model-cmd') { setInput('/model '); suppressSlashRef.current = true; setSlashOpen(true); setSlashIndex(0); inputRef.current?.focus(); return; }
    if (item.kind === 'client') { setInput(item.cmd); suppressSlashRef.current = true; inputRef.current?.focus(); return; }
    // skill / fill: fill the command and let the user type arguments
    setInput(`${item.cmd} `);
    suppressSlashRef.current = true;
    setSlashIndex(0);
    inputRef.current?.focus();
  };

  // Workspace + session list state lives in the App-level WorkspaceProvider (the
  // sidebar section renders it and must persist across tabs); this view consumes
  // it and mirrors the "which session is open" decision into the local stream.
  const wsCtx = useWorkspaces();
  const {
    workspaces,
    archivedSessions,
    activeWorkspaceId,
    setActiveWorkspaceId,
    activeSessionId,
    setActiveSessionId,
    loadSessions,
    activateWorkspace,
    setShowNewWorkspace,
    handleUnarchive,
    newConvSeq,
  } = wsCtx;
 /** Middle-column view: conversation (message stream) | trajectory (-style ledger). */
  const [view, setView] = useState<'chat' | 'trajectory'>('chat');
  const [gitStatus, setGitStatus] = useState<WorktreeStatus | null>(null);
  const [isolated, setIsolated] = useState(false);
  const [taskStarted, setTaskStarted] = useState(false);
  // BUG-96: the backend still holds this session's single input slot (the
  // previous turn task is alive — e.g. after a renderer reload, or a turn
  // hung past its last event). The composer flips to the Stop affordance
  // until the stale turn is cancelled; set from sessions.running sync and
  // from SESSION_BUSY send failures.
  const [staleBusy, setStaleBusy] = useState(false);
  const [workspaceInstructions, setWorkspaceInstructions] = useState<string | null>(null);
  // WI-35 B: the banner notes when the composite exceeded the byte budget.
  const [instructionsTruncated, setInstructionsTruncated] = useState(false);
  const messagesRef = useRef<Message[]>([]);
  const sessionIdRef2 = useRef<string>('');
  // Thread fork/backtrack: backtrack picker state + delete second confirmation
  const [backtrackOpen, setBacktrackOpen] = useState(false);
  const [backtrackMessages, setBacktrackMessages] = useState<SessionMessage[]>([]);
  const [backtrackLoading, setBacktrackLoading] = useState(false);
  const [confirmDeleteSession, setConfirmDeleteSession] = useState<string | null>(null);
  // /unarchive restore overlay
  const [archiveOpen, setArchiveOpen] = useState(false);
  // Plan review card (plan fold): driven by the model's
  // `exit_plan_mode` call — the plan text is presented for review; approve
  // exits plan mode, reject sends the feedback back to the model.
  const [planReview, setPlanReview] = useState<{ requestId: string; plan: string } | null>(null);
  // Right-hand Plan panel: the plan markdown stays available after approval as
  // an execution reference (the review card itself closes on decision).
  const [lastPlan, setLastPlan] = useState<string | null>(null);
  const [planApproved, setPlanApproved] = useState(false);
  const [planPanelOpen, setPlanPanelOpen] = useState(false);
  // Panel file tab (review): a turn's changed file opened as a diff alongside
  // the Plan tab. The diff text is the turn-event snapshot — exact for the
  // reviewed change and worktree-correct by construction.
  const [reviewFile, setReviewFile] = useState<{ path: string; text: string } | null>(null);
  // Fullscreen viewer for the diff shown in the right panel (Esc / ✕ closes).
  const [diffFull, setDiffFull] = useState(false);
  useEffect(() => {
    if (!diffFull) return;
    const onKey = (e: KeyboardEvent) => { if (e.key === 'Escape') setDiffFull(false); };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [diffFull]);
  const [panelTab, setPanelTab] = useState<'plan' | 'file'>('plan');
  const openReviewFile = (path: string, text: string) => {
    setReviewFile({ path, text });
    setPanelTab('file');
  };
  // Plan phase for the status bar: planning (read-only) vs executing (the
  // review approved and the sandbox was released back to workspace-write).
  // Plan phase for the status bar: planning (read-only) vs executing (the
  // review approved and the sandbox was released back to workspace-write).
  const [planPhase, setPlanPhase] = useState<'planning' | 'executing'>('planning');
  const { width: planWidth, onMouseDown: planOnMouseDown } = useResizable(320, 260, 420, 'right');
 // Composer permission preset (permission-presets parity): one bundle
  // picker replaces the old approval dropdown. manual / accept-edits / auto /
  // danger-full-access, or the derived `custom`.
  const [permissionPreset, setPermissionPresetState] = useState('manual');
  // The execute callback must read the latest value (state closures go stale):
  // when a non-default preset was picked before the session exists, it is
  // pushed to the backend once TaskStarted delivers the session id.
  const permissionPresetRef = useRef(permissionPreset);
  const sessionIdRef3 = useRef<string | null>(null);

  const refreshGitStatus = useCallback(async () => {
    const active = workspaces.find(w => w.id === activeWorkspaceId);
    if (active) {
      try { setGitStatus(await window.starriver.getWorktreeStatus(active.path)); }
      catch { setGitStatus(null); }
    } else {
      setGitStatus(null);
    }
  }, [workspaces, activeWorkspaceId]);

  useEffect(() => { refreshGitStatus(); }, [refreshGitStatus]);

  useEffect(() => {
    const onFocus = () => refreshGitStatus();
    window.addEventListener('focus', onFocus);
    return () => window.removeEventListener('focus', onFocus);
  }, [refreshGitStatus]);

  const loadSessionMessages = useCallback(async (sessionId: string) => {
    try {
      const gen = ++loadGenRef.current;
      // WI-126: load the WHOLE history up front (user decision — no more
      // "load earlier" paging). The no-limit session.messages call keeps
      // server-stamped seqs (BUG-86 fork anchors read straight off the rows).
      const msgs = await window.starriver.getSessionMessages(sessionId);
      if (gen !== loadGenRef.current) return;
      let loaded: Message[] = msgs.map((m: any) => ({
        id: crypto.randomUUID(), role: m.role as Message['role'], content: m.content,
        seq: m.seq ?? undefined,
        ts: m.ts ?? undefined,
        usage: m.usage ?? undefined,
        durationMs: m.durationMs ?? undefined,
        doneTs: m.doneTs ?? undefined,
        fileChanges: m.fileChanges ?? undefined,
      }));
      setMessages(loaded);
      messagesRef.current = loaded;
      setTodoHideUpTo(0);
      setActiveSessionId(sessionId);
      sessionIdRef2.current = sessionId;
      // Pin the runner to the agent that started the session (write-once host
      // stamp); legacy rows fall back to workspace membership (ungrouped ⇒
      // chat), so follow-ups never swap the agent mid-conversation.
      window.starriver.listSessions().then((all) => {
        if (gen !== loadGenRef.current) return;
        const s = all.find(x => x.id === sessionId);
        const chat = s?.agent ? s.agent === CHAT_AGENT_NAME : !s?.workspaceId;
        sessionAgentRef.current = chat ? CHAT_AGENT_NAME : CODING_AGENT_NAME;
        setNewTaskMode(chat ? 'chat' : 'coding');
        // WI-152: the status-bar workspace (and new sends' cwd) follows the session's own — switching from a nexplan session
        // to a starriver one no longer lingers on the previous workspace. Chat sessions have none and stay put.
        const wsId = s?.workspaceId;
        if (wsId) {
          setActiveWorkspaceId(wsId);
          if (!chat) {
            window.starriver.setActiveWorkspace(wsId, CODING_AGENT_NAME)
              .catch((e) => console.error('[Coding] session workspace activation failed:', e));
          }
        }
      }).catch(() => {});
      // Replay the session's creation-locked execution mode and let the host apply
      // it to the kernel (sandbox/approval presets follow the session instead of
      // sticking to whatever the previous session left behind).
      window.starriver.getAgentMode?.(sessionId)
        .then((m) => {
          if (gen !== loadGenRef.current) return;
          setMode(m && m !== 'standard' ? m : null);
        })
        .catch((e) => console.error('[Coding] getAgentMode(session) failed:', e));
      // Permission preset read-back: the picker reflects the session's folded
      // knobs (table name, or custom for non-table combos). The folded sandbox
      // also drives the plan phase: read-only ⇒ still planning. (BUG-92: the
      // paged path used to return early and skip this plus the plan resets.)
      window.starriver.getPermission?.(sessionId)
        .then((p) => {
          if (gen !== loadGenRef.current) return;
          setPermissionPresetState(p?.preset ?? 'manual');
          permissionPresetRef.current = p?.preset ?? 'manual';
          setPlanPhase(p?.sandbox === 'read-only' ? 'planning' : 'executing');
        })
        .catch((e) => console.error('[Coding] getPermission(session) failed:', e));
      // The Plan panel shows the live plan of THIS session only — a reopened
      // session has no renderer-side plan readback yet.
      setLastPlan(null);
      setPlanPanelOpen(false);
      setPlanApproved(false);
    } catch (e) { console.error('[Coding] Load session error:', e); }
  }, []);

  const [mode, setMode] = useState<string | null>(null);
  // /explore borrows Plan's read-only policy for one exploration: the mode to
  // restore when the turn ends.
  const exploreRestoreRef = useRef<string | null>(null);
  const restoreExploreMode = () => {
    if (exploreRestoreRef.current === null) return;
    const restore = exploreRestoreRef.current;
    exploreRestoreRef.current = null;
    setMode(restore && restore !== 'standard' ? restore : null);
    void window.starriver.setAgentMode(restore === 'standard' ? null : restore)
      .catch((e) => console.error('[Coding] setAgentMode (explore restore) failed:', e));
    if (restore !== 'plan' && sessionIdRef2.current) {
      window.starriver.confirmPlan(sessionIdRef2.current)
        .catch((e) => console.error('[Coding] confirmPlan (explore unlock) failed:', e));
    }
    // The exploration's borrowed planning phase ends with the turn: re-derive
    // preset + phase so the approval picker reflects the real state (WI-144).
    // confirmPlan's release stamps land async — deriving the phase from a
    // getPermission read here would race them; when we just confirmed a
    // non-plan restore the phase is 'executing' by construction. The preset
    // fold (approval/policy) is untouched by confirmPlan, so the readback is
    // race-free.
    if (sessionIdRef2.current) {
      if (restore !== 'plan') setPlanPhase('executing');
      window.starriver.getPermission?.(sessionIdRef2.current)
        .then((p) => {
          setPermissionPresetState(p?.preset ?? 'manual');
          permissionPresetRef.current = p?.preset ?? 'manual';
          if (restore === 'plan') {
            setPlanPhase(p?.sandbox === 'read-only' ? 'planning' : 'executing');
          }
        })
        .catch((e) => console.error('[Coding] getPermission (explore restore) failed:', e));
    }
  };

  const [approval, setApproval] = useState<{ tool: string; reason: string; params?: any; resolve: (allowed: boolean, remember?: boolean) => void } | null>(null);
  // WI-67 user-questions: an agent ask surfaces here and renders as a
  // composer-side question card (options + free text).
  const [question, setQuestion] = useState<{
    questionId: string;
    text: string;
    options: string[];
  } | null>(null);
  const [errorMsg, setErrorMsg] = useState<string | null>(null);

  const showError = useCallback((msg: string) => {
    setErrorMsg(msg);
    setTimeout(() => setErrorMsg(null), 4000);
  }, []);

  // On mount, load the deployment default as the initial staged mode for new
 // sessions (binding: the mode locks per session at creation; the global
  // settings only pick the default for NEW sessions).
  const [pendingMode, setPendingMode] = useState<string | null>(null);
  useEffect(() => {
    window.starriver.getAgentMode?.()
      .then((m) => { setPendingMode(m && m !== 'standard' ? m : null); setMode(m && m !== 'standard' ? m : null); })
      .catch((e) => console.error('[Coding] getAgentMode failed:', e));
  }, []);

 // Mode selection (binding): only a fresh session (nothing sent yet) can
  // change it — the staged value rides the first execute and the host stamps it
  // as locked; once the session starts even slash commands cannot change it.
  const handleModeChange = useCallback(async (newMode: string | null) => {
    if (sessionIdRef2.current || activeSessionId) {
      showError(t('coding.modeLocked'));
      return;
    }
    setPendingMode(newMode);
    setMode(newMode);
    // Staged phase must track the staged mode (only Plan has a planning phase).
    setPlanPhase(newMode === 'plan' ? 'planning' : 'executing');
  }, [activeSessionId, showError, t]);

 // WI-5 (ui-agent-preset parity): staged agent preset for NEW sessions
  // the roster loads once, the staged id rides the first execute, the host
  // locks it into the session header. Null = built-in coding composition.
  const [agentPresets, setAgentPresets] = useState<{ id: string; agent: string; name: string; description: string; broken: string | null }[]>([]);
  const [pendingPreset, setPendingPreset] = useState<string | null>(null);
  const [activePresetLabel, setActivePresetLabel] = useState<string | null>(null);
  // New Task mode switch: fixed Coding / Chat slots. Chat runs the built-in
  // Chat Agent with no workspace / execution-mode / approval chrome — the
  // host binds workspaces per agent, so those sessions land ungrouped exactly
  // like the Chat tab's. The staged preset stays the roster default (coding).
  const [newTaskMode, setNewTaskMode] = useState<'coding' | 'chat'>('coding');
  const stagedChat = newTaskMode === 'chat';
  // New Task screen state (drives the top-stacked layout): hero + composer
  // stack from the top, none of the session chrome is mounted.
  const isNewTask = messages.length === 0 && !activeSessionId;
  // The agent a session started with stays its agent until the conversation
  // ends — flipping the switch mid-session must not swap the runner mid-flight.
  const sessionAgentRef = useRef<string>(CODING_AGENT_NAME);
  useEffect(() => {
    window.starriver.listAgentPresets?.()
      .then((roster: any) => {
        const rows = Array.isArray(roster?.presets) ? roster.presets : [];
        setAgentPresets(rows);
        const def = rows.find((r: any) => r.isDefault) ?? rows[0];
        if (def) setPendingPreset(def.id);
      })
      .catch((e) => console.error('[Coding] listAgentPresets failed:', e));
  }, []);

  // Session sync: the sidebar (WorkspaceProvider) owns which session is open —
  // mirror that decision into the local message stream here. Empty id closes
  // the stream. This also restores the active conversation when the view
  // remounts after a tab switch.
  useEffect(() => {
    if (activeSessionId === sessionIdRef2.current) return;
    sessionIdRef2.current = activeSessionId;
    // The stop/approval ref must track the OPEN session too: it only filled
    // from live TaskStarted/sessionStarted events, so after a restart / tab
    // switch / session remount Stop resolved to "" — the click armed the
    // 20s watchdog and NEVER sent a cancel (host log for session
    // e3d6121b on 2026-10-07: zero `cancel requested` all day while the
    // user pressed 结束 twice).
    sessionIdRef.current = activeSessionId;
    // BUG-92: view state is per-session — reset it on EVERY switch (A→B
    // included), not only when leaving for the empty/new-task state. The
    // declaration above promises "reset on session switch"; the old code
    // only reset on the two empty-id branches, so usage/goal/doctor/plan
    // from session A leaked into session B's view.
    setSessionUsage({ in: 0, out: 0 });
    setGoalInfo(null);
    setDoctorReport(null);
    setLastPlan(null);
    setPlanPanelOpen(false);
    setPlanApproved(false);
    // Same leak family, found in the 2026-10-07 sweep: the previous
    // session's run state, pending approval/question cards, turn meta and
    // the todo watermark all survived an A→B switch.
    setSending(false);
    sendingRef.current = false;
    setTodoHideUpTo(0);
    setApproval(null);
    setQuestion(null);
    setTurnUsage(null);
    setTurnSummary(null);
    stopControl.reset();
    if (!activeSessionId) {
      setMessages([]);
      messagesRef.current = [];
      setStaleBusy(false);
      setQueue([]);
      return;
    }
    // BUG-96: re-arm the busy/Stop affordance from the backend's single
    // input slot — the renderer's own run state dies on reload while the
    // turn task (and the slot) lives on. This is ALSO what keeps the
    // composer honest per OPEN session: `sending` is reset above (it
    // belonged to session A's execute), so an idle session B must show
    // 发送 — the button keys on `sending || staleBusy`, and only the
    // backend slot registry knows whether B itself is running. staleBusy
    // deliberately keeps the composer in SEND mode (BUG-96/141 contract);
    // it only swaps the button to 停止.
    window.starriver.listRunningSessions?.().then((ids: string[]) => {
      setStaleBusy(Array.isArray(ids) && ids.includes(activeSessionId));
    }).catch(() => {});
    refreshQueueRef.current(activeSessionId);
    void loadSessionMessages(activeSessionId);
  }, [activeSessionId, loadSessionMessages]);

  // Sidebar "+ New conversation": reset the composer for a fresh run. The
  // provider already activated the workspace and cleared isolation host-side;
  // this is the view-local part (stream, task flag, staged mode, focus).
  const lastConvSeqRef = useRef(newConvSeq);
  useEffect(() => {
    if (newConvSeq === lastConvSeqRef.current) return;
    lastConvSeqRef.current = newConvSeq;
    setMessages([]);
    messagesRef.current = [];
    setTodoHideUpTo(0);
    setQueue([]);
    setSessionUsage({ in: 0, out: 0 });
    sessionIdRef2.current = '';
    sessionAgentRef.current = CODING_AGENT_NAME;
    setTaskStarted(false);
    setIsolated(false);
    setMode(pendingMode && pendingMode !== 'standard' ? pendingMode : null);
    // Fresh-task phase follows the staged mode: a staged Plan session starts
    // in its planning phase (approval picker locked until the review passes).
    setPlanPhase(pendingMode === 'plan' ? 'planning' : 'executing');
    // Back to the New Task screen: the chat view (and its control row) must
    // return even if the user left the session on the trajectory view.
    setView('chat');
    requestAnimationFrame(() => {
      requestAnimationFrame(() => {
        inputRef.current?.focus();
      });
    });
  }, [newConvSeq, pendingMode]);

  // Workspace activation side effects: fresh-task flag + instructions banner
  // preview (the same file set as the agent loop's agent-instructions chain).
  // Git status follows via refreshGitStatus's own effect.
  useEffect(() => {
    setTaskStarted(false);
    if (!activeWorkspaceId) { setWorkspaceInstructions(null); return; }
    let cancelled = false;
    Promise.resolve(window.starriver.readWorkspaceInstructionsMeta?.(activeWorkspaceId))
      .then((v) => {
        if (!cancelled) {
          setWorkspaceInstructions(v?.content ?? null);
          setInstructionsTruncated(!!v?.truncated);
        }
      })
      .catch(() => { if (!cancelled) setWorkspaceInstructions(null); });
    return () => { cancelled = true; };
  }, [activeWorkspaceId]);

  // FPM P4b: after a daemon crash-restart, main broadcasts connected — the
  // provider refreshes the workspace/session snapshots; refresh git status here.
  useEffect(() => {
    const off = (window.starriver as any).onDaemonStatus?.((status: any) => {
      if (status?.state === 'connected') {
        refreshGitStatus();
      }
    });
    return () => off?.();
  }, [refreshGitStatus]);

  useEffect(() => { messagesRef.current = messages; }, [messages]);

  useEffect(() => {
    if (!sending) {
      let raf1: number;
      let raf2: number;
      raf1 = requestAnimationFrame(() => {
        raf2 = requestAnimationFrame(() => {
          inputRef.current?.focus();
        });
      });
      return () => {
        cancelAnimationFrame(raf1);
        cancelAnimationFrame(raf2);
      };
    }
  }, [sending]);

  // Model selector state
  const [activeModelId, setActiveModelId] = useState<string>('');
  const [bindings, setBindings] = useState<AgentModelBinding[]>([]);
  /** Agent-scoped reasoning effort ('low'|'high'|'max'; default High). */
  const [reasoningEffort, setReasoningEffort] = useState<string>('high');
  // New Task chat mode: the composer's selects write the CHAT agent's own
  // binding — Chat and Coding keep independent models, so the staged mode
  // decides which agent the selects act on.
  const [chatModelId, setChatModelId] = useState<string>('');
  const [chatReasoningEffort, setChatReasoningEffort] = useState<string>('high');
  // null → Ungrouped (the Chat Agent keeps no workspace binding).
  const [chatWorkspaceId, setChatWorkspaceId] = useState<string | null>(null);

  // WI-152 S2: resolve the image gate from the staged agent's active route.
  // The resolved `visionSupported` (declaration > built-in table) decides;
  // an unknown provider defaults to permissive (backend strips if wrong).
  const activeModelKey = stagedChat ? chatModelId : activeModelId;
  useEffect(() => {
    const disabled = !imagesAcceptedForRoute(models, activeModelKey);
    imagesDisabledRef.current = disabled;
    setImagesDisabled(disabled);
  }, [activeModelKey, models]);

  const refreshModels = useCallback(async () => {
    try {
      const [allModels, agentBindings] = await Promise.all([
        window.starriver.listModels(),
        window.starriver.listAgentModelBindings(),
      ]);
      setModels(allModels);
      setBindings(agentBindings);
      const cb = agentBindings.find(b => b.agentName === CODING_AGENT_NAME);
      const firstModel = cb?.activeModel
        || (cb?.boundModels.length ? cb.boundModels[0].id : '');
      if (firstModel) {
        setActiveModelId(firstModel);
        // Engine activation happens server-side in switch_agent (per-agent model),
        // so Chat and Coding each keep their own model without a shared override.
        console.log('[Coding] Active model:', firstModel);
      }
      // The composer no longer offers Default/Off; migrate any legacy stored
      // value ('' = provider default, 'off') to High so the engine matches
      // what the dropdown shows.
      const syncEffort = (agentName: string, stored?: string | null) => {
        const effort = normalizeReasoningEffort(stored);
        if (effort === (stored ?? '')) return effort;
        try {
          const r = window.starriver.setAgentReasoningEffort(agentName, effort);
          if (r && typeof r.catch === 'function') {
            r.catch((e: unknown) => console.error('[Coding] Reasoning migration failed:', e));
          }
        } catch (e) {
          // A missing/throwing bridge must not abort the rest of refreshModels.
          console.error('[Coding] Reasoning migration failed:', e);
        }
        return effort;
      };
      setReasoningEffort(syncEffort(CODING_AGENT_NAME, cb?.reasoningEffort));
      const chatB = agentBindings.find(b => b.agentName === CHAT_AGENT_NAME);
      const chatFirst = chatB?.activeModel
        || (chatB?.boundModels.length ? chatB.boundModels[0].id : '');
      if (chatFirst) setChatModelId(chatFirst);
      setChatReasoningEffort(syncEffort(CHAT_AGENT_NAME, chatB?.reasoningEffort));
    } catch (e) { console.error(e); }
  }, []);

  useEffect(() => {
    let cancelled = false;
    // No tab-entry switch any more: CodingView's executes name the coding
    // agent explicitly and the host resolves config (workspace activation
    // included) from the per-agent durable state.
    const restoreWorkspace = async () => {
      try {
        const [id, chatId, list] = await Promise.all([
          Promise.resolve(window.starriver.getActiveWorkspace?.(CODING_AGENT_NAME)),
          Promise.resolve(window.starriver.getActiveWorkspace?.(CHAT_AGENT_NAME)),
          window.starriver.listWorkspaces(),
        ]);
        if (cancelled) return;
        if (id && list.some(w => w.id === id)) setActiveWorkspaceId(id);
        if (chatId && list.some(w => w.id === chatId)) setChatWorkspaceId(chatId);
      } catch { /* a failed restore must not block the Coding view */ }
    };
    restoreWorkspace();
    refreshModels();
    return () => { cancelled = true; };
  }, []);

  const handleModelChange = async (key: string) => {
    const prev = activeModelId;
    setActiveModelId(key);
    try {
      const [providerId, variantId] = key.split('/');
      await window.starriver.setAgentActiveModel(CODING_AGENT_NAME, providerId, variantId || providerId);
      console.log('[Coding] Model switched to:', providerId, variantId);
    } catch (e) {
      // BUG-92: roll the optimistic pick back — the selector and /status
      // must not advertise a model the engine never activated.
      if (prev && prev !== key) setActiveModelId(prev);
      console.error('[Coding] Model switch failed:', e);
    }
  };

  const handleReasoningChange = async (effort: string) => {
    setReasoningEffort(effort);
    try {
      await window.starriver.setAgentReasoningEffort(CODING_AGENT_NAME, effort);
      console.log('[Coding] Reasoning effort set to:', effort || '(provider default)');
    } catch (e) { console.error('[Coding] Reasoning switch failed:', e); }
  };

  const handleChatModelChange = async (key: string) => {
    const prev = chatModelId;
    setChatModelId(key);
    try {
      const [providerId, variantId] = key.split('/');
      await window.starriver.setAgentActiveModel(CHAT_AGENT_NAME, providerId, variantId || providerId);
      console.log('[Coding] Chat model switched to:', providerId, variantId);
    } catch (e) {
      if (prev && prev !== key) setChatModelId(prev);
      console.error('[Coding] Chat model switch failed:', e);
    }
  };

  const handleChatReasoningChange = async (effort: string) => {
    setChatReasoningEffort(effort);
    try {
      await window.starriver.setAgentReasoningEffort(CHAT_AGENT_NAME, effort);
      console.log('[Coding] Chat reasoning effort set to:', effort || '(provider default)');
    } catch (e) { console.error('[Coding] Chat reasoning switch failed:', e); }
  };

  // Chat workspace binding (New Task Chat): pick an existing workspace for the
  // Chat Agent, or none to keep its sessions ungrouped. Written through the
  // per-agent host binding only — the coding activation/sidebar highlight is
  // a different agent's state and stays untouched.
  const handleChatWorkspaceChange = async (id: string) => {
    // '' is an explicit "not in a workspace" choice; null (never picked) only
    // shows the placeholder. Both clear the Chat Agent's workspace binding.
    setChatWorkspaceId(id);
    try {
      await window.starriver.setActiveWorkspace(id || null, CHAT_AGENT_NAME);
    } catch (e) { console.error('[Coding] Chat workspace switch failed:', e); }
  };

  // Streaming-tail scroll stickiness (same contract as ChatTab): follow the
  // tail until the user scrolls away.
  const codingListRef = useRef<HTMLDivElement>(null);
  const codingFollowRef = useRef(true);
  useEffect(() => {
    if (codingFollowRef.current) bottomRef.current?.scrollIntoView({ block: 'end' });
  }, [messages]);

  const addMessage = useCallback((role: Message['role'], content: string, streaming?: boolean, toolName?: string, meta?: unknown) => {
    const msg: Message = { id: crypto.randomUUID(), role, content, streaming, toolName, ts: Date.now(), meta };
    setMessages((prev) => [...prev, msg]);
    return msg.id;
  }, []);

  // Provider-side actions (unarchive, session rename) drop their in-chat tool
  // toast through this bridge while the view is mounted.
  useEffect(() => {
    wsCtx.registerNotifier((text, kind) => addMessage('tool', text, false, kind));
    return () => wsCtx.registerNotifier(null);
  }, [wsCtx, addMessage]);

  // Fresh-conversation requests from the provider reset the view-local
  // isolation toggle (the /worktree switch is per session).
  useEffect(() => {
    wsCtx.isolationResetRef.current = () => setIsolated(false);
    return () => { wsCtx.isolationResetRef.current = null; };
  }, [wsCtx]);

  const updateMessage = useCallback((id: string, content: string, streaming?: boolean) => {
    setMessages((prev) => prev.map((m) => (m.id === id ? { ...m, content, streaming } : m)));
  }, []);

  /// /backtrack: open the backtrack picker, listing the current session's user messages.
  const openBacktrack = useCallback(async () => {
    const sid = sessionIdRef2.current;
    if (!sid) {
      addMessage('tool', '⚠ ' + t('thread.noActiveSession'), false, 'backtrack');
      return;
    }
    setBacktrackLoading(true);
    try {
      const tl = await window.starriver.getSessionTimeline(sid);
      const users = tl.filter(m => m.role === 'user');
      if (users.length === 0) {
        addMessage('tool', '⚠ ' + t('thread.noUserMessages'), false, 'backtrack');
        setBacktrackOpen(false);
        return;
      }
      setBacktrackMessages(users);
      setBacktrackOpen(true);
    } catch (e) { showError(t('thread.backtrackFailed')); }
    finally { setBacktrackLoading(false); }
  }, [addMessage, showError, t]);

  /// Select a user message → fork a new thread and switch to it (truncation point = before that message).
  const handleBacktrackSelect = useCallback(async (msg: SessionMessage) => {
    const sid = sessionIdRef2.current;
    if (!sid) return;
    setBacktrackOpen(false);
    try {
      const info = await window.starriver.forkSession(sid, msg.id, msg.content);
      const msgs = await window.starriver.getSessionMessages(info.id);
      const loaded = msgs.map(m => ({
        id: crypto.randomUUID(), role: m.role as Message['role'], content: m.content,
      }));
      setMessages(loaded);
      messagesRef.current = loaded;
      setTodoHideUpTo(0);
      setActiveSessionId(info.id);
      sessionIdRef2.current = info.id;
      setInput(msg.content); // prefill the backtrack point's text for editing + resubmit
      addMessage('tool', `↪ ${t('thread.forked', { id: info.id })}`, false, 'fork');
      await loadSessions();
    } catch (e) { showError(t('thread.forkFailed')); }
  }, [addMessage, showError, loadSessions, t]);

  /// /delete: execute deletion after the second confirmation.
  const confirmSessionDelete = useCallback(async (sessionId: string) => {
    setConfirmDeleteSession(null);
    try {
      await window.starriver.deleteSession(sessionId);
      if (sessionIdRef2.current === sessionId) {
        setMessages([]); messagesRef.current = [];
        setActiveSessionId(''); sessionIdRef2.current = '';
      }
      addMessage('tool', `🗑 ${t('thread.deleted')}`, false, 'delete');
      await loadSessions();
    } catch (e) { showError(t('thread.deleteFailed')); }
  }, [addMessage, showError, loadSessions, t]);

  // /unarchive + /name flows moved to WorkspaceProvider (the sidebar rows own
  // them now); this view keeps the /delete confirmation via confirmSessionDelete.

  // Task stop control: end-button state + cancel wiring (hook shared with ChatTab).
  // Third arg: the open-session ref as fallback — the primary only fills from
  // live TaskStarted events, so after restart/remount Stop would otherwise be
  // a 20s no-op that never sends the cancel (2026-10-07 session e3d6121b).
  // Fourth arg: cancel resolved false — the backend has no live turn for this
  // session, so the composer's Stop state is stale (the turn's terminal event
  // never reached this view). Drop the run state and flip back to Send
  // instead of wedging until the user clicks 新建 task (2026-10-12 report).
  const stopControl = useTaskStop(sessionIdRef, useCallback((text: string) => {
    addMessage('tool', text, false);
  }, [addMessage]), sessionIdRef2, useCallback(() => {
    sendingRef.current = false;
    setSending(false);
    setStaleBusy(false);
    addMessage('tool', t('common.turnAlreadyEnded'), false);
  }, [addMessage, t]));

  // BUG-96: recover a session whose previous turn still holds the single
  // input slot (renderer lost its run state or the turn hung). Cancel the
  // stale task, then poll the backend's slot registry until it releases —
  // the terminal event cannot arrive here because this renderer has no live
  // stream for that turn.
  const stopStaleTurn = useCallback(() => {
    const sid = sessionIdRef.current || sessionIdRef2.current;
    if (!sid) return;
    if (!confirm(t('common.confirmStop'))) return;
    (async () => {
      try {
        const ok = await window.starriver.cancel(sid);
        if (!ok) {
          // Nothing was running host-side: the stale-busy marker is stale by
          // definition (BUG-141 contract — slot proven free). Any latched
          // sending state is stale with it — flip the composer back to Send.
          sendingRef.current = false;
          setSending(false);
          setStaleBusy(false);
          return;
        }
        for (let i = 0; i < 20; i++) {
          const ids: string[] = await window.starriver.listRunningSessions().catch(() => [] as string[]);
          if (!Array.isArray(ids) || !ids.includes(sid)) {
            setStaleBusy(false);
            addMessage('tool', t('common.staleBusyStopped'), false);
            return;
          }
          await new Promise((r) => setTimeout(r, 500));
        }
        // Still held after 10s: leave the busy affordance up.
      } catch { setStaleBusy(false); }
    })();
  }, [t, addMessage]);

  // rAF-based debounce for streaming text updates (prevents UI freeze on long output)
  const rafRef = useRef<number>(0);
  const pendingTextRef = useRef<string>('');
  const flushTextDelta = useCallback((assistantId: string) => {
    if (rafRef.current) return; // rAF already scheduled
    rafRef.current = requestAnimationFrame(() => {
      rafRef.current = 0;
      updateMessage(assistantId, pendingTextRef.current, true);
    });
  }, [updateMessage]);

  // Streaming reasoning: deltas coalesce in a buffer, flushed per animation
  // frame into the (created-on-demand) thinking bubble.
  const flushThinkingDelta = useCallback(() => {
    thinkingRafRef.current = 0;
    const text = thinkingBufRef.current;
    if (!text) return;
    thinkingBufRef.current = '';
    thinkingViaDeltaRef.current = true;
    if (!thinkingIdRef.current) {
      // The rAF flush can land right after the step's prose started streaming:
      // if the assistant placeholder already holds text, hoist the thinking
      // message BEFORE it (reasoning logically precedes the prose), or the
      // step grouping would misfile this thinking into the next step.
      //
      // But when the placeholder is still EMPTY (pure-tool step: no prose until
      // the turn's final message), inserting before it would hoist EVERY later
      // thinking row above all the tool rows that already appended — the fold
      // then showed the whole turn's thinking first, tools last, out of order.
      // An empty placeholder means nothing streamed yet → append in place so
      // the messages array stays chronological.
      const placeholderId = assistantIdRef.current;
      const placeholderHasProse = !!placeholderId && !!(pendingTextRef.current || '').trim();
      if (placeholderId && placeholderHasProse) {
        const msg: Message = { id: crypto.randomUUID(), role: 'thinking', content: text, streaming: true, ts: Date.now() };
        thinkingIdRef.current = msg.id;
        setMessages(prev => {
          const idx = prev.findIndex(m => m.id === placeholderId);
          return idx === -1 ? [...prev, msg] : [...prev.slice(0, idx), msg, ...prev.slice(idx)];
        });
      } else {
        thinkingIdRef.current = addMessage('thinking', text, true);
      }
    } else {
      setMessages(prev => prev.map(m =>
        m.id === thinkingIdRef.current ? { ...m, content: (m.content || '') + text } : m
      ));
    }
  }, [addMessage]);

  const closeThinkingBubble = useCallback(() => {
    if (thinkingRafRef.current) {
      cancelAnimationFrame(thinkingRafRef.current);
      thinkingRafRef.current = 0;
    }
    flushThinkingDelta();
    if (thinkingIdRef.current) {
      // Capture the id BEFORE the updater: React runs the updater after this
      // synchronous block, by which time the ref is already nulled — reading
      // .current inside would never match and the row would stream forever.
      const tid = thinkingIdRef.current;
      setMessages(prev => prev.map(m =>
        m.id === tid ? { ...m, streaming: false } : m
      ));
      thinkingIdRef.current = null;
    }
    thinkingViaDeltaRef.current = false;
  }, [flushThinkingDelta]);

  const enqueuePrompt = useCallback((text: string) => {
    const sid = sessionIdRef2.current;
    setInput('');
    const fallback = () => {
      queueSeqRef.current += 1;
      setQueue([...queueItemsRef.current, { id: `local-${queueSeqRef.current}`, text }]);
    };
    if (!sid) {
      fallback();
      return;
    }
    window.starriver.queueAdd?.(sid, text).then((id: string | null) => {
      if (id) setQueue([...queueItemsRef.current, { id, text }]);
      else fallback();
    }).catch(fallback);
  }, []);

  const removeQueued = useCallback((id: string) => {
    setQueue(queueItemsRef.current.filter(q => q.id !== id));
    const sid = sessionIdRef2.current;
    if (sid && !id.startsWith('local-')) window.starriver.queueRemove?.(sid, id).catch(() => {});
  }, []);

  const saveQueueEdit = useCallback((id: string) => {
    const text = queueEditDraft.trim();
    setEditingQueueId(null);
    if (!text) return;
    setQueue(queueItemsRef.current.map(q => q.id === id ? { ...q, text } : q));
    const sid = sessionIdRef2.current;
    if (sid && !id.startsWith('local-')) {
      window.starriver.queueUpdate?.(sid, id, text).catch(() => {});
    }
  }, [queueEditDraft]);

  const sendQueuedNow = useCallback(() => {
    const sid = sessionIdRef2.current;
    if (!sid) return;
    // Send Now is ONE atomic host call: cancel FIRST (abort the live turn,
    // park), flush SECOND (unpark + wake) — the parked flag must END false
    // or the driver never claims the queued message. The old renderer
    // choreography (queueFlush → cancel across two IPC calls) let the
    // trailing cancel re-park the queue right after the flush: the queued
    // message was never claimed, nothing visibly happened, and with a live
    // turn the host round loop wedged forever waiting for a follow-up
    // terminal that never came (session input slot leaked until restart).
    const now = window.starriver.queueFlushNow?.(sid);
    if (now && typeof now.then === 'function') {
      now.then((flushed: boolean) => {
        // false = no live agent (fresh app with a durable parked queue):
        // resync the dock; the queue stays durable and runs with the
        // session's next execute.
        if (!flushed) refreshQueueRef.current(sid);
      }).catch(() => refreshQueueRef.current(sid));
      return;
    }
    // Legacy host without session.queueFlushNow: cancel then flush — the
    // final state must be UNPARKED, so the flush has to come last.
    const go = () => {
      const p = window.starriver.queueFlush?.(sid);
      if (p && typeof p.catch === 'function') p.catch(() => {});
    };
    const c = window.starriver.cancel?.(sid);
    if (c && typeof c.then === 'function') c.then(go, go); else go();
  }, []);

  const refreshQueue = useCallback((sid: string) => {
    window.starriver.queueList?.(sid).then((items: { id: string; text: string }[]) => {
      if (sessionIdRef2.current !== sid) return;
      setQueue(Array.isArray(items) ? items : []);
    }).catch(() => {});
  }, []);
  refreshQueueRef.current = refreshQueue;

  const send = useCallback(async () => {
    const text = input.trim();
    if (!text && attachments.length === 0) return;
    // WI-152 S2 send guard: a text-only route must never ship images (the
    // composer gate already blocks admission; this catches stale pending
    // attachments after a model switch).
    if (imagesDisabledRef.current && attachments.some(a => a.kind === 'image')) {
      rejectImages();
      return;
    }
    if (sendingRef.current) {
      // Running: queue the composed text (slash commands included — they run
      // through the same path when drained). Attachments ride the SAME
      // admission seam as a direct send: save them first and inline the
      // resulting refs into the queued text. Queueing the raw text alone
      // silently dropped every attached image, and worse left the strip
      // armed so the stale files re-attached to the NEXT send.
      const { imageRefs, docLinks, docTexts } = await saveAttachments(attachments);
      // The strip is a staging area: once the refs live in the queued text,
      // clear it (same contract as the direct-send path below).
      setAttachments([]);
      enqueuePrompt(composeWithAttachments(text, { imageRefs, docLinks, docTexts }));
      return;
    }
    setInput('');
    // WI-88: "resend from here" — fork before this user message, then run
    // the (possibly edited) text on the fresh thread.
    if (resendFromRef.current != null && sessionIdRef2.current) {
      const forkSeq = resendFromRef.current;
      resendFromRef.current = null;
      try {
        const forked = await window.starriver.forkSession(sessionIdRef2.current, forkSeq, text);
        await loadSessions();
        setActiveSessionId(forked.id);
        sessionIdRef2.current = forked.id;
        sessionIdRef.current = forked.id;
      } catch (e) {
        console.error('[Coding] fork for resend failed:', e);
      }
    }
    setSending(true);
    // Immediate ref write (the render-time assignment re-affirms it): the
    // stream's first events must pass the idle gate before React re-renders.
    sendingRef.current = true;
    stopControl.onTerminal();

    // Save pending attachments and build references: images are admitted
    // through the backend attachment seam (sha256 refs → markdown img refs),
    // binary documents → plain path links, text documents → extracted text.
    let imageRefs = '';
    let docLinks = '';
    let docTexts = '';
    if (attachments.length > 0) {
      const { saved, imageRefs: refs, docLinks: links, docTexts: texts } =
        await saveAttachments(attachments);
      // The strip is a composer staging area: once the message goes out the
      // thumbnails are cleared (refs live in the sent message content).
      setAttachments([]);
      imageRefs = refs;
      docLinks = links;
      docTexts = texts;
      void saved;
    }

    // Thumbnails ride the bubble ABOVE the text: image refs first, then the
    // typed message — UserMessageContent renders refs as thumbnails.
    const userDisplay = [imageRefs, text].filter(Boolean).join('\n');
    codingFollowRef.current = true; // a fresh send re-engages tail following
    addMessage('user', userDisplay);
    // Invocation marker: if the task starts with a skill slash command, show it.
    // Skill `command` values are slash-less — strip the leading '/' or the
    // ⚡ marker never matches.
    const firstTok = (text.split(/\s+/)[0] || '').replace(/^\//, '');
    const skillCmd = skills.find(s => s.command === firstTok);
    if (skillCmd) {
      addMessage('tool', `⚡ invoked skill: ${skillCmd.name}`, false, skillCmd.name);
    }
    // One assistant message per loop turn (a tool-calling run produces multiple turns).
    // TextDone marks a turn boundary so the next TextDelta starts a fresh bubble.
    let assistantId = addMessage('assistant', '', true);
    assistantIdRef.current = assistantId;
    let outputBuffer = '';
    let turnEnded = false;
    pendingTextRef.current = '';
    // Fresh turn: drop the previous turn's file-change summary and diff buffer.
    turnDiffsRef.current = new Map();
    setTurnSummary(null);
    turnStartRef.current = Date.now();
    turnTokensRef.current = { input: 0, output: 0 };
    setTurnUsage(null);
    lastAssistantIdRef.current = null;

    // Handle slash commands
    let taskText = text;

    // Client-side commands (intercepted before LLM):
    if (text === '/compact' || text.startsWith('/compact ')) {
      const sid = sessionIdRef2.current;
      const parsed = text.startsWith('/compact ') ? parseInt(text.slice(9).trim(), 10) : NaN;
      const keep = Number.isFinite(parsed) ? parsed : 10;
      if (sid) {
        try {
          const result = await window.starriver.compactSession(sid, keep);
          setMessages(prev => [...prev, { id: crypto.randomUUID(), role: 'tool' as const, content: `📦 Compacted: ~${result.before} → ~${result.after} tokens (keeping ${keep} recent messages)` }]);
        } catch (e: any) {
          setMessages(prev => [...prev, { id: crypto.randomUUID(), role: 'tool' as const, content: `❌ Compaction failed: ${e?.message || e}` }]);
        }
      } else {
        addMessage('tool', '📦 No active session — start a task first', false, 'compact');
      }
      setSending(false);
      return;
    }
    if (text === '/status' || text.startsWith('/status ')) {
      const sessInfo = [`Session: ${sessionIdRef2.current || '(new)'}`];
      if (activeModelId) sessInfo.push(`Model: ${activeModelId}`);
      sessInfo.push(`Mode: ${mode ?? 'Auto'}`);
      sessInfo.push(`Messages: ${messagesRef.current.length} (includes tool/thinking rows)`);
      addMessage('tool', `📊 ${sessInfo.join(' · ')}`, false, 'status');
      setSending(false);
      return;
    }
    if (text === '/diff' || text.startsWith('/diff ')) {
      const ws = workspaces.find(w => w.id === activeWorkspaceId);
      if (!ws?.path) {
        addMessage('tool', '📄 No workspace directory — activate a workspace first', false, 'diff');
      } else {
        try {
          const diff = await window.starriver.workspaceDiff(ws.path);
          addMessage('tool', diff.trim() ? `📄 Workspace diff:\n\`\`\`diff\n${diff}\n\`\`\`` : '📄 No changes in workspace', false, 'diff');
        } catch (e: any) {
          addMessage('tool', `📄 Diff failed: ${e?.message || e}`, false, 'diff');
        }
      }
      setSending(false);
      return;
    }
    if (text === '/usage' || text.startsWith('/usage ')) {
      const sid = sessionIdRef2.current;
      if (!sid) {
        addMessage('tool', '📊 No active session — start a task first', false, 'usage');
      } else {
        try {
          const u = await window.starriver.sessionUsage(sid);
          addMessage('tool', `📊 Session ${sid.slice(0, 8)} · ${u.tokens} tokens · ${u.count} messages`, false, 'usage');
        } catch (e: any) {
          addMessage('tool', `📊 Usage failed: ${e?.message || e}`, false, 'usage');
        }
      }
      setSending(false);
      return;
    }
    if (text === '/init' || text.startsWith('/init ')) {
      const ws = workspaces.find(w => w.id === activeWorkspaceId);
      if (!ws?.path) {
        addMessage('tool', '📄 No workspace directory — activate a workspace first', false, 'init');
      } else {
        try {
          const path = await window.starriver.initProjectAgents(ws.path);
          addMessage('tool', `📄 Project doc — ${path}`, false, 'init');
        } catch (e: any) {
          addMessage('tool', `📄 Init failed: ${e?.message || e}`, false, 'init');
        }
      }
      setSending(false);
      return;
    }
    if (text === '/export' || text.startsWith('/export ')) {
      const sid = sessionIdRef2.current;
      const ws = workspaces.find(w => w.id === activeWorkspaceId);
      if (!sid || !ws?.path) {
        addMessage('tool', '📄 No active session + workspace — start a task and activate a workspace first', false, 'export');
      } else {
        try {
          const path = await window.starriver.exportSession(sid, ws.path);
          addMessage('tool', `📄 Exported — ${path}`, false, 'export');
        } catch (e: any) {
          addMessage('tool', `📄 Export failed: ${e?.message || e}`, false, 'export');
        }
      }
      setSending(false);
      return;
    }
    if (text === '/lang') {
      const next = lang === 'zh' ? 'en' : 'zh';
      setLang(next);
      addMessage('tool', `🌐 Language switched to ${next === 'zh' ? '中文' : 'English'}`, false, 'lang');
      setSending(false);
      return;
    }
    if (text === '/worktree') {
      // Local per-session toggle: carried on the next execute of this view.
      const next = !isolated;
      setIsolated(next);
      addMessage('tool', `⑂ worktree isolation: ${next ? 'ON' : 'OFF'}`, false, 'worktree');
      setSending(false);
      return;
    }
    if (text === '/backtrack') {
      await openBacktrack();
      setSending(false);
      return;
    }
    if (text === '/delete') {
      const sid = sessionIdRef2.current;
      if (!sid) {
        addMessage('tool', '⚠ ' + t('thread.noActiveSession'), false, 'delete');
      } else {
        setConfirmDeleteSession(sid);
      }
      setSending(false);
      return;
    }
    if (text === '/name' || text.startsWith('/name ')) {
      const sid = sessionIdRef2.current;
      const title = text.slice(6).trim();
      if (!sid) {
        addMessage('tool', '⚠ ' + t('thread.noActiveSession'), false, 'name');
      } else if (!title) {
        addMessage('tool', '✏ ' + t('thread.nameUsage'), false, 'name');
      } else {
        try {
          await window.starriver.renameSession(sid, title);
          addMessage('tool', `✏ ${t('thread.renamed')}`, false, 'name');
          await loadSessions();
        } catch (e: any) {
          addMessage('tool', `✏ ${t('thread.renameFailed')}: ${e?.message || e}`, false, 'name');
        }
      }
      setSending(false);
      return;
    }
    if (text === '/archive') {
      const sid = sessionIdRef2.current;
      if (!sid) {
        addMessage('tool', '⚠ ' + t('thread.noActiveSession'), false, 'archive');
      } else {
        try {
          await window.starriver.archiveSession(sid);
          if (sessionIdRef2.current === sid) {
            setMessages([]); messagesRef.current = [];
            setActiveSessionId(''); sessionIdRef2.current = '';
          }
          addMessage('tool', `📦 ${t('thread.archived')}`, false, 'archive');
          await loadSessions();
        } catch (e: any) {
          addMessage('tool', `📦 ${t('thread.archiveFailed')}: ${e?.message || e}`, false, 'archive');
        }
      }
      setSending(false);
      return;
    }
    if (text === '/unarchive') {
      try {
        await loadSessions();
        setArchiveOpen(true);
      } catch (e: any) {
        addMessage('tool', `📦 ${t('thread.unarchiveFailed')}: ${e?.message || e}`, false, 'unarchive');
      }
      setSending(false);
      return;
    }
    if (text === '/help') {
      const cmdList = buildSlashItems('')
        .filter(i => i.group === 'coding')
        .map(i => `${i.cmd} — ${i.desc}`)
        .join('\n');
      addMessage('tool', `ℹ️ ${t('thread.helpTitle')}:\n${cmdList}`, false, 'help');
      setSending(false);
      return;
    }
    if (text.startsWith('/review ')) {
      // WI-96: structured targets first (`uncommitted` / `branch <name>` /
      // `commit <sha>`), then the legacy quoted file list.
      const rest = text.slice(8).trim();
      const cwd = workspaces.find(w => w.id === activeWorkspaceId)?.path;
      let target: { kind: string; branch?: string; sha?: string; files?: string[] } | null = null;
      if (rest === 'uncommitted' || rest === 'uncommitted changes') {
        target = { kind: 'uncommitted' };
      } else if (rest.startsWith('branch ')) {
        target = { kind: 'baseBranch', branch: rest.slice(7).trim() };
      } else if (rest.startsWith('commit ')) {
        target = { kind: 'commit', sha: rest.slice(7).trim() };
      }
      if (target) {
        // Structured review: child-thread run, findings card, no turn.
        addMessage('tool', `🔍 ${t('coding.reviewRunning')}`, true, 'review', { reviewRun: true });
        try {
          const result = await window.starriver.runReview?.(target, [], cwd);
          setMessages(prev => prev.map(m =>
            m.streaming && m.meta != null && (m.meta as { reviewRun?: boolean }).reviewRun === true
              ? { ...m, streaming: false, content: `🔍 ${t('coding.reviewDone', { n: result?.findings?.length ?? 0 })}`, result: result?.raw, ok: true, meta: { findings: result?.findings } }
              : m
          ));
        } catch (e) {
          console.error('[Coding] runReview failed:', e);
          setMessages(prev => prev.map(m =>
            m.streaming && m.meta != null && (m.meta as { reviewRun?: boolean }).reviewRun === true
              ? { ...m, streaming: false, content: `🔍 ${t('coding.reviewFailed')}: ${e}`, ok: false }
              : m
          ));
        }
        setSending(false);
        return;
      }
      // Quoted paths survive spaces: "my file.ts" stays one entry.
      const fileList = rest.match(/"[^"]+"|\S+/g)?.map(x => x.replace(/^"|"$/g, '')).filter(f => f.length > 0) ?? [];
      if (fileList.length > 0) {
        // Note: display-only — startCodeReview already sets the backend review
        // prompt + read/grep whitelist. Calling setAgentMode('review') here would
        // clobber both back to the base prompt + full tools.
        try { await window.starriver.startCodeReview(JSON.stringify(fileList)); setMode('review'); }
        catch (e) { console.error('[Coding] startCodeReview failed:', e); }
        taskText = `Please review the following files: ${fileList.join(', ')}`;
      }
    } else if (text.startsWith('/goal')) {
      const rest = text.slice(5).trim();
      const sid = sessionIdRef2.current || sessionIdRef.current;
      if (rest === 'clear' || rest === 'stop') {
        if (sid) await window.starriver.clearGoal?.(sid).catch(console.error);
        setGoalInfo(null);
        addMessage('tool', `🎯 ${t('coding.goalCleared')}`, false, 'goal');
      } else if (rest === 'pause' || rest === 'resume') {
        // Pause/resume toggles the in-card state note (host status is
        // advisory; turns simply stop auto-continuing).
        setGoalInfo(prev => prev ? { ...prev, status: rest === 'pause' ? 'paused' : 'active' } : prev);
        addMessage('tool', `🎯 ${rest === 'pause' ? t('coding.goalPaused') : t('coding.goalResumed')}`, false, 'goal');
      } else if (rest && sid) {
        // Parse "objective | budget" or plain objective.
        const [obj, budgetStr] = rest.split('|').map(x => x.trim());
        const budget = budgetStr ? Number(budgetStr.replace(/[^0-9]/g, '')) || undefined : undefined;
        await window.starriver.setGoal?.(sid, obj, budget).catch(console.error);
        setGoalInfo({ objective: obj, tokenBudget: budget, tokensUsed: 0, status: 'active' });
        addMessage('tool', `🎯 ${t('coding.goalSet')}: ${obj}`, false, 'goal');
        taskText = `Work toward this goal autonomously: ${obj}. When fully achieved, state GOAL COMPLETE.`;
      } else {
        addMessage('tool', `🎯 ${goalInfo ? `${goalInfo.objective} (${goalInfo.status})` : t('coding.goalUsage')}`, false, 'goal');
      }
    } else if (text.startsWith('/doctor')) {
      addMessage('tool', `🩺 ${t('coding.doctorRunning')}`, true, 'doctor', { doctorRun: true });
      try {
        const result = await window.starriver.runDoctor?.();
        setDoctorReport(result?.checks ?? []);
        setMessages(prev => prev.map(m =>
          m.streaming && m.meta != null && (m.meta as { doctorRun?: boolean }).doctorRun === true
            ? { ...m, streaming: false, content: `🩺 ${t('coding.doctorDone', { n: result?.checks?.length ?? 0 })}`, ok: true, meta: { checks: result?.checks } }
            : m
        ));
      } catch (e) {
        setMessages(prev => prev.map(m =>
          m.streaming && m.meta != null && (m.meta as { doctorRun?: boolean }).doctorRun === true
            ? { ...m, streaming: false, content: `🩺 ${t('coding.doctorFailed')}: ${e}`, ok: false }
            : m
        ));
      }
      setSending(false);
      return;
    } else if (text.startsWith('/explore ')) {
      const query = text.slice(9).trim();
      if (query) {
        // /explore borrows Plan's read-only policy for one exploration and
        // restores the previous mode at turn end — plan must NOT persist as the
        // global default (every later new session would be read-only locked).
        // Write UI state directly (bypassing the session lock: this is a
        // turn-level override that does not touch the session's locked mode).
        exploreRestoreRef.current = mode ?? 'standard';
        // A rejected mode switch must not leave `sending` latched (the
        // composer locks forever — every other await here is wrapped).
        try {
          await window.starriver.setAgentMode('plan');
        } catch (err) {
          console.error('setAgentMode(plan) failed', err);
          setSending(false);
          return;
        }
        setMode('plan');
        // The exploration runs under Plan's planning phase: keep the approval
        // picker locked for its duration (restored at turn end, WI-144).
        setPlanPhase('planning');
        taskText = `Explore the codebase: ${query}. Use read and grep to understand the code. Do not modify any files.`;
      }
    } else if (text.startsWith('/test ')) {
      const target = text.slice(6).trim();
      if (target) {
        taskText = `Write unit tests for: ${target}. Generate comprehensive test cases covering normal cases, edge cases, and error handling.`;
      }
    }

    // Add image references / document links / extracted text to task text
    // (shared composition with the queued path — same payload either way).
    taskText = composeWithAttachments(taskText, { imageRefs, docLinks, docTexts });

    // History is structured-loaded and compacted by the backend execute() from the session store,
    // the frontend no longer assembles "Previous conversation:" text (same as the Chat section).

    // A fresh session carries the staged execution mode (the host stamps and
 // locks it — continuing sessions do not send it. The staged
    // agent preset (WI-5) follows the same creation-time binding shape. Chat
    // mode rides the Chat Agent exactly like the Chat tab: no execution mode,
    // no worktree isolation, no preset — the chat profile (tools: [], mode:
    // chat) and its own model binding apply host-side.
    const runAgent = sessionIdRef2.current ? sessionAgentRef.current : (stagedChat ? CHAT_AGENT_NAME : CODING_AGENT_NAME);
    if (!sessionIdRef2.current) sessionAgentRef.current = runAgent;
    const freshExecutionMode = sessionIdRef2.current || stagedChat ? undefined : (pendingMode ?? 'standard');
    const freshAgentPreset = sessionIdRef2.current || stagedChat ? undefined : (pendingPreset ?? undefined);
    // BUG-113: the permission preset picked on the New Task screen rides the
    // creation call — the host stamps both knobs into the fresh session
    // BEFORE the first turn runs. (The old post-TaskStarted best-effort
    // setPermissionPreset write silently lost races and left the engine on
    // manual while the UI advertised the pick.) Continuing sessions carry no
    // preset here; the composer's onChange path handles mid-session switches.
    const freshPermissionPreset = sessionIdRef2.current || stagedChat ? undefined
      : (permissionPresetRef.current || undefined);
    const send = (cb: (err: string | null, event: any) => void) => window.starriver.execute(taskText, sessionIdRef2.current || undefined, cb, freshExecutionMode, runAgent, stagedChat ? undefined : isolated, freshAgentPreset, freshPermissionPreset);
    send((err, event) => {
      if (err) {
        if (/SESSION_BUSY|single input slot|active turn/i.test(String(err))) {
          // BUG-96: recoverable — the previous turn still holds the slot.
          updateMessage(assistantId, '⚠️ ' + t('common.staleBusy'), false);
          setStaleBusy(true);
        } else {
          updateMessage(assistantId, `Error: ${err}`, false);
        }
        setSending(false);
        return;
      }
      if (!event) return;

      // 2026-10-04: the error note on a failed turn must land on the turn's
      // LAST assistant bubble and must never overwrite streamed text. The
      // legacy delta flow keeps assistantIdRef live and buffers its prose in
      // outputBuffer (full-content set is correct there); the modern
      // assistantText flow nulls assistantIdRef per step and its bubbles are
      // already final — append instead. With no bubble at all the note
      // becomes its own row so the failure is never silently dropped (the
      // pre-fix turnError wrote into the send-time placeholder via a stale
      // closure id and the ❌ vanished for the user).
      const surfaceTurnFailure = (legacyBuffer: string, note: string) => {
        const liveId = assistantIdRef.current;
        if (liveId) {
          updateMessage(liveId, legacyBuffer + '\n\n' + note, false);
          return;
        }
        const lastId = lastAssistantIdRef.current;
        if (lastId) {
          setMessages(prev => prev.map(m => m.id === lastId
            ? { ...m, content: (m.content || '') + '\n\n' + note, streaming: false }
            : m));
          return;
        }
        addMessage('assistant', note, false);
      };
      // Terminal parity for FAILED turns: the duration/token footer and the
      // sidebar refresh that case 'turnEnd' performs (without this an errored
      // turn ended with no failure marker and no meta anywhere).
      const finishTurnMeta = () => {
        const finishedId = assistantIdRef.current || lastAssistantIdRef.current;
        if (finishedId && turnStartRef.current > 0) {
          setTurnUsage({
            durationMs: Date.now() - turnStartRef.current,
            input: turnTokensRef.current.input,
            output: turnTokensRef.current.output,
            forId: finishedId,
          });
        }
        loadSessions();
      };

      // Idle gate: the preload listener now outlives a turn's terminal
      // (driver-claimed queue turns must stay visible), so foreign sessions'
      // follow-up turns can also land here. While this view is idle, only
      // TaskStarted may pass — it re-engages its own session (and carries
      // session_id); anything else would pollute the open transcript with
      // another session's stream.
      // 逐字节实时一致（开放回合快照流）：远端（手机/Web）发起的回合以
      // 「重放会话历史」在桌面对齐——快照事件需穿过空闲门。
      if (!sendingRef.current
          && (event.type || event.eventType) !== 'TaskStarted'
          && (event.type || event.eventType) !== 'traceSnapshot') {
        return;
      }
      // Session gate: every protocol event carries session_id (kernel ui.rs).
      // The busy gate alone leaked a running session's stream into whichever
      // conversation happened to be open — switching sessions mid-turn made
      // the old turn print into the new transcript. TaskStarted/sessionStarted
      // stay exempt: their handlers re-engage the session they belong to (new
      // task creation / queue auto-claim; sessionStarted only adopts a session
      // into an empty ref, never hijacks an open one). An EMPTY open-session
      // ref passes too — pre-sessionStarted events (e.g. a turnError before
      // the host created the session) belong to this view's own sending turn.
      // Events without session_id keep legacy behavior.
      const eventSid: string | undefined = event.sessionId ?? event.session_id;
      const eventType = event.type || event.eventType || '';
      if (eventSid && sessionIdRef2.current && eventSid !== sessionIdRef2.current
          && eventType !== 'TaskStarted' && eventType !== 'sessionStarted') {
        return;
      }

      switch (event.type || event.eventType || '') {
        case 'TaskStarted':
          if (event.session_id) {
            sessionIdRef.current = event.session_id;
            if (!sessionIdRef2.current) {
              sessionIdRef2.current = event.session_id;
              setActiveSessionId(event.session_id);
              // The host files the fresh session under its workspace before
              // the first execution spawns, so this refresh already sees the
              // projcache row — the new task appears under its workspace
              // group immediately instead of at turn end.
              void loadSessions();
              // WI-5: freeze the staged preset's label — the binding locked
              // host-side at creation and shows read-only in the status bar.
              if (freshAgentPreset) {
                const row = agentPresets.find(r => r.id === freshAgentPreset);
                setActivePresetLabel(row?.name ?? freshAgentPreset);
              }
              // BUG-113: a permission preset picked before sending rode the
              // creation call itself (freshPermissionPreset on execute) — the
              // host stamped the session before this turn started. The old
              // post-TaskStarted best-effort write is gone: it silently lost
              // races and left the engine on manual while the UI showed the
              // picked preset.
            }
          }
          // Queue auto-claim: the agent driver opens the next turn from the
          // durable queue the instant the previous one settles — no local
          // send() ran, so the busy state never armed and the queue card
          // never cleared (2026-10-05 session 85a4410f). Materialize the
          // claimed card entries as the user bubble and re-arm the run state
          // so Stop/streaming reflect the live turn.
          {
            const sid = event.session_id || sessionIdRef2.current;
            if (sid && sid === sessionIdRef2.current) {
              const claimed = queueItemsRef.current;
              if (claimed.length) {
                setQueue([]);
                for (const q of claimed) addMessage('user', q.text);
              }
              // Always re-assert (idempotent when a send already armed it): a
              // terminal processed in the same tick leaves a pending false
              // state that would otherwise clobber the ref on commit and
              // drop this turn's first events at the idle gate below.
              sendingRef.current = true;
              setSending(true);
            }
          }
          stopControl.onTaskStarted();
          if (event.session_id || sessionIdRef2.current) {
              refreshQueueRef.current(event.session_id || sessionIdRef2.current);
            }
          // New turn: the previous turn's fully-completed todo list is stale
          // noise on a fresh run — suppress it (watermark at the current end
          // of stream) until this turn writes a fresh todo_write.
          {
            const msgs = messagesRef.current;
            for (let i = msgs.length - 1; i >= 0; i--) {
              const m = msgs[i];
              if (m.role === 'tool' && m.result && /todo/i.test(m.content)) {
                const parsed = parseTodoState(m.result);
                if (parsed && parsed.items.length > 0 && parsed.items.every(it => it.done)) {
                  setTodoHideUpTo(i + 1);
                }
                break;
              }
            }
          }
          break;
        case 'RequestLog':
          if (debugEnabledRef.current) {
            setDebugLog(prev => [...prev, { turn: event.turn ?? 0, kind: 'request', data: event.body }]);
          }
          break;
        case 'ResponseLog':
          if (debugEnabledRef.current) {
            setDebugLog(prev => [...prev, { turn: event.turn ?? 0, kind: 'response', data: event.chunks ?? [] }]);
          }
          break;
        case 'TextDelta':
          // New loop turn (previous turn hit TextDone) → start a fresh assistant bubble,
          // matching how thinking/tool messages are segmented per turn/call.
          if (turnEnded) {
            assistantId = addMessage('assistant', '', true);
            assistantIdRef.current = assistantId;
            outputBuffer = '';
            turnEnded = false;
          } else if (assistantIdRef.current) {
            // The eager send-time placeholder sits right after the user
            // message; once this turn's prose starts it belongs at the
            // chronological tail (after the thinking/tool rows) — relocate it
            // or the answer renders above the whole process fold.
            const pid = assistantIdRef.current;
            setMessages(prev => {
              const idx = prev.findIndex(m => m.id === pid);
              if (idx === -1 || idx === prev.length - 1) return prev;
              const moved = prev[idx];
              return [...prev.slice(0, idx), ...prev.slice(idx + 1), moved];
            });
          }
          outputBuffer += (event.text || event.delta || '');
          pendingTextRef.current = outputBuffer;
          flushTextDelta(assistantId);
          break;
        case 'TextDone':
          if (rafRef.current) { cancelAnimationFrame(rafRef.current); rafRef.current = 0; }
          updateMessage(assistantId, outputBuffer, false);
          turnEnded = true; // next TextDelta belongs to a new turn → new bubble
          break;
        case 'ThinkingStart':
          // A new reasoning block begins: CLOSE the previous bubble (flush its
          // buffer + freeze its live state) instead of just nulling the ref —
          // a swallowed/missing Done used to orphan the previous row in the
          // streaming state forever (shimmer never stops). First delta/text
          // for this block creates a fresh row lazily.
          closeThinkingBubble();
          break;
        case 'ThinkingDelta': {
          const delta = event.text || '';
          if (!thinkingIdRef.current) {
            if (!delta) break; // no content yet — still no row
            thinkingIdRef.current = addMessage('thinking', delta, true);
            break;
          }
          const tid = thinkingIdRef.current;
          if (tid) {
            setMessages(prev => prev.map(m =>
              m.id === tid ? { ...m, content: m.content + delta } : m
            ));
          }
          break;
        }
        case 'ThinkingDone':
          closeThinkingBubble();
          break;
        case 'ThinkingText':
          // Authoritative full text when the message lands in the log (deltas may
          // be partially deduped): replace the content and end streaming, or the
          // thinking row keeps shimmering after the turn ends.
          if (thinkingIdRef.current) {
            const ttid = thinkingIdRef.current;
            setMessages(prev => prev.map(m =>
              m.id === ttid ? { ...m, content: event.text || m.content, streaming: false } : m
            ));
            thinkingIdRef.current = null;
          } else if (event.text) {
            addMessage('thinking', event.text, false);
          }
          break;
        case 'ToolCallStart':
          toolNameRef.current[event.id || ''] = event.name || event.tool_name || 'tool';
          const tmid = addMessage('tool', '🔧 ' + (event.name || event.tool_name || 'tool') + '...', true, event.name);
          toolMsgIdRef.current[event.id || ''] = tmid;
          break;
        case 'ToolCallInput':
          break;
        case 'ToolCallDone': {
          let mid = toolMsgIdRef.current[event.id || ''];
          const tname = toolNameRef.current[event.id || ''] || event.name || event.tool_name || '';
          // If no prior ToolCallStart (accumulator path), create a message now
          if (!mid) {
            mid = addMessage('tool', '🔧 ' + (tname || 'tool'), true, tname);
            toolMsgIdRef.current[event.id || ''] = mid;
          }
          // ToolCallDone now carries name from core; tname falls back to it when
          // no ToolCallStart preceded (accumulator path) — never renders "undefined".
          if (mid) {
            const cmd = toolCommandOf(event.input);
            updateMessage(mid, cmd ? `🔧 ${tname}: ${cmd}` : `🔧 ${tname || 'tool'}`, false);
          }
          break;
        }
        case 'ToolCallResult': {
          const mid = toolMsgIdRef.current[event.id || ''] || toolMsgIdRef.current[''];
          if (mid) {
            // Attach the full output to the tool card (shown in the result box);
            // the inline marker keeps the compact/legacy render consistent.
            const startedAt = toolStartRef.current[event.id || ''];
            const durationMs = startedAt ? Date.now() - startedAt : undefined;
            delete toolStartRef.current[event.id || ''];
            setMessages(prev => prev.map(m => m.id === mid ? {
              ...m,
              content: m.content + (event.ok ? ' ✓' : ' ✗'),
              result: event.output || '',
              ok: event.ok,
              streaming: false,
              meta: event.meta,
              durationMsTool: durationMs,
            } : m));
          } else {
            // BUG-147 defense-in-depth: an unpaired OK result with no card
            // payload carries zero renderable information — the inline path
            // would draw a bare "Tool ✓成功" ghost card (the compaction-prune
            // leak's shape; the orphan path drops its output by
            // construction). The relay now filters prune rewrites at the
            // source; this catches anything that still slips through.
            // Unpaired errors and meta cards keep rendering — those carry
            // real signal.
            if (!event.ok || event.meta) {
              addMessage('tool', event.ok ? '✓' : '✗ ' + (event.output || '').slice(0, 200), false);
            }
          }
          delete toolMsgIdRef.current[event.id || ''];
          break;
        }
        case 'ToolCallError':
          addMessage('tool', '❌ ' + (event.error || 'Tool error'), false);
          break;
        case 'ApprovalRequired':
          setApproval({
            tool: event.tool || event.tool_name || '',
            reason: event.reason || '',
            params: event.params || {},
            resolve: (allowed: boolean, remember?: boolean) => {
              if (sessionIdRef.current) {
                window.starriver.approve(sessionIdRef.current, allowed, remember ?? false).catch(console.error);
              }
              setApproval(null);
            },
          });
          break;
        // ── Modern kernel UiEvent contract (whole-message events) ──
        case 'sessionStarted':
          {
          const sid = event.sessionId ?? event.session_id;
          if (sid) {
            sessionIdRef.current = sid;
            if (!sessionIdRef2.current) {
              sessionIdRef2.current = sid;
              setActiveSessionId(sid);
            }
          }
          stopControl.onTaskStarted();
          break;
          }
        case 'assistantTextDelta': {
          // Typewriter: open (or reuse) the streaming assistant bubble and
          // append the delta; the final assistantText replaces the content
          // with the authoritative block.
          if (!assistantIdRef.current) {
            const nid = addMessage('assistant', '', true);
            assistantIdRef.current = nid;
            outputBuffer = '';
            turnEnded = false;
          } else {
            // Reusing the eager send-time placeholder: relocate it to the
            // chronological tail first, or the streamed answer renders above
            // this turn's thinking/tool rows (its stale position is right
            // after the user message).
            const pid = assistantIdRef.current;
            setMessages(prev => {
              const idx = prev.findIndex(m => m.id === pid);
              if (idx === -1 || idx === prev.length - 1) return prev;
              const moved = prev[idx];
              return [...prev.slice(0, idx), ...prev.slice(idx + 1), moved];
            });
          }
          outputBuffer += event.text || '';
          pendingTextRef.current = outputBuffer;
          flushTextDelta(assistantIdRef.current);
          break;
        }
        case 'assistantText': {
          // Step boundary: this step's model message is complete, so its
          // reasoning ends here — close the current thinking bubble; the next
 // step's thinkingDelta opens a fresh row (: one row per reasoning
          // block, not the whole turn crammed into the first one).
          closeThinkingBubble();
          // Remember the bubble: turnEnd (doneTs/summary/usage) keys off the
          // last assistant message, not assistantIdRef (nulled here).
          let aid = assistantIdRef.current;
          if (aid) {
            if (!(pendingTextRef.current || '').trim()) {
              // Pure-tool turn: the eager send-time placeholder never streamed
              // prose, and every thinking/tool row appended after it. The final
              // message belongs at the chronological tail — move it there
              // instead of filling the stale placeholder in place, or the
              // answer would render above the whole step fold.
              const staleId = aid;
              setMessages(prev => {
                const stale = prev.find(m => m.id === staleId);
                if (!stale) return prev;
                return [
                  ...prev.filter(m => m.id !== staleId),
                  { ...stale, content: event.text || '', streaming: false },
                ];
              });
            } else {
              updateMessage(aid, event.text || '', false);
            }
            assistantIdRef.current = null;
          } else {
            aid = addMessage('assistant', event.text || '', false);
          }
          lastAssistantIdRef.current = aid;
          break;
        }
        case 'thinkingDelta':
          thinkingBufRef.current += event.text || '';
          if (!thinkingRafRef.current) {
            thinkingRafRef.current = requestAnimationFrame(flushThinkingDelta);
          }
          break;
        case 'thinkingText':
          if (thinkingViaDeltaRef.current) {
            // Deltas already streamed this bubble; the full text would
            // duplicate it.
            break;
          }
          if (!thinkingIdRef.current) {
            thinkingIdRef.current = addMessage('thinking', event.text || '', false);
          } else {
            setMessages(prev => prev.map(m =>
              m.id === thinkingIdRef.current ? { ...m, content: event.text || '' } : m
            ));
          }
          break;
        case 'traceSnapshot': {
          // 逐字节实时一致（开放回合快照流）：远端（手机/Web）发起的回合，
          // 桌面以「重放会话历史」对齐——用户消息/思考/context 全在快照反映的
          // 历史里。桌面本地回合走乐观渲染（sending 门已挡掉），零影响。
          const snapSid: string | undefined = event.sessionId ?? event.session_id;
          if (snapSid && snapSid === sessionIdRef2.current && !sendingRef.current) {
            if (remoteResyncTimerRef.current == null) {
              remoteResyncTimerRef.current = window.setTimeout(() => {
                remoteResyncTimerRef.current = null;
                const sid = sessionIdRef2.current;
                if (sid && !sendingRef.current) void loadSessionMessages(sid);
              }, 800);
            }
          }
          break;
        }
        case 'toolCall': {
          // A pure-tool step (reasoning but no prose) never sees assistantText:
          // the first tool call equally marks the model message complete — close
          // the thinking bubble.
          closeThinkingBubble();
          const tname = event.name || 'tool';
          const cmd = toolCommandOf(event.input);
          const tmid = addMessage('tool', cmd ? `🔧 ${tname}: ${cmd}` : `🔧 ${tname}...`, true, tname);
          toolMsgIdRef.current[event.id || ''] = tmid;
          toolNameByIdRef.current[event.id || ''] = tname;
          const startedAt = Date.now();
          toolStartRef.current[event.id || ''] = startedAt;
          // WI-152: shell-type tools (bash/terminal_*) light the terminal entry icon while running.
          // WI-157: record the start time; the popover row shows a running stopwatch.
          if (/^bash$|^terminal/.test(tname)) {
            shellCallsRef.current.set(event.id || '', { cmd: cmd || tname, startedAt });
            syncShellState();
          }
          // WI-91: subagent spawns render as nested activity rows.
          if (tname === 'subagent_spawn') {
            setMessages(prev => prev.map(m => m.id === tmid ? { ...m, isSubagent: true } : m));
          }
          break;
        }
        case 'toolResult': {
          const mid = toolMsgIdRef.current[event.id || ''] || toolMsgIdRef.current[''];
          if (shellCallsRef.current.delete(event.id || '')) syncShellState();
          if (mid) {
            const startedAt = toolStartRef.current[event.id || ''];
            const durationMs = startedAt ? Date.now() - startedAt : undefined;
            delete toolStartRef.current[event.id || ''];
            setMessages(prev => prev.map(m => m.id === mid ? {
              ...m,
              content: m.content + (event.ok ? ' ✓' : ' ✗'),
              result: event.output || '',
              ok: event.ok,
              streaming: false,
              meta: event.meta,
              durationMsTool: durationMs,
            } : m));
          } else {
            addMessage('tool', event.ok ? '✓' : '✗ ' + (event.output || '').slice(0, 200), false);
          }
          // File-change bookkeeping: edit/write/patch outputs carry a unified
          // diff; keep the latest one per file for the turn-end summary.
          const rname = toolNameByIdRef.current[event.id || ''];
          if (event.ok && (rname === 'edit' || rname === 'write' || rname === 'patch') && isDiffContent(event.output || '')) {
            for (const f of parseDiff(event.output || '')) {
              const lines = [
                `--- ${f.path}`,
                `+++ ${f.path}`,
                ...f.hunks.flatMap(h => [
                  h.header,
                  ...h.lines.map(l => (l.kind === 'add' ? '+' : l.kind === 'del' ? '-' : ' ') + l.text),
                ]),
              ];
              turnDiffsRef.current.set(f.path, { add: f.add, del: f.del, text: lines.join('\n') });
            }
          }
          delete toolMsgIdRef.current[event.id || ''];
          delete toolNameByIdRef.current[event.id || ''];
          break;
        }
        case 'compaction': {
          // Context compaction checkpoint: fold-row marker in the stream.
          closeThinkingBubble();
          addMessage('compaction', event.summary || '', false);
          break;
        }
        case 'usage':
          // Per-step token usage; summed across the turn for the cost line
          // (WI-90) and across the session for the persistent readout.
          // BUG-92: a turn still running in session A must not inflate the
          // readout of session B after a switch.
          if (event.sessionId && event.sessionId !== sessionIdRef2.current) break;
          turnTokensRef.current.input += event.inputTokens || 0;
          turnTokensRef.current.output += event.outputTokens || 0;
          setSessionUsage(prev => ({
            in: prev.in + (event.inputTokens || 0),
            out: prev.out + (event.outputTokens || 0),
          }));
          break;
        case 'goalStatus':
          // BUG-87: the host's goal loop broadcasts live accounting — keep
          // the goal card's tokensUsed/status current instead of a stale 0.
          if (event.sessionId && event.sessionId !== sessionIdRef2.current) break;
          setGoalInfo((prev) => ({
            objective: event.objective ?? prev?.objective ?? '',
            tokenBudget: event.tokenBudget ?? prev?.tokenBudget,
            tokensUsed: event.tokensUsed ?? prev?.tokensUsed ?? 0,
            status: event.status ?? prev?.status ?? 'active',
          }));
          break;
        case 'approvalRequested':
          setApproval({
            tool: event.toolName || event.tool_name || '',
            reason: event.reason || '',
            params: event.params || {},
            resolve: (allowed: boolean, remember?: boolean) => {
              if (sessionIdRef.current) {
                window.starriver.approve(sessionIdRef.current, allowed, remember ?? false).catch(console.error);
              }
              setApproval(null);
            },
          });
          break;
        case 'instructionsBudget':
          // WI-35 A: the workspace-instructions composite was clipped to the
          // byte budget — surface it as a compaction-style system line.
          addMessage(
            'compaction',
            t('coding.instructionsBudgetLine', {
              composed: event.composedBytes ?? 0,
              budget: event.budgetBytes ?? 0,
            }),
            false,
          );
          break;
        case 'questionAsked':
          setQuestion({
            questionId: event.questionId || '',
            text: event.text || '',
            options: Array.isArray(event.options) ? event.options : [],
          });
          break;
        case 'planReviewRequested':
          // Plan fold: the model submitted its plan via exit_plan_mode —
          // open the review card with the plan text, and auto-open the
          // right-hand Plan panel (review badge until approved).
          setPlanReview({ requestId: event.requestId || '', plan: event.plan || '' });
          setLastPlan(event.plan || null);
          setPlanApproved(false);
          setPlanPhase('planning');
          setPlanPanelOpen(true);
          break;
        case 'planReviewDecided':
          // Any surface's decision (or a timeout) closes the card everywhere.
          // The plan markdown itself is KEPT in the right panel as the
          // execution reference; approval flips the status-bar phase.
          setPlanReview(null);
          if (event.approved) {
            setPlanApproved(true);
            setPlanPhase('executing');
          }
          break;
        case 'turnEnd':
          if (rafRef.current) { cancelAnimationFrame(rafRef.current); rafRef.current = 0; }
          closeThinkingBubble();
          // WI-152: turn end means running shell tool calls have settled — clear,
          // so a lost result event cannot leave the icon stuck lit.
          if (shellCallsRef.current.size) { shellCallsRef.current.clear(); syncShellState(); }
          {
            const truncated = event.reason === 'max-tokens';
            // The modern flow (assistantText) nulls assistantIdRef, so fall
            // back to the last assistant bubble this turn produced.
            const finishedId = assistantIdRef.current || lastAssistantIdRef.current;
            if (finishedId) {
              const note = truncated ? '\n\n⚠️ ' + t('coding.turnTruncated') : '';
              setMessages(prev => prev.map(m => {
                if (m.id !== finishedId) return m;
                // Legacy delta path buffers text in outputBuffer; the modern
                // path leaves it empty — keep the message content then.
                const content = outputBuffer
                  ? outputBuffer + note
                  : (m.content || '') + note;
                return { ...m, content, streaming: false, doneTs: Date.now() };
              }));
            }
            assistantIdRef.current = null;
            if (finishedId && turnDiffsRef.current.size > 0) {
              const files = [...turnDiffsRef.current.entries()].map(([path, v]) => ({ path, ...v }));
              setTurnSummary({
                files,
                add: files.reduce((n, f) => n + f.add, 0),
                del: files.reduce((n, f) => n + f.del, 0),
                forId: finishedId,
              });
              turnDiffsRef.current = new Map();
            }
            if (finishedId && turnStartRef.current > 0) {
              setTurnUsage({
                durationMs: Date.now() - turnStartRef.current,
                input: turnTokensRef.current.input,
                output: turnTokensRef.current.output,
                forId: finishedId,
              });
            }
            // The one-shot read-only exploration ends: restore the previous
            // execution mode (its turn never shows the confirm card). Restore UI
            // state and the kernel's global mode directly (turn-level override;
            // the session's locked value stays untouched).
            restoreExploreMode();
            // Plan fold: the confirm card is review-driven now — it opens on
            // planReviewRequested (the model's exit_plan_mode call), not on
            // turn end.
          }
          setSending(false);
          setAttachments([]);
          stopControl.onTerminal();
          // BUG-141: a terminal event proves the backend input slot is free —
          // any stale-busy marker armed on reload/session-switch is stale now.
          setStaleBusy(false);
          loadSessions();
          // Idle background terminals: a shell parked at its prompt survives
          // the turn by design, but a forgotten one is easy to miss. Surface
          // each idler once per idle episode at the settle; fresh activity
          // re-arms the reminder.
          window.starriver.listTerminals?.().then((r) => {
            const rows = r?.terminals ?? [];
            rows.forEach((x) => {
              if (x.status !== 'idle') idleNotifiedRef.current.delete(x.id);
            });
            const fresh = rows.filter((x) => x.status === 'idle' && !idleNotifiedRef.current.has(x.id));
            if (!fresh.length) return;
            fresh.forEach((x) => idleNotifiedRef.current.add(x.id));
            const idleCount = rows.filter((x) => x.status === 'idle').length;
            addMessage('tool', t('coding.terminalsIdleHint', { count: idleCount }));
          }).catch(() => {});
          break;
        case 'cancelled':
          if (rafRef.current) { cancelAnimationFrame(rafRef.current); rafRef.current = 0; }
          updateMessage(assistantId, (outputBuffer ? outputBuffer + '\n\n' : '') + t('common.taskStopped'), false);
          setSending(false);
          setAttachments([]);
          if (shellCallsRef.current.size) { shellCallsRef.current.clear(); syncShellState(); }
          assistantIdRef.current = null;
          stopControl.onTerminal();
          setStaleBusy(false); // BUG-141: slot proven free.
          refreshGitStatus();
          break;
        case 'TaskCompleted':
          if (rafRef.current) { cancelAnimationFrame(rafRef.current); rafRef.current = 0; }
          updateMessage(assistantId, outputBuffer, false);
          closeThinkingBubble();
          setSending(false);
          setAttachments([]);
          assistantIdRef.current = null;
          stopControl.onTerminal();
          setStaleBusy(false); // BUG-141: slot proven free.
          loadSessions();
          // A successful turn may have switched branches / committed — the
          // status bar must not keep showing the pre-turn branch.
          refreshGitStatus();
          break;
        case 'TaskFailed':
          if (rafRef.current) { cancelAnimationFrame(rafRef.current); rafRef.current = 0; }
          closeThinkingBubble();
          if (/SESSION_BUSY|single input slot|active turn/i.test(String(event.error || ''))) {
            surfaceTurnFailure(outputBuffer, '⚠️ ' + t('common.staleBusy'));
            setStaleBusy(true);
          } else {
            surfaceTurnFailure(outputBuffer, '❌ ' + (event.error || 'Failed'));
            // BUG-141: a non-busy failure also proves the slot was free.
            setStaleBusy(false);
          }
          finishTurnMeta();
          restoreExploreMode();
          setSending(false);
          setAttachments([]);
          assistantIdRef.current = null;
          stopControl.onTerminal();
          break;
        case 'turnError':
          // Kernel-level turn failure (e.g. no LLM route, provider 500 after
          // the retry ladder). It is a terminal event but arrives without
          // TaskFailed — surface it instead of leaving the bubble stuck on
          // "Thinking…".
          if (rafRef.current) { cancelAnimationFrame(rafRef.current); rafRef.current = 0; }
          closeThinkingBubble();
          if (/SESSION_BUSY|single input slot|active turn/i.test(String(event.error || event.message || ''))) {
            surfaceTurnFailure(outputBuffer, '⚠️ ' + t('common.staleBusy'));
            setStaleBusy(true);
          } else {
            surfaceTurnFailure(outputBuffer, '❌ ' + (event.error || event.message || 'Turn failed'));
            // BUG-141: a non-busy failure also proves the slot was free.
            setStaleBusy(false);
          }
          finishTurnMeta();
          restoreExploreMode();
          setSending(false);
          setAttachments([]);
          assistantIdRef.current = null;
          stopControl.onTerminal();
          break;
        case 'TaskCancelled':
          if (rafRef.current) { cancelAnimationFrame(rafRef.current); rafRef.current = 0; }
          updateMessage(assistantId, (outputBuffer ? outputBuffer + '\n\n' : '') + t('common.taskStopped'), false);
          setSending(false);
          setAttachments([]);
          assistantIdRef.current = null;
          stopControl.onTerminal();
          setStaleBusy(false); // BUG-141: slot proven free.
          refreshGitStatus();
          break;
        default: {
          console.log('[event] unknown:', event.type || event.eventType, JSON.stringify(event).slice(0, 200));
        }
      }
    }, freshExecutionMode);
  }, [input, sending, addMessage, updateMessage, loadSessions, attachments, t, isolated, refreshGitStatus, workspaces, activeWorkspaceId, lang, setLang, openBacktrack, pendingMode, pendingPreset, agentPresets, stagedChat, mode, rejectImages]);

  const handleKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); send(); }
  };

  // Group messages into display items: assistant tool-call lines become tool cards,
  // and a directly-following tool-result message ("✓ …"/"✗ …") is attached to the
  // card as its execution result — one card shows command + result together.
  type CodingDisplay =
    | { key: string; type: 'msg'; message: Message }
    | { key: string; type: 'tool'; tool: string; result?: string; ok?: boolean | null; streaming: boolean; isSubagent?: boolean; durationMsTool?: number }
    | { key: string; type: 'step'; thinking: Message[]; items: CodingDisplay[] };
  const displayItems = useMemo<CodingDisplay[]>(() => {
    const items: CodingDisplay[] = [];
    // Turn-step grouping: one assistant round (with its preceding thinking rows
    // and tool rows) forms a step; user messages are turn boundaries rendered
    // outside any step.
    let step: { key: string; thinking: Message[]; items: CodingDisplay[] } | null = null;
    const flushStep = () => {
      if (step && (step.thinking.length > 0 || step.items.length > 0)) {
        items.push({ key: step.key, type: 'step', thinking: step.thinking, items: step.items });
      }
      step = null;
    };
    let n = 0;
    // Belt & braces: only the NEWEST thinking row may render live — an event
    // quirk (missing Done / out-of-order Start) must never leave an early row
    // shimmering after later steps already ran.
    let lastThinkId: string | null = null;
    for (let i = messages.length - 1; i >= 0; i--) {
      if (messages[i].role === 'thinking') { lastThinkId = messages[i].id; break; }
    }
    for (const m of messages) {
      if (m.role === 'user') {
        flushStep();
        // History rows: display_pieces splits one user message into a text
        // piece + one piece per Image block. Merge pieces of the SAME logged
        // message (same server seq) back into ONE bubble — but never merge
        // distinct sends: consecutive user rows with different seqs are
        // separate prompts (e.g. retries after a failed turn) and must stay
        // separate bubbles.
        const prev = items[items.length - 1];
        if (
          prev && prev.type === 'msg' && prev.message.role === 'user' &&
          m.seq != null && prev.message.seq != null && prev.message.seq === m.seq
        ) {
          prev.message = {
            ...prev.message,
            content: `${prev.message.content || ''}\n${m.content || ''}`.trim(),
          };
          continue;
        }
        items.push({ key: `${m.id}#u${n++}`, type: 'msg', message: { ...m } });
        continue;
      }
      const pushThink = (mm: Message) => {
        // Thinking belongs to the assistant round that FOLLOWS it (same step).
        // When the current sub-step already has real content (tools/text), this
        // thinking belongs to the next step — flush and open a new sub-step so
 // each step's thinking sits at the head of its own step (: one row
        // per reasoning block, placed where it happened). An empty streaming
        // assistant placeholder does not count as real content.
        const hasRealItems = !!step && step.items.some(it =>
          it.type === 'tool' || (it.type === 'msg' && !!(it.message.content || '').trim())
        );
        if (hasRealItems) flushStep();
        if (!step) step = { key: `step-${mm.id}`, thinking: [], items: [] };
        step.thinking.push(mm);
      };
      if (m.role === 'thinking') {
        // Empty thinking rows are display noise (an eager placeholder that
        // never got reasoning text) — skip them entirely, live or persisted.
        if (!(m.content || '').trim()) continue;
        // Non-newest rows render as done even if their streaming flag was
        // never cleared (missing Done / out-of-order Start).
        pushThink(m.streaming && m.id !== lastThinkId ? { ...m, streaming: false } : m);
        continue;
      }
      if (m.role === 'assistant') {
        if (!step) step = { key: `step-${m.id}`, thinking: [], items: [] };
        for (const s of splitAssistantContent(m.content || '')) {
          if (s.kind === 'tool') {
            step.items.push({ key: `${m.id}#t${n++}`, type: 'tool', tool: s.text, streaming: !!m.streaming, isSubagent: m.isSubagent, durationMsTool: m.durationMsTool });
          } else {
            step.items.push({ key: `${m.id}#p${n++}`, type: 'msg', message: { ...m, content: s.text } });
          }
        }
        continue;
      }
      if (m.role === 'tool') {
        // WI-147: client-side feedback rows (slash-command output, fork/
        // back-track notices, provider toasts) are NOT work — they carry no
        // 🔧 marker (host tool rows always start with it) and no server seq;
        // render them as TOP-LEVEL tool rows so the turn work-fold never
        // hides them. Host tool calls keep grouping into steps.
        const content = m.content || '';
        if (m.seq == null && !content.startsWith('🔧')) {
          flushStep();
          items.push({
            key: `${m.id}#t${n++}`, type: 'tool', tool: content,
            result: m.result, ok: m.ok, streaming: !!m.streaming,
            isSubagent: m.isSubagent, durationMsTool: m.durationMsTool,
          });
          continue;
        }
        if (!step) step = { key: `step-${m.id}`, thinking: [], items: [] };
        const lead = content.match(/^([✓✗])\s*/);
        const last = step.items[step.items.length - 1];
        if (lead && last && last.type === 'tool') {
          // Stored tool-result message right after its tool card → attach as result.
          last.result = content.replace(/^[✓✗]\s*/, '');
          last.ok = lead[1] === '✓';
          last.streaming = !!m.streaming;
          last.durationMsTool = m.durationMsTool ?? last.durationMsTool;
        } else {
          step.items.push({
            key: `${m.id}#t${n++}`, type: 'tool', tool: content,
            result: m.result, ok: m.ok, streaming: !!m.streaming,
            isSubagent: m.isSubagent, durationMsTool: m.durationMsTool,
          });
        }
        continue;
      }
      // system and other roles: flush the current step, render standalone.
      flushStep();
      items.push({ key: `${m.id}#m${n++}`, type: 'msg', message: m });
    }
    flushStep();
    return items;
  }, [messages]);

  // Turn folding (iOS parity): every user message starts a turn (same semantics as
  // the Trajectory's TURN x); earlier turns collapse to a one-line summary by default, and the last
  // turn always stays expanded. Pure frontend derivation — zero protocol/persistence changes.
  /** Turn id pending scroll-into-view (rail tick / post-collapse); consumed once after render. */
  const pendingRevealRef = useRef<string | null>(null);
  const [turnRevealNonce, setTurnRevealNonce] = useState(0);
  const railRef = useRef<HTMLDivElement | null>(null);
  const [turnTip, setTurnTip] = useState<{ x: number; y: number; no: number; title: string; steps: number } | null>(null);
  interface TurnGroup {
    /** The turn's user message id; loose rows before any user message group under 'head'. */
    id: string;
    title: string;
    items: CodingDisplay[];
    /** Turn duration (ms): history projection (last assistant durationMs) → in-group ts span → default. */
    durationMs?: number;
    /** Streaming turn's start epoch-ms — drives the work-fold row's ticking (WI-147). */
    liveFrom?: number;
    rowCount: number;
  }
  const turnGroups = useMemo<TurnGroup[]>(() => {
    const previewOf = (it: CodingDisplay): string => {
      if (it.type === 'msg') {
        const t = (it.message.content || '').split('\n')[0];
        return t.startsWith('[') ? '' : t; // [system prompt] / [runtime-context] never make a title
      }
      if (it.type === 'step') {
        const t0 = (it.thinking[0]?.content || '').split('\n')[0];
        if (t0.trim()) return t0;
        const first = it.items[0];
        if (first?.type === 'tool') return first.tool;
        if (first?.type === 'msg') return (first.message.content || '').split('\n')[0];
      }
      return '';
    };
    const turns: TurnGroup[] = [];
    for (const it of displayItems) {
      if (it.type === 'msg' && it.message.role === 'user') {
        const title =
          (it.message.content || '')
            .split('\n')[0]
            .replace(/!\[[^\]]*\]\([^)]*\)/g, '')
            .trim() || t('coding.turnUntitled');
        turns.push({ id: it.message.id, title, items: [it], rowCount: 1 });
        continue;
      }
      if (turns.length === 0) turns.push({ id: 'head', title: '', items: [], rowCount: 0 });
      const last = turns[turns.length - 1];
      last.items.push(it);
      last.rowCount += 1;
      if (!last.title) {
        const p = previewOf(it).trim();
        if (p) last.title = p.length > 80 ? p.slice(0, 80) + '…' : p;
      }
    }
    // Fold-row duration: prefer the history projection's durationMs on the last assistant; live
    // turns lack it (the projection only comes back on reload), so fall back to the first-ts → last-doneTs span
    // (an assistant bubble's ts is creation time; completion lives on doneTs); with neither, use the
    // default (missing pieces stay hidden, matching the TurnUsageMeta convention).
    const groupMsgs = (items: CodingDisplay[]): Message[] => items.flatMap((it) =>
      it.type === 'msg'
        ? [it.message]
        : it.type === 'step'
          ? [...it.thinking, ...it.items.filter((x): x is Extract<CodingDisplay, { type: 'msg' }> => x.type === 'msg').map((x) => x.message)]
          : []);
    for (const tg of turns) {
      const msgs = groupMsgs(tg.items);
      let durationMs: number | undefined;
      for (let i = msgs.length - 1; i >= 0; i--) {
        const m = msgs[i];
        if (m.role === 'assistant' && m.durationMs != null) {
          durationMs = m.durationMs;
          break;
        }
      }
      if (durationMs == null) {
        const starts = msgs.map((m) => m.ts ?? m.doneTs).filter((x): x is number => x != null);
        const ends = msgs.map((m) => m.doneTs ?? m.ts).filter((x): x is number => x != null);
        if (starts.length > 0 && ends.length > 0) {
          const span = Math.max(...ends) - Math.min(...starts);
          if (span > 0) durationMs = span;
        }
      }
      if (durationMs != null) tg.durationMs = durationMs;
      const starts = msgs.map((m) => m.ts).filter((x): x is number => x != null);
      if (starts.length > 0) tg.liveFrom = Math.min(...starts);
    }
    return turns;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [displayItems]);
  /** WI-147: per-turn work-fold state — the steps hide until expanded. The
   *  streaming last turn is force-expanded (live steps stay visible) and the
   *  manual toggles persist until the session switches. */
  const [workFoldOpen, setWorkFoldOpen] = useState<Set<string>>(new Set());
  const toggleWorkFold = (id: string) => setWorkFoldOpen(prev => {
    const next = new Set(prev);
    if (next.has(id)) next.delete(id); else next.add(id);
    return next;
  });
  useEffect(() => { setWorkFoldOpen(new Set()); }, [activeSessionId]);
  /** WI-147: split one turn into the iOS layout parts — the leading user
   *  message (head), the working steps (fold: thinking/tool rounds in
   *  trajectory order), and everything else OUTSIDE the fold in chronological
   *  order: system/compaction notices stay visible, and the turn summary
   *  (the LAST non-empty assistant prose — lifted out of its step when
   *  embedded) closes the turn. Folded steps hide only the work, never
   *  notices.
   *  hoistSummary: lifting only makes sense for a FINISHED turn — the last
   *  prose is then the real closing summary. On the live turn every mid-turn
   *  explanation would get pinned below the fold while later steps keep
   *  stacking above it (timeline upside down), so pass false while sending. */
  const partitionTurn = (items: CodingDisplay[], hoistSummary = true): {
    head: CodingDisplay[]; fold: CodingDisplay[]; outside: CodingDisplay[];
  } => {
    if (items.length === 0) return { head: [], fold: [], outside: [] };
    const head: CodingDisplay[] = [];
    let rest = items;
    if (items[0].type === 'msg' && items[0].message.role === 'user') {
      head.push(items[0]);
      rest = items.slice(1);
    }
    let sumIdx = -1;
    let sumSub = -1;
    rest.forEach((it, i) => {
      if (it.type === 'msg' && it.message.role === 'assistant' && (it.message.content || '').trim()) {
        sumIdx = i; sumSub = -1;
      } else if (it.type === 'step') {
        it.items.forEach((sub, si) => {
          if (sub.type === 'msg' && sub.message.role === 'assistant' && (sub.message.content || '').trim()) {
            sumIdx = i; sumSub = si;
          }
        });
      }
    });
    const fold: CodingDisplay[] = [];
    const outside: CodingDisplay[] = [];
    rest.forEach((it, i) => {
      if (it.type !== 'step') { outside.push(it); return; }
      if (hoistSummary && i === sumIdx && sumSub >= 0) {
        const step = it as Extract<CodingDisplay, { type: 'step' }>;
        fold.push({ ...step, items: step.items.filter((_, si) => si !== sumSub) });
        outside.push(step.items[sumSub]);
        return;
      }
      fold.push(it);
    });
    return { head, fold, outside };
  };
  /** Rail navigation: scroll to a turn (turns are laid out flat, no expansion needed). */
  const revealTurn = (tgId: string) => {
    pendingRevealRef.current = tgId;
    setTurnRevealNonce(n => n + 1);
  };
  // Post-expansion scroll positioning: wait for the new DOM, then scroll the target turn's header row into view.
  useEffect(() => {
    const tgId = pendingRevealRef.current;
    if (!tgId) return;
    pendingRevealRef.current = null;
    requestAnimationFrame(() => {
      codingListRef.current
        ?.querySelector(`[data-work-fold="${tgId}"]`)
        ?.scrollIntoView({ block: 'start' });
    });
  }, [turnGroups, turnRevealNonce]);
  // Proximity zoom: the rule near the cursor lengthens dynamically (Dock-style), written straight to style to avoid re-renders.
  const magnifyTicks = (clientY: number | null) => {
    const wrap = railRef.current;
    if (!wrap) return;
    wrap.querySelectorAll<HTMLDivElement>('.turn-tick').forEach((el) => {
      if (clientY == null) { el.style.width = '14px'; return; }
      const r = el.getBoundingClientRect();
      const d = Math.abs(r.top + r.height / 2 - clientY);
      const inf = Math.max(0, 1 - d / 110);
      el.style.width = Math.round(14 + inf * 22) + 'px';
    });
  };

  /** The last top-level display item across all turns (same fold-visibility
   *  rule as the render below). The empty streaming-assistant placeholder may
   *  show its "Thinking…" dots only in that slot — anything rendered after it
   *  means steps already ran and pre-output dots would mislead. */
  const lastDisplayKey = (() => {
    let key: string | null = null;
    for (let gi = 0; gi < turnGroups.length; gi++) {
      // Same hoist rule as the render map below — the two must agree or the
      // "Thinking…" placeholder could pick the wrong last slot.
      const live = gi === turnGroups.length - 1 && sending;
      const { head, fold, outside } = partitionTurn(turnGroups[gi].items, !live);
      const workOpen = live || workFoldOpen.has(turnGroups[gi].id);
      const seq = [...head, ...(workOpen ? fold : []), ...outside];
      if (seq.length) key = seq[seq.length - 1].key;
    }
    return key;
  })();

  /** WI-101: fetch the previous page and prepend it. */
  // Renders one display item (steps are wrapped by the outer map: thinking rows
  // + content items).
  // quietAssistantPlaceholder: while the same step's thinking is streaming, an
  // empty assistant placeholder skips its own "Thinking…" dots (otherwise two
  // shimmering bubbles appear).
  const renderItem = (item: CodingDisplay, opts?: { quietAssistantPlaceholder?: boolean }) => {
    if (item.type === 'step') return null;
    if (item.type === 'tool') {
      // WI-91: subagent spawns render as nested activity rows — an indented
      // card with the subagent icon, template label, live/streamed/done
      // state, and the returned text nested inside.
      if (item.isSubagent) {
        const label = (item.tool || '').replace(/^🔧\s*/, '');
        return (
          <div key={item.key} className="message tool subagent-row" data-testid="subagent-row">
            <div className="subagent-card">
              <div className="subagent-head">
                <span className="subagent-icon">🤖</span>
                <span className="subagent-title">{t('coding.subagentTitle')}</span>
                <span className="subagent-label">{label}</span>
                {item.streaming
                  ? <span className="subagent-state running">{t('coding.subagentRunning')}</span>
                  : item.ok === true
                    ? <span className="subagent-state done">✓</span>
                    : item.ok === false
                      ? <span className="subagent-state failed">✗</span>
                      : null}
              </div>
              {item.result && (
                <div className="subagent-body">
                  {item.result.split('\n').slice(0, 12).map((line, i) => (
                    <div key={i} className="subagent-line">{line}</div>
                  ))}
                  {item.result.split('\n').length > 12 && (
                    <div className="subagent-more">… {t('coding.subagentMore')}</div>
                  )}
                </div>
              )}
            </div>
          </div>
        );
      }
      return (
        <div key={item.key} className="message tool">
          <div className="bubble">
            <ToolRow content={item.tool} result={item.result} ok={item.ok} streaming={item.streaming} collapsible onOpenDiff={openReviewFile} durationMs={item.durationMsTool} />
            {item.streaming && <span className="cursor-blink">|</span>}
          </div>
        </div>
      );
    }
    const m = item.message;
    // Live "Thinking…" dots are only honest while nothing has happened after
    // the placeholder: once later steps/tool rows render, the model is no
    // longer pre-output, so the dots would mislead (screenshot bug: dots
    // stuck above already-completed steps). Quieted unless this item is the
    // last rendered display item (and the same step's thinking isn't
    // streaming — pre-existing rule).
    if (m.role === 'assistant' && m.streaming && !m.content && (opts?.quietAssistantPlaceholder || item.key !== lastDisplayKey)) {
      return null;
    }
    if (m.role === 'compaction') {
      return (
        <div key={item.key} className="message compaction">
          <CompactionRow summary={m.content} />
        </div>
      );
    }
    // WI-96: review tool rows with structured findings render the card.
    if (m.role === 'tool' && m.meta && typeof m.meta === 'object' && Array.isArray((m.meta as { findings?: unknown[] }).findings)) {
      const findings = (m.meta as { findings?: Parameters<typeof FindingsCard>[0]['findings'] }).findings;
      if (findings && findings.length > 0) {
        return (
          <div key={item.key} className="message tool">
            <FindingsCard findings={findings} onOpen={(file) => openReviewFile?.(file, '')} />
          </div>
        );
      }
    }
    // Turn file-change summary: live accumulator first, then the
    // history projection carried on the final assistant piece.
    const summaryFiles =
      turnSummary && m.id === turnSummary.forId
        ? turnSummary.files
        : m.role === 'assistant'
          ? m.fileChanges?.map((f) => ({ ...f, text: f.diff }))
          : undefined;
    return (
      <div key={item.key} className={`message ${m.role}`}>
        <div className="bubble">
          {m.role === 'assistant' && m.streaming && !m.content ? (
            <span className="thinking-indicator">
              <span className="thinking-dot" />
              <span className="thinking-dot" />
              <span className="thinking-dot" />
              <span style={{ marginLeft: 6, color: 'var(--text-muted)' }}>{t('common.thinkingDots')}</span>
            </span>
          ) : m.role === 'assistant' ? (
            // Tool lines were already split into cards above; prose only.
            <MarkdownContent source={m.content || ''} />
          ) : m.role === 'user' ? (
            // WI-1 P1: image refs render as thumbnails (lazy attachments.read).
            <UserMessageContent text={m.content || ''} />
          ) : (
            // '(processing...)' is only valid while a live message streams;
            // a history message with empty content renders blank.
            <div>{m.content || (m.streaming ? '(processing...)' : '')}</div>
          )}
          {m.streaming && m.content && <span className="cursor-blink">|</span>}
        </div>
        {/* Completion time + copy: under user input and each turn's final
            assistant summary (streaming bubbles get it once finished). */}
        {/* End-of-turn extras (duration, token usage, file summary) stack
            below the summary, never beside it: the assistant message row
            wraps and this footer takes a full row under the bubble. */}
        <div className={m.role === 'assistant' ? 'assistant-footer' : m.role === 'user' ? 'user-footer' : undefined}>
          {/* File-change summary sits ABOVE the meta row; duration and
              token spend share the timestamp row. */}
          {summaryFiles && summaryFiles.length > 0 && (
            <TurnFileSummary files={summaryFiles} onOpen={openReviewFile} />
          )}
          {m.role === 'user' && (
            <button
              type="button"
              className="resend-btn"
              data-testid="resend-btn"
              title={t('coding.resend')}
              onClick={() => {
                setInput(m.content || '');
                if (m.seq != null) {
                  resendFromRef.current = m.seq;
                } else {
                  // BUG-92: live-sent rows carry no server seq — resolve the
                  // fork point from the timeline (last matching user entry)
                  // before the send; unresolved falls back to a plain send.
                  const text = m.content || '';
                  resendFromRef.current = null;
                  void window.starriver.getSessionTimeline?.(sessionIdRef2.current).then((tl) => {
                    const hit = [...(tl ?? [])].reverse().find(
                      (x: any) => x.role === 'user' && x.content === text);
                    resendFromRef.current = hit?.id ?? null;
                  }).catch(() => {});
                }
                inputRef.current?.focus();
              }}
            >↺ {t('coding.resend')}</button>
          )}
          {(m.role === 'user' || (m.role === 'assistant' && !m.streaming && m.content)) && (
            <MessageMeta
              ts={m.role === 'assistant' ? (m.doneTs ?? m.ts) : m.ts}
              text={m.role === 'assistant'
                ? m.content.split('\n').filter((l) => !l.startsWith('🔧')).join('\n').trim()
                : m.content}
              extra={
                <TurnUsageMeta
                  m={m}
                  live={turnUsage && m.id === turnUsage.forId ? turnUsage : null}
                />
              }
            />
          )}
        </div>
      </div>
    );
  };

  // Two distinct screens: New Task (no session yet — workspace + execution-mode
  // pickers above the input, no trajectory switch) vs Session (trajectory
  // switch, mode pinned in the bottom status bar). A turn that was sent but
  // not yet acked by TaskStarted already counts as the Session screen — the
  // pickers must not flash back mid-send.
  const inSession = !!activeSessionId || messages.length > 0;
  // WI-125 turn rail: multi-turn sessions get navigation; a running turn shows
  // its tick live even when it is the session's first (only) turn — gating on
  // group count alone left the rail absent for the entire first turn.
  const showTurnRail = inSession && (turnGroups.length > 1 || sending);
  // Todo widget state: the latest todo_write result across the message stream
  // (live and replayed history share the same `## TODO` text shape), honoring
  // the todoHideUpTo stale-todo watermark above.
  const latestTodo = useMemo(() => {
    for (let i = messages.length - 1; i >= todoHideUpTo; i--) {
      const m = messages[i];
      if (m.role === 'tool' && m.result && /todo/i.test(m.content)) {
        const parsed = parseTodoState(m.result);
        if (parsed) return parsed;
      }
    }
    return null;
  }, [messages, todoHideUpTo]);
  const [todoOpen, setTodoOpen] = useState(false);
  const todoDone = latestTodo?.items.filter(i => i.done).length ?? 0;
  const todoTotal = latestTodo?.items.length ?? 0;
  const todoCurrent = latestTodo?.items.find(i => i.inProgress) ?? null;
  const modeLabel = mode === 'plan' ? (planPhase === 'executing' ? t('coding.executionPlanExecuting') : t('coding.executionPlan'))
    : mode === 'ptc' ? t('coding.executionPtc')
    : mode === 'goal' ? t('coding.executionGoal')
    : t('coding.executionStandard');

  return (
    <div className="coding-view" style={{ display: 'flex', flexDirection: 'column', height: '100%', gap: 0 }}>
      {/* Row: coding area + (optional) Plan panel; the status bar spans full width below. */}
      <div style={{ flex: 1, minWidth: 0, display: 'flex', minHeight: 0 }}>
      {/* Left: Coding area */}
      <div style={{ flex: 1, minWidth: 0, display: 'flex', flexDirection: 'column', height: '100%' }}>
      {/* Error toast */}
      {errorMsg && (
        <div style={{ padding: '8px 16px', background: 'var(--danger-soft)', borderBottom: '1px solid var(--danger-border)', color: 'var(--danger)', fontSize: 13, textAlign: 'center' }}>
          {errorMsg}
        </div>
      )}
      {/* Toolbar (Conversation/Trajectory switch + always-on skill chips) is a
          Session-screen affordance. The New Task screen opens clean: no title
          bar, no separator line. */}
      {inSession && (
        <div className="toolbar">
          <div className="coding-view-switch" role="tablist">
            <button type="button" role="tab" aria-selected={view === 'chat'} className={view === 'chat' ? 'active' : ''} onClick={() => setView('chat')}>{t('coding.sessionTab')}</button>
            <button type="button" role="tab" aria-selected={view === 'trajectory'} className={view === 'trajectory' ? 'active' : ''} onClick={() => setView('trajectory')}>{t('coding.trajectoryTab')}</button>
            {lastPlan && (
              <button type="button" className={planPanelOpen ? 'active' : ''} aria-label="plan-panel" aria-pressed={planPanelOpen}
                onClick={() => setPlanPanelOpen(o => !o)}>📄 Plan</button>
            )}
          </div>

          {/* Model & reasoning selectors live in the composer's control row. */}
          {/* Always-on skills chips (body injected every prompt) */}
          {skills.filter(s => s.always_on).map(s => (
            <span key={s.name} className="toolbar-chip" data-tip={`Always-on skill: ${s.name}`}>⚡ {s.name}</span>
          ))}
          {/* Right end: open the workspace externally (terminal / IDE). */}
          <ToolsMenu
            cwd={workspaces.find(w => w.id === activeWorkspaceId)?.path ?? null}
            onError={setErrorMsg}
          />
        </div>
      )}
      {/* Workspace instructions banner (same file set as the model context) */}
      {workspaceInstructions && (
        <details style={{ margin: '0 16px', padding: '8px 12px', background: 'var(--panel-bg)', borderRadius: 6, border: '1px solid var(--accent-border)' }}>
          <summary style={{ cursor: 'pointer', fontSize: 13, fontWeight: 600, color: 'var(--accent)' }}>
            {t('coding.workspaceInstructions')}
            {instructionsTruncated && (
              <span style={{ color: 'var(--danger)', fontWeight: 400 }}> · {t('coding.instructionsTruncated')}</span>
            )}
          </summary>
          <pre style={{ marginTop: 8, fontSize: 12, whiteSpace: 'pre-wrap', color: 'var(--text-secondary)', maxHeight: 200, overflowY: 'auto' }}>
            {workspaceInstructions}
          </pre>
        </details>
      )}
      {(view === 'chat' || !inSession) && (<>
      <div className="turn-body">
      {showTurnRail && (
        <div
          className="turn-rail"
          ref={railRef}
          onMouseMove={(e) => magnifyTicks(e.clientY)}
          onMouseLeave={() => magnifyTicks(null)}
        >
          <div className="rail-scroll">
            {turnGroups.map((tg, i) => (
              <div
                key={i}
                className="turn-tick"
                onClick={() => revealTurn(tg.id)}
                onMouseEnter={(e) => {
                  const r = e.currentTarget.getBoundingClientRect();
                  setTurnTip({ x: r.right + 10, y: Math.max(10, r.top + r.height / 2 - 24), no: i + 1, title: tg.title, steps: Math.max(1, tg.items.filter(it => it.type === 'step').length) });
                }}
                onMouseLeave={() => setTurnTip(null)}
              />
            ))}
          </div>
          {turnTip && (
            <div className="tick-tip" style={{ left: turnTip.x, top: turnTip.y }}>
              <div className="no">TURN {turnTip.no} · {turnTip.steps} steps</div>
              <div className="tt">{turnTip.title}</div>
            </div>
          )}
        </div>
      )}
      <div className={`message-list coding-messages${showTurnRail ? ' rail-inset' : ''}${isNewTask ? ' new-task' : ''}`} ref={codingListRef} onScroll={() => {
          const el = codingListRef.current;
          if (!el) return;
          codingFollowRef.current = el.scrollHeight - el.scrollTop - el.clientHeight < 80;
        }}>
        {messages.length === 0 && (
          <div className="empty-state">
            {/* The mode switch lives where the sparkle box used to — the empty
                state IS the chooser. Hidden once a session exists: the runner
                is locked then. */}
            {!activeSessionId && !sending && (
              <div className="newtask-mode-row">
                <AgentPresetPicker mode={newTaskMode} onChange={(m) => setNewTaskMode(m)} />
              </div>
            )}
            <p>{stagedChat ? t('coding.emptyChat') : t('coding.empty')}</p>
          </div>
        )}
        {turnGroups.map((tg, gi) => {
          const isLast = gi === turnGroups.length - 1;
          const renderTurnItem = (item: CodingDisplay) => {
            if (item.type === 'step') {
              const thinkingStreaming = item.thinking.some((t) => t.streaming);
              // No fold: thinking + tool rows render flat, in the order they
              // happened (trajectory order). The step container only keeps the
 // left-rail styling around one assistant round.
              return (
                <div key={item.key} className="turn-step">
                  {item.thinking.map((tm) => <ThinkingRow key={tm.id} m={tm} />)}
                  {item.items.map((it) => renderItem(it, { quietAssistantPlaceholder: thinkingStreaming }))}
                </div>
              );
            }
            return renderItem(item);
          };
          // WI-147 iOS layout: user head → work fold (steps) → notices/summary.
          // Live turn (last group while sending): no summary hoist — mid-turn
          // prose stays in trajectory order instead of being pinned below.
          const { head, fold, outside } = partitionTurn(tg.items, !(isLast && sending));
          const workOpen = (isLast && sending) || workFoldOpen.has(tg.id);
          return (
            <React.Fragment key={tg.id}>
              {head.map(renderTurnItem)}
              {fold.length > 0 && (
                <WorkFoldRow
                  group={{ id: tg.id, durationMs: tg.durationMs, liveFrom: isLast && sending ? tg.liveFrom : undefined }}
                  expanded={workOpen}
                  onToggle={() => toggleWorkFold(tg.id)}
                />
              )}
              {workOpen ? fold.map(renderTurnItem) : null}
              {outside.map(renderTurnItem)}
            </React.Fragment>
          );
        })}
        <div ref={bottomRef} />
      </div>
      </div>
      {/* API Debug log — collapsible cards, default off */}
      {debugEnabled && (
        <div style={{ flexShrink: 0, maxHeight: '35%', overflowY: 'auto', borderTop: '1px solid var(--border)', background: 'var(--panel-bg)' }}>
          <div style={{ padding: '5px 12px', fontSize: 12, fontWeight: 600, color: 'var(--text-muted)', borderBottom: '1px solid var(--border)', display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
            <span>{debugLog.length > 0 ? t('chat.debugEntries', { count: debugLog.length }) : t('chat.debugWaiting')}</span>
            {debugLog.length > 0 && <button onClick={() => setDebugLog([])} style={{ background: 'none', border: 'none', cursor: 'pointer', color: 'var(--text-muted)', fontSize: 11 }}>{t('chat.debugClear')}</button>}
          </div>
          {debugLog.length === 0 && (
            <div style={{ padding: '8px 12px', fontSize: 12, color: 'var(--text-muted)' }}>
              {t('chat.debugHint')}
            </div>
          )}
          {debugLog.map((entry, i) => (
            <details key={i} style={{ padding: '4px 12px', borderBottom: '1px solid var(--border)' }}>
              <summary style={{ fontSize: 12, cursor: 'pointer', fontWeight: 600, color: entry.kind === 'request' ? 'var(--accent)' : 'var(--success)' }}>
                {entry.kind === 'request' ? '⬆ REQUEST' : '⬇ RESPONSE'} · turn {entry.turn} · {entry.kind === 'request' ? 'body' : `${(entry.data as string[]).length} chunks`}
              </summary>
              <pre style={{ fontSize: 11, whiteSpace: 'pre-wrap', wordBreak: 'break-all', maxHeight: 220, overflowY: 'auto', color: 'var(--text)', margin: '6px 0 2px', background: 'var(--surface)', padding: 6, borderRadius: 4, border: '1px solid var(--border)' }}>
                {entry.kind === 'request'
                  ? (() => { try { return JSON.stringify(entry.data, null, 2); } catch { return String(entry.data); } })()
                  : (entry.data as string[]).join('\n')}
              </pre>
            </details>
          ))}
        </div>
      )}
      {/* Plan review card (plan fold): review-driven — the model's
          exit_plan_mode call presents the plan; the right-hand Plan panel shows
          the markdown, and the decision card renders as a Composer overlay
          (passed down below), covering the input bar while a decision is
          pending — the composer has nothing to offer mid-review. */}
      {/* Todo widget (global, not per-message): the latest todo_write state
          with a collapsed progress summary and an expandable full list. */}
      {inSession && latestTodo && (
        <div className={`todo-widget${showTurnRail ? ' rail-inset' : ''}`}>
          <button type="button" className="todo-widget-head" aria-expanded={todoOpen} aria-label="todo-widget"
            onClick={() => setTodoOpen(o => !o)}>
            <span className="todo-widget-title">{t('coding.todoTitle')}</span>
            <span className="todo-widget-bar" aria-hidden="true">
              <span className="todo-widget-bar-fill" style={{ width: `${todoTotal ? Math.round(todoDone * 100 / todoTotal) : 0}%` }} />
            </span>
            <span className="todo-widget-count">{todoDone}/{todoTotal}</span>
            {todoCurrent && !todoOpen && (
              <span className="todo-widget-current">⚙️ {todoCurrent.title}</span>
            )}
            <span className="todo-widget-chevron" aria-hidden="true">{todoOpen ? '⌃' : '⌄'}</span>
          </button>
          {todoOpen && <TodoCard todo={latestTodo} />}
        </div>
      )}
      <div
        ref={slashBoxRef}
        className={`${isNewTask ? 'newtask-composer' : ''}${showTurnRail ? ' rail-inset' : ''}`}
        style={{ position: 'relative' }}
      >
        {/* New Task screen: the Coding / Chat switch sits in the empty state
            above (it IS the hero); this row is contextual. Coding: workspace
            picker + execution mode (DSH creation-time binding). Chat: pick an
            existing workspace for the Chat Agent or stay ungrouped — no
            create, no execution mode. Everything hides once Send creates the
            session — the locked mode/preset then show in the bottom status
            bar. */}
        {messages.length === 0 && !activeSessionId && !sending && (
          <div className="composer-mode-picker">
            {stagedChat ? (
              <WorkspacePicker
                workspaces={workspaces}
                activeId={chatWorkspaceId}
                onSelect={(id) => { void handleChatWorkspaceChange(id); }}
                noneLabel={t('workspace.ungroupedSessions')}
                placeholderLabel={t('coding.selectWorkspace')}
                ariaLabel="chat-workspace"
              />
            ) : (
              <>
                <WorkspacePicker
                  workspaces={workspaces}
                  activeId={activeWorkspaceId}
                  onSelect={(id) => {
                    activateWorkspace(id).catch((e) => console.error('[Coding] Activate workspace failed:', e));
                  }}
                  onNew={() => setShowNewWorkspace(true)}
                />
                <ExecutionModePicker value={pendingMode} onChange={(v) => handleModeChange(v)} />
              </>
            )}
          </div>
        )}
        {/* WI-97: goal status card (objective + status). */}
        {goalInfo && (
          <div className={`goal-status-card ${goalInfo.status}`} data-testid="goal-status-card">
            <span>🎯 {goalInfo.objective}</span>
            <span className="goal-status-badge">{goalInfo.status}</span>
            {goalInfo.tokenBudget != null && (
              <span className="goal-status-budget">{goalInfo.tokensUsed}/{goalInfo.tokenBudget}</span>
            )}
          </div>
        )}
        {/* WI-98: doctor report card. */}
        {doctorReport && doctorReport.length > 0 && (
          <div className="doctor-report" data-testid="doctor-report">
            {doctorReport.map(c => (
              <div key={c.name} className={`doctor-row ${c.status.toLowerCase()}`}>
                <span className={`doctor-status ${c.status.toLowerCase()}`}>{c.status}</span>
                <span className="doctor-name">{c.name}</span>
                <span className="doctor-detail">{c.detail}</span>
              </div>
            ))}
          </div>
        )}
        {mode === 'goal' && (() => {
          const used = sessionUsage.in + sessionUsage.out;
          const pct = goalBudget > 0 ? Math.min(100, Math.round((used / goalBudget) * 100)) : 0;
          const over80 = goalBudget > 0 && pct >= 80;
          const over95 = goalBudget > 0 && pct >= 95;
          return (
            <div className={`goal-budget-card${over95 ? ' over95' : over80 ? ' over80' : ''}`} data-testid="goal-budget-card">
              <span className="goal-budget-label">🎯 {t('coding.goalBudget')}</span>
              {goalBudget > 0 ? (
                <>
                  <span className="goal-budget-nums">
                    {used.toLocaleString()} / {goalBudget.toLocaleString()} ({pct}%)
                  </span>
                  <div className="goal-budget-bar">
                    <div className="goal-budget-fill" style={{ width: `${pct}%` }} />
                  </div>
                  {over95 && <span className="goal-budget-warn">⚠ {t('coding.goalBudgetExceeded')}</span>}
                </>
              ) : (
                <input
                  type="number"
                  min={0}
                  placeholder={t('coding.goalBudgetPlaceholder')}
                  className="goal-budget-input"
                  onKeyDown={(e) => {
                    if (e.key === 'Enter') {
                      const v = Number((e.target as HTMLInputElement).value) || 0;
                      setGoalBudgetPersisted(v);
                      (e.target as HTMLInputElement).blur();
                    }
                  }}
                  defaultValue=""
                />
              )}
            </div>
          );
        })()}
        {(sessionUsage.in > 0 || sessionUsage.out > 0) && (
          <div className="composer-usage" data-testid="composer-usage">
            {(t('coding.usageSummary') as string)
              .replace('{in}', String(sessionUsage.in))
              .replace('{out}', String(sessionUsage.out))}
          </div>
        )}
        <Composer
          railInset={showTurnRail}
          fileSuggest={!stagedChat && activeWorkspaceId ? { workspaceId: activeWorkspaceId } : null}
          planReviewCard={
            planReview && sessionIdRef2.current ? (
              <PlanReviewCard
                plan={planReview.plan}
                currentPreset={permissionPresetRef.current}
                onApplyPreset={(preset) => {
                  // Approve + apply: the preset write lands BEFORE the plan
                  // decision so execution starts under the chosen mode; a
                  // failed write is logged but never blocks the approval.
                  const sid = sessionIdRef2.current!;
                  window.starriver
                    .setPermissionPreset(sid, preset)
                    .catch((e) => console.error('[Coding] setPermissionPreset (plan approval) failed:', e))
                    .then(() => {
                      // The card's pick is now the engine's truth — sync the
                      // composer picker so the UI can't advertise a broader
                      // mode than the ledger holds (2026-10-14: the picker
                      // kept showing 完全访问 while the card's 标准 write had
                      // confined the session to workspace-write; the model
                      // read that from the runtime context and every
                      // out-of-workspace edit failed with sandbox confine).
                      setPermissionPresetState(preset);
                      permissionPresetRef.current = preset;
                      setPlanPhase('executing');
                      window.starriver
                        .planReviewDecision(sid, planReview.requestId, true)
                        .catch((e) => console.error('[Coding] planReviewDecision failed:', e));
                    });
                }}
                onDecide={(feedback) => {
                  window.starriver
                    .planReviewDecision(sessionIdRef2.current!, planReview.requestId, false, feedback)
                    .catch((e) => console.error('[Coding] planReviewDecision failed:', e));
                }}
              />
            ) : null
          }
          value={input}
          onChange={(v) => {
            setInput(v);
            if (suppressSlashRef.current) { suppressSlashRef.current = false; setSlashOpen(false); return; }
            if (v.startsWith('/')) { setSlashOpen(true); setSlashIndex(0); } else { setSlashOpen(false); }
          }}
          onKeyDown={(e) => {
            // Enter/arrows during IME composition (Chinese candidate selection)
            // belong to the IME, not the app — treating them as plain keys would
            // send the message on candidate confirmation.
            if (e.nativeEvent.isComposing || e.keyCode === 229) return;
            if (slashOpen) {
              const items = buildSlashItems(input.slice(1));
              if (e.key === 'ArrowDown') { e.preventDefault(); if (items.length) setSlashIndex(i => (i + 1) % items.length); return; }
              if (e.key === 'ArrowUp') { e.preventDefault(); if (items.length) setSlashIndex(i => (i - 1 + items.length) % items.length); return; }
              if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); if (items.length) handleSlashSelect(items[Math.min(slashIndex, items.length - 1)]); return; }
              if (e.key === 'Escape') { e.preventDefault(); setSlashOpen(false); return; }
            }
            if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); send(); }
          }}
          onPaste={handlePaste}
          onDrop={handleDrop}
          placeholder={stagedChat
            ? t('chat.placeholder')
            : (sending ? t('coding.queuePlaceholder') : t('coding.placeholder'))}
          queueDock={queueItems.length > 0 ? (
            <ul className="queue-dock" data-testid="queue-dock">
              {queueItems.map((q) => (
                <li key={q.id} className="queue-row" data-testid="queue-row">
                  <span className="queue-grip" aria-hidden="true">⠿</span>
                  {editingQueueId === q.id ? (
                    <textarea
                      className="queue-edit"
                      data-testid="queue-editor"
                      aria-label={t('coding.queuedEdit')}
                      value={queueEditDraft}
                      autoFocus
                      rows={2}
                      onChange={(e) => setQueueEditDraft(e.target.value)}
                      onKeyDown={(e) => {
                        if (e.nativeEvent.isComposing || e.keyCode === 229) return;
                        if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); saveQueueEdit(q.id); }
                        if (e.key === 'Escape') { e.preventDefault(); setEditingQueueId(null); }
                      }}
                    />
                  ) : (
                    <span className="queue-text">{queueDisplayText(q.text, t)}</span>
                  )}
                  {editingQueueId === q.id ? (
                    <button type="button" className="queue-btn" data-testid="queue-save" onClick={() => saveQueueEdit(q.id)}>{t('common.save')}</button>
                  ) : (
                    <>
                      <button type="button" className="queue-btn queue-now" data-testid="queue-now" title={t('coding.queuedSteer')} onClick={() => sendQueuedNow()}>↑ {t('coding.queuedNow')}</button>
                      <button type="button" className="queue-btn queue-icon-btn" data-testid="queue-edit" aria-label={t('coding.queuedEdit')} title={t('coding.queuedEdit')} onClick={() => { setEditingQueueId(q.id); setQueueEditDraft(q.text); }} dangerouslySetInnerHTML={{ __html: ICON_PENCIL }} />
                      <button type="button" className="queue-btn queue-icon-btn" data-testid="queue-remove" aria-label={t('coding.queuedRemove')} title={t('coding.queuedRemove')} onClick={() => removeQueued(q.id)} dangerouslySetInnerHTML={{ __html: ICON_TRASH }} />
                    </>
                  )}
                </li>
              ))}
            </ul>
          ) : null}
          stacked
          trailingControls={(() => {
            // Both modes carry model/reasoning selects; they write the staged
            // agent's OWN binding — Coding and Chat keep independent models,
            // so a select must never leak across modes.
            const agentName = stagedChat ? CHAT_AGENT_NAME : CODING_AGENT_NAME;
            const modelId = stagedChat ? chatModelId : activeModelId;
            const effort = stagedChat ? chatReasoningEffort : reasoningEffort;
            const onModel = stagedChat ? handleChatModelChange : handleModelChange;
            const onEffort = stagedChat ? handleChatReasoningChange : handleReasoningChange;
            return (
              <>
                <MiniSelect
                  aria-label={stagedChat ? 'chat-model' : 'coding-model'} title={t('common.modelTip')}
                  value={modelId} onChange={(e) => onModel(e.target.value)}
                  disabled={sending}>
                  {bindings.length === 0 && <option value="">{t('coding.noModels')}</option>}
                  {bindings.filter(b => b.agentName === agentName).map(binding =>
                    binding.boundModels.map(bm => (
                      <option key={bm.id} value={bm.id}>{bm.name}</option>
                    ))
                  )}
                </MiniSelect>
                <MiniSelect
                  aria-label={stagedChat ? 'chat-reasoning' : 'coding-reasoning'} title={t('common.reasoningTip')}
                  value={effort} onChange={(e) => onEffort(e.target.value)}
                  disabled={sending}>
                  {reasoningOptionsFor().map(o => (
                    <option key={o.value} value={o.value}>{t('common.r_' + (o.value || 'default'))}</option>
                  ))}
                </MiniSelect>
              </>
            );
          })()}
          leadingControls={(terminals.length > 0 || shellBusy) ? (
            <button type="button" className="composer-terminals-btn" aria-label="background-terminals"
              title={t('coding.terminalsTitle')} onClick={() => setTerminalsOpen(o => !o)}
              dangerouslySetInnerHTML={{ __html: TERMINAL_ICON }} />
          ) : null}
          permissionPreset={stagedChat ? undefined : (mode === 'plan' && planPhase === 'planning' ? 'manual' : permissionPreset)}
          // The plan lock belongs to the planning phase only: once the review passes (planReviewDecided/confirmPlan, or on reopen
          // when getPermission folds out workspace-write) the phase flips to executing and the approval-mode picker unlocks,
          // letting the user re-pick the approval mode for later turns (WI-144); during planning it stays locked to Manual.
          permissionLocked={!stagedChat && mode === 'plan' && planPhase === 'planning'}
          onPermissionPresetChange={stagedChat ? undefined : (m) => {
            setPermissionPresetState(m);
            permissionPresetRef.current = m;
            if (sessionIdRef2.current) {
              return window.starriver.setPermissionPreset(sessionIdRef2.current, m).catch((e) => {
                console.error('[Coding] setPermissionPreset failed:', e);
                throw e;
              });
            }
          }}
          disabled={false}
          inputRef={inputRef}
          attachments={attachments}
          onRemoveAttachment={removeAttachment}
          onFileSelect={handleFileSelect}
          imagesDisabled={imagesDisabled}
          imagesNotice={imagesNotice}
          onSend={send}
          onStop={staleBusy && !stopControl.stopping ? stopStaleTurn : stopControl.requestStop}
          sending={sending || staleBusy}
          stopping={stopControl.stopping}
          canSend={!!input.trim() || attachments.length > 0}
          skills={skills}
          mcpServers={mcpServers}
          onInsertSkill={() => { suppressSlashRef.current = true; }}
          approval={approval ? {
            tool: approval.tool,
            reason: approval.reason,
            params: approval.params,
            onAllow: () => approval.resolve(true, false),
            onDeny: () => approval.resolve(false),
            onAllowSimilar: () => approval.resolve(true, true),
          } : null}
          question={question ? {
            questionId: question.questionId,
            text: question.text,
            options: question.options,
            onAnswer: (answer) => {
              const sid = sessionIdRef2.current;
              if (sid) {
                window.starriver.respondQuestion(sid, question.questionId, answer).catch(console.error);
              }
              setQuestion(null);
            },
          } : null}
        />
        {/* WI-150: background terminals popover — hangs off the composer, above
            the input bar; lists start time + live elapsed per terminal. */}
        {terminalsOpen && (terminals.length > 0 || shellBusy) && (
          <div className="terminals-popover" data-testid="terminals-panel">
            <div className="terminals-head">
              <span>{t('coding.terminalsTitle')}</span>
              <span className="terminals-count">{terminals.length + (shellBusy ? shellCmds.length : 0)}</span>
            </div>
            {/* WI-152: running bash command (one-shot process, not a PTY); since WI-157 shows start time + running stopwatch. */}
            {shellCmds.map((entry, i) => (
              <ShellCmdRow key={`shell-${i}`} entry={entry} />
            ))}
            {terminals.map(term => (
              <TerminalRow key={term.id} term={term} onKill={() => {
                window.starriver.killTerminal?.(term.id)
                  .then(() => setTerminals(prev => prev.filter(x => x.id !== term.id)))
                  .catch(() => {});
              }} />
            ))}
          </div>
        )}
        {slashOpen && (() => {
          const items = buildSlashItems(input.slice(1));
          let lastGroup: string | null = null;
          const groupLabel = (g: string) => g === 'coding' ? t('coding.groupCoding') : t('coding.groupSkills');
          return (
            <div className="slash-palette">
              {items.length === 0 && (
                <div className="slash-empty">{t('coding.noCommands', { input })}</div>
              )}
              {items.map((item, i) => {
                const showHeader = item.group !== lastGroup;
                lastGroup = item.group;
                return (
                  <React.Fragment key={item.id}>
                    {showHeader && <div className="slash-group">{groupLabel(item.group)}</div>}
                    <div
                      className={`slash-item${i === slashIndex ? ' selected' : ''}`}
                      onClick={() => handleSlashSelect(item)}
                      onMouseEnter={() => setSlashIndex(i)}>
                      <span className="slash-cmd">{item.cmd}</span>
                      <span className="slash-desc">{item.desc}</span>
                    </div>
                  </React.Fragment>
                );
              })}
            </div>
          );
        })()}
      </div>
      </>
      )}
      {/* Trajectory: lives INSIDE the middle column — same display area as
          Conversation (full-width swap on tab switch), not a right-side
          sibling panel. Plan/File review panel stays to the far right. */}
      {view === 'trajectory' && inSession && (
        <div className="traj-scroll"><TrajectoryView sessionId={activeSessionId} /></div>
      )}
      </div>
      {/* Right: side panel — two tabs over one resizable shell. Plan: the
          exit_plan_mode markdown, kept after approval as an execution
          reference (auto-opens on planReviewRequested). 审查: a turn's changed
          file opened as a diff from the changed-files card. */}
      {inSession && (reviewFile || (lastPlan && planPanelOpen)) && (() => {
        const showPlanTab = !!lastPlan;
        const active: 'plan' | 'file' =
          panelTab === 'file' && reviewFile ? 'file' : reviewFile && !showPlanTab ? 'file' : 'plan';
        const closeActive = () => {
          if (active === 'file') {
            setReviewFile(null);
            if (!(planPanelOpen && lastPlan)) setPanelTab('plan');
          } else {
            setPlanPanelOpen(false);
            if (reviewFile) setPanelTab('file');
          }
        };
        return (
          <div className="plan-panel" style={{ width: planWidth }}>
            <div className="plan-panel-resizer" onMouseDown={planOnMouseDown} />
            <div className="plan-panel-head">
              <div className="plan-panel-tabs">
                {showPlanTab && (
                  <button
                    type="button"
                    className={`plan-panel-tab${active === 'plan' ? ' active' : ''}`}
                    onClick={() => setPanelTab('plan')}
                  >
                    {t('coding.planPanelTitle')}
                    <span className={`plan-panel-badge${planApproved ? ' approved' : ''}`}>
                      {planApproved ? t('coding.planBadgeApproved') : t('coding.planBadgeReview')}
                    </span>
                  </button>
                )}
                {reviewFile && (
                  <button
                    type="button"
                    className={`plan-panel-tab${active === 'file' ? ' active' : ''}`}
                    title={reviewFile.path}
                    onClick={() => setPanelTab('file')}
                  >
                    {reviewFile.path.split('/').pop()}
                  </button>
                )}
              </div>
              {active === 'file' && reviewFile && (
                <button
                  type="button"
                  className="plan-panel-fs"
                  aria-label="fullscreen-diff"
                  title={t('common.fullscreen')}
                  onClick={() => setDiffFull(true)}
                >⤢</button>
              )}
              <button type="button" className="plan-panel-close" aria-label="close-plan-panel" onClick={closeActive}>✕</button>
            </div>
            <div className="plan-panel-body">
              {active === 'file' && reviewFile ? (
                <DiffView text={reviewFile.text} maxLines={1000000} />
              ) : (
                <MarkdownContent source={lastPlan || ''} />
              )}
            </div>
          </div>
        );
      })()}
      {/* Fullscreen diff viewer: the whole change in one scrollable surface;
          click backdrop / ✕ / Esc to close. */}
      {diffFull && reviewFile && (
        <div className="diff-fullscreen" onClick={() => setDiffFull(false)}>
          <div className="diff-fullscreen-card" onClick={(e) => e.stopPropagation()}>
            <div className="diff-fullscreen-head">
              <span className="diff-fullscreen-path" title={reviewFile.path}>{reviewFile.path}</span>
              <button
                type="button"
                className="diff-fullscreen-close"
                aria-label="close-fullscreen-diff"
                title={t('common.close')}
                onClick={() => setDiffFull(false)}
              >✕</button>
            </div>
            <div className="diff-fullscreen-body">
              <DiffView text={reviewFile.text} maxLines={1000000} />
            </div>
          </div>
        </div>
      )}
    </div>
      {/* Bottom status bar: workspace (folder icon) + path + session execution
          mode (right of the directory) + git branch (branch icon). The plan
          phase (planning/executing) follows the approved review + sandbox. */}
      {inSession && <div className="coding-status-bar">
        {(() => {
          const active = workspaces.find(w => w.id === activeWorkspaceId);
          const presetItem = inSession && activePresetLabel ? (
            <span className="coding-status-item coding-status-preset" title={t('coding.presetTip')}>
              {activePresetLabel}
            </span>
          ) : null;
          const modeItem = inSession ? (
            <span className="coding-status-item coding-status-mode" data-tip={t('coding.modeTip')}>
              <span className="coding-status-icon" dangerouslySetInnerHTML={{ __html: EXEC_MODE_ICONS[mode || 'standard'] || EXEC_MODE_ICONS.standard }} />
              {modeLabel}
            </span>
          ) : null;
          return active ? (
            <>
              <span className="coding-status-item" title={active.path}>
                <span className="coding-status-icon" dangerouslySetInnerHTML={{ __html: ICON_WORKSPACE_FOLDER }} />
                {active.title}
              </span>
              <span className="coding-status-item coding-status-cwd">{active.path}</span>
              {modeItem}
              {presetItem}
              {gitStatus && (
                <span className="coding-status-item coding-status-git" title={gitStatus.isWorktree ? 'worktree' : 'git branch'}>
                  <span className="coding-status-icon" dangerouslySetInnerHTML={{ __html: ICON_GIT_BRANCH }} />
                  {gitStatus.isRepo ? (gitStatus.branch || t('worktree.detached')) : t('worktree.notGitRepo')}
                </span>
              )}
            </>
          ) : modeItem;
        })()}
      </div>}

      {backtrackOpen && (
        <BacktrackModal
          messages={backtrackMessages}
          loading={backtrackLoading}
          onSelect={handleBacktrackSelect}
          onCancel={() => setBacktrackOpen(false)}
        />
      )}
      {confirmDeleteSession && (
        <SessionDeleteModal
          onConfirm={() => confirmSessionDelete(confirmDeleteSession)}
          onCancel={() => setConfirmDeleteSession(null)}
        />
      )}
      {archiveOpen && (
        <ArchiveModal
          sessions={archivedSessions}
          onRestore={(s) => { setArchiveOpen(false); handleUnarchive(s.id); }}
          onCancel={() => setArchiveOpen(false)}
        />
      )}
    </div>
  );
}


/* ─── WorkspaceProvider + sidebar section ─── */

// App-level store for the workspace/session registry. The sidebar section
// renders it persistently across tabs, so the state (and which session is
// open) survives tab switches; CodingView consumes the same context and
// mirrors the "open session" decision into the message stream.
interface WorkspaceCtx {
  workspaces: WorkspaceInfo[];
  sessions: SessionInfo[];
  archivedSessions: SessionInfo[];
  activeWorkspaceId: string | null;
  setActiveWorkspaceId: (id: string | null) => void;
  activeSessionId: string;
  setActiveSessionId: (id: string) => void;
  collapsedWorkspaces: Set<string>;
  archivedOpenKeys: Record<string, boolean>;
  toggleArchivedGroup: (key: string) => void;
  renamingWorkspace: string | null;
  setRenamingWorkspace: (id: string | null) => void;
  renameValue: string;
  setRenameValue: (v: string) => void;
  renamingSession: string | null;
  setRenamingSession: (id: string | null) => void;
  renameSessionValue: string;
  setRenameSessionValue: (v: string) => void;
  showNewWorkspace: boolean;
  setShowNewWorkspace: (v: boolean) => void;
  newWorkspacePath: string;
  setNewWorkspacePath: (v: string) => void;
  confirmDeleteWorkspace: WorkspaceInfo | null;
  setConfirmDeleteWorkspace: (w: WorkspaceInfo | null) => void;
  loadSessions: () => Promise<void>;
  /** WI-179: session ids with a live turn (scheduled active runs ∪ interactive
   *  running slots), poll-refreshed every 3s while the page is visible. The
   *  sidebar session rows render the blinking dot off this set. */
  runningSessionIds: Set<string>;
  /** WI-179: immediate one-shot re-poll — call right after triggering a run
   *  (scheduled first-run) so the dot appears without waiting a tick. */
  refreshRunningSessions: () => void;
  toggleWorkspace: (workspaceId: string) => Promise<void>;
  /** Plain activation (persist + refresh) without the sidebar's fold toggle —
   *  the New Task screen's workspace picker uses this. */
  activateWorkspace: (workspaceId: string) => Promise<void>;
  requestNewConversation: (workspaceId: string) => Promise<void>;
  /** CodingView registers a reset for view-local per-session flags (isolation toggle). */
  registerIsolationReset: (fn: (() => void) | null) => void;
  archiveSession: (sessionId: string) => Promise<void>;
  handleUnarchive: (sessionId: string) => Promise<void>;
  handleRenameSession: (sessionId: string) => Promise<void>;
  handleRenameSubmit: (workspaceId: string) => Promise<void>;
  handleCreateWorkspace: (path: string) => Promise<void>;
  handleDeleteWorkspace: (workspaceId: string, e: React.MouseEvent) => void;
  handleConfirmDelete: () => Promise<void>;
  newConvSeq: number;
  wsError: string | null;
  /** Provider-level error toast — shared by sidebar flows (unarchive/restore). */
  showError: (msg: string) => void;
  isolationResetRef: React.MutableRefObject<(() => void) | null>;
  /** CodingView registers its addMessage so provider-side actions (unarchive,
   *  session rename) still drop the in-chat tool toast, as before. */
  registerNotifier: (fn: ((text: string, kind: string) => void) | null) => void;
}

const WorkspaceContext = createContext<WorkspaceCtx | null>(null);

export function useWorkspaces(): WorkspaceCtx {
  const ctx = useContext(WorkspaceContext);
  if (!ctx) throw new Error('useWorkspaces must be used within WorkspaceProvider');
  return ctx;
}

// WI-180: cross-tab "open this session" — tabs rendered through the registry
// (Scheduled panel) have no handle on the sidebar or the tab state, so they
// dispatch; the bridge (mounted inside the provider) does the two calls the
// sidebar task rows make: activate the session, open the Coding view.
export const OPEN_SESSION_EVENT = 'starriver:open-session';

export function requestOpenSession(sessionId: string): void {
  window.dispatchEvent(new CustomEvent(OPEN_SESSION_EVENT, { detail: { sessionId } }));
}

function OpenSessionBridge({ setTab }: { setTab: (tab: TabId) => void }) {
  const ws = useWorkspaces();
  useEffect(() => {
    const open = (e: Event) => {
      const sessionId = (e as CustomEvent<{ sessionId?: string }>).detail?.sessionId;
      if (!sessionId) return;
      ws.setActiveSessionId(sessionId);
      setTab('coding');
    };
    window.addEventListener(OPEN_SESSION_EVENT, open);
    return () => window.removeEventListener(OPEN_SESSION_EVENT, open);
  }, [ws, setTab]);
  return null;
}

export function WorkspaceProvider({ children }: { children: React.ReactNode }) {
  const { t } = useI18n();
  const [workspaces, setWorkspaces] = useState<WorkspaceInfo[]>([]);
  const [sessions, setSessions] = useState<SessionInfo[]>([]);
  // WI-179: live mirror for the running-poll — it must judge "a running
  // session has no sidebar row yet" against the freshest list without
  // re-creating the poll callback (the 3s interval would re-arm on every
  // state change otherwise).
  const sessionsRef = useRef<SessionInfo[]>([]);
  const [archivedSessions, setArchivedSessions] = useState<SessionInfo[]>([]);
  const [activeWorkspaceId, setActiveWorkspaceId] = useState<string | null>(null);
  const [activeSessionId, setActiveSessionId] = useState<string>('');
  const [collapsedWorkspaces, setCollapsedWorkspaces] = useState<Set<string>>(new Set());
  // Per-group archived sub-section collapse state.
  const [archivedOpenKeys, setArchivedOpenKeys] = useState<Record<string, boolean>>({});
  const [renamingWorkspace, setRenamingWorkspace] = useState<string | null>(null);
  const [renameValue, setRenameValue] = useState('');
  const [confirmDeleteWorkspace, setConfirmDeleteWorkspace] = useState<WorkspaceInfo | null>(null);
  const [renamingSession, setRenamingSession] = useState<string | null>(null);
  const [renameSessionValue, setRenameSessionValue] = useState('');
  const [showNewWorkspace, setShowNewWorkspace] = useState(false);
  const [newWorkspacePath, setNewWorkspacePath] = useState('');
  const [wsError, setWsError] = useState<string | null>(null);
  /** Bumped on every sidebar "new conversation" request so a mounted CodingView
   *  can reset the composer (stream, staged mode, focus). */
  const [newConvSeq, setNewConvSeq] = useState(0);

  const showError = useCallback((msg: string) => {
    setWsError(msg);
    setTimeout(() => setWsError(null), 4000);
  }, []);

  // In-chat toast bridge: CodingView registers addMessage while mounted.
  const viewNotifierRef = useRef<((text: string, kind: string) => void) | null>(null);
  const registerNotifier = useCallback((fn: ((text: string, kind: string) => void) | null) => {
    viewNotifierRef.current = fn;
  }, []);
  // Per-session view flags (isolation toggle) live in CodingView; the provider
  // resets them through this bridge on fresh-conversation requests.
  const isolationResetRef = useRef<(() => void) | null>(null);
  const registerIsolationReset = useCallback((fn: (() => void) | null) => {
    isolationResetRef.current = fn;
  }, []);
  const notifyView = useCallback((text: string, kind: string) => {
    try { viewNotifierRef.current?.(text, kind); } catch { /* view raced unmount */ }
  }, []);

  const loadWorkspaces = useCallback(async () => {
    try { setWorkspaces(await window.starriver.listWorkspaces()); } catch (e) { console.error(e); }
  }, []);

  const loadSessions = useCallback(async () => {
    try {
      // Grouped (workspace) sessions live here. Sessions with no workspaceId
      // are Chat conversations — they render only in the Chat tab. Sessions
      // whose workspace was deleted stay on disk but no longer render here.
      const all = await window.starriver.listSessions();
      sessionsRef.current = all;
      setSessions(all);
    } catch (e) { console.error(e); }
    // Sync the archive list, keeping the sidebar Archived section count current
    try { setArchivedSessions(await window.starriver.listArchivedSessions()); }
    catch (e) { console.error(e); }
  }, []);

  const handleRenameSubmit = useCallback(async (workspaceId: string) => {
    const title = renameValue.trim();
    setRenamingWorkspace(null);
    if (!title) return;
    try {
      await window.starriver.renameWorkspace(workspaceId, title);
      // Reflect locally: the registry round-trip is authoritative but an
      // immediate refetch would flash stale rows mid-rename.
      setWorkspaces(prev => prev.map(w => w.id === workspaceId ? { ...w, title } : w));
    } catch (e) { showError('Failed to rename workspace'); }
  }, [renameValue, showError]);

  const activateWorkspace = useCallback(async (workspaceId: string) => {
    await window.starriver.setActiveWorkspace(workspaceId, CODING_AGENT_NAME);
    setActiveWorkspaceId(workspaceId);
    await loadSessions();
    // Task flag + instructions banner + git status are view-side effects keyed
    // on activeWorkspaceId inside CodingView.
  }, [loadSessions]);

  // WI-179: running-session set — merge the two run registries: scheduled
  // active runs (jobs.list carries the run's session_id) and interactive
  // runs (sessions.running / host single-input slot). Polled every 3s while
  // the page is visible; the dot goes dark on the first poll after the run
  // settles. Failures keep the previous set (a blip must not flash dots off).
  const [runningSessionIds, setRunningSessionIds] = useState<Set<string>>(() => new Set());
  // WI-179 follow-up: the create/Run-Now pokes only cover those two trigger
  // paths — a scheduler-fired run had NO session-list refresher at all, so
  // its sidebar row waited for an unrelated reload and the user saw it
  // appear only at turn end (the host attaches the projcache row at run
  // start; listing is not the problem). While the poll runs, any running id
  // missing from the session list forces a list refresh, so the row
  // materializes within one tick regardless of what triggered the run; the
  // non-empty → empty transition refreshes once more so final titles land.
  const runningIdsRef = useRef<Set<string>>(new Set());
  const refreshRunningSessions = useCallback(async () => {
    const ids: string[] = [];
    let jobsOk = false;
    let runningOk = false;
    try {
      const jobs = (await Promise.resolve(window.starriver.listJobs?.() ?? [])) as JobInfo[];
      for (const j of Array.isArray(jobs) ? jobs : []) {
        if (j?.status === 'running' && j.sessionId) ids.push(j.sessionId);
      }
      jobsOk = true;
    } catch { /* jobs registry unreachable — keep previous set */ }
    try {
      const running = await Promise.resolve(window.starriver.listRunningSessions?.() ?? []);
      if (Array.isArray(running)) ids.push(...running);
      runningOk = true;
    } catch { /* ignore */ }
    // Both registries down: keep the previous set, skip refresh decisions.
    if (!jobsOk && !runningOk) return;
    const prev = runningIdsRef.current;
    const next = new Set(ids);
    const changed = prev.size !== next.size || [...next].some(id => !prev.has(id));
    const missingRow = [...next].some(id => !sessionsRef.current.some(s => s.id === id));
    if (changed) {
      runningIdsRef.current = next;
      setRunningSessionIds(next);
    }
    if (missingRow || (changed && next.size === 0 && prev.size > 0)) {
      void loadSessions();
    }
  }, [loadSessions]);
  useEffect(() => {
    void refreshRunningSessions();
    const timer = setInterval(() => {
      if (document.visibilityState !== 'visible') return;
      void refreshRunningSessions();
    }, 3000);
    return () => clearInterval(timer);
  }, [refreshRunningSessions]);

  // Clicking a workspace header activates it; clicking the active one again
  // toggles its section folded (sessions stay visible on first activation).
  // WI-152 guard (2026-10-05): with a session OPEN, clicking another group is
  // navigation only — fold/unfold, never activation. The active-workspace
  // binding belongs to the open session: the bottom status bar renders from
  // it, and the host resolves the session's next-turn cwd from the same
  // binding, so letting a group click steal it (starriver session + nexplan
  // click) flipped the status bar AND would have run the session's next turn
  // in the wrong directory. The New Task screen (no open session) keeps
  // click-to-activate: there the pick is the default for the next task.
  const toggleWorkspace = useCallback(async (workspaceId: string) => {
    const openWs = activeSessionId
      ? sessionsRef.current.find(s => s.id === activeSessionId)?.workspaceId ?? null
      : null;
    const navigateOnly = !!activeSessionId && workspaceId !== openWs;
    if (activeWorkspaceId !== workspaceId && !navigateOnly) {
      await activateWorkspace(workspaceId);
      return;
    }
    setCollapsedWorkspaces(prev => {
      const next = new Set(prev);
      if (next.has(workspaceId)) { next.delete(workspaceId); }
      else { next.add(workspaceId); }
      return next;
    });
  }, [activeSessionId, activeWorkspaceId, activateWorkspace]);

 // Register a workspace from an existing directory (no mkdir, the title
  // defaults to the basename). Picking a directory in the dialog creates it;
  // the returned registry row is appended locally and the standard activation
  // path runs (session list refresh + workspace-instructions banner).
  const handleCreateWorkspace = useCallback(async (path: string) => {
    let ws;
    try {
      ws = await window.starriver.createWorkspace(path);
    } catch (e: any) {
      showError(e?.message || 'Failed to add workspace');
      return;
    }
    setShowNewWorkspace(false);
    setNewWorkspacePath('');
    setWorkspaces(prev => prev.some(w => w.id === ws.id) ? prev : [...prev, ws]);
    await activateWorkspace(ws.id);
  }, [activateWorkspace, showError]);

  const handleDeleteWorkspace = useCallback((workspaceId: string, e: React.MouseEvent) => {
    e.stopPropagation();
    const w = workspaces.find(x => x.id === workspaceId);
    if (w) setConfirmDeleteWorkspace(w);
  }, [workspaces]);

  const handleConfirmDelete = useCallback(async () => {
    if (!confirmDeleteWorkspace) return;
    const workspaceId = confirmDeleteWorkspace.id;
    try {
      await window.starriver.deleteWorkspace(workspaceId);
      if (activeWorkspaceId === workspaceId) {
        setActiveWorkspaceId(null);
        // Closing the active session clears the message stream via CodingView's
        // sync effect; the instructions banner clears via its workspace effect.
        setActiveSessionId('');
        await window.starriver.setActiveWorkspace(null, CODING_AGENT_NAME);
      }
      setConfirmDeleteWorkspace(null);
      await Promise.all([loadWorkspaces(), loadSessions()]);
    } catch (err) { console.error(err); }
  }, [confirmDeleteWorkspace, activeWorkspaceId, loadWorkspaces, loadSessions, setActiveSessionId, setActiveWorkspaceId]);

  // Start a new conversation inside a workspace. If that workspace isn't the
  // active one, activate it first — otherwise the click silently no-ops.
  const requestNewConversation = useCallback(async (workspaceId: string) => {
    if (activeWorkspaceId !== workspaceId) {
      try {
        await activateWorkspace(workspaceId);
      } catch (e) {
        console.error('[Workspace] Activate workspace failed:', e);
        return;
      }
    }
    isolationResetRef.current?.();
    setActiveSessionId('');
    setNewConvSeq(n => n + 1);
  }, [activeWorkspaceId, activateWorkspace, setActiveSessionId]);

  // Sidebar archive button: closing the active session clears the stream via
  // CodingView's sync effect.
  const archiveSession = useCallback(async (sessionId: string) => {
    if (activeSessionId === sessionId) setActiveSessionId('');
    try { await window.starriver.archiveSession(sessionId); } catch { return; }
    await loadSessions();
  }, [activeSessionId, setActiveSessionId, loadSessions]);

  // Click an archived row (or the modal's restore) → back to the default list.
  const handleUnarchive = useCallback(async (sessionId: string) => {
    try {
      await window.starriver.unarchiveSession(sessionId);
      setArchivedSessions(prev => prev.filter(s => s.id !== sessionId));
      notifyView(`📦 ${t('thread.unarchived')}`, 'unarchive');
      await loadSessions();
    } catch (e) { showError(t('thread.unarchiveFailed')); }
  }, [loadSessions, notifyView, showError, t]);

  // Double-click a session title → submit an inline rename.
  const handleRenameSession = useCallback(async (sessionId: string) => {
    const title = renameSessionValue.trim();
    setRenamingSession(null);
    if (!title) return;
    try {
      await window.starriver.renameSession(sessionId, title);
      notifyView(`✏ ${t('thread.renamed')}`, 'name');
      await loadSessions();
    } catch (e) { showError(t('thread.renameFailed')); }
  }, [renameSessionValue, loadSessions, notifyView, showError, t]);

  const toggleArchivedGroup = useCallback((key: string) =>
    setArchivedOpenKeys(prev => ({ ...prev, [key]: !prev[key] })), []);

  useEffect(() => {
    loadWorkspaces(); loadSessions();
  }, [loadWorkspaces, loadSessions]);

  // FPM P4b: after a daemon crash-restart, main broadcasts connected — refresh
  // the workspace/session snapshots (state persists in SQLite, so a reconnect
  // restores the scene). Embedded mode does not broadcast; no side effects.
  useEffect(() => {
    const off = (window.starriver as any).onDaemonStatus?.((status: any) => {
      if (status?.state === 'connected') {
        void loadWorkspaces();
        void loadSessions();
      }
    });
    return () => off?.();
  }, [loadWorkspaces, loadSessions]);

  const value: WorkspaceCtx = {
    workspaces, sessions, archivedSessions,
    activeWorkspaceId, setActiveWorkspaceId,
    activeSessionId, setActiveSessionId,
    collapsedWorkspaces,
    archivedOpenKeys, toggleArchivedGroup,
    renamingWorkspace, setRenamingWorkspace, renameValue, setRenameValue,
    renamingSession, setRenamingSession, renameSessionValue, setRenameSessionValue,
    showNewWorkspace, setShowNewWorkspace, newWorkspacePath, setNewWorkspacePath,
    confirmDeleteWorkspace, setConfirmDeleteWorkspace,
    loadSessions, toggleWorkspace, activateWorkspace, requestNewConversation, archiveSession,
    runningSessionIds, refreshRunningSessions,
    handleUnarchive, handleRenameSession, handleRenameSubmit, handleCreateWorkspace,
    handleDeleteWorkspace, handleConfirmDelete,
    newConvSeq, wsError, showError, registerNotifier, registerIsolationReset, isolationResetRef,
  };

  return <WorkspaceContext.Provider value={value}>{children}</WorkspaceContext.Provider>;
}

// The persistent sidebar section: Workspaces tree (foldable via the header —
// the open/closed folder glyph is the indicator; no triangle carets).
export function WorkspaceSection({ onOpenCoding }: { onOpenCoding: () => void }) {
  const { t } = useI18n();
  const ws = useWorkspaces();
  // Sessions spawned by scheduled tasks (jobs.taskRuns projection) — they get
  // their own folder under Tasks, apart from plain chat conversations.
  // WI-180: keep the TASK NAME per session too — sessions created before the
  // backend stamped fallback titles (and any that never get one) still render
  // a meaningful row instead of the bare "Chat".
  const [taskRuns, setTaskRuns] = useState<Map<string, string>>(new Map());
  useEffect(() => {
    Promise.resolve(window.starriver.listAllTaskRuns?.() ?? [])
      .then(runs => {
        const next = new Map<string, string>();
        for (const r of runs) {
          if (r.sessionId && !next.has(r.sessionId)) next.set(r.sessionId, r.taskName);
        }
        setTaskRuns(next);
      })
      .catch(() => {});
  }, [ws.sessions]);
  // Row label: real title → owning task's name → generic fallback.
  const sessionLabel = (s: SessionInfo) => s.title || taskRuns.get(s.id) || 'Chat';
  // Folder fold state for the Tasks / Ungrouped sections (same interaction as
  // the workspace groups above).
  const [foldedFolders, setFoldedFolders] = useState<Record<string, boolean>>({});
  const toggleFolder = (key: string) => setFoldedFolders(prev => ({ ...prev, [key]: !prev[key] }));
  return (
    <div className="sb-ws">
      {/* Section label: static (no icon, no fold, no counter) — only the
          workspace rows below carry the folder glyph and fold behavior. */}
      <div className="sb-ws-header">
        <span className="sb-ws-title">{t('workspace.title')}</span>
        <button
          className="sb-ws-add"
          onClick={() => ws.setShowNewWorkspace(true)}
          data-tip={t('workspace.new')}
          aria-label={t('workspace.new')}>
          +
        </button>
      </div>
      <div className="sb-ws-body">
          {ws.wsError && <div className="sb-ws-error">{ws.wsError}</div>}
          {ws.workspaces.length === 0 && ws.sessions.length === 0 && !ws.showNewWorkspace && (
            <div className="sb-ws-empty">
              <div>{t('workspace.empty')}</div>
              <div className="sb-ws-empty-hint">{t('workspace.emptyHint')}</div>
            </div>
          )}

          {ws.workspaces.map(w => {
            const isExpanded = !ws.collapsedWorkspaces.has(w.id);
            const workspaceSessions = ws.sessions.filter(s => s.workspaceId === w.id);
            return (
              <div key={w.id}>
                {/* Workspace header: click activates (or folds when already active) */}
                <div
                  className={`coding-group-header sb-ws-group${ws.activeWorkspaceId === w.id ? ' is-active' : ''}`}
                  onClick={() => ws.toggleWorkspace(w.id)}>
                  <div className="sb-ws-row">
                    <span className={'sb-ws-chevron' + (isExpanded ? ' open' : '')} aria-hidden="true" dangerouslySetInnerHTML={{ __html: ICON_CHEVRON_FOLD }} />
                    <span className="sb-ws-folder" dangerouslySetInnerHTML={{ __html: isExpanded ? ICON_FOLDER_OPEN : ICON_FOLDER_CLOSED }} />
                    {ws.renamingWorkspace === w.id ? (
                      <input
                        autoFocus
                        value={ws.renameValue}
                        onChange={(e) => ws.setRenameValue(e.target.value)}
                        onKeyDown={(e) => { if (e.key === 'Enter') ws.handleRenameSubmit(w.id); if (e.key === 'Escape') ws.setRenamingWorkspace(null); }}
                        onBlur={() => ws.setRenamingWorkspace(null)}
                        onClick={(e) => e.stopPropagation()}
                        className="sb-ws-rename"
                      />
                    ) : (
                      <span
                        className="sb-ws-name"
                        onDoubleClick={(e) => { e.stopPropagation(); ws.setRenamingWorkspace(w.id); ws.setRenameValue(w.title); }}
                        title={w.path}>
                        {w.title}
                      </span>
                    )}
                  </div>
                  <div className="sb-ws-actions">
                    <button
                      className="sb-ws-icon-btn"
                      onClick={(e) => {
                        e.stopPropagation();
                        try { localStorage.setItem('starriver-task-kind', 'coding'); } catch { /* storage unavailable */ }
                        ws.requestNewConversation(w.id);
                        onOpenCoding();
                      }}
                      data-tip={t('coding.newChat')}
                      aria-label={t('coding.newChat')}>
                      +
                    </button>
                    <button className="coding-group-delete" onClick={(e) => ws.handleDeleteWorkspace(w.id, e)}
                      data-tip={t('workspace.deleteTitle')}
                      aria-label={t('workspace.deleteTitle')}>
                    <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
                      <path d="M3 6h18" />
                      <path d="M8 6V4a1 1 0 0 1 1-1h6a1 1 0 0 1 1 1v2" />
                      <path d="M19 6l-1 14a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2L5 6" />
                      <path d="M10 11v6" />
                      <path d="M14 11v6" />
                    </svg>
                  </button>
                  </div>
                </div>

                {/* Sessions in workspace */}
                {isExpanded && (
                  <div>
                    {workspaceSessions.length === 0 && (
                      <div className="sb-ws-none">No conversations yet</div>
                    )}
                    {workspaceSessions.map(s => (
                      <div key={s.id}
                        className={`coding-session-row sb-ws-session${ws.activeSessionId === s.id ? ' is-active' : ''}`}
                        onClick={() => { ws.setActiveSessionId(s.id); onOpenCoding(); }}
                        data-tip={s.title || 'Coding task'}>
                        <div className="sb-ws-row">
                          {/* WI-179: blinking dot while the session has a live turn. */}
                          {ws.runningSessionIds.has(s.id) && (
                            <span className="sb-run-dot" data-tip={t('scheduled.runningTip')} aria-label={t('scheduled.runningTip')} />
                          )}
                          {ws.renamingSession === s.id ? (
                            <input
                              autoFocus
                              value={ws.renameSessionValue}
                              onChange={(e) => ws.setRenameSessionValue(e.target.value)}
                              onKeyDown={(e) => { if (e.key === 'Enter') ws.handleRenameSession(s.id); if (e.key === 'Escape') ws.setRenamingSession(null); }}
                              onBlur={() => ws.setRenamingSession(null)}
                              onClick={(e) => e.stopPropagation()}
                              className="sb-ws-rename"
                            />
                          ) : (
                            <span
                              className="sb-ws-session-name"
                              onDoubleClick={(e) => { e.stopPropagation(); ws.setRenamingSession(s.id); ws.setRenameSessionValue(s.title || ''); }}
                              title="Double-click to rename">
                              {s.title || 'Coding task'}
                            </span>
                          )}
                          <button
                            className="sb-ws-icon-btn"
                            onClick={(e) => { e.stopPropagation(); ws.archiveSession(s.id); }}
                            aria-label={t('thread.archive')}
                            data-tip={t('thread.archive')}>
                            <span dangerouslySetInnerHTML={{ __html: ICON_ARCHIVE }} />
                          </button>
                        </div>
                      </div>
                    ))}
                    {(() => {
                      const wsArchived = ws.archivedSessions.filter(s => s.workspaceId === w.id);
                      if (wsArchived.length === 0) return null;
                      const key = `ws:${w.id}`;
                      const open = !!ws.archivedOpenKeys[key];
                      return (
                        <div>
                          <div
                            className="coding-group-header sb-ws-archived"
                            onClick={() => ws.toggleArchivedGroup(key)}>
                            <span className={'sb-ws-chevron' + (open ? ' open' : '')} aria-hidden="true" dangerouslySetInnerHTML={{ __html: ICON_CHEVRON_FOLD }} />
                            <span className="sb-ws-folder" dangerouslySetInnerHTML={{ __html: open ? ICON_FOLDER_OPEN : ICON_FOLDER_CLOSED }} />
                            <span className="sb-ws-archived-label">{t('thread.archivedSection')} ({wsArchived.length})</span>
                          </div>
                          {open && wsArchived.map(s => (
                            <div key={s.id} className="coding-session-row sb-ws-archived-row"
                              onClick={() => ws.handleUnarchive(s.id)}
                              data-tip={s.title || 'Coding task'}>
                              <span className="sb-ws-session-name sb-ws-archived-name">{s.title || 'Coding task'}</span>
                              <button
                                className="sb-ws-icon-btn"
                                onClick={(e) => { e.stopPropagation(); ws.handleUnarchive(s.id); }}
                                data-tip={t('thread.unarchive')}
                                aria-label={t('thread.unarchive')}>
                                <span dangerouslySetInnerHTML={{ __html: ICON_UNARCHIVE }} />
                              </button>
                            </div>
                          ))}
                        </div>
                      );
                    })()}
                  </div>
                )}
              </div>
            );
          })}


          {/* Sessions whose workspace was deleted stay on disk and remain
              recoverable regardless of archived state: restore re-homes into
              the active workspace when one exists, else unarchives only (the
              row stays visible in this group either way). */}
          {(() => {
            const orphanWorkspaces = new Set(
              [...ws.sessions, ...ws.archivedSessions]
                .filter(s => s.workspaceId && !ws.workspaces.some(w => w.id === s.workspaceId))
                .map(s => s.workspaceId as string),
            );
            const archivedOrphans = ws.archivedSessions.filter(s => s.workspaceId && orphanWorkspaces.has(s.workspaceId) && !ws.sessions.some(x => x.id === s.id));
            const orphans: { id: string; title: string; archived: boolean }[] = [
              ...ws.sessions
                .filter(s => s.workspaceId && !ws.workspaces.some(w => w.id === s.workspaceId))
                .map(s => ({ id: s.id, title: s.title || 'Coding task', archived: false })),
              ...archivedOrphans.map(s => ({ id: s.id, title: s.title || 'Coding task', archived: true })),
            ];
            if (orphans.length === 0) return null;
            const candidate = ws.activeWorkspaceId ?? ws.workspaces[0]?.id;
            // The target must still exist — attaching to a stale id dies on
            // the FK and used to vanish without feedback.
            const validTarget = candidate && ws.workspaces.some(w => w.id === candidate) ? candidate : undefined;
            const restore = (sessionId: string) => {
              const call = validTarget
                ? window.starriver.assignSessionWorkspace(sessionId, validTarget)
                : Promise.resolve(window.starriver.unarchiveSession?.(sessionId));
              void Promise.resolve(call)
                .then(() => ws.loadSessions())
                .catch((e) => ws.showError(String(e?.message ?? e)));
            };
            return (
              <div>
                <div className="sb-ws-group sb-ws-ungrouped" style={{ cursor: 'pointer' }} onClick={() => toggleFolder('ungrouped')}
                  data-tip={t('workspace.ungroupedHint')}>
                  <div className="sb-ws-row">
                    <span className={'sb-ws-chevron' + (!foldedFolders.ungrouped ? ' open' : '')} aria-hidden="true" dangerouslySetInnerHTML={{ __html: ICON_CHEVRON_FOLD }} />
                    <span className="sb-ws-folder" dangerouslySetInnerHTML={{ __html: foldedFolders.ungrouped ? ICON_FOLDER_CLOSED : ICON_FOLDER_OPEN }} />
                    <span className="sb-ws-ungrouped-label">{t('workspace.ungrouped')}</span>
                  </div>
                </div>
                {!foldedFolders.ungrouped && orphans.map(s => (
                  <div key={s.id} className="coding-session-row sb-ws-archived-row sb-ws-ungrouped-row" data-tip={s.title}>
                    <span className="sb-ws-session-name">
                      {s.archived ? '📦 ' : ''}{s.title}
                    </span>
                    <button
                      className="sb-ws-icon-btn"
                      onClick={() => restore(s.id)}
                      data-tip={validTarget ? t('workspace.ungroupedRestore') : t('workspace.ungroupedRestoreNoTarget')}
                      aria-label={t('workspace.ungroupedRestore')}>
                      <span dangerouslySetInnerHTML={{ __html: ICON_UNARCHIVE }} />
                    </button>
                  </div>
                ))}
              </div>
            );
          })()}

          {/* Tasks: chats and scheduled runs that live outside any workspace,
              as two folders under one Tasks header (same level as Workspace).
              Rows reopen with the agent that started them (host write-once
              stamp; legacy ungrouped rows fall back to chat). */}
          {(() => {
            const ungrouped = ws.sessions.filter(s => !s.workspaceId);
            // WI-152: scheduled-run 分组 —— run_history 投影命中,或会话自带
            // scheduled origin 戳;旧会话(记录已滚出)按任务名兜底(标题在
            // 创建时被精确盖为任务名)。
            const taskNames = new Set(Array.from(taskRuns.values()));
            const isRun = (s: SessionInfo) =>
              taskRuns.has(s.id) || s.origin === 'scheduled' || (!!s.title && taskNames.has(s.title));
            const chats = ungrouped.filter(s => !isRun(s));
            const runs = ungrouped.filter(isRun);
            if (chats.length === 0 && runs.length === 0) return null;
            const folder = (key: string, label: string, rows: SessionInfo[]) => {
              const folded = !!foldedFolders[key];
              return (
              <div>
                <div className="sb-ws-group sb-ws-ungrouped" style={{ cursor: 'pointer' }} onClick={() => toggleFolder(key)}>
                  <div className="sb-ws-row">
                    <span className={'sb-ws-chevron' + (!folded ? ' open' : '')} aria-hidden="true" dangerouslySetInnerHTML={{ __html: ICON_CHEVRON_FOLD }} />
                    <span className="sb-ws-folder" dangerouslySetInnerHTML={{ __html: folded ? ICON_FOLDER_CLOSED : ICON_FOLDER_OPEN }} />
                    <span className="sb-ws-ungrouped-label">{label}</span>
                  </div>
                </div>
                {!folded && rows.map(s => (
                  <div key={s.id}
                    className={`coding-session-row sb-ws-session${ws.activeSessionId === s.id ? ' is-active' : ''}`}
                    onClick={() => { ws.setActiveSessionId(s.id); onOpenCoding(); }}
                    data-tip={sessionLabel(s)}>
                    <div className="sb-ws-row">
                      {/* WI-179: blinking dot while the session has a live turn. */}
                      {ws.runningSessionIds.has(s.id) && (
                        <span className="sb-run-dot" data-tip={t('scheduled.runningTip')} aria-label={t('scheduled.runningTip')} />
                      )}
                      <span className="sb-ws-session-name">{sessionLabel(s)}</span>
                    </div>
                  </div>
                ))}
              </div>
            );
            };
            return (
              <div>
                <div className="sb-ws-header" style={{ marginTop: 14 }}>
                  <span className="sb-ws-title">{t('workspace.tasks')}</span>
                </div>
                {folder('chats', t('workspace.tasksChat'), chats)}
                {runs.length > 0 && folder('scheduled', t('workspace.tasksScheduled'), runs)}
              </div>
            );
          })()}
      </div>

      {/* Add workspace dialog: pick an existing directory, title defaults to basename */}
      {ws.showNewWorkspace && (
        <div className="modal-overlay" onClick={() => { ws.setShowNewWorkspace(false); ws.setNewWorkspacePath(''); }}>
          <div className="modal-card" onClick={(e) => e.stopPropagation()}>
            <div className="modal-title">{t('workspace.addTitle')}</div>
            <div className="modal-path-row">
              <input
                autoFocus
                value={ws.newWorkspacePath}
                onChange={(e) => ws.setNewWorkspacePath(e.target.value)}
                placeholder={t('workspace.pickHint')}
                onKeyDown={(e) => { if (e.key === 'Enter' && ws.newWorkspacePath.trim()) ws.handleCreateWorkspace(ws.newWorkspacePath.trim()); if (e.key === 'Escape') { ws.setShowNewWorkspace(false); ws.setNewWorkspacePath(''); } }}
              />
              <button className="modal-btn" onClick={async (e) => {
                e.preventDefault();
                const dir = await window.starriver.selectDirectory();
                if (dir) ws.handleCreateWorkspace(dir);
              }}>{t('workspace.browse')}</button>
            </div>
            <div className="modal-hint">{t('workspace.pickHint')}</div>
            <div className="modal-actions">
              <button className="modal-btn" onClick={() => { ws.setShowNewWorkspace(false); ws.setNewWorkspacePath(''); }}>{t('common.cancel')}</button>
              <button className="modal-btn primary" onClick={() => ws.handleCreateWorkspace(ws.newWorkspacePath.trim())} disabled={!ws.newWorkspacePath.trim()}>{t('common.create')}</button>
            </div>
          </div>
        </div>
      )}

      {ws.confirmDeleteWorkspace && (
        <ConfirmDeleteModal
          workspace={ws.confirmDeleteWorkspace}
          sessionCount={ws.sessions.filter(s => s.workspaceId === ws.confirmDeleteWorkspace!.id).length}
          onConfirm={ws.handleConfirmDelete}
          onCancel={() => ws.setConfirmDeleteWorkspace(null)}
        />
      )}
    </div>
  );
}


/* ─── 界面定时锁（Scheduled UI Lock）───
 * 每日锁定期 + 每次随机 10 位字母数字口令的自律型锁屏。锁屏层只是
 * fixed 覆盖层：下方应用树保持挂载，daemon 会话与事件流不受影响
 * （"锁屏期间会话继续运行"）。纯逻辑见 ./uiLock；日程持久化在
 * localStorage（starriver.uiLock），tick 时重读 — 设置改动即时生效。
 * 定位是"继续使用的刻意摩擦"，不是安全边界：口令展示在锁屏上，
 * DevTools 移除覆盖层不在防御范围。 */

/** useUiLock：挂载即判定 + 15s tick + 聚焦/可见性补判定（休眠唤醒追上）。 */
function useUiLock(): { locked: boolean; code: string; start: string; end: string; unlock: () => void } {
  const [locked, setLocked] = useState(false);
  const [code, setCode] = useState('');
  const [win, setWin] = useState(() => {
    const s = loadUiLockSchedule();
    return { start: s.start, end: s.end };
  });
  // 已解锁的 occurrence（本窗口内不再重锁）；当前口令所属 occurrence。
  const unlockedOccRef = useRef<string | null>(null);
  const currentOccRef = useRef<string | null>(null);
  const lastScheduleRef = useRef<string>('');

  const tick = useCallback(() => {
    const schedule = loadUiLockSchedule();
    const sig = `${schedule.enabled}|${schedule.start}|${schedule.end}`;
    if (sig !== lastScheduleRef.current) {
      // 日程变更（含首次读取）：清空解锁记忆，按新日程重新评估。
      lastScheduleRef.current = sig;
      unlockedOccRef.current = null;
      currentOccRef.current = null;
    }
    setWin((prev) => (prev.start === schedule.start && prev.end === schedule.end ? prev : { start: schedule.start, end: schedule.end }));
    const verdict = evaluateUiLockTick(schedule, new Date(), unlockedOccRef.current, currentOccRef.current);
    if (verdict) {
      currentOccRef.current = verdict.occurrenceId;
      if (verdict.relock) setCode(generateLockCode()); // 每个 occurrence 只生成一次新口令
      setLocked(true);
    } else {
      setLocked(false);
    }
  }, []);

  useEffect(() => {
    tick();
    const id = window.setInterval(tick, LOCK_TICK_MS);
    const catchUp = () => tick();
    window.addEventListener('focus', catchUp);
    document.addEventListener('visibilitychange', catchUp);
    return () => {
      window.clearInterval(id);
      window.removeEventListener('focus', catchUp);
      document.removeEventListener('visibilitychange', catchUp);
    };
  }, [tick]);

  const unlock = useCallback(() => {
    unlockedOccRef.current = currentOccRef.current; // 本 occurrence 内不再重锁
    setLocked(false);
  }, []);

  return { locked, code, start: win.start, end: win.end, unlock };
}

export function LockOverlay({ code, start, end, onUnlock }: {
  code: string; start: string; end: string; onUnlock: () => void;
}) {
  const { t } = useI18n();
  const [value, setValue] = useState('');
  const [error, setError] = useState(false);
  const [copied, setCopied] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);
  const valueRef = useRef('');
  valueRef.current = value;

  useEffect(() => { inputRef.current?.focus(); }, []);

  const submit = () => {
    if (valueRef.current.trim() === code) {
      onUnlock();
    } else {
      setError(true);
      setValue('');
      inputRef.current?.focus();
      window.setTimeout(() => setError(false), 400);
    }
  };
  const copy = () => {
    try { void navigator.clipboard?.writeText(code); } catch { /* clipboard 不可用 — 忽略 */ }
    setCopied(true);
    window.setTimeout(() => setCopied(false), 1500);
  };

  return (
    <div className="ui-lock-overlay" data-testid="ui-lock-overlay" role="dialog" aria-modal="true" aria-label={t('uiLock.lockedTitle')}>
      <div className="ui-lock-card">
        <div style={{ fontSize: 30, lineHeight: 1 }}>🔒</div>
        <h1 style={{ fontSize: 17, margin: '8px 0 4px' }}>{t('uiLock.lockedTitle')}</h1>
        <p style={{ fontSize: 12.5, color: 'var(--text-muted)', margin: '0 0 16px', lineHeight: 1.6 }}>
          {t('uiLock.lockedDesc')}<br />
          {t('uiLock.window', { start, end })}
        </p>
        <div style={{ fontSize: 12, color: 'var(--text-muted)', textAlign: 'left', marginBottom: 6 }}>{t('uiLock.codeLabel')}</div>
        <div className="ui-lock-code-row">
          <div className="ui-lock-code" data-testid="ui-lock-code">{code}</div>
          <button type="button" className="ui-lock-copy" onClick={copy}>{copied ? t('uiLock.copied') : t('uiLock.copy')}</button>
        </div>
        <input
          ref={inputRef}
          className={'ui-lock-input' + (error ? ' ui-lock-shake' : '')}
          value={value}
          onChange={(e) => setValue(e.target.value)}
          onKeyDown={(e) => { if (e.key === 'Enter') submit(); }}
          placeholder={t('uiLock.inputLabel')}
          aria-label={t('uiLock.inputLabel')}
          autoComplete="off"
          spellCheck={false}
        />
        <button type="button" className="ui-lock-unlock" onClick={submit}>{t('uiLock.unlock')}</button>
        <div className="ui-lock-error" data-testid="ui-lock-error">{error ? t('uiLock.wrongCode') : ''}</div>
      </div>
    </div>
  );
}

/** App 三条返回路径（checking / error / shell 替换 / 主 shell）末尾统一挂载；
 *  未锁定时渲染 null，锁定时覆盖全屏。 */
export function UiLockGate() {
  const lock = useUiLock();
  if (!lock.locked) return null;
  return <LockOverlay code={lock.code} start={lock.start} end={lock.end} onUnlock={lock.unlock} />;
}

/** 设置 → 常规 → 定时锁分区：启用开关 + 起止时间，合法变更即时持久化
 *  （无保存按钮，与 localStorage 分区惯例一致）；无效/相同时间阻断保存。 */
export function UiLockScheduleSection() {
  const { t } = useI18n();
  const [schedule, setSchedule] = useState<UiLockSchedule>(() => {
    const s = loadUiLockSchedule();
    // 兜底：本地存储可能被外部改坏 — 坏日程（start===end 等）回到默认，
    // 让用户从已知状态重新配置。
    return s.start === s.end ? { ...DEFAULT_UI_LOCK_SCHEDULE } : s;
  });
  const [errKey, setErrKey] = useState<string | null>(null);

  const update = (patch: Partial<UiLockSchedule>) => {
    const next = { ...schedule, ...patch };
    if (!isValidTime(next.start) || !isValidTime(next.end) || next.start === next.end) {
      setErrKey('uiLock.invalidRange'); // 无效/相同：行内报错且不落库
      return;
    }
    setErrKey(null);
    setSchedule(next);
    saveUiLockSchedule(next);
  };
  const timeInputStyle: React.CSSProperties = {
    width: '100%', marginTop: 4, padding: '6px 8px', border: '1px solid var(--border-strong)',
    borderRadius: 6, fontSize: 13, background: 'var(--surface)', color: 'var(--text)',
  };

  return (
    <div data-testid="ui-lock-section" style={{ borderTop: '1px solid var(--border)', marginTop: 16, paddingTop: 14 }}>
      <div style={{ fontSize: 14, fontWeight: 600, color: 'var(--text)', marginBottom: 6 }}>{t('uiLock.title')}</div>
      <div style={{ fontSize: 12.5, color: 'var(--text-muted)', lineHeight: 1.55, marginBottom: 10 }}>{t('uiLock.desc')}</div>
      <label style={{ display: 'flex', alignItems: 'center', gap: 8, cursor: 'pointer', marginBottom: 10, fontSize: 13.5 }}>
        <input type="checkbox" aria-label="ui-lock-enabled" checked={schedule.enabled} onChange={(e) => update({ enabled: e.target.checked })} />
        {t('uiLock.enable')}
      </label>
      <div style={{ display: 'flex', gap: 12 }}>
        <label style={{ flex: 1, fontSize: 12, color: 'var(--text-muted)' }}>
          {t('uiLock.start')}
          <input type="time" aria-label="ui-lock-start" value={schedule.start} onChange={(e) => update({ start: e.target.value })} style={timeInputStyle} />
        </label>
        <label style={{ flex: 1, fontSize: 12, color: 'var(--text-muted)' }}>
          {t('uiLock.end')}
          <input type="time" aria-label="ui-lock-end" value={schedule.end} onChange={(e) => update({ end: e.target.value })} style={timeInputStyle} />
        </label>
      </div>
      <div data-testid="ui-lock-error" style={{ color: 'var(--danger)', fontSize: 12, minHeight: 15, marginTop: 4 }}>
        {errKey ? t(errKey) : ''}
      </div>
    </div>
  );
}


/* ─── SettingsDialog ─── */

/// Remote-companion pairing (iPhone): expose the protocol server address +
/// token as a starriver://pair QR code and copyable payloads.
function RemoteAccessSection() {
  const { t } = useI18n();
  const [info, setInfo] = useState<{ port: number; token: string } | null>(null);
  const [lan, setLan] = useState<string>('127.0.0.1');
  const [qrSvg, setQrSvg] = useState<string>('');
  const [copied, setCopied] = useState<string>('');

  useEffect(() => {
    Promise.resolve(window.starriver.httpServerInfo?.())
      .then((raw) => { if (raw) setInfo(JSON.parse(raw)); })
      .catch((e) => console.error('[pairing] httpServerInfo failed:', e));
    Promise.resolve(window.starriver.lanAddress?.())
      .then(setLan)
      .catch(() => {});
  }, []);
  const startServer = () => {
    Promise.resolve(window.starriver.startHttpServer?.())
      .then((raw) => setInfo(JSON.parse(raw)))
      .catch((e) => console.error('[pairing] startHttpServer failed:', e));
  };
  const pairingUrl = info
    ? `starriver://pair?v=1&host=${encodeURIComponent(lan)}&port=${info.port}&token=${info.token}`
    : '';
  useEffect(() => {
    if (!pairingUrl) { setQrSvg(''); return; }
    import('qrcode')
      .then((qr) => qr.toString(pairingUrl, { type: 'svg', margin: 1, width: 148 }))
      .then(setQrSvg)
      .catch((e) => console.error('[pairing] qr failed:', e));
  }, [pairingUrl]);
  const copy = (text: string, tag: string) => {
    navigator.clipboard?.writeText(text).then(() => {
      setCopied(tag);
      setTimeout(() => setCopied(''), 1500);
    }).catch(() => {});
  };
  const pairingJson = info ? JSON.stringify({ host: lan, port: info.port, token: info.token }) : '';

  return (
    <div>
      {!info ? (
        <>
          <div style={{ fontSize: 13, color: 'var(--text-muted)', marginBottom: 8 }}>{t('settings.remoteHint')}</div>
          <button onClick={startServer} style={{ padding: '6px 14px', border: '1px solid var(--border-strong)', borderRadius: 6, background: 'var(--surface)', color: 'var(--text)', fontSize: 14, cursor: 'pointer' }}>
            {t('settings.remoteStart')}
          </button>
        </>
      ) : (
        <div style={{ display: 'flex', gap: 14, alignItems: 'flex-start' }}>
          {qrSvg && (
            <div className="qr-box" dangerouslySetInnerHTML={{ __html: qrSvg }} />
          )}
          <div style={{ flex: 1, minWidth: 0, fontSize: 13, color: 'var(--text-secondary)', lineHeight: 1.7 }}>
            <div>{t('settings.remoteAddr')}: <code style={{ fontFamily: 'var(--mono)' }}>{lan}:{info.port}</code></div>
            <div style={{ wordBreak: 'break-all', userSelect: 'text' }}>{t('settings.remoteToken')}: <code style={{ fontFamily: 'var(--mono)' }}>{info.token}</code></div>
            <div style={{ display: 'flex', gap: 8, marginTop: 6, flexWrap: 'wrap' }}>
              <button onClick={() => copy(info.token, 'token')} style={copyBtn}>{copied === 'token' ? t('settings.copied') : t('settings.copyToken')}</button>
              <button onClick={() => copy(pairingUrl, 'url')} style={copyBtn}>{copied === 'url' ? t('settings.copied') : t('settings.copyPairUrl')}</button>
              <button onClick={() => copy(pairingJson, 'json')} style={copyBtn}>{copied === 'json' ? t('settings.copied') : t('settings.copyPairJson')}</button>
            </div>
            <div style={{ marginTop: 6, fontSize: 12, color: 'var(--text-muted)' }}>{t('settings.remoteNote')}</div>
          </div>
        </div>
      )}
    </div>
  );
}
const copyBtn: React.CSSProperties = {
  padding: '4px 10px', border: '1px solid var(--border-strong)', borderRadius: 6,
  background: 'var(--surface)', color: 'var(--text)', fontSize: 13, cursor: 'pointer',
};

/* ─── Settings hot-update sections (WI-4: settings sections / model-provider
 *  config / plugin inventory — first consumers of settings.get/update) ─── */

const settingsInputStyle: React.CSSProperties = {
  width: '100%', padding: '6px 10px', border: '1px solid var(--border-strong)', borderRadius: 5,
  fontSize: 13, background: 'var(--surface)', color: 'var(--text)', outline: 'none', boxSizing: 'border-box',
};

function SettingsField({ label, hint, children }: {
  label: string; hint?: string; children: React.ReactNode;
}) {
  return (
    <div>
      <div style={{ fontSize: 13, fontWeight: 600, color: 'var(--text)', marginBottom: 4 }}>{label}</div>
      {children}
      {hint && <div style={{ fontSize: 12, color: 'var(--text-muted)', marginTop: 3 }}>{hint}</div>}
    </div>
  );
}

/** Shared save-row status line. */
function SaveStatusLine({ status, message, savedText }: {
  status: 'idle' | 'saving' | 'saved' | 'error'; message: string; savedText: string;
}) {
  if (status === 'idle') return null;
  const color = status === 'error' ? 'var(--danger)' : status === 'saved' ? 'var(--success)' : 'var(--text-muted)';
  const text = status === 'error' ? message : status === 'saving' ? '…' : savedText;
  return <span data-testid="settings-save-status" style={{ fontSize: 12, color }}>{text}</span>;
}

/** Number-or-null field semantics: empty input clears the override (the
 *  patch sends null = fall back to the default); non-numeric or negative
 *  input blocks the save instead of silently coercing. */
function numOrNull(raw: string): number | null | 'invalid' {
  const s = raw.trim();
  if (!s) return null;
  const n = Number(s);
  return Number.isFinite(n) && n >= 0 ? n : 'invalid';
}

/** Agent/prompt namespace editor: reads via settings.get, saves a deep-merge
 *  patch via settings.update. The host validates patches wholesale, so a
 *  rejected save keeps the form state and surfaces the error here. */
export function SettingsAgentSection() {
  const { t } = useI18n();
  const [f, setF] = useState({
    maxParallel: '', turnBudget: '', compactionRatio: '', timeRefresh: '',
    instructionsMaxBytes: '', persona: '', includeIdentity: true, includeRuntime: true,
  });
  const [loadError, setLoadError] = useState<string | null>(null);
  const [status, setStatus] = useState<'idle' | 'saving' | 'saved' | 'error'>('idle');
  const [message, setMessage] = useState('');

  const fromSettings = useCallback((s: any) => setF({
    maxParallel: s?.agent?.max_parallel_tool_calls?.toString() ?? '',
    turnBudget: s?.agent?.turn_tool_call_budget?.toString() ?? '',
    compactionRatio: s?.agent?.compaction_threshold_ratio?.toString() ?? '',
    timeRefresh: s?.agent?.time_refresh_interval_ms?.toString() ?? '',
    instructionsMaxBytes: s?.prompt?.instructions_max_bytes?.toString() ?? '',
    persona: s?.prompt?.persona_global ?? '',
    includeIdentity: s?.prompt?.include_identity !== false,
    includeRuntime: s?.prompt?.include_runtime_context !== false,
  }), []);

  useEffect(() => {
    Promise.resolve(window.starriver.getSettings?.())
      .then(fromSettings)
      .catch((e) => setLoadError(String(e?.message || e)));
  }, [fromSettings]);

  const save = () => {
    // agent.context_window is intentionally NOT editable here: per-provider
    // context windows live in the Models settings (llm.providers[].context_window);
    // the global default stays whatever settings.yaml carries (deep-merge
    // preserves it — update_from_patch only rewrites patched keys).
    const agentNums = {
      max_parallel_tool_calls: numOrNull(f.maxParallel),
      turn_tool_call_budget: numOrNull(f.turnBudget),
      compaction_threshold_ratio: numOrNull(f.compactionRatio),
      time_refresh_interval_ms: numOrNull(f.timeRefresh),
    };
    const instructionsMaxBytes = numOrNull(f.instructionsMaxBytes);
    if (Object.values(agentNums).includes('invalid') || instructionsMaxBytes === 'invalid') {
      setStatus('error'); setMessage(t('settings.invalidNumber')); return;
    }
    setStatus('saving'); setMessage('');
    Promise.resolve(window.starriver.updateSettings?.({
      agent: agentNums,
      prompt: {
        include_identity: f.includeIdentity,
        include_runtime_context: f.includeRuntime,
        instructions_max_bytes: instructionsMaxBytes,
        persona_global: f.persona.trim() || null,
      },
    }))
      .then((updated: any) => { fromSettings(updated); setStatus('saved'); })
      .catch((e: any) => { setStatus('error'); setMessage(String(e?.message || e)); });
  };

  if (loadError) {
    return <div data-testid="agent-load-error" style={{ fontSize: 13, color: 'var(--danger)' }}>
      {t('settings.loadFailed')}: {loadError}
    </div>;
  }

  const grid: React.CSSProperties = { display: 'grid', gridTemplateColumns: '1fr 1fr', gap: '4px 14px', marginBottom: 12 };
  return (
    <div>
      <div style={{ fontSize: 12, color: 'var(--text-muted)', marginBottom: 12, lineHeight: 1.6 }}>
        {t('settings.agentHint')}
      </div>
      <div style={grid}>
        <SettingsField label={t('settings.maxParallelToolCalls')} hint={t('settings.fieldHint')}>
          <input aria-label="agent-max-parallel" style={settingsInputStyle} value={f.maxParallel}
            onChange={(e) => setF({ ...f, maxParallel: e.target.value })} />
        </SettingsField>
        <SettingsField label={t('settings.turnToolCallBudget')} hint={t('settings.fieldHint')}>
          <input aria-label="agent-turn-budget" style={settingsInputStyle} value={f.turnBudget}
            onChange={(e) => setF({ ...f, turnBudget: e.target.value })} />
        </SettingsField>
        <SettingsField label={t('settings.compactionThreshold')} hint={t('settings.fieldHint')}>
          <input aria-label="agent-compaction-ratio" style={settingsInputStyle} value={f.compactionRatio}
            onChange={(e) => setF({ ...f, compactionRatio: e.target.value })} />
        </SettingsField>
        <SettingsField label={t('settings.timeRefresh')} hint={t('settings.fieldHint')}>
          <input aria-label="agent-time-refresh" style={settingsInputStyle} value={f.timeRefresh}
            onChange={(e) => setF({ ...f, timeRefresh: e.target.value })} />
        </SettingsField>
      </div>
      {/* WI-35: real truncation semantics live host-side (compose_budgeted);
          the renderer has no per-session omitted/truncated signal yet, so the
          field carries the factual budget hint instead of a fake counter. */}
      <SettingsField label={t('settings.instructionsMaxBytes')} hint={t('settings.instructionsBudgetHint')}>
        <input aria-label="prompt-instructions-max" style={settingsInputStyle} value={f.instructionsMaxBytes}
          onChange={(e) => setF({ ...f, instructionsMaxBytes: e.target.value })} />
      </SettingsField>
      <div style={{ display: 'flex', flexDirection: 'column', gap: 6, margin: '10px 0 12px' }}>
        <label style={{ display: 'flex', alignItems: 'center', gap: 8, cursor: 'pointer', fontSize: 12 }}>
          <input type="checkbox" checked={f.includeIdentity}
            onChange={(e) => setF({ ...f, includeIdentity: e.target.checked })} />
          {t('settings.includeIdentity')}
        </label>
        <label style={{ display: 'flex', alignItems: 'center', gap: 8, cursor: 'pointer', fontSize: 12 }}>
          <input type="checkbox" checked={f.includeRuntime}
            onChange={(e) => setF({ ...f, includeRuntime: e.target.checked })} />
          {t('settings.includeRuntime')}
        </label>
      </div>
      <SettingsField label={t('settings.personaGlobal')}>
        <textarea aria-label="prompt-persona" rows={3} style={{ ...settingsInputStyle, resize: 'vertical' }}
          placeholder={t('settings.personaPlaceholder')} value={f.persona}
          onChange={(e) => setF({ ...f, persona: e.target.value })} />
      </SettingsField>
      <div style={{ display: 'flex', alignItems: 'center', gap: 10, marginTop: 12 }}>
        <button className="btn btn-primary btn-small" disabled={status === 'saving'} onClick={save}
          aria-label="agent-save">{status === 'saving' ? t('settings.saving') : t('settings.save')}</button>
        <SaveStatusLine status={status} message={message} savedText={t('settings.saved')} />
      </div>
    </div>
  );
}

interface ProviderDraft {
  name: string; baseUrl: string; apiKey: string; apiFormat: string; defaultModel: string;
  temperature: string; maxTokens: string; contextWindow: string; concurrency: string;
}

const draftFromProvider = (p: any): ProviderDraft => ({
  name: p?.name ?? '',
  baseUrl: p?.base_url ?? '',
  apiKey: p?.api_key ?? '',
  apiFormat: p?.api_format || 'openai-chat',
  defaultModel: p?.default_model ?? '',
  temperature: p?.temperature?.toString() ?? '',
  maxTokens: p?.max_tokens?.toString() ?? '',
  contextWindow: p?.context_window?.toString() ?? '',
  concurrency: p?.concurrency?.toString() ?? '',
});

/** Model-provider route editor (settings `llm.providers` slice): read-modify-
 *  write — a provider save replaces the whole array with the latest loaded
 *  list plus the edited entry, so unrelated routes survive. Add/remove and
 *  variant fetching stay in the Models tab (models.add / providers.*). */
export function SettingsModelsSection() {
  const { t } = useI18n();
  const [providers, setProviders] = useState<any[] | null>(null);
  const [drafts, setDrafts] = useState<Record<string, ProviderDraft>>({});
  const [statuses, setStatuses] = useState<Record<string, { state: 'idle' | 'saving' | 'saved' | 'error'; message: string }>>({});
  const [loadError, setLoadError] = useState<string | null>(null);

  const load = useCallback((s: any) => {
    const list: any[] = s?.llm?.providers ?? [];
    setProviders(list);
    setDrafts(Object.fromEntries(list.map((p) => [p.id as string, draftFromProvider(p)])));
    setStatuses({});
  }, []);

  useEffect(() => {
    Promise.resolve(window.starriver.getSettings?.())
      .then(load)
      .catch((e) => setLoadError(String(e?.message || e)));
  }, [load]);

  const saveProvider = (id: string) => {
    const current = providers ?? [];
    const entry = current.find((p) => p.id === id);
    const d = drafts[id];
    if (!entry || !d) return;
    const nums = {
      temperature: numOrNull(d.temperature),
      max_tokens: numOrNull(d.maxTokens),
      context_window: numOrNull(d.contextWindow),
      concurrency: numOrNull(d.concurrency),
    };
    if (Object.values(nums).includes('invalid')) {
      setStatuses((s) => ({ ...s, [id]: { state: 'error', message: t('settings.invalidNumber') } }));
      return;
    }
    const updated = {
      ...entry,
      name: d.name.trim() || entry.name,
      base_url: d.baseUrl.trim(),
      api_key: d.apiKey.trim(),
      api_format: d.apiFormat,
      default_model: d.defaultModel.trim(),
      ...nums,
    };
    setStatuses((s) => ({ ...s, [id]: { state: 'saving', message: '' } }));
    Promise.resolve(window.starriver.updateSettings?.({
      llm: { providers: current.map((p) => (p.id === id ? updated : p)) },
    }))
      .then((merged: any) => {
        // Rebuilding ALL drafts from the snapshot silently discards unsaved
        // edits in every other card — refresh the saved row only.
        const mergedEntry = (merged?.llm?.providers ?? []).find((p: any) => p.id === id) ?? updated;
        setProviders((prev) => (prev ?? []).map((p: any) => (p.id === id ? mergedEntry : p)));
        setDrafts((prev) => ({ ...prev, [id]: draftFromProvider(mergedEntry) }));
        setStatuses((s) => ({ ...s, [id]: { state: 'saved', message: '' } }));
      })
      .catch((e: any) => {
        setStatuses((s) => ({ ...s, [id]: { state: 'error', message: String(e?.message || e) } }));
      });
  };

  if (loadError) {
    return <div data-testid="models-load-error" style={{ fontSize: 13, color: 'var(--danger)' }}>
      {t('settings.loadFailed')}: {loadError}
    </div>;
  }
  if (providers && providers.length === 0) {
    return <div style={{ fontSize: 13, color: 'var(--text-muted)' }}>{t('settings.noProviders')}</div>;
  }

  const grid: React.CSSProperties = { display: 'grid', gridTemplateColumns: '1fr 1fr', gap: '4px 14px', marginBottom: 8 };
  return (
    <div>
      <div style={{ fontSize: 12, color: 'var(--text-muted)', marginBottom: 12, lineHeight: 1.6 }}>
        {t('settings.modelsHint')}
      </div>
      {(providers ?? []).map((p) => {
        const d = drafts[p.id] ?? draftFromProvider(p);
        const st = statuses[p.id]?.state ?? 'idle';
        const set = (patch: Partial<ProviderDraft>) => setDrafts((prev) => ({ ...prev, [p.id]: { ...d, ...patch } }));
        return (
          <div key={p.id} style={{ padding: '10px 12px', marginBottom: 8, borderRadius: 6, background: 'var(--bg-soft)', border: '1px solid var(--border)' }}>
            <div style={{ display: 'flex', alignItems: 'center', gap: 6, marginBottom: 8 }}>
              <span style={{ fontSize: 13, fontFamily: 'var(--mono)', color: 'var(--text-muted)' }}>{p.id}</span>
              <span style={{ fontSize: 11, background: 'var(--accent-soft)', color: 'var(--accent)', borderRadius: 3, padding: '1px 6px' }}>{p.model_type}</span>
              <span style={{ fontSize: 11, color: 'var(--text-muted)' }}>{p.provider_type}</span>
              <span style={{ fontSize: 11, color: 'var(--text-muted)', marginLeft: 'auto' }}>
                {t('settings.providerModels', { count: p.models?.length ?? 0 })}
              </span>
            </div>
            <div style={grid}>
              <SettingsField label={t('models.providerName')}>
                <input aria-label={`provider-${p.id}-name`} style={settingsInputStyle} value={d.name}
                  onChange={(e) => set({ name: e.target.value })} />
              </SettingsField>
              <SettingsField label={t('models.baseUrl')}>
                <input aria-label={`provider-${p.id}-base-url`} style={settingsInputStyle} value={d.baseUrl}
                  onChange={(e) => set({ baseUrl: e.target.value })} />
              </SettingsField>
              <SettingsField label={t('models.apiKey')} hint={t('settings.apiKeyHint')}>
                <input aria-label={`provider-${p.id}-api-key`} style={{ ...settingsInputStyle, fontFamily: 'var(--mono)' }} value={d.apiKey}
                  onChange={(e) => set({ apiKey: e.target.value })} />
              </SettingsField>
              <SettingsField label={t('models.defaultModel')}>
                <input aria-label={`provider-${p.id}-default-model`} style={settingsInputStyle} value={d.defaultModel}
                  onChange={(e) => set({ defaultModel: e.target.value })} />
              </SettingsField>
            </div>
            <details style={{ marginBottom: 6 }}>
              <summary style={{ fontSize: 12, color: 'var(--text-muted)', cursor: 'pointer' }}>{t('models.advanced')}</summary>
              <div style={grid}>
                <SettingsField label={t('models.apiFormat')}>
                  <select aria-label={`provider-${p.id}-api-format`} style={settingsInputStyle} value={d.apiFormat}
                    onChange={(e) => set({ apiFormat: e.target.value })}>
                    <option value="openai-chat">openai-chat</option>
                    <option value="openai-responses">openai-responses</option>
                    <option value="anthropic">anthropic</option>
                  </select>
                </SettingsField>
                <SettingsField label={t('models.temperature')} hint={t('settings.fieldHint')}>
                  <input aria-label={`provider-${p.id}-temperature`} style={settingsInputStyle} value={d.temperature}
                    onChange={(e) => set({ temperature: e.target.value })} />
                </SettingsField>
                <SettingsField label={t('models.maxTokens')} hint={t('settings.fieldHint')}>
                  <input aria-label={`provider-${p.id}-max-tokens`} style={settingsInputStyle} value={d.maxTokens}
                    onChange={(e) => set({ maxTokens: e.target.value })} />
                </SettingsField>
                <SettingsField label={t('models.contextWindow')} hint={t('settings.fieldHint')}>
                  <input aria-label={`provider-${p.id}-context-window`} style={settingsInputStyle} value={d.contextWindow}
                    onChange={(e) => set({ contextWindow: e.target.value })} />
                </SettingsField>
                <SettingsField label={t('models.concurrency')} hint={t('settings.fieldHint')}>
                  <input aria-label={`provider-${p.id}-concurrency`} style={settingsInputStyle} value={d.concurrency}
                    onChange={(e) => set({ concurrency: e.target.value })} />
                </SettingsField>
                {/* WI-152 v2: capability is automatic — no vision picker. */}
              </div>
            </details>
            <div style={{ display: 'flex', alignItems: 'center', gap: 10 }}>
              <button className="btn btn-primary btn-small" disabled={st === 'saving'} onClick={() => saveProvider(p.id)}
                aria-label={`provider-${p.id}-save`}>{st === 'saving' ? t('settings.saving') : t('settings.save')}</button>
              <SaveStatusLine status={st} message={statuses[p.id]?.message ?? ''} savedText={t('settings.saved')} />
            </div>
          </div>
        );
      })}
    </div>
  );
}

/** Plugin inventory: installed plugins with their contributions (UI tabs,
 * permissions, skills) and uninstall — the read-mostly
 *  ui-settings plugin pane. Install/market stay in the Plugins tab. */
export function SettingsPluginsSection() {
  const { t } = useI18n();
  const [plugins, setPlugins] = useState<PluginEntry[] | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);

  useEffect(() => {
    Promise.resolve(window.starriver.listPlugins?.())
      .then((list) => setPlugins(list ?? []))
      .catch((e) => setLoadError(String(e?.message || e)));
  }, []);

  const uninstall = async (p: PluginEntry) => {
    if (!window.confirm(t('settings.uninstallConfirm', { name: p.name, count: p.skills.length }))) return;
    setBusy(p.name);
    try {
      const list = await window.starriver.uninstallPlugin(p.name);
      setPlugins(list ?? []);
      await syncPluginTabs();
    } catch (e: any) {
      setLoadError(String(e?.message || e));
    } finally {
      setBusy(null);
    }
  };

  if (loadError) {
    return <div data-testid="plugins-load-error" style={{ fontSize: 13, color: 'var(--danger)' }}>
      {t('settings.loadFailed')}: {loadError}
    </div>;
  }
  if (plugins && plugins.length === 0) {
    return <div style={{ fontSize: 13, color: 'var(--text-muted)' }}>{t('plugins.noPlugins')}</div>;
  }

  const chip: React.CSSProperties = {
    display: 'inline-block', fontSize: 11, borderRadius: 3, padding: '1px 6px', marginRight: 4,
    background: 'var(--bg-soft)', border: '1px solid var(--border)', color: 'var(--text-muted)',
  };
  return (
    <div>
      <div style={{ fontSize: 12, color: 'var(--text-muted)', marginBottom: 12, lineHeight: 1.6 }}>
        {t('settings.pluginsHint')}
      </div>
      {(plugins ?? []).map((p) => (
        <div key={p.name} data-testid={`plugin-card-${p.name}`}
          style={{ padding: '10px 12px', marginBottom: 8, borderRadius: 6, background: 'var(--bg-soft)', border: '1px solid var(--border)' }}>
          <div style={{ display: 'flex', alignItems: 'center', gap: 6 }}>
            <span style={{ fontWeight: 600, fontSize: 13 }}>{p.name}</span>
            {p.version && <span style={{ fontSize: 11, color: 'var(--text-muted)' }}>v{p.version}</span>}
            {p.ui && <span style={{ fontSize: 11, color: 'var(--accent)', background: 'var(--accent-soft)', borderRadius: 3, padding: '1px 5px' }}>UI</span>}
            <button className="btn btn-secondary btn-small" disabled={busy === p.name}
              style={{ fontSize: 12, padding: '3px 10px', color: 'var(--danger)', marginLeft: 'auto' }}
              onClick={() => uninstall(p)}>{t('plugins.uninstall')}</button>
          </div>
          {p.description && <div style={{ fontSize: 12, color: 'var(--text-muted)', marginTop: 4 }}>{p.description}</div>}
          <div style={{ fontSize: 12, color: 'var(--text-muted)', marginTop: 4 }}>
            {t('settings.skillsCount', { count: p.skills.length })}
            {p.skills.length > 0 && `: ${p.skills.join(', ')}`}
          </div>
          {p.ui?.tabs && p.ui.tabs.length > 0 && (
            <div style={{ fontSize: 12, color: 'var(--text-muted)', marginTop: 4 }}>
              {t('settings.uiTabs')}: {p.ui.tabs.map((tab) => (
                <span key={tab.id} style={chip}>{tab.title}</span>
              ))}
            </div>
          )}
          {p.ui?.permissions && p.ui.permissions.length > 0 && (
            <div style={{ fontSize: 12, color: 'var(--text-muted)', marginTop: 4 }}>
              {t('settings.permissions')}: {p.ui.permissions.map((perm) => (
                <span key={perm} style={chip}>{perm}</span>
              ))}
            </div>
          )}
          {(p.installedAt || p.installed_at) && (
            <div style={{ fontSize: 11, color: 'var(--text-muted)', marginTop: 4 }}>
              {t('plugins.installedAt')} {(p.installedAt || p.installed_at).slice(0, 10)}
            </div>
          )}
        </div>
      ))}
    </div>
  );
}

/** Subagent delegation (Settings → Coding): enable a template and bind its
    child model (DSH authorize-selectable-child-models parity). An enabled
    template without a bound model runs the child on the calling agent's
    route. Self-fetching: reads the config and model registry on mount. */
/** Xcode 风格的代码高亮配色面板(2026-10-04 方案:配色可配置)。
 *  仅浅色一组 × 11 个 token,平铺无折叠(dark 组保留默认值渲染,不在设置里编辑);
 *  色块即改即生效(applyCodeTokens 注入覆盖 style),
 *  与默认值不同的行显示 ↺ 单行还原;localStorage 持久化,无保存按钮。 */
const CODE_TOKEN_ROWS: Array<{ key: CodeTokenKey; label: string }> = [
  { key: 'comment', label: 'codeToken.comment' },
  { key: 'keyword', label: 'codeToken.keyword' },
  { key: 'string', label: 'codeToken.string' },
  { key: 'number', label: 'codeToken.number' },
  { key: 'link', label: 'codeToken.link' },
  { key: 'meta', label: 'codeToken.meta' },
  { key: 'type', label: 'codeToken.type' },
  { key: 'member', label: 'codeToken.member' },
  { key: 'other-type', label: 'codeToken.otherType' },
  { key: 'diff-add', label: 'codeToken.diffAdd' },
  { key: 'diff-del', label: 'codeToken.diffDel' },
];

function CodeTokenColorsSection() {
  const { t } = useI18n();
  const [version, setVersion] = useState(0);
  const bump = () => setVersion((v) => v + 1);
  return (
    <div data-tick={version} style={{ marginBottom: 16 }}>
      <div style={{ fontSize: 14, fontWeight: 600, color: 'var(--text)', marginBottom: 8 }}>{t('settings.codePalette.title')}</div>
      <div style={{ padding: '4px 0 2px' }}>
        {CODE_TOKEN_ROWS.map(({ key, label }) => {
          const current = currentTokenValue('light', key);
          const overridden = current.toLowerCase() !== DEFAULT_LIGHT[key].toLowerCase();
          return (
            <div key={key} style={{ display: 'flex', alignItems: 'center', gap: 8, padding: '3px 0' }}>
              <span style={{ flex: 1, fontSize: 14, color: 'var(--text)' }}>{t(label)}</span>
              {overridden && (
                <button
                  type="button"
                  title={t('settings.codePalette.reset')}
                  onClick={() => { setCodeToken('light', key, DEFAULT_LIGHT[key]); bump(); }}
                  style={{ background: 'none', border: 'none', color: 'var(--text-muted)', cursor: 'pointer', fontSize: 14, padding: '0 2px' }}
                >↺</button>
              )}
              <input
                type="color"
                value={current}
                aria-label={t(label)}
                onChange={(e) => { setCodeToken('light', key, e.target.value); bump(); }}
                style={{ width: 36, height: 22, padding: 0, border: '1px solid var(--border)', borderRadius: 6, background: 'none', cursor: 'pointer' }}
              />
            </div>
          );
        })}
      </div>
      <button type="button" className="btn btn-secondary btn-small" onClick={() => { resetCodeTokens(); bump(); }}>
        {t('settings.codePalette.resetAll')}
      </button>
    </div>
  );
}

export function SubagentDelegationSection() {
  const [subagents, setSubagents] = useState<SubagentConfig[]>([]);
  const [models, setModels] = useState<ModelEntry[]>([]);

  useEffect(() => {
    (async () => {
      try { setModels(await window.starriver.listModels()); } catch (e) { console.error(e); }
      try {
        setSubagents(await window.starriver.listSubagentConfigs?.() ?? []);
      } catch (e) { console.error(e); }
    })();
  }, []);

  const saveSubagent = async (name: string, enabled: boolean, model?: string) => {
    try {
      await window.starriver.setSubagentConfig(name, enabled, model);
      const next = await window.starriver.listSubagentConfigs();
      setSubagents(next ?? []);
    } catch (e) { console.error(e); }
  };

  if (subagents.length === 0) return null;
  return (
    <div style={{ marginBottom: 16 }}>
      <h4 style={{ margin: '0 0 2px', fontSize: 14, color: 'var(--text-secondary)' }}>Subagent Delegation</h4>
      <p style={{ margin: '0 0 8px', fontSize: 12, color: 'var(--text-muted)' }}>
        Enable a template and pick its model — the child runs under that route, or inherits the calling agent's model when none is picked.
      </p>
      {subagents.map((s) => {
        const modelOptions = models.flatMap(m =>
          m.models.length > 0
            ? m.models.map(v => `${m.id}/${v.id}`)
            : [`${m.id}/${m.defaultModel}`],
        );
        return (
          <div key={s.name} style={{
            padding: '10px 14px', marginBottom: 6, borderRadius: 6,
            background: 'var(--surface)', border: '1px solid var(--border)',
          }}>
            <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between' }}>
              <div style={{ minWidth: 0 }}>
                <span style={{ fontWeight: 600, fontSize: 13 }}>{s.name}</span>
                <span style={{ marginLeft: 8, fontSize: 12, color: 'var(--text-muted)' }}>{s.description}</span>
              </div>
              <div style={{ display: 'flex', gap: 8, alignItems: 'center', flexShrink: 0 }}>
                {s.enabled && (
                  <select
                    value={s.model ?? ''}
                    onChange={(e) => saveSubagent(s.name, true, e.target.value || undefined)}
                    style={{ fontSize: 13, padding: '3px 6px', borderRadius: 4, border: '1px solid var(--border-strong)', maxWidth: 220 }}
                    title="Child model route"
                  >
                    <option value="">Calling agent model</option>
                    {modelOptions.map((opt) => <option key={opt} value={opt}>{opt}</option>)}
                  </select>
                )}
                <label style={{ fontSize: 12, color: 'var(--text-secondary)', display: 'flex', alignItems: 'center', gap: 4, cursor: 'pointer' }}>
                  <input
                    type="checkbox"
                    checked={s.enabled}
                    onChange={(e) => saveSubagent(s.name, e.target.checked, s.model)}
                    title={s.enabled ? 'Disable template' : 'Enable template'}
                  />
                  enabled
                </label>
              </div>
            </div>
            <div style={{ fontSize: 12, color: 'var(--text-muted)', marginTop: 4 }}>
              {s.readOnly ? 'read-only' : 'read/write'} · tools: {s.tools.join(', ')}
            </div>
          </div>
        );
      })}
    </div>
  );
}

export function SettingsDialog({ onClose }: {
  onClose: () => void;
}) {
  const { t, lang, setLang } = useI18n();
  const [tab, setTab] = useState<'general' | 'coding' | 'agent'>('general');

  const tabBtn = (t2: 'general' | 'coding' | 'agent', label: string) => {
    const active = tab === t2;
    return (
      <button onClick={() => setTab(t2)} style={{
        flex: 1, padding: '9px 8px', border: 'none', cursor: 'pointer', background: 'none',
        color: active ? 'var(--accent)' : 'var(--text-muted)', fontSize: 14, fontWeight: 600,
        borderBottom: `2px solid ${active ? 'var(--accent)' : 'transparent'}`, transition: 'all 0.15s',
      }}>{label}</button>
    );
  };

  return (
    <div style={{
      position: 'fixed', inset: 0, background: 'var(--overlay-strong)',
      display: 'flex', alignItems: 'center', justifyContent: 'center', zIndex: 1000,
    }} onClick={onClose}>
      <div style={{
        background: 'var(--surface)', borderRadius: 10, width: 520, height: 440,
        display: 'flex', flexDirection: 'column', overflow: 'hidden',
        boxShadow: 'var(--shadow-md)',
      }} onClick={(e) => e.stopPropagation()}>
        <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', padding: '16px 20px 8px' }}>
          <h3 style={{ margin: 0, fontSize: 15 }}>{t('settings.title')}</h3>
          <button onClick={onClose} style={{ background: 'none', border: 'none', fontSize: 18, cursor: 'pointer', color: 'var(--text-muted)', padding: '0 2px' }}>✕</button>
        </div>

        <div style={{ display: 'flex', padding: '4px 12px 0', gap: 10, borderBottom: '1px solid var(--border)' }}>
          {tabBtn('general', t('settings.general'))}
          {tabBtn('coding', t('settings.coding'))}
          {tabBtn('agent', t('settings.tabAgent'))}
        </div>

        <div style={{ padding: '16px 20px', overflowY: 'auto' }}>
          {tab === 'general' && (
            <>
              <div style={{ marginBottom: 16 }}>
                <div style={{ fontSize: 14, fontWeight: 600, color: 'var(--text)', marginBottom: 8 }}>{t('settings.language')}</div>
                <select value={lang} onChange={(e) => setLang(e.target.value as 'en' | 'zh')}
                  style={{ width: '100%', padding: '8px 12px', border: '1px solid var(--border-strong)', borderRadius: 6, fontSize: 14, background: 'var(--surface)', cursor: 'pointer' }}>
                  <option value="en">{t('settings.english')}</option>
                  <option value="zh">{t('settings.chinese')}</option>
                </select>
              </div>
              {/* Theme picker removed (2026-10-05): Classic Gray is the only
                  supported theme for now; boot forces the default so a stale
                  saved pick can't stick with no picker to undo it. */}
              <CodeTokenColorsSection />
              <UiLockScheduleSection />
            </>
          )}
          {tab === 'coding' && (
            <div>
              <SubagentDelegationSection />
            </div>
          )}
          {tab === 'agent' && <SettingsAgentSection />}
        </div>

        <div style={{ display: 'flex', justifyContent: 'flex-end', gap: 8, padding: '12px 20px 16px', borderTop: '1px solid var(--bg-soft)' }}>
          <button className="btn btn-secondary btn-small" onClick={onClose}>{t('settings.close')}</button>
        </div>
      </div>
    </div>
  );
}

/** Remote Access (iPhone pairing) — lives in its own dialog, opened from the
 *  phone icon next to the sidebar's Settings button. */
function RemoteAccessDialog({ onClose }: { onClose: () => void }) {
  const { t } = useI18n();
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => { if (e.key === 'Escape') onClose(); };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [onClose]);
  return (
    <div style={{
      position: 'fixed', inset: 0, background: 'var(--overlay-strong)',
      display: 'flex', alignItems: 'center', justifyContent: 'center', zIndex: 1000,
    }} onClick={onClose}>
      <div style={{
        background: 'var(--surface)', borderRadius: 10, width: 460,
        padding: '16px 20px 18px', boxShadow: 'var(--shadow-md)',
      }} onClick={(e) => e.stopPropagation()}>
        <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginBottom: 10 }}>
          <h3 style={{ margin: 0, fontSize: 15 }}>{t('settings.remoteTitle')}</h3>
          <button onClick={onClose} style={{ background: 'none', border: 'none', fontSize: 18, cursor: 'pointer', color: 'var(--text-muted)', padding: '0 2px' }}>✕</button>
        </div>
        <RemoteAccessSection />
      </div>
    </div>
  );
}

const tabIcons: Record<string, string> = {
  chat: '<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"><path d="M3 4h10v7H6l-3 2.5V4z"/></svg>',
  coding: '<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><polyline points="6,4 2,8 6,12"/><polyline points="10,4 14,8 10,12"/></svg>',
  newTask: '<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><rect x="2.5" y="2.5" width="11" height="11" rx="2.5"/><path d="M8 5.5v5M5.5 8h5"/></svg>',
  agents: '<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"><circle cx="8" cy="5" r="3"/><path d="M3 14c0-3.3 2.7-6 5-6s5 2.7 5 6"/></svg>',
  skills: '<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"><rect x="2" y="2" width="5" height="5" rx="1"/><rect x="9" y="2" width="5" height="5" rx="1"/><rect x="2" y="9" width="5" height="5" rx="1"/><rect x="9" y="9" width="5" height="5" rx="1"/></svg>',
  models: '<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round"><path d="M14 10.4V5.6a1.3 1.3 0 0 0-.65-1.13l-5-2.87a1.3 1.3 0 0 0-1.3 0l-5 2.87A1.3 1.3 0 0 0 1.35 5.6v4.8a1.3 1.3 0 0 0 .65 1.13l5 2.87a1.3 1.3 0 0 0 1.3 0l5-2.87a1.3 1.3 0 0 0 .65-1.13z"/><path d="M2.2 4.8 8 8.05l5.8-3.25M8 8.05v6.1"/></svg>',
  settings: '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12.22 2h-.44a2 2 0 0 0-2 2v.18a2 2 0 0 1-1 1.73l-.43.25a2 2 0 0 1-2 0l-.15-.08a2 2 0 0 0-2.73.73l-.22.38a2 2 0 0 0 .73 2.73l.15.1a2 2 0 0 1 1 1.72v.51a2 2 0 0 1-1 1.74l-.15.09a2 2 0 0 0-.73 2.73l.22.38a2 2 0 0 0 2.73.73l.15-.08a2 2 0 0 1 2 0l.43.25a2 2 0 0 1 1 1.73V20a2 2 0 0 0 2 2h.44a2 2 0 0 0 2-2v-.18a2 2 0 0 1 1-1.73l.43-.25a2 2 0 0 1 2 0l.15.08a2 2 0 0 0 2.73-.73l.22-.39a2 2 0 0 0-.73-2.73l-.15-.08a2 2 0 0 1-1-1.74v-.5a2 2 0 0 1 1-1.74l.15-.09a2 2 0 0 0 .73-2.73l-.22-.38a2 2 0 0 0-2.73-.73l-.15.08a2 2 0 0 1-2 0l-.43-.25a2 2 0 0 1-1-1.73V4a2 2 0 0 0-2-2z"/><circle cx="12" cy="12" r="3"/></svg>',
  plugins: '<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round"><path d="M3 3h3.5a1.5 1.5 0 0 1 3 0H13v3.5a1.5 1.5 0 0 1 0 3V13H9.5a1.5 1.5 0 0 1-3 0H3V9.5a1.5 1.5 0 0 1 0-3z"/></svg>',
  mcp: '<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round"><circle cx="8" cy="5" r="3"/><path d="M8 8v4M5.5 12h5M6 14h4"/></svg>',
  file: '<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round"><path d="M4 1.5h5.2L13 5.3v9.2H4z"/><path d="M9.2 1.5V5.3H13"/></svg>',
  scheduled: '<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round"><circle cx="8" cy="8" r="5.5"/><path d="M8 4.5V8l2 2M5 2l1 1.5M11 2L10 3.5M3.5 5.5L5 6.5"/></svg>',
  knowledge: '<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round"><path d="M2 3h3.5a1.5 1.5 0 0 1 3 0H14v10H2V3z"/><rect x="4" y="4" width="4" height="2" rx="0.5"/><path d="M6 5v4"/></svg>',
  memory: '<svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round"><circle cx="8" cy="8" r="2"/><path d="M8 3v1.5M8 11.5V13M3 8h1.5M11.5 8H13M4.5 4.5l1 1M10.5 10.5l1 1M4.5 11.5l1-1M10.5 5.5l1-1"/></svg>',
  hooks: '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M18 16.98h-5.99c-1.1 0-1.95.94-2.48 1.9A4 4 0 0 1 2 17c.01-.7.2-1.4.57-2"/><path d="m6 17 3.13-5.78c.53-.97.1-2.18-.5-2.9a4 4 0 0 1 6.89-4.2"/><path d="m12 6 3.13 5.73C15.66 12.7 16.9 13 18 13a4 4 0 0 1 0 8"/></svg>',
};

/* ─── Backend error screen ─── */

export function BackendErrorScreen({ error }: { error: string }) {
  const { t } = useI18n();
  const lower = (error || '').toLowerCase();
  const kind:
    | 'binaryMissing'
    | 'spawnFailed'
    | 'tokenMismatch'
    | 'versionMismatch'
    | 'unknown' = lower.includes('binary not found')
      ? 'binaryMissing'
      : lower.includes('exited before reporting') || lower.includes('startup timed out')
        ? 'spawnFailed'
        : lower.includes('invalid token') || lower.includes('unauthorized')
          ? 'tokenMismatch'
          : lower.includes('version mismatch')
            ? 'versionMismatch'
            : 'unknown';
  const hintKey =
    kind === 'binaryMissing'
      ? 'backendError.hintBinaryMissing'
      : kind === 'spawnFailed'
        ? 'backendError.hintSpawnFailed'
        : kind === 'tokenMismatch'
          ? 'backendError.hintTokenMismatch'
          : kind === 'versionMismatch'
            ? 'backendError.hintVersionMismatch'
            : null;
  return (
    <div style={{
      height: '100vh', display: 'flex', alignItems: 'center', justifyContent: 'center',
      background: 'var(--danger-bg)', color: 'var(--danger-soft)', fontFamily: 'var(--font)',
    }}>
      <div style={{ maxWidth: 640, padding: 32, width: '100%' }}>
        <h1 style={{ color: 'var(--danger-soft)', fontSize: 22, marginBottom: 8 }}>{t('backendError.title')}</h1>
        <p style={{ marginBottom: 8, lineHeight: 1.6 }}>{t('backendError.desc')}</p>
        <p style={{ marginBottom: 8, fontWeight: 600 }}>{t(`backendError.${kind}`)}</p>
        {hintKey && (
          <p style={{ marginBottom: 16, lineHeight: 1.6, color: 'var(--text-muted)' }}>{t(hintKey)}</p>
        )}
        <div style={{ display: 'flex', gap: 10, marginTop: 16 }}>
          <button
            onClick={() => { void window.starriver?.retryInit?.(); }}
            style={{
              padding: '8px 20px', borderRadius: 6, border: 'none', cursor: 'pointer',
              background: 'var(--accent)', color: 'var(--surface)', fontSize: 15,
            }}
          >
            {t('backendError.retry')}
          </button>
          <button
            onClick={() => window.starriver?.quitApp?.()}
            style={{
              padding: '8px 20px', borderRadius: 6, border: 'none', cursor: 'pointer',
              background: 'var(--danger)', color: 'var(--surface)', fontSize: 15,
            }}
          >
            {t('backendError.quit')}
          </button>
        </div>
        <details style={{ marginTop: 20 }}>
          <summary style={{ cursor: 'pointer', fontSize: 13, color: 'var(--text-muted)' }}>
            {t('backendError.devDetail')}
          </summary>
          <pre style={{
            background: 'var(--overlay)', border: '1px solid var(--danger-soft)', borderRadius: 6,
            padding: 12, overflow: 'auto', maxHeight: 240, whiteSpace: 'pre-wrap',
            wordBreak: 'break-word', fontFamily: 'ui-monospace, Menlo, monospace', fontSize: 13,
            color: 'var(--danger-soft)', marginTop: 8,
          }}>{error}</pre>
        </details>
      </div>
    </div>
  );
}


/* ─── HooksTab (WI-115): manage PreToolUse / PostToolUse shell scripts ─── */

type HookScript = { event: string; name: string; size: number; modifiedSecs: number };

const HOOK_EVENTS = ['PreToolUse', 'PostToolUse'] as const;

const HOOK_TEMPLATE_AUDIT = `#!/bin/sh
# Audit: log every gated tool call (params arrive as a JSON file).
echo "$(date '+%F %T') $STARRIVER_EVENT $STARRIVER_TOOL session=$STARRIVER_SESSION $(cat "$STARRIVER_PARAMS_FILE")" >> "$HOME/.starriver/hooks/audit.log"
exit 0
`;

const HOOK_TEMPLATE_DENY = `#!/bin/sh
# Deny: exit 2 blocks the tool; stdout is the reason shown to the model.
case "$(cat "$STARRIVER_PARAMS_FILE")" in
  *rm\\ -rf*) echo "blocked by hook: rm -rf is not allowed"; exit 2 ;;
esac
exit 0
`;

function hookToolBadges(name: string): string[] | null {
  const stem = name.split('.').slice(0, -1).join('.') || name;
  if (stem === 'all' || name === 'all') return null; // matches every tool
  return stem
    .split(/[^A-Za-z0-9_]/)
    .filter(Boolean);
}

export function HooksTab() {
  const { t } = useI18n();
  const [hooks, setHooks] = useState<HookScript[]>([]);
  const [modal, setModal] = useState<{ mode: 'create' } | { mode: 'edit'; hook: HookScript } | null>(null);

  const refresh = useCallback(async () => {
    try { setHooks(await window.starriver.listHooks()); } catch (e) { console.error(e); }
  }, []);
  useEffect(() => { refresh(); }, [refresh]);

  const handleDelete = async (h: HookScript) => {
    if (!confirm(t('hooks.deleteConfirm').replace('{name}', h.name))) return;
    try { await window.starriver.deleteHook(h.event, h.name); await refresh(); } catch (e) { console.error(e); }
  };

  const row = (h: HookScript) => {
    const tools = hookToolBadges(h.name);
    return (
      <li key={`${h.event}/${h.name}`} className="skill-item" style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
        <span style={{ fontFamily: 'ui-monospace, Menlo, monospace', fontSize: 12 }}>{h.name}</span>
        <span style={{ display: 'flex', gap: 4, flex: 1, flexWrap: 'wrap' }}>
          {tools === null
            ? <span className="badge">{t('hooks.allTools')}</span>
            : tools.map((tk) => <span key={tk} className="badge">{tk}</span>)}
        </span>
        <span style={{ fontSize: 12, color: 'var(--text-muted)' }}>{h.size}B</span>
        <button className="btn btn-secondary btn-small" onClick={() => setModal({ mode: 'edit', hook: h })}>Edit</button>
        <button className="btn btn-secondary btn-small" style={{ color: 'var(--danger)' }} onClick={() => handleDelete(h)}>Delete</button>
      </li>
    );
  };

  const group = (event: string, label: string) => {
    const items = hooks.filter((h) => h.event === event);
    return (
      <div key={event} style={{ marginBottom: 16 }}>
        <h4 style={{ margin: '0 0 6px', fontSize: 13, color: 'var(--text-secondary)' }}>{label}</h4>
        {items.length === 0
          ? <div style={{ fontSize: 13, color: 'var(--text-muted)', padding: '6px 0' }}>—</div>
          : <ul className="skills-list" style={{ margin: 0, paddingLeft: 0, listStyle: 'none' }}>{items.map(row)}</ul>}
      </div>
    );
  };

  return (
    <div className="skills-container">
      <div className="skills-header">
        <h3>Hooks</h3>
        <button className="btn btn-primary btn-small" onClick={() => setModal({ mode: 'create' })}>+ {t('hooks.new')}</button>
      </div>
      <p style={{ fontSize: 13, color: 'var(--text-secondary)', marginTop: 0 }}>{t('hooks.subtitle')}</p>
      {hooks.length === 0 && (
        <div style={{ border: '1px dashed var(--border-strong)', borderRadius: 8, padding: 16, marginBottom: 16 }}>
          <div style={{ fontWeight: 600, marginBottom: 4 }}>{t('hooks.empty')}</div>
          <div style={{ fontSize: 13, color: 'var(--text-muted)', marginBottom: 10 }}>{t('hooks.emptyHint')}</div>
          <div style={{ fontSize: 13, fontWeight: 600, marginBottom: 4 }}>{t('hooks.envTitle')}</div>
          <pre style={{ fontSize: 12, margin: 0, fontFamily: 'ui-monospace, Menlo, monospace', whiteSpace: 'pre-wrap' }}>
{`STARRIVER_EVENT       PreToolUse | PostToolUse
STARRIVER_TOOL        <tool name>
STARRIVER_SESSION     <session id>
STARRIVER_WORKSPACE   <workspace root>
STARRIVER_PARAMS_FILE <JSON file with the full tool arguments>
STARRIVER_RESULT_FILE <JSON {ok, value|error}> (PostToolUse only)`}
          </pre>
        </div>
      )}
      {group('PreToolUse', t('hooks.groupPre'))}
      {group('PostToolUse', t('hooks.groupPost'))}
      {modal && (
        <HookEditorModal
          initial={modal.mode === 'edit' ? modal.hook : undefined}
          onClose={() => setModal(null)}
          onSaved={() => { setModal(null); refresh(); }}
        />
      )}
    </div>
  );
}

function HookEditorModal({ onClose, onSaved, initial }: {
  onClose: () => void;
  onSaved: () => void;
  initial?: HookScript;
}) {
  const { t } = useI18n();
  const [event, setEvent] = useState<string>(initial?.event ?? 'PreToolUse');
  const [name, setName] = useState(initial?.name ?? '');
  const [content, setContent] = useState('');
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const editing = !!initial;

  const loadContent = useCallback(async () => {
    if (!initial) return;
    try { setContent(await window.starriver.readHook(initial.event, initial.name)); }
    catch (e) { setError(e?.message || String(e)); }
  }, [initial]);
  useEffect(() => { loadContent(); }, [loadContent]);

  const save = async () => {
    setError(null);
    let finalName = name.trim();
    if (!finalName) { setError(t('hooks.fieldName') + ' ?'); return; }
    if (!/\.[A-Za-z0-9]+$/.test(finalName)) finalName += '.sh';
    setSaving(true);
    try {
      await window.starriver.writeHook(event, finalName, content);
      onSaved();
    } catch (e: any) {
      setError(`${t('hooks.saveError')}: ${e?.message || e}`);
    } finally { setSaving(false); }
  };

  const label: React.CSSProperties = { fontSize: 12, color: 'var(--text-secondary)', display: 'block', marginBottom: 4 };
  const input: React.CSSProperties = { width: '100%', padding: '6px 10px', border: '1px solid var(--border-strong)', borderRadius: 5, fontSize: 14, outline: 'none', fontFamily: 'inherit', boxSizing: 'border-box' };
  const tplBtn = (labelText: string, body: string) => (
    <button className="btn btn-secondary btn-small" type="button"
      onClick={() => setContent(body)}>{labelText}</button>
  );

  return (
    <div style={{ position: 'fixed', inset: 0, background: 'var(--overlay-strong)', display: 'flex', alignItems: 'center', justifyContent: 'center', zIndex: 1000 }} onClick={onClose}>
      <div style={{ background: 'var(--surface)', borderRadius: 10, width: 580, maxHeight: '85vh', overflowY: 'auto', boxShadow: 'var(--shadow-md)' }} onClick={(e) => e.stopPropagation()}>
        <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', padding: '16px 20px 0' }}>
          <h3 style={{ margin: 0, fontSize: 15 }}>{editing ? t('hooks.edit') : t('hooks.new')}</h3>
          <button onClick={onClose} style={{ background: 'none', border: 'none', fontSize: 18, cursor: 'pointer', color: 'var(--text-muted)' }}>✕</button>
        </div>
        <div style={{ padding: '16px 20px', display: 'flex', flexDirection: 'column', gap: 10 }}>
          {error && (
            <div style={{ padding: '8px 12px', background: 'var(--danger-soft)', border: '1px solid var(--danger-border)', borderRadius: 6, color: 'var(--danger)', fontSize: 13 }}>{error}</div>
          )}
          <div style={{ display: 'flex', gap: 10 }}>
            <div style={{ width: 180 }}>
              <label style={label}>{t('hooks.fieldEvent')}</label>
              <select value={event} onChange={(e) => setEvent(e.target.value)} style={input} disabled={editing}>
                {HOOK_EVENTS.map((ev) => <option key={ev} value={ev}>{ev}</option>)}
              </select>
            </div>
            <div style={{ flex: 1 }}>
              <label style={label}>{t('hooks.fieldName')}</label>
              <input value={name} onChange={(e) => setName(e.target.value)} placeholder="bash-audit.sh" style={input} disabled={editing} />
            </div>
          </div>
          <div style={{ fontSize: 12, color: 'var(--text-muted)', marginTop: -4 }}>{t('hooks.nameHint')}</div>
          {!editing && content === '' && (
            <div style={{ display: 'flex', gap: 6 }}>
              {tplBtn(t('hooks.templateAudit'), HOOK_TEMPLATE_AUDIT)}
              {tplBtn(t('hooks.templateDeny'), HOOK_TEMPLATE_DENY)}
            </div>
          )}
          <div>
            <label style={label}>{t('hooks.fieldContent')}</label>
            <textarea value={content} onChange={(e) => setContent(e.target.value)} rows={12}
              style={{ ...input, fontFamily: 'ui-monospace, Menlo, monospace', fontSize: 13, resize: 'vertical' }} />
          </div>
        </div>
        <div style={{ display: 'flex', justifyContent: 'flex-end', gap: 8, padding: '0 20px 16px' }}>
          <button className="btn btn-secondary btn-small" onClick={onClose}>Cancel</button>
          <button className="btn btn-primary btn-small" onClick={save} disabled={saving}
            style={{ opacity: saving ? 0.6 : 1, cursor: saving ? 'wait' : 'pointer' }}>
            {saving ? 'Saving...' : 'Save'}
          </button>
        </div>
      </div>
    </div>
  );
}

/* ─── Built-in tab contributions (FPM P2): registry-driven; plugin tabs take the same path ─── */


registerTab({ id: 'coding', labelKey: 'nav.newTask', icon: tabIcons.newTask, render: ({ debugEnabled }) => <CodingView debugEnabled={debugEnabled} /> });
// The standalone Chat tab is retired: New Task's Chat mode runs the same Chat
// Agent (ungrouped sessions, own model binding). ChatTab stays as dead code
// until its two orphaned affordances get new homes — the ungrouped-session
// history list and the Chat Agent binding UI.
registerTab({ id: 'skills', labelKey: 'nav.skills', icon: tabIcons.skills, render: () => <SkillsTab /> });
registerTab({ id: 'plugins', labelKey: 'nav.plugins', icon: tabIcons.plugins, render: () => <PluginsTab /> });
registerTab({ id: 'hooks', labelKey: 'nav.hooks', icon: tabIcons.hooks, render: () => <HooksTab /> });
registerTab({ id: 'mcp', labelKey: 'nav.mcp', icon: tabIcons.mcp, render: () => <McpTab /> });
registerTab({ id: 'scheduled', labelKey: 'nav.scheduled', icon: tabIcons.scheduled, render: () => <ScheduledTasksTab /> });
registerTab({ id: 'models', labelKey: 'nav.models', icon: tabIcons.models, render: () => <ModelsTab /> });
registerTab({ id: 'memory', labelKey: 'nav.memory', icon: tabIcons.memory, render: () => <MemoryTab /> });

// Sidebar tab list. Re-clicking the active "New Task" (coding) item must
// return to the New Task screen — a session otherwise has no path back to a
// fresh task, and the click used to be a no-op setTab.
// Sidebar groups: Agent is the daily driver (tasks + schedules + the
// workspace tree); Hub is capability management. Fixed order per group —
// the registry's registration order is an implementation detail. Unknown
// tabs (plugin contributions) append to Hub so they stay reachable.
const SIDEBAR_GROUPS: Record<'agent' | 'hub', string[]> = {
  agent: ['coding', 'scheduled'],
  hub: ['models', 'skills', 'plugins', 'hooks', 'mcp', 'memory'],
};

export function SidebarNavItems({ tab, onNavigate, group = 'agent' }: { tab: TabId; onNavigate: (id: TabId) => void; group?: 'agent' | 'hub' }) {
  const { t } = useI18n();
  const ws = useWorkspaces();
  const fixed = SIDEBAR_GROUPS[group];
  // Hub appends only unknown tabs (plugin contributions) — the Agent group's
  // fixed tabs never leak into the Hub list.
  const known = new Set([...SIDEBAR_GROUPS.agent, ...SIDEBAR_GROUPS.hub]);
  const ids = group === 'hub'
    ? [...fixed, ...listTabs().map(item => item.id).filter(id => !known.has(id))]
    : fixed;
  return (
    <nav className="sidebar-nav">
      {ids.map((id) => {
        const item = getTab(id);
        if (!item) return null;
        return (
          <button
            key={item.id}
            className={`nav-item ${tab === item.id ? 'active' : ''}`}
            onClick={() => {
              if (item.id === 'coding' && tab === 'coding') {
                if (ws.activeWorkspaceId) void ws.requestNewConversation(ws.activeWorkspaceId);
                else ws.setActiveSessionId('');
              }
              onNavigate(item.id);
            }}
          >
            <span className="nav-icon" dangerouslySetInnerHTML={{ __html: item.icon || tabIcons[item.id] || "" }} />
            {t(item.labelKey)}
          </button>
        );
      })}
    </nav>
  );
}

function App() {
  const { t } = useI18n();
  const [tab, setTab] = useState<TabId>('coding');
  // Sidebar split: Agent (daily driver) vs Hub (capability management). The
  // switch doubles as navigation — landing on the group's first tab keeps a
  // visible active item at all times.
  const [sidebarGroup, setSidebarGroup] = useState<'agent' | 'hub'>('agent');
  const switchSidebarGroup = (group: 'agent' | 'hub') => {
    setSidebarGroup(group);
    if (!SIDEBAR_GROUPS[group].includes(tab)) {
      setTab(group === 'agent' ? 'coding' : SIDEBAR_GROUPS.hub[0]);
    }
  };
  const [showSettings, setShowSettings] = useState(false);
  const [showRemoteAccess, setShowRemoteAccess] = useState(false);
  // API Debug is currently off with no toggle surface (the Settings → Coding
  // switch was removed); the plumbing stays so the panel can be re-wired.
  const [debugEnabled] = useState(false);
  // Gate the whole shell on backend health: 'checking' → nothing mounts (so a
  // dead backend never fires the tab mount effects that would spam "No handler
  // registered"), then either a full-screen error or the normal app.
  const [backendState, setBackendState] = useState<'checking' | 'ok' | 'error'>('checking');
  const [backendError, setBackendError] = useState<string | null>(null);
  // Whole-shell replacement (shell:replace, elevated permission): off by
  // default; the user can return to the built-in UI at any time.
  const [shellDismissed, setShellDismissed] = useState(false);
  // Sidebar hosts the persistent Workspaces tree: keep it readable (a 100px
  // column would clip session titles to uselessness).
  const { width: navWidth, onMouseDown: onNavResize } = useResizable(240, 200, 300);

  useEffect(() => {
    window.starriver?.backendHealth?.().then((h) => {
      if (h && !h.ok) {
        setBackendState('error');
        setBackendError(h.error || 'unknown backend init error');
      } else {
        setBackendState('ok');
      }
    }).catch(() => { setBackendState('ok'); /* health unavailable — proceed normally */ });
  }, []);

  // Theme seam: the Theme picker is removed for now (Classic Gray is the only
  // supported theme), so boot forces the default — a stale saved pick (e.g.
  // dark) must not stick around with no picker to undo it. applyCodeTokens
  // is the independent code-highlight palette channel and still applies.
  useEffect(() => {
    applyTheme(null);
    applyCodeTokens();
  }, []);

  // FPM P3: load installed plugins' UI contributions (tabs) into the shell.
  // The plugin-theme re-apply went away with the Theme setting.
  useEffect(() => {
    syncPluginTabs().catch((error) => console.error('[plugin-host] sync failed:', error));
  }, []);

  if (backendState === 'error') return (
    <>
      <BackendErrorScreen error={backendError || ''} />
      <UiLockGate />
    </>
  );
  if (backendState === 'checking') return (
    <>
      <div style={{ height: '100vh', display: 'flex', alignItems: 'center', justifyContent: 'center', background: 'var(--bg-soft)', color: 'var(--text-muted)', fontFamily: 'var(--font)' }}>
        正在启动…
      </div>
      <UiLockGate />
    </>
  );

  const shellReplacement = !shellDismissed ? getShellReplacement() : null;
  if (shellReplacement) {
    return (
      <>
        <div style={{ height: '100vh', display: 'flex', flexDirection: 'column' }}>
          <div style={{
            display: 'flex', alignItems: 'center', justifyContent: 'space-between',
            padding: '6px 12px', borderBottom: '1px solid var(--border)',
            background: 'var(--sidebar-bg)', fontSize: 13, color: 'var(--text-secondary)', flexShrink: 0,
          }}>
            <span>Shell plugin: {shellReplacement.name}</span>
            <button className="btn btn-secondary btn-small" onClick={() => setShellDismissed(true)}>
              {t('settings.backToBuiltin')}
            </button>
          </div>
          <div style={{ flex: 1, minHeight: 0 }}>{shellReplacement.render({ debugEnabled })}</div>
        </div>
        <UiLockGate />
      </>
    );
  }

  return (
    <WorkspaceProvider>
    <div className="app-shell" style={{ display: 'flex', height: '100vh' }}>
      {/* hiddenInset titlebar: a slim always-on-top strip gives the window its
          drag region (move by dragging the top edge; double-click zooms per the
          macOS setting). Interactive UI lives below it. */}
      <div className="titlebar-drag" aria-hidden="true" />
      <aside className="sidebar" style={{ width: navWidth, flexShrink: 0 }}>
        <hr className="sidebar-divider" />
        <SidebarGroupSwitch group={sidebarGroup} onChange={switchSidebarGroup} />
        {sidebarGroup === 'agent' && <SearchBox onOpenSession={() => setTab('coding')} />}
        <SidebarNavItems tab={tab} onNavigate={setTab} group={sidebarGroup} />
        {/* Persistent Workspaces section: selecting a session jumps to the Coding tab */}
        {sidebarGroup === 'agent' && <WorkspaceSection onOpenCoding={() => setTab('coding')} />}
        <div style={{ marginTop: 'auto', display: 'flex', alignItems: 'stretch', borderTop: '1px solid var(--border)' }}>
          <button
            className="nav-item"
            onClick={() => setShowSettings(true)}
            style={{ flex: 1, display: 'flex', alignItems: 'center', gap: 6, borderRadius: 0, paddingTop: 12, paddingBottom: 12 }}
            title={t('settings.title')}
          >
            <span className="nav-icon" dangerouslySetInnerHTML={{ __html: tabIcons.settings || "" }} />
            {t('nav.settings')}
          </button>
          <button
            className="nav-item"
            onClick={() => setShowRemoteAccess(true)}
            style={{ width: 44, display: 'flex', alignItems: 'center', justifyContent: 'center', borderRadius: 0, paddingTop: 12, paddingBottom: 12 }}
            title={t('settings.remoteTitle')}
          >
            <span className="nav-icon" dangerouslySetInnerHTML={{ __html: ICON_PHONE }} />
          </button>
        </div>
      </aside>
      {/* Resize handle */}
      <div onMouseDown={onNavResize} style={{ width: 4, cursor: 'col-resize', background: 'transparent', flexShrink: 0 }}
        onMouseEnter={(e) => (e.target as HTMLElement).style.background = 'var(--bg-soft)'}
        onMouseLeave={(e) => (e.target as HTMLElement).style.background = 'transparent'} />
      <main className="main-content" style={{ flex: 1, minWidth: 0, overflow: 'hidden' }}>
        {(() => {
          const active = getTab(tab);
          return active ? active.render({ debugEnabled }) : null;
        })()}
      </main>
      {showSettings && (
        <SettingsDialog
          onClose={() => setShowSettings(false)}
        />
      )}
      {showRemoteAccess && (
        <RemoteAccessDialog
          onClose={() => setShowRemoteAccess(false)}
        />
      )}
    </div>
    <OpenSessionBridge setTab={setTab} />
    {/* 定时锁覆盖层：最后挂载、fixed 全屏 — 覆盖一切但不卸载应用树（会话继续运行） */}
    <UiLockGate />
    </WorkspaceProvider>
  );
}

/* ─── bootstrap ─── */

// Guarded so importing app.tsx in a test (jsdom has no #root) doesn't crash.
const rootEl = document.getElementById('root');
if (rootEl) {
  const root = createRoot(rootEl);
  root.render(<I18nProvider><App /></I18nProvider>);
}
