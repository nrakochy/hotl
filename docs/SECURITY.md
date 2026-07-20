# SECURITY.md — stance

Core belief 12: **defaults are the safety design.** Enforcement ships ON with a curated default policy. The cautionary tale is Forge (11): a well-built policy engine behind a default-off flag with an allow-all policy file is equivalent to nothing.

## M0 routing table (the rows for surfaces that exist today — r2 R2)

| Untrusted path | Where it flows | M0 control | Hardening milestone |
|---|---|---|---|
| bash/tool output → model context | tool results, verbatim | human gated the *command*; output enters context unsanitized — model treats tool results as data by system-prompt instruction only | M5 inspector/sanitizer |
| repo instruction files (AGENTS.md/CLAUDE.md) → context | tagged user item | untrusted-content envelope (wrapping + explicit non-authority statement), `SyntheticReason::ProjectInstructions` provenance | M2 (auto-memory joins the same envelope) |
| write-now / execute-later files | disk → future execution outside any gate | protected-paths class escalates the ask with a *why* warning (git hooks, Makefile-class, agent-instruction files, shell rc, harness settings); no allow-rule persistence exists | M1 sandbox floor + allow rules |
| session log at rest (JSONL, permanent by design) | `~/.local/share/hotl/sessions/` | secret sentinel-masking at ingestion (secret-named env values replaced before bytes land) | M2 store audit; M3b retention/GC |
| headless `-p` stdout → shells/CI logs | caller's environment | asks default-deny (nothing interactive ever blocks or leaks a prompt); output is the model's answer only | MD (`--json` schema freeze) |
| zsh scrollback → transcript | *(surface not built yet — cut-line item)* | n/a until the plugin ships; row reserved | with the plugin |

**Build-phase reality (r2 R1 — stated loudly, not hidden):** the kernel sandbox floor lands **M1**. During M0, every exec/mutating tool call is individually human-gated (y/n; headless default-denies on timeout) and **allow-rule persistence is disabled** — "always allow" does not exist until the floor exists, so ask-fatigue cannot manufacture an ungoverned allowlist (r2 R3). What M0 *does* ship: the routing-table rows for its actual surfaces (bash-output→context, repo-instruction-files→context, zsh-scrollback→transcript, JSONL-at-rest, headless-stdout), the protected-paths execute-later class (`.git/hooks/`, hook/settings files, Makefile-class, AGENTS.md — writes escalate to a warning ask), the untrusted-content envelope on repo instruction files, and secret sentinel-masking at transcript ingestion (0001 §M0).

Layers (02, 09, 12):

1. **Permission rules** — allow/ask/deny with pattern matching; deny-first evaluation; protected-paths tier checked *before* allow rules (12); workspace trust gates project-supplied capability grants.
2. **Inspector pipeline** — composable checks voting deny > ask > allow: rule-based, AST command scanning (tree-sitter, 10), repetition, and LLM judges with adversarially-stripped inputs (tool results withheld from the judge — 12's classifier design).
3. **Kernel sandbox floor (native only; lands M1 — see build-phase reality above)** — Seatbelt (macOS) / Landlock (Linux ≥5.13, incl. WSL2) isolation for execution, with sandbox-aware auto-approve; credential masking via proxy sentinels so secrets never enter the sandboxed process (12). **Hosts where the floor is unavailable (older Linux kernels) degrade fail-closed to the M0 posture permanently: every exec individually human-gated, allow-rule persistence disabled, loud banner. Windows native is unsupported (no floor designed); WSL2 is the Windows path** (distribution.md §D3). The browser profile has no kernel sandbox and relies on the browser's own — with the compensating controls owed by Sec #4 (proxy + plugin confinement) before browser ships (blueprint §WASM).

## M3a routing rows + MCP sanitizer spec (the exit-gate artifacts — r2 R11)

| Untrusted path | Where it flows | Control | Notes |
|---|---|---|---|
| MCP server result → model context | tool results | **one named chokepoint**: `hotl_mcp::sanitize` — every string a server returns passes it before entering the transcript | see spec below; bypassing it is a bug by definition |
| MCP server binary → execution | child process on your machine | trust store first-use screen (below); server binaries run **outside** the bash sandbox floor (they are user-installed programs, not model-directed commands) — installing one is the trust decision | hash-change re-prompt |
| MCP tool descriptions/schemas → model context | the `mcp` tool's listing output | same sanitizer chokepoint (descriptions are server-authored text — a poisoned description is the classic MCP attack) | listed only on demand (deferred loading) |
| `tools/list_changed` notification → tool surface | schema cache invalidation | notification only marks the cache stale; the refreshed listing re-passes the sanitizer; **new tools never auto-run** — every MCP call remains gated per call | |
| skills / owner config files → context | `skill` tool output | owner-authored, still enveloped (files quote external content) | M3b |
| shadow snapshot store at rest | `~/.local/share/hotl/shadow/` | **not masked** — deliberately: snapshots hold the user's own workspace files, already at rest unmasked in the workspace itself; masking applies to *transcripts* (model-visible text), not to the user's own file mirror | M3b row |

**Sanitizer spec (input classes × transforms):** input classes are (a) tool-call result content, (b) tool listings (names, descriptions, schemas), (c) server-sent errors. Transforms, applied in order to every class: (1) strip ANSI escapes and C0 control characters except `\n`/`\t` (terminal-injection defense); (2) enforce a per-result byte cap (default 50 KB) with an explicit `[truncated N bytes]` marker (context-flooding defense); (3) wrap in the untrusted-content envelope with `source="mcp:<server>/<tool>"` and the standing non-authority statement (prompt-injection defense — same wording discipline as repo instruction files). Injection point: exactly one, the `mcp` tool's result assembly; there is no code path from a server response to the transcript that skips it.

**Trust-store first-use screen (Sec #12):** the first call to a server raises a *protected* ask (never auto-allowable by rules): server name, binary path, SHA-256 of the binary, and what approval means ("this program will run on your machine and its output will enter the model's context"). Approval is recorded in `~/.config/hotl/trust.toml` keyed by server name → binary hash. A changed hash re-raises the screen (content-hash revocation, the standing rule below). Denial simply fails the call back to the model.

**Not yet specified — each bound to a named milestone gate, not floating debt (r2 R5):** cross-agent-message routing rows are an **M4 exit gate**; the default policy file contents + the remaining trust-prompt screens (extension install, workspace trust — the MCP first-use screen shipped with M3a) + parameterized capabilities (fs scoped to path globs, http to host allowlists) are an **M5 entry gate**. These prompts and defaults *are* the real boundary — undesigned, they are the Forge failure recursed (Sec #12); gated, they cannot be silently skipped.

Other standing rules:
- Permission mediation lives in the embedding protocol, keyed by transcript-stable IDs, surviving reconnects (05).
- Extension trust is granular: metadata-visible / execution-blocked when untrusted; content-hash revocation on file change (03, 11); identity env vars applied last (03).
- Supply chain: pinned deps; SHA-pinned remote installs default ON (grok's discipline, 03); lifecycle-script allowlists (Pi, 08).
- Tool descriptions must not promise protections the executor doesn't implement — tested as an invariant (11's drift lesson).
- No telemetry. Secret-scrubbing in logs stays (07). Crash dumps are local, secret-scrubbed, and only ever shared manually by the user; the update *check* defaults off (distribution.md §D6/D8).

**Reporting a vulnerability (from first public release):** GitHub private security advisories on the repo, or email the owner (address in the repo README once public). Coordinated disclosure, 90-day default window. Report before publishing; good-faith research against your own installation is welcome.
