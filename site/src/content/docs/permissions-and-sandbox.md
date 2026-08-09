---
title: 'Permissions & the sandbox — what the guardrails do'
description: What hotl's permission gate, protected paths, allow-rules, and kernel sandbox actually protect — and what they deliberately do not.
---

The *why* behind hotl's safety model: what the y/N gate, protected paths, allow-rules, and the kernel sandbox actually protect — and, just as important, what they do **not**. For exact syntax see [configuration.md](../configuration/); for a first run see [quickstart.md](../quickstart/). Opinions here are marked as such.

## The one control that matters: you choose when to approve

Permissions have **two independent axes**. The *mode* says how a call is
handled; *plan mode* says what posture you're in. Both route through the same
evaluation pipeline, and deny rules and the protected-path floor apply
identically no matter where you set them.

### The mode: how a call is handled

**`bypass` (the default): ordinary tool calls run without asking.** The floor
still holds: `bash` runs inside the kernel sandbox, writes to execute-later
paths (git hooks, Makefiles, shell rc, agent-instruction files, hotl's own
config) always stop and ask, deny rules refuse outright, every silenced prompt
appears in the transcript as an auto-allow with its granting rule, and
`hotl undo` reverses any change. It's named for what it does to the gate — it
bypasses it — because that's a trust decision, not a convenience.

**`ask`: every mutating or executing call asks y/N first** — set
`[permissions] mode = "ask"` (or `HOTL_PERMISSIONS=ask`) if you want the human
on every loop. When there's no human — headless `-p`, sub-agents, a timed-out
interactive prompt — an ask becomes an automatic **no**, in every mode.

**`dontask`: never wait for input.** Anything that would normally prompt is
denied instead of asked — allow-rules and read-only tools still run, but
nothing pauses for a human who isn't there. This is the right posture for CI:
a script that hits an unapproved action should fail loudly, not hang waiting
on a prompt nobody will answer.

`dontask` is strictly stricter than `ask` — it only ever *denies more* than
the default pipeline would, never less — so a `security-enforced` build (which
forces `bypass` back to `ask`) leaves it alone.

> `auto` was the old name for `bypass`. It still parses, in config files and
> in session logs, and always will.

### Plan mode: the other axis

**`plan` puts `write` and `edit` on the same footing as a protected path:
always ask, never auto.** Everything else — `bash`, MCP servers, `web_fetch`,
`web_search` — takes the mode exactly as it would without plan on. So the
agent can shell out, hit your issue tracker, and read a docs page while it
works out what to propose, and still stops before it changes a file.

Toggle it with `/plan` (or `/plan on` / `/plan off`), `--plan` on the command
line, `[permissions] plan = true`, `HOTL_PLAN=1`, or `session/set_plan` over
ACP. It composes with whichever mode you're in:

| | `ask` | `bypass` | `dontask` |
|---|---|---|---|
| **plan off** | prompt per mutating call | run without prompting | allow-rule or refuse |
| **plan on** | same as `ask` | shell and network run freely, **file edits stop and ask** | **file edits refused** (no one to say yes) |

The toggle is a durable log entry, so `hotl resume` restores both axes as you
left them. An `[[allow]]` rule on `write` or `edit` cannot auto-approve while
plan is on — plan's floor sits above the allow tiers, the one thing it keeps
from the days when it was a hard block.

**Plan mode is a posture, not a boundary.** It is not a guarantee that your
tree is untouched: `bash` follows the mode, so under plan+`bypass` a shell
redirect (`printf … > src/main.rs`) changes a file without ever touching the
`write` tool. What plan buys you is that the agent's *natural* path to a
mutation stops for a human first, and that a shell doing it instead is
conspicuous in the transcript. If you need something genuinely unable to
mutate, that's `dontask` with no allow-rules — not plan.

> `/mode plan` was valid before plan became its own axis. It now points you at
> `/plan`, and a session log or config carrying `mode = "plan"` turns the
> overlay on rather than reading as a typo.

Admins who need `ask` guaranteed compile with `--features security-enforced`:
that build ignores the mode key entirely (honestly: it's organizational
control, not DRM — a user with a toolchain can build a permissive binary).
Admins can also pre-approve known tools machine-wide in
`/etc/hotl/preapproved.toml` — same `[[allow]]`/`[[deny]]` syntax as your
config, plus `lock_user_allows = true` to make theirs the only allow tier.
hotl refuses that file unless it is root-owned and not group/world-writable.

Everything else below exists to make that gate trustworthy: to keep an approved action from doing more than you thought, and to give you a way back if you approve something you shouldn't have.

### `ask_user` is not a permission gate

The `ask_user` tool ([configuration.md](../configuration/#built-in-tools)) puts a structured multiple-choice question to you — a header, a prompt, 2–4 options, plus free text. It looks like another y/N moment but it isn't one: **it never authorizes a tool call.** The answer becomes a plain-text tool result, the same shape a `read` returns, and nothing about that text can grant permission for a later mutating call — a model cannot launder an edit or a shell command through a question. That's also why its own permission is `None` and it runs under plan mode: answering a question changes nothing on disk. Like an ordinary ask, a question with no human to answer it — headless `-p`, JSON mode, a sub-agent — never hangs: it resolves immediately to a documented "no human available" default the model can act on.

### Approved work runs concurrently where that's safe

Within one model turn the agent often issues several tool calls at once. hotl runs the read-only ones concurrently — a batch of five file reads doesn't queue behind itself — while anything that mutates or executes (`bash`, `write`, `edit`) runs strictly one at a time, in source order, and never overlaps with anything else. Permission asks are unaffected: every approval is still presented to you one at a time, before the calls it gates run. Sub-agents (`spawn`) count as overlap-safe too: each child runs in its own isolated session, so several approved sub-agents work side by side. Concurrency never changes *what* is allowed — only how long the allowed work takes.

## The sandbox floor: mostly write-confinement, *not* a security wall

When you approve a `bash` command, it runs inside a kernel sandbox (Seatbelt on macOS, Landlock on Linux) that confines **writes** to your working directory, the temp dir, `/dev`, and any extra directories you list in [`[sandbox].writable`](../configuration/#sandbox-write-floor-sandbox). A command can't scribble over files elsewhere on disk.

Since 0.10 it also denies a small, fixed set of **reads** — see [the read carve](#the-read-carve) below. Read this part carefully, because it is the most misunderstood thing about hotl:

> **The sandbox still does not stop a command from reading most of your files, or from using the network.** Two carves aside, reads are open and egress is open, on purpose — the agent legitimately reads your whole tree and fetches dependencies. An approved command cannot read `~/.ssh/id_rsa` any more, but it *can* read a `.env` in your project and send it anywhere.

The sandbox stops the agent **tampering with your filesystem outside the project**, and keeps a short list of credential paths out of its reach. It is **not** a data-loss or exfiltration boundary. The thing standing between the agent and your secrets is *your approval of each command* — not the sandbox. So when a command asks to run, read what it actually does. A plausible "run the tests" command that also `curl`s somewhere will exfiltrate freely once you say yes.

### The read carve

Three tiers are denied to every sandboxed child — the `bash` tool, `grep`'s ripgrep, post-edit diagnostic commands, and your shell hooks alike. Three of those four never show you a prompt, which is why the config lever exists.

| Tier | What | Liftable? |
|---|---|---|
| **A** | hotl's own config dir (`~/.config/hotl`) and data dir (`~/.local/share/hotl`) | **never** |
| **B** | `~/.ssh`, `~/.aws`, `~/.config/gcloud`, `~/.azure`, and `.netrc` / `.npmrc` / `.pypirc` / `.dockercfg` in `$HOME` | yes, two ways |
| **C** | directories your own `[[deny]]` rules name | **never** (the rule is a "never") |

Tiers B and C point in opposite directions and neither replaces the other: `[sandbox].readable` *subtracts* from B, and your deny rules *add* C.

Tier A is the important one and the reason there is no override: the session token under the data dir is what drives hotl's own control socket. A sandboxed `bash` runs as *you*, so file permissions do not stop it reading that token — and a command that reads it can approve its own future tool calls. The config dir holds your allow-rules, hooks, and `api_key_helper` for the same reason. The `read`, `write` and `edit` tools refuse those paths outright too, in every mode, with no prompt that unlocks them — which does mean the agent can't read your `config.toml` for you either.

Tier B is the ordinary credential class. **This does not break agent-run `git push` if your keys are in `ssh-agent`** — the agent socket stays reachable, and `ssh` never opens a key file in that case. It *does* break when no agent is loaded, when your cloud session isn't cached, or for a private-registry `npm install`. Two ways out:

```toml
[sandbox]
readable = ["~/.aws"]   # standing; needs a session restart
```

…or press **`s`** instead of `y` at a `bash` ask, which lifts Tier B for **that one command only**. The `s` option appears only where it would do something. Once config has lifted the tier, every ask says `reads:open`; the hardened default stays silent.

**Tier C is your own deny rules, enforced at the kernel too.** A `[[deny]]` over
a path used to govern only hotl's own file tools — it stopped
`read {"path": "/Volumes/secrets/k"}` and did nothing at all about
`bash {"command": "cat /Volumes/secrets/k"}`. You wrote down an intent and half
of it was enforced. Now a deny rule that names a real directory becomes a kernel
read-deny as well:

```toml
[[deny]]
tool = "read"
path_prefix = "/Volumes/secrets"   # or "~/Documents/tax"
```

It is unliftable on purpose: a deny rule is a "never" in-process, so a kernel
twin that `s` could lift would be the drift this closes.

**Not every deny rule can reach the kernel, and hotl tells you which.** The
kernel sees paths, not command names or match patterns, so three shapes stay
in-process only:

- `path_prefix = ".ssh/"` — a *floating* prefix, matching at any depth. Expressing that at the kernel would mean enumerating every `.ssh` on the disk at startup and missing any created later, so hotl reports it instead of approximating. Write `path_prefix = "~/.ssh"` to cover shell commands too.
- `[[deny]] tool = "bash" prefix = "curl "` — a command name has no kernel expression, and never will.
- a path that does not exist yet — nothing to deny; it starts reaching the kernel once it exists.

Each of those is still enforced in full for `read`/`write`/`edit`/`glob`/`grep`.
`hotl doctor` lists them under `not reaching shell commands`, with the form to
write instead, and names the rule behind every path it *does* deny. `[[allow]]`
rules never move the kernel floor in either direction.

**It denies contents, not existence.** `ls ~` and `ls -la ~` keep working — the carve blocks reading file data, not `stat`. One honest platform difference: on Linux `ls ~/.ssh` still lists filenames (Landlock has no sub-path carve-out, so the parent's directory-listing right is hierarchical) where macOS hides them. File contents are denied on both.

*(Opinion:* with the default open egress, the honest rule is: don't run hotl against secrets you wouldn't paste into a terminal command yourself — or close the door: see the next section.*)*

On hosts with no sandbox mechanism (older Linux kernels, or `HOTL_SANDBOX=off`), the floor is simply absent — every `bash` ask is marked `UNSANDBOXED`, and allow-rules for `bash` stop working. The gate still holds; the confinement doesn't.

### `sandboxed:` means it was proven, not that it was configured

At startup hotl spawns one throwaway sandboxed process that tries to write a file outside the confinement, and only reports the floor as enforced if that write **fails**. Until this check existed, "enforced" meant "the sandbox binary is on disk" — and since that single answer is what lets `bash` allow-rules auto-approve without asking you, a profile that failed to apply for any reason meant *unconfined* commands being auto-approved silently. Now a host that can't demonstrate confinement degrades loudly instead of claiming it.

It costs one process spawn per session, capped at two seconds. The probe file is uniquely named and deleted on every path, including the one where it leaks. If neither `/var/tmp` nor `$HOME` is writable, point `HOTL_SANDBOX_PROBE_DIR` at somewhere that is — outside your working directory, outside `TMPDIR`, and outside every `[sandbox].writable` entry, or it proves nothing.

### Widening the floor deliberately (`[sandbox].writable`)

Some tools keep their caches outside the workspace — bazel writes `~/Library/Caches/bazel` and `~/.bazel_disk_cache`, ccache has `~/.ccache` — and the default floor refuses those writes. `[sandbox].writable` in `config.toml` re-allows the directories you name, for everything that runs under the floor: `bash`, `grep`, post-edit diagnostics, and shell hooks. Full syntax and validation rules: [configuration.md](../configuration/#sandbox-write-floor-sandbox).

The widening cannot be turned against hotl itself: an entry that would expose hotl's own config dir (allow-rules, hooks, the `api_key_helper` command) or data dir (session logs, snapshots) is refused with a warning — which is also why `~` and `/` can never be made writable, since they contain the config dir. Refusal is per-entry; the rest of the list still applies. The startup probe picks its target outside the widened set, so `sandboxed:` still means *proven* — including your extra directories. By default the widening applies only to spawned processes; the `write`/`edit` file tools follow it only if you additionally set `file_tools = "writable"` (next section).

### What each platform can and can't confine

The floor is real on both platforms, but they are not equivalent, and the differences are worth knowing before you rely on one:

- **Linux needs kernel ≥ 6.2** for the full floor. Below that (RHEL 9's 5.14, Ubuntu 22.04's 5.15) Landlock exists but lacks the *truncate* right, so an approved command can still zero a file anywhere on the host. hotl refuses to certify that as enforced: those kernels lose `bash` auto-allow unless you set `HOTL_SANDBOX=best-effort`, which accepts the partial floor and labels every ask `sandboxed:landlock(partial)`. Landlock's network confinement separately needs ≥ 6.7.
- **Unix-domain sockets are a network operation, not a file write**, so the write floor never covered them. On macOS the container-daemon socket class (`docker.sock`, `podman.sock`, `containerd`, `crio`) is denied by default — reaching the Docker API is a complete escape, since it can mount the host root — and `HOTL_UNIX_SOCKETS=open` opts back in, marked `unix:open` in every ask. `ssh-agent`/`gpg-agent` stay reachable so `git push` over SSH keeps working. **On Linux none of this is enforceable**: Landlock has no rule covering a connect to a pathname socket at any ABI. If a writable daemon socket is a privilege boundary you depend on, don't run `mode = "bypass"` on that host.
- **macOS also denies Apple Events** from a confined command, because `osascript -e 'tell application "Terminal" to do script …'` runs its payload in a process that isn't a descendant of the sandbox — an escape that never touches disk. Plain AppleScript still runs; only the cross-application send is refused. `HOTL_MACOS_AUTOMATION=allow` restores Xcode/Simulator flows that drive tools this way, marked `automation:allow`.

### Your provider key doesn't reach the commands you approve

Child processes used to inherit hotl's whole environment, so `ANTHROPIC_API_KEY` was one `env` away from any auto-approved `bash` call. Provider credentials are now stripped from every child — `bash`, `grep`'s ripgrep, post-edit diagnostics, and your shell hooks alike.

The scrub is deliberately narrow. The tempting rule — strip anything named like a secret — would also take `GITHUB_TOKEN`, `CARGO_REGISTRY_TOKEN` and `NPM_TOKEN`, breaking `gh`, `cargo publish` and `npm publish` in ways that look like unrelated bugs. Add names with `HOTL_SCRUB_ENV=A,B,C`, or take the broad rule knowingly with `HOTL_SCRUB_ENV_STRICT=1`.

## Opting out of open egress

`[network].egress` in `config.toml` closes the door the previous section describes as open. Set it to `"off"` and an approved command can reach only your own machine — loopback and unix-domain sockets; the kernel refuses everything else. Set it to `"allowlist"` and you add whatever hosts the agent needs beyond the ones hotl already ships:

```toml
[network]
egress = "allowlist"
allow = ["internal.example.com", "*.corp.example.com"]
```

Allowed hosts are reached through a small local proxy, so `cargo fetch` and `git pull` keep working while a `curl` to anywhere else gets a 403 that tells the model exactly which control refused it (`hotl egress: "HOST" is not in [network].allow`). Tools that ignore the proxy environment don't get around anything — they hit the kernel's loopback-only wall and fail. Every bash ask shows the active state: `net:off` or `net:allow(N)`.

### An allowlist doesn't start empty

Until 0.10 it did, which is the real reason nobody ran `allowlist`: the first ten minutes of any session were a wall of 403s from package registries. An allowlist now starts from a **starter list** hotl ships — the registries, their CDNs, and the forges a build reaches without anyone deciding to reach them — and your `allow` entries are added to it. `hotl doctor` prints the effective list with both sources; [configuration.md](../configuration/#network-egress-network) enumerates it inline. `defaults = false` under `[network]` drops it and gives you a list containing exactly what you wrote.

**It is not an anti-exfiltration control, and it does not claim to be.** It bounds accidents and drive-by fetches. `github.com` is on it and is bidirectional — a gist push leaves through it. Adding to the list is a security review, not a convenience patch, which is why every entry is an exact host and a test refuses wildcards there.

### A blocked host is a question, not a dead end

The other reason nobody ran `allowlist`: the only recourse to a 403 was to stop the session, edit `config.toml`, restart, and re-prompt. Now a host that isn't on the list prompts:

```
  network: reaching "registry.npmjs.org" was not in the approved command
  this host is not in [network].allow
  [y] allow for this session   [n] deny
```

Both answers stick for the rest of the session, and both are session-scoped only — `y` doesn't touch your config, it prints the line to paste if you want it permanently. Remembering the *deny* matters as much as remembering the allow: without it, a retrying command uses you as a rate limiter.

**The prompt is deliberately rare, and that is the security property, not a nicety.** A prompt you clear reflexively is not a control, and every one you answer without reading makes the next one likelier to go the same way. So three filters sit in front of you: the starter list, the session's own decisions, and — the interesting one — **hosts you already saw**. If you approved `curl https://docs.example.com/x`, you read that host; the connection it opens does not prompt again. What still prompts is the surprise: a redirect, a CDN, a transitive registry dep, a host that only appeared because a `[[allow]]` rule approved the command without showing it to you. That last case is the point — *a rule is not a human*, so a rule-approved `curl` to an unlisted host still asks.

Two sharp edges worth knowing. A URL carrying userinfo (`https://good.com@evil.com/`) counts as showing you **nothing**, because your eye lands on `good.com` while the connection goes to `evil.com`. And approving an *edited* command shows nothing either — the summary you read is stale the moment you change it.

**What `y` actually grants.** One host, for this session, for every connection to it — not one connection. A `CONNECT` tunnel then carries unbounded bytes in both directions for as long as it lives. That is a bigger grant than the single prompt suggests, which is why it is worth reading the host.

**Where it doesn't ask.** Headless (`-p`, `--schema`) never installs the prompt at all — an unlisted host 403s immediately rather than hanging. Sub-agents are denied the same way. `egress = "off"` has no prompt either, and cannot: the kernel refuses the connection with no proxy in the path to intercept. If nobody answers within two minutes the connection is refused, but nothing is recorded — a timeout is not a decision, so the next attempt asks again.

Three honest caveats. First, this is **opt-in**: the default stays open because the agent legitimately fetches things, and a silently broken network by default would just teach everyone to turn the feature off. Second, **only HTTP traffic can traverse the proxy** — `git` over an SSH remote (`git@github.com:…`) fails under `off`/`allowlist` no matter what you allow; switch those repos to HTTPS remotes while running restricted. Third, it is **not airtight**: an allowed host is allowed for *everything* (an allowlisted `github.com` can still receive a push of your data), DNS still resolves (macOS resolves names via a local system service; on Linux, Landlock confines TCP only, and needs kernel ≥ 6.7), so a determined DNS-tunnel can still leak — treat egress restriction as a strong brake on casual exfiltration, not a cleanroom. And if the kernel can't enforce the restriction you configured, hotl says so loudly — `NET:UNENFORCED(reason)` in every bash ask — and `bash` allow-rules stop auto-approving, the same fail-closed posture as an unsandboxed host. The full mechanics and limits live in [SECURITY.md](https://github.com/nrakochy/hotl/blob/master/docs/SECURITY.md).

Two smaller things about the proxy. It is **bounded**: a client that opens a connection and never finishes its request gets ten seconds, and there is a ceiling of 64 live connections before it starts answering `503` — an unfinished request used to pin a socket for the life of the process. And it is **authenticated**: the proxy URL hotl hands your commands carries a per-session credential (`http://hotl:<token>@127.0.0.1:<port>`), so another process on the same machine can't quietly spend your allowlist. curl, git-over-HTTP, pip and cargo all forward it and are unaffected; a client that honors the proxy host but discards credentials gets a `407`, and `HOTL_PROXY_AUTH=off` is the escape hatch. The token isn't cryptographic and doesn't pretend to be — it rides your commands' environment, so it separates *processes*, not *users*.

## What `web_fetch` shows you, and where it refuses to go

`web_fetch` always asks, and the ask now carries the **whole URL** — path, query and all. It used to show only the host, which hid precisely the thing worth seeing: a fetch exfiltrates through the URL itself, so `web_fetch: pastebin.com` and `web_fetch: pastebin.com/p?d=<your ssh key>` looked identical at the moment you approved one. Long URLs are elided in the middle with an explicit count of what was cut, never trimmed silently.

Two targets get stricter treatment:

- **Cloud instance-metadata addresses** (`169.254.169.254` and its siblings) are refused outright, on the first hop and on every redirect hop. On a cloud VM that endpoint hands out instance credentials to anything that asks, and nothing legitimate needs an agent to read it. `HOTL_WEB_ALLOW_METADATA=1` exists if you genuinely do.
- **A redirect from the public web into your private network is refused.** You approved hop one; hop two into `10.0.0.5` or `127.0.0.1` is a target you never saw. An allowed public host could otherwise 302 into an internal service and return its response into the model's context. A chain that *starts* private is fine — "fetch `http://localhost:3000` and tell me what's wrong" is a real workflow, and that target was on screen when you approved it.

Fetching a private or loopback address directly still works, but it is a **protected** ask: it prompts in every mode, including the default `bypass`, the same way a write to an execute-later path does.

One honest limit: the classification reads literal addresses. A *hostname* that resolves into private space on a redirect hop isn't caught, because the decision has to be made without doing a DNS lookup.

## The workspace boundary: the file tools stay in the project

`read`, `write`, `edit`, `glob`, and `grep` are scoped to the working
directory, and the scope is enforced on the **file descriptor**, not on the
path string. A path counts as inside only if a descent from the
workspace-root descriptor reached it *without traversing a symlink*. Because
the decision is made on the descriptor the tool then uses, there is no name
to re-resolve afterwards and therefore no check/open race to lose. (On Linux
this is a single `openat2(RESOLVE_BENEATH)`; elsewhere it is a
component-by-component `openat` with `O_NOFOLLOW`.)

The practical consequences:

- **A symlink out of the tree is refused, not followed.** `ln -s ~ link`
  followed by `grep --path link` used to search your whole home directory
  with no prompt. It now stops at `link`.
- **`read` outside the working directory is a protected ask.** An absolute
  path, a `..` escape, or a path that leaves through a symlink prompts — and
  it is *protected*, so it prompts in **every** mode, including the default
  `bypass`. This is the one deliberate exception to "ordinary tool calls run
  without asking", and it is deliberate for a plain reason: an ordinary ask
  is auto-approved under the shipped default, so it would have protected
  nothing.
- **`write` and `edit` never follow a symlink**, at any component including
  the last, so a `docs/notes.md` that points at `~/.zshrc` is refused rather
  than quietly rewriting your shell config. Their protected-path
  classification also runs on the *resolved* target, so a symlink can't
  launder a protected write into an ordinary one.

**This is the change most likely to surprise you.** If your workflow has the
agent read `~/notes.md` mid-session, that read now prompts where it used to
be silent. Approving it works exactly as before — the escalation is a gate,
not a ban, and the prompt shows you where the path really lands, links
resolved.

Benign symlinks *inside* the tree are refused too. Telling "points inside"
from "points outside" would mean resolving a name and comparing the result,
which is precisely the name-based check this design exists to remove. The
refusal tells the model to re-issue with the absolute path and take the ask,
so nothing is unreachable — only gated.

**Opting `write`/`edit` into the widened floor.** By default the workspace
boundary above ignores `[sandbox].writable` — those directories are writable
to *bash*, not to the file tools. `file_tools = "writable"` in `[sandbox]`
is the deliberate, documented step that extends the boundary: a `write` or
`edit` whose path lands under a listed directory becomes an **ordinary** ask
(the same tier as an in-workspace write, so `mode = "bypass"` approves it)
instead of a protected one, and runs through the same fd-descent,
symlink-refusing guard as workspace writes — anchored at that directory. Two
things never change: a path outside both the workspace and the listed
directories stays a protected ask, and a protected filename (a `Makefile`,
a `.zshrc`) under a listed directory still escalates — the grant widens
*where* the tools may write, never *what kind* of write gets waved through.
An unknown `file_tools` value falls back to `"workspace"` with a warning.

## Protected paths: some writes are more dangerous than they look

Writing a file is usually harmless until *later*. A `.git/hooks/pre-commit`, a `Makefile`, a `build.rs`, your `~/.zshrc`, an `~/.ssh/authorized_keys` — writing these is benign, but the *next* git command, build, shell, or login runs code or grants access you never explicitly approved. This is the "write-now, execute-later" trap.

hotl keeps a list of these **protected paths** and escalates their write ask with a warning that says *why* it's dangerous. A protected path can never be silently auto-approved by an allow-rule — it always asks, no matter what your `config.toml's [[allow]]` says. The list covers git hooks/config, build entrypoints (`Makefile`, `build.rs`, `conftest.py`, `setup.py`), toolchain entrypoints that run a command on the next ordinary invocation (`.cargo/config.toml`, `package.json` npm scripts, `.envrc`, `.pre-commit-config.yaml`, compose files, `.vscode/tasks.json` and `launch.json`), CI workflows under `.github/workflows/` (they run on your next push, with the repo's secrets), agent-instruction files (`AGENTS.md`, `CLAUDE.md`), the whole shell-startup family (`.profile`, `.bash_profile`, `.bash_login`, `.bash_logout`, `.bashrc`, `.zshrc`, `.zshenv`, `.zprofile`, `.zlogin`), hotl's own config directory (`~/.config/hotl/`, including `config.toml` and its `api_key_helper` command), SSH keys and config, cloud and package-registry credentials (`.aws/`, `.npmrc`, `.pypirc`, `.netrc`, …), and cron/systemd units.

## Why allow-rules are a file you edit

Approving every `cargo test` gets tedious, and tedium is a security problem: a person mashing `y` to clear prompts will eventually approve something they shouldn't. That's *ask-fatigue*, and it's how well-meaning tools grow an ungoverned "allow everything" habit.

hotl's answer: you can pre-approve trusted command families and file scopes — but **only by editing the `[[allow]]` section of `~/.config/hotl/config.toml` deliberately.** There is no in-console "always allow this" button, because a button is exactly the fatigue-driven reflex we want to avoid. Persisting trust should be a considered act with an editor, not a keystroke mid-task.

Even then, allow-rules are trust *grants*, not fine scopes, and hotl treats them cautiously:
- A `bash` prefix like `cargo ` is a grant to that command family — so a command that tacks on `; curl … | sh` or `&& rm -rf ~` (any shell chaining/redirection) drops back to asking. The prefix isn't a leash on the rest of the line.
- A `write`/`edit` path prefix is checked after resolving `..`, so `src/../../etc/x` doesn't sneak past a `src/` rule.
- Protected paths ignore allow-rules entirely (above).

## The safety net: snapshots and undo

Approval is a judgment call, and judgment is fallible. So hotl photographs your workspace before and after every mutating batch (into a private git repo that never touches your project's own `.git`), and `hotl undo` restores the last pre-change snapshot. Secret-bearing files are kept out of these snapshots. This doesn't prevent a bad change — it makes one reversible, which is what lets you approve steps at a reasonable pace instead of agonizing over each one.

## The honest summary

| Threat | What protects you | What does *not* |
|---|---|---|
| Agent changes a file you didn't intend | the y/N gate + undo | — |
| Agent writes outside the project | the sandbox floor (bash) | — |
| Agent reads `~/.ssh` / `~/.aws` and exfiltrates it | the read carve (default on, at the kernel) | — |
| Agent reads hotl's session token and approves its own calls | the Tier A carve — kernel for `bash`, flat refusal in the file tools | — |
| Agent reads an in-tree secret (`.env`) and exfiltrates it | **your reading of each approved command**, plus `[network].egress` if you set it | the read carve (in-tree secrets are out of scope) and the default open egress |
| A file tool reading or writing outside the project | the workspace boundary, enforced on the fd — out-of-tree `read` is a protected ask, and no file tool follows a symlink | `bash` — an approved command still reads anything you can read |
| A benign-looking write that runs code later | protected-path escalation | — |
| Ask-fatigue growing a blanket allowlist | file-only allow-rules, no in-console button | — |

The gate is the wall. The sandbox, protected paths, and undo make the wall livable and the mistakes recoverable. None of them replaces you looking at what you approve.

**Source of record:** [docs/SECURITY.md](https://github.com/nrakochy/hotl/blob/master/docs/SECURITY.md) is the authoritative stance and routing table; this file is its user-facing explanation.
