//! `{{path}}` templates in prompts and labels. A path is a [`Selector`];
//! strings render raw, anything else as compact JSON. Parsed once at
//! validation time so a dangling root fails the plan before any agent starts.

use serde_json::Value;
use thiserror::Error;

use crate::select::{Lookup, SelectError, Selector};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Lit(String),
    Expr(Selector),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    pub segments: Vec<Segment>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TemplateError {
    #[error("template: {0}")]
    Parse(String),
    #[error("{0}")]
    Select(#[from] SelectError),
}

impl Template {
    pub fn parse(text: &str) -> Result<Template, TemplateError> {
        let mut segments = Vec::new();
        let mut rest = text;
        while let Some(open) = rest.find("{{") {
            if open > 0 {
                segments.push(Segment::Lit(rest[..open].to_string()));
            }
            let after = &rest[open + 2..];
            let Some(close) = after.find("}}") else {
                return Err(TemplateError::Parse(format!(
                    "unclosed `{{{{` in `{}`",
                    snippet(&rest[open..])
                )));
            };
            segments.push(Segment::Expr(Selector::parse(&after[..close])?));
            rest = &after[close + 2..];
        }
        if !rest.is_empty() {
            segments.push(Segment::Lit(rest.to_string()));
        }
        Ok(Template { segments })
    }

    /// The root name of every `{{path}}`, in order, duplicates included.
    pub fn roots(&self) -> impl Iterator<Item = &str> {
        self.segments.iter().filter_map(|s| match s {
            Segment::Expr(sel) => Some(sel.root.as_str()),
            Segment::Lit(_) => None,
        })
    }

    pub fn render(&self, scope: &dyn Lookup) -> Result<String, TemplateError> {
        let mut out = String::new();
        for s in &self.segments {
            match s {
                Segment::Lit(t) => out.push_str(t),
                Segment::Expr(sel) => match sel.eval(scope)? {
                    Value::String(t) => out.push_str(&t),
                    other => out.push_str(&other.to_string()),
                },
            }
        }
        Ok(out)
    }
}

fn snippet(s: &str) -> String {
    let short: String = s.chars().take(24).collect();
    if short.len() < s.len() {
        format!("{short}…")
    } else {
        short
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn scope() -> serde_json::Map<String, Value> {
        let Value::Object(m) = json!({
            "args": {"target": "src/", "n": 3},
            "item": {"file": "a.rs", "tags": ["x", "y"]},
            "prev": {"ok": true}
        }) else {
            unreachable!()
        };
        m
    }

    #[test]
    fn strings_render_raw_and_everything_else_as_compact_json() {
        let t = Template::parse(
            "look at {{args.target}} ({{ args.n }} times): {{item.tags}} / {{prev}}",
        )
        .unwrap();
        assert_eq!(
            t.render(&scope()).unwrap(),
            r#"look at src/ (3 times): ["x","y"] / {"ok":true}"#
        );
        assert_eq!(
            t.roots().collect::<Vec<_>>(),
            ["args", "args", "item", "prev"]
        );
    }

    #[test]
    fn plain_text_and_single_braces_pass_through() {
        let t = Template::parse("Reply {isReal: bool, why}.").unwrap();
        assert_eq!(
            t.segments,
            vec![Segment::Lit("Reply {isReal: bool, why}.".into())]
        );
        assert_eq!(t.render(&scope()).unwrap(), "Reply {isReal: bool, why}.");
    }

    #[test]
    fn parse_errors_name_the_problem() {
        let e = Template::parse("see {{args.target")
            .unwrap_err()
            .to_string();
        assert!(e.contains("unclosed") && e.contains("{{args.target"), "{e}");
        let e = Template::parse("see {{1bad}}").unwrap_err().to_string();
        assert!(e.contains("`1bad`"), "{e}");
    }

    #[test]
    fn render_errors_name_the_path() {
        let e = Template::parse("{{item.nope}}")
            .unwrap()
            .render(&scope())
            .unwrap_err()
            .to_string();
        assert!(e.contains("item.nope") && e.contains("no key"), "{e}");
        let e = Template::parse("{{Review}}")
            .unwrap()
            .render(&scope())
            .unwrap_err()
            .to_string();
        assert!(e.contains("`Review` is not available"), "{e}");
    }
}
