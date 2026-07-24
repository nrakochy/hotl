---
title: 'The console: hotl'
description: "Drive the hotl agent from a full-screen terminal console: streaming transcript, activity strip, modal permission asks, vim-style input."
---

Drive the agent from a full-screen terminal console — streaming transcript, a loop-motif activity strip, modal permission asks, and a vim-style input editor. Assumes a working agent ([quickstart.md](../quickstart/)).

## Launch

```
hotl                  # new session
hotl <id-prefix>      # continue a specific earlier session
hotl --resume         # pick from recent sessions (numbered list, newest first)
hotl resume [id]      # same thing, spelled as a subcommand
```

Bare `hotl` **is** the console (the `tui` subcommand and the old line-based REPL are gone). It needs a real terminal: piped stdin/stdout exits with a pointer at `hotl -p "prompt"`, the headless path for scripts and CI.

The console is a pure ACP client of the same engine `-p` headless uses — same permission gate, same session logs, same `hotl undo` afterwards.

## Theming

The console wears the same palette as `hotl watch`, from the same `[settings.theme]` table in `~/.config/hotl/config.toml`:

```toml
[settings.theme]
preset = "warm"       # tokyo-night (the default) | warm | catppuccin | gruvbox | nord | dracula
accent = "#88c0d0"    # optional per-slot #rrggbb overrides
```

Eight slots: `active` (working), `blocked` (waiting on you), `idle` (settled), `ink`/`muted`/`faint` (text tiers), `accent`, and `band` (the strip background). An unknown preset or invalid color falls back with a one-line warning — the console always launches.

### Making it warmer

Two knobs, one in each table:

- **`preset = "warm"`** — a deliberately low-blue palette (paper-white ink, amber accent, terracotta) instead of the cool blue-grey default.
- **`[settings] density = "comfortable"`** (the default) or `"spacious"` — more room between turns and a wider gutter. `"compact"` is the old edge-to-edge look. See [configuration.md](../configuration/).

**Font size and family are your terminal's job, not hotl's** — like `vim` or `htop`, the app draws onto whatever grid the emulator gives it and can't set the point size or typeface. To go bigger or warmer there, change it in your terminal: Ghostty (`font-size`, `font-family` in `~/.config/ghostty/config`), iTerm2 (Preferences → Profiles → Text), Kitty (`font_size`, `font_family` in `kitty.conf`), Alacritty (`font` in `alacritty.toml`). A warm monospace face — Berkeley Mono, Comic Code, IBM Plex Mono — pairs well with the `warm` preset.

## The screen

Top to bottom:

1. **Transcript** — every turn carries a marker in the left gutter, so you can see the shape of the conversation by scanning straight down: `❯` your prompts, `●` the assistant (with a `│` bar down a long answer), `✓ ✗ ⛔` tool cards (`✓ bash  cargo test · 2s`), `⤷` steers, `·` dim notices (retries, fallbacks, compaction). Inside an assistant answer, headings, bullets, and code get light styling so a long reply is scannable. With the input empty, `j`/`k` scroll it; it snaps back to following the bottom on your next prompt.
2. **Activity strip** — one line that tells you what the turn is doing, animated as a loop drawing itself:

   | You see | It means |
   |---|---|
   | `· ─ ·` resting | idle — your move (after a turn it also shows real token usage) |
   | the loop drawing itself, then turning · "thinking" | the model is reasoning |
   | the loop turning · "writing · ~N tok" | the reply is streaming (`~N tok` is a chars/4 approximation; exact usage arrives at the end of the turn) |
   | a dot orbiting the loop · tool name | a tool is running |
   | **the loop halted with a gap** · "waiting on you" | a permission ask — the gap is you; nothing moves until you answer |
   | the loop coiling up · "folding history…" | context compaction |

   When the model has an active `todo_write` checklist, the strip also
   carries a compact `done/total` count — and, while one item is
   `in_progress`, that item's own label (e.g. `2/5 wiring the gate`) — so you
   can see plan progress at a glance without opening the transcript. An
   empty or never-started list shows nothing extra.

3. **Input** — bordered editor, title shows `-- INSERT --` / `-- NORMAL --`.
4. **Hint row** — the keys that matter right now.

There is **no bell, ever** — salience is visual only. `hotl watch` is the thing that pings across panes; the console itself is silent.

## Prompting and steering

Type and press `Enter` to prompt. **Typing while a turn runs is steering**: submit and it becomes a pinned `⤷` chip — dim while queued, and the engine folds it in at the next step. `Shift`/`Alt`+`Enter` inserts a newline.

## History recall

Your submitted prompts are remembered across sessions (shell-style), stored under `[history]` in `config.toml` ([configuration.md](../configuration/)).

- **`↑` / `↓`** — walk previous prompts. Recall triggers only at the buffer's edge: `↑` from the **first** line steps to an older prompt, `↓` from the **last** line steps to a newer one; anywhere else the arrows just move the cursor between lines. What's on the line when you start walking becomes a **prefix filter** — type `git ` then `↑` and you only cycle prompts that began with `git `. An empty line walks everything. Your in-progress text is saved and comes back when you press `↓` past the newest match; editing a recalled prompt keeps it and drops you out of recall.
- **`Ctrl-R`** — reverse-incremental search. The input line becomes `(reverse-i-search)'query': match`; each character narrows to the most recent prompt containing it, and pressing `Ctrl-R` again steps to the next older match. `Enter` drops the match into the input to edit or send; `Esc` cancels and restores what you had.

Only prompts that start a turn are saved to disk — steers and `/slash` commands aren't, though the running session still recalls everything you typed. Consecutive duplicates are collapsed, and the file is size-bounded (see `[history]`). Vim `k`/`j` remain pure cursor/scroll motion — recall is on the arrows.

## Slash commands

A line starting with `/` is handled locally and never becomes a prompt on its
own.

| Command | Effect |
|---|---|
| `/rename <name>` | Rename the session (1–64 chars); the badge and terminal title follow. |
| `/plan` | Switch to plan mode: read-only until you approve a plan (see [permissions-and-sandbox.md](../permissions-and-sandbox/)). |
| `/mode <ask\|auto\|plan\|dontask>` | Switch to that permission mode. An unknown name prints usage and changes nothing. |
| `/<skill> [args]` | Load one of your skills by name and follow it, with the rest of the line passed as arguments. |

A non-default mode shows as a badge on the strip next to the session name.
Switching mode never starts a turn — it's session bookkeeping, and it's
durable (`hotl resume` restores whichever mode you left the session in).

Built-ins are matched first, so a skill named `rename` cannot shadow
`/rename`. Any other name is looked up in your skill roster — bare
(`/brainstorming`) or qualified (`/superpowers:brainstorming`). A name that
matches nothing prints an unknown-command notice and costs you no turn.

`/<skill>` exists because the agent is shown a compact index rather than every
skill's description ([configuration.md](../configuration/)); when it doesn't
reach for the skill you had in mind, this is how you hand it over directly.

## Permission asks

An ask freezes the loop (the gap glyph) and opens a modal with the tool summary — and a loud `⚠` line when a protected path is involved. `y` allows. `n` starts a deny: type an optional reason, `Enter` sends it (the reason goes to the model verbatim; `Esc` backs out of the deny).

## Questions

The agent can also ask a **structured question** (`ask_user`) — a header, a prompt, and 2–4 numbered options — when it hits a genuine ambiguity instead of guessing. It freezes the loop the same way a permission ask does (same gap glyph, same "waiting on you" strip), but **it is not a permission ask**: answering it never authorizes any tool, it only supplies text the model reads on its next turn.

Press a digit (`1`–`4`) to pick that option — it submits immediately, no confirm step. To answer with something not listed, just start typing: the modal switches to free text, `Enter` submits it, `Esc` clears it back to the picker.

In headless (`-p`) or JSON mode there is no one to ask, so the question resolves immediately to a documented "no human available" answer and the model proceeds on its own judgment — it never hangs a scripted run.

## Interrupting

- `Esc` (with the input empty) — interrupt the running turn; press again to insist.
- `Ctrl-C` — cancel the turn while one runs; quit from idle.

## Vim keys

**Off by default** — the input editor is a plain insert-mode field unless you ask for more. Opt in with `vim_mode = true` under `[behavior]` in `config.toml` ([configuration.md](../configuration/)). Note that turning it on gives `Esc` its Normal-mode meaning, so interrupting a turn from an empty input moves to `Ctrl-C`.

(`hotl watch`'s own `[settings] vim_mode` is a separate key and stays **on**: there the vim letters are additive over a read-only list, and arrows/`enter`/`q`/`r` work either way.)

| Keys | Do |
|---|---|
| `Esc` / `i a I A o O` | Normal mode / back to Insert (with the usual cursor placement) |
| `h l 0 $ w b e` | Motions, with counts (`3w`) |
| `d c y` + motion | Delete / change / yank; `dd cc yy` for the whole line |
| `x p u` | Delete char · paste · undo (one level) |
| `j k` | Scroll the transcript when the input is empty; move lines otherwise |
| `↑ ↓` | Recall prompt history at the buffer's edges (see [History recall](#history-recall)); `Ctrl-R` searches it |
| `Enter` | Submit (either mode) |

## The `$EDITOR` escape hatch

`Ctrl-E` (any mode) or `:e` (normal mode) suspends the console and opens the current input in `$EDITOR` (falls back to `vi`). Save and quit to bring the text back into the input; quit without saving to leave it unchanged.
