---
title: Driving hotl from another program
description: How a separate program starts, finds, drives, restarts and reads results from many hotl sessions on one machine, over ACP.
---

You can run hotl from your own program instead of typing into it. Your program
starts sessions, sends them prompts, answers their permission asks and reads
their results. This page is for teams building that program: a dashboard, a
job runner, or an editor plugin that manages many agents at once.

hotl speaks ACP, the Agent Client Protocol ("a protocol editors use to embed an
agent"). Messages are JSON-RPC: one JSON object per line, each request with an
`id`, each reply carrying the same `id`. The [wire schemas](../wire-schema/)
page lists every message hotl adds.

## What you get

- **One process per session.** Each hotl process runs one session at a time.
  Opening a second session on the same connection ends the first. To run ten
  sessions, start ten processes. Your program keeps the list of them.
- **Standard ACP version 1.** A test in this repository drives `hotl acp` with
  the official ACP Rust library and fails if any message hotl writes does not
  decode with it.
- **Extras when you ask for them.** hotl's own additions are keys whose names
  start with `hotl.dev/`. hotl sends them unasked only to a program that opts
  in. To opt in, send this inside `initialize`:
  `clientCapabilities._meta["hotl.dev/extensions"]: 1`. One key is the
  exception: `hotl.dev/output` answers a request your program made, so it goes
  to any program that asked (see [Ask for a JSON answer](#ask-for-a-json-answer)).

## Start a session

There are two ways, depending on whether the session should outlive your
program.

| You want | Run | You talk to it over |
|---|---|---|
| A session that ends when your program ends | `hotl acp` | The process's standard input and output |
| A session that keeps running on its own | `hotl bg [prompt]` or `hotl serve --id <id>` | A local socket |

**`hotl acp`.** Start it as a child process and write JSON lines to it. Open a
session with `session/new`, then send `session/prompt`.

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{"session":{"notices":{}},"_meta":{"hotl.dev/extensions":1}}}}
{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/Users/you/work/app","mcpServers":[]}}
```

**`hotl bg`.** This starts `hotl serve` in the background and returns at once.
It prints the run id (`bg-` and a number), the attach command and the path of
a log file. The background process inherits your shell's model setting and
API key.

**`hotl serve`.** This is the process `hotl bg` starts. Run it yourself when
your program wants to own the process and choose its run id.

| Flag | What it does |
|---|---|
| `--id <id>` | The run id. Without it, hotl uses `bg-` and its own process number. If a live session already uses that id, hotl exits with code 1. |
| `--prompt <text>` | A first prompt to start on. |
| `--name <name>` | A session name. |
| `--worktree <name>` | Run in a new git worktree (`@derived` lets hotl pick the name). |
| `--resume <session-id>` | Reopen a stored session (see [Restart after a crash](#restart-after-a-crash)). |

A served session waits for a program to connect. When it needs a permission
answer and nothing is connected, it holds the ask until a program connects and
answers it.

## Find running sessions

```
hotl attach --json
```

This prints one JSON object per line, one line per running served session,
sorted by run id. With no sessions running it prints nothing. It always exits
with code 0.

One line looks like this (the values are an example):

```json
{"runId":"bg-48211","sessionId":"01JDX3Q8M2K7T9V4B6N1C5R0FZ","pid":48214,"cwd":"/Users/you/work/app","name":null,"version":"0.32.0","startedAtMs":1763740800000,"state":"idle"}
```

| Field | Meaning |
|---|---|
| `runId` | The id of the running process. You connect with it. |
| `sessionId` | The id of the stored session log this process is writing. |
| `pid` | The process number of the `hotl serve` process. For `hotl bg` it differs from the number in the run id, which is the number of the `hotl bg` command that started it. |
| `cwd` | The folder the session works in. |
| `name` | The session name, or `null`. |
| `version` | The hotl version running it. |
| `startedAtMs` | When it started, in milliseconds since 1970. |
| `state` | `idle`, `running` or `waiting`. |

The three states:

| State | Meaning |
|---|---|
| `idle` | No task is running and nothing is waiting for an answer. |
| `running` | A task is running. |
| `waiting` | The session is waiting for an answer: a permission ask, a question from the model, or a request for outgoing network access. This wins over `running`. |

Each line comes from a status file, `<runId>.status.json`, in hotl's `run`
folder (`~/.local/share/hotl/run/` on macOS and Linux, unless
`XDG_DATA_HOME` moves it). Only you can read it. hotl writes the file whole
when the state changes, so a reader never sees half a file. hotl removes it
when the session stops. A file whose process has died is skipped. On macOS
and Linux, `hotl gc` deletes it along with that session's socket, access key
and log.

Only sessions started with `hotl serve` or `hotl bg` have a status file. A
`hotl acp` session over standard input and output does not appear here.

## Connect to a running session

Each served session has two files in the `run` folder:

| File | What it is |
|---|---|
| `<runId>.sock` | The socket, a local connection point your program opens. |
| `<runId>.token` | The access key. Only you can read it. It holds the key and nothing else. |

Open the socket and send `initialize` with the access key in
`params._meta["hotl.dev/socketToken"]`. A missing or wrong key closes the
connection with no reply. Then send `session/resume` with the run id as the
session id.

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{"session":{"notices":{}},"_meta":{"hotl.dev/extensions":1}},"_meta":{"hotl.dev/socketToken":"<contents of bg-48211.token>"}}}
{"jsonrpc":"2.0","id":2,"method":"session/resume","params":{"sessionId":"bg-48211","cwd":"/Users/you/work/app"}}
```

The reply carries the current permission mode and settings. Any ask the
session was holding arrives next, as `session/request_permission`. Earlier
history is not sent again. `session/new` and `session/load` are refused on the
socket, because the session already exists. `session/close`, or closing the
connection, leaves the session running. `_hotl/session/stop` ends it.

## Ask for a JSON answer

Put a JSON Schema in the prompt's `_meta` under `hotl.dev/outputSchema`. Any
program can do this, whether or not it opted in.

```json
{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":"<session id>","prompt":[{"type":"text","text":"count the open issues"}],"_meta":{"hotl.dev/outputSchema":{"type":"object","required":["n"],"properties":{"n":{"type":"integer"}}}}}}
```

The reply carries the checked answer:

```json
{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn","_meta":{"hotl.dev/output":{"n":3}}}}
```

What happens in between:

1. hotl checks the schema first. If it is not a JSON object or not a valid
   schema, the request fails with error `-32602` and the model is never called.
2. hotl adds a short instruction to your prompt asking for one JSON object that
   fits the schema, and nothing else.
3. When the task finishes, hotl reads the model's final reply as JSON. A reply
   wrapped in a fenced code block is accepted. hotl then checks it against
   the schema.
4. If the check fails, hotl tells the model what was wrong and asks again. It
   does this at most twice. The retries also pass the schema to the model's
   API when the schema allows it.
5. If the answer still does not fit after two retries, the request fails with
   error `-32003`. Its message starts with
   `output did not validate after 2 retries:` and names what was wrong.

Things to know:

- **A retry waits its turn.** It is queued like any other prompt. If your
  program sent another prompt first, that prompt runs before the retry. Replies
  can then arrive in a different order from your requests. Each request still
  gets exactly one reply, so match replies by `id`.
- **Only a finished task is checked.** If the task is cancelled, or ends in an
  error or at a limit, you get the ordinary reply with no `hotl.dev/output`. A
  cancelled task is not retried.
- **Cost covers every try.** The usage in `hotl.dev/turn` adds up the first
  try and the retries.

A program that opted in also sees `agentCapabilities._meta["hotl.dev/outputSchema"]: true`
in the `initialize` reply. A program that did not opt in is not told, but the
key still works for it.

## Read cost and outcome

When your program has opted in, every `session/prompt` reply carries
`_meta["hotl.dev/turn"]`. When the task ends in an error, it is inside the
error instead, at `error.data._meta["hotl.dev/turn"]`. The `-32003` error
above carries it there too.

Models measure text in tokens, roughly three-quarters of a word.

| Part | What it holds |
|---|---|
| `outcome.kind` | How the task ended: `done`, `cancelled`, `turn_limit`, `refused`, `doom_loop`, `tool_failure_budget`, `denial_spiral`, `budget` or `error`. Some kinds carry more fields, such as a `message`. |
| `usage.input_tokens`, `usage.output_tokens` | Tokens sent and received. |
| `usage.cache_read_input_tokens`, `usage.cache_creation_input_tokens` | Tokens read from and written to the provider's prompt cache. |
| `usage.hit_ratio` | The share of input tokens read from the cache. Left out when the cache was not used. |
| `usage.cost_usd` | The cost in US dollars. Left out when hotl has no price for the model. |

The cost is priced at the session's main model. If the task fell back to
another model part way through, the figure does not reflect that model's
price.

## List past sessions and how they relate

`session/list` returns stored sessions, 50 per page. When there are more, the
reply has a `nextCursor`. Send it back as `cursor` to get the next page. Pass `cwd` to see only sessions from one folder. A session you
reopened many times shows as one row, under the id of its newest log.

For a program that opted in, a row can carry `_meta["hotl.dev/session"]`:

| Field | Meaning |
|---|---|
| `parentId` | The listed row this session was forked from. |
| `runId` | The run id of the running `hotl serve` process that holds this session, if one does. |

Either field is left out when it does not apply. A row with neither has no
`_meta`. A session that was only reopened, not forked, has no `parentId`.

`parentId` names a row in the full list. With a `cwd` filter, or on a later
page, the parent may not be in the rows you got back.

## Throwaway sessions

Add `"hotl.dev/open": {"ephemeral": true}` to the `_meta` of `session/new` to
open a session that leaves no session log behind.

```json
{"jsonrpc":"2.0","id":4,"method":"session/new","params":{"cwd":"/Users/you/work/app","mcpServers":[],"_meta":{"hotl.dev/open":{"ephemeral":true}}}}
```

While it runs, hotl keeps its log, saved outputs and any sub-agent logs in a
private temporary folder. hotl deletes that folder when the session closes.
The session does not appear in `session/list`, and `session/load` of its id
fails.

Not everything is thrown away. Memory notes and plan files are still written
where they always are, under `~/.local/share/hotl/memory/` and
`~/.local/share/hotl/plans/`. File changes the agent makes in your project
stay too.

`ephemeral` works only on `session/new`. It is refused with error `-32602` on
`session/load`, `session/resume` and `_hotl/session/fork`, and together with a
worktree.

## Restart after a crash

If a served session's process dies, start it again from the stored log:

```
hotl bg --resume <session-id> [prompt]
hotl serve --id <id> --resume <session-id> [--prompt <text>]
```

`<session-id>` is the full id of a stored session, such as the `sessionId` from
`hotl attach --json`. Names, id prefixes and `@last` are not accepted here. If
the session was reopened before, hotl continues from its newest log.

What happens:

- hotl writes a new log file that continues the old one. The new
  `sessionId` in the status file is that new log's id.
- The session keeps the permission mode and plan mode from the stored log,
  and reports them to the program that connects.
- It works in the folder you ran the command from, or in the session's own
  worktree if it had one.
- If the stored history ends on your prompt, or on tool results the model had
  not answered yet, hotl starts the task again from there. If it ends on a
  finished reply, the session waits for a prompt.

What is lost or paid for twice:

- A reply the model was still writing when the process died was never saved.
  hotl asks the model again, so you pay for that step twice.
- A tool call the model asked for, whose result was never saved, is recorded
  as failed with the text `Not executed (the turn ended first).` hotl cannot
  tell whether the call had already started. If it had, its effects stay, and
  the model may run it again.

Refusals:

| Case | Result |
|---|---|
| No stored session with that id | `hotl serve` exits with code 1: ``no stored session `<id>` `` |
| The session is already running in another process | `hotl serve` exits with code 1: `session <id> is already running as <runId> — attach to it instead` |
| `--resume` together with `--worktree` or `-w` | Exits with code 2. A resumed session reopens its own worktree. |

`hotl bg` prints its start message before the background process opens the
session. If the resume is refused, the reason is in the log file `hotl bg`
printed, and the session never appears in `hotl attach --json`.

## Pace your launches

Each hotl process retries the model on its own when a request fails. Nothing
coordinates retries between processes. If you start many sessions at once and
the provider limits your rate, they all retry separately.

To see retries as data, your program must offer both of these in
`initialize`:

- `clientCapabilities.session.notices` (any object, such as `{}`), and
- `clientCapabilities._meta["hotl.dev/extensions"]: 1`.

Then each retry arrives as a `session/update` of kind `notice`, titled
`Retrying (<attempt>/<max>)`, with `_meta["hotl.dev/retry"]`:

| Field | Meaning |
|---|---|
| `attempt` | Which try is about to be made, counting from 1. |
| `max` | How many tries are allowed. |
| `status` | The HTTP status that caused the retry. Left out when there was none. |
| `delayMs` | How long hotl waits before the next try, in milliseconds. |
| `scope` | What was being retried: `sample` (a reply from the model), `summarize` (summarizing older history), `goal_eval` (checking a `/goal` condition), `title` (naming the session) or `aside` (a `/btw` side question). |

A program that offered notices but did not opt in gets the notice without the
`_meta`. A program that did not offer notices gets no retry notice at all. Use
`delayMs` and `status` to hold back new launches while sessions are being
slowed down.

## What it is not

- **One program per session.** A served session talks to one connection at a
  time. A second connection takes over and the first is closed. There is no
  read-only second viewer.
- **A lost connection loses pending replies.** If your program disconnects
  while a prompt is running, the task keeps going, but its reply is not sent
  to the next connection.
- **Same machine only.** hotl listens only on a local socket, never on a
  network port. To drive it from another machine, forward the socket over ssh
  (OpenSSH can forward a unix socket with `ssh -L`) and read the access key on
  the machine that runs hotl.
- **No shared rate limit.** Each process retries on its own. Pacing many
  sessions is your program's job.
- **No lock on two resumes at once.** The already-running check reads the
  status files once, before the session opens. Two `--resume` commands for
  the same session started at the same moment can both pass it and continue
  the session into two separate logs. Do not launch two.
- **The current todo list and goal are not sent on connect.** A program that
  connects to a served session learns them only when one of them changes.
- **`--resume` needs a value.** `--resume` with nothing after it is ignored,
  and a fresh session starts instead.
- **`hotl fleet` is not this.** The `hotl fleet` command is reserved and does
  nothing yet. A program that manages many sessions is a separate project
  that drives hotl as described on this page.
