# RELIABILITY.md — budgets and recovery conventions

The rule: **every recovery path has an explicit budget with per-incident reset.**

- **Provider retries**: pure-data error classifier → jittered exponential backoff honoring server hints; context-overflow is never retried (routed to compaction); specific recoveries worth having: transport rebuild, image-strip on 413.
- **Compaction**: two thresholds (trigger/target) to prevent thrash; bounded attempts with a thrash guard that errors instead of looping; **guaranteed last-resort degradation floor — if every summarize attempt fails, hard-truncate to system prompt + typed digest + verbatim tail; a failed compaction never bricks the session**. Thresholds anchor to provider-reported usage of the previous response + margin; tokenizer delta is a hint.
- **Tool failures**: per-tool consecutive-failure budget, reset on success, with retry feedback in the tool result.
- **Turn caps**: max requests per turn; stop-gate blocks capped (an 8-block Stop cap).
- **Doom loops**: detect repeating call patterns (any period, not just consecutive-identical); surface to the human rather than silently breaking.
- **Fallback chains**: short (≤3), availability-triggered only — never on auth/billing errors.
- **Token accounting**: real tokenizer (proven affordable); provider usage preferred when reported.
- **Five loops that need budgets + terminal states:**
  - *Writer disk-full* (M1): one retry after an fsync/space check → session enters **"log sealed"** read-only state; the projection never advances past un-acked bytes, so there is nothing to un-commit.
  - *MCP reconnect* (M3a): 5 attempts, jittered → server marked degraded, its tools drop from the registry with an errors-as-prompts notice; manual re-enable.
  - *Catalog etag refresh* (catalog-later): one failed refresh → stale-with-timestamp; retry next process start, never in-session loops.
  - *Hook repeat-offender* (M5): 3 timeouts/errors in a session → handler evicted for the session, surfaced once.
  - *WASM epoch repeat-offender* (browser milestone): 3 epoch kills → component demoted to metadata-visible/execution-blocked.
- **Retention/GC**: session logs are append-only and permanent *by design*, which is exactly why secrets must be masked at ingestion (M0) — masking is the retention policy for secrets. For bulk: a GC budget (age- or size-triggered archive/prune of whole session files, never in-file rewrites) is owed with M3b's tree work.

- **Crash recovery of an in-flight turn** (owed; tech-debt #8): committed steps survive a process crash via the durable log (durable-append-before-projection-advance means only acked bytes are canon), but the *current, uncommitted sample* does not — a killed process loses it, and recovery is `hotl resume` + a human re-prompt, not automatic step-level replay. This is an explicit **non-guarantee** today, acceptable because the primary use is interactive and human-on-the-loop. When long autonomous/unattended runs arrive (headless batches, M4 orchestration), the convention becomes: checkpoint the in-flight sample as a partial-assistant entry so a respawn resumes the sample, not the turn. Contrast the *writer disk-full* loop above, which is already budgeted — that governs commit *failure*, not process *death* mid-sample.

Anything long-lived (daemon, background jobs) needs a converge-to-newest story before it ships.
