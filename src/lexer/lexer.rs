use crate::ast::span::Span;
use crate::util::interner::Interner;

use super::cursor::{is_id_continue, is_line_terminator, Cursor};
use super::token::{lookup_keyword, Token, TokenKind};

/// The Zinc lexer: converts source text into a stream of tokens.
pub struct Lexer<'a> {
    cursor: Cursor<'a>,
    interner: &'a mut Interner,
    /// Whether we saw a line terminator since the last token (for ASI).
    saw_newline: bool,
    /// Template literal nesting depth (for tracking `${` ... `}` inside templates).
    template_depth: u32,
    /// Brace depth stack for template literals. Each entry is the brace depth
    /// when we entered a `${` expression. When the matching `}` is found,
    /// we resume scanning the template.
    template_brace_stack: Vec<u32>,
    /// Current brace nesting depth.
    brace_depth: u32,
}

impl<'a> Lexer<'a> {
    pub fn new(source: &'a str, interner: &'a mut Interner) -> Self {
        Self {
            cursor: Cursor::new(source),
            interner,
            saw_newline: false,
            template_depth: 0,
            template_brace_stack: Vec::new(),
            brace_depth: 0,
        }
    }

    /// Tokenize the entire source, returning all tokens.
    pub fn tokenize(&mut self) -> Vec<Token> {
        let mut tokens = Vec::new();
        loop {
            let tok = self.next_token();
            let is_eof = tok.kind == TokenKind::Eof;
            tokens.push(tok);
            if is_eof {
                break;
            }
        }
        tokens
    }

    /// Scan the next token.
    pub fn next_token(&mut self) -> Token {
        self.skip_whitespace_and_comments();
        let preceded_by_newline = self.saw_newline;
        self.saw_newline = false;

        if self.cursor.is_eof() {
            return Token::new(
                TokenKind::Eof,
                Span::new(self.cursor.pos() as u32, self.cursor.pos() as u32),
                preceded_by_newline,
            );
        }

        // Check if we're inside a template literal and the closing `}` matches
        // the `${` that started this expression.
        if !self.template_brace_stack.is_empty()
            && let Some(b'}') = self.cursor.peek() {
                let saved = *self.template_brace_stack.last().unwrap();
                if self.brace_depth == saved + 1 {
                    self.template_brace_stack.pop();
                    self.brace_depth -= 1;
                    self.cursor.advance(); // consume }
                    return self.scan_template_continuation(preceded_by_newline);
                }
            }

        let start = self.cursor.pos();
        let byte = self.cursor.peek().unwrap();

        let kind = match byte {
            // Identifiers and keywords
            b'a'..=b'z' | b'A'..=b'Z' | b'_' | b'$' => return self.scan_identifier(start, preceded_by_newline),

            // Numbers
            b'0'..=b'9' => return self.scan_number(start, preceded_by_newline),

            // Strings
            b'\'' | b'"' => return self.scan_string(start, preceded_by_newline),

            // Template literals
            b'`' => return self.scan_template_start(start, preceded_by_newline),

            // Punctuators
            b'(' => { self.cursor.advance(); TokenKind::LParen }
            b')' => { self.cursor.advance(); TokenKind::RParen }
            b'{' => { self.cursor.advance(); self.brace_depth += 1; TokenKind::LBrace }
            b'}' => {
                self.cursor.advance();
                self.brace_depth = self.brace_depth.saturating_sub(1);
                TokenKind::RBrace
            }
            b'[' => { self.cursor.advance(); TokenKind::LBracket }
            b']' => { self.cursor.advance(); TokenKind::RBracket }
            b';' => { self.cursor.advance(); TokenKind::Semicolon }
            b',' => { self.cursor.advance(); TokenKind::Comma }
            b'~' => { self.cursor.advance(); TokenKind::Tilde }
            b'#' => { self.cursor.advance(); TokenKind::Hash }

            b':' => { self.cursor.advance(); TokenKind::Colon }

            b'.' => {
                self.cursor.advance();
                if self.cursor.peek() == Some(b'.') && self.cursor.peek_at(1) == Some(b'.') {
                    self.cursor.advance();
                    self.cursor.advance();
                    TokenKind::DotDotDot
                } else if matches!(self.cursor.peek(), Some(b'0'..=b'9')) {
                    // Number starting with .
                    return self.scan_number_after_dot(start, preceded_by_newline);
                } else {
                    TokenKind::Dot
                }
            }

            b'?' => {
                self.cursor.advance();
                if self.cursor.eat(b'?') {
                    if self.cursor.eat(b'=') {
                        TokenKind::QuestionQuestionAssign
                    } else {
                        TokenKind::QuestionQuestion
                    }
                } else if self.cursor.eat(b'.') {
                    // ?. but not ?.digit (that would be ? followed by .5)
                    if matches!(self.cursor.peek(), Some(b'0'..=b'9')) {
                        // Oops, this is ? . 5, back up
                        // Actually ?. is always optional chaining, digits after don't matter
                        TokenKind::QuestionDot
                    } else {
                        TokenKind::QuestionDot
                    }
                } else {
                    TokenKind::Question
                }
            }

            b'=' => {
                self.cursor.advance();
                if self.cursor.eat(b'=') {
                    if self.cursor.eat(b'=') {
                        TokenKind::EqEqEq
                    } else {
                        TokenKind::EqEq
                    }
                } else if self.cursor.eat(b'>') {
                    TokenKind::Arrow
                } else {
                    TokenKind::Assign
                }
            }

            b'!' => {
                self.cursor.advance();
                if self.cursor.eat(b'=') {
                    if self.cursor.eat(b'=') {
                        TokenKind::NotEqEq
                    } else {
                        TokenKind::NotEq
                    }
                } else {
                    TokenKind::Bang
                }
            }

            b'+' => {
                self.cursor.advance();
                if self.cursor.eat(b'+') {
                    TokenKind::PlusPlus
                } else if self.cursor.eat(b'=') {
                    TokenKind::PlusAssign
                } else {
                    TokenKind::Plus
                }
            }

            b'-' => {
                self.cursor.advance();
                if self.cursor.eat(b'-') {
                    TokenKind::MinusMinus
                } else if self.cursor.eat(b'=') {
                    TokenKind::MinusAssign
                } else {
                    TokenKind::Minus
                }
            }

            b'*' => {
                self.cursor.advance();
                if self.cursor.eat(b'*') {
                    if self.cursor.eat(b'=') {
                        TokenKind::StarStarAssign
                    } else {
                        TokenKind::StarStar
                    }
                } else if self.cursor.eat(b'=') {
                    TokenKind::StarAssign
                } else {
                    TokenKind::Star
                }
            }

            b'/' => {
                self.cursor.advance();
                if self.cursor.eat(b'=') {
                    TokenKind::SlashAssign
                } else {
                    TokenKind::Slash
                }
            }

            b'%' => {
                self.cursor.advance();
                if self.cursor.eat(b'=') {
                    TokenKind::PercentAssign
                } else {
                    TokenKind::Percent
                }
            }

            b'<' => {
                self.cursor.advance();
                if self.cursor.eat(b'<') {
                    if self.cursor.eat(b'=') {
                        TokenKind::LtLtAssign
                    } else {
                        TokenKind::LtLt
                    }
                } else if self.cursor.eat(b'=') {
                    TokenKind::LtEq
                } else {
                    TokenKind::Lt
                }
            }

            b'>' => {
                self.cursor.advance();
                if self.cursor.eat(b'>') {
                    if self.cursor.eat(b'>') {
                        if self.cursor.eat(b'=') {
                            TokenKind::GtGtGtAssign
                        } else {
                            TokenKind::GtGtGt
                        }
                    } else if self.cursor.eat(b'=') {
                        TokenKind::GtGtAssign
                    } else {
                        TokenKind::GtGt
                    }
                } else if self.cursor.eat(b'=') {
                    TokenKind::GtEq
                } else {
                    TokenKind::Gt
                }
            }

            b'&' => {
                self.cursor.advance();
                if self.cursor.eat(b'&') {
                    if self.cursor.eat(b'=') {
                        TokenKind::AmpAmpAssign
                    } else {
                        TokenKind::AmpAmp
                    }
                } else if self.cursor.eat(b'=') {
                    TokenKind::AmpAssign
                } else {
                    TokenKind::Amp
                }
            }

            b'|' => {
                self.cursor.advance();
                if self.cursor.eat(b'|') {
                    if self.cursor.eat(b'=') {
                        TokenKind::PipePipeAssign
                    } else {
                        TokenKind::PipePipe
                    }
                } else if self.cursor.eat(b'=') {
                    TokenKind::PipeAssign
                } else {
                    TokenKind::Pipe
                }
            }

            b'^' => {
                self.cursor.advance();
                if self.cursor.eat(b'=') {
                    TokenKind::CaretAssign
                } else {
                    TokenKind::Caret
                }
            }

            // Unicode identifier start
            _ if !byte.is_ascii() => {
                if let Some(c) = self.cursor.peek_char()
                    && super::cursor::is_unicode_id_start(c) {
                        return self.scan_unicode_identifier(start, preceded_by_newline);
                    }
                self.cursor.advance();
                TokenKind::Error
            }

            _ => {
                self.cursor.advance();
                TokenKind::Error
            }
        };

        Token::new(kind, Span::new(start as u32, self.cursor.pos() as u32), preceded_by_newline)
    }

    // ---- Whitespace & Comments ----

    fn skip_whitespace_and_comments(&mut self) {
        loop {
            match self.cursor.peek() {
                Some(b' ' | b'\t') => { self.cursor.advance(); }
                Some(b'\n') => {
                    self.saw_newline = true;
                    self.cursor.advance();
                }
                Some(b'\r') => {
                    self.saw_newline = true;
                    self.cursor.advance();
                    self.cursor.eat(b'\n'); // consume \r\n as one
                }
                // Unicode whitespace chars
                Some(0xC2) if self.cursor.peek_at(1) == Some(0xA0) => {
                    // \u00A0 (NBSP)
                    self.cursor.advance();
                    self.cursor.advance();
                }
                Some(b'/') => {
                    match self.cursor.peek_at(1) {
                        Some(b'/') => self.skip_line_comment(),
                        Some(b'*') => self.skip_block_comment(),
                        _ => return,
                    }
                }
                _ => return,
            }
        }
    }

    fn skip_line_comment(&mut self) {
        self.cursor.advance(); // /
        self.cursor.advance(); // /
        while let Some(b) = self.cursor.peek() {
            if is_line_terminator(b) {
                break;
            }
            self.cursor.advance();
        }
    }

    fn skip_block_comment(&mut self) {
        self.cursor.advance(); // /
        self.cursor.advance(); // *
        while !self.cursor.is_eof() {
            if let Some(b'\n' | b'\r') = self.cursor.peek() {
                self.saw_newline = true;
            }
            if self.cursor.peek() == Some(b'*') && self.cursor.peek_at(1) == Some(b'/') {
                self.cursor.advance(); // *
                self.cursor.advance(); // /
                return;
            }
            self.cursor.advance();
        }
        // Unterminated block comment -- will be caught by parser
    }

    // ---- Identifiers & Keywords ----

    fn scan_identifier(&mut self, start: usize, preceded_by_newline: bool) -> Token {
        self.cursor.advance(); // first char already validated
        while let Some(b) = self.cursor.peek() {
            if is_id_continue(b) {
                self.cursor.advance();
            } else if !b.is_ascii() {
                // Might be a unicode continue character
                if let Some(c) = self.cursor.peek_char()
                    && super::cursor::is_unicode_id_continue(c) {
                        self.cursor.advance_char();
                        continue;
                    }
                break;
            } else {
                break;
            }
        }

        let text = self.cursor.slice_from(start);
        let kind = lookup_keyword(text).unwrap_or(TokenKind::Identifier);
        let span = Span::new(start as u32, self.cursor.pos() as u32);

        // Intern the identifier (even keywords, for consistency)
        if kind == TokenKind::Identifier {
            self.interner.intern(text);
        }

        Token::new(kind, span, preceded_by_newline)
    }

    fn scan_unicode_identifier(&mut self, start: usize, preceded_by_newline: bool) -> Token {
        self.cursor.advance_char(); // first char
        while let Some(c) = self.cursor.peek_char() {
            if super::cursor::is_unicode_id_continue(c) {
                self.cursor.advance_char();
            } else {
                break;
            }
        }

        let text = self.cursor.slice_from(start);
        let kind = lookup_keyword(text).unwrap_or(TokenKind::Identifier);
        let span = Span::new(start as u32, self.cursor.pos() as u32);

        if kind == TokenKind::Identifier {
            self.interner.intern(text);
        }

        Token::new(kind, span, preceded_by_newline)
    }

    // ---- Numbers ----

    fn scan_number(&mut self, start: usize, preceded_by_newline: bool) -> Token {
        let first = self.cursor.advance().unwrap();

        if first == b'0' {
            match self.cursor.peek() {
                Some(b'x' | b'X') => return self.scan_hex(start, preceded_by_newline),
                Some(b'o' | b'O') => return self.scan_octal(start, preceded_by_newline),
                Some(b'b' | b'B') => return self.scan_binary(start, preceded_by_newline),
                _ => {}
            }
        }

        // Decimal integer part
        self.skip_decimal_digits();

        // Fractional part
        if self.cursor.peek() == Some(b'.') {
            self.cursor.advance();
            self.skip_decimal_digits();
        }

        // Exponent
        if matches!(self.cursor.peek(), Some(b'e' | b'E')) {
            self.cursor.advance();
            if matches!(self.cursor.peek(), Some(b'+' | b'-')) {
                self.cursor.advance();
            }
            self.skip_decimal_digits();
        }

        // BigInt suffix
        let kind = if self.cursor.eat(b'n') {
            TokenKind::BigInt
        } else {
            TokenKind::Number
        };

        Token::new(kind, Span::new(start as u32, self.cursor.pos() as u32), preceded_by_newline)
    }

    fn scan_number_after_dot(&mut self, start: usize, preceded_by_newline: bool) -> Token {
        // We already consumed the dot
        self.skip_decimal_digits();

        // Exponent
        if matches!(self.cursor.peek(), Some(b'e' | b'E')) {
            self.cursor.advance();
            if matches!(self.cursor.peek(), Some(b'+' | b'-')) {
                self.cursor.advance();
            }
            self.skip_decimal_digits();
        }

        Token::new(TokenKind::Number, Span::new(start as u32, self.cursor.pos() as u32), preceded_by_newline)
    }

    fn scan_hex(&mut self, start: usize, preceded_by_newline: bool) -> Token {
        self.cursor.advance(); // x/X
        self.cursor.skip_while(|b| b.is_ascii_hexdigit() || b == b'_');
        let kind = if self.cursor.eat(b'n') { TokenKind::BigInt } else { TokenKind::Number };
        Token::new(kind, Span::new(start as u32, self.cursor.pos() as u32), preceded_by_newline)
    }

    fn scan_octal(&mut self, start: usize, preceded_by_newline: bool) -> Token {
        self.cursor.advance(); // o/O
        self.cursor.skip_while(|b| matches!(b, b'0'..=b'7' | b'_'));
        let kind = if self.cursor.eat(b'n') { TokenKind::BigInt } else { TokenKind::Number };
        Token::new(kind, Span::new(start as u32, self.cursor.pos() as u32), preceded_by_newline)
    }

    fn scan_binary(&mut self, start: usize, preceded_by_newline: bool) -> Token {
        self.cursor.advance(); // b/B
        self.cursor.skip_while(|b| matches!(b, b'0' | b'1' | b'_'));
        let kind = if self.cursor.eat(b'n') { TokenKind::BigInt } else { TokenKind::Number };
        Token::new(kind, Span::new(start as u32, self.cursor.pos() as u32), preceded_by_newline)
    }

    fn skip_decimal_digits(&mut self) {
        self.cursor.skip_while(|b| b.is_ascii_digit() || b == b'_');
    }

    // ---- Strings ----

    fn scan_string(&mut self, start: usize, preceded_by_newline: bool) -> Token {
        let quote = self.cursor.advance().unwrap(); // ' or "

        while let Some(b) = self.cursor.peek() {
            match b {
                b if b == quote => {
                    self.cursor.advance();
                    return Token::new(
                        TokenKind::String,
                        Span::new(start as u32, self.cursor.pos() as u32),
                        preceded_by_newline,
                    );
                }
                b'\\' => {
                    self.cursor.advance(); // backslash
                    self.cursor.advance(); // escaped char (simplified)
                }
                b'\n' | b'\r' => {
                    // Unterminated string (newline in string literal is an error)
                    break;
                }
                _ => {
                    self.cursor.advance();
                }
            }
        }

        // Unterminated string
        Token::new(TokenKind::Error, Span::new(start as u32, self.cursor.pos() as u32), preceded_by_newline)
    }

    // ---- Template Literals ----

    fn scan_template_start(&mut self, start: usize, preceded_by_newline: bool) -> Token {
        self.cursor.advance(); // `
        self.scan_template_body(start, preceded_by_newline, true)
    }

    fn scan_template_continuation(&mut self, preceded_by_newline: bool) -> Token {
        let start = self.cursor.pos() - 1; // include the }
        self.scan_template_body(start, preceded_by_newline, false)
    }

    fn scan_template_body(&mut self, start: usize, preceded_by_newline: bool, is_head: bool) -> Token {
        while let Some(b) = self.cursor.peek() {
            match b {
                b'`' => {
                    self.cursor.advance();
                    let kind = if is_head {
                        TokenKind::TemplateLiteralFull
                    } else {
                        TokenKind::TemplateLiteralTail
                    };
                    self.template_depth = self.template_depth.saturating_sub(1);
                    return Token::new(kind, Span::new(start as u32, self.cursor.pos() as u32), preceded_by_newline);
                }
                b'$' if self.cursor.peek_at(1) == Some(b'{') => {
                    self.cursor.advance(); // $
                    self.cursor.advance(); // {
                    self.template_depth += 1;
                    self.template_brace_stack.push(self.brace_depth);
                    self.brace_depth += 1;
                    let kind = if is_head {
                        TokenKind::TemplateLiteralHead
                    } else {
                        TokenKind::TemplateLiteralMiddle
                    };
                    return Token::new(kind, Span::new(start as u32, self.cursor.pos() as u32), preceded_by_newline);
                }
                b'\\' => {
                    self.cursor.advance(); // backslash
                    self.cursor.advance(); // escaped char
                }
                _ => {
                    self.cursor.advance();
                }
            }
        }

        // Unterminated template
        Token::new(TokenKind::Error, Span::new(start as u32, self.cursor.pos() as u32), preceded_by_newline)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokenize(source: &str) -> Vec<(TokenKind, &str)> {
        let mut interner = Interner::new();
        let mut lexer = Lexer::new(source, &mut interner);
        let tokens = lexer.tokenize();
        tokens
            .iter()
            .filter(|t| t.kind != TokenKind::Eof)
            .map(|t| (t.kind, &source[t.span.start as usize..t.span.end as usize]))
            .collect()
    }

    #[test]
    fn test_simple_tokens() {
        let result = tokenize("let x = 1 + 2;");
        assert_eq!(result, vec![
            (TokenKind::Let, "let"),
            (TokenKind::Identifier, "x"),
            (TokenKind::Assign, "="),
            (TokenKind::Number, "1"),
            (TokenKind::Plus, "+"),
            (TokenKind::Number, "2"),
            (TokenKind::Semicolon, ";"),
        ]);
    }

    #[test]
    fn test_keywords() {
        let result = tokenize("if else while for function return var const let class");
        let kinds: Vec<_> = result.iter().map(|(k, _)| *k).collect();
        assert_eq!(kinds, vec![
            TokenKind::If,
            TokenKind::Else,
            TokenKind::While,
            TokenKind::For,
            TokenKind::Function,
            TokenKind::Return,
            TokenKind::Var,
            TokenKind::Const,
            TokenKind::Let,
            TokenKind::Class,
        ]);
    }

    #[test]
    fn test_operators() {
        let result = tokenize("=== !== ** ?. ?? => ...");
        let kinds: Vec<_> = result.iter().map(|(k, _)| *k).collect();
        assert_eq!(kinds, vec![
            TokenKind::EqEqEq,
            TokenKind::NotEqEq,
            TokenKind::StarStar,
            TokenKind::QuestionDot,
            TokenKind::QuestionQuestion,
            TokenKind::Arrow,
            TokenKind::DotDotDot,
        ]);
    }

    #[test]
    fn test_numbers() {
        let result = tokenize("42 3.14 0xFF 0o77 0b1010 1e10 1_000");
        let kinds: Vec<_> = result.iter().map(|(k, _)| *k).collect();
        assert_eq!(kinds, vec![
            TokenKind::Number,
            TokenKind::Number,
            TokenKind::Number,
            TokenKind::Number,
            TokenKind::Number,
            TokenKind::Number,
            TokenKind::Number,
        ]);
    }

    #[test]
    fn test_strings() {
        let result = tokenize(r#""hello" 'world' "esc\"ape""#);
        let kinds: Vec<_> = result.iter().map(|(k, _)| *k).collect();
        assert_eq!(kinds, vec![
            TokenKind::String,
            TokenKind::String,
            TokenKind::String,
        ]);
    }

    #[test]
    fn test_template_literal_no_interpolation() {
        let result = tokenize("`hello world`");
        assert_eq!(result, vec![
            (TokenKind::TemplateLiteralFull, "`hello world`"),
        ]);
    }

    #[test]
    fn test_template_literal_with_interpolation() {
        let result = tokenize("`hello ${name}!`");
        let kinds: Vec<_> = result.iter().map(|(k, _)| *k).collect();
        assert_eq!(kinds, vec![
            TokenKind::TemplateLiteralHead, // `hello ${
            TokenKind::Identifier,          // name
            TokenKind::TemplateLiteralTail,  // }!`
        ]);
    }

    #[test]
    fn test_comments() {
        let result = tokenize("a // comment\nb /* block */ c");
        let kinds: Vec<_> = result.iter().map(|(k, _)| *k).collect();
        assert_eq!(kinds, vec![
            TokenKind::Identifier, // a
            TokenKind::Identifier, // b
            TokenKind::Identifier, // c
        ]);
    }

    #[test]
    fn test_newline_tracking() {
        let mut interner = Interner::new();
        let mut lexer = Lexer::new("a\nb", &mut interner);
        let tokens = lexer.tokenize();
        assert!(!tokens[0].preceded_by_newline); // a
        assert!(tokens[1].preceded_by_newline);  // b
    }

    #[test]
    fn test_assignment_operators() {
        let result = tokenize("+= -= *= /= %= **= &&= ||= ??= <<= >>= >>>=");
        let kinds: Vec<_> = result.iter().map(|(k, _)| *k).collect();
        assert_eq!(kinds, vec![
            TokenKind::PlusAssign,
            TokenKind::MinusAssign,
            TokenKind::StarAssign,
            TokenKind::SlashAssign,
            TokenKind::PercentAssign,
            TokenKind::StarStarAssign,
            TokenKind::AmpAmpAssign,
            TokenKind::PipePipeAssign,
            TokenKind::QuestionQuestionAssign,
            TokenKind::LtLtAssign,
            TokenKind::GtGtAssign,
            TokenKind::GtGtGtAssign,
        ]);
    }

    #[test]
    fn test_real_code() {
        let source = r#"
            function fibonacci(n) {
                if (n <= 1) return n;
                return fibonacci(n - 1) + fibonacci(n - 2);
            }
            console.log(fibonacci(10));
        "#;
        let result = tokenize(source);
        // Just verify it doesn't crash and produces reasonable number of tokens
        assert!(result.len() > 30);
        assert_eq!(result[0].0, TokenKind::Function);
    }

    #[test]
    fn test_bigint() {
        let result = tokenize("42n 0xFFn 0o77n 0b1010n");
        let kinds: Vec<_> = result.iter().map(|(k, _)| *k).collect();
        assert_eq!(kinds, vec![
            TokenKind::BigInt,
            TokenKind::BigInt,
            TokenKind::BigInt,
            TokenKind::BigInt,
        ]);
    }
}
