pub mod nav;
pub mod panes;
pub mod procs;
pub mod status;
pub mod surface;

pub use nav::{select_pane, select_pane_argv};
pub use panes::{
    capture_pane, jump_argv, list_panes, parse_panes, run_jump, tmux_available, Pane, PANE_FORMAT,
};
pub use procs::{agent_for, parse_ps, read_proc_table, ProcTable};
pub use status::{classify, detector_for, extract_status_line, ClaudeDetector, GenericDetector, OpenCodeDetector, PiDetector, Signals, StatusDetector};
pub use surface::{observations, TmuxSurface};
