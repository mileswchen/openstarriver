import React, { useMemo, useState } from 'react';
import ReactMarkdown from 'react-markdown';
import remarkGfm from 'remark-gfm';
import { useI18n } from '../i18n';

/**
 * Compaction fold row: when the model's history is compacted into a checkpoint, the
 * conversation flow gains this marker row — no rendered row is ever removed (compaction
 * affects the model window, not the UI transcript). The checkpoint summary Markdown
 *
 * sits inside a disclosure. Markdown rendering matches app.tsx's MarkdownContent settings
 * (pre language-tag promotion + external links in new windows); implemented separately to avoid a contrib → app cycle.
 */
export function CompactionRow({ summary }: { summary: string }) {
  const { t } = useI18n();
  const [open, setOpen] = useState(false);
  const expandable = summary.trim().length > 0;
  // The first line of the first section serves as the row's preview title (fixed shape of structured checkpoints).
  const label = useMemo(() => {
    const m = summary.match(/^##\s+Primary Request and Intent\s*\n+([^\n#][^\n]*)/m);
    return m?.[1]?.trim() || '';
  }, [summary]);
  return (
    <div className="compaction-row" data-testid="compaction-row">
      <button
        type="button"
        className="compaction-button"
        aria-expanded={expandable ? open : undefined}
        disabled={!expandable}
        onClick={() => expandable && setOpen((v) => !v)}
      >
        <span className="compaction-icon" aria-hidden>🗜</span>
        <span className="compaction-title">{label || t('chat.compactionTitle')}</span>
        <span className="compaction-sep" aria-hidden />
        <span className="compaction-hint" aria-hidden>{expandable ? (open ? '−' : '+') : ''}</span>
      </button>
      {open && expandable && (
        <div className="compaction-body">
          <div className="md-content">
            <ReactMarkdown
              remarkPlugins={[remarkGfm]}
              components={{
                pre({ node: _node, children }) {
                  const el = React.Children.toArray(children)[0] as any;
                  const cls: string | undefined = el?.props?.className;
                  const lang = /language-([\w+-]+)/.exec(cls || '')?.[1];
                  return <pre data-lang={lang || 'code'}>{children}</pre>;
                },
                a({ node: _node, ...rest }) {
                  return <a {...rest} target="_blank" rel="noopener noreferrer" />;
                },
              }}
            >
              {summary || ''}
            </ReactMarkdown>
          </div>
        </div>
      )}
    </div>
  );
}

export default CompactionRow;
