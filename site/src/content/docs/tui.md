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
hotl --fork-from [id] # NEW session seeded with that one's history (--keep-turns N for a prefix)
```

Resume continues a conversation; fork branches a new one from it, leaving the
original free to keep running. See [sessions.md](../sessions/) for the full
semantics and for phase pipelines.

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

### Scrolling

| Key | Effect |
|---|---|
| `PageUp` / `PageDown` | Scroll the transcript a page (ten items) |
| `Ctrl-Home` / `Ctrl-End` | Jump to the top / back to following the newest |
| mouse wheel | Scroll three items a notch |
| `j` / `k` | One item, in vim Normal mode with the input empty |

`Home` and `End` on their own are line motions in the input, which is why the
document-level jumps are the `Ctrl` pair.

### Selecting and copying

Drag with the left mouse button and the region highlights; let go and it is on
your clipboard. The hint row confirms with `copied N lines`, and the next key
you press clears both the highlight and the notice.

What you see is what you get. Dragging a transcript line from the left edge
copies the prose without the gutter pad or the role glyph, so pasting a reply
elsewhere gives you the text and nothing else. Start the drag partway into a
line and you get exactly the span you dragged. The input box and hint row have
no spine and copy verbatim.

Selection is a region of the *screen*, not of the conversation, so it cannot
reach above the top of the window — scroll first, then drag.

The copy travels as an [OSC 52][osc52] escape, which needs no helper process
and works over SSH. Two caveats worth knowing:

- Inside tmux it needs `set -g set-clipboard on`.
- A few terminals disable OSC 52 by default as a security measure. There is no
  reply to check, so a refused copy is silent.

Mouse capture is what makes the wheel and the drag work, and it costs you the
terminal's own drag-select. Hold `Shift` to bypass capture on most emulators
and get the real thing back for one drag. To turn things off for good:

| Setting | Effect |
|---|---|
| `[behavior] copy_on_select = false` | Drag does nothing; the wheel still scrolls |
| `[behavior] mouse = false` | No capture at all — the terminal owns the mouse again |
| `HOTL_MOUSE=0` | Same as above, per-run; overrides the config key |

[osc52]: https://invisible-island.net/xterm/ctlseqs/ctlseqs.html#h3-Operating-System-Commands

### Pasting

Multi-line paste works: the terminal hands the whole payload over at once. It
does not submit — press `Enter` when you are ready. (Before bracketed paste, a
ten-line paste arrived as ten `Enter` presses and fired ten turns.)

A paste of three or more lines compacts to a `[Pasted text #1 +N lines]` token
instead of flooding the input box; the full text is held aside and reaches the
model verbatim when you submit. The transcript keeps the token — what you see
is what you typed, not a wall of paste.

Drag an image file onto the terminal and the pasted path becomes an
`[Image #1]` token the same way. On submit, hotl reads the file and sends it
to the model as a real image (png, jpg/jpeg, gif, webp — case-insensitive;
5MB per image, 8 images per prompt, 16MB decoded total per prompt). The
console enforces those same caps itself before sending, so a path that no
longer resolves, an image over a cap, an empty file, or a prompt over the
total all degrade to an inline note on that one attachment — never an error,
and the rest of the prompt still sends. Models whose catalog entry cannot
take images get the text with a one-line omission note instead.

Past roughly 24MB of base64 (~18MB of image data) alive in the session's
live context, hotl folds older history to make room.

Tokens are ordinary text, and `Backspace` only swallows one whole while a
live attachment backs it — the same bracketed text typed by hand deletes one
character, like anywhere else. Editing inside a real token turns it back
into literal text (its held content is dropped at submit). Recall (`↑`/`↓`)
and prompt history both replay the expanded text — the path, not
`[Image #1]` — so a recalled entry is self-contained.

### Model thinking

When the provider returns reasoning, it renders dimmed under a faint spine,
collapsed to the first three lines with a `[+N lines · ctrl-t]` trailer.
`Ctrl-T` toggles the full text. `HOTL_THINKING=0` turns extended thinking off
at the engine, which is what you want if you are not reading it — it is billed
either way.

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
| `/plan [on\|off]` | Toggle plan mode: `write`/`edit` always ask, everything else follows the mode (see [permissions-and-sandbox.md](../permissions-and-sandbox/)). Bare `/plan` toggles; `on`/`off` are for scripted input. |
| `/mode <ask\|bypass\|dontask>` | Switch to that permission mode. An unknown name prints usage and changes nothing; `/mode plan` points you at `/plan`. |
| `/effort [level]` | Set the reasoning depth: `low` \| `medium` \| `high` \| `xhigh` \| `max`, or `default` to hand it back to the provider. Bare `/effort` reports the current rung rather than cycling — five rungs are not a toggle. Recorded durably, so `hotl resume` keeps it. See [configuration.md](../configuration/#reasoning-effort-provider-effort). |
| `/reload` | Re-read `config.toml` without losing the session (see [Reloading config](#reloading-config)). |
| `/help` | Open the key overlay. `?` only works from an empty input; this works whatever you have typed. |
| `/status` | What this session is running: name, model, permission mode and plan state, reasoning effort, context window, todo count. |
| `/context` | What is *filling* the window, by source (see [The context report](#the-context-report)). Safe to run mid-turn. |
| `/cost` | Session token totals and, when the provider reports one, cost. |
| `/clear` | Clear the **transcript view**. The session log and the model's context are untouched. |
| `/quit` | Leave the console (the session log is already on disk). |
| `/<skill> [args]` | Load one of your skills by name and follow it, with the rest of the line passed as arguments — any attached images ride along too. |

Typing `/` opens a menu of every command and skill, filtered as you keep
typing: `↑` / `↓` pick, `Tab` completes the highlighted name, `Enter` runs
it, `Esc` dismisses. The descriptions beside each skill come from its
roster entry and cost nothing by default: the always-sent tool description
omits them, and the model only sees one if it explicitly queries the skill
tool for it.

### The context report

`/context` answers "what is filling my context window, and how much room is
left?" It prints into the scrollback rather than taking over the screen, and
it is a **read**: it appends nothing to the session and starts no turn, so
unlike `/reload` it is safe to run while a turn is running.

```
· Context Usage — claude-opus-5 · 1.0M window
·
·   ▇▇▇▇▇▇▇▇▇▇▇▇▇▇▇▇▇▇▇▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁
·
·   reported     241.3k / 1.0M  (24%)  last turn
·   estimated    262.8k / 1.0M  (26%)  rows below
·
·   ▣ system prompt    5.3k   (0.5%)
·   ▣ tool schemas    14.4k   (1.4%)
·   ◆ memory           1.8k   (0.2%)
·   ▪ messages       102.4k  (10.2%)
·   ▪ tool results   138.8k  (13.9%)
·   ▫ free space     737.2k  (73.7%)
```

**Two totals, on purpose.** `reported` is the provider's exact figure for the
last turn — accurate, but a single number with no breakdown. `estimated` is
hotl's own per-item accounting, the same ruler that decides when to fold
history, and it deliberately overcounts so that estimation error causes an
early fold rather than an overflow. Showing both means the gap between them is
visible: that gap *is* the overcount margin. Free space is computed from
whichever total is larger, so the report may understate your remaining room
but never overstates it. Before the first turn there is no `reported` line.

**The rows** cover everything in the window exactly once — every item lands in
one row, and the rows sum to the estimate. Zero rows are hidden.

| Group | Rows |
|---|---|
| `▣` stable prefix | system prompt, tool schemas, skills roster, agents roster |
| `◆` session preamble | project instructions, memory, todos |
| `▪` conversation | messages, tool results, folded history, harness injections, images |
| `▫` free space | turns red below 15% |

The shape carries the grouping and the color separates rows within a group, so
the table still reads on a monochrome terminal. `skills roster` and `agents
roster` get their own rows because the `skill` and `spawn` tools carry their
whole roster inside the schema hotl sends every turn — they are often the
largest single line in the prefix. `harness injections` is where system
reminders, doom-loop nudges, retry feedback and sub-agent results land;
a large sub-agent result is the single biggest surprise a session can hit,
which is why it is not folded into `messages`.

The meter is the same numbers as one row, colored per row in table order. On a
terminal narrower than about 24 content columns it is dropped rather than
rendered as a misleading two-cell bar.

`/context` is TUI-only. It describes a live session's assembled context, and
`hotl -p` builds one, runs it and exits — there is no session left to measure.
Under `hotl acp` the same thing is a `session/context` call: the reply is a
thin `{"ok": true}` ack and the report itself arrives as a `context_report`
`session/update`, so every attached surface sees it.

### Reloading config

`/reload` picks up `config.toml` edits without ending the session. It re-reads
the file, rebuilds the engine from it, and re-opens your session onto the new
one — the transcript stays on screen, and the model keeps its context, todos,
name and permission mode. Everything the engine owns reloads: `[provider]
model`, `[[allow]]` rules, `[[mcp]]` servers, `[[hook]]`, the skill roster,
`system-prompt.md`, `[context]`. So do the console's own settings: theme,
density, `vim_mode`, `mouse`, `copy_on_select`.

It reports what it got — `config reloaded — model … · mode … · N skill(s)` —
and any warnings the reload produced. If the file does not parse, or the
provider it names cannot be selected, **nothing changes**: the running engine
keeps serving and the notice says the previous config is still live.

Two things it deliberately does not do:

- **It refuses while a turn is running.** Rebuilding replaces the session, and
  the reply in flight would go with it. Let the turn finish, or press `Esc`
  twice to take control back, then reload. Abandoning work stays your call.
- **It does not override a posture you chose.** `/mode` and `/plan` are both
  logged with the session, so a session running `dontask` because you asked for
  `dontask` still runs it after a reload, and a session you put in plan mode
  stays there — the same inheritance `hotl resume` gives you. A session that
  never set either has none of its own, so it picks up the reloaded
  `[permissions]` values. Either way the notice and the badge name what you
  ended up with; the badge is always what the engine is actually enforcing.

Some settings are fixed for the life of the process and a reload will not move
them. Restart hotl to change these:

| Setting | Why |
|---|---|
| `[sandbox]` extras | Installed once, before the first tool can run |
| `[network]` egress | Same — and widening egress mid-process would be a hole, not a feature |
| `[behavior] sandbox` | The confinement probe has already run |
| `[concurrency] worker_threads` / `blocking_threads` | Resolved before the async runtime exists |
| `[history]` | The recall ring is loaded at startup; re-reading it would drop prompts submitted since |

Under `hotl acp` the same thing is a `session/reload_config` call, so an editor
or orchestrator embedding hotl gets it too.

### The permission-mode badge

The strip **always** shows the session's permission mode, next to the session
name. Read it before you walk away from a run.

The mode it shows is the one the engine is actually enforcing: it arrives from
the server when the session opens and again whenever it changes, so it is never
a guess. If a build coerces your request — a `security-enforced` build forces
`bypass` back to `ask` — the badge shows what you got, not what you asked for.
`bypass` and `dontask` wear the blocked color, because in those modes nobody is
being consulted before a tool runs.

Plan mode is the other half of the posture, so the badge carries both: with it
on the chip reads `plan · bypass` and takes the accent color, because plan is
something you deliberately chose rather than a default you inherited.

**`hotl setup` writes `mode = "bypass"`.** If you took the setup default, your
sessions approve mutating tool calls without asking. `/mode ask` switches back,
and `/plan` narrows just the file edits without giving up the shell.

Switching either axis never starts a turn — it's session bookkeeping, and both
are durable (`hotl resume` restores the posture you left the session in).

Built-ins are matched first, so a skill named `rename` cannot shadow
`/rename`. Any other name is looked up in your skill roster — bare
(`/brainstorming`) or qualified (`/superpowers:brainstorming`). A name that
matches nothing prints an unknown-command notice and costs you no turn.

`/<skill>` exists because the agent is shown a compact index rather than every
skill's description ([skills.md](../skills/)); when it doesn't
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
| `/` then `↑ ↓` | Pick from the command menu; `Tab` completes, `Enter` runs it, `Esc` dismisses |
| `Enter` | Submit (either mode) |

## The `$EDITOR` escape hatch

`Ctrl-E` (any mode) or `:e` (normal mode) suspends the console and opens the current input in `$EDITOR` (falls back to `vi`). Save and quit to bring the text back into the input; quit without saving to leave it unchanged.
