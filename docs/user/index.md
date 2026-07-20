# hotl user docs — `hotl` the agent (execute)

User-facing docs for the **execute** capability: the coding agent behind the bare `hotl` command. For the `hotl watch` dashboard, see [crates/hotl/README.md](../../crates/hotl/README.md). For design and internals, start at [AGENTS.md](../../AGENTS.md).

Docs here follow the five-mode framework (specs `docs/references/documentation-authoring.md`): one doc, one mode. Read by need:

| You want to… | Read | Mode |
|---|---|---|
| Run it the first time, start to finish | [quickstart.md](quickstart.md) | Tutorial |
| Look up a config file, env var, subcommand, or exit code | [configuration.md](configuration.md) | Reference |
| Understand the y/N gate, protected paths, and the sandbox — and what they don't cover | [permissions-and-sandbox.md](permissions-and-sandbox.md) | Explanation |
| Fix an error you hit | [troubleshooting.md](troubleshooting.md) | Reference (error → cause → fix) |

**Status (2026-07-20):** milestones M0–M3 implemented; pre-1.0. The execute harness is **not yet published to crates.io** — `cargo install hotl` still installs the older `watch`-only release. Until 0.2.0 ships, run the agent from a source build ([quickstart.md](quickstart.md)). Not every surface is documented here yet (MCP setup and the hooks guide are stubs); gaps are listed at the bottom of each file.
