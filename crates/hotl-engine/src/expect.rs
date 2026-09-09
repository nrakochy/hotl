//! Expectation-checked tool calls (0050 T5).
//!
//! The model may say what it predicts a `bash` or `grep` call returns. This
//! module is the check: pure, total, and reading only [`OutcomeFacts`] —
//! never the result's prose, which is the model's text and not a protocol.
//!
//! Expectations are **model-stated only** (OD8). The harness never assumes a
//! verify-class command "should" exit 0: running tests to reproduce a failure
//! is routine, and a false surprise there would stop batches the model meant
//! to run.

use hotl_tools::ToolOutcome;
use serde_json::Value;

/// A result that did not match what the model said it expected.
#[derive(Debug, Clone, PartialEq)]
pub enum Miss {
    /// The prediction was well-formed and wrong.
    Expected { what: String, got: String },
    /// The prediction itself was malformed — the model's own mistake, not a
    /// surprise about the world, so it never stops a batch.
    Malformed(String),
}

impl Miss {
    /// Appended to the surprising call's own result, so the model sees the
    /// miss where it looks for the answer.
    pub fn trailer(&self) -> String {
        match self {
            Self::Expected { what, got } => {
                format!("\n[expectation failed: expected {what}, got {got}]")
            }
            Self::Malformed(why) => format!("\n[expectation ignored: {why}]"),
        }
    }

    /// The result every call after the miss gets. Names the call that
    /// surprised the model and why, so the next sample needs no archaeology.
    pub fn skip_text(&self, call_index: usize) -> String {
        match self {
            Self::Expected { what, got } => format!(
                "Not executed: call {call_index} mispredicted (expected {what}, got {got})."
            ),
            Self::Malformed(why) => format!("Not executed: call {call_index} ({why})."),
        }
    }

    /// One line for the reminder: what was expected and what happened.
    pub fn what(&self) -> String {
        match self {
            Self::Expected { what, .. } => what.clone(),
            Self::Malformed(why) => why.clone(),
        }
    }

    pub fn got(&self) -> String {
        match self {
            Self::Expected { got, .. } => got.clone(),
            Self::Malformed(_) => "nothing to compare".into(),
        }
    }
}

/// Check one call's result against its stated `expect`, if any.
///
/// `None` = nothing to check, or nothing to report. Only `bash` and `grep`
/// carry the field today; an `expect` on any other tool is ignored rather
/// than rejected, because a tool that never advertised the key cannot be
/// blamed for a model that guessed it.
pub fn check(tool: &str, input: &Value, outcome: &ToolOutcome) -> Option<Miss> {
    let expect = input.get("expect")?;
    if !matches!(tool, "bash" | "grep") {
        return None;
    }
    let Some(obj) = expect.as_object() else {
        return Some(Miss::Malformed(
            "`expect` must be an object; the call ran anyway".into(),
        ));
    };
    if obj.is_empty() {
        return None;
    }
    for (key, value) in obj {
        let miss = match (tool, key.as_str()) {
            ("bash", "exit") => check_exit(value, outcome),
            ("bash", "contains") => check_contains(value, outcome),
            ("bash", "empty") => check_empty(value, outcome),
            ("grep", "matches") => check_matches(value, outcome),
            _ => Some(Miss::Malformed(format!(
                "`expect.{key}` is not a key `{tool}` understands; the call ran anyway"
            ))),
        };
        // First miss wins: the model gets one clear surprise, not a list.
        if miss.is_some() {
            return miss;
        }
    }
    None
}

fn check_exit(value: &Value, outcome: &ToolOutcome) -> Option<Miss> {
    let want = match value.as_str() {
        Some("zero") => true,
        Some("nonzero") => false,
        _ => {
            return Some(Miss::Malformed(
                "`expect.exit` must be \"zero\" or \"nonzero\"; the call ran anyway".into(),
            ))
        }
    };
    // A process that never exited on its own (killed by a signal) has no code
    // to compare — neither met nor missed.
    let code = outcome.facts.exit?;
    if (code == 0) == want {
        return None;
    }
    Some(Miss::Expected {
        what: if want {
            "exit 0".into()
        } else {
            "a nonzero exit".into()
        },
        got: format!("exit {code}"),
    })
}

fn check_contains(value: &Value, outcome: &ToolOutcome) -> Option<Miss> {
    let Some(needle) = value.as_str() else {
        return Some(Miss::Malformed(
            "`expect.contains` must be a string; the call ran anyway".into(),
        ));
    };
    if outcome.content.contains(needle) {
        return None;
    }
    Some(Miss::Expected {
        what: format!("output containing `{needle}`"),
        got: "output without it".into(),
    })
}

fn check_empty(value: &Value, outcome: &ToolOutcome) -> Option<Miss> {
    let Some(want_empty) = value.as_bool() else {
        return Some(Miss::Malformed(
            "`expect.empty` must be true or false; the call ran anyway".into(),
        ));
    };
    // `bash` renders no output as the literal `(no output)`, which is what a
    // model reading the transcript sees as empty.
    let body = outcome.content.trim();
    let is_empty = body.is_empty() || body == "(no output)";
    if is_empty == want_empty {
        return None;
    }
    Some(Miss::Expected {
        what: if want_empty {
            "no output".into()
        } else {
            "some output".into()
        },
        got: if is_empty {
            "no output".into()
        } else {
            "output".into()
        },
    })
}

fn check_matches(value: &Value, outcome: &ToolOutcome) -> Option<Miss> {
    let want = match value.as_str() {
        Some("some") => true,
        Some("none") => false,
        _ => {
            return Some(Miss::Malformed(
                "`expect.matches` must be \"some\" or \"none\"; the call ran anyway".into(),
            ))
        }
    };
    let matched = outcome.facts.matched?;
    if matched == want {
        return None;
    }
    Some(Miss::Expected {
        what: if want {
            "at least one match".into()
        } else {
            "no matches".into()
        },
        got: if matched {
            "matches".into()
        } else {
            "no matches".into()
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hotl_tools::OutcomeFacts;
    use serde_json::json;

    fn shell(content: &str, exit: i32) -> ToolOutcome {
        let outcome = if exit == 0 {
            ToolOutcome::ok(content)
        } else {
            ToolOutcome::err(content)
        };
        outcome.with_facts(OutcomeFacts {
            exit: Some(exit),
            ..Default::default()
        })
    }

    fn searched(matched: bool) -> ToolOutcome {
        ToolOutcome::ok(if matched { "a.rs:1:hit" } else { "No matches" }).with_facts(
            OutcomeFacts {
                matched: Some(matched),
                ..Default::default()
            },
        )
    }

    #[test]
    fn a_stated_exit_is_checked_both_ways() {
        let miss = check(
            "bash",
            &json!({"expect": {"exit": "zero"}}),
            &shell("boom", 1),
        );
        assert_eq!(
            miss,
            Some(Miss::Expected {
                what: "exit 0".into(),
                got: "exit 1".into()
            })
        );
        assert!(check(
            "bash",
            &json!({"expect": {"exit": "zero"}}),
            &shell("ok", 0)
        )
        .is_none());
        assert!(check(
            "bash",
            &json!({"expect": {"exit": "nonzero"}}),
            &shell("ok", 0)
        )
        .is_some());
        assert!(check(
            "bash",
            &json!({"expect": {"exit": "nonzero"}}),
            &shell("boom", 2)
        )
        .is_none());
    }

    #[test]
    fn contains_and_empty_read_the_content() {
        assert!(check(
            "bash",
            &json!({"expect": {"contains": "ok"}}),
            &shell("nope", 0)
        )
        .is_some());
        assert!(check(
            "bash",
            &json!({"expect": {"contains": "ok"}}),
            &shell("all ok here", 0)
        )
        .is_none());
        // `(no output)` is how bash renders empty, so it counts as empty.
        assert!(check(
            "bash",
            &json!({"expect": {"empty": true}}),
            &shell("(no output)", 0)
        )
        .is_none());
        assert!(check(
            "bash",
            &json!({"expect": {"empty": true}}),
            &shell("something", 0)
        )
        .is_some());
    }

    #[test]
    fn a_search_prediction_reads_matched_not_the_prose() {
        assert!(check(
            "grep",
            &json!({"expect": {"matches": "none"}}),
            &searched(true)
        )
        .is_some());
        assert!(check(
            "grep",
            &json!({"expect": {"matches": "some"}}),
            &searched(false)
        )
        .is_some());
        assert!(check(
            "grep",
            &json!({"expect": {"matches": "some"}}),
            &searched(true)
        )
        .is_none());
    }

    #[test]
    fn no_expect_is_never_a_miss() {
        assert!(check("bash", &json!({"command": "false"}), &shell("boom", 1)).is_none());
        // A tool that never advertised the key is not blamed for it.
        assert!(check("read", &json!({"expect": {"exit": "zero"}}), &shell("x", 1)).is_none());
        // An empty object states nothing.
        assert!(check("bash", &json!({"expect": {}}), &shell("boom", 1)).is_none());
    }

    #[test]
    fn a_malformed_expect_names_the_key() {
        let unknown = check(
            "bash",
            &json!({"expect": {"colour": "blue"}}),
            &shell("x", 0),
        );
        assert!(
            matches!(&unknown, Some(Miss::Malformed(m)) if m.contains("colour")),
            "{unknown:?}"
        );
        let wrong_type = check("bash", &json!({"expect": {"exit": 0}}), &shell("x", 0));
        assert!(
            matches!(&wrong_type, Some(Miss::Malformed(m)) if m.contains("exit")),
            "{wrong_type:?}"
        );
        let not_object = check("bash", &json!({"expect": "zero"}), &shell("x", 0));
        assert!(
            matches!(not_object, Some(Miss::Malformed(_))),
            "{not_object:?}"
        );
    }

    /// A signal-killed process has no code, so a prediction about its exit is
    /// neither met nor missed — inventing one would stop batches on noise.
    #[test]
    fn a_fact_the_tool_could_not_state_is_not_a_miss() {
        let killed = ToolOutcome::err("[killed by SIGKILL]");
        assert!(check("bash", &json!({"expect": {"exit": "zero"}}), &killed).is_none());
    }

    #[test]
    fn the_texts_name_the_expectation_the_observation_and_the_next_step() {
        let miss = Miss::Expected {
            what: "exit 0".into(),
            got: "exit 1".into(),
        };
        assert_eq!(
            miss.trailer(),
            "\n[expectation failed: expected exit 0, got exit 1]"
        );
        assert_eq!(
            miss.skip_text(1),
            "Not executed: call 1 mispredicted (expected exit 0, got exit 1)."
        );
    }
}
