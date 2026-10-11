// WI-146 regression guard: the tool-logo refresh. Pins the icon resolution
// table behaviorally (toolIconFor is exported from app.tsx):
//   - read-family (read/grep/glob/ls/find/lsp_*) → magnifier
//   - MCP bridge tools (mcp_<server>_<tool>) → plug
//   - git splits read/write by subcommand: mutators (commit/push/…) → commit
//     dot, everything else (status/log/diff/no hint) → branch
//   - previously unmapped tools (todo_write/ask_user/subagent_spawn/…)
//     now resolve to dedicated glyphs instead of the empty-square fallback,
//     and terminal_*/web_fetch ride the terminal/globe icons.
import { describe, it, expect } from 'vitest';
import { toolIconFor } from './app';

const TAG = (svg: string) => svg;

describe('tool icon resolution (WI-146)', () => {
  it('read-family tools resolve to the magnifier', () => {
    for (const name of ['read', 'grep', 'glob', 'ls', 'find', 'lsp_symbols', 'lsp_diagnostics']) {
      expect(toolIconFor(name)).toContain('circle cx="11" cy="11" r="8"');
      expect(toolIconFor(name)).not.toContain('M4 19.5v-15'); // the retired book glyph
    }
  });

  it('mcp bridge tools resolve to the plug regardless of server/tool suffix', () => {
    for (const name of ['mcp_context7_get-docs', 'mcp_nexplan_bug_add']) {
      const icon = toolIconFor(name);
      expect(icon).toContain('M18 8v5a4 4 0 0 1-4 4h-4a4 4 0 0 1-4-4V8Z');
    }
  });

  it('git splits read/write by subcommand', () => {
    const branchMark = 'cx="18" cy="6" r="3"';
    const commitMark = 'circle cx="12" cy="12" r="3.5"';
    // Mutators → commit dot.
    for (const cmd of ['git commit -m ok', 'git push origin main', 'GIT PULL', 'git rebase -i']) {
      expect(toolIconFor('git', cmd)).toContain(commitMark);
    }
    // Inspections → branch.
    for (const cmd of ['git status -b', 'git log --oneline', 'git diff HEAD~1', 'git show abc']) {
      expect(toolIconFor('git', cmd)).toContain(branchMark);
    }
    // No/unknown hint defaults to branch (inspection is the safer read).
    expect(toolIconFor('git')).toContain(branchMark);
    expect(toolIconFor('git', 'blorp')).toContain(branchMark);
    // A mutator verb only counts as a standalone word — "digit" must not trip
    // the `git` matcher ("di**git**").
    expect(toolIconFor('git', 'git status --digit=hours')).toContain(branchMark);
  });

  it('previously unmapped tools get dedicated glyphs (not the empty square)', () => {
    const square = '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="3.5" y="3.5" width="17" height="17" rx="4"/></svg>';
    for (const name of ['todo_write', 'ask_user', 'exit_plan_mode', 'plan_write', 'goal_write',
      'generate_image', 'reload_image', 'session_reference', 'subagent_spawn', 'python_exec']) {
      expect(toolIconFor(name), name).not.toBe(square);
    }
    // Spot-check two shapes.
    expect(toolIconFor('goal_write')).toContain('r="10"');
    expect(toolIconFor('subagent_spawn')).toContain('x="4" y="8"');
    // WI-174: todo_write rides the ListTodo glyph (checkbox + todo rows).
    expect(toolIconFor('todo_write')).toContain('<rect x="3" y="5" width="6" height="6" rx="1"/>');
  });

  it('terminal_* ride the terminal icon and web_fetch the globe', () => {
    for (const name of ['terminal_start', 'terminal_read', 'terminal_kill']) {
      // WI-148: the terminal frame is square (18×18) like the other framed glyphs.
      expect(toolIconFor(name)).toContain('rect x="3" y="3" width="18" height="18" rx="3"');
    }
    expect(toolIconFor('web_fetch')).toContain('r="10"');
    // Unmapped names still fall back to the empty square.
    expect(toolIconFor('who_knows')).toContain('rect x="3.5" y="3.5"');
  });

  it('every icon is a 16x16 house-style svg', () => {
    for (const icon of [TAG(toolIconFor('grep')), TAG(toolIconFor('git', 'git push')), TAG(toolIconFor('mcp_a_b'))]) {
      expect(icon.startsWith('<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"')).toBe(true);
    }
  });
});
