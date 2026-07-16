# hotl — a human-on-the-loop terminal agent dashboard

[![crates.io](https://img.shields.io/crates/v/hotl.svg)](https://crates.io/crates/hotl)

I was inspired by [herdr](https://herdr.dev/), but I wanted to retain those sweet sweet navigation bindings from [neovim-tmux navigation](https://github.com/alexghergh/nvim-tmux-navigation).

So, byo-keybindings. Run this tui in a pane of your own. It discovers AI-agent processes across your
ze multiplexer, shows their live status, gives an (optional) audible ping when an agent is waiting on your input.

![hotl demo](https://raw.githubusercontent.com/nrakochy/hotl/master/docs/hotl.gif)

## Quick start

**Requirements:** [tmux](https://github.com/tmux/tmux) on your `PATH` (run `hotl` from inside a tmux session) and `ps` (standard on macOS/Linux).

Install a prebuilt binary — no toolchain needed:

**macOS / Linux**

    curl --proto '=https' --tlsv1.2 -LsSf https://github.com/nrakochy/hotl/releases/latest/download/hotl-installer.sh | sh

**Windows (PowerShell)**

    powershell -c "irm https://github.com/nrakochy/hotl/releases/latest/download/hotl-installer.ps1 | iex"

Or grab a `.tar.xz` / `.zip` for your platform directly from the
[latest release](https://github.com/nrakochy/hotl/releases/latest).

With Rust installed, you can instead build from crates.io:

    cargo install hotl

Then, from inside tmux, open a pane and run it:

    hotl

Keys: `j`/`k` (or ↓/↑) move · `enter` jump to the selected agent · `r` refresh
· `q` or `Ctrl-c` quit · `Ctrl-h`/`j`/`k`/`l` switch tmux panes.

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

    cargo install --path crates/hotl   # installs to ~/.cargo/bin/hotl
    hotl

If `~/.cargo/bin` is not on your PATH, either add it, or symlink the built
binary into a directory that already is:

    ln -sf ~/.cargo/bin/hotl ~/.nix-profile/bin/hotl

## Usage

Inside tmux, create a pane (e.g. `Ctrl-b %` for a vertical split), then run
`hotl` in it. It lists the AI agents it finds, with each agent's live status,
and refreshes about once a second.

Keys:

- `j` / `k` (or ↓ / ↑) — move the selection
- `gg` / `G` — jump to the top / bottom of the list
- `enter` (or `gd`) — jump focus to the selected agent's pane (`hotl` stays open)
- `Ctrl-h` / `Ctrl-j` / `Ctrl-k` / `Ctrl-l` — switch to the neighboring tmux pane
- `r` — refresh now
- `q` or `Ctrl-c` — quit

(`Ctrl`/arrow keys work regardless of `vim_mode`; the `j`/`k`/`gg`/`G`/`gd`
letter bindings require `vim_mode = true`, the default.)

Detected agents (v1): `claude`, `codex`, `pi`.

Each agent shows a live status glyph: an animated braille snake while working ·
`!` blocked (needs your input) · `√` idle · `·` unknown. When an agent
transitions **into** blocked, `hotl` plays an audible ping so you know it's
waiting on you.

## tmux + vim-tmux-navigator

`hotl` handles `Ctrl-h/j/k/l` itself by switching tmux panes, so it works out
of the box. If you also use
[vim-tmux-navigator](https://github.com/christoomey/vim-tmux-navigator), add
`hotl` to its `vim_pattern` so tmux forwards those keys **into** `hotl` (which
does the pane switch) instead of stepping around it — this keeps movement
seamless whether the focused pane is Vim, another navigator-aware app, or
`hotl`. In `~/.config/tmux/tmux.conf`:

    # add `hotl` to the alternation (…|fzf|hotl)
    vim_pattern='(\S+/)?g?\.?(view|l?n?vim?x?|fzf|hotl)(diff)?(-wrapped)?'
    is_vim="ps -o state= -o comm= -t '#{pane_tty}' \
        | grep -iqE '^[^TXZ ]+ +${vim_pattern}$'"
    bind-key -n 'C-h' if-shell "$is_vim" 'send-keys C-h' 'select-pane -L'
    bind-key -n 'C-j' if-shell "$is_vim" 'send-keys C-j' 'select-pane -D'
    bind-key -n 'C-k' if-shell "$is_vim" 'send-keys C-k' 'select-pane -U'
    bind-key -n 'C-l' if-shell "$is_vim" 'send-keys C-l' 'select-pane -R'

Reload with `tmux source-file ~/.config/tmux/tmux.conf`.

## Config

Optional `~/.config/hotl/config.toml` (absent → sensible defaults):

    [settings]
    ping_on_blocked = true         # audible ping when an agent needs input
    poll_interval_ms = 1000        # scan cadence
    agents = ["claude", "codex", "pi"]   # process names counted as agents
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
