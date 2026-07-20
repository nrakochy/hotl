# hotl user docs — `hotl` the agent (execute)

User-facing docs for the **execute** capability: the coding agent behind the bare `hotl` command. For the `hotl watch` dashboard, see [crates/hotl/README.md](../../crates/hotl/README.md). For design and internals, start at [AGENTS.md](../../AGENTS.md).

Docs here follow the five-mode framework (specs `docs/references/documentation-authoring.md`): one doc, one mode. Read by need:

| You want to… | Read | Mode |
|---|---|---|
| Run it the first time, start to finish | [quickstart.md](quickstart.md) | Tutorial |
| Look up a config file, env var, subcommand, or exit code | [configuration.md](configuration.md) | Reference |
| Understand the y/N gate, protected paths, and the sandbox — and what they don't cover | [permissions-and-sandbox.md](permissions-and-sandbox.md) | Explanation |
| Run a session detached and reconnect later | [backgrounding.md](backgrounding.md) | How-to |
| Connect an MCP tool server | [mcp.md](mcp.md) | How-to |
| Run your own checks/policy on tool calls (diagnostics + hooks) | [hooks.md](hooks.md) | How-to |
| Remove hotl and its data | [uninstall.md](uninstall.md) | How-to |
| Fix an error you hit | [troubleshooting.md](troubleshooting.md) | Reference (error → cause → fix) |

**Status (2026-07-20):** milestones M0–M5 implemented (M4 surfaces/orchestration and M5 extension hooks landed; the durable-asks M4 pin is tracked separately in the 0006 plan); pre-1.0. The execute harness is **not yet published to crates.io** — `cargo install hotl` still installs the older `watch`-only release. Until 0.2.0 ships, run the agent from a source build ([quickstart.md](quickstart.md)). Advanced surfaces (`hotl acp`, `spawn` sub-agents) are covered in the design docs, not yet here.
