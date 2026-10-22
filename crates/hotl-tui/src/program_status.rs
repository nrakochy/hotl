//! OSC 7501 program status (0071 T6): the console tells the terminal whether
//! it is working, blocked on the human, done, or idle, so a tab or a pane
//! can show it. Spec rev 0.1 (the Ghostty proposal); Pi and Claude Code emit
//! the same vocabulary. Emitted blind — an unknown OSC is ignored by every
//! terminal, and a `?` query reply would land in crossterm's event stream.
//!
//! `ESC ] 7501 ; state=<s>:app=hotl[:kind=<k>][:msg=<b64>] ESC \`

use crate::app::{Phase, State};

/// Where the program stands. `Done` and `Error` survive the process per the
/// spec; `Clear` is what quitting sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramState {
    Working,
    Blocked,
    Done,
    Error,
    Idle,
    Clear,
}

/// Why it is blocked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Permission,
    Question,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramStatus {
    pub state: ProgramState,
    pub kind: Option<Kind>,
    /// The session name or title — never prompt or model text.
    pub msg: Option<String>,
}

/// Decoded bytes a `msg` may carry; the spec's cap.
const MSG_MAX_BYTES: usize = 2048;
/// The whole sequence's cap.
const SEQUENCE_MAX_BYTES: usize = 4096;
const APP: &str = "hotl";

impl ProgramStatus {
    pub fn new(state: ProgramState) -> Self {
        Self {
            state,
            kind: None,
            msg: None,
        }
    }

    /// The status the current phase means, with the session's label as the
    /// message.
    pub fn for_state(state: &State) -> Self {
        let (program, kind) = match &state.phase {
            Phase::Idle => (ProgramState::Idle, None),
            Phase::Sampling { .. } | Phase::Streaming { .. } | Phase::Tool { .. } => {
                (ProgramState::Working, None)
            }
            Phase::WaitingAsk { .. } | Phase::WaitingEgress { .. } => {
                (ProgramState::Blocked, Some(Kind::Permission))
            }
            Phase::WaitingQuestion { .. } => (ProgramState::Blocked, Some(Kind::Question)),
        };
        Self {
            state: program,
            kind,
            msg: label(state),
        }
    }

    /// A turn ended: `done`, or `error` when it failed.
    pub fn finished(state: &State, error: bool) -> Self {
        Self {
            state: if error {
                ProgramState::Error
            } else {
                ProgramState::Done
            },
            kind: None,
            msg: label(state),
        }
    }

    /// The wire bytes. Control characters never reach the terminal, the
    /// message is capped before encoding, and the whole sequence after.
    pub fn encode(&self) -> String {
        let state = match self.state {
            ProgramState::Working => "working",
            ProgramState::Blocked => "blocked",
            ProgramState::Done => "done",
            ProgramState::Error => "error",
            ProgramState::Idle => "idle",
            ProgramState::Clear => "clear",
        };
        let mut body = format!("state={state}:app={APP}");
        if let Some(kind) = self.kind {
            body.push_str(match kind {
                Kind::Permission => ":kind=permission",
                Kind::Question => ":kind=question",
            });
        }
        if let Some(msg) = &self.msg {
            let clean: String = msg.chars().filter(|c| !c.is_control()).collect();
            let mut end = clean.len().min(MSG_MAX_BYTES);
            while !clean.is_char_boundary(end) {
                end -= 1;
            }
            if end > 0 {
                body.push_str(":msg=");
                body.push_str(&base64(&clean.as_bytes()[..end]));
            }
        }
        let seq = format!("\x1b]7501;{body}\x1b\\");
        if seq.len() <= SEQUENCE_MAX_BYTES {
            seq
        } else {
            // Drop the message rather than send a truncated sequence.
            Self {
                msg: None,
                ..self.clone()
            }
            .encode()
        }
    }
}

fn label(state: &State) -> Option<String> {
    state
        .session_name
        .clone()
        .or_else(|| state.session_title.clone())
}

/// Standard base64 with padding — the one encoder this crate needs.
pub fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// `HOTL_PROGRAM_STATUS`: `0` off, `1` always on, unset → on unless tmux or
/// screen is in the way (they swallow the sequence).
pub fn enabled(flag: Option<&str>, tmux: Option<&str>, sty: Option<&str>) -> bool {
    match flag.map(str::trim) {
        Some("0") => false,
        Some("1") => true,
        _ => tmux.is_none() && sty.is_none(),
    }
}

/// Writes a status only when it changed — the terminal has no use for the
/// same report twice (Pi's JSON compare).
#[derive(Default)]
pub struct Dedup {
    last: Option<String>,
}

impl Dedup {
    /// The bytes to write, or `None` when they are what was written last.
    pub fn next(&mut self, status: &ProgramStatus) -> Option<String> {
        let seq = status.encode();
        if self.last.as_deref() == Some(seq.as_str()) {
            return None;
        }
        self.last = Some(seq.clone());
        Some(seq)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_matches_the_spec_grammar() {
        let s = ProgramStatus::new(ProgramState::Working);
        assert_eq!(s.encode(), "\x1b]7501;state=working:app=hotl\x1b\\");
        let blocked = ProgramStatus {
            state: ProgramState::Blocked,
            kind: Some(Kind::Question),
            msg: Some("fix-auth".into()),
        };
        assert_eq!(
            blocked.encode(),
            "\x1b]7501;state=blocked:app=hotl:kind=question:msg=Zml4LWF1dGg=\x1b\\"
        );
        assert_eq!(
            ProgramStatus::new(ProgramState::Clear).encode(),
            "\x1b]7501;state=clear:app=hotl\x1b\\"
        );
        assert_eq!(base64(b"Man"), "TWFu");
        assert_eq!(base64(b"Ma"), "TWE=");
        assert_eq!(base64(b""), "");
    }

    #[test]
    fn a_message_is_base64_and_capped_at_2048_bytes() {
        let long = "x".repeat(5000);
        let s = ProgramStatus {
            state: ProgramState::Idle,
            kind: None,
            msg: Some(long),
        };
        let seq = s.encode();
        assert!(seq.len() <= SEQUENCE_MAX_BYTES, "{}", seq.len());
        let b64 = seq
            .split(":msg=")
            .nth(1)
            .unwrap()
            .trim_end_matches("\x1b\\");
        assert_eq!(b64.len(), MSG_MAX_BYTES.div_ceil(3) * 4);
    }

    #[test]
    fn control_characters_never_reach_the_wire() {
        let s = ProgramStatus {
            state: ProgramState::Done,
            kind: None,
            msg: Some("a\x1b]0;evil\x07b\n".into()),
        };
        let seq = s.encode();
        assert_eq!(seq.matches('\x1b').count(), 2, "{seq:?}");
        // ESC, BEL and the newline are gone; the printable rest stays.
        assert!(seq.contains(":msg=YV0wO2V2aWxi"), "{seq:?}");
    }

    #[test]
    fn program_status_is_off_under_tmux_unless_forced() {
        assert!(enabled(None, None, None));
        assert!(!enabled(None, Some("/tmp/tmux-1/default,1,0"), None));
        assert!(!enabled(None, None, Some("1234.pts-0.host")));
        assert!(enabled(Some("1"), Some("x"), Some("y")));
        assert!(!enabled(Some("0"), None, None));
    }

    #[test]
    fn the_same_status_twice_is_written_once() {
        let mut d = Dedup::default();
        let s = ProgramStatus::new(ProgramState::Working);
        assert!(d.next(&s).is_some());
        assert!(d.next(&s).is_none());
        assert!(d.next(&ProgramStatus::new(ProgramState::Idle)).is_some());
        assert!(d.next(&s).is_some(), "a change back is a change");
    }
}
