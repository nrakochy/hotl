---
title: 'hotl user docs — hotl the agent (execute)'
---

User-facing docs for the **execute** capability: the coding agent behind the bare `hotl` command. For the `hotl watch` dashboard, see [crates/hotl/README.md](https://github.com/nrakochy/hotl/blob/master/crates/hotl/README.md). For the architecture, see [ARCHITECTURE.md](https://github.com/nrakochy/hotl/blob/master/ARCHITECTURE.md).

Docs here follow a five-mode framework — one doc, one mode. Read by need:

| You want to… | Read | Mode |
|---|---|---|
| Run it the first time, start to finish | [quickstart.md](quickstart/) | Tutorial |
| Look up a config file, env var, subcommand, or exit code | [configuration.md](configuration/) | Reference |
| Understand the y/N gate, protected paths, and the sandbox — and what they don't cover | [permissions-and-sandbox.md](permissions-and-sandbox/) | Explanation |
| Run a session detached and reconnect later | [backgrounding.md](backgrounding/) | How-to |
| Drive the agent from a full-screen console | [tui.md](tui/) | How-to |
| Connect an MCP tool server | [mcp.md](mcp/) | How-to |
| Run through a gateway / fetch keys from a command | [gateway.md](gateway/) | How-to |
| Run your own checks/policy on tool calls (diagnostics + hooks) | [hooks.md](hooks/) | How-to |
| Remove hotl and its data | [uninstall.md](uninstall/) | How-to |
| Fix an error you hit | [troubleshooting.md](troubleshooting/) | Reference (error → cause → fix) |

**Status (2026-07-20):** pre-1.0; the core harness, the headless/ACP surfaces, and the extension hooks described here are implemented. The execute harness is **not yet published to crates.io** — `cargo install hotl` still installs the older `watch`-only release. Until 0.2.0 ships, run the agent from a source build ([quickstart.md](quickstart/)). Advanced surfaces (`hotl acp`, `spawn` sub-agents) are not yet documented here. Permission prompts are opt-in as of the next release (`[permissions] mode = "auto"` is the default) — see [permissions-and-sandbox.md](permissions-and-sandbox/).
