/// Minimal x86-64 assembler. Emits raw machine code bytes.
///
/// Only handles the instructions needed to JIT-compile simple numeric JS
/// functions. Targets Linux System V AMD64 ABI.
///
/// Register constants match the AMD64 encoding (0=RAX … 15=R15).
/// All assembler methods accept u32 register numbers (like arm64.rs) for
/// a uniform interface with the compiler layer, but treat values as u8 internally.

// Integer register constants (AMD64 encoding)
pub const RAX: u32 = 0;  // return value / scratch
pub const RCX: u32 = 1;  // shift-count register (reserved, not allocated to locals)
pub const RDX: u32 = 2;  // arg3 / caller-saved
pub const RBX: u32 = 3;  // callee-saved local #4
pub const RSP: u32 = 4;  // stack pointer
pub const RBP: u32 = 5;  // frame pointer
pub const RSI: u32 = 6;  // arg2 / caller-saved local #5
pub const RDI: u32 = 7;  // arg1 (entry) / caller-saved local #6 (after prologue)
pub const R8:  u32 = 8;  // arg5 / caller-saved local
pub const R9:  u32 = 9;  // arg6 / caller-saved local
pub const R10: u32 = 10; // caller-saved
pub const R11: u32 = 11; // caller-saved / IDIV scratch
pub const R12: u32 = 12; // callee-saved local #0
pub const R13: u32 = 13; // callee-saved local #1
pub const R14: u32 = 14; // callee-saved local #2
pub const R15: u32 = 15; // callee-saved local #3 / globals pointer

// XMM register constants (same encoding 0..15)
pub const XMM0:  u32 = 0;
pub const XMM1:  u32 = 1;
pub const XMM2:  u32 = 2;
pub const XMM3:  u32 = 3;
pub const XMM4:  u32 = 4;
pub const XMM5:  u32 = 5;
pub const XMM6:  u32 = 6;
pub const XMM7:  u32 = 7;
pub const XMM8:  u32 = 8;
pub const XMM9:  u32 = 9;
pub const XMM10: u32 = 10;
pub const XMM11: u32 = 11;
pub const XMM12: u32 = 12;
pub const XMM13: u32 = 13;
pub const XMM14: u32 = 14;
pub const XMM15: u32 = 15;

pub struct Assembler {
    pub code: Vec<u8>,
}

impl Default for Assembler {
    fn default() -> Self { Self::new() }
}

// ---- Internal encoding helpers ----

/// REX prefix for 64-bit integer ops: REX.W + optional REX.R (reg field) + REX.B (rm field).
#[inline]
fn rex_rb(reg: u8, rm: u8) -> u8 {
    0x48 | ((reg >> 3) << 2) | (rm >> 3)
}

/// ModRM byte for register-to-register (mod=11).
#[inline]
fn modrm_rr(reg: u8, rm: u8) -> u8 {
    0xC0 | ((reg & 7) << 3) | (rm & 7)
}

impl Assembler {
    pub fn new() -> Self {
        Self { code: Vec::with_capacity(1024) }
    }

    pub fn offset(&self) -> usize { self.code.len() }

    // ---- Stack / frame ----

    pub fn push_reg(&mut self, r: u32) {
        let r = r as u8;
        if r >= 8 { self.code.push(0x41); }
        self.code.push(0x50 + (r & 7));
    }

    pub fn pop_reg(&mut self, r: u32) {
        let r = r as u8;
        if r >= 8 { self.code.push(0x41); }
        self.code.push(0x58 + (r & 7));
    }

    /// SUB RSP, imm8  (used for stack alignment)
    pub fn sub_rsp_imm8(&mut self, imm: u8) {
        self.code.extend_from_slice(&[0x48, 0x83, 0xEC, imm]);
    }

    /// ADD RSP, imm8  (undo stack alignment)
    pub fn add_rsp_imm8(&mut self, imm: u8) {
        self.code.extend_from_slice(&[0x48, 0x83, 0xC4, imm]);
    }

    /// MOV RBP, RSP
    pub fn mov_rbp_rsp(&mut self) {
        self.code.extend_from_slice(&[0x48, 0x89, 0xE5]);
    }

    // ---- Move ----

    /// MOV dst, src  (64-bit register)
    pub fn mov_reg(&mut self, dst: u32, src: u32) {
        let (d, s) = (dst as u8, src as u8);
        self.code.extend_from_slice(&[rex_rb(d, s), 0x8B, modrm_rr(d, s)]);
    }

    /// MOV dst, imm64  (full 64-bit constant)
    pub fn mov_imm64(&mut self, dst: u32, value: u64) {
        let d = dst as u8;
        // MOV r/m64, imm32 (sign-extended) when value fits
        if value as i64 >= i32::MIN as i64 && value as i64 <= i32::MAX as i64 {
            let rex = 0x48 | (d >> 3);
            self.code.extend_from_slice(&[rex, 0xC7, 0xC0 | (d & 7)]);
            self.code.extend_from_slice(&(value as i32).to_le_bytes());
        } else {
            // Full REX.W B8+rd imm64
            let rex = 0x48 | (d >> 3);
            self.code.push(rex);
            self.code.push(0xB8 + (d & 7));
            self.code.extend_from_slice(&value.to_le_bytes());
        }
    }

    /// Equivalent of ARM64 MOVZ: zero-extend imm16 into dst (uses 32-bit MOV).
    pub fn movz(&mut self, dst: u32, imm16: u16) {
        let d = dst as u8;
        // MOV r32, imm32 — no REX (zero-extends to 64-bit)
        if d >= 8 { self.code.push(0x41); }
        self.code.push(0xB8 + (d & 7));
        self.code.extend_from_slice(&(imm16 as u32).to_le_bytes());
    }

    // ---- Arithmetic ----

    /// ADD dst, rn, src  (dst = rn + src; if dst==rn this is dst += src)
    pub fn add_reg(&mut self, dst: u32, rn: u32, src: u32) {
        if dst != rn { self.mov_reg(dst, rn); }
        let (d, s) = (dst as u8, src as u8);
        self.code.extend_from_slice(&[rex_rb(d, s), 0x03, modrm_rr(d, s)]);
    }

    /// ADD dst, rn, imm  (dst = rn + imm)
    pub fn add_imm(&mut self, dst: u32, rn: u32, imm: u32) {
        if dst != rn { self.mov_reg(dst, rn); }
        self.emit_alu_imm(0, dst as u8, imm as i64);
    }

    /// SUB dst, rn, src  (dst = rn - src)
    pub fn sub_reg(&mut self, dst: u32, rn: u32, src: u32) {
        if dst != rn { self.mov_reg(dst, rn); }
        let (d, s) = (dst as u8, src as u8);
        self.code.extend_from_slice(&[rex_rb(d, s), 0x2B, modrm_rr(d, s)]);
    }

    /// SUB dst, rn, imm  (dst = rn - imm)
    pub fn sub_imm(&mut self, dst: u32, rn: u32, imm: u32) {
        if dst != rn { self.mov_reg(dst, rn); }
        self.emit_alu_imm(5, dst as u8, imm as i64);
    }

    /// IMUL dst, src  (dst = dst * src, signed 64-bit)
    pub fn mul(&mut self, dst: u32, _rn: u32, src: u32) {
        // ARM64 mul(rd, rn, rm) — in x86-64 we do dst = dst * src (rn must equal dst)
        let (d, s) = (dst as u8, src as u8);
        self.code.extend_from_slice(&[rex_rb(d, s), 0x0F, 0xAF, modrm_rr(d, s)]);
    }

    /// IDIV: dst = dst / src  (signed integer division, result is quotient)
    pub fn sdiv(&mut self, dst: u32, _rn: u32, src: u32) {
        self.idiv_helper(dst as u8, src as u8, true);
    }

    /// Modulo: dst = dst % src  (via IDIV, result is remainder)
    pub fn rem(&mut self, dst: u32, src: u32) {
        self.idiv_helper(dst as u8, src as u8, false);
    }

    /// NEG dst  (dst = -dst)
    pub fn neg_reg(&mut self, dst: u32) {
        let d = dst as u8;
        self.code.extend_from_slice(&[0x48 | (d >> 3), 0xF7, 0xC0 | (3 << 3) | (d & 7)]);
    }

    /// INC dst  (dst += 1, via add_imm)
    pub fn inc_reg(&mut self, dst: u32) {
        self.add_imm(dst, dst, 1);
    }

    /// DEC dst  (dst -= 1, via sub_imm)
    pub fn dec_reg(&mut self, dst: u32) {
        self.sub_imm(dst, dst, 1);
    }

    // ---- Bitwise ----

    /// AND dst, rn, src  (dst = rn & src)
    pub fn and_reg(&mut self, dst: u32, rn: u32, src: u32) {
        if dst != rn { self.mov_reg(dst, rn); }
        let (d, s) = (dst as u8, src as u8);
        self.code.extend_from_slice(&[rex_rb(s, d), 0x21, modrm_rr(s, d)]);
    }

    /// OR dst, rn, src  (dst = rn | src)
    pub fn orr_reg(&mut self, dst: u32, rn: u32, src: u32) {
        if dst != rn { self.mov_reg(dst, rn); }
        let (d, s) = (dst as u8, src as u8);
        self.code.extend_from_slice(&[rex_rb(s, d), 0x09, modrm_rr(s, d)]);
    }

    /// XOR dst, rn, src  (dst = rn ^ src)
    pub fn eor_reg(&mut self, dst: u32, rn: u32, src: u32) {
        if dst != rn { self.mov_reg(dst, rn); }
        let (d, s) = (dst as u8, src as u8);
        self.code.extend_from_slice(&[rex_rb(s, d), 0x31, modrm_rr(s, d)]);
    }

    /// NOT dst  (bitwise complement)
    pub fn mvn(&mut self, dst: u32, src: u32) {
        // For one-operand NOT, src must equal dst (ARM64 mvn uses src, not XZR+src here)
        let _ = src;
        let d = dst as u8;
        self.code.extend_from_slice(&[0x48 | (d >> 3), 0xF7, 0xC0 | (2 << 3) | (d & 7)]);
    }

    /// SHL dst, cnt  (logical left shift by register)
    pub fn lsl_reg(&mut self, dst: u32, _rn: u32, cnt: u32) {
        self.shift_by_reg(dst as u8, cnt as u8, 4);
    }

    /// SHR dst, cnt  (logical right shift by register)
    pub fn lsr_reg(&mut self, dst: u32, _rn: u32, cnt: u32) {
        self.shift_by_reg(dst as u8, cnt as u8, 5);
    }

    /// SAR dst, cnt  (arithmetic right shift by register)
    pub fn asr_reg(&mut self, dst: u32, _rn: u32, cnt: u32) {
        self.shift_by_reg(dst as u8, cnt as u8, 7);
    }

    // ---- Comparison ----

    /// CMP ra, rb  (set flags from ra - rb)
    pub fn cmp_reg(&mut self, ra: u32, rb: u32) {
        let (a, b) = (ra as u8, rb as u8);
        // CMP r64, r/m64 (0x3B): reg=ra, rm=rb
        self.code.extend_from_slice(&[rex_rb(a, b), 0x3B, modrm_rr(a, b)]);
    }

    /// CMP r, imm  (set flags from r - imm)
    pub fn cmp_imm(&mut self, r: u32, imm: u32) {
        self.emit_alu_imm(7, r as u8, imm as i64);
    }

    /// TEST r, r  (set ZF if r == 0; equivalent to ARM64 CBZ guard)
    pub fn test_reg(&mut self, r: u32) {
        let r = r as u8;
        self.code.extend_from_slice(&[rex_rb(r, r), 0x85, modrm_rr(r, r)]);
    }

    // ---- Branches ----

    /// JMP rel32  (byte_offset = distance from THIS instruction start to target)
    pub fn b(&mut self, byte_offset: i32) {
        // JMP is 5 bytes; rel32 = target - (ip + 5) = byte_offset - 5
        self.code.push(0xE9);
        self.code.extend_from_slice(&(byte_offset - 5).to_le_bytes());
    }

    /// JE rel32  (jump if equal / zero)
    pub fn b_eq(&mut self, byte_offset: i32) { self.jcc(0x84, byte_offset); }
    /// JNE rel32
    pub fn b_ne(&mut self, byte_offset: i32) { self.jcc(0x85, byte_offset); }
    /// JL rel32  (signed less-than)
    pub fn b_lt(&mut self, byte_offset: i32) { self.jcc(0x8C, byte_offset); }
    /// JGE rel32  (signed greater-or-equal)
    pub fn b_ge(&mut self, byte_offset: i32) { self.jcc(0x8D, byte_offset); }
    /// JLE rel32  (signed less-or-equal)
    pub fn b_le(&mut self, byte_offset: i32) { self.jcc(0x8E, byte_offset); }
    /// JG rel32  (signed greater-than)
    pub fn b_gt(&mut self, byte_offset: i32) { self.jcc(0x8F, byte_offset); }

    /// JZ rel32  (same as JE — used for non-fused CBZ equivalent)
    pub fn cbz(&mut self, r: u32, byte_offset: i32) {
        self.test_reg(r);
        self.jcc(0x84, byte_offset);
    }

    /// JNZ rel32  (same as JNE — CBNZ equivalent)
    pub fn cbnz(&mut self, r: u32, byte_offset: i32) {
        self.test_reg(r);
        self.jcc(0x85, byte_offset);
    }

    /// CALL rel32  (byte_offset = distance from THIS instruction start to target)
    pub fn bl(&mut self, byte_offset: i32) {
        // CALL is 5 bytes; rel32 = target - (ip + 5) = byte_offset - 5
        self.code.push(0xE8);
        self.code.extend_from_slice(&(byte_offset - 5).to_le_bytes());
    }

    /// RET
    pub fn ret(&mut self) {
        self.code.push(0xC3);
    }

    // ---- FP branches after UCOMISD ----
    // UCOMISD sets CF/ZF/PF. For JumpIfFalse we emit the INVERSE condition:
    //   after Lt  (a<b):  inverse = a>=b = JAE (CF=0)       → 0x83
    //   after Le  (a<=b): inverse = a>b  = JA  (CF=0,ZF=0)  → 0x87
    //   after Gt  (a>b):  inverse = a<=b = JBE (CF=1|ZF=1)  → 0x86
    //   after Ge  (a>=b): inverse = a<b  = JB  (CF=1)       → 0x82
    //   after Eq:         inverse = a!=b = JNE (ZF=0)       → 0x85
    //   after Ne:         inverse = a==b = JE  (ZF=1)       → 0x84

    pub fn b_jae(&mut self, byte_offset: i32) { self.jcc(0x83, byte_offset); } // JAE (fp >=)
    pub fn b_ja(&mut self, byte_offset: i32)  { self.jcc(0x87, byte_offset); } // JA  (fp >)
    pub fn b_jbe(&mut self, byte_offset: i32) { self.jcc(0x86, byte_offset); } // JBE (fp <=)
    pub fn b_jb(&mut self, byte_offset: i32)  { self.jcc(0x82, byte_offset); } // JB  (fp <)

    // ---- Floating-point (XMM/SSE2) ----

    /// ADDSD dst, src  (dst += src, f64)
    pub fn fadd(&mut self, dst: u32, _rn: u32, src: u32) {
        self.sse2_op(0xF2, 0x58, dst as u8, src as u8);
    }

    /// SUBSD dst, src  (dst -= src, f64)
    pub fn fsub(&mut self, dst: u32, _rn: u32, src: u32) {
        self.sse2_op(0xF2, 0x5C, dst as u8, src as u8);
    }

    /// MULSD dst, src  (dst *= src, f64)
    pub fn fmul(&mut self, dst: u32, _rn: u32, src: u32) {
        self.sse2_op(0xF2, 0x59, dst as u8, src as u8);
    }

    /// DIVSD dst, src  (dst /= src, f64)
    pub fn fdiv(&mut self, dst: u32, _rn: u32, src: u32) {
        self.sse2_op(0xF2, 0x5E, dst as u8, src as u8);
    }

    /// UCOMISD a, b  (compare f64 registers, sets CF/ZF/PF)
    pub fn fcmp(&mut self, a: u32, b: u32) {
        let (a, b) = (a as u8, b as u8);
        self.code.push(0x66);
        if a >= 8 || b >= 8 {
            self.code.push(0x40 | ((a >> 3) << 2) | (b >> 3));
        }
        self.code.extend_from_slice(&[0x0F, 0x2E, 0xC0 | ((a & 7) << 3) | (b & 7)]);
    }

    /// CVTSI2SD xmm_dst, r64_src  (signed int64 → f64)
    pub fn scvtf(&mut self, dst_xmm: u32, src_r64: u32) {
        let (d, s) = (dst_xmm as u8, src_r64 as u8);
        self.code.push(0xF2);
        self.code.push(0x48 | ((d >> 3) << 2) | (s >> 3)); // REX.W + R + B
        self.code.extend_from_slice(&[0x0F, 0x2A, 0xC0 | ((d & 7) << 3) | (s & 7)]);
    }

    /// CVTTSD2SI r64_dst, xmm_src  (f64 → signed int64, truncate toward zero)
    pub fn fcvtzs(&mut self, dst_r64: u32, src_xmm: u32) {
        let (d, s) = (dst_r64 as u8, src_xmm as u8);
        self.code.push(0xF2);
        self.code.push(0x48 | ((d >> 3) << 2) | (s >> 3));
        self.code.extend_from_slice(&[0x0F, 0x2C, 0xC0 | ((d & 7) << 3) | (s & 7)]);
    }

    /// MOVQ xmm_dst, r64_src  (move integer bits into XMM register)
    pub fn fmov_to_fp(&mut self, dst_xmm: u32, src_r64: u32) {
        let (d, s) = (dst_xmm as u8, src_r64 as u8);
        self.code.push(0x66);
        self.code.push(0x48 | ((d >> 3) << 2) | (s >> 3));
        self.code.extend_from_slice(&[0x0F, 0x6E, 0xC0 | ((d & 7) << 3) | (s & 7)]);
    }

    /// MOVQ r64_dst, xmm_src  (move XMM bits to integer register)
    pub fn fmov_from_fp(&mut self, dst_r64: u32, src_xmm: u32) {
        let (d, s) = (dst_r64 as u8, src_xmm as u8);
        self.code.push(0x66);
        self.code.push(0x48 | ((d >> 3) << 2) | (s >> 3));
        self.code.extend_from_slice(&[0x0F, 0x7E, 0xC0 | ((d & 7) << 3) | (s & 7)]);
    }

    /// MOVSD dst_xmm, src_xmm  (copy f64 register)
    pub fn fmov_reg(&mut self, dst: u32, src: u32) {
        let (d, s) = (dst as u8, src as u8);
        self.code.push(0xF2);
        if d >= 8 || s >= 8 {
            self.code.push(0x40 | ((d >> 3) << 2) | (s >> 3));
        }
        self.code.extend_from_slice(&[0x0F, 0x10, 0xC0 | ((d & 7) << 3) | (s & 7)]);
    }

    /// Store f64 to memory: MOV [base + offset], xmm  (used for FP saves — not needed in loop JIT)
    pub fn str_fp(&mut self, _rt: u32, _base: u32, _imm: u32) {
        // No-op placeholder: x86-64 loop FP JIT saves no XMM regs (all caller-saved)
    }

    /// Load f64 from memory: MOV xmm, [base + offset]  (ditto)
    pub fn ldr_fp(&mut self, _rt: u32, _base: u32, _imm: u32) {
        // No-op placeholder
    }

    // ---- Memory (load/store for globals buffer) ----

    /// MOV dst, [base + disp]  (64-bit load)
    pub fn ldr_imm(&mut self, dst: u32, base: u32, disp: u32) {
        let (d, b) = (dst as u8, base as u8);
        self.mem_op(0x8B, d, b, disp as i32);
    }

    /// MOV [base + disp], src  (64-bit store)
    pub fn str_imm(&mut self, src: u32, base: u32, disp: u32) {
        let (s, b) = (src as u8, base as u8);
        // STR uses opcode 0x89 with reg=src, rm=base+disp
        self.mem_op_store(s, b, disp as i32);
    }

    // ---- SXTW stub (not needed on x86-64, values are natively 64-bit) ----
    pub fn sxtw(&mut self, _dst: u32, _src: u32) {}

    // ---- Patch helpers ----

    pub fn patch_branch(&mut self, branch_offset: usize, target_offset: usize) {
        let opcode0 = self.code[branch_offset];
        let (rel32_off, insn_len): (usize, i32) = if opcode0 == 0xE9 || opcode0 == 0xE8 {
            (branch_offset + 1, 5)  // JMP/CALL rel32
        } else if opcode0 == 0x0F {
            (branch_offset + 2, 6)  // Jcc rel32 (two-byte opcode)
        } else if opcode0 == 0x48 || opcode0 == 0x49 || opcode0 == 0x4C || opcode0 == 0x4D {
            // TEST + JCC: the branch itself is the Jcc that follows TEST
            // This case is handled by cbz/cbnz which emit TEST then JCC;
            // the patch offset should point at the JCC, not the TEST.
            // If caller passes the offset of TEST, skip it (TEST is 3 bytes).
            let jcc_off = branch_offset + 3;
            let rel32_off = jcc_off + 2;
            let rel32 = (target_offset as i32) - (jcc_off as i32 + 6);
            self.code[rel32_off..rel32_off + 4].copy_from_slice(&rel32.to_le_bytes());
            return;
        } else {
            return;
        };
        let rel32 = (target_offset as i32) - (branch_offset as i32 + insn_len);
        self.code[rel32_off..rel32_off + 4].copy_from_slice(&rel32.to_le_bytes());
    }

    // ---- ARM64 compatibility stubs (not emitted on x86-64) ----

    pub fn stp_pre(&mut self, _rt1: u32, _rt2: u32, _rn: u32, _imm7: i32) {}
    pub fn ldp_post(&mut self, _rt1: u32, _rt2: u32, _rn: u32, _imm7: i32) {}

    // ---- Private helpers ----

    fn jcc(&mut self, cond: u8, byte_offset: i32) {
        self.code.extend_from_slice(&[0x0F, cond]);
        self.code.extend_from_slice(&byte_offset.to_le_bytes());
    }

    fn emit_alu_imm(&mut self, opext: u8, dst: u8, imm: i64) {
        let rex = 0x48 | (dst >> 3);
        let modrm = 0xC0 | (opext << 3) | (dst & 7);
        if imm >= -128 && imm <= 127 {
            self.code.extend_from_slice(&[rex, 0x83, modrm, imm as u8]);
        } else {
            self.code.extend_from_slice(&[rex, 0x81, modrm]);
            self.code.extend_from_slice(&(imm as i32).to_le_bytes());
        }
    }

    fn sse2_op(&mut self, prefix: u8, opcode: u8, dst: u8, src: u8) {
        self.code.push(prefix);
        if dst >= 8 || src >= 8 {
            self.code.push(0x40 | ((dst >> 3) << 2) | (src >> 3));
        }
        self.code.extend_from_slice(&[0x0F, opcode, 0xC0 | ((dst & 7) << 3) | (src & 7)]);
    }

    fn shift_by_reg(&mut self, dst: u8, cnt: u8, opext: u8) {
        // Move shift count to RCX (only if not already there)
        if cnt as u32 != RCX {
            self.mov_reg(RCX, cnt as u32);
        }
        let rex = 0x48 | (dst >> 3);
        self.code.extend_from_slice(&[rex, 0xD3, 0xC0 | (opext << 3) | (dst & 7)]);
    }

    fn idiv_helper(&mut self, ra: u8, rb: u8, want_quotient: bool) {
        // Choose scratch register: R11 unless ra==R11, then use RCX
        if ra == R11 as u8 {
            // Save divisor in RCX (never allocated as local)
            self.mov_reg(RCX, rb as u32);
            self.mov_reg(RAX, ra as u32);
            self.code.extend_from_slice(&[0x48, 0x99]);        // CQO
            self.code.extend_from_slice(&[0x48, 0xF7, 0xF9]); // IDIV RCX
        } else {
            self.mov_reg(R11, rb as u32);
            self.mov_reg(RAX, ra as u32);
            self.code.extend_from_slice(&[0x48, 0x99]);        // CQO
            self.code.extend_from_slice(&[0x49, 0xF7, 0xFB]); // IDIV R11
        }
        if want_quotient {
            self.mov_reg(ra as u32, RAX);
        } else {
            self.mov_reg(ra as u32, RDX);
        }
    }

    fn mem_op(&mut self, opcode: u8, reg: u8, base: u8, disp: i32) {
        let rex = 0x48 | ((reg >> 3) << 2) | (base >> 3);
        let base_enc = base & 7;

        // Base=RSP(4) or R12(4+REX.B) requires SIB byte
        if base_enc == 4 {
            let modrm = 0x80 | ((reg & 7) << 3) | 4; // mod=10, SIB follows
            let sib = 0x24; // scale=0, no index, base=RSP/R12
            self.code.extend_from_slice(&[rex, opcode, modrm, sib]);
            self.code.extend_from_slice(&disp.to_le_bytes());
            return;
        }

        // Base=RBP(5) or R13(5+REX.B) with mod=00 means RIP-relative; force mod=10
        let modrm = 0x80 | ((reg & 7) << 3) | base_enc; // mod=10
        self.code.extend_from_slice(&[rex, opcode, modrm]);
        self.code.extend_from_slice(&disp.to_le_bytes());
    }

    fn mem_op_store(&mut self, src: u8, base: u8, disp: i32) {
        // MOV [base+disp], src: opcode 0x89, reg=src, rm=base+disp
        let rex = 0x48 | ((src >> 3) << 2) | (base >> 3);
        let base_enc = base & 7;

        if base_enc == 4 {
            let modrm = 0x80 | ((src & 7) << 3) | 4;
            let sib = 0x24;
            self.code.extend_from_slice(&[rex, 0x89, modrm, sib]);
            self.code.extend_from_slice(&disp.to_le_bytes());
            return;
        }
        let modrm = 0x80 | ((src & 7) << 3) | base_enc;
        self.code.extend_from_slice(&[rex, 0x89, modrm]);
        self.code.extend_from_slice(&disp.to_le_bytes());
    }
}
