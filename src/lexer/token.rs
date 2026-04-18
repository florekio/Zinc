use crate::ast::span::Span;

/// All token types in ECMAScript 2020+.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    // ---- Literals ----
    Number,
    BigInt,
    String,
    RegExp,
    TemplateLiteralHead,   // `text${
    TemplateLiteralMiddle, // }text${
    TemplateLiteralTail,   // }text`
    TemplateLiteralFull,   // `text` (no interpolation)

    // ---- Identifiers & Keywords ----
    Identifier,

    // Keywords
    Await,
    Break,
    Case,
    Catch,
    Class,
    Const,
    Continue,
    Debugger,
    Default,
    Delete,
    Do,
    Else,
    Enum,
    Export,
    Extends,
    Finally,
    For,
    Function,
    If,
    Import,
    In,
    Instanceof,
    Let,
    New,
    Of,
    Return,
    Super,
    Switch,
    This,
    Throw,
    Try,
    Typeof,
    Var,
    Void,
    While,
    With,
    Yield,

    // Strict mode reserved words
    Implements,
    Interface,
    Package,
    Private,
    Protected,
    Public,
    Static,

    // Literal keywords
    True,
    False,
    Null,
    Undefined,

    // ---- Punctuators ----
    // Grouping
    LParen,    // (
    RParen,    // )
    LBrace,    // {
    RBrace,    // }
    LBracket,  // [
    RBracket,  // ]

    // Operators
    Dot,          // .
    DotDotDot,    // ...
    Semicolon,    // ;
    Comma,        // ,
    Colon,        // :
    Question,     // ?
    QuestionDot,  // ?.
    QuestionQuestion, // ??
    Arrow,        // =>
    Hash,         // #  (bare hash, error recovery)
    PrivateIdentifier, // #name  (private field/method name)

    // Assignment
    Assign,           // =
    PlusAssign,       // +=
    MinusAssign,      // -=
    StarAssign,       // *=
    SlashAssign,      // /=
    PercentAssign,    // %=
    StarStarAssign,   // **=
    AmpAssign,        // &=
    PipeAssign,       // |=
    CaretAssign,      // ^=
    LtLtAssign,       // <<=
    GtGtAssign,       // >>=
    GtGtGtAssign,     // >>>=
    AmpAmpAssign,     // &&=
    PipePipeAssign,   // ||=
    QuestionQuestionAssign, // ??=

    // Arithmetic
    Plus,       // +
    Minus,      // -
    Star,       // *
    Slash,      // /
    Percent,    // %
    StarStar,   // **
    PlusPlus,   // ++
    MinusMinus, // --

    // Comparison
    EqEq,      // ==
    NotEq,     // !=
    EqEqEq,    // ===
    NotEqEq,   // !==
    Lt,        // <
    Gt,        // >
    LtEq,      // <=
    GtEq,      // >=

    // Bitwise
    Amp,       // &
    Pipe,      // |
    Caret,     // ^
    Tilde,     // ~
    LtLt,      // <<
    GtGt,      // >>
    GtGtGt,    // >>>

    // Logical
    AmpAmp,   // &&
    PipePipe, // ||
    Bang,      // !

    // ---- Special ----
    Eof,
    /// Illegal/unexpected character
    Error,
}

impl TokenKind {
    /// Returns true if this token is a keyword that can also be used as an identifier
    /// in non-strict mode (contextual keywords).
    pub fn is_contextual_keyword(&self) -> bool {
        matches!(
            self,
            TokenKind::Let
                | TokenKind::Of
                | TokenKind::Yield
                | TokenKind::Await
                | TokenKind::Static
                | TokenKind::Implements
                | TokenKind::Interface
                | TokenKind::Package
                | TokenKind::Private
                | TokenKind::Protected
                | TokenKind::Public
        )
    }

    pub fn is_assignment_operator(&self) -> bool {
        matches!(
            self,
            TokenKind::Assign
                | TokenKind::PlusAssign
                | TokenKind::MinusAssign
                | TokenKind::StarAssign
                | TokenKind::SlashAssign
                | TokenKind::PercentAssign
                | TokenKind::StarStarAssign
                | TokenKind::AmpAssign
                | TokenKind::PipeAssign
                | TokenKind::CaretAssign
                | TokenKind::LtLtAssign
                | TokenKind::GtGtAssign
                | TokenKind::GtGtGtAssign
                | TokenKind::AmpAmpAssign
                | TokenKind::PipePipeAssign
                | TokenKind::QuestionQuestionAssign
        )
    }
}

/// A token with its kind, span, and whether it was preceded by a line terminator
/// (needed for ASI).
#[derive(Debug, Clone)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
    /// True if there was at least one line terminator before this token.
    pub preceded_by_newline: bool,
}

impl Token {
    pub fn new(kind: TokenKind, span: Span, preceded_by_newline: bool) -> Self {
        Self {
            kind,
            span,
            preceded_by_newline,
        }
    }
}

/// Look up a keyword from an identifier string.
pub fn lookup_keyword(s: &str) -> Option<TokenKind> {
    match s {
        "await" => Some(TokenKind::Await),
        "break" => Some(TokenKind::Break),
        "case" => Some(TokenKind::Case),
        "catch" => Some(TokenKind::Catch),
        "class" => Some(TokenKind::Class),
        "const" => Some(TokenKind::Const),
        "continue" => Some(TokenKind::Continue),
        "debugger" => Some(TokenKind::Debugger),
        "default" => Some(TokenKind::Default),
        "delete" => Some(TokenKind::Delete),
        "do" => Some(TokenKind::Do),
        "else" => Some(TokenKind::Else),
        "enum" => Some(TokenKind::Enum),
        "export" => Some(TokenKind::Export),
        "extends" => Some(TokenKind::Extends),
        "false" => Some(TokenKind::False),
        "finally" => Some(TokenKind::Finally),
        "for" => Some(TokenKind::For),
        "function" => Some(TokenKind::Function),
        "if" => Some(TokenKind::If),
        "import" => Some(TokenKind::Import),
        "in" => Some(TokenKind::In),
        "instanceof" => Some(TokenKind::Instanceof),
        "let" => Some(TokenKind::Let),
        "new" => Some(TokenKind::New),
        "null" => Some(TokenKind::Null),
        "of" => Some(TokenKind::Of),
        "return" => Some(TokenKind::Return),
        "super" => Some(TokenKind::Super),
        "switch" => Some(TokenKind::Switch),
        "this" => Some(TokenKind::This),
        "throw" => Some(TokenKind::Throw),
        "true" => Some(TokenKind::True),
        "try" => Some(TokenKind::Try),
        "typeof" => Some(TokenKind::Typeof),
        "undefined" => Some(TokenKind::Undefined),
        "var" => Some(TokenKind::Var),
        "void" => Some(TokenKind::Void),
        "while" => Some(TokenKind::While),
        "with" => Some(TokenKind::With),
        "yield" => Some(TokenKind::Yield),
        // Strict mode reserved
        "implements" => Some(TokenKind::Implements),
        "interface" => Some(TokenKind::Interface),
        "package" => Some(TokenKind::Package),
        "private" => Some(TokenKind::Private),
        "protected" => Some(TokenKind::Protected),
        "public" => Some(TokenKind::Public),
        "static" => Some(TokenKind::Static),
        _ => None,
    }
}
