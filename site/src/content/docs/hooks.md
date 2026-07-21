---
title: 'Hooks — running your own checks on tool calls'
---

**Mode: how-to.** Steps to make the `hotl` agent run *your* logic when it uses a tool — block a call, rewrite it, or clean up a result. Two mechanisms: post-edit **diagnostics** (run a check after edits) and **hooks** (intercept any tool call). Assumes a working agent ([quickstart.md](../quickstart/)).

## Post-edit diagnostics (the simple one)

Make the agent see your project's own check output right after it edits a file. In `~/.config/hotl/config.toml`:

```toml
[diagnostics]
rs = "cargo check -q --message-format=short"
py = "ruff check ."
```

After a successful `edit`/`write` to a `.rs` file, `cargo check` runs (under the sandbox floor, 30 s timeout) and up to 30 lines of its output are appended to the tool result — so the agent notices breakage in the same step it caused it. A clean, quiet check adds nothing.

## Hooks (intercept tool calls)

A hook runs at one of two moments: **before** a tool call (`pre_tool` — you can allow, block, or rewrite it) or **after** (`post_tool` — you can replace the result). In `~/.config/hotl/config.toml`:

```toml
[[hook]]
event = "pre_tool"
command = "/usr/local/bin/guard"

[[hook]]
event = "post_tool"
command = "/usr/local/bin/scrub"
```

Your command receives the event as JSON on **stdin** and returns a decision as JSON on **stdout**.

**pre_tool** — stdin `{"event":"pre_tool","tool":"bash","input":{...}}`, respond with one of:
```json
{"decision":"continue"}
{"decision":"deny","message":"why the model should not do this"}
{"decision":"rewrite","input":{ ...replacement args... }}
```
A `deny` becomes an error tool result carrying your message. A `rewrite` swaps the arguments and **re-enters the normal permission gate** — a hook cannot push a call past the y/N ask.

**post_tool** — stdin `{"event":"post_tool","tool":"read","result":"<up to 2KB>"}`, respond with `{"result":"replacement"}` to change what the model sees, or anything else to leave it.

### Rules hooks live by

- Hook commands run **under the sandbox floor**, like `bash` — they're commands, not trusted-by-position.
- The result a hook sees is **capped at 2 KB** (a hook can't be used to amplify a huge result into the process).
- A hook that fails 3 times in a session is **evicted** for that session.
- A hook can **block** a call but never **grant** one: a crashed or malformed hook is a no-op, never an auto-approval.

## Which to use

Use **diagnostics** for "run my linter/compiler after edits." Use a **pre_tool hook** for policy ("never let bash touch production"), and a **post_tool hook** to redact or reshape results. Diagnostics are simpler; hooks are general.
