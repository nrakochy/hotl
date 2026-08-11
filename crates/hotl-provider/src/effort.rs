//! The provider-neutral reasoning-depth ladder and its per-dialect projection.
//!
//! hotl carries one ladder; each dialect spells it differently on the wire
//! (`output_config.effort` on Anthropic, `reasoning_effort` on OpenAI). A
//! dialect that accepts fewer rungs clamps rather than errors — see
//! [`EffortLadder::resolve`].

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Provider-neutral reasoning-depth ladder, low → high.
///
/// Ordering is load-bearing: [`EffortLadder::resolve`] clamps by distance along
/// this ladder, so the discriminants must stay monotonic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

/// Every rung, ascending. The order is what makes ties break downward.
pub const ALL_EFFORTS: &[Effort] = &[
    Effort::Low,
    Effort::Medium,
    Effort::High,
    Effort::XHigh,
    Effort::Max,
];

/// The five levels, spelled for a usage or warning message.
pub const EFFORT_LEVELS: &str = "low | medium | high | xhigh | max";

impl Effort {
    /// The canonical wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::XHigh => "xhigh",
            Effort::Max => "max",
        }
    }

    /// Position on the ladder, 0-based, ascending.
    fn rank(self) -> u8 {
        match self {
            Effort::Low => 0,
            Effort::Medium => 1,
            Effort::High => 2,
            Effort::XHigh => 3,
            Effort::Max => 4,
        }
    }
}

impl fmt::Display for Effort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A string that names no rung. Callers warn and ignore rather than abort.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseEffortError;

impl fmt::Display for ParseEffortError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "not a known effort level ({EFFORT_LEVELS})")
    }
}

impl std::error::Error for ParseEffortError {}

impl FromStr for Effort {
    type Err = ParseEffortError;

    /// Canonical lowercase spellings, plus `x-high`/`x_high` for `xhigh` —
    /// the same alias tolerance `PermissionMode::from_str` gives `dont-ask`.
    /// Case-insensitive; callers trim first.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "low" => Ok(Effort::Low),
            "medium" => Ok(Effort::Medium),
            "high" => Ok(Effort::High),
            "xhigh" | "x-high" | "x_high" => Ok(Effort::XHigh),
            "max" => Ok(Effort::Max),
            _ => Err(ParseEffortError),
        }
    }
}

/// Absolute distance along the ladder.
fn distance(a: Effort, b: Effort) -> u8 {
    a.rank().abs_diff(b.rank())
}

/// How one dialect renders the neutral ladder.
///
/// A dialect declares only the rungs it accepts; the default `resolve` clamps
/// to the nearest one it has, so a three-rung provider answers `Max` with its
/// own top rung instead of erroring, and a zero-rung model omits the field.
pub trait EffortLadder {
    /// Rungs this dialect accepts for `model`, ascending. Empty = unsupported.
    fn rungs(&self, model: &str) -> &'static [Effort];

    /// The nearest accepted rung to `want`, or `None` when there are none.
    ///
    /// Ties break **downward**: `min_by_key` over an ascending slice keeps the
    /// first minimum, and over-spending someone's money on a guess is the
    /// worse failure.
    fn resolve(&self, model: &str, want: Effort) -> Option<Effort> {
        self.rungs(model)
            .iter()
            .copied()
            .min_by_key(|r| distance(*r, want))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Stub(&'static [Effort]);

    impl EffortLadder for Stub {
        fn rungs(&self, _model: &str) -> &'static [Effort] {
            self.0
        }
    }

    #[test]
    fn rungs_are_ascending_so_ties_break_downward() {
        // `High` is the genuine tie: 2 from Low, 2 from Max. Ascending order
        // makes `min_by_key` keep Low.
        let l = Stub(&[Effort::Low, Effort::Max]);
        assert_eq!(l.resolve("m", Effort::High), Some(Effort::Low));
        assert_eq!(l.resolve("m", Effort::Medium), Some(Effort::Low));
    }

    #[test]
    fn a_full_ladder_is_the_identity() {
        let l = Stub(ALL_EFFORTS);
        for &e in ALL_EFFORTS {
            assert_eq!(l.resolve("m", e), Some(e));
        }
    }

    #[test]
    fn an_empty_ladder_resolves_to_none() {
        // The `claude-haiku-4-5` shape: `caps.effort == false`.
        let l = Stub(&[]);
        assert_eq!(l.resolve("m", Effort::Max), None);
    }

    #[test]
    fn a_short_ladder_clamps_up_and_down() {
        let l = Stub(&[Effort::Medium, Effort::High]);
        assert_eq!(l.resolve("m", Effort::Low), Some(Effort::Medium));
        assert_eq!(l.resolve("m", Effort::Max), Some(Effort::High));
    }

    #[test]
    fn parse_accepts_the_aliases_and_rejects_junk() {
        for s in ["xhigh", "x-high", "x_high", "XHigh"] {
            assert_eq!(s.parse::<Effort>(), Ok(Effort::XHigh), "{s}");
        }
        for s in ["ultra", "", "HIGH ", "none", "minimal"] {
            assert!(s.parse::<Effort>().is_err(), "{s}");
        }
    }

    #[test]
    fn as_str_round_trips_through_from_str() {
        for &e in ALL_EFFORTS {
            assert_eq!(e.as_str().parse::<Effort>(), Ok(e));
        }
    }
}
