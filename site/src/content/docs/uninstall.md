---
title: 'Uninstalling hotl'
description: Remove the hotl binary, the zsh plugin, and optionally config and session data.
---

Remove the `hotl` agent and its data. `destructive:` the data steps delete session history and undo snapshots — read before running.

## 1. Remove the binary

- Installed with `cargo install`: `cargo uninstall hotl`.
- Installed from a source build: delete the binary you built (`~/sources/hotl/target/release/hotl`) or the copy you put on your `PATH`.
- Installed via the shell installer: remove the binary from the install dir it named (typically `~/.local/bin/hotl` or `~/.cargo/bin/hotl`).
- Installed with `nix profile install`: `nix profile remove hotl` (if the name is ambiguous, `nix profile list` shows the entry to remove).

## 2. Remove the zsh plugin (if you added it)

Delete the `eval "$(hotl init zsh)"` line from your `~/.zshrc`. Details: [shell.md](../shell/).

## 3. Remove config (optional)

`destructive:` this deletes your allow-rules, memory, MCP/hook config, and system prompt.

```
rm -rf ~/.config/hotl
```

## 4. Remove data — sessions and undo snapshots (optional)

`destructive:` this deletes all session logs and the shadow-git snapshots backing `hotl undo`. Do this only if you don't need session history or the ability to undo past edits.

```
rm -rf ~/.local/share/hotl
```

(If you set `XDG_CONFIG_HOME` or `XDG_DATA_HOME`, the config and data live under those instead of `~/.config` and `~/.local/share`.)

## What hotl never leaves behind

hotl writes nothing outside those three locations, its own binary (only when you run [`hotl update`](../updating/)), and your explicit edits to your own files. It installs no daemon, no launch agent, no cron entry, and sends no telemetry — there is no background process to stop.
