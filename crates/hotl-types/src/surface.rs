use crate::observation::{AgentObservation, Source};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceError(pub String);

impl fmt::Display for SurfaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for SurfaceError {}

pub trait Surface {
    fn observe(&self) -> Result<Vec<AgentObservation>, SurfaceError>;
    fn focus(&self, obs: &AgentObservation) -> Result<(), SurfaceError>;
    fn source(&self) -> Source;
}
