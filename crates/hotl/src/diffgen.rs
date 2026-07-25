//! Unified-diff generation for the permission ask card.
//!
//! The human approving a `write`/`edit` should see what changes, not just the
//! tool's one-line summary — for a supervision-first product that was the
//! largest single gap in the UI. Generation lives here (the runtime crate)
//! because `write`'s "before" is the file on disk; `hotl-tui` stays pure and
//! only renders the `{op, text}` rows it receives over the wire.
//!
//! A plain LCS over lines, deliberately: `similar` would be nicer and is not
//! worth a workspace dependency for one card.

use serde_json::Value;

/// Longest side, in lines, before the O(n·m) LCS table is abandoned for a
/// whole-file replace. A minified bundle must not turn an approval prompt
/// into a multi-second stall.
const LCS_MAX_LINES: usize = 2_000;

/// Rows the card will show before it truncates. An approval card that scrolls
/// is an approval card nobody reads.
const MAX_DIFF_LINES: usize = 40;

/// Unchanged lines kept either side of a change.
const CONTEXT_LINES: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Ctx,
    Add,
    Del,
    /// The `[+N more lines]` trailer; never part of the file's content.
    Trailer,
}

impl Op {
    /// The wire spelling. `hotl-tui` decodes these back into its own `Op`.
    pub fn as_str(self) -> &'static str {
        match self {
            Op::Ctx => "ctx",
            Op::Add => "add",
            Op::Del => "del",
            Op::Trailer => "trailer",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub op: Op,
    pub text: String,
}

impl DiffLine {
    fn new(op: Op, text: impl Into<String>) -> Self {
        DiffLine {
            op,
            text: text.into(),
        }
    }

    pub fn to_json(&self) -> Value {
        serde_json::json!({"op": self.op.as_str(), "text": self.text})
    }
}

/// Split into lines, dropping the single empty element a trailing newline
/// produces — `"a\n"` is one line, not two.
fn lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut v: Vec<&str> = text.split('\n').collect();
    if v.last() == Some(&"") {
        v.pop();
    }
    v
}

/// A unified diff of `old` → `new`: context, deletions, and additions in file
/// order, trimmed to `CONTEXT_LINES` around each change and capped at
/// `MAX_DIFF_LINES` with a `Trailer`.
pub fn unified(old: &str, new: &str) -> Vec<DiffLine> {
    let a = lines(old);
    let b = lines(new);
    let full = if a.len() > LCS_MAX_LINES || b.len() > LCS_MAX_LINES {
        // Too big to diff cheaply: show it as a wholesale replacement rather
        // than stalling the human the engine is already blocked on.
        a.iter()
            .map(|l| DiffLine::new(Op::Del, *l))
            .chain(b.iter().map(|l| DiffLine::new(Op::Add, *l)))
            .collect()
    } else {
        script(&a, &b)
    };
    cap(trim_context(full))
}

/// The full edit script from an LCS table, without context trimming.
fn script(a: &[&str], b: &[&str]) -> Vec<DiffLine> {
    // lcs[i][j] = length of the LCS of a[i..] and b[j..].
    let mut lcs = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::new();
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            out.push(DiffLine::new(Op::Ctx, a[i]));
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            out.push(DiffLine::new(Op::Del, a[i]));
            i += 1;
        } else {
            out.push(DiffLine::new(Op::Add, b[j]));
            j += 1;
        }
    }
    out.extend(a[i..].iter().map(|l| DiffLine::new(Op::Del, *l)));
    out.extend(b[j..].iter().map(|l| DiffLine::new(Op::Add, *l)));
    out
}

/// Drop runs of context more than `CONTEXT_LINES` from any change. A file
/// with no changes at all keeps nothing — there is nothing to approve.
fn trim_context(script: Vec<DiffLine>) -> Vec<DiffLine> {
    let changed: Vec<bool> = script.iter().map(|l| l.op != Op::Ctx).collect();
    if !changed.iter().any(|&c| c) {
        return Vec::new();
    }
    script
        .into_iter()
        .enumerate()
        .filter(|(i, l)| {
            l.op != Op::Ctx || {
                let lo = i.saturating_sub(CONTEXT_LINES);
                let hi = (i + CONTEXT_LINES).min(changed.len() - 1);
                changed[lo..=hi].iter().any(|&c| c)
            }
        })
        .map(|(_, l)| l)
        .collect()
}

/// Cap at `MAX_DIFF_LINES`, replacing the tail with a count.
fn cap(mut d: Vec<DiffLine>) -> Vec<DiffLine> {
    if d.len() <= MAX_DIFF_LINES {
        return d;
    }
    let hidden = d.len() - MAX_DIFF_LINES;
    d.truncate(MAX_DIFF_LINES);
    d.push(DiffLine::new(
        Op::Trailer,
        format!("[+{hidden} more lines]"),
    ));
    d
}

/// A diff for the ask card, when the tool is one that changes a file.
/// `write`'s "before" is the file on disk — the one I/O in this module, kept
/// here so `hotl-tui` stays pure.
///
/// INVARIANT (unimplemented — see
/// specs/exec-plans/active/0020-remediation-surface.md RQ-2): every
/// `edit`/`write` ask carries a diff. `EngineEvent::Ask` does not yet carry
/// `tool`/`input` (`hotl-engine/src/lib.rs`, raised in `turn.rs`), so
/// `acp.rs` cannot call this on the ask path. The generator and the renderer
/// are both built and tested; adoption is populating those two fields at that
/// one engine call site.
pub fn for_tool(tool: &str, input: &Value) -> Option<Vec<DiffLine>> {
    let s = |key: &str| input.get(key).and_then(Value::as_str).unwrap_or("");
    match tool {
        "edit" => Some(unified(s("old_string"), s("new_string"))),
        "write" => {
            // The file may not exist yet — that is a creation, all additions.
            let path = input.get("path").and_then(Value::as_str)?;
            let before = std::fs::read_to_string(path).unwrap_or_default();
            Some(unified(&before, s("content")))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unified_marks_only_what_changed() {
        let d = unified("a\nb\nc\n", "a\nB\nc\n");
        let ops: Vec<_> = d.iter().map(|l| l.op).collect();
        assert_eq!(ops, vec![Op::Ctx, Op::Del, Op::Add, Op::Ctx]);
        assert_eq!(d[1].text, "b");
        assert_eq!(d[2].text, "B");
    }

    #[test]
    fn a_new_file_is_all_additions() {
        let d = unified("", "x\ny\n");
        assert!(d.iter().all(|l| l.op == Op::Add));
        assert_eq!(d.len(), 2);
    }

    #[test]
    fn long_diffs_are_capped_with_a_trailer() {
        let old: String = (0..500).map(|i| format!("{i}\n")).collect();
        let new: String = (0..500).map(|i| format!("x{i}\n")).collect();
        let d = unified(&old, &new);
        assert!(d.len() <= MAX_DIFF_LINES + 1, "{} rows", d.len());
        assert_eq!(d.last().unwrap().op, Op::Trailer);
        assert!(d.last().unwrap().text.contains("more"));
    }

    #[test]
    fn far_context_is_trimmed_but_near_context_is_kept() {
        let old: String = (0..30).map(|i| format!("line{i}\n")).collect();
        let new = old.replace("line15", "CHANGED");
        let d = unified(&old, &new);
        let texts: Vec<&str> = d.iter().map(|l| l.text.as_str()).collect();
        assert!(texts.contains(&"line12"), "3 lines of context: {texts:?}");
        assert!(!texts.contains(&"line11"), "further context is cut");
        assert!(texts.contains(&"line18"));
        assert!(!texts.contains(&"line19"));
    }

    #[test]
    fn an_unchanged_file_yields_no_rows() {
        assert!(unified("a\nb\n", "a\nb\n").is_empty());
    }

    #[test]
    fn for_tool_handles_edit_write_and_ignores_the_rest() {
        let edit = for_tool(
            "edit",
            &json!({"path": "x.rs", "old_string": "a\n", "new_string": "b\n"}),
        );
        assert!(edit.is_some());
        assert!(for_tool("bash", &json!({"command": "ls"})).is_none());
        assert!(for_tool("read", &json!({"path": "x"})).is_none());
    }

    #[test]
    fn write_diffs_against_the_file_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "old\n").unwrap();
        let d = for_tool(
            "write",
            &json!({"path": path.to_str().unwrap(), "content": "new\n"}),
        )
        .expect("write produces a diff");
        assert_eq!(d[0], DiffLine::new(Op::Del, "old"));
        assert_eq!(d[1], DiffLine::new(Op::Add, "new"));
    }

    #[test]
    fn a_huge_file_falls_back_to_a_whole_file_replace() {
        let old: String = (0..LCS_MAX_LINES + 1).map(|i| format!("{i}\n")).collect();
        let d = unified(&old, "one line\n");
        assert!(!d.is_empty());
        assert_eq!(d.last().unwrap().op, Op::Trailer);
    }

    #[test]
    fn the_wire_shape_is_op_and_text() {
        let j = DiffLine::new(Op::Add, "x").to_json();
        assert_eq!(j["op"], "add");
        assert_eq!(j["text"], "x");
    }
}
