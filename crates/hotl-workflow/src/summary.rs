//! The approval summary: what the human sees before a run starts, and the
//! agent-count upper bound the `max_agents` cap is checked against.

use serde_json::Value;

use crate::plan::{Plan, Shape};
use crate::select::{Lookup, Selector};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Estimate {
    /// Upper bound on agent starts: `each` counts one placeholder item.
    pub agents: usize,
    /// An `each` phase's item count is unknowable before the run.
    pub open_ended: bool,
}

impl std::fmt::Display for Estimate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "≈{}{}",
            self.agents,
            if self.open_ended { "+" } else { "" }
        )
    }
}

/// Phases shown in the summary before it elides to `… +N`.
const SHOWN_PHASES: usize = 3;

/// How many items an `each` selector yields against `args`, or `None` when it
/// reads something no pre-flight can see.
fn count_items(selector: &str, args: &Value) -> Option<usize> {
    let items = Selector::parse(selector).ok()?.eval(&ArgsOnly(args)).ok()?;
    match items {
        Value::Array(v) => Some(v.len()),
        Value::Null => None,
        _ => Some(1),
    }
}

/// `args` alone: what a pre-flight actually knows. An `each` selector that
/// reads a phase's output resolves to nothing here and stays open-ended.
struct ArgsOnly<'a>(&'a Value);

impl Lookup for ArgsOnly<'_> {
    fn get(&self, root: &str) -> Option<&Value> {
        (root == "args").then_some(self.0)
    }
}

impl Plan {
    pub fn estimate(&self) -> Estimate {
        self.estimate_with(&Value::Null)
    }

    /// The pre-flight estimate against the `args` this run was given (0058
    /// T7). An `each` phase whose selector reads only `args` is *countable*
    /// before the run — resolve it and quote the real number instead of
    /// `≈1+`, which understates a 40-item fan-out by 39 agents at the exact
    /// moment a human is deciding whether to approve it.
    pub fn estimate_with(&self, args: &Value) -> Estimate {
        let mut agents = 0;
        let mut open_ended = false;
        for phase in &self.phases {
            let votes = |specs: &[crate::plan::AgentSpec]| -> usize {
                specs.iter().map(|s| s.votes.unwrap_or(1).max(1)).sum()
            };
            match phase.shape() {
                Ok(Shape::Parallel(specs)) => agents += votes(specs),
                Ok(Shape::Each { selector, stages }) => match count_items(selector, args) {
                    Some(n) => agents += votes(stages) * n,
                    None => {
                        open_ended = true;
                        agents += votes(stages);
                    }
                },
                Ok(Shape::UntilQuiet { cfg, agents: specs }) => {
                    agents += votes(specs) * cfg.max_rounds.max(1)
                }
                Err(_) => {}
            }
        }
        Estimate { agents, open_ended }
    }

    /// One phase as the summary spells it: `Review (4 ∥)`, `Verify (each × 3
    /// votes)`, `Find (≤10 rounds × 1)`.
    pub fn phase_blurb(phase: &crate::plan::Phase) -> String {
        let votes_suffix = |specs: &[crate::plan::AgentSpec]| -> String {
            match specs.iter().filter_map(|s| s.votes).max() {
                Some(v) if v > 1 => format!(" × {v} votes"),
                _ => String::new(),
            }
        };
        match phase.shape() {
            Ok(Shape::Parallel(specs)) => {
                format!("{} ({} ∥{})", phase.title, specs.len(), votes_suffix(specs))
            }
            Ok(Shape::Each { stages, .. }) => {
                let stages_part = if stages.len() > 1 {
                    format!(" × {} stages", stages.len())
                } else {
                    String::new()
                };
                format!(
                    "{} (each{stages_part}{})",
                    phase.title,
                    votes_suffix(stages)
                )
            }
            Ok(Shape::UntilQuiet { cfg, agents }) => format!(
                "{} (≤{} rounds × {}{})",
                phase.title,
                cfg.max_rounds,
                agents.len(),
                votes_suffix(agents)
            ),
            Err(_) => format!("{} (?)", phase.title),
        }
    }

    /// ``workflow `name` — 3 phases, ≈7+ agents: Review (4 ∥) → Verify (each × 3
    /// votes) → Find (≤10 rounds × 1)``, plus ` (serialised: N mutating agents
    /// share the tree)` when `serialised > 0`. Kept to ~110 chars: phases past
    /// the third elide to `… +N`.
    pub fn summary_line(&self, serialised: usize) -> String {
        self.summary_line_with(serialised, &Value::Null)
    }

    /// [`Self::summary_line`] against this run's `args`, so a countable
    /// `each` shows its real width.
    pub fn summary_line_with(&self, serialised: usize, args: &Value) -> String {
        let n = self.phases.len();
        let mut chain: Vec<String> = self
            .phases
            .iter()
            .take(SHOWN_PHASES)
            .map(Plan::phase_blurb)
            .collect();
        if n > SHOWN_PHASES {
            chain.push(format!("… +{}", n - SHOWN_PHASES));
        }
        let mut line = format!(
            "workflow `{}` — {n} phase{}, {} agents: {}",
            self.name,
            if n == 1 { "" } else { "s" },
            self.estimate_with(args),
            chain.join(" → ")
        );
        if serialised > 0 {
            line.push_str(&format!(
                " (serialised: {serialised} mutating agent{} the tree)",
                if serialised == 1 {
                    " shares"
                } else {
                    "s share"
                }
            ));
        }
        line
    }
}

#[cfg(test)]
mod tests {
    use crate::plan::tests::fixture;
    use crate::plan::Plan;
    use serde_json::json;

    #[test]
    fn estimate_is_an_upper_bound_marked_open_ended_by_each() {
        let plan = fixture();
        // Review 2 + Verify 1 stage × 3 votes (one placeholder item) + Find 10 rounds × 1.
        let e = plan.estimate();
        assert_eq!((e.agents, e.open_ended), (15, true));
        assert_eq!(e.to_string(), "≈15+");
        let closed = Plan::from_json(json!({"name": "x", "phases": [{"title": "A", "agents": [{"label": "a", "prompt": "p"}, {"label": "b", "prompt": "p"}]}]})).unwrap();
        assert_eq!(closed.estimate().to_string(), "≈2");
    }

    /// 0058 T7: an `each` over `args` is countable before the run, so the
    /// pre-flight quotes the real width instead of understating it by N−1 at
    /// the moment a human is deciding whether to approve the fan-out.
    #[test]
    fn an_each_over_args_estimates_the_real_item_count() {
        let plan = Plan::from_json(json!({
            "name": "p",
            "phases": [{
                "title": "Fix",
                "each": "args.files",
                "stages": [{"label": "a", "prompt": "{{item}}"}]
            }]
        }))
        .unwrap();
        let args = json!({"files": ["a.rs", "b.rs", "c.rs", "d.rs"]});
        let e = plan.estimate_with(&args);
        assert_eq!((e.agents, e.open_ended), (4, false));
        assert_eq!(e.to_string(), "≈4", "no `+`: this one is known");
        assert!(plan.summary_line_with(0, &args).contains("≈4 agents"));

        // No args, or a selector over a phase's output: still open-ended.
        assert_eq!(plan.estimate().to_string(), "≈1+");
        let downstream = Plan::from_json(json!({
            "name": "p",
            "phases": [
                {"title": "Find", "agents": [{"label": "f", "prompt": "p"}]},
                {"title": "Fix", "each": "Find.hits", "stages": [{"label": "a", "prompt": "{{item}}"}]}
            ]
        }))
        .unwrap();
        assert!(downstream.estimate_with(&args).open_ended);
    }

    #[test]
    fn summary_line_spells_each_shape_and_elides_after_three_phases() {
        let plan = fixture();
        assert_eq!(
            plan.summary_line(0),
            "workflow `review-changes` — 3 phases, ≈15+ agents: Review (2 ∥) → Verify (each × 3 votes) → Find (≤10 rounds × 1)"
        );
        assert!(plan
            .summary_line(2)
            .ends_with(" (serialised: 2 mutating agents share the tree)"));

        let mut five = plan.clone();
        for t in ["D", "E"] {
            let mut p = plan.phases[0].clone();
            p.title = t.into();
            five.phases.push(p);
        }
        let line = five.summary_line(0);
        assert!(
            line.contains("5 phases") && line.ends_with("→ … +2"),
            "{line}"
        );
        assert!(
            line.chars().count() <= 120,
            "{} chars: {line}",
            line.chars().count()
        );
    }
}
