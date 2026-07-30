---
title: 'Troubleshooting — hotl the agent'
description: hotl error messages mapped to causes and fixes; run hotl doctor first.
---

Look up the message you saw. Text in `code` is what hotl prints; find yours by grepping this file for a distinctive phrase. Run `hotl doctor` first for setup problems — it diagnoses most of the table below in one shot.

## Startup / provider

| Message or symptom | Cause | Fix |
|---|---|---|
| `ANTHROPIC_API_KEY is not set.` | Provider is anthropic (the default) but no key. | Set `ANTHROPIC_API_KEY`, or switch to another provider: `export HOTL_MODEL=openai/<model>` with `OPENAI_API_KEY`, or `HOTL_OPENAI_BASE_URL` for a local endpoint. |
| You have a Claude Pro/Max plan and no API key | A subscription covers Claude Code and claude.ai, not third-party tools. | Get a key from the [Claude Console](https://platform.claude.com/) (billed per token), or run a local model. Full answer: [can I use my Claude subscription?](../gateway/#can-i-use-my-claude-pro-or-max-subscription) |
| `OPENAI_API_KEY is not set (required for api.openai.com; …)` | `openai` provider against the default hosted URL, no key. | Set `OPENAI_API_KEY`, or point `HOTL_OPENAI_BASE_URL` at a local server (e.g. `http://localhost:11434/v1`) to run keyless. |
| `unknown provider \`X\` in HOTL_MODEL` | `HOTL_MODEL` isn't `anthropic/…` or `openai/…`. | Use `provider/model`. `openai` covers all OpenAI-compatible endpoints. |
| `doctor` provider line shows `FAIL` | Same as the above three. | Fix the env vars in the shell you'll run `hotl` from, then re-run `hotl doctor`. |
| `WARNING — HOTL_OPENAI_BASE_URL is a non-loopback http:// URL and OPENAI_API_KEY is set` | Your key would cross the network unencrypted. | Use `https://`, an SSH tunnel, or a loopback address. The run proceeds, but the key is exposed. |

## Permissions & sandbox

| Message or symptom | Cause | Fix |
|---|---|---|
| The agent's action was `(denied)` and you never saw a prompt | Headless (`-p`) or non-interactive terminal — asks auto-deny. | Run interactively, or add an allow-rule in `config.toml` for the action the run needs. See [configuration.md](../configuration/#allow-rules-allow). |
| An allow-rule you wrote still prompts | The command has a shell operator, the path escapes the prefix via `..`, the target is a protected path, or (for `bash`) the sandbox isn't enforced. | Expected — these are the carve-outs. See [permissions-and-sandbox.md](../permissions-and-sandbox/). Simplify the command, or approve it by hand. |
| Ask shows `UNSANDBOXED` | No kernel sandbox on this host, or `HOTL_SANDBOX=off`. | On older Linux, none is available; on macOS ensure `/usr/bin/sandbox-exec` exists. `bash` allow-rules are disabled while unsandboxed, by design. |
| `⚠ PROTECTED PATH —` before an ask | The write targets a write-now/execute-later file (git hook, build.rs, ssh, creds, …). | Intended. Approve only if you meant to write that file; it can run code or grant access later. |

## During a turn

| Message or symptom | Cause | Fix |
|---|---|---|
| `stopped — the model kept repeating: …` | Doom-loop guard: the model made the same tool call in a tight cycle. In `ask` mode you declined to continue; in `bypass`/`dontask` it stops on its own (nobody is watching). | Re-prompt with a more specific instruction; the loop usually means the task was ambiguous. |
| `stopped — \`TOOL\` failed too many times in a row.` | A tool failed 5 consecutive times (tool-failure budget). | Check the tool's error output in the transcript; the underlying command or path is wrong. |
| `turn limit reached` / `stopped after N model steps` | The turn spent its `max_turns` budget (default 100 model steps; a tool round-trip costs one). | Raise `[behavior] max_turns` in `config.toml` (or `HOTL_MAX_TURNS`). `-1` removes the cap — the turn then ends only when the model is done, the context fills, or you interrupt. |
| `(context compacted — …)` | Normal: history was summarized to stay within the window. | None. If it happens too early, set `HOTL_CONTEXT_WINDOW` to your model's real window size. |
| `session log is sealed` / `could not create session log` | The session log couldn't be written (permissions, disk). | Check `~/.local/share/hotl/sessions/` is writable (`hotl doctor` reports this). |
| `preapproved rules at … refused` | The admin file isn't root-owned, or is group/world-writable. | `sudo chown root /etc/hotl/preapproved.toml && sudo chmod 644 /etc/hotl/preapproved.toml` |
| `permissions.mode=auto requested, but this is a security-enforced build` | Expected on enforced builds; per-action asks are the build's contract. | None. |

## Minified reads and edits

| Message or symptom | Cause | Fix |
|---|---|---|
| `[minified unavailable for \`X\`: no grammar for this file type…]` | The extension isn't one of the six supported languages. | None — the plain view was served. Expected for Markdown, TOML, shell, and everything else. |
| `[minified unavailable for \`X\`: the file does not parse cleanly…]` | The grammar found a syntax error. Either the file really is broken, or the pinned grammar is older than the syntax the file uses (a new language edition feature). | If the file is valid, the grammar is stale — that's the note's purpose. The plain view was served, so nothing is blocked. |
| `[minified unavailable …: the minifier produced output it could not verify…]` | Self-validation caught its own output: the view failed re-parse, or its structure didn't match the source's. A bug guard firing, not a file problem. | None needed — it degraded to the plain view, which is the designed behavior. Worth reporting with the file. |
| `[minified unavailable …: the minified view is N bytes, over the … cap]` | Minified reads are whole-file or nothing, and this file exceeds 200KB even minified. | None — the plain paged read was served. Use `offset`/`limit` on it as usual. |
| `minified reads return the whole file, so \`offset\`/\`limit\` do not apply` | The model passed both `minified: true` and a paging argument. | Expected, and self-correcting: the error names the plain read. The minified view has no line numbers to page by. |
| `\`old_string\` was not found in the minified view of \`X\`` | Almost always: the text was quoted from a **plain** read, whose whitespace differs. | Re-read with `minified: true` and copy from that view — or drop `minified` from the edit. |
| `the matched text … is only formatting the minifier inserted` | `old_string` covered only separators the minifier synthesized, which exist nowhere in the file. | Include a real token in `old_string`. |
| `multi-line replacements can corrupt python indentation through the minified view` | A `new_string` containing a newline, in a language where indentation *is* syntax. | Use a plain edit (omit `minified`) for that change. Deliberate refusal, not a limitation to work around. |
| `this edit would leave \`X\` no longer parsing; nothing was written` | The projected splice would break the file. Caught before the write. | Re-check `old_string`/`new_string` against a fresh minified read. The file is untouched. |
| `this build has no minify support` | A `--no-default-features` build (no C toolchain). The `minified` argument isn't in the tool schema for such builds, so a model shouldn't reach this. | Re-issue without `minified`, or use a default build. |
| Savings look smaller than expected | Comment-light or small files save 10–18%; `keep_comments = true` (the default) is the conservative mode; JSX-heavy `.tsx` saves only on its non-JSX portion. | Set `[minify] keep_comments = false` for 44–59%, accepting that the model reads code without the *why*. The per-read trailer always reports the real figure. |

## MCP servers

| Message or symptom | Cause | Fix |
|---|---|---|
| `config.toml ignored (parse error)` | Malformed `config.toml`. | Fix the TOML; a bad file is ignored wholesale (fail-closed), so no servers load until it parses. |
| First `mcp` use shows a `PROTECTED PATH`-style screen with a hash | First use of that server (or its binary changed). | Expected — approving runs that binary and lets its output into context. Verify the path/hash, then approve. |
| MCP call returns `… timed out after 30s` | The server didn't respond. | Check the server runs standalone; hotl won't hang on it. |

## Resume & undo

| Message or symptom | Cause | Fix |
|---|---|---|
| `no session starts with \`X\`` | No session id has that prefix. | Run bare `hotl resume` to list ids, then use a longer prefix. |
| `WARNING — … broken parent_id chain …` on resume | The session log was edited or truncated after it was written. | The context is still loaded, but treat it as untrusted — a broken chain means tampering or corruption. |
| `hotl undo`: `no shadow snapshots found` | git wasn't available when the session ran, so nothing was snapshotted. | Install `git`; `hotl doctor` warns when snapshots are disabled. |
| `hotl undo` didn't remove a file the agent created | By design: undo restores tracked files but never deletes new ones. | Delete the unwanted new file by hand; undo lists what it changed. |

**Report a bug hotl mislabels or a fix that's wrong:** the harness treats a repeated failure as a docs/behavior bug — file it against the repo. **Not covered here:** live-provider quirks — no real model has driven hotl end to end yet, so novel model behavior is expected and worth reporting.
