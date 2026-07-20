use std::io;
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pane {
    pub session: String,
    pub window_index: u32,
    pub window_name: String,
    pub pane_index: u32,
    pub pane_id: String,
    pub pane_pid: u32,
    pub current_command: String,
    pub current_path: String,
    pub pane_active: bool,
    pub window_active: bool,
    pub title: String,
}

// Delimited by U+001F (not '|') so paths/titles with ordinary chars aren't split wrong.
pub const PANE_FORMAT: &str = "#{session_name}\u{1f}#{window_index}\u{1f}#{window_name}\u{1f}#{pane_index}\u{1f}#{pane_id}\u{1f}#{pane_pid}\u{1f}#{pane_current_command}\u{1f}#{pane_current_path}\u{1f}#{pane_active}\u{1f}#{window_active}\u{1f}#{pane_title}";

// Malformed lines (wrong field count / bad numbers) are skipped, not fatal.
pub fn parse_panes(raw: &str) -> Vec<Pane> {
    raw.lines().filter_map(parse_line).collect()
}

fn parse_line(line: &str) -> Option<Pane> {
    let f: Vec<&str> = line.split('\u{1f}').collect();
    if f.len() != 11 {
        return None;
    }
    Some(Pane {
        session: f[0].to_string(),
        window_index: f[1].parse().ok()?,
        window_name: f[2].to_string(),
        pane_index: f[3].parse().ok()?,
        pane_id: f[4].to_string(),
        pane_pid: f[5].parse().ok()?,
        current_command: f[6].to_string(),
        current_path: f[7].to_string(),
        pane_active: f[8] == "1",
        window_active: f[9] == "1",
        title: f[10].to_string(),
    })
}

// Uses tmux's `;` command separator, passed as a literal argument.
pub fn jump_argv(pane: &Pane) -> Vec<String> {
    let win_target = format!("{}:{}", pane.session, pane.window_index);
    vec![
        "switch-client".into(), "-t".into(), pane.session.clone(), ";".into(),
        "select-window".into(), "-t".into(), win_target, ";".into(),
        "select-pane".into(), "-t".into(), pane.pane_id.clone(),
    ]
}

pub fn list_panes() -> io::Result<Vec<Pane>> {
    let out = Command::new("tmux")
        .args(["list-panes", "-a", "-F", PANE_FORMAT])
        .output()?;
    if !out.status.success() {
        return Err(io::Error::other(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    Ok(parse_panes(&String::from_utf8_lossy(&out.stdout)))
}

pub fn run_jump(pane: &Pane) -> io::Result<()> {
    let status = Command::new("tmux").args(jump_argv(pane)).status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other("tmux jump failed"))
    }
}

pub fn tmux_available() -> bool {
    Command::new("tmux")
        .arg("-V")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn capture_pane(pane_id: &str, max_lines: usize) -> io::Result<String> {
    let out = Command::new("tmux")
        .args(["capture-pane", "-p", "-t", pane_id])
        .output()?;
    if !out.status.success() {
        return Err(io::Error::other("capture-pane failed"));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(max_lines);
    Ok(lines[start..].join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = "\
base-0\u{1f}0\u{1f}zsh\u{1f}0\u{1f}%25\u{1f}35580\u{1f}claude\u{1f}/Users/nrakochy/sources/lca-worktrees/nar-lattice-login-map-view-transition\u{1f}0\u{1f}1\u{1f}✳ task a
base-0\u{1f}0\u{1f}zsh\u{1f}1\u{1f}%51\u{1f}30578\u{1f}claude\u{1f}/Users/nrakochy/sources/lca\u{1f}0\u{1f}1\u{1f}✳ task b
base-0\u{1f}0\u{1f}zsh\u{1f}4\u{1f}%55\u{1f}80031\u{1f}zsh\u{1f}/Users/nrakochy/sources/lca-worktrees/nar-keycloak-theme-publish-ci\u{1f}1\u{1f}1\u{1f}zsh";

    #[test]
    fn parses_all_panes() {
        let panes = parse_panes(FIXTURE);
        assert_eq!(panes.len(), 3);
    }

    #[test]
    fn parses_fields_of_first_pane() {
        let p = &parse_panes(FIXTURE)[0];
        assert_eq!(p.session, "base-0");
        assert_eq!(p.window_index, 0);
        assert_eq!(p.window_name, "zsh");
        assert_eq!(p.pane_index, 0);
        assert_eq!(p.pane_id, "%25");
        assert_eq!(p.pane_pid, 35580);
        assert_eq!(p.current_command, "claude");
        assert!(!p.pane_active);
        assert!(p.window_active);
        assert_eq!(p.title, "✳ task a");
    }

    #[test]
    fn reads_active_flags() {
        let panes = parse_panes(FIXTURE);
        assert!(panes[2].pane_active);
    }

    #[test]
    fn skips_malformed_lines() {
        let raw = "too\u{1f}few\u{1f}fields\nbase-0\u{1f}0\u{1f}zsh\u{1f}0\u{1f}%25\u{1f}35580\u{1f}claude\u{1f}/tmp\u{1f}0\u{1f}1\u{1f}title";
        assert_eq!(parse_panes(raw).len(), 1);
    }

    #[test]
    fn field_containing_pipe_is_not_dropped() {
        let sep = '\u{1f}';
        let fields = [
            "base-0", "0", "zsh", "0", "%25", "35580", "claude",
            "/tmp/weird|dir", "0", "1", "title",
        ];
        let line = fields.join(&sep.to_string());
        let panes = parse_panes(&line);
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].current_path, "/tmp/weird|dir");
    }

    #[test]
    fn empty_input_yields_no_panes() {
        assert!(parse_panes("").is_empty());
    }

    #[test]
    fn builds_jump_argv() {
        let p = Pane {
            session: "base-0".into(),
            window_index: 3,
            window_name: "zsh".into(),
            pane_index: 2,
            pane_id: "%54".into(),
            pane_pid: 1,
            current_command: "claude".into(),
            current_path: "/tmp".into(),
            pane_active: false,
            window_active: false,
            title: String::new(),
        };
        assert_eq!(
            jump_argv(&p),
            vec![
                "switch-client", "-t", "base-0", ";",
                "select-window", "-t", "base-0:3", ";",
                "select-pane", "-t", "%54",
            ]
        );
    }
}
