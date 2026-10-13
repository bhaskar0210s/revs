//! Open document text with a line index, LSP (UTF-16) positions and incremental edits.

pub struct Text {
    text: String,
    /// Byte offset where each line starts. Always non-empty; `starts[0] == 0`.
    starts: Vec<usize>,
}

/// How one edit changed the line structure: lines `start ..= start + removed` were
/// replaced by lines `start ..= start + inserted`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineEdit {
    pub start: usize,
    pub removed: usize,
    pub inserted: usize,
}

impl Text {
    pub fn new(text: String) -> Self {
        let mut starts = Vec::with_capacity(text.len() / 32 + 1);
        starts.push(0);
        starts.extend(newline_ends(text.as_bytes(), 0));
        Self { text, starts }
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    pub fn line_count(&self) -> usize {
        self.starts.len()
    }

    /// Line content without its line terminator.
    pub fn line(&self, line: usize) -> &str {
        let Some(&start) = self.starts.get(line) else {
            return "";
        };
        let end = self
            .starts
            .get(line + 1)
            .map_or(self.text.len(), |next| next - 1);
        let content = &self.text[start..end];
        content.strip_suffix('\r').unwrap_or(content)
    }

    pub fn utf16_len(&self, line: usize) -> u32 {
        self.line(line).encode_utf16().count() as u32
    }

    /// Byte offset of an LSP position, clamped to the document.
    pub fn offset(&self, line: u32, character: u32) -> usize {
        let line = line as usize;
        if line >= self.starts.len() {
            return self.text.len();
        }
        let start = self.starts[line];
        let content = self.line(line);
        let mut units = 0u32;
        for (index, ch) in content.char_indices() {
            if units >= character {
                return start + index;
            }
            units += ch.len_utf16() as u32;
        }
        start + content.len()
    }

    fn line_of_offset(&self, offset: usize) -> usize {
        match self.starts.binary_search(&offset) {
            Ok(line) => line,
            Err(next) => next - 1,
        }
    }

    pub fn replace_all(&mut self, text: String) -> LineEdit {
        let removed = self.starts.len() - 1;
        *self = Self::new(text);
        LineEdit {
            start: 0,
            removed,
            inserted: self.starts.len() - 1,
        }
    }

    /// Applies an LSP incremental change.
    pub fn edit(&mut self, start: (u32, u32), end: (u32, u32), new_text: &str) -> LineEdit {
        let from = self.offset(start.0, start.1);
        let to = self.offset(end.0, end.1).max(from);
        let start_line = self.line_of_offset(from);
        let removed = self.text.as_bytes()[from..to]
            .iter()
            .filter(|&&byte| byte == b'\n')
            .count();
        let inserted_starts: Vec<usize> = newline_ends(new_text.as_bytes(), from).collect();
        let inserted = inserted_starts.len();
        self.text.replace_range(from..to, new_text);

        let delta = new_text.len() as isize - (to - from) as isize;
        let replaced = start_line + 1..start_line + 1 + removed;
        self.starts.splice(replaced, inserted_starts);
        for start in &mut self.starts[start_line + 1 + inserted..] {
            *start = (*start as isize + delta) as usize;
        }
        LineEdit {
            start: start_line,
            removed,
            inserted,
        }
    }
}

/// Offsets just past each `\n` in `bytes`, shifted by `base`.
fn newline_ends(bytes: &[u8], base: usize) -> impl Iterator<Item = usize> + '_ {
    bytes
        .iter()
        .enumerate()
        .filter(|(_, byte)| **byte == b'\n')
        .map(move |(index, _)| base + index + 1)
}

/// Moves a tracked line through an edit. Lines inside the replaced block stay on the
/// block's first line, matching how an anchor at the line start behaves in the editor.
pub fn shift_line(line: u32, edit: LineEdit) -> u32 {
    let line = line as usize;
    if line <= edit.start {
        return line as u32;
    }
    if line > edit.start + edit.removed {
        return (line + edit.inserted - edit.removed) as u32;
    }
    (edit.start + (line - edit.start).min(edit.inserted)) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check_index(text: &Text) {
        let fresh = Text::new(text.as_str().to_string());
        assert_eq!(text.starts, fresh.starts, "line index out of sync");
    }

    #[test]
    fn lines_and_lengths() {
        let text = Text::new("a\r\nbé😀\n\nlast".into());
        assert_eq!(text.line_count(), 4);
        assert_eq!(text.line(0), "a");
        assert_eq!(text.line(1), "bé😀");
        assert_eq!(text.utf16_len(1), 4);
        assert_eq!(text.line(2), "");
        assert_eq!(text.line(3), "last");
        assert_eq!(text.line(9), "");
    }

    #[test]
    fn offsets_clamp() {
        let text = Text::new("ab😀c\nxy".into());
        assert_eq!(text.offset(0, 2), 2);
        assert_eq!(text.offset(0, 4), 6);
        assert_eq!(text.offset(0, 99), 7);
        assert_eq!(text.offset(1, 1), 9);
        assert_eq!(text.offset(5, 0), 10);
    }

    #[test]
    fn edits_keep_index() {
        let mut text = Text::new("one\ntwo\nthree\nfour\n".into());
        let edit = text.edit((1, 1), (2, 2), "X\nY\nZ");
        assert_eq!(text.as_str(), "one\ntX\nY\nZree\nfour\n");
        assert_eq!(
            edit,
            LineEdit {
                start: 1,
                removed: 1,
                inserted: 2
            }
        );
        check_index(&text);
        let edit = text.edit((0, 0), (3, 0), "");
        assert_eq!(text.as_str(), "Zree\nfour\n");
        assert_eq!(edit.removed, 3);
        check_index(&text);
        text.edit((2, 0), (2, 0), "tail");
        assert_eq!(text.as_str(), "Zree\nfour\ntail");
        check_index(&text);
        let edit = text.replace_all("new\n".into());
        assert_eq!(edit.removed, 2);
        check_index(&text);
    }

    #[test]
    fn shifting_lines() {
        let edit = LineEdit {
            start: 5,
            removed: 2,
            inserted: 0,
        };
        assert_eq!(shift_line(3, edit), 3);
        assert_eq!(shift_line(6, edit), 5);
        assert_eq!(shift_line(9, edit), 7);
        let edit = LineEdit {
            start: 5,
            removed: 0,
            inserted: 3,
        };
        assert_eq!(shift_line(5, edit), 5);
        assert_eq!(shift_line(6, edit), 9);
    }
}
