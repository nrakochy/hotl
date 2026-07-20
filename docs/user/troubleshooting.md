# Troubleshooting — `hotl` the agent

**Mode: reference (error → cause → fix).** Look up the message you saw. Text in `code` is what hotl prints; find yours by grepping this file for a distinctive phrase. Run `hotl doctor` first for setup problems — it diagnoses most of the table below in one shot.

## Startup / provider

| Message or symptom | Cause | Fix |
|---|---|---|
| `ANTHROPIC_API_KEY is not set.` | Provider is anthropic (the default) but no key. | Set `ANTHROPIC_API_KEY`, or switch to another provider: `export HOTL_MODEL=openai/<model>` with `OPENAI_API_KEY`, or `HOTL_OPENAI_BASE_URL` for a local endpoint. |
| `OPENAI_API_KEY is not set (required for api.openai.com; …)` | `openai` provider against the default hosted URL, no key. | Set `OPENAI_API_KEY`, or point `HOTL_OPENAI_BASE_URL` at a local server (e.g. `http://localhost:11434/v1`) to run keyless. |
| `unknown provider \`X\` in HOTL_MODEL` | `HOTL_MODEL` isn't `anthropic/…` or `openai/…`. | Use `provider/model`. `openai` covers all OpenAI-compatible endpoints. |
| `doctor` provider line shows `FAIL` | Same as the above three. | Fix the env vars in the shell you'll run `hotl` from, then re-run `hotl doctor`. |
| `WARNING — HOTL_OPENAI_BASE_URL is a non-loopback http:// URL and OPENAI_API_KEY is set` | Your key would cross the network unencrypted. | Use `https://`, an SSH tunnel, or a loopback address. The run proceeds, but the key is exposed. |

## Permissions & sandbox

| Message or symptom | Cause | Fix |
|---|---|---|
| The agent's action was `(denied)` and you never saw a prompt | Headless (`-p`) or non-interactive terminal — asks auto-deny. | Run interactively, or add an allow-rule in `config.toml` for the action the run needs. See [configuration.md](configuration.md#allow-rules-allow). |
| An allow-rule you wrote still prompts | The command has a shell operator, the path escapes the prefix via `..`, the target is a protected path, or (for `bash`) the sandbox isn't enforced. | Expected — these are the carve-outs. See [permissions-and-sandbox.md](permissions-and-sandbox.md). Simplify the command, or approve it by hand. |
| Ask shows `UNSANDBOXED` | No kernel sandbox on this host, or `HOTL_SANDBOX=off`. | On older Linux, none is available; on macOS ensure `/usr/bin/sandbox-exec` exists. `bash` allow-rules are disabled while unsandboxed, by design. |
| `⚠ PROTECTED PATH —` before an ask | The write targets a write-now/execute-later file (git hook, build.rs, ssh, creds, …). | Intended. Approve only if you meant to write that file; it can run code or grant access later. |

## During a turn

| Message or symptom | Cause | Fix |
|---|---|---|
| `stopped — the model kept repeating: …` | Doom-loop guard: the model made the same tool call in a tight cycle and you declined to continue. | Re-prompt with a more specific instruction; the loop usually means the task was ambiguous. |
| `stopped — \`TOOL\` failed too many times in a row.` | A tool failed 5 consecutive times (tool-failure budget). | Check the tool's error output in the transcript; the underlying command or path is wrong. |
| `stopped at max_turns` | The turn hit the 25-step cap. | Break the task into smaller prompts. |
| `(context compacted — …)` | Normal: history was summarized to stay within the window. | None. If it happens too early, set `HOTL_CONTEXT_WINDOW` to your model's real window size. |
| `session log is sealed` / `could not create session log` | The session log couldn't be written (permissions, disk). | Check `~/.local/share/hotl/sessions/` is writable (`hotl doctor` reports this). |

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

**Report a bug hotl mislabels or a fix that's wrong:** the harness treats a repeated failure as a docs/behavior bug (specs core belief). File it against the repo. **Not covered here:** live-provider quirks — no real model has driven hotl end to end yet, so novel model behavior is expected and worth reporting.
