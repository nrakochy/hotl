//! Selectors: `Ident ('.' Ident | '[*]' | '[' N ']')*` over the run's scope.
//!
//! Roots are phase titles, `args`, and (inside an `each` phase) `item`/`prev`.
//! `[*]` flattens one level and skips `null` elements — a failed agent's slot
//! — so one refused agent never takes a whole downstream phase with it.

use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Key(String),
    Index(usize),
    Flatten,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selector {
    pub root: String,
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SelectError {
    #[error("selector `{selector}`: {message}")]
    Parse { selector: String, message: String },
    #[error("selector `{selector}`: {message}")]
    Eval { selector: String, message: String },
}

/// Where a name may be looked up: the executor's scope, or a test map.
pub trait Lookup {
    fn get(&self, root: &str) -> Option<&Value>;
}

impl Lookup for serde_json::Map<String, Value> {
    fn get(&self, root: &str) -> Option<&Value> {
        serde_json::Map::get(self, root)
    }
}

pub fn is_ident(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// The cursor while evaluating: one value, or the elements a `[*]` spread.
enum Cursor {
    One(Value),
    Many(Vec<Value>),
}

impl Selector {
    pub fn parse(text: &str) -> Result<Selector, SelectError> {
        let err = |message: String| SelectError::Parse {
            selector: text.to_string(),
            message,
        };
        let s = text.trim();
        if s.is_empty() {
            return Err(err("empty selector".into()));
        }
        let ident_end = s.find(['.', '[']).unwrap_or(s.len());
        let root = &s[..ident_end];
        if !is_ident(root) {
            return Err(err(format!("`{root}` is not a valid root name")));
        }
        let mut steps = Vec::new();
        let mut rest = &s[ident_end..];
        while !rest.is_empty() {
            if let Some(after) = rest.strip_prefix('.') {
                let end = after.find(['.', '[']).unwrap_or(after.len());
                let key = &after[..end];
                if !is_ident(key) {
                    return Err(err(format!("`{key}` is not a valid key after `.`")));
                }
                steps.push(Step::Key(key.to_string()));
                rest = &after[end..];
            } else if let Some(after) = rest.strip_prefix("[*]") {
                steps.push(Step::Flatten);
                rest = after;
            } else if let Some(after) = rest.strip_prefix('[') {
                let Some(close) = after.find(']') else {
                    return Err(err("unclosed `[`".into()));
                };
                let n: usize = after[..close].trim().parse().map_err(|_| {
                    err(format!("`[{}]` is not `[*]` or an index", &after[..close]))
                })?;
                steps.push(Step::Index(n));
                rest = &after[close + 1..];
            } else {
                return Err(err(format!("unexpected `{rest}`")));
            }
        }
        Ok(Selector {
            root: root.to_string(),
            steps,
        })
    }

    pub fn eval(&self, scope: &dyn Lookup) -> Result<Value, SelectError> {
        let err = |message: String| SelectError::Eval {
            selector: self.to_string(),
            message,
        };
        let Some(root) = scope.get(&self.root) else {
            return Err(err(format!("`{}` is not available here", self.root)));
        };
        let mut cursor = Cursor::One(root.clone());
        for step in &self.steps {
            cursor = match (cursor, step) {
                (Cursor::One(v), Step::Key(k)) => Cursor::One(key(&v, k).map_err(&err)?),
                (Cursor::One(v), Step::Index(n)) => Cursor::One(index(&v, *n).map_err(&err)?),
                (Cursor::One(v), Step::Flatten) => Cursor::Many(spread(&v).map_err(&err)?),
                (Cursor::Many(vs), Step::Key(k)) => Cursor::Many(
                    vs.iter()
                        .filter(|v| !v.is_null())
                        .map(|v| key(v, k))
                        .collect::<Result<_, _>>()
                        .map_err(&err)?,
                ),
                (Cursor::Many(vs), Step::Index(n)) => Cursor::Many(
                    vs.iter()
                        .filter(|v| !v.is_null())
                        .map(|v| index(v, *n))
                        .collect::<Result<_, _>>()
                        .map_err(&err)?,
                ),
                (Cursor::Many(vs), Step::Flatten) => {
                    let mut out = Vec::new();
                    for v in vs.iter().filter(|v| !v.is_null()) {
                        out.extend(spread(v).map_err(&err)?);
                    }
                    Cursor::Many(out)
                }
            };
        }
        Ok(match cursor {
            Cursor::One(v) => v,
            Cursor::Many(vs) => Value::Array(vs),
        })
    }
}

fn key(v: &Value, k: &str) -> Result<Value, String> {
    match v {
        Value::Object(map) => map
            .get(k)
            .cloned()
            .ok_or_else(|| format!("no key `{k}` (has: {})", keys_of(map))),
        other => Err(format!("`.{k}` needs an object, found {}", kind(other))),
    }
}

fn index(v: &Value, n: usize) -> Result<Value, String> {
    match v {
        Value::Array(items) => items
            .get(n)
            .cloned()
            .ok_or_else(|| format!("index {n} is out of range (length {})", items.len())),
        other => Err(format!("`[{n}]` needs an array, found {}", kind(other))),
    }
}

fn spread(v: &Value) -> Result<Vec<Value>, String> {
    match v {
        Value::Array(items) => Ok(items.iter().filter(|i| !i.is_null()).cloned().collect()),
        other => Err(format!("`[*]` needs an array, found {}", kind(other))),
    }
}

fn keys_of(map: &serde_json::Map<String, Value>) -> String {
    let mut keys: Vec<&str> = map.keys().map(String::as_str).take(8).collect();
    if map.len() > 8 {
        keys.push("…");
    }
    keys.join(", ")
}

pub(crate) fn kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

impl std::fmt::Display for Selector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.root)?;
        for s in &self.steps {
            match s {
                Step::Key(k) => write!(f, ".{k}")?,
                Step::Index(n) => write!(f, "[{n}]")?,
                Step::Flatten => f.write_str("[*]")?,
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn scope() -> serde_json::Map<String, Value> {
        let v = json!({
            "args": {"target": "src/", "n": 3},
            "Review": [
                {"findings": [{"file": "a.rs", "title": "t1"}, {"file": "b.rs", "title": "t2"}]},
                null,
                {"findings": [{"file": "c.rs", "title": "t3"}]}
            ],
            "Plain": "just text"
        });
        let Value::Object(m) = v else { unreachable!() };
        m
    }

    #[test]
    fn every_grammar_production_parses_and_round_trips() {
        for text in [
            "args",
            "args.target",
            "Review[*]",
            "Review[1]",
            "Review[*].findings[*].file",
            "a_b-c.x_y[0]",
        ] {
            let s = Selector::parse(text).unwrap();
            assert_eq!(s.to_string(), text);
        }
        assert_eq!(
            Selector::parse("Review[*].findings[ 2 ]").unwrap().steps,
            vec![Step::Flatten, Step::Key("findings".into()), Step::Index(2)]
        );
    }

    #[test]
    fn parse_errors_name_the_selector() {
        for bad in [
            "", "1abc", "args.", "args..x", "args[", "args[x]", "args x", ".args",
        ] {
            let e = Selector::parse(bad).unwrap_err();
            assert!(matches!(e, SelectError::Parse { .. }), "{bad}");
            assert!(
                e.to_string().contains(&format!("`{}`", bad.trim())),
                "{bad}: {e}"
            );
        }
    }

    #[test]
    fn flatten_spreads_and_skips_nulls() {
        let s = scope();
        let files = Selector::parse("Review[*].findings[*].file")
            .unwrap()
            .eval(&s)
            .unwrap();
        assert_eq!(files, json!(["a.rs", "b.rs", "c.rs"]));
        let one = Selector::parse("Review[0].findings[1].title")
            .unwrap()
            .eval(&s)
            .unwrap();
        assert_eq!(one, json!("t2"));
        assert_eq!(
            Selector::parse("args.n").unwrap().eval(&s).unwrap(),
            json!(3)
        );
        assert_eq!(
            Selector::parse("Plain").unwrap().eval(&s).unwrap(),
            json!("just text")
        );
    }

    #[test]
    fn eval_errors_name_the_path_and_the_problem() {
        let s = scope();
        let e = Selector::parse("Nope")
            .unwrap()
            .eval(&s)
            .unwrap_err()
            .to_string();
        assert!(e.contains("`Nope`") && e.contains("not available"), "{e}");
        let e = Selector::parse("Plain[*]")
            .unwrap()
            .eval(&s)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("Plain[*]") && e.contains("needs an array") && e.contains("a string"),
            "{e}"
        );
        let e = Selector::parse("Review[*].missing")
            .unwrap()
            .eval(&s)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("no key `missing`") && e.contains("findings"),
            "{e}"
        );
        let e = Selector::parse("Review[7]")
            .unwrap()
            .eval(&s)
            .unwrap_err()
            .to_string();
        assert!(e.contains("out of range"), "{e}");
        let e = Selector::parse("args.target.x")
            .unwrap()
            .eval(&s)
            .unwrap_err()
            .to_string();
        assert!(e.contains("needs an object"), "{e}");
    }
}
