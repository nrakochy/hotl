use types::Status;

pub struct Signals<'a> {
    pub title: &'a str,
    pub tail: &'a str,
}

pub trait StatusDetector {
    /// Determine the agent's current status from terminal signals.
    fn classify(&self, sig: &Signals) -> Status;

    /// Extract a one-line status summary from the pane tail (e.g. token usage).
    /// Returns `None` if nothing relevant is found.
    fn status_line(&self, tail: &str) -> Option<String> {
        let _ = tail;
        None
    }
}

// ─── Shared helpers ────────────────────────────────────────────────────────────

/// Braille spinner char (U+2800..=U+28FF) at the start of a string.
fn starts_with_braille(s: &str) -> bool {
    matches!(s.trim_start().chars().next(), Some(c) if ('\u{2800}'..='\u{28FF}').contains(&c))
}

fn title_is_working(title: &str) -> bool {
    starts_with_braille(title)
}

fn tail_has_prompt(tail: &str) -> bool {
    tail.lines().any(|l| {
        let t = l.trim();
        let Some(rest) = t.strip_prefix('❯') else { return false };
        let rest = rest.trim_start();
        // "❯ 1. …" is a selection-menu item, not an idle prompt.
        !(rest.chars().next().is_some_and(|c| c.is_ascii_digit())
            && rest.trim_start_matches(|c: char| c.is_ascii_digit()).starts_with('.'))
    })
}

// ─── Claude ────────────────────────────────────────────────────────────────────

pub struct ClaudeDetector;

impl StatusDetector for ClaudeDetector {
    fn classify(&self, sig: &Signals) -> Status {
        if title_is_working(sig.title) {
            Status::Working
        } else if Self::tail_is_blocked(sig.tail) {
            Status::Blocked
        } else if tail_has_prompt(sig.tail) {
            Status::Idle
        } else {
            Status::Unknown
        }
    }

    fn status_line(&self, tail: &str) -> Option<String> {
        tail.lines()
            .find(|l| l.contains("ctx:"))
            .map(|l| l.trim().to_string())
    }
}

impl ClaudeDetector {
    fn tail_is_blocked(tail: &str) -> bool {
        let t = tail.to_lowercase();
        t.contains("esc to cancel")
            && (t.contains("enter to select")
                || t.contains("to navigate")
                || t.contains("↑↓"))
    }
}

// ─── Pi ────────────────────────────────────────────────────────────────────────

pub struct PiDetector;

impl StatusDetector for PiDetector {
    fn classify(&self, sig: &Signals) -> Status {
        if Self::tail_is_working(sig.tail) {
            Status::Working
        } else if Self::tail_is_blocked(sig.tail) {
            Status::Blocked
        } else if Self::tail_is_idle(sig.tail) {
            Status::Idle
        } else {
            Status::Unknown
        }
    }

    fn status_line(&self, tail: &str) -> Option<String> {
        tail.lines()
            .find(|l| l.contains('↑') && l.contains('↓') && l.contains("/1"))
            .map(|l| l.trim().to_string())
    }
}

impl PiDetector {
    /// Pi shows a braille spinner followed by "Working..." when processing.
    fn tail_is_working(tail: &str) -> bool {
        tail.lines().any(|l| starts_with_braille(l) && l.contains("Working"))
    }

    /// Pi is idle when the footer with token stats is visible and no spinner.
    fn tail_is_idle(tail: &str) -> bool {
        tail.lines().any(|l| l.contains('↑') && l.contains('↓') && l.contains("/1"))
    }

    /// Pi prompts the user for trust decisions or other confirmations.
    fn tail_is_blocked(tail: &str) -> bool {
        let t = tail.to_lowercase();
        (t.contains("allow") || t.contains("trust") || t.contains("approve"))
            && (t.contains("y/n") || t.contains("yes") || t.contains("enter to"))
    }
}

// ─── Generic (fallback) ────────────────────────────────────────────────────────

pub struct GenericDetector;

impl StatusDetector for GenericDetector {
    fn classify(&self, sig: &Signals) -> Status {
        if title_is_working(sig.title) {
            Status::Working
        } else if tail_has_prompt(sig.tail) {
            Status::Idle
        } else {
            Status::Unknown
        }
    }
}

// ─── Dispatch ──────────────────────────────────────────────────────────────────

pub fn detector_for(agent_name: &str) -> Box<dyn StatusDetector> {
    match agent_name {
        "claude" => Box::new(ClaudeDetector),
        "pi" => Box::new(PiDetector),
        _ => Box::new(GenericDetector),
    }
}

pub fn classify(agent_name: &str, title: &str, tail: &str) -> Status {
    detector_for(agent_name).classify(&Signals { title, tail })
}

pub fn extract_status_line(agent_name: &str, tail: &str) -> Option<String> {
    detector_for(agent_name).status_line(tail)
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
        assert_eq!(classify("claude", "\u{2809} Refactoring", IDLE_TAIL), Status::Working);
    }

    #[test]
    fn idle_when_prompt_and_non_braille_title() {
        assert_eq!(classify("claude", "✳ Some task", IDLE_TAIL), Status::Idle);
    }

    #[test]
    fn blocked_when_selection_form_present() {
        assert_eq!(classify("claude", "✳ Some task", BLOCKED_TAIL), Status::Blocked);
    }

    #[test]
    fn blocked_takes_precedence_over_idle() {
        let tail = format!("{}\n{}", IDLE_TAIL, BLOCKED_TAIL);
        assert_eq!(classify("claude", "✳", &tail), Status::Blocked);
    }

    #[test]
    fn working_takes_precedence_over_blocked() {
        assert_eq!(classify("claude", "\u{2809}", BLOCKED_TAIL), Status::Working);
    }

    #[test]
    fn unknown_when_no_signals() {
        assert_eq!(classify("claude", "plain title", "just some log output"), Status::Unknown);
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

    #[test]
    fn idle_when_prompt_has_drafted_input() {
        assert_eq!(classify("claude", "✳", "❯ some draft text"), Status::Idle);
    }

    #[test]
    fn menu_option_prompt_is_not_idle() {
        assert_eq!(classify("codex", "plain", "❯ 1. Yes"), Status::Unknown);
    }

    // --- status_line extraction ---

    #[test]
    fn claude_status_line_extracts_ctx() {
        let tail = "some output\n  [I] .../proj [main] ctx:9%\nmore";
        assert_eq!(
            extract_status_line("claude", tail).as_deref(),
            Some("[I] .../proj [main] ctx:9%"),
        );
    }

    #[test]
    fn claude_status_line_none_when_no_ctx() {
        assert_eq!(extract_status_line("claude", "just log output\n❯ "), None);
    }

    #[test]
    fn pi_status_line_extracts_footer() {
        let tail = "───\n~/sources/hotl (master)\n↑9.5k ↓1.9k R174k W32k CH98.8% $0.379 3.2%/1.0M (auto)  model • medium";
        assert_eq!(
            extract_status_line("pi", tail).as_deref(),
            Some("↑9.5k ↓1.9k R174k W32k CH98.8% $0.379 3.2%/1.0M (auto)  model • medium"),
        );
    }

    #[test]
    fn pi_status_line_none_when_no_footer() {
        assert_eq!(extract_status_line("pi", "just some output"), None);
    }

    #[test]
    fn generic_status_line_is_none() {
        assert_eq!(extract_status_line("codex", "anything"), None);
    }

    // --- Pi detector tests ---

    const PI_WORKING_TAIL: &str = "\
$ bash some-command
Elapsed 0.1s

 \u{280f} Working...

───────────────────────────────────

───────────────────────────────────
~/sources/hotl (master)
↑9.5k ↓1.9k R174k W32k CH98.8% $0.379 3.2%/1.0M (auto)  model • medium";

    const PI_IDLE_TAIL: &str = "\
───────────────────────────────────

───────────────────────────────────
~/sources/hotl (master)
↑9.5k ↓1.9k R174k W32k CH98.8% $0.379 3.2%/1.0M (auto)  model • medium";

    const PI_BLOCKED_TAIL: &str = "\
Allow tool execution in /tmp/proj?
Trust this project? (y/n)
───────────────────────────────────";

    #[test]
    fn pi_working_when_braille_spinner_present() {
        assert_eq!(classify("pi", "π - hotl", PI_WORKING_TAIL), Status::Working);
    }

    #[test]
    fn pi_idle_when_footer_visible_no_spinner() {
        assert_eq!(classify("pi", "π - hotl", PI_IDLE_TAIL), Status::Idle);
    }

    #[test]
    fn pi_blocked_on_trust_prompt() {
        assert_eq!(classify("pi", "π - hotl", PI_BLOCKED_TAIL), Status::Blocked);
    }

    #[test]
    fn pi_unknown_when_no_signals() {
        assert_eq!(classify("pi", "π - hotl", "just some output"), Status::Unknown);
    }

    #[test]
    fn pi_working_takes_precedence_over_idle() {
        assert_eq!(classify("pi", "π - hotl", PI_WORKING_TAIL), Status::Working);
    }
}
