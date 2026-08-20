//! The plan: one serde struct, JSON in a tool call or TOML on disk (0044 D1).
//!
//! A [`Phase`] is one flat struct with a [`Phase::shape`] accessor instead of
//! a `#[serde(untagged)]` enum: untagged errors are unreadable, and these
//! errors go back to the model to fix.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use hotl_provider::Effort;

use crate::select::{is_ident, Selector};
use crate::template::Template;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    /// `^[a-z0-9][a-z0-9-]*$` — also the file stem of a saved recipe.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Run width. Can lower the configured `[workflows] concurrency`, never raise it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<usize>,
    /// Agent-start ceiling for this run, capped by `[workflows] max_agents`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_agents: Option<usize>,
    /// Selector for the run's result; default = the last phase's output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(default)]
    pub phases: Vec<Phase>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phase {
    /// Also the root name later selectors/templates read this phase's output by.
    pub title: String,
    /// Parallel (with `until_quiet`: one round's agents).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents: Option<Vec<AgentSpec>>,
    /// Per-item pipeline: a selector over earlier outputs or `args`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub each: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stages: Option<Vec<AgentSpec>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until_quiet: Option<UntilQuiet>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UntilQuiet {
    /// Consecutive rounds that add nothing new before the phase stops.
    #[serde(default = "default_rounds")]
    pub rounds: usize,
    #[serde(default = "default_max_rounds")]
    pub max_rounds: usize,
    /// Comma-separated field paths that identify an element (`file,line`).
    pub key: String,
}

fn default_rounds() -> usize {
    2
}
fn default_max_rounds() -> usize {
    10
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Isolation {
    None,
    Worktree,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSpec {
    /// Templated; also half of the agent's id, so reused labels are fine.
    pub label: String,
    /// Templated. `{{args.x}}`, `{{Phase}}`, and inside `each`: `{{item}}`, `{{prev}}`.
    pub prompt: String,
    /// JSON Schema the answer must satisfy; without one the text is the value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<Value>,
    /// Agent def name; default `general-purpose`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolation: Option<Isolation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<i64>,
    /// N identical agents; `{accepted, votes}` by majority on `accept`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub votes: Option<usize>,
    /// Field path inside each vote whose truthiness is counted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accept: Option<String>,
}

/// What a phase does, once its fields are known to be consistent.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Shape<'a> {
    Parallel(&'a [AgentSpec]),
    Each {
        selector: &'a str,
        stages: &'a [AgentSpec],
    },
    UntilQuiet {
        cfg: &'a UntilQuiet,
        agents: &'a [AgentSpec],
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

/// One finding from [`Plan::validate`], phrased for the model to act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanError {
    pub severity: Severity,
    /// `plan`, `phase \`Review\``, `phase \`Review\` agent \`bugs\``.
    pub at: String,
    pub message: String,
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.at, self.message)
    }
}

/// Reserved roots a phase title may not shadow.
pub const RESERVED_ROOTS: [&str; 3] = ["args", "item", "prev"];

/// Built-in read-only agent defs (`hotl_tools::agents`), named here so a
/// `worktree` request on one can be called the no-op it is.
const READ_ONLY_BUILTINS: [&str; 2] = ["explore", "plan"];

pub fn is_plan_name(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

impl Phase {
    /// Exactly one of: `agents`; `each` + `stages`; `until_quiet` + `agents`.
    pub fn shape(&self) -> Result<Shape<'_>, String> {
        match (&self.agents, &self.each, &self.stages, &self.until_quiet) {
            (Some(agents), None, None, None) => Ok(Shape::Parallel(agents)),
            (None, Some(each), Some(stages), None) => Ok(Shape::Each {
                selector: each,
                stages,
            }),
            (Some(agents), None, None, Some(cfg)) => Ok(Shape::UntilQuiet { cfg, agents }),
            _ => Err(
                "use exactly one shape: `agents` (parallel), `each` + `stages` \
                      (per-item pipeline), or `until_quiet` + `agents` (repeat until quiet)"
                    .into(),
            ),
        }
    }

    /// Every agent spec, whichever shape.
    pub fn specs(&self) -> &[AgentSpec] {
        self.agents
            .as_deref()
            .or(self.stages.as_deref())
            .unwrap_or(&[])
    }
}

impl Plan {
    pub fn from_toml(text: &str) -> Result<Plan, String> {
        toml::from_str(text).map_err(|e| e.to_string())
    }

    pub fn from_json(value: Value) -> Result<Plan, String> {
        serde_json::from_value(value).map_err(|e| e.to_string())
    }

    /// Every error and warning. Errors mean nothing may run.
    pub fn validate(&self) -> Vec<PlanError> {
        let mut v = Validator::default();
        if !is_plan_name(&self.name) {
            v.error(
                "plan",
                format!("name `{}` must match ^[a-z0-9][a-z0-9-]*$", self.name),
            );
        }
        if self.phases.is_empty() {
            v.error("plan", "at least one phase is required");
        }
        if self.concurrency == Some(0) {
            v.error("plan", "`concurrency` must be at least 1");
        }
        if self.max_agents == Some(0) {
            v.error("plan", "`max_agents` must be at least 1");
        }
        let mut seen: HashSet<&str> = HashSet::new();
        let mut available: Vec<&str> = vec!["args"];
        for phase in &self.phases {
            let at = format!("phase `{}`", phase.title);
            if !is_ident(&phase.title) {
                v.error(&at, "title must be a name like `Review` or `find_bugs` (it is also a selector root)");
            } else if RESERVED_ROOTS.contains(&phase.title.as_str()) {
                v.error(&at, format!("title `{}` is reserved", phase.title));
            }
            if !seen.insert(&phase.title) {
                v.error(&at, "duplicate title");
            }
            let shape = match phase.shape() {
                Ok(s) => s,
                Err(msg) => {
                    v.error(&at, msg);
                    available.push(&phase.title);
                    continue;
                }
            };
            let mut roots: Vec<&str> = available.clone();
            let mut stage_roots: Vec<&str> = Vec::new();
            let specs: &[AgentSpec] = match shape {
                Shape::Parallel(agents) => {
                    if agents.is_empty() {
                        v.error(&at, "`agents` is empty");
                    }
                    agents
                }
                Shape::Each { selector, stages } => {
                    if stages.is_empty() {
                        v.error(&at, "`stages` is empty");
                    }
                    match Selector::parse(selector) {
                        Ok(sel) if !available.contains(&sel.root.as_str()) => v.error(
                            &at,
                            format!("`each = \"{selector}\"` reads `{}`, which is not an earlier phase or `args`", sel.root),
                        ),
                        Ok(_) => {}
                        Err(e) => v.error(&at, format!("`each`: {e}")),
                    }
                    stage_roots.push("item");
                    stages
                }
                Shape::UntilQuiet { cfg, agents } => {
                    if agents.is_empty() {
                        v.error(&at, "`agents` is empty");
                    }
                    if cfg.rounds == 0 {
                        v.error(&at, "`until_quiet.rounds` must be at least 1");
                    }
                    if cfg.max_rounds < cfg.rounds.max(1) {
                        v.error(&at, "`until_quiet.max_rounds` must be at least `rounds`");
                    }
                    if cfg.key.split(',').any(|k| k.trim().is_empty()) {
                        v.error(&at, "`until_quiet.key` is a comma-separated list of field paths, like `file,line`");
                    }
                    // The phase reads its own union so far.
                    roots.push(&phase.title);
                    agents
                }
            };
            for (i, spec) in specs.iter().enumerate() {
                let at = format!("{at} agent `{}`", spec.label);
                if i == 1 && matches!(shape, Shape::Each { .. }) {
                    stage_roots.push("prev");
                }
                let known = |root: &str| roots.contains(&root) || stage_roots.contains(&root);
                v.check_spec(&at, spec, &known);
            }
            available.push(&phase.title);
        }
        if let Some(output) = &self.output {
            match Selector::parse(output) {
                Ok(sel) if !available.contains(&sel.root.as_str()) => v.error(
                    "plan",
                    format!(
                        "`output = \"{output}\"` reads `{}`, which is not a phase or `args`",
                        sel.root
                    ),
                ),
                Ok(_) => {}
                Err(e) => v.error("plan", format!("`output`: {e}")),
            }
        }
        v.out
    }

    /// Errors only (what blocks a run).
    pub fn errors(&self) -> Vec<PlanError> {
        self.validate()
            .into_iter()
            .filter(|e| e.severity == Severity::Error)
            .collect()
    }

    /// Phase titles in order.
    pub fn titles(&self) -> impl Iterator<Item = &str> {
        self.phases.iter().map(|p| p.title.as_str())
    }
}

/// The JSON Schema for [`Plan`] the `workflow` tool embeds in its own input
/// schema — what teaches the model the format. Hand-written to stay short;
/// `fixture_validates_against_the_plan_schema` keeps it honest.
pub fn json_schema() -> Value {
    let agent = serde_json::json!({
        "type": "object",
        "required": ["label", "prompt"],
        "additionalProperties": false,
        "properties": {
            "label": {"type": "string", "description": "Short name; may use {{item.x}} inside `each`."},
            "prompt": {"type": "string", "description": "The brief. Templates: {{args.x}}, {{PhaseTitle}}, and inside `each`: {{item}}, {{prev}}."},
            "schema": {"type": "object", "description": "JSON Schema the answer must satisfy (validated, retried twice). Without one the answer is a text string."},
            "agent": {"type": "string", "description": "Agent def: general-purpose (default), explore or plan (read-only), or agents/*.md."},
            "model": {"type": "string"},
            "effort": {"type": "string", "enum": ["low", "medium", "high", "xhigh", "max"]},
            "isolation": {"type": "string", "enum": ["none", "worktree"], "description": "worktree: own git checkout, merged back on success."},
            "max_turns": {"type": "integer", "minimum": 1},
            "votes": {"type": "integer", "minimum": 1, "description": "Run N identical agents; value becomes {accepted, votes} by majority on `accept`. Needs `schema`."},
            "accept": {"type": "string", "description": "Field path in each vote whose truthiness is counted, e.g. isReal."}
        }
    });
    serde_json::json!({
        "type": "object",
        "required": ["name", "phases"],
        "additionalProperties": false,
        "properties": {
            "name": {"type": "string", "pattern": "^[a-z0-9][a-z0-9-]*$"},
            "description": {"type": "string"},
            "concurrency": {"type": "integer", "minimum": 1, "description": "Lowers the configured width; never raises it."},
            "max_agents": {"type": "integer", "minimum": 1},
            "output": {"type": "string", "description": "Selector for the result, e.g. Verify[*].votes; default: the last phase's output."},
            "phases": {
                "type": "array",
                "minItems": 1,
                "items": {
                    "type": "object",
                    "required": ["title"],
                    "additionalProperties": false,
                    "description": "Exactly one shape: `agents` (all at once); `each` + `stages` (a pipeline per selected item, no barrier); `until_quiet` + `agents` (rounds until nothing new).",
                    "properties": {
                        "title": {"type": "string", "description": "Identifier; later phases read this phase's output as {{Title}} / Title[*]."},
                        "agents": {"type": "array", "items": agent},
                        "each": {"type": "string", "description": "Selector over earlier outputs or args: Review[*].findings[*]"},
                        "stages": {"type": "array", "items": agent},
                        "until_quiet": {
                            "type": "object",
                            "required": ["key"],
                            "additionalProperties": false,
                            "properties": {
                                "rounds": {"type": "integer", "minimum": 1, "default": 2, "description": "Quiet rounds before stopping."},
                                "max_rounds": {"type": "integer", "minimum": 1, "default": 10},
                                "key": {"type": "string", "description": "Comma-separated field paths that identify an element: file,line"}
                            }
                        }
                    }
                }
            }
        }
    })
}

#[derive(Default)]
struct Validator {
    out: Vec<PlanError>,
}

impl Validator {
    fn error(&mut self, at: &str, message: impl Into<String>) {
        self.out.push(PlanError {
            severity: Severity::Error,
            at: at.to_string(),
            message: message.into(),
        });
    }
    fn warn(&mut self, at: &str, message: impl Into<String>) {
        self.out.push(PlanError {
            severity: Severity::Warning,
            at: at.to_string(),
            message: message.into(),
        });
    }

    fn check_spec(&mut self, at: &str, spec: &AgentSpec, known: &dyn Fn(&str) -> bool) {
        if spec.label.trim().is_empty() {
            self.error(at, "`label` is empty");
        }
        if spec.prompt.trim().is_empty() {
            self.error(at, "`prompt` is empty");
        }
        for (field, text) in [("label", &spec.label), ("prompt", &spec.prompt)] {
            match Template::parse(text) {
                Ok(t) => {
                    for root in t.roots() {
                        if !known(root) {
                            self.error(at, format!("`{field}` reads `{{{{{root}…}}}}`, which is not available here"));
                        }
                    }
                }
                Err(e) => self.error(at, format!("`{field}`: {e}")),
            }
        }
        if let Some(schema) = &spec.schema {
            if let Err(e) = jsonschema::validator_for(schema) {
                self.error(at, format!("`schema` is not a valid JSON Schema: {e}"));
            }
        }
        match spec.votes {
            Some(0) => self.error(at, "`votes` must be at least 1"),
            Some(n) => {
                if spec.accept.is_none() {
                    self.error(
                        at,
                        "`votes` needs `accept` (the field whose truthiness is counted)",
                    );
                }
                if spec.schema.is_none() {
                    self.error(
                        at,
                        "`votes` needs a `schema` so the votes are values, not prose",
                    );
                }
                if n % 2 == 0 {
                    self.warn(at, format!("`votes = {n}` is even; a tie is not accepted"));
                }
            }
            None => {
                if spec.accept.is_some() {
                    self.warn(at, "`accept` does nothing without `votes`");
                }
            }
        }
        if spec.isolation == Some(Isolation::Worktree)
            && spec
                .agent
                .as_deref()
                .is_some_and(|a| READ_ONLY_BUILTINS.contains(&a))
        {
            self.warn(
                at,
                "`isolation = \"worktree\"` is a no-op on a read-only agent",
            );
        }
        if spec.max_turns.is_some_and(|n| n < 1) {
            self.error(at, "`max_turns` must be at least 1");
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    pub(crate) const FIXTURE: &str = include_str!("../tests/fixtures/review-changes.toml");

    pub(crate) fn fixture() -> Plan {
        Plan::from_toml(FIXTURE).unwrap()
    }

    fn errors_of(plan: &Plan) -> Vec<String> {
        plan.errors().iter().map(ToString::to_string).collect()
    }

    #[test]
    fn toml_and_json_parse_to_the_same_plan() {
        let plan = fixture();
        let as_json = serde_json::to_value(&plan).unwrap();
        assert_eq!(Plan::from_json(as_json).unwrap(), plan);
        assert_eq!(plan.name, "review-changes");
        assert_eq!(
            plan.titles().collect::<Vec<_>>(),
            ["Review", "Verify", "Find"]
        );
        assert!(matches!(plan.phases[0].shape(), Ok(Shape::Parallel(a)) if a.len() == 2));
        assert!(
            matches!(plan.phases[1].shape(), Ok(Shape::Each { selector: "Review[*].findings[*]", stages }) if stages[0].votes == Some(3))
        );
        assert!(
            matches!(plan.phases[2].shape(), Ok(Shape::UntilQuiet { cfg, .. }) if cfg.rounds == 2 && cfg.max_rounds == 10)
        );
        assert_eq!(plan.phases[0].agents.as_ref().unwrap()[0].effort, None);
        assert!(plan.validate().is_empty(), "{:?}", plan.validate());
    }

    #[test]
    fn effort_and_isolation_parse_as_enums_and_unknown_fields_are_rejected() {
        let plan = Plan::from_json(json!({
            "name": "x",
            "phases": [{"title": "A", "agents": [{"label": "a", "prompt": "p", "effort": "xhigh", "isolation": "worktree"}]}]
        }))
        .unwrap();
        let spec = &plan.phases[0].agents.as_ref().unwrap()[0];
        assert_eq!(spec.effort, Some(Effort::XHigh));
        assert_eq!(spec.isolation, Some(Isolation::Worktree));

        let e = Plan::from_json(json!({"name": "x", "phases": [{"title": "A", "agents": [{"label": "a", "prompt": "p", "effort": "ultra"}]}]})).unwrap_err();
        assert!(e.contains("ultra") || e.contains("variant"), "{e}");
        let e = Plan::from_json(json!({"name": "x", "phases": [], "concurrncy": 3})).unwrap_err();
        assert!(e.contains("concurrncy"), "{e}");
        let e =
            Plan::from_toml("name = \"x\"\n[[phases]]\ntitle = \"A\"\nagnets = []\n").unwrap_err();
        assert!(e.contains("agnets"), "{e}");
    }

    #[test]
    fn a_phase_must_have_exactly_one_shape() {
        let both = Phase {
            title: "A".into(),
            agents: Some(vec![]),
            each: Some("args.x".into()),
            stages: Some(vec![]),
            until_quiet: None,
        };
        assert!(both.shape().unwrap_err().contains("exactly one shape"));
        let none = Phase {
            title: "A".into(),
            agents: None,
            each: None,
            stages: None,
            until_quiet: None,
        };
        assert!(none.shape().is_err());
        let each_without_stages = Phase {
            each: Some("args.x".into()),
            ..none.clone()
        };
        assert!(each_without_stages.shape().is_err());
    }

    fn plan_with(phases: Value) -> Plan {
        Plan::from_json(json!({"name": "ok", "phases": phases})).unwrap()
    }

    #[test]
    fn every_validation_rule_has_a_failing_case_with_a_prompt_shaped_message() {
        let bad_name = Plan::from_json(json!({"name": "Bad Name", "phases": [{"title": "A", "agents": [{"label": "a", "prompt": "p"}]}]})).unwrap();
        assert!(errors_of(&bad_name)[0].contains("^[a-z0-9][a-z0-9-]*$"));

        let empty = Plan::from_json(json!({"name": "x"})).unwrap();
        assert!(errors_of(&empty)[0].contains("at least one phase"));

        let dup = plan_with(json!([
            {"title": "A", "agents": [{"label": "a", "prompt": "p"}]},
            {"title": "A", "agents": [{"label": "a", "prompt": "p"}]}
        ]));
        assert!(errors_of(&dup)
            .iter()
            .any(|e| e.contains("phase `A`: duplicate title")));

        let reserved =
            plan_with(json!([{"title": "args", "agents": [{"label": "a", "prompt": "p"}]}]));
        assert!(errors_of(&reserved)[0].contains("reserved"));
        let spaced =
            plan_with(json!([{"title": "Find bugs", "agents": [{"label": "a", "prompt": "p"}]}]));
        assert!(errors_of(&spaced)[0].contains("selector root"));

        let no_agents = plan_with(json!([{"title": "A", "agents": []}]));
        assert!(errors_of(&no_agents)[0].contains("`agents` is empty"));

        let bad_schema = plan_with(
            json!([{"title": "A", "agents": [{"label": "a", "prompt": "p", "schema": {"type": "nope"}}]}]),
        );
        assert!(errors_of(&bad_schema)[0].contains("not a valid JSON Schema"));

        let dangling = plan_with(
            json!([{"title": "A", "agents": [{"label": "a", "prompt": "see {{B}} and {{item}}"}]}]),
        );
        let errs = errors_of(&dangling);
        assert!(
            errs.iter().any(|e| e.contains("`prompt` reads `{{B…}}`")),
            "{errs:?}"
        );
        assert!(errs.iter().any(|e| e.contains("{{item…}}")), "{errs:?}");
        let self_ref =
            plan_with(json!([{"title": "A", "agents": [{"label": "a", "prompt": "{{A}}"}]}]));
        assert!(
            errors_of(&self_ref)[0].contains("{{A…}}"),
            "a parallel phase cannot read itself"
        );

        let bad_each = plan_with(json!([
            {"title": "A", "agents": [{"label": "a", "prompt": "p"}]},
            {"title": "B", "each": "C[*]", "stages": [{"label": "s", "prompt": "{{item}}"}]}
        ]));
        assert!(errors_of(&bad_each)[0].contains("`each = \"C[*]\"` reads `C`"));
        let unparsable_each = plan_with(
            json!([{"title": "B", "each": "args[", "stages": [{"label": "s", "prompt": "p"}]}]),
        );
        assert!(errors_of(&unparsable_each)[0].contains("`each`: selector `args[`"));

        let prev_in_first_stage =
            plan_with(json!([{"title": "B", "each": "args.items", "stages": [
                {"label": "s1", "prompt": "{{prev}}"},
                {"label": "s2", "prompt": "{{prev}} {{item}}"}
            ]}]));
        let errs = errors_of(&prev_in_first_stage);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].contains("agent `s1`") && errs[0].contains("{{prev…}}"));

        let votes = plan_with(
            json!([{"title": "A", "agents": [{"label": "a", "prompt": "p", "votes": 3}]}]),
        );
        let errs = errors_of(&votes);
        assert!(
            errs.iter().any(|e| e.contains("needs `accept`")),
            "{errs:?}"
        );
        assert!(
            errs.iter().any(|e| e.contains("needs a `schema`")),
            "{errs:?}"
        );

        let uq = plan_with(
            json!([{"title": "F", "until_quiet": {"rounds": 3, "max_rounds": 2, "key": "file,"}, "agents": [{"label": "a", "prompt": "{{F}}"}]}]),
        );
        let errs = errors_of(&uq);
        assert!(errs.iter().any(|e| e.contains("max_rounds")), "{errs:?}");
        assert!(
            errs.iter().any(|e| e.contains("comma-separated")),
            "{errs:?}"
        );

        let output = Plan::from_json(json!({"name": "x", "output": "Nope.x", "phases": [{"title": "A", "agents": [{"label": "a", "prompt": "p"}]}]})).unwrap();
        assert!(errors_of(&output)[0].contains("`output = \"Nope.x\"` reads `Nope`"));

        let caps = Plan::from_json(json!({"name": "x", "concurrency": 0, "max_agents": 0, "phases": [{"title": "A", "agents": [{"label": "a", "prompt": "p", "max_turns": 0}]}]})).unwrap();
        let errs = errors_of(&caps);
        assert!(errs
            .iter()
            .any(|e| e.contains("`concurrency` must be at least 1")));
        assert!(errs
            .iter()
            .any(|e| e.contains("`max_agents` must be at least 1")));
        assert!(errs
            .iter()
            .any(|e| e.contains("`max_turns` must be at least 1")));
    }

    #[test]
    fn fixture_validates_against_the_plan_schema() {
        let schema = jsonschema::validator_for(&json_schema()).unwrap();
        let plan = serde_json::to_value(fixture()).unwrap();
        let errors: Vec<String> = schema.iter_errors(&plan).map(|e| e.to_string()).collect();
        assert!(errors.is_empty(), "{errors:?}");
        assert!(!schema.is_valid(&json!({"name": "x", "phases": [{"title": "A", "agnets": []}]})));
    }

    #[test]
    fn warnings_do_not_block_a_run() {
        let plan = plan_with(json!([{"title": "A", "agents": [
            {"label": "a", "prompt": "p", "votes": 2, "accept": "ok", "schema": {"type": "object"}},
            {"label": "b", "prompt": "p", "agent": "explore", "isolation": "worktree"},
            {"label": "c", "prompt": "p", "accept": "ok"}
        ]}]));
        assert!(plan.errors().is_empty());
        let warnings: Vec<String> = plan.validate().iter().map(ToString::to_string).collect();
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert!(warnings[0].contains("even"));
        assert!(warnings[1].contains("no-op on a read-only agent"));
        assert!(warnings[2].contains("`accept` does nothing"));
    }
}
