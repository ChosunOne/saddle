use unicode_width::UnicodeWidthChar;

#[derive(Debug, Default, Clone)]
pub struct Editor {
    text: String,
    cursor: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Layout {
    pub rows: Vec<String>,
    pub cursor: (usize, usize),
}

impl Editor {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn replace(&mut self, start: usize, end: usize, with: &str) {
        self.text.replace_range(start..end, with);
        self.cursor = start + with.len();
    }

    pub fn set(&mut self, text: String) {
        self.cursor = text.len();
        self.text = text;
    }

    pub fn clear(&mut self) {
        self.set(String::new());
    }

    pub fn insert(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub fn insert_str(&mut self, s: &str) {
        let s = s.replace("\r\n", "\n").replace('\r', "\n");
        let mut clean = String::with_capacity(s.len());
        for c in s.chars() {
            match c {
                '\t' => clean.push_str("    "),
                '\n' => clean.push('\n'),
                c if c.is_control() => {}
                c => clean.push(c),
            }
        }
        self.text.insert_str(self.cursor, &clean);
        self.cursor += clean.len();
    }

    fn prev(&self, at: usize) -> usize {
        self.text[..at]
            .char_indices()
            .next_back()
            .map_or(0, |(i, _)| i)
    }

    fn next(&self, at: usize) -> usize {
        self.text[at..]
            .chars()
            .next()
            .map_or(at, |c| at + c.len_utf8())
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            let start = self.prev(self.cursor);
            self.text.drain(start..self.cursor);
            self.cursor = start;
        }
    }

    pub fn delete(&mut self) {
        let end = self.next(self.cursor);
        self.text.drain(self.cursor..end);
    }

    pub fn left(&mut self) {
        self.cursor = self.prev(self.cursor);
    }

    pub fn right(&mut self) {
        self.cursor = self.next(self.cursor);
    }

    fn line_start(&self) -> usize {
        self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1)
    }

    fn line_end(&self) -> usize {
        self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |i| self.cursor + i)
    }

    pub fn home(&mut self) {
        self.cursor = self.line_start();
    }

    pub fn end(&mut self) {
        self.cursor = self.line_end();
    }

    fn column(&self) -> usize {
        self.text[self.line_start()..self.cursor].chars().count()
    }

    fn offset_in_line(&self, start: usize, col: usize) -> usize {
        let line = &self.text[start..];
        let end = line.find('\n').unwrap_or(line.len());
        line[..end]
            .char_indices()
            .nth(col)
            .map_or(start + end, |(i, _)| start + i)
    }

    pub fn up(&mut self) -> bool {
        let start = self.line_start();
        if start == 0 {
            return false;
        }
        let col = self.column();
        let prev_start = self.text[..start - 1].rfind('\n').map_or(0, |i| i + 1);
        self.cursor = self.offset_in_line(prev_start, col);
        true
    }

    pub fn down(&mut self) -> bool {
        let end = self.line_end();
        if end == self.text.len() {
            return false;
        }
        let col = self.column();
        self.cursor = self.offset_in_line(end + 1, col);
        true
    }

    fn word_start_before(&self, at: usize) -> usize {
        let before = &self.text[..at];
        let trimmed = before.trim_end();
        trimmed
            .char_indices()
            .rev()
            .find(|(_, c)| c.is_whitespace())
            .map_or(0, |(i, c)| i + c.len_utf8())
    }

    pub fn word_left(&mut self) {
        self.cursor = self.word_start_before(self.cursor);
    }

    pub fn word_right(&mut self) {
        let rest = &self.text[self.cursor..];
        let skip_ws = rest.len() - rest.trim_start().len();
        let word = rest[skip_ws..]
            .find(char::is_whitespace)
            .unwrap_or(rest.len() - skip_ws);
        self.cursor += skip_ws + word;
    }

    pub fn delete_word_back(&mut self) {
        let start = self.word_start_before(self.cursor);
        self.text.drain(start..self.cursor);
        self.cursor = start;
    }

    pub fn kill_to_line_start(&mut self) {
        let start = self.line_start();
        if start == self.cursor {
            self.backspace();
        } else {
            self.text.drain(start..self.cursor);
            self.cursor = start;
        }
    }

    pub fn kill_to_line_end(&mut self) {
        let end = self.line_end();
        if end == self.cursor {
            self.delete();
        } else {
            self.text.drain(self.cursor..end);
        }
    }

    pub fn layout(&self, width: usize) -> Layout {
        let width = width.max(1);
        let mut rows = vec![String::new()];
        let mut col = 0;
        let mut cursor = None;
        for (i, c) in self.text.char_indices() {
            if c == '\n' {
                if i == self.cursor {
                    cursor = Some(end_cursor(&mut rows, col, width));
                }
                rows.push(String::new());
                col = 0;
                continue;
            }
            let w = c.width().unwrap_or(0);
            if col + w > width && col > 0 {
                rows.push(String::new());
                col = 0;
            }
            if i == self.cursor {
                cursor = Some((rows.len() - 1, col));
            }
            rows.last_mut().expect("rows is never empty").push(c);
            col += w;
        }
        let cursor = cursor.unwrap_or_else(|| end_cursor(&mut rows, col, width));
        Layout { rows, cursor }
    }
}

fn end_cursor(rows: &mut Vec<String>, col: usize, width: usize) -> (usize, usize) {
    if col >= width {
        rows.push(String::new());
        (rows.len() - 1, 0)
    } else {
        (rows.len() - 1, col)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ed(text: &str) -> Editor {
        let mut e = Editor::default();
        e.set(text.into());
        e
    }

    #[test]
    fn editing_and_line_movement() {
        let mut e = ed("first line\nsé");
        assert!(e.up());
        assert_eq!(&e.text()[e.cursor..], "rst line\nsé");
        assert!(!e.up());
        e.end();
        assert!(e.down());
        assert_eq!(e.cursor, e.text().len());
        assert!(!e.down());
        e.backspace();
        assert_eq!(e.text(), "first line\ns");
        e.insert('ü');
        e.insert_str("\r\nnext\tx\x1b[2J");
        assert_eq!(e.text(), "first line\nsü\nnext    x[2J");
    }

    #[test]
    fn words_and_kills() {
        let mut e = ed("cargo test  --all");
        e.delete_word_back();
        assert_eq!(e.text(), "cargo test  ");
        e.word_left();
        assert_eq!(e.cursor, 6);
        e.word_right();
        assert_eq!(e.cursor, 10);
        e.kill_to_line_end();
        assert_eq!(e.text(), "cargo test");
        e.kill_to_line_start();
        assert_eq!(e.text(), "");
    }

    #[test]
    fn layout_wraps_and_places_the_cursor() {
        let e = ed("abcdef\ngh");
        assert_eq!(
            e.layout(4),
            Layout {
                rows: vec!["abcd".into(), "ef".into(), "gh".into()],
                cursor: (2, 2)
            }
        );
        let e = ed("abcd");
        assert_eq!(
            e.layout(4),
            Layout {
                rows: vec!["abcd".into(), "".into()],
                cursor: (1, 0)
            }
        );
        let mut e = ed("ab\n");
        e.cursor = 2;
        assert_eq!(e.layout(10).cursor, (0, 2));
        assert_eq!(ed("").layout(10).cursor, (0, 0));
        assert_eq!(ed("日本語").layout(4).rows, vec!["日本", "語"]);
    }
}
