---
title: 'Wire schemas'
description: "JSON Schemas for hotl's machine wires: the -p --json event stream and what hotl adds to ACP. Generated from the Rust types and published with these docs."
---

hotl has two machine-readable wires. Each has a JSON Schema generated from the
Rust types that produce it:

| Wire | Version | Schema |
|---|---|---|
| `hotl -p --json` event stream | `schema_version` on every line (now `2`) | [`json-stream-v2.schema.json`](/hotl/schema/json-stream-v2.schema.json) |
| `hotl acp`: hotl's additions to ACP v1 | ACP `protocolVersion` `1` | [`acp-hotl-extensions.schema.json`](/hotl/schema/acp-hotl-extensions.schema.json) |

Each file's `$id` is its URL. `hotl schema json-stream` and `hotl schema acp`
print the same bytes, so a client can pin the schema of the binary it runs.

## The version rule

An **additive** change, such as a new frame type or a new optional field, keeps
the version and the same file. A **breaking** change bumps the version and
publishes a new file beside the old one (`json-stream-v3.schema.json`). The old
URL keeps working. Optional fields are left out rather than sent as `null`,
unless the schema says `null` is a value.

The old [`acp-v1.schema.json`](/hotl/schema/acp-v1.schema.json) is still
published for that reason. It describes hotl's own ACP-like protocol
(`protocolVersion: "0.1"`), which this release removed. It is frozen and no
longer matches anything hotl sends.

## ACP: standard v1 plus opt-in extras

`hotl acp`, the console, `hotl serve` and `hotl attach` speak standard
[Agent Client Protocol](https://agentclientprotocol.com) v1. Every standard
message follows the ACP v1 schema, so hotl does not publish a copy of it. A
test in this repository drives the built `hotl acp` with the official ACP Rust
SDK and fails if any message hotl writes does not decode through the SDK's
types.

hotl's own extras are off until a client asks for them. A client opts in by
sending `clientCapabilities._meta["hotl.dev/extensions"]: 1` in `initialize`.
Without that, hotl sends no `_hotl/` method and no `hotl.dev/` key of its own
accord. It still answers `_hotl/` requests the client chooses to send. One key
is an answer rather than an extra: a client that sends
`hotl.dev/outputSchema` on a prompt gets `hotl.dev/output` back whether or not
it opted in.

`acp-hotl-extensions.schema.json` describes only the extras:

| Part of the file | What it holds |
|---|---|
| `methods` | Each `_hotl/` method: who sends it (`from`), its `params` and its `result` |
| `meta` | Each `hotl.dev/` `_meta` key: the messages it rides on (`on`) and its `schema` |
| `$defs` | The shapes the two lists point to |

The `_hotl/` methods:

| Method | Does |
|---|---|
| `_hotl/session/fork` | Starts a new session from a copy of an earlier one (`keepItems` or `keepTurns` keeps only its first items or turns) |
| `_hotl/session/rename`, `set_color`, `set_goal` | Name, color and goal of the session |
| `_hotl/session/steer` | Adds text to the turn that is running |
| `_hotl/session/fold` | Summarizes the history now (`/compact`) |
| `_hotl/session/aside` | Asks a side question that does not enter the history (`/btw`) |
| `_hotl/session/remember` | Saves a memory note, or returns the memory report |
| `_hotl/session/reload_config` | Re-reads `config.toml` and re-opens the session |
| `_hotl/session/context`, `workflows`, `jobs`, `children` | Reports: what fills the context, workflow runs, background jobs, sub-agents |
| `_hotl/session/jobs/stop`, `children/stop` | Stops one job or sub-agent |
| `_hotl/session/stop` | Ends the session (on `hotl serve`, ends the background process) |
| `_hotl/session/update` | Agent to client: an event with no standard update, such as a goal verdict or a finished job |
| `_hotl/session/request_question` | Agent to client: the model asks you a question, when the client has no form support (`elicitation/create`) |

The `hotl.dev/turn` key carries a turn's outcome, token use and cost. It rides
on the `session/prompt` result. When a turn ends in an error, it rides inside
the JSON-RPC error instead, at `error.data._meta["hotl.dev/turn"]`.

Keys for programs that drive hotl (see [Driving hotl from another program](../embedding/)):

| Key | Where it appears | Who gets it | Holds |
|---|---|---|---|
| `hotl.dev/outputSchema` | In a prompt the client sends | Any client may send it | A JSON Schema the answer must fit |
| `hotl.dev/outputSchema` | In hotl's answer to `initialize`, set to `true` | Clients that opted in | That hotl accepts the key above |
| `hotl.dev/output` | In hotl's reply to that prompt | Any client that sent a schema | The checked answer. If it still does not fit after two retries, the prompt fails with error `-32003` instead |
| `hotl.dev/session` | On each row of a session list | Clients that opted in | `parentId`, the listed row the session was forked from, and `runId`, the running `hotl serve` process holding it. Each is left out when it does not apply |
| `hotl.dev/retry` | On a "Retrying" notice | Clients that opted in and offered notices | `attempt` (the try that just failed), `max`, `status` (when there was one), `delayMs` and `scope` |

`hotl.dev/open` on `session/new` also takes `ephemeral: true`, which opens a
throwaway session whose log is never stored. It is refused on `session/load`,
`session/resume` and `_hotl/session/fork`, and together with `worktree`.
