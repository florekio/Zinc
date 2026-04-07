use crate::ast::node::*;
use crate::ast::span::Span;
use crate::lexer::token::TokenKind;

use super::error::{ParseError, ParseResult};
use super::expression::{self, parse_block_statement, parse_expression, parse_params};
use super::parser::Parser;

/// Parse a statement or declaration.
pub fn parse_statement(p: &mut Parser) -> ParseResult<Statement> {
    match p.current_kind() {
        TokenKind::LBrace => {
            let block = parse_block_statement(p)?;
            Ok(Statement::Block(block))
        }
        TokenKind::Var | TokenKind::Let | TokenKind::Const => parse_variable_declaration(p),
        TokenKind::Semicolon => {
            let span = p.current().span;
            p.advance();
            Ok(Statement::Empty(span))
        }
        TokenKind::If => parse_if(p),
        TokenKind::While => parse_while(p),
        TokenKind::Do => parse_do_while(p),
        TokenKind::For => parse_for(p),
        TokenKind::Switch => parse_switch(p),
        TokenKind::Return => parse_return(p),
        TokenKind::Break => parse_break(p),
        TokenKind::Continue => parse_continue(p),
        TokenKind::Throw => parse_throw(p),
        TokenKind::Try => parse_try(p),
        TokenKind::Function => parse_function_declaration(p, false),
        TokenKind::Identifier if p.current_text() == "async" && p.peek().kind == TokenKind::Function => {
            p.advance(); // consume 'async'
            parse_function_declaration(p, true)
        }
        TokenKind::Class => parse_class_declaration(p),
        TokenKind::Debugger => {
            let span = p.current().span;
            p.advance();
            p.expect_semicolon()?;
            Ok(Statement::Debugger(span))
        }
        _ => parse_expression_statement(p),
    }
}

fn parse_variable_declaration(p: &mut Parser) -> ParseResult<Statement> {
    let start = p.pos();
    let kind = match p.current_kind() {
        TokenKind::Var => VarKind::Var,
        TokenKind::Let => VarKind::Let,
        TokenKind::Const => VarKind::Const,
        _ => unreachable!(),
    };
    p.advance();

    let mut declarations = Vec::new();
    loop {
        let decl_start = p.pos();

        let id = if p.at(TokenKind::LBrace) {
            // Object destructuring pattern: { a, b, c }
            parse_object_pattern(p)?
        } else if p.at(TokenKind::LBracket) {
            // Array destructuring pattern: [ a, b, c ]
            parse_array_pattern(p)?
        } else {
            let name = p.intern_current();
            let name_span = p.current().span;
            p.expect(TokenKind::Identifier)?;
            Pattern::Identifier(Identifier { name, span: name_span })
        };

        let init = if p.eat(TokenKind::Assign) {
            Some(parse_expression(p, 3)?)
        } else {
            None
        };

        declarations.push(VariableDeclarator {
            id,
            init,
            span: Span::new(decl_start, p.pos()),
        });

        if !p.eat(TokenKind::Comma) {
            break;
        }
    }

    p.expect_semicolon()?;
    Ok(Statement::Variable(VariableDeclaration {
        kind,
        declarations,
        span: Span::new(start, p.pos()),
    }))
}

fn parse_if(p: &mut Parser) -> ParseResult<Statement> {
    let start = p.pos();
    p.expect(TokenKind::If)?;
    p.expect(TokenKind::LParen)?;
    let test = parse_expression(p, 0)?;
    p.expect(TokenKind::RParen)?;
    let consequent = parse_statement(p)?;
    let alternate = if p.eat(TokenKind::Else) {
        Some(parse_statement(p)?)
    } else {
        None
    };
    Ok(Statement::If(Box::new(IfStatement {
        test,
        consequent,
        alternate,
        span: Span::new(start, p.pos()),
    })))
}

fn parse_while(p: &mut Parser) -> ParseResult<Statement> {
    let start = p.pos();
    p.expect(TokenKind::While)?;
    p.expect(TokenKind::LParen)?;
    let test = parse_expression(p, 0)?;
    p.expect(TokenKind::RParen)?;
    let body = parse_statement(p)?;
    Ok(Statement::While(Box::new(WhileStatement {
        test,
        body,
        span: Span::new(start, p.pos()),
    })))
}

fn parse_do_while(p: &mut Parser) -> ParseResult<Statement> {
    let start = p.pos();
    p.expect(TokenKind::Do)?;
    let body = parse_statement(p)?;
    p.expect(TokenKind::While)?;
    p.expect(TokenKind::LParen)?;
    let test = parse_expression(p, 0)?;
    p.expect(TokenKind::RParen)?;
    p.expect_semicolon()?;
    Ok(Statement::DoWhile(Box::new(DoWhileStatement {
        body,
        test,
        span: Span::new(start, p.pos()),
    })))
}

fn parse_for(p: &mut Parser) -> ParseResult<Statement> {
    let start = p.pos();
    p.expect(TokenKind::For)?;
    p.expect(TokenKind::LParen)?;

    // for (init; test; update) or for (left in right) or for (left of right)
    let init = if p.at(TokenKind::Semicolon) {
        None
    } else if p.at_any(&[TokenKind::Var, TokenKind::Let, TokenKind::Const]) {
        let var_start = p.pos();
        let kind = match p.current_kind() {
            TokenKind::Var => VarKind::Var,
            TokenKind::Let => VarKind::Let,
            TokenKind::Const => VarKind::Const,
            _ => unreachable!(),
        };
        p.advance();
        let name = p.intern_current();
        let name_span = p.current().span;
        p.expect(TokenKind::Identifier)?;

        // for-in / for-of
        if p.at(TokenKind::In) {
            p.advance();
            let right = parse_expression(p, 0)?;
            p.expect(TokenKind::RParen)?;
            let body = parse_statement(p)?;
            let decl = VariableDeclaration {
                kind,
                declarations: vec![VariableDeclarator {
                    id: Pattern::Identifier(Identifier { name, span: name_span }),
                    init: None,
                    span: name_span,
                }],
                span: Span::new(var_start, name_span.end),
            };
            return Ok(Statement::ForIn(Box::new(ForInStatement {
                left: ForInOfLeft::Variable(decl),
                right,
                body,
                span: Span::new(start, p.pos()),
            })));
        }
        if p.at(TokenKind::Of) || (p.at(TokenKind::Identifier) && p.current_text() == "of") {
            p.advance();
            let right = parse_expression(p, 3)?;
            p.expect(TokenKind::RParen)?;
            let body = parse_statement(p)?;
            let decl = VariableDeclaration {
                kind,
                declarations: vec![VariableDeclarator {
                    id: Pattern::Identifier(Identifier { name, span: name_span }),
                    init: None,
                    span: name_span,
                }],
                span: Span::new(var_start, name_span.end),
            };
            return Ok(Statement::ForOf(Box::new(ForOfStatement {
                left: ForInOfLeft::Variable(decl),
                right,
                body,
                is_await: false,
                span: Span::new(start, p.pos()),
            })));
        }

        // Regular for
        let init_expr = if p.eat(TokenKind::Assign) {
            Some(parse_expression(p, 3)?)
        } else {
            None
        };
        let mut declarations = vec![VariableDeclarator {
            id: Pattern::Identifier(Identifier { name, span: name_span }),
            init: init_expr,
            span: Span::new(var_start, p.pos()),
        }];
        while p.eat(TokenKind::Comma) {
            let d_start = p.pos();
            let d_name = p.intern_current();
            let d_span = p.current().span;
            p.expect(TokenKind::Identifier)?;
            let d_init = if p.eat(TokenKind::Assign) {
                Some(parse_expression(p, 3)?)
            } else {
                None
            };
            declarations.push(VariableDeclarator {
                id: Pattern::Identifier(Identifier { name: d_name, span: d_span }),
                init: d_init,
                span: Span::new(d_start, p.pos()),
            });
        }
        Some(ForInit::Variable(VariableDeclaration {
            kind,
            declarations,
            span: Span::new(var_start, p.pos()),
        }))
    } else if p.at(TokenKind::Identifier) && (p.peek().kind == TokenKind::In || (p.peek().kind == TokenKind::Identifier && p.token_text(p.peek()) == "of")) {
        // for (x in obj) or for (x of iterable) — x already declared
        let name = p.intern_current();
        let name_span = p.current().span;
        p.advance(); // consume identifier
        let is_of = p.current_kind() != TokenKind::In;
        p.advance(); // consume 'in' or 'of'
        let right = parse_expression(p, 0)?;
        p.expect(TokenKind::RParen)?;
        let body = parse_statement(p)?;
        let id = Pattern::Identifier(Identifier { name, span: name_span });
        if is_of {
            return Ok(Statement::ForOf(Box::new(ForOfStatement {
                left: ForInOfLeft::Pattern(id), right, body, is_await: false,
                span: Span::new(start, p.pos()),
            })));
        } else {
            return Ok(Statement::ForIn(Box::new(ForInStatement {
                left: ForInOfLeft::Pattern(id), right, body,
                span: Span::new(start, p.pos()),
            })));
        }
    } else {
        Some(ForInit::Expression(parse_expression(p, 0)?))
    };

    p.expect(TokenKind::Semicolon)?;
    let test = if p.at(TokenKind::Semicolon) {
        None
    } else {
        Some(parse_expression(p, 0)?)
    };
    p.expect(TokenKind::Semicolon)?;
    let update = if p.at(TokenKind::RParen) {
        None
    } else {
        Some(parse_expression(p, 0)?)
    };
    p.expect(TokenKind::RParen)?;
    let body = parse_statement(p)?;

    Ok(Statement::For(Box::new(ForStatement {
        init,
        test,
        update,
        body,
        span: Span::new(start, p.pos()),
    })))
}

fn parse_switch(p: &mut Parser) -> ParseResult<Statement> {
    let start = p.pos();
    p.expect(TokenKind::Switch)?;
    p.expect(TokenKind::LParen)?;
    let discriminant = parse_expression(p, 0)?;
    p.expect(TokenKind::RParen)?;
    p.expect(TokenKind::LBrace)?;

    let mut cases = Vec::new();
    while !p.at(TokenKind::RBrace) && !p.at(TokenKind::Eof) {
        let case_start = p.pos();
        let test = if p.eat(TokenKind::Case) {
            Some(parse_expression(p, 0)?)
        } else {
            p.expect(TokenKind::Default)?;
            None
        };
        p.expect(TokenKind::Colon)?;

        let mut consequent = Vec::new();
        while !p.at_any(&[TokenKind::Case, TokenKind::Default, TokenKind::RBrace, TokenKind::Eof]) {
            consequent.push(parse_statement(p)?);
        }
        cases.push(SwitchCase {
            test,
            consequent,
            span: Span::new(case_start, p.pos()),
        });
    }
    p.expect(TokenKind::RBrace)?;

    Ok(Statement::Switch(Box::new(SwitchStatement {
        discriminant,
        cases,
        span: Span::new(start, p.pos()),
    })))
}

fn parse_return(p: &mut Parser) -> ParseResult<Statement> {
    let start = p.pos();
    p.expect(TokenKind::Return)?;
    let argument = if p.at(TokenKind::Semicolon)
        || p.at(TokenKind::RBrace)
        || p.at(TokenKind::Eof)
        || p.preceded_by_newline()
    {
        None
    } else {
        Some(parse_expression(p, 0)?)
    };
    p.expect_semicolon()?;
    Ok(Statement::Return(ReturnStatement {
        argument,
        span: Span::new(start, p.pos()),
    }))
}

fn parse_break(p: &mut Parser) -> ParseResult<Statement> {
    let start = p.pos();
    p.expect(TokenKind::Break)?;
    let label = if !p.preceded_by_newline() && p.at(TokenKind::Identifier) {
        let name = p.intern_current();
        p.advance();
        Some(name)
    } else {
        None
    };
    p.expect_semicolon()?;
    Ok(Statement::Break(BreakStatement {
        label,
        span: Span::new(start, p.pos()),
    }))
}

fn parse_continue(p: &mut Parser) -> ParseResult<Statement> {
    let start = p.pos();
    p.expect(TokenKind::Continue)?;
    let label = if !p.preceded_by_newline() && p.at(TokenKind::Identifier) {
        let name = p.intern_current();
        p.advance();
        Some(name)
    } else {
        None
    };
    p.expect_semicolon()?;
    Ok(Statement::Continue(ContinueStatement {
        label,
        span: Span::new(start, p.pos()),
    }))
}

fn parse_throw(p: &mut Parser) -> ParseResult<Statement> {
    let start = p.pos();
    p.expect(TokenKind::Throw)?;
    if p.preceded_by_newline() {
        return Err(ParseError::new(
            "No line break allowed after 'throw'",
            p.current().span,
        ));
    }
    let argument = parse_expression(p, 0)?;
    p.expect_semicolon()?;
    Ok(Statement::Throw(ThrowStatement {
        argument,
        span: Span::new(start, p.pos()),
    }))
}

fn parse_try(p: &mut Parser) -> ParseResult<Statement> {
    let start = p.pos();
    p.expect(TokenKind::Try)?;
    let block = parse_block_statement(p)?;

    let handler = if p.eat(TokenKind::Catch) {
        let catch_start = p.pos();
        let param = if p.eat(TokenKind::LParen) {
            let name = p.intern_current();
            let name_span = p.current().span;
            p.expect(TokenKind::Identifier)?;
            p.expect(TokenKind::RParen)?;
            Some(Pattern::Identifier(Identifier { name, span: name_span }))
        } else {
            None
        };
        let body = parse_block_statement(p)?;
        Some(CatchClause {
            param,
            body,
            span: Span::new(catch_start, p.pos()),
        })
    } else {
        None
    };

    let finalizer = if p.eat(TokenKind::Finally) {
        Some(parse_block_statement(p)?)
    } else {
        None
    };

    if handler.is_none() && finalizer.is_none() {
        return Err(ParseError::new(
            "try must have catch or finally",
            Span::new(start, p.pos()),
        ));
    }

    Ok(Statement::Try(Box::new(TryStatement {
        block,
        handler,
        finalizer,
        span: Span::new(start, p.pos()),
    })))
}

fn parse_function_declaration(p: &mut Parser, is_async: bool) -> ParseResult<Statement> {
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

    Ok(Statement::Function(FunctionDeclaration {
        id,
        params,
        body,
        is_async,
        is_generator,
        span: Span::new(start, p.pos()),
    }))
}

fn parse_class_declaration(p: &mut Parser) -> ParseResult<Statement> {
    let start = p.pos();
    p.expect(TokenKind::Class)?;

    let id = if p.at(TokenKind::Identifier) {
        let name = p.intern_current();
        p.advance();
        Some(name)
    } else {
        None
    };

    let super_class = if p.eat(TokenKind::Extends) {
        Some(parse_expression(p, 0)?)
    } else {
        None
    };

    let body = parse_class_body(p)?;

    Ok(Statement::Class(ClassDeclaration {
        id,
        super_class,
        body,
        span: Span::new(start, p.pos()),
    }))
}

fn parse_class_body(p: &mut Parser) -> ParseResult<ClassBody> {
    let start = p.pos();
    p.expect(TokenKind::LBrace)?;
    let mut body = Vec::new();

    while !p.at(TokenKind::RBrace) && !p.at(TokenKind::Eof) {
        // Skip semicolons in class body
        if p.eat(TokenKind::Semicolon) {
            continue;
        }

        let member_start = p.pos();
        let is_static = p.at(TokenKind::Static)
            && !matches!(p.peek().kind, TokenKind::LParen | TokenKind::Assign);
        if is_static {
            p.advance();
        }

        // Check for getter/setter
        let text = p.current_text().to_owned();
        if (text == "get" || text == "set") && p.current_kind() == TokenKind::Identifier {
            let next = p.peek().kind;
            if next != TokenKind::LParen {
                let method_kind = if text == "get" {
                    MethodKind::Get
                } else {
                    MethodKind::Set
                };
                p.advance(); // get/set
                let (key, computed) = expression::parse_property_key_for_class(p)?;
                let value = expression::parse_function_body_expr(p, false, false)?;
                body.push(ClassMember::Method(MethodDefinition {
                    key,
                    value,
                    kind: method_kind,
                    is_static,
                    computed,
                    span: Span::new(member_start, p.pos()),
                }));
                continue;
            }
        }

        let (key, computed) = expression::parse_property_key_for_class(p)?;

        // Constructor or method
        if p.at(TokenKind::LParen) {
            let is_constructor = !computed
                && matches!(&key, PropertyKey::Identifier(id) if p.interner.resolve(*id) == "constructor");
            let method_kind = if is_constructor {
                MethodKind::Constructor
            } else {
                MethodKind::Method
            };
            let value = expression::parse_function_body_expr(p, false, false)?;
            body.push(ClassMember::Method(MethodDefinition {
                key,
                value,
                kind: method_kind,
                is_static,
                computed,
                span: Span::new(member_start, p.pos()),
            }));
        } else {
            // Class field
            let value = if p.eat(TokenKind::Assign) {
                Some(parse_expression(p, 3)?)
            } else {
                None
            };
            p.expect_semicolon()?;
            body.push(ClassMember::Property(ClassProperty {
                key,
                value,
                is_static,
                computed,
                span: Span::new(member_start, p.pos()),
            }));
        }
    }
    p.expect(TokenKind::RBrace)?;

    Ok(ClassBody {
        body,
        span: Span::new(start, p.pos()),
    })
}

fn parse_expression_statement(p: &mut Parser) -> ParseResult<Statement> {
    let start = p.pos();
    let expr = parse_expression(p, 0)?;
    p.expect_semicolon()?;
    let end = p.pos();
    Ok(Statement::Expression(ExpressionStatement {
        expression: expr,
        span: Span::new(start, end),
    }))
}

/// Parse an object destructuring pattern: { a, b, c } or { a: x, b: y }
fn parse_object_pattern(p: &mut Parser) -> ParseResult<Pattern> {
    let start = p.pos();
    p.expect(TokenKind::LBrace)?;
    let mut properties = Vec::new();

    while !p.at(TokenKind::RBrace) && !p.at(TokenKind::Eof) {
        if p.at(TokenKind::DotDotDot) {
            let rest_start = p.pos();
            p.advance();
            let name = p.intern_current();
            let name_span = p.current().span;
            p.expect(TokenKind::Identifier)?;
            properties.push(ObjectPatternProperty::Rest(RestElement {
                argument: Pattern::Identifier(Identifier { name, span: name_span }),
                span: Span::new(rest_start, p.pos()),
            }));
            break;
        }

        let prop_start = p.pos();
        let key_name = p.intern_current();
        let key_span = p.current().span;
        p.advance(); // consume identifier (or keyword used as property)

        if p.eat(TokenKind::Colon) {
            // Renamed: { a: x }
            let value_name = p.intern_current();
            let value_span = p.current().span;
            p.expect(TokenKind::Identifier)?;
            properties.push(ObjectPatternProperty::Property {
                key: PropertyKey::Identifier(key_name),
                value: Pattern::Identifier(Identifier { name: value_name, span: value_span }),
                computed: false,
                shorthand: false,
                span: Span::new(prop_start, p.pos()),
            });
        } else {
            // Shorthand: { a } means { a: a }
            properties.push(ObjectPatternProperty::Property {
                key: PropertyKey::Identifier(key_name),
                value: Pattern::Identifier(Identifier { name: key_name, span: key_span }),
                computed: false,
                shorthand: true,
                span: Span::new(prop_start, p.pos()),
            });
        }

        if !p.at(TokenKind::RBrace) {
            p.expect(TokenKind::Comma)?;
        }
    }
    p.expect(TokenKind::RBrace)?;

    Ok(Pattern::Object(ObjectPattern {
        properties,
        span: Span::new(start, p.pos()),
    }))
}

/// Parse an array destructuring pattern: [ a, b, c ]
fn parse_array_pattern(p: &mut Parser) -> ParseResult<Pattern> {
    let start = p.pos();
    p.expect(TokenKind::LBracket)?;
    let mut elements = Vec::new();

    while !p.at(TokenKind::RBracket) && !p.at(TokenKind::Eof) {
        if p.at(TokenKind::Comma) {
            // Hole: [, , x]
            elements.push(None);
            p.advance();
            continue;
        }
        if p.at(TokenKind::DotDotDot) {
            let rest_start = p.pos();
            p.advance();
            let name = p.intern_current();
            let name_span = p.current().span;
            p.expect(TokenKind::Identifier)?;
            elements.push(Some(Pattern::Rest(Box::new(RestElement {
                argument: Pattern::Identifier(Identifier { name, span: name_span }),
                span: Span::new(rest_start, p.pos()),
            }))));
            break;
        }
        let name = p.intern_current();
        let name_span = p.current().span;
        p.expect(TokenKind::Identifier)?;
        elements.push(Some(Pattern::Identifier(Identifier { name, span: name_span })));

        if !p.at(TokenKind::RBracket) {
            p.expect(TokenKind::Comma)?;
        }
    }
    p.expect(TokenKind::RBracket)?;

    Ok(Pattern::Array(ArrayPattern {
        elements,
        span: Span::new(start, p.pos()),
    }))
}
