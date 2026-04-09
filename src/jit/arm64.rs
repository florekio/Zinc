/// Minimal ARM64 assembler. Emits raw 32-bit instruction words.
///
/// This is NOT a general-purpose assembler — it only handles the instructions
/// needed to JIT-compile simple numeric JS functions (fibonacci, loops, math).
// ARM64 register names (for readability)
pub const X0: u32 = 0;
pub const X1: u32 = 1;
pub const X2: u32 = 2;
pub const X19: u32 = 19; // callee-saved, used for local 'n'
pub const X20: u32 = 20; // callee-saved, used for temps
pub const X29: u32 = 29; // frame pointer
pub const X30: u32 = 30; // link register (return address)
pub const SP: u32 = 31;  // stack pointer (in some encodings)
pub const XZR: u32 = 31; // zero register (in other encodings)

pub struct Assembler {
    pub code: Vec<u8>,
}

impl Default for Assembler {
    fn default() -> Self {
        Self::new()
    }
}

impl Assembler {
    pub fn new() -> Self {
        Self {
            code: Vec::with_capacity(1024),
        }
    }

    /// Emit a raw 32-bit instruction (little-endian).
    fn emit(&mut self, inst: u32) {
        self.code.extend_from_slice(&inst.to_le_bytes());
    }

    /// Current offset in bytes (for jump patching).
    pub fn offset(&self) -> usize {
        self.code.len()
    }

    // ---- Arithmetic ----

    /// ADD Xd, Xn, Xm
    pub fn add_reg(&mut self, rd: u32, rn: u32, rm: u32) {
        self.emit(0x8B000000 | (rm << 16) | (rn << 5) | rd);
    }

    /// ADD Xd, Xn, #imm12
    pub fn add_imm(&mut self, rd: u32, rn: u32, imm12: u32) {
        self.emit(0x91000000 | ((imm12 & 0xFFF) << 10) | (rn << 5) | rd);
    }

    /// SUB Xd, Xn, Xm
    pub fn sub_reg(&mut self, rd: u32, rn: u32, rm: u32) {
        self.emit(0xCB000000 | (rm << 16) | (rn << 5) | rd);
    }

    /// SUB Xd, Xn, #imm12
    pub fn sub_imm(&mut self, rd: u32, rn: u32, imm12: u32) {
        self.emit(0xD1000000 | ((imm12 & 0xFFF) << 10) | (rn << 5) | rd);
    }

    /// MUL Xd, Xn, Xm
    pub fn mul(&mut self, rd: u32, rn: u32, rm: u32) {
        self.emit(0x9B007C00 | (rm << 16) | (rn << 5) | rd);
    }

    /// SDIV Xd, Xn, Xm
    pub fn sdiv(&mut self, rd: u32, rn: u32, rm: u32) {
        self.emit(0x9AC00C00 | (rm << 16) | (rn << 5) | rd);
    }

    // ---- Comparison ----

    /// CMP Xn, Xm (alias for SUBS XZR, Xn, Xm)
    pub fn cmp_reg(&mut self, rn: u32, rm: u32) {
        self.emit(0xEB00001F | (rm << 16) | (rn << 5));
    }

    /// CMP Xn, #imm12
    pub fn cmp_imm(&mut self, rn: u32, imm12: u32) {
        self.emit(0xF100001F | ((imm12 & 0xFFF) << 10) | (rn << 5));
    }

    // ---- Branches ----

    /// B (unconditional branch, PC-relative)
    /// offset is in bytes, must be aligned to 4
    pub fn b(&mut self, byte_offset: i32) {
        let imm26 = ((byte_offset >> 2) as u32) & 0x3FFFFFF;
        self.emit(0x14000000 | imm26);
    }

    /// B.cond (conditional branch)
    /// Condition codes: EQ=0, NE=1, LT=11, GE=10, LE=13, GT=12
    pub fn b_cond(&mut self, cond: u32, byte_offset: i32) {
        let imm19 = ((byte_offset >> 2) as u32) & 0x7FFFF;
        self.emit(0x54000000 | (imm19 << 5) | cond);
    }

    /// B.EQ
    pub fn b_eq(&mut self, byte_offset: i32) { self.b_cond(0, byte_offset); }
    /// B.NE
    pub fn b_ne(&mut self, byte_offset: i32) { self.b_cond(1, byte_offset); }
    /// B.LT (signed less than)
    pub fn b_lt(&mut self, byte_offset: i32) { self.b_cond(11, byte_offset); }
    /// B.GE (signed greater or equal)
    pub fn b_ge(&mut self, byte_offset: i32) { self.b_cond(10, byte_offset); }
    /// B.LE (signed less or equal)
    pub fn b_le(&mut self, byte_offset: i32) { self.b_cond(13, byte_offset); }
    /// B.GT (signed greater than)
    pub fn b_gt(&mut self, byte_offset: i32) { self.b_cond(12, byte_offset); }

    /// BL (branch with link = function call)
    pub fn bl(&mut self, byte_offset: i32) {
        let imm26 = ((byte_offset >> 2) as u32) & 0x3FFFFFF;
        self.emit(0x94000000 | imm26);
    }

    /// BLR Xn (branch to register with link = indirect call)
    pub fn blr(&mut self, rn: u32) {
        self.emit(0xD63F0000 | (rn << 5));
    }

    /// RET (return to caller, via X30)
    pub fn ret(&mut self) {
        self.emit(0xD65F03C0);
    }

    // ---- Move ----

    /// MOV Xd, Xm (alias for ORR Xd, XZR, Xm)
    pub fn mov_reg(&mut self, rd: u32, rm: u32) {
        self.emit(0xAA0003E0 | (rm << 16) | rd);
    }

    /// MOV Xd, #imm16 (MOVZ — zero-extends, puts imm16 in bits 0-15)
    pub fn movz(&mut self, rd: u32, imm16: u16) {
        self.emit(0xD2800000 | ((imm16 as u32) << 5) | rd);
    }

    /// MOVK Xd, #imm16, LSL #shift (keep other bits, insert imm16 at position)
    pub fn movk(&mut self, rd: u32, imm16: u16, shift: u32) {
        let hw = shift / 16; // 0, 1, 2, or 3
        self.emit(0xF2800000 | (hw << 21) | ((imm16 as u32) << 5) | rd);
    }

    /// Load a full 64-bit immediate into Xd (uses movz + up to 3 movk)
    pub fn mov_imm64(&mut self, rd: u32, value: u64) {
        self.movz(rd, (value & 0xFFFF) as u16);
        if value > 0xFFFF {
            self.movk(rd, ((value >> 16) & 0xFFFF) as u16, 16);
        }
        if value > 0xFFFF_FFFF {
            self.movk(rd, ((value >> 32) & 0xFFFF) as u16, 32);
        }
        if value > 0xFFFF_FFFF_FFFF {
            self.movk(rd, ((value >> 48) & 0xFFFF) as u16, 48);
        }
    }

    // ---- Load/Store ----

    /// STP Xt1, Xt2, [Xn, #imm]! (pre-index store pair — push to stack)
    pub fn stp_pre(&mut self, rt1: u32, rt2: u32, rn: u32, imm7: i32) {
        let imm = ((imm7 / 8) as u32) & 0x7F;
        self.emit(0xA9800000 | (imm << 15) | (rt2 << 10) | (rn << 5) | rt1);
    }

    /// LDP Xt1, Xt2, [Xn], #imm (post-index load pair — pop from stack)
    pub fn ldp_post(&mut self, rt1: u32, rt2: u32, rn: u32, imm7: i32) {
        let imm = ((imm7 / 8) as u32) & 0x7F;
        self.emit(0xA8C00000 | (imm << 15) | (rt2 << 10) | (rn << 5) | rt1);
    }

    /// STR Xt, [Xn, #imm] (store register, unsigned offset)
    pub fn str_imm(&mut self, rt: u32, rn: u32, imm12: u32) {
        let scaled = imm12 / 8; // 8-byte aligned for 64-bit
        self.emit(0xF9000000 | (scaled << 10) | (rn << 5) | rt);
    }

    /// LDR Xt, [Xn, #imm] (load register, unsigned offset)
    pub fn ldr_imm(&mut self, rt: u32, rn: u32, imm12: u32) {
        let scaled = imm12 / 8;
        self.emit(0xF9400000 | (scaled << 10) | (rn << 5) | rt);
    }

    // ---- Patch helpers ----

    /// Patch a branch instruction at `offset` to jump to `target`.
    pub fn patch_branch(&mut self, branch_offset: usize, target_offset: usize) {
        let relative = (target_offset as i32 - branch_offset as i32) >> 2;
        let existing = u32::from_le_bytes([
            self.code[branch_offset],
            self.code[branch_offset + 1],
            self.code[branch_offset + 2],
            self.code[branch_offset + 3],
        ]);
        let opcode = existing & 0xFF000000;
        let patched = if opcode == 0x14000000 || opcode == 0x94000000 {
            // B or BL: 26-bit offset
            (existing & 0xFC000000) | ((relative as u32) & 0x3FFFFFF)
        } else {
            // B.cond: 19-bit offset
            (existing & 0xFF00001F) | (((relative as u32) & 0x7FFFF) << 5)
        };
        self.code[branch_offset..branch_offset + 4].copy_from_slice(&patched.to_le_bytes());
    }
}
