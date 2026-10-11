// Architecture-guard tests (BACKLOG supplement 6 · frontend plugin axis):
// they lock the three invariants of the conversation-node registry —
//   1. consumers must not hardcode tool branches (render extension goes only through the registry);
//   2. built-in contributions (run_code / todo_write) must be registered;
//   3. declarative template interpolation must HTML-escape.
// These three are the executable form of §5 in
// docs/superpowers/specs/2026-09-08-frontend-plugin-slots-design.md; red = the architecture was bypassed — fix that first.

import { readFileSync, readdirSync } from 'node:fs';
import { join } from 'node:path';
import { describe, it, expect } from 'vitest';

const rendererDir = join(__dirname);

function listRendererSources(dir: string, out: string[] = []): string[] {
  for (const name of readdirSync(dir)) {
    const full = join(dir, name);
    if (name === 'node_modules' || name === 'contrib') continue;
    if (name.endsWith('.test.ts') || name.endsWith('.test.tsx')) continue;
    if (name.endsWith('.tsx') || name.endsWith('.ts')) out.push(full);
    else if (!name.includes('.')) out.push(...listRendererSources(full, out));
  }
  return out;
}

describe('slots 架构守护 — 注册表是工具渲染的唯一路径', () => {
  it('消费层源码不得出现 toolName 的硬编码相等分支', () => {
    const sources = listRendererSources(rendererDir, [])
      .filter((f) => !f.endsWith('slots_arch.test.ts'));
    const violations: string[] = [];
    for (const file of sources) {
      const text = readFileSync(file, 'utf8');
      const matches = text.match(/toolName\s*(?:===|!==|==)\s*['"][^'"]+['"]/g);
      if (matches) {
        violations.push(`${file}: ${matches.join(', ')}`);
      }
    }
    expect(
      violations,
      `工具分支必须经 renderToolNode 注册制（violations: ${violations.join(' | ')}）`,
    ).toEqual([]);
  });

  it('内置贡献在册: run_code / todo_write 可渲染', async () => {
    await import('./app');
    const { renderToolNode } = await import('./contrib/conversationNodes');
    expect(renderToolNode({
      toolName: 'run_code', input: '{}', output: '{"output":"ok"}',
    })).not.toBeNull();
    expect(renderToolNode({
      toolName: 'todo_write', input: '{}', output: '## TODO\n- [x] done',
    })).not.toBeNull();
  });
});

describe('声明式模板 — 插值转义', () => {
  it('{{output}} 插值含 HTML 时按实体转义', async () => {
    const { renderTemplateNode } = await import('./contrib/conversationNodes');
    const node = renderTemplateNode(
      { id: 'evil', toolName: 'bash', template: '<b>{{output}}</b>' },
      { toolName: 'bash', input: '{}', output: '<script>alert(1)</script>' },
    ) as { props: { dangerouslySetInnerHTML: { __html: string } } };
    expect(node.props.dangerouslySetInnerHTML.__html).not.toContain('<script>');
    expect(node.props.dangerouslySetInnerHTML.__html).toContain('&lt;script&gt;');
  });
});
