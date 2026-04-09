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
    // Pre-scan: check if all opcodes are JIT-able
    if !can_jit(chunk) {
        return None;
    }

    let mut asm = Assembler::new();

    // ---- Function prologue ----
    // Save callee-saved registers and link register
    // STP X29, X30, [SP, #-48]!   (save frame pointer + return address)
    asm.stp_pre(X29, X30, SP, -48);
    // MOV X29, SP
    asm.mov_reg(X29, SP);
    // Save callee-saved registers we'll use
    // STP X19, X20, [SP, #16]
    asm.str_imm(X19, SP, 16);
    asm.str_imm(X20, SP, 24);

    // X0 = first argument (the JS function's parameter)
    // Save it in X19 (callee-saved)
    asm.mov_reg(X19, X0);

    // Map: local slot 0 = X19 (the parameter)
    // We'll use X0-X3 as scratch, X19 as param, X20 as temp for fib(n-1)

    // ---- Compile bytecode ----
    let mut ip = 0;
    let code = &chunk.code;
    let constants = &chunk.constants;

    // Track jump patch locations
    let mut jump_patches: Vec<(usize, usize)> = Vec::new(); // (asm_offset, bytecode_target)
    let mut bc_to_asm: Vec<usize> = vec![0; code.len() + 1]; // bytecode offset -> asm offset

    // First pass: compile
    while ip < code.len() {
        bc_to_asm[ip] = asm.offset();

        let op = OpCode::from_byte(code[ip])?;
        ip += 1;

        match op {
            OpCode::GetLocal => {
                let slot = code[ip] as u32;
                ip += 1;
                // For now, only slot 0 (parameter) is supported, mapped to X19
                if slot == 0 {
                    asm.mov_reg(X0, X19);
                    // Push to our "virtual stack" by storing in memory
                    // We use [SP+32] onwards as our operand stack
                    // Actually, for simple functions, just track in registers
                } else {
                    return None; // Can't handle multiple locals yet
                }
            }

            OpCode::Const => {
                let idx = ((code[ip] as u16) << 8 | code[ip + 1] as u16) as usize;
                ip += 2;
                let val = constants[idx];
                // Get the integer value
                if let Some(i) = val.as_int() {
                    asm.movz(X1, i as u16);
                    if i < 0 {
                        // For negative numbers, use SUB from zero
                        asm.movz(X1, (-i) as u16);
                        asm.sub_reg(X1, XZR, X1); // negate
                    }
                } else if let Some(f) = val.as_number() {
                    let i = f as i64;
                    if (0..=0xFFFF).contains(&i) {
                        asm.movz(X1, i as u16);
                    } else {
                        return None; // Can't handle large constants
                    }
                } else {
                    return None;
                }
            }

            OpCode::Zero => {
                asm.movz(X1, 0);
            }

            OpCode::One => {
                asm.movz(X1, 1);
            }

            OpCode::Add => {
                // X0 = X0 + X1 (assumes left in X0, right in X1)
                asm.add_reg(X0, X0, X1);
            }

            OpCode::Sub => {
                asm.sub_reg(X0, X0, X1);
            }

            OpCode::Mul => {
                asm.mul(X0, X0, X1);
            }

            OpCode::Le => {
                // CMP X0, X1 — flags are set, used by next JumpIfFalse
                asm.cmp_reg(X0, X1);
            }

            OpCode::Lt => {
                asm.cmp_reg(X0, X1);
            }

            OpCode::Ge => {
                asm.cmp_reg(X0, X1);
            }

            OpCode::Gt => {
                asm.cmp_reg(X0, X1);
            }

            OpCode::JumpIfFalse => {
                let offset = ((code[ip] as i16) << 8 | code[ip + 1] as i16) as i32;
                ip += 2;
                let target_bc = (ip as i32 + offset) as usize;

                // Emit a placeholder branch — we'll patch the target later
                let branch_offset = asm.offset();
                // For Le: JumpIfFalse means "jump if NOT le" = "jump if GT"
                // We need to invert: if the test was Le and JumpIfFalse,
                // we branch when GT (the condition is false)
                asm.b_gt(0); // placeholder — will be patched
                jump_patches.push((branch_offset, target_bc));
            }

            OpCode::Jump => {
                let offset = ((code[ip] as i16) << 8 | code[ip + 1] as i16) as i32;
                ip += 2;
                let target_bc = (ip as i32 + offset) as usize;
                let branch_offset = asm.offset();
                asm.b(0); // placeholder
                jump_patches.push((branch_offset, target_bc));
            }

            OpCode::Return => {
                // X0 already has the return value
                // Epilogue
                asm.ldr_imm(X19, SP, 16);
                asm.ldr_imm(X20, SP, 24);
                asm.ldp_post(X29, X30, SP, 48);
                asm.ret();
            }

            OpCode::GetGlobal => {
                // This is used for recursive calls: GetGlobal("fib")
                // Skip the operand — we'll handle Call next
                ip += 2;
                // The function to call is... ourselves! We'll use BL to self.
            }

            OpCode::Call => {
                let _argc = code[ip];
                ip += 1;
                // Recursive call to ourselves.
                // X0 already has the argument (from the Sub before this)
                // BL to the start of our function (offset 0 from our code start)
                let call_offset = asm.offset();
                asm.bl(-(call_offset as i32)); // jump back to start
            }

            OpCode::Pop => {
                // No-op for register-based JIT
            }

            OpCode::Halt => {
                // Same as Return for JIT
                asm.ldr_imm(X19, SP, 16);
                asm.ldr_imm(X20, SP, 24);
                asm.ldp_post(X29, X30, SP, 48);
                asm.ret();
            }

            _ => return None, // Unsupported opcode
        }
    }
    bc_to_asm[ip] = asm.offset();

    // ---- Patch jumps ----
    for (asm_offset, bc_target) in &jump_patches {
        let target_asm = bc_to_asm[*bc_target];
        asm.patch_branch(*asm_offset, target_asm);
    }

    // ---- Allocate executable memory and write code ----
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
            _ => return false,
        }
        ip += op.instruction_size();
    }
    true
}
