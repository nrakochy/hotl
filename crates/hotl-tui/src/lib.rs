//! Elm core for `hotl tui`: everything pure lives here — state, update, view,
//! the modal vim editor, the loop-motif animation frames, and the ACP client
//! codec. All I/O stays in the `hotl` binary's runtime (`src/tui.rs`); effects
//! leave `update` only as [`app::Cmd`] data. The TUI talks ONLY the ACP wire —
//! never the engine directly.

pub mod anim;
pub mod app;
pub mod client;
pub mod view;
pub mod vim;
