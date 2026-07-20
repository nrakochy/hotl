# Contributing to hotl

Thanks for looking. hotl is a **personal-first, owner-operator** tool published for other owner-operators — not a platform chasing feature breadth. That shapes what belongs here.

## Scope

hotl is deliberately small, and every feature is an explicit decision. **Before a PR that adds a feature, open an issue proposing it** so scope is agreed before code. This isn't bureaucracy; it's how a small tool stays small. A PR that adds an un-agreed feature will be asked to start with that discussion.

Things deliberately **not** in scope: telemetry, a plugin marketplace, hosted/enterprise config, RAG memory, a leader daemon. Please don't PR these.

## The bar for code

Run before you push — CI enforces all of it:

```
cargo test --workspace --locked
cargo clippy --workspace --all-targets -- -D warnings
cargo audit          # dependency vulnerabilities
```

Local conventions on top of that:
- **No function over ~60 lines.** Long functions get split; the codebase holds to this.
- **Errors are prompts.** Every error string a model can see must instruct it what to do next (a tested invariant).
- **No string-sniffing for provenance.** Injected items carry a `SyntheticReason`; never parse text to learn where something came from.
- **Forward-compat serde on anything persisted** (`#[serde(other)] Unknown`, default + skip-when-none).
- **Rust practices** — idiomatic and warning-clean; no `unsafe` without a written justification.
- **Tests are golden and deterministic.** The engine is tested by driving the real actor/turn/persistence stack with a scripted provider (`hotl-testkit`) and asserting on the normalized log. Add a scenario there for behavior changes.

## Security-relevant changes

Read [docs/SECURITY.md](docs/SECURITY.md) first. Anything touching the permission gate, the sandbox, allow-rules, the untrusted-content envelope, or data-at-rest needs its routing-table row updated in the same PR. "Defaults are the safety design" — a change that weakens a default needs an explicit rationale.

## Reporting a vulnerability

Do **not** open a public issue for a security bug. Use GitHub private security advisories on the repo, or email the owner (address in the README once public). Coordinated disclosure, 90-day default window. Good-faith research against your own installation is welcome.

## Commits and PRs

Scoped, revertible commits with a clear subject (`feat(engine): …`, `fix(tools): …`, `docs: …`). When you make a design call, write the rationale into the PR — decisions count only once written down.
