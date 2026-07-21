---
title: 'Backgrounding a session'
description: Run a hotl agent detached from your terminal with hotl bg, then reconnect from anywhere with hotl attach.
---

Run a `hotl` agent detached from your terminal, then reconnect to it later — without tmux. Assumes a working agent ([quickstart.md](../quickstart/)).

## The model

`hotl bg` starts the session as its **own background process** that listens on a unix socket and outlives the shell you launched it from. You `hotl attach` to drive it, disconnect whenever you like, and reattach later — from any terminal. It is not a tmux session and not a `&` job; it's a real detached process you connect to on demand.

The key property: when the agent needs a permission decision **while you're detached**, the ask is **parked** and re-issued the instant you reattach. So a backgrounded session can keep working and only waits for you at the moments it needs approval.

## Start one

```
hotl bg "refactor the parser and run the tests"
```

Prints a session id and the attach command. The opening prompt is optional — `hotl bg` alone starts an idle session you prompt after attaching. The background session inherits the provider env (`HOTL_MODEL` + key) from the shell you run `hotl bg` in — run it where `hotl doctor` passes.

## List and attach

```
hotl attach            # list live backgrounded sessions
hotl attach bg-12345   # connect to one (an id prefix works)
```

Once attached, it behaves like a line console: type to prompt, type mid-turn to steer, answer `y`/`N` when it asks. Anything that happened while you were detached is in the session log (the live view starts from when you attach).

## Detach, reattach, stop

- **Detach:** `Ctrl-D` — the session keeps running; reattach anytime with `hotl attach <id>`.
- **Reattach:** `hotl attach <id>` — any parked permission ask is re-presented immediately.
- **Stop the session:** type `/stop` while attached — ends the background process and removes its socket.

## Answering asks without staying attached

A backgrounded session parks asks indefinitely, so you can `hotl bg` a task, walk away, and approve its steps when you next attach. (The console's ask modal also waits until you answer; only headless `-p` default-denies.)

## Limits

- **One attacher at a time.** A second `hotl attach` waits until the first detaches.
- **Not restart-durable.** A backgrounded session lives in a running process — it does **not** survive a machine reboot or the process being killed. Its full history is in the session log, so `hotl resume <id>` reconstructs the conversation into a fresh session, but a mid-turn ask that was parked in memory is lost across a reboot. (Log-backed asks that survive a restart are planned.)
- **Fire-and-forget without attaching** is also possible for pre-authorized tasks: `hotl -p "…" &` with allow-rules covering what it needs (headless denies anything not pre-approved).
