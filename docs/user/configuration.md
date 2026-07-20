# Configuration reference — `hotl` the agent

**Mode: reference.** Facts about the command surface, config files, and environment variables of the `hotl` agent, in the system's own structure. It states what each thing is; it does not teach a workflow (see [quickstart.md](quickstart.md)) or argue for a choice (see [permissions-and-sandbox.md](permissions-and-sandbox.md)). All paths are literal; `~` is the invoking user's home. Behavior described is as of the M0–M3 build, 2026-07-20.

## Subcommands

| Command | Effect |
|---|---|
| `hotl` | Interactive agent REPL. Type to prompt; type again mid-turn to steer; `Ctrl-C` interrupts the running turn; `Ctrl-D` or `exit`/`quit` leaves. |
| `hotl -p "PROMPT"` | Headless one-shot: run PROMPT to completion, print the answer, exit. See [Headless](#headless--p----json). |
| `hotl -p "PROMPT" --json` | Headless with a JSONL event stream on stdout instead of prose. |
| `hotl resume` | List recent sessions (id + age), newest first. |
| `hotl resume <id-prefix>` | Start a new session seeded with an earlier session's full context (replayed from its log and ancestry). |
| `hotl undo` | Restore workspace files to before the most recent session's last mutating step. Confirm-gated; `--force`/`-f` skips the prompt. |
| `hotl doctor` | Non-mutating checks: provider/keys, sandbox, config, allow-rules, session store, memory, secrets audit, undo/git. Exit 1 if any check FAILs. |
| `hotl init zsh` | Print the zsh `:` prefix plugin to stdout; `eval "$(hotl init zsh)"` in `~/.zshrc` makes a line starting `: ` run as an agent prompt. |
| `hotl watch` | The tmux dashboard (separate capability; [crates/hotl/README.md](../../crates/hotl/README.md)). |
| `hotl fleet` | Reserved (orchestrate); not built — exits 2. |
| `hotl update` | Reserved (distribution milestone); not built — exits 2. |
| `hotl --help` | Usage summary. |

## Environment variables

| Variable | Default | Meaning |
|---|---|---|
| `HOTL_MODEL` | `claude-opus-4-8` | `provider/model`. `anthropic/<model>` or `openai/<model>`. A bare value (no `/`) means anthropic. `openai` covers any OpenAI-compatible endpoint. |
| `ANTHROPIC_API_KEY` | — | Required when the provider is `anthropic`. |
| `OPENAI_API_KEY` | — | Required for `openai/…` against `api.openai.com`. Optional for other base URLs. |
| `HOTL_OPENAI_BASE_URL` | `https://api.openai.com/v1` | Endpoint for the `openai` provider. Set it to a local server (e.g. `http://localhost:11434/v1` for Ollama) to run keyless. `requires network:` unless loopback. A non-loopback `http://` URL with a key set triggers a cleartext-key warning. |
| `HOTL_CONTEXT_WINDOW` | `200000` | Model context size in tokens. The agent compacts old history at ~80% of this. Set it to your model's real window so compaction fires at the right time. |
| `HOTL_FAST_MODEL` | (= `HOTL_MODEL`) | A cheaper model used only for compaction summaries. |
| `HOTL_SANDBOX` | (unset) | `off` disables the bash sandbox floor. Every `bash` ask is then marked `UNSANDBOXED`. |
| `XDG_CONFIG_HOME` | `~/.config` | Base for the config dir (`$XDG_CONFIG_HOME/hotl`). |
| `XDG_DATA_HOME` | `~/.local/share` | Base for session logs and shadow snapshots (`$XDG_DATA_HOME/hotl`). |

## Config files

All optional. Location: `~/.config/hotl/` (or `$XDG_CONFIG_HOME/hotl/`). Missing files mean "feature off"; a malformed file is ignored with a warning, never half-applied.

| File | Purpose |
|---|---|
| `system-prompt.md` | Replaces the built-in instructions to the agent. Non-empty content wins; otherwise the default is used. |
| `permissions.toml` | Allow-rules — auto-approve trusted tool calls. See [below](#allow-rules-permissionstoml). |
| `memory/MEMORY.md` | Loaded into every session's starting context (capped at 16 KB), wrapped as untrusted content. |
| `mcp.toml` | External MCP tool servers. See [below](#mcp-servers-mcptoml). |
| `hooks.toml` | Post-edit check commands by file extension. See [below](#post-edit-diagnostics-hookstoml). |
| `trust.toml` | Written by hotl, not you: records which MCP server binaries you've approved (by SHA-256). |
| `skills/*.md` | One procedure per file; the `skill` tool lists them and loads one by name on request. |

### Allow-rules (`permissions.toml`)

Auto-approve tool calls so you aren't prompted for trusted operations. Deliberately file-only — there is no in-REPL "always allow" (that is by design; see [permissions-and-sandbox.md](permissions-and-sandbox.md#why-allow-rules-are-a-file-you-edit)).

```toml
# ~/.config/hotl/permissions.toml
[[allow]]
tool = "bash"
prefix = "cargo "          # auto-allow bash commands beginning with "cargo "

[[allow]]
tool = "write"             # or "edit"
path_prefix = "src/"       # auto-allow writes/edits under src/
```

Rules that do **not** auto-allow, even with a matching rule (safety carve-outs):
- A `bash` command containing a shell control operator (`;`, `|`, `&`, `<`, `>`, backtick, `$(`, braces, newline) — it does more than the prefix implies.
- A `bash` rule at all when the sandbox floor is not enforced.
- A `write`/`edit` path that resolves outside the prefix after `..` normalization, or is absolute against a relative prefix.
- Any write to a protected (execute-later) path — always asks. See [permissions-and-sandbox.md](permissions-and-sandbox.md#protected-paths).

### MCP servers (`mcp.toml`)

Declare external tool servers. Each is exposed to the model through one `mcp` tool; the **first** use of a server prompts you to approve its binary (shown with its SHA-256), and a changed binary re-prompts. Server output is sanitized before it reaches the model.

```toml
# ~/.config/hotl/mcp.toml
[[server]]
name = "docs"
command = "/usr/local/bin/docs-mcp"
args = ["--stdio"]
description = "project documentation search"
```

Transport is stdio JSON-RPC only. *(Full MCP how-to guide: not yet written — this table is the current reference.)*

### Post-edit diagnostics (`hooks.toml`)

After a successful `edit`/`write`, run a check command for that file's extension and show the agent the result. The command runs under the same sandbox floor as `bash` and is killed after 30 s.

```toml
# ~/.config/hotl/hooks.toml
[diagnostics]
rs = "cargo check -q --message-format=short"
py = "ruff check ."
```

*(Extended hooks guide: not yet written.)*

## Headless (`-p` / `--json`)

`hotl -p "PROMPT"` runs one turn and exits. Because no human is present, **every permission ask is auto-denied** — headless runs cannot perform gated actions unless an allow-rule covers them. Configure `permissions.toml` for anything a headless run must do.

`--json` emits one JSON object per line (a stable-ish event stream for scripts): event types include `text_delta`, `tool_start`, `tool_done`, `ask_denied`, `compacted`, and a terminal `turn_done` carrying the outcome and token usage.

### Exit codes

| Code | Meaning |
|---|---|
| `0` | The turn completed (`Done`). |
| `130` | Interrupted (`Ctrl-C` / cancelled). |
| `1` | Any other outcome: error, refusal, turn-limit, doom-loop, tool-failure-budget, or a `doctor` FAIL. |
| `2` | Bad usage, or a reserved subcommand (`fleet`, `update`). |

## Data at rest

| Path | Contents |
|---|---|
| `~/.local/share/hotl/sessions/<ulid>.jsonl` | Append-only session logs. Permanent by design. Secret-named env values are masked at write time; the log is otherwise sensitive — treat it as such. |
| `~/.local/share/hotl/shadow/<ulid>.git` | Per-session git snapshots backing `hotl undo`. Secret-bearing files (`.env`, `*.pem`, `*.key`, `id_*`, `.ssh/`, `.aws/`, `.npmrc`, `.pypirc`, `.netrc`, `secrets.*`, `credentials`) are excluded. No automatic cleanup yet. |

**Engine defaults (not user-configurable via env yet):** max 25 turns per prompt, 32000 max output tokens, adaptive thinking on, static prompt caching on, a tool that fails 5 times consecutively stops the turn.

**Not covered here:** the full MCP setup guide, the hooks cookbook, and uninstall — tracked as open items for the distribution milestone (`docs/design-docs/distribution.md §D7`).
