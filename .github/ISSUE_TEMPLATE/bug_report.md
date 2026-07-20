---
name: Bug report
about: Something in hotl behaves wrong
title: ''
labels: bug
---

**Which capability?** watch (`hotl watch`) · execute (`hotl`/agent) · other

**hotl version / commit:** (`hotl update` prints the version)

**Platform:** macOS / Linux (distro + kernel) · shell

**Provider:** anthropic / openai-compatible (which endpoint? Ollama/Groq/…)

**What happened:**

**What you expected:**

**Steps to reproduce** (a `hotl -p "…"` one-shot, or the exact prompts):

**Relevant output:** paste the error text (hotl error messages are designed to be greppable — include the whole line). For a crash, run with `--json` if headless.

**Sandbox:** does `hotl doctor` show the sandbox `enforced`, `disabled`, or `unavailable`?

> Do NOT paste secrets. Session logs mask secret-named env values but are otherwise sensitive.
> For a **security** vulnerability, do not file here — use a private advisory (see SECURITY.md).
