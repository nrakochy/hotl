# hotl — human on the loop

[![crates.io](https://img.shields.io/crates/v/hotl.svg)](https://crates.io/crates/hotl)

One binary, three capabilities, and you on the loop for all of them:

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
