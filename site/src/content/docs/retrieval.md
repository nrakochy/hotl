---
title: Retrieval (recall)
description: Plug a search backend into the agent with the recall tool.
---

hotl's default retrieval is agentic: the model greps and reads the working
tree, which is always current and never leaves your machine. The `recall`
tool is for corpora that outgrow that — a large notes directory, team docs,
anything you can't grep because you don't know the keywords.

Nothing is on by default. Configure a backend and the model gains one
`recall` tool; configure none and the tool doesn't exist.

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

Add more `[[retrieval]]` sections; the model then picks with the `backend`
argument. Keep descriptions specific ("personal notes", "platform docs") —
the model routes on them.
