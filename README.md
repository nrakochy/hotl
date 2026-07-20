# hotl — human on the loop

[![crates.io](https://img.shields.io/crates/v/hotl.svg)](https://crates.io/crates/hotl)

Running one agent is easy. Running several, all day, is a supervision
problem: knowing which one is blocked on you, trusting what they're allowed
to do, and recovering when one goes sideways. hotl is one binary that takes
that problem in three stages — watch the agents you already run, run its own
agent with guardrails you can see, and eventually orchestrate fleets — with
you on the loop at every stage:

| Capability | Command | Status |
|---|---|---|
| **Watch** | `hotl watch` | **Shipped** — a tmux dashboard that discovers your AI-agent processes, shows live status, pings when one is blocked on you, and jumps focus to it |
| **Execute** | `hotl` | **Building** — a personal agent harness (event-log-as-canon, ACP-native): steering REPL + `-p` headless, gated tools under a kernel sandbox floor, managed context (compaction/memory), MCP client, session resume + `undo`. Any OpenAI-compatible or Anthropic model. **[User docs → docs/user/](docs/user/index.md)** |
| **Orchestrate** | `hotl fleet` | **Future** — drives fleets of agents over the same protocol any editor uses; only its seams exist today |

> **Pre-1.0 — and a breaking change:** bare `hotl` is now the **agent**; the
> dashboard moved to `hotl watch`. Every mutating or executing tool call asks
> y/n; a kernel sandbox floor confines `bash` writes. The execute harness is
> **not yet published** — `cargo install hotl` still installs the older
> `watch`-only release; run the agent from a source build ([docs/user/quickstart.md](docs/user/quickstart.md))
> until 0.2.0 ships. Expect breaking changes at every 0.x minor.

## Why hotl

**Stay in charge without babysitting.** Agents earn their keep on long runs,
but long runs block on you at unpredictable moments — and the usual answer is
cycling through panes to check. `hotl watch` replaces that with a dashboard:
it discovers every agent across your tmux session, shows who's working and
who's waiting, pings when one needs you, and `enter` jumps focus straight to
it. Your attention goes where it's actually needed.

**Safety is the default, not a flag.** Every mutating or executing tool call
asks y/n before it runs. `bash` executes under a kernel sandbox floor
(Seatbelt on macOS, Landlock on Linux) that confines writes to the working
directory. Writes to execute-later paths — git hooks, shell rc, Makefiles,
agent-instruction files — always escalate with a warning that says why.
Secret-named env values are masked before bytes ever land on disk. And the
stance is written down honestly, including what the sandbox does **not**
cover: [`docs/SECURITY.md`](docs/SECURITY.md).

**Nothing is ever lost.** Resume any session, `undo` the agent's file
changes, steer mid-turn without losing the thread. This works because every
session is recorded as an append-only log that nothing rewrites — even
context compaction adds a summary on top instead of destroying history, so
a failed compaction can't brick a session.

**Standard protocols, any model.** Anthropic or any OpenAI-compatible
endpoint (OpenAI, Groq, Ollama, a local server — it's just a base URL). MCP
for tools, ACP for embedding in editors — the same contract the future
`hotl fleet` orchestrator will speak, so the seams are already in place.

## Watch — quick start

**Requirements:** [tmux](https://github.com/tmux/tmux) on your `PATH` (run it from inside a tmux session) and `ps` (standard on macOS/Linux).

Install a prebuilt binary — no toolchain needed:

    curl --proto '=https' --tlsv1.2 -LsSf https://github.com/nrakochy/hotl/releases/latest/download/hotl-installer.sh | sh

Or with Rust installed:

    cargo install hotl

Then, from inside tmux, open a pane and run it:

    hotl

Keys: `j`/`k` (or ↓/↑) move · `enter` jump to the selected agent · `r` refresh
· `q` or `Ctrl-c` quit · `Ctrl-h`/`j`/`k`/`l` switch tmux panes.

**Full dashboard docs — install options, usage, config, keys:** [`crates/hotl/README.md`](crates/hotl/README.md).

## The docs

[`ARCHITECTURE.md`](ARCHITECTURE.md) is the harness at a glance — the layers, the connective planes, and how a prompt flows through the system. The [user docs](docs/user/index.md) cover installing and running the agent, and [`docs/SECURITY.md`](docs/SECURITY.md) is the security stance.

## Releasing

Cut a release with the helper script — it bumps the workspace version, commits,
tags `vX.Y.Z`, and pushes. The tag triggers the crates.io publish and the
prebuilt-binary/installer workflows.

    scripts/release.sh patch    # bug fix
    scripts/release.sh minor    # feature, or breaking pre-1.0
    scripts/release.sh major    # 1.0
    scripts/release.sh 0.4.2    # explicit version

Versions are immutable on crates.io — always go up, never reuse one. The tag
must match the `[workspace.package]` version (the script keeps them in sync).
The lib crates publish in lockstep and are internal — no semver promise attaches
to their APIs, only to the binary's contracts.

## License

MIT OR Apache-2.0.
