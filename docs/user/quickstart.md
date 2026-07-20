# Quickstart — your first hotl session

**Mode: tutorial.** This walks you from a fresh checkout to a completed agent task, once, with no forks. Every command is copy-runnable and paired with the output you should see. Why things work the way they do is deliberately deferred — see [permissions-and-sandbox.md](permissions-and-sandbox.md) and [configuration.md](configuration.md) once you've finished.

**Preconditions:**
- macOS or Linux, a terminal, and `git`.
- A Rust toolchain (`rustc`/`cargo`), version ≥ 1.82. (`curl https://sh.rustup.rs -sSf | sh` if you have none.)
- A model to talk to — one of: a local [Ollama](https://ollama.com) server, or an API key for any OpenAI-compatible endpoint. You do **not** need an Anthropic key for this tutorial.

`mutates:` this creates config files under `~/.config/hotl/` and a session log under `~/.local/share/hotl/`, and (in the last step) edits a file in a throwaway git repo.

## 1. Build the binary

```
cd ~/sources/hotl
cargo build --release -p hotl
```

Expected: a compile that ends in `Finished \`release\` profile [optimized]`. The binary is now at `~/sources/hotl/target/release/hotl`. For this tutorial we call it by full path; add it to your `PATH` later if you like.

## 2. Point it at a model

Pick the line that matches what you have, and run it in this shell:

```
# Local Ollama (nothing leaves your machine):
export HOTL_MODEL=openai/llama3.1
export HOTL_OPENAI_BASE_URL=http://localhost:11434/v1

# — or — a hosted OpenAI-compatible API:
export HOTL_MODEL=openai/gpt-5
export OPENAI_API_KEY=sk-your-key-here
```

The value of `HOTL_MODEL` is always `provider/model`. `openai/…` covers every OpenAI-compatible endpoint, local or hosted.

## 3. Confirm the setup

```
~/sources/hotl/target/release/hotl doctor
```

Expected — the provider line reads `ok`, and the sandbox line names a mechanism:

```
hotl 0.1.2 — doctor
  ok    provider: llama3.1 selected (keys present)
  ok    sandbox: enforced (seatbelt)
  ok    config: /Users/you/.config/hotl (default system prompt)
  ok    allow rules: none (every gated tool call asks)
  ok    sessions: /Users/you/.local/share/hotl/sessions (writable)
  ok    memory: none (create /Users/you/.config/hotl/memory/MEMORY.md to enable)
  ok    secrets audit: no current secret values found in stored logs
  ok    undo: git found — sessions snapshot before/after mutating steps
```

If the provider line says `FAIL`, your `HOTL_MODEL`/key env vars aren't set — redo step 2 in this same shell. Do not continue past a `FAIL` provider line.

## 4. Make a throwaway workspace

So the agent has something safe to change, and `undo` has you covered:

```
mkdir -p /tmp/hotl-demo && cd /tmp/hotl-demo && git init -q
printf 'fn main() {\n    println!("helo world");\n}\n' > main.rs
```

## 5. Run one task

Start the agent (interactive):

```
~/sources/hotl/target/release/hotl
```

You'll see a banner and a `❯` prompt. Type this and press enter:

```
fix the typo in main.rs
```

The agent will read the file, then ask before it edits — you'll see a line like:

```
allow edit main.rs? [y/N]
```

Type `y` and enter. It applies the edit and reports what it changed. Confirm:

```
cat /tmp/hotl-demo/main.rs
```

Expected: `helo` is now `hello`.

## 6. Undo it

Leave the agent (`Ctrl-D`), then:

```
~/sources/hotl/target/release/hotl undo
```

It asks to confirm, lists `main.rs`, and restores the file to before the edit. `cat main.rs` shows `helo` again. That snapshot was taken automatically around the edit in step 5.

## You've now seen the whole loop

Type a request → the agent reads freely → it **asks before changing anything** → you approve per step → every change is snapshotted for `undo`. That approve-each-step rhythm is the core of the tool.

**Next:**
- Tired of approving trusted commands every time? → allow-rules in [configuration.md](configuration.md#allow-rules-permissionstoml).
- Want to know exactly what the y/N gate and sandbox protect (and what they don't)? → [permissions-and-sandbox.md](permissions-and-sandbox.md).
- Running it in a script instead of interactively? → headless mode in [configuration.md](configuration.md#headless--p----json).

**Not covered here:** connecting MCP tool servers and the post-edit hooks feature (see [configuration.md](configuration.md), stubs noted there).
