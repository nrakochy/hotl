# Changelog

Notable changes to hotl. Pre-1.0, breaking changes land at every 0.x minor;
the internal library crates version in lockstep with the binary and carry no
semver promise of their own.

## [Unreleased]

### Added

- Named sessions: start one with `-n/--name` (TUI, `hotl bg`, headless `-p`),
  rename mid-session with `/rename <name>` — the TUI's first slash command.
  The name shows as a badge above the input, in the terminal tab title, and
  in the resume picker.
- `hotl -r [arg]` resume flag (same path as `hotl resume`): bare lists
  sessions; the arg accepts the picker number, an id-prefix, or a name.

## [0.2.0] - 2026-07-21

The execute harness ships: hotl is now a human-on-the-loop terminal AI agent,
with the original dashboard aboard as a subcommand.

### Breaking

- Bare `hotl` is now the **agent**; the tmux dashboard moved to `hotl watch`.
- Crate identity swap on crates.io: `hotl-types` and `hotl-tui` now hold the
  harness's conversation types and agent console. The watch-era code they
  shipped through 0.1.5 lives on as `hotl-watch-types` and `hotl-watch-tui`.

### Added

- Agent harness: steering console TUI and `-p` headless mode, against any
  Anthropic or OpenAI-compatible model (`HOTL_MODEL=provider/model`).
- Permission gate on every mutating or executing tool call — `auto` (default)
  or `ask` mode — under a kernel sandbox floor (Seatbelt on macOS, Landlock
  on Linux) confining `bash` writes to the working directory. Writes to
  execute-later paths (git hooks, shell rc, Makefiles, agent-instruction
  files) always ask, in every mode. `--features security-enforced` builds
  make prompting impossible to disable by config.
- Append-only session log with `hotl resume`, `hotl undo` (git snapshots
  around every mutating step), and non-destructive context compaction.
- MCP client (stdio), ACP server (`hotl acp`), background sessions
  (`hotl bg` / attach), `hotl doctor` setup check.
- Theme presets shared by both surfaces (`tokyo-night` default);
  `[settings.theme]` in `~/.config/hotl/config.toml`.
- Fifteen internal library crates first published in lockstep with the
  binary (`hotl-engine`, `hotl-tools`, `hotl-provider*`, `hotl-watch-*`, …).

## [0.1.5] and earlier

Watch-only releases: bare `hotl` was the tmux dashboard that is now
`hotl watch`.
