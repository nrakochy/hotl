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

`hotl mcp add <name> <command> [args...]` prints a correctly-shaped block for you to paste. It deliberately does not write `config.toml` itself — see [Managing servers](#managing-servers).

## 2. Verify it's seen

```
hotl mcp
```

lists every configured server with its command line and what the trust gate will do with it:

```
docs    npx -y @acme/docs-mcp   screens on first use
local   node ./tools/srv.js     session-only (workspace script — never persisted)
stale   python -m oldpkg        unreadable binary — will ask every time
```

`hotl doctor` reports the same thing as a summary line, and warns about a corrupt `trust.toml` or a grant whose server has left the config.

## 3. First use → approve the program

The **first** time the agent uses a server, you get a protected prompt showing the server name and everything that decides what program runs — the binary, its arguments, a SHA-256 of the binary, and a SHA-256 of any argument that resolves to a local file (the script an interpreter actually runs):

```
⚠ PROTECTED PATH — first use of MCP server `docs` (or its program changed).
binary: /usr/local/bin/node
args: ["/opt/docs-mcp/server.js"]
  sha256:…
  /opt/docs-mcp/server.js: sha256:…
Approving runs this program on your machine and lets its output into the model's context.
allow mcp: docs.search? [y/N]
```

Approve once and it's remembered (in `~/.config/hotl/trust.toml`). If **anything** in that fingerprint changes — the binary, the args, or the script's contents — you're asked again; a changed program is a new trust decision. This prompt can never be auto-approved by an allow-rule.

`hotl mcp show <name>` prints the same text on demand, without starting anything.

## Managing servers

| Command | Effect |
|---|---|
| `hotl mcp [list]` | Roster + trust state, plus warnings about stale grants and a corrupt `trust.toml`. |
| `hotl mcp show <name>` | The fingerprint text above, plus the recorded key. Never starts the server. |
| `hotl mcp add <name> <command> [args...]` | Prints a `[[mcp]]` block to paste. **Writes nothing.** |
| `hotl mcp untrust <name>\|--all` | Drops the grant; the server is screened again on next use. |
| `hotl mcp test <name>` | Starts the server, handshakes, lists its tools. Screens first if untrusted; records nothing. |

Two things this command deliberately cannot do:

- **It never writes `config.toml`.** A CLI that edited config would be a path `bash -c 'hotl mcp add …'` could take, and hotl's bash analysis reads redirects, `tee`, and `dd` — not a program that writes config as a side effect.
- **It cannot grant trust.** Trust is recorded only by a human answering the screen above; a CLI grant would be an "always allow" by another name. `untrust` is the one mutation, because revocation only ever *reduces* privilege.

## What hotl does to server output

Everything a server returns is **sanitized** before the model sees it: terminal escape codes stripped, size capped at 50 KB, and wrapped in an untrusted-content envelope labeled with the server and tool. A poisoned tool description or result can't smuggle instructions to the model or forge its way out of the envelope. Servers run **outside** the bash sandbox (they're programs you installed, not model-directed commands) — which is exactly why the first-use approval shows you the binary and its hash.

## Limits (current)

- stdio transport only (no HTTP/SSE servers yet).
- Tools only — MCP *resources* and *prompts* aren't consumed.
- Sub-agents (`spawn`) get no MCP tools — MCP is top-level only.
