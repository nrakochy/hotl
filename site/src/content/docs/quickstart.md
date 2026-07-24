---
title: 'Quickstart — your first hotl session'
description: Install hotl, point it at a model, and complete one approved agent task — then undo it. Every command paired with the output you should see.
---

From nothing installed to a completed agent task. Every command is copy-runnable and paired with the output you should see; the why behind things lives in [permissions-and-sandbox.md](../permissions-and-sandbox/) and [configuration.md](../configuration/).

**Preconditions:**
- macOS or Linux, a terminal, and `git`.
- A model to talk to — one of: a local [Ollama](https://ollama.com) server, or an API key for any OpenAI-compatible endpoint. You do **not** need an Anthropic key for this tutorial.

`mutates:` this installs the `hotl` binary, creates config files under `~/.config/hotl/` and a session log under `~/.local/share/hotl/`, and (in the last step) makes one approved edit in a git repo of your choosing.

## 1. Install the binary

Prebuilt, no toolchain needed:

```
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/nrakochy/hotl/releases/latest/download/hotl-installer.sh | sh
```

(Or, with a Rust toolchain ≥ 1.88: `cargo install hotl`. Building from a checkout — `cargo build --release -p hotl` — works too; then substitute your `target/release/hotl` path for `hotl` below.)

Expected: the installer reports where it put `hotl` (usually `~/.cargo/bin`). Open a fresh shell if needed, then confirm:

```
hotl --version
```

## 2. Point it at a model

Pick the lines that match what you have, and run them in this shell:

```
# Local Ollama (nothing leaves your machine):
export HOTL_MODEL=openai/llama3.1
export HOTL_OPENAI_BASE_URL=http://localhost:11434/v1

# — or — a hosted OpenAI-compatible API:
export HOTL_MODEL=openai/gpt-5
export OPENAI_API_KEY=sk-your-key-here
```

The value of `HOTL_MODEL` is always `provider/model`. `openai/…` covers every OpenAI-compatible endpoint, local or hosted.

Then, **for this tutorial only**, turn on per-action prompts so you see every decision the agent wants to make:

```
export HOTL_PERMISSIONS=ask
```

(The out-of-the-box default is `auto`: ordinary tool calls run without asking, under the sandbox floor, with `undo` covering you. `ask` makes the gate visible, which is the point of a first session.)

## 3. Confirm the setup

```
hotl doctor
```

Expected — the provider line reads `ok`, and the sandbox line names a mechanism:

```
hotl 0.2.0 — doctor
  ok    provider: llama3.1 selected (keys present)
  ok    sandbox: enforced (seatbelt)
  ok    config: none at /Users/you/.config/hotl/config.toml (defaults; run `hotl setup`)
  ok    allow rules: none (every gated tool call asks)
  ok    sessions: /Users/you/.local/share/hotl/sessions (writable)
  ok    memory: none (create /Users/you/.config/hotl/memory/MEMORY.md to enable)
  ok    secrets audit: no current secret values found in stored logs
  ok    undo: git found — sessions snapshot before/after mutating steps
```

If the provider line says `FAIL`, your `HOTL_MODEL`/key env vars aren't set — redo step 2 in this same shell. Do not continue past a `FAIL` provider line.

## 4. Run one task

`cd` into any git repository (`undo` snapshots ride on git), start the agent:

```
hotl
```

You'll see a banner and a `❯` prompt. Ask for something small and concrete — a typo fix, a comment, a rename:

```
fix the typo in README.md
```

The agent reads freely, then asks before it edits — you'll see a line like:

```
allow edit README.md? [y/N]
```

Type `y` and enter. It applies the edit and reports what it changed. Confirm with `git diff`.

## 5. Undo it

Leave the agent (`Ctrl-D`), then:

```
hotl undo
```

It asks to confirm, lists the files it touched, and restores them to before the edit — `git diff` is clean again. That snapshot was taken automatically around the edit in step 4.

## You've now seen the whole loop

Type a request → the agent reads freely → it **asks before changing anything** (in `ask` mode) → you approve per step → every change is snapshotted for `undo`. When you drop the `HOTL_PERMISSIONS=ask` from step 2, the default `auto` mode silences the ordinary prompts but keeps everything else: the kernel sandbox floor on `bash`, always-ask protection on execute-later paths (git hooks, shell rc, Makefiles, agent-instruction files), the full transcript of every auto-allowed call, and `undo`.

**Next:**
- Staying in `ask` mode but tired of approving trusted commands every time? → allow-rules in [configuration.md](../configuration/#allow-rules-allow).
- Want to know exactly what the gate and sandbox protect (and what they don't)? → [permissions-and-sandbox.md](../permissions-and-sandbox/).
- Running it in a script instead of interactively? → headless mode in [configuration.md](../configuration/#headless--p----json).
- Want `: fix the tests` straight from your shell prompt? → [shell.md](../shell/).

**Not covered here:** connecting MCP tool servers and the post-edit hooks feature (see [configuration.md](../configuration/), stubs noted there).
