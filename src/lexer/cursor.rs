/// A cursor over source code bytes with position tracking.
pub struct Cursor<'a> {
    source: &'a str,
    bytes: &'a [u8],
    pos: usize,
    /// Line number (1-based).
    line: u32,
    /// Column offset of the current line start.
    line_start: usize,
}

impl<'a> Cursor<'a> {
    pub fn new(source: &'a str) -> Self {
        Self {
            source,
            bytes: source.as_bytes(),
            pos: 0,
            line: 1,
            line_start: 0,
        }
    }

    /// Current byte position in the source.
    #[inline]
    pub fn pos(&self) -> usize {
        self.pos
    }

    /// Current line number (1-based).
    #[inline]
    pub fn line(&self) -> u32 {
        self.line
    }

    /// Current column (0-based).
    #[inline]
    pub fn column(&self) -> u32 {
        (self.pos - self.line_start) as u32
    }

    /// Are we at the end of the source?
    #[inline]
    pub fn is_eof(&self) -> bool {
        self.pos >= self.bytes.len()
    }

    /// Peek the current byte without consuming.
    #[inline]
    pub fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    /// Peek the byte at offset from current position.
    #[inline]
    pub fn peek_at(&self, offset: usize) -> Option<u8> {
        self.bytes.get(self.pos + offset).copied()
    }

    /// Peek the current character (handles multi-byte UTF-8).
    pub fn peek_char(&self) -> Option<char> {
        if self.pos >= self.source.len() || !self.source.is_char_boundary(self.pos) {
            return None;
        }
        self.source[self.pos..].chars().next()
    }

    /// Advance by one byte and return it.
    #[inline]
    pub fn advance(&mut self) -> Option<u8> {
        let byte = self.bytes.get(self.pos).copied()?;
        self.pos += 1;
        if byte == b'\n' {
            self.line += 1;
            self.line_start = self.pos;
        }
        Some(byte)
    }

    /// Advance by one character (handles multi-byte UTF-8) and return it.
    pub fn advance_char(&mut self) -> Option<char> {
        if self.pos >= self.source.len() || !self.source.is_char_boundary(self.pos) {
            return None;
        }
        let ch = self.source[self.pos..].chars().next()?;
        let len = ch.len_utf8();
        for _ in 0..len {
            self.advance();
        }
        Some(ch)
    }

    /// Advance if the current byte matches `expected`.
    #[inline]
    pub fn eat(&mut self, expected: u8) -> bool {
        if self.peek() == Some(expected) {
            self.advance();
            true
        } else {
            false
        }
    }

    /// Advance while `predicate` returns true for the current byte.
    pub fn skip_while(&mut self, predicate: impl Fn(u8) -> bool) {
        while let Some(b) = self.peek() {
            if predicate(b) {
                self.advance();
            } else {
                break;
            }
        }
    }

    /// Get a slice of the source from `start` to current position.
    pub fn slice_from(&self, start: usize) -> &'a str {
        &self.source[start..self.pos]
    }

    /// Get a slice of the source between two positions.
    pub fn slice(&self, start: usize, end: usize) -> &'a str {
        &self.source[start..end]
    }

    /// Get the entire source.
    pub fn source(&self) -> &'a str {
        self.source
    }
}

/// Check if a byte is an ASCII identifier start character (a-z, A-Z, _, $).
#[inline]
pub fn is_id_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b == b'$'
}

/// Check if a byte is an ASCII identifier continue character (a-z, A-Z, 0-9, _, $).
#[inline]
pub fn is_id_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

/// Check if a character is a Unicode identifier start (ID_Start or $ or _).
pub fn is_unicode_id_start(c: char) -> bool {
    c == '$' || c == '_' || c.is_alphabetic()
}

/// Check if a character is a Unicode identifier continue (ID_Continue or $ or \u200C or \u200D).
pub fn is_unicode_id_continue(c: char) -> bool {
    c == '$' || c == '\u{200C}' || c == '\u{200D}' || c.is_alphanumeric() || c == '_'
}

/// Check if a byte is a line terminator.
#[inline]
pub fn is_line_terminator(b: u8) -> bool {
    b == b'\n' || b == b'\r'
}
