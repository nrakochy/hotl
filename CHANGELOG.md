# Changelog

Notable changes to hotl. Pre-1.0, breaking changes land at every 0.x minor;
the internal library crates version in lockstep with the binary and carry no
semver promise of their own.

## [Unreleased]

First release to reach crates.io since 0.4.1. v0.5.0 through v0.6.1 were
tagged and pushed but never published; those versions are skipped rather than
backfilled, so 0.4.1 upgrades straight to this one. The `hotl subscribe`
webhook bridge is deliberately not in this release — it stays unreleased on
master.

### Fixed

- **Releases reach crates.io again.** The publish workflow ran the test suite
  without installing ripgrep, while `ci.yml` has installed it since the `grep`
  tool landed. `grep` spawns `rg`, and its test asserts real ripgrep exit-code
  semantics rather than skipping when the binary is absent — so every tag from
  v0.5.0 on went green in CI and then died in its release job, one step before
  publishing. Only the release path was affected; a missing `rg` at runtime has
  always degraded to a tool error telling the agent to fall back to `bash`.
- **`hotl watch` no longer lists hotl's own utility processes as agents.** Pane
  discovery skips the non-agent subcommands, so a `watch`, `gc` or `doctor`
  pane stops appearing in the dashboard as something you could steer.

### Added

- **`hotl watch` shows what a blocked agent is waiting on.** A pane in the
  Blocked state carries its pending question through to the dashboard instead
  of only reporting that it is blocked, so the list says what the agent needs
  rather than just that it needs something. Observations also carry the
  originating prompt and the raw pane tail.

### Changed

- **Skills get a dedicated docs page**, covering the grouped index, search over
  collapsed sources, and why there is deliberately no index database. The
  landing page now leads with how the context window is spent — including that
  skills are indexed and lazily loaded rather than preloaded.

## [0.6.1] - 2026-07-26

### Fixed

- **The cache marker no longer lands on the todo reminder.** The `<todos>`
  reminder was the last user-role item in the projection, so it took the cache
  breakpoint — and its text changes on every todo edit, so that breakpoint
  wrote a cache entry nothing ever read and re-billed the whole history at full
  price on every sample the list was active, which is to say the default
  workflow. The per-sample ephemeral suffix is now a separate channel the
  breakpoint chooser never sees, so a marker on ephemeral content is
  unrepresentable rather than merely avoided.

### Added

- **Rolling cache anchors keep tool-heavy turns inside the API's lookback.** A
  breakpoint's cache lookup walks at most ~20 content blocks back, so one wide
  batch used to push the previous entry out of reach and full-miss every sample
  after it. Extra breakpoints now land at deterministic stride crossings —
  including *inside* a wide tool batch — computed as a pure function of the
  durable items, so the marker one sample writes is the marker the next sample
  reads and the speculative and sequential build paths still agree byte for
  byte.
- **A one-hour cache TTL for interactive sessions.** `hotl tui`, `hotl acp` and
  `hotl bg`/attach ask for 1h on the prefix and anchor breakpoints — the
  sessions that pay for a multi-minute human pause. Headless `-p` and sub-agent
  children stay at five minutes. The latest breakpoint always stays at five
  minutes: its segment is rewritten every sample, so a longer-lived write
  premium there recurs per turn and buys nothing.
- **`cost_usd` in usage frames**, priced per bucket from the model catalog and
  surfaced in the TUI, `--json` stream and ACP wire. One-hour cache writes bill
  at 2× input, five-minute at 1.25×, reads at 0.1×; the per-TTL split is read
  from the wire when the provider reports one and never guessed when it does
  not.

### Changed

- **Fork seeds no longer commit the todo reminder into child logs.** `fork`
  takes the durable projection only, so an ephemeral item can no longer stop
  being ephemeral by being written into a child's canon.

## [0.6.0] - 2026-07-26

### Added

- **Loop overhead is now a measured, CI-gated number.** A `LoopLedger` stamps
  ten fixed phases per sample and flushes one telemetry report per turn (never
  a transcript entry), including max-RSS; a testkit gate compares the loop's
  own overhead — everything that is not the provider round-trip or the tools'
  work — against a committed baseline with tolerance bands, and a permanent
  teeth-check proves the gate still catches a real regression. Measured on the
  reference machine the steady-state loop reads p50 ≈ 175µs per sample, inside
  the design budget.
- **Cache telemetry is visible everywhere usage is.** Prompt-cache read and
  creation tokens now surface with a hit-ratio percentage in the TUI, `--json`
  stream, and ACP wire, derived at one shared site instead of three renderers.

### Changed

- **The committer no longer serializes or masks anything a turn can prepare.**
  Turn-originated entries arrive at the actor pre-serialized and pre-masked
  (`MaskedBytes` — unmaskable bytes are unrepresentable by construction); the
  actor validates, mints the id, splices the envelope through the same serde
  path, and forwards. A 60KB tool result no longer stalls every other session
  command behind the sole committer, and log bytes are provably unchanged.
- **The stream no longer waits on the disk per entry.** Intra-turn commits are
  pipelined behind a bounded ack window with three hard barriers (before any
  tool runs, before the sample-boundary refresh, at turn end), and the writer
  drains its queue into one `write_all` + one `sync_data` — windowless group
  commit, no timer, no new loss window. fsync-before-ack and
  projection-advances-only-on-ack hold verbatim; kill-between-enqueue-and-sync
  is a golden scenario.
- **A sample boundary costs one fsync, not two.** The Completed pair
  (assistant item + usage) commits as one causally-atomic group — one writer
  message, one sync, one ack.
- **The next request is built and sent while the boundary settles.** At each
  sample boundary the commit, the snapshot refresh, and the next provider
  request fire concurrently; the in-flight stream is adopted only if the
  refreshed head proves nothing intervened, else it is cancelled and rebuilt
  sequentially — transcripts are byte-identical either way. A mispredict
  (e.g. a steer landing at that exact boundary) costs one cancelled request;
  its billed usage is currently *not* folded into reported usage — a recorded
  follow-up. Snapshot delivery itself moved from a mailbox round-trip to an
  epoch-fenced watch channel published by the actor only after durability.
- **Connections are warm before the first token needs them.** The HTTP client
  now negotiates HTTP/2 with keep-alives (the workspace was HTTP/1.1-only),
  and the pool is armed — one lightweight, credential-free handshake request —
  at `hotl -p` startup and TUI session open, moving DNS+TCP+TLS off the
  critical path after every idle window. Arming is failure-invisible and
  RAII-scoped; idle sessions hold nothing.
- **Small loop diets.** Single-tool batches (the majority case) execute inline
  in the turn task instead of through the parallel-chunk machinery; doom-loop
  signatures fold as results arrive instead of in a batch pass; the builtin
  tool registry is memoized instead of rebuilt per spawn-gate check; and hook
  dispatch is gated on a live per-event mask, so a session with no hook for an
  event pays one atomic load instead of payload construction and cap copies.

## [0.5.2] - 2026-07-25

### Fixed

- **`sandboxed:` in an ask now means the sandbox was proven on this host**, not
  that the mechanism exists on disk. `probe()` used to answer "enforced" the
  moment it saw `/usr/bin/sandbox-exec`, and that one boolean is the sole gate
  on `bash` allow-rules auto-approving without a human — so a profile that
  failed to apply meant silent auto-approval of *unconfined* shell commands. It
  now spawns a real sandboxed child that tries to write outside the
  confinement, and reports `Enforced` only if that write fails and leaves
  nothing behind. One spawn per process, bounded at 2s, and a host that cannot
  demonstrate confinement degrades to the loud `UNSANDBOXED` posture instead of
  claiming one. If neither `/var/tmp` nor `$HOME` is writable, set
  `HOTL_SANDBOX_PROBE_DIR` to somewhere outside the working directory and
  outside `TMPDIR`.
- **Linux: `truncate(2)` by path escaped the floor on every kernel.** The
  Landlock ruleset was pinned to ABI v2, and Landlock only restricts rights it
  is asked to *handle* — so `LANDLOCK_ACCESS_FS_TRUNCATE` (ABI v3, Linux 6.2)
  was never requested and an approved command could zero any file on the host.
  The handled set is now v3. Note this is not reachable via `truncate -s 0` or
  `> file`, which open the file for writing first and were already denied; it
  took a raw `truncate(2)`, which is why it survived.
- **Provider credentials no longer reach child processes.** `bash`, `grep`'s
  `rg`, diagnostics and owner shell hooks inherited the full environment, so
  `ANTHROPIC_API_KEY` was one `env` away from any auto-approved command. The
  scrub is deliberately narrow — provider keys only — because the obvious
  broad heuristic would also strip `GITHUB_TOKEN` / `CARGO_REGISTRY_TOKEN` /
  `NPM_TOKEN` and silently break `gh`, `cargo publish` and `npm publish`.
- **`web_fetch` shows you the whole URL.** The ask read `web_fetch:
  pastebin.com` while its own code comment noted that a fetch exfiltrates via
  the URL itself — so the one thing you needed to see (`?d=<your ssh key>`) was
  the one thing hidden. The ask now carries the full URL, elided only with an
  explicit remaining-character count.
- **`web_fetch` no longer follows a redirect off the public web into your
  private network**, and refuses cloud instance-metadata addresses
  (`169.254.169.254` and friends) on every hop including the first. An allowed
  public host could previously 302 into the metadata service and hand instance
  credentials to the model, having been approved as `web_fetch: example.com`.
  A chain that *starts* private still works — "fetch my dev server" is a real
  workflow, and you saw and approved that target.
- **A failed HTTP client build is reported instead of silently downgraded.**
  `web_fetch`/`web_search` fell back to a bare client with **no redirect policy
  and no timeout**, quietly deleting the per-hop egress re-check and letting a
  hung origin hold a request permit forever. A panicked fetch task is also no
  longer reported as "cancelled".
- **The egress proxy is bounded, capped and authenticated.** An unfinished
  request head pinned a task and a socket for the life of the process (now a
  10s timeout), accepts were unbounded (now 64 live connections, then 503), the
  loopback proxy was unauthenticated so any local process could spend your
  allowlist, and a request carrying two `Host:` headers had the policy check
  one value while the upstream honored the other — that is refused now, not
  resolved first-wins.
- **Post-edit diagnostics output is defanged.** A compiler error quoting a file
  the model just wrote could carry a forged `</diagnostics>` and reclaim the
  surrounding context. Every other untrusted path in the workspace already did
  this; diagnostics was the one that did not.

- **The transcript scrolls without vim mode.** `PageUp`/`PageDown`,
  `Ctrl-Home`/`Ctrl-End`, and the mouse wheel now scroll; previously scroll
  was reachable only from vim Normal mode, which `[behavior] vim_mode`
  (default `false`) makes unreachable. Set `HOTL_MOUSE=0` to keep terminal
  text selection. Bare `Home`/`End` became real line motions in the input.
- **Bracketed paste.** A multi-line paste is inserted as text. It used to
  arrive as one `Enter` per line and submit one turn per line.
- **The permission badge shows the real mode.** The session's effective mode
  now travels on the wire (`session/new` result and a `mode_changed`
  notification) and is always displayed. The badge previously read `ask`
  unconditionally while `hotl setup` writes `mode = "auto"` — so the shipped
  default auto-approved mutating tool calls while the UI rendered exactly as
  if it were prompting per-action.
- **The hint row no longer advertises dead keys** while a permission ask is
  open during a `Ctrl-R` search, and a modal now clears the live search
  (tech-debt #13).
- **`hotl watch` restores every terminal mode it set**, and the signal-path
  restore now disables mouse reporting and bracketed paste too — a killed TUI
  no longer leaves your shell emitting escape sequences on mouse movement.
- **Library code no longer writes to stderr mid-TUI.** `build_registry`
  returns its warnings; one caller, outside the terminal guard, prints them
  (T3-23, the half this crate owns).

### Changed

Four hardening changes below can break a workflow that used to work. Each has
a named opt-out, and every opt-out is **labeled in the ask** — there is no
silent way to run with the denial lifted.

- **Linux kernels 5.13–6.1 lose `bash` auto-allow by default.** Those kernels
  have Landlock but not the truncate right (see above), so the floor is
  genuinely partial and hotl no longer certifies it. RHEL 9 (5.14) and Ubuntu
  22.04 (5.15) are the common cases. `HOTL_SANDBOX=best-effort` accepts the
  partial floor and labels every ask `sandboxed:landlock(partial)`; upgrading
  the kernel is the other answer.
- **macOS: the container-daemon socket class is denied.** `docker.sock`,
  `podman.sock`, `containerd`, `crio` — a unix-socket connect is a *network*
  operation, not a file write, so the write floor never covered it, and the
  Docker API is a complete escape (mount the host root, write anywhere). It
  survived `egress = "off"` too. `HOTL_UNIX_SOCKETS=open` restores
  docker-in-the-loop workflows and marks every ask `unix:open`.
  `ssh-agent`/`gpg-agent` stay reachable, so `git push` over SSH is unaffected.
  **On Linux this is not enforceable at all** — Landlock has no rule covering
  `connect(2)` to a pathname socket at any ABI — and hotl makes no claim there.
- **macOS: Apple Events from a confined command are denied.** `osascript -e
  'tell application "Terminal" to do script …'` runs its payload in a process
  that is *not* a descendant of the sandbox. Plain AppleScript still works;
  only the cross-application event send is refused.
  `HOTL_MACOS_AUTOMATION=allow` restores Apple-Event-driven Xcode/Simulator/
  Instruments flows and marks every ask `automation:allow`.
- **`HTTP_PROXY` under `egress = "allowlist"` now carries a credential**
  (`http://hotl:<token>@127.0.0.1:<port>`). curl, git-over-HTTP, pip and cargo
  all forward it as `Proxy-Authorization` and are unaffected — verified end to
  end. A client that honors the proxy host but drops proxy credentials gets a
  `407`; `HOTL_PROXY_AUTH=off` restores the previous behavior. The token is
  explicitly *not* cryptographic: it separates local processes for one session
  and is visible to anything running as you, which is exactly the boundary it
  does not claim to defend.

- **A `web_fetch` at a private, loopback, or cloud-metadata address is now a
  *protected* ask** — the same escalation `write`/`edit` give an execute-later
  path, so it prompts in every mode including the default `auto`. Public hosts
  are an ordinary ask, unchanged.
- **`-p --json` schema version 2.** `turn_done.outcome` is now the tagged
  object `{"kind": …}` instead of a Rust `Debug` string, and
  `thinking_delta` carries `text`. Consumers pinned to v1 must update. The
  frame schema is now pinned by a test — nothing pinned it before, which is
  how the `Debug` string survived inside a stream documented as a contract.
- **`hotl attach` renders every update type**, not the four it handled
  before (it silently dropped denials, auto-allow rules, retries, fallbacks,
  queued prompts, todos, and thinking).

### Added

- Model thinking renders collapsed in the transcript (`ctrl-t` expands);
  `HOTL_THINKING=0` disables it.
- Cache reads, session token totals, and context fullness on the status strip.
- `/help`, `/status`, `/cost`, `/clear`, `/quit`.
- `hotl -p -` reads the prompt from stdin; `@[file]` works in the console too.
- **TUI command completion.** Typing `/` opens a filtering popup of every
  built-in command and loadable skill: `↑↓` picks, `tab` completes, `enter`
  runs, `esc` dismisses. `initialize`'s `skills` field now carries
  `{name, description}` objects instead of bare strings; compatibility runs
  one way — a newer TUI still reads the old bare-string shape from an older
  engine, but a client parsing the documented bare-string shape needs
  updating to read this one. Descriptions cost nothing by default: the
  always-sent tool description omits them, and the model only sees one if
  it explicitly queries the skill tool.

## [0.5.1] - 2026-07-24

### Changed

- **The console's vim input editor is now opt-in.** `[behavior] vim_mode`
  defaults to `false`: the input is a plain insert-mode field, and `Esc` on an
  empty input keeps its "interrupt the turn" meaning. A modal editor ambushes
  anyone without the muscle memory — one stray `Esc` and typing stops
  inserting — so it now waits to be asked for. Set `[behavior] vim_mode = true`
  to get motions, operators, counts, and `Ctrl-e`/`:e` back. `hotl watch`'s
  separate `[settings] vim_mode` is unaffected and stays **on**: there the
  letter keys are additive over a read-only list, and arrows, `enter`, `q`, and
  `r` work either way.

### Fixed

- **`hotl watch` never pinged for hotl's own console.** The detector knew only
  the plain-CLI ask (`allow …? [y/N]`), so a console TUI sitting on a
  permission card — or an `ask_user` question — read as *unknown* rather than
  *blocked*: no ping, no color, nothing to jump to. The one agent watch should
  know best was the one it couldn't see. It now reads the phase the console
  already publishes in its terminal title (`— waiting on you` / `— working`),
  which tmux records per pane, with the card's own hint row as a backstop. The
  title is what survives a long session, where the card sits too far up the
  screen for the captured tail to reach.

## [0.5.0] - 2026-07-24

### Added

- **`recall` — a pluggable retrieval seam.** Configure `[[retrieval]]`
  backends (any stdio MCP server exposing a search tool) and the model gains
  one `recall` tool for conceptual search over your notes/docs corpora.
  Results arrive as provenance-tagged, untrusted-enveloped tool results;
  first use of a backend raises the same protected trust screen as MCP.
  Nothing is configured by default, and no built-in backend touches the
  network. (Design: agentic search stays the default; `recall` is for
  corpora that outgrow grep.)

### Fixed

- **Steering while a tool ran could break the rest of the session.** The steer
  was appended the moment it arrived, which put it between the assistant turn
  that called the tools and the results answering them. The provider then saw
  the results as a turn whose predecessor made no tool calls at all, and
  rejected every later request — on Bedrock-style endpoints as *"the number of
  toolResult blocks … exceeds the number of toolUse blocks of previous turn"*.
  Steers that arrive mid-batch are now held until the results land (the model
  still sees them at the same point — the next sample happens after the batch
  closes), a turn that dies mid-batch closes the calls it left open, and
  sessions already written this way are repaired as they resume.
- **Provider errors read as a sentence instead of a JSON dump.** The full
  response body used to be printed verbatim; the message is now pulled out of
  whichever shape the provider uses (`error.message`, `message`, an
  `x-amzn-errortype` header), rendered as `HTTP 400 ValidationException: …`,
  and clipped rather than dumped when it runs long.
- **A signal no longer leaves your terminal wedged.** Both TUIs restored the
  screen only when their guard dropped, so anything that killed the process
  outright — a real `SIGINT`, a `SIGTERM`, closing the window (`SIGHUP`) —
  left the terminal in raw mode inside the alternate screen: no echo, no
  cursor, the shell prompt drawn invisibly over the dashboard, and a second
  Ctrl-C needed before the terminal was usable. Ctrl-C normally reaches the
  console as a key, so this only showed up once something restored sane tty
  modes underneath it. The restore now also runs from a signal handler and a
  panic hook, and the process exits `128+signo`.

## [0.4.1] - 2026-07-23

### Changed

- **The console transcript is easier to read at a glance.** Every turn now
  carries a fixed marker in the left gutter — `❯` you, `●` the assistant
  (with a `│` bar down multi-line answers), `✓`/`✗`/`⛔` tools, `⤷` steer,
  `·` notice — each in its role color, so the shape of the conversation is
  visible by scanning down. Assistant answers also get light structure:
  `#` headings bold, `-`/`*` bullets with a `•` marker, and fenced or
  indented code on a muted band. Tool cards drop the `[name]` brackets; the
  glyph moved to the gutter and the name keeps its status color.
- The transcript now defaults to **comfortable** spacing — a blank line
  between turns and a small left gutter. Set `[settings] density = "compact"`
  for the previous edge-to-edge look, or `"spacious"` for more.

### Added

- **`[settings] density`** — `compact` | `comfortable` | `spacious`, the
  console transcript's vertical spacing and gutter width. Unknown values
  warn and fall back to comfortable.
- **`warm` theme preset** — a low-blue palette (paper-white ink, amber
  accent, terracotta) for a less clinical console. Opt in with
  `[settings.theme] preset = "warm"`; the default stays `tokyo-night`.
  Note that font size and family are set in your terminal, not hotl — see
  the docs' "Making it warmer".

## [0.4.0] - 2026-07-23

### Changed

- **Skills load lazily.** The `skill` tool used to advertise every skill
  name *and* a 150-character description on every single request — about
  980 tokens for a 24-skill roster, whether or not a skill was ever used,
  and growing with each one added. The always-sent index is now grouped by
  source with descriptions dropped, and any source over 12 skills
  collapses to a few names plus a count, so the cost grows per source
  rather than per skill: registering a 300-skill marketplace adds one
  line, not 300 names. On that same roster it measures 149 tokens against
  978 — an 85% cut.

  Because collapsed skills are no longer named up front, the tool gained
  two ways to find them: `{"query": "…"}` ranks every skill — collapsed
  ones included — against its full description and returns the best
  matches, and `{"source": "…"}` lists one source outright. Loading by
  `{"name"}` is unchanged, as is `hotl skills`, which still prints the
  whole roster with full descriptions.

### Added

- `/<skill>` in the console TUI loads a skill by name and follows it,
  with the rest of the line passed as arguments
  (`/brainstorming redesign the parser`). Built-in commands like `/rename`
  are matched first; an unrecognised name prints a notice and costs no
  turn. This is the manual override for a skill the agent doesn't think to
  search for. ACP `initialize` now returns the skill names so any front
  end can offer the same thing.

- Endpoints that authenticate for you: `[provider] auth = "subscription"`
  (env `HOTL_PROVIDER_AUTH`) runs hotl with no credential of its own, for
  operator-provisioned endpoints — corporate gateways that terminate auth at
  the edge, internal proxies fronting Bedrock or Vertex. It is not a way to
  spend a personal Claude subscription, which Anthropic's terms restrict to
  Claude Code and claude.ai; the gateway guide says so plainly, since the
  wrong route is easy to find. The setting is provider-neutral —
  identical for `anthropic/…` and `openai/…`. Requires `base_url`, and
  fails at startup without one rather than as a mid-session 401. Any API
  key in the environment is discarded rather than forwarded, so a local
  endpoint never receives a production credential by accident.
- `[provider] base_url` now applies to the `anthropic` provider too (env
  `HOTL_ANTHROPIC_BASE_URL`), so any Anthropic-shaped endpoint is
  reachable. Both `https://host/v1` and the bare `https://host` resolve.
  `hotl doctor`'s gateway check follows the active provider instead of
  only ever probing the OpenAI base URL.

### Fixed

- The TUI wraps long lines instead of clipping them, in both the
  transcript and the input. A multi-line input buffer showed only the
  cursor's line, the input box was fixed at three rows, and transcript
  output was cut at the right edge. The input now grows to ten rows and
  scrolls to keep the cursor visible.

## [0.3.0] - 2026-07-22

### Added

- Skill marketplaces: register extra skill sources with
  `hotl skills add <name> <git-url|path>` (plus `list` / `update` /
  `remove`) or a `[skills.marketplaces]` map in config.toml. Git sources
  are cloned under `~/.config/hotl/marketplaces/<name>` and touch the
  network only on explicit `add`/`update`. Skills resolve bare or as
  `<marketplace>:<skill>` when a name collides.
- Named sessions: start one with `-n/--name` (TUI, `hotl bg`, headless `-p`),
  rename mid-session with `/rename <name>` — the TUI's first slash command.
  The name shows as a badge above the input, in the terminal tab title, and
  in the resume picker.
- `hotl -r [arg]` resume flag (same path as `hotl resume`): bare lists
  sessions; the arg accepts the picker number, an id-prefix, or a name.

### Fixed

- Interrupts are delivered everywhere they can land: during the compaction
  window (the continuation respawn no longer reuses a token carrying a
  swallowed cancel) and while a permission ask is pending (cancel waits for
  the answer instead of ending the turn). The session actor holds a weak
  sender, so dropping a handle exits the task rather than leaking it — with
  its log fd and projection — per spawned subagent or replaced ACP session.
- ACP `session/new` and `session/load` interrupt the replaced session's
  in-flight turn, which otherwise kept running tools invisibly in the shared
  working directory. `session/load` auto-continues an interrupted turn again.
- Shell hooks: the stdin payload write runs inside the hook timeout, so a
  hook that never drains stdin times out at 10s instead of wedging the turn.
- Anthropic in-stream SSE errors carry their canonical HTTP statuses
  (`overloaded` → 529, `rate_limit` → 429, `api_error` → 500), so the
  fallback chain and retry classifier can see them.
- MCP `tools/call` gets a 600s leash (protocol chatter stays at 30s), and a
  timed-out request sends `notifications/cancelled` so the server stops work
  instead of racing a retry into a duplicate.
- `hotl serve`: a live socket is never stolen (connect-probe before unlink),
  the exit guard only removes the socket it bound, a second `hotl attach`
  takes over cleanly, and accept failures back off instead of busy-spinning.

## [0.2.0] - 2026-07-21

The execute harness ships: hotl is now a human-on-the-loop terminal AI agent,
with the original dashboard aboard as a subcommand.

### Breaking

- Bare `hotl` is now the **agent**; the tmux dashboard moved to `hotl watch`.
- Crate identity swap on crates.io: `hotl-types` and `hotl-tui` now hold the
  harness's conversation types and agent console. The watch-era code they
  shipped through 0.1.5 lives on as `hotl-watch-types` and `hotl-watch-tui`.

### Added

- Agent harness: steering console TUI and `-p` headless mode, against any
  Anthropic or OpenAI-compatible model (`HOTL_MODEL=provider/model`).
- Permission gate on every mutating or executing tool call — `auto` (default)
  or `ask` mode — under a kernel sandbox floor (Seatbelt on macOS, Landlock
  on Linux) confining `bash` writes to the working directory. Writes to
  execute-later paths (git hooks, shell rc, Makefiles, agent-instruction
  files) always ask, in every mode. `--features security-enforced` builds
  make prompting impossible to disable by config.
- Append-only session log with `hotl resume`, `hotl undo` (git snapshots
  around every mutating step), and non-destructive context compaction.
- MCP client (stdio), ACP server (`hotl acp`), background sessions
  (`hotl bg` / attach), `hotl doctor` setup check.
- Theme presets shared by both surfaces (`tokyo-night` default);
  `[settings.theme]` in `~/.config/hotl/config.toml`.
- Fifteen internal library crates first published in lockstep with the
  binary (`hotl-engine`, `hotl-tools`, `hotl-provider*`, `hotl-watch-*`, …).

## [0.1.5] and earlier

Watch-only releases: bare `hotl` was the tmux dashboard that is now
`hotl watch`.
