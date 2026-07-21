use watch_types::Status;

pub struct Signals<'a> {
    pub title: &'a str,
    pub tail: &'a str,
}

pub trait StatusDetector {
    fn classify(&self, sig: &Signals) -> Status;
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

fn tail_is_idle(tail: &str) -> bool {
    tail.lines().any(|l| {
        let t = l.trim();
        let Some(rest) = t.strip_prefix('❯') else {
            return false;
        };
        // "❯ 1. …" is a selection-menu item, not an idle prompt.
        let rest = rest.trim_start();
        !(rest.chars().next().is_some_and(|c| c.is_ascii_digit())
            && rest
                .trim_start_matches(|c: char| c.is_ascii_digit())
                .starts_with('.'))
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
}

// hotl's permission ask renders as `allow <summary>? [y/N — …]`.
fn tail_is_hotl_ask(tail: &str) -> bool {
    tail.to_lowercase().contains("[y/n")
}

pub struct HotlDetector;
impl StatusDetector for HotlDetector {
    fn classify(&self, sig: &Signals) -> Status {
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

    #[test]
    fn hotl_blocked_on_permission_ask() {
        assert_eq!(classify("hotl", "plain", HOTL_ASK_TAIL), Status::Blocked);
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
}
