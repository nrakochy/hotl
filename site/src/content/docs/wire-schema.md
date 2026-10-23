---
title: 'Wire schemas'
description: "JSON Schemas for hotl's two machine wires — the -p --json event stream and the ACP-dialect payloads — generated from the Rust types and published with these docs."
---

hotl has two machine-readable wires, and both now have a published JSON Schema
generated from the Rust types that produce them:

| Wire | Version key | Schema |
|---|---|---|
| `hotl -p --json` event stream | `schema_version` on every line (now `2`) | [`json-stream-v2.schema.json`](/hotl/schema/json-stream-v2.schema.json) |
| `hotl acp` payloads (results, `session/update` bodies, client requests and replies) | `schemaVersion` (now `1`) | [`acp-v1.schema.json`](/hotl/schema/acp-v1.schema.json) |

Each file's `$id` is its URL. `hotl schema json-stream` and `hotl schema acp`
print the same bytes, so a client can pin the schema of the binary it runs.

## The version rule

An **additive** change — a new frame type, a new optional field — keeps the
version and the same file. A **breaking** change bumps the version and
publishes a new file beside the old one (`json-stream-v3.schema.json`); the old
URL keeps resolving. Optional fields are omitted rather than sent as `null`
unless the schema says `null` is a value.

## The ACP index

`acp-v1.schema.json` is one file with every payload under `$defs`:

| Method or message | `$defs` entry |
|---|---|
| `initialize` result | `InitializeResult` |
| `session/new`, `session/load`, fork result | `SessionOpenResult` |
| `session/prompt` result | `PromptResult` |
| `session/update` params | `SessionUpdateParams` (its `update` is a stream frame or an ACP-only kind) |
| `session/set_plan`, `set_mode`, `set_effort`, `set_model`, `set_goal` results | `PlanAck`, `ModeAck`, `EffortAck`, `ModelAck`, `GoalAck` |
| `session/reload_config` result | `ReloadAck` / `ReloadSessionAck` |
| `session/steer`, `session/fold` results | `Queued`; `session/cancel` → `Cancelled`; stops → `StoppedAck`; the rest → `Ack` |
| server → client `session/request_permission`, `request_question`, `request_egress` | `RequestPermissionParams`, `RequestQuestionParams`, `RequestEgressParams` |
| what the client answers them with | `PermissionReply`, `QuestionReply`, `EgressReply` |

## A dialect, not ACP v1

`hotl acp` is ACP-*shaped*: JSON-RPC over stdio, `session/*` methods,
`session/update` notifications. It is not upstream ACP v1 — its `initialize`
answers `protocolVersion: "0.1"`, prompts take `params.text`, updates carry
`update.type`. The `initialize` result says so in upstream's own extension
slot: `agentCapabilities._meta.hotl` names the `dialect`, the upstream schema
release it was checked against (`acpSchema`), and every method it speaks
(`extensions`). An ACP v1 client such as Zed will not drive it today.
