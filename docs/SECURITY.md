# SECURITY.md — stance

**The floor is the safety design.** What ships ON in every mode and cannot be configured off: the kernel sandbox, protected-path escalations, deny rules, undo snapshots, secret masking, and transcript visibility of every silenced prompt. Per-action *prompting* is a mode (see "Permission modes" below): opt-in for the daily driver, mandatory in the `security-enforced` build. The cautionary example still binds — a control that can silently lapse is equivalent to nothing — which is why the floor has no off switch and every mode change is visible at startup.

This document describes the controls as they exist in the code today. Gaps are listed at the end.

## Permission modes

Prompting is a *mode*, not the identity of the tool. The trust boundary moves
with it:

| | default build, `mode="bypass"` (default) | default build, `mode="ask"` | `mode="dontask"` | `security-enforced` build |
|---|---|---|---|---|
| Ordinary bash/write/edit/MCP | runs, no prompt, `ToolAutoAllowed` in transcript | y/N ask per action | refused unless an allow-rule matches | y/N ask per action (config cannot change this) |
| Protected execute-later paths | **always asks** (headless: denies) | always asks | always asks (headless: denies) | always asks |
| File tool (`read`/`write`/`edit`) outside the working directory | **always asks** (headless: denies) | always asks | always asks (headless: denies) | always asks |
| Admin preapproved (`/etc/hotl/preapproved.toml`) | grants apply (redundant under bypass) | grants silence matching asks | grants are the only thing that runs | grants are the admin's no-prompt channel |
| Admin/user deny rules | refuse the call outright, with the rule named in the tool result | same | same | same |
| Kernel sandbox / egress / undo / masking | on | on | on | on |

**Plan mode is a second, orthogonal axis, and is not a security control.** It
moves `write`/`edit` into the "always asks" row above — never auto, not even
via an allow-rule — and leaves every other tool on the mode's row. It is a
posture that makes the agent's natural mutation path stop for a human; it is
**not** a guarantee the tree is untouched, because `bash` still follows the
mode and a shell redirect writes a file without the `write` tool. Do not treat
`--plan` as a sandbox. The unattended read-only posture is `dontask` with no
allow-rules.

In `bypass`, the boundary is **sandbox + protected asks + deny rules + undo**,
not per-action approval. The README's "safety" claim holds unconditionally
only for the `security-enforced` build; the default build's floor is the row
above. `/etc/hotl/preapproved.toml` is trusted only when root-owned and not
group/world-writable; otherwise it is refused loudly at startup and in
`hotl doctor`.

## What the sandbox is not (read this first)

The kernel sandbox floor is **not data-loss prevention.** Since 0.10 it carves two read denials out of an otherwise open read surface — hotl's own config and data dirs, and the credential class (`~/.ssh`, `~/.aws`, `~/.config/gcloud`, `~/.azure`, `.netrc`/`.npmrc`/`.pypirc`/`.dockercfg`) — but **everything else stays readable**, most importantly secrets *inside the working tree* (`.env`, `config/secrets.yml`), which cannot be carved without breaking the agent's actual job. A `bash` command that the human approves (or that an allow-rule matches) can still **read those and send them anywhere over the network**: network egress is open by design (the agent legitimately fetches dependencies). Treat the human approval prompt, not the sandbox, as the exfiltration boundary — and know that a plausible-looking approved command (`run the tests`, which also `curl`s) exfiltrates freely. Egress restriction exists but is **opt-in** (`[network]` in config.toml — see "Network egress" below); the default is open. Under the default policy, do not run hotl against secrets you would not paste into a command yourself.

**What this section does *not* cover, since 0.5.x: the file tools.** The
paragraph above is about `bash`, and stays true of it word for word — an
approved command still reads anything you can read and sends it anywhere.
The narrower change is that `read`, `write`, `edit`, `glob`, and `grep` are
now *workspace-contained*, and the containment is enforced on the **file
descriptor** rather than the path string: a path is inside only if a descent
from the workspace-root fd reached it without traversing a symlink
(`openat2(RESOLVE_BENEATH)` on Linux, component-wise `openat` with
`O_NOFOLLOW` elsewhere). Since the check is made on the descriptor the tool
then uses, there is no name to re-resolve and no check/open race.
Consequences: `glob`/`grep` refuse an out-of-tree or symlinked search root
outright; `read` outside the tree is a **protected ask that outranks
`mode=bypass`**, so it prompts in every mode; `write`/`edit` never follow a
symlink at any component and classify protected paths on the *resolved*
target, so a symlink cannot launder a protected write into an ordinary one.
This closes the "`ln -s ~ link` then `grep --path link`" read of the whole
home directory under no prompt at all. It does **not** narrow `bash`.

## The permission gate

Every mutating or executing tool call passes one fixed pipeline before it runs:

1. **PreToolUse hooks** (in-process, then owner-configured shell hooks) may deny or rewrite the call. A rewritten call **re-enters the gate** — a hook cannot launder a call past the ask.
2. **Allow rules** (`[[allow]]` in `~/.config/hotl/config.toml`) may auto-approve it, narrated. Rules are deliberately editor-written only — there is no in-console "always allow," so ask-fatigue cannot manufacture an ungoverned allowlist. Rule matching defends against shell-operator smuggling after an allowed prefix (`ls && curl …` does not match an `ls` rule) and `..` path traversal.
3. **Protected paths** are checked *before* allow rules and **never auto-approve**. Writes that could execute later outside any gate escalate the ask with a *why* warning. The class covers: `.git/hooks/`, Makefile-class files (`Makefile`, `justfile`, `build.rs`, `conftest.py`, `*.gyp`), agent-instruction files (`AGENTS.md`, `CLAUDE.md`), harness/editor settings (`.hotl/`, `.claude/`, `settings.json`), shell rc files, `.ssh/`, credential stores (`.aws/`, `.config/gcloud/`, `.azure/`, `.npmrc`, `.pypirc`, `.netrc`, `.dockercfg`), git config, and cron/systemd units.
4. **The human ask** — a y/N modal in the console, escalated with a `⚠` line when a protected path is involved. Headless (`-p`, `--json`, or non-TTY stdin) **default-denies immediately**: nothing interactive ever blocks or leaks a prompt into CI logs. Interactive asks (the console modal, an attached `hotl bg` session) wait until you answer.

Asks are durable: a `pending_ask` entry is committed to the session log before the question is surfaced and an `ask_resolved` entry after — a crash mid-ask is visible on replay, never silently resolved.

A repetition detector (doom-loop) halts a turn that repeats the same tool-call cycle and asks the human whether to continue; a per-tool consecutive-failure budget ends turns that keep failing the same way.

## The kernel sandbox floor

`bash` executes confined — **Seatbelt** on macOS (deny all file writes, then re-allow the working directory, temp, `/dev`, and any `[sandbox].writable` directories the owner configured), **Landlock** on Linux ≥ 6.2 (same shape; see the ABI note below). Network egress is open by default and restrictable per host (see "Network egress" below and "what the sandbox is not").

**The read carve (0.10).** Reads used to be wholly open. Two tiers are now denied to every sandboxed child — the `bash` tool, `grep`'s ripgrep, post-edit diagnostic commands, and owner-configured shell hooks alike. Three of those four have no y/N prompt, which is why the config lever below exists and is not merely a convenience.

- **Tier A — hotl's own config and data dirs. Default on, never liftable.** The session token under the data dir is mode 0600 in a 0700 directory, but a sandboxed `bash` runs as the *same uid*, so DAC does not stop it; reading the token and driving the session socket is a complete bypass of the permission gate. The config dir holds allow-rules, hooks and `api_key_helper`. `[sandbox].readable` refuses any entry touching either, and the per-command grant does not apply to this tier. The run dir lives inside the data dir, so denying the data dir covers it.
- **Tier B — the credential class. Default on, liftable.** `~/.ssh`, `~/.aws`, `~/.config/gcloud`, `~/.azure`, and the `$HOME` dotfiles `.netrc`, `.npmrc`, `.pypirc`, `.dockercfg`. Sourced from the same constant as the execute-later *write* classification, so the read set and the write set cannot drift. Agent-run `git push` is unaffected when your keys are in `ssh-agent` — the profile deliberately keeps the agent socket reachable, and `ssh` never opens a key file in that case. It *does* break when no agent is loaded, when no cloud session is cached, or for a private-registry `npm install`.

Two levers lift Tier B. `[sandbox].readable = ["~/.ssh"]` is standing (it reaches hooks and diagnostics, and needs a session restart, since `[sandbox]` is installed once at startup); pressing `s` instead of `y` at a `bash` ask lifts it for **that one command only** — scoped around the single tool call, so it cannot leak into the next one, and unreachable headless or from a sub-agent, whose asks auto-deny. Once Tier B is lifted by config, every ask is labeled `reads:open`; the hardened default is silent.

**What the carve denies, precisely: contents, not existence.** macOS denies `file-read-data` rather than `file-read*`, because denying metadata as well breaks `ls -la ~` (it stats every child). Landlock has no sub-path carve-out and its rights *union across ancestors*, so a denial is expressed by not granting read on any ancestor and re-granting every sibling; the ancestor keeps `ReadDir` so `ls ~` still works. **Documented asymmetry:** because that `ReadDir` is hierarchical, `ls ~/.ssh` still lists filenames on Linux where macOS hides them. File *contents* are denied on both.

**Verified, not assumed.** The startup probe plants a canary inside Tier A and has its sandboxed child attempt the read in the same spawn it already uses for the write test. A host that confines writes but cannot narrow reads stays `Enforced` and is labeled `reads:open` — demoting it would revoke `bash` auto-approval for a restriction nobody opted into. `hotl doctor` prints the resolved deny set and the probe's verdict.

**Not covered by the carve.** The in-process file tools are not child processes and a kernel rule cannot reach them; `read`/`write`/`edit` refuse Tier A outright instead (see below), and out-of-workspace reads remain a protected ask. MCP servers are spawned outside this floor — installing one is the trust decision. `api_key_helper` runs plainly, because it exists to reach a credential store. And the developer's own shell is never confined: nothing here changes a `git push` you type yourself.

**The file tools refuse Tier A outright.** `read`, `write` and `edit` are in-process, so the kernel carve does not reach them. An out-of-workspace `read` is normally a protected ask a human can approve — but for hotl's own config and data dirs that would mean a human approving the removal of the permission gate, which under an injected prompt they might. Those paths are therefore refused at the run-time door in every mode, on the *resolved* target, with no approval that unlocks them. The cost is real and deliberate: the agent cannot read your `config.toml` for you either.

**`Enforced` is a runtime claim, not a configuration one.** At startup hotl spawns one sandboxed child that attempts to write outside the confinement, and reports the floor as enforced only if the write fails *and* leaves nothing on disk. The prior check — "does `/usr/bin/sandbox-exec` exist" — could not distinguish a working profile from one that silently failed to apply, and that single boolean is what gates `bash` allow-rules auto-approving without a human. The probe is memoized (one spawn per process) and bounded (2s, then fail-closed). It writes to `/var/tmp` or `$HOME`, uniquely named and deleted on every path; `HOTL_SANDBOX_PROBE_DIR` overrides the location and must be outside the working directory, `TMPDIR`, and every `[sandbox].writable` entry.

**`[sandbox].writable` widens the floor without opening it to hotl itself.** The owner may list extra writable directories (bazel/ccache-style out-of-workspace caches) in `~/.config/hotl/config.toml`; they join the kernel re-allow set for every sandboxed spawn, and the probe's outside-the-floor target is chosen outside the *widened* set so `Enforced` still describes the floor children actually get. Validation is fail-closed per entry, on canonicalized paths (a symlink cannot smuggle a protected directory in): an entry that is, contains, or sits inside hotl's config dir or data dir is **refused** — a writable config dir is self-granted privilege escalation (the agent could rewrite its own allow-rules, hooks, or `api_key_helper`), and a writable data dir means tamperable session logs and snapshots. That refusal is what keeps `~` and `/` unlistable. Risky system roots (`/etc`, `/usr`, `/opt`, …) are honored with a loud warning. The separate `[sandbox].file_tools = "writable"` opt-in extends the same directories to the `write`/`edit` tool boundary (ordinary asks, same fd-descent symlink-refusing guard, anchored at the extra root); protected-path escalation always runs first, so the grant never downgrades an execute-later write. Defaults preserve today's posture exactly: no entries, `file_tools = "workspace"`.

**Linux ABI floor: v3 (kernel 6.2).** Landlock restricts only the rights present in the *handled* mask, so a ruleset that does not handle `LANDLOCK_ACCESS_FS_TRUNCATE` leaves `truncate(2)` by path unconfined — an approved command can zero any file on the host. That right arrives at ABI v3. Kernels 5.13–6.1 therefore yield a genuinely partial floor and are **not certified**: they degrade fail-closed like an unsandboxed host unless the operator sets `HOTL_SANDBOX=best-effort`, which accepts the partial floor and renders `sandboxed:landlock(partial)` in every ask. (The gap is not reachable through `truncate -s 0` or a `>` redirect — both open the file for writing first, which `WriteFile` denies at ABI v1 — which is why it survived earlier review.) Landlock's network confinement independently requires ABI v4 / kernel ≥ 6.7.

**macOS automation.** `(allow default)` leaves Apple Events open, so `osascript -e 'tell application "Terminal" to do script "…"'` executes its payload in a process that is not a descendant of the sandboxed child — an escape with no disk write. `appleevent-send` is denied by default; plain AppleScript still runs. `HOTL_MACOS_AUTOMATION=allow` opts out and is labeled `automation:allow` in every ask; Apple-Event-driven Xcode/Simulator/Instruments flows need it. The AppleEvents and LaunchServices mach brokers are deliberately *not* denied by name: doing so converts the refusal into an indefinite hang rather than a fast error.

**Child environment.** Provider credentials (`ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_ADMIN_KEY`, `OPENAI_API_KEY`, `HOTL_API_KEY_HELPER`) are removed from every child process — `bash`, `grep`'s ripgrep, diagnostics, and owner shell hooks share one chokepoint, so there is no second spawn path to forget. The list is deliberately narrow: the `KEY`/`TOKEN`/`SECRET` heuristic the ingestion masker uses would also strip `GITHUB_TOKEN`/`CARGO_REGISTRY_TOKEN`/`NPM_TOKEN` and break `gh`, `cargo publish` and `npm publish` silently. `HOTL_SCRUB_ENV=A,B,C` adds names; `HOTL_SCRUB_ENV_STRICT=1` applies the broad heuristic.

Hosts where the floor is unavailable (older Linux kernels) **degrade fail-closed**: every exec is individually human-gated with an `UNSANDBOXED` banner in the ask, and `bash` allow-rules stop applying — auto-approval of commands exists only while the sandbox is enforced. `HOTL_SANDBOX=off` is an explicit escape hatch and is labeled as such in every ask. Windows native is unsupported (no floor designed); WSL2 is the Windows path.

Owner-configured shell hooks run under the same floor.

**Every opt-out above is labeled.** The ask carries `sandboxed:<mechanism>` plus a marker for each denial the operator lifted — ` unix:open`, ` automation:allow`, or the `landlock(partial)` mechanism name. Silence is the hardened state; there is no way to run with a denial lifted and no marker.

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

**The proxy is bounded, capped and authenticated.** An unfinished request head is dropped after 10s (`408`) rather than pinning a task and a socket for the life of the process; live connections are capped at 64, beyond which the proxy answers `503` rather than queueing unbounded work — the same Layer-B discipline `SessionConcurrency` applies to subprocesses and requests. The listener requires a per-session `Proxy-Authorization` credential, carried as userinfo on the proxy URL injected into the child environment (`http://hotl:<token>@127.0.0.1:<port>`), so a *different local process* cannot spend the allowlist. curl, git-over-HTTP, pip and cargo forward it; `HOTL_PROXY_AUTH=off` restores the unauthenticated listener for a client that honors the proxy host but discards credentials. The token is 128 bits from a non-cryptographic PRNG and is **not** claimed to be secret from the user themself — it rides the child's environment by construction, which is precisely the boundary it does not defend. A request bearing more than one `Host:` header is refused with `400`, not resolved first-wins: two `Host` headers let the policy check one value while the upstream honors the other.

**Degradation is fail-closed**, mirroring the UNSANDBOXED posture: when `off`/`allowlist` is configured but the kernel can't back it (no seatbelt, Landlock without the net ABI, `HOTL_SANDBOX=off`), every bash ask is loudly marked `NET:UNENFORCED(reason)` and bash allow-rules stop auto-approving. An unknown `egress` value fails closed to `off` with a startup warning — a typo never means open. While a restriction is active and enforced, the ask label carries `net:off` or `net:allow(N)`.

**Honest limits.**
- macOS: DNS resolution rides the mDNSResponder unix-domain socket, which stays allowed — name *resolution* still works under `off`/`allowlist` and is not exfil-confined (a DNS tunnel can leak data even in `off` mode).
- Linux: Landlock net is **TCP-only** — UDP, including DNS and DNS-tunnel exfiltration, is not confined — and **port-scoped, not address-scoped**: the proxy port *number* is connectable on any host, and `off` blocks loopback TCP too (unix-domain sockets stay open).
- The allowlist is host-granular: an allowed host is fully reachable, any path, any method — and therefore also usable as an exfiltration destination (an allowed `github.com` accepts pushes to any repo). List hosts you trust with your data, not merely hosts you fetch from.
- The proxy is HTTP-only: `git` over SSH remotes, and any other non-HTTP protocol, cannot traverse it — under `off`/`allowlist` they fail at the kernel wall regardless of the list. Use HTTPS remotes when running restricted.
- `web_fetch`'s SSRF guard classifies **literal addresses only**. Cloud instance-metadata addresses (`169.254.169.254`, `169.254.170.2`, `fd00:ec2::254`) are refused on every hop including the first, and a redirect out of the public internet into private/loopback space is refused as a target the human never approved (a chain that *starts* private is allowed, and was escalated at the ask). A **hostname that resolves** into private space on a redirect hop is not caught — a synchronous redirect policy cannot resolve names — so this narrows drive-by SSRF, it does not close it. `HOTL_WEB_ALLOW_METADATA=1` lifts the metadata refusal.
- Unix-domain sockets are a network operation, not a file write, so the write floor does not cover them. On macOS the container/orchestrator daemon socket class (`docker.sock`, `podman.sock`, `containerd`, `crio`) is denied by default — it is root-equivalent — and `HOTL_UNIX_SOCKETS=open` opts out, marked `unix:open` in every ask. `ssh-agent`/`gpg-agent` sockets stay reachable so `git push` over SSH keeps working; they are capability-limited, not arbitrary-write. **On Linux none of this is enforceable**: Landlock has no rule covering `connect(2)` to a pathname socket at any ABI (v6 `Scope` covers abstract sockets only), so a local daemon socket is reachable from a confined command. Do not run hotl with `mode = "bypass"` on a host where a writable daemon socket is a privilege boundary you rely on.

## Untrusted input → model context

Everything that flows into the model's context from a source other than the user is wrapped in an **untrusted-content envelope**: a provenance-tagged wrapper (`trust="untrusted"`, `source=…`) carrying an explicit non-authority statement — the content cannot authorize tool use, override the user's instructions, or change the rules — with closing-delimiter defang (a zero-width space inserted into `</`) so the content cannot fake its own closing tag.

| Untrusted path | Control |
|---|---|
| repo instruction files (`AGENTS.md`/`CLAUDE.md`, incl. nested) → context | untrusted-content envelope |
| auto-memory files → context | same envelope; clipped to a 16 KB load budget |
| MCP server output → context | sanitizer chokepoint (below) |
| sub-agent result → parent context | `<subagent-result trust="untrusted">` envelope |
| bash/tool output → context | human gated the *command*; output enters context unsanitized — the model treats tool results as data by system-prompt instruction only (see gaps) |
| post-edit diagnostics output → context | closing-delimiter defang, so a compiler error quoting a model-written file cannot forge `</diagnostics>` and reclaim the surrounding context. Not otherwise sanitized or byte-capped like MCP output — the "native tool output is not sanitized" gap below still stands |
| api_key_helper command (config/env) → key | editor-written planes only (config.toml is a protected path); runs as harness infrastructure outside the tool sandbox, never model-initiated; stdout registered with the ingestion masker (startup key), stderr console-only; **caveat:** auth-error response bodies from the provider/gateway are persisted in the session log — the startup helper key is masked, but a key *refreshed* mid-session is not re-registered with the masker, so a gateway that echoes keys in auth-error bodies would persist that refreshed key in the log |

## MCP

**Sanitizer — one named chokepoint.** Every string a server returns — call results, `tools/list` listings (names, descriptions, schemas), and errors — passes `hotl_mcp::sanitize` before entering the transcript; a code path that skips it is a bug by definition. Transforms, in order: (1) strip ANSI escapes and C0 control characters except `\n`/`\t` (terminal-injection defense); (2) enforce a 50 KB per-result byte cap with an explicit `[truncated N bytes]` marker (context-flooding defense); (3) wrap in the untrusted-content envelope with `source="mcp:<server>/<tool>"` (prompt-injection defense). Tool listings load only on demand (deferred loading), and a `tools/list_changed` notification only marks the cache stale — the refreshed listing re-passes the sanitizer, and every MCP call remains gated per call; new tools never auto-run.

**Trust store — first-use screen.** The first call to a server raises a *protected* ask (never auto-allowable): server name, binary path, SHA-256 of the binary, and what approval means ("this program will run on your machine and its output will enter the model's context"). Approval is recorded in `~/.config/hotl/trust.toml` keyed by server name → binary hash; a changed hash re-raises the screen. An unreadable binary is recorded honestly as having no integrity check; a failed trust-store write keeps the grant in memory only and re-asks next session. Server binaries run **outside** the bash sandbox floor — they are user-installed programs, not model-directed commands; installing one is the trust decision.

**`hotl mcp` cannot grant.** The inspection command is read-mostly by construction, and both limits are load-bearing rather than incidental:

| Capability | Where it lives | Why |
|---|---|---|
| grant trust | the in-session screen only | A CLI verb writing `trust.toml` is the "always allow" the permission model omits everywhere else, and it is reachable from model-directed bash at the same uid with `hotl` on `PATH`. |
| register a server | hand-edited `config.toml` | `hotl mcp add` *prints* the block. A CLI that wrote config would be a path `bash -c 'hotl mcp add …'` could take, and `bash_protected_write_reason` does not cover it — that analysis reads redirects, `tee`, and `dd`, not a program that writes config as a side effect. Only the kernel floor stops it, incidentally rather than by design. |
| revoke a grant | `hotl mcp untrust` | Revocation only ever reduces privilege. |
| start a server | `hotl mcp test` | Screens with the same fingerprint text first, refuses without a TTY, and records nothing — screening a one-shot spawn is not a durable grant. |

Registering a server grants nothing: the first-use screen still fires. The invariants are enforced by `no_verb_ever_writes_config_toml` and `test_records_no_trust`.

## Retrieval (`recall`)

The `recall` tool searches owner-configured knowledge backends (`[[retrieval]]`
in config.toml). The tool is absent when nothing is configured.

- **Everything a backend returns is untrusted.** Results, and backend errors,
  pass one sanitizer chokepoint — control/ANSI strip, 50 KB byte cap, defang,
  then the untrusted-content envelope with `recall:<backend>` provenance —
  before entering the transcript. Retrieved text may inform the work; it can
  never authorize tool use or change the rules.
- **Backends that execute a program inherit the MCP posture.** An MCP-backed
  backend raises the protected first-use ask carrying the server binary's
  SHA-256 (recorded to the same `trust.toml`; a grant covers the server
  whether it is reached via `mcp` or `recall`), then a plain per-call ask.
  In-process backends with no execution or egress run without asking.
- **No egress by default.** hotl ships no cloud retrieval backend; a backend
  reaches the network only if the owner configures one that does.
- Oversized result sets ride the existing eviction path: blob on disk, head
  preview + read-back pointer in context.

## Sub-agents and protocol clients

**`spawn` (sub-agents).** The child has **no human on the loop**, so its permission asks default-deny — it runs only auto-allowed/read-only tools under the parent's sandbox floor and rules. The depth cap is **structural, not a counter**: children are built with a builtins-only tool registry — no `spawn`, no MCP — so a child cannot recurse or reach external servers; the capability simply isn't in its registry. Results return to the parent inside the untrusted envelope. `fork` and `teammate` are reserved topologies.

**`hotl acp` (protocol surface).** The connected client answers `session/request_permission` round-trips — it *is* the human-on-the-loop for that session, exactly like the console. A missing or malformed reply, or a client that hangs up, resolves to deny.

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

## Self-update

`hotl update` is the only thing that writes hotl's own binary, and only when you run it — there is no background check. It verifies the release archive's SHA-256 **in process before decompressing**, extracts only the executable (refusing absolute and `..` entry paths), runs `--version` on the result, and swaps by same-directory rename so a failure never leaves a partial binary.

**The checksum is integrity, not provenance.** It travels in the same document, from the same host, over the same TLS session as the archive: it catches a corrupted or truncated download, not a release someone replaced upstream. A signature made with a key that never touches CI is the control that would cover that, and it is not shipped — see the gaps below. This is the same trust the installer script, `cargo install`, and `nix profile install` already place in the upstream host, not less.

Installs owned by a package manager (cargo, Nix, Homebrew) and source builds are detected and refused with the right command instead — hotl never writes a binary another tool is tracking. **`security-enforced` builds refuse outright**: the published binaries are ordinary builds, so replacing one would silently drop the enforced posture the version banner advertises. A sandboxed child cannot reach the binary either, unless it happens to sit in the working directory or temp.

## Known gaps (planned, not shipped)

- **No egress ask.** A host not on the allowlist gets a flat 403; there is no y/N ask ("bash wants to reach `host` — allow for this session?") the way tool permissions have. That interaction is what would make `allowlist` livable as the *default* — the first `cargo build` would ask once about crates.io instead of failing — and is the recorded path to flipping the egress default, along with a story for the SSH gap. Until it ships, egress restriction stays opt-in.
- **The read carve does not reach in-tree secrets.** `.env` and `config/secrets.yml` inside the working directory stay readable to `bash`, and cannot be carved without breaking the agent's actual job. This is a deliberate non-goal, not a deferral — with egress open by default, the approval prompt remains the exfiltration boundary for them.
- **The read carve does not reach MCP servers or `api_key_helper`.** Both are spawned outside the floor: installing an MCP server is the trust decision (first-use hash screen), and the key helper exists to reach a credential store.
- **Linux has no kernel deny for the execute-later paths under the workspace** (`.git/hooks`, `.github/workflows`, `.cargo`, `.hotl`, `.claude`), which macOS denies in the Seatbelt profile. Landlock's rights union across ancestors, so expressing it would mean granting no write on the workspace root itself — measured on 6.8, that also denies creating any new file at the top of the tree and `.git/index.lock`. Linux relies on the permission-layer escalation alone here.
- **The credential read-deny is `$HOME`-relative and path-shaped.** A credential store somewhere else (a custom `AWS_CONFIG_FILE`, a keyring daemon, a mounted secret) is not covered, and neither is the container-credential path class.
- **Native tool output is not sanitized.** bash/read results enter context verbatim; only MCP output passes the sanitizer chokepoint. Post-edit diagnostics output is closing-delimiter defanged (so it cannot forge its own envelope's end) but is not otherwise sanitized or byte-capped, so this gap stands for it too. The system prompt instructs the model to treat tool results as data — an instruction, not an enforcement.
- **`web_fetch`'s SSRF guard does not resolve names.** Literal private/loopback/metadata addresses are classified and refused; a hostname that *resolves* into private space on a redirect hop is not. Closing it needs a custom connector that classifies the resolved `SocketAddr`, which is why it is listed rather than shipped.
- **The egress proxy credential is not cryptographic.** It separates local processes within one session; it is visible to anything running as the same user, since it rides the child's environment. It is not a defense against that user.
- **The permission pipeline has no AST or LLM inspectors.** Command scanning is heuristic (shell-operator detection), not tree-sitter-based; there are no LLM judges voting on calls.
- **No third-party extension trust screens.** Moot today — hooks and settings load only from owner config, never from the repo — but required before any repo-supplied or third-party extension lane ships.
- **Release artifacts are not signed.** `hotl update` verifies a SHA-256 that ships alongside the archive, which is integrity against corruption, not provenance. Closing it means a minisign key held offline and its public half compiled in — and it should raise the installer script at the same time, since signing only the updater hardens the one path that isn't the documented install.
- **Browser/WASM profile does not exist** and has no kernel sandbox story yet; it will not ship without compensating controls.

## Standing rules

- Tool descriptions must not promise protections the executor doesn't implement — tested as an invariant.
- Supply chain: pinned deps; SHA-pinned remote installs default ON; lifecycle-script allowlists.
- No telemetry. Secret-scrubbing in logs stays. Crash dumps are local, secret-scrubbed, and only ever shared manually by the user; there is no update check — `hotl update` reaches the network only when you run it.

## Reporting a vulnerability

GitHub private security advisories on the repo, or email the owner (address in the repo README). Coordinated disclosure, 90-day default window. Report before publishing; good-faith research against your own installation is welcome.
