use std::collections::HashMap;

#[derive(Debug, Default)]
pub struct ProcTable {
    pub cmd: HashMap<u32, String>,
    pub children: HashMap<u32, Vec<u32>>,
}

pub fn parse_ps(raw: &str) -> ProcTable {
    let mut table = ProcTable::default();
    for line in raw.lines().skip(1) {
        let line = line.trim_start();
        let Some((pid, rest)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let rest = rest.trim_start();
        let Some((ppid, cmd)) = rest.split_once(char::is_whitespace) else {
            continue;
        };
        let cmd = cmd.trim_start();
        let (Ok(pid), Ok(ppid)) = (pid.parse::<u32>(), ppid.parse::<u32>()) else {
            continue;
        };
        table.cmd.insert(pid, cmd.to_string());
        table.children.entry(ppid).or_default().push(pid);
    }
    table
}

fn matched_agent_name(command: &str, agent_names: &[String]) -> Option<String> {
    let first = command.split_whitespace().next()?;
    let base = first.trim_start_matches('-').rsplit('/').next()?;
    agent_names.iter().find(|n| n.as_str() == base).cloned()
}

pub fn agent_for(pane_pid: u32, procs: &ProcTable, agent_names: &[String]) -> Option<types::Agent> {
    let mut stack = vec![pane_pid];
    while let Some(pid) = stack.pop() {
        if let Some(cmd) = procs.cmd.get(&pid) {
            if let Some(name) = matched_agent_name(cmd, agent_names) {
                return Some(types::Agent { name, pid, argv: cmd.clone() });
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
    let out = Command::new("ps").args(["-axo", "pid,ppid,command"]).output()?;
    if !out.status.success() {
        return Err(io::Error::other("ps failed"));
    }
    Ok(parse_ps(&String::from_utf8_lossy(&out.stdout)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PS: &str = "\
  PID  PPID COMMAND
 3244     1 /usr/bin/login
30578  3244 -zsh
70060 30578 claude --permission-mode bypassPermissions
80031  3244 -zsh";

    fn names() -> Vec<String> {
        vec!["claude".to_string(), "codex".to_string()]
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
        let t = parse_ps("  PID  PPID COMMAND\n999     1 claude");
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
}
