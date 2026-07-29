//! Fixture: real-shaped Rust for the minifier's property tests. Never compiled
//! as part of the crate — `include_str!`'d as data.

use std::collections::HashMap;

/// A parsed record.
#[derive(Debug, Clone)]
pub struct Record {
    pub name: String,
    pub count: u32,
}

pub enum Shape {
    Point,
    Line { len: u32 },
}

/// Characters that must survive verbatim: a statement separator and a comment
/// marker, both inside a string.
const TRICKY: &str = "a;b // not a comment";

/// A raw string spanning lines. One leaf token; its bytes are never rewritten.
const RAW: &str = r#"line one
line two; still inside // the raw string
"#;

impl Record {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            count: 0,
        }
    }

    pub fn describe(&self, shape: &Shape) -> String {
        match shape {
            Shape::Point => format!("{}: point", self.name),
            Shape::Line { len } if *len > 10 => format!("{}: long line", self.name),
            Shape::Line { len } => format!("{}: line of {len}", self.name),
        }
    }
}

pub fn tally(records: &[Record]) -> HashMap<String, u32> {
    let mut out = HashMap::new();
    for r in records {
        // Accumulate per name.
        *out.entry(r.name.clone()).or_insert(0) += r.count;
    }
    out
}

pub fn banner(empty: bool) -> &'static str {
    if empty || TRICKY.is_empty() {
        RAW
    } else {
        TRICKY
    }
}
