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

Eight roots, read in place and in this order:

| # | Root | Shown as | Also addressable as |
|---|---|---|---|
| 0 | `~/.config/hotl/skills/*.md` | `hotl` | — |
| 1 | `<dir>/.agents/skills/<name>/SKILL.md` for each `dir` from the working directory up to the repo root, nearest first | `project` | `project:<name>` |
| 2 | `<dir>/.claude/skills/<name>/SKILL.md`, the same walk | `project` | `project:<name>` |
| 3 | `[plugins.sources]` entries — [Agent Plugins](../plugins/), each plugin's `skills/*/SKILL.md` | `<plugin>` | `<plugin>:<name>` |
| 4 | `[skills.marketplaces]` entries — registered git checkouts or local dirs, walked for `SKILL.md` (see [marketplaces](#skill-marketplaces)) | `<marketplace>` | `<marketplace>:<name>` |
| 5 | `~/.agents/skills/<name>/SKILL.md` — the directory Codex, Gemini, Goose, Pi, OpenCode and Copilot also read | `agents` | — |
| 6 | `~/.claude/skills/<name>/SKILL.md` — your Claude Code skills | `claude` | — |
| 7 | `~/.claude/plugins/cache/…/skills/<name>/SKILL.md` — Claude Code plugin skills, highest installed version per plugin | `claude:<plugin>` | `<plugin>:<name>` |

A repo carries its own skills under `.agents/skills` or `.claude/skills`. The
walk starts at the working directory and stops at the repo root (the nearest
directory holding `.git`; with no `.git` anywhere, the working directory
alone), so a skill in a parent checkout never loads. The nearest directory
wins a name inside the project tier.

Claude-format skills load **in place** — no porting, no copying. The body
loads on demand prefixed with its base directory so `references/` and
`scripts/` paths resolve (scripts still run through the normal bash gate and
sandbox). A bare name goes to the first root in the table that has it; your
own flat files always win, so a cloned repo cannot redirect `/deploy`. Every
root below the first keeps its `source:skill` alias, which is the only form
when the bare name is taken. Two roots that can only claim bare names
(`agents`, `claude`) lose a taken name outright. Either way the loser is
reported, never dropped in silence: `hotl skills list`, `hotl doctor` and the
startup warnings all print
`skill \`deploy\`: project …/.claude/skills/deploy shadowed by project …/.agents/skills/deploy`.

Switch roots off in `[skills]`:

    [skills]
    project = false   # no .agents/skills or .claude/skills from the repo
    agents = false    # no ~/.agents/skills
    claude = false    # no ~/.claude/skills, no Claude plugin skills

## Frontmatter hotl reads

A `SKILL.md` starts with a `---` fence. hotl reads these keys and ignores
the rest; a flat `~/.config/hotl/skills/*.md` file has no frontmatter and
takes its description from its first line.

| Key | Effect |
|---|---|
| `name` | The skill's name. Letters, digits, `.`, `_`, `-`, alphanumeric first, at most 64 characters; anything else falls back to the directory name with a warning. |
| `description` | What the skill is for — what `{"query"}` searches and what the `/` menu shows. |
| `paths` | Globs (`src/**`, `*.toml`, `crates/*/src/**`) the skill applies to: an inline list, a comma-separated string, or `- item` lines. See [Skills that follow the path](#skills-that-follow-the-path). |
| `disable-model-invocation` | `true` hides the skill from the model's index, the full listing, `{"query"}` and `{"source"}`. It still loads by exact name, which is what `/<skill>` does. |
| `user-invocable` | `false` takes the skill out of the `/` menu and the editor's skill list. The model still sees and loads it. Both flags set is a skill nothing can reach — `hotl doctor` says so. |
| `argument-hint` | Opts a `SKILL.md` into [`$ARGUMENTS` substitution](#forcing-one-yourself). Flat files substitute without it. |

Booleans are `true` or `false`; anything else keeps the default and warns.

## Skills that follow the path

A skill with `paths:` stays out of the always-sent index for the whole
session — showing it after the first touch would change the cached prefix
mid-session. Instead, the first time a tool reads or edits a file one of its
globs covers, one short harness-written line enters the transcript:

    Skill `rusty` applies to `src/lib.rs`. Load it with skill{"name":"rusty"} if it fits the task.

Once per skill per session, counted under harness injections in `/context`.
The line never quotes the skill's own text, so a repo skill cannot inject
through it. Globs are relative to the working directory; a path outside it
never matches. The skill is still reachable by exact name, `{"query"}` and
`{"source"}`, and in the `/` menu. A glob that does not parse is a warning,
and the skill behaves as a plain one.

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

**The catalog budget.** The index also has a ceiling: `[skills]
catalog_tokens`, by default 2 % of the session model's context window
(never below 400 tokens, never above 10,000). When the index would be
larger, it is squeezed one rung at a time, lowest root in the table first,
until it fits: a source collapses to three names and a count, then to a
bare count, then leaves the index with a trailer saying how many sources
`{"query"}` still reaches. Your own flat root never moves. `hotl skills
list` prints the budget and every rung taken (`catalog: 812 bytes of 1200
(0 steps)`), and `hotl doctor` warns when any was.

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
A project skill is no different: a cloned repo's `.agents/skills` is text
under the same envelope, and the one line a `paths:` skill can place on its
own is written by hotl, not by the skill. The write side is covered too — a
write under `.agents/` or `.claude/` is an execute-later path, so the model
cannot plant a project skill under `bypass` without the ⚑ notice.
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

The arguments reach the skill body. The prompt tells the model to call
`skill{"name":"brainstorming","arguments":"redesign the skill system"}`, and
at load the body's placeholders are filled:

| Placeholder | Becomes |
|---|---|
| `$ARGUMENTS`, `$@` | The whole argument string. |
| `$1` … `$9` | One word each — split on whitespace, with `"quoted words"` kept together. A missing word is empty. |
| `${2:-staging}` | Word 2, or `staging` when there is none. |

Any other `$` is left alone. Your flat `~/.config/hotl/skills/*.md` files
always substitute; a `SKILL.md` substitutes only when its frontmatter has
`argument-hint:`, because a shared skill may carry a literal `$1` inside a
shell snippet. With no arguments the body is served as written, placeholders
and all, so the model sees what the skill expects.

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
| `hotl skills` | List every discovered skill with its source, its flags (`model-hidden`, `user-hidden`, `paths`, `args`) and full description — the human view never collapses — then its `paths:` globs, the catalog budget line, and every warning (name collisions, bad frontmatter, unreachable skills). |
| `hotl skills add <name> <url\|path>` | Register a marketplace (clones a git URL; a local path is read in place). |
| `hotl skills update [name]` | Refresh one git marketplace, or all of them. |
| `hotl skills remove <name>` | Unregister a marketplace. |
