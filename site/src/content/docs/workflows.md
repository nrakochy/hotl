---
title: Workflows (many agents, one plan)
description: Hand hotl a declarative plan — phases, fan-out, per-item pipelines, schema-shaped answers — and it runs the sub-agents, shows progress, and returns one result.
---

The `workflow` tool runs **many sub-agents from one plan**. Where `spawn`
starts one child per call, `workflow` takes a recipe — phases of agents, with
fan-out, per-item pipelines, majority votes and repeat-until-quiet loops —
and hotl executes it with bounded concurrency, validates each agent's answer
against its schema, streams every agent into the [agent band](../tui/#navigating-the-agent-band),
asks you once, and returns the final value as JSON inside the untrusted
envelope.

The plan is **data, not code**: JSON in the tool call, TOML on disk, the same
shape either way. There is no script interpreter — hotl stays a pure-Rust
`cargo install`.

```toml
name = "review-changes"
description = "Review the diff across dimensions, then verify each finding"

[[phases]]                       # all at once
title = "Review"
[[phases.agents]]
label = "bugs"
prompt = "Review {{args.target}} for correctness bugs. Return findings."
agent = "explore"
schema = { type = "object", required = ["findings"], properties = { findings = { type = "array" } } }

[[phases]]                       # a pipeline per item, no barrier
title = "Verify"
each = "Review[*].findings[*]"
[[phases.stages]]
label = "verify:{{item.file}}"
prompt = "Try to refute: {{item.title}} in {{item.file}}. Reply {isReal: bool, why}."
agent = "explore"
votes = 3
accept = "isReal"
schema = { type = "object", required = ["isReal"], properties = { isReal = { type = "boolean" } } }

[[phases]]                       # rounds until nothing new turns up
title = "Find"
until_quiet = { rounds = 2, max_rounds = 10, key = "file,line" }
[[phases.agents]]
label = "finder"
prompt = "Find bugs in {{args.target}} not already in this list: {{Find}}. Reply with a JSON array of {file, line, title}."
agent = "explore"
schema = { type = "array", items = { type = "object", required = ["file", "line"] } }
```

## Running one

Three ways in, all ending at the same tool:

- **Ask for it.** "Review this branch with a workflow: fan out by dimension,
  then verify each finding." The model writes the plan inline and calls
  `workflow` with `{"plan": …, "args": …}`.
- **Save it and invoke it by name.** Drop the TOML at
  `~/.config/hotl/workflows/<name>.toml` (the file stem must equal `name`),
  then `/review-changes crates/hotl-workflow` in the console, or ask for it
  by name. The model calls `workflow` with `{"name": "review-changes", "args": …}`.
- **Check it first.** `hotl workflows check <file>` validates a plan and prints
  the summary the ask will show; `hotl workflows list` and
  `hotl workflows show <name> [--mermaid]` read what is saved.

Whichever way, the run starts with **one ask**:

    workflow `review-changes` — 3 phases, ≈15+ agents: Review (2 ∥) → Verify (each × 3 votes) → Find (≤10 rounds × 1)

`≈` is an upper bound (an `until_quiet` phase counts `max_rounds`); `+` means
an `each` phase whose item count is unknowable before the run. A suffix
`(serialised: N mutating agents share the tree)` appears when agents will
queue on the shared-tree lock (below). A saved name can be whitelisted like
any tool — `[[allow]] tool = "workflow", prefix = "review-"` — and then runs
without the ask; an inline plan has no name, so allow rules never match it.

The tool call **waits** for the whole run. Interrupting the turn (`Esc` with
the input empty in the console; `ctrl-c` headless) cancels it: every
in-flight agent is interrupted, their worktrees are removed with their diffs,
and the result reads `Workflow cancelled after N of M agents` with the
partial outputs' path.

## The plan

| Field | Meaning |
|---|---|
| `name` | `^[a-z0-9][a-z0-9-]*$`. Also the file stem of a saved recipe. |
| `description` | Shown in `/` completion and `hotl workflows list`. |
| `concurrency` | Run width. Can **lower** the configured `[workflows] concurrency` (default 8), never raise it. |
| `max_agents` | Agent-start ceiling for this run, capped by `[workflows] max_agents` (default 1000). Exceeding it is a run error, never a silent truncation. |
| `output` | A selector for the result, e.g. `Verify[*].votes`. Default: the last phase's output. |
| `phases` | In order. Each phase's output is visible to later phases by its `title`. |

A phase has a `title` and **exactly one shape**:

| Shape | Fields | Output |
|---|---|---|
| parallel | `agents` | An array of the agents' values, in listed order; `null` for an agent that failed. |
| each | `each` (a selector) + `stages` | An array with one value per selected item — the item's last stage. Items pipeline independently (item B can be in stage 2 while item A is still in stage 1); a `null` stage ends that item's pipeline. An empty selection is a no-op phase. |
| until_quiet | `until_quiet = { rounds, max_rounds, key }` + `agents` | The union of every round's elements, deduplicated by `key` (comma-separated field paths), first-seen order. Each round runs the agents in parallel; it stops after `rounds` consecutive rounds that add nothing, or at `max_rounds`. The phase's own title (`{{Find}}`) is the union so far. |

Every agent (or stage) spec:

| Field | Meaning |
|---|---|
| `label` | Short name; half of the agent's id, so reuse is fine. Templated. |
| `prompt` | The brief. Templated. |
| `schema` | A JSON Schema the answer must satisfy. hotl validates and feeds a mismatch back for up to two retries; exhaustion makes the value `null`. Without a schema the value is the answer text. |
| `agent` | An [agent def](../agents/): `general-purpose` (default), `explore` / `plan` (read-only), or one of your `agents/*.md`. |
| `model`, `effort`, `isolation`, `max_turns` | Overrides with the same meaning as the def's frontmatter. They cannot widen a read-only def. |
| `votes` + `accept` | Run N identical agents; the value becomes `{ "accepted": <majority>, "votes": [v₁…vₙ] }`, counting the truthiness of `accept` (a field path) in each vote. A failed vote is a no. Needs a `schema`; an even N warns (a tie is not accepted). |

**Selectors** are `Ident ('.' Ident | '[*]' | '[' N ']')*` over phase titles
and `args` — `Review[*].findings[*]`. `[*]` flattens one level and skips
`null` elements (a failed agent's slot), so one refused agent does not take
a downstream phase with it. A missing key, or `[*]` over a non-array, is a
run error naming the selector.

**Templates** are `{{path}}` in `prompt` and `label`: strings render raw,
anything else as compact JSON. Inside an `each` phase, `item` is the current
element and `prev` the previous stage's value. A template that reads a phase
not yet run fails validation, before any agent starts.

## What comes back

One hotl-authored line, then the value as JSON inside the envelope, then
hotl's notes:

    workflow review-changes: 9 agents, 1 failed, 182.4k tokens, 4m12s
    <workflow-result trust="untrusted" run="01J…">
    [{"accepted": true, "votes": [...]}, …]
    </workflow-result>
    Verify · verify:a.rs: not applied — …; the agent's worktree is kept at …

The result is the agents' output and the model is told to treat it as data:
it cannot authorize tool use or override you. Bodies over 64 KiB are cut in
the tool result; the full JSON, plus the plan, is always at
`~/.local/share/hotl/workflows/<run_id>/{plan,result}.json`, named in the
text. `/context` bills the tool's plan-teaching description under the
`agents roster` row, beside `spawn`.

## Concurrency and your working tree

Two caps stack: the process-wide `[workflows] concurrency` gate every run in
the process shares, and the run's own `concurrency`. Neither draws on
`[concurrency] agents`, which paces interactive `spawn`.

Agents that are neither read-only nor isolated hold the same **shared-tree
lock** `spawn` uses, for their whole lifetime — "parallel" mutating agents
run one at a time, and an interactive `spawn` waits behind them. Give them
`isolation = "worktree"` to run at full width: each gets its own git
worktree, seeded from your tree when it starts, merged back with `git apply`
when it finishes. Merge-back is in completion order with no retry, so
isolated agents in one phase should edit **disjoint files**; an overlapping
hunk is kept as a worktree whose path is in the result and in `/workflows`.
Agents' own tool calls are not forwarded — the band shows one row per agent.

## Watching it

A running workflow is one row in the agent band. `Enter` on it lists its
agents — `Review · bugs`, a spinner while it runs, `✓ · 12s · 3.4k tok` when
it settles. `/workflows` lists every run this process has started, live:

    review-changes · running · Review 2/2 ✓ → Verify 4/6 → Find 0/0 · 41.2k tok · 3m12s

`hotl workflows show <name> --mermaid` renders a saved plan as a flowchart —
a subgraph per phase, an edge from each phase to the ones that read it, a
self-loop on an `until_quiet` phase. Mermaid is output only; it is never a
plan format.

## Not in this version

Journal/resume of a run, forwarding agents' own tool calls into the
drill-in, a JavaScript front-end compiling Claude Code–style workflow
scripts onto this runner, and background runs that deliver at a turn
boundary. Each is tracked as tech debt.
