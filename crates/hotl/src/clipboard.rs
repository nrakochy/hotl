//! Put a copied selection on the system clipboard.
//!
//! One transport is not enough. hotl used to write only the OSC 52 escape, and
//! it is silently dropped in the two setups a terminal app meets most:
//!   - tmux forwards an application's OSC 52 only with `set-clipboard on`; the
//!     shipped default (`external`/`off`) swallows it, and the DCS passthrough
//!     that would route around that is itself off by default.
//!   - some terminals never implemented OSC 52 at all (Apple Terminal), so a
//!     bare write reaches nothing.
//!
//! So a copy fans out to every sink that could apply and lands if any does.
//! Each writes the same bytes, so overlap is harmless, and each is best-effort:
//! there is no reply to read, and a copy that reached one sink must never be
//! reported as failed because another was absent.
//!
//! No new dependency: OSC 52 is an escape sequence, and every other sink is a
//! program the platform already ships — reached the way `$EDITOR` and the
//! `afplay` chime are, not linked in.

use std::io::Write;
use std::process::{Command, Stdio};

/// The OSC 52 clipboard write: `ESC ] 52 ; c ; <base64> BEL`. Base64 keeps
/// newlines and control bytes in the copied text from ending the sequence
/// early. This is the one sink that crosses SSH to a bare terminal, so it is
/// always attempted even when a local program will also be run.
pub fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", hotl_tools::b64::encode(text.as_bytes()))
}

/// `tmux load-buffer -w -`: read stdin into a paste buffer and (`-w`) forward
/// it to the *outer* terminal's clipboard. Being a tmux command rather than an
/// in-band escape, it is unaffected by `set-clipboard` and `allow-passthrough`
/// — the very settings that gate a bare OSC 52 — and it reaches the real
/// terminal even when hotl is an SSH hop running inside the tmux. `None` when
/// not under tmux.
fn tmux_argv(in_tmux: bool) -> Option<&'static [&'static str]> {
    in_tmux.then_some(&["tmux", "load-buffer", "-w", "-"])
}

/// Native clipboard programs for the current OS, all tried best-effort: the one
/// that fits the box lands the copy, the rest fail cleanly (not installed, or
/// installed with no display server to talk to). Trying them all rather than
/// stopping at the first that *spawns* is deliberate — a `wl-copy` present on
/// an X11 box spawns fine and does nothing, and must not shadow `xclip`. This
/// is the sink that covers a local terminal which never spoke OSC 52.
fn native_candidates() -> &'static [&'static [&'static str]] {
    #[cfg(target_os = "macos")]
    {
        &[&["pbcopy"]]
    }
    #[cfg(target_os = "windows")]
    {
        &[&["clip"]]
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        &[
            &["wl-copy"],
            &["xclip", "-selection", "clipboard"],
            &["xsel", "--clipboard", "--input"],
        ]
    }
}

/// Put `text` on the clipboard by every route that applies. The OSC 52 escape
/// goes to `out` — the terminal hotl already draws to — then the tmux and
/// native-program sinks run as detached children. Best-effort throughout:
/// nothing here fails in a way the caller should act on.
pub fn copy(out: &mut impl Write, text: &str) {
    emit_osc52(out, text);
    spill_to_programs(text);
}

/// The in-band half: the escape, flushed so it lands with the frame already
/// drawn rather than whenever the next write happens to flush.
fn emit_osc52(out: &mut impl Write, text: &str) {
    let _ = write!(out, "{}", osc52(text));
    let _ = out.flush();
}

/// The out-of-band half: hand the text to tmux and to every native program.
fn spill_to_programs(text: &str) {
    if let Some(argv) = tmux_argv(std::env::var_os("TMUX").is_some()) {
        spawn_write(argv, text);
    }
    for argv in native_candidates() {
        spawn_write(argv, text);
    }
}

/// Spawn `argv`, feed `text` to its stdin, and reap it off the run loop. Silent
/// on every failure: a missing program, a closed pipe, a nonzero exit are all
/// just this route not landing, which the other sinks are there to cover.
fn spawn_write(argv: &[&str], text: &str) {
    let Ok(mut child) = Command::new(argv[0])
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return;
    };
    // The `ChildStdin` drops at the end of this block, closing the pipe so the
    // program sees EOF and exits. Screen-bounded text never fills the pipe, so
    // the write cannot block.
    if let Some(mut sink) = child.stdin.take() {
        let _ = sink.write_all(text.as_bytes());
    }
    // Reap on a detached thread so a slow program never stalls the UI and an
    // unreaped child never lingers as a zombie — the `afplay` pattern.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_clipboard_escape_is_a_well_formed_osc_52() {
        // `ESC ] 52 ; c ; <base64> BEL` — `c` is the clipboard selection.
        assert_eq!(osc52("hi"), "\x1b]52;c;aGk=\x07");
        // Newlines are payload, not terminators: a multi-line copy must not
        // truncate at the first one.
        assert_eq!(osc52("a\nb"), "\x1b]52;c;YQpi\x07");
    }

    #[test]
    fn emit_osc52_writes_the_escape_to_the_given_writer() {
        // The in-band sink is tested against a buffer so it drives no
        // subprocess and touches no real clipboard.
        let mut buf: Vec<u8> = Vec::new();
        emit_osc52(&mut buf, "hi");
        assert_eq!(buf, b"\x1b]52;c;aGk=\x07");
    }

    #[test]
    fn the_tmux_sink_is_present_exactly_under_tmux() {
        // Off tmux there is nothing to route around; under it we always add the
        // `-w` forward, which is what makes the copy survive `set-clipboard
        // external` and `allow-passthrough off`.
        assert_eq!(tmux_argv(false), None);
        assert_eq!(
            tmux_argv(true),
            Some(&["tmux", "load-buffer", "-w", "-"][..])
        );
    }

    #[test]
    fn a_native_clipboard_program_exists_for_this_os() {
        // A bare OSC 52 is not enough on a terminal that never implemented it,
        // so every OS must offer at least one native fallback.
        let candidates = native_candidates();
        assert!(!candidates.is_empty());
        assert!(candidates.iter().all(|argv| !argv.is_empty()));
        #[cfg(target_os = "macos")]
        assert_eq!(candidates[0], &["pbcopy"]);
    }
}
