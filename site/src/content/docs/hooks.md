---
title: 'Hooks — running your own checks on tool calls'
description: "Run your own logic on hotl tool calls: post-edit diagnostics and hooks that block, rewrite, or clean up."
---

Make the `hotl` agent run *your* logic when it uses a tool — block a call, rewrite it, or clean up a result. Two mechanisms: post-edit **diagnostics** (run a check after edits) and **hooks** (intercept any tool call). Assumes a working agent ([quickstart.md](../quickstart/)).

## Post-edit diagnostics (the simple one)

Make the agent see your project's own check output right after it edits a file. In `~/.config/hotl/config.toml`:

```toml
[diagnostics]
rs = "cargo check -q --message-format=short"
py = "ruff check ."
```

After a successful `edit`/`write` to a `.rs` file, `cargo check` runs (under the sandbox floor, 30 s timeout) and up to 30 lines of its output are appended to the tool result — so the agent notices breakage in the same step it caused it. A clean, quiet check adds nothing.

## Hooks (intercept tool calls, prompts, and turn-end)

hotl deliberately supports six events — not Claude's ~35 — each with a concrete consumer:

| `event` | Fires when | Can do |
|---|---|---|
| `pre_tool` | before a tool call | allow, block, or rewrite it |
| `post_tool` | after a tool succeeds | replace the result |
| `user_prompt` | a prompt is admitted, before the turn samples | inject `additionalContext` |
| `notification` | the agent blocks on you, goes idle, or finishes | fire-and-forget (ring a bell, `tmux display-message`, push a phone notification) |
| `stop` | the model just finished replying with no tool calls | veto turn-end once, with a reason (bounded — see below) |
| `session_end` | the session actor shuts down | fire-and-forget cleanup |

In `~/.config/hotl/config.toml`:

```toml
[[hook]]
event = "pre_tool"
command = "/usr/local/bin/guard"
matcher = "bash,write"      # exact tool names, comma-separated; "*" or omitted = every tool
env = { LOG_LEVEL = "warn" } # optional extra env for the command (see identity env, below)

[[hook]]
event = "post_tool"
command = "/usr/local/bin/scrub"

[[hook]]
event = "user_prompt"
command = "/usr/local/bin/remind"

[[hook]]
event = "notification"
command = "/usr/local/bin/notify"

[[hook]]
event = "stop"
command = "/usr/local/bin/gate"

[[hook]]
event = "session_end"
command = "/usr/local/bin/cleanup"
```

`matcher` only applies to `pre_tool`/`post_tool` (the tool-scoped events); it's ignored elsewhere. When several hooks match the same event they run **concurrently**, and their results are folded **deterministically in the order they're listed in config.toml** — never by whichever finishes first — so a fast hook can never race a slower, more-restrictive one.

Your command receives the event as JSON on **stdin** and returns a decision as JSON on **stdout**.

Every stdin envelope carries the event **twice**: hotl's own lowercase `event` (unchanged, so an already-shipped `pre_tool`/`post_tool` hook keeps working), and `hookEventName`, Claude's own camelCase name for the same event (`PreToolUse`, `PostToolUse`, `UserPromptSubmit`, `Notification`, `Stop`, `SessionEnd`) — so a `~/.claude`-style hook script that keys on `hookEventName` (the only key it knows for the brand-new `user_prompt`/`notification`/`stop` events) can read hotl's envelope unmodified.

**pre_tool** — stdin `{"event":"pre_tool","hookEventName":"PreToolUse","tool":"bash","input":{...}}`, respond with one of:
```json
{"decision":"continue"}
{"decision":"deny","message":"why the model should not do this"}
{"decision":"rewrite","input":{ ...replacement args... }}
```
A `deny` becomes an error tool result carrying your message. A `rewrite` swaps the arguments and **re-enters the normal permission gate** — a hook cannot push a call past the y/N ask. With several matching hooks, `deny` beats `rewrite` beats `continue`.

**post_tool** — stdin `{"event":"post_tool","hookEventName":"PostToolUse","tool":"read","result":"<up to 2KB>"}`, respond with `{"result":"replacement"}` to change what the model sees, or anything else to leave it.

**user_prompt** — stdin `{"event":"user_prompt","hookEventName":"UserPromptSubmit","prompt":"..."}`, respond with:
```json
{"hookSpecificOutput":{"additionalContext":"remember: use pnpm, not npm"}}
```
(the same nested shape Claude Code uses, so an existing `additionalContext` hook script ports unmodified). The text becomes one reminder committed right after the prompt it answers — never a system-prompt edit, so the prefix cache stays stable. Several matching hooks' context is concatenated into that one reminder, in the order they're listed.

**notification** — stdin `{"event":"notification","hookEventName":"Notification","kind":"blocked"|"idle"|"done","detail":"..."}`. Fire-and-forget: your command's stdout is ignored, and a slow or hung notifier is spawned detached with its own timeout — it can never stall the agent. This is the seam behind `hotl watch`/desktop notifiers. `blocked` also fires from the structured `ask_user` question surface, not just the permission ask.

**stop** — stdin `{"event":"stop","hookEventName":"Stop","outcome":"the model's final reply text"}`, respond with:
```json
{"decision":"block","reason":"tests haven't been run yet"}
```
or `{"decision":"allow"}` (the default for anything else). A `block` injects your `reason` as a reminder and lets the model keep going — **bounded**: `stop` shares one small per-prompt budget with hotl's own todo-list nudge, so a hook that always blocks can never wedge a turn forever.

**session_end** — stdin `{"event":"session_end","hookEventName":"SessionEnd"}`. Runs to completion at actor shutdown (bounded by its own timeout) rather than fire-and-forget — the process waits for it, so it's guaranteed to actually run before `hotl` exits.

### Rules hooks live by

- Hook commands run **under the sandbox floor**, like `bash` — they're commands, not trusted-by-position.
- Each hook process draws a permit from the same concurrent-process budget `bash`/`grep` share (`[concurrency].subprocs`, default 8) — a burst of hooks (or a `notification` storm) can't fork-storm the host.
- The result a `post_tool` hook sees is **capped at 2 KB**; injected context (`user_prompt`/`stop`) is capped around 10K characters.
- A hook that fails 3 times in a session is **evicted** for that session.
- A hook can **block** a call, or **add context**, but never **grant** anything: a crashed, malformed, or timed-out hook is a no-op, never an auto-approval.
- `env` in a `[[hook]]` entry is applied *before* hotl's own identity env (`HOTL_HOOK_EVENT`) — your config can't spoof which event your own script thinks it's answering.
- `notification` still never blocks the turn, but in one-shot `hotl -p` runs the process waits (briefly, ~10 s) for any still-running `notification` hook before exiting — so a `tmux display-message`/desktop-notifier hook actually fires instead of being silently killed when the process exits right after the turn ends.

## Which to use

Use **diagnostics** for "run my linter/compiler after edits." Use a **pre_tool hook** for tool policy ("never let bash touch production"), a **post_tool hook** to redact or reshape results, **user_prompt** to inject standing context, **notification** to hook up `hotl watch`/a desktop alert, and **stop** for a last-word "are you actually done?" check. Diagnostics are simpler; hooks are general.
