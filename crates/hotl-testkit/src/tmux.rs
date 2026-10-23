//! A thin tmux driver for terminal-level tests (0074 T3): a private tmux
//! server per session (`-L hotl-smoke-<pid>-<n>`, `-f /dev/null`), keys in,
//! `capture-pane` out, `wait_for` with a deadline, and a guard that kills the
//! server on drop — panic included. The developer's own tmux is never touched.
//! Argv comes from `watch_tmux::panes`; this is the one `Command` call site.

use std::ops::Range;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use watch_tmux::panes;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// What a scenario does with a missing tmux: `Absent` and `CannotStart` skip
/// (with a printed reason) unless `HOTL_TMUX_SMOKE=required` makes them fail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    Available,
    Absent,
    CannotStart(String),
}

/// How to start the pane's program.
#[derive(Debug, Clone)]
pub struct Launch {
    pub cols: u16,
    pub rows: u16,
    pub cwd: std::path::PathBuf,
    /// Set on the pane's program. `TMUX` is always removed, or tmux refuses
    /// to start a session from inside one.
    pub env: Vec<(String, String)>,
    pub cmd: Vec<String>,
}

pub struct TmuxSession {
    socket: String,
    name: String,
    cols: u16,
}

fn tmux(args: &[String]) -> std::io::Result<std::process::Output> {
    Command::new("tmux").args(args).output()
}

impl TmuxSession {
    /// Is a private server startable here? Probes with a throwaway `sh`.
    pub fn availability() -> Availability {
        if !panes::tmux_available() {
            return Availability::Absent;
        }
        match Self::start(&Launch {
            cols: 20,
            rows: 5,
            cwd: std::env::temp_dir(),
            env: Vec::new(),
            cmd: vec!["sh".into()],
        }) {
            Ok(_guard) => Availability::Available,
            Err(e) => Availability::CannotStart(e.to_string()),
        }
    }

    /// Start a detached session whose first window runs `cmd`. Polls
    /// `has-session` for up to 2 s; a refusal is the error, with tmux's stderr.
    pub fn start(opts: &Launch) -> std::io::Result<Self> {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let socket = format!("hotl-smoke-{}-{n}", std::process::id());
        let name = "smoke".to_string();
        let cmd: Vec<&str> = opts.cmd.iter().map(String::as_str).collect();
        let argv = panes::new_session_argv(
            &socket,
            &name,
            opts.cols,
            opts.rows,
            &opts.cwd.to_string_lossy(),
            &cmd,
        );
        let mut command = Command::new("tmux");
        command.args(&argv).env_remove("TMUX");
        for (k, v) in &opts.env {
            command.env(k, v);
        }
        let out = command.output()?;
        if !out.status.success() {
            return Err(std::io::Error::other(format!(
                "tmux new-session failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        let session = Self {
            socket,
            name,
            cols: opts.cols,
        };
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let has = tmux(&[
                "-L".into(),
                session.socket.clone(),
                "has-session".into(),
                "-t".into(),
                session.name.clone(),
            ])?;
            if has.status.success() {
                return Ok(session);
            }
            if Instant::now() > deadline {
                return Err(std::io::Error::other(format!(
                    "tmux session never came up: {}",
                    String::from_utf8_lossy(&has.stderr).trim()
                )));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    pub fn socket(&self) -> &str {
        &self.socket
    }

    /// Key names: `Enter`, `Down`, `C-g`, `Escape`, `PageDown`, `Tab`.
    pub fn send_keys(&self, keys: &[&str]) {
        let _ = tmux(&panes::send_keys_argv(
            &self.socket,
            &self.name,
            keys,
            false,
        ));
    }

    /// Literal text (`send-keys -l`): `Enter` typed this way is five letters.
    pub fn type_text(&self, s: &str) {
        let _ = tmux(&panes::send_keys_argv(&self.socket, &self.name, &[s], true));
    }

    pub fn type_line(&self, s: &str) {
        self.type_text(s);
        self.send_keys(&["Enter"]);
    }

    /// Raw rows, padded to `cols` (tmux trims trailing spaces).
    pub fn capture(&self) -> Vec<String> {
        let out = tmux(&panes::capture_argv(&self.socket, &self.name, false));
        let text = out
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default();
        let cols = self.cols as usize;
        text.lines()
            .map(|l| {
                let width = l.chars().count();
                if width < cols {
                    format!("{l}{}", " ".repeat(cols - width))
                } else {
                    l.to_string()
                }
            })
            .collect()
    }

    /// Poll `capture` every 100 ms until `pred` holds or `within` passes;
    /// on timeout, panic with `what` and the last frame so the failure shows
    /// the screen.
    pub fn wait_for(
        &self,
        what: &str,
        pred: impl Fn(&[String]) -> bool,
        within: Duration,
    ) -> Vec<String> {
        let deadline = Instant::now() + within;
        let last = loop {
            let rows = self.capture();
            if pred(&rows) {
                return rows;
            }
            if Instant::now() > deadline {
                break rows;
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        panic!(
            "timed out waiting for {what} ({within:?}); last frame:\n{}",
            last.join("\n")
        );
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        let _ = tmux(&panes::resize_argv(&self.socket, &self.name, cols, rows));
        self.cols = cols;
    }

    /// Does the private server still answer? (The drop guard's test.)
    pub fn server_alive(socket: &str) -> bool {
        tmux(&[
            "-L".into(),
            socket.into(),
            "has-session".into(),
            "-t".into(),
            "smoke".into(),
        ])
        .map(|o| o.status.success())
        .unwrap_or(false)
    }
}

impl Drop for TmuxSession {
    fn drop(&mut self) {
        let _ = tmux(&panes::kill_server_argv(&self.socket));
    }
}

/// The moving parts a live terminal shows and `TestBackend` never did
/// (0074 Design). Pure, so scenarios never regex inline.
#[derive(Debug, Clone, Default)]
pub struct Masks {
    /// The strip's resting wave (`hotl_tui::anim::at_rest()`), substituted for
    /// any run of wave glyphs so a running strip and a resting one share a
    /// prefix; the *state* is asserted by the text after it.
    pub at_rest: String,
    /// The card spine's working frames (`view::WORKING_FRAMES`) → `⠿`.
    pub frames: Vec<String>,
    /// Glyphs the wave is drawn with, in any order.
    pub wave_glyphs: Vec<char>,
}

/// Mask elapsed seconds, token estimates, byte counts, a moving wave, a
/// working-frame glyph and ULIDs; then rotate the transcript region's leading
/// blank run to its end (view.rs's own rule) unless a modal is up.
pub fn normalize_rows(rows: &[String], region: Range<usize>, masks: &Masks) -> Vec<String> {
    let mut out: Vec<String> = rows.iter().map(|r| mask_row(r, masks)).collect();
    let region = region.start.min(out.len())..region.end.min(out.len());
    let modal_up = out[region.clone()]
        .iter()
        .any(|r| r.contains('┌') || r.contains('╭'));
    if !modal_up {
        let leading = out[region.clone()]
            .iter()
            .take_while(|r| r.trim().is_empty())
            .count();
        if leading > 0 && leading < region.len() {
            let slice = &mut out[region.clone()];
            slice.rotate_left(leading);
        }
    }
    out
}

fn mask_row(row: &str, masks: &Masks) -> String {
    let mut s = row.to_string();
    for frame in &masks.frames {
        if !frame.is_empty() {
            s = s.replace(frame.as_str(), "⠿");
        }
    }
    if !masks.at_rest.is_empty() && !masks.wave_glyphs.is_empty() {
        s = mask_wave(&s, masks);
    }
    s = mask_number_unit(&s, "s", "N");
    s = mask_tok(&s);
    s = mask_bytes(&s);
    mask_ulids(&s)
}

/// A run of ≥ 4 wave glyphs → the resting wave.
fn mask_wave(s: &str, masks: &Masks) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if masks.wave_glyphs.contains(&chars[i]) {
            let start = i;
            while i < chars.len() && masks.wave_glyphs.contains(&chars[i]) {
                i += 1;
            }
            if i - start >= 4 {
                out.push_str(&masks.at_rest);
            } else {
                out.extend(&chars[start..i]);
            }
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// `· 3s` / `· 12.4s` → `· Ns` (only directly after a middle dot).
fn mask_number_unit(s: &str, unit: &str, with: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find("· ") {
        let cut = i + "· ".len();
        out.push_str(&rest[..cut]);
        rest = &rest[cut..];
        let digits = rest
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .count();
        if digits > 0 && rest[digits..].starts_with(unit) {
            let after = &rest[digits + unit.len()..];
            if after.is_empty() || !after.chars().next().unwrap().is_alphanumeric() {
                out.push_str(with);
                out.push_str(unit);
                rest = after;
                continue;
            }
        }
    }
    out.push_str(rest);
    out
}

/// `~123 tok` → `~N tok`.
fn mask_tok(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('~') {
        out.push_str(&rest[..i + 1]);
        rest = &rest[i + 1..];
        let digits = rest
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == 'k')
            .count();
        if digits > 0 && rest[digits..].starts_with(" tok") {
            out.push('N');
            rest = &rest[digits..];
        }
    }
    out.push_str(rest);
    out
}

/// `4.2 KB` / `512 B` / `1.5 MB` → `N KB`.
fn mask_bytes(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    'outer: while !rest.is_empty() {
        for unit in [" KB", " MB", " B"] {
            if let Some(i) = rest.find(unit) {
                // Walk back over the number.
                let before = &rest[..i];
                let num_len = before
                    .chars()
                    .rev()
                    .take_while(|c| c.is_ascii_digit() || *c == '.')
                    .count();
                let num_start = before.len() - num_len;
                let boundary = num_start == 0
                    || !before[..num_start]
                        .chars()
                        .last()
                        .unwrap()
                        .is_alphanumeric();
                if num_len > 0 && boundary {
                    out.push_str(&before[..num_start]);
                    out.push_str("N KB");
                    rest = &rest[i + unit.len()..];
                    continue 'outer;
                }
            }
        }
        out.push_str(rest);
        break;
    }
    out
}

/// 26 Crockford-base32 characters with a leading digit → `ULID`.
fn mask_ulids(s: &str) -> String {
    const ALPHABET: &str = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let run = chars[i..]
            .iter()
            .take_while(|c| ALPHABET.contains(**c))
            .count();
        let boundary_before = i == 0 || !chars[i - 1].is_alphanumeric();
        if run == 26 && boundary_before && chars[i].is_ascii_digit() {
            let boundary_after = i + 26 >= chars.len() || !chars[i + 26].is_alphanumeric();
            if boundary_after {
                out.push_str("ULID");
                i += 26;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn masks() -> Masks {
        Masks {
            at_rest: "▁▁▁▁▁▁▁▁▁▁▁▁".into(),
            frames: ["⠁", "⠂", "⠄", "⡀"].iter().map(|s| s.to_string()).collect(),
            wave_glyphs: "▁▂▃▄▅▆▇█".chars().collect(),
        }
    }

    #[test]
    fn normalize_masks_elapsed_bytes_and_tokens() {
        let rows = vec![
            "▁▁▁▁▁▁▁▁▁▁▁▁ preparing 1 call · 4.2 KB · 3s".to_string(),
            "thinking · ~123 tok · 12.4s".to_string(),
            "writing · ~1.2k tok · 7s · 512 B".to_string(),
            "session 01HZX5Q7W9A3BCDEFGHJKMNPQR done".to_string(),
        ];
        let out = normalize_rows(&rows, 0..0, &masks());
        assert_eq!(out[0], "▁▁▁▁▁▁▁▁▁▁▁▁ preparing 1 call · N KB · Ns");
        assert_eq!(out[1], "thinking · ~N tok · Ns");
        assert_eq!(out[2], "writing · ~N tok · Ns · N KB");
        assert_eq!(out[3], "session ULID done");
    }

    #[test]
    fn normalize_replaces_a_moving_wave_with_the_resting_one() {
        let rows = vec![
            "▂▃▅▇█▇▅▃▂▁▁▁ thinking · 2s".to_string(),
            "⠂ running card".to_string(),
        ];
        let out = normalize_rows(&rows, 0..0, &masks());
        assert_eq!(out[0], "▁▁▁▁▁▁▁▁▁▁▁▁ thinking · Ns");
        assert_eq!(out[1], "⠿ running card");
        // A short run of wave glyphs in prose is left alone.
        let out = normalize_rows(&["a ▁▂ b".to_string()], 0..0, &masks());
        assert_eq!(out[0], "a ▁▂ b");
    }

    #[test]
    fn normalize_rotates_leading_blank_rows_like_view() {
        let rows: Vec<String> = ["", "", "❯ hi", "answer", "strip"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let out = normalize_rows(&rows, 0..4, &masks());
        assert_eq!(out, ["❯ hi", "answer", "", "", "strip"]);
        // A modal in the region pins the rows where they are.
        let rows: Vec<String> = ["", "┌ ask ┐", "strip"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let out = normalize_rows(&rows, 0..2, &masks());
        assert_eq!(out, ["", "┌ ask ┐", "strip"]);
    }

    fn skip_unless_available() -> bool {
        match TmuxSession::availability() {
            Availability::Available => true,
            other => {
                if std::env::var("HOTL_TMUX_SMOKE").as_deref() == Ok("required") {
                    panic!("tmux required but {other:?}");
                }
                eprintln!("skipped: tmux {other:?}");
                false
            }
        }
    }

    #[test]
    fn tmux_guard_kills_its_private_server() {
        if !skip_unless_available() {
            return;
        }
        let session = TmuxSession::start(&Launch {
            cols: 40,
            rows: 10,
            cwd: std::env::temp_dir(),
            env: Vec::new(),
            cmd: vec!["sh".into()],
        })
        .unwrap();
        let socket = session.socket().to_string();
        assert!(TmuxSession::server_alive(&socket));
        drop(session);
        assert!(!TmuxSession::server_alive(&socket), "kill-server on drop");
    }

    #[test]
    fn tmux_type_line_is_literal() {
        if !skip_unless_available() {
            return;
        }
        let session = TmuxSession::start(&Launch {
            cols: 40,
            rows: 10,
            cwd: std::env::temp_dir(),
            env: Vec::new(),
            cmd: vec!["cat".into()],
        })
        .unwrap();
        session.type_line("Enter go");
        let rows = session.wait_for(
            "cat to echo the literal line",
            |rows| rows.iter().filter(|r| r.starts_with("Enter go")).count() >= 2,
            Duration::from_secs(5),
        );
        assert_eq!(rows[0].len(), 40, "rows are padded to the width");
    }
}
