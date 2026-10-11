import React from 'react';
import { highlightCodeHtml, langFromLabel } from './codeLang';

/** Layout modes: 'single' (shipped) — 'split' is the future GitHub-style B layout. */
export type DiffViewMode = 'single' | 'split';

export interface DiffLine {
  kind: 'ctx' | 'add' | 'del';
  oldNum: number | null;
  newNum: number | null;
  text: string;
}

export interface DiffHunk {
  header: string;
  lines: DiffLine[];
}

export interface DiffFile {
  path: string;
  add: number;
  del: number;
  hunks: DiffHunk[];
}

/** True when text looks like a unified diff (--- / +++ file headers or @@ hunks).
 *  git omits the count when it is 1 (`@@ -3 +3 @@`) — accept both forms
 *  (BUG-86: the strict form rejected single-change hunks that parseDiff
 *  itself happily parses). */
export function isDiffContent(text: string): boolean {
  return (
    /^--- .+\n\+\+\+ .+/m.test(text) ||
    /^@@ -\d+(?:,\d+)? \+\d+(?:,\d+)? @@/m.test(text)
  );
}

/** Parse unified diff text into files → hunks → numbered rows. */
export function parseDiff(text: string): DiffFile[] {
  const files: DiffFile[] = [];
  let file: DiffFile | null = null;
  let hunk: DiffHunk | null = null;
  let oldLine = 0;
  let newLine = 0;
  // Hunk body budget from the @@ header: a ---/+++ pair is only a real file
  // header when no hunk body is still open (BUG-90 follow-up: `--- note` /
  // `+++ note` adjacent inside a hunk used to be misparsed as a new file,
  // truncating the rest of the hunk).
  let oldRemaining = 0;
  let newRemaining = 0;
  const bodyConsumed = (kind: 'add' | 'del' | 'ctx') => {
    if (kind !== 'add') oldRemaining -= 1;
    if (kind !== 'del') newRemaining -= 1;
    if (oldRemaining <= 0 && newRemaining <= 0 && hunk && file) {
      file.hunks.push(hunk);
      hunk = null;
    }
  };
  const closeFile = () => {
    if (file) {
      if (hunk) file.hunks.push(hunk);
      hunk = null;
      files.push(file);
      file = null;
    }
  };
  const allLines = text.split('\n');
  for (let li = 0; li < allLines.length; li++) {
    const raw = allLines[li];
    // A real file header is a --- line whose NEXT line is a +++ line;
    // anything else inside a hunk body is content (BUG-90).
    const next = allLines[li + 1] ?? '';
    const isFileHeader =
      raw.startsWith('--- ') && next.startsWith('+++ ') && !hunk;
    // Keep the raw line for hunk-context classification (BUG-85).
    const line = raw;
    if (hunk && (raw.startsWith('--- ') || raw.startsWith('+++ ')) && !isFileHeader) {
      // A ---/+++ line INSIDE a hunk body is content (a deleted "-- note"
      // line renders as "--- note"), not a new file header (BUG-90).
      // Real file headers only appear before the first @@ of a file.
      const m = raw.match(/^[+-]{3} (.*)$/);
      if (m) {
        const path = m[1].replace(/^(a|b)\//, '');
        if (raw.startsWith('--- ') && !hunk) {
          closeFile();
          file = { path, add: 0, del: 0, hunks: [] };
        } else if (raw.startsWith('+++ ') && file && !hunk) {
          // BUG-84: a deleted file's `+++ /dev/null` must not clobber the
          // path captured from `--- a/gone.txt`.
          if (path !== '/dev/null') file.path = path;
        }
        if (hunk && file) {
          if (raw.startsWith('+')) {
            hunk.lines.push({ kind: 'add', oldNum: null, newNum: newLine || null, text: raw.slice(1) });
            file.add += 1;
            if (newLine) newLine += 1;
            bodyConsumed('add');
          } else if (raw.startsWith('-')) {
            hunk.lines.push({ kind: 'del', oldNum: oldLine || null, newNum: null, text: raw.slice(1) });
            file.del += 1;
            if (oldLine) oldLine += 1;
            bodyConsumed('del');
          } else {
            hunk.lines.push({ kind: 'ctx', oldNum: oldLine || null, newNum: newLine || null, text: raw.slice(1) });
            if (oldLine) oldLine += 1;
            if (newLine) newLine += 1;
            bodyConsumed('ctx');
          }
        }
        continue;
      }
    }
    if (isFileHeader || (line.startsWith('+++ ') && file && !hunk)) {
      const m = line.match(/^[+-]{3} (.*)$/);
      if (!m) continue;
      const path = m[1].replace(/^(a|b)\//, '');
      if (line.startsWith('--- ') && isFileHeader) {
        closeFile();
        file = { path, add: 0, del: 0, hunks: [] };
      } else if (file) {
        // BUG-84: keep the --- path for deletions.
        if (path !== '/dev/null') file.path = path;
      }
      continue;
    }
    if (line.startsWith('@@ ')) {
      if (file && hunk) file.hunks.push(hunk);
      const m = line.match(/^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@/);
      oldLine = m ? parseInt(m[1], 10) : 0;
      newLine = m ? parseInt(m[3], 10) : 0;
      oldRemaining = m && m[2] != null ? parseInt(m[2], 10) : 1;
      newRemaining = m && m[4] != null ? parseInt(m[4], 10) : 1;
      hunk = { header: line, lines: [] };
      continue;
    }
    if (!file || !hunk) continue;
    if (line === '' && raw === '') continue; // a truly empty line outside hunk context
    if (line.startsWith('\\')) continue;
    if (line.startsWith('+')) {
      hunk.lines.push({ kind: 'add', oldNum: null, newNum: newLine || null, text: line.slice(1) });
      file.add += 1;
      if (newLine) newLine += 1;
      bodyConsumed('add');
    } else if (line.startsWith('-')) {
      hunk.lines.push({ kind: 'del', oldNum: oldLine || null, newNum: null, text: line.slice(1) });
      file.del += 1;
      if (oldLine) oldLine += 1;
      bodyConsumed('del');
    } else {
      const body = line.startsWith(' ') ? line.slice(1) : line;
      hunk.lines.push({ kind: 'ctx', oldNum: oldLine || null, newNum: newLine || null, text: body });
      if (oldLine) oldLine += 1;
      if (newLine) newLine += 1;
      bodyConsumed('ctx');
    }
  }
  closeFile();
  return files;
}

/** Render a unified diff inside a tool result box (layout A: single column,
 *  per-line numbers, red deletions / green additions). */
export function DiffView({
  text, mode = 'single', maxLines = 50,
}: {
  text: string;
  mode?: DiffViewMode;
  maxLines?: number;
}) {
  const files = React.useMemo(() => parseDiff(text), [text]);
  if (files.length === 0) return null;
  const totalLines = files.reduce((n, f) => n + f.hunks.reduce((m, h) => m + h.lines.length, 0), 0);
  const truncated = totalLines > maxLines;
  let rendered = 0;
  return (
    <div className="diff-view">
      {files.map((f, fi) => {
        // File path extension → language, for per-line syntax coloring (same mapping as "read: A.java")
        const lang = langFromLabel(f.path);
        return (
        <div className="diff-file" key={fi}>
          <div className="diff-file-head">
            <span className="diff-file-path">{f.path}</span>
            <span className="diff-file-stats">
              <span className="diff-stat-add">+{f.add}</span>
              {' '}
              <span className="diff-stat-del">−{f.del}</span>
            </span>
          </div>

          {f.hunks.map((h, hi) => (
            <div className="diff-hunk" key={hi}>
              <div className="diff-hunk-header">{h.header}</div>
              {h.lines.map((l, li) => {
                if (rendered >= maxLines) return null;
                rendered += 1;
                // Per-line syntax coloring by file extension (WI-177 follow-up): diff lines are complete source lines,
                // so splitting independently never breaks neighboring lines; an unterminated string only affects its own line.
                const rowHtml = highlightCodeHtml(l.text, lang);
                return (
                  <div key={li} className={`diff-row ${l.kind}`}>
                    <span className="diff-num">{l.kind === 'del' ? `-${l.oldNum ?? ''}` : (l.newNum ?? '')}</span>
                    <span className="diff-sig">{l.kind === 'add' ? '+' : l.kind === 'del' ? '−' : ' '}</span>
                    <span
                      className="diff-text"
                      dangerouslySetInnerHTML={rowHtml !== null ? { __html: rowHtml } : undefined}
                    >
                      {rowHtml === null ? l.text : undefined}
                    </span>
                  </div>
                );
              })}
            </div>
          ))}
        </div>
        );
      })}
      {truncated && <div className="diff-trunc">… 共 {totalLines} 行，显示前 {maxLines} 行</div>}
    </div>
  );
}
