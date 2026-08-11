//! The session actor: sole committer, projection owner, turn scheduler.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use hotl_context::breakdown::ToolTokens;
use hotl_context::{compaction, tokens};
use hotl_platform::Clock;
use hotl_provider::{Provider, SamplingRequest, StreamEvent, ToolDef};
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
/// Byte cap on images buffered across held steers and queued prompts. Text
/// gets `HELD_BYTES_MAX`; base64 is three orders of magnitude denser, so it
/// gets its own — and it stays well under `hotl_types::IMAGE_B64_BUDGET` so a
/// released fold can never be the thing that trips the window budget.
const HELD_IMAGE_B64_MAX: usize = 4 * 1024 * 1024;
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
    /// The proposer's declaration of whether its sample had closed. Not
    /// state and not a decision input — the held-steer release's assertion
    /// reads it once, at settle, and it dies with this entry.
    stage: crate::SampleStage,
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

    /// The snapshot a turn task samples against, as **two channels**: the
    /// durable projection the head already holds, and the ephemeral suffix
    /// regenerated per read. The reminder is never spliced into `items` — not
    /// even into a copy of it — so it can never be committed, replayed,
    /// double-counted, or (the point of the split) chosen as a cache
    /// breakpoint by a serializer that only sees a flat list.
    ///
    /// Both fields hand back an `Arc` the head or the process already holds
    /// whenever it can: the common no-todos read allocates nothing at all,
    /// which is what keeps "Vec with clone at grant" the cost model §Read
    /// invariant priced.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            durable: Arc::clone(&self.items),
            tail: tail_for(&self.todos),
        }
    }
}

/// The ephemeral suffix a set of todos renders to. One definition, shared by
/// the published head and by the actor's own copy ([`Head::snapshot`]) — two
/// would be two answers to "what rides after the cache marker".
fn tail_for(todos: &[Todo]) -> Arc<Vec<Item>> {
    match hotl_tools::todo::render_reminder(todos) {
        Some(reminder) => Arc::new(vec![reminder]),
        None => empty_tail(),
    }
}

/// These two carry a whole roster inside `description()` (`skills.rs:470`,
/// `agents.rs:470`), so `/context` bills them as rows of their own instead of
/// burying a session's biggest schema line in the tool-schema total.
const SKILLS_TOOL: &str = "skill";
const AGENTS_TOOL: &str = "spawn";

/// A tool definition's share of the request: the three strings that go on the
/// wire, under the same profile-less estimator every other call site uses.
fn estimate_tool_def(def: &ToolDef) -> u64 {
    tokens::estimate_text(&def.name)
        + tokens::estimate_text(&def.description)
        + tokens::estimate_text(&def.input_schema.to_string())
}

/// The three tool rows of a `/context` report. Split by name, never by index:
/// registry order is a registration detail.
fn tool_tokens(registry: &Registry) -> ToolTokens {
    let mut out = ToolTokens::default();
    for def in registry.defs() {
        let n = estimate_tool_def(&def);
        match def.name.as_str() {
            SKILLS_TOOL => out.skills += n,
            AGENTS_TOOL => out.agents += n,
            _ => out.schemas += n,
        }
    }
    out
}

/// One read of the projection head, split by durability.
///
/// The split is the invariant: `durable` is byte-stable between supersede
/// events (append-only projection), `tail` is regenerated every read. Every
/// consumer that must not see ephemeral content — the fork seed, the cache
/// breakpoint chooser — reads `durable` and structurally cannot reach `tail`.
#[derive(Clone, Debug)]
pub struct Snapshot {
    /// The durable projection — byte-stable between supersede events.
    pub durable: Arc<Vec<Item>>,
    /// Ephemeral per-sample suffix (todo reminder today), regenerated per
    /// read, never committed, rendered after every cache marker.
    pub tail: Arc<Vec<Item>>,
}

/// The one shared empty ephemeral tail. A `OnceLock` rather than
/// `Arc::new(Vec::new())` per call so that a session with no todos — the
/// common case, and the one on the sample-boundary hot path — allocates
/// nothing to say "nothing ephemeral here".
pub(crate) fn empty_tail() -> Arc<Vec<Item>> {
    static EMPTY: std::sync::OnceLock<Arc<Vec<Item>>> = std::sync::OnceLock::new();
    Arc::clone(EMPTY.get_or_init(|| Arc::new(Vec::new())))
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

    /// The same two-channel read a turn takes off the published head, taken
    /// from the actor's own copy — the freshest one there is, and no watch
    /// round-trip. Read-only: nothing here advances or publishes.
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            durable: Arc::clone(&self.items),
            tail: tail_for(&self.todos),
        }
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

/// Why a held steer may land right now — and, for the one case that needs
/// it, the *proof*. There is no third variant, and between them they are the
/// whole of the protection the `ProposePrepared` handler's old `sampling`
/// flip gave: a steer is only ever appended at a boundary the actor itself
/// just created, so it can never precede an assistant item that could not
/// have seen it (72a6f1b).
#[derive(Clone, Copy, Debug)]
enum Boundary {
    /// A commit this turn made just landed and was applied to the head.
    /// `stage` is the proposer's own declaration that its sample had closed
    /// — the mid-sample half of the old assert's protection, checked rather
    /// than assumed (see [`crate::SampleStage`]).
    CommitSettled { stage: crate::SampleStage },
    /// The turn is over; nothing will answer an open batch now, so no sample
    /// can be in flight by construction.
    TurnEnded,
}

impl Boundary {
    /// Whether this really is a boundary a held steer may land at.
    fn is_between_samples(self) -> bool {
        match self {
            Self::CommitSettled { stage } => stage == crate::SampleStage::AtBoundary,
            Self::TurnEnded => true,
        }
    }
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
    /// Plan mode, the second permission axis. A separate cell rather than a
    /// bit packed into `mode`: the two are set by separate commands and no
    /// invariant couples them, so there is nothing to keep consistent across
    /// a single load. Seeded from `rules.plan()` at session start.
    plan: AtomicBool,
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
        PermissionMode::Bypass => 1,
        PermissionMode::DontAsk => 2,
    }
}

fn u8_to_mode(v: u8) -> PermissionMode {
    match v {
        1 => PermissionMode::Bypass,
        2 => PermissionMode::DontAsk,
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
        let plan = AtomicBool::new(deps.rules.plan());
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
            plan,
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
    /// build can't be flipped to `Bypass` mid-session by a client request.
    /// Returns the mode actually stored (post-coercion) so the caller logs
    /// the durable `ModeSet` entry with what really took effect, not the
    /// raw request.
    fn set_mode(&self, mode: PermissionMode) -> PermissionMode {
        let mode = hotl_tools::rules::enforced_mode(mode);
        self.mode.store(mode_to_u8(mode), Ordering::Relaxed);
        mode
    }

    /// Plan mode right now — the second axis `evaluate` gates against.
    pub(crate) fn effective_plan(&self) -> bool {
        self.plan.load(Ordering::Relaxed)
    }

    /// Runtime plan-mutation entry point (`SessionCmd::SetPlan`, reachable via
    /// ACP `session/set_plan` and the TUI `/plan` command). No `enforced_mode`
    /// counterpart: the overlay only ever adds an ask, so no build tightens it.
    fn set_plan(&self, plan: bool) {
        self.plan.store(plan, Ordering::Relaxed);
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
    let mut queue: VecDeque<QueuedPrompt> = VecDeque::new();
    // Steers that arrived while a turn was live (or a tool batch open),
    // waiting for a boundary before they can be appended.
    let mut held_steers: Vec<HeldSteer> = Vec::new();
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
                let stage = entry.stage;
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
                    Boundary::CommitSettled { stage },
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
            SessionCmd::Prompt { text, images } => {
                running = admit_prompt(
                    &shared,
                    &mut log,
                    &mut head,
                    &mut pipeline,
                    &mut queue,
                    running,
                    QueuedPrompt {
                        text,
                        images,
                        synthetic: None,
                    },
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
                    QueuedPrompt {
                        text,
                        images: Vec::new(),
                        synthetic: Some(synthetic),
                    },
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
            SessionCmd::Steer { text, images } => {
                admit_steer(
                    &shared,
                    &mut log,
                    &mut head,
                    &mut pipeline,
                    &mut held_steers,
                    running,
                    HeldSteer { text, images },
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
                // build's log never claims `bypass` while it actually ran
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
            SessionCmd::SetPlan(plan) => {
                // The plan axis, same shape as `SetMode`: atomic first so it
                // gates the running session immediately, then the durable
                // entry `hotl resume` replays. No coercion — see `set_plan`.
                shared.set_plan(plan);
                let _ = shared
                    .append(
                        &mut log,
                        &mut pipeline,
                        &mut head,
                        EntryPayload::PlanSet { on: plan },
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
            SessionCmd::ContextBreakdown { reply } => {
                // The one arm that reads and returns. No append, no publish,
                // no `.await` — that is what makes `/context` safe mid-turn.
                let snap = head.snapshot();
                let _ = reply.send(hotl_context::breakdown::breakdown(
                    &shared.system,
                    tool_tokens(&shared.registry),
                    &snap.durable,
                    &snap.tail,
                    shared.config.context_window,
                ));
            }
            SessionCmd::Propose { entries, reply } => {
                let committed = commit(&shared, &mut log, &mut head, &mut pipeline, entries).await;
                let _ = reply.send(committed);
            }
            SessionCmd::ProposePrepared {
                proposal,
                stage,
                mode,
                reply,
            } => {
                let result = commit_prepared(
                    &shared,
                    &mut log,
                    &mut head,
                    &mut pipeline,
                    proposal,
                    mode,
                    stage,
                )
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
                        Boundary::CommitSettled { stage },
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
    queue: &'a mut VecDeque<QueuedPrompt>,
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
    queue: &mut VecDeque<QueuedPrompt>,
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
        Some(next) => {
            start_turn(
                shared,
                log,
                head,
                pipeline,
                next,
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
///
/// Callers (`push_or_fold_steer`, `admit_prompt`'s queue-full arm) append
/// their own image-drop note onto `text`'s tail before calling in. A fold
/// triggered by the *text* cap can then truncate that note away along with
/// the rest of the tail — FOLD_MARK still discloses that something was
/// dropped, just not what. The image-only-full case (text under cap, only
/// images over) keeps the note intact. Correct trade, not a bug.
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

/// Append what fits of `src` to `dst` under `max_bytes` of base64, returning
/// how many images were dropped. Dropping is disclosed in-band by the caller,
/// the same bargain `fold_into` makes for text.
fn fold_images(
    dst: &mut Vec<hotl_types::UserImage>,
    src: Vec<hotl_types::UserImage>,
    max_bytes: usize,
) -> usize {
    let mut total: usize = dst.iter().map(|i| i.data.len()).sum();
    let mut dropped = 0;
    for img in src {
        if total + img.data.len() > max_bytes {
            dropped += 1;
            continue;
        }
        total += img.data.len();
        dst.push(img);
    }
    dropped
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
/// `running` is the same hold one step earlier, and one step wider: a steer
/// that arrives while a request is in flight would otherwise commit *ahead*
/// of the assistant item that request is about to produce. A live turn is
/// always either mid-sample or mid-batch, so holding for its whole life
/// costs nothing — [`release_steers`] runs at every boundary the actor
/// creates, which is where the steer would have landed anyway.
/// INVARIANT: a steer never precedes an assistant item that could not have seen
/// it. Enforced by `a_mid_stream_steer_commits_after_the_reply_it_did_not_see`
/// and `a_steer_inside_the_boundary_group_lands_after_the_assistant_item`.
async fn admit_steer(
    shared: &SharedDeps,
    log: &mut SessionLog,
    head: &mut Head,
    pipeline: &mut Pipeline,
    held: &mut Vec<HeldSteer>,
    running: bool,
    steer: HeldSteer,
) {
    if running || awaiting_tool_results(head.items()) {
        // Past either byte cap — text or images — fold into the last held
        // steer rather than grow without bound; it all commits at release.
        push_or_fold_steer(held, steer);
        return;
    }
    append_steer(shared, log, head, pipeline, steer).await;
}

/// Push a steer onto the held buffer, or fold it into the last entry once
/// either cap is reached. Sync and self-contained so the bound is unit-testable
/// without a session behind it.
fn push_or_fold_steer(held: &mut Vec<HeldSteer>, steer: HeldSteer) {
    let text_total: usize = held.iter().map(|s| s.text.len()).sum();
    let image_total: usize = held
        .iter()
        .flat_map(|s| s.images.iter())
        .map(|i| i.data.len())
        .sum();
    let incoming: usize = steer.images.iter().map(|i| i.data.len()).sum();
    let full = text_total >= HELD_BYTES_MAX || image_total + incoming > HELD_IMAGE_B64_MAX;
    match held.last_mut() {
        Some(last) if full => {
            // Images first, disclosure on the INCOMING text, one fold_into
            // call — keeps FOLD_MARK the literal tail so it never stacks.
            let dropped = fold_images(&mut last.images, steer.images, HELD_IMAGE_B64_MAX);
            let mut text = steer.text;
            if dropped > 0 {
                text.push_str(&format!(
                    "\n[{dropped} image(s) not attached: steer buffer full]"
                ));
            }
            fold_into(&mut last.text, &text, HELD_BYTES_MAX);
        }
        _ => held.push(steer),
    }
}

/// A steer waiting for a between-samples boundary, with the images its
/// committed `Item::User` will carry.
struct HeldSteer {
    text: String,
    images: Vec<hotl_types::UserImage>,
}

async fn append_steer(
    shared: &SharedDeps,
    log: &mut SessionLog,
    head: &mut Head,
    pipeline: &mut Pipeline,
    steer: HeldSteer,
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
                    text: steer.text,
                    synthetic: Some(SyntheticReason::Steer),
                    images: steer.images,
                },
            },
        )
        .await;
}

/// Append the steers that were waiting on a sample or a batch, oldest first,
/// once the reply has landed and the pairing is closed.
///
/// `at` carries the two halves of the held-steer rule's proof, and both are
/// checked here rather than argued in a comment:
///
/// - **mid-sample** — [`Boundary::is_between_samples`]: the commit that
///   created this boundary closed its sample, so the model's reply is
///   already durable. Assert-only, because no site can violate it today; it
///   is aimed squarely at §Commit granularity's intra-sample `BlockEnd`
///   pipelining, which would otherwise reintroduce 72a6f1b silently.
/// - **mid-batch** — `awaiting_tool_results`: a steer that split an open
///   tool batch would strand the results from the calls they answer.
async fn release_steers(
    shared: &SharedDeps,
    log: &mut SessionLog,
    head: &mut Head,
    pipeline: &mut Pipeline,
    held: &mut Vec<HeldSteer>,
    at: Boundary,
) {
    debug_assert!(
        at.is_between_samples(),
        "a held steer may only land between samples: releasing behind an in-sample \
         commit would put it ahead of the assistant item the model is still \
         producing — the inversion 72a6f1b fixed ({at:?})"
    );
    if held.is_empty() || awaiting_tool_results(head.items()) {
        return;
    }
    for steer in std::mem::take(held) {
        append_steer(shared, log, head, pipeline, steer).await;
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
    stage: crate::SampleStage,
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
                        stage,
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
                    Ok(forwarded) => crate::ProposeReply::Ticket(push_pending(
                        pipeline, forwarded, items, seq, stage,
                    )),
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
    stage: crate::SampleStage,
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
        stage,
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
    let Some(plan) = compaction::plan(head.items(), tail_budget, hotl_types::IMAGE_B64_BUDGET)
    else {
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
            images: Vec::new(),
        }]),
        // A summarize is a one-shot call against a prompt that is different
        // every time: nothing to cache, and nothing ephemeral to append.
        ephemeral_tail: empty_tail(),
        tools: Vec::new().into(),
        thinking: false,
        // Compaction summarizes; it does not reason. Both depth knobs stay off
        // regardless of the session's setting — a `max`-effort session should
        // not pay `max` to fold its own history.
        effort: None,
        cache: hotl_provider::CachePolicy::Off,
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
                    // Finish the stream rather than abandoning it
                    // mid-flight: an error event is terminal for the
                    // provider's own generator, so this is one more poll,
                    // and it is what marks the attempt as really consumed
                    // rather than left in flight.
                    crate::turn::drain_to_end(&mut stream).await;
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

/// A prompt waiting its turn (one-at-a-time promotion), with everything the
/// committed `Item::User` will carry.
struct QueuedPrompt {
    text: String,
    images: Vec<hotl_types::UserImage>,
    synthetic: Option<SyntheticReason>,
}

/// Start a turn now, or queue the prompt if one is running (one-at-a-time
/// promotion). Carries an optional provenance tag (T2).
#[allow(clippy::too_many_arguments)]
async fn admit_prompt(
    shared: &Arc<SharedDeps>,
    log: &mut SessionLog,
    head: &mut Head,
    pipeline: &mut Pipeline,
    queue: &mut VecDeque<QueuedPrompt>,
    running: bool,
    prompt: QueuedPrompt,
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
            // where provenance has already stopped being per-prompt. Its
            // images ride along up to `HELD_IMAGE_B64_MAX`; the overflow is
            // dropped and disclosed. Two folded prompts may then both say
            // `[Image #1]`, and renumbering would mean parsing message text,
            // which types-layer policy forbids — accepted on this
            // pathological path (text folding already truncates far worse).
            Some(last) if full => {
                // Images first, disclosure on the INCOMING text, one fold_into
                // call — keeps FOLD_MARK the literal tail so it never stacks.
                let dropped = fold_images(&mut last.images, prompt.images, HELD_IMAGE_B64_MAX);
                let mut text = prompt.text;
                if dropped > 0 {
                    text.push_str(&format!(
                        "\n[{dropped} image(s) not attached: prompt queue full]"
                    ));
                }
                fold_into(&mut last.text, &text, HELD_BYTES_MAX);
            }
            _ => queue.push_back(prompt),
        }
        let _ = events.send(EngineEvent::PromptQueued).await;
        return true;
    }
    start_turn(
        shared,
        log,
        head,
        pipeline,
        prompt,
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
    prompt: QueuedPrompt,
    cmd_tx: &mpsc::WeakSender<SessionCmd>,
    events: &mpsc::Sender<EngineEvent>,
    current_turn: &Arc<Mutex<CancellationToken>>,
) -> bool {
    // Captured before `text` moves into the committed item — `UserPromptSubmit`
    // hooks (tier-1 gap #7) see the prompt exactly as submitted. Text only:
    // images are not exposed to hooks (v1) — the inline `[Image #N]` markers
    // still tell a hook one was attached.
    let prompt_for_hooks = prompt.text.clone();
    let payload = EntryPayload::Item {
        item: Item::User {
            text: prompt.text,
            synthetic: prompt.synthetic,
            images: prompt.images,
        },
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
                        synthetic: Some(SyntheticReason::SystemReminder), images: Vec::new()
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
        awaiting_tool_results, commit_prepared, compact, fold_images, fold_into, pair_tool_results,
        push_or_fold_steer, release_steers, summarize_bounded, Head, HeldSteer, Pipeline,
        ProjectionHead, Resolution, SharedDeps, FOLD_MARK, HELD_BYTES_MAX, HELD_IMAGE_B64_MAX,
    };
    use hotl_store::SessionLog;
    use hotl_types::{EntryPayload, Item, SyntheticReason, ToolResultItem};
    use serde_json::json;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    #[test]
    fn appending_to_the_head_shares_existing_image_payloads() {
        let (tx, _rx) = tokio::sync::watch::channel(Arc::new(ProjectionHead {
            items: Arc::new(Vec::new()),
            todos: Arc::new(Vec::new()),
            leaf: None,
            epoch: 0,
        }));
        let img = hotl_types::UserImage {
            media_type: "image/png".into(),
            data: "aW1nMQ==".into(),
        };
        let before = Arc::clone(&img.data);
        let mut head = Head::new(
            tx,
            vec![Item::User {
                text: "look".into(),
                synthetic: None,
                images: vec![img],
            }],
            Vec::new(),
        );
        head.apply(Item::Assistant { blocks: Vec::new() });
        let Item::User { images, .. } = &head.items()[0] else {
            panic!("user item")
        };
        // INVARIANT: `Arc::make_mut`'s clone stays pointer-sized per image.
        // Enforced by this test.
        assert!(Arc::ptr_eq(&images[0].data, &before));
    }

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
            images: Vec::new(),
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
    fn folding_bounds_held_images_and_reports_what_it_dropped() {
        let img = |n: usize| hotl_types::UserImage {
            media_type: "image/png".into(),
            data: "A".repeat(n).into(),
        };
        let mut dst = vec![img(8)];
        // Room for 8 more bytes: the first extra fits, the second does not.
        let dropped = fold_images(&mut dst, vec![img(8), img(8)], 16);
        assert_eq!(dropped, 1);
        assert_eq!(dst.len(), 2);
        assert!(dst.iter().map(|i| i.data.len()).sum::<usize>() <= 16);
    }

    #[test]
    fn a_held_steer_over_the_image_cap_drops_images_rather_than_growing() {
        // INVARIANT: the held-steer buffer is bounded in BYTES, images included.
        // Enforced by a_held_steer_over_the_image_cap_drops_images_rather_than_growing.
        let mut held: Vec<HeldSteer> = Vec::new();
        let big = || hotl_types::UserImage {
            media_type: "image/png".into(),
            data: "A".repeat(HELD_IMAGE_B64_MAX).into(),
        };
        for _ in 0..4 {
            push_or_fold_steer(
                &mut held,
                HeldSteer {
                    text: "go".into(),
                    images: vec![big()],
                },
            );
        }
        let total: usize = held
            .iter()
            .flat_map(|s| s.images.iter())
            .map(|i| i.data.len())
            .sum();
        assert!(total <= HELD_IMAGE_B64_MAX, "{total}");
    }

    #[test]
    fn a_fold_that_drops_an_image_still_ends_with_fold_mark() {
        // INVARIANT: FOLD_MARK is fold_into's literal tail after a truncating
        // fold, even when that same fold also drops an image and appends a
        // disclosure note — otherwise the next fold's FOLD_MARK-stripping
        // precondition silently fails. Enforced by
        // a_fold_that_drops_an_image_still_ends_with_fold_mark.
        let mut held: Vec<HeldSteer> = Vec::new();
        let img = |n: usize| hotl_types::UserImage {
            media_type: "image/png".into(),
            data: "A".repeat(n).into(),
        };
        // First push is unconditional: seed an entry with an image total
        // that leaves no room for another byte.
        push_or_fold_steer(
            &mut held,
            HeldSteer {
                text: "seed".into(),
                images: vec![img(HELD_IMAGE_B64_MAX)],
            },
        );
        // Second push both truncates (text alone is over HELD_BYTES_MAX) and
        // drops an image (the cap is already maxed) in the same fold.
        push_or_fold_steer(
            &mut held,
            HeldSteer {
                text: "y".repeat(HELD_BYTES_MAX + 4_096),
                images: vec![img(1)],
            },
        );
        let text = &held[0].text;
        assert!(
            text.ends_with(FOLD_MARK),
            "a fold that also drops an image must still end in FOLD_MARK: \
             ...{}",
            &text[text.len().saturating_sub(120)..]
        );
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
            crate::SampleStage::AtBoundary,
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
            crate::SampleStage::AtBoundary,
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
            crate::SampleStage::AtBoundary,
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
                crate::SampleStage::AtBoundary,
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
            crate::SampleStage::AtBoundary,
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
            crate::SampleStage::AtBoundary,
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
            crate::SampleStage::AtBoundary,
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
                crate::SampleStage::AtBoundary,
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

    // --- Task 10 (S2c held-steer boundary proof) ---------------------

    /// The mid-sample half of the held-steer rule, as a *checked* argument.
    ///
    /// `release_steers` rests on the fact that a turn commits nothing
    /// between granting itself a snapshot and the `Completed` group that
    /// closes the sample — so every ack the actor settles is genuinely
    /// between samples. No shipped site can violate that, which is exactly
    /// why it needs an assertion rather than a comment: the spec's own
    /// intra-sample `BlockEnd` pipelining (§Commit granularity) would create
    /// the first violator, and a steer released behind one lands ahead of
    /// the assistant item the model is still producing — 72a6f1b, silently
    /// back.
    ///
    /// This drives the guard with the declaration such a site would have to
    /// make.
    #[tokio::test]
    #[cfg_attr(
        not(debug_assertions),
        ignore = "drives a debug_assert, which release builds compile out"
    )]
    #[should_panic(expected = "a held steer may only land between samples")]
    async fn an_in_sample_commit_is_not_a_boundary_a_held_steer_may_land_at() {
        let dir = tempfile::tempdir().unwrap();
        let (shared, mut log, mut head) = test_shared(dir.path());
        let mut held = vec![super::HeldSteer {
            text: "hold me".to_string(),
            images: Vec::new(),
        }];
        release_steers(
            &shared,
            &mut log,
            &mut head,
            &mut Pipeline::default(),
            &mut held,
            super::Boundary::CommitSettled {
                stage: crate::SampleStage::InSample,
            },
        )
        .await;
    }

    /// …and the boundary every shipped site actually declares does release.
    #[tokio::test]
    async fn a_commit_that_closed_its_sample_releases_the_steer_it_held() {
        let dir = tempfile::tempdir().unwrap();
        let (shared, mut log, mut head) = test_shared(dir.path());
        let mut held = vec![super::HeldSteer {
            text: "hold me".to_string(),
            images: Vec::new(),
        }];
        release_steers(
            &shared,
            &mut log,
            &mut head,
            &mut Pipeline::default(),
            &mut held,
            super::Boundary::CommitSettled {
                stage: crate::SampleStage::AtBoundary,
            },
        )
        .await;
        assert!(held.is_empty(), "the steer must have landed");
        assert_eq!(head.items().len(), 1, "…and reached the projection");
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
            crate::SampleStage::AtBoundary,
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
