# hotl — human on the loop

[![crates.io](https://img.shields.io/crates/v/hotl.svg)](https://crates.io/crates/hotl)

Running one agent is easy. Running several, all day, is a supervision
problem: knowing which one is blocked on you, trusting what they're allowed
to do, and recovering when one goes sideways. hotl is one binary that takes
that problem in three stages, with you on the loop at every stage:

| Capability | Command | Status |
|---|---|---|
| **Execute** | `hotl` | **Shipped** — a personal agent harness: steering console TUI + `-p` headless, gated tools under a kernel sandbox floor, managed context, MCP client, ACP server, session resume + `undo`. Any Anthropic or OpenAI-compatible model. |
| **Watch** | `hotl watch` | **Shipped** — a tmux dashboard that discovers your AI-agent processes, shows live status, pings when one is blocked on you, and jumps focus to it. |
| **Orchestrate** | `hotl fleet` | **Future** — drives fleets of agents over the same protocol any editor uses; only its seams exist today. |

**User docs: [nrakochy.github.io/hotl](https://nrakochy.github.io/hotl/)** ·
[Architecture](https://github.com/nrakochy/hotl/blob/master/ARCHITECTURE.md) ·
[Security stance](https://github.com/nrakochy/hotl/blob/master/docs/SECURITY.md)

> Pre-1.0: expect breaking changes at every 0.x minor. As of 0.2.0, bare
> `hotl` is the agent; the dashboard moved to `hotl watch`.

## Install

Prebuilt binary, no toolchain needed (macOS / Linux):

    curl --proto '=https' --tlsv1.2 -LsSf https://github.com/nrakochy/hotl/releases/latest/download/hotl-installer.sh | sh

Or grab a `.tar.xz` for your platform from the
[latest release](https://github.com/nrakochy/hotl/releases/latest).
With Rust ≥ 1.82 installed:

    cargo install hotl

## The agent — `hotl`

Point it at a model (`HOTL_MODEL` is always `provider/model`):

    # Anthropic:
    export HOTL_MODEL=anthropic/claude-opus-4-8
    export ANTHROPIC_API_KEY=sk-ant-…

    # — or — any OpenAI-compatible endpoint, hosted or local:
    export HOTL_MODEL=openai/gpt-5
    export OPENAI_API_KEY=sk-…

    # — or — local Ollama (nothing leaves your machine):
    export HOTL_MODEL=openai/llama3.1
    export HOTL_OPENAI_BASE_URL=http://localhost:11434/v1

Check the setup, then run it:

    hotl doctor    # provider, sandbox floor, config, sessions — all should read ok
    hotl           # interactive console TUI
    hotl -p "fix the typo in main.rs"   # headless one-shot

**The safety floor never turns off.** `bash` executes under a kernel sandbox
(Seatbelt on macOS, Landlock on Linux) confining writes to the working
directory. Writes to execute-later paths — git hooks, shell rc, Makefiles,
agent-instruction files — always stop and ask, in every mode. By default the
agent otherwise runs uninterrupted; prefer a y/n prompt on every mutating
call? Set `[permissions] mode = "ask"`. Need prompting guaranteed? A build
with `--features security-enforced` cannot disable it by any config. What
the sandbox does and does not cover is written down honestly in
[SECURITY.md](https://github.com/nrakochy/hotl/blob/master/docs/SECURITY.md).

**Nothing is ever lost.** Every session is an append-only log that nothing
rewrites. `hotl resume` continues an earlier session, `hotl undo` reverses
the agent's file changes (git snapshots around every mutating step), and
context compaction adds a summary on top instead of destroying history.

Also aboard: MCP client for external tools, `hotl acp` to embed in
ACP-speaking editors, `hotl bg` to background a session and re-attach later.
Full walkthrough: [quickstart](https://nrakochy.github.io/hotl/quickstart/).

## The dashboard — `hotl watch`

Inside tmux, open a pane (e.g. `Ctrl-b %`) and run `hotl watch`. It discovers
AI-agent processes (default: `claude`, `codex`, `hotl`), lists them with a
live status glyph — an animated braille snake while working · `!` blocked
(needs your input) · `√` idle · `·` unknown — refreshing about once a second,
and plays an audible ping when an agent transitions into blocked.

Keys:

- `j` / `k` (or ↓ / ↑) — move the selection · `gg` / `G` — top / bottom
- `enter` (or `gd`) — jump focus to the selected agent's pane
- `Ctrl-h` / `Ctrl-j` / `Ctrl-k` / `Ctrl-l` — switch to the neighboring tmux pane
- `r` — refresh now · `q` or `Ctrl-c` — quit

(`Ctrl`/arrow keys work regardless of `vim_mode`; the letter bindings require
`[settings] vim_mode = true`, the default for `watch`. The agent console's
input editor is a separate key, `[behavior] vim_mode`, which defaults **off**.)

### tmux + vim-tmux-navigator

`hotl watch` handles `Ctrl-h/j/k/l` itself by switching tmux panes, so it
works out of the box. If you also use
[vim-tmux-navigator](https://github.com/christoomey/vim-tmux-navigator),
extend its `is_vim` check so tmux forwards those keys **into** `hotl watch`
instead of stepping around it. Match the full argv (`ps -o args=`), **not** a
bare `hotl` in `vim_pattern`: a name match would swallow your navigation keys
whenever an agent-console pane (bare `hotl`) is focused. In
`~/.config/tmux/tmux.conf`:

    vim_pattern='(\S+/)?g?\.?(view|l?n?vim?x?|fzf)(diff)?(-wrapped)?'
    hotl_watch_pattern='(\S+/)?hotl watch( |$)'
    is_vim="ps -o state= -o comm= -t '#{pane_tty}' \
        | grep -iqE '^[^TXZ ]+ +${vim_pattern}$' \
        || ps -o state= -o args= -t '#{pane_tty}' \
        | grep -iqE '^[^TXZ ]+ +${hotl_watch_pattern}'"
    bind-key -n 'C-h' if-shell "$is_vim" 'send-keys C-h' 'select-pane -L'
    bind-key -n 'C-j' if-shell "$is_vim" 'send-keys C-j' 'select-pane -D'
    bind-key -n 'C-k' if-shell "$is_vim" 'send-keys C-k' 'select-pane -U'
    bind-key -n 'C-l' if-shell "$is_vim" 'send-keys C-l' 'select-pane -R'

Reload with `tmux source-file ~/.config/tmux/tmux.conf`.

## Config

Optional `~/.config/hotl/config.toml` (absent → sensible defaults):

    [settings]
    ping_on_blocked = true         # watch: audible ping when an agent needs input
    poll_interval_ms = 1000        # watch: scan cadence
    agents = ["claude", "codex", "hotl"]   # watch: process names counted as agents
    vim_mode = true                # watch: vim letter keys; false = arrows only

    [behavior]
    vim_mode = false               # agent console: true = modal vim input editor

    [permissions]
    mode = "auto"                  # agent: "ask" = y/n on every mutating call

    [settings.theme]
    preset  = "tokyo-night"        # themes both the agent console and watch
    blocked = "#ff0000"            # optional: override any slot on top

Built-in presets: `default`, `tokyo-night`, `catppuccin`, `gruvbox`, `nord`,
`dracula`. Overridable slots: `active`, `blocked`, `idle`, `ink`, `muted`,
`faint`, `accent`, `band`. An unknown preset name falls back to `default`.
Full reference: [configuration](https://nrakochy.github.io/hotl/configuration/).

## Requirements

- macOS or Linux; `hotl watch` additionally needs
  [tmux](https://github.com/tmux/tmux) on your `PATH` (run it from inside a
  tmux session) and `ps` (standard on both).
- The workspace's library crates (`hotl-engine`, `hotl-tools`, …) are
  internal components published in lockstep with this binary — no semver
  promise attaches to their APIs; pin exact or don't depend.

## License

hotl is dual-licensed:

1. **Open source:** GNU Affero General Public License v3.0 or later
   (AGPL-3.0-or-later).
2. **Commercial:** commercial licenses are available for organizations that
   cannot comply with AGPL. Contact nick.rakochy@gmail.com for details.
