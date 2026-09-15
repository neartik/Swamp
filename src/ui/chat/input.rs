use camino::{Utf8Path, Utf8PathBuf};
use unicode_width::UnicodeWidthStr;

/// A multi-line prompt buffer with readline motions. Characters, not graphemes: swamp does
/// not carry a segmentation crate, and a combining mark only ever costs a keypress.
#[derive(Debug, Default, Clone)]
pub struct Editor {
    buf: String,
    cursor: usize,
}

impl Editor {
    pub fn text(&self) -> &str {
        &self.buf
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn clear(&mut self) {
        self.buf.clear();
        self.cursor = 0;
    }

    pub fn set(&mut self, text: &str) {
        self.buf = text.to_owned();
        self.cursor = self.buf.len();
    }

    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.buf)
    }

    pub fn insert(&mut self, c: char) {
        self.buf.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub fn insert_str(&mut self, s: &str) {
        self.buf.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    pub fn backspace(&mut self) {
        let Some(prev) = self.prev_boundary(self.cursor) else {
            return;
        };
        self.buf.replace_range(prev..self.cursor, "");
        self.cursor = prev;
    }

    pub fn delete(&mut self) {
        if let Some(next) = self.next_boundary(self.cursor) {
            self.buf.replace_range(self.cursor..next, "");
        }
    }

    pub fn left(&mut self) {
        if let Some(prev) = self.prev_boundary(self.cursor) {
            self.cursor = prev;
        }
    }

    pub fn right(&mut self) {
        if let Some(next) = self.next_boundary(self.cursor) {
            self.cursor = next;
        }
    }

    pub fn home(&mut self) {
        self.cursor = self.line_start(self.cursor);
    }

    pub fn end(&mut self) {
        self.cursor = self.line_end(self.cursor);
    }

    pub fn word_left(&mut self) {
        while self.cursor > 0 && self.char_before().is_some_and(char::is_whitespace) {
            self.left();
        }
        while self.cursor > 0 && self.char_before().is_some_and(|c| !c.is_whitespace()) {
            self.left();
        }
    }

    pub fn word_right(&mut self) {
        while self.char_at().is_some_and(char::is_whitespace) {
            self.right();
        }
        while self.char_at().is_some_and(|c| !c.is_whitespace()) {
            self.right();
        }
    }

    pub fn kill_to_end(&mut self) {
        let end = self.line_end(self.cursor);
        self.buf.replace_range(self.cursor..end, "");
    }

    pub fn kill_to_start(&mut self) {
        let start = self.line_start(self.cursor);
        self.buf.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    pub fn kill_word(&mut self) {
        let at = self.cursor;
        self.word_left();
        self.buf.replace_range(self.cursor..at, "");
    }

    /// Moves the cursor one display line; false means there was no line to move to, which is
    /// what turns an arrow key into history navigation.
    pub fn up(&mut self) -> bool {
        let start = self.line_start(self.cursor);
        if start == 0 {
            return false;
        }
        let col = self.cursor - start;
        let prev_start = self.line_start(start - 1);
        let prev_end = start - 1;
        self.cursor = (prev_start + col).min(prev_end);
        self.snap();
        true
    }

    pub fn down(&mut self) -> bool {
        let end = self.line_end(self.cursor);
        if end >= self.buf.len() {
            return false;
        }
        let col = self.cursor - self.line_start(self.cursor);
        let next_start = end + 1;
        let next_end = self.line_end(next_start);
        self.cursor = (next_start + col).min(next_end);
        self.snap();
        true
    }

    pub fn lines(&self) -> Vec<&str> {
        self.buf.split('\n').collect()
    }

    /// Which visual row the cursor is on, and how many columns into it.
    pub fn cursor_rc(&self) -> (usize, usize) {
        let start = self.line_start(self.cursor);
        let row = self.buf[..start].matches('\n').count();
        (row, self.buf[start..self.cursor].width())
    }

    fn snap(&mut self) {
        while !self.buf.is_char_boundary(self.cursor) {
            self.cursor -= 1;
        }
    }

    fn char_before(&self) -> Option<char> {
        self.buf[..self.cursor].chars().next_back()
    }

    fn char_at(&self) -> Option<char> {
        self.buf[self.cursor..].chars().next()
    }

    fn prev_boundary(&self, at: usize) -> Option<usize> {
        self.buf[..at]
            .chars()
            .next_back()
            .map(|c| at - c.len_utf8())
    }

    fn next_boundary(&self, at: usize) -> Option<usize> {
        self.buf[at..].chars().next().map(|c| at + c.len_utf8())
    }

    fn line_start(&self, at: usize) -> usize {
        self.buf[..at].rfind('\n').map_or(0, |i| i + 1)
    }

    fn line_end(&self, at: usize) -> usize {
        self.buf[at..].find('\n').map_or(self.buf.len(), |i| at + i)
    }
}

/// `.swamp/chat_history`, one entry per line, newlines escaped. Replaces rustyline's file.
#[derive(Debug, Default)]
pub struct History {
    items: Vec<String>,
    path: Option<Utf8PathBuf>,
    cap: usize,
    pos: usize,
    stash: Option<String>,
}

impl History {
    pub fn load(path: Option<&Utf8Path>, cap: usize) -> History {
        let items: Vec<String> = path
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|text| text.lines().map(unescape).collect())
            .unwrap_or_default();
        let pos = items.len();
        History {
            items,
            path: path.map(Utf8Path::to_path_buf),
            cap: cap.max(1),
            pos,
            stash: None,
        }
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn push(&mut self, entry: &str) {
        if entry.trim().is_empty() {
            return;
        }
        if self.items.last().map(String::as_str) != Some(entry) {
            self.items.push(entry.to_owned());
        }
        while self.items.len() > self.cap {
            self.items.remove(0);
        }
        self.pos = self.items.len();
        self.stash = None;
        self.save();
    }

    /// The in-progress buffer is stashed at index `len`, so walking back and forth is lossless.
    pub fn prev(&mut self, current: &str) -> Option<String> {
        if self.pos == 0 {
            return None;
        }
        if self.pos == self.items.len() {
            self.stash = Some(current.to_owned());
        }
        self.pos -= 1;
        self.items.get(self.pos).cloned()
    }

    pub fn forward(&mut self) -> Option<String> {
        if self.pos >= self.items.len() {
            return None;
        }
        self.pos += 1;
        match self.items.get(self.pos) {
            Some(entry) => Some(entry.clone()),
            None => Some(self.stash.take().unwrap_or_default()),
        }
    }

    pub fn reset(&mut self) {
        self.pos = self.items.len();
    }

    fn save(&self) {
        let Some(path) = &self.path else {
            return;
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let body: String = self
            .items
            .iter()
            .map(|e| format!("{}\n", escape(e)))
            .collect();
        let _ = std::fs::write(path, body);
    }
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\n', "\\n")
}

fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readline_motions_move_by_character_and_by_word() {
        let mut e = Editor::default();
        e.insert_str("hello world");
        e.word_left();
        assert_eq!(e.cursor(), 6);
        e.kill_to_end();
        assert_eq!(e.text(), "hello ");
        e.kill_word();
        assert_eq!(e.text(), "");
        e.insert_str("日本語");
        e.left();
        assert_eq!(e.cursor(), 6, "one character back, not one byte");
        e.backspace();
        assert_eq!(e.text(), "日語");
    }

    #[test]
    fn a_second_line_moves_the_cursor_instead_of_the_history() {
        let mut e = Editor::default();
        e.insert_str("one");
        e.insert('\n');
        e.insert_str("two");
        assert_eq!(e.cursor_rc(), (1, 3));
        assert!(e.up());
        assert_eq!(e.cursor_rc(), (0, 3));
        assert!(!e.up(), "nothing above the first line");
        assert!(e.down());
    }

    #[test]
    fn history_round_trips_through_the_stash_and_escapes_newlines() {
        let mut h = History::load(None, 10);
        h.push("first");
        h.push("second\nline");
        assert_eq!(h.prev("draft").as_deref(), Some("second\nline"));
        assert_eq!(h.prev("draft").as_deref(), Some("first"));
        assert_eq!(h.forward().as_deref(), Some("second\nline"));
        assert_eq!(
            h.forward().as_deref(),
            Some("draft"),
            "the stash comes back"
        );
        assert_eq!(unescape(&escape("a\nb\\c")), "a\nb\\c");
    }

    #[test]
    fn history_is_capped_and_never_repeats_the_last_entry() {
        let mut h = History::load(None, 2);
        h.push("a");
        h.push("a");
        h.push("b");
        h.push("c");
        assert_eq!(h.len(), 2);
        assert_eq!(h.prev("").as_deref(), Some("c"));
    }
}
