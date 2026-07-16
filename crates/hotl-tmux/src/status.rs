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

    /// Extract the model name from the pane tail.
    /// Returns `None` if the model cannot be determined.
    fn model(&self, tail: &str) -> Option<String> {
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

    /// Claude's status bar: `[I] .../path [branch] ModelName ctx:N%` or just
    /// `[I] .../path [branch] ModelName`. Match lines starting with `[I]` or
    /// `[A]` (insert/agent mode indicators).
    fn status_line(&self, tail: &str) -> Option<String> {
        tail.lines()
            .map(|l| l.trim())
            .find(|l| l.starts_with("[I]") || l.starts_with("[A]"))
            .map(|l| l.to_string())
    }

    /// Model name appears after the last `]` bracket on the status line:
    /// `[I] .../path [branch] Sonnet 5` → `Sonnet 5`
    fn model(&self, tail: &str) -> Option<String> {
        let line = self.status_line(tail)?;
        // Find the last `] ` and take everything after it, trimming ctx info.
        let after_bracket = line.rsplit("] ").next()?;
        let model = after_bracket
            .split(" ctx:")
            .next()
            .unwrap_or(after_bracket)
            .trim();
        if model.is_empty() { None } else { Some(model.to_string()) }
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

    /// Pi's footer: `↑9.5k ↓1.9k ... 3.2%/1.0M (auto)  model • level`
    fn status_line(&self, tail: &str) -> Option<String> {
        tail.lines()
            .find(|l| l.contains('↑') && l.contains('↓') && l.contains("/1"))
            .map(|l| l.trim().to_string())
    }

    /// Model is at the end of the footer, after the `(auto)` or percentage:
    /// `↑9.5k ↓1.9k ... 6.4%/1.0M (auto)  us.anthropic.claude-opus-4-6-v1 • medium`
    fn model(&self, tail: &str) -> Option<String> {
        let line = self.status_line(tail)?;
        // Model appears after the last run of whitespace following the stats.
        // Split on `•` to separate model from thinking level if present.
        // The model identifier comes after `(auto)` or the percentage block.
        let model_part = if let Some(after) = line.split("(auto)").nth(1) {
            after.trim().to_string()
        } else {
            // fallback: take from after the percentage pattern `/1.0M`
            line.split("/1").nth(1)
                .and_then(|s| s.split_whitespace().nth(1))
                .map(|rest| {
                    // rejoin remaining words
                    let idx = line.find(rest)?;
                    Some(line[idx..].to_string())
                })
                .flatten()?
        };
        if model_part.is_empty() { None } else { Some(model_part) }
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

// ─── OpenCode ──────────────────────────────────────────────────────────────────

pub struct OpenCodeDetector;

impl StatusDetector for OpenCodeDetector {
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

    /// OpenCode shows the model in the `Build · ModelName Provider` line.
    fn status_line(&self, tail: &str) -> Option<String> {
        Self::build_line(tail).map(|l| l.to_string())
    }

    fn model(&self, tail: &str) -> Option<String> {
        let line = Self::build_line(tail)?;
        // Format: "Build · US Anthropic Claude Opus 4.6 Amazon Bedrock"
        let after = line.split("Build · ").nth(1)?.trim();
        if after.is_empty() { None } else { Some(after.to_string()) }
    }
}

impl OpenCodeDetector {
    /// OpenCode shows `⬝` progress dots and `esc interrupt` when working.
    fn tail_is_working(tail: &str) -> bool {
        tail.contains("esc interrupt")
    }

    /// OpenCode is idle when the editor/footer is visible without a working
    /// indicator. Detected by `Build ·` model line or `tab agents` footer.
    fn tail_is_idle(tail: &str) -> bool {
        !Self::tail_is_working(tail)
            && (Self::build_line(tail).is_some() || tail.contains("Ask anything"))
    }

    /// OpenCode may prompt for tool approval.
    fn tail_is_blocked(tail: &str) -> bool {
        let t = tail.to_lowercase();
        (t.contains("approve") || t.contains("allow") || t.contains("confirm"))
            && (t.contains("y/n") || t.contains("enter") || t.contains("deny"))
    }

    /// Find the `Build · ...` line showing the active model/agent.
    fn build_line(tail: &str) -> Option<&str> {
        tail.lines()
            .filter_map(|l| {
                let t = l.trim().trim_start_matches('┃').trim_start_matches('▣').trim();
                if t.starts_with("Build") && t.contains('·') { Some(t) } else { None }
            })
            .last()
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
        "opencode" => Box::new(OpenCodeDetector),
        _ => Box::new(GenericDetector),
    }
}

pub fn classify(agent_name: &str, title: &str, tail: &str) -> Status {
    detector_for(agent_name).classify(&Signals { title, tail })
}

pub fn extract_status_line(agent_name: &str, tail: &str) -> Option<String> {
    detector_for(agent_name).status_line(tail)
}

pub fn extract_model(agent_name: &str, tail: &str) -> Option<String> {
    detector_for(agent_name).model(tail)
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

    // --- Claude tests ---

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
    fn idle_when_prompt_has_drafted_input() {
        assert_eq!(classify("claude", "✳", "❯ some draft text"), Status::Idle);
    }

    #[test]
    fn menu_option_prompt_is_not_idle() {
        assert_eq!(classify("codex", "plain", "❯ 1. Yes"), Status::Unknown);
    }

    #[test]
    fn claude_status_line_matches_mode_indicator() {
        let tail = "some output\n  [I] .../sources/hotl [master*] Sonnet 5\n  -- INSERT --";
        assert_eq!(
            extract_status_line("claude", tail).as_deref(),
            Some("[I] .../sources/hotl [master*] Sonnet 5"),
        );
    }

    #[test]
    fn claude_status_line_with_ctx() {
        let tail = "❯\n  [I] .../proj [main] Opus 4.8 ctx:15%";
        assert_eq!(
            extract_status_line("claude", tail).as_deref(),
            Some("[I] .../proj [main] Opus 4.8 ctx:15%"),
        );
    }

    #[test]
    fn claude_status_line_none_when_missing() {
        assert_eq!(extract_status_line("claude", "just log output\n❯ "), None);
    }

    #[test]
    fn claude_model_from_status_line() {
        let tail = "❯\n  [I] .../sources/hotl [master*] Sonnet 5\n  -- INSERT --";
        assert_eq!(extract_model("claude", tail).as_deref(), Some("Sonnet 5"));
    }

    #[test]
    fn claude_model_with_ctx_suffix() {
        let tail = "❯\n  [I] .../proj [main] Opus 4.8 ctx:15%";
        assert_eq!(extract_model("claude", tail).as_deref(), Some("Opus 4.8"));
    }

    #[test]
    fn claude_model_none_when_no_status_line() {
        assert_eq!(extract_model("claude", "plain output"), None);
    }

    // --- Generic tests ---

    #[test]
    fn generic_detector_ignores_blocked_form() {
        assert_eq!(classify("codex", "plain", BLOCKED_TAIL), Status::Unknown);
    }

    #[test]
    fn generic_detector_detects_working_and_idle() {
        assert_eq!(classify("codex", "\u{2809}", ""), Status::Working);
        assert_eq!(classify("codex", "plain", IDLE_TAIL), Status::Idle);
    }

    // --- Pi tests ---

    const PI_WORKING_TAIL: &str = "\
$ bash some-command
Elapsed 0.1s

 \u{280f} Working...

───────────────────────────────────

───────────────────────────────────
~/sources/hotl (master)
↑9.5k ↓1.9k R174k W32k CH98.8% $0.379 3.2%/1.0M (auto)  us.anthropic.claude-opus-4-6-v1 • medium";

    const PI_IDLE_TAIL: &str = "\
───────────────────────────────────

───────────────────────────────────
~/sources/hotl (master)
↑9.5k ↓1.9k R174k W32k CH98.8% $0.379 3.2%/1.0M (auto)  us.anthropic.claude-opus-4-6-v1 • medium";

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

    #[test]
    fn pi_status_line_extracts_footer() {
        assert_eq!(
            extract_status_line("pi", PI_IDLE_TAIL).as_deref(),
            Some("↑9.5k ↓1.9k R174k W32k CH98.8% $0.379 3.2%/1.0M (auto)  us.anthropic.claude-opus-4-6-v1 • medium"),
        );
    }

    #[test]
    fn pi_model_extraction() {
        assert_eq!(
            extract_model("pi", PI_IDLE_TAIL).as_deref(),
            Some("us.anthropic.claude-opus-4-6-v1 • medium"),
        );
    }

    #[test]
    fn pi_model_none_when_no_footer() {
        assert_eq!(extract_model("pi", "just some output"), None);
    }

    // --- OpenCode tests ---

    const OC_WORKING_TAIL: &str = "\
  ┃  Build · US Anthropic Claude Opus 4.6 Amazon Bedrock
  ╹▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀
   ⬝⬝⬝⬝⬝⬝⬝⬝  esc interrupt                                tab agents  ctrl+p commands    • OpenCode 1.15.12";

    const OC_IDLE_TAIL: &str = "\
  ┃
  ┃  Build · US Anthropic Claude Opus 4.6 Amazon Bedrock
  ╹▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀
                                                         tab agents  ctrl+p commands    • OpenCode 1.15.12";

    const OC_WELCOME_TAIL: &str = "\
  ┃  Ask anything... \"Fix a TODO in the codebase\"
  ┃
  ┃  Build · US Anthropic Claude Opus 4.6 Amazon Bedrock
  ╹▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀
                                                         tab agents  ctrl+p commands    • OpenCode 1.15.12";

    const OC_BLOCKED_TAIL: &str = "\
Approve tool execution? (y/n)
tab agents  ctrl+p commands    • OpenCode 1.15.12";

    #[test]
    fn opencode_working_when_esc_interrupt() {
        assert_eq!(classify("opencode", "OpenCode", OC_WORKING_TAIL), Status::Working);
    }

    #[test]
    fn opencode_idle_with_build_line() {
        assert_eq!(classify("opencode", "OpenCode", OC_IDLE_TAIL), Status::Idle);
    }

    #[test]
    fn opencode_idle_on_welcome_screen() {
        assert_eq!(classify("opencode", "OpenCode", OC_WELCOME_TAIL), Status::Idle);
    }

    #[test]
    fn opencode_blocked_on_approve_prompt() {
        assert_eq!(classify("opencode", "OpenCode", OC_BLOCKED_TAIL), Status::Blocked);
    }

    #[test]
    fn opencode_unknown_when_no_signals() {
        assert_eq!(classify("opencode", "OpenCode", "random output"), Status::Unknown);
    }

    #[test]
    fn opencode_working_takes_precedence_over_idle() {
        assert_eq!(classify("opencode", "OpenCode", OC_WORKING_TAIL), Status::Working);
    }

    #[test]
    fn opencode_model_extraction() {
        assert_eq!(
            extract_model("opencode", OC_IDLE_TAIL).as_deref(),
            Some("US Anthropic Claude Opus 4.6 Amazon Bedrock"),
        );
    }

    #[test]
    fn opencode_model_none_when_no_build_line() {
        assert_eq!(extract_model("opencode", "random output"), None);
    }

    #[test]
    fn opencode_status_line_is_build_line() {
        assert_eq!(
            extract_status_line("opencode", OC_IDLE_TAIL).as_deref(),
            Some("Build · US Anthropic Claude Opus 4.6 Amazon Bedrock"),
        );
    }

    #[test]
    fn generic_model_is_none() {
        assert_eq!(extract_model("codex", "anything"), None);
    }
}
