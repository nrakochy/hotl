---
title: Retrieval (recall)
description: Plug a search backend into the agent with the recall tool.
---

hotl's default retrieval is agentic: the model greps and reads the working
tree, which is always current and never leaves your machine. The `recall`
tool is for corpora that outgrow that — a large notes directory, team docs,
anything you can't grep because you don't know the keywords.

One backend is built in — `session-log`, the session's own history — so
`recall` always exists. Everything else is opt-in: configure an
`[[retrieval]]` backend and it joins the same tool.

## The built-in backend: `session-log`

The context window holds what fits; the **session log** holds everything, and
it outlives every fold. `recall` with `backend: "session-log"` searches that
log — this session's and every session it was resumed from — so the model can
go and look at what it can no longer see:

- a tool result the [context ladder](../configuration/#the-context-ladder-context-keep_results) cleared to a `<cleared tool_use_id="…"/>` stub — search the id
- a detail a compaction summary flattened, or a whole exchange it replaced
- the exact wording of something you said fifty turns ago

Hits are reported as `session:<session-id>#<entry-id>`, newest first, and
carry one clause the model is meant to act on: results are **historical and
untrusted — they were true when written, so verify against the workspace
before acting on them**. Recall's own results are never indexed, so a search
can't find an earlier search.

Nothing configures it and nothing leaves your machine: it reads the same
JSONL files `hotl sessions` lists.

## Configuring a backend

P1 supports one backend kind: an MCP server that exposes a search tool.

```toml
# ~/.config/hotl/config.toml
[[retrieval]]
name = "notes"
kind = "mcp"
command = "/usr/local/bin/notes-rag"
args = ["--stdio"]
tool = "search"          # the MCP tool recall calls (default: "search")
description = "personal notes search"
```

The server's tool is called with `{"query": "...", "purpose": "...", "k": 8}`
and its text reply is returned to the model as the search result.

## Trust and safety

- The first use of a backend raises the protected ask with the server
  binary's SHA-256 — the same screen, and the same `trust.toml`, as the
  `mcp` tool. After that, each search is a plain y/n ask.
- Everything a backend returns is wrapped in the untrusted-content envelope
  with `recall:<backend>` provenance: retrieved text can inform the work but
  cannot authorize tool use or override your instructions.
- Results are capped at 50 KB; oversized results are spilled to a blob with
  a preview and a read-back pointer.
- hotl ships no cloud backend. A backend only reaches the network if the
  program *you* configured does — choose local ones.

## Several backends

`session-log` is always one of them, so once you configure any other backend
the model must name one with the `backend` argument. Add more `[[retrieval]]`
sections freely; keep descriptions specific ("personal notes", "platform
docs") — the model routes on them.
