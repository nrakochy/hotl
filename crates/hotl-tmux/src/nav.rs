use std::io;
use std::process::Command;
use types::Dir;

pub fn select_pane_argv(dir: Dir) -> Vec<String> {
    let flag = match dir {
        Dir::Up => "-U",
        Dir::Down => "-D",
        Dir::Left => "-L",
        Dir::Right => "-R",
    };
    vec!["select-pane".into(), flag.into()]
}

/// Move focus to the neighboring pane. A missing neighbor is a harmless no-op
/// (tmux exits non-zero but prints nothing), so that returns `Ok(None)`. A real
/// failure — e.g. no tmux server — writes to stderr; we surface that message as
/// `Ok(Some(msg))` so the caller can report it instead of silently swallowing.
pub fn select_pane(dir: Dir) -> io::Result<Option<String>> {
    let out = Command::new("tmux").args(select_pane_argv(dir)).output()?;
    if out.status.success() {
        return Ok(None);
    }
    let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
    // Empty stderr == "no pane in that direction": benign, not worth reporting.
    Ok((!msg.is_empty()).then_some(msg))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn left_argv() {
        assert_eq!(select_pane_argv(Dir::Left), vec!["select-pane", "-L"]);
    }

    #[test]
    fn right_argv() {
        assert_eq!(select_pane_argv(Dir::Right), vec!["select-pane", "-R"]);
    }

    #[test]
    fn up_down_argv() {
        assert_eq!(select_pane_argv(Dir::Up), vec!["select-pane", "-U"]);
        assert_eq!(select_pane_argv(Dir::Down), vec!["select-pane", "-D"]);
    }
}
