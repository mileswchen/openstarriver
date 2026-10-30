# Browser automation via external MCP (option A)

The first-party `browser` tool family (see [browser.md](browser.md)) is the
recommended path. This page documents the alternative that works with zero
Rust code today: attaching an external browser MCP server through starriver's
MCP bridge — the same approach Claude Code / Gemini CLI / Cline document.

## Quick start (Playwright MCP)

```yaml
# ~/.starriver/mcp.yaml
mcp_servers:
  playwright:
    transport: stdio
    command: npx
    args: ["@playwright/mcp@latest"]
```

or Google's CDP-flavored server (matches what we built first-party):

```yaml
mcp_servers:
  chrome-devtools:
    transport: stdio
    command: npx
    args: ["chrome-devtools-mcp@latest"]
```

Requires Node.js + npx on PATH. The bridge auto-discovers tools, health-
checks, and retries (stdio/SSE/HTTP all supported).

## Known limitations vs the first-party family

These are the reasons option A is a stopgap, not the destination:

1. **Screenshots don't reach the attachment pipeline.** The MCP adapter
   passes tool results as text; a base64 screenshot comes back as a wall of
   base64 (or is dropped) instead of a `sha256:` attachment ref the UI
   renders inline. This alone roughly doubles the cost of every screenshot.
2. **No prompt-guide section.** First-party tool families carry a
   `<tool>:guide` division-of-labor section steering the model (snapshot
   before screenshot, web_fetch for static pages, untrusted-content
   warning). MCP tools arrive as bare schemas.
3. **No spill/timeout integration.** Oversized MCP outputs are not spilled
   to disk with a truncation note; there is no per-tool timeout budget.
4. **Context bloat.** Playwright MCP registers ~20 tools; our family is 9
   with terser schemas.
5. **Node dependency** for the MCP server process.

## When to still use it

- You want Playwright's exact semantics (network capture, tracing, PDF) that
  the first-party family hasn't grown yet (see Phase 2 in browser.md).
- Evaluating/prototyping before relying on the first-party path.
