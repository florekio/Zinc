/// Byte range in source code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub fn new(start: u32, end: u32) -> Self {
        Self { start, end }
    }

    pub fn empty() -> Self {
        Self { start: 0, end: 0 }
    }

    /// Merge two spans to create a span covering both.
    pub fn merge(self, other: Span) -> Span {
        Span {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }

    pub fn len(&self) -> u32 {
        self.end - self.start
    }

    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }
}

/// Line and column position (1-based line, 0-based column).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    pub line: u32,
    pub column: u32,
}

/// Source location with start and end positions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceLocation {
    pub start: Position,
    pub end: Position,
}
