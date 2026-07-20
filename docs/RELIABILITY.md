# RELIABILITY.md — budgets and recovery conventions

The rule (core belief 7): **every recovery path has an explicit budget with per-incident reset.** Sources: 01, 04, 06, 12.

- **Provider retries**: pure-data error classifier → jittered exponential backoff honoring server hints; context-overflow is never retried (routed to compaction); specific recoveries worth having: transport rebuild, image-strip on 413 (06).
- **Compaction**: two thresholds (trigger/target) to prevent thrash; bounded attempts with a thrash guard that errors instead of looping (04, 12); **guaranteed last-resort degradation floor — if every summarize attempt fails, hard-truncate to system prompt + typed digest + verbatim tail; a failed compaction never bricks the session** (Sec #10). Thresholds anchor to provider-reported usage of the previous response + margin; tokenizer delta is a hint (A12b).
- **Tool failures**: per-tool consecutive-failure budget, reset on success, with retry feedback in the tool result (11).
- **Turn caps**: max requests per turn; stop-gate blocks capped (12's 8-block Stop cap).
- **Doom loops**: detect repeating call patterns (any period, not just consecutive-identical — 11); surface to the human rather than silently breaking (10).
- **Fallback chains**: short (≤3), availability-triggered only — never on auth/billing errors (12).
- **Token accounting**: real tokenizer (Goose proves it's affordable, 09); provider usage preferred when reported.
- **The five loops round 1 found unbudgeted (S9) — budgets + terminal states** (r2 R9):
  - *Writer disk-full* (M1): one retry after an fsync/space check → session enters **"log sealed"** read-only state; the projection never advances past un-acked bytes, so there is nothing to un-commit.
  - *MCP reconnect* (M3a): 5 attempts, jittered → server marked degraded, its tools drop from the registry with an errors-as-prompts notice; manual re-enable.
  - *Catalog etag refresh* (catalog-later): one failed refresh → stale-with-timestamp; retry next process start, never in-session loops.
  - *Hook repeat-offender* (M5): 3 timeouts/errors in a session → handler evicted for the session, surfaced once.
  - *WASM epoch repeat-offender* (browser milestone): 3 epoch kills → component demoted to metadata-visible/execution-blocked.
- **Retention/GC** (r2 R6): session logs are append-only and permanent *by design*, which is exactly why secrets must be masked at ingestion (M0, Sec #8) — masking is the retention policy for secrets. For bulk: a GC budget (age- or size-triggered archive/prune of whole session files, never in-file rewrites) is owed with M3b's tree work.

- **Crash recovery of an in-flight turn** (owed; tech-debt #8): committed steps survive a process crash via the durable log (durable-append-before-projection-advance means only acked bytes are canon), but the *current, uncommitted sample* does not — a killed process loses it, and recovery is `hotl resume` + a human re-prompt, not automatic step-level replay. This is an explicit **non-guarantee** today, acceptable because the primary use is interactive and human-on-the-loop. When long autonomous/unattended runs arrive (headless batches, M4 orchestration), the convention becomes: checkpoint the in-flight sample as a partial-assistant entry so a respawn resumes the sample, not the turn. Contrast the *writer disk-full* loop above, which is already budgeted — that governs commit *failure*, not process *death* mid-sample.

Anything long-lived (daemon, background jobs) needs a converge-to-newest story before it ships (05 lesson 11).
