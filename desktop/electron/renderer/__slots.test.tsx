/// <reference types="vitest/globals" />
import { describe, it, expect } from 'vitest';
import React from 'react';
import { render, screen } from '@testing-library/react';
import {
  contribute, renderSlot, removeContributions, SlotKeys,
} from './contrib/slots';
import {
  registerConversationNode, renderToolNode, removeConversationNodes,
} from './contrib/conversationNodes';

describe('slots — 职责链注册表', () => {
  it('按 priority 顺序尝试，第一个非 null 生效', () => {
    const d1 = contribute(SlotKeys.conversationToolDetail, {
      owner: 'a', id: 'low', priority: 10,
      render: () => null,
    });
    const d2 = contribute(SlotKeys.conversationToolDetail, {
      owner: 'b', id: 'high', priority: 5,
      render: () => ({ node: React.createElement('div', {}, 'from-high') }),
    });
    const result = renderSlot(SlotKeys.conversationToolDetail, {
      toolName: 'bash', input: 'ls',
    });
    expect(result?.handled).toBe(true);
    d2.dispose();
    d1.dispose();
    expect(renderSlot(SlotKeys.conversationToolDetail, { toolName: 'bash' })).toBeNull();
  });

  it('removeContributions 按 owner 批量清理', () => {
    contribute(SlotKeys.conversationToolDetail, { owner: 'plug', id: 'x', render: () => ({ node: 'x' }) });
    removeContributions(SlotKeys.conversationToolDetail, 'plug');
    expect(renderSlot(SlotKeys.conversationToolDetail, { toolName: 'bash' })).toBeNull();
  });
});

describe('conversationNodes — 注册制渲染', () => {
  it('toolName 精确命中优先于通配', () => {
    const d1 = registerConversationNode({
      owner: 'test', id: 'wild', render: () => React.createElement('div', {}, 'wildcard'),
    });
    const d2 = registerConversationNode({
      owner: 'test', id: 'run_code-node', toolName: 'run_code',
      render: () => React.createElement('div', {}, 'ptc-node'),
    });
    const node = renderToolNode({ toolName: 'run_code', input: '{}', output: '{}' });
    expect(node).not.toBeNull();
    render(React.createElement('div', {}, node));
    expect(screen.getByText('ptc-node')).toBeTruthy();
    expect(screen.queryByText('wildcard')).toBeNull();
    d1.dispose(); d2.dispose();
  });

  it('removeConversationNodes 按 owner 清理', () => {
    const d = registerConversationNode({
      owner: 'test2', id: 'x', render: () => React.createElement('div', {}, 'x'),
    });
    d.dispose();
    expect(renderToolNode({ toolName: 'anything' })).toBeNull();
    void removeConversationNodes;
  });
});
