# The console: `hotl tui`

**Mode: how-to.** Drive the agent from a full-screen terminal console — streaming transcript, a loop-motif activity strip, modal permission asks, and a vim-style input editor. Assumes a working agent ([quickstart.md](quickstart.md)).

## Launch

```
hotl tui                  # new session
hotl tui <id-prefix>      # continue a specific earlier session
hotl tui --resume         # pick from recent sessions (numbered list, newest first)
```

The console is a pure ACP client of the same engine the REPL uses — same permission gate, same session logs, same `hotl undo` afterwards.

## The screen

Top to bottom:

1. **Transcript** — your prompts (`❯ `), the streaming reply, tool cards `[✓ bash] cargo test · 2s`, and dim notices (retries, fallbacks, compaction). With the input empty, `j`/`k` scroll it; it snaps back to following the bottom on your next prompt.
2. **Activity strip** — one line that tells you what the turn is doing, animated as a loop drawing itself:

   | You see | It means |
   |---|---|
   | `· ─ ·` resting | idle — your move (after a turn it also shows real token usage) |
   | the loop drawing itself, then turning · "thinking" | the model is reasoning |
   | the loop turning · "writing · ~N tok" | the reply is streaming (`~N tok` is a chars/4 approximation; exact usage arrives at the end of the turn) |
   | a dot orbiting the loop · tool name | a tool is running |
   | **the loop halted with a gap** · "waiting on you" | a permission ask — the gap is you; nothing moves until you answer |
   | the loop coiling up · "folding history…" | context compaction |

3. **Input** — bordered editor, title shows `-- INSERT --` / `-- NORMAL --`.
4. **Hint row** — the keys that matter right now.

There is **no bell, ever** — salience is visual only. `hotl watch` is the thing that pings across panes; the console itself is silent.

## Prompting and steering

Type and press `Enter` to prompt. **Typing while a turn runs is steering**: submit and it becomes a pinned `⤷` chip — dim while queued, and the engine folds it in at the next step. `Shift`/`Alt`+`Enter` inserts a newline.

## Permission asks

An ask freezes the loop (the gap glyph) and opens a modal with the tool summary — and a loud `⚠` line when a protected path is involved. `y` allows. `n` starts a deny: type an optional reason, `Enter` sends it (the reason goes to the model verbatim; `Esc` backs out of the deny).

## Interrupting

- `Esc` (with the input empty) — interrupt the running turn; press again to insist.
- `Ctrl-C` — cancel the turn while one runs; quit from idle.

## Vim keys

On by default; `vim_mode = false` under `[behavior]` in `config.toml` pins plain insert-mode editing ([configuration.md](configuration.md)).

| Keys | Do |
|---|---|
| `Esc` / `i a I A o O` | Normal mode / back to Insert (with the usual cursor placement) |
| `h l 0 $ w b e` | Motions, with counts (`3w`) |
| `d c y` + motion | Delete / change / yank; `dd cc yy` for the whole line |
| `x p u` | Delete char · paste · undo (one level) |
| `j k` | Scroll the transcript when the input is empty; move lines otherwise |
| `Enter` | Submit (either mode) |

## The `$EDITOR` escape hatch

`Ctrl-E` (any mode) or `:e` (normal mode) suspends the console and opens the current input in `$EDITOR` (falls back to `vi`). Save and quit to bring the text back into the input; quit without saving to leave it unchanged.
