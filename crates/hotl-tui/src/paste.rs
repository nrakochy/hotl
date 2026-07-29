//! Paste compaction: the token vocabulary for dropped images and long pastes.
//!
//! A bracketed paste that is a single image path (a terminal drag-and-drop)
//! compacts to `[Image #N]`; a paste of three or more lines compacts to
//! `[Pasted text #N +M lines]`. The compacted content lives in a side table
//! (`Attachment`) on `State` — deliberately not in `Editor`, whose buffer is
//! plain lines and whose `set_text` (used by `$EDITOR` round-trips and
//! history recall) replaces the buffer wholesale and must not orphan tokens.
//!
//! Tokens are ordinary text in the buffer: the human can see, move, and
//! delete them. A mangled token submits literally and its side-table entry is
//! silently dropped — that is the escape hatch, not a bug. Everything that
//! knows the token grammar lives in this one module.
//!
//! At submit, paste tokens expand back to their content (the core holds it;
//! no I/O), while image tokens stay inline and their paths ride the
//! [`PromptPayload`] to the runtime, which alone may read the filesystem.
//! `file://` URIs and Windows drive paths are out of scope for v1 — they fail
//! the path-shape gate and insert literally, a safe degrade.

use std::ops::Range;

/// Extensions that compact, mapped to their IANA media types. Exactly the
/// set the model APIs accept as image blocks — a wider net would produce
/// `[Image #N]` tokens the wire could never honor.
const IMAGE_EXTENSIONS: [(&str, &str); 5] = [
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
];

/// One compacted paste riding the current draft, keyed positionally: the Nth
/// `Image` entry owns `[Image #N]`, the Nth `Paste` entry owns
/// `[Pasted text #N +lines lines]`. Numbering is per-draft; the table is
/// cleared on every submit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attachment {
    /// A dropped image path. The runtime reads + base64s it at send time —
    /// this crate is pure and never touches the fs.
    Image { path: String, media_type: String },
    /// A 3+-line paste; `lines` regenerates the exact marker at expansion.
    Paste { text: String, lines: usize },
}

/// What a bracketed paste turned out to be, after CRLF normalization.
#[derive(Debug, PartialEq, Eq)]
pub enum PasteKind {
    Image { path: String, media_type: String },
    Text { text: String, lines: usize },
    Literal,
}

/// A draft's wire-bound content. The core fills `text` (paste tokens
/// expanded, `[Image #N]` tokens left inline) and `images` with
/// `data: None`; the runtime seam reads the files and fills `data`; the wire
/// codec ships only filled entries.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PromptPayload {
    pub text: String,
    pub images: Vec<ImageAttachment>,
}

impl PromptPayload {
    /// A payload with no attachments — slash-command desugaring and other
    /// synthesized prompts.
    pub fn text_only(text: String) -> Self {
        Self {
            text,
            images: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageAttachment {
    /// The exact inline token, e.g. `[Image #2]` — lets the runtime annotate
    /// a failed read in place.
    pub marker: String,
    pub path: String,
    pub media_type: String,
    /// Base64 (RFC 4648 standard alphabet, padded); `None` until the runtime
    /// seam encodes the file.
    pub data: Option<String>,
}

/// Classify a paste. Normalizes line endings the same way
/// `Editor::insert_text` does, so the stored content matches what literal
/// insertion would have produced.
pub fn classify(text: &str) -> PasteKind {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    if let Some((path, media_type)) = dropped_path(&normalized) {
        return PasteKind::Image { path, media_type };
    }
    // `str::lines` ignores a trailing newline: "a\nb\n" is two lines of
    // content, not three — only pastes with 3+ lines of content compact.
    let lines = normalized.lines().count();
    if lines >= 3 {
        return PasteKind::Text {
            text: normalized,
            lines,
        };
    }
    PasteKind::Literal
}

pub fn image_marker(n: usize) -> String {
    format!("[Image #{n}]")
}

pub fn paste_marker(n: usize, lines: usize) -> String {
    format!("[Pasted text #{n} +{lines} lines]")
}

/// Apply `(marker, replacement)` pairs to `text` in a single left-to-right
/// pass. Scanning the ORIGINAL text is the whole point: a replacement's body
/// may contain another marker verbatim, and a second `str::replace` pass would
/// rewrite it.
fn substitute(text: &str, subs: &[(String, String)]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        match subs
            .iter()
            // An empty marker matches at every offset without consuming any
            // input, so `i` would never advance — skip it, don't loop forever.
            .filter(|(m, _)| !m.is_empty())
            .filter_map(|(m, r)| text[i..].find(m.as_str()).map(|off| (i + off, m, r)))
            .min_by_key(|&(at, m, _)| (at, std::cmp::Reverse(m.len())))
        {
            Some((at, m, r)) => {
                out.push_str(&text[i..at]);
                out.push_str(r);
                i = at + m.len();
            }
            None => {
                out.push_str(&text[i..]);
                break;
            }
        }
    }
    out
}

/// Expand a submitted draft for the wire. Paste tokens are replaced by their
/// content; image tokens stay inline (the model sees where the image sat) and
/// surviving images become payload entries with `data: None`.
///
/// INVARIANT: image survival and paste substitution are both decided against
/// the ORIGINAL buffer text, never a partially-expanded accumulator — a paste
/// body containing `[Image #1]` or another paste's marker cannot resurrect a
/// deleted token or get rewritten. Enforced by
/// `paste_content_containing_a_marker_cannot_resurrect_an_image` (image half)
/// and `one_pastes_body_is_never_rewritten_by_a_later_pastes_marker`
/// (substitution half).
pub fn expand_for_wire(text: &str, attachments: &[Attachment]) -> PromptPayload {
    let mut subs = Vec::new();
    let mut images = Vec::new();
    let (mut img_n, mut paste_n) = (0usize, 0usize);
    for att in attachments {
        match att {
            Attachment::Image { path, media_type } => {
                img_n += 1;
                let marker = image_marker(img_n);
                if text.contains(&marker) {
                    images.push(ImageAttachment {
                        marker,
                        path: path.clone(),
                        media_type: media_type.clone(),
                        data: None,
                    });
                }
            }
            Attachment::Paste {
                text: content,
                lines,
            } => {
                paste_n += 1;
                subs.push((paste_marker(paste_n, *lines), content.clone()));
            }
        }
    }
    PromptPayload {
        text: substitute(text, &subs),
        images,
    }
}

/// Expand a submitted draft for the on-disk prompt history: paste tokens
/// become their content, image tokens become their path — exactly the bytes
/// pre-compaction behavior would have written, so a recalled entry is
/// self-contained (a token without its side table is dead text).
pub fn expand_for_history(text: &str, attachments: &[Attachment]) -> String {
    let mut subs = Vec::new();
    let (mut img_n, mut paste_n) = (0usize, 0usize);
    for att in attachments {
        match att {
            Attachment::Image { path, .. } => {
                img_n += 1;
                subs.push((image_marker(img_n), path.clone()));
            }
            Attachment::Paste {
                text: content,
                lines,
            } => {
                paste_n += 1;
                subs.push((paste_marker(paste_n, *lines), content.clone()));
            }
        }
    }
    substitute(text, &subs)
}

/// Byte ranges of well-formed tokens in one visual row, for chip styling.
/// Grammar-only, no side table: a stale token still styles, which is honest —
/// it will also submit literally.
pub fn token_ranges(s: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(off) = s[i..].find('[') {
        let start = i + off;
        match token_len_at(&s[start..]) {
            Some(len) => {
                out.push(start..start + len);
                i = start + len;
            }
            None => i = start + 1,
        }
    }
    out
}

/// If `s` ends with one full token, its length in chars — grammar-only, no
/// side table. The backspace arm calls `token_suffix_chars_in` instead,
/// which layers the live-attachment check on top of this same boundary
/// check; kept for its own contract and tests. Tokens are pure ASCII, but
/// the editor counts columns in chars, so the contract is chars.
pub fn token_suffix_chars(s: &str) -> Option<usize> {
    let start = s.rfind('[')?;
    let len = token_len_at(&s[start..])?;
    (start + len == s.len()).then(|| s[start..].chars().count())
}

/// The marker strings the side table currently backs, in draft order. The
/// backspace arm needs them: the grammar alone cannot tell a live token from
/// prose that happens to end like one.
pub fn live_tokens(attachments: &[Attachment]) -> Vec<String> {
    let (mut img_n, mut paste_n) = (0usize, 0usize);
    attachments
        .iter()
        .map(|att| match att {
            Attachment::Image { .. } => {
                img_n += 1;
                image_marker(img_n)
            }
            Attachment::Paste { lines, .. } => {
                paste_n += 1;
                paste_marker(paste_n, *lines)
            }
        })
        .collect()
}

/// `token_suffix_chars`, restricted to tokens a live attachment backs.
pub fn token_suffix_chars_in(s: &str, live: &[String]) -> Option<usize> {
    let n = token_suffix_chars(s)?;
    let tok = &s[s.rfind('[')?..];
    live.iter().any(|m| m == tok).then_some(n)
}

/// Token length in bytes when `s` begins with a well-formed token.
fn token_len_at(s: &str) -> Option<usize> {
    fn digits(s: &str) -> Option<usize> {
        let n = s.bytes().take_while(u8::is_ascii_digit).count();
        (n > 0).then_some(n)
    }
    if let Some(rest) = s.strip_prefix("[Image #") {
        let d = digits(rest)?;
        return rest[d..]
            .starts_with(']')
            .then_some("[Image #".len() + d + 1);
    }
    if let Some(rest) = s.strip_prefix("[Pasted text #") {
        let d1 = digits(rest)?;
        let after = rest[d1..].strip_prefix(" +")?;
        let d2 = digits(after)?;
        after[d2..].strip_prefix(" lines]")?;
        return Some("[Pasted text #".len() + d1 + 2 + d2 + " lines]".len());
    }
    None
}

/// Recognize a paste that is a single dropped image path. Terminals deliver
/// drag-and-drop as a bracketed paste of the path in one of three quoting
/// forms, usually with a trailing space or newline:
///
/// - bare with backslash escapes: `/a/My\ Shot\ 2.png` (macOS Terminal, iTerm2)
/// - single-quoted, `'` escaped as `'\''`: `'/a/My Shot.png'`
/// - double-quoted with `\" \\ \$ \`` escapes: `"/a/My Shot.png"`
///
/// Two gates keep prose honest: the unescaped candidate must LOOK like a path
/// (starts `/`, `~`, `./`, `../` — drops are always absolute, so a bare
/// `logo.png` mentioned in a sentence stays literal), and must end in a known
/// image extension. In the bare form an unescaped space is the discriminator
/// between a dropped path and a sentence that happens to mention one.
fn dropped_path(text: &str) -> Option<(String, String)> {
    let t = text.trim();
    if t.is_empty() || t.contains('\n') {
        return None;
    }
    let candidate = if t.len() >= 2 && t.starts_with('\'') && t.ends_with('\'') {
        t[1..t.len() - 1].replace("'\\''", "'")
    } else if t.len() >= 2 && t.starts_with('"') && t.ends_with('"') {
        unescape_double_quoted(&t[1..t.len() - 1])?
    } else {
        unescape_bare(t)?
    };
    if !(candidate.starts_with('/')
        || candidate.starts_with('~')
        || candidate.starts_with("./")
        || candidate.starts_with("../"))
    {
        return None;
    }
    let media_type = media_type_for(&candidate)?;
    Some((candidate, media_type.to_string()))
}

/// Undo shell double-quoting. Only `\" \\ \$ \`` are escapes inside double
/// quotes; any other backslash pair passes through. An unescaped interior `"`
/// means this was never one quoted path.
fn unescape_double_quoted(inner: &str) -> Option<String> {
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(e @ ('"' | '\\' | '$' | '`')) => out.push(e),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            },
            '"' => return None,
            _ => out.push(c),
        }
    }
    Some(out)
}

/// Undo bare backslash escaping. Unescaped whitespace disqualifies: a real
/// dropped path arrives with its spaces escaped, prose does not.
fn unescape_bare(t: &str) -> Option<String> {
    let mut out = String::with_capacity(t.len());
    let mut chars = t.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(e) => out.push(e),
                None => out.push('\\'),
            },
            c if c.is_whitespace() => return None,
            _ => out.push(c),
        }
    }
    Some(out)
}

/// Media type for a path whose final component has a known image extension
/// (ASCII case-insensitive) and a non-empty stem.
fn media_type_for(path: &str) -> Option<&'static str> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let (stem, ext) = name.rsplit_once('.')?;
    if stem.is_empty() {
        return None;
    }
    let ext = ext.to_ascii_lowercase();
    IMAGE_EXTENSIONS
        .iter()
        .find(|(e, _)| *e == ext)
        .map(|(_, mt)| *mt)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(path: &str, media_type: &str) -> PasteKind {
        PasteKind::Image {
            path: path.into(),
            media_type: media_type.into(),
        }
    }

    #[test]
    fn bare_escaped_quoted_and_trailing_space_forms_all_unwrap() {
        for (paste, want_path, want_mt) in [
            ("/a/b.png", "/a/b.png", "image/png"),
            ("/a/b.png ", "/a/b.png", "image/png"),
            ("/a/b.png\n", "/a/b.png", "image/png"),
            ("/a/My\\ Shot\\ 2.png", "/a/My Shot 2.png", "image/png"),
            ("'/a/My Shot.png'", "/a/My Shot.png", "image/png"),
            ("'/a/it'\\''s.png'", "/a/it's.png", "image/png"),
            ("\"/a/My Shot.png\"", "/a/My Shot.png", "image/png"),
            ("~/x.JPEG", "~/x.JPEG", "image/jpeg"),
            ("./x.webp", "./x.webp", "image/webp"),
            ("../shots/x.gif", "../shots/x.gif", "image/gif"),
            ("/a/b.JPG", "/a/b.JPG", "image/jpeg"),
        ] {
            assert_eq!(classify(paste), image(want_path, want_mt), "{paste:?}");
        }
    }

    #[test]
    fn a_sentence_mentioning_an_image_stays_literal() {
        for paste in [
            "see shot.png thanks",
            "logo.png",
            "/a/b.txt",
            "/a/b.png.zip",
            "/a/b.png /a/c.png",
            "/a/.png",
            "http://x.example/a.png",
            "",
            " ",
        ] {
            assert_eq!(classify(paste), PasteKind::Literal, "{paste:?}");
        }
    }

    #[test]
    fn three_lines_compact_two_do_not() {
        assert_eq!(classify("a\nb"), PasteKind::Literal);
        // A trailing newline is not a third line of content.
        assert_eq!(classify("a\nb\n"), PasteKind::Literal);
        assert_eq!(
            classify("a\nb\nc"),
            PasteKind::Text {
                text: "a\nb\nc".into(),
                lines: 3
            }
        );
        // CRLF input normalizes before counting.
        assert_eq!(
            classify("a\r\nb\r\nc"),
            PasteKind::Text {
                text: "a\nb\nc".into(),
                lines: 3
            }
        );
    }

    #[test]
    fn expansion_replaces_paste_markers_and_keeps_image_markers() {
        let atts = vec![
            Attachment::Image {
                path: "/a/b.png".into(),
                media_type: "image/png".into(),
            },
            Attachment::Paste {
                text: "x\ny\nz".into(),
                lines: 3,
            },
        ];
        let text = "look at [Image #1] and [Pasted text #1 +3 lines] please";
        let p = expand_for_wire(text, &atts);
        assert_eq!(p.text, "look at [Image #1] and x\ny\nz please");
        assert_eq!(p.images.len(), 1);
        assert_eq!(p.images[0].marker, "[Image #1]");
        assert_eq!(p.images[0].path, "/a/b.png");
        assert_eq!(p.images[0].media_type, "image/png");
        assert_eq!(p.images[0].data, None);
    }

    #[test]
    fn a_deleted_marker_drops_its_orphan() {
        let atts = vec![
            Attachment::Image {
                path: "/a/b.png".into(),
                media_type: "image/png".into(),
            },
            Attachment::Paste {
                text: "long".into(),
                lines: 3,
            },
        ];
        let p = expand_for_wire("no tokens here", &atts);
        assert_eq!(p.text, "no tokens here");
        assert!(p.images.is_empty());
    }

    #[test]
    fn paste_content_containing_a_marker_cannot_resurrect_an_image() {
        let atts = vec![
            Attachment::Image {
                path: "/a/b.png".into(),
                media_type: "image/png".into(),
            },
            Attachment::Paste {
                text: "sneaky [Image #1] inside".into(),
                lines: 3,
            },
        ];
        // The human deleted the image token; only the paste token remains.
        let p = expand_for_wire("[Pasted text #1 +3 lines]", &atts);
        assert_eq!(p.text, "sneaky [Image #1] inside");
        assert!(p.images.is_empty());
    }

    #[test]
    fn substitute_ignores_an_empty_marker_instead_of_looping_forever() {
        // Guard, not a feature: no producer emits "", but a bogus one must not hang.
        assert_eq!(substitute("abc", &[(String::new(), "X".into())]), "abc");
    }

    #[test]
    fn one_pastes_body_is_never_rewritten_by_a_later_pastes_marker() {
        let atts = vec![
            Attachment::Paste {
                text: "quoting [Pasted text #2 +3 lines] here".into(),
                lines: 3,
            },
            Attachment::Paste {
                text: "SECOND".into(),
                lines: 3,
            },
        ];
        let p = expand_for_wire(
            "[Pasted text #1 +3 lines] and [Pasted text #2 +3 lines]",
            &atts,
        );
        // The literal marker inside paste #1's body must survive verbatim.
        assert_eq!(p.text, "quoting [Pasted text #2 +3 lines] here and SECOND");
    }

    #[test]
    fn history_expansion_has_the_same_one_pass_guarantee() {
        // Paste must precede Image in attachment order: the accumulator bug
        // only fires when an earlier step's inserted body is re-scanned by a
        // later step's replace — Image-then-Paste (the brief's original
        // ordering) never re-visits the accumulator and does not discriminate.
        let atts = vec![
            Attachment::Paste {
                text: "quoting [Image #1] here".into(),
                lines: 3,
            },
            Attachment::Image {
                path: "/a/b.png".into(),
                media_type: "image/png".into(),
            },
        ];
        // The image token the human deleted must not reappear from inside a body.
        assert_eq!(
            expand_for_history("[Pasted text #1 +3 lines]", &atts),
            "quoting [Image #1] here"
        );
    }

    #[test]
    fn history_expansion_restores_paths_and_paste_text() {
        let atts = vec![
            Attachment::Image {
                path: "/a/b.png".into(),
                media_type: "image/png".into(),
            },
            Attachment::Paste {
                text: "x\ny\nz".into(),
                lines: 3,
            },
        ];
        assert_eq!(
            expand_for_history("[Image #1] then [Pasted text #1 +3 lines]", &atts),
            "/a/b.png then x\ny\nz"
        );
    }

    #[test]
    fn markers_number_per_kind_independently() {
        let atts = vec![
            Attachment::Image {
                path: "/a/1.png".into(),
                media_type: "image/png".into(),
            },
            Attachment::Paste {
                text: "p1".into(),
                lines: 3,
            },
            Attachment::Image {
                path: "/a/2.png".into(),
                media_type: "image/png".into(),
            },
        ];
        let text = "[Image #1] [Pasted text #1 +3 lines] [Image #2]";
        let p = expand_for_wire(text, &atts);
        assert_eq!(p.images.len(), 2);
        assert_eq!(p.images[0].path, "/a/1.png");
        assert_eq!(p.images[1].path, "/a/2.png");
        assert_eq!(p.text, "[Image #1] p1 [Image #2]");
    }

    #[test]
    fn token_ranges_find_only_well_formed_tokens() {
        let s = "a [Image #1] b [Image #] c [Pasted text #2 +10 lines] [Pasted text #1] d";
        let ranges = token_ranges(s);
        let found: Vec<&str> = ranges.iter().map(|r| &s[r.clone()]).collect();
        assert_eq!(found, vec!["[Image #1]", "[Pasted text #2 +10 lines]"]);
    }

    #[test]
    fn token_suffix_chars_matches_only_a_full_token() {
        assert_eq!(token_suffix_chars("say [Image #12]"), Some(11));
        assert_eq!(
            token_suffix_chars("[Pasted text #1 +3 lines]"),
            Some("[Pasted text #1 +3 lines]".chars().count())
        );
        assert_eq!(token_suffix_chars("say [Image #12] "), None);
        assert_eq!(token_suffix_chars("say [Image #]"), None);
        assert_eq!(token_suffix_chars("say Image #12]"), None);
    }

    #[test]
    fn a_token_with_no_attachment_behind_it_is_not_a_token_to_swallow() {
        let live = live_tokens(&[Attachment::Image {
            path: "/a/b.png".into(),
            media_type: "image/png".into(),
        }]);
        assert_eq!(live, vec!["[Image #1]".to_string()]);
        assert_eq!(token_suffix_chars_in("look at [Image #1]", &live), Some(10));
        assert_eq!(
            token_suffix_chars_in("why does it render [Image #2]", &live),
            None
        );
        assert_eq!(
            token_suffix_chars_in("why does it render [Image #1]", &[]),
            None
        );
    }
}
