//! The session actor: sole committer, projection owner, turn scheduler.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use hotl_context::breakdown::ToolTokens;
use hotl_context::{compaction, tokens};
use hotl_platform::Clock;
use hotl_provider::{Effort, Provider, SamplingRequest, StreamEvent, ToolDef};
use hotl_store::SessionLog;
use hotl_tools::{
    rules::{PermissionMode, Rules},
    Registry,
};
use hotl_types::{assistant_text, EntryPayload, Item, SyntheticReason, Todo, TokenUsage};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{
    turn, EngineConfig, EngineEvent, GoalVerdictKind, Outcome, SessionCmd, SessionDeps, TurnEnd,
};
use hotl_context::goal::GoalVerdict;

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
/// The goal evaluator's output is two short lines; the bound is generous.
const GOAL_EVAL_MAX_TOKENS: u32 = 300;
const GOAL_EVAL_ATTEMPTS: u32 = 2;
/// Consecutive not-yet verdicts with no tool executed before the loop pauses
/// (0051 G1, OD2) — Claude Code's Stop-hook block cap, and the same number
/// `TURN_EXTENSION_MAX` bounds a single turn's nudges by.
const GOAL_STALL_TURNS: u32 = 8;
/// Wall-clock bound on the inline goal evaluation — the same posture as
/// [`COMPACT_SUMMARIZE_TIMEOUT`] (the actor's loop blocks for its duration,
/// and admission blocking during the call is its serialization working as
/// designed); on expiry the gate fails open rather than wedging.
const GOAL_EVAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
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
    /// `Arc<Item>` elements (0033 Task 5): a shared-head `make_mut` clone is
    /// pointer copies, never a deep copy of every string and image.
    items: Arc<Vec<Arc<Item>>>,
    /// The `todo_write` checklist — ephemeral session context that is never
    /// canon and never an entry in `items`. It rides the head rather than
    /// being stitched into it, so the published value stays *the durable
    /// projection*; the stitch happens per read, in [`Self::snapshot`], the
    /// same "ephemeral, request-only" shape the MOIM turn-context block has.
    /// A todo change is itself a durable `Todos` entry, so it moves
    /// `leaf`/`epoch` like any other commit.
    todos: Arc<Vec<Todo>>,
    /// The decisions log beside them (0056 T2). Rides the head for the same
    /// reason `todos` does — it is durable as a `Todos` entry, but it is not
    /// a projection item and must never become one.
    decisions: Arc<Vec<hotl_types::Decision>>,
    /// Running CONSERVATIVE-profile estimate of `items` (0033 Task 7),
    /// maintained at the two mutation sites so the pre-anchor estimate is
    /// O(1) instead of a per-char walk of the session. If a model-keyed
    /// profile ever lands, this sum must be keyed to it.
    estimated: u64,
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

    /// The plan's nodes, borrowed. `plan_state` clones both halves; the
    /// effort schedule's phase check runs at every turn start and only reads.
    pub(crate) fn nodes(&self) -> &[Todo] {
        &self.todos
    }

    /// The plan as it stands — what the goal gate's machine leaves read.
    pub fn plan_state(&self) -> crate::plan_state::PlanState {
        crate::plan_state::PlanState {
            todos: (*self.todos).clone(),
            decisions: (*self.decisions).clone(),
        }
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
            durable_estimate: self.estimated,
            tail: tail_for(&self.todos),
        }
    }
}

/// The ephemeral suffix a set of todos renders to. One definition, shared by
/// the published head and by the actor's own copy ([`Head::snapshot`]) — two
/// would be two answers to "what rides after the cache marker".
fn tail_for(todos: &[Todo]) -> Arc<Vec<Arc<Item>>> {
    match hotl_tools::todo::render_reminder(todos) {
        Some(reminder) => Arc::new(vec![Arc::new(reminder)]),
        None => empty_tail(),
    }
}

/// These carry a whole roster (or, for `workflow`, the plan schema it
/// teaches) inside `description()`/`schema()`, so `/context` bills them as
/// rows of their own instead of burying a session's biggest schema line in
/// the tool-schema total.
const SKILLS_TOOL: &str = "skill";
const AGENTS_TOOLS: [&str; 2] = ["spawn", "workflow"];

/// A tool definition's share of the request: the three strings that go on the
/// wire, under the session's own [`tokens::TokenProfile`] — the same ruler the
/// compaction trigger reads, so `/context` and the fold cannot disagree.
fn estimate_tool_def(def: &ToolDef, profile: &tokens::TokenProfile) -> u64 {
    tokens::estimate_text_with(&def.name, profile)
        + tokens::estimate_text_with(&def.description, profile)
        + tokens::estimate_text_with(&def.input_schema.to_string(), profile)
}

/// The three tool rows of a `/context` report. Split by name, never by index:
/// registry order is a registration detail.
fn tool_tokens(registry: &Registry, profile: &tokens::TokenProfile) -> ToolTokens {
    let mut out = ToolTokens::default();
    for def in registry.defs() {
        let n = estimate_tool_def(&def, profile);
        match def.name.as_str() {
            SKILLS_TOOL => out.skills += n,
            name if AGENTS_TOOLS.contains(&name) => out.agents += n,
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
    pub durable: Arc<Vec<Arc<Item>>>,
    /// Running CONSERVATIVE-profile estimate of `durable` — equals
    /// `tokens::estimate_items(&durable)` by construction (the projection
    /// maintains it at its two mutation sites; property-tested).
    pub durable_estimate: u64,
    /// Ephemeral per-sample suffix (todo reminder today), regenerated per
    /// read, never committed, rendered after every cache marker.
    pub tail: Arc<Vec<Arc<Item>>>,
}

/// The one shared empty ephemeral tail. A `OnceLock` rather than
/// `Arc::new(Vec::new())` per call so that a session with no todos — the
/// common case, and the one on the sample-boundary hot path — allocates
/// nothing to say "nothing ephemeral here".
pub(crate) fn empty_tail() -> Arc<Vec<Arc<Item>>> {
    static EMPTY: std::sync::OnceLock<Arc<Vec<Arc<Item>>>> = std::sync::OnceLock::new();
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
        decisions: Arc::new(Vec::new()),
        items: Arc::new(Vec::new()),
        todos: Arc::new(Vec::new()),
        estimated: 0,
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
    items: Arc<Vec<Arc<Item>>>,
    /// See [`ProjectionHead::estimated`] — maintained here, published there.
    estimated: u64,
    todos: Arc<Vec<Todo>>,
    decisions: Arc<Vec<hotl_types::Decision>>,
    leaf: Option<String>,
    epoch: u64,
    /// The session's token profile (0057 T6). The running `estimated` sum must
    /// be keyed to the same ruler the compaction trigger reads, or the O(1)
    /// pre-anchor estimate and the trigger disagree.
    profile: tokens::TokenProfile,
    /// First entry id this head ever applied, and the id of the newest
    /// `Compaction` entry — together they bound the log span a fold's digest
    /// was computed from (`EntryPayload::Compaction::source_range`, 0057 T4).
    /// Two ids rather than a per-item id vector: the projection carries no
    /// entry ids, and putting one on every item would put a `String` clone on
    /// the commit hot path 0033 exists to keep cheap.
    first_entry: Option<String>,
    last_fold: Option<String>,
}

impl Head {
    /// Seed the head with the session's starting projection and publish it,
    /// so the very first read (a resumed session's `Continue`, or `fork`)
    /// never sees the empty placeholder [`head_channel`] created.
    fn new(
        tx: tokio::sync::watch::Sender<Arc<ProjectionHead>>,
        items: Vec<Item>,
        todos: Vec<Todo>,
        decisions: Vec<hotl_types::Decision>,
        profile: tokens::TokenProfile,
    ) -> Self {
        let items: Vec<Arc<Item>> = items.into_iter().map(Arc::new).collect();
        let estimated = tokens::estimate_items_with(&items, &profile);
        let mut head = Self {
            tx,
            items: Arc::new(items),
            estimated,
            todos: Arc::new(todos),
            decisions: Arc::new(decisions),
            leaf: None,
            epoch: 0,
            profile,
            first_entry: None,
            last_fold: None,
        };
        head.publish();
        head
    }

    fn items(&self) -> &Arc<Vec<Arc<Item>>> {
        &self.items
    }

    fn todos(&self) -> &Arc<Vec<Todo>> {
        &self.todos
    }

    /// The plan as it stands, for the goal gate's evidence and machine leaves.
    fn plan_state(&self) -> crate::plan_state::PlanState {
        crate::plan_state::PlanState {
            todos: (*self.todos).clone(),
            decisions: (*self.decisions).clone(),
        }
    }

    fn decisions(&self) -> &Arc<Vec<hotl_types::Decision>> {
        &self.decisions
    }

    /// The same two-channel read a turn takes off the published head, taken
    /// from the actor's own copy — the freshest one there is, and no watch
    /// round-trip. Read-only: nothing here advances or publishes.
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            durable: Arc::clone(&self.items),
            durable_estimate: self.estimated,
            tail: tail_for(&self.todos),
        }
    }

    /// Apply one acked item. Deliberately does NOT publish: a commit and the
    /// steer released behind it must reach the head as one visible step (see
    /// `release_steers`), or a turn's refresh could observe the commit
    /// without the steer that overtook it.
    fn apply(&mut self, item: Item) {
        self.estimated += tokens::estimate_item_with(&item, &self.profile);
        Arc::make_mut(&mut self.items).push(Arc::new(item));
    }

    /// Record the durable leaf and epoch an acked unit advanced the head to.
    fn advance(&mut self, leaf: String, seq: u64) {
        if self.first_entry.is_none() {
            self.first_entry = Some(leaf.clone());
        }
        self.leaf = Some(leaf);
        self.epoch = seq;
    }

    /// The log span a fold about to run covers: from the entry after the last
    /// fold (or this head's first entry) to the newest entry applied.
    fn fold_span(&self) -> Option<(String, String)> {
        let start = self
            .last_fold
            .clone()
            .or_else(|| self.first_entry.clone())?;
        Some((start, self.leaf.clone()?))
    }

    fn set_todos(&mut self, todos: Vec<Todo>) {
        self.todos = Arc::new(todos);
    }

    /// Append decisions, stamping each with the session clock — the model
    /// supplies `what`/`why` and never the time.
    fn append_decisions(&mut self, new: Vec<hotl_types::Decision>, now_ms: u64) {
        if new.is_empty() {
            return;
        }
        let mut all = (*self.decisions).clone();
        all.extend(new.into_iter().map(|d| hotl_types::Decision {
            when_ms: now_ms,
            ..d
        }));
        self.decisions = Arc::new(all);
    }

    /// Re-point the projection after a fold. The published head moves without
    /// an epoch bump: the compaction entry's own commit already advanced
    /// `leaf`/`epoch`, and this is that entry taking effect. Unobservable
    /// mid-turn by construction — the actor terminates a turn *before*
    /// committing a compaction (§Read invariant).
    fn repoint(&mut self, items: Vec<Arc<Item>>) {
        // O(folded list), rare, and correct by construction.
        self.estimated = tokens::estimate_items_with(&items, &self.profile);
        self.items = Arc::new(items);
        self.publish();
    }

    /// The one publish site. `watch::Sender::send` never fails here: the
    /// actor holds a receiver for the whole session inside `SharedDeps`.
    fn publish(&mut self) {
        let _ = self.tx.send(Arc::new(ProjectionHead {
            items: Arc::clone(&self.items),
            todos: Arc::clone(&self.todos),
            decisions: Arc::clone(&self.decisions),
            estimated: self.estimated,
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
    /// Wrap-up mode (0058 T8): the session is out of turn budget and has one
    /// prompt left to record a result. A third roster axis rather than a
    /// second meaning for `plan`, because it narrows further — everything
    /// that is not a read or `report_result` disappears — and the two are set
    /// by different callers for different reasons.
    wrapup: AtomicBool,
    /// The session's current effort, separate from `config.effort` (the
    /// startup default) so `SetEffort` can flip it without rebuilding
    /// `EngineConfig`. Encoded `0` = unset (provider default), `1..=5` = the
    /// ladder ascending — the sentinel is why this is an `AtomicU8` and not a
    /// packed `Option`, and it keeps the same shape as `mode`'s codec.
    effort: AtomicU8,
    /// The human set a rung by hand (`/effort`, `session/set_effort`), so the
    /// phase schedule stops writing over it — a deliberate pin outranks
    /// config (0059 T1).
    effort_pinned: AtomicBool,
    /// Tool calls this session has run, against `config.max_tool_calls`.
    /// Session-scoped, not per-turn: the cap exists so an unattended session
    /// cannot run forever, and a fresh turn is not a fresh budget.
    tool_calls: AtomicU64,
    /// Session spend in micro-dollars, against `config.max_cost_usd`.
    /// Integer so the running total is an atomic; the price of one turn is
    /// far above a millionth of a dollar, so nothing meaningful rounds away.
    spend_micros: AtomicU64,
    /// Which budget thresholds have already been announced (bit 0 = 50%,
    /// 1 = 80%, 2 = 100%), so each fires once per session.
    budget_notices: AtomicU8,
    /// Did the most recent tool batch run a verify-class command and no edit?
    /// Written by the turn task, read at the next turn's start to pick the
    /// schedule's phase. An atomic rather than a `TurnContinuation` field:
    /// a compaction respawn is the same turn and must keep the same answer.
    last_batch_verified: AtomicBool,
    pub sandbox_enforced: bool,
    pub clock: Arc<dyn Clock>,
    pub system: Arc<str>,
    /// `tokens::estimate_text(&system)` computed once — the system prompt is
    /// a byte-stable session constant (0033 Task 7).
    pub system_estimate: u64,
    pub cwd: PathBuf,
    pub config: EngineConfig,
    /// The Layer-B budget every tool call in this session's batches draws a
    /// `subproc()` permit from — see `SessionDeps::concurrency`.
    pub concurrency: hotl_tools::concurrency::SessionConcurrency,
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
    /// The session's ULID, sent as every sample's `SamplingRequest::cache_key`
    /// (0045 D1): stable across resume, distinct per fork, shared by the
    /// speculative sample — which IS the next prefix.
    pub session_id: Arc<str>,
    /// Monotonic masking-rules epoch (commit-protocol.md §Proposal
    /// payloads' `rules_epoch` guard). Today masking rules never change
    /// mid-session, so this is constant for the life of a `SharedDeps` — the
    /// guard is implemented anyway, ahead of whatever eventually bumps it.
    rules_epoch: std::sync::atomic::AtomicU32,
    /// Where this session files its plan artifact, if anywhere (0056 T2).
    plan_files: Option<crate::PlanFiles>,
    /// What the harness observed commands do (0056 T4). Session-scoped, not
    /// turn-scoped: a goal loop spans turns, and "has this run since the last
    /// edit" is exactly the question a per-turn ledger could not answer.
    pub(crate) command_ledger: std::sync::Mutex<hotl_context::goal::CommandLedger>,
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

/// `0` is "unset", not a rung — the ladder starts at 1 so "the provider's own
/// default" stays representable.
fn effort_to_u8(effort: Option<Effort>) -> u8 {
    match effort {
        None => 0,
        Some(Effort::Low) => 1,
        Some(Effort::Medium) => 2,
        Some(Effort::High) => 3,
        Some(Effort::XHigh) => 4,
        Some(Effort::Max) => 5,
    }
}

fn u8_to_effort(v: u8) -> Option<Effort> {
    match v {
        1 => Some(Effort::Low),
        2 => Some(Effort::Medium),
        3 => Some(Effort::High),
        4 => Some(Effort::XHigh),
        5 => Some(Effort::Max),
        _ => None,
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
        let effort = AtomicU8::new(effort_to_u8(deps.config.effort));
        let effort_pinned = AtomicBool::new(false);
        let tool_calls = AtomicU64::new(0);
        let spend_micros = AtomicU64::new(0);
        let budget_notices = AtomicU8::new(0);
        let last_batch_verified = AtomicBool::new(false);
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
        let session_id: Arc<str> = deps.log.session_id.as_str().into();
        let system: Arc<str> = deps.system.into();
        let system_estimate = tokens::estimate_text_with(&system, &deps.config.token_profile);
        let shared = Self {
            provider: deps.provider,
            registry: deps.registry,
            rules: deps.rules,
            mode,
            plan,
            wrapup: AtomicBool::new(false),
            effort,
            effort_pinned,
            tool_calls,
            spend_micros,
            budget_notices,
            last_batch_verified,
            sandbox_enforced: deps.sandbox_enforced,
            clock: deps.clock,
            system,
            system_estimate,
            cwd: deps.cwd,
            config: deps.config,
            concurrency: deps.concurrency,
            hooks: deps.hooks,
            hook_mask,
            notifications,
            masker,
            session_id,
            rules_epoch: std::sync::atomic::AtomicU32::new(0),
            head_rx,
            plan_files: deps.plan_files,
            command_ledger: std::sync::Mutex::new(hotl_context::goal::CommandLedger::default()),
        };
        (shared, deps.log)
    }

    /// Render the plan artifact for this session, if it files one. Every
    /// failure is swallowed: a read-only data dir must not fail a
    /// `todo_write`, and the log already holds the truth.
    fn write_plan_artifact(&self, state: &crate::plan_state::PlanState) {
        let Some(files) = self.plan_files.as_ref() else {
            return;
        };
        let artifact = crate::plan_state::PlanArtifact::new(
            files.project.clone(),
            self.session_id.to_string(),
            self.clock.now_ms(),
            state,
        );
        let _ = crate::plan_state::write_artifact(&files.dir, &artifact);
        if let Some(mirror) = files.repo_mirror.as_ref() {
            let _ = crate::plan_state::mirror_markdown(mirror, &artifact);
        }
    }

    /// The session's plan as markdown, for the compaction digest (0057's
    /// COPY VERBATIM list names the plan's DECISIONS, and the model cannot
    /// copy what it was not shown). Read off the published head, so it is the
    /// plan as it stands at the fold. `None` when there is no plan yet.
    pub(crate) fn plan_markdown(&self) -> Option<String> {
        let state = self.head_rx.borrow().plan_state();
        (!state.is_empty()).then(|| state.markdown())
    }

    /// The human-readable half of the artifact, for the `present_plan` reply
    /// and the surfaces' card. `None` when this session files none.
    pub(crate) fn plan_artifact_path(&self) -> Option<String> {
        self.plan_files
            .as_ref()
            .map(|f| f.dir.join("current.md").display().to_string())
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

    /// Wrap-up right now (0058 T8). Not durable: it belongs to one prompt at
    /// the end of a child's life, and a resumed session has a fresh budget.
    pub(crate) fn effective_wrapup(&self) -> bool {
        self.wrapup.load(Ordering::Relaxed)
    }

    fn set_wrapup(&self, on: bool) {
        self.wrapup.store(on, Ordering::Relaxed);
    }

    /// Runtime plan-mutation entry point (`SessionCmd::SetPlan`, reachable via
    /// ACP `session/set_plan` and the TUI `/plan` command). No `enforced_mode`
    /// counterpart: the overlay only ever adds an ask, so no build tightens it.
    fn set_plan(&self, plan: bool) {
        self.plan.store(plan, Ordering::Relaxed);
    }

    /// The depth the next request should carry — not `config.effort`, which is
    /// only ever the startup default.
    pub(crate) fn effective_effort(&self) -> Option<Effort> {
        u8_to_effort(self.effort.load(Ordering::Relaxed))
    }

    /// Runtime effort-mutation entry point (`SessionCmd::SetEffort`, reachable
    /// via ACP `session/set_effort` and the TUI `/effort` command). No
    /// `enforced_mode` counterpart: no build tightens effort.
    fn set_effort(&self, effort: Option<Effort>) {
        self.effort.store(effort_to_u8(effort), Ordering::Relaxed);
        // A hand-set rung is a decision, and the schedule is configuration:
        // the decision wins for the rest of the session.
        self.effort_pinned.store(true, Ordering::Relaxed);
    }

    /// Would this batch cross `max_tool_calls`? Checked once per batch, so a
    /// wide batch may cross by its own width — the cap is a runaway backstop.
    /// `Some((used, cap))` refuses.
    pub(crate) fn tool_call_budget(&self) -> Option<(f64, f64)> {
        let cap = self.config.max_tool_calls;
        if cap == 0 {
            return None;
        }
        let used = self.tool_calls.load(Ordering::Relaxed);
        (used >= cap).then_some((used as f64, cap as f64))
    }

    /// Count `n` calls against the session's tool-call budget.
    pub(crate) fn count_tool_calls(&self, n: u64) {
        self.tool_calls.fetch_add(n, Ordering::Relaxed);
    }

    /// Session spend so far, in USD.
    pub(crate) fn spent_usd(&self) -> f64 {
        self.spend_micros.load(Ordering::Relaxed) as f64 / 1_000_000.0
    }

    /// Fold one sample's cost into the session total and report any spend
    /// threshold it just crossed. Each threshold is announced once.
    pub(crate) fn add_spend(&self, usd: f64) -> Option<u8> {
        if usd > 0.0 {
            self.spend_micros
                .fetch_add((usd * 1_000_000.0) as u64, Ordering::Relaxed);
        }
        let cap = self.config.max_cost_usd;
        if cap <= 0.0 {
            return None;
        }
        let pct = (self.spent_usd() / cap * 100.0) as u32;
        // Highest crossed threshold first: a jump straight past 80 to 100
        // should say 100, not 50.
        for (bit, threshold) in [(2u8, 100u32), (1, 80), (0, 50)] {
            if pct >= threshold {
                let mask = 1u8 << bit;
                let before = self.budget_notices.fetch_or(mask, Ordering::Relaxed);
                if before & mask == 0 {
                    return Some(threshold as u8);
                }
                return None;
            }
        }
        None
    }

    /// What the last tool batch was: a verify-class command with no edit
    /// alongside it. The turn task's half of the phase derivation (0059 T1).
    pub(crate) fn note_batch_verified(&self, verified: bool) {
        self.last_batch_verified.store(verified, Ordering::Relaxed);
    }

    /// Which phase the next turn opens in. `plan` when plan mode is on, or
    /// when the plan is between steps; a previous batch that only verified
    /// means `verify`; else `implement`. Plan outranks verify deliberately —
    /// deciding what a green test run means is planning, not verifying.
    fn turn_phase(&self) -> hotl_provider::Phase {
        if self.effective_plan() || self.plan_is_between_steps() {
            hotl_provider::Phase::Plan
        } else if self.last_batch_verified.load(Ordering::Relaxed) {
            hotl_provider::Phase::Verify
        } else {
            hotl_provider::Phase::Implement
        }
    }

    /// Work left, and nothing being worked on (0056 T1) — the moment the last
    /// step landed and the next has not been chosen.
    ///
    /// Both guards are load-bearing. An **empty** plan is not a plan: without
    /// the `has_open_nodes` clause every session that never calls `todo_write`
    /// would sit in `plan` forever, since a list of nothing trivially has no
    /// node in progress. An **all-completed** plan is finished work, not an
    /// unfinished one. `Failed` and `NeedsMoreSteps` count as open, which is
    /// exactly right: those are the nodes worth thinking about rather than
    /// grinding at. A `replan` node needs no special case — while it runs it
    /// is implementation, and when it lands the plan is between steps again.
    fn plan_is_between_steps(&self) -> bool {
        let head = self.head_rx.borrow();
        let nodes = head.nodes();
        crate::plan_state::has_open_nodes(nodes)
            && !nodes
                .iter()
                .any(|n| n.status == hotl_types::TodoStatus::InProgress)
    }

    /// Write the schedule's rung for this turn's phase, at turn start only —
    /// so every sample inside one turn carries one depth. Appends no
    /// `EffortSet` entry: the schedule is config, and `hotl resume` re-derives
    /// it. A pinned session is left alone.
    fn apply_effort_schedule(&self) {
        let Some(schedule) = self.config.effort_schedule else {
            return;
        };
        if self.effort_pinned.load(Ordering::Relaxed) {
            return;
        }
        let rung = schedule.rung(self.turn_phase()).or(self.config.effort);
        self.effort.store(effort_to_u8(rung), Ordering::Relaxed);
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
    // Resumed history is repaired on the way in, both for a steer that landed
    // mid-batch (`pair_tool_results`) and for a crash that persisted an
    // assistant's tool calls without their results (`close_dangling_batches`)
    // — either would otherwise fail every request on the session forever.
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
        close_dangling_batches(pair_tool_results(std::mem::take(&mut deps.initial_items))),
        std::mem::take(&mut deps.initial_todos),
        std::mem::take(&mut deps.initial_decisions),
        deps.config.token_profile,
    );
    let mut running = false;
    // The active goal (0034). In-memory beyond the seed, so the turn counter
    // resets on resume by design; the condition itself is durable (`GoalSet`).
    let goal_started_ms = deps.clock.now_ms();
    let mut goal: Option<GoalState> = deps
        .initial_goal
        .take()
        .map(|c| GoalState::new(c, goal_started_ms));
    let mut queue: VecDeque<QueuedPrompt> = VecDeque::new();
    // Steers that arrived while a turn was live (or a tool batch open),
    // waiting for a boundary before they can be appended.
    let mut held_steers: Vec<HeldSteer> = Vec::new();
    let (shared, mut log) = SharedDeps::new(deps, notifications, head_rx);
    let shared = Arc::new(shared);
    // Usage carried across compaction respawns within one logical turn.
    let mut carry_usage = TokenUsage::default();
    // The misprediction count's twin of `carry_usage` (0050 T5): a compaction
    // respawn or a goal-loop continuation is still one prompt, and the single
    // `TurnDone` reports the whole prompt's number.
    let mut carry_mispredictions: u32 = 0;
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
                    !held_steers.is_empty(),
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
                    !held_steers.is_empty(),
                )
                .await;
            }
            SessionCmd::Continue => {
                if !running && crate::needs_continuation(head.items()) {
                    spawn_turn(&shared, &cmd_tx, &events, &current_turn, None);
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
            SessionCmd::SetWrapUp(on) => shared.set_wrapup(on),
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
                // The overlay was invisible before 0050 T2: the roster moves
                // under the model at the next boundary, so it is told here,
                // in the same arm, and the toggle stays one cache break.
                let body = if plan {
                    hotl_context::PLAN_ON_REMINDER
                } else {
                    hotl_context::PLAN_OFF_REMINDER
                };
                let _ = shared
                    .append(
                        &mut log,
                        &mut pipeline,
                        &mut head,
                        EntryPayload::Item {
                            item: Item::User {
                                text: format!("<system-reminder>{body}</system-reminder>"),
                                synthetic: Some(SyntheticReason::SystemReminder),
                                images: Vec::new(),
                            },
                        },
                    )
                    .await;
            }
            SessionCmd::SetEffort(effort) => {
                // Same shape as `SetPlan`: atomic first so the next request
                // carries it, then the durable entry `hotl resume` replays.
                // Without the entry a resumed session would silently revert to
                // the config default while the status line still showed what
                // the user set.
                shared.set_effort(effort);
                let _ = shared
                    .append(
                        &mut log,
                        &mut pipeline,
                        &mut head,
                        EntryPayload::EffortSet {
                            effort: effort.map(|e| e.as_str().to_string()),
                        },
                    )
                    .await;
            }
            SessionCmd::PresentPlan { summary, nodes } => {
                // The same durable path a `todo_write` takes — a presented
                // plan IS the plan, not a proposal held somewhere else — plus
                // the event that puts it in front of the human.
                head.set_todos(nodes);
                let items = (**head.todos()).clone();
                let decisions = (**head.decisions()).clone();
                let _ = shared
                    .append(
                        &mut log,
                        &mut pipeline,
                        &mut head,
                        EntryPayload::Todos {
                            items: items.clone(),
                            decisions: decisions.clone(),
                        },
                    )
                    .await;
                shared.write_plan_artifact(&crate::plan_state::PlanState {
                    todos: items.clone(),
                    decisions,
                });
                let _ = events
                    .send(EngineEvent::TodosChanged {
                        items: items.clone(),
                    })
                    .await;
                let _ = events
                    .send(EngineEvent::PlanPresented {
                        summary,
                        nodes: items,
                        path: shared.plan_artifact_path(),
                    })
                    .await;
            }
            SessionCmd::SetTodos {
                todos: new_todos,
                decisions: new_decisions,
            } => {
                // Nodes replace, decisions append: the model rewrites the
                // whole list every call, but "why we chose this" is a fact
                // that happened — a rewrite must not be able to erase one.
                let now_ms = shared.clock.now_ms();
                head.set_todos(new_todos);
                head.append_decisions(new_decisions, now_ms);
                let items = (**head.todos()).clone();
                let decisions = (**head.decisions()).clone();
                let _ = shared
                    .append(
                        &mut log,
                        &mut pipeline,
                        &mut head,
                        EntryPayload::Todos {
                            items: items.clone(),
                            decisions: decisions.clone(),
                        },
                    )
                    .await;
                // After the durable append, never before: the artifact is a
                // convenience view of the log, and a file that leads it would
                // be a second source of truth for the plan.
                shared.write_plan_artifact(&crate::plan_state::PlanState {
                    todos: items.clone(),
                    decisions,
                });
                let _ = events.send(EngineEvent::TodosChanged { items }).await;
            }
            SessionCmd::SetGoal(condition) => {
                // The `SetTodos` append+emit shape. Clearing when no goal is
                // active is a silent no-op — no tombstone, no event.
                let clearing_nothing = condition.is_none() && goal.is_none();
                if !clearing_nothing {
                    // Resume/replace restarts the counters with the rest of it.
                    let now_ms = shared.clock.now_ms();
                    goal = condition.clone().map(|c| GoalState::new(c, now_ms));
                    let outcome = condition.is_none().then(|| "cleared".to_string());
                    let _ = shared
                        .append(
                            &mut log,
                            &mut pipeline,
                            &mut head,
                            EntryPayload::GoalSet {
                                condition: condition.clone(),
                                outcome,
                            },
                        )
                        .await;
                    let _ = events.send(EngineEvent::GoalChanged { condition }).await;
                }
            }
            SessionCmd::ContextBreakdown { reply } => {
                // The one arm that reads and returns. No append, no publish,
                // no `.await` — that is what makes `/context` safe mid-turn.
                let snap = head.snapshot();
                let _ = reply.send(hotl_context::breakdown::breakdown(
                    &shared.system,
                    tool_tokens(&shared.registry, &shared.config.token_profile),
                    &snap.durable,
                    &snap.tail,
                    shared.config.context_window,
                    &shared.config.token_profile,
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
            SessionCmd::TurnFinished {
                end,
                usage,
                mispredictions,
                tools_ran,
                unrecoverable,
            } => {
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
                        steers_held: !held_steers.is_empty(),
                        log: &mut log,
                        head: &mut head,
                        pipeline: &mut pipeline,
                        queue: &mut queue,
                        running: &mut running,
                        carry_usage: &mut carry_usage,
                        carry_mispredictions: &mut carry_mispredictions,
                        compact_streak: &mut compact_streak,
                        goal: &mut goal,
                        cmd_tx: &cmd_tx,
                        events: &events,
                        current_turn: &current_turn,
                    },
                    end,
                    usage,
                    mispredictions,
                    tools_ran,
                    unrecoverable,
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
            crate::hooks::call_session_end(hooks, crate::hooks::ACTOR_MAIN).await;
        },
        else {}
    );
}

/// The mutable session state `on_turn_finished` threads back into the loop.
struct TurnFinishedCtx<'a> {
    shared: &'a Arc<SharedDeps>,
    /// Whether steers are held for release right now — a gate clause of the
    /// fast admission path (0033 Task 10).
    steers_held: bool,
    log: &'a mut SessionLog,
    head: &'a mut Head,
    pipeline: &'a mut Pipeline,
    queue: &'a mut VecDeque<QueuedPrompt>,
    running: &'a mut bool,
    carry_usage: &'a mut TokenUsage,
    /// `carry_usage`'s twin for mispredictions (0050 T5).
    carry_mispredictions: &'a mut u32,
    compact_streak: &'a mut u32,
    goal: &'a mut Option<GoalState>,
    cmd_tx: &'a mpsc::WeakSender<SessionCmd>,
    events: &'a mpsc::Sender<EngineEvent>,
    current_turn: &'a Arc<Mutex<CancellationToken>>,
}

/// The active `/goal` (0034). In-memory only (the condition is separately
/// durable as `GoalSet`), so the turn counter resets to zero on resume —
/// per the docs, elapsed time and turns-taken restart with the process.
struct GoalState {
    condition: String,
    turns: u32,
    /// Consecutive not-yet verdicts with no tool executed. Zeroed by any
    /// turn that ran one, and by the stall itself — the pause is a rest, not
    /// a tombstone, so the next prompt starts from a full budget again.
    idle_turns: u32,
    /// When the goal was set (or restored), for the progress line's elapsed.
    started_ms: u64,
    /// Tokens spent since then, the evaluator's own calls included (0051 G4)
    /// — what the loop actually cost, not what the last turn cost.
    spent: TokenUsage,
}

impl GoalState {
    fn new(condition: String, now_ms: u64) -> Self {
        Self {
            condition,
            turns: 0,
            idle_turns: 0,
            started_ms: now_ms,
            spent: TokenUsage::default(),
        }
    }

    fn progress(&self, now_ms: u64) -> hotl_context::goal::GoalProgress {
        hotl_context::goal::GoalProgress {
            turns: self.turns,
            elapsed_secs: now_ms.saturating_sub(self.started_ms) / 1_000,
            input_tokens: self.spent.input_tokens
                + self.spent.cache_read_input_tokens
                + self.spent.cache_creation_input_tokens,
            output_tokens: self.spent.output_tokens,
        }
    }
}

/// A turn ended: either report it (and promote the queue) or, on a compaction
/// request, fold and respawn the continuation.
async fn on_turn_finished(
    ctx: TurnFinishedCtx<'_>,
    end: TurnEnd,
    mut usage: TokenUsage,
    mut mispredictions: u32,
    tools_ran: u32,
    unrecoverable: bool,
) {
    // Compaction's own dead end (the streak cap) is the third unrecoverable
    // class, and only `try_compact` knows it happened.
    let mut unrecoverable = unrecoverable;
    let outcome = match end {
        TurnEnd::Outcome(outcome) => Some(outcome),
        TurnEnd::Compact { spec, cont } => {
            *ctx.carry_usage += usage;
            usage = TokenUsage::default();
            // The continuation carries its own running count in
            // `TurnContinuation`, so folding it here too would double it.
            mispredictions = 0;
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
                &mut unrecoverable,
                ctx.carry_usage,
            )
            .await
        }
        TurnEnd::Clear {
            ids,
            cont,
            spec_usage,
        } => {
            *ctx.carry_usage += usage;
            // A digest the clear abandoned was still billed (0051 decision 6).
            *ctx.carry_usage += spec_usage;
            usage = TokenUsage::default();
            mispredictions = 0;
            try_clear(
                ctx.shared,
                ctx.log,
                ctx.head,
                ctx.pipeline,
                ids,
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
        // The owner has to act (0051 G6): tombstone, so resume never re-arms
        // the loop against a dead key. Before the Done gate deliberately —
        // an error outcome never reaches it, and leaving the goal armed here
        // is how a revoked credential burns a whole session's budget.
        if let (true, Outcome::Error { message }) = (unrecoverable, &outcome) {
            if let Some(state) = ctx.goal.take() {
                let _ = ctx
                    .shared
                    .append(
                        ctx.log,
                        ctx.pipeline,
                        ctx.head,
                        EntryPayload::GoalSet {
                            condition: None,
                            outcome: Some("error".into()),
                        },
                    )
                    .await;
                let _ = ctx
                    .events
                    .send(EngineEvent::GoalChanged { condition: None })
                    .await;
                let _ = ctx
                    .events
                    .send(EngineEvent::GoalVerdict {
                        verdict: GoalVerdictKind::Errored,
                        reason: message.clone(),
                        turns: state.turns,
                        usage: state.spent,
                        // An unrecoverable error is about the provider, not
                        // about the work: no validation has anything to say.
                        evidence: Vec::new(),
                    })
                    .await;
            }
        }
        // The goal gate (0034): between outcome resolution and `end_turn`,
        // exactly where the compaction respawn sits. Fires only on Done +
        // active goal + empty queue — a user-queued prompt outranks the
        // continuation (the goal re-evaluates after it), and a non-Done
        // outcome (interrupt, error) returns control with the goal intact.
        if matches!(outcome, Outcome::Done { .. }) && ctx.queue.is_empty() {
            if let Some(state) = ctx.goal.as_mut() {
                state.turns += 1;
                let turns = state.turns;
                // The turn's own tokens join the goal's running total before
                // the evaluator reads the progress line, so the number the
                // evaluator judges a token bound against is current.
                state.spent += usage;
                let now_ms = ctx.shared.clock.now_ms();
                let progress = state.progress(now_ms);
                // Esc during the evaluation returns control immediately: the
                // eval races the turn's cancel token (`try_compact`'s
                // pattern); cancel and timeout both fail open.
                let cancel = ctx
                    .current_turn
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                let snapshot = Arc::clone(ctx.head.items());
                // What the harness itself observed (0056 T4/T5), read once:
                // the evidence the evaluator reads, the machine leaves that
                // may settle the condition outright, and the post-filter that
                // refuses a `met` the validations refute.
                let plan = ctx.head.plan_state();
                let (evidence, machine, prose) = {
                    let ledger = ctx
                        .shared
                        .command_ledger
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let evidence = hotl_context::goal::evidence_lines(
                        &plan.todos,
                        &ledger,
                        &hotl_tools::rules::argv,
                    );
                    let oracle = hotl_context::goal::Oracle {
                        ledger: &ledger,
                        cwd: &ctx.shared.cwd,
                        turns,
                        argv: &hotl_tools::rules::argv,
                    };
                    let partial = hotl_context::goal::evaluate_machine(
                        &hotl_context::goal::parse(&state.condition),
                        &oracle,
                    );
                    (evidence, partial.decided, partial.prose)
                };
                // A machine leaf that settled it needs no model at all — the
                // cheapest verdict there is, and the one a transcript cannot
                // argue with.
                let settled = match (machine, prose.is_empty()) {
                    (Some(false), _) => Some((
                        GoalVerdict::NotYetMet,
                        "a check in the condition is not satisfied".to_string(),
                    )),
                    (Some(true), true) => Some((
                        GoalVerdict::Met,
                        "every check in the condition is satisfied".to_string(),
                    )),
                    _ => None,
                };
                let (verdict, eval_usage) = if let Some(v) = settled {
                    (Some(v), TokenUsage::default())
                } else {
                    // Only the prose remainder reaches the model.
                    let condition = if prose.is_empty() {
                        state.condition.clone()
                    } else {
                        prose.join(" and ")
                    };
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => (None, TokenUsage::default()),
                        v = tokio::time::timeout(
                            GOAL_EVAL_TIMEOUT,
                            evaluate_goal(ctx.shared, &snapshot[..], &condition, progress, &evidence),
                        ) => v.unwrap_or_default(),
                    }
                };
                // The post-filter (0056 T4): a `met` the harness's own
                // observations contradict is downgraded, naming the command.
                // A rubric asks the model to be honest; this does not ask.
                let verdict = match verdict {
                    Some((GoalVerdict::Met, reason)) => {
                        match evidence.iter().find(|e| !e.status.is_green()) {
                            Some(e) => Some((
                                GoalVerdict::NotYetMet,
                                format!(
                                    "`{}` has not run green this turn ({})",
                                    e.command, e.status
                                ),
                            )),
                            None => Some((GoalVerdict::Met, reason)),
                        }
                    }
                    other => other,
                };
                let evidence: Vec<String> = evidence.iter().map(ToString::to_string).collect();
                // Before every branch (0051 decision 6): met, impossible,
                // failed and stalled all report the evaluator's spend in the
                // final TurnDone, and not-yet accumulates it like a
                // compaction respawn.
                state.spent += eval_usage;
                *ctx.carry_usage += eval_usage;
                // Captured now: the met/impossible arm tombstones the goal
                // before it can report what the goal cost.
                let spent = state.spent;
                match verdict {
                    Some((GoalVerdict::NotYetMet, reason)) => {
                        let _ = ctx
                            .events
                            .send(EngineEvent::GoalVerdict {
                                verdict: GoalVerdictKind::NotYet,
                                reason: reason.clone(),
                                turns,
                                usage: spent,
                                evidence: evidence.clone(),
                            })
                            .await;
                        // A turn that only talked is idle (0051 decision 1);
                        // judged after the verdict, since a talk-only turn
                        // can legitimately be the one the evaluator calls met.
                        if tools_ran == 0 {
                            state.idle_turns += 1;
                        } else {
                            state.idle_turns = 0;
                        }
                        if state.idle_turns >= GOAL_STALL_TURNS {
                            // A pause, not a tombstone: the goal stays set and
                            // the next user prompt re-enters the gate with a
                            // full idle budget.
                            state.idle_turns = 0;
                            let _ = ctx
                                .events
                                .send(EngineEvent::GoalVerdict {
                                    verdict: GoalVerdictKind::Stalled,
                                    reason: format!(
                                        "no tool ran in the last {GOAL_STALL_TURNS} goal turns"
                                    ),
                                    turns,
                                    usage: spent,
                                    evidence: evidence.clone(),
                                })
                                .await;
                            // Falls through to `end_turn`: one TurnDone, as
                            // every other resolution.
                        } else {
                            // No `end_turn`, so no intermediate `TurnDone`:
                            // that one suppression keeps every surface in
                            // "turn running" (TUI phase machine, ACP's parked
                            // reply, headless run_until_idle), and the usage
                            // folds into carry like a compaction respawn so
                            // the single final TurnDone reports cumulative
                            // spend.
                            *ctx.carry_usage += usage;
                            *ctx.carry_mispredictions += mispredictions;
                            let guidance = hotl_context::goal::guidance_text(
                                &reason,
                                &state.condition,
                                state.progress(now_ms),
                            );
                            *ctx.running = start_turn(
                                ctx.shared,
                                ctx.log,
                                ctx.head,
                                ctx.pipeline,
                                QueuedPrompt {
                                    text: guidance,
                                    images: Vec::new(),
                                    synthetic: Some(SyntheticReason::GoalGuidance),
                                },
                                ctx.cmd_tx,
                                ctx.events,
                                ctx.current_turn,
                                ctx.steers_held,
                            )
                            .await;
                            return;
                        }
                    }
                    Some((v @ (GoalVerdict::Met | GoalVerdict::Impossible), reason)) => {
                        let (kind, word) = if v == GoalVerdict::Met {
                            (GoalVerdictKind::Met, "achieved")
                        } else {
                            (GoalVerdictKind::Impossible, "impossible")
                        };
                        // The tombstone: resume must never restore this goal.
                        *ctx.goal = None;
                        let _ = ctx
                            .shared
                            .append(
                                ctx.log,
                                ctx.pipeline,
                                ctx.head,
                                EntryPayload::GoalSet {
                                    condition: None,
                                    outcome: Some(word.into()),
                                },
                            )
                            .await;
                        let _ = ctx
                            .events
                            .send(EngineEvent::GoalChanged { condition: None })
                            .await;
                        let _ = ctx
                            .events
                            .send(EngineEvent::GoalVerdict {
                                verdict: kind,
                                reason,
                                turns,
                                usage: spent,
                                evidence: evidence.clone(),
                            })
                            .await;
                    }
                    None => {
                        // Fail open: keep the goal, end the turn — never
                        // trap the user in a loop on a broken evaluator.
                        let _ = ctx
                            .events
                            .send(EngineEvent::GoalVerdict {
                                verdict: GoalVerdictKind::EvalFailed,
                                reason: "goal evaluation returned no verdict".into(),
                                turns,
                                usage: spent,
                                evidence: evidence.clone(),
                            })
                            .await;
                    }
                }
            }
        }
        let mut total = usage;
        total += std::mem::take(ctx.carry_usage);
        let total_mispredictions = mispredictions + std::mem::take(ctx.carry_mispredictions);
        *ctx.running = end_turn(
            ctx.shared,
            ctx.log,
            ctx.head,
            ctx.pipeline,
            ctx.queue,
            outcome,
            total,
            total_mispredictions,
            ctx.cmd_tx,
            ctx.events,
            ctx.current_turn,
            ctx.steers_held,
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
    // Set when the streak cap is what ended the turn: no retry, fallback or
    // further fold can make room, so the goal gate must tombstone (0051 G6).
    unrecoverable: &mut bool,
    // The fold's own summarize is spend the turn pays for, so it rides the
    // same carry the turn's samples do (0051 decision 6).
    carry_usage: &mut TokenUsage,
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
        *unrecoverable = true;
        Err("context window exhausted — compaction can no longer make room".into())
    } else {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Some(Outcome::Cancelled),
            compacted = compact(shared, log, head, pipeline, spec) => compacted,
        }
    };
    match compacted {
        Ok((degraded, fold_usage)) => {
            *carry_usage += fold_usage;
            let _ = events.send(EngineEvent::Compacted { degraded }).await;
            if cancel.is_cancelled() {
                return Some(Outcome::Cancelled);
            }
            respawn_turn(shared, cmd_tx, events, cancel, *cont, None);
            None // still running: same logical turn continues
        }
        Err(message) => Some(Outcome::Error { message }),
    }
}

/// The ladder's cheap rung (0057): commit one `Cleared` entry, re-point the
/// projection to the stubs, and respawn the continuation. No streak cap —
/// unlike a fold this spends no model call and, being once per prompt
/// (`TurnContinuation::cleared`), it cannot spiral.
#[allow(clippy::too_many_arguments)]
async fn try_clear(
    shared: &Arc<SharedDeps>,
    log: &mut SessionLog,
    head: &mut Head,
    pipeline: &mut Pipeline,
    ids: Vec<String>,
    cont: Box<crate::TurnContinuation>,
    cmd_tx: &mpsc::WeakSender<SessionCmd>,
    events: &mpsc::Sender<EngineEvent>,
    current_turn: &Arc<Mutex<CancellationToken>>,
) -> Option<Outcome> {
    let cancel = current_turn
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    // Drain before reading `items`, exactly as `compact` does: a rewrite
    // planned against the pre-drain projection would miss entries the drain
    // landed.
    pipeline.drain(head, Resolution::Abort).await;
    if !shared
        .append(
            log,
            pipeline,
            head,
            EntryPayload::Cleared { ids: ids.clone() },
        )
        .await
    {
        return Some(Outcome::Error {
            message: "session log is sealed".into(),
        });
    }
    let mut items = head.items().as_ref().clone();
    let count = hotl_types::clear_results(&mut items, &ids);
    head.repoint(items);
    let _ = events.send(EngineEvent::Cleared { count }).await;
    if cancel.is_cancelled() {
        return Some(Outcome::Cancelled);
    }
    respawn_turn(shared, cmd_tx, events, cancel, *cont, None);
    None // still running: same logical turn continues
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
    mispredictions: u32,
    cmd_tx: &mpsc::WeakSender<SessionCmd>,
    events: &mpsc::Sender<EngineEvent>,
    current_turn: &Arc<Mutex<CancellationToken>>,
    steers_held: bool,
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
    let _ = events
        .send(EngineEvent::TurnDone {
            outcome,
            usage,
            mispredictions,
        })
        .await;
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
                steers_held,
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
fn awaiting_tool_results<I: std::borrow::Borrow<Item>>(items: &[I]) -> bool {
    matches!(
        items.last().map(std::borrow::Borrow::borrow),
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

/// The error results a batch nothing will ever answer gets, so the request
/// built from it is well-formed. Empty when the blocks hold no tool_use.
fn unanswered_results(blocks: &[serde_json::Value]) -> Vec<hotl_types::ToolResultItem> {
    hotl_types::assistant_tool_uses(blocks)
        .iter()
        .map(|tu| hotl_types::ToolResultItem {
            tool_use_id: tu.id.clone(),
            content: "Not executed (the turn ended first).".into(),
            is_error: true,
        })
        .collect()
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
    let Some(Item::Assistant { blocks }) = head.items().last().map(Arc::as_ref) else {
        return;
    };
    let results = unanswered_results(blocks);
    if results.is_empty() {
        return;
    }
    let payload = EntryPayload::Item {
        item: Item::ToolResults { results },
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

/// Answer every unanswered assistant tool_use, not just the tail: a later
/// prompt can shove a crash-orphaned call into mid-history. Runs after
/// `pair_tool_results`, so real results are already adjacent. In-memory only
/// (an append-only log has no mid-history edit) — re-runs each resume, no-ops
/// on an answered history.
pub(crate) fn close_dangling_batches(items: Vec<Item>) -> Vec<Item> {
    let mut out: Vec<Item> = Vec::with_capacity(items.len() + 1);
    let mut it = items.into_iter().peekable();
    while let Some(item) = it.next() {
        if let Item::Assistant { blocks } = &item {
            let results = unanswered_results(blocks);
            if !results.is_empty() && !matches!(it.peek(), Some(Item::ToolResults { .. })) {
                out.push(item);
                out.push(Item::ToolResults { results });
                continue;
            }
        }
        out.push(item);
    }
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
        Outcome::DenialSpiral { consecutive, total } => Some(format!(
            "denial spiral: {consecutive} in a row, {total} total"
        )),
        Outcome::Budget { kind, used, cap } => Some(crate::budget_refusal(kind, *used, *cap)),
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
) -> Result<(bool, TokenUsage), String> {
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
            let spec_usage = spec.usage;
            if spec.prefix_end < spec.kept_from && spec.kept_from <= head.items().len() {
                let plan = compaction::Plan {
                    prefix_end: spec.prefix_end,
                    kept_from: spec.kept_from,
                };
                let pins = pre_compact_pins(shared, head, &plan).await;
                let digest = vec![compaction::digest_item(&spec.text)];
                let payload = EntryPayload::Compaction {
                    digest: digest.clone(),
                    prefix_end: spec.prefix_end,
                    kept_from: spec.kept_from,
                    degraded: false,
                    pinned: pins.clone(),
                    source_range: head.fold_span(),
                };
                if !shared.append(log, pipeline, head, payload).await {
                    return Err("session log is sealed".into());
                }
                head.last_fold = head.leaf.clone();
                head.repoint(compaction::apply(head.items(), &plan, &digest, &pins));
                post_compact(shared, &spec.text).await;
                return Ok((false, spec_usage));
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
    let pins = pre_compact_pins(shared, head, &plan).await;
    // Timeout or two failed attempts: the floor digest keeps the session moving
    // rather than ending the turn on housekeeping.
    let (summary, fold_usage) =
        summarize_bounded(summarize(shared, folded), COMPACT_SUMMARIZE_TIMEOUT).await;
    // `text` is the digest body the `PostCompact` hook reads; the floor
    // digest has no summary to hand it, so it hands the empty string.
    let (digest, degraded, text) = match summary {
        Some(text) => (vec![compaction::digest_item(&text)], false, text),
        None => (vec![compaction::floor_digest()], true, String::new()),
    };
    let payload = EntryPayload::Compaction {
        digest: digest.clone(),
        prefix_end: plan.prefix_end,
        kept_from: plan.kept_from,
        degraded,
        pinned: pins.clone(),
        source_range: head.fold_span(),
    };
    if !shared.append(log, pipeline, head, payload).await {
        return Err("session log is sealed".into());
    }
    head.last_fold = head.leaf.clone();
    head.repoint(compaction::apply(head.items(), &plan, &digest, &pins));
    post_compact(shared, &text).await;
    Ok((degraded, fold_usage))
}

/// Ask the `PreCompact` hooks what to keep. Bounded by
/// [`crate::hooks::HOOK_CALL_TIMEOUT`], and never a veto: a hung or silent
/// hook folds with no pins rather than wedging a fold the window needs.
async fn pre_compact_pins(
    shared: &SharedDeps,
    head: &Head,
    plan: &compaction::Plan,
) -> Vec<String> {
    crate::hooks::hook_gate!(
        shared.hooks,
        shared.hook_mask(),
        crate::hooks::EventMask::PRE_COMPACT,
        |hooks| {
            let items = head.items();
            let folded_ids: Vec<String> = items[plan.prefix_end..plan.kept_from]
                .iter()
                .filter_map(|i| match &**i {
                    Item::ToolResults { results } => Some(results),
                    _ => None,
                })
                .flatten()
                .map(|r| r.tool_use_id.clone())
                .collect();
            let info = crate::hooks::CompactInfo {
                folded_ids,
                kept_from: plan.kept_from,
                estimate_pct: ((head.estimated.saturating_mul(100)
                    / shared.config.context_window.max(1))
                .min(100)) as u8,
            };
            crate::hooks::call_pre_compact(hooks, crate::hooks::ACTOR_MAIN, &info).await.pins
        },
        else Vec::new()
    )
}

/// Tell the `PostCompact` hooks what the model will read. Awaited under the
/// hook timeout like `SessionEnd`: no turn is in flight during a fold.
async fn post_compact(shared: &SharedDeps, digest: &str) {
    crate::hooks::hook_gate!(
        shared.hooks,
        shared.hook_mask(),
        crate::hooks::EventMask::POST_COMPACT,
        |hooks| crate::hooks::call_post_compact(hooks, crate::hooks::ACTOR_MAIN, digest).await,
        else ()
    );
}

/// The inline fold's summarize under a wall-clock bound. `None` on either a
/// failed summarize or an exceeded bound — both degrade to the floor digest,
/// which is why one return type covers them. Split out from [`compact`] so the
/// bound is testable without a session behind it. A summarize the bound cut
/// off reports no spend: the future was dropped, so nothing came back to
/// count.
async fn summarize_bounded(
    fut: impl std::future::Future<Output = (Option<String>, TokenUsage)>,
    bound: std::time::Duration,
) -> (Option<String>, TokenUsage) {
    tokio::time::timeout(bound, fut).await.unwrap_or_default()
}

/// Returns the digest text and what producing it cost: housekeeping the
/// session pays for is housekeeping the session's totals must show (0051
/// decision 6).
pub(crate) async fn summarize(
    shared: &SharedDeps,
    folded: &[Arc<Item>],
) -> (Option<String>, TokenUsage) {
    let model = shared.config.utility();
    let request = SamplingRequest {
        model,
        max_tokens: SUMMARIZE_MAX_TOKENS,
        system: compaction::SUMMARIZE_SYSTEM.into(),
        items: Arc::new(vec![Arc::new(Item::User {
            // The plan rides along (0056 T2): its DECISIONS are on the
            // digest's COPY VERBATIM list, and the model cannot copy what it
            // was not shown.
            text: compaction::summarize_prompt(folded, shared.plan_markdown().as_deref()),
            synthetic: None,
            images: Vec::new(),
        })]),
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
        // Keyless (0045 D2): a different prefix every call would only spend
        // the session key's ~15 req/min routing budget.
        cache_key: None,
    };
    let mut spent = TokenUsage::default();
    for _ in 0..SUMMARIZE_ATTEMPTS {
        let mut stream = shared.provider.stream(request.clone());
        let mut text: Option<String> = None;
        while let Some(event) = stream.next().await {
            match event {
                Ok(StreamEvent::Completed { blocks, usage, .. }) => {
                    spent += usage;
                    text = Some(assistant_text(&blocks));
                }
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
            return (Some(t), spent);
        }
    }
    (None, spent)
}

/// One goal evaluation against the durable projection (0034): `summarize`'s
/// shape — fast model with session-model fallback, no thinking, no cache,
/// bounded attempts. `None` = no parseable verdict (the gate fails open).
async fn evaluate_goal(
    shared: &SharedDeps,
    items: &[Arc<Item>],
    condition: &str,
    progress: hotl_context::goal::GoalProgress,
    evidence: &[hotl_context::goal::EvidenceLine],
) -> (Option<(GoalVerdict, String)>, TokenUsage) {
    let model = shared.config.utility();
    let request = SamplingRequest {
        model,
        max_tokens: GOAL_EVAL_MAX_TOKENS,
        system: hotl_context::goal::GOAL_EVAL_SYSTEM.into(),
        items: Arc::new(vec![Arc::new(Item::User {
            text: hotl_context::goal::eval_prompt(condition, progress, evidence, items),
            synthetic: None,
            images: Vec::new(),
        })]),
        // A one-shot call against a prompt that is different every time:
        // nothing to cache, and nothing ephemeral to append (`summarize`).
        ephemeral_tail: empty_tail(),
        tools: Vec::new().into(),
        thinking: false,
        effort: None,
        cache: hotl_provider::CachePolicy::Off,
        turn_context: None,
        // Keyless (0045 D2), as `summarize`.
        cache_key: None,
    };
    // The evaluator is not free: its tokens are the goal loop's, and every
    // attempt counts even when the reply is unparseable (0051 decision 6).
    let mut spent = TokenUsage::default();
    for _ in 0..GOAL_EVAL_ATTEMPTS {
        let mut stream = shared.provider.stream(request.clone());
        let mut text: Option<String> = None;
        while let Some(event) = stream.next().await {
            match event {
                Ok(StreamEvent::Completed { blocks, usage, .. }) => {
                    spent += usage;
                    text = Some(assistant_text(&blocks));
                }
                Ok(_) => {}
                Err(_) => {
                    text = None;
                    // Finish the stream rather than abandoning it mid-flight
                    // — same reasoning as `summarize`.
                    crate::turn::drain_to_end(&mut stream).await;
                    break;
                }
            }
        }
        if let Some(v) = text.as_deref().and_then(hotl_context::goal::parse_verdict) {
            return (Some(v), spent);
        }
    }
    (None, spent)
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
    steers_held: bool,
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
        steers_held,
    )
    .await
}

/// 0033 Task 10: whether prompt admission may take the ticketed fast path —
/// forward the prompt, spawn the turn immediately, and let it dispatch
/// sample 1 speculatively while the fsync resolves. ALL clauses must hold,
/// else the awaited path runs unchanged:
/// - `Pipelined` ack mode: a `Sync` session issues no tickets (the same
///   predicate that keeps `Turn::speculate` off the `Sync` comparison run);
/// - no `USER_PROMPT` hook unmasked: a hook may append a reminder entry,
///   which would move the leaf and turn every admission dispatch into a
///   billed mispredict;
/// - no steer held for release at this admission: its release commits an
///   entry for the same reason.
fn admission_may_speculate(
    ack_mode: crate::AckMode,
    hook_mask: crate::hooks::EventMask,
    steers_held: bool,
) -> bool {
    ack_mode == crate::AckMode::Pipelined
        && !hook_mask.contains(crate::hooks::EventMask::USER_PROMPT)
        && !steers_held
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
    steers_held: bool,
) -> bool {
    // Captured before `text` moves into the committed item — `UserPromptSubmit`
    // hooks (tier-1 gap #7) see the prompt exactly as submitted. Text only:
    // images are not exposed to hooks (v1) — the inline `[Image #N]` markers
    // still tell a hook one was attached.
    let prompt_for_hooks = prompt.text.clone();
    let item = Item::User {
        text: prompt.text,
        synthetic: prompt.synthetic,
        images: prompt.images,
    };
    // The fast path (0033 Task 10): the prompt rides the same ticketed
    // forward every proposal takes, the turn spawns with the ticket plus a
    // predicted snapshot, and the fsync resolves during TLS + TTFB. Any
    // intervening entry moves the leaf and refuses adoption (sequential
    // rebuild, byte-identical); a crash after dispatch but before the fsync
    // loses a prompt whose only effect was an un-adopted provider call —
    // exactly the exposure §Causal groups (b) already accepts, and barrier
    // (a) still guarantees no tool runs first.
    if admission_may_speculate(shared.config.ack_mode, shared.hook_mask(), steers_held) {
        let payload = EntryPayload::Item { item: item.clone() };
        if let Ok(entry) =
            crate::turn::prepare_entry(&payload, shared.masker(), shared.rules_epoch()).await
        {
            let (prepared, _) = entry.into_parts();
            let seq = pipeline.next_seq();
            match shared.forward_prepared(log, prepared) {
                Ok(forwarded) => {
                    let ticket = push_pending(
                        pipeline,
                        forwarded,
                        vec![item.clone()],
                        seq,
                        crate::SampleStage::AtBoundary,
                    );
                    // The next head, predicted: cheap pointer clones after
                    // Task 5. The head applies the real prompt on ack, as
                    // pending items always do — the projection invariant is
                    // untouched.
                    let mut durable: Vec<Arc<Item>> = (**head.items()).clone();
                    let durable_estimate =
                        head.estimated + tokens::estimate_item_with(&item, &head.profile);
                    durable.push(Arc::new(item));
                    let predicted = Snapshot {
                        durable: Arc::new(durable),
                        durable_estimate,
                        tail: tail_for(head.todos()),
                    };
                    spawn_turn(
                        shared,
                        cmd_tx,
                        events,
                        current_turn,
                        Some(crate::Admission { ticket, predicted }),
                    );
                    return true;
                }
                // Sealed at forward time: the same failure surface the
                // awaited path reports below.
                Err(_) => {
                    let _ = events
                        .send(EngineEvent::TurnDone {
                            outcome: Outcome::Error {
                                message: "session log is sealed".into(),
                            },
                            usage: TokenUsage::default(),
                            mispredictions: 0,
                        })
                        .await;
                    return false;
                }
            }
        }
        // Prepare failed (serialization) — fall through to the awaited path,
        // which will surface the same failure through `append`.
    }
    let payload = EntryPayload::Item { item };
    if !shared.append(log, pipeline, head, payload).await {
        let _ = events
            .send(EngineEvent::TurnDone {
                outcome: Outcome::Error {
                    message: "session log is sealed".into(),
                },
                usage: TokenUsage::default(),
                mispredictions: 0,
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
            if let Some(context) = crate::hooks::call_user_prompt(hooks, crate::hooks::ACTOR_MAIN, &prompt_for_hooks).await {
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
    spawn_turn(shared, cmd_tx, events, current_turn, None);
    true
}

/// Spawn a fresh turn task against the current projection, installing a new
/// interrupt token for it.
fn spawn_turn(
    shared: &Arc<SharedDeps>,
    cmd_tx: &mpsc::WeakSender<SessionCmd>,
    events: &mpsc::Sender<EngineEvent>,
    current_turn: &Arc<Mutex<CancellationToken>>,
    admission: Option<crate::Admission>,
) {
    let token = CancellationToken::new();
    *current_turn
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = token.clone();
    // Turn start is the only place the phase schedule moves the rung —
    // `respawn_turn` below is a continuation of the same turn and must not.
    shared.apply_effort_schedule();
    // A fresh prompt is a fresh turn: no counters carry in.
    respawn_turn(
        shared,
        cmd_tx,
        events,
        token,
        crate::TurnContinuation::default(),
        admission,
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
    admission: Option<crate::Admission>,
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
        admission,
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
                    mispredictions: 0,
                    tools_ran: 0,
                    // A panic is a hotl bug, not a dead credential.
                    unrecoverable: false,
                })
                .await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::COMPACT_SUMMARIZE_TIMEOUT;
    use super::{
        awaiting_tool_results, close_dangling_batches, commit_prepared, compact, fold_images,
        fold_into, pair_tool_results, push_or_fold_steer, release_steers, summarize_bounded, Head,
        HeldSteer, Pipeline, ProjectionHead, Resolution, SharedDeps, FOLD_MARK, HELD_BYTES_MAX,
        HELD_IMAGE_B64_MAX,
    };

    /// 0033 Task 10: each gate clause refuses the fast admission path on its
    /// own — `Sync` sessions issue no tickets, an unmasked `USER_PROMPT`
    /// hook may append a leaf-moving reminder, and a held steer's release
    /// commits an entry for the same reason.
    #[test]
    fn admission_speculates_only_when_every_gate_clause_holds() {
        use super::admission_may_speculate;
        use crate::hooks::EventMask;
        assert!(admission_may_speculate(
            crate::AckMode::Pipelined,
            EventMask::NONE,
            false
        ));
        assert!(
            !admission_may_speculate(crate::AckMode::Sync, EventMask::NONE, false),
            "a Sync session issues no tickets"
        );
        assert!(
            !admission_may_speculate(crate::AckMode::Pipelined, EventMask::USER_PROMPT, false),
            "an unmasked USER_PROMPT hook may move the leaf"
        );
        assert!(
            !admission_may_speculate(crate::AckMode::Pipelined, EventMask::ALL, false),
            "USER_PROMPT inside a wider mask still gates"
        );
        assert!(
            !admission_may_speculate(crate::AckMode::Pipelined, EventMask::NONE, true),
            "a held steer's release would move the leaf"
        );
        assert!(
            admission_may_speculate(crate::AckMode::Pipelined, EventMask::NOTIFICATION, false),
            "hooks that never write entries do not gate"
        );
    }

    /// 0033 Task 7: the projection's running estimate must equal a
    /// from-scratch `estimate_items` walk after every mutation the projection
    /// has — `apply` and `repoint` — in any interleaving.
    #[test]
    fn running_estimate_equals_batch_estimate_under_apply_and_repoint() {
        use hotl_context::tokens;
        let (tx, _rx) = super::head_channel();
        let mut head = Head::new(
            tx,
            vec![hotl_types::Item::System {
                text: "seed".into(),
            }],
            Vec::new(),
            Vec::new(),
            tokens::TokenProfile::CONSERVATIVE,
        );
        let mix: Vec<hotl_types::Item> = vec![
            hotl_types::Item::User {
                text: "ascii and 日本語 and 🦀".into(),
                synthetic: None,
                images: vec![hotl_types::UserImage {
                    media_type: "image/png".into(),
                    data: "aGVsbG8=".into(),
                }],
            },
            hotl_types::Item::Assistant {
                blocks: vec![
                    serde_json::json!({"type": "text", "text": "reply"}),
                    serde_json::json!({"type": "tool_use", "id": "t1", "name": "read", "input": {}}),
                ],
            },
            hotl_types::Item::ToolResults {
                results: vec![hotl_types::ToolResultItem {
                    tool_use_id: "t1".into(),
                    content: "result body".into(),
                    is_error: false,
                }],
            },
            hotl_types::Item::Unknown,
            hotl_types::Item::User {
                text: "follow-up".into(),
                synthetic: Some(hotl_types::SyntheticReason::Steer),
                images: Vec::new(),
            },
        ];
        let check = |head: &Head, step: &str| {
            assert_eq!(
                head.estimated,
                tokens::estimate_items(head.items()),
                "running sum diverged after {step}"
            );
        };
        check(&head, "seed");
        for (round, item) in mix.iter().cycle().take(12).enumerate() {
            head.apply(item.clone());
            check(&head, &format!("apply #{round}"));
            if round == 4 || round == 9 {
                // A fold: keep the first item, splice a digest, keep the tail.
                let mut folded: Vec<std::sync::Arc<hotl_types::Item>> =
                    vec![head.items()[0].clone()];
                folded.push(std::sync::Arc::new(hotl_types::Item::User {
                    text: format!("digest of round {round}"),
                    synthetic: Some(hotl_types::SyntheticReason::CompactionSummary),
                    images: Vec::new(),
                }));
                folded.extend(head.items().iter().skip(round).cloned());
                head.repoint(folded);
                check(&head, &format!("repoint #{round}"));
            }
        }
    }
    use hotl_store::SessionLog;
    use hotl_types::{EntryPayload, Item, SyntheticReason, ToolResultItem};
    use serde_json::json;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    #[test]
    fn appending_to_the_head_shares_existing_image_payloads() {
        let (tx, _rx) = tokio::sync::watch::channel(Arc::new(ProjectionHead {
            decisions: Arc::new(Vec::new()),
            estimated: 0,
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
            Vec::new(),
            hotl_context::TokenProfile::CONSERVATIVE,
        );
        head.apply(Item::Assistant { blocks: Vec::new() });
        let Item::User { images, .. } = &*head.items()[0] else {
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
        let (hung, spent) =
            summarize_bounded(std::future::pending(), COMPACT_SUMMARIZE_TIMEOUT).await;
        assert!(
            hung.is_none(),
            "a hung summarize must degrade to the floor digest, not stall the command loop"
        );
        assert_eq!(
            spent,
            hotl_types::TokenUsage::default(),
            "a dropped future reports no spend"
        );
        let (answered, _) = summarize_bounded(
            std::future::ready((
                Some("DIGEST".to_string()),
                hotl_types::TokenUsage::default(),
            )),
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
        assert!(!awaiting_tool_results::<Item>(&[]));
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

    /// The synthesized answer `close_dangling_batches` inserts — same shape
    /// as `close_open_batch`'s, so the two repair paths stay identical.
    fn unanswered(id: &str) -> Item {
        Item::ToolResults {
            results: vec![ToolResultItem {
                tool_use_id: id.into(),
                content: "Not executed (the turn ended first).".into(),
                is_error: true,
            }],
        }
    }

    #[test]
    fn a_dangling_tail_batch_is_answered() {
        // The reported crash shape: the log ends on an unanswered assistant
        // tool_use. Left alone it is the resume HTTP 400.
        let repaired = close_dangling_batches(vec![calls("t1")]);
        assert_eq!(repaired, vec![calls("t1"), unanswered("t1")]);
        // ...and the answered tail is now a turn worth continuing on resume.
        assert!(crate::needs_continuation(&repaired));
    }

    #[test]
    fn a_dangling_mid_history_batch_is_answered() {
        // A prompt typed after resume shoved the dangling call into
        // mid-history (start_turn appends unconditionally), where a tail-only
        // repair would miss it and the 400 would persist across restarts.
        let repaired = close_dangling_batches(vec![calls("t1"), user("typed after resume")]);
        assert_eq!(
            repaired,
            vec![calls("t1"), unanswered("t1"), user("typed after resume")]
        );
    }

    #[test]
    fn every_dangling_batch_is_answered_not_just_one() {
        let repaired = close_dangling_batches(vec![calls("t1"), calls("t2")]);
        assert_eq!(
            repaired,
            vec![calls("t1"), unanswered("t1"), calls("t2"), unanswered("t2")]
        );
    }

    #[test]
    fn an_answered_history_is_left_alone_and_repair_is_idempotent() {
        let good = vec![user("start"), calls("t1"), answers("t1"), says("done")];
        assert_eq!(close_dangling_batches(good.clone()), good);
        // A plain assistant turn (no tool_use) is never touched.
        assert_eq!(close_dangling_batches(vec![says("hi")]), vec![says("hi")]);
        // Re-running over a repaired history changes nothing.
        let once = close_dangling_batches(vec![calls("t1"), user("x")]);
        assert_eq!(close_dangling_batches(once.clone()), once);
    }

    // --- Task 8 (S2a PreparedPayload) --------------------------------

    fn test_deps(dir: &std::path::Path, log: hotl_store::SessionLog) -> crate::SessionDeps {
        crate::SessionDeps {
            concurrency: Default::default(),
            provider: Arc::new(hotl_provider::ScriptedProvider::new(vec![])),
            registry: Arc::new(hotl_tools::Registry::builtin()),
            rules: Arc::new(hotl_tools::rules::Rules::default()),
            sandbox_enforced: false,
            clock: Arc::new(hotl_platform::SystemClock),
            log,
            system: "sys".into(),
            cwd: dir.to_path_buf(),
            hooks: None,
            initial_items: Vec::new(),
            initial_todos: Vec::new(),
            initial_decisions: Vec::new(),
            plan_files: None,
            initial_goal: None,
            config: crate::EngineConfig::default(),
        }
    }

    /// A `SharedDeps` + its log + a live `Head`. `commit_prepared`/`compact`
    /// take the head, so the published projection is exercised by every test
    /// below rather than simulated.
    fn test_shared(dir: &std::path::Path) -> (Arc<SharedDeps>, SessionLog, super::Head) {
        let log = SessionLog::create(dir, "m", None, hotl_store::Masker::empty(), 0).expect("log");
        let (head_tx, head_rx) = super::head_channel();
        let head = super::Head::new(
            head_tx,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            hotl_context::TokenProfile::CONSERVATIVE,
        );
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
            head.items()
                .iter()
                .map(|i| (**i).clone())
                .collect::<Vec<_>>(),
            [user("one"), user("two"), user("three")]
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
            usage: hotl_types::TokenUsage::default(),
        };
        let (degraded, _) = compact(&shared, &mut log, &mut head, &mut pipeline, Some(spec))
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
