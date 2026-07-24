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
| `hotl undo` | Restore workspace files to before the most recent session's last mutating step. Confirm-gated; `--force`/`-f` skips the prompt. |
| `hotl bg [prompt]` | Background a session as a detached socket server; `hotl attach` to reach it. See [backgrounding.md](../backgrounding/). |
| `hotl attach [id]` | Connect to a backgrounded session (bare: list live ones). |
| `hotl gc [--dry-run] [--days N] [--keep N]` | Prune old sessions/shadows/blobs per `[retention]`. See [below](#hotl-gc). |
| `hotl setup [--force]` | Write a commented starter `config.toml` (never overwrites without `--force`). |
| `hotl doctor` | Non-mutating checks: provider/keys, sandbox, config, allow-rules, session store, memory, secrets audit, undo/git. Exit 1 if any check FAILs. |
| `hotl init zsh` | Print the zsh `:` prefix plugin to stdout; `eval "$(hotl init zsh)"` in `~/.zshrc` makes a line starting `: ` run as an agent prompt. |
| `hotl watch` | The tmux dashboard (separate capability; [crates/hotl/README.md](https://github.com/nrakochy/hotl/blob/master/crates/hotl/README.md)). |
| `hotl update [ver]` | Print the version + how to update (compares against `ver` if given). |
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
window = 200000            # your model's context size in tokens
evict_tokens = 20000       # offload tool results larger than this (0 disables)
compaction_reset = false   # fresh-slate compaction instead of in-place
show_used_pct = true       # show context-fullness in each turn's status

[behavior]
sandbox = true             # false disables the bash sandbox floor
vim_mode = false           # true = vim-style keys in the console's input editor

[permissions]
mode = "auto"   # "auto" | "ask" | "plan" | "dontask"
                # auto: no per-action y/N; protected paths + sandbox still guard.
                # ask: approve every mutating/executing call.
                # plan: read-only until you approve a plan (see permissions-and-sandbox.md).
                # dontask: never wait for input — deny anything not pre-approved (the -p/CI posture).
                # A security-enforced build ignores this key entirely (ask stays on).

[network]
egress = "open"            # "open" | "off" | "allowlist" (bash network egress)
allow = ["github.com", "*.crates.io"]   # hosts reachable in allowlist mode

[web.search]                # optional: enables web_search (absent by default)
url = "https://s.example/api"   # a JSON search API you run/subscribe to
api_key_env = "SEARCH_KEY"      # name of an env var holding the key (never the key itself)
result_cap = 8                  # max results per search (default 8)

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

### Other files (not "config", so not in config.toml)

| File | Purpose |
|---|---|
| `system-prompt.md` | Replaces the built-in agent instructions (prose). |
| `memory/MEMORY.md` | Loaded into every session's starting context (capped at 16 KB), enveloped. |
| `skills/*.md` | One procedure per file; the `skill` tool lists and loads them by name. |
| `agents/*.md` | One sub-agent definition per file — `tools`/`model`/`effort` frontmatter, body = system prompt. See [agents.md](../agents/). |

**Claude Code skills load too.** If you have skills in the Claude format —
`~/.claude/skills/<name>/SKILL.md`, or plugin skills under
`~/.claude/plugins/cache/` (highest installed version per plugin) — the
`skill` tool reads them in place: the body loads on demand prefixed with its
base directory so `references/` and `scripts/` paths resolve (scripts still
run through the normal bash gate and sandbox). Bare names prefer hotl's own
skills, then your marketplaces, then your Claude skills, then plugins; a
plugin skill is always also reachable as `plugin:skill`. Opt out with:

    [skills]
    claude = false

#### How the agent finds a skill

Skills stay out of the context until they are used. What the model is shown
on every request is a grouped index — one line per source, with descriptions
left out entirely, and any source over 12 skills collapsed to a few names
plus a count:

    hotl: deploy, release
    claude: auth, go-service, system-shape, vps-cluster
    claude:superpowers (14): brainstorming, executing-plans, writing-plans, +11 more

On a 24-skill roster that index measures about 150 tokens where the old full
roster took 980 — and it grows per *source*, so registering a 300-skill
marketplace adds one line, not 300 names. From there the agent has three
moves:

| Call | What it does |
|---|---|
| `{"name": "deploy"}` | Loads that skill. The usual call. |
| `{"query": "review a pull request"}` | Searches every skill's full description — **including collapsed ones** — and returns the best 8. |
| `{"source": "superpowers"}` | Lists one source in full. |

Calling it with no arguments still lists everything.

A collapsed skill is hidden, not unreachable: search covers the whole roster,
so `query` finds skills the index never named. `hotl skills` always prints
every skill with its full description — the human view never collapses.

**Forcing one yourself.** In the console TUI, type `/` and the skill name:

    /brainstorming redesign the skill system

Built-in commands (`/rename`) win the name; anything else is looked up as a
skill, with the rest of the line passed along as arguments. An unrecognised
name stays an unknown-command notice and never reaches the model. This is the
manual override for the times the agent doesn't think to search.
| `trust.toml` | Written by hotl, not you: approved MCP server binary hashes. |

### Skill marketplaces

Register extra skill sources — any git repo or local directory containing
`SKILL.md` skills:

```toml
[skills.marketplaces]
acme = "https://github.com/acme/skills.git"   # managed checkout
team = "~/work/team-skills"                   # local, read in place
```

Git sources are cloned by `hotl skills add acme <url>` (or `hotl skills
update` for an entry added by hand) into `~/.config/hotl/marketplaces/<name>`
and refreshed only by `hotl skills update` — never at startup. `hotl skills`
lists every discovered skill with its source; `hotl skills remove <name>`
unregisters one. Skills are discovered up to four directory levels below
the root, so flat (`<skill>/SKILL.md`) and plugin-repo
(`plugins/<p>/skills/<s>/SKILL.md`) layouts both work. A skill whose bare
name is taken stays addressable as `<marketplace>:<skill>`.

### Built-in tools

| Tool | Effect | Permission |
|---|---|---|
| `read` | Read a text file (2000 lines / 200KB per call, `offset` continues a truncated read). | None — read-only |
| `edit` | Exact string replacement in a file. | Ask (protected paths escalate) |
| `write` | Write a file, creating parent directories. | Ask (protected paths escalate) |
| `bash` | Run a shell command under the sandbox floor. | Ask |
| `glob` | List files under the working directory matching a filename pattern (`*.rs`, `**/*.toml`, or a bare substring); hidden/vendor directories (`.git`, `node_modules`, `target`) are skipped. In-process — no subprocess, so it still works with no `rg` on `PATH` or when the sandbox floor degrades. | None — read-only |
| `grep` | Search file contents with ripgrep (`pattern` is a regex; optional `path`, `glob` filter, `files_only`). Runs through the same sandboxed command path as `bash`, so content search inherits the kernel write-confinement floor. | None — read-only |
| `todo_write` | Replace the session's task checklist (every call sends the whole list). Keeps the model on-plan on long unattended runs and gives you a glanceable progress signal in the console strip. | None |
| `ask_user` | Ask you a structured multiple-choice question (a header, a prompt, and 2–4 labelled options, plus free text) when the model hits a genuine ambiguity instead of guessing. | None — see below |
| `web_fetch` | Fetch one or more URLs (an array — fetched concurrently in one call) and return their text (HTML stripped). Always registered; needs no configuration. | Ask (always, even under an allowlist) |
| `web_search` | Search via the `[web.search]` backend you configure and get back titles/URLs/snippets; `web_fetch` a result for the full text. Registered **only** when `[web.search]` is set — absent otherwise, so nothing phones home by default. | Ask |
| `spawn` | Delegate a self-contained subtask to a fresh, isolated sub-agent (`agent_type`: `general-purpose`, `explore`, `plan`, or your own `agents/*.md` def); `fork: true` seeds it with your own current context instead. See [agents.md](../agents/). | Ask |

`glob` and `grep` are workspace-scoped: an absolute path or a `..` escape outside the working directory is refused, and both run without a permission ask because that containment is what makes them safe reads. Both are parallel-safe, so a batch of several `glob`/`grep` calls in one turn runs concurrently.

`todo_write` is session-scoped ephemeral context, not part of the model transcript: the current list rides into every request as a tagged reminder, but it never becomes part of the durable conversation the model reads back verbatim. A text-only reply with `pending`/`in_progress` items still open gets nudged to finish or update the list — bounded to at most two nudges per prompt, so it can never wedge an unattended run. Sub-agents spawned with the `spawn` tool get their own independent list, wired to their own session.

`ask_user`'s permission is `None` for a specific reason, not an oversight: it is **not a permission gate**. It's a plain data-gathering round-trip — the human's answer becomes a text tool result, exactly like a `read` — so it never authorizes any mutating action on its own (see [permissions-and-sandbox.md](../permissions-and-sandbox/)). It runs during plan mode for the same reason `read`/`glob`/`grep` do: asking a clarifying question changes nothing on disk. Headless (`-p`) and JSON-mode runs have no one to ask, so the question always resolves — never hangs — to a documented "no human available" answer the model can act on. See [tui.md](../tui/#questions) for the console picker.

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
| `HOTL_CONTEXT_WINDOW` | `[context].window` | Context size in tokens; compaction fires at ~80%. From ~60% the summary is precomputed in the background, so the fold itself doesn't pause the session. |
| `HOTL_FAST_MODEL` | `[provider].fast_model` | Cheap model for compaction summaries. |
| `HOTL_EVICT_TOKENS` | `[context].evict_tokens` | Tool-result eviction threshold (`0` disables). |
| `HOTL_PERMISSIONS` | `[permissions].mode` | `auto` (default: no per-action asks) \| `ask` \| `plan` \| `dontask`; a typo fails closed to `ask`. |
| `HOTL_SANDBOX` | `[behavior].sandbox` | `off` disables the bash sandbox floor. |
| `HOTL_CONCURRENCY_REQUESTS` | `[concurrency].requests` | Concurrent `web_fetch`/`web_search` HTTP requests (default 4). |
| `HOTL_CONCURRENCY_AGENTS` | `[concurrency].agents` | Concurrent sub-agent (`spawn`) sessions (default 4) — global across the parent and every child. |
| `HOTL_CONCURRENCY_SUBPROCS` | `[concurrency].subprocs` | Reserved (subprocess batching; no effect yet). |
| `HOTL_CONCURRENCY_WORKER_THREADS` | `[concurrency].worker_threads` | Reserved (tokio worker-thread pool; parsed but deliberately not wired — see below). |
| `HOTL_CONCURRENCY_BLOCKING_THREADS` | `[concurrency].blocking_threads` | `spawn_blocking` pool cap (bounds `glob`'s tree walk; default 16). |
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

### MCP servers (`[[mcp]]`)

Declare external tool servers. Each is exposed to the model through one `mcp` tool; the **first** use of a server prompts you to approve its binary (shown with its SHA-256), and a changed binary re-prompts. Server output is sanitized before it reaches the model. Full guide: [mcp.md](../mcp/).

### Post-edit diagnostics (`[diagnostics]`) and hooks (`[[hook]]`)

`[diagnostics]` runs a check command after a successful `edit`/`write` (under the sandbox floor, 30 s timeout). `[[hook]]` intercepts tool calls. Full guide: [hooks.md](../hooks/).

### Network egress (`[network]`)

Restricts what `bash` commands (and diagnostics/hooks, which run under the same floor) may reach over the network. `egress` is one of `open` (default; unrestricted), `off` (loopback and unix-domain sockets only), or `allowlist` (loopback plus the hosts in `allow`, reached through a local filtering proxy). `allow` entries are hostnames or `*.domain` wildcards — a wildcard matches the apex and any subdomain depth; no ports; matching is case-insensitive; an empty list allows nothing. An unknown `egress` value fails closed to `off` with a startup warning. While a restriction is configured, the bash ask label carries `net:off` / `net:allow(N)` — or `NET:UNENFORCED(reason)` on hosts where the kernel cannot back it (Linux needs kernel ≥ 6.7 for Landlock net; `HOTL_SANDBOX=off` also unenforces it), in which case `bash` allow-rules stop auto-approving. A denied fetch returns `hotl egress: "HOST" is not in [network].allow`. Why and limits: [permissions-and-sandbox.md](../permissions-and-sandbox/#opting-out-of-open-egress).

### Web tools (`web_fetch` / `web_search`, `[web]`)

`web_fetch` reads one or more URLs as text — pass an array to fetch several pages in one call, concurrently (bounded by `[concurrency].requests`, default 4). It needs no configuration and is always registered. `web_search` is backend-pluggable: hotl ships no built-in search endpoint, so it stays **absent from the registry** until you set `[web.search]` — nothing phones home by default, the same discipline as `recall`/MCP. Point `url` at a JSON search API you run or subscribe to (SearXNG, Brave, Tavily, an internal endpoint); its response is mapped to `{title, url, snippet}` rows, tolerant of a few common field-name shapes. The API key is named by `api_key_env` — an environment variable, never a literal key in config.toml.

Both tools honor the *same* `[network]` egress policy `bash` does — there is exactly one egress authority, never a second allowlist. With `egress = "off"` both refuse every host outright; with `"allowlist"`, a host outside `allow` fails closed with a message telling you to add it. Even when a fetch is allowed, it still asks (network side effects can exfiltrate via the URL itself) — the ask names every host in the batch.

Every byte a fetch or search returns enters the model inside the untrusted-content envelope, tagged with its source (`web:<host>`) — web content is data the model can use to inform its work, never an instruction it can act on unprompted, the same treatment `spawn` and `recall` results get.

### Concurrency (`[concurrency]`)

The shared budget that bounds concurrent external work, one process-wide instance shared by the parent session and every sub-agent it spawns:

- `requests` caps how many `web_fetch`/`web_search` HTTP calls run at once (a batch of 20 URLs never opens more than `requests` sockets simultaneously; default 4).
- `agents` caps how many `spawn` children run their expensive step (the LLM call) at once — a model that issues 30 `spawn` calls in one batch still only runs `agents` at a time; the rest queue rather than stampeding the provider (default 4). See [agents.md](../agents/).
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

**See also:** [mcp.md](../mcp/) for connecting MCP tool servers, [hooks.md](../hooks/) for diagnostics and hooks, and [uninstall.md](../uninstall/) for removal.
