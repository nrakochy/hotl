---
title: Sub-agents (spawn, agent defs)
description: Delegate self-contained subtasks to fresh, isolated sub-agents — built-in shapes or your own agents/*.md definitions.
---

The `spawn` tool hands a self-contained subtask to a fresh sub-agent: its own
engine, its own session log, its own isolated context. It runs to completion
and returns only its final result — useful for focused, separable work
(research a question, summarize a large file, audit a directory) that would
otherwise crowd the parent's context.

```json
{"agent_type": "explore", "task": "find every place TokenUsage is summed"}
```

One child per call is `spawn`'s whole shape. To run **many** — phases of
agents, a pipeline per item, majority votes, repeat-until-quiet — hand the
`workflow` tool a declarative plan instead; it drives the same agent defs
through the same child builder, with one ask for the whole run. See
[workflows.md](../workflows/).

## Choosing an `agent_type`

Three built-in agent types ship with hotl:

| `agent_type` | Tools | Use for |
|---|---|---|
| `general-purpose` *(default)* | Base builtins only: `read`/`edit`/`write`/`bash`/`glob`/`grep` — never the parent's `web_fetch`/`web_search`/MCP/skills/`recall` | Open-ended subtasks: research, implement, summarize. |
| `explore` | Read-only (`read`/`glob`/`grep`) | Fast search — locate code, files, answers. Safe to fan out several at once. |
| `plan` | Read-only | Investigate, then propose a step-by-step plan without touching the workspace. |

Beyond the built-ins, define your own in `agents/*.md` under your config dir
(`~/.config/hotl/agents/`, alongside `skills/`):

```markdown
---
name: reviewer
description: reviews diffs against the house style
tools: read-only
model: claude-haiku-4-5-20251001
---
You are a strict code reviewer. Read the diff, flag correctness and
style issues, and say nothing else.
```

Frontmatter fields:

| Field | Meaning |
|---|---|
| `name` | The `agent_type` string `spawn` matches on. Falls back to the filename if omitted. |
| `description` | Shown to the model alongside the built-ins when it's choosing an `agent_type`. |
| `tools` | `all` (default) \| `read-only` \| a comma list of tool names (`read, grep, bash`). |
| `model` | Override the child's model. Omit to inherit the parent's. |
| `effort` | Reasoning depth for this child: `low` \| `medium` \| `high` \| `xhigh` \| `max`. Replaces the parent's for that child only. Omit to inherit. An unrecognized value warns and the def still loads. |
| `isolation` | `worktree` gives this def's children their own git worktree to work in; `none` (default) shares your working directory. See [Worktree isolation](#worktree-isolation) below. Beats the `[agents] isolation` default. |
| `max_turns` | A ceiling on the child's turns, replacing the engine default for that child only. A non-numeric value warns and the def still loads. |

A def's `effort` is what makes the depth ladder compose with fan-out: a
read-only searcher can run cheap under a parent thinking hard.

    ---
    name: explore-cheap
    description: Fast read-only search, at the bottom of the ladder.
    tools: read-only
    effort: low
    ---
    Locate the code and report file:line. Do not analyze.

Spawn that from a session at `effort = "high"` and the parent keeps its depth
while every child it fans out runs at `low`. See
[configuration.md](../configuration/#reasoning-effort-provider-effort) for the
ladder itself and how each provider spells it.

The body after the `---` fence is the child's system prompt. Omit it to
inherit the parent's system prompt unchanged (useful for a def that only
narrows the tool set, like a stricter `explore`).

**`~/.claude/agents/*.md` loads too** (Claude Code's own agent format), the
same opt-in-by-default, opt-out convention as skills:

    [agents]
    claude = false

**Built-in names always win.** A user def named `explore` or `plan` is
ignored with a startup warning — never a silent override. This is the same
rule Claude Code's own corpus converges on: user definitions cannot shadow
the built-ins.

## `fork`: continue with your own context

```json
{"agent_type": "general-purpose", "task": "keep going on this from where I left off", "fork": true}
```

A plain `spawn` starts the child with nothing but the task brief. `fork:
true` instead seeds the child with *your own current context* — a
history-inheriting continuation, not a fresh start. Use it when the
sub-agent genuinely needs what you've already learned this session (files
read, decisions made) rather than a self-contained brief it can act on
alone.

When the chosen `agent_type` doesn't change the system prompt or model, the
seed is byte-identical to your own context (verbatim history, brief appended
as the next turn) — the fork's first request can then replay your
provider-side cached prefix instead of paying full input price for a large
session. A def that *does* override the system prompt or model (like the
built-in `explore`/`plan`, which have their own persona) can't reuse that
cache anyway, so `fork` instead wraps your history into an explicit,
labeled background block the child receives as context, not as its own
prior turns.

## The return contract

A sub-agent gets its brief twice — inline in its first message, and as a file
at `<data dir>/spawn/<id>/TASK.md` it is told to read first. The file is
there because a long brief is recalled better when the child can go back and
re-read it; the inline copy is what makes the first turn actionable without
one.

`TASK.md` also carries a **slice** of your plan, when you have one: every
decision you have recorded, and only the steps the brief names by id plus the
one in progress. Not the whole plan — a child briefed on one file that reads
twenty unrelated steps is being invited to overstep, and a self-contained
subtask is the point. Decisions travel whole because they are short and a
child cannot rediscover them: re-litigating a settled choice is the expensive
failure. The child is told it is *contributing to* those steps, not
completing them, so `completed` keeps meaning "my brief is done".

It answers through a tool, not through prose. Every child carries
`report_result`, registered past whatever `tools:` its def names — it is how
the child speaks at all, so a narrow tool list cannot remove it:

| Field | |
| --- | --- |
| `outcome` | `completed`, `blocked`, `needs_input` or `unverifiable` — required |
| `summary` | what it found or did, at most 6000 characters (≈1,500 tokens); a longer one is **refused**, not truncated |
| `files_touched` | every path it created, edited or deleted |
| `commits` | commit shas it made |
| `citations` | `path:line` for each claim worth checking |
| `question` | required with `needs_input`: the one thing a human must decide |

The parent sees that object, pretty-printed, inside the usual untrusted
envelope — tagged `typed="true"` so a client can tell a shape from prose.

A child that stops without reporting is asked twice more ("Call report_result
now with what you have"). If it still doesn't, the parent writes the result
itself: `{"outcome": "unverifiable", "summary": <the child's last words>}`.
The parent always gets a shape — what it never gets is a claim of success
nobody made.

**Two clocks bound a child**, and neither one throws away an answer. A child
that sends nothing at all for `[agents] child_idle_secs` (default 300s) is
stopped and comes back `{"outcome": "unverifiable", "reason": "idle"}` — a
stuck sub-agent is reported, not waited on. A child that has already reported
and then hangs (an MCP server holding stdout open, say) is reaped after
`completion_grace_secs` (default 20s) **and its result is kept**: the answer
is on disk, and what was being waited for was only the process.

A child that runs out of turns gets exactly one more prompt — "call
report_result now with what you have; do not start new work" — and for that
prompt the only tools it is offered are reads and `report_result`. "Do not
start new work" is the shape of the roster, not just a sentence in a prompt.

`workflow` agents are the exception: they answer against their phase's own
JSON schema, so they are not given `report_result`. Two return contracts in
one roster is just a way to lose the reply.

## The permission envelope

What a sub-agent may do is fixed before it starts, and every axis of it only
ever narrows:

| | Where it comes from | Rule |
| --- | --- | --- |
| Permission mode | forced, never inherited | always `dontask` — a child has no human on the loop, so `ask` would deadlock and `bypass` would let a sub-agent do without a human what you are standing right there to authorize |
| Allow / deny rules | inherited verbatim | your `[[allow]]`/`[[deny]]` apply inside children exactly as they do to you |
| Tool set | the def's `tools:`, intersected with yours | `tools: all` under a read-only parent is read-only; a name you don't have is not one the child gains |
| `spawn` / `workflow` | never present | depth-1 is structural — naming them in `tools:` does nothing |
| Worktree isolation | the def's `isolation:`, then `[agents] isolation` | not a call argument; `spawn {isolation: …}` is refused as an unknown key |
| Hooks | inherited | they run inside children too, under `actor: "child:<id>"` — and, as everywhere, a hook can only deny |

So a mutating call inside a child runs only if one of your own allow rules
already covered it. Anything else is denied with a message the sub-agent can
act on, never a prompt nobody is there to answer.

## Depth, isolation, and trust

- **Depth is capped at one level, structurally.** A child's registry is
  built fresh and never contains `spawn` — a user agent def cannot re-enable
  recursion by naming `spawn` in `tools:`. There is no config knob for this
  today; it's a hard invariant.
- **A sub-agent's result is untrusted content to the parent.** Everything a
  child returns — including a `fork`'s eventual result — is wrapped the same
  way a `recall`/`web_fetch` result is: data that can inform the parent's
  work, never an instruction it can act on unprompted. A forged closing tag
  inside a child's output is defanged before it reaches the model.
- **A sub-agent has no human on the loop.** Its permission asks default-deny
  — it can only do auto-allowed or read-only work. Give a mutating def
  matching allow-rules if you want it to actually write/run commands.
- **Concurrent children share one budget.** `[concurrency].agents` (default
  4) bounds how many children run their expensive step (the LLM call) at
  once, globally across the whole process — a model that issues 30 `spawn`
  calls in one batch still only runs 4 at a time; the rest queue. Two
  *mutating* children (anything broader than a read-only tool scope) that
  share your working directory never run concurrently regardless of that
  budget — two children editing the same tree at once would corrupt each
  other. **Isolated children are the exception**: they each edit their own
  worktree, so they run at full width. Read-only fan-out (`explore`) has
  never been affected.
- **Identical siblings start staggered.** Fan out three `explore` children at
  once and all three would send the same system prompt and tool roster before
  the provider has cached any of it — three cache *writes* of the same bytes.
  The first one goes; the rest wait for its first response byte (up to
  `[agents] prefix_stagger_ms`, default 5s) and then start as cache reads. A
  sibling whose prefix differs at all — a different def, model or tool set —
  is never held, and a first child that dies without speaking frees its peers
  on the timeout. Set `prefix_stagger_ms = 0` to turn it off.
- **`teammate` (a peer topology, not a child) is reserved** — not available
  yet.

## Worktree isolation

Turn it on per def:

```
---
name: refactorer
isolation: worktree
---
```

…or for every mutating child:

    [agents]
    isolation = "worktree"

The def's own `isolation:` wins where both are set. Read-only defs
(`explore`, `plan`, anything with a read-only tool scope) are never isolated
— they cannot write, and a checkout per child would be pure cost on the
fan-out hot path.

**What the child starts from.** A copy of your **current working tree** —
including uncommitted and untracked files, so the child reads what you are
actually looking at, not the last commit. **Gitignored files are not
copied.** That is what keeps `target/` and `node_modules/` free — but it
also means a child cannot read your `.env`, and a child that builds pays a
cold build.

**What happens when it finishes.**

- Its changes are applied to your working tree **whole or not at all**, and
  **never staged** — your index is untouched either way.
- On conflict nothing is applied. The child's worktree is left in place and
  its path is reported along with the diff, so its work is never destroyed.
- Two isolated children can produce diffs that conflict with *each other*.
  The second to finish loses and reports. That is the accepted cost of
  running them in parallel.
- If you keep editing while a child runs, a child seeded three turns ago may
  be diffing against a base you have moved past. `git apply` refuses; the
  work is preserved and reported rather than merged wrong.

**The bash caveat.** The file tools (`read`/`write`/`edit`/`glob`/`grep`)
are strictly confined to the worktree. `bash` is not: hotl's kernel write
floor is process-wide, so a command can `cd ..` and write to your tree. This
is isolation against *accidental* collision between children, not
containment of a hostile one.

**Where it lives.** `<workspace>/.git/hotl-worktrees/<id>` — inside `.git/`,
so it never shows up in `git status`, `glob`, or `grep`.
Worktrees are removed when the child finishes; a conflicted one stays until
you deal with it.

**Without git** — no `git` on `PATH`, or a workspace that is not a git
worktree — the child runs in your working directory as usual and the
`spawn` result says so.

See [Configuration → Concurrency](../configuration/#concurrency-concurrency)
for the full `[concurrency]` reference, and
[permissions-and-sandbox.md](../permissions-and-sandbox/) for how permission
gating and the untrusted-content envelope work everywhere else in hotl.
