# hotl — human on the loop

[![crates.io](https://img.shields.io/crates/v/hotl.svg)](https://crates.io/crates/hotl)

One binary, three capabilities, and you on the loop for all of them:

| Capability | Command | Status |
|---|---|---|
| **Watch** | `hotl watch` | **Shipped** — a tmux dashboard that discovers your AI-agent processes, shows live status, pings when one is blocked on you, and jumps focus to it |
| **Execute** | `hotl` | **Building (M0)** — a personal agent harness (event-log-as-canon, ACP-native, designed in [docs/](AGENTS.md)): streaming REPL + `-p` headless, 4 gated tools, append-only session log. Needs `ANTHROPIC_API_KEY` |
| **Orchestrate** | `hotl fleet` | **Future** — drives fleets of agents over the same protocol any editor uses; only its seams exist today |

> **Pre-1.0 — and a breaking change:** bare `hotl` is now the **agent**; the
> dashboard moved to `hotl watch`. The harness is early M0 — every mutating or
> executing tool call asks y/n (no allow-rules exist yet, by design; the kernel
> sandbox lands at M1). Expect breaking changes at every 0.x minor.

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

[`AGENTS.md`](AGENTS.md) is the map. The short version: [`ARCHITECTURE.md`](ARCHITECTURE.md) is the harness at a glance; [`docs/design-docs/`](docs/design-docs/index.md) holds the settled design (including the vendored six-harness research corpus it cites); [`docs/exec-plans/`](docs/PLANS.md) holds the master plan, the merge plan, and two rounds of adversarial review with responses.

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
