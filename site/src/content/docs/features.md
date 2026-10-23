---
title: 'Feature stages'
description: "Every hotl feature switch — environment variables and the newer config keys — with its stage (experimental, beta, stable, deprecated) and the release that introduced it."
---

hotl labels every user-facing switch with a stage. `hotl features` prints this
table; `hotl doctor` warns about each non-stable switch you have turned on.

| Stage | What it promises |
|---|---|
| **experimental** | May change or go without notice. Sandbox and egress escape hatches are labelled in every ask. |
| **beta** | Shipped and used; the shape may still move, and it will get a config twin. |
| **stable** | Covered by the distribution contract: a breaking change waits for a minor release and is in the changelog. |
| **deprecated** | Still works; the replacement is named; removal is announced one minor release ahead. |

Being listed changes nothing about how a switch behaves. `0.29.0` means the
next release.

<!-- features:begin -->
| Switch | Stage | Since | What |
|---|---|---|---|
| `HOTL_MODEL` | stable | 0.2.0 | the session model (`provider/model`) |
| `HOTL_PROVIDER_BASE_URL` | stable | 0.12.0 | the active provider's base URL |
| `HOTL_PROVIDER_AUTH` | stable | 0.4.0 | `api_key` or `subscription` |
| `HOTL_API_KEY_HELPER` | stable | 0.2.0 | a command whose stdout is the API key |
| `HOTL_API_KEY_HELPER_TTL_SECS` | stable | 0.2.0 | re-run the key helper after this long |
| `HOTL_CONTEXT_WINDOW` | stable | 0.2.0 | override the catalogued window |
| `HOTL_FAST_MODEL` | stable | 0.2.0 | housekeeping model |
| `HOTL_UTILITY_MODEL` | stable | 0.26.0 | model for digests and goal checks |
| `HOTL_EFFORT` | stable | 0.12.0 | reasoning depth |
| `HOTL_EVICT_TOKENS` | stable | 0.2.0 | spill tool results larger than this |
| `HOTL_PERMISSIONS` | stable | 0.2.0 | `ask` | `bypass` | `dontask` |
| `HOTL_PLAN` | stable | 0.9.0 | start in plan mode |
| `HOTL_SANDBOX` | stable | 0.2.0 | `off` disables the bash floor |
| `HOTL_MAX_TURNS` | stable | 0.5.1 | model steps per prompt |
| `HOTL_MAX_TOOL_CALLS` | stable | 0.26.0 | tool calls per session |
| `HOTL_MAX_COST_USD` | stable | 0.26.0 | USD per session |
| `HOTL_MAX_TOKENS` | stable | 0.14.0 | per-sample output tokens |
| `HOTL_CONCURRENCY_REQUESTS` | stable | 0.5.0 | concurrent provider requests |
| `HOTL_CONCURRENCY_AGENTS` | stable | 0.5.0 | concurrent sub-agents |
| `HOTL_CONCURRENCY_BLOCKING_THREADS` | stable | 0.5.0 | blocking-pool size |
| `HOTL_MOUSE` | stable | 0.5.2 | mouse capture in the console |
| `HOTL_SCRUB_ENV` | stable | 0.5.2 | extra env names scrubbed from children |
| `HOTL_SANDBOX_PROBE_DIR` | stable | 0.5.2 | where the sandbox probe writes |
| `HOTL_SHELL` | stable | 0.12.0 | the POSIX shell `bash` runs |
| `HOTL_PREAPPROVED` | stable | 0.2.0 | admin preapproved-rules file |
| `HOTL_THINKING` | beta | 0.5.2 | extended thinking on/off (config key pending) |
| `HOTL_SCRUB_ENV_STRICT` | beta | 0.5.2 | heuristic scrub of secret-looking env |
| `HOTL_COMPACTION_RESET` | beta | 0.2.0 | fresh-slate folds |
| `HOTL_HIDE_CONTEXT_PCT` | beta | 0.2.0 | keep fullness out of the turn context |
| `HOTL_PROGRAM_STATUS` | beta | 0.29.0 | terminal progress/status sequences |
| `HOTL_UNIX_SOCKETS` | experimental | 0.5.2 | let sandboxed bash reach unix sockets |
| `HOTL_MACOS_AUTOMATION` | experimental | 0.5.2 | let sandboxed bash drive Apple Events |
| `HOTL_WEB_ALLOW_METADATA` | experimental | 0.5.2 | let web_fetch reach metadata addresses |
| `HOTL_PROXY_AUTH` | experimental | 0.5.2 | egress proxy credential mode |
| `HOTL_CONCURRENCY_SUBPROCS` | experimental | 0.5.0 | reserved; parsed but not wired |
| `HOTL_CONCURRENCY_WORKER_THREADS` | experimental | 0.5.0 | reserved; parsed but not wired |
| `HOTL_SHELL_TRACE` | experimental | 0.29.0 | zsh plugin: attach the last command to `: ` |
| `HOTL_TRACE_LINES` | experimental | 0.29.0 | zsh plugin: lines of output captured |
| `HOTL_TRACE_MAX_AGE` | experimental | 0.29.0 | zsh plugin: oldest trace attached |
| `HOTL_PROMPT_LINES` | experimental | 0.29.0 | zsh plugin: rows your prompt takes |
| `HOTL_OPENAI_BASE_URL` | deprecated | 0.2.0 | alias of the provider base URL — use `HOTL_PROVIDER_BASE_URL` |
| `HOTL_ANTHROPIC_BASE_URL` | deprecated | 0.4.0 | alias of the provider base URL — use `HOTL_PROVIDER_BASE_URL` |
| `HOTL_CACHE_WARMING / [provider.cache_warming]` | experimental | 0.29.0 | keep the 5-minute prompt cache alive while a tool runs |
| `[provider.cache_breakpoints]` | experimental | 0.24.0 | explicit GPT-5.6 prompt-cache breakpoints |
| `[agents.max_depth]` | experimental | 0.29.0 | how deep spawned agents may nest |
| `[agents.max_children]` | experimental | 0.29.0 | running plus parked sub-agents |
| `[behavior.max_jobs]` | experimental | 0.29.0 | running background jobs per session |
<!-- features:end -->
