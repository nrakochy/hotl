---
title: 'Backgrounding a session'
description: Run a hotl agent detached from your terminal with hotl bg, then reconnect from anywhere with hotl attach.
---

Run a `hotl` agent detached from your terminal, then reconnect to it later — without tmux. Assumes a working agent ([quickstart.md](../quickstart/)).

## Background jobs are not background sessions

Three different things share the word:

| | What it is | Verb |
|---|---|---|
| **Background job** | One shell command the *agent* started with `bash {background: true}` — a test suite, a build, a dev server — that keeps running while the session goes on. The agent is told when it ends; you see `⚙ N` on the strip and list them with `/jobs`. Jobs die with their session unless started with `detach: true`. See [configuration.md](../configuration/#built-in-tools) and [tui.md](../tui/). | `bash` tool, `/jobs` |
| **Background sub-agent** | A child session the *agent* started with `spawn {background: true}` and kept working beside. Its typed result comes back as a notification; its row stays in the agent band until it finishes; `/agents` lists them. Children die with their session. See [agents.md](../agents/#background-sub-agents-wait-send-resume). | `spawn`/`wait`/`send`/`resume`, `/agents` |
| **Background session** | The whole *hotl process* detached from your terminal, driven over a socket from any shell you attach from. This page. | `hotl bg`, `hotl attach` |

A background session can of course run background jobs and sub-agents; they end with it like any other session's.

## The model

`hotl bg` starts the session as its **own background process** that listens on a unix socket and outlives the shell you launched it from. You `hotl attach` to drive it, disconnect whenever you like, and reattach later — from any terminal. It is not a tmux session and not a `&` job; it's a real detached process you connect to on demand.

The key property: when the agent needs a permission decision **while you're detached**, the ask is **parked** and re-issued the instant you reattach. So a backgrounded session can keep working and only waits for you at the moments it needs approval.

## Start one

```
hotl bg "refactor the parser and run the tests"
```

Prints a session id and the attach command. To reopen a stored session in
the background instead of starting a new one, pass its full id:
`hotl bg --resume <session-id> [prompt]` (not together with `-w`). If the
stored session stopped in the middle of a task, it carries on with it. The opening prompt is optional — `hotl bg` alone starts an idle session you prompt after attaching. The background session inherits the provider env (`HOTL_MODEL` + key) from the shell you run `hotl bg` in — run it where `hotl doctor` passes.

## List and attach

```
hotl attach            # list live backgrounded sessions
hotl attach bg-12345   # connect to one (an id prefix works)
hotl attach --json     # one JSON line per live session, for scripts
```

`--json` prints each live session's run id, stored session id, process
number, folder, name, hotl version, start time and state (`idle`, `running`
or `waiting`). The fields are described in
[Driving hotl from another program](../embedding/).

Once attached, it behaves like a line console: type to prompt, type mid-turn to steer, answer `y`/`N` when it asks. Anything that happened while you were detached is in the session log (the live view starts from when you attach).

## Detach, reattach, stop

- **Detach:** `Ctrl-D` — the session keeps running; reattach anytime with `hotl attach <id>`.
- **Reattach:** `hotl attach <id>` — any parked permission ask is re-presented immediately.
- **Stop the session:** type `/stop` while attached — ends the background process and removes its socket.

## Attaching from another ACP client

The socket speaks ACP v1, the same JSON-RPC lines `hotl acp` speaks on stdio, so any ACP client can attach. Open with `initialize` and put the session token (the `<id>.token` file next to the socket) in `params._meta["hotl.dev/socketToken"]`; a missing or wrong token closes the connection with no reply. Then send `session/resume` with the session id: there is no history replay, and any parked ask arrives as a `session/request_permission` right after the reply. `session/new` and `session/load` are refused because the session already exists. `session/close`, or just hanging up, detaches and leaves the session running; `_hotl/session/stop` ends it.

## Answering asks without staying attached

A backgrounded session parks asks indefinitely, so you can `hotl bg` a task, walk away, and approve its steps when you next attach. (The console's ask modal also waits until you answer; only headless `-p` default-denies.)

## Limits

- **One attacher at a time.** A second `hotl attach` takes over: the first client is disconnected and the session carries on with the new one.
- **Not restart-durable.** A backgrounded session lives in a running process — it does **not** survive a machine reboot or the process being killed. Its full history is in the session log, so `hotl resume <id>` reconstructs the conversation into a fresh session, but a mid-turn ask that was parked in memory is lost across a reboot. (Log-backed asks that survive a restart are planned.)
- **Fire-and-forget without attaching** is also possible for pre-authorized tasks: `hotl -p "…" &` with allow-rules covering what it needs (headless denies anything not pre-approved).
