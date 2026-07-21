---
title: 'Overview — what hotl is'
description: The design commitments behind the hotl agent — the append-only session log, the permission gate and kernel sandbox floor, provider seams, protocols, and surfaces — and a map of these docs by need.
---

hotl is a human-on-the-loop agent harness in one binary: bare `hotl` is the coding agent, `hotl watch` is the tmux dashboard for supervising agents, and `hotl fleet` (orchestration) is reserved. These docs cover the agent; for the dashboard see [crates/hotl/README.md](https://github.com/nrakochy/hotl/blob/master/crates/hotl/README.md), and for the internals see [ARCHITECTURE.md](https://github.com/nrakochy/hotl/blob/master/ARCHITECTURE.md).

## The design commitments

- **The session log is the source of truth.** Every session is an append-only event log that nothing rewrites. `hotl resume` continues from it, `hotl undo` reverses the agent's file changes via shadow-git snapshots taken around every mutating step, and context compaction appends a summary on top of history instead of replacing it — a failed compaction can't corrupt a session.
- **A permission gate with a kernel floor under it.** Every mutating or executing tool call passes the gate: `auto` (default, no ordinary prompts) or `ask` (y/N per call). Independent of mode, `bash` runs confined by Seatbelt (macOS) or Landlock (Linux) to writes inside the working directory, and writes to execute-later paths — git hooks, shell rc, Makefiles, agent-instruction files — always prompt. A build with `--features security-enforced` cannot have prompting disabled by any config. Network egress control is opt-in: `[network].allow` routes HTTP through a local allowlist proxy, and unenforceable restrictions fail loudly, never silently.
- **Any model, two provider seams.** `HOTL_MODEL=provider/model`: `anthropic/…` speaks the Messages API (SSE streaming, prompt-cache placement); `openai/…` speaks chat-completions and covers OpenAI, Groq, Ollama, gateways — anything with a base URL.
- **Standard protocols at the edges.** MCP client (stdio transport) for external tools; `hotl acp` serves ACP over stdio so any ACP-speaking editor can embed the agent — the same seam the future `hotl fleet` orchestrator will drive.
- **Surfaces for how you actually work.** A console TUI you can steer mid-turn, `-p` headless (with `--json`) for scripts and CI, a zsh `: ` prefix that turns a shell line into an agent prompt, `hotl bg`/`attach` for sessions that outlive your terminal, and `hotl watch` for supervising every agent in your tmux session.
- **Nothing hidden.** No daemon, no telemetry; config lives in `~/.config/hotl`, sessions in `~/.local/share/hotl`, and every auto-allowed call is visible in the transcript.

## Read by need

| You want to… | Read |
|---|---|
| Run it the first time, start to finish | [quickstart.md](../quickstart/) |
| Drive the agent from a full-screen console | [tui.md](../tui/) |
| Prompt the agent straight from your shell (`: ` prefix) | [shell.md](../shell/) |
| Run a session detached and reconnect later | [backgrounding.md](../backgrounding/) |
| Understand the y/N gate, protected paths, and the sandbox — and what they don't cover | [permissions-and-sandbox.md](../permissions-and-sandbox/) |
| Connect an MCP tool server | [mcp.md](../mcp/) |
| Run your own checks/policy on tool calls (diagnostics + hooks) | [hooks.md](../hooks/) |
| Run through a gateway / fetch keys from a command | [gateway.md](../gateway/) |
| Look up a config file, env var, subcommand, or exit code | [configuration.md](../configuration/) |
| Fix an error you hit | [troubleshooting.md](../troubleshooting/) |
| Remove hotl and its data | [uninstall.md](../uninstall/) |
| Point an AI agent at these docs | [llms.txt](../llms.txt) — the machine-readable map |

**Status (2026-07-21):** pre-1.0; the core harness, the headless/ACP surfaces, and the extension hooks described here are implemented and published — install with the [release installer or `cargo install hotl`](../quickstart/) (0.2.0+; earlier crates.io releases were the `watch`-only dashboard). Advanced surfaces (`hotl acp`, `spawn` sub-agents) are not yet documented here. Permission prompts are opt-in (`[permissions] mode = "auto"` is the default) — see [permissions-and-sandbox.md](../permissions-and-sandbox/).
