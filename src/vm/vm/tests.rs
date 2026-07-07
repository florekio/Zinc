use super::*;
use crate::compiler::chunk::Chunk;
use crate::compiler::opcode::OpCode;

/// Helper: build a chunk and interner, returning (chunk, interner).
fn make_env() -> (Chunk, Interner) {
    let mut interner = Interner::new();
    let name = interner.intern("<test>");
    let source = interner.intern("<test-src>");
    let chunk = Chunk::new(name, source);
    (chunk, interner)
}

fn emit_op(chunk: &mut Chunk, op: OpCode) {
    chunk.emit_op(op, 1);
}

#[test]
fn test_inline_string_producer() {
    // SSO: new_str packs short strings inline (no interning) and they behave
    // as strings across the polymorphic helpers.
    let (chunk, interner) = make_env();
    let mut vm = Vm::new(chunk, interner);
    let before = vm.interner.len();

    let v = vm.new_str("abc");
    assert!(vm.is_string_like(v));
    assert!(v.is_inline_string());
    assert_eq!(vm.string_char_len(v), 3);
    assert_eq!(vm.value_to_string(v), "abc");
    assert_eq!(vm.type_of_value(v), "string");
    // Producing a short value did NOT intern it.
    assert_eq!(vm.interner.len(), before, "inline string must not be interned on creation");

    // Strict equality vs an interned copy (content compare, no allocation).
    let interned = Value::string(vm.interner.intern("abc"));
    assert!(vm.strict_eq(v, interned));
    assert!(vm.strict_eq(interned, v));
    let other = vm.new_str("abd");
    assert!(!vm.strict_eq(v, other));

    // Interns on demand (property-key path) and round-trips.
    let id = vm.flatten_to_string_id(v);
    assert_eq!(vm.interner.resolve(id), "abc");

    // Empty stays the interned singleton (avoids a truthy empty string).
    let e = vm.new_str("");
    assert!(!e.is_inline_string());
    assert_eq!(vm.value_to_string(e), "");

    // 5 bytes (Unicode): still inline; char count, not byte length.
    let u = vm.new_str("a\u{2713}b");
    assert!(u.is_inline_string());
    assert_eq!(vm.string_char_len(u), 3);
    assert_eq!(vm.value_to_string(u), "a\u{2713}b");

    // > 5 bytes: falls back to interning.
    let long = vm.new_str("abcdef");
    assert!(!long.is_inline_string());
    assert!(long.is_interned_string());
    assert_eq!(vm.value_to_string(long), "abcdef");
}

fn emit_const_number(chunk: &mut Chunk, n: f64) {
    let idx = chunk.add_constant(Value::number(n));
    chunk.emit_op_u16(OpCode::Const, idx, 1);
}

fn emit_const_int(chunk: &mut Chunk, n: i32) {
    let idx = chunk.add_constant(Value::int(n));
    chunk.emit_op_u16(OpCode::Const, idx, 1);
}

fn emit_const_string(chunk: &mut Chunk, interner: &mut Interner, s: &str) {
    let sid = interner.intern(s);
    let idx = chunk.add_constant(Value::string(sid));
    chunk.emit_op_u16(OpCode::Const, idx, 1);
}

fn run(chunk: Chunk, interner: Interner) -> Result<Value, VmError> {
    let mut vm = Vm::new(chunk, interner);
    vm.run()
}

// -- Constants --

#[test]
fn test_push_constant_number() {
    let (mut chunk, interner) = make_env();
    emit_const_number(&mut chunk, 42.5);
    emit_op(&mut chunk, OpCode::Halt);
    let result = run(chunk, interner).unwrap();
    assert_eq!(result.as_number(), Some(42.5));
}

#[test]
fn test_push_literals() {
    let (mut chunk, interner) = make_env();
    emit_op(&mut chunk, OpCode::True);
    emit_op(&mut chunk, OpCode::Halt);
    assert_eq!(run(chunk, interner).unwrap().as_bool(), Some(true));

    let (mut chunk, interner) = make_env();
    emit_op(&mut chunk, OpCode::False);
    emit_op(&mut chunk, OpCode::Halt);
    assert_eq!(run(chunk, interner).unwrap().as_bool(), Some(false));

    let (mut chunk, interner) = make_env();
    emit_op(&mut chunk, OpCode::Null);
    emit_op(&mut chunk, OpCode::Halt);
    assert!(run(chunk, interner).unwrap().is_null());

    let (mut chunk, interner) = make_env();
    emit_op(&mut chunk, OpCode::Undefined);
    emit_op(&mut chunk, OpCode::Halt);
    assert!(run(chunk, interner).unwrap().is_undefined());

    let (mut chunk, interner) = make_env();
    emit_op(&mut chunk, OpCode::Zero);
    emit_op(&mut chunk, OpCode::Halt);
    assert_eq!(run(chunk, interner).unwrap().as_int(), Some(0));

    let (mut chunk, interner) = make_env();
    emit_op(&mut chunk, OpCode::One);
    emit_op(&mut chunk, OpCode::Halt);
    assert_eq!(run(chunk, interner).unwrap().as_int(), Some(1));
}

// -- Arithmetic --

#[test]
fn test_add_numbers() {
    let (mut chunk, interner) = make_env();
    emit_const_number(&mut chunk, 10.0);
    emit_const_number(&mut chunk, 20.0);
    emit_op(&mut chunk, OpCode::Add);
    emit_op(&mut chunk, OpCode::Halt);
    let result = run(chunk, interner).unwrap();
    assert_eq!(result.as_number(), Some(30.0));
}

#[test]
fn test_add_strings() {
    let (mut chunk, mut interner) = make_env();
    emit_const_string(&mut chunk, &mut interner, "hello ");
    emit_const_string(&mut chunk, &mut interner, "world");
    emit_op(&mut chunk, OpCode::Add);
    emit_op(&mut chunk, OpCode::Halt);
    let result = run(chunk, interner).unwrap();
    // Result should be a string (TAG_STRING or ConsString object)
    assert!(result.is_string() || result.is_object());
}

#[test]
fn test_sub() {
    let (mut chunk, interner) = make_env();
    emit_const_number(&mut chunk, 50.0);
    emit_const_number(&mut chunk, 8.0);
    emit_op(&mut chunk, OpCode::Sub);
    emit_op(&mut chunk, OpCode::Halt);
    assert_eq!(run(chunk, interner).unwrap().as_number(), Some(42.0));
}

#[test]
fn test_mul() {
    let (mut chunk, interner) = make_env();
    emit_const_number(&mut chunk, 6.0);
    emit_const_number(&mut chunk, 7.0);
    emit_op(&mut chunk, OpCode::Mul);
    emit_op(&mut chunk, OpCode::Halt);
    assert_eq!(run(chunk, interner).unwrap().as_number(), Some(42.0));
}

#[test]
fn test_div() {
    let (mut chunk, interner) = make_env();
    emit_const_number(&mut chunk, 84.0);
    emit_const_number(&mut chunk, 2.0);
    emit_op(&mut chunk, OpCode::Div);
    emit_op(&mut chunk, OpCode::Halt);
    assert_eq!(run(chunk, interner).unwrap().as_number(), Some(42.0));
}

#[test]
fn test_rem() {
    let (mut chunk, interner) = make_env();
    emit_const_number(&mut chunk, 10.0);
    emit_const_number(&mut chunk, 3.0);
    emit_op(&mut chunk, OpCode::Rem);
    emit_op(&mut chunk, OpCode::Halt);
    assert_eq!(run(chunk, interner).unwrap().as_number(), Some(1.0));
}

#[test]
fn test_exp() {
    let (mut chunk, interner) = make_env();
    emit_const_number(&mut chunk, 2.0);
    emit_const_number(&mut chunk, 10.0);
    emit_op(&mut chunk, OpCode::Exp);
    emit_op(&mut chunk, OpCode::Halt);
    assert_eq!(run(chunk, interner).unwrap().as_number(), Some(1024.0));
}

#[test]
fn test_neg() {
    let (mut chunk, interner) = make_env();
    emit_const_number(&mut chunk, 42.0);
    emit_op(&mut chunk, OpCode::Neg);
    emit_op(&mut chunk, OpCode::Halt);
    assert_eq!(run(chunk, interner).unwrap().as_number(), Some(-42.0));
}

#[test]
fn test_inc_dec() {
    let (mut chunk, interner) = make_env();
    emit_const_int(&mut chunk, 5);
    emit_op(&mut chunk, OpCode::Inc);
    emit_op(&mut chunk, OpCode::Halt);
    assert_eq!(run(chunk, interner).unwrap().as_number(), Some(6.0));

    let (mut chunk, interner) = make_env();
    emit_const_int(&mut chunk, 5);
    emit_op(&mut chunk, OpCode::Dec);
    emit_op(&mut chunk, OpCode::Halt);
    assert_eq!(run(chunk, interner).unwrap().as_number(), Some(4.0));
}

// -- Comparison --

#[test]
fn test_strict_eq() {
    let (mut chunk, interner) = make_env();
    emit_const_number(&mut chunk, 42.0);
    emit_const_int(&mut chunk, 42);
    emit_op(&mut chunk, OpCode::StrictEq);
    emit_op(&mut chunk, OpCode::Halt);
    assert_eq!(run(chunk, interner).unwrap().as_bool(), Some(true));
}

#[test]
fn test_comparison_lt() {
    let (mut chunk, interner) = make_env();
    emit_const_number(&mut chunk, 1.0);
    emit_const_number(&mut chunk, 2.0);
    emit_op(&mut chunk, OpCode::Lt);
    emit_op(&mut chunk, OpCode::Halt);
    assert_eq!(run(chunk, interner).unwrap().as_bool(), Some(true));
}

// -- Not --

#[test]
fn test_not() {
    let (mut chunk, interner) = make_env();
    emit_op(&mut chunk, OpCode::True);
    emit_op(&mut chunk, OpCode::Not);
    emit_op(&mut chunk, OpCode::Halt);
    assert_eq!(run(chunk, interner).unwrap().as_bool(), Some(false));

    let (mut chunk, interner) = make_env();
    emit_op(&mut chunk, OpCode::Zero);
    emit_op(&mut chunk, OpCode::Not);
    emit_op(&mut chunk, OpCode::Halt);
    assert_eq!(run(chunk, interner).unwrap().as_bool(), Some(true));
}

// -- Globals --

#[test]
fn test_define_and_get_global() {
    let (mut chunk, mut interner) = make_env();
    // define global "x" = 42
    let x_id = interner.intern("x");
    let name_idx = chunk.add_constant(Value::string(x_id));
    emit_const_int(&mut chunk, 42);
    chunk.emit_op_u16(OpCode::DefineGlobal, name_idx, 1);
    // get global "x"
    chunk.emit_op_u16(OpCode::GetGlobal, name_idx, 1);
    emit_op(&mut chunk, OpCode::Halt);

    let result = run(chunk, interner).unwrap();
    assert_eq!(result.as_int(), Some(42));
}

#[test]
fn test_set_global() {
    let (mut chunk, mut interner) = make_env();
    let x_id = interner.intern("x");
    let name_idx = chunk.add_constant(Value::string(x_id));
    // define global "x" = 0
    emit_op(&mut chunk, OpCode::Zero);
    chunk.emit_op_u16(OpCode::DefineGlobal, name_idx, 1);
    // set global "x" = 99
    emit_const_int(&mut chunk, 99);
    chunk.emit_op_u16(OpCode::SetGlobal, name_idx, 1);
    emit_op(&mut chunk, OpCode::Pop); // SetGlobal leaves value on stack
    // get it back
    chunk.emit_op_u16(OpCode::GetGlobal, name_idx, 1);
    emit_op(&mut chunk, OpCode::Halt);

    let result = run(chunk, interner).unwrap();
    assert_eq!(result.as_int(), Some(99));
}

#[test]
fn test_get_undefined_global_is_error() {
    let (mut chunk, mut interner) = make_env();
    let x_id = interner.intern("nope");
    let name_idx = chunk.add_constant(Value::string(x_id));
    chunk.emit_op_u16(OpCode::GetGlobal, name_idx, 1);
    emit_op(&mut chunk, OpCode::Halt);
    let err = run(chunk, interner).unwrap_err();
    match err {
        VmError::ReferenceError(msg) => assert!(msg.contains("nope")),
        VmError::RuntimeError(msg) => assert!(msg.contains("ReferenceError") && msg.contains("nope"),
            "expected ReferenceError about 'nope', got: {msg}"),
        other => panic!("expected ReferenceError, got {other:?}"),
    }
}

// -- Locals --

#[test]
fn test_get_set_local() {
    let (mut chunk, interner) = make_env();
    // slot 0 = placeholder for the "script" local
    emit_op(&mut chunk, OpCode::Undefined);
    // push 42 into slot 1
    emit_const_int(&mut chunk, 42);
    // GetLocal slot 1
    chunk.emit_op_u8(OpCode::GetLocal, 1, 1);
    emit_op(&mut chunk, OpCode::Halt);
    let result = run(chunk, interner).unwrap();
    assert_eq!(result.as_int(), Some(42));
}

// -- Control Flow --

#[test]
fn test_jump_if_false() {
    // Push false, JumpIfFalse over push(1), push(2), Halt
    let (mut chunk, interner) = make_env();
    emit_op(&mut chunk, OpCode::False);
    let jump_pos = chunk.emit_jump(OpCode::JumpIfFalse, 1);
    emit_const_int(&mut chunk, 1); // should be skipped
    emit_op(&mut chunk, OpCode::Halt);
    chunk.patch_jump(jump_pos);
    emit_const_int(&mut chunk, 2); // should be reached
    emit_op(&mut chunk, OpCode::Halt);

    let result = run(chunk, interner).unwrap();
    assert_eq!(result.as_int(), Some(2));
}

#[test]
fn test_loop() {
    // Simple loop: sum = 0, i = 3; while (i > 0) { sum += i; i--; }
    // We'll use globals for sum and i, but simpler with locals:
    //   slot 0 = sum = 0
    //   slot 1 = i = 3
    let (mut chunk, interner) = make_env();

    // slot 0: sum = 0
    emit_op(&mut chunk, OpCode::Zero);
    // slot 1: i = 3
    emit_const_int(&mut chunk, 3);

    // loop_start:
    let loop_start = chunk.len();
    // push i (slot 1)
    chunk.emit_op_u8(OpCode::GetLocal, 1, 1);
    // push 0
    emit_op(&mut chunk, OpCode::Zero);
    // i > 0 ?
    emit_op(&mut chunk, OpCode::Gt);
    // if false, jump to end
    let exit_jump = chunk.emit_jump(OpCode::JumpIfFalse, 1);

    // sum = sum + i
    chunk.emit_op_u8(OpCode::GetLocal, 0, 1); // push sum
    chunk.emit_op_u8(OpCode::GetLocal, 1, 1); // push i
    emit_op(&mut chunk, OpCode::Add);           // sum + i
    chunk.emit_op_u8(OpCode::SetLocal, 0, 1);  // store back to sum
    emit_op(&mut chunk, OpCode::Pop);            // pop the SetLocal result

    // i = i - 1
    chunk.emit_op_u8(OpCode::GetLocal, 1, 1);
    emit_op(&mut chunk, OpCode::Dec);
    chunk.emit_op_u8(OpCode::SetLocal, 1, 1);
    emit_op(&mut chunk, OpCode::Pop);

    // loop back
    chunk.emit_loop(loop_start, 1);

    // exit:
    chunk.patch_jump(exit_jump);
    // push sum
    chunk.emit_op_u8(OpCode::GetLocal, 0, 1);
    emit_op(&mut chunk, OpCode::Halt);

    let result = run(chunk, interner).unwrap();
    // 3 + 2 + 1 = 6
    assert_eq!(result.as_number(), Some(6.0));
}

// -- Bitwise --

#[test]
fn test_bitwise_and() {
    let (mut chunk, interner) = make_env();
    emit_const_int(&mut chunk, 0b1100);
    emit_const_int(&mut chunk, 0b1010);
    emit_op(&mut chunk, OpCode::BitAnd);
    emit_op(&mut chunk, OpCode::Halt);
    assert_eq!(run(chunk, interner).unwrap().as_int(), Some(0b1000));
}

#[test]
fn test_bitwise_or() {
    let (mut chunk, interner) = make_env();
    emit_const_int(&mut chunk, 0b1100);
    emit_const_int(&mut chunk, 0b1010);
    emit_op(&mut chunk, OpCode::BitOr);
    emit_op(&mut chunk, OpCode::Halt);
    assert_eq!(run(chunk, interner).unwrap().as_int(), Some(0b1110));
}

#[test]
fn test_shl() {
    let (mut chunk, interner) = make_env();
    emit_const_int(&mut chunk, 1);
    emit_const_int(&mut chunk, 4);
    emit_op(&mut chunk, OpCode::Shl);
    emit_op(&mut chunk, OpCode::Halt);
    assert_eq!(run(chunk, interner).unwrap().as_int(), Some(16));
}

// -- TypeOf --

#[test]
fn test_typeof() {
    let (mut chunk, interner) = make_env();
    emit_const_number(&mut chunk, 2.5);
    emit_op(&mut chunk, OpCode::TypeOf);
    emit_op(&mut chunk, OpCode::Halt);
    let result = run(chunk, interner).unwrap();
    assert!(result.is_string());
}

// -- Dup / Swap --

#[test]
fn test_dup() {
    let (mut chunk, interner) = make_env();
    emit_const_int(&mut chunk, 7);
    emit_op(&mut chunk, OpCode::Dup);
    emit_op(&mut chunk, OpCode::Add);
    emit_op(&mut chunk, OpCode::Halt);
    assert_eq!(run(chunk, interner).unwrap().as_number(), Some(14.0));
}

#[test]
fn test_swap() {
    let (mut chunk, interner) = make_env();
    emit_const_int(&mut chunk, 10);
    emit_const_int(&mut chunk, 3);
    emit_op(&mut chunk, OpCode::Swap);
    emit_op(&mut chunk, OpCode::Sub);
    emit_op(&mut chunk, OpCode::Halt);
    // After swap: stack = [3, 10], sub => 3 - 10 = -7
    assert_eq!(run(chunk, interner).unwrap().as_number(), Some(-7.0));
}

// -- Return --

#[test]
fn test_return() {
    let (mut chunk, interner) = make_env();
    emit_const_int(&mut chunk, 99);
    emit_op(&mut chunk, OpCode::Return);
    assert_eq!(run(chunk, interner).unwrap().as_int(), Some(99));
}

#[test]
fn test_return_undefined() {
    let (mut chunk, interner) = make_env();
    emit_op(&mut chunk, OpCode::ReturnUndefined);
    assert!(run(chunk, interner).unwrap().is_undefined());
}

// -- Abstract equality --

#[test]
fn test_abstract_eq_null_undefined() {
    let (mut chunk, interner) = make_env();
    emit_op(&mut chunk, OpCode::Null);
    emit_op(&mut chunk, OpCode::Undefined);
    emit_op(&mut chunk, OpCode::Eq);
    emit_op(&mut chunk, OpCode::Halt);
    assert_eq!(run(chunk, interner).unwrap().as_bool(), Some(true));
}

#[test]
fn test_strict_ne() {
    let (mut chunk, interner) = make_env();
    emit_const_int(&mut chunk, 1);
    emit_op(&mut chunk, OpCode::True);
    emit_op(&mut chunk, OpCode::StrictNe);
    emit_op(&mut chunk, OpCode::Halt);
    assert_eq!(run(chunk, interner).unwrap().as_bool(), Some(true));
}
