use zinc::ast::node::*;
use zinc::lexer::lexer::Lexer;
use zinc::parser::parser::Parser;
use zinc::util::interner::Interner;

fn parse(source: &str) -> (Program, Interner) {
    let mut interner = Interner::new();
    let tokens = {
        let mut lexer = Lexer::new(source, &mut interner);
        lexer.tokenize()
    };
    let mut parser = Parser::new(tokens, source, &mut interner);
    let program = parser.parse_program().expect("parse error");
    assert!(parser.errors.is_empty(), "parser errors: {:?}", parser.errors);
    (program, interner)
}

#[test]
fn test_variable_declaration() {
    let (prog, _) = parse("let x = 42;");
    assert_eq!(prog.body.len(), 1);
    match &prog.body[0] {
        Statement::Variable(decl) => {
            assert_eq!(decl.kind, VarKind::Let);
            assert_eq!(decl.declarations.len(), 1);
            assert!(decl.declarations[0].init.is_some());
        }
        other => panic!("expected Variable, got {other:?}"),
    }
}

#[test]
fn test_function_declaration() {
    let (prog, interner) = parse("function foo(a, b) { return a + b; }");
    assert_eq!(prog.body.len(), 1);
    match &prog.body[0] {
        Statement::Function(f) => {
            assert_eq!(interner.resolve(f.id.unwrap()), "foo");
            assert_eq!(f.params.len(), 2);
            assert_eq!(f.body.body.len(), 1);
        }
        other => panic!("expected Function, got {other:?}"),
    }
}

#[test]
fn test_if_else() {
    let (prog, _) = parse("if (x > 0) { y = 1; } else { y = 2; }");
    assert_eq!(prog.body.len(), 1);
    match &prog.body[0] {
        Statement::If(stmt) => {
            assert!(stmt.alternate.is_some());
        }
        other => panic!("expected If, got {other:?}"),
    }
}

#[test]
fn test_for_loop() {
    let (prog, _) = parse("for (let i = 0; i < 10; i++) { x += i; }");
    assert_eq!(prog.body.len(), 1);
    match &prog.body[0] {
        Statement::For(f) => {
            assert!(f.init.is_some());
            assert!(f.test.is_some());
            assert!(f.update.is_some());
        }
        other => panic!("expected For, got {other:?}"),
    }
}

#[test]
fn test_while_loop() {
    let (prog, _) = parse("while (true) { break; }");
    assert_eq!(prog.body.len(), 1);
    assert!(matches!(&prog.body[0], Statement::While(_)));
}

#[test]
fn test_try_catch() {
    let (prog, _) = parse("try { x(); } catch (e) { log(e); } finally { cleanup(); }");
    assert_eq!(prog.body.len(), 1);
    match &prog.body[0] {
        Statement::Try(t) => {
            assert!(t.handler.is_some());
            assert!(t.finalizer.is_some());
        }
        other => panic!("expected Try, got {other:?}"),
    }
}

#[test]
fn test_arrow_function() {
    let (prog, _) = parse("const add = (a, b) => a + b;");
    assert_eq!(prog.body.len(), 1);
    match &prog.body[0] {
        Statement::Variable(decl) => {
            let init = decl.declarations[0].init.as_ref().unwrap();
            assert!(matches!(init, Expression::ArrowFunction(_)));
        }
        other => panic!("expected Variable, got {other:?}"),
    }
}

#[test]
fn test_arrow_no_params() {
    let (prog, _) = parse("const f = () => 42;");
    match &prog.body[0] {
        Statement::Variable(decl) => {
            let init = decl.declarations[0].init.as_ref().unwrap();
            match init {
                Expression::ArrowFunction(a) => {
                    assert!(a.params.is_empty());
                }
                other => panic!("expected ArrowFunction, got {other:?}"),
            }
        }
        other => panic!("expected Variable, got {other:?}"),
    }
}

#[test]
fn test_object_literal() {
    let (prog, _) = parse("const obj = { a: 1, b: 2, c };");
    assert_eq!(prog.body.len(), 1);
    match &prog.body[0] {
        Statement::Variable(decl) => {
            let init = decl.declarations[0].init.as_ref().unwrap();
            match init {
                Expression::Object(o) => {
                    assert_eq!(o.properties.len(), 3);
                    // Third property should be shorthand
                    match &o.properties[2] {
                        ObjectProperty::Property(p) => assert!(p.shorthand),
                        other => panic!("expected Property, got {other:?}"),
                    }
                }
                other => panic!("expected Object, got {other:?}"),
            }
        }
        other => panic!("expected Variable, got {other:?}"),
    }
}

#[test]
fn test_array_literal() {
    let (prog, _) = parse("const arr = [1, 2, , 3];");
    match &prog.body[0] {
        Statement::Variable(decl) => {
            let init = decl.declarations[0].init.as_ref().unwrap();
            match init {
                Expression::Array(a) => {
                    assert_eq!(a.elements.len(), 4);
                    assert!(a.elements[2].is_none()); // hole
                }
                other => panic!("expected Array, got {other:?}"),
            }
        }
        other => panic!("expected Variable, got {other:?}"),
    }
}

#[test]
fn test_member_expression() {
    let (prog, _) = parse("console.log(x);");
    match &prog.body[0] {
        Statement::Expression(expr_stmt) => {
            match &expr_stmt.expression {
                Expression::Call(call) => {
                    assert!(matches!(&call.callee, Expression::Member(_)));
                    assert_eq!(call.arguments.len(), 1);
                }
                other => panic!("expected Call, got {other:?}"),
            }
        }
        other => panic!("expected ExpressionStatement, got {other:?}"),
    }
}

#[test]
fn test_operator_precedence() {
    // 1 + 2 * 3 should parse as 1 + (2 * 3)
    let (prog, _) = parse("1 + 2 * 3;");
    match &prog.body[0] {
        Statement::Expression(expr_stmt) => {
            match &expr_stmt.expression {
                Expression::Binary(b) => {
                    assert_eq!(b.operator, BinaryOperator::Add);
                    assert!(matches!(&b.right, Expression::Binary(inner) if inner.operator == BinaryOperator::Mul));
                }
                other => panic!("expected Binary, got {other:?}"),
            }
        }
        other => panic!("expected ExpressionStatement, got {other:?}"),
    }
}

#[test]
fn test_class_declaration() {
    let source = r#"
        class Animal {
            constructor(name) {
                this.name = name;
            }
            speak() {
                return this.name;
            }
        }
    "#;
    let (prog, _) = parse(source);
    assert_eq!(prog.body.len(), 1);
    match &prog.body[0] {
        Statement::Class(c) => {
            assert!(c.id.is_some());
            assert_eq!(c.body.body.len(), 2); // constructor + speak
        }
        other => panic!("expected Class, got {other:?}"),
    }
}

#[test]
fn test_fibonacci() {
    let source = r#"
        function fibonacci(n) {
            if (n <= 1) return n;
            return fibonacci(n - 1) + fibonacci(n - 2);
        }
        const result = fibonacci(10);
    "#;
    let (prog, _) = parse(source);
    assert_eq!(prog.body.len(), 2);
    assert!(matches!(&prog.body[0], Statement::Function(_)));
    assert!(matches!(&prog.body[1], Statement::Variable(_)));
}

#[test]
fn test_switch_statement() {
    let source = r#"
        switch (x) {
            case 1:
                y = "one";
                break;
            case 2:
                y = "two";
                break;
            default:
                y = "other";
        }
    "#;
    let (prog, _) = parse(source);
    match &prog.body[0] {
        Statement::Switch(s) => {
            assert_eq!(s.cases.len(), 3);
            assert!(s.cases[2].test.is_none()); // default
        }
        other => panic!("expected Switch, got {other:?}"),
    }
}

#[test]
fn test_for_of() {
    let (prog, _) = parse("for (const x of items) { process(x); }");
    assert!(matches!(&prog.body[0], Statement::ForOf(_)));
}

#[test]
fn test_for_in() {
    let (prog, _) = parse("for (let key in obj) { log(key); }");
    assert!(matches!(&prog.body[0], Statement::ForIn(_)));
}

#[test]
fn test_spread_in_call() {
    let (prog, _) = parse("foo(...args);");
    match &prog.body[0] {
        Statement::Expression(e) => match &e.expression {
            Expression::Call(c) => {
                assert_eq!(c.arguments.len(), 1);
                assert!(matches!(&c.arguments[0], Expression::Spread(_)));
            }
            other => panic!("expected Call, got {other:?}"),
        },
        other => panic!("expected ExpressionStatement, got {other:?}"),
    }
}

#[test]
fn test_new_expression() {
    let (prog, _) = parse("new Foo(1, 2);");
    match &prog.body[0] {
        Statement::Expression(e) => {
            assert!(matches!(&e.expression, Expression::New(_)));
        }
        other => panic!("expected ExpressionStatement, got {other:?}"),
    }
}

#[test]
fn test_ternary() {
    let (prog, _) = parse("const x = a ? b : c;");
    match &prog.body[0] {
        Statement::Variable(decl) => {
            assert!(matches!(
                decl.declarations[0].init.as_ref().unwrap(),
                Expression::Conditional(_)
            ));
        }
        other => panic!("expected Variable, got {other:?}"),
    }
}
