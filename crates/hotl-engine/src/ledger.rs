//! LoopLedger — per-sample phase stamps and a flushable summary (§S1).
//!
//! Owned solely by the turn task: nothing else ever touches a `Turn`'s
//! ledger, so plain `&mut self` methods are enough — no locking, no atomics.
//! Fixed capacity, no heap allocation per stamp: the instrument must not
//! perturb the loop it measures.

use std::time::Instant;

/// The ten boundary events a turn's sample passes through. Declaration order
/// is the critical-path order the design doc's per-phase deltas price
/// (`SnapshotReady−BoundaryStart` prices the snapshot transport, etc.) — see
/// [`DELTA_PAIRS`]. A doom-loop abort or a failed sample can legitimately
/// skip phases past the point of failure; see [`LoopLedger::stamp`].
///
/// `BatchProposed`/`WatermarkDurable` are the one place the critical path
/// forks: a sample proposes once (the model's own assistant+usage entry) or
/// twice (that entry, then — only when a tool phase runs — the tool-results
/// entry). These two phases use [`LoopLedger::restamp`] (last-wins, not
/// `stamp`'s first-wins), so their recorded position always tracks the
/// sample's REAL final commit — after `ToolsJoined` when a tool phase ran,
/// otherwise right after `LastBlockEnd`. Every other phase is written
/// exactly once by construction, so `stamp`'s first-wins rule is what
/// applies to them.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum Phase {
    BoundaryStart,
    SnapshotReady,
    RequestBuilt,
    FirstByte,
    LastBlockEnd,
    ToolsSpawned,
    ToolsJoined,
    BatchProposed,
    WatermarkDurable,
    BoundaryEnd,
}

/// Width of the raw-counter array a sample occupies — one slot per [`Phase`].
pub const PHASE_COUNT: usize = 10;

/// Fixed sample capacity: past this, `start_sample` keeps dropping new
/// samples instead of growing (never reallocates, never panics).
const CAPACITY: usize = 64;

/// Consecutive phase pairs along the critical path, in declaration order —
/// each pairing is one attributable stage of the loop. `(ToolsJoined,
/// BatchProposed)` is zero-width for a sample with no tool phase (both
/// `ToolsSpawned`/`ToolsJoined` are legitimately absent) — that is the
/// zero-width-when-absent rule in [`width`], not a measurement gap. For a
/// sample that DID run a tool phase, `restamp`'s last-wins semantics on
/// `BatchProposed` (see [`Phase`]) guarantee this pairing prices real time:
/// the gap between the tools finishing and the tool-results propose call.
const DELTA_PAIRS: [(Phase, Phase); PHASE_COUNT - 1] = [
    (Phase::BoundaryStart, Phase::SnapshotReady),
    (Phase::SnapshotReady, Phase::RequestBuilt),
    (Phase::RequestBuilt, Phase::FirstByte),
    (Phase::FirstByte, Phase::LastBlockEnd),
    (Phase::LastBlockEnd, Phase::ToolsSpawned),
    (Phase::ToolsSpawned, Phase::ToolsJoined),
    (Phase::ToolsJoined, Phase::BatchProposed),
    (Phase::BatchProposed, Phase::WatermarkDurable),
    (Phase::WatermarkDurable, Phase::BoundaryEnd),
];

/// Raw monotonic nanoseconds since this process's first call — comparable
/// only to other readings from the same process run, never persisted or
/// compared across processes. Isolated behind one fn so a TSC source
/// (quanta) can replace `Instant` later without touching any call site.
fn now_nanos() -> u64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    let start = START.get_or_init(Instant::now);
    start.elapsed().as_nanos() as u64
}

/// The span between two stamps, or zero-width if either endpoint is absent
/// (`0` — a phase legitimately never stamped, e.g. no tool phase this
/// sample). Never underflows: an out-of-order pair of stamps reports `0`
/// rather than panicking or wrapping.
fn width(from: u64, to: u64) -> u64 {
    if from == 0 || to == 0 {
        0
    } else {
        to.saturating_sub(from)
    }
}

/// `overhead = (BoundaryEnd−BoundaryStart) − stream − tools`, per the design
/// doc's metric — `stream = LastBlockEnd−FirstByte`, `tools =
/// ToolsJoined−ToolsSpawned`, both zero-width when absent.
fn sample_overhead(raw: &[u64; PHASE_COUNT]) -> u64 {
    let total = width(
        raw[Phase::BoundaryStart as usize],
        raw[Phase::BoundaryEnd as usize],
    );
    let stream = width(
        raw[Phase::FirstByte as usize],
        raw[Phase::LastBlockEnd as usize],
    );
    let tools = width(
        raw[Phase::ToolsSpawned as usize],
        raw[Phase::ToolsJoined as usize],
    );
    total.saturating_sub(stream).saturating_sub(tools)
}

/// Nearest-rank percentile (`p` in `0..=100`) over an ascending-sorted slice;
/// `0` on an empty slice. Advisory statistics only (design doc §S1) — no
/// interpolation, deterministic on ties.
fn percentile(sorted: &[u64], p: u64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let n = sorted.len() as u64;
    // Textbook nearest-rank: rank = ceil(p/100 * n), clamped to [1, n],
    // 1-indexed.
    let rank = (p * n).div_ceil(100);
    let idx = rank.clamp(1, n) - 1;
    sorted[idx as usize]
}

/// One attributable stage's timing across every sample in a flush.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PhaseDeltaSummary {
    pub from: Phase,
    pub to: Phase,
    pub p50_ns: u64,
    pub p99_ns: u64,
}

/// What gets flushed at `TurnFinished` (as an [`crate::EngineEvent::LedgerReport`],
/// never the canon log) and what a CI regression gate consumes.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LedgerSummary {
    /// Samples actually taken this turn, capped at 64 — past the cap this is
    /// `64`, not the true (dropped) count.
    pub sample_count: usize,
    pub overhead_p50_ns: u64,
    pub overhead_p99_ns: u64,
    pub phase_deltas: Vec<PhaseDeltaSummary>,
    pub max_rss_bytes: u64,
    /// Raw per-sample phase stamps (nanoseconds, process-relative; `0` =
    /// absent), in [`Phase`] declaration order — the material the p50/p99
    /// fields above are computed from, exposed directly so a consumer isn't
    /// limited to this module's own aggregation choices.
    pub samples: Vec<[u64; PHASE_COUNT]>,
}

/// Fixed-capacity, single-writer store of raw per-sample phase stamps —
/// owned by one `Turn` for its whole lifetime.
pub struct LoopLedger {
    samples: [[u64; PHASE_COUNT]; CAPACITY],
    /// Samples started so far, capped at `CAPACITY`.
    len: usize,
    /// Slot `stamp` writes into; `CAPACITY` is the overflow sentinel (every
    /// `stamp` becomes a no-op) once the cap is hit.
    current: usize,
}

impl Default for LoopLedger {
    fn default() -> Self {
        Self::new()
    }
}

impl LoopLedger {
    pub fn new() -> Self {
        Self {
            samples: [[0; PHASE_COUNT]; CAPACITY],
            len: 0,
            current: CAPACITY,
        }
    }

    /// Advance to the next sample slot. Silently drops past `CAPACITY` —
    /// never reallocates, never panics: a pathological turn with hundreds of
    /// samples must not turn the instrument itself into a cost.
    pub fn start_sample(&mut self) {
        if self.len < CAPACITY {
            self.current = self.len;
            self.len += 1;
        } else {
            self.current = CAPACITY;
        }
    }

    /// Record `phase` into the current slot. First stamp wins — a phase
    /// stamped twice keeps its earliest value. A no-op before the first
    /// `start_sample()` call or past the capacity cap.
    pub fn stamp(&mut self, phase: Phase) {
        if self.current >= CAPACITY {
            return;
        }
        let slot = &mut self.samples[self.current][phase as usize];
        if *slot == 0 {
            *slot = now_nanos();
        }
    }

    /// Record `phase` into the current slot, always overwriting — the "last
    /// wins" counterpart to [`stamp`](Self::stamp). `BatchProposed`/
    /// `WatermarkDurable` are the only phases that use this: a sample can
    /// legitimately propose twice (the model's own assistant+usage entry,
    /// then — only when a tool phase runs — the tool-results entry), and the
    /// *last* commit is the one actually on the sample's critical path, not
    /// the first. Every other phase is stamped once by construction, so
    /// `stamp`'s first-wins rule is a no-op difference for them; this method
    /// exists so the two propose-adjacent phases don't have to lie about
    /// which commit they priced. A no-op before the first `start_sample()`
    /// call or past the capacity cap.
    pub fn restamp(&mut self, phase: Phase) {
        if self.current >= CAPACITY {
            return;
        }
        self.samples[self.current][phase as usize] = now_nanos();
    }

    /// Compute the flushable summary. `max_rss_bytes` is threaded in rather
    /// than read here so the pure aggregation stays independent of the
    /// platform syscall (see [`max_rss_bytes`] below).
    pub fn summary(&self, max_rss_bytes: u64) -> LedgerSummary {
        let used = &self.samples[..self.len];
        let mut overheads: Vec<u64> = used.iter().map(sample_overhead).collect();
        overheads.sort_unstable();
        let phase_deltas = DELTA_PAIRS
            .iter()
            .map(|&(from, to)| {
                let mut deltas: Vec<u64> = used
                    .iter()
                    .map(|s| width(s[from as usize], s[to as usize]))
                    .collect();
                deltas.sort_unstable();
                PhaseDeltaSummary {
                    from,
                    to,
                    p50_ns: percentile(&deltas, 50),
                    p99_ns: percentile(&deltas, 99),
                }
            })
            .collect();
        LedgerSummary {
            sample_count: self.len,
            overhead_p50_ns: percentile(&overheads, 50),
            overhead_p99_ns: percentile(&overheads, 99),
            phase_deltas,
            max_rss_bytes,
            samples: used.to_vec(),
        }
    }
}

/// Max resident set size in bytes (`getrusage(RUSAGE_SELF).ru_maxrss`,
/// normalized — macOS reports bytes, Linux/BSD report kilobytes). `0` if the
/// syscall fails; a broken RSS reading is never a reason to fail the flush.
pub fn max_rss_bytes() -> u64 {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } != 0 {
        return 0;
    }
    normalize_rss(usage.ru_maxrss.max(0) as u64)
}

#[cfg(target_os = "macos")]
fn normalize_rss(raw: u64) -> u64 {
    raw
}

#[cfg(not(target_os = "macos"))]
fn normalize_rss(raw: u64) -> u64 {
    raw * 1024
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw_with(pairs: &[(Phase, u64)]) -> [u64; PHASE_COUNT] {
        let mut raw = [0u64; PHASE_COUNT];
        for &(phase, ns) in pairs {
            raw[phase as usize] = ns;
        }
        raw
    }

    #[test]
    fn sample_overhead_subtracts_stream_and_tools() {
        let raw = raw_with(&[
            (Phase::BoundaryStart, 1_000),
            (Phase::SnapshotReady, 1_100),
            (Phase::RequestBuilt, 1_200),
            (Phase::FirstByte, 1_300),
            (Phase::LastBlockEnd, 1_800), // stream = 500
            (Phase::ToolsSpawned, 1_850),
            (Phase::ToolsJoined, 2_050), // tools = 200
            (Phase::BatchProposed, 2_100),
            (Phase::WatermarkDurable, 2_150),
            (Phase::BoundaryEnd, 2_200), // total = 1200
        ]);
        // overhead = total(1200) - stream(500) - tools(200)
        assert_eq!(sample_overhead(&raw), 500);
    }

    #[test]
    fn sample_overhead_treats_absent_phases_as_zero_width() {
        // No tool phase this sample, and (contrived) no stream phase either —
        // both must contribute zero, not an underflow or a bogus value
        // computed against whatever happens to sit at index 0.
        let raw = raw_with(&[(Phase::BoundaryStart, 1_000), (Phase::BoundaryEnd, 1_500)]);
        assert_eq!(sample_overhead(&raw), 500);
    }

    #[test]
    fn width_never_underflows_on_an_out_of_order_pair() {
        assert_eq!(width(500, 100), 0);
    }

    #[test]
    fn percentile_is_nearest_rank_on_sorted_input() {
        let sorted = [10u64, 20, 30, 40, 50];
        assert_eq!(percentile(&sorted, 50), 30);
        assert_eq!(percentile(&sorted, 99), 50);
        assert_eq!(percentile(&sorted, 0), 10);
    }

    #[test]
    fn percentile_of_empty_input_is_zero() {
        assert_eq!(percentile(&[], 50), 0);
    }

    #[test]
    fn start_sample_then_stamp_targets_a_fresh_slot_each_time() {
        let mut ledger = LoopLedger::new();
        ledger.start_sample();
        ledger.stamp(Phase::BoundaryStart);
        ledger.start_sample();
        ledger.stamp(Phase::BoundaryStart);
        let report = ledger.summary(0);
        assert_eq!(report.sample_count, 2);
        assert_ne!(
            report.samples[0][Phase::BoundaryStart as usize],
            0,
            "sample 0 must have its own stamp"
        );
        assert_ne!(
            report.samples[1][Phase::BoundaryStart as usize],
            0,
            "sample 1 must have its own stamp"
        );
    }

    #[test]
    fn stamp_before_any_start_sample_is_a_noop() {
        let mut ledger = LoopLedger::new();
        ledger.stamp(Phase::BoundaryStart); // no start_sample() yet
        let report = ledger.summary(0);
        assert_eq!(report.sample_count, 0, "no sample was ever started");
    }

    #[test]
    fn first_stamp_wins() {
        let mut ledger = LoopLedger::new();
        ledger.start_sample();
        ledger.stamp(Phase::BoundaryStart);
        let first = ledger.summary(0).samples[0][Phase::BoundaryStart as usize];
        // A real (non-zero) wall-clock gap so a bug that overwrites would be
        // visible even on a very fast machine.
        std::thread::sleep(std::time::Duration::from_micros(50));
        ledger.stamp(Phase::BoundaryStart);
        let second = ledger.summary(0).samples[0][Phase::BoundaryStart as usize];
        assert_eq!(
            first, second,
            "the second stamp must not overwrite the first"
        );
    }

    #[test]
    fn restamp_overwrites_with_the_latest_value() {
        // `BatchProposed`/`WatermarkDurable` (turn.rs) can legitimately be
        // proposed twice in one sample — the model's own assistant+usage
        // entry, then (when a tool phase runs) the tool-results entry. The
        // *last* commit is the one on the sample's real critical path, so
        // `restamp` — unlike `stamp` — always overwrites.
        let mut ledger = LoopLedger::new();
        ledger.start_sample();
        ledger.stamp(Phase::BatchProposed);
        let first = ledger.summary(0).samples[0][Phase::BatchProposed as usize];
        std::thread::sleep(std::time::Duration::from_micros(50));
        ledger.restamp(Phase::BatchProposed);
        let second = ledger.summary(0).samples[0][Phase::BatchProposed as usize];
        assert!(
            second > first,
            "restamp must overwrite with a later value, got first={first} second={second}"
        );
    }

    #[test]
    fn restamp_before_any_start_sample_is_a_noop() {
        let mut ledger = LoopLedger::new();
        ledger.restamp(Phase::BatchProposed); // no start_sample() yet
        let report = ledger.summary(0);
        assert_eq!(report.sample_count, 0, "no sample was ever started");
    }

    #[test]
    fn a_phase_may_be_legitimately_absent() {
        let mut ledger = LoopLedger::new();
        ledger.start_sample();
        ledger.stamp(Phase::BoundaryStart);
        ledger.stamp(Phase::BoundaryEnd);
        // No tool phase this sample (e.g. a Done reply with no tool calls).
        let report = ledger.summary(0);
        assert_eq!(report.samples[0][Phase::ToolsSpawned as usize], 0);
        assert_eq!(report.samples[0][Phase::ToolsJoined as usize], 0);
    }

    #[test]
    fn overflow_past_capacity_is_safe_and_drops() {
        let mut ledger = LoopLedger::new();
        for _ in 0..(CAPACITY + 36) {
            ledger.start_sample();
            ledger.stamp(Phase::BoundaryStart);
            ledger.stamp(Phase::BoundaryEnd);
        }
        let report = ledger.summary(0);
        assert_eq!(
            report.sample_count, CAPACITY,
            "overflow must cap, not grow, the stored sample count"
        );
        assert_eq!(report.samples.len(), CAPACITY);
    }

    #[test]
    fn summary_reports_sample_count_and_passes_through_max_rss() {
        let mut ledger = LoopLedger::new();
        ledger.start_sample();
        ledger.stamp(Phase::BoundaryStart);
        ledger.stamp(Phase::BoundaryEnd);
        let report = ledger.summary(123_456);
        assert_eq!(report.sample_count, 1);
        assert_eq!(report.max_rss_bytes, 123_456);
    }

    #[test]
    fn summary_overhead_percentiles_span_every_sample() {
        // Two samples with distinct overhead (500ns and 100ns via
        // hand-built raw counters), asserting the p50/p99 read off the
        // smaller/larger value respectively — nearest-rank over n=2 puts p50
        // at index 0 (sorted ascending) and p99 at index 1.
        // `BoundaryStart` must be non-zero — `0` is the "absent" sentinel
        // `width` treats as zero-width, which would defeat this test.
        let mut samples = [[0u64; PHASE_COUNT]; CAPACITY];
        samples[0] = raw_with(&[(Phase::BoundaryStart, 1_000), (Phase::BoundaryEnd, 1_100)]);
        samples[1] = raw_with(&[(Phase::BoundaryStart, 1_000), (Phase::BoundaryEnd, 1_500)]);
        let ledger = LoopLedger {
            samples,
            len: 2,
            current: 1,
        };
        let report = ledger.summary(0);
        assert_eq!(report.overhead_p50_ns, 100);
        assert_eq!(report.overhead_p99_ns, 500);
    }

    #[test]
    fn phase_deltas_cover_every_consecutive_pair_in_declaration_order() {
        let ledger = LoopLedger::new();
        let report = ledger.summary(0);
        assert_eq!(report.phase_deltas.len(), PHASE_COUNT - 1);
        assert_eq!(report.phase_deltas[0].from, Phase::BoundaryStart);
        assert_eq!(report.phase_deltas[0].to, Phase::SnapshotReady);
        assert_eq!(report.phase_deltas[PHASE_COUNT - 2].to, Phase::BoundaryEnd);
    }

    #[test]
    fn now_nanos_is_monotonically_nondecreasing() {
        let a = now_nanos();
        let b = now_nanos();
        assert!(b >= a);
    }

    #[test]
    fn max_rss_bytes_returns_a_plausible_reading() {
        // A real syscall on a real running process: some resident memory has
        // always been touched by the time a test binary is running.
        assert!(max_rss_bytes() > 0);
    }
}
