//! The session actor: sole committer, projection owner, turn scheduler.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use hotl_context::compaction;
use hotl_platform::Clock;
use hotl_provider::{Provider, SamplingRequest, StreamEvent};
use hotl_store::SessionLog;
use hotl_tools::{
    rules::{PermissionMode, Rules},
    Registry,
};
use hotl_types::{assistant_text, EntryPayload, Item, SyntheticReason, Todo, TokenUsage};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{turn, EngineConfig, EngineEvent, Outcome, SessionCmd, SessionDeps, TurnEnd};

/// Verbatim tail kept through a compaction, as a share of the window.
pub(crate) const TAIL_RATIO: f64 = 0.3;
const SUMMARIZE_ATTEMPTS: u32 = 2;
const SUMMARIZE_MAX_TOKENS: u32 = 2_000;
/// Compactions without an intervening completed sample before giving up —
/// prevents a fold-the-digest spiral when the tail alone overflows.
/// INVARIANT: a fold with progress behind it never draws down this cap.
/// Enforced by `three_folds_with_progress_do_not_exhaust_the_streak`.
const MAX_COMPACT_STREAK: u32 = 2;
/// Wall-clock bound on the inline compaction summarize. The actor's command
/// loop is blocked for its duration (T3-4), so it is bounded even though the
/// provider call has its own retries: a degraded floor digest is a handled
/// outcome, an unresponsive session is not. Sized as one full retry budget
/// ([`SUMMARIZE_ATTEMPTS`] attempts under the provider's own per-request
/// timeout) so the outer net never cuts a legitimate retry short.
/// INVARIANT: the actor's command loop stalls for at most this long on a fold.
/// Enforced by `a_hung_inline_summarize_degrades_instead_of_wedging`.
const COMPACT_SUMMARIZE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
/// Queued prompts before new ones coalesce into the last entry (T3-7).
const QUEUE_MAX: usize = 64;
/// Bytes of held steering (or coalesced prompt) text before folding truncates
/// with a marker.
const HELD_BYTES_MAX: usize = 64 * 1024;
/// In-band disclosure that a fold dropped text. Stripped before each new fold
/// so repeated folding never stacks markers.
const FOLD_MARK: &str = "\n[… later text truncated]";

/// One forwarded unit — a single entry, or a whole causal `Group` — waiting
/// on the writer's ack (commit-protocol.md §Pipelined commits, "The actor's
/// bookkeeping").
struct PendingAck {
    ack: tokio::sync::oneshot::Receiver<std::io::Result<hotl_store::Ack>>,
    /// The projection items this unit carries, in commit order. Applied when
    /// the ack lands, in FIFO order — never on forward. A `Group` applies
    /// whole or not at all, which is what one ack for one writer message
    /// gives.
    items: Vec<Item>,
    /// Id of this unit's **last** entry — the durable leaf once applied
    /// (§Ordering authority), and what its ticket bears.
    id: String,
    /// That entry's `seq`: the epoch the published head reaches when this
    /// unit is applied.
    seq: u64,
    /// Resolved when this unit is the last of its proposal: one ticket per
    /// proposal. `None` for interior entries and for every actor-originated
    /// append.
    ticket: Option<tokio::sync::oneshot::Sender<Result<crate::CommitAck, crate::CommitFailed>>>,
}

/// The projection head the actor publishes (commit-protocol.md §Read
/// invariant, amendment 2): epoch-fenced, published **only post-ack**, and
/// read by turn tasks through a `watch::Receiver` they cannot write. The
/// actor is the sole publisher — the `watch::Sender` never leaves [`Head`],
/// which only [`run`] owns.
pub struct ProjectionHead {
    /// The durable projection: exactly the items the log carries.
    items: Arc<Vec<Item>>,
    /// The `todo_write` checklist — ephemeral session context that is never
    /// canon and never an entry in `items`. It rides the head rather than
    /// being stitched into it, so the published value stays *the durable
    /// projection*; the stitch happens per read, in [`Self::snapshot`], the
    /// same "ephemeral, request-only" shape the MOIM turn-context block has.
    /// A todo change is itself a durable `Todos` entry, so it moves
    /// `leaf`/`epoch` like any other commit.
    todos: Arc<Vec<Todo>>,
    /// Id of the newest entry applied — the **durable leaf** (§Ordering
    /// authority), and what an optimistic dispatch's `expected_leaf` is
    /// compared against. `None` before anything is applied.
    leaf: Option<String>,
    /// `seq` of that entry: the session-global commit order, which is what
    /// makes `wait_for(|p| p.epoch >= my_ack_seq)` well-formed.
    epoch: u64,
}

impl ProjectionHead {
    /// The durable leaf: id of the newest entry applied.
    pub fn leaf(&self) -> Option<&str> {
        self.leaf.as_deref()
    }

    /// `seq` of that entry — the session-global commit order.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The snapshot a turn task samples against: the durable projection,
    /// plus the todo reminder appended as the last item when the list is
    /// non-empty. The reminder rides *this* returned `Arc`, never `items`
    /// itself, so it can never be committed, replayed, or double-counted
    /// (each call starts fresh from the durable projection). The empty-list
    /// path returns the same `Arc` the head already holds — no allocation on
    /// the common case, which is what keeps "Vec with clone at grant" the
    /// cost model §Read invariant priced.
    pub fn snapshot(&self) -> Arc<Vec<Item>> {
        match hotl_tools::todo::render_reminder(&self.todos) {
            Some(reminder) => {
                let mut with_reminder = (*self.items).clone();
                with_reminder.push(reminder);
                Arc::new(with_reminder)
            }
            None => Arc::clone(&self.items),
        }
    }
}

/// The head channel, created before the actor so a `SessionHandle` can hand
/// out readers immediately (see [`crate::SessionHandle::head`]). The
/// `Sender` goes to the actor and never leaves it.
pub(crate) fn head_channel() -> (
    tokio::sync::watch::Sender<Arc<ProjectionHead>>,
    tokio::sync::watch::Receiver<Arc<ProjectionHead>>,
) {
    tokio::sync::watch::channel(Arc::new(ProjectionHead {
        items: Arc::new(Vec::new()),
        todos: Arc::new(Vec::new()),
        leaf: None,
        epoch: 0,
    }))
}

/// The actor's side of the published head: the live projection plus the one
/// `watch::Sender` in the process. Every projection advance goes through
/// here, which is what makes "publishes only post-ack, in ack order" a
/// property of one type rather than of every call site.
struct Head {
    tx: tokio::sync::watch::Sender<Arc<ProjectionHead>>,
    items: Arc<Vec<Item>>,
    todos: Arc<Vec<Todo>>,
    leaf: Option<String>,
    epoch: u64,
}

impl Head {
    /// Seed the head with the session's starting projection and publish it,
    /// so the very first read (a resumed session's `Continue`, or `fork`)
    /// never sees the empty placeholder [`head_channel`] created.
    fn new(
        tx: tokio::sync::watch::Sender<Arc<ProjectionHead>>,
        items: Vec<Item>,
        todos: Vec<Todo>,
    ) -> Self {
        let mut head = Self {
            tx,
            items: Arc::new(items),
            todos: Arc::new(todos),
            leaf: None,
            epoch: 0,
        };
        head.publish();
        head
    }

    fn items(&self) -> &Arc<Vec<Item>> {
        &self.items
    }

    fn todos(&self) -> &Arc<Vec<Todo>> {
        &self.todos
    }

    /// Apply one acked item. Deliberately does NOT publish: a commit and the
    /// steer released behind it must reach the head as one visible step (see
    /// `release_steers`), or a turn's refresh could observe the commit
    /// without the steer that overtook it.
    fn apply(&mut self, item: Item) {
        Arc::make_mut(&mut self.items).push(item);
    }

    /// Record the durable leaf and epoch an acked unit advanced the head to.
    fn advance(&mut self, leaf: String, seq: u64) {
        self.leaf = Some(leaf);
        self.epoch = seq;
    }

    fn set_todos(&mut self, todos: Vec<Todo>) {
        self.todos = Arc::new(todos);
    }

    /// Re-point the projection after a fold. The published head moves without
    /// an epoch bump: the compaction entry's own commit already advanced
    /// `leaf`/`epoch`, and this is that entry taking effect. Unobservable
    /// mid-turn by construction — the actor terminates a turn *before*
    /// committing a compaction (§Read invariant).
    fn repoint(&mut self, items: Vec<Item>) {
        self.items = Arc::new(items);
        self.publish();
    }

    /// The one publish site. `watch::Sender::send` never fails here: the
    /// actor holds a receiver for the whole session inside `SharedDeps`.
    fn publish(&mut self) {
        let _ = self.tx.send(Arc::new(ProjectionHead {
            items: Arc::clone(&self.items),
            todos: Arc::clone(&self.todos),
            leaf: self.leaf.clone(),
            epoch: self.epoch,
        }));
    }
}

/// Why a held steer may land right now. There is no third caller, and that
/// is the whole of the protection the `ProposePrepared` handler's old
/// `sampling` flip gave: a steer is only ever appended at a boundary the
/// actor itself just created, so it can never precede an assistant item that
/// could not have seen it (72a6f1b).
#[derive(Clone, Copy, Debug)]
enum Boundary {
    /// A commit this turn made just landed and was applied to the head.
    CommitSettled,
    /// The turn is over; nothing will answer an open batch now.
    TurnEnded,
}

/// How a drain settles the tickets it resolves.
#[derive(Clone, Copy)]
enum Resolution {
    /// The ordinary path: the ticket reports its byte offset.
    Ack,
    /// The conflict table's Abort arm: the projection still advances over
    /// every entry the drain lands (the bytes are canon), but the turn's
    /// claim on them is discarded (commit-protocol.md §conflict table, step
    /// 3 then step 4).
    Abort,
}

/// The actor's mirror of the writer's queue (commit-protocol.md §Pipelined
/// commits). **Bookkeeping, not semantic state** (§Tripwire re-check): it
/// holds only entries already minted and already forwarded, it is fully
/// derivable from what the actor has sent, and no rule consults it as a
/// decision input — the conflict table reads the queued leaf, which is its
/// tail (and lives in `SessionLog::last_id`, not here).
///
/// Depth is bounded by the turn-side `ACK_WINDOW`: a turn may hold at most
/// that many unresolved tickets before it must wait on the oldest, and every
/// actor-originated append drains this first.
#[derive(Default)]
struct Pipeline {
    fifo: VecDeque<PendingAck>,
    /// Global commit order across the session (§Ordering authority),
    /// assigned at validation so a ticket carries it eagerly.
    ///
    /// In memory only. The shipped entry envelope has no `seq` field and
    /// golden byte-stability is defined over the parent chain exclusively,
    /// so writing one would be a wire-format change this revision does not
    /// make — and §Ordering authority already forbids a projector from
    /// consulting `seq` at all ("audit and debugging only").
    seq: u64,
}

impl Pipeline {
    fn is_empty(&self) -> bool {
        self.fifo.is_empty()
    }

    /// Assign the next commit order, at the mint that is about to happen.
    ///
    /// **Every** commit the actor makes calls this — `Sync` proposals,
    /// `Pipelined` proposals and actor-originated inline appends alike —
    /// because §Ordering authority defines `seq` as the global commit order
    /// across the whole session, not a count of the pipelined subset. A
    /// counter that skipped the other paths would also make S2c's watch
    /// predicate (`epoch >= my_ack_seq`, where `epoch` is the seq of the
    /// newest entry applied to the published head) compare two different
    /// numberings.
    ///
    /// Assigned at validation, before the write, so a ticket carries it
    /// eagerly — which means a mint the writer then refuses burns its
    /// number. That is harmless: the head simply never stops on it, and
    /// `seq` is monotonic either way.
    ///
    /// Within a proposal it is assigned per entry in proposal order, so the
    /// run is contiguous and `seq` order still equals disk order.
    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    /// Resolve every outstanding ack, in order — the barrier the actor runs
    /// before any inline append and the conflict table's step (3).
    ///
    /// INVARIANT: the projection advances strictly in ack order, and only on
    /// an ack. Enforced by `the_pipeline_advances_the_projection_in_fifo_order`
    /// and, end to end, by
    /// `a_writer_death_before_fsync_never_resolves_a_pipelined_ticket`.
    async fn drain(&mut self, head: &mut Head, resolution: Resolution) {
        while !self.is_empty() {
            // The borrow ends here, so the pop below is legal — and dropping
            // this future mid-await loses nothing: the receiver stays in the
            // FIFO for the next drain.
            let acked = {
                let entry = self.fifo.front_mut().expect("just checked non-empty");
                await_ack(&mut entry.ack).await
            };
            let entry = self.fifo.pop_front().expect("just checked non-empty");
            let settled = apply_ack(entry, acked, head, resolution);
            head.publish();
            settled.resolve();
        }
    }
}

/// What woke the actor's select loop.
enum Woke {
    Ack(std::io::Result<hotl_store::Ack>),
    Cmd(Option<SessionCmd>),
}

/// The oldest pending ack, or a future that never resolves when there is
/// none — the loop's ack arm is only live while the FIFO is non-empty.
async fn next_ack(front: &mut Option<PendingAck>) -> std::io::Result<hotl_store::Ack> {
    match front {
        Some(entry) => await_ack(&mut entry.ack).await,
        None => std::future::pending().await,
    }
}

/// The writer's answer for one forwarded entry; a dropped sender means the
/// writer died before it could ack (the SIGKILL case), which is never an ack.
async fn await_ack(
    ack: &mut tokio::sync::oneshot::Receiver<std::io::Result<hotl_store::Ack>>,
) -> std::io::Result<hotl_store::Ack> {
    match ack.await {
        Ok(result) => result,
        Err(_) => Err(std::io::Error::other(
            "the log writer stopped before the entry was committed",
        )),
    }
}

/// A ticket that has been decided but not yet told. Held between the *apply*
/// and the *resolve* halves of commit-protocol.md §Read invariant's
/// "apply → publish → resolve" so the publish can sit between them — and so
/// a steer released at the same boundary joins the same published head.
#[must_use = "a decided ticket must be resolved, or its proposer waits forever"]
struct Settled {
    ticket: Option<tokio::sync::oneshot::Sender<Result<crate::CommitAck, crate::CommitFailed>>>,
    resolved: Result<crate::CommitAck, crate::CommitFailed>,
}

impl Settled {
    fn resolve(self) {
        if let Some(ticket) = self.ticket {
            let _ = ticket.send(self.resolved);
        }
    }
}

/// The *apply* half: fold one acked unit (entry or whole `Group`) into the
/// head. The caller publishes, then resolves — in that order, which is what
/// makes the turn's `epoch >= my_ack_seq` predicate a gate rather than a
/// race.
fn apply_ack(
    entry: PendingAck,
    acked: std::io::Result<hotl_store::Ack>,
    head: &mut Head,
    resolution: Resolution,
) -> Settled {
    let resolved = match acked {
        Ok(ack) => {
            for item in entry.items {
                head.apply(item);
            }
            head.advance(entry.id, entry.seq);
            match resolution {
                Resolution::Ack => Ok(crate::CommitAck { offset: ack.offset }),
                Resolution::Abort => Err(crate::CommitFailed::Aborted),
            }
        }
        // Nothing was acked, so the projection must not advance: a crash may
        // leave the log ahead of the projection, never the reverse.
        Err(_) => Err(crate::CommitFailed::LogSealed),
    };
    Settled {
        ticket: entry.ticket,
        resolved,
    }
}

/// Dependencies shared with turn tasks. The log is *not* here: only the actor
/// loop writes it, so it lives as a local in [`run`].
pub(crate) struct SharedDeps {
    pub provider: Arc<dyn Provider>,
    pub registry: Arc<Registry>,
    pub rules: Arc<Rules>,
    /// The session's *current effective* permission mode — separate from
    /// `rules.mode()` (the startup default) so `SetMode` can flip it without
    /// reallocating `Rules` (task 4: mode moves, `Rules` stays a plain
    /// cheap-to-share value). Seeded from `rules.mode()` at session start.
    mode: AtomicU8,
    pub sandbox_enforced: bool,
    pub clock: Arc<dyn Clock>,
    pub system: Arc<str>,
    pub cwd: PathBuf,
    pub config: EngineConfig,
    pub snapshots: Option<Arc<dyn crate::Snapshotter>>,
    pub hooks: Option<Arc<dyn crate::hooks::Hooks>>,
    /// §S1 HookRouter gate (Task 5): the union of event kinds `hooks`
    /// actually wants dispatched, read by every `hook_gate!` call site as
    /// one atomic load — never a fresh `Hooks::event_mask` dyn call — to
    /// decide whether to build ANY per-event work. `NONE` when `hooks` is
    /// `None`. Prefers `hooks.mask_handle()` — the SAME cell the impl
    /// narrows on eviction, so a mid-session narrowing is visible here
    /// immediately (reviewer finding: a one-time snapshot copy would go
    /// stale the moment the impl's own state changed). Falls back to a
    /// fresh `Arc` seeded from a single `event_mask()` call when the impl
    /// has no live handle to offer (the trait's default `None`) — the same
    /// "compute once at session start" shape `mode` uses for `rules.mode()`.
    hook_mask: Arc<AtomicU8>,
    /// The session-scoped `notify` drain (Finding 1 fix) — shared with
    /// whatever built this session's `SessionHandle`, so the CLI's exit-time
    /// drain call reaches the exact same detached `Notification` hook tasks
    /// this actor (and any `question_sink`) spawns.
    pub notifications: crate::hooks::NotificationDrain,
    /// The same masker the log's inline path uses (commit-protocol.md
    /// §Proposal payloads: "there is no second masking policy — only a
    /// second, cheap, caller") — cloned from `SessionLog`'s own `Arc<Masker>`
    /// at construction, so proposal build in the turn task masks under
    /// exactly the rules the actor would have used.
    masker: Arc<hotl_store::Masker>,
    /// Monotonic masking-rules epoch (commit-protocol.md §Proposal
    /// payloads' `rules_epoch` guard). Today masking rules never change
    /// mid-session, so this is constant for the life of a `SharedDeps` — the
    /// guard is implemented anyway, ahead of whatever eventually bumps it.
    rules_epoch: std::sync::atomic::AtomicU32,
    /// The read side of the published head (commit-protocol.md §Read
    /// invariant): a turn's sample-boundary refresh. Only a `Receiver` is
    /// shared — the `Sender` lives in [`run`]'s [`Head`], so the actor stays
    /// the sole publisher by construction rather than by convention.
    head_rx: tokio::sync::watch::Receiver<Arc<ProjectionHead>>,
}

/// `PermissionMode` has no natural discriminant to lean on across an atomic
/// (and shouldn't grow one just for this) — a tiny, exhaustively-matched
/// codec keeps the two in lockstep instead.
fn mode_to_u8(mode: PermissionMode) -> u8 {
    match mode {
        PermissionMode::Ask => 0,
        PermissionMode::Auto => 1,
        PermissionMode::Plan => 2,
        PermissionMode::DontAsk => 3,
    }
}

fn u8_to_mode(v: u8) -> PermissionMode {
    match v {
        1 => PermissionMode::Auto,
        2 => PermissionMode::Plan,
        3 => PermissionMode::DontAsk,
        _ => PermissionMode::Ask,
    }
}

impl SharedDeps {
    fn new(
        deps: SessionDeps,
        notifications: crate::hooks::NotificationDrain,
        head_rx: tokio::sync::watch::Receiver<Arc<ProjectionHead>>,
    ) -> (Self, SessionLog) {
        let mode = AtomicU8::new(mode_to_u8(deps.rules.mode()));
        let hook_mask = deps
            .hooks
            .as_ref()
            .and_then(|h| h.mask_handle())
            .unwrap_or_else(|| {
                Arc::new(AtomicU8::new(
                    deps.hooks
                        .as_ref()
                        .map_or(crate::hooks::EventMask::NONE, |h| h.event_mask())
                        .bits(),
                ))
            });
        // Cloned before `deps.log` moves out below — cheap (an `Arc` bump),
        // and it's how a turn-side `prepare_payload` call ends up masking
        // under the exact same rules the log's own inline path uses.
        let masker = deps.log.masker_handle();
        let shared = Self {
            provider: deps.provider,
            registry: deps.registry,
            rules: deps.rules,
            mode,
            sandbox_enforced: deps.sandbox_enforced,
            clock: deps.clock,
            system: deps.system.into(),
            cwd: deps.cwd,
            config: deps.config,
            snapshots: deps.snapshots,
            hooks: deps.hooks,
            hook_mask,
            notifications,
            masker,
            rules_epoch: std::sync::atomic::AtomicU32::new(0),
            head_rx,
        };
        (shared, deps.log)
    }

    /// A turn's read side of the published head — see the `head_rx` field
    /// doc. Cloned per turn task; `watch::Receiver` clones observe the same
    /// value, so this grants no second source of truth.
    pub(crate) fn head(&self) -> tokio::sync::watch::Receiver<Arc<ProjectionHead>> {
        self.head_rx.clone()
    }

    /// The live §S1 mask [`crate::hooks::hook_gate!`] branches on — see the
    /// `hook_mask` field doc.
    pub(crate) fn hook_mask(&self) -> crate::hooks::EventMask {
        crate::hooks::mask_of(&self.hook_mask)
    }

    /// The mode `evaluate` should gate against right now — not necessarily
    /// `rules.mode()`, which is only ever the startup default.
    pub(crate) fn effective_mode(&self) -> PermissionMode {
        u8_to_mode(self.mode.load(Ordering::Relaxed))
    }

    /// Runtime mode-mutation entry point (`SessionCmd::SetMode`, reachable
    /// via ACP `session/set_mode` and the TUI `/mode` command). Routes
    /// through [`hotl_tools::rules::enforced_mode`] — the same coercion
    /// `Rules::with_mode` applies at startup — so a `security-enforced`
    /// build can't be flipped to `Auto` mid-session by a client request.
    /// Returns the mode actually stored (post-coercion) so the caller logs
    /// the durable `ModeSet` entry with what really took effect, not the
    /// raw request.
    fn set_mode(&self, mode: PermissionMode) -> PermissionMode {
        let mode = hotl_tools::rules::enforced_mode(mode);
        self.mode.store(mode_to_u8(mode), Ordering::Relaxed);
        mode
    }

    /// Commit one entry: forward it to the writer at the `Durable` tier and
    /// await the ack ("Writer fsyncs, acks with the byte offset" —
    /// commit-protocol.md §Durability ordering). `false` = the log is sealed,
    /// and the caller must NOT advance the projection.
    ///
    /// INVARIANT: the projection only ever advances after this returns `true`,
    /// so a crash can leave the log ahead of the projection but never the
    /// reverse. Enforced by
    /// `a_writer_death_before_fsync_never_leaves_the_projection_ahead_of_the_log`.
    ///
    /// The failure surfaces to the user via the turn outcome, not stderr.
    ///
    /// The pipeline is drained first, always: this entry's ack sits *behind*
    /// everything already forwarded, so projecting it before those would
    /// invert the ack order the whole design rests on.
    ///
    /// The payload is taken **by value** and its item (if any) applied here,
    /// so the head advances — and publishes — in exactly one place per
    /// commit path. A caller that appended and then pushed for itself could
    /// leave the published head one entry behind the projection it names.
    async fn append(
        &self,
        log: &mut SessionLog,
        pipeline: &mut Pipeline,
        head: &mut Head,
        payload: EntryPayload,
    ) -> bool {
        pipeline.drain(head, Resolution::Ack).await;
        let seq = pipeline.next_seq();
        if log
            .append_acked(&payload, self.clock.now_ms())
            .await
            .is_err()
        {
            return false;
        }
        if let EntryPayload::Item { item } = payload {
            head.apply(item);
        }
        // `append_acked` only advances the chain on a committed entry, so
        // this is the id of the line that just landed.
        if let Some(id) = log.last_id() {
            head.advance(id.to_string(), seq);
        }
        head.publish();
        true
    }

    /// The masker turn-side proposal build masks under
    /// ([`crate::turn`]'s `prepare_entry`) — see the `masker` field doc.
    pub(crate) fn masker(&self) -> &Arc<hotl_store::Masker> {
        &self.masker
    }

    /// The current masking-rules epoch — see the `rules_epoch` field doc.
    pub(crate) fn rules_epoch(&self) -> u32 {
        self.rules_epoch.load(Ordering::Relaxed)
    }

    /// Commit one already-prepared entry: no serialization, no masking here
    /// (commit-protocol.md §Proposal payloads) — splice and forward only.
    /// Mirrors `append`'s durable-tier/await-ack shape.
    async fn append_prepared(
        &self,
        log: &mut SessionLog,
        prepared: hotl_store::PreparedPayload,
    ) -> bool {
        log.append_prepared(prepared, self.clock.now_ms())
            .await
            .is_ok()
    }

    /// The `Pipelined` half: mint, splice, forward, and hand the ack channel
    /// to the actor's FIFO — no await (commit-protocol.md §Durability
    /// ordering, step 3 answering before step 5).
    fn forward_prepared(
        &self,
        log: &mut SessionLog,
        prepared: hotl_store::PreparedPayload,
    ) -> std::io::Result<hotl_store::Forwarded> {
        log.forward_prepared(prepared, self.clock.now_ms())
    }

    /// The `Group` half of the same (commit-protocol.md §Causal groups): one
    /// writer message, one `sync_data`, one ack bearing the group's last
    /// entry id.
    fn forward_group(
        &self,
        log: &mut SessionLog,
        group: Vec<hotl_store::PreparedPayload>,
    ) -> std::io::Result<hotl_store::Forwarded> {
        log.forward_group(group, self.clock.now_ms())
    }
}

pub(crate) async fn run(
    mut deps: SessionDeps,
    mut cmd_rx: mpsc::Receiver<SessionCmd>,
    cmd_tx: mpsc::WeakSender<SessionCmd>,
    events: mpsc::Sender<EngineEvent>,
    current_turn: Arc<Mutex<CancellationToken>>,
    notifications: crate::hooks::NotificationDrain,
    head_tx: tokio::sync::watch::Sender<Arc<ProjectionHead>>,
) {
    // Resumed history is repaired on the way in: a log written by a build that
    // let a steer land mid-batch would otherwise fail every request forever.
    //
    // The `todo_write` checklist (M4/tier-1 gap #3) is actor-owned, ephemeral
    // session context: it never lives in the durable projection — it is
    // stitched onto a *read* of the head only (`ProjectionHead::snapshot`),
    // the same "ephemeral, request-only" shape as the MOIM turn-context
    // block. A resumed session seeds it from its replayed `Todos` entry
    // (`SessionDeps::initial_todos`); a fresh session starts empty.
    let head_rx = head_tx.subscribe();
    let mut head = Head::new(
        head_tx,
        pair_tool_results(std::mem::take(&mut deps.initial_items)),
        std::mem::take(&mut deps.initial_todos),
    );
    let mut running = false;
    let mut queue: VecDeque<(String, Option<SyntheticReason>)> = VecDeque::new();
    // Steers that arrived while a turn was live (or a tool batch open),
    // waiting for a boundary before they can be appended.
    let mut held_steers: Vec<String> = Vec::new();
    let (shared, mut log) = SharedDeps::new(deps, notifications, head_rx);
    let shared = Arc::new(shared);
    // Usage carried across compaction respawns within one logical turn.
    let mut carry_usage = TokenUsage::default();
    let mut compact_streak: u32 = 0;
    let mut pipeline = Pipeline::default();

    loop {
        // The oldest pending ack is held out of the FIFO for the duration of
        // the select, so the ack future borrows a local rather than the
        // pipeline every command handler also needs. It goes straight back
        // when a command wins the race — before any handler runs, so a
        // handler's own drain still sees a whole FIFO.
        let mut front = pipeline.fifo.pop_front();
        // Acks are polled first: the projection is what every command reads,
        // and the FIFO is bounded, so this can never starve the mailbox.
        let woke = tokio::select! {
            biased;
            acked = next_ack(&mut front) => Woke::Ack(acked),
            cmd = cmd_rx.recv() => Woke::Cmd(cmd),
        };
        let cmd = match woke {
            Woke::Ack(acked) => {
                let entry = front.expect("the ack arm only runs with a front entry");
                let settled = apply_ack(entry, acked, &mut head, Resolution::Ack);
                // A commit this turn made just landed: that is the boundary a
                // held steer was waiting for. It is appended BEFORE the
                // publish below, so a turn's refresh sees the commit and the
                // steer as one step — which is what turns a steer at the
                // boundary into a clean adoption miss instead of a race
                // (commit-protocol.md §Causal groups, the adoption rule).
                release_steers(
                    &shared,
                    &mut log,
                    &mut head,
                    &mut pipeline,
                    &mut held_steers,
                    Boundary::CommitSettled,
                )
                .await;
                head.publish();
                settled.resolve();
                continue;
            }
            Woke::Cmd(cmd) => {
                if let Some(entry) = front {
                    pipeline.fifo.push_front(entry);
                }
                match cmd {
                    Some(cmd) => cmd,
                    None => break,
                }
            }
        };
        match cmd {
            SessionCmd::Prompt(text) => {
                running = admit_prompt(
                    &shared,
                    &mut log,
                    &mut head,
                    &mut pipeline,
                    &mut queue,
                    running,
                    text,
                    None,
                    &cmd_tx,
                    &events,
                    &current_turn,
                )
                .await;
            }
            SessionCmd::PromptTagged { text, synthetic } => {
                running = admit_prompt(
                    &shared,
                    &mut log,
                    &mut head,
                    &mut pipeline,
                    &mut queue,
                    running,
                    text,
                    Some(synthetic),
                    &cmd_tx,
                    &events,
                    &current_turn,
                )
                .await;
            }
            SessionCmd::Continue => {
                if !running && crate::needs_continuation(head.items()) {
                    spawn_turn(&shared, &cmd_tx, &events, &current_turn);
                    running = true;
                }
            }
            SessionCmd::Steer(text) => {
                admit_steer(
                    &shared,
                    &mut log,
                    &mut head,
                    &mut pipeline,
                    &mut held_steers,
                    running,
                    text,
                )
                .await
            }
            SessionCmd::Rename(name) => {
                let _ = shared
                    .append(
                        &mut log,
                        &mut pipeline,
                        &mut head,
                        EntryPayload::Rename { name },
                    )
                    .await;
            }
            SessionCmd::SetMode(mode) => {
                // Effective immediately: the atomic, not a rebuilt `Rules`,
                // is what `evaluate` reads. The durable entry is what lets
                // `hotl resume` restore it, exactly like `Rename`/name — it
                // records the post-coercion mode, so a security-enforced
                // build's log never claims `auto` while it actually ran
                // `ask`.
                let mode = shared.set_mode(mode);
                let _ = shared
                    .append(
                        &mut log,
                        &mut pipeline,
                        &mut head,
                        EntryPayload::ModeSet {
                            mode: mode.as_str().into(),
                        },
                    )
                    .await;
            }
            SessionCmd::SetTodos(new_todos) => {
                head.set_todos(new_todos);
                let items = (**head.todos()).clone();
                let _ = shared
                    .append(
                        &mut log,
                        &mut pipeline,
                        &mut head,
                        EntryPayload::Todos {
                            items: items.clone(),
                        },
                    )
                    .await;
                let _ = events.send(EngineEvent::TodosChanged { items }).await;
            }
            SessionCmd::Propose { entries, reply } => {
                let committed = commit(&shared, &mut log, &mut head, &mut pipeline, entries).await;
                let _ = reply.send(committed);
            }
            SessionCmd::ProposePrepared {
                proposal,
                mode,
                reply,
            } => {
                let result =
                    commit_prepared(&shared, &mut log, &mut head, &mut pipeline, proposal, mode)
                        .await;
                // `Sync` applies inline, so the boundary a held steer waits
                // for is *here*; `Pipelined` reaches it at the ack, in the
                // loop's FIFO arm above. A stale-epoch reject is not a
                // boundary at all — nothing was committed and the turn is
                // about to resend the SAME entries, so releasing here would
                // land the steer BEFORE the retried assistant blocks it was
                // held to avoid preceding (72a6f1b).
                if mode == crate::AckMode::Sync
                    && !matches!(result, crate::ProposeReply::StaleEpoch)
                {
                    release_steers(
                        &shared,
                        &mut log,
                        &mut head,
                        &mut pipeline,
                        &mut held_steers,
                        Boundary::CommitSettled,
                    )
                    .await;
                }
                let _ = reply.send(result);
            }
            SessionCmd::WriteBlob {
                tool_use_id,
                content,
                reply,
            } => {
                let result = match log.write_blob_acked(&tool_use_id, &content).await {
                    Ok(path) => Ok(path.display().to_string()),
                    Err(_) => Err(content), // hand the content back — never lose it
                };
                let _ = reply.send(result);
            }
            SessionCmd::TurnFinished { end, usage } => {
                // The turn is over, so nothing will answer an open batch now.
                // Close it, then let held steers land before a queued prompt
                // starts the next turn behind them.
                // A turn that died mid-sample must not strand its steers.
                close_open_batch(&shared, &mut log, &mut head, &mut pipeline).await;
                release_steers(
                    &shared,
                    &mut log,
                    &mut head,
                    &mut pipeline,
                    &mut held_steers,
                    Boundary::TurnEnded,
                )
                .await;
                on_turn_finished(
                    TurnFinishedCtx {
                        shared: &shared,
                        log: &mut log,
                        head: &mut head,
                        pipeline: &mut pipeline,
                        queue: &mut queue,
                        running: &mut running,
                        carry_usage: &mut carry_usage,
                        compact_streak: &mut compact_streak,
                        cmd_tx: &cmd_tx,
                        events: &events,
                        current_turn: &current_turn,
                    },
                    end,
                    usage,
                )
                .await;
            }
            SessionCmd::BumpRulesEpoch => {
                shared.rules_epoch.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    // SessionEnd (Finding 1 fix): AWAITED, not fire-and-forget — the command
    // channel closed (every `SessionHandle`/turn task dropped its sender), so
    // this actor is shutting down for good and nothing needs it responsive
    // any more. Blocking here (bounded by `call_session_end`'s own timeout)
    // is what actually guarantees the hook runs to completion: this task is
    // itself the one `SessionHandle::finish` awaits before the one-shot
    // CLI's `block_on` drops its runtime, so a detached spawn here would
    // just move the same race somewhere else.
    // §S1 HookRouter gate: a masked-off (or hook-less) session skips the
    // call (and its timeout registration) entirely.
    crate::hooks::hook_gate!(
        shared.hooks,
        shared.hook_mask(),
        crate::hooks::EventMask::SESSION_END,
        |hooks| {
            crate::hooks::call_session_end(hooks).await;
        },
        else {}
    );
}

/// The mutable session state `on_turn_finished` threads back into the loop.
struct TurnFinishedCtx<'a> {
    shared: &'a Arc<SharedDeps>,
    log: &'a mut SessionLog,
    head: &'a mut Head,
    pipeline: &'a mut Pipeline,
    queue: &'a mut VecDeque<(String, Option<SyntheticReason>)>,
    running: &'a mut bool,
    carry_usage: &'a mut TokenUsage,
    compact_streak: &'a mut u32,
    cmd_tx: &'a mpsc::WeakSender<SessionCmd>,
    events: &'a mpsc::Sender<EngineEvent>,
    current_turn: &'a Arc<Mutex<CancellationToken>>,
}

/// A turn ended: either report it (and promote the queue) or, on a compaction
/// request, fold and respawn the continuation.
async fn on_turn_finished(ctx: TurnFinishedCtx<'_>, end: TurnEnd, mut usage: TokenUsage) {
    let outcome = match end {
        TurnEnd::Outcome(outcome) => Some(outcome),
        TurnEnd::Compact { spec, cont } => {
            *ctx.carry_usage += usage;
            usage = TokenUsage::default();
            try_compact(
                ctx.shared,
                ctx.log,
                ctx.head,
                ctx.pipeline,
                ctx.compact_streak,
                spec,
                cont,
                ctx.cmd_tx,
                ctx.events,
                ctx.current_turn,
            )
            .await
        }
    };
    if let Some(outcome) = outcome {
        *ctx.compact_streak = 0;
        let mut total = usage;
        total += std::mem::take(ctx.carry_usage);
        *ctx.running = end_turn(
            ctx.shared,
            ctx.log,
            ctx.head,
            ctx.pipeline,
            ctx.queue,
            outcome,
            total,
            ctx.cmd_tx,
            ctx.events,
            ctx.current_turn,
        )
        .await;
    }
}

/// One compaction attempt on behalf of a turn that hit the threshold: fold,
/// announce, respawn the continuation. `Some(outcome)` means compaction can't
/// proceed (streak cap, nothing to fold, sealed log) and the turn ends.
#[allow(clippy::too_many_arguments)]
async fn try_compact(
    shared: &Arc<SharedDeps>,
    log: &mut SessionLog,
    head: &mut Head,
    pipeline: &mut Pipeline,
    compact_streak: &mut u32,
    spec: Option<crate::SpecDigest>,
    cont: Box<crate::TurnContinuation>,
    cmd_tx: &mpsc::WeakSender<SessionCmd>,
    events: &mpsc::Sender<EngineEvent>,
    current_turn: &Arc<Mutex<CancellationToken>>,
) -> Option<Outcome> {
    // INVARIANT: the streak counts folds with no intervening completed sample
    // — a long, productive turn folds as often as it needs to, and only a
    // fold-the-digest spiral (no progress between folds) trips the cap.
    // Enforced by `three_folds_with_progress_do_not_exhaust_the_streak`.
    if cont.samples_since_compact > 0 {
        *compact_streak = 0;
    }
    *compact_streak += 1;
    // The token interrupt() cancels right now belongs to the turn that just
    // ended with `Compact`. Honor it through the whole compaction window —
    // race the inline summarize against it, and hand the *same* token to the
    // continuation — so an interrupt anywhere in the window ends the logical
    // turn instead of being silently swallowed.
    let cancel = current_turn
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let compacted = if *compact_streak > MAX_COMPACT_STREAK {
        Err("context window exhausted — compaction can no longer make room".into())
    } else {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Some(Outcome::Cancelled),
            compacted = compact(shared, log, head, pipeline, spec) => compacted,
        }
    };
    match compacted {
        Ok(degraded) => {
            let _ = events.send(EngineEvent::Compacted { degraded }).await;
            if cancel.is_cancelled() {
                return Some(Outcome::Cancelled);
            }
            respawn_turn(shared, cmd_tx, events, cancel, *cont);
            None // still running: same logical turn continues
        }
        Err(message) => Some(Outcome::Error { message }),
    }
}

/// Annotate + report a finished turn, then promote the next queued prompt.
/// Returns whether a turn is (still) running.
#[allow(clippy::too_many_arguments)]
async fn end_turn(
    shared: &Arc<SharedDeps>,
    log: &mut SessionLog,
    head: &mut Head,
    pipeline: &mut Pipeline,
    queue: &mut VecDeque<(String, Option<SyntheticReason>)>,
    outcome: Outcome,
    usage: TokenUsage,
    cmd_tx: &mpsc::WeakSender<SessionCmd>,
    events: &mpsc::Sender<EngineEvent>,
    current_turn: &Arc<Mutex<CancellationToken>>,
) -> bool {
    annotate(shared, log, head, pipeline, &outcome).await;
    // Notification: the turn completed — fire-and-forget, computed before
    // `outcome` moves into the event below. §S1 HookRouter gate: masked-off
    // (or hook-less) skips even the `outcome_detail` computation.
    crate::hooks::hook_gate!(
        shared.hooks,
        shared.hook_mask(),
        crate::hooks::EventMask::NOTIFICATION,
        |hooks| {
            crate::hooks::notify(
                hooks,
                &shared.notifications,
                crate::hooks::NotificationKind::Done,
                outcome_detail(&outcome),
            );
        },
        else {}
    );
    let _ = events.send(EngineEvent::TurnDone { outcome, usage }).await;
    match queue.pop_front() {
        Some((next, synthetic)) => {
            start_turn(
                shared,
                log,
                head,
                pipeline,
                next,
                synthetic,
                cmd_tx,
                events,
                current_turn,
            )
            .await
        }
        None => {
            // Notification: nothing queued behind it — the session goes
            // idle awaiting the next prompt.
            crate::hooks::hook_gate!(
                shared.hooks,
                shared.hook_mask(),
                crate::hooks::EventMask::NOTIFICATION,
                |hooks| {
                    crate::hooks::notify(
                        hooks,
                        &shared.notifications,
                        crate::hooks::NotificationKind::Idle,
                        "awaiting a prompt",
                    );
                },
                else {}
            );
            false
        }
    }
}

/// A short human-readable rendering of an outcome for `Notification` hooks
/// (a `hotl watch`/desktop consumer, not a protocol payload — free-form text
/// is fine).
fn outcome_detail(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Done { text } => text.clone(),
        other => format!("{other:?}"),
    }
}

/// Append `text` to `dst`, keeping the *head* and disclosing any truncation
/// in-band. Dropping a user's words silently is worse than telling the model
/// some were dropped, and an unbounded buffer is worse than both.
/// INVARIANT: neither the prompt queue nor the held-steer buffer grows without
/// bound. Enforced by `folding_bounds_the_buffer_and_discloses_the_truncation`.
fn fold_into(dst: &mut String, text: &str, max_bytes: usize) {
    if dst.ends_with(FOLD_MARK) {
        dst.truncate(dst.len() - FOLD_MARK.len());
    }
    if !dst.is_empty() {
        dst.push_str("\n\n");
    }
    dst.push_str(text);
    if dst.len() > max_bytes {
        let mut end = max_bytes;
        while !dst.is_char_boundary(end) {
            end -= 1;
        }
        dst.truncate(end);
        dst.push_str(FOLD_MARK);
    }
}

/// Whether the projection is mid-batch: it ends on an assistant turn whose
/// tool calls have no results yet. Both APIs require those results to be the
/// very next message, so nothing else may be appended in this window.
fn awaiting_tool_results(items: &[Item]) -> bool {
    matches!(
        items.last(),
        Some(Item::Assistant { blocks }) if !hotl_types::assistant_tool_uses(blocks).is_empty()
    )
}

/// Durable admission on arrival; projection advances only after the append
/// (commit-protocol §durability). Linear-log M1 records the steer as a
/// tagged user item; the `steer_admission` entry kind arrives with M3b's tree.
///
/// Steering mid-batch is the normal case — the human reacts while a tool runs
/// — and that is precisely the window where appending would strand the batch's
/// results away from the calls they answer. Such a steer is held instead and
/// released once the results land. The model sees it at the same moment either
/// way: the next sample happens after the batch closes.
///
/// `sampling` is the same hold one step earlier: a steer that arrives while a
/// request is in flight would otherwise commit *ahead* of the assistant item
/// that request is about to produce.
/// INVARIANT: a steer never precedes an assistant item that could not have seen
/// it. Enforced by `a_mid_stream_steer_commits_after_the_reply_it_did_not_see`.
async fn admit_steer(
    shared: &SharedDeps,
    log: &mut SessionLog,
    head: &mut Head,
    pipeline: &mut Pipeline,
    held: &mut Vec<String>,
    running: bool,
    text: String,
) {
    if running || awaiting_tool_results(head.items()) {
        // Past the byte cap, coalesce into the last held steer rather than grow
        // without bound — every entry is committed at once on release.
        let total: usize = held.iter().map(String::len).sum();
        match held.last_mut() {
            Some(last) if total >= HELD_BYTES_MAX => fold_into(last, &text, HELD_BYTES_MAX),
            _ => held.push(text),
        }
        return;
    }
    append_steer(shared, log, head, pipeline, text).await;
}

async fn append_steer(
    shared: &SharedDeps,
    log: &mut SessionLog,
    head: &mut Head,
    pipeline: &mut Pipeline,
    text: String,
) {
    // The other half of the held-steer rule, as a structural guard: a steer
    // that split a tool batch would strand the results from the calls they
    // answer, and every API rejects that history outright.
    debug_assert!(
        !awaiting_tool_results(head.items()),
        "a steer must never land while a tool batch is open"
    );
    shared
        .append(
            log,
            pipeline,
            head,
            EntryPayload::Item {
                item: Item::User {
                    text,
                    synthetic: Some(SyntheticReason::Steer),
                },
            },
        )
        .await;
}

/// Append the steers that were waiting on a sample or a batch, oldest first,
/// once the reply has landed and the pairing is closed.
async fn release_steers(
    shared: &SharedDeps,
    log: &mut SessionLog,
    head: &mut Head,
    pipeline: &mut Pipeline,
    held: &mut Vec<String>,
    _at: Boundary,
) {
    if held.is_empty() || awaiting_tool_results(head.items()) {
        return;
    }
    for text in std::mem::take(held) {
        append_steer(shared, log, head, pipeline, text).await;
    }
}

/// Answer a batch nothing will answer any more. A turn that dies before it can
/// report leaves calls hanging; the next request would be rejected for the
/// missing results, so the protocol gets completed here instead.
async fn close_open_batch(
    shared: &SharedDeps,
    log: &mut SessionLog,
    head: &mut Head,
    pipeline: &mut Pipeline,
) {
    let Some(Item::Assistant { blocks }) = head.items().last() else {
        return;
    };
    let uses = hotl_types::assistant_tool_uses(blocks);
    if uses.is_empty() {
        return;
    }
    let payload = EntryPayload::Item {
        item: Item::ToolResults {
            results: uses
                .iter()
                .map(|tu| hotl_types::ToolResultItem {
                    tool_use_id: tu.id.clone(),
                    content: "Not executed (the turn ended first).".into(),
                    is_error: true,
                })
                .collect(),
        },
    };
    shared.append(log, pipeline, head, payload).await;
}

/// Restore tool_use/tool_result adjacency in history written before steers
/// were held. Items that landed in the gap move to just after the results
/// they interrupted — the order the model would have seen anyway, since the
/// gap only ever opened while a batch was still running. Nothing is dropped.
pub(crate) fn pair_tool_results(items: Vec<Item>) -> Vec<Item> {
    let mut out: Vec<Item> = Vec::with_capacity(items.len());
    // Items pulled out of an open batch, waiting to go back in behind it.
    let mut stranded: Vec<Item> = Vec::new();
    for item in items {
        if !awaiting_tool_results(&out) && stranded.is_empty() {
            out.push(item);
            continue;
        }
        match item {
            Item::ToolResults { .. } => {
                out.push(item);
                out.append(&mut stranded);
            }
            // Another assistant turn means no results were ever coming; the
            // gap was not an open batch, so leave the order as it was found.
            Item::Assistant { .. } => {
                out.append(&mut stranded);
                out.push(item);
            }
            _ => stranded.push(item),
        }
    }
    out.append(&mut stranded);
    out
}

/// Commit a proposal: append each entry durably, then project it.
async fn commit(
    shared: &SharedDeps,
    log: &mut SessionLog,
    head: &mut Head,
    pipeline: &mut Pipeline,
    entries: Vec<EntryPayload>,
) -> bool {
    for payload in entries {
        if !shared.append(log, pipeline, head, payload).await {
            return false;
        }
    }
    true
}

/// Commit a proposal of already-prepared entries (commit-protocol.md
/// §Proposal payloads): no serialization, no masking here — see `commit`
/// above for the actor-built-entries twin this mirrors. The rules-epoch
/// guard is checked once, for the whole batch, before anything is
/// appended: every entry in one turn-task proposal is built from one
/// `rules_epoch` reading (`crate::turn::Turn::propose`), so a mixed batch
/// would itself be a bug upstream, not something to partially commit around.
async fn commit_prepared(
    shared: &SharedDeps,
    log: &mut SessionLog,
    head: &mut Head,
    pipeline: &mut Pipeline,
    proposal: crate::EntryProposal,
    mode: crate::AckMode,
) -> crate::ProposeReply {
    let current_epoch = shared.rules_epoch();
    // "Predates" (commit-protocol.md §Proposal payloads), not merely
    // "differs": epoch only ever advances, and the actor is its sole owner,
    // so a proposal can never legitimately carry an epoch newer than the
    // actor's own — but an equal-or-newer stamp is never rejected either,
    // matching the spec's literal wording rather than a stricter `!=`.
    if proposal
        .entries()
        .iter()
        .any(|e| e.payload().rules_epoch() < current_epoch)
    {
        return crate::ProposeReply::StaleEpoch;
    }
    if proposal.is_empty() {
        return crate::ProposeReply::Committed;
    }
    match mode {
        // The pre-revision path, entry at a time. It is the arm the
        // revision's own counter assertions are measured against ("the same
        // session driven through `Sync` and `Pipelined` modes produces the
        // same normalized transcript", and the `Completed` boundary's 2→1
        // fsync), so it deliberately does NOT collapse a group into one
        // writer message — that collapse is exactly what is being measured.
        crate::AckMode::Sync => {
            // This proposal's acks sit behind everything already forwarded,
            // so those have to land (and project) first.
            pipeline.drain(head, Resolution::Ack).await;
            for entry in into_entries(proposal) {
                let (payload, item) = entry.into_parts();
                let seq = pipeline.next_seq();
                if !shared.append_prepared(log, payload).await {
                    return crate::ProposeReply::Sealed;
                }
                if let Some(item) = item {
                    head.apply(item);
                }
                if let Some(id) = log.last_id() {
                    head.advance(id.to_string(), seq);
                }
                head.publish();
            }
            crate::ProposeReply::Committed
        }
        // Validate → mint → assign seq → forward → answer, with no await in
        // between (commit-protocol.md §Durability ordering: `Pipelined`
        // splits step 5). One ticket per proposal, bearing the last entry's
        // id and seq.
        crate::AckMode::Pipelined => match proposal {
            crate::EntryProposal::Single(entry) => {
                let (payload, item) = entry.into_parts();
                let seq = pipeline.next_seq();
                match shared.forward_prepared(log, payload) {
                    Ok(forwarded) => crate::ProposeReply::Ticket(push_pending(
                        pipeline,
                        forwarded,
                        item.into_iter().collect(),
                        seq,
                    )),
                    // Sealed before the entry was even minted. Anything
                    // already forwarded is canon and still lands; the turn
                    // learns the log is gone from this reply.
                    Err(_) => crate::ProposeReply::Sealed,
                }
            }
            // One causal event: one writer message, one `sync_data`, one
            // ack, one ticket (commit-protocol.md §Causal groups). `seq` is
            // assigned per entry in group order, so the run stays contiguous
            // and `seq` order still equals disk order; the ticket bears the
            // last one, which is the epoch the head reaches when the group
            // is applied.
            crate::EntryProposal::Group(entries) => {
                let mut payloads = Vec::with_capacity(entries.len());
                let mut items = Vec::with_capacity(entries.len());
                let mut seq = 0;
                for entry in entries {
                    let (payload, item) = entry.into_parts();
                    seq = pipeline.next_seq();
                    payloads.push(payload);
                    items.extend(item);
                }
                match shared.forward_group(log, payloads) {
                    Ok(forwarded) => {
                        crate::ProposeReply::Ticket(push_pending(pipeline, forwarded, items, seq))
                    }
                    Err(_) => crate::ProposeReply::Sealed,
                }
            }
        },
    }
}

/// Consume a proposal into its entries — the `Sync` arm's shared tail, which
/// treats both shapes identically (see that arm's comment).
fn into_entries(proposal: crate::EntryProposal) -> Vec<crate::PreparedEntry> {
    match proposal {
        crate::EntryProposal::Single(entry) => vec![entry],
        crate::EntryProposal::Group(entries) => entries,
    }
}

/// Record one forwarded unit in the actor's FIFO and mint its ticket.
fn push_pending(
    pipeline: &mut Pipeline,
    forwarded: hotl_store::Forwarded,
    items: Vec<Item>,
    seq: u64,
) -> crate::CommitTicket {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let ticket = crate::CommitTicket {
        id: forwarded.id.clone(),
        seq,
        ack: rx,
    };
    pipeline.fifo.push_back(PendingAck {
        ack: forwarded.ack,
        items,
        id: forwarded.id,
        seq,
        ticket: Some(tx),
    });
    ticket
}

/// Non-Done outcomes leave a durable annotation in the log.
async fn annotate(
    shared: &SharedDeps,
    log: &mut SessionLog,
    head: &mut Head,
    pipeline: &mut Pipeline,
    outcome: &Outcome,
) {
    let reason = match outcome {
        Outcome::Cancelled => Some("user interrupt".to_string()),
        Outcome::TurnLimit => Some(format!("max_turns ({}) reached", shared.config.max_turns)),
        Outcome::DoomLoop { pattern } => Some(format!("doom loop: {pattern}")),
        Outcome::ToolFailureBudget { tool } => Some(format!("tool failure budget: {tool}")),
        Outcome::Error { message } => Some(format!("error: {message}")),
        Outcome::Done { .. } | Outcome::Refused => None,
    };
    if let Some(reason) = reason {
        shared
            .append(log, pipeline, head, EntryPayload::Cancelled { reason })
            .await;
    }
}

/// Compact the projection (M2): fold `[prefix..kept_from)` into a typed
/// digest via the fast model, floor to a placeholder if summarize fails, and
/// re-point the projection with an appended `compaction` entry — the log
/// keeps everything. A digest the turn speculatively precomputed folds
/// instantly; otherwise the summarize runs inline in the actor (no turn is
/// in flight, and admission blocking during that call is the serialization
/// working as designed).
async fn compact(
    shared: &SharedDeps,
    log: &mut SessionLog,
    head: &mut Head,
    pipeline: &mut Pipeline,
    spec: Option<crate::SpecDigest>,
) -> Result<bool, String> {
    // Drain-before-BUILD-and-mint (commit-protocol.md §conflict table, the
    // Abort arm's steps 3→5). Both halves are load-bearing and fail
    // differently: minting after the drain keeps the fold chained onto the
    // drained leaf, and *building* after it is what makes the digest's
    // content and visibility set cover every entry the drain landed — a fold
    // computed against the pre-drain projection would leave a pipeline of
    // assistant blocks visible below a fold that never saw them. Every
    // `items` read below therefore happens after this line, and the tickets
    // resolve `Aborted`: the bytes are canon, the turn's claim on them is
    // not.
    pipeline.drain(head, Resolution::Abort).await;
    // Speculative hit: the digest was planned against this same projection
    // lineage (it only appends between folds), so its indices still name the
    // same items. Reset mode folds a wider span than the speculation covered,
    // so it never uses one; the turn doesn't speculate in reset mode.
    if !shared.config.compaction_reset {
        if let Some(spec) = spec {
            if spec.prefix_end < spec.kept_from && spec.kept_from <= head.items().len() {
                let digest = vec![compaction::digest_item(&spec.text)];
                let payload = EntryPayload::Compaction {
                    digest: digest.clone(),
                    prefix_end: spec.prefix_end,
                    kept_from: spec.kept_from,
                    degraded: false,
                };
                if !shared.append(log, pipeline, head, payload).await {
                    return Err("session log is sealed".into());
                }
                let plan = compaction::Plan {
                    prefix_end: spec.prefix_end,
                    kept_from: spec.kept_from,
                };
                head.repoint(compaction::apply(head.items(), &plan, &digest));
                return Ok(false);
            }
        }
    }
    let tail_budget = (shared.config.context_window as f64 * TAIL_RATIO) as u64;
    let Some(plan) = compaction::plan(head.items(), tail_budget) else {
        return Err("context window exhausted — nothing left to compact".into());
    };
    // Reset mode (#9): fold *everything* after the preserved prefix into the
    // digest and keep no verbatim tail — the continuation is a fresh slate.
    // In-place mode (default): fold [prefix..kept_from) and keep the tail.
    let plan = if shared.config.compaction_reset {
        compaction::Plan {
            prefix_end: plan.prefix_end,
            kept_from: head.items().len(),
        }
    } else {
        plan
    };
    let snapshot = Arc::clone(head.items());
    let folded = &snapshot[plan.prefix_end..plan.kept_from];
    // Timeout or two failed attempts: the floor digest keeps the session moving
    // rather than ending the turn on housekeeping.
    let (digest, degraded) =
        match summarize_bounded(summarize(shared, folded), COMPACT_SUMMARIZE_TIMEOUT).await {
            Some(text) => (vec![compaction::digest_item(&text)], false),
            None => (vec![compaction::floor_digest()], true),
        };
    let payload = EntryPayload::Compaction {
        digest: digest.clone(),
        prefix_end: plan.prefix_end,
        kept_from: plan.kept_from,
        degraded,
    };
    if !shared.append(log, pipeline, head, payload).await {
        return Err("session log is sealed".into());
    }
    head.repoint(compaction::apply(head.items(), &plan, &digest));
    Ok(degraded)
}

/// The inline fold's summarize under a wall-clock bound. `None` on either a
/// failed summarize or an exceeded bound — both degrade to the floor digest,
/// which is why one return type covers them. Split out from [`compact`] so the
/// bound is testable without a session behind it.
async fn summarize_bounded(
    fut: impl std::future::Future<Output = Option<String>>,
    bound: std::time::Duration,
) -> Option<String> {
    tokio::time::timeout(bound, fut).await.ok().flatten()
}

pub(crate) async fn summarize(shared: &SharedDeps, folded: &[Item]) -> Option<String> {
    let model = shared
        .config
        .fast_model
        .clone()
        .unwrap_or_else(|| shared.config.model.clone());
    let request = SamplingRequest {
        model,
        max_tokens: SUMMARIZE_MAX_TOKENS,
        system: compaction::SUMMARIZE_SYSTEM.into(),
        items: Arc::new(vec![Item::User {
            text: compaction::summarize_prompt(folded),
            synthetic: None,
        }]),
        tools: Vec::new().into(),
        thinking: false,
        cache_static: false,
        turn_context: None,
    };
    for _ in 0..SUMMARIZE_ATTEMPTS {
        let mut stream = shared.provider.stream(request.clone());
        let mut text: Option<String> = None;
        while let Some(event) = stream.next().await {
            match event {
                Ok(StreamEvent::Completed { blocks, .. }) => text = Some(assistant_text(&blocks)),
                Ok(_) => {}
                Err(_) => {
                    text = None;
                    break;
                }
            }
        }
        if let Some(t) = text.filter(|t| !t.trim().is_empty()) {
            return Some(t);
        }
    }
    None
}

/// Start a turn now, or queue the prompt if one is running (one-at-a-time
/// promotion). Carries an optional provenance tag (T2).
#[allow(clippy::too_many_arguments)]
async fn admit_prompt(
    shared: &Arc<SharedDeps>,
    log: &mut SessionLog,
    head: &mut Head,
    pipeline: &mut Pipeline,
    queue: &mut VecDeque<(String, Option<SyntheticReason>)>,
    running: bool,
    text: String,
    synthetic: Option<SyntheticReason>,
    cmd_tx: &mpsc::WeakSender<SessionCmd>,
    events: &mpsc::Sender<EngineEvent>,
    current_turn: &Arc<Mutex<CancellationToken>>,
) -> bool {
    if running {
        let full = queue.len() >= QUEUE_MAX;
        match queue.back_mut() {
            // Past the cap the prompt is folded into the last pending entry
            // rather than pushed: memory is bounded and nothing vanishes
            // without the model being told. It *was* absorbed, so the event is
            // still `PromptQueued`. A folded prompt inherits the tag of the
            // entry it joins — only reachable past QUEUE_MAX pending prompts,
            // where provenance has already stopped being per-prompt.
            Some(last) if full => fold_into(&mut last.0, &text, HELD_BYTES_MAX),
            _ => queue.push_back((text, synthetic)),
        }
        let _ = events.send(EngineEvent::PromptQueued).await;
        return true;
    }
    start_turn(
        shared,
        log,
        head,
        pipeline,
        text,
        synthetic,
        cmd_tx,
        events,
        current_turn,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn start_turn(
    shared: &Arc<SharedDeps>,
    log: &mut SessionLog,
    head: &mut Head,
    pipeline: &mut Pipeline,
    text: String,
    synthetic: Option<SyntheticReason>,
    cmd_tx: &mpsc::WeakSender<SessionCmd>,
    events: &mpsc::Sender<EngineEvent>,
    current_turn: &Arc<Mutex<CancellationToken>>,
) -> bool {
    // Captured before `text` moves into the committed item — `UserPromptSubmit`
    // hooks (tier-1 gap #7) see the prompt exactly as submitted.
    let prompt_for_hooks = text.clone();
    let payload = EntryPayload::Item {
        item: Item::User { text, synthetic },
    };
    if !shared.append(log, pipeline, head, payload).await {
        let _ = events
            .send(EngineEvent::TurnDone {
                outcome: Outcome::Error {
                    message: "session log is sealed".into(),
                },
                usage: TokenUsage::default(),
            })
            .await;
        return false;
    }
    // UserPromptSubmit: a hook's `additionalContext` becomes one tagged
    // `SystemReminder` user item committed right after the prompt it answers
    // — never a system-prompt edit (prefix-cache stability), the one
    // reminder chokepoint every injection site shares. Best-effort: a sealed
    // log here doesn't fail the turn (the prompt itself already landed).
    // §S1 HookRouter gate: a masked-off (or hook-less) session skips the
    // call (and its timeout registration) entirely.
    crate::hooks::hook_gate!(
        shared.hooks,
        shared.hook_mask(),
        crate::hooks::EventMask::USER_PROMPT,
        |hooks| {
            if let Some(context) = crate::hooks::call_user_prompt(hooks, &prompt_for_hooks).await {
                let reminder = EntryPayload::Item {
                    item: Item::User {
                        text: format!("<system-reminder>{context}</system-reminder>"),
                        synthetic: Some(SyntheticReason::SystemReminder),
                    },
                };
                shared.append(log, pipeline, head, reminder).await;
            }
        },
        else {}
    );
    spawn_turn(shared, cmd_tx, events, current_turn);
    true
}

/// Spawn a fresh turn task against the current projection, installing a new
/// interrupt token for it.
fn spawn_turn(
    shared: &Arc<SharedDeps>,
    cmd_tx: &mpsc::WeakSender<SessionCmd>,
    events: &mpsc::Sender<EngineEvent>,
    current_turn: &Arc<Mutex<CancellationToken>>,
) {
    let token = CancellationToken::new();
    *current_turn
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = token.clone();
    // A fresh prompt is a fresh turn: no counters carry in.
    respawn_turn(
        shared,
        cmd_tx,
        events,
        token,
        crate::TurnContinuation::default(),
    );
}

/// Spawn a turn task under an existing token, seeded with `cont`. Compaction
/// respawns use this directly (no new user item, same logical turn — the
/// interrupt token carries over so a cancel during the fold still lands, and
/// `cont` carries the per-turn safety counters so `max_turns`, the doom
/// detector and the failure budget bound the *whole* turn).
/// INVARIANT: a compaction respawn continues the same logical turn, counters
/// included. Enforced by `max_turns_is_enforced_across_a_compaction`.
fn respawn_turn(
    shared: &Arc<SharedDeps>,
    cmd_tx: &mpsc::WeakSender<SessionCmd>,
    events: &mpsc::Sender<EngineEvent>,
    token: CancellationToken,
    cont: crate::TurnContinuation,
) {
    // The turn task holds a strong sender for its lifetime; a failed upgrade
    // means the handle is gone and there is nobody left to run for.
    let Some(cmd_tx) = cmd_tx.upgrade() else {
        return;
    };
    let supervisor_tx = cmd_tx.clone();
    let handle = tokio::spawn(turn::run(
        shared.clone(),
        cmd_tx,
        events.clone(),
        token,
        cont,
    ));
    // INVARIANT: exactly one `TurnFinished` per spawned turn, panic included —
    // `running` is cleared and the prompt queue drains on every exit path.
    // Enforced by `a_panicking_turn_reports_an_error_and_the_session_keeps_working`.
    // The supervisor's strong sender drops the moment the turn task ends, so it
    // never keeps the command channel (or the actor) alive on its own.
    tokio::spawn(async move {
        if handle.await.is_err() {
            let _ = supervisor_tx
                .send(SessionCmd::TurnFinished {
                    end: TurnEnd::Outcome(Outcome::Error {
                        message: "the turn ended unexpectedly (internal error). \
                                  The session is intact — retry, or rephrase the request."
                            .into(),
                    }),
                    usage: TokenUsage::default(),
                })
                .await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::COMPACT_SUMMARIZE_TIMEOUT;
    use super::{
        awaiting_tool_results, commit_prepared, compact, fold_into, pair_tool_results,
        summarize_bounded, Pipeline, Resolution, SharedDeps,
    };
    use hotl_store::SessionLog;
    use hotl_types::{EntryPayload, Item, SyntheticReason, ToolResultItem};
    use serde_json::json;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    /// T3-4. The paused clock is legal here: this drives `summarize_bounded`
    /// alone, with no actor and therefore no writer-thread ack for the clock to
    /// auto-advance past (0011's standing constraint). `compact` calls exactly
    /// this function, so the bound under test is the shipped one.
    #[tokio::test(start_paused = true)]
    async fn a_hung_inline_summarize_degrades_instead_of_wedging() {
        let hung = summarize_bounded(std::future::pending(), COMPACT_SUMMARIZE_TIMEOUT).await;
        assert!(
            hung.is_none(),
            "a hung summarize must degrade to the floor digest, not stall the command loop"
        );
        let answered = summarize_bounded(
            std::future::ready(Some("DIGEST".to_string())),
            COMPACT_SUMMARIZE_TIMEOUT,
        )
        .await;
        assert_eq!(
            answered.as_deref(),
            Some("DIGEST"),
            "a summarize that answers inside the bound must still be used"
        );
    }

    fn user(text: &str) -> Item {
        Item::User {
            text: text.into(),
            synthetic: Some(SyntheticReason::Steer),
        }
    }

    fn calls(id: &str) -> Item {
        Item::Assistant {
            blocks: vec![json!({"type": "tool_use", "id": id, "name": "read", "input": {}})],
        }
    }

    fn says(text: &str) -> Item {
        Item::Assistant {
            blocks: vec![json!({"type": "text", "text": text})],
        }
    }

    fn answers(id: &str) -> Item {
        Item::ToolResults {
            results: vec![ToolResultItem {
                tool_use_id: id.into(),
                content: "ok".into(),
                is_error: false,
            }],
        }
    }

    /// T3-7: dropping a user's words silently is worse than telling the model
    /// some were dropped, and an unbounded buffer is worse than both.
    #[test]
    fn folding_bounds_the_buffer_and_discloses_the_truncation() {
        let mut dst = String::from("first");
        fold_into(&mut dst, &"x".repeat(10_000), 128);
        assert!(
            dst.len() <= 128 + 64,
            "fold must bound the entry, got {}",
            dst.len()
        );
        assert!(
            dst.starts_with("first"),
            "the oldest text is kept, not clobbered"
        );
        assert!(
            dst.contains("truncated"),
            "truncation must be disclosed in-band"
        );
        // Idempotent under repeated folding — 1000 folds stay bounded, and the
        // marker never stacks.
        for _ in 0..1_000 {
            fold_into(&mut dst, "more", 128);
        }
        assert!(dst.len() <= 128 + 64, "got {}", dst.len());
        assert!(dst.starts_with("first"));
    }

    #[test]
    fn folding_under_the_cap_keeps_every_word() {
        let mut dst = String::from("first");
        fold_into(&mut dst, "second", 1_024);
        assert!(dst.contains("first") && dst.contains("second"));
        assert!(!dst.contains("truncated"), "nothing was dropped: {dst}");
    }

    #[test]
    fn only_unanswered_tool_calls_hold_the_batch_open() {
        assert!(awaiting_tool_results(&[calls("t1")]));
        assert!(!awaiting_tool_results(&[says("hello")]));
        assert!(!awaiting_tool_results(&[calls("t1"), answers("t1")]));
        assert!(!awaiting_tool_results(&[]));
    }

    #[test]
    fn a_stranded_steer_moves_behind_the_results_it_interrupted() {
        let repaired = pair_tool_results(vec![calls("t1"), user("wait"), answers("t1")]);
        assert_eq!(repaired, vec![calls("t1"), answers("t1"), user("wait")]);
    }

    #[test]
    fn several_stranded_items_keep_their_order() {
        let repaired = pair_tool_results(vec![
            calls("t1"),
            user("one"),
            user("two"),
            answers("t1"),
            says("done"),
        ]);
        assert_eq!(
            repaired,
            vec![
                calls("t1"),
                answers("t1"),
                user("one"),
                user("two"),
                says("done"),
            ]
        );
    }

    #[test]
    fn already_paired_history_is_left_alone() {
        let good = vec![
            user("start"),
            calls("t1"),
            answers("t1"),
            user("next"),
            says("done"),
        ];
        assert_eq!(pair_tool_results(good.clone()), good);
    }

    #[test]
    fn a_gap_with_no_results_coming_is_not_reordered() {
        // Nothing answered t1, so there is no batch to move anything behind —
        // reordering here would only invent a new history.
        let orphaned = vec![calls("t1"), user("never answered"), says("moved on")];
        assert_eq!(pair_tool_results(orphaned.clone()), orphaned);
    }

    #[test]
    fn a_trailing_gap_survives_repair() {
        let trailing = vec![calls("t1"), user("last word")];
        assert_eq!(pair_tool_results(trailing.clone()), trailing);
    }

    // --- Task 8 (S2a PreparedPayload) --------------------------------

    fn test_deps(dir: &std::path::Path, log: hotl_store::SessionLog) -> crate::SessionDeps {
        crate::SessionDeps {
            provider: Arc::new(hotl_provider::ScriptedProvider::new(vec![])),
            registry: Arc::new(hotl_tools::Registry::builtin()),
            rules: Arc::new(hotl_tools::rules::Rules::default()),
            sandbox_enforced: false,
            clock: Arc::new(hotl_platform::SystemClock),
            log,
            system: "sys".into(),
            cwd: dir.to_path_buf(),
            snapshots: None,
            hooks: None,
            initial_items: Vec::new(),
            initial_todos: Vec::new(),
            config: crate::EngineConfig::default(),
        }
    }

    /// A `SharedDeps` + its log + a live `Head`. `commit_prepared`/`compact`
    /// take the head, so the published projection is exercised by every test
    /// below rather than simulated.
    fn test_shared(dir: &std::path::Path) -> (Arc<SharedDeps>, SessionLog, super::Head) {
        let log = SessionLog::create(dir, "m", None, hotl_store::Masker::empty(), 0).expect("log");
        let (head_tx, head_rx) = super::head_channel();
        let head = super::Head::new(head_tx, Vec::new(), Vec::new());
        let (shared, log) = SharedDeps::new(
            test_deps(dir, log),
            crate::hooks::NotificationDrain::new(),
            head_rx,
        );
        (Arc::new(shared), log, head)
    }

    /// commit-protocol.md §Proposal payloads' rules_epoch guard: "the actor
    /// rejects a payload whose epoch predates the current masking rules" —
    /// tested directly against `commit_prepared`, the actor's real commit
    /// path for prepared entries, not just a hand-extracted predicate.
    #[tokio::test]
    async fn commit_prepared_rejects_an_entry_whose_epoch_predates_current_and_commits_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (shared, mut log, mut head) = test_shared(dir.path());
        let before = std::fs::read_to_string(log.path()).unwrap();

        // A genuinely OLDER epoch, not merely a different one: bump the
        // actor's real epoch, then stamp the entry with what used to be
        // current — exactly the shape a real reject-then-retry produces
        // (commit-protocol.md §Proposal payloads: "rejects a payload whose
        // epoch PREDATES the current masking rules").
        let old_epoch = shared.rules_epoch();
        shared.rules_epoch.fetch_add(1, Ordering::Relaxed);
        assert!(shared.rules_epoch() > old_epoch);

        let payload = hotl_types::EntryPayload::Usage {
            usage: hotl_types::TokenUsage::default(),
        };
        let prepared =
            hotl_store::prepare_payload(&payload, &hotl_store::Masker::empty(), old_epoch)
                .expect("prepare");
        let entries = vec![crate::PreparedEntry::new(prepared, None)];

        let result = commit_prepared(
            &shared,
            &mut log,
            &mut head,
            &mut Pipeline::default(),
            crate::EntryProposal::of(entries),
            crate::AckMode::Sync,
        )
        .await;
        assert!(matches!(result, crate::ProposeReply::StaleEpoch));
        assert!(
            head.items().is_empty(),
            "a stale proposal must not touch the projection"
        );
        let after = std::fs::read_to_string(log.path()).unwrap();
        assert_eq!(before, after, "a stale proposal must not reach the log");
    }

    /// The check is "predates", not "differs from": an entry stamped with an
    /// epoch that is not older than current — including one newer than the
    /// actor has ever advanced to, which can't happen in production since
    /// the actor is the epoch's sole owner, but pins the direction the guard
    /// actually checks — is accepted, not rejected.
    #[tokio::test]
    async fn commit_prepared_accepts_an_entry_whose_epoch_is_not_older_than_current() {
        let dir = tempfile::tempdir().unwrap();
        let (shared, mut log, mut head) = test_shared(dir.path());

        let newer_epoch = shared.rules_epoch() + 1;
        let payload = hotl_types::EntryPayload::Usage {
            usage: hotl_types::TokenUsage::default(),
        };
        let prepared =
            hotl_store::prepare_payload(&payload, &hotl_store::Masker::empty(), newer_epoch)
                .expect("prepare");
        let entries = vec![crate::PreparedEntry::new(prepared, None)];

        let result = commit_prepared(
            &shared,
            &mut log,
            &mut head,
            &mut Pipeline::default(),
            crate::EntryProposal::of(entries),
            crate::AckMode::Sync,
        )
        .await;
        assert!(matches!(result, crate::ProposeReply::Committed));
    }

    // --- Task 9 (S2b pipelined commits) ------------------------------

    fn prepared(shared: &SharedDeps, item: Item) -> crate::PreparedEntry {
        let payload = EntryPayload::Item { item: item.clone() };
        let prepared = hotl_store::prepare_payload(
            &payload,
            &hotl_store::Masker::empty(),
            shared.rules_epoch(),
        )
        .expect("prepare");
        crate::PreparedEntry::new(prepared, Some(item))
    }

    /// commit-protocol.md §Durability ordering: `Pipelined` splits step 5.
    /// The actor answers with a ticket the moment it forwards, and the
    /// projection does NOT advance until the writer acks.
    #[tokio::test]
    async fn a_pipelined_proposal_answers_with_a_ticket_before_the_projection_moves() {
        let dir = tempfile::tempdir().unwrap();
        let (shared, mut log, mut head) = test_shared(dir.path());
        let mut pipeline = Pipeline::default();

        let reply = commit_prepared(
            &shared,
            &mut log,
            &mut head,
            &mut pipeline,
            crate::EntryProposal::of(vec![prepared(&shared, user("hi"))]),
            crate::AckMode::Pipelined,
        )
        .await;
        let crate::ProposeReply::Ticket(ticket) = reply else {
            panic!("Pipelined must answer with a ticket, got {reply:?}")
        };
        assert_eq!(ticket.seq, 1, "seq is assigned at validation, eagerly");
        assert!(!ticket.id.is_empty(), "so is the ulid");
        assert!(
            head.items().is_empty(),
            "the projection advances only on ack, never on forward"
        );

        pipeline.drain(&mut head, Resolution::Ack).await;
        assert_eq!(head.items().len(), 1, "…and it advances when the ack lands");
        let ack = ticket
            .ack
            .await
            .expect("the actor resolves the ticket")
            .expect("committed");
        assert!(ack.offset > 0, "the ticket carries the byte offset");
    }

    /// "acks arrive in order (one writer, one FIFO channel) and the
    /// projection advances in that order" — three proposals in, three items
    /// out, same order, with strictly increasing seq and offsets.
    #[tokio::test]
    async fn the_pipeline_advances_the_projection_in_fifo_order() {
        let dir = tempfile::tempdir().unwrap();
        let (shared, mut log, mut head) = test_shared(dir.path());
        let mut pipeline = Pipeline::default();

        let mut tickets = Vec::new();
        for text in ["one", "two", "three"] {
            let reply = commit_prepared(
                &shared,
                &mut log,
                &mut head,
                &mut pipeline,
                crate::EntryProposal::of(vec![prepared(&shared, user(text))]),
                crate::AckMode::Pipelined,
            )
            .await;
            let crate::ProposeReply::Ticket(ticket) = reply else {
                panic!("expected a ticket")
            };
            tickets.push(ticket);
        }
        assert!(head.items().is_empty());

        pipeline.drain(&mut head, Resolution::Ack).await;
        assert_eq!(
            head.items().as_slice(),
            [user("one"), user("two"), user("three")].as_slice()
        );
        let mut last = 0;
        for (i, ticket) in tickets.into_iter().enumerate() {
            assert_eq!(ticket.seq, i as u64 + 1);
            let ack = ticket.ack.await.expect("resolved").expect("committed");
            assert!(ack.offset > last, "offsets follow disk order");
            last = ack.offset;
        }
    }

    /// The pipelined half of matrix case 4's invariant, at the seam that
    /// decides it: an ack that never comes leaves the projection exactly
    /// where it was, and the ticket says `LogSealed` rather than naming an
    /// offset for bytes nobody synced.
    #[tokio::test]
    async fn an_unacked_pipelined_entry_never_advances_the_projection() {
        let dir = tempfile::tempdir().unwrap();
        let (shared, mut log, mut head) = test_shared(dir.path());
        let mut pipeline = Pipeline::default();
        log.inject_fault(hotl_store::WriteFault::DropAckBeforeFsync);

        let reply = commit_prepared(
            &shared,
            &mut log,
            &mut head,
            &mut pipeline,
            crate::EntryProposal::of(vec![prepared(&shared, user("doomed"))]),
            crate::AckMode::Pipelined,
        )
        .await;
        let crate::ProposeReply::Ticket(ticket) = reply else {
            panic!("expected a ticket")
        };

        pipeline.drain(&mut head, Resolution::Ack).await;
        assert!(
            head.items().is_empty(),
            "a crash may leave the log ahead of the projection, never the reverse"
        );
        assert_eq!(
            ticket.ack.await.expect("resolved"),
            Err(crate::CommitFailed::LogSealed)
        );
    }

    /// §Ordering authority: `seq` is "global commit order across the whole
    /// session", not a count of the pipelined subset. Every commit the actor
    /// makes advances it — a `Sync` proposal and an actor-originated inline
    /// append included — or the watch predicate S2c is built on
    /// (`epoch >= my_ack_seq`, where `epoch` is the seq of the newest entry
    /// applied to the published head) is comparing two different counters.
    #[tokio::test]
    async fn seq_is_the_session_wide_commit_order_not_the_pipelined_subset() {
        let dir = tempfile::tempdir().unwrap();
        let (shared, mut log, mut head) = test_shared(dir.path());
        let mut pipeline = Pipeline::default();

        // A Sync proposal…
        commit_prepared(
            &shared,
            &mut log,
            &mut head,
            &mut pipeline,
            crate::EntryProposal::of(vec![prepared(&shared, user("sync"))]),
            crate::AckMode::Sync,
        )
        .await;
        // …and an actor-originated inline append.
        assert!(
            shared
                .append(
                    &mut log,
                    &mut pipeline,
                    &mut head,
                    EntryPayload::Rename {
                        name: "inline".into()
                    },
                )
                .await
        );

        let reply = commit_prepared(
            &shared,
            &mut log,
            &mut head,
            &mut pipeline,
            crate::EntryProposal::of(vec![prepared(&shared, user("pipelined"))]),
            crate::AckMode::Pipelined,
        )
        .await;
        let crate::ProposeReply::Ticket(ticket) = reply else {
            panic!("expected a ticket")
        };
        assert_eq!(
            ticket.seq, 3,
            "the third commit of the session carries seq 3, not seq 1"
        );
    }

    /// commit-protocol.md test matrix case 7 + the conflict table's Abort
    /// arm, steps (3)→(5): the fold drains the FIFO first, so it *builds*
    /// against a projection that already contains every entry the drain
    /// landed, and only then mints — chaining the compaction entry onto the
    /// drained leaf. The spec's digest is deliberately one the pre-drain
    /// projection could not have produced (`kept_from` past its length), so
    /// a fold that built before the drain would silently skip it.
    #[tokio::test]
    async fn a_compaction_drains_the_pipeline_before_it_builds_and_mints() {
        let dir = tempfile::tempdir().unwrap();
        let (shared, mut log, mut head) = test_shared(dir.path());
        let mut pipeline = Pipeline::default();

        let mut tickets = Vec::new();
        for text in ["first", "second"] {
            let reply = commit_prepared(
                &shared,
                &mut log,
                &mut head,
                &mut pipeline,
                crate::EntryProposal::of(vec![prepared(&shared, user(text))]),
                crate::AckMode::Pipelined,
            )
            .await;
            let crate::ProposeReply::Ticket(ticket) = reply else {
                panic!("expected a ticket")
            };
            tickets.push(ticket);
        }
        assert!(
            head.items().is_empty(),
            "two entries forwarded, none projected yet"
        );

        let spec = crate::SpecDigest {
            prefix_end: 0,
            kept_from: 2,
            text: "folded".into(),
        };
        let degraded = compact(&shared, &mut log, &mut head, &mut pipeline, Some(spec))
            .await
            .expect("the fold must see the drained projection");
        assert!(!degraded);

        for ticket in tickets {
            assert_eq!(
                ticket.ack.await.expect("resolved"),
                Err(crate::CommitFailed::Aborted),
                "an aborted turn loses its claim on the log, never the bytes"
            );
        }

        // The bytes are canon and the aborting entry chains onto them: the
        // compaction entry's parent is the last entry the drain landed.
        let entries: Vec<hotl_types::Entry> = std::fs::read_to_string(log.path())
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).expect("entry"))
            .collect();
        let last = entries.len() - 1;
        assert!(
            matches!(entries[last].payload, EntryPayload::Compaction { .. }),
            "the fold is minted last: {:?}",
            entries[last].payload
        );
        assert_eq!(
            entries[last].parent_id.as_deref(),
            Some(entries[last - 1].id.as_str()),
            "the aborting entry chains onto the drained leaf"
        );
    }

    #[tokio::test]
    async fn commit_prepared_commits_a_fresh_entry_and_updates_the_projection() {
        let dir = tempfile::tempdir().unwrap();
        let (shared, mut log, mut head) = test_shared(dir.path());

        let epoch = shared.rules_epoch();
        let payload = EntryPayload::Item { item: user("hi") };
        let prepared = hotl_store::prepare_payload(&payload, &hotl_store::Masker::empty(), epoch)
            .expect("prepare");
        let entries = vec![crate::PreparedEntry::new(prepared, Some(user("hi")))];

        let result = commit_prepared(
            &shared,
            &mut log,
            &mut head,
            &mut Pipeline::default(),
            crate::EntryProposal::of(entries),
            crate::AckMode::Sync,
        )
        .await;
        assert!(matches!(result, crate::ProposeReply::Committed));
        assert_eq!(
            head.items().len(),
            1,
            "a fresh proposal must reach the projection"
        );

        let replayed = hotl_store::replay(log.path()).expect("replay");
        assert_eq!(
            replayed.items.len(),
            1,
            "and the disk, chained after the header"
        );
    }
}
