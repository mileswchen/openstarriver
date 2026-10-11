// Round-10 regressions: contrib registry semantics that were silently broken.
import { describe, it, expect } from 'vitest';
import React from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import {
  registerConversationNode,
  removeConversationNodes,
  removeConversationNodesByOwnerPrefix,
  renderTemplateNode,
  renderToolNode,
} from './contrib/conversationNodes';
import { contribute, contributors } from './contrib/slots';
import { registerTheme, unregisterTheme, listThemes } from './contrib/registry';

describe('round10: conversation node registration', () => {
  it('same owner+id re-registration overwrites (documented contract)', () => {
    const ctx = { toolName: 'bash', input: 'x', output: 'old', ok: true };
    registerConversationNode({
      owner: 'plugin:t',
      id: 'n1',
      toolName: 'bash',
      render: (c) => (c.output === 'old' ? <b>OLD</b> : null),
    });
    registerConversationNode({
      owner: 'plugin:t',
      id: 'n1',
      toolName: 'bash',
      render: (c) => (c.output === 'new' ? <i>NEW</i> : null),
    });
    // The stale entry must be gone — only the updated render answers.
    expect(renderToStaticMarkup(renderToolNode({ ...ctx, output: 'new' } as never) as any)).toContain('NEW');
    expect(renderToolNode({ ...ctx, output: 'old' } as never)).toBeNull();
    removeConversationNodes('plugin:t');
  });

  it('owner-prefix removal clears plugin-owned nodes', () => {
    registerConversationNode({
      owner: 'plugin:demo',
      id: 'x',
      toolName: 'bash',
      render: () => <u>P</u>,
    });
    registerConversationNode({
      owner: 'builtin:core',
      id: 'x',
      toolName: 'bash',
      render: () => <u>B</u>,
    });
    removeConversationNodesByOwnerPrefix('plugin:');
    expect(renderToolNode({ toolName: 'bash', output: 'o' } as never)).not.toContain('P');
  });
});

describe('round10: template interpolation is literal', () => {
  it('$& and $` in tool output render verbatim', () => {
    const html = renderToStaticMarkup(
      renderTemplateNode(
        { id: 'n', toolName: 'bash', template: 'out={{output}}' },
        { toolName: 'bash', output: 'cost: $& tail `$&`' } as never,
      ) as any,
    );
    expect(html).toContain('cost: $&amp;');
    expect(html).not.toContain('{{output}}');
  });
});

describe('round10: slot contribution overwrite', () => {
  it('same owner+id replaces instead of duplicating', () => {
    contribute('conversation.tool-detail', {
      owner: 'p',
      id: 'card',
      render: () => ({ node: <b>A</b> }),
    }).dispose();
    contribute('conversation.tool-detail', {
      owner: 'p',
      id: 'card',
      render: () => ({ node: <b>A</b> }),
    });
    contribute('conversation.tool-detail', {
      owner: 'p',
      id: 'card',
      render: () => ({ node: <i>B</i> }),
    });
    const list = contributors('conversation.tool-detail').filter((c) => c.owner === 'p' && c.id === 'card');
    expect(list).toHaveLength(1);
  });
});

describe('round10: theme unregistration', () => {
  it('unregisterTheme removes a registered theme', () => {
    registerTheme({ name: 'plugin:test-r10', css: ':root{}' });
    expect(listThemes().some((t) => t.name === 'plugin:test-r10')).toBe(true);
    unregisterTheme('plugin:test-r10');
    expect(listThemes().some((t) => t.name === 'plugin:test-r10')).toBe(false);
  });
});
