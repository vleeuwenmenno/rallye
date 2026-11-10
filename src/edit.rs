//! Pure session-edit core: byte parsing and an in-memory working copy with undo/redo.
//! The original bytes are immutable and nothing here touches the filesystem.

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    Hex,
    Unsigned,
    Signed,
}

impl Mode {
    pub fn range(self) -> &'static str {
        match self {
            Mode::Hex => "00..FF (optional 0x)",
            Mode::Unsigned => "0..255",
            Mode::Signed => "-128..127",
        }
    }
}

/// Parse a typed value into the raw byte. Out-of-range or malformed text is an error, never clamped or wrapped.
pub fn parse_byte(text: &str, mode: Mode) -> Result<u8, String> {
    let t = text.trim();
    let bad = || format!("'{t}' is not valid, expected {}", mode.range());
    match mode {
        Mode::Hex => {
            let h = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")).unwrap_or(t);
            if h.is_empty() || h.len() > 2 || !h.bytes().all(|c| c.is_ascii_hexdigit()) {
                return Err(bad());
            }
            u8::from_str_radix(h, 16).map_err(|_| bad())
        }
        Mode::Unsigned => t.parse::<u8>().map_err(|_| bad()),
        Mode::Signed => t.parse::<i8>().map(|v| v as u8).map_err(|_| bad()),
    }
}

/// Small numeric field buffer. Caret offsets always lie on UTF-8 character boundaries.
#[derive(Debug, Clone)]
pub struct CellText {
    pub text: String,
    pub caret: usize,
    pub selected_all: bool,
}

impl CellText {
    pub fn new(text: String) -> Self {
        Self { caret: text.len(), text, selected_all: true }
    }
    pub fn select_all(&mut self) {
        self.selected_all = true;
        self.caret = self.text.len();
    }
    pub fn home(&mut self) {
        self.caret = 0;
        self.selected_all = false;
    }
    pub fn end(&mut self) {
        self.caret = self.text.len();
        self.selected_all = false;
    }
    pub fn left(&mut self) {
        self.caret = if self.selected_all { 0 } else {
            self.text[..self.caret].char_indices().next_back().map_or(0, |(i, _)| i)
        };
        self.selected_all = false;
    }
    pub fn right(&mut self) {
        self.caret = if self.selected_all { self.text.len() } else {
            self.caret + self.text[self.caret..].chars().next().map_or(0, char::len_utf8)
        };
        self.selected_all = false;
    }
    pub fn insert(&mut self, inserted: &str) -> Result<(), String> {
        let remaining = if self.selected_all { 0 } else { self.text.len() };
        if remaining.saturating_add(inserted.len()) > 64 {
            return Err("Input too long (maximum 64 bytes); enter a single byte value".into());
        }
        if self.selected_all {
            self.text.clear();
            self.caret = 0;
        }
        self.text.insert_str(self.caret, inserted);
        self.caret += inserted.len();
        self.selected_all = false;
        Ok(())
    }
    pub fn delete(&mut self, backward: bool) {
        if self.selected_all {
            self.text.clear();
            self.caret = 0;
            self.selected_all = false;
        } else if backward {
            let end = self.caret;
            self.left();
            self.text.replace_range(self.caret..end, "");
        } else {
            let start = self.caret;
            self.right();
            self.text.replace_range(start..self.caret, "");
            self.caret = start;
        }
    }
}

/// Maximum remembered edits per working copy; the oldest are dropped first.
pub const MAX_HISTORY: usize = 1000;

/// One dump's session copy. `original` is never mutated; `bytes` is what the UI shows.
#[derive(Debug, Clone)]
pub struct Working {
    original: Vec<u8>,
    bytes: Vec<u8>,
    undo: Vec<(usize, u8, u8)>, // (offset, old, new)
    redo: Vec<(usize, u8, u8)>,
}

impl Working {
    pub fn new(original: Vec<u8>) -> Self {
        Self { bytes: original.clone(), original, undo: vec![], redo: vec![] }
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    #[cfg(test)]
    pub fn original(&self) -> &[u8] {
        &self.original
    }
    /// Number of bytes differing from the original (dirty means > 0, so a reverted edit is clean).
    pub fn changed(&self) -> usize {
        self.bytes.iter().zip(&self.original).filter(|(a, b)| a != b).count()
    }
    pub fn dirty(&self) -> bool {
        self.bytes != self.original
    }
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }
    /// Ok(true) if the byte changed; Ok(false) for a no-op (no history entry).
    pub fn set(&mut self, offset: usize, value: u8) -> Result<bool, String> {
        let old = *self.bytes.get(offset).ok_or_else(|| format!("offset 0x{offset:X} is outside the {} byte file", self.bytes.len()))?;
        if old == value {
            return Ok(false);
        }
        self.bytes[offset] = value;
        self.undo.push((offset, old, value));
        if self.undo.len() > MAX_HISTORY {
            self.undo.remove(0);
        }
        self.redo.clear();
        Ok(true)
    }
    pub fn undo(&mut self) -> bool {
        let Some((o, old, new)) = self.undo.pop() else { return false };
        self.bytes[o] = old;
        self.redo.push((o, old, new));
        true
    }
    pub fn redo(&mut self) -> bool {
        let Some((o, old, new)) = self.redo.pop() else { return false };
        self.bytes[o] = new;
        self.undo.push((o, old, new));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_preserves_invalid_text_and_rejects_excess_length() {
        for text in ["1 2", "1\n2", "1é2"] {
            let mut field = CellText::new("99".into());
            field.insert(text).unwrap();
            assert_eq!(field.text, text);
            assert!(parse_byte(&field.text, Mode::Unsigned).is_err());
        }
        let mut field = CellText::new("99".into());
        field.insert("000000001").unwrap();
        assert_eq!(parse_byte(&field.text, Mode::Unsigned), Ok(1));
        let before = (field.text.clone(), field.caret);
        assert!(field.insert(&"0".repeat(65)).is_err());
        assert_eq!((field.text, field.caret), before);
    }

    #[test]
    fn caret_moves_and_inserts_in_middle() {
        let mut field = CellText::new("123".into());
        field.right(); // collapse the initial selection to its end
        field.left();
        field.insert("0").unwrap();
        assert_eq!((&*field.text, field.caret), ("1203", 3));
        field.delete(true);
        assert_eq!((&*field.text, field.caret), ("123", 2));
        field.delete(false);
        assert_eq!((&*field.text, field.caret), ("12", 2));
        field.home();
        field.left();
        field.insert("-").unwrap();
        assert_eq!((&*field.text, field.caret), ("-12", 1));
        field.end();
        field.right();
        assert_eq!(field.caret, 3);
    }

    #[test]
    fn selection_and_unicode_caret_boundaries() {
        let mut field = CellText::new("128".into());
        field.left();
        assert_eq!(field.caret, 0);
        field.select_all();
        field.insert("1é2").unwrap();
        field.left();
        assert_eq!(field.caret, 3);
        field.left();
        assert_eq!(field.caret, 1);
        field.delete(false);
        assert_eq!((&*field.text, field.caret), ("12", 1));
        field.select_all();
        field.delete(true);
        field.delete(true);
        field.delete(false);
        assert_eq!((&*field.text, field.caret), ("", 0));
    }

    #[test]
    fn parse_boundaries() {
        assert_eq!(parse_byte("255", Mode::Unsigned), Ok(255));
        assert!(parse_byte("256", Mode::Unsigned).is_err());
        assert!(parse_byte("-1", Mode::Unsigned).is_err());
        assert_eq!(parse_byte("-128", Mode::Signed), Ok(0x80));
        assert_eq!(parse_byte("127", Mode::Signed), Ok(0x7F));
        assert!(parse_byte("128", Mode::Signed).is_err());
        assert!(parse_byte("-129", Mode::Signed).is_err());
        assert_eq!(parse_byte("ff", Mode::Hex), Ok(0xFF));
        assert_eq!(parse_byte("0X0a", Mode::Hex), Ok(0x0A));
        assert_eq!(parse_byte("5", Mode::Hex), Ok(5));
        for s in ["100", "0x", "", "g1", "-1", "0x123"] {
            assert!(parse_byte(s, Mode::Hex).is_err(), "{s}");
        }
        for s in ["", "abc", "1.5", " "] {
            assert!(parse_byte(s, Mode::Unsigned).is_err() && parse_byte(s, Mode::Signed).is_err(), "{s}");
        }
        assert_eq!(parse_byte(" 7 ", Mode::Unsigned), Ok(7));
    }

    #[test]
    fn set_bounds_noop_and_history() {
        let mut w = Working::new(vec![1, 2, 3]);
        assert!(w.set(3, 0).is_err());
        assert_eq!(w.set(1, 2), Ok(false)); // no-op: no history, not dirty
        assert!(!w.can_undo() && !w.dirty());
        assert_eq!(w.set(1, 9), Ok(true));
        assert!(w.dirty() && w.changed() == 1);
        assert_eq!(w.set(1, 2), Ok(true)); // back to original: clean although history exists
        assert!(!w.dirty() && w.can_undo());
        assert_eq!(w.original(), [1, 2, 3]);
    }

    #[test]
    fn undo_redo_and_redo_cleared_by_new_edit() {
        let mut w = Working::new(vec![0, 0]);
        w.set(0, 5).unwrap();
        w.set(1, 6).unwrap();
        assert!(w.undo() && w.bytes() == [5, 0]);
        assert!(w.redo() && w.bytes() == [5, 6]);
        w.undo();
        w.set(0, 7).unwrap();
        assert!(!w.can_redo());
        assert!(w.undo() && w.undo() && !w.undo());
        assert_eq!(w.bytes(), w.original());
        assert!(!w.redo() || w.bytes() == [5, 0]);
    }

    #[test]
    fn history_is_bounded() {
        let mut w = Working::new(vec![0]);
        for i in 0..(MAX_HISTORY + 50) {
            w.set(0, (i % 2) as u8 + 1).unwrap_or(false);
        }
        assert_eq!(w.undo.len(), MAX_HISTORY);
    }

    #[test]
    fn two_targets_are_isolated_and_original_untouched() {
        let base = vec![10u8, 20];
        let (mut a, b) = (Working::new(base.clone()), Working::new(base.clone()));
        a.set(0, 99).unwrap();
        assert_eq!(a.bytes(), [99, 20]);
        assert_eq!(b.bytes(), [10, 20]);
        assert!(a.dirty() && !b.dirty());
        assert_eq!(base, [10, 20]); // baseline bytes stay original: target==baseline still shows a delta
        assert_eq!(a.original(), base.as_slice());
    }
}
