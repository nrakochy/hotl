# hotl — human on the loop

[![crates.io](https://img.shields.io/crates/v/hotl.svg)](https://crates.io/crates/hotl)

Running one agent is easy. Running several, all day, is a supervision
problem: knowing which one is blocked on you, trusting what they're allowed
to do, and recovering when one goes sideways. hotl is one static binary — no
Node, no Python, no daemon, no telemetry — that takes that problem in three
stages, with you on the loop at every stage:

| Capability | Command | Status |
|---|---|---|
| **Execute** | `hotl` | **Shipped** — a personal agent harness (event-log-as-canon, ACP-native): steering console TUI + `-p` headless, gated tools under a kernel sandbox floor, managed context, skills, sub-agents, MCP, session resume + `undo`. Any OpenAI-compatible or Anthropic model. **[User docs → nrakochy.github.io/hotl](https://nrakochy.github.io/hotl/)** |
| **Watch** | `hotl watch` | **Shipped** — a tmux dashboard that discovers your AI-agent processes, shows live status, pings when one is blocked on you, and jumps focus to it |
| **Orchestrate** | `hotl fleet` | **Reserved** — will drive fleets of agents over the same protocol any editor uses; exits 2 today, only its seams exist |

> **Pre-1.0:** bare `hotl` is the **agent**; the tmux dashboard is
> `hotl watch`. Expect breaking changes at every 0.x minor — see
> [CHANGELOG.md](CHANGELOG.md). The internal library crates publish in
> lockstep with the binary and carry no semver promise of their own.

## Why hotl

**Stay in charge without babysitting.** Agents earn their keep on long runs,
but long runs block on you at unpredictable moments — and the usual answer is
cycling through panes to check. `hotl watch` replaces that with a dashboard:
it discovers every agent across your tmux session, shows who's working and
who's waiting, pings when one needs you, and `enter` jumps focus straight to
it. Your attention goes where it's actually needed.

**A safety floor under a gate you choose.** Three permission modes set how
much you're asked: `bypass` (default — ordinary calls run without prompting),
`ask` (approve every mutating or executing call), and `dontask` (never wait
for input; deny anything not pre-approved — the CI posture). An unrecognized
mode fails closed to `ask`. Plan mode is a separate toggle that composes with
any of them: `/plan` makes `write` and `edit` always ask while leaving the
shell and the network to the mode, so the agent can research a change without
being able to casually make one. Underneath, regardless of mode: `bash` (and hooks, and diagnostics)
runs confined by the kernel — Seatbelt on macOS, Landlock on Linux ≥ 6.2
including WSL2 (native Windows builds and runs, but its floor is not yet
certified — every exec is individually gated there) — with writes limited to
the working directory, temp, and any
`[sandbox].writable` directories the owner listed (never widenable from
inside: entries exposing hotl's own config are refused);
writes to execute-later and credential paths (git hooks, shell rc, Makefiles,
`.ssh/`, credential stores, agent-instruction files) always prompt, and are
checked *before* allow-rules; and every silenced prompt stays visible in the
transcript. Where the floor can't be enforced, hotl degrades **fail-closed** —
each exec is individually gated behind an `UNSANDBOXED` banner and bash
allow-rules stop applying. `HOTL_SANDBOX=off` disables it loudly, never
quietly. Need the gate guaranteed? Build with `--features security-enforced`
and the mode key is ignored entirely. This is write-confinement, not
exfiltration prevention — the stance is written down honestly, including what
it does **not** cover: [`docs/SECURITY.md`](docs/SECURITY.md).

**Egress you can close.** `[network] egress` is `open` (default), `off`
(loopback and unix sockets only), or `allowlist` (loopback plus
`[network].allow` hosts, with `*.domain` wildcards). Allowed hosts are reached
through a small local proxy, and tools that ignore proxy env vars don't slip
past it — they hit the kernel's loopback-only wall and fail. An unknown value
fails closed to `off`, and `web_fetch`/`web_search` honor that same single
authority — there is no second allowlist to keep in sync. Three honest limits:
it's opt-in, only HTTP traverses the proxy (SSH git remotes fail while
restricted), and it is not airtight — an allowed host is allowed for
*everything*, and DNS still resolves, so treat it as a strong brake on casual
exfiltration, not a cleanroom. On Linux, egress confinement needs kernel
≥ 6.7 (TCP only); where it can't be enforced you get `NET:UNENFORCED(reason)`
on every bash ask and allow-rules stop auto-approving.

**Nothing is ever lost.** Resume any session, `undo` the agent's file
changes, steer mid-turn without losing the thread. This works because every
session is recorded as an append-only log that nothing rewrites — even
context compaction adds a summary on top instead of destroying history, so
a failed compaction can't brick a session. Secrets are masked at log write
time, and secret-bearing files never enter the snapshot store.

**Context stays slim by construction.** Tool results past a size threshold
are evicted to files (`[context].evict_tokens`) — a preview stays inline and
the agent pages the full result back on demand. Compaction summaries are
precomputed in the background from ~60% context-full and fold in at ~80%, so
the fold never pauses the session. The prompt prefix is byte-stable, keeping
provider prompt caches hot.

**Untrusted content stays data.** Anything returned by a sub-agent, `recall`,
`web_fetch`/`web_search`, or an MCP tool arrives inside a provenance-tagged
envelope — material that can inform the work, never an instruction the model
may act on unprompted.

**Standard protocols, any model.** Anthropic (Messages API) or any
OpenAI-compatible endpoint — OpenAI, Groq, Ollama, LiteLLM, a local server;
it's just a base URL. MCP for tools, ACP for embedding in editors — the same
contract the future `hotl fleet` orchestrator will speak.

## Install

Prebuilt binary — no toolchain needed (macOS / Linux):

    curl --proto '=https' --tlsv1.2 -LsSf https://github.com/nrakochy/hotl/releases/latest/download/hotl-installer.sh | sh

Or with Rust ≥ 1.88 installed:

    cargo install hotl

With Nix (flakes enabled) — run it without installing, or add it to a profile:

    nix run github:nrakochy/hotl
    nix profile install github:nrakochy/hotl

The flake tracks `master`; your `flake.lock` decides when that moves. As a
flake input:

    inputs.hotl = {
      url = "github:nrakochy/hotl";
      inputs.nixpkgs.follows = "nixpkgs";   # avoids a second nixpkgs, and a second rustc
    };

Nix builds hotl from source, so the first build compiles the whole dependency
tree — use the prebuilt binary above if you want it now.

To upgrade later, `hotl update` installs the latest release — replacing the
binary in place if you used the installer script, and printing the right
command if cargo, Nix, or Homebrew owns it. hotl checks for a release only when
you run that command. See [updating](https://nrakochy.github.io/hotl/updating/).

## Execute — quick start

Point `HOTL_MODEL` at a model (`provider/model` — `anthropic/…` or `openai/…`, which covers any OpenAI-compatible endpoint incl. local Ollama), then:

    hotl doctor    # provider, sandbox floor, config, sessions — all should read ok
    hotl           # interactive console TUI
    hotl -p "fix the typo in main.rs"   # headless one-shot

Keys never live in `config.toml`: use env vars, or an `api_key_helper`
command whose stdout is the key. Precedence is always env var > config.toml >
default.

Full tutorial: [quickstart](https://nrakochy.github.io/hotl/quickstart/).

### Commands

| Command | What it does |
|---|---|
| `hotl` | Console TUI; `-p "<prompt>"` headless, `--json` for a JSONL event stream |
| `hotl resume` / `hotl undo` | Continue a session; reverse the agent's file edits |
| `hotl bg` / `hotl attach` | Run a session detached from any terminal, reconnect later |
| `hotl acp` | Serve ACP over stdio so an ACP-speaking editor can embed the agent |
| `hotl skills` | Manage skills and skill marketplaces |
| `hotl mcp` | List MCP servers and their trust state; screen one, revoke a grant |
| `hotl doctor` / `hotl setup` | Setup check (nonzero on failure); write a commented starter config |
| `hotl gc` | Prune sessions and snapshots per `[retention]` |
| `hotl watch` | The tmux supervision dashboard |

Exit codes: `0` turn completed · `130` interrupted · `1` any other outcome
(error, refusal, turn-limit, doom-loop, tool-failure budget, a `doctor` FAIL)
· `2` bad usage or a reserved subcommand.

### Extending it

| Extension point | Shape | Docs |
|---|---|---|
| **Skills** | `skills/*.md` procedures plus `hotl skills` marketplaces. Indexed, never preloaded — the agent sees a grouped index and pulls a skill's text only when it loads one | [skills](https://nrakochy.github.io/hotl/skills/) |
| **Sub-agents** | `spawn` with built-in types (`general-purpose`, `explore`, `plan`) or your own `agents/*.md`; plus `fork` | [agents](https://nrakochy.github.io/hotl/agents/) |
| **MCP servers** | `[[mcp]]`, stdio transport | [mcp](https://nrakochy.github.io/hotl/mcp/) |
| **Retrieval** | `[[retrieval]]` backends behind one `recall` tool; nothing configured by default, no built-in backend touches the network | [retrieval](https://nrakochy.github.io/hotl/retrieval/) |
| **Hooks & diagnostics** | Six events (`pre_tool`, `post_tool`, `user_prompt`, `notification`, `stop`, `session_end`) that can block or add context but never *grant*; `[diagnostics]` commands run after edits | [hooks](https://nrakochy.github.io/hotl/hooks/) |
| **Shell integration** | zsh `: ` prefix turns a shell line into an agent prompt; `@[path]` file capture, OSC-133 marks | [shell](https://nrakochy.github.io/hotl/shell/) |
| **Gateways** | OpenAI-compatible gateways and command-sourced API keys | [gateway](https://nrakochy.github.io/hotl/gateway/) |

Claude Code's `~/.claude/skills`, `~/.claude/agents`, and plugin layouts load
in place — no porting.

State lives in the open: config at `~/.config/hotl/config.toml` (the only
settings file — permissions, MCP, hooks, and retrieval are all sections in
it), append-only session logs at `~/.local/share/hotl/sessions/<ulid>.jsonl`,
and per-session git snapshots under `~/.local/share/hotl/shadow/`.

## Watch — quick start

**Requirements:** [tmux](https://github.com/tmux/tmux) on your `PATH` (run it from inside a tmux session) and `ps` (standard on macOS/Linux). Not available on native Windows — there is no tmux and no pane-capture protocol to port to; use WSL2.

From inside tmux, open a pane and run it:

    hotl watch

Keys: `j`/`k` (or ↓/↑) move · `enter` jump to the selected agent · `r` refresh
· `q` or `Ctrl-c` quit · `Ctrl-h`/`j`/`k`/`l` switch tmux panes.

**Full dashboard docs — install options, usage, config, keys:** [`crates/hotl/README.md`](crates/hotl/README.md).

## The docs

The [user docs](https://nrakochy.github.io/hotl/) (source in
`site/src/content/docs/`, deployed on each release) cover installing and
running the agent — start at
[overview](https://nrakochy.github.io/hotl/overview/) for the design
commitments, or
[configuration](https://nrakochy.github.io/hotl/configuration/) to look up any
subcommand, config key, env var, or exit code. Pointing an AI agent at hotl?
[`llms.txt`](https://nrakochy.github.io/hotl/llms.txt) is the machine-readable
map.

In this repo: [`ARCHITECTURE.md`](ARCHITECTURE.md) is the harness at a glance
— the layers, the connective planes, and how a prompt flows through the
system; [`docs/SECURITY.md`](docs/SECURITY.md) is the security stance; and
[`docs/RELIABILITY.md`](docs/RELIABILITY.md) covers the failure behavior.

## Releasing

Cut a release with the helper script — it bumps the workspace version (and
every internal path-dep pin, which publish in lockstep), promotes the
changelog's `[Unreleased]` section, and commits.

    scripts/release.sh patch    # bug fix
    scripts/release.sh minor    # feature, or breaking pre-1.0
    scripts/release.sh major    # 1.0
    scripts/release.sh 0.4.2    # explicit version

The full sequence, in order:

1. **Refuse early.** Clean tree; `[Unreleased]` exists and is non-empty; no
   section for this version yet; `gh` present and authenticated. Nothing is
   edited until every reason to refuse is established — a half-bumped tree is
   worse than no release.
2. **Test locally.** `cargo test --workspace --locked`, then `cargo check` for
   Linux in a `rust:slim` container. The local suite only proves the release
   builds on *this* machine, and every release target but macOS is Linux.
3. **Bump and commit** — version, path-dep pins, docs hero, `llms.txt`,
   changelog promotion.
4. **Push the commit alone**, then **wait for CI to go green on that exact
   SHA**.
5. **Tag and push the tag** — a second, separate push.

Step 5 is what the whole gate protects. The tag triggers three workflows — the
crates.io publish, the prebuilt-binary/installer release, and the Nix tag check
— and none of them waits for CI on its own, so holding the tag is the only
thing that stops all three. `publish.yml` re-checks the same evidence before it
publishes, which covers a tag cut by hand or from another machine.

The wait requires these `ci.yml` jobs to be green on the commit:

    fmt  watch  harness  msrv  docs  audit

The `nix` legs are deliberately advisory — they are master-only, legitimately
skipped when no build input moved, and re-verified against the published tag by
`nix-tag.yml`, whose failure is already non-fatal. A Nix-only regression can
therefore still reach a tag; `nix-tag.yml` reports it minutes later.

If CI goes red, the script stops with the commit pushed and nothing tagged.
Fix the break, then finish the release without re-bumping anything:

    scripts/release.sh --tag-only

It re-reads the version from `Cargo.toml`, refuses unless `HEAD` is what
`origin/<branch>` points at, waits, tags, and pushes. It edits no files.

**There is no way to tag without green CI.** No flag, no env var. If CI is red,
still running, or unreachable, the script exits without creating a tag — so
none of the three release workflows can fire. Recover with `--tag-only`.

| Env var | Effect |
|---|---|
| `HOTL_SKIP_LINUX_CHECK=1` | Skip the Docker Linux cross-check (accepts that a Linux-only break may reach the tag) |
| `HOTL_CI_WAIT_TIMEOUT` | Seconds to wait for CI, default 1800 |
| `HOTL_CI_POLL_INTERVAL` | Seconds between polls, default 15 |

`scripts/wait-for-ci.sh <sha>` can be run on its own to check any commit.

Versions are immutable on crates.io — always go up, never reuse one. The tag
must match the `[workspace.package]` version (the script keeps them in sync).
The lib crates publish in lockstep and are internal — no semver promise attaches
to their APIs, only to the binary's contracts.

## License

hotl is dual-licensed:

1. **Open source:** GNU Affero General Public License v3.0 or later
   ([AGPL-3.0-or-later](LICENSE)).
2. **Commercial:** commercial licenses are available for organizations that
   cannot comply with AGPL. Contact nick.rakochy@gmail.com for details.
