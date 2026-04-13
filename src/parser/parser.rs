use crate::ast::node::*;
use crate::ast::span::Span;
use crate::lexer::token::{Token, TokenKind};
use crate::util::interner::{Interner, StringId};

use super::error::{ParseError, ParseResult};
use super::statement;

/// Decode \uXXXX and \u{XXXX} escapes in an identifier/string.
pub(crate) fn decode_unicode_escapes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() && bytes[i + 1] == b'u' {
            i += 2;
            if i < bytes.len() && bytes[i] == b'{' {
                i += 1;
                let mut code = 0u32;
                while i < bytes.len() && bytes[i] != b'}' {
                    let d = match bytes[i] {
                        b'0'..=b'9' => bytes[i] - b'0',
                        b'a'..=b'f' => bytes[i] - b'a' + 10,
                        b'A'..=b'F' => bytes[i] - b'A' + 10,
                        _ => { i += 1; continue; }
                    };
                    code = code * 16 + d as u32;
                    i += 1;
                }
                if i < bytes.len() { i += 1; } // skip '}'
                if let Some(c) = char::from_u32(code) { out.push(c); }
            } else if i + 4 <= bytes.len() {
                let mut code = 0u32;
                let mut valid = true;
                for j in 0..4 {
                    let d = match bytes[i + j] {
                        b'0'..=b'9' => bytes[i + j] - b'0',
                        b'a'..=b'f' => bytes[i + j] - b'a' + 10,
                        b'A'..=b'F' => bytes[i + j] - b'A' + 10,
                        _ => { valid = false; break; }
                    };
                    code = code * 16 + d as u32;
                }
                if valid {
                    i += 4;
                    if let Some(c) = char::from_u32(code) { out.push(c); }
                }
            }
        } else {
            // Push this UTF-8 byte (via char iteration for correctness)
            let remaining = &s[i..];
            if let Some(c) = remaining.chars().next() {
                out.push(c);
                i += c.len_utf8();
            } else {
                break;
            }
        }
    }
    out
}

/// The Zinc parser: recursive descent with Pratt expression parsing.
pub struct Parser<'a> {
    tokens: Vec<Token>,
    pos: usize,
    source: &'a str,
    pub(crate) interner: &'a mut Interner,
    pub errors: Vec<ParseError>,
}

impl<'a> Parser<'a> {
    pub fn new(tokens: Vec<Token>, source: &'a str, interner: &'a mut Interner) -> Self {
        Self {
            tokens,
            pos: 0,
            source,
            interner,
            errors: Vec::new(),
        }
    }

    /// Parse a complete program (script).
    pub fn parse_program(&mut self) -> ParseResult<Program> {
        let start = self.pos();
        let mut body = Vec::new();

        while !self.at(TokenKind::Eof) {
            match statement::parse_statement(self) {
                Ok(stmt) => body.push(stmt),
                Err(e) => {
                    self.errors.push(e);
                    self.synchronize();
                }
            }
        }

        let end = self.pos();
        Ok(Program {
            body,
            source_type: SourceType::Script,
            span: Span::new(start, end),
        })
    }

    // ---- Token consumption ----

    /// Current token.
    pub(crate) fn current(&self) -> &Token {
        &self.tokens[self.pos.min(self.tokens.len() - 1)]
    }

    /// Current token kind.
    pub(crate) fn current_kind(&self) -> TokenKind {
        self.current().kind
    }

    /// Current token span start position (as u32).
    pub(crate) fn pos(&self) -> u32 {
        self.current().span.start
    }

    /// Check if current token is of the given kind.
    pub(crate) fn at(&self, kind: TokenKind) -> bool {
        self.current_kind() == kind
    }

    /// Check if current token is any of the given kinds.
    pub(crate) fn at_any(&self, kinds: &[TokenKind]) -> bool {
        kinds.contains(&self.current_kind())
    }

    /// Advance to the next token and return the consumed one.
    pub(crate) fn advance(&mut self) -> &Token {
        let tok = &self.tokens[self.pos.min(self.tokens.len() - 1)];
        if self.pos < self.tokens.len() - 1 {
            self.pos += 1;
        }
        tok
    }

    /// Consume the current token if it matches, return true. Otherwise false.
    pub(crate) fn eat(&mut self, kind: TokenKind) -> bool {
        if self.at(kind) {
            self.advance();
            true
        } else {
            false
        }
    }

    /// Consume the current token if it matches, error if not.
    pub(crate) fn expect(&mut self, kind: TokenKind) -> ParseResult<&Token> {
        if self.at(kind) {
            Ok(self.advance())
        } else {
            Err(ParseError::expected(
                &format!("{kind:?}"),
                self.current_kind(),
                self.current().span,
            ))
        }
    }

    /// Get the text of a token from the source.
    pub(crate) fn token_text(&self, token: &Token) -> &str {
        &self.source[token.span.start as usize..token.span.end as usize]
    }

    /// Get the text of the current token.
    pub(crate) fn current_text(&self) -> &str {
        self.token_text(self.current())
    }

    /// Intern the current token's text and return its StringId.
    /// If the identifier contains \uXXXX escapes, they are decoded.
    pub(crate) fn intern_current(&mut self) -> StringId {
        let text = &self.source[self.current().span.start as usize..self.current().span.end as usize];
        if text.contains("\\u") {
            let decoded = decode_unicode_escapes(text);
            self.interner.intern(&decoded)
        } else {
            let s = text.to_owned();
            self.interner.intern(&s)
        }
    }

    /// Check if the current token was preceded by a line terminator (for ASI).
    pub(crate) fn preceded_by_newline(&self) -> bool {
        self.current().preceded_by_newline
    }

    /// Expect a semicolon (with ASI support).
    pub(crate) fn expect_semicolon(&mut self) -> ParseResult<()> {
        if self.eat(TokenKind::Semicolon) {
            return Ok(());
        }
        // ASI: automatic semicolon insertion
        if self.preceded_by_newline()
            || self.at(TokenKind::RBrace)
            || self.at(TokenKind::Eof)
        {
            return Ok(());
        }
        Err(ParseError::expected(
            "';'",
            self.current_kind(),
            self.current().span,
        ))
    }

    /// Peek at the next token (one ahead of current).
    pub(crate) fn peek(&self) -> &Token {
        let next = (self.pos + 1).min(self.tokens.len() - 1);
        &self.tokens[next]
    }

    /// Error recovery: skip tokens until we find a likely statement boundary.
    fn synchronize(&mut self) {
        while !self.at(TokenKind::Eof) {
            // After a semicolon, we're likely at a new statement
            if self.eat(TokenKind::Semicolon) {
                return;
            }
            // These tokens usually start statements
            match self.current_kind() {
                TokenKind::Function
                | TokenKind::Class
                | TokenKind::Var
                | TokenKind::Let
                | TokenKind::Const
                | TokenKind::If
                | TokenKind::While
                | TokenKind::For
                | TokenKind::Return
                | TokenKind::Switch
                | TokenKind::Try
                | TokenKind::Throw => return,
                _ => {
                    self.advance();
                }
            }
        }
    }

    /// Parse a numeric literal from token text.
    pub(crate) fn parse_number(&self, text: &str) -> f64 {
        if text.starts_with("0x") || text.starts_with("0X") {
            let clean: String = text[2..].chars().filter(|&c| c != '_').collect();
            i64::from_str_radix(&clean, 16).unwrap_or(0) as f64
        } else if text.starts_with("0o") || text.starts_with("0O") {
            let clean: String = text[2..].chars().filter(|&c| c != '_').collect();
            i64::from_str_radix(&clean, 8).unwrap_or(0) as f64
        } else if text.starts_with("0b") || text.starts_with("0B") {
            let clean: String = text[2..].chars().filter(|&c| c != '_').collect();
            i64::from_str_radix(&clean, 2).unwrap_or(0) as f64
        } else {
            let clean: String = text.chars().filter(|&c| c != '_').collect();
            clean.parse::<f64>().unwrap_or(f64::NAN)
        }
    }

    /// Parse the string content between quotes (handling escapes).
    pub(crate) fn parse_string_value(&mut self, text: &str) -> StringId {
        // Strip quotes
        let inner = &text[1..text.len() - 1];
        // TODO: full escape sequence handling
        let unescaped = unescape_string(inner);
        self.interner.intern(&unescaped)
    }
}

/// Basic string unescape (handles common escape sequences).
fn unescape_string(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => result.push('\n'),
                Some('r') => result.push('\r'),
                Some('t') => result.push('\t'),
                Some('b') => result.push('\u{0008}'),
                Some('f') => result.push('\u{000C}'),
                Some('v') => result.push('\u{000B}'),
                Some('\\') => result.push('\\'),
                Some('\'') => result.push('\''),
                Some('"') => result.push('"'),
                Some('`') => result.push('`'),
                Some('0') if !matches!(chars.peek(), Some('0'..='9')) => result.push('\0'),
                Some('u') => {
                    // \uXXXX or \u{XXXX}
                    if chars.peek() == Some(&'{') {
                        chars.next(); // skip {
                        let mut hex = String::new();
                        while let Some(&c) = chars.peek() {
                            if c == '}' { chars.next(); break; }
                            hex.push(c);
                            chars.next();
                        }
                        if let Ok(code) = u32::from_str_radix(&hex, 16)
                            && let Some(ch) = char::from_u32(code) {
                                result.push(ch);
                            }
                    } else {
                        let mut hex = String::with_capacity(4);
                        for _ in 0..4 {
                            if let Some(c) = chars.next() { hex.push(c); }
                        }
                        if let Ok(code) = u32::from_str_radix(&hex, 16)
                            && let Some(ch) = char::from_u32(code) {
                                result.push(ch);
                            }
                    }
                }
                Some('x') => {
                    // \xXX hex escape
                    let mut hex = String::with_capacity(2);
                    for _ in 0..2 {
                        if let Some(c) = chars.next() { hex.push(c); }
                    }
                    if let Ok(code) = u32::from_str_radix(&hex, 16)
                        && let Some(ch) = char::from_u32(code) {
                            result.push(ch);
                        }
                }
                Some(c) => {
                    // Non-escape characters: \a -> a (identity escape in sloppy mode)
                    result.push(c);
                }
                None => result.push('\\'),
            }
        } else {
            result.push(c);
        }
    }
    result
}
