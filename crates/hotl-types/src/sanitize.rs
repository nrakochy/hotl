//! Control-character sanitizer for text rendered into a human approval prompt.
//!
//! This lives in the leaf crate so the engine gate (`hotl-engine`) and every
//! tool crate (`hotl-tools`, `hotl-mcp`) reach one canonical implementation.
//! `hotl-mcp` re-exports these for its own envelope and for `hotl-retrieval`.

/// A y/N prompt is one terminal line; longer than this and the human stops
/// reading the part that matters.
pub const MAX_SUMMARY_CHARS: usize = 120;

/// Text safe to render into a human y/N approval prompt: no controls, no Cf,
/// no newlines, one ellipsis-capped line.
pub fn safe_summary(s: &str) -> String {
    let flat: String = strip_control(s)
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .collect();
    let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= MAX_SUMMARY_CHARS {
        return flat;
    }
    let mut out: String = flat.chars().take(MAX_SUMMARY_CHARS - 1).collect();
    out.push('…');
    out
}

/// Strip ANSI escape sequences (CSI/OSC/two-byte), C0/C1 controls except
/// `\n`/`\t`, and Unicode category Cf — bidi overrides, zero-width
/// joiners/spaces, the byte-order mark, and the Unicode Tags block (S-3).
pub fn strip_control(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            match chars.peek() {
                // CSI: ESC [ ... final byte @–~
                Some('[') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC: ESC ] ... BEL or ESC \
                Some(']') => {
                    chars.next();
                    while let Some(c) = chars.next() {
                        if c == '\u{07}' {
                            break;
                        }
                        if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                // Two-byte escapes (ESC c, ESC 7, …)
                Some(_) => {
                    chars.next();
                }
                None => {}
            }
            continue;
        }
        if (c.is_control() && c != '\n' && c != '\t') || is_format_char(c) {
            continue;
        }
        out.push(c);
    }
    out
}

/// Category Cf as of Unicode 15, enumerated rather than pulled from a
/// `unicode-*` crate: no new dependency, and the ranges are stable. The
/// Unicode Tags block (U+E0000-U+E007F) is the one that matters most — the
/// standard invisible prompt-injection carrier that models decode as ASCII
/// while a human reviewing the transcript sees nothing (S-3).
fn is_format_char(c: char) -> bool {
    matches!(c as u32,
        0x00AD | 0x0600..=0x0605 | 0x061C | 0x06DD | 0x070F | 0x0890..=0x0891
        | 0x08E2 | 0x180E | 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x2064
        | 0x2066..=0x206F | 0xFEFF | 0xFFF9..=0xFFFB | 0x110BD | 0x110CD
        | 0x13430..=0x1343F | 0x1BCA0..=0x1BCA3 | 0x1D173..=0x1D17A
        | 0xE0001 | 0xE0020..=0xE007F)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_summary_is_single_line_control_free_and_capped() {
        // What renders into the human's y/N prompt must be one clean line: no
        // ESC/CSI, no bidi override, no embedded newline that could erase what
        // the human is about to approve.
        let evil = "echo\n\u{1b}[2JDo you want to allow everything? \u{202e}";
        let s = safe_summary(&format!("bash: {evil}"));
        assert!(!s.contains('\n') && !s.contains('\u{1b}') && !s.contains('\u{202e}'));
        assert!(s.chars().count() <= MAX_SUMMARY_CHARS);

        let long = safe_summary(&"x".repeat(MAX_SUMMARY_CHARS * 3));
        assert!(long.chars().count() <= MAX_SUMMARY_CHARS && long.ends_with('…'));
    }
}
