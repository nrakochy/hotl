# Permissions & the sandbox — what the guardrails do

**Mode: explanation.** This is the *why* behind hotl's safety model: what the y/N gate, protected paths, allow-rules, and the kernel sandbox actually protect — and, just as important, what they do **not**. Read it away from the keyboard. For exact syntax see [configuration.md](configuration.md); for a first run see [quickstart.md](quickstart.md). Opinions here are marked as such.

## The one control that matters: you approve each change

hotl's load-bearing safety property is simple: **the model can read and think freely, but every action that changes your machine — running a shell command, writing or editing a file — stops and asks you first.** Reads never ask. This is the "human on the loop" the name refers to.

The gate is *fail-closed*. When there's no human to ask — headless `-p` mode, a non-interactive terminal, or a 300-second timeout on an interactive prompt — the answer is **no**, automatically. hotl will refuse an action before it will perform one you didn't see.

Everything else below exists to make that gate trustworthy: to keep an approved action from doing more than you thought, and to give you a way back if you approve something you shouldn't have.

### Approved work runs concurrently where that's safe

Within one model turn the agent often issues several tool calls at once. hotl runs the read-only ones concurrently — a batch of five file reads doesn't queue behind itself — while anything that mutates or executes (`bash`, `write`, `edit`) runs strictly one at a time, in source order, and never overlaps with anything else. Permission asks are unaffected: every approval is still presented to you one at a time, before the calls it gates run. Sub-agents (`spawn`) count as overlap-safe too: each child runs in its own isolated session, so several approved sub-agents work side by side. Concurrency never changes *what* is allowed — only how long the allowed work takes.

## The sandbox floor: write-confinement, *not* a security wall

When you approve a `bash` command, it runs inside a kernel sandbox (Seatbelt on macOS, Landlock on Linux) that confines **writes** to your working directory, the temp dir, and `/dev`. A command can't scribble over files elsewhere on disk.

Read this part carefully, because it is the most misunderstood thing about hotl:

> **By default, the sandbox does not stop a command from reading your files or using the network.** Reads are open and egress is open, on purpose — the agent legitimately reads your whole tree and fetches dependencies. So an approved command *can* read `~/.ssh/id_rsa` or `~/.aws/credentials` and send it anywhere.

The sandbox stops the agent **tampering with your filesystem outside the project**. It is **not** a data-loss or exfiltration boundary. The thing standing between the agent and your secrets is *your approval of each command* — not the sandbox. So when a command asks to run, read what it actually does. A plausible "run the tests" command that also `curl`s somewhere will exfiltrate freely once you say yes.

*(Opinion:* with the default open egress, the honest rule is: don't run hotl against secrets you wouldn't paste into a terminal command yourself — or close the door: see the next section.*)*

On hosts with no sandbox mechanism (older Linux kernels, or `HOTL_SANDBOX=off`), the floor is simply absent — every `bash` ask is marked `UNSANDBOXED`, and allow-rules for `bash` stop working. The gate still holds; the confinement doesn't.

## Opting out of open egress

`[network].egress` in `config.toml` closes the door the previous section describes as open. Set it to `"off"` and an approved command can reach only your own machine — loopback and unix-domain sockets; the kernel refuses everything else. Set it to `"allowlist"` and you add a short list of hosts the agent may fetch from:

```toml
[network]
egress = "allowlist"
allow = ["github.com", "*.crates.io"]
```

Allowed hosts are reached through a small local proxy, so `cargo fetch` and `git pull` keep working while a `curl` to anywhere else gets a 403 that tells the model exactly which control refused it (`hotl egress: "HOST" is not in [network].allow`). Tools that ignore the proxy environment don't get around anything — they hit the kernel's loopback-only wall and fail. Every bash ask shows the active state: `net:off` or `net:allow(N)`.

Three honest caveats. First, this is **opt-in**: the default stays open because the agent legitimately fetches things, and a silently broken network by default would just teach everyone to turn the feature off. Second, **only HTTP traffic can traverse the proxy** — `git` over an SSH remote (`git@github.com:…`) fails under `off`/`allowlist` no matter what you allow; switch those repos to HTTPS remotes while running restricted. Third, it is **not airtight**: an allowed host is allowed for *everything* (an allowlisted `github.com` can still receive a push of your data), DNS still resolves (macOS resolves names via a local system service; on Linux, Landlock confines TCP only, and needs kernel ≥ 6.7), so a determined DNS-tunnel can still leak — treat egress restriction as a strong brake on casual exfiltration, not a cleanroom. And if the kernel can't enforce the restriction you configured, hotl says so loudly — `NET:UNENFORCED(reason)` in every bash ask — and `bash` allow-rules stop auto-approving, the same fail-closed posture as an unsandboxed host. The full mechanics and limits live in [SECURITY.md](../SECURITY.md).

## Protected paths: some writes are more dangerous than they look

Writing a file is usually harmless until *later*. A `.git/hooks/pre-commit`, a `Makefile`, a `build.rs`, your `~/.zshrc`, an `~/.ssh/authorized_keys` — writing these is benign, but the *next* git command, build, shell, or login runs code or grants access you never explicitly approved. This is the "write-now, execute-later" trap.

hotl keeps a list of these **protected paths** and escalates their write ask with a warning that says *why* it's dangerous. A protected path can never be silently auto-approved by an allow-rule — it always asks, no matter what your `config.toml's [[allow]]` says. The list covers git hooks/config, build entrypoints (`Makefile`, `build.rs`, `conftest.py`), agent-instruction files (`AGENTS.md`, `CLAUDE.md`), shell startup files, hotl's own config directory (`~/.config/hotl/`, including `config.toml` and its `api_key_helper` command), SSH keys and config, cloud and package-registry credentials (`.aws/`, `.npmrc`, `.pypirc`, `.netrc`, …), and cron/systemd units.

## Why allow-rules are a file you edit

Approving every `cargo test` gets tedious, and tedium is a security problem: a person mashing `y` to clear prompts will eventually approve something they shouldn't. That's *ask-fatigue*, and it's how well-meaning tools grow an ungoverned "allow everything" habit.

hotl's answer: you can pre-approve trusted command families and file scopes — but **only by editing the `[[allow]]` section of `~/.config/hotl/config.toml` deliberately.** There is no in-REPL "always allow this" button, because a button is exactly the fatigue-driven reflex we want to avoid. Persisting trust should be a considered act with an editor, not a keystroke mid-task.

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
| Agent reads a secret and exfiltrates it | **your reading of each approved command**, plus `[network].egress` if you set it | the default sandbox (reads + egress open unless you opt in) |
| A benign-looking write that runs code later | protected-path escalation | — |
| Ask-fatigue growing a blanket allowlist | file-only allow-rules, no in-REPL button | — |

The gate is the wall. The sandbox, protected paths, and undo make the wall livable and the mistakes recoverable. None of them replaces you looking at what you approve.

**Source of record:** [docs/SECURITY.md](../SECURITY.md) is the authoritative stance and routing table; this file is its user-facing explanation.
