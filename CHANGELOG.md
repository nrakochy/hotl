# Changelog

Notable changes to hotl. Pre-1.0, breaking changes land at every 0.x minor;
the internal library crates version in lockstep with the binary and carry no
semver promise of their own.

## [Unreleased]

### Fixed

- **Entry ids minted in one process now strictly increase**, even within a
  single millisecond. A plain ULID orders same-millisecond ids by its random
  half, so a log span could read as running backwards on a fast machine — the
  nix leg of CI caught it in the fold-span test.

## [0.27.0] - 2026-09-10

### Changed

- **The transcript is quieter, and work reads as a second voice** (plan 0061).
  Tool cards sit a column in from prose, muted, wearing a title-cased verb
  (`Bash`, `Read`, `Wrote`) and a quiet `→` on success — only failure (`✗`) and
  denial (`⊘`) stay loud. A settled card moves its clock to a faint result row
  that also says how much came back (`└ 1,204 lines · 8s`). A `bash` card
  elides the `cd …&&` prefix to the directory it lands in
  (`hotl ❯ cargo test`). A closed run of settled, successful cards folds into
  one line — `→ ran 2 shell commands, read 1 file · 3s` — and **Ctrl-O**
  unfolds every rollup, the way Ctrl-T unfolds reasoning. A sub-agent card is
  now a two-row block under a bar: the brief on top, `N calls · 43s ·
  Read c5.rs · ↑↓ agents` beneath it. Every turn closes with one faint line
  saying what it cost: `✻ 2m 14s · 6 calls · done 10:27`.
- **Assistant prose renders inline `code` and `**bold**`** (plan 0061), and
  `1.` numbered lists take the accent the way bullets do. Themes gain a ninth
  role, `strong`, for emphasis — set it under `[theme]` like any other slot.

### Added

- **The console says what it is waiting for** (plan 0061). A running `bash`
  shows its newest output line on one faint row, with a live line count on the
  card and on the strip; ten seconds of silence reads `quiet 12s` in the
  blocked color, on the card and on the strip alike. A call queued behind the
  subprocess budget parks a hollow card that says how many are ahead of it.
  Esc now shows: the strip reads `interrupting · 3s` and the hint names the
  second press. A provider backoff shows its countdown — `retrying 2/5 ·
  HTTP 429 · 4s left` — with the wave at rest, because nothing is computing.
  A history fold says `folding history…` for the whole time the actor is
  blocked on it, and a goal evaluation says `◎ evaluating`. A paused goal
  finally reads `◎ /goal paused` instead of `active`.
- **`thinking` counts thinking** (plan 0061). Reasoning deltas no longer flip
  the strip to `writing · ~0 tok`; the phase stays `thinking` and reports the
  tokens it actually thought.
- **New stream frames**, all additive (plan 0061): `tool_progress` (a running
  bash's newest line and raw counts, never persisted), `tool_queued`,
  `compacting`, `goal_evaluating`; `tool_done` gained `lines`/`bytes`, and
  `retrying` gained `delay_ms`, `max`, `status` and `scope` — so the digest's
  and the goal evaluator's own retries are reported instead of swallowed.
  `hotl attach` renders each of them; `hotl -p` output is unchanged.

### Fixed

- **A dead session server said nothing** (plan 0061). The console exited with a
  bare code; it now prints one line — `hotl: the session server closed the
  connection` — after the screen is restored. A failed write to the server is
  treated the same way instead of being discarded.
- **The echo of what you typed waited behind image loading** (plan 0061). The
  frame is painted before the command queue drains, so a dropped 4MB image no
  longer holds your own prompt off the screen.
- **A running sub-agent call showed no elapsed time** in the drill-in (plan
  0061) — the one place a stuck child could hide. It now reads on the parent's
  clock.

## [0.26.0] - 2026-09-10

### Removed

- **`hotl undo` and the shadow-git snapshot store** (plan 0054). Session
  close waited up to 2s for queued snapshots to drain — the one place the
  feature still sat on a user-visible path — and the store kept a second,
  history-retaining copy of workspace files under the data dir. Both are
  gone: no per-session `shadow/<ulid>.git`, no `undoStatus` on the prompt
  reply (an additive field; clients that read it see it absent, as they
  already did for sessions without git), no `undo` strip chip, no doctor
  undo check. Revert the agent's edits with your own VCS. `hotl gc` and
  `[retention]` still sweep legacy `shadow/` dirs from earlier releases.
  The always-on safety floor is now: kernel sandbox, protected-path
  escalations, deny rules, secret masking, transcript visibility.

### Added

- **Sub-agents return a typed result** (plan 0058). Every child gets
  `report_result` and answers through it, not through prose: an outcome, a
  bounded summary, the files it touched, commits, and `path:line` citations.
  Its brief now arrives twice — inline and as a `TASK.md` it is told to read
  first — carrying the parent's decisions whole plus only the plan steps the
  brief names, so a child knows what was already settled without being invited
  to overstep. A child that stops without reporting is nudged twice, then its
  last words become `{"outcome": "unverifiable"}`. The parent always gets a
  shape.
- **A running sub-agent is watchable** (plan 0058). A child's own prose
  streams to the parent (`child_text`), and one `agent` frame carries its whole
  token bill, instead of a stalled card.
- **A per-turn `delegation` frame** (plan 0058): how many sub-agents ran, how
  many paths more than one of them touched, and how many claimed `completed`
  while the caller's own optional `validate_cmd` disagreed. That command is
  named in the spawn approval, so nothing runs you did not see.
- **Workflows can stop and ask you** (plan 0058). A `human_input` phase
  collects its fields, puts its actions to a person, and routes the run by what
  they choose — including back to an earlier phase. `hotl approve <session>`
  answers what a backgrounded session is waiting on, from a second terminal,
  without taking the session over.
- **`workflow {resume: "<run_id>"}`** (plan 0058) replays the agent calls whose
  content is unchanged and re-runs from the first that differs. A run that died
  on its fourth phase no longer re-runs three.
- `[agents] prefix_stagger_ms`, `child_idle_secs`, `completion_grace_secs`
  (plan 0058).

- **`[provider] effort` also takes a per-phase table** (plan 0059) — `effort =
  { plan = "xhigh", implement = "high", verify = "xhigh" }` — applied once at
  turn start, so every sample in a turn is billed at one depth. `plan` when
  plan mode is on or the plan has open steps and none is in progress, `verify`
  when the previous turn's last batch ran a test runner and edited nothing
  (`[behavior] verify_commands` extends the built-in table), else `implement`.
  `/effort <rung>` pins the session and stops the schedule; a bare `/effort`
  names the schedule while it governs.
- **`[provider] utility_model`** (`HOTL_UTILITY_MODEL`, plan 0059): the model
  for one-off calls you pay for and never see — compaction digests and
  `/goal` evaluations. Resolves `utility_model` → `fast_model` → the session
  model, from one place, so the two cannot drift onto different models.
- **`[behavior] max_tool_calls` and `max_cost_usd`** (`HOTL_MAX_TOOL_CALLS`,
  `HOTL_MAX_COST_USD`, plan 0059): session-level caps, not per-turn. The cost
  cap is checked before each sample goes out, priced at that sample's worst
  case; an uncatalogued model has no price and is never refused (hotl warns
  once at startup instead of guessing). Crossing 50%, 80% or 100% raises one
  notice each, and the console strip carries a spend meter that turns amber at
  80%.
- **`[behavior] ask_expiry_secs`** (default 3600, plan 0059): a permission ask
  parked while no client is attached is denied after this long, the denial is
  recorded in the session log, and the turn carries on. `0` restores the old
  wait-forever behaviour.

- **Typed plan steps** (plan 0056). A `todo_write` item is a plan node now: a
  stable id, the ids of the steps it depends on (resolved and cycle-checked
  when you send them), an `acceptance` sentence, the exact `validate_cmd` that
  proves it, and a `replan` flag. Two new statuses — `failed` and
  `needs_more_steps` — join the three you had. The strip shows a failed step
  as its own number (`3/7 ·1!`).
- **The plan outlives the session** (plan 0056). Every `todo_write` writes
  `<xdg-data>/hotl/plans/<project-id>/current.json` and `current.md`, filed by
  the repo's `origin` remote (so two worktrees share one plan) or the working
  directory. A fresh session in a project whose plan still has open steps gets
  one reminder naming the file. `[plan] repo_dir` mirrors the markdown into
  your repo as `hotl-plan.md`. Compaction summaries read the live plan and its
  decisions, so a fold cannot lose the plan it was working to.
- **A decisions log** (plan 0056). `todo_write` takes `decisions` — what you
  chose and why — which append rather than replace, and ride the plan
  artifact.
- **`present_plan`** (plan 0056). In plan mode the model gets one extra tool:
  the only way a plan reaches you. The TUI shows a card — press `a` to approve
  (plan mode drops and the model implements one step at a time) or `r` to say
  what to change. ACP clients approve with `session/set_plan`; `-p --plan`
  prints the summary and the numbered steps, and exits 3 naming the paths if
  the tree changed anyway.
- **`/goal` conditions the harness decides itself** (plan 0056), with no model
  and no tokens: `tests_green("<cmd>")`, `file_exists("<path>")`,
  `output_matches("<cmd>", "<glob>")`, `no_edits_since_verify`, `turns <= N`,
  joined with `and`/`or` and parentheses. A false check settles the turn on the
  spot; only the prose remainder reaches the evaluator. A wholly prose
  condition behaves as before.

- **A context ladder: hotl reclaims cheapest-first before it summarizes**
  (plan 0057). At 60% of the window, tool results older than the last four
  user turns become one-line `<cleared tool_use_id="…"/>` stubs — no model
  call, no history lost, the session log still holds every byte — and only
  if that isn't enough does the 80% fold pay for a summary. Clearing never
  touches the last four turns, the turn in flight, or a `skill` result, and
  happens at most once per prompt. `[context] keep_results` tunes the window
  (`0` turns the rung off). The strip says `cleared N results`.
- **`recall` is always available, over your own session history** (plan
  0057). A built-in `session-log` backend indexes the session log — this
  session's and every session it was resumed from — so the model can go and
  look at what it can no longer see: a result the ladder cleared, a detail a
  summary flattened, the exact words you used fifty turns ago. Hits are
  reported as `session:<id>#<entry-id>` and carry one clause the model is
  meant to act on: they are historical and untrusted, true when written, so
  verify against the workspace first. Nothing is configured and nothing
  leaves your machine. **If you had configured a `[[retrieval]]` backend you
  now have two, so `recall` calls must name one with `backend`.**
- **`PreCompact` and `PostCompact` hooks** (plan 0057). A `pre_compact` hook
  is told which tool results are about to be folded away
  (`{"foldedIds":[…],"keptFrom":…,"estimatePct":…}`) and answers
  `{"pin":["t7"]}` to keep them verbatim through the fold; `post_compact`
  receives the digest the model will read from then on. A hook can pin,
  never veto — a fold the window needs is not a hook's to refuse — and a
  hung one folds with no pins. See [hooks.md](https://hotl.dev/hooks/).

- **`/goal` pauses instead of spinning, and gives up when only you can fix
  it** (plan 0051). Eight consecutive goal turns in which no tool ran now end
  the loop with `◎ goal paused …` — the goal stays set, and your next prompt
  re-arms it with a full budget. The brake caps *idle* turns, not turns: a
  turn that ran a tool resets the count, so a goal making progress is still
  unbounded and the condition text remains the way to bound it. A denied or
  hook-blocked call is not progress, so plan mode and a `dontask` session
  stall out cleanly rather than burning tokens on talk. Separately, an error
  only you can clear — a revoked key, a 401/402/403/404, a context window
  compaction can no longer make room in — now clears the goal with
  "run /goal again to continue" instead of leaving the loop armed against a
  dead credential across a resume. A rate limit or an outage still leaves it
  armed, because retrying is what fixes those.

- **`bash` and `grep` take an optional `expect`** (plan 0050). State what you
  predict a call returns — `{"exit": "zero"}`, `{"contains": "…"}`,
  `{"empty": true}`, or `{"matches": "some"|"none"}` for `grep` — and a
  result that misses the prediction stops the rest of that batch instead of
  running every remaining call against a belief that is already wrong. The
  surprising call keeps its real result plus an `[expectation failed: …]`
  trailer; the calls after it report which call surprised the model; one
  reminder says so in the transcript, and the turn carries on. A batch
  without `expect` behaves exactly as before. Expectations are stated by the
  model, never assumed by hotl — running tests to reproduce a failure is
  routine, and a guessed "should pass" there would stop work you meant to
  do. `turn_done` gains a `mispredictions` count (omitted when zero).

### Fixed

- **A `/goal` loop that folds or clears no longer under-reports its own
  spend.** The compaction summarizer's tokens, the segment a respawn ended,
  and a clear's abandoned digest now reach `GoalState::spent`, so the progress
  line and `goal_verdict.usage` match the turn's reported total. The session
  cost cap is unchanged — its meter still reads the turn's own samples.

- **A sub-agent could hang forever** (plan 0058). Two clocks bound one now, and
  neither discards an answer: a silent child ends `unverifiable`; a child that
  reported and then hung is reaped and its result kept. A child out of turns
  gets one wrap-up prompt whose only tools are reads and `report_result`.
- **`ask_user` questions raised while nothing was attached were answered by
  nobody and never re-issued** (plan 0058). They now park and re-issue on
  attach exactly like permission asks. Inside a sub-agent, `ask_user` no longer
  says "proceed with your best judgment" — nobody is reachable from there, so
  it says to finish with `report_result {outcome: "needs_input"}`.

- **A model with a denser tokenizer was budgeted as if it were ASCII** (plan
  0057, tech-debt #63). The per-model chars-per-token ratio from the model
  catalog now drives every estimate hotl makes — the compaction trigger, the
  clear trigger, the spill threshold and `/context` — so the fullness number
  you see and the number that folds your history read the same ruler. No
  fold point moves for the models where the catalog agreed with the old
  fixed ratio; models the catalog rates as denser-per-character now fold
  slightly later rather than slightly early.

- **A parallel tool batch no longer forks a child per call** (plan 0055). The
  concurrency budget's `subprocs` limit existed but nothing drew on it: a
  12-call batch of parallel-safe calls forked 12 children at once, and four
  sub-agents doing the same forked 48. Every executed call now takes one
  permit from the session's share of the one process-wide budget, so a wide
  batch queues instead of stampeding — parent and children draw from the same
  pool. Parallelism is unchanged up to the budget (`[concurrency] subprocs`,
  default 8); gating stays serial and results still pair in source order.
  Tools that block on a nested session (`spawn`, `workflow`) take no permit:
  holding one across a child's own tool calls would deadlock a small budget.

- **The prompt-cache marker budget is enforced in release builds** (plan
  0055). The Anthropic four-breakpoint cap was a debug assertion, so a release
  build that planned a fifth `cache_control` shipped it and the API answered
  400. The bound is structural now (the anchor plan is truncated to the
  newest two) with a runtime check behind it, and CI runs the provider's tests
  under `--release` — the only build that exercises it.

- **1M-window models were compacting at 160K** (plan 0050). The model
  catalog knew every model's context window and nothing ever asked it: a
  session with no explicit `[context] window` got a hardcoded 200,000
  regardless of the model, so Opus 4.8 folded its history at a sixth of the
  window it actually had — and an 8K local model overflowed on turn two.
  The catalog now decides, `HOTL_CONTEXT_WINDOW` and `[context] window`
  still win outright, and a model the catalog does not know says so at
  startup instead of guessing in silence.

- **Plan mode was invisible to the model** (plan 0050). Plan put `write` and
  `edit` behind an ask and told the model nothing, so it kept proposing
  edits it could not make and burned turns discovering the wall. Turning
  plan on now drops those two tools from the roster the model is offered and
  says, in one reminder, what is unavailable and what a finished plan looks
  like; turning it off restores both and says that too. Sessions started
  with `--plan`, `HOTL_PLAN`, `[permissions] plan` or an ACP
  `session/new` get the same reminder in their opening context. `bash` keeps
  its existing ask.

- **A provider stream that died mid-answer ended the turn** (plan 0050). The
  retry ladder only covered the request *before* the first byte arrived, so
  an overloaded provider (529) or a proxy dropping a live connection one
  paragraph in cost the whole turn — the most expensive failure a long turn
  can take. The sample is now re-sent against the same snapshot, at most
  twice, with the partial answer taken back off the screen; a stream that
  had already sealed a tool call is never re-sampled, and auth failures
  still fail immediately.

- **Dropped and pasted images land as `[Image #N]` on Windows** (tracker
  #108). The drop classifier only knew POSIX path shapes (`/`, `~`, `./`,
  `../`), so a Windows Terminal drag-and-drop and the temp PNG that `Ctrl-V`
  writes under `%TEMP%` both inserted as literal text, and the bare-path
  unescape would have eaten their backslashes. Drive-letter paths
  (`C:\…`, bare or quoted, backslashes kept as separators) now compact like
  any other drop; drive-relative and UNC forms still insert literally.

### Changed

- **A sub-agent no longer inherits `bypass`** (plan 0058). Your allow and deny
  rules still apply inside children; the mode is forced to `dontask`, so a
  mutating call in a child runs only if one of your own rules already covered
  it. A def's `tools:` now intersects with what the spawning session could do
  rather than replacing it. A child's tool-call and cost budgets are its own,
  so a wide fan-out cannot end the parent's turn.
- **Your hooks run inside sub-agents** (plan 0058). Every hook envelope carries
  `actor`: `"main"` or `"child:<id>"` — including `pre_compact` and
  `post_compact`, so a hook can tell whose context is folding. A policy that
  stopped applying the moment work was delegated was not one.
- **Identical sub-agents fanned out together stagger** (plan 0058): the first
  goes, the rest wait for its first response byte, so their shared prefix is
  cached once instead of N times.
- **On macOS the sandbox now denies writes to `.git/config`** as well as
  `.git/hooks` (plan 0058). An isolated child can still commit in its worktree;
  it cannot install a hook or set `core.pager` to run something later. On Linux
  neither is denied — Landlock cannot carve a path back out of the
  working-directory grant, and `permissions-and-sandbox.md` says so.
- **`spawn` refuses past `agents × 4` queued children**, naming both numbers,
  instead of queueing silently (plan 0058). A workflow's `each` phase over
  `args` now reports its real width before you approve it. Cancelling a
  workflow drains: agents in flight settle, agents never admitted are recorded
  `skipped`, and the summary accounts for all of them.

- **The model sees its step budget once it is nearly spent** (plan 0059). The
  ephemeral turn-context block carries `turn_budget="80/100"` and `elapsed_s`,
  and past 80% of `max_turns` a fixed sentence saying not to finish early
  because the budget is low. Nothing appears below that threshold, and none of
  it reaches the durable, cached history.
- **Repeated identical failures, unproductive A-B-A-B cycles and four edits to
  one file are each named once per turn** as a system reminder (plan 0059).
  These are nudges, not stops — the doom-loop detector remains the hard stop,
  and fires only on identical results.
- **Three denied tool calls in a row, or twenty in one turn, end the turn**
  with a `denial_spiral` outcome naming both counts (plan 0059). A denial has
  never drawn down the per-tool failure budget and still does not.
- The JSON stream gains `budget_notice` frames and `denial_spiral`/`budget`
  turn outcomes (plan 0059). All additive; the schema version is unchanged.

- **The `/goal` evaluator cannot approve work the validations refute** (plan
  0056). It works to a rubric — results not intent, a claim without a command
  result is not evidence — and reads the harness's own record of what each
  step's `validate_cmd` did. A `met` verdict any evidence line contradicts is
  downgraded to *not yet*, naming the command. `goal_verdict` carries those
  lines, and bare `/goal` shows them.
- **A step marked `needs_more_steps` counts as open work** (plan 0056). The
  todo gate treats `[?]` as unfinished — the model said the step needs
  splitting, and splitting it is the next action. A `failed` step does not:
  it was attempted and reported, and sending the model back at a step it just
  said did not work is how a bounded gate becomes a loop.

- **`bash` and `grep` results spill at 6,000 tokens** (plan 0057), rather
  than the session-wide `[context] evict_tokens`: their output is a haystack
  you wanted one needle out of, unlike a `read`, which is a file you asked
  for in full. `[context] evict_tokens_per_tool = { bash = 12000 }` merges
  over those defaults. A spilled result now keeps its first 1,536 *and* last
  512 characters with the gap marked, so a build log's verdict and a stack
  trace's cause survive alongside the command that produced them — and the
  pointer names the line offset to `read` from.
- **Compaction summaries copy your constraints and decisions verbatim**
  (plan 0057). The summarizer works from two explicit lists: things to
  reproduce word for word — your corrections and constraints (anything
  spelled MUST, never, always, only), the plan's decisions, validation
  results still open, every path created or modified — and things to
  compress. The digest gains a `CONSTRAINTS` section, the newest eight tool
  results are read at 2,048 bytes instead of 600, and each fold records the
  log span it covers so `recall` can fetch it back.

- **The goal loop says where it stands, and counts what it costs**
  (plan 0051). Both the agent's guidance and the evaluator's prompt now carry
  `turn 3 · 12m elapsed · 41.2k in / 3.1k out`, so a turn or time bound
  written into the condition is judged against real numbers instead of a
  guess. The condition itself moved out of the harness's system-reminder
  voice into its own `<goal-condition>` tag, quoted as data like every other
  untrusted input. The evaluator is no longer free: its tokens fold into the
  goal's running spend, into the turn's final `turn_done`, and onto the
  `goal_verdict` frame (new additive `usage` field) — and the same fix makes
  the compaction summarizer report its own spend, inline and speculative
  alike. Bare `/goal` now shows condition, elapsed, turns, spend and the
  evaluator's last reason; with nothing active it shows the goal that
  resolved last; `/goal clear` echoes what it ended.

- CI's harness job now runs `hotl-provider-openai-responses` and
  `hotl-retrieval`, neither of which had ever been in the PR gate.

## [0.25.0] - 2026-09-03

### Added

- **`/goal` starts working the moment you set it** (plan 0048). Typing
  `/goal <condition>` while idle submits the condition as the prompt — the
  Claude Code and Codex reading of "the condition itself as the directive" —
  so a goal no longer waits for a second message. Set mid-turn, it only arms
  the check for when that turn ends, as before. Headless, `hotl --goal
  "<condition>"` now runs with no `-p`: the condition is the prompt, and an
  explicit `-p` still wins.

### Changed

- **The console grid has rules of its own** (plan 0049, the UX redesign's
  layout phase). The activity strip is three zones with a drop order: the
  phase and its elapsed on the left, the todo count in the middle, and a
  chip cluster on the right (session name, mode, context share, `⚑ N`).
  When width runs out, the todo label folds to its count, then usage
  detail, the name and the model drop — the mode, context and flag chips
  never do, and nothing is ever painted over the text, so the flag count
  no longer vanishes under a long usage line and `esc to interrupt` has
  left the strip for the hint row. The strip now reports the *state*, not
  the last event: while any card runs it lists them (`spawn ×2 · 41s`) on
  the oldest one's clock, says `thinking` once none does, and says
  `writing` only when text actually streams — before this a sibling
  finishing read `writing · ~0 tok` over cards still running. In the
  default `comfortable` density a blank row falls only where the speaker
  changes (you, the model, the harness) and before every prompt, so a run
  of tool cards is one block; `spacious` keeps a blank between every item.
  The input box sits on the gutter with its text under your `❯` prompts, a
  gap row separates it from the strip on terminals of 30 rows or more, and
  the draft grows to a third of the screen. A session shorter than the
  screen anchors to the strip, so the first prompt appears where the
  newest row always will and the first overflow moves nothing. Prose wraps
  at a new `[settings] measure` (110 columns; `0` = full width) while
  cards, code and reports keep the width. Modals size themselves to their
  content (60–90% of the frame) with a cell of padding, clip commands and
  diff lines with `…` instead of wrapping them, and scroll with
  `PgUp`/`PgDn`; the `ask_user` modal lists its options with a `›` cursor
  that `↑`/`↓` move and `Enter` picks. `?` is a labeled table that fits
  80×24 with vim off and scrolls when it must. Denied cards wear a
  one-cell `⊘` instead of the two-cell `⛔` that pushed them out of line;
  the idle hint no longer says `esc interrupt`, and every hint cuts at a
  separator on a narrow terminal; a queued steer reads `· queued`; one
  child call is `1 call`.
- **A turn can be told "keep going" eight times per prompt, not three**
  (plan 0048). The shared budget the `stop` hook's `block` and hotl's own
  todo-list nudge draw from (`TURN_EXTENSION_MAX`) now matches Claude
  Code's Stop-hook block cap. The todo nudge keeps its own bound of two
  within it; `max_turns`, the doom-loop detector and the tool failure
  budget are unchanged.
- **`Ctrl-E` no longer opens `$EDITOR` — `Ctrl-G` does** (plan 0047 P0, the
  UX redesign's table-stakes phase). `Ctrl-E` takes its readline meaning,
  end of line, so the whole readline vocabulary reads the same in hotl as
  in your shell; `Ctrl-G` is the editor chord Codex, opencode and Droid
  converged on. `:e` in vim Normal is unchanged. Muscle memory on the old
  chord now parks the cursor at the end of the line instead of suspending
  the console.
- **The composer is a real line editor in the default (non-vim) mode**
  (plan 0047 P0). `←`/`→` move, across line breaks; `Delete` deletes
  forward — a whole `[Image #N]` token when one starts at the cursor, as
  `Backspace` already did behind one; `Alt-←`/`Alt-→` move by word; and
  `Ctrl-A`/`Ctrl-E`/`Ctrl-K`/`Ctrl-U`/`Ctrl-W` do what they do in readline,
  in both vim modes. No kill ring: killed text is gone. Before this the
  arrows and `Delete` were dead keys and every `Ctrl` chord but two was
  silently swallowed.
- **`Ctrl-V` pastes the system clipboard, images included** (plan 0047 P0;
  tracker #39). A copied screenshot — a bitmap, not a file — is written to
  a temp PNG and lands as `[Image #N]` through the path a dropped file
  already takes; copied text inserts as an ordinary paste; an empty
  clipboard leaves a one-line notice. New dependency `arboard` (pure Rust
  on every platform: X11 + Wayland, AppKit, Win32). Terminals that own
  `Ctrl-V` themselves keep delivering text as a bracketed paste but never
  an image.
- **`/effort` is offered by the `/` completion popup** (plan 0047 P0). It
  was dispatched but never listed, so the docs were the only way to find it.
- **The `?` help overlay is generated from the command table and the
  keymap** (plan 0047 P0), so it can no longer drift: it lists every
  builtin (`/goal` and `/workflows` were missing), shows vim keys only when
  `vim_mode` is on, and names `ctrl-g` for the editor.
- **The `ask_user` question modal no longer dresses as a permission ask**
  (plan 0047 P0): its own title, `a question for you`, and the accent
  border instead of the blocked color. Blocked + `waiting on you` are now
  exclusive to prompts where your key grants authority; the strip's phase
  text is untouched — the turn is halted either way.

## [0.24.0] - 2026-08-20

### Fixed

- **OpenAI prompt caching on GPT-5.6: explicit cache breakpoints, so the
  prefix is read instead of rewritten every sample** (plan 0046; the routing
  half was 0045). GPT-5.6 and later cache only at *breakpoints*, place an
  implicit one at the latest user or tool message, no longer fall back to the
  longest matching unmarked prefix, and bill cache writes at 1.25× the input
  rate. hotl ends every request with a per-sample user message (the
  `<turn-context>` block, preceded by the todo reminder), so on GPT-5.6 the
  implicit breakpoint always landed on bytes the cache had never seen: zero
  reads, and the whole history written again at 1.25× on every sample — which
  is what a `163k in · 33k cached · 17% hit` strip at 18% context was
  showing, *with* `prompt_cache_key` already on the wire. Both OpenAI dialects
  now send `prompt_cache_options: {mode: "explicit"}` and put a
  `prompt_cache_breakpoint` on the last content block of every durable user
  message and on the last result of every tool-result batch; the todo
  reminder and turn context ride after the last marker, where they can change
  freely. Placement is append-stable by construction (a marker is only ever
  decided while walking the durable items), so sample N+1 re-presents sample
  N's newest marker over the same bytes and the read hits before any write.
  The markers are gated on the model name (`gpt-5.6`, `gpt-5.7-mini`,
  `gpt-6`, a `gpt-5.6-sol` gateway alias — anything at or past 5.6; unknown
  names fail closed to today's implicit wire), overridable either way with
  `[provider] cache_breakpoints = true | false`, and guarded by a one-shot
  probe: an endpoint that 400s on a prompt-cache field gets the same sample
  re-sent without the fields behind a `Retrying` notice, and the provider
  stays on the implicit wire for the rest of the session. Pre-5.6 models,
  OpenAI-compatible servers and `CachePolicy::Off` bodies are byte-identical
  to before. Separately, OpenAI's `cache_write_tokens` now lands in
  `cache_creation_input_tokens` and is carved out of the input figure like
  `cached_tokens` already was, so `hit` stops hiding the writes inside `in`;
  `/cost` prints a `written` total and the strip shows one once it is
  nonzero — the figure OpenAI's own troubleshooting says to watch, and just as
  telling on a write-heavy Anthropic session.

## [0.23.0] - 2026-08-20

### Fixed

- **OpenAI prompt caching: the session id now rides as `prompt_cache_key`,
  and cached tokens are no longer double-counted** (plan 0045). OpenAI
  routes a request to a cache shard by `prompt_cache_key` first and the
  prompt-prefix hash second, and throttles any one key past ~15 requests a
  minute; hotl sent no key, so every sample of every session — org-wide
  behind a shared gateway — collapsed onto the same routing key (the shared
  system-prompt head) and mostly missed, which is what a `4% hit` strip on
  `gpt-5.6-sol` was showing. Both OpenAI dialects now send the session's
  ULID as `prompt_cache_key` (stable across resume, distinct per fork,
  shared by speculative samples); one-shot calls — compaction summaries and
  `/goal` evaluation — stay keyless. The chat-completions dialect sends it
  unconditionally: a documented OpenAI parameter that Ollama, llama.cpp and
  vLLM ignore, and a deliberate exception to the byte-conservatism rule
  that guards `reasoning_effort` — if a strict gateway rejects the key, that
  is the field to look at. Separately, OpenAI's `input_tokens` /
  `prompt_tokens` *include* `cached_tokens`, while hotl's `TokenUsage`
  counts cached tokens outside the input figure (Anthropic semantics); both
  dialects now subtract at parse time, so on OpenAI the strip's `in` and
  `ctx %` stop over-reporting, `hit` stops under-reporting, and a catalogued
  model would no longer be over-billed.

## [0.22.0] - 2026-08-20

### Added

- **`workflow`: many sub-agents from one declarative plan** (plan 0044,
  hotl's answer to `/workflows`). The model hands hotl a plan — phases of
  agents run all at once, a pipeline per selected item with no barrier
  between stages, majority `votes`, and `until_quiet` rounds that stop when
  nothing new turns up — or names a saved recipe from
  `~/.config/hotl/workflows/*.toml`, and hotl runs it: bounded by a
  process-wide `[workflows] concurrency` gate (default 8) that a plan can
  lower but never raise, capped at `max_agents` (default 1000, a run error
  rather than a silent truncation), every schema-shaped answer validated
  with two retries, and every agent a `spawn`-shaped child (asks
  default-deny, no `spawn`/`workflow`/MCP in its registry, mutating agents
  serialised on the shared-tree lock unless `isolation = "worktree"`). One
  ask covers the whole run and names its phases and an upper bound on agent
  count; `[[allow]] tool = "workflow"` can whitelist a saved name only. The
  run is one row in the agent band, its agents in the drill-in with token
  counts; `/workflows` lists every run this process started with per-phase
  progress; `/<recipe> [args]` invokes a saved one (after built-ins and
  skills). The result comes back as JSON inside
  `<workflow-result trust="untrusted">`, cut at 64 KiB in context with the
  full JSON at `~/.local/share/hotl/workflows/<run_id>/`. `hotl workflows
  list | show <name> [--mermaid] | check <file>` inspects recipes from the
  shell; `--mermaid` draws a plan as a flowchart. The plan is data, not
  code — no script engine, so `cargo install hotl` stays pure Rust. Agent
  defs gained a `max_turns:` field along the way, and `child_tool` done
  frames now carry the child's `tokens`. New crate `hotl-workflow` holds
  the runner; see the new [workflows guide](site/src/content/docs/workflows.md).

### Changed

- **Sub-agent navigation moved into the agent band, and finished spawns
  leave it** (plan 0043, the 0042 follow-up). The band below the input used
  to list every spawn the session ever produced, settled or not, so it only
  ever grew; it now lists the spawns still *running* (plus the one you are
  viewing or pointing at, pinned until you leave it) and collapses to
  nothing when nothing runs. Navigation happens in the band, not the
  transcript: with the input empty, `↑`/`↓` move a highlight over
  `main` and the running spawns — without vim whenever the band shows, with
  `vim_mode = true` from Normal mode only (Insert keeps the arrows for
  history recall) — and vim Normal `j`/`k` move it too. `Enter` on a spawn
  opens its live stream, `Enter` on `main` returns, and `Esc` backs out one
  rung per press (stream → highlight → interrupt). The 0042 transcript
  cursor is deleted rather than rebound: with no band, vim `j`/`k` go back
  to scrolling the transcript one item. Non-vim users now have a keyboard
  path into the drill-in for the first time.

## [0.21.0] - 2026-08-19

### Changed

- **Sub-agent streams are vim-navigable, and spawn cards stay one line**
  (plan 0042, the 0039 follow-up). The `ctrl-o` selector is gone — it
  required a vim Normal mode that was unreachable mid-turn, so j/k never
  worked. In its place: with `vim_mode = true`, an empty input and Normal
  mode, `j`/`k` walk a highlighted **transcript cursor** over items and
  `Enter` on a spawn card opens that agent's live stream. `Esc` gets a
  predictable ladder — in Insert it only enters Normal (it never interrupts
  the turn); in Normal it backs out one rung per press: stream → main,
  cursor → follow-tail, then interrupt. A running spawn card no longer
  churns a three-row child tail at tick rate: it stays one calm line
  (`spawn  survey · N calls · Ns · <latest child call>`), with the full call
  list in the drill-in. The band below the input is now a passive per-agent
  summary. With `vim_mode = false` (the default) nothing changes; note the
  drill-in currently has no non-vim path (mouse selection remains out of
  scope).

- **`nix flake check` no longer pays thin-LTO link costs for test binaries**
  (plan 0041). The check derivation's checkPhase compiles ~70 test binaries
  in release mode, so the thin-LTO + `codegen-units = 1` profile v0.16.0
  added for shipped-binary parity turned it into ~28 minutes of link work
  for tests that run in seconds — the critical path of every master push.
  The check build now disables both knobs via `CARGO_PROFILE_RELEASE_*` env
  overrides (measured locally: the full darwin check drops from ~28 to ~3.5
  minutes); the shipped package, `cargo install`, and nixpkgs-style builds
  keep the LTO'd release profile untouched.

## [0.20.0] - 2026-08-19

### Added

- **The status strip is true at session open** (plan 0040). The
  `session/new`/`session/load` reply now carries the at-open reality the
  strip used to leave blank — all additive keys, old clients and servers
  interoperate: `contextTokens` (the engine's estimate of what already
  fills the window, so the strip shows a bare `N% ctx` before the first
  turn — a resumed session's inherited fullness, a fresh session's seed),
  `todos` (a resumed checklist shows immediately instead of after the next
  `todo_write`), `effort` (a resumed session's inherited level — `/effort`
  and `/status` no longer report the config default while the engine runs
  the inherited one), and `previousModel` (when the configured model
  changed since the session last ran, a one-line transcript notice names
  the switch instead of swapping silently; fires on fork too). Usage
  counters and cost still start at zero on resume — only the context gauge
  carries over, and the at-open estimate yields to the real usage line the
  moment a turn completes.
- **Sub-agent streams** (plan 0039). A `spawn` card is no longer opaque for
  its whole lifetime: the child's tool lifecycle is forwarded on the parent
  stream as a new `child_tool` frame (additive — the stream schema version
  stands) carrying the spawn call's `parent_id`, a `phase` of `start`/`done`,
  and `ok` on done frames only. A running spawn card shows its last three
  child calls indented inline with an `… +N earlier` line; a settled one
  collapses to a `· N calls` detail. `hotl attach` shows `↳` child lines and
  headless `-p` prints them to stderr; `-p --json` emits the frames.
- **Agent selector** (plan 0039). Once a turn spawns sub-agents, a selector
  strip appears below the input: `● main` plus one row per spawn. `Ctrl-O`
  focuses it; `↑`/`↓` (and `j`/`k` under vim Normal) move the selection and
  the whole region above the strip swaps to the chosen agent's own stream —
  live, following its tail, scrollable. `Esc` returns to main; any other key
  drops straight back into the prompt while the chosen stream stays up.

### Changed

- **Consecutive same-target tool calls merge into one accumulating card**
  (plan 0039). Four paged reads of one file render
  `✓ read ~/…/app.rs · ×4 · 3s`, not four rows. Merging never crosses another
  transcript item, never touches `spawn` cards, and never absorbs into a
  failed card — a retry after a failure starts fresh, and a merged card with
  any failed call settles ✗.
- **Long paths middle-elide on tool cards** (plan 0039). The display summary
  abbreviates your home directory to `~` and elides the middle of a long path
  so the filename and page details survive the card width. Display only:
  permission asks and ⚑ notices always carry the full verbatim path.

## [0.19.0] - 2026-08-19

### Changed

- **The running tool card's marker is now the braille wanderer from
  `hotl watch`** (plan 0038), replacing the ◐◓◑◒ circle spinner — the same
  organism everywhere hotl shows work. Same 8 fps cadence as before (one
  full wander every 2 s, matching watch's 125 ms twin); finished cards keep
  their `✓`/`✗`/`⛔` markers unchanged.
- **Tool calls have identity end-to-end** (plan 0037). Every tool lifecycle
  event now carries the provider's `tool_use` id (additive `"id"` field on
  `tool_start`/`tool_done`/`tool_denied`/`tool_auto_allowed`/`tool_flagged`
  frames; the stream schema version stands), and the TUI settles each card by
  id instead of "newest card with that name" — a turn that fans out N
  concurrent same-name calls (paged reads of one big file, typically) no
  longer strands N−1 cards on a frozen `0s` spinner while the last card shows
  whichever status arrived last. Every running card's elapsed time now ticks,
  not just the newest, and paged reads name their page:
  `read notes.vtt · from line 2001 · 500 lines` on cards, asks, and ⚑ notices
  alike (whole-file reads render exactly as before).
- **⚑ flag notices coalesce per turn** (plan 0036 follow-up, plan 0037). The
  bypass floor still evaluates and resolves every call, but the notification
  now fires once per distinct (tool, class, path, allowed/refused) decision
  per prompt on every surface at once — six page reads of one outside file
  are one notice and `flags: 1`, not six. A new prompt re-flags, so nothing
  stays buried across a long session, and a flagged refusal is never
  swallowed by an earlier allow-notice on the same file. The strip's `flags`
  chip therefore counts decisions a human can actually review.
- **Identical read-only calls in one batch run once** (plan 0037). Calls in a
  single assistant batch with byte-identical name and input, whose tool is
  read-only and parallel-safe, execute once and every duplicate still gets
  its own result on the wire. Mutating duplicates are exempt and still run
  each time — they are the model's to own and the doom guard's to catch.

## [0.18.0] - 2026-08-18

### Changed

- **Bypass mode no longer prompts — it notifies** (plan 0036). A session in
  the default `bypass` mode never blocks on a permission prompt: the
  protected floor now maps each class to a non-blocking disposition,
  surfaced as a loud ⚑ notice in the transcript (`tool_flagged` on the wire)
  and a running `⚑ flags: N` chip on the console strip. Execute-later writes
  inside the session root, reads outside it, and private-network fetches
  **run with a notice**; writes outside the session root, writes into hotl's
  own config/data dirs, and cloud-metadata fetches are **refused with a
  notice** — mirroring the kernel sandbox, which already lets `bash` read
  outside the tree and denies it writes there. Two prompts survive in
  bypass, deliberately: the MCP/retrieval first-use trust screen (a
  recorded, once-per-binary grant) and `bash` without a live kernel sandbox.
  The `ask`/`dontask`/plan floors are unchanged, deny rules and the plan
  overlay still outrank bypass, and `security-enforced` builds cannot reach
  any of it. `hotl doctor`'s permissions line states the new meaning.
- **Absolute paths under the session root are in-session — in every mode**
  (plan 0036). `read`/`write`/`edit`/`glob`/`grep` re-anchor an absolute
  path that resolves under their root (through the macOS `/var` alias or an
  in-root symlink) instead of classifying it "outside the working
  directory", so a model spelling an in-session path absolutely — as models
  constantly do, e.g. under `~/sources/worktrees/…` in a `~/sources` session
  — no longer trips a false outside-the-workspace prompt. Absolute in-root
  `glob`/`grep` search roots now work too. Containment is still decided on
  the file descriptor, never the name.

## [0.17.0] - 2026-08-17

### Changed

- **Shadow snapshots are now invisible** (plan 0035). The first mutating tool
  call no longer stalls behind an awaited pre-batch snapshot — on a large
  repo that wait was ~60s of `git add` hashing the whole workspace before the
  tool could run, and every later mutating batch paid a further stat walk
  (0033 already detached the post-batch half; this finishes the thought).
  Snapshots now happen at quiet windows — session open, and each mutating
  batch's end — on a dedicated background worker the turn path never waits
  for: dispatch and turn-done latency are independent of repo size and
  snapshotter speed, enforced by a regression test. A capture that raced a
  mutation is committed under a `tainted` label and never restored; the only
  remaining user-visible cost is a bounded (2s) drain at session close.
  `hotl undo` semantics sharpen with the model: it restores the newest
  *clean* snapshot — the agent's last completed checkpoint (batch-end state,
  not dispatch-time state) — refuses when the agent made no mutations that
  session instead of trampling edits that were only yours, explains a locked
  shadow index instead of guessing, and no longer misclaims that files
  created after the snapshot are kept (they are removed; the pre-undo
  checkpoint retains them). `hotl doctor` reports the undo point (ready /
  warming / no agent mutations), and the console strip and `/status` carry an
  `undo` chip off a new additive `undoStatus` field on the prompt reply.
  Snapshots of git workspaces also borrow the workspace's own object store
  (`alternates`) and enable git's many-files levers, shrinking capture cost;
  `core.fsmonitor` was measured to deadlock under the shadow's bare-gitdir
  shape and stays off. Known limit: a backgrounded process (`cmd &`) that
  writes after its batch ends mutates outside any snapshot window — same as
  the old design.

### Added

- **`/goal` — run until a condition is met** (plan 0034). Set a completion
  condition (`/goal all tests pass and the change is committed`); after each
  turn a small fast evaluator (the compaction summarizer's model) judges it
  against the conversation. *Not yet met* and the agent takes another turn
  with the verdict's reason as guidance — with no intermediate turn-done, so
  the console stays in "working", attached surfaces stay parked, and the one
  final result reports the cumulative token spend. *Met* or *impossible* and
  the goal clears with a transcript notice. Bare `/goal` reports condition,
  elapsed time and turns; `/goal clear` ends it early; the status strip shows
  `◎ /goal active · Nm` meanwhile. The evaluator fails open (a timeout,
  garbage verdict, or Esc during the check ends the turn with the goal
  intact — it can never trap the run in a loop), a queued prompt outranks
  the continuation, and there is deliberately no built-in turn cap — bound
  it in the condition text. An active goal survives `hotl resume` (turn
  count and timer restart); an achieved/cleared one never comes back, and a
  fork never inherits one. Headless parity: `hotl -p --goal "<condition>"
  "<prompt>"` runs the whole loop in one invocation, and ACP clients get
  `session/set_goal` plus `goal_changed`/`goal_verdict` updates (additive —
  no schema-version bump).

## [0.16.0] - 2026-08-14

### Changed

- **The client hot path sheds its avoidable milliseconds** (plan 0033). What
  you can see: streamed text now coalesces delta bursts and draws on the
  existing 30 Hz tick instead of paying a full render per delta, the
  transcript re-renders only what actually grew (a long answer no longer
  re-wraps from the top on every delta), and `hotl` paints a live composer
  immediately — session hydration (skills walk, sandbox probe, git
  orientation) runs behind it, a prompt typed during that window fires the
  moment the session opens, and the orientation's git reads run in parallel
  under a 1.5s cap so a wedged repo can't hold the first turn hostage. What
  you can't see: the projection stops deep-copying the whole history on
  every commit (`Arc<Item>` elements), the Anthropic request body serializes
  in one borrowed pass (byte-identical to the old builder, differentially
  tested; the OpenAI dialects keep the old build for now — tracker #81), the
  pre-anchor context estimate is an O(1) running sum instead of a per-char
  walk of the session, the post-batch shadow-git snapshot no longer gates
  the next sample, and prompt admission rides the same ticket pipeline as
  every other commit, so the first request's build-and-send overlaps the
  prompt's fsync instead of waiting behind it (gated off automatically for
  `Sync` sessions, `UserPromptSubmit` hooks, and held steers). Per-phase
  client spans now ride the `-p --json` stream (`ledger_report.phase_deltas`,
  additive) and `scripts/ab-wallclock.sh` prints them — the before/after A/B
  against a pre-0033 binary still needs a live-credential run and is owed.
- **`cargo install hotl` now builds the same optimized binary the published
  artifacts ship** (thin LTO, one codegen unit in the release profile) —
  parity hygiene worth single-digit-% CPU, paid in build time, not runtime.

Open question for a future release (0033 Task 8): the workspace-orientation
item sits at durable index 1 with live `git status` inside it, which keeps a
brand-new session in the same repo from reusing another session's cache
prefix (resumes are unaffected). Moving it after the stable start items
would warm new sessions at the price of prefix-layout churn now.

## [0.15.0] - 2026-08-14

### Changed

- **Sessions spend fewer round trips — and cheaper ones — on the same task.**
  Four levers, all aimed at wall-clock parity with other harnesses on the
  identical model: a bash command that fails on a sandbox write-denial now
  says so in the result, naming `[sandbox].writable` as the remedy instead of
  leaving the model to diagnose a bare "Operation not permitted" (the docs
  carry a ready-to-paste block for the common toolchain caches — `~/.cargo`,
  `~/.npm`, `~/.cache` and friends; the confined-by-default posture is
  unchanged); a successful `edit` echoes the edited region in its new form
  (numbered, ±3 lines, capped at 40) so verifying the splice no longer costs
  a follow-up read; session start hands the model the orientation it used to
  re-derive every time — branch and dirt count, recent commit subjects,
  porcelain status, top-level listing — as one untrusted-enveloped item
  (repo-derived text can never authorize tool use); and the default system
  prompt now says when to fan investigation out to parallel explore agents
  instead of leaving delegation to chance.
- **Compaction digests default to Haiku on catalogued Anthropic models.** An
  unset `fast_model` ran every digest — including the speculative one racing
  the live turn for rate limit — on the main model at full price. Haiku
  sessions keep Haiku, uncatalogued and OpenAI-compat sessions keep the
  session model, and `HOTL_FAST_MODEL` / `[provider] fast_model` still win.

### Added

- **Agent Plugins 1.0.0 is hotl's primary extension package.** One directory
  bundling skills and MCP servers under a `plugin.json` manifest, in the
  portable [Agent Plugins](https://github.com/agentplugins/agent-plugins-spec)
  format — a plugin written for any conformant client loads in hotl
  unchanged. Register packages in `[plugins.sources]` and manage them with
  the new `hotl plugins list/add/update/remove` (git runs only on explicit
  add/update; `add` validates the fresh checkout and prints its reports, so
  a broken plugin is visible at install time; `remove` always preserves the
  plugin's data directory). Plugin skills slot in right after your own flat
  skills — ahead of marketplaces and the Claude roots — and are always also
  addressable as `<plugin>:<skill>`; plugin MCP servers register as
  `<plugin>:<server>` beside `[[mcp]]` entries and pass through the same
  first-use trust screen. Spec conformance (path containment, per-component
  failure isolation, `${PLUGIN_ROOT}`/`${PLUGIN_DATA}` expansion, the
  reserved-variable overlay order) is pinned by a per-checklist-row
  conformance suite. Remote (`streamable-http`/`sse`) server entries are
  validated but skipped as unsupported transports for now. The existing
  marketplace and Claude-cache lanes keep working unchanged.
- **`[[mcp]]` servers can now set `env` and `cwd`.** An ordered
  `env = [["KEY", "value"], …]` overlays the inherited environment (later
  duplicates win) and `cwd` sets the working directory — what plugin servers
  use under the hood, and available to hand-configured servers too.
- **`/cost` shows recent cache health.** A new `cache N% last turn` segment
  reports the last completed turn's read split, overwritten each turn — so a
  cache-bust regression (the single biggest latency multiplier available)
  shows up immediately instead of hiding inside the ever-growing cumulative
  counter.
- **`scripts/ab-wallclock.sh` profiles a `hotl -p` run** — wall-clock, sample
  count, per-sample cache hit %, inter-sample gaps, tool calls, and
  denial-smelling results from the session log. Run it once per binary to A/B
  a change's round-trip behavior.

### Security

- **A server's `env` and `cwd` are part of its trust fingerprint.** A
  configured environment reaches code execution through variables like
  `NODE_OPTIONS`, `PYTHONPATH`, or `LD_PRELOAD`, so both fields now fold
  into the MCP trust key (new `fp3:` scheme) and render on the approval
  screen — shown equals recorded. **Existing MCP trust grants re-screen
  once** after this update; approve the same server again and the new grant
  sticks.

### Fixed

- The shell installer no longer puts `hotl-sbx` — the Windows write-floor
  launcher, a stub that refuses to run everywhere else — on Unix machines. A
  per-target `binaries` override confines it to the Windows archive; the four
  Unix archives now ship `hotl` alone. `hotl update` was never affected (it
  extracts only the `hotl` executable), so an install from an earlier release
  just deletes `~/.cargo/bin/hotl-sbx`.
- Unit tests that spawn scripted child sessions no longer write their logs
  into the real `~/.local/share/hotl/sessions` store — the child store is
  injected, and the test builder points it at a tempdir.

## [0.14.0] - 2026-08-14

### Changed

- **hotl now works noticeably deeper per prompt on the same model.** On the
  default Anthropic models, sessions had been producing fewer agent steps and
  shallower work than other harnesses give the identical model — not one bug
  but a stack of defaults that all leaned toward stopping early. All of them
  flipped: the default system prompt now carries the persistence, tool-choice,
  todo-discipline, and verify-before-done guidance Opus-class models are tuned
  to expect (your `~/.config/hotl/system-prompt.md` override still wins);
  unconfigured sessions on catalogued Anthropic models with effort support run
  at `xhigh` (Haiku and every uncatalogued model keep the provider default,
  and `/effort`/`HOTL_EFFORT`/`[provider] effort` still override); the
  context-fullness percentage is no longer broadcast to the model each sample
  (it measurably induces premature wrap-up — opt back in with `[context]
  show_used_pct = true`); the per-sample output cap doubled to 64K tokens and
  became configurable (`[provider] max_tokens` / `HOTL_MAX_TOKENS`, always
  clamped to the model's catalogued maximum); and tool descriptions steer
  proactively instead of listing caps, with the shrinking `<retry
  attempts_left>` countdown on failed tools replaced by a single last-chance
  hint. Each lever reverts independently without rebuilding if you preferred
  the old behavior.
- **A bare `/effort` (and `/status`) now reports the effort the session
  actually runs at** — `effort xhigh (default)` when the seeded default
  governs — instead of the word "default". Note the asymmetry `/effort
  default` creates on purpose: clearing selects the *provider's* own default
  (`high` on Anthropic today), not the `xhigh` session default, and the clear
  notice says so.

### Added

- **The model now knows where it is running.** A byte-stable `<env platform
  arch is_git_repo model date/>` block rides first in session-start context,
  so the model can pick GNU vs BSD flag spellings, reach for git only where
  there is a repo, and date-stamp what it writes — facts that previously
  never crossed the wire.
- **A third provider dialect: `HOTL_MODEL=openai-responses/<model>` speaks
  the OpenAI Responses API.** OpenAI's current reasoning models refuse
  `reasoning_effort` next to function tools on `/v1/chat/completions`
  ("Please use /v1/responses instead"), and every hotl request carries tools
  — so setting an effort level on them was a dead end. The new dialect fixes
  exactly that: it reuses the `openai/` door's configuration untouched
  (`OPENAI_API_KEY`, `HOTL_OPENAI_BASE_URL`, the api-key helper, both auth
  modes), maps all five effort rungs 1:1, streams reasoning summaries live
  as thinking, and replays encrypted reasoning items verbatim so multi-step
  tool loops hold with `store: false` on every request — nothing is kept
  server-side. `openai/` stays the documented door for every
  OpenAI-compatible endpoint, and the one 400 the new dialect fixes now
  carries a hint naming `HOTL_MODEL=openai-responses/<model>`. Reasoning
  items never leak across dialects: the other two providers drop them from
  replayed history, exactly as signed Claude thinking never crosses the
  other way.

### Fixed

- **A response truncated at the output-token cap is no longer silently
  accepted as the answer.** The turn loop now recognizes the truncation stop
  reason: complete tool calls still run, otherwise the model is told it was
  cut off and asked to continue (bounded at three continues per prompt,
  surviving compaction folds); a truncated trailing thinking block is pruned
  before it can poison the next request. Server-side `pause_turn` similarly
  resumes instead of ending the turn.
- **Legitimate polling no longer trips the doom-loop brake.** Three identical
  tool calls only end the turn when their results were also identical — "same
  call, same answer, no progress". Repeating a status check whose output
  keeps changing is now recognized as progress; a genuine no-progress loop
  still stops, at most one round-trip later than before.
- **The OpenAI-compatible dialect no longer sends `reasoning_effort` values
  real servers reject** — `xhigh`/`max` clamp to `high` on that wire (the
  `openai-responses/` dialect keeps all five rungs).
- **Quitting no longer needs a second ctrl-c after a bash descendant escaped
  its process group.** A command whose descendant left the process group (a
  daemonizing tool: gpg-agent, an ssh ControlMaster, sccache, …) keeps the
  merged output pipe open, which parks the drain thread on tokio's blocking
  pool for the rest of the session — an accepted trade. What was not accepted:
  dropping the runtime on exit waited on that thread *forever*, after the
  terminal guard had already restored the screen, so ctrl-c looked like it quit
  while the process sat there until a second ctrl-c (a real SIGINT once raw
  mode is off) killed it. Runtime shutdown is now bounded (500ms grace);
  pending async tasks still drop and flush, only stuck blocking threads are
  abandoned on the way out.

## [0.13.0] - 2026-08-11

### Changed

- **The activity snake rears up to glance between travels.** While a turn is
  working, the status-strip snake now swims one way for ten seconds, springs its
  head up in the middle to glance right then left, swims back, and glances in the
  inverse order — a cycle that restarts at the start of each working turn. It is
  driven by a new per-turn work-tick clock that advances across the
  thinking/writing/tool sub-phases and pauses while a prompt is blocked, so every
  frame stays a pure function of the tick count and remains golden-testable.

### Fixed

- **A turn that crashed mid-batch bricked every later resume.** A turn that died
  after committing its `tool_use` calls but before their results left an
  unanswered assistant batch in the log; on resume the provider rejected the
  request with HTTP 400 (`tool_calls must be followed by tool messages`), and
  because the log is append-only it failed on *every* resume forever — the old
  repair only closed the tail, so a prompt typed after resume stranded the
  dangling call mid-history. Resume now synthesizes an error result for every
  unanswered batch, in memory only (the log is never rewritten) and idempotently.
  Two UI companions land with it: an outright turn failure renders as a loud
  `Error` item (blocked color, ✗) instead of a muted notice, and a requested
  `/<skill>` that finishes a turn without ever loading now warns.

- **A dropped image path with spaces in the filename is recognized again.** A
  terminal that delivers a drag-and-drop as a bracketed paste (iTerm2, tmux, and
  others) sends the path with literal spaces, not shell-escaped ones, so a name
  like `Screenshot 2026-08-10 at 11.52.46 AM.png` failed the path-shape gate and
  was inserted as plain text instead of compacting to an `[Image #N]` token.
  Literal interior spaces are kept now; a space immediately before a `/` still
  keeps a multi-path paste (`/a/b.png /a/c.png`) literal.

- **Windows: a worktree-isolated child could not write to its own floor.**
  fsguard's NT-namespace `open_root` built a backslash-only `\??\<path>`, but
  `git rev-parse --show-toplevel` reports forward slashes, so every
  worktree-isolated child write failed with `ERROR_PATH_NOT_FOUND`. Separators
  are normalized before the NT prefix now. Surfaced while repairing the seven
  harness tests that never passed on the Windows leg — backslashes escaping in a
  TOML hook `command`, `core.autocrlf` rewriting seeded worktrees, an exclusive
  log write handle blocking an mtime backdate, and separator-keyed assertions.

- **The release gate now covers the Windows suite.** `wait-for-ci.sh` checks a
  named allowlist of jobs rather than CI's overall conclusion, and
  `harness-windows` — added to `ci.yml` after the gate's `REQUIRED` set was
  written — was missing from it, which is how v0.12.0 published to crates.io with
  the Windows suite red. It is in `REQUIRED` now. Maintainer tooling; no change to
  installed behavior.

## [0.12.0] - 2026-08-11

### Added

- **A reasoning-effort ladder.** One provider-neutral set of rungs —
  `low | medium | high | xhigh | max` — that each dialect spells its own way:
  `output_config.effort` on Anthropic, a flattened top-level `reasoning_effort`
  on OpenAI-compatible endpoints. Four surfaces set it: `[provider] effort` in
  `config.toml`, `HOTL_EFFORT`, an `effort:` line in an agent def (which
  replaces the parent's for that child only), and `/effort` in the console —
  the last recorded durably, so `hotl resume` keeps it. ACP clients get
  `session/set_effort`, whose change is broadcast to every attached surface.

  A model that accepts fewer rungs **clamps to its nearest one** rather than
  erroring, and ties clamp downward, toward the cheaper rung. A model with no
  effort support (`claude-haiku-4-5` today) gets no field; a model hotl does not
  recognize is never refused one, because hotl allowlists no model names.
  Compaction is pinned off regardless of the session's setting.

  **An unconfigured session's requests are unchanged, byte for byte.** Unset
  emits no field on either wire — no warm prompt cache invalidated on upgrade,
  and no unknown key sent to a local server that might reject it. `thinking`
  stays its own independent off-switch.

- **A provider-neutral endpoint env var: `HOTL_PROVIDER_BASE_URL`.** It mirrors
  `[provider].base_url` and pairs with `HOTL_PROVIDER_AUTH`, so the endpoint knob
  no longer names a vendor. The vendor-specific `HOTL_OPENAI_BASE_URL` and
  `HOTL_ANTHROPIC_BASE_URL` remain honored as legacy aliases and still win when
  both are set (specific beats general), so nothing breaks. The
  `auth = "subscription"` "requires base_url" error now names the neutral var.

- **Native Windows builds, runs, and is tested.** The whole workspace compiles
  for `x86_64-pc-windows-msvc` with no warnings, and the harness suite runs on
  `windows-latest` in CI. The file tools, the session server, the process
  reaper, `hotl bg`/`attach`, and the `bash` tool all work.

  It is **not yet a confined platform**. The `win-writerestricted` write floor
  is implemented but has never been executed on real hardware, so `probe()`
  reports `Unavailable` and native Windows behaves exactly like an old Linux
  kernel: every exec individually human-gated, `UNSANDBOXED` in the ask, no
  allow-rule persistence. WSL2 remains the confined Windows path. See
  `docs/SECURITY.md` for what the designed floor can and cannot express — in
  particular that the read carve is **absent** there rather than degraded.

- **`hotl-platform` grew capability traits**, one adapter per platform:
  `PrivateFs`, `KnownPaths`, `Entropy`, `DirHandle`, `ProcessControl`, `Ipc`,
  `ConsoleControl`. Statically dispatched, so nothing lands on the hot path of
  a bash call. `ARCHITECTURE.md`'s claim that core crates sit behind platform
  traits is now most of the way to true.

- **The `bash` tool resolves a POSIX shell, or leaves the registry.** With no
  `sh` on Windows the tool is absent and the model is told why, rather than
  being handed `cmd` or PowerShell — neither of whose grammar the deny rules
  can analyze, so a rule would silently stop applying.

### Fixed

- **A deny path rule now matches case-insensitively where the filesystem
  does.** `deny_path_matches` compared bytes while its two siblings folded
  case, so on default APFS — and on NTFS — `~/.SSH/id_rsa` walked straight
  past a `~/.ssh` deny rule. The rule matched a spelling rather than a file.
  Linux behavior is unchanged.

- **The file tools refuse filenames Windows resolves differently from every
  path matcher**: alternate data streams (`AGENTS.md:evil`), trailing dots and
  spaces, and reserved device names (`CON`, `NUL`, `COM1`). Refused on
  **create** as well as read, and on every platform — these are names where
  Win32 opens one file while a deny rule, a glob and the write classifier all
  match another.

## [0.11.0] - 2026-08-10

### Added

- **`/context` — what is filling the context window, by source.** A twelve-row
  breakdown in the scrollback: system prompt, tool schemas, skills roster,
  agents roster, project instructions, memory, todos, folded history, messages,
  tool results, harness injections, images — plus free space, a per-row meter
  and two totals. Every item in the window lands in exactly one row and the
  rows sum to the estimate, so nothing is dropped or double-counted (images in
  particular are lifted out of their owning message rather than billed twice).

  Both totals are shown and labelled: `reported` is the provider's exact figure
  for the last turn, `estimated` is hotl's own per-item accounting — the same
  ruler that decides when to fold history, which deliberately overcounts. Free
  space is computed from whichever is larger, so the report may understate your
  remaining room and never overstates it. The gap between the two lines is the
  overcount margin, visible for the first time.

  Shape carries the grouping (`▣` stable prefix, `◆` session preamble, `▪`
  conversation, `▫` free space) so the table reads on a monochrome terminal;
  color separates rows *within* a group and is derived from the theme palette,
  never hardcoded. Free space turns to the `blocked` slot below 15%.

  It is a read — no log entry, no projection advance — so unlike `/reload` it
  is safe to run mid-turn. Over `hotl acp` it is `session/context`: a thin
  `{"ok": true}` ack plus a `context_report` `session/update` broadcast, so
  every attached surface gets the report. Additive: the update schema version
  is unchanged.

- **The model config resolved to is now on the summary line**, on both
  surfaces: the TUI's idle strip (`⠐⠒⠒⠒⠒⠒⠒⠒⠒⠒⠒⠂ claude-opus-5 · 120 in · 45
  out · 0% ctx`) and the per-turn footer headless runs print (`[claude-opus-5
  · in 120 out 45 cache-read 0]`). A model comes from `config.toml`,
  `HOTL_MODEL` or a flag, and until now the only way to see which one won was
  `/status`. The `provider/` prefix is trimmed for width; a mid-session
  fallback re-seeds the strip, so the name shown is the model the next turn
  will use.

### Changed

- **The activity strip is a braille snake now, at 30fps, gradient-lit from the
  theme.** The loop motif (`╭─╮╰─╯` and friends, 8 frames/sec) is gone. In its
  place a snake swims a fixed wave across 12 cells — two dots thick at the
  head, thinning to one at the tail, advancing one sub-column per tick, so it
  crosses the strip in about four fifths of a second. At rest it lies flat
  (`⠐⠒⠒⠒⠒⠒⠒⠒⠒⠒⠒⠂`), which is the old resting line in the new alphabet.

  Phase is now carried by **color rather than shape**: one motion everywhere,
  lit by a two-slot gradient the theme owns. Idle rests on `faint → idle`,
  thinking climbs `faint → accent`, writing runs `accent → ink`, a tool warms
  `accent → active`, and anything waiting on you lands on `faint → blocked`.
  Every endpoint is a palette slot, so a preset or a single-slot override in
  `[theme]` recolors the animation along with everything else.

  Phases that are not running a turn show the **resting** shape rather than a
  frozen frame of the animation: idle still schedules no wakeups at all, and a
  permission prompt that kept moving would read as progress when the point is
  that nothing happens until you answer.

  Every frame is integer arithmetic on the tick count — no floats — so the
  animation is bit-identical on every platform and its frames can be pinned in
  tests. The rate lives in one place (`hotl_tui::anim::TICK_HZ`); the ticker
  interval and both elapsed-seconds displays derive from it.

- **The running-tool card's spinner is pinned to 8fps.** It used to be indexed
  by the raw tick, so raising the strip to 30fps would have spun this 4-frame
  glyph seven times a second — a strobe. It keeps the rate it always had.

- **The transcript is wrapped once per change instead of once per frame.**
  Wrapping the session is the most expensive thing the view does, and at 30fps
  it was being redone 30 times a second so an animation could move. Rows are
  now memoized per item, keyed by a hash of what actually reaches the screen,
  and re-wrapped only for items that changed — during a turn that is exactly
  one: the assistant text growing, or the tool card ticking. Scrolling reuses
  the rows it already has; a resize, a density change, `ctrl-t`, or a new theme
  drops the memo wholesale. Measured on a 601-item / 326KB session: 1.99ms →
  0.16ms per frame.

## [0.10.0] - 2026-08-10

### Added

- **`egress = "allowlist"` is usable now: it starts with a list, and a blocked
  host asks instead of 403ing.** Two things kept anyone from turning egress
  restriction on. The list started empty, so the first ten minutes of a real
  session were a wall of failures; and a blocked host produced a flat 403 whose
  only recourse was to stop the session, edit `config.toml`, restart, and
  re-prompt. Both are fixed. The default is unchanged — egress is still `open`
  — but the restricted mode is now something you could plausibly live in.

  **An allowlist starts from a curated list** of 19 exact hosts: the package
  registries (crates.io, npm, PyPI, Go, RubyGems), their CDNs, and the git
  forges a build reaches without anyone deciding to reach them. Your `allow`
  entries are added to it; `defaults = false` under `[network]` drops it.
  `hotl doctor` now prints the egress posture and the effective list split by
  source, and `configuration.md` enumerates the starter list inline — a default
  nobody can enumerate is a default nobody can audit, which is also why every
  entry is an exact host and a test refuses wildcards there.

  It is **not** an anti-exfiltration control and does not claim to be. It
  bounds accidents and drive-by fetches. `github.com` is on it and is
  bidirectional: a gist push leaves through it.

  **A host outside the list now prompts** on the console TUI, `hotl attach`,
  and ACP (`session/request_egress`): `y` allows it for the session, `n` denies
  it for the session. Both stick, symmetrically — remembering the deny is what
  stops a retrying command from using you as a rate limiter. Neither writes
  `config.toml`; a permanent grant stays a deliberate edit, and the prompt
  prints the line to paste. One `y` covers every connection to that host for
  the session, which is a larger grant than one prompt suggests.

  **The prompt is deliberately rare, and that is the security property.** A
  prompt you clear reflexively is not a control. So the ask does not fire for a
  host you were already shown: approve `curl https://docs.example.com/x` and
  the connection it opens does not ask again. What still asks is the surprise —
  a redirect, a CDN, a transitive dep, or a host that only appeared because an
  `[[allow]]` rule approved the command without showing it to you. *A rule is
  not a human*, so a rule-approved `curl` to an unlisted host still prompts.
  Two sharp edges: a URL carrying userinfo (`https://good.com@evil.com/`) shows
  you nothing, because your eye lands on one host while the connection goes to
  another; and editing a command at the ask makes its summary stale, so it
  shows nothing either.

  Everything that is not a live human `y` refuses. Headless (`-p`, `--schema`)
  never installs the prompt at all, so it denies by construction rather than by
  a flag; sub-agents deny with a message their model can act on; a cancelled
  turn, a two-minute deadline, a malformed reply, a dropped event all refuse. A
  deadline records nothing — a timeout is not a decision, so the next attempt
  asks again. `egress = "off"` has no prompt and cannot: the kernel refuses the
  connection with no proxy in the path to intercept. The 403 body an existing
  deployment sees is byte-for-byte what it was.

  `web_fetch`/`web_search` share the same session decision table as bash's
  proxy, so one answer covers both. Full threat model and limits:
  [SECURITY.md](docs/SECURITY.md) §Network egress.

- **Per-sub-agent worktree isolation: `isolation: worktree`.** An isolated
  sub-agent works in its own `git worktree` instead of your working
  directory, and its changes are applied back when it finishes. Turn it on
  per def (`isolation: worktree` in `agents/*.md` frontmatter — a field that
  has parsed and been ignored since M4) or for every mutating child
  (`[agents] isolation = "worktree"`). The def's own setting wins; read-only
  defs like `explore` are never isolated.

  **Isolated children run in parallel.** Two mutating children sharing one
  working tree would corrupt each other, so hotl has serialized them for a
  child's entire lifetime. Worktrees make that collision physically
  impossible, so isolated children now run at the full `[concurrency].agents`
  width and take a lock only for the duration of one `git apply`. A mutating
  child *without* a worktree is serialized exactly as before.

  Three things worth knowing before turning it on:

  - The child starts from a copy of your **current working tree** —
    uncommitted and untracked files included, so it reads what you are
    actually looking at rather than the last commit. **Gitignored files are
    not copied**: that is what keeps `target/` free, and it also means a
    child cannot read your `.env` and a child that builds pays a cold build.
  - Its changes are applied **whole or not at all**, and **never staged**.
    On conflict nothing is written, and the child's worktree is left in place
    with its path and diff reported — its work is never destroyed. Two
    isolated children can also conflict with each other; the second to finish
    loses and reports.
  - Isolation confines the **file tools**, not `bash`. hotl's kernel write
    floor is process-wide, so a child's `bash` can `cd ..` and write to your
    tree. This is isolation against accidental collision, not containment of
    a hostile child.

  Without git — or in a directory that is not a git worktree — the child runs
  in your working directory as before and the `spawn` result says so.

### Changed

- **A `[[deny]]` rule over a path now also denies the read to shell commands.
  Tightening, opt-in by writing the rule.** A path deny governed only hotl's own
  file tools: it stopped `read {"path": "/Volumes/secrets/k"}` and did nothing
  at all about `bash {"command": "cat /Volumes/secrets/k"}`. You wrote down an
  intent and half of it was enforced. A deny rule naming an existing directory
  by an **absolute** or **`~/`-rooted** `path_prefix` is now projected onto the
  kernel read-deny as a third tier, alongside hotl's own dirs and the credential
  class. It is unliftable — a deny is a "never" in-process, and `s` at a bash
  ask lifts only the credential tier.

  Projection is conservative and says what it skipped. A **floating relative**
  prefix (`.ssh/`, which matches at any depth) has no kernel expression, and
  neither does a command-subject rule (`bash prefix = "curl "`); both are still
  enforced in full in-process, and `hotl doctor` lists them under
  `not reaching shell commands` with the form to write instead. A path inside a
  write root cannot be denied at all (Landlock unions rights across ancestors)
  and is dropped with a warning rather than reported as live.
  `path_prefix = "/"` and `""` are refused at the kernel, each with its own
  message. `[[allow]]` rules move the kernel floor in neither direction.

  `hotl doctor` grows a `containment` section naming what put each denied path
  there — built-in, hotl default, or the rule text itself.

- **`~/` in `path_prefix` now expands on the deny side. Tightening.**
  `path_prefix = "~/.ssh"` matched only a literal `~/…` tool input, so it missed
  `/Users/you/.ssh/id_ed25519` — the form a model actually writes. It expands
  against `$HOME` and anchors at the root; the literal form keeps matching too.

- **`~/` in `path_prefix` also expands on the allow side. Loosening.** An
  `[[allow]]` rule with `path_prefix = "~/x"` auto-approved nothing before;
  it now auto-approves writes under `$HOME/x`. That is plainly what the rule was
  written to mean, and the alternative ships a config language where the same
  syntax works in one section and silently fails in the other — but it is an
  auto-approval that did not exist, in the tier that skips the human. Check any
  `~/`-rooted allow rule you already have.

- **The kernel sandbox denies a fixed set of reads. Default on, behavior
  change.** The floor confined writes; reads were wholly open, which left two
  live holes. The session token under the data dir is mode 0600 in a 0700
  directory — but a sandboxed `bash` runs as the *same uid*, so DAC does not
  stop it, and reading that token lets a command drive hotl's own control
  socket, which is a complete bypass of the permission gate. And `~/.ssh` /
  `~/.aws` were readable with egress open by default.

  Two tiers are now denied to every sandboxed child — the `bash` tool,
  `grep`'s ripgrep, post-edit diagnostics, and shell hooks alike. Three of
  those four have no y/N prompt, which is why the config lever below exists
  and is not merely a convenience.

  - **Tier A — hotl's config dir and data dir. Never liftable**, by config or
    by prompt. The `read`, `write` and `edit` tools refuse those paths
    outright too, in every mode, on the resolved target — a kernel rule cannot
    reach in-process tools, and an `AskProtected` a human can approve is an
    approval of the gate's own removal. The cost is deliberate: the agent
    cannot read your `config.toml` for you either.
  - **Tier B — `~/.ssh`, `~/.aws`, `~/.config/gcloud`, `~/.azure`, and
    `.netrc` / `.npmrc` / `.pypirc` / `.dockercfg`.** Sourced from the same
    constant as the execute-later *write* classification, so the read set and
    the write set cannot drift.

  **Agent-run `git push` is unaffected when your keys are in `ssh-agent`** —
  the profile deliberately keeps the agent socket reachable, and `ssh` never
  opens a key file in that case. It *does* break with no agent loaded, no
  cached cloud session, or a private-registry `npm install`. Two ways out:
  `[sandbox].readable = ["~/.aws"]` is standing (it reaches hooks and
  diagnostics, and needs a session restart), or press `s` instead of `y` at a
  `bash` ask to lift Tier B for **that one command only** — scoped around the
  single tool call, and unreachable headless or from a sub-agent.

  **Contents are denied, not existence:** `ls ~` and `ls -la ~` keep working.
  macOS denies `file-read-data` rather than `file-read*`, because denying
  metadata breaks `ls -la` (it stats every child). Landlock has no sub-path
  carve-out and its rights union across ancestors, so the denial is expressed
  by not granting read on any ancestor and re-granting every sibling. One
  measured asymmetry, documented rather than papered over: `ls ~/.ssh` still
  lists filenames on Linux where macOS hides them. File contents are denied on
  both.

  The startup probe now tests the carve in the same spawn it already used for
  the write test — one child per process, still. A host that confines writes
  but cannot narrow reads stays `Enforced` and is labeled `reads:open`;
  demoting it would revoke `bash` auto-approval for a restriction nobody
  opted into. `hotl doctor` prints the resolved deny set and the verdict.

  **Still not covered, and the docs say so:** secrets *inside* the working
  tree (`.env`), which cannot be carved without breaking the agent's job; MCP
  servers and `api_key_helper`, both spawned outside this floor; and your own
  shell, which is never confined. Egress is still open by default. This is
  confinement, not exfiltration prevention.

- **`hotl doctor` reports the read carve** — the resolved Tier A/Tier B paths,
  whether the probe certified it, and any directory the carve could not open
  (fail-closed, but worth naming).

- **Releases are gated on green CI for the exact commit being tagged.**
  `scripts/release.sh` now pushes the release commit, waits for CI, and only
  then creates the tag; `publish.yml` re-checks the same evidence before
  publishing. There is no override — no flag or env var tags without green CI,
  so a red build cannot reach crates.io, the GitHub Release, or the installer.
  `scripts/release.sh --tag-only` finishes a release whose CI went red on the
  first attempt. Maintainer tooling — no change to installed behavior.

### Fixed

- **A `[[deny]]` rule in `config.toml` never loaded at all. Tightening.** Only
  the `[[allow]]` section was lifted out of `config.toml` before the rules were
  parsed, so every user-written `[[deny]]` was dropped on the floor — it governed
  nothing, and said nothing about it. Only `/etc/hotl/preapproved.toml` could
  deny. Both sections load now, which means a deny rule already sitting in your
  config starts refusing what it always said it would.

- **`Rules::lint` had no caller.** It reports rules that can never match — an
  unknown key from a typo, a rule with no predicate, an allow rule over a tool
  with no declared subject — and nothing printed it, so a silently dead
  permission rule stayed silent. Startup and `hotl doctor` both print it now,
  along with the kernel-reach notes above.

- **The loop ledger's first phase stamp could be silently overwritten.**
  `now_nanos` reads elapsed time from an epoch initialized on its own first
  call, so the first caller in a process read `0` — which `stamp` and `width`
  both use to mean "never stamped". A real reading is now floored at 1ns, so
  `0` is only ever the sentinel. Visible as a ~1-in-5 flaky
  `first_stamp_wins`; in a real session it could drop the earliest phase
  boundary from a profile.
- **The `nix (macos-latest)` CI leg is green again.** Its darwin skip list had
  fallen behind the code by three tests that need a subprocess to run through
  the sandbox floor — impossible inside nix's own Seatbelt builder. Red since
  v0.8.0.

## [0.9.1] - 2026-07-30

### Added

- **`read` and `edit` gained a `minified` mode: a token-stream view of source
  code, and edits that match against it without ever writing it to disk.**
  Reading source is usually an agent's largest token expense, and much of a
  source file is typography — indentation, blank lines, alignment. `read` with
  `minified: true` parses the file with a tree-sitter grammar and re-joins its
  leaf tokens with the smallest separators that preserve meaning.

  Measured on hotl's own source: **20–26% fewer bytes with comments kept,
  44–59% with them stripped.** Comment-light or small files save less (10–18%
  kept). It is not a uniform win, so every minified read carries a trailer
  reporting the real figure for that file rather than a headline. Two honesty
  notes, stated in the docs as well: these are *byte* savings run through
  hotl's flat ~3 chars/token estimator, and a real tokenizer encodes a
  newline-plus-indent run as roughly one token — so the token saving is
  smaller. A JSX-heavy `.tsx` saves only on its non-JSX portion, because JSX
  whitespace is renderer-visible and is copied through untouched.

  Languages: Rust, Go, Python, JavaScript, TypeScript, TSX/JSX. Anything else
  serves the plain view.

  **`keep_comments` defaults to `true`**, because comments are meaning and
  stripping them is the lossy mode. Set `[minify] keep_comments = false` for
  the larger saving and accept that the model reads code with the *why*
  removed.

  This is not whitespace-stripping. Some languages are whitespace-*sensitive*:
  Python's indentation is syntax, and Go and JS/TS insert implicit semicolons
  at line breaks. So Python keeps one logical line per line with indentation
  renormalized to one space per level, and Go and JS/TS get explicit `;` at the
  statement boundaries where the source relied on automatic insertion — read
  from the parse tree, not guessed from a lexical trigger table, because
  `let a = b\n(c)` is one call expression while `a\n++b` is two statements and
  the token pair at the line break is identical in both. Every view is then
  re-parsed *and* its named-node structure compared against the source's; a
  mismatch is a refusal, not a warning. That check is the load-bearing one: a
  bare re-parse passes on output that means something else.

  **Every failure serves the plain view with a note saying why** — no grammar
  for the extension, a file that does not parse, a view over the 200KB cap,
  `enable = false`. The feature can cost you savings; it cannot cost you
  access. The note is as important as the fallback: it is how a stale grammar
  becomes diagnosable from the transcript instead of just producing zero
  savings forever.

  `offset`/`limit` are **refused** in minified mode rather than reinterpreted.
  They are raw-file line numbers, and the minified view has no lines the model
  can count, so paging in that coordinate system would ask the model to name
  positions it cannot see. The error names the plain read.

  **Editing.** Text quoted from a minified read will not match a plain `edit` —
  the whitespace differs. Pass `minified: true` there too: `old_string` is
  matched in the minified view, the match is projected back to exact source
  byte offsets, and only those bytes are replaced. **The file on disk keeps all
  its comments, indentation, and formatting; it is never written in minified
  form.** What makes that safe rather than clever is the position map's
  invariant — a segment's minified bytes are a verbatim copy of its source
  bytes, so the projection is arithmetic. Matching is exact and must be unique
  in that view (the domain is already whitespace-normalized, so tolerant
  matching would only blur uniqueness). Two guards refuse rather than risk the
  file: a multi-line `new_string` in Python, which would land at a source
  column the view never showed, and any splice that would leave the file no
  longer parsing — checked *before* the write, so nothing is written.

  One caveat: `new_string` lands in the file in the spelling you wrote it, so a
  minified-style replacement stays minified-style in that one spot. `cargo fmt`
  heals Rust at commit time.

  Configuration is one section, both keys defaulting on:

  ```toml
  [minify]
  # enable = true          # false makes minified:true serve the plain view
  # keep_comments = true   # false strips comments (lossy)
  ```

  **Build footprint.** The tree-sitter grammars compile C, which the workspace
  had avoided on purpose. They sit behind a default-on `minify` cargo feature,
  so `cargo install hotl --no-default-features` is still a pure-Rust build with
  no C toolchain required; in that build the `minified` argument is not
  advertised in the tool schema at all, so the model never sees an option the
  binary cannot honor.

## [0.9.0] - 2026-07-29

### Changed

- **Plan mode is an overlay, not a fourth mode.** Plan meant "pure reads
  only": every tool that wasn't `read`/`glob`/`grep` was denied outright, and
  nothing could widen it — plan's block sat above the allow tiers on purpose.
  That made it useless for the thing it should be best at. An agent asked to
  *plan* a change couldn't reach an issue tracker, fetch a doc page, or run
  `git log` to work out what to propose.

  Permissions now have two independent axes. The **mode** (`ask` | `bypass` |
  `dontask`) decides how a call is handled; **plan** decides what posture the
  session is in. Plan's entire permission effect is to put `write` and `edit`
  on the same footing as a protected path — always ask, never auto, not even
  via an `[[allow]]` rule. Every other tool takes the mode untouched:

  | | `ask` | `bypass` | `dontask` |
  |---|---|---|---|
  | **plan off** | prompt per mutating call | run without prompting | allow-rule or refuse |
  | **plan on** | same as `ask` | shell and network run freely, **file edits stop and ask** | **file edits refused** |

  Internally this needed no new mechanism. `Rules::evaluate` already had a
  tier meaning "always ask, never auto" that sits above the allow rules — the
  protected-path floor. Plan is that tier applied to two more tools, selected
  by a new `Tool::edits_files`. `read_only()` is untouched, so `dontask` and
  `ToolScope::ReadOnly` sub-agents behave exactly as before.

  Turning it on: `/plan` (or `/plan on|off`), the new `--plan` flag,
  `[permissions] plan = true`, `HOTL_PLAN=1`, or `session/set_plan` over ACP.
  The toggle is durable in its own `plan_set` log entry, independent of
  `mode_set`, so `hotl resume` restores both axes. The TUI badge carries both:
  `plan · bypass`, in the accent color.

  **Plan is a posture, not an enforcement boundary, and the docs now say so.**
  Because `bash` follows the mode, a shell redirect (`printf … > src/main.rs`)
  changes a file without ever touching the `write` tool. What plan buys is
  that the agent's *natural* mutation path stops for a human, and that a shell
  doing it instead is conspicuous in the transcript. The previous claim that
  `hotl -p --plan` "can't mutate anything, sandbox or no sandbox" is gone from
  `SECURITY.md` and `permissions-and-sandbox.md`. The unattended can't-mutate
  posture is `dontask` with no allow-rules.

- **`auto` is renamed `bypass`.** It is the mode that bypasses the gate, and
  that is a trust decision rather than a convenience — the old name read like
  the latter. `hotl setup` now writes `mode = "bypass"`, and `hotl doctor`,
  the TUI badge, `/status`, the ACP frames, and the auto-allow rule string
  (`permissions.mode=bypass`) all follow.

### Migration

Nothing to do for most users; both old spellings keep working.

- `mode = "auto"` still parses, permanently, in `config.toml`,
  `HOTL_PERMISSIONS`, ACP `session/set_mode`, and session logs. It resolves to
  `bypass`. Only the canonical output changed — new logs say `bypass`.
- `mode = "plan"` turns the overlay on and leaves the mode at your configured
  default, rather than failing closed as a typo would. This applies to
  `config.toml`, `HOTL_PERMISSIONS`, ACP, and replayed session logs alike.
- **A session logged as `plan` gains the shell and the network on its next
  `hotl resume`**, because the mode axis falls back to your default (`bypass`
  out of the box). This is the intended change, but it is the one behavior
  shift that arrives without you asking for it.
- `/mode plan` now points you at `/plan` instead of switching modes.
- ACP clients: `session/new`, `session/load`, `initialize`, and
  `session/reload_config` gained `plan` / `defaultPlan` fields, and a new
  `plan_changed` notification joins `mode_changed`. All additive, so
  `UPDATE_SCHEMA_VERSION` stands. A client still sending
  `set_mode {"mode": "plan"}` is routed to the overlay and acked, not errored.

### Fixed

- **`--plan` now exists.** `permissions-and-sandbox.md` had advertised
  `hotl -p --plan "audit X"` since the flag was documented, while `parse_args`
  rejected it with exit 2. It works now, interactively and headless.
- **`dontask` is no longer described as "the `-p`/CI posture".** Five doc
  locations said so; headless `-p` actually resolves to the configured
  default, and being headless only changes what an *ask* does (it denies).
  The prose is corrected rather than the behavior — changing `-p`'s default
  mode is a separate decision, deliberately not taken here.
- **`recall` is no longer listed as a plan-mode read-only tool.**
  `permissions-and-sandbox.md` named it alongside `read`/`glob`/`grep`, but a
  recall backend can spawn an arbitrary configured program, and a test has
  asserted `read_only() == false` for it since it left that class.
- **`scripts/release.sh` now bumps `llms.txt`.** Its version line was two
  releases stale (0.6.2) because nothing kept it in step.

## [0.8.0] - 2026-07-29

### Added

- **`hotl mcp`.** Nothing outside a live turn could say which MCP servers were
  configured, whether they were trusted, or what fingerprint the approval
  screen would show; a broken server was discoverable only mid-turn, and a
  grant could only be revoked by hand-editing `trust.toml`. `hotl mcp list`
  gives the roster with a trust state per server — trusted, screens on first
  use, unreadable binary, or session-only (a workspace script, never persisted
  by design). `show` prints the exact screen text without starting anything,
  `test` starts a server and lists its tools, `untrust` revokes.

  The command is read-mostly on purpose. `add` **prints** a `[[mcp]]` block to
  paste rather than writing `config.toml`: a CLI that edited config would be a
  path `bash -c 'hotl mcp add …'` could take, and hotl's bash analysis reads
  redirects, `tee`, and `dd` — not a program that writes config as a side
  effect. And there is no verb that *grants* trust, because that would be the
  "always allow" the permission model omits everywhere else. `untrust` is the
  one mutation, since revocation only ever reduces privilege; `test` screens
  with the same fingerprint text, refuses without a TTY, and records nothing.

- **`hotl doctor` reports MCP trust.** How many servers are configured and how
  many the gate will screen, a warning per server whose binary cannot be read,
  and a warning per grant whose server has left the config. This is also where
  `trust.toml`'s parse warnings finally surface — `load_reporting` was built to
  produce them, but the registry loads the store with `load`, which drops them,
  so a corrupt `trust.toml` previously showed up only as an unexplained wave of
  re-prompts.

- **`/reload` — pick up `config.toml` changes mid-session.** `config.toml` was
  read exactly once per process, so a new model, a new `[[allow]]` rule, a new
  MCP server or a different theme cost you the session to try. `/reload` now
  re-reads it, rebuilds the engine, and re-opens the session onto the new one
  through the ordinary resume path — the transcript stays on screen, and the
  model keeps its context, todos, name and mode. It reports what it got, plus
  any warnings the reload produced.

  Everything a scaffold owns reloads: `[provider]`, `[[allow]]`, `[[mcp]]`,
  `[[hook]]`, the skill roster, `system-prompt.md`, `[context]`. So do the
  console's own settings — theme, density, `vim_mode`, `mouse`,
  `copy_on_select`. What does not is process-wide set-once state: `[sandbox]`
  extras, `[network]` egress, the thread pools, and the prompt-history ring.
  Those are named in the notice and documented, not silently ignored — a
  reload that could widen the write floor or egress mid-process would be a
  hole, not a feature.

  Two deliberate limits. It **refuses while a turn is running** (rebuilding
  replaces the session, and the reply in flight would go with it — abandoning
  work stays the esc ladder's job), and it **does not override a mode you
  chose**: `/mode` is logged with the session and survives the reload, the same
  inheritance `hotl resume` gives you. A session that never set one has no mode
  of its own and picks up the reloaded `[permissions] mode`; either way the
  notice and the badge name what you ended up with.

  A `config.toml` that does not parse, or names a provider that cannot be
  selected, changes nothing: the running engine keeps serving and the notice
  says the previous config is still live.

  New ACP method `session/reload_config`, so an editor or orchestrator
  embedding `hotl acp` gets it too, plus additive `config_reloaded` /
  `config_reload_failed` `session/update` notifications (no schema bump). A
  connection served without a reload hook answers the method with an explicit
  error rather than a silent no-op.

- **Drag to copy.** Selecting a region of the console with the left mouse
  button copies it to the clipboard on release, giving back what mouse
  capture took away. The selection is a region of the screen, so what is
  highlighted is exactly what is copied — but dragging a transcript line from
  the left edge trims the gutter and role glyph, so pasted prose arrives
  clean. Transport is OSC 52: no clipboard dependency, and it works over SSH
  (tmux needs `set-clipboard on`). The hint row confirms with `copied N
  lines` until the next keypress. `[behavior] copy_on_select = false` turns
  it off.

- **`[behavior] mouse`.** Mouse capture is now a config key, not just the
  `HOTL_MOUSE` environment variable — which still wins when set, per the
  env > config > default precedence. Retires a documented TODO.

- **`hotl update` installs the latest release.** It reads the release's
  `dist-manifest.json`, verifies the archive's SHA-256 in process *before*
  decompressing it, unpacks only the executable (refusing absolute or `..`
  paths), runs `--version` on the result to prove it works, and only then
  renames it over the running binary — atomically, and safely while hotl is
  running. Any failure leaves the original untouched. `--check` looks without
  writing, `--version X.Y.Z` installs a specific release including an older
  one, `-y` skips the prompt.

  It replaces only installs it can own: the installer script and hand-unpacked
  tarballs. `cargo install`, Nix, Homebrew, and source builds are detected and
  told the right command instead — overwriting a cargo-installed binary would
  leave `.crates.toml` stale and be reverted by the next `cargo install`. The
  installer and `cargo install` share `~/.cargo/bin`, so the two are separated
  by cargo's own `.crates.toml` record rather than by path.

  hotl contacts the feed **only** when you run the command; there is still no
  background check. What the checksum does and does not prove is stated in the
  docs: it catches a corrupt download, not a replaced release. That is the same
  trust every existing install path already places in GitHub over TLS, and
  signing is the separate change that would raise it.

  A `security-enforced` build refuses to update, because the published binaries
  are ordinary builds and swapping one in would silently drop the enforced
  posture.

- **Image input.** Drag an image onto the console and the pasted path
  compacts to an `[Image #1]` token; on submit hotl reads the file and sends
  it to the model as a real image content block — provider-neutral
  (Anthropic base64 source blocks, OpenAI-compatible `image_url` data URLs),
  gated per model by the catalog's `images` capability, and persisted inline
  in the session log so resume and speculation byte-identity hold. png,
  jpg/jpeg, gif, webp; capped at 5MB per image, 8 per prompt, 16MB decoded
  total per prompt — an image past any cap is dropped with an inline note
  rather than failing the whole prompt, all validated at the wire before
  anything durable is written. `session/prompt` and `session/steer` accept
  an optional `images` array; the open result advertises `"images": true`
  for feature detection. Steers carry images too, and so does `/skill`.
  Recalling a prompt (`↑`) replays the image itself rather than a dead
  token, and `Backspace` swallows an attachment's token whole only while
  that attachment is still live. Past roughly 24MB of base64 image bytes
  alive in the context, the session folds older history to make room.

- **Long pastes compact.** A paste of three or more lines becomes a
  `[Pasted text #1 +N lines]` token in the composer instead of flooding it;
  the full text reaches the model on submit and the transcript keeps the
  token. `Backspace` right after a token deletes it whole; editing inside
  one turns it back into literal text. Prompt history stores the expanded
  bytes, so recalled entries stay self-contained.

### Changed

- **Release archives are `.tar.gz`, not `.tar.xz`.** `hotl update` decodes the
  archive in process, and the pure-Rust xz decoders are far narrower crates
  than `flate2`; keeping xz would have meant a C toolchain on the
  `cargo install` path. Costs a few MB per download. Assets from v0.7.1 and
  earlier stay `.tar.xz` and `hotl update --version` refuses them by name.

- **`hotl update <version>` no longer takes a positional version.** It existed
  only because nothing could fetch the real one; `hotl update` now looks it up.
  Use `--version X.Y.Z` to pin a release.

### Security

- **The protected execute-later floor is airtight, and now covers bash.**
  Writes to the execute-later class (`.git/hooks/`, `.github/workflows/`,
  `Makefile`, `build.rs`, `.envrc`, shell rc files, …) are classified on the
  filesystem-normalized path and case-folded, so a doubled separator
  (`.github//workflows/x`) or a case variant (`MAKEFILE`) can no longer slip a
  silent write past the raw-string check. `bash` is held to the same floor: a
  command that *writes* an execute-later path escalates to the protected ask
  instead of running unprompted, and on macOS the sandbox additionally denies
  those writes at the kernel. Reads and ordinary build/git flows are unchanged
  — the default stays low-friction.

- **The `hotl serve` control socket is authenticated.** A backgrounded
  session's unix socket now requires a per-session token (from the OS CSPRNG,
  written `0600` next to the socket) plus a same-uid peer check before a client
  is promoted; the run dir is `0700` and the socket `0600`. An unauthenticated
  connection is refused *without* evicting the attached client — closing a path
  by which any local process could take over a session, answer its permission
  asks, or steer other live sessions. `hotl attach` authenticates
  transparently.

- **Sandboxed tool processes no longer share hotl's controlling terminal.**
  `bash`, `grep`, and diagnostic children start their own session (`setsid`)
  rather than only a new process group, so a confined command can no longer
  inject keystrokes into — or paint spoofed UI over — the human's approval
  prompt through the shared terminal. Process-group termination is unchanged.

- **MCP server trust covers the script, not just the interpreter.** The
  first-use approval fingerprint now hashes the contents of any argument that
  resolves to a local file (the `server.js` a `node`/`python` command runs), so
  editing a trusted server's script re-raises the approval screen instead of
  silently reusing the grant. A server whose script lives inside the workspace
  is never trusted durably.

- **Deny rules apply everywhere they should.** A `[[deny]]` on a read-only tool
  (`read`, `grep`, `glob`) is now enforced — those calls previously bypassed
  rule evaluation entirely — and a `bash` deny can no longer be walked past by
  piping the command into a shell (`echo '…' | sh`), a here-string, or by
  casing the command name (`cUrl`).

- **Model-authored approval summaries are sanitized.** Every permission
  prompt's summary is flattened to a single control-character-free line at one
  engine chokepoint, so a tool argument carrying terminal escapes (a `\r`-and-
  erase sequence, a bidi override) can no longer spoof what the human is
  approving. Previously only MCP and recall summaries were scrubbed.

## [0.7.1] - 2026-07-27

### Fixed

- **A second Esc takes control back; Ctrl-C escalates to quit.** The two keys
  now share one interrupt ladder: the first press cancels the turn, the
  second stops waiting for the server. Esc detaches — the phase returns to
  Idle unconditionally, and everything the dead turn still emits is absorbed
  (usage folds into session totals; stray deltas, asks, and the late prompt
  result cannot reclaim the screen). Esc also works in the ask/question
  pickers, where it was a dead key. Ctrl-C quits: immediately when idle, on
  the second press when busy, and it is no longer swallowed by the help
  overlay. Headless `-p` gets the same second rung — a turn that ignores its
  cancel no longer leaves the process unkillable; a second Ctrl-C force-quits
  with exit code 130.

- **The Nix build's test phase passes again.** `nix flake check` — and any
  nixpkgs-style build with tests on — carried three latent failure classes,
  each hidden behind the previous by cargo's fail-fast: three
  `#[should_panic]` tests drove `debug_assert!`s that release builds compile
  out (now dev-build-only); the loop-overhead perf gates compared a
  real-hardware baseline against the builder's scratch filesystem, where
  `sync_data()` is nearly free (now skipped in the checkPhase); and the Linux
  builder has no writable path outside the sandbox floor — `/var/tmp` does
  not exist there — so the eight tests that must witness a write outside the
  floor are skipped on Linux, as the darwin floor list (now extended with the
  0.7.0 `[sandbox]` tests) always was. Every skipped test still runs on the
  raw CI runners, where the coverage is real.

### Changed

- **CI is faster.** Pushes that touch no build inputs (docs, changelog, site)
  skip the two ~8-minute cold Nix legs; the audit job restores a cached
  `cargo-audit` binary (~3 minutes → ~15 seconds) while still fetching
  current advisories on every run; the MCP client suite dropped a 20-second
  timeout sleep to 2 seconds under `cfg(test)`; and the Linux clippy gate is
  clean again after the `[sandbox]` refactor left an orphaned Landlock
  wrapper behind.

- The site's quickstart and uninstall pages document the Nix install path.

## [0.7.0] - 2026-07-27

### Fixed

- **A crashed MCP server's stderr now reaches the error on every disconnect
  path.** When the server process died before a request could be written to
  it, the failure surfaced as a bare `server pipe closed: Broken pipe` — the
  EPIPE write path skipped the grace-and-compose step the EOF read path
  already had, so the server's own diagnostics (usually the one line that
  explains the crash) were dropped. Both paths now append the stderr tail.
  This race is also what intermittently failed the release test gate from
  v0.5.2 on.

### Added

- **`[sandbox].writable` — owner-configured writable directories.** The kernel
  write floor (working directory, temp, `/dev`) can now be widened with
  directories listed in config.toml, for every sandboxed spawn — bash, grep,
  diagnostics, hooks — so tools that keep caches outside the workspace (bazel,
  ccache) work under the floor instead of dying on their first write. Missing
  directories are created at startup; entries are canonicalized and validated
  fail-closed, one by one. An entry that is, contains, or sits inside hotl's
  own config or data dir is refused — a writable config dir would let a
  sandboxed command rewrite the allow-rules and hooks that govern it — which
  is also what keeps `~` and `/` unlistable. Risky system roots (`/etc`,
  `/usr`, …) are honored with a loud warning. The startup probe picks its
  outside-the-floor target outside the *widened* set, so `sandboxed:` still
  means proven, and `hotl doctor` prints the resolved list plus every
  validation warning.
- **`[sandbox].file_tools = "writable"`** separately opts the `write`/`edit`
  tools into those same directories. A write there becomes an ordinary ask
  (the tier `mode = "auto"` approves) instead of the protected one, and runs
  through the same symlink-refusing fd-descent guard as workspace writes,
  anchored at the extra root. Protected filenames still escalate — the grant
  widens *where* the tools may write, never what kind of write is waved
  through. The default `"workspace"` keeps file tools workspace-only, and an
  unknown value fails closed to it with a warning.

## [0.6.2] - 2026-07-27

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
