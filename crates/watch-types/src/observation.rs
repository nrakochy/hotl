use crate::agent::{Agent, Status};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Tmux,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocationHandle {
    Tmux { pane_id: String, session: String, window_index: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub group: String,
    pub sub_group: Option<String>,
    pub handle: LocationHandle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentObservation {
    pub agent: Agent,
    pub cwd: String,
    pub status: Status,
    pub status_line: Option<String>,
    pub location: Location,
    pub source: Source,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Agent;

    fn obs() -> AgentObservation {
        AgentObservation {
            agent: Agent { name: "claude".into(), pid: 1, argv: "claude".into() },
            cwd: "/tmp/proj".into(),
            status: Status::Idle,
            status_line: None,
            location: Location {
                group: "base-0".into(),
                sub_group: Some("zsh (0)".into()),
                handle: LocationHandle::Tmux {
                    pane_id: "%1".into(),
                    session: "base-0".into(),
                    window_index: 0,
                },
            },
            source: Source::Tmux,
        }
    }

    #[test]
    fn observation_round_trips_fields() {
        let o = obs();
        assert_eq!(o.agent.name, "claude");
        assert_eq!(o.status, Status::Idle);
        assert_eq!(o.status_line, None);
        assert_eq!(o.location.group, "base-0");
        assert_eq!(o.source, Source::Tmux);
        match o.location.handle {
            LocationHandle::Tmux { pane_id, .. } => assert_eq!(pane_id, "%1"),
        }
    }
}
