---
title: 'Configuration reference — hotl the agent'
description: Reference for every hotl subcommand, config file key, environment variable, and exit code.
---

Reference for the command surface, config files, and environment variables of the `hotl` agent. For a guided first run see [quickstart.md](../quickstart/); for the reasoning behind the safety model see [permissions-and-sandbox.md](../permissions-and-sandbox/). All paths are literal; `~` is the invoking user's home.

## Subcommands

| Command | Effect |
|---|---|
| `hotl [id-prefix\|--resume]` | The full-screen console: streaming transcript, activity strip, modal asks, vim input. Needs a terminal (no TTY → exit 2, use `-p`). `hotl -r --search <text>` lists only the sessions whose transcript mentions the text, with an excerpt, before you pick. See [tui.md](../tui/). |
| `hotl -p "PROMPT"` | Headless one-shot: run PROMPT to completion, print the answer, exit. See [Headless](#headless--p----json). |
| `hotl -p "PROMPT" --json` | Headless with a JSONL event stream on stdout instead of prose. |
| `hotl resume [id-prefix]` | Continue an earlier session in the console (bare: pick from a numbered list). The seeded session replays the earlier one's full context from its log and ancestry. With `-p` it is the headless resume (see [Headless](#headless--p----json)). |
| `hotl --fork-from <n\|id\|name\|@last>` | Start a **new** session seeded with another's history, pinned to that session's state at fork time (the original can keep running). `--keep-turns <n>` / `--keep <items>` fork at a prefix instead of the head; `@last` is the newest session. Works headless too (`hotl -p "…" --fork-from …`). See [sessions.md](../sessions/). |
| `hotl bg [prompt]` | Background a session as a detached socket server; `hotl attach` to reach it. See [backgrounding.md](../backgrounding/). |
| `hotl attach [id]` | Connect to a backgrounded session (bare: list live ones). |
| `hotl gc [--dry-run] [--days N] [--keep N]` | Prune old sessions/blobs per `[retention]`; in a git repo, also sweep session worktrees. See [below](#hotl-gc). |
| `hotl setup [--force]` | Write a commented starter `config.toml` (never overwrites without `--force`). |
| `hotl doctor` | Non-mutating checks: provider/keys, sandbox, config, allow-rules, session store, MCP trust, memory, secrets audit. Exit 1 if any check FAILs. |
| `hotl init zsh\|bash\|fish` | Shell integration to stdout. zsh: the `:` prefix plugin plus tab completion (`eval "$(hotl init zsh)"` in `~/.zshrc`); bash and fish: completion only. See [shell.md](../shell/). |
| `hotl export <id\|name\|@last> [--md\|--html]` | Print a past session's transcript — what the model saw, in order — as Markdown (default) or a self-contained HTML page, secrets masked. |
| `hotl stats [--days N] [--json]` | Sessions, turns, tool calls, tokens and cost across every saved session, by model and by day. Cost reads `—` when any session's model is not in the price catalog. |
| `hotl skills [add\|update\|remove]` | List every discovered skill with its source; manage skill marketplaces. See [skills.md](../skills/). |
| `hotl workflows [list\|show <name> [--mermaid]\|check <file>]` | Saved workflow recipes: list them, print one as TOML or a Mermaid flowchart, or validate a plan file and print the summary its ask will show (exit 1 on errors). See [workflows.md](../workflows/). |
| `hotl mcp [list\|show\|add\|untrust\|test]` | Inspect configured MCP servers and their trust grants. Read-mostly: it never writes `config.toml`, and there is no verb that grants trust. See [below](#hotl-mcp) and [mcp.md](../mcp/). |
| `hotl watch` | The tmux dashboard (separate capability; [crates/hotl/README.md](https://github.com/nrakochy/hotl/blob/master/crates/hotl/README.md)). |
| `hotl update` | Install the latest release. `--check` only looks; `--version X.Y.Z` picks one; `-y` skips the prompt. See [updating](../updating/). |
| `hotl fleet` | Reserved (orchestrate); not built — exits 2. |
| `hotl --help` | Usage summary. |

## One config file: `config.toml`

Everything hand-editable lives in **`~/.config/hotl/config.toml`** (or `$XDG_CONFIG_HOME/hotl/config.toml`; on Windows, **`%LOCALAPPDATA%\hotl\config\config.toml`**, and `XDG_CONFIG_HOME` is honored there too — which is what keeps a Git Bash session and a PowerShell session pointed at one file. Local rather than roaming `%APPDATA%` on purpose: this file names machine-specific writable directories, and roaming them would silently widen or break the sandbox floor on the other machine). `hotl setup` writes a commented starter. It's the only settings file — there is no `permissions.toml`/`mcp.toml`/`hooks.toml`; those are sections here now. A malformed file is ignored with a warning, never half-applied.

```toml
[provider]
model = "openai/gpt-5"                      # provider/model; openai/ covers any
                                            # OpenAI-compatible endpoint, and
                                            # openai-responses/<model> speaks the
                                            # OpenAI Responses API (required for
                                            # effort on OpenAI's reasoning models;
                                            # same key + base_url)
base_url = "http://localhost:11434/v1"      # endpoint for the active provider
auth = "api_key"                            # or "subscription": hotl holds no credential (requires base_url)
fast_model = "..."                          # cheap model for compaction summaries; absent = claude-haiku-4-5 on catalogued Anthropic models, else the session model
utility_model = "..."                       # model for one-off calls the session pays for but never shows
                                            # (compaction digests, goal checks); absent = fast_model, then the session model
fallback_models = ["claude-sonnet-5"]       # tried in order when the session model is unreachable (never on auth/billing errors);
                                            # at most two, same provider as `model`; no env twin
effort = "high"                             # low | medium | high | xhigh | max; absent = xhigh on catalogued Anthropic models, else the provider's default
                                            # or a per-phase table, applied at turn start:
                                            # effort = { plan = "xhigh", implement = "high", verify = "xhigh" }
api_key_helper = "..."                      # command whose trimmed stdout is the API key; beats static key env vars; 5s timeout, 64KB cap
api_key_helper_ttl_secs = 300               # re-run the helper when the cached key is older; absent = startup + auth-failure only
max_tokens = 64000                          # per-sample output cap (thinking + text); clamped to the model's catalogued maximum
cache_breakpoints = true                    # OpenAI dialects only: explicit GPT-5.6 prompt-cache breakpoints; absent = on for model names at or past gpt-5.6, off otherwise
cache_warming = true                        # keep the 5-minute prompt cache alive while a long tool runs (one cache-read request when it saves ≥ $0.05); absent = on; env twin HOTL_CACHE_WARMING
stream_idle_secs = 90                       # a stream with no byte at all (pings included) for this long is dead and retried; floor 15
thinking_stall_secs = 120                   # an open thinking block with no delta for this long (pings still coming) is dropped and retried; floor 15

[context]
window = 200000            # usually unnecessary — looked up per model; see below
evict_tokens = 20000       # offload tool results larger than this (0 disables)
evict_tokens_per_tool = { bash = 6000, grep = 6000 }   # per-tool override
keep_results = 4           # user turns whose tool results are never cleared
compaction_reset = false   # fresh-slate compaction instead of in-place
show_used_pct = false      # opt in to showing the model context-fullness each
                           # turn (default hidden: it induces premature wrap-up)
instructions_max_bytes = 32768  # AGENTS.md/CLAUDE.md (root and nested) are cut
                           # at a line past this; the model and you are both
                           # told. 0 lifts the cap

[behavior]
sandbox = true             # false disables the bash sandbox floor
vim_mode = false           # true = vim-style keys in the console's input editor
mouse = true               # false stops mouse capture: no wheel scroll, no drag
                           # select, and your terminal owns the mouse again
copy_on_select = true      # false stops a mouse drag copying to the clipboard
                           # (needs mouse = true; without capture there are no
                           # drag events to act on)
auto_title = true          # false: never spend a utility-model call naming an
                           # unnamed console session after its first turn
max_turns = 100            # model steps one prompt may spend (a tool round-trip
                           # costs one). -1 = unlimited: run until the model is
                           # done, the context fills, or you interrupt.
verify_commands = []       # extra command prefixes that count as verification
                           # when [provider] effort is a per-phase table
max_tool_calls = 0         # tool calls one SESSION may run; 0 = no cap
max_cost_usd = 0.0         # USD one SESSION may spend; 0 = no cap. Needs a
                           # catalogued model — hotl never guesses a price
ask_expiry_secs = 3600     # a parked permission ask nobody answers is denied
                           # after this many seconds; 0 = wait forever
max_jobs = 8               # background bash jobs one SESSION may run at once

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
project = true              # false: skip the repo's .agents/skills and .claude/skills
agents = true               # false: skip ~/.agents/skills
claude = true               # false: skip ~/.claude/skills and Claude plugin skills
catalog_tokens = 800        # ceiling for the always-sent skill index; absent = 2% of the window (400..10000)

[skills.marketplaces]       # extra skill sources — managed by `hotl skills`
acme = "https://github.com/acme/skills.git"   # managed checkout
team = "~/work/team-skills"                    # local, read in place

[agents]                    # sub-agent defs (see agents.md)
claude = true               # false: skip ~/.claude/agents
isolation = "none"          # "worktree": every mutating child gets its own
                            # git worktree and they run in parallel; a def's
                            # own `isolation:` frontmatter wins
prefix_stagger_ms = 5000    # identical siblings wait this long for the first
                            # one's first response byte, so their shared
                            # prefix is cached once instead of N times; 0 off
child_idle_secs = 300       # a child that sends nothing for this long is
                            # stopped and reported unverifiable; 0 off
# review_model = "…"        # a stronger model for the built-in `review`
                            # agent only; omit to inherit the session's
completion_grace_secs = 20  # after a child reports, how long it gets to exit
                            # before it is reaped — result kept either way
max_depth = 1               # how deep children may nest (1..=3): a child
                            # gets `spawn` only below this; `workflow` never
max_children = 8            # running + finished (parked) children per session

[workflows]                 # the workflow tool's fan-out caps (see workflows.md)
concurrency = 8             # agents one process runs at once across every workflow
max_agents = 1000           # agent starts one run may make; a plan can lower either

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

[formatters]               # post-edit formatters, run before the check (see hooks.md)
rs = "rustfmt --edition 2021 $FILE"         # $FILE = the shell-quoted path

[diagnostics]              # post-edit checks (see hooks.md)
rs = "cargo check -q --message-format=short"

[settings]
density = "comfortable"    # transcript spacing: compact | comfortable | spacious
measure = 110              # widest prose row, columns; 0 = full width

[settings.theme]           # palette for the console AND `hotl watch` (see tui.md)
preset = "warm"            # tokyo-night (the default) | warm | catppuccin | gruvbox | nord | dracula
accent = "#88c0d0"         # optional per-slot #rrggbb overrides: active blocked idle
                           # ink muted faint accent band
```

**`density`** controls how much room the console TUI gives the transcript
(colors live under `[settings.theme]`; the two are independent):

| Value | Blank rows | Left gutter |
|---|---|---|
| `compact` | none | none — edge to edge |
| `comfortable` *(default)* | between speakers (you, the model, the harness) | 2 columns |
| `spacious` | between every item | 4 columns |

An unrecognized value warns and falls back to `comfortable`. The gutter is
where the role spine is drawn (see [tui.md](../tui/)).

**`measure`** caps how wide a prose row may run on a wide terminal (default
`110` columns; `0` = the full width). Only prose wraps at it — your prompts,
the model's answers, notices; tool cards, code blocks and `/context` reports
always use the full width. `warm` is a low-blue
palette — paper-white ink, amber accent, terracotta — for a less clinical
feel; it's opt-in, the default stays `tokyo-night`.

**Precedence for the scalar settings: environment variable > config.toml > default.** So a `HOTL_MODEL` in the shell overrides `[provider].model`, and CI can override anything without editing the file.

### Reloading without restarting

`config.toml` is read at startup. In the console, `/reload` re-reads it and rebuilds the engine against the new file, keeping the session — the transcript, the model's context, the todos, the session name, its permission mode, and plan mode all carry forward. `hotl acp` clients reach the same thing as `session/reload_config`.

Most of the file reloads: `[provider]`, `[[allow]]`, `[[mcp]]`, `[[hook]]`, `[formatters]`, `[diagnostics]`, `[skills]`, `[agents]`, `[workflows]`, saved workflow recipes, `[context]`, `[behavior]`, `[settings]`, `system-prompt.md`. A reload that fails to parse or to select a provider changes nothing and says so — the running engine keeps serving.

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
| `memory/MEMORY.md` | Per user, hand-written; loaded into every session's starting context (capped at 16 KB), enveloped. The per-project file `/remember` writes lives in the data dir — see [Memory](#memory-memory). |
| `skills/*.md` | One procedure per file; the `skill` tool lists and loads them by name. See [skills.md](../skills/). |
| `agents/*.md` | One sub-agent definition per file — `tools`/`model`/`effort`/`isolation` frontmatter, body = system prompt. See [agents.md](../agents/). |
| `trust.toml` | Written by hotl, not you: approved MCP server fingerprints — `fp3:` for a program (binary hash, args, env, cwd), `fpu1:` for a URL server (address, headers, the token's variable name). |

### Skills (`[skills]`)

Five keys. Everything else about skills — the grouped index the agent sees,
search over collapsed sources, bodies read on demand, marketplaces, and the
`/` dispatch — lives in [skills.md](../skills/).

| Key | Effect |
|---|---|
| `project` | `false` stops the project walk — `.agents/skills` and `.claude/skills` from the working directory up to the repo root (default `true`). |
| `agents` | `false` stops reading `~/.agents/skills` (default `true`). |
| `claude` | `false` stops reading `~/.claude/skills` and Claude plugin skills (default `true`). |
| `catalog_tokens` | Token ceiling for the always-sent skill index; absent = 2 % of the model's context window, floor 400, cap 10,000. See [the catalog budget](../skills/#the-design-three-stages-of-disclosure). |
| `[skills.marketplaces]` | One `name = "<git url or local path>"` per extra skill source; managed by `hotl skills add` / `update` / `remove`. |

### Built-in tools

| Tool | Effect | Permission |
|---|---|---|
| `read` | Read a text file (2000 lines / 200KB per call; any single line over 8KB is clipped; `offset`/`limit` continue a truncated read). `minified: true` serves a smaller token-stream view of source code instead — see [`[minify]`](#minified-reads-and-edits-minify). | None inside the working directory — **outside it, a protected ask** |
| `edit` | Exact string replacement in a file. `minified: true` matches against the minified view and splices into the real file, which keeps its formatting. | Ask (protected paths escalate) |
| `write` | Write a file, creating parent directories. | Ask (protected paths escalate) |
| `bash` | Run a shell command under the sandbox floor. stdout and stderr share one pipe, so output arrives in the order the command actually wrote it; a failure ends with `[exit N]` or `[killed by SIGNAME]`. `background: true` starts the command as a **job** instead: the call returns at once with the job's files under `<data>/sessions/<session>.jobs/<id>/` — `cmd`, `pid`, `started`, `log` (stdout + stderr, written by the child), `rc` (exit code or `killed:SIG…`, absent while running) — and the literal line that stops it (`kill -- -<pid>`). The same ask, the same floor, no timeout unless `timeout_ms` is set; the model is told when the job ends through a small notification carrying the last 8 KiB of the log. `detach: true` (implies background) lets the job outlive the session; it asks like a write to an execute-later path. Jobs die with the session otherwise. `[behavior] max_jobs` caps running jobs (default 8). | Ask |
| `glob` | List files under the working directory matching a **real glob**: `*` (does not cross `/`), `**` (recurses), `?`, `[a-z]`, `{a,b}`. A pattern with no `/` matches the file name at any depth (`*.rs`). Newest-first by default (`sort`: `"mtime"` \| `"path"`), capped at 1000. Respects `.gitignore`; `.git` is never walked; symlinks are never followed. In-process — no subprocess, so it still works with no `rg` on `PATH` or when the sandbox floor degrades, and the walk runs on the blocking pool so a large or hostile tree cannot stall the runtime. | None — read-only |
| `grep` | Search file contents with ripgrep (`pattern` is a regex; optional `path`, `glob` filter, `files_only`). The `path` argument is resolved without following symlinks before it becomes argv. Runs through the same sandboxed command path as `bash`, so content search inherits the kernel write-confinement floor. | None — read-only |
| `todo_write` | Replace the session's task checklist (every call sends the whole list). Keeps the model on-plan on long unattended runs and gives you a glanceable progress signal in the console strip. | None |
| `ask_user` | Ask you a structured multiple-choice question (a header, a prompt, and 2–4 labelled options, plus free text) when the model hits a genuine ambiguity instead of guessing. | None — see below |
| `web_fetch` | Fetch one or more URLs (an array — fetched concurrently in one call) and return their text (HTML stripped). Always registered; needs no configuration. | Ask (always, even under an allowlist) |
| `web_search` | Search via the `[web.search]` backend you configure and get back titles/URLs/snippets; `web_fetch` a result for the full text. Registered **only** when `[web.search]` is set — absent otherwise, so nothing phones home by default. | Ask |
| `spawn` | Delegate a self-contained subtask to a fresh, isolated sub-agent (`agent_type`: `general-purpose`, `explore`, `plan`, or your own `agents/*.md` def); `fork: true` seeds it with your own current context instead. `background: true` returns at once with the child's id (read-only or worktree-isolated children only); the typed result arrives as a notification when it finishes. See [agents.md](../agents/). | Ask (` (background)` appended) |
| `wait` | Block until a background sub-agent finishes (`child`, or the first to finish) and return its typed result; `timeout_ms` (default 120000) with the child still running. The result then has one reader — no notification follows. | None |
| `send` | Steer a running background sub-agent: `text` lands in its context before its next sample. | None |
| `resume` | Continue a finished sub-agent with its context intact (`child`, `text`, optional `background`). Not for isolated, stopped or evicted children. | Ask (`resume sub-agent <id>: …`) |

**All five file tools are workspace-contained, and the boundary is the file descriptor.** A path is inside the working directory if — and only if — a descent from the workspace-root fd reached it *without traversing a symlink*: on Linux one `openat2(RESOLVE_BENEATH)`, everywhere else a component-by-component `openat` with `O_NOFOLLOW`. Because the check is made on the descriptor the tool then uses, there is no name to re-resolve and so no check/open race. A lexical `..`-normalising pass still runs first, but only to pick the error message and the permission — never as the boundary.

What that means per tool:

- `glob` and `grep` refuse an absolute path or a `..` escape outright, and refuse a search root reached through a symlink. This is why they run with no ask.
- `read` runs unprompted inside the tree — and an absolute path that resolves under the session root counts as inside, in every mode. Outside it — a `..` escape, an absolute path that really leaves the root, or a path that leaves through a symlink — it is **protected**: it prompts under `ask`/`dontask`/plan, and under the default `bypass` it runs with a ⚑ notice instead of a prompt. That is deliberate: an ordinary ask would be auto-approved under the shipped default and protect nothing, while a blocking prompt in bypass would park an unattended run.
- `write` and `edit` never follow a symlink at any component, including the final one, so a `docs/notes.md` that points at `~/.zshrc` is refused rather than silently writing the target. Their protected-path classification also runs on the *resolved* target, so a symlink cannot launder a protected write into an ordinary one.

A refusal is a prompt: it names the offending component and tells the model to re-issue with the absolute path and take the ask. Benign in-tree symlinks are refused too — distinguishing them would mean resolving a name and comparing it, which is the very check this design removes.

`glob` and `grep` are both parallel-safe, so a batch of several `glob`/`grep` calls in one turn runs concurrently.

`todo_write` is session-scoped ephemeral context, not part of the model transcript: the current list rides into every request as a tagged reminder, but it never becomes part of the durable conversation the model reads back verbatim. A text-only reply with `pending`/`in_progress` items still open gets nudged to finish or update the list — bounded to at most two nudges per prompt, so it can never wedge an unattended run. Sub-agents spawned with the `spawn` tool get their own independent list, wired to their own session.

`ask_user`'s permission is `None` for a specific reason, not an oversight: it is **not a permission gate**. It's a plain data-gathering round-trip — the human's answer becomes a text tool result, exactly like a `read` — so it never authorizes any mutating action on its own (see [permissions-and-sandbox.md](../permissions-and-sandbox/)). It runs under plan mode for the same reason `read`/`glob`/`grep` do: asking a clarifying question changes nothing on disk. Headless (`-p`) and JSON-mode runs have no one to ask, so the question always resolves — never hangs — to a documented "no human available" answer the model can act on. See [tui.md](../tui/#questions) for the console picker.

### Environment variables

| Variable | Overrides | Meaning |
|---|---|---|
| `HOTL_MODEL` | `[provider].model` | `provider/model`; `openai/…` covers any OpenAI-compatible endpoint, and `openai-responses/…` speaks the OpenAI Responses API — required for effort on OpenAI's reasoning models (they refuse `reasoning_effort` next to tools on chat/completions); same key and base URL sources as `openai/…`. |
| `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` | — | Provider keys (never put keys in config.toml). |
| `HOTL_PROVIDER_BASE_URL` | `[provider].base_url` | Endpoint for the active provider (any provider). Both `https://host/v1` and the bare `https://host` resolve. A non-loopback `http://` URL with a key set warns (cleartext). |
| `HOTL_OPENAI_BASE_URL` / `HOTL_ANTHROPIC_BASE_URL` | `[provider].base_url` | Legacy vendor-specific aliases for the row above; still honored, and win over the neutral name when both are set. |
| `HOTL_PROVIDER_AUTH` | `[provider].auth` | `api_key` (default) or `subscription` — see [endpoints that authenticate for you](../gateway/#endpoints-that-authenticate-for-you). |
| `HOTL_API_KEY_HELPER` | `[provider].api_key_helper` | Overrides the config.toml key of the same name. |
| `HOTL_API_KEY_HELPER_TTL_SECS` | `[provider].api_key_helper_ttl_secs` | Overrides the config.toml key of the same name. |
| `HOTL_CONTEXT_WINDOW` | `[context].window` | Context size in tokens; compaction fires at ~80%. From ~60% the summary is precomputed in the background, so the fold itself doesn't pause the session. Leave unset to get the [per-model window](#context-window-context-window). |
| `HOTL_FAST_MODEL` | `[provider].fast_model` | Cheap model for compaction summaries. Absent everywhere: catalogued Anthropic sessions digest on `claude-haiku-4-5`; anything else keeps the session model. |
| `HOTL_UTILITY_MODEL` | `[provider].utility_model` | Model for one-off housekeeping calls — compaction digests and `/goal` evaluations. Resolved `utility_model` → `fast_model` → the session model, so setting neither changes nothing. |
| `HOTL_EFFORT` | `[provider].effort` | Reasoning depth: `low` \| `medium` \| `high` \| `xhigh` \| `max`. Unset defaults to `xhigh` on catalogued Anthropic models with effort support; other models get no depth field. An unrecognized value warns and is ignored. The env var is always a scalar; the config key also takes a per-phase table. |
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
| `HOTL_MAX_TOOL_CALLS` | `[behavior].max_tool_calls` | Tool calls one **session** may run before a batch is refused. Absent or `0` = no cap. |
| `HOTL_MAX_COST_USD` | `[behavior].max_cost_usd` | USD one **session** may spend before a sample is refused pre-flight. Absent or `0` = no cap; an uncatalogued model has no price and so no cap (warned at startup). |
| `HOTL_MAX_TOKENS` | `[provider].max_tokens` | Per-sample output-token cap, thinking included (default 64000); always clamped to the model's catalogued maximum. |
| `HOTL_CONCURRENCY_REQUESTS` | `[concurrency].requests` | Concurrent `web_fetch`/`web_search` HTTP requests (default 4). |
| `HOTL_CONCURRENCY_AGENTS` | `[concurrency].agents` | Concurrent sub-agent (`spawn`) sessions (default 4) — global across the parent and every child. |
| `HOTL_CONCURRENCY_SUBPROCS` | `[concurrency].subprocs` | Reserved (subprocess batching; no effect yet). |
| `HOTL_CONCURRENCY_WORKER_THREADS` | `[concurrency].worker_threads` | Reserved (tokio worker-thread pool; parsed but deliberately not wired — see below). |
| `HOTL_CONCURRENCY_BLOCKING_THREADS` | `[concurrency].blocking_threads` | `spawn_blocking` pool cap (bounds `glob`'s tree walk; default 16). |
| `HOTL_MOUSE` | `[behavior].mouse` | `0` disables console mouse capture, keeping your terminal's own drag-select and middle-click paste. Anything else leaves the wheel scrolling the transcript and drags copying. |
| `HOTL_THINKING` | *(pending `[behavior].thinking`)* | `0` turns off extended thinking. It is billed whether or not you read it, so this is the switch that matters if you don't. |
| `XDG_CONFIG_HOME` / `XDG_DATA_HOME` | — | Bases for the config dir and the session store. |

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
- Any write to a protected (execute-later) path — never silently auto: it asks under `ask`/`dontask`/plan and runs with a ⚑ notice under `bypass`. See [permissions-and-sandbox.md](../permissions-and-sandbox/#protected-paths).

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

Declare external tool servers. Each is exposed to the model through one `mcp` tool; the **first** use of a server prompts you to approve it — a program is shown with its SHA-256, a remote server with its address and header names — and a change re-prompts. Server output is sanitized before it reaches the model. Full guide: [mcp.md](../mcp/).

| Key | Effect |
|---|---|
| `name` | How the model and `hotl mcp` refer to it. |
| `command`, `args`, `env`, `cwd` | A program to start and speak to over stdio. |
| `url` | A Streamable HTTP endpoint instead of a program — exactly one of `command` and `url`. Absolute `https://`; plain `http://` for loopback only. |
| `bearer_token_env` | The **name** of an environment variable holding the bearer token for a `url` server. Read at connect time; never written to any file hotl owns. |
| `headers` | `[["Name", "value"], …]` literal headers for a `url` server. A literal `Authorization` is refused — use `bearer_token_env`. |
| `description` | What the server is for; the model reads it to decide when to use it. |

One invalid entry loads **no** servers and warns once naming it and the fix.

### Post-edit formatters (`[formatters]`), diagnostics (`[diagnostics]`) and hooks (`[[hook]]`)

`[formatters]` runs a format command after a successful `edit`/`write`, then `[diagnostics]` runs a check command (both under the sandbox floor, 30 s timeout). `[[hook]]` intercepts tool calls. Full guide: [hooks.md](../hooks/).

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

When a sandboxed command fails on one of these denied writes, the result the model sees names the sandbox and points here — you'll be asked to add the directory rather than watching the agent work around it. Ready-to-paste set for the common toolchain caches:

```toml
# [sandbox]
# writable = ["~/.cargo", "~/.npm", "~/.cache", "~/go", "~/.gradle", "~/.m2"]
```

`writable` entries are absolute paths (`~/` expands). Each is created if missing (Landlock can only grant access to a directory that exists when the sandbox is built), canonicalized (symlinks resolved), and validated:

- **Refused — hotl's own directories.** An entry that is, contains, or sits inside the config dir (`~/.config/hotl`, or `%LOCALAPPDATA%\hotl\config`) or data dir (`~/.local/share/hotl`, or `%LOCALAPPDATA%\hotl\data`) is dropped with a warning, and the rest are honored. A writable config dir would let a sandboxed command rewrite the allow-rules, hooks, and `api_key_helper` that govern it — self-granted privilege escalation. This is also why `~` and `/` can never be listed: they contain the config dir.
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

### The context ladder (`[context] keep_results`)

hotl reclaims context cheapest-first, and only climbs to the next rung when the one below it has run out of room:

| At | Rung | What it costs |
|----|------|---------------|
| any size | **spill** — a single oversized tool result goes to a file, leaving a head+tail preview and a `read` pointer (`evict_tokens`, `evict_tokens_per_tool`) | nothing; the file is still on disk |
| 60% | **clear** — tool results older than the last `keep_results` user turns become one-line `<cleared …/>` stubs | nothing; the session log still holds every byte, and `recall` fetches one back by id |
| 80% | **fold** — earlier history is replaced by a typed summary (compaction) | a summarize call, and the detail the summary didn't keep |
| on request | `/compact [focus]` runs the same fold at any fullness ([tui.md](../tui/#folding-on-purpose)) | the same summarize call |

Clearing happens at most once per prompt and never touches the last `keep_results` turns, the turn in flight, or a `skill` result — those are the history the model is still working from. Set `keep_results = 0` to turn the rung off entirely and go straight from spilling to folding.

```toml
[context]
keep_results = 8   # a workflow that re-reads old tool output; costs context
```

Both the clear and the fold rewrite the start of the prompt, so each one costs exactly one prompt-cache miss — which is why clearing is batched into a single pass rather than one per result.

`bash` and `grep` spill at **6,000** tokens rather than the session-wide `evict_tokens`: their output is a haystack the model wanted one needle out of, unlike a `read`, which is a file it asked for in full. `evict_tokens_per_tool` merges over those two defaults — naming `bash` leaves `grep` alone — and a spilled result keeps its first 1,536 and last 512 characters, so a build log's verdict and a stack trace's cause survive alongside the command that produced them.

```toml
[context]
evict_tokens_per_tool = { bash = 12000 }   # you page through long build logs
```

### Reasoning effort (`[provider] effort`)

One neutral ladder — `low | medium | high | xhigh | max` — that each provider spells its own way. Anthropic gets `output_config.effort`; OpenAI-compatible endpoints get a top-level `reasoning_effort`. You set the rung once and hotl projects it.

Precedence, highest first: `/effort` mid-session → `HOTL_EFFORT` → `[provider] effort` → **`xhigh` (catalogued Anthropic models with effort support)** → the provider's own default.

```toml
[provider]
effort = "high"
```

**An unconfigured session on a catalogued Anthropic model with effort support runs at `xhigh`** — the agentic default, matching what coding harnesses ship for Opus-class models. Every other model — Haiku, the whole OpenAI-compatible family, gateway aliases — gets no depth field at all, so those requests are byte-for-byte what they were before this setting existed: no warm prompt cache invalidated on upgrade, and no unknown key sent to a local server that might reject it.

`/effort default` (or `unset`/`none`) clears your rung to the **provider's** default (`high` on Anthropic today) — which since the `xhigh` session default is *not* the same as never having set one. The console says so when you clear it.

A model that accepts fewer rungs clamps to its nearest one rather than erroring, and ties clamp **downward**, toward the cheaper rung: guessing upward spends your money on an inference hotl cannot justify. The OpenAI-compatible dialect accepts only `low | medium | high` on the wire, so `xhigh`/`max` clamp to `high` there. A model with no effort support at all (today: `claude-haiku-4-5`) simply gets no field. A model hotl does not recognize is never *refused* an effort — hotl allowlists no model names, so the field goes out and the provider's own answer is what you see.

`effort` and `thinking` stay two knobs. `HOTL_THINKING=0` turns extended thinking off; on an OpenAI-compatible endpoint that is spelled `reasoning_effort: "none"` and wins over any rung you set.

### The utility model (`[provider] utility_model`)

Some calls a session makes are not the conversation: the digest that compaction folds your history into, and the verdict `/goal` asks for after each turn. You pay for both and see neither, so they take their own model:

```toml
[provider]
utility_model = "claude-haiku-4-5"
```

Resolution is `utility_model` → `fast_model` → the session model, in that order, and one resolver serves both call sites — compaction and goal evaluation can never end up on different models. Neither call inherits the session's reasoning effort or extended thinking, for the same reason: housekeeping at `max` rates is a cost nobody opts into.

`/effort` in the console changes the rung mid-session and is recorded durably, so `hotl resume` keeps it. Sub-agents take their own rung from an `effort:` line in the agent def — see [agents.md](../agents/). Compaction never inherits the session's rung: folding your own history at `max` rates is a cost nobody opts into.

#### A schedule over phases

`effort` also takes a table, so the depth follows the kind of work rather than sitting at one rung all session:

```toml
[provider]
effort = { plan = "xhigh", implement = "high", verify = "xhigh" }
```

The phase is derived once, **at turn start**, so every sample inside one turn is billed at one depth:

- **`plan`** — plan mode is on.
- **`verify`** — the previous turn's last tool batch ran a test runner and edited nothing. The built-in table is `cargo test`, `cargo nextest`, `cargo check`, `cargo clippy`, `pytest`, `npm|pnpm|yarn test`, `go test`, `make test`, `just test`; add your own with `[behavior] verify_commands = ["tox", "bazel test"]`. A batch that edited a file is implementation however it ends.
- **`implement`** — everything else.

A phase you leave out inherits the session's effort, and so does a rung that names no level (it warns at startup rather than failing the file). The schedule is configuration, not a decision: it writes no durable entry, `hotl resume` re-derives it, and `HOTL_EFFORT` stays a scalar. The moment you run `/effort <rung>` the session is **pinned** and the schedule stops — a rung you set by hand outranks a rung a table chose. A bare `/effort` names the schedule while it still governs.

### Switching models (`/model`, `[provider] fallback_models`)

`/model <provider/model>` (or `session/set_model` under `hotl acp`) changes the session model for every turn from the next one on. The switch waits for the turn boundary, is written to the log, and every assistant item records the model that produced it, so `hotl resume` runs the last model you switched to and `hotl export` can tell which model wrote what. The prefix cache is cold after a switch — the next turn re-writes the whole context — and the console notice says so with the token count.

A switch across dialects (`anthropic/…` ↔ `openai/…` or `openai-responses/…`) swaps the provider under the session. History is never stripped: the other dialect's signed thinking stays in the log and is dropped only at the wire, so switching back restores it. The configured `fast_model`, `utility_model` and `fallback_models` belong to the startup provider; after a cross-dialect switch housekeeping (compaction digests, `/goal` verdicts) runs on the session model instead, and the fallback chain is empty until you switch back. Reasoning effort and permission mode are untouched. Sub-agents keep the model they were started on.

A model whose window cannot hold the current context is refused outright rather than failing on the next request — run `/compact` and try again.

`fallback_models` is the availability chain: when the session model is unreachable (a 529, a timeout — never an auth or billing error) the turn is retried on the next model in the list, and the item it produces is stamped with the fallback. Chains are short by design (see [RELIABILITY.md](https://github.com/nrakochy/hotl/blob/master/docs/RELIABILITY.md)): the first two entries are kept and a longer list warns. Entries are bare names on the session's provider; an entry with another provider's prefix is dropped with a warning, a same-provider prefix is stripped, and the session model or a duplicate is ignored. Under a catalogued session model, an uncatalogued fallback or one with a narrower window warns — compaction is sized to the session model. None of this refuses to start; it is configuration only, with no environment variable.

### Session budgets (`[behavior] max_tool_calls`, `max_cost_usd`)

Two caps that bound a whole **session**, not a turn — a fresh prompt does not refill them:

```toml
[behavior]
max_tool_calls = 500
max_cost_usd = 5.0
```

`max_tool_calls` is checked once per tool batch, before any call in it runs. A single wide batch may therefore cross the cap by its own width: this is a runaway backstop, the same posture `max_turns` takes, not an accounting ledger.

`max_cost_usd` is checked **before** each sample goes out, priced at the worst case that sample could bill — the whole prompt at input rates plus a full `max_tokens` of output. The cap needs a price, and hotl allowlists no model names, so an **uncatalogued model is never refused**: the whole OpenAI-compatible family and every gateway alias run uncapped, and hotl warns once at startup rather than guessing a rate. Cap those sessions with `max_tool_calls`, which needs no price. Compaction digests and `/goal` evaluations are billed but do not yet move this meter.

Crossing 50%, 80% or 100% of the spend budget raises one notice each — a transcript line and the console's strip meter. Either cap ending a turn produces a typed outcome (`budget` on the JSON stream) whose message names the knob to raise.

**Cache keep-alive.** A session on the 5-minute prompt cache (`-p`, sub-agents,
workflow steps) whose tool call is about to outlive it — a long test run, a
sub-agent, a background job — sends one tiny cache-read request before the
entry expires, so the next sample reads the history at the cache-read price
instead of rewriting it. It fires only while a tool is running, only when the
model's catalogued prices say the saving is at least $0.05 (about 10k cached
tokens on Opus-class models), never on the 1-hour cache the console uses,
and never on a model hotl cannot price. Each ping's tokens count toward
`max_cost_usd` and the session totals; `/cost` shows `cache kept warm N× ($)`,
`-p` prints `· cache kept warm (…)`, and `--json` emits a `cache_warm` frame.
An endpoint that rejects the one-token request is tried once and left alone.
`[provider] cache_warming = false` or `HOTL_CACHE_WARMING=0` turns it off.

### Ask expiry (`[behavior] ask_expiry_secs`)

A permission ask raised while no client is attached (`hotl bg`, `hotl serve`) is **parked**: its reply channel is held and the ask is re-issued the moment you attach. Without a deadline, a session nobody returns to holds that turn open forever. After `ask_expiry_secs` — one hour by default — a parked ask is **denied**, the denial is recorded in the session log like any other, and the turn carries on. `0` restores the old wait-forever behavior. Headless (`dontask`) sessions are unaffected: they already deny immediately.

### Denial spirals

Three denied calls in a row, or twenty in one turn, end the turn with a `denial_spiral` outcome. A denial is a human (or a rule) saying no; asking again is spending samples on an answer that is not going to change. The message names both counts, and the fix is either to change what the agent is being asked to do or to relax the rule doing the denying. This is separate from the per-tool failure budget: a denial is a decision, never a malfunction, and it has never drawn that budget down.

### Concurrency (`[concurrency]`)

The shared budget that bounds concurrent external work, one process-wide instance shared by the parent session and every sub-agent it spawns:

- `requests` caps how many `web_fetch`/`web_search` HTTP calls run at once (a batch of 20 URLs never opens more than `requests` sockets simultaneously; default 4).
- `agents` caps how many `spawn` children run their expensive step (the LLM call) at once — a model that issues 30 `spawn` calls in one batch still only runs `agents` at a time; the rest queue rather than stampeding the provider (default 4). Two *mutating* children that share your working directory are serialized on top of that cap; children with `isolation = "worktree"` are not. See [agents.md](../agents/).
- `subprocs` is reserved config surface for upcoming subprocess-batching work; setting it has no effect yet.
- `blocking_threads` caps the tokio blocking-thread pool (default 16) — the pool `glob`'s tree walk uses; tokio's own unconfigured default is 512.
- `worker_threads` is parsed for completeness but stays deliberately inert: it only applies to a multi-threaded async runtime, and hotl runs a single-threaded (`current_thread`) runtime everywhere by design (switching would risk breaking `!Send` futures in the TUI/actor code). Setting it logs a startup warning noting it has no effect.

### Workflows (`[workflows]`)

The `workflow` tool's own caps, separate from `[concurrency] agents` (which paces interactive `spawn`):

- `concurrency` is the process-wide gate: how many workflow agents run at once across every run in the process (default 8). A plan's own `concurrency` can lower its run below this, never raise it.
- `max_agents` is the most agent starts one run may make (default 1000). A plan's `max_agents` can lower it. Reaching it is a run error — hotl never truncates a run quietly.

Both are floors of one. Mutating agents that are not worktree-isolated still serialise on the shared-tree lock regardless of width. See [workflows.md](../workflows/).

### Memory (`[memory]`)

Two files load into every session's starting context, each under its own
16 KiB cap and the same untrusted envelope as a repo's `AGENTS.md`:

| File | Who writes it |
|---|---|
| `~/.config/hotl/memory/MEMORY.md` | You, by hand. Per user. |
| `<xdg-data>/hotl/memory/<project-id>/MEMORY.md` | `/remember`, after your yes. Per project — the same id the plan artifact uses, so two worktrees of one repo share it. Hand-editable. |

```toml
[memory]
mode = "read-only"   # read-only (default) | read-write | off
```

`read-only` loads both files and writes nothing: `/remember <note>` says so,
and the model has no `remember` tool. `read-write` turns the write path on:
`/remember <note>` (or `session/remember` under `hotl acp`) asks you with the
same y/n a tool ask uses, then appends `- YYYY-MM-DD: <note>` under `## Notes`
and records a `memory_appended` entry in the session log. The model may
*propose* a note through the `remember` tool; its permission gate is that
same confirm, so under `bypass` the tool is refused with the ⚑ notice rather
than auto-allowed, and no tool — not `write`, not `edit`, not `bash` — can
reach the file any other way (it lives in hotl's own data dir, Tier A). `off`
loads neither file. The dial is read at session start; a change takes effect
next session.

A note is one line of up to 2,000 characters — newlines become spaces so a
note cannot forge a heading. The new line is not shown to the model until the
next session. When the file plus the new line would pass the cap, the write
is refused and the notice names the file and its size: prune it by hand or
move old lines to another file. Nothing rolls, nothing is clipped. Bare
`/remember` prints the last entries, the file path, its size against the cap
and the dial; `hotl doctor` reports the same.

### Plan artifact (`[plan]`)

The `todo_write` checklist is a durable per-project plan. Every write renders `<xdg-data>/hotl/plans/<project-id>/current.json` (machine) and `current.md` (human), temp-then-rename, alongside the decisions the model recorded. The project id is the `origin` remote's URL when there is one, else the canonical working directory — so two worktrees of the same repo share one plan, and two unrelated checkouts do not.

A fresh session in a project whose plan still has open steps gets one reminder naming the file. A resumed session does not: it already carries the list.

- `repo_dir` mirrors `current.md` into your repo as `hotl-plan.md` (relative paths resolve against the workspace). Absent by default — the plan lives in the data dir, and committing it is your choice. Only the markdown is mirrored; the JSON is harness state.

```toml
[plan]
repo_dir = "docs/exec-plans/active"
```

### Retention (`[retention]`)

Bounds the growth of the session/blob stores. `hotl gc` prunes on demand; with a `[retention]` policy set, a prune also runs quietly at startup. See [`hotl gc`](#hotl-gc).

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

`hotl gc [--dry-run] [--days N] [--keep N]` prunes whole sessions (log + evicted-result blobs, plus any legacy `shadow/<ulid>.git` left by releases before 0.26) older than `max_age_days` or beyond `max_sessions`, and sweeps dead backgrounded-session sockets. Flags override `[retention]`. With no policy and no flags it's a no-op that tells you so. `--dry-run` lists what would go without deleting.

Run inside a git repository, `hotl gc` also sweeps the session worktrees under `.git/hotl-worktrees/` (`hotl -w`, see [sessions.md](../sessions/#worktrees)), with or without a retention policy. Each one lands in one of three bins:

| Bin | Rule | What happens |
|---|---|---|
| live | the lock names a pid that is running | kept: `kept fix-auth: live (session 01JC…)` |
| clean, session gone | not live, the session's log is gone, nothing uncommitted, and the branch is merged or empty | removed without an ask, branch deleted (`branch -d`) |
| everything else | the session log still exists, or uncommitted changes, or commits not on your branch | asks `fix-auth (fix-auth): 3 commits not on master, dirty — remove? [y/N]` (default no); `--dry-run` or no terminal lists it and deletes nothing |

The no-ask bin needs a session's log to be gone, which is `[retention]`'s doing — without a policy, every worktree goes through the ask. An unmerged branch is never deleted. The startup prune never sweeps worktrees.

## hotl mcp

Inspects the servers declared in `[[mcp]]` and the grants recorded in `trust.toml`.

| Command | Effect |
|---|---|
| `hotl mcp [list]` | Every configured server, its command line, and what the trust gate will do with it. Also warns about a corrupt `trust.toml` and about grants whose server has left the config. |
| `hotl mcp show <name>` | The exact fingerprint text the in-session approval screen shows, plus the recorded key. Reads the binary; never starts it. |
| `hotl mcp add <name> <command> [args...]` | Prints a `[[mcp]]` block to paste into `config.toml`. **Writes nothing.** |
| `hotl mcp add <name> --url <url> [--bearer-env VAR] [--header NAME=VALUE]…` | The same block for a remote server; the token is named by its environment variable, never pasted. |
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

`hotl -p "PROMPT"` runs one turn and exits. Because no human is present, **every permission ask is auto-denied** — headless runs cannot perform gated actions unless an allow-rule covers them. Configure `[[allow]]` rules in config.toml for anything a headless run must do. Memory is read-only in `-p` whatever `[memory] mode` says: there is nobody to confirm a write, so the `remember` tool is not offered.

`hotl -p -` reads the prompt from stdin instead (`git log | hotl -p -`); a bare
`-p` with something piped in does the same. A terminal with no prompt is still
a usage error rather than a silent wait.

**`--resume <id|name|@last>`** continues a session from a script the way `-r`
does from the console (`hotl resume <s> -p "…"` is the same thing): the
chain's name, mode, plan, effort, todos and goal carry over, an explicit `-n`
or `--goal` replaces the inherited one, and each run adds one log to the
chain. List numbers are console-only. See [Unattended
loops](../sessions/#unattended-loops-cron) for the cron shape.

**`--ephemeral`** runs the prompt and keeps no session: the log lives in a temp
dir for the run and is removed at exit, and sub-agents of that run vanish with
it. `recall`, eviction and replay work as usual while it runs. `hotl gc` and
the secrets audit still run against the real store — they are about the store,
not this run. Not available with `--json-schema`.

**`--json-schema <file>`** makes the answer a single JSON object valid against
the schema in `file`: the contract rides in as a tagged context item, the
answer is checked here against your schema as written, and a mismatch is fed
back for up to two retries. On a retry the turn also asks Anthropic for the
schema directly (`output_config.format`), so the corrected answer is
grammar-constrained — on models that support structured outputs, with a copy
of the schema that has `additionalProperties: false` on every object and the
numeric/string constraints stripped (the check here still enforces them). The
first attempt is byte-for-byte what it was, a schema with `$ref` keeps the
plain retry, and an endpoint that rejects the field is tried once and left
alone. Other dialects keep the retry loop as it was.

`--json` emits one JSON object per line — a machine contract, not a log. Every
frame carries `"schema_version"` and a `"type"`: `text_delta`,
`thinking_delta` (with its `text`), `tool_start`, `tool_done`, `tool_denied`,
`tool_auto_allowed`, `child_tool` (a sub-agent's tool activity, with the
spawn call's `parent_id`, a `phase` of `start`/`done`, and `ok` on done
frames only), `retrying`, `fallback_model`, `prompt_queued`,
`compacted` (now also with `focus`, `usage` and `estimate`), `fold_skipped`,
`model_changed` (with `model`, `previous`, `context_tokens` and
`context_window` — the cache cost of the switch),
`todos_changed`, `ask_denied`, `question_no_human`, `cache_warm` (a cache
keep-alive ping's `usage`, with `cost_usd` when the model has a price), and a
terminal
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

The shell's twin of `turn_done.outcome.kind`: a script can tell "a cap I set
stopped it" from "the harness pulled a brake" from "it broke" without parsing
anything.

| Code | Meaning | `outcome.kind` |
|---|---|---|
| `0` | Done. A `--goal` run: the verdict was `met`, or there was no goal. | `done` |
| `1` | Failed, or the model refused; a `doctor` FAIL; a `--goal` run whose last verdict was `impossible` or `errored`. | `error`, `refused` |
| `2` | Bad usage, a reserved subcommand (`fleet`), or the console with no TTY. | — |
| `3` | Plan mode changed the working tree. | — |
| `4` | Stopped by a **cap you set**: `max_turns`, `max_tool_calls`, `max_cost_usd`. | `turn_limit`, `budget` |
| `5` | Stopped by a **brake the harness pulled**: doom loop, tool-failure budget, denial spiral; a `--goal` run whose last verdict was `stalled`. | `doom_loop`, `tool_failure_budget`, `denial_spiral` |
| `130` | Interrupted (`Ctrl-C` / cancelled). | `cancelled` |

The stderr sentence for a cap or a brake names its exit code, so a cron log
explains its own status.

## Data at rest

| Path | Contents |
|---|---|
| `~/.local/share/hotl/sessions/<ulid>.jsonl` | Append-only session logs. Permanent by design. Secret-named env values are masked at write time; the log is otherwise sensitive — treat it as such. An `--ephemeral` run writes nothing here. |

**Engine defaults (not user-configurable via env yet):** max 25 turns per prompt, 32000 max output tokens, adaptive thinking on, static prompt caching on, a tool that fails 5 times consecutively stops the turn.

**See also:** [mcp.md](../mcp/) for connecting MCP tool servers, [hooks.md](../hooks/) for diagnostics and hooks, and [uninstall.md](../uninstall/) for removal.
