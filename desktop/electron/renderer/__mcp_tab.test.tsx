/// <reference types="vitest/globals" />
import { describe, it, expect, beforeAll, afterAll, vi } from 'vitest';
import React from 'react';
import { render, screen, waitFor, fireEvent, within } from '@testing-library/react';
import { McpTab, isPresetCreated } from './app';
import { I18nProvider } from './i18n';

const originalStarriver = (window as any).starriver;
beforeAll(() => {
  (window as any).starriver = {
    listMcpServers: () => Promise.resolve([]),
    addMcpServer: vi.fn().mockResolvedValue(undefined),
    removeMcpServer: vi.fn().mockResolvedValue(undefined),
    refreshMcpTools: vi.fn().mockResolvedValue(undefined),
    oauthConnect: vi.fn().mockResolvedValue('https://slack.com/oauth/authorize'),
    oauthDisconnect: vi.fn().mockResolvedValue(undefined),
    testMcpServer: vi.fn().mockResolvedValue({ ok: true, toolCount: 2, tools: ['search', 'read_file'], error: null }),
    testMcpTool: vi.fn().mockResolvedValue({ ok: true, output: '{"result":"ok"}' }),
  };
});
afterAll(() => { (window as any).starriver = originalStarriver; });

function starriver(): any { return (window as any).starriver; }

/** Render McpTab and switch to the Servers tab (created instances live there). */
async function openServersTab() {
  render(<I18nProvider><McpTab /></I18nProvider>);
  await waitFor(() => expect(screen.getByText('Filesystem')).toBeTruthy());
  fireEvent.click(screen.getByText('Servers'));
}

function server(name: string, over: Partial<any> = {}) {
  return { name, transport: 'stdio', command: 'npx', args: [], env: [], url: null, oauthConnected: false, toolCount: 0, status: 'connected', ...over };
}

describe('McpTab — popular servers gallery', () => {
  it('renders overseas + domestic presets', async () => {
    render(<I18nProvider><McpTab /></I18nProvider>);
    await waitFor(() => expect(screen.getByText('Filesystem')).toBeTruthy());
    expect(screen.getByText('GitHub')).toBeTruthy();
    expect(screen.getByText('Playwright')).toBeTruthy();
    expect(screen.getByText('Amap Maps')).toBeTruthy();   // domestic
  });

  it('shows overseas and domestic presets in one view with a divider', async () => {
    render(<I18nProvider><McpTab /></I18nProvider>);
    await waitFor(() => expect(screen.getByText('Filesystem')).toBeTruthy());
    // Both region presets render on the same page (no filter tabs, no region labels).
    expect(screen.getByText('Filesystem')).toBeTruthy();   // overseas preset
    expect(screen.getByText('Amap Maps')).toBeTruthy();    // domestic preset
    expect(screen.getByText('Baidu Maps')).toBeTruthy();   // domestic preset
  });

  it('clicking a preset opens the form prefilled', async () => {
    render(<I18nProvider><McpTab /></I18nProvider>);
    await waitFor(() => expect(screen.getByText('Filesystem')).toBeTruthy());
    fireEvent.click(screen.getByText('Filesystem'));
    // Name field prefilled with the preset name.
    const nameInput = Array.from(document.querySelectorAll('input')).find(i => i.value === 'Filesystem') as HTMLInputElement;
    expect(nameInput).toBeTruthy();
    // Args textarea carries the package + allowed-dir placeholder.
    const argsTa = document.querySelector('textarea') as HTMLTextAreaElement;
    expect(argsTa.value).toContain('@modelcontextprotocol/server-filesystem');
    expect(argsTa.value).toContain('/path/to/allowed/dir');
  });

  it('a key-requiring preset prefills the env key for the user to fill', async () => {
    render(<I18nProvider><McpTab /></I18nProvider>);
    await waitFor(() => expect(screen.getByText('GitHub')).toBeTruthy());
    fireEvent.click(screen.getByText('GitHub'));
    // Two textareas: args then env. Env has the prefilled key with empty value.
    const tas = Array.from(document.querySelectorAll('textarea'));
    const envTa = tas[tas.length - 1] as HTMLTextAreaElement;
    expect(envTa.value).toContain('GITHUB_PERSONAL_ACCESS_TOKEN=');
  });

  it('renders the newly added overseas + domestic presets', async () => {
    render(<I18nProvider><McpTab /></I18nProvider>);
    await waitFor(() => expect(screen.getByText('Filesystem')).toBeTruthy());
    expect(screen.getByText('GitLab')).toBeTruthy();
    expect(screen.getByText('Notion')).toBeTruthy();
    expect(screen.getByText('Baidu Maps')).toBeTruthy();
    expect(screen.getByText('Notify')).toBeTruthy();
  });

  it('domestic section lists the China presets', async () => {
    render(<I18nProvider><McpTab /></I18nProvider>);
    await waitFor(() => expect(screen.getByText('Filesystem')).toBeTruthy());
    expect(screen.getByText('Amap Maps')).toBeTruthy();
    expect(screen.getByText('Baidu Maps')).toBeTruthy();
    expect(screen.getByText('Notify')).toBeTruthy();
  });

  it('a no-key preset (Filesystem) prefills with an empty env for one-click add', async () => {
    render(<I18nProvider><McpTab /></I18nProvider>);
    await waitFor(() => expect(screen.getByText('Filesystem')).toBeTruthy());
    fireEvent.click(screen.getByText('Filesystem'));
    const tas = Array.from(document.querySelectorAll('textarea'));
    const envTa = tas[tas.length - 1] as HTMLTextAreaElement;
    expect(envTa.value).toBe('');   // no env keys → nothing to fill
  });

  it('renders the Slack remote (http) preset', async () => {
    render(<I18nProvider><McpTab /></I18nProvider>);
    await waitFor(() => expect(screen.getByText('Slack')).toBeTruthy());
  });

  it('a remote preset prefills the endpoint URL into the form', async () => {
    render(<I18nProvider><McpTab /></I18nProvider>);
    await waitFor(() => expect(screen.getByText('Slack')).toBeTruthy());
    fireEvent.click(screen.getByText('Slack'));
    // The endpoint input (placeholder) carries the Slack MCP URL.
    await waitFor(() => {
      const urlInput = screen.getByPlaceholderText('https://mcp.slack.com/mcp') as HTMLInputElement;
      expect(urlInput.value).toBe('https://mcp.slack.com/mcp');
    });
  });

  it('the Slack preset prefills the editable OAuth endpoint fields', async () => {
    render(<I18nProvider><McpTab /></I18nProvider>);
    await waitFor(() => expect(screen.getByText('Slack')).toBeTruthy());
    fireEvent.click(screen.getByText('Slack'));
    // OAuth fields are shown and carry the preset's endpoints/scopes.
    await waitFor(() => {
      expect(screen.getByText('Authorization URL')).toBeTruthy();
      expect(screen.getByText('Token URL')).toBeTruthy();
      expect(screen.getByText('Scopes (comma-separated)')).toBeTruthy();
    });
    expect(screen.getByDisplayValue('https://slack.com/api/oauth.v2.user.access')).toBeTruthy();
    const scopes = screen.getByDisplayValue('chat:write, search:read.public, channels:history, users:read') as HTMLInputElement;
    expect(scopes).toBeTruthy();
  });

  it('saving an http preset passes transport=http + url + oauth json', async () => {
    starriver().addMcpServer.mockClear();
    render(<I18nProvider><McpTab /></I18nProvider>);
    await waitFor(() => expect(screen.getByText('Slack')).toBeTruthy());
    fireEvent.click(screen.getByText('Slack'));
    await waitFor(() => expect(screen.getByPlaceholderText('https://mcp.slack.com/mcp')).toBeTruthy());
    fireEvent.click(screen.getByText('Save'));
    await waitFor(() => {
      const [, transport, , , , url, oauthJson] = starriver().addMcpServer.mock.calls.at(-1);
      expect(transport).toBe('http');
      expect(url).toBe('https://mcp.slack.com/mcp');
      expect(JSON.parse(oauthJson).token_url).toBe('https://slack.com/api/oauth.v2.user.access');
      expect(JSON.parse(oauthJson).scopes).toContain('chat:write');
    });
  });

  it('renders the Gmail official preset and prefills its endpoint', async () => {
    render(<I18nProvider><McpTab /></I18nProvider>);
    await waitFor(() => expect(screen.getByText('Gmail')).toBeTruthy());
    fireEvent.click(screen.getByText('Gmail'));
    await waitFor(() => {
      const urlInput = screen.getByPlaceholderText('https://mcp.slack.com/mcp') as HTMLInputElement;
      expect(urlInput.value).toBe('https://gmailmcp.googleapis.com/mcp/v1');
    });
  });

  it('renders the NetEase mail preset and prefills EMAIL_TYPE=163', async () => {
    render(<I18nProvider><McpTab /></I18nProvider>);
    await waitFor(() => expect(screen.getByText('163 Mail')).toBeTruthy());
    fireEvent.click(screen.getByText('163 Mail'));
    await waitFor(() => {
      const envTa = Array.from(document.querySelectorAll('textarea')).at(-1) as HTMLTextAreaElement;
      expect(envTa.value).toContain('EMAIL_TYPE=163');
      expect(envTa.value).toContain('EMAIL_USER=');
    });
  });

  it('renders the Alibaba DevOps preset and prefills the token env', async () => {
    render(<I18nProvider><McpTab /></I18nProvider>);
    await waitFor(() => expect(screen.getByText('Alibaba DevOps')).toBeTruthy());
    fireEvent.click(screen.getByText('Alibaba DevOps'));
    await waitFor(() => {
      const envTa = Array.from(document.querySelectorAll('textarea')).at(-1) as HTMLTextAreaElement;
      expect(envTa.value).toContain('YUNXIAO_ACCESS_TOKEN=');
    });
  });

  it('every preset card exposes a + to create an instance from the template', async () => {
    render(<I18nProvider><McpTab /></I18nProvider>);
    await waitFor(() => expect(screen.getByLabelText('create-filesystem')).toBeTruthy());
    expect(screen.getByLabelText('create-github')).toBeTruthy();
    expect(screen.getByLabelText('create-slack')).toBeTruthy();
    expect(screen.getByLabelText('create-amap')).toBeTruthy();
  });
});

describe('McpTab — create from template with +', () => {
  it('clicking + opens the prefilled form; saving the instance hides the +', async () => {
    const servers: any[] = [];
    starriver().listMcpServers = () => Promise.resolve([...servers]);
    starriver().addMcpServer = vi.fn(async () => {
      servers.push({ name: 'GitHub', transport: 'stdio', command: 'npx', args: ['-y', '@github/mcp-server'], env: [], url: null, oauthConnected: false, toolCount: 0, status: 'connected' });
    });
    render(<I18nProvider><McpTab /></I18nProvider>);
    await waitFor(() => expect(screen.getByLabelText('create-github')).toBeTruthy());
    fireEvent.click(screen.getByLabelText('create-github'));
    // The form opens prefilled from the GitHub preset.
    await waitFor(() => expect(screen.getByPlaceholderText('github')).toBeTruthy());
    fireEvent.click(screen.getByText('Save'));
    await waitFor(() => expect(starriver().addMcpServer).toHaveBeenCalled());
    // After save + refresh the + is replaced by a ✓ created marker.
    await waitFor(() => expect(screen.queryByLabelText('create-github')).toBeNull());
    expect(screen.getByTitle('✓ created')).toBeTruthy();
  });

  it('an existing instance hides the +; deleting it restores the +', async () => {
    const servers: any[] = [server('GitHub')];
    starriver().listMcpServers = () => Promise.resolve([...servers]);
    starriver().removeMcpServer = vi.fn(async () => { servers.length = 0; });
    const origConfirm = window.confirm;
    window.confirm = (() => true) as any;
    try {
      render(<I18nProvider><McpTab /></I18nProvider>);
      // Created already → no + for GitHub.
      await waitFor(() => expect(screen.queryByLabelText('create-github')).toBeNull());
      expect(screen.getByTitle('✓ created')).toBeTruthy();
      // Servers tab → delete the instance.
      fireEvent.click(screen.getByText('Servers'));
      fireEvent.click(await screen.findByText('Remove'));
      await waitFor(() => expect(screen.queryByText('GitHub')).toBeNull());
      // Back to Templates → the + is restored.
      fireEvent.click(screen.getByText('Templates'));
      await waitFor(() => expect(screen.getByLabelText('create-github')).toBeTruthy());
    } finally {
      window.confirm = origConfirm;
    }
  });
});

describe('McpTab — servers tab & test window', () => {
  it('shows a Connect button on an http server with OAuth config and invokes oauthConnect', async () => {
    starriver().listMcpServers = () => Promise.resolve([
      server('Slack', {
        transport: 'http', command: '', url: 'https://mcp.slack.com/mcp',
        oauth: { authorization_url: 'https://slack.com/oauth', token_url: 'https://slack.com/token', client_id: 'id', client_secret: 'sec', scopes: [], redirect_uri: 'http://localhost:3118/callback' },
      }),
    ]);
    starriver().oauthConnect.mockClear();
    await openServersTab();
    const connectBtn = await screen.findByText('Connect');
    fireEvent.click(connectBtn);
    await waitFor(() => {
      expect(starriver().oauthConnect).toHaveBeenCalledWith('Slack');
    });
  });

  it('an http server without OAuth config shows no Connect button (plain HTTP connects automatically)', async () => {
    starriver().listMcpServers = () => Promise.resolve([
      server('nexplan', { transport: 'http', command: '', url: 'http://192.168.3.194:3345/mcp' }),
    ]);
    await openServersTab();
    await screen.findByText('nexplan');
    expect(screen.queryByText('Connect')).toBeNull();
  });

  it('Test button opens the test window; Test Connection invokes testMcpServer and shows the result', async () => {
    starriver().listMcpServers = () => Promise.resolve([server('GitHub')]);
    starriver().testMcpServer.mockResolvedValue({ ok: true, toolCount: 3, tools: ['search', 'read_file', 'list'], error: null });
    await openServersTab();
    fireEvent.click(await screen.findByText('Test'));
    const win = await screen.findByTestId('mcp-test-window');
    fireEvent.click(within(win).getByText('Test'));
    await waitFor(() => {
      expect(starriver().testMcpServer).toHaveBeenCalledWith('GitHub');
      expect(within(win).getByText('✓ 3 tool(s)')).toBeTruthy();
    });
  });

  it('test window surfaces a connection failure', async () => {
    starriver().listMcpServers = () => Promise.resolve([server('Broken', { command: '/nonexistent' })]);
    starriver().testMcpServer.mockResolvedValue({ ok: false, toolCount: 0, tools: [], error: 'process not launched' });
    await openServersTab();
    fireEvent.click(await screen.findByText('Test'));
    const win = await screen.findByTestId('mcp-test-window');
    fireEvent.click(within(win).getByText('Test'));
    await waitFor(() => {
      expect(within(win).getByText('✗ process not launched')).toBeTruthy();
    });
  });

  it('a successful test lists tools with a Run button that invokes testMcpTool', async () => {
    starriver().listMcpServers = () => Promise.resolve([server('GitHub')]);
    starriver().testMcpServer.mockResolvedValue({ ok: true, toolCount: 2, tools: ['search', 'read_file'], error: null });
    starriver().testMcpTool.mockResolvedValue({ ok: true, output: '{"result":"ok"}' });
    await openServersTab();
    fireEvent.click(await screen.findByText('Test'));
    const win = await screen.findByTestId('mcp-test-window');
    fireEvent.click(within(win).getByText('Test'));
    await waitFor(() => expect(within(win).getByText('search')).toBeTruthy());
    fireEvent.click(within(win).getAllByText('Run')[0]);
    await waitFor(() => {
      expect(starriver().testMcpTool).toHaveBeenCalledWith('GitHub', 'search');
      expect(within(win).getByText('{"result":"ok"}')).toBeTruthy();
    });
  });
});

describe('isPresetCreated', () => {
  const preset = {
    id: 'amap', icon: '', region: 'overseas' as const, transport: 'stdio',
    command: 'npx', args: [], env: {}, name: { en: 'Amap Maps', zh: '高德地图' }, desc: { en: '', zh: '' },
  };
  it('matches an instance by its localized name (en or zh) or id', () => {
    expect(isPresetCreated(preset, [server('Amap Maps')])).toBe(true);
    expect(isPresetCreated(preset, [server('高德地图')])).toBe(true);
    expect(isPresetCreated(preset, [server('amap')])).toBe(true);     // id match
    expect(isPresetCreated(preset, [server('Slack')])).toBe(false);
  });
  it('is false when there are no instances', () => {
    expect(isPresetCreated(preset, [])).toBe(false);
  });
});
