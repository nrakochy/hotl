# Changelog

Notable changes to hotl. Pre-1.0, breaking changes land at every 0.x minor;
the internal library crates version in lockstep with the binary and carry no
semver promise of their own.

## [Unreleased]

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
