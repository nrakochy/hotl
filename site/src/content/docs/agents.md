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

## Choosing an `agent_type`

Three built-in agent types ship with hotl:

| `agent_type` | Tools | Use for |
|---|---|---|
| `general-purpose` *(default)* | Everything the parent has, minus `spawn` | Open-ended subtasks: research, implement, summarize. |
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
| `effort` | Parsed, not yet applied — hotl has no effort ladder today (only a thinking on/off switch). Reserved for a future release. |

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
  *mutating* children (anything broader than a read-only tool scope) never
  run concurrently regardless of that budget — per-child workspace isolation
  (`isolation: worktree`) isn't built yet, and two children editing the same
  tree at once would corrupt each other. Read-only fan-out (`explore`) is
  unaffected and can run at full width.
- **`teammate` (a peer topology, not a child) is reserved** — not available
  yet.

See [Configuration → Concurrency](../configuration/#concurrency-concurrency)
for the full `[concurrency]` reference, and
[permissions-and-sandbox.md](../permissions-and-sandbox/) for how permission
gating and the untrusted-content envelope work everywhere else in hotl.
