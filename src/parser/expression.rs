use crate::ast::node::*;
use crate::ast::span::Span;
use crate::lexer::token::TokenKind;

use super::error::{ParseError, ParseResult};
use super::parser::Parser;

/// Binding power (precedence) for Pratt parsing.
/// Higher = binds tighter. Returns (left_bp, right_bp).
/// Left-associative: right_bp = left_bp + 1
/// Right-associative: right_bp = left_bp
fn infix_binding_power(kind: TokenKind) -> Option<(u8, u8)> {
    match kind {
        TokenKind::Comma => Some((1, 2)),
        // Assignment is right-associative
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
        | TokenKind::QuestionQuestionAssign => Some((3, 2)),
        // Ternary
        TokenKind::Question => Some((4, 3)),
        // Nullish coalescing
        TokenKind::QuestionQuestion => Some((5, 6)),
        // Logical OR
        TokenKind::PipePipe => Some((7, 8)),
        // Logical AND
        TokenKind::AmpAmp => Some((9, 10)),
        // Bitwise OR
        TokenKind::Pipe => Some((11, 12)),
        // Bitwise XOR
        TokenKind::Caret => Some((13, 14)),
        // Bitwise AND
        TokenKind::Amp => Some((15, 16)),
        // Equality
        TokenKind::EqEq | TokenKind::NotEq | TokenKind::EqEqEq | TokenKind::NotEqEq => {
            Some((17, 18))
        }
        // Relational
        TokenKind::Lt
        | TokenKind::Gt
        | TokenKind::LtEq
        | TokenKind::GtEq
        | TokenKind::Instanceof
        | TokenKind::In => Some((19, 20)),
        // Shift
        TokenKind::LtLt | TokenKind::GtGt | TokenKind::GtGtGt => Some((21, 22)),
        // Additive
        TokenKind::Plus | TokenKind::Minus => Some((23, 24)),
        // Multiplicative
        TokenKind::Star | TokenKind::Slash | TokenKind::Percent => Some((25, 26)),
        // Exponentiation (right-associative)
        TokenKind::StarStar => Some((28, 27)),
        _ => None,
    }
}

/// Parse an expression with minimum binding power.
pub fn parse_expression(p: &mut Parser, min_bp: u8) -> ParseResult<Expression> {
    let mut left = parse_prefix(p)?;

    loop {
        let kind = p.current_kind();

        // Postfix operators
        match kind {
            TokenKind::PlusPlus | TokenKind::MinusMinus if !p.preceded_by_newline() => {
                let op = if kind == TokenKind::PlusPlus {
                    UpdateOperator::Increment
                } else {
                    UpdateOperator::Decrement
                };
                let start = expr_span(&left).start;
                p.advance();
                left = Expression::Update(Box::new(UpdateExpression {
                    operator: op,
                    argument: left,
                    prefix: false,
                    span: Span::new(start, p.current().span.start),
                }));
                continue;
            }
            // Member access and calls bind very tightly
            TokenKind::Dot if 30 >= min_bp => {
                p.advance();
                let prop_name = p.intern_current();
                // Keywords are valid after dot, so accept any token as property name
                if p.at(TokenKind::Identifier) || is_keyword_property(p.current_kind()) {
                    p.advance();
                } else {
                    return Err(ParseError::expected("property name", p.current_kind(), p.current().span));
                }
                let start = expr_span(&left).start;
                left = Expression::Member(Box::new(MemberExpression {
                    object: left,
                    property: MemberProperty::Identifier(prop_name),
                    computed: false,
                    span: Span::new(start, p.pos()),
                }));
                continue;
            }
            TokenKind::LBracket if 30 >= min_bp => {
                p.advance(); // [
                let prop = parse_expression(p, 0)?;
                p.expect(TokenKind::RBracket)?;
                let start = expr_span(&left).start;
                left = Expression::Member(Box::new(MemberExpression {
                    object: left,
                    property: MemberProperty::Expression(prop),
                    computed: true,
                    span: Span::new(start, p.pos()),
                }));
                continue;
            }
            TokenKind::LParen if 30 >= min_bp => {
                let args = parse_arguments(p)?;
                let start = expr_span(&left).start;
                left = Expression::Call(Box::new(CallExpression {
                    callee: left,
                    arguments: args,
                    span: Span::new(start, p.pos()),
                }));
                continue;
            }
            // Tagged template: tag`hello${x}world`
            TokenKind::TemplateLiteralFull | TokenKind::TemplateLiteralHead if 30 >= min_bp => {
                let template = parse_template_literal(p)?;
                let start = expr_span(&left).start;
                left = Expression::TaggedTemplate(Box::new(TaggedTemplateExpression {
                    tag: left,
                    quasi: template,
                    span: Span::new(start, p.pos()),
                }));
                continue;
            }
            TokenKind::QuestionDot if 30 >= min_bp => {
                // Build an OptionalChainExpression collecting all ?. and . and () chains
                let start = expr_span(&left).start;
                let mut chain = Vec::new();

                // Parse first ?. element
                p.advance(); // ?.
                chain.push(parse_optional_chain_element(p, true)?);

                // Continue collecting chain elements (both ?. and regular . / () / [])
                loop {
                    if p.at(TokenKind::QuestionDot) {
                        p.advance();
                        chain.push(parse_optional_chain_element(p, true)?);
                    } else if p.at(TokenKind::Dot) {
                        p.advance();
                        let prop_name = p.intern_current();
                        p.advance();
                        chain.push(OptionalChainElement::Member {
                            property: MemberProperty::Identifier(prop_name),
                            computed: false,
                            optional: false,
                        });
                    } else if p.at(TokenKind::LBracket) {
                        p.advance();
                        let prop = parse_expression(p, 0)?;
                        p.expect(TokenKind::RBracket)?;
                        chain.push(OptionalChainElement::Member {
                            property: MemberProperty::Expression(prop),
                            computed: true,
                            optional: false,
                        });
                    } else if p.at(TokenKind::LParen) {
                        let args = parse_arguments(p)?;
                        chain.push(OptionalChainElement::Call {
                            arguments: args,
                            optional: false,
                        });
                    } else {
                        break;
                    }
                }

                left = Expression::OptionalChain(Box::new(OptionalChainExpression {
                    base: left,
                    chain,
                    span: Span::new(start, p.pos()),
                }));
                continue;
            }
            _ => {}
        }

        // Infix operators
        if let Some((l_bp, r_bp)) = infix_binding_power(kind) {
            if l_bp < min_bp {
                break;
            }

            // Ternary
            if kind == TokenKind::Question {
                p.advance(); // ?
                let consequent = parse_expression(p, 0)?;
                p.expect(TokenKind::Colon)?;
                let alternate = parse_expression(p, r_bp)?;
                let start = expr_span(&left).start;
                let end = expr_span(&alternate).end;
                left = Expression::Conditional(Box::new(ConditionalExpression {
                    test: left,
                    consequent,
                    alternate,
                    span: Span::new(start, end),
                }));
                continue;
            }

            // Assignment
            if kind.is_assignment_operator() {
                p.advance();
                let right = parse_expression(p, r_bp)?;
                let start = expr_span(&left).start;
                let end = expr_span(&right).end;
                let op = match kind {
                    TokenKind::Assign => AssignmentOperator::Assign,
                    TokenKind::PlusAssign => AssignmentOperator::AddAssign,
                    TokenKind::MinusAssign => AssignmentOperator::SubAssign,
                    TokenKind::StarAssign => AssignmentOperator::MulAssign,
                    TokenKind::SlashAssign => AssignmentOperator::DivAssign,
                    TokenKind::PercentAssign => AssignmentOperator::RemAssign,
                    TokenKind::StarStarAssign => AssignmentOperator::ExpAssign,
                    TokenKind::AmpAssign => AssignmentOperator::BitAndAssign,
                    TokenKind::PipeAssign => AssignmentOperator::BitOrAssign,
                    TokenKind::CaretAssign => AssignmentOperator::BitXorAssign,
                    TokenKind::LtLtAssign => AssignmentOperator::ShlAssign,
                    TokenKind::GtGtAssign => AssignmentOperator::ShrAssign,
                    TokenKind::GtGtGtAssign => AssignmentOperator::UShrAssign,
                    TokenKind::AmpAmpAssign => AssignmentOperator::AndAssign,
                    TokenKind::PipePipeAssign => AssignmentOperator::OrAssign,
                    TokenKind::QuestionQuestionAssign => AssignmentOperator::NullishAssign,
                    _ => unreachable!(),
                };
                let target = expr_to_assignment_target(left)?;
                left = Expression::Assignment(Box::new(AssignmentExpression {
                    operator: op,
                    left: target,
                    right,
                    span: Span::new(start, end),
                }));
                continue;
            }

            p.advance();
            let right = parse_expression(p, r_bp)?;
            let start = expr_span(&left).start;
            let end = expr_span(&right).end;

            // Logical operators
            if matches!(kind, TokenKind::AmpAmp | TokenKind::PipePipe | TokenKind::QuestionQuestion) {
                let op = match kind {
                    TokenKind::AmpAmp => LogicalOperator::And,
                    TokenKind::PipePipe => LogicalOperator::Or,
                    TokenKind::QuestionQuestion => LogicalOperator::NullishCoalescing,
                    _ => unreachable!(),
                };
                left = Expression::Logical(Box::new(LogicalExpression {
                    operator: op,
                    left,
                    right,
                    span: Span::new(start, end),
                }));
                continue;
            }

            // Binary operators
            let op = match kind {
                TokenKind::Plus => BinaryOperator::Add,
                TokenKind::Minus => BinaryOperator::Sub,
                TokenKind::Star => BinaryOperator::Mul,
                TokenKind::Slash => BinaryOperator::Div,
                TokenKind::Percent => BinaryOperator::Rem,
                TokenKind::StarStar => BinaryOperator::Exp,
                TokenKind::EqEq => BinaryOperator::EqEq,
                TokenKind::NotEq => BinaryOperator::NotEq,
                TokenKind::EqEqEq => BinaryOperator::StrictEq,
                TokenKind::NotEqEq => BinaryOperator::StrictNotEq,
                TokenKind::Lt => BinaryOperator::Lt,
                TokenKind::LtEq => BinaryOperator::LtEq,
                TokenKind::Gt => BinaryOperator::Gt,
                TokenKind::GtEq => BinaryOperator::GtEq,
                TokenKind::Amp => BinaryOperator::BitAnd,
                TokenKind::Pipe => BinaryOperator::BitOr,
                TokenKind::Caret => BinaryOperator::BitXor,
                TokenKind::LtLt => BinaryOperator::Shl,
                TokenKind::GtGt => BinaryOperator::Shr,
                TokenKind::GtGtGt => BinaryOperator::UShr,
                TokenKind::In => BinaryOperator::In,
                TokenKind::Instanceof => BinaryOperator::InstanceOf,
                TokenKind::Comma => {
                    // Comma expression: collect into a sequence
                    let mut exprs = vec![left, right];
                    while p.eat(TokenKind::Comma) {
                        exprs.push(parse_expression(p, r_bp)?);
                    }
                    let end = expr_span(exprs.last().unwrap()).end;
                    left = Expression::Sequence(SequenceExpression {
                        expressions: exprs,
                        span: Span::new(start, end),
                    });
                    continue;
                }
                _ => unreachable!("unhandled infix operator: {kind:?}"),
            };

            left = Expression::Binary(Box::new(BinaryExpression {
                operator: op,
                left,
                right,
                span: Span::new(start, end),
            }));
            continue;
        }

        break;
    }

    Ok(left)
}

/// Parse a prefix expression (unary, literal, identifier, etc.)
fn parse_prefix(p: &mut Parser) -> ParseResult<Expression> {
    let kind = p.current_kind();
    let start = p.pos();

    match kind {
        // ---- Literals ----
        TokenKind::Number => {
            let text = p.current_text().to_owned();
            let span = p.current().span;
            p.advance();
            let value = p.parse_number(&text);
            Ok(Expression::NumberLiteral(NumberLiteral { value, span }))
        }
        TokenKind::BigInt => {
            // Treat BigInt as a regular number (strip 'n' suffix)
            let text = p.current_text().to_owned();
            let span = p.current().span;
            p.advance();
            let stripped = text.trim_end_matches('n');
            let value = p.parse_number(stripped);
            Ok(Expression::NumberLiteral(NumberLiteral { value, span }))
        }
        TokenKind::String => {
            let text = p.current_text().to_owned();
            let span = p.current().span;
            p.advance();
            let value = p.parse_string_value(&text);
            Ok(Expression::StringLiteral(StringLiteral { value, span }))
        }
        TokenKind::True => {
            let span = p.current().span;
            p.advance();
            Ok(Expression::BooleanLiteral(BooleanLiteral { value: true, span }))
        }
        TokenKind::False => {
            let span = p.current().span;
            p.advance();
            Ok(Expression::BooleanLiteral(BooleanLiteral { value: false, span }))
        }
        TokenKind::Null => {
            let span = p.current().span;
            p.advance();
            Ok(Expression::NullLiteral(span))
        }
        TokenKind::RegExp => {
            let text = p.current_text().to_owned();
            let span = p.current().span;
            p.advance();
            // text is "/pattern/flags" — find closing slash (last unescaped /)
            let inner = &text[1..]; // skip opening '/'
            let closing = inner.rfind('/').unwrap_or(inner.len());
            let pattern_str = &inner[..closing];
            let flags_str = &inner[closing + 1..];
            let pattern = p.interner.intern(pattern_str);
            let flags = p.interner.intern(flags_str);
            Ok(Expression::RegExpLiteral(RegExpLiteral { pattern, flags, span }))
        }
        TokenKind::Undefined => {
            // `undefined` is technically an identifier, but we treat it as a keyword
            let span = p.current().span;
            let name = p.intern_current();
            p.advance();
            Ok(Expression::Identifier(Identifier { name, span }))
        }

        // ---- Identifiers ----
        TokenKind::Identifier => {
            let name = p.intern_current();
            let span = p.current().span;
            p.advance();

            // Check for arrow function: `ident =>`
            if p.at(TokenKind::Arrow) && !p.preceded_by_newline() {
                p.advance(); // =>
                let body = parse_arrow_body(p)?;
                let end = arrow_body_end(&body);
                return Ok(Expression::ArrowFunction(Box::new(ArrowFunctionExpression {
                    params: vec![Pattern::Identifier(Identifier { name, span })],
                    body,
                    is_async: false,
                    span: Span::new(start, end),
                })));
            }

            Ok(Expression::Identifier(Identifier { name, span }))
        }

        // Contextual keywords usable as identifiers (except await/yield which have special semantics)
        kind if kind.is_contextual_keyword() && kind != TokenKind::Await && kind != TokenKind::Yield => {
            let name = p.intern_current();
            let span = p.current().span;
            p.advance();
            Ok(Expression::Identifier(Identifier { name, span }))
        }

        TokenKind::This => {
            let span = p.current().span;
            p.advance();
            Ok(Expression::This(span))
        }

        TokenKind::Super => {
            let span = p.current().span;
            p.advance();
            Ok(Expression::Super(span))
        }

        // ---- Grouping / Arrow ----
        TokenKind::LParen => {
            p.advance(); // (
            if p.at(TokenKind::RParen) {
                // () => ... (arrow with no params)
                p.advance(); // )
                p.expect(TokenKind::Arrow)?;
                let body = parse_arrow_body(p)?;
                let end = arrow_body_end(&body);
                return Ok(Expression::ArrowFunction(Box::new(ArrowFunctionExpression {
                    params: vec![],
                    body,
                    is_async: false,
                    span: Span::new(start, end),
                })));
            }
            let expr = parse_expression(p, 0)?;
            p.expect(TokenKind::RParen)?;

            // Check for arrow: (params) =>
            if p.at(TokenKind::Arrow) && !p.preceded_by_newline() {
                p.advance(); // =>
                let params = expr_to_params(expr)?;
                let body = parse_arrow_body(p)?;
                let end = arrow_body_end(&body);
                return Ok(Expression::ArrowFunction(Box::new(ArrowFunctionExpression {
                    params,
                    body,
                    is_async: false,
                    span: Span::new(start, end),
                })));
            }

            Ok(expr)
        }

        // ---- Array literal ----
        TokenKind::LBracket => {
            p.advance(); // [
            let mut elements = Vec::new();
            while !p.at(TokenKind::RBracket) && !p.at(TokenKind::Eof) {
                if p.at(TokenKind::Comma) {
                    // Elision
                    elements.push(None);
                    p.advance();
                    continue;
                }
                if p.at(TokenKind::DotDotDot) {
                    let spread_start = p.pos();
                    p.advance();
                    let arg = parse_expression(p, 3)?;
                    let end = expr_span(&arg).end;
                    elements.push(Some(Expression::Spread(Box::new(SpreadElement {
                        argument: arg,
                        span: Span::new(spread_start, end),
                    }))));
                } else {
                    elements.push(Some(parse_expression(p, 3)?)); // bp > comma
                }
                if !p.at(TokenKind::RBracket) {
                    p.expect(TokenKind::Comma)?;
                }
            }
            p.expect(TokenKind::RBracket)?;
            Ok(Expression::Array(ArrayExpression {
                elements,
                span: Span::new(start, p.pos()),
            }))
        }

        // ---- Object literal ----
        TokenKind::LBrace => parse_object_expression(p),

        // ---- Function expression ----
        TokenKind::Function => parse_function_expression(p),

        // ---- Unary prefix operators ----
        TokenKind::Minus => {
            p.advance();
            let arg = parse_expression(p, 27)?; // unary prefix has high bp
            let end = expr_span(&arg).end;
            Ok(Expression::Unary(Box::new(UnaryExpression {
                operator: UnaryOperator::Minus,
                argument: arg,
                prefix: true,
                span: Span::new(start, end),
            })))
        }
        TokenKind::Plus => {
            p.advance();
            let arg = parse_expression(p, 27)?;
            let end = expr_span(&arg).end;
            Ok(Expression::Unary(Box::new(UnaryExpression {
                operator: UnaryOperator::Plus,
                argument: arg,
                prefix: true,
                span: Span::new(start, end),
            })))
        }
        TokenKind::Bang => {
            p.advance();
            let arg = parse_expression(p, 27)?;
            let end = expr_span(&arg).end;
            Ok(Expression::Unary(Box::new(UnaryExpression {
                operator: UnaryOperator::Not,
                argument: arg,
                prefix: true,
                span: Span::new(start, end),
            })))
        }
        TokenKind::Tilde => {
            p.advance();
            let arg = parse_expression(p, 27)?;
            let end = expr_span(&arg).end;
            Ok(Expression::Unary(Box::new(UnaryExpression {
                operator: UnaryOperator::BitNot,
                argument: arg,
                prefix: true,
                span: Span::new(start, end),
            })))
        }
        TokenKind::Typeof => {
            p.advance();
            let arg = parse_expression(p, 27)?;
            let end = expr_span(&arg).end;
            Ok(Expression::Unary(Box::new(UnaryExpression {
                operator: UnaryOperator::TypeOf,
                argument: arg,
                prefix: true,
                span: Span::new(start, end),
            })))
        }
        TokenKind::Void => {
            p.advance();
            let arg = parse_expression(p, 27)?;
            let end = expr_span(&arg).end;
            Ok(Expression::Unary(Box::new(UnaryExpression {
                operator: UnaryOperator::Void,
                argument: arg,
                prefix: true,
                span: Span::new(start, end),
            })))
        }
        TokenKind::Delete => {
            p.advance();
            let arg = parse_expression(p, 27)?;
            let end = expr_span(&arg).end;
            Ok(Expression::Unary(Box::new(UnaryExpression {
                operator: UnaryOperator::Delete,
                argument: arg,
                prefix: true,
                span: Span::new(start, end),
            })))
        }
        TokenKind::PlusPlus => {
            p.advance();
            let arg = parse_expression(p, 27)?;
            let end = expr_span(&arg).end;
            Ok(Expression::Update(Box::new(UpdateExpression {
                operator: UpdateOperator::Increment,
                argument: arg,
                prefix: true,
                span: Span::new(start, end),
            })))
        }
        TokenKind::MinusMinus => {
            p.advance();
            let arg = parse_expression(p, 27)?;
            let end = expr_span(&arg).end;
            Ok(Expression::Update(Box::new(UpdateExpression {
                operator: UpdateOperator::Decrement,
                argument: arg,
                prefix: true,
                span: Span::new(start, end),
            })))
        }

        // ---- new ----
        TokenKind::New => {
            p.advance();
            // Parse callee at BP 31 (higher than call BP 30) so () is NOT consumed as a call
            let callee = parse_expression(p, 31)?;
            let args = if p.at(TokenKind::LParen) {
                parse_arguments(p)?
            } else {
                Vec::new()
            };
            Ok(Expression::New(Box::new(NewExpression {
                callee,
                arguments: args,
                span: Span::new(start, p.pos()),
            })))
        }

        // ---- yield ----
        TokenKind::Yield => {
            p.advance();
            let delegate = p.eat(TokenKind::Star);
            let argument = if !p.preceded_by_newline()
                && !p.at_any(&[
                    TokenKind::Semicolon,
                    TokenKind::RBrace,
                    TokenKind::RParen,
                    TokenKind::RBracket,
                    TokenKind::Comma,
                    TokenKind::Colon,
                    TokenKind::Eof,
                ])
            {
                Some(parse_expression(p, 3)?)
            } else {
                None
            };
            let end = argument
                .as_ref()
                .map(|a| expr_span(a).end)
                .unwrap_or(p.pos());
            Ok(Expression::Yield(Box::new(YieldExpression {
                argument,
                delegate,
                span: Span::new(start, end),
            })))
        }

        // ---- await ----
        TokenKind::Await => {
            p.advance();
            let argument = parse_expression(p, 27)?;
            let end = expr_span(&argument).end;
            Ok(Expression::Await(Box::new(AwaitExpression {
                argument,
                span: Span::new(start, end),
            })))
        }

        // ---- Spread (in array/call context) ----
        TokenKind::DotDotDot => {
            p.advance();
            let arg = parse_expression(p, 3)?;
            let end = expr_span(&arg).end;
            Ok(Expression::Spread(Box::new(SpreadElement {
                argument: arg,
                span: Span::new(start, end),
            })))
        }

        // ---- Template literal ----
        TokenKind::TemplateLiteralFull => {
            let text = p.current_text().to_owned();
            let span = p.current().span;
            p.advance();
            let raw = p.interner.intern(&text[1..text.len() - 1]);
            let cooked = Some(raw);
            Ok(Expression::TemplateLiteral(TemplateLiteral {
                quasis: vec![TemplateElement {
                    raw,
                    cooked,
                    tail: true,
                    span,
                }],
                expressions: vec![],
                span,
            }))
        }

        // Template literal with interpolation: `text${expr}text${expr}text`
        TokenKind::TemplateLiteralHead => {
            let head_text = p.current_text().to_owned();
            let start_span = p.current().span;
            p.advance();
            // Head text is `text${ — strip ` and ${
            let head_str = &head_text[1..head_text.len() - 2];
            let head_id = p.interner.intern(head_str);

            let mut quasis = vec![TemplateElement {
                raw: head_id,
                cooked: Some(head_id),
                tail: false,
                span: start_span,
            }];
            let mut expressions = Vec::new();

            loop {
                // Parse the interpolated expression
                let expr = parse_expression(p, 0)?;
                expressions.push(expr);

                // Next token should be TemplateLiteralMiddle or TemplateLiteralTail
                match p.current_kind() {
                    TokenKind::TemplateLiteralMiddle => {
                        let text = p.current_text().to_owned();
                        let span = p.current().span;
                        p.advance();
                        // Middle text is }text${ — strip } and ${
                        let mid_str = &text[1..text.len() - 2];
                        let mid_id = p.interner.intern(mid_str);
                        quasis.push(TemplateElement {
                            raw: mid_id,
                            cooked: Some(mid_id),
                            tail: false,
                            span,
                        });
                    }
                    TokenKind::TemplateLiteralTail => {
                        let text = p.current_text().to_owned();
                        let span = p.current().span;
                        p.advance();
                        // Tail text is }text` — strip } and `
                        let tail_str = &text[1..text.len() - 1];
                        let tail_id = p.interner.intern(tail_str);
                        quasis.push(TemplateElement {
                            raw: tail_id,
                            cooked: Some(tail_id),
                            tail: true,
                            span,
                        });
                        break;
                    }
                    _ => {
                        return Err(ParseError::expected(
                            "template continuation",
                            p.current_kind(),
                            p.current().span,
                        ));
                    }
                }
            }

            let end = p.pos();
            Ok(Expression::TemplateLiteral(TemplateLiteral {
                quasis,
                expressions,
                span: Span::new(start_span.start, end),
            }))
        }

        _ => Err(ParseError::unexpected(kind, p.current().span)),
    }
}

/// Parse a single optional chain element after `?.`
fn parse_optional_chain_element(p: &mut Parser, optional: bool) -> ParseResult<OptionalChainElement> {
    if p.at(TokenKind::LParen) {
        let args = parse_arguments(p)?;
        Ok(OptionalChainElement::Call { arguments: args, optional })
    } else if p.at(TokenKind::LBracket) {
        p.advance(); // [
        let prop = parse_expression(p, 0)?;
        p.expect(TokenKind::RBracket)?;
        Ok(OptionalChainElement::Member {
            property: MemberProperty::Expression(prop),
            computed: true,
            optional,
        })
    } else {
        let prop_name = p.intern_current();
        p.advance();
        Ok(OptionalChainElement::Member {
            property: MemberProperty::Identifier(prop_name),
            computed: false,
            optional,
        })
    }
}

/// Parse a parenthesized argument list: `(expr, expr, ...)`
pub fn parse_arguments(p: &mut Parser) -> ParseResult<Vec<Expression>> {
    p.expect(TokenKind::LParen)?;
    let mut args = Vec::new();
    while !p.at(TokenKind::RParen) && !p.at(TokenKind::Eof) {
        if p.at(TokenKind::DotDotDot) {
            let start = p.pos();
            p.advance();
            let arg = parse_expression(p, 3)?;
            let end = expr_span(&arg).end;
            args.push(Expression::Spread(Box::new(SpreadElement {
                argument: arg,
                span: Span::new(start, end),
            })));
        } else {
            args.push(parse_expression(p, 3)?); // bp > comma (don't consume commas as sequence)
        }
        if !p.at(TokenKind::RParen) {
            p.expect(TokenKind::Comma)?;
        }
    }
    p.expect(TokenKind::RParen)?;
    Ok(args)
}

/// Parse an object expression: `{ key: value, ... }`
fn parse_object_expression(p: &mut Parser) -> ParseResult<Expression> {
    let start = p.pos();
    p.advance(); // {
    let mut properties = Vec::new();

    while !p.at(TokenKind::RBrace) && !p.at(TokenKind::Eof) {
        if p.at(TokenKind::DotDotDot) {
            let spread_start = p.pos();
            p.advance();
            let arg = parse_expression(p, 3)?;
            let end = expr_span(&arg).end;
            properties.push(ObjectProperty::SpreadElement(SpreadElement {
                argument: arg,
                span: Span::new(spread_start, end),
            }));
        } else {
            properties.push(ObjectProperty::Property(parse_property(p)?));
        }
        if !p.at(TokenKind::RBrace) {
            p.expect(TokenKind::Comma)?;
        }
    }
    p.expect(TokenKind::RBrace)?;

    Ok(Expression::Object(ObjectExpression {
        properties,
        span: Span::new(start, p.pos()),
    }))
}

fn parse_property(p: &mut Parser) -> ParseResult<Property> {
    let start = p.pos();

    // Check for getter/setter
    let text = p.current_text().to_owned();
    if (text == "get" || text == "set") && p.current_kind() == TokenKind::Identifier {
        let next = p.peek().kind;
        if next == TokenKind::Identifier
            || next == TokenKind::String
            || next == TokenKind::Number
            || next == TokenKind::LBracket
        {
            let kind_val = if text == "get" {
                PropertyKindVal::Get
            } else {
                PropertyKindVal::Set
            };
            p.advance(); // get/set
            let (key, computed) = parse_property_key(p)?;
            // Parse the function value
            let func = parse_function_body(p, false, false)?;
            return Ok(Property {
                key,
                value: func,
                kind: kind_val,
                shorthand: false,
                computed,
                method: true,
                span: Span::new(start, p.pos()),
            });
        }
    }

    // async method / async generator method / plain generator method
    // Detect `async foo()`, `async *foo()`, and `*foo()`
    let is_async_method = text == "async"
        && p.current_kind() == TokenKind::Identifier
        && !p.peek().preceded_by_newline
        && matches!(
            p.peek().kind,
            TokenKind::Identifier | TokenKind::String | TokenKind::Number
            | TokenKind::LBracket | TokenKind::Star
        )
        && is_keyword_or_identifier_for_prop(p.peek().kind);
    if is_async_method {
        p.advance(); // async
        let is_generator = p.eat(TokenKind::Star);
        let (key, computed) = parse_property_key(p)?;
        let func = parse_function_body(p, true, is_generator)?;
        return Ok(Property {
            key,
            value: func,
            kind: PropertyKindVal::Init,
            shorthand: false,
            computed,
            method: true,
            span: Span::new(start, p.pos()),
        });
    }
    if p.at(TokenKind::Star) {
        p.advance(); // *
        let (key, computed) = parse_property_key(p)?;
        let func = parse_function_body(p, false, true)?;
        return Ok(Property {
            key,
            value: func,
            kind: PropertyKindVal::Init,
            shorthand: false,
            computed,
            method: true,
            span: Span::new(start, p.pos()),
        });
    }

    let (key, computed) = parse_property_key(p)?;

    // Shorthand property: { x } -> { x: x }
    if !computed
        && matches!(&key, PropertyKey::Identifier(_))
        && (p.at(TokenKind::Comma) || p.at(TokenKind::RBrace))
    {
        let name = match &key {
            PropertyKey::Identifier(id) => *id,
            _ => unreachable!(),
        };
        return Ok(Property {
            key,
            value: Expression::Identifier(Identifier {
                name,
                span: Span::new(start, p.pos()),
            }),
            kind: PropertyKindVal::Init,
            shorthand: true,
            computed: false,
            method: false,
            span: Span::new(start, p.pos()),
        });
    }

    // Method shorthand: { foo() {} }
    if p.at(TokenKind::LParen) {
        let func = parse_function_body(p, false, false)?;
        return Ok(Property {
            key,
            value: func,
            kind: PropertyKindVal::Init,
            shorthand: false,
            computed,
            method: true,
            span: Span::new(start, p.pos()),
        });
    }

    // Regular property: { key: value }
    p.expect(TokenKind::Colon)?;
    let value = parse_expression(p, 3)?;

    Ok(Property {
        key,
        value,
        kind: PropertyKindVal::Init,
        shorthand: false,
        computed,
        method: false,
        span: Span::new(start, p.pos()),
    })
}

fn parse_property_key(p: &mut Parser) -> ParseResult<(PropertyKey, bool)> {
    match p.current_kind() {
        TokenKind::Identifier => {
            let name = p.intern_current();
            p.advance();
            Ok((PropertyKey::Identifier(name), false))
        }
        // Keywords valid as property names
        k if is_keyword_property(k) => {
            let name = p.intern_current();
            p.advance();
            Ok((PropertyKey::Identifier(name), false))
        }
        TokenKind::String => {
            let text = p.current_text().to_owned();
            p.advance();
            let value = p.parse_string_value(&text);
            Ok((PropertyKey::StringLiteral(value), false))
        }
        TokenKind::Number => {
            let text = p.current_text().to_owned();
            p.advance();
            let value = p.parse_number(&text);
            Ok((PropertyKey::NumberLiteral(value), false))
        }
        TokenKind::LBracket => {
            p.advance(); // [
            let expr = parse_expression(p, 0)?;
            p.expect(TokenKind::RBracket)?;
            Ok((PropertyKey::Computed(Box::new(expr)), true))
        }
        _ => Err(ParseError::expected(
            "property name",
            p.current_kind(),
            p.current().span,
        )),
    }
}

fn is_keyword_or_identifier_for_prop(kind: TokenKind) -> bool {
    kind == TokenKind::Identifier
        || kind == TokenKind::String
        || kind == TokenKind::Number
        || kind == TokenKind::LBracket
        || kind == TokenKind::Star
        || is_keyword_property(kind)
}

fn is_keyword_property(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Let
            | TokenKind::Const
            | TokenKind::Var
            | TokenKind::Function
            | TokenKind::Class
            | TokenKind::If
            | TokenKind::Else
            | TokenKind::For
            | TokenKind::While
            | TokenKind::Do
            | TokenKind::Return
            | TokenKind::Break
            | TokenKind::Continue
            | TokenKind::Switch
            | TokenKind::Case
            | TokenKind::Default
            | TokenKind::Try
            | TokenKind::Catch
            | TokenKind::Finally
            | TokenKind::Throw
            | TokenKind::New
            | TokenKind::Delete
            | TokenKind::Typeof
            | TokenKind::Void
            | TokenKind::In
            | TokenKind::Instanceof
            | TokenKind::This
            | TokenKind::Super
            | TokenKind::With
            | TokenKind::Yield
            | TokenKind::Await
            | TokenKind::Import
            | TokenKind::Export
            | TokenKind::Static
            | TokenKind::True
            | TokenKind::False
            | TokenKind::Null
            | TokenKind::Undefined
            | TokenKind::Of
    )
}

/// Parse a function expression: `function name?(params) { body }`
fn parse_function_expression(p: &mut Parser) -> ParseResult<Expression> {
    let start = p.pos();
    p.expect(TokenKind::Function)?;

    let is_generator = p.eat(TokenKind::Star);
    let id = if p.at(TokenKind::Identifier) {
        let name = p.intern_current();
        p.advance();
        Some(name)
    } else {
        None
    };

    let params = parse_params(p)?;
    let body = parse_block_statement(p)?;

    Ok(Expression::Function(Box::new(FunctionExpression {
        id,
        params,
        body,
        is_async: false,
        is_generator,
        span: Span::new(start, p.pos()),
    })))
}

/// Parse function parameters: `(a, b = 1, ...rest)`
pub fn parse_params(p: &mut Parser) -> ParseResult<Vec<Pattern>> {
    p.expect(TokenKind::LParen)?;
    let mut params = Vec::new();
    while !p.at(TokenKind::RParen) && !p.at(TokenKind::Eof) {
        if p.at(TokenKind::DotDotDot) {
            let rest_start = p.pos();
            p.advance();
            // Rest parameter can be an identifier, array pattern, or object pattern
            let arg = if p.at(TokenKind::LBracket) {
                super::statement::parse_array_pattern(p)?
            } else if p.at(TokenKind::LBrace) {
                super::statement::parse_object_pattern(p)?
            } else {
                let name = p.intern_current();
                let name_span = p.current().span;
                p.expect(TokenKind::Identifier)?;
                Pattern::Identifier(Identifier { name, span: name_span })
            };
            params.push(Pattern::Rest(Box::new(RestElement {
                argument: arg,
                span: Span::new(rest_start, p.pos()),
            })));
            break; // rest must be last
        }

        let param_start = p.pos();
        // Array or Object destructuring parameter
        let base = if p.at(TokenKind::LBracket) {
            super::statement::parse_array_pattern(p)?
        } else if p.at(TokenKind::LBrace) {
            super::statement::parse_object_pattern(p)?
        } else {
            let name = p.intern_current();
            let name_span = p.current().span;
            p.expect(TokenKind::Identifier)?;
            Pattern::Identifier(Identifier { name, span: name_span })
        };

        // Optional default value
        if p.eat(TokenKind::Assign) {
            let default_val = parse_expression(p, 3)?;
            let end = expr_span(&default_val).end;
            params.push(Pattern::Assignment(Box::new(AssignmentPattern {
                left: base,
                right: default_val,
                span: Span::new(param_start, end),
            })));
        } else {
            params.push(base);
        }

        if !p.at(TokenKind::RParen) {
            p.expect(TokenKind::Comma)?;
        }
    }
    p.expect(TokenKind::RParen)?;
    Ok(params)
}

/// Parse a block statement: `{ ... }`
pub fn parse_block_statement(p: &mut Parser) -> ParseResult<BlockStatement> {
    let start = p.pos();
    p.expect(TokenKind::LBrace)?;
    let mut body = Vec::new();
    while !p.at(TokenKind::RBrace) && !p.at(TokenKind::Eof) {
        body.push(super::statement::parse_statement(p)?);
    }
    p.expect(TokenKind::RBrace)?;
    Ok(BlockStatement {
        body,
        span: Span::new(start, p.pos()),
    })
}

/// Parse function body (for methods): `(params) { body }` and wrap in FunctionExpression
fn parse_function_body(p: &mut Parser, is_async: bool, is_generator: bool) -> ParseResult<Expression> {
    let start = p.pos();
    let params = parse_params(p)?;
    let body = parse_block_statement(p)?;
    Ok(Expression::Function(Box::new(FunctionExpression {
        id: None,
        params,
        body,
        is_async,
        is_generator,
        span: Span::new(start, p.pos()),
    })))
}

/// Parse arrow function body: expression or block.
fn parse_arrow_body(p: &mut Parser) -> ParseResult<ArrowBody> {
    if p.at(TokenKind::LBrace) {
        Ok(ArrowBody::Block(parse_block_statement(p)?))
    } else {
        Ok(ArrowBody::Expression(parse_expression(p, 3)?))
    }
}

fn arrow_body_end(body: &ArrowBody) -> u32 {
    match body {
        ArrowBody::Expression(e) => expr_span(e).end,
        ArrowBody::Block(b) => b.span.end,
    }
}

/// Get the span of an expression.
pub fn expr_span(expr: &Expression) -> Span {
    match expr {
        Expression::NumberLiteral(n) => n.span,
        Expression::StringLiteral(s) => s.span,
        Expression::BooleanLiteral(b) => b.span,
        Expression::NullLiteral(s) => *s,
        Expression::RegExpLiteral(r) => r.span,
        Expression::TemplateLiteral(t) => t.span,
        Expression::Identifier(i) => i.span,
        Expression::This(s) => *s,
        Expression::Array(a) => a.span,
        Expression::Object(o) => o.span,
        Expression::Function(f) => f.span,
        Expression::ArrowFunction(a) => a.span,
        Expression::Class(c) => c.span,
        Expression::Unary(u) => u.span,
        Expression::Update(u) => u.span,
        Expression::Binary(b) => b.span,
        Expression::Logical(l) => l.span,
        Expression::Conditional(c) => c.span,
        Expression::Assignment(a) => a.span,
        Expression::Sequence(s) => s.span,
        Expression::Member(m) => m.span,
        Expression::Call(c) => c.span,
        Expression::New(n) => n.span,
        Expression::TaggedTemplate(t) => t.span,
        Expression::OptionalChain(o) => o.span,
        Expression::Spread(s) => s.span,
        Expression::Yield(y) => y.span,
        Expression::Await(a) => a.span,
        Expression::MetaProperty(m) => m.span,
        Expression::Import(i) => i.span,
        Expression::Super(s) => *s,
    }
}

/// Convert an expression to an assignment target.
fn expr_to_assignment_target(expr: Expression) -> ParseResult<AssignmentTarget> {
    match expr {
        Expression::Identifier(id) => Ok(AssignmentTarget::Identifier(id)),
        Expression::Member(m) => Ok(AssignmentTarget::Member(m)),
        Expression::Array(arr) => {
            // Convert array expression to array destructuring pattern
            let mut elements = Vec::new();
            for elem in arr.elements {
                match elem {
                    Some(Expression::Identifier(id)) => elements.push(Some(Pattern::Identifier(id))),
                    Some(Expression::Assignment(a)) => {
                        if let AssignmentTarget::Identifier(id) = a.left {
                            elements.push(Some(Pattern::Assignment(Box::new(AssignmentPattern {
                                left: Pattern::Identifier(id),
                                right: a.right,
                                span: a.span,
                            }))));
                        } else {
                            elements.push(None);
                        }
                    }
                    Some(Expression::Spread(s)) => {
                        if let Expression::Identifier(id) = s.argument {
                            elements.push(Some(Pattern::Rest(Box::new(RestElement {
                                argument: Pattern::Identifier(id),
                                span: s.span,
                            }))));
                        } else {
                            elements.push(None);
                        }
                    }
                    None => elements.push(None),
                    _ => elements.push(None),
                }
            }
            Ok(AssignmentTarget::Pattern(Pattern::Array(ArrayPattern {
                elements,
                span: arr.span,
            })))
        }
        Expression::Object(obj) => {
            // Convert object expression to object destructuring pattern
            let mut properties = Vec::new();
            for prop in obj.properties {
                if let ObjectProperty::Property(p) = prop
                    && let Expression::Identifier(id) = p.value {
                        let key = match p.key {
                            PropertyKey::Identifier(s) | PropertyKey::StringLiteral(s) => s,
                            _ => continue,
                        };
                        properties.push(ObjectPatternProperty::Property {
                            key: PropertyKey::Identifier(key),
                            value: Pattern::Identifier(id),
                            computed: false,
                            shorthand: p.shorthand,
                            span: p.span,
                        });
                    }
            }
            Ok(AssignmentTarget::Pattern(Pattern::Object(ObjectPattern {
                properties,
                span: obj.span,
            })))
        }
        _ => {
            let span = expr_span(&expr);
            Err(ParseError::new("Invalid assignment target", span))
        }
    }
}

/// Convert a parenthesized expression to arrow function params.
fn expr_to_params(expr: Expression) -> ParseResult<Vec<Pattern>> {
    match expr {
        Expression::Identifier(id) => Ok(vec![Pattern::Identifier(id)]),
        Expression::Sequence(seq) => {
            let mut params = Vec::new();
            for e in seq.expressions {
                params.push(expr_to_param(e)?);
            }
            Ok(params)
        }
        Expression::Assignment(a) => {
            let left = match a.left {
                AssignmentTarget::Identifier(id) => Pattern::Identifier(id),
                _ => {
                    return Err(ParseError::new("Invalid parameter", a.span));
                }
            };
            Ok(vec![Pattern::Assignment(Box::new(AssignmentPattern {
                left,
                right: a.right,
                span: a.span,
            }))])
        }
        other => Ok(vec![expr_to_param(other)?]),
    }
}

fn expr_to_param(expr: Expression) -> ParseResult<Pattern> {
    match expr {
        Expression::Identifier(id) => Ok(Pattern::Identifier(id)),
        Expression::Assignment(a) => {
            let left = match a.left {
                AssignmentTarget::Identifier(id) => Pattern::Identifier(id),
                AssignmentTarget::Pattern(p) => p,
                _ => {
                    return Err(ParseError::new("Invalid parameter", a.span));
                }
            };
            Ok(Pattern::Assignment(Box::new(AssignmentPattern {
                left,
                right: a.right,
                span: a.span,
            })))
        }
        Expression::Spread(s) => {
            let param = expr_to_param(s.argument)?;
            Ok(Pattern::Rest(Box::new(RestElement {
                argument: param,
                span: s.span,
            })))
        }
        Expression::Array(a) => array_expr_to_pattern(a),
        Expression::Object(o) => object_expr_to_pattern(o),
        _ => {
            let span = expr_span(&expr);
            Err(ParseError::new("Invalid parameter", span))
        }
    }
}

fn array_expr_to_pattern(arr: ArrayExpression) -> ParseResult<Pattern> {
    let mut elements = Vec::new();
    for e_opt in arr.elements {
        if let Some(e) = e_opt {
            match e {
                Expression::Spread(s) => {
                    let inner = expr_to_param(s.argument)?;
                    elements.push(Some(Pattern::Rest(Box::new(RestElement {
                        argument: inner,
                        span: s.span,
                    }))));
                }
                other => elements.push(Some(expr_to_param(other)?)),
            }
        } else {
            elements.push(None);
        }
    }
    Ok(Pattern::Array(ArrayPattern { elements, span: arr.span }))
}

fn object_expr_to_pattern(o: ObjectExpression) -> ParseResult<Pattern> {
    let mut props = Vec::new();
    for prop in o.properties {
        if let ObjectProperty::Property(p) = prop {
            let value = expr_to_param(p.value)?;
            props.push(ObjectPatternProperty::Property {
                key: p.key,
                value,
                computed: p.computed,
                shorthand: p.shorthand,
                span: p.span,
            });
        }
    }
    Ok(Pattern::Object(ObjectPattern { properties: props, span: o.span }))
}

/// Parse a property key in class body (re-exported for statement.rs).
pub fn parse_property_key_for_class(p: &mut Parser) -> ParseResult<(PropertyKey, bool)> {
    parse_property_key(p)
}

/// Parse function body and return as Expression (for class methods).
pub fn parse_function_body_expr(
    p: &mut Parser,
    is_async: bool,
    is_generator: bool,
) -> ParseResult<Expression> {
    parse_function_body(p, is_async, is_generator)
}

/// Parse a template literal starting at the current TemplateLiteralFull or TemplateLiteralHead token.
/// Returns the TemplateLiteral AST node.
pub fn parse_template_literal(p: &mut Parser) -> ParseResult<TemplateLiteral> {
    if p.at(TokenKind::TemplateLiteralFull) {
        let text = p.current_text().to_owned();
        let span = p.current().span;
        p.advance();
        let raw = p.interner.intern(&text[1..text.len() - 1]);
        return Ok(TemplateLiteral {
            quasis: vec![TemplateElement { raw, cooked: Some(raw), tail: true, span }],
            expressions: vec![],
            span,
        });
    }
    // TemplateLiteralHead
    let head_text = p.current_text().to_owned();
    let start_span = p.current().span;
    p.advance();
    let head_str = &head_text[1..head_text.len() - 2];
    let head_id = p.interner.intern(head_str);
    let mut quasis = vec![TemplateElement { raw: head_id, cooked: Some(head_id), tail: false, span: start_span }];
    let mut expressions = Vec::new();
    loop {
        let expr = parse_expression(p, 0)?;
        expressions.push(expr);
        match p.current_kind() {
            TokenKind::TemplateLiteralMiddle => {
                let text = p.current_text().to_owned();
                let span = p.current().span;
                p.advance();
                let mid_str = &text[1..text.len() - 2];
                let mid_id = p.interner.intern(mid_str);
                quasis.push(TemplateElement { raw: mid_id, cooked: Some(mid_id), tail: false, span });
            }
            TokenKind::TemplateLiteralTail => {
                let text = p.current_text().to_owned();
                let span = p.current().span;
                p.advance();
                let tail_str = &text[1..text.len() - 1];
                let tail_id = p.interner.intern(tail_str);
                quasis.push(TemplateElement { raw: tail_id, cooked: Some(tail_id), tail: true, span });
                break;
            }
            _ => return Err(ParseError::expected("template continuation", p.current_kind(), p.current().span)),
        }
    }
    let end = p.pos();
    Ok(TemplateLiteral { quasis, expressions, span: Span::new(start_span.start, end) })
}
