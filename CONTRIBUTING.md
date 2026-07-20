# Contributing to hotl

Thanks for looking. hotl is a **personal-first, owner-operator** tool published for other owner-operators — not a platform chasing feature breadth. That shapes what belongs here.

## Scope is ledger-governed

Every feature is an explicit decision recorded in [docs/design-docs/feature-ledger.md](docs/design-docs/feature-ledger.md) (adopted/rejected, per source) and scheduled in [docs/exec-plans/active/0001-harness-build.md](docs/exec-plans/active/0001-harness-build.md) (the milestone-scope authority). **If a change isn't on the ledger as adopted, it isn't in the plan** — open an issue proposing the ledger row *before* a PR, so scope is agreed before code. This isn't bureaucracy; it's how a small tool stays small. A PR that adds an un-adopted feature will be asked to start with the ledger discussion.

Things deliberately **not** in scope (see the ledger's "rejected" columns and [docs/design-docs/blueprint.md](docs/design-docs/blueprint.md) §skip list): telemetry, a plugin marketplace, hosted/enterprise config, RAG memory, a leader daemon. Please don't PR these.

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
- **Rust practices** follow [docs/references/rust-specs/](docs/references/rust-specs/README.md).
- **Tests are golden and deterministic.** The engine is tested by driving the real actor/turn/persistence stack with a scripted provider (`hotl-testkit`) and asserting on the normalized log. Add a scenario there for behavior changes.

## Security-relevant changes

Read [docs/SECURITY.md](docs/SECURITY.md) first. Anything touching the permission gate, the sandbox, allow-rules, the untrusted-content envelope, or data-at-rest needs its routing-table row updated in the same PR. "Defaults are the safety design" — a change that weakens a default needs an explicit rationale.

## Reporting a vulnerability

Do **not** open a public issue for a security bug. Use GitHub private security advisories on the repo, or email the owner (address in the README once public). Coordinated disclosure, 90-day default window. Good-faith research against your own installation is welcome.

## Commits and PRs

Scoped, revertible commits with a clear subject (`feat(engine): …`, `fix(tools): …`, `docs: …`). Reference the ledger row or exec-plan item the change implements. Keep the decision log in the relevant plan updated when you make a design call — decisions count only once written down.
