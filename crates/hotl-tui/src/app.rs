//! The Elm core: `State` × `Msg` → mutations + `Cmd` effects. Pure — elapsed
//! time is tick counts (8/sec), never wall-clock, so every transition is
//! golden-testable.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use hotl_tools::ask::{Question, QuestionOption};
use serde_json::Value;

use crate::complete::{self, Completion};
use crate::paste;
use crate::select;
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
        /// The proposed change, when the server sent one. Empty for every ask
        /// today — see `hotl::diffgen::for_tool`'s unimplemented-invariant
        /// marker (RQ-2): the engine's ask carries no tool input to diff.
        diff: Vec<DiffLine>,
    },
    /// A structured `ask_user` question (tier-1 gap #4) — NOT a permission
    /// ask: answering it never authorizes a tool, it only supplies text the
    /// model reads. `input` is the free-text buffer; once the human starts
    /// typing, digit keys stop selecting and become ordinary characters.
    WaitingQuestion {
        req_id: u64,
        header: String,
        prompt: String,
        options: Vec<QuestionOption>,
        input: String,
    },
}

/// One row of a proposed change, as it arrives on the wire. The generator
/// lives in the runtime crate (`hotl::diffgen` — `write`'s "before" is a file
/// read); this crate only renders what it is handed, so the core stays pure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffOp {
    Ctx,
    Add,
    Del,
    /// The `[+N more lines]` trailer; never file content.
    Trailer,
}

impl DiffOp {
    pub fn from_wire(s: &str) -> Option<Self> {
        Some(match s {
            "ctx" => DiffOp::Ctx,
            "add" => DiffOp::Add,
            "del" => DiffOp::Del,
            "trailer" => DiffOp::Trailer,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub op: DiffOp,
    pub text: String,
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
    /// Model reasoning, when the provider returns it. Billed on every turn
    /// (`EngineConfig.thinking` defaults true) and, before this, never shown
    /// — T3-15. Collapsed to `view::THINKING_COLLAPSED_LINES` unless
    /// `State.thinking_expanded`. Its own variant rather than part of
    /// `Assistant`: the spine marker and style differ, and the transcript is
    /// what `Scroll::At` indexes, so conflating them would make thinking
    /// un-skippable. Empty deltas create no item — until R3 sends
    /// `thinking.display: "summarized"` the text really is empty.
    Thinking {
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

/// Running totals across every turn in this session. Per-turn usage is
/// overwritten by design (the strip shows one line); these are what a human
/// actually budgets against.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct SessionUsage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    /// Accumulated only across turns that reported a price. `None` means no
    /// turn ever did — the UI must then show nothing rather than `$0.00`.
    pub cost_usd: Option<f64>,
}

impl SessionUsage {
    /// Fold one turn's usage payload in. Absent keys count as zero, which is
    /// what `TokenUsage`'s own `#[serde(default)]` fields already mean.
    pub fn add(&mut self, usage: &Value) {
        let n = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
        self.input += n("input_tokens");
        self.output += n("output_tokens");
        self.cache_read += n("cache_read_input_tokens");
        if let Some(c) = usage.get("cost_usd").and_then(Value::as_f64) {
            *self.cost_usd.get_or_insert(0.0) += c;
        }
    }
}

/// The library's context window, used until the handshake reports the real
/// one (`initialize`'s `contextWindow`). Only a fallback: an older server
/// that does not report it still leaves the client rendering something
/// honest rather than dividing by zero.
pub const DEFAULT_CONTEXT_WINDOW: u64 = 200_000;

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
    /// Running totals across every turn, the basis of `usage_line`.
    pub session_usage: SessionUsage,
    pub help_open: bool,
    /// First Esc sent a cancel; suppresses duplicate notices until the result.
    pub interrupt_sent: bool,
    /// Turns the user detached from (second Esc) whose prompt results are
    /// still in flight. Everything a detached turn emits is absorbed until
    /// its result arrives and decrements this — the phase belongs to the
    /// user now, and nothing the dead turn says may take it back.
    pub detached_turns: u32,
    /// `tool_auto_allowed` arrives before its `tool_start`; the rule parks
    /// here until the card exists.
    pub pending_auto_rule: Option<String>,
    /// Display name (badge + titles); seeded from the open handshake,
    /// updated by `/rename`.
    pub session_name: Option<String>,
    /// Effective permission mode (`ask` | `auto` | `plan` | `dontask`).
    /// Seeded from the open handshake and corrected by every `mode_changed`
    /// notification, so it is what the engine enforces rather than what the
    /// user asked for. `/plan` and `/mode` update it optimistically; the
    /// notification is what makes an engine coercion visible.
    pub mode: String,
    /// Model context window in tokens, from the handshake. What the context
    /// gauge divides by; `DEFAULT_CONTEXT_WINDOW` until a server reports one.
    pub context_window: u64,
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
    /// Every completable `/` command: the built-ins, plus one row per skill
    /// name the handshake advertised. Built once at startup.
    pub commands: Vec<complete::Command>,
    /// The open completion popup, or `None`. Derived from the editor buffer
    /// after every keystroke — never a mode that can outlive what is typed.
    pub completion: Option<Completion>,
    /// Esc closed the popup; suppresses it until the buffer stops being a
    /// `/` command, so the next fresh slash opens it again.
    pub dismissed: bool,
    /// `Ctrl-T`: show model reasoning in full rather than collapsed.
    /// Reasoning is context for a decision, not the decision — collapsed is
    /// the default posture.
    pub thinking_expanded: bool,
    /// Compacted pastes riding the current draft (`paste::Attachment`),
    /// keyed positionally to their `[Image #N]` / `[Pasted text #N …]`
    /// tokens. Lives here rather than in `Editor` so `$EDITOR` round-trips
    /// and history recall (both replace the buffer via `set_text`) cannot
    /// orphan valid tokens. Cleared on every submit; a mangled token's
    /// entry is silently dropped at expansion (the orphan rule).
    pub attachments: Vec<paste::Attachment>,
    /// The live mouse drag, in *screen* cell coordinates rather than transcript
    /// offsets. Transient: it survives only until the next real user action
    /// (see the clearing rule at the top of `update`).
    pub selection: Option<select::Selection>,
    /// Lines copied by the last drag, shown in the hint until the next action
    /// clears it. There is no timer to expire it — the runtime's ticker is
    /// armed only while a turn runs, so an idle console would keep a timed
    /// notice forever.
    pub copy_notice: Option<usize>,
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
            session_usage: SessionUsage::default(),
            help_open: false,
            interrupt_sent: false,
            detached_turns: 0,
            pending_auto_rule: None,
            session_name: None,
            mode: "ask".into(),
            context_window: DEFAULT_CONTEXT_WINDOW,
            skills: Vec::new(),
            density: hotl_theme::Density::default(),
            todos: Vec::new(),
            commands: complete::builtins(),
            completion: None,
            dismissed: false,
            thinking_expanded: false,
            attachments: Vec::new(),
            selection: None,
            copy_notice: None,
        }
    }

    /// Seed the loadable-skill roster and the completion table from it.
    ///
    /// One path for both the open handshake and `/reload`, so `skills` (what
    /// `/<name>` dispatch resolves against) and `commands` (what the popup
    /// offers) can never disagree about which skills exist.
    pub fn set_skills(&mut self, skills: Vec<(String, String)>) {
        self.skills = skills.iter().map(|(name, _)| name.clone()).collect();
        self.commands = complete::builtins();
        self.commands.extend(
            skills
                .into_iter()
                .map(|(name, description)| complete::Command {
                    name,
                    description,
                    builtin: false,
                }),
        );
    }

    #[cfg(test)]
    pub(crate) fn test_default() -> Self {
        State::new(true, "test-model".into())
    }
}

#[derive(Debug, PartialEq)]
pub enum Msg {
    /// The `update` object from a `session/update` notification.
    Update(Value),
    PermissionRequest {
        req_id: u64,
        summary: String,
        protected_why: Option<String>,
        /// The proposed change, when the server sent one — empty for every
        /// ask until the engine's ask carries tool input (RQ-2).
        diff: Vec<DiffLine>,
    },
    QuestionRequest {
        req_id: u64,
        question: Question,
    },
    PromptResult {
        outcome_kind: String,
        outcome_text: Option<String>,
        usage: Value,
    },
    Key(KeyEvent),
    /// Bracketed-paste payload. Literal text, never keys — see `Msg::Key`.
    /// A multi-line paste used to arrive as one `Enter` per line and submit
    /// one turn per line.
    Paste(String),
    /// Transcript scroll from a key or the mouse wheel. Vim's `j`/`k` reach
    /// the same `scroll::apply` via `EditorEvent::Scroll*`.
    Scroll(crate::scroll::Intent),
    Tick,
    /// `$EDITOR` result; `None` = unchanged/aborted.
    EditorDone(Option<String>),
    /// Left button pressed: anchor a new selection at this cell.
    SelectStart {
        col: u16,
        row: u16,
    },
    /// Left button dragged: move the selection head. One per cell crossed.
    SelectExtend {
        col: u16,
        row: u16,
    },
    /// Left button released: copy, unless the drag never left its anchor.
    SelectEnd,
    /// The runtime finished a copy and reports how much reached the clipboard.
    /// `0` means the region held nothing worth copying.
    Copied {
        lines: usize,
    },
    /// The runtime re-read the client-side half of `config.toml`
    /// (`Cmd::ReloadSettings`). Theme, mouse and copy-on-select live in the
    /// runtime's own locals; these two live in `State`.
    SettingsReloaded {
        vim_mode: bool,
        density: hotl_theme::Density,
        warnings: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    /// The wire-bound draft: paste tokens already expanded, `[Image #N]`
    /// tokens inline, image paths riding with `data: None` until the
    /// runtime seam reads and encodes the files.
    SendPrompt(paste::PromptPayload),
    SendSteer(paste::PromptPayload),
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
    /// Answer a `session/request_question`. Exactly one of `selected`
    /// (a single label — v1 is single-select even when `multi` was set) or
    /// `free_text` is populated.
    ReplyQuestion {
        req_id: u64,
        selected: Vec<String>,
        free_text: Option<String>,
    },
    /// Send `session/reload_config` (fire-and-forget; the engine broadcasts
    /// `config_reloaded`, which is what the client actually acts on).
    ReloadConfig,
    /// Re-read the client-side half of `config.toml` — theme, density, vim
    /// mode, mouse. The runtime owns this one: `hotl-tui` never touches the
    /// filesystem.
    ReloadSettings,
    OpenEditor(String),
    SetTitle(String),
    /// Append a submitted prompt to the on-disk history file (the runtime
    /// owns the file; the core just names what to persist).
    AppendHistory(String),
    /// Copy this screen region to the clipboard. The core names the region;
    /// the runtime resolves it against the rendered buffer and writes OSC 52,
    /// then reports back as `Msg::Copied`.
    CopySelection(select::Selection),
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
    // A selection is a region of the *screen*, so any deliberate user action
    // retires it — but not the two message kinds that arrive on their own
    // schedule. Excluding `Update` is what lets a drag work mid-turn, and it
    // is safe to exclude: the highlight is painted at fixed cells and the copy
    // scrapes the live buffer, so the two agree even as text moves underneath.
    //
    // INVARIANT: a live drag survives arriving stream tokens. Enforced by
    // `streaming_updates_do_not_clear_a_live_drag`.
    if !matches!(
        &msg,
        Msg::SelectStart { .. }
            | Msg::SelectExtend { .. }
            | Msg::SelectEnd
            | Msg::Copied { .. }
            | Msg::Tick
            | Msg::Update(_)
    ) {
        state.selection = None;
        state.copy_notice = None;
    }
    // A detached turn (second Esc) is dead to the UI but alive on the wire
    // until its prompt result arrives. The wire is FIFO, so everything it
    // emits lands before that result: absorb it all here — except durable
    // session state — so nothing a dead turn says can reclaim the phase the
    // user took back. Its asks go unanswered on purpose: their reply channels
    // die with the cancelled turn and the server prunes them.
    if state.detached_turns > 0 {
        match &msg {
            Msg::Update(v) => {
                let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
                // The reload pair joins the durable-state exceptions: a
                // `/reload` issued after an esc-esc detach replaces the session
                // outright, and swallowing that would leave the badge, the
                // model and the skill roster describing an engine that is gone.
                if !matches!(
                    kind,
                    "mode_changed" | "todos_changed" | "config_reloaded" | "config_reload_failed"
                ) {
                    return Vec::new();
                }
            }
            Msg::PermissionRequest { .. } | Msg::QuestionRequest { .. } => return Vec::new(),
            Msg::PromptResult { usage, .. } => {
                state.detached_turns -= 1;
                state.session_usage.add(usage);
                return Vec::new();
            }
            _ => {}
        }
    }
    match msg {
        Msg::Update(v) => on_update(state, &v),
        Msg::PermissionRequest {
            req_id,
            summary,
            protected_why,
            diff,
        } => {
            state.phase = Phase::WaitingAsk {
                req_id,
                summary,
                protected_why,
                input: String::new(),
                denying: false,
                diff,
            };
            // The ask owns the keyboard and the screen now. A popup or a live
            // reverse-i-search left over from mid-typing would steal the first
            // Esc, draw a stale menu under the "waiting on you" card, and
            // advertise keys `on_ask_key` ignores (tracker #13).
            state.completion = None;
            state.editor.clear_search();
            vec![Cmd::SetTitle(title(state, " — waiting on you"))]
        }
        Msg::QuestionRequest { req_id, question } => {
            state.phase = Phase::WaitingQuestion {
                req_id,
                header: question.header,
                prompt: question.prompt,
                options: question.options,
                input: String::new(),
            };
            // Same reasoning as the ask arm above (tracker #13).
            state.completion = None;
            state.editor.clear_search();
            vec![Cmd::SetTitle(title(state, " — waiting on you"))]
        }
        Msg::PromptResult {
            outcome_kind,
            outcome_text,
            usage,
        } => on_prompt_result(state, &outcome_kind, outcome_text, &usage),
        Msg::Key(key) => on_key(state, key),
        Msg::Paste(text) => {
            // A dropped image path or a 3+-line paste compacts to a token;
            // the content parks in the side table until submit. Numbering is
            // per-kind and per-draft. Everything else inserts literally —
            // `insert_text`'s never-submits invariant carries over either
            // way (tokens contain no newline).
            match paste::classify(&text) {
                paste::PasteKind::Image { path, media_type } => {
                    let n = 1 + state
                        .attachments
                        .iter()
                        .filter(|a| matches!(a, paste::Attachment::Image { .. }))
                        .count();
                    state.editor.insert_text(&paste::image_marker(n));
                    state
                        .attachments
                        .push(paste::Attachment::Image { path, media_type });
                }
                paste::PasteKind::Text { text, lines } => {
                    let n = 1 + state
                        .attachments
                        .iter()
                        .filter(|a| matches!(a, paste::Attachment::Paste { .. }))
                        .count();
                    state.editor.insert_text(&paste::paste_marker(n, lines));
                    state
                        .attachments
                        .push(paste::Attachment::Paste { text, lines });
                }
                paste::PasteKind::Literal => state.editor.insert_text(&text),
            }
            refresh(state);
            Vec::new()
        }
        Msg::Scroll(intent) => {
            crate::scroll::apply(state, intent);
            Vec::new()
        }
        Msg::Tick => {
            on_tick(state);
            Vec::new()
        }
        Msg::EditorDone(content) => {
            if let Some(text) = content {
                state.editor.set_text(text.trim_end_matches('\n'));
                refresh(state);
            }
            Vec::new()
        }
        Msg::SelectStart { col, row } => {
            state.selection = Some(select::Selection::new(col, row));
            state.copy_notice = None;
            Vec::new()
        }
        Msg::SelectExtend { col, row } => {
            if let Some(sel) = &mut state.selection {
                sel.head = (col, row);
            }
            Vec::new()
        }
        // The highlight deliberately stays up after the copy — it is the only
        // confirmation of *what* was copied. The clearing rule above retires it
        // on the next action.
        Msg::SelectEnd => match state.selection {
            Some(sel) if !sel.is_empty() => vec![Cmd::CopySelection(sel)],
            _ => {
                state.selection = None;
                Vec::new()
            }
        },
        Msg::Copied { lines } => {
            state.copy_notice = (lines > 0).then_some(lines);
            Vec::new()
        }
        // The runtime already applied the settings it owns (theme, mouse,
        // copy-on-select); these two live here. `vim_mode` also has to reach
        // the editor, which holds its own copy.
        Msg::SettingsReloaded {
            vim_mode,
            density,
            warnings,
        } => {
            state.vim_mode = vim_mode;
            state.editor.set_vim_mode(vim_mode);
            state.density = density;
            for w in warnings {
                notice(state, w);
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
        // T3-15: thinking is billed on every turn and used to be dropped on
        // the floor (`_ => {}`). Deltas accumulate into one item so a burst
        // of reasoning is one collapsible block, not fifty.
        "thinking_delta" => {
            let delta = text_of("text");
            if !delta.is_empty() {
                match state.transcript.last_mut() {
                    Some(TranscriptItem::Thinking { text }) => text.push_str(&delta),
                    _ => state
                        .transcript
                        .push(TranscriptItem::Thinking { text: delta }),
                }
                enter_streaming(state);
            }
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
        // Server-side truth, not a client guess: the badge showed "ask" while
        // the shipped default ran "auto" (evaluation §5.7). Optimistic /mode
        // and /plan updates are corrected here when the engine coerces them
        // (a security-enforced build forces Auto→Ask).
        // INVARIANT: `state.mode` is what the engine enforces, never what the
        // user asked for. Enforced by `mode_changed_updates_the_badge_state`.
        "mode_changed" => state.mode = text_of("mode"),
        // `/reload` landed: the engine now runs a scaffold built from the
        // config on disk, and the session was re-opened onto it. Everything
        // here is server-side truth — the client re-seeds rather than guesses,
        // exactly as it does at the open handshake.
        "config_reloaded" => {
            state.model = text_of("model");
            state.mode = text_of("mode");
            if let Some(w) = v
                .get("context_window")
                .and_then(Value::as_u64)
                .filter(|&w| w > 0)
            {
                state.context_window = w;
            }
            state.set_skills(crate::client::parse_skills(v));
            for w in v
                .get("warnings")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                notice(state, w.to_string());
            }
            notice(
                state,
                format!(
                    "config reloaded — model {} · mode {} · {} skill(s). \
                     [sandbox], [network] and the thread pools are process-wide — restart to change those.",
                    state.model,
                    state.mode,
                    state.skills.len()
                ),
            );
        }
        // The rebuild failed (a typo in config.toml, an unreachable provider).
        // The engine deliberately kept running the old scaffold, and saying so
        // is the whole point: a silent failure here reads as "reloaded".
        "config_reload_failed" => notice(
            state,
            format!(
                "config reload failed: {} — the previous config is still live",
                text_of("reason")
            ),
        ),
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
    state.session_usage.add(usage);
    state.usage_line = Some(format_usage(state, usage));
    state.phase = Phase::Idle;
    state.interrupt_sent = false;
    vec![Cmd::SetTitle(title(state, ""))]
}

fn outcome_notice(kind: &str, text: Option<&str>) -> Option<String> {
    Some(match kind {
        "done" => return None,
        "cancelled" => "turn cancelled".into(),
        // Name the knob: hitting this looks like an unexplained stop, and the
        // fix ([behavior] max_turns, or -1 for no cap) is not guessable.
        "turn_limit" => "turn limit reached — raise [behavior] max_turns (-1 = no cap)".into(),
        "refused" => "provider refused the request".into(),
        other => format!("{other}: {}", text.unwrap_or(""))
            .trim_end_matches([':', ' '])
            .to_string(),
    })
}

/// Compact token count: verbatim below 1000, else one decimal with a `k`/`M`
/// suffix. The strip has one line to spend, so `12.0k` beats `12000`.
fn tok(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}k", n as f64 / 1_000.0),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

/// The strip's usage line: session totals, this turn's context fullness, and
/// cost when — and only when — the payload reported one.
///
/// Per-turn input/output is overwritten by design (the strip shows one line);
/// the *totals* are what a human actually budgets against. The context gauge
/// reads the latest turn, because that is what the next turn will carry.
///
/// INVARIANT: no cost segment is rendered unless the turn result carried
/// `cost_usd` — the UI never estimates prices. R4 owns the catalog that
/// populates it (see the plan's RQ table). Enforced by
/// `cost_is_shown_only_when_the_payload_carries_it`.
fn format_usage(state: &State, usage: &Value) -> String {
    let n = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    let u = &state.session_usage;
    let mut parts = vec![
        format!("{} in", tok(u.input)),
        format!("{} out", tok(u.output)),
    ];
    if u.cache_read > 0 {
        parts.push(format!("{} cached", tok(u.cache_read)));
    }
    // Per-turn, not accumulated (a session-wide average would blur a cold
    // first turn into a warm tenth one) — present only when this turn had
    // cache activity to report (§S1 cache telemetry).
    if let Some(ratio) = usage.get("hit_ratio").and_then(Value::as_f64) {
        parts.push(format!("{:.0}% hit", ratio * 100.0));
    }
    // What the *next* turn starts from: everything resident in this turn's
    // context, not the session's running total.
    let live = n("input_tokens") + n("cache_read_input_tokens") + n("cache_creation_input_tokens");
    if let Some(pct) = (live * 100).checked_div(state.context_window) {
        parts.push(format!("{}% ctx", pct.min(100)));
    }
    if let Some(cost) = u.cost_usd {
        parts.push(format!("${cost:.2}"));
    }
    parts.join(" · ")
}

/// True while something else must own the keyboard and the screen: a live
/// reverse-i-search (the input box is showing its prompt, not the buffer),
/// or a permission ask / structured question (`on_ask_key` / `on_question_key`
/// intercept every key before it ever reaches here). The popup must not be
/// shown, and must not intercept keys, in either case.
fn modal_active(state: &State) -> bool {
    state.editor.search_prompt().is_some()
        || matches!(
            state.phase,
            Phase::WaitingAsk { .. } | Phase::WaitingQuestion { .. }
        )
}

/// Recompute the popup from the editor buffer. Called after every key that
/// reaches the editor and after a splice, so the popup is always a function
/// of what is actually typed. A buffer that is no longer a `/` command
/// re-arms `dismissed` — that is the only thing that clears it. While
/// something else owns the keyboard (`modal_active`) the popup stays closed
/// regardless of what the buffer says.
fn refresh(state: &mut State) {
    if modal_active(state) {
        state.completion = None;
        return;
    }
    let text = state.editor.text();
    if !text.starts_with('/') {
        state.dismissed = false;
    }
    state.completion = complete::recompute(
        &state.commands,
        &text,
        state.editor.cursor(),
        state.dismissed,
    );
}

/// Splice the highlighted command into the buffer. The spliced text carries a
/// trailing space, so `refresh` closes the popup on the way out.
fn accept_selected(state: &mut State) {
    let Some(idx) = state
        .completion
        .as_ref()
        .and_then(|c| c.matches.get(c.selected).copied())
    else {
        return;
    };
    let Some(name) = state.commands.get(idx).map(|c| c.name.clone()) else {
        return;
    };
    let text = complete::accept(&state.editor.text(), state.editor.cursor(), &name);
    state.editor.set_text(&text);
    refresh(state);
}

fn on_key(state: &mut State, key: KeyEvent) -> Vec<Cmd> {
    // Ctrl-C outranks every transient owner of the keyboard (help overlay,
    // popup, modals): idle it quits, busy it interrupts, and once an
    // interrupt is pending — from either key — it quits outright, so two
    // presses always suffice to leave.
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        state.help_open = false;
        if state.phase == Phase::Idle || state.interrupt_sent {
            return vec![Cmd::Quit];
        }
        state.interrupt_sent = true;
        notice(state, "interrupting — ctrl-c again quits".into());
        return vec![Cmd::Cancel];
    }
    if state.help_open {
        state.help_open = false;
        return Vec::new();
    }
    // Ctrl-T expands model reasoning. Above the editor for the same reason as
    // the scroll keys: `Editor::handle` swallows every Ctrl chord it does not
    // itself bind.
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('t') {
        state.thinking_expanded = !state.thinking_expanded;
        return Vec::new();
    }
    if matches!(state.phase, Phase::WaitingAsk { .. }) {
        return on_ask_key(state, key);
    }
    if matches!(state.phase, Phase::WaitingQuestion { .. }) {
        return on_question_key(state, key);
    }
    // Transcript scrolling, unconditional — not gated on vim mode, which is
    // the whole defect (`vim.rs::vertical` was the only emitter and needs
    // `[behavior] vim_mode = true`). PageUp/PageDown have no meaning in a
    // ten-row input box, so they need no layering; Ctrl-Home/Ctrl-End is the
    // document-start/end convention, leaving bare Home/End as line motions.
    // This sits above the editor because the generic Ctrl swallow in
    // `Editor::handle` would otherwise eat Ctrl-Home.
    // INVARIANT: reachable with `[behavior] vim_mode = false`. Enforced by
    // `page_keys_scroll_the_transcript_without_vim_mode`.
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let intent = match key.code {
        KeyCode::PageUp => Some(crate::scroll::Intent::Up(crate::scroll::PAGE)),
        KeyCode::PageDown => Some(crate::scroll::Intent::Down(crate::scroll::PAGE)),
        KeyCode::Home if ctrl => Some(crate::scroll::Intent::Top),
        KeyCode::End if ctrl => Some(crate::scroll::Intent::Bottom),
        _ => None,
    };
    if let Some(intent) = intent {
        crate::scroll::apply(state, intent);
        return Vec::new();
    }
    if key.code == KeyCode::Esc && state.phase != Phase::Idle && state.editor.is_empty() {
        return interrupt_or_detach(state);
    }
    if key.code == KeyCode::Char('?') && state.editor.is_empty() {
        state.help_open = true;
        return Vec::new();
    }
    // The popup owns these four keys while it is open. Esc is layered — it
    // dismisses here and only reaches the editor's Insert→Normal transition
    // on a second press. Enter splices and then falls through, so submitting
    // takes its ordinary path.
    if let Some(c) = &state.completion {
        match key.code {
            KeyCode::Up | KeyCode::Down => {
                let last = c.matches.len().saturating_sub(1);
                let next = if key.code == KeyCode::Up {
                    c.selected.saturating_sub(1)
                } else {
                    (c.selected + 1).min(last)
                };
                if let Some(c) = &mut state.completion {
                    c.selected = next;
                }
                return Vec::new();
            }
            KeyCode::Esc => {
                state.dismissed = true;
                state.completion = None;
                return Vec::new();
            }
            KeyCode::Tab => {
                accept_selected(state);
                return Vec::new();
            }
            KeyCode::Enter => accept_selected(state),
            _ => {}
        }
    }
    let event = state.editor.handle(key);
    refresh(state);
    match event {
        EditorEvent::Submit(text) if text.trim().is_empty() => {
            // The editor already reset its buffer; stale attachments must
            // not leak their numbering into the next draft.
            state.attachments.clear();
            Vec::new()
        }
        EditorEvent::Submit(text) => {
            // Expand while the side table is alive — it dies with this
            // draft. The wire gets paste content + image paths; disk history
            // gets the fully-expanded text (exactly the bytes pre-compaction
            // behavior wrote, so a recalled entry is self-contained).
            let history_text = paste::expand_for_history(&text, &state.attachments);
            // INVARIANT: the recall ring holds fully-expanded text, so a
            // recalled prompt is self-contained. Enforced by
            // `recalling_an_image_prompt_replays_the_path_not_a_dead_token`.
            state.editor.remember(history_text.clone());
            let payload = paste::expand_for_wire(&text, &state.attachments);
            state.attachments.clear();
            let cmds = submit(state, text.clone(), payload);
            // Persist only prompt-starting submissions (they emit SendPrompt),
            // and only when the literal text wasn't a slash command — a skill
            // invocation desugars to a prompt but shouldn't leave its `/name`
            // (or the expanded template) on disk. In-session recall still walks
            // everything via the editor's own ring.
            let starts_turn = cmds.iter().any(|c| matches!(c, Cmd::SendPrompt(_)));
            if starts_turn && !text.trim_start().starts_with('/') {
                let mut out = vec![Cmd::AppendHistory(history_text)];
                out.extend(cmds);
                out
            } else {
                cmds
            }
        }
        EditorEvent::OpenExternal(text) => vec![Cmd::OpenEditor(text)],
        EditorEvent::ScrollUp => {
            crate::scroll::apply(state, crate::scroll::Intent::Up(1));
            Vec::new()
        }
        EditorEvent::ScrollDown => {
            crate::scroll::apply(state, crate::scroll::Intent::Down(1));
            Vec::new()
        }
        EditorEvent::None => Vec::new(),
    }
}

/// The Esc ladder: the first press asks the engine to cancel; the second
/// stops waiting for the answer. Control returns unconditionally — the
/// client must never depend on the server to hand the prompt line back.
fn interrupt_or_detach(state: &mut State) -> Vec<Cmd> {
    if !state.interrupt_sent {
        state.interrupt_sent = true;
        notice(state, "interrupting — esc again takes control back".into());
        return vec![Cmd::Cancel];
    }
    state.detached_turns += 1;
    state.phase = Phase::Idle;
    state.interrupt_sent = false;
    notice(
        state,
        "control is yours — the interrupted turn is abandoned".into(),
    );
    // One more cancel for the road: harmless if the turn is already dead,
    // decisive if the first one raced the wire.
    vec![Cmd::Cancel, Cmd::SetTitle(title(state, ""))]
}

/// The display/wire fork: the transcript keeps `text` (tokens and all — what
/// the human typed), the wire gets `payload` (tokens expanded, image paths
/// riding). A built-in slash command drops the payload; the skill desugar
/// re-uses its images.
fn submit(state: &mut State, text: String, payload: paste::PromptPayload) -> Vec<Cmd> {
    if let Some(rest) = text.trim().strip_prefix('/') {
        return slash_command(state, rest, payload);
    }
    if state.phase == Phase::Idle {
        state.transcript.push(TranscriptItem::User { text });
        state.phase = Phase::Sampling { ticks: 0 };
        state.scroll = Scroll::Follow;
        vec![
            Cmd::SendPrompt(payload),
            Cmd::SetTitle(title(state, " — working")),
        ]
    } else {
        state
            .transcript
            .push(TranscriptItem::Steer { text, queued: true });
        vec![Cmd::SendSteer(payload)]
    }
}

/// The TUI's slash commands. Built-ins resolve first; an unmatched
/// `/<skill>` asks the model to load that skill, which is the human
/// override for skills the tool description no longer names. Anything
/// else is a transcript notice — unresolved slash input never reaches the
/// model.
fn slash_command(state: &mut State, rest: &str, payload: paste::PromptPayload) -> Vec<Cmd> {
    let (cmd, arg) = rest
        .split_once(char::is_whitespace)
        .map(|(c, a)| (c, a.trim()))
        .unwrap_or((rest.trim(), ""));
    match cmd {
        "rename" => {
            // The one source of truth for what a session name may be — the
            // same function `acp.rs` validates against, so the TUI and the
            // wire can never disagree (this file already imports
            // `hotl_tools::rules::PermissionMode` for exactly that reason).
            let Some(name) = hotl_types::normalize_session_name(arg) else {
                notice(state, "usage: /rename <name> (1–64 chars)".into());
                return Vec::new();
            };
            state.session_name = Some(name.clone());
            notice(state, format!("session renamed to {name}"));
            let suffix = if state.phase == Phase::Idle {
                ""
            } else {
                " — working"
            };
            vec![Cmd::Rename(name), Cmd::SetTitle(title(state, suffix))]
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
        // Re-read `config.toml` without losing the session. The settings half
        // goes first so the theme flips at once while the engine rebuild — a
        // provider handshake and a skill walk — is still in flight.
        //
        // Idle-only, deliberately: a rebuild replaces the session, and the
        // reply a running turn is mid-way through producing would die with it.
        // Abandoning a turn stays the user's call (the esc ladder), never a
        // side effect of a command about configuration.
        "reload" => {
            if state.phase != Phase::Idle {
                notice(
                    state,
                    "/reload needs an idle session — finish the turn or press esc twice".into(),
                );
                return Vec::new();
            }
            notice(state, "reloading config…".into());
            vec![Cmd::ReloadSettings, Cmd::ReloadConfig]
        }
        // `?` only opens help while the buffer is empty, so the moment you
        // have typed anything help is unreachable — a discoverability bug,
        // not a new feature.
        "help" => {
            state.help_open = true;
            Vec::new()
        }
        // The single highest-value "what am I actually running?" answer, and
        // exactly the state the §5.7 mode bug proved users could not see.
        "status" => {
            let name = state.session_name.as_deref().unwrap_or("(unnamed)");
            let todos = state.todos.len();
            notice(
                state,
                format!(
                    "{name} · model {} · mode {} · context {} tok · {todos} todo(s)",
                    state.model, state.mode, state.context_window
                ),
            );
            Vec::new()
        }
        // The strip shows a compact line; this prints the breakdown without
        // stealing strip width.
        "cost" => {
            let u = state.session_usage;
            let mut text = format!(
                "session: {} in · {} out · {} cached",
                tok(u.input),
                tok(u.output),
                tok(u.cache_read)
            );
            match u.cost_usd {
                Some(c) => text.push_str(&format!(" · ${c:.2}")),
                // R4 owns the price catalog; inventing a number here would be
                // worse than saying nothing.
                None => text.push_str(" · cost not reported by the provider"),
            }
            notice(state, text);
            Vec::new()
        }
        // The transcript is a projection, so clearing the *view* is safe and
        // client-side. The notice must say so: a user who thinks this
        // truncated the model's context is worse off than one who never ran it.
        "clear" => {
            state.transcript.clear();
            state.scroll = Scroll::Follow;
            notice(
                state,
                "cleared the transcript view — the session log and the model's context are untouched"
                    .into(),
            );
            Vec::new()
        }
        "quit" => vec![Cmd::Quit],
        other if state.skills.iter().any(|s| s == other) => {
            // Desugars to an ordinary prompt: the model calls the skill tool,
            // so the TUI never reads skill files itself. The ARGUMENTS come
            // from the expanded payload, not the raw buffer, so pastes arrive
            // as content and image markers keep pointing at real attachments.
            let expanded_arg = payload
                .text
                .trim()
                .strip_prefix('/')
                .and_then(|r| r.split_once(char::is_whitespace))
                .map(|(_, a)| a.trim())
                .unwrap_or("");
            let mut text = format!("Load the skill `{other}` and follow it for this task.");
            if !expanded_arg.is_empty() {
                text.push_str(&format!("\n\nARGUMENTS: {expanded_arg}"));
            }
            let payload = paste::PromptPayload {
                text: text.clone(),
                images: payload.images,
            };
            submit(state, text, payload)
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
        // The modal is the model waiting on you; wanting out of it is
        // wanting the turn gone. Same ladder as the plain-editor Esc.
        KeyCode::Esc => interrupt_or_detach(state),
        _ => Vec::new(),
    }
}

/// `ask_user`'s modal (tier-1 gap #4): number keys 1-N pick an option
/// instantly (submits right away — no confirm step, matching `on_ask_key`'s
/// `y`); any other printable character starts free text instead (typing
/// commits to free text — once `input` is non-empty, digits are just more
/// text, never a late option pick). Esc while typing free text backs out to
/// the picker rather than submitting a partial answer.
fn on_question_key(state: &mut State, key: KeyEvent) -> Vec<Cmd> {
    let Phase::WaitingQuestion {
        req_id,
        options,
        input,
        ..
    } = &mut state.phase
    else {
        return Vec::new();
    };
    let req_id = *req_id;
    if !input.is_empty() {
        match key.code {
            KeyCode::Char(c) => input.push(c),
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Esc => input.clear(),
            KeyCode::Enter => {
                let text = input.clone();
                return resume_after_question(state, req_id, Vec::new(), Some(text));
            }
            _ => {}
        }
        return Vec::new();
    }
    match key.code {
        KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
            let idx = c as usize - '1' as usize;
            if let Some(opt) = options.get(idx) {
                let label = opt.label.clone();
                return resume_after_question(state, req_id, vec![label], None);
            }
        }
        KeyCode::Char(c) => input.push(c),
        // Same ladder as the ask picker: Esc with nothing typed is "I want
        // the turn gone", not a dead key.
        KeyCode::Esc => return interrupt_or_detach(state),
        _ => {}
    }
    Vec::new()
}

fn resume_after_question(
    state: &mut State,
    req_id: u64,
    selected: Vec<String>,
    free_text: Option<String>,
) -> Vec<Cmd> {
    state.phase = Phase::Sampling { ticks: 0 };
    vec![
        Cmd::ReplyQuestion {
            req_id,
            selected,
            free_text,
        },
        Cmd::SetTitle(title(state, " — working")),
    ]
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
        Phase::Sampling { ticks } | Phase::Streaming { ticks, .. } => *ticks += 1,
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
        Phase::Idle | Phase::WaitingAsk { .. } | Phase::WaitingQuestion { .. } => {}
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

    /// A non-`Char` key with modifiers (Ctrl-Home, Ctrl-End, …).
    fn press_mod(s: &mut State, code: KeyCode, mods: KeyModifiers) -> Vec<Cmd> {
        update(s, Msg::Key(KeyEvent::new(code, mods)))
    }

    fn notices(n: usize) -> Vec<TranscriptItem> {
        (0..n)
            .map(|i| TranscriptItem::Notice {
                text: i.to_string(),
            })
            .collect()
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
                diff: Vec::new(),
            },
        );
    }

    fn on_result(s: &mut State, kind: &str, text: Option<String>, usage: &Value) -> Vec<Cmd> {
        update(
            s,
            Msg::PromptResult {
                outcome_kind: kind.into(),
                outcome_text: text,
                usage: usage.clone(),
            },
        )
    }

    #[test]
    fn the_usage_line_shows_cache_reads_and_session_totals() {
        let mut s = State::test_default();
        s.context_window = 200_000;
        let usage = |i, o, c| {
            json!({
                "input_tokens": i, "output_tokens": o,
                "cache_read_input_tokens": c, "cache_creation_input_tokens": 0
            })
        };
        on_result(&mut s, "done", None, &usage(1_000, 500, 4_000));
        on_result(&mut s, "done", None, &usage(2_000, 500, 8_000));

        let line = s.usage_line.clone().unwrap();
        assert!(line.contains("3.0k in"), "session totals: {line}");
        assert!(line.contains("1.0k out"), "{line}");
        assert!(
            line.contains("12.0k cached"),
            "cache reads must show: {line}"
        );
        // (2_000 + 8_000) / 200_000 of the window is live in the latest turn.
        assert!(line.contains("5% ctx"), "context gauge: {line}");
    }

    #[test]
    fn hit_ratio_percentage_shows_when_present() {
        let mut s = State::test_default();
        s.context_window = 200_000;
        on_result(
            &mut s,
            "done",
            None,
            &json!({
                "input_tokens": 25, "output_tokens": 5,
                "cache_read_input_tokens": 50, "cache_creation_input_tokens": 25,
                "hit_ratio": 0.5
            }),
        );
        let line = s.usage_line.clone().unwrap();
        assert!(line.contains("50% hit"), "hit ratio must show: {line}");
    }

    #[test]
    fn hit_ratio_is_omitted_when_the_payload_carries_none() {
        let mut s = State::test_default();
        on_result(&mut s, "done", None, &json!({"input_tokens": 10}));
        assert!(
            !s.usage_line.as_ref().unwrap().contains("hit"),
            "no cache activity in the payload, no hit-ratio segment: {:?}",
            s.usage_line
        );
    }

    #[test]
    fn cost_is_shown_only_when_the_payload_carries_it() {
        let mut s = State::test_default();
        on_result(&mut s, "done", None, &json!({"input_tokens": 10}));
        assert!(
            !s.usage_line.as_ref().unwrap().contains('$'),
            "no invented prices"
        );

        on_result(
            &mut s,
            "done",
            None,
            &json!({"input_tokens": 10, "cost_usd": 0.0123}),
        );
        assert!(s.usage_line.as_ref().unwrap().contains("$0.01"));
    }

    #[test]
    fn the_context_gauge_is_omitted_without_a_window() {
        let mut s = State::test_default();
        s.context_window = 0;
        on_result(&mut s, "done", None, &json!({"input_tokens": 10}));
        let line = s.usage_line.clone().unwrap();
        assert!(!line.contains("ctx"), "no window, no gauge: {line}");
    }

    #[test]
    fn thinking_deltas_accumulate_into_one_item() {
        let mut s = State::test_default();
        upd(&mut s, json!({"type": "thinking_delta", "text": "first "}));
        upd(&mut s, json!({"type": "thinking_delta", "text": "second"}));
        assert_eq!(
            s.transcript,
            vec![TranscriptItem::Thinking {
                text: "first second".into()
            }]
        );
        // Text after thinking starts a separate assistant item.
        upd(&mut s, json!({"type": "text_delta", "text": "answer"}));
        assert_eq!(s.transcript.len(), 2);
    }

    #[test]
    fn empty_thinking_deltas_create_nothing() {
        // Until R3 sends `thinking.display: "summarized"` the deltas are empty
        // — that must render as nothing, not as an empty dimmed block.
        let mut s = State::test_default();
        upd(&mut s, json!({"type": "thinking_delta", "text": ""}));
        assert!(s.transcript.is_empty());
    }

    #[test]
    fn ctrl_t_toggles_thinking_expansion() {
        let mut s = State::test_default();
        assert!(!s.thinking_expanded);
        ctrl(&mut s, 't');
        assert!(s.thinking_expanded);
        ctrl(&mut s, 't');
        assert!(!s.thinking_expanded);
    }

    #[test]
    fn a_modal_transition_clears_the_editor_search() {
        let mut s = State::test_default();
        s.editor.load_history(vec!["cargo test".into()]);
        s.editor
            .handle(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
        assert!(s.editor.search_prompt().is_some());
        update(
            &mut s,
            Msg::PermissionRequest {
                req_id: 1,
                summary: "write ./x".into(),
                protected_why: None,
                diff: Vec::new(),
            },
        );
        assert!(
            s.editor.search_prompt().is_none(),
            "search survived the ask"
        );
    }

    #[test]
    fn mode_changed_updates_the_badge_state() {
        let mut s = State::test_default();
        assert_eq!(s.mode, "ask");
        upd(&mut s, json!({"type": "mode_changed", "mode": "auto"}));
        assert_eq!(s.mode, "auto");
    }

    #[test]
    fn a_multiline_paste_fires_no_turns() {
        let mut s = State::new(false, "m".into());
        let cmds = update(&mut s, Msg::Paste("a\nb\nc\nd".into()));
        assert!(cmds.is_empty(), "paste must not emit SendPrompt: {cmds:?}");
        // 3+ lines compact to a token; the content parks in the side table.
        assert_eq!(s.editor.text(), "[Pasted text #1 +4 lines]");
        assert_eq!(
            s.attachments,
            vec![paste::Attachment::Paste {
                text: "a\nb\nc\nd".into(),
                lines: 4
            }]
        );
        assert_eq!(s.phase, Phase::Idle);
        assert!(s.transcript.is_empty());
    }

    #[test]
    fn a_two_line_paste_stays_literal() {
        let mut s = State::new(false, "m".into());
        update(&mut s, Msg::Paste("a\nb".into()));
        assert_eq!(s.editor.text(), "a\nb");
        assert!(s.attachments.is_empty());
    }

    #[test]
    fn an_image_path_paste_compacts_to_a_token() {
        let mut s = State::new(false, "m".into());
        // The dropped form: escaped space, trailing space — one smoke case;
        // the full form table lives in paste.rs.
        update(&mut s, Msg::Paste("/tmp/My\\ Shot.png ".into()));
        assert_eq!(s.editor.text(), "[Image #1]");
        assert_eq!(
            s.attachments,
            vec![paste::Attachment::Image {
                path: "/tmp/My Shot.png".into(),
                media_type: "image/png".into()
            }]
        );
    }

    #[test]
    fn submit_expands_pastes_ships_image_paths_and_clears_the_table() {
        let mut s = State::new(false, "m".into());
        update(&mut s, Msg::Paste("/tmp/shot.png".into()));
        type_str(&mut s, " and ");
        update(&mut s, Msg::Paste("x\ny\nz".into()));
        let cmds = press(&mut s, KeyCode::Enter);
        // Transcript shows what the human typed, tokens and all.
        assert!(matches!(
            s.transcript.last(),
            Some(TranscriptItem::User { text })
                if text == "[Image #1] and [Pasted text #1 +3 lines]"
        ));
        // History gets the fully-expanded bytes; the wire gets paste content
        // plus the image path with data unfilled (the runtime seam's job).
        let [Cmd::AppendHistory(h), Cmd::SendPrompt(p), Cmd::SetTitle(_)] = &cmds[..] else {
            panic!("unexpected cmds: {cmds:?}");
        };
        assert_eq!(h, "/tmp/shot.png and x\ny\nz");
        assert_eq!(p.text, "[Image #1] and x\ny\nz");
        assert_eq!(p.images.len(), 1);
        assert_eq!(p.images[0].path, "/tmp/shot.png");
        assert_eq!(p.images[0].data, None);
        assert!(s.attachments.is_empty(), "the table dies with the draft");
    }

    #[test]
    fn recalling_an_image_prompt_replays_the_path_not_a_dead_token() {
        let mut s = State::new(false, "m".into());
        update(&mut s, Msg::Paste("/tmp/shot.png".into()));
        type_str(&mut s, " what is this?");
        press(&mut s, KeyCode::Enter);
        // Up recalls the EXPANDED prompt, so a re-submit is self-contained.
        press(&mut s, KeyCode::Up);
        assert_eq!(s.editor.text(), "/tmp/shot.png what is this?");
    }

    #[test]
    fn markers_number_per_kind_and_reset_after_submit() {
        let mut s = State::new(false, "m".into());
        update(&mut s, Msg::Paste("/a/1.png".into()));
        update(&mut s, Msg::Paste("l1\nl2\nl3".into()));
        update(&mut s, Msg::Paste("/a/2.png".into()));
        assert_eq!(
            s.editor.text(),
            "[Image #1][Pasted text #1 +3 lines][Image #2]"
        );
        press(&mut s, KeyCode::Enter);
        // A fresh draft numbers from #1 again.
        update(&mut s, Msg::Paste("/a/3.png".into()));
        assert_eq!(s.editor.text(), "[Image #1]");
        assert_eq!(s.attachments.len(), 1);
    }

    #[test]
    fn a_mangled_token_submits_literally_and_drops_the_orphan() {
        let mut s = State::new(false, "m".into());
        update(&mut s, Msg::Paste("/a/b.png".into()));
        // The human deletes a char INSIDE the token (backspace at its end
        // would swallow it whole — that path has its own vim.rs test).
        s.editor.cursor_to((0, 9));
        press(&mut s, KeyCode::Backspace);
        let cmds = press(&mut s, KeyCode::Enter);
        let Some(Cmd::SendPrompt(p)) = cmds.iter().find(|c| matches!(c, Cmd::SendPrompt(_))) else {
            panic!("expected a prompt: {cmds:?}");
        };
        assert_eq!(p.text, "[Image #]");
        assert!(p.images.is_empty(), "the orphan must not ship");
    }

    #[test]
    fn a_steer_carries_images_identically() {
        let mut s = State::new(false, "m".into());
        s.phase = Phase::Sampling { ticks: 0 };
        update(&mut s, Msg::Paste("/a/b.png".into()));
        let cmds = press(&mut s, KeyCode::Enter);
        let [Cmd::SendSteer(p)] = &cmds[..] else {
            panic!("expected a steer: {cmds:?}");
        };
        assert_eq!(p.text, "[Image #1]");
        assert_eq!(p.images[0].path, "/a/b.png");
    }

    #[test]
    fn an_empty_submit_clears_stale_attachments() {
        let mut s = State::new(false, "m".into());
        update(&mut s, Msg::Paste("/a/b.png".into()));
        // Delete the whole token, then submit the empty buffer.
        for _ in 0.."[Image #1]".len() {
            press(&mut s, KeyCode::Backspace);
        }
        press(&mut s, KeyCode::Enter);
        assert!(s.attachments.is_empty());
    }

    #[test]
    fn editor_roundtrip_keeps_attachments() {
        let mut s = State::new(false, "m".into());
        update(&mut s, Msg::Paste("/a/b.png".into()));
        // `$EDITOR` replaces the whole buffer via set_text; the token
        // survives textually and the side table lives in State, so the
        // attachment still ships at submit.
        let cmds = update(
            &mut s,
            Msg::EditorDone(Some("look: [Image #1] please".into())),
        );
        assert!(cmds.is_empty(), "{cmds:?}");
        let cmds = press(&mut s, KeyCode::Enter);
        let Some(Cmd::SendPrompt(p)) = cmds.iter().find(|c| matches!(c, Cmd::SendPrompt(_))) else {
            panic!("expected a prompt: {cmds:?}");
        };
        assert_eq!(p.images.len(), 1);
        assert_eq!(p.images[0].path, "/a/b.png");
    }

    #[test]
    fn page_keys_scroll_the_transcript_without_vim_mode() {
        // The regression: scroll used to be reachable only from vim Normal
        // mode, which `[behavior] vim_mode = false` (the default) makes
        // unreachable.
        let mut s = State::new(false, "m".into());
        s.transcript = notices(30);
        press(&mut s, KeyCode::PageUp);
        assert_eq!(s.scroll, Scroll::At(20));
        press_mod(&mut s, KeyCode::Home, KeyModifiers::CONTROL);
        assert_eq!(s.scroll, Scroll::At(0));
        press_mod(&mut s, KeyCode::End, KeyModifiers::CONTROL);
        assert_eq!(s.scroll, Scroll::Follow);
        press(&mut s, KeyCode::PageDown);
        assert_eq!(s.scroll, Scroll::Follow);
        assert!(s.editor.text().is_empty(), "scroll keys must not type");
    }

    #[test]
    fn bare_home_and_end_are_line_motions_not_scroll() {
        let mut s = State::new(false, "m".into());
        s.transcript = notices(30);
        s.editor.set_text("hello world");
        press(&mut s, KeyCode::Home);
        assert_eq!(s.editor.cursor(), (0, 0));
        assert_eq!(s.scroll, Scroll::Follow, "bare Home must not scroll");
        press(&mut s, KeyCode::End);
        assert_eq!(s.editor.cursor(), (0, 11));
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
                diff: Vec::new(),
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

    fn question(s: &mut State) {
        update(
            s,
            Msg::QuestionRequest {
                req_id: 9,
                question: Question {
                    header: "Scope".into(),
                    prompt: "How far?".into(),
                    options: vec![
                        QuestionOption {
                            label: "MVP".into(),
                            description: None,
                        },
                        QuestionOption {
                            label: "Full".into(),
                            description: Some("everything".into()),
                        },
                    ],
                    multi: false,
                },
            },
        );
    }

    #[test]
    fn question_request_freezes_into_waiting_question_with_the_options() {
        let mut s = State::test_default();
        s.phase = Phase::Idle;
        question(&mut s);
        assert!(matches!(
            &s.phase,
            Phase::WaitingQuestion { req_id: 9, header, options, .. }
            if header == "Scope" && options.len() == 2
        ));
        update(&mut s, Msg::Tick);
        assert!(
            matches!(s.phase, Phase::WaitingQuestion { .. }),
            "the loop halts on a question exactly like a permission ask"
        );
    }

    #[test]
    fn selecting_option_index_by_digit_emits_the_answer_with_that_label() {
        let mut s = State::test_default();
        question(&mut s);
        let cmds = press(&mut s, KeyCode::Char('2'));
        assert!(matches!(
            &cmds[..],
            [Cmd::ReplyQuestion { req_id: 9, selected, free_text: None }, ..]
            if selected == &vec!["Full".to_string()]
        ));
        assert!(!matches!(s.phase, Phase::WaitingQuestion { .. }));
    }

    #[test]
    fn typing_instead_of_a_digit_switches_to_free_text_and_enter_submits_it() {
        let mut s = State::test_default();
        question(&mut s);
        type_str(&mut s, "neither, do it differently");
        let cmds = press(&mut s, KeyCode::Enter);
        assert!(matches!(
            &cmds[..],
            [Cmd::ReplyQuestion { req_id: 9, selected, free_text: Some(t) }, ..]
            if selected.is_empty() && t == "neither, do it differently"
        ));
        assert!(!matches!(s.phase, Phase::WaitingQuestion { .. }));
    }

    #[test]
    fn a_digit_out_of_range_is_a_no_op_and_a_digit_after_typing_is_just_text() {
        let mut s = State::test_default();
        question(&mut s);
        // Only 2 options: '9' is out of range and does not submit.
        let cmds = press(&mut s, KeyCode::Char('9'));
        assert!(cmds.is_empty());
        assert!(matches!(s.phase, Phase::WaitingQuestion { .. }));

        // Once free text has started, a digit is just another character.
        type_str(&mut s, "opt");
        press(&mut s, KeyCode::Char('2'));
        let cmds = press(&mut s, KeyCode::Enter);
        assert!(matches!(
            &cmds[..],
            [Cmd::ReplyQuestion { free_text: Some(t), .. }, ..] if t == "opt2"
        ));
    }

    #[test]
    fn typing_mid_turn_queues_steer() {
        let mut s = State::test_default();
        s.phase = Phase::Streaming { ticks: 0, chars: 0 };
        type_str(&mut s, "wait");
        let cmds = press(&mut s, KeyCode::Enter);
        assert!(matches!(&cmds[..], [Cmd::SendSteer(t)] if t.text == "wait"));
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
    fn esc_interrupts_then_second_esc_takes_control_back() {
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
        assert_eq!(s.phase, Phase::Idle, "the second esc hands the prompt back");
        assert!(!s.interrupt_sent);
        assert_eq!(s.detached_turns, 1);
        assert!(cmds.contains(&Cmd::Cancel), "the dying turn is still told");
        assert!(
            matches!(cmds.last(), Some(Cmd::SetTitle(t)) if t == "hotl"),
            "the working suffix is dropped: {cmds:?}"
        );
    }

    /// The wire is FIFO, so everything a detached turn emits arrives before
    /// its prompt result. None of it may touch the phase the user took back —
    /// only durable session state (mode, todos) still lands.
    #[test]
    fn a_detached_turns_updates_and_asks_cannot_reclaim_the_screen() {
        let mut s = State::test_default();
        s.phase = Phase::Streaming { ticks: 0, chars: 0 };
        press(&mut s, KeyCode::Esc);
        press(&mut s, KeyCode::Esc);
        let items = s.transcript.len();
        upd(&mut s, json!({"type":"text_delta","text":"zombie"}));
        assert_eq!(
            s.phase,
            Phase::Idle,
            "a dead turn's delta restarted the spinner"
        );
        assert_eq!(s.transcript.len(), items);
        update(
            &mut s,
            Msg::PermissionRequest {
                req_id: 9,
                summary: "write ./x".into(),
                protected_why: None,
                diff: Vec::new(),
            },
        );
        assert_eq!(s.phase, Phase::Idle, "a dead turn's ask opened a modal");
        upd(&mut s, json!({"type":"mode_changed","mode":"plan"}));
        assert_eq!(s.mode, "plan", "durable session state still lands");
    }

    /// After a detach the old turn's result must be absorbed — usage folds
    /// into the session totals (those tokens were billed) but the phase
    /// belongs to whatever the user is doing now.
    #[test]
    fn a_detached_turns_late_result_is_absorbed_without_clobbering_a_new_turn() {
        let mut s = State::test_default();
        s.phase = Phase::Streaming { ticks: 0, chars: 0 };
        press(&mut s, KeyCode::Esc);
        press(&mut s, KeyCode::Esc);
        type_str(&mut s, "hi");
        press(&mut s, KeyCode::Enter);
        assert!(matches!(s.phase, Phase::Sampling { .. }));
        let cmds = on_result(&mut s, "cancelled", None, &json!({"input_tokens": 7}));
        assert!(
            matches!(s.phase, Phase::Sampling { .. }),
            "the dead turn's result yanked the new turn back to idle"
        );
        assert_eq!(s.detached_turns, 0);
        assert_eq!(s.session_usage.input, 7, "billed tokens still count");
        assert!(
            cmds.is_empty(),
            "no title/notice churn for an abandoned turn"
        );
        // The next result is the live turn's and lands normally.
        on_result(&mut s, "done", None, &json!({}));
        assert_eq!(s.phase, Phase::Idle);
    }

    #[test]
    fn ctrl_c_escalates_to_quit_on_the_second_press() {
        let mut s = State::test_default();
        s.phase = Phase::Streaming { ticks: 0, chars: 0 };
        assert!(matches!(ctrl(&mut s, 'c')[..], [Cmd::Cancel]));
        assert!(s.interrupt_sent, "the first ctrl-c is an interrupt");
        assert!(matches!(ctrl(&mut s, 'c')[..], [Cmd::Quit]));
    }

    #[test]
    fn ctrl_c_after_an_esc_interrupt_quits() {
        let mut s = State::test_default();
        s.phase = Phase::Streaming { ticks: 0, chars: 0 };
        press(&mut s, KeyCode::Esc);
        assert!(matches!(ctrl(&mut s, 'c')[..], [Cmd::Quit]));
    }

    #[test]
    fn ctrl_c_is_never_swallowed_by_the_help_overlay() {
        let mut s = State::test_default();
        s.help_open = true;
        assert!(matches!(ctrl(&mut s, 'c')[..], [Cmd::Quit]));
        assert!(!s.help_open);
    }

    /// Esc in the ask picker joins the same ladder: the modal is the model
    /// waiting on you, and wanting out of it is wanting the turn gone.
    #[test]
    fn esc_interrupts_from_the_ask_picker_and_detaches_on_the_second_press() {
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 0 };
        ask(&mut s);
        let cmds = press(&mut s, KeyCode::Esc);
        assert!(matches!(cmds[..], [Cmd::Cancel]));
        press(&mut s, KeyCode::Esc);
        assert_eq!(s.phase, Phase::Idle, "the second esc closes the modal too");
    }

    #[test]
    fn esc_interrupts_from_the_question_picker() {
        let mut s = State::test_default();
        update(
            &mut s,
            Msg::QuestionRequest {
                req_id: 3,
                question: Question {
                    header: "Pick".into(),
                    prompt: "which?".into(),
                    options: vec![],
                    multi: false,
                },
            },
        );
        let cmds = press(&mut s, KeyCode::Esc);
        assert!(matches!(cmds[..], [Cmd::Cancel]));
        assert!(s.interrupt_sent);
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
        // Session totals plus the context gauge; no cache segment (this turn
        // read none) and no cost (the payload carried none).
        assert_eq!(s.usage_line.as_deref(), Some("120 in · 45 out · 0% ctx"));
        assert!(matches!(&cmds[..], [Cmd::SetTitle(t)] if t == "hotl"));
    }

    /// Dead animation state: the compacting phase was defined, ticked,
    /// exited, and animated, but never assigned outside tests (evaluation §7)
    /// — the engine emits only `Compacted { degraded }`, a *completion*
    /// signal. It is gone until the engine emits a compaction-*start* event
    /// (this plan's RQ-3); the frames are preserved in the plan's Task 12 so
    /// restoring it is a copy-paste.
    #[test]
    fn no_unreachable_phase_variants() {
        let src = include_str!("app.rs");
        // Split so this assertion is not its own counter-example.
        let needle = concat!("Compact", "ing");
        assert!(
            !src.contains(needle),
            "a phase nothing assigns must not ship — see RQ-3"
        );
    }

    #[test]
    fn compacted_and_retrying_become_notices() {
        let mut s = State::test_default();
        upd(&mut s, json!({"type":"compacted","degraded":false}));
        assert!(
            matches!(s.transcript.last(), Some(TranscriptItem::Notice { text }) if text.contains("folded"))
        );
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

    fn with_skills(names: &[(&str, &str)]) -> State {
        let mut s = State::test_default();
        for (name, description) in names {
            s.commands.push(crate::complete::Command {
                name: (*name).into(),
                description: (*description).into(),
                builtin: false,
            });
            s.skills.push((*name).into());
        }
        s
    }

    fn selected(s: &State) -> String {
        let c = s.completion.as_ref().expect("popup open");
        s.commands[c.matches[c.selected]].name.clone()
    }

    #[test]
    fn typing_a_slash_opens_the_popup_and_narrows_as_you_type() {
        let mut s = with_skills(&[("review", "review a pull request")]);
        type_str(&mut s, "/");
        // Nine built-ins plus the one skill.
        assert_eq!(s.completion.as_ref().map(|c| c.matches.len()), Some(10));
        type_str(&mut s, "re");
        assert_eq!(selected(&s), "reload");
        // `reload`, `rename` and `review` prefix-match; no other built-in
        // contains "re".
        assert_eq!(s.completion.as_ref().map(|c| c.matches.len()), Some(3));
    }

    #[test]
    fn arrows_move_the_selection_and_saturate_at_both_ends() {
        let mut s = with_skills(&[("review", "review a pull request")]);
        type_str(&mut s, "/re");
        press(&mut s, KeyCode::Down);
        assert_eq!(selected(&s), "rename");
        assert_eq!(
            s.editor.text(),
            "/re",
            "the popup owns the arrows while open — no history recall, no buffer change"
        );
        press(&mut s, KeyCode::Up);
        press(&mut s, KeyCode::Up);
        assert_eq!(selected(&s), "reload", "up saturates at the top");
        for _ in 0..10 {
            press(&mut s, KeyCode::Down);
        }
        assert_eq!(selected(&s), "review", "down saturates at the bottom");
    }

    #[test]
    fn tab_completes_the_selection_without_starting_a_turn() {
        let mut s = with_skills(&[("review", "review a pull request")]);
        type_str(&mut s, "/re");
        let cmds = press(&mut s, KeyCode::Tab);
        assert!(cmds.is_empty(), "tab is not a submit: {cmds:?}");
        assert_eq!(s.editor.text(), "/reload ");
        assert!(
            s.completion.is_none(),
            "the trailing space closes the popup"
        );
        assert_eq!(s.phase, Phase::Idle);
    }

    #[test]
    fn enter_runs_the_highlighted_command_not_the_literal_text() {
        let mut s = with_skills(&[("review", "review a pull request")]);
        type_str(&mut s, "/pl");
        let cmds = press(&mut s, KeyCode::Enter);
        assert!(
            matches!(&cmds[..], [Cmd::SetMode(m)] if m == "plan"),
            "got {cmds:?}"
        );
        assert!(
            !matches!(s.transcript.last(), Some(TranscriptItem::Notice { text }) if text.contains("unknown")),
            "the partial word must never reach slash_command"
        );
    }

    /// Finding 5: the one interaction the `State::skills`/`State::commands`
    /// split risks — both are populated at a single site (`tui.rs`) and
    /// cannot drift today, but nothing previously exercised selecting a
    /// *skill* row (as opposed to a builtin) through the popup and running
    /// it. `enter_runs_the_highlighted_command_not_the_literal_text` covers
    /// the builtin case; this is its skill-row counterpart.
    #[test]
    fn selecting_a_skill_in_the_popup_and_pressing_enter_dispatches_it() {
        let mut s = with_skills(&[("review", "review a pull request")]);
        type_str(&mut s, "/rev");
        assert_eq!(selected(&s), "review", "the skill is the highlighted match");
        let cmds = press(&mut s, KeyCode::Enter);
        let Some(Cmd::SendPrompt(p)) = cmds.first() else {
            panic!("expected a prompt, got {cmds:?}");
        };
        assert!(
            p.text.contains("Load the skill `review`"),
            "the popup selection must dispatch the skill, not the literal typed word: {}",
            p.text
        );
    }

    #[test]
    fn esc_dismisses_and_stays_dismissed_until_the_slash_is_gone() {
        let mut s = with_skills(&[("review", "review a pull request")]);
        type_str(&mut s, "/re");
        press(&mut s, KeyCode::Esc);
        assert!(s.completion.is_none());
        type_str(&mut s, "n");
        assert!(s.completion.is_none(), "still dismissed while typing");

        // Clearing the buffer past the slash re-arms it.
        for _ in 0.."/ren".chars().count() {
            press(&mut s, KeyCode::Backspace);
        }
        type_str(&mut s, "/re");
        assert!(s.completion.is_some(), "a fresh slash opens it again");
    }

    /// Esc is layered: it belongs to the popup first, and only reaches the
    /// editor's Insert→Normal transition once the popup is gone.
    #[test]
    fn the_first_esc_dismisses_the_popup_and_the_second_reaches_normal_mode() {
        let mut s = with_skills(&[("review", "review a pull request")]);
        type_str(&mut s, "/re");
        press(&mut s, KeyCode::Esc);
        assert_eq!(s.editor.mode(), crate::vim::Mode::Insert);
        press(&mut s, KeyCode::Esc);
        assert_eq!(s.editor.mode(), crate::vim::Mode::Normal);
    }

    /// Finding 1 (blocking): reverse-i-search must own the keyboard the
    /// instant it starts. Before the fix, `state.completion` survived the
    /// `Ctrl-R` that started the search, so the popup's own Esc handler
    /// swallowed the first Esc — the search only ended on the second one.
    #[test]
    fn ctrl_r_closes_a_stale_popup_and_the_first_esc_ends_the_search() {
        let mut s = with_skills(&[("review", "review a pull request")]);
        type_str(&mut s, "/re");
        assert!(s.completion.is_some(), "popup open before ctrl-r");
        ctrl(&mut s, 'r');
        assert!(
            s.editor.search_prompt().is_some(),
            "ctrl-r must still start the search"
        );
        assert!(
            s.completion.is_none(),
            "the search owns the input area now — the popup must not survive it"
        );
        press(&mut s, KeyCode::Esc);
        assert!(
            s.editor.search_prompt().is_none(),
            "one esc must end the search outright, not get swallowed by a stale popup"
        );
    }

    /// A permission ask arriving mid-typing (the popup was open on a partial
    /// `/` word) must close the popup immediately — the ask owns the
    /// keyboard, and a stale menu must not linger over its card.
    #[test]
    fn a_permission_ask_mid_typing_closes_the_open_popup() {
        let mut s = with_skills(&[("review", "review a pull request")]);
        type_str(&mut s, "/re");
        assert!(s.completion.is_some(), "popup open before the ask arrives");
        ask(&mut s);
        assert!(
            s.completion.is_none(),
            "the ask must close a popup left open from mid-typing"
        );
    }

    #[test]
    fn an_argument_closes_the_popup_before_submit() {
        let mut s = with_skills(&[("review", "review a pull request")]);
        type_str(&mut s, "/mode ");
        assert!(s.completion.is_none());
        let cmds = type_and_submit(&mut s, "auto");
        assert!(
            matches!(&cmds[..], [Cmd::SetMode(m)] if m == "auto"),
            "got {cmds:?}"
        );
    }

    /// The `$EDITOR` round trip replaces the whole buffer outside the normal
    /// key path. If the popup from a stale `/re` survives that replacement,
    /// the next Enter splices against a selection that no longer describes
    /// what's on screen — silently destroying the user's freehand prompt in
    /// favor of a bogus `/rename`.
    #[test]
    fn editor_done_clears_a_stale_popup_so_enter_submits_the_real_text() {
        let mut s = with_skills(&[("review", "review a pull request")]);
        type_str(&mut s, "/re");
        assert!(
            s.completion.is_some(),
            "popup open before the editor round trip"
        );
        update(&mut s, Msg::EditorDone(Some("explain the bug".into())));
        assert!(
            s.completion.is_none(),
            "the popup must not survive a buffer replaced out from under it"
        );
        let cmds = press(&mut s, KeyCode::Enter);
        assert_eq!(s.editor.text(), "");
        assert!(
            matches!(
                &cmds[..],
                [Cmd::AppendHistory(h), Cmd::SendPrompt(p), Cmd::SetTitle(_)]
                    if h == "explain the bug" && p.text == "explain the bug"
            ),
            "the editor's real content must reach the model unchanged, got {cmds:?}"
        );
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

    fn slash(s: &mut State, rest: &str) -> Vec<Cmd> {
        slash_command(s, rest, paste::PromptPayload::text_only(format!("/{rest}")))
    }

    fn last_notice(s: &State) -> String {
        match s.transcript.last() {
            Some(TranscriptItem::Notice { text }) => text.clone(),
            other => panic!("expected a notice, got {other:?}"),
        }
    }

    #[test]
    fn rename_uses_the_shared_normalizer() {
        let mut s = State::test_default();
        slash(&mut s, "rename   spaced name  ");
        assert_eq!(s.session_name.as_deref(), Some("spaced name"));
        slash(&mut s, &format!("rename {}", "x".repeat(65)));
        assert!(last_notice(&s).contains("1–64"));
        // The one source of truth, not a copy of its rules.
        assert_eq!(
            hotl_types::normalize_session_name("  ok  ").as_deref(),
            Some("ok")
        );
    }

    #[test]
    fn the_new_builtins_dispatch_and_are_completable() {
        // `complete::BUILTINS` and `slash_command`'s arms must agree — the pin
        // test below covers the set; this covers each one's effect.
        let mut s = State::test_default();
        assert!(slash(&mut s, "help").is_empty());
        assert!(s.help_open);

        let mut s = State::test_default();
        s.mode = "auto".into();
        s.model = "claude-opus-4-8".into();
        slash(&mut s, "status");
        let text = last_notice(&s);
        assert!(
            text.contains("auto") && text.contains("claude-opus-4-8"),
            "{text}"
        );

        let mut s = State::test_default();
        s.session_usage.input = 1_500;
        slash(&mut s, "cost");
        assert!(last_notice(&s).contains("1.5k"), "{}", last_notice(&s));

        let mut s = State::test_default();
        s.transcript = vec![TranscriptItem::Notice { text: "old".into() }];
        slash(&mut s, "clear");
        assert_eq!(
            s.transcript.len(),
            1,
            "the clear notice replaces the transcript"
        );
        assert!(
            last_notice(&s).contains("view"),
            "must not imply the log was cleared: {}",
            last_notice(&s)
        );

        let mut s = State::test_default();
        assert_eq!(slash(&mut s, "quit"), vec![Cmd::Quit]);
    }

    #[test]
    fn unknown_slash_still_reaches_no_model() {
        let mut s = State::test_default();
        let cmds = slash(&mut s, "compact");
        assert!(
            cmds.is_empty(),
            "/compact is deferred; it must not become a prompt"
        );
        assert!(last_notice(&s).contains("unknown command"));
    }

    /// Finding 4 (minor): `complete::BUILTINS` and `slash_command`'s match
    /// arms are two unpinned sources of truth for the same list — nothing
    /// enforces that a name in one exists in the other. This pins them: a
    /// name only `slash_command` recognizes just doesn't show up in the
    /// popup (silently missable), but a name only `BUILTINS` advertises is
    /// worse — the popup completes it and then Enter dispatches to the
    /// `unknown command: /<name>` notice. Add a 4th entry to `BUILTINS`
    /// without a matching arm and this test catches it.
    #[test]
    fn every_builtin_name_dispatches_to_something_other_than_unknown_command() {
        for cmd in complete::builtins() {
            let mut s = State::test_default();
            type_and_submit(&mut s, &format!("/{}", cmd.name));
            let unknown = format!("unknown command: /{}", cmd.name);
            assert!(
                !matches!(s.transcript.last(), Some(TranscriptItem::Notice { text }) if *text == unknown),
                "BUILTINS lists `{}` but slash_command has no matching dispatch arm for it",
                cmd.name
            );
        }
    }

    #[test]
    fn slash_reload_emits_the_settings_half_before_the_wire_half() {
        let mut s = State::test_default();
        let cmds = type_and_submit(&mut s, "/reload");
        assert_eq!(
            cmds,
            vec![Cmd::ReloadSettings, Cmd::ReloadConfig],
            "the theme must flip before the engine rebuild is awaited"
        );
        assert_eq!(s.phase, Phase::Idle, "a reload never starts a turn");
        assert!(last_notice(&s).contains("reloading"));
    }

    /// A rebuild replaces the session, taking the in-flight turn's reply with
    /// it. Abandoning a turn stays the esc ladder's job.
    #[test]
    fn slash_reload_is_refused_while_a_turn_runs() {
        let mut s = State::test_default();
        s.phase = Phase::Sampling { ticks: 0 };
        let cmds = slash(&mut s, "reload");
        assert!(cmds.is_empty(), "got {cmds:?}");
        assert_eq!(
            s.phase,
            Phase::Sampling { ticks: 0 },
            "the turn is untouched"
        );
        assert!(last_notice(&s).contains("idle session"));
    }

    #[test]
    fn config_reloaded_reseeds_model_mode_window_and_the_skill_roster() {
        let mut s = State::test_default();
        s.set_skills(vec![("old".into(), "gone after the reload".into())]);
        let cmds = update(
            &mut s,
            Msg::Update(json!({
                "type": "config_reloaded",
                "model": "anthropic/claude-opus-5",
                "mode": "plan",
                "context_window": 900_000,
                "skills": [{"name": "run", "description": "launch the app"}],
                "warnings": ["[skills.marketplaces] `bad name` — entry skipped"],
            })),
        );
        assert!(cmds.is_empty(), "a reload notification commands nothing");
        assert_eq!(s.model, "anthropic/claude-opus-5");
        assert_eq!(s.mode, "plan");
        assert_eq!(s.context_window, 900_000);
        assert_eq!(s.skills, vec!["run".to_string()], "the old roster is gone");
        assert!(
            s.commands.iter().any(|c| c.name == "run" && !c.builtin),
            "the completion table follows the roster"
        );
        assert!(
            s.commands.iter().any(|c| c.name == "reload" && c.builtin),
            "built-ins survive a roster swap"
        );
        assert!(!s.commands.iter().any(|c| c.name == "old"));
        let notices: Vec<&String> = s
            .transcript
            .iter()
            .filter_map(|i| match i {
                TranscriptItem::Notice { text } => Some(text),
                _ => None,
            })
            .collect();
        assert!(
            notices.iter().any(|t| t.contains("entry skipped")),
            "server-side warnings reach the transcript: {notices:?}"
        );
        assert!(
            notices.iter().any(|t| t.contains("config reloaded")),
            "{notices:?}"
        );
    }

    /// A zero (or absent) window is an older server saying nothing, not a
    /// claim that the model has no context — the gauge must keep dividing by
    /// what it already knows.
    #[test]
    fn config_reloaded_keeps_the_known_window_when_the_server_reports_none() {
        let mut s = State::test_default();
        s.context_window = 200_000;
        update(
            &mut s,
            Msg::Update(json!({"type": "config_reloaded", "model": "m", "mode": "ask"})),
        );
        assert_eq!(s.context_window, 200_000);
    }

    #[test]
    fn a_failed_reload_says_the_previous_config_is_still_live() {
        let mut s = State::test_default();
        update(
            &mut s,
            Msg::Update(json!({
                "type": "config_reload_failed",
                "reason": "TOML parse error at line 3",
            })),
        );
        let text = last_notice(&s);
        assert!(text.contains("TOML parse error"), "{text}");
        assert!(text.contains("still live"), "{text}");
    }

    /// The detached-turn filter swallows everything a dead turn emits. A
    /// reload is not the dead turn talking — it replaced the session outright,
    /// and a swallowed `config_reloaded` would leave the badge, the model and
    /// the roster describing an engine that is gone.
    #[test]
    fn a_detached_turn_does_not_swallow_the_reload_notifications() {
        let mut s = State::test_default();
        s.detached_turns = 1;
        update(
            &mut s,
            Msg::Update(
                json!({"type": "config_reloaded", "model": "m2", "mode": "auto",
                               "skills": [], "context_window": 123_456}),
            ),
        );
        assert_eq!(s.model, "m2");
        assert_eq!(s.mode, "auto");
        assert_eq!(s.context_window, 123_456);

        // …while an ordinary update from the dead turn still is swallowed.
        let before = s.transcript.len();
        update(
            &mut s,
            Msg::Update(json!({"type": "assistant_delta", "text": "ghost"})),
        );
        assert_eq!(s.transcript.len(), before);
    }

    #[test]
    fn settings_reloaded_applies_vim_mode_and_density_and_shows_warnings() {
        let mut s = State::test_default();
        assert!(s.vim_mode, "test default is vim on");
        update(
            &mut s,
            Msg::SettingsReloaded {
                vim_mode: false,
                density: hotl_theme::Density::Compact,
                warnings: vec!["unknown density 'wat' — using comfortable".into()],
            },
        );
        assert!(!s.vim_mode);
        assert_eq!(s.density, hotl_theme::Density::Compact);
        assert!(last_notice(&s).contains("unknown density"));
        // The editor holds its own copy; a stale one would leave modal keys
        // live after vim mode was turned off.
        s.editor.set_text("abc");
        assert_eq!(s.editor.text(), "abc");
    }

    #[test]
    fn a_known_skill_name_after_slash_prompts_for_that_skill() {
        let mut s = State::test_default();
        s.skills = vec!["brainstorming".into(), "superpowers:brainstorming".into()];

        let cmds = type_and_submit(&mut s, "/brainstorming redesign the skill system");
        let Some(Cmd::SendPrompt(p)) = cmds.first() else {
            panic!("expected a prompt, got {cmds:?}");
        };
        assert!(p.text.contains("`brainstorming`"), "{}", p.text);
        assert!(
            p.text.contains("ARGUMENTS: redesign the skill system"),
            "the argument rides along: {}",
            p.text
        );
        assert_eq!(s.phase, Phase::Sampling { ticks: 0 });

        // Qualified names resolve too, and take no argument fine.
        let mut s = State::test_default();
        s.skills = vec!["superpowers:brainstorming".into()];
        let cmds = type_and_submit(&mut s, "/superpowers:brainstorming");
        let Some(Cmd::SendPrompt(p)) = cmds.first() else {
            panic!("expected a prompt, got {cmds:?}");
        };
        assert!(!p.text.contains("ARGUMENTS"), "{}", p.text);
    }

    #[test]
    fn a_skill_invocation_carries_its_attached_images() {
        let mut s = State::new(false, "m".into());
        s.skills = vec!["brainstorming".into()];
        type_str(&mut s, "/brainstorming ");
        update(&mut s, Msg::Paste("/tmp/mockup.png".into()));
        let cmds = press(&mut s, KeyCode::Enter);
        let Some(Cmd::SendPrompt(p)) = cmds.iter().find_map(|c| match c {
            Cmd::SendPrompt(p) => Some(Cmd::SendPrompt(p.clone())),
            _ => None,
        }) else {
            panic!("a skill desugars to a prompt: {cmds:?}");
        };
        assert_eq!(p.images.len(), 1, "the mockup must ride along");
        assert_eq!(p.images[0].path, "/tmp/mockup.png");
        assert!(
            p.text.contains("[Image #1]"),
            "and its marker stays inline: {}",
            p.text
        );
        assert!(
            p.text.starts_with("Load the skill `brainstorming`"),
            "{}",
            p.text
        );
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

    /// Press, drag to `(col, row)`, release — one whole mouse gesture.
    fn drag_to(s: &mut State, col: u16, row: u16) -> Vec<Cmd> {
        update(s, Msg::SelectStart { col: 2, row: 1 });
        update(s, Msg::SelectExtend { col, row });
        update(s, Msg::SelectEnd)
    }

    #[test]
    fn a_finished_drag_asks_the_runtime_to_copy() {
        let mut s = State::test_default();
        let cmds = drag_to(&mut s, 9, 3);
        assert!(
            matches!(&cmds[..], [Cmd::CopySelection(sel)] if sel.anchor == (2, 1) && sel.head == (9, 3)),
            "got {cmds:?}"
        );
    }

    #[test]
    fn a_finished_drag_leaves_the_highlight_up() {
        let mut s = State::test_default();
        drag_to(&mut s, 9, 3);
        assert!(
            s.selection.is_some(),
            "the copied region stays visible until the next action"
        );
    }

    #[test]
    fn a_click_that_never_dragged_copies_nothing() {
        let mut s = State::test_default();
        update(&mut s, Msg::SelectStart { col: 2, row: 1 });
        let cmds = update(&mut s, Msg::SelectEnd);
        assert!(cmds.is_empty(), "got {cmds:?}");
        assert!(s.selection.is_none(), "a bare click leaves nothing painted");
    }

    #[test]
    fn a_keypress_clears_the_selection_and_the_notice() {
        let mut s = State::test_default();
        drag_to(&mut s, 9, 3);
        update(&mut s, Msg::Copied { lines: 3 });
        press(&mut s, KeyCode::Char('x'));
        assert!(s.selection.is_none());
        assert!(s.copy_notice.is_none());
    }

    #[test]
    fn streaming_updates_do_not_clear_a_live_drag() {
        let mut s = State::test_default();
        update(&mut s, Msg::SelectStart { col: 2, row: 1 });
        upd(
            &mut s,
            json!({"type": "text_delta", "text": "still writing"}),
        );
        update(&mut s, Msg::SelectExtend { col: 9, row: 3 });
        let cmds = update(&mut s, Msg::SelectEnd);
        assert!(
            matches!(&cmds[..], [Cmd::CopySelection(_)]),
            "a drag must survive arriving tokens, got {cmds:?}"
        );
    }

    #[test]
    fn a_copy_of_nothing_raises_no_notice() {
        let mut s = State::test_default();
        drag_to(&mut s, 9, 3);
        update(&mut s, Msg::Copied { lines: 0 });
        assert_eq!(s.copy_notice, None);
    }

    #[test]
    fn a_copy_records_the_line_count_for_the_hint() {
        let mut s = State::test_default();
        drag_to(&mut s, 9, 3);
        update(&mut s, Msg::Copied { lines: 3 });
        assert_eq!(s.copy_notice, Some(3));
    }
}
