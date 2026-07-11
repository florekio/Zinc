use crate::runtime::value::Value;
use crate::util::interner::StringId;

use super::opcode::OpCode;

bitflags::bitflags! {
    /// Flags describing a compiled chunk's characteristics.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct ChunkFlags: u16 {
        const STRICT    = 0b0000_0001;
        const GENERATOR = 0b0000_0010;
        const ASYNC     = 0b0000_0100;
        const MODULE    = 0b0000_1000;
        const ARROW     = 0b0001_0000;
        /// Concise method (object/class shorthand). Has no [[Construct]] slot.
        const METHOD    = 0b0010_0000;
        /// Top-level script or eval code. Value-producing statements update the
        /// VM completion register so `eval(...)` yields the spec completion value.
        const SCRIPT    = 0b0100_0000;
        /// Class field initializer thunk. A direct eval from here may not
        /// reference `arguments` (ClassFieldDefinition early errors).
        const FIELD_INIT = 0b1000_0000;
        /// Class constructor: calling without `new` is a TypeError.
        const CLASS_CTOR = 0b1_0000_0000;
    }
}

/// A compiled bytecode unit (one per function/script/module).
#[derive(Debug)]
pub struct Chunk {
    /// Bytecode instructions.
    pub code: Vec<u8>,
    /// Constant pool (numbers, strings, nested chunks, etc.)
    pub constants: Vec<Value>,
    /// Run-length encoded line info: (bytecode_offset, source_line).
    pub lines: Vec<(u32, u32)>,
    /// Source file name.
    pub source_name: StringId,
    /// Number of local variable slots needed.
    pub local_count: u16,
    /// Number of upvalues captured.
    pub upvalue_count: u16,
    /// Number of declared parameters.
    pub param_count: u16,
    /// Function.length: params before first default/rest.
    pub formal_length: u16,
    /// Function name (for stack traces).
    pub name: StringId,
    /// Parameter binding names (simple identifiers). Used by direct eval running
    /// in a parameter default to detect var/function-declaration collisions.
    pub param_names: Vec<StringId>,
    /// Body-level lexical (let/const/class) binding names, same purpose.
    pub lexical_names: Vec<StringId>,
    /// Chunk flags (strict, generator, async, etc.)
    pub flags: ChunkFlags,
    /// Upvalue descriptors for this closure.
    pub upvalue_descriptors: Vec<UpvalueDescriptor>,
    /// Exception handler table.
    pub exception_handlers: Vec<ExceptionHandler>,
    /// Nested function chunks (referenced by Closure opcode).
    pub child_chunks: Vec<Chunk>,
    /// Absolute chunk indices of direct children (filled during VM flattening).
    pub children: Vec<usize>,
    /// Monomorphic property inline cache: one byte per GetProperty/SetProperty callsite.
    /// Index via ic_slot embedded in the instruction. 0xFF = cold (not yet cached).
    /// Value = index into obj.properties Vec at the last successful lookup.
    pub property_ic: Vec<u8>,
    /// Number of IC slots allocated so far for this chunk.
    pub ic_slot_count: u16,
    /// The body references the `arguments` object (set for arrow chunks so
    /// closure creation knows to capture the defining scope's arguments).
    pub uses_arguments: bool,
    /// Every formal parameter is a plain identifier (no defaults, rest or
    /// destructuring). Mapped-arguments aliasing only applies to these.
    pub simple_params: bool,
    /// Set when a patched jump's offset exceeded the i16 encoding — the
    /// bytecode is CORRUPT (the offset wrapped) and must not run. The
    /// compiler turns this into a per-script compile error; the long-term
    /// fix is 32-bit conditional jump variants. Huge minified bundles
    /// (Google's ~200 KB main chunk) hit this.
    pub jump_overflow: bool,
}

/// Describes how a closure captures one upvalue.
#[derive(Debug, Clone)]
pub struct UpvalueDescriptor {
    /// Index: if `is_local` is true, this is a local slot in the *enclosing* function.
    /// If false, this is an upvalue index in the enclosing function's upvalue list.
    /// u16 — enclosing functions in minified bundles exceed 256 local slots.
    pub index: u16,
    /// True if capturing directly from the enclosing function's locals.
    /// False if capturing from the enclosing function's upvalues (transitive capture).
    pub is_local: bool,
}

/// Exception handler entry in the handler table.
#[derive(Debug, Clone)]
pub struct ExceptionHandler {
    /// Start of the try block (bytecode offset).
    pub try_start: u32,
    /// End of the try block (bytecode offset).
    pub try_end: u32,
    /// Start of catch handler (0 if no catch).
    pub catch_target: u32,
    /// Start of finally handler (0 if no finally).
    pub finally_target: u32,
    /// Operand stack depth at try entry (for unwinding).
    pub stack_depth: u16,
    /// Local slot for catch parameter (-1/0xFFFF if none).
    pub catch_binding: u16,
}

impl Chunk {
    pub fn new(name: StringId, source_name: StringId) -> Self {
        Self {
            code: Vec::new(),
            constants: Vec::new(),
            lines: Vec::new(),
            source_name,
            local_count: 0,
            upvalue_count: 0,
            param_count: 0,
            formal_length: 0,
            name,
            param_names: Vec::new(),
            lexical_names: Vec::new(),
            flags: ChunkFlags::empty(),
            upvalue_descriptors: Vec::new(),
            exception_handlers: Vec::new(),
            child_chunks: Vec::new(),
            children: Vec::new(),
            property_ic: Vec::new(),
            ic_slot_count: 0,
            jump_overflow: false,
            uses_arguments: false,
            simple_params: true,
        }
    }

    /// Allocate an IC slot for a GetProperty/SetProperty instruction.
    /// Returns the slot index (to be embedded in bytecode).
    pub fn alloc_ic_slot(&mut self) -> u16 {
        let slot = self.ic_slot_count;
        self.ic_slot_count += 1;
        self.property_ic.push(0xFF); // 0xFF = cold
        slot
    }

    // ---- Emit helpers ----

    /// Write a single byte.
    pub fn emit_byte(&mut self, byte: u8, line: u32) {
        self.code.push(byte);
        self.add_line(line);
    }

    /// Emit an opcode.
    pub fn emit_op(&mut self, op: OpCode, line: u32) {
        self.emit_byte(op as u8, line);
    }

    /// Emit an opcode followed by a u8 operand.
    pub fn emit_op_u8(&mut self, op: OpCode, operand: u8, line: u32) {
        self.emit_byte(op as u8, line);
        self.emit_byte(operand, line);
    }

    /// Emit an opcode followed by a u16 operand (big-endian).
    pub fn emit_op_u16(&mut self, op: OpCode, operand: u16, line: u32) {
        self.emit_byte(op as u8, line);
        self.code.push((operand >> 8) as u8);
        self.code.push((operand & 0xFF) as u8);
        self.add_line(line);
        self.add_line(line);
    }

    /// Emit an opcode followed by a u32 operand (big-endian).
    pub fn emit_op_u32(&mut self, op: OpCode, operand: u32, line: u32) {
        self.emit_byte(op as u8, line);
        self.code.push((operand >> 24) as u8);
        self.code.push((operand >> 16) as u8);
        self.code.push((operand >> 8) as u8);
        self.code.push((operand & 0xFF) as u8);
        for _ in 0..4 {
            self.add_line(line);
        }
    }

    /// Emit a jump instruction with a placeholder offset.
    /// Returns the position of the offset bytes for later patching.
    pub fn emit_jump(&mut self, op: OpCode, line: u32) -> usize {
        self.emit_byte(op as u8, line);
        let pos = self.code.len();
        // Placeholder i16 offset
        self.code.push(0xFF);
        self.code.push(0xFF);
        self.add_line(line);
        self.add_line(line);
        pos
    }

    /// Emit a placeholder i16 jump offset that follows another operand (e.g.
    /// the name index of WithGetCheck/WithSetCheck). Returns the position of
    /// the offset bytes for later patching with `patch_jump`.
    pub fn emit_offset_placeholder(&mut self, line: u32) -> usize {
        let pos = self.code.len();
        self.code.push(0xFF);
        self.code.push(0xFF);
        self.add_line(line);
        self.add_line(line);
        pos
    }

    /// Patch a previously emitted jump to target the current position.
    pub fn patch_jump(&mut self, offset_pos: usize) {
        let jump_target = self.code.len();
        let offset = jump_target as i32 - offset_pos as i32 - 2; // -2 for the offset bytes themselves
        if offset < i16::MIN as i32 || offset > i16::MAX as i32 {
            // Truncating would corrupt control flow (the VM would land
            // mid-instruction). Mark the chunk poisoned; the compiler
            // reports it as a compile error after the body finishes.
            self.jump_overflow = true;
        }
        let offset = offset as i16;
        self.code[offset_pos] = (offset >> 8) as u8;
        self.code[offset_pos + 1] = (offset & 0xFF) as u8;
    }

    /// Emit a backward loop jump to `loop_start`.
    pub fn emit_loop(&mut self, loop_start: usize, line: u32) {
        self.emit_byte(OpCode::Loop as u8, line);
        let offset = self.code.len() - loop_start + 2; // +2 for the offset bytes
        if offset > u16::MAX as usize {
            self.jump_overflow = true;
        }
        self.code.push((offset >> 8) as u8);
        self.code.push((offset & 0xFF) as u8);
        self.add_line(line);
        self.add_line(line);
    }

    // ---- Constant pool ----

    /// Add a constant to the pool and return its index.
    pub fn add_constant(&mut self, value: Value) -> u16 {
        let index = self.constants.len();
        self.constants.push(value);
        debug_assert!(index <= u16::MAX as usize, "Constant pool overflow");
        index as u16
    }

    // ---- Line info ----

    fn add_line(&mut self, line: u32) {
        if let Some(last) = self.lines.last()
            && last.1 == line {
                return; // Same line, no need to add
            }
        self.lines.push((self.code.len() as u32 - 1, line));
    }

    /// Get the source line for a bytecode offset.
    pub fn get_line(&self, offset: u32) -> u32 {
        // Binary search for the last entry with offset <= target
        match self.lines.binary_search_by_key(&offset, |&(o, _)| o) {
            Ok(i) => self.lines[i].1,
            Err(0) => 0,
            Err(i) => self.lines[i - 1].1,
        }
    }

    /// Read a u16 from the bytecode at the given offset (big-endian).
    pub fn read_u16(&self, offset: usize) -> u16 {
        ((self.code[offset] as u16) << 8) | (self.code[offset + 1] as u16)
    }

    /// Read an i16 from the bytecode at the given offset (big-endian).
    pub fn read_i16(&self, offset: usize) -> i16 {
        self.read_u16(offset) as i16
    }

    /// Read a u32 from the bytecode at the given offset (big-endian).
    pub fn read_u32(&self, offset: usize) -> u32 {
        ((self.code[offset] as u32) << 24)
            | ((self.code[offset + 1] as u32) << 16)
            | ((self.code[offset + 2] as u32) << 8)
            | (self.code[offset + 3] as u32)
    }

    /// Current bytecode length.
    pub fn len(&self) -> usize {
        self.code.len()
    }

    pub fn is_empty(&self) -> bool {
        self.code.is_empty()
    }
}
