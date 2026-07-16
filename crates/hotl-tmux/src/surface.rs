use crate::panes::{list_panes, run_jump, Pane};
use crate::procs::{agent_for, read_proc_table, ProcTable};
use crate::status::classify;
use types::{AgentObservation, Location, LocationHandle, Source, Surface, SurfaceError};

pub struct TmuxSurface {
    agents: Vec<String>,
}

impl TmuxSurface {
    pub fn new(agents: &[String]) -> Self {
        TmuxSurface { agents: agents.to_vec() }
    }
}

/// The agent's own status line: the first captured line containing `ctx:`
/// (claude's context-usage token), trimmed. `None` if no such line.
pub fn status_line(tail: &str) -> Option<String> {
    tail.lines()
        .find(|l| l.contains("ctx:"))
        .map(|l| l.trim().to_string())
}

// Pure mapping, kept separate from Surface::observe so it's testable without tmux I/O.
pub fn observations(
    panes: &[Pane],
    procs: &ProcTable,
    agent_names: &[String],
    tail_for: &dyn Fn(&str) -> String,
) -> Vec<AgentObservation> {
    let mut out = Vec::new();
    for pane in panes {
        let Some(agent) = agent_for(pane.pane_pid, procs, agent_names) else {
            continue;
        };
        let tail = tail_for(&pane.pane_id);
        let status = classify(&agent.name, &pane.title, &tail);
        out.push(AgentObservation {
            agent,
            cwd: pane.current_path.clone(),
            status,
            status_line: status_line(&tail),
            location: Location {
                group: pane.session.clone(),
                sub_group: Some(format!("{} ({})", pane.window_name, pane.window_index)),
                handle: LocationHandle::Tmux {
                    pane_id: pane.pane_id.clone(),
                    session: pane.session.clone(),
                    window_index: pane.window_index,
                },
            },
            source: Source::Tmux,
        });
    }
    out
}

impl Surface for TmuxSurface {
    fn observe(&self) -> Result<Vec<AgentObservation>, SurfaceError> {
        let panes = list_panes().map_err(|e| SurfaceError(e.to_string()))?;
        let procs = read_proc_table().map_err(|e| SurfaceError(e.to_string()))?;
        let tail_for = |pane_id: &str| -> String {
            crate::panes::capture_pane(pane_id, 15).unwrap_or_default()
        };
        Ok(observations(&panes, &procs, &self.agents, &tail_for))
    }

    fn focus(&self, obs: &AgentObservation) -> Result<(), SurfaceError> {
        match &obs.location.handle {
            LocationHandle::Tmux { pane_id, session, window_index } => {
                let pane = Pane {
                    session: session.clone(),
                    window_index: *window_index,
                    window_name: String::new(),
                    pane_index: 0,
                    pane_id: pane_id.clone(),
                    pane_pid: 0,
                    current_command: String::new(),
                    current_path: String::new(),
                    pane_active: false,
                    window_active: false,
                    title: String::new(),
                };
                run_jump(&pane).map_err(|e| SurfaceError(e.to_string()))
            }
        }
    }

    fn source(&self) -> Source {
        Source::Tmux
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panes::parse_panes;
    use crate::procs::parse_ps;
    use types::Status;

    const PANES: &str = "\
base-0\u{1f}0\u{1f}zsh\u{1f}0\u{1f}%25\u{1f}35580\u{1f}claude\u{1f}/tmp/a\u{1f}0\u{1f}1\u{1f}✳ task a
work\u{1f}0\u{1f}edit\u{1f}0\u{1f}%60\u{1f}40000\u{1f}claude\u{1f}/tmp/d\u{1f}1\u{1f}1\u{1f}\u{2809} task d";
    const PS: &str = "  PID  PPID COMMAND\n35580 1 claude\n40000 1 claude";

    #[test]
    fn maps_panes_to_observations() {
        let panes = parse_panes(PANES);
        let procs = parse_ps(PS);
        let names = vec!["claude".to_string()];
        let tail_for = |id: &str| -> String {
            if id == "%25" { "❯\n  [I] .../tmp/a [main] ctx:9%".to_string() } else { String::new() }
        };
        let obs = observations(&panes, &procs, &names, &tail_for);
        assert_eq!(obs.len(), 2);

        let a = obs.iter().find(|o| o.cwd == "/tmp/a").unwrap();
        assert_eq!(a.location.group, "base-0");
        assert_eq!(a.location.sub_group.as_deref(), Some("zsh (0)"));
        assert_eq!(a.status, Status::Idle);

        let d = obs.iter().find(|o| o.cwd == "/tmp/d").unwrap();
        assert_eq!(d.status, Status::Working);
        match &d.location.handle {
            LocationHandle::Tmux { pane_id, .. } => assert_eq!(pane_id, "%60"),
        }

        assert_eq!(a.status_line.as_deref(), Some("[I] .../tmp/a [main] ctx:9%"));
        assert_eq!(d.status_line, None);
    }

    #[test]
    fn status_line_extracts_ctx_line() {
        let tail = "\
some output
❯ ◯ main
  [I] .../sources/hotl [master] Opus 4.8 (1M context) ctx:15%
  -- INSERT -- ↑/↓ to select
  ⏺ general-purpose  Review crate    21m · ↓ 56k tokens";
        assert_eq!(
            status_line(tail).as_deref(),
            Some("[I] .../sources/hotl [master] Opus 4.8 (1M context) ctx:15%"),
        );
    }

    #[test]
    fn status_line_none_when_no_ctx() {
        assert_eq!(status_line("just some log output\n❯ "), None);
        assert_eq!(status_line(""), None);
    }

    #[test]
    fn panes_without_agents_are_dropped() {
        let panes = parse_panes(
            "s\u{1f}0\u{1f}w\u{1f}0\u{1f}%9\u{1f}999\u{1f}zsh\u{1f}/tmp\u{1f}0\u{1f}1\u{1f}title",
        );
        let procs = parse_ps("  PID  PPID COMMAND\n999 1 -zsh");
        let obs = observations(&panes, &procs, &["claude".to_string()], &|_| String::new());
        assert!(obs.is_empty());
    }
}
