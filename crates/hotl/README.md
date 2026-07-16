# hotl

A terminal agent dashboard. Run it in a pane of your own; it discovers AI-agent
processes across your terminal multiplexer, shows their live status, and lets you
jump focus to an agent — grouped by session → window.

`hotl` never owns a terminal. The multiplexer owns your agents' panes; `hotl`
observes them from the outside and can switch your focus to one. Today it
observes **tmux**; the observation layer is surface-agnostic so other backends
(e.g. zellij) can be added without changing the rest of the tool.

## Run it locally

Build the optimized binary:

    cargo build --release

The binary is then at `target/release/hotl`. You have a few ways to run it.

### Option A — run by path (no install)

From inside a tmux session, open a pane and run the absolute path:

    ~/sources/hotl/target/release/hotl

### Option B — put it on your PATH

`cargo install` drops binaries in `~/.cargo/bin`. If that directory is on your
PATH (this machine's zsh config adds it), install and run by name:

    cargo install --path .        # installs to ~/.cargo/bin/hotl
    hotl

If `~/.cargo/bin` is not on your PATH, either add it, or symlink the built
binary into a directory that already is:

    ln -sf ~/.cargo/bin/hotl ~/.nix-profile/bin/hotl

## Usage

Inside tmux, create a pane (e.g. `Ctrl-b %` for a vertical split), then run
`hotl` in it. It lists the AI agents it finds, grouped by session → window, with
each agent's live status, and refreshes about once a second.

Keys:

- `j` / `k` (or ↓ / ↑) — move the selection
- `enter` — jump focus to the selected agent's pane (`hotl` stays open)
- `r` — refresh now
- `q` — quit

Detected agents (v1): `claude`, `codex`.

Each agent shows a live status glyph: `●` working · `!` blocked (needs your
input) · `√` idle · `·` unknown. When an agent transitions **into** blocked,
`hotl` plays an audible ping so you know it's waiting on you.

## Config

Optional `~/.config/hotl/config.toml` (absent → sensible defaults):

    [settings]
    ping_on_blocked = true         # audible ping when an agent needs input
    poll_interval_ms = 1000        # scan cadence
    agents = ["claude", "codex"]   # process names counted as agents
    vim_mode = true                # vim list keys (j/k/gg/G/gd); false = arrows only

    [settings.theme]
    preset  = "tokyo-night"        # base palette; omit → default
    blocked = "#ff0000"            # optional: override any slot on top

Built-in presets: `default`, `tokyo-night`, `catppuccin`, `gruvbox`, `nord`,
`dracula`. Overridable slots: `active`, `blocked`, `idle`, `ink`, `muted`,
`faint`, `accent`, `band`. An unknown preset name falls back to `default`.

A `[plugins]` section is reserved for future use (parsed but inert).

## Requirements

- A supported multiplexer on your PATH — today **tmux** (run `hotl` from inside
  a tmux session)
- `ps` (standard on macOS/Linux)

## Build

    cargo build --release
    ./target/release/hotl
