//! Full-featured UTF-8 safe composer buffer with cursor, selection, word movement,
//! clipboard operations, and prompt history.
//!
//! Kept UI-agnostic (returns plain strings & byte ranges) so editing invariants
//! are thoroughly unit-tested headlessly.

use std::ops::Range;

/// Editable text buffer with selection (`anchor..cursor`) and prompt history.
#[derive(Debug, Clone, Default)]
pub struct TextBuffer {
    text: String,
    cursor: usize,
    anchor: usize,
    history: Vec<String>,
    history_idx: Option<usize>,
    draft_before_history: String,
}

impl TextBuffer {
    pub fn as_str(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    #[cfg(test)]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn selection(&self) -> Range<usize> {
        if self.anchor <= self.cursor {
            self.anchor..self.cursor
        } else {
            self.cursor..self.anchor
        }
    }

    pub fn has_selection(&self) -> bool {
        self.anchor != self.cursor
    }

    pub fn selected_text(&self) -> Option<&str> {
        let range = self.selection();
        if range.is_empty() {
            None
        } else {
            self.text.get(range)
        }
    }

    pub fn set_text(&mut self, value: impl Into<String>) {
        self.text = value.into();
        self.cursor = self.text.len();
        self.anchor = self.cursor;
        self.history_idx = None;
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.anchor = 0;
        self.history_idx = None;
    }

    /// Record a submitted prompt into history (deduping consecutive identical entries)
    /// and clear the buffer.
    pub fn commit_submission(&mut self) -> Option<String> {
        let trimmed = self.text.trim().to_string();
        if trimmed.is_empty() {
            return None;
        }
        if self.history.last().map(String::as_str) != Some(trimmed.as_str()) {
            self.history.push(trimmed.clone());
            if self.history.len() > 200 {
                self.history.remove(0);
            }
        }
        self.clear();
        Some(trimmed)
    }

    pub fn insert(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        self.delete_selection_internal();
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
        self.anchor = self.cursor;
        self.history_idx = None;
    }

    pub fn select_all(&mut self) {
        self.anchor = 0;
        self.cursor = self.text.len();
    }

    pub fn backspace(&mut self) {
        if self.delete_selection_internal() {
            return;
        }
        if self.cursor == 0 {
            return;
        }
        let prev = prev_char_boundary(&self.text, self.cursor);
        self.text.replace_range(prev..self.cursor, "");
        self.cursor = prev;
        self.anchor = prev;
        self.history_idx = None;
    }

    pub fn delete_word_backward(&mut self) {
        if self.delete_selection_internal() {
            return;
        }
        let target = prev_word_boundary(&self.text, self.cursor);
        if target < self.cursor {
            self.text.replace_range(target..self.cursor, "");
            self.cursor = target;
            self.anchor = target;
            self.history_idx = None;
        }
    }

    pub fn delete_forward(&mut self) {
        if self.delete_selection_internal() {
            return;
        }
        if self.cursor >= self.text.len() {
            return;
        }
        let next = next_char_boundary(&self.text, self.cursor);
        self.text.replace_range(self.cursor..next, "");
        self.anchor = self.cursor;
        self.history_idx = None;
    }

    pub fn cut(&mut self) -> Option<String> {
        let selected = self.selected_text()?.to_string();
        self.delete_selection_internal();
        Some(selected)
    }

    pub fn move_left(&mut self, extend: bool) {
        if !extend && self.has_selection() {
            let start = self.selection().start;
            self.cursor = start;
            self.anchor = start;
            return;
        }
        self.cursor = prev_char_boundary(&self.text, self.cursor);
        if !extend {
            self.anchor = self.cursor;
        }
    }

    pub fn move_right(&mut self, extend: bool) {
        if !extend && self.has_selection() {
            let end = self.selection().end;
            self.cursor = end;
            self.anchor = end;
            return;
        }
        self.cursor = next_char_boundary(&self.text, self.cursor);
        if !extend {
            self.anchor = self.cursor;
        }
    }

    pub fn move_word_left(&mut self, extend: bool) {
        self.cursor = prev_word_boundary(&self.text, self.cursor);
        if !extend {
            self.anchor = self.cursor;
        }
    }

    pub fn move_word_right(&mut self, extend: bool) {
        self.cursor = next_word_boundary(&self.text, self.cursor);
        if !extend {
            self.anchor = self.cursor;
        }
    }

    pub fn move_home(&mut self, extend: bool) {
        let line_start = self.text[..self.cursor]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        self.cursor = line_start;
        if !extend {
            self.anchor = self.cursor;
        }
    }

    pub fn move_end(&mut self, extend: bool) {
        let line_end = self.text[self.cursor..]
            .find('\n')
            .map(|i| self.cursor + i)
            .unwrap_or(self.text.len());
        self.cursor = line_end;
        if !extend {
            self.anchor = self.cursor;
        }
    }

    /// Cycle backward through submitted prompts when on a single-line buffer.
    pub fn history_prev(&mut self) -> bool {
        if self.text.contains('\n') || self.history.is_empty() {
            return false;
        }
        let next_idx = match self.history_idx {
            None => {
                self.draft_before_history = self.text.clone();
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.history_idx = Some(next_idx);
        self.text = self.history[next_idx].clone();
        self.cursor = self.text.len();
        self.anchor = self.cursor;
        true
    }

    /// Cycle forward through submitted prompts back toward the in-progress draft.
    pub fn history_next(&mut self) -> bool {
        let Some(cur) = self.history_idx else {
            return false;
        };
        if cur + 1 >= self.history.len() {
            self.history_idx = None;
            self.text = std::mem::take(&mut self.draft_before_history);
        } else {
            let next_idx = cur + 1;
            self.history_idx = Some(next_idx);
            self.text = self.history[next_idx].clone();
        }
        self.cursor = self.text.len();
        self.anchor = self.cursor;
        true
    }

    /// Build a display string with an inline cursor glyph (`▎`) and optional selection range
    /// adjusted for the inserted cursor marker.
    pub fn render_parts(&self) -> (String, Option<Range<usize>>) {
        if self.has_selection() {
            return (self.text.clone(), Some(self.selection()));
        }
        let mut out = String::with_capacity(self.text.len() + 3);
        out.push_str(&self.text[..self.cursor]);
        out.push('▎');
        out.push_str(&self.text[self.cursor..]);
        (out, None)
    }

    fn delete_selection_internal(&mut self) -> bool {
        let range = self.selection();
        if range.is_empty() {
            return false;
        }
        self.text.replace_range(range.clone(), "");
        self.cursor = range.start;
        self.anchor = range.start;
        self.history_idx = None;
        true
    }
}

fn prev_char_boundary(s: &str, idx: usize) -> usize {
    if idx == 0 {
        return 0;
    }
    s[..idx]
        .char_indices()
        .next_back()
        .map(|(i, _)| i)
        .unwrap_or(0)
}

fn next_char_boundary(s: &str, idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    let ch_len = s[idx..].chars().next().map(char::len_utf8).unwrap_or(0);
    (idx + ch_len).min(s.len())
}

fn prev_word_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx;
    while i > 0 {
        let p = prev_char_boundary(s, i);
        let ch = s[p..i].chars().next().unwrap();
        if !ch.is_whitespace() {
            break;
        }
        i = p;
    }
    while i > 0 {
        let p = prev_char_boundary(s, i);
        let ch = s[p..i].chars().next().unwrap();
        if ch.is_whitespace() || (!ch.is_alphanumeric() && ch != '_') {
            if i == idx {
                return p;
            }
            break;
        }
        i = p;
    }
    i
}

fn next_word_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx;
    while i < s.len() {
        let n = next_char_boundary(s, i);
        let ch = s[i..n].chars().next().unwrap();
        if !ch.is_whitespace() {
            break;
        }
        i = n;
    }
    while i < s.len() {
        let n = next_char_boundary(s, i);
        let ch = s[i..n].chars().next().unwrap();
        if ch.is_whitespace() || (!ch.is_alphanumeric() && ch != '_') {
            if i == idx {
                return n;
            }
            break;
        }
        i = n;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supports_utf8_cursor_selection_word_ops_and_history() {
        let mut buf = TextBuffer::default();
        buf.insert("hello 🌍 world");
        assert_eq!(buf.cursor(), buf.as_str().len());

        buf.move_word_left(true);
        assert_eq!(buf.selected_text(), Some("world"));
        buf.insert("rust");
        assert_eq!(buf.as_str(), "hello 🌍 rust");

        buf.select_all();
        assert_eq!(buf.cut().as_deref(), Some("hello 🌍 rust"));
        assert!(buf.is_empty());

        buf.insert("first prompt");
        assert_eq!(buf.commit_submission().as_deref(), Some("first prompt"));
        buf.insert("second prompt");
        assert_eq!(buf.commit_submission().as_deref(), Some("second prompt"));

        buf.insert("wip");
        assert!(buf.history_prev());
        assert_eq!(buf.as_str(), "second prompt");
        assert!(buf.history_prev());
        assert_eq!(buf.as_str(), "first prompt");
        assert!(buf.history_next());
        assert_eq!(buf.as_str(), "second prompt");
        assert!(buf.history_next());
        assert_eq!(buf.as_str(), "wip");
    }
}
