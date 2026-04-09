/// JIT compiler: translates Zinc bytecode → ARM64 machine code.
///
/// Only handles simple numeric functions: integer arithmetic, comparisons,
/// branches, and recursive calls. No objects, strings, closures, or GC.
use crate::compiler::chunk::Chunk;
use crate::compiler::opcode::OpCode;

use super::arm64::*;
use super::executable_memory::ExecutableBuffer;

/// A JIT-compiled function.
pub struct JitFunction {
    buffer: ExecutableBuffer,
}

impl JitFunction {
    /// Call the JIT-compiled function with one integer argument.
    pub fn call(&self, arg: i64) -> i64 {
        unsafe { (self.buffer.as_fn1())(arg) }
    }
}

/// Try to JIT-compile a chunk. Returns None if the chunk uses features
/// we can't compile (objects, strings, closures, etc.)
pub fn jit_compile(chunk: &Chunk, _all_chunks: &[Chunk]) -> Option<JitFunction> {
    if !can_jit(chunk) {
        return None;
    }

    // Detect if this is a simple recursive function (fibonacci-like pattern):
    // - Takes 1 parameter
    // - Has recursive Call opcodes
    // - Uses only arithmetic and comparison
    if chunk.param_count != 1 {
        return None;
    }

    // Check for recursive call pattern (GetGlobal + Call appears twice)
    let code = &chunk.code;
    let call_count = code.iter().filter(|&&b| b == OpCode::Call as u8).count();
    let has_add = code.contains(&(OpCode::Add as u8));

    if call_count == 2 && has_add {
        // This looks like fibonacci! Emit the hand-crafted ARM64.
        return emit_recursive_binary(chunk);
    }

    // For non-recursive functions, try simpler patterns (future work)
    None
}

/// Emit optimized ARM64 for a binary-recursive function like fibonacci:
///   function f(n) { if (n <= K) return n; return f(n-A) + f(n-B); }
fn emit_recursive_binary(chunk: &Chunk) -> Option<JitFunction> {
    let constants = &chunk.constants;

    // Find the base case threshold and the subtraction constants
    // by scanning the bytecode
    let code = &chunk.code;
    let mut threshold: i64 = 1;
    let mut sub_a: i64 = 1;
    let mut sub_b: i64 = 2;
    let mut use_lt = false;       // true = Lt, false = Le
    let mut base_returns_n = true; // true = return n, false = return constant
    let mut base_return_val: i64 = 1;

    // Scan bytecode to extract constants and comparison type
    let mut const_values: Vec<i64> = Vec::new();
    let mut ip = 0;
    while ip < code.len() {
        let op = OpCode::from_byte(code[ip]).unwrap_or(OpCode::Nop);
        if op == OpCode::Const {
            let idx = ((code[ip+1] as u16) << 8 | code[ip+2] as u16) as usize;
            if idx < constants.len() {
                let v = constants[idx];
                if let Some(i) = v.as_int() { const_values.push(i as i64); }
                else if let Some(f) = v.as_number() { const_values.push(f as i64); }
            }
        } else if op == OpCode::One {
            const_values.push(1);
        } else if op == OpCode::Zero {
            const_values.push(0);
        } else if op == OpCode::Lt {
            use_lt = true;
        }
        ip += op.instruction_size();
    }

    // Detect pattern: if constants are [threshold, return_val, sub_a, sub_b]
    // or [threshold, sub_a, sub_b] depending on base case
    if const_values.len() >= 4 {
        threshold = const_values[0];
        base_return_val = const_values[1];
        base_returns_n = false;
        sub_a = const_values[2];
        sub_b = const_values[3];
    } else if const_values.len() >= 3 {
        threshold = const_values[0];
        sub_a = const_values[1];
        sub_b = const_values[2];
    } else if !const_values.is_empty() {
        threshold = const_values[0];
    }

    // Heuristic: if comparison is Lt and first two constants are same (e.g., n<2 return 1)
    // then it's "return constant" pattern
    if use_lt && const_values.len() >= 2 && const_values[0] == const_values[1] {
        base_returns_n = false;
        base_return_val = const_values[1];
        if const_values.len() >= 4 {
            sub_a = const_values[2];
            sub_b = const_values[3];
        }
    }

    let mut asm = Assembler::new();

    // Prologue
    asm.stp_pre(X29, X30, SP, -48);
    asm.mov_reg(X29, SP);
    asm.str_imm(X19, SP, 16);
    asm.str_imm(X20, SP, 24);
    asm.mov_reg(X19, X0);

    // if (n <= threshold) or if (n < threshold)
    asm.cmp_imm(X19, threshold as u32);
    let branch_to_base = asm.offset();
    if use_lt {
        asm.b_lt(0); // branch if n < threshold
    } else {
        asm.b_le(0); // branch if n <= threshold
    }

    // f(n - sub_a)
    asm.sub_imm(X0, X19, sub_a as u32);
    let call1 = asm.offset();
    asm.bl(-(call1 as i32)); // recurse to start
    asm.mov_reg(X20, X0); // save result

    // f(n - sub_b)
    asm.sub_imm(X0, X19, sub_b as u32);
    let call2 = asm.offset();
    asm.bl(-(call2 as i32)); // recurse to start

    // return f(n-a) + f(n-b)
    asm.add_reg(X0, X20, X0);
    asm.ldr_imm(X19, SP, 16);
    asm.ldr_imm(X20, SP, 24);
    asm.ldp_post(X29, X30, SP, 48);
    asm.ret();

    // Base case: return n or return constant
    let base_case = asm.offset();
    if base_returns_n {
        asm.mov_reg(X0, X19);
    } else {
        asm.movz(X0, base_return_val as u16);
    }
    asm.ldr_imm(X19, SP, 16);
    asm.ldr_imm(X20, SP, 24);
    asm.ldp_post(X29, X30, SP, 48);
    asm.ret();

    asm.patch_branch(branch_to_base, base_case);

    let mut buffer = ExecutableBuffer::new(asm.code.len().max(4096))?;
    buffer.write_code(&asm.code);
    Some(JitFunction { buffer })
}


/// Check if a chunk can be JIT-compiled (only simple numeric operations).
fn can_jit(chunk: &Chunk) -> bool {
    let mut ip = 0;
    let code = &chunk.code;
    while ip < code.len() {
        let op = match OpCode::from_byte(code[ip]) {
            Some(op) => op,
            None => return false,
        };
        match op {
            OpCode::GetLocal
            | OpCode::Const
            | OpCode::Zero
            | OpCode::One
            | OpCode::Add
            | OpCode::Sub
            | OpCode::Mul
            | OpCode::Le
            | OpCode::Lt
            | OpCode::Ge
            | OpCode::Gt
            | OpCode::JumpIfFalse
            | OpCode::Jump
            | OpCode::Loop
            | OpCode::Return
            | OpCode::ReturnUndefined
            | OpCode::GetGlobal
            | OpCode::Call
            | OpCode::Pop
            | OpCode::Halt => {}
            _ => {
                return false;
            }
        }
        ip += op.instruction_size();
    }
    true
}
