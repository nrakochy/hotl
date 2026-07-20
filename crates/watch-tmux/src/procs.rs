use std::collections::{HashMap, HashSet};

#[derive(Debug, Default)]
pub struct ProcTable {
    pub cmd: HashMap<u32, String>,
    pub children: HashMap<u32, Vec<u32>>,
    /// Pids in job-control stopped state (`T…`) — e.g. an agent the user ctrl-z'd.
    pub stopped: HashSet<u32>,
}

pub fn parse_ps(raw: &str) -> ProcTable {
    let mut table = ProcTable::default();
    for line in raw.lines().skip(1) {
        let line = line.trim_start();
        let Some((pid, rest)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let rest = rest.trim_start();
        let Some((ppid, rest)) = rest.split_once(char::is_whitespace) else {
            continue;
        };
        let rest = rest.trim_start();
        let Some((state, cmd)) = rest.split_once(char::is_whitespace) else {
            continue;
        };
        let cmd = cmd.trim_start();
        let (Ok(pid), Ok(ppid)) = (pid.parse::<u32>(), ppid.parse::<u32>()) else {
            continue;
        };
        table.cmd.insert(pid, cmd.to_string());
        table.children.entry(ppid).or_default().push(pid);
        if state.starts_with('T') {
            table.stopped.insert(pid);
        }
    }
    table
}

/// `hotl` invocations that are not an interactive agent surface: the dashboard
/// itself, the future orchestrator, maintenance commands, and the detached
/// session host (whose asks park until `hotl attach` — which *is* captured).
const HOTL_NON_AGENT_SUBCOMMANDS: &[&str] = &["watch", "fleet", "gc", "doctor", "serve"];

fn matched_agent_name(command: &str, agent_names: &[String]) -> Option<String> {
    let mut words = command.split_whitespace();
    let first = words.next()?;
    let base = first.trim_start_matches('-').rsplit('/').next()?;
    let name = agent_names.iter().find(|n| n.as_str() == base)?;
    if base == "hotl" {
        if let Some(sub) = words.next() {
            if HOTL_NON_AGENT_SUBCOMMANDS.contains(&sub) {
                return None;
            }
        }
    }
    Some(name.clone())
}

pub fn agent_for(pane_pid: u32, procs: &ProcTable, agent_names: &[String]) -> Option<watch_types::Agent> {
    let mut stack = vec![pane_pid];
    while let Some(pid) = stack.pop() {
        if let Some(cmd) = procs.cmd.get(&pid) {
            if let Some(name) = matched_agent_name(cmd, agent_names) {
                return Some(watch_types::Agent { name, pid, argv: cmd.clone() });
            }
        }
        if let Some(kids) = procs.children.get(&pid) {
            stack.extend(kids.iter().copied());
        }
    }
    None
}

use std::io;
use std::process::Command;

pub fn read_proc_table() -> io::Result<ProcTable> {
    let out = Command::new("ps").args(["-axo", "pid,ppid,state,command"]).output()?;
    if !out.status.success() {
        return Err(io::Error::other("ps failed"));
    }
    Ok(parse_ps(&String::from_utf8_lossy(&out.stdout)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PS: &str = "\
  PID  PPID STAT COMMAND
 3244     1 Ss   /usr/bin/login
30578  3244 S    -zsh
70060 30578 S+   claude --permission-mode bypassPermissions
80031  3244 S    -zsh";

    fn names() -> Vec<String> {
        vec!["claude".to_string(), "codex".to_string(), "hotl".to_string()]
    }

    #[test]
    fn parses_ps_rows() {
        let t = parse_ps(PS);
        assert_eq!(t.cmd.get(&70060).unwrap(), "claude --permission-mode bypassPermissions");
        assert_eq!(t.children.get(&30578).unwrap(), &vec![70060]);
        assert_eq!(t.children.get(&3244).unwrap().len(), 2);
    }

    #[test]
    fn finds_agent_descendant_of_pane() {
        let t = parse_ps(PS);
        let a = agent_for(30578, &t, &names()).expect("agent found");
        assert_eq!(a.name, "claude");
        assert_eq!(a.pid, 70060);
        assert_eq!(a.argv, "claude --permission-mode bypassPermissions");
    }

    #[test]
    fn shell_only_pane_has_no_agent() {
        let t = parse_ps(PS);
        assert!(agent_for(80031, &t, &names()).is_none());
    }

    #[test]
    fn matches_agent_when_pane_pid_is_the_agent_itself() {
        let t = parse_ps("  PID  PPID STAT COMMAND\n999     1 S+   claude");
        let a = agent_for(999, &t, &names()).expect("agent found");
        assert_eq!(a.pid, 999);
    }

    #[test]
    fn matched_agent_name_handles_path_and_login_shell() {
        let n = names();
        assert_eq!(matched_agent_name("/opt/homebrew/bin/claude --foo", &n).as_deref(), Some("claude"));
        assert_eq!(matched_agent_name("-zsh", &n), None);
        assert_eq!(matched_agent_name("node server.js", &n), None);
    }

    #[test]
    fn bare_hotl_is_an_agent() {
        let n = names();
        assert_eq!(matched_agent_name("hotl", &n).as_deref(), Some("hotl"));
        assert_eq!(matched_agent_name("/usr/local/bin/hotl", &n).as_deref(), Some("hotl"));
        assert_eq!(matched_agent_name("hotl resume abc123", &n).as_deref(), Some("hotl"));
        assert_eq!(matched_agent_name("hotl attach bg-12345", &n).as_deref(), Some("hotl"));
        // Flags before a prompt are still the agent (`hotl -p "…"`).
        assert_eq!(matched_agent_name("hotl -p do the thing", &n).as_deref(), Some("hotl"));
    }

    #[test]
    fn hotl_non_agent_subcommands_are_skipped() {
        let n = names();
        // The dashboard must never discover itself.
        assert_eq!(matched_agent_name("hotl watch", &n), None);
        assert_eq!(matched_agent_name("/opt/homebrew/bin/hotl watch", &n), None);
        assert_eq!(matched_agent_name("hotl fleet", &n), None);
        assert_eq!(matched_agent_name("hotl gc --dry-run", &n), None);
        assert_eq!(matched_agent_name("hotl doctor", &n), None);
        assert_eq!(matched_agent_name("hotl serve --id bg-1", &n), None);
    }

    #[test]
    fn subcommand_filter_only_applies_to_hotl() {
        // Another agent binary with a "watch" argument still matches.
        let n = names();
        assert_eq!(matched_agent_name("claude watch", &n).as_deref(), Some("claude"));
    }

    #[test]
    fn stopped_state_is_recorded() {
        let t = parse_ps("  PID  PPID STAT COMMAND\n999 1 T    hotl\n998 1 S+   hotl");
        assert!(t.stopped.contains(&999));
        assert!(!t.stopped.contains(&998));
    }
}
