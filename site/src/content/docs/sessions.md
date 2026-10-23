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

The picker's label is the session's name, else a generated title marked
`(auto)`: after an unnamed console session's first completed turn, a
~50-token call on the utility model names it in at most six words. Names
outrank titles everywhere — the badge, the terminal title, the picker — and
`-r <name>` matches names only, never a title. `/rename` mid-first-turn wins
over the title; a resumed chain is never re-titled; a fork inherits neither
label. `[behavior] auto_title = false` turns it off; `-p` runs never title.

The console shows the inherited reality immediately, before any turn runs:
the status strip carries the inherited context fullness (`N% ctx`) and todo
list, and `/effort` reports the inherited level. Usage counters and cost
start at zero for the resumed session — only the context gauge carries over.
A resumed session runs the last model `/model` switched it to, when this
machine can serve it; otherwise it falls back to the configured model with a
warning naming the one it could not run. If the model changed since the
session last ran — by config or by `/model` — a one-line transcript notice
names the switch: the inherited transcript was produced by the previous model.

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

## Worktrees

```
hotl -w fix-auth              # a fresh git worktree on branch fix-auth, from the local HEAD
hotl -w                       # branch and directory named after the session id: hotl/<id>
hotl -p "do it" -w fix-auth   # headless too; hotl bg -w fix-auth as well
hotl -r fix-auth              # reopens the same worktree
hotl -r fix-auth --no-worktree   # …or back in the main checkout
```

`-w [name]` starts the session in its own git worktree so two sessions on
one repo stop stepping on each other. The worktree lives at
`<repo>/.git/hotl-worktrees/<name>` — under `.git/`, so it is invisible to
the main checkout's `git status`, `glob` and `rg`, and inside the sandbox's
write floor without moving it. Run `hotl -w` from the repository root; from a
subdirectory it refuses and names the root. The branch is cut from your
**local** `HEAD`, never a remote ref, so unpushed commits are there; an
existing, free branch is reused.

What changes inside the session: `read`/`write`/`edit`/`glob`/`grep` resolve
relative paths in the worktree, `bash` starts there, the orientation item
the model reads shows the worktree's branch and status, and sub-agents
spawned from the session merge back into **it**. The first transcript line
says `working in worktree .git/hotl-worktrees/fix-auth on branch fix-auth`;
the terminal title reads `hotl · fix-auth ⎇ fix-auth`, and `hotl watch`
shows the branch. `-w name` also names the session (`-n name`) unless you
pass `-n` yourself; a bare `-w` names nothing.

Rules worth knowing:

- **Uncommitted changes stay where they are.** The worktree starts at
  `HEAD`; your dirty main checkout is not copied into it (Amp's shape).
- **Kept on exit.** Quitting never removes a worktree; `hotl gc`, run in the
  repo, does — see [configuration.md](../configuration/#hotl-gc). While a
  session is open the worktree is locked (`git worktree list` shows
  `locked hotl <id> pid <n>`); a clean exit unlocks it, a crash leaves the
  lock for `gc` to read.
- **Resume reopens it.** `hotl -r` on a worktree session relocks and roots
  there; a directory you deleted is recreated from its branch; a branch you
  deleted refuses, and `--no-worktree` resumes in the main checkout and
  remembers that. A second `hotl -r` of a session that is still open
  elsewhere is refused. A fork inherits its parent's worktree; `--fork-from
  X -w name` opens a new one from the current `HEAD` instead. `-w` with `-r`
  is an error.
- **`bash` is not confined to the worktree.** The kernel floor is the
  checkout you ran `hotl` in; a `cd ../../..` reaches the main tree. The
  file tools are rooted; `bash` is cooperative.
- **Nothing gitignored is there.** A worktree has no `.env` and no
  `target/`; point `CARGO_TARGET_DIR` (or your build's equivalent) at a
  shared directory if cold builds hurt.

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

This is a script you run, not a cron job you schedule — for the scheduled kind
see [Unattended loops](#unattended-loops-cron) below. (One residual worth
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

## Unattended loops (cron)

A cron job, a Makefile or a shell loop can drive hotl the way a person does:
run a prompt, pick up yesterday's session by name, and read the exit code to
tell "the cap stopped it" from "it broke".

```sh
#!/bin/sh
# ~/bin/hotl-gc — a tidy-up loop; crontab: */30 * * * * ~/bin/hotl-gc >> ~/hotl-gc.log 2>&1
set -u
cd /path/to/repo || exit 2
NAME="gc-$(date +%G-%V)"          # one chain per ISO week; see "Why a weekly name"
if hotl export "$NAME" >/dev/null 2>&1; then OPEN="--resume $NAME"; else OPEN="-n $NAME"; fi
flock -n /tmp/hotl-gc.lock \
  hotl -p "Entering the GC pass. Fix one failing lint or one TODO you can finish in this \
turn, run the checks, and stop. Say what you changed." $OPEN --goal 'tests_green("make check") and turns <= 6'
case $? in
  0) ;;                               # done
  4) echo "stopped by a cap";;        # max_turns / max_cost_usd — raise or accept
  5) echo "harness brake — look at the log";;
  *) echo "failed ($?)";;
esac
```

**What the loop may do.** Headless auto-denies every ask, so list what the
loop is allowed in `[[allow]]`: `cargo`, `make`, `git add`, `git commit` —
never `git push` from a loop.

**The stop condition.** `--goal` is the loop's stop condition and `turns <= N`
its ceiling. The [exit codes](../configuration/#exit-codes) say how it ended:
`4` is a cap you set, `5` a brake the harness pulled, `1` a failure or an
impossible goal.

**Why a weekly name.** Every `--resume` adds one log to the chain, `hotl gc`
never prunes an ancestor of a live chain, and replay keeps the newest 32 hops.
A loop that resumed one name forever would keep every log and, past 32 runs,
lose its oldest context. A dated name starts a fresh chain each week and lets
`[retention]` reclaim the old ones. When the new week should remember how the
last one ended, open it with `--fork-from @last --keep-turns 2 -n "$NAME"`
instead of `-n "$NAME"`.

**One at a time.** `flock` is there because cron does not wait for the
previous run. Pipe `--json` into `jq` for a dashboard. `hotl stats --days 7`
says what the loop cost; `hotl export "$NAME"` shows what it did.

**Windows.** Task Scheduler runs the same command; the chain lives under
`%LOCALAPPDATA%\hotl\data`. `flock` has no twin — use a lock file, or the
scheduler's "do not start a new instance".

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
To keep working in the *same* session instead, `/compact <what the next phase
needs>` folds the history in place ([tui.md](../tui/#folding-on-purpose)).

## Seeing lineage

`hotl -r`'s picker annotates any session that descends from another with
`↳ from <parent-id-prefix>`. `hotl gc` never prunes a session that a retained
session descends from — including its evicted-result blobs — so forking does not
put the parent's history at risk of collection.
