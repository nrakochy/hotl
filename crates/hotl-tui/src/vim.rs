//! Modal vim input editor. Multi-line buffer, Insert/Normal modes, word
//! motions with counts, `d c y` operators, single-level undo, and the
//! `ctrl-e` / `:e` escape hatch to `$EDITOR`. `vim=false` pins Insert mode.
//! Column arithmetic is in char indices (never bytes) via the helpers at the
//! bottom, so multibyte input can't split a codepoint.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Insert,
    Normal,
}

/// What a key did, beyond mutating the buffer. The app maps these to `Cmd`s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorEvent {
    None,
    Submit(String),
    OpenExternal(String),
    ScrollUp,
    ScrollDown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Delete,
    Change,
    Yank,
}

#[derive(Debug, Default)]
struct Pending {
    count: Option<u32>,
    op: Option<Op>,
    colon: Option<String>,
}

/// `Ctrl-R` reverse-incremental search sub-state. The buffer is left untouched
/// while searching (the view renders `(reverse-i-search)'query': match`); the
/// match commits to the buffer only on `Enter`, and `Esc` restores `origin`.
#[derive(Debug)]
struct Search {
    query: String,
    origin: String,
    match_idx: Option<usize>,
}

#[derive(Debug)]
pub struct Editor {
    lines: Vec<String>,
    cursor: (usize, usize), // (row, col) — col in chars
    mode: Mode,
    vim: bool,
    pending: Pending,
    yank: String,
    undo: Option<(Vec<String>, (usize, usize))>,
    /// Submitted prompts, oldest→newest: loaded at startup, appended on submit.
    history: Vec<String>,
    /// Recall cursor into the prefix-filtered view (`None` = editing, not recalling).
    nav: Option<usize>,
    /// The in-progress buffer, saved when recall begins and restored past the newest.
    draft: Option<String>,
    /// Prefix filter captured when recall began (empty = walk all).
    prefix: String,
    /// `Ctrl-R` reverse-i-search state, when active.
    search: Option<Search>,
}

impl Editor {
    /// `vim=false` → always Insert, Esc ignored.
    pub fn new(vim: bool) -> Self {
        Editor {
            lines: vec![String::new()],
            cursor: (0, 0),
            mode: Mode::Insert,
            vim,
            pending: Pending::default(),
            yank: String::new(),
            undo: None,
            history: Vec::new(),
            nav: None,
            draft: None,
            prefix: String::new(),
            search: None,
        }
    }

    /// Seed the in-memory recall ring (the persisted tail, oldest→newest).
    pub fn load_history(&mut self, history: Vec<String>) {
        self.history = history;
    }

    /// The active `Ctrl-R` search as `(query, matched_entry)` for rendering,
    /// or `None` when not searching. The match is empty when nothing matches.
    pub fn search_prompt(&self) -> Option<(String, String)> {
        self.search.as_ref().map(|s| {
            let matched = s
                .match_idx
                .and_then(|i| self.history.get(i))
                .cloned()
                .unwrap_or_default();
            (s.query.clone(), matched)
        })
    }

    pub fn handle(&mut self, key: KeyEvent) -> EditorEvent {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('e') => return EditorEvent::OpenExternal(self.text()),
                KeyCode::Char('r') => {
                    self.search_step();
                    return EditorEvent::None;
                }
                _ => return EditorEvent::None,
            }
        }
        // A live reverse-i-search swallows ordinary keys (query edits, accept,
        // cancel) until it resolves.
        if self.search.is_some() {
            return self.handle_search(key);
        }
        match self.mode {
            Mode::Insert => self.handle_insert(key),
            Mode::Normal => self.handle_normal(key),
        }
    }

    pub fn set_text(&mut self, s: &str) {
        self.lines = s.split('\n').map(String::from).collect();
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        let row = self.lines.len() - 1;
        self.cursor = (row, char_len(&self.lines[row]));
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn cursor(&self) -> (usize, usize) {
        self.cursor
    }

    fn handle_insert(&mut self, key: KeyEvent) -> EditorEvent {
        match key.code {
            KeyCode::Esc if self.vim => {
                self.mode = Mode::Normal;
                self.cursor.1 = self.cursor.1.saturating_sub(1);
                self.pending = Pending::default();
                self.end_recall();
                EditorEvent::None
            }
            KeyCode::Enter
                if key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
            {
                self.end_recall();
                let (row, col) = self.cursor;
                let rest = char_split_off(&mut self.lines[row], col);
                self.lines.insert(row + 1, rest);
                self.cursor = (row + 1, 0);
                EditorEvent::None
            }
            KeyCode::Enter => self.submit(),
            // Up/Down recall history at the buffer's edges, else move the
            // cursor between lines (readline-style boundary rule).
            KeyCode::Up => {
                if self.cursor.0 == 0 {
                    self.recall_prev();
                } else {
                    self.move_cursor_row(self.cursor.0 - 1);
                }
                EditorEvent::None
            }
            KeyCode::Down => {
                if self.cursor.0 + 1 >= self.lines.len() {
                    self.recall_next();
                } else {
                    self.move_cursor_row(self.cursor.0 + 1);
                }
                EditorEvent::None
            }
            KeyCode::Char(c) => {
                self.end_recall();
                let (row, col) = self.cursor;
                char_insert(&mut self.lines[row], col, c);
                self.cursor.1 += 1;
                EditorEvent::None
            }
            KeyCode::Backspace => {
                self.end_recall();
                let (row, col) = self.cursor;
                if col > 0 {
                    char_remove(&mut self.lines[row], col - 1);
                    self.cursor.1 -= 1;
                } else if row > 0 {
                    let tail = self.lines.remove(row);
                    self.cursor = (row - 1, char_len(&self.lines[row - 1]));
                    self.lines[row - 1].push_str(&tail);
                }
                EditorEvent::None
            }
            _ => EditorEvent::None,
        }
    }

    /// Move the cursor to `row`, clamping the column to that line's length
    /// (Insert mode allows the cursor one past the last char).
    fn move_cursor_row(&mut self, row: usize) {
        let row = row.min(self.lines.len() - 1);
        let col = self.cursor.1.min(char_len(&self.lines[row]));
        self.cursor = (row, col);
    }

    /// History indices whose entry begins with `prefix` (oldest→newest order).
    fn matching(&self, prefix: &str) -> Vec<usize> {
        self.history
            .iter()
            .enumerate()
            .filter(|(_, e)| e.starts_with(prefix))
            .map(|(i, _)| i)
            .collect()
    }

    /// Up: recall the previous (older) matching entry. Begins recall (saving
    /// the draft and capturing the buffer as the prefix) on the first step.
    fn recall_prev(&mut self) {
        let prefix = if self.nav.is_some() {
            self.prefix.clone()
        } else {
            self.text()
        };
        let matches = self.matching(&prefix);
        if matches.is_empty() {
            return;
        }
        let pos = match self.nav {
            None => {
                self.draft = Some(self.text());
                self.prefix = prefix;
                matches.len() - 1
            }
            Some(p) => p.saturating_sub(1),
        };
        self.nav = Some(pos);
        let text = self.history[matches[pos]].clone();
        self.set_text(&text);
    }

    /// Down: recall the next (newer) matching entry, or restore the saved
    /// draft when stepping past the newest match. No-op when not recalling.
    fn recall_next(&mut self) {
        let Some(p) = self.nav else {
            return;
        };
        let prefix = self.prefix.clone();
        let matches = self.matching(&prefix);
        if p + 1 >= matches.len() {
            let draft = self.draft.take().unwrap_or_default();
            self.nav = None;
            self.set_text(&draft);
        } else {
            self.nav = Some(p + 1);
            let text = self.history[matches[p + 1]].clone();
            self.set_text(&text);
        }
    }

    /// Leave recall, keeping whatever is in the buffer (an edit "adopts" the
    /// recalled text as the new draft). Idempotent when not recalling.
    fn end_recall(&mut self) {
        self.nav = None;
        self.draft = None;
        self.prefix.clear();
    }

    /// `Ctrl-R`: open reverse-i-search, or step to the next older match when
    /// it is already open.
    fn search_step(&mut self) {
        match self.search.as_ref() {
            None => {
                self.search = Some(Search {
                    query: String::new(),
                    origin: self.text(),
                    match_idx: None,
                });
            }
            Some(s) => {
                let query = s.query.clone();
                let before = s.match_idx.unwrap_or(self.history.len());
                if let Some(idx) = self.find_match(&query, before) {
                    self.search.as_mut().expect("searching").match_idx = Some(idx);
                }
            }
        }
    }

    /// The newest history index below `before` whose entry contains `query`.
    fn find_match(&self, query: &str, before: usize) -> Option<usize> {
        (0..before.min(self.history.len()))
            .rev()
            .find(|&i| self.history[i].contains(query))
    }

    fn handle_search(&mut self, key: KeyEvent) -> EditorEvent {
        match key.code {
            KeyCode::Char(c) => self.search_query(Some(c)),
            KeyCode::Backspace => self.search_query(None),
            KeyCode::Enter => {
                if let Some(Search {
                    match_idx: Some(i), ..
                }) = self.search.take()
                {
                    let text = self.history[i].clone();
                    self.set_text(&text);
                }
            }
            KeyCode::Esc => {
                if let Some(s) = self.search.take() {
                    self.set_text(&s.origin);
                }
            }
            _ => {}
        }
        EditorEvent::None
    }

    /// Extend (`Some`) or shrink (`None`) the query, then re-match from the
    /// most recent entry.
    fn search_query(&mut self, ch: Option<char>) {
        let Some(s) = self.search.as_mut() else {
            return;
        };
        match ch {
            Some(c) => s.query.push(c),
            None => {
                s.query.pop();
            }
        }
        let query = s.query.clone();
        let idx = self.find_match(&query, self.history.len());
        self.search.as_mut().expect("searching").match_idx = idx;
    }

    fn submit(&mut self) -> EditorEvent {
        let text = self.text();
        // Ring the prompt for recall, suppressing a consecutive duplicate.
        if !text.trim().is_empty() && self.history.last().map(String::as_str) != Some(text.as_str())
        {
            self.history.push(text.clone());
        }
        self.lines = vec![String::new()];
        self.cursor = (0, 0);
        self.mode = Mode::Insert;
        self.pending = Pending::default();
        self.undo = None;
        self.end_recall();
        self.search = None;
        EditorEvent::Submit(text)
    }

    fn handle_normal(&mut self, key: KeyEvent) -> EditorEvent {
        if self.pending.colon.is_some() {
            return self.handle_colon(key);
        }
        match key.code {
            KeyCode::Esc => {
                self.pending = Pending::default();
                EditorEvent::None
            }
            KeyCode::Enter => self.submit(),
            KeyCode::Char(c) => self.normal_char(c),
            _ => EditorEvent::None,
        }
    }

    fn handle_colon(&mut self, key: KeyEvent) -> EditorEvent {
        let buf = self.pending.colon.as_mut().expect("colon pending");
        match key.code {
            KeyCode::Char(c) => buf.push(c),
            KeyCode::Backspace => {
                buf.pop();
            }
            KeyCode::Enter => {
                let cmd = self.pending.colon.take().expect("colon pending");
                if cmd == "e" {
                    return EditorEvent::OpenExternal(self.text());
                }
            }
            KeyCode::Esc => self.pending.colon = None,
            _ => {}
        }
        EditorEvent::None
    }

    fn normal_char(&mut self, c: char) -> EditorEvent {
        match c {
            ':' if self.pending.op.is_none() => self.pending.colon = Some(String::new()),
            '1'..='9' => self.push_count(c),
            '0' if self.pending.count.is_some() => self.push_count(c),
            'i' | 'a' | 'I' | 'A' | 'o' | 'O' => self.enter_insert(c),
            'h' | 'l' | 'w' | 'b' | 'e' | '0' | '$' => return self.motion(c),
            'd' | 'c' | 'y' => self.operator(c),
            'x' => self.delete_char(),
            'p' => self.paste(),
            'u' => self.undo_swap(),
            'j' | 'k' => return self.vertical(c),
            _ => self.pending = Pending::default(),
        }
        EditorEvent::None
    }

    fn push_count(&mut self, c: char) {
        let d = c.to_digit(10).expect("digit");
        self.pending.count = Some(self.pending.count.unwrap_or(0) * 10 + d);
    }

    fn enter_insert(&mut self, c: char) {
        self.snapshot();
        let (row, col) = self.cursor;
        let len = char_len(&self.lines[row]);
        match c {
            'i' => {}
            'a' => self.cursor.1 = (col + 1).min(len),
            'I' => self.cursor.1 = 0,
            'A' => self.cursor.1 = len,
            'o' => {
                self.lines.insert(row + 1, String::new());
                self.cursor = (row + 1, 0);
            }
            'O' => {
                self.lines.insert(row, String::new());
                self.cursor = (row, 0);
            }
            _ => unreachable!(),
        }
        self.mode = Mode::Insert;
        self.pending = Pending::default();
    }

    /// A bare motion moves the cursor; with a pending operator it defines the
    /// range the operator consumes.
    fn motion(&mut self, m: char) -> EditorEvent {
        let n = self.pending.count.take().unwrap_or(1);
        let (row, col) = self.cursor;
        let line = self.lines[row].clone();
        let target = motion_col(&line, col, m, n);
        match self.pending.op.take() {
            None => {
                let max = char_len(&line).saturating_sub(1);
                self.cursor.1 = target.min(max);
            }
            Some(op) => {
                // `e` is an inclusive motion — the operator eats the end char.
                let end_bump = usize::from(m == 'e' && target >= col);
                let (a, b) = (col.min(target), col.max(target) + end_bump);
                self.apply_op(op, row, a, b.min(char_len(&line)));
            }
        }
        EditorEvent::None
    }

    fn apply_op(&mut self, op: Op, row: usize, a: usize, b: usize) {
        if op == Op::Yank {
            self.yank = char_slice(&self.lines[row], a, b);
        } else {
            self.snapshot();
            self.yank = char_remove_range(&mut self.lines[row], a, b);
            self.cursor.1 = a.min(char_len(&self.lines[row]).saturating_sub(1));
            if op == Op::Change {
                self.cursor.1 = a;
                self.mode = Mode::Insert;
            }
        }
    }

    /// `d`/`c`/`y` — doubled (`dd`/`cc`/`yy`) acts on the whole line.
    fn operator(&mut self, c: char) {
        let op = match c {
            'd' => Op::Delete,
            'c' => Op::Change,
            _ => Op::Yank,
        };
        if self.pending.op != Some(op) {
            self.pending.op = Some(op);
            return;
        }
        self.pending = Pending::default();
        let row = self.cursor.0;
        self.yank = format!("{}\n", self.lines[row]);
        match op {
            Op::Yank => {}
            Op::Change => {
                self.snapshot();
                self.lines[row].clear();
                self.cursor.1 = 0;
                self.mode = Mode::Insert;
            }
            Op::Delete => {
                self.snapshot();
                self.lines.remove(row);
                if self.lines.is_empty() {
                    self.lines.push(String::new());
                }
                self.cursor = (row.min(self.lines.len() - 1), 0);
            }
        }
    }

    fn delete_char(&mut self) {
        let (row, col) = self.cursor;
        if col < char_len(&self.lines[row]) {
            self.snapshot();
            char_remove(&mut self.lines[row], col);
            self.cursor.1 = col.min(char_len(&self.lines[row]).saturating_sub(1));
        }
        self.pending = Pending::default();
    }

    /// Paste after the cursor; a line-yank (trailing `\n`) opens a line below.
    fn paste(&mut self) {
        if self.yank.is_empty() {
            return;
        }
        self.snapshot();
        let (row, col) = self.cursor;
        if let Some(line) = self.yank.strip_suffix('\n') {
            self.lines.insert(row + 1, line.to_string());
            self.cursor = (row + 1, 0);
        } else {
            let at = (col + 1).min(char_len(&self.lines[row]));
            let yank = self.yank.clone();
            for (i, c) in yank.chars().enumerate() {
                char_insert(&mut self.lines[row], at + i, c);
            }
            self.cursor.1 = at + char_len(&yank) - 1;
        }
        self.pending = Pending::default();
    }

    fn vertical(&mut self, c: char) -> EditorEvent {
        if self.is_empty() {
            return if c == 'j' {
                EditorEvent::ScrollDown
            } else {
                EditorEvent::ScrollUp
            };
        }
        let n = self.pending.count.take().unwrap_or(1) as usize;
        let (row, col) = self.cursor;
        let row = if c == 'j' {
            (row + n).min(self.lines.len() - 1)
        } else {
            row.saturating_sub(n)
        };
        self.cursor = (row, col.min(char_len(&self.lines[row]).saturating_sub(1)));
        EditorEvent::None
    }

    fn snapshot(&mut self) {
        self.undo = Some((self.lines.clone(), self.cursor));
    }

    /// Single-level undo: `u` swaps the buffer with the last snapshot (so a
    /// second `u` redoes).
    fn undo_swap(&mut self) {
        if let Some((lines, cursor)) = self.undo.take() {
            self.undo = Some((std::mem::replace(&mut self.lines, lines), self.cursor));
            self.cursor = cursor;
        }
        self.pending = Pending::default();
    }
}

/// Pure single-line motions in char indices — the operator range math above
/// leans on these being unit-testable.
fn motion_col(line: &str, col: usize, m: char, n: u32) -> usize {
    let chars: Vec<char> = line.chars().collect();
    let len = chars.len();
    match m {
        'h' => col.saturating_sub(n as usize),
        'l' => (col + n as usize).min(len),
        '0' => 0,
        '$' => len,
        'w' => {
            let mut i = col;
            for _ in 0..n {
                while i < len && !chars[i].is_whitespace() {
                    i += 1;
                }
                while i < len && chars[i].is_whitespace() {
                    i += 1;
                }
            }
            i
        }
        'b' => {
            let mut i = col;
            for _ in 0..n {
                while i > 0 && chars[i - 1].is_whitespace() {
                    i -= 1;
                }
                while i > 0 && !chars[i - 1].is_whitespace() {
                    i -= 1;
                }
            }
            i
        }
        'e' => {
            let mut i = col;
            for _ in 0..n {
                i += 1;
                while i < len && chars[i].is_whitespace() {
                    i += 1;
                }
                while i + 1 < len && !chars[i + 1].is_whitespace() {
                    i += 1;
                }
            }
            i.min(len.saturating_sub(1))
        }
        _ => col,
    }
}

fn char_len(s: &str) -> usize {
    s.chars().count()
}

fn byte_idx(s: &str, ci: usize) -> usize {
    s.char_indices().nth(ci).map_or(s.len(), |(i, _)| i)
}

fn char_insert(s: &mut String, ci: usize, c: char) {
    let b = byte_idx(s, ci);
    s.insert(b, c);
}

fn char_remove(s: &mut String, ci: usize) {
    let b = byte_idx(s, ci);
    s.remove(b);
}

fn char_remove_range(s: &mut String, a: usize, b: usize) -> String {
    let (ba, bb) = (byte_idx(s, a), byte_idx(s, b));
    s.drain(ba..bb).collect()
}

fn char_split_off(s: &mut String, ci: usize) -> String {
    let b = byte_idx(s, ci);
    s.split_off(b)
}

fn char_slice(s: &str, a: usize, b: usize) -> String {
    s[byte_idx(s, a)..byte_idx(s, b)].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive an editor with a `"iabc<esc>0dw"`-style key script.
    fn keys(ed: &mut Editor, spec: &str) -> Vec<EditorEvent> {
        let mut events = Vec::new();
        let mut it = spec.chars().peekable();
        while let Some(c) = it.next() {
            let key = if c == '<' {
                let name: String = it.by_ref().take_while(|&c| c != '>').collect();
                match name.as_str() {
                    "esc" => KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                    "cr" => KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                    "bs" => KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
                    "up" => KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
                    "down" => KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
                    "c-e" => KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL),
                    "c-r" => KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
                    other => panic!("unknown key token <{other}>"),
                }
            } else {
                KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
            };
            events.push(ed.handle(key));
        }
        events
    }

    /// Plan-style scripts assume a Normal start; the editor opens in Insert
    /// (ready to type a prompt), so lead with an Esc.
    fn ed(spec: &str) -> Editor {
        let mut e = Editor::new(true);
        keys(&mut e, "<esc>");
        keys(&mut e, spec);
        e
    }

    #[test]
    fn esc_enters_normal_and_backs_cursor_up() {
        let e = ed("iabc<esc>");
        assert_eq!(e.mode(), Mode::Normal);
        assert_eq!(e.cursor(), (0, 2));
    }

    #[test]
    fn insert_entry_keys_place_the_cursor() {
        for (script, cursor, mode_text) in [
            ("iabc<esc>0a", (0, 1), "abc"),
            ("iabc<esc>I", (0, 0), "abc"),
            ("iabc<esc>0A", (0, 3), "abc"),
            ("iabc<esc>o", (1, 0), "abc\n"),
            ("iabc<esc>O", (0, 0), "\nabc"),
        ] {
            let e = ed(script);
            assert_eq!(e.mode(), Mode::Insert, "{script}");
            assert_eq!(e.cursor(), cursor, "{script}");
            assert_eq!(e.text(), mode_text, "{script}");
        }
    }

    #[test]
    fn dw_deletes_a_word() {
        assert_eq!(ed("ione two<esc>0dw").text(), "two");
    }

    #[test]
    fn counted_w_lands_on_fourth_word() {
        let e = ed("ione two three four<esc>03w");
        assert_eq!(e.cursor(), (0, 14));
    }

    #[test]
    fn b_and_e_word_motions() {
        assert_eq!(ed("ione two<esc>b").cursor(), (0, 4));
        assert_eq!(ed("ione two<esc>0e").cursor(), (0, 2));
    }

    #[test]
    fn dd_removes_the_middle_line() {
        let mut e = Editor::new(true);
        e.set_text("a\nb\nc");
        keys(&mut e, "<esc>kdd");
        assert_eq!(e.text(), "a\nc");
    }

    #[test]
    fn cc_clears_the_line_into_insert() {
        let e = ed("ihello<esc>cc");
        assert_eq!(e.text(), "");
        assert_eq!(e.mode(), Mode::Insert);
    }

    #[test]
    fn u_restores_one_step() {
        assert_eq!(ed("ione two<esc>0dwu").text(), "one two");
    }

    #[test]
    fn yy_then_p_duplicates_the_line() {
        assert_eq!(ed("iab<esc>yyp").text(), "ab\nab");
    }

    #[test]
    fn count_with_l_then_d_dollar() {
        assert_eq!(ed("iabcdef<esc>03ld$").text(), "abc");
    }

    #[test]
    fn x_deletes_under_cursor() {
        assert_eq!(ed("iabc<esc>0x").text(), "bc");
    }

    #[test]
    fn empty_buffer_j_k_scroll_the_transcript() {
        let mut e = Editor::new(true);
        keys(&mut e, "<esc>");
        assert_eq!(keys(&mut e, "j"), vec![EditorEvent::ScrollDown]);
        assert_eq!(keys(&mut e, "k"), vec![EditorEvent::ScrollUp]);
    }

    #[test]
    fn j_k_move_lines_when_buffer_has_text() {
        let mut e = Editor::new(true);
        e.set_text("aaa\nb");
        keys(&mut e, "<esc>k");
        assert_eq!(e.cursor().0, 0);
        keys(&mut e, "j");
        assert_eq!(e.cursor().0, 1);
    }

    #[test]
    fn ctrl_e_and_colon_e_open_external_with_full_text() {
        let mut e = Editor::new(true);
        let evs = keys(&mut e, "<esc>ihi<c-e>");
        assert_eq!(evs.last(), Some(&EditorEvent::OpenExternal("hi".into())));
        let evs = keys(&mut e, "<esc>:e<cr>");
        assert_eq!(evs.last(), Some(&EditorEvent::OpenExternal("hi".into())));
    }

    #[test]
    fn vim_false_ignores_esc_and_inserts_everything() {
        let mut e = Editor::new(false);
        keys(&mut e, "abc<esc>x");
        assert_eq!(e.mode(), Mode::Insert);
        assert_eq!(e.text(), "abcx");
    }

    #[test]
    fn enter_submits_and_resets() {
        let mut e = Editor::new(true);
        let evs = keys(&mut e, "<esc>ihello<cr>");
        assert_eq!(evs.last(), Some(&EditorEvent::Submit("hello".into())));
        assert!(e.is_empty());
    }

    // ---- prompt history: recall + reverse-i-search (Insert mode) ----

    /// A fresh Insert-mode editor with a seeded history ring.
    fn ed_hist(entries: &[&str]) -> Editor {
        let mut e = Editor::new(true);
        e.load_history(entries.iter().map(|s| s.to_string()).collect());
        e
    }

    #[test]
    fn up_walks_history_newest_first_and_does_not_wrap() {
        let mut e = ed_hist(&["first", "second", "third"]);
        keys(&mut e, "<up>");
        assert_eq!(e.text(), "third");
        keys(&mut e, "<up>");
        assert_eq!(e.text(), "second");
        keys(&mut e, "<up>");
        assert_eq!(e.text(), "first");
        keys(&mut e, "<up>"); // oldest reached — stays put, no wrap
        assert_eq!(e.text(), "first");
    }

    #[test]
    fn prefix_captured_at_nav_start_filters_the_walk_and_down_restores_the_draft() {
        let mut e = ed_hist(&["apple", "banana", "avocado"]);
        keys(&mut e, "a"); // draft "a" becomes the prefix filter
        keys(&mut e, "<up>"); // newest entry starting with "a"
        assert_eq!(e.text(), "avocado");
        keys(&mut e, "<up>"); // older "a" match — "banana" is skipped
        assert_eq!(e.text(), "apple");
        keys(&mut e, "<down>");
        assert_eq!(e.text(), "avocado");
        keys(&mut e, "<down>"); // past the newest match → the saved draft returns
        assert_eq!(e.text(), "a");
    }

    #[test]
    fn empty_prefix_walks_every_entry() {
        let mut e = ed_hist(&["one", "two"]);
        keys(&mut e, "<up>");
        assert_eq!(e.text(), "two");
        keys(&mut e, "<up>");
        assert_eq!(e.text(), "one");
    }

    #[test]
    fn up_down_move_the_cursor_off_the_boundary_lines_without_recall() {
        let mut e = ed_hist(&["recalled"]);
        e.set_text("l1\nl2\nl3"); // cursor on the last line
        keys(&mut e, "<up>"); // not first line → cursor moves up, buffer intact
        assert_eq!(e.cursor().0, 1);
        assert_eq!(e.text(), "l1\nl2\nl3");
        keys(&mut e, "<up>");
        assert_eq!(e.cursor().0, 0);
        assert_eq!(e.text(), "l1\nl2\nl3");
        keys(&mut e, "<down>"); // not last line → cursor moves down, no recall
        assert_eq!(e.cursor().0, 1);
        assert_eq!(e.text(), "l1\nl2\nl3");
    }

    #[test]
    fn up_on_the_first_line_recalls_even_with_a_multiline_buffer() {
        let mut e = ed_hist(&["l1\nl2 recalled"]);
        e.set_text("l1\nl2"); // 2-line buffer, cursor on the last line
        keys(&mut e, "<up>"); // cursor to the first line
        assert_eq!(e.cursor().0, 0);
        keys(&mut e, "<up>"); // on the first line now → recall (prefix "l1\nl2")
        assert_eq!(e.text(), "l1\nl2 recalled");
    }

    #[test]
    fn submit_rings_the_prompt_with_consecutive_dedup() {
        let mut e = Editor::new(true);
        keys(&mut e, "hello<cr>");
        keys(&mut e, "hello<cr>"); // identical to the previous — not re-ringed
        keys(&mut e, "world<cr>");
        keys(&mut e, "<up>");
        assert_eq!(e.text(), "world");
        keys(&mut e, "<up>");
        assert_eq!(e.text(), "hello");
        keys(&mut e, "<up>"); // only two distinct entries exist
        assert_eq!(e.text(), "hello");
    }

    #[test]
    fn typing_during_recall_exits_recall_and_keeps_the_recalled_text() {
        let mut e = ed_hist(&["alpha", "beta"]);
        keys(&mut e, "<up>"); // "beta"
        keys(&mut e, "!"); // an edit ends recall, keeping "beta!" as the draft
        assert_eq!(e.text(), "beta!");
        keys(&mut e, "<down>"); // no active recall → nothing to step to
        assert_eq!(e.text(), "beta!");
        keys(&mut e, "<up>"); // fresh nav, prefix "beta!" matches nothing
        assert_eq!(e.text(), "beta!");
    }

    #[test]
    fn ctrl_r_reverse_search_matches_steps_older_and_accepts_into_the_buffer() {
        let mut e = ed_hist(&["deploy staging", "run tests", "deploy prod"]);
        keys(&mut e, "<c-r>");
        keys(&mut e, "deploy"); // most-recent entry containing "deploy"
        assert_eq!(
            e.search_prompt(),
            Some(("deploy".into(), "deploy prod".into()))
        );
        keys(&mut e, "<c-r>"); // step to the next older match
        assert_eq!(
            e.search_prompt(),
            Some(("deploy".into(), "deploy staging".into()))
        );
        keys(&mut e, "<cr>"); // accept into the buffer, exit search
        assert_eq!(e.search_prompt(), None);
        assert_eq!(e.text(), "deploy staging");
    }

    #[test]
    fn ctrl_r_esc_cancels_and_restores_the_original_buffer() {
        let mut e = ed_hist(&["find me"]);
        keys(&mut e, "draft");
        keys(&mut e, "<c-r>");
        keys(&mut e, "find");
        assert_eq!(e.search_prompt(), Some(("find".into(), "find me".into())));
        keys(&mut e, "<esc>"); // cancel search
        assert_eq!(e.search_prompt(), None);
        assert_eq!(e.text(), "draft");
    }

    #[test]
    fn ctrl_r_backspace_shrinks_the_query_and_rematches() {
        let mut e = ed_hist(&["cargo build", "cargo test"]);
        keys(&mut e, "<c-r>");
        keys(&mut e, "build");
        assert_eq!(
            e.search_prompt(),
            Some(("build".into(), "cargo build".into()))
        );
        keys(&mut e, "<bs><bs><bs><bs><bs>"); // erase "build" → query "cargo"
        assert_eq!(
            e.search_prompt(),
            Some(("".into(), "cargo test".into())),
            "empty query matches the most recent entry"
        );
    }
}
