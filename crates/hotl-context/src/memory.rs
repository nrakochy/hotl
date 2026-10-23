//! The per-project memory file (0069): `<data>/memory/<project>/MEMORY.md`,
//! loaded under its own cap and the untrusted envelope like the per-user
//! file, and appended to one dated line at a time after the owner's yes.
//! Pure over a `&Path`: the project id and the data dir are passed in.

use std::path::{Path, PathBuf};

use hotl_platform::PrivateFs as _;
use hotl_types::{Item, SyntheticReason};

pub const MEMORY_FILE: &str = "MEMORY.md";

/// Hard cap on what a memory file may contribute to the context. Two files
/// (per user, per project) each get their own.
pub const MEMORY_BUDGET_BYTES: usize = 16 * 1024;

const NOTES_HEADING: &str = "## Notes";

/// The project file as a context item, or `None` when absent or blank. Same
/// clip and envelope as `load_memory`, with the project in the source.
pub fn load_project_memory(dir: &Path, project: &str) -> Option<(Item, Option<String>)> {
    let content = std::fs::read_to_string(dir.join(MEMORY_FILE)).ok()?;
    if content.trim().is_empty() {
        return None;
    }
    let source = format!("memory/{project}/{MEMORY_FILE}");
    let (text, notice) = crate::envelope_capped(&source, &content, MEMORY_BUDGET_BYTES);
    Some((
        Item::User {
            text,
            synthetic: Some(SyntheticReason::Memory),
            images: Vec::new(),
        },
        notice,
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryEntry {
    pub date: String,
    pub note: String,
}

/// The `- YYYY-MM-DD: note` bullets, in file order. Anything else — the
/// header, a hand-written paragraph, a bullet without a date — is skipped,
/// never an error: the owner may edit the file by hand.
pub fn entries(dir: &Path) -> Vec<MemoryEntry> {
    let Ok(text) = std::fs::read_to_string(dir.join(MEMORY_FILE)) else {
        return Vec::new();
    };
    text.lines().filter_map(parse_bullet).collect()
}

fn parse_bullet(line: &str) -> Option<MemoryEntry> {
    let rest = line.strip_prefix("- ")?;
    let (date, note) = rest.split_once(": ")?;
    let is_date = date.len() == 10
        && date.chars().enumerate().all(|(i, c)| {
            if i == 4 || i == 7 {
                c == '-'
            } else {
                c.is_ascii_digit()
            }
        });
    is_date.then(|| MemoryEntry {
        date: date.to_string(),
        note: note.to_string(),
    })
}

/// The last `n` entries.
pub fn tail(entries: &[MemoryEntry], n: usize) -> &[MemoryEntry] {
    &entries[entries.len().saturating_sub(n)..]
}

#[derive(Debug)]
pub struct Appended {
    pub path: PathBuf,
    /// The file's size after the write.
    pub bytes: usize,
}

#[derive(Debug)]
pub enum AppendError {
    /// The file plus the new line would pass the cap; nothing was written.
    OverBudget {
        have: usize,
        cap: usize,
    },
    Io(std::io::Error),
}

impl std::fmt::Display for AppendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OverBudget { have, cap } => write!(
                f,
                "the memory file is {have} bytes against a {cap}-byte cap; prune it by hand \
                 or move old lines to another file"
            ),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl From<std::io::Error> for AppendError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// Append `- <date>: <note>` under `## Notes`. A missing file is created with
/// its header; a hand-edited file that lost the heading gets it back. The cap
/// refuses rather than rolls: a clipped file would lose its newest lines
/// (0069 decision 3). Whole-file temp-then-rename, owner-only.
pub fn append(dir: &Path, project: &str, note: &str, date: &str) -> Result<Appended, AppendError> {
    let path = dir.join(MEMORY_FILE);
    let mut text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            format!("# Memory — {project}\n\n{NOTES_HEADING}\n")
        }
        Err(e) => return Err(e.into()),
    };
    if !text.lines().any(|l| l.trim_end() == NOTES_HEADING) {
        if !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&format!("\n{NOTES_HEADING}\n"));
    }
    if !text.ends_with('\n') {
        text.push('\n');
    }
    let line = format!("- {date}: {note}\n");
    if text.len() + line.len() > MEMORY_BUDGET_BYTES {
        return Err(AppendError::OverBudget {
            have: text.len(),
            cap: MEMORY_BUDGET_BYTES,
        });
    }
    text.push_str(&line);
    std::fs::create_dir_all(dir)?;
    atomic_write(&path, text.as_bytes())?;
    Ok(Appended {
        path,
        bytes: text.len(),
    })
}

/// Twin of `plan_state::atomic_write` in hotl-engine (which cannot be seen
/// from here): temp file beside the target, owner-only, then rename.
fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    {
        use std::io::Write;
        let mut f = hotl_platform::PRIVATE_FS.create_file_truncate(&tmp)?;
        f.write_all(bytes)?;
        f.flush()?;
    }
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_creates_header_and_notes_section_on_first_write() {
        let dir = tempfile::tempdir().unwrap();
        let mem = dir.path().join("memory/abc");
        let a = append(&mem, "abc", "tests need nextest", "2026-10-22").unwrap();
        assert_eq!(a.path, mem.join("MEMORY.md"));
        let text = std::fs::read_to_string(&a.path).unwrap();
        assert_eq!(
            text,
            "# Memory — abc\n\n## Notes\n- 2026-10-22: tests need nextest\n"
        );
        assert_eq!(a.bytes, text.len());
        assert!(!mem.join("MEMORY.tmp").exists());
    }

    #[test]
    fn append_adds_one_dated_bullet_and_keeps_earlier_lines_byte_identical() {
        let dir = tempfile::tempdir().unwrap();
        let mem = dir.path();
        let before =
            "# Memory — abc\n\nA paragraph the owner wrote.\n\n## Notes\n- 2026-10-01: first\n";
        std::fs::write(mem.join("MEMORY.md"), before).unwrap();
        append(mem, "abc", "second", "2026-10-22").unwrap();
        let after = std::fs::read_to_string(mem.join("MEMORY.md")).unwrap();
        assert_eq!(after, format!("{before}- 2026-10-22: second\n"));
        // A hand-edited file that lost the heading gets it back, once.
        std::fs::write(mem.join("MEMORY.md"), "just a line").unwrap();
        append(mem, "abc", "x", "2026-10-22").unwrap();
        append(mem, "abc", "y", "2026-10-23").unwrap();
        assert_eq!(
            std::fs::read_to_string(mem.join("MEMORY.md")).unwrap(),
            "just a line\n\n## Notes\n- 2026-10-22: x\n- 2026-10-23: y\n"
        );
    }

    #[test]
    fn append_refuses_past_the_budget_and_leaves_the_file_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let mem = dir.path();
        let mut big = String::from("## Notes\n");
        while big.len() < MEMORY_BUDGET_BYTES - 10 {
            big.push_str("- 2026-01-01: filler line of some length\n");
        }
        std::fs::write(mem.join("MEMORY.md"), &big).unwrap();
        let err = append(mem, "abc", "one more", "2026-10-22").unwrap_err();
        match err {
            AppendError::OverBudget { have, cap } => {
                assert_eq!(have, big.len());
                assert_eq!(cap, MEMORY_BUDGET_BYTES);
            }
            other => panic!("{other:?}"),
        }
        assert!(err.to_string().contains("prune it by hand"));
        assert_eq!(std::fs::read_to_string(mem.join("MEMORY.md")).unwrap(), big);
        assert!(!mem.join("MEMORY.tmp").exists());
    }

    #[test]
    fn entries_skip_hand_edited_lines_that_are_not_bullets() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("MEMORY.md"),
            "# Memory\n\nprose\n- not dated: nope\n- 2026-10-22: kept\n- 2026-1-2: bad date\n\
             ## Notes\n- 2026-10-23: also kept: with a colon\n",
        )
        .unwrap();
        let e = entries(dir.path());
        assert_eq!(
            e,
            vec![
                MemoryEntry {
                    date: "2026-10-22".into(),
                    note: "kept".into()
                },
                MemoryEntry {
                    date: "2026-10-23".into(),
                    note: "also kept: with a colon".into()
                },
            ]
        );
        assert!(entries(&dir.path().join("missing")).is_empty());
    }

    #[test]
    fn project_memory_loads_capped_and_enveloped_with_the_project_source() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_project_memory(dir.path(), "abc").is_none());
        std::fs::write(dir.path().join("MEMORY.md"), "  \n").unwrap();
        assert!(
            load_project_memory(dir.path(), "abc").is_none(),
            "blank is absent"
        );
        std::fs::write(
            dir.path().join("MEMORY.md"),
            "x".repeat(MEMORY_BUDGET_BYTES * 2),
        )
        .unwrap();
        let (item, notice) = load_project_memory(dir.path(), "abc").unwrap();
        let Item::User {
            text, synthetic, ..
        } = item
        else {
            panic!("a user item");
        };
        assert_eq!(synthetic, Some(SyntheticReason::Memory));
        assert!(text.contains("memory/abc/MEMORY.md"), "{}", &text[..200]);
        assert!(text.contains("trust=\"untrusted\""), "{}", &text[..200]);
        assert!(text.len() < MEMORY_BUDGET_BYTES + 1024, "{}", text.len());
        assert!(notice.is_some(), "the clip is reported");
    }

    #[test]
    fn tail_is_the_last_n() {
        let e: Vec<MemoryEntry> = (0..5)
            .map(|i| MemoryEntry {
                date: format!("2026-10-0{i}"),
                note: i.to_string(),
            })
            .collect();
        assert_eq!(tail(&e, 2), &e[3..]);
        assert_eq!(tail(&e, 10), &e[..]);
        assert!(tail(&e, 0).is_empty());
    }
}
