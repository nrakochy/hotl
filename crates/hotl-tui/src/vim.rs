//! Input editor for the TUI. This is the insert-only skeleton (Task 2); the
//! modal vim layer (Task 4) replaces the internals behind the same API.

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

#[derive(Debug)]
pub struct Editor {
    buffer: String,
    vim: bool,
}

impl Editor {
    /// `vim=false` → always Insert, Esc ignored.
    pub fn new(vim: bool) -> Self {
        Editor { buffer: String::new(), vim }
    }

    pub fn handle(&mut self, key: KeyEvent) -> EditorEvent {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            if key.code == KeyCode::Char('e') {
                return EditorEvent::OpenExternal(self.buffer.clone());
            }
            return EditorEvent::None;
        }
        match key.code {
            KeyCode::Enter if key.modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) => {
                self.buffer.push('\n');
                EditorEvent::None
            }
            KeyCode::Enter => EditorEvent::Submit(std::mem::take(&mut self.buffer)),
            KeyCode::Char(c) => {
                self.buffer.push(c);
                EditorEvent::None
            }
            KeyCode::Backspace => {
                self.buffer.pop();
                EditorEvent::None
            }
            _ => EditorEvent::None,
        }
    }

    pub fn set_text(&mut self, s: &str) {
        self.buffer = s.to_string();
    }

    pub fn text(&self) -> String {
        self.buffer.clone()
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    pub fn mode(&self) -> Mode {
        let _ = self.vim; // modal layer lands in Task 4
        Mode::Insert
    }
}
