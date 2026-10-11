// Shared code-highlight helpers (WI-177 follow-up, 2026-10-04). Lives at
// renderer root: it carries hljs wiring consumed by both app.tsx (command /
// output / result bodies) and diffview.tsx (diff rows), and keeps hex-free
// contrib/ untouched (the palette hexes live in codePaletteDefaults.ts).

import hljs from 'highlight.js/lib/common';

export const HIGHLIGHT_MAX_CHARS = 100_000;
const HIGHLIGHT_LANGS = new Set([
  'bash', 'shell', 'sh', 'zsh', 'powershell', 'pwsh', 'cmd',
  'javascript', 'js', 'jsx', 'typescript', 'ts', 'tsx',
  'python', 'py', 'rust', 'rs', 'go', 'golang', 'ruby', 'rb', 'java',
  'json', 'xml', 'html', 'css', 'scss', 'sql', 'yaml', 'yml', 'diff',
  'markdown', 'md', 'c', 'cpp',
  'kotlin', 'kts', 'swift', 'csharp', 'cs', 'objectivec', 'vbnet',
  'less', 'lua', 'perl', 'php', 'r', 'graphql', 'makefile', 'ini', 'toml',
  'properties', 'ps1',
]);

/** highlight.js → escaped HTML in the language-whitelisted `lang`, or null
 *  (= render as plain text: unknown lang, empty code, or over the size cap —
 *  giant tool logs never pay the highlight cost). */
export function highlightCodeHtml(code: string, lang?: string | null): string | null {
  const base = highlightBase(code, lang);
  if (base === null) return null;
  return isBashLike(lang) ? bashCommandHeuristic(base) : base;
}

/** Whole-code highlight split into per-line HTML (BUG-143): multi-line
 *  strings/comments stay colored across lines instead of being re-tokenized
 *  per line (the old per-line pass painted unterminated-quote lines red and
 *  re-opened fake strings on continuation lines). Returns null for the
 *  plain-text fallback cases (same rules as highlightCodeHtml). */
export function highlightCodeLines(code: string, lang?: string | null): string[] | null {
  const base = highlightBase(code.replace(/\n$/, ''), lang);
  if (base === null) return null;
  const decorated = isBashLike(lang) ? bashCommandHeuristic(base) : base;
  return splitHtmlLines(decorated);
}

function highlightBase(code: string, lang?: string | null): string | null {
  if (!code || !lang || code.length > HIGHLIGHT_MAX_CHARS || !HIGHLIGHT_LANGS.has(lang)) return null;
  try {
    return hljs.highlight(code, { language: lang, ignoreIllegals: true }).value;
  } catch { return null; }
}

const BASH_LIKE = new Set(['bash', 'shell', 'sh', 'zsh']);
function isBashLike(lang?: string | null): boolean {
  return !!lang && BASH_LIKE.has(lang);
}

/** BUG-143 companion: hljs's bash grammar only knows a small common-command
 *  list (cp/ls/echo… — grep/node/cargo/git are plain). Wrap the first word of
 *  every command segment (split on newline/&&/||/;/|/(/) as hljs-built_in,
 *  transparently skipping `VAR=value` environment prefixes (value = no-space
 *  chunk). Operates on hljs PLAIN segments only — strings/keywords are inside
 *  spans and untouched; flags (-x) and paths (/x, $X) never match. */
function bashCommandHeuristic(html: string): string {
  const parts = html.split(/(<span class="hljs-[^"]*">|<\/span>)/);
  let depth = 0; // >0 = inside an hljs span (string/comment/…): never rewrite
  return parts
    .map((part, i) => {
      if (i % 2 === 1) {
        depth += part.startsWith('</') ? -1 : 1;
        return part; // tag — keep verbatim
      }
      if (depth > 0) return part;
      return part
        .split(/(\n|&&|\|\||[;|(])/)
        .map((seg, j) => (j % 2 === 1 ? seg : wrapFirstCommand(seg)))
        .join('');
    })
    .join('');
}

function wrapFirstCommand(segment: string): string {
  let i = 0;
  const ws = () => {
    while (i < segment.length && (segment[i] === ' ' || segment[i] === '\t')) i++;
  };
  ws();
  // Transparently skip a FOO=bar prefix (the value is one space-free blob) so the command's first word still gets colored
  for (;;) {
    const assign = /^[A-Za-z_]\w*=\S*/.exec(segment.slice(i));
    if (!assign) break;
    i += assign[0].length;
    ws();
  }
  const word = /^([a-zA-Z][\w.:-]*)/.exec(segment.slice(i));
  if (!word) return segment;
  return `${segment.slice(0, i)}<span class="hljs-built_in">${word[0]}</span>${segment.slice(i + word[0].length)}`;
}

/** Split highlighted HTML into per-line HTML, carrying open hljs spans across
 *  newlines (close at line end, reopen at next line start). */
function splitHtmlLines(html: string): string[] {
  const lines: string[] = [];
  let out = '';
  const stack: string[] = [];
  const pushLine = () => {
    out += '</span>'.repeat(stack.length);
    lines.push(out);
    out = stack.map((cls) => `<span class="${cls}">`).join('');
  };
  const tagRe = /<span class="([^"]*)">|<\/span>/g;
  let last = 0;
  let m: RegExpExecArray | null;
  const addText = (text: string) => {
    const pieces = text.split('\n');
    pieces.forEach((piece, i) => {
      if (i > 0) pushLine();
      out += piece;
    });
  };
  while ((m = tagRe.exec(html))) {
    addText(html.slice(last, m.index));
    if (m[1] !== undefined) {
      stack.push(m[1]);
      out += m[0];
    } else {
      stack.pop();
      out += m[0];
    }
    last = m.index + m[0].length;
  }
  addText(html.slice(last));
  pushLine();
  return lines;
}

/** Common source-file extensions → language label for the code block. */
const EXT_LANG: Record<string, string> = {
  java: 'java', xml: 'xml', html: 'html', htm: 'html', svg: 'xml', xhtml: 'xml',
  json: 'json', js: 'javascript', mjs: 'javascript', cjs: 'javascript', jsx: 'jsx',
  ts: 'typescript', tsx: 'tsx', py: 'python', rb: 'ruby', go: 'go', rs: 'rust',
  c: 'c', h: 'c', cpp: 'cpp', cc: 'cpp', hpp: 'cpp', cs: 'csharp', kt: 'kotlin',
  kts: 'kotlin', swift: 'swift', sh: 'bash', bash: 'bash', zsh: 'bash', ps1: 'powershell',
  md: 'markdown', markdown: 'markdown', yml: 'yaml', yaml: 'yaml', toml: 'toml',
  ini: 'ini', properties: 'properties', sql: 'sql', css: 'css', scss: 'scss',
  graphql: 'graphql', gradle: 'gradle',
};

/** Derive a language from the file name in the tool label ("read: A.java" → java). */
export function langFromLabel(label: string): string | null {
  const last = label.trim().split(/\s+/).pop() || '';
  const clean = last.replace(/[,;:)\]}"']+$/, '');
  const dot = clean.lastIndexOf('.');
  if (dot <= 0 || dot === clean.length - 1) return null;
  const ext = clean.slice(dot + 1).toLowerCase();
  return EXT_LANG[ext] || null;
}

/** Tools whose result IS file content (whole files / snippets) — the result
 *  body inherits the file's language, derived from the path extension. */
const FILE_CONTENT_TOOLS = new Set(['read', 'write', 'edit', 'patch']);

/** Language for a file-content tool result, from the path's extension
 *  (reuses EXT_LANG/langFromLabel; bash cmdText never matches — gated by tool). */
export function fileResultLang(tname: string, cmdText: string): string | null {
  if (!FILE_CONTENT_TOOLS.has(tname)) return null;
  return langFromLabel(cmdText);
}
