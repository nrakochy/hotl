# SECURITY.md — stance

**The floor is the safety design.** What ships ON in every mode and cannot be configured off: the kernel sandbox, protected-path escalations, deny rules, undo snapshots, secret masking, and transcript visibility of every silenced prompt. Per-action *prompting* is a mode (see "Permission modes" below): opt-in for the daily driver, mandatory in the `security-enforced` build. The cautionary example still binds — a control that can silently lapse is equivalent to nothing — which is why the floor has no off switch and every mode change is visible at startup.

This document describes the controls as they exist in the code today. Gaps are listed at the end.

## Permission modes

Prompting is a *mode*, not the identity of the tool. The trust boundary moves
with it:

| | default build, `mode="auto"` (default) | default build, `mode="ask"` | `security-enforced` build |
|---|---|---|---|
| Ordinary bash/write/edit/MCP | runs, no prompt, `ToolAutoAllowed` in transcript | y/N ask per action | y/N ask per action (config cannot change this) |
| Protected execute-later paths | **always asks** (headless: denies) | always asks | always asks |
| Admin preapproved (`/etc/hotl/preapproved.toml`) | grants apply (redundant under auto) | grants silence matching asks | grants are the admin's no-prompt channel |
| Admin/user deny rules | refuse the call outright, with the rule named in the tool result | same | same |
| Kernel sandbox / egress / undo / masking | on | on | on |

In `auto`, the boundary is **sandbox + protected asks + deny rules + undo**,
not per-action approval. The README's "safety" claim holds unconditionally
only for the `security-enforced` build; the default build's floor is the row
above. `/etc/hotl/preapproved.toml` is trusted only when root-owned and not
group/world-writable; otherwise it is refused loudly at startup and in
`hotl doctor`.

## What the sandbox is not (read this first)

The kernel sandbox floor is **write-confinement, not data-loss prevention.** A `bash` command that the human approves (or that an allow-rule matches) can **read any file the user can read and send it anywhere over the network** — reads and network egress are open by design (the agent legitimately reads the tree and fetches dependencies). The floor stops the agent *tampering with the filesystem outside the working directory*; it does **not** stop *exfiltration*. Treat the human approval prompt, not the sandbox, as the exfiltration boundary — and know that a plausible-looking approved command (`run the tests`, which also `curl`s) exfiltrates freely. Egress restriction exists but is **opt-in** (`[network]` in config.toml — see "Network egress" below); the default is open. Under the default policy, do not run hotl against secrets you would not paste into a command yourself.

## The permission gate

Every mutating or executing tool call passes one fixed pipeline before it runs:

1. **PreToolUse hooks** (in-process, then owner-configured shell hooks) may deny or rewrite the call. A rewritten call **re-enters the gate** — a hook cannot launder a call past the ask.
2. **Allow rules** (`[[allow]]` in `~/.config/hotl/config.toml`) may auto-approve it, narrated. Rules are deliberately editor-written only — there is no in-REPL "always allow," so ask-fatigue cannot manufacture an ungoverned allowlist. Rule matching defends against shell-operator smuggling after an allowed prefix (`ls && curl …` does not match an `ls` rule) and `..` path traversal.
3. **Protected paths** are checked *before* allow rules and **never auto-approve**. Writes that could execute later outside any gate escalate the ask with a *why* warning. The class covers: `.git/hooks/`, Makefile-class files (`Makefile`, `justfile`, `build.rs`, `conftest.py`, `*.gyp`), agent-instruction files (`AGENTS.md`, `CLAUDE.md`), harness/editor settings (`.hotl/`, `.claude/`, `settings.json`), shell rc files, `.ssh/`, credential stores (`.aws/`, `.config/gcloud/`, `.azure/`, `.npmrc`, `.pypirc`, `.netrc`, `.dockercfg`), git config, and cron/systemd units.
4. **The human ask** — y/N with the sandbox status in the prompt. Headless (`-p`, `--json`, or non-TTY stdin) **default-denies immediately**: nothing interactive ever blocks or leaks a prompt into CI logs. Interactive asks deny on timeout (`HOTL_ASK_TIMEOUT`, default 300 s).

Asks are durable: a `pending_ask` entry is committed to the session log before the question is surfaced and an `ask_resolved` entry after — a crash mid-ask is visible on replay, never silently resolved.

A repetition detector (doom-loop) halts a turn that repeats the same tool-call cycle and asks the human whether to continue; a per-tool consecutive-failure budget ends turns that keep failing the same way.

## The kernel sandbox floor

`bash` executes confined — **Seatbelt** on macOS (deny all file writes, then re-allow the working directory, temp, and `/dev`), **Landlock** on Linux ≥ 5.13 including WSL2 (same shape). Reads stay open; network egress is open by default and restrictable per host (see "Network egress" below and "what the sandbox is not").

Hosts where the floor is unavailable (older Linux kernels) **degrade fail-closed**: every exec is individually human-gated with an `UNSANDBOXED` banner in the ask, and `bash` allow-rules stop applying — auto-approval of commands exists only while the sandbox is enforced. `HOTL_SANDBOX=off` is an explicit escape hatch and is labeled as such in every ask. Windows native is unsupported (no floor designed); WSL2 is the Windows path.

Owner-configured shell hooks run under the same floor.

### Network egress

`[network].egress` in `~/.config/hotl/config.toml` selects one of three modes. The default is **open** — egress hardening is opt-in. That is a considered decision, not an oversight:

- A restricted default breaks the first prompt a new user runs: `cargo build`, `npm install`, `git clone` all reach the network, and `allowlist` mode breaks **git over SSH remotes unconditionally** (SSH does not speak the HTTP proxy, and the kernel blocks the direct connection) — no list fixes that.
- A default allowlist generous enough to not break workflows (`github.com`, the package registries) is itself an exfiltration channel — an agent that can reach github.com can push to an attacker's repo. Shipping that as the default would manufacture a false sense of security while adding friction: the worst combination.
- On Linux kernels < 6.7 a restricted default would land every user in `NET:UNENFORCED` — disabling their bash allow-rules by the fail-closed rule below — based on kernel version alone.

So the human gate (not the sandbox) stays the *default* exfiltration boundary, and egress restriction is the opt-in structural backstop for running against material where that gate alone is not acceptable; "what the sandbox is not" tells you when that is. The control is **connection-granular, not payload-aware**: it restricts *all* egress to unlisted hosts (legitimate fetches included) and none to listed ones (exfiltration included) — it narrows destinations, it does not classify traffic.

- **`open`** (default) — egress unrestricted; exactly the behavior described above.
- **`off`** — no egress: the kernel confines the command to loopback and unix-domain sockets.
- **`allowlist`** (`allow = ["github.com", "*.crates.io"]`) — the same kernel loopback-only confinement, plus a local filtering HTTP proxy for the listed hosts. Matching is case-insensitive and host-granular (no ports, no paths); `*.example.com` matches the apex and any subdomain depth; an empty list allows nothing.

**Kernel backing.** macOS: Seatbelt network clauses — deny all network, then re-allow unix-domain sockets and loopback. Linux: Landlock net (ABI v4, kernel ≥ 6.7), handled as a **hard requirement** — `ConnectTcp` with zero allowed ports for `off`, exactly the proxy port for `allowlist`; a kernel without the net ABI can never silently skip net enforcement.

**The proxy is not the control; the kernel is.** The proxy (127.0.0.1, ephemeral port) filters `CONNECT` and absolute-form HTTP by host for *cooperating* clients — those honoring the `HTTP(S)_PROXY`/`ALL_PROXY` variables hotl injects into the command's environment (curl, git, pip, cargo…). A non-cooperating client that ignores the proxy env hits the kernel loopback-only wall and **fails closed**. A denied request gets a `403` whose body — `hotl egress: "HOST" is not in [network].allow` — is an errors-as-prompts message the model sees in tool output.

**Degradation is fail-closed**, mirroring the UNSANDBOXED posture: when `off`/`allowlist` is configured but the kernel can't back it (no seatbelt, Landlock without the net ABI, `HOTL_SANDBOX=off`), every bash ask is loudly marked `NET:UNENFORCED(reason)` and bash allow-rules stop auto-approving. An unknown `egress` value fails closed to `off` with a startup warning — a typo never means open. While a restriction is active and enforced, the ask label carries `net:off` or `net:allow(N)`.

**Honest limits.**
- macOS: DNS resolution rides the mDNSResponder unix-domain socket, which stays allowed — name *resolution* still works under `off`/`allowlist` and is not exfil-confined (a DNS tunnel can leak data even in `off` mode).
- Linux: Landlock net is **TCP-only** — UDP, including DNS and DNS-tunnel exfiltration, is not confined — and **port-scoped, not address-scoped**: the proxy port *number* is connectable on any host, and `off` blocks loopback TCP too (unix-domain sockets stay open).
- The allowlist is host-granular: an allowed host is fully reachable, any path, any method — and therefore also usable as an exfiltration destination (an allowed `github.com` accepts pushes to any repo). List hosts you trust with your data, not merely hosts you fetch from.
- The proxy is HTTP-only: `git` over SSH remotes, and any other non-HTTP protocol, cannot traverse it — under `off`/`allowlist` they fail at the kernel wall regardless of the list. Use HTTPS remotes when running restricted.

## Untrusted input → model context

Everything that flows into the model's context from a source other than the user is wrapped in an **untrusted-content envelope**: a provenance-tagged wrapper (`trust="untrusted"`, `source=…`) carrying an explicit non-authority statement — the content cannot authorize tool use, override the user's instructions, or change the rules — with closing-delimiter defang (a zero-width space inserted into `</`) so the content cannot fake its own closing tag.

| Untrusted path | Control |
|---|---|
| repo instruction files (`AGENTS.md`/`CLAUDE.md`, incl. nested) → context | untrusted-content envelope |
| auto-memory files → context | same envelope; clipped to a 16 KB load budget |
| MCP server output → context | sanitizer chokepoint (below) |
| sub-agent result → parent context | `<subagent-result trust="untrusted">` envelope |
| bash/tool output → context | human gated the *command*; output enters context unsanitized — the model treats tool results as data by system-prompt instruction only (see gaps) |
| api_key_helper command (config/env) → key | editor-written planes only (config.toml is a protected path); runs as harness infrastructure outside the tool sandbox, never model-initiated; stdout registered with the ingestion masker (startup key), stderr console-only; **caveat:** auth-error response bodies from the provider/gateway are persisted in the session log — the startup helper key is masked, but a key *refreshed* mid-session is not re-registered with the masker, so a gateway that echoes keys in auth-error bodies would persist that refreshed key in the log |

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

- **No egress ask.** A host not on the allowlist gets a flat 403; there is no y/N ask ("bash wants to reach `host` — allow for this session?") the way tool permissions have. That interaction is what would make `allowlist` livable as the *default* — the first `cargo build` would ask once about crates.io instead of failing — and is the recorded path to flipping the egress default, along with a story for the SSH gap. Until it ships, egress restriction stays opt-in.
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
