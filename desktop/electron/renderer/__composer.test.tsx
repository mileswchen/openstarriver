/// <reference types="vitest/globals" />
import { describe, it, expect, vi } from 'vitest';
import React, { useRef, useState } from 'react';
import { render, screen, fireEvent } from '@testing-library/react';
import { Composer, detectAttachmentKind, isTextDocument, ToolRow, AssistantContent } from './app';
import { I18nProvider } from './i18n';

const SKILLS: any[] = [
  { name: 'Code Review', description: 'review code', has_command: true, command: 'review' },
  { name: 'Weather', description: 'weather info', has_command: true, command: 'weather' },
];

const MCP_SERVERS: any[] = [
  { name: 'GitHub', transport: 'stdio', command: 'npx', args: [], env: [], url: null, oauthConnected: false, toolCount: 2, status: 'connected' },
  { name: 'Slack', transport: 'http', command: '', args: [], env: [], url: 'https://mcp.slack.com/mcp', oauthConnected: false, toolCount: 5, status: 'disconnected' },
];

/** Controlled harness around the Composer (it is a controlled input). */
function Harness({ value = '', onInsertSkill, skills = SKILLS, mcpServers = [], attachments = [], approval = null, sending = false, stopping = false, onStop = () => {}, permissionPreset, onPermissionPresetChange, permissionLocked }: {
  value?: string; onInsertSkill?: (token: string) => void; skills?: any[]; mcpServers?: any[]; attachments?: any[];
  approval?: { tool: string; reason: string; params?: any; onAllow: () => void; onDeny: () => void; onAllowSimilar?: () => void } | null;
  sending?: boolean; stopping?: boolean; onStop?: () => void;
  permissionPreset?: string; onPermissionPresetChange?: (preset: string) => void;
  permissionLocked?: boolean;
}) {
  const [v, setV] = useState(value);
  const ref = useRef<HTMLTextAreaElement>(null);
  return (
    <Composer
      value={v}
      onChange={setV}
      onKeyDown={() => {}}
      onPaste={() => {}}
      onDrop={() => {}}
      placeholder="msg"
      disabled={sending}
      inputRef={ref}
      attachments={attachments}
      onRemoveAttachment={() => {}}
      onFileSelect={() => {}}
      onSend={() => {}}
      onStop={onStop}
      sending={sending}
      stopping={stopping}
      canSend={!!v.trim()}
      skills={skills}
      mcpServers={mcpServers}
      onInsertSkill={onInsertSkill}
      approval={approval}
      permissionPreset={permissionPreset}
      onPermissionPresetChange={onPermissionPresetChange}
      permissionLocked={permissionLocked}
    />
  );
}

/** Open the + menu, then hover the Skills root item to fly out its sub-panel. */
function openSkillsMenu() {
  fireEvent.click(screen.getByLabelText('add-menu'));
  fireEvent.mouseEnter(screen.getByText('Skills'));
}

/** Open the + menu, then hover the MCP root item to fly out its sub-panel. */
function openMcpMenu() {
  fireEvent.click(screen.getByLabelText('add-menu'));
  fireEvent.mouseEnter(screen.getByText('MCP'));
}

function codeLines(container: HTMLElement): string {
  return Array.from(container.querySelectorAll('.tool-row-code .code-text'))
    .map((el) => el.textContent ?? '')
    .join('\n');
}

describe('Composer — + menu', () => {
  it('renders a single + button to the left of the input', () => {
    render(<I18nProvider><Harness /></I18nProvider>);
    const btn = screen.getByLabelText('add-menu');
    expect(btn).toBeTruthy();
    expect(btn.textContent).toBe('＋');
  });

  it('clicking + opens a menu with Skills / MCP / File entries', () => {
    render(<I18nProvider><Harness /></I18nProvider>);
    expect(screen.queryByText(/Skills/)).toBeNull();
    fireEvent.click(screen.getByLabelText('add-menu'));
    expect(screen.getByText(/Skills/)).toBeTruthy();
    expect(screen.getByText(/MCP/)).toBeTruthy();
    expect(screen.getByText(/File/)).toBeTruthy();
  });

  it('renders inline SVG icons for all three menu entries', () => {
    const { container } = render(<I18nProvider><Harness /></I18nProvider>);
    fireEvent.click(screen.getByLabelText('add-menu'));
    const icons = container.querySelectorAll('.attach-menu-item .attach-menu-icon svg');
    expect(icons.length).toBe(3);
  });

  it('clicking + again closes the menu', () => {
    render(<I18nProvider><Harness /></I18nProvider>);
    fireEvent.click(screen.getByLabelText('add-menu'));
    expect(screen.getByText(/Skills/)).toBeTruthy();
    fireEvent.click(screen.getByLabelText('add-menu'));
    expect(screen.queryByText(/Skills/)).toBeNull();
  });

  it('clicking Skills shows the installed skill list', () => {
    render(<I18nProvider><Harness /></I18nProvider>);
    openSkillsMenu();
    expect(screen.getByText('/review')).toBeTruthy();
    expect(screen.getByText('/weather')).toBeTruthy();
  });

  it('the skill picker search box filters the list', () => {
    render(<I18nProvider><Harness /></I18nProvider>);
    openSkillsMenu();
    fireEvent.change(screen.getByPlaceholderText('Search skills…'), { target: { value: 'wea' } });
    expect(screen.queryByText('/review')).toBeNull();
    expect(screen.getByText('/weather')).toBeTruthy();
  });

  it('selecting a skill inserts its slash token at the caret', () => {
    render(<I18nProvider><Harness value="hello" /></I18nProvider>);
    const ta = screen.getByPlaceholderText('msg') as HTMLTextAreaElement;
    ta.setSelectionRange(2, 2); // caret between "he" and "llo"
    openSkillsMenu();
    fireEvent.mouseDown(screen.getByText('/review')); // items bind insertSkill on mousedown
    expect(ta.value).toBe('he/review llo');
  });

  it('selecting a skill calls onInsertSkill with the token (so the parent can suppress its slash palette)', () => {
    const insertSpy = vi.fn();
    render(<I18nProvider><Harness value="" onInsertSkill={insertSpy} /></I18nProvider>);
    openSkillsMenu();
    fireEvent.mouseDown(screen.getByText('/review'));
    expect(insertSpy).toHaveBeenCalledWith('review');
    const ta = screen.getByPlaceholderText('msg') as HTMLTextAreaElement;
    expect(ta.value).toBe('/review ');
  });

  it('shows an empty state when no skills are installed', () => {
    render(<I18nProvider><Harness skills={[]} /></I18nProvider>);
    openSkillsMenu();
    expect(screen.getByText('—')).toBeTruthy();
  });

  it('clicking MCP lists all MCP instances (name, tool count, status)', () => {
    render(<I18nProvider><Harness mcpServers={MCP_SERVERS} /></I18nProvider>);
    openMcpMenu();
    expect(screen.getByText('GitHub')).toBeTruthy();
    expect(screen.getByText('Slack')).toBeTruthy();
    expect(screen.getByText('2 tool(s)')).toBeTruthy();
    expect(screen.getByText('5 tool(s)')).toBeTruthy();
    expect(screen.getByText('connected')).toBeTruthy();
  });

  it('MCP panel shows an empty state when there are no servers', () => {
    render(<I18nProvider><Harness mcpServers={[]} /></I18nProvider>);
    openMcpMenu();
    expect(screen.getByText(/No MCP servers configured/)).toBeTruthy();
  });

  it('moving the mouse away collapses the flyout', () => {
    render(<I18nProvider><Harness /></I18nProvider>);
    openSkillsMenu();
    expect(screen.getByText('/review')).toBeTruthy();
    fireEvent.mouseLeave(screen.getByText('Skills'));
    expect(screen.queryByText('/review')).toBeNull();
  });

  it('hovering a different root item switches the flyout', () => {
    render(<I18nProvider><Harness mcpServers={MCP_SERVERS} /></I18nProvider>);
    openSkillsMenu();
    expect(screen.getByText('/review')).toBeTruthy();
    fireEvent.mouseEnter(screen.getByText('MCP'));
    expect(screen.queryByText('/review')).toBeNull();
    expect(screen.getByText('GitHub')).toBeTruthy();
  });

  it('the file entry is present and the hidden input accepts documents + images', () => {
    render(<I18nProvider><Harness /></I18nProvider>);
    fireEvent.click(screen.getByLabelText('add-menu'));
    expect(screen.getByText(/File/)).toBeTruthy();
    const input = document.querySelector('input[type=file]') as HTMLInputElement;
    expect(input.accept).toContain('image/*');
    expect(input.accept).toContain('.pdf');
    expect(input.accept).toContain('.md');
    expect(input.accept).toContain('.xlsx');
  });
});

describe('Composer — attachment chips', () => {
  it('renders an image thumbnail for image attachments', () => {
    const { container } = render(<I18nProvider><Harness attachments={[{ id: '1', name: 'pic.png', kind: 'image', dataUrl: 'data:image/png;base64,AAA', state: 'pending' }]} /></I18nProvider>);
    expect(container.querySelector('img')).toBeTruthy();
    expect(screen.getByText('pic.png')).toBeTruthy();
  });

  it('renders a document chip for document attachments (with extracted text)', () => {
    render(<I18nProvider><Harness attachments={[{ id: '2', name: 'readme.md', kind: 'document', dataUrl: '', text: '# hello', state: 'pending' }]} /></I18nProvider>);
    expect(screen.getByText('📄')).toBeTruthy();
    expect(screen.getByText('readme.md')).toBeTruthy();
    expect(screen.queryByText(/no text extracted/)).toBeNull();
  });

  it('flags binary documents that have no extracted text', () => {
    render(<I18nProvider><Harness attachments={[{ id: '3', name: 'report.pdf', kind: 'document', dataUrl: 'data:application/pdf;base64,AAA', state: 'pending' }]} /></I18nProvider>);
    expect(screen.getByText('📄')).toBeTruthy();
    expect(screen.getByText(/no text extracted/)).toBeTruthy();
  });
});

describe('Composer — approval panel', () => {
  it('renders nothing when no approval is pending', () => {
    render(<I18nProvider><Harness /></I18nProvider>);
    expect(screen.queryByText('Allow Once')).toBeNull();
    expect(screen.queryByText('Deny')).toBeNull();
  });

  it('renders header, reason and the command from params when approval is pending', () => {
    render(<I18nProvider><Harness approval={{ tool: 'bash', reason: 'run this command?', params: { command: 'git log --oneline -1' }, onAllow: () => {}, onDeny: () => {}, onAllowSimilar: () => {} }} /></I18nProvider>);
 // header: "Terminal" for bash, not the raw tool id.
    expect(screen.getByText('Terminal')).toBeTruthy();
    expect(screen.getByText('run this command?')).toBeTruthy();
    expect(screen.getByText('Allow Once')).toBeTruthy();
    expect(screen.getByText('Deny')).toBeTruthy();
    // The command to run is shown in a monospace block, taken from params.command.
    const cmd = document.querySelector('.approve-cmd') as HTMLElement;
    expect(cmd).toBeTruthy();
    expect(cmd.textContent).toContain('git log --oneline -1');
 //: caret next to Allow hides "Allow Similar Commands" until opened.
    expect(screen.getByLabelText('more-allow-options')).toBeTruthy();
    expect(screen.queryByText('Allow Similar Commands')).toBeNull();
  });

  it('falls back to the raw tool id when there is no friendly name', () => {
    render(<I18nProvider><Harness approval={{ tool: 'weird_tool', reason: '', onAllow: () => {}, onDeny: () => {} }} /></I18nProvider>);
    expect(screen.getByText('weird_tool')).toBeTruthy();
  });

  it('renders Deny before Allow (Allow is the primary action)', () => {
    const { container } = render(<I18nProvider><Harness approval={{ tool: 'bash', reason: '', onAllow: () => {}, onDeny: () => {} }} /></I18nProvider>);
    const footer = container.querySelector('.approve-footer') as HTMLElement;
    const kids = Array.from(footer.children).map(c => (c as HTMLElement).className);
    const denyIdx = kids.findIndex(k => String(k).includes('approve-deny'));
    const wrapIdx = kids.findIndex(k => String(k).includes('approve-allow-wrap'));
    expect(denyIdx).toBeGreaterThan(-1);
    expect(wrapIdx).toBeGreaterThan(denyIdx); // Deny then Allow Once▾, left→right
  });

  it('renders no caret when onAllowSimilar is not provided', () => {
    render(<I18nProvider><Harness approval={{ tool: 'edit', reason: '', onAllow: () => {}, onDeny: () => {} }} /></I18nProvider>);
    expect(screen.queryByLabelText('more-allow-options')).toBeNull();
  });

  it('places the approval panel above the input bar', () => {
    const { container } = render(<I18nProvider><Harness approval={{ tool: 'edit', reason: '', onAllow: () => {}, onDeny: () => {} }} /></I18nProvider>);
    const wrap = container.querySelector('.composer-wrap') as HTMLElement;
    const kids = Array.from(wrap.children).map(c => (c as HTMLElement).className);
    const panelIdx = kids.findIndex(k => String(k).includes('approve-panel'));
    const barIdx = kids.findIndex(k => String(k).includes('input-bar'));
    expect(panelIdx).toBeGreaterThan(-1);
    // The panel is a sibling rendered above (before) the input bar.
    expect(panelIdx).toBeLessThan(barIdx);
  });

  it('calls onAllow / onDeny when the buttons are clicked', () => {
    const onAllow = vi.fn();
    const onDeny = vi.fn();
    render(<I18nProvider><Harness approval={{ tool: 'bash', reason: '', onAllow, onDeny, onAllowSimilar: () => {} }} /></I18nProvider>);
    fireEvent.click(screen.getByText('Deny'));
    expect(onDeny).toHaveBeenCalledTimes(1);
    fireEvent.click(screen.getByText('Allow Once'));
    expect(onAllow).toHaveBeenCalledTimes(1);
  });

  it('opens "Allow Similar Commands" from the caret and calls onAllowSimilar on select', () => {
    const onAllowSimilar = vi.fn();
    render(<I18nProvider><Harness approval={{ tool: 'bash', reason: '', onAllow: () => {}, onDeny: () => {}, onAllowSimilar }} /></I18nProvider>);
    expect(screen.queryByText('Allow Similar Commands')).toBeNull();
    fireEvent.click(screen.getByLabelText('more-allow-options'));
    // Both choices are available once expanded (Allow Once is also listed in the menu).
    expect(screen.getAllByText('Allow Once').length).toBe(2);
    expect(screen.getByText('Allow Similar Commands')).toBeTruthy();
    fireEvent.click(screen.getByText('Allow Similar Commands'));
    expect(onAllowSimilar).toHaveBeenCalledTimes(1);
    // Menu closes after selecting.
    expect(screen.queryByText('Allow Similar Commands')).toBeNull();
  });

  it('closes the "Allow Similar Commands" menu on outside click', () => {
    render(<I18nProvider><Harness approval={{ tool: 'bash', reason: '', onAllow: () => {}, onDeny: () => {}, onAllowSimilar: () => {} }} /></I18nProvider>);
    fireEvent.click(screen.getByLabelText('more-allow-options'));
    expect(screen.getByText('Allow Similar Commands')).toBeTruthy();
    fireEvent.mouseDown(document.body);
    expect(screen.queryByText('Allow Similar Commands')).toBeNull();
  });
});

describe('detectAttachmentKind / isTextDocument', () => {
  it('classifies images by mime or extension', () => {
    expect(detectAttachmentKind('photo.png', 'image/png')).toBe('image');
    expect(detectAttachmentKind('photo.jpg', '')).toBe('image');
    expect(detectAttachmentKind('x.WEBP', '')).toBe('image');
  });

  it('classifies everything else as a document', () => {
    expect(detectAttachmentKind('report.pdf', 'application/pdf')).toBe('document');
    expect(detectAttachmentKind('notes.txt', 'text/plain')).toBe('document');
    expect(detectAttachmentKind('book.docx', 'application/vnd.openxmlformats-officedocument.wordprocessingml.document')).toBe('document');
  });

  it('recognizes plain-text documents for client-side extraction', () => {
    expect(isTextDocument('a.txt', 'text/plain')).toBe(true);
    expect(isTextDocument('a.md', '')).toBe(true);
    expect(isTextDocument('data.json', 'application/json')).toBe(true);
    expect(isTextDocument('config.yaml', '')).toBe(true);
    expect(isTextDocument('a.pdf', 'application/pdf')).toBe(false);
    expect(isTextDocument('a.xlsx', 'application/vnd.openxmlformats-officedocument.spreadsheetml.sheet')).toBe(false);
  });
});

describe('ToolRow', () => {
  /// Diff-bearing results (edit/write/patch) offer review: the click jumps the
  /// diff into the right-hand side panel with the parsed file path.
  it('offers 审查 on diff results and opens the panel with path + diff text', () => {
    localStorage.setItem('starriver-language', 'zh');
    const onOpenDiff = vi.fn();
    const text = '--- crates/x.rs\n+++ crates/x.rs\n@@ -1,1 +1,2 @@\n one\n+two\n';
    render(
      <I18nProvider>
        <ToolRow content="🔧 edit: crates/x.rs" result={text} ok={true} collapsible onOpenDiff={onOpenDiff} />
      </I18nProvider>,
    );
    fireEvent.click(screen.getByText('审查'));
    expect(onOpenDiff).toHaveBeenCalledWith('crates/x.rs', text);
    // Restore the file-wide default locale (later tests assert English labels).
    localStorage.setItem('starriver-language', 'en');
  });

  it('non-diff results carry no 审查 button', () => {
    render(
      <I18nProvider>
        <ToolRow content="🔧 bash: git status" result="On branch main" ok={true} collapsible />
      </I18nProvider>,
    );
    expect(screen.queryByText('审查')).toBeNull();
  });

  it('strips the 🔧 prefix, the ✓ marker and renders the label/output text', () => {
    const { container } = render(<I18nProvider><ToolRow content={'🔧 patch: mycontext/pom.xml\n✓ created pom.xml'} /></I18nProvider>);
    const label = container.querySelector('.tool-row-label') as HTMLElement;
    const body = container.querySelector('.tool-row-status') as HTMLElement;
    expect(screen.queryByText('🔧')).toBeNull();
    expect(screen.queryByText(/✓/)).toBeNull();
    expect(label.textContent).toBe('patch: mycontext/pom.xml');
    expect(body.textContent).toBe('created pom.xml');
    expect(container.querySelector('.tool-row')).toBeTruthy();
  });

  it('renders an icon chip for labeled rows', () => {
    const { container } = render(<I18nProvider><ToolRow content="🔧 bash: git commit" /></I18nProvider>);
    expect(container.querySelector('.tool-row-icon svg')).toBeTruthy();
  });

  it('renders no status badge for successful outputs', () => {
    const { container } = render(<I18nProvider><ToolRow content={'🔧 read: notes.txt\n✓ hello'} /></I18nProvider>);
    expect(container.querySelector('.tool-row-status .status-icon')).toBeNull();
    expect(container.querySelector('.tool-row-status')?.textContent).toBe('hello');
  });

  it('renders a red status badge only when the tool failed', () => {
    const { container } = render(<I18nProvider><ToolRow content={'🔧 read: missing.txt\n✗ ENOENT: no such file'} /></I18nProvider>);
    const badge = container.querySelector('.tool-row-status .status-icon') as HTMLElement;
    expect(badge).toBeTruthy();
    expect(badge.getAttribute('class')).toContain('status-err');
    expect(screen.queryByText(/✗/)).toBeNull();
  });

  it('strips a trailing ✓ on the label with no badge (success)', () => {
    const { container } = render(<I18nProvider><ToolRow content="🔧 read: notes.txt ✓" /></I18nProvider>);
    const label = container.querySelector('.tool-row-label') as HTMLElement;
    expect(screen.queryByText(/✓/)).toBeNull();
    expect(label.textContent).toBe('read: notes.txt');
    expect(container.querySelector('.tool-row-label .status-icon')).toBeNull();
  });

  it('splits a live failure into label + red badge + error text', () => {
    const { container } = render(<I18nProvider><ToolRow content="🔧 read: missing.txt ✗ ENOENT: no such file" /></I18nProvider>);
    expect(screen.queryByText(/✗/)).toBeNull();
    expect(container.querySelector('.tool-row-label')?.textContent).toBe('read: missing.txt');
    expect(container.querySelector('.tool-row-status .status-err')).toBeTruthy();
    expect(container.querySelector('.tool-row-status')?.textContent).toBe('ENOENT: no such file');
  });

  it('renders bash commands as their own bash code block (not inline text)', () => {
    const cmd = 'find /Users/x/m -name ".DS_Store" -delete 2>/dev/null; echo "===" && find /Users/x/m -type f';
    const { container } = render(<I18nProvider><ToolRow content={'🔧 bash: ' + cmd} /></I18nProvider>);
    const label = container.querySelector('.tool-row-label') as HTMLElement;
    const pre = container.querySelector('.tool-row-code') as HTMLElement;
    expect(label.textContent).toBe('bash:');
    expect(pre).toBeTruthy();
    expect(pre.getAttribute('data-lang')).toBe('bash');
    expect(pre.textContent).toBe(cmd);
  });

  it('splits short bash commands into label + code block too', () => {
    const { container } = render(<I18nProvider><ToolRow content="🔧 bash: git status" /></I18nProvider>);
    expect(container.querySelector('.tool-row-label')?.textContent).toBe('bash:');
    expect(container.querySelector('.tool-row-code')?.getAttribute('data-lang')).toBe('bash');
    expect(codeLines(container)).toBe('git status');
  });

  it('does not render a command block for a bare command tool label', () => {
    const { container } = render(<I18nProvider><ToolRow content="🔧 bash" /></I18nProvider>);
    expect(container.querySelector('.tool-row-code')).toBeNull();
    expect(container.querySelector('.tool-row-label')?.textContent).toBe('bash');
  });

  it('keeps the bash output as body text after the command block', () => {
    const { container } = render(<I18nProvider><ToolRow content={'🔧 bash: git status\n✓ clean'} /></I18nProvider>);
    expect(codeLines(container)).toBe('git status');
    expect(container.querySelector('.tool-row-status')?.textContent).toBe('clean');
    expect(container.querySelector('.tool-row-status .status-icon')).toBeNull();
  });

  it('renders a history multiline bash command as ONE code block (escaped \n)', () => {
    // History labels flatten newlines to literal "\n" (tool_call_label in Rust);
    // ToolRow must unescape and box the whole command.
    const { container } = render(<I18nProvider><ToolRow content={'🔧 bash: # 注释\\nls -la; echo done'} /></I18nProvider>);
    const pre = container.querySelector('.tool-row-code') as HTMLElement;
    expect(container.querySelector('.tool-row-label')?.textContent).toBe('bash:');
    expect(pre).toBeTruthy();
    expect(pre.getAttribute('data-lang')).toBe('bash');
    expect(codeLines(pre)).toBe('# 注释\nls -la; echo done');
    // No leftover plain-text status row.
    expect(container.querySelector('.tool-row-status')?.textContent ?? '').toBe('');
  });

  it('renders a live multiline bash command as ONE code block with trailing ✓', () => {
    const { container } = render(<I18nProvider><ToolRow content={'🔧 bash: # 注释\nls -la; echo done ✓'} /></I18nProvider>);
    const pre = container.querySelector('.tool-row-code') as HTMLElement;
    expect(container.querySelector('.tool-row-label')?.textContent).toBe('bash:');
    expect(pre).toBeTruthy();
    expect(codeLines(pre)).toBe('# 注释\nls -la; echo done');
    expect(screen.queryByText(/✓/)).toBeNull();
    expect(container.querySelector('.tool-row-status')?.textContent ?? '').toBe('');
  });

  it('boxes the body when the label is just "bash:"', () => {
    const { container } = render(<I18nProvider><ToolRow content={'bash:\n# 注释\nls -la'} /></I18nProvider>);
    const pre = container.querySelector('.tool-row-code') as HTMLElement;
    expect(container.querySelector('.tool-row-label')?.textContent).toBe('bash:');
    expect(pre).toBeTruthy();
    expect(codeLines(pre)).toBe('# 注释\nls -la');
  });

  it('renders no icon chip for unlabeled rows (bare fallback output)', () => {
    const { container } = render(<I18nProvider><ToolRow content="✓ created pom.xml" /></I18nProvider>);
    expect(container.querySelector('.tool-row-icon')).toBeNull();
    expect(screen.queryByText(/✓/)).toBeNull();
    expect(container.querySelector('.tool-row-label')?.textContent).toBe('created pom.xml');
    expect(container.querySelector('.tool-row-label .status-icon')).toBeNull();
  });

  it('shows "(processing...)" while streaming with empty content', () => {
    render(<I18nProvider><ToolRow content="" streaming /></I18nProvider>);
    expect(screen.getByText('(processing...)')).toBeTruthy();
  });

  it('renders XML outputs as a dark code block with a language label', () => {
    const { container } = render(<I18nProvider><ToolRow content={'🔧 read: mycontext/pom.xml\n✓ <?xml version="1.0"?>\n<project>…</project>'} /></I18nProvider>);
    const pre = container.querySelector('.tool-row-code') as HTMLElement;
    expect(pre).toBeTruthy();
    expect(pre.getAttribute('data-lang')).toBe('xml');
    // The ✓ marker is stripped inside the code block; no badge for success.
    expect(pre.textContent).toBe('<?xml version="1.0"?>\n<project>…</project>');
    expect(container.querySelector('.tool-row-code-wrap .status-icon')).toBeNull();
  });

  it('keeps the XML declaration on the first line inside the code block', () => {
    // Stored tool results prefix "✓ " then the output — when the output starts
    // with "<?xml", the declaration must stay INSIDE the code block, not render
    // as a separate label row.
    const xml = '<?xml version="1.0" encoding="UTF-8"?>\n<project xmlns="http://maven.apache.org/POM/4.0.0">\n  <modelVersion>4.0.0</modelVersion>\n</project>';
    const { container } = render(<I18nProvider><ToolRow content={'✓ ' + xml} /></I18nProvider>);
    const pre = container.querySelector('.tool-row-code') as HTMLElement;
    expect(pre).toBeTruthy();
    expect(pre.getAttribute('data-lang')).toBe('xml');
    expect(codeLines(container)).toBe(xml);
    expect(container.querySelector('.tool-row-label')).toBeNull();
    expect(container.querySelector('.tool-row-status')).toBeNull();
  });

  it('keeps JSON starting on the first line inside the code block', () => {
    const { container } = render(<I18nProvider><ToolRow content={'✓ {"name":"mycontext","version":"1.0.0"}'} /></I18nProvider>);
    const pre = container.querySelector('.tool-row-code') as HTMLElement;
    expect(pre).toBeTruthy();
    expect(pre.getAttribute('data-lang')).toBe('json');
    expect(pre.textContent).toBe('{"name":"mycontext","version":"1.0.0"}');
    expect(container.querySelector('.tool-row-label')).toBeNull();
  });

  it('renders JSON outputs as a code block with a language label', () => {
    const { container } = render(<I18nProvider><ToolRow content={'🔧 read: data.json\n✓ {"name": "x", "ok": true}'} /></I18nProvider>);
    const pre = container.querySelector('.tool-row-code') as HTMLElement;
    expect(pre).toBeTruthy();
    expect(pre.getAttribute('data-lang')).toBe('json');
  });

  it('keeps non-code outputs as plain text', () => {
    const { container } = render(<I18nProvider><ToolRow content={'🔧 read: notes.txt\n✓ hello\nworld'} /></I18nProvider>);
    expect(container.querySelector('.tool-row-code')).toBeNull();
    expect(screen.getByText(/hello/)).toBeTruthy();
    expect(screen.getByText(/world/)).toBeTruthy();
  });

  it('renders Java source using the file extension from the label', () => {
    const java = '✓ package com.mycontext.propagation;\nimport java.util.concurrent.*;\n\nclass A {\n  int x = 1;\n}';
    const { container } = render(<I18nProvider><ToolRow content={'🔧 read: ContextPropagatorTest.java\n' + java} /></I18nProvider>);
    const pre = container.querySelector('.tool-row-code') as HTMLElement;
    expect(pre).toBeTruthy();
    expect(pre.getAttribute('data-lang')).toBe('java');
    expect(pre.textContent).toContain('package com.mycontext.propagation;');
  });

  it('does not wrap a one-line status in a code block even for code-file labels', () => {
    const { container } = render(<I18nProvider><ToolRow content={'🔧 patch: ContextPropagatorTest.java\n✓ created /path/ContextPropagatorTest.java'} /></I18nProvider>);
    expect(container.querySelector('.tool-row-code')).toBeNull();
  });

  describe('collapsible ToolRow (Coding card)', () => {
    it('renders the three-part header: icon + title + command summary + chevron, folded by default', () => {
      const { container } = render(<I18nProvider><ToolRow content={'🔧 bash: which java && java -version 2>&1'} collapsible /></I18nProvider>);
      const details = container.querySelector('.tool-row-details') as HTMLDetailsElement;
      expect(details).toBeTruthy();
      expect(details.open).toBe(false);
      // Exactly one icon — inside the summary header's leading morph slot, no
      // duplicate outer icon.
      expect(container.querySelectorAll('.disclosure-icon svg').length).toBe(1);
      expect(container.querySelector('.tool-row-title')?.textContent).toBe('Shell');
      expect(container.querySelector('.tool-row-summary-text')?.textContent).toContain('which java && java -version');
      expect(container.querySelector('.disclosure-chevron')).toBeTruthy();
    });

    it('keeps streaming cards open so the command is visible', () => {
      const { container } = render(<I18nProvider><ToolRow content={'🔧 bash: git status'} streaming collapsible /></I18nProvider>);
      const details = container.querySelector('.tool-row-details') as HTMLDetailsElement;
      expect(details.open).toBe(true);
      expect(codeLines(container)).toBe('git status');
    });

    it('toggles the detail when the header is clicked', () => {
      const { container } = render(<I18nProvider><ToolRow content={'🔧 bash: git status'} collapsible /></I18nProvider>);
      const details = container.querySelector('.tool-row-details') as HTMLDetailsElement;
      const summary = container.querySelector('.tool-row-summary') as HTMLElement;
      expect(details.open).toBe(false);
      fireEvent.click(summary);
      expect(details.open).toBe(true);
      expect(codeLines(container)).toBe('git status');
      fireEvent.click(summary);
      expect(details.open).toBe(false);
    });

    it('shows the result in a boxed block with a success mark (result prop)', () => {
      const { container } = render(
        <I18nProvider><ToolRow content={'🔧 bash: which java && java -version 2>&1'} result={'/usr/bin/java\nUnable to locate Java.'} ok collapsible /></I18nProvider>
      );
      const result = container.querySelector('.tool-row-result') as HTMLElement;
      expect(result).toBeTruthy();
      expect(result.querySelector('.tool-row-result-head .status-ok')).toBeTruthy();
      expect(result.querySelector('.tool-row-result-head')?.textContent).toContain('Success');
      expect(result.querySelector('.tool-row-result-body')?.textContent).toContain('/usr/bin/java');
    });

    it('marks failed results with a red badge', () => {
      const { container } = render(<I18nProvider><ToolRow content={'🔧 bash: mvn compile'} result={'mvn: command not found'} ok={false} collapsible /></I18nProvider>);
      const result = container.querySelector('.tool-row-result') as HTMLElement;
      expect(result.classList.contains('is-err')).toBe(true);
      expect(result.querySelector('.tool-row-result-head .status-err')).toBeTruthy();
      expect(result.querySelector('.tool-row-result-head')?.textContent).toContain('Failed');
    });

    it('uses the inline stored output as the result when no result prop is given', () => {
      const { container } = render(<I18nProvider><ToolRow content={'🔧 bash: git status\n✓ clean'} collapsible /></I18nProvider>);
      fireEvent.click(container.querySelector('.tool-row-summary') as HTMLElement);
      expect(container.querySelector('.tool-row-result-head')?.textContent).toContain('Success');
      expect(container.querySelector('.tool-row-result-body')?.textContent).toBe('clean');
    });

    it('boxes XML results in a collapsible card with a lang summary', () => {
      const xml = '<?xml version="1.0" encoding="UTF-8"?>\n<project/>';
      const { container } = render(<I18nProvider><ToolRow content={'✓ ' + xml} collapsible /></I18nProvider>);
      const details = container.querySelector('.tool-row-details') as HTMLDetailsElement;
      expect(details).toBeTruthy();
      expect(container.querySelector('.tool-row-title')?.textContent).toBe('XML');
      expect(container.querySelector('.tool-row-summary-text')?.textContent).toContain('<?xml');
      fireEvent.click(container.querySelector('.tool-row-summary') as HTMLElement);
      expect(details.open).toBe(true);
      expect(container.querySelector('.tool-row-result-body')?.textContent).toContain('<project/>');
    });

    it('renders a diff view when the result is a unified diff', () => {
      const diff = '--- a.txt\n+++ a.txt\n@@ -1,1 +1,1 @@\n-old\n+new\n';
      const { container } = render(<I18nProvider><ToolRow content={'🔧 edit: a.txt'} result={diff} ok collapsible /></I18nProvider>);
      fireEvent.click(container.querySelector('.tool-row-summary') as HTMLElement);
      expect(container.querySelector('.diff-view')).toBeTruthy();
      expect(container.querySelector('.diff-row.add')?.textContent).toContain('new');
      expect(container.querySelector('.diff-row.del')?.textContent).toContain('old');
    });

    it('renders plain result text when it is not a diff', () => {
      const { container } = render(<I18nProvider><ToolRow content={'🔧 bash: echo hi'} result={'hi'} ok collapsible /></I18nProvider>);
      fireEvent.click(container.querySelector('.tool-row-summary') as HTMLElement);
      expect(container.querySelector('.diff-view')).toBeNull();
      expect(container.querySelector('.tool-row-result-body')?.textContent).toBe('hi');
    });
  });
});

describe('AssistantContent', () => {
  it('renders plain prose as markdown only', () => {
    const { container } = render(<I18nProvider><AssistantContent content="Let me check." /></I18nProvider>);
    expect(container.querySelector('.md-content')).toBeTruthy();
    expect(container.querySelector('.tool-row')).toBeNull();
    expect(screen.getByText('Let me check.')).toBeTruthy();
  });

  it('pulls 🔧 tool lines out of history-rebuilt assistant bubbles into ToolRows', () => {
    const { container } = render(<I18nProvider><AssistantContent content={'Cleaning up.\n🔧 bash: find . -name ".DS_Store" -delete\nDone.'} /></I18nProvider>);
    expect(screen.queryByText(/🔧/)).toBeNull();
    const rows = container.querySelectorAll('.tool-row');
    expect(rows.length).toBe(1);
    // The bash command is a code block, separated from the surrounding prose.
    expect(rows[0].querySelector('.tool-row-code')?.getAttribute('data-lang')).toBe('bash');
    expect(rows[0].querySelector('.tool-row-code')?.textContent).toBe('find . -name ".DS_Store" -delete');
    // The prose stays as markdown segments before and after the tool row.
    expect(screen.getByText('Cleaning up.')).toBeTruthy();
    expect(screen.getByText('Done.')).toBeTruthy();
  });

  it('handles multiple tool lines interleaved with text', () => {
    const { container } = render(<I18nProvider><AssistantContent content={'Step one\n🔧 bash: git add -A\n🔧 read: pom.xml\nStep two'} /></I18nProvider>);
    expect(container.querySelectorAll('.tool-row').length).toBe(2);
    expect(screen.getByText('Step one')).toBeTruthy();
    expect(screen.getByText('Step two')).toBeTruthy();
  });
});

describe('Composer — stop button', () => {
  it('shows the localized Send when idle (zh)', () => {
    localStorage.setItem('starriver-language', 'zh');
    render(<I18nProvider><Harness /></I18nProvider>);
    expect(screen.getByText('发送')).toBeTruthy();
  });

  it('shows enabled 结束 while sending and fires onStop on click (zh)', () => {
    localStorage.setItem('starriver-language', 'zh');
    const onStop = vi.fn();
    render(<I18nProvider><Harness sending onStop={onStop} /></I18nProvider>);
    const btn = screen.getByText('结束') as HTMLButtonElement;
    expect(btn.disabled).toBe(false);
    fireEvent.click(btn);
    expect(onStop).toHaveBeenCalledTimes(1);
  });

  it('shows disabled 停止中… while stopping (zh)', () => {
    localStorage.setItem('starriver-language', 'zh');
    render(<I18nProvider><Harness sending stopping /></I18nProvider>);
    const btn = screen.getByText('停止中…') as HTMLButtonElement;
    expect(btn.disabled).toBe(true);
    expect(screen.queryByText('Send')).toBeNull();
  });

  it('shows English labels when language is en', () => {
    localStorage.setItem('starriver-language', 'en');
    render(<I18nProvider><Harness sending /></I18nProvider>);
    const btn = screen.getByText('Stop') as HTMLButtonElement;
    expect(btn.disabled).toBe(false);
  });
});

describe('Composer — permission preset picker', () => {
  it('shows the current preset label (i18n) with a monochrome SVG icon', () => {
    localStorage.setItem('starriver-language', 'en');
    const { container } = render(<I18nProvider><Harness permissionPreset="manual" onPermissionPresetChange={() => {}} /></I18nProvider>);
    const btn = screen.getByLabelText('permission-preset');
    expect(screen.queryByText('手动确认 · 工作区写入')).toBeNull(); // no hardcoded Chinese in en
    expect(btn.textContent).toContain('Manual');
    const icon = btn.querySelector('.composer-approval-icon svg');
    expect(icon).toBeTruthy();
    // Monochrome SVG: currentColor stroke, no fill color values
    expect(icon.getAttribute('stroke')).toBe('currentColor');
  });

  it('opens the menu with the preset table and reports the picked one', () => {
    localStorage.setItem('starriver-language', 'en');
    const onPermissionPresetChange = vi.fn();
    render(<I18nProvider><Harness permissionPreset="manual" onPermissionPresetChange={onPermissionPresetChange} /></I18nProvider>);
    fireEvent.click(screen.getByLabelText('permission-preset'));
    expect(screen.getByText('Accept-Edits')).toBeTruthy();
    expect(screen.getByText('Auto')).toBeTruthy();
    expect(screen.getByText('Full access')).toBeTruthy();
    fireEvent.click(screen.getByText('Auto'));
    expect(onPermissionPresetChange).toHaveBeenCalledWith('auto');
    // picking collapses the menu
    expect(screen.queryByText('Full access')).toBeNull();
  });

  it('routes the danger preset through the risk modal before writing', () => {
    localStorage.setItem('starriver-language', 'en');
    const onPermissionPresetChange = vi.fn();
    render(<I18nProvider><Harness permissionPreset="manual" onPermissionPresetChange={onPermissionPresetChange} /></I18nProvider>);
    fireEvent.click(screen.getByLabelText('permission-preset'));
    fireEvent.click(screen.getByText('Full access'));
    // The write waits for the risk confirmation.
    expect(onPermissionPresetChange).not.toHaveBeenCalled();
    expect(screen.getByText('Enable full access?')).toBeTruthy();
    const confirm = screen.getByText('Enable').closest('button') as HTMLButtonElement;
    expect(confirm.disabled).toBe(true);
    fireEvent.click(screen.getByLabelText('risk-acknowledge'));
    expect(confirm.disabled).toBe(false);
    fireEvent.click(confirm);
    expect(onPermissionPresetChange).toHaveBeenCalledWith('danger-full-access');
    expect(screen.queryByText('Enable full access?')).toBeNull();
  });

  it('stays closed until clicked and marks the active item', () => {
    localStorage.setItem('starriver-language', 'en');
    render(<I18nProvider><Harness permissionPreset="accept-edits" onPermissionPresetChange={() => {}} /></I18nProvider>);
    expect(screen.queryByText('Full access')).toBeNull();
    fireEvent.click(screen.getByLabelText('permission-preset'));
    // the label appears twice (trigger + menu item); the menu one carries .active
    const active = screen.getAllByText('Accept-Edits')
      .map(el => el.closest('button'))
      .find(btn => btn?.className.includes('composer-approval-item'));
    expect(active?.className).toContain('active');
  });

  it('shows the derived custom label for non-table combos', () => {
    localStorage.setItem('starriver-language', 'en');
    render(<I18nProvider><Harness permissionPreset="custom" onPermissionPresetChange={() => {}} /></I18nProvider>);
    const btn = screen.getByLabelText('permission-preset');
    expect(btn.textContent).toContain('Custom');
  });
});

describe('Composer — permission picker locked (Plan)', () => {
  it('disables the trigger and hides the caret when locked', () => {
    localStorage.setItem('starriver-language', 'en');
    const onPermissionPresetChange = vi.fn();
    render(<I18nProvider><Harness permissionPreset="manual" permissionLocked onPermissionPresetChange={onPermissionPresetChange} /></I18nProvider>);
    const btn = screen.getByLabelText('permission-preset') as HTMLButtonElement;
    expect(btn.disabled).toBe(true);
    expect(screen.queryByText('Full access')).toBeNull(); // menu never opens
    fireEvent.click(btn);
    expect(screen.queryByText('Full access')).toBeNull();
    expect(onPermissionPresetChange).not.toHaveBeenCalled();
  });
});
