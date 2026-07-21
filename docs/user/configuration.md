# Configuration reference — `hotl` the agent

**Mode: reference.** Facts about the command surface, config files, and environment variables of the `hotl` agent, in the system's own structure. It states what each thing is; it does not teach a workflow (see [quickstart.md](quickstart.md)) or argue for a choice (see [permissions-and-sandbox.md](permissions-and-sandbox.md)). All paths are literal; `~` is the invoking user's home. Behavior described is as of the 2026-07-20 source build.

## Subcommands

| Command | Effect |
|---|---|
| `hotl` | Interactive agent REPL. Type to prompt; type again mid-turn to steer; `Ctrl-C` interrupts the running turn; `Ctrl-D` or `exit`/`quit` leaves. |
| `hotl -p "PROMPT"` | Headless one-shot: run PROMPT to completion, print the answer, exit. See [Headless](#headless--p----json). |
| `hotl -p "PROMPT" --json` | Headless with a JSONL event stream on stdout instead of prose. |
| `hotl resume` | List recent sessions (id + age), newest first. |
| `hotl resume <id-prefix>` | Start a new session seeded with an earlier session's full context (replayed from its log and ancestry). |
| `hotl undo` | Restore workspace files to before the most recent session's last mutating step. Confirm-gated; `--force`/`-f` skips the prompt. |
| `hotl tui [id-prefix\|--resume]` | Full-screen console: streaming transcript, activity strip, modal asks, vim input. See [tui.md](tui.md). |
| `hotl bg [prompt]` | Background a session as a detached socket server; `hotl attach` to reach it. See [backgrounding.md](backgrounding.md). |
| `hotl attach [id]` | Connect to a backgrounded session (bare: list live ones). |
| `hotl gc [--dry-run] [--days N] [--keep N]` | Prune old sessions/shadows/blobs per `[retention]`. See [below](#hotl-gc). |
| `hotl setup [--force]` | Write a commented starter `config.toml` (never overwrites without `--force`). |
| `hotl doctor` | Non-mutating checks: provider/keys, sandbox, config, allow-rules, session store, memory, secrets audit, undo/git. Exit 1 if any check FAILs. |
| `hotl init zsh` | Print the zsh `:` prefix plugin to stdout; `eval "$(hotl init zsh)"` in `~/.zshrc` makes a line starting `: ` run as an agent prompt. |
| `hotl watch` | The tmux dashboard (separate capability; [crates/hotl/README.md](../../crates/hotl/README.md)). |
| `hotl update [ver]` | Print the version + how to update (compares against `ver` if given). |
| `hotl fleet` | Reserved (orchestrate); not built — exits 2. |
| `hotl --help` | Usage summary. |

## One config file: `config.toml`

Everything hand-editable lives in **`~/.config/hotl/config.toml`** (or `$XDG_CONFIG_HOME/hotl/config.toml`). `hotl setup` writes a commented starter. It's the only settings file — there is no `permissions.toml`/`mcp.toml`/`hooks.toml`; those are sections here now. A malformed file is ignored with a warning, never half-applied.

```toml
[provider]
model = "openai/gpt-5"                      # provider/model
base_url = "http://localhost:11434/v1"      # OpenAI-compatible endpoint
fast_model = "..."                          # cheap model for compaction summaries
api_key_helper = "..."                      # command whose trimmed stdout is the API key; beats static key env vars; 5s timeout, 64KB cap
api_key_helper_ttl_secs = 300               # re-run the helper when the cached key is older; absent = startup + auth-failure only

[context]
window = 200000            # your model's context size in tokens
evict_tokens = 20000       # offload tool results larger than this (0 disables)
compaction_reset = false   # fresh-slate compaction instead of in-place
show_used_pct = true       # show context-fullness in each turn's status

[behavior]
ask_timeout_secs = 300     # 0 = wait forever for a permission answer
sandbox = true             # false disables the bash sandbox floor
vim_mode = true            # vim-style keys in the `hotl tui` input editor

[network]
egress = "open"            # "open" | "off" | "allowlist" (bash network egress)
allow = ["github.com", "*.crates.io"]   # hosts reachable in allowlist mode

[retention]
max_age_days = 30          # prune sessions older than this (hotl gc)
max_sessions = 200         # keep at most this many

[[allow]]                  # allow-rules (see below)
tool = "bash"
prefix = "cargo "

[[mcp]]                    # MCP servers (see below)
name = "docs"
command = "/usr/local/bin/docs-mcp"
args = ["--stdio"]
description = "project documentation search"

[[hook]]                   # tool-call hooks (see hooks.md)
event = "pre_tool"
command = "/usr/local/bin/guard"

[diagnostics]              # post-edit checks (see hooks.md)
rs = "cargo check -q --message-format=short"
```

**Precedence for the scalar settings: environment variable > config.toml > default.** So a `HOTL_MODEL` in the shell overrides `[provider].model`, and CI can override anything without editing the file.

### Other files (not "config", so not in config.toml)

| File | Purpose |
|---|---|
| `system-prompt.md` | Replaces the built-in agent instructions (prose). |
| `memory/MEMORY.md` | Loaded into every session's starting context (capped at 16 KB), enveloped. |
| `skills/*.md` | One procedure per file; the `skill` tool lists and loads them by name. |
| `trust.toml` | Written by hotl, not you: approved MCP server binary hashes. |

### Environment variables

| Variable | Overrides | Meaning |
|---|---|---|
| `HOTL_MODEL` | `[provider].model` | `provider/model`; `openai/…` covers any OpenAI-compatible endpoint. |
| `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` | — | Provider keys (never put keys in config.toml). |
| `HOTL_OPENAI_BASE_URL` | `[provider].base_url` | OpenAI-compatible endpoint. A non-loopback `http://` URL with a key set warns (cleartext). |
| `HOTL_API_KEY_HELPER` | `[provider].api_key_helper` | Overrides the config.toml key of the same name. |
| `HOTL_API_KEY_HELPER_TTL_SECS` | `[provider].api_key_helper_ttl_secs` | Overrides the config.toml key of the same name. |
| `HOTL_CONTEXT_WINDOW` | `[context].window` | Context size in tokens; compaction fires at ~80%. |
| `HOTL_FAST_MODEL` | `[provider].fast_model` | Cheap model for compaction summaries. |
| `HOTL_EVICT_TOKENS` | `[context].evict_tokens` | Tool-result eviction threshold (`0` disables). |
| `HOTL_ASK_TIMEOUT` | `[behavior].ask_timeout_secs` | `0` = wait forever (backgrounded sessions). |
| `HOTL_SANDBOX` | `[behavior].sandbox` | `off` disables the bash sandbox floor. |
| `XDG_CONFIG_HOME` / `XDG_DATA_HOME` | — | Bases for the config dir and the session/shadow store. |

### Allow-rules (`[[allow]]`)

Auto-approve tool calls so you aren't prompted for trusted operations. Deliberately config-only — there is no in-REPL "always allow" (that is by design; see [permissions-and-sandbox.md](permissions-and-sandbox.md#why-allow-rules-are-a-file-you-edit)).

```toml
[[allow]]
tool = "bash"
prefix = "cargo "          # auto-allow bash commands beginning with "cargo "

[[allow]]
tool = "write"             # or "edit"
path_prefix = "src/"       # auto-allow writes/edits under src/
```

Rules that do **not** auto-allow, even with a matching rule (safety carve-outs):
- A `bash` command containing a shell control operator (`;`, `|`, `&`, `<`, `>`, backtick, `$(`, braces, newline) — it does more than the prefix implies.
- A `bash` rule at all when the sandbox floor is not enforced, or when a configured `[network]` egress restriction cannot be kernel-enforced on this host.
- A `write`/`edit` path that resolves outside the prefix after `..` normalization, or is absolute against a relative prefix.
- Any write to a protected (execute-later) path — always asks. See [permissions-and-sandbox.md](permissions-and-sandbox.md#protected-paths).

### MCP servers (`[[mcp]]`)

Declare external tool servers. Each is exposed to the model through one `mcp` tool; the **first** use of a server prompts you to approve its binary (shown with its SHA-256), and a changed binary re-prompts. Server output is sanitized before it reaches the model. Full guide: [mcp.md](mcp.md).

### Post-edit diagnostics (`[diagnostics]`) and hooks (`[[hook]]`)

`[diagnostics]` runs a check command after a successful `edit`/`write` (under the sandbox floor, 30 s timeout). `[[hook]]` intercepts tool calls. Full guide: [hooks.md](hooks.md).

### Network egress (`[network]`)

Restricts what `bash` commands (and diagnostics/hooks, which run under the same floor) may reach over the network. `egress` is one of `open` (default; unrestricted), `off` (loopback and unix-domain sockets only), or `allowlist` (loopback plus the hosts in `allow`, reached through a local filtering proxy). `allow` entries are hostnames or `*.domain` wildcards — a wildcard matches the apex and any subdomain depth; no ports; matching is case-insensitive; an empty list allows nothing. An unknown `egress` value fails closed to `off` with a startup warning. While a restriction is configured, the bash ask label carries `net:off` / `net:allow(N)` — or `NET:UNENFORCED(reason)` on hosts where the kernel cannot back it (Linux needs kernel ≥ 6.7 for Landlock net; `HOTL_SANDBOX=off` also unenforces it), in which case `bash` allow-rules stop auto-approving. A denied fetch returns `hotl egress: "HOST" is not in [network].allow`. Why and limits: [permissions-and-sandbox.md](permissions-and-sandbox.md#opting-out-of-open-egress).

### Retention (`[retention]`)

Bounds the growth of the session/shadow/blob stores. `hotl gc` prunes on demand; with a `[retention]` policy set, a prune also runs quietly at startup. See [`hotl gc`](#hotl-gc).

## hotl gc

`hotl gc [--dry-run] [--days N] [--keep N]` prunes whole sessions (log + evicted-result blobs + shadow snapshot repo) older than `max_age_days` or beyond `max_sessions`, and sweeps dead backgrounded-session sockets. Flags override `[retention]`. With no policy and no flags it's a no-op that tells you so. `--dry-run` lists what would go without deleting.

## Headless (`-p` / `--json`)

`hotl -p "PROMPT"` runs one turn and exits. Because no human is present, **every permission ask is auto-denied** — headless runs cannot perform gated actions unless an allow-rule covers them. Configure `[[allow]]` rules in config.toml for anything a headless run must do.

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

**See also:** [mcp.md](mcp.md) for connecting MCP tool servers, [hooks.md](hooks.md) for diagnostics and hooks, and [uninstall.md](uninstall.md) for removal.
