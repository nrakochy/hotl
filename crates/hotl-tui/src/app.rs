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
    Sampling { ticks: u64 },
    Streaming { ticks: u64, chars: u64 },
    Tool { name: String, ticks: u64 },
    WaitingAsk { req_id: u64, summary: String, protected_why: Option<String>, input: String, denying: bool },
    Compacting { ticks: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptItem {
    User { text: String },
    /// `queued=true` → pinned chip until the engine admits it (`prompt_queued`).
    Steer { text: String, queued: bool },
    /// Grows via `text_delta`.
    Assistant { text: String },
    Tool { name: String, summary: String, status: ToolStatus, ticks: u64 },
    /// Retrying / fallback / compacted / outcome errors.
    Notice { text: String },
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
    PermissionRequest { req_id: u64, summary: String, protected_why: Option<String> },
    PromptResult { outcome_kind: String, outcome_text: Option<String>, usage: Value },
    Key(KeyEvent),
    Tick,
    /// `$EDITOR` result; `None` = unchanged/aborted.
    EditorDone(Option<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    SendPrompt(String),
    SendSteer(String),
    Cancel,
    ReplyPermission { req_id: u64, allow: bool, message: Option<String> },
    OpenEditor(String),
    SetTitle(String),
    Quit,
}

const TITLE_WORKING: &str = "hotl — working";
const TITLE_WAITING: &str = "hotl — waiting on you";
const TITLE_IDLE: &str = "hotl";

pub fn update(state: &mut State, msg: Msg) -> Vec<Cmd> {
    match msg {
        Msg::Update(v) => on_update(state, &v),
        Msg::PermissionRequest { req_id, summary, protected_why } => {
            state.phase = Phase::WaitingAsk { req_id, summary, protected_why, input: String::new(), denying: false };
            vec![Cmd::SetTitle(TITLE_WAITING.into())]
        }
        Msg::PromptResult { outcome_kind, outcome_text, usage } => on_prompt_result(state, &outcome_kind, outcome_text, &usage),
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
            state.transcript.push(TranscriptItem::Tool { name: name.clone(), summary: text_of("summary"), status, ticks: 0 });
            state.phase = Phase::Tool { name, ticks: 0 };
        }
        "tool_done" => {
            let ok = v.get("ok").and_then(Value::as_bool).unwrap_or(false);
            mark_last_tool(state, &text_of("name"), if ok { ToolStatus::Done } else { ToolStatus::Failed });
            enter_streaming(state);
        }
        // Denied tools never get a `tool_start` (the engine returns before
        // running them) — the denial itself is the card.
        "tool_denied" => {
            let name = text_of("name");
            state.transcript.push(TranscriptItem::Tool { name: name.clone(), summary: name, status: ToolStatus::Denied, ticks: 0 });
            enter_streaming(state);
        }
        "tool_auto_allowed" => state.pending_auto_rule = Some(text_of("rule")),
        "retrying" => {
            let attempt = v.get("attempt").and_then(Value::as_u64).unwrap_or(0);
            notice(state, format!("retrying (attempt {attempt}) — {}", text_of("reason")));
        }
        "fallback_model" => {
            state.model = text_of("model");
            notice(state, format!("model fallback → {}", state.model));
        }
        "prompt_queued" => {
            if let Some(TranscriptItem::Steer { queued, .. }) =
                state.transcript.iter_mut().rev().find(|i| matches!(i, TranscriptItem::Steer { queued: true, .. }))
            {
                *queued = false;
            }
        }
        "compacted" => {
            let degraded = v.get("degraded").and_then(Value::as_bool).unwrap_or(false);
            notice(state, if degraded { "history folded — degraded".into() } else { "history folded".into() });
            if matches!(state.phase, Phase::Compacting { .. }) {
                state.phase = Phase::Sampling { ticks: 0 };
            }
        }
        // `turn_done` rides in the prompt result; thinking stays in Sampling.
        _ => {}
    }
    Vec::new()
}

fn on_prompt_result(state: &mut State, kind: &str, text: Option<String>, usage: &Value) -> Vec<Cmd> {
    // A turn that streamed nothing still shows its outcome text.
    if turn_chars(&state.transcript) == 0 {
        if let Some(t) = text.as_deref().filter(|t| kind == "done" && !t.is_empty()) {
            state.transcript.push(TranscriptItem::Assistant { text: t.to_string() });
        }
    }
    if let Some(n) = outcome_notice(kind, text.as_deref()) {
        notice(state, n);
    }
    state.usage_line = Some(format_usage(usage));
    state.phase = Phase::Idle;
    state.interrupt_sent = false;
    vec![Cmd::SetTitle(TITLE_IDLE.into())]
}

fn outcome_notice(kind: &str, text: Option<&str>) -> Option<String> {
    Some(match kind {
        "done" => return None,
        "cancelled" => "turn cancelled".into(),
        "turn_limit" => "turn limit reached".into(),
        "refused" => "provider refused the request".into(),
        other => format!("{other}: {}", text.unwrap_or("")).trim_end_matches([':', ' ']).to_string(),
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
        EditorEvent::Submit(text) => submit(state, text),
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
                state.scroll = if i + 1 >= state.transcript.len() { Scroll::Follow } else { Scroll::At(i + 1) };
            }
            Vec::new()
        }
        EditorEvent::None => Vec::new(),
    }
}

fn submit(state: &mut State, text: String) -> Vec<Cmd> {
    if state.phase == Phase::Idle {
        state.transcript.push(TranscriptItem::User { text: text.clone() });
        state.phase = Phase::Sampling { ticks: 0 };
        state.scroll = Scroll::Follow;
        vec![Cmd::SendPrompt(text), Cmd::SetTitle(TITLE_WORKING.into())]
    } else {
        state.transcript.push(TranscriptItem::Steer { text: text.clone(), queued: true });
        vec![Cmd::SendSteer(text)]
    }
}

fn on_ask_key(state: &mut State, key: KeyEvent) -> Vec<Cmd> {
    let Phase::WaitingAsk { req_id, input, denying, .. } = &mut state.phase else { return Vec::new() };
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

fn resume_after_ask(state: &mut State, req_id: u64, allow: bool, message: Option<String>) -> Vec<Cmd> {
    state.phase = Phase::Sampling { ticks: 0 };
    vec![
        Cmd::ReplyPermission { req_id, allow, message },
        Cmd::SetTitle(TITLE_WORKING.into()),
    ]
}

fn on_tick(state: &mut State) {
    match &mut state.phase {
        Phase::Sampling { ticks } | Phase::Streaming { ticks, .. } | Phase::Compacting { ticks } => *ticks += 1,
        Phase::Tool { ticks, .. } => {
            *ticks += 1;
            // The running card's elapsed stays in lock-step with the strip's.
            if let Some(TranscriptItem::Tool { ticks: card, status, .. }) =
                state.transcript.iter_mut().rev().find(|i| matches!(i, TranscriptItem::Tool { .. }))
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
        state.transcript.push(TranscriptItem::Assistant { text: text.to_string() });
    }
}

/// Streaming resumes with this turn's running char total (chars survive a
/// tool interlude by recount, not by stashing).
fn enter_streaming(state: &mut State) {
    let ticks = match state.phase {
        Phase::Streaming { ticks, .. } => ticks,
        _ => 0,
    };
    state.phase = Phase::Streaming { ticks, chars: turn_chars(&state.transcript) };
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
        update(s, Msg::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)))
    }

    fn type_str(s: &mut State, text: &str) {
        for c in text.chars() {
            press(s, KeyCode::Char(c));
        }
    }

    fn ask(s: &mut State) {
        update(
            s,
            Msg::PermissionRequest { req_id: 7, summary: "run bash: rm -rf ./x".into(), protected_why: None },
        );
    }

    #[test]
    fn prompt_echoes_immediately_and_enters_sampling() {
        let mut s = State::test_default();
        type_str(&mut s, "hello");
        let cmds = press(&mut s, KeyCode::Enter);
        assert!(matches!(s.transcript.last(), Some(TranscriptItem::User { text }) if text == "hello"));
        assert!(matches!(s.phase, Phase::Sampling { .. }));
        assert!(matches!(cmds[..], [Cmd::SendPrompt(_), Cmd::SetTitle(_)]));
    }

    #[test]
    fn text_delta_moves_sampling_to_streaming_and_counts_chars() {
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 8 };
        upd(&mut s, json!({"type":"text_delta","text":"hi you"}));
        assert!(matches!(s.phase, Phase::Streaming { chars: 6, .. }));
        assert!(matches!(s.transcript.last(), Some(TranscriptItem::Assistant { text }) if text == "hi you"));
    }

    #[test]
    fn tool_start_and_done_drive_tool_phase_and_card() {
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 0 };
        upd(&mut s, json!({"type":"text_delta","text":"hi you"}));
        upd(&mut s, json!({"type":"tool_start","name":"bash","summary":"echo hi"}));
        assert!(matches!(&s.phase, Phase::Tool { name, .. } if name == "bash"));
        assert!(matches!(s.transcript.last(), Some(TranscriptItem::Tool { status: ToolStatus::Running, .. })));
        upd(&mut s, json!({"type":"tool_done","name":"bash","ok":true}));
        assert!(matches!(s.transcript.last(), Some(TranscriptItem::Tool { status: ToolStatus::Done, .. })));
        assert!(matches!(s.phase, Phase::Streaming { chars: 6, .. }), "chars survive the tool interlude");
    }

    #[test]
    fn permission_request_freezes_into_waiting_ask() {
        let mut s = State::test_default();
        s.phase = Phase::Tool { name: "bash".into(), ticks: 3 };
        update(
            &mut s,
            Msg::PermissionRequest { req_id: 7, summary: "run bash".into(), protected_why: Some("prod".into()) },
        );
        let before = s.phase.clone();
        assert!(matches!(&before, Phase::WaitingAsk { req_id: 7, summary, protected_why: Some(w), .. }
            if summary == "run bash" && w == "prod"));
        update(&mut s, Msg::Tick);
        assert_eq!(s.phase, before, "the loop halts — ticks do not advance in an ask");
    }

    #[test]
    fn ask_y_allows_and_n_with_reason_denies() {
        let mut s = State::test_default();
        ask(&mut s);
        let cmds = press(&mut s, KeyCode::Char('y'));
        assert!(matches!(cmds[..], [Cmd::ReplyPermission { req_id: 7, allow: true, message: None }, ..]));
        assert!(!matches!(s.phase, Phase::WaitingAsk { .. }));

        ask(&mut s);
        press(&mut s, KeyCode::Char('n'));
        type_str(&mut s, "wrong dir");
        let cmds = press(&mut s, KeyCode::Enter);
        assert!(matches!(&cmds[..], [Cmd::ReplyPermission { req_id: 7, allow: false, message: Some(m) }, ..]
            if m == "wrong dir"));
        assert!(!matches!(s.phase, Phase::WaitingAsk { .. }));
    }

    #[test]
    fn typing_mid_turn_queues_steer() {
        let mut s = State::test_default();
        s.phase = Phase::Streaming { ticks: 0, chars: 0 };
        type_str(&mut s, "wait");
        let cmds = press(&mut s, KeyCode::Enter);
        assert!(matches!(&cmds[..], [Cmd::SendSteer(t)] if t == "wait"));
        assert!(matches!(s.transcript.last(), Some(TranscriptItem::Steer { queued: true, .. })));
        upd(&mut s, json!({"type":"prompt_queued"}));
        assert!(matches!(s.transcript.last(), Some(TranscriptItem::Steer { queued: false, .. })));
    }

    #[test]
    fn esc_interrupts_then_second_esc_cancels() {
        let mut s = State::test_default();
        s.phase = Phase::Streaming { ticks: 0, chars: 0 };
        let cmds = press(&mut s, KeyCode::Esc);
        assert!(matches!(cmds[..], [Cmd::Cancel]));
        assert!(s.interrupt_sent);
        assert!(matches!(s.transcript.last(), Some(TranscriptItem::Notice { .. })), "state notes the interrupt");
        let cmds = press(&mut s, KeyCode::Esc);
        assert!(matches!(cmds[..], [Cmd::Cancel]));
        update(
            &mut s,
            Msg::PromptResult { outcome_kind: "cancelled".into(), outcome_text: None, usage: json!({}) },
        );
        assert!(s.transcript.iter().any(|i| matches!(i, TranscriptItem::Notice { text } if text.contains("cancel"))));
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
        assert!(matches!(s.transcript.last(), Some(TranscriptItem::Notice { text }) if text.contains("folded")));
        assert!(!matches!(s.phase, Phase::Compacting { .. }));
        upd(&mut s, json!({"type":"retrying","attempt":2,"reason":"overloaded"}));
        assert!(matches!(s.transcript.last(), Some(TranscriptItem::Notice { text }) if text.contains("overloaded")));
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
}
