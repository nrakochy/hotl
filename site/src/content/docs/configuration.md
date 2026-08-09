---
title: 'Configuration reference — hotl the agent'
description: Reference for every hotl subcommand, config file key, environment variable, and exit code.
---

Reference for the command surface, config files, and environment variables of the `hotl` agent. For a guided first run see [quickstart.md](../quickstart/); for the reasoning behind the safety model see [permissions-and-sandbox.md](../permissions-and-sandbox/). All paths are literal; `~` is the invoking user's home.

## Subcommands

| Command | Effect |
|---|---|
| `hotl [id-prefix\|--resume]` | The full-screen console: streaming transcript, activity strip, modal asks, vim input. Needs a terminal (no TTY → exit 2, use `-p`). See [tui.md](../tui/). |
| `hotl -p "PROMPT"` | Headless one-shot: run PROMPT to completion, print the answer, exit. See [Headless](#headless--p----json). |
| `hotl -p "PROMPT" --json` | Headless with a JSONL event stream on stdout instead of prose. |
| `hotl resume [id-prefix]` | Continue an earlier session in the console (bare: pick from a numbered list). The seeded session replays the earlier one's full context from its log and ancestry. |
| `hotl --fork-from <n\|id\|name\|@last>` | Start a **new** session seeded with another's history, pinned to that session's state at fork time (the original can keep running). `--keep-turns <n>` / `--keep <items>` fork at a prefix instead of the head; `@last` is the newest session. Works headless too (`hotl -p "…" --fork-from …`). See [sessions.md](../sessions/). |
| `hotl undo` | Restore workspace files to before the most recent session's last mutating step. Confirm-gated; `--force`/`-f` skips the prompt. |
| `hotl bg [prompt]` | Background a session as a detached socket server; `hotl attach` to reach it. See [backgrounding.md](../backgrounding/). |
| `hotl attach [id]` | Connect to a backgrounded session (bare: list live ones). |
| `hotl gc [--dry-run] [--days N] [--keep N]` | Prune old sessions/shadows/blobs per `[retention]`. See [below](#hotl-gc). |
| `hotl setup [--force]` | Write a commented starter `config.toml` (never overwrites without `--force`). |
| `hotl doctor` | Non-mutating checks: provider/keys, sandbox, config, allow-rules, session store, MCP trust, memory, secrets audit, undo/git. Exit 1 if any check FAILs. |
| `hotl init zsh` | Print the zsh `:` prefix plugin to stdout; `eval "$(hotl init zsh)"` in `~/.zshrc` makes a line starting `: ` run as an agent prompt. |
| `hotl skills [add\|update\|remove]` | List every discovered skill with its source; manage skill marketplaces. See [skills.md](../skills/). |
| `hotl mcp [list\|show\|add\|untrust\|test]` | Inspect configured MCP servers and their trust grants. Read-mostly: it never writes `config.toml`, and there is no verb that grants trust. See [below](#hotl-mcp) and [mcp.md](../mcp/). |
| `hotl watch` | The tmux dashboard (separate capability; [crates/hotl/README.md](https://github.com/nrakochy/hotl/blob/master/crates/hotl/README.md)). |
| `hotl update` | Install the latest release. `--check` only looks; `--version X.Y.Z` picks one; `-y` skips the prompt. See [updating](../updating/). |
| `hotl fleet` | Reserved (orchestrate); not built — exits 2. |
| `hotl --help` | Usage summary. |

## One config file: `config.toml`

Everything hand-editable lives in **`~/.config/hotl/config.toml`** (or `$XDG_CONFIG_HOME/hotl/config.toml`). `hotl setup` writes a commented starter. It's the only settings file — there is no `permissions.toml`/`mcp.toml`/`hooks.toml`; those are sections here now. A malformed file is ignored with a warning, never half-applied.

```toml
[provider]
model = "openai/gpt-5"                      # provider/model
base_url = "http://localhost:11434/v1"      # endpoint for the active provider
auth = "api_key"                            # or "subscription": hotl holds no credential (requires base_url)
fast_model = "..."                          # cheap model for compaction summaries
api_key_helper = "..."                      # command whose trimmed stdout is the API key; beats static key env vars; 5s timeout, 64KB cap
api_key_helper_ttl_secs = 300               # re-run the helper when the cached key is older; absent = startup + auth-failure only

[context]
window = 200000            # usually unnecessary — looked up per model; see below
evict_tokens = 20000       # offload tool results larger than this (0 disables)
compaction_reset = false   # fresh-slate compaction instead of in-place
show_used_pct = true       # show context-fullness in each turn's status

[behavior]
sandbox = true             # false disables the bash sandbox floor
vim_mode = false           # true = vim-style keys in the console's input editor
mouse = true               # false stops mouse capture: no wheel scroll, no drag
                           # select, and your terminal owns the mouse again
copy_on_select = true      # false stops a mouse drag copying to the clipboard
                           # (needs mouse = true; without capture there are no
                           # drag events to act on)
max_turns = 100            # model steps one prompt may spend (a tool round-trip
                           # costs one). -1 = unlimited: run until the model is
                           # done, the context fills, or you interrupt.

[permissions]
mode = "bypass"   # "bypass" | "ask" | "dontask"  ("auto" = the old name for bypass)
                  # bypass: no per-action y/N; protected paths + sandbox still guard.
                  # ask: approve every mutating/executing call.
                  # dontask: never wait for input — deny anything not pre-approved (CI).
                  # A security-enforced build ignores this key entirely (ask stays on).
plan = false      # the other axis, independent of mode: write/edit always ask,
                  # never auto — everything else still follows `mode`.
                  # `/plan`, `--plan`, or HOTL_PLAN=1 toggle it. See
                  # permissions-and-sandbox.md.

[network]
egress = "open"            # "open" | "off" | "allowlist" (bash network egress)
allow = ["internal.example.com"]   # hosts reachable in allowlist mode, on top
                                   # of hotl's starter list
defaults = true            # false: use exactly `allow`, no starter list

[sandbox]                  # widen the kernel write floor (see below)
writable = ["~/Library/Caches/bazel", "~/.bazel_disk_cache"]
file_tools = "workspace"   # "workspace" | "writable" — how far write/edit follow those dirs

[web.search]                # optional: enables web_search (absent by default)
url = "https://s.example/api"   # a JSON search API you run/subscribe to
api_key_env = "SEARCH_KEY"      # name of an env var holding the key (never the key itself)
result_cap = 8                  # max results per search (default 8)

[skills]                    # skill roots (see skills.md)
claude = true               # false: skip ~/.claude/skills and Claude plugin skills

[skills.marketplaces]       # extra skill sources — managed by `hotl skills`
acme = "https://github.com/acme/skills.git"   # managed checkout
team = "~/work/team-skills"                    # local, read in place

[agents]                    # sub-agent defs (see agents.md)
claude = true               # false: skip ~/.claude/agents
isolation = "none"          # "worktree": every mutating child gets its own
                            # git worktree and they run in parallel; a def's
                            # own `isolation:` frontmatter wins

[concurrency]               # Layer-B budgets; every field optional, safe defaults
requests = 4                # concurrent web_fetch/web_search HTTP requests
agents = 4                  # concurrent spawn sub-agent sessions (global, parent + children)

[retention]
max_age_days = 30          # prune sessions older than this (hotl gc)
max_sessions = 200         # keep at most this many

[history]                  # console prompt recall (↑/↓, Ctrl-R) — see tui.md
enabled = true             # false: recall works in-session, nothing on disk
max_entries = 1000         # oldest entries trimmed past this
max_bytes = 2097152        # ...and past this size (2 MiB); the smaller cap wins
# path = "..."             # default: <xdg-data>/hotl/history.jsonl (~ expanded)

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

[settings]
density = "comfortable"    # transcript spacing: compact | comfortable | spacious

[settings.theme]           # palette for the console AND `hotl watch` (see tui.md)
preset = "warm"            # tokyo-night (the default) | warm | catppuccin | gruvbox | nord | dracula
accent = "#88c0d0"         # optional per-slot #rrggbb overrides: active blocked idle
                           # ink muted faint accent band
```

**`density`** controls how much room the console TUI gives the transcript
(colors live under `[settings.theme]`; the two are independent):

| Value | Between turns | Left gutter |
|---|---|---|
| `compact` | no blank line | none — edge to edge |
| `comfortable` *(default)* | one blank line | 2 columns |
| `spacious` | one blank line | 4 columns |

An unrecognized value warns and falls back to `comfortable`. The gutter is
where the role spine is drawn (see [tui.md](../tui/)). `warm` is a low-blue
palette — paper-white ink, amber accent, terracotta — for a less clinical
feel; it's opt-in, the default stays `tokyo-night`.

**Precedence for the scalar settings: environment variable > config.toml > default.** So a `HOTL_MODEL` in the shell overrides `[provider].model`, and CI can override anything without editing the file.

### Reloading without restarting

`config.toml` is read at startup. In the console, `/reload` re-reads it and rebuilds the engine against the new file, keeping the session — the transcript, the model's context, the todos, the session name, its permission mode, and plan mode all carry forward. `hotl acp` clients reach the same thing as `session/reload_config`.

Most of the file reloads: `[provider]`, `[[allow]]`, `[[mcp]]`, `[[hook]]`, `[diagnostics]`, `[skills]`, `[agents]`, `[context]`, `[behavior]`, `[settings]`, `system-prompt.md`. A reload that fails to parse or to select a provider changes nothing and says so — the running engine keeps serving.

These are fixed for the life of the process; restart to change them:

| Setting | Why |
|---|---|
| `[sandbox]`, `[network]` | Installed once, before the first tool can run. Widening egress or the write floor mid-process would defeat them. |
| `[behavior] sandbox` | The confinement probe has already run. |
| `[concurrency] worker_threads`, `blocking_threads` | Resolved before the async runtime is built. |
| `[history]` | The recall ring is loaded at startup; re-reading it would drop prompts submitted since. |

`[permissions] mode` reloads, but it never overrides a mode you chose: `/mode` is logged with the session and survives, exactly as it does across `hotl resume`. A session that never set one has no mode of its own and picks up the reloaded default. See [tui.md](../tui/#reloading-config).

### Other files (not "config", so not in config.toml)

| File | Purpose |
|---|---|
| `system-prompt.md` | Replaces the built-in agent instructions (prose). |
| `memory/MEMORY.md` | Loaded into every session's starting context (capped at 16 KB), enveloped. |
| `skills/*.md` | One procedure per file; the `skill` tool lists and loads them by name. See [skills.md](../skills/). |
| `agents/*.md` | One sub-agent definition per file — `tools`/`model`/`effort`/`isolation` frontmatter, body = system prompt. See [agents.md](../agents/). |
| `trust.toml` | Written by hotl, not you: approved MCP server binary hashes. |

### Skills (`[skills]`)

Two keys. Everything else about skills — the grouped index the agent sees,
search over collapsed sources, bodies read on demand, marketplaces, and the
`/` dispatch — lives in [skills.md](../skills/).

| Key | Effect |
|---|---|
| `claude` | `false` stops reading `~/.claude/skills` and Claude plugin skills (default `true`). |
| `[skills.marketplaces]` | One `name = "<git url or local path>"` per extra skill source; managed by `hotl skills add` / `update` / `remove`. |

### Built-in tools

| Tool | Effect | Permission |
|---|---|---|
| `read` | Read a text file (2000 lines / 200KB per call; any single line over 8KB is clipped; `offset`/`limit` continue a truncated read). `minified: true` serves a smaller token-stream view of source code instead — see [`[minify]`](#minified-reads-and-edits-minify). | None inside the working directory — **outside it, a protected ask** |
| `edit` | Exact string replacement in a file. `minified: true` matches against the minified view and splices into the real file, which keeps its formatting. | Ask (protected paths escalate) |
| `write` | Write a file, creating parent directories. | Ask (protected paths escalate) |
| `bash` | Run a shell command under the sandbox floor. stdout and stderr share one pipe, so output arrives in the order the command actually wrote it; a failure ends with `[exit N]` or `[killed by SIGNAME]`. | Ask |
| `glob` | List files under the working directory matching a **real glob**: `*` (does not cross `/`), `**` (recurses), `?`, `[a-z]`, `{a,b}`. A pattern with no `/` matches the file name at any depth (`*.rs`). Newest-first by default (`sort`: `"mtime"` \| `"path"`), capped at 1000. Respects `.gitignore`; `.git` is never walked; symlinks are never followed. In-process — no subprocess, so it still works with no `rg` on `PATH` or when the sandbox floor degrades, and the walk runs on the blocking pool so a large or hostile tree cannot stall the runtime. | None — read-only |
| `grep` | Search file contents with ripgrep (`pattern` is a regex; optional `path`, `glob` filter, `files_only`). The `path` argument is resolved without following symlinks before it becomes argv. Runs through the same sandboxed command path as `bash`, so content search inherits the kernel write-confinement floor. | None — read-only |
| `todo_write` | Replace the session's task checklist (every call sends the whole list). Keeps the model on-plan on long unattended runs and gives you a glanceable progress signal in the console strip. | None |
| `ask_user` | Ask you a structured multiple-choice question (a header, a prompt, and 2–4 labelled options, plus free text) when the model hits a genuine ambiguity instead of guessing. | None — see below |
| `web_fetch` | Fetch one or more URLs (an array — fetched concurrently in one call) and return their text (HTML stripped). Always registered; needs no configuration. | Ask (always, even under an allowlist) |
| `web_search` | Search via the `[web.search]` backend you configure and get back titles/URLs/snippets; `web_fetch` a result for the full text. Registered **only** when `[web.search]` is set — absent otherwise, so nothing phones home by default. | Ask |
| `spawn` | Delegate a self-contained subtask to a fresh, isolated sub-agent (`agent_type`: `general-purpose`, `explore`, `plan`, or your own `agents/*.md` def); `fork: true` seeds it with your own current context instead. See [agents.md](../agents/). | Ask |

**All five file tools are workspace-contained, and the boundary is the file descriptor.** A path is inside the working directory if — and only if — a descent from the workspace-root fd reached it *without traversing a symlink*: on Linux one `openat2(RESOLVE_BENEATH)`, everywhere else a component-by-component `openat` with `O_NOFOLLOW`. Because the check is made on the descriptor the tool then uses, there is no name to re-resolve and so no check/open race. A lexical `..`-normalising pass still runs first, but only to pick the error message and the permission — never as the boundary.

What that means per tool:

- `glob` and `grep` refuse an absolute path or a `..` escape outright, and refuse a search root reached through a symlink. This is why they run with no ask.
- `read` runs unprompted inside the tree. Outside it — an absolute path, a `..` escape, or a path that leaves through a symlink — it is a **protected ask that outranks `mode=auto`**, so it prompts in every mode. That is deliberate: an ordinary ask would be auto-approved under the shipped default and protect nothing.
- `write` and `edit` never follow a symlink at any component, including the final one, so a `docs/notes.md` that points at `~/.zshrc` is refused rather than silently writing the target. Their protected-path classification also runs on the *resolved* target, so a symlink cannot launder a protected write into an ordinary one.

A refusal is a prompt: it names the offending component and tells the model to re-issue with the absolute path and take the ask. Benign in-tree symlinks are refused too — distinguishing them would mean resolving a name and comparing it, which is the very check this design removes.

`glob` and `grep` are both parallel-safe, so a batch of several `glob`/`grep` calls in one turn runs concurrently.

`todo_write` is session-scoped ephemeral context, not part of the model transcript: the current list rides into every request as a tagged reminder, but it never becomes part of the durable conversation the model reads back verbatim. A text-only reply with `pending`/`in_progress` items still open gets nudged to finish or update the list — bounded to at most two nudges per prompt, so it can never wedge an unattended run. Sub-agents spawned with the `spawn` tool get their own independent list, wired to their own session.

`ask_user`'s permission is `None` for a specific reason, not an oversight: it is **not a permission gate**. It's a plain data-gathering round-trip — the human's answer becomes a text tool result, exactly like a `read` — so it never authorizes any mutating action on its own (see [permissions-and-sandbox.md](../permissions-and-sandbox/)). It runs under plan mode for the same reason `read`/`glob`/`grep` do: asking a clarifying question changes nothing on disk. Headless (`-p`) and JSON-mode runs have no one to ask, so the question always resolves — never hangs — to a documented "no human available" answer the model can act on. See [tui.md](../tui/#questions) for the console picker.

### Environment variables

| Variable | Overrides | Meaning |
|---|---|---|
| `HOTL_MODEL` | `[provider].model` | `provider/model`; `openai/…` covers any OpenAI-compatible endpoint. |
| `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` | — | Provider keys (never put keys in config.toml). |
| `HOTL_OPENAI_BASE_URL` | `[provider].base_url` | OpenAI-compatible endpoint. A non-loopback `http://` URL with a key set warns (cleartext). |
| `HOTL_ANTHROPIC_BASE_URL` | `[provider].base_url` | Anthropic-shaped endpoint. Both `https://host/v1` and the bare `https://host` resolve. |
| `HOTL_PROVIDER_AUTH` | `[provider].auth` | `api_key` (default) or `subscription` — see [endpoints that authenticate for you](../gateway/#endpoints-that-authenticate-for-you). |
| `HOTL_API_KEY_HELPER` | `[provider].api_key_helper` | Overrides the config.toml key of the same name. |
| `HOTL_API_KEY_HELPER_TTL_SECS` | `[provider].api_key_helper_ttl_secs` | Overrides the config.toml key of the same name. |
| `HOTL_CONTEXT_WINDOW` | `[context].window` | Context size in tokens; compaction fires at ~80%. From ~60% the summary is precomputed in the background, so the fold itself doesn't pause the session. Leave unset to get the [per-model window](#context-window-context-window). |
| `HOTL_FAST_MODEL` | `[provider].fast_model` | Cheap model for compaction summaries. |
| `HOTL_EVICT_TOKENS` | `[context].evict_tokens` | Tool-result eviction threshold (`0` disables). |
| `HOTL_PERMISSIONS` | `[permissions].mode` | `bypass` (default: no per-action asks) \| `ask` \| `dontask`; `auto` still parses as `bypass`, and a typo fails closed to `ask`. |
| `HOTL_PLAN` | `[permissions].plan` | Any value but `0`/`false`/empty turns plan mode on. |
| `HOTL_SANDBOX` | `[behavior].sandbox` | `off` disables the bash sandbox floor. `best-effort` accepts a *partial* Linux floor on kernels 5.13–6.1 (no truncate right); every ask is then labeled `sandboxed:landlock(partial)`. Unset is the hardened default. |
| `HOTL_SANDBOX_PROBE_DIR` | — | Where the startup smoke test writes its probe file. Must be writable and outside the whole write set — the working directory, `TMPDIR`, and any `[sandbox].writable` entry. Only needed on hosts where neither `/var/tmp` nor `$HOME` qualifies — otherwise the sandbox reports itself unavailable rather than unproven. |
| `HOTL_UNIX_SOCKETS` | — | `open` lifts the macOS deny on the container/orchestrator daemon socket class (`docker.sock`, `podman.sock`, `containerd`, `crio`) for docker-in-the-loop workflows. Marks every ask `unix:open`. No effect on Linux, where the deny is not expressible. |
| `HOTL_MACOS_AUTOMATION` | — | `allow` lifts the macOS Apple Events deny, for Xcode/Simulator/Instruments flows driven by AppleScript. Marks every ask `automation:allow`. |
| `HOTL_SCRUB_ENV` | — | Comma-separated extra variable names to strip from every child process's environment, on top of the provider keys stripped by default. |
| `HOTL_SCRUB_ENV_STRICT` | — | `1` also strips every variable whose name contains `KEY`/`TOKEN`/`SECRET`/`PASSWORD`/`PASSWD`/`CREDENTIAL`/`AUTH` with a value of 8+ characters. Stricter, but it will break `gh`, `cargo publish` and `npm publish`, which need their tokens. |
| `HOTL_WEB_ALLOW_METADATA` | — | `1` permits `web_fetch` to reach cloud instance-metadata addresses (`169.254.169.254`, `169.254.170.2`, `fd00:ec2::254`), which are otherwise refused on every redirect hop including the first. Nothing legitimate needs this. |
| `HOTL_PROXY_AUTH` | — | `off` drops the `Proxy-Authorization` requirement on the local egress proxy, for a client that honors `HTTP_PROXY` but discards its credentials. Without it, any local process could spend your allowlist. |
| `HOTL_MAX_TURNS` | `[behavior].max_turns` | Model steps per prompt (default 100); `-1` = unlimited. |
| `HOTL_CONCURRENCY_REQUESTS` | `[concurrency].requests` | Concurrent `web_fetch`/`web_search` HTTP requests (default 4). |
| `HOTL_CONCURRENCY_AGENTS` | `[concurrency].agents` | Concurrent sub-agent (`spawn`) sessions (default 4) — global across the parent and every child. |
| `HOTL_CONCURRENCY_SUBPROCS` | `[concurrency].subprocs` | Reserved (subprocess batching; no effect yet). |
| `HOTL_CONCURRENCY_WORKER_THREADS` | `[concurrency].worker_threads` | Reserved (tokio worker-thread pool; parsed but deliberately not wired — see below). |
| `HOTL_CONCURRENCY_BLOCKING_THREADS` | `[concurrency].blocking_threads` | `spawn_blocking` pool cap (bounds `glob`'s tree walk; default 16). |
| `HOTL_MOUSE` | `[behavior].mouse` | `0` disables console mouse capture, keeping your terminal's own drag-select and middle-click paste. Anything else leaves the wheel scrolling the transcript and drags copying. |
| `HOTL_THINKING` | *(pending `[behavior].thinking`)* | `0` turns off extended thinking. It is billed whether or not you read it, so this is the switch that matters if you don't. |
| `XDG_CONFIG_HOME` / `XDG_DATA_HOME` | — | Bases for the config dir and the session/shadow store. |

### Allow-rules (`[[allow]]`)

Auto-approve tool calls so you aren't prompted for trusted operations. Deliberately config-only — there is no in-console "always allow" (that is by design; see [permissions-and-sandbox.md](../permissions-and-sandbox/#why-allow-rules-are-a-file-you-edit)).

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
- Any write to a protected (execute-later) path — always asks. See [permissions-and-sandbox.md](../permissions-and-sandbox/#protected-paths).

### Deny-rules (`[[deny]]`)

Same schema as `[[allow]]`, opposite meaning: a match refuses the call outright,
before any allow tier, any mode, and any `bypass`. Deny outranks everything, and
unlike an allow rule it also governs the tools that never prompt at all
(`read`, `glob`, `grep`).

```toml
[[deny]]
tool = "read"
path_prefix = "~/Documents/tax"    # also denied to shell commands — see below

[[deny]]
tool = "bash"
prefix = "curl "                   # hotl's bash tool only
```

**A deny rule that names a real directory also becomes a kernel read-deny**, so
it reaches `bash` and not just hotl's own file tools — the third tier of
[the read carve](../permissions-and-sandbox/#the-read-carve). Which shapes reach
the kernel, and which stay in-process, is the `path_prefix` distinction below.
`hotl doctor` lists both.

#### `path_prefix`, three forms

| Form | Matches | Reaches shell commands? |
|---|---|---|
| `/Volumes/secrets` — **absolute** | only at the filesystem root | **yes** (if the path exists) |
| `~/.ssh` — **home-rooted** | expands against `$HOME`, then anchors at the root | **yes** (if the path exists) |
| `.ssh/` — **floating relative** | that component sequence at *any* depth: `.ssh/id_rsa`, `src/../.ssh/config`, `/Users/you/.ssh/authorized_keys` | no |

Matching is on whole components, so `.ssh/` never matches `.sshfs/`. On the deny
side the floating form deliberately over-matches: catching a path you did not
mean costs an ask, while missing one costs the secret. It has no kernel
expression, though — that would mean enumerating every matching directory on the
disk at startup and silently missing any created later — so hotl reports it
rather than approximating. Name the directory (`~/.ssh`) to cover shell commands
too.

`~/` expansion applies to both sections. On the allow side that means a
`path_prefix = "~/x"` rule begins auto-approving under `$HOME/x`, which is what
it always read as granting but did not do.

`path_prefix = "/"` and `path_prefix = ""` are refused at the kernel with a
message naming the rule: each would deny every read and leave the agent unable
to run anything. They still deny in-process.

### MCP servers (`[[mcp]]`)

Declare external tool servers. Each is exposed to the model through one `mcp` tool; the **first** use of a server prompts you to approve its binary (shown with its SHA-256), and a changed binary re-prompts. Server output is sanitized before it reaches the model. Full guide: [mcp.md](../mcp/).

### Post-edit diagnostics (`[diagnostics]`) and hooks (`[[hook]]`)

`[diagnostics]` runs a check command after a successful `edit`/`write` (under the sandbox floor, 30 s timeout). `[[hook]]` intercepts tool calls. Full guide: [hooks.md](../hooks/).

### Network egress (`[network]`)

Restricts what `bash` commands (and diagnostics/hooks, which run under the same floor) may reach over the network. `egress` is one of `open` (default; unrestricted), `off` (loopback and unix-domain sockets only), or `allowlist` (loopback plus the effective allowlist, reached through a local filtering proxy). `allow` entries are hostnames or `*.domain` wildcards — a wildcard matches the apex and any subdomain depth; no ports; matching is case-insensitive. An unknown `egress` value fails closed to `off` with a startup warning. While a restriction is configured, the bash ask label carries `net:off` / `net:allow(N)` — or `NET:UNENFORCED(reason)` on hosts where the kernel cannot back it (Linux needs kernel ≥ 6.7 for Landlock net; `HOTL_SANDBOX=off` also unenforces it), in which case `bash` allow-rules stop auto-approving. Why and limits: [permissions-and-sandbox.md](../permissions-and-sandbox/#opting-out-of-open-egress).

**The effective allowlist is hotl's starter list plus your `allow`**, deduped on the normalized host, starter entries first. `defaults = false` drops the starter list, so the allowlist is exactly what you wrote. `hotl doctor` prints the effective list split by source.

The starter list — 19 exact hosts, never wildcards, because a default nobody can enumerate is a default nobody can audit:

```
crates.io  static.crates.io  index.crates.io  static.rust-lang.org
sh.rustup.rs  docs.rs
registry.npmjs.org  registry.yarnpkg.com
pypi.org  files.pythonhosted.org
proxy.golang.org  sum.golang.org
rubygems.org
github.com  api.github.com  codeload.github.com
objects.githubusercontent.com  raw.githubusercontent.com  gitlab.com
```

It bounds accidents and drive-by fetches; it is **not** an anti-exfiltration control — `github.com` is bidirectional and a gist push leaves through it.

A host outside the effective list **asks** on an interactive surface (`[y] allow for this session · [n] deny`) and returns `hotl egress: "HOST" is not in [network].allow` when denied, when nobody answers within two minutes, or when there is no human to ask — headless and sub-agents never get the prompt. The ask is skipped for hosts that were on screen in a call you approved this turn; see [permissions-and-sandbox.md](../permissions-and-sandbox/#a-blocked-host-is-a-question-not-a-dead-end).

### Sandbox write floor (`[sandbox]`)

Widens the kernel write floor — the "deny all file writes, then re-allow the working directory, temp, and `/dev`" confinement every sandboxed command runs under — with directories you name. This is for tools that keep their caches outside the workspace: bazel, ccache, sccache, and friends fail under the default floor because their first write lands in `~/.cache`-style paths.

```toml
[sandbox]
writable = ["~/Library/Caches/bazel", "~/.bazel_disk_cache"]
file_tools = "workspace"   # optional; see below
```

`writable` entries are absolute paths (`~/` expands). Each is created if missing (Landlock can only grant access to a directory that exists when the sandbox is built), canonicalized (symlinks resolved), and validated:

- **Refused — hotl's own directories.** An entry that is, contains, or sits inside the config dir (`~/.config/hotl`) or data dir (`~/.local/share/hotl`) is dropped with a warning, and the rest are honored. A writable config dir would let a sandboxed command rewrite the allow-rules, hooks, and `api_key_helper` that govern it — self-granted privilege escalation. This is also why `~` and `/` can never be listed: they contain the config dir.
- **Warned but honored — system roots.** `/etc`, `/usr`, `/bin`, `/opt`, `/Library`, and similar are accepted with a loud warning: binaries and configuration living there become writable to every sandboxed command.
- **Skipped** — relative paths, entries that are files, entries that cannot be created or resolved. Each with its own warning; a bad entry never takes the rest down.

The startup probe that certifies the sandbox stays honest: its outside-the-floor target is chosen outside the *widened* set, so an `Enforced` verdict always describes the floor your commands actually get. `hotl doctor` prints the resolved list and every validation warning.

#### `readable` — lifting the credential read-deny

Sandboxed commands cannot read `~/.ssh`, `~/.aws`, `~/.config/gcloud`, `~/.azure`, or `.netrc` / `.npmrc` / `.pypirc` / `.dockercfg` — see [the read carve](../permissions-and-sandbox/#the-read-carve). `readable` lifts named paths back out of that denial, and is the only lever that reaches diagnostics, hooks, and `grep`, none of which have a prompt.

```toml
[sandbox]
readable = ["~/.aws"]
```

Validation mirrors `writable` — absolute paths, `~/` expands, symlinks resolved, and an entry that is, contains, or sits inside hotl's config or data dir is **refused** (that tier is never liftable, by config or by prompt). Two deliberate differences:

- **Missing directories are not created.** A read-deny on a path that does not exist is already a no-op, and conjuring `~/.ssh` out of parsing your config would be wrong. A missing entry is dropped with a warning.
- **An entry inside a writable root is dropped, loudly.** Landlock resolves the closest matching rule, so a write grant on an ancestor re-opens the read regardless of what the deny set says; shipping the denial anyway would be a claim the kernel does not honor.

`[sandbox]` is installed once at startup, so changing `readable` needs a session restart. For a one-off, press `s` instead of `y` at the `bash` ask — that lifts the credential tier for that single command. Once `readable` has emptied the tier, every ask is labeled `reads:open`. `hotl doctor` prints the resolved deny set and every warning.

`readable` and `[[deny]]` rules are complementary, not two ways to do one thing:
`readable` **subtracts** from the credential tier, and a projectable
[`[[deny]]`](#deny-rules-deny) **adds** a third tier. Neither can reach the
other's — `readable` cannot lift a deny rule (a deny is a "never"), and a deny
rule cannot un-lift a `readable` entry it does not name. Both are honored, and
`s` lifts only the credential tier.

`file_tools` is a separate, deliberate step. By default (`"workspace"`) the `write`/`edit` file tools stay confined to the working directory — `writable` only widens what *spawned processes* (bash, grep, diagnostics, hooks) may write. Set `file_tools = "writable"` to let `write`/`edit` operate under the `writable` roots too: those writes become ordinary asks (the same tier as an in-workspace write, so `mode = "bypass"` approves them), they go through the same symlink-refusing descent as workspace writes, and protected filenames ([protected paths](../permissions-and-sandbox/#protected-paths)) still escalate. An unknown value falls back to `"workspace"` with a warning.

### Web tools (`web_fetch` / `web_search`, `[web]`)

`web_fetch` reads one or more URLs as text — pass an array to fetch several pages in one call, concurrently (bounded by `[concurrency].requests`, default 4). It needs no configuration and is always registered. `web_search` is backend-pluggable: hotl ships no built-in search endpoint, so it stays **absent from the registry** until you set `[web.search]` — nothing phones home by default, the same discipline as `recall`/MCP. Point `url` at a JSON search API you run or subscribe to (SearXNG, Brave, Tavily, an internal endpoint); its response is mapped to `{title, url, snippet}` rows, tolerant of a few common field-name shapes. The API key is named by `api_key_env` — an environment variable, never a literal key in config.toml.

Both tools honor the *same* `[network]` egress policy `bash` does — there is exactly one egress authority, never a second allowlist. With `egress = "off"` both refuse every host outright; with `"allowlist"`, a host outside `allow` fails closed with a message telling you to add it. Even when a fetch is allowed, it still asks (network side effects can exfiltrate via the URL itself) — the ask names every host in the batch.

Every byte a fetch or search returns enters the model inside the untrusted-content envelope, tagged with its source (`web:<host>`) — web content is data the model can use to inform its work, never an instruction it can act on unprompted, the same treatment `spawn` and `recall` results get.

### Minified reads and edits (`[minify]`)

Reading source files is usually an agent's largest token expense, and a good
share of a source file is typography: indentation, blank lines, alignment.
`read` with `minified: true` serves a **token-stream re-serialization** instead
— the file parsed with a tree-sitter grammar, its leaf tokens re-joined with the
smallest separators that preserve meaning.

```toml
[minify]
# enable = true          # false makes `minified: true` serve the plain view
# keep_comments = true   # false strips comments (lossy — see below)
```

Supported: `.rs`, `.go`, `.py`/`.pyi`, `.js`/`.mjs`/`.cjs`/`.jsx`,
`.ts`/`.mts`/`.cts`, `.tsx`. Anything else falls back to the plain view.

**What it actually saves.** Measured on hotl's own source: **20–26%** with
comments kept, **44–59%** with them stripped. Small or comment-light files save
less (10–18% kept). It is not a uniform win and the headline is not one number —
the trailer on every minified read reports the real figure for that file. Two
honesty notes: these are *byte* savings run through hotl's flat ~3 chars/token
estimator, and a real BPE tokenizer encodes a newline-plus-indent run as roughly
one token, so the token saving is smaller than the byte saving. And a JSX-heavy
`.tsx` file saves only on its non-JSX portion, because JSX whitespace is
renderer-visible and is copied through untouched.

**`keep_comments` defaults to `true`,** because comments are meaning and
stripping them is the lossy mode. Turn it off when you want the larger saving and
accept that the model is reading code with the *why* removed.

**Not whitespace-stripping.** Some languages are whitespace-*sensitive*: Python's
indentation is syntax, and Go and JavaScript insert implicit semicolons at line
breaks. So Python keeps one logical line per line with indentation renormalized
to one space per level, and Go and JS/TS get explicit `;` at the statement
boundaries where the source relied on automatic insertion — read from the parse
tree, not guessed. Every minified view is then re-parsed and its named-node
structure compared against the source's; a mismatch is a refusal, not a warning.

**Whole file only.** `offset`/`limit` are refused with an error naming the plain
read. They are raw-file line numbers, and the minified view has no lines the
model can count, so paging in that coordinate system would ask the model to name
positions it cannot see. A view over the 200KB cap falls back to the plain paged
read.

**Every failure serves the plain view with a note saying why** — no grammar for
the extension, a file that does not parse, the minifier declining its own output,
`enable = false`. The feature can cost you savings; it cannot cost you access.
The note matters as much as the fallback: it is how you notice a grammar has
gone stale.

**Editing through the view.** Text quoted from a minified read will not match a
plain `edit` — the whitespace differs. Pass `minified: true` to `edit` as well:
`old_string` is matched in the minified view, the match is projected back to
exact source byte offsets, and only those bytes are replaced. **The file on disk
keeps all its comments, indentation and formatting; it is never written in
minified form.** Matching is exact and must be unique in that view (the domain is
already whitespace-normalized, so tolerant matching would only blur uniqueness).
Two refusals to expect: a multi-line `new_string` in Python, because it would
land at a source column the view never showed; and any splice that would leave
the file no longer parsing, checked before the write, so nothing is written.

One caveat worth knowing: `new_string` lands in the file in the spelling you
wrote it, so a minified-style replacement stays minified-style in that one spot.
For Rust, `cargo fmt` heals it at commit time.

**Build footprint.** The tree-sitter grammars compile C. They sit behind a
default-on `minify` cargo feature, so `cargo install hotl --no-default-features`
is a pure-Rust build; in that build the `minified` argument is not advertised in
the tool schema at all.

### Context window (`[context] window`)

`[context] window` sets the token budget compaction triggers against (at 80%). **Leave it unset unless you need to override it** — hotl looks the window up per model, so the Claude Opus family gets its 1M window and Haiku 4.5 gets its 200K, without you telling it. A model hotl doesn't recognize (any local or gateway model) falls back to 200,000 and prints a warning naming this setting.

Precedence, highest first: `HOTL_CONTEXT_WINDOW` → `[context] window` → the model's known window → 200,000.

```toml
[context]
window = 8192   # only needed for a model hotl doesn't recognize, or a
                # gateway that trims the window below the model's own
```

Setting this too high overflows the model mid-turn; too low burns a summarize call and discards context you were still paying to keep.

### Concurrency (`[concurrency]`)

The shared budget that bounds concurrent external work, one process-wide instance shared by the parent session and every sub-agent it spawns:

- `requests` caps how many `web_fetch`/`web_search` HTTP calls run at once (a batch of 20 URLs never opens more than `requests` sockets simultaneously; default 4).
- `agents` caps how many `spawn` children run their expensive step (the LLM call) at once — a model that issues 30 `spawn` calls in one batch still only runs `agents` at a time; the rest queue rather than stampeding the provider (default 4). Two *mutating* children that share your working directory are serialized on top of that cap; children with `isolation = "worktree"` are not. See [agents.md](../agents/).
- `subprocs` is reserved config surface for upcoming subprocess-batching work; setting it has no effect yet.
- `blocking_threads` caps the tokio blocking-thread pool (default 16) — the pool `glob`'s tree walk uses; tokio's own unconfigured default is 512.
- `worker_threads` is parsed for completeness but stays deliberately inert: it only applies to a multi-threaded async runtime, and hotl runs a single-threaded (`current_thread`) runtime everywhere by design (switching would risk breaking `!Send` futures in the TUI/actor code). Setting it logs a startup warning noting it has no effect.

### Retention (`[retention]`)

Bounds the growth of the session/shadow/blob stores. `hotl gc` prunes on demand; with a `[retention]` policy set, a prune also runs quietly at startup. See [`hotl gc`](#hotl-gc).

### History (`[history]`)

The console's prompt history — recalled with `↑`/`↓` and searched with `Ctrl-R` ([tui.md](../tui/)) — persisted as JSONL at `<xdg-data>/hotl/history.jsonl` (or a `path` you set, `~` expanded). Both caps bound the file: it is trimmed to satisfy `max_entries` **and** `max_bytes` (the smaller wins), oldest first, at startup — so the on-disk file is self-bounding, not just the in-session ring. Only prompts that start a turn are written (not steers or slash-commands); consecutive duplicates are collapsed. `enabled = false` keeps recall working within the running session but reads and writes nothing on disk.

## Admin preapproved rules

`/etc/hotl/preapproved.toml` lets a machine admin pre-approve or refuse tool
use for every hotl user. Same syntax as your `[[allow]]` rules, plus a lock:

    lock_user_allows = false   # true: your own [[allow]] rules are ignored

    [[allow]]
    tool = "bash"
    prefix = "git "

    [[deny]]
    tool = "bash"
    prefix = "curl "

hotl trusts the file only when it is owned by root and not group/world-
writable (`sudo chown root /etc/hotl/preapproved.toml && sudo chmod 644
/etc/hotl/preapproved.toml`); otherwise it is refused with a startup warning
and a `hotl doctor` row. Grants show in the transcript tagged `admin:`.
Protected paths outrank admin grants; admin denies outrank everything.

## hotl gc

`hotl gc [--dry-run] [--days N] [--keep N]` prunes whole sessions (log + evicted-result blobs + shadow snapshot repo) older than `max_age_days` or beyond `max_sessions`, and sweeps dead backgrounded-session sockets. Flags override `[retention]`. With no policy and no flags it's a no-op that tells you so. `--dry-run` lists what would go without deleting.

## hotl mcp

Inspects the servers declared in `[[mcp]]` and the grants recorded in `trust.toml`.

| Command | Effect |
|---|---|
| `hotl mcp [list]` | Every configured server, its command line, and what the trust gate will do with it. Also warns about a corrupt `trust.toml` and about grants whose server has left the config. |
| `hotl mcp show <name>` | The exact fingerprint text the in-session approval screen shows, plus the recorded key. Reads the binary; never starts it. |
| `hotl mcp add <name> <command> [args...]` | Prints a `[[mcp]]` block to paste into `config.toml`. **Writes nothing.** |
| `hotl mcp untrust <name>\|--all` | Drops the grant, so the server is screened again on next use. |
| `hotl mcp test <name>` | Starts the server, does the MCP handshake, lists its tools. Screens first if it is not already trusted, and records nothing either way. |

Four trust states appear in `list`:

- **trusted** — a grant is on file and the program still matches it; no screen.
- **screens on first use** — no grant, or the program changed since the grant.
- **unreadable binary — will ask every time** — the binary could not be hashed, so no integrity check applies. Grants are never recorded or honoured for it.
- **session-only (workspace script — never persisted)** — a file-resolving argument lives inside the agent-writable workspace, so trust is deliberately not persisted across sessions.

### Why it cannot grant trust

`hotl mcp` is read-mostly on purpose, and the two limits are the design rather than an omission:

**It never writes `config.toml`.** `add` prints a block for you to paste. A CLI that edited config would be a path `bash -c 'hotl mcp add …'` could take, and the permission layer does not cover that — its bash analysis reads redirects, `tee`, and `dd`, not a program that writes config as a side effect. Only the kernel sandbox stops it, and incidentally rather than by design.

**There is no verb that grants trust.** Trust is recorded only by a human answering the in-session fingerprint screen. A CLI grant would be the "always allow" that hotl deliberately omits everywhere else. `untrust` is the one mutation this command makes, because revocation only ever *reduces* privilege.

If you need non-interactive provisioning, register servers with a pasted `[[mcp]]` block and let each machine screen them once. See [permissions-and-sandbox](../permissions-and-sandbox/).

## Headless (`-p` / `--json`)

`hotl -p "PROMPT"` runs one turn and exits. Because no human is present, **every permission ask is auto-denied** — headless runs cannot perform gated actions unless an allow-rule covers them. Configure `[[allow]]` rules in config.toml for anything a headless run must do.

`hotl -p -` reads the prompt from stdin instead (`git log | hotl -p -`); a bare
`-p` with something piped in does the same. A terminal with no prompt is still
a usage error rather than a silent wait.

`--json` emits one JSON object per line — a machine contract, not a log. Every
frame carries `"schema_version"` and a `"type"`: `text_delta`,
`thinking_delta` (with its `text`), `tool_start`, `tool_done`, `tool_denied`,
`tool_auto_allowed`, `retrying`, `fallback_model`, `prompt_queued`,
`compacted`, `todos_changed`, `ask_denied`, `question_no_human`, and a terminal
`turn_done` carrying token usage and a tagged outcome:

```json
{"type":"turn_done","outcome":{"kind":"done","text":"…"},"usage":{…},"schema_version":2}
```

`outcome.kind` is one of `done`, `cancelled`, `turn_limit`, `refused`,
`doom_loop` (with `pattern`), `tool_failure_budget` (with `tool`), or `error`
(with `message`).

**Schema version 2 is a breaking change.** In v1 `outcome` was a Rust `Debug`
string (`"Done { text: \"…\" }"`), which no parser could read reliably.
Consumers pinned to v1 must update.

### Exit codes

| Code | Meaning |
|---|---|
| `0` | The turn completed (`Done`). |
| `130` | Interrupted (`Ctrl-C` / cancelled). |
| `1` | Any other outcome: error, refusal, turn-limit, doom-loop, tool-failure-budget, or a `doctor` FAIL. |
| `2` | Bad usage, a reserved subcommand (`fleet`), or the console with no TTY. |

## Data at rest

| Path | Contents |
|---|---|
| `~/.local/share/hotl/sessions/<ulid>.jsonl` | Append-only session logs. Permanent by design. Secret-named env values are masked at write time; the log is otherwise sensitive — treat it as such. |
| `~/.local/share/hotl/shadow/<ulid>.git` | Per-session git snapshots backing `hotl undo`. Secret-bearing files (`.env`, `*.pem`, `*.key`, `id_*`, `.ssh/`, `.aws/`, `.npmrc`, `.pypirc`, `.netrc`, `secrets.*`, `credentials`) are excluded. No automatic cleanup yet. |

**Engine defaults (not user-configurable via env yet):** max 25 turns per prompt, 32000 max output tokens, adaptive thinking on, static prompt caching on, a tool that fails 5 times consecutively stops the turn.

**See also:** [mcp.md](../mcp/) for connecting MCP tool servers, [hooks.md](../hooks/) for diagnostics and hooks, and [uninstall.md](../uninstall/) for removal.
