# SECURITY.md — stance

**Defaults are the safety design.** Enforcement ships ON with a curated default policy. A cautionary example: a well-built policy engine behind a default-off flag with an allow-all policy file is equivalent to nothing.

This document describes the controls as they exist in the code today. Gaps are listed at the end, loudly, not hidden.

## What the sandbox is not (read this first)

The kernel sandbox floor is **write-confinement, not data-loss prevention.** A `bash` command that the human approves (or that an allow-rule matches) can **read any file the user can read and send it anywhere over the network** — reads and network egress are open by design (the agent legitimately reads the tree and fetches dependencies). The floor stops the agent *tampering with the filesystem outside the working directory*; it does **not** stop *exfiltration*. Treat the human approval prompt, not the sandbox, as the exfiltration boundary — and know that a plausible-looking approved command (`run the tests`, which also `curl`s) exfiltrates freely. A network-egress allowlist is a planned control (see gaps); until it exists, do not run hotl against secrets you would not paste into a command yourself.

## The permission gate

Every mutating or executing tool call passes one fixed pipeline before it runs:

1. **PreToolUse hooks** (in-process, then owner-configured shell hooks) may deny or rewrite the call. A rewritten call **re-enters the gate** — a hook cannot launder a call past the ask.
2. **Allow rules** (`[[allow]]` in `~/.config/hotl/config.toml`) may auto-approve it, narrated. Rules are deliberately editor-written only — there is no in-REPL "always allow," so ask-fatigue cannot manufacture an ungoverned allowlist. Rule matching defends against shell-operator smuggling after an allowed prefix (`ls && curl …` does not match an `ls` rule) and `..` path traversal.
3. **Protected paths** are checked *before* allow rules and **never auto-approve**. Writes that could execute later outside any gate escalate the ask with a *why* warning. The class covers: `.git/hooks/`, Makefile-class files (`Makefile`, `justfile`, `build.rs`, `conftest.py`, `*.gyp`), agent-instruction files (`AGENTS.md`, `CLAUDE.md`), harness/editor settings (`.hotl/`, `.claude/`, `settings.json`), shell rc files, `.ssh/`, credential stores (`.aws/`, `.config/gcloud/`, `.azure/`, `.npmrc`, `.pypirc`, `.netrc`, `.dockercfg`), git config, and cron/systemd units.
4. **The human ask** — y/N with the sandbox status in the prompt. Headless (`-p`, `--json`, or non-TTY stdin) **default-denies immediately**: nothing interactive ever blocks or leaks a prompt into CI logs. Interactive asks deny on timeout (`HOTL_ASK_TIMEOUT`, default 300 s).

Asks are durable: a `pending_ask` entry is committed to the session log before the question is surfaced and an `ask_resolved` entry after — a crash mid-ask is visible on replay, never silently resolved.

A repetition detector (doom-loop) halts a turn that repeats the same tool-call cycle and asks the human whether to continue; a per-tool consecutive-failure budget ends turns that keep failing the same way.

## The kernel sandbox floor

`bash` executes confined — **Seatbelt** on macOS (deny all file writes, then re-allow the working directory, temp, and `/dev`), **Landlock** on Linux ≥ 5.13 including WSL2 (same shape). Reads and network stay open (see "what the sandbox is not").

Hosts where the floor is unavailable (older Linux kernels) **degrade fail-closed**: every exec is individually human-gated with an `UNSANDBOXED` banner in the ask, and `bash` allow-rules stop applying — auto-approval of commands exists only while the sandbox is enforced. `HOTL_SANDBOX=off` is an explicit escape hatch and is labeled as such in every ask. Windows native is unsupported (no floor designed); WSL2 is the Windows path.

Owner-configured shell hooks run under the same floor.

## Untrusted input → model context

Everything that flows into the model's context from a source other than the user is wrapped in an **untrusted-content envelope**: a provenance-tagged wrapper (`trust="untrusted"`, `source=…`) carrying an explicit non-authority statement — the content cannot authorize tool use, override the user's instructions, or change the rules — with closing-delimiter defang (a zero-width space inserted into `</`) so the content cannot fake its own closing tag.

| Untrusted path | Control |
|---|---|
| repo instruction files (`AGENTS.md`/`CLAUDE.md`, incl. nested) → context | untrusted-content envelope |
| auto-memory files → context | same envelope; clipped to a 16 KB load budget |
| MCP server output → context | sanitizer chokepoint (below) |
| sub-agent result → parent context | `<subagent-result trust="untrusted">` envelope |
| bash/tool output → context | human gated the *command*; output enters context unsanitized — the model treats tool results as data by system-prompt instruction only (see gaps) |

## MCP

**Sanitizer — one named chokepoint.** Every string a server returns — call results, `tools/list` listings (names, descriptions, schemas), and errors — passes `hotl_mcp::sanitize` before entering the transcript; a code path that skips it is a bug by definition. Transforms, in order: (1) strip ANSI escapes and C0 control characters except `\n`/`\t` (terminal-injection defense); (2) enforce a 50 KB per-result byte cap with an explicit `[truncated N bytes]` marker (context-flooding defense); (3) wrap in the untrusted-content envelope with `source="mcp:<server>/<tool>"` (prompt-injection defense). Tool listings load only on demand (deferred loading), and a `tools/list_changed` notification only marks the cache stale — the refreshed listing re-passes the sanitizer, and every MCP call remains gated per call; new tools never auto-run.

**Trust store — first-use screen.** The first call to a server raises a *protected* ask (never auto-allowable): server name, binary path, SHA-256 of the binary, and what approval means ("this program will run on your machine and its output will enter the model's context"). Approval is recorded in `~/.config/hotl/trust.toml` keyed by server name → binary hash; a changed hash re-raises the screen. An unreadable binary is recorded honestly as having no integrity check; a failed trust-store write keeps the grant in memory only and re-asks next session. Server binaries run **outside** the bash sandbox floor — they are user-installed programs, not model-directed commands; installing one is the trust decision.

## Sub-agents and protocol clients

**`spawn` (sub-agents).** The child has **no human on the loop**, so its permission asks default-deny — it runs only auto-allowed/read-only tools under the parent's sandbox floor and rules. The depth cap is **structural, not a counter**: children are built with a builtins-only tool registry — no `spawn`, no MCP — so a child cannot recurse or reach external servers; the capability simply isn't in its registry. Results return to the parent inside the untrusted envelope. `fork` and `teammate` are reserved topologies.

**`hotl acp` (protocol surface).** The connected client answers `session/request_permission` round-trips — it *is* the human-on-the-loop for that session, exactly like the REPL. A missing or malformed reply, or a client that hangs up, resolves to deny.

## Hooks

Two lanes, both owner-authored in `~/.config/hotl/config.toml` — hotl does not load configuration from the repository it runs in, so a repo cannot ship hooks or settings that change behavior:

- **In-process hooks** (`PreToolUse`/`PostToolUse`), payload-capped.
- **Shell-command hooks** — JSON over stdio, run under the sandbox floor with a 10 s timeout. Three consecutive failures evict the hook for the session. Malformed output is a no-op: **fail-open on the decision, never on permission** — a broken hook cannot grant, only fail to block.

## Data at rest

| Artifact | Location | Control |
|---|---|---|
| session log (append-only JSONL, permanent by design) | `~/.local/share/hotl/sessions/` | secret masking at ingestion: values of secret-named env vars (`KEY`/`TOKEN`/`SECRET`/`PASSWORD`/`CREDENTIAL`/`AUTH`…, ≥ 8 chars) are replaced with `«masked:NAME»` — including their JSON-escaped forms — before bytes land |
| evicted oversized tool results | `<session>.blobs/` | same masking; files written `0600`; blob filenames sanitized against path injection |
| shadow snapshot store (powers `undo`) | per-session bare git repo | secret-bearing files are **excluded entirely, not masked** (`.env*`, `*.pem`, `*.key`, `id_*`, `*.p12`/`*.pfx`, `.ssh/`, `.aws/`, `.npmrc`, `.pypirc`, `.netrc`, `secrets.*`, `credentials`) — git history would keep a transient secret alive after the workspace file is deleted or rotated, so credentials never enter |

The log carries a hash chain: replay verifies each entry chains to its parent and warns if a log was edited or truncated after being written. A secrets audit flags older logs that still contain a *current* secret value (append-only means they can't be scrubbed — the remedy is rotation, and the tool says so).

Retention is explicit: `hotl gc` (with `--dry-run`) and a `[retention]` policy (`max_age_days` / `max_sessions`) prune whole sessions — log, blobs, and shadow repo together. The default is keep-everything; a configured policy also runs automatically at startup.

## `hotl watch`

A single-user tool on a single-user assumption. It runs `ps` (every user's process command lines) and `tmux capture-pane` (whatever is on screen); on a shared host these can surface other users' secrets (`mysql -pPASSWORD`, `--token=…`) and scrollback. All `ps`/`tmux` calls use argv arrays — no shell interpolation, so no command injection — making this local information disclosure inherent to a process dashboard, not an execution risk. Don't run it on a host where you shouldn't see other users' process arguments.

## Known gaps (planned, not shipped)

- **No network-egress allowlist.** The sandbox leaves network open; the approval prompt is the only exfiltration control. This is the largest gap — see "what the sandbox is not."
- **Native tool output is not sanitized.** bash/read results enter context verbatim; only MCP output passes the sanitizer chokepoint. The system prompt instructs the model to treat tool results as data — an instruction, not an enforcement.
- **The permission pipeline has no AST or LLM inspectors.** Command scanning is heuristic (shell-operator detection), not tree-sitter-based; there are no LLM judges voting on calls.
- **No third-party extension trust screens.** Moot today — hooks and settings load only from owner config, never from the repo — but required before any repo-supplied or third-party extension lane ships.
- **Browser/WASM profile does not exist** and has no kernel sandbox story yet; it will not ship without compensating controls.

## Standing rules

- Tool descriptions must not promise protections the executor doesn't implement — tested as an invariant.
- Supply chain: pinned deps; SHA-pinned remote installs default ON; lifecycle-script allowlists.
- No telemetry. Secret-scrubbing in logs stays. Crash dumps are local, secret-scrubbed, and only ever shared manually by the user; the update *check* defaults off.

## Reporting a vulnerability

GitHub private security advisories on the repo, or email the owner (address in the repo README). Coordinated disclosure, 90-day default window. Report before publishing; good-faith research against your own installation is welcome.
