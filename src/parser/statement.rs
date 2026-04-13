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
        // Check for labeled statement: `identifier : statement`
        TokenKind::Identifier if p.peek().kind == TokenKind::Colon => {
            let start = p.pos();
            let label = p.intern_current();
            p.advance(); // identifier
            p.advance(); // colon
            let body = parse_statement(p)?;
            Ok(Statement::Labeled(Box::new(LabeledStatement {
                label,
                body,
                span: Span::new(start, p.pos()),
            })))
        }
        TokenKind::Import => parse_import_declaration(p),
        TokenKind::Export => parse_export_declaration(p),
        _ => parse_expression_statement(p),
    }
}

fn parse_import_declaration(p: &mut Parser) -> ParseResult<Statement> {
    let start = p.pos();
    p.advance(); // consume 'import'

    // import 'module' (side-effect only)
    if p.at(TokenKind::String) {
        let source = p.intern_current();
        let _source_span = p.current().span;
        p.advance();
        p.expect_semicolon()?;
        return Ok(Statement::Import(ImportDeclaration::Standard {
            specifiers: Vec::new(),
            source,
            span: Span::new(start, p.pos()),
        }));
    }

    let mut specifiers = Vec::new();

    // import defaultExport from 'module'
    if p.at(TokenKind::Identifier) && p.peek().kind != TokenKind::Comma && p.current_text() != "from" {
        let local = p.intern_current();
        let span = p.current().span;
        // Check if this is `import x from` (default) or `import { ... } from`
        if p.peek().kind == TokenKind::LBrace || (p.peek().kind == TokenKind::Identifier && p.token_text(p.peek()) == "from") {
            p.advance();
            specifiers.push(ImportSpecifier::Default { local, span });
            // May have additional named imports: import x, { a, b } from 'mod'
            if p.eat(TokenKind::Comma) {
                if p.at(TokenKind::LBrace) {
                    parse_named_imports(p, &mut specifiers)?;
                } else if p.at(TokenKind::Star) {
                    parse_namespace_import(p, &mut specifiers)?;
                }
            }
        } else {
            p.advance();
            specifiers.push(ImportSpecifier::Default { local, span });
        }
    } else if p.at(TokenKind::LBrace) {
        // import { a, b as c } from 'module'
        parse_named_imports(p, &mut specifiers)?;
    } else if p.at(TokenKind::Star) {
        // import * as ns from 'module'
        parse_namespace_import(p, &mut specifiers)?;
    }

    // expect 'from'
    if p.at(TokenKind::Identifier) && p.current_text() == "from" {
        p.advance();
    }

    // expect module source string
    let source = p.intern_current();
    p.expect(TokenKind::String)?;
    p.expect_semicolon()?;

    Ok(Statement::Import(ImportDeclaration::Standard {
        specifiers,
        source,
        span: Span::new(start, p.pos()),
    }))
}

fn parse_named_imports(p: &mut Parser, specifiers: &mut Vec<ImportSpecifier>) -> ParseResult<()> {
    p.expect(TokenKind::LBrace)?;
    while !p.at(TokenKind::RBrace) && !p.at(TokenKind::Eof) {
        let imported = p.intern_current();
        let span = p.current().span;
        p.advance();
        let local = if p.at(TokenKind::Identifier) && p.current_text() == "as" {
            p.advance(); // 'as'
            let l = p.intern_current();
            p.advance();
            l
        } else {
            imported
        };
        specifiers.push(ImportSpecifier::Named { imported, local, span });
        if !p.at(TokenKind::RBrace) {
            p.expect(TokenKind::Comma)?;
        }
    }
    p.expect(TokenKind::RBrace)?;
    Ok(())
}

fn parse_namespace_import(p: &mut Parser, specifiers: &mut Vec<ImportSpecifier>) -> ParseResult<()> {
    let span = p.current().span;
    p.advance(); // '*'
    if p.at(TokenKind::Identifier) && p.current_text() == "as" {
        p.advance(); // 'as'
    }
    let local = p.intern_current();
    p.advance();
    specifiers.push(ImportSpecifier::Namespace { local, span });
    Ok(())
}

fn parse_export_declaration(p: &mut Parser) -> ParseResult<Statement> {
    let start = p.pos();
    p.advance(); // consume 'export'

    // export default expr
    if p.at(TokenKind::Default) {
        p.advance();
        let expr = if p.at(TokenKind::Function) {
            // Parse as function declaration, then convert to FunctionExpression
            let func_start = p.pos();
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
            Expression::Function(Box::new(FunctionExpression {
                id,
                params,
                body,
                is_async: false,
                is_generator,
                span: Span::new(func_start, p.pos()),
            }))
        } else if p.at(TokenKind::Class) {
            // Parse as class declaration, then convert to ClassExpression
            let class_start = p.pos();
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
            Expression::Class(Box::new(ClassExpression {
                id,
                super_class,
                body,
                span: Span::new(class_start, p.pos()),
            }))
        } else {
            let expr = parse_expression(p, 0)?;
            p.expect_semicolon()?;
            expr
        };
        return Ok(Statement::Export(Box::new(ExportDeclaration::Default {
            declaration: expr,
            span: Span::new(start, p.pos()),
        })));
    }

    // export function/var/let/const/class
    if p.at_any(&[TokenKind::Function, TokenKind::Var, TokenKind::Let, TokenKind::Const, TokenKind::Class]) {
        let decl = match p.current_kind() {
            TokenKind::Function => parse_function_declaration(p, false)?,
            TokenKind::Class => parse_class_declaration(p)?,
            _ => parse_variable_declaration(p)?,
        };
        return Ok(Statement::Export(Box::new(ExportDeclaration::Declaration {
            declaration: Box::new(decl),
            span: Span::new(start, p.pos()),
        })));
    }

    // export { a, b } or export { a, b } from 'mod'
    if p.at(TokenKind::LBrace) {
        p.advance();
        let mut specifiers = Vec::new();
        while !p.at(TokenKind::RBrace) && !p.at(TokenKind::Eof) {
            let local = p.intern_current();
            let local_span = p.current().span;
            p.advance();
            let exported = if p.at(TokenKind::Identifier) && p.current_text() == "as" {
                p.advance();
                let e = p.intern_current();
                p.advance();
                e
            } else {
                local
            };
            specifiers.push(ExportSpecifier {
                local,
                exported,
                span: local_span,
            });
            if !p.at(TokenKind::RBrace) {
                p.expect(TokenKind::Comma)?;
            }
        }
        p.expect(TokenKind::RBrace)?;
        let source = if p.at(TokenKind::Identifier) && p.current_text() == "from" {
            p.advance();
            let s = Some(p.intern_current());
            p.expect(TokenKind::String)?;
            s
        } else {
            None
        };
        p.expect_semicolon()?;
        return Ok(Statement::Export(Box::new(ExportDeclaration::Named {
            specifiers,
            source,
            span: Span::new(start, p.pos()),
        })));
    }

    // export * from 'mod'
    if p.at(TokenKind::Star) {
        p.advance();
        let exported = if p.at(TokenKind::Identifier) && p.current_text() == "as" {
            p.advance();
            let e = p.intern_current();
            p.advance();
            Some(e)
        } else {
            None
        };
        if p.at(TokenKind::Identifier) && p.current_text() == "from" {
            p.advance();
        }
        let source = p.intern_current();
        p.expect(TokenKind::String)?;
        p.expect_semicolon()?;
        return Ok(Statement::Export(Box::new(ExportDeclaration::All {
            source,
            exported,
            span: Span::new(start, p.pos()),
        })));
    }

    Err(ParseError::unexpected(p.current().kind, p.current().span))
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

        // Check for destructuring pattern in for-in/for-of: for (var [a,b] of ...)
        if p.at(TokenKind::LBracket) || p.at(TokenKind::LBrace) {
            let pat_start = p.pos();
            let pattern = if p.at(TokenKind::LBracket) {
                parse_array_pattern(p)?
            } else {
                parse_object_pattern(p)?
            };
            let pat_end = p.pos();
            // Must be for-in or for-of
            if p.at(TokenKind::In) {
                p.advance();
                let right = parse_expression(p, 0)?;
                p.expect(TokenKind::RParen)?;
                let body = parse_statement(p)?;
                let decl = VariableDeclaration {
                    kind,
                    declarations: vec![VariableDeclarator {
                        id: pattern,
                        init: None,
                        span: Span::new(pat_start, pat_end),
                    }],
                    span: Span::new(var_start, pat_end),
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
                        id: pattern,
                        init: None,
                        span: Span::new(pat_start, pat_end),
                    }],
                    span: Span::new(var_start, pat_end),
                };
                return Ok(Statement::ForOf(Box::new(ForOfStatement {
                    left: ForInOfLeft::Variable(decl),
                    right,
                    body,
                    is_await: false,
                    span: Span::new(start, p.pos()),
                })));
            }
            // Regular for: for (const [a,b] = init; test; update) body
            if p.at(TokenKind::Assign) {
                p.advance();
                let init_expr = parse_expression(p, 3)?;
                let mut declarations = vec![VariableDeclarator {
                    id: pattern,
                    init: Some(init_expr),
                    span: Span::new(pat_start, p.pos()),
                }];
                // Optional further declarators
                while p.eat(TokenKind::Comma) {
                    let d_start = p.pos();
                    let d_pat = if p.at(TokenKind::LBracket) {
                        parse_array_pattern(p)?
                    } else if p.at(TokenKind::LBrace) {
                        parse_object_pattern(p)?
                    } else {
                        let d_name = p.intern_current();
                        let d_span = p.current().span;
                        p.expect(TokenKind::Identifier)?;
                        Pattern::Identifier(Identifier { name: d_name, span: d_span })
                    };
                    let d_init = if p.eat(TokenKind::Assign) {
                        Some(parse_expression(p, 3)?)
                    } else { None };
                    declarations.push(VariableDeclarator {
                        id: d_pat,
                        init: d_init,
                        span: Span::new(d_start, p.pos()),
                    });
                }
                let decl = VariableDeclaration {
                    kind,
                    declarations,
                    span: Span::new(var_start, p.pos()),
                };
                p.expect(TokenKind::Semicolon)?;
                let test = if p.at(TokenKind::Semicolon) { None } else { Some(parse_expression(p, 0)?) };
                p.expect(TokenKind::Semicolon)?;
                let update = if p.at(TokenKind::RParen) { None } else { Some(parse_expression(p, 0)?) };
                p.expect(TokenKind::RParen)?;
                let body = parse_statement(p)?;
                return Ok(Statement::For(Box::new(ForStatement {
                    init: Some(ForInit::Variable(decl)),
                    test,
                    update,
                    body,
                    span: Span::new(start, p.pos()),
                })));
            }
            return Err(ParseError::unexpected(p.current().kind, p.current().span));
        }

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
    } else if p.at(TokenKind::Identifier) && (p.peek().kind == TokenKind::In || p.peek().kind == TokenKind::Of || (p.peek().kind == TokenKind::Identifier && p.token_text(p.peek()) == "of")) {
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
        let expr = parse_expression(p, 0)?;
        // Check if this is for-of/for-in with expression LHS: for (expr of/in ...)
        if p.at(TokenKind::Of) || p.at(TokenKind::In) {
            let is_of = p.at(TokenKind::Of);
            p.advance(); // consume 'of' or 'in'
            let right = parse_expression(p, 0)?;
            p.expect(TokenKind::RParen)?;
            let body = parse_statement(p)?;
            // Convert expr to pattern for the left-hand side
            let left = ForInOfLeft::Expression(expr);
            if is_of {
                return Ok(Statement::ForOf(Box::new(ForOfStatement {
                    left, right, body, is_await: false,
                    span: Span::new(start, p.pos()),
                })));
            } else {
                return Ok(Statement::ForIn(Box::new(ForInStatement {
                    left, right, body,
                    span: Span::new(start, p.pos()),
                })));
            }
        }
        Some(ForInit::Expression(expr))
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
            let pat = if p.at(TokenKind::LBrace) {
                parse_object_pattern(p)?
            } else if p.at(TokenKind::LBracket) {
                parse_array_pattern(p)?
            } else {
                let name = p.intern_current();
                let name_span = p.current().span;
                p.expect(TokenKind::Identifier)?;
                Pattern::Identifier(Identifier { name, span: name_span })
            };
            p.expect(TokenKind::RParen)?;
            Some(pat)
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

        // Computed property: { [expr]: value }
        if p.at(TokenKind::LBracket) {
            p.advance();
            let key_expr = super::expression::parse_expression(p, 0)?;
            p.expect(TokenKind::RBracket)?;
            p.expect(TokenKind::Colon)?;
            let value_pat = if p.at(TokenKind::LBrace) {
                parse_object_pattern(p)?
            } else if p.at(TokenKind::LBracket) {
                parse_array_pattern(p)?
            } else {
                let value_name = p.intern_current();
                let value_span = p.current().span;
                p.expect(TokenKind::Identifier)?;
                Pattern::Identifier(Identifier { name: value_name, span: value_span })
            };
            properties.push(ObjectPatternProperty::Property {
                key: PropertyKey::Computed(Box::new(key_expr)),
                value: value_pat,
                computed: true,
                shorthand: false,
                span: Span::new(prop_start, p.pos()),
            });
            if !p.at(TokenKind::RBrace) {
                p.expect(TokenKind::Comma)?;
            }
            continue;
        }

        let key_name = p.intern_current();
        let key_span = p.current().span;
        p.advance(); // consume identifier (or keyword used as property)

        if p.eat(TokenKind::Colon) {
            // Renamed: { a: x }, { a: {b} }, { a: [x, y] }, { a: x = 5 }
            let value_pat = if p.at(TokenKind::LBrace) {
                parse_object_pattern(p)?
            } else if p.at(TokenKind::LBracket) {
                parse_array_pattern(p)?
            } else {
                let value_name = p.intern_current();
                let value_span = p.current().span;
                p.expect(TokenKind::Identifier)?;
                // Check for default: { a: x = 5 }
                if p.eat(TokenKind::Assign) {
                    let default_expr = super::expression::parse_expression(p, 3)?;
                    Pattern::Assignment(Box::new(AssignmentPattern {
                        left: Pattern::Identifier(Identifier { name: value_name, span: value_span }),
                        right: default_expr,
                        span: Span::new(value_span.start, p.pos()),
                    }))
                } else {
                    Pattern::Identifier(Identifier { name: value_name, span: value_span })
                }
            };
            properties.push(ObjectPatternProperty::Property {
                key: PropertyKey::Identifier(key_name),
                value: value_pat,
                computed: false,
                shorthand: false,
                span: Span::new(prop_start, p.pos()),
            });
        } else if p.eat(TokenKind::Assign) {
            // Shorthand with default: { a = 10 }
            let default_expr = super::expression::parse_expression(p, 3)?;
            properties.push(ObjectPatternProperty::Property {
                key: PropertyKey::Identifier(key_name),
                value: Pattern::Assignment(Box::new(AssignmentPattern {
                    left: Pattern::Identifier(Identifier { name: key_name, span: key_span }),
                    right: default_expr,
                    span: Span::new(key_span.start, p.pos()),
                })),
                computed: false,
                shorthand: true,
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
        if p.at(TokenKind::LBracket) {
            // Nested array pattern: [[a, b], c]
            let nested = parse_array_pattern(p)?;
            elements.push(Some(nested));
        } else if p.at(TokenKind::LBrace) {
            // Nested object pattern: [{a, b}, c]
            let nested = parse_object_pattern(p)?;
            elements.push(Some(nested));
        } else {
            let name = p.intern_current();
            let name_span = p.current().span;
            p.expect(TokenKind::Identifier)?;
            // Check for default value: x = expr
            if p.eat(TokenKind::Assign) {
                let default_expr = super::expression::parse_expression(p, 3)?;
                elements.push(Some(Pattern::Assignment(Box::new(AssignmentPattern {
                    left: Pattern::Identifier(Identifier { name, span: name_span }),
                    right: default_expr,
                    span: Span::new(name_span.start, p.pos()),
                }))));
            } else {
                elements.push(Some(Pattern::Identifier(Identifier { name, span: name_span })));
            }
        }

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
