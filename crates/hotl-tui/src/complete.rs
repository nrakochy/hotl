//! `/`-command completion: the pure half. Given the command table and the
//! editor buffer, decide whether a popup is open and what is in it. There is
//! no "completion mode" — the popup is a projection of the buffer, recomputed
//! after every keystroke, so it can never fall out of sync with what is typed.
//! Column arithmetic is in char indices (never bytes), matching `vim.rs`.

/// One completable command: a TUI built-in or a loadable skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    /// Without the leading `/` — `"rename"`, `"superpowers:brainstorming"`.
    pub name: String,
    /// Client-facing only; empty renders as name-only. Content enters
    /// context only when asked for: the always-sent tool description omits
    /// it — the model sees it only by calling the skill tool itself.
    pub description: String,
    /// Built-ins sort above skills at equal match quality.
    pub builtin: bool,
}

/// The open popup. `matches` holds indices into the caller's command table,
/// best first; `selected` indexes into `matches`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    pub matches: Vec<usize>,
    pub selected: usize,
}

/// The TUI's own commands, in `slash_command`'s dispatch order. Descriptions
/// are hand-written here because built-ins have no roster to read them from.
const BUILTINS: [(&str, &str); 9] = [
    ("rename", "name this session"),
    ("plan", "toggle plan mode (file edits always ask)"),
    ("mode", "set the permission mode"),
    ("reload", "re-read config.toml"),
    ("help", "show the key bindings"),
    ("status", "what this session is running"),
    ("cost", "token and cost breakdown"),
    ("clear", "clear the transcript view"),
    ("quit", "leave the console"),
];

pub fn builtins() -> Vec<Command> {
    BUILTINS
        .iter()
        .map(|(name, description)| Command {
            name: (*name).into(),
            description: (*description).into(),
            builtin: true,
        })
        .collect()
}

/// The command word being typed: `Some("re")` for `/re`, `Some("")` for a
/// bare `/` (which matches everything), `None` when the buffer is not a
/// single-line command-in-progress. `None` is what closes the popup.
pub fn word(buffer: &str, cursor: (usize, usize)) -> Option<String> {
    if cursor.0 != 0 || buffer.contains('\n') {
        return None;
    }
    let chars: Vec<char> = buffer.chars().collect();
    if chars.first() != Some(&'/') {
        return None;
    }
    // `cursor.1 == 0` is the cursor sitting before the `/` — not in the word.
    let upto = cursor.1.min(chars.len());
    if upto == 0 {
        return None;
    }
    if chars[1..upto].iter().any(|c| c.is_whitespace()) {
        return None;
    }
    Some(chars[1..upto].iter().collect())
}

/// Match quality, sortable ascending: exact matches before prefix hits
/// before substring hits, then built-ins before skills, then shorter names
/// before longer as the final tiebreak. `None` = no match.
///
/// The exact tier exists so a skill named identically to a builtin's strict
/// prefix (a skill named `mod` against the builtin `mode`) is still
/// reachable via Enter: typing `/mod` makes the skill an exact match while
/// the builtin is only a prefix match, so the skill wins regardless of
/// built-in status. Built-ins must outrank skills within a shared tier —
/// otherwise any skill shorter than the shortest builtin (this repo ships
/// `run`, `init`, `loop`, `auth`, all shorter than `mode`/`plan`) would
/// pre-empt every builtin on a bare `/`.
fn rank(cmd: &Command, needle: &str) -> Option<(u8, bool, usize)> {
    let name = cmd.name.to_lowercase();
    let tier = if name == needle {
        0
    } else if name.starts_with(needle) {
        1
    } else if name.contains(needle) {
        2
    } else {
        return None;
    };
    Some((tier, !cmd.builtin, name.chars().count()))
}

/// The popup for this buffer, or `None` when there is nothing to show.
/// Pure: the caller owns `dismissed` and the command table.
pub fn recompute(
    commands: &[Command],
    buffer: &str,
    cursor: (usize, usize),
    dismissed: bool,
) -> Option<Completion> {
    if dismissed {
        return None;
    }
    let needle = word(buffer, cursor)?.to_lowercase();
    let mut scored: Vec<((u8, bool, usize, String), usize)> = commands
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            rank(c, &needle)
                .map(|(tier, skill, len)| ((tier, skill, len, c.name.to_lowercase()), i))
        })
        .collect();
    if scored.is_empty() {
        return None;
    }
    scored.sort();
    Some(Completion {
        matches: scored.into_iter().map(|(_, i)| i).collect(),
        selected: 0,
    })
}

/// Splice `name` in place of the command word, leaving one trailing space and
/// any existing argument intact. The result always ends the popup: it either
/// contains whitespace before the end or is followed by one.
pub fn accept(buffer: &str, cursor: (usize, usize), name: &str) -> String {
    let chars: Vec<char> = buffer.chars().collect();
    let from = cursor.1.min(chars.len());
    // The word ends at the first whitespace at or after the cursor — anything
    // past that is an argument the human already typed, and it survives.
    let end = chars[from..]
        .iter()
        .position(|c| c.is_whitespace())
        .map_or(chars.len(), |p| from + p);
    let tail: String = chars[end..].iter().collect();
    format!("/{name} {}", tail.trim_start())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The command word is what has been typed between the `/` and the
    /// cursor. Anything that is not a command-in-progress yields `None`,
    /// which is what closes the popup.
    #[test]
    fn word_is_the_text_between_the_slash_and_the_cursor() {
        assert_eq!(word("/re", (0, 3)).as_deref(), Some("re"));
        assert_eq!(word("/", (0, 1)).as_deref(), Some(""));
        // Cursor parked back inside the word completes on the shorter prefix.
        assert_eq!(word("/rename", (0, 3)).as_deref(), Some("re"));
    }

    #[test]
    fn word_is_none_once_the_input_stops_being_a_command_word() {
        assert_eq!(word("fix the bug", (0, 3)), None, "no leading slash");
        assert_eq!(word("/mode ask", (0, 9)), None, "whitespace before cursor");
        assert_eq!(word("/re\nx", (1, 1)), None, "multi-line buffer");
        assert_eq!(word("/re", (0, 0)), None, "cursor before the slash");
    }

    fn table() -> Vec<Command> {
        let mut cmds = builtins();
        for (name, description) in [
            ("review", "review a pull request"),
            ("rag-recall", "search the retrieval index"),
            ("superpowers:brainstorming", "turn an idea into a design"),
            // Shorter than every builtin — pins "built-ins first" against the
            // ordering that puts name length ahead of built-in status.
            ("run", "launch and drive the app"),
        ] {
            cmds.push(Command {
                name: name.into(),
                description: description.into(),
                builtin: false,
            });
        }
        cmds
    }

    fn names(commands: &[Command], c: &Completion) -> Vec<String> {
        c.matches
            .iter()
            .map(|&i| commands[i].name.clone())
            .collect()
    }

    #[test]
    fn a_bare_slash_matches_every_command_builtins_first() {
        let cmds = table();
        let c = recompute(&cmds, "/", (0, 1), false).expect("open on a bare slash");
        assert_eq!(c.selected, 0);
        assert_eq!(
            names(&cmds, &c),
            vec![
                "cost",
                "help",
                "mode",
                "plan",
                "quit",
                "clear",
                "reload",
                "rename",
                "status",
                "run",
                "review",
                "rag-recall",
                "superpowers:brainstorming",
            ],
            "built-ins first even though `run` is shorter than every builtin; \
             shorter names before longer within each group"
        );
    }

    /// Prefix beats substring, so the command you are spelling stays on top
    /// even when a longer name also contains those letters.
    #[test]
    fn prefix_matches_rank_above_substring_matches() {
        let cmds = table();
        let c = recompute(&cmds, "/re", (0, 3), false).expect("open");
        assert_eq!(
            names(&cmds, &c),
            vec!["reload", "rename", "review", "rag-recall"],
            "`rag-recall` contains `re` (at \"rag-\"[re]\"call\") and must sort below the prefix hits; \
             `mode` has no `r` at all and must not appear"
        );
        // `reload` and `rename` are both 6-char built-in prefix hits, so the
        // table's own ordering decides and `/re` no longer means `rename`.
        // One more letter separates them, which is the price of a builtin
        // that shares a prefix — not a special case worth ranking around.
        let c = recompute(&cmds, "/ren", (0, 4), false).expect("open");
        assert_eq!(names(&cmds, &c), vec!["rename"]);
    }

    /// Regression: a skill whose name is a strict prefix of a longer builtin
    /// must still rank first, so Enter loads the skill rather than running
    /// the builtin's usage message. Before `rank` ordered length ahead of
    /// built-in status, `mode` (a builtin) always outranked a shorter skill
    /// named `mod`, making that skill unreachable via Enter.
    #[test]
    fn a_skill_that_is_a_strict_prefix_of_a_builtin_still_ranks_first() {
        let mut cmds = table();
        cmds.push(Command {
            name: "mod".into(),
            description: "a skill, not the builtin".into(),
            builtin: false,
        });
        let c = recompute(&cmds, "/mod", (0, 4), false).expect("open");
        assert_eq!(
            names(&cmds, &c),
            vec!["mod", "mode"],
            "the shorter skill name must sort ahead of the longer builtin"
        );
        assert_eq!(c.selected, 0);
    }

    #[test]
    fn matching_is_case_insensitive_and_reaches_qualified_names() {
        let cmds = table();
        let c = recompute(&cmds, "/BRAIN", (0, 6), false).expect("open");
        assert_eq!(names(&cmds, &c), vec!["superpowers:brainstorming"]);
    }

    #[test]
    fn no_match_and_dismissed_both_close_the_popup() {
        let cmds = table();
        assert_eq!(recompute(&cmds, "/zzz", (0, 4), false), None, "no match");
        assert_eq!(recompute(&cmds, "/re", (0, 3), true), None, "dismissed");
        assert_eq!(
            recompute(&cmds, "/mode ask", (0, 9), false),
            None,
            "arg typed"
        );
        assert_eq!(recompute(&[], "/re", (0, 3), false), None, "empty table");
    }

    /// Accepting always leaves a trailing space: the next thing typed is an
    /// argument, and the space is also what closes the popup.
    #[test]
    fn accept_replaces_the_word_and_leaves_a_trailing_space() {
        assert_eq!(accept("/re", (0, 3), "rename"), "/rename ");
        assert_eq!(accept("/", (0, 1), "plan"), "/plan ");
    }

    #[test]
    fn accept_keeps_an_existing_argument_without_doubling_the_space() {
        assert_eq!(accept("/re fix-auth", (0, 3), "rename"), "/rename fix-auth");
    }

    /// Char indices, not bytes: a multibyte prefix must not split a codepoint
    /// or miscount where the word ends.
    #[test]
    fn accept_counts_characters_not_bytes() {
        assert_eq!(accept("/né", (0, 3), "rename"), "/rename ");
        assert_eq!(accept("/né args", (0, 3), "rename"), "/rename args");
    }
}
