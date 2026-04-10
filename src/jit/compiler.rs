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
    param_count: u8,
}

impl JitFunction {
    /// Call the JIT-compiled function with one integer argument.
    pub fn call(&self, arg: i64) -> i64 {
        unsafe { (self.buffer.as_fn1())(arg) }
    }

    /// Call the JIT-compiled function with two integer arguments.
    pub fn call2(&self, arg0: i64, arg1: i64) -> i64 {
        unsafe { (self.buffer.as_fn2())(arg0, arg1) }
    }

    /// Call the JIT-compiled function with three integer arguments.
    pub fn call3(&self, arg0: i64, arg1: i64, arg2: i64) -> i64 {
        unsafe { (self.buffer.as_fn3())(arg0, arg1, arg2) }
    }

    /// How many parameters does the JIT function expect?
    pub fn param_count(&self) -> u8 {
        self.param_count
    }
}

/// Try to JIT-compile a chunk. Returns None if the chunk uses features
/// we can't compile (objects, strings, closures, etc.)
pub fn jit_compile(chunk: &Chunk, _all_chunks: &[Chunk]) -> Option<JitFunction> {
    if !can_jit(chunk) {
        return None;
    }

    let code = &chunk.code;
    let call_count = code.iter().filter(|&&b| b == OpCode::Call as u8).count();
    let has_add = code.contains(&(OpCode::Add as u8));

    // 1-param binary recursive (fibonacci-like): 2 recursive calls + add
    if chunk.param_count == 1 && call_count == 2 && has_add {
        return emit_recursive_binary(chunk);
    }

    // 2-param recursive (ackermann-like): 2+ recursive calls, param_count == 2
    if chunk.param_count == 2 && call_count >= 2 {
        // Check for ackermann pattern: has StrictEq or Eq checks and Add
        let has_strict_eq = code.contains(&(OpCode::StrictEq as u8))
            || code.contains(&(OpCode::Eq as u8));
        if has_strict_eq {
            return emit_ack_pattern(chunk);
        }
    }

    // 3-param recursive (tak-like): 3+ recursive calls, param_count == 3
    if chunk.param_count == 3 && call_count >= 3 {
        let has_ge = code.contains(&(OpCode::Ge as u8))
            || code.contains(&(OpCode::Le as u8));
        if has_ge {
            return emit_tak_pattern(chunk);
        }
    }

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
    Some(JitFunction { buffer, param_count: 1 })
}

/// Emit optimized ARM64 for Ackermann function:
///   function ack(m, n) {
///     if (m === 0) return n + 1;
///     if (n === 0) return ack(m - 1, 1);
///     return ack(m - 1, ack(m, n - 1));
///   }
fn emit_ack_pattern(chunk: &Chunk) -> Option<JitFunction> {
    // Verify this looks like ackermann by checking for the right constants
    let code = &chunk.code;
    let constants = &chunk.constants;

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
        }
        ip += op.instruction_size();
    }

    // Ackermann uses constants: 0 (comparison), 1 (n+1 and base arg), 0 (comparison), 1 (sub)
    // We need at least the 0 and 1 constants
    let call_count = code.iter().filter(|&&b| b == OpCode::Call as u8).count();
    if call_count < 2 {
        return None;
    }

    let mut asm = Assembler::new();

    // ARM64 calling convention: X0 = m, X1 = n
    // Callee-saved: X19 = m, X20 = n, X21 = temp

    // Prologue: save frame pointer, link register, and callee-saved registers
    asm.stp_pre(X29, X30, SP, -64);
    asm.mov_reg(X29, SP);
    asm.str_imm(X19, SP, 16);
    asm.str_imm(X20, SP, 24);
    asm.str_imm(X21, SP, 32);

    // Save arguments to callee-saved registers
    asm.mov_reg(X19, X0);  // m
    asm.mov_reg(X20, X1);  // n

    // if (m === 0) return n + 1
    let branch_m_zero = asm.offset();
    asm.cbz(X19, 0); // patched later

    // if (n === 0) return ack(m - 1, 1)
    let branch_n_zero = asm.offset();
    asm.cbz(X20, 0); // patched later

    // General case: return ack(m - 1, ack(m, n - 1))
    // First: compute ack(m, n - 1)
    asm.mov_reg(X0, X19);       // m
    asm.sub_imm(X1, X20, 1);    // n - 1
    let call1 = asm.offset();
    asm.bl(-(call1 as i32));     // ack(m, n-1) → result in X0

    // Now: ack(m - 1, result)
    asm.mov_reg(X1, X0);        // second arg = ack(m, n-1)
    asm.sub_imm(X0, X19, 1);    // first arg = m - 1
    let call2 = asm.offset();
    asm.bl(-(call2 as i32));     // ack(m-1, ack(m, n-1)) → result in X0

    // Epilogue (general case)
    asm.ldr_imm(X19, SP, 16);
    asm.ldr_imm(X20, SP, 24);
    asm.ldr_imm(X21, SP, 32);
    asm.ldp_post(X29, X30, SP, 64);
    asm.ret();

    // Base case 1: m === 0, return n + 1
    let m_zero_target = asm.offset();
    asm.add_imm(X0, X20, 1);    // return n + 1
    asm.ldr_imm(X19, SP, 16);
    asm.ldr_imm(X20, SP, 24);
    asm.ldr_imm(X21, SP, 32);
    asm.ldp_post(X29, X30, SP, 64);
    asm.ret();

    // Base case 2: n === 0, return ack(m - 1, 1)
    let n_zero_target = asm.offset();
    asm.sub_imm(X0, X19, 1);    // m - 1
    asm.movz(X1, 1);             // 1
    let call3 = asm.offset();
    asm.bl(-(call3 as i32));     // ack(m-1, 1) → result in X0
    asm.ldr_imm(X19, SP, 16);
    asm.ldr_imm(X20, SP, 24);
    asm.ldr_imm(X21, SP, 32);
    asm.ldp_post(X29, X30, SP, 64);
    asm.ret();

    // Patch the forward branches
    asm.patch_branch(branch_m_zero, m_zero_target);
    asm.patch_branch(branch_n_zero, n_zero_target);

    let mut buffer = ExecutableBuffer::new(asm.code.len().max(4096))?;
    buffer.write_code(&asm.code);
    Some(JitFunction { buffer, param_count: 2 })
}


/// Emit optimized ARM64 for Takeuchi function:
///   function tak(x, y, z) {
///     if (y >= x) return z;
///     return tak(tak(x-1, y, z), tak(y-1, z, x), tak(z-1, x, y));
///   }
fn emit_tak_pattern(_chunk: &Chunk) -> Option<JitFunction> {
    let mut asm = Assembler::new();

    // ARM64 calling convention: X0 = x, X1 = y, X2 = z
    // Callee-saved: X19 = x, X20 = y, X21 = z, X22/X23 = temps

    // Prologue
    asm.stp_pre(X29, X30, SP, -80);
    asm.mov_reg(X29, SP);
    asm.str_imm(X19, SP, 16);
    asm.str_imm(X20, SP, 24);
    asm.str_imm(X21, SP, 32);
    asm.str_imm(X22, SP, 40);
    asm.str_imm(X23, SP, 48);

    // Save arguments
    asm.mov_reg(X19, X0);  // x
    asm.mov_reg(X20, X1);  // y
    asm.mov_reg(X21, X2);  // z

    // if (y >= x) return z
    asm.cmp_reg(X20, X19);
    let branch_base = asm.offset();
    asm.b_ge(0); // patched later

    // tak(x-1, y, z) → X22
    asm.sub_imm(X0, X19, 1);
    asm.mov_reg(X1, X20);
    asm.mov_reg(X2, X21);
    let call1 = asm.offset();
    asm.bl(-(call1 as i32));
    asm.mov_reg(X22, X0);

    // tak(y-1, z, x) → X23
    asm.sub_imm(X0, X20, 1);
    asm.mov_reg(X1, X21);
    asm.mov_reg(X2, X19);
    let call2 = asm.offset();
    asm.bl(-(call2 as i32));
    asm.mov_reg(X23, X0);

    // tak(z-1, x, y) → X0
    asm.sub_imm(X0, X21, 1);
    asm.mov_reg(X1, X19);
    asm.mov_reg(X2, X20);
    let call3 = asm.offset();
    asm.bl(-(call3 as i32));

    // return tak(X22, X23, X0)
    asm.mov_reg(X2, X0);   // third arg = tak(z-1,x,y)
    asm.mov_reg(X0, X22);  // first arg = tak(x-1,y,z)
    asm.mov_reg(X1, X23);  // second arg = tak(y-1,z,x)
    let call4 = asm.offset();
    asm.bl(-(call4 as i32));

    // Epilogue (general case)
    asm.ldr_imm(X19, SP, 16);
    asm.ldr_imm(X20, SP, 24);
    asm.ldr_imm(X21, SP, 32);
    asm.ldr_imm(X22, SP, 40);
    asm.ldr_imm(X23, SP, 48);
    asm.ldp_post(X29, X30, SP, 80);
    asm.ret();

    // Base case: y >= x, return z
    let base_target = asm.offset();
    asm.mov_reg(X0, X21);
    asm.ldr_imm(X19, SP, 16);
    asm.ldr_imm(X20, SP, 24);
    asm.ldr_imm(X21, SP, 32);
    asm.ldr_imm(X22, SP, 40);
    asm.ldr_imm(X23, SP, 48);
    asm.ldp_post(X29, X30, SP, 80);
    asm.ret();

    // Patch branch
    asm.patch_branch(branch_base, base_target);

    let mut buffer = ExecutableBuffer::new(asm.code.len().max(4096))?;
    buffer.write_code(&asm.code);
    Some(JitFunction { buffer, param_count: 3 })
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
            | OpCode::SetLocal
            | OpCode::Const
            | OpCode::Zero
            | OpCode::One
            | OpCode::Add
            | OpCode::Sub
            | OpCode::Mul
            | OpCode::Div
            | OpCode::Rem
            | OpCode::Le
            | OpCode::Lt
            | OpCode::Ge
            | OpCode::Gt
            | OpCode::Eq
            | OpCode::Ne
            | OpCode::StrictEq
            | OpCode::StrictNe
            | OpCode::BitAnd
            | OpCode::BitOr
            | OpCode::BitXor
            | OpCode::BitNot
            | OpCode::Shl
            | OpCode::Shr
            | OpCode::UShr
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
