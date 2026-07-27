use watch_types::Status;

pub struct Signals<'a> {
    pub title: &'a str,
    pub tail: &'a str,
}

pub trait StatusDetector {
    fn classify(&self, sig: &Signals) -> Status;
    /// The question the pane is presenting to the human, best-effort. Called
    /// only for panes already classified Blocked (see [`extract_prompt`]), so
    /// impls do pure extraction, not state detection.
    fn prompt(&self, _sig: &Signals) -> Option<String> {
        None
    }
}

// Braille spinner char (U+2800..=U+28FF).
fn title_is_working(title: &str) -> bool {
    matches!(title.trim_start().chars().next(), Some(c) if ('\u{2800}'..='\u{28FF}').contains(&c))
}

fn tail_is_blocked(tail: &str) -> bool {
    let t = tail.to_lowercase();
    t.contains("esc to cancel")
        && (t.contains("enter to select") || t.contains("to navigate") || t.contains("↑↓"))
}

/// A selection-menu option line: optional `❯` cursor, then `N.`.
fn is_menu_option(line: &str) -> bool {
    let t = line.trim();
    let t = t.strip_prefix('❯').unwrap_or(t).trim_start();
    t.chars().next().is_some_and(|c| c.is_ascii_digit())
        && t.trim_start_matches(|c: char| c.is_ascii_digit())
            .starts_with('.')
}

/// Visual chrome: blank, or made only of box-drawing/rule characters — never
/// the header line a question hunt should land on.
fn is_chrome(line: &str) -> bool {
    let t = line.trim();
    t.chars().all(|c| {
        matches!(
            c,
            '─' | '│' | '╭' | '╮' | '╰' | '╯' | '═' | '━' | '┄' | '-' | '·' | ' '
        )
    })
}

fn tail_is_idle(tail: &str) -> bool {
    tail.lines().any(|l| {
        let t = l.trim();
        // "❯ 1. …" is a selection-menu item, not an idle prompt.
        t.starts_with('❯') && !is_menu_option(t)
    })
}

pub struct ClaudeDetector;
impl StatusDetector for ClaudeDetector {
    fn classify(&self, sig: &Signals) -> Status {
        if title_is_working(sig.title) {
            Status::Working
        } else if tail_is_blocked(sig.tail) {
            Status::Blocked
        } else if tail_is_idle(sig.tail) {
            Status::Idle
        } else {
            Status::Unknown
        }
    }

    // The selection form renders its question just above the numbered
    // options, so the header is the nearest content line over the first one.
    fn prompt(&self, sig: &Signals) -> Option<String> {
        let lines: Vec<&str> = sig.tail.lines().collect();
        if let Some(menu_at) = lines.iter().position(|l| is_menu_option(l)) {
            if let Some(header) = lines[..menu_at].iter().rev().find(|l| !is_chrome(l)) {
                return Some(header.trim().to_string());
            }
        }
        lines
            .iter()
            .rev()
            .find(|l| l.trim().ends_with('?'))
            .map(|l| l.trim().to_string())
    }
}

// hotl's permission ask renders as `allow <summary>? [y/N — …]` when it is
// attached to a plain terminal, and as a modal card in the console TUI. The
// TUI's hint row repeats the card's words on the bottom screen line, which is
// what a captured tail can still reach once the card itself has scrolled past
// it; the question picker names its own keys the same way.
fn tail_is_hotl_ask(tail: &str) -> bool {
    let t = tail.to_lowercase();
    t.contains("[y/n") || t.contains("y allow · n deny") || t.contains("1-9 pick an option")
}

/// The console TUI names its phase in the terminal title — `hotl` or
/// `hotl · <name>`, plus a state suffix — which tmux records as
/// `#{pane_title}`. That is the one signal that survives a long session: the
/// ask card is centered over the transcript, so the rows below it can fill
/// the whole captured tail. `None` means the pane wears some other program's
/// title (the shell's, before the TUI has set one).
fn hotl_title_status(title: &str) -> Option<Status> {
    let t = title.trim();
    if t != "hotl" && !t.starts_with("hotl ") {
        return None;
    }
    Some(if t.ends_with("— waiting on you") {
        Status::Blocked
    } else if t.ends_with("— working") {
        Status::Working
    } else {
        Status::Idle
    })
}

pub struct HotlDetector;
impl StatusDetector for HotlDetector {
    fn classify(&self, sig: &Signals) -> Status {
        // The title is the TUI's own report of its phase, so it outranks the
        // screen — which can still show an answered ask a frame later, and a
        // Blocked read there would fire a ping nobody is waiting behind.
        if let Some(status) = hotl_title_status(sig.title) {
            return status;
        }
        if title_is_working(sig.title) {
            Status::Working
        } else if tail_is_hotl_ask(sig.tail) {
            Status::Blocked
        } else if tail_is_idle(sig.tail) {
            Status::Idle
        } else {
            Status::Unknown
        }
    }

    fn prompt(&self, sig: &Signals) -> Option<String> {
        let lines: Vec<&str> = sig.tail.lines().collect();
        // Plain-terminal ask: the `allow …? [y/N — …]` line carries the
        // tool summary itself.
        if let Some(ask) = lines.iter().find(|l| l.to_lowercase().contains("[y/n")) {
            return Some(ask.trim().to_string());
        }
        // Console-TUI ask: the hint row sits on the bottom screen line; the
        // question (or what's left of the card) is the nearest content line
        // above it. A hint-only tail has nothing worth extracting.
        let hint_at = lines.iter().position(|l| {
            let t = l.to_lowercase();
            t.contains("y allow · n deny") || t.contains("1-9 pick an option")
        })?;
        lines[..hint_at]
            .iter()
            .rev()
            .find(|l| !is_chrome(l))
            .map(|l| l.trim().to_string())
    }
}

pub struct GenericDetector;
impl StatusDetector for GenericDetector {
    fn classify(&self, sig: &Signals) -> Status {
        if title_is_working(sig.title) {
            Status::Working
        } else if tail_is_idle(sig.tail) {
            Status::Idle
        } else {
            Status::Unknown
        }
    }
}

pub fn detector_for(agent_name: &str) -> Box<dyn StatusDetector> {
    match agent_name {
        "claude" => Box::new(ClaudeDetector),
        "hotl" => Box::new(HotlDetector),
        _ => Box::new(GenericDetector),
    }
}

pub fn classify(agent_name: &str, title: &str, tail: &str) -> Status {
    detector_for(agent_name).classify(&Signals { title, tail })
}

/// The pending question of a Blocked pane, best-effort. Gated on the
/// detector's own classification so `prompt` implies Blocked — a Working
/// pane never carries a prompt (a stale ask frame must not flap a
/// change-detecting consumer), and the extraction impls stay pure.
pub fn extract_prompt(agent_name: &str, title: &str, tail: &str) -> Option<String> {
    let sig = Signals { title, tail };
    let detector = detector_for(agent_name);
    if detector.classify(&sig) != Status::Blocked {
        return None;
    }
    detector.prompt(&sig)
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDLE_TAIL: &str = "\
✻ Brewed for 9m
─────────────
❯
─────────────
  [I] .../sources/lca [branch] Opus 4.8 ctx:17%";

    const BLOCKED_TAIL: &str = "\
Do you want to proceed?
❯ 1. Yes
  2. No
enter to select · esc to cancel · ↑↓ to navigate";

    #[test]
    fn working_when_title_has_braille_spinner() {
        assert_eq!(
            classify("claude", "\u{2809} Refactoring", IDLE_TAIL),
            Status::Working
        );
    }

    #[test]
    fn idle_when_prompt_and_non_braille_title() {
        assert_eq!(classify("claude", "✳ Some task", IDLE_TAIL), Status::Idle);
    }

    #[test]
    fn blocked_when_selection_form_present() {
        assert_eq!(
            classify("claude", "✳ Some task", BLOCKED_TAIL),
            Status::Blocked
        );
    }

    #[test]
    fn blocked_takes_precedence_over_idle() {
        let tail = format!("{}\n{}", IDLE_TAIL, BLOCKED_TAIL);
        assert_eq!(classify("claude", "✳", &tail), Status::Blocked);
    }

    #[test]
    fn working_takes_precedence_over_blocked() {
        assert_eq!(
            classify("claude", "\u{2809}", BLOCKED_TAIL),
            Status::Working
        );
    }

    #[test]
    fn unknown_when_no_signals() {
        assert_eq!(
            classify("claude", "plain title", "just some log output"),
            Status::Unknown
        );
    }

    #[test]
    fn generic_detector_ignores_blocked_form() {
        assert_eq!(classify("codex", "plain", BLOCKED_TAIL), Status::Unknown);
    }

    #[test]
    fn generic_detector_detects_working_and_idle() {
        assert_eq!(classify("codex", "\u{2809}", ""), Status::Working);
        assert_eq!(classify("codex", "plain", IDLE_TAIL), Status::Idle);
    }

    const HOTL_ASK_TAIL: &str = "\
● bash: cargo test
⚠ PROTECTED PATH — writes to .git/hooks/ execute later
allow bash: cargo test? [y/N — add a reason after 'n' to tell the model why] ";

    // The console TUI's titles, as tmux reports them in `#{pane_title}`.
    // `crates/hotl/tests/watch_sees_tui_state.rs` renders the real TUI to
    // keep these honest.
    const TUI_BLOCKED_TITLE: &str = "hotl · fix-auth — waiting on you";
    const TUI_WORKING_TITLE: &str = "hotl · fix-auth — working";
    const TUI_IDLE_TITLE: &str = "hotl · fix-auth";

    // The TUI's hint row, which sits on the bottom screen line while an ask
    // is up — so the captured tail reaches it however long the session is.
    const TUI_ASK_HINT: &str = "y allow · n deny · type a reason after n · ctrl-c cancel";

    #[test]
    fn hotl_blocked_on_permission_ask() {
        assert_eq!(classify("hotl", "plain", HOTL_ASK_TAIL), Status::Blocked);
    }

    #[test]
    fn hotl_blocked_when_tui_title_says_waiting_on_you() {
        assert_eq!(classify("hotl", TUI_BLOCKED_TITLE, ""), Status::Blocked);
    }

    #[test]
    fn hotl_working_when_tui_title_says_working() {
        assert_eq!(classify("hotl", TUI_WORKING_TITLE, ""), Status::Working);
    }

    #[test]
    fn hotl_idle_when_tui_title_carries_no_state_suffix() {
        assert_eq!(classify("hotl", TUI_IDLE_TITLE, ""), Status::Idle);
        assert_eq!(classify("hotl", "hotl", ""), Status::Idle);
    }

    /// The screen can lag the phase by a frame; the title is the TUI's own
    /// report, so an answered ask must not linger as Blocked (a false ping).
    #[test]
    fn hotl_tui_title_outranks_a_stale_screen() {
        assert_eq!(
            classify("hotl", TUI_WORKING_TITLE, TUI_ASK_HINT),
            Status::Working
        );
    }

    /// Before the first turn ends the TUI has set no title at all, so the
    /// pane still wears the shell's. The ask hint carries it.
    #[test]
    fn hotl_blocked_on_tui_ask_hint_without_a_title() {
        assert_eq!(classify("hotl", "zsh", TUI_ASK_HINT), Status::Blocked);
    }

    #[test]
    fn hotl_blocked_on_tui_question_hint_without_a_title() {
        let tail = "1-9 pick an option · type for free text · enter submit · esc clear";
        assert_eq!(classify("hotl", "zsh", tail), Status::Blocked);
    }

    #[test]
    fn hotl_ignores_titles_from_other_programs() {
        assert_eq!(classify("hotl", "hotline — working", ""), Status::Unknown);
    }

    #[test]
    fn hotl_idle_on_prompt_marker() {
        assert_eq!(classify("hotl", "plain", "some output\n❯ "), Status::Idle);
    }

    #[test]
    fn hotl_ask_takes_precedence_over_idle_prompt() {
        let tail = format!("❯ \n{HOTL_ASK_TAIL}");
        assert_eq!(classify("hotl", "plain", &tail), Status::Blocked);
    }

    #[test]
    fn hotl_unknown_when_no_signals() {
        assert_eq!(
            classify("hotl", "plain", "streaming model output…"),
            Status::Unknown
        );
    }

    #[test]
    fn idle_when_prompt_has_drafted_input() {
        assert_eq!(classify("claude", "✳", "❯ some draft text"), Status::Idle);
    }

    #[test]
    fn menu_option_prompt_is_not_idle() {
        // Generic detector has no blocked rule, so "❯ 1." must not read as idle either.
        assert_eq!(classify("codex", "plain", "❯ 1. Yes"), Status::Unknown);
    }

    #[test]
    fn claude_prompt_is_the_menu_header_when_blocked() {
        assert_eq!(
            extract_prompt("claude", "✳", BLOCKED_TAIL).as_deref(),
            Some("Do you want to proceed?")
        );
    }

    #[test]
    fn claude_prompt_none_unless_blocked() {
        assert_eq!(extract_prompt("claude", "✳", IDLE_TAIL), None);
        // A working title outranks a blocked-looking tail: no prompt either.
        assert_eq!(extract_prompt("claude", "\u{2809}", BLOCKED_TAIL), None);
    }

    #[test]
    fn claude_prompt_falls_back_to_the_question_line() {
        let tail = "\
Should I continue with the risky migration?
enter to select · esc to cancel · ↑↓ to navigate";
        assert_eq!(
            extract_prompt("claude", "✳", tail).as_deref(),
            Some("Should I continue with the risky migration?")
        );
    }

    #[test]
    fn claude_prompt_skips_chrome_between_header_and_menu() {
        let tail = "\
Do you want to proceed?
─────────────
❯ 1. Yes
  2. No
enter to select · esc to cancel · ↑↓ to navigate";
        assert_eq!(
            extract_prompt("claude", "✳", tail).as_deref(),
            Some("Do you want to proceed?")
        );
    }

    #[test]
    fn hotl_prompt_is_the_plain_ask_line() {
        assert_eq!(
            extract_prompt("hotl", "plain", HOTL_ASK_TAIL).as_deref(),
            Some("allow bash: cargo test? [y/N — add a reason after 'n' to tell the model why]")
        );
    }

    #[test]
    fn hotl_prompt_from_tui_hint_is_the_line_above() {
        let tail = format!("Allow web fetch of docs.rs?\n{TUI_ASK_HINT}");
        assert_eq!(
            extract_prompt("hotl", "zsh", &tail).as_deref(),
            Some("Allow web fetch of docs.rs?")
        );
    }

    #[test]
    fn hotl_hint_only_tail_has_no_prompt() {
        assert_eq!(extract_prompt("hotl", "zsh", TUI_ASK_HINT), None);
    }

    #[test]
    fn hotl_title_blocked_with_empty_tail_has_no_prompt() {
        assert_eq!(extract_prompt("hotl", TUI_BLOCKED_TITLE, ""), None);
    }

    #[test]
    fn generic_agent_never_extracts_a_prompt() {
        assert_eq!(extract_prompt("codex", "plain", BLOCKED_TAIL), None);
    }
}
