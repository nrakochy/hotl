//! The Elm core: `State` × `Msg` → mutations + `Cmd` effects. Pure — elapsed
//! time is tick counts (8/sec), never wall-clock, so every transition is
//! golden-testable.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::Value;

use crate::vim::{Editor, EditorEvent};

/// What the agent is doing right now. `ticks` count time *in this phase*
/// (8/sec); `WaitingAsk` deliberately has none — the loop is halted on you.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Sampling {
        ticks: u64,
    },
    Streaming {
        ticks: u64,
        chars: u64,
    },
    Tool {
        name: String,
        ticks: u64,
    },
    WaitingAsk {
        req_id: u64,
        summary: String,
        protected_why: Option<String>,
        input: String,
        denying: bool,
    },
    Compacting {
        ticks: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptItem {
    User {
        text: String,
    },
    /// `queued=true` → pinned chip until the engine admits it (`prompt_queued`).
    Steer {
        text: String,
        queued: bool,
    },
    /// Grows via `text_delta`.
    Assistant {
        text: String,
    },
    Tool {
        name: String,
        summary: String,
        status: ToolStatus,
        ticks: u64,
    },
    /// Retrying / fallback / compacted / outcome errors.
    Notice {
        text: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolStatus {
    Running,
    Done,
    Failed,
    Denied,
    AutoAllowed { rule: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scroll {
    Follow,
    At(usize),
}

#[derive(Debug)]
pub struct State {
    pub phase: Phase,
    pub transcript: Vec<TranscriptItem>,
    pub scroll: Scroll,
    pub editor: Editor,
    pub vim_mode: bool,
    pub model: String,
    /// Set on the prompt result (real usage; streaming shows `chars/4`).
    pub usage_line: Option<String>,
    pub help_open: bool,
    /// First Esc sent a cancel; suppresses duplicate notices until the result.
    pub interrupt_sent: bool,
    /// `tool_auto_allowed` arrives before its `tool_start`; the rule parks
    /// here until the card exists.
    pub pending_auto_rule: Option<String>,
    /// Display name (badge + titles); seeded from the open handshake,
    /// updated by `/rename`.
    pub session_name: Option<String>,
    /// Effective permission mode (`ask` | `auto` | `plan` | `dontask`);
    /// updated optimistically by `/plan` and `/mode`. Defaults to `ask` —
    /// the library default — until the engine says otherwise.
    pub mode: String,
    /// Every loadable skill name, from the `initialize` result. `/<name>`
    /// resolves against this, so an unknown slash stays an unknown
    /// command instead of becoming a wasted turn.
    pub skills: Vec<String>,
    /// Transcript spacing, from `[settings] density`. Drives the blank line
    /// between turns and the left-gutter width the role spine lives in.
    pub density: hotl_theme::Density,
    /// The `todo_write` checklist, from `todos_changed` updates. Empty means
    /// either no list yet or the model cleared it — both render as nothing.
    pub todos: Vec<hotl_tools::todo::Todo>,
}

impl State {
    pub fn new(vim_mode: bool, model: String) -> Self {
        State {
            phase: Phase::Idle,
            transcript: Vec::new(),
            scroll: Scroll::Follow,
            editor: Editor::new(vim_mode),
            vim_mode,
            model,
            usage_line: None,
            help_open: false,
            interrupt_sent: false,
            pending_auto_rule: None,
            session_name: None,
            mode: "ask".into(),
            skills: Vec::new(),
            density: hotl_theme::Density::default(),
            todos: Vec::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn test_default() -> Self {
        State::new(true, "test-model".into())
    }
}

#[derive(Debug)]
pub enum Msg {
    /// The `update` object from a `session/update` notification.
    Update(Value),
    PermissionRequest {
        req_id: u64,
        summary: String,
        protected_why: Option<String>,
    },
    PromptResult {
        outcome_kind: String,
        outcome_text: Option<String>,
        usage: Value,
    },
    Key(KeyEvent),
    Tick,
    /// `$EDITOR` result; `None` = unchanged/aborted.
    EditorDone(Option<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    SendPrompt(String),
    SendSteer(String),
    /// Send `session/rename` (fire-and-forget; the ack is noise).
    Rename(String),
    /// Send `session/set_mode` (fire-and-forget; the ack is noise). Payload
    /// is the mode name (`"ask" | "auto" | "plan" | "dontask"`) — already
    /// validated by `slash_command` before this is emitted.
    SetMode(String),
    Cancel,
    ReplyPermission {
        req_id: u64,
        allow: bool,
        message: Option<String>,
    },
    OpenEditor(String),
    SetTitle(String),
    /// Append a submitted prompt to the on-disk history file (the runtime
    /// owns the file; the core just names what to persist).
    AppendHistory(String),
    Quit,
}

/// Terminal-tab title: `hotl` / `hotl · <name>`, plus a state suffix.
fn title(state: &State, suffix: &str) -> String {
    match &state.session_name {
        Some(n) => format!("hotl · {n}{suffix}"),
        None => format!("hotl{suffix}"),
    }
}

pub fn update(state: &mut State, msg: Msg) -> Vec<Cmd> {
    match msg {
        Msg::Update(v) => on_update(state, &v),
        Msg::PermissionRequest {
            req_id,
            summary,
            protected_why,
        } => {
            state.phase = Phase::WaitingAsk {
                req_id,
                summary,
                protected_why,
                input: String::new(),
                denying: false,
            };
            vec![Cmd::SetTitle(title(state, " — waiting on you"))]
        }
        Msg::PromptResult {
            outcome_kind,
            outcome_text,
            usage,
        } => on_prompt_result(state, &outcome_kind, outcome_text, &usage),
        Msg::Key(key) => on_key(state, key),
        Msg::Tick => {
            on_tick(state);
            Vec::new()
        }
        Msg::EditorDone(content) => {
            if let Some(text) = content {
                state.editor.set_text(text.trim_end_matches('\n'));
            }
            Vec::new()
        }
    }
}

fn on_update(state: &mut State, v: &Value) -> Vec<Cmd> {
    let text_of = |key: &str| v.get(key).and_then(Value::as_str).unwrap_or("").to_string();
    match v.get("type").and_then(Value::as_str).unwrap_or("") {
        "text_delta" => {
            append_assistant(state, &text_of("text"));
            enter_streaming(state);
        }
        "tool_start" => {
            let status = match state.pending_auto_rule.take() {
                Some(rule) => ToolStatus::AutoAllowed { rule },
                None => ToolStatus::Running,
            };
            let name = text_of("name");
            state.transcript.push(TranscriptItem::Tool {
                name: name.clone(),
                summary: text_of("summary"),
                status,
                ticks: 0,
            });
            state.phase = Phase::Tool { name, ticks: 0 };
        }
        "tool_done" => {
            let ok = v.get("ok").and_then(Value::as_bool).unwrap_or(false);
            mark_last_tool(
                state,
                &text_of("name"),
                if ok {
                    ToolStatus::Done
                } else {
                    ToolStatus::Failed
                },
            );
            enter_streaming(state);
        }
        // Denied tools never get a `tool_start` (the engine returns before
        // running them) — the denial itself is the card.
        "tool_denied" => {
            let name = text_of("name");
            state.transcript.push(TranscriptItem::Tool {
                name: name.clone(),
                summary: name,
                status: ToolStatus::Denied,
                ticks: 0,
            });
            enter_streaming(state);
        }
        "tool_auto_allowed" => state.pending_auto_rule = Some(text_of("rule")),
        "todos_changed" => {
            state.todos = v
                .get("items")
                .cloned()
                .and_then(|items| serde_json::from_value(items).ok())
                .unwrap_or_default();
        }
        "retrying" => {
            let attempt = v.get("attempt").and_then(Value::as_u64).unwrap_or(0);
            notice(
                state,
                format!("retrying (attempt {attempt}) — {}", text_of("reason")),
            );
        }
        "fallback_model" => {
            state.model = text_of("model");
            notice(state, format!("model fallback → {}", state.model));
        }
        "prompt_queued" => {
            if let Some(TranscriptItem::Steer { queued, .. }) = state
                .transcript
                .iter_mut()
                .rev()
                .find(|i| matches!(i, TranscriptItem::Steer { queued: true, .. }))
            {
                *queued = false;
            }
        }
        "compacted" => {
            let degraded = v.get("degraded").and_then(Value::as_bool).unwrap_or(false);
            notice(
                state,
                if degraded {
                    "history folded — degraded".into()
                } else {
                    "history folded".into()
                },
            );
            if matches!(state.phase, Phase::Compacting { .. }) {
                state.phase = Phase::Sampling { ticks: 0 };
            }
        }
        // `turn_done` rides in the prompt result; thinking stays in Sampling.
        _ => {}
    }
    Vec::new()
}

fn on_prompt_result(
    state: &mut State,
    kind: &str,
    text: Option<String>,
    usage: &Value,
) -> Vec<Cmd> {
    // A turn that streamed nothing still shows its outcome text.
    if turn_chars(&state.transcript) == 0 {
        if let Some(t) = text.as_deref().filter(|t| kind == "done" && !t.is_empty()) {
            state.transcript.push(TranscriptItem::Assistant {
                text: t.to_string(),
            });
        }
    }
    if let Some(n) = outcome_notice(kind, text.as_deref()) {
        notice(state, n);
    }
    state.usage_line = Some(format_usage(usage));
    state.phase = Phase::Idle;
    state.interrupt_sent = false;
    vec![Cmd::SetTitle(title(state, ""))]
}

fn outcome_notice(kind: &str, text: Option<&str>) -> Option<String> {
    Some(match kind {
        "done" => return None,
        "cancelled" => "turn cancelled".into(),
        "turn_limit" => "turn limit reached".into(),
        "refused" => "provider refused the request".into(),
        other => format!("{other}: {}", text.unwrap_or(""))
            .trim_end_matches([':', ' '])
            .to_string(),
    })
}

fn format_usage(usage: &Value) -> String {
    let n = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    format!("{} in · {} out tok", n("input_tokens"), n("output_tokens"))
}

fn on_key(state: &mut State, key: KeyEvent) -> Vec<Cmd> {
    if state.help_open {
        state.help_open = false;
        return Vec::new();
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return match state.phase {
            Phase::Idle => vec![Cmd::Quit],
            _ => vec![Cmd::Cancel],
        };
    }
    if matches!(state.phase, Phase::WaitingAsk { .. }) {
        return on_ask_key(state, key);
    }
    if key.code == KeyCode::Esc && state.phase != Phase::Idle && state.editor.is_empty() {
        if !state.interrupt_sent {
            state.interrupt_sent = true;
            notice(state, "interrupting — esc again to insist".into());
        }
        return vec![Cmd::Cancel];
    }
    if key.code == KeyCode::Char('?') && state.editor.is_empty() {
        state.help_open = true;
        return Vec::new();
    }
    match state.editor.handle(key) {
        EditorEvent::Submit(text) if text.trim().is_empty() => Vec::new(),
        EditorEvent::Submit(text) => {
            let cmds = submit(state, text.clone());
            // Persist only prompt-starting submissions (they emit SendPrompt),
            // and only when the literal text wasn't a slash command — a skill
            // invocation desugars to a prompt but shouldn't leave its `/name`
            // (or the expanded template) on disk. In-session recall still walks
            // everything via the editor's own ring.
            let starts_turn = cmds.iter().any(|c| matches!(c, Cmd::SendPrompt(_)));
            if starts_turn && !text.trim_start().starts_with('/') {
                let mut out = vec![Cmd::AppendHistory(text)];
                out.extend(cmds);
                out
            } else {
                cmds
            }
        }
        EditorEvent::OpenExternal(text) => vec![Cmd::OpenEditor(text)],
        EditorEvent::ScrollUp => {
            let cur = match state.scroll {
                Scroll::Follow => state.transcript.len(),
                Scroll::At(i) => i,
            };
            state.scroll = Scroll::At(cur.saturating_sub(1));
            Vec::new()
        }
        EditorEvent::ScrollDown => {
            if let Scroll::At(i) = state.scroll {
                state.scroll = if i + 1 >= state.transcript.len() {
                    Scroll::Follow
                } else {
                    Scroll::At(i + 1)
                };
            }
            Vec::new()
        }
        EditorEvent::None => Vec::new(),
    }
}

fn submit(state: &mut State, text: String) -> Vec<Cmd> {
    if let Some(rest) = text.trim().strip_prefix('/') {
        return slash_command(state, rest);
    }
    if state.phase == Phase::Idle {
        state
            .transcript
            .push(TranscriptItem::User { text: text.clone() });
        state.phase = Phase::Sampling { ticks: 0 };
        state.scroll = Scroll::Follow;
        vec![
            Cmd::SendPrompt(text),
            Cmd::SetTitle(title(state, " — working")),
        ]
    } else {
        state.transcript.push(TranscriptItem::Steer {
            text: text.clone(),
            queued: true,
        });
        vec![Cmd::SendSteer(text)]
    }
}

/// The TUI's slash commands. Built-ins resolve first; an unmatched
/// `/<skill>` asks the model to load that skill, which is the human
/// override for skills the tool description no longer names. Anything
/// else is a transcript notice — unresolved slash input never reaches the
/// model.
fn slash_command(state: &mut State, rest: &str) -> Vec<Cmd> {
    let (cmd, arg) = rest
        .split_once(char::is_whitespace)
        .map(|(c, a)| (c, a.trim()))
        .unwrap_or((rest.trim(), ""));
    match cmd {
        "rename" => {
            // Same rules as hotl_types::normalize_session_name (this crate
            // has no hotl-types dep): trimmed, non-empty, ≤ 64 chars.
            let name = arg.trim();
            if name.is_empty() || name.chars().count() > 64 {
                notice(state, "usage: /rename <name> (1–64 chars)".into());
                return Vec::new();
            }
            state.session_name = Some(name.to_string());
            notice(state, format!("session renamed to {name}"));
            let suffix = if state.phase == Phase::Idle {
                ""
            } else {
                " — working"
            };
            vec![
                Cmd::Rename(name.to_string()),
                Cmd::SetTitle(title(state, suffix)),
            ]
        }
        "plan" => set_mode(state, "plan"),
        "mode" => {
            // Delegate to `PermissionMode::from_str` — the same parser ACP's
            // `session/set_mode` uses — so the TUI and the wire protocol
            // share one source of truth on what a valid mode name is
            // (including the `dont_ask`/`dont-ask` aliases a hand-rolled
            // list here previously rejected). The canonical `as_str()` form
            // is what gets stored/sent, so the badge and the wire payload
            // never disagree with what the alias actually meant.
            let Some(mode) = hotl_tools::rules::PermissionMode::from_str(arg.trim()) else {
                notice(state, "usage: /mode <ask|auto|plan|dontask>".into());
                return Vec::new();
            };
            set_mode(state, mode.as_str())
        }
        other if state.skills.iter().any(|s| s == other) => {
            // Desugars to an ordinary prompt: the model calls the skill
            // tool, so the TUI never reads skill files itself.
            let mut text = format!("Load the skill `{other}` and follow it for this task.");
            if !arg.is_empty() {
                text.push_str(&format!("\n\nARGUMENTS: {arg}"));
            }
            submit(state, text)
        }
        other => {
            notice(state, format!("unknown command: /{other}"));
            Vec::new()
        }
    }
}

fn on_ask_key(state: &mut State, key: KeyEvent) -> Vec<Cmd> {
    let Phase::WaitingAsk {
        req_id,
        input,
        denying,
        ..
    } = &mut state.phase
    else {
        return Vec::new();
    };
    let req_id = *req_id;
    if *denying {
        match key.code {
            KeyCode::Char(c) => input.push(c),
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Esc => {
                *denying = false;
                input.clear();
            }
            KeyCode::Enter => {
                let message = Some(input.clone()).filter(|m| !m.trim().is_empty());
                return resume_after_ask(state, req_id, false, message);
            }
            _ => {}
        }
        return Vec::new();
    }
    match key.code {
        KeyCode::Char('y') => resume_after_ask(state, req_id, true, None),
        KeyCode::Char('n') => {
            *denying = true;
            Vec::new()
        }
        _ => Vec::new(),
    }
}

fn resume_after_ask(
    state: &mut State,
    req_id: u64,
    allow: bool,
    message: Option<String>,
) -> Vec<Cmd> {
    state.phase = Phase::Sampling { ticks: 0 };
    vec![
        Cmd::ReplyPermission {
            req_id,
            allow,
            message,
        },
        Cmd::SetTitle(title(state, " — working")),
    ]
}

fn on_tick(state: &mut State) {
    match &mut state.phase {
        Phase::Sampling { ticks }
        | Phase::Streaming { ticks, .. }
        | Phase::Compacting { ticks } => *ticks += 1,
        Phase::Tool { ticks, .. } => {
            *ticks += 1;
            // The running card's elapsed stays in lock-step with the strip's.
            if let Some(TranscriptItem::Tool {
                ticks: card,
                status,
                ..
            }) = state
                .transcript
                .iter_mut()
                .rev()
                .find(|i| matches!(i, TranscriptItem::Tool { .. }))
            {
                if matches!(status, ToolStatus::Running | ToolStatus::AutoAllowed { .. }) {
                    *card += 1;
                }
            }
        }
        Phase::Idle | Phase::WaitingAsk { .. } => {}
    }
}

fn append_assistant(state: &mut State, text: &str) {
    if let Some(TranscriptItem::Assistant { text: t }) = state.transcript.last_mut() {
        t.push_str(text);
    } else {
        state.transcript.push(TranscriptItem::Assistant {
            text: text.to_string(),
        });
    }
}

/// Streaming resumes with this turn's running char total (chars survive a
/// tool interlude by recount, not by stashing).
fn enter_streaming(state: &mut State) {
    let ticks = match state.phase {
        Phase::Streaming { ticks, .. } => ticks,
        _ => 0,
    };
    state.phase = Phase::Streaming {
        ticks,
        chars: turn_chars(&state.transcript),
    };
}

fn turn_chars(transcript: &[TranscriptItem]) -> u64 {
    transcript
        .iter()
        .rev()
        .take_while(|i| !matches!(i, TranscriptItem::User { .. }))
        .map(|i| match i {
            TranscriptItem::Assistant { text } => text.len() as u64,
            _ => 0,
        })
        .sum()
}

fn mark_last_tool(state: &mut State, name: &str, status: ToolStatus) {
    if let Some(TranscriptItem::Tool { status: s, .. }) = state
        .transcript
        .iter_mut()
        .rev()
        .find(|i| matches!(i, TranscriptItem::Tool { name: n, .. } if n == name))
    {
        *s = status;
    }
}

fn notice(state: &mut State, text: String) {
    state.transcript.push(TranscriptItem::Notice { text });
}

/// `/plan` and `/mode <name>` share this: optimistic local update (the badge
/// flips immediately) plus the durable `SetMode` the surface issues. Never
/// starts a turn — a mode switch is session bookkeeping, not a prompt.
fn set_mode(state: &mut State, mode: &str) -> Vec<Cmd> {
    state.mode = mode.to_string();
    notice(state, format!("permission mode set to {mode}"));
    vec![Cmd::SetMode(mode.to_string())]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn upd(s: &mut State, v: Value) -> Vec<Cmd> {
        update(s, Msg::Update(v))
    }

    fn press(s: &mut State, code: KeyCode) -> Vec<Cmd> {
        update(s, Msg::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    fn ctrl(s: &mut State, c: char) -> Vec<Cmd> {
        update(
            s,
            Msg::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)),
        )
    }

    fn type_str(s: &mut State, text: &str) {
        for c in text.chars() {
            press(s, KeyCode::Char(c));
        }
    }

    fn ask(s: &mut State) {
        update(
            s,
            Msg::PermissionRequest {
                req_id: 7,
                summary: "run bash: rm -rf ./x".into(),
                protected_why: None,
            },
        );
    }

    #[test]
    fn prompt_echoes_immediately_and_enters_sampling() {
        let mut s = State::test_default();
        type_str(&mut s, "hello");
        let cmds = press(&mut s, KeyCode::Enter);
        assert!(
            matches!(s.transcript.last(), Some(TranscriptItem::User { text }) if text == "hello")
        );
        assert!(matches!(s.phase, Phase::Sampling { .. }));
        assert!(matches!(
            cmds[..],
            [Cmd::AppendHistory(_), Cmd::SendPrompt(_), Cmd::SetTitle(_)]
        ));
    }

    #[test]
    fn todos_changed_populates_state() {
        let mut s = State::test_default();
        assert!(s.todos.is_empty());
        upd(
            &mut s,
            json!({"type":"todos_changed","items":[
                {"content":"wire the gate","status":"in_progress"},
                {"content":"write docs","status":"pending"}
            ]}),
        );
        assert_eq!(s.todos.len(), 2);
        assert_eq!(s.todos[0].content, "wire the gate");
        assert_eq!(s.todos[0].status, hotl_tools::todo::TodoStatus::InProgress);

        // A later `todos_changed` fully replaces the list (including down
        // to empty — the model clearing it is a real, renderable state).
        upd(&mut s, json!({"type":"todos_changed","items":[]}));
        assert!(s.todos.is_empty());
    }

    #[test]
    fn text_delta_moves_sampling_to_streaming_and_counts_chars() {
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 8 };
        upd(&mut s, json!({"type":"text_delta","text":"hi you"}));
        assert!(matches!(s.phase, Phase::Streaming { chars: 6, .. }));
        assert!(
            matches!(s.transcript.last(), Some(TranscriptItem::Assistant { text }) if text == "hi you")
        );
    }

    #[test]
    fn tool_start_and_done_drive_tool_phase_and_card() {
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 0 };
        upd(&mut s, json!({"type":"text_delta","text":"hi you"}));
        upd(
            &mut s,
            json!({"type":"tool_start","name":"bash","summary":"echo hi"}),
        );
        assert!(matches!(&s.phase, Phase::Tool { name, .. } if name == "bash"));
        assert!(matches!(
            s.transcript.last(),
            Some(TranscriptItem::Tool {
                status: ToolStatus::Running,
                ..
            })
        ));
        upd(&mut s, json!({"type":"tool_done","name":"bash","ok":true}));
        assert!(matches!(
            s.transcript.last(),
            Some(TranscriptItem::Tool {
                status: ToolStatus::Done,
                ..
            })
        ));
        assert!(
            matches!(s.phase, Phase::Streaming { chars: 6, .. }),
            "chars survive the tool interlude"
        );
    }

    #[test]
    fn permission_request_freezes_into_waiting_ask() {
        let mut s = State::test_default();
        s.phase = Phase::Tool {
            name: "bash".into(),
            ticks: 3,
        };
        update(
            &mut s,
            Msg::PermissionRequest {
                req_id: 7,
                summary: "run bash".into(),
                protected_why: Some("prod".into()),
            },
        );
        let before = s.phase.clone();
        assert!(
            matches!(&before, Phase::WaitingAsk { req_id: 7, summary, protected_why: Some(w), .. }
            if summary == "run bash" && w == "prod")
        );
        update(&mut s, Msg::Tick);
        assert_eq!(
            s.phase, before,
            "the loop halts — ticks do not advance in an ask"
        );
    }

    #[test]
    fn ask_y_allows_and_n_with_reason_denies() {
        let mut s = State::test_default();
        ask(&mut s);
        let cmds = press(&mut s, KeyCode::Char('y'));
        assert!(matches!(
            cmds[..],
            [
                Cmd::ReplyPermission {
                    req_id: 7,
                    allow: true,
                    message: None
                },
                ..
            ]
        ));
        assert!(!matches!(s.phase, Phase::WaitingAsk { .. }));

        ask(&mut s);
        press(&mut s, KeyCode::Char('n'));
        type_str(&mut s, "wrong dir");
        let cmds = press(&mut s, KeyCode::Enter);
        assert!(
            matches!(&cmds[..], [Cmd::ReplyPermission { req_id: 7, allow: false, message: Some(m) }, ..]
            if m == "wrong dir")
        );
        assert!(!matches!(s.phase, Phase::WaitingAsk { .. }));
    }

    #[test]
    fn typing_mid_turn_queues_steer() {
        let mut s = State::test_default();
        s.phase = Phase::Streaming { ticks: 0, chars: 0 };
        type_str(&mut s, "wait");
        let cmds = press(&mut s, KeyCode::Enter);
        assert!(matches!(&cmds[..], [Cmd::SendSteer(t)] if t == "wait"));
        assert!(matches!(
            s.transcript.last(),
            Some(TranscriptItem::Steer { queued: true, .. })
        ));
        upd(&mut s, json!({"type":"prompt_queued"}));
        assert!(matches!(
            s.transcript.last(),
            Some(TranscriptItem::Steer { queued: false, .. })
        ));
    }

    #[test]
    fn esc_interrupts_then_second_esc_cancels() {
        let mut s = State::test_default();
        s.phase = Phase::Streaming { ticks: 0, chars: 0 };
        let cmds = press(&mut s, KeyCode::Esc);
        assert!(matches!(cmds[..], [Cmd::Cancel]));
        assert!(s.interrupt_sent);
        assert!(
            matches!(s.transcript.last(), Some(TranscriptItem::Notice { .. })),
            "state notes the interrupt"
        );
        let cmds = press(&mut s, KeyCode::Esc);
        assert!(matches!(cmds[..], [Cmd::Cancel]));
        update(
            &mut s,
            Msg::PromptResult {
                outcome_kind: "cancelled".into(),
                outcome_text: None,
                usage: json!({}),
            },
        );
        assert!(s
            .transcript
            .iter()
            .any(|i| matches!(i, TranscriptItem::Notice { text } if text.contains("cancel"))));
    }

    #[test]
    fn prompt_result_returns_to_idle_with_usage() {
        let mut s = State::test_default();
        s.phase = Phase::Streaming { ticks: 4, chars: 9 };
        let cmds = update(
            &mut s,
            Msg::PromptResult {
                outcome_kind: "done".into(),
                outcome_text: Some("fin".into()),
                usage: json!({"input_tokens": 120, "output_tokens": 45}),
            },
        );
        assert_eq!(s.phase, Phase::Idle);
        assert_eq!(s.usage_line.as_deref(), Some("120 in · 45 out tok"));
        assert!(matches!(&cmds[..], [Cmd::SetTitle(t)] if t == "hotl"));
    }

    #[test]
    fn compacted_and_retrying_become_notices() {
        let mut s = State::test_default();
        s.phase = Phase::Compacting { ticks: 3 };
        upd(&mut s, json!({"type":"compacted","degraded":false}));
        assert!(
            matches!(s.transcript.last(), Some(TranscriptItem::Notice { text }) if text.contains("folded"))
        );
        assert!(!matches!(s.phase, Phase::Compacting { .. }));
        upd(
            &mut s,
            json!({"type":"retrying","attempt":2,"reason":"overloaded"}),
        );
        assert!(
            matches!(s.transcript.last(), Some(TranscriptItem::Notice { text }) if text.contains("overloaded"))
        );
    }

    #[test]
    fn tick_only_advances_active_phases() {
        let mut s = State::test_default();
        let cmds = update(&mut s, Msg::Tick);
        assert!(cmds.is_empty());
        assert_eq!(s.phase, Phase::Idle);
        s.phase = Phase::Sampling { ticks: 0 };
        update(&mut s, Msg::Tick);
        assert!(matches!(s.phase, Phase::Sampling { ticks: 1 }));
    }

    #[test]
    fn ctrl_c_quits_when_idle_cancels_when_running() {
        let mut s = State::test_default();
        assert!(matches!(ctrl(&mut s, 'c')[..], [Cmd::Quit]));
        s.phase = Phase::Streaming { ticks: 0, chars: 0 };
        assert!(matches!(ctrl(&mut s, 'c')[..], [Cmd::Cancel]));
    }

    fn type_and_submit(s: &mut State, text: &str) -> Vec<Cmd> {
        type_str(s, text);
        press(s, KeyCode::Enter)
    }

    #[test]
    fn slash_rename_sets_name_emits_cmd_and_title_not_a_prompt() {
        let mut s = State::test_default();
        let cmds = type_and_submit(&mut s, "/rename fix-auth");
        assert_eq!(s.session_name.as_deref(), Some("fix-auth"));
        assert!(
            matches!(&cmds[..], [Cmd::Rename(n), Cmd::SetTitle(t)]
                if n == "fix-auth" && t == "hotl · fix-auth"),
            "got {cmds:?}"
        );
        assert_eq!(s.phase, Phase::Idle, "a slash command never starts a turn");
        assert!(
            matches!(s.transcript.last(), Some(TranscriptItem::Notice { text }) if text.contains("fix-auth"))
        );
    }

    #[test]
    fn slash_rename_without_arg_shows_usage() {
        let mut s = State::test_default();
        let cmds = type_and_submit(&mut s, "/rename");
        assert!(cmds.is_empty());
        assert_eq!(s.session_name, None);
        assert!(
            matches!(s.transcript.last(), Some(TranscriptItem::Notice { text }) if text.contains("usage"))
        );
    }

    #[test]
    fn slash_plan_sets_mode_and_does_not_start_a_turn() {
        let mut s = State::test_default();
        let cmds = type_and_submit(&mut s, "/plan");
        assert!(
            matches!(&cmds[..], [Cmd::SetMode(m)] if m == "plan"),
            "got {cmds:?}"
        );
        assert_eq!(s.phase, Phase::Idle);
        assert_eq!(s.mode, "plan");
    }

    #[test]
    fn slash_mode_sets_the_named_mode() {
        let mut s = State::test_default();
        let cmds = type_and_submit(&mut s, "/mode dontask");
        assert!(
            matches!(&cmds[..], [Cmd::SetMode(m)] if m == "dontask"),
            "got {cmds:?}"
        );
        assert_eq!(s.mode, "dontask");
        assert_eq!(s.phase, Phase::Idle);
    }

    #[test]
    fn slash_mode_accepts_dont_ask_alias_via_shared_parser() {
        // Finding 2 (Plan 2 review, MINOR): the old hardcoded
        // ["ask","auto","plan","dontask"] list rejected the `dont_ask`
        // alias that `PermissionMode::from_str` (and ACP) accept. Now that
        // `/mode` delegates to that parser, the alias must work, and the
        // canonical `as_str()` form ("dontask") is what gets sent/stored —
        // not the raw alias the user typed.
        let mut s = State::test_default();
        let cmds = type_and_submit(&mut s, "/mode dont_ask");
        assert!(
            matches!(&cmds[..], [Cmd::SetMode(m)] if m == "dontask"),
            "got {cmds:?}"
        );
        assert_eq!(s.mode, "dontask");
    }

    #[test]
    fn slash_mode_unknown_shows_usage_and_never_reaches_model() {
        let mut s = State::test_default();
        let cmds = type_and_submit(&mut s, "/mode wat");
        assert!(cmds.is_empty(), "got {cmds:?}");
        assert_eq!(s.phase, Phase::Idle);
        assert!(
            matches!(s.transcript.last(), Some(TranscriptItem::Notice { text }) if text.contains("usage"))
        );
    }

    #[test]
    fn unknown_slash_command_never_reaches_the_model() {
        let mut s = State::test_default();
        let cmds = type_and_submit(&mut s, "/frobnicate now");
        assert!(cmds.is_empty(), "got {cmds:?}");
        assert!(
            matches!(s.transcript.last(), Some(TranscriptItem::Notice { text }) if text.contains("/frobnicate"))
        );
    }

    #[test]
    fn a_known_skill_name_after_slash_prompts_for_that_skill() {
        let mut s = State::test_default();
        s.skills = vec!["brainstorming".into(), "superpowers:brainstorming".into()];

        let cmds = type_and_submit(&mut s, "/brainstorming redesign the skill system");
        let Some(Cmd::SendPrompt(text)) = cmds.first() else {
            panic!("expected a prompt, got {cmds:?}");
        };
        assert!(text.contains("`brainstorming`"), "{text}");
        assert!(
            text.contains("ARGUMENTS: redesign the skill system"),
            "the argument rides along: {text}"
        );
        assert_eq!(s.phase, Phase::Sampling { ticks: 0 });

        // Qualified names resolve too, and take no argument fine.
        let mut s = State::test_default();
        s.skills = vec!["superpowers:brainstorming".into()];
        let cmds = type_and_submit(&mut s, "/superpowers:brainstorming");
        let Some(Cmd::SendPrompt(text)) = cmds.first() else {
            panic!("expected a prompt, got {cmds:?}");
        };
        assert!(!text.contains("ARGUMENTS"), "{text}");
    }

    #[test]
    fn a_builtin_wins_over_a_skill_of_the_same_name() {
        let mut s = State::test_default();
        s.skills = vec!["rename".into()];
        let cmds = type_and_submit(&mut s, "/rename fix-auth");
        assert!(
            matches!(&cmds[..], [Cmd::Rename(n), _] if n == "fix-auth"),
            "got {cmds:?}"
        );
    }

    #[test]
    fn a_prompt_turn_persists_to_history_the_literal_text() {
        let mut s = State::test_default();
        let cmds = type_and_submit(&mut s, "fix the bug");
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::AppendHistory(t) if t == "fix the bug")),
            "got {cmds:?}"
        );
    }

    #[test]
    fn slash_commands_and_steers_do_not_persist_to_disk_history() {
        // A slash command never starts a turn → nothing to persist.
        let mut s = State::test_default();
        let cmds = type_and_submit(&mut s, "/rename foo");
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::AppendHistory(_))));

        // A skill invocation desugars to a prompt, but the *literal* was a
        // slash command — it is not written to the on-disk history either.
        let mut s = State::test_default();
        s.skills = vec!["brainstorming".into()];
        let cmds = type_and_submit(&mut s, "/brainstorming redesign");
        assert!(cmds.iter().any(|c| matches!(c, Cmd::SendPrompt(_))));
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::AppendHistory(_))));

        // A steer (typed mid-turn) uses SendSteer, not SendPrompt → not persisted.
        let mut s = State::test_default();
        s.phase = Phase::Streaming { ticks: 0, chars: 0 };
        let cmds = type_and_submit(&mut s, "wait, use X");
        assert!(cmds.iter().any(|c| matches!(c, Cmd::SendSteer(_))));
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::AppendHistory(_))));
    }

    #[test]
    fn named_session_titles_carry_the_name() {
        let mut s = State::test_default();
        s.session_name = Some("fix-auth".into());
        let cmds = type_and_submit(&mut s, "hello");
        assert!(
            matches!(&cmds[..], [Cmd::AppendHistory(_), Cmd::SendPrompt(_), Cmd::SetTitle(t)] if t == "hotl · fix-auth — working"),
            "got {cmds:?}"
        );
    }
}
