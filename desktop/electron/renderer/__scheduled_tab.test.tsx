/// <reference types="vitest/globals" />
import { describe, it, expect, beforeAll, afterAll, beforeEach, vi } from 'vitest';
import React from 'react';
import { render, screen, waitFor, fireEvent } from '@testing-library/react';
import { ScheduledTasksTab, parseSkillTokens, WorkspaceProvider } from './app';
import { I18nProvider } from './i18n';

const MODEL_ENTRIES = [
  {
    id: 'deepseek', name: 'DeepSeek', modelType: 'cloud', providerType: 'cloud',
    baseUrl: 'https://api.deepseek.com', apiKey: '!keychain:model/deepseekv4',
    models: [{ id: 'deepseek-chat', name: 'DeepSeek Chat' }, { id: 'deepseek-reasoner', name: 'DeepSeek Reasoner' }],
    defaultModel: 'deepseek-chat',
  },
  {
    id: 'ollama', name: 'Ollama Local', modelType: 'local', providerType: 'ollama',
    baseUrl: 'http://localhost:11434', apiKey: '',
    models: [], defaultModel: 'qwen3:4b',
  },
];

const originalStarriver = (window as any).starriver;
beforeAll(() => {
  (window as any).starriver = {
    listScheduledTasks: () => Promise.resolve([]),
    listModels: () => Promise.resolve(MODEL_ENTRIES),
    listSkills: () => Promise.resolve([]),
    listAgents: () => Promise.resolve([
      { name: 'Chat Agent', description: '', version: '0.1.0' },
      { name: 'Starriver Code', description: '', version: '0.1.0' },
    ]),
    addScheduledTask: vi.fn().mockResolvedValue(undefined),
    listWorkspaces: vi.fn().mockResolvedValue([]),
    updateScheduledTask: vi.fn().mockResolvedValue(undefined),
    removeScheduledTask: vi.fn().mockResolvedValue(undefined),
    runScheduledTaskNow: vi.fn().mockResolvedValue('queued'),
    getTaskRunHistory: () => Promise.resolve([]),
    listJobs: () => Promise.resolve([]),
    listSessions: () => Promise.resolve([]),
    listArchivedSessions: () => Promise.resolve([]),
    listRunningSessions: () => Promise.resolve([]),
    listAllTaskRuns: () => Promise.resolve([]),
    getSessionTrace: () => Promise.resolve({ entries: [] }),
  };
});
afterAll(() => {
  (window as any).starriver = originalStarriver;
});

function starriver(): any { return (window as any).starriver; }

// Skills available to attach. "review" is a command-skill; "summarize-skill" is name-only.
const SKILLS = [
  { name: 'code-review', description: 'Review code', has_command: true, command: 'review' },
  { name: 'summarize-skill', description: 'Summarize text', has_command: false },
  { name: 'deploy', description: 'Deploy app', has_command: true, command: 'deploy' },
];

describe('parseSkillTokens', () => {
  it('resolves command + name tokens, ignores unknown, dedupes', () => {
    const out = parseSkillTokens('/review then /summarize-skill and /review again /nosuch', SKILLS as any);
    expect(out).toEqual(['code-review', 'summarize-skill']);
  });
  it('matches a bare name token when no command', () => {
    expect(parseSkillTokens('/summarize-skill', SKILLS as any)).toEqual(['summarize-skill']);
  });
  it('returns empty for a prompt with no tokens', () => {
    expect(parseSkillTokens('just a normal prompt', SKILLS as any)).toEqual([]);
  });
});

describe('ScheduledTasksTab — skill picker', () => {
  beforeEach(() => {
    (window as any).starriver = {
      ...(window as any).starriver,
      listSkills: () => Promise.resolve(SKILLS),
    };
  });

  it('lists installed skills when the Skill button is clicked', async () => {
    render(<I18nProvider><WorkspaceProvider><ScheduledTasksTab /></WorkspaceProvider></I18nProvider>);
    fireEvent.click(screen.getByText('New Task'));
    await waitFor(() => {
      expect(Array.from(document.querySelectorAll('textarea')).length).toBeGreaterThan(0);
    });
    fireEvent.click(screen.getByTitle('Skill'));
    // Popup shows the skills (rendered as /review, /summarize-skill, /deploy).
    await waitFor(() => {
      expect(screen.getByText('/review')).toBeTruthy();
      expect(screen.getByText('/summarize-skill')).toBeTruthy();
      expect(screen.getByText('/deploy')).toBeTruthy();
    });
  });

  it('inserts a token via the button picker and passes resolved skill name on save', async () => {
    starriver().addScheduledTask.mockClear();
    render(<I18nProvider><WorkspaceProvider><ScheduledTasksTab /></WorkspaceProvider></I18nProvider>);
    fireEvent.click(screen.getByText('New Task'));
    await waitFor(() => expect(screen.getByPlaceholderText('Enter a task name')).toBeTruthy());
    fireEvent.change(screen.getByPlaceholderText('Enter a task name'), { target: { value: 'T' } });
    // Open the button picker and pick /review (→ skill name "code-review").
    fireEvent.click(screen.getByTitle('Skill'));
    const item = await waitFor(() => screen.getByText('/review'));
    fireEvent.mouseDown(item);
    // Save.
    fireEvent.click(screen.getByText('Save'));
    await waitFor(() => {
      const skills = starriver().addScheduledTask.mock.calls.at(-1)[9];
      expect(skills).toEqual(['code-review']);
    });
  });

  it('slash autocomplete filters and inserts the token on Enter', async () => {
    render(<I18nProvider><WorkspaceProvider><ScheduledTasksTab /></WorkspaceProvider></I18nProvider>);
    fireEvent.click(screen.getByText('New Task'));
    const ta = (await waitFor(() => screen.getByPlaceholderText('Describe what the agent should do when triggered...'))) as HTMLTextAreaElement;
    // Type a slash fragment using the native value setter so the caret is at the end,
    // which is what triggers the slash autocomplete in onPromptChange.
    const setter = Object.getOwnPropertyDescriptor(window.HTMLTextAreaElement.prototype, 'value')!.set!;
    setter.call(ta, '/rev');
    ta.setSelectionRange(4, 4);
    ta.dispatchEvent(new Event('input', { bubbles: true }));
    // Popup opens filtered: only /review matches "rev".
    await waitFor(() => expect(screen.getByText('/review')).toBeTruthy());
    expect(screen.queryByText('/summarize-skill')).toBeNull();
    expect(screen.queryByText('/deploy')).toBeNull();
    // Enter selects the first (only) match and inserts it.
    fireEvent.keyDown(ta, { key: 'Enter' });
    await waitFor(() => expect(ta.value).toContain('/review'));
  });
});

describe('ScheduledTasksTab — templates popover (WI-174)', () => {
  it('opens the picker with all 15 templates from the header button; page starts without cards', async () => {
    render(<I18nProvider><WorkspaceProvider><ScheduledTasksTab /></WorkspaceProvider></I18nProvider>);
    // The page has no template grid until the picker opens (WI-174).
    await waitFor(() => expect(screen.getByText('No scheduled tasks configured.')).toBeTruthy());
    expect(screen.queryByText('Hourly Weather')).toBeNull();
    fireEvent.click(screen.getByText('New Task from Template'));
    const pop = await waitFor(() => screen.getByTestId('tpl-popover'));
    expect(pop.querySelectorAll('.tpl-card').length).toBe(15);
  });

  it('clicking a template closes the picker and opens the form prefilled', async () => {
    render(<I18nProvider><WorkspaceProvider><ScheduledTasksTab /></WorkspaceProvider></I18nProvider>);
    fireEvent.click(screen.getByText('New Task from Template'));
    const card = await waitFor(() => screen.getByText('Hourly Weather'));
    fireEvent.click(card);
    // Picker closed, form open prefilled with the template's prompt + name.
    await waitFor(() => expect(screen.queryByTestId('tpl-popover')).toBeNull());
    const ta = screen.getByPlaceholderText('Describe what the agent should do when triggered...') as HTMLTextAreaElement;
    await waitFor(() => expect(ta.value).toContain('weather forecast'));
    expect((screen.getByPlaceholderText('Enter a task name') as HTMLInputElement).value).toBe('Hourly Weather');
  });

  it('Esc closes the picker', async () => {
    render(<I18nProvider><WorkspaceProvider><ScheduledTasksTab /></WorkspaceProvider></I18nProvider>);
    fireEvent.click(screen.getByText('New Task from Template'));
    await waitFor(() => expect(screen.getByTestId('tpl-popover')).toBeTruthy());
    fireEvent.keyDown(document, { key: 'Escape' });
    await waitFor(() => expect(screen.queryByTestId('tpl-popover')).toBeNull());
  });
});

describe('ScheduledTasksTab — model selector', () => {
  it('lists configured models in the Add Task modal', async () => {
    render(<I18nProvider><WorkspaceProvider><ScheduledTasksTab /></WorkspaceProvider></I18nProvider>);
    // Open the Add Task modal.
    fireEvent.click(screen.getByText('New Task'));
    // Wait for the model options to load.
    await waitFor(() => {
      const select = screen.getByText('Default (active model)');
      expect(select).toBeTruthy();
    });
    const options = Array.from(document.querySelectorAll('select')).flatMap(
      s => Array.from(s.options).map(o => o.textContent)
    );
    expect(options).toContain('DeepSeek · DeepSeek Chat');
    expect(options).toContain('DeepSeek · DeepSeek Reasoner');
    expect(options).toContain('Ollama Local · qwen3:4b');
  });

  it('passes the chosen model to addScheduledTask on save', async () => {
    render(<I18nProvider><WorkspaceProvider><ScheduledTasksTab /></WorkspaceProvider></I18nProvider>);
    fireEvent.click(screen.getByText('New Task'));

    // Fill required fields: name + prompt.
    const inputs = Array.from(document.querySelectorAll('input')) as HTMLInputElement[];
    const nameInput = inputs.find(i => i.placeholder === 'Enter a task name');
    expect(nameInput).toBeTruthy();
    fireEvent.change(nameInput!, { target: { value: 'Review' } });
    const ta = document.querySelector('textarea')!;
    fireEvent.change(ta, { target: { value: 'review the codebase' } });

    // Choose a model from the model dropdown. Wait for the async-loaded option
    // to exist before firing the change (otherwise React keeps the empty value).
    await waitFor(() => {
      const hasOption = Array.from(document.querySelectorAll('select'))
        .some(s => Array.from(s.options).some(o => o.value === 'deepseek/deepseek-reasoner'));
      expect(hasOption).toBe(true);
    });
    const modelSelect = Array.from(document.querySelectorAll('select'))
      .find(s => Array.from(s.options).some(o => o.textContent === 'Default (active model)'))!;
    fireEvent.change(modelSelect, { target: { value: 'deepseek/deepseek-reasoner' } });

    fireEvent.click(screen.getByText('Save'));

    await waitFor(() => {
      expect(starriver().addScheduledTask).toHaveBeenCalledWith(
        'Review', expect.any(String), 'review the codebase', 'deepseek/deepseek-reasoner', null, null, true, null, null, [],
      );
    });
  });

  it('saves with model null when default is selected', async () => {
    render(<I18nProvider><WorkspaceProvider><ScheduledTasksTab /></WorkspaceProvider></I18nProvider>);
    fireEvent.click(screen.getByText('New Task'));
    const nameInput = Array.from(document.querySelectorAll('input')).find(i => i.placeholder === 'Enter a task name')!;
    fireEvent.change(nameInput, { target: { value: 'Default Model Task' } });
    fireEvent.change(document.querySelector('textarea')!, { target: { value: 'use default' } });
    fireEvent.click(screen.getByText('Save'));
    await waitFor(() => {
      expect(starriver().addScheduledTask).toHaveBeenCalledWith(
        'Default Model Task', expect.any(String), 'use default', null, null, null, true, null, null, []
      );
    });
  });

  it('Once mode shows separate date + time inputs and saves a combined datetime', async () => {
    starriver().addScheduledTask.mockClear();
    render(<I18nProvider><WorkspaceProvider><ScheduledTasksTab /></WorkspaceProvider></I18nProvider>);
    fireEvent.click(screen.getByText('New Task'));
    // Switch to Once mode.
    fireEvent.click(screen.getByText('Once'));
    // The Once date input renders before the Validity date inputs in the modal,
    // so the first date input is the Once one. The time input is unique.
    const dateInput = document.querySelectorAll('input[type="date"]')[0] as HTMLInputElement;
    const timeInput = document.querySelector('input[type="time"]') as HTMLInputElement;
    expect(dateInput).toBeTruthy();
    expect(timeInput).toBeTruthy();

    const nameInput = Array.from(document.querySelectorAll('input')).find(i => i.placeholder === 'Enter a task name')!;
    fireEvent.change(nameInput, { target: { value: 'OneShot' } });
    fireEvent.change(document.querySelector('textarea')!, { target: { value: 'run once' } });
    fireEvent.change(dateInput, { target: { value: '2026-08-20' } });
    fireEvent.change(timeInput, { target: { value: '14:30' } });
    fireEvent.click(screen.getByText('Save'));

    await waitFor(() => {
      const [, modeJson] = starriver().addScheduledTask.mock.calls.at(-1)!;
      expect(JSON.parse(modeJson)).toEqual({ type: 'once', datetime: '2026-08-20T14:30' });
    });
  });
});

const TASK_FIXTURE = {
  name: 'My Task', prompt: 'do stuff', model: '', enabled: true,
  modeType: 'periodic', period: 'daily', time: '09:00', dayOfWeek: 1, dayOfMonth: 1, month: 1,
  hours: 3, daysOfWeek: [1, 2, 3, 4, 5], datetime: '',
  startDate: '', endDate: '', skills: [], description: 'Daily 09:00',
  lastRun: '', nextRun: '', lastStatus: '',
};

describe('ScheduledTasksTab — task list (WI-174: no sub-tabs)', () => {
  // Each test pins listScheduledTasks so earlier mutations don't bleed across tests.
  beforeEach(() => {
    (window as any).starriver = {
      ...(window as any).starriver,
      listScheduledTasks: () => Promise.resolve([]),
      listJobs: () => Promise.resolve([]),
    listSessions: () => Promise.resolve([]),
    listArchivedSessions: () => Promise.resolve([]),
    listRunningSessions: () => Promise.resolve([]),
    listAllTaskRuns: () => Promise.resolve([]),
      getSessionTrace: () => Promise.resolve({ entries: [] }),
    };
  });

  it('shows the task list with an empty hint when no tasks exist', async () => {
    render(<I18nProvider><WorkspaceProvider><ScheduledTasksTab /></WorkspaceProvider></I18nProvider>);
    await screen.findByText('No scheduled tasks configured.');
    // WI-174: guidance replaces the old default-to-Templates-tab conversion.
    expect(screen.getByText(/quick start/i)).toBeTruthy();
    expect(screen.queryByText('Hourly Weather')).toBeNull();
    expect(screen.queryByTestId('tpl-popover')).toBeNull();
  });

  it('shows existing tasks without any template grid or segmented tabs', async () => {
    starriver().listScheduledTasks = () => Promise.resolve([TASK_FIXTURE as any]);
    render(<I18nProvider><WorkspaceProvider><ScheduledTasksTab /></WorkspaceProvider></I18nProvider>);
    await screen.findByText('My Task');
    expect(screen.queryByText('Hourly Weather')).toBeNull();
    expect(screen.queryByText('Templates')).toBeNull();
  });

  it('exposes both New Task and New Task from Template buttons', () => {
    render(<I18nProvider><WorkspaceProvider><ScheduledTasksTab /></WorkspaceProvider></I18nProvider>);
    expect(screen.getByText('New Task')).toBeTruthy();
    expect(screen.getByText('New Task from Template')).toBeTruthy();
  });
  });

describe('ScheduledTasksTab — agent selector (WI-64 Phase B tail)', () => {
  it('选择 agent 后保存时传给 addScheduledTask;默认为 null(Chat)', async () => {
    starriver().addScheduledTask.mockClear();
    render(<I18nProvider><WorkspaceProvider><ScheduledTasksTab /></WorkspaceProvider></I18nProvider>);
    fireEvent.click(screen.getByText('New Task'));
    await waitFor(() => expect(screen.getByPlaceholderText('Enter a task name')).toBeTruthy());
    fireEvent.change(screen.getByPlaceholderText('Enter a task name'), { target: { value: 'T' } });
    fireEvent.change(screen.getByPlaceholderText('Describe what the agent should do when triggered...'), { target: { value: 'do it' } });
    // Default: no agent picked → null.
    fireEvent.click(screen.getByText('Save'));
    await waitFor(() => {
      const agentArg = starriver().addScheduledTask.mock.calls.at(-1)[4];
      expect(agentArg).toBeNull();
    });
    // Pick the coding agent → passed through (re-open: the first save closes the form).
    starriver().addScheduledTask.mockClear();
    fireEvent.click(screen.getByText('New Task'));
    await waitFor(() => expect(screen.getByPlaceholderText('Enter a task name')).toBeTruthy());
    fireEvent.change(screen.getByPlaceholderText('Enter a task name'), { target: { value: 'T2' } });
    fireEvent.change(screen.getByPlaceholderText('Describe what the agent should do when triggered...'), { target: { value: 'do it' } });
    fireEvent.change(screen.getByTestId('scheduled-agent'), { target: { value: 'Starriver Code' } });
    fireEvent.click(screen.getByText('Save'));
    await waitFor(() => {
      const agentArg = starriver().addScheduledTask.mock.calls.at(-1)[4];
      expect(agentArg).toBe('Starriver Code');
    });
  });
});

describe('WI-179 — create-then-run', () => {
  function fillAndSave(name: string) {
    fireEvent.click(screen.getByText('New Task'));
    fireEvent.change(screen.getByPlaceholderText('Enter a task name'), { target: { value: name } });
    fireEvent.change(screen.getByPlaceholderText('Describe what the agent should do when triggered...'), { target: { value: 'do it' } });
    fireEvent.click(screen.getByText('Save'));
  }

  it('runs the task immediately after creation (the first run builds the session)', async () => {
    starriver().addScheduledTask.mockClear();
    starriver().runScheduledTaskNow.mockClear();
    render(<I18nProvider><WorkspaceProvider><ScheduledTasksTab /></WorkspaceProvider></I18nProvider>);
    fillAndSave('First Run Task');
    await waitFor(() => {
      expect(starriver().addScheduledTask).toHaveBeenCalledWith(
        'First Run Task', expect.any(String), 'do it', null, null, null, true, null, null, [],
      );
    });
    await waitFor(() => {
      expect(starriver().runScheduledTaskNow).toHaveBeenCalledTimes(1);
      expect(starriver().runScheduledTaskNow).toHaveBeenCalledWith('First Run Task');
    });
    // Editing does NOT trigger a run: only creation does.
    starriver().runScheduledTaskNow.mockClear();
  });

  it('a failed first run does not roll back the saved task and surfaces the error', async () => {
    starriver().addScheduledTask.mockClear();
    starriver().runScheduledTaskNow.mockRejectedValueOnce(new Error('model unavailable'));
    render(<I18nProvider><WorkspaceProvider><ScheduledTasksTab /></WorkspaceProvider></I18nProvider>);
    fillAndSave('Failing First Run');
    await waitFor(() => {
      expect(starriver().addScheduledTask).toHaveBeenCalled();
    });
    await waitFor(() => {
      expect(starriver().runScheduledTaskNow).toHaveBeenCalledWith('Failing First Run');
    });
    // Task creation still succeeded (form closed) and the failure is visible.
    await waitFor(() => {
      expect(document.body.textContent).toContain('first run failed');
    });
  });
});

describe('WI-180 — Run Now jumps to the run session', () => {
  it('dispatches open-session with the run sessionId once the scheduler registers the run', async () => {
    const TASK = {
      name: 'Jump Task', prompt: 'p', enabled: true, modeType: 'interval',
      hours: 1, daysOfWeek: [0, 1, 2, 3, 4, 5, 6],
    };
    (window as any).starriver = {
      ...(window as any).starriver,
      listScheduledTasks: () => Promise.resolve([TASK]),
      listAllTaskRuns: () => Promise.resolve([]),
      listJobs: () => Promise.resolve([
        { taskName: 'Jump Task', status: 'running', startedAt: '2026-10-04T12:00:00Z', summary: '', sessionId: 's-jump-9' },
      ]),
    };
    const seen: string[] = [];
    const listener = (e: Event) => seen.push((e as CustomEvent<{ sessionId: string }>).detail.sessionId);
    window.addEventListener('starriver:open-session', listener);
    try {
      render(<I18nProvider><WorkspaceProvider><ScheduledTasksTab /></WorkspaceProvider></I18nProvider>);
      await waitFor(() => expect(screen.getByText('Jump Task')).toBeTruthy());
      fireEvent.click(screen.getByText('Run Now'));
      // The poll finds the active run's sessionId (≤ a few 400ms ticks) and
      // asks the shell to open it.
      await waitFor(() => {
        expect(seen).toContain('s-jump-9');
      }, { timeout: 4000 });
    } finally {
      window.removeEventListener('starriver:open-session', listener);
    }
  });
});
