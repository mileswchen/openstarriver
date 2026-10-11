// Starriver Desktop TUI v4 — napi-rs single process + event callbacks

import { getRuntime, shutdown } from './runtime.js';

// ANSI colors
const C = { r: '\x1b[0m', d: '\x1b[2m', g: '\x1b[32m', y: '\x1b[33m', c: '\x1b[36m', R: '\x1b[31m', b: '\x1b[1m', w: '\x1b[37m' };
function clr(color: keyof typeof C, text: string) { return `${C[color]}${text}${C.r}`; }

async function main() {
  console.log(clr('b', '\n  Starriver Desktop — Coding Agent\n'));
  console.log(clr('d', '  Rust Core (napi-rs) — 单进程'));
  console.log(clr('d', '  /help 帮助  /tools 工具  Ctrl+C 退出\n'));

  const runtime = getRuntime();
  process.on('beforeExit', () => shutdown());

  process.stdout.write('> ');

  for await (const line of console) {
    const input = line.trim();
    if (!input) { process.stdout.write('> '); continue; }
    if (input.startsWith('/')) { await handleCommand(input, runtime); process.stdout.write('> '); continue; }

    await executeTask(runtime, input);
    process.stdout.write('> ');
  }
}

function printEvent(event: any) {
  switch (event.type) {
    case 'TaskStarted':      console.log(`  ${clr('c', '\u25b6')}  ${(event.session_id || '').slice(0, 8)}`); break;
    case 'TextDelta':        process.stdout.write(event.text || ''); break;
    case 'ToolCallStart':    console.log(`\n  ${clr('y', '\uD83D\uDD27')} ${event.name}`); break;
    case 'ToolCallResult':   console.log(`  ${event.ok ? clr('g', '\u2713') : clr('R', '\u2717')} [${(event.output || '').length}B]`); break;
    case 'ToolCallError':    console.log(`  ${clr('R', '\u2717')} ${event.error}`); break;
    case 'CompactionTriggered': console.log(`  ${clr('d', `\uD83D\uDCE6 ${event.before_tokens}\u2192${event.after_tokens}`)}`); break;
    case 'TaskCompleted':    console.log(`\n  ${clr('g', '\u2705')} ${event.summary || ''}`); break;
    case 'TaskFailed':       console.log(`\n  ${clr('R', '\u274C')} ${event.error}`); break;
    case 'TaskCancelled':    console.log(`\n  ${clr('y', '\u23F9')} \u5DF2\u53D6\u6D88`); break;
  }
}

async function executeTask(runtime: any, task: string) {
  console.log('');
  return new Promise<void>((resolve) => {
    runtime.execute(task, (_err: any, event: any) => {
      if (!event) return;
      if (event.type === 'ApprovalRequired') {
        // The stdin for-await consumer is blocked by executeTask — the answer line cannot be read here;
        // print the real semantics (auto-deny on timeout) instead of a [Y/n] prompt that cannot be honored —
        // otherwise a Y typed by the user would run as a new task after the task ends.
        process.stdout.write(`\n  ${clr('y', '\u26A0')}  ${event.tool}: ${event.reason}\n  ${clr('d', '\u5BA1\u6279\u5DF2\u8BF7\u6C42\uFF0C\u8D85\u65F6\u81EA\u52A8\u62D2\u7EDD')}\n`);
        return;
      }
      printEvent(event);
      if (event.type === 'TaskCompleted' || event.type === 'TaskFailed' || event.type === 'TaskCancelled') {
        resolve();
      }
    });
  });
}

async function handleCommand(cmd: string, runtime: any) {
  const [name, ...rest] = cmd.slice(1).split(' ');
  switch (name) {
    case 'health':
      console.log(clr('d', '  Starriver Desktop — Rust Core OK'));
      break;
    case 'tools': {
      const tools = runtime.listTools();
      console.log(tools.map((t: any) => clr('d', `  ${t.name.padEnd(12)} ${t.description}`)).join('\n'));
      break;
    }
    case 'plan': {
      // The /plan advertised by help: switch to plan execution mode and run the description as the task.
      try {
        runtime.setExecutionMode('plan');
        console.log(clr('d', '  \u5DF2\u5207\u6362\u5230 plan \u6A21\u5F0F\uFF08\u53EA\u8BFB\u89C4\u5212\uFF09'));
      } catch (e: any) {
        console.log(clr('y', `  \u5207\u6362\u5931\u8D25: ${e?.message ?? e}`));
      }
      const task = rest.join(' ').trim();
      if (task) await executeTask(runtime, task);
      break;
    }
    case 'help':
      console.log(clr('d', '\n  \u547D\u4EE4:\n    /plan <\u63CF\u8FF0>  \u5207\u5230 plan \u6A21\u5F0F\u5E76\u5206\u6790\u8BA1\u5212\n    /health       \u5065\u5EB7\u68C0\u67E5\n    /tools        \u5217\u51FA\u5DE5\u5177\n    /help         \u5E2E\u52A9\n    <\u8F93\u5165>        \u6267\u884C\u4EFB\u52A1\n'));
      break;
    default:
      console.log(clr('y', `  \u672A\u77E5: /${name}`));
  }
}

main().catch(err => { console.error('\u9519\u8BEF:', err); process.exit(1); });
