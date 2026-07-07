use super::*;
use crate::compiler::disassemble::disassemble;
use crate::lexer::lexer::Lexer;
use crate::parser::parser::Parser;

fn compile(source: &str) -> (Chunk, Interner) {
    let mut interner = Interner::new();
    let tokens = {
        let mut lexer = Lexer::new(source, &mut interner);
        lexer.tokenize()
    };
    let program = {
        let mut parser = Parser::new(tokens, source, &mut interner);
        parser.parse_program().expect("parse error")
    };
    let chunk = {
        let compiler = Compiler::new(&mut interner);
        compiler.compile_program(&program).expect("compile error")
    };
    (chunk, interner)
}

#[test]
fn test_compile_number() {
    let (chunk, interner) = compile("42;");
    let dis = disassemble(&chunk, &interner);
    assert!(dis.contains("Const"));
    assert!(dis.contains("Halt"));
}

#[test]
fn test_compile_addition() {
    let (chunk, interner) = compile("1 + 2;");
    let dis = disassemble(&chunk, &interner);
    assert!(dis.contains("One"));
    assert!(dis.contains("Add"));
}

#[test]
fn test_compile_variable() {
    let (chunk, interner) = compile("var x = 10;");
    let dis = disassemble(&chunk, &interner);
    assert!(dis.contains("DefineGlobal"));
}

#[test]
fn test_compile_if() {
    let (chunk, interner) = compile("if (true) { 1; } else { 2; }");
    let dis = disassemble(&chunk, &interner);
    assert!(dis.contains("JumpIfFalse"));
    assert!(dis.contains("Jump"));
}

#[test]
fn test_compile_while() {
    let (chunk, interner) = compile("while (true) { 1; }");
    let dis = disassemble(&chunk, &interner);
    assert!(dis.contains("Loop"));
}

#[test]
fn test_compile_function() {
    let (chunk, interner) = compile("function foo(x) { return x + 1; }");
    let dis = disassemble(&chunk, &interner);
    assert!(dis.contains("Closure"));
    assert!(dis.contains("DefineGlobal"));
    assert!(dis.contains("Return"));
}

#[test]
fn test_compile_boolean_true() {
    let (chunk, _) = compile("true;");
    assert_eq!(chunk.code[0], OpCode::True as u8);
}

#[test]
fn test_compile_boolean_false() {
    let (chunk, _) = compile("false;");
    assert_eq!(chunk.code[0], OpCode::False as u8);
}

#[test]
fn test_compile_null() {
    let (chunk, _) = compile("null;");
    assert_eq!(chunk.code[0], OpCode::Null as u8);
}

#[test]
fn test_compile_zero_one() {
    let (chunk, _) = compile("0;");
    assert_eq!(chunk.code[0], OpCode::Zero as u8);
    let (chunk, _) = compile("1;");
    assert_eq!(chunk.code[0], OpCode::One as u8);
}

#[test]
fn test_compile_for_loop() {
    let (chunk, interner) = compile("for (var i = 0; i < 10; i++) { i; }");
    let dis = disassemble(&chunk, &interner);
    assert!(dis.contains("Loop"));
    assert!(dis.contains("JumpIfFalse"));
}

#[test]
fn test_compile_logical_and() {
    let (chunk, interner) = compile("true && false;");
    let dis = disassemble(&chunk, &interner);
    assert!(dis.contains("JumpIfFalsePeek"));
}

#[test]
fn test_compile_logical_or() {
    let (chunk, interner) = compile("false || true;");
    let dis = disassemble(&chunk, &interner);
    assert!(dis.contains("JumpIfTruePeek"));
}

#[test]
fn test_compile_ternary() {
    let (chunk, interner) = compile("true ? 1 : 2;");
    let dis = disassemble(&chunk, &interner);
    assert!(dis.contains("JumpIfFalse"));
    assert!(dis.contains("Jump"));
}

#[test]
fn test_compile_unary_neg() {
    let (chunk, interner) = compile("-1;");
    let dis = disassemble(&chunk, &interner);
    assert!(dis.contains("Neg"));
}

#[test]
fn test_compile_typeof_global() {
    let (chunk, interner) = compile("typeof x;");
    let dis = disassemble(&chunk, &interner);
    assert!(dis.contains("TypeOfGlobal"));
}

#[test]
fn test_compile_throw() {
    let (chunk, interner) = compile("throw 42;");
    let dis = disassemble(&chunk, &interner);
    assert!(dis.contains("Throw"));
}

#[test]
fn test_compile_return() {
    let (chunk, interner) = compile("function f() { return 1; }");
    let dis = disassemble(&chunk, &interner);
    assert!(dis.contains("Return"));
}

#[test]
fn test_compile_new() {
    let (chunk, interner) = compile("new Foo(1, 2);");
    let dis = disassemble(&chunk, &interner);
    assert!(dis.contains("Construct"));
}

#[test]
fn test_compile_array() {
    let (chunk, interner) = compile("[1, 2, 3];");
    let dis = disassemble(&chunk, &interner);
    assert!(dis.contains("CreateArray"));
    assert!(dis.contains("SetArrayItem"));
}

#[test]
fn test_compile_object() {
    let (chunk, interner) = compile("({a: 1});");
    let dis = disassemble(&chunk, &interner);
    assert!(dis.contains("CreateObject"));
    assert!(dis.contains("DefineDataProp"));
}

#[test]
fn test_compile_arrow() {
    let (chunk, interner) = compile("var f = (x) => x + 1;");
    let dis = disassemble(&chunk, &interner);
    assert!(dis.contains("Closure"));
}

#[test]
fn test_compile_arrow_block() {
    let (chunk, interner) = compile("var f = (x) => { return x; };");
    let dis = disassemble(&chunk, &interner);
    assert!(dis.contains("Closure"));
    assert!(dis.contains("Return"));
}

#[test]
fn test_compile_string() {
    let (chunk, interner) = compile("\"hello\";");
    let dis = disassemble(&chunk, &interner);
    assert!(dis.contains("Const"));
}

#[test]
fn test_compile_break_continue() {
    let (chunk, interner) = compile("while (true) { break; }");
    let dis = disassemble(&chunk, &interner);
    assert!(dis.contains("Jump"));
    assert!(dis.contains("Loop"));
}

#[test]
fn test_compile_do_while() {
    let (chunk, interner) = compile("do { 1; } while (true);");
    let dis = disassemble(&chunk, &interner);
    assert!(dis.contains("Loop"));
}

#[test]
fn test_compile_member_access() {
    let (chunk, interner) = compile("a.b;");
    let dis = disassemble(&chunk, &interner);
    assert!(dis.contains("GetProperty"));
}

#[test]
fn test_compile_computed_access() {
    let (chunk, interner) = compile("a[0];");
    let dis = disassemble(&chunk, &interner);
    assert!(dis.contains("GetElement"));
}

#[test]
fn test_compile_assignment_add() {
    let (chunk, interner) = compile("var x = 0; x += 1;");
    let dis = disassemble(&chunk, &interner);
    assert!(dis.contains("Add"));
    assert!(dis.contains("SetGlobal"));
}

// with statement not yet implemented

#[test]
fn test_compile_debugger() {
    let (chunk, _) = compile("debugger;");
    assert!(chunk.code.contains(&(OpCode::Debugger as u8)));
}

#[test]
fn test_compile_undefined_ident() {
    let (chunk, _) = compile("undefined;");
    assert_eq!(chunk.code[0], OpCode::Undefined as u8);
}

#[test]
fn test_compile_delete_global() {
    let (chunk, interner) = compile("delete x;");
    let dis = disassemble(&chunk, &interner);
    assert!(dis.contains("DeleteGlobal"));
}

#[test]
fn test_compile_method_call() {
    let (chunk, interner) = compile("a.b(1);");
    let dis = disassemble(&chunk, &interner);
    assert!(dis.contains("CallMethod"));
}
