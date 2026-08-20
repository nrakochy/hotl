//! `plan → flowchart LR` (0044 D4: output only, never an input format). One
//! subgraph per phase, one node per agent or stage, edges from the phases a
//! phase reads (`each` and template roots), a terminal `out[(output)]`.

use crate::plan::{AgentSpec, Plan, Shape};
use crate::select::Selector;
use crate::template::Template;

/// Mermaid node ids: `[A-Za-z0-9_]` only.
fn node_id(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// A quoted label; `"` is the one character Mermaid cannot take inside one.
fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "#quot;"))
}

fn agent_label(spec: &AgentSpec) -> String {
    let mut label = spec.label.clone();
    if let Some(v) = spec.votes.filter(|v| *v > 1) {
        label.push_str(&format!(" ×{v}"));
    }
    label
}

/// Phases this phase reads, in first-reference order, itself included for an
/// `until_quiet` phase (its prompts read the union so far).
fn reads(plan: &Plan, phase: &crate::plan::Phase) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |root: &str| {
        if plan.titles().any(|t| t == root) && !out.iter().any(|r| r == root) {
            out.push(root.to_string());
        }
    };
    if let Some(each) = &phase.each {
        if let Ok(sel) = Selector::parse(each) {
            push(&sel.root);
        }
    }
    for spec in phase.specs() {
        for text in [&spec.label, &spec.prompt] {
            if let Ok(t) = Template::parse(text) {
                for root in t.roots() {
                    push(root);
                }
            }
        }
    }
    out
}

pub fn render(plan: &Plan) -> String {
    let mut out = String::from("flowchart LR\n");
    for phase in &plan.phases {
        let pid = node_id(&phase.title);
        let title = match phase.shape() {
            Ok(Shape::Each { selector, .. }) => format!("{} (each {selector})", phase.title),
            Ok(Shape::UntilQuiet { cfg, .. }) => format!(
                "{} (until quiet ×{}, ≤{} rounds)",
                phase.title, cfg.rounds, cfg.max_rounds
            ),
            _ => phase.title.clone(),
        };
        out.push_str(&format!("  subgraph {pid}[{}]\n", quote(&title)));
        for (i, spec) in phase.specs().iter().enumerate() {
            out.push_str(&format!(
                "    {pid}_{i}_{}[{}]\n",
                node_id(&spec.label),
                quote(&agent_label(spec))
            ));
        }
        out.push_str("  end\n");
        for src in reads(plan, phase) {
            out.push_str(&format!("  {} --> {pid}\n", node_id(&src)));
        }
    }
    let last = plan
        .output
        .as_deref()
        .and_then(|o| Selector::parse(o).ok())
        .map(|s| s.root)
        .or_else(|| plan.titles().last().map(str::to_string));
    if let Some(last) = last {
        out.push_str(&format!("  {} --> out[(output)]\n", node_id(&last)));
    }
    out
}

#[cfg(test)]
mod tests {
    use crate::plan::tests::fixture;

    #[test]
    fn golden_for_the_fixture() {
        let expected = "\
flowchart LR
  subgraph Review[\"Review\"]
    Review_0_bugs[\"bugs\"]
    Review_1_perf[\"perf\"]
  end
  subgraph Verify[\"Verify (each Review[*].findings[*])\"]
    Verify_0_verify___item_file__[\"verify:{{item.file}} ×3\"]
  end
  Review --> Verify
  subgraph Find[\"Find (until quiet ×2, ≤10 rounds)\"]
    Find_0_finder[\"finder\"]
  end
  Find --> Find
  Find --> out[(output)]
";
        assert_eq!(super::render(&fixture()), expected);
    }

    #[test]
    fn quotes_and_output_selectors_are_handled() {
        let mut plan = fixture();
        plan.output = Some("Review[0]".into());
        plan.phases[0].agents.as_mut().unwrap()[0].label = "say \"hi\"".into();
        let m = super::render(&plan);
        assert!(m.contains("[\"say #quot;hi#quot;\"]"), "{m}");
        assert!(m.ends_with("  Review --> out[(output)]\n"), "{m}");
    }
}
