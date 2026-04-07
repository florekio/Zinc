use crate::ast::span::Span;
use crate::lexer::token::TokenKind;

#[derive(Debug)]
pub struct ParseError {
    pub message: String,
    pub span: Span,
}

impl ParseError {
    pub fn new(message: impl Into<String>, span: Span) -> Self {
        Self {
            message: message.into(),
            span,
        }
    }

    pub fn expected(expected: &str, got: TokenKind, span: Span) -> Self {
        Self {
            message: format!("Expected {expected}, got {got:?}"),
            span,
        }
    }

    pub fn unexpected(kind: TokenKind, span: Span) -> Self {
        Self {
            message: format!("Unexpected token {kind:?}"),
            span,
        }
    }
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "SyntaxError at {}..{}: {}",
            self.span.start, self.span.end, self.message
        )
    }
}

impl std::error::Error for ParseError {}

pub type ParseResult<T> = Result<T, ParseError>;
