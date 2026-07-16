use types::{AgentObservation, Surface, SurfaceError};

/// Result of polling every surface: the observations that succeeded, plus a
/// warning for each surface that failed. A single broken surface degrades the
/// list rather than blanking it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub observations: Vec<AgentObservation>,
    pub warnings: Vec<String>,
}

pub struct Listener {
    surfaces: Vec<Box<dyn Surface>>,
}

impl Listener {
    pub fn new(surfaces: Vec<Box<dyn Surface>>) -> Self {
        Listener { surfaces }
    }

    /// Poll all surfaces, keeping observations from the ones that succeed. Only
    /// errors if *every* surface failed (a total outage worth reporting as an
    /// error); otherwise partial failures ride along in `warnings`.
    pub fn snapshot(&self) -> Result<Snapshot, SurfaceError> {
        let mut snap = Snapshot::default();
        let mut failed = 0usize;
        for s in &self.surfaces {
            match s.observe() {
                Ok(obs) => snap.observations.extend(obs),
                Err(e) => {
                    failed += 1;
                    snap.warnings.push(e.to_string());
                }
            }
        }
        if failed > 0 && failed == self.surfaces.len() {
            return Err(SurfaceError(snap.warnings.join("; ")));
        }
        Ok(snap)
    }

    pub fn focus(&self, obs: &AgentObservation) -> Result<(), SurfaceError> {
        for s in &self.surfaces {
            if s.source() == obs.source {
                return s.focus(obs);
            }
        }
        Err(SurfaceError(format!("no surface for source {:?}", obs.source)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use types::{Agent, Location, LocationHandle, Source, Status};

    fn obs(cwd: &str) -> AgentObservation {
        AgentObservation {
            agent: Agent { name: "claude".into(), pid: 1, argv: "claude".into() },
            cwd: cwd.into(),
            status: Status::Idle,
            status_line: None,
            location: Location {
                group: "g".into(),
                sub_group: None,
                handle: LocationHandle::Tmux {
                    pane_id: "%1".into(),
                    session: "g".into(),
                    window_index: 0,
                },
            },
            source: Source::Tmux,
        }
    }

    struct FakeSurface {
        items: Vec<AgentObservation>,
        focused: RefCell<Vec<String>>,
    }
    impl Surface for FakeSurface {
        fn observe(&self) -> Result<Vec<AgentObservation>, SurfaceError> {
            Ok(self.items.clone())
        }
        fn focus(&self, o: &AgentObservation) -> Result<(), SurfaceError> {
            self.focused.borrow_mut().push(o.cwd.clone());
            Ok(())
        }
        fn source(&self) -> Source {
            Source::Tmux
        }
    }

    struct BadSurface(&'static str);
    impl Surface for BadSurface {
        fn observe(&self) -> Result<Vec<AgentObservation>, SurfaceError> {
            Err(SurfaceError(self.0.into()))
        }
        fn focus(&self, _: &AgentObservation) -> Result<(), SurfaceError> { Ok(()) }
        fn source(&self) -> Source { Source::Tmux }
    }

    #[test]
    fn snapshot_concatenates_all_surfaces() {
        let a = FakeSurface { items: vec![obs("a")], focused: RefCell::new(vec![]) };
        let b = FakeSurface { items: vec![obs("b"), obs("c")], focused: RefCell::new(vec![]) };
        let l = Listener::new(vec![Box::new(a), Box::new(b)]);
        let snap = l.snapshot().unwrap();
        assert_eq!(snap.observations.len(), 3);
        assert!(snap.warnings.is_empty());
    }

    #[test]
    fn snapshot_keeps_healthy_surfaces_when_one_fails() {
        let good = FakeSurface { items: vec![obs("a"), obs("b")], focused: RefCell::new(vec![]) };
        let l = Listener::new(vec![Box::new(good), Box::new(BadSurface("boom"))]);
        let snap = l.snapshot().expect("partial failure is not a hard error");
        assert_eq!(snap.observations.len(), 2, "healthy surface's agents survive");
        assert_eq!(snap.warnings.len(), 1);
        assert!(snap.warnings[0].contains("boom"));
    }

    #[test]
    fn focus_dispatches_to_matching_source() {
        let s = FakeSurface { items: vec![], focused: RefCell::new(vec![]) };
        let l = Listener::new(vec![Box::new(s)]);
        l.focus(&obs("target")).unwrap();
    }

    #[test]
    fn snapshot_errors_only_when_all_surfaces_fail() {
        let l = Listener::new(vec![Box::new(BadSurface("boom"))]);
        assert!(l.snapshot().is_err(), "sole surface failing is a total outage");

        let l2 = Listener::new(vec![Box::new(BadSurface("a")), Box::new(BadSurface("b"))]);
        assert!(l2.snapshot().is_err(), "every surface failing is a hard error");
    }
}
