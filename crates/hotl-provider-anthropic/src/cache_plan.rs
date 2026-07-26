//! Where the deep cache breakpoints go, as a pure function of the durable
//! items.
//!
//! Anthropic's cache lookback walks at most ~20 content blocks back from a
//! `cache_control` marker. Marking only the latest user-role item is therefore
//! fine right up until one turn is wide — a 40-result tool batch, say — at
//! which point the new marker cannot see the entry the previous sample wrote,
//! the lookup misses, and the whole history re-bills at write price on every
//! sample from then on. The fix is rolling *anchors*: extra markers dropped at
//! fixed stride crossings, close enough together that the next sample's
//! markers can still reach the entries this one wrote.
//!
//! "Close enough together" is a claim about *markable* positions, and two
//! things can still open a gap wider than the lookback:
//!
//! - An assistant item wider than ~19 blocks. Assistant blocks are never
//!   markable (see [`candidates`]), so a single wide turn pushes its anchor to
//!   the first candidate past it and nothing can be placed inside — the
//!   residual pinned by `an_oversized_assistant_turn_degrades_deterministically`.
//! - Budget exhaustion: only the last [`MAX_ANCHORS`] crossings are kept, so
//!   the shallow ones are dropped as history grows. This one is harmless when
//!   crossings accumulate gradually, one or two per turn — cache entries are
//!   prefix-cumulative, so a dropped shallow anchor is already sealed behind
//!   the deeper ones that replaced it. It is NOT harmless when a single turn
//!   appends ≥3 stride crossings at once (a ~45+ block user-role turn): the
//!   shallowest of those brand-new crossings is dropped before any request
//!   ever marked it, so this request's markers can land more than the
//!   lookback past the *previous* request's deepest entry — that one request
//!   re-bills its whole history at write price, self-healing on the next
//!   sample once growth returns to normal.
//!
//! Both are degradations, not correctness bugs: the placement stays
//! deterministic and append-stable either way.
//!
//! Two properties make this safe, and both come from the same decision — the
//! planner is a pure function of `items` with no state and no config:
//!
//! - **Stability.** Crossings are prefix-sum thresholds over an append-only
//!   list, so a crossing, once computed, lands on the same block forever. The
//!   marker a request writes is the marker the *next* request reads.
//! - **Byte-identity.** hotl's speculation system rebuilds a request on two
//!   paths (optimistic dispatch vs. sequential rebuild) and proves the bodies
//!   equal byte for byte. A planner that remembered anything would make those
//!   two paths diverge; one that only reads the item list cannot.

use hotl_types::Item;

/// Stride, in wire content blocks, between rolling anchors.
///
/// Both numbers come from Anthropic's prompt-caching documentation: the
/// lookback that walks ~20 content blocks back from a breakpoint, and its own
/// remedy for long conversations — place an intermediate breakpoint roughly
/// every 15 blocks. 15 under 20 leaves 5 blocks of slack, which covers the
/// item straddling a crossing **when that item is at most 5 blocks wide**; a
/// wider straddling item eats into the margin, and one wider than the lookback
/// itself opens a real gap (see the module doc's residuals).
///
/// The premise the whole scheme rests on: a `cache_control` marker is
/// *metadata*, NOT part of the prefix the API hashes. Moving a marker between
/// requests therefore invalidates nothing — which is exactly why anchors may
/// roll, and why a marker this request drops costs nothing beyond the entry it
/// stops refreshing.
pub(crate) const ANCHOR_STRIDE: usize = 15;

/// The API's per-request `cache_control` budget. Spent as: 1 prefix marker
/// (system, or the last tool def when there is no system prompt) + up to
/// [`MAX_ANCHORS`] rolling anchors + 1 latest.
pub(crate) const MAX_BREAKPOINTS: usize = 4;

/// What is left of the budget for rolling anchors once the prefix marker and
/// the latest marker have taken theirs.
const MAX_ANCHORS: usize = MAX_BREAKPOINTS - 2;

/// Wire content-block cost of one item.
///
/// MUST mirror `build_messages` exactly — the planner's arithmetic is over
/// *rendered* blocks, so an item this function mis-sizes moves every crossing
/// after it. Anything `build_messages` skips costs 0.
fn item_blocks(item: &Item) -> usize {
    match item {
        // Never reaches the wire from the messages list: the system prompt
        // travels in the request's `system` field.
        Item::System { .. } | Item::Unknown => 0,
        Item::User { .. } => 1,
        Item::Assistant { blocks } => blocks.len(),
        Item::ToolResults { results } => results.len(),
    }
}

/// A markable wire position: `(item index, block index within that item's
/// rendered message)`.
///
/// Ordered by wire position — `Ord` is what "strictly before latest" means
/// below, and it is exactly lexicographic on the two fields because
/// `build_messages` emits items in order and blocks within an item in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Mark {
    pub item: usize,
    pub block: usize,
}

pub(crate) struct Plan {
    /// Rolling anchors at deterministic stride crossings, oldest→newest,
    /// strictly before [`Self::latest`]. At most [`MAX_ANCHORS`].
    pub anchors: Vec<Mark>,
    /// The last durable user-role block: a `User` item's text block, or the
    /// last `tool_result` block of a `ToolResults` item.
    pub latest: Option<Mark>,
}

impl Plan {
    /// Does a `cache_control` marker belong on this rendered block at all —
    /// i.e. is it either the LATEST marker or a rolling ANCHOR? Anchors are
    /// deduped against `latest` when the plan is built, so the two can never
    /// both answer yes for the same block — the API rejects two markers on
    /// one content block.
    #[cfg(test)]
    pub(crate) fn marks(&self, item: usize, block: usize) -> bool {
        self.is_latest(item, block) || self.is_anchor(item, block)
    }

    /// Is this rendered block the single LATEST marker — the one Task 4
    /// (mode-derived 1h TTL) requires to always render plain, regardless of
    /// `CachePolicy::Static`'s `prefix_ttl`? Its segment is rewritten every
    /// sample, so a longer-lived write premium there recurs per turn and buys
    /// nothing.
    pub(crate) fn is_latest(&self, item: usize, block: usize) -> bool {
        self.latest == Some(Mark { item, block })
    }

    /// Is this rendered block a rolling anchor — ttl-eligible, unlike
    /// `latest`? Mutually exclusive with [`Self::is_latest`] by construction
    /// (anchors are deduped against `latest` when the plan is built).
    pub(crate) fn is_anchor(&self, item: usize, block: usize) -> bool {
        self.anchors.contains(&Mark { item, block })
    }
}

/// Every candidate marker position, in wire order, paired with its cumulative
/// 1-based wire block index across the whole rendered message list.
///
/// Candidates are **user-role blocks only**: each `User` item's single text
/// block, and each individual `tool_result` block inside a `ToolResults` item.
/// The interior positions are the point — they are what lets an anchor land
/// *inside* a wide tool batch instead of being stranded before it.
///
/// Assistant blocks are never candidates. They are echoed to the wire verbatim
/// (thinking signatures and all) and this crate does not mutate them.
///
/// Every durable item is a candidate regardless of its `synthetic` tag:
/// ephemerality is positional now (`SamplingRequest::ephemeral_tail` is a
/// separate list this planner never sees), so there is no tag to special-case.
fn candidates(items: &[Item]) -> Vec<(usize, Mark)> {
    let mut out = Vec::new();
    let mut before = 0usize;
    for (item, entry) in items.iter().enumerate() {
        match entry {
            Item::User { .. } => out.push((before + 1, Mark { item, block: 0 })),
            Item::ToolResults { results } => out
                .extend((0..results.len()).map(|block| (before + block + 1, Mark { item, block }))),
            Item::Assistant { .. } | Item::System { .. } | Item::Unknown => {}
        }
        before += item_blocks(entry);
    }
    out
}

/// Crossing k (k = 1, 2, 3, …) is the first candidate whose cumulative wire
/// block index is ≥ k·[`ANCHOR_STRIDE`]; the returned vec holds them in k
/// order.
///
/// One entry per k, duplicates included: when a run of non-candidate blocks
/// (a wide assistant turn) skips past several thresholds at once, the same
/// candidate answers for several k. Keeping the duplicates is what makes the
/// result *append-stable as a prefix* — appending items only ever adds
/// candidates at strictly higher cumulative indices, so every already-resolved
/// k keeps its answer and the vec only grows at the tail. Callers dedup.
fn crossings(cands: &[(usize, Mark)]) -> Vec<Mark> {
    let mut out = Vec::new();
    let mut k = 1usize;
    for (pos, mark) in cands {
        while *pos >= k * ANCHOR_STRIDE {
            out.push(*mark);
            k += 1;
        }
    }
    out
}

/// Plan the deep breakpoints for one durable item list. Pure: same items in,
/// same plan out, always.
pub(crate) fn plan(items: &[Item]) -> Plan {
    let cands = candidates(items);
    // The last candidate *is* the last durable user-role block, by
    // construction — the same position the pre-anchor serializer marked. (A
    // degenerate `ToolResults` with zero results contributes no candidate, so
    // it cannot be picked as a position that has no block to carry a marker.)
    let latest = cands.last().map(|(_, m)| *m);
    let mut anchors: Vec<Mark> = Vec::new();
    for m in crossings(&cands) {
        // Crossings are non-decreasing, so the first one that reaches `latest`
        // ends the useful range: a crossing that lands exactly on `latest` is
        // dropped (one block, one marker) and anything past it cannot exist.
        if latest.is_some_and(|l| m >= l) {
            break;
        }
        if anchors.last() != Some(&m) {
            anchors.push(m);
        }
    }
    // Keep the *last* MAX_ANCHORS: the deepest markers are the ones a growing
    // history still needs. The shallow ones are already sealed behind them.
    if anchors.len() > MAX_ANCHORS {
        anchors.drain(..anchors.len() - MAX_ANCHORS);
    }
    Plan { anchors, latest }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hotl_types::ToolResultItem;
    use serde_json::json;

    fn user(text: &str) -> Item {
        Item::User {
            text: text.into(),
            synthetic: None,
        }
    }

    fn assistant(blocks: usize) -> Item {
        Item::Assistant {
            blocks: (0..blocks)
                .map(|i| json!({"type": "text", "text": format!("b{i}")}))
                .collect(),
        }
    }

    fn results(n: usize) -> Item {
        Item::ToolResults {
            results: (0..n)
                .map(|i| ToolResultItem {
                    tool_use_id: format!("t{i}"),
                    content: "out".into(),
                    is_error: false,
                })
                .collect(),
        }
    }

    fn mark(item: usize, block: usize) -> Mark {
        Mark { item, block }
    }

    /// The arithmetic, spelled out. Wire blocks: user@1, assistant@2,
    /// tool_result j@3+j (j = 0..29, so 3..=32).
    #[test]
    fn crossings_land_on_the_first_candidate_at_or_past_each_stride() {
        let items = vec![user("go"), assistant(1), results(30)];
        // 15 → the tool_result at cumulative 15 (j = 12);
        // 30 → the tool_result at cumulative 30 (j = 27); 45 is past the end.
        assert_eq!(
            crossings(&candidates(&items)),
            vec![mark(2, 12), mark(2, 27)]
        );
        let p = plan(&items);
        assert_eq!(p.anchors, vec![mark(2, 12), mark(2, 27)]);
        assert_eq!(p.latest, Some(mark(2, 29)));
    }

    /// A tool batch wider than the stride gets an anchor *inside* it — the
    /// whole reason candidates are per-`tool_result` and not per-item.
    #[test]
    fn a_wide_tool_batch_gets_an_interior_anchor() {
        let items = vec![user("go"), assistant(1), results(30)];
        let p = plan(&items);
        assert!(
            p.anchors.iter().all(|m| m.item == 2 && m.block > 0),
            "anchors must sit inside the batch, not before it: {:?}",
            p.anchors
        );
        // …and every marker is within the API's lookback of the previous one.
        let mut positions: Vec<usize> = p.anchors.iter().map(|m| 3 + m.block).collect();
        positions.push(3 + p.latest.unwrap().block);
        for pair in positions.windows(2) {
            assert!(pair[1] - pair[0] <= 20, "lookback gap: {positions:?}");
        }
    }

    /// The budget: many crossings exist, the last two win.
    #[test]
    fn at_most_two_anchors_and_they_are_the_deepest_two() {
        let items = vec![results(100)];
        let all = crossings(&candidates(&items));
        // 15, 30, 45, 60, 75, 90 → blocks 14, 29, 44, 59, 74, 89.
        assert_eq!(
            all,
            vec![
                mark(0, 14),
                mark(0, 29),
                mark(0, 44),
                mark(0, 59),
                mark(0, 74),
                mark(0, 89)
            ]
        );
        let p = plan(&items);
        assert_eq!(p.anchors, vec![mark(0, 74), mark(0, 89)]);
        assert_eq!(p.latest, Some(mark(0, 99)));
    }

    /// A crossing that lands exactly on `latest` is dropped: one block never
    /// carries two markers.
    #[test]
    fn a_crossing_on_latest_is_deduped_to_a_single_marker() {
        // 15 tool results: cumulative 1..=15, so crossing 1 is the last block,
        // which is also `latest`.
        let items = vec![results(ANCHOR_STRIDE)];
        assert_eq!(crossings(&candidates(&items)), vec![mark(0, 14)]);
        let p = plan(&items);
        assert_eq!(p.latest, Some(mark(0, 14)));
        assert!(p.anchors.is_empty(), "{:?}", p.anchors);
        assert!(p.marks(0, 14));
        assert!(!p.marks(0, 13));
    }

    /// Two `User` items in a row are two separate candidates one block apart.
    #[test]
    fn plain_user_items_are_candidates_at_their_single_block() {
        let items = vec![user("a"), user("b"), user("c")];
        let p = plan(&items);
        assert!(p.anchors.is_empty(), "no stride is crossed in 3 blocks");
        assert_eq!(p.latest, Some(mark(2, 0)));
    }

    /// Nothing to mark is a legal plan, not a panic.
    #[test]
    fn an_item_list_with_no_user_role_blocks_plans_nothing() {
        let p = plan(&[]);
        assert!(p.anchors.is_empty() && p.latest.is_none());
        let p = plan(&[
            assistant(3),
            Item::System { text: "s".into() },
            Item::Unknown,
        ]);
        assert!(p.anchors.is_empty() && p.latest.is_none());
        // A degenerate empty tool batch offers no block to carry a marker.
        let p = plan(&[results(0)]);
        assert!(p.anchors.is_empty() && p.latest.is_none());
    }

    /// System and Unknown items cost zero wire blocks, so they must not shift
    /// a single crossing.
    #[test]
    fn skipped_items_do_not_move_crossings() {
        let bare = vec![user("go"), results(30)];
        let padded = vec![
            Item::System { text: "s".into() },
            user("go"),
            Item::Unknown,
            results(30),
        ];
        let a = plan(&bare);
        let b = plan(&padded);
        // Same block positions, shifted only by the two inserted item indices.
        assert_eq!(
            a.anchors.iter().map(|m| m.block).collect::<Vec<_>>(),
            b.anchors.iter().map(|m| m.block).collect::<Vec<_>>()
        );
        assert_eq!(a.latest.unwrap().block, b.latest.unwrap().block);
        assert_eq!(b.latest.unwrap().item, 3);
    }

    /// A transcript that grows the way a session does: alternating assistant
    /// turns and tool batches of varying width, so crossings resolve at many
    /// different offsets rather than on a tidy multiple.
    fn growing_transcript() -> Vec<Item> {
        let mut items = vec![user("go")];
        for round in 0..14usize {
            items.push(assistant(1 + round % 3));
            items.push(results(1 + (round * 5) % 17));
        }
        items
    }

    /// THE invariant. Replay `plan` over every prefix of a growing transcript:
    /// the crossing list only ever grows at the tail, so a marker written by
    /// one sample is at the same byte offset for every later sample — which is
    /// the difference between a cache hit and a full re-bill.
    #[test]
    fn anchor_positions_never_move_under_append() {
        let full = growing_transcript();
        let mut previous: Vec<Mark> = Vec::new();
        for n in 0..=full.len() {
            let now = crossings(&candidates(&full[..n]));
            assert!(
                now.starts_with(&previous),
                "a crossing moved when items were appended: {previous:?} -> {now:?} (n = {n})"
            );
            previous = now;
        }
        assert!(
            previous.len() >= 5,
            "fixture must actually exercise several crossings, saw {}",
            previous.len()
        );
        // And every anchor the planner ever chose is one of the final
        // crossings, at the same position.
        let final_crossings = previous;
        for n in 0..=full.len() {
            for anchor in plan(&full[..n]).anchors {
                assert!(
                    final_crossings.contains(&anchor),
                    "anchor {anchor:?} at n = {n} is not a crossing of the full transcript"
                );
            }
        }
    }

    /// The accepted residual gap, pinned rather than fixed: nothing in the
    /// engine bounds how many blocks one assistant turn may carry, and
    /// assistant blocks can never be marked. A turn wider than the stride
    /// therefore pushes its anchor to the first candidate *after* it — the
    /// lookback gap over that turn is real, but the behaviour is deterministic
    /// and still append-stable, which is what keeps it a degradation instead
    /// of a correctness bug.
    #[test]
    fn an_oversized_assistant_turn_degrades_deterministically() {
        let items = vec![user("go"), assistant(25), user("next"), user("again")];
        // Blocks: user@1, assistant@2..=26, user@27, user@28.
        assert_eq!(crossings(&candidates(&items)), vec![mark(2, 0)]);
        let p = plan(&items);
        assert_eq!(p.anchors, vec![mark(2, 0)], "first candidate past the turn");
        assert_eq!(p.latest, Some(mark(3, 0)));

        // Still stable under append, which is the part that must not break.
        let mut previous: Vec<Mark> = Vec::new();
        for n in 0..=items.len() {
            let now = crossings(&candidates(&items[..n]));
            assert!(now.starts_with(&previous), "{previous:?} -> {now:?}");
            previous = now;
        }
    }

    /// One wide assistant turn can skip several thresholds at once. Both k's
    /// resolve to the same block; the plan emits it once.
    #[test]
    fn several_crossings_resolving_to_one_block_emit_one_anchor() {
        let items = vec![user("go"), assistant(40), user("next"), user("again")];
        // Blocks: user@1, assistant@2..=41, user@42, user@43.
        // 15 and 30 both resolve to the user at 42; 45 is past the end.
        assert_eq!(crossings(&candidates(&items)), vec![mark(2, 0), mark(2, 0)]);
        let p = plan(&items);
        assert_eq!(p.anchors, vec![mark(2, 0)]);
        assert_eq!(p.latest, Some(mark(3, 0)));
    }
}
