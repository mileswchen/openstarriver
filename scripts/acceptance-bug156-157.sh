#!/usr/bin/env bash
# Acceptance tests for BUG-156 (session.waitTurn) and BUG-157 (agent.tool_env).
#
# Boots two throwaway daemons from a given binary — one WITHOUT agent.tool_env
# (the "before") and one WITH it (the "after") — and runs five cases:
#
#   bug156-1  waitTurn returns completed + finalAssistant when the turn ends
#   bug156-2  waitTurn reports timeout while the turn is live, then completed
#   bug156-3  waitTurn reports session_not_running immediately for an
#             unknown/never-started session
#   bug157-1  WITHOUT tool_env the bash tool cannot run cargo
#   bug157-2  WITH tool_env {PATH, RUSTUP_HOME, CARGO_HOME} it can
#
# Usage:
#   scripts/acceptance-bug156-157.sh [binary]
#     default binary: target/debug/starriverd of this repo
#
# The script copies .credentials.yaml/settings.yaml/models.yaml from the
# real ~/.starriver, so the demo turns use the local model route. Working
# dirs live under /tmp/starriver-acceptance and are wiped per run.
set -uo pipefail

BIN="${1:-$(cd "$(dirname "$0")/.." && pwd)/target/debug/starriverd}"
[ -x "$BIN" ] || { echo "binary not found: $BIN"; exit 2; }
REAL_HOME="${REAL_HOME:-$HOME}"
WORK="${WORK:-/tmp/starriver-acceptance}"
mkdir -p "$WORK"
PASS=0; FAIL=0; FAILED_CASES=()

say()  { printf '\n\033[1m== %s ==\033[0m\n' "$1"; }
pass() { PASS=$((PASS+1)); printf '\033[32mPASS\033[0m %s\n' "$1"; }
fail() { FAIL=$((FAIL+1)); FAILED_CASES+=("$1"); printf '\033[31mFAIL\033[0m %s\n' "$1"; [ $# -gt 1 ] && printf '      %s\n' "$2"; }

# ── invoke helper: unique request ids every call (the daemon caches by id) ──
python3 - "$WORK" <<'PY'
import sys, textwrap
open(sys.argv[1] + "/invoke.py", "w").write(textwrap.dedent('''
    import json, random, sys, time, urllib.request
    BASE, TOK = sys.argv[1], sys.argv[2]
    _id = [int(time.time() * 1000) + random.randint(0, 999)]
    def invoke(method, params, timeout=240):
        _id[0] += 1
        body = json.dumps({"v": 1, "id": f"acc-{_id[0]}", "method": method,
                           "params": params}).encode()
        req = urllib.request.Request(BASE + "/api/v1/invoke", data=body, method="POST",
                                     headers={"Content-Type": "application/json",
                                              "Authorization": "Bearer " + TOK})
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return json.load(r)
    if __name__ == "__main__":
        method = sys.argv[3]
        params = json.loads(sys.argv[4]) if len(sys.argv) > 4 else {}
        print(json.dumps(invoke(method, params), ensure_ascii=False))
'''))
PY

# boot <name> <settings-extra-yaml>; echoes "<port> <token>"
boot() {
  local name="$1" extra="$2"
  local home="$WORK/$name/home"
  rm -rf "$WORK/$name"; mkdir -p "$home/.starriver/agents/demo" "$WORK/$name/run"
  for f in .credentials.yaml settings.yaml models.yaml; do
    [ -f "$REAL_HOME/.starriver/$f" ] && cp "$REAL_HOME/.starriver/$f" "$home/.starriver/$f"
  done
  cat > "$home/.starriver/agents/demo/AGENT.md" <<'MD'
---
name: demo
description: acceptance demo agent
mode: build
model:
  default: zhipu/glm-5.3-flash
---

You are a terse demo agent. Run the exact command the task names, then report
the raw output verbatim. Do NOT attempt recovery or workarounds if a command
fails — report the failure verbatim.
MD
  python3 - "$home/.starriver/settings.yaml" "$extra" <<'PY'
# Patch the agent: section IN PLACE. A duplicate top-level agent: key makes
# the YAML corrupt and the daemon quarantines the file (settings.yaml.corrupt)
# with default providers — silently no model route.
import os, re, sys
path, extra = sys.argv[1], sys.argv[2]
t = open(path).read() if os.path.exists(path) else ""
t = re.sub(r"active_workspaces:\n(    .*\n)+", "active_workspaces: {}\n", t)
if re.search(r"^agent:\n", t, re.M):
    if "max_parallel_tool_calls" in t:
        t = re.sub(r"max_parallel_tool_calls:.*", "max_parallel_tool_calls: 1", t)
    else:
        t = t.replace("agent:\n", "agent:\n  max_parallel_tool_calls: 1\n", 1)
    if extra:
        t = t.replace("agent:\n", "agent:\n" + extra + "\n", 1)
else:
    t += "\nagent:\n  max_parallel_tool_calls: 1\n" + (extra + "\n" if extra else "")
open(path, "w").write(t)
PY
  ( cd "$WORK/$name/run" && env HOME="$home" PATH="$REAL_HOME/.cargo/bin:$PATH" \
      "$BIN" > boot.json 2> boot.err & )
  for _ in $(seq 1 20); do
    sleep 1
    [ -s "$WORK/$name/run/boot.json" ] && grep -q token "$WORK/$name/run/boot.json" 2>/dev/null && break
  done
  python3 - "$WORK/$name/run/boot.json" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
print(d["port"], d["token"])
PY
}

shutdown() {
  [ -n "${1:-}" ] && env HOME="$WORK/$2/home" "$BIN" --shutdown >/dev/null 2>&1 || true
}

wait_completed() { # base token sid timeout_ms  -> "ended reason" line
  python3 "$WORK/invoke.py" "$1" "$2" session.waitTurn \
    "{\"sessionId\":\"$3\",\"timeoutMs\":$4}" \
  | python3 -c "import json,sys;r=json.load(sys.stdin).get('result') or {};print(r.get('ended'),r.get('reason'))"
}

approve_if_pending() { # base token sid — the default preset asks per bash call
  python3 "$WORK/invoke.py" "$1" "$2" approval.respond \
    "{\"sessionId\":\"$3\",\"allowed\":true,\"remember\":true}" >/dev/null 2>&1 || true
}

wait_sid() { # base token — poll sessions.running until the new turn registers
             # (execute returns on acceptance; registration lands a beat later)
  local sid=""
  for _ in $(seq 1 20); do
    sid=$(python3 "$WORK/invoke.py" "$1" "$2" sessions.running \
      | python3 -c "import json,sys;r=json.load(sys.stdin).get('result') or [];print(r[0] if r else '')")
    [ -n "$sid" ] && { echo "$sid"; return 0; }
    sleep 0.5
  done
  return 1
}

wait_turn_approved() { # base token sid max_secs — wait in short slices, re-
                       # answering the bash approval each round (the question
                       # only becomes answerable once the model calls the tool)
  local base="$1" tok="$2" sid="$3" deadline=$(( $(date +%s) + $4 )) r=""
  while [ "$(date +%s)" -lt "$deadline" ]; do
    r=$(wait_completed "$base" "$tok" "$sid" 4000)
    case "$r" in *"True completed"*) echo "$r"; return 0 ;; esac
    approve_if_pending "$base" "$tok" "$sid"
  done
  echo "$r"
}

last_assistant() { # base token sid -> last assistant message content
  python3 "$WORK/invoke.py" "$1" "$2" session.messages "{\"sessionId\":\"$3\"}" \
    | python3 -c "import json,sys;ms=json.load(sys.stdin).get('result') or [];print(next((m['content'] for m in reversed(ms) if m['role']=='assistant'),''))"
}

cargo_task='Use the bash tool to run exactly `cargo --version` once. Report its raw output verbatim. If it fails, report the failure verbatim. Do not retry or work around.'

say "booting daemon WITHOUT tool_env (before)"
read -r PORT_A TOK_A < <(boot a "")
say "booting daemon WITH tool_env (after: PATH + RUSTUP_HOME + CARGO_HOME)"
read -r PORT_B TOK_B < <(boot b "  tool_env:
    PATH: \"$REAL_HOME/.cargo/bin:\$PATH\"
    RUSTUP_HOME: \"$REAL_HOME/.rustup\"
    CARGO_HOME: \"$REAL_HOME/.cargo\"")
BASE_A="http://127.0.0.1:$PORT_A"; BASE_B="http://127.0.0.1:$PORT_B"

# ── bug156-3: not_running ────────────────────────────────────────────────
say "bug156-3 waitTurn on unknown session → session_not_running (immediately)"
T0=$(python3 -c 'import time;print(time.time())')
RES=$(python3 "$WORK/invoke.py" "$BASE_A" "$TOK_A" session.waitTurn \
  '{"sessionId":"no-such-session","timeoutMs":60000}')
T1=$(python3 -c 'import time;print(time.time())')
python3 - "$RES" "$T0" "$T1" <<'PY' && pass bug156-3 || fail bug156-3 "$RES"
import json, sys
res, t0, t1 = sys.argv[1], float(sys.argv[2]), float(sys.argv[3])
r = (json.loads(res).get("result") or {})
ok = (r.get("ended") is False and r.get("reason") == "session_not_running"
      and (t1 - t0) < 5)
sys.exit(0 if ok else 1)
PY

# ── bug157-1: before — cargo must fail without tool_env ─────────────────
say "bug157-1 WITHOUT tool_env: bash cargo --version fails"
python3 "$WORK/invoke.py" "$BASE_A" "$TOK_A" session.execute \
  "{\"task\":\"$cargo_task\",\"agent\":\"demo\"}" >/dev/null
SID_A=$(wait_sid "$BASE_A" "$TOK_A" || true)
[ -n "$SID_A" ] || { fail bug157-1 "turn never started (execute lost — that is bug156 territory; retry)"; }
if [ -n "$SID_A" ]; then
  wait_turn_approved "$BASE_A" "$TOK_A" "$SID_A" 120 >/dev/null
  OUT_A=$(last_assistant "$BASE_A" "$TOK_A" "$SID_A")
  python3 - "$OUT_A" <<'PY' && pass bug157-1 || fail bug157-1 "$OUT_A"
import sys
out = sys.argv[1]
bad = ("rustup could not choose" in out) or ("command not found" in out)
good = ("cargo " in out and "rustup could not choose" not in out)
sys.exit(0 if (bad and not good) else 1)
PY
fi

# ── bug156-2: timeout while live, then completed ────────────────────────
say "bug156-2 waitTurn: timeout while live → completed after the turn ends"
python3 "$WORK/invoke.py" "$BASE_B" "$TOK_B" session.execute \
  '{"task":"Use the bash tool to run exactly `sleep 8 && echo demo-done`. Report the raw output verbatim.","agent":"demo"}' >/dev/null
SID_B=$(wait_sid "$BASE_B" "$TOK_B" || true)
[ -n "$SID_B" ] || { fail bug156-2 "turn never started"; }
if [ -n "$SID_B" ]; then
  approve_if_pending "$BASE_B" "$TOK_B" "$SID_B"
  R1=$(wait_completed "$BASE_B" "$TOK_B" "$SID_B" 2500)
  case "$R1" in *"False timeout"*) pass bug156-2-timeout ;; *) fail bug156-2-timeout "$R1" ;; esac
  R2=$(wait_turn_approved "$BASE_B" "$TOK_B" "$SID_B" 120)
  FINAL=$(last_assistant "$BASE_B" "$TOK_B" "$SID_B")
  case "$R2" in *"True completed"*) pass bug156-2-completed ;; *) fail bug156-2-completed "$R2" ;; esac
  case "$FINAL" in *demo-done*) pass bug156-2-final-assistant ;; *) fail bug156-2-final-assistant "$FINAL" ;; esac
fi

# ── bug157-2: after — tool_env makes cargo work ─────────────────────────
say "bug157-2 WITH tool_env: bash cargo --version succeeds"
python3 "$WORK/invoke.py" "$BASE_B" "$TOK_B" session.execute \
  "{\"task\":\"$cargo_task\",\"agent\":\"demo\"}" >/dev/null
SID_C=$(wait_sid "$BASE_B" "$TOK_B" || true)
[ -n "$SID_C" ] || { fail bug157-2 "turn never started"; }
if [ -n "$SID_C" ]; then
  wait_turn_approved "$BASE_B" "$TOK_B" "$SID_C" 120 >/dev/null
  OUT_C=$(last_assistant "$BASE_B" "$TOK_B" "$SID_C")
  python3 - "$OUT_C" <<'PY' && pass bug157-2 || fail bug157-2 "$OUT_C"
import re, sys
out = sys.argv[1]
sys.exit(0 if re.search(r"cargo \d+\.\d+\.\d+", out) and "could not choose" not in out else 1)
PY
fi

# ── bug156-1: completed carries a non-empty finalAssistant ──────────────
say "bug156-1 waitTurn completed → finalAssistant is the last assistant message"
[ -n "${SID_C:-}" ] && python3 - "$OUT_C" <<'PY' && pass bug156-1 || fail bug156-1 "${OUT_C:-<no session>}"
import sys
sys.exit(0 if len(sys.argv[1].strip()) > 0 else 1)
PY

shutdown "$PORT_A" a
shutdown "$PORT_B" b

say "summary"
echo "PASS: $PASS  FAIL: $FAIL"
[ "$FAIL" -gt 0 ] && printf 'failed: %s\n' "${FAILED_CASES[*]}"
exit $([ "$FAIL" -eq 0 ] && echo 0 || echo 1)
