---
title: 'Connecting MCP tool servers'
description: Give the hotl agent extra tools from an MCP server over the stdio transport.
---

Give the `hotl` agent extra tools from an MCP server. Assumes you have a working agent (see [quickstart.md](../quickstart/)) and an MCP server program on your machine. For the security model behind the approval prompts, see [permissions-and-sandbox.md](../permissions-and-sandbox/).

## What MCP gives you

An MCP (Model Context Protocol) server is a separate program that exposes tools — documentation search, a database query, a web API. Once configured, the agent can call them through a single `mcp` tool. hotl speaks the **stdio** transport (a server it launches as a child process).

Servers can also arrive bundled inside an [Agent Plugin](../plugins/) — they register as `<plugin>:<server>` beside your `[[mcp]]` entries and follow everything on this page (the roster, the trust screen, `hotl mcp test`) unchanged.

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

Two optional keys shape the server's process: `env = [["KEY", "value"], …]` overlays the inherited environment in order (a later duplicate wins), and `cwd = "/some/dir"` sets its working directory. Both are part of the trust fingerprint — a configured env can reach code execution through variables like `NODE_OPTIONS`, so changing either re-raises the approval screen exactly like changing `args`.

`hotl mcp add <name> <command> [args...]` prints a correctly-shaped block for you to paste. It deliberately does not write `config.toml` itself — see [Managing servers](#managing-servers).

### Remote servers (URL)

A server can be an address instead of a program. Name a `url` and no `command`
— exactly one of the two:

```toml
[[mcp]]
name = "figma"
url = "http://127.0.0.1:3845/mcp"          # Figma desktop app, Dev Mode on; plain http = loopback only
description = "Figma designs: frames, components, variables"

[[mcp]]
name = "github"
url = "https://api.githubcopilot.com/mcp/"
bearer_token_env = "GITHUB_MCP_TOKEN"       # the NAME of an env var; the value is read at connect time
description = "GitHub issues, PRs, code search"

[[mcp]]
name = "tracker"
url = "https://mcp.example.com/mcp"
headers = [["X-Team", "core"]]              # literal, non-secret headers only
```

hotl speaks MCP's **Streamable HTTP** transport: one POST per message,
JSON or event-stream replies, the server's session id carried on every
later request. SSE-only servers are not supported (the ecosystem has
deprecated them). The URL must be absolute `https://`; plain `http://` is
accepted only for `localhost` and loopback addresses, and a literal private
or cloud-metadata address is refused at load. hotl never follows a redirect
from an MCP server — a 3xx is an error naming where it pointed.

**The token is a name, not a value.** `bearer_token_env` names an
environment variable; hotl reads it when it connects and sends it as
`Authorization: Bearer …`. The value never goes in `config.toml`, the trust
file, the transcript, an error or the screen. If the variable is unset the
connect is refused with the variable named; a `401` is refused the same way
(no silent retry). A literal `Authorization` header in `headers` is a config
error pointing you at `bearer_token_env`. Command helpers (`op read …`) are
not yet a token source.

**The first-use screen shows where tool calls go.** For a URL server the
approval text reads `hotl will send tool calls to https://api.githubcopilot.com
with a token from $GITHUB_MCP_TOKEN.`, then the URL and the header *names*
with each value masked after four characters. The grant is keyed on the
address, the headers and the token's variable name (`fpu1:` in
`trust.toml`), so changing any of them asks again; the token's value is not
part of the key, so rotating it does not.

**Egress.** Remote MCP traffic leaves from hotl's own process, so the bash
proxy never sees it. The connector therefore makes the same two checks
`web_fetch` makes before opening a socket: under `[network] egress =
"allowlist"` a host not on the list raises the one-per-host session ask,
and under `off` it is refused with the policy named. The default `open`
passes — the first-use screen that names the origin is the decision.

**What the server says about itself stays out of the prompt.** The
`initialize` reply's `instructions` field is dropped at the transport; tool
listings and results pass the sanitizer exactly as for a program.

`hotl mcp add <name> --url <url> [--bearer-env VAR] [--header NAME=VALUE]…`
prints the block.

#### Figma

The desktop app serves MCP at `http://127.0.0.1:3845/mcp` with Dev Mode on
and no login — the first block above is all it takes. Figma's *remote* server
(`mcp.figma.com`) only admits clients in Figma's own catalog, which hotl is
not in, so there is no configuration that reaches it yet; that is the OAuth
login phase of plan 0063, which waits on a server that accepts a client it
has not pre-approved.

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
| `hotl mcp [list]` | Roster + trust state (a URL server shows its address and `token: $VAR (set\|unset)`), plus warnings about an invalid `[[mcp]]` entry, stale grants and a corrupt `trust.toml`. |
| `hotl mcp show <name>` | The fingerprint text above, plus the recorded key. Never starts the server. |
| `hotl mcp add <name> <command> [args...]` | Prints a `[[mcp]]` block to paste. **Writes nothing.** |
| `hotl mcp add <name> --url <url> [--bearer-env VAR] [--header NAME=VALUE]…` | The same, for a remote server. A literal `Authorization` header is refused. |
| `hotl mcp untrust <name>\|--all` | Drops the grant; the server is screened again on next use. |
| `hotl mcp test <name>` | Starts the server (or connects to the URL, through the egress check and with the token), handshakes, lists its tools. Screens first if untrusted; records nothing. |

Two things this command deliberately cannot do:

- **It never writes `config.toml`.** A CLI that edited config would be a path `bash -c 'hotl mcp add …'` could take, and hotl's bash analysis reads redirects, `tee`, and `dd` — not a program that writes config as a side effect.
- **It cannot grant trust.** Trust is recorded only by a human answering the screen above; a CLI grant would be an "always allow" by another name. `untrust` is the one mutation, because revocation only ever *reduces* privilege.

## What hotl does to server output

Everything a server returns is **sanitized** before the model sees it: terminal escape codes stripped, size capped at 50 KB, and wrapped in an untrusted-content envelope labeled with the server and tool. A poisoned tool description or result can't smuggle instructions to the model or forge its way out of the envelope. Servers run **outside** the bash sandbox (they're programs you installed, not model-directed commands) — which is exactly why the first-use approval shows you the binary and its hash.

## Limits (current)

- Transports: stdio and Streamable HTTP. SSE-only servers are not supported. OAuth login (a server that wants a browser sign-in) is not yet shipped — phase 2 of plan 0063; a bearer token from the environment covers GitHub, Context7, gateway `/mcp` endpoints and the Figma desktop app today.
- Tools only — MCP *resources* and *prompts* aren't consumed.
- Sub-agents (`spawn`) get no MCP tools — MCP is top-level only.
