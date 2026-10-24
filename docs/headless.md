# Headless integration guide

How to drive starriverd (the daemon) from scripts, CI, and benchmark
harnesses. The daemon listens on `POST /api/v1/invoke` (JSON-RPC-style
envelope, Bearer token from the boot banner).

## Recommended: execute + waitTurn

```jsonc
// 1. start a turn — success means the request was ACCEPTED, not that the
//    turn finished (or even started pulling work yet)
POST /api/v1/invoke
{ "v": 1, "id": "r1", "method": "session.execute",
  "params": { "task": "run the test suite", "sessionId": "..." } }

// 2. block until the turn ends (runs on the server's blocking pool)
POST /api/v1/invoke
{ "v": 1, "id": "r2", "method": "session.waitTurn",
  "params": { "sessionId": "...", "timeoutMs": 600000 } }

// → { "ended": true, "reason": "completed",
//     "finalAssistant": { "role": "assistant", "content": "...", ... } }
```

`reason` values:

| reason | meaning | what to do |
|---|---|---|
| `completed` | the turn ended | read `finalAssistant` |
| `timeout` | still running at the deadline | call `waitTurn` again to keep waiting |
| `session_not_running` | no input slot held — the execute never started, or the session id is unknown | re-send the execute once; if it repeats, check the session id |

`timeoutMs` defaults to 300000 (5 min), hard cap 3600000 (1 h).

## Polling (`sessions.running` + `session.messages`) — known traps

The polling path still works, but three failure modes have bitten real
drivers (documented so you can recognize them):

1. **Completion race.** The final assistant message can land one poll cycle
   BEFORE the turn leaves `sessions.running`. Any client that gates
   completion on "new messages arrived" will wait forever after the last
   message. Gate on the running set instead — or just use `waitTurn`, which
   keys on the slot state and cannot have this race.
2. **Lost execute.** `session.execute` can return success without a turn
   starting. Poll `sessions.running` for your session id and re-send if it
   never appears (`waitTurn` reports this as `session_not_running`).
3. **Compaction rewrites history.** When context compaction fires mid-turn,
   `session.messages` can SHRINK (observed 217 → 168). Baseline-diff
   bookkeeping ("everything after index N is new") silently breaks. Treat
   the message list as a view, not an append-only log.

## Other notes for headless clients

- **Permission preset**: `session.execute` does not take a preset parameter;
  create the session first, then `HostRuntime::__api_session_setPermissionPreset`
  (e.g. `danger-full-access`) before the first execute.
- **Idempotency**: the daemon caches successful responses by request `id`
  for 15 minutes. Reusing an id returns the cached response — generate a
  fresh id per logical request.
- **Parallel tool calls**: keep `agent.max_parallel_tool_calls: 1` in
  settings until the scheduler drain fix lands (upstream issue #2);
  a parallel `tool_use` block can wedge the turn.

Evidence for the traps: harness2 adaptation campaign logs
(`docs/STARRIVER-FINDINGS.md` in the harness2 repository).
