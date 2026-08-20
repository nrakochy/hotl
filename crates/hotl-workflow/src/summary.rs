//! The approval summary: what the human sees before a run starts, and the
//! agent-count upper bound the `max_agents` cap is checked against.

use crate::plan::{Plan, Shape};

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

impl Plan {
    pub fn estimate(&self) -> Estimate {
        let mut agents = 0;
        let mut open_ended = false;
        for phase in &self.phases {
            let votes = |specs: &[crate::plan::AgentSpec]| -> usize {
                specs.iter().map(|s| s.votes.unwrap_or(1).max(1)).sum()
            };
            match phase.shape() {
                Ok(Shape::Parallel(specs)) => agents += votes(specs),
                Ok(Shape::Each { stages, .. }) => {
                    open_ended = true;
                    agents += votes(stages);
                }
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
            self.estimate(),
            chain.join(" → ")
        );
        if serialised > 0 {
            line.push_str(&format!(
                " (serialised: {serialised} mutating agent{} share the tree)",
                if serialised == 1 { "" } else { "s" }
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
