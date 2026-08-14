---
title: 'Skills — indexed, never preloaded'
description: Saved procedures the agent loads on demand — the grouped index, search over collapsed sources, bodies read fresh at call time, why there is no index database, the trust envelope, and skill marketplaces.
---

A skill is a saved procedure — a markdown file the agent loads by name and
follows: a deploy checklist, a review protocol, a scaffolding recipe. The
design commitment is that skills never sit in the context waiting to be
used: what rides on every request is a compact grouped index, and a skill's
text enters the conversation only at the moment the agent loads it.

## Where skills come from

Five kinds of root, read in place:

| Root | What it is |
|---|---|
| `~/.config/hotl/skills/*.md` | Your own flat files — one procedure per file. |
| `[plugins.sources]` entries | [Agent Plugins](../plugins/) — each plugin's `skills/*/SKILL.md`, the primary package lane. |
| `[skills.marketplaces]` entries | Registered git checkouts or local dirs, walked for `SKILL.md` (see [marketplaces](#skill-marketplaces)). |
| `~/.claude/skills/<name>/SKILL.md` | Your Claude Code skills. |
| `~/.claude/plugins/cache/…/skills/<name>/SKILL.md` | Claude Code plugin skills — highest installed version per plugin. |

Claude-format skills load **in place** — no porting, no copying. The body
loads on demand prefixed with its base directory so `references/` and
`scripts/` paths resolve (scripts still run through the normal bash gate and
sandbox). Bare names resolve by precedence — hotl's own skills, then your
plugins, then your marketplaces, then your Claude skills, then Claude
plugins — and a plugin or marketplace skill is always *also* addressable as
`source:skill`, which is the only form when its bare name is taken. Opt out
of the Claude roots with:

    [skills]
    claude = false

## The design: three stages of disclosure

**Stage 1 — always in context: a grouped index, nothing more.** The `skill`
tool's description shows one line per *source*, with descriptions left out
entirely, and any source over 12 skills collapsed to its first three names
plus a count:

    hotl: deploy, release
    claude: auth, go-service, system-shape, vps-cluster
    claude:superpowers (14): brainstorming, executing-plans, writing-plans, +11 more

On a 24-skill roster that index measures about 150 tokens where a full
roster of names and descriptions took 980. More important than today's
number is the shape: the index grows per **source**, not per skill, so
registering a 300-skill marketplace adds one line to every request, not 300
names.

**Stage 2 — descriptions on request.** From the index the agent has three
moves:

| Call | What it does |
|---|---|
| `{"name": "deploy"}` | Loads that skill. The usual call. |
| `{"query": "review a pull request"}` | Searches every skill's full description — **including collapsed ones** — and returns the best 8. |
| `{"source": "superpowers"}` | Lists one source in full. |

Calling the tool with no arguments still lists everything. Search is what
makes collapsing safe: a collapsed skill is hidden, not unreachable.
Results are ranked by where the query's words land — a hit on a skill's
name outranks a hit in its description — in a deterministic order, with
descriptions returned untruncated. Zero matches returns every skill name
rather than a dead end.

**Stage 3 — the body, only when named.** Loading a skill reads its file
from disk at the moment of the call. Bodies are never preloaded and never
cached, so an edit to a skill — yours, or one Claude Code just updated
under `~/.claude` — is what the very next load serves.

## Why there is no index database

A design like this invites a persistent index — SQLite, full-text search,
a build step. hotl deliberately has none, because the numbers say no:

- **The searchable corpus is one-line descriptions.** Ranking a few dozen
  — or a few thousand — of those in memory takes microseconds. An index
  earns its keep on corpora orders of magnitude larger.
- **Staleness is unrepresentable.** Discovery rescans the roots at process
  start and bodies are read at call time, so there is nothing to
  invalidate — two of the four roots belong to Claude Code and change out
  from under hotl, and it never matters.
- **No shared state.** A persistent index is a second source of truth that
  every concurrent session would have to lock and trust — the kind of
  hidden moving part the harness's [design commitments](../overview/) rule
  out, and the skill system honors that.

The cost of the rescan is a frontmatter-only walk, once per process — and
the alternative is machinery whose only job is to equal what the walk
already delivers.

## Trust

A skill instructs; it never authorizes. Loaded bodies arrive inside the
same provenance-tagged envelope as every other untrusted input —
`<skill name="…" trust="untrusted">`, with closing-tag sequences
neutralized so content cannot break out of it — and carry a reminder that
a skill cannot authorize tool use or override what you say in the session.
Everything a skill asks for still passes the ordinary
[permission gate and sandbox](../permissions-and-sandbox/). A body past the
eviction threshold is moved to a file like any oversized tool result, with
a preview inline. Sub-agents never receive the `skill` tool at all
([agents.md](../agents/)).

## Forcing one yourself

In the console TUI, type `/` and the skill name:

    /brainstorming redesign the skill system

Built-in commands (`/rename`) win the name; anything else is looked up as a
skill, with the rest of the line passed along as arguments. An unrecognised
name stays an unknown-command notice and never reaches the model. This is
the manual override for the times the agent doesn't think to search — see
[tui.md](../tui/) for the `/` menu.

## Skill marketplaces

Register extra skill sources — any git repo or local directory containing
`SKILL.md` skills:

```toml
[skills.marketplaces]
acme = "https://github.com/acme/skills.git"   # managed checkout
team = "~/work/team-skills"                   # local, read in place
```

Git sources are cloned by `hotl skills add acme <url>` (or `hotl skills
update` for an entry added by hand) into `~/.config/hotl/marketplaces/<name>`
and refreshed only by `hotl skills update` — never at startup. Skills are
discovered up to four directory levels below the root, so flat
(`<skill>/SKILL.md`) and plugin-repo (`plugins/<p>/skills/<s>/SKILL.md`)
layouts both work. A skill whose bare name is taken stays addressable as
`<marketplace>:<skill>`.

## The `hotl skills` CLI

| Command | What it does |
|---|---|
| `hotl skills` | List every discovered skill with its source and full description — the human view never collapses. |
| `hotl skills add <name> <url\|path>` | Register a marketplace (clones a git URL; a local path is read in place). |
| `hotl skills update [name]` | Refresh one git marketplace, or all of them. |
| `hotl skills remove <name>` | Unregister a marketplace. |
