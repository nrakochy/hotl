---
title: Sessions, resume, and forking
description: Continue a session, or fork one into a new session that inherits its history — the basis for multi-phase work that keeps perfect recall instead of a handoff summary.
---

Every session is an append-only log in `~/.local/share/hotl/sessions/<ulid>.jsonl`.
The conversation you see is a projection of that log, which is why hotl can
reconstruct a session later without keeping anything in memory between runs.

Two things you can do with an earlier session:

- **Resume** it — continue the same conversation. The new session inherits the
  history and picks up where you left off (`-r`, `hotl resume`).
- **Fork** it — start a *new* session seeded with that history, optionally only
  a prefix of it. The original is untouched and can keep running.

Resume is for "carry on". Fork is for "branch from here".

## Resuming

```
hotl -r                 # pick from recent sessions (numbered, newest first)
hotl -r <n|id|name>     # list number, id prefix, or session name
hotl <id-prefix>        # same, positionally (id prefix only)
hotl resume [arg]       # the subcommand spelling of -r
```

A resumed session that was interrupted mid-turn finishes that turn on load.

The console shows the inherited reality immediately, before any turn runs:
the status strip carries the inherited context fullness (`N% ctx`) and todo
list, and `/effort` reports the inherited level. Usage counters and cost
start at zero for the resumed session — only the context gauge carries over.
If the configured model changed since the session last ran, a one-line
transcript notice names the switch: the inherited transcript was produced by
the previous model.

## Forking

```
hotl --fork-from <n|id|name|@last> [-n <name>]
hotl --fork-from @last --keep-turns 3
hotl -p "write the plan" --fork-from auth-explore
```

`@last` is the most recently active session — what makes a scripted pipeline
possible without capturing ids.

`--keep-turns <n>` forks at the end of the *n*-th completed turn instead of at
the head. `--keep <items>` does the same in raw projection items, for scripts
that want exact control; it must land on a turn boundary, and the error names
the nearest valid one if it doesn't. Only one of the two per invocation, and
neither means anything without `--fork-from`.

### What a fork inherits, exactly

- **The parent's history, frozen at the fork point.** The fork records the
  parent's tip entry, so the parent's own session can keep working — appending
  turns, or compacting — without ever rewriting what the fork inherited. Fork a
  live session freely.
- **The parent is never written to.** Forking is a read.
- **Not the parent's name.** Two live sessions sharing one name would break
  `-r <name>`. Give the fork its own with `-n`.
- **Permission mode and plan mode**, from the parent's *final* values — even
  when you fork at an earlier prefix. Mode changes aren't positioned in the
  transcript, so there is nothing to rewind them to. Worth knowing if you fork
  back past a `/plan` toggle: check the badge.
- **Todos only when forking at the head.** A fork cut back to an earlier prefix
  drops them: a checklist describing work the fork's history no longer contains
  is worse than no checklist.
- **No auto-continue.** Resuming an interrupted session finishes the interrupted
  turn; forking one does not. You forked to send it somewhere else, and
  answering the parent's stale prompt first would spend a full-price sample
  doing it.

**Lineage depth.** Replay follows at most 32 ancestors. A long daily chain of
forks and resumes eventually sheds its oldest history, with a warning when it
does. When you see that warning, start fresh rather than chaining further.

## Phase pipelines

The reason forking exists. Multi-phase work — explore, then plan, then refine —
normally means summarizing what one agent learned so the next can read it. A
fork skips the summary: the next phase has the *raw* exploration, every file
read and every tool result, exactly as it happened.

```sh
hotl -p "Explore how the auth flow works. Read broadly; do not propose changes." -n auth-explore
hotl -p "Entering phase: Plan. Using only what you learned above, write an implementation plan." --fork-from @last -n auth-plan
hotl -p "Entering phase: Refine. Revise the plan against these review notes: …" --fork-from @last
```

Two things are true about this, and they are worth keeping apart.

**Perfect recall, always.** The planning phase reads the exploration itself, not
a lossy description of it. Nothing about that depends on timing — it is the
durable reason to prefer a fork over a handoff summary, and it holds a week
later just as well as a second later.

**A cache read, when the phases run back to back.** Providers price a request
prefix that is byte-for-byte identical to a recent one at roughly a tenth of
normal input cost. A fork's first request *is* such a prefix — hotl proves this
on wire bytes, not by intention — so an inherited transcript that would
otherwise be re-billed in full costs about 10%. The catch is the cache's
lifetime, and it is shorter than you might guess: **five minutes for headless
`-p` runs**, one hour for console sessions. That is not a setting — hotl picks
it per surface, because a console session has human pauses worth paying the
longer cache's premium for and a scripted run does not.

So a `-p` pipeline has a five-minute window between phases. Run them back to
back and you get the discount; leave a gap and the first request pays full price
once, then caches normally. hotl prints a note when the session you are forking
looks too cold for it.

This is a script you run, not a cron job you schedule. (One residual worth
naming: cache matching looks back a bounded distance from the request's newest
breakpoint, so a pathologically block-heavy final turn can leave a sliver of the
tail uncached. The rolling anchors bound how big that sliver can get.)

The OpenAI dialects route by a caller-supplied key before the prefix hash, so
hotl sends the session id as `prompt_cache_key` and a session's samples stay on
one cache shard. On GPT-5.6 and later the cache reads only at explicit
breakpoints (and bills writes at 1.25×), so hotl sends explicit-only mode with a
marker on the last block of every durable user message and tool result — the
per-sample todo reminder and turn context ride after the last marker, where
they can change without touching the cached prefix. Earlier OpenAI models, and
compatible servers, cache implicitly and get no breakpoints; a model name that
hides the version can be forced either way with `[provider] cache_breakpoints`.

## Keeping the discount: phase instructions go in the prompt

The system prompt is byte-stable for a session's lifetime by design — every
dynamic thing hotl adds (memory, project instructions, todo reminders,
per-sample metadata) rides as a tagged message or a post-cache-marker block
instead. That is what makes an inherited transcript reusable at all.

So a phase instruction is a **prompt**, never a per-phase system prompt. Give
each phase its own persona in the system prompt and you have changed the first
bytes of every request: a full-price cold start, in a different cache namespace,
every phase. A test in the tree asserts a fork with a different system prompt
registers as a prefix break, precisely so this cannot be re-added quietly.

The same rule explains the two `spawn` fork shapes described in
[sub-agents](../agents/): a def that overrides the system prompt or model
can't reuse the cache anyway, so `fork` wraps the history into a labeled
background block for it instead of replaying it as the child's own turns.

**Cheap summarizer.** To have a wrap-up fork run on a cheaper model, give its
agent def its own `model:` ([sub-agents](../agents/)). That is cache-breaking by
definition — a different model is a different cache namespace — but still often
worth it on a long transcript: you pay full input once on a cheap model instead
of having an expensive one re-read everything. (`[provider] fast_model` is a
different thing: it is the model hotl uses for its own compaction summaries, and
it does not affect what a fork runs on.)

## The wrap-up recipe

End a long session by asking the agent to spawn a fork:

> spawn a general-purpose sub-agent with `fork: true` and the task "write the
> handoff summary of everything above"

The summary is written *with the entire tool-call history in hand* — no handoff
prompt to compose, nothing selectively remembered. The wrap-up child is a
first-class descendant in the store: it records its parent and its fork point,
so the lineage-aware `hotl gc` will not prune the history it descends from.

## Seeing lineage

`hotl -r`'s picker annotates any session that descends from another with
`↳ from <parent-id-prefix>`. `hotl gc` never prunes a session that a retained
session descends from — including its evicted-result blobs — so forking does not
put the parent's history at risk of collection.
