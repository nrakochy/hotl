---
title: 'Connecting MCP tool servers'
description: Give the hotl agent extra tools from an MCP server over the stdio transport.
---

Give the `hotl` agent extra tools from an MCP server. Assumes you have a working agent (see [quickstart.md](../quickstart/)) and an MCP server program on your machine. For the security model behind the approval prompts, see [permissions-and-sandbox.md](../permissions-and-sandbox/).

## What MCP gives you

An MCP (Model Context Protocol) server is a separate program that exposes tools — documentation search, a database query, a web API. Once configured, the agent can call them through a single `mcp` tool. hotl speaks the **stdio** transport (a server it launches as a child process).

## 1. Declare the server

Create the `[[mcp]]` section of `~/.config/hotl/config.toml`:

```toml
[[mcp]]
name = "docs"                       # how you'll refer to it
command = "/usr/local/bin/docs-mcp" # the server program (absolute path recommended)
args = ["--stdio"]                  # optional launch args
description = "project documentation search"
```

Add one `[[mcp]]` block per server. A malformed file is ignored **whole** (fail-closed) with a warning — no servers load until it parses.

## 2. Verify it's seen

```
hotl doctor
```

There's no dedicated MCP line yet, but a parse error in `config.toml` prints a warning at startup. Start a session and the model will see your servers named in the `mcp` tool's description.

## 3. First use → approve the binary

The **first** time the agent uses a server, you get a protected prompt showing the server name, its binary path, and a SHA-256 of that binary:

```
⚠ PROTECTED PATH — first use of MCP server `docs` (or its binary changed).
binary: /usr/local/bin/docs-mcp
  sha256:…
Approving runs this program on your machine and lets its output into the model's context.
allow mcp: docs.search? [y/N]
```

Approve once and it's remembered (in `~/.config/hotl/trust.toml`). If the binary file **changes**, you're asked again — a changed program is a new trust decision. This prompt can never be auto-approved by an allow-rule.

## What hotl does to server output

Everything a server returns is **sanitized** before the model sees it: terminal escape codes stripped, size capped at 50 KB, and wrapped in an untrusted-content envelope labeled with the server and tool. A poisoned tool description or result can't smuggle instructions to the model or forge its way out of the envelope. Servers run **outside** the bash sandbox (they're programs you installed, not model-directed commands) — which is exactly why the first-use approval shows you the binary and its hash.

## Limits (current)

- stdio transport only (no HTTP/SSE servers yet).
- Tools only — MCP *resources* and *prompts* aren't consumed.
- Sub-agents (`spawn`) get no MCP tools — MCP is top-level only.
