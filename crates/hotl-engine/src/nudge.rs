//! Stagnation detectors that name a pattern once and let the turn continue.
//!
//! The hard stop lives elsewhere: `turn::detect_doom_loop` ends a turn when a
//! block of calls repeats three times with identical results. These are the
//! softer signals it deliberately does not catch — a failure repeated twice, a
//! two-call cycle whose results keep changing, one file edited over and over —
//! and each fires **once per turn per signature**, so a model that ignores the
//! nudge is not nagged into a second problem.
//!
//! Pure over the call ledger: no engine types, no I/O, unit-testable alone.

use std::collections::{HashMap, HashSet, VecDeque};

/// Identical failures in a row before the streak is named. Two: the first
/// retry is legitimate, the second is a pattern.
const ERROR_STREAK: usize = 2;
/// Edits to one file in a turn before the churn is named.
const FILE_CHURN: u32 = 4;
/// Calls the cycle detector looks back over — one A-B-A-B period.
const CYCLE_WINDOW: usize = 4;

/// Which detector fired. Part of the memo key, so a cycle and a streak over
/// the same calls are two different things to say.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Kind {
    ErrorStreak,
    Cycle,
    Churn,
}

/// One executed call, as the detectors read it.
#[derive(Debug, Clone)]
pub(crate) struct Call {
    pub tool: String,
    /// Identity of the call: tool plus arguments.
    pub args_hash: u64,
    /// Identity of what came back — what separates a cycle that is making
    /// progress from one that is not.
    pub result_hash: u64,
    pub is_error: bool,
    /// The file an edit touched: the churn key. `None` for every other tool.
    pub target: Option<String>,
    /// The error's first line, quoted back so the nudge names the failure
    /// rather than gesturing at it.
    pub error_line: String,
}

impl Call {
    /// Call identity for the streak and cycle memos.
    fn signature(&self) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(&(&self.tool, self.args_hash), &mut h);
        std::hash::Hasher::finish(&h)
    }
}

fn hash_of<T: std::hash::Hash>(v: T) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(&v, &mut h);
    std::hash::Hasher::finish(&h)
}

/// Per-turn detector state: what has already been said, the trailing call
/// window, and the turn's per-file edit counts.
#[derive(Debug, Default)]
pub(crate) struct Detectors {
    seen: HashSet<(Kind, u64)>,
    window: VecDeque<Call>,
    churn: HashMap<String, u32>,
}

impl Detectors {
    /// Fold one batch's finished calls in and return whatever is worth saying.
    /// Source order; at most one reminder per (kind, signature) per turn.
    pub(crate) fn observe(&mut self, calls: &[Call]) -> Vec<String> {
        let mut out = Vec::new();
        for call in calls {
            if let Some(file) = &call.target {
                let n = {
                    let slot = self.churn.entry(file.clone()).or_insert(0);
                    *slot += 1;
                    *slot
                };
                if n >= FILE_CHURN && self.fires(Kind::Churn, hash_of(file)) {
                    out.push(format!(
                        "You have edited {file} {n} times this turn. Reconsider the \
                         approach before the next edit."
                    ));
                }
            }
            self.window.push_back(call.clone());
            while self.window.len() > CYCLE_WINDOW {
                self.window.pop_front();
            }
            out.extend(self.error_streak());
            out.extend(self.cycle());
        }
        out
    }

    /// Has this (kind, signature) not been said yet this turn?
    fn fires(&mut self, kind: Kind, signature: u64) -> bool {
        self.seen.insert((kind, signature))
    }

    /// The same call failing the same way twice running.
    fn error_streak(&mut self) -> Option<String> {
        let n = self.window.len();
        if n < ERROR_STREAK {
            return None;
        }
        let tail: Vec<&Call> = self.window.iter().skip(n - ERROR_STREAK).collect();
        let signature = tail[0].signature();
        if !tail
            .iter()
            .all(|c| c.is_error && c.signature() == signature)
        {
            return None;
        }
        let (tool, line) = (
            tail[0].tool.clone(),
            tail[ERROR_STREAK - 1].error_line.clone(),
        );
        self.fires(Kind::ErrorStreak, signature).then(|| {
            format!(
                "The last two {tool} calls failed the same way ({line}). Change the \
                 approach instead of retrying."
            )
        })
    }

    /// A-B-A-B whose results keep changing. Identical results are the doom
    /// loop's business — that is a hard stop, and saying it twice in two
    /// voices would be worse than saying it once.
    fn cycle(&mut self) -> Option<String> {
        if self.window.len() < CYCLE_WINDOW {
            return None;
        }
        let w: Vec<&Call> = self.window.iter().collect();
        let (a1, b1, a2, b2) = (w[0], w[1], w[2], w[3]);
        let (sa, sb) = (a1.signature(), b1.signature());
        if sa == sb || a2.signature() != sa || b2.signature() != sb {
            return None;
        }
        if a1.result_hash == a2.result_hash && b1.result_hash == b2.result_hash {
            return None;
        }
        let (a, b) = (a1.tool.clone(), b1.tool.clone());
        // Order-free key: A-B-A-B and B-A-B-A are one cycle to report.
        let key = hash_of((sa.min(sb), sa.max(sb)));
        self.fires(Kind::Cycle, key).then(|| {
            format!(
                "Calls are alternating between {a} and {b} without converging; stop \
                 and reassess."
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(tool: &str, args: u64, result: u64, is_error: bool) -> Call {
        Call {
            tool: tool.into(),
            args_hash: args,
            result_hash: result,
            is_error,
            target: None,
            error_line: format!("{tool} exploded"),
        }
    }

    fn edit(file: &str, n: u64) -> Call {
        Call {
            target: Some(file.into()),
            ..call("edit", n, n, false)
        }
    }

    #[test]
    fn error_streak_fires_once_per_signature() {
        let mut d = Detectors::default();
        assert!(d.observe(&[call("bash", 1, 9, true)]).is_empty());
        let out = d.observe(&[call("bash", 1, 9, true)]);
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(
            out[0].starts_with("The last two bash calls failed"),
            "{out:?}"
        );
        assert!(out[0].contains("bash exploded"), "{out:?}");
        // A third identical failure is the same signature: nothing more to say.
        assert!(d.observe(&[call("bash", 1, 9, true)]).is_empty());
        // A different call failing twice is its own streak.
        d.observe(&[call("bash", 2, 8, true)]);
        let out = d.observe(&[call("bash", 2, 8, true)]);
        assert_eq!(out.len(), 1, "{out:?}");
    }

    #[test]
    fn a_streak_needs_the_same_call_and_a_real_error() {
        let mut d = Detectors::default();
        // Two failures of *different* calls are not a streak.
        d.observe(&[call("bash", 1, 9, true), call("bash", 2, 9, true)]);
        assert!(d.seen.is_empty());
        // Two identical calls that succeeded are not a streak.
        let mut d = Detectors::default();
        d.observe(&[call("read", 1, 9, false), call("read", 1, 9, false)]);
        assert!(d.seen.is_empty());
    }

    #[test]
    fn abab_cycle_is_named() {
        let mut d = Detectors::default();
        let out = d.observe(&[
            call("bash", 1, 10, false),
            call("read", 2, 20, false),
            call("bash", 1, 11, false),
            call("read", 2, 21, false),
        ]);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(
            out[0],
            "Calls are alternating between bash and read without converging; stop and reassess."
        );
        // The same cycle again says nothing more.
        let out = d.observe(&[call("bash", 1, 12, false), call("read", 2, 22, false)]);
        assert!(out.is_empty(), "{out:?}");
    }

    #[test]
    fn an_unchanging_cycle_is_left_to_the_doom_detector() {
        let mut d = Detectors::default();
        let out = d.observe(&[
            call("bash", 1, 10, false),
            call("read", 2, 20, false),
            call("bash", 1, 10, false),
            call("read", 2, 20, false),
        ]);
        assert!(out.is_empty(), "identical results are a hard stop: {out:?}");
    }

    #[test]
    fn file_churn_fires_at_four_edits() {
        let mut d = Detectors::default();
        for n in 0..3 {
            assert!(d.observe(&[edit("a.rs", n)]).is_empty(), "edit {n}");
        }
        let out = d.observe(&[edit("a.rs", 3)]);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(
            out[0],
            "You have edited a.rs 4 times this turn. Reconsider the approach before the next edit."
        );
        // A fifth edit says nothing more; a different file counts separately.
        assert!(d.observe(&[edit("a.rs", 4)]).is_empty());
        for n in 0..3 {
            assert!(d.observe(&[edit("b.rs", n)]).is_empty());
        }
        assert_eq!(d.observe(&[edit("b.rs", 3)]).len(), 1);
    }
}
