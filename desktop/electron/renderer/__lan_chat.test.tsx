/// <reference types="vitest/globals" />
// LAN chat mini-app: discovery list (hostname naming), history + markRead,
// text send, image render, unread badge. All protocol traffic goes through
// window.starriver.invoke → mocked here with an in-memory lan.* backend.
import { describe, it, expect, beforeAll, afterAll, beforeEach, vi } from 'vitest';
import React from 'react';
import { render, screen, waitFor, fireEvent } from '@testing-library/react';
import { LanChatDialog, useLanChatUnread } from './app';
import { I18nProvider } from './i18n';

const originalStarriver = (window as any).starriver;

// ── In-memory lan.* backend ──

interface FakeMessage {
  id: number;
  dir: 'in' | 'out';
  kind: 'text' | 'image';
  text?: string;
  image?: { ref: string; mime: string; width: number; height: number; bytes: number };
  tsMs: number;
  status?: 'sending' | 'sent' | 'failed';
}

const TINY_PNG_BASE64 =
  'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==';

let peers: any[];
let conversations: Record<string, FakeMessage[]>;
let unread: Record<string, number>;
let markReadCalls: string[];
let sends: any[];
let attachments: Record<string, { base64: string; mime: string }>;
let enabled: boolean;
let nextId: number;

function resetBackend() {
  enabled = true;
  nextId = 100;
  peers = [
    { id: 'peer-dev', hostname: 'dev-linux', ip: '192.168.1.23', port: 51001, online: true, lastSeenMs: 500 },
    { id: 'peer-studio', hostname: 'studio-win', ip: '192.168.1.40', port: 51002, online: false, lastSeenMs: 40000 },
  ];
  conversations = {
    'peer-dev': [
      { id: 1, dir: 'in', kind: 'text', text: '收到，构建完成了', tsMs: 1_700_000_010_000 },
      { id: 2, dir: 'in', kind: 'image', image: { ref: 'sha256:abc', mime: 'image/png', width: 1, height: 1, bytes: 70 }, tsMs: 1_700_000_020_000 },
    ],
  };
  unread = { 'peer-dev': 2 };
  markReadCalls = [];
  sends = [];
  attachments = { 'sha256:abc': { base64: TINY_PNG_BASE64, mime: 'image/png' } };
}

function makeInvoke() {
  return vi.fn(async (requestJson: string) => {
    const request = JSON.parse(requestJson);
    const respond = (result: unknown) => JSON.stringify({ v: 1, id: request.id, ok: true, result });
    switch (request.method) {
      case 'lan.state': {
        const unreadTotal = Object.values(unread).reduce((sum: number, n) => sum + (n as number), 0);
        return respond({ enabled, self: { id: 'self-1', hostname: 'xiaomo-macpro', port: 51000, ip: '192.168.1.9' }, peers, conversations: Object.keys(conversations), unread, unreadTotal });
      }
      case 'lan.messages': {
        const sinceId = request.params.sinceId ?? 0;
        const messages = (conversations[request.params.peerId] ?? []).filter((m) => m.id > sinceId);
        const peer = peers.find((p) => p.id === request.params.peerId) ?? null;
        return respond({ messages, peer });
      }
      case 'lan.attachment':
        return respond(attachments[request.params.id] ?? null);
      case 'lan.markRead':
        markReadCalls.push(request.params.peerId);
        delete unread[request.params.peerId];
        return respond({ ok: true });
      case 'lan.send': {
        sends.push(request.params);
        const list = conversations[request.params.peerId] ?? (conversations[request.params.peerId] = []);
        const message: FakeMessage = { id: ++nextId, dir: 'out', kind: request.params.kind, tsMs: Date.now(), status: 'sent' };
        if (request.params.kind === 'text') message.text = request.params.text;
        if (request.params.kind === 'image') message.image = { ref: 'sha256:out', mime: request.params.mime, width: 1, height: 1, bytes: 70 };
        list.push(message);
        attachments['sha256:out'] = { base64: TINY_PNG_BASE64, mime: request.params.mime ?? 'image/png' };
        return respond({ ok: true, id: message.id });
      }
      default:
        return JSON.stringify({ v: 1, id: request.id, ok: false, error: { code: 'METHOD_NOT_FOUND', message: request.method } });
    }
  });
}

let invoke: ReturnType<typeof makeInvoke>;

beforeAll(() => {
  (window as any).starriver = {};
});
afterAll(() => {
  (window as any).starriver = originalStarriver;
});

beforeEach(() => {
  localStorage.setItem('starriver-language', 'en');
  resetBackend();
  invoke = makeInvoke();
  (window as any).starriver.invoke = invoke;
});

async function renderDialog() {
  const rendered = render(
    <I18nProvider>
      <LanChatDialog onClose={() => {}} />
    </I18nProvider>,
  );
  await waitFor(() => expect(screen.getByTestId('lan-chat-peer-title').textContent).toBe('dev-linux'));
  return rendered;
}

describe('LanChatDialog — 实例发现列表', () => {
  it('按 hostname 渲染实例，在线/离线与 ip:port 次行', async () => {
    await renderDialog();
    // hostname 同时出现在实例行与会话头 → getAllByText。
    expect(screen.getAllByText('dev-linux').length).toBeGreaterThan(0);
    expect(screen.getAllByText('studio-win').length).toBeGreaterThan(0);
    expect(screen.getAllByText('192.168.1.23:51001').length).toBeGreaterThan(0);
    // 本机行
    expect(screen.getByText('This Mac')).toBeTruthy();
    expect(screen.getByText('xiaomo-macpro')).toBeTruthy();
  });

  it('无实例时显示空态文案', async () => {
    peers = [];
    conversations = {};
    render(<I18nProvider><LanChatDialog onClose={() => {}} /></I18nProvider>);
    await waitFor(() => expect(screen.getByTestId('lan-chat-empty')).toBeTruthy());
    expect(screen.getByTestId('lan-chat-empty').textContent).toContain('No other Starriver instance found');
  });

  it('enabled:false 显示未启用提示', async () => {
    enabled = false;
    render(<I18nProvider><LanChatDialog onClose={() => {}} /></I18nProvider>);
    await waitFor(() => expect(screen.getByText(/Messaging disabled/)).toBeTruthy());
  });
});

describe('LanChatDialog — 会话与发送', () => {
  it('选中实例加载历史 + markRead + 图片解析为 data URL', async () => {
    await renderDialog();
    await waitFor(() => expect(screen.getByText('收到，构建完成了')).toBeTruthy());
    expect(markReadCalls).toContain('peer-dev');
    await waitFor(
      () => {
        const image = document.querySelector('img.lan-chat-msg-image') as HTMLImageElement | null;
        expect(image).toBeTruthy();
        expect(image!.src.startsWith('data:image/png;base64,')).toBe(true);
      },
      { timeout: 3000 },
    );
    expect(invoke.mock.calls.some(([payload]) => String(payload).includes('"method":"lan.attachment"'))).toBe(true);
  });

  it('Enter 发送文字：lan.send 参数正确并立即回显', async () => {
    await renderDialog();
    const input = screen.getByTestId('lan-chat-input') as HTMLInputElement;
    fireEvent.change(input, { target: { value: 'hello from test' } });
    fireEvent.keyDown(input, { key: 'Enter' });
    await waitFor(() => expect(sends.length).toBe(1));
    expect(sends[0]).toMatchObject({ peerId: 'peer-dev', kind: 'text', text: 'hello from test' });
    await waitFor(() => expect(screen.getByText('hello from test')).toBeTruthy());
  });

  it('空文本不发送', async () => {
    await renderDialog();
    const input = screen.getByTestId('lan-chat-input');
    fireEvent.keyDown(input, { key: 'Enter' });
    expect(sends.length).toBe(0);
  });
});

describe('useLanChatUnread — 侧栏徽标数据', () => {
  it('从 lan.state 轮询未读总数', async () => {
    let probe: number | null = null;
    function Probe() {
      probe = useLanChatUnread();
      return null;
    }
    render(<I18nProvider><Probe /></I18nProvider>);
    await waitFor(() => expect(probe).toBe(2));
  });

  it('enabled:false 时未读为 0', async () => {
    enabled = false;
    let probe: number | null = null;
    function Probe() {
      probe = useLanChatUnread();
      return null;
    }
    render(<I18nProvider><Probe /></I18nProvider>);
    await waitFor(() => expect(probe).toBe(0));
  });
});
