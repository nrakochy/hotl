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
| **Execute** | `hotl` | **Shipped** — a personal agent harness (event-log-as-canon, ACP-native): steering console TUI + `-p` headless, gated tools under a kernel sandbox floor, managed context (compaction/memory), MCP client, session resume + `undo`. Any OpenAI-compatible or Anthropic model. **[User docs → nrakochy.github.io/hotl](https://nrakochy.github.io/hotl/)** |
| **Watch** | `hotl watch` | **Shipped** — a tmux dashboard that discovers your AI-agent processes, shows live status, pings when one is blocked on you, and jumps focus to it |
| **Orchestrate** | `hotl fleet` | **Future** — drives fleets of agents over the same protocol any editor uses; only its seams exist today |

> **Pre-1.0 — and a breaking change at 0.2.0:** bare `hotl` is now the
> **agent**; the dashboard moved to `hotl watch`. Every mutating or executing
> tool call passes a permission gate, and a kernel sandbox floor confines
> `bash` writes. Expect breaking changes at every 0.x minor — see
> [CHANGELOG.md](CHANGELOG.md).

## Why hotl

**Stay in charge without babysitting.** Agents earn their keep on long runs,
but long runs block on you at unpredictable moments — and the usual answer is
cycling through panes to check. `hotl watch` replaces that with a dashboard:
it discovers every agent across your tmux session, shows who's working and
who's waiting, pings when one needs you, and `enter` jumps focus straight to
it. Your attention goes where it's actually needed.

**A safety floor that never turns off — and prompts only if you want them.**
By default hotl runs uninterrupted: no per-action y/n. What always holds:
`bash` executes under a kernel sandbox floor (Seatbelt on macOS, Landlock on
Linux) confining writes to the working directory; writes to execute-later
paths — git hooks, shell rc, Makefiles, agent-instruction files — always
stop and ask, in every mode; every silenced prompt is visible in the
transcript; and `hotl undo` reverses any approved-by-default change. Prefer
per-action approval? `[permissions] mode = "ask"`. Need it guaranteed?
Compile with `--features security-enforced` and prompting cannot be disabled
by any config. The stance is written down honestly, including what the
sandbox does **not** cover: [`docs/SECURITY.md`](docs/SECURITY.md).

**Nothing is ever lost.** Resume any session, `undo` the agent's file
changes, steer mid-turn without losing the thread. This works because every
session is recorded as an append-only log that nothing rewrites — even
context compaction adds a summary on top instead of destroying history, so
a failed compaction can't brick a session.

**Standard protocols, any model.** Anthropic or any OpenAI-compatible
endpoint (OpenAI, Groq, Ollama, a local server — it's just a base URL). MCP
for tools, ACP for embedding in editors — the same contract the future
`hotl fleet` orchestrator will speak, so the seams are already in place.

## Install

Prebuilt binary — no toolchain needed (macOS / Linux):

    curl --proto '=https' --tlsv1.2 -LsSf https://github.com/nrakochy/hotl/releases/latest/download/hotl-installer.sh | sh

Or with Rust ≥ 1.88 installed:

    cargo install hotl

## Execute — quick start

Point `HOTL_MODEL` at a model (`provider/model` — `anthropic/…` or `openai/…`, which covers any OpenAI-compatible endpoint incl. local Ollama), then:

    hotl doctor    # provider, sandbox floor, config, sessions — all should read ok
    hotl           # interactive console TUI
    hotl -p "fix the typo in main.rs"   # headless one-shot

Full tutorial: [quickstart](https://nrakochy.github.io/hotl/quickstart/).

## Watch — quick start

**Requirements:** [tmux](https://github.com/tmux/tmux) on your `PATH` (run it from inside a tmux session) and `ps` (standard on macOS/Linux).

From inside tmux, open a pane and run it:

    hotl watch

Keys: `j`/`k` (or ↓/↑) move · `enter` jump to the selected agent · `r` refresh
· `q` or `Ctrl-c` quit · `Ctrl-h`/`j`/`k`/`l` switch tmux panes.

**Full dashboard docs — install options, usage, config, keys:** [`crates/hotl/README.md`](crates/hotl/README.md).

## The docs

[`ARCHITECTURE.md`](ARCHITECTURE.md) is the harness at a glance — the layers, the connective planes, and how a prompt flows through the system. The [user docs](https://nrakochy.github.io/hotl/) (source in `site/src/content/docs/`, deployed on each release) cover installing and running the agent, and [`docs/SECURITY.md`](docs/SECURITY.md) is the security stance.

## Releasing

Cut a release with the helper script — it bumps the workspace version (and
every internal path-dep pin, which publish in lockstep), commits, tags
`vX.Y.Z`, and pushes. The tag triggers the crates.io publish and the
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

hotl is dual-licensed:

1. **Open source:** GNU Affero General Public License v3.0 or later
   ([AGPL-3.0-or-later](LICENSE)).
2. **Commercial:** commercial licenses are available for organizations that
   cannot comply with AGPL. Contact nick.rakochy@gmail.com for details.
