---
title: 'Plugins — the Agent Plugins package format'
description: hotl's primary extension package — one directory bundling skills and MCP servers under a manifest, per the Agent Plugins 1.0.0 spec — installation, precedence, PLUGIN_ROOT/PLUGIN_DATA, trust, and failure boundaries.
---

A plugin is one directory that bundles skills and MCP servers under a
versioned manifest, in the portable [Agent Plugins 1.0.0
format](https://github.com/agentplugins/agent-plugins-spec). It is the
primary way to package extensions for hotl: a plugin written for any
conformant client works here unchanged, and a plugin built against hotl
works elsewhere. hotl adopts the *package format* — there is no plugin
marketplace, registry, or auto-update, and discovery never touches the
network.

## The package shape

```text
my-plugin/
├── plugin.json          # required manifest: $schema + name (+ metadata)
├── skills/
│   └── summarize/
│       └── SKILL.md     # one skill per immediate child directory
├── mcp.json             # optional MCP servers (stdio)
└── com.example.client/  # client extension dirs — ignored by hotl
```

`plugin.json` needs two fields; everything else is optional metadata:

```json
{
  "$schema": "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json",
  "name": "my-plugin",
  "version": "1.0.0",
  "description": "What this plugin provides"
}
```

The manifest `name` (lowercase letters, digits, `.` and `-`) is the
plugin's identity everywhere: the skill qualifier, the MCP server prefix,
and its data-directory key.

## Installing

```text
hotl plugins add team https://github.com/acme/team-plugin.git
hotl plugins add local ~/work/my-plugin
hotl plugins list
hotl plugins update [handle]
hotl plugins remove <handle>
```

`add` writes `[plugins.sources]` in config.toml — a git URL clones under
`~/.config/hotl/plugins/<handle>`, a local path is read in place — then
immediately loads the fresh checkout and prints what it found, load
reports included, so a broken plugin is visible at install time rather
than at first run. Git runs only on `add` and `update`.

```toml
[plugins.sources]
team = "https://github.com/acme/team-plugin.git"   # managed checkout
local = "~/work/my-plugin"                          # read in place
```

The key is just the install handle; the manifest `name` is the identity
(a mismatch warns). A handle containing dots must be quoted
(`"acme.tools" = …`), or TOML reads it as a nested table.

`remove` deletes a managed checkout but **always preserves** the
plugin's data directory, printing where it lives — server state survives
a reinstall.

## Skills from plugins

Each immediate child of `skills/` containing a `SKILL.md` is one skill.
Bare names resolve by precedence:

1. your own flat skills (`~/.config/hotl/skills/`)
2. **plugins**
3. `[skills.marketplaces]`
4. your Claude Code skills
5. the Claude Code plugin cache

Your loose skills always win a name tie, and plugins outrank every other
package lane. A plugin skill is always *also* addressable as
`<plugin>:<skill>` — the only form when its bare name is taken. Trust is
unchanged from ordinary skills: loaded bodies arrive in an untrusted
envelope; a skill instructs, it never authorizes.

## MCP servers from plugins

`mcp.json` declares servers in the spec's closed format:

```json
{
  "$schema": "https://agent-plugins.org/schemas/1.0.0/mcp.schema.json",
  "mcpServers": {
    "validator": {
      "type": "stdio",
      "command": "./bin/validator",
      "args": ["--data", "${PLUGIN_DATA}/validator"],
      "env": {"CONFIG": "${PLUGIN_ROOT}/config.json"},
      "cwd": "${PLUGIN_ROOT}"
    }
  }
}
```

Servers register as `<plugin>:<server>` beside your `[[mcp]]` entries
(whose names cannot contain `:`, so nothing collides) and flow through
the same first-use trust screen, listing, and sandbox posture as any
other server. `hotl mcp list` and the in-session roster always agree —
both read one composition.

hotl connects over **stdio only**. A valid `streamable-http` or `sse`
entry is fully validated, then skipped with a `skipped — unsupported
transport` report in `hotl plugins list`; that wording (as opposed to
`entry invalid`) means the entry is well-formed and would load in a
client with remote transport support.

Two placeholders — `${PLUGIN_ROOT}` (the plugin directory) and
`${PLUGIN_DATA}` (a per-plugin writable state directory under
`~/.local/share/hotl/plugin-data/<name>`, preserved across updates) —
expand in `args`, `env` values, and `cwd`. Expansion is a single
non-recursive pass; anything else (`${HOME}`, `$VAR`) stays literal, and
`command` never expands: it is a bare executable name or a
`./`-relative path inside the plugin.

### Trust: env and cwd are screened

A configured `env` is a code-execution channel through an
already-trusted binary (`NODE_OPTIONS`, `PYTHONPATH`, `LD_PRELOAD`), so
a server's env and cwd are part of its trust fingerprint and render on
the approval screen. This shipped as a new `fp3:` fingerprint scheme —
**existing MCP trust grants re-screen once** after updating; approve the
same server again and the new grant sticks.

## Failure boundaries

Failures are contained to the narrowest unit, and every failure is
reported in `hotl plugins list` (and as a startup warning), never
silently swallowed:

| What breaks | What happens |
|---|---|
| `plugin.json` invalid (or escapes the plugin dir) | the whole plugin is rejected |
| an unknown manifest field / non-object `extensions` | reported and ignored — the plugin still loads |
| one `SKILL.md` broken or escaping | that skill is skipped; siblings load |
| `mcp.json` malformed or wrong version | MCP is disabled for that plugin; skills still load |
| one server entry invalid | that entry is skipped; siblings load |

Symlinks inside a plugin may point anywhere *within* the plugin
directory; anything resolving outside it is rejected at the same
boundaries.

The `io.github.nrakochy.hotl` extension namespace is reserved for
future hotl-specific plugin data (hooks, agent definitions); hotl
assigns it no behavior today, and other clients ignore it by spec.
