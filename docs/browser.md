# Browser automation (first-party `browser` tools)

Starriver ships first-party browser automation: a CDP-driven
(`chromiumoxide`) Chromium/Chrome session service in `crates/starriver-browser`,
exposed to the model as a nine-tool family registered by the host at boot
(same seam as the persistent-terminal tools).

## Tools

| Tool | What it does |
|---|---|
| `browser_start` | Launch a browser session (headless by default) and return its id. Requires approval (it spawns a host process), mirroring `terminal_start`. |
| `browser_navigate` | Open an http(s) URL (localhost/private allowed — testing your own dev server is the point; `file://` and other schemes are refused), settle to load, report final URL + title. |
| `browser_snapshot` | **The grounding primitive.** Renders the page as a text outline with `[eN]` refs on interactive elements (snapshot-ref model, same as Playwright's aria snapshot / chrome-devtools-mcp). Cheaper and steadier than screenshots. |
| `browser_click` / `browser_type` | Act on the refs `browser_snapshot` assigned. A navigation invalidates refs — the error says so and tells the model to re-snapshot. |
| `browser_eval` | Evaluate JS and return its JSON value (capped). |
| `browser_screenshot` | PNG → content-addressed attachment store → `![browser-screenshot](sha256:…)` markdown ref the UI renders inline (imagegen parity). |
| `browser_list` / `browser_close` | Enumerate / close sessions (closing kills the process and removes the profile dir). |

Sessions are owner-scoped (`agent_id`), survive across tool calls, and are
reaped three ways: explicit `browser_close`, the 15-minute idle reaper, and
`kill_all` at host shutdown (terminal-service parity).

## Settings (`~/.starriver/settings.yaml`)

```yaml
browser:
  browser_path: /Applications/Google Chrome.app/Contents/MacOS/Google Chrome  # optional
  headless: true          # default; false opens a visible window (local desktop)
  viewport: 1280x800      # default
  idle_timeout_secs: 900  # default
  max_sessions: 4         # default
```

Browser binary discovery: `browser.browser_path` > per-call `browserPath` >
`STARRIVER_BROWSER_BIN` env > platform defaults (Chrome, Chromium, Edge).
No browser installed → `browser_start` fails with `NO_BROWSER` and names the
candidates it tried.

## Security posture

- Navigation is restricted to http/https; `file://`, `javascript:` etc. are
  refused before any session state is touched.
- Local/private network access is **allowed by design** — the primary use
  case is driving a dev server the agent just started (bash already has full
  network access; SSRF protection at this layer would be theater).
- Page content is untrusted data — the `browser:guide` prompt section
  carries the same warning `web_fetch`'s does.
- Ref arguments are JSON-escaped and shape-checked (`eN` only) before they
  reach the page.

## Tests

- `crates/starriver-browser`: 16 unit tests (snapshot renderer, URL gate,
  viewport parsing, ref validation, session registry, error paths) — no
  browser needed.
- `crates/starriver-host/src/tools/browser.rs`: 8 executor tests (args
  validation, stable error codes, family shape) — no browser needed.
- `crates/starriver-browser/tests/live.rs`: full live flow
  (start → navigate → snapshot → type/submit → click → stale-ref →
  screenshot → close) against a locally-installed Chrome. **Ignored by
  default**; run with:

  ```sh
  cargo test -p starriver-browser --test live -- --ignored
  ```

## Environment quirks (write-restricted sandbox)

Two known failures on sandboxed dev machines (Seatbelt profile denying
`/dev/null` writes), documented so they are recognizable:

1. **Chrome itself cannot start** — its Crashpad handler opens `/dev/null`
   at boot (`close_stdio.cc` FATAL). The `#[ignore]`d live test is the
   correct answer there: run it on an unrestricted machine.
2. **`cargo test` fails in `rustix`'s build script** — its rustc feature
   probe redirects stdout/stderr to `/dev/null` and panics with EPERM.
   Local workaround (do NOT commit):

   ```sh
   # vendor rustix with build.rs Stdio::null → Stdio::piped,
   # then add to the workspace Cargo.toml:
   #   [patch.crates-io]
   #   rustix = { path = "vendor/rustix-0.38.44" }
   ```

   `cargo check` (no dev-deps) is unaffected.

## Relationship to web_fetch

`web_fetch` stays the tool for plain static pages (cheaper, cacheable,
SSRF-guarded). The browser family covers what it cannot do: JS-rendered
content, interacting with a web UI, and testing local dev servers. Phase 2
may add a hint in `web_fetch` output when a page looks JS-heavy.
